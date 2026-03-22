//! Integration tests for the Codixing HTTP server routes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use codixing_core::{EmbeddingConfig, Engine, IndexConfig};
use codixing_server::routes::build_router;
use codixing_server::state::new_state;

/// Create a BM25-only engine in a temp directory with a small Rust file.
fn make_test_engine(dir: &std::path::Path) -> Engine {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("main.rs"),
        r#"pub fn hello() -> &'static str { "world" }

pub struct Config {
    pub verbose: bool,
}
"#,
    )
    .unwrap();

    let mut config = IndexConfig::new(dir);
    config.embedding = EmbeddingConfig {
        enabled: false,
        ..EmbeddingConfig::default()
    };
    Engine::init(dir, config).expect("engine init should succeed")
}

#[tokio::test]
async fn health_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn status_returns_stats() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(json["file_count"].as_u64().unwrap() > 0);
    assert!(json["chunk_count"].as_u64().unwrap() > 0);
    assert!(json["symbol_count"].as_u64().unwrap() > 0);
    assert_eq!(json["embedding_enabled"], false);
}

#[tokio::test]
async fn search_returns_results() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/search")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "query": "hello",
                        "limit": 5
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(json["total"].as_u64().unwrap() > 0);
    assert!(json["results"].as_array().unwrap().len() > 0);
    assert!(json["elapsed_ms"].as_u64().is_some());
}

#[tokio::test]
async fn symbols_returns_list() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/symbols")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "filter": "hello"
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(json["total"].as_u64().unwrap() > 0);
    let symbols = json["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());
    // Verify the hello symbol was found.
    let names: Vec<&str> = symbols
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.iter().any(|n| n.contains("hello")),
        "expected 'hello' symbol, got: {names:?}"
    );
}
