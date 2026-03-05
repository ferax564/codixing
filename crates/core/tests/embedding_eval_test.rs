//! Embedding evaluation harness — BM25 vs hybrid Recall@k comparison.
//!
//! All tests in this file are `#[ignore]` because they download ONNX model
//! weights (~130 MB for BGE-Base, ~45 MB for BGE-Small) on first run and
//! take 30–120 s to complete depending on hardware.
//!
//! # Running
//! ```bash
//! FASTEMBED_CACHE_DIR=~/.cache/fastembed \
//!   cargo test --test embedding_eval_test -- --ignored --nocapture
//! ```
//!
//! # What these tests measure
//! - `compare_bm25_vs_hybrid_recall`: Overall Recall@k for BM25-only
//!   (`Strategy::Instant`) vs hybrid BM25+vector (`Strategy::Fast`) on a
//!   12-query suite (mix of identifier and pure-NL queries).
//! - `compare_embedding_models`: NL-only Recall@5 across `BgeSmallEn` (384d)
//!   and `BgeBaseEn` (768d) to detect whether upgrading the model improves
//!   recall for natural-language engineering queries.

use std::time::Instant;

use tempfile::TempDir;

use codixing_core::config::{EmbeddingConfig, EmbeddingModel};
use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};

// ---------------------------------------------------------------------------
// Eval case definition
// ---------------------------------------------------------------------------

struct EvalCase {
    /// The natural-language or identifier query sent to the search engine.
    query: &'static str,
    /// Substring of `file_path` that must appear in the top-`k` results.
    expected_file: &'static str,
    /// Recall cut-off.
    k: usize,
    /// `"identifier"` — token present verbatim in the source;
    /// `"nl"` — no exact identifier match, semantic similarity required.
    query_type: &'static str,
}

/// 12 queries: 4 identifier (BM25 should dominate) + 8 pure NL (vector helps).
const EVAL_CASES: &[EvalCase] = &[
    // ------------------------------------------------------------------
    // Identifier queries — BM25 should be strong here.
    // ------------------------------------------------------------------
    EvalCase {
        query: "Parser",
        expected_file: "src/parser.rs",
        k: 3,
        query_type: "identifier",
    },
    EvalCase {
        query: "BM25Retriever Tantivy",
        expected_file: "src/retriever.rs",
        k: 3,
        query_type: "identifier",
    },
    EvalCase {
        query: "split_camel_case",
        expected_file: "src/tokenizer.rs",
        k: 5,
        query_type: "identifier",
    },
    EvalCase {
        query: "VectorIndex",
        expected_file: "src/vector.rs",
        k: 3,
        query_type: "identifier",
    },
    // ------------------------------------------------------------------
    // Natural-language queries — vector embeddings should help here.
    // ------------------------------------------------------------------
    EvalCase {
        query: "parse source code into tree structure",
        expected_file: "src/parser.rs",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "find nearest neighbor in high dimensional space",
        expected_file: "src/vector.rs",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "configuration settings for indexing pipeline",
        expected_file: "src/config.rs",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "split identifiers on word boundaries camel case",
        expected_file: "src/tokenizer.rs",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "search interface that backends implement",
        expected_file: "src/retriever.rs",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "coordinate parsing and retrieval",
        expected_file: "src/engine.rs",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "serialize configuration to disk",
        expected_file: "src/config.rs",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "approximate nearest neighbor quantization",
        expected_file: "src/vector.rs",
        k: 5,
        query_type: "nl",
    },
];

// ---------------------------------------------------------------------------
// Synthetic codebase — same 6 core files as retrieval_quality_test.rs
// plus 4 distractor files to raise the retrieval difficulty.
// ---------------------------------------------------------------------------

/// Core source files (identical to `retrieval_quality_test.rs`).
const CORE_FILES: &[(&str, &str)] = &[
    (
        "src/parser.rs",
        r#"
/// Parses source code into an AST.
pub struct Parser {
    language: Language,
}

impl Parser {
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    /// Parse a UTF-8 source buffer and return the root node.
    pub fn parse(&self, source: &[u8]) -> Option<Tree> {
        todo!()
    }
}
"#,
    ),
    (
        "src/engine.rs",
        r#"
use crate::parser::Parser;
use crate::retriever::Retriever;

/// Top-level search engine coordinating parsing and retrieval.
pub struct Engine {
    parser: Parser,
    retriever: Box<dyn Retriever>,
}

impl Engine {
    pub fn init(root: &str) -> Self {
        todo!()
    }

    /// Search for code matching the query string.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        todo!()
    }
}
"#,
    ),
    (
        "src/retriever.rs",
        r#"
/// Trait implemented by all retrieval backends.
pub trait Retriever {
    fn search(&self, query: &str, limit: usize) -> Vec<SearchResult>;
}

/// BM25 retriever backed by Tantivy.
pub struct BM25Retriever {
    index: TantivyIndex,
}

impl Retriever for BM25Retriever {
    fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        self.index.search(query, limit)
    }
}
"#,
    ),
    (
        "src/tokenizer.rs",
        r#"
/// Splits code identifiers into searchable sub-tokens.
///
/// Handles camelCase, snake_case, PascalCase, and dot.path.names.
pub struct CodeTokenizer;

impl CodeTokenizer {
    pub fn tokenize(text: &str) -> Vec<String> {
        split_on_boundaries(text)
            .into_iter()
            .flat_map(|word| split_camel_case(&word))
            .collect()
    }
}

fn split_camel_case(word: &str) -> Vec<String> {
    // implementation omitted
    vec![word.to_lowercase()]
}

fn split_on_boundaries(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .map(str::to_string)
        .collect()
}
"#,
    ),
    (
        "src/config.rs",
        r#"
use serde::{Deserialize, Serialize};

/// Configuration for the indexing pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    pub root: String,
    pub max_chunk_size: usize,
    pub embedding_enabled: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            root: ".".into(),
            max_chunk_size: 1500,
            embedding_enabled: true,
        }
    }
}
"#,
    ),
    (
        "src/vector.rs",
        r#"
/// HNSW approximate nearest-neighbour vector index.
pub struct VectorIndex {
    dims: usize,
    quantize: bool,
}

impl VectorIndex {
    pub fn new(dims: usize, quantize: bool) -> Self {
        Self { dims, quantize }
    }

    /// Find the k nearest vectors to `query`.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        todo!()
    }

    /// Add a vector associated with `chunk_id`.
    pub fn add(&mut self, chunk_id: u64, vector: &[f32]) {
        todo!()
    }
}
"#,
    ),
];

