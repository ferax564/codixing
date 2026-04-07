use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// Ruby language support.
pub struct RubyLanguage;

const ENTITY_KINDS: &[&str] = &["class", "module", "method", "singleton_method"];

impl LanguageSupport for RubyLanguage {
    fn language(&self) -> Language {
        Language::Ruby
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_ruby::LANGUAGE.into()
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
        extract_ruby_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "#")
    }
}

fn collect_entities(
    lang: &RubyLanguage,
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
        };
        entities.push(entity);

        // Recurse into class/module bodies with updated scope.
        if matches!(entity_kind, EntityKind::Class | EntityKind::Module) {
            let mut child_scope = scope.to_vec();
            if !name.is_empty() {
                child_scope.push(name);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "body_statement" {
                    let mut body_cursor = child.walk();
                    for body_child in child.children(&mut body_cursor) {
                        collect_entities(lang, &body_child, source, &child_scope, entities);
                    }
                } else if child.kind() != "class"
                    && child.kind() != "module"
                    && child.kind() != "end"
                {
                    collect_entities(lang, &child, source, &child_scope, entities);
                }
            }
            return;
        }
    }

    // Recurse into children for non-entity nodes.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(lang, &child, source, scope, entities);
    }
}

fn match_entity_kind(kind: &str) -> Option<EntityKind> {
    match kind {
        "class" => Some(EntityKind::Class),
        "module" => Some(EntityKind::Module),
        "method" => Some(EntityKind::Method),
        "singleton_method" => Some(EntityKind::Method),
        _ => None,
    }
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "class" | "module" => {
            // class/module name is a constant node or scoped constant.
            if let Some(name_node) = node.child_by_field_name("name") {
                return Some(node_text(&name_node, source).to_string());
            }
            find_name_node(node, source)
        }
        "method" | "singleton_method" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                return Some(node_text(&name_node, source).to_string());
            }
            find_name_node(node, source)
        }
        _ => find_name_node(node, source),
    }
}

fn extract_ruby_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "method" | "singleton_method" => {
            let text = node_text(node, source);
            // Take first line (def line).
            let first_line = text.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                None
            } else {
                Some(first_line.to_string())
            }
        }
        "class" => {
            // First line: `class ClassName < SuperClass`
            let text = node_text(node, source);
            Some(text.lines().next().unwrap_or("").trim().to_string())
        }
        "module" => {
            let text = node_text(node, source);
            Some(text.lines().next().unwrap_or("").trim().to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ruby(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_ruby::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = RubyLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_class_with_methods() {
        let src = r#"
# A simple calculator class.
class Calculator
  # Adds two numbers.
  def add(a, b)
    a + b
  end

  def subtract(a, b)
    a - b
  end
end
"#;
        let (_, entities) = parse_ruby(src);

        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Calculator");
        assert!(
            classes[0]
                .signature
                .as_ref()
                .unwrap()
                .contains("Calculator")
        );

        let methods: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 2);
        assert!(methods.iter().any(|m| m.name == "add"));
        assert!(methods.iter().any(|m| m.name == "subtract"));
        // Methods should be scoped inside the class.
        assert!(
            methods
                .iter()
                .all(|m| m.scope.contains(&"Calculator".to_string()))
        );
    }

    #[test]
    fn extract_module_and_singleton_method() {
        let src = r#"
module Greetable
  def self.hello(name)
    "Hello, #{name}!"
  end
end
"#;
        let (_, entities) = parse_ruby(src);

        let modules: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].name, "Greetable");

        let methods: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        // singleton_method maps to Method
        assert!(!methods.is_empty());
        assert!(methods.iter().any(|m| m.name == "hello"));
    }
}
