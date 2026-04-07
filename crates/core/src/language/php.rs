use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, node_line_range, node_text,
};

/// PHP language support using the native `tree-sitter-php` grammar.
pub struct PhpLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_definition",
    "class_declaration",
    "interface_declaration",
    "trait_declaration",
    "enum_declaration",
    "namespace_definition",
    "namespace_use_declaration",
    "const_declaration",
    "method_declaration",
];

impl LanguageSupport for PhpLanguage {
    fn language(&self) -> Language {
        Language::Php
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_php::LANGUAGE_PHP.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let root = tree.root_node();
        collect_entities(&root, source, &[], &mut entities);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_php_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_php_doc_comment(node, source)
    }
}

fn collect_entities(
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str) {
        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        entities.push(SemanticEntity {
            kind: entity_kind.clone(),
            name: name.clone(),
            signature: extract_php_signature(node, source),
            doc_comment: extract_php_doc_comment(node, source),
            byte_range: node.start_byte()..node.end_byte(),
            line_range: node_line_range(node),
            scope: scope.to_vec(),
            visibility: Visibility::default(),
        });

        // Recurse into class/interface/trait/enum bodies with updated scope.
        if matches!(
            entity_kind,
            EntityKind::Class
                | EntityKind::Interface
                | EntityKind::Trait
                | EntityKind::Enum
                | EntityKind::Namespace
        ) {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                let ck = child.kind();
                if ck == "declaration_list" || ck == "enum_declaration_list" {
                    let mut body_cursor = child.walk();
                    for body_child in child.children(&mut body_cursor) {
                        collect_entities(&body_child, source, &child_scope, entities);
                    }
                }
            }
            return;
        }
    }

    // Also handle `expression_statement` containing require/include.
    if kind_str == "expression_statement" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let ck = child.kind();
            if ck == "require_expression"
                || ck == "require_once_expression"
                || ck == "include_expression"
                || ck == "include_once_expression"
            {
                let name = node_text(&child, source).trim().to_string();
                entities.push(SemanticEntity {
                    kind: EntityKind::Import,
                    name,
                    signature: Some(node_text(&child, source).trim().to_string()),
                    doc_comment: None,
                    byte_range: child.start_byte()..child.end_byte(),
                    line_range: node_line_range(&child),
                    scope: scope.to_vec(),
                    visibility: Visibility::default(),
                });
            }
        }
    }

    // Recurse into children for non-entity or top-level nodes.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(&child, source, scope, entities);
    }
}

fn match_entity_kind(kind: &str) -> Option<EntityKind> {
    match kind {
        "function_definition" => Some(EntityKind::Function),
        "class_declaration" => Some(EntityKind::Class),
        "interface_declaration" => Some(EntityKind::Interface),
        "trait_declaration" => Some(EntityKind::Trait),
        "enum_declaration" => Some(EntityKind::Enum),
        "namespace_definition" => Some(EntityKind::Namespace),
        "namespace_use_declaration" => Some(EntityKind::Import),
        "const_declaration" => Some(EntityKind::Constant),
        "method_declaration" => Some(EntityKind::Method),
        _ => None,
    }
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "namespace_use_declaration" => {
            // Return the full use statement text.
            let text = node_text(node, source)
                .trim()
                .trim_end_matches(';')
                .to_string();
            Some(text)
        }
        "namespace_definition" => {
            // Extract namespace name from `namespace_name` child.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "namespace_name" {
                    return Some(node_text(&child, source).to_string());
                }
            }
            None
        }
        "const_declaration" => {
            // const_element has a `name` child.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "const_element" {
                    let mut inner = child.walk();
                    for ic in child.children(&mut inner) {
                        if ic.kind() == "name" {
                            return Some(node_text(&ic, source).to_string());
                        }
                    }
                }
            }
            None
        }
        _ => {
            // Most declarations have a `name` child.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "name" {
                    return Some(node_text(&child, source).to_string());
                }
            }
            None
        }
    }
}

fn extract_php_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_definition" | "method_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                // Abstract methods end with `;`.
                Some(
                    text.lines()
                        .next()
                        .unwrap_or(text)
                        .trim()
                        .trim_end_matches(';')
                        .to_string(),
                )
            }
        }
        "class_declaration"
        | "interface_declaration"
        | "trait_declaration"
        | "enum_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "namespace_definition" => {
            let text = node_text(node, source);
            // namespace declarations end with `;` or `{`.
            if let Some(semi) = text.find(';') {
                Some(text[..=semi].trim().to_string())
            } else if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "const_declaration" | "namespace_use_declaration" => {
            let text = node_text(node, source);
            Some(text.trim().trim_end_matches(';').to_string())
        }
        _ => None,
    }
}

fn extract_php_doc_comment(node: &Node, source: &[u8]) -> Option<String> {
    // Look for preceding comment nodes (PHPDoc `/** ... */` or `//` line comments).
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
            c.trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_start_matches("//")
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

    fn parse_php(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = PhpLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_class_and_methods() {
        let src = r#"<?php
/** A user model. */
class User extends Model {
    public function getName(): string {
        return $this->name;
    }

    private function validate(): bool {
        return true;
    }
}
"#;
        let (_, entities) = parse_php(src);
        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "User");
        assert!(
            classes[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("user model")
        );

        let methods: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 2);
        assert!(methods.iter().any(|m| m.name == "getName"));
        assert!(methods.iter().any(|m| m.name == "validate"));
        // Methods should be scoped inside the class.
        assert!(
            methods
                .iter()
                .all(|m| m.scope.contains(&"User".to_string()))
        );
    }

    #[test]
    fn extract_interface_and_trait() {
        let src = r#"<?php
interface Authenticatable {
    public function authenticate(): bool;
}

trait HasFactory {
    public static function factory(): self {
        return new static();
    }
}
"#;
        let (_, entities) = parse_php(src);
        let ifaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "Authenticatable");

        let traits: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Trait)
            .collect();
        assert_eq!(traits.len(), 1);
        assert_eq!(traits[0].name, "HasFactory");
    }

    #[test]
    fn extract_namespace_and_use() {
        let src = r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;
"#;
        let (_, entities) = parse_php(src);
        let namespaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Namespace)
            .collect();
        assert_eq!(namespaces.len(), 1);
        assert!(namespaces[0].name.contains("App"));

        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert!(!imports.is_empty());
    }

    #[test]
    fn extract_function_and_const() {
        let src = r#"<?php
const VERSION = '1.0';

function helper(): void {}
"#;
        let (_, entities) = parse_php(src);
        let consts: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .collect();
        assert_eq!(consts.len(), 1);
        assert_eq!(consts[0].name, "VERSION");

        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "helper");
    }

    #[test]
    fn extract_enum() {
        let src = r#"<?php
enum Status {
    case Active;
    case Inactive;
}
"#;
        let (_, entities) = parse_php(src);
        let enums: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Enum)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Status");
    }

    #[test]
    fn extract_require_include() {
        let src = r#"<?php
require_once 'vendor/autoload.php';
include 'config.php';
"#;
        let (_, entities) = parse_php(src);
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 2);
    }
}
