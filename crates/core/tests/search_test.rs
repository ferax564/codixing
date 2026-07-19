//! Integration tests for BM25 search.
//!
//! These tests focus on BM25 correctness and use `Strategy::Instant` to
//! ensure they do not depend on the embedding model being downloaded.

mod common;

use std::{fs, sync::Mutex};

use codixing_core::persistence::IndexStore;
use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};
use tempfile::tempdir;

/// Create an `IndexConfig` with embeddings disabled (BM25-only mode).
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

#[test]
fn search_finds_rust_function() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("add")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(!results.is_empty(), "expected search results for 'add'");
    assert!(
        results.iter().any(|r| r.file_path.contains("main.rs")),
        "expected at least one result from main.rs, got files: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn search_finds_python_class() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("Validator")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected search results for 'Validator'"
    );
    assert!(
        results.iter().any(|r| r.file_path.contains("utils.py")),
        "expected at least one result from utils.py, got files: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn search_finds_typescript_class() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("App")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(!results.is_empty(), "expected search results for 'App'");
    assert!(
        results.iter().any(|r| r.file_path.contains("index.ts")),
        "expected at least one result from index.ts, got files: {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
}

#[test]
fn search_with_file_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Search with a file filter that restricts to Python files.
    let results = engine
        .search(
            SearchQuery::new("Validator")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_file_filter(".py"),
        )
        .unwrap();

    for result in &results {
        assert!(
            result.file_path.ends_with(".py"),
            "expected only .py results when file_filter is '.py', got: {}",
            result.file_path
        );
    }

    // Search with a file filter for Rust files should not return Python results.
    let results = engine
        .search(
            SearchQuery::new("Validator")
                .with_limit(10)
                .with_strategy(Strategy::Instant)
                .with_file_filter(".rs"),
        )
        .unwrap();

    assert!(
        results.is_empty(),
        "expected no results when searching for 'Validator' in .rs files"
    );
}

#[test]
fn search_no_results() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(
            SearchQuery::new("xyzzy_nonexistent_gibberish_42")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();

    assert!(
        results.is_empty(),
        "expected no results for nonsense query, got {} results",
        results.len()
    );
}

#[test]
fn exact_search_hydrates_content_after_reopen() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    let marker = "reopened_exact_marker_74219";
    fs::write(
        src.join("lib.rs"),
        format!("pub fn fixture() {{ let marker = \"{marker}\"; }}\n"),
    )
    .unwrap();

    drop(Engine::init(root, bm25_config(root)).unwrap());
    let store = IndexStore::open(root).unwrap();
    assert!(!store.chunk_trigram_path().exists());
    fs::write(store.chunk_trigram_path(), b"ignored legacy corruption").unwrap();
    drop(store);
    let engine = Engine::open(root).unwrap();
    let results = engine
        .search(
            SearchQuery::new(marker)
                .with_limit(10)
                .with_strategy(Strategy::Exact),
        )
        .unwrap();

    assert!(
        results.iter().any(|result| result.content.contains(marker)),
        "exact search should hydrate compact metadata content from Tantivy after reopen"
    );
}

#[test]
fn exact_search_streams_across_file_batches_and_preserves_global_ranking() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    for file_index in 0..70 {
        let repeats = if file_index == 69 { 5 } else { 1 };
        let body = "abcd ".repeat(repeats);
        fs::write(
            src.join(format!("file_{file_index:02}.rs")),
            format!("// {body}\npub fn fixture_{file_index}() {{}}\n"),
        )
        .unwrap();
    }
    fs::write(
        src.join("false_positive.rs"),
        "// abc is separated, while bcd only shares the required trigrams\npub fn decoy() {}\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();
    let query = SearchQuery::new("abcd")
        .with_limit(10)
        .with_strategy(Strategy::Exact);
    let first = engine.search(query.clone()).unwrap();
    let second = engine.search(query).unwrap();

    assert_eq!(first[0].file_path, "src/file_69.rs");
    assert_eq!(first[0].score, 5.0);
    assert!(
        first
            .iter()
            .all(|result| result.file_path != "src/false_positive.rs"),
        "full substring verification must reject trigram false positives"
    );
    let first_order: Vec<_> = first.iter().map(|result| &result.chunk_id).collect();
    let second_order: Vec<_> = second.iter().map(|result| &result.chunk_id).collect();
    assert_eq!(first_order, second_order);

    let filtered = engine
        .search(
            SearchQuery::new("abcd")
                .with_limit(10)
                .with_file_filter("file_68")
                .with_strategy(Strategy::Exact),
        )
        .unwrap();
    assert!(!filtered.is_empty());
    assert!(
        filtered
            .iter()
            .all(|result| result.file_path.contains("file_68"))
    );
}

