//! Cross-file context assembly: build minimal context for understanding a search result.
//!
//! Given a code location, assembles the matched chunk plus its import chain,
//! key callees, and usage examples — all within a configurable token budget.

use std::collections::HashSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::engine::examples::UsageExample;
use crate::error::Result;
use crate::retriever::{DocFilter, SearchQuery, SearchResult, Strategy};

use super::Engine;

/// How the assembler treats `token_budget`.
///
/// `Soft` (default) preserves the legacy behavior: the primary chunk is
/// emitted in full even if it exceeds the budget on its own, and only the
/// derived imports/callees/examples are bounded. `Strict` enforces the
/// budget as a hard cap by slicing the primary chunk down to a window of
/// lines around the caller-supplied line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum BudgetMode {
    /// Budget is a target. Primary chunks larger than the budget are kept
    /// intact, and `AssembledContext::over_budget` is set so callers can
    /// surface the overshoot.
    #[default]
    Soft,
    /// Budget is a hard cap. The primary chunk is sliced around the focus
    /// line until the assembled context fits, even if that means dropping
    /// the surrounding function body.
    Strict,
}

impl BudgetMode {
    /// Stable string label used in CLI/MCP output.
    pub fn as_str(&self) -> &'static str {
        match self {
            BudgetMode::Soft => "soft",
            BudgetMode::Strict => "strict",
        }
    }
}

/// Agent workflow mode for [`AgentContextPack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentContextMode {
    /// Find relevant files and symbols only.
    Locate,
    /// Explain the repository context for the task.
    #[default]
    Understand,
    /// Prepare for a code edit.
    Edit,
    /// Review a diff or proposed change.
    Review,
    /// Select likely tests and verification paths.
    Test,
    /// Plan a cross-cutting migration.
    Migrate,
    /// Debug a production-like incident.
    Incident,
}

impl AgentContextMode {
    /// Stable string label used in CLI/MCP input and JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Locate => "locate",
            Self::Understand => "understand",
            Self::Edit => "edit",
            Self::Review => "review",
            Self::Test => "test",
            Self::Migrate => "migrate",
            Self::Incident => "incident",
        }
    }
}

impl FromStr for AgentContextMode {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "locate" => Ok(Self::Locate),
            "understand" => Ok(Self::Understand),
            "edit" => Ok(Self::Edit),
            "review" => Ok(Self::Review),
            "test" => Ok(Self::Test),
            "migrate" => Ok(Self::Migrate),
            "incident" => Ok(Self::Incident),
            other => Err(format!(
                "invalid mode '{other}' (expected locate, understand, edit, review, test, migrate, or incident)"
            )),
        }
    }
}

/// Stable JSON context pack for AI agents.
///
/// This schema intentionally ships evidence handles rather than full source
/// text. Agents can ask Codixing for `read_file(E1)`/`read_symbol(...)` after
/// receiving this pack, which keeps the first context assembly call compact and
/// reproducible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextPack {
    /// Schema version for clients that cache or validate tool output.
    pub schema_version: u32,
    /// Task text supplied by the caller.
    pub task_summary: String,
    /// Workflow mode used to tune ranking and next-tool recommendations.
    pub mode: AgentContextMode,
    /// Caller-provided token budget for this context pack.
    pub token_budget: usize,
    /// Optional branch/worktree label supplied by the caller.
    pub branch: Option<String>,
    /// Optional caller-provided risk label (for example, low/medium/high).
    pub risk_level: Option<String>,
    /// Files supplied as changed or task-local anchors.
    pub changed_files: Vec<String>,
    /// Ranked file-level orientation.
    pub repo_orientation: Vec<AgentContextOrientation>,
    /// Evidence handles agents should read first.
    pub must_read: Vec<AgentContextEvidence>,
    /// Related symbols discovered from evidence files.
    pub related_symbols: Vec<AgentContextSymbol>,
    /// Likely tests for the task-local files.
    pub likely_tests: Vec<AgentContextTest>,
    /// Documentation and convention files relevant to the task.
    pub docs_and_conventions: Vec<AgentContextDocument>,
    /// Risks or guardrails inferred from the pack.
    pub risks: Vec<String>,
    /// Suggested follow-up Codixing tools.
    pub recommended_next_tools: Vec<String>,
    /// Estimated tokens for this JSON payload.
    pub total_estimated_tokens: usize,
    /// True when low-priority sections were dropped to fit the budget.
    pub truncated: bool,
}

/// File-level orientation for an agent context pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextOrientation {
    pub id: String,
    pub path: String,
    pub why: String,
    pub symbols: Vec<String>,
}

/// A compact evidence handle that can be expanded by later tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextEvidence {
    pub id: String,
    pub path: String,
    pub range: String,
    pub start_line: u64,
    pub end_line: u64,
    pub kind: String,
    pub reason: String,
    pub score: f32,
    pub signature: Option<String>,
}

