pub mod bash;
pub mod c;
pub mod cpp;
pub mod csharp;
pub mod doc;
pub mod dockerfile;
pub mod go;
pub mod java;
pub mod kotlin;
pub mod makefile;
pub mod markdown;
pub mod matlab;
pub mod mermaid;
pub mod php;
pub mod python;
pub mod ruby;
pub mod rust;
pub mod scala;
pub mod swift;
pub mod toml_lang;
pub mod typescript;
pub mod xml;
pub mod yaml;
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
    Bash,
    Matlab,
    // Config languages (line-based, no tree-sitter)
    Yaml,
    Toml,
    Dockerfile,
    Makefile,
    // Diagram / markup config (line-based, no tree-sitter)
    Mermaid,
    Xml,
    // Doc languages (structured parsing, no tree-sitter)
    Markdown,
    Html,
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
            Self::Bash => "Bash",
            Self::Matlab => "Matlab",
            Self::Yaml => "YAML",
            Self::Toml => "TOML",
            Self::Dockerfile => "Dockerfile",
            Self::Makefile => "Makefile",
            Self::Mermaid => "Mermaid",
            Self::Xml => "XML",
            Self::Markdown => "Markdown",
            Self::Html => "HTML",
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
            Self::Bash => &["sh", "bash", "zsh", "bats"],
            Self::Matlab => &["m"],
            Self::Yaml => &["yaml", "yml"],
            Self::Toml => &["toml"],
            Self::Dockerfile => &["dockerfile"],
            Self::Makefile => &["mk"],
            Self::Mermaid => &["mmd", "mermaid"],
            Self::Xml => &["xml", "drawio"],
            Self::Markdown => &["md", "mdx"],
            Self::Html => &["html", "htm"],
        }
    }

    /// Whether this language uses tree-sitter for parsing.
    ///
    /// Config languages (YAML, TOML, Dockerfile, Makefile) use line-based
    /// parsing instead.
    pub fn is_tree_sitter(self) -> bool {
        !matches!(
            self,
            Self::Yaml
                | Self::Toml
                | Self::Dockerfile
                | Self::Makefile
                | Self::Mermaid
                | Self::Xml
                | Self::Markdown
                | Self::Html
        )
    }

    /// Whether this language represents a documentation format (Markdown, HTML).
    pub fn is_doc(self) -> bool {
        matches!(self, Self::Markdown | Self::Html)
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
    Language::Bash,
    Language::Matlab,
    Language::Yaml,
    Language::Toml,
    Language::Dockerfile,
    Language::Makefile,
    Language::Mermaid,
    Language::Xml,
    Language::Markdown,
    Language::Html,
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
    /// A configuration key, environment variable, or build variable.
    Variable,
    /// A type/kind identifier in config files (e.g., Kubernetes `kind: Deployment`).
    Type,
}

impl std::fmt::Display for EntityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Interface => "interface",
            Self::Trait => "trait",
            Self::TypeAlias => "type",
            Self::Constant => "const",
            Self::Static => "static",
            Self::Module => "module",
            Self::Import => "import",
            Self::Impl => "impl",
            Self::Namespace => "namespace",
            Self::Variable => "variable",
            Self::Type => "type",
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

/// Lightweight entity extraction for config languages (YAML, TOML, Dockerfile, Makefile).
///
/// These languages use line-based parsing instead of tree-sitter, so they
/// implement a simpler trait that operates on raw source bytes.
pub trait ConfigLanguageSupport: Send + Sync {
    /// Which language this implementation handles.
    fn language(&self) -> Language;

    /// Extract semantic entities from source text using line-based parsing.
    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity>;
}

/// Registry mapping languages to their `LanguageSupport` implementations.
pub struct LanguageRegistry {
    impls: Vec<Arc<dyn LanguageSupport>>,
    config_impls: Vec<Arc<dyn ConfigLanguageSupport>>,
}