/// Distractor files — increase retrieval difficulty by adding plausible noise.
const DISTRACTOR_FILES: &[(&str, &str)] = &[
    (
        "src/cache.rs",
        r#"
/// LRU cache implementation with configurable eviction policy.
pub struct LruCache<K, V> {
    capacity: usize,
    map: std::collections::LinkedHashMap<K, V>,
}

impl<K: Eq + std::hash::Hash, V> LruCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, map: Default::default() }
    }

    /// Insert a key-value pair.  Evicts the least-recently-used entry when
    /// the cache is at capacity.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        todo!()
    }

    /// Look up an entry and mark it as recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        todo!()
    }
}
"#,
    ),
    (
        "src/logger.rs",
        r#"
/// Structured logger with configurable severity levels.
pub struct Logger {
    level: LogLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl Logger {
    pub fn new(level: LogLevel) -> Self {
        Self { level }
    }

    pub fn info(&self, msg: &str) { todo!() }
    pub fn warn(&self, msg: &str) { todo!() }
    pub fn error(&self, msg: &str) { todo!() }
}
"#,
    ),
    (
        "src/metrics.rs",
        r#"
/// Performance counters and histograms for runtime monitoring.
pub struct MetricsRegistry {
    counters: std::collections::HashMap<String, u64>,
    histograms: std::collections::HashMap<String, Vec<f64>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            counters: Default::default(),
            histograms: Default::default(),
        }
    }

    /// Increment a named counter by `delta`.
    pub fn increment(&mut self, name: &str, delta: u64) {
        *self.counters.entry(name.to_string()).or_default() += delta;
    }

    /// Record an observation into a histogram bucket.
    pub fn observe(&mut self, name: &str, value: f64) {
        self.histograms.entry(name.to_string()).or_default().push(value);
    }
}
"#,
    ),
    (
        "src/auth.rs",
        r#"
/// JWT token validation and session management.
pub struct AuthService {
    secret: Vec<u8>,
    session_ttl_secs: u64,
}

impl AuthService {
    pub fn new(secret: Vec<u8>, session_ttl_secs: u64) -> Self {
        Self { secret, session_ttl_secs }
    }

    /// Validate a JWT and return the subject claim on success.
    pub fn validate_token(&self, token: &str) -> Option<String> {
        todo!()
    }

    /// Create a new session token for `subject`.
    pub fn create_token(&self, subject: &str) -> String {
        todo!()
    }
}
"#,
    ),
];

// ---------------------------------------------------------------------------
// Index builder helpers
// ---------------------------------------------------------------------------

/// Build an index with `BgeBaseEn` embeddings enabled (production defaults).
///
/// Creates all 6 core files plus 4 distractor files, then runs `Engine::init`.
/// Returns `(engine, tmp_dir)` — caller must keep `tmp_dir` alive.
fn build_eval_index() -> (Engine, TempDir) {
    build_index_with_model(EmbeddingModel::BgeBaseEn)
}

