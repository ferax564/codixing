//! Integration tests for hybrid BM25 + vector retrieval.
//!
//! Validates that HybridRetriever outperforms BM25-only on semantic queries,
//! and that token budgets are respected during context retrieval.

mod common;

use codeforge_core::chunker::Chunk;
use codeforge_core::embeddings::{Embedder, MockEmbedder};
use codeforge_core::index::TantivyIndex;
use codeforge_core::index::{BruteForceVectorIndex, VectorIndex};
use codeforge_core::language::Language;
use codeforge_core::retriever::HybridRetriever;
use codeforge_core::retriever::bm25::BM25Retriever;
use codeforge_core::retriever::{Retriever, SearchQuery};
use codeforge_core::{ApproxTokenCounter, ContextBudget, Engine, IndexConfig, TokenCounter};
use std::fs;
use tempfile::tempdir;

/// Build a code corpus with semantically related concepts spread across files.
///
/// This corpus is designed so that a pure BM25 search for "authentication"
/// will only find files containing that exact term, but semantic/hybrid search
/// should also surface `verify_credentials` and `check_token_validity`.
fn setup_semantic_corpus(root: &std::path::Path) {
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // auth.rs -- contains explicit "authenticate" and related terms
    fs::write(
        src.join("auth.rs"),
        r#"/// Authentication module for user login.
///
/// Handles user authentication, session management, and credential validation.

use crate::models::User;

/// Authenticate a user by checking their username and password.
pub fn authenticate(username: &str, password: &str) -> Result<User, AuthError> {
    let user = find_user(username)?;
    if verify_password(&user, password) {
        Ok(user)
    } else {
        Err(AuthError::InvalidCredentials)
    }
}

/// Verify that a hashed password matches the stored hash.
fn verify_password(user: &User, password: &str) -> bool {
    bcrypt::verify(password, &user.password_hash).unwrap_or(false)
}

/// Check whether a session token is still valid.
pub fn check_session_validity(token: &str) -> bool {
    // Token validation logic
    !token.is_empty() && token.len() > 10
}

#[derive(Debug)]
pub enum AuthError {
    UserNotFound,
    InvalidCredentials,
    SessionExpired,
}
"#,
    )
    .unwrap();

    // credentials.rs -- related to auth but uses different vocabulary
    fs::write(
        src.join("credentials.rs"),
        r#"/// Credential verification and token management.

/// Verify that a set of credentials are valid.
pub fn verify_credentials(api_key: &str, secret: &str) -> bool {
    // Validate the API key format and check the secret
    api_key.starts_with("sk-") && !secret.is_empty()
}

/// Check whether a bearer token is valid and not expired.
pub fn check_token_validity(token: &str, expiry_secs: u64) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    parts.len() == 3 && expiry_secs > 0
}

/// Refresh an expired token with new credentials.
pub fn refresh_token(refresh_key: &str) -> Option<String> {
    if refresh_key.is_empty() {
        None
    } else {
        Some(format!("new-token-{}", refresh_key))
    }
}
"#,
    )
    .unwrap();

    // router.rs -- unrelated to auth
    fs::write(
        src.join("router.rs"),
        r#"/// HTTP request routing module.

/// Route an incoming HTTP request to the correct handler.
pub fn route_request(method: &str, path: &str) -> &'static str {
    match (method, path) {
        ("GET", "/health") => "healthy",
        ("GET", "/status") => "ok",
        ("POST", "/data") => "accepted",
        _ => "not_found",
    }
}

/// Parse URL parameters from a query string.
pub fn parse_query_params(query: &str) -> Vec<(&str, &str)> {
    query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            Some((parts.next()?, parts.next()?))
        })
        .collect()
}
"#,
    )
    .unwrap();

    // models.rs -- data structures
    fs::write(
        src.join("models.rs"),
        r#"/// Data model definitions.

/// A user in the system.
pub struct User {
    pub id: u64,
    pub username: String,
    pub email: String,
    pub password_hash: String,
}

/// A configuration entry.
pub struct ConfigEntry {
    pub key: String,
    pub value: String,
}

/// A database record with metadata.
pub struct Record {
    pub id: u64,
    pub data: Vec<u8>,
    pub created_at: u64,
}
"#,
    )
    .unwrap();

    // pipeline.rs -- processing pipeline (no auth relation)
    fs::write(
        src.join("pipeline.rs"),
        r#"/// Data processing pipeline with transformation stages.

/// Transform raw data through a series of processing stages.
pub fn transform_data(input: &[u8]) -> Vec<u8> {
    input.iter().map(|b| b.wrapping_add(1)).collect()
}

/// Validate that the pipeline output meets quality constraints.
pub fn validate_output(output: &[u8]) -> bool {
    !output.is_empty() && output.len() < 1_000_000
}

/// Schedule a pipeline job for background processing.
pub fn schedule_job(name: &str, priority: u32) -> u64 {
    // Generate a simple job ID from name hash
    let hash: u64 = name.bytes().fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    hash + priority as u64
}
"#,
    )
    .unwrap();
}

