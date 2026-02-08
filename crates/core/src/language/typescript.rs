use tree_sitter::{Node, Tree};

use super::{
    EntityKind, Language, LanguageSupport, SemanticEntity, extract_preceding_comments,
    find_name_node, node_line_range, node_text,
};

/// TypeScript language support.
pub struct TypeScriptLanguage;

/// TSX language support.
pub struct TsxLanguage;

/// JavaScript language support.
pub struct JavaScriptLanguage;

const TS_ENTITY_KINDS: &[&str] = &[
    "function_declaration",
    "class_declaration",
    "interface_declaration",
    "type_alias_declaration",
    "method_definition",
    "export_statement",
    "import_statement",
    "lexical_declaration",
];

const JS_ENTITY_KINDS: &[&str] = &[
    "function_declaration",
    "class_declaration",
    "method_definition",
    "export_statement",
    "import_statement",
    "lexical_declaration",
];

impl LanguageSupport for TypeScriptLanguage {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        TS_ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let root = tree.root_node();
        collect_entities(&root, source, &[], &mut entities, true);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_ts_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }
}

impl LanguageSupport for TsxLanguage {
    fn language(&self) -> Language {
        Language::Tsx
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        TS_ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let root = tree.root_node();
        collect_entities(&root, source, &[], &mut entities, true);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_ts_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }
}

impl LanguageSupport for JavaScriptLanguage {
    fn language(&self) -> Language {
        Language::JavaScript
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_javascript::LANGUAGE.into()
    }

    fn entity_node_kinds(&self) -> &[&str] {
        JS_ENTITY_KINDS
    }

    fn extract_entities(&self, tree: &Tree, source: &[u8]) -> Vec<SemanticEntity> {
        let mut entities = Vec::new();
        let root = tree.root_node();
        collect_entities(&root, source, &[], &mut entities, false);
        entities
    }

    fn extract_signature(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_ts_signature(node, source)
    }

    fn extract_doc_comment(&self, node: &Node, source: &[u8]) -> Option<String> {
        extract_preceding_comments(node, source, "//")
    }
}

fn collect_entities(
    node: &Node,
    source: &[u8],
    scope: &[String],
    entities: &mut Vec<SemanticEntity>,
    is_typescript: bool,
) {
    let kind_str = node.kind();

    if let Some(entity_kind) = match_entity_kind(kind_str, node, source, is_typescript) {
        let name = extract_entity_name(node, source, kind_str).unwrap_or_default();

        // Skip if this is a lexical_declaration that's not an arrow function / const function
        if kind_str == "lexical_declaration"
            && entity_kind == EntityKind::Function
            && name.is_empty()
        {
            // Not a const arrow function, skip
        } else {
            let entity = SemanticEntity {
                kind: entity_kind.clone(),
                name: name.clone(),
                signature: extract_ts_signature(node, source),
                doc_comment: extract_preceding_comments(node, source, "//"),
                byte_range: node.start_byte()..node.end_byte(),
                line_range: node_line_range(node),
                scope: scope.to_vec(),
            };
            entities.push(entity);

            // Recurse into class/interface bodies with updated scope
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
                    if child.kind() == "class_body"
                        || child.kind() == "object"
                        || child.kind() == "statement_block"
                    {
                        let mut body_cursor = child.walk();
                        for body_child in child.children(&mut body_cursor) {
                            collect_entities(
                                &body_child,
                                source,
                                &child_scope,
                                entities,
                                is_typescript,
                            );
                        }
                    }
                }
                return;
            }
        }
    }

    // For export_statement, recurse into the exported declaration
    if kind_str == "export_statement" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let child_kind = child.kind();
            if child_kind != "export" && child_kind != "default" {
                collect_entities(&child, source, scope, entities, is_typescript);
            }
        }
        return;
    }

    // Recurse into children for non-entity or top-level nodes
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_entities(&child, source, scope, entities, is_typescript);
    }
}

