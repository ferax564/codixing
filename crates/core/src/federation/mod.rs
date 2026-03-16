//! Cross-repo federated search.
//!
//! Wraps multiple [`Engine`] instances and presents a unified search surface
//! using multi-list RRF fusion with per-project boost weights.
//!
//! # Architecture
//!
//! Each registered project has its own independent `Engine` instance backed by
//! its own `.codixing/` index directory.  `FederatedEngine` fans out queries to
//! all loaded engines, prefixes file paths with the project name for
//! disambiguation, and fuses the per-project ranked lists via Reciprocal Rank
//! Fusion (RRF).
//!
//! # Lazy loading & LRU eviction
//!
//! When `lazy_load` is enabled (the default), engines are opened on first
//! query.  When the number of resident engines exceeds `max_resident`, the
//! least-recently-used engine is evicted (its `Option<Engine>` is set back to
//! `None`).

pub mod config;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tracing::{info, warn};

use crate::error::Result;
use crate::symbols::Symbol;
use crate::{Engine, SearchQuery, SearchResult};

pub use config::FederationConfig;

/// A search result annotated with its source project.
#[derive(Debug, Clone)]
pub struct FederatedResult {
    /// The underlying search result (with `file_path` prefixed by project name).
    pub result: SearchResult,
    /// Human-readable project name (directory name of the project root).
    pub project: String,
    /// Absolute root path of the project.
    pub project_root: PathBuf,
}

/// Aggregate statistics across all loaded engines.
#[derive(Debug, Clone)]
pub struct FederatedStats {
    /// Number of registered projects.
    pub project_count: usize,
    /// Number of currently loaded (resident) engines.
    pub loaded_count: usize,
    /// Total files across all loaded engines.
    pub total_files: usize,
    /// Total chunks across all loaded engines.
    pub total_chunks: usize,
    /// Total symbols across all loaded engines.
    pub total_symbols: usize,
}

/// Information about a registered project.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    /// Human-readable project name.
    pub name: String,
    /// Absolute root path.
    pub root: PathBuf,
    /// Whether the engine is currently loaded in memory.
    pub loaded: bool,
    /// Number of indexed files (0 if not loaded).
    pub file_count: usize,
}

/// A project slot: root path, derived name, per-project weight, and the
/// optionally-loaded engine behind a `RwLock`.
struct ProjectSlot {
    root: PathBuf,
    name: String,
    weight: f32,
    engine: Arc<RwLock<Option<Engine>>>,
}

/// Federated engine that wraps multiple independent `Engine` instances.
pub struct FederatedEngine {
    /// Registered projects.
    slots: Vec<ProjectSlot>,
    /// Federation-level configuration.
    config: FederationConfig,
    /// LRU order: front = least recently used, back = most recently used.
    /// Values are indices into `slots`.
    lru_order: Mutex<VecDeque<usize>>,
}

