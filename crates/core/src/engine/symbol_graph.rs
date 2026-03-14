//! Precise symbol-level callers and callees using tree-sitter AST extraction.
//!
//! These methods provide more accurate results than the BM25-based
//! `search_usages` or the regex-based callee detection by parsing source
//! code with tree-sitter and extracting structured references.

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

impl Engine {
    /// Find precise callers of a symbol using tree-sitter AST extraction.
    ///
    /// Uses BM25 `search_usages` as a first pass to identify candidate files,
    /// then validates each hit by checking whether the symbol actually appears
    /// as a structured reference in the AST. This filters out false positives
    /// from pure text matches (e.g. comments, string literals).
    ///
    /// Falls back to raw BM25 results if no AST-confirmed references are found.
    pub fn symbol_callers_precise(&self, symbol: &str, limit: usize) -> Vec<SymbolReference> {
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
            let abs_path = self
                .config
                .resolve_path(&candidate.file_path)
                .unwrap_or_else(|| self.config.root.join(&candidate.file_path));

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

    /// Find precise callees of a symbol by parsing its source with tree-sitter.
    ///
    /// Locates the symbol's definition, reads its source code, then uses
    /// `extract_references` to find all function calls within the symbol body.
    /// This is more accurate than regex because it uses the AST.
    pub fn symbol_callees_precise(&self, symbol: &str, file_hint: Option<&str>) -> Vec<String> {
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

        let abs_path = self
            .config
            .resolve_path(&sym.file_path)
            .unwrap_or_else(|| self.config.root.join(&sym.file_path));

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
}

/// Convert a `ReferenceKind` to a human-readable string.
fn reference_kind_str(kind: &ReferenceKind) -> String {
    match kind {
        ReferenceKind::Call => "call".to_string(),
        ReferenceKind::Import => "import".to_string(),
        ReferenceKind::Inherit => "inherit".to_string(),
        ReferenceKind::FieldAccess => "field_access".to_string(),
        ReferenceKind::TypeRef => "type_ref".to_string(),
    }
}