#[test]
fn exact_search_short_query_keeps_bm25_fallback() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("lib.rs"), "pub fn xy() -> usize { 2 }\n").unwrap();
    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(SearchQuery::new("xy").with_strategy(Strategy::Exact))
        .unwrap();

    assert!(
        results
            .iter()
            .any(|result| result.content.contains("fn xy"))
    );
}

#[test]
fn exact_search_bm25_fallback_treats_colons_as_literal_text() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.rs"),
        "pub const MARKER: &str = \"scheme:literalcolonmarker\";\n",
    )
    .unwrap();
    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(SearchQuery::new("scheme:literalcolonmarker").with_strategy(Strategy::Exact))
        .unwrap();
    assert!(
        results
            .iter()
            .any(|result| result.content.contains("scheme:literalcolonmarker"))
    );

    let missing = engine
        .search(SearchQuery::new("unknownfield:missingvalue").with_strategy(Strategy::Exact))
        .expect("literal colons must not be parsed as Tantivy field syntax");
    assert!(missing.is_empty());
}

#[test]
fn exact_search_preserves_lossy_utf8_replacement_matches() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("invalid.rs"),
        b"pub fn invalid_utf8() { let _ = \"\xff\"; }\n",
    )
    .unwrap();
    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .search(SearchQuery::new("\u{FFFD}").with_strategy(Strategy::Exact))
        .unwrap();

    assert!(
        results
            .iter()
            .any(|result| result.content.contains('\u{FFFD}'))
    );
}

#[test]
fn search_with_progress_reports_bm25_phase() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let phases = Mutex::new(Vec::new());
    let query = SearchQuery::new("add")
        .with_limit(10)
        .with_strategy(Strategy::Instant);

    let results = engine.search_with_progress(query, |phase, partial| {
        phases
            .lock()
            .unwrap()
            .push((phase.to_string(), partial.len()));
    });

    assert!(results.is_ok());
    let results = results.unwrap();
    assert!(!results.is_empty(), "expected results for 'add'");

    let phases = phases.into_inner().unwrap();
    assert!(!phases.is_empty(), "should have at least one phase");
    assert_eq!(phases[0].0, "bm25", "first phase should be BM25");
    assert!(phases[0].1 > 0, "BM25 phase should have results");

    // Instant strategy should only produce the bm25 phase.
    assert_eq!(phases.len(), 1, "Instant should have exactly one phase");
}

#[test]
fn search_with_progress_returns_actual_exact_results() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();
    let query = SearchQuery::new("add")
        .with_limit(10)
        .with_strategy(Strategy::Exact);
    let expected = engine.search(query.clone()).unwrap();

    let phases = Mutex::new(Vec::new());
    let results = engine
        .search_with_progress(query, |phase, partial| {
            phases
                .lock()
                .unwrap()
                .push((phase.to_string(), partial.len()));
        })
        .unwrap();

    let result_ids: Vec<_> = results.iter().map(|result| &result.chunk_id).collect();
    let expected_ids: Vec<_> = expected.iter().map(|result| &result.chunk_id).collect();
    assert_eq!(result_ids, expected_ids);
    let phases = phases.into_inner().unwrap();
    assert_eq!(phases[0].0, "bm25");
    assert_eq!(phases[1].0, "exact");
    assert_eq!(phases[1].1, expected.len());
}

