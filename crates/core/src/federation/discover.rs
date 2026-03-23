//! Auto-discovery of workspace projects for federation configuration.
//!
//! Scans a root directory for multi-project workspace patterns (Cargo, npm,
//! pnpm, Go, Git submodules, nested repos) and returns a list of discovered
//! projects ready to be added to a [`FederationConfig`].
//!
//! # Supported Workspace Types
//!
//! | Pattern | Detection |
//! |---|---|
//! | Cargo workspace | `Cargo.toml` with `[workspace].members` globs |
//! | npm/pnpm workspace | `package.json` with `"workspaces"` array, or `pnpm-workspace.yaml` |
//! | Go workspace | `go.work` file with `use` directives |
//! | Git submodules | `.gitmodules` file |
//! | Nested projects | Directories (depth 1-2) containing `Cargo.toml`, `package.json`, or `go.mod` |

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::config::{FederationConfig, ProjectEntry};

/// Maximum directory depth for nested project scanning.
const MAX_SCAN_DEPTH: usize = 3;

/// A project discovered by the auto-discovery scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredProject {
    /// Absolute path to the project root.
    pub root: PathBuf,
    /// Human-readable project name (last path component).
    pub name: String,
    /// How this project was detected.
    pub project_type: ProjectType,
    /// Suggested RRF weight based on project type.
    pub weight: f32,
}

/// How a project was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProjectType {
    /// Member of a Cargo workspace (`Cargo.toml` `[workspace].members`).
    CargoWorkspace,
    /// Member of an npm workspace (`package.json` `"workspaces"`).
    NpmWorkspace,
    /// Member of a pnpm workspace (`pnpm-workspace.yaml`).
    PnpmWorkspace,
    /// Member of a Go workspace (`go.work` `use` directives).
    GoWorkspace,
    /// Monorepo subdirectory with its own project manifest.
    MonorepoPackage,
    /// Git submodule (`.gitmodules`).
    GitSubmodule,
    /// Nested directory with its own `.codixing/` index.
    NestedIndexed,
}

impl fmt::Display for ProjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CargoWorkspace => write!(f, "cargo-workspace"),
            Self::NpmWorkspace => write!(f, "npm-workspace"),
            Self::PnpmWorkspace => write!(f, "pnpm-workspace"),
            Self::GoWorkspace => write!(f, "go-workspace"),
            Self::MonorepoPackage => write!(f, "monorepo-package"),
            Self::GitSubmodule => write!(f, "git-submodule"),
            Self::NestedIndexed => write!(f, "nested-indexed"),
        }
    }
}

/// Discover projects under `root` by probing all supported workspace types.
///
/// Returns a deduplicated list of projects, sorted by path.
pub fn discover_projects(root: &Path) -> Vec<DiscoveredProject> {
    let mut projects = Vec::new();

    // 1. Cargo workspace members
    discover_cargo_workspace(root, &mut projects);

    // 2. npm / pnpm workspaces
    discover_npm_workspaces(root, &mut projects);

    // 3. Go workspace (go.work)
    discover_go_workspace(root, &mut projects);

    // 4. Git submodules
    discover_git_submodules(root, &mut projects);

    // 5. Nested projects (directories with manifests or .codixing/ indexes)
    discover_nested_projects(root, &mut projects);

    // Deduplicate by canonical path
    dedup_projects(&mut projects);

    // Sort by path for deterministic output
    projects.sort_by(|a, b| a.root.cmp(&b.root));

    projects
}

/// Convert a list of discovered projects into a [`FederationConfig`].
pub fn to_federation_config(projects: &[DiscoveredProject]) -> FederationConfig {
    FederationConfig {
        projects: projects
            .iter()
            .map(|p| ProjectEntry {
                root: p.root.clone(),
                weight: p.weight,
            })
            .collect(),
        rrf_k: 60.0,
        lazy_load: true,
        max_resident: 5,
    }
}

