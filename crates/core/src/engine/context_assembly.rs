//! Cross-file context assembly: build minimal context for understanding a search result.
//!
//! Given a code location, assembles the matched chunk plus its import chain,
//! key callees, and usage examples — all within a configurable token budget.

use std::collections::HashSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::engine::examples::UsageExample;
use crate::error::{CodixingError, Result};
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

    /// Infer a workflow mode from natural-language task text.
    ///
    /// Keyword cues beat word-count heuristics so short edit/debug prompts
    /// (`fix crash in sync`, `add dry-run flag`) land on the right pack shape.
    /// When no cue matches, short tasks default to [`Self::Locate`] and longer
    /// phrasing to [`Self::Understand`].
    pub fn infer_from_task(task: &str) -> Self {
        let lower = task.to_ascii_lowercase();
        let tokens: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .filter(|t| !t.is_empty())
            .collect();
        let has = |words: &[&str]| {
            words.iter().any(|word| {
                if word.contains(' ') {
                    lower.contains(word)
                } else {
                    tokens.iter().any(|token| token == word)
                }
            })
        };

        if has(&[
            "incident",
            "outage",
            "prod",
            "production",
            "oncall",
            "on-call",
            "sev",
            "p0",
            "p1",
            "regression in prod",
        ]) {
            return Self::Incident;
        }
        if has(&[
            "migrate",
            "migration",
            "upgrade",
            "rename across",
            "rollout",
            "deprecate",
        ]) {
            return Self::Migrate;
        }
        if has(&[
            "review",
            "pr",
            "pull request",
            "diff",
            "lgtm",
            "code review",
        ]) {
            return Self::Review;
        }
        if has(&[
            "test",
            "tests",
            "unittest",
            "unit test",
            "coverage",
            "assert",
            "pytest",
            "cargo test",
        ]) {
            return Self::Test;
        }
        if has(&[
            "fix",
            "bug",
            "debug",
            "crash",
            "error",
            "failing",
            "broken",
            "investigate",
            "root cause",
            "why is",
            "stack trace",
            "panic",
        ]) {
            // Debugging is closer to incident/edit workflows than pure locate.
            return Self::Incident;
        }
        if has(&[
            "edit",
            "change",
            "add",
            "implement",
            "update",
            "modify",
            "refactor",
            "wire",
            "introduce",
            "create",
            "write",
            "patch",
            "remove",
            "delete",
            "rename",
        ]) {
            return Self::Edit;
        }
        if has(&[
            "how does",
            "how do",
            "explain",
            "understand",
            "walk through",
            "what is",
            "what does",
            "overview",
            "architecture",
            "why does",
            "choose",
            "chosen",
            "selects",
            "selection",
            "modes",
            "mode",
        ]) {
            return Self::Understand;
        }
        if has(&[
            "find",
            "where is",
            "locate",
            "search for",
            "look for",
            "which file",
        ]) {
            return Self::Locate;
        }

        if tokens.len() <= 6 {
            Self::Locate
        } else {
            Self::Understand
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

impl AgentContextPack {
    /// Render the pack as compact line-oriented text.
    ///
    /// This carries the same evidence as the JSON form at a fraction of the
    /// tokens: no repeated keys, one line per item, `path:start-end` ranges
    /// (1-based, inclusive) that can be handed straight to a file reader.
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(
            out,
            "# {} pack: {} evidence{}",
            self.mode.as_str(),
            self.must_read.len(),
            if self.truncated {
                " (truncated to budget)"
            } else {
                ""
            }
        );

        if self.must_read.is_empty() {
            out.push_str("No matching code found. Try `codixing search` with different terms or `codixing grep` for a literal.\n");
        } else {
            out.push_str("\n## Read first\n");
        }
        for evidence in &self.must_read {
            let kind = match evidence.kind.as_str() {
                "implementation" => String::new(),
                other => format!(" [{other}]"),
            };
            let signature = evidence
                .signature
                .as_deref()
                .map(first_line)
                .filter(|s| !s.is_empty())
                .map(|s| format!("  {s}"))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "{} {}:{}-{}{}{}",
                evidence.id, evidence.path, evidence.start_line, evidence.end_line, kind, signature
            );
            if let Some(preview) = &evidence.preview {
                for line in preview.lines() {
                    let _ = writeln!(out, "   | {line}");
                }
            }
        }

        if !self.related_symbols.is_empty() {
            out.push_str("\n## Defined in the evidence\n");
            for symbol in &self.related_symbols {
                let label = symbol
                    .signature
                    .clone()
                    .unwrap_or_else(|| format!("{} {}", symbol.kind, symbol.name));
                let start = symbol
                    .range
                    .trim_start_matches('L')
                    .split('-')
                    .next()
                    .unwrap_or("");
                let _ = writeln!(out, "{}:{} {}", symbol.path, start, label);
            }
        }

        // `repo_orientation` (neighbouring symbols per file) stays in the JSON
        // form; in text it costs more tokens than it saves follow-up calls.

        if !self.likely_tests.is_empty() {
            out.push_str("\n## Likely tests\n");
            for test in &self.likely_tests {
                let _ = writeln!(out, "{}  (covers {})", test.path, test.source_path);
            }
        }

        if !self.docs_and_conventions.is_empty() {
            out.push_str("\n## Docs\n");
            for doc in &self.docs_and_conventions {
                let title = doc
                    .title
                    .as_deref()
                    .map(|t| format!("  {t}"))
                    .unwrap_or_default();
                let range = doc.range.replace('L', "");
                let _ = writeln!(out, "{} {}:{}{}", doc.id, doc.path, range, title);
            }
        }

        if !self.risks.is_empty() {
            out.push_str("\n## Notes\n");
            for risk in &self.risks {
                let _ = writeln!(out, "- {risk}");
            }
        }

        if !self.recommended_next_tools.is_empty() {
            let _ = writeln!(out, "\nNext: {}", self.recommended_next_tools.join(" · "));
        }
        out
    }
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
    /// First lines of the evidence span, present for the leading items only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
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
    /// End line (0-indexed, exclusive).
    pub line_end: usize,
    /// The source code content.
    pub content: String,
    /// Relevance score (0.0-1.0).
    pub relevance: f32,
}