/// Related symbol metadata for task-local files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextSymbol {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub range: String,
    pub signature: Option<String>,
}

/// Test mapping surfaced by an agent context pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextTest {
    pub path: String,
    pub source_path: String,
    pub confidence: f32,
    pub reason: String,
}

/// Documentation or convention evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContextDocument {
    pub id: String,
    pub path: String,
    pub range: String,
    pub title: Option<String>,
    pub reason: String,
}

/// Assembled cross-file context for a code location.
#[derive(Debug, Clone, Serialize)]
pub struct AssembledContext {
    /// The primary search result (the matched chunk).
    pub primary: SearchResult,
    /// Import chain: signatures of types/functions from dependency files.
    pub imports: Vec<ContextSnippet>,
    /// Key callees: signatures of functions called by the primary entity.
    pub callees: Vec<ContextSnippet>,
    /// Usage examples from tests, call sites, and doc blocks.
    pub examples: Vec<UsageExample>,
    /// Total estimated token count of the assembled context.
    pub total_tokens: usize,
    /// Budget the caller asked for, before any soft/strict adjustment.
    pub requested_budget: usize,
    /// Whether the budget was treated as a target (`Soft`) or hard cap
    /// (`Strict`).
    pub budget_mode: BudgetMode,
    /// True when `total_tokens > requested_budget`. Always false for
    /// `Strict` unless the minimum 1-line slice still overshoots.
    pub over_budget: bool,
    /// Human-readable explanation when the assembled context could not be
    /// shrunk further. `None` when the budget is satisfied.
    pub oversize_reason: Option<String>,
}

/// A snippet of code from a related file, used as context.
#[derive(Debug, Clone, Serialize)]
pub struct ContextSnippet {
    /// File path of the snippet.
    pub file_path: String,
    /// Start line (0-indexed).
    pub line_start: usize,
    /// End line (0-indexed).
    pub line_end: usize,
    /// The source code content.
    pub content: String,
    /// Relevance score (0.0-1.0).
    pub relevance: f32,
}

/// Simple token count estimator (~4 chars per token for code).
pub fn estimate_token_count(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// In-place shrink the primary chunk to a line window centered on `focus_line`
/// such that the resulting content fits within ~80% of `budget` (leaving room
/// for the imports/callees/examples sections).
///
/// `focus_line` is the absolute file line (0-indexed) the caller asked about.
/// The window is grown symmetrically until adding more lines would exceed the
/// budget. Always preserves at least one line — the focus line itself.
fn shrink_primary_to_window(
    primary: &mut SearchResult,
    file: &str,
    focus_line: u64,
    budget: usize,
    engine: &Engine,
) {
    // Reserve ~20% of the budget for the dependent sections so a strict
    // budget still produces useful import/callee context. Floor at 1
    // token so callers passing tiny budgets still get a hard cap (a 64-
    // token floor previously let strict mode silently exceed budgets
    // smaller than 80 tokens).
    let primary_target = budget.saturating_sub(budget / 5).max(1);

    // Re-read the file as raw lines so we can grow a symmetric window.
    let abs = engine
        .config
        .resolve_path(file)
        .unwrap_or_else(|| engine.config.root.join(file));
    let file_text = match std::fs::read_to_string(&abs) {
        Ok(t) => t,
        Err(_) => return,
    };
    let lines: Vec<&str> = file_text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return;
    }
    let focus = (focus_line as usize).min(total.saturating_sub(1));

    let mut start = focus;
    let mut end = focus;
    let mut content = lines[focus].to_string();
    let mut tokens = estimate_token_count(&content);

    loop {
        let can_grow_up = start > 0;
        let can_grow_down = end + 1 < total;
        if !can_grow_up && !can_grow_down {
            break;
        }

        // Pick the side that grows the smaller token jump first.
        let up_cost = if can_grow_up {
            estimate_token_count(lines[start - 1]) + 1
        } else {
            usize::MAX
        };
        let down_cost = if can_grow_down {
            estimate_token_count(lines[end + 1]) + 1
        } else {
            usize::MAX
        };
        let pick_up = up_cost <= down_cost;

        let next_tokens = if pick_up {
            tokens + up_cost
        } else {
            tokens + down_cost
        };
        if next_tokens > primary_target {
            break;
        }

        if pick_up {
            start -= 1;
        } else {
            end += 1;
        }
        content = lines[start..=end].join("\n");
        tokens = next_tokens;
    }

    primary.line_start = start as u64;
    primary.line_end = end as u64;
    primary.content = content;
}

fn human_result_range(start: u64, end: u64) -> (u64, u64, String) {
    let human_start = start + 1;
    let human_end = if end <= start { human_start } else { end };
    (
        human_start,
        human_end,
        format!("L{human_start}-L{human_end}"),
    )
}

