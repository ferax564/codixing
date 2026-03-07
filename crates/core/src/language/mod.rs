pub mod c;
pub mod cpp;
pub mod csharp;
pub mod go;
pub mod java;
pub mod kotlin;
pub mod php;
pub mod python;
pub mod ruby;
pub mod rust;
pub mod scala;
pub mod swift;
pub mod typescript;
pub mod zig;

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Supported programming languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    // Tier 2
    Ruby,
    Swift,
    Kotlin,
    Scala,
    // Tier 3
    Zig,
    Php,
}

impl Language {
    /// Human-readable display name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::TypeScript => "TypeScript",
            Self::Tsx => "TSX",
            Self::JavaScript => "JavaScript",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::CSharp => "C#",
            Self::Ruby => "Ruby",
            Self::Swift => "Swift",
            Self::Kotlin => "Kotlin",
            Self::Scala => "Scala",
            Self::Zig => "Zig",
            Self::Php => "PHP",
        }
    }

    /// File extensions associated with this language.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::Python => &["py", "pyi"],
            Self::TypeScript => &["ts", "mts", "cts"],
            Self::Tsx => &["tsx"],
            Self::JavaScript => &["js", "mjs", "cjs", "jsx"],
            Self::Go => &["go"],
            Self::Java => &["java"],
            Self::C => &["c", "h"],
            Self::Cpp => &["cpp", "cxx", "cc", "hpp", "hxx", "hh"],
            Self::CSharp => &["cs"],
            Self::Ruby => &["rb", "rake", "gemspec"],
            Self::Swift => &["swift"],
            Self::Kotlin => &["kt", "kts"],
            Self::Scala => &["scala", "sc"],
            Self::Zig => &["zig"],
            Self::Php => &["php", "phtml", "php3", "php4", "php5", "phps"],
        }
    }
}

/// All language variants for iteration.
pub const ALL_LANGUAGES: &[Language] = &[
    Language::Rust,
    Language::Python,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Go,
    Language::Java,
    Language::C,
    Language::Cpp,
    Language::CSharp,
    Language::Ruby,
    Language::Swift,
    Language::Kotlin,
    Language::Scala,
    Language::Zig,
    Language::Php,
];

/// The kind of semantic entity extracted from an AST.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    TypeAlias,
    Constant,
    Static,
    Module,
    Import,
    Impl,
    Namespace,
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Function  => "function",
            Self::Method    => "method",
            Self::Class     => "class",
            Self::Struct    => "struct",
            Self::Enum      => "enum",
            Self::Interface => "interface",
            Self::Trait     => "trait",
            Self::TypeAlias => "type",
            Self::Constant  => "const",
            Self::Static    => "static",
            Self::Module    => "module",
            Self::Import    => "import",
            Self::Impl      => "impl",
            Self::Namespace => "namespace",
        })
    }
}

/// A semantic entity extracted from source code via tree-sitter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticEntity {
    pub kind: EntityKind,
    pub name: String,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
    pub byte_range: std::ops::Range<usize>,
    pub line_range: std::ops::Range<usize>,
    /// Scope chain from outermost to innermost (e.g., `["module", "ClassName"]`).
    pub scope: Vec<String>,
}

/// Per-language entity extraction and tree-sitter integration.
pub trait LanguageSupport: Send + Sync {
    /// Which language this implementation handles.
    fn language(&self) -> Language;

    /// The tree-sitter language function for parsing.
    fn tree_sitter_language(&self) -> tree_sitter::Language;

    /// AST node kinds that represent semantic entities.
    fn entity_node_kinds(&self) -> &[&str];

    /// Extract all semantic entities from a parsed tree.
    fn extract_entities(&self, tree: &tree_sitter::Tree, source: &[u8]) -> Vec<SemanticEntity>;

    /// Extract the signature of a node (e.g., `fn foo(x: i32) -> bool`).
    fn extract_signature(&self, node: &tree_sitter::Node, source: &[u8]) -> Option<String>;

    /// Extract doc comments preceding a node.
    fn extract_doc_comment(&self, node: &tree_sitter::Node, source: &[u8]) -> Option<String>;
}

/// Registry mapping languages to their `LanguageSupport` implementations.
pub struct LanguageRegistry {
    impls: Vec<Arc<dyn LanguageSupport>>,
}

