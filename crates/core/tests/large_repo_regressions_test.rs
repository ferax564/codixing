//! Regression tests for the retrieval and large-repository fixes that came
//! out of the 2026-11 review (`codixing-review-2026-11-10.md`).
//!
//! Each test pins one behaviour an agent relies on: the definition comes
//! first, a smaller limit returns a prefix of the default results, `usages`
//! misses no call site, tests are found per file, and the lightweight open
//! modes serve exactly the queries they were built for.

use std::fs;
use std::path::Path;

use codixing_core::{AgentContextMode, Engine, IndexConfig, SearchQuery, Strategy};
use tempfile::tempdir;

fn no_embed_config(root: &Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A small Rust project whose type `Widget` has several `impl` blocks in
/// files that sort *before* the file defining the struct.
fn widget_project(root: &Path) {
    write(
        root,
        "src/a_render.rs",
        "use crate::widget::Widget;\n\nimpl Widget {\n    pub fn render(&self) -> String {\n        self.label.clone()\n    }\n}\n",
    );
    write(
        root,
        "src/b_resize.rs",
        "use crate::widget::Widget;\n\nimpl Widget {\n    pub fn resize(&mut self, width: u32) {\n        self.width = width;\n    }\n}\n",
    );
    write(
        root,
        "src/widget.rs",
        "/// A rectangular thing on screen.\npub struct Widget {\n    pub label: String,\n    pub width: u32,\n}\n\npub fn build_widget(label: &str) -> Widget {\n    Widget {\n        label: label.to_string(),\n        width: 0,\n    }\n}\n",
    );
    write(
        root,
        "src/app.rs",
        "use crate::widget::{Widget, build_widget};\n\npub struct App {\n    pub root: Widget,\n}\n\npub fn start() -> App {\n    let inner = App { root: build_widget(\"root\") };\n    let _again = crate::widget::build_widget(\"second\");\n    inner\n}\n",
    );
    write(
        root,
        "tests/widget_test.rs",
        "use demo::widget::build_widget;\n\n#[test]\nfn builds() {\n    assert_eq!(build_widget(\"x\").label, \"x\");\n}\n",
    );
}

#[test]
fn identifier_search_puts_the_type_before_its_impl_blocks() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    widget_project(root);
    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    let strategy = engine.detect_strategy("Widget");
    let results = engine
        .search(
            SearchQuery::new("Widget")
                .with_limit(5)
                .with_strategy(strategy),
        )
        .unwrap();

    assert!(!results.is_empty(), "no results for Widget");
    assert_eq!(
        results[0].file_path,
        "src/widget.rs",
        "`struct Widget` must outrank `impl Widget` blocks, got {:?}",
        results.iter().map(|r| &r.file_path).collect::<Vec<_>>()
    );
    assert!(
        results[0].content.contains("pub struct Widget"),
        "the first hit should show the definition head: {}",
        results[0].content
    );
}

#[test]
fn smaller_limit_returns_a_prefix_of_the_default_results() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    widget_project(root);
    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    for query in ["widget label width", "build widget"] {
        let run = |limit: usize| {
            engine
                .search(
                    SearchQuery::new(query)
                        .with_limit(limit)
                        .with_strategy(Strategy::Instant),
                )
                .unwrap()
                .into_iter()
                .map(|r| (r.file_path, r.line_start))
                .collect::<Vec<_>>()
        };
        let default = run(10);
        for limit in [1, 2, 3] {
            let limited = run(limit);
            assert_eq!(
                limited,
                default.iter().take(limit).cloned().collect::<Vec<_>>(),
                "`{query}` with limit {limit} must be a prefix of the default ranking"
            );
        }
    }
}

#[test]
fn exact_references_find_every_call_site() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    widget_project(root);
    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    let refs = engine.exact_symbol_references("build_widget");
    let at = |file: &str, line_1_based: usize| {
        refs.iter()
            .find(|r| r.file_path == file && r.line + 1 == line_1_based)
            .unwrap_or_else(|| panic!("missing reference {file}:{line_1_based} in {refs:#?}"))
    };

    assert_eq!(at("src/widget.rs", 7).kind, "definition");
    assert_eq!(at("src/app.rs", 1).kind, "import");
    // A plain call and a path-qualified call in production code.
    assert_eq!(at("src/app.rs", 8).kind, "call");
    assert_eq!(at("src/app.rs", 9).kind, "call");
    // The test file is found too.
    assert_eq!(at("tests/widget_test.rs", 5).kind, "call");

    // Whole words only: `build_widget` must not match inside another name.
    write(root, "src/other.rs", "pub fn rebuild_widget_cache() {}\n");
    let mut engine = engine;
    engine.sync().unwrap();
    let refs = engine.exact_symbol_references("build_widget");
    assert!(
        refs.iter().all(|r| r.file_path != "src/other.rs"),
        "substring occurrences are not references: {refs:#?}"
    );
}

