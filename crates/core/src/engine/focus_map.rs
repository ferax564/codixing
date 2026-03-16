//! Focus-aware repo map: Personalized PageRank seeded by recently touched
//! files to surface contextually relevant code.

use std::collections::HashMap;
use std::path::Path;

use super::Engine;

/// A single entry in the focus map — a file ranked by relevance to the
/// current working context.
#[derive(Debug, Clone)]
pub struct FocusMapEntry {
    /// Relative file path.
    pub file_path: String,
    /// Personalized PageRank score (0..1, normalized so max = 1.0).
    pub rank: f32,
    /// Top symbols defined in this file (if requested).
    pub symbols: Vec<String>,
    /// How this file relates to the seed files.
    pub relationship: String,
}

/// Options for focus map generation.
#[derive(Debug, Clone)]
pub struct FocusMapOptions {
    /// Maximum number of files to return.
    pub max_files: usize,
    /// Whether to include top symbol names per file.
    pub include_symbols: bool,
    /// Decay factor for seed weight by recency position (e.g., 0.7 means
    /// second file gets 0.7x weight, third gets 0.7^2, etc.).
    pub seed_decay: f32,
}

impl Default for FocusMapOptions {
    fn default() -> Self {
        Self {
            max_files: 20,
            include_symbols: true,
            seed_decay: 0.7,
        }
    }
}

impl Engine {
    /// Generate a focus map: files ranked by Personalized PageRank seeded
    /// from the given files.
    ///
    /// The first file in `seed_files` gets weight 1.0, subsequent files
    /// decay geometrically by `options.seed_decay`.  Each result is
    /// annotated with its relationship to the seed set.
    pub fn focus_map(&self, seed_files: &[&str], options: &FocusMapOptions) -> Vec<FocusMapEntry> {
        if seed_files.is_empty() {
            return Vec::new();
        }

        let graph = match &self.graph {
            Some(g) => g,
            None => return Vec::new(),
        };

        // Build weighted seeds: most recent first, geometrically decaying.
        let seeds: Vec<(&str, f32)> = seed_files
            .iter()
            .enumerate()
            .map(|(i, &path)| {
                let weight = options.seed_decay.powi(i as i32);
                (path, weight)
            })
            .collect();

        let scores = crate::graph::compute_weighted_personalized_pagerank(
            graph,
            self.config.graph.damping,
            self.config.graph.iterations,
            1e-6,
            &seeds,
        );

        // Sort by score descending.
        let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        ranked.truncate(options.max_files);

        // Pre-compute seed-related lookups.
        let seed_set: std::collections::HashSet<&str> = seed_files.iter().copied().collect();
        let mut direct_deps: HashMap<&str, Vec<String>> = HashMap::new();
        let mut direct_callers: HashMap<&str, Vec<String>> = HashMap::new();
        for &seed in seed_files {
            direct_deps.insert(seed, graph.callees(seed));
            direct_callers.insert(seed, graph.callers(seed));
        }

        ranked
            .into_iter()
            .map(|(file_path, rank)| {
                // Determine relationship.
                let relationship = if seed_set.contains(file_path.as_str()) {
                    "seed (actively edited)".to_string()
                } else {
                    classify_relationship(&file_path, seed_files, &direct_deps, &direct_callers)
                };

                // Optionally collect top symbols.
                let symbols = if options.include_symbols {
                    let mut syms: Vec<String> = self
                        .symbols
                        .filter("", Some(&file_path))
                        .into_iter()
                        .map(|s| {
                            if let Some(ref sig) = s.signature {
                                sig.clone()
                            } else {
                                format!("{:?} {}", s.kind, s.name)
                            }
                        })
                        .collect();
                    syms.truncate(8); // limit symbols per file
                    syms
                } else {
                    Vec::new()
                };

                FocusMapEntry {
                    file_path,
                    rank,
                    symbols,
                    relationship,
                }
            })
            .collect()
    }

    /// Generate a focus map by auto-detecting seed files from git.
    ///
    /// Uses `git diff --name-only` (unstaged changes) and
    /// `git log --since=1hour --name-only` to discover recently touched files.
    pub fn focus_map_from_git(&self, options: &FocusMapOptions) -> Vec<FocusMapEntry> {
        let root = self.root();
        let seeds = detect_git_seeds(root);
        if seeds.is_empty() {
            return Vec::new();
        }
        let seed_refs: Vec<&str> = seeds.iter().map(|s| s.as_str()).collect();
        self.focus_map(&seed_refs, options)
    }
}

/// Classify how a file relates to the seed set.
fn classify_relationship(
    file: &str,
    seeds: &[&str],
    direct_deps: &HashMap<&str, Vec<String>>,
    direct_callers: &HashMap<&str, Vec<String>>,
) -> String {
    // Check if any seed directly imports this file.
    for &seed in seeds {
        if let Some(deps) = direct_deps.get(seed) {
            if deps.iter().any(|d| d == file) {
                return format!("direct dependency of {seed}");
            }
        }
    }

    // Check if this file directly imports any seed.
    for &seed in seeds {
        if let Some(callers) = direct_callers.get(seed) {
            if callers.iter().any(|c| c == file) {
                return format!("directly imports {seed}");
            }
        }
    }

    // Otherwise it's transitive / co-dependent.
    "transitive dependency".to_string()
}

