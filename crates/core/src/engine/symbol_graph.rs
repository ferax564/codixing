//! Precise symbol-level callers and callees using tree-sitter AST extraction.
//!
//! These methods provide more accurate results than the BM25-based
//! `search_usages` or the regex-based callee detection by parsing source
//! code with tree-sitter and extracting structured references.
//!
//! When a pre-built symbol-level graph is available (populated during
//! indexing), callers/callees queries are answered directly from the graph
//! without re-reading or re-parsing files.

use crate::graph::extract::extract_references;
use crate::graph::types::ReferenceKind;
use crate::language::detect_language;

use super::Engine;

/// A precise symbol reference found via AST analysis.
#[derive(Debug, Clone)]
pub struct SymbolReference {
    /// Relative file path where the reference was found.
    pub file_path: String,
    /// 0-indexed line number.
    pub line: usize,
    /// Kind of reference: "call", "import", "type_ref", etc.
    pub kind: String,
    /// The source line containing the reference.
    pub context: String,
}

/// Options controlling how `Engine::symbol_references` collects results.
///
/// **Complete mode** (`complete = true`) disables ranking and caps; it
/// enumerates every known call site / import for the symbol and returns
/// them deterministically. Use for "all callers" or "blast radius"
/// queries where sticky-mode agents were observed to stop after the
/// top-K ranked hits and miss the long tail (see
/// `docs/research-recall-stickiness-2026-04-13.md` §4.8).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReferenceOptions {
    /// If `true`, return the full deterministic set with no cap.
    pub complete: bool,
    /// Upper bound on results when `complete = false`. `None` means use
    /// the default ranked limit (20).
    pub max_results: Option<usize>,
}

impl Engine {
    /// Find precise callers of a symbol using the pre-built symbol graph or
    /// tree-sitter AST extraction.
    ///
    /// First checks the symbol-level inner graph for pre-computed call edges.
    /// If the graph has results, returns them directly (no file I/O needed).
    /// Otherwise falls back to BM25 search + AST validation.
    pub fn symbol_callers_precise(&self, symbol: &str, limit: usize) -> Vec<SymbolReference> {
        // Phase 0: Check the pre-built symbol graph.
        if let Some(ref graph) = self.graph {
            let symbol_callers = graph.get_symbol_callers(symbol);
            if !symbol_callers.is_empty() {
                let mut refs: Vec<SymbolReference> = symbol_callers
                    .into_iter()
                    .map(|(file, caller_name)| {
                        // Try to get the source line for context.
                        let context = self.read_line_at_symbol(&file, &caller_name);
                        let line = self.symbol_line(&file, &caller_name);
                        SymbolReference {
                            file_path: file,
                            line,
                            kind: "call".to_string(),
                            context,
                        }
                    })
                    .collect();
                refs.truncate(limit);
                return refs;
            }
        }

        // Phase 1: BM25 search to get candidate files
        let candidates = match self.search_usages(symbol, limit * 3) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };

        if candidates.is_empty() {
            return Vec::new();
        }

        // Phase 2: For each candidate, validate via AST extraction
        let mut precise_refs: Vec<SymbolReference> = Vec::new();

        for candidate in &candidates {
            let Some(abs_path) = self.config.resolve_path(&candidate.file_path) else {
                continue;
            };

            let source = match std::fs::read_to_string(&abs_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let lang = match detect_language(&abs_path) {
                Some(l) => l,
                None => continue,
            };

            let refs = extract_references(&source, &candidate.file_path, &lang);

            // Filter to references that match the target symbol name
            let symbol_base = symbol.rsplit("::").next().unwrap_or(symbol);
            for r in &refs {
                let ref_base = r.target_name.rsplit("::").next().unwrap_or(&r.target_name);
                if ref_base == symbol_base {
                    let context_line = source.lines().nth(r.line).unwrap_or("").trim().to_string();

                    precise_refs.push(SymbolReference {
                        file_path: r.file.clone(),
                        line: r.line,
                        kind: reference_kind_str(&r.kind),
                        context: context_line,
                    });
                }
            }
        }

        // Deduplicate by (file, line)
        precise_refs.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        precise_refs.dedup_by(|a, b| a.file_path == b.file_path && a.line == b.line);
        precise_refs.truncate(limit);

        precise_refs
    }

