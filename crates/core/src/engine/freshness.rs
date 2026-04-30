//! File freshness audit: identifies files that haven't been updated relative
//! to recent development activity.
//!
//! Combines three engine capabilities:
//! - `get_recency_map` (git commit timestamps per file)
//! - `find_orphans` (graph connectivity — files with no importers)
//! - `callers` (count files that import a given file)

use std::collections::HashMap;

use crate::orphans::{OrphanConfidence, OrphanOptions};

use super::Engine;

/// Options for the freshness audit.
#[derive(Debug, Clone)]
pub struct FreshnessOptions {
    /// Flag files not modified in this many days.
    pub threshold_days: u64,
    /// Glob pattern to include (substring match, e.g. "*.rs", "crates/").
    pub include_pattern: Option<String>,
    /// Substring patterns to exclude. A file matches if it contains *any*
    /// of these patterns. Empty vec = no extra exclusion beyond the
    /// orphan-detection defaults.
    pub exclude_patterns: Vec<String>,
}

impl Default for FreshnessOptions {
    fn default() -> Self {
        Self {
            threshold_days: 21,
            include_pattern: None,
            exclude_patterns: Vec::new(),
        }
    }
}

/// Classification tier for a file in the freshness audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshnessTier {
    /// Orphan (Certain/High confidence) AND stale — strong dead-code candidate.
    Critical,
    /// Stale but still imported by at least one file.
    Warning,
    /// Recently orphaned (no importers but modified recently).
    Info,
}

impl FreshnessTier {
    /// Return a short label for the tier.
    pub fn as_str(&self) -> &str {
        match self {
            FreshnessTier::Critical => "critical",
            FreshnessTier::Warning => "warning",
            FreshnessTier::Info => "info",
        }
    }
}

impl std::fmt::Display for FreshnessTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single entry in the freshness audit.
#[derive(Debug, Clone)]
pub struct FreshnessEntry {
    /// Relative file path from the project root.
    pub file_path: String,
    /// Classification tier.
    pub tier: FreshnessTier,
    /// Days since the last git commit touching this file.
    /// `0` if the file has no commit history in the recency window (treated as fresh).
    pub days_old: u64,
    /// Unix timestamp of the last git commit (0 if unknown).
    pub last_modified_ts: i64,
    /// Whether this file has no importers in the dependency graph.
    pub is_orphan: bool,
    /// Orphan confidence label (only meaningful when `is_orphan` is true).
    pub orphan_confidence: Option<OrphanConfidence>,
    /// Number of files that import this file (callers count).
    pub importer_count: usize,
    /// Human-readable reason for inclusion in this tier.
    pub reason: String,
}

/// Result of a freshness audit.
#[derive(Debug, Clone)]
pub struct FreshnessReport {
    /// All entries, sorted by tier then by `days_old` descending.
    pub entries: Vec<FreshnessEntry>,
    /// Total number of files considered.
    pub files_audited: usize,
}

