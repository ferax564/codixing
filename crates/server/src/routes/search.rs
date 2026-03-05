use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::time::Instant;

use codixing_core::{SearchQuery, Strategy};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    pub file_filter: Option<String>,
    #[serde(default)]
    pub strategy: Strategy,
    pub token_budget: Option<usize>,
}

fn default_limit() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<ResultItem>,
    pub formatted_context: Option<String>,
    pub total: usize,
    pub strategy_used: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ResultItem {
    pub chunk_id: String,
    pub file_path: String,
    pub language: String,
    pub score: f32,
    pub line_start: u64,
    pub line_end: u64,
    pub signature: String,
    pub scope_chain: Vec<String>,
    pub content: String,
}

pub async fn search_handler(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let start = Instant::now();
    let strategy = req.strategy;
    let token_budget = req.token_budget;

    let mut sq = SearchQuery::new(&req.query)
        .with_limit(req.limit)
        .with_strategy(strategy);

    if let Some(ref f) = req.file_filter {
        sq = sq.with_file_filter(f);
    }
    if let Some(b) = token_budget {
        sq = sq.with_token_budget(b);
    }

    let engine = state.read().await;
    let search_results = engine.search(sq)?;

    let formatted_context = if token_budget.is_some() || req.token_budget.is_some() {
        Some(engine.format_results(&search_results, token_budget))
    } else {
        None
    };

    let total = search_results.len();
    let elapsed_ms = start.elapsed().as_millis() as u64;

    let results = search_results
        .into_iter()
        .map(|r| ResultItem {
            chunk_id: r.chunk_id,
            file_path: r.file_path,
            language: r.language,
            score: r.score,
            line_start: r.line_start,
            line_end: r.line_end,
            signature: r.signature,
            scope_chain: r.scope_chain,
            content: r.content,
        })
        .collect();

    Ok(Json(SearchResponse {
        results,
        formatted_context,
        total,
        strategy_used: format!("{strategy:?}").to_lowercase(),
        elapsed_ms,
    }))
}
