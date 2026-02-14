use crate::language::{Language, LanguageRegistry};

use super::{ReferenceKind, SymbolKind};

/// A definition found in source code (function, struct, enum, trait, impl, etc.).
#[derive(Debug, Clone)]
pub struct DefinitionInfo {
    /// Name of the defined symbol.
    pub name: String,
    /// What kind of symbol this is.
    pub kind: SymbolKind,
    /// File path where the definition was found.
    pub file: String,
    /// 0-indexed line number of the definition.
    pub line: usize,
}

/// A reference found in source code (function call, use declaration, etc.).
#[derive(Debug, Clone)]
pub struct ReferenceInfo {
    /// Name of the referenced symbol.
    pub target_name: String,
    /// What kind of reference this is.
    pub kind: ReferenceKind,
    /// File path where the reference was found.
    pub file: String,
    /// 0-indexed line number of the reference.
    pub line: usize,
}

/// Extract all definitions (functions, structs, enums, traits, impls) from source code.
///
/// Creates a fresh `tree_sitter::Parser` per call (the type is `!Send`).
/// Returns an empty `Vec` if the language is unsupported or parsing fails.
pub fn extract_definitions(source: &str, file_path: &str, lang: &Language) -> Vec<DefinitionInfo> {
    let registry = LanguageRegistry::new();
    let lang_support = match registry.get(*lang) {
        Some(ls) => ls,
        None => return Vec::new(),
    };

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&lang_support.tree_sitter_language())
        .is_err()
    {
        return Vec::new();
    }

    let tree = match parser.parse(source.as_bytes(), None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut defs = Vec::new();
    collect_definitions(
        &tree.root_node(),
        source.as_bytes(),
        file_path,
        lang,
        &mut defs,
    );
    defs
}

/// Extract all references (function calls, use declarations) from source code.
///
/// Creates a fresh `tree_sitter::Parser` per call (the type is `!Send`).
/// Returns an empty `Vec` if the language is unsupported or parsing fails.
pub fn extract_references(source: &str, file_path: &str, lang: &Language) -> Vec<ReferenceInfo> {
    let registry = LanguageRegistry::new();
    let lang_support = match registry.get(*lang) {
        Some(ls) => ls,
        None => return Vec::new(),
    };

    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&lang_support.tree_sitter_language())
        .is_err()
    {
        return Vec::new();
    }

    let tree = match parser.parse(source.as_bytes(), None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let mut refs = Vec::new();
    collect_references(
        &tree.root_node(),
        source.as_bytes(),
        file_path,
        lang,
        &mut refs,
    );
    refs
}

// ---------------------------------------------------------------------------
// Rust definition extraction
// ---------------------------------------------------------------------------

fn collect_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    lang: &Language,
    defs: &mut Vec<DefinitionInfo>,
) {
    if *lang == Language::Rust {
        collect_rust_definitions(node, source, file_path, defs);
    }
    // Other languages will be added in Task 11
}

