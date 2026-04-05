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

use codixing_core::config::EmbeddingConfig;
use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};

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
            queries: None,
            doc_filter: None,
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
            queries: None,
            doc_filter: None,
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
            queries: None,
            doc_filter: None,
        })
        .unwrap_or_default();

    let top = results.first().map(|r| r.file_path.as_str()).unwrap_or("");
    assert_eq!(
        top, "src/tokenizer.rs",
        "signature field boost should rank tokenizer.rs first; got {top}"
    );
}

// ---------------------------------------------------------------------------
// Tier 2 language retrieval quality
// ---------------------------------------------------------------------------

/// Build a BM25-only index with representative Tier 2 language files.
fn build_tier2_index() -> (Engine, TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();

    let files: &[(&str, &str)] = &[
        (
            "lib/authenticator.rb",
            r#"
# Validates JWT tokens and manages user sessions.
class Authenticator
  def initialize(secret, session_ttl)
    @secret = secret
    @session_ttl = session_ttl
  end

  # Validate a JWT token and return the subject claim.
  def validate_token(token)
    JWT.decode(token, @secret)
  end

  # Create a signed JWT for the given subject.
  def create_token(subject)
    JWT.encode({ sub: subject, exp: Time.now.to_i + @session_ttl }, @secret)
  end
end
"#,
        ),
        (
            "lib/cache.rb",
            r#"
# Thread-safe LRU cache with configurable capacity.
class LruCache
  def initialize(capacity)
    @capacity = capacity
    @store = {}
    @mutex = Mutex.new
  end

  def get(key)
    @mutex.synchronize { @store[key] }
  end

  def insert(key, value)
    @mutex.synchronize do
      @store.delete(key) if @store.size >= @capacity
      @store[key] = value
    end
  end
end
"#,
        ),
        (
            "Sources/Renderer.swift",
            r#"
import Foundation

/// Renders scene objects to a Metal framebuffer.
protocol Drawable {
    func draw(in context: RenderContext)
}

/// 2D sprite with position and texture atlas.
struct Sprite: Drawable {
    var position: CGPoint
    var texture: String

    func draw(in context: RenderContext) {
        context.blit(texture, at: position)
    }
}

/// Metal-backed render context.
class RenderContext {
    func blit(_ texture: String, at point: CGPoint) {}
}
"#,
        ),
        (
            "Sources/EventBus.swift",
            r#"
/// Publish-subscribe event bus for decoupled component communication.
class EventBus<T> {
    private var subscribers: [(T) -> Void] = []

    /// Subscribe to events of type T.
    func subscribe(_ handler: @escaping (T) -> Void) {
        subscribers.append(handler)
    }

    /// Publish an event to all subscribers.
    func publish(_ event: T) {
        subscribers.forEach { $0(event) }
    }
}
"#,
        ),
        (
            "src/main/kotlin/Repository.kt",
            r#"
/**
 * Generic repository interface for CRUD operations on domain entities.
 */
interface Repository<T, ID> {
    fun findById(id: ID): T?
    fun save(entity: T): T
    fun delete(id: ID)
    fun findAll(): List<T>
}

/**
 * In-memory implementation backed by a HashMap.
 */
class InMemoryRepository<T, ID> : Repository<T, ID> {
    private val store = HashMap<ID, T>()

    override fun findById(id: ID): T? = store[id]
    override fun save(entity: T): T { store[entity.hashCode() as ID] = entity; return entity }
    override fun delete(id: ID) { store.remove(id) }
    override fun findAll(): List<T> = store.values.toList()
}
"#,
        ),
        (
            "src/main/kotlin/Logger.kt",
            r#"
/** Structured logger with configurable log levels. */
enum class LogLevel { DEBUG, INFO, WARN, ERROR }

class StructuredLogger(private val level: LogLevel) {
    fun info(msg: String) = log(LogLevel.INFO, msg)
    fun warn(msg: String) = log(LogLevel.WARN, msg)
    fun error(msg: String) = log(LogLevel.ERROR, msg)

    private fun log(l: LogLevel, msg: String) {
        if (l >= level) println("[$l] $msg")
    }
}
"#,
        ),
        (
            "src/main/scala/MetricsCollector.scala",
            r#"
/**
 * Collects named counters and histograms for observability.
 */
class MetricsCollector {
  private val counters  = scala.collection.mutable.Map[String, Long]()
  private val histograms = scala.collection.mutable.Map[String, List[Double]]()

  /** Increment a counter by delta. */
  def increment(name: String, delta: Long = 1): Unit =
    counters(name) = counters.getOrElse(name, 0L) + delta

  /** Record an observation into a histogram. */
  def observe(name: String, value: Double): Unit =
    histograms(name) = value :: histograms.getOrElse(name, Nil)
}
"#,
        ),
        (
            "src/main/scala/EventSourcing.scala",
            r#"
/** Append-only event store for event-sourced aggregates. */
trait EventStore[E] {
  def append(aggregateId: String, events: Seq[E]): Unit
  def load(aggregateId: String): Seq[E]
}

/** In-memory event store implementation. */
class InMemoryEventStore[E] extends EventStore[E] {
  private val log = scala.collection.mutable.Map[String, List[E]]()

  def append(id: String, events: Seq[E]): Unit =
    log(id) = log.getOrElse(id, Nil) ++ events

  def load(id: String): Seq[E] = log.getOrElse(id, Nil)
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
    config.embedding = EmbeddingConfig {
        enabled: false,
        ..Default::default()
    };

    let engine = Engine::init(root, config).expect("tier2 index build failed");
    (engine, tmp)
}

// Ruby -----------------------------------------------------------------------

#[test]
fn recall_ruby_jwt_authenticator() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(
        &engine,
        "JWT token validate session",
        "lib/authenticator.rb",
        3,
    );
}

#[test]
fn recall_ruby_lru_cache_mutex() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(&engine, "LRU cache thread-safe capacity", "lib/cache.rb", 3);
}

// Swift ----------------------------------------------------------------------

#[test]
fn recall_swift_drawable_sprite() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(
        &engine,
        "Drawable protocol sprite texture",
        "Sources/Renderer.swift",
        3,
    );
}

#[test]
fn recall_swift_event_bus_publish_subscribe() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(
        &engine,
        "publish subscribe event handler",
        "Sources/EventBus.swift",
        3,
    );
}

// Kotlin ---------------------------------------------------------------------

#[test]
fn recall_kotlin_repository_interface() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(
        &engine,
        "Repository CRUD findById save",
        "src/main/kotlin/Repository.kt",
        3,
    );
}

#[test]
fn recall_kotlin_structured_logger() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(
        &engine,
        "StructuredLogger log level warn error",
        "src/main/kotlin/Logger.kt",
        3,
    );
}

// Scala ----------------------------------------------------------------------

#[test]
fn recall_scala_metrics_counter_histogram() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(
        &engine,
        "MetricsCollector counter histogram increment",
        "src/main/scala/MetricsCollector.scala",
        3,
    );
}

#[test]
fn recall_scala_event_sourcing_store() {
    let (engine, _tmp) = build_tier2_index();
    assert_recall(
        &engine,
        "EventStore append load aggregate",
        "src/main/scala/EventSourcing.scala",
        3,
    );
}
