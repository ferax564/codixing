use std::fs;

use tracing::warn;

use crate::error::{CodixingError, Result};
use crate::index::trigram::extract_required_trigrams;

use super::{Engine, GrepMatch};

impl Engine {
    // -------------------------------------------------------------------------
    // File and symbol reading
    // -------------------------------------------------------------------------

    /// Read raw source lines from a file in the indexed project.
    ///
    /// `path` must be relative to the project root (e.g. `"src/engine.rs"`).
    /// `line_start` and `line_end` are both **0-indexed inclusive** bounds.
    /// Omitting either means "from the beginning" / "to the end of file".
    ///
    /// Returns `None` if the file does not exist on disk.
    pub fn read_file_range(
        &self,
        path: &str,
        line_start: Option<u64>,
        line_end: Option<u64>,
    ) -> Result<Option<String>> {
        let abs = self
            .config
            .resolve_path(path)
            .unwrap_or_else(|| self.config.root.join(path));
        if !abs.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&abs)?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len() as u64;
        let start = line_start.unwrap_or(0).min(total) as usize;
        let end = line_end.map(|e| (e + 1).min(total)).unwrap_or(total) as usize;
        Ok(Some(lines[start..end].join("\n")))
    }

    /// Read the complete source of the first symbol whose name matches `name`.
    ///
    /// Performs the same case-insensitive substring lookup as [`Engine::symbols`],
    /// then reads the exact source lines from disk.
    ///
    /// Returns `None` if no matching symbol is found or the file is not on disk.
    pub fn read_symbol_source(&self, name: &str, file: Option<&str>) -> Result<Option<String>> {
        let matches = self.symbols.filter(name, file);
        let sym = match matches.into_iter().next() {
            Some(s) => s,
            None => return Ok(None),
        };
        self.read_file_range(
            &sym.file_path,
            Some(sym.line_start as u64),
            Some(sym.line_end as u64),
        )
    }

    /// Perform a regex or literal search across all source files in the project.
    ///
    /// Unlike [`Engine::search`] which queries the pre-built BM25/vector index,
    /// `grep_code` scans the raw file content — ideal for exact identifiers,
    /// string literals, TODO comments, or any pattern requiring verbatim matching.
    ///
    /// - `literal`: when `true`, the pattern is treated as a plain string (all
    ///   regex metacharacters are escaped before compilation).
    /// - `file_glob`: optional glob pattern (e.g. `"*.rs"`, `"src/**/*.py"`) to
    ///   restrict which files are searched.  `None` searches all indexed files.
    /// - `context_lines`: number of surrounding lines to include (clamped to 5).
    /// - `limit`: maximum total matches to return (default 50).
    ///
    /// Returns [`CodixingError::Index`] if the pattern fails to compile.
    pub fn grep_code(
        &self,
        pattern: &str,
        literal: bool,
        file_glob: Option<&str>,
        context_lines: usize,
        limit: usize,
    ) -> Result<Vec<GrepMatch>> {
        use regex::Regex;

        let context_lines = context_lines.min(5);
        let limit = if limit == 0 { 50 } else { limit };

        let compiled_pattern = if literal {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        let re = Regex::new(&compiled_pattern)
            .map_err(|e| CodixingError::Index(format!("grep pattern error: {e}")))?;

        // Build a glob matcher if file_glob is provided.
        let glob_pat: Option<glob::Pattern> = match file_glob {
            Some(g) => Some(
                glob::Pattern::new(g)
                    .map_err(|e| CodixingError::Index(format!("invalid file glob: {e}")))?,
            ),
            None => None,
        };

        let mut matches: Vec<GrepMatch> = Vec::new();

        // Use the file-level trigram index to narrow the candidate set before
        // doing any disk I/O.  For a literal pattern all trigrams must be
        // present; for a regex we extract required prefix seeds.  When pre-
        // filtering is not possible (pattern too short, or too broad) we fall
        // back to scanning all indexed files.
        let candidate_set: Option<std::collections::HashSet<&str>> = if literal {
            // Use the raw pattern bytes, NOT compiled_pattern — the latter has
            // regex escaping applied (e.g. `foo.bar` → `foo\.bar`) which would
            // produce wrong trigrams and cause false negatives.
            self.file_trigram
                .candidates_for_literal(pattern.as_bytes())
                .map(|v| v.into_iter().collect())
        } else {
            let trigrams = extract_required_trigrams(pattern);
            self.file_trigram
                .candidates_for_trigrams(&trigrams)
                .map(|v| v.into_iter().collect())
        };

        // Iterate over the already-indexed file set (relative paths).
        let mut rel_paths: Vec<String> = self.file_chunk_counts.keys().cloned().collect();
        rel_paths.sort_unstable(); // deterministic ordering

        'files: for rel_path in &rel_paths {
            // Skip files that the trigram pre-filter ruled out.
            if let Some(ref candidates) = candidate_set {
                if !candidates.contains(rel_path.as_str()) {
                    continue;
                }
            }

            // Apply glob filter if present.
            if let Some(ref pat) = glob_pat {
                // Match against both the full rel_path and just the filename.
                let filename = std::path::Path::new(rel_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if !pat.matches(rel_path) && !pat.matches(filename) {
                    continue;
                }
            }

            let abs = self
                .config
                .resolve_path(rel_path)
                .unwrap_or_else(|| self.config.root.join(rel_path));
            let content = match fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(e) => {
                    warn!(file = %rel_path, error = %e, "grep_code: skipping unreadable file");
                    continue;
                }
            };

            let lines: Vec<&str> = content.lines().collect();
            let n = lines.len();

            for (i, line) in lines.iter().enumerate() {
                if let Some(m) = re.find(line) {
                    let before: Vec<String> = lines[i.saturating_sub(context_lines)..i]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    let after_start = (i + 1).min(n);
                    let after_end = (i + 1 + context_lines).min(n);
                    let after: Vec<String> = lines[after_start..after_end]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();

                    matches.push(GrepMatch {
                        file_path: rel_path.clone(),
                        line_number: i as u64,
                        line: line.to_string(),
                        match_start: m.start(),
                        match_end: m.end(),
                        before,
                        after,
                    });

                    if matches.len() >= limit {
                        break 'files;
                    }
                }
            }
        }

        Ok(matches)
    }
}
