use std::path::{Path, PathBuf};

use dashmap::DashMap;

use crate::language::SemanticEntity;

/// A cached parse result for a single file.
pub struct CachedTree {
    pub tree: tree_sitter::Tree,
    pub content_hash: u64,
    pub entities: Vec<SemanticEntity>,
}

/// Thread-safe cache mapping file paths to their most recent parse results.
///
/// Uses `DashMap` for lock-free concurrent reads and fine-grained locking on writes.
/// Cache entries are keyed by file path and validated by content hash — if the
/// source bytes change, the stale entry is treated as a miss.
pub struct TreeCache {
    cache: DashMap<PathBuf, CachedTree>,
}

impl TreeCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    /// Look up a cached tree for `path` whose content hash matches `content_hash`.
    ///
    /// Returns a cloned tree and entity list on hit, or `None` on miss (including
    /// when the stored hash differs from `content_hash`).
    pub fn get(
        &self,
        path: &Path,
        content_hash: u64,
    ) -> Option<(tree_sitter::Tree, Vec<SemanticEntity>)> {
        let entry = self.cache.get(path)?;
        if entry.content_hash == content_hash {
            Some((entry.tree.clone(), entry.entities.clone()))
        } else {
            None
        }
    }

    /// Insert (or replace) a cached entry for the given path.
    pub fn insert(
        &self,
        path: PathBuf,
        tree: tree_sitter::Tree,
        content_hash: u64,
        entities: Vec<SemanticEntity>,
    ) {
        self.cache.insert(
            path,
            CachedTree {
                tree,
                content_hash,
                entities,
            },
        );
    }

    /// Remove the cached entry for `path`, if any.
    pub fn remove(&self, path: &Path) {
        self.cache.remove(path);
    }

    /// Number of entries currently cached.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Snapshot of all cached paths and their content hashes.
    pub fn content_hashes(&self) -> Vec<(PathBuf, u64)> {
        self.cache
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().content_hash))
            .collect()
    }
}

impl Default for TreeCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Helper: parse a tiny Rust snippet and return its tree.
    fn parse_tree(source: &[u8]) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn insert_and_get_hit() {
        let cache = TreeCache::new();
        let source = b"fn hello() {}";
        let tree = parse_tree(source);
        let hash = 42u64;
        let entities = vec![];

        cache.insert(PathBuf::from("a.rs"), tree, hash, entities);
        assert_eq!(cache.len(), 1);

        let result = cache.get(Path::new("a.rs"), hash);
        assert!(result.is_some());
    }

    #[test]
    fn get_miss_on_wrong_hash() {
        let cache = TreeCache::new();
        let source = b"fn hello() {}";
        let tree = parse_tree(source);

        cache.insert(PathBuf::from("a.rs"), tree, 1, vec![]);
        assert!(cache.get(Path::new("a.rs"), 999).is_none());
    }

    #[test]
    fn remove_entry() {
        let cache = TreeCache::new();
        let source = b"fn hello() {}";
        let tree = parse_tree(source);

        cache.insert(PathBuf::from("a.rs"), tree, 1, vec![]);
        assert!(!cache.is_empty());

        cache.remove(Path::new("a.rs"));
        assert!(cache.is_empty());
    }

    #[test]
    fn content_hashes_snapshot() {
        let cache = TreeCache::new();

        cache.insert(PathBuf::from("a.rs"), parse_tree(b"fn a() {}"), 10, vec![]);
        cache.insert(PathBuf::from("b.rs"), parse_tree(b"fn b() {}"), 20, vec![]);

        let hashes = cache.content_hashes();
        assert_eq!(hashes.len(), 2);

        let a_hash = hashes
            .iter()
            .find(|(p, _)| p == Path::new("a.rs"))
            .map(|(_, h)| *h);
        assert_eq!(a_hash, Some(10));
    }
}
