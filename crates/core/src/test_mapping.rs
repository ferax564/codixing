//! Test-to-code mapping (Phase 15).
//!
//! Automatically links test files to the source files they test, enabling
//! bidirectional queries: "find tests for this code" and "find code tested
//! by this test".

use std::collections::HashMap;
use std::path::Path;

/// A single mapping between a test file and its corresponding source file.
#[derive(Debug, Clone)]
pub struct TestMapping {
    /// Relative path of the test file.
    pub test_file: String,
    /// Relative path of the source file being tested.
    pub source_file: String,
    /// Confidence score (0.0 to 1.0) for this mapping.
    pub confidence: f32,
    /// Human-readable reason for the mapping.
    pub reason: String,
}

/// Options for test mapping discovery.
#[derive(Debug, Clone)]
pub struct TestMappingOptions {
    /// Maximum number of mappings to return.
    pub limit: usize,
    /// Minimum confidence threshold (0.0 to 1.0).
    pub min_confidence: f32,
}

impl Default for TestMappingOptions {
    fn default() -> Self {
        Self {
            limit: 200,
            min_confidence: 0.0,
        }
    }
}

/// Check if a filename looks like a test file based on naming conventions.
pub fn is_test_file(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let filename_lower = filename.to_lowercase();

    // Python: test_foo.py
    filename_lower.starts_with("test_")
        // Rust: foo_test.rs
        || filename_lower.ends_with("_test.rs")
        // Go: foo_test.go
        || filename_lower.ends_with("_test.go")
        // JS/TS: foo.test.js, foo.test.ts, foo.test.tsx, foo.test.jsx
        || filename_lower.ends_with(".test.js")
        || filename_lower.ends_with(".test.ts")
        || filename_lower.ends_with(".test.tsx")
        || filename_lower.ends_with(".test.jsx")
        // JS/TS: foo.spec.js, foo.spec.ts, foo.spec.tsx, foo.spec.jsx
        || filename_lower.ends_with(".spec.js")
        || filename_lower.ends_with(".spec.ts")
        || filename_lower.ends_with(".spec.tsx")
        || filename_lower.ends_with(".spec.jsx")
        // Directory conventions
        || path.contains("/tests/")
        || path.contains("/test/")
        || path.contains("/__tests__/")
        || path.starts_with("tests/")
        || path.starts_with("test/")
        || path.starts_with("__tests__/")
}

/// Strip test-related affixes from a filename, returning the base source name.
///
/// Examples:
/// - `test_foo.py` -> `foo.py`
/// - `foo_test.go` -> `foo.go`
/// - `foo.test.ts` -> `foo.ts`
/// - `foo.spec.js` -> `foo.js`
/// - `TestFoo.java` -> `Foo.java`
pub fn strip_test_affixes(filename: &str) -> Option<String> {
    let lower = filename.to_lowercase();

    // Python: test_foo.py -> foo.py
    if lower.starts_with("test_") {
        let ext = Path::new(filename).extension()?.to_str()?;
        let stem = &filename[5..filename.len() - ext.len() - 1];
        return Some(format!("{stem}.{ext}"));
    }

    // Rust: foo_test.rs -> foo.rs
    if lower.ends_with("_test.rs") {
        let stem = &filename[..filename.len() - 8]; // "_test.rs" = 8 chars
        return Some(format!("{stem}.rs"));
    }

    // Go: foo_test.go -> foo.go
    if lower.ends_with("_test.go") {
        let stem = &filename[..filename.len() - 8];
        return Some(format!("{stem}.go"));
    }

    // JS/TS: foo.test.js -> foo.js, foo.spec.ts -> foo.ts
    for pattern in &[
        ".test.js",
        ".test.ts",
        ".test.tsx",
        ".test.jsx",
        ".spec.js",
        ".spec.ts",
        ".spec.tsx",
        ".spec.jsx",
    ] {
        if lower.ends_with(pattern) {
            let ext = &pattern[pattern.rfind('.').unwrap()..]; // e.g. ".js"
            let stem = &filename[..filename.len() - pattern.len()];
            return Some(format!("{stem}{ext}"));
        }
    }

    None
}