/// Build an index using the given embedding `model`.
fn build_index_with_model(model: EmbeddingModel) -> (Engine, TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    for (rel_path, content) in CORE_FILES.iter().chain(DISTRACTOR_FILES.iter()) {
        let abs = root.join(rel_path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
    }

    let mut config = IndexConfig::new(root);
    config.embedding = EmbeddingConfig {
        enabled: true,
        model,
        contextual_embeddings: true, // production default
        quantize: true,              // production default
        reranker_enabled: false,
        ..Default::default()
    };

    let engine = Engine::init(root, config).expect("index build failed");
    (engine, tmp)
}

// ---------------------------------------------------------------------------
// Test 1: BM25 vs Hybrid Recall@k
// ---------------------------------------------------------------------------

/// Comprehensive BM25 vs hybrid comparison across 12 queries.
///
/// Prints a side-by-side table, per-type breakdown, latency statistics, and
/// a verdict on whether fine-tuning BGE on CodeSearchNet is recommended.
///
/// Run with:
/// ```bash
/// FASTEMBED_CACHE_DIR=~/.cache/fastembed \
///   cargo test --test embedding_eval_test compare_bm25_vs_hybrid_recall \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn compare_bm25_vs_hybrid_recall() {
    let (engine, _tmp) = build_eval_index();

    let mut bm25_hits = 0usize;
    let mut hybrid_hits = 0usize;
    let mut bm25_nl_hits = 0usize;
    let mut hybrid_nl_hits = 0usize;
    let mut bm25_total_ms = 0u128;
    let mut hybrid_total_ms = 0u128;

    let total = EVAL_CASES.len();
    let nl_count = EVAL_CASES.iter().filter(|c| c.query_type == "nl").count();

    println!();
    println!(
        "{:<50} {:>8} {:>8} {:>10}",
        "Query", "BM25", "Hybrid", "Type"
    );
    println!("{}", "-".repeat(80));

    for case in EVAL_CASES {
        // --- BM25 only (Strategy::Instant) ---
        let t0 = Instant::now();
        let bm25_results = engine
            .search(
                SearchQuery::new(case.query)
                    .with_limit(case.k)
                    .with_strategy(Strategy::Instant),
            )
            .unwrap_or_default();
        bm25_total_ms += t0.elapsed().as_millis();

        let bm25_hit = bm25_results
            .iter()
            .any(|r| r.file_path.contains(case.expected_file));

        // --- Hybrid BM25+vector (Strategy::Fast) ---
        let t0 = Instant::now();
        let hybrid_results = engine
            .search(
                SearchQuery::new(case.query)
                    .with_limit(case.k)
                    .with_strategy(Strategy::Fast),
            )
            .unwrap_or_default();
        hybrid_total_ms += t0.elapsed().as_millis();

        let hybrid_hit = hybrid_results
            .iter()
            .any(|r| r.file_path.contains(case.expected_file));

        if bm25_hit {
            bm25_hits += 1;
        }
        if hybrid_hit {
            hybrid_hits += 1;
        }
        if case.query_type == "nl" {
            if bm25_hit {
                bm25_nl_hits += 1;
            }
            if hybrid_hit {
                hybrid_nl_hits += 1;
            }
        }

        // Truncate long queries for display.
        let display_query = if case.query.len() > 48 {
            &case.query[..48]
        } else {
            case.query
        };

        println!(
            "{:<50} {:>8} {:>8} {:>10}",
            display_query,
            if bm25_hit { "HIT" } else { "miss" },
            if hybrid_hit { "HIT" } else { "miss" },
            case.query_type,
        );
    }

    println!("{}", "-".repeat(80));

    // --- Summary statistics ---
    let bm25_recall = bm25_hits as f64 / total as f64 * 100.0;
    let hybrid_recall = hybrid_hits as f64 / total as f64 * 100.0;
    let overall_gap = hybrid_recall - bm25_recall;

    let bm25_nl_recall = bm25_nl_hits as f64 / nl_count as f64 * 100.0;
    let hybrid_nl_recall = hybrid_nl_hits as f64 / nl_count as f64 * 100.0;
    let nl_gap = hybrid_nl_recall - bm25_nl_recall;

    let bm25_avg_ms = if total > 0 {
        bm25_total_ms / total as u128
    } else {
        0
    };
    let hybrid_avg_ms = if total > 0 {
        hybrid_total_ms / total as u128
    } else {
        0
    };

    println!();
    println!("=== RESULTS ===");
    println!(
        "Overall  — BM25: {:.0}% ({}/{}),  Hybrid: {:.0}% ({}/{}),  gap: {:+.0}%",
        bm25_recall, bm25_hits, total, hybrid_recall, hybrid_hits, total, overall_gap,
    );
    println!(
        "NL-only  — BM25: {:.0}% ({}/{}),  Hybrid: {:.0}% ({}/{}),  gap: {:+.0}%",
        bm25_nl_recall, bm25_nl_hits, nl_count, hybrid_nl_recall, hybrid_nl_hits, nl_count, nl_gap,
    );
    println!(
        "Latency  — BM25: {}ms avg,  Hybrid: {}ms avg",
        bm25_avg_ms, hybrid_avg_ms,
    );

    // --- Verdict ---
    println!();
    if nl_gap > 15.0 {
        println!(
            "VERDICT: Hybrid beats BM25-only by {:.0}% on NL queries. \
             Fine-tuning BGE on CodeSearchNet is RECOMMENDED.",
            nl_gap
        );
    } else if nl_gap > 5.0 {
        println!(
            "VERDICT: Hybrid shows {:.0}% gain on NL queries. \
             Consider fine-tuning if retrieval quality is critical.",
            nl_gap
        );
    } else {
        println!(
            "VERDICT: Hybrid gap is {:.0}% on NL queries. \
             Current BM25 is sufficient; fine-tuning is NOT recommended.",
            nl_gap
        );
    }

    // --- Guard: hybrid must not significantly regress vs BM25 ---
    assert!(
        hybrid_hits >= bm25_hits.saturating_sub(1),
        "Hybrid strategy regressed vs BM25: bm25={} hybrid={} — \
         investigate RRF weights or vector index quality",
        bm25_hits,
        hybrid_hits,
    );
}