fn human_symbol_range(start: usize, end: usize) -> String {
    let human_start = start + 1;
    let human_end = end + 1;
    format!("L{human_start}-L{human_end}")
}

fn result_kind(result: &SearchResult) -> &'static str {
    if result.is_doc() {
        "documentation"
    } else if result.file_path.contains("/test")
        || result.file_path.contains("/tests/")
        || result.file_path.contains("__tests__")
        || result.file_path.ends_with("_test.rs")
        || result.file_path.ends_with("_test.go")
        || result.file_path.ends_with(".test.ts")
        || result.file_path.ends_with(".test.tsx")
        || result.file_path.ends_with(".spec.ts")
        || result.file_path.ends_with(".spec.tsx")
    {
        "test"
    } else {
        "implementation"
    }
}

fn doc_title(result: &SearchResult) -> Option<String> {
    result
        .content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| {
            line.trim_start_matches('#')
                .trim()
                .chars()
                .take(100)
                .collect()
        })
}

fn search_limit_for_mode(mode: AgentContextMode, token_budget: usize) -> usize {
    let mode_limit = match mode {
        AgentContextMode::Locate => 8,
        AgentContextMode::Understand => 10,
        AgentContextMode::Edit => 12,
        AgentContextMode::Review => 14,
        AgentContextMode::Test => 12,
        AgentContextMode::Migrate => 16,
        AgentContextMode::Incident => 14,
    };
    mode_limit.min((token_budget / 300).clamp(4, 16))
}

fn update_pack_token_estimate(pack: &mut AgentContextPack) {
    pack.total_estimated_tokens = 0;
    pack.total_estimated_tokens = serde_json::to_string(pack)
        .map(|json| estimate_token_count(&json))
        .unwrap_or(0);
}

fn enforce_agent_pack_budget(pack: &mut AgentContextPack) {
    update_pack_token_estimate(pack);

    while pack.total_estimated_tokens > pack.token_budget {
        let dropped = pack.docs_and_conventions.pop().is_some()
            || pack.related_symbols.pop().is_some()
            || pack.likely_tests.pop().is_some()
            || pack.repo_orientation.pop().is_some()
            || pack.must_read.pop().is_some();

        if !dropped {
            break;
        }
        pack.truncated = true;
        update_pack_token_estimate(pack);
    }
}

