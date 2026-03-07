//! Per-language import path → indexed file path resolution.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::language::Language;

use super::extractor::RawImport;

/// Resolves raw import strings to indexed file paths.
pub struct ImportResolver {
    indexed_files: HashSet<String>,
    #[allow(dead_code)]
    root: PathBuf,
}

impl ImportResolver {
    /// Create a new resolver with the set of all indexed file paths (relative, forward-slash).
    pub fn new(indexed_files: HashSet<String>, root: PathBuf) -> Self {
        Self {
            indexed_files,
            root,
        }
    }

    /// Attempt to resolve `raw` (extracted from `source_file`) to an indexed path.
    ///
    /// Returns `Some(path)` when the import maps to a known indexed file, `None` otherwise.
    pub fn resolve(&self, raw: &RawImport, source_file: &str) -> Option<String> {
        match raw.language {
            Language::Rust => self.resolve_rust(&raw.path, source_file),
            Language::Python => self.resolve_python(&raw.path, source_file),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                self.resolve_js_ts(&raw.path, source_file)
            }
            Language::Go => self.resolve_go(&raw.path),
            Language::Java => self.resolve_java(&raw.path),
            Language::C | Language::Cpp => {
                if raw.is_relative {
                    self.resolve_c_relative(&raw.path, source_file)
                } else {
                    None // Angle-bracket includes are external.
                }
            }
            Language::CSharp => self.resolve_csharp(&raw.path),
            Language::Ruby => self.resolve_ruby(&raw.path, source_file, raw.is_relative),
            Language::Swift => None, // Swift module imports are external frameworks.
            Language::Kotlin => self.resolve_kotlin(&raw.path),
            Language::Scala => self.resolve_scala(&raw.path),
            // Tier 3: Zig and PHP import resolution not yet supported.
            Language::Zig | Language::Php => None,
        }
    }

    // -------------------------------------------------------------------------
    // Rust
    // -------------------------------------------------------------------------

    fn resolve_rust(&self, import: &str, source_file: &str) -> Option<String> {
        // Strip leading `crate::` or `super::` to get a module path.
        let module_path = import
            .strip_prefix("crate::")
            .or_else(|| import.strip_prefix("super::"))
            .unwrap_or(import);

        // Convert `parser::Parser` segments.
        let parts: Vec<&str> = module_path.split("::").collect();

        // Build prefix list: static candidates + the actual crate src root derived
        // from the source file path.  This handles Cargo workspaces where files live
        // at `crates/*/src/` rather than the top-level `src/`.
        //
        // e.g. source "crates/core/src/engine.rs" → crate_root "crates/core/src"
        // so we also try "crates/core/src/graph/extractor.rs" for `crate::graph::extractor`.
        let mut prefixes: Vec<String> = vec!["src".to_string(), "lib".to_string()];
        if let Some(root) = crate_src_root(source_file) {
            if root != "src" && root != "lib" {
                prefixes.push(root);
            }
        }
        prefixes.push(String::new()); // bare path (no prefix) last

        // Try each prefix length (shortest first) — `crate::parser::Parser` should match
        // `src/parser.rs` before trying `src/parser/Parser.rs`.
        for len in 1..=parts.len() {
            let seg = parts[..len].join("/");
            for prefix in &prefixes {
                let base = if prefix.is_empty() {
                    seg.clone()
                } else {
                    format!("{prefix}/{seg}")
                };

                // Try as a direct file.
                let as_file = format!("{base}.rs");
                if self.indexed_files.contains(&as_file) {
                    return Some(as_file);
                }

                // Try as a module directory.
                let as_mod = format!("{base}/mod.rs");
                if self.indexed_files.contains(&as_mod) {
                    return Some(as_mod);
                }
            }
        }

        None
    }

    // -------------------------------------------------------------------------
    // Python
    // -------------------------------------------------------------------------

    fn resolve_python(&self, import: &str, source_file: &str) -> Option<String> {
        let source_dir = parent_dir(source_file);

        if import.starts_with('.') {
            // Relative import: count leading dots.
            let dots = import.chars().take_while(|&c| c == '.').count();
            let rest = import[dots..].trim_start_matches('.');

            // Navigate up by (dots - 1) directories from source_dir.
            let base_dir = go_up(&source_dir, dots.saturating_sub(1));

            let candidate = if rest.is_empty() {
                // `from . import foo` — module name is in `imported_names`, not path.
                // We can't fully resolve without seeing the name; just try __init__.
                format!("{base_dir}/__init__.py")
            } else {
                let module_path = rest.replace('.', "/");
                format!("{base_dir}/{module_path}.py")
            };

            let norm = normalize_path(&candidate);
            if self.indexed_files.contains(&norm) {
                return Some(norm);
            }
            // Try as package.
            let pkg = normalize_path(&format!(
                "{}/{rest}/__init__.py",
                go_up(&source_dir, dots.saturating_sub(1))
            ));
            if self.indexed_files.contains(&pkg) {
                return Some(pkg);
            }

            return None;
        }

        // Absolute import: `import foo.bar` → `foo/bar.py`
        let module_path = import.replace('.', "/");
        let as_file = format!("{module_path}.py");
        if self.indexed_files.contains(&as_file) {
            return Some(as_file);
        }
        let as_pkg = format!("{module_path}/__init__.py");
        if self.indexed_files.contains(&as_pkg) {
            return Some(as_pkg);
        }

        None
    }

    // -------------------------------------------------------------------------
    // TypeScript / JavaScript
    // -------------------------------------------------------------------------

    fn resolve_js_ts(&self, import: &str, source_file: &str) -> Option<String> {
        let source_dir = parent_dir(source_file);

        // Absolute (non-relative) imports are external packages.
        if !import.starts_with("./") && !import.starts_with("../") {
            return None;
        }

        let joined = join_paths(&source_dir, import);
        let norm = normalize_path(&joined);

        // Try as-is.
        if self.indexed_files.contains(&norm) {
            return Some(norm);
        }

        // Try with common extensions.
        for ext in &["ts", "tsx", "js", "jsx", "mts", "cts"] {
            let candidate = format!("{norm}.{ext}");
            if self.indexed_files.contains(&candidate) {
                return Some(candidate);
            }
        }

        // Try as directory index.
        for ext in &["ts", "tsx", "js", "jsx"] {
            let candidate = format!("{norm}/index.{ext}");
            if self.indexed_files.contains(&candidate) {
                return Some(candidate);
            }
        }

        None
    }

    // -------------------------------------------------------------------------
    // Go
    // -------------------------------------------------------------------------

    fn resolve_go(&self, import: &str) -> Option<String> {
        // Go import paths are package paths like "github.com/user/pkg/sub".
        // Match against any `.go` file whose directory ends with the import path suffix.
        for file in &self.indexed_files {
            if !file.ends_with(".go") {
                continue;
            }
            let dir = parent_dir(file);
            if dir.ends_with(import) || dir == import {
                return Some(file.clone());
            }
        }
        None
    }

    // -------------------------------------------------------------------------
    // Java
    // -------------------------------------------------------------------------

    fn resolve_java(&self, import: &str) -> Option<String> {
        // Strip wildcard: `com.example.*` → `com/example`
        let stripped = import.trim_end_matches(".*");
        let path = stripped.replace('.', "/");
        let as_file = format!("{path}.java");
        if self.indexed_files.contains(&as_file) {
            return Some(as_file);
        }
        // Also try under `src/`.
        let as_src = format!("src/{as_file}");
        if self.indexed_files.contains(&as_src) {
            return Some(as_src);
        }
        None
    }

    // -------------------------------------------------------------------------
    // C / C++ (relative only — angle-bracket handled in resolve())
    // -------------------------------------------------------------------------

    fn resolve_c_relative(&self, include: &str, source_file: &str) -> Option<String> {
        let source_dir = parent_dir(source_file);
        let candidate = normalize_path(&join_paths(&source_dir, include));
        if self.indexed_files.contains(&candidate) {
            return Some(candidate);
        }
        None
    }

    // -------------------------------------------------------------------------
    // C#
    // -------------------------------------------------------------------------

    fn resolve_csharp(&self, import: &str) -> Option<String> {
        let path = import.replace('.', "/");
        let as_file = format!("{path}.cs");
        if self.indexed_files.contains(&as_file) {
            return Some(as_file);
        }
        let as_src = format!("src/{as_file}");
        if self.indexed_files.contains(&as_src) {
            return Some(as_src);
        }
        None
    }

    // -------------------------------------------------------------------------
    // Ruby
    // -------------------------------------------------------------------------

    fn resolve_ruby(&self, import: &str, source_file: &str, is_relative: bool) -> Option<String> {
        if is_relative {
            // `require_relative './lib/foo'` → resolve relative to source file dir.
            let source_dir = parent_dir(source_file);
            let joined = join_paths(&source_dir, import);
            let norm = normalize_path(&joined);
            // Try as-is, then with `.rb` extension.
            if self.indexed_files.contains(&norm) {
                return Some(norm.clone());
            }
            let with_ext = format!("{norm}.rb");
            if self.indexed_files.contains(&with_ext) {
                return Some(with_ext);
            }
        } else {
            // Absolute require: `require 'lib/foo'` → try `lib/foo.rb`.
            let as_file = if import.ends_with(".rb") {
                import.to_string()
            } else {
                format!("{import}.rb")
            };
            if self.indexed_files.contains(&as_file) {
                return Some(as_file.clone());
            }
            // Also try under `lib/`.
            let as_lib = format!("lib/{as_file}");
            if self.indexed_files.contains(&as_lib) {
                return Some(as_lib);
            }
        }
        None
    }

    // -------------------------------------------------------------------------
    // Kotlin
    // -------------------------------------------------------------------------

    fn resolve_kotlin(&self, import: &str) -> Option<String> {
        // Strip wildcard: `com.example.*` → `com/example`
        let stripped = import.trim_end_matches(".*");
        let path = stripped.replace('.', "/");
        let as_file = format!("{path}.kt");
        if self.indexed_files.contains(&as_file) {
            return Some(as_file.clone());
        }
        let as_src = format!("src/{as_file}");
        if self.indexed_files.contains(&as_src) {
            return Some(as_src);
        }
        let as_main = format!("src/main/kotlin/{as_file}");
        if self.indexed_files.contains(&as_main) {
            return Some(as_main);
        }
        None
    }

    // -------------------------------------------------------------------------
    // Scala
    // -------------------------------------------------------------------------

    fn resolve_scala(&self, import: &str) -> Option<String> {
        // Strip wildcard: `com.example._` → `com/example`
        let stripped = import.trim_end_matches("._");
        let path = stripped.replace('.', "/");
        let as_file = format!("{path}.scala");
        if self.indexed_files.contains(&as_file) {
            return Some(as_file.clone());
        }
        let as_src = format!("src/{as_file}");
        if self.indexed_files.contains(&as_src) {
            return Some(as_src);
        }
        let as_main = format!("src/main/scala/{as_file}");
        if self.indexed_files.contains(&as_main) {
            return Some(as_main);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Derive the Rust crate source root from a source file path.
///
/// For `crates/core/src/engine.rs` → `"crates/core/src"`.
/// For `src/main.rs`              → `"src"`.
/// Returns `None` if no `src/` component is found.
fn crate_src_root(source_file: &str) -> Option<String> {
    // Look for the last `/src/` segment (handles nested paths safely).
    if let Some(idx) = source_file.rfind("/src/") {
        Some(source_file[..idx + 4].to_string()) // up to and including "/src" (no trailing /)
    } else if source_file.starts_with("src/") {
        Some("src".to_string())
    } else {
        None
    }
}

/// Return the directory component of a relative file path (always uses `/`).
fn parent_dir(file: &str) -> String {
    match file.rfind('/') {
        Some(idx) => file[..idx].to_string(),
        None => String::new(),
    }
}

/// Join a directory and a (possibly `../`-relative) path using simple string ops.
fn join_paths(dir: &str, rel: &str) -> String {
    if rel.starts_with('/') {
        return rel.to_string();
    }
    let base = if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    };
    normalize_path(&base)
}

/// Normalize a path: collapse `.` and `..` segments.
fn normalize_path(path: &str) -> String {
    // Use Path for resolution then convert back.
    let p = Path::new(path);
    let mut components: Vec<&str> = Vec::new();
    for seg in p.components() {
        use std::path::Component;
        match seg {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            Component::Normal(s) => {
                if let Some(s) = s.to_str() {
                    components.push(s);
                }
            }
            _ => {}
        }
    }
    components.join("/")
}

/// Walk up `n` directory levels from `dir`.
fn go_up(dir: &str, n: usize) -> String {
    let mut parts: Vec<&str> = dir.split('/').collect();
    for _ in 0..n {
        if !parts.is_empty() {
            parts.pop();
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::extractor::RawImport;

    fn make_resolver(files: &[&str]) -> ImportResolver {
        ImportResolver::new(
            files.iter().map(|s| s.to_string()).collect(),
            PathBuf::from("/project"),
        )
    }

    #[test]
    fn rust_crate_import_resolves_to_src_file() {
        let resolver = make_resolver(&["src/parser.rs", "src/engine.rs"]);
        let raw = RawImport {
            path: "crate::parser::Parser".to_string(),
            language: Language::Rust,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/main.rs");
        assert_eq!(resolved, Some("src/parser.rs".to_string()));
    }

    #[test]
    fn rust_crate_import_resolves_to_mod_rs() {
        let resolver = make_resolver(&["src/parser/mod.rs"]);
        let raw = RawImport {
            path: "crate::parser".to_string(),
            language: Language::Rust,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/main.rs");
        assert_eq!(resolved, Some("src/parser/mod.rs".to_string()));
    }

    #[test]
    fn typescript_relative_import_resolves() {
        let resolver = make_resolver(&["src/foo.ts", "src/bar.ts"]);
        let raw = RawImport {
            path: "./foo".to_string(),
            language: Language::TypeScript,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/index.ts");
        assert_eq!(resolved, Some("src/foo.ts".to_string()));
    }

    #[test]
    fn typescript_external_import_returns_none() {
        let resolver = make_resolver(&["src/foo.ts"]);
        let raw = RawImport {
            path: "react".to_string(),
            language: Language::TypeScript,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "src/index.ts"), None);
    }

    #[test]
    fn external_rust_std_returns_none() {
        let resolver = make_resolver(&["src/main.rs"]);
        let raw = RawImport {
            path: "std::collections::HashMap".to_string(),
            language: Language::Rust,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "src/main.rs"), None);
    }

    #[test]
    fn rust_workspace_import_resolves() {
        // Simulates a Cargo workspace where files live under crates/*/src/
        let resolver = make_resolver(&[
            "crates/core/src/graph/extractor.rs",
            "crates/core/src/engine.rs",
        ]);
        let raw = RawImport {
            path: "crate::graph::extractor".to_string(),
            language: Language::Rust,
            is_relative: true,
        };
        // Source file is also under the workspace; resolver should derive "crates/core/src" root.
        let resolved = resolver.resolve(&raw, "crates/core/src/engine.rs");
        assert_eq!(
            resolved,
            Some("crates/core/src/graph/extractor.rs".to_string())
        );
    }

    #[test]
    fn crate_src_root_derived_correctly() {
        assert_eq!(
            crate_src_root("crates/core/src/engine.rs"),
            Some("crates/core/src".to_string())
        );
        assert_eq!(crate_src_root("src/main.rs"), Some("src".to_string()));
        assert_eq!(crate_src_root("README.md"), None);
    }

    #[test]
    fn python_relative_dot_import() {
        let resolver = make_resolver(&["src/helpers.py", "src/utils.py"]);
        let raw = RawImport {
            path: ".".to_string(),
            language: Language::Python,
            is_relative: true,
        };
        // `from . import helpers` — we can't resolve without the name; expect None or __init__
        let resolved = resolver.resolve(&raw, "src/utils.py");
        // No __init__.py in our set, so None is fine.
        assert!(resolved.is_none() || resolved.as_deref() == Some("src/__init__.py"));
    }
}
