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
async fn reindex_rejects_paths_outside_root() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.md"), "# outside\n").unwrap();

    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/index/reindex")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "file_path": outside.path().join("outside.md")
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("must stay within the indexed project roots")
    );
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

#[tokio::test]
async fn reindex_file_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let file_path = dir.path().join("src/main.rs");
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/index/reindex")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "file_path": file_path.to_str().unwrap()
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

    assert_eq!(json["status"], "ok");
    assert!(json["elapsed_ms"].as_u64().is_some());
}

#[tokio::test]
async fn remove_file_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let file_path = dir.path().join("src/main.rs");
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/index/file")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "file_path": file_path.to_str().unwrap()
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

    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn repo_map_returns_response() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/graph/repo-map")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "token_budget": 2048
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

    assert!(json["map"].is_null() || json["map"].is_string());
}

#[tokio::test]
async fn graph_callees_returns_response() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/graph/callees?file=src/main.rs&depth=1")
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

    assert!(json["files"].as_array().is_some());
}

#[tokio::test]
async fn graph_call_graph_returns_response() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/graph/call-graph")
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

    assert!(json["edges"].as_array().is_some());
}

#[tokio::test]
async fn graph_export_returns_response() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/graph/export")
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

    assert!(json["nodes"].as_array().is_some());
    assert!(json["edges"].as_array().is_some());
}

#[tokio::test]
async fn graph_view_returns_html() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/graph/view")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        html.contains("<html") || html.contains("<!DOCTYPE"),
        "expected HTML response, got: {}",
        &html[..html.len().min(200)]
    );
}

// ---------------------------------------------------------------------------
// SSE streaming tests for POST /index/sync
// ---------------------------------------------------------------------------

/// Parse an SSE byte stream into (event_name, data_payload) frames.
///
/// Each SSE frame is separated by `\n\n`. Within a frame, lines of the form
/// `event: NAME` and `data: PAYLOAD` are recognized. Empty frames and
/// comments (`:` prefix) are skipped.
fn parse_sse_frames(body: &[u8]) -> Vec<(String, String)> {
    let text = std::str::from_utf8(body).unwrap_or("");
    let mut frames = Vec::new();

    for block in text.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut event_name = String::from("message");
        let mut data_lines: Vec<&str> = Vec::new();

        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event_name = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }

        if !data_lines.is_empty() {
            frames.push((event_name, data_lines.join("\n")));
        }
    }

    frames
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_sse_returns_event_stream_content_type() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/index/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected text/event-stream, got: {content_type}"
    );

    // Drain the body so the spawned task completes and drops the lock.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("SSE stream should close within 3s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_sse_emits_progress_frames() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/index/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("SSE stream should close within 3s")
    .expect("SSE body read should succeed");

    let frames = parse_sse_frames(&body_bytes);
    assert!(
        !frames.is_empty(),
        "expected at least one SSE frame, got 0. raw body: {:?}",
        String::from_utf8_lossy(&body_bytes)
    );

    // At least one frame should be a progress event with a recognizable message.
    let progress_frames: Vec<&(String, String)> =
        frames.iter().filter(|(ev, _)| ev == "progress").collect();
    assert!(
        !progress_frames.is_empty(),
        "expected at least one 'progress' SSE frame, got events: {:?}",
        frames.iter().map(|(e, _)| e).collect::<Vec<_>>()
    );

    // The first progress message is always "scanning files" (see
    // Engine::sync_with_progress in crates/core/src/engine/sync.rs).
    let first_progress = &progress_frames[0].1;
    assert!(
        first_progress.contains("scanning") || first_progress.contains("files"),
        "first progress frame should mention scanning/files, got: {first_progress:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_sse_emits_terminal_result_or_error_frame() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_test_engine(dir.path());
    let app = build_router(new_state(engine));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/index/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("SSE stream should close within 3s")
    .expect("SSE body read should succeed");

    let frames = parse_sse_frames(&body_bytes);

    // The stream MUST close with either a `result` frame (happy path) or an
    // `error` frame as the *last* frame — progress after terminal would mean
    // the SSE contract is broken.
    let terminal = frames.last();
    assert!(
        terminal.is_some_and(|(ev, _)| ev == "result" || ev == "error"),
        "expected the LAST frame to be 'result' or 'error', got events: {:?}",
        frames.iter().map(|(e, _)| e).collect::<Vec<_>>()
    );

    let (kind, payload) = terminal.unwrap();
    if kind == "result" {
        // Happy path: payload is JSON-serialized SyncStats with numeric fields.
        let json: serde_json::Value = serde_json::from_str(payload)
            .unwrap_or_else(|e| panic!("result payload should be JSON: {e}. got: {payload:?}"));
        assert!(
            json.is_object(),
            "result payload should be a JSON object, got: {payload:?}"
        );
    } else {
        // An error frame carries a plain-text message — just assert it's non-empty.
        assert!(
            !payload.is_empty(),
            "error frame payload should be non-empty"
        );
    }
}