impl Engine {
    /// Build a stable JSON context pack for AI agents from a task description.
    ///
    /// The returned pack contains ranked file orientation, evidence handles,
    /// related symbols, likely tests, docs/conventions, risks, and suggested
    /// next tools. Source bodies are intentionally omitted; agents should expand
    /// evidence IDs with follow-up read tools.
    pub fn agent_context_pack(
        &self,
        task: &str,
        mode: AgentContextMode,
        token_budget: usize,
        changed_files: &[String],
        branch: Option<String>,
        risk_level: Option<String>,
    ) -> Result<AgentContextPack> {
        let token_budget = token_budget.max(256);
        let limit = search_limit_for_mode(mode, token_budget);
        let strategy = match mode {
            AgentContextMode::Locate => Strategy::Instant,
            _ => self.detect_strategy(task),
        };

        let results = self.search(
            SearchQuery::new(task)
                .with_limit(limit)
                .with_strategy(strategy)
                .with_doc_filter(DocFilter::CodeOnly),
        )?;

        let mut selected = Vec::new();
        let mut seen_locations = HashSet::new();

        for file in changed_files {
            if let Some(result) = results.iter().find(|r| r.file_path == *file).cloned() {
                let key = format!(
                    "{}:{}:{}",
                    result.file_path, result.line_start, result.line_end
                );
                if seen_locations.insert(key) {
                    selected.push(result);
                }
                continue;
            }

            if let Some(result) = self.agent_context_file_anchor(file)? {
                let key = format!(
                    "{}:{}:{}",
                    result.file_path, result.line_start, result.line_end
                );
                if seen_locations.insert(key) {
                    selected.push(result);
                }
            }
        }

        for result in results {
            let key = format!(
                "{}:{}:{}",
                result.file_path, result.line_start, result.line_end
            );
            if seen_locations.insert(key) {
                selected.push(result);
            }
        }

        let changed_set: HashSet<&str> = changed_files.iter().map(String::as_str).collect();
        let mut must_read = Vec::new();
        for result in selected.iter().take(limit) {
            let (start_line, end_line, range) =
                human_result_range(result.line_start, result.line_end);
            let reason = if changed_set.contains(result.file_path.as_str()) {
                "Caller marked this file as changed or task-local".to_string()
            } else {
                format!("Ranked {:.2} for the task query", result.score)
            };
            must_read.push(AgentContextEvidence {
                id: format!("E{}", must_read.len() + 1),
                path: result.file_path.clone(),
                range,
                start_line,
                end_line,
                kind: result_kind(result).to_string(),
                reason,
                score: result.score,
                signature: if result.signature.trim().is_empty() {
                    None
                } else {
                    Some(result.signature.clone())
                },
            });
        }

        let mut repo_orientation = Vec::new();
        let mut seen_files = HashSet::new();
        for evidence in &must_read {
            if !seen_files.insert(evidence.path.clone()) {
                continue;
            }
            let mut symbols = self.symbols.filter("", Some(&evidence.path));
            symbols.sort_by_key(|s| s.line_start);
            let symbols = symbols
                .into_iter()
                .take(6)
                .map(|s| {
                    s.signature
                        .unwrap_or_else(|| format!("{} {}", s.kind, s.name))
                })
                .collect();
            let why = if changed_set.contains(evidence.path.as_str()) {
                "Changed-file anchor for this task".to_string()
            } else {
                format!("Contains evidence handle {}", evidence.id)
            };
            repo_orientation.push(AgentContextOrientation {
                id: format!("O{}", repo_orientation.len() + 1),
                path: evidence.path.clone(),
                why,
                symbols,
            });
        }

        let mut related_symbols = Vec::new();
        let mut seen_symbols = HashSet::new();
        for evidence in &must_read {
            let mut symbols = self.symbols.filter("", Some(&evidence.path));
            symbols.sort_by_key(|s| s.line_start);
            for symbol in symbols.into_iter().take(8) {
                let key = format!("{}:{}:{}", symbol.file_path, symbol.name, symbol.line_start);
                if !seen_symbols.insert(key) {
                    continue;
                }
                related_symbols.push(AgentContextSymbol {
                    name: symbol.name,
                    kind: symbol.kind.to_string(),
                    path: symbol.file_path,
                    range: human_symbol_range(symbol.line_start, symbol.line_end),
                    signature: symbol.signature,
                });
                if related_symbols.len() >= 16 {
                    break;
                }
            }
            if related_symbols.len() >= 16 {
                break;
            }
        }

        let mut likely_tests = Vec::new();
        let mut seen_tests = HashSet::new();
        for path in changed_files
            .iter()
            .map(String::as_str)
            .chain(repo_orientation.iter().map(|o| o.path.as_str()))
        {
            for mapping in self.find_tests_for_file(path).into_iter().take(4) {
                let key = format!("{}:{}", mapping.source_file, mapping.test_file);
                if seen_tests.insert(key) {
                    likely_tests.push(AgentContextTest {
                        path: mapping.test_file,
                        source_path: mapping.source_file,
                        confidence: mapping.confidence,
                        reason: mapping.reason,
                    });
                }
                if likely_tests.len() >= 12 {
                    break;
                }
            }
            if likely_tests.len() >= 12 {
                break;
            }
        }

        let docs = self
            .search(
                SearchQuery::new(task)
                    .with_limit(4)
                    .with_strategy(Strategy::Instant)
                    .with_doc_filter(DocFilter::DocsOnly),
            )
            .unwrap_or_default();
        let docs_and_conventions = docs
            .iter()
            .enumerate()
            .map(|(idx, result)| {
                let (_, _, range) = human_result_range(result.line_start, result.line_end);
                AgentContextDocument {
                    id: format!("D{}", idx + 1),
                    path: result.file_path.clone(),
                    range,
                    title: doc_title(result),
                    reason: "Relevant documentation or repository convention".to_string(),
                }
            })
            .collect();

        let mut risks = Vec::new();
        if matches!(
            mode,
            AgentContextMode::Edit | AgentContextMode::Migrate | AgentContextMode::Incident
        ) {
            risks.push("Run change_impact before editing each implementation file.".to_string());
        }
        if matches!(mode, AgentContextMode::Review) {
            risks.push(
                "Compare this pack against the actual diff before drawing review conclusions."
                    .to_string(),
            );
        }
        if !likely_tests.is_empty() {
            risks.push("Run the likely_tests set before finalizing changes.".to_string());
        }
        if must_read.iter().any(|e| {
            e.path.contains("mcp")
                || e.path.contains("cli")
                || e.path.contains("api")
                || e.path.contains("server")
        }) {
            risks.push(
                "Some evidence touches a user-facing API surface; check docs and compatibility."
                    .to_string(),
            );
        }
        if let Some(level) = risk_level.as_deref() {
            if matches!(level.to_ascii_lowercase().as_str(), "high" | "critical") {
                risks.push(
                    "High risk level requested; prefer patch preview, impact analysis, and full tests."
                        .to_string(),
                );
            }
        }

        let first_path = must_read
            .first()
            .map(|e| e.path.as_str())
            .unwrap_or("<path>");
        let mut recommended_next_tools = vec!["read_file(E1)".to_string()];
        match mode {
            AgentContextMode::Locate => {
                recommended_next_tools.push("find_symbol(<symbol>)".to_string());
                recommended_next_tools.push("code_search(<refined query>)".to_string());
            }
            AgentContextMode::Understand => {
                recommended_next_tools.push(format!("assemble_location_context({first_path})"));
                recommended_next_tools.push("get_repo_map".to_string());
            }
            AgentContextMode::Edit | AgentContextMode::Migrate => {
                recommended_next_tools.push(format!("change_impact({first_path})"));
                recommended_next_tools.push(format!("find_tests(file={first_path})"));
                recommended_next_tools.push(format!("api_surface({first_path})"));
            }
            AgentContextMode::Review => {
                recommended_next_tools.push("review_context(<diff>)".to_string());
                recommended_next_tools.push(format!("change_impact({first_path})"));
            }
            AgentContextMode::Test => {
                recommended_next_tools.push(format!("find_tests(file={first_path})"));
                recommended_next_tools.push("find_source_for_test(<test file>)".to_string());
            }
            AgentContextMode::Incident => {
                recommended_next_tools.push(format!("change_impact({first_path})"));
                recommended_next_tools.push("search_changes(<symptom>)".to_string());
                recommended_next_tools.push("get_hotspots".to_string());
            }
        }

        let mut pack = AgentContextPack {
            schema_version: 1,
            task_summary: task.to_string(),
            mode,
            token_budget,
            branch,
            risk_level,
            changed_files: changed_files.to_vec(),
            repo_orientation,
            must_read,
            related_symbols,
            likely_tests,
            docs_and_conventions,
            risks,
            recommended_next_tools,
            total_estimated_tokens: 0,
            truncated: false,
        };
        enforce_agent_pack_budget(&mut pack);
        Ok(pack)
    }

