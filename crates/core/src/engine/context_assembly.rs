//! Cross-file context assembly: build minimal context for understanding a search result.
//!
//! Given a code location, assembles the matched chunk plus its import chain,
//! key callees, and usage examples — all within a configurable token budget.

use serde::Serialize;

use crate::engine::examples::UsageExample;
use crate::retriever::SearchResult;

use super::Engine;

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

impl Engine {
    /// Assemble cross-file context for a code location specified by file path and line.
    ///
    /// This is the primary entry point for the CLI and MCP tool. It constructs
    /// a minimal `SearchResult` for the location and delegates to `assemble_context`.
    pub fn assemble_context_for_location(
        &self,
        file: &str,
        line: u64,
        token_budget: usize,
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

        let primary = SearchResult {
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

        self.assemble_context(&primary, token_budget)
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

        AssembledContext {
            primary: result.clone(),
            imports,
            callees,
            examples,
            total_tokens,
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
        };
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(json.contains("src/main.rs"));
        assert!(json.contains("src/config.rs"));
    }
}
