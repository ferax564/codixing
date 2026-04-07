use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, find_name_node,
    node_line_range, node_text,
};

/// C# language support.
pub struct CSharpLanguage;

const ENTITY_KINDS: &[&str] = &[
    "class_declaration",
    "method_declaration",
    "interface_declaration",
    "namespace_declaration",
    "enum_declaration",
    "struct_declaration",
    "property_declaration",
    "constructor_declaration",
    "using_directive",
];

impl LanguageSupport for CSharpLanguage {
    fn language(&self) -> Language {
        Language::CSharp
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_c_sharp::LANGUAGE.into()
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
        extract_csharp_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_csharp_doc_comment(node, source)
    }
}

fn collect_entities(
    lang: &CSharpLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str) {
        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        let entity = SemanticEntity {
            kind: entity_kind.clone(),
            name: name.clone(),
            signature: lang.extract_signature(node, source),
            doc_comment: lang.extract_doc_comment(node, source),
            byte_range: node.start_byte()..node.end_byte(),
            line_range: node_line_range(node),
            scope: scope.to_vec(),
            visibility: Visibility::default(),
            type_relations: Vec::new(),
        };
        entities.push(entity);

        // Recurse into class/interface/namespace/struct/enum bodies with updated scope
        if matches!(
            entity_kind,
            EntityKind::Class
                | EntityKind::Interface
                | EntityKind::Namespace
                | EntityKind::Struct
                | EntityKind::Enum
        ) {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "declaration_list"
                    || child.kind() == "enum_member_declaration_list"
                {
                    let mut body_cursor = child.walk();
                    for body_child in child.children(&mut body_cursor) {
                        collect_entities(lang, &body_child, source, &child_scope, entities);
                    }
                }
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
        "class_declaration" => Some(EntityKind::Class),
        "method_declaration" => Some(EntityKind::Method),
        "interface_declaration" => Some(EntityKind::Interface),
        "namespace_declaration" => Some(EntityKind::Namespace),
        "enum_declaration" => Some(EntityKind::Enum),
        "struct_declaration" => Some(EntityKind::Struct),
        "property_declaration" => Some(EntityKind::Constant),
        "constructor_declaration" => Some(EntityKind::Function),
        "using_directive" => Some(EntityKind::Import),
        _ => None,
    }
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "using_directive" => {
            let text = node_text(node, source);
            Some(text.trim().trim_end_matches(';').to_string())
        }
        "property_declaration" => find_name_node(node, source),
        "namespace_declaration" => find_name_node(node, source),
        _ => find_name_node(node, source),
    }
}

fn extract_csharp_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "method_declaration" | "constructor_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).trim().to_string())
            }
        }
        "class_declaration"
        | "interface_declaration"
        | "struct_declaration"
        | "enum_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "namespace_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "property_declaration" => {
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

fn extract_csharp_doc_comment(node: &Node, source: &[u8]) -> Option<String> {
    // C# uses /// for XML doc comments and // for regular line comments
    // The tree-sitter-c-sharp grammar represents them as `comment` nodes
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        let kind = sib.kind();
        if kind == "comment" {
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
                .trim_start_matches("//")
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim()
                .to_string()
        })
        .collect();
    Some(cleaned.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_csharp(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = CSharpLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_class_and_method() {
        let src = r#"
/// A simple calculator.
public class Calculator
{
    /// Add two numbers.
    public int Add(int a, int b)
    {
        return a + b;
    }

    public int Subtract(int a, int b)
    {
        return a - b;
    }
}
"#;
        let (_, entities) = parse_csharp(src);
        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Calculator");
        assert!(
            classes[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("simple calculator")
        );

        let methods: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 2);
        assert!(methods.iter().any(|m| m.name == "Add"));
        assert!(methods.iter().any(|m| m.name == "Subtract"));
        assert!(methods[0].scope.contains(&"Calculator".to_string()));
    }

    #[test]
    fn extract_interface_and_namespace() {
        let src = r#"
namespace MyApp
{
    public interface IDrawable
    {
        void Draw();
    }
}
"#;
        let (_, entities) = parse_csharp(src);
        let namespaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Namespace)
            .collect();
        assert_eq!(namespaces.len(), 1);
        assert_eq!(namespaces[0].name, "MyApp");

        let ifaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "IDrawable");
        assert!(ifaces[0].scope.contains(&"MyApp".to_string()));
    }

    #[test]
    fn extract_enum_and_using() {
        let src = r#"
using System;
using System.Collections.Generic;

public enum Color
{
    Red,
    Green,
    Blue
}
"#;
        let (_, entities) = parse_csharp(src);
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
        assert_eq!(imports.len(), 2);
    }

    #[test]
    fn extract_struct_and_property() {
        let src = r#"
public struct Point
{
    public double X { get; set; }
    public double Y { get; set; }
}
"#;
        let (_, entities) = parse_csharp(src);
        let structs: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Point");

        let props: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .collect();
        assert_eq!(props.len(), 2);
    }
}
