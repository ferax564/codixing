use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::chunker::Chunker;
use crate::chunker::cast::CastChunker;
use crate::config::IndexConfig;
use crate::error::{CodeforgeError, Result};
use crate::graph::CodeGraph;
use crate::graph::extract::{extract_definitions, extract_references};
use crate::graph::persistence::{load_graph, save_graph};
use crate::index::TantivyIndex;
use crate::language::{Language, SemanticEntity, detect_language};
use crate::parser::Parser;
use crate::persistence::{IndexMeta, IndexStore};
use crate::retriever::bm25::BM25Retriever;
use crate::retriever::{Retriever, SearchQuery, SearchResult};
use crate::symbols::persistence::{deserialize_symbols, serialize_symbols};
use crate::symbols::{Symbol, SymbolTable};

/// Summary statistics about the index.
#[derive(Debug, Clone)]
pub struct IndexStats {
    /// Number of files indexed.
    pub file_count: usize,
    /// Number of code chunks produced.
    pub chunk_count: usize,
    /// Number of unique symbol names.
    pub symbol_count: usize,
}

/// Top-level facade that wires together parsing, chunking, indexing,
/// and retrieval into a single coherent API.
pub struct Engine {
    config: IndexConfig,
    store: IndexStore,
    parser: Parser,
    tantivy: TantivyIndex,
    symbols: SymbolTable,
    /// Per-file chunk counts, used for stats.
    file_chunk_counts: HashMap<String, usize>,
    /// Lazily-built code graph.
    graph: Option<CodeGraph>,
}

impl Engine {
    /// Initialize a new index for the project at `root`.
    ///
    /// Walks the directory tree, parses all supported source files in parallel
    /// using rayon, chunks them with the cAST algorithm, indexes chunks in
    /// Tantivy, and populates the symbol table. All state is persisted to the
    /// `.codeforge/` directory.
    pub fn init(root: impl AsRef<Path>, config: IndexConfig) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodeforgeError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::init(&root, &config)?;
        let tantivy = TantivyIndex::create_in_dir(&store.tantivy_dir())?;
        let parser = Parser::new();
        let symbols = SymbolTable::new();

        let files = walk_source_files(&root, &config)?;
        info!(file_count = files.len(), "discovered source files");

        let chunk_count = AtomicUsize::new(0);
        let file_chunk_map = dashmap::DashMap::<String, usize>::new();

        let ctx = IndexContext {
            root: &root,
            config: &config,
            parser: &parser,
            tantivy: &tantivy,
            symbols: &symbols,
            chunk_count: &chunk_count,
            file_chunk_map: &file_chunk_map,
        };

        // Process files in parallel: parse → chunk → index.
        files.par_iter().for_each(|path| {
            if let Err(e) = process_file(path, &ctx) {
                warn!(path = %path.display(), error = %e, "skipping file");
            }
        });

        tantivy.commit()?;

        let total_chunks = chunk_count.load(Ordering::Relaxed);
        let total_symbols = symbols.len();

        // Convert DashMap to HashMap.
        let file_chunk_counts: HashMap<String, usize> = file_chunk_map.into_iter().collect();

        // Persist everything.
        let sym_bytes = serialize_symbols(&symbols)?;
        store.save_symbols_bytes(&sym_bytes)?;

        let hashes: Vec<(PathBuf, u64)> = parser.cache().content_hashes().into_iter().collect();
        store.save_tree_hashes(&hashes)?;

        let meta = IndexMeta {
            version: "0.1.0".to_string(),
            file_count: files.len(),
            chunk_count: total_chunks,
            symbol_count: total_symbols,
            last_indexed: unix_timestamp_string(),
        };
        store.save_meta(&meta)?;