impl FederatedEngine {
    /// Create a new federated engine from a [`FederationConfig`].
    ///
    /// If `lazy_load` is `false`, all engines are opened immediately.
    /// Projects whose root does not contain a `.codixing/` index are skipped
    /// with a warning.
    pub fn new(config: FederationConfig) -> Result<Self> {
        let mut slots = Vec::with_capacity(config.projects.len());
        let mut lru_order = VecDeque::new();

        for entry in &config.projects {
            let root = match entry.root.canonicalize() {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        root = %entry.root.display(),
                        error = %e,
                        "federation: skipping project — root path not found"
                    );
                    continue;
                }
            };

            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.to_string_lossy().to_string());

            let engine = if !config.lazy_load {
                if !Engine::index_exists(&root) {
                    warn!(
                        root = %root.display(),
                        "federation: skipping project — no .codixing/ index found"
                    );
                    continue;
                }
                match Engine::open(&root) {
                    Ok(e) => {
                        info!(project = %name, root = %root.display(), "federation: loaded engine");
                        Some(e)
                    }
                    Err(e) => {
                        warn!(
                            project = %name,
                            root = %root.display(),
                            error = %e,
                            "federation: failed to open engine — skipping"
                        );
                        continue;
                    }
                }
            } else {
                None
            };

            let idx = slots.len();
            if engine.is_some() {
                lru_order.push_back(idx);
            }

            slots.push(ProjectSlot {
                root,
                name,
                weight: entry.weight,
                engine: Arc::new(RwLock::new(engine)),
            });
        }

        Ok(Self {
            slots,
            config,
            lru_order: Mutex::new(lru_order),
        })
    }

    /// Ensure the engine at `slot_index` is loaded, performing LRU eviction
    /// if necessary.  Returns `true` if the engine is available after this
    /// call.
    fn ensure_loaded(&self, slot_index: usize) -> bool {
        let slot = &self.slots[slot_index];

        // Fast path: already loaded.
        {
            let guard = slot.engine.read().expect("engine lock poisoned");
            if guard.is_some() {
                // Touch LRU.
                self.touch_lru(slot_index);
                return true;
            }
        }

        // Slow path: need to open the engine.
        if !Engine::index_exists(&slot.root) {
            warn!(
                project = %slot.name,
                root = %slot.root.display(),
                "federation: cannot load — no .codixing/ index"
            );
            return false;
        }

        // Evict if necessary (before acquiring write lock on the target slot,
        // to avoid potential deadlocks).
        self.maybe_evict();

        let mut guard = slot.engine.write().expect("engine lock poisoned");
        // Double-check after acquiring the write lock.
        if guard.is_some() {
            self.touch_lru(slot_index);
            return true;
        }

        match Engine::open(&slot.root) {
            Ok(engine) => {
                info!(
                    project = %slot.name,
                    root = %slot.root.display(),
                    "federation: lazy-loaded engine"
                );
                *guard = Some(engine);
                self.touch_lru(slot_index);
                true
            }
            Err(e) => {
                warn!(
                    project = %slot.name,
                    root = %slot.root.display(),
                    error = %e,
                    "federation: failed to lazy-load engine"
                );
                false
            }
        }
    }

    /// Move `slot_index` to the back of the LRU queue (most recently used).
    fn touch_lru(&self, slot_index: usize) {
        let mut lru = self.lru_order.lock().expect("lru lock poisoned");
        lru.retain(|&i| i != slot_index);
        lru.push_back(slot_index);
    }

    /// If the number of loaded engines exceeds `max_resident`, evict the LRU
    /// engine.
    fn maybe_evict(&self) {
        let mut lru = self.lru_order.lock().expect("lru lock poisoned");
        while lru.len() >= self.config.max_resident {
            if let Some(victim) = lru.pop_front() {
                let slot = &self.slots[victim];
                let mut guard = slot.engine.write().expect("engine lock poisoned");
                if guard.is_some() {
                    info!(
                        project = %slot.name,
                        "federation: evicting engine (LRU)"
                    );
                    *guard = None;
                }
            }
        }
    }

    /// Search across all projects, fusing results via multi-list RRF.
    pub fn search(&self, query: SearchQuery) -> Result<Vec<FederatedResult>> {
        let limit = query.limit;
        let mut per_project: Vec<(String, PathBuf, f32, Vec<SearchResult>)> = Vec::new();

        for (idx, slot) in self.slots.iter().enumerate() {
            if !self.ensure_loaded(idx) {
                continue;
            }

            let guard = slot.engine.read().expect("engine lock poisoned");
            if let Some(engine) = guard.as_ref() {
                match engine.search(query.clone()) {
                    Ok(results) => {
                        per_project.push((
                            slot.name.clone(),
                            slot.root.clone(),
                            slot.weight,
                            results,
                        ));
                    }
                    Err(e) => {
                        warn!(
                            project = %slot.name,
                            error = %e,
                            "federation: search failed in project — skipping"
                        );
                    }
                }
            }
        }

        Ok(federate_results(per_project, self.config.rrf_k, limit))
    }

    /// Find symbols by name across all loaded engines.
    ///
    /// Returns `(project_name, Symbol)` pairs.
    pub fn find_symbol(&self, name: &str) -> Result<Vec<(String, Symbol)>> {
        let mut all_symbols = Vec::new();

        for (idx, slot) in self.slots.iter().enumerate() {
            if !self.ensure_loaded(idx) {
                continue;
            }

            let guard = slot.engine.read().expect("engine lock poisoned");
            if let Some(engine) = guard.as_ref() {
                match engine.symbols(name, None) {
                    Ok(symbols) => {
                        for sym in symbols {
                            all_symbols.push((slot.name.clone(), sym));
                        }
                    }
                    Err(e) => {
                        warn!(
                            project = %slot.name,
                            error = %e,
                            "federation: symbol lookup failed — skipping"
                        );
                    }
                }
            }
        }

        Ok(all_symbols)
    }

    /// Aggregate statistics across all loaded engines.
    pub fn stats(&self) -> FederatedStats {
        let mut loaded_count = 0usize;
        let mut total_files = 0usize;
        let mut total_chunks = 0usize;
        let mut total_symbols = 0usize;

        for slot in &self.slots {
            let guard = slot.engine.read().expect("engine lock poisoned");
            if let Some(engine) = guard.as_ref() {
                loaded_count += 1;
                let s = engine.stats();
                total_files += s.file_count;
                total_chunks += s.chunk_count;
                total_symbols += s.symbol_count;
            }
        }

        FederatedStats {
            project_count: self.slots.len(),
            loaded_count,
            total_files,
            total_chunks,
            total_symbols,
        }
    }

    /// List all registered projects with their load status.
    pub fn projects(&self) -> Vec<ProjectInfo> {
        self.slots
            .iter()
            .map(|slot| {
                let guard = slot.engine.read().expect("engine lock poisoned");
                let (loaded, file_count) = match guard.as_ref() {
                    Some(engine) => (true, engine.stats().file_count),
                    None => (false, 0),
                };
                ProjectInfo {
                    name: slot.name.clone(),
                    root: slot.root.clone(),
                    loaded,
                    file_count,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Multi-list RRF fusion
// ---------------------------------------------------------------------------

/// Fuse per-project search results using Reciprocal Rank Fusion (RRF).
///
/// For each result `d` appearing at rank `r` in project `p`:
///
/// ```text
/// score(d) += weight[p] / (k + r + 1)
/// ```
///
/// Results are de-duplicated by `chunk_id`, sorted by fused score (descending),
/// and truncated to `limit`.
fn federate_results(
    per_project: Vec<(String, PathBuf, f32, Vec<SearchResult>)>,
    rrf_k: f32,
    limit: usize,
) -> Vec<FederatedResult> {
    let mut scores: HashMap<String, (f32, FederatedResult)> = HashMap::new();

    for (project_name, project_root, weight, results) in per_project {
        for (rank, mut result) in results.into_iter().enumerate() {
            // Prefix file path with project name for cross-project disambiguation.
            let original_path = result.file_path.clone();
            result.file_path = format!("{}/{}", project_name, original_path);

            let key = format!("{}:{}", project_name, result.chunk_id);
            let rrf_score = weight / (rrf_k + rank as f32 + 1.0);

            scores
                .entry(key)
                .and_modify(|(s, _)| *s += rrf_score)
                .or_insert((
                    rrf_score,
                    FederatedResult {
                        result,
                        project: project_name.clone(),
                        project_root: project_root.clone(),
                    },
                ));
        }
    }

    let mut ranked: Vec<_> = scores.into_values().collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(limit);
    ranked
        .into_iter()
        .map(|(score, mut fr)| {
            fr.result.score = score;
            fr
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retriever::SearchResult;

    fn make_result(chunk_id: &str, file_path: &str, score: f32) -> SearchResult {
        SearchResult {
            chunk_id: chunk_id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            score,
            line_start: 0,
            line_end: 10,
            signature: String::new(),
            scope_chain: vec![],
            content: format!("content of {chunk_id}"),
        }
    }

    #[test]
    fn test_federate_results_rrf() {
        // Two projects, each returning 2 results.
        let per_project = vec![
            (
                "alpha".to_string(),
                PathBuf::from("/alpha"),
                1.0,
                vec![
                    make_result("a1", "src/lib.rs", 10.0),
                    make_result("a2", "src/main.rs", 8.0),
                ],
            ),
            (
                "beta".to_string(),
                PathBuf::from("/beta"),
                1.0,
                vec![
                    make_result("b1", "src/app.rs", 9.0),
                    make_result("b2", "src/util.rs", 7.0),
                ],
            ),
        ];

        let fused = federate_results(per_project, 60.0, 10);
        assert_eq!(fused.len(), 4);

        // The top two should be the rank-0 results from each project
        // (both have RRF score 1.0 / (60 + 0 + 1) = 0.01639...).
        let top_two_projects: Vec<&str> = fused[..2].iter().map(|r| r.project.as_str()).collect();
        assert!(top_two_projects.contains(&"alpha"));
        assert!(top_two_projects.contains(&"beta"));

        // Scores should be monotonically non-increasing.
        for window in fused.windows(2) {
            assert!(window[0].result.score >= window[1].result.score);
        }
    }

    #[test]
    fn test_federate_results_project_prefix() {
        let per_project = vec![(
            "my-project".to_string(),
            PathBuf::from("/my-project"),
            1.0,
            vec![make_result("c1", "src/lib.rs", 5.0)],
        )];

        let fused = federate_results(per_project, 60.0, 10);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].result.file_path, "my-project/src/lib.rs");
        assert_eq!(fused[0].project, "my-project");
    }

    #[test]
    fn test_project_weights_affect_ranking() {
        // Project alpha has weight 1.0, beta has weight 3.0.
        // Both return a single result at rank 0, so:
        //   alpha: 1.0 / (60 + 0 + 1) = 0.01639
        //   beta:  3.0 / (60 + 0 + 1) = 0.04918
        // Beta should rank higher.
        let per_project = vec![
            (
                "alpha".to_string(),
                PathBuf::from("/alpha"),
                1.0,
                vec![make_result("a1", "src/lib.rs", 10.0)],
            ),
            (
                "beta".to_string(),
                PathBuf::from("/beta"),
                3.0,
                vec![make_result("b1", "src/lib.rs", 5.0)],
            ),
        ];

        let fused = federate_results(per_project, 60.0, 10);
        assert_eq!(fused.len(), 2);
        assert_eq!(
            fused[0].project, "beta",
            "higher-weighted project should rank first"
        );
        assert_eq!(fused[1].project, "alpha");

        // Verify the actual scores.
        let beta_expected = 3.0 / 61.0;
        let alpha_expected = 1.0 / 61.0;
        assert!(
            (fused[0].result.score - beta_expected).abs() < 1e-6,
            "beta score mismatch: {} vs {}",
            fused[0].result.score,
            beta_expected
        );
        assert!(
            (fused[1].result.score - alpha_expected).abs() < 1e-6,
            "alpha score mismatch: {} vs {}",
            fused[1].result.score,
            alpha_expected
        );
    }

    #[test]
    fn test_federate_results_truncation() {
        let results: Vec<SearchResult> = (0..20)
            .map(|i| make_result(&format!("c{i}"), &format!("src/f{i}.rs"), 10.0 - i as f32))
            .collect();

        let per_project = vec![("proj".to_string(), PathBuf::from("/proj"), 1.0, results)];

        let fused = federate_results(per_project, 60.0, 5);
        assert_eq!(fused.len(), 5, "should truncate to limit");
    }

    #[test]
    fn test_federation_config_parsing() {
        let json = r#"{
            "projects": [
                { "root": "/a" },
                { "root": "/b", "weight": 2.0 }
            ],
            "rrf_k": 42.0
        }"#;
        let cfg: FederationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.projects.len(), 2);
        assert!((cfg.rrf_k - 42.0).abs() < f32::EPSILON);
    }
}
