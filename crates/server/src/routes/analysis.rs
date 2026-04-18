use std::fs;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use codixing_core::EntityKind;
use codixing_core::complexity::{count_cyclomatic_complexity, risk_band};

use crate::error::ApiError;
use crate::routes::paths::resolve_repo_path;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// POST /find-symbol
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct FindSymbolRequest {
    pub name: String,
    pub file: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FindSymbolResponse {
    pub symbols: Vec<SymbolDefItem>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct SymbolDefItem {
    pub name: String,
    pub kind: String,
    pub language: String,
    pub file_path: String,
    pub line_start: usize,
    pub line_end: usize,
    pub signature: Option<String>,
    pub scope: Vec<String>,
}

pub async fn find_symbol_handler(
    State(state): State<AppState>,
    Json(req): Json<FindSymbolRequest>,
) -> Result<Json<FindSymbolResponse>, ApiError> {
    let engine = state.read().await;
    let syms = engine.symbols(&req.name, req.file.as_deref())?;

    let total = syms.len();
    let symbols = syms
        .into_iter()
        .map(|s| SymbolDefItem {
            name: s.name,
            kind: format!("{:?}", s.kind),
            language: s.language.name().to_string(),
            file_path: s.file_path,
            line_start: s.line_start,
            line_end: s.line_end,
            signature: s.signature,
            scope: s.scope,
        })
        .collect();

    Ok(Json(FindSymbolResponse { symbols, total }))
}

// ---------------------------------------------------------------------------
// POST /grep
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GrepRequest {
    pub pattern: String,
    #[serde(default)]
    pub literal: bool,
    pub file_glob: Option<String>,
    #[serde(default = "default_grep_limit")]
    pub limit: usize,
    #[serde(default)]
    pub context_lines: usize,
}

fn default_grep_limit() -> usize {
    50
}

#[derive(Debug, Serialize)]
pub struct GrepResponse {
    pub matches: Vec<GrepMatchItem>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct GrepMatchItem {
    pub file_path: String,
    pub line_number: u64,
    pub line: String,
    pub match_start: usize,
    pub match_end: usize,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

pub async fn grep_handler(
    State(state): State<AppState>,
    Json(req): Json<GrepRequest>,
) -> Result<Json<GrepResponse>, ApiError> {
    if req.pattern.is_empty() {
        return Err(ApiError::BadRequest(
            "pattern must not be empty".to_string(),
        ));
    }

    let engine = state.read().await;
    let results = engine.grep_code(
        &req.pattern,
        req.literal,
        req.file_glob.as_deref(),
        req.context_lines,
        req.limit,
    )?;

    let total = results.len();
    let matches = results
        .into_iter()
        .map(|m| GrepMatchItem {
            file_path: m.file_path,
            line_number: m.line_number,
            line: m.line,
            match_start: m.match_start,
            match_end: m.match_end,
            before: m.before,
            after: m.after,
        })
        .collect();

    Ok(Json(GrepResponse { matches, total }))
}

// ---------------------------------------------------------------------------
// GET /hotspots?days=90&limit=10
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct HotspotsQuery {
    #[serde(default = "default_hotspot_days")]
    pub days: u64,
    #[serde(default = "default_hotspot_limit")]
    pub limit: usize,
}

fn default_hotspot_days() -> u64 {
    90
}

fn default_hotspot_limit() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct HotspotsResponse {
    pub hotspots: Vec<HotspotItem>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct HotspotItem {
    pub file_path: String,
    pub commit_count: usize,
    pub author_count: usize,
    pub score: f32,
}

pub async fn hotspots_handler(
    State(state): State<AppState>,
    Query(params): Query<HotspotsQuery>,
) -> Result<Json<HotspotsResponse>, ApiError> {
    let engine = state.read().await;
    let hotspots = engine.get_hotspots(params.limit, params.days);

    let total = hotspots.len();
    let items = hotspots
        .into_iter()
        .map(|h| HotspotItem {
            file_path: h.file_path,
            commit_count: h.commit_count,
            author_count: h.author_count,
            score: h.score,
        })
        .collect();

    Ok(Json(HotspotsResponse {
        hotspots: items,
        total,
    }))
}

// ---------------------------------------------------------------------------
// GET /complexity/*file
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ComplexityQuery {
    #[serde(default = "default_min_complexity")]
    pub min_complexity: usize,
}

fn default_min_complexity() -> usize {
    1
}

#[derive(Debug, Serialize)]
pub struct ComplexityResponse {
    pub file: String,
    pub functions: Vec<ComplexityItem>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct ComplexityItem {
    pub name: String,
    pub cyclomatic_complexity: usize,
    pub risk: &'static str,
    pub line_start: usize,
    pub line_end: usize,
}

pub async fn complexity_handler(
    State(state): State<AppState>,
    Path(file): Path<String>,
    Query(params): Query<ComplexityQuery>,
) -> Result<Json<ComplexityResponse>, ApiError> {
    let engine = state.read().await;
    let file = resolve_repo_path(&engine, &file)?;

    let source = fs::read_to_string(&file.absolute)
        .map_err(|e| ApiError::BadRequest(format!("cannot read '{}': {e}", file.relative)))?;

    let syms = engine.symbols("", Some(&file.relative))?;
    let mut fns: Vec<_> = syms
        .iter()
        .filter(|s| matches!(s.kind, EntityKind::Function | EntityKind::Method))
        .collect();
    fns.sort_by_key(|s| s.line_start);

    let lines: Vec<&str> = source.lines().collect();

    let mut functions: Vec<ComplexityItem> = fns
        .iter()
        .map(|s| {
            let cc = count_cyclomatic_complexity(&lines, s.line_start, s.line_end);
            ComplexityItem {
                name: s.name.clone(),
                cyclomatic_complexity: cc,
                risk: risk_band(cc),
                line_start: s.line_start,
                line_end: s.line_end,
            }
        })
        .filter(|item| item.cyclomatic_complexity >= params.min_complexity)
        .collect();

    functions.sort_by_key(|b| std::cmp::Reverse(b.cyclomatic_complexity));

    let total = functions.len();
    Ok(Json(ComplexityResponse {
        file: file.relative,
        functions,
        total,
    }))
}

// ---------------------------------------------------------------------------
// GET /outline/*file
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct OutlineResponse {
    pub file: String,
    pub symbols: Vec<OutlineItem>,
    pub total: usize,
}

#[derive(Debug, Serialize)]
pub struct OutlineItem {
    pub name: String,
    pub kind: String,
    pub line_start: usize,
    pub line_end: usize,
    pub scope: Vec<String>,
    pub signature: Option<String>,
}

pub async fn outline_handler(
    State(state): State<AppState>,
    Path(file): Path<String>,
) -> Result<Json<OutlineResponse>, ApiError> {
    let engine = state.read().await;
    let file = resolve_repo_path(&engine, &file)?;
    let mut syms = engine.symbols("", Some(&file.relative))?;
    syms.sort_by_key(|s| s.line_start);

    let total = syms.len();
    let symbols = syms
        .into_iter()
        .map(|s| OutlineItem {
            name: s.name,
            kind: format!("{:?}", s.kind),
            line_start: s.line_start,
            line_end: s.line_end,
            scope: s.scope,
            signature: s.signature,
        })
        .collect();

    Ok(Json(OutlineResponse {
        file: file.relative,
        symbols,
        total,
    }))
}
