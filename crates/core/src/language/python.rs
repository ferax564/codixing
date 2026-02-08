use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// Python language support.
pub struct PythonLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_definition",
    "class_definition",
    "decorated_definition",
    "import_statement",
    "import_from_statement",
];

impl LanguageSupport for PythonLanguage {
    fn language(&self) -> Language {
        Language::Python
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let root = tree.root_node();
        collect_entities(self, &root, source, &[], &mut entities);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_python_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_python_doc_comment(node, source)
    }
}

fn collect_entities(
    lang: &PythonLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some((entity_kind, target_node)) = match_entity_kind(node, kind_str) {
        let name =
            extract_entity_name(&target_node, source, target_node.kind()).unwrap_or_default();

        let entity = SemanticEntity {
            kind: entity_kind,
            name: name.clone(),
            signature: lang.extract_signature(&target_node, source),
            doc_comment: lang.extract_doc_comment(node, source),
            byte_range: node.start_byte()..node.end_byte(),
            line_range: node_line_range(node),
            scope: scope.to_vec(),
        };
        entities.push(entity);

        // Recurse into class bodies with updated scope
        if target_node.kind() == "class_definition" {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            if let Some(body) = target_node.child_by_field_name("body") {
                let mut cursor = body.walk();
                for child in body.children(&mut cursor) {
                    collect_entities(lang, &child, source, &child_scope, entities);
                }
            }
            return;
        }

        // For decorated_definition, don't recurse into children
        // (we already extracted the inner function/class)
        if kind_str == "decorated_definition" {
            return;
        }
    }

    // Recurse into children for non-entity or top-level nodes
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

/// Match a tree-sitter node kind to an `EntityKind`.
/// For `decorated_definition`, unwrap and return the inner definition node.
fn match_entity_kind<'a>(node: &'a Node<'a>, kind: &str) -> Option<(EntityKind, Node<'a>)> {
    match kind {
        "function_definition" => Some((EntityKind::Function, *node)),
        "class_definition" => Some((EntityKind::Class, *node)),
        "decorated_definition" => {
            // Find the inner function_definition or class_definition
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_definition" => return Some((EntityKind::Function, child)),
                    "class_definition" => return Some((EntityKind::Class, child)),
                    _ => {}
                }
            }
            None
        }
        "import_statement" | "import_from_statement" => Some((EntityKind::Import, *node)),
        _ => None,
    }
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "import_statement" | "import_from_statement" => {
            let text = node_text(node, source);
            Some(text.trim().to_string())
        }
        _ => find_name_node(node, source),
    }
}

fn extract_python_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_definition" => {
            let text = node_text(node, source);
            // Take everything up to the colon that starts the body
            // Find the colon after the closing parenthesis (and optional return type)
            if let Some(colon_pos) = find_def_colon(text) {
                Some(text[..colon_pos].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "class_definition" => {
            let text = node_text(node, source);
            if let Some(colon_pos) = text.find(':') {
                Some(text[..colon_pos].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        _ => None,
    }
}

/// Find the colon that starts the function body.
/// We need to handle the case where there are colons inside type annotations.
/// Strategy: find the last `)` then find the `:` after it (could be after `-> type`).
fn find_def_colon(text: &str) -> Option<usize> {
    // Find last closing paren of the parameter list
    let mut paren_depth = 0;
    let mut last_close_paren = None;
    for (i, ch) in text.char_indices() {
        match ch {
            '(' => paren_depth += 1,
            ')' => {
                paren_depth -= 1;
                if paren_depth == 0 {
                    last_close_paren = Some(i);
                }
            }
            _ => {}
        }
    }
    if let Some(paren_pos) = last_close_paren {
        // Find the `:` after the closing paren (and optional return type annotation)
        // Skip the `-> ...` portion
        let rest = &text[paren_pos..];
        // Find the first `:` that is NOT inside a `[` bracket (for generic types)
        let mut bracket_depth = 0;
        for (i, ch) in rest.char_indices() {
            match ch {
                '[' => bracket_depth += 1,
                ']' => bracket_depth -= 1,
                ':' if bracket_depth == 0 => return Some(paren_pos + i),
                _ => {}
            }
        }
    }
    None
}

fn extract_python_doc_comment(node: &Node, source: &[u8]) -> Option<String> {
    // First try preceding # comments
    let hash_comment = extract_preceding_comments(node, source, "#");
    if hash_comment.is_some() {
        return hash_comment;
    }

    // For decorated_definition, look at the inner node
    let target = if node.kind() == "decorated_definition" {
        let mut cursor = node.walk();
        let mut inner = None;
        for child in node.children(&mut cursor) {
            if child.kind() == "function_definition" || child.kind() == "class_definition" {
                inner = Some(child);
                break;
            }
        }
        inner
    } else if node.kind() == "function_definition" || node.kind() == "class_definition" {
        Some(*node)
    } else {
        None
    };

    // Look for docstring: first statement in the body that is an expression_statement
    // containing a string literal
    if let Some(target_node) = target {
        if let Some(body) = target_node.child_by_field_name("body") {
            let mut cursor = body.walk();
            // Only check the first statement for a docstring
            if let Some(child) = body.children(&mut cursor).next() {
                if child.kind() == "expression_statement" {
                    let mut inner_cursor = child.walk();
                    for inner in child.children(&mut inner_cursor) {
                        if inner.kind() == "string" || inner.kind() == "concatenated_string" {
                            let text = node_text(&inner, source);
                            // Strip triple quotes
                            let stripped = text
                                .trim_start_matches("\"\"\"")
                                .trim_start_matches("'''")
                                .trim_end_matches("\"\"\"")
                                .trim_end_matches("'''")
                                .trim();
                            return Some(stripped.to_string());
                        }
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_python(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = PythonLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_function_with_docstring() {
        let src = r#"
def add(x: int, y: int) -> int:
    """Add two numbers together."""
    return x + y
"#;
        let (_, entities) = parse_python(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
        assert!(fns[0].signature.as_ref().unwrap().contains("def add"));
        assert!(
            fns[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("Add two numbers")
        );
    }

    #[test]
    fn extract_class_with_methods() {
        let src = r#"
class Point:
    """A 2D point."""

    def __init__(self, x: float, y: float):
        self.x = x
        self.y = y

    def distance(self, other: 'Point') -> float:
        return ((self.x - other.x) ** 2 + (self.y - other.y) ** 2) ** 0.5
"#;
        let (_, entities) = parse_python(src);
        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Point");
        assert!(
            classes[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("2D point")
        );

        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 2);
        assert!(
            fns.iter()
                .any(|f| f.name == "__init__" && !f.scope.is_empty())
        );
        assert!(fns.iter().any(|f| f.name == "distance"));
    }

    #[test]
    fn extract_decorated_function() {
        let src = r#"
@staticmethod
def greet(name: str) -> str:
    return f"Hello, {name}!"
"#;
        let (_, entities) = parse_python(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "greet");
        assert!(fns[0].signature.as_ref().unwrap().contains("def greet"));
    }

    #[test]
    fn extract_imports() {
        let src = r#"
import os
from pathlib import Path
"#;
        let (_, entities) = parse_python(src);
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 2);
    }
}
