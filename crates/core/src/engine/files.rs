use std::fs;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use tracing::warn;

use crate::error::{CodixingError, Result};
use crate::index::trigram::build_query_plan;

use super::{Engine, GrepMatch, GrepOptions};

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
    /// Thin wrapper around [`Engine::grep_code_opts`] retained for backward
    /// compatibility with existing callers. New features (`case_insensitive`,
    /// `invert`, asymmetric context) are only reachable via `grep_code_opts`.
    ///
    /// - `literal`: when `true`, the pattern is treated as a plain string (all
    ///   regex metacharacters are escaped before compilation).
    /// - `file_glob`: optional glob pattern (e.g. `"*.rs"`, `"src/**/*.py"`) to
    ///   restrict which files are searched.  `None` searches all indexed files.
    /// - `context_lines`: symmetric surrounding lines to include (clamped to 5).
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
        let opts = GrepOptions::from_simple(pattern, literal, file_glob, context_lines, limit);
        self.grep_code_opts(&opts)
    }

    /// Structured variant of [`Engine::grep_code`].
    ///
    /// Accepts a [`GrepOptions`] struct so callers can request case-insensitive
    /// matching, inverted line selection, or asymmetric before/after context
    /// without bloating the positional signature. Uses the file-level trigram
    /// index to narrow the candidate set before any disk I/O — except in
    /// invert mode, where every indexed file must be scanned (a file with no
    /// matching trigrams still has plenty of non-matching lines to emit).
    pub fn grep_code_opts(&self, opts: &GrepOptions) -> Result<Vec<GrepMatch>> {
        use regex::RegexBuilder;

        let before_context = opts.before_context.min(5);
        let after_context = opts.after_context.min(5);
        let limit = if opts.limit == 0 { 50 } else { opts.limit };

        let compiled_pattern = if opts.literal {
            regex::escape(&opts.pattern)
        } else {
            opts.pattern.clone()
        };
        let re = RegexBuilder::new(&compiled_pattern)
            .case_insensitive(opts.case_insensitive)
            .build()
            .map_err(|e| CodixingError::Index(format!("grep pattern error: {e}")))?;

        let glob_pat: Option<glob::Pattern> = match opts.file_glob.as_deref() {
            Some(g) => Some(
                glob::Pattern::new(g)
                    .map_err(|e| CodixingError::Index(format!("invalid file glob: {e}")))?,
            ),
            None => None,
        };

        // Trigram pre-filter is only sound for positive matches. Invert mode
        // needs the full indexed set — a file with no matching trigrams still
        // has plenty of non-matching lines to emit. Case-insensitive literal
        // matching also bypasses the prefilter because the trigram index is
        // case-sensitive.
        let candidate_set: Option<std::collections::HashSet<&str>> =
            if opts.invert || (opts.literal && opts.case_insensitive) {
                None
            } else if opts.literal {
                self.get_file_trigram()
                    .candidates_for_literal(opts.pattern.as_bytes())
                    .map(|v| v.into_iter().collect())
            } else {
                let plan = build_query_plan(&opts.pattern);
                self.get_file_trigram()
                    .execute_plan(&plan)
                    .map(|v| v.into_iter().collect())
            };

        let mut rel_paths: Vec<String> = self.file_chunk_counts.keys().cloned().collect();
        rel_paths.sort_unstable();

        let candidate_paths: Vec<&String> = rel_paths
            .iter()
            .filter(|p| {
                if let Some(ref candidates) = candidate_set {
                    if !candidates.contains(p.as_str()) {
                        return false;
                    }
                }
                if let Some(ref pat) = glob_pat {
                    let filename = std::path::Path::new(p.as_str())
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    if !pat.matches(p) && !pat.matches(filename) {
                        return false;
                    }
                }
                true
            })
            .collect();

        let done = AtomicBool::new(false);
        let results = Mutex::new(Vec::<GrepMatch>::new());
        let config = &self.config;
        let invert = opts.invert;

        candidate_paths.par_iter().for_each(|rel_path| {
            if done.load(Ordering::Relaxed) {
                return;
            }

            let abs = config
                .resolve_path(rel_path)
                .unwrap_or_else(|| config.root.join(rel_path.as_str()));
            let content = match fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(e) => {
                    warn!(file = %rel_path, error = %e, "grep_code: skipping unreadable file");
                    return;
                }
            };

            let lines: Vec<&str> = content.lines().collect();
            let n = lines.len();
            let mut file_matches = Vec::new();

            for (i, line) in lines.iter().enumerate() {
                let m = re.find(line);
                let hit = if invert { m.is_none() } else { m.is_some() };
                if !hit {
                    continue;
                }

                let before: Vec<String> = lines[i.saturating_sub(before_context)..i]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                let after_start = (i + 1).min(n);
                let after_end = (i + 1 + after_context).min(n);
                let after: Vec<String> = lines[after_start..after_end]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();

                // In invert mode there is no regex match span — report the full
                // line extent so downstream renderers can still highlight a
                // range if they want to.
                let (match_start, match_end) = match m {
                    Some(found) => (found.start(), found.end()),
                    None => (0, line.len()),
                };

                file_matches.push(GrepMatch {
                    file_path: rel_path.to_string(),
                    line_number: i as u64,
                    line: line.to_string(),
                    match_start,
                    match_end,
                    before,
                    after,
                });
            }

            if !file_matches.is_empty() {
                let mut guard = results.lock().unwrap_or_else(|e| e.into_inner());
                guard.extend(file_matches);
                if guard.len() >= limit {
                    done.store(true, Ordering::Relaxed);
                }
            }
        });

        let mut matches = results.into_inner().unwrap_or_else(|e| e.into_inner());
        matches.sort_by(|a, b| {
            a.file_path
                .cmp(&b.file_path)
                .then(a.line_number.cmp(&b.line_number))
        });
        matches.truncate(limit);

        Ok(matches)
    }

    /// Like [`grep_code`] but **skips trigram pre-filtering** — always scans
    /// every indexed file.  Used exclusively for benchmarking to measure the
    /// speedup provided by the trigram index.
    #[doc(hidden)]
    pub fn grep_code_full_scan(
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

        let glob_pat: Option<glob::Pattern> = match file_glob {
            Some(g) => Some(
                glob::Pattern::new(g)
                    .map_err(|e| CodixingError::Index(format!("invalid file glob: {e}")))?,
            ),
            None => None,
        };

        // No trigram pre-filtering — full scan baseline.
        let candidate_set: Option<std::collections::HashSet<&str>> = None;

        let mut rel_paths: Vec<String> = self.file_chunk_counts.keys().cloned().collect();
        rel_paths.sort_unstable();

        let candidate_paths: Vec<&String> = rel_paths
            .iter()
            .filter(|p| {
                if let Some(ref candidates) = candidate_set {
                    if !candidates.contains(p.as_str()) {
                        return false;
                    }
                }
                if let Some(ref pat) = glob_pat {
                    let filename = std::path::Path::new(p.as_str())
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("");
                    if !pat.matches(p) && !pat.matches(filename) {
                        return false;
                    }
                }
                true
            })
            .collect();

        let done = AtomicBool::new(false);
        let results = Mutex::new(Vec::<GrepMatch>::new());
        let config = &self.config;

        candidate_paths.par_iter().for_each(|rel_path| {
            if done.load(Ordering::Relaxed) {
                return;
            }
            let abs = config
                .resolve_path(rel_path)
                .unwrap_or_else(|| config.root.join(rel_path.as_str()));
            let content = match fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) => return,
            };
            let lines: Vec<&str> = content.lines().collect();
            let n = lines.len();
            let mut file_matches = Vec::new();
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
                    file_matches.push(GrepMatch {
                        file_path: rel_path.to_string(),
                        line_number: i as u64,
                        line: line.to_string(),
                        match_start: m.start(),
                        match_end: m.end(),
                        before,
                        after,
                    });
                }
            }
            if !file_matches.is_empty() {
                let mut guard = results.lock().unwrap_or_else(|e| e.into_inner());
                guard.extend(file_matches);
                if guard.len() >= limit {
                    done.store(true, Ordering::Relaxed);
                }
            }
        });

        let mut matches = results.into_inner().unwrap_or_else(|e| e.into_inner());
        matches.sort_by(|a, b| {
            a.file_path
                .cmp(&b.file_path)
                .then(a.line_number.cmp(&b.line_number))
        });
        matches.truncate(limit);
        Ok(matches)
    }
}