#[test]
fn complete_reference_mode_is_exhaustive_and_ordered() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    widget_project(root);
    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    let refs = engine.symbol_references(
        "build_widget",
        codixing_core::ReferenceOptions {
            complete: true,
            max_results: None,
        },
    );
    let locations: Vec<(String, usize)> = refs
        .iter()
        .map(|r| (r.file_path.clone(), r.line + 1))
        .collect();
    let mut sorted = locations.clone();
    sorted.sort();
    assert_eq!(
        locations, sorted,
        "complete mode is ordered by (file, line)"
    );
    for expected in [
        ("src/app.rs".to_string(), 8),
        ("src/app.rs".to_string(), 9),
        ("src/widget.rs".to_string(), 7),
        ("tests/widget_test.rs".to_string(), 5),
    ] {
        assert!(
            locations.contains(&expected),
            "complete mode missed {expected:?}: {locations:?}"
        );
    }
}

#[test]
fn tests_are_found_per_source_file() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    widget_project(root);
    // More than 200 unrelated test/source pairs: the old project-wide map kept
    // only its top 200 entries, so most files reported no tests at all.
    for i in 0..230 {
        write(
            root,
            &format!("src/gen/m{i}.rs"),
            &format!("pub fn f{i}() {{}}\n"),
        );
        write(
            root,
            &format!("src/gen/m{i}_test.rs"),
            &format!("#[test]\nfn t{i}() {{}}\n"),
        );
    }
    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    let tests = engine.find_tests_for_file("src/widget.rs");
    assert!(
        tests.iter().any(|m| m.test_file == "tests/widget_test.rs"),
        "naming-convention test not found: {tests:#?}"
    );

    let generated = engine.find_tests_for_file("src/gen/m229.rs");
    assert!(
        generated
            .iter()
            .any(|m| m.test_file == "src/gen/m229_test.rs"),
        "per-file lookup must not be cut off by a global limit: {generated:#?}"
    );

    let sources = engine.find_source_for_test("src/gen/m7_test.rs");
    assert!(
        sources.iter().any(|m| m.source_file == "src/gen/m7.rs"),
        "reverse lookup failed: {sources:#?}"
    );
}

#[test]
fn ask_does_not_pin_symbols_for_ordinary_words() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/sync.rs",
        "/// Decide which files changed since the last run and reindex them.\npub fn sync(root: &str) -> usize {\n    let changed = detect_changed_files(root);\n    changed.len()\n}\n\nfn detect_changed_files(_root: &str) -> Vec<String> {\n    Vec::new()\n}\n",
    );
    // Symbols whose names merely *contain* a long word of the task.
    write(
        root,
        "tests/incremental_test.rs",
        "#[test]\nfn incremental_update_removes_file() {}\n\n#[test]\nfn incremental_symbol_uniqueness() {}\n\n#[test]\nfn incremental_delta_reloads() {}\n",
    );
    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    let task = "how does incremental sync decide which files changed";
    let pack = engine
        .agent_context_pack(
            task,
            AgentContextMode::infer_from_task(task),
            4000,
            &[],
            None,
            None,
        )
        .unwrap();

    let first = pack.must_read.first().expect("pack has evidence");
    assert_eq!(
        first.path, "src/sync.rs",
        "the implementation must lead the pack, not tests named after a task word: {:#?}",
        pack.must_read
    );
    assert!(
        pack.related_symbols.iter().all(|s| s.kind != "import"),
        "imports carry no information for an agent: {:#?}",
        pack.related_symbols
    );

    // The text rendering carries the same evidence at a fraction of the size.
    let text = pack.render_text();
    let json = serde_json::to_string(&pack).unwrap();
    assert!(text.contains("src/sync.rs:"), "{text}");
    assert!(
        text.len() < json.len(),
        "text ({} bytes) should be smaller than JSON ({} bytes)",
        text.len(),
        json.len()
    );
}

