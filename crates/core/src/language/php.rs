use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, extract_preceding_comments,
    node_line_range, node_text,
};

/// PHP language support (Tier 3 — uses C grammar as structural fallback).
///
/// PHP shares many C-like constructs (functions, structs via classes, enums).
/// Full PHP-native tree-sitter support is planned once `tree-sitter-php` is
/// available on crates.io. Until then the C grammar provides structural
/// chunking sufficient for BM25 + symbol-table extraction.
pub struct PhpLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_definition",
    "struct_specifier",
    "enum_specifier",
    "declaration",
];

impl LanguageSupport for PhpLanguage {
    fn language(&self) -> Language {
        Language::Php
    }

    /// Uses the C grammar as a structural approximation for PHP files.
    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_c::LANGUAGE.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        collect_entities(self, &tree.root_node(), source, &[], &mut entities);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        let text = node_text(node, source);
        let first = text.lines().next()?.trim();
        if first.is_empty() { None } else { Some(first.to_string()) }
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }
}

fn collect_entities(
    lang: &PhpLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind = node.kind();

    if let Some(entity_kind) = map_entity_kind(kind) {
        let name = extract_name(node, source).unwrap_or_default();
        entities.push(SemanticEntity {
            kind: entity_kind,
            name,
            signature: lang.extract_signature(node, source),
            doc_comment: lang.extract_doc_comment(node, source),
            byte_range: node.start_byte()..node.end_byte(),
            line_range: node_line_range(node),
            scope: scope.to_vec(),
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

fn map_entity_kind(kind: &str) -> Option<EntityKind> {
    match kind {
        "function_definition" => Some(EntityKind::Function),
        "struct_specifier" => Some(EntityKind::Struct),
        "enum_specifier" => Some(EntityKind::Enum),
        _ => None,
    }
}

fn extract_name(node: &Node, source: &[u8]) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("declarator") {
        let text = node_text(&name_node, source);
        let ident: String = text
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !ident.is_empty() {
            return Some(ident);
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}
