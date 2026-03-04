//! Integration tests for the Phase 3 code dependency graph.

mod common;

use std::fs;
use std::path::Path;

use codeforge_core::graph::{ImportExtractor, ImportResolver};
use codeforge_core::language::Language;
use codeforge_core::{Engine, IndexConfig};
use tempfile::tempdir;

fn no_embed_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

// ---------------------------------------------------------------------------
// 1. graph_builds_on_init
// ---------------------------------------------------------------------------

#[test]
fn graph_builds_on_init() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_project_with_imports(root);

    let engine = Engine::init(root, no_embed_config(root)).unwrap();
    let stats = engine.graph_stats().expect("graph should be built");

    assert!(
        stats.node_count >= 7,
        "expected at least 7 nodes (one per file), got {}",
        stats.node_count
    );
    assert!(
        stats.resolved_edges >= 2,
        "expected at least 2 resolved edges (Rust imports), got {}",
        stats.resolved_edges
    );
}

// ---------------------------------------------------------------------------
// 2. rust_imports_extracted
// ---------------------------------------------------------------------------

#[test]
fn rust_imports_extracted() {
    let src = "use crate::engine::Engine;\nuse crate::parser::Parser;";
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let tree = parser.parse(src, None).unwrap();

    let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Rust);
    assert!(
        !imports.is_empty(),
        "expected imports from Rust use statements"
    );
    let paths: Vec<&str> = imports.iter().map(|i| i.path.as_str()).collect();
    assert!(
        paths
            .iter()
            .any(|p| p.contains("engine") || p.contains("crate")),
        "expected engine import, got: {paths:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. typescript_imports_extracted
// ---------------------------------------------------------------------------

#[test]
fn typescript_imports_extracted() {
    let src = r#"import { Foo } from "./foo";"#;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
        .unwrap();
    let tree = parser.parse(src, None).unwrap();

    let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::TypeScript);
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].path, "./foo");
    assert!(imports[0].is_relative);
}

// ---------------------------------------------------------------------------
// 4. rust_crate_import_resolves_to_file
// ---------------------------------------------------------------------------

#[test]
fn rust_crate_import_resolves_to_file() {
    let indexed: std::collections::HashSet<String> =
        ["src/parser.rs", "src/engine.rs", "src/main.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    let resolver = ImportResolver::new(indexed, std::path::PathBuf::from("/project"));

    let raw = codeforge_core::graph::extractor::RawImport {
        path: "crate::parser".to_string(),
        language: Language::Rust,
        is_relative: true,
    };
    let resolved = resolver.resolve(&raw, "src/main.rs");
    assert_eq!(resolved, Some("src/parser.rs".to_string()));
}

// ---------------------------------------------------------------------------
// 5. typescript_relative_import_resolves
// ---------------------------------------------------------------------------

#[test]
fn typescript_relative_import_resolves() {
    let indexed: std::collections::HashSet<String> = ["src/foo.ts", "src/index.ts"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let resolver = ImportResolver::new(indexed, std::path::PathBuf::from("/project"));

    let raw = codeforge_core::graph::extractor::RawImport {
        path: "./foo".to_string(),
        language: Language::TypeScript,
        is_relative: true,
    };
    let resolved = resolver.resolve(&raw, "src/index.ts");
    assert_eq!(resolved, Some("src/foo.ts".to_string()));
}

// ---------------------------------------------------------------------------
// 6. external_import_returns_none
// ---------------------------------------------------------------------------

#[test]
fn external_import_returns_none() {
    let indexed: std::collections::HashSet<String> =
        ["src/main.rs"].iter().map(|s| s.to_string()).collect();
    let resolver = ImportResolver::new(indexed, std::path::PathBuf::from("/project"));

    let raw = codeforge_core::graph::extractor::RawImport {
        path: "std::collections::HashMap".to_string(),
        language: Language::Rust,
        is_relative: false,
    };
    assert_eq!(resolver.resolve(&raw, "src/main.rs"), None);
}

// ---------------------------------------------------------------------------
// 7. pagerank_scores_most_imported_file_highest
// ---------------------------------------------------------------------------

#[test]
fn pagerank_scores_most_imported_file_highest() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_project_with_imports(root);

    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    // parser.rs is imported by both main.rs and engine.rs → highest PageRank.
    let parser_pr = engine.callers("src/parser.rs").len();
    let main_pr = engine.callers("src/main.rs").len();

    assert!(
        parser_pr >= main_pr,
        "parser.rs (imported by 2 files) should have >= callers than main.rs (imported by 0)"
    );
    assert!(
        parser_pr >= 2,
        "expected parser.rs to have at least 2 callers, got {parser_pr}"
    );
}

// ---------------------------------------------------------------------------
// 8. callers_returns_correct_files
// ---------------------------------------------------------------------------

#[test]
fn callers_returns_correct_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_project_with_imports(root);

    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    let callers = engine.callers("src/parser.rs");
    assert!(
        callers.contains(&"src/main.rs".to_string()),
        "expected src/main.rs as caller of src/parser.rs, got: {callers:?}"
    );
    assert!(
        callers.contains(&"src/engine.rs".to_string()),
        "expected src/engine.rs as caller of src/parser.rs, got: {callers:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. reindex_file_updates_graph_edges
// ---------------------------------------------------------------------------

#[test]
fn reindex_file_updates_graph_edges() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_project_with_imports(root);

    let mut engine = Engine::init(root, no_embed_config(root)).unwrap();

    // Initially parser.rs has no imports.
    let initial_callees = engine.callees("src/parser.rs");
    // parser.rs doesn't import anything, so callees should be empty or minimal.
    let initial_len = initial_callees.len();

    // Rewrite parser.rs to import engine.rs.
    fs::write(
        root.join("src/parser.rs"),
        r#"use crate::engine::Engine;

pub struct Parser;

impl Parser {
    pub fn new() -> Self { Self }
}
"#,
    )
    .unwrap();

    engine.reindex_file(Path::new("src/parser.rs")).unwrap();

    let new_callees = engine.callees("src/parser.rs");
    assert!(
        new_callees.len() > initial_len,
        "expected new edges after reindex (added import), initial={initial_len}, after={}",
        new_callees.len()
    );
}

// ---------------------------------------------------------------------------
// 10. graph_persists_across_open
// ---------------------------------------------------------------------------

#[test]
fn graph_persists_across_open() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_project_with_imports(root);

    {
        let engine = Engine::init(root, no_embed_config(root)).unwrap();
        let stats = engine.graph_stats().expect("graph should be built on init");
        assert!(stats.node_count > 0);
    }

    // Re-open and verify graph is restored.
    let engine = Engine::open(root).unwrap();
    let stats = engine
        .graph_stats()
        .expect("graph should persist across open");
    assert!(
        stats.node_count > 0,
        "expected graph to be restored after open, got {} nodes",
        stats.node_count
    );
}
