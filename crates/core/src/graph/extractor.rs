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
}
