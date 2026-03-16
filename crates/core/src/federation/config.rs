//! Federation configuration: parse `codixing-federation.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CodixingError, Result};

/// Top-level federation configuration.
///
/// Loaded from a `codixing-federation.json` file.
///
/// ```json
/// {
///     "projects": [
///         { "root": "/path/to/project-a" },
///         { "root": "/path/to/project-b", "weight": 1.2 }
///     ],
///     "rrf_k": 60.0,
///     "lazy_load": true,
///     "max_resident": 5
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationConfig {
    /// List of projects to federate over.
    pub projects: Vec<ProjectEntry>,
    /// RRF constant `k` (default 60.0).  Higher values flatten rank differences.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f32,
    /// When `true` (the default), engines are loaded on first query rather than
    /// at startup.
    #[serde(default = "default_lazy_load")]
    pub lazy_load: bool,
    /// Maximum number of engines held in memory simultaneously.
    /// Beyond this limit the least-recently-used engine is evicted.
    #[serde(default = "default_max_resident")]
    pub max_resident: usize,
}

/// A single project entry in the federation config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Root directory of the project (must contain a `.codixing/` index).
    pub root: PathBuf,
    /// Per-project weight applied during RRF fusion (default 1.0).
    /// Higher values rank this project's results higher.
    #[serde(default = "default_weight")]
    pub weight: f32,
}

fn default_rrf_k() -> f32 {
    60.0
}
fn default_lazy_load() -> bool {
    true
}
fn default_max_resident() -> usize {
    5
}
fn default_weight() -> f32 {
    1.0
}

impl FederationConfig {
    /// Load a federation config from a JSON file on disk.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            CodixingError::Config(format!(
                "failed to read federation config at {}: {e}",
                path.display()
            ))
        })?;
        let config: Self = serde_json::from_str(&content).map_err(|e| {
            CodixingError::Config(format!(
                "failed to parse federation config at {}: {e}",
                path.display()
            ))
        })?;
        Ok(config)
    }

    /// Derive a `project_name -> weight` mapping.
    ///
    /// The project name is the last component of the root path (i.e. the
    /// directory name).
    pub fn project_weights(&self) -> HashMap<String, f32> {
        self.projects
            .iter()
            .map(|p| {
                let name = p
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                (name, p.weight)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_federation_config_parsing() {
        let json = r#"{
            "projects": [
                { "root": "/home/user/project-a" },
                { "root": "/home/user/project-b", "weight": 1.5 }
            ],
            "rrf_k": 42.0,
            "lazy_load": false,
            "max_resident": 3
        }"#;
        let cfg: FederationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.projects.len(), 2);
        assert!((cfg.rrf_k - 42.0).abs() < f32::EPSILON);
        assert!(!cfg.lazy_load);
        assert_eq!(cfg.max_resident, 3);
        assert!((cfg.projects[0].weight - 1.0).abs() < f32::EPSILON); // default
        assert!((cfg.projects[1].weight - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_federation_config_defaults() {
        let json = r#"{ "projects": [{ "root": "/a" }] }"#;
        let cfg: FederationConfig = serde_json::from_str(json).unwrap();
        assert!((cfg.rrf_k - 60.0).abs() < f32::EPSILON);
        assert!(cfg.lazy_load);
        assert_eq!(cfg.max_resident, 5);
    }

    #[test]
    fn test_project_weights() {
        let json = r#"{
            "projects": [
                { "root": "/home/user/alpha" },
                { "root": "/home/user/beta", "weight": 2.0 }
            ]
        }"#;
        let cfg: FederationConfig = serde_json::from_str(json).unwrap();
        let weights = cfg.project_weights();
        assert!((weights["alpha"] - 1.0).abs() < f32::EPSILON);
        assert!((weights["beta"] - 2.0).abs() < f32::EPSILON);
    }
}