impl LanguageRegistry {
    /// Build a registry with all supported languages (Tier 1 + Tier 2 + config).
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
            Arc::new(bash::BashLanguage),
            Arc::new(matlab::MatlabLanguage),
        ];
        let config_impls: Vec<Arc<dyn ConfigLanguageSupport>> = vec![
            Arc::new(yaml::YamlLanguage),
            Arc::new(toml_lang::TomlLanguage),
            Arc::new(dockerfile::DockerfileLanguage),
            Arc::new(makefile::MakefileLanguage),
            Arc::new(mermaid::MermaidLanguage),
            Arc::new(xml::XmlLanguage),
        ];
        Self {
            impls,
            config_impls,
        }
    }

    /// Look up the `LanguageSupport` for a given tree-sitter-backed `Language`.
    pub fn get(&self, lang: Language) -> Option<Arc<dyn LanguageSupport>> {
        self.impls.iter().find(|i| i.language() == lang).cloned()
    }

    /// Look up the `ConfigLanguageSupport` for a given config `Language`.
    pub fn get_config(&self, lang: Language) -> Option<Arc<dyn ConfigLanguageSupport>> {
        self.config_impls
            .iter()
            .find(|i| i.language() == lang)
            .cloned()
    }

    /// All registered languages (both tree-sitter and config).
    pub fn languages(&self) -> Vec<Language> {
        let mut langs: Vec<Language> = self.impls.iter().map(|i| i.language()).collect();
        langs.extend(self.config_impls.iter().map(|i| i.language()));
        langs
    }
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect language from a file path's extension or filename.
///
/// For most languages, detection is extension-based. Dockerfile and Makefile
/// are also detected by their filename (e.g., `Dockerfile`, `Dockerfile.prod`,
/// `Makefile`, `GNUmakefile`).
pub fn detect_language(path: &Path) -> Option<Language> {
    // First, try filename-based detection for config languages that use
    // well-known filenames without (or with unusual) extensions.
    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
        let lower = file_name.to_lowercase();
        // Dockerfile, Dockerfile.prod, Dockerfile.dev, etc.
        if lower == "dockerfile" || lower.starts_with("dockerfile.") {
            return Some(Language::Dockerfile);
        }
        // Makefile, makefile, GNUmakefile
        if lower == "makefile" || lower == "gnumakefile" {
            return Some(Language::Makefile);
        }
    }

    // Extension-based detection.
    let ext = path.extension()?.to_str()?;

    // Disambiguate `.m` files: Objective-C vs MATLAB.
    // If the file exists, peek at the first 512 bytes for ObjC indicators.
    if ext == "m" {
        if let Ok(bytes) = std::fs::read(path) {
            let peek = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
            if peek.contains("#import")
                || peek.contains("@interface")
                || peek.contains("@implementation")
            {
                return None; // Objective-C — not supported
            }
        }
        return Some(Language::Matlab);
    }

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
    fn detect_config_languages() {
        // YAML
        assert_eq!(
            detect_language(Path::new("config.yaml")),
            Some(Language::Yaml)
        );
        assert_eq!(detect_language(Path::new("ci.yml")), Some(Language::Yaml));
        // TOML
        assert_eq!(
            detect_language(Path::new("Cargo.toml")),
            Some(Language::Toml)
        );
        // Dockerfile (filename-based)
        assert_eq!(
            detect_language(Path::new("Dockerfile")),
            Some(Language::Dockerfile)
        );
        assert_eq!(
            detect_language(Path::new("Dockerfile.prod")),
            Some(Language::Dockerfile)
        );
        assert_eq!(
            detect_language(Path::new("app.dockerfile")),
            Some(Language::Dockerfile)
        );
        // Makefile (filename-based)
        assert_eq!(
            detect_language(Path::new("Makefile")),
            Some(Language::Makefile)
        );
        assert_eq!(
            detect_language(Path::new("GNUmakefile")),
            Some(Language::Makefile)
        );
        assert_eq!(
            detect_language(Path::new("rules.mk")),
            Some(Language::Makefile)
        );
    }

    #[test]
    fn detect_unknown_extension() {
        assert_eq!(detect_language(Path::new("foo.xyz")), None);
    }

    #[test]
    fn registry_has_all_languages() {
        let registry = LanguageRegistry::new();
        let langs = registry.languages();
        // TODO: restore assert_eq!(langs.len(), ALL_LANGUAGES.len()) after doc impls added
        assert_eq!(langs.len(), ALL_LANGUAGES.len() - 2);
        for lang in ALL_LANGUAGES {
            if lang.is_doc() {
                // Doc languages have no registry impl yet
                continue;
            }
            if lang.is_tree_sitter() {
                assert!(
                    registry.get(*lang).is_some(),
                    "Missing tree-sitter {:?}",
                    lang
                );
            } else {
                assert!(
                    registry.get_config(*lang).is_some(),
                    "Missing config {:?}",
                    lang
                );
            }
        }
    }

    #[test]
    fn detect_markdown_language() {
        assert_eq!(
            detect_language(Path::new("README.md")),
            Some(Language::Markdown)
        );
        assert_eq!(
            detect_language(Path::new("docs/guide.mdx")),
            Some(Language::Markdown)
        );
    }

    #[test]
    fn detect_html_language() {
        assert_eq!(
            detect_language(Path::new("docs/index.html")),
            Some(Language::Html)
        );
        assert_eq!(detect_language(Path::new("api.htm")), Some(Language::Html));
    }

    #[test]
    fn markdown_is_doc() {
        assert!(Language::Markdown.is_doc());
        assert!(Language::Html.is_doc());
        assert!(!Language::Rust.is_doc());
        assert!(!Language::Yaml.is_doc());
    }

    #[test]
    fn doc_languages_are_not_tree_sitter() {
        assert!(!Language::Markdown.is_tree_sitter());
        assert!(!Language::Html.is_tree_sitter());
    }
}
