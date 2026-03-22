use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SymbolsRequest {
    #[serde(default)]
    pub filter: String,
    pub file: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SymbolsResponse {
    pub symbols: Vec<SymbolItem>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct SymbolItem {
    pub name: String,
    pub kind: String,
    pub language: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: Option<String>,
}

pub async fn symbols_handler(
    State(state): State<AppState>,
    Json(req): Json<SymbolsRequest>,
) -> Result<Json<SymbolsResponse>, ApiError> {
    let engine = state.read().await;
    let syms = engine.symbols(&req.filter, req.file.as_deref())?;

    let total = syms.len();
    let symbols = syms
        .into_iter()
        .map(|s| SymbolItem {
            name: s.name,
            kind: format!("{:?}", s.kind),
            language: s.language.name().to_string(),
            file_path: s.file_path,
            line_start: s.line_start,
            line_end: s.line_end,
            signature: s.signature,
        })
        .collect();

    Ok(Json(SymbolsResponse { symbols, total }))
}