fn collect_rust_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    defs: &mut Vec<DefinitionInfo>,
) {
    let kind_str = node.kind();

    if let Some(sym_kind) = rust_def_kind(kind_str) {
        let name = rust_def_name(node, source, kind_str);
        if let Some(name) = name {
            defs.push(DefinitionInfo {
                name,
                kind: sym_kind,
                file: file_path.to_string(),
                line: node.start_position().row,
            });
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_definitions(&child, source, file_path, defs);
    }
}

/// Map a tree-sitter-rust node kind to our SymbolKind.
fn rust_def_kind(kind: &str) -> Option<SymbolKind> {
    match kind {
        "function_item" => Some(SymbolKind::Function),
        "struct_item" => Some(SymbolKind::Struct),
        "enum_item" => Some(SymbolKind::Enum),
        "trait_item" => Some(SymbolKind::Trait),
        "impl_item" => Some(SymbolKind::Type), // impl blocks mapped to Type
        _ => None,
    }
}

/// Extract the name of a Rust definition node.
fn rust_def_name(node: &tree_sitter::Node, source: &[u8], kind: &str) -> Option<String> {
    match kind {
        "impl_item" => {
            // For `impl Foo { ... }` extract "Foo"
            // For `impl Trait for Type { ... }` extract "Type"
            if let Some(type_node) = node.child_by_field_name("type") {
                Some(node_text(&type_node, source))
            } else {
                // Fallback: look for a type_identifier child
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "type_identifier" {
                        return Some(node_text(&child, source));
                    }
                }
                None
            }
        }
        _ => {
            // Most definitions have a "name" field
            if let Some(name_node) = node.child_by_field_name("name") {
                Some(node_text(&name_node, source))
            } else {
                // Fallback: first identifier or type_identifier child
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "identifier" || child.kind() == "type_identifier" {
                        return Some(node_text(&child, source));
                    }
                }
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rust reference extraction
// ---------------------------------------------------------------------------

fn collect_references(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    lang: &Language,
    refs: &mut Vec<ReferenceInfo>,
) {
    if *lang == Language::Rust {
        collect_rust_references(node, source, file_path, refs);
    }
    // Other languages will be added in Task 11
}

fn collect_rust_references(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    refs: &mut Vec<ReferenceInfo>,
) {
    let kind_str = node.kind();

    match kind_str {
        "call_expression" => {
            if let Some(name) = rust_call_target(node, source) {
                refs.push(ReferenceInfo {
                    target_name: name,
                    kind: ReferenceKind::Call,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
            // Still recurse into children (calls can be nested)
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_rust_references(&child, source, file_path, refs);
            }
        }
        "use_declaration" => {
            let text = node_text(node, source);
            let path = text.trim_start_matches("use ").trim_end_matches(';').trim();
            if !path.is_empty() {
                refs.push(ReferenceInfo {
                    target_name: path.to_string(),
                    kind: ReferenceKind::Import,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
            // No need to recurse into use declarations
        }
        _ => {
            // Recurse into children
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_rust_references(&child, source, file_path, refs);
            }
        }
    }
}

/// Extract the target name from a Rust call expression.
///
/// Handles:
/// - Simple calls: `foo()` -> "foo"
/// - Scoped calls: `std::io::read()` -> "std::io::read"
/// - Method calls via `field_expression`: `obj.method()` -> "method"
/// - Macro calls: `println!()` -> "println!" (via `macro_invocation`)
fn rust_call_target(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let func_node = node.child_by_field_name("function")?;
    match func_node.kind() {
        "identifier" => Some(node_text(&func_node, source)),
        "scoped_identifier" | "scoped_type_identifier" => Some(node_text(&func_node, source)),
        "field_expression" => {
            // obj.method() -> extract "method"
            if let Some(field) = func_node.child_by_field_name("field") {
                Some(node_text(&field, source))
            } else {
                Some(node_text(&func_node, source))
            }
        }
        "generic_function" => {
            // turbofish: foo::<T>() -> extract "foo"
            if let Some(func_inner) = func_node.child_by_field_name("function") {
                Some(node_text(&func_inner, source))
            } else {
                Some(node_text(&func_node, source))
            }
        }
        _ => Some(node_text(&func_node, source)),
    }
}

/// Get the text of a tree-sitter node from source bytes.
fn node_text(node: &tree_sitter::Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_rust_function_definitions() {
        let src = r#"
fn main() {
    println!("hello");
}

pub fn helper(x: i32) -> i32 {
    x + 1
}

fn another() {}
"#;
        let defs = extract_definitions(src, "src/main.rs", &Language::Rust);
        let fns: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Function)
            .collect();
        assert_eq!(fns.len(), 3);

        let names: Vec<&str> = fns.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"another"));

        // Check file path is set correctly
        assert!(fns.iter().all(|d| d.file == "src/main.rs"));

        // Check lines are distinct and reasonable
        for f in &fns {
            assert!(f.line < 20, "line {} should be small", f.line);
        }
    }

    #[test]
    fn extract_rust_struct_definitions() {
        let src = r#"
pub struct Point {
    pub x: f64,
    pub y: f64,
}

struct Config {
    verbose: bool,
}
"#;
        let defs = extract_definitions(src, "src/types.rs", &Language::Rust);
        let structs: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Struct)
            .collect();
        assert_eq!(structs.len(), 2);

        let names: Vec<&str> = structs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"Point"));
        assert!(names.contains(&"Config"));
    }

    #[test]
    fn extract_rust_enum_and_trait_definitions() {
        let src = r#"
pub enum Color {
    Red,
    Green,
    Blue,
}

pub trait Drawable {
    fn draw(&self);
}
"#;
        let defs = extract_definitions(src, "src/lib.rs", &Language::Rust);

        let enums: Vec<_> = defs.iter().filter(|d| d.kind == SymbolKind::Enum).collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].name, "Color");

        let traits: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Trait)
            .collect();
        assert_eq!(traits.len(), 1);
        assert_eq!(traits[0].name, "Drawable");
    }

    #[test]
    fn extract_rust_impl_definitions() {
        let src = r#"
struct Foo;

impl Foo {
    fn new() -> Self { Foo }
    fn bar(&self) {}
}
"#;
        let defs = extract_definitions(src, "src/foo.rs", &Language::Rust);

        // Should have: struct Foo, impl Foo, fn new, fn bar
        let impls: Vec<_> = defs.iter().filter(|d| d.kind == SymbolKind::Type).collect();
        assert_eq!(impls.len(), 1);
        assert_eq!(impls[0].name, "Foo");

        let fns: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Function)
            .collect();
        assert_eq!(fns.len(), 2);
        let fn_names: Vec<&str> = fns.iter().map(|d| d.name.as_str()).collect();
        assert!(fn_names.contains(&"new"));
        assert!(fn_names.contains(&"bar"));
    }

    #[test]
    fn extract_rust_call_references() {
        let src = r#"
use std::collections::HashMap;

fn main() {
    let x = helper(42);
    let y = std::cmp::max(x, 10);
    let v = Vec::new();
    println!("done");
}

fn helper(n: i32) -> i32 { n }
"#;
        let refs = extract_references(src, "src/main.rs", &Language::Rust);

        // Should have: use import, helper(), std::cmp::max(), Vec::new()
        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        let call_names: Vec<&str> = calls.iter().map(|r| r.target_name.as_str()).collect();
        assert!(
            call_names.contains(&"helper"),
            "expected 'helper' in {call_names:?}"
        );
        assert!(
            call_names.iter().any(|n| n.contains("max")),
            "expected call containing 'max' in {call_names:?}"
        );
        assert!(
            call_names.iter().any(|n| n.contains("new")),
            "expected call containing 'new' in {call_names:?}"
        );

        // Check use imports
        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
        assert!(imports[0].target_name.contains("HashMap"));
    }

    #[test]
    fn extract_rust_method_call_references() {
        let src = r#"
fn process() {
    let v = vec![1, 2, 3];
    let total = v.iter().sum();
    let s = String::from("hello");
    let upper = s.to_uppercase();
}
"#;
        let refs = extract_references(src, "src/lib.rs", &Language::Rust);
        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        let call_names: Vec<&str> = calls.iter().map(|r| r.target_name.as_str()).collect();

        // Method calls should extract the method name
        assert!(
            call_names.contains(&"iter"),
            "expected 'iter' in {call_names:?}"
        );
        assert!(
            call_names.contains(&"sum"),
            "expected 'sum' in {call_names:?}"
        );
        assert!(
            call_names.contains(&"to_uppercase"),
            "expected 'to_uppercase' in {call_names:?}"
        );
        assert!(
            call_names.iter().any(|n| n.contains("from")),
            "expected 'from' call in {call_names:?}"
        );
    }

    #[test]
    fn extract_rust_use_declaration_references() {
        let src = r#"
use std::io;
use std::collections::HashMap;
use crate::config::IndexConfig;
"#;
        let refs = extract_references(src, "src/lib.rs", &Language::Rust);
        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert_eq!(imports.len(), 3);

        let import_names: Vec<&str> = imports.iter().map(|r| r.target_name.as_str()).collect();
        assert!(
            import_names.contains(&"std::io"),
            "expected 'std::io' in {import_names:?}"
        );
        assert!(
            import_names.contains(&"std::collections::HashMap"),
            "expected 'std::collections::HashMap' in {import_names:?}"
        );
        assert!(
            import_names.iter().any(|n| n.contains("IndexConfig")),
            "expected import containing 'IndexConfig' in {import_names:?}"
        );
    }

    #[test]
    fn empty_source_produces_no_results() {
        let defs = extract_definitions("", "empty.rs", &Language::Rust);
        assert!(defs.is_empty());

        let refs = extract_references("", "empty.rs", &Language::Rust);
        assert!(refs.is_empty());
    }

    #[test]
    fn definitions_include_correct_line_numbers() {
        let src = "fn first() {}\nfn second() {}\nfn third() {}\n";
        let defs = extract_definitions(src, "test.rs", &Language::Rust);
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0].line, 0);
        assert_eq!(defs[0].name, "first");
        assert_eq!(defs[1].line, 1);
        assert_eq!(defs[1].name, "second");
        assert_eq!(defs[2].line, 2);
        assert_eq!(defs[2].name, "third");
    }

    #[test]
    fn extract_rust_nested_calls() {
        let src = r#"
fn compute() {
    let result = outer(inner(42));
}
fn outer(x: i32) -> i32 { x }
fn inner(x: i32) -> i32 { x }
"#;
        let refs = extract_references(src, "test.rs", &Language::Rust);
        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        let call_names: Vec<&str> = calls.iter().map(|r| r.target_name.as_str()).collect();
        assert!(
            call_names.contains(&"outer"),
            "expected 'outer' in {call_names:?}"
        );
        assert!(
            call_names.contains(&"inner"),
            "expected 'inner' in {call_names:?}"
        );
    }
}
