//! Tree-sitter AST import extraction for all supported languages.

use tree_sitter::{Node, Tree};

use crate::language::Language;

/// A raw import statement extracted from source code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    /// The import path string (e.g. `crate::parser`, `./utils`, `net/http`).
    pub path: String,
    /// Language the import came from.
    pub language: Language,
    /// True if the import is relative to the current file.
    pub is_relative: bool,
}

/// Extracts raw imports from a tree-sitter AST.
pub struct ImportExtractor;

impl ImportExtractor {
    /// Walk the AST and extract all import/use statements for the given language.
    pub fn extract(tree: &Tree, source: &[u8], language: Language) -> Vec<RawImport> {
        match language {
            Language::Rust => extract_rust(tree, source),
            Language::Python => extract_python(tree, source),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                extract_js_ts(tree, source, language)
            }
            Language::Go => extract_go(tree, source),
            Language::Java => extract_java(tree, source),
            Language::C | Language::Cpp => extract_c(tree, source, language),
            Language::CSharp => extract_csharp(tree, source),
            Language::Ruby => extract_ruby(tree, source),
            Language::Swift => extract_swift(tree, source),
            Language::Kotlin => extract_kotlin(tree, source),
            Language::Scala => extract_scala(tree, source),
            Language::Zig => extract_zig(tree, source),
            Language::Php => extract_php(tree, source),
            Language::Bash => extract_bash(tree, source),
            Language::Matlab => extract_matlab(tree, source),
            // Config languages use line-based parsing; no tree-sitter imports.
            Language::Yaml
            | Language::Toml
            | Language::Dockerfile
            | Language::Makefile
            | Language::Mermaid
            | Language::Xml => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn node_text<'a>(node: &Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Walk all descendant nodes, calling `visit` for each.
fn walk_all<F: FnMut(Node)>(node: Node, visit: &mut F) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        visit(n);
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

fn extract_rust(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "use_declaration" {
            return;
        }
        // Recursively flatten the use tree into paths.
        let mut paths = Vec::new();
        flatten_use_tree(node, source, "", &mut paths);
        for path in paths {
            let is_relative = path.starts_with("crate::") || path.starts_with("super::");
            imports.push(RawImport {
                path,
                language: Language::Rust,
                is_relative,
            });
        }
    });
    imports
}

