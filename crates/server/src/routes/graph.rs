use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use codeforge_core::RepoMapOptions;

use crate::error::ApiError;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// POST /graph/repo-map
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RepoMapRequest {
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    #[serde(default = "default_include_imports")]
    pub include_imports: bool,
    #[serde(default = "default_include_signatures")]
    pub include_signatures: bool,
    #[serde(default)]
    pub min_pagerank: f32,
}

fn default_token_budget() -> usize {
    4096
}
fn default_include_imports() -> bool {
    true
}
fn default_include_signatures() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct RepoMapResponse {
    pub map: Option<String>,
    pub available: bool,
}

pub async fn repo_map_handler(
    State(state): State<AppState>,
    Json(req): Json<RepoMapRequest>,
) -> Result<Json<RepoMapResponse>, ApiError> {
    let engine = state.read().await;
    let opts = RepoMapOptions {
        token_budget: req.token_budget,
        include_imports: req.include_imports,
        include_signatures: req.include_signatures,
        min_pagerank: req.min_pagerank,
    };
    let map = engine.repo_map(opts);
    let available = map.is_some();
    Ok(Json(RepoMapResponse { map, available }))
}

// ---------------------------------------------------------------------------
// GET /graph/callers?file=<path>&depth=1
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FileDepthQuery {
    pub file: String,
    #[serde(default = "default_depth")]
    pub depth: usize,
}

fn default_depth() -> usize {
    1
}

#[derive(Debug, Serialize)]
pub struct FilesResponse {
    pub files: Vec<String>,
    pub count: usize,
}

pub async fn callers_handler(
    State(state): State<AppState>,
    Query(params): Query<FileDepthQuery>,
) -> Result<Json<FilesResponse>, ApiError> {
    let engine = state.read().await;
    let files = if params.depth <= 1 {
        engine.callers(&params.file)
    } else {
        engine.dependencies(&params.file, params.depth)
    };
    let count = files.len();
    Ok(Json(FilesResponse { files, count }))
}

// ---------------------------------------------------------------------------
// GET /graph/callees?file=<path>&depth=1
// ---------------------------------------------------------------------------

pub async fn callees_handler(
    State(state): State<AppState>,
    Query(params): Query<FileDepthQuery>,
) -> Result<Json<FilesResponse>, ApiError> {
    let engine = state.read().await;
    let files = engine.callees(&params.file);
    let count = files.len();
    Ok(Json(FilesResponse { files, count }))
}

// ---------------------------------------------------------------------------
// GET /graph/stats
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GraphStatsResponse {
    pub available: bool,
    pub node_count: usize,
    pub edge_count: usize,
    pub resolved_edges: usize,
    pub external_edges: usize,
}

pub async fn stats_handler(
    State(state): State<AppState>,
) -> Result<Json<GraphStatsResponse>, ApiError> {
    let engine = state.read().await;
    let response = match engine.graph_stats() {
        Some(s) => GraphStatsResponse {
            available: true,
            node_count: s.node_count,
            edge_count: s.edge_count,
            resolved_edges: s.resolved_edges,
            external_edges: s.external_edges,
        },
        None => GraphStatsResponse {
            available: false,
            node_count: 0,
            edge_count: 0,
            resolved_edges: 0,
            external_edges: 0,
        },
    };
    Ok(Json(response))
}