    fn agent_context_file_anchor(&self, file: &str) -> Result<Option<SearchResult>> {
        let mut symbols = self.symbols.filter("", Some(file));
        symbols.sort_by_key(|s| s.line_start);

        if let Some(symbol) = symbols.into_iter().next() {
            let content = self
                .read_file_range(
                    file,
                    Some(symbol.line_start as u64),
                    Some(symbol.line_end as u64),
                )?
                .unwrap_or_default();
            return Ok(Some(SearchResult {
                chunk_id: format!("agent-anchor:{file}:{}", symbol.line_start),
                file_path: file.to_string(),
                language: symbol.language.name().to_string(),
                score: 1.0,
                line_start: symbol.line_start as u64,
                line_end: symbol.line_end as u64 + 1,
                signature: symbol.signature.clone().unwrap_or_default(),
                scope_chain: symbol.scope.clone(),
                content,
            }));
        }

        let Some(content) = self.read_file_range(file, None, Some(80))? else {
            return Ok(None);
        };
        let line_count = content.lines().count().max(1) as u64;
        Ok(Some(SearchResult {
            chunk_id: format!("agent-anchor:{file}:0"),
            file_path: file.to_string(),
            language: String::new(),
            score: 1.0,
            line_start: 0,
            line_end: line_count,
            signature: String::new(),
            scope_chain: Vec::new(),
            content,
        }))
    }

    /// Assemble cross-file context for a code location specified by file path and line.
    ///
    /// This is the primary entry point for the CLI and MCP tool. It constructs
    /// a minimal `SearchResult` for the location and delegates to `assemble_context`.
    ///
    /// Backwards-compatible shim: defaults to `BudgetMode::Soft` so existing
    /// callers see the legacy "primary chunk emitted in full" behavior.
    pub fn assemble_context_for_location(
        &self,
        file: &str,
        line: u64,
        token_budget: usize,
    ) -> AssembledContext {
        self.assemble_context_for_location_with_mode(file, line, token_budget, BudgetMode::Soft)
    }

    /// Assemble cross-file context for a code location with explicit budget mode.
    pub fn assemble_context_for_location_with_mode(
        &self,
        file: &str,
        line: u64,
        token_budget: usize,
        mode: BudgetMode,
    ) -> AssembledContext {
        // Read the file content around the target line to construct a primary result.
        let content = self
            .read_file_range(file, Some(line), Some(line.saturating_add(30)))
            .ok()
            .flatten()
            .unwrap_or_default();

        // Find a symbol that overlaps this line for better context.
        let symbols = self.symbols.filter("", Some(file));
        let overlapping = symbols.iter().find(|s| {
            let start = s.line_start as u64;
            let end = s.line_end as u64;
            line >= start && line <= end
        });

        let (line_start, line_end, signature, content) = if let Some(sym) = overlapping {
            let sym_content = self
                .read_file_range(file, Some(sym.line_start as u64), Some(sym.line_end as u64))
                .ok()
                .flatten()
                .unwrap_or(content);
            (
                sym.line_start as u64,
                sym.line_end as u64,
                sym.signature.clone().unwrap_or_default(),
                sym_content,
            )
        } else {
            (line, line.saturating_add(30), String::new(), content)
        };

        let mut primary = SearchResult {
            chunk_id: format!("{file}:{line_start}"),
            file_path: file.to_string(),
            language: String::new(),
            score: 1.0,
            line_start,
            line_end,
            signature,
            scope_chain: Vec::new(),
            content,
        };

        // Strict mode: shrink the primary chunk to a window around `line`
        // before letting `assemble_context` allocate the imports/callees
        // budget. Without this the primary chunk alone can exceed the
        // budget and the rest of the context degenerates to nothing.
        if mode == BudgetMode::Strict
            && estimate_token_count(&primary.content) > token_budget.saturating_sub(64)
        {
            shrink_primary_to_window(&mut primary, file, line, token_budget, self);
        }

        self.assemble_context_with_mode(&primary, token_budget, mode)
    }