        info!(
            files = files.len(),
            chunks = total_chunks,
            symbols = total_symbols,
            "index initialized"
        );

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            graph: None,
        })
    }

    /// Open an existing index from the `.codeforge/` directory.
    ///
    /// Restores the Tantivy index, symbol table, and tree hashes from disk.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| CodeforgeError::Config(format!("cannot resolve root path: {e}")))?;

        let store = IndexStore::open(&root)?;
        let config = store.load_config()?;
        let tantivy = TantivyIndex::open_in_dir(&store.tantivy_dir())?;

        // Restore symbols.
        let symbols = if store.symbols_path().exists() {
            let bytes = store.load_symbols_bytes()?;
            deserialize_symbols(&bytes)?
        } else {
            SymbolTable::new()
        };

        let parser = Parser::new();
        let meta = store.load_meta()?;
        let file_chunk_counts = HashMap::new(); // Not persisted; rebuilt on reindex.

        // Load persisted graph if available.
        let graph = if store.graph_path().exists() {
            match load_graph(&store.graph_path()) {
                Ok(g) => {
                    info!(
                        nodes = g.node_count(),
                        edges = g.edge_count(),
                        "loaded code graph"
                    );
                    Some(g)
                }
                Err(e) => {
                    warn!(error = %e, "failed to load code graph, skipping");
                    None
                }
            }
        } else {
            None
        };

        info!(
            files = meta.file_count,
            chunks = meta.chunk_count,
            symbols = meta.symbol_count,
            "index opened"
        );

        Ok(Self {
            config,
            store,
            parser,
            tantivy,
            symbols,
            file_chunk_counts,
            graph,
        })
    }

    /// Search the index using BM25 ranking.
    pub fn search(&self, query: SearchQuery) -> Result<Vec<SearchResult>> {
        let retriever = BM25Retriever::new(&self.tantivy);
        retriever.search(&query)
    }

    /// Query the symbol table.
    ///
    /// Performs case-insensitive substring matching on symbol names.
    /// If `file` is provided, also filters by file path.
    pub fn symbols(&self, filter: &str, file: Option<&str>) -> Result<Vec<Symbol>> {
        Ok(self.symbols.filter(filter, file))
    }

    /// Access the code graph, if it has been built.
    pub fn graph(&self) -> Option<&CodeGraph> {
        self.graph.as_ref()
    }

    /// Build a code graph from all indexed files.
    ///
    /// Walks the project source files, extracts definitions and references
    /// using tree-sitter, creates graph nodes for each definition, and
    /// resolves references to definition nodes by name matching.
    pub fn build_graph(&mut self) -> Result<&CodeGraph> {
        let root = &self.config.root.clone();
        let files = walk_source_files(root, &self.config)?;

        let mut graph = CodeGraph::new();

        // First pass: extract all definitions and add them as nodes.
        // We collect (name -> NodeIndex) for reference resolution.
        let mut name_to_nodes: HashMap<String, Vec<petgraph::graph::NodeIndex>> = HashMap::new();

        for path in &files {
            let source = match fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let lang = match detect_language(path) {
                Some(l) => l,
                None => continue,
            };

            let rel_path = path.strip_prefix(root).unwrap_or(path);
            let rel_str = normalize_path(rel_path);

            let defs = extract_definitions(&source, &rel_str, &lang);
            for def in &defs {
                let idx =
                    graph.add_symbol_with_line(&def.name, &def.file, def.kind.clone(), def.line);
                name_to_nodes.entry(def.name.clone()).or_default().push(idx);
            }
        }

        // Second pass: extract references and resolve to definition nodes.
        for path in &files {
            let source = match fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let lang = match detect_language(path) {
                Some(l) => l,
                None => continue,
            };

            let rel_path = path.strip_prefix(root).unwrap_or(path);
            let rel_str = normalize_path(rel_path);

            let defs = extract_definitions(&source, &rel_str, &lang);
            let refs = extract_references(&source, &rel_str, &lang);

            // For each reference, find the definition node that contains this
            // reference (by line range) and the target definition node (by name).
            for reference in &refs {
                // Find the source node: the definition in this file whose range
                // contains the reference line.
                let source_node = find_enclosing_definition(&defs, reference.line, &name_to_nodes);

                // Find target nodes by name.
                let target_name = &reference.target_name;
                let target_nodes = name_to_nodes.get(target_name);

                if let (Some(src_idx), Some(targets)) = (source_node, target_nodes) {
                    for &tgt_idx in targets {
                        // Avoid self-edges.
                        if src_idx != tgt_idx {
                            graph.add_reference(src_idx, tgt_idx, reference.kind.clone());
                        }
                    }
                }
            }
        }

        info!(
            nodes = graph.node_count(),
            edges = graph.edge_count(),
            "built code graph"
        );

        self.graph = Some(graph);
        Ok(self.graph.as_ref().unwrap())
    }

    /// Re-index a single file (after modification).
    ///
    /// Removes old data, re-parses, re-chunks, and re-indexes.
    pub fn reindex_file(&mut self, path: &Path) -> Result<()> {
        let abs_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.config.root.join(path)
        };

        let rel_path = abs_path.strip_prefix(&self.config.root).unwrap_or(path);
        let rel_str = normalize_path(rel_path);

        // Remove old data.
        self.tantivy.remove_file(&rel_str)?;
        self.symbols.remove_file(&rel_str);

        // Read and re-process.
        let source = fs::read(&abs_path)?;
        let result = self.parser.parse_file(&abs_path, &source)?;
        let chunker = CastChunker;
        let chunks = chunker.chunk(
            &rel_str,
            &source,
            &result.tree,
            result.language,
            &self.config.chunk,
        );

        for chunk in &chunks {
            self.tantivy.add_chunk(chunk)?;
        }

        for entity in &result.entities {
            self.symbols
                .insert(symbol_from_entity(entity, &rel_str, result.language));
        }

        self.tantivy.commit()?;
        self.file_chunk_counts.insert(rel_str, chunks.len());

        debug!(path = %abs_path.display(), chunks = chunks.len(), "reindexed file");
        Ok(())
    }

    /// Remove a file from the index entirely.
    pub fn remove_file(&mut self, path: &Path) -> Result<()> {
        let rel_path = path.strip_prefix(&self.config.root).unwrap_or(path);
        let rel_str = normalize_path(rel_path);

        self.tantivy.remove_file(&rel_str)?;
        self.tantivy.commit()?;
        self.symbols.remove_file(&rel_str);
        self.parser.invalidate(path);
        self.file_chunk_counts.remove(&rel_str);

        debug!(path = %path.display(), "removed file from index");
        Ok(())
    }

    /// Return summary statistics about the current index.
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            file_count: self.file_chunk_counts.len(),
            chunk_count: self.file_chunk_counts.values().sum(),
            symbol_count: self.symbols.len(),
        }
    }

    /// Access the underlying index configuration.
    pub fn config(&self) -> &IndexConfig {
        &self.config
    }

    /// Access the underlying symbol table.
    pub fn symbol_table(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Start watching the project directory for file changes.
    ///
    /// Returns a [`FileWatcher`] that can be polled for batches of changes.
    /// Call [`Self::apply_changes`] with the resulting batch to update the index.
    pub fn watch(&self) -> Result<crate::watcher::FileWatcher> {
        crate::watcher::FileWatcher::new(&self.config.root, &self.config)
    }

    /// Apply a batch of file changes to the index.
    ///
    /// For each modified file, re-parses and re-indexes it. For each removed
    /// file, removes it from the index.
    pub fn apply_changes(&mut self, changes: &[crate::watcher::FileChange]) -> Result<()> {
        use crate::watcher::ChangeKind;

        for change in changes {
            match change.kind {
                ChangeKind::Modified => {
                    if let Err(e) = self.reindex_file(&change.path) {
                        warn!(path = %change.path.display(), error = %e, "failed to reindex");
                    }
                }
                ChangeKind::Removed => {
                    if let Err(e) = self.remove_file(&change.path) {
                        warn!(path = %change.path.display(), error = %e, "failed to remove");
                    }
                }
            }
        }

        Ok(())
    }

    /// Persist current state to disk.
    pub fn save(&self) -> Result<()> {
        let sym_bytes = serialize_symbols(&self.symbols)?;
        self.store.save_symbols_bytes(&sym_bytes)?;

        let hashes: Vec<(PathBuf, u64)> =
            self.parser.cache().content_hashes().into_iter().collect();
        self.store.save_tree_hashes(&hashes)?;

        let stats = self.stats();
        let meta = IndexMeta {
            version: "0.1.0".to_string(),
            file_count: stats.file_count,
            chunk_count: stats.chunk_count,
            symbol_count: stats.symbol_count,
            last_indexed: unix_timestamp_string(),
        };
        self.store.save_meta(&meta)?;

        // Persist code graph if built.
        if let Some(ref graph) = self.graph {
            save_graph(graph, &self.store.graph_path())?;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the definition that encloses a given line number.
///
/// Returns the `NodeIndex` of the closest definition whose line is at or before
/// the reference line. This is a heuristic: we pick the last definition that
/// starts before or at the reference line.
fn find_enclosing_definition(
    defs: &[crate::graph::extract::DefinitionInfo],
    ref_line: usize,
    name_to_nodes: &HashMap<String, Vec<petgraph::graph::NodeIndex>>,
) -> Option<petgraph::graph::NodeIndex> {
    // Find the last definition whose line <= ref_line.
    let mut best: Option<&crate::graph::extract::DefinitionInfo> = None;
    for def in defs {
        if def.line <= ref_line {
            match best {
                None => best = Some(def),
                Some(b) if def.line >= b.line => best = Some(def),
                _ => {}
            }
        }
    }

    let def = best?;
    let nodes = name_to_nodes.get(&def.name)?;
    // Return the first node that matches (from this file).
    nodes.first().copied()
}

/// Shared context passed to `process_file` to avoid too-many-arguments.
struct IndexContext<'a> {
    root: &'a Path,
    config: &'a IndexConfig,
    parser: &'a Parser,
    tantivy: &'a TantivyIndex,
    symbols: &'a SymbolTable,
    chunk_count: &'a AtomicUsize,
    file_chunk_map: &'a dashmap::DashMap<String, usize>,
}

/// Process a single file: parse → chunk → index → extract symbols.
fn process_file(path: &Path, ctx: &IndexContext<'_>) -> Result<()> {
    let source = fs::read(path)?;
    let result = ctx.parser.parse_file(path, &source)?;

    let rel_path = path.strip_prefix(ctx.root).unwrap_or(path);
    let rel_str = normalize_path(rel_path);

    let chunker = CastChunker;
    let chunks = chunker.chunk(
        &rel_str,
        &source,
        &result.tree,
        result.language,
        &ctx.config.chunk,
    );

    ctx.chunk_count.fetch_add(chunks.len(), Ordering::Relaxed);
    ctx.file_chunk_map.insert(rel_str.clone(), chunks.len());

    for chunk in &chunks {
        ctx.tantivy.add_chunk(chunk)?;
    }

    for entity in &result.entities {
        ctx.symbols
            .insert(symbol_from_entity(entity, &rel_str, result.language));
    }

    debug!(
        path = %rel_str,
        language = result.language.name(),
        chunks = chunks.len(),
        entities = result.entities.len(),
        "indexed file"
    );

    Ok(())
}

/// Walk the directory tree and collect all source files with supported extensions.
fn walk_source_files(root: &Path, config: &IndexConfig) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_dir_recursive(root, config, &mut files)?;
    Ok(files)
}

/// Recursive directory walker that respects exclude patterns.
fn walk_dir_recursive(dir: &Path, config: &IndexConfig, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Skip excluded directories/files.
        if config.exclude_patterns.iter().any(|p| p == name.as_ref()) {
            continue;
        }

        if path.is_dir() {
            walk_dir_recursive(&path, config, files)?;
        } else if path.is_file() {
            // Only include files with supported language extensions.
            if detect_language(&path).is_some() {
                // If specific languages are configured, filter by them.
                if !config.languages.is_empty() {
                    if let Some(lang) = detect_language(&path) {
                        if config.languages.contains(&lang.name().to_lowercase()) {
                            files.push(path);
                        }
                    }
                } else {
                    files.push(path);
                }
            }
        }
    }

    Ok(())
}

