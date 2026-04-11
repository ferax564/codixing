use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, Visibility, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// C++ language support.
pub struct CppLanguage;

const ENTITY_KINDS: &[&str] = &[
    "function_definition",
    "class_specifier",
    "struct_specifier",
    "enum_specifier",
    "namespace_definition",
    "template_declaration",
    "type_definition",
    "declaration",
    "preproc_include",
];

impl LanguageSupport for CppLanguage {
    fn language(&self) -> Language {
        Language::Cpp
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_cpp::LANGUAGE.into()
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
        extract_cpp_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }
}

fn collect_entities(
    lang: &CppLanguage,
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str, node, source) {
        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        // Skip anonymous structs/classes/enums
        if name.is_empty()
            && matches!(
                entity_kind,
                EntityKind::Struct | EntityKind::Class | EntityKind::Enum
            )
        {
            // Don't add, but do recurse
        } else {
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

            // Recurse into class/struct/namespace bodies with updated scope
            if matches!(
                entity_kind,
                EntityKind::Class | EntityKind::Struct | EntityKind::Namespace
            ) {
                let mut child_scope = scope.to_vec();
                if !name.is_empty() {
                    child_scope.push(name);
                }
                let body_kind = if entity_kind == EntityKind::Namespace {
                    "declaration_list"
                } else {
                    "field_declaration_list"
                };
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == body_kind {
                        let mut body_cursor = child.walk();
                        for body_child in child.children(&mut body_cursor) {
                            collect_entities(lang, &body_child, source, &child_scope, entities);
                        }
                    }
                }
                return;
            }
        }
    }

    // For template_declaration, recurse into the inner declaration
    if kind_str == "template_declaration" {
        // Already handled as an entity if matched, but we need to recurse
        // for the inner declaration which may also be matched
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
        "class_specifier" => {
            if let Some(parent) = node.parent() {
                if parent.kind() == "type_definition" {
                    return None;
                }
            }
            Some(EntityKind::Class)
        }
        "struct_specifier" => {
            if let Some(parent) = node.parent() {
                if parent.kind() == "type_definition" {
                    return None;
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
        "namespace_definition" => Some(EntityKind::Namespace),
        "template_declaration" => {
            // Classify based on the inner declaration
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_definition" => return Some(EntityKind::Function),
                    "class_specifier" => return Some(EntityKind::Class),
                    "struct_specifier" => return Some(EntityKind::Struct),
                    "declaration" => {
                        if is_function_declaration(&child, source) {
                            return Some(EntityKind::Function);
                        }
                        return Some(EntityKind::Class);
                    }
                    _ => {}
                }
            }
            None
        }
        "type_definition" => Some(EntityKind::TypeAlias),
        "declaration" => {
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
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return find_declarator_name(&declarator, source);
            }
            None
        }
        "class_specifier" | "struct_specifier" | "enum_specifier" => find_name_node(node, source),
        "namespace_definition" => find_name_node(node, source),
        "template_declaration" => {
            // Find the name in the inner declaration
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_definition" => {
                        if let Some(decl) = child.child_by_field_name("declarator") {
                            return find_declarator_name(&decl, source);
                        }
                    }
                    "class_specifier" | "struct_specifier" => {
                        return find_name_node(&child, source);
                    }
                    "declaration" => {
                        if let Some(decl) = child.child_by_field_name("declarator") {
                            return find_declarator_name(&decl, source);
                        }
                        return find_name_node(&child, source);
                    }
                    _ => {}
                }
            }
            None
        }
        "type_definition" => {
            if let Some(declarator) = node.child_by_field_name("declarator") {
                return Some(node_text(&declarator, source).to_string());
            }
            None
        }
        "declaration" => {
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
    let nk = node.kind();
    if nk == "identifier" || nk == "field_identifier" || nk == "qualified_identifier" {
        return Some(node_text(node, source).to_string());
    }
    if nk == "destructor_name" || nk == "operator_name" {
        return Some(node_text(node, source).to_string());
    }
    // Check the "declarator" field first
    if let Some(decl) = node.child_by_field_name("declarator") {
        return find_declarator_name(&decl, source);
    }
    // Fallback: check children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier"
            || child.kind() == "field_identifier"
            || child.kind() == "qualified_identifier"
        {
            return Some(node_text(&child, source).to_string());
        }
        if child.kind() == "function_declarator"
            || child.kind() == "pointer_declarator"
            || child.kind() == "reference_declarator"
        {
            if let Some(name) = find_declarator_name(&child, source) {
                return Some(name);
            }
        }
    }
    None
}

fn extract_cpp_signature(node: &Node, source: &[u8]) -> Option<String> {
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
        "class_specifier" | "struct_specifier" | "enum_specifier" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "namespace_definition" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "template_declaration" => {
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
        "declaration" => {
            let text = node_text(node, source);
            Some(text.trim().trim_end_matches(';').to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_cpp(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = CppLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_class_and_function() {
        let src = r#"
// A simple class.
class Point {
public:
    double x;
    double y;
};

// Calculate distance.
double distance(Point a, Point b) {
    return 0.0;
}
"#;
        let (_, entities) = parse_cpp(src);
        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Point");
        assert!(
            classes[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("simple class")
        );

        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "distance");
    }

    #[test]
    fn extract_namespace() {
        let src = r#"
namespace math {
    int add(int a, int b) {
        return a + b;
    }
}
"#;
        let (_, entities) = parse_cpp(src);
        let namespaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Namespace)
            .collect();
        assert_eq!(namespaces.len(), 1);
        assert_eq!(namespaces[0].name, "math");

        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
        // Function should have namespace scope
        assert!(fns[0].scope.contains(&"math".to_string()));
    }

    #[test]
    fn extract_template_and_include() {
        let src = r#"
#include <vector>

template<typename T>
T max(T a, T b) {
    return a > b ? a : b;
}
"#;
        let (_, entities) = parse_cpp(src);
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);

        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        // Should find the template function
        assert!(!fns.is_empty());
        assert!(fns.iter().any(|f| f.name == "max"));
    }
}