    /// Assemble cross-file context for a search result.
    ///
    /// Budget allocation: 40% imports, 30% callees, 30% examples.
    ///
    /// 1. **Import chain (40%)**: From the dependency graph, find files imported
    ///    by the primary file. For each dependency, look up symbols whose names
    ///    appear in the primary chunk. Extract just their signatures.
    ///
    /// 2. **Key callees (30%)**: Find the primary entity in the chunk (symbol
    ///    whose line range overlaps). Use `symbol_callees_precise` to get callee
    ///    names, then look up their signatures from the symbol table.
    ///
    /// 3. **Usage examples (30%)**: From `find_usage_examples`. Fit within
    ///    remaining budget.
    pub fn assemble_context(&self, result: &SearchResult, token_budget: usize) -> AssembledContext {
        self.assemble_context_with_mode(result, token_budget, BudgetMode::Soft)
    }

    /// Assemble cross-file context with explicit `BudgetMode`.
    ///
    /// `Soft` keeps the primary chunk intact even when it exceeds `token_budget`
    /// and reports the overshoot via `AssembledContext::over_budget`.
    /// `Strict` expects the caller to have already shrunk the primary chunk
    /// (see `assemble_context_for_location_with_mode`).
    pub fn assemble_context_with_mode(
        &self,
        result: &SearchResult,
        token_budget: usize,
        mode: BudgetMode,
    ) -> AssembledContext {
        let primary_tokens = estimate_token_count(&result.content);
        let remaining = token_budget.saturating_sub(primary_tokens);

        let import_budget = remaining * 40 / 100;
        let callee_budget = remaining * 30 / 100;
        let example_budget = remaining * 30 / 100;

        // --- 1. Import chain ---
        let imports = self.gather_import_snippets(result, import_budget);

        // --- 2. Key callees ---
        let callees = self.gather_callee_snippets(result, callee_budget);

        // --- 3. Usage examples ---
        let examples = self.gather_usage_examples(result, example_budget);

        let total_tokens = primary_tokens
            + imports
                .iter()
                .map(|s| estimate_token_count(&s.content))
                .sum::<usize>()
            + callees
                .iter()
                .map(|s| estimate_token_count(&s.content))
                .sum::<usize>()
            + examples
                .iter()
                .map(|e| estimate_token_count(&e.context))
                .sum::<usize>();

        let over_budget = total_tokens > token_budget;
        let oversize_reason = if over_budget {
            // The only way the assembler exceeds the requested budget is when
            // the primary chunk is itself larger than the budget — derived
            // sections honor `import_budget`/`callee_budget`/`example_budget`.
            let span = result.line_end.saturating_sub(result.line_start) + 1;
            let hint = match mode {
                BudgetMode::Soft => {
                    "; pass --budget-mode strict to slice the primary chunk around --line N"
                }
                BudgetMode::Strict => {
                    "; the requested budget is below the minimum 1-line slice for this file"
                }
            };
            Some(format!(
                "primary chunk indivisible (function spans {span} line(s), {primary_tokens} tokens > budget {token_budget}){hint}",
            ))
        } else {
            None
        };

        AssembledContext {
            primary: result.clone(),
            imports,
            callees,
            examples,
            total_tokens,
            requested_budget: token_budget,
            budget_mode: mode,
            over_budget,
            oversize_reason,
        }
    }