/// Cheap token count estimate: about 3.2 bytes per token.
///
/// Budgets are ceilings, so the estimate errs high. Measured against cl100k,
/// four bytes per token undercounts real code by 5–60% (Rust ≈ 3.8 bytes per
/// token, JSON ≈ 3.6, C with tabs and macros ≈ 3.2), which let "strict"
/// output overshoot its budget.
pub fn estimate_token_count(text: &str) -> usize {
    (text.len() * 5).div_ceil(16)
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
    // `file` can originate from the MCP reviewer.  Never fall back to a raw
    // join here: that would undo `resolve_path`'s traversal and symlink checks
    // when strict mode re-reads the source to shrink an oversized chunk.
    let Some(abs) = engine.config.resolve_path(file) else {
        return;
    };
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
    // Exclusive end, like every other result span.
    primary.line_end = end as u64 + 1;
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

/// 1-based inclusive range of a symbol (`end` is 0-based exclusive).
fn human_symbol_range(start: usize, end: usize) -> String {
    let human_start = start + 1;
    let human_end = end.max(human_start);
    format!("L{human_start}-L{human_end}")
}

/// Number of leading evidence items that carry a source preview.
const AGENT_PACK_PREVIEW_ITEMS: usize = 3;
/// Maximum preview size per evidence item.
const AGENT_PACK_PREVIEW_LINES: usize = 6;
const AGENT_PACK_PREVIEW_LINE_BYTES: usize = 140;

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim_end().to_string()
}

/// Lower-cased search terms of a query worth matching against source lines.
fn snippet_terms(query: &str) -> Vec<String> {
    const SKIP: &[&str] = &[
        "the", "and", "for", "how", "does", "what", "where", "which", "with", "from", "that",
        "this", "are", "was", "when", "why", "who", "into", "its", "any", "all", "not",
    ];
    let mut terms: Vec<String> = Vec::new();
    for raw in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let lower = raw.to_ascii_lowercase();
        if lower.len() < 3 || SKIP.contains(&lower.as_str()) {
            continue;
        }
        // `snake_case` terms also match on their parts.
        for part in lower.split('_').filter(|p| p.len() >= 3) {
            if part != lower && !terms.iter().any(|t| t == part) {
                terms.push(part.to_string());
            }
        }
        if !terms.contains(&lower) {
            terms.push(lower);
        }
    }
    terms
}

