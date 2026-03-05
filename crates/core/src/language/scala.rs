use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, find_name_node, node_line_range,
    node_text,
};

/// Scala language support.
pub struct ScalaLanguage;

const ENTITY_KINDS: &[&str] = &[
    "class_definition",
    "object_definition",
    "trait_definition",
    "function_definition",
    "val_definition",
];

impl LanguageSupport for ScalaLanguage {
    fn language(&self) -> Language {
        Language::Scala
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_scala::LANGUAGE.into()
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
        extract_scala_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_scala_doc_comment(node, source)
    }
}

fn collect_entities(
    lang: &ScalaLanguage,
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
        };
        entities.push(entity);

        // Recurse into class/object/trait template bodies with updated scope.
        if matches!(
            entity_kind,
            EntityKind::Class | EntityKind::Module | EntityKind::Interface
        ) {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "template_body" {
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

fn match_entity_kind(kind: &str) -> Option<EntityKind> {
    match kind {
        "class_definition" => Some(EntityKind::Class),
        "object_definition" => Some(EntityKind::Module),
        "trait_definition" => Some(EntityKind::Interface),
        "function_definition" => Some(EntityKind::Function),
        "val_definition" => Some(EntityKind::Constant),
        _ => None,
    }
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "val_definition" => {
            // val name: Type = ...
            if let Some(pattern) = node.child_by_field_name("pattern") {
                return Some(node_text(&pattern, source).to_string());
            }
            find_name_node(node, source)
        }
        _ => {
            if let Some(name_node) = node.child_by_field_name("name") {
                return Some(node_text(&name_node, source).to_string());
            }
            // Fallback: look for identifier child.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" || child.kind() == "type_identifier" {
                    return Some(node_text(&child, source).to_string());
                }
            }
            find_name_node(node, source)
        }
    }
}

fn extract_scala_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_definition" => {
            let text = node_text(node, source);
            // Take everything before `=` (expression body) or `{` (block body).
            let stop = text
                .find('{')
                .or_else(|| text.find(" = "))
                .unwrap_or(text.len());
            let sig = text[..stop].trim();
            if sig.is_empty() {
                None
            } else {
                Some(sig.to_string())
            }
        }
        "class_definition" | "object_definition" | "trait_definition" => {
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

fn extract_scala_doc_comment(node: &Node, source: &[u8]) -> Option<String> {
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        let kind = sib.kind();
        if kind == "block_comment" || kind == "line_comment" || kind == "comment" {
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

    fn parse_scala(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_scala::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = ScalaLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_class_and_function() {
        let src = r#"
/** A simple calculator. */
class Calculator {
    /** Adds two numbers. */
    def add(a: Int, b: Int): Int = a + b

    def subtract(a: Int, b: Int): Int = a - b
}
"#;
        let (_, entities) = parse_scala(src);

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
    }

    #[test]
    fn extract_object_and_trait() {
        let src = r#"
trait Drawable {
    def draw(): Unit
}

object Main {
    def main(args: Array[String]): Unit = {
        println("Hello, world!")
    }
}
"#;
        let (_, entities) = parse_scala(src);

        let traits: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert!(!traits.is_empty(), "expected at least one trait");
        assert!(traits.iter().any(|t| t.name == "Drawable"));

        let objects: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert!(!objects.is_empty(), "expected at least one object");
        assert!(objects.iter().any(|o| o.name == "Main"));
    }
}
