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

mod embedding_backend_tests {
    use super::super::EmbeddingBackend;

    #[test]
    fn embedding_backend_dimension() {
        assert_eq!(EmbeddingBackend::Mock.dimension(), 32);
        assert_eq!(EmbeddingBackend::Onnx.dimension(), 384);
        assert_eq!(
            EmbeddingBackend::External {
                url: "https://api.example.com/embed".into(),
                model: "text-embedding-3-small".into(),
                dimension: 1536,
                api_key: None,
                batch_size: None,
            }
            .dimension(),
            1536
        );
    }

    #[test]
    fn embedding_backend_serde_roundtrip() {
        // Mock
        let mock = EmbeddingBackend::Mock;
        let json = serde_json::to_string(&mock).unwrap();
        let parsed: EmbeddingBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mock);

        // Onnx
        let onnx = EmbeddingBackend::Onnx;
        let json = serde_json::to_string(&onnx).unwrap();
        let parsed: EmbeddingBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, onnx);

        // External
        let external = EmbeddingBackend::External {
            url: "https://api.voyageai.com/v1/embeddings".into(),
            model: "voyage-code-3".into(),
            dimension: 1024,
            api_key: Some("$VOYAGE_API_KEY".into()),
            batch_size: Some(64),
        };
        let json = serde_json::to_string(&external).unwrap();
        let parsed: EmbeddingBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, external);
    }

    #[test]
    fn embedding_backend_default_is_mock() {
        let backend = EmbeddingBackend::default();
        assert_eq!(backend, EmbeddingBackend::Mock);
    }

    #[test]
    fn index_config_defaults_to_mock() {
        use crate::config::IndexConfig;
        let config = IndexConfig::new("/tmp/test");
        assert_eq!(config.embedding_backend, EmbeddingBackend::Mock);
    }

    #[test]
    fn embedding_backend_external_no_optional_fields() {
        // External with None api_key and batch_size should serde properly.
        let external = EmbeddingBackend::External {
            url: "http://localhost:8000/embed".into(),
            model: "custom-model".into(),
            dimension: 768,
            api_key: None,
            batch_size: None,
        };
        let json = serde_json::to_string(&external).unwrap();
        let parsed: EmbeddingBackend = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, external);
        assert_eq!(parsed.dimension(), 768);
    }
}

mod http_embedder_tests {
    use super::super::Embedder;
    use super::super::http::HttpEmbedder;

    #[test]
    fn http_embedder_constructs() {
        let embedder = HttpEmbedder::new(
            "https://api.openai.com/v1/embeddings",
            "text-embedding-3-small",
            1536,
            Some("sk-test-key".into()),
            32,
        );
        assert_eq!(embedder.dimension(), 1536);
    }

    #[test]
    fn http_embedder_send_sync() {
        fn _assert_send_sync<T: Send + Sync>() {}
        _assert_send_sync::<HttpEmbedder>();
    }
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
