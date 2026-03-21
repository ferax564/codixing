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
    match lang {
        Language::Rust => collect_rust_definitions(node, source, file_path, defs),
        Language::Python => collect_python_definitions(node, source, file_path, defs),
        Language::TypeScript => collect_typescript_definitions(node, source, file_path, defs),
        Language::Go => collect_go_definitions(node, source, file_path, defs),
        _ => {} // Other languages not yet supported for graph extraction
    }
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

    // For `impl Trait for Type`, also extract the trait name so that
    // trait methods defined in the impl block can be linked back to the trait.
    if kind_str == "impl_item" {
        if let Some(trait_name) = rust_impl_trait_name(node, source) {
            // Extract each method inside the impl block with a qualified name
            // "TraitName::method" so the symbol graph can link trait method calls
            // through the impl to the concrete definition.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "declaration_list" {
                    let mut inner_cursor = child.walk();
                    for item in child.children(&mut inner_cursor) {
                        if item.kind() == "function_item" {
                            if let Some(name_node) = item.child_by_field_name("name") {
                                let method_name = node_text(&name_node, source);
                                // Add a qualified "Trait::method" definition so that
                                // callers searching for "Trait::method" can find it.
                                defs.push(DefinitionInfo {
                                    name: format!("{}::{}", trait_name, method_name),
                                    kind: SymbolKind::Function,
                                    file: file_path.to_string(),
                                    line: item.start_position().row,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_rust_definitions(&child, source, file_path, defs);
    }
}

/// Extract the trait name from `impl Trait for Type { ... }`.
///
/// Returns `Some("Trait")` for `impl Trait for Type`, `None` for `impl Type`.
fn rust_impl_trait_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // In tree-sitter-rust, `impl Trait for Type` has a "trait" field.
    if let Some(trait_node) = node.child_by_field_name("trait") {
        return Some(node_text(&trait_node, source));
    }
    // Fallback: look for a `for` keyword, which only appears in trait impls.
    let mut found_for = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "for" || node_text(&child, source) == "for" {
            found_for = true;
        }
        // The first type_identifier before "for" is the trait name.
        if !found_for && child.kind() == "type_identifier" {
            let text = node_text(&child, source);
            // Check that the next sibling is "for"
            if let Some(next) = child.next_sibling() {
                if next.kind() == "for" || node_text(&next, source) == "for" {
                    return Some(text);
                }
            }
        }
    }
    None
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
    match lang {
        Language::Rust => collect_rust_references(node, source, file_path, refs),
        Language::Python => collect_python_references(node, source, file_path, refs),
        Language::TypeScript => collect_typescript_references(node, source, file_path, refs),
        Language::Go => collect_go_references(node, source, file_path, refs),
        _ => {} // Other languages not yet supported for graph extraction
    }
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
        "impl_item" => {
            // For `impl Trait for Type { ... }`, emit an Inherit reference
            // from Type to Trait so the call graph knows about the relationship.
            if let Some(trait_name) = rust_impl_trait_name(node, source) {
                refs.push(ReferenceInfo {
                    target_name: trait_name,
                    kind: ReferenceKind::Inherit,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
            // Recurse into the impl body to find call references.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_rust_references(&child, source, file_path, refs);
            }
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

// ---------------------------------------------------------------------------
// Python definition extraction
// ---------------------------------------------------------------------------

fn collect_python_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    defs: &mut Vec<DefinitionInfo>,
) {
    let kind_str = node.kind();

    match kind_str {
        "function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                defs.push(DefinitionInfo {
                    name: node_text(&name_node, source),
                    kind: SymbolKind::Function,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                defs.push(DefinitionInfo {
                    name: node_text(&name_node, source),
                    kind: SymbolKind::Struct,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        "decorated_definition" => {
            // Unwrap decorated definitions to find the inner function/class
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                match child.kind() {
                    "function_definition" | "class_definition" => {
                        collect_python_definitions(&child, source, file_path, defs);
                        return;
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_python_definitions(&child, source, file_path, defs);
    }
}

// ---------------------------------------------------------------------------
// Python reference extraction
// ---------------------------------------------------------------------------

fn collect_python_references(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    refs: &mut Vec<ReferenceInfo>,
) {
    let kind_str = node.kind();

    match kind_str {
        "call" => {
            if let Some(name) = python_call_target(node, source) {
                refs.push(ReferenceInfo {
                    target_name: name,
                    kind: ReferenceKind::Call,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
            // Recurse into children (calls can be nested)
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_python_references(&child, source, file_path, refs);
            }
        }
        "class_definition" => {
            // Extract Inherit references from `class Dog(Animal):` superclass list.
            if let Some(bases) = node.child_by_field_name("superclasses") {
                let mut cursor = bases.walk();
                for child in bases.children(&mut cursor) {
                    match child.kind() {
                        "identifier" => {
                            let base_name = node_text(&child, source);
                            if !base_name.is_empty() {
                                refs.push(ReferenceInfo {
                                    target_name: base_name,
                                    kind: ReferenceKind::Inherit,
                                    file: file_path.to_string(),
                                    line: node.start_position().row,
                                });
                            }
                        }
                        "attribute" => {
                            // e.g. `module.BaseClass`
                            let base_name = node_text(&child, source);
                            if !base_name.is_empty() {
                                refs.push(ReferenceInfo {
                                    target_name: base_name,
                                    kind: ReferenceKind::Inherit,
                                    file: file_path.to_string(),
                                    line: node.start_position().row,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            // Recurse into the class body for call references.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_python_references(&child, source, file_path, refs);
            }
        }
        "import_statement" | "import_from_statement" => {
            let text = node_text(node, source);
            if !text.is_empty() {
                refs.push(ReferenceInfo {
                    target_name: text.trim().to_string(),
                    kind: ReferenceKind::Import,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        _ => {
            // Recurse into children
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_python_references(&child, source, file_path, refs);
            }
        }
    }
}

/// Extract the target name from a Python call expression.
///
/// Handles:
/// - Simple calls: `foo()` -> "foo"
/// - Attribute calls: `obj.method()` -> "method"
/// - Dotted calls: `os.path.join()` -> "join"
fn python_call_target(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // In Python, the call node's first named child is the function being called.
    // It can be an `identifier`, `attribute`, or a more complex expression.
    let func_node = node.child_by_field_name("function")?;
    match func_node.kind() {
        "identifier" => Some(node_text(&func_node, source)),
        "attribute" => {
            // `obj.method()` -> extract "method" (the `attribute` field)
            if let Some(attr) = func_node.child_by_field_name("attribute") {
                Some(node_text(&attr, source))
            } else {
                Some(node_text(&func_node, source))
            }
        }
        _ => Some(node_text(&func_node, source)),
    }
}

// ---------------------------------------------------------------------------
// TypeScript definition extraction
// ---------------------------------------------------------------------------

fn collect_typescript_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    defs: &mut Vec<DefinitionInfo>,
) {
    let kind_str = node.kind();

    match kind_str {
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                defs.push(DefinitionInfo {
                    name: node_text(&name_node, source),
                    kind: SymbolKind::Function,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        "class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                defs.push(DefinitionInfo {
                    name: node_text(&name_node, source),
                    kind: SymbolKind::Struct,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        "interface_declaration" => {
            // Extract interface as Trait kind (analogous to Rust traits).
            if let Some(name_node) = node.child_by_field_name("name") {
                let iface_name = node_text(&name_node, source);
                defs.push(DefinitionInfo {
                    name: iface_name.clone(),
                    kind: SymbolKind::Trait,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
                // Also extract method signatures from the interface body.
                if let Some(body) = node.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        // Interface method signatures appear as various node types
                        // depending on the tree-sitter-typescript grammar version.
                        // Common types: "method_signature", "property_signature".
                        if child.kind() == "method_signature"
                            || child.kind() == "method_definition"
                        {
                            if let Some(method_name_node) = child.child_by_field_name("name") {
                                let method_name = node_text(&method_name_node, source);
                                defs.push(DefinitionInfo {
                                    name: method_name.clone(),
                                    kind: SymbolKind::Function,
                                    file: file_path.to_string(),
                                    line: child.start_position().row,
                                });
                                // Also add a qualified "Interface::method" definition
                                // for precise trait-dispatch linking.
                                defs.push(DefinitionInfo {
                                    name: format!("{}::{}", iface_name, method_name),
                                    kind: SymbolKind::Function,
                                    file: file_path.to_string(),
                                    line: child.start_position().row,
                                });
                            }
                        }
                    }
                }
            }
            // Don't recurse — we already handled the body above.
            return;
        }
        "method_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                defs.push(DefinitionInfo {
                    name: node_text(&name_node, source),
                    kind: SymbolKind::Function,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        _ => {}
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_typescript_definitions(&child, source, file_path, defs);
    }
}

// ---------------------------------------------------------------------------
// TypeScript reference extraction
// ---------------------------------------------------------------------------

fn collect_typescript_references(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    refs: &mut Vec<ReferenceInfo>,
) {
    let kind_str = node.kind();

    match kind_str {
        "call_expression" => {
            if let Some(name) = typescript_call_target(node, source) {
                refs.push(ReferenceInfo {
                    target_name: name,
                    kind: ReferenceKind::Call,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
            // Recurse into children (calls can be nested)
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_typescript_references(&child, source, file_path, refs);
            }
        }
        "class_declaration" => {
            // Extract Inherit references from `implements` and `extends` clauses.
            // e.g. `class ConsoleLogger implements Logger { ... }`
            // e.g. `class Dog extends Animal { ... }`
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "class_heritage" {
                    let mut inner_cursor = child.walk();
                    for clause in child.children(&mut inner_cursor) {
                        // `implements_clause` or `extends_clause`
                        if clause.kind() == "implements_clause"
                            || clause.kind() == "extends_clause"
                        {
                            let mut clause_cursor = clause.walk();
                            for type_node in clause.children(&mut clause_cursor) {
                                // Type identifiers in the clause
                                if type_node.kind() == "type_identifier"
                                    || type_node.kind() == "identifier"
                                {
                                    let name = node_text(&type_node, source);
                                    if !name.is_empty() {
                                        refs.push(ReferenceInfo {
                                            target_name: name,
                                            kind: ReferenceKind::Inherit,
                                            file: file_path.to_string(),
                                            line: node.start_position().row,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Recurse into the class body for call references.
            let mut cursor2 = node.walk();
            for child in node.children(&mut cursor2) {
                collect_typescript_references(&child, source, file_path, refs);
            }
        }
        "import_statement" => {
            let text = node_text(node, source);
            let trimmed = text.trim().trim_end_matches(';').trim();
            if !trimmed.is_empty() {
                refs.push(ReferenceInfo {
                    target_name: trimmed.to_string(),
                    kind: ReferenceKind::Import,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        _ => {
            // Recurse into children
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_typescript_references(&child, source, file_path, refs);
            }
        }
    }
}

/// Extract the target name from a TypeScript/JavaScript call expression.
///
/// Handles:
/// - Simple calls: `foo()` -> "foo"
/// - Member calls: `obj.method()` -> "method"
/// - Chained calls: `console.log()` -> "log"
fn typescript_call_target(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let func_node = node.child_by_field_name("function")?;
    match func_node.kind() {
        "identifier" => Some(node_text(&func_node, source)),
        "member_expression" => {
            // `obj.method()` -> extract "method" (the `property` field)
            if let Some(prop) = func_node.child_by_field_name("property") {
                Some(node_text(&prop, source))
            } else {
                Some(node_text(&func_node, source))
            }
        }
        _ => Some(node_text(&func_node, source)),
    }
}

// ---------------------------------------------------------------------------
// Go definition extraction
// ---------------------------------------------------------------------------

fn collect_go_definitions(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    defs: &mut Vec<DefinitionInfo>,
) {
    let kind_str = node.kind();

    match kind_str {
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                defs.push(DefinitionInfo {
                    name: node_text(&name_node, source),
                    kind: SymbolKind::Function,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                defs.push(DefinitionInfo {
                    name: node_text(&name_node, source),
                    kind: SymbolKind::Function,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        "type_declaration" => {
            // type_declaration contains type_spec children with actual type names
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let sym_kind = go_classify_type_spec(&child);
                        defs.push(DefinitionInfo {
                            name: node_text(&name_node, source),
                            kind: sym_kind,
                            file: file_path.to_string(),
                            line: node.start_position().row,
                        });
                    }
                }
            }
            // Don't recurse into type_declaration children (already handled)
            return;
        }
        _ => {}
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_go_definitions(&child, source, file_path, defs);
    }
}

/// Classify a Go type_spec into Struct, Trait (interface), or Type.
fn go_classify_type_spec(node: &tree_sitter::Node) -> SymbolKind {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "struct_type" => return SymbolKind::Struct,
            "interface_type" => return SymbolKind::Trait,
            _ => {}
        }
    }
    SymbolKind::Type
}

// ---------------------------------------------------------------------------
// Go reference extraction
// ---------------------------------------------------------------------------

fn collect_go_references(
    node: &tree_sitter::Node,
    source: &[u8],
    file_path: &str,
    refs: &mut Vec<ReferenceInfo>,
) {
    let kind_str = node.kind();

    match kind_str {
        "call_expression" => {
            if let Some(name) = go_call_target(node, source) {
                refs.push(ReferenceInfo {
                    target_name: name,
                    kind: ReferenceKind::Call,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
            // Recurse into children (calls can be nested)
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_go_references(&child, source, file_path, refs);
            }
        }
        "import_declaration" => {
            let text = node_text(node, source);
            if !text.is_empty() {
                refs.push(ReferenceInfo {
                    target_name: text.trim().to_string(),
                    kind: ReferenceKind::Import,
                    file: file_path.to_string(),
                    line: node.start_position().row,
                });
            }
        }
        _ => {
            // Recurse into children
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_go_references(&child, source, file_path, refs);
            }
        }
    }
}

/// Extract the target name from a Go call expression.
///
/// Handles:
/// - Simple calls: `foo()` -> "foo"
/// - Qualified calls: `fmt.Println()` -> "Println"
/// - Selector calls: `obj.Method()` -> "Method"
fn go_call_target(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let func_node = node.child_by_field_name("function")?;
    match func_node.kind() {
        "identifier" => Some(node_text(&func_node, source)),
        "selector_expression" => {
            // `pkg.Func()` -> extract "Func" (the `field` field)
            if let Some(field) = func_node.child_by_field_name("field") {
                Some(node_text(&field, source))
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

    // -----------------------------------------------------------------------
    // Python extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_python_definitions() {
        let src = r#"
def greet(name):
    return f"Hello, {name}!"

class Animal:
    def __init__(self, name):
        self.name = name

    def speak(self):
        pass

def helper():
    pass
"#;
        let defs = extract_definitions(src, "app.py", &Language::Python);
        let fns: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Function)
            .collect();
        // greet, __init__, speak, helper
        assert!(
            fns.len() >= 3,
            "expected at least 3 functions, got {} ({:?})",
            fns.len(),
            fns.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        let fn_names: Vec<&str> = fns.iter().map(|d| d.name.as_str()).collect();
        assert!(
            fn_names.contains(&"greet"),
            "expected 'greet' in {fn_names:?}"
        );
        assert!(
            fn_names.contains(&"helper"),
            "expected 'helper' in {fn_names:?}"
        );

        let structs: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Animal");

        // All defs should have correct file path
        assert!(defs.iter().all(|d| d.file == "app.py"));
    }

    #[test]
    fn extract_python_call_references() {
        let src = r#"
def main():
    greet("world")
    x = helper(42)
    print(x)
    obj.method()
"#;
        let refs = extract_references(src, "app.py", &Language::Python);
        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        let call_names: Vec<&str> = calls.iter().map(|r| r.target_name.as_str()).collect();
        assert!(
            call_names.contains(&"greet"),
            "expected 'greet' in {call_names:?}"
        );
        assert!(
            call_names.contains(&"helper"),
            "expected 'helper' in {call_names:?}"
        );
        assert!(
            call_names.contains(&"print"),
            "expected 'print' in {call_names:?}"
        );
    }

    #[test]
    fn extract_python_import_references() {
        let src = r#"
import os
from pathlib import Path
"#;
        let refs = extract_references(src, "app.py", &Language::Python);
        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert_eq!(imports.len(), 2, "expected 2 imports, got {imports:?}");
    }

    // -----------------------------------------------------------------------
    // TypeScript extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_typescript_definitions() {
        let src = r#"
function greet(name: string): string {
    return `Hello, ${name}!`;
}

class Animal {
    name: string;

    constructor(name: string) {
        this.name = name;
    }

    speak(): string {
        return this.name;
    }
}

function helper(x: number): number {
    return x + 1;
}
"#;
        let defs = extract_definitions(src, "app.ts", &Language::TypeScript);
        let fns: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Function)
            .collect();
        assert!(
            fns.len() >= 2,
            "expected at least 2 functions, got {} ({:?})",
            fns.len(),
            fns.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        let fn_names: Vec<&str> = fns.iter().map(|d| d.name.as_str()).collect();
        assert!(
            fn_names.contains(&"greet"),
            "expected 'greet' in {fn_names:?}"
        );
        assert!(
            fn_names.contains(&"helper"),
            "expected 'helper' in {fn_names:?}"
        );

        let structs: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1, "expected 1 class/struct, got {structs:?}");
        assert_eq!(structs[0].name, "Animal");

        assert!(defs.iter().all(|d| d.file == "app.ts"));
    }

    #[test]
    fn extract_typescript_call_references() {
        let src = r#"
function main() {
    greet("world");
    const x = helper(42);
    console.log(x);
}
"#;
        let refs = extract_references(src, "app.ts", &Language::TypeScript);
        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        let call_names: Vec<&str> = calls.iter().map(|r| r.target_name.as_str()).collect();
        assert!(
            call_names.contains(&"greet"),
            "expected 'greet' in {call_names:?}"
        );
        assert!(
            call_names.contains(&"helper"),
            "expected 'helper' in {call_names:?}"
        );
    }

    #[test]
    fn extract_typescript_method_definitions() {
        let src = r#"
class Greeter {
    greet(name: string): string {
        return `Hello, ${name}!`;
    }

    farewell(): void {
        console.log("Goodbye");
    }
}
"#;
        let defs = extract_definitions(src, "greeter.ts", &Language::TypeScript);
        let fns: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Function)
            .collect();
        let fn_names: Vec<&str> = fns.iter().map(|d| d.name.as_str()).collect();
        assert!(
            fn_names.contains(&"greet"),
            "expected 'greet' method in {fn_names:?}"
        );
        assert!(
            fn_names.contains(&"farewell"),
            "expected 'farewell' method in {fn_names:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Go extraction tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_go_definitions() {
        let src = r#"
package main

func Add(a int, b int) int {
    return a + b
}

type Point struct {
    X float64
    Y float64
}

func (p *Point) Distance() float64 {
    return 0.0
}

func helper() {}
"#;
        let defs = extract_definitions(src, "main.go", &Language::Go);
        let fns: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Function)
            .collect();
        assert!(
            fns.len() >= 2,
            "expected at least 2 functions, got {} ({:?})",
            fns.len(),
            fns.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
        let fn_names: Vec<&str> = fns.iter().map(|d| d.name.as_str()).collect();
        assert!(fn_names.contains(&"Add"), "expected 'Add' in {fn_names:?}");
        assert!(
            fn_names.contains(&"helper"),
            "expected 'helper' in {fn_names:?}"
        );
        // Distance is a method (mapped to Function in our graph)
        assert!(
            fn_names.contains(&"Distance"),
            "expected 'Distance' in {fn_names:?}"
        );

        let structs: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1, "expected 1 struct, got {structs:?}");
        assert_eq!(structs[0].name, "Point");

        assert!(defs.iter().all(|d| d.file == "main.go"));
    }

    #[test]
    fn extract_go_call_references() {
        let src = r#"
package main

import "fmt"

func main() {
    fmt.Println("hello")
    x := Add(1, 2)
    helper()
}
"#;
        let refs = extract_references(src, "main.go", &Language::Go);
        let calls: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .collect();
        let call_names: Vec<&str> = calls.iter().map(|r| r.target_name.as_str()).collect();
        assert!(
            call_names.contains(&"Add"),
            "expected 'Add' in {call_names:?}"
        );
        assert!(
            call_names.contains(&"helper"),
            "expected 'helper' in {call_names:?}"
        );
    }

    #[test]
    fn extract_go_import_references() {
        let src = r#"
package main

import "fmt"
import (
    "os"
    "strings"
)
"#;
        let refs = extract_references(src, "main.go", &Language::Go);
        let imports: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Import)
            .collect();
        assert!(
            imports.len() >= 2,
            "expected at least 2 imports, got {imports:?}"
        );
    }

    #[test]
    fn extract_go_type_definitions() {
        let src = r#"
package main

type Handler func(string) error

type Config struct {
    Verbose bool
}

type Stringer interface {
    String() string
}
"#;
        let defs = extract_definitions(src, "types.go", &Language::Go);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.contains(&"Handler"),
            "expected 'Handler' in {names:?}"
        );
        assert!(names.contains(&"Config"), "expected 'Config' in {names:?}");
        assert!(
            names.contains(&"Stringer"),
            "expected 'Stringer' in {names:?}"
        );

        // Config should be Struct
        let structs: Vec<_> = defs
            .iter()
            .filter(|d| d.kind == SymbolKind::Struct)
            .collect();
        assert!(
            structs.iter().any(|s| s.name == "Config"),
            "expected Config as Struct"
        );
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