    /// Gather import chain snippets from the dependency graph.
    ///
    /// For each file that the primary result's file imports, finds symbols
    /// whose names appear in the primary chunk content and extracts their
    /// signatures.
    fn gather_import_snippets(&self, result: &SearchResult, budget: usize) -> Vec<ContextSnippet> {
        let mut snippets = Vec::new();
        let mut used_tokens = 0;

        // Get dependency files from the graph.
        let dep_files = match &self.graph {
            Some(g) => g.callees(&result.file_path),
            None => return snippets,
        };

        for dep_file in &dep_files {
            if used_tokens >= budget {
                break;
            }

            // Find symbols in the dependency file that appear in the primary content.
            let dep_symbols = self.symbols.filter("", Some(dep_file.as_str()));
            for sym in &dep_symbols {
                if used_tokens >= budget {
                    break;
                }

                // Check if this symbol's name appears in the primary chunk.
                if !result.content.contains(&sym.name) {
                    continue;
                }

                // Use the signature if available, otherwise just the name.
                let content = sym
                    .signature
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| format!("{} {}", sym.kind, sym.name));

                let tokens = estimate_token_count(&content);
                if used_tokens + tokens > budget {
                    continue;
                }

                snippets.push(ContextSnippet {
                    file_path: sym.file_path.clone(),
                    line_start: sym.line_start,
                    line_end: sym.line_end,
                    content,
                    relevance: 0.8,
                });
                used_tokens += tokens;
            }
        }

