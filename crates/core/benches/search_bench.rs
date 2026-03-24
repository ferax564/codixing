use std::fs;
use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};

use codixing_core::retriever::hybrid::rrf_fuse;
use codixing_core::{Engine, IndexConfig, SearchQuery, SearchResult, Strategy};

/// Create a temporary project directory with realistic source files for
/// benchmarking.  Returns the temp directory handle (keep alive!) and root path.
fn setup_bench_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // Generate several files with realistic Rust code.
    for i in 0..20 {
        let content = format!(
            r#"
/// Module {i} documentation.
///
/// This module provides utilities for processing data items.
pub mod module_{i} {{
    /// Configuration for module {i}.
    pub struct Config_{i} {{
        pub verbose: bool,
        pub max_retries: usize,
        pub timeout_ms: u64,
    }}

    impl Config_{i} {{
        /// Create a new default configuration.
        pub fn new() -> Self {{
            Self {{
                verbose: false,
                max_retries: 3,
                timeout_ms: 5000,
            }}
        }}
    }}

    /// Process a batch of items using the given configuration.
    pub fn process_batch(items: &[String], config: &Config_{i}) -> Vec<String> {{
        items
            .iter()
            .map(|item| format!("processed_{{}}_{i}", item))
            .collect()
    }}

    /// Validate input data before processing.
    pub fn validate(input: &str) -> bool {{
        !input.is_empty() && input.len() < 1024
    }}

    /// Helper function to compute hash of input.
    pub fn compute_hash(data: &[u8]) -> u64 {{
        let mut h: u64 = 0;
        for &b in data {{
            h = h.wrapping_mul(31).wrapping_add(b as u64);
        }}
        h
    }}

    #[cfg(test)]
    mod tests {{
        use super::*;

        #[test]
        fn test_validate() {{
            assert!(validate("hello"));
            assert!(!validate(""));
        }}

        #[test]
        fn test_process_batch() {{
            let cfg = Config_{i}::new();
            let items = vec!["a".to_string(), "b".to_string()];
            let result = process_batch(&items, &cfg);
            assert_eq!(result.len(), 2);
        }}
    }}
}}
"#,
        );
        fs::write(src.join(format!("mod_{i}.rs")), content).unwrap();
    }

    // Main entry point file.
    fs::write(
        src.join("main.rs"),
        r#"
/// Application entry point.
fn main() {
    println!("Hello, benchmark world!");
}

/// Add two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub struct AppConfig {
    pub verbose: bool,
    pub threads: usize,
    pub output_dir: String,
}
"#,
    )
    .unwrap();

    (dir, root)
}

/// Create a BM25-only engine over the bench project.
fn setup_engine() -> (tempfile::TempDir, Engine) {
    let (dir, root) = setup_bench_project();
    let mut config = IndexConfig::new(&root);
    config.embedding.enabled = false;
    let engine = Engine::init(&root, config).unwrap();
    (dir, engine)
}

fn bench_bm25_search(c: &mut Criterion) {
    let (_dir, engine) = setup_engine();

    c.bench_function("bm25_search_identifier", |b| {
        b.iter(|| {
            engine
                .search(
                    SearchQuery::new("process_batch")
                        .with_limit(10)
                        .with_strategy(Strategy::Instant),
                )
                .unwrap()
        })
    });

    c.bench_function("bm25_search_natural_language", |b| {
        b.iter(|| {
            engine
                .search(
                    SearchQuery::new("validate input data before processing")
                        .with_limit(10)
                        .with_strategy(Strategy::Instant),
                )
                .unwrap()
        })
    });

    c.bench_function("bm25_search_with_file_filter", |b| {
        b.iter(|| {
            engine
                .search(
                    SearchQuery::new("Config")
                        .with_limit(5)
                        .with_strategy(Strategy::Instant)
                        .with_file_filter("mod_5"),
                )
                .unwrap()
        })
    });
}

