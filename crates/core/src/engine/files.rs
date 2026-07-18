use std::fs;
use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use tracing::warn;

use crate::error::{CodixingError, Result};
use crate::index::trigram::build_query_plan;

use super::{Engine, GrepMatch, GrepOptions};

/// Hard ceiling for the source body returned by [`Engine::read_file_range`].
///
/// MCP's largest response envelope is 48 KiB-equivalent (`12_000 * 4`), so a
/// 64 KiB core ceiling leaves enough headroom for that layer to recognize and
/// annotate truncation while preventing a hostile single-line/minified file
/// from allocating without bound in lower-level callers.
const MAX_READ_FILE_RANGE_BYTES: usize = 64 * 1024;
const READ_FILE_RANGE_TRUNCATION_MARKER: &str =
    "\n\n<!-- truncated: file range exceeded 64 KiB safety limit -->";

/// Scan one LF-delimited logical line without ever allocating the line itself.
///
/// Selected bytes are appended directly to `output` up to `output_limit`.
/// Returning `truncated = true` means more bytes existed in this line than fit.
/// A CR immediately before LF is removed to match [`str::lines`] semantics.
fn read_logical_line<R: BufRead>(
    reader: &mut R,
    mut output: Option<&mut Vec<u8>>,
    output_limit: usize,
) -> std::io::Result<(bool, bool)> {
    let mut saw_bytes = false;

    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((saw_bytes, false));
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let segment_len = newline.unwrap_or(buffer.len());
        let consumed = newline.map_or(segment_len, |position| position + 1);
        saw_bytes |= segment_len > 0 || newline.is_some();

        let mut truncated = false;
        if let Some(destination) = output.as_deref_mut() {
            let available = output_limit.saturating_sub(destination.len());
            let copied = available.min(segment_len);
            destination.extend_from_slice(&buffer[..copied]);
            truncated = copied < segment_len;
        }

        reader.consume(consumed);

        if truncated {
            return Ok((true, true));
        }
        if newline.is_some() {
            if let Some(destination) = output.as_deref_mut()
                && destination.last() == Some(&b'\r')
            {
                destination.pop();
            }
            return Ok((true, false));
        }
    }
}

impl Engine {
    // -------------------------------------------------------------------------
    // File and symbol reading
    // -------------------------------------------------------------------------

    /// Return all files represented in the index with their chunk counts.
    ///
    /// This is backed by `file_chunk_counts`, which is populated for every
    /// indexed file, including symbol-free docs/configs. If an older or
    /// partially-loaded engine has an empty derived map but populated
    /// `chunk_meta`, rebuild the view from chunk metadata as a fallback.
    pub fn indexed_files(&self) -> Vec<(String, usize)> {
        let mut files: std::collections::BTreeMap<String, usize> = self
            .file_chunk_counts
            .iter()
            .map(|(path, count)| (path.clone(), *count))
            .collect();

        if files.is_empty() && !self.chunk_meta.is_empty() {
            for entry in self.chunk_meta.iter() {
                *files.entry(entry.value().file_path.clone()).or_insert(0) += 1;
            }
        }

        files.into_iter().collect()
    }

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
        let Some(abs) = self.config.resolve_path(path) else {
            return Ok(None);
        };
        let start = line_start.unwrap_or(0);
        if line_end.is_some_and(|end| end < start) {
            return Ok(Some(String::new()));
        }

        let file = fs::File::open(abs)?;
        let mut reader = BufReader::new(file);
        let body_limit =
            MAX_READ_FILE_RANGE_BYTES.saturating_sub(READ_FILE_RANGE_TRUNCATION_MARKER.len());
        let mut body = Vec::with_capacity(body_limit.min(8 * 1024));
        let mut line_number = 0u64;
        let mut selected_lines = 0usize;
        let mut truncated = false;

        loop {
            if line_number >= start {
                if line_end.is_some_and(|end| line_number > end) {
                    break;
                }

                // `join("\n")` inserts a separator only when another logical
                // line actually exists. Peek before a full-buffer read so an
                // additional empty line cannot be silently omitted at the cap.
                if selected_lines > 0 && body.len() >= body_limit {
                    truncated = !reader.fill_buf()?.is_empty();
                    break;
                }

                let separator_added = selected_lines > 0;
                if separator_added {
                    body.push(b'\n');
                }
                let (has_line, line_truncated) =
                    read_logical_line(&mut reader, Some(&mut body), body_limit)?;
                if !has_line {
                    if separator_added {
                        body.pop();
                    }
                    break;
                }
                selected_lines += 1;
                if line_truncated {
                    truncated = true;
                    break;
                }
            } else {
                let (has_line, _) = read_logical_line(&mut reader, None, 0)?;
                if !has_line {
                    break;
                }
            }

            if line_end == Some(line_number) {
                break;
            }
            line_number = line_number.saturating_add(1);
        }

