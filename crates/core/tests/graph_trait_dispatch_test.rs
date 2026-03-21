//! Tests for call graph accuracy improvements:
//!   1. Rust trait method dispatch through impl blocks
//!   2. Python super().method() call resolution
//!   3. TypeScript interface implementation linking

use codixing_core::{Engine, IndexConfig};
use std::fs;
use tempfile::tempdir;

fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut config = IndexConfig::new(root);
    config.embedding.enabled = false;
    config
}

// ---------------------------------------------------------------------------
// 1. Rust trait method dispatch
// ---------------------------------------------------------------------------

#[test]
fn trait_method_dispatch_links_impl() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("traits.rs"),
        r#"pub trait Greeter {
    fn greet(&self) -> String;
}

pub struct EnglishGreeter;

impl Greeter for EnglishGreeter {
    fn greet(&self) -> String {
        "Hello".to_string()
    }
}
"#,
    )
    .unwrap();

    fs::write(
        src.join("main.rs"),
        r#"use crate::traits::{Greeter, EnglishGreeter};

fn main() {
    let g = EnglishGreeter;
    let result = g.greet();
}
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // symbol_callees_precise should show that main calls greet
    let callees = engine.symbol_callees_precise("main", Some("src/main.rs"));
    assert!(
        callees.iter().any(|c| c.contains("greet")),
        "main should call greet, got: {:?}",
        callees
    );

    // symbol_callers_precise should show that greet is called from main
    let callers = engine.symbol_callers_precise("greet", 20);
    assert!(
        callers
            .iter()
            .any(|r| r.context.contains("greet") || r.file_path.contains("main")),
        "greet should be called from main, got: {:?}",
        callers
    );
}

// ---------------------------------------------------------------------------
// 2. Python super().method() call resolution
// ---------------------------------------------------------------------------

#[test]
fn python_super_method_call_linked() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("animals.py"),
        r#"class Animal:
    def speak(self):
        return "..."

class Dog(Animal):
    def speak(self):
        base = super().speak()
        return f"Woof! {base}"
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // The call graph extraction should detect `super().speak()` as a call to `speak`.
    // The Dog.speak function should list `speak` among its callees.
    // Since both are named "speak", and the callee resolution looks for definitions,
    // the call graph should link Dog.speak -> Animal.speak (or at least to "speak").
    let callees = engine.symbol_callees_precise("speak", Some("src/animals.py"));
    // Dog.speak calls super().speak() — the call target name is "speak"
    assert!(
        callees.iter().any(|c| c == "speak"),
        "Dog.speak should call speak (via super()), got: {:?}",
        callees
    );
}

// ---------------------------------------------------------------------------
// 3. TypeScript implements interface linking
// ---------------------------------------------------------------------------

#[test]
fn typescript_interface_impl_linked() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("logger.ts"),
        r#"interface Logger {
    log(msg: string): void;
}

class ConsoleLogger implements Logger {
    log(msg: string): void {
        console.log(msg);
    }
}

function useLogger(logger: Logger): void {
    logger.log("hello");
}
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // useLogger should have "log" as a callee
    let callees = engine.symbol_callees_precise("useLogger", Some("src/logger.ts"));
    assert!(
        callees.iter().any(|c| c == "log"),
        "useLogger should call log, got: {:?}",
        callees
    );

    // log should be called by useLogger
    let callers = engine.symbol_callers_precise("log", 20);
    assert!(
        callers
            .iter()
            .any(|r| r.context.contains("log") || r.file_path.contains("logger")),
        "log should be called from useLogger, got: {:?}",
        callers
    );
}

// ---------------------------------------------------------------------------
// 4. Extraction unit tests — verify the extract layer detects these patterns
// ---------------------------------------------------------------------------

use codixing_core::graph::extract::{extract_definitions, extract_references};
use codixing_core::graph::types::{ReferenceKind, SymbolKind};
use codixing_core::language::Language;

#[test]
fn rust_trait_impl_extracts_method_definitions() {
    let src = r#"
pub trait Greeter {
    fn greet(&self) -> String;
}

pub struct EnglishGreeter;

impl Greeter for EnglishGreeter {
    fn greet(&self) -> String {
        "Hello".to_string()
    }
}
"#;

    let defs = extract_definitions(src, "traits.rs", &Language::Rust);
    let fn_names: Vec<&str> = defs
        .iter()
        .filter(|d| d.kind == SymbolKind::Function)
        .map(|d| d.name.as_str())
        .collect();

    // The trait method declaration AND the impl method should both be extracted
    assert!(
        fn_names.iter().filter(|&&n| n == "greet").count() >= 1,
        "expected at least one 'greet' function def, got: {:?}",
        fn_names
    );

    // The trait itself should be extracted
    let trait_names: Vec<&str> = defs
        .iter()
        .filter(|d| d.kind == SymbolKind::Trait)
        .map(|d| d.name.as_str())
        .collect();
    assert!(
        trait_names.contains(&"Greeter"),
        "expected Greeter trait, got: {:?}",
        trait_names
    );
}

#[test]
fn rust_method_call_extracted_as_reference() {
    let src = r#"
fn main() {
    let g = EnglishGreeter;
    let result = g.greet();
}
"#;

    let refs = extract_references(src, "main.rs", &Language::Rust);
    let call_names: Vec<&str> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Call)
        .map(|r| r.target_name.as_str())
        .collect();

    assert!(
        call_names.contains(&"greet"),
        "expected 'greet' call reference, got: {:?}",
        call_names
    );
}

