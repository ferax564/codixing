use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// C language support.
pub struct CLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_definition",
    "struct_specifier",
    "enum_specifier",
    "type_definition",
    "declaration",
    "preproc_include",
];

impl LanguageSupport for CLanguage {
    fn language(&self) -> Language {
        Language::C
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_c::LANGUAGE.into()
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
        extract_c_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }
}

fn collect_entities(
    lang: &CLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str, node, source) {
        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        // Skip anonymous structs/enums unless they're part of a typedef
        if name.is_empty() && matches!(entity_kind, EntityKind::Struct | EntityKind::Enum) {
            // Don't add, but do recurse
        } else {
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
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

fn match_entity_kind(kind: &str, node: &Node, source: &[u8]) -> Option<EntityKind> {
    match kind {
        "function_definition" => Some(EntityKind::Function),
        "struct_specifier" => {
            // Only at top level (not inside a typedef)
            if let Some(parent) = node.parent() {
                if parent.kind() == "type_definition" {
                    return None; // Will be handled by type_definition
                }
            }
            Some(EntityKind::Struct)
        }
        "enum_specifier" => {
            if let Some(parent) = node.parent() {
                if parent.kind() == "type_definition" {
                    return None;
                }
            }
            Some(EntityKind::Enum)
        }
        "type_definition" => Some(EntityKind::TypeAlias),
        "declaration" => {
            // Only include function declarations (prototypes), not variable declarations
            if is_function_declaration(node, source) {
                Some(EntityKind::Function)
            } else {
                None
            }
        }
        "preproc_include" => Some(EntityKind::Import),
        _ => None,
    }
}

/// Check if a `declaration` node is a function prototype.
fn is_function_declaration(node: &Node, _source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_declarator" {
            return true;
        }
        // Handle pointer declarators containing function_declarator
        if child.kind() == "pointer_declarator" || child.kind() == "init_declarator" {
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                if inner.kind() == "function_declarator" {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "function_definition" => {
            // Function name is inside the declarator
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return find_declarator_name(&declarator, source);
            }
            None
        }
        "struct_specifier" | "enum_specifier" => find_name_node(node, source),
        "type_definition" => {
            // The typedef name is the last identifier (the declarator)
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return Some(node_text(&declarator, source).to_string());
            }
            None
        }
        "declaration" => {
            // Function prototype name
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return find_declarator_name(&declarator, source);
            }
            None
        }
        "preproc_include" => {
            let text = node_text(node, source);
            Some(text.trim().to_string())
        }
        _ => find_name_node(node, source),
    }
}

/// Recursively find the identifier name inside a declarator chain.
fn find_declarator_name(node: &Node, source: &[u8]) -> Option<String> {
    if node.kind() == "identifier" {
        return Some(node_text(node, source).to_string());
    }
    // Check the "declarator" field first
    if let Some(decl) = node.child_by_field_name("declarator") {
        return find_declarator_name(&decl, source);
    }
    // Fallback: check children for identifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
        if child.kind() == "function_declarator"
            || child.kind() == "pointer_declarator"
            || child.kind() == "array_declarator"
        {
            if let Some(name) = find_declarator_name(&child, source) {
                return Some(name);
            }
        }
    }
    None
}

fn extract_c_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_definition" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "declaration" => {
            let text = node_text(node, source);
            Some(text.trim().trim_end_matches(';').to_string())
        }
        "struct_specifier" | "enum_specifier" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "type_definition" => {
            let text = node_text(node, source);
            Some(
                text.lines()
                    .next()
                    .unwrap_or(text)
                    .trim()
                    .trim_end_matches(';')
                    .to_string(),
            )
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_c(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = CLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_function() {
        let src = r#"
// Add two integers.
int add(int a, int b) {
    return a + b;
}
"#;
        let (_, entities) = parse_c(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
        assert!(fns[0].signature.as_ref().unwrap().contains("int add"));
        assert!(
            fns[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("Add two integers")
        );
    }

    #[test]
    fn extract_struct_and_typedef() {
        let src = r#"
struct Point {
    double x;
    double y;
};

typedef unsigned long size_t;
"#;
        let (_, entities) = parse_c(src);
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");

        let types: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::TypeAlias)
            .collect();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "size_t");
    }

    #[test]
    fn extract_enum_and_include() {
        let src = r#"
#include <stdio.h>

enum Color {
    RED,
    GREEN,
    BLUE
};
"#;
        let (_, entities) = parse_c(src);
        let enums: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Enum)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Color");

        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
    }
}