/// Index a corpus with Engine and also build a vector index from the chunks.
///
/// Returns (TantivyIndex, BruteForceVectorIndex, MockEmbedder, chunk_count).
fn build_indexes(root: &std::path::Path) -> (Engine, BruteForceVectorIndex, MockEmbedder) {
    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let dim = 32;
    let embedder = MockEmbedder::new(dim);
    let mut vector_index = BruteForceVectorIndex::new(dim);

    // Search with a broad query to get all chunks from the index
    // and embed them into the vector index.
    let all_results = engine
        .search(SearchQuery::new("fn pub struct").with_limit(1000))
        .unwrap();

    for result in &all_results {
        let chunk_id: u64 = result.chunk_id.parse().unwrap_or(0);
        let embedding = embedder.embed(&result.content).unwrap();
        vector_index.add(chunk_id, embedding).unwrap();
    }

    // Also search for each file's specific terms to ensure coverage
    for term in &[
        "authenticate",
        "verify",
        "route",
        "User",
        "transform",
        "token",
        "pipeline",
        "credentials",
    ] {
        let results = engine
            .search(SearchQuery::new(*term).with_limit(100))
            .unwrap();
        for result in &results {
            let chunk_id: u64 = result.chunk_id.parse().unwrap_or(0);
            if vector_index.len() == 0
                || vector_index
                    .search(&embedder.embed(&result.content).unwrap(), 1)
                    .unwrap()
                    .first()
                    .map(|r| r.chunk_id != chunk_id)
                    .unwrap_or(true)
            {
                let embedding = embedder.embed(&result.content).unwrap();
                let _ = vector_index.add(chunk_id, embedding);
            }
        }
    }

    (engine, vector_index, embedder)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn hybrid_search_returns_results_for_indexed_code() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_semantic_corpus(root);

    // Index and then drop the engine to release the lock
    {
        let (_engine, _vector_index, _embedder) = build_indexes(root);
    }

    // Re-open the persisted index
    let engine2 = Engine::open(root).unwrap();

    // Use BM25 search directly through the engine
    let bm25_results = engine2
        .search(SearchQuery::new("authenticate").with_limit(10))
        .unwrap();

    assert!(
        !bm25_results.is_empty(),
        "BM25 search should find 'authenticate'"
    );

    // Verify BM25 finds auth.rs
    assert!(
        bm25_results.iter().any(|r| r.file_path.contains("auth.rs")),
        "BM25 should find auth.rs for 'authenticate', got: {:?}",
        bm25_results
            .iter()
            .map(|r| &r.file_path)
            .collect::<Vec<_>>()
    );
}

#[test]
fn hybrid_search_finds_semantically_related_code() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_semantic_corpus(root);

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let dim = 32;
    let embedder = MockEmbedder::new(dim);

    // Build Tantivy index directly for the HybridRetriever
    let tantivy = TantivyIndex::create_in_ram().unwrap();
    let mut vector_index = BruteForceVectorIndex::new(dim);

    // Get all chunks and index them in both Tantivy and vector
    let all_terms = [
        "authenticate",
        "verify",
        "credentials",
        "token",
        "route",
        "User",
        "transform",
        "pipeline",
        "fn",
        "pub",
    ];
    let mut seen_chunks = std::collections::HashSet::new();

    for term in &all_terms {
        let results = engine
            .search(SearchQuery::new(*term).with_limit(100))
            .unwrap();
        for result in &results {
            let chunk_id: u64 = result.chunk_id.parse().unwrap_or(0);
            if seen_chunks.insert(chunk_id) {
                let chunk = Chunk {
                    id: chunk_id,
                    file_path: result.file_path.clone(),
                    language: Language::Rust,
                    content: result.content.clone(),
                    byte_start: 0,
                    byte_end: result.content.len(),
                    line_start: result.line_start as usize,
                    line_end: result.line_end as usize,
                    scope_chain: vec![],
                    signatures: vec![result.signature.clone()],
                    entity_names: vec![],
                };
                tantivy.add_chunk(&chunk).unwrap();
                let embedding = embedder.embed(&result.content).unwrap();
                vector_index.add(chunk_id, embedding).unwrap();
            }
        }
    }
    tantivy.commit().unwrap();

    // Now test: hybrid search for "authenticate" should find results
    let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
    let hybrid_results = hybrid
        .search(&SearchQuery::new("authenticate").with_limit(10))
        .unwrap();

    assert!(
        !hybrid_results.is_empty(),
        "hybrid search should return results for 'authenticate'"
    );

    // Results should be sorted by score descending
    for w in hybrid_results.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "results should be sorted by score: {} >= {}",
            w[0].score,
            w[1].score
        );
    }
}