/// Pick the window of `max_lines` consecutive lines that best matches `query`.
///
/// Returns the zero-based offset of the window inside `content` and its lines.
/// A chunk's first lines are often a doc comment or the tail of the previous
/// item; the lines that actually mention the query terms are what lets a
/// reader judge a hit without opening the file. Falls back to the leading
/// non-blank lines when nothing matches.
pub fn query_focused_lines<'a>(
    content: &'a str,
    query: &str,
    max_lines: usize,
) -> (usize, Vec<&'a str>) {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() || max_lines == 0 {
        return (0, Vec::new());
    }
    let terms = snippet_terms(query);
    let scores: Vec<usize> = lines
        .iter()
        .map(|line| {
            if terms.is_empty() {
                return 0;
            }
            let lower = line.to_ascii_lowercase();
            terms.iter().filter(|t| lower.contains(t.as_str())).count()
        })
        .collect();

    let window = max_lines.min(lines.len());
    let mut best_start = 0;
    let mut best_score = 0;
    let mut running: usize = scores[..window].iter().sum();
    if running > best_score {
        best_score = running;
    }
    for start in 1..=(lines.len() - window) {
        running = running + scores[start + window - 1] - scores[start - 1];
        if running > best_score {
            best_score = running;
            best_start = start;
        }
    }
    if best_score == 0 {
        // No term matched: show the leading non-blank lines.
        let start = lines.iter().position(|l| !l.trim().is_empty()).unwrap_or(0);
        let end = (start + window).min(lines.len());
        return (start, lines[start..end].to_vec());
    }
    // Do not open the window on blank lines.
    let mut start = best_start;
    let end = best_start + window;
    while start + 1 < end && lines[start].trim().is_empty() {
        start += 1;
    }
    (start, lines[start..end].to_vec())
}

/// Query-focused preview of an evidence span, bounded per line.
fn evidence_preview(content: &str, query: &str) -> Option<String> {
    let (_, window) = query_focused_lines(content, query, AGENT_PACK_PREVIEW_LINES);
    let mut lines = Vec::new();
    for line in window {
        if line.trim().is_empty() {
            continue;
        }
        let mut end = line.len().min(AGENT_PACK_PREVIEW_LINE_BYTES);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        lines.push(line[..end].trim_end().to_string());
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
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

/// A task token that may name a symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentifierCandidate {
    text: String,
    /// True when the token has identifier *shape* (`snake_case`, `camelCase`,
    /// `Foo::bar`, letters mixed with digits). Unshaped tokens are ordinary
    /// words and only count when the symbol table knows them exactly.
    shaped: bool,
}

/// Words that never name a symbol worth pinning, however the task is phrased.
const TASK_STOPWORDS: &[&str] = &[
    "about", "after", "also", "been", "before", "between", "call", "calls", "change", "changed",
    "class", "code", "command", "does", "each", "file", "files", "find", "flag", "from",
    "function", "have", "into", "make", "method", "more", "most", "need", "only", "over", "result",
    "results", "should", "some", "test", "tests", "than", "that", "their", "then", "there",
    "these", "this", "type", "used", "uses", "using", "value", "what", "when", "where", "which",
    "while", "will", "with", "work", "works", "would",
];

/// Extract tokens from a task that may name symbols.
///
/// Prose words are kept only as *unshaped* candidates; the caller accepts them
/// solely on an exact, specific symbol-table hit. Treating every long word as
/// an identifier pinned unrelated symbols (anything containing "incremental")
/// above the real search hits.
fn task_identifier_candidates(task: &str) -> Vec<IdentifierCandidate> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in task.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':')) {
        let token = raw.trim_matches(':');
        if token.len() < 3 {
            continue;
        }
        let has_lower = token.chars().any(|c| c.is_ascii_lowercase());
        let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
        let inner_upper = token.chars().skip(1).any(|c| c.is_ascii_uppercase());
        let shaped = token.contains('_')
            || token.contains("::")
            || (inner_upper && has_lower)
            || (has_alpha && token.chars().any(|c| c.is_ascii_digit()));
        if !shaped {
            if token.len() < 4 || !has_alpha {
                continue;
            }
            if TASK_STOPWORDS.contains(&token.to_ascii_lowercase().as_str()) {
                continue;
            }
        }
        if seen.insert(token.to_string()) {
            out.push(IdentifierCandidate {
                text: token.to_string(),
                shaped,
            });
        }
        if out.len() >= 12 {
            break;
        }
    }
    out
}