fn bench_rrf_fuse(c: &mut Criterion) {
    // Generate synthetic result lists of varying sizes.
    fn make_results(n: usize) -> Vec<SearchResult> {
        (0..n)
            .map(|i| SearchResult {
                chunk_id: format!("chunk_{i}"),
                file_path: format!("src/mod_{}.rs", i % 20),
                language: "Rust".to_string(),
                score: 1.0 / (1.0 + i as f32),
                line_start: (i * 10) as u64,
                line_end: ((i + 1) * 10) as u64,
                signature: format!("fn func_{i}()"),
                scope_chain: vec!["module".to_string()],
                content: format!("fn func_{i}() {{ /* body */ }}"),
            })
            .collect()
    }

    let list_10 = make_results(10);
    let list_10b = make_results(10);
    c.bench_function("rrf_fuse_10x10", |b| {
        b.iter(|| rrf_fuse(&list_10, &list_10b, 60.0))
    });

    let list_50 = make_results(50);
    let list_50b = make_results(50);
    c.bench_function("rrf_fuse_50x50", |b| {
        b.iter(|| rrf_fuse(&list_50, &list_50b, 60.0))
    });

    let list_100 = make_results(100);
    let list_100b = make_results(100);
    c.bench_function("rrf_fuse_100x100", |b| {
        b.iter(|| rrf_fuse(&list_100, &list_100b, 60.0))
    });
}

fn bench_detect_strategy(c: &mut Criterion) {
    let (_dir, engine) = setup_engine();

    c.bench_function("detect_strategy_identifier", |b| {
        b.iter(|| engine.detect_strategy("process_batch"))
    });

    c.bench_function("detect_strategy_natural_language", |b| {
        b.iter(|| engine.detect_strategy("how does the search engine rank results"))
    });

    c.bench_function("detect_strategy_two_words", |b| {
        b.iter(|| engine.detect_strategy("IndexConfig new"))
    });
}

fn bench_sync_noop(c: &mut Criterion) {
    let (dir, root) = setup_bench_project();
    let mut config = IndexConfig::new(&root);
    config.embedding.enabled = false;
    let mut engine = Engine::init(&root, config).unwrap();

    // First sync populates hashes — subsequent syncs should be fast no-ops.
    engine.sync().unwrap();

    c.bench_function("sync_noop_20_files", |b| b.iter(|| engine.sync().unwrap()));

    drop(dir);
}

fn bench_grep_trigram(c: &mut Criterion) {
    let (_dir, engine) = setup_engine();

    // Literal grep — trigram narrows to files containing "process_batch"
    c.bench_function("grep_literal_with_trigram", |b| {
        b.iter(|| {
            engine
                .grep_code("process_batch", true, None, 0, 50)
                .unwrap()
        })
    });
    c.bench_function("grep_literal_full_scan", |b| {
        b.iter(|| {
            engine
                .grep_code_full_scan("process_batch", true, None, 0, 50)
                .unwrap()
        })
    });

    // Regex grep — trigram extracts trigrams from regex pattern
    c.bench_function("grep_regex_with_trigram", |b| {
        b.iter(|| {
            engine
                .grep_code("compute_hash.*data", false, None, 0, 50)
                .unwrap()
        })
    });
    c.bench_function("grep_regex_full_scan", |b| {
        b.iter(|| {
            engine
                .grep_code_full_scan("compute_hash.*data", false, None, 0, 50)
                .unwrap()
        })
    });

    // OR pattern — QueryPlan union of branches
    c.bench_function("grep_or_pattern_with_trigram", |b| {
        b.iter(|| {
            engine
                .grep_code("(process_batch|compute_hash)", false, None, 0, 50)
                .unwrap()
        })
    });
    c.bench_function("grep_or_pattern_full_scan", |b| {
        b.iter(|| {
            engine
                .grep_code_full_scan("(process_batch|compute_hash)", false, None, 0, 50)
                .unwrap()
        })
    });
}

criterion_group!(
    benches,
    bench_bm25_search,
    bench_rrf_fuse,
    bench_detect_strategy,
    bench_sync_noop,
    bench_grep_trigram,
);
criterion_main!(benches);