// ---------------------------------------------------------------------------
// Cargo workspace discovery
// ---------------------------------------------------------------------------

/// Parse `Cargo.toml` at `root` and discover workspace members.
fn discover_cargo_workspace(root: &Path, projects: &mut Vec<DiscoveredProject>) {
    let cargo_toml = root.join("Cargo.toml");
    if !cargo_toml.is_file() {
        return;
    }

    let content = match std::fs::read_to_string(&cargo_toml) {
        Ok(c) => c,
        Err(e) => {
            debug!(path = %cargo_toml.display(), error = %e, "cannot read Cargo.toml");
            return;
        }
    };

    let table: toml::Value = match content.parse() {
        Ok(t) => t,
        Err(e) => {
            debug!(path = %cargo_toml.display(), error = %e, "cannot parse Cargo.toml");
            return;
        }
    };

    // Extract [workspace].members array
    let members = table
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array());

    let Some(members) = members else {
        return;
    };

    for member in members {
        let Some(pattern) = member.as_str() else {
            continue;
        };

        // Expand glob pattern relative to root
        let abs_pattern = root.join(pattern);
        let pattern_str = abs_pattern.to_string_lossy().to_string();

        match glob::glob(&pattern_str) {
            Ok(paths) => {
                for entry in paths.flatten() {
                    if entry.is_dir() && entry != root {
                        add_project(projects, &entry, ProjectType::CargoWorkspace, 1.0);
                    }
                }
            }
            Err(e) => {
                debug!(pattern = %pattern_str, error = %e, "invalid Cargo workspace glob");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// npm / pnpm workspace discovery
// ---------------------------------------------------------------------------

/// Parse `package.json` workspaces or `pnpm-workspace.yaml` at `root`.
fn discover_npm_workspaces(root: &Path, projects: &mut Vec<DiscoveredProject>) {
    // Try package.json "workspaces" field first
    let pkg_json = root.join("package.json");
    if pkg_json.is_file() {
        if let Ok(content) = std::fs::read_to_string(&pkg_json) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(workspaces) = json.get("workspaces").and_then(|w| w.as_array()) {
                    for ws in workspaces {
                        if let Some(pattern) = ws.as_str() {
                            expand_workspace_glob(
                                root,
                                pattern,
                                ProjectType::NpmWorkspace,
                                projects,
                            );
                        }
                    }
                    return; // Found npm workspaces, skip pnpm check
                }
            }
        }
    }

    // Try pnpm-workspace.yaml
    let pnpm_yaml = root.join("pnpm-workspace.yaml");
    if pnpm_yaml.is_file() {
        if let Ok(content) = std::fs::read_to_string(&pnpm_yaml) {
            // Simple line-based parser for pnpm-workspace.yaml:
            //   packages:
            //     - 'packages/*'
            //     - 'apps/*'
            let mut in_packages = false;
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed == "packages:" {
                    in_packages = true;
                    continue;
                }
                if in_packages {
                    if trimmed.starts_with("- ") {
                        let pattern = trimmed
                            .trim_start_matches("- ")
                            .trim_matches('\'')
                            .trim_matches('"');
                        expand_workspace_glob(root, pattern, ProjectType::PnpmWorkspace, projects);
                    } else if !trimmed.is_empty() && !trimmed.starts_with('#') {
                        // New top-level key, stop parsing packages
                        break;
                    }
                }
            }
        }
    }
}

/// Expand a workspace glob pattern and add matching directories as projects.
fn expand_workspace_glob(
    root: &Path,
    pattern: &str,
    project_type: ProjectType,
    projects: &mut Vec<DiscoveredProject>,
) {
    let abs_pattern = root.join(pattern);
    let pattern_str = abs_pattern.to_string_lossy().to_string();

    match glob::glob(&pattern_str) {
        Ok(paths) => {
            for entry in paths.flatten() {
                if entry.is_dir() && entry != root {
                    // For npm/pnpm, verify the directory has a package.json
                    let has_manifest = entry.join("package.json").is_file();
                    if has_manifest {
                        add_project(projects, &entry, project_type, 1.0);
                    }
                }
            }
        }
        Err(e) => {
            debug!(pattern = %pattern_str, error = %e, "invalid workspace glob");
        }
    }
}

