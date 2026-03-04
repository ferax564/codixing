use std::path::PathBuf;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// POST /index/reindex
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ReindexRequest {
    pub file_path: String,
}

#[derive(Debug, Serialize)]
pub struct ReindexResponse {
    pub status: &'static str,
    pub file_path: String,
    pub elapsed_ms: u64,
}

pub async fn reindex_handler(
    State(state): State<AppState>,
    Json(req): Json<ReindexRequest>,
) -> Result<Json<ReindexResponse>, ApiError> {
    let start = Instant::now();
    let path = PathBuf::from(&req.file_path);

    let mut engine = state.write().await;
    engine.reindex_file(&path)?;

    Ok(Json(ReindexResponse {
        status: "ok",
        file_path: req.file_path,
        elapsed_ms: start.elapsed().as_millis() as u64,
    }))
}

// ---------------------------------------------------------------------------
// DELETE /index/file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RemoveFileRequest {
    pub file_path: String,
}

#[derive(Debug, Serialize)]
pub struct RemoveFileResponse {
    pub status: &'static str,
    pub file_path: String,
}

pub async fn remove_file_handler(
    State(state): State<AppState>,
    Json(req): Json<RemoveFileRequest>,
) -> Result<Json<RemoveFileResponse>, ApiError> {
    let path = PathBuf::from(&req.file_path);

    let mut engine = state.write().await;
    engine.remove_file(&path)?;

    Ok(Json(RemoveFileResponse {
        status: "ok",
        file_path: req.file_path,
    }))
}

// ---------------------------------------------------------------------------
// GET /status
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub version: &'static str,
    pub file_count: usize,
    pub chunk_count: usize,
    pub symbol_count: usize,
    pub vector_count: usize,
    pub embedding_enabled: bool,
}

pub async fn status_handler(
    State(state): State<AppState>,
) -> Result<Json<StatusResponse>, ApiError> {
    let engine = state.read().await;
    let stats = engine.stats();

    Ok(Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        file_count: stats.file_count,
        chunk_count: stats.chunk_count,
        symbol_count: stats.symbol_count,
        vector_count: stats.vector_count,
        embedding_enabled: engine.config().embedding.enabled,
    }))
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

pub async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}