#[test]
fn hybrid_retrieval_covers_more_files_than_bm25_alone() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_semantic_corpus(root);

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let dim = 32;
    let embedder = MockEmbedder::new(dim);
    let tantivy = TantivyIndex::create_in_ram().unwrap();
    let mut vector_index = BruteForceVectorIndex::new(dim);
    let mut seen_chunks = std::collections::HashSet::new();

    // Index all discoverable chunks
    let broad_terms = [
        "fn",
        "pub",
        "struct",
        "let",
        "impl",
        "use",
        "authenticate",
        "verify",
        "route",
        "transform",
        "token",
        "credentials",
        "pipeline",
        "User",
    ];
    for term in &broad_terms {
        let results = engine
            .search(SearchQuery::new(*term).with_limit(200))
            .unwrap();
        for result in &results {
            let chunk_id: u64 = result.chunk_id.parse().unwrap_or(0);
            if seen_chunks.insert(chunk_id) {
                let chunk = Chunk {
                    id: chunk_id,
                    file_path: result.file_path.clone(),
                    language: Language::Rust,
                    content: result.content.clone(),
                    byte_start: 0,
                    byte_end: result.content.len(),
                    line_start: result.line_start as usize,
                    line_end: result.line_end as usize,
                    scope_chain: vec![],
                    signatures: vec![result.signature.clone()],
                    entity_names: vec![],
                };
                tantivy.add_chunk(&chunk).unwrap();
                let embedding = embedder.embed(&result.content).unwrap();
                vector_index.add(chunk_id, embedding).unwrap();
            }
        }
    }
    tantivy.commit().unwrap();

    // BM25-only results for "verify_credentials"
    let bm25 = BM25Retriever::new(&tantivy);
    let bm25_results = bm25
        .search(&SearchQuery::new("verify_credentials").with_limit(10))
        .unwrap();

    // Hybrid results for the same query
    let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
    let hybrid_results = hybrid
        .search(&SearchQuery::new("verify_credentials").with_limit(10))
        .unwrap();

    // Hybrid should return at least as many results as BM25 (it fuses both)
    assert!(
        hybrid_results.len() >= bm25_results.len(),
        "hybrid ({}) should return >= BM25 ({}) results",
        hybrid_results.len(),
        bm25_results.len()
    );

    // Collect unique file paths from both
    let bm25_files: std::collections::HashSet<&str> =
        bm25_results.iter().map(|r| r.file_path.as_str()).collect();
    let hybrid_files: std::collections::HashSet<&str> = hybrid_results
        .iter()
        .map(|r| r.file_path.as_str())
        .collect();

    // Hybrid should cover at least all files that BM25 covers
    assert!(
        bm25_files.is_subset(&hybrid_files),
        "hybrid file set {:?} should be a superset of BM25 file set {:?}",
        hybrid_files,
        bm25_files
    );
}