fn flatten_use_tree(node: Node, source: &[u8], prefix: &str, out: &mut Vec<String>) {
    match node.kind() {
        "use_declaration" => {
            let mut c = node.walk();
            for child in node.children(&mut c) {
                if child.kind() != "use" && child.kind() != ";" {
                    flatten_use_tree(child, source, prefix, out);
                }
            }
        }
        "scoped_use_list" => {
            // e.g. `crate::parser::{Foo, Bar}`
            let mut path_part = String::new();
            let mut c = node.walk();
            for child in node.children(&mut c) {
                match child.kind() {
                    "use_list" => {
                        flatten_use_tree(child, source, &format!("{prefix}{path_part}"), out);
                    }
                    "::" => {}
                    _ => {
                        let t = node_text(&child, source);
                        if !t.is_empty() && t != "::" {
                            if path_part.is_empty() {
                                path_part = t.to_string();
                            } else {
                                path_part = format!("{path_part}::{t}");
                            }
                        }
                    }
                }
            }
        }
        "use_list" => {
            let mut c = node.walk();
            for child in node.children(&mut c) {
                if child.kind() != "," && child.kind() != "{" && child.kind() != "}" {
                    flatten_use_tree(child, source, prefix, out);
                }
            }
        }
        "use_wildcard" | "use_as_clause" => {
            // e.g. `crate::foo::*`  or  `use foo as bar`
            let text = node_text(&node, source);
            let path = if prefix.is_empty() {
                text.to_string()
            } else {
                format!("{prefix}::{text}")
            };
            out.push(path);
        }
        "scoped_identifier" | "identifier" | "self" | "crate" | "super" => {
            let text = node_text(&node, source);
            let path = if prefix.is_empty() {
                text.to_string()
            } else {
                format!("{prefix}::{text}")
            };
            out.push(path);
        }
        _ => {
            // For any other node (e.g. a scoped_identifier that spans the whole path)
            // just emit its text.
            let text = node_text(&node, source);
            if !text.is_empty() && !text.contains('{') {
                let path = if prefix.is_empty() {
                    text.to_string()
                } else {
                    format!("{prefix}::{text}")
                };
                out.push(path);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

fn extract_python(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        match node.kind() {
            "import_statement" => {
                // `import foo.bar` → path = "foo.bar"
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    let t = node_text(&child, source).trim().to_string();
                    if !t.is_empty() {
                        imports.push(RawImport {
                            path: t,
                            language: Language::Python,
                            is_relative: false,
                        });
                    }
                }
            }
            "import_from_statement" => {
                // `from . import helpers` or `from .sibling import Foo`
                let module = extract_python_from_module(&node, source);
                let is_relative = module.starts_with('.');
                if !module.is_empty() {
                    imports.push(RawImport {
                        path: module,
                        language: Language::Python,
                        is_relative,
                    });
                }
            }
            _ => {}
        }
    });
    imports
}

fn extract_python_from_module(node: &Node, source: &[u8]) -> String {
    // Children: `from` keyword, then the module (dotted_name or relative_import)
    let mut c = node.walk();
    let children: Vec<Node> = node.children(&mut c).collect();
    // Find the module after `from`
    let mut found_from = false;
    for child in &children {
        if child.kind() == "from" {
            found_from = true;
            continue;
        }
        if found_from && child.kind() != "import" {
            let text = node_text(child, source).trim().to_string();
            if !text.is_empty() {
                return text;
            }
        }
        if child.kind() == "import" {
            break;
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// TypeScript / JavaScript
// ---------------------------------------------------------------------------

fn extract_js_ts(tree: &Tree, source: &[u8], language: Language) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        match node.kind() {
            "import_statement" => {
                // Look for `string` child containing the path.
                let mut c = node.walk();
                for child in node.named_children(&mut c) {
                    if child.kind() == "string" {
                        let raw = node_text(&child, source);
                        let path = strip_quotes(raw);
                        if !path.is_empty() {
                            let is_relative = path.starts_with("./") || path.starts_with("../");
                            imports.push(RawImport {
                                path: path.to_string(),
                                language,
                                is_relative,
                            });
                        }
                    }
                }
            }
            "call_expression" => {
                // require("./foo") calls
                let func_text = node
                    .child_by_field_name("function")
                    .map(|f| node_text(&f, source))
                    .unwrap_or("");
                if func_text == "require" {
                    if let Some(args) = node.child_by_field_name("arguments") {
                        let mut c = args.walk();
                        for arg in args.named_children(&mut c) {
                            if arg.kind() == "string" {
                                let raw = node_text(&arg, source);
                                let path = strip_quotes(raw);
                                if !path.is_empty() {
                                    let is_relative =
                                        path.starts_with("./") || path.starts_with("../");
                                    imports.push(RawImport {
                                        path: path.to_string(),
                                        language,
                                        is_relative,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    });
    imports
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"'))
        || (s.starts_with('\'') && s.ends_with('\''))
        || (s.starts_with('`') && s.ends_with('`'))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

fn extract_go(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() == "import_spec" {
            // The path child is a `interpreted_string_literal`.
            let mut c = node.walk();
            for child in node.named_children(&mut c) {
                if child.kind() == "interpreted_string_literal" {
                    let raw = node_text(&child, source);
                    let path = strip_quotes(raw);
                    if !path.is_empty() {
                        imports.push(RawImport {
                            path: path.to_string(),
                            language: Language::Go,
                            is_relative: false, // Go imports are always absolute
                        });
                    }
                }
            }
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

fn extract_java(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() == "import_declaration" {
            let text = node_text(&node, source);
            // Strip leading `import ` and trailing `;`.
            let path = text
                .trim()
                .strip_prefix("import ")
                .unwrap_or("")
                .trim_end_matches(';')
                .trim()
                .to_string();
            if !path.is_empty() {
                imports.push(RawImport {
                    path,
                    language: Language::Java,
                    is_relative: false,
                });
            }
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// C / C++
// ---------------------------------------------------------------------------

fn extract_c(tree: &Tree, source: &[u8], language: Language) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() == "preproc_include" {
            let text = node_text(&node, source);
            // #include "foo.h" → relative; #include <stdio.h> → external
            let (path, is_relative) = parse_c_include(text);
            if !path.is_empty() {
                imports.push(RawImport {
                    path,
                    language,
                    is_relative,
                });
            }
        }
    });
    imports
}

fn parse_c_include(text: &str) -> (String, bool) {
    // Find the include path between quotes or angle brackets.
    let text = text.trim();
    if let Some(start) = text.find('"') {
        if let Some(end) = text[start + 1..].find('"') {
            return (text[start + 1..start + 1 + end].to_string(), true);
        }
    }
    if let Some(start) = text.find('<') {
        if let Some(end) = text[start + 1..].find('>') {
            return (text[start + 1..start + 1 + end].to_string(), false);
        }
    }
    (String::new(), false)
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

fn extract_csharp(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() == "using_directive" {
            let text = node_text(&node, source);
            // `using System.IO;` → path = "System.IO"
            let path = text
                .trim()
                .strip_prefix("using ")
                .unwrap_or("")
                .trim_end_matches(';')
                .trim()
                .to_string();
            if !path.is_empty() && !path.starts_with("static ") && !path.contains('=') {
                imports.push(RawImport {
                    path,
                    language: Language::CSharp,
                    is_relative: false,
                });
            }
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// Ruby
// ---------------------------------------------------------------------------

fn extract_ruby(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call" {
            return;
        }
        // Look for `require`, `require_relative`, or `include` method calls.
        let method_text = node
            .child_by_field_name("method")
            .map(|n| node_text(&n, source))
            .unwrap_or("");
        match method_text {
            "require" | "require_relative" => {
                let is_relative = method_text == "require_relative";
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut c = args.walk();
                    for arg in args.named_children(&mut c) {
                        if arg.kind() == "string" || arg.kind() == "string_content" {
                            let raw = node_text(&arg, source);
                            let path = strip_quotes(raw.trim());
                            if !path.is_empty() {
                                imports.push(RawImport {
                                    path: path.to_string(),
                                    language: Language::Ruby,
                                    is_relative,
                                });
                            }
                        } else {
                            // May be a bare string node with string_content children.
                            let mut ic = arg.walk();
                            for inner in arg.children(&mut ic) {
                                if inner.kind() == "string_content" {
                                    let path = node_text(&inner, source);
                                    if !path.is_empty() {
                                        imports.push(RawImport {
                                            path: path.to_string(),
                                            language: Language::Ruby,
                                            is_relative,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "include" => {
                // `include Module` — constant reference.
                if let Some(args) = node.child_by_field_name("arguments") {
                    let mut c = args.walk();
                    for arg in args.named_children(&mut c) {
                        let t = node_text(&arg, source).trim().to_string();
                        if !t.is_empty() {
                            imports.push(RawImport {
                                path: t,
                                language: Language::Ruby,
                                is_relative: false,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// Swift
// ---------------------------------------------------------------------------

fn extract_swift(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "import_declaration" {
            return;
        }
        // `import Foundation` or `import class UIKit.UIView`
        let text = node_text(&node, source).trim().to_string();
        // Strip leading `import` keyword and optional access-kind (class, struct, …).
        let after_import = text.strip_prefix("import").unwrap_or("").trim();
        // Strip optional access kind: class, struct, enum, protocol, typealias, func, var, let.
        let module = strip_swift_access_kind(after_import);
        if !module.is_empty() {
            // The first component (before any `.`) is the module name.
            let module_name = module.split('.').next().unwrap_or(module);
            imports.push(RawImport {
                path: module_name.to_string(),
                language: Language::Swift,
                is_relative: false,
            });
        }
    });
    imports
}

/// Strip an optional Swift import access kind from an import path token.
fn strip_swift_access_kind(s: &str) -> &str {
    const ACCESS_KINDS: &[&str] = &[
        "class ",
        "struct ",
        "enum ",
        "protocol ",
        "typealias ",
        "func ",
        "var ",
        "let ",
    ];
    for ak in ACCESS_KINDS {
        if let Some(rest) = s.strip_prefix(ak) {
            return rest.trim();
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Kotlin
// ---------------------------------------------------------------------------

fn extract_kotlin(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "import_header" {
            return;
        }
        // `import com.example.Foo` or `import com.example.*`
        let text = node_text(&node, source).trim().to_string();
        let path = text
            .strip_prefix("import ")
            .unwrap_or("")
            .trim_end_matches(';')
            .trim()
            .to_string();
        if !path.is_empty() {
            imports.push(RawImport {
                path,
                language: Language::Kotlin,
                is_relative: false,
            });
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// Scala
// ---------------------------------------------------------------------------

fn extract_scala(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "import_declaration" {
            return;
        }
        // `import com.example.Foo` or `import com.example._`
        let text = node_text(&node, source).trim().to_string();
        let path = text
            .strip_prefix("import ")
            .unwrap_or("")
            .trim()
            .to_string();
        if !path.is_empty() {
            imports.push(RawImport {
                path,
                language: Language::Scala,
                is_relative: false,
            });
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// Zig
// ---------------------------------------------------------------------------

fn extract_zig(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "builtin_function" {
            return;
        }
        // Check if this is an @import call.
        let builtin_name = node.child(0).map(|n| node_text(&n, source)).unwrap_or("");
        if builtin_name != "@import" {
            return;
        }
        // Find the string argument.
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if child.kind() == "arguments" {
                let mut ac = child.walk();
                for arg in child.children(&mut ac) {
                    if arg.kind() == "string" {
                        // Extract string_content child.
                        let mut sc = arg.walk();
                        for s in arg.children(&mut sc) {
                            if s.kind() == "string_content" {
                                let path = node_text(&s, source).to_string();
                                if !path.is_empty() {
                                    let is_relative = path.ends_with(".zig");
                                    imports.push(RawImport {
                                        path,
                                        language: Language::Zig,
                                        is_relative,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// PHP
// ---------------------------------------------------------------------------

fn extract_php(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        match node.kind() {
            "namespace_use_declaration" => {
                // `use Foo\Bar\Baz;`
                let mut c = node.walk();
                for child in node.children(&mut c) {
                    if child.kind() == "namespace_use_clause" {
                        let text = node_text(&child, source).trim().to_string();
                        if !text.is_empty() {
                            imports.push(RawImport {
                                path: text,
                                language: Language::Php,
                                is_relative: false,
                            });
                        }
                    }
                }
            }
            "require_expression"
            | "require_once_expression"
            | "include_expression"
            | "include_once_expression" => {
                // Find the string argument.
                let mut c = node.walk();
                for child in node.children(&mut c) {
                    if child.kind() == "string" {
                        let mut sc = child.walk();
                        for s in child.children(&mut sc) {
                            if s.kind() == "string_content" {
                                let path = node_text(&s, source).to_string();
                                if !path.is_empty() {
                                    let is_relative =
                                        path.starts_with("./") || path.starts_with("../");
                                    imports.push(RawImport {
                                        path,
                                        language: Language::Php,
                                        is_relative,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// Bash
// ---------------------------------------------------------------------------

fn extract_bash(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "command" {
            return;
        }
        // Look for `source` or `.` commands.
        let mut c = node.walk();
        let children: Vec<_> = node.children(&mut c).collect();
        if children.is_empty() {
            return;
        }
        let cmd_name = node_text(&children[0], source).trim().to_string();
        if cmd_name != "source" && cmd_name != "." {
            return;
        }
        // The second child is the file argument.
        if children.len() < 2 {
            return;
        }
        // The argument might be a word or a string node.
        let arg_node = &children[1];
        let path = match arg_node.kind() {
            "string" => {
                // Extract string_content child.
                let mut sc = arg_node.walk();
                let mut content = String::new();
                for s in arg_node.children(&mut sc) {
                    if s.kind() == "string_content" || s.kind() == "raw_string_content" {
                        content = node_text(&s, source).to_string();
                        break;
                    }
                }
                if content.is_empty() {
                    // Fallback: strip quotes from the node text.
                    let text = node_text(arg_node, source);
                    text.trim_matches('"').trim_matches('\'').to_string()
                } else {
                    content
                }
            }
            "raw_string" => {
                let text = node_text(arg_node, source);
                text.trim_matches('\'').to_string()
            }
            "word" | "simple_expansion" | "concatenation" => {
                node_text(arg_node, source).trim().to_string()
            }
            _ => node_text(arg_node, source).trim().to_string(),
        };

        if !path.is_empty() {
            // Skip paths with variable expansions ($VAR, ${VAR}) — too complex.
            if path.contains('$') {
                return;
            }
            let is_relative = !path.starts_with('/');
            imports.push(RawImport {
                path,
                language: Language::Bash,
                is_relative,
            });
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// Matlab
// ---------------------------------------------------------------------------

fn extract_matlab(tree: &Tree, source: &[u8]) -> Vec<RawImport> {
    let mut imports = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        // Look for function calls: `addpath('dir')` and direct function calls
        // that could reference .m files.
        if node.kind() != "function_call" {
            return;
        }
        // Get the function name.
        let mut c = node.walk();
        let children: Vec<_> = node.children(&mut c).collect();
        if children.is_empty() {
            return;
        }
        let func_name = node_text(&children[0], source).trim().to_string();

        if func_name == "addpath" || func_name == "run" {
            // Extract string argument for addpath/run.
            for child in &children {
                if child.kind() == "arguments" {
                    let mut ac = child.walk();
                    for arg in child.children(&mut ac) {
                        if arg.kind() == "string" {
                            let text = node_text(&arg, source);
                            let path = text.trim_matches('\'').trim_matches('"').trim().to_string();
                            if !path.is_empty() {
                                imports.push(RawImport {
                                    path,
                                    language: Language::Matlab,
                                    is_relative: false,
                                });
                            }
                        }
                    }
                }
            }
        }
    });
    imports
}

// ---------------------------------------------------------------------------
// CallExtractor — function/method call sites
// ---------------------------------------------------------------------------

/// Extracts the names of called functions/methods from a tree-sitter AST.
///
/// Only simple call names are extracted (direct identifiers and the last
/// segment of qualified calls).  Used to build `EdgeKind::Calls` edges in the
/// dependency graph.
pub struct CallExtractor;

impl CallExtractor {
    /// Walk the AST and return callee names for the given language.
    pub fn extract_calls(tree: &Tree, source: &[u8], language: Language) -> Vec<String> {
        match language {
            Language::Rust => extract_rust_calls(tree, source),
            Language::Python => extract_python_calls(tree, source),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                extract_js_calls(tree, source)
            }
            Language::Go => extract_go_calls(tree, source),
            Language::Java => extract_java_calls(tree, source),
            Language::C | Language::Cpp => extract_c_cpp_calls(tree, source),
            Language::Ruby => extract_ruby_calls(tree, source),
            Language::Swift => extract_swift_calls(tree, source),
            Language::Kotlin => extract_kotlin_calls(tree, source),
            _ => Vec::new(),
        }
    }
}

fn extract_rust_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call_expression" {
            return;
        }
        if let Some(func) = node.child_by_field_name("function") {
            let name = match func.kind() {
                "identifier" => node_text(&func, source).to_string(),
                "scoped_identifier" => func
                    .child_by_field_name("name")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default(),
                "field_expression" => func
                    .child_by_field_name("field")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if !name.is_empty() {
                calls.push(name);
            }
        }
    });
    calls
}

fn extract_python_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call" {
            return;
        }
        if let Some(func) = node.child_by_field_name("function") {
            let name = match func.kind() {
                "identifier" => node_text(&func, source).to_string(),
                "attribute" => func
                    .child_by_field_name("attribute")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if !name.is_empty() {
                calls.push(name);
            }
        }
    });
    calls
}

fn extract_js_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call_expression" {
            return;
        }
        if let Some(func) = node.child_by_field_name("function") {
            let name = match func.kind() {
                "identifier" => node_text(&func, source).to_string(),
                "member_expression" => func
                    .child_by_field_name("property")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if !name.is_empty() {
                calls.push(name);
            }
        }
    });
    calls
}

fn extract_go_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call_expression" {
            return;
        }
        if let Some(func) = node.child_by_field_name("function") {
            let name = match func.kind() {
                "identifier" => node_text(&func, source).to_string(),
                "selector_expression" => func
                    .child_by_field_name("field")
                    .map(|n| node_text(&n, source).to_string())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            if !name.is_empty() {
                calls.push(name);
            }
        }
    });
    calls
}

fn extract_java_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        match node.kind() {
            "method_invocation" => {
                // `obj.method(args)` — the name field holds the method name.
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = node_text(&name_node, source).to_string();
                    if !name.is_empty() {
                        calls.push(name);
                    }
                }
            }
            "object_creation_expression" => {
                // `new Foo(args)` — the type field holds the class name.
                if let Some(type_node) = node.child_by_field_name("type") {
                    let name = node_text(&type_node, source).to_string();
                    if !name.is_empty() {
                        calls.push(name);
                    }
                }
            }
            _ => {}
        }
    });
    calls
}

fn extract_c_cpp_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call_expression" {
            return;
        }
        if let Some(func) = node.child_by_field_name("function") {
            let name = match func.kind() {
                "identifier" => node_text(&func, source).to_string(),
                "qualified_identifier" => {
                    // For C++ qualified calls like `ns::func()`, extract the
                    // last identifier segment.
                    let text = node_text(&func, source);
                    text.rsplit("::").next().unwrap_or(text).to_string()
                }
                "field_expression" => {
                    // `obj.method()` or `ptr->method()`
                    func.child_by_field_name("field")
                        .map(|n| node_text(&n, source).to_string())
                        .unwrap_or_default()
                }
                "template_function" => {
                    // `func<T>()` — extract the function name.
                    func.child_by_field_name("name")
                        .map(|n| node_text(&n, source).to_string())
                        .unwrap_or_default()
                }
                _ => String::new(),
            };
            if !name.is_empty() {
                calls.push(name);
            }
        }
    });
    calls
}

fn extract_ruby_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call" {
            return;
        }
        // Ruby `call` nodes have a `method` field.
        if let Some(method) = node.child_by_field_name("method") {
            let name = node_text(&method, source).to_string();
            if !name.is_empty() {
                calls.push(name);
            }
        }
    });
    calls
}

fn extract_swift_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call_expression" {
            return;
        }
        // Swift call_expression: first child is the function expression.
        let mut c = node.walk();
        for child in node.children(&mut c) {
            match child.kind() {
                "simple_identifier" => {
                    let name = node_text(&child, source).to_string();
                    if !name.is_empty() {
                        calls.push(name);
                    }
                    break;
                }
                "navigation_expression" => {
                    // `obj.method()` — last simple_identifier child.
                    let mut nc = child.walk();
                    let mut last_id = String::new();
                    for nav_child in child.children(&mut nc) {
                        if nav_child.kind() == "simple_identifier" {
                            last_id = node_text(&nav_child, source).to_string();
                        }
                    }
                    if !last_id.is_empty() {
                        calls.push(last_id);
                    }
                    break;
                }
                _ => {}
            }
        }
    });
    calls
}

fn extract_kotlin_calls(tree: &Tree, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    walk_all(tree.root_node(), &mut |node| {
        if node.kind() != "call_expression" {
            return;
        }
        // Kotlin call_expression: first child is the callee.
        let mut c = node.walk();
        for child in node.children(&mut c) {
            match child.kind() {
                "simple_identifier" => {
                    let name = node_text(&child, source).to_string();
                    if !name.is_empty() {
                        calls.push(name);
                    }
                    break;
                }
                "navigation_expression" => {
                    // `obj.method()` — the navigation_suffix contains the
                    // method name.
                    if let Some(suffix) = child.child_by_field_name("suffix") {
                        let name = node_text(&suffix, source).to_string();
                        if !name.is_empty() {
                            calls.push(name);
                        }
                    } else {
                        // Fallback: last simple_identifier child.
                        let mut nc = child.walk();
                        let mut last_id = String::new();
                        for nav_child in child.children(&mut nc) {
                            if nav_child.kind() == "simple_identifier" {
                                last_id = node_text(&nav_child, source).to_string();
                            }
                        }
                        if !last_id.is_empty() {
                            calls.push(last_id);
                        }
                    }
                    break;
                }
                _ => {}
            }
        }
    });
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_rust(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    fn parse_ts(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    fn parse_python(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn rust_use_declaration_extracted() {
        let src = "use crate::parser::Parser;\nuse std::collections::HashMap;";
        let tree = parse_rust(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Rust);
        assert!(!imports.is_empty(), "expected at least one Rust import");
        let paths: Vec<&str> = imports.iter().map(|i| i.path.as_str()).collect();
        assert!(
            paths
                .iter()
                .any(|p| p.contains("crate") || p.contains("parser")),
            "expected crate import, got: {paths:?}"
        );
    }

    #[test]
    fn rust_relative_imports_flagged() {
        let src = "use crate::engine::Engine;";
        let tree = parse_rust(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Rust);
        let crate_import = imports.iter().find(|i| i.path.contains("crate"));
        assert!(crate_import.is_some());
        assert!(crate_import.unwrap().is_relative);
    }

    #[test]
    fn typescript_import_extracted() {
        let src = r#"import { Foo } from "./foo";"#;
        let tree = parse_ts(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::TypeScript);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "./foo");
        assert!(imports[0].is_relative);
    }

    #[test]
    fn typescript_external_import_not_relative() {
        let src = r#"import React from "react";"#;
        let tree = parse_ts(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::TypeScript);
        assert_eq!(imports.len(), 1);
        assert!(!imports[0].is_relative);
    }

    #[test]
    fn python_from_import_extracted() {
        let src = "from . import helpers";
        let tree = parse_python(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Python);
        assert!(!imports.is_empty());
        assert!(imports[0].is_relative);
    }

    #[test]
    fn c_include_relative_vs_external() {
        assert_eq!(
            parse_c_include(r#"#include "myheader.h""#),
            ("myheader.h".to_string(), true)
        );
        assert_eq!(
            parse_c_include("#include <stdio.h>"),
            ("stdio.h".to_string(), false)
        );
    }

    fn parse_bash_tree(source: &str) -> Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn bash_source_relative_extracted() {
        let src = "source ./helpers.sh";
        let tree = parse_bash_tree(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Bash);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "./helpers.sh");
        assert!(imports[0].is_relative);
    }

    #[test]
    fn bash_dot_command_extracted() {
        let src = ". ./lib/common.sh";
        let tree = parse_bash_tree(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Bash);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "./lib/common.sh");
        assert!(imports[0].is_relative);
    }

    #[test]
    fn bash_source_absolute_path() {
        let src = "source /etc/profile.d/custom.sh";
        let tree = parse_bash_tree(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Bash);
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].path, "/etc/profile.d/custom.sh");
        assert!(!imports[0].is_relative);
    }

    #[test]
    fn bash_source_with_variable_skipped() {
        let src = "source $HOME/.bashrc";
        let tree = parse_bash_tree(src);
        let imports = ImportExtractor::extract(&tree, src.as_bytes(), Language::Bash);
        // Variable expansion paths should be skipped.
        assert!(imports.is_empty());
    }
}
