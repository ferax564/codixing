//! Cross-file context assembly: build minimal context for understanding a search result.
//!
//! Given a code location, assembles the matched chunk plus its import chain,
//! key callees, and usage examples — all within a configurable token budget.

use serde::Serialize;

use crate::engine::examples::UsageExample;
use crate::retriever::SearchResult;

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

impl Engine {
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
}