// ---------------------------------------------------------------------------
// Test 2: Model comparison — BgeSmallEn vs BgeBaseEn
// ---------------------------------------------------------------------------

/// Compare NL Recall@5 and average embed latency across two BGE model sizes.
///
/// Builds two separate indices (one per model) and runs all NL queries from
/// `EVAL_CASES` with `Strategy::Fast` on each.
///
/// Prints a per-query hit/miss table and a summary with the recommendation.
///
/// Run with:
/// ```bash
/// FASTEMBED_CACHE_DIR=~/.cache/fastembed \
///   cargo test --test embedding_eval_test compare_embedding_models \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn compare_embedding_models() {
    let nl_cases: Vec<&EvalCase> = EVAL_CASES
        .iter()
        .filter(|c| c.query_type == "nl")
        .collect();

    let nl_count = nl_cases.len();

    // Models to compare.  JinaEmbedCode is intentionally omitted to avoid
    // a third large model download in typical CI/dev environments.
    let models: &[(&str, EmbeddingModel)] = &[
        ("BgeSmallEn (384d)", EmbeddingModel::BgeSmallEn),
        ("BgeBaseEn  (768d)", EmbeddingModel::BgeBaseEn),
    ];

    // (model_label, hits, total_search_ms)
    let mut results: Vec<(&str, usize, u128)> = Vec::new();

    // Per-query hit matrix — rows=queries, cols=models.
    let mut hit_matrix: Vec<Vec<bool>> = vec![vec![false; models.len()]; nl_count];

    for (col, (label, model)) in models.iter().enumerate() {
        println!("\nBuilding index for {}...", label);
        let t_build = Instant::now();
        let (engine, _tmp) = build_index_with_model(model.clone());
        println!("  Index built in {}ms", t_build.elapsed().as_millis());

        let mut hits = 0usize;
        let mut total_ms = 0u128;

        for (row, case) in nl_cases.iter().enumerate() {
            let t0 = Instant::now();
            let search_results = engine
                .search(
                    SearchQuery::new(case.query)
                        .with_limit(case.k)
                        .with_strategy(Strategy::Fast),
                )
                .unwrap_or_default();
            total_ms += t0.elapsed().as_millis();

            let hit = search_results
                .iter()
                .any(|r| r.file_path.contains(case.expected_file));

            hit_matrix[row][col] = hit;
            if hit {
                hits += 1;
            }
        }

        results.push((label, hits, total_ms));
    }

    // --- Print comparison table ---
    println!();
    println!("{}", "=".repeat(90));
    println!("Model comparison — NL Recall@k (Strategy::Fast)");
    println!("{}", "=".repeat(90));

    // Header
    let header_pad = 52usize;
    print!("{:<width$}", "NL Query", width = header_pad);
    for (label, _, _) in &results {
        print!("{:>20}", label);
    }
    println!();
    println!("{}", "-".repeat(header_pad + 20 * results.len()));

    // Rows
    for (row, case) in nl_cases.iter().enumerate() {
        let display_query = if case.query.len() > 50 {
            &case.query[..50]
        } else {
            case.query
        };
        print!("{:<width$}", display_query, width = header_pad);
        for col in 0..results.len() {
            print!(
                "{:>20}",
                if hit_matrix[row][col] { "HIT" } else { "miss" }
            );
        }
        println!();
    }

    println!("{}", "-".repeat(header_pad + 20 * results.len()));

    // Summary row
    print!("{:<width$}", "NL Recall@k", width = header_pad);
    for (_, hits, _) in &results {
        let recall_pct = *hits as f64 / nl_count as f64 * 100.0;
        print!("{:>20}", format!("{:.0}% ({}/{})", recall_pct, hits, nl_count));
    }
    println!();

    // Latency row
    print!("{:<width$}", "Avg search latency", width = header_pad);
    for (_, _, total_ms) in &results {
        let avg_ms = if nl_count > 0 {
            total_ms / nl_count as u128
        } else {
            0
        };
        print!("{:>20}", format!("{}ms", avg_ms));
    }
    println!();
    println!("{}", "=".repeat(header_pad + 20 * results.len()));

    // --- Verdict ---
    println!();
    if results.len() >= 2 {
        let (small_label, small_hits, _) = results[0];
        let (base_label, base_hits, _) = results[1];
        let small_recall = small_hits as f64 / nl_count as f64 * 100.0;
        let base_recall = base_hits as f64 / nl_count as f64 * 100.0;
        let gain = base_recall - small_recall;

        if gain > 10.0 {
            println!(
                "VERDICT: {} outperforms {} by {:.0}% on NL recall. \
                 Upgrading to the 768d model is RECOMMENDED for production.",
                base_label, small_label, gain
            );
        } else if gain > 0.0 {
            println!(
                "VERDICT: {} edges out {} by {:.0}%. \
                 Upgrade if memory budget allows.",
                base_label, small_label, gain
            );
        } else {
            println!(
                "VERDICT: {} matches or exceeds {} (gap: {:.0}%). \
                 The smaller model is sufficient; switching is NOT recommended.",
                small_label,
                base_label,
                gain.abs()
            );
        }
    }

    // Both models must beat a trivial 25% baseline on NL queries.
    for (label, hits, _) in &results {
        let recall = *hits as f64 / nl_count as f64;
        assert!(
            recall >= 0.25,
            "Model {} NL recall {:.0}% is below the 25% baseline — \
             vector index may not be loading correctly",
            label,
            recall * 100.0,
        );
    }
}
// ---------------------------------------------------------------------------
// Test 3: Real-codebase eval — Codixing own source (83 files)
// ---------------------------------------------------------------------------