/// Find the best matching source file for a test file, given a list of all files.
///
/// Returns `(source_file_path, confidence)` or `None` if no match is found.
pub fn find_best_source_match(test_file: &str, all_files: &[String]) -> Option<(String, f32)> {
    let test_filename = test_file.rsplit('/').next().unwrap_or(test_file);
    let stripped = strip_test_affixes(test_filename)?;

    // Strategy 1: Same directory — test file sits next to source file.
    let test_dir = test_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let same_dir_candidate = if test_dir.is_empty() {
        stripped.clone()
    } else {
        format!("{test_dir}/{stripped}")
    };
    if all_files.iter().any(|f| f == &same_dir_candidate) {
        return Some((same_dir_candidate, 0.9));
    }

    // Strategy 2: Directory convention mappings.
    // tests/test_foo.py -> src/foo.py, lib/foo.py, foo.py
    // __tests__/Foo.test.tsx -> src/Foo.tsx, Foo.tsx
    // test/foo_test.go -> foo.go, pkg/foo.go, internal/foo.go
    let directory_candidates = build_directory_candidates(test_file, &stripped);
    for candidate in &directory_candidates {
        if all_files.iter().any(|f| f == candidate) {
            return Some((candidate.clone(), 0.8));
        }
    }

    // Strategy 3: Fuzzy filename match — find any file with the same basename.
    let stripped_lower = stripped.to_lowercase();
    let mut best: Option<(String, f32)> = None;
    for file in all_files {
        let filename = file.rsplit('/').next().unwrap_or(file);
        if filename.to_lowercase() == stripped_lower && file != test_file {
            // Base confidence 0.7, boost slightly if paths share components.
            let shared = count_shared_path_components(test_file, file);
            let confidence = (0.7 + shared as f32 * 0.05).min(0.85);
            if best.as_ref().is_none_or(|(_, c)| confidence > *c) {
                best = Some((file.clone(), confidence));
            }
        }
    }
    best
}

/// Build directory-convention candidate paths for source file discovery.
fn build_directory_candidates(test_path: &str, stripped_name: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let parts: Vec<&str> = test_path.split('/').collect();

    if parts.len() < 2 {
        return candidates;
    }

    // Remove the test directory component and filename, try common source dirs.
    let test_dir_idx = parts.iter().position(|&p| {
        let lower = p.to_lowercase();
        lower == "tests" || lower == "test" || lower == "__tests__"
    });

    if let Some(idx) = test_dir_idx {
        // Reconstruct path prefix before the test dir.
        let prefix: String = if idx > 0 {
            parts[..idx].join("/")
        } else {
            String::new()
        };

        // Reconstruct path suffix after the test dir (subfolders within tests/).
        let suffix: String = if idx + 1 < parts.len() - 1 {
            parts[idx + 1..parts.len() - 1].join("/")
        } else {
            String::new()
        };

        let build_path = |dir: &str| -> String {
            let mut p = String::new();
            if !prefix.is_empty() {
                p.push_str(&prefix);
                p.push('/');
            }
            if !dir.is_empty() {
                p.push_str(dir);
                p.push('/');
            }
            if !suffix.is_empty() {
                p.push_str(&suffix);
                p.push('/');
            }
            p.push_str(stripped_name);
            p
        };

        // Try: src/, lib/, (no dir), pkg/, internal/, app/
        for dir in &["src", "lib", "", "pkg", "internal", "app"] {
            candidates.push(build_path(dir));
        }
    }

    candidates
}

/// Count shared path components between two paths (ignoring filename).
fn count_shared_path_components(a: &str, b: &str) -> usize {
    let a_parts: Vec<&str> = a.split('/').collect();
    let b_parts: Vec<&str> = b.split('/').collect();
    // Skip the last component (filename).
    let a_dirs = &a_parts[..a_parts.len().saturating_sub(1)];
    let b_dirs = &b_parts[..b_parts.len().saturating_sub(1)];
    a_dirs.iter().filter(|d| b_dirs.contains(d)).count()
}

