use std::path::{Component, Path, PathBuf};

use codixing_core::{Engine, IndexConfig};

use crate::error::ApiError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoPath {
    pub absolute: PathBuf,
    pub relative: String,
}

fn outside_root_error(requested: &str) -> ApiError {
    ApiError::BadRequest(format!(
        "file path '{requested}' must stay within the indexed project roots"
    ))
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn resolve_relative_candidate(config: &IndexConfig, requested: &str) -> PathBuf {
    let requested = requested.replace('\\', "/");

    if let Some(existing) = config.resolve_path(&requested) {
        return existing;
    }

    let first_component =
        Path::new(&requested)
            .components()
            .find_map(|component| match component {
                Component::Normal(part) => Some(part.to_os_string()),
                _ => None,
            });

    for extra_root in &config.extra_roots {
        let prefix = extra_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| extra_root.to_string_lossy().into_owned());
        let prefix_with_slash = format!("{prefix}/");
        let extra_candidate = if requested == prefix {
            Some(extra_root.clone())
        } else {
            requested
                .strip_prefix(&prefix_with_slash)
                .map(|stripped| extra_root.join(stripped))
        };

        if let Some(extra_candidate) = extra_candidate {
            let primary_claims_prefix = first_component
                .as_ref()
                .map(|component| config.root.join(component))
                .is_some_and(|path| path.exists());
            if !primary_claims_prefix {
                return extra_candidate;
            }
        }
    }

    config.root.join(&requested)
}

fn normalize_against_known_roots(config: &IndexConfig, abs_path: &Path) -> Option<String> {
    if let Some(relative) = config.normalize_path(abs_path) {
        return Some(relative);
    }

    let canonical_primary = config.root.canonicalize().ok();
    if let Some(primary_root) = canonical_primary {
        if let Ok(rel) = abs_path.strip_prefix(&primary_root) {
            return Some(rel.to_string_lossy().replace('\\', "/"));
        }
    }

    for extra_root in &config.extra_roots {
        let prefix = extra_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| extra_root.to_string_lossy().into_owned());
        let canonical_extra = extra_root.canonicalize().ok();
        if let Some(extra_root) = canonical_extra {
            if let Ok(rel) = abs_path.strip_prefix(&extra_root) {
                return Some(format!(
                    "{prefix}/{}",
                    rel.to_string_lossy().replace('\\', "/")
                ));
            }
        }
    }

    None
}

pub(crate) fn resolve_repo_path(engine: &Engine, requested: &str) -> Result<RepoPath, ApiError> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err(ApiError::BadRequest(
            "file path must not be empty".to_string(),
        ));
    }

    let config = engine.config();
    let requested_path = Path::new(requested);
    let candidate = if requested_path.is_absolute() {
        normalize_lexical(requested_path)
    } else {
        normalize_lexical(&resolve_relative_candidate(config, requested))
    };

    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .map_err(|e| ApiError::BadRequest(format!("cannot resolve '{requested}': {e}")))?;
        if let Some(relative) = normalize_against_known_roots(config, &canonical) {
            return Ok(RepoPath {
                absolute: canonical,
                relative,
            });
        }
        return Err(outside_root_error(requested));
    }

    let Some(relative) = config.normalize_path(&candidate) else {
        return Err(outside_root_error(requested));
    };

    Ok(RepoPath {
        absolute: candidate,
        relative,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use codixing_core::{EmbeddingConfig, IndexConfig};
    use tempfile::tempdir;

    use super::*;

    fn make_engine(root: &Path, extra_root: Option<&Path>) -> Engine {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();

        let mut config = IndexConfig::new(root);
        config.embedding = EmbeddingConfig {
            enabled: false,
            ..EmbeddingConfig::default()
        };

        if let Some(extra_root) = extra_root {
            fs::create_dir_all(extra_root.join("src")).unwrap();
            fs::write(extra_root.join("src/shared.rs"), "pub fn shared() {}\n").unwrap();
            config.extra_roots.push(extra_root.to_path_buf());
        }

        Engine::init(root, config).unwrap()
    }

    #[test]
    fn resolves_primary_root_paths() {
        let dir = tempdir().unwrap();
        let engine = make_engine(dir.path(), None);

        let resolved = resolve_repo_path(&engine, "src/main.rs").unwrap();
        assert_eq!(resolved.relative, "src/main.rs");
        assert!(resolved.absolute.ends_with("src/main.rs"));
    }

    #[test]
    fn resolves_extra_root_paths_even_when_passed_as_prefixed_relative_paths() {
        let root = tempdir().unwrap();
        let extra = tempdir().unwrap();
        let engine = make_engine(root.path(), Some(extra.path()));
        let prefix = extra.path().file_name().unwrap().to_string_lossy();

        let requested = format!("{prefix}/src/shared.rs");
        let resolved = resolve_repo_path(&engine, &requested).unwrap();

        assert_eq!(resolved.relative, requested);
        assert_eq!(
            resolved.absolute,
            extra.path().join("src/shared.rs").canonicalize().unwrap()
        );
    }

    #[test]
    fn rejects_absolute_paths_outside_known_roots() {
        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("outside.md"), "# outside\n").unwrap();
        let engine = make_engine(root.path(), None);

        let err = resolve_repo_path(&engine, outside.path().join("outside.md").to_str().unwrap())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("must stay within the indexed project roots")
        );
    }

    #[test]
    fn rejects_relative_traversal_outside_root() {
        let root = tempdir().unwrap();
        let engine = make_engine(root.path(), None);

        let err = resolve_repo_path(&engine, "../outside.rs").unwrap_err();
        assert!(
            err.to_string()
                .contains("must stay within the indexed project roots")
        );
    }

    #[test]
    fn keeps_primary_root_priority_when_extra_root_prefix_collides() {
        let workspace = tempdir().unwrap();
        let root = workspace.path().join("project");
        let extra = workspace.path().join("deps").join("shared");

        fs::create_dir_all(root.join("shared/src")).unwrap();
        fs::write(root.join("shared/src/main.rs"), "pub fn primary() {}\n").unwrap();

        let engine = make_engine(&root, Some(&extra));
        let resolved = resolve_repo_path(&engine, "shared/src/main.rs").unwrap();

        assert_eq!(resolved.relative, "shared/src/main.rs");
        assert_eq!(
            resolved.absolute,
            root.join("shared/src/main.rs").canonicalize().unwrap()
        );
    }

    #[test]
    fn resolves_missing_prefixed_paths_to_extra_root_when_primary_namespace_does_not_exist() {
        let workspace = tempdir().unwrap();
        let root = workspace.path().join("project");
        let extra = workspace.path().join("deps").join("shared");
        let engine = make_engine(&root, Some(&extra));

        let resolved = resolve_repo_path(&engine, "shared/src/new.rs").unwrap();

        assert_eq!(resolved.relative, "shared/src/new.rs");
        assert_eq!(resolved.absolute, extra.join("src/new.rs"));
    }
}