/// Eval cases designed with a deliberate lexical gap between query and answer.
///
/// For each case the query deliberately avoids the exact identifier/filename
/// so BM25 must rely on co-occurring tokens while the vector model can bridge
/// the semantic gap through learned associations.
const REAL_CODEBASE_CASES: &[EvalCase] = &[
    // --- Architecture / concept queries (no exact file name in query) ---
    EvalCase {
        query: "component that scores documents by graph centrality",
        expected_file: "pagerank",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "data structure mapping file paths to dependency edges",
        expected_file: "graph",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "reciprocal rank fusion of two ranked lists",
        expected_file: "retriever",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "merge adjacent context windows from the same file",
        expected_file: "formatter",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "convert raw import path to resolved file location",
        expected_file: "resolver",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "serialize index metadata to disk between sessions",
        expected_file: "persistence",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "split camelCase and snake_case identifiers into tokens",
        expected_file: "tantivy",   // CodeTokenizer is in index/tantivy.rs
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "watch filesystem for changes and trigger incremental reindex",
        expected_file: "watcher",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "approximate nearest neighbour search with memory quantization",
        expected_file: "vector",
        k: 5,
        query_type: "nl",
    },
    EvalCase {
        query: "extract function and class names from source AST",
        expected_file: "language",
        k: 5,
        query_type: "nl",
    },
    // --- Identifier queries: BM25 should dominate ---
    EvalCase {
        query: "Engine",
        expected_file: "engine",
        k: 3,
        query_type: "identifier",
    },
    EvalCase {
        query: "IndexConfig",
        expected_file: "config",
        k: 3,
        query_type: "identifier",
    },
    EvalCase {
        query: "CodeGraph pagerank",
        expected_file: "pagerank",
        k: 3,
        query_type: "identifier",
    },
    EvalCase {
        query: "BM25Retriever",
        expected_file: "bm25",   // retriever/bm25.rs
        k: 3,
        query_type: "identifier",
    },
];