impl LanguageRegistry {
    /// Build a registry with all supported languages (Tier 1 + Tier 2).
    pub fn new() -> Self {
        let impls: Vec<Arc<dyn LanguageSupport>> = vec![
            Arc::new(rust::RustLanguage),
            Arc::new(python::PythonLanguage),
            Arc::new(typescript::TypeScriptLanguage),
            Arc::new(typescript::TsxLanguage),
            Arc::new(typescript::JavaScriptLanguage),
            Arc::new(go::GoLanguage),
            Arc::new(java::JavaLanguage),
            Arc::new(c::CLanguage),
            Arc::new(cpp::CppLanguage),
            Arc::new(csharp::CSharpLanguage),
            Arc::new(ruby::RubyLanguage),
            Arc::new(swift::SwiftLanguage),
            Arc::new(kotlin::KotlinLanguage),
            Arc::new(scala::ScalaLanguage),
            Arc::new(zig::ZigLanguage),
            Arc::new(php::PhpLanguage),
        ];
        Self { impls }
    }

    /// Look up the `LanguageSupport` for a given `Language`.
    pub fn get(&self, lang: Language) -> Option<Arc<dyn LanguageSupport>> {
        self.impls.iter().find(|i| i.language() == lang).cloned()
    }

    /// All registered languages.
    pub fn languages(&self) -> Vec<Language> {
        self.impls.iter().map(|i| i.language()).collect()
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect language from a file path's extension.
pub fn detect_language(path: &Path) -> Option<Language> {
    let ext = path.extension()?.to_str()?;
    for lang in ALL_LANGUAGES {
        if lang.extensions().contains(&ext) {
            return Some(*lang);
        }
    }
    None
}

/// Helper: get node text from source bytes.
pub(crate) fn node_text<'a>(node: &tree_sitter::Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Helper: compute line range (0-indexed) for a node.
pub(crate) fn node_line_range(node: &tree_sitter::Node) -> std::ops::Range<usize> {
    node.start_position().row..node.end_position().row + 1
}

/// Helper: extract preceding line comments (// or #) as doc comments.
pub(crate) fn extract_preceding_comments(
    node: &tree_sitter::Node,
    source: &[u8],
    comment_prefix: &str,
) -> Option<String> {
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        let kind = sib.kind();
        if kind == "comment" || kind == "line_comment" || kind == "block_comment" {
            let text = node_text(&sib, source).trim().to_string();
            comments.push(text);
            sibling = sib.prev_sibling();
        } else {
            break;
        }
    }
    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    let cleaned: Vec<String> = comments
        .iter()
        .map(|c| {
            c.trim_start_matches(comment_prefix)
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim()
                .to_string()
        })
        .collect();
    Some(cleaned.join("\n"))
}

/// Helper: find the name child node in an AST node.
pub(crate) fn find_name_node<'a>(node: &'a tree_sitter::Node<'a>, source: &[u8]) -> Option<String> {
    // Try common name field patterns
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(&name_node, source).to_string());
    }
    // Fallback: look for an identifier child
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_known_languages() {
        assert_eq!(detect_language(Path::new("foo.rs")), Some(Language::Rust));
        assert_eq!(detect_language(Path::new("bar.py")), Some(Language::Python));
        assert_eq!(
            detect_language(Path::new("baz.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(detect_language(Path::new("qux.tsx")), Some(Language::Tsx));
        assert_eq!(detect_language(Path::new("main.go")), Some(Language::Go));
        assert_eq!(detect_language(Path::new("App.java")), Some(Language::Java));
        assert_eq!(detect_language(Path::new("lib.c")), Some(Language::C));
        assert_eq!(detect_language(Path::new("lib.cpp")), Some(Language::Cpp));
        assert_eq!(
            detect_language(Path::new("Program.cs")),
            Some(Language::CSharp)
        );
        assert_eq!(detect_language(Path::new("app.rb")), Some(Language::Ruby));
        assert_eq!(
            detect_language(Path::new("Main.swift")),
            Some(Language::Swift)
        );
        assert_eq!(detect_language(Path::new("Foo.kt")), Some(Language::Kotlin));
        assert_eq!(
            detect_language(Path::new("Bar.scala")),
            Some(Language::Scala)
        );
    }

    #[test]
    fn detect_unknown_extension() {
        assert_eq!(detect_language(Path::new("foo.xyz")), None);
        assert_eq!(detect_language(Path::new("no_ext")), None);
    }

    #[test]
    fn registry_has_all_languages() {
        let registry = LanguageRegistry::new();
        let langs = registry.languages();
        assert_eq!(langs.len(), ALL_LANGUAGES.len());
        for lang in ALL_LANGUAGES {
            assert!(registry.get(*lang).is_some(), "Missing {:?}", lang);
        }
    }
}
