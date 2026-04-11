use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, node_line_range, node_text,
};

/// Zig language support using the native `tree-sitter-zig` grammar.
pub struct ZigLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_declaration",
    "variable_declaration",
    "test_declaration",
];

impl LanguageSupport for ZigLanguage {
    fn language(&self) -> Language {
        Language::Zig
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_zig::LANGUAGE.into()
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
        extract_zig_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_zig_doc_comment(node, source)
    }
}

fn collect_entities(
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    match kind_str {
        "function_declaration" => {
            let is_pub = has_pub_child(node, source);
            let name = extract_fn_name(node, source).unwrap_or_default();
            let _ = is_pub; // visibility info available if needed later
            entities.push(SemanticEntity {
                kind: EntityKind::Function,
                name,
                signature: extract_zig_signature(node, source),
                doc_comment: extract_zig_doc_comment(node, source),
                byte_range: node.start_byte()..node.end_byte(),
                line_range: node_line_range(node),
                scope: scope.to_vec(),
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
        }
        "variable_declaration" => {
            // variable_declaration covers: const imports, const values, const
            // struct/enum definitions.
            if let Some((entity_kind, name)) = classify_variable_decl(node, source) {
                entities.push(SemanticEntity {
                    kind: entity_kind.clone(),
                    name: name.clone(),
                    signature: extract_zig_signature(node, source),
                    doc_comment: extract_zig_doc_comment(node, source),
                    byte_range: node.start_byte()..node.end_byte(),
                    line_range: node_line_range(node),
                    scope: scope.to_vec(),
                    visibility: Visibility::default(),
                    type_relations: Vec::new(),
                });

                // Recurse into struct/enum bodies with updated scope.
                if matches!(entity_kind, EntityKind::Struct | EntityKind::Enum) {
                    let mut child_scope = scope.to_vec();
                    if !name.is_empty() {
                        child_scope.push(name);
                    }
                    // Find the struct_declaration/enum_declaration child and
                    // recurse into its children.
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "struct_declaration"
                            || child.kind() == "enum_declaration"
                        {
                            let mut inner_cursor = child.walk();
                            for inner in child.children(&mut inner_cursor) {
                                collect_entities(&inner, source, &child_scope, entities);
                            }
                        }
                    }
                    return;
                }
            }
        }
        "test_declaration" => {
            let name = extract_test_name(node, source).unwrap_or_else(|| "test".to_string());
            entities.push(SemanticEntity {
                kind: EntityKind::Function,
                name,
                signature: extract_zig_signature(node, source),
                doc_comment: extract_zig_doc_comment(node, source),
                byte_range: node.start_byte()..node.end_byte(),
                line_range: node_line_range(node),
                scope: scope.to_vec(),
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
        }
        _ => {}
    }

    // Recurse into children.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(&child, source, scope, entities);
    }
}

/// Classify a `variable_declaration` node.
///
/// In Zig, `const Foo = struct { ... }` parses as a `variable_declaration`
/// whose value child is a `struct_declaration`.  Similarly for enums, imports,
/// and plain constants.
fn classify_variable_decl(node: &Node, source: &[u8]) -> Option<(EntityKind, String)> {
    let name = extract_var_name(node, source)?;

    // Check the value child to determine what this declaration really is.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "struct_declaration" => return Some((EntityKind::Struct, name)),
            "enum_declaration" => return Some((EntityKind::Enum, name)),
            "builtin_function" => {
                // @import("std") — treat as an import.
                let builtin_name = child.child(0).map(|n| node_text(&n, source)).unwrap_or("");
                if builtin_name == "@import" {
                    return Some((EntityKind::Import, name));
                }
            }
            _ => {}
        }
    }

    // If it has `const` keyword and no special value, it's a constant.
    if is_const_decl(node, source) {
        Some((EntityKind::Constant, name))
    } else {
        None
    }
}

/// Check whether a `variable_declaration` has a `pub` keyword child.
fn has_pub_child(node: &Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "pub" || node_text(&child, source) == "pub" {
            return true;
        }
    }
    false
}

/// Check whether a `variable_declaration` has a `const` keyword.
fn is_const_decl(node: &Node, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "const" || node_text(&child, source) == "const" {
            return true;
        }
    }
    false
}

/// Extract the name from a `function_declaration` node.
fn extract_fn_name(node: &Node, source: &[u8]) -> Option<String> {
    // The function name is an `identifier` child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

/// Extract the name from a `variable_declaration` node (the identifier after
/// `const`/`var`).
fn extract_var_name(node: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

/// Extract the name from a `test_declaration` node (the string literal).
fn extract_test_name(node: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "string" {
            // Extract the string_content child.
            let mut inner = child.walk();
            for sc in child.children(&mut inner) {
                if sc.kind() == "string_content" {
                    return Some(format!("test \"{}\"", node_text(&sc, source)));
                }
            }
        }
    }
    None
}

fn extract_zig_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).trim().to_string())
            }
        }
        "variable_declaration" => {
            let text = node_text(node, source);
            // For struct/enum declarations show up to the opening brace.
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).trim().to_string())
            }
        }
        "test_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).trim().to_string())
            }
        }
        _ => None,
    }
}

fn extract_zig_doc_comment(node: &Node, source: &[u8]) -> Option<String> {
    // Zig doc comments use `///` prefix.
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        let kind = sib.kind();
        if kind == "doc_comment" || kind == "line_comment" || kind == "comment" {
            let text = node_text(&sib, source).trim().to_string();
            comments.push(text);
            sibling = sib.prev_sibling();
        } else {
            break;
        }
    }
    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    let cleaned: Vec<String> = comments
        .iter()
        .map(|c| {
            c.trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches("//")
                .trim()
                .to_string()
        })
        .collect();
    Some(cleaned.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_zig(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_zig::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = ZigLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_function() {
        let src = r#"
/// Add two numbers.
pub fn add(a: i32, b: i32) i32 {
    return a + b;
}
"#;
        let (_, entities) = parse_zig(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
        assert!(fns[0].signature.as_ref().unwrap().contains("fn add"));
        assert!(
            fns[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("Add two numbers")
        );
    }

    #[test]
    fn extract_struct_and_enum() {
        let src = r#"
pub const Point = struct {
    x: f64,
    y: f64,
};

pub const Color = enum {
    red,
    green,
    blue,
};
"#;
        let (_, entities) = parse_zig(src);
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");

        let enums: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Enum)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Color");
    }

    #[test]
    fn extract_const_and_import() {
        let src = r#"
const std = @import("std");
pub const MAX_SIZE: usize = 1024;
"#;
        let (_, entities) = parse_zig(src);
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].name, "std");

        let consts: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .collect();
        assert_eq!(consts.len(), 1);
        assert_eq!(consts[0].name, "MAX_SIZE");
    }

    #[test]
    fn extract_test_declaration() {
        let src = r#"
test "add test" {
    const result = add(1, 2);
}
"#;
        let (_, entities) = parse_zig(src);
        let tests: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function && e.name.starts_with("test"))
            .collect();
        assert_eq!(tests.len(), 1);
        assert!(tests[0].name.contains("add test"));
    }

    #[test]
    fn extract_private_function() {
        let src = r#"
fn private_fn() void {}
"#;
        let (_, entities) = parse_zig(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "private_fn");
    }
}