fn demote_test_path_score(path: &str, score: f32) -> f32 {
    if path.contains("/test")
        || path.contains("/tests/")
        || path.contains("__tests__")
        || path.ends_with("_test.rs")
        || path.ends_with("_test.go")
        || path.ends_with(".test.ts")
        || path.ends_with(".test.tsx")
        || path.ends_with(".spec.ts")
        || path.ends_with(".spec.tsx")
    {
        score * 0.55
    } else {
        score
    }
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

/// Whether two line spans (0-based start, exclusive end) share a line.
///
/// Ends are exclusive, so `[0, 10)` and `[10, 20)` are neighbours, not
/// duplicates: comparing with `<=` dropped the chunk right after one that
/// was already selected. Identical spans always count as overlapping, which
/// also covers a degenerate empty span.
fn spans_overlap(a: (u64, u64), b: (u64, u64)) -> bool {
    a == b || (a.0 < b.1 && b.0 < a.1)
}

/// The definition a result is "about": the innermost non-import symbol that
/// starts inside the result or contains its first line. Taking the first
/// overlapping symbol picked the enclosing `impl` block (or an import) rather
/// than the function being looked at.
fn primary_symbol_of<'a>(
    file_symbols: &'a [crate::symbols::Symbol],
    result: &SearchResult,
) -> Option<&'a crate::symbols::Symbol> {
    file_symbols
        .iter()
        .filter(|s| !matches!(s.kind, crate::language::EntityKind::Import))
        .filter(|s| {
            let sym_start = s.line_start as u64;
            let sym_end = s.line_end as u64;
            sym_start >= result.line_start && sym_start <= result.line_end
                || result.line_start >= sym_start && result.line_start <= sym_end
        })
        .min_by_key(|s| {
            (
                // Prefer the symbol that starts closest to the result's start,
                // then the smallest span.
                (s.line_start as u64).abs_diff(result.line_start),
                s.line_end - s.line_start,
            )
        })
}

impl Engine {
    fn search_agent_task(
        &self,
        task: &str,
        limit: usize,
        strategy: Strategy,
        doc_filter: DocFilter,
    ) -> Result<Vec<SearchResult>> {
        let query = |text: &str| {
            SearchQuery::new(text)
                .with_limit(limit)
                .with_strategy(strategy)
                .with_doc_filter(doc_filter)
        };

        match self.search(query(task)) {
            Err(CodixingError::QueryParse(_)) => {
                let normalized = task
                    .chars()
                    .map(|character| {
                        if character.is_alphanumeric() || character == '_' {
                            character
                        } else {
                            ' '
                        }
                    })
                    .collect::<String>();
                self.search(query(&normalized))
            }
            result => result,
        }
    }

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
        use super::pipeline::{definition_tier, is_test_like_path};

        let token_budget = token_budget.max(256);
        let limit = search_limit_for_mode(mode, token_budget);
        let word_count = task.split_whitespace().count();

        // The task text goes through the same retrieval path as `search`, so a
        // pack is never worse than a plain search for the same words.
        let strategy = self.detect_strategy(task);
        let mut results = self.search_agent_task(task, limit, strategy, DocFilter::CodeOnly)?;