/// Index Codixing's own source tree and compare BM25 vs hybrid on hard NL queries.
///
/// This test uses 83 real Rust files where the lexical gap between query tokens
/// and document tokens is intentionally maximised — the expected file is never
/// named in the query.
///
/// Run with:
/// ```bash
/// FASTEMBED_CACHE_DIR=~/.cache/fastembed \
///   cargo test --test embedding_eval_test real_codebase_bm25_vs_hybrid \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn real_codebase_bm25_vs_hybrid() {
    // Path to the actual codixing source tree — skip if not available.
    let codixing_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()   // crates/core → crates
        .unwrap()
        .parent()   // crates → repo root
        .unwrap()
        .to_path_buf();

    if !codixing_src.join("Cargo.toml").exists() {
        eprintln!("real_codebase_bm25_vs_hybrid: skipped (repo root not found)");
        return;
    }

    // Build a temporary index with BGE-Base embeddings over the real source.
    let tmp = tempfile::tempdir().expect("tempdir");

    // Copy only the crates/core/src tree so we test on Rust code, not docs/editors.
    let src_root = codixing_src.join("crates/core/src");
    let dest_root = tmp.path().join("src");
    copy_dir_recursive(&src_root, &dest_root);

    let mut config = IndexConfig::new(tmp.path());
    config.embedding = EmbeddingConfig {
        enabled: true,
        model: EmbeddingModel::BgeBaseEn,
        contextual_embeddings: true,
        quantize: true,
        reranker_enabled: false,
        ..Default::default()
    };

    println!("\nBuilding index over real codixing source ({} files)...",
        count_rs_files(tmp.path()));
    let t_build = Instant::now();
    let engine = Engine::init(tmp.path(), config).expect("init failed");
    println!("Index built in {}ms  ({} files, {} chunks)",
        t_build.elapsed().as_millis(),
        engine.stats().file_count,
        engine.stats().chunk_count,
    );

    let total = REAL_CODEBASE_CASES.len();
    let nl_cases: Vec<&EvalCase> = REAL_CODEBASE_CASES.iter()
        .filter(|c| c.query_type == "nl").collect();
    let id_cases: Vec<&EvalCase> = REAL_CODEBASE_CASES.iter()
        .filter(|c| c.query_type == "identifier").collect();

    let mut bm25_nl = 0usize;
    let mut hybrid_nl = 0usize;
    let mut bm25_id = 0usize;
    let mut hybrid_id = 0usize;
    let mut bm25_ms = 0u128;
    let mut hybrid_ms = 0u128;

    println!();
    println!("{:<55} {:>6} {:>8}  {}", "Query", "BM25", "Hybrid", "Type");
    println!("{}", "-".repeat(82));

    for case in REAL_CODEBASE_CASES {
        let t0 = Instant::now();
        let bm25_res = engine.search(
            SearchQuery::new(case.query).with_limit(case.k).with_strategy(Strategy::Instant)
        ).unwrap_or_default();
        bm25_ms += t0.elapsed().as_millis();
        let bm25_hit = bm25_res.iter().any(|r| r.file_path.contains(case.expected_file));

        let t0 = Instant::now();
        let hybrid_res = engine.search(
            SearchQuery::new(case.query).with_limit(case.k).with_strategy(Strategy::Fast)
        ).unwrap_or_default();
        hybrid_ms += t0.elapsed().as_millis();
        let hybrid_hit = hybrid_res.iter().any(|r| r.file_path.contains(case.expected_file));

        match case.query_type {
            "nl"         => { if bm25_hit { bm25_nl += 1; } if hybrid_hit { hybrid_nl += 1; } }
            "identifier" => { if bm25_hit { bm25_id += 1; } if hybrid_hit { hybrid_id += 1; } }
            _            => {}
        }

        let q = if case.query.len() > 53 { &case.query[..53] } else { case.query };
        println!("{:<55} {:>6} {:>8}  {}",
            q,
            if bm25_hit { "HIT" } else { "miss" },
            if hybrid_hit { "HIT" } else { "miss" },
            case.query_type,
        );
    }

    println!("{}", "-".repeat(82));

    let nl_n  = nl_cases.len();
    let id_n  = id_cases.len();
    let bm25_nl_pct    = bm25_nl   as f64 / nl_n  as f64 * 100.0;
    let hybrid_nl_pct  = hybrid_nl as f64 / nl_n  as f64 * 100.0;
    let bm25_id_pct    = bm25_id   as f64 / id_n  as f64 * 100.0;
    let hybrid_id_pct  = hybrid_id as f64 / id_n  as f64 * 100.0;
    let nl_gap         = hybrid_nl_pct - bm25_nl_pct;

    println!();
    println!("=== RESULTS (real codebase, {} files) ===", engine.stats().file_count);
    println!("NL queries    BM25: {:.0}% ({}/{}),  Hybrid: {:.0}% ({}/{}),  gap: {:+.0}%",
        bm25_nl_pct, bm25_nl, nl_n, hybrid_nl_pct, hybrid_nl, nl_n, nl_gap);
    println!("Identifiers   BM25: {:.0}% ({}/{}),  Hybrid: {:.0}% ({}/{})",
        bm25_id_pct, bm25_id, id_n, hybrid_id_pct, hybrid_id, id_n);
    println!("Latency       BM25: {}ms avg,  Hybrid: {}ms avg",
        bm25_ms / total as u128, hybrid_ms / total as u128);

    println!();
    if nl_gap > 15.0 {
        println!(
            "VERDICT ▶ Hybrid beats BM25 by {:.0}% on NL queries over a real codebase.\n\
             Fine-tuning BGE on CodeSearchNet is STRONGLY RECOMMENDED.\n\
             Expected gain: +{:.0}–{:.0}% additional NL recall after domain-specific training.",
            nl_gap, nl_gap * 0.3, nl_gap * 0.6
        );
    } else if nl_gap > 5.0 {
        println!(
            "VERDICT ▶ Moderate hybrid gain ({:.0}%) on NL queries.\n\
             Consider fine-tuning if serving >50% NL queries; likely not worth it otherwise.",
            nl_gap
        );
    } else {
        println!(
            "VERDICT ▶ Hybrid gap is {:.0}% on a real codebase.\n\
             BM25 is the dominant signal for this query distribution.\n\
             Fine-tuning is NOT recommended — invest in query understanding instead.",
            nl_gap
        );
    }

    // Regression guard: hybrid must not be worse than BM25 on NL.
    assert!(
        hybrid_nl >= bm25_nl.saturating_sub(1),
        "Hybrid regressed vs BM25 on NL: bm25={} hybrid={}", bm25_nl, hybrid_nl
    );
}

// ---------------------------------------------------------------------------
// Test 4: Real-codebase multi-model comparison
// ---------------------------------------------------------------------------