/// Auto-detect seed files from git working tree and recent history.
///
/// Returns relative paths (forward-slash normalized) ordered by recency
/// (most recent first).
fn detect_git_seeds(root: &Path) -> Vec<String> {
    let mut seeds = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. Unstaged changes (most relevant — actively being edited).
    if let Some(files) = git_diff_name_only(root) {
        for f in files {
            if seen.insert(f.clone()) {
                seeds.push(f);
            }
        }
    }

    // 2. Staged changes.
    if let Some(files) = git_diff_staged_name_only(root) {
        for f in files {
            if seen.insert(f.clone()) {
                seeds.push(f);
            }
        }
    }

    // 3. Recent commits (last hour).
    if let Some(files) = git_recent_files(root) {
        for f in files {
            if seen.insert(f.clone()) {
                seeds.push(f);
            }
        }
    }

    seeds
}

fn git_diff_name_only(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .args(["diff", "--name-only"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_name_list(&String::from_utf8_lossy(&out.stdout)))
}

fn git_diff_staged_name_only(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_name_list(&String::from_utf8_lossy(&out.stdout)))
}

fn git_recent_files(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .args([
            "log",
            "--since=1 hour ago",
            "--name-only",
            "--pretty=format:",
        ])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_name_list(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_name_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.replace('\\', "/"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::CodeGraph;
    use crate::language::Language;
    use crate::symbols::SymbolTable;

    /// Build a minimal Engine-like test by directly testing classify_relationship.
    #[test]
    fn classify_direct_dependency() {
        let mut deps = HashMap::new();
        deps.insert("a.rs", vec!["b.rs".to_string()]);
        let callers = HashMap::new();
        let rel = classify_relationship("b.rs", &["a.rs"], &deps, &callers);
        assert!(rel.contains("direct dependency"), "got: {rel}");
    }

    #[test]
    fn classify_direct_importer() {
        let deps = HashMap::new();
        let mut callers = HashMap::new();
        callers.insert("a.rs", vec!["b.rs".to_string()]);
        let rel = classify_relationship("b.rs", &["a.rs"], &deps, &callers);
        assert!(rel.contains("directly imports"), "got: {rel}");
    }

    #[test]
    fn classify_transitive() {
        let deps = HashMap::new();
        let callers = HashMap::new();
        let rel = classify_relationship("c.rs", &["a.rs"], &deps, &callers);
        assert_eq!(rel, "transitive dependency");
    }

    #[test]
    fn parse_name_list_filters_blanks() {
        let list = parse_name_list("a.rs\n\nb.rs\n  \nc.rs\n");
        assert_eq!(list, vec!["a.rs", "b.rs", "c.rs"]);
    }

    // -----------------------------------------------------------------------
    // classify_relationship edge cases (Task 2B)
    // -----------------------------------------------------------------------

    #[test]
    fn classify_both_caller_and_callee_of_seed() {
        // File "b.rs" is both a direct dependency of seed "a.rs"
        // AND directly imports seed "a.rs". The function should return
        // the first match it finds (direct dependency check runs first).
        let mut deps = HashMap::new();
        deps.insert("a.rs", vec!["b.rs".to_string()]);
        let mut callers = HashMap::new();
        callers.insert("a.rs", vec!["b.rs".to_string()]);

        let rel = classify_relationship("b.rs", &["a.rs"], &deps, &callers);
        // The function checks deps first, so it should classify as "direct dependency".
        assert!(
            rel.contains("direct dependency"),
            "when file is both caller and callee, deps-check wins; got: {rel}"
        );
    }

    #[test]
    fn classify_no_relationship_to_seed() {
        // File "z.rs" has no relationship to any seed in deps or callers.
        let deps = HashMap::new();
        let callers = HashMap::new();
        let rel = classify_relationship("z.rs", &["a.rs", "b.rs"], &deps, &callers);
        assert_eq!(
            rel, "transitive dependency",
            "file with no direct relationship should be transitive"
        );
    }

    #[test]
    fn classify_multiple_seeds_first_match_wins() {
        // "c.rs" is a dep of seed "b.rs" but not of seed "a.rs".
        let mut deps = HashMap::new();
        deps.insert("a.rs", vec!["x.rs".to_string()]);
        deps.insert("b.rs", vec!["c.rs".to_string()]);
        let callers = HashMap::new();

        let rel = classify_relationship("c.rs", &["a.rs", "b.rs"], &deps, &callers);
        assert!(
            rel.contains("direct dependency of b.rs"),
            "should report which seed; got: {rel}"
        );
    }
}
