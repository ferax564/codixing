use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// Rust language support.
pub struct RustLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_item",
    "struct_item",
    "enum_item",
    "impl_item",
    "trait_item",
    "type_item",
    "const_item",
    "static_item",
    "mod_item",
    "use_declaration",
];

impl LanguageSupport for RustLanguage {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
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
        extract_rust_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "///")
    }
}

fn collect_entities(
    lang: &RustLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str) {
        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        let entity = SemanticEntity {
            kind: entity_kind,
            name: name.clone(),
            signature: lang.extract_signature(node, source),
            doc_comment: lang.extract_doc_comment(node, source),
            byte_range: node.start_byte()..node.end_byte(),
            line_range: node_line_range(node),
            scope: scope.to_vec(),
        };
        entities.push(entity);

        // Recurse into impl/trait/mod bodies with updated scope
        if matches!(kind_str, "impl_item" | "trait_item" | "mod_item") {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_entities(lang, &child, source, &child_scope, entities);
            }
            return;
        }
    }

    // Recurse into children for non-entity or top-level nodes
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

fn match_entity_kind(kind: &str) -> Option<EntityKind> {
    match kind {
        "function_item" => Some(EntityKind::Function),
        "struct_item" => Some(EntityKind::Struct),
        "enum_item" => Some(EntityKind::Enum),
        "impl_item" => Some(EntityKind::Impl),
        "trait_item" => Some(EntityKind::Trait),
        "type_item" => Some(EntityKind::TypeAlias),
        "const_item" => Some(EntityKind::Constant),
        "static_item" => Some(EntityKind::Static),
        "mod_item" => Some(EntityKind::Module),
        "use_declaration" => Some(EntityKind::Import),
        _ => None,
    }
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "impl_item" => {
            // For impl blocks: "impl Foo" or "impl Trait for Type"
            if let Some(type_node) = node.child_by_field_name("type") {
                Some(node_text(&type_node, source).to_string())
            } else {
                find_name_node(node, source)
            }
        }
        "use_declaration" => {
            // Return the full use path
            let text = node_text(node, source);
            Some(
                text.trim_start_matches("use ")
                    .trim_end_matches(';')
                    .to_string(),
            )
        }
        _ => find_name_node(node, source),
    }
}

fn extract_rust_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_item" => {
            // Everything up to the body block
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.to_string())
            }
        }
        "struct_item" | "enum_item" | "trait_item" => {
            let text = node_text(node, source);
            // Just the declaration line
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else if let Some(semi) = text.find(';') {
                Some(text[..=semi].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "impl_item" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "type_item" | "const_item" | "static_item" => {
            let text = node_text(node, source);
            Some(text.lines().next().unwrap_or(text).trim().to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_rust(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = RustLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_function() {
        let src = r#"
/// Add two numbers.
fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;
        let (_, entities) = parse_rust(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
        assert!(fns[0].signature.as_ref().unwrap().contains("fn add"));
        assert!(fns[0].doc_comment.as_ref().unwrap().contains("Add two"));
    }

    #[test]
    fn extract_struct_and_impl() {
        let src = r#"
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(&self, other: &Point) -> f64 {
        ((self.x - other.x).powi(2) + (self.y - other.y).powi(2)).sqrt()
    }
}
"#;
        let (_, entities) = parse_rust(src);
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");

        let impls: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Impl)
            .collect();
        assert_eq!(impls.len(), 1);

        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 2);
        // Functions inside impl should have scope
        assert!(fns.iter().any(|f| f.name == "new" && !f.scope.is_empty()));
        assert!(fns.iter().any(|f| f.name == "distance"));
    }

    #[test]
    fn extract_enum_and_trait() {
        let src = r#"
pub enum Color {
    Red,
    Green,
    Blue,
}

pub trait Drawable {
    fn draw(&self);
    fn color(&self) -> Color;
}
"#;
        let (_, entities) = parse_rust(src);
        assert!(
            entities
                .iter()
                .any(|e| e.kind == EntityKind::Enum && e.name == "Color")
        );
        assert!(
            entities
                .iter()
                .any(|e| e.kind == EntityKind::Trait && e.name == "Drawable")
        );
    }

    #[test]
    fn extract_use_and_const() {
        let src = r#"
use std::collections::HashMap;

const MAX_SIZE: usize = 1024;

static GLOBAL: &str = "hello";

type Result<T> = std::result::Result<T, MyError>;
"#;
        let (_, entities) = parse_rust(src);
        assert!(entities.iter().any(|e| e.kind == EntityKind::Import));
        assert!(
            entities
                .iter()
                .any(|e| e.kind == EntityKind::Constant && e.name == "MAX_SIZE")
        );
        assert!(
            entities
                .iter()
                .any(|e| e.kind == EntityKind::Static && e.name == "GLOBAL")
        );
        assert!(
            entities
                .iter()
                .any(|e| e.kind == EntityKind::TypeAlias && e.name == "Result")
        );
    }

    #[test]
    fn line_ranges_are_correct() {
        let src = "fn hello() {\n    42\n}\n";
        let (_, entities) = parse_rust(src);
        let f = &entities[0];
        assert_eq!(f.line_range.start, 0);
        assert_eq!(f.line_range.end, 3);
    }
}