#[test]
fn context_selects_the_innermost_function() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let mut source = String::from("pub struct Machine;\n\nimpl Machine {\n");
    for i in 0..40 {
        source.push_str(&format!(
            "    pub fn step_{i}(&self) -> u32 {{\n        let value = {i};\n        value + 1\n    }}\n\n"
        ));
    }
    source.push_str("}\n");
    write(root, "src/machine.rs", &source);
    let engine = Engine::init(root, no_embed_config(root)).unwrap();

    // 0-based line of `let value = 20;` inside `step_20`.
    let line = source
        .lines()
        .position(|l| l.contains("let value = 20;"))
        .unwrap() as u64;
    let ctx = engine.assemble_context_for_location_with_mode(
        "src/machine.rs",
        line,
        4096,
        codixing_core::engine::context_assembly::BudgetMode::Strict,
    );

    assert!(
        ctx.primary.content.contains("fn step_20"),
        "primary chunk should be the enclosing method: {}",
        ctx.primary.content
    );
    assert!(
        !ctx.primary.content.contains("fn step_0("),
        "primary chunk must not be the whole impl block: {} lines",
        ctx.primary.content.lines().count()
    );
}

#[test]
fn lightweight_open_modes_serve_their_queries_and_reject_search() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    widget_project(root);
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    // grep-only engine: text scan works without symbols or chunk metadata.
    let files = Engine::open_read_only_files(root).unwrap();
    let matches = files
        .grep_code_opts(&codixing_core::GrepOptions {
            pattern: "build_widget".to_string(),
            literal: true,
            case_insensitive: false,
            invert: false,
            file_glob: None,
            before_context: 0,
            after_context: 0,
            limit: 100,
            count_mode: false,
        })
        .unwrap();
    assert!(
        matches.iter().any(|m| m.file_path == "src/app.rs"),
        "grep-only engine missed a match: {matches:#?}"
    );
    assert!(
        files.search(SearchQuery::new("Widget")).is_err(),
        "search must be rejected on a grep-only engine, not answered from an empty index"
    );

    // symbols-only engine.
    let symbols = Engine::open_read_only_symbols(root).unwrap();
    assert!(
        symbols
            .symbols("Widget", None)
            .unwrap()
            .iter()
            .any(|s| s.name == "Widget" && s.file_path == "src/widget.rs")
    );
    assert!(symbols.search(SearchQuery::new("Widget")).is_err());

    // reference engine: exact references, including the definition kind.
    let references = Engine::open_read_only_references(root).unwrap();
    let refs = references.exact_symbol_references("build_widget");
    assert!(refs.iter().any(|r| r.kind == "definition"));
    assert!(refs.iter().any(|r| r.kind == "call"));
}

#[test]
fn symbol_table_validation_stamp_is_written_and_bound_to_the_bytes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    widget_project(root);
    drop(Engine::init(root, no_embed_config(root)).unwrap());

    // Locate the published symbol table and its stamp.
    let generations = root.join(".codixing/generations");
    let generation = fs::read_dir(&generations)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.join("symbols_v2.bin").is_file())
        .expect("a generation with symbols_v2.bin");
    let symbols = generation.join("symbols_v2.bin");
    let stamp = generation.join("symbols_v2.bin.sum");
    assert!(
        stamp.is_file(),
        "init must stamp the symbol table it validated"
    );

    // A stamp that does not match the bytes is ignored: the table still
    // loads (through full validation) and lookups still work.
    fs::write(&stamp, b"not a real stamp").unwrap();
    let engine = Engine::open_read_only(root).unwrap();
    assert!(
        engine
            .symbols("Widget", None)
            .unwrap()
            .iter()
            .any(|s| s.name == "Widget")
    );
    drop(engine);

    // A matching stamp must never vouch for different bytes: corrupt the
    // table but keep the old stamp, and loading must fall back to validation
    // and reject the file rather than serve garbage.
    let original_stamp = {
        // Re-create a valid stamp by letting a writer open the index.
        fs::remove_file(&stamp).unwrap();
        drop(Engine::open(root).unwrap());
        fs::read(&stamp).expect("writer open re-stamps the table")
    };
    let mut bytes = fs::read(&symbols).unwrap();
    let middle = bytes.len() / 2;
    let end = (middle + 16).min(bytes.len());
    for byte in &mut bytes[middle..end] {
        *byte ^= 0xFF;
    }
    fs::write(&symbols, &bytes).unwrap();
    fs::write(&stamp, &original_stamp).unwrap();
    assert!(
        codixing_core::symbols::mmap::MmapSymbolTable::load(&symbols).is_err(),
        "a stale stamp must not let a modified symbol table skip validation"
    );
}