        let body = match String::from_utf8(body) {
            Ok(body) => body,
            Err(error) if truncated && error.utf8_error().error_len().is_none() => {
                let valid_up_to = error.utf8_error().valid_up_to();
                let mut bytes = error.into_bytes();
                bytes.truncate(valid_up_to);
                String::from_utf8(bytes).expect("validated UTF-8 prefix")
            }
            Err(error) => {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error).into());
            }
        };

        if truncated {
            let mut output = body;
            output.push_str(READ_FILE_RANGE_TRUNCATION_MARKER);
            Ok(Some(output))
        } else {
            Ok(Some(body))
        }
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

        // Guard against pre-v0.33 indexes whose file_chunk_counts / file_trigram
        // are empty because they were built before the content-side trigram
        // index existed. Before v0.37 this silently returned "0 matches across
        // 0 files" with no hint — surface it as an actionable error instead.
        //
        // Require BOTH `file_chunk_counts` empty AND `chunk_meta` empty before
        // erroring, so we don't false-positive on valid indexes whose mmap
        // hydration transiently has not populated one of the two maps yet.
        if self.file_chunk_counts.is_empty() && self.chunk_meta.is_empty() {
            return Err(CodixingError::Index(
                "grep requires an indexed file set but the index is empty — \
                 either this is a pre-v0.33 index built before the content \
                 trigram was added, or mmap hydration failed. \
                 Run `codixing init <root>` to rebuild."
                    .to_string(),
            ));
        }

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
                if let Some(ref candidates) = candidate_set
                    && !candidates.contains(p.as_str())
                {
                    return false;
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
        let count_mode = opts.count_mode;

        candidate_paths.par_iter().for_each(|rel_path| {
            if done.load(Ordering::Relaxed) {
                return;
            }

            let Some(abs) = config.resolve_path(rel_path) else {
                warn!(file = %rel_path, "grep_code: skipping path outside configured roots");
                return;
            };
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

                // Count mode: skip line text + context allocations entirely.
                // A common kernel grep (e.g. GFP_KERNEL, 30K+ hits) would
                // otherwise allocate ~30K String copies just for `--count`.
                if count_mode {
                    file_matches.push(GrepMatch {
                        file_path: rel_path.to_string(),
                        line_number: i as u64,
                        line: String::new(),
                        match_start: 0,
                        match_end: 0,
                        before: Vec::new(),
                        after: Vec::new(),
                    });
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
                if let Some(ref candidates) = candidate_set
                    && !candidates.contains(p.as_str())
                {
                    return false;
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
            let Some(abs) = config.resolve_path(rel_path) else {
                return;
            };
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

#[cfg(test)]
mod path_containment_tests {
    use super::*;
    use crate::config::IndexConfig;
    use tempfile::tempdir;

    #[test]
    fn read_file_range_cannot_escape_project_root() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("inside.rs"), "fn inside() {}\n").unwrap();
        fs::write(parent.path().join("secret.txt"), "secret\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        assert!(
            engine
                .read_file_range("inside.rs", None, None)
                .unwrap()
                .unwrap()
                .contains("inside")
        );
        assert_eq!(
            engine.read_file_range("../secret.txt", None, None).unwrap(),
            None
        );
        assert_eq!(
            engine
                .read_file_range(
                    parent.path().join("secret.txt").to_str().unwrap(),
                    None,
                    None
                )
                .unwrap(),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_file_range_cannot_follow_symlink_outside_project_root() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("inside.rs"), "fn inside() {}\n").unwrap();
        let outside = parent.path().join("secret.txt");
        fs::write(&outside, "secret\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();
        symlink(&outside, root.join("linked-secret")).unwrap();

        assert_eq!(
            engine.read_file_range("linked-secret", None, None).unwrap(),
            None
        );
    }

    #[test]
    fn read_file_range_streams_ranges_with_str_lines_newline_semantics() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("lines.rs"), "zero\r\none\n\nthree\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        assert_eq!(
            engine.read_file_range("lines.rs", None, None).unwrap(),
            Some("zero\none\n\nthree".to_string())
        );
        assert_eq!(
            engine
                .read_file_range("lines.rs", Some(1), Some(2))
                .unwrap(),
            Some("one\n".to_string())
        );
        assert_eq!(
            engine
                .read_file_range("lines.rs", Some(3), Some(1))
                .unwrap(),
            Some(String::new())
        );
    }

    #[test]
    fn read_file_range_bounds_a_single_minified_line() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("project");
        fs::create_dir(&root).unwrap();
        let mut minified = "x".repeat(2 * 1024 * 1024);
        minified.push_str("TAIL_SENTINEL");
        fs::write(root.join("minified.js"), minified).unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();
        let output = engine
            .read_file_range("minified.js", None, None)
            .unwrap()
            .unwrap();

        assert!(output.len() <= MAX_READ_FILE_RANGE_BYTES);
        assert!(output.contains("file range exceeded 64 KiB safety limit"));
        assert!(!output.contains("TAIL_SENTINEL"));
    }
}