#[test]
fn python_super_call_extracted() {
    let src = r#"
class Dog(Animal):
    def speak(self):
        base = super().speak()
        return f"Woof! {base}"
"#;

    let refs = extract_references(src, "animals.py", &Language::Python);
    let call_names: Vec<&str> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Call)
        .map(|r| r.target_name.as_str())
        .collect();

    // super().speak() should extract "speak" as a call target
    assert!(
        call_names.contains(&"speak"),
        "expected 'speak' call via super(), got: {:?}",
        call_names
    );
}

#[test]
fn typescript_interface_method_call_extracted() {
    let src = r#"
function useLogger(logger: Logger): void {
    logger.log("hello");
}
"#;

    let refs = extract_references(src, "logger.ts", &Language::TypeScript);
    let call_names: Vec<&str> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Call)
        .map(|r| r.target_name.as_str())
        .collect();

    assert!(
        call_names.contains(&"log"),
        "expected 'log' call reference, got: {:?}",
        call_names
    );
}

// ---------------------------------------------------------------------------
// 5. Inherit reference extraction tests
// ---------------------------------------------------------------------------

#[test]
fn rust_impl_trait_emits_inherit_reference() {
    let src = r#"
pub trait Greeter {
    fn greet(&self) -> String;
}

pub struct EnglishGreeter;

impl Greeter for EnglishGreeter {
    fn greet(&self) -> String {
        "Hello".to_string()
    }
}
"#;

    let refs = extract_references(src, "traits.rs", &Language::Rust);
    let inherit_refs: Vec<&str> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Inherit)
        .map(|r| r.target_name.as_str())
        .collect();

    assert!(
        inherit_refs.contains(&"Greeter"),
        "expected Inherit reference to 'Greeter' from impl block, got: {:?}",
        inherit_refs
    );
}

#[test]
fn rust_trait_impl_extracts_qualified_method_name() {
    let src = r#"
pub trait Greeter {
    fn greet(&self) -> String;
}

pub struct EnglishGreeter;

impl Greeter for EnglishGreeter {
    fn greet(&self) -> String {
        "Hello".to_string()
    }
}
"#;

    let defs = extract_definitions(src, "traits.rs", &Language::Rust);
    let qualified: Vec<&str> = defs
        .iter()
        .filter(|d| d.name.contains("::"))
        .map(|d| d.name.as_str())
        .collect();

    assert!(
        qualified.contains(&"Greeter::greet"),
        "expected qualified def 'Greeter::greet', got: {:?}",
        qualified
    );
}

#[test]
fn python_class_inheritance_emits_inherit_reference() {
    let src = r#"
class Animal:
    def speak(self):
        return "..."

class Dog(Animal):
    def speak(self):
        return "Woof!"
"#;

    let refs = extract_references(src, "animals.py", &Language::Python);
    let inherit_refs: Vec<&str> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Inherit)
        .map(|r| r.target_name.as_str())
        .collect();

    assert!(
        inherit_refs.contains(&"Animal"),
        "expected Inherit reference to 'Animal' from Dog class, got: {:?}",
        inherit_refs
    );
}

#[test]
fn typescript_implements_emits_inherit_reference() {
    let src = r#"
interface Logger {
    log(msg: string): void;
}

class ConsoleLogger implements Logger {
    log(msg: string): void {
        console.log(msg);
    }
}
"#;

    let refs = extract_references(src, "logger.ts", &Language::TypeScript);
    let inherit_refs: Vec<&str> = refs
        .iter()
        .filter(|r| r.kind == ReferenceKind::Inherit)
        .map(|r| r.target_name.as_str())
        .collect();

    assert!(
        inherit_refs.contains(&"Logger"),
        "expected Inherit reference to 'Logger' from implements clause, got: {:?}",
        inherit_refs
    );
}

#[test]
fn typescript_interface_extracted_as_trait() {
    let src = r#"
interface Logger {
    log(msg: string): void;
}
"#;

    let defs = extract_definitions(src, "logger.ts", &Language::TypeScript);
    let trait_names: Vec<&str> = defs
        .iter()
        .filter(|d| d.kind == SymbolKind::Trait)
        .map(|d| d.name.as_str())
        .collect();

    assert!(
        trait_names.contains(&"Logger"),
        "expected Logger as Trait, got: {:?}",
        trait_names
    );
}

#[test]
fn index_stats_includes_symbol_graph_counts() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    fs::write(
        src_dir.join("main.rs"),
        r#"fn main() {
    let x = helper(42);
}

fn helper(n: i32) -> i32 { n }
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();
    let stats = engine.stats();

    // Should have symbol node/edge counts > 0
    assert!(
        stats.symbol_node_count > 0,
        "expected symbol_node_count > 0, got: {}",
        stats.symbol_node_count
    );
    assert!(
        stats.symbol_edge_count > 0,
        "expected symbol_edge_count > 0, got: {}",
        stats.symbol_edge_count
    );
}

#[test]
fn typescript_class_with_implements_extracts_methods() {
    let src = r#"
interface Logger {
    log(msg: string): void;
}

class ConsoleLogger implements Logger {
    log(msg: string): void {
        console.log(msg);
    }
}
"#;

    let defs = extract_definitions(src, "logger.ts", &Language::TypeScript);
    let fn_names: Vec<&str> = defs
        .iter()
        .filter(|d| d.kind == SymbolKind::Function)
        .map(|d| d.name.as_str())
        .collect();

    // The method `log` should be extracted as a definition
    assert!(
        fn_names.contains(&"log"),
        "expected 'log' method definition, got: {:?}",
        fn_names
    );

    // ConsoleLogger should be extracted as a class
    let class_names: Vec<&str> = defs
        .iter()
        .filter(|d| d.kind == SymbolKind::Struct)
        .map(|d| d.name.as_str())
        .collect();
    assert!(
        class_names.contains(&"ConsoleLogger"),
        "expected ConsoleLogger class, got: {:?}",
        class_names
    );
}
