use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, node_line_range, node_text,
};

/// Kotlin language support.
pub struct KotlinLanguage;

/// In tree-sitter-kotlin-ng, both classes and interfaces parse as
/// `class_declaration`.  Interfaces have an `interface` keyword child.
const ENTITY_KINDS: &[&str] = &[
    "class_declaration",
    "function_declaration",
    "object_declaration",
];

impl LanguageSupport for KotlinLanguage {
    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_kotlin_ng::LANGUAGE.into()
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
        extract_kotlin_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_kotlin_doc_comment(node, source)
    }
}

fn collect_entities(
    lang: &KotlinLanguage,
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
            type_relations: Vec::new(),
        };
        entities.push(entity);

        // Recurse into class/interface/object bodies with updated scope.
        if matches!(
            entity_kind,
            EntityKind::Class | EntityKind::Interface | EntityKind::Module
        ) {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "class_body" {
                    let mut body_cursor = child.walk();
                    for body_child in child.children(&mut body_cursor) {
                        collect_entities(lang, &body_child, source, &child_scope, entities);
                    }
                    return;
                }
            }
        }
    }

    // Recurse into children for non-entity or top-level nodes.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

/// Map a tree-sitter node kind to an `EntityKind`.
///
/// In tree-sitter-kotlin-ng, `class_declaration` is used for both classes and
/// interfaces.  We differentiate by looking for an `interface` keyword child.
fn match_entity_kind(node: &Node, kind: &str, source: &[u8]) -> Option<EntityKind> {
    match kind {
        "class_declaration" => {
            // Check if first keyword child is `interface`.
            if has_keyword_child(node, source, "interface") {
                Some(EntityKind::Interface)
            } else {
                Some(EntityKind::Class)
            }
        }
        "function_declaration" => Some(EntityKind::Function),
        "object_declaration" => Some(EntityKind::Module),
        _ => None,
    }
}

/// Return true if the node has a direct leaf child with the given text.
fn has_keyword_child(node: &Node, source: &[u8], keyword: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.child_count() == 0 && node_text(&child, source).trim() == keyword {
            return true;
        }
    }
    false
}

fn extract_entity_name(node: &Node, source: &[u8]) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(&name_node, source).to_string());
    }
    // Fallback: look for `simple_identifier` or `type_identifier` child.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "simple_identifier"
            || child.kind() == "type_identifier"
            || child.kind() == "identifier"
        {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

fn extract_kotlin_signature(node: &Node, source: &[u8]) -> Option<String> {
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
        "class_declaration" | "object_declaration" => {
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

fn extract_kotlin_doc_comment(node: &Node, source: &[u8]) -> Option<String> {
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        let kind = sib.kind();
        if kind == "multiline_comment" || kind == "line_comment" {
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

    fn parse_kotlin(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = KotlinLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_class_and_function() {
        let src = r#"
/** A simple calculator. */
class Calculator {
    /** Adds two numbers. */
    fun add(a: Int, b: Int): Int {
        return a + b
    }

    fun subtract(a: Int, b: Int): Int {
        return a - b
    }
}
"#;
        let (_, entities) = parse_kotlin(src);

        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Calculator");

        let functions: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(functions.len(), 2);
        assert!(functions.iter().any(|f| f.name == "add"));
        assert!(functions.iter().any(|f| f.name == "subtract"));
        // Functions should be scoped inside the class.
        assert!(
            functions
                .iter()
                .all(|f| f.scope.contains(&"Calculator".to_string()))
        );
    }

    #[test]
    fn extract_interface_and_object() {
        let src = r#"
interface Drawable {
    fun draw()
}

object Singleton {
    fun getInstance(): Singleton = this
}
"#;
        let (_, entities) = parse_kotlin(src);

        // In tree-sitter-kotlin-ng, `interface` parses as `class_declaration`
        // with an `interface` keyword child.  Our match_entity_kind maps it to Interface.
        let interfaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert!(!interfaces.is_empty(), "expected at least one interface");
        assert!(interfaces.iter().any(|i| i.name == "Drawable"));

        let objects: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert!(!objects.is_empty(), "expected at least one object");
        assert!(objects.iter().any(|o| o.name == "Singleton"));
    }
}