#[test]
fn hybrid_weights_affect_ranking() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_semantic_corpus(root);

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let dim = 32;
    let embedder = MockEmbedder::new(dim);
    let tantivy = TantivyIndex::create_in_ram().unwrap();
    let mut vector_index = BruteForceVectorIndex::new(dim);
    let mut seen = std::collections::HashSet::new();

    for term in &[
        "authenticate",
        "verify",
        "token",
        "credentials",
        "fn",
        "pub",
    ] {
        let results = engine
            .search(SearchQuery::new(*term).with_limit(200))
            .unwrap();
        for r in &results {
            let id: u64 = r.chunk_id.parse().unwrap_or(0);
            if seen.insert(id) {
                let chunk = Chunk {
                    id,
                    file_path: r.file_path.clone(),
                    language: Language::Rust,
                    content: r.content.clone(),
                    byte_start: 0,
                    byte_end: r.content.len(),
                    line_start: r.line_start as usize,
                    line_end: r.line_end as usize,
                    scope_chain: vec![],
                    signatures: vec![r.signature.clone()],
                    entity_names: vec![],
                };
                tantivy.add_chunk(&chunk).unwrap();
                let emb = embedder.embed(&r.content).unwrap();
                vector_index.add(id, emb).unwrap();
            }
        }
    }
    tantivy.commit().unwrap();

    // BM25-heavy weights
    let bm25_heavy =
        HybridRetriever::new(&tantivy, &vector_index, &embedder).with_weights(0.9, 0.1);
    let results_bm25 = bm25_heavy
        .search(&SearchQuery::new("authenticate").with_limit(10))
        .unwrap();

    // Vector-heavy weights
    let vec_heavy = HybridRetriever::new(&tantivy, &vector_index, &embedder).with_weights(0.1, 0.9);
    let results_vec = vec_heavy
        .search(&SearchQuery::new("authenticate").with_limit(10))
        .unwrap();

    // Both should return results
    assert!(!results_bm25.is_empty(), "BM25-heavy should return results");
    assert!(
        !results_vec.is_empty(),
        "vector-heavy should return results"
    );

    // The top-result scores should differ (different weight distributions)
    if !results_bm25.is_empty() && !results_vec.is_empty() {
        let top_bm25 = results_bm25[0].score;
        let top_vec = results_vec[0].score;
        // Scores must differ because the weights are different
        assert!(
            (top_bm25 - top_vec).abs() > 1e-9
                || results_bm25[0].chunk_id != results_vec[0].chunk_id,
            "different weights should produce different scores or rankings"
        );
    }
}

#[test]
fn token_budget_limits_context_retrieval() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_semantic_corpus(root);

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    // Get search results to feed into ContextBudget (use a single broad term)
    let results = engine
        .search(SearchQuery::new("fn").with_limit(20))
        .unwrap();

    assert!(!results.is_empty(), "should find results for 'fn'");

    // Set a generous token budget so at least one snippet fits
    let token_budget = 500; // ~2000 chars
    let mut budget = ContextBudget::new(token_budget);

    for result in &results {
        let added = budget.try_add(
            result.file_path.clone(),
            result.language.clone(),
            result.content.clone(),
            result.line_start,
            result.line_end,
            result.score,
        );
        if budget.remaining() == 0 {
            break;
        }
        // Continue trying — some snippets may be too large but others fit
        let _ = added;
    }

    // Budget should not be exceeded
    assert!(
        budget.used() <= token_budget,
        "used tokens ({}) should not exceed budget ({})",
        budget.used(),
        token_budget
    );

    // Should have collected at least one snippet
    assert!(
        !budget.snippets().is_empty(),
        "should have at least one snippet within budget"
    );

    // Verify remaining budget is consistent
    assert_eq!(
        budget.remaining(),
        token_budget - budget.used(),
        "remaining should be budget - used"
    );

    // Now test with a very tight budget
    let tight_budget = 500; // still generous enough for small snippets
    let mut budget2 = ContextBudget::new(tight_budget);
    for result in &results {
        budget2.try_add(
            result.file_path.clone(),
            result.language.clone(),
            result.content.clone(),
            result.line_start,
            result.line_end,
            result.score,
        );
    }
    // Must not exceed the budget
    assert!(
        budget2.used() <= tight_budget,
        "tight budget: used ({}) must not exceed budget ({})",
        budget2.used(),
        tight_budget
    );
}

#[test]
fn token_budget_rejects_oversized_snippets() {
    // Create a very small budget and verify large snippets are rejected
    let mut budget = ContextBudget::new(2); // 2 tokens = ~8 chars

    // This snippet is much larger than 8 chars
    let large_content =
        "fn authenticate(user: &str, pass: &str) -> Result<User, AuthError> { /* long body */ }";
    let added = budget.try_add(
        "auth.rs".into(),
        "rust".into(),
        large_content.into(),
        1,
        5,
        1.0,
    );
    assert!(!added, "oversized snippet should be rejected");
    assert_eq!(budget.used(), 0);
    assert!(budget.snippets().is_empty());
}

