use std::path::{Path, PathBuf};

use anyhow::Context;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use codeforge_core::{CodeforgeError, ContextBudget, Engine, IndexConfig, SearchQuery};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "codeforge-server")]
#[command(about = "CodeForge HTTP API server")]
struct Cli {
    /// Host address to bind to.
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Port to bind to.
    #[arg(long, default_value_t = 3002)]
    port: u16,
}

#[derive(Debug, Clone, Deserialize)]
struct AdapterRequest {
    source: String,
    #[serde(default)]
    config: Value,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "status": "error",
                "error": self.message,
            })),
        )
            .into_response()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let bind_addr = format!("{}:{}", cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;

    info!("codeforge-server listening on {bind_addr}");
    axum::serve(listener, build_app())
        .await
        .context("codeforge-server failed")
}

fn build_app() -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/api/v1/index", post(index_handler))
        .route("/api/v1/search", post(search_handler))
        .route("/api/v1/hybrid-search", post(hybrid_search_handler))
        .route("/api/v1/context", post(context_handler))
}

async fn health_handler() -> Json<Value> {
    Json(json!({
        "ok": true,
        "status": "healthy",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn index_handler(Json(req): Json<AdapterRequest>) -> Result<Json<Value>, ApiError> {
    let root = resolve_source_root(&req.source)?;
    let index_dir = root.join(".codeforge");
    if index_dir.exists() {
        std::fs::remove_dir_all(&index_dir).map_err(|e| {
            ApiError::internal(format!(
                "failed to reset existing index at {}: {e}",
                index_dir.display()
            ))
        })?;
    }

    let mut index_config = IndexConfig::new(&root);
    apply_index_overrides(&mut index_config, &req.config)?;

    let engine = Engine::init(&root, index_config).map_err(map_engine_error)?;
    let stats = engine.stats();

    Ok(Json(json!({
        "operation": "index",
        "status": "ok",
        "indexed_path": root.to_string_lossy(),
        "files": stats.file_count,
        "chunks": stats.chunk_count,
        "symbols": stats.symbol_count,
    })))
}

async fn search_handler(Json(req): Json<AdapterRequest>) -> Result<Json<Value>, ApiError> {
    let root = resolve_source_root(&req.source)?;
    let engine = Engine::open(&root).map_err(map_engine_error)?;
    let query = parse_search_query(&req.config, true)?;
    let query_text = query.query.clone();
    let results = engine.search(query).map_err(map_engine_error)?;

    let mapped: Vec<Value> = results
        .into_iter()
        .map(|result| {
            json!({
                "path": result.file_path,
                "language": result.language,
                "score": result.score,
                "start_line": result.line_start,
                "end_line": result.line_end,
                "signature": result.signature,
                "content": result.content,
            })
        })
        .collect();

    Ok(Json(json!({
        "operation": "search",
        "status": "ok",
        "query": query_text,
        "results": mapped,
    })))
}

async fn hybrid_search_handler(Json(req): Json<AdapterRequest>) -> Result<Json<Value>, ApiError> {
    let root = resolve_source_root(&req.source)?;
    let engine = Engine::open(&root).map_err(map_engine_error)?;
    let query = parse_search_query(&req.config, true)?;
    let query_text = query.query.clone();
    let results = engine.hybrid_search(query).map_err(map_engine_error)?;

    let mapped: Vec<Value> = results
        .into_iter()
        .map(|result| {
            json!({
                "path": result.file_path,
                "language": result.language,
                "score": result.score,
                "start_line": result.line_start,
                "end_line": result.line_end,
                "signature": result.signature,
                "content": result.content,
            })
        })
        .collect();

    Ok(Json(json!({
        "operation": "hybrid_search",
        "status": "ok",
        "query": query_text,
        "results": mapped,
    })))
}

async fn context_handler(Json(req): Json<AdapterRequest>) -> Result<Json<Value>, ApiError> {
    let root = resolve_source_root(&req.source)?;
    let engine = Engine::open(&root).map_err(map_engine_error)?;
    let token_budget = req
        .config
        .get("token_budget")
        .and_then(Value::as_u64)
        .unwrap_or(2048)
        .clamp(128, 65_536) as usize;

    let mut budget = ContextBudget::new(token_budget);

    if let Some(query_text) = extract_query_text(&req.config) {
        let query = parse_search_query(&req.config, false)?;
        let results = engine.search(query).map_err(map_engine_error)?;
        for result in results {
            if budget.remaining() == 0 {
                break;
            }
            budget.try_add(
                result.file_path,
                result.language,
                result.content,
                result.line_start,
                result.line_end,
                result.score,
            );
        }

        let snippets: Vec<Value> = budget
            .into_snippets()
            .into_iter()
            .map(|s| {
                json!({
                    "path": s.file_path,
                    "start_line": s.line_start,
                    "end_line": s.line_end,
                    "score": s.score,
                    "language": s.language,
                    "content": s.content,
                    "token_count": s.token_count,
                })
            })
            .collect();

        return Ok(Json(json!({
            "operation": "context_retrieval",
            "status": "ok",
            "query": query_text,
            "token_budget": token_budget,
            "snippets": snippets,
        })));
    }

    let symbol_filter = req
        .config
        .get("filter")
        .or_else(|| req.config.get("symbol"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let file_filter = req
        .config
        .get("file")
        .or_else(|| req.config.get("file_filter"))
        .and_then(Value::as_str);
    let symbols = engine
        .symbols(symbol_filter, file_filter)
        .map_err(map_engine_error)?;

    for symbol in symbols {
        if budget.remaining() == 0 {
            break;
        }
        let signature = symbol.signature.unwrap_or_default();
        budget.try_add(
            symbol.file_path,
            String::new(),
            signature,
            symbol.line_start as u64,
            symbol.line_end as u64,
            0.0,
        );
    }

    let snippets: Vec<Value> = budget
        .into_snippets()
        .into_iter()
        .map(|s| {
            json!({
                "path": s.file_path,
                "start_line": s.line_start,
                "end_line": s.line_end,
                "score": s.score,
                "signature": s.content,
                "content": "",
                "token_count": s.token_count,
            })
        })
        .collect();

    Ok(Json(json!({
        "operation": "context_retrieval",
        "status": "ok",
        "query": Value::Null,
        "token_budget": token_budget,
        "snippets": snippets,
    })))
}

fn parse_search_query(config: &Value, require_query: bool) -> Result<SearchQuery, ApiError> {
    let query_text = extract_query_text(config);
    if require_query && query_text.is_none() {
        return Err(ApiError::bad_request(
            "search query missing: expected config.query",
        ));
    }

    let query_text = query_text.unwrap_or_else(|| "fn".to_string());
    let limit = config
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .clamp(1, 100) as usize;

    let mut query = SearchQuery::new(query_text).with_limit(limit);
    if let Some(file_filter) = config
        .get("file")
        .or_else(|| config.get("file_filter"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        query = query.with_file_filter(file_filter.to_string());
    }

    Ok(query)
}

fn extract_query_text(config: &Value) -> Option<String> {
    config
        .get("query")
        .or_else(|| config.get("q"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn apply_index_overrides(config: &mut IndexConfig, raw: &Value) -> Result<(), ApiError> {
    if let Some(languages) = raw.get("languages") {
        let values = languages
            .as_array()
            .ok_or_else(|| ApiError::bad_request("config.languages must be an array of strings"))?;
        config.languages.clear();
        for value in values {
            let language = value.as_str().ok_or_else(|| {
                ApiError::bad_request("config.languages must be an array of strings")
            })?;
            config.languages.insert(language.to_lowercase());
        }
    }

    if let Some(excludes) = raw.get("exclude_patterns") {
        let values = excludes.as_array().ok_or_else(|| {
            ApiError::bad_request("config.exclude_patterns must be an array of strings")
        })?;
        let mut parsed = Vec::with_capacity(values.len());
        for value in values {
            let pattern = value.as_str().ok_or_else(|| {
                ApiError::bad_request("config.exclude_patterns must be an array of strings")
            })?;
            parsed.push(pattern.to_string());
        }
        config.exclude_patterns = parsed;
    }

    if let Some(chunk) = raw.get("chunk") {
        let chunk_obj = chunk
            .as_object()
            .ok_or_else(|| ApiError::bad_request("config.chunk must be an object"))?;
        if let Some(max_chars) = chunk_obj.get("max_chars") {
            config.chunk.max_chars = max_chars
                .as_u64()
                .ok_or_else(|| ApiError::bad_request("config.chunk.max_chars must be a number"))?
                as usize;
        }
        if let Some(min_chars) = chunk_obj.get("min_chars") {
            config.chunk.min_chars = min_chars
                .as_u64()
                .ok_or_else(|| ApiError::bad_request("config.chunk.min_chars must be a number"))?
                as usize;
        }
    }

    Ok(())
}

fn resolve_source_root(source: &str) -> Result<PathBuf, ApiError> {
    let path = parse_source_path(source)?;
    let canonical = path.canonicalize().map_err(|e| {
        ApiError::bad_request(format!("source path '{}' is invalid: {e}", path.display()))
    })?;

    if canonical.is_file() {
        return canonical
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| ApiError::bad_request("source file has no parent directory"));
    }
    Ok(canonical)
}

fn parse_source_path(source: &str) -> Result<PathBuf, ApiError> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("source is required"));
    }

    if let Some(path) = trimmed.strip_prefix("file://") {
        return Ok(PathBuf::from(path));
    }

    if trimmed.contains("://") {
        return Err(ApiError::bad_request(
            "only local filesystem paths are supported for source",
        ));
    }

    Ok(PathBuf::from(trimmed))
}

fn map_engine_error(err: CodeforgeError) -> ApiError {
    match err {
        CodeforgeError::IndexNotFound { .. } => {
            ApiError::new(StatusCode::NOT_FOUND, err.to_string())
        }
        CodeforgeError::Config(_)
        | CodeforgeError::Parse { .. }
        | CodeforgeError::QueryParse(_)
        | CodeforgeError::UnsupportedLanguage { .. } => {
            ApiError::new(StatusCode::BAD_REQUEST, err.to_string())
        }
        _ => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode};
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tower::ServiceExt;

    use super::build_app;

    async fn request_json(
        method: Method,
        uri: &str,
        payload: Option<Value>,
    ) -> (StatusCode, Value) {
        let app = build_app();
        let mut builder = Request::builder().method(method).uri(uri);
        if payload.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let body = payload
            .map(|value| Body::from(value.to_string()))
            .unwrap_or_else(Body::empty);
        let request = builder.body(body).unwrap();

        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        (status, json)
    }

    fn write_fixture_project(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src").join("lib.rs"),
            r#"
pub fn router_handler(name: &str) -> String {
    format!("hello-{name}")
}

pub struct RouterConfig {
    pub name: String,
}
"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let (status, body) = request_json(Method::GET, "/health", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "healthy");
        assert_eq!(body["ok"], true);
    }

    #[tokio::test]
    async fn index_search_and_context_flow() {
        let project = tempdir().unwrap();
        write_fixture_project(project.path());
        let source = project.path().to_string_lossy().to_string();

        let (status, index_body) = request_json(
            Method::POST,
            "/api/v1/index",
            Some(json!({"source": source, "config": {"op": "index"}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(index_body["operation"], "index");
        assert_eq!(index_body["status"], "ok");
        assert!(index_body["files"].as_u64().unwrap() >= 1);

        let (status, search_body) = request_json(
            Method::POST,
            "/api/v1/search",
            Some(json!({"source": source, "config": {"op": "search", "query": "router_handler", "limit": 5}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(search_body["operation"], "search");
        assert_eq!(search_body["status"], "ok");
        assert!(!search_body["results"].as_array().unwrap().is_empty());

        let (status, context_body) = request_json(
            Method::POST,
            "/api/v1/context",
            Some(
                json!({"source": source, "config": {"op": "context_retrieval", "query": "router_handler", "token_budget": 128}}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(context_body["operation"], "context_retrieval");
        assert_eq!(context_body["status"], "ok");
        assert!(!context_body["snippets"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn hybrid_search_returns_results() {
        let project = tempdir().unwrap();
        write_fixture_project(project.path());
        let source = project.path().to_string_lossy().to_string();

        // Index first
        let (status, _) = request_json(
            Method::POST,
            "/api/v1/index",
            Some(json!({"source": source, "config": {"op": "index"}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Now query hybrid search
        let (status, body) = request_json(
            Method::POST,
            "/api/v1/hybrid-search",
            Some(json!({"source": source, "config": {"op": "hybrid_search", "query": "router_handler", "limit": 5}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["operation"], "hybrid_search");
        assert_eq!(body["status"], "ok");
        assert!(!body["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_without_index_returns_not_found() {
        let project = tempdir().unwrap();
        write_fixture_project(project.path());
        let source = project.path().to_string_lossy().to_string();

        let (status, body) = request_json(
            Method::POST,
            "/api/v1/search",
            Some(json!({"source": source, "config": {"op": "search", "query": "router"}})),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["status"], "error");
        assert!(body["error"].as_str().unwrap().contains("index not found"));
    }
}
