//! File freshness audit: identifies files that haven't been updated relative
//! to recent development activity.
//!
//! Combines three engine capabilities:
//! - `get_recency_map` (git commit timestamps per file)
//! - `find_orphans` (graph connectivity — files with no importers)
//! - `callers` (count files that import a given file)

use std::collections::{HashMap, HashSet};

use crate::orphans::{OrphanConfidence, OrphanOptions};

use super::Engine;

/// Audit profile: what kind of repo this is, which decides whether docs
/// and static assets get flagged as code orphans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuditProfile {
    /// Source-only repo. Skip documentation, templates, and static assets
    /// entirely so the audit signal stays focused on code.
    Code,
    /// Application repo with docs, templates, and static assets co-located
    /// with code (this is the default, matches the EZKeel-style "mixed"
    /// repo from issue #103). Docs/templates/static files are classified
    /// separately from code orphans and capped at the `Info` tier.
    #[default]
    Mixed,
    /// Audit everything with no extra classification. Equivalent to the
    /// pre-0.41.2 behavior — kept for users who liked the firehose.
    App,
}

impl AuditProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditProfile::Code => "code",
            AuditProfile::Mixed => "mixed",
            AuditProfile::App => "app",
        }
    }
}

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
    /// Audit profile, controls how docs/templates/static assets are scored.
    pub profile: AuditProfile,
}

impl Default for FreshnessOptions {
    fn default() -> Self {
        Self {
            threshold_days: 21,
            include_pattern: None,
            exclude_patterns: Vec::new(),
            profile: AuditProfile::default(),
        }
    }
}

/// File extensions that are documentation/markup, not importable code.
const DOC_EXTS: &[&str] = &["md", "markdown", "mdx", "rst", "adoc", "asciidoc", "txt"];
/// File extensions that are static assets / templates served by code,
/// never imported themselves but routinely referenced via `//go:embed`,
/// `include_bytes!`, or framework asset pipelines.
const ASSET_EXTS: &[&str] = &[
    "html", "htm", "css", "scss", "svg", "png", "jpg", "jpeg", "gif", "ico", "webp", "woff",
    "woff2", "ttf", "json", "yaml", "yml", "toml",
];

fn extension_is(path: &str, candidates: &[&str]) -> bool {
    let ext = match path.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => return false,
    };
    candidates.iter().any(|c| c == &ext)
}

fn is_doc_file(path: &str) -> bool {
    extension_is(path, DOC_EXTS)
}

fn is_asset_file(path: &str) -> bool {
    extension_is(path, ASSET_EXTS)
}