        // Exact symbol-table definitions named by the task are merged in ahead
        // of the text hits. A single-identifier task is already served by the
        // definition-first `Goto` strategy above.
        if word_count > 1 {
            const MAX_PINNED: usize = 4;
            let top_score = results.first().map(|r| r.score).unwrap_or(1.0).max(1.0);
            let mut pinned: Vec<SearchResult> = Vec::new();
            let result_files: std::collections::HashSet<String> =
                results.iter().map(|r| r.file_path.clone()).collect();
            for candidate in task_identifier_candidates(task) {
                if pinned.len() >= MAX_PINNED {
                    break;
                }
                let mut defs = self.symbols.lookup(&candidate.text);
                if defs.is_empty()
                    && let Some(last) = candidate.text.rsplit("::").next()
                    && last != candidate.text
                {
                    defs = self.symbols.lookup(last);
                }
                // Pin production definitions only; an ordinary word must also
                // match the symbol name exactly, including case (`request`
                // is not `Request`).
                defs.retain(|s| {
                    definition_tier(&s.kind) >= 2
                        && !is_test_like_path(&s.file_path)
                        && (candidate.shaped || s.name == candidate.text)
                });
                let production = defs.len();
                if production == 0 {
                    continue;
                }
                if !candidate.shaped {
                    // An ordinary word must name something specific: `sync`
                    // with one definition qualifies, `read` with hundreds
                    // does not.
                    let capitalised = candidate
                        .text
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase());
                    let cap = if capitalised { 8 } else { 3 };
                    if production > cap {
                        continue;
                    }
                    // A lower-case word is only a name if the search agrees:
                    // on a large repository `journal`, `commit` and
                    // `transaction` each name some unrelated driver struct.
                    if !capitalised {
                        defs.retain(|s| result_files.contains(s.file_path.as_str()));
                        if defs.is_empty() {
                            continue;
                        }
                    }
                }
                defs.sort_by(|a, b| {
                    definition_tier(&b.kind)
                        .cmp(&definition_tier(&a.kind))
                        .then_with(|| a.file_path.cmp(&b.file_path))
                        .then_with(|| a.line_start.cmp(&b.line_start))
                });
                let per_identifier = if candidate.shaped { 2 } else { 1 };
                for symbol in defs.into_iter().take(per_identifier) {
                    if pinned.len() >= MAX_PINNED {
                        break;
                    }
                    // Only the head of the definition is needed for the
                    // preview; large symbols are read on demand by the agent.
                    let head_end = symbol.line_end.min(symbol.line_start + 12);
                    let content = self
                        .read_file_range(
                            &symbol.file_path,
                            Some(symbol.line_start as u64),
                            Some(head_end as u64),
                        )?
                        .unwrap_or_default();
                    // A shaped identifier is strong evidence and leads the
                    // pack. An ordinary word that happens to name a symbol is
                    // weaker: it slots in right below the best text hit.
                    let weight = if candidate.shaped { 2.0 } else { 0.99 };
                    pinned.push(SearchResult {
                        chunk_id: format!(
                            "agent-def:{}:{}:{}",
                            symbol.file_path, symbol.name, symbol.line_start
                        ),
                        file_path: symbol.file_path.clone(),
                        language: symbol.language.name().to_string(),
                        score: top_score * weight - pinned.len() as f32 * 0.01,
                        line_start: symbol.line_start as u64,
                        // Symbol and chunk spans share one convention:
                        // 0-based start, exclusive end.
                        line_end: symbol.line_end as u64,
                        signature: symbol.signature.clone().unwrap_or_default(),
                        scope_chain: symbol.scope.clone(),
                        content,
                    });
                }
            }
            if !pinned.is_empty() {
                for result in &mut pinned {
                    result.score = demote_test_path_score(&result.file_path, result.score);
                }
                pinned.extend(results);
                results = pinned;
                results.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }

        let mut selected: Vec<SearchResult> = Vec::new();
        // Two entries for the same file are duplicates when their line ranges
        // overlap (a pinned definition and the chunk that contains it).
        let overlaps_selected = |selected: &[SearchResult], candidate: &SearchResult| {
            selected.iter().any(|kept| {
                kept.file_path == candidate.file_path
                    && spans_overlap(
                        (kept.line_start, kept.line_end),
                        (candidate.line_start, candidate.line_end),
                    )
            })
        };

        for file in changed_files {
            if let Some(result) = results.iter().find(|r| r.file_path == *file).cloned() {
                if !overlaps_selected(&selected, &result) {
                    selected.push(result);
                }
                continue;
            }

            if let Some(result) = self.agent_context_file_anchor(file)?
                && !overlaps_selected(&selected, &result)
            {
                selected.push(result);
            }
        }