impl Engine {
    /// Audit files for freshness by combining git recency, orphan detection,
    /// and import graph connectivity.
    ///
    /// Returns a [`FreshnessReport`] containing tiered entries:
    /// - **Critical**: orphan (Certain/High) AND older than `threshold_days`
    /// - **Warning**: not orphaned but older than `threshold_days`
    /// - **Info**: orphaned but modified within `threshold_days`
    pub fn audit_freshness(&self, options: FreshnessOptions) -> FreshnessReport {
        let now_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // 1. Build a recency map covering a wide window (use threshold_days * 2,
        //    minimum 180 days) so stale files outside the default git window still
        //    have an entry when they exist in the repo history.
        let recency_window = options.threshold_days.max(180);
        let recency: HashMap<String, i64> =
            crate::engine::recency::build_recency_map(self.store.root(), recency_window);

        // 2. Run orphan detection only when the import graph is available.
        //    Without a graph every file looks like an orphan, producing false
        //    Critical alerts.
        let has_graph = self.graph.is_some();
        let orphan_map: HashMap<String, OrphanConfidence> = if has_graph {
            let mut orphan_opts = OrphanOptions {
                limit: usize::MAX,
                check_dynamic_refs: false, // skip text search for performance
                ..OrphanOptions::default()
            };
            if let Some(ref inc) = options.include_pattern {
                orphan_opts.include_patterns = vec![inc.clone()];
            }
            for exc in &options.exclude_patterns {
                orphan_opts.exclude_patterns.push(exc.clone());
            }
            self.find_orphans(orphan_opts)
                .into_iter()
                .map(|o| (o.file_path, o.confidence))
                .collect()
        } else {
            HashMap::new()
        };

        // 3. Iterate over all indexed files.
        //
        // `file_chunk_counts` is rebuilt from `chunk_meta` on `Engine::open()`.
        // On large indexes (Linux kernel) chunk_meta can be incomplete or the
        // mmap may not hydrate, leaving `file_chunk_counts` empty even though
        // the graph is fully populated. Union both sources so audit never
        // reports 0 files on a populated index.
        let mut file_set: std::collections::HashSet<String> =
            self.file_chunk_counts.keys().cloned().collect();
        if let Some(ref graph) = self.graph {
            for file in graph.file_paths() {
                file_set.insert(file);
            }
        }
        let all_files: Vec<String> = file_set.into_iter().collect();
        let mut entries: Vec<FreshnessEntry> = Vec::new();
        let files_audited = all_files.len();

        for file in &all_files {
            // Apply include/exclude filters.
            if let Some(ref inc) = options.include_pattern {
                if !file.contains(inc.as_str()) {
                    continue;
                }
            }
            if options
                .exclude_patterns
                .iter()
                .any(|p| file.contains(p.as_str()))
            {
                continue;
            }

            // Compute days since last commit.
            // Files with no git history (new / untracked) are treated as fresh
            // (age 0) rather than infinitely old, so they are never flagged stale.
            let last_ts = recency.get(file).copied().unwrap_or(0);
            let days_old = if last_ts > 0 {
                ((now_ts - last_ts).max(0) as u64) / 86_400
            } else {
                0 // No commit history — treat as fresh
            };

            // Use >= so the threshold is inclusive ("N+ days" semantics).
            let is_stale = days_old >= options.threshold_days;
            let orphan_confidence = orphan_map.get(file).cloned();
            let is_orphan = has_graph && orphan_confidence.is_some();
            let is_strong_orphan = has_graph
                && matches!(
                    orphan_confidence,
                    Some(OrphanConfidence::Certain) | Some(OrphanConfidence::High)
                );

            // Only report files that are stale OR orphaned.
            if !is_stale && !is_orphan {
                continue;
            }

            // Count importers only when needed (Warning tier).
            let importer_count = if !is_orphan {
                self.callers(file).len()
            } else {
                0
            };

            let (tier, reason) = if is_strong_orphan && is_stale {
                (
                    FreshnessTier::Critical,
                    format!(
                        "No importers ({}) and not modified in {} day(s) — likely dead code",
                        orphan_confidence
                            .as_ref()
                            .map(|c| c.as_str())
                            .unwrap_or("unknown"),
                        days_old
                    ),
                )
            } else if is_orphan && !is_stale {
                (
                    FreshnessTier::Info,
                    format!(
                        "No importers ({}) but recently modified",
                        orphan_confidence
                            .as_ref()
                            .map(|c| c.as_str())
                            .unwrap_or("unknown")
                    ),
                )
            } else if is_stale {
                // Not a strong orphan — might still be entry point / test / moderate.
                (
                    FreshnessTier::Warning,
                    format!(
                        "Not modified in {} day(s), still imported by {} file(s)",
                        days_old, importer_count
                    ),
                )
            } else {
                // is_orphan (Moderate/Low) but recent — skip, not worth reporting.
                continue;
            };

            entries.push(FreshnessEntry {
                file_path: file.clone(),
                tier,
                days_old,
                last_modified_ts: last_ts,
                is_orphan,
                orphan_confidence,
                importer_count,
                reason,
            });
        }

        // 4. Sort: within each tier, oldest first.
        entries.sort_by(|a, b| {
            let tier_order = |t: &FreshnessTier| match t {
                FreshnessTier::Critical => 0,
                FreshnessTier::Warning => 1,
                FreshnessTier::Info => 2,
            };
            tier_order(&a.tier)
                .cmp(&tier_order(&b.tier))
                .then(b.days_old.cmp(&a.days_old))
        });

        FreshnessReport {
            entries,
            files_audited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, IndexConfig};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn audit_reports_files_even_when_chunk_counts_empty() {
        // Regression: on the Linux kernel, audit was reporting 0 files
        // despite a populated 84K-node graph, because file_chunk_counts
        // rebuilds from chunk_meta and that rebuild can fail on large
        // indexes. The fix unions chunk-count keys with graph file_paths().
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(
            root.join("a.rs"),
            "fn hello() { world(); }\nfn world() {}\n",
        )
        .unwrap();
        fs::write(root.join("b.rs"), "fn main() { super::a::hello(); }\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let mut engine = Engine::init(&root, config).unwrap();

        // Simulate the failure mode: clear file_chunk_counts so audit cannot
        // rely on it. The graph should still enumerate files.
        engine.file_chunk_counts.clear();

        let report = engine.audit_freshness(FreshnessOptions {
            threshold_days: 180,
            include_pattern: None,
            exclude_patterns: Vec::new(),
        });

        assert!(
            report.files_audited >= 2,
            "audit should count both files via the graph even when \
             file_chunk_counts is empty, got files_audited={}",
            report.files_audited
        );
    }

    #[test]
    fn audit_excludes_every_pattern_in_the_list() {
        // Regression for #103: --exclude must accept multiple patterns and
        // skip files matching ANY of them. Pre-fix the CLI rejected repeated
        // --exclude flags entirely; post-fix the option is `Vec<String>` and
        // the freshness loop matches with `.any()`.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(
            root.join("real.rs"),
            "fn hello() { world(); }\nfn world() {}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("vendor")).unwrap();
        fs::write(root.join("vendor/dep.rs"), "fn vendored() {}\n").unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(
            root.join("node_modules/m.rs"),
            "fn from_node_modules() {}\n",
        )
        .unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let report = engine.audit_freshness(FreshnessOptions {
            threshold_days: 0,
            include_pattern: None,
            exclude_patterns: vec!["vendor".to_string(), "node_modules".to_string()],
        });

        for entry in &report.entries {
            assert!(
                !entry.file_path.contains("vendor") && !entry.file_path.contains("node_modules"),
                "exclude patterns leaked into report: {}",
                entry.file_path
            );
        }
    }
}
