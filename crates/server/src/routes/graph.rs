use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Query, State};
use axum::response::Html;
use serde::{Deserialize, Serialize};

use codixing_core::language::detect_language;
use codixing_core::{EdgeKind, Engine, RepoMapOptions, Symbol};

use crate::error::ApiError;
use crate::state::AppState;

const GRAPH_VIEW_HTML: &str = include_str!("../../assets/graph_viewer.html");

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RefreshMode {
    #[default]
    None,
    Sync,
    Git,
}

fn parse_refresh_mode(value: Option<&str>) -> Result<RefreshMode, ApiError> {
    match value.unwrap_or("none").trim().to_ascii_lowercase().as_str() {
        "" | "none" => Ok(RefreshMode::None),
        "sync" => Ok(RefreshMode::Sync),
        "git" => Ok(RefreshMode::Git),
        other => Err(ApiError::BadRequest(format!(
            "invalid refresh mode `{other}`; expected one of: none, sync, git"
        ))),
    }
}

fn default_depth() -> usize {
    1
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

fn default_include_external() -> bool {
    true
}

fn default_symbol_limit() -> usize {
    4
}

fn default_commit_limit() -> usize {
    18
}

fn default_include_files() -> bool {
    true
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn clamp_limit(limit: usize, max: usize) -> usize {
    limit.max(1).min(max)
}

fn normalize_git_status(status: &str) -> String {
    match status.chars().next().unwrap_or('M') {
        'A' => "added".to_string(),
        'M' => "modified".to_string(),
        'D' => "deleted".to_string(),
        'R' => "renamed".to_string(),
        'C' => "copied".to_string(),
        'T' => "type-changed".to_string(),
        other => other.to_string(),
    }
}

fn display_label(path: &str, external: bool) -> String {
    if external {
        return path
            .trim_start_matches("__ext__:")
            .split(['/', ':'])
            .find(|part| !part.is_empty())
            .unwrap_or("external")
            .to_string();
    }
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Top-level directory names that are too generic to form meaningful cluster boundaries.
/// When a path starts with one of these, the second segment is used instead.
const TRANSPARENT_DIRS: &[&str] = &["src", "lib", "pkg", "source", "sources", "code"];

fn cluster_for_path(path: &str, external: bool) -> String {
    if external {
        return "external".to_string();
    }

    let mut parts = path.split('/').filter(|part| !part.is_empty());
    match (parts.next(), parts.next()) {
        (Some("crates"), Some(name)) => format!("crate:{name}"),
        (Some("packages"), Some(name)) => format!("package:{name}"),
        (Some("apps"), Some(name)) => format!("app:{name}"),
        (Some(first), Some(second)) if TRANSPARENT_DIRS.contains(&first) => second.to_string(),
        (Some(first), _) => first.to_string(),
        _ => "root".to_string(),
    }
}

fn git_stdout(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GraphRefreshResponse {
    pub mode: String,
    pub changed: bool,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub unchanged: usize,
}

impl GraphRefreshResponse {
    fn none() -> Self {
        Self {
            mode: "none".to_string(),
            changed: false,
            added: 0,
            modified: 0,
            removed: 0,
            unchanged: 0,
        }
    }
}

fn apply_refresh(engine: &mut Engine, mode: RefreshMode) -> Result<GraphRefreshResponse, ApiError> {
    match mode {
        RefreshMode::None => Ok(GraphRefreshResponse::none()),
        RefreshMode::Sync => {
            let stats = engine.sync()?;
            Ok(GraphRefreshResponse {
                mode: "sync".to_string(),
                changed: stats.added + stats.modified + stats.removed > 0,
                added: stats.added,
                modified: stats.modified,
                removed: stats.removed,
                unchanged: stats.unchanged,
            })
        }
        RefreshMode::Git => {
            let stats = engine.git_sync()?;
            Ok(GraphRefreshResponse {
                mode: "git".to_string(),
                changed: !stats.unchanged && (stats.modified + stats.removed > 0),
                added: 0,
                modified: stats.modified,
                removed: stats.removed,
                unchanged: usize::from(stats.unchanged),
            })
        }
    }
}

#[derive(Debug, Serialize)]
struct GitRepoInfo {
    branch: Option<String>,
    head_commit: Option<String>,
}

fn git_repo_info(root: &Path) -> GitRepoInfo {
    GitRepoInfo {
        branch: git_stdout(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        head_commit: git_stdout(root, &["rev-parse", "HEAD"]),
    }
}

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
        engine.transitive_callers(&params.file, params.depth)
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
    let files = if params.depth <= 1 {
        engine.callees(&params.file)
    } else {
        engine.transitive_callees(&params.file, params.depth)
    };
    let count = files.len();
    Ok(Json(FilesResponse { files, count }))
}

// ---------------------------------------------------------------------------
// GET /graph/stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct GraphStatsResponse {
    pub available: bool,
    pub node_count: usize,
    pub edge_count: usize,
    pub resolved_edges: usize,
    pub external_edges: usize,
    pub call_edges: usize,
}

fn graph_stats_response(engine: &Engine) -> GraphStatsResponse {
    match engine.graph_stats() {
        Some(s) => GraphStatsResponse {
            available: true,
            node_count: s.node_count,
            edge_count: s.edge_count,
            resolved_edges: s.resolved_edges,
            external_edges: s.external_edges,
            call_edges: s.call_edges,
        },
        None => GraphStatsResponse {
            available: false,
            node_count: 0,
            edge_count: 0,
            resolved_edges: 0,
            external_edges: 0,
            call_edges: 0,
        },
    }
}

pub async fn stats_handler(
    State(state): State<AppState>,
) -> Result<Json<GraphStatsResponse>, ApiError> {
    let engine = state.read().await;
    Ok(Json(graph_stats_response(&engine)))
}

// ---------------------------------------------------------------------------
// GET /graph/export
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GraphExportQuery {
    #[serde(default)]
    pub refresh: Option<String>,
    #[serde(default = "default_include_external")]
    pub include_external: bool,
    #[serde(default = "default_symbol_limit")]
    pub symbol_limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphSymbolPreview {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphNodeResponse {
    pub id: String,
    pub label: String,
    pub path: String,
    pub language: String,
    pub cluster: String,
    pub external: bool,
    pub pagerank: f32,
    pub in_degree: usize,
    pub out_degree: usize,
    pub symbol_count: usize,
    pub symbols: Vec<GraphSymbolPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphEdgeResponse {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub label: String,
    pub external: bool,
}

#[derive(Debug, Serialize)]
pub struct GraphSnapshotResponse {
    pub available: bool,
    pub root: String,
    pub branch: Option<String>,
    pub head_commit: Option<String>,
    pub generated_at_ms: u128,
    pub refresh: GraphRefreshResponse,
    pub graph: GraphStatsResponse,
    pub visible_node_count: usize,
    pub visible_edge_count: usize,
    pub nodes: Vec<GraphNodeResponse>,
    pub edges: Vec<GraphEdgeResponse>,
}

#[derive(Debug, Clone, Default)]
struct SymbolSummary {
    count: usize,
    previews: Vec<GraphSymbolPreview>,
}

fn build_symbol_summaries(symbols: Vec<Symbol>, limit: usize) -> HashMap<String, SymbolSummary> {
    let mut by_file: HashMap<String, Vec<Symbol>> = HashMap::new();
    for symbol in symbols {
        by_file
            .entry(symbol.file_path.clone())
            .or_default()
            .push(symbol);
    }

    by_file
        .into_iter()
        .map(|(file, mut file_symbols)| {
            file_symbols.sort_by(|a, b| {
                a.line_start
                    .cmp(&b.line_start)
                    .then_with(|| a.name.cmp(&b.name))
            });
            let previews = if limit == 0 {
                Vec::new()
            } else {
                file_symbols
                    .iter()
                    .take(limit)
                    .map(|symbol| GraphSymbolPreview {
                        name: symbol.name.clone(),
                        kind: symbol.kind.to_string(),
                        line: symbol.line_start + 1,
                        signature: symbol.signature.clone(),
                    })
                    .collect()
            };
            (
                file,
                SymbolSummary {
                    count: file_symbols.len(),
                    previews,
                },
            )
        })
        .collect()
}

fn ensure_external_node(nodes: &mut BTreeMap<String, GraphNodeResponse>, path: &str) {
    nodes
        .entry(path.to_string())
        .or_insert_with(|| GraphNodeResponse {
            id: path.to_string(),
            label: display_label(path, true),
            path: path.to_string(),
            language: "External".to_string(),
            cluster: cluster_for_path(path, true),
            external: true,
            pagerank: 0.0,
            in_degree: 0,
            out_degree: 0,
            symbol_count: 0,
            symbols: Vec::new(),
        });
}

fn build_graph_snapshot_payload(
    engine: &Engine,
    include_external: bool,
    symbol_limit: usize,
    refresh: GraphRefreshResponse,
) -> GraphSnapshotResponse {
    let root = engine.config().root.display().to_string();
    let git = git_repo_info(&engine.config().root);
    let graph = graph_stats_response(engine);

    let Some(data) = engine.graph_data() else {
        return GraphSnapshotResponse {
            available: false,
            root,
            branch: git.branch,
            head_commit: git.head_commit,
            generated_at_ms: now_unix_ms(),
            refresh,
            graph,
            visible_node_count: 0,
            visible_edge_count: 0,
            nodes: Vec::new(),
            edges: Vec::new(),
        };
    };

    let symbol_summaries =
        build_symbol_summaries(engine.symbol_table().all_symbols(), symbol_limit);
    let mut nodes = BTreeMap::<String, GraphNodeResponse>::new();

    for node in &data.nodes {
        let summary = symbol_summaries
            .get(&node.file_path)
            .cloned()
            .unwrap_or_default();
        nodes.insert(
            node.file_path.clone(),
            GraphNodeResponse {
                id: node.file_path.clone(),
                label: display_label(&node.file_path, false),
                path: node.file_path.clone(),
                language: node.language.name().to_string(),
                cluster: cluster_for_path(&node.file_path, false),
                external: false,
                pagerank: node.pagerank,
                in_degree: 0,
                out_degree: 0,
                symbol_count: summary.count,
                symbols: summary.previews,
            },
        );
    }

    let mut degree_map = HashMap::<String, (usize, usize)>::new();
    let mut seen_edges = HashSet::<(String, String, String)>::new();
    let mut edges = Vec::<GraphEdgeResponse>::new();

    for (source, target, edge) in data.edges {
        let external = matches!(edge.kind, EdgeKind::External) || target.starts_with("__ext__:");
        if external && !include_external {
            continue;
        }

        if external {
            ensure_external_node(&mut nodes, &target);
        }

        let edge_kind = match edge.kind {
            EdgeKind::Resolved => "import",
            EdgeKind::External => "external",
            EdgeKind::Calls => "call",
        };
        let dedupe_key = (source.clone(), target.clone(), edge_kind.to_string());
        if !seen_edges.insert(dedupe_key) {
            continue;
        }

        degree_map.entry(source.clone()).or_default().1 += 1;
        degree_map.entry(target.clone()).or_default().0 += 1;

        edges.push(GraphEdgeResponse {
            source,
            target,
            kind: edge_kind.to_string(),
            label: edge.raw_import,
            external,
        });
    }

    for (path, (in_degree, out_degree)) in degree_map {
        if let Some(node) = nodes.get_mut(&path) {
            node.in_degree = in_degree;
            node.out_degree = out_degree;
        }
    }

    let mut nodes: Vec<GraphNodeResponse> = nodes.into_values().collect();
    nodes.sort_by(|a, b| {
        b.pagerank
            .partial_cmp(&a.pagerank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
    });

    GraphSnapshotResponse {
        available: true,
        root,
        branch: git.branch,
        head_commit: git.head_commit,
        generated_at_ms: now_unix_ms(),
        refresh,
        graph,
        visible_node_count: nodes.len(),
        visible_edge_count: edges.len(),
        nodes,
        edges,
    }
}

pub async fn export_handler(
    State(state): State<AppState>,
    Query(params): Query<GraphExportQuery>,
) -> Result<Json<GraphSnapshotResponse>, ApiError> {
    let refresh_mode = parse_refresh_mode(params.refresh.as_deref())?;
    let symbol_limit = params.symbol_limit.min(12);

    let response = match refresh_mode {
        RefreshMode::None => {
            let engine = state.read().await;
            build_graph_snapshot_payload(
                &engine,
                params.include_external,
                symbol_limit,
                GraphRefreshResponse::none(),
            )
        }
        _ => {
            let mut engine = state.write().await;
            let refresh = apply_refresh(&mut engine, refresh_mode)?;
            build_graph_snapshot_payload(&engine, params.include_external, symbol_limit, refresh)
        }
    };

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// GET /graph/history
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GraphHistoryQuery {
    #[serde(default = "default_commit_limit")]
    pub limit: usize,
    #[serde(default = "default_include_files")]
    pub include_files: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitFileResponse {
    pub status: String,
    pub path: String,
    pub previous_path: Option<String>,
    pub indexed: bool,
    pub cluster: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitEntryResponse {
    pub id: String,
    pub short_id: String,
    pub summary: String,
    pub author: String,
    pub authored_at: String,
    pub is_merge: bool,
    pub files_changed: usize,
    pub indexed_files_changed: usize,
    pub files: Vec<CommitFileResponse>,
}

#[derive(Debug, Serialize)]
pub struct GraphHistoryResponse {
    pub available: bool,
    pub root: String,
    pub branch: Option<String>,
    pub head_commit: Option<String>,
    pub commits: Vec<CommitEntryResponse>,
}

fn parse_commit_files(block: &str) -> Vec<CommitFileResponse> {
    block
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }

            let parts: Vec<&str> = trimmed.split('\t').collect();
            let (status, path, previous_path) = match parts.as_slice() {
                [status, path] => (*status, (*path).to_string(), None),
                [status, previous, path] if status.starts_with('R') || status.starts_with('C') => {
                    (*status, (*path).to_string(), Some((*previous).to_string()))
                }
                _ => return None,
            };

            Some(CommitFileResponse {
                status: normalize_git_status(status),
                indexed: detect_language(Path::new(&path)).is_some(),
                cluster: cluster_for_path(&path, false),
                path,
                previous_path,
            })
        })
        .collect()
}

fn build_commit_entry(
    header: &str,
    file_lines: &[String],
    include_files: bool,
) -> Option<CommitEntryResponse> {
    let fields: Vec<&str> = header.split('\x1f').collect();
    if fields.len() < 6 {
        return None;
    }

    let files = parse_commit_files(&file_lines.join("\n"));
    let indexed_files_changed = files.iter().filter(|file| file.indexed).count();
    Some(CommitEntryResponse {
        id: fields[0].to_string(),
        short_id: fields[1].to_string(),
        author: fields[2].to_string(),
        authored_at: fields[3].to_string(),
        summary: fields[4].to_string(),
        is_merge: !fields[5].trim().is_empty() && fields[5].split_whitespace().count() > 1,
        files_changed: files.len(),
        indexed_files_changed,
        files: if include_files { files } else { Vec::new() },
    })
}

fn parse_git_history(stdout: &str, include_files: bool) -> Vec<CommitEntryResponse> {
    let mut commits = Vec::new();
    let mut current_header: Option<String> = None;
    let mut current_files = Vec::<String>::new();

    for line in stdout.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.contains('\x1f') {
            if let Some(header) = current_header.take() {
                if let Some(commit) = build_commit_entry(&header, &current_files, include_files) {
                    commits.push(commit);
                }
                current_files.clear();
            }
            current_header = Some(trimmed.to_string());
        } else if current_header.is_some() {
            current_files.push(trimmed.to_string());
        }
    }

    if let Some(header) = current_header {
        if let Some(commit) = build_commit_entry(&header, &current_files, include_files) {
            commits.push(commit);
        }
    }

    commits
}

fn git_history(root: &Path, limit: usize, include_files: bool) -> Option<Vec<CommitEntryResponse>> {
    let pretty = "%H%x1f%h%x1f%an%x1f%ad%x1f%s%x1f%P";
    let limit = limit.to_string();
    let output = Command::new("git")
        .args([
            "log",
            "--date=iso-strict",
            &format!("--pretty=format:{pretty}"),
            "--name-status",
            "-n",
            &limit,
        ])
        .current_dir(root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(parse_git_history(&stdout, include_files))
}

pub async fn history_handler(
    State(state): State<AppState>,
    Query(params): Query<GraphHistoryQuery>,
) -> Result<Json<GraphHistoryResponse>, ApiError> {
    let engine = state.read().await;
    let root = engine.config().root.display().to_string();
    let git = git_repo_info(&engine.config().root);
    let limit = clamp_limit(params.limit, 100);

    let Some(commits) = git_history(&engine.config().root, limit, params.include_files) else {
        return Ok(Json(GraphHistoryResponse {
            available: false,
            root,
            branch: git.branch,
            head_commit: git.head_commit,
            commits: Vec::new(),
        }));
    };

    Ok(Json(GraphHistoryResponse {
        available: true,
        root,
        branch: git.branch,
        head_commit: git.head_commit,
        commits,
    }))
}

// ---------------------------------------------------------------------------
// GET /graph/call-graph
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CallGraphResponse {
    pub available: bool,
    pub edge_count: usize,
    pub edges: Vec<CallEdge>,
}

#[derive(Debug, Serialize)]
pub struct CallEdge {
    pub caller: String,
    pub callee: String,
}

pub async fn call_graph_handler(
    State(state): State<AppState>,
) -> Result<Json<CallGraphResponse>, ApiError> {
    let engine = state.read().await;
    match engine.graph_data() {
        Some(data) => {
            let edges: Vec<CallEdge> = data
                .edges
                .iter()
                .filter(|(_, _, e)| e.kind == EdgeKind::Calls)
                .map(|(from, _, e)| CallEdge {
                    caller: from.clone(),
                    callee: e.raw_import.clone(),
                })
                .collect();
            let edge_count = edges.len();
            Ok(Json(CallGraphResponse {
                available: true,
                edge_count,
                edges,
            }))
        }
        None => Ok(Json(CallGraphResponse {
            available: false,
            edge_count: 0,
            edges: vec![],
        })),
    }
}

// ---------------------------------------------------------------------------
// GET /graph/view
// ---------------------------------------------------------------------------

pub async fn view_handler() -> Html<&'static str> {
    Html(GRAPH_VIEW_HTML)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_mode_parser_accepts_supported_values() {
        assert_eq!(parse_refresh_mode(None).unwrap(), RefreshMode::None);
        assert_eq!(parse_refresh_mode(Some("sync")).unwrap(), RefreshMode::Sync);
        assert_eq!(parse_refresh_mode(Some("git")).unwrap(), RefreshMode::Git);
        assert!(parse_refresh_mode(Some("wat")).is_err());
    }

    #[test]
    fn cluster_prefers_crate_name() {
        assert_eq!(
            cluster_for_path("crates/server/src/main.rs", false),
            "crate:server"
        );
        assert_eq!(cluster_for_path("docs/index.html", false), "docs");
        assert_eq!(cluster_for_path("__ext__:tokio::sync", true), "external");
    }

    #[test]
    fn parse_git_history_handles_renames_and_merges() {
        let sample = concat!(
            "abcdef\x1fabc123\x1fAlice\x1f2026-03-06T10:00:00+00:00\x1fMerge feature atlas\x1fparent1 parent2\n",
            "R100\told.rs\tnew.rs\n",
            "M\tcrates/server/src/routes/graph.rs\n",
            "123456\x1f1234567\x1fAlice\x1f2026-03-05T10:00:00+00:00\x1fFollow-up\x1fparent1\n",
            "A\tcrates/server/src/main.rs\n"
        );

        let commits = parse_git_history(sample, true);
        assert_eq!(commits.len(), 2);
        assert!(commits[0].is_merge);
        assert_eq!(commits[0].files_changed, 2);
        assert_eq!(commits[0].files[0].status, "renamed");
        assert_eq!(commits[0].files[0].previous_path.as_deref(), Some("old.rs"));
        assert!(commits[0].files[1].indexed);
    }
}