#[test]
fn search_with_progress_reports_fused_phase_for_fast() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let phases = Mutex::new(Vec::new());
    let query = SearchQuery::new("add")
        .with_limit(10)
        .with_strategy(Strategy::Fast);

    let results = engine.search_with_progress(query, |phase, partial| {
        phases
            .lock()
            .unwrap()
            .push((phase.to_string(), partial.len()));
    });

    assert!(results.is_ok());

    let phases = phases.into_inner().unwrap();
    assert!(
        phases.len() >= 2,
        "Fast strategy should report at least bm25 + fused phases, got: {phases:?}"
    );
    assert_eq!(phases[0].0, "bm25", "first phase should be BM25");
    assert_eq!(phases[1].0, "fused", "second phase should be fused");
}

#[test]
fn search_with_progress_no_results_still_reports_bm25() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let phases = Mutex::new(Vec::new());
    let query = SearchQuery::new("xyzzy_nonexistent_gibberish_42")
        .with_limit(10)
        .with_strategy(Strategy::Instant);

    let results = engine.search_with_progress(query, |phase, partial| {
        phases
            .lock()
            .unwrap()
            .push((phase.to_string(), partial.len()));
    });

    assert!(results.is_ok());
    let phases = phases.into_inner().unwrap();
    assert_eq!(
        phases.len(),
        1,
        "even with no results, bm25 phase should fire"
    );
    assert_eq!(phases[0].0, "bm25");
    assert_eq!(phases[0].1, 0, "no results expected for nonsense query");
}

#[test]
fn search_usages_prioritizes_production_references() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    let docs = root.join("docs");
    fs::create_dir_all(src.join("logging")).unwrap();
    fs::create_dir_all(src.join("media")).unwrap();
    fs::create_dir_all(src.join("infra")).unwrap();
    fs::create_dir_all(&docs).unwrap();

    fs::write(
        src.join("logging/redact.ts"),
        r#"export function redactSensitiveText(input: string): string {
  return input.replace(/token=.*/, "token=[redacted]");
}
"#,
    )
    .unwrap();
    fs::write(
        src.join("logging/redact.test.ts"),
        r#"import { redactSensitiveText } from "./redact";

test("redactSensitiveText removes secrets", () => {
  expect(redactSensitiveText("token=secret")).toBe("token=[redacted]");
});
"#,
    )
    .unwrap();
    fs::write(
        docs.join("redact.md"),
        "Call redactSensitiveText before writing logs.",
    )
    .unwrap();
    fs::write(
        src.join("media/fetch.ts"),
        r#"import { redactSensitiveText } from "../logging/redact";

export function logMediaFetch(url: string) {
  console.log(redactSensitiveText(url));
}
"#,
    )
    .unwrap();
    fs::write(
        src.join("infra/errors.ts"),
        r#"import { redactSensitiveText } from "../logging/redact";

export function serializeError(error: Error) {
  return redactSensitiveText(error.message);
}
"#,
    )
    .unwrap();
    fs::write(
        src.join("gateway.ts"),
        r#"import { redactSensitiveText } from "./logging/redact";

export function logGateway(message: string) {
  return redactSensitiveText(message);
}
"#,
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();
    let results = engine.search_usages("redactSensitiveText", 3).unwrap();
    let files: Vec<_> = results.iter().map(|r| r.file_path.as_str()).collect();

    assert_eq!(
        results.len(),
        3,
        "expected three top usage results: {files:?}"
    );
    assert!(
        files.iter().all(|path| {
            path.contains("src/media/fetch.ts")
                || path.contains("src/infra/errors.ts")
                || path.contains("src/gateway.ts")
        }),
        "expected production usage files before definitions/tests/docs, got: {files:?}"
    );
}
