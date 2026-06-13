//! Integration tests for external-context import: index GitHub/ADR exports,
//! verify they become searchable, link to code, filter by source, are
//! idempotent on re-import, and survive a `sync`.

use std::fs;

use tempfile::TempDir;

use codixing_core::{Engine, IndexConfig, SearchQuery, SourceFilter, Strategy, parse_source};

/// BM25-only config (no model downloads in CI).
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

/// Build a small repo with a symbol the imported docs will reference.
fn init_repo() -> (TempDir, Engine) {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/engine.rs"),
        "/// Initialize the engine.\npub fn add_chunk() { todo!() }\n",
    )
    .unwrap();
    let engine = Engine::init(root, bm25_config(root)).unwrap();
    (dir, engine)
}

#[test]
fn imports_github_issues_and_search_finds_them() {
    let (_dir, mut engine) = init_repo();

    let json = r#"[
      {
        "number": 99,
        "title": "Rate limiting is broken",
        "body": "The throttler drops requests under load. `add_chunk` is implicated.",
        "state": "OPEN",
        "author": {"login": "alice"},
        "labels": [{"name": "bug"}],
        "url": "https://github.com/o/r/issues/99"
      }
    ]"#;
    let docs = codixing_core::external::github::parse_bytes(json.as_bytes()).unwrap();
    let stats = engine.import_external(docs).unwrap();
    assert_eq!(stats.documents, 1);
    assert!(stats.chunks >= 1);

    // The imported issue is searchable by its content.
    let results = engine
        .search(
            SearchQuery::new("rate limiting throttler")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let hit = results
        .iter()
        .find(|r| r.is_external())
        .expect("expected an external-context result");
    assert_eq!(hit.external_source(), Some("github"));
    assert_eq!(hit.file_path, "_external/github/issue-99.md");
    assert!(hit.content.contains("throttler"));
}

#[test]
fn import_links_to_code_via_doc_edges() {
    let (_dir, mut engine) = init_repo();

    // Body references `add_chunk`, which is defined in src/engine.rs.
    let json = r#"[
      {"number": 1, "title": "Investigate add_chunk", "body": "See `add_chunk` in the engine.", "state": "open"}
    ]"#;
    let docs = codixing_core::external::github::parse_bytes(json.as_bytes()).unwrap();
    let stats = engine.import_external(docs).unwrap();
    assert_eq!(
        stats.doc_edges, 1,
        "expected one doc->code edge to add_chunk"
    );

    // The graph now lists the issue as a caller/importer of the code file.
    let callers = engine.callers("src/engine.rs");
    assert!(
        callers
            .iter()
            .any(|c| c.contains("_external/github/issue-1")),
        "expected the imported issue to link to src/engine.rs, got: {callers:?}"
    );
}

#[test]
fn source_filter_scopes_results() {
    let (_dir, mut engine) = init_repo();

    let gh =
        r#"[{"number": 5, "title": "Widget crash", "body": "widget explodes", "state": "open"}]"#;
    engine
        .import_external(codixing_core::external::github::parse_bytes(gh.as_bytes()).unwrap())
        .unwrap();

    // An ADR mentioning the same word.
    let adr = codixing_core::ExternalDocument::new(
        "adr",
        "0001",
        "Widget architecture",
        "We will build the widget as a module.",
    );
    engine.import_external(vec![adr]).unwrap();

    // --source github returns only the github doc.
    let gh_only = engine
        .search(
            SearchQuery::new("widget")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_source_filter(SourceFilter::Named("github".to_string())),
        )
        .unwrap();
    assert!(!gh_only.is_empty());
    assert!(
        gh_only
            .iter()
            .all(|r| r.external_source() == Some("github"))
    );

    // --source external returns both imports but no code.
    let any_external = engine
        .search(
            SearchQuery::new("widget")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_source_filter(SourceFilter::ExternalOnly),
        )
        .unwrap();
    assert!(any_external.iter().all(|r| r.is_external()));
    let sources: std::collections::HashSet<_> = any_external
        .iter()
        .filter_map(|r| r.external_source())
        .collect();
    assert!(sources.contains("github") && sources.contains("adr"));
}

