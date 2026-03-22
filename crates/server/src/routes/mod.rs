pub mod graph;
pub mod index;
pub mod search;
pub mod symbols;

use axum::Router;
use axum::routing::{delete, get, post};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// Build the axum router with all API routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Search
        .route("/search", post(search::search_handler))
        // Symbols
        .route("/symbols", post(symbols::symbols_handler))
        // Index management
        .route("/index/reindex", post(index::reindex_handler))
        .route("/index/file", delete(index::remove_file_handler))
        .route("/index/sync", post(index::sync_sse_handler))
        // Status + health
        .route("/status", get(index::status_handler))
        .route("/health", get(index::health_handler))
        // Graph intelligence
        .route("/graph/repo-map", post(graph::repo_map_handler))
        .route("/graph/callers", get(graph::callers_handler))
        .route("/graph/callees", get(graph::callees_handler))
        .route("/graph/stats", get(graph::stats_handler))
        .route("/graph/export", get(graph::export_handler))
        .route("/graph/call-graph", get(graph::call_graph_handler))
        .route("/graph/history", get(graph::history_handler))
        .route("/graph/view", get(graph::view_handler))
        // Middleware
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
