//! Bash language support — tree-sitter-based symbol extraction.
//!
//! Extracts functions and top-level variable assignments from shell scripts.

use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, extract_preceding_comments,
    node_line_range, node_text,
};

/// Bash/Shell language support using the `tree-sitter-bash` grammar.
pub struct BashLanguage;

const ENTITY_KINDS: &[&str] = &["function_definition", "variable_assignment"];

impl LanguageSupport for BashLanguage {
    fn language(&self) -> Language {
        Language::Bash
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_bash::LANGUAGE.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        collect_entities(&tree.root_node(), source, &mut entities);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_bash_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "#")
    }
}

fn collect_entities(node: &Node, source: &[u8], entities: &mut Vec<SemanticEntity>) {
    let kind_str = node.kind();

    match kind_str {
        "function_definition" => {
            let name = extract_fn_name(node, source).unwrap_or_default();
            entities.push(SemanticEntity {
                kind: EntityKind::Function,
                name,
                signature: extract_bash_signature(node, source),
                doc_comment: extract_preceding_comments(node, source, "#"),
                byte_range: node.start_byte()..node.end_byte(),
                line_range: node_line_range(node),
                scope: vec![],
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
        }
        "variable_assignment"
            // Only extract top-level variable assignments (depth 0 or 1 from root).
            if is_top_level(node) => {
                if let Some(name) = node.child_by_field_name("name") {
                    let var_name = node_text(&name, source).to_string();
                    entities.push(SemanticEntity {
                        kind: EntityKind::Variable,
                        name: var_name,
                        signature: extract_bash_signature(node, source),
                        doc_comment: extract_preceding_comments(node, source, "#"),
                        byte_range: node.start_byte()..node.end_byte(),
                        line_range: node_line_range(node),
                        scope: vec![],
                        visibility: Visibility::default(),
                        type_relations: Vec::new(),
                    });
                }
            }
        _ => {}
    }

    // Recurse into children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(&child, source, entities);
    }
}

/// Check if a node is at the top level (parent is program or root).
fn is_top_level(node: &Node) -> bool {
    node.parent()
        .is_none_or(|p| p.kind() == "program" || p.parent().is_none())
}

/// Extract function name from a `function_definition` node.
fn extract_fn_name(node: &Node, source: &[u8]) -> Option<String> {
    // Try the "name" field first (covers `function foo { ... }` syntax).
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(&name_node, source).to_string());
    }
    // Fallback: look for a `word` child (covers `foo() { ... }` syntax).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "word" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

fn extract_bash_signature(node: &Node, source: &[u8]) -> Option<String> {
    let text = node_text(node, source);
    match node.kind() {
        "function_definition" => {
            // Show first line or up to `{`.
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).trim().to_string())
            }
        }
        "variable_assignment" => Some(text.lines().next().unwrap_or(text).trim().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_bash(source: &str) -> Vec<SemanticEntity> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = BashLanguage;
        lang.extract_entities(&tree, source.as_bytes())
    }

    #[test]
    fn extract_function() {
        let src = r#"
# Greet a user.
greet() {
    echo "Hello, $1"
}
"#;
        let entities = parse_bash(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "greet");
        assert!(fns[0].signature.as_ref().unwrap().contains("greet"));
        assert!(
            fns[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("Greet a user")
        );
    }

    #[test]
    fn extract_function_keyword_syntax() {
        let src = r#"
function deploy {
    echo "deploying..."
}
"#;
        let entities = parse_bash(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "deploy");
    }

    #[test]
    fn extract_variable() {
        let src = r#"
# App version
VERSION="1.0.0"
PORT=8080
"#;
        let entities = parse_bash(src);
        let vars: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Variable)
            .collect();
        assert_eq!(vars.len(), 2);
        assert!(vars.iter().any(|v| v.name == "VERSION"));
        assert!(vars.iter().any(|v| v.name == "PORT"));
    }
}