    /// Unified reference lookup with options.
    ///
    /// - Ranked mode (`opts.complete = false`): thin wrapper over
    ///   `symbol_callers_precise` with the configured limit.
    /// - Complete mode (`opts.complete = true`): exhausts the symbol graph
    ///   (when present) and deduplicates on `(file, line)`. Falls back to
    ///   BM25+AST scanning with a large candidate cap when the graph is
    ///   unavailable. Results are sorted by `(file_path, line)` — no
    ///   scoring, no truncation — so the output is deterministic.
    ///
    /// Intended caller: `codixing usages --complete` and the matching
    /// MCP tool surface.
    pub fn symbol_references(&self, symbol: &str, opts: ReferenceOptions) -> Vec<SymbolReference> {
        if !opts.complete {
            let limit = opts.max_results.unwrap_or(20);
            return self.symbol_callers_precise(symbol, limit);
        }

        // Complete mode is an exhaustive text scan. Candidates used to come
        // from ranked BM25 retrieval, which silently dropped files (including
        // production call sites) that did not rank; "complete" must mean
        // every occurrence.
        let mut refs = self.exact_symbol_references(symbol);
        refs.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.line.cmp(&b.line)));
        refs
    }

    /// Upper bound on references collected for one symbol.
    pub const MAX_EXACT_REFERENCES: usize = 100_000;

    /// Every occurrence of `symbol` as a whole word in the indexed files.
    ///
    /// Candidate files come from the file trigram index, the same structure
    /// `grep` uses, so recall equals a word-boundary text search and no call
    /// site can be missed because of how a chunk ranked. Each hit is then
    /// classified from the symbol table and the line itself:
    ///
    /// - `definition` — a symbol of this name is defined on that line
    /// - `import` — `use` / `import` / `from … import` / `#include` / `require`
    /// - `doc` — the file is prose (Markdown, reStructuredText, …)
    /// - `comment` — the line is a comment
    /// - `call` — the name is followed by `(`
    /// - `reference` — anything else (type position, field, value)
    ///
    /// At most [`Self::MAX_EXACT_REFERENCES`] hits are returned, in
    /// unspecified order.
    pub fn exact_symbol_references(&self, symbol: &str) -> Vec<SymbolReference> {
        use std::collections::{HashMap, HashSet};

        let base = symbol.rsplit("::").next().unwrap_or(symbol).trim();
        if base.is_empty() {
            return Vec::new();
        }
        let opts = super::GrepOptions {
            pattern: format!(r"\b{}\b", regex::escape(base)),
            literal: false,
            case_insensitive: false,
            invert: false,
            file_glob: None,
            before_context: 0,
            after_context: 0,
            limit: Self::MAX_EXACT_REFERENCES,
            count_mode: false,
        };
        let Ok(matches) = self.grep_code_opts(&opts) else {
            return Vec::new();
        };

        // (file, 0-based first line) of every definition with this name.
        let definitions: HashSet<(String, usize)> = self
            .symbols
            .lookup(base)
            .into_iter()
            .filter(|s| crate::symbols::definition_tier(&s.kind) >= 1)
            .map(|s| (s.file_path, s.line_start))
            .collect();

        // The line that *is* each definition: the first occurrence of the name
        // at or just below the symbol's first line (attributes or decorators
        // may precede the name). Only that line is the definition; a
        // recursive call two lines further down is a call.
        let mut occurrences: HashMap<&str, Vec<(usize, bool)>> = HashMap::new();
        for m in &matches {
            occurrences
                .entry(m.file_path.as_str())
                .or_default()
                .push((m.line_number as usize, is_comment_like(&m.line)));
        }
        for lines in occurrences.values_mut() {
            lines.sort_unstable();
        }
        let mut definition_lines: HashSet<(String, usize)> = HashSet::new();
        for (file, start) in &definitions {
            let Some(lines) = occurrences.get(file.as_str()) else {
                continue;
            };
            let first = lines.partition_point(|(line, _)| line < start);
            if let Some((line, _)) = lines[first..]
                .iter()
                .take_while(|(line, _)| *line <= start + DEFINITION_HEAD_WINDOW)
                .find(|(_, comment)| !comment)
            {
                definition_lines.insert((file.clone(), *line));
            }
        }

        matches
            .into_iter()
            .map(|m| {
                let line_number = m.line_number as usize;
                let is_definition = definition_lines.contains(&(m.file_path.clone(), line_number));
                let kind = classify_reference(&m.file_path, &m.line, m.match_end, is_definition);
                SymbolReference {
                    file_path: m.file_path,
                    line: line_number,
                    kind: kind.to_string(),
                    context: m.line.trim().to_string(),
                }
            })
            .collect()
    }

    /// Find precise callees of a symbol using the pre-built symbol graph or
    /// tree-sitter AST extraction.
    ///
    /// First checks the symbol-level inner graph for pre-computed call edges.
    /// If the graph has results, returns them directly.
    /// Otherwise falls back to reading and re-parsing the symbol's source.
    pub fn symbol_callees_precise(&self, symbol: &str, file_hint: Option<&str>) -> Vec<String> {
        // Phase 0: Check the pre-built symbol graph.
        if let Some(ref graph) = self.graph {
            let symbol_callees = graph.get_symbol_callees(symbol);
            if !symbol_callees.is_empty() {
                return symbol_callees;
            }
        }

        // Fallback: re-parse the symbol's source.
        // 1. Find the symbol's source code
        let src = match self.read_symbol_source(symbol, file_hint) {
            Ok(Some(s)) => s,
            _ => return Vec::new(),
        };

        // 2. Determine language from the symbol table
        let matches = self.symbols.filter(symbol, file_hint);
        let sym = match matches.into_iter().next() {
            Some(s) => s,
            None => return Vec::new(),
        };

        let Some(abs_path) = self.config.resolve_path(&sym.file_path) else {
            return Vec::new();
        };

        let lang = match detect_language(&abs_path) {
            Some(l) => l,
            None => return Vec::new(),
        };

        // 3. Extract references from the symbol's source
        let refs = extract_references(&src, &sym.file_path, &lang);

        // 4. Filter to Call references, deduplicate, exclude self
        let mut callees: Vec<String> = refs
            .iter()
            .filter(|r| r.kind == ReferenceKind::Call)
            .map(|r| {
                // Extract the base name for display
                r.target_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(&r.target_name)
                    .to_string()
            })
            .filter(|name| name != symbol)
            .collect();

        callees.sort();
        callees.dedup();
        callees
    }

    /// Helper: read a single source line at a symbol's definition for context.
    fn read_line_at_symbol(&self, file: &str, symbol_name: &str) -> String {
        let syms = self.symbols.filter(symbol_name, Some(file));
        let sym = match syms.into_iter().next() {
            Some(s) => s,
            None => return String::new(),
        };
        let Some(abs_path) = self.config.resolve_path(&sym.file_path) else {
            return String::new();
        };
        match std::fs::read_to_string(&abs_path) {
            Ok(source) => source
                .lines()
                .nth(sym.line_start)
                .unwrap_or("")
                .trim()
                .to_string(),
            Err(_) => String::new(),
        }
    }

    /// Helper: get the definition line number of a symbol in a file.
    fn symbol_line(&self, file: &str, symbol_name: &str) -> usize {
        let syms = self.symbols.filter(symbol_name, Some(file));
        syms.into_iter().next().map(|s| s.line_start).unwrap_or(0)
    }
}

