//! MATLAB language support — tree-sitter-based symbol extraction.
//!
//! Extracts functions, classes, and nested functions from MATLAB source files.

use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, node_line_range, node_text,
};

/// MATLAB language support using the `tree-sitter-matlab` grammar.
pub struct MatlabLanguage;

const ENTITY_KINDS: &[&str] = &["function_definition", "class_definition"];

impl LanguageSupport for MatlabLanguage {
    fn language(&self) -> Language {
        Language::Matlab
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_matlab::LANGUAGE.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        collect_entities(&tree.root_node(), source, &[], &mut entities);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_matlab_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_matlab_doc_comment(node, source)
    }
}

fn collect_entities(
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    // Determine the scope for children of this node.
    let child_scope = match kind_str {
        "function_definition" => {
            let name = extract_fn_name(node, source).unwrap_or_default();
            entities.push(SemanticEntity {
                kind: EntityKind::Function,
                name: name.clone(),
                signature: extract_matlab_signature(node, source),
                doc_comment: extract_matlab_doc_comment(node, source),
                byte_range: node.start_byte()..node.end_byte(),
                line_range: node_line_range(node),
                scope: scope.to_vec(),
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
            let mut s = scope.to_vec();
            if !name.is_empty() {
                s.push(name);
            }
            s
        }
        "class_definition" => {
            let name = extract_class_name(node, source).unwrap_or_default();
            entities.push(SemanticEntity {
                kind: EntityKind::Class,
                name: name.clone(),
                signature: extract_matlab_signature(node, source),
                doc_comment: extract_matlab_doc_comment(node, source),
                byte_range: node.start_byte()..node.end_byte(),
                line_range: node_line_range(node),
                scope: scope.to_vec(),
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
            let mut s = scope.to_vec();
            if !name.is_empty() {
                s.push(name);
            }
            s
        }
        _ => scope.to_vec(),
    };

    // Always recurse into all children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(&child, source, &child_scope, entities);
    }
}

/// Extract the function name from a MATLAB `function_definition` node.
///
/// In MATLAB's tree-sitter grammar, the function name is a direct `identifier`
/// child of `function_definition`, NOT inside `function_output`.  The structure
/// is: `function_definition -> function_output? -> identifier(name)`.
fn extract_fn_name(node: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // The function name is a top-level identifier child,
        // NOT the one inside function_output (which is the return var).
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

/// Extract the class name from a MATLAB `class_definition` node.
fn extract_class_name(node: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

fn extract_matlab_signature(node: &Node, source: &[u8]) -> Option<String> {
    let text = node_text(node, source);
    // Show the first line of the definition.
    Some(text.lines().next().unwrap_or(text).trim().to_string())
}

fn extract_matlab_doc_comment(node: &Node, source: &[u8]) -> Option<String> {
    // MATLAB comments use `%` prefix.
    // The tree-sitter grammar may insert whitespace/newline nodes between
    // the comment and the function, so we skip non-comment non-entity siblings.
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        let kind = sib.kind();
        if kind == "comment" {
            let text = node_text(&sib, source).trim().to_string();
            comments.push(text);
            sibling = sib.prev_sibling();
        } else if sib.is_named() {
            // Stop at any named node that isn't a comment (e.g., another function).
            break;
        } else {
            // Skip unnamed nodes (whitespace, newlines).
            sibling = sib.prev_sibling();
        }
    }
    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    let cleaned: Vec<String> = comments
        .iter()
        .map(|c| c.trim_start_matches('%').trim().to_string())
        .collect();
    Some(cleaned.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_matlab(source: &str) -> Vec<SemanticEntity> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_matlab::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = MatlabLanguage;
        lang.extract_entities(&tree, source.as_bytes())
    }

    #[test]
    fn extract_function() {
        let src = r#"
% Compute the sum of two numbers.
function result = add(a, b)
    result = a + b;
end
"#;
        let entities = parse_matlab(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
        assert!(fns[0].signature.as_ref().unwrap().contains("function"));
        assert!(
            fns[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("Compute the sum")
        );
    }

    #[test]
    fn extract_class() {
        let src = r#"
classdef MyClass
    methods
        function obj = MyClass()
        end
    end
end
"#;
        let entities = parse_matlab(src);
        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "MyClass");
    }

    #[test]
    fn extract_nested_function() {
        let src = r#"
function result = outer(x)
    result = inner(x);

    function y = inner(x)
        y = x * 2;
    end
end
"#;
        let entities = parse_matlab(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 2);
        let outer = fns.iter().find(|f| f.name == "outer").unwrap();
        assert!(outer.scope.is_empty());
        let inner = fns.iter().find(|f| f.name == "inner").unwrap();
        assert_eq!(inner.scope, vec!["outer".to_string()]);
    }
}