/// Convert a `SemanticEntity` to a `Symbol`.
fn symbol_from_entity(entity: &SemanticEntity, file_path: &str, language: Language) -> Symbol {
    Symbol {
        name: entity.name.clone(),
        kind: entity.kind.clone(),
        language,
        file_path: file_path.to_string(),
        line_start: entity.line_range.start,
        line_end: entity.line_range.end,
        byte_start: entity.byte_range.start,
        byte_end: entity.byte_range.end,
        signature: entity.signature.clone(),
        scope: entity.scope.clone(),
    }
}

/// Normalize a path to a forward-slash string for consistent cross-platform storage.
fn normalize_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Simple unix timestamp as a human-readable string.
fn unix_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Create a temporary project with some source files.
    fn setup_project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();

        // Create a simple Rust file.
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            r#"
/// Entry point.
fn main() {
    println!("Hello, world!");
}

/// Add two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub struct Config {
    pub verbose: bool,
    pub threads: usize,
}
"#,
        )
        .unwrap();

        fs::write(
            src_dir.join("lib.rs"),
            r#"
/// A helper function.
pub fn helper() -> String {
    "help".to_string()
}

pub trait Processor {
    fn process(&self, input: &str) -> String;
}
"#,
        )
        .unwrap();

        (dir, root)
    }

    #[test]
    fn init_indexes_project() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let engine = Engine::init(&root, config).unwrap();
        let stats = engine.stats();

        assert_eq!(stats.file_count, 2, "expected 2 source files");
        assert!(stats.chunk_count > 0, "expected at least 1 chunk");
        assert!(stats.symbol_count > 0, "expected at least 1 symbol");
    }

    #[test]
    fn search_finds_function() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let engine = Engine::init(&root, config).unwrap();

        let results = engine
            .search(SearchQuery::new("add").with_limit(5))
            .unwrap();
        assert!(!results.is_empty(), "expected search results for 'add'");

        // At least one result should be from main.rs.
        assert!(
            results.iter().any(|r| r.file_path.contains("main.rs")),
            "expected result from main.rs"
        );
    }

    #[test]
    fn symbols_returns_matching() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let engine = Engine::init(&root, config).unwrap();

        let syms = engine.symbols("Config", None).unwrap();
        assert!(
            !syms.is_empty(),
            "expected at least 1 symbol matching 'Config'"
        );
        assert!(syms.iter().any(|s| s.name == "Config"));
    }

    #[test]
    fn open_restores_index() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        // Init and drop.
        {
            let engine = Engine::init(&root, config).unwrap();
            let stats = engine.stats();
            assert!(stats.chunk_count > 0);
        }

        // Re-open.
        let engine = Engine::open(&root).unwrap();
        let results = engine
            .search(SearchQuery::new("helper").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected search results after re-opening index"
        );
    }

    #[test]
    fn reindex_file_updates_index() {
        let (_dir, root) = setup_project();
        let config = IndexConfig::new(&root);

        let mut engine = Engine::init(&root, config).unwrap();

        // Modify main.rs.
        fs::write(
            root.join("src/main.rs"),
            r#"
/// New entry point.
fn main() {
    println!("Modified!");
}

pub fn unique_new_function() -> bool {
    true
}
"#,
        )
        .unwrap();

        engine.reindex_file(Path::new("src/main.rs")).unwrap();

        let results = engine
            .search(SearchQuery::new("unique_new_function").with_limit(5))
            .unwrap();
        assert!(
            !results.is_empty(),
            "expected to find newly added function after reindex"
        );
    }

    #[test]
    fn engine_builds_graph_from_indexed_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();

        // Graph should be None before build.
        assert!(engine.graph().is_none());

        engine.build_graph().unwrap();
        let graph = engine.graph().unwrap();
        // Should find at least main and greet as definitions.
        assert!(
            graph.node_count() >= 2,
            "expected at least 2 nodes, got {}",
            graph.node_count()
        );
    }

    #[test]
    fn engine_build_graph_creates_edges() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();
        engine.build_graph().unwrap();
        let graph = engine.graph().unwrap();

        // main calls greet, so there should be at least 1 edge.
        assert!(
            graph.edge_count() >= 1,
            "expected at least 1 edge (main->greet), got {}",
            graph.edge_count()
        );
    }

    #[test]
    fn graph_persistence_round_trip() {
        use crate::graph::persistence::{load_graph, save_graph};

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        let config = IndexConfig::new(&root);
        let mut engine = Engine::init(&root, config).unwrap();
        engine.build_graph().unwrap();

        let graph = engine.graph().unwrap();
        let original_nodes = graph.node_count();
        let original_edges = graph.edge_count();

        // Save and load.
        let graph_path = dir.path().join("graph.json");
        save_graph(graph, &graph_path).unwrap();
        let loaded = load_graph(&graph_path).unwrap();

        assert_eq!(loaded.node_count(), original_nodes);
        assert_eq!(loaded.edge_count(), original_edges);
    }

    #[test]
    fn open_loads_persisted_graph() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        fs::write(
            src_dir.join("main.rs"),
            "fn main() { greet(); }\nfn greet() {}\n",
        )
        .unwrap();

        // Init, build graph, save.
        {
            let config = IndexConfig::new(&root);
            let mut engine = Engine::init(&root, config).unwrap();
            engine.build_graph().unwrap();
            engine.save().unwrap();
        }

        // Re-open — should load graph.
        let engine = Engine::open(&root).unwrap();
        let graph = engine.graph();
        assert!(graph.is_some(), "expected graph to be loaded on open");
        assert!(
            graph.unwrap().node_count() >= 2,
            "expected at least 2 nodes after loading"
        );
    }
}
