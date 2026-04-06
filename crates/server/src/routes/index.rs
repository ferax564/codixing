use std::convert::Infallible;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

use crate::error::ApiError;
use crate::routes::paths::resolve_repo_path;
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
    let mut engine = state.write().await;
    let path = resolve_repo_path(&engine, &req.file_path)?;
    engine.reindex_file(&path.absolute)?;
    engine.save()?;

    Ok(Json(ReindexResponse {
        status: "ok",
        file_path: path.relative,
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
    let mut engine = state.write().await;
    let path = resolve_repo_path(&engine, &req.file_path)?;
    engine.remove_file(&path.absolute)?;
    engine.save()?;

    Ok(Json(RemoveFileResponse {
        status: "ok",
        file_path: path.relative,
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

// ---------------------------------------------------------------------------
// POST /index/sync — SSE streaming sync
// ---------------------------------------------------------------------------

pub async fn sync_sse_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);

    tokio::spawn(async move {
        // Acquire the write lock before entering block_in_place so we hold it
        // across the synchronous sync operation.
        let mut engine = state.write().await;

        let tx_progress = tx.clone();
        let result = tokio::task::block_in_place(move || {
            engine.sync_with_progress(move |msg| {
                let event = Event::default().event("progress").data(msg);
                // Ignore send errors (client may have disconnected).
                let _ = tx_progress.blocking_send(Ok(event));
            })
        });

        let final_event = match result {
            Ok(stats) => {
                let json = serde_json::to_string(&stats).unwrap_or_default();
                Event::default().event("result").data(json)
            }
            Err(e) => Event::default().event("error").data(e.to_string()),
        };
        let _ = tx.send(Ok(final_event)).await;
    });

    Sse::new(ReceiverStream::new(rx))
}