/// Convert a `ReferenceKind` to a human-readable string.
/// Lines below a symbol's first line in which its name may first appear
/// (attributes or decorators can sit between the two).
const DEFINITION_HEAD_WINDOW: usize = 3;

/// Whether a line is a comment or an attribute/preprocessor line, which never
/// holds the definition itself.
fn is_comment_like(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('#')
}

/// Classify one whole-word occurrence of a symbol. `is_definition` is true
/// for the single line that declares the symbol.
fn classify_reference(
    file_path: &str,
    line: &str,
    match_end: usize,
    is_definition: bool,
) -> &'static str {
    if is_definition {
        return "definition";
    }
    let trimmed = line.trim_start();

    let lower_path = file_path.to_ascii_lowercase();
    if [".md", ".mdx", ".rst", ".txt", ".adoc", ".html", ".htm"]
        .iter()
        .any(|ext| lower_path.ends_with(ext))
    {
        return "doc";
    }

    if trimmed.starts_with("use ")
        || trimmed.starts_with("pub use ")
        || trimmed.starts_with("pub(crate) use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("from ")
        || trimmed.starts_with("#include")
        || trimmed.contains("require(")
    {
        return "import";
    }
    if trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('#')
        || trimmed.starts_with("\"\"\"")
        || trimmed.starts_with("--")
    {
        return "comment";
    }

    let after = line.get(match_end..).unwrap_or("").trim_start();
    if after.starts_with('(') || after.starts_with("::<") || after.starts_with("!(") {
        "call"
    } else {
        "reference"
    }
}

fn reference_kind_str(kind: &ReferenceKind) -> String {
    match kind {
        ReferenceKind::Call => "call".to_string(),
        ReferenceKind::Import => "import".to_string(),
        ReferenceKind::Inherit => "inherit".to_string(),
        ReferenceKind::FieldAccess => "field_access".to_string(),
        ReferenceKind::TypeRef => "type_ref".to_string(),
    }
}