// ---------------------------------------------------------------------------
// Go workspace discovery
// ---------------------------------------------------------------------------

/// Parse `go.work` at `root` and discover workspace modules.
fn discover_go_workspace(root: &Path, projects: &mut Vec<DiscoveredProject>) {
    let go_work = root.join("go.work");
    if !go_work.is_file() {
        return;
    }

    let content = match std::fs::read_to_string(&go_work) {
        Ok(c) => c,
        Err(e) => {
            debug!(path = %go_work.display(), error = %e, "cannot read go.work");
            return;
        }
    };

    // go.work format:
    //   go 1.21
    //   use (
    //       ./cmd/foo
    //       ./pkg/bar
    //   )
    // Or single-line: use ./cmd/foo
    let mut in_use_block = false;
    for line in content.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("use (") {
            in_use_block = true;
            continue;
        }
        if in_use_block && trimmed == ")" {
            in_use_block = false;
            continue;
        }

        let dir = if in_use_block {
            // Inside use ( ... ) block
            Some(trimmed.trim_matches('.').trim_start_matches('/'))
        } else if let Some(rest) = trimmed.strip_prefix("use ") {
            // Single-line use directive
            let d = rest.trim().trim_matches('.').trim_start_matches('/');
            Some(d)
        } else {
            None
        };

        if let Some(dir_str) = dir {
            if dir_str.is_empty() {
                continue;
            }
            // Reconstruct relative path from root
            let candidate = if trimmed.contains("./") {
                root.join(trimmed.strip_prefix("use ").unwrap_or(trimmed).trim())
            } else if in_use_block {
                root.join(trimmed)
            } else {
                continue;
            };

            if candidate.is_dir() {
                add_project(projects, &candidate, ProjectType::GoWorkspace, 1.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Git submodule discovery
// ---------------------------------------------------------------------------

/// Parse `.gitmodules` at `root` and discover submodules.
fn discover_git_submodules(root: &Path, projects: &mut Vec<DiscoveredProject>) {
    let gitmodules = root.join(".gitmodules");
    if !gitmodules.is_file() {
        return;
    }

    let content = match std::fs::read_to_string(&gitmodules) {
        Ok(c) => c,
        Err(e) => {
            debug!(path = %gitmodules.display(), error = %e, "cannot read .gitmodules");
            return;
        }
    };

    // .gitmodules format:
    //   [submodule "vendor/lib"]
    //       path = vendor/lib
    //       url = https://...
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("path") {
            let rest = rest.trim();
            if let Some(path_val) = rest.strip_prefix('=') {
                let submodule_path = root.join(path_val.trim());
                if submodule_path.is_dir() {
                    add_project(
                        projects,
                        &submodule_path,
                        ProjectType::GitSubmodule,
                        0.8, // slightly lower default weight for submodules
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Nested project discovery
// ---------------------------------------------------------------------------

/// Walk directories up to `MAX_SCAN_DEPTH` looking for project manifests
/// or existing `.codixing/` indexes.
fn discover_nested_projects(root: &Path, projects: &mut Vec<DiscoveredProject>) {
    walk_for_projects(root, root, 0, projects);
}

fn walk_for_projects(root: &Path, dir: &Path, depth: usize, projects: &mut Vec<DiscoveredProject>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // Skip hidden directories and common non-project directories
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.')
            || name_str == "node_modules"
            || name_str == "target"
            || name_str == "vendor"
            || name_str == "dist"
            || name_str == "build"
            || name_str == "__pycache__"
        {
            continue;
        }

        // Skip if this is the root itself
        if path == root {
            continue;
        }

        // Check for existing .codixing/ index (highest priority)
        if path.join(".codixing").is_dir() {
            add_project(projects, &path, ProjectType::NestedIndexed, 1.0);
            continue; // Don't recurse into already-indexed projects
        }

        // Check for project manifests (only at depth 1-2)
        if depth < 2 {
            let has_cargo = path.join("Cargo.toml").is_file();
            let has_package_json = path.join("package.json").is_file();
            let has_go_mod = path.join("go.mod").is_file();

            if has_cargo || has_package_json || has_go_mod {
                add_project(projects, &path, ProjectType::MonorepoPackage, 1.0);
                // Don't recurse into discovered packages
                continue;
            }
        }

        // Recurse into subdirectories
        walk_for_projects(root, &path, depth + 1, projects);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Add a project to the list, canonicalizing the path.
fn add_project(
    projects: &mut Vec<DiscoveredProject>,
    path: &Path,
    project_type: ProjectType,
    weight: f32,
) {
    let root = match path.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "cannot canonicalize project path");
            return;
        }
    };

    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| root.display().to_string());

    projects.push(DiscoveredProject {
        root,
        name,
        project_type,
        weight,
    });
}

/// Deduplicate projects by canonical path, keeping the first occurrence
/// (which is from a more specific discovery strategy).
fn dedup_projects(projects: &mut Vec<DiscoveredProject>) {
    let mut seen = HashSet::new();
    projects.retain(|p| seen.insert(p.root.clone()));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a temporary directory structure for testing.
    fn setup_cargo_workspace(dir: &Path) {
        fs::write(
            dir.join("Cargo.toml"),
            r#"
[workspace]
members = ["crates/*"]
"#,
        )
        .unwrap();

        let crate_a = dir.join("crates/alpha");
        let crate_b = dir.join("crates/beta");
        fs::create_dir_all(&crate_a).unwrap();
        fs::create_dir_all(&crate_b).unwrap();
        fs::write(crate_a.join("Cargo.toml"), "[package]\nname = \"alpha\"").unwrap();
        fs::write(crate_b.join("Cargo.toml"), "[package]\nname = \"beta\"").unwrap();
    }

    fn setup_npm_workspace(dir: &Path) {
        fs::write(
            dir.join("package.json"),
            r#"{
                "name": "my-monorepo",
                "workspaces": ["packages/*"]
            }"#,
        )
        .unwrap();

        let pkg_a = dir.join("packages/app");
        let pkg_b = dir.join("packages/lib");
        fs::create_dir_all(&pkg_a).unwrap();
        fs::create_dir_all(&pkg_b).unwrap();
        fs::write(pkg_a.join("package.json"), r#"{"name": "app"}"#).unwrap();
        fs::write(pkg_b.join("package.json"), r#"{"name": "lib"}"#).unwrap();
    }

    fn setup_pnpm_workspace(dir: &Path) {
        fs::write(
            dir.join("pnpm-workspace.yaml"),
            "packages:\n  - 'apps/*'\n  - 'libs/*'\n",
        )
        .unwrap();
        // Also need a package.json (without workspaces) so npm discovery doesn't trigger
        fs::write(dir.join("package.json"), r#"{"name": "pnpm-root"}"#).unwrap();

        let app = dir.join("apps/web");
        let lib = dir.join("libs/shared");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::write(app.join("package.json"), r#"{"name": "web"}"#).unwrap();
        fs::write(lib.join("package.json"), r#"{"name": "shared"}"#).unwrap();
    }

    fn setup_go_workspace(dir: &Path) {
        fs::write(
            dir.join("go.work"),
            "go 1.21\n\nuse (\n\t./cmd/api\n\t./pkg/core\n)\n",
        )
        .unwrap();

        let cmd_api = dir.join("cmd/api");
        let pkg_core = dir.join("pkg/core");
        fs::create_dir_all(&cmd_api).unwrap();
        fs::create_dir_all(&pkg_core).unwrap();
        fs::write(cmd_api.join("go.mod"), "module example.com/cmd/api").unwrap();
        fs::write(pkg_core.join("go.mod"), "module example.com/pkg/core").unwrap();
    }

    fn setup_git_submodules(dir: &Path) {
        let vendor_lib = dir.join("vendor/lib");
        fs::create_dir_all(&vendor_lib).unwrap();

        fs::write(
            dir.join(".gitmodules"),
            "[submodule \"vendor/lib\"]\n\tpath = vendor/lib\n\turl = https://example.com/lib.git\n",
        )
        .unwrap();
    }

    #[test]
    fn test_discover_cargo_workspace() {
        let dir = tempfile::tempdir().unwrap();
        setup_cargo_workspace(dir.path());

        let mut projects = Vec::new();
        discover_cargo_workspace(dir.path(), &mut projects);

        assert_eq!(projects.len(), 2);
        let names: HashSet<String> = projects.iter().map(|p| p.name.clone()).collect();
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
        assert!(
            projects
                .iter()
                .all(|p| p.project_type == ProjectType::CargoWorkspace)
        );
    }

    #[test]
    fn test_discover_npm_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        setup_npm_workspace(dir.path());

        let mut projects = Vec::new();
        discover_npm_workspaces(dir.path(), &mut projects);

        assert_eq!(projects.len(), 2);
        let names: HashSet<String> = projects.iter().map(|p| p.name.clone()).collect();
        assert!(names.contains("app"));
        assert!(names.contains("lib"));
        assert!(
            projects
                .iter()
                .all(|p| p.project_type == ProjectType::NpmWorkspace)
        );
    }

    #[test]
    fn test_discover_pnpm_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        setup_pnpm_workspace(dir.path());

        let mut projects = Vec::new();
        discover_npm_workspaces(dir.path(), &mut projects);

        assert_eq!(projects.len(), 2);
        let names: HashSet<String> = projects.iter().map(|p| p.name.clone()).collect();
        assert!(names.contains("web"));
        assert!(names.contains("shared"));
        assert!(
            projects
                .iter()
                .all(|p| p.project_type == ProjectType::PnpmWorkspace)
        );
    }

    #[test]
    fn test_discover_go_workspace() {
        let dir = tempfile::tempdir().unwrap();
        setup_go_workspace(dir.path());

        let mut projects = Vec::new();
        discover_go_workspace(dir.path(), &mut projects);

        assert_eq!(projects.len(), 2);
        let names: HashSet<String> = projects.iter().map(|p| p.name.clone()).collect();
        assert!(names.contains("api"));
        assert!(names.contains("core"));
        assert!(
            projects
                .iter()
                .all(|p| p.project_type == ProjectType::GoWorkspace)
        );
    }

    #[test]
    fn test_discover_git_submodules() {
        let dir = tempfile::tempdir().unwrap();
        setup_git_submodules(dir.path());

        let mut projects = Vec::new();
        discover_git_submodules(dir.path(), &mut projects);

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "lib");
        assert_eq!(projects[0].project_type, ProjectType::GitSubmodule);
        assert!((projects[0].weight - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn test_discover_nested_projects() {
        let dir = tempfile::tempdir().unwrap();

        // Create a nested project with a Cargo.toml
        let nested = dir.path().join("services/auth");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("Cargo.toml"), "[package]\nname = \"auth\"").unwrap();

        // Create a project with .codixing/ index
        let indexed = dir.path().join("libs/indexed-lib");
        fs::create_dir_all(indexed.join(".codixing")).unwrap();

        let mut projects = Vec::new();
        discover_nested_projects(dir.path(), &mut projects);

        assert_eq!(projects.len(), 2);
        let names: HashSet<String> = projects.iter().map(|p| p.name.clone()).collect();
        assert!(names.contains("auth"));
        assert!(names.contains("indexed-lib"));
    }

    #[test]
    fn test_dedup_projects() {
        let dir = tempfile::tempdir().unwrap();

        // Set up a directory that would be found by both Cargo workspace and nested scan
        let sub = dir.path().join("sub-project");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("Cargo.toml"), "[package]\nname = \"sub\"").unwrap();

        let canonical = sub.canonicalize().unwrap();

        let mut projects = vec![
            DiscoveredProject {
                root: canonical.clone(),
                name: "sub-project".to_string(),
                project_type: ProjectType::CargoWorkspace,
                weight: 1.0,
            },
            DiscoveredProject {
                root: canonical,
                name: "sub-project".to_string(),
                project_type: ProjectType::MonorepoPackage,
                weight: 1.0,
            },
        ];

        dedup_projects(&mut projects);
        assert_eq!(projects.len(), 1);
        // First occurrence (CargoWorkspace) should be kept
        assert_eq!(projects[0].project_type, ProjectType::CargoWorkspace);
    }

    #[test]
    fn test_discover_projects_combined() {
        let dir = tempfile::tempdir().unwrap();
        setup_cargo_workspace(dir.path());
        setup_git_submodules(dir.path());

        let projects = discover_projects(dir.path());

        // Should find 2 cargo members + 1 submodule = 3 projects
        assert!(projects.len() >= 3);
        let types: HashSet<ProjectType> = projects.iter().map(|p| p.project_type).collect();
        assert!(types.contains(&ProjectType::CargoWorkspace));
        assert!(types.contains(&ProjectType::GitSubmodule));
    }

    #[test]
    fn test_to_federation_config() {
        let projects = vec![
            DiscoveredProject {
                root: PathBuf::from("/a"),
                name: "a".to_string(),
                project_type: ProjectType::CargoWorkspace,
                weight: 1.0,
            },
            DiscoveredProject {
                root: PathBuf::from("/b"),
                name: "b".to_string(),
                project_type: ProjectType::GitSubmodule,
                weight: 0.8,
            },
        ];

        let config = to_federation_config(&projects);
        assert_eq!(config.projects.len(), 2);
        assert_eq!(config.projects[0].root, PathBuf::from("/a"));
        assert!((config.projects[0].weight - 1.0).abs() < f32::EPSILON);
        assert!((config.projects[1].weight - 0.8).abs() < f32::EPSILON);
        assert!((config.rrf_k - 60.0).abs() < f32::EPSILON);
        assert!(config.lazy_load);
    }

    #[test]
    fn test_discover_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let projects = discover_projects(dir.path());
        assert!(projects.is_empty());
    }

    #[test]
    fn test_max_depth_limiting() {
        let dir = tempfile::tempdir().unwrap();

        // Create a project deeply nested (beyond MAX_SCAN_DEPTH)
        let deep = dir.path().join("a/b/c/d/deep-project");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("Cargo.toml"), "[package]\nname = \"deep\"").unwrap();

        let mut projects = Vec::new();
        discover_nested_projects(dir.path(), &mut projects);

        // Should NOT find the deeply nested project
        assert!(
            !projects.iter().any(|p| p.name == "deep-project"),
            "should not discover projects beyond MAX_SCAN_DEPTH"
        );
    }

    #[test]
    fn test_project_type_display() {
        assert_eq!(
            format!("{}", ProjectType::CargoWorkspace),
            "cargo-workspace"
        );
        assert_eq!(format!("{}", ProjectType::NpmWorkspace), "npm-workspace");
        assert_eq!(format!("{}", ProjectType::PnpmWorkspace), "pnpm-workspace");
        assert_eq!(format!("{}", ProjectType::GoWorkspace), "go-workspace");
        assert_eq!(
            format!("{}", ProjectType::MonorepoPackage),
            "monorepo-package"
        );
        assert_eq!(format!("{}", ProjectType::GitSubmodule), "git-submodule");
        assert_eq!(format!("{}", ProjectType::NestedIndexed), "nested-indexed");
    }
}