fn match_entity_kind(
    kind: &str,
    node: &Node,
    source: &[u8],
    is_typescript: bool,
) -> Option<EntityKind> {
    match kind {
        "function_declaration" => Some(EntityKind::Function),
        "class_declaration" => Some(EntityKind::Class),
        "method_definition" => Some(EntityKind::Method),
        "interface_declaration" if is_typescript => Some(EntityKind::Interface),
        "type_alias_declaration" if is_typescript => Some(EntityKind::TypeAlias),
        "import_statement" => Some(EntityKind::Import),
        "lexical_declaration" => {
            // Check if this is a `const foo = (...) => ...` pattern
            if is_arrow_function(node, source) {
                Some(EntityKind::Function)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Check if a lexical_declaration contains an arrow function.
fn is_arrow_function(node: &Node, _source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(value) = child.child_by_field_name("value") {
                return value.kind() == "arrow_function";
            }
        }
    }
    false
}

fn extract_entity_name(node: &Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "import_statement" => {
            let text = node_text(node, source);
            Some(text.trim().trim_end_matches(';').to_string())
        }
        "lexical_declaration" => {
            // For `const foo = () => {}`, extract "foo"
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    return find_name_node(&child, source);
                }
            }
            None
        }
        _ => find_name_node(node, source),
    }
}

fn extract_ts_signature(node: &Node, source: &[u8]) -> Option<String> {
    let kind = node.kind();
    match kind {
        "function_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "class_declaration" | "interface_declaration" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "method_definition" => {
            let text = node_text(node, source);
            if let Some(brace) = text.find('{') {
                Some(text[..brace].trim().to_string())
            } else {
                Some(text.lines().next().unwrap_or(text).to_string())
            }
        }
        "type_alias_declaration" => {
            let text = node_text(node, source);
            Some(text.lines().next().unwrap_or(text).trim().to_string())
        }
        "lexical_declaration" => {
            // For const arrow functions: `const foo = (x: number): string =>`
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ts(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = TypeScriptLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    fn parse_js(source: &str) -> (Tree, Vec<SemanticEntity>) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let lang = JavaScriptLanguage;
        let entities = lang.extract_entities(&tree, source.as_bytes());
        (tree, entities)
    }

    #[test]
    fn extract_ts_function_and_class() {
        let src = r#"
// Greet someone.
function greet(name: string): string {
    return `Hello, ${name}!`;
}

class Greeter {
    greeting: string;

    constructor(message: string) {
        this.greeting = message;
    }

    greet(): string {
        return this.greeting;
    }
}
"#;
        let (_, entities) = parse_ts(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "greet");
        assert!(
            fns[0]
                .signature
                .as_ref()
                .unwrap()
                .contains("function greet")
        );
        assert!(
            fns[0]
                .doc_comment
                .as_ref()
                .unwrap()
                .contains("Greet someone")
        );

        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Greeter");

        let methods: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert!(methods.len() >= 1);
    }

    #[test]
    fn extract_ts_interface_and_type() {
        let src = r#"
interface Printable {
    print(): void;
}

type StringOrNumber = string | number;
"#;
        let (_, entities) = parse_ts(src);
        let ifaces: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Interface)
            .collect();
        assert_eq!(ifaces.len(), 1);
        assert_eq!(ifaces[0].name, "Printable");

        let types: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::TypeAlias)
            .collect();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "StringOrNumber");
    }

    #[test]
    fn extract_ts_arrow_function() {
        let src = r#"
const add = (a: number, b: number): number => {
    return a + b;
};
"#;
        let (_, entities) = parse_ts(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "add");
    }

    #[test]
    fn extract_ts_import() {
        let src = r#"
import { readFile } from "fs";
"#;
        let (_, entities) = parse_ts(src);
        let imports: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
    }

    #[test]
    fn extract_js_function() {
        let src = r#"
function hello(name) {
    return "Hello, " + name;
}
"#;
        let (_, entities) = parse_js(src);
        let fns: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "hello");
    }
}
