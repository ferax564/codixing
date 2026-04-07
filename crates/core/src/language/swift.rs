use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, extract_preceding_comments,
    node_line_range, node_text,
};

/// Swift language support.
pub struct SwiftLanguage;

/// In the tree-sitter-swift grammar, `class`, `struct`, `enum`, and `extension`
/// all parse as `class_declaration`. We differentiate by inspecting the first
/// keyword child.
const ENTITY_KINDS: &[&str] = &[
    "class_declaration",
    "function_declaration",
    "protocol_declaration",
    "import_declaration",
];

impl LanguageSupport for SwiftLanguage {
    fn language(&self) -> Language {
        Language::Swift
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_swift::LANGUAGE.into()
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
        extract_swift_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "///")
            .or_else(|| extract_preceding_comments(node, source, "//"))
    }
}

fn collect_entities(
    lang: &SwiftLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(node, kind_str, source) {
        let name = extract_entity_name(node, source).unwrap_or_default();

        let entity = SemanticEntity {
            kind: entity_kind.clone(),
            name: name.clone(),
            signature: lang.extract_signature(node, source),
            doc_comment: lang.extract_doc_comment(node, source),
            byte_range: node.start_byte()..node.end_byte(),
            line_range: node_line_range(node),
            scope: scope.to_vec(),
            visibility: Visibility::default(),
        };
        entities.push(entity);

        // Recurse into container bodies with updated scope.
        if matches!(
            entity_kind,
            EntityKind::Class | EntityKind::Struct | EntityKind::Interface | EntityKind::Module
        ) {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                let ck = child.kind();
                // Swift bodies: class_body, protocol_body, enum_class_body
                if ck == "class_body" || ck == "protocol_body" || ck == "enum_class_body" {
                    let mut body_cursor = child.walk();
                    for body_child in child.children(&mut body_cursor) {
                        collect_entities(lang, &body_child, source, &child_scope, entities);
                    }
                    return;
                }
            }
        }
    }

    // Recurse into children for non-entity nodes.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

/// Map a tree-sitter node kind to an `EntityKind`.
///
/// In tree-sitter-swift, `class_declaration` is reused for `class`, `struct`,
/// `enum`, and `extension`. We differentiate by looking at the first keyword
/// child of the node.
fn match_entity_kind(node: &Node, kind: &str, source: &[u8]) -> Option<EntityKind> {
    match kind {
        "class_declaration" => {
            // Inspect the first non-whitespace child keyword.
            let kw = first_keyword(node, source);
            match kw.as_deref() {
                Some("class") => Some(EntityKind::Class),
                Some("struct") => Some(EntityKind::Struct),
                Some("enum") => Some(EntityKind::Enum),
                Some("extension") => Some(EntityKind::Module),
                _ => Some(EntityKind::Class), // fallback
            }
        }
        "function_declaration" => Some(EntityKind::Function),
        "protocol_declaration" => Some(EntityKind::Interface),
        "import_declaration" => Some(EntityKind::Import),
        _ => None,
    }
}

/// Return the text of the first leaf child in a node (the keyword).
fn first_keyword(node: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.child_count() == 0 {
            let text = node_text(&child, source).trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn extract_entity_name(node: &Node, source: &[u8]) -> Option<String> {
    // Swift uses `type_identifier`, `simple_identifier`, or `user_type` for names.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let ck = child.kind();
        if ck == "type_identifier" || ck == "simple_identifier" {
            return Some(node_text(&child, source).to_string());
        }
        // user_type is used in extension declarations.
        if ck == "user_type" {
            let text = node_text(&child, source).to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn extract_swift_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_declaration" | "class_declaration" | "protocol_declaration" => {
            let text = node_text(node, source);
            // Take everything before the opening brace `{`.
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).trim().to_string())
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_swift(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_swift::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = SwiftLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_class_and_function() {
        let src = r#"
import Foundation

/// A simple greeter class.
class Greeter {
    var name: String

    /// Creates a greeting message.
    func greet() -> String {
        return "Hello, \(name)!"
    }
}
"#;
        let (_, entities) = parse_swift(src);

        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert!(!classes.is_empty(), "expected at least one class");
        assert!(classes.iter().any(|c| c.name == "Greeter"));

        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(!imports.is_empty(), "expected at least one import");
    }

    #[test]
    fn extract_struct_and_protocol() {
        let src = r#"
protocol Drawable {
    func draw()
}

struct Point: Drawable {
    var x: Double
    var y: Double

    func draw() {
        print("(\(x), \(y))")
    }
}
"#;
        let (_, entities) = parse_swift(src);

        let protocols: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert!(!protocols.is_empty(), "expected at least one protocol");

        // In tree-sitter-swift, `struct` parses as `class_declaration` with
        // keyword child `struct`.  Our `match_entity_kind` maps it to
        // `EntityKind::Struct`.
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert!(!structs.is_empty(), "expected at least one struct");
        assert!(structs.iter().any(|s| s.name == "Point"));
    }
}
