//! Orphan file detection (Phase 14).
//!
//! Identifies files that have zero in-degree in the dependency graph — no other
//! tracked file imports them. These are potential dead code candidates.

/// Confidence level for orphan classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanConfidence {
    /// No references at all — almost certainly dead code.
    Certain,
    /// Only dynamic/string references found (e.g. in configs).
    High,
    /// Entry point or test file — expected to have zero in-degree.
    Moderate,
    /// Inconclusive — might be referenced by external code.
    Low,
}

impl OrphanConfidence {
    /// Return a short lowercase label for this confidence level.
    pub fn as_str(&self) -> &str {
        match self {
            OrphanConfidence::Certain => "certain",
            OrphanConfidence::High => "high",
            OrphanConfidence::Moderate => "moderate",
            OrphanConfidence::Low => "low",
        }
    }
}

impl std::fmt::Display for OrphanConfidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An orphan file result.
#[derive(Debug, Clone)]
pub struct OrphanFile {
    pub file_path: String,
    pub confidence: OrphanConfidence,
    pub reason: String,
    /// Number of symbols defined in this file (more symbols = more waste if truly orphaned).
    pub symbol_count: usize,
    /// Lines of code.
    pub lines: usize,
}

/// Options for orphan detection.
#[derive(Debug, Clone)]
pub struct OrphanOptions {
    /// File glob patterns to include (e.g. "*.rs", "*.py").
    pub include_patterns: Vec<String>,
    /// File patterns to exclude from orphan detection.
    pub exclude_patterns: Vec<String>,
    /// Whether to check for dynamic references via text search.
    pub check_dynamic_refs: bool,
    /// Maximum results to return.
    pub limit: usize,
}

impl Default for OrphanOptions {
    fn default() -> Self {
        Self {
            include_patterns: Vec::new(),
            exclude_patterns: vec![
                "test".to_string(),
                "spec".to_string(),
                "bench".to_string(),
                "__pycache__".to_string(),
                "node_modules".to_string(),
            ],
            check_dynamic_refs: true,
            limit: 50,
        }
    }
}

/// Check if a file path looks like an entry point.
pub fn is_entry_point(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let entry_names = [
        "main.rs",
        "lib.rs",
        "mod.rs",
        "index.js",
        "index.ts",
        "index.tsx",
        "app.rs",
        "app.py",
        "app.js",
        "app.ts",
        "__init__.py",
        "__main__.py",
        "setup.py",
        "setup.cfg",
        "pyproject.toml",
        "Cargo.toml",
        "package.json",
        "manage.py",
        "wsgi.py",
        "asgi.py",
    ];
    entry_names.contains(&filename)
        || filename.starts_with("main")
        || path.contains("/bin/")
        || path.starts_with("bin/")
        || path.contains("/scripts/")
        || path.starts_with("scripts/")
}

/// Check if a file path looks like a test file.
pub fn is_test_file(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    filename.starts_with("test_")
        || filename.ends_with("_test.rs")
        || filename.ends_with("_test.go")
        || filename.ends_with(".test.js")
        || filename.ends_with(".test.ts")
        || filename.ends_with(".spec.js")
        || filename.ends_with(".spec.ts")
        || path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("/__tests__/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_entry_point_detects_main_rs() {
        assert!(is_entry_point("src/main.rs"));
    }

    #[test]
    fn is_entry_point_detects_lib_rs() {
        assert!(is_entry_point("crates/core/src/lib.rs"));
    }

    #[test]
    fn is_entry_point_rejects_random_rs() {
        assert!(!is_entry_point("src/random.rs"));
    }

    #[test]
    fn is_entry_point_detects_bin_dir() {
        assert!(is_entry_point("src/bin/helper.rs"));
    }

    #[test]
    fn is_entry_point_detects_scripts_dir() {
        assert!(is_entry_point("scripts/deploy.py"));
    }

    #[test]
    fn is_test_file_detects_test_prefix() {
        assert!(is_test_file("tests/test_foo.py"));
    }

    #[test]
    fn is_test_file_detects_rust_test_suffix() {
        assert!(is_test_file("src/foo_test.rs"));
    }

    #[test]
    fn is_test_file_detects_go_test_suffix() {
        assert!(is_test_file("pkg/server_test.go"));
    }

    #[test]
    fn is_test_file_detects_js_test_suffix() {
        assert!(is_test_file("src/app.test.js"));
    }

    #[test]
    fn is_test_file_detects_tests_dir() {
        assert!(is_test_file("src/tests/helpers.rs"));
    }

    #[test]
    fn is_test_file_rejects_regular() {
        assert!(!is_test_file("src/regular.rs"));
    }

    #[test]
    fn orphan_options_default_has_expected_excludes() {
        let opts = OrphanOptions::default();
        assert!(opts.exclude_patterns.contains(&"test".to_string()));
        assert!(opts.exclude_patterns.contains(&"spec".to_string()));
        assert!(opts.exclude_patterns.contains(&"bench".to_string()));
        assert!(opts.exclude_patterns.contains(&"__pycache__".to_string()));
        assert!(opts.exclude_patterns.contains(&"node_modules".to_string()));
        assert!(opts.check_dynamic_refs);
        assert_eq!(opts.limit, 50);
    }

    #[test]
    fn orphan_confidence_as_str() {
        assert_eq!(OrphanConfidence::Certain.as_str(), "certain");
        assert_eq!(OrphanConfidence::High.as_str(), "high");
        assert_eq!(OrphanConfidence::Moderate.as_str(), "moderate");
        assert_eq!(OrphanConfidence::Low.as_str(), "low");
    }

    #[test]
    fn orphan_confidence_display() {
        assert_eq!(format!("{}", OrphanConfidence::Certain), "certain");
        assert_eq!(format!("{}", OrphanConfidence::High), "high");
        assert_eq!(format!("{}", OrphanConfidence::Moderate), "moderate");
        assert_eq!(format!("{}", OrphanConfidence::Low), "low");
    }
}