/// Compare BgeBaseEn, JinaEmbedCode, and (if `qwen3` feature enabled) Qwen3
/// on the `REAL_CODEBASE_CASES` queries using `Strategy::Fast`.
///
/// Prints a full hit matrix across models so you can see which model surfaces
/// each file and where they diverge.
///
/// Run with:
/// ```bash
/// FASTEMBED_CACHE_DIR=~/.cache/fastembed \
///   cargo test --test embedding_eval_test real_codebase_model_comparison \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore]
fn real_codebase_model_comparison() {
    let codixing_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    if !codixing_src.join("Cargo.toml").exists() {
        eprintln!("real_codebase_model_comparison: skipped (repo root not found)");
        return;
    }

    // Models to compare.
    let models: Vec<(&str, EmbeddingModel)> = {
        #[allow(unused_mut)]
        let mut v: Vec<(&str, EmbeddingModel)> = vec![
            ("BgeBaseEn (768d)", EmbeddingModel::BgeBaseEn),
            ("JinaCode  (768d)", EmbeddingModel::JinaEmbedCode),
        ];
        #[cfg(feature = "qwen3")]
        v.push(("Qwen3     (1024d)", EmbeddingModel::Qwen3SmallEmbedding));
        v
    };

    let n_models = models.len();
    let n_cases = REAL_CODEBASE_CASES.len();
    let nl_cases: Vec<usize> = REAL_CODEBASE_CASES
        .iter()
        .enumerate()
        .filter(|(_, c)| c.query_type == "nl")
        .map(|(i, _)| i)
        .collect();
    let id_cases: Vec<usize> = REAL_CODEBASE_CASES
        .iter()
        .enumerate()
        .filter(|(_, c)| c.query_type == "identifier")
        .map(|(i, _)| i)
        .collect();

    // hit_matrix[case_idx][model_idx] = bool
    let mut hit_matrix = vec![vec![false; n_models]; n_cases];
    let mut build_ms = vec![0u128; n_models];
    let mut search_ms = vec![0u128; n_models];

    for (col, (label, model)) in models.iter().enumerate() {
        // Build a fresh temp index for each model.
        let tmp = tempfile::tempdir().expect("tempdir");
        let src_root = codixing_src.join("crates/core/src");
        let dest_root = tmp.path().join("src");
        copy_dir_recursive(&src_root, &dest_root);

        let mut config = IndexConfig::new(tmp.path());
        config.embedding = EmbeddingConfig {
            enabled: true,
            model: model.clone(),
            contextual_embeddings: true,
            quantize: true,
            reranker_enabled: false,
            ..Default::default()
        };

        let t_build = Instant::now();
        print!("\nBuilding index for {} ... ", label);
        let engine = Engine::init(tmp.path(), config).expect("init failed");
        build_ms[col] = t_build.elapsed().as_millis();
        println!("done in {}ms ({} files, {} chunks)",
            build_ms[col], engine.stats().file_count, engine.stats().chunk_count);

        for (row, case) in REAL_CODEBASE_CASES.iter().enumerate() {
            let t0 = Instant::now();
            let results = engine
                .search(
                    SearchQuery::new(case.query)
                        .with_limit(case.k)
                        .with_strategy(Strategy::Fast),
                )
                .unwrap_or_default();
            search_ms[col] += t0.elapsed().as_millis();

            hit_matrix[row][col] = results.iter().any(|r| r.file_path.contains(case.expected_file));
        }
    }

    // --- Print hit matrix ---
    let col_w = 18usize;
    let q_w = 54usize;
    println!();
    println!("{}", "=".repeat(q_w + col_w * n_models + 8));
    println!("Real-codebase model comparison  (Strategy::Fast, {} queries)", n_cases);
    println!("{}", "=".repeat(q_w + col_w * n_models + 8));
    print!("{:<width$} {:>6}", "Query", "type", width = q_w);
    for (label, _) in &models {
        print!("  {:>width$}", label, width = col_w - 2);
    }
    println!();
    println!("{}", "-".repeat(q_w + col_w * n_models + 8));

    for (row, case) in REAL_CODEBASE_CASES.iter().enumerate() {
        let q = if case.query.len() > q_w - 1 { &case.query[..q_w - 1] } else { case.query };
        print!("{:<width$} {:>6}", q, case.query_type, width = q_w);
        for col in 0..n_models {
            print!("  {:>width$}", if hit_matrix[row][col] { "HIT" } else { "miss" }, width = col_w - 2);
        }
        println!();
    }

    println!("{}", "-".repeat(q_w + col_w * n_models + 8));

    // NL recall row
    print!("{:<width$} {:>6}", "NL Recall@k", "nl", width = q_w);
    for col in 0..n_models {
        let hits = nl_cases.iter().filter(|&&r| hit_matrix[r][col]).count();
        print!("  {:>width$}", format!("{:.0}% ({}/{})", hits as f64 / nl_cases.len() as f64 * 100.0, hits, nl_cases.len()), width = col_w - 2);
    }
    println!();

    // Identifier recall row
    print!("{:<width$} {:>6}", "ID Recall@k", "id", width = q_w);
    for col in 0..n_models {
        let hits = id_cases.iter().filter(|&&r| hit_matrix[r][col]).count();
        print!("  {:>width$}", format!("{:.0}% ({}/{})", hits as f64 / id_cases.len() as f64 * 100.0, hits, id_cases.len()), width = col_w - 2);
    }
    println!();

    // Search latency row
    print!("{:<width$} {:>6}", "Avg search latency", "", width = q_w);
    for col in 0..n_models {
        let avg = if n_cases > 0 { search_ms[col] / n_cases as u128 } else { 0 };
        print!("  {:>width$}", format!("{}ms", avg), width = col_w - 2);
    }
    println!();

    println!("{}", "=".repeat(q_w + col_w * n_models + 8));

    // --- Verdict ---
    println!();
    let bge_nl = nl_cases.iter().filter(|&&r| hit_matrix[r][0]).count();
    let jina_nl = nl_cases.iter().filter(|&&r| hit_matrix[r][1]).count();
    let nl_n = nl_cases.len();
    let gap = jina_nl as f64 / nl_n as f64 * 100.0 - bge_nl as f64 / nl_n as f64 * 100.0;

    if gap > 10.0 {
        println!("VERDICT ▶ JinaEmbedCode beats BgeBaseEn by {:.0}% on NL recall over real Rust code.", gap);
        println!("          Switch default model to JinaEmbedCode.");
    } else if gap > 0.0 {
        println!("VERDICT ▶ JinaEmbedCode edges BgeBaseEn by {:.0}% — marginal gain; both are viable.", gap);
    } else if gap == 0.0 {
        println!("VERDICT ▶ JinaEmbedCode and BgeBaseEn tie on NL recall. Stick with BgeBaseEn (default).");
    } else {
        println!("VERDICT ▶ BgeBaseEn outperforms JinaEmbedCode by {:.0}% on this corpus.", -gap);
        println!("          JinaEmbedCode does NOT improve over current default.");
    }
}