        snippets
    }

    /// Gather callee signatures for the primary entity.
    ///
    /// Finds the symbol whose line range overlaps the primary chunk, then
    /// uses `symbol_callees_precise` to find what it calls.
    fn gather_callee_snippets(&self, result: &SearchResult, budget: usize) -> Vec<ContextSnippet> {
        let mut snippets = Vec::new();
        let mut used_tokens = 0;

        // Find the primary symbol (whose line range overlaps the chunk).
        let file_symbols = self.symbols.filter("", Some(&result.file_path));
        let primary_sym = file_symbols.iter().find(|s| {
            let sym_start = s.line_start as u64;
            let sym_end = s.line_end as u64;
            sym_start >= result.line_start && sym_start <= result.line_end
                || result.line_start >= sym_start && result.line_start <= sym_end
        });

        let sym_name = match primary_sym {
            Some(s) => s.name.clone(),
            None => return snippets,
        };

        // Get callees of this symbol.
        let callee_names = self.symbol_callees_precise(&sym_name, Some(&result.file_path));

        for callee_name in &callee_names {
            if used_tokens >= budget {
                break;
            }

            // Look up the callee's definition for its signature.
            let defs = self.symbols.lookup(callee_name);
            let def = match defs.into_iter().next() {
                Some(d) => d,
                None => continue,
            };

            let content = def
                .signature
                .as_ref()
                .cloned()
                .unwrap_or_else(|| format!("{} {}", def.kind, def.name));

            let tokens = estimate_token_count(&content);
            if used_tokens + tokens > budget {
                continue;
            }

            snippets.push(ContextSnippet {
                file_path: def.file_path.clone(),
                line_start: def.line_start,
                line_end: def.line_end,
                content,
                relevance: 0.7,
            });
            used_tokens += tokens;
        }

        snippets
    }

    /// Gather usage examples within the remaining token budget.
    fn gather_usage_examples(&self, result: &SearchResult, budget: usize) -> Vec<UsageExample> {
        // Find the primary symbol name from the chunk.
        let file_symbols = self.symbols.filter("", Some(&result.file_path));
        let primary_sym = file_symbols.iter().find(|s| {
            let sym_start = s.line_start as u64;
            let sym_end = s.line_end as u64;
            sym_start >= result.line_start && sym_start <= result.line_end
                || result.line_start >= sym_start && result.line_start <= sym_end
        });

        let sym_name = match primary_sym {
            Some(s) => s.name.clone(),
            None => return Vec::new(),
        };

        let all_examples = self.find_usage_examples(&sym_name, 10);

        // Filter to fit within the budget.
        let mut used_tokens = 0;
        let mut examples = Vec::new();
        for ex in all_examples {
            let tokens = estimate_token_count(&ex.context);
            if used_tokens + tokens > budget {
                break;
            }
            used_tokens += tokens;
            examples.push(ex);
        }

        examples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_token_count(""), 0);
        assert_eq!(estimate_token_count("hello world"), 3); // (11+3)/4 = 3
        assert_eq!(estimate_token_count("fn main() {}"), 3); // 12/4 = 3
    }

    #[test]
    fn estimate_tokens_empty_is_zero() {
        // (0 + 3) / 4 = 0 in integer division
        assert_eq!(estimate_token_count(""), 0);
    }

    #[test]
    fn estimate_tokens_short_string() {
        // "ab" -> (2+3)/4 = 1
        assert_eq!(estimate_token_count("ab"), 1);
    }

    #[test]
    fn context_snippet_creation() {
        let snippet = ContextSnippet {
            file_path: "src/lib.rs".into(),
            line_start: 10,
            line_end: 15,
            content: "pub fn helper() -> Result<()>".into(),
            relevance: 0.8,
        };
        assert_eq!(snippet.file_path, "src/lib.rs");
        assert!(snippet.relevance > 0.0);
    }

    #[test]
    fn assembled_context_serializes() {
        let ctx = AssembledContext {
            primary: SearchResult {
                chunk_id: "test:0".into(),
                file_path: "src/main.rs".into(),
                language: "rust".into(),
                score: 1.0,
                line_start: 0,
                line_end: 10,
                signature: String::new(),
                scope_chain: Vec::new(),
                content: "fn main() {}".into(),
            },
            imports: vec![ContextSnippet {
                file_path: "src/config.rs".into(),
                line_start: 0,
                line_end: 5,
                content: "pub struct Config {}".into(),
                relevance: 0.8,
            }],
            callees: Vec::new(),
            examples: Vec::new(),
            total_tokens: 10,
            requested_budget: 4096,
            budget_mode: BudgetMode::Soft,
            over_budget: false,
            oversize_reason: None,
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("src/main.rs"));
        assert!(json.contains("src/config.rs"));
        assert!(json.contains("\"budget_mode\":\"Soft\""));
    }

    #[test]
    fn soft_mode_records_overshoot_with_reason() {
        // Regression for #102: soft mode must report when the primary chunk
        // alone exceeds the requested budget so callers can surface the
        // overshoot instead of silently shipping over the cap.
        use crate::{Engine, IndexConfig};
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        // Build a single very large function so the primary chunk is
        // indivisible under the legacy assembler.
        let mut body = String::from("fn big() {\n");
        for i in 0..400 {
            body.push_str(&format!("    let x{i} = {i} + {i};\n"));
        }
        body.push_str("}\n");
        fs::write(root.join("big.rs"), &body).unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let ctx = engine.assemble_context_for_location_with_mode(
            "big.rs",
            5,
            500, // tiny budget vs ~2.5K-token function
            BudgetMode::Soft,
        );
        assert!(
            ctx.over_budget,
            "soft mode should flag overshoot: total={}, budget={}",
            ctx.total_tokens, ctx.requested_budget
        );
        let reason = ctx.oversize_reason.as_deref().unwrap_or_default();
        assert!(
            reason.contains("primary chunk indivisible"),
            "reason should explain why: {reason}"
        );
        assert!(
            reason.contains("--budget-mode strict"),
            "reason should point at the strict escape hatch: {reason}"
        );
    }

    #[test]
    fn strict_mode_slices_primary_to_fit_budget() {
        // Regression for #102: strict mode must enforce the budget as a
        // hard cap by slicing the primary chunk around `--line N`.
        use crate::{Engine, IndexConfig};
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut body = String::from("fn big() {\n");
        for i in 0..400 {
            body.push_str(&format!("    let x{i} = {i} + {i};\n"));
        }
        body.push_str("}\n");
        fs::write(root.join("big.rs"), &body).unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let budget = 500;
        let ctx = engine.assemble_context_for_location_with_mode(
            "big.rs",
            200,
            budget,
            BudgetMode::Strict,
        );
        assert!(
            ctx.total_tokens <= budget,
            "strict mode should keep total ≤ budget: total={}, budget={}",
            ctx.total_tokens,
            budget
        );
        assert!(
            !ctx.over_budget,
            "strict mode should not be flagged over budget when slicing succeeded"
        );
        // Window centered on line 200 should have shrunk the original
        // 400+ line function into something much smaller.
        let span = ctx.primary.line_end - ctx.primary.line_start + 1;
        assert!(
            span < 400,
            "primary chunk should have been sliced down (span={span})"
        );
    }

    #[test]
    fn agent_context_pack_serializes_stable_schema() {
        use crate::{Engine, IndexConfig};
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/greeting.rs"),
            "pub fn greeting(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/greeting_test.rs"),
            "#[test]\nfn greeting_includes_name() {\n    assert!(crate::greeting(\"codixing\").contains(\"codixing\"));\n}\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "# Greeting module\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let pack = engine
            .agent_context_pack(
                "change greeting output",
                AgentContextMode::Edit,
                3000,
                &["src/greeting.rs".to_string()],
                Some("feature/greeting".to_string()),
                Some("high".to_string()),
            )
            .unwrap();

        assert_eq!(pack.schema_version, 1);
        assert_eq!(pack.mode, AgentContextMode::Edit);
        assert_eq!(pack.branch.as_deref(), Some("feature/greeting"));
        assert!(
            pack.must_read
                .iter()
                .any(|entry| entry.path == "src/greeting.rs"),
            "changed file should be pinned into must_read: {pack:#?}"
        );
        assert!(
            pack.likely_tests
                .iter()
                .any(|test| test.path == "tests/greeting_test.rs"),
            "expected mapped tests in pack: {pack:#?}"
        );
        assert!(
            pack.recommended_next_tools
                .iter()
                .any(|tool| tool.starts_with("change_impact(")),
            "edit mode should recommend impact analysis: {pack:#?}"
        );

        let json = serde_json::to_value(&pack).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["mode"], "edit");
        assert!(
            json["must_read"][0]["id"]
                .as_str()
                .unwrap()
                .starts_with('E')
        );
    }
}
