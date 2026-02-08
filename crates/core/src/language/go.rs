use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// Go language support.
pub struct GoLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_declaration",
    "method_declaration",
    "type_declaration",
    "import_declaration",
];

impl LanguageSupport for GoLanguage {
    fn language(&self) -> Language {
        Language::Go
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_go::LANGUAGE.into()
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
        extract_go_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }
}

fn collect_entities(
    lang: &GoLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str) {
        // type_declaration can contain multiple type_spec children
        if kind_str == "type_declaration" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    let name = find_name_node(&child, source).unwrap_or_default();
                    let inner_kind = classify_type_spec(&child);
                    let entity = SemanticEntity {
                        kind: inner_kind,
                        name,
                        signature: extract_type_spec_signature(&child, source),
                        doc_comment: lang.extract_doc_comment(node, source),
                        byte_range: node.start_byte()..node.end_byte(),
                        line_range: node_line_range(node),
                        scope: scope.to_vec(),
                    };
                    entities.push(entity);
                }
            }
            return;
        }

        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        let entity = SemanticEntity {
            kind: entity_kind,
            name,
            signature: lang.extract_signature(node, source),
            doc_comment: lang.extract_doc_comment(node, source),
            byte_range: node.start_byte()..node.end_byte(),
            line_range: node_line_range(node),
            scope: scope.to_vec(),
        };
        entities.push(entity);
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

fn match_entity_kind(kind: &str) -> Option<EntityKind> {
    match kind {
        "function_declaration" => Some(EntityKind::Function),
        "method_declaration" => Some(EntityKind::Method),
        "type_declaration" => Some(EntityKind::TypeAlias),
        "import_declaration" => Some(EntityKind::Import),
        _ => None,
    }
}

/// Classify a type_spec node into Struct, Interface, or TypeAlias.
fn classify_type_spec(node: &Node) -> EntityKind {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "struct_type" => return EntityKind::Struct,
            "interface_type" => return EntityKind::Interface,
            _ => {}
        }
    }
    EntityKind::TypeAlias
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "import_declaration" => {
            let text = node_text(node, source);
            Some(text.trim().to_string())
        }
        "method_declaration" => find_name_node(node, source),
        _ => find_name_node(node, source),
    }
}

fn extract_go_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_declaration" | "method_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        _ => None,
    }
}

fn extract_type_spec_signature(node: &Node, source: &[u8]) -> Option<String> {
    let text = node_text(node, source);
    let full = format!("type {text}");
    if let Some(brace) = full.find('{') {
        Some(full[..brace].trim().to_string())
    } else {
        Some(full.lines().next().unwrap_or(&full).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_go(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = GoLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_function() {
        let src = r#"
package main

// Add adds two integers.
func Add(a int, b int) int {
    return a + b
}
"#;
        let (_, entities) = parse_go(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "Add");
        assert!(fns[0].signature.as_ref().unwrap().contains("func Add"));
        assert!(fns[0].doc_comment.as_ref().unwrap().contains("Add adds"));
    }

    #[test]
    fn extract_struct_and_method() {
        let src = r#"
package main

type Point struct {
    X float64
    Y float64
}

func (p *Point) Distance(other *Point) float64 {
    return 0.0
}
"#;
        let (_, entities) = parse_go(src);
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");

        let methods: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "Distance");
        assert!(
            methods[0]
                .signature
                .as_ref()
                .unwrap()
                .contains("func (p *Point) Distance")
        );
    }

    #[test]
    fn extract_interface_and_import() {
        let src = r#"
package main

import "fmt"

type Stringer interface {
    String() string
}
"#;
        let (_, entities) = parse_go(src);
        let ifaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "Stringer");

        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
    }
}