        for result in results {
            if !overlaps_selected(&selected, &result) {
                selected.push(result);
            }
        }

        let changed_set: HashSet<&str> = changed_files.iter().map(String::as_str).collect();
        let mut must_read = Vec::new();
        for result in selected.iter().take(limit) {
            let (start_line, end_line, range) =
                human_result_range(result.line_start, result.line_end);
            let reason = if changed_set.contains(result.file_path.as_str()) {
                "changed file".to_string()
            } else if result.chunk_id.starts_with("agent-def:") {
                "definition named in the task".to_string()
            } else {
                "matches the task".to_string()
            };
            // A few lines of source for the leading evidence often answer the
            // question without a follow-up read.
            let preview = if must_read.len() < AGENT_PACK_PREVIEW_ITEMS {
                evidence_preview(&result.content, task)
            } else {
                None
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
                preview,
            });
        }

        // Per-file symbol context. Imports and module declarations carry no
        // navigational value, so only real definitions are listed:
        // `related_symbols` are the ones defined *inside* an evidence span,
        // `repo_orientation` the nearest ones *around* it.
        let mut repo_orientation = Vec::new();
        let mut related_symbols = Vec::new();
        let mut seen_files = HashSet::new();
        for evidence in &must_read {
            if !seen_files.insert(evidence.path.clone()) {
                continue;
            }
            // Zero-based inclusive spans of every evidence item in this file.
            let spans: Vec<(usize, usize)> = must_read
                .iter()
                .filter(|e| e.path == evidence.path)
                .map(|e| {
                    (
                        e.start_line.saturating_sub(1) as usize,
                        e.end_line.saturating_sub(1) as usize,
                    )
                })
                .collect();
            let evidence_ids: Vec<&str> = must_read
                .iter()
                .filter(|e| e.path == evidence.path)
                .map(|e| e.id.as_str())
                .collect();

            let mut definitions = self.symbols.file_symbols(&evidence.path);
            definitions.retain(|s| s.file_path == evidence.path && definition_tier(&s.kind) >= 2);
            definitions.sort_by_key(|s| s.line_start);

            let mut inside = Vec::new();
            let mut around = Vec::new();
            for symbol in definitions {
                let distance = spans
                    .iter()
                    .map(|&(start, end)| {
                        if symbol.line_start < start {
                            start - symbol.line_start
                        } else {
                            symbol.line_start.saturating_sub(end)
                        }
                    })
                    .min()
                    .unwrap_or(usize::MAX);
                if distance == 0 {
                    inside.push(symbol);
                } else {
                    around.push((distance, symbol));
                }
            }

            // The evidence line already shows its own signature; list only
            // the *other* definitions that live inside the span.
            let own_signatures: Vec<String> = must_read
                .iter()
                .filter(|e| e.path == evidence.path)
                .filter_map(|e| e.signature.as_deref().map(first_line))
                .collect();
            inside.retain(|s| {
                s.signature
                    .as_deref()
                    .map(first_line)
                    .is_none_or(|sig| !own_signatures.contains(&sig))
            });
            for symbol in inside.into_iter().take(2) {
                if related_symbols.len() >= 8 {
                    break;
                }
                related_symbols.push(AgentContextSymbol {
                    name: symbol.name,
                    kind: symbol.kind.to_string(),
                    path: symbol.file_path,
                    range: human_symbol_range(symbol.line_start, symbol.line_end),
                    signature: symbol.signature.as_deref().map(first_line),
                });
            }

            around.sort_by_key(|(distance, symbol)| (*distance, symbol.line_start));
            // Orientation is most useful for the leading files; later files
            // get fewer neighbours so the pack stays small.
            let keep = if repo_orientation.len() < 3 { 4 } else { 2 };
            around.truncate(keep);
            around.sort_by_key(|(_, symbol)| symbol.line_start);
            let symbols = around
                .into_iter()
                .map(|(_, s)| {
                    let label = s
                        .signature
                        .as_deref()
                        .map(first_line)
                        .unwrap_or_else(|| format!("{} {}", s.kind, s.name));
                    format!("L{} {}", s.line_start + 1, label)
                })
                .collect();
            let why = if changed_set.contains(evidence.path.as_str()) {
                "changed file".to_string()
            } else {
                format!("holds {}", evidence_ids.join(", "))
            };
            repo_orientation.push(AgentContextOrientation {
                id: format!("O{}", repo_orientation.len() + 1),
                path: evidence.path.clone(),
                why,
                symbols,
            });
        }

        let mut likely_tests = Vec::new();
        let mut seen_tests = HashSet::new();
        let test_sources: Vec<&str> = changed_files
            .iter()
            .map(String::as_str)
            .chain(repo_orientation.iter().map(|o| o.path.as_str()))
            .collect();
        // One snapshot of the file list serves every source file in the pack.
        'tests: for (_, mappings) in self.find_tests_for_files(&test_sources) {
            for mapping in mappings.into_iter().take(4) {
                let key = format!("{}:{}", mapping.source_file, mapping.test_file);
                if seen_tests.insert(key) {
                    likely_tests.push(AgentContextTest {
                        path: mapping.test_file,
                        source_path: mapping.source_file,
                        confidence: mapping.confidence,
                        reason: mapping.reason,
                    });
                }
                if likely_tests.len() >= 8 {
                    break 'tests;
                }
            }
        }

        let docs = self
            .search_agent_task(task, 4, Strategy::Instant, DocFilter::DocsOnly)
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
        if let Some(level) = risk_level.as_deref()
            && matches!(level.to_ascii_lowercase().as_str(), "high" | "critical")
        {
            risks.push(
                "High risk level requested; prefer patch preview, impact analysis, and full tests."
                    .to_string(),
            );
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
        let mut symbols = self.symbols.file_symbols(file);
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
                line_end: symbol.line_end as u64,
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

        // Find the *innermost* definition that contains this line. Taking the
        // first overlapping symbol returned the enclosing `impl` block or
        // class (thousands of lines) instead of the method the caller asked
        // about.
        let symbols = self.symbols.file_symbols(file);
        let overlapping = symbols
            .iter()
            .filter(|s| {
                s.file_path == file
                    && !matches!(s.kind, crate::language::EntityKind::Import)
                    && line >= s.line_start as u64
                    && line < s.line_end as u64
            })
            .min_by_key(|s| (s.line_end - s.line_start, s.line_start));

        let (line_start, line_end, signature, content) = if let Some(sym) = overlapping {
            let sym_content = self
                .read_file_range(
                    file,
                    Some(sym.line_start as u64),
                    // `line_end` is exclusive; the range read is inclusive.
                    Some(sym.line_end.saturating_sub(1).max(sym.line_start) as u64),
                )
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
        let dep_files = match self.graph_ref() {
            Some(g) => g.callees(&result.file_path),
            None => return snippets,
        };

        for dep_file in &dep_files {
            if used_tokens >= budget {
                break;
            }

            // Find symbols in the dependency file that appear in the primary content.
            let dep_symbols = self.symbols.file_symbols(dep_file.as_str());
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
        let file_symbols = self.symbols.file_symbols(&result.file_path);
        let primary_sym = primary_symbol_of(&file_symbols, result);

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
        let file_symbols = self.symbols.file_symbols(&result.file_path);
        let primary_sym = primary_symbol_of(&file_symbols, result);

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

    fn primary_for_shrink_test() -> SearchResult {
        SearchResult {
            chunk_id: "original:0".into(),
            file_path: "original.rs".into(),
            language: "rust".into(),
            score: 1.0,
            line_start: 0,
            line_end: 0,
            signature: String::new(),
            scope_chain: Vec::new(),
            content: "original content".into(),
        }
    }

    #[test]
    fn strict_window_does_not_read_traversal_or_absolute_paths() {
        use crate::{Engine, IndexConfig};
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("outside.rs");
        fs::write(&outside, "outside sentinel\nsecond line\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        for candidate in ["../outside.rs".to_string(), outside.display().to_string()] {
            let mut primary = primary_for_shrink_test();
            shrink_primary_to_window(&mut primary, &candidate, 0, 100, &engine);
            assert_eq!(
                primary.content, "original content",
                "strict window unexpectedly read {candidate}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn strict_window_does_not_follow_symlink_outside_root() {
        use crate::{Engine, IndexConfig};
        use std::fs;
        use std::os::unix::fs::symlink;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("outside.rs");
        fs::write(&outside, "outside sentinel\nsecond line\n").unwrap();
        symlink(&outside, root.join("escape.rs")).unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();
        let mut primary = primary_for_shrink_test();

        shrink_primary_to_window(&mut primary, "escape.rs", 0, 100, &engine);
        assert_eq!(primary.content, "original content");
    }

    #[test]
    fn adjacent_spans_are_not_duplicates() {
        // Exclusive ends: a chunk that starts where another stops is a
        // different chunk and must stay in the pack.
        assert!(!spans_overlap((0, 10), (10, 20)));
        assert!(!spans_overlap((10, 20), (0, 10)));
        // A definition inside the chunk that contains it is a duplicate.
        assert!(spans_overlap((0, 10), (4, 6)));
        assert!(spans_overlap((4, 6), (0, 10)));
        assert!(spans_overlap((0, 10), (9, 30)));
        // Identical spans, including empty ones.
        assert!(spans_overlap((5, 9), (5, 9)));
        assert!(spans_overlap((5, 5), (5, 5)));
    }

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_token_count(""), 0);
        assert_eq!(estimate_token_count("hello world"), 4); // ceil(11 / 3.2)
        assert_eq!(estimate_token_count("fn main() {}"), 4); // ceil(12 / 3.2)
    }

    #[test]
    fn estimate_tokens_empty_is_zero() {
        assert_eq!(estimate_token_count(""), 0);
    }

    #[test]
    fn estimate_tokens_short_string() {
        // "ab" -> ceil(2 / 3.2) = 1
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
    fn agent_context_mode_infers_from_task_cues() {
        assert_eq!(
            AgentContextMode::infer_from_task("how does agent context pack choose modes"),
            AgentContextMode::Understand
        );
        assert_eq!(
            AgentContextMode::infer_from_task("add dry-run flag to init"),
            AgentContextMode::Edit
        );
        assert_eq!(
            AgentContextMode::infer_from_task("fix crash in sync"),
            AgentContextMode::Incident
        );
        assert_eq!(
            AgentContextMode::infer_from_task("review this PR"),
            AgentContextMode::Review
        );
        assert_eq!(
            AgentContextMode::infer_from_task("AgentContextMode"),
            AgentContextMode::Locate
        );
    }

    #[test]
    fn agent_context_pack_prefers_symbol_definitions() {
        use crate::{Engine, IndexConfig};
        use std::fs;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(
            root.join("src/mode.rs"),
            "pub enum AgentContextMode {\n    Locate,\n    Understand,\n}\n\nimpl AgentContextMode {\n    pub fn infer_from_task(task: &str) -> Self {\n        let _ = task;\n        Self::Locate\n    }\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("tests/mode_test.rs"),
            "#[test]\nfn agent_context_mode_works() {\n    let _ = crate::mode::AgentContextMode::Locate;\n}\n",
        )
        .unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();
        let pack = engine
            .agent_context_pack(
                "how does AgentContextMode choose modes",
                AgentContextMode::Understand,
                2500,
                &[],
                None,
                None,
            )
            .unwrap();
        assert!(
            pack.must_read
                .iter()
                .any(|entry| entry.path == "src/mode.rs"),
            "definition file should rank in must_read: {pack:#?}"
        );
        assert!(
            pack.must_read
                .first()
                .is_some_and(|entry| entry.path == "src/mode.rs"),
            "definition should outrank tests: {pack:#?}"
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

        let punctuation_pack = engine
            .agent_context_pack(
                "greeting: include name",
                AgentContextMode::Locate,
                3000,
                &[],
                None,
                None,
            )
            .unwrap();
        assert_eq!(punctuation_pack.task_summary, "greeting: include name");
        assert!(
            punctuation_pack
                .must_read
                .iter()
                .any(|entry| entry.path == "src/greeting.rs"),
            "punctuation-normalized task should still retrieve code: {punctuation_pack:#?}"
        );
    }
}
