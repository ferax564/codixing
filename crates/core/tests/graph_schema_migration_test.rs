//! Graph schema-version migration.
//!
//! Resolver/extractor fixes change how edges are computed, but existing
//! indexes keep edges resolved by the old code until the graph is rebuilt.
//! The index stamps a graph schema version on disk; when the binary's version
//! is newer, the next sync auto-rebuilds the graph so fixes reach upgraded
//! installs without a manual `sync --rebuild-graph`.

use std::fs;
use std::path::{Path, PathBuf};

use codixing_core::graph::GRAPH_SCHEMA_VERSION;
use codixing_core::{Engine, IndexConfig};
use tempfile::tempdir;

fn no_embed_config(root: &Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

fn setup_mod_project(root: &Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod foo;\n").unwrap();
    fs::write(root.join("src/foo.rs"), "pub fn f() -> u32 { 1 }\n").unwrap();
}

fn schema_version_path(root: &Path) -> PathBuf {
    root.join(".codixing").join("graph").join("schema.version")
}

#[test]
fn init_stamps_current_graph_schema_version() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_mod_project(root);

    let _engine = Engine::init(root, no_embed_config(root)).unwrap();

    let stamped = fs::read_to_string(schema_version_path(root))
        .expect("init must write graph/schema.version");
    assert_eq!(stamped.trim(), GRAPH_SCHEMA_VERSION.to_string());
}

#[test]
fn sync_rebuilds_graph_when_schema_version_outdated() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_mod_project(root);

    {
        let _engine = Engine::init(root, no_embed_config(root)).unwrap();
    }

    // Simulate an index whose graph was built by an older binary.
    fs::write(schema_version_path(root), "1").unwrap();

    let mut engine = Engine::open(root).unwrap();
    engine.sync().unwrap();

    let stamped = fs::read_to_string(schema_version_path(root)).unwrap();
    assert_eq!(
        stamped.trim(),
        GRAPH_SCHEMA_VERSION.to_string(),
        "sync must bump an outdated graph schema version"
    );

    // The rebuilt graph must be functional: `mod foo;` in lib.rs edges to foo.rs.
    let callers = engine.callers("src/foo.rs");
    assert!(
        callers.iter().any(|c| c.ends_with("lib.rs")),
        "rebuilt graph should contain the mod-declaration edge, got {callers:?}"
    );
}

#[test]
fn sync_keeps_current_schema_version() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_mod_project(root);

    {
        let _engine = Engine::init(root, no_embed_config(root)).unwrap();
    }

    let mut engine = Engine::open(root).unwrap();
    engine.sync().unwrap();

    let stamped = fs::read_to_string(schema_version_path(root)).unwrap();
    assert_eq!(stamped.trim(), GRAPH_SCHEMA_VERSION.to_string());
}