/// Discover test-to-source mappings for a set of files.
///
/// Combines naming convention and directory convention strategies. When
/// `import_deps` is provided (mapping from file -> list of files it imports),
/// import-based analysis is also used for higher confidence.
pub fn discover_test_mappings(
    files: &[String],
    import_deps: Option<&HashMap<String, Vec<String>>>,
    options: &TestMappingOptions,
) -> Vec<TestMapping> {
    let mut mappings: Vec<TestMapping> = Vec::new();
    // Track best mapping per (test_file, source_file) pair.
    let mut best: HashMap<(String, String), TestMapping> = HashMap::new();

    for file in files {
        if !is_test_file(file) {
            continue;
        }

        // Strategy A: Import analysis (highest confidence when available).
        if let Some(deps) = import_deps {
            if let Some(imports) = deps.get(file.as_str()) {
                for imported in imports {
                    // Only map to files that are in our indexed set and not test files themselves.
                    if files.contains(imported) && !is_test_file(imported) {
                        let key = (file.clone(), imported.clone());
                        let mapping = TestMapping {
                            test_file: file.clone(),
                            source_file: imported.clone(),
                            confidence: 0.95,
                            reason: format!("Test file imports {imported}"),
                        };
                        best.entry(key)
                            .and_modify(|existing| {
                                if mapping.confidence > existing.confidence {
                                    *existing = mapping.clone();
                                }
                            })
                            .or_insert(mapping);
                    }
                }
            }
        }

        // Strategy B: Naming/directory convention.
        if let Some((source, confidence)) = find_best_source_match(file, files) {
            if !is_test_file(&source) {
                let key = (file.clone(), source.clone());
                let reason = if confidence >= 0.9 {
                    "Naming convention match (same directory)".to_string()
                } else if confidence >= 0.8 {
                    "Directory convention match".to_string()
                } else {
                    "Filename match (different directory)".to_string()
                };
                let mapping = TestMapping {
                    test_file: file.clone(),
                    source_file: source,
                    confidence,
                    reason,
                };
                best.entry(key)
                    .and_modify(|existing| {
                        if mapping.confidence > existing.confidence {
                            *existing = mapping.clone();
                        }
                    })
                    .or_insert(mapping);
            }
        }
    }

    mappings.extend(best.into_values());

    // Filter by min confidence and sort by confidence descending.
    mappings.retain(|m| m.confidence >= options.min_confidence);
    mappings.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.test_file.cmp(&b.test_file))
            .then_with(|| a.source_file.cmp(&b.source_file))
    });
    mappings.truncate(options.limit);
    mappings
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_test_file_python() {
        assert!(is_test_file("tests/test_foo.py"));
        assert!(is_test_file("test_bar.py"));
        assert!(!is_test_file("foo.py"));
    }

    #[test]
    fn is_test_file_rust() {
        assert!(is_test_file("src/foo_test.rs"));
        assert!(!is_test_file("src/foo.rs"));
    }

    #[test]
    fn is_test_file_go() {
        assert!(is_test_file("pkg/server_test.go"));
        assert!(!is_test_file("pkg/server.go"));
    }

    #[test]
    fn is_test_file_js_ts() {
        assert!(is_test_file("src/app.test.js"));
        assert!(is_test_file("src/utils.spec.ts"));
        assert!(is_test_file("src/Component.test.tsx"));
        assert!(is_test_file("__tests__/Foo.test.tsx"));
        assert!(!is_test_file("src/app.js"));
    }

    #[test]
    fn is_test_file_directory_conventions() {
        assert!(is_test_file("tests/helpers.rs"));
        assert!(is_test_file("test/something.py"));
        assert!(is_test_file("__tests__/Button.tsx"));
    }

    #[test]
    fn strip_test_affixes_python() {
        assert_eq!(strip_test_affixes("test_foo.py"), Some("foo.py".to_string()));
    }

    #[test]
    fn strip_test_affixes_rust() {
        assert_eq!(
            strip_test_affixes("parser_test.rs"),
            Some("parser.rs".to_string())
        );
    }

    #[test]
    fn strip_test_affixes_go() {
        assert_eq!(
            strip_test_affixes("server_test.go"),
            Some("server.go".to_string())
        );
    }

    #[test]
    fn strip_test_affixes_js_spec() {
        assert_eq!(
            strip_test_affixes("utils.spec.js"),
            Some("utils.js".to_string())
        );
    }

    #[test]
    fn strip_test_affixes_tsx_test() {
        assert_eq!(
            strip_test_affixes("Component.test.tsx"),
            Some("Component.tsx".to_string())
        );
    }

    #[test]
    fn strip_test_affixes_non_test() {
        assert_eq!(strip_test_affixes("regular.rs"), None);
        assert_eq!(strip_test_affixes("utils.py"), None);
    }

    #[test]
    fn find_best_source_match_same_dir() {
        let files = vec![
            "src/foo.rs".to_string(),
            "src/foo_test.rs".to_string(),
            "src/bar.rs".to_string(),
        ];
        let result = find_best_source_match("src/foo_test.rs", &files);
        assert!(result.is_some());
        let (path, conf) = result.unwrap();
        assert_eq!(path, "src/foo.rs");
        assert!((conf - 0.9).abs() < 0.01);
    }

    #[test]
    fn find_best_source_match_directory_convention() {
        let files = vec![
            "src/foo.py".to_string(),
            "tests/test_foo.py".to_string(),
            "src/bar.py".to_string(),
        ];
        let result = find_best_source_match("tests/test_foo.py", &files);
        assert!(result.is_some());
        let (path, conf) = result.unwrap();
        assert_eq!(path, "src/foo.py");
        assert!((conf - 0.8).abs() < 0.01);
    }

    #[test]
    fn find_best_source_match_fuzzy() {
        let files = vec![
            "lib/utils/parser.go".to_string(),
            "test/parser_test.go".to_string(),
        ];
        // No direct directory match, falls back to fuzzy filename match.
        let result = find_best_source_match("test/parser_test.go", &files);
        assert!(result.is_some());
        let (path, conf) = result.unwrap();
        assert_eq!(path, "lib/utils/parser.go");
        assert!(conf >= 0.7);
    }

    #[test]
    fn find_best_source_match_no_match() {
        let files = vec![
            "src/main.rs".to_string(),
            "tests/test_foo.py".to_string(),
        ];
        let result = find_best_source_match("tests/test_foo.py", &files);
        // There's no foo.py in the file list.
        assert!(result.is_none());
    }

    #[test]
    fn discover_mappings_naming_convention() {
        let files = vec![
            "src/engine.rs".to_string(),
            "src/engine_test.rs".to_string(),
            "src/parser.rs".to_string(),
        ];
        let options = TestMappingOptions::default();
        let mappings = discover_test_mappings(&files, None, &options);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].test_file, "src/engine_test.rs");
        assert_eq!(mappings[0].source_file, "src/engine.rs");
        assert!(mappings[0].confidence >= 0.9);
    }

    #[test]
    fn discover_mappings_import_analysis() {
        let files = vec![
            "src/engine.rs".to_string(),
            "tests/test_engine.py".to_string(),
            "src/parser.rs".to_string(),
        ];
        let mut imports: HashMap<String, Vec<String>> = HashMap::new();
        imports.insert(
            "tests/test_engine.py".to_string(),
            vec!["src/engine.rs".to_string(), "src/parser.rs".to_string()],
        );
        let options = TestMappingOptions::default();
        let mappings = discover_test_mappings(&files, Some(&imports), &options);
        // Should have import-based mappings (0.95) and naming-based mapping.
        assert!(mappings.len() >= 2);
        // The import-based mapping should be first (highest confidence).
        assert!(mappings[0].confidence >= 0.95);
    }

    #[test]
    fn discover_mappings_no_false_positives() {
        let files = vec![
            "src/engine.rs".to_string(),
            "src/parser.rs".to_string(),
            "src/main.rs".to_string(),
        ];
        let options = TestMappingOptions::default();
        let mappings = discover_test_mappings(&files, None, &options);
        // None of these are test files, so no mappings should be produced.
        assert!(mappings.is_empty());
    }

    #[test]
    fn discover_mappings_js_tests_dir() {
        let files = vec![
            "src/Button.tsx".to_string(),
            "__tests__/Button.test.tsx".to_string(),
            "src/App.tsx".to_string(),
        ];
        let options = TestMappingOptions::default();
        let mappings = discover_test_mappings(&files, None, &options);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].test_file, "__tests__/Button.test.tsx");
        assert_eq!(mappings[0].source_file, "src/Button.tsx");
    }

    #[test]
    fn discover_mappings_min_confidence_filter() {
        let files = vec![
            "lib/utils/parser.go".to_string(),
            "test/parser_test.go".to_string(),
        ];
        let options = TestMappingOptions {
            min_confidence: 0.85,
            ..Default::default()
        };
        let mappings = discover_test_mappings(&files, None, &options);
        // Fuzzy match has confidence ~0.7, should be filtered out.
        assert!(mappings.is_empty());
    }

    #[test]
    fn discover_mappings_import_wins_over_naming() {
        let files = vec![
            "src/foo.rs".to_string(),
            "src/bar.rs".to_string(),
            "src/foo_test.rs".to_string(),
        ];
        let mut imports: HashMap<String, Vec<String>> = HashMap::new();
        imports.insert(
            "src/foo_test.rs".to_string(),
            vec!["src/foo.rs".to_string()],
        );
        let options = TestMappingOptions::default();
        let mappings = discover_test_mappings(&files, Some(&imports), &options);
        // Should have exactly one mapping for foo_test -> foo, with import confidence.
        let foo_mappings: Vec<_> = mappings
            .iter()
            .filter(|m| m.test_file == "src/foo_test.rs" && m.source_file == "src/foo.rs")
            .collect();
        assert_eq!(foo_mappings.len(), 1);
        // Import analysis (0.95) should win over naming convention (0.9).
        assert!((foo_mappings[0].confidence - 0.95).abs() < 0.01);
    }
}
