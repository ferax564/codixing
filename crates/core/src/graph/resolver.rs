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
            Language::Swift => self.resolve_swift(&raw.path),
            Language::Kotlin => self.resolve_kotlin(&raw.path),
            Language::Scala => self.resolve_scala(&raw.path),
            Language::Zig => self.resolve_zig(&raw.path, source_file),
            Language::Php => self.resolve_php(&raw.path, source_file, raw.is_relative),
            Language::Bash => self.resolve_bash(&raw.path, source_file),
            Language::Matlab => self.resolve_matlab(&raw.path, source_file),
            // Config, doc, and assembly have no import resolution.
            Language::Assembly
            | Language::Yaml
            | Language::Toml
            | Language::Dockerfile
            | Language::Makefile
            | Language::Mermaid
            | Language::Xml
            | Language::Markdown
            | Language::Html
            | Language::Rst
            | Language::AsciiDoc
            | Language::PlainText
            | Language::OpenApi
            | Language::Jupyter
            | Language::Pdf => None,
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

        // Absolute package imports are normally external, but monorepos often
        // expose local packages through path aliases such as
        // `openclaw/plugin-sdk/plugin-entry` -> `src/plugin-sdk/plugin-entry.ts`.
        if !import.starts_with("./") && !import.starts_with("../") {
            return self.resolve_js_ts_alias(import);
        }

        let joined = join_paths(&source_dir, import);
        let norm = normalize_path(&joined);

        // TypeScript .js→.ts extension swap (moduleResolution: "node16"/"bundler").
        let js_to_ts_swaps: &[(&str, &[&str])] = &[
            (".js", &[".ts", ".tsx"]),
            (".jsx", &[".tsx"]),
            (".mjs", &[".mts"]),
            (".cjs", &[".cts"]),
        ];
        for &(js_ext, ts_exts) in js_to_ts_swaps {
            if let Some(stem) = norm.strip_suffix(js_ext) {
                for ts_ext in ts_exts {
                    let candidate = format!("{stem}{ts_ext}");
                    if self.indexed_files.contains(&candidate) {
                        return Some(candidate);
                    }
                }
                break;
            }
        }

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

    fn resolve_js_ts_alias(&self, import: &str) -> Option<String> {
        let parts: Vec<&str> = import.split('/').filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            return None;
        }

        let mut candidates = Vec::new();
        let mut src_suffix_directory_candidates = Vec::new();
        candidates.push(import.to_string());
        candidates.push(format!("src/{import}"));

        // Try package-suffix aliases, e.g. `@scope/pkg/foo` -> `src/pkg/foo`.
        for start in 1..parts.len() {
            let suffix = parts[start..].join("/");
            candidates.push(suffix.clone());
            if suffix.contains('/') {
                candidates.push(format!("src/{suffix}"));
            } else {
                src_suffix_directory_candidates.push(format!("src/{suffix}"));
            }
        }

        for base in candidates {
            if let Some(path) = self.resolve_js_ts_candidate(&base) {
                return Some(path);
            }
        }

        for base in src_suffix_directory_candidates {
            if let Some(path) = self.resolve_js_ts_directory_index_candidate(&base) {
                return Some(path);
            }
        }

        None
    }

    fn resolve_js_ts_directory_index_candidate(&self, base: &str) -> Option<String> {
        for ext in &["ts", "tsx", "js", "jsx"] {
            let candidate = format!("{base}/index.{ext}");
            if self.indexed_files.contains(&candidate) {
                return Some(candidate);
            }
        }

        None
    }

    fn resolve_js_ts_candidate(&self, base: &str) -> Option<String> {
        if self.indexed_files.contains(base) {
            return Some(base.to_string());
        }

        for ext in &["ts", "tsx", "js", "jsx", "mts", "cts"] {
            let candidate = format!("{base}.{ext}");
            if self.indexed_files.contains(&candidate) {
                return Some(candidate);
            }
        }

        for ext in &["ts", "tsx", "js", "jsx"] {
            let candidate = format!("{base}/index.{ext}");
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

    // -------------------------------------------------------------------------
    // Swift
    // -------------------------------------------------------------------------

    fn resolve_swift(&self, import: &str) -> Option<String> {
        // Standard frameworks (Foundation, UIKit, SwiftUI, etc.) are external.
        const EXTERNAL_FRAMEWORKS: &[&str] = &[
            "Foundation",
            "UIKit",
            "SwiftUI",
            "AppKit",
            "CoreData",
            "CoreGraphics",
            "Combine",
            "Darwin",
            "Dispatch",
            "ObjectiveC",
            "os",
            "XCTest",
        ];
        if EXTERNAL_FRAMEWORKS.contains(&import) {
            return None;
        }

        // Try Swift Package Manager convention: Sources/ModuleName/
        for file in &self.indexed_files {
            if !file.ends_with(".swift") {
                continue;
            }
            // Check if file is under Sources/ModuleName/ directory.
            let prefix = format!("Sources/{import}/");
            if file.starts_with(&prefix) {
                return Some(file.clone());
            }
        }

        // Try finding a directory matching the module name with .swift files.
        for file in &self.indexed_files {
            if !file.ends_with(".swift") {
                continue;
            }
            let dir = parent_dir(file);
            if dir == import || dir.ends_with(&format!("/{import}")) {
                return Some(file.clone());
            }
        }

        None
    }

    // -------------------------------------------------------------------------
    // Zig
    // -------------------------------------------------------------------------

    fn resolve_zig(&self, import: &str, source_file: &str) -> Option<String> {
        // `@import("std")` and other non-.zig imports are external packages.
        if !import.ends_with(".zig") {
            return None;
        }

        // Relative file import: resolve relative to the source file.
        let source_dir = parent_dir(source_file);
        let candidate = normalize_path(&join_paths(&source_dir, import));
        if self.indexed_files.contains(&candidate) {
            return Some(candidate);
        }

        // Also try from the project root (bare path).
        if self.indexed_files.contains(import) {
            return Some(import.to_string());
        }

        // Try under src/.
        let as_src = format!("src/{import}");
        if self.indexed_files.contains(&as_src) {
            return Some(as_src);
        }

        None
    }

    // -------------------------------------------------------------------------
    // PHP (PSR-4 style)
    // -------------------------------------------------------------------------

    fn resolve_php(&self, import: &str, source_file: &str, is_relative: bool) -> Option<String> {
        if is_relative {
            // `require_once './helpers.php'` or `require '../config.php'`
            let source_dir = parent_dir(source_file);
            let joined = join_paths(&source_dir, import);
            let norm = normalize_path(&joined);
            if self.indexed_files.contains(&norm) {
                return Some(norm);
            }
            return None;
        }

        // `require 'vendor/...'` — vendor dependencies are external.
        if import.starts_with("vendor/") || import.starts_with("vendor\\") {
            return None;
        }

        // If the import looks like a file path (has .php extension), try it directly.
        if import.ends_with(".php") {
            if self.indexed_files.contains(import) {
                return Some(import.to_string());
            }
            // Try with normalized slashes.
            let normalized = import.replace('\\', "/");
            if self.indexed_files.contains(&normalized) {
                return Some(normalized);
            }
            return None;
        }

        // PSR-4 namespace resolution: `App\Models\User` → try common layouts.
        let path = import.replace('\\', "/");

        // Try common PHP project directory prefixes.
        for prefix in &["src", "app", "lib", ""] {
            let base = if prefix.is_empty() {
                path.clone()
            } else {
                format!("{prefix}/{path}")
            };
            let as_file = format!("{base}.php");
            if self.indexed_files.contains(&as_file) {
                return Some(as_file);
            }
        }

        // Also try stripping the first namespace segment (e.g. `App\Models\User` → `Models/User.php`)
        // since PSR-4 often maps the root namespace to a directory.
        if let Some(idx) = path.find('/') {
            let without_root = &path[idx + 1..];
            for prefix in &["src", "app", "lib", ""] {
                let base = if prefix.is_empty() {
                    without_root.to_string()
                } else {
                    format!("{prefix}/{without_root}")
                };
                let as_file = format!("{base}.php");
                if self.indexed_files.contains(&as_file) {
                    return Some(as_file);
                }
            }
        }

        None
    }

    // -------------------------------------------------------------------------
    // Bash
    // -------------------------------------------------------------------------

    fn resolve_bash(&self, import: &str, source_file: &str) -> Option<String> {
        // Absolute path: check if it matches an indexed file.
        if import.starts_with('/') {
            // We can't resolve absolute paths against our relative indexed files
            // without knowing the project root mount point. Return None.
            return None;
        }

        // Relative path: resolve relative to the source file.
        let source_dir = parent_dir(source_file);
        let joined = join_paths(&source_dir, import);
        let norm = normalize_path(&joined);
        if self.indexed_files.contains(&norm) {
            return Some(norm);
        }

        // Try the bare import path from project root.
        let bare = normalize_path(import);
        if self.indexed_files.contains(&bare) {
            return Some(bare);
        }

        None
    }

    // -------------------------------------------------------------------------
    // Matlab
    // -------------------------------------------------------------------------

    fn resolve_matlab(&self, import: &str, source_file: &str) -> Option<String> {
        // Matlab function calls resolve to `functionname.m` in the same directory
        // or on the path. The extractor provides function names for direct calls
        // and directory paths for `addpath`.

        // If it looks like a directory path (addpath), we can't resolve to a single file.
        if import.ends_with('/') {
            return None;
        }

        // Dot-qualified path: aerotool.core.SessionState
        if import.contains('.') {
            let segments: Vec<&str> = import.split('.').collect();

            // Strategy 1: MATLAB +pkg convention
            // aerotool.core.SessionState → +aerotool/+core/SessionState.m
            let plus_path = segments
                .iter()
                .enumerate()
                .map(|(i, seg)| {
                    if i < segments.len() - 1 {
                        format!("+{seg}")
                    } else {
                        seg.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("/")
                + ".m";
            if self.indexed_files.contains(&plus_path) {
                return Some(plus_path);
            }

            // Strategy 2: Plain path without + prefixes
            let plain_path = segments.join("/") + ".m";
            if self.indexed_files.contains(&plain_path) {
                return Some(plain_path);
            }

            // Strategy 3: Last-segment fallback (search by function name)
            let last = segments.last().unwrap_or(&"");
            let suffix = format!("/{last}.m");
            let root_name = format!("{last}.m");
            for f in &self.indexed_files {
                if f.ends_with(&suffix) || f == &root_name {
                    return Some(f.clone());
                }
            }

            return None;
        }

        // Non-dot: plain function name → functionname.m
        if import.contains('/') {
            return None;
        }

        // Try to find `import.m` relative to the source file.
        let source_dir = parent_dir(source_file);
        let candidate = if source_dir.is_empty() {
            format!("{import}.m")
        } else {
            format!("{source_dir}/{import}.m")
        };
        if self.indexed_files.contains(&candidate) {
            return Some(candidate);
        }

        // Try from project root.
        let root_candidate = format!("{import}.m");
        if self.indexed_files.contains(&root_candidate) {
            return Some(root_candidate);
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
    fn assembly_import_returns_none() {
        // Assembly has no import resolution — v0.37 added it to the line-based
        // language set. This test pins that behavior so regressions trip CI.
        let resolver = make_resolver(&["arch/arm64/kernel/entry.S"]);
        let raw = RawImport {
            path: "foo".to_string(),
            language: Language::Assembly,
            is_relative: false,
        };
        assert_eq!(
            resolver.resolve(&raw, "arch/arm64/kernel/entry.S"),
            None,
            "assembly imports should always resolve to None"
        );
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
    fn typescript_package_alias_resolves_to_src_suffix() {
        let resolver = make_resolver(&["src/plugin-sdk/plugin-entry.ts"]);
        let raw = RawImport {
            path: "openclaw/plugin-sdk/plugin-entry".to_string(),
            language: Language::TypeScript,
            is_relative: false,
        };

        assert_eq!(
            resolver.resolve(&raw, "extensions/openai/index.ts"),
            Some("src/plugin-sdk/plugin-entry.ts".to_string())
        );
    }

    #[test]
    fn typescript_package_alias_resolves_directory_index() {
        let resolver = make_resolver(&["src/plugin-sdk/index.ts"]);
        let raw = RawImport {
            path: "openclaw/plugin-sdk".to_string(),
            language: Language::TypeScript,
            is_relative: false,
        };

        assert_eq!(
            resolver.resolve(&raw, "extensions/openai/index.ts"),
            Some("src/plugin-sdk/index.ts".to_string())
        );
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

    // -----------------------------------------------------------------
    // PHP resolver tests
    // -----------------------------------------------------------------

    #[test]
    fn php_psr4_namespace_resolves() {
        let resolver = make_resolver(&["src/Models/User.php", "app/Http/Controller.php"]);
        let raw = RawImport {
            path: "App\\Models\\User".to_string(),
            language: Language::Php,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "src/index.php");
        assert_eq!(resolved, Some("src/Models/User.php".to_string()));
    }

    #[test]
    fn php_psr4_app_prefix_resolves() {
        let resolver = make_resolver(&["app/Http/Controller.php"]);
        let raw = RawImport {
            path: "App\\Http\\Controller".to_string(),
            language: Language::Php,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "app/index.php");
        assert_eq!(resolved, Some("app/Http/Controller.php".to_string()));
    }

    #[test]
    fn php_relative_require_resolves() {
        let resolver = make_resolver(&["lib/helpers.php"]);
        let raw = RawImport {
            path: "./helpers.php".to_string(),
            language: Language::Php,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "lib/index.php");
        assert_eq!(resolved, Some("lib/helpers.php".to_string()));
    }

    #[test]
    fn php_relative_parent_dir_resolves() {
        let resolver = make_resolver(&["config.php"]);
        let raw = RawImport {
            path: "../config.php".to_string(),
            language: Language::Php,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/index.php");
        assert_eq!(resolved, Some("config.php".to_string()));
    }

    #[test]
    fn php_vendor_returns_none() {
        let resolver = make_resolver(&["src/main.php"]);
        let raw = RawImport {
            path: "vendor/autoload.php".to_string(),
            language: Language::Php,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "src/main.php"), None);
    }

    // -----------------------------------------------------------------
    // Zig resolver tests
    // -----------------------------------------------------------------

    #[test]
    fn zig_relative_import_resolves() {
        let resolver = make_resolver(&["src/utils.zig", "src/main.zig"]);
        let raw = RawImport {
            path: "utils.zig".to_string(),
            language: Language::Zig,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/main.zig");
        assert_eq!(resolved, Some("src/utils.zig".to_string()));
    }

    #[test]
    fn zig_std_import_returns_none() {
        let resolver = make_resolver(&["src/main.zig"]);
        let raw = RawImport {
            path: "std".to_string(),
            language: Language::Zig,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "src/main.zig"), None);
    }

    #[test]
    fn zig_package_import_returns_none() {
        let resolver = make_resolver(&["src/main.zig"]);
        let raw = RawImport {
            path: "zap".to_string(),
            language: Language::Zig,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "src/main.zig"), None);
    }

    #[test]
    fn zig_src_fallback_resolves() {
        let resolver = make_resolver(&["src/network.zig"]);
        let raw = RawImport {
            path: "network.zig".to_string(),
            language: Language::Zig,
            is_relative: true,
        };
        // Source file is at root, so relative fails but src/ fallback should work.
        let resolved = resolver.resolve(&raw, "build.zig");
        assert_eq!(resolved, Some("src/network.zig".to_string()));
    }

    // -----------------------------------------------------------------
    // Swift resolver tests
    // -----------------------------------------------------------------

    #[test]
    fn swift_external_framework_returns_none() {
        let resolver = make_resolver(&["Sources/MyApp/main.swift"]);
        let raw = RawImport {
            path: "Foundation".to_string(),
            language: Language::Swift,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "Sources/MyApp/main.swift"), None);
    }

    #[test]
    fn swift_uikit_returns_none() {
        let resolver = make_resolver(&["Sources/MyApp/main.swift"]);
        let raw = RawImport {
            path: "UIKit".to_string(),
            language: Language::Swift,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "Sources/MyApp/main.swift"), None);
    }

    #[test]
    fn swift_local_module_resolves_spm() {
        let resolver = make_resolver(&[
            "Sources/MyApp/main.swift",
            "Sources/Networking/Client.swift",
        ]);
        let raw = RawImport {
            path: "Networking".to_string(),
            language: Language::Swift,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "Sources/MyApp/main.swift");
        assert_eq!(
            resolved,
            Some("Sources/Networking/Client.swift".to_string())
        );
    }

    #[test]
    fn swift_local_module_resolves_by_dir() {
        let resolver = make_resolver(&["Networking/Client.swift", "App/main.swift"]);
        let raw = RawImport {
            path: "Networking".to_string(),
            language: Language::Swift,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "App/main.swift");
        assert_eq!(resolved, Some("Networking/Client.swift".to_string()));
    }

    // -----------------------------------------------------------------
    // Bash resolver tests
    // -----------------------------------------------------------------

    #[test]
    fn bash_relative_source_resolves() {
        let resolver = make_resolver(&["scripts/helpers.sh", "scripts/deploy.sh"]);
        let raw = RawImport {
            path: "./helpers.sh".to_string(),
            language: Language::Bash,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "scripts/deploy.sh");
        assert_eq!(resolved, Some("scripts/helpers.sh".to_string()));
    }

    #[test]
    fn bash_parent_relative_resolves() {
        let resolver = make_resolver(&["lib/common.sh", "scripts/deploy.sh"]);
        let raw = RawImport {
            path: "../lib/common.sh".to_string(),
            language: Language::Bash,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "scripts/deploy.sh");
        assert_eq!(resolved, Some("lib/common.sh".to_string()));
    }

    #[test]
    fn bash_absolute_path_returns_none() {
        let resolver = make_resolver(&["scripts/deploy.sh"]);
        let raw = RawImport {
            path: "/usr/local/bin/helpers.sh".to_string(),
            language: Language::Bash,
            is_relative: false,
        };
        assert_eq!(resolver.resolve(&raw, "scripts/deploy.sh"), None);
    }

    #[test]
    fn bash_bare_path_from_root() {
        let resolver = make_resolver(&["lib/utils.sh", "scripts/run.sh"]);
        let raw = RawImport {
            path: "lib/utils.sh".to_string(),
            language: Language::Bash,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "scripts/run.sh");
        assert_eq!(resolved, Some("lib/utils.sh".to_string()));
    }

    // -----------------------------------------------------------------
    // Matlab resolver tests
    // -----------------------------------------------------------------

    #[test]
    fn matlab_function_resolves_same_dir() {
        let resolver = make_resolver(&["src/helper.m", "src/main.m"]);
        let raw = RawImport {
            path: "helper".to_string(),
            language: Language::Matlab,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "src/main.m");
        assert_eq!(resolved, Some("src/helper.m".to_string()));
    }

    #[test]
    fn matlab_function_resolves_from_root() {
        let resolver = make_resolver(&["utils.m", "src/main.m"]);
        let raw = RawImport {
            path: "utils".to_string(),
            language: Language::Matlab,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "src/main.m");
        assert_eq!(resolved, Some("utils.m".to_string()));
    }

    #[test]
    fn matlab_addpath_dir_returns_none() {
        let resolver = make_resolver(&["lib/tool.m"]);
        let raw = RawImport {
            path: "lib/tools".to_string(),
            language: Language::Matlab,
            is_relative: false,
        };
        // addpath-style directory import should return None.
        assert_eq!(resolver.resolve(&raw, "main.m"), None);
    }

    #[test]
    fn resolve_matlab_dot_path_with_plus_prefix() {
        let resolver = make_resolver(&[
            "+aerotool/+core/SessionState.m",
            "+aerotool/+compute/GatingEvaluator.m",
            "src/utils.m",
        ]);
        let raw = RawImport {
            path: "aerotool.core.SessionState".to_string(),
            language: Language::Matlab,
            is_relative: false,
        };
        assert_eq!(
            resolver.resolve(&raw, "main.m"),
            Some("+aerotool/+core/SessionState.m".to_string())
        );
    }

    #[test]
    fn resolve_matlab_dot_path_without_plus() {
        let resolver = make_resolver(&["aerotool/core/SessionState.m"]);
        let raw = RawImport {
            path: "aerotool.core.SessionState".to_string(),
            language: Language::Matlab,
            is_relative: false,
        };
        assert_eq!(
            resolver.resolve(&raw, "main.m"),
            Some("aerotool/core/SessionState.m".to_string())
        );
    }

    #[test]
    fn resolve_matlab_dot_path_last_segment_fallback() {
        let resolver = make_resolver(&["lib/SessionState.m"]);
        let raw = RawImport {
            path: "aerotool.core.SessionState".to_string(),
            language: Language::Matlab,
            is_relative: false,
        };
        let resolved = resolver.resolve(&raw, "main.m");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("SessionState.m"));
    }

    #[test]
    fn resolve_matlab_plain_name_still_works() {
        let resolver = make_resolver(&["lib/helper.m"]);
        let raw = RawImport {
            path: "helper".to_string(),
            language: Language::Matlab,
            is_relative: false,
        };
        assert_eq!(
            resolver.resolve(&raw, "lib/main.m"),
            Some("lib/helper.m".to_string())
        );
    }

    // -----------------------------------------------------------------
    // TypeScript .js → .ts extension swap tests (node16 / bundler)
    // -----------------------------------------------------------------

    #[test]
    fn typescript_js_extension_resolves_to_ts() {
        let resolver = make_resolver(&["src/utils.ts", "src/index.ts"]);
        let raw = RawImport {
            path: "./utils.js".to_string(),
            language: Language::TypeScript,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/index.ts");
        assert_eq!(resolved, Some("src/utils.ts".to_string()));
    }

    #[test]
    fn typescript_jsx_extension_resolves_to_tsx() {
        let resolver = make_resolver(&["src/Button.tsx", "src/App.tsx"]);
        let raw = RawImport {
            path: "./Button.jsx".to_string(),
            language: Language::TypeScript,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/App.tsx");
        assert_eq!(resolved, Some("src/Button.tsx".to_string()));
    }

    #[test]
    fn typescript_mjs_extension_resolves_to_mts() {
        let resolver = make_resolver(&["lib/config.mts", "lib/main.mts"]);
        let raw = RawImport {
            path: "./config.mjs".to_string(),
            language: Language::TypeScript,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "lib/main.mts");
        assert_eq!(resolved, Some("lib/config.mts".to_string()));
    }

    #[test]
    fn typescript_cjs_extension_resolves_to_cts() {
        let resolver = make_resolver(&["lib/helper.cts", "lib/main.cts"]);
        let raw = RawImport {
            path: "./helper.cjs".to_string(),
            language: Language::TypeScript,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "lib/main.cts");
        assert_eq!(resolved, Some("lib/helper.cts".to_string()));
    }

    #[test]
    fn typescript_js_extension_falls_through_if_no_ts() {
        let resolver = make_resolver(&["src/legacy.js", "src/index.ts"]);
        let raw = RawImport {
            path: "./legacy.js".to_string(),
            language: Language::TypeScript,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/index.ts");
        assert_eq!(resolved, Some("src/legacy.js".to_string()));
    }

    #[test]
    fn typescript_js_extension_prefers_ts_over_js() {
        // Both .ts and .js exist — .ts should win (TypeScript compiler behavior).
        let resolver = make_resolver(&["src/utils.ts", "src/utils.js", "src/index.ts"]);
        let raw = RawImport {
            path: "./utils.js".to_string(),
            language: Language::TypeScript,
            is_relative: true,
        };
        let resolved = resolver.resolve(&raw, "src/index.ts");
        assert_eq!(resolved, Some("src/utils.ts".to_string()));
    }
}