// ---------------------------------------------------------------------------
// Test 5: Qwen3 standalone benchmark (separate process — avoids OOM)
// ---------------------------------------------------------------------------

/// Benchmark Qwen3-Embedding-0.6B (1024d, candle backend) on the
/// `REAL_CODEBASE_CASES` queries using `Strategy::Fast`.
///
/// **Run separately from `real_codebase_model_comparison`** — loading all
/// three models in a single process exhausts available memory and triggers
/// an OOM kill.  This test loads only the Qwen3 model.
///
/// Requires: `--features codixing-core/qwen3`
///
/// ```bash
/// FASTEMBED_CACHE_DIR=~/.cache/fastembed \
///   cargo test --test embedding_eval_test qwen3_standalone \
///   --features codixing-core/qwen3 -- --ignored --nocapture
/// ```
#[test]
#[ignore]
#[cfg(feature = "qwen3")]
fn qwen3_standalone() {
    let codixing_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();

    if !codixing_src.join("Cargo.toml").exists() {
        eprintln!("qwen3_standalone: skipped (repo root not found)");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let src_root = codixing_src.join("crates/core/src");
    let dest_root = tmp.path().join("src");
    copy_dir_recursive(&src_root, &dest_root);

    let mut config = IndexConfig::new(tmp.path());
    config.embedding = EmbeddingConfig {
        enabled: true,
        model: EmbeddingModel::Qwen3SmallEmbedding,
        contextual_embeddings: true,
        quantize: true,
        reranker_enabled: false,
        ..Default::default()
    };

    println!("\nBuilding Qwen3 index (1024d, candle backend) ...");
    let t_build = Instant::now();
    let engine = Engine::init(tmp.path(), config).expect("init failed");
    println!(
        "Index built in {}ms  ({} files, {} chunks)",
        t_build.elapsed().as_millis(),
        engine.stats().file_count,
        engine.stats().chunk_count,
    );

    let nl_cases: Vec<&EvalCase> = REAL_CODEBASE_CASES.iter().filter(|c| c.query_type == "nl").collect();
    let id_cases: Vec<&EvalCase> = REAL_CODEBASE_CASES.iter().filter(|c| c.query_type == "identifier").collect();
    let total = REAL_CODEBASE_CASES.len();

    let mut nl_hits = 0usize;
    let mut id_hits = 0usize;
    let mut total_ms = 0u128;

    println!();
    println!("{:<55} {:>6}  {}", "Query", "Hit?", "Type");
    println!("{}", "-".repeat(70));

    for case in REAL_CODEBASE_CASES {
        let t0 = Instant::now();
        let results = engine
            .search(
                SearchQuery::new(case.query)
                    .with_limit(case.k)
                    .with_strategy(Strategy::Fast),
            )
            .unwrap_or_default();
        total_ms += t0.elapsed().as_millis();

        let hit = results.iter().any(|r| r.file_path.contains(case.expected_file));
        match case.query_type {
            "nl"         => { if hit { nl_hits += 1; } }
            "identifier" => { if hit { id_hits += 1; } }
            _            => {}
        }

        let q = if case.query.len() > 53 { &case.query[..53] } else { case.query };
        println!("{:<55} {:>6}  {}", q, if hit { "HIT" } else { "miss" }, case.query_type);
    }

    println!("{}", "-".repeat(70));

    let nl_n  = nl_cases.len();
    let id_n  = id_cases.len();
    let nl_pct = nl_hits as f64 / nl_n as f64 * 100.0;
    let id_pct = id_hits as f64 / id_n as f64 * 100.0;
    let avg_ms = if total > 0 { total_ms / total as u128 } else { 0 };

    println!();
    println!("=== Qwen3 RESULTS (real codebase, {} files) ===", engine.stats().file_count);
    println!("NL Recall@k:    {:.0}% ({}/{})", nl_pct, nl_hits, nl_n);
    println!("ID Recall@k:    {:.0}% ({}/{})", id_pct, id_hits, id_n);
    println!("Avg latency:    {}ms", avg_ms);

    println!();
    // Compare against known BgeBaseEn baseline (100% NL, 100% ID).
    if nl_pct >= 100.0 {
        println!("VERDICT ▶ Qwen3 matches BgeBaseEn on NL recall (100%). Viable alternative.");
    } else {
        let gap = 100.0 - nl_pct;
        println!(
            "VERDICT ▶ Qwen3 trails BgeBaseEn by {:.0}% on NL recall ({:.0}% vs 100%).\n\
             The 1.2 GB candle-backend model does NOT improve over the current default.",
            gap, nl_pct
        );
    }
}

/// Recursively copy a directory tree.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path);
        } else {
            std::fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}

fn count_rs_files(root: &std::path::Path) -> usize {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() { n += count_rs_files(&p); }
            else if p.extension().map(|x| x == "rs").unwrap_or(false) { n += 1; }
        }
    }
    n
}