/// Best-effort scan for Go `//go:embed` directives across the indexed file
/// set. Returns the union of:
/// - Go file paths that contain at least one `//go:embed` line.
/// - Directory prefixes named in those directives, normalized to forward
///   slashes and relative to the project root.
///
/// The match is line-prefix based and intentionally tolerant — false
/// positives downgrade severity, they do not introduce broken behavior.
fn detect_go_embed_roots<I, S>(
    project_root: &std::path::Path,
    files: I,
) -> (HashSet<String>, Vec<String>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut wrappers = HashSet::new();
    let mut prefixes: Vec<String> = Vec::new();

    for file in files {
        let file = file.as_ref();
        if !file.ends_with(".go") {
            continue;
        }
        let abs = project_root.join(file);
        let content = match std::fs::read_to_string(&abs) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut found_directive = false;
        for line in content.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("//go:embed") {
                found_directive = true;
                for token in rest.split_whitespace() {
                    // `//go:embed` accepts both bare globs (`templates/*`)
                    // and quoted patterns (`"templates/*"` for paths with
                    // spaces). Strip surrounding quotes before trimming
                    // glob suffixes so the prefix matches actual files.
                    let unquoted = token
                        .trim_start_matches('"')
                        .trim_end_matches('"')
                        .trim_start_matches('\'')
                        .trim_end_matches('\'');
                    let cleaned = unquoted
                        .trim_end_matches('*')
                        .trim_end_matches('/')
                        .trim_end_matches("/*");
                    if !cleaned.is_empty() {
                        // Resolve the directive token relative to the file's
                        // own directory so `//go:embed templates` in
                        // `web/embed.go` becomes `web/templates`.
                        let parent = std::path::Path::new(file).parent();
                        let joined = match parent {
                            Some(p) if !p.as_os_str().is_empty() => {
                                p.join(cleaned).to_string_lossy().replace('\\', "/")
                            }
                            _ => cleaned.to_string(),
                        };
                        prefixes.push(joined);
                    }
                }
            }
        }
        if found_directive {
            wrappers.insert(file.to_string());
        }
    }

    prefixes.sort();
    prefixes.dedup();
    (wrappers, prefixes)
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

        // Detect Go `//go:embed` wrappers + their target prefixes so we
        // never flag the embed root file or anything embedded under it as
        // dead code. Skipped on the `App` profile to preserve the legacy
        // firehose behavior for users who explicitly opt out of the new
        // classification.
        let (embed_wrappers, embed_prefixes) = if options.profile == AuditProfile::App {
            (HashSet::new(), Vec::new())
        } else {
            detect_go_embed_roots(self.store.root(), all_files.iter().map(|s| s.as_str()))
        };
        let is_embed_protected = |path: &str| -> bool {
            embed_wrappers.contains(path)
                || embed_prefixes
                    .iter()
                    .any(|p| path == p || path.starts_with(&format!("{p}/")))
        };

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

            // Profile-driven filtering / classification.
            let is_doc = is_doc_file(file);
            let is_asset = is_asset_file(file);
            match options.profile {
                AuditProfile::Code if is_doc || is_asset => continue,
                _ => {}
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

            // Mixed-profile classification: documentation, static assets,
            // and Go embed wrappers are not import-graph nodes — they
            // routinely show up as "orphans" because nothing imports them
            // in the language sense. Cap their tier at `Info` and label
            // the reason so users can tell intentional standalone files
            // apart from dead code.
            let asset_label = if options.profile != AuditProfile::App {
                if is_embed_protected(file) {
                    Some("Go embed root / embedded asset")
                } else if is_doc {
                    Some("documentation")
                } else if is_asset {
                    Some("static asset / template")
                } else {
                    None
                }
            } else {
                None
            };

            let (tier, reason) = if let Some(label) = asset_label {
                (
                    FreshnessTier::Info,
                    format!(
                        "{label} — no code importers expected (last modified {days_old} day(s) ago)"
                    ),
                )
            } else if is_strong_orphan && is_stale {
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
            profile: AuditProfile::App,
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
            profile: AuditProfile::App,
        });

        for entry in &report.entries {
            assert!(
                !entry.file_path.contains("vendor") && !entry.file_path.contains("node_modules"),
                "exclude patterns leaked into report: {}",
                entry.file_path
            );
        }
    }

    #[test]
    fn mixed_profile_classifies_docs_as_info_not_critical() {
        // Regression for #103: docs and templates routinely have no
        // importers and look stale, but they're not dead code. Default
        // `mixed` profile must cap them at `Info` instead of `Critical`.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/design.md"), "# Design notes\n").unwrap();
        fs::write(root.join("README.md"), "# Project\n").unwrap();
        fs::create_dir_all(root.join("web/templates")).unwrap();
        fs::write(
            root.join("web/templates/index.html"),
            "<html><body>hi</body></html>\n",
        )
        .unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let report = engine.audit_freshness(FreshnessOptions {
            threshold_days: 0,
            include_pattern: None,
            exclude_patterns: Vec::new(),
            profile: AuditProfile::Mixed,
        });

        let docs: Vec<_> = report
            .entries
            .iter()
            .filter(|e| e.file_path.ends_with(".md") || e.file_path.ends_with(".html"))
            .collect();
        assert!(!docs.is_empty(), "should have surfaced doc/template files");
        for entry in &docs {
            assert_eq!(
                entry.tier,
                FreshnessTier::Info,
                "doc/template should be Info under mixed profile, got {:?} for {}",
                entry.tier,
                entry.file_path
            );
            assert!(
                entry.reason.contains("documentation") || entry.reason.contains("static asset"),
                "reason should label the asset kind: {} ({})",
                entry.file_path,
                entry.reason
            );
        }
    }

    #[test]
    fn code_profile_skips_docs_and_static_assets() {
        // `--profile code` is for source-only repos. Docs / templates /
        // assets should be excluded from the audit entirely.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(root.join("README.md"), "# Project\n").unwrap();
        fs::write(
            root.join("real.rs"),
            "fn hello() { world(); }\nfn world() {}\n",
        )
        .unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let report = engine.audit_freshness(FreshnessOptions {
            threshold_days: 0,
            include_pattern: None,
            exclude_patterns: Vec::new(),
            profile: AuditProfile::Code,
        });

        for entry in &report.entries {
            assert!(
                !entry.file_path.ends_with(".md"),
                "code profile should skip docs: {}",
                entry.file_path
            );
        }
    }

    #[test]
    fn go_embed_root_handles_quoted_patterns() {
        // Regression for review of #103: `//go:embed "templates/*"` is
        // valid Go (quoted patterns let you embed paths with spaces).
        // Pre-fix the prefix included the surrounding quotes and never
        // matched a real file path, so the embed wrapper lost its
        // mixed-profile protection.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("web/templates")).unwrap();
        fs::write(
            root.join("web/templates/index.html"),
            "<html><body>hi</body></html>\n",
        )
        .unwrap();
        fs::write(
            root.join("web/embed.go"),
            "package web\n\nimport \"embed\"\n\n//go:embed \"templates/*\"\nvar Templates embed.FS\n",
        )
        .unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let report = engine.audit_freshness(FreshnessOptions {
            threshold_days: 0,
            include_pattern: None,
            exclude_patterns: Vec::new(),
            profile: AuditProfile::Mixed,
        });

        let embed_entry = report
            .entries
            .iter()
            .find(|e| e.file_path.ends_with("web/embed.go"))
            .expect("web/embed.go should appear in the audit");
        assert_eq!(
            embed_entry.tier,
            FreshnessTier::Info,
            "quoted go-embed root should still be Info, got {:?}",
            embed_entry.tier
        );
    }

    #[test]
    fn go_embed_root_protected_from_critical_dead_code_flag() {
        // Regression for #103: `web/embed.go` exists solely to expose a
        // `//go:embed` filesystem to the rest of the program. Nothing in
        // the import graph imports it directly, so it would otherwise
        // show up as `Critical` dead code. With Go-embed detection it
        // gets the protected `Info` label instead.
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("web/templates")).unwrap();
        fs::write(
            root.join("web/templates/index.html"),
            "<html><body>hi</body></html>\n",
        )
        .unwrap();
        fs::write(
            root.join("web/embed.go"),
            "package web\n\nimport \"embed\"\n\n//go:embed templates\nvar Templates embed.FS\n",
        )
        .unwrap();

        let mut config = IndexConfig::new(&root);
        config.embedding.enabled = false;
        let engine = Engine::init(&root, config).unwrap();

        let report = engine.audit_freshness(FreshnessOptions {
            threshold_days: 0,
            include_pattern: None,
            exclude_patterns: Vec::new(),
            profile: AuditProfile::Mixed,
        });

        let embed_entry = report
            .entries
            .iter()
            .find(|e| e.file_path.ends_with("web/embed.go"))
            .expect("web/embed.go should appear in the audit");
        assert_eq!(
            embed_entry.tier,
            FreshnessTier::Info,
            "go-embed root file should be Info, got {:?}",
            embed_entry.tier
        );
        assert!(
            embed_entry.reason.contains("Go embed"),
            "reason should mention Go embed: {}",
            embed_entry.reason
        );
    }
}
