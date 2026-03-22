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

pub fn complex_function(x: i32) -> i32 {
    if x > 0 {
        if x > 10 {
            for i in 0..x {
                if i % 2 == 0 {
                    return i;
                }
            }
        }
    }
    x
}

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
    assert!(!json["results"].as_array().unwrap().is_empty());
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
    let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(
        names.iter().any(|n| n.contains("hello")),
        "expected 'hello' symbol, got: {names:?}"
    );
}

#[tokio::test]
async fn find_symbol_returns_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/find-symbol")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "name": "hello"
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
    let first = &symbols[0];
    assert_eq!(first["name"].as_str().unwrap(), "hello");
    assert!(first["kind"].as_str().is_some());
    assert!(first["file_path"].as_str().is_some());
    assert!(first["line_start"].as_u64().is_some());
}

#[tokio::test]
async fn grep_returns_matches() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/grep")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "pattern": "hello",
                        "literal": true
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
    let matches = json["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    let first = &matches[0];
    assert!(first["file_path"].as_str().is_some());
    assert!(first["line_number"].as_u64().is_some());
    assert!(first["line"].as_str().unwrap().contains("hello"));
}

#[tokio::test]
async fn grep_rejects_empty_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/grep")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "pattern": ""
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn hotspots_returns_list() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/hotspots?days=90&limit=5")
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

    // Temp dir is not a git repo, so expect empty results but valid response shape.
    assert!(json["total"].as_u64().is_some());
    assert!(json["hotspots"].as_array().is_some());
}

#[tokio::test]
async fn complexity_returns_functions() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/complexity/src/main.rs")
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

    assert_eq!(json["file"].as_str().unwrap(), "src/main.rs");
    assert!(json["total"].as_u64().unwrap() > 0);
    let functions = json["functions"].as_array().unwrap();
    assert!(!functions.is_empty());
    let first = &functions[0];
    assert!(first["name"].as_str().is_some());
    assert!(first["cyclomatic_complexity"].as_u64().is_some());
    assert!(first["risk"].as_str().is_some());
}

#[tokio::test]
async fn complexity_missing_file_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/complexity/nonexistent.rs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn outline_returns_symbols() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/outline/src/main.rs")
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

    assert_eq!(json["file"].as_str().unwrap(), "src/main.rs");
    assert!(json["total"].as_u64().unwrap() > 0);
    let symbols = json["symbols"].as_array().unwrap();
    assert!(!symbols.is_empty());

    let names: Vec<&str> = symbols.iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(
        names.contains(&"hello"),
        "expected 'hello' in outline, got: {names:?}"
    );
}

#[tokio::test]
async fn graph_stats_returns_response() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/graph/stats")
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

    // Even with a single file, the graph should report something.
    assert!(json["available"].as_bool().is_some());
    assert!(json["node_count"].as_u64().is_some());
    assert!(json["edge_count"].as_u64().is_some());
}

#[tokio::test]
async fn graph_callers_returns_response() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/graph/callers?file=src/main.rs")
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

    assert!(json["count"].as_u64().is_some());
    assert!(json["files"].as_array().is_some());
}
