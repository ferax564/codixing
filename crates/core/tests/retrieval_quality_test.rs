//! Retrieval quality tests — Recall@k assertions on a synthetic codebase.
//!
//! The tests build a small but realistic in-memory index, run named queries,
//! and assert that the expected file appears in the top-k results.  These act
//! as a regression harness: a failing test means a retrieval regression was
//! introduced.
//!
//! Design principles:
//! - No network access (BM25-only, no embedder required).
//! - Deterministic: same files, same queries, same expected results.
//! - Fast: total test time << 1 s.

use std::collections::HashSet;

use tempfile::TempDir;

use codeforge_core::config::EmbeddingConfig;
use codeforge_core::{Engine, IndexConfig, SearchQuery, Strategy};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a tiny BM25-only index over a synthetic multi-file project.
///
/// Returns `(engine, tmp_dir)` — `tmp_dir` must stay alive for the duration
/// of the test.
fn build_test_index() -> (Engine, TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    // --- synthetic source files ---

    let files: &[(&str, &str)] = &[
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

    for (rel_path, content) in files {
        let abs = root.join(rel_path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
    }

    let mut config = IndexConfig::new(root);
    // BM25-only — no embedding model needed.
    config.embedding = EmbeddingConfig {
        enabled: false,
        ..Default::default()
    };

    let engine = Engine::init(root, config).expect("index build failed");
    (engine, tmp)
}

/// Assert that `expected_file` appears in the top `k` results for `query`.
fn assert_recall(engine: &Engine, query: &str, expected_file: &str, k: usize) {
    let results = engine
        .search(SearchQuery {
            query: query.to_string(),
            limit: k,
            file_filter: None,
            strategy: Strategy::Instant,
            token_budget: None,
        })
        .unwrap_or_default();

    let found_files: HashSet<&str> = results.iter().map(|r| r.file_path.as_str()).collect();
    assert!(
        found_files.contains(expected_file),
        "Recall@{k} FAIL: query={query:?} expected={expected_file} got={found_files:?}"
    );
}

// ---------------------------------------------------------------------------
// Recall@k tests
// ---------------------------------------------------------------------------

#[test]
fn recall_parser_struct() {
    let (engine, _tmp) = build_test_index();
    // Exact struct name.
    assert_recall(&engine, "Parser", "src/parser.rs", 3);
}

#[test]
fn recall_engine_search_method() {
    let (engine, _tmp) = build_test_index();
    assert_recall(&engine, "Engine search", "src/engine.rs", 5);
}

#[test]
fn recall_retriever_trait() {
    let (engine, _tmp) = build_test_index();
    assert_recall(&engine, "Retriever trait", "src/retriever.rs", 5);
}

#[test]
fn recall_tokenizer_camel_case() {
    let (engine, _tmp) = build_test_index();
    assert_recall(&engine, "camelCase tokenizer split", "src/tokenizer.rs", 5);
}

#[test]
fn recall_vector_index_hnsw() {
    let (engine, _tmp) = build_test_index();
    assert_recall(&engine, "HNSW nearest neighbour", "src/vector.rs", 5);
}

#[test]
fn recall_config_serialization() {
    let (engine, _tmp) = build_test_index();
    assert_recall(&engine, "IndexConfig serialize", "src/config.rs", 5);
}

#[test]
fn recall_bm25_retriever() {
    let (engine, _tmp) = build_test_index();
    assert_recall(&engine, "BM25Retriever Tantivy", "src/retriever.rs", 3);
}

#[test]
fn recall_parse_method_docstring() {
    let (engine, _tmp) = build_test_index();
    // Query based on doc comment, not identifier.
    assert_recall(&engine, "parse source buffer root node", "src/parser.rs", 5);
}

// ---------------------------------------------------------------------------
// Precision guard: irrelevant files should NOT dominate top-1
// ---------------------------------------------------------------------------

#[test]
fn top1_parser_is_parser_file() {
    let (engine, _tmp) = build_test_index();
    let results = engine
        .search(SearchQuery {
            query: "Parser new language".to_string(),
            limit: 5,
            file_filter: None,
            strategy: Strategy::Instant,
            token_budget: None,
        })
        .unwrap_or_default();

    let top = results.first().map(|r| r.file_path.as_str()).unwrap_or("");
    assert_eq!(
        top, "src/parser.rs",
        "top-1 for 'Parser new' should be parser.rs, got {top}"
    );
}

#[test]
fn field_boost_ranks_signature_match_high() {
    let (engine, _tmp) = build_test_index();
    // Query exactly matches the function signature in tokenizer.rs.
    let results = engine
        .search(SearchQuery {
            query: "split_camel_case".to_string(),
            limit: 5,
            file_filter: None,
            strategy: Strategy::Instant,
            token_budget: None,
        })
        .unwrap_or_default();

    let top = results.first().map(|r| r.file_path.as_str()).unwrap_or("");
    assert_eq!(
        top, "src/tokenizer.rs",
        "signature field boost should rank tokenizer.rs first; got {top}"
    );
}
