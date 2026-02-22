//! Integration tests for ONNX Runtime embedding inference.
//!
//! These tests require the `vector` feature and downloaded model files
//! (all-MiniLM-L6-v2). If model files are not present, tests print a
//! SKIP message and return successfully.
//!
//! To download model files:
//!   bash models/minilm/download.sh
//!
//! To run:
//!   cargo test --features vector -- onnx

#![cfg(feature = "vector")]

use codeforge_core::embeddings::{Embedder, OnnxEmbedder};

/// Resolve the model directory from `CODEFORGE_MODEL_DIR` env var or the
/// default `models/minilm` relative path (which works when running from
/// the codeforge workspace root).
fn model_dir() -> std::path::PathBuf {
    std::env::var("CODEFORGE_MODEL_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("models/minilm"))
}

/// Helper: load the ONNX embedder if model files are present.
/// Returns `None` (with a SKIP message) if files are missing.
fn load_embedder() -> Option<OnnxEmbedder> {
    let dir = model_dir();
    if !dir.join("model.onnx").exists() {
        eprintln!(
            "SKIP: model files not found at {}. Run `bash models/minilm/download.sh` first.",
            dir.display()
        );
        return None;
    }
    Some(OnnxEmbedder::load(&dir).expect("Failed to load ONNX model"))
}

#[test]
fn test_onnx_embed_single_text() {
    let Some(embedder) = load_embedder() else {
        return;
    };

    let embedding = embedder
        .embed("fn main() { println!(\"hello\"); }")
        .expect("embed should succeed");

    // all-MiniLM-L6-v2 produces 384-dimensional vectors
    assert_eq!(
        embedding.len(),
        384,
        "expected 384-dim embedding, got {}",
        embedding.len()
    );

    // Verify L2 normalization: ||v|| should be ~1.0
    let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-4,
        "expected unit norm, got {norm}"
    );

    // Verify no NaN or Inf values
    assert!(
        embedding.iter().all(|x| x.is_finite()),
        "embedding contains NaN or Inf"
    );
}

#[test]
fn test_onnx_embed_batch() {
    let Some(embedder) = load_embedder() else {
        return;
    };

    let texts = &[
        "fn add(a: i32, b: i32) -> i32 { a + b }",
        "def multiply(x, y): return x * y",
        "SELECT * FROM users WHERE id = 1",
    ];
    let embeddings = embedder
        .embed_batch(texts)
        .expect("embed_batch should succeed");

    assert_eq!(embeddings.len(), 3, "expected 3 embeddings");
    for (i, emb) in embeddings.iter().enumerate() {
        assert_eq!(
            emb.len(),
            384,
            "embedding[{i}] should be 384-dim, got {}",
            emb.len()
        );
        let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "embedding[{i}] should be unit-normalized, got norm={norm}"
        );
    }
}

#[test]
fn test_onnx_similar_code_closer_than_unrelated() {
    let Some(embedder) = load_embedder() else {
        return;
    };

    let rust_fn = embedder
        .embed("fn calculate_sum(numbers: &[i32]) -> i32 { numbers.iter().sum() }")
        .expect("embed rust function");
    let python_fn = embedder
        .embed("def calculate_sum(numbers): return sum(numbers)")
        .expect("embed python function");
    let sql_query = embedder
        .embed("SELECT COUNT(*) FROM orders WHERE status = 'shipped' GROUP BY region")
        .expect("embed sql query");

    // Cosine similarity (vectors are already L2-normalized, so dot product = cosine)
    let sim_rust_python: f32 = rust_fn.iter().zip(python_fn.iter()).map(|(a, b)| a * b).sum();
    let sim_rust_sql: f32 = rust_fn.iter().zip(sql_query.iter()).map(|(a, b)| a * b).sum();

    assert!(
        sim_rust_python > sim_rust_sql,
        "Rust-Python similarity ({sim_rust_python:.4}) should exceed Rust-SQL similarity ({sim_rust_sql:.4})"
    );

    // The two sum functions should have high similarity (>0.5)
    assert!(
        sim_rust_python > 0.5,
        "Rust-Python sum functions should be semantically similar (got {sim_rust_python:.4})"
    );
}

#[test]
fn test_onnx_dimension() {
    let Some(embedder) = load_embedder() else {
        return;
    };

    assert_eq!(
        embedder.dimension(),
        384,
        "all-MiniLM-L6-v2 should report 384 dimensions"
    );
}

#[test]
fn test_onnx_empty_input() {
    let Some(embedder) = load_embedder() else {
        return;
    };

    let embedding = embedder.embed("").expect("empty string should embed successfully");

    assert_eq!(
        embedding.len(),
        384,
        "empty input should still produce 384-dim vector"
    );

    // Should still be finite (might be all zeros if no tokens, but should not be NaN)
    assert!(
        embedding.iter().all(|x| x.is_finite()),
        "embedding of empty string contains NaN or Inf"
    );
}
