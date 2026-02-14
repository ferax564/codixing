//! Tests for the embeddings module.

use super::{Embedder, MockEmbedder};

#[test]
fn mock_embedder_dimension() {
    let embedder = MockEmbedder::new(384);
    assert_eq!(embedder.dimension(), 384);
}

#[test]
fn mock_embedder_dimension_custom() {
    let embedder = MockEmbedder::new(768);
    assert_eq!(embedder.dimension(), 768);
}

#[test]
fn mock_embedder_produces_correct_length() {
    let embedder = MockEmbedder::new(384);
    let vec = embedder.embed("hello world").unwrap();
    assert_eq!(vec.len(), 384);
}

#[test]
fn mock_embedder_deterministic() {
    let embedder = MockEmbedder::new(128);
    let v1 = embedder.embed("hello world").unwrap();
    let v2 = embedder.embed("hello world").unwrap();
    assert_eq!(v1, v2, "same input should produce identical vectors");
}

#[test]
fn mock_embedder_different_inputs_differ() {
    let embedder = MockEmbedder::new(128);
    let v1 = embedder.embed("hello").unwrap();
    let v2 = embedder.embed("world").unwrap();
    assert_ne!(v1, v2, "different inputs should produce different vectors");
}

#[test]
fn mock_embedder_values_in_range() {
    let embedder = MockEmbedder::new(384);
    let vec = embedder.embed("test string").unwrap();
    for &val in &vec {
        assert!(
            (-0.5..=0.5).contains(&val),
            "value {val} out of expected range [-0.5, 0.5]"
        );
    }
}

#[test]
fn mock_embedder_zero_dimension() {
    let embedder = MockEmbedder::new(0);
    let vec = embedder.embed("anything").unwrap();
    assert!(vec.is_empty());
    assert_eq!(embedder.dimension(), 0);
}

#[test]
fn mock_embedder_empty_input() {
    let embedder = MockEmbedder::new(64);
    let vec = embedder.embed("").unwrap();
    assert_eq!(vec.len(), 64);
}

#[test]
fn mock_embedder_batch() {
    let embedder = MockEmbedder::new(128);
    let texts: &[&str] = &["hello", "world", "foo"];
    let results = embedder.embed_batch(texts).unwrap();

    assert_eq!(results.len(), 3);
    for vec in &results {
        assert_eq!(vec.len(), 128);
    }

    // Batch results should match individual results.
    let v0 = embedder.embed("hello").unwrap();
    let v1 = embedder.embed("world").unwrap();
    let v2 = embedder.embed("foo").unwrap();
    assert_eq!(results[0], v0);
    assert_eq!(results[1], v1);
    assert_eq!(results[2], v2);
}

#[test]
fn mock_embedder_batch_empty() {
    let embedder = MockEmbedder::new(128);
    let results = embedder.embed_batch(&[]).unwrap();
    assert!(results.is_empty());
}

#[test]
fn embedder_trait_is_object_safe() {
    // Verify the trait can be used as a trait object.
    fn _accept_dyn(_: &dyn Embedder) {}
    let embedder = MockEmbedder::new(64);
    _accept_dyn(&embedder);
}

#[test]
fn embedder_trait_send_sync() {
    // Verify MockEmbedder satisfies Send + Sync bounds.
    fn _assert_send_sync<T: Send + Sync>() {}
    _assert_send_sync::<MockEmbedder>();
}

#[cfg(feature = "vector")]
mod onnx_tests {
    use super::super::OnnxEmbedder;
    use std::path::PathBuf;

    #[test]
    fn load_returns_error_for_missing_dir() {
        let result = OnnxEmbedder::load(&PathBuf::from("/nonexistent/model/dir"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("model file not found"),
            "expected 'model file not found', got: {err}"
        );
    }

    #[test]
    fn load_returns_error_for_missing_tokenizer() {
        let dir = tempfile::tempdir().unwrap();
        // Create a dummy model.onnx but no tokenizer.json.
        std::fs::write(dir.path().join("model.onnx"), b"not a real model").unwrap();

        let result = OnnxEmbedder::load(dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("tokenizer file not found"),
            "expected 'tokenizer file not found', got: {err}"
        );
    }

    #[test]
    fn from_paths_returns_error_for_invalid_model() {
        let dir = tempfile::tempdir().unwrap();
        let model_path = dir.path().join("model.onnx");
        let tokenizer_path = dir.path().join("tokenizer.json");

        std::fs::write(&model_path, b"not a real onnx model").unwrap();
        std::fs::write(&tokenizer_path, b"{}").unwrap();

        let result = OnnxEmbedder::from_paths(&model_path, &tokenizer_path);
        assert!(result.is_err(), "should fail with invalid model data");
    }
}