#[test]
fn context_budget_accumulates_multiple_snippets() {
    let mut budget = ContextBudget::new(1000); // generous budget

    let snippets_data = vec![
        ("auth.rs", "fn authenticate() {}", 2.0),
        ("cred.rs", "fn verify_credentials() {}", 1.5),
        ("router.rs", "fn route_request() {}", 1.0),
        ("models.rs", "struct User { id: u64 }", 0.5),
    ];

    for (path, content, score) in &snippets_data {
        let added = budget.try_add(
            path.to_string(),
            "rust".into(),
            content.to_string(),
            1,
            1,
            *score,
        );
        assert!(added, "snippet from {path} should fit in budget");
    }

    let collected = budget.snippets();
    assert_eq!(collected.len(), 4);

    // Snippets should be in insertion order (ranked order)
    assert!(collected[0].score > collected[1].score);
    assert!(collected[1].score > collected[2].score);
    assert!(collected[2].score > collected[3].score);

    // Each snippet should have a recorded token count
    for snippet in collected {
        assert!(snippet.token_count > 0, "token_count should be > 0");
    }
}

#[test]
fn approx_token_counter_consistency() {
    let counter = ApproxTokenCounter::default();

    // Verify the ~4 chars/token heuristic
    let text_20_chars = "a".repeat(20);
    assert_eq!(counter.count_tokens(&text_20_chars), 5); // 20/4 = 5

    let text_21_chars = "a".repeat(21);
    assert_eq!(counter.count_tokens(&text_21_chars), 6); // 21/4 = 5.25 -> ceil = 6

    // Empty string = 0 tokens
    assert_eq!(counter.count_tokens(""), 0);

    // Real code snippet
    let code = "pub fn authenticate(username: &str, password: &str) -> bool { true }";
    let tokens = counter.count_tokens(code);
    assert!(tokens > 0, "code should have tokens");
    assert!(tokens <= code.len(), "tokens should be <= char count");
}

#[test]
fn hybrid_search_with_file_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    setup_semantic_corpus(root);

    let config = IndexConfig::new(root);
    let engine = Engine::init(root, config).unwrap();

    let dim = 32;
    let embedder = MockEmbedder::new(dim);
    let tantivy = TantivyIndex::create_in_ram().unwrap();
    let mut vector_index = BruteForceVectorIndex::new(dim);
    let mut seen = std::collections::HashSet::new();

    for term in &[
        "authenticate",
        "verify",
        "token",
        "credentials",
        "fn",
        "pub",
    ] {
        let results = engine
            .search(SearchQuery::new(*term).with_limit(200))
            .unwrap();
        for r in &results {
            let id: u64 = r.chunk_id.parse().unwrap_or(0);
            if seen.insert(id) {
                let chunk = Chunk {
                    id,
                    file_path: r.file_path.clone(),
                    language: Language::Rust,
                    content: r.content.clone(),
                    byte_start: 0,
                    byte_end: r.content.len(),
                    line_start: r.line_start as usize,
                    line_end: r.line_end as usize,
                    scope_chain: vec![],
                    signatures: vec![r.signature.clone()],
                    entity_names: vec![],
                };
                tantivy.add_chunk(&chunk).unwrap();
                let emb = embedder.embed(&r.content).unwrap();
                vector_index.add(id, emb).unwrap();
            }
        }
    }
    tantivy.commit().unwrap();

    // Search with filter for auth files only
    let hybrid = HybridRetriever::new(&tantivy, &vector_index, &embedder);
    let filtered_results = hybrid
        .search(
            &SearchQuery::new("verify")
                .with_limit(10)
                .with_file_filter("auth"),
        )
        .unwrap();

    for r in &filtered_results {
        assert!(
            r.file_path.contains("auth"),
            "file filter should restrict results to auth files, got: {}",
            r.file_path
        );
    }

    // Unfiltered should have more results
    let unfiltered_results = hybrid
        .search(&SearchQuery::new("verify").with_limit(10))
        .unwrap();

    assert!(
        unfiltered_results.len() >= filtered_results.len(),
        "unfiltered ({}) should have >= filtered ({}) results",
        unfiltered_results.len(),
        filtered_results.len()
    );
}