#[test]
fn reimport_replaces_prior_documents_for_the_source() {
    let (_dir, mut engine) = init_repo();

    let v1 = r#"[
      {"number": 1, "title": "One", "body": "first", "state": "open"},
      {"number": 2, "title": "Two", "body": "second", "state": "open"}
    ]"#;
    let s1 = engine
        .import_external(codixing_core::external::github::parse_bytes(v1.as_bytes()).unwrap())
        .unwrap();
    assert_eq!(s1.documents, 2);
    assert_eq!(s1.replaced, 0);

    // Re-import a smaller set: the old github docs are cleared first.
    let v2 = r#"[{"number": 1, "title": "One revised", "body": "rewritten", "state": "closed"}]"#;
    let s2 = engine
        .import_external(codixing_core::external::github::parse_bytes(v2.as_bytes()).unwrap())
        .unwrap();
    assert_eq!(s2.documents, 1);
    assert_eq!(s2.replaced, 2, "both prior github docs should be replaced");

    // issue-2 should no longer be in the index; issue-1 reflects the new body.
    let all = engine
        .search(
            SearchQuery::new("second")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_source_filter(SourceFilter::ExternalOnly),
        )
        .unwrap();
    assert!(
        all.iter()
            .all(|r| r.file_path != "_external/github/issue-2.md"),
        "issue-2 should have been removed on re-import"
    );
}

#[test]
fn imported_docs_survive_sync_and_persist_across_reopen() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().to_path_buf();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();

    // Write an ADR file outside the repo tree so the file walk never sees it.
    let adr_dir = TempDir::new().unwrap();
    fs::write(
        adr_dir.path().join("0001-use-rust.md"),
        "# 1. Use Rust\n\n## Status\n\nAccepted\n\nWe pick Rust for the core.\n",
    )
    .unwrap();

    {
        let mut engine = Engine::init(&root, bm25_config(&root)).unwrap();
        let adr = parse_source("adr", adr_dir.path()).unwrap();
        engine.import_external(adr).unwrap();

        // A normal sync (no file changes) must not purge the virtual import.
        engine.sync().unwrap();
    }

    // Re-open from disk: the imported ADR is still searchable.
    let engine = Engine::open(&root).unwrap();
    let results = engine
        .search(
            SearchQuery::new("use rust for the core")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_source_filter(SourceFilter::Named("adr".to_string())),
        )
        .unwrap();
    assert!(
        results.iter().any(|r| r.file_path == "_external/adr/1.md"),
        "imported ADR should persist across sync + reopen, got: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
    // Content rehydrates from Tantivy stored fields (no source file on disk).
    let hit = results
        .iter()
        .find(|r| r.file_path == "_external/adr/1.md")
        .unwrap();
    assert!(
        engine
            .resolve_chunk_content(hit.chunk_id.parse().unwrap())
            .unwrap()
            .contains("Rust")
    );
}

#[test]
fn imports_jira_and_linear_and_scopes_by_source() {
    let (_dir, mut engine) = init_repo();

    // Jira REST JSON with an ADF description referencing code.
    let jira = r#"{"issues":[{"key":"OPS-3","fields":{
        "summary":"Throttle add_chunk","description":{"type":"doc","content":[
        {"type":"paragraph","content":[{"type":"text","text":"`add_chunk` is too eager."}]}]},
        "status":{"name":"Open"},"labels":["perf"]}}]}"#;
    let jdocs = codixing_core::external::jira::parse_str(jira).unwrap();
    let js = engine.import_external(jdocs).unwrap();
    assert_eq!(js.documents, 1);
    assert_eq!(js.doc_edges, 1, "jira issue should link to add_chunk");

    // Linear CSV.
    let linear = "ID,Title,Description,Status,Labels\nENG-4,Cache add_chunk results,\"memoize `add_chunk`\",Todo,perf\n";
    let ldocs = codixing_core::external::linear::parse_str(linear).unwrap();
    let ls = engine.import_external(ldocs).unwrap();
    assert_eq!(ls.documents, 1);

    // Each source is independently searchable and scoped.
    // Virtual-path ids are slugified to lowercase.
    for (src, expect_path) in [
        ("jira", "_external/jira/ops-3.md"),
        ("linear", "_external/linear/eng-4.md"),
    ] {
        let res = engine
            .search(
                SearchQuery::new("add_chunk")
                    .with_limit(10)
                    .with_strategy(Strategy::Instant)
                    .with_source_filter(SourceFilter::Named(src.to_string())),
            )
            .unwrap();
        assert!(
            res.iter().any(|r| r.file_path == expect_path),
            "expected {expect_path} for --source {src}, got: {:?}",
            res.iter().map(|r| &r.file_path).collect::<Vec<_>>()
        );
        assert!(res.iter().all(|r| r.external_source() == Some(src)));
    }
}
