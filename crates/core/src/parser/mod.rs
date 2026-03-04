pub mod tree_cache;

use std::path::Path;

use tracing::debug;
use xxhash_rust::xxh3::xxh3_64;

use crate::error::{CodeforgeError, Result};
use crate::language::{Language, LanguageRegistry, SemanticEntity, detect_language};

pub use tree_cache::TreeCache;

/// Result of parsing a single source file.
pub struct ParseResult {
    /// The detected language.
    pub language: Language,
    /// Semantic entities extracted from the AST.
    pub entities: Vec<SemanticEntity>,
    /// The tree-sitter concrete syntax tree.
    pub tree: tree_sitter::Tree,
    /// xxh3-64 hash of the source bytes, used for cache validation.
    pub content_hash: u64,
}

/// Code parser backed by tree-sitter with an incremental tree cache.
///
/// A fresh `tree_sitter::Parser` is created on every call because the type is
/// `!Send + !Sync` and cannot be shared across threads. Parser construction is
/// cheap, so this has negligible overhead even under heavy parallelism via rayon.
pub struct Parser {
    registry: LanguageRegistry,
    cache: TreeCache,
}

impl Parser {
    /// Create a new parser with the default language registry and an empty cache.
    pub fn new() -> Self {
        Self {
            registry: LanguageRegistry::new(),
            cache: TreeCache::new(),
        }
    }

    /// Parse a single file, returning the tree-sitter tree and extracted entities.
    ///
    /// If the cache already holds a result whose content hash matches, the cached
    /// tree and entities are returned without re-parsing.
    pub fn parse_file(&self, path: &Path, source: &[u8]) -> Result<ParseResult> {
        let language =
            detect_language(path).ok_or_else(|| CodeforgeError::UnsupportedLanguage {
                path: path.to_path_buf(),
            })?;

        let content_hash = xxh3_64(source);

        // Fast path: cache hit.
        if let Some((tree, entities)) = self.cache.get(path, content_hash) {
            debug!(path = %path.display(), "parser cache hit");
            return Ok(ParseResult {
                language,
                entities,
                tree,
                content_hash,
            });
        }

        // Slow path: parse and populate cache.
        let result = self.do_parse(path, source, language, content_hash)?;

        self.cache.insert(
            path.to_path_buf(),
            result.tree.clone(),
            content_hash,
            result.entities.clone(),
        );

        Ok(result)
    }

    /// Force a re-parse, bypassing the cache entirely.
    ///
    /// The new result is stored in the cache, replacing any previous entry.
    pub fn parse_file_uncached(&self, path: &Path, source: &[u8]) -> Result<ParseResult> {
        let language =
            detect_language(path).ok_or_else(|| CodeforgeError::UnsupportedLanguage {
                path: path.to_path_buf(),
            })?;

        let content_hash = xxh3_64(source);
        let result = self.do_parse(path, source, language, content_hash)?;

        self.cache.insert(
            path.to_path_buf(),
            result.tree.clone(),
            content_hash,
            result.entities.clone(),
        );

        Ok(result)
    }

    /// Remove a file from the cache.
    pub fn invalidate(&self, path: &Path) {
        self.cache.remove(path);
    }

    /// Access the underlying tree cache.
    pub fn cache(&self) -> &TreeCache {
        &self.cache
    }

    /// Access the language registry.
    pub fn registry(&self) -> &LanguageRegistry {
        &self.registry
    }

    /// Number of files currently held in the tree cache.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    /// Perform the actual tree-sitter parse and entity extraction.
    fn do_parse(
        &self,
        path: &Path,
        source: &[u8],
        language: Language,
        content_hash: u64,
    ) -> Result<ParseResult> {
        let lang_support =
            self.registry
                .get(language)
                .ok_or_else(|| CodeforgeError::UnsupportedLanguage {
                    path: path.to_path_buf(),
                })?;

        // Create a fresh parser — tree_sitter::Parser is !Send, so we cannot
        // keep one across threads. Construction is cheap (~microseconds).
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&lang_support.tree_sitter_language())
            .map_err(|e| CodeforgeError::Parse {
                path: path.to_path_buf(),
                message: format!("failed to set language: {e}"),
            })?;

        let tree = ts_parser
            .parse(source, None)
            .ok_or_else(|| CodeforgeError::Parse {
                path: path.to_path_buf(),
                message: "tree-sitter returned no tree".to_string(),
            })?;

        let entities = lang_support.extract_entities(&tree, source);

        debug!(
            path = %path.display(),
            language = language.name(),
            entities = entities.len(),
            "parsed file"
        );

        Ok(ParseResult {
            language,
            entities,
            tree,
            content_hash,
        })
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::language::EntityKind;

    const RUST_SOURCE: &str = r#"
/// Greet someone by name.
pub fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}

pub struct Config {
    pub verbose: bool,
}
"#;

    #[test]
    fn parse_rust_file_extracts_entities() {
        let parser = Parser::new();
        let path = Path::new("example.rs");
        let result = parser.parse_file(path, RUST_SOURCE.as_bytes()).unwrap();

        assert_eq!(result.language, Language::Rust);
        assert!(!result.entities.is_empty());

        let fns: Vec<_> = result
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].name, "greet");

        let structs: Vec<_> = result
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name, "Config");
    }

    #[test]
    fn cache_hit_on_same_content() {
        let parser = Parser::new();
        let path = Path::new("lib.rs");
        let source = RUST_SOURCE.as_bytes();

        // First call: cold parse.
        let r1 = parser.parse_file(path, source).unwrap();
        assert_eq!(parser.cache_len(), 1);

        // Second call: should hit the cache (same content hash).
        let r2 = parser.parse_file(path, source).unwrap();
        assert_eq!(parser.cache_len(), 1);

        assert_eq!(r1.content_hash, r2.content_hash);
        assert_eq!(r1.entities.len(), r2.entities.len());
    }

    #[test]
    fn cache_miss_on_different_content() {
        let parser = Parser::new();
        let path = Path::new("lib.rs");

        let source_v1 = b"fn v1() {}";
        let source_v2 = b"fn v2() {} fn v2b() {}";

        let r1 = parser.parse_file(path, source_v1).unwrap();
        assert_eq!(parser.cache_len(), 1);

        let r2 = parser.parse_file(path, source_v2).unwrap();
        assert_eq!(parser.cache_len(), 1); // same path, replaced

        // Hashes must differ.
        assert_ne!(r1.content_hash, r2.content_hash);
        // v2 has two functions.
        let fn_count = r2
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .count();
        assert_eq!(fn_count, 2);
    }

    #[test]
    fn invalidate_removes_from_cache() {
        let parser = Parser::new();
        let path = Path::new("temp.rs");

        parser.parse_file(path, b"fn x() {}").unwrap();
        assert_eq!(parser.cache_len(), 1);

        parser.invalidate(path);
        assert_eq!(parser.cache_len(), 0);
    }

    #[test]
    fn unsupported_language_returns_error() {
        let parser = Parser::new();
        let result = parser.parse_file(Path::new("data.xyz"), b"hello");
        assert!(result.is_err());
    }

    #[test]
    fn parse_file_uncached_always_reparses() {
        let parser = Parser::new();
        let path = Path::new("lib.rs");
        let source = RUST_SOURCE.as_bytes();

        // Populate cache.
        parser.parse_file(path, source).unwrap();
        assert_eq!(parser.cache_len(), 1);

        // Uncached parse should still succeed and update the cache entry.
        let result = parser.parse_file_uncached(path, source).unwrap();
        assert_eq!(parser.cache_len(), 1);
        assert!(!result.entities.is_empty());
    }
}
