pub mod community;
pub mod cypher_export;
pub mod extract;
pub mod extractor;
pub mod graphml_export;
pub mod html_export;
pub mod obsidian_export;
pub mod pagerank;
pub mod persistence;
pub mod repomap;
pub mod resolver;
pub mod surprise;
pub mod types;

use std::collections::{HashMap, HashSet};

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};

use crate::language::Language;

// Re-export public types from sub-modules.
pub use community::CommunityResult;
pub use cypher_export::{CypherExportOptions, export_cypher};
pub use extractor::{CallExtractor, ImportExtractor};
pub use graphml_export::{GraphmlExportOptions, export_graphml};
pub use html_export::HtmlExportOptions;
pub use obsidian_export::{ObsidianExportOptions, export_obsidian};
pub use pagerank::{
    compute_pagerank, compute_personalized_pagerank, compute_weighted_personalized_pagerank,
};
pub use repomap::{RepoMapOptions, generate_repo_map};
pub(crate) use resolver::BorrowedImportResolver;
pub use resolver::ImportResolver;
pub use surprise::SurprisingEdge;
pub use types::{ReferenceKind, SymbolKind, SymbolNode};

/// Version of the graph edge-extraction schema.
///
/// Bump this whenever the import extractor or resolver changes which edges it
/// produces (new import forms, resolution fixes). Persisted indexes stamp the
/// version they were built with; on mismatch the next sync auto-rebuilds the
/// graph so fixes reach existing installs without a manual
/// `sync --rebuild-graph`.
///
/// History: 1 = implicit pre-v0.46 (no stamp on disk); 2 = `mod foo;`
/// declarations emit edges + `self::`/`super::` imports anchor to the
/// importing file's module.
pub const GRAPH_SCHEMA_VERSION: u32 = 2;

/// Confidence level of a dependency edge, auto-derived from [`EdgeKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeConfidence {
    /// AST-resolved imports (EdgeKind::Resolved).
    Verified,
    /// Call extraction (EdgeKind::Calls).
    High,
    /// Doc-to-code references (EdgeKind::DocumentedBy).
    Medium,
    /// External/unresolved (EdgeKind::External).
    Low,
}

impl EdgeConfidence {
    /// Return the provenance label for this confidence level.
    pub fn provenance(&self) -> &'static str {
        match self {
            Self::Verified => "EXTRACTED",
            Self::High => "RESOLVED",
            Self::Medium => "INFERRED",
            Self::Low => "EXTERNAL",
        }
    }
}

impl std::fmt::Display for EdgeConfidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.provenance())
    }
}

/// Kind of a dependency edge between files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeKind {
    /// The import resolved to an indexed file in this project.
    Resolved,
    /// The import refers to an external package / stdlib and could not be resolved.
    External,
    /// A function/method call site resolved to a symbol defined in another file.
    /// These edges are extracted from call expressions via [`CallExtractor`] and
    /// complement import edges with fine-grained call-level coupling information.
    Calls,
    /// A documentation file references a symbol defined in a code file.
    DocumentedBy,
}

impl EdgeKind {
    /// Return the default confidence level for this edge kind.
    pub fn default_confidence(&self) -> EdgeConfidence {
        match self {
            EdgeKind::Resolved => EdgeConfidence::Verified,
            EdgeKind::Calls => EdgeConfidence::High,
            EdgeKind::DocumentedBy => EdgeConfidence::Medium,
            EdgeKind::External => EdgeConfidence::Low,
        }
    }
}

/// A node in the dependency graph representing a single source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeNode {
    /// Relative path, forward-slash normalized.
    pub file_path: String,
    /// Detected language.
    pub language: Language,
    /// PageRank score, 0.0 until `apply_pagerank` is called.
    pub pagerank: f32,
    /// Number of outgoing import edges.
    pub out_degree: usize,
    /// Number of incoming import edges.
    pub in_degree: usize,
    /// Community ID assigned by Louvain detection, `None` until detection runs.
    #[serde(default)]
    pub community: Option<usize>,
}

/// An edge in the dependency graph representing an import relationship.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeEdge {
    /// Import string as it appears in the source code.
    pub raw_import: String,
    /// Whether the import resolved to a known file or is external.
    pub kind: EdgeKind,
    /// Confidence level, auto-derived from [`EdgeKind`].
    #[serde(default = "default_edge_confidence")]
    pub confidence: EdgeConfidence,
}

fn default_edge_confidence() -> EdgeConfidence {
    EdgeConfidence::Verified
}

fn edge_kind_code(kind: &EdgeKind) -> u8 {
    match kind {
        EdgeKind::Resolved => 0,
        EdgeKind::External => 1,
        EdgeKind::Calls => 2,
        EdgeKind::DocumentedBy => 3,
    }
}

fn edge_confidence_code(confidence: EdgeConfidence) -> u8 {
    match confidence {
        EdgeConfidence::Verified => 0,
        EdgeConfidence::High => 1,
        EdgeConfidence::Medium => 2,
        EdgeConfidence::Low => 3,
    }
}

fn symbol_kind_code(kind: &types::SymbolKind) -> u8 {
    match kind {
        types::SymbolKind::Function => 0,
        types::SymbolKind::Struct => 1,
        types::SymbolKind::Enum => 2,
        types::SymbolKind::Trait => 3,
        types::SymbolKind::Module => 4,
        types::SymbolKind::Const => 5,
        types::SymbolKind::Type => 6,
    }
}

fn reference_kind_code(kind: &types::ReferenceKind) -> u8 {
    match kind {
        types::ReferenceKind::Call => 0,
        types::ReferenceKind::Import => 1,
        types::ReferenceKind::Inherit => 2,
        types::ReferenceKind::FieldAccess => 3,
        types::ReferenceKind::TypeRef => 4,
    }
}

/// Edge kinds that constitute a true *import-graph* boundary, used by
/// `cross_imports*`. Calls and DocumentedBy edges are intentionally
/// excluded because they are not enforceable architectural boundaries.
pub const DEFAULT_IMPORT_BOUNDARY_KINDS: &[EdgeKind] = &[EdgeKind::Resolved, EdgeKind::External];

/// One matched edge contributing to a `cross_imports_ranked_with_evidence`
/// result: `(target_file, raw_import_text, edge_kind)`.
pub type CrossImportEvidence = (String, String, EdgeKind);

/// One row of `cross_imports_ranked_with_evidence`: source file path,
/// score, and the per-edge evidence list that produced the score.
pub type CrossImportEvidenceRow = (String, f32, Vec<CrossImportEvidence>);

/// Flat, serialization-friendly representation of the graph.
///
/// Used for bitcode persistence — avoids petgraph index fragility across rebuilds.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphData {
    pub nodes: Vec<CodeNode>,
    /// Edges as `(from_path, to_path, edge)` triples.
    pub edges: Vec<(String, String, CodeEdge)>,
}

/// Summary statistics about the dependency graph.
#[derive(Debug, Clone)]
pub struct GraphStats {
    pub node_count: usize,
    pub edge_count: usize,
    pub resolved_edges: usize,
    pub external_edges: usize,
    /// Number of call-site edges added by [`CallExtractor`].
    pub call_edges: usize,
    /// Number of doc-to-code edges added by `add_doc_edges`.
    pub doc_edges: usize,
    /// Number of nodes in the symbol-level graph.
    pub symbol_nodes: usize,
    /// Number of edges in the symbol-level graph.
    pub symbol_edges: usize,
    /// Confidence breakdown: (verified, high, medium, low).
    pub confidence_counts: (usize, usize, usize, usize),
}

/// In-memory dependency graph over source files.
///
/// Wraps a petgraph `DiGraph` with a path→NodeIndex lookup table so callers
/// can work with file paths rather than opaque indices.
///
/// Also contains an optional symbol-level graph (`inner`) that tracks
/// fine-grained symbol→symbol references (calls, type refs, imports).
pub struct CodeGraph {
    graph: DiGraph<CodeNode, CodeEdge>,
    path_to_node: HashMap<String, NodeIndex>,
    /// Distinct real-file edges, excluding unresolved external pseudo-nodes.
    /// This tiny derived index keeps query-time degree lookups exact without
    /// walking a dependency hub's complete raw adjacency.
    real_file_edges: HashSet<(NodeIndex, NodeIndex)>,
    /// Exact distinct real-file in/out degree, indexed by file-graph node ID.
    /// The vector follows petgraph's swap-removal layout.
    real_file_degrees: Vec<RealFileDegree>,
    /// A compact, noise-free sample of distinct real neighbours. Keeping this
    /// beside the raw multigraph means bounded queries never inspect parallel
    /// or unresolved edges at all.
    real_file_neighbors: Vec<RealFileNeighbors>,
    /// Symbol-level directed graph: nodes are [`SymbolNode`]s, edges are
    /// [`ReferenceKind`]s. Used by context assembly and precise callers/callees.
    pub(crate) inner: DiGraph<types::SymbolNode, types::ReferenceKind>,
    /// Exact symbol-node postings by owning file. Incremental updates and
    /// semantic checkpoint comparisons must scale with the changed file, not
    /// with every symbol in the repository.
    symbol_nodes_by_file: HashMap<String, Vec<NodeIndex>>,
    /// Monotonic in-process generation of the file-level graph topology.
    file_revision: u64,
    /// Process-unique identity used by caches that outlive an Engine instance.
    cache_identity: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct RealFileDegree {
    incoming: usize,
    outgoing: usize,
}

#[derive(Debug, Clone, Default)]
struct RealFileNeighbors {
    incoming: Vec<NodeIndex>,
    outgoing: Vec<NodeIndex>,
}

/// The bounded graph expansion is an approximation. Its derived neighbour
/// sample and per-query work remain fixed even for million-edge hubs.
const MAX_BOUNDED_EDGE_SCANS: usize = 4_096;

fn bounded_edge_scan_budget(limit: usize) -> usize {
    limit.min(MAX_BOUNDED_EDGE_SCANS)
}

fn insert_bounded_neighbor(
    graph: &DiGraph<CodeNode, CodeEdge>,
    neighbors: &mut Vec<NodeIndex>,
    candidate: NodeIndex,
) {
    let candidate_path = &graph[candidate].file_path;
    let position = neighbors.binary_search_by(|neighbor| {
        graph[*neighbor]
            .file_path
            .as_str()
            .cmp(candidate_path.as_str())
    });
    let Err(position) = position else {
        return;
    };
    if position >= MAX_BOUNDED_EDGE_SCANS {
        return;
    }
    neighbors.insert(position, candidate);
    if neighbors.len() > MAX_BOUNDED_EDGE_SCANS {
        neighbors.pop();
    }
}

fn remove_bounded_neighbor(neighbors: &mut Vec<NodeIndex>, target: NodeIndex) -> bool {
    let Some(position) = neighbors.iter().position(|neighbor| *neighbor == target) else {
        return false;
    };
    neighbors.remove(position);
    true
}

/// Canonical, index-independent graph state affected by re-indexing one file.
///
/// Petgraph node indices and insertion order are deliberately excluded. This
/// snapshot is used at incremental checkpoint boundaries to prove that a file
/// edit left every persisted graph/semantic input unchanged before retaining
/// the active generation's hard-linked graph-derived sidecars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileGraphSemanticState {
    file_nodes: Vec<FileGraphNodeState>,
    file_edges: Vec<(String, String, String, u8, u8)>,
    symbol_nodes: Vec<SymbolGraphNodeState>,
    symbol_edges: Vec<SymbolGraphEdgeState>,
}

/// One file-owned definition staged for an incremental symbol-graph update.
/// The owning update already carries the file path, so storing it on every
/// definition would duplicate the same allocation across large batches.
#[derive(Debug, Clone)]
pub(crate) struct FileSymbolDefinition {
    pub(crate) name: String,
    pub(crate) kind: types::SymbolKind,
    pub(crate) line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FileGraphNodeState {
    path: String,
    language: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolGraphNodeState {
    name: String,
    file: String,
    kind: u8,
    line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolGraphEdgeState {
    source: SymbolGraphNodeState,
    target: SymbolGraphNodeState,
    kind: u8,
}

static NEXT_GRAPH_CACHE_IDENTITY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

impl CodeGraph {
    /// Create an empty graph.
    pub fn new() -> Self {
        Self {
            graph: DiGraph::new(),
            path_to_node: HashMap::new(),
            real_file_edges: HashSet::new(),
            real_file_degrees: Vec::new(),
            real_file_neighbors: Vec::new(),
            inner: DiGraph::new(),
            symbol_nodes_by_file: HashMap::new(),
            file_revision: 0,
            cache_identity: NEXT_GRAPH_CACHE_IDENTITY
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    fn is_real_file_node(&self, index: NodeIndex) -> bool {
        self.graph
            .node_weight(index)
            .is_some_and(|node| !node.file_path.starts_with("__ext__:"))
    }

    fn record_real_file_edge(&mut self, source: NodeIndex, target: NodeIndex) {
        if (!self.is_real_file_node(source) || !self.is_real_file_node(target))
            || !self.real_file_edges.insert((source, target))
        {
            return;
        }
        self.real_file_degrees[source.index()].outgoing += 1;
        self.real_file_degrees[target.index()].incoming += 1;
        insert_bounded_neighbor(
            &self.graph,
            &mut self.real_file_neighbors[source.index()].outgoing,
            target,
        );
        insert_bounded_neighbor(
            &self.graph,
            &mut self.real_file_neighbors[target.index()].incoming,
            source,
        );
    }

    fn forget_real_file_edge(
        &mut self,
        source: NodeIndex,
        target: NodeIndex,
        refill_source: bool,
        refill_target: bool,
    ) {
        if !self.real_file_edges.remove(&(source, target)) {
            return;
        }
        self.real_file_degrees[source.index()].outgoing = self.real_file_degrees[source.index()]
            .outgoing
            .saturating_sub(1);
        self.real_file_degrees[target.index()].incoming = self.real_file_degrees[target.index()]
            .incoming
            .saturating_sub(1);
        let refill_outgoing = remove_bounded_neighbor(
            &mut self.real_file_neighbors[source.index()].outgoing,
            target,
        );
        let refill_incoming = remove_bounded_neighbor(
            &mut self.real_file_neighbors[target.index()].incoming,
            source,
        );
        if refill_source
            && refill_outgoing
            && self.real_file_degrees[source.index()].outgoing
                > self.real_file_neighbors[source.index()].outgoing.len()
        {
            self.refill_real_file_neighbors(source, petgraph::Direction::Outgoing);
        }
        if refill_target
            && refill_incoming
            && self.real_file_degrees[target.index()].incoming
                > self.real_file_neighbors[target.index()].incoming.len()
        {
            self.refill_real_file_neighbors(target, petgraph::Direction::Incoming);
        }
    }

    /// Refill a sampled adjacency after one of its lexicographically-smallest
    /// members is removed. Mutation may scan the raw adjacency, but retains at
    /// most the fixed cap and query-time work remains strictly bounded.
    fn refill_real_file_neighbors(&mut self, index: NodeIndex, direction: petgraph::Direction) {
        let degree = self.real_file_degree(index);
        let expected = match direction {
            petgraph::Direction::Incoming => degree.incoming,
            petgraph::Direction::Outgoing => degree.outgoing,
        };
        let mut neighbors = Vec::with_capacity(expected.min(MAX_BOUNDED_EDGE_SCANS));
        for neighbor in self.graph.neighbors_directed(index, direction) {
            let edge = match direction {
                petgraph::Direction::Incoming => (neighbor, index),
                petgraph::Direction::Outgoing => (index, neighbor),
            };
            if self.real_file_edges.contains(&edge) {
                insert_bounded_neighbor(&self.graph, &mut neighbors, neighbor);
            }
        }
        match direction {
            petgraph::Direction::Incoming => {
                self.real_file_neighbors[index.index()].incoming = neighbors;
            }
            petgraph::Direction::Outgoing => {
                self.real_file_neighbors[index.index()].outgoing = neighbors;
            }
        }
    }

    fn rebuild_real_file_edge_index(&mut self) {
        let mut real_file_edges = HashSet::new();
        let mut real_file_degrees = vec![RealFileDegree::default(); self.graph.node_count()];
        let mut real_file_neighbors = vec![RealFileNeighbors::default(); self.graph.node_count()];
        for edge in self.graph.edge_references() {
            let source = edge.source();
            let target = edge.target();
            if !self.is_real_file_node(source)
                || !self.is_real_file_node(target)
                || !real_file_edges.insert((source, target))
            {
                continue;
            }
            real_file_degrees[source.index()].outgoing += 1;
            real_file_degrees[target.index()].incoming += 1;
            insert_bounded_neighbor(
                &self.graph,
                &mut real_file_neighbors[source.index()].outgoing,
                target,
            );
            insert_bounded_neighbor(
                &self.graph,
                &mut real_file_neighbors[target.index()].incoming,
                source,
            );
        }
        self.real_file_edges = real_file_edges;
        self.real_file_degrees = real_file_degrees;
        self.real_file_neighbors = real_file_neighbors;
    }

    fn real_file_degree(&self, index: NodeIndex) -> RealFileDegree {
        self.real_file_degrees
            .get(index.index())
            .copied()
            .unwrap_or_default()
    }

    /// Snapshot graph state owned by `file_path` and replaced by its re-index.
    ///
    /// Incoming edges are deliberately excluded: a high-fan-in utility must
    /// not allocate or walk the repository merely to prove that a comment edit
    /// left its own semantics unchanged. Sorting canonical values makes
    /// equality independent of petgraph's insertion and swap-removal indices.
    pub(crate) fn file_semantic_state(&self, file_path: &str) -> FileGraphSemanticState {
        self.file_semantic_state_observing(file_path, || {}, || {})
    }

    fn file_semantic_state_observing(
        &self,
        file_path: &str,
        mut visit_symbol_node: impl FnMut(),
        mut visit_symbol_edge: impl FnMut(),
    ) -> FileGraphSemanticState {
        let mut file_edges = Vec::new();
        let mut file_nodes = Vec::new();
        if let Some(&file_node) = self.path_to_node.get(file_path)
            && let Some(node) = self.graph.node_weight(file_node)
        {
            file_nodes.push(FileGraphNodeState {
                path: node.file_path.clone(),
                language: node.language.name().to_string(),
            });
            for edge in self
                .graph
                .edges_directed(file_node, petgraph::Direction::Outgoing)
            {
                let Some(source) = self.graph.node_weight(edge.source()) else {
                    continue;
                };
                let Some(target) = self.graph.node_weight(edge.target()) else {
                    continue;
                };
                let weight = edge.weight();
                file_edges.push((
                    source.file_path.clone(),
                    target.file_path.clone(),
                    weight.raw_import.clone(),
                    edge_kind_code(&weight.kind),
                    edge_confidence_code(weight.confidence),
                ));
            }
        }
        file_edges.sort();

        let symbol_identity = |index| {
            self.inner
                .node_weight(index)
                .map(|node| SymbolGraphNodeState {
                    name: node.name.clone(),
                    file: node.file.clone(),
                    kind: symbol_kind_code(&node.kind),
                    line: node.line,
                })
        };
        let symbol_indices = self
            .symbol_nodes_by_file
            .get(file_path)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut symbol_nodes = Vec::with_capacity(symbol_indices.len());
        for &index in symbol_indices {
            visit_symbol_node();
            if let Some(node) = symbol_identity(index) {
                debug_assert_eq!(node.file, file_path);
                symbol_nodes.push(node);
            }
        }
        symbol_nodes.sort();

        let mut symbol_edges = Vec::new();
        for &index in symbol_indices {
            for edge in self
                .inner
                .edges_directed(index, petgraph::Direction::Outgoing)
            {
                visit_symbol_edge();
                let Some(source) = symbol_identity(edge.source()) else {
                    continue;
                };
                let Some(target) = symbol_identity(edge.target()) else {
                    continue;
                };
                symbol_edges.push(SymbolGraphEdgeState {
                    source,
                    target,
                    kind: reference_kind_code(edge.weight()),
                });
            }
        }
        symbol_edges.sort();

        FileGraphSemanticState {
            file_nodes,
            file_edges,
            symbol_nodes,
            symbol_edges,
        }
    }

    fn touch_file_graph(&mut self) {
        self.file_revision = self.file_revision.wrapping_add(1);
    }

    /// Drop unresolved-import pseudo-nodes once their last incident edge is
    /// gone. Long-running watchers otherwise retain one `__ext__:*` node for
    /// every unique import string ever seen, even after the source changed.
    fn prune_orphan_external_nodes(&mut self, candidates: Vec<String>) {
        for path in candidates {
            let is_orphan = self.path_to_node.get(&path).is_some_and(|idx| {
                self.graph
                    .edges_directed(*idx, petgraph::Direction::Incoming)
                    .next()
                    .is_none()
                    && self
                        .graph
                        .edges_directed(*idx, petgraph::Direction::Outgoing)
                        .next()
                        .is_none()
            });
            if is_orphan {
                self.remove_file(&path);
            }
        }
    }

    /// Current in-process file-graph topology generation.
    pub fn file_revision(&self) -> u64 {
        self.file_revision
    }

    /// Process-unique identity for caches shared across Engine lifetimes.
    pub(crate) fn cache_identity(&self) -> u64 {
        self.cache_identity
    }

    /// Add a symbol node to the symbol-level graph, returning its index.
    pub fn add_symbol(&mut self, name: &str, file: &str, kind: types::SymbolKind) -> NodeIndex {
        self.add_symbol_node(types::SymbolNode {
            name: name.to_string(),
            file: file.to_string(),
            kind,
            line: None,
        })
    }

    /// Add a symbol node with a line number to the symbol-level graph.
    pub fn add_symbol_with_line(
        &mut self,
        name: &str,
        file: &str,
        kind: types::SymbolKind,
        line: usize,
    ) -> NodeIndex {
        self.add_symbol_node(types::SymbolNode {
            name: name.to_string(),
            file: file.to_string(),
            kind,
            line: Some(line),
        })
    }

    fn add_symbol_node(&mut self, node: types::SymbolNode) -> NodeIndex {
        let file = node.file.clone();
        let index = self.inner.add_node(node);
        self.symbol_nodes_by_file
            .entry(file)
            .or_default()
            .push(index);
        index
    }

    fn rebuild_symbol_file_index(&mut self) {
        self.symbol_nodes_by_file.clear();
        for index in self.inner.node_indices() {
            if let Some(node) = self.inner.node_weight(index) {
                self.symbol_nodes_by_file
                    .entry(node.file.clone())
                    .or_default()
                    .push(index);
            }
        }
    }

    /// Replace only the symbol-level graph while preserving the file graph.
    /// Persisted symbol graphs already rebuild their derived file postings on
    /// decode, so moving both structures keeps open/reload linear with one pass.
    pub(crate) fn replace_symbol_graph(&mut self, symbol_graph: CodeGraph) {
        self.inner = symbol_graph.inner;
        self.symbol_nodes_by_file = symbol_graph.symbol_nodes_by_file;
    }

    /// Add a reference edge to the symbol-level graph.
    pub fn add_reference(&mut self, from: NodeIndex, to: NodeIndex, kind: types::ReferenceKind) {
        self.inner.add_edge(from, to, kind);
    }

    /// Return the distinct language-independent names owned by one file.
    /// Incremental sync uses this small list only after a definition change to
    /// discover calls that had no old resolved edge to follow.
    pub(crate) fn file_symbol_resolution_names(&self, file: &str) -> Vec<String> {
        let mut names: Vec<_> = self
            .symbol_nodes_by_file
            .get(file)
            .into_iter()
            .flatten()
            .filter_map(|index| self.inner.node_weight(*index))
            .map(|node| node.name.clone())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Compare the language-independent names that participate in symbol
    /// resolution for one file. Multiplicity is retained: introducing or
    /// removing a duplicate definition can make a formerly unique call target
    /// ambiguous even when the set of names is otherwise unchanged. Borrowed
    /// names keep this exact comparison bounded to two pointer arrays.
    pub(crate) fn file_symbol_resolution_matches(
        &self,
        file: &str,
        definitions: &[FileSymbolDefinition],
    ) -> bool {
        let mut old_keys: Vec<_> = self
            .symbol_nodes_by_file
            .get(file)
            .into_iter()
            .flatten()
            .filter_map(|index| self.inner.node_weight(*index))
            .map(|node| node.name.as_str())
            .collect();
        let mut new_keys: Vec<_> = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect();
        old_keys.sort_unstable();
        new_keys.sort_unstable();
        old_keys == new_keys
    }

    /// Return only names whose per-file definition multiplicity changed.
    ///
    /// A file may add one definition while retaining common names such as
    /// `new`. Re-resolving callers of every retained name turns that local edit
    /// into a repository-wide cascade even though those names' global
    /// resolution cardinality did not change. Comparing the sorted multisets
    /// preserves duplicate-definition transitions while cloning only the names
    /// that can actually change another file's call resolution.
    pub(crate) fn file_symbol_resolution_changed_names(
        &self,
        file: &str,
        definitions: &[FileSymbolDefinition],
    ) -> Vec<String> {
        let mut old_keys: Vec<_> = self
            .symbol_nodes_by_file
            .get(file)
            .into_iter()
            .flatten()
            .filter_map(|index| self.inner.node_weight(*index))
            .map(|node| node.name.as_str())
            .collect();
        let mut new_keys: Vec<_> = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect();
        old_keys.sort_unstable();
        new_keys.sort_unstable();

        let mut changed = Vec::new();
        let mut old_index = 0usize;
        let mut new_index = 0usize;
        while old_index < old_keys.len() || new_index < new_keys.len() {
            let name = match (old_keys.get(old_index), new_keys.get(new_index)) {
                (Some(old), Some(new)) if old <= new => *old,
                (Some(_), Some(new)) => *new,
                (Some(old), None) => *old,
                (None, Some(new)) => *new,
                (None, None) => break,
            };

            let old_start = old_index;
            while old_keys
                .get(old_index)
                .is_some_and(|candidate| *candidate == name)
            {
                old_index += 1;
            }
            let new_start = new_index;
            while new_keys
                .get(new_index)
                .is_some_and(|candidate| *candidate == name)
            {
                new_index += 1;
            }
            if old_index - old_start != new_index - new_start {
                changed.push(name.to_owned());
            }
        }
        changed
    }

    /// Replace the definitions and outgoing references owned by `file`.
    ///
    /// Definitions whose resolution-name multiplicity did not change retain
    /// their node indices and incoming cross-file references. Only changed
    /// names are removed and rebuilt, so adding an unrelated definition to a
    /// high-fan-in target does not require refreshing every retained caller.
    /// Outgoing references are always removed because the edited file owns
    /// them and its exact new source must be resolved again.
    pub(crate) fn refresh_file_symbols(
        &mut self,
        file: &str,
        definitions: &[FileSymbolDefinition],
        changed_resolution_names: &[String],
    ) {
        debug_assert!(
            changed_resolution_names
                .windows(2)
                .all(|names| names[0] < names[1]),
            "changed resolution names must be sorted and unique"
        );
        debug_assert!(
            !changed_resolution_names.is_empty()
                || self.file_symbol_resolution_matches(file, definitions),
            "an empty change set requires identical resolution keys"
        );
        let resolution_name_changed = |name: &str| {
            changed_resolution_names
                .binary_search_by(|candidate| candidate.as_str().cmp(name))
                .is_ok()
        };
        let retained_indices: Vec<_> = self
            .symbol_nodes_by_file
            .get(file)
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                self.inner
                    .node_weight(*index)
                    .is_some_and(|node| !resolution_name_changed(&node.name))
            })
            .collect();

        // Pair retained duplicate definitions deterministically. Cross-file
        // resolution rejects duplicates, but stable pairing still keeps local
        // metadata and non-call reference kinds coherent.
        let mut old_by_name: HashMap<String, Vec<(NodeIndex, u8, Option<usize>)>> = HashMap::new();
        for index in &retained_indices {
            if let Some(node) = self.inner.node_weight(*index) {
                old_by_name.entry(node.name.clone()).or_default().push((
                    *index,
                    symbol_kind_code(&node.kind),
                    node.line,
                ));
            }
        }
        for nodes in old_by_name.values_mut() {
            nodes.sort_unstable_by_key(|(index, kind, line)| (*kind, *line, index.index()));
        }

        let mut new_by_name: HashMap<&str, Vec<&FileSymbolDefinition>> = HashMap::new();
        for definition in definitions
            .iter()
            .filter(|definition| !resolution_name_changed(&definition.name))
        {
            new_by_name
                .entry(definition.name.as_str())
                .or_default()
                .push(definition);
        }
        for definitions in new_by_name.values_mut() {
            definitions.sort_unstable_by_key(|definition| {
                (symbol_kind_code(&definition.kind), definition.line)
            });
        }

        for (name, definitions) in new_by_name {
            let existing = old_by_name
                .get(name)
                .expect("unchanged resolution keys must retain every symbol name");
            debug_assert_eq!(existing.len(), definitions.len());
            for ((index, _, _), definition) in existing.iter().zip(definitions) {
                let node = self
                    .inner
                    .node_weight_mut(*index)
                    .expect("symbol file posting must reference a live node");
                node.kind = definition.kind.clone();
                node.line = Some(definition.line);
            }
        }

        let mut outgoing_edges: Vec<_> = retained_indices
            .iter()
            .flat_map(|index| {
                self.inner
                    .edges_directed(*index, petgraph::Direction::Outgoing)
                    .map(|edge| edge.id())
            })
            .collect();
        // DiGraph removes edges with swap-removal, so lower indices can be
        // replaced by higher pending edges. Remove from the end to keep every
        // collected EdgeIndex valid until its turn.
        outgoing_edges.sort_unstable_by_key(|edge| std::cmp::Reverse(edge.index()));
        outgoing_edges.dedup();
        for edge in outgoing_edges {
            self.inner.remove_edge(edge);
        }

        self.remove_file_symbols_named(file, changed_resolution_names);
        for definition in definitions
            .iter()
            .filter(|definition| resolution_name_changed(&definition.name))
        {
            self.add_symbol_with_line(
                &definition.name,
                file,
                definition.kind.clone(),
                definition.line,
            );
        }
    }

    /// Symbol nodes with this exact name owned by one file. The per-file
    /// posting keeps incremental call resolution proportional to the target
    /// file rather than the repository-wide symbol count.
    pub(crate) fn symbol_nodes_named_in_file(&self, file: &str, name: &str) -> Vec<NodeIndex> {
        self.symbol_nodes_by_file
            .get(file)
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                self.inner
                    .node_weight(*index)
                    .is_some_and(|node| node.name == name)
            })
            .collect()
    }

    /// Function definition nodes in source order for enclosing-call lookup.
    pub(crate) fn function_nodes_in_file(&self, file: &str) -> Vec<(usize, NodeIndex)> {
        let mut functions: Vec<_> = self
            .symbol_nodes_by_file
            .get(file)
            .into_iter()
            .flatten()
            .filter_map(|index| {
                let node = self.inner.node_weight(*index)?;
                (node.kind == types::SymbolKind::Function)
                    .then_some((node.line.unwrap_or_default(), *index))
            })
            .collect();
        functions.sort_unstable_by_key(|(line, index)| (*line, index.index()));
        functions
    }

    /// Return the number of nodes in the symbol-level graph.
    pub fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    /// Return the number of edges in the symbol-level graph.
    pub fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    /// Return or insert a node for `file_path`.
    pub fn get_or_insert_node(&mut self, file_path: &str, language: Language) -> NodeIndex {
        if let Some(&idx) = self.path_to_node.get(file_path) {
            return idx;
        }
        let node = CodeNode {
            file_path: file_path.to_string(),
            language,
            pagerank: 0.0,
            out_degree: 0,
            in_degree: 0,
            community: None,
        };
        let idx = self.graph.add_node(node);
        debug_assert_eq!(idx.index(), self.real_file_degrees.len());
        self.real_file_degrees.push(RealFileDegree::default());
        self.real_file_neighbors.push(RealFileNeighbors::default());
        self.path_to_node.insert(file_path.to_string(), idx);
        self.touch_file_graph();
        idx
    }

    /// Add a resolved edge between two indexed files.
    pub fn add_edge(
        &mut self,
        from: &str,
        to: &str,
        raw_import: &str,
        from_lang: Language,
        to_lang: Language,
    ) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        let to_idx = self.get_or_insert_node(to, to_lang);
        let kind = EdgeKind::Resolved;
        let confidence = kind.default_confidence();
        self.record_real_file_edge(from_idx, to_idx);
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: raw_import.to_string(),
                kind,
                confidence,
            },
        );
        self.touch_file_graph();
        // Update degree counters.
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
        if let Some(n) = self.graph.node_weight_mut(to_idx) {
            n.in_degree += 1;
        }
    }

    /// Add an external (unresolved) edge; `to` is the raw import string used as a label.
    pub fn add_external_edge(&mut self, from: &str, raw_import: &str, from_lang: Language) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        // External target represented as a pseudo-node with the raw import as path.
        let ext_key = format!("__ext__:{raw_import}");
        let to_idx = self.get_or_insert_node(&ext_key, from_lang);
        let kind = EdgeKind::External;
        let confidence = kind.default_confidence();
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: raw_import.to_string(),
                kind,
                confidence,
            },
        );
        self.touch_file_graph();
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
    }

    /// Remove a file node and all its incident edges from the graph.
    pub fn remove_file(&mut self, file_path: &str) {
        if let Some(idx) = self.path_to_node.remove(file_path) {
            // Collect neighbours whose degree counters need adjustment.
            let in_neighbours: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
                .collect();
            let out_neighbours: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .collect();
            let possible_orphan_externals: Vec<String> = out_neighbours
                .iter()
                .filter_map(|neighbor| self.graph.node_weight(*neighbor))
                .filter(|node| node.file_path.starts_with("__ext__:"))
                .map(|node| node.file_path.clone())
                .collect();

            let last_idx = NodeIndex::new(self.graph.node_count().saturating_sub(1));
            let incident_real_edges: HashSet<_> =
                [petgraph::Direction::Incoming, petgraph::Direction::Outgoing]
                    .into_iter()
                    .flat_map(|direction| self.graph.edges_directed(idx, direction))
                    .map(|edge| (edge.source(), edge.target()))
                    .filter(|edge| self.real_file_edges.contains(edge))
                    .collect();
            // petgraph moves the last node into the removed slot. Preserve the
            // derived-edge keys for that survivor without a repository scan.
            let moved_real_edges: HashSet<_> = if idx == last_idx {
                HashSet::new()
            } else {
                [petgraph::Direction::Incoming, petgraph::Direction::Outgoing]
                    .into_iter()
                    .flat_map(|direction| self.graph.edges_directed(last_idx, direction))
                    .map(|edge| (edge.source(), edge.target()))
                    .filter(|edge| {
                        edge.0 != idx && edge.1 != idx && self.real_file_edges.contains(edge)
                    })
                    .collect()
            };
            for (source, target) in incident_real_edges {
                self.forget_real_file_edge(source, target, source != idx, target != idx);
            }

            for nb in &in_neighbours {
                if let Some(n) = self.graph.node_weight_mut(*nb) {
                    n.out_degree = n.out_degree.saturating_sub(1);
                }
            }
            for nb in &out_neighbours {
                if let Some(n) = self.graph.node_weight_mut(*nb) {
                    n.in_degree = n.in_degree.saturating_sub(1);
                }
            }

            // petgraph swap_remove_node swaps the last node into position `idx`.
            // We must update path_to_node for the swapped node.
            if idx != last_idx
                && let Some(swapped_path) = self
                    .graph
                    .node_weight(last_idx)
                    .map(|n| n.file_path.clone())
            {
                self.path_to_node.insert(swapped_path, idx);
            }
            self.graph.remove_node(idx);
            self.real_file_degrees.swap_remove(idx.index());
            self.real_file_neighbors.swap_remove(idx.index());
            for (source, target) in moved_real_edges {
                if self.real_file_edges.remove(&(source, target)) {
                    let mapped_source = if source == last_idx { idx } else { source };
                    let mapped_target = if target == last_idx { idx } else { target };
                    if source == last_idx
                        && let Some(position) = self.real_file_neighbors[mapped_target.index()]
                            .incoming
                            .iter()
                            .position(|neighbor| *neighbor == last_idx)
                    {
                        self.real_file_neighbors[mapped_target.index()].incoming[position] = idx;
                    }
                    if target == last_idx
                        && let Some(position) = self.real_file_neighbors[mapped_source.index()]
                            .outgoing
                            .iter()
                            .position(|neighbor| *neighbor == last_idx)
                    {
                        self.real_file_neighbors[mapped_source.index()].outgoing[position] = idx;
                    }
                    self.real_file_edges.insert((mapped_source, mapped_target));
                }
            }
            self.touch_file_graph();
            self.prune_orphan_external_nodes(possible_orphan_externals);
        }
    }

    /// Remove only the outgoing edges of `file_path` (used before re-extracting imports).
    pub fn remove_file_edges(&mut self, file_path: &str) {
        if let Some(&idx) = self.path_to_node.get(file_path) {
            let out_neighbours: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(idx, petgraph::Direction::Outgoing)
                .collect();
            let possible_orphan_externals: Vec<String> = out_neighbours
                .iter()
                .filter_map(|neighbor| self.graph.node_weight(*neighbor))
                .filter(|node| node.file_path.starts_with("__ext__:"))
                .map(|node| node.file_path.clone())
                .collect();
            let distinct_real_targets: HashSet<_> = out_neighbours
                .iter()
                .copied()
                .filter(|target| self.real_file_edges.contains(&(idx, *target)))
                .collect();
            for target in distinct_real_targets {
                self.forget_real_file_edge(idx, target, false, true);
            }
            for nb in &out_neighbours {
                if let Some(n) = self.graph.node_weight_mut(*nb) {
                    n.in_degree = n.in_degree.saturating_sub(1);
                }
            }
            // Remove all outgoing edges.
            let out_edges: Vec<_> = self
                .graph
                .edges_directed(idx, petgraph::Direction::Outgoing)
                .map(|e| e.id())
                .collect();
            let removed_any = !out_edges.is_empty();
            for e in out_edges {
                self.graph.remove_edge(e);
            }
            if removed_any {
                self.touch_file_graph();
            }
            if let Some(n) = self.graph.node_weight_mut(idx) {
                n.out_degree = 0;
            }
            self.prune_orphan_external_nodes(possible_orphan_externals);
        }
    }

    /// Files that import `file_path` (direct callers).
    pub fn callers(&self, file_path: &str) -> Vec<String> {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Incoming)
            .filter_map(|nb| self.graph.node_weight(nb))
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .map(|n| n.file_path.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    /// Number of distinct files that import or reference `file_path`.
    /// Parallel edges from the same file count once, matching [`Self::callers`]
    /// without cloning every caller path.
    pub fn caller_count(&self, file_path: &str) -> usize {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return 0;
        };
        self.real_file_degree(idx).incoming
    }

    /// Files that `file_path` imports (direct callees / dependencies).
    pub fn callees(&self, file_path: &str) -> Vec<String> {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        self.graph
            .neighbors_directed(idx, petgraph::Direction::Outgoing)
            .filter_map(|nb| self.graph.node_weight(nb))
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .map(|n| n.file_path.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    }

    fn bounded_real_neighbors_observing(
        &self,
        index: NodeIndex,
        direction: petgraph::Direction,
        limit: usize,
        mut observe_edge: impl FnMut(),
    ) -> Vec<String> {
        if limit == 0 {
            return Vec::new();
        }
        let budget = bounded_edge_scan_budget(limit);
        let mut selected = Vec::<String>::with_capacity(budget);
        let neighbors = match direction {
            petgraph::Direction::Incoming => {
                self.real_file_neighbors[index.index()].incoming.as_slice()
            }
            petgraph::Direction::Outgoing => {
                self.real_file_neighbors[index.index()].outgoing.as_slice()
            }
        };
        for &neighbor in neighbors.iter().take(budget) {
            observe_edge();
            let Some(node) = self.graph.node_weight(neighbor) else {
                continue;
            };
            debug_assert!(!node.file_path.starts_with("__ext__:"));
            selected.push(node.file_path.clone());
        }
        selected.sort_unstable();
        selected
    }

    /// Return at most `limit` stable, distinct real-file callees plus the exact
    /// distinct real transition degree. Derived-adjacency visits are hard-capped;
    /// probability for useful neighbours hidden behind noisy edges therefore
    /// resets instead of being concentrated onto the sampled subset.
    pub(crate) fn bounded_callees(&self, file_path: &str, limit: usize) -> (Vec<String>, usize) {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return (Vec::new(), 0);
        };
        let selected =
            self.bounded_real_neighbors_observing(idx, petgraph::Direction::Outgoing, limit, || {});
        (selected, self.real_file_degree(idx).outgoing)
    }

    /// Return at most `limit` distinct real files that import `file_path`.
    ///
    /// Parallel edges are skipped until the distinct limit is reached or the
    /// incoming adjacency is exhausted. Results are sorted for determinism.
    pub(crate) fn bounded_callers(&self, file_path: &str, limit: usize) -> Vec<String> {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return Vec::new();
        };
        self.bounded_real_neighbors_observing(idx, petgraph::Direction::Incoming, limit, || {})
    }

    /// Count distinct callers up to `limit`, ignoring parallel edges without
    /// fabricating popularity when the adjacency contains fewer real callers.
    #[cfg(test)]
    fn bounded_callees_with_edge_visits(
        &self,
        file_path: &str,
        limit: usize,
    ) -> (Vec<String>, usize, usize) {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return (Vec::new(), 0, 0);
        };
        let mut edge_visits = 0;
        let selected = self.bounded_real_neighbors_observing(
            idx,
            petgraph::Direction::Outgoing,
            limit,
            || edge_visits += 1,
        );
        (selected, self.real_file_degree(idx).outgoing, edge_visits)
    }

    /// Find files under `from_prefix` that import any file under `to_prefix`.
    ///
    /// Answers module-level cross-package queries like "which gateway files
    /// import from the security module?" in a single pass over the edge list.
    ///
    /// Both prefixes are matched with `starts_with`, so `"src/gateway"` matches
    /// `"src/gateway/server.ts"` and `"src/gateway/hooks.ts"`.
    pub fn cross_imports(&self, from_prefix: &str, to_prefix: &str) -> Vec<String> {
        let mut result = std::collections::HashSet::new();
        for edge in self.graph.edge_references() {
            let source = &self.graph[edge.source()];
            let target = &self.graph[edge.target()];
            if source.file_path.starts_with(from_prefix)
                && target.file_path.starts_with(to_prefix)
                && !source.file_path.starts_with("__ext__:")
            {
                result.insert(source.file_path.clone());
            }
        }
        let mut sorted: Vec<String> = result.into_iter().collect();
        sorted.sort();
        sorted
    }

    /// Find files under `from_prefix` that import any file under `to_prefix`, ranked by relevance.
    ///
    /// Score = sum of target PageRank values for each cross-import edge, multiplied by
    /// a recency boost: `1 + exp(-0.05 * days_old)` for the source file.
    ///
    /// Returns `(file_path, score)` pairs sorted by score descending.
    ///
    /// Considers only true import-graph edges (`EdgeKind::Resolved` and
    /// `EdgeKind::External`). Call-graph and DocumentedBy edges are
    /// excluded so that an unrelated function call sharing a name with a
    /// symbol in the target package does not produce a phantom
    /// architecture-boundary violation. Use
    /// [`Self::cross_imports_ranked_with_kinds`] to broaden the edge kinds.
    pub fn cross_imports_ranked(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        recency_map: Option<&std::collections::HashMap<String, i64>>,
        limit: Option<usize>,
    ) -> Vec<(String, f32)> {
        self.cross_imports_ranked_with_kinds(
            from_prefix,
            to_prefix,
            recency_map,
            limit,
            DEFAULT_IMPORT_BOUNDARY_KINDS,
        )
    }

    /// Lower-level cross-import query that lets the caller pick which
    /// `EdgeKind`s count as an import-boundary edge.
    pub fn cross_imports_ranked_with_kinds(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        recency_map: Option<&std::collections::HashMap<String, i64>>,
        limit: Option<usize>,
        kinds: &[EdgeKind],
    ) -> Vec<(String, f32)> {
        let mut scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();

        for edge in self.graph.edge_references() {
            let source = &self.graph[edge.source()];
            let target = &self.graph[edge.target()];
            let edge_kind = &edge.weight().kind;

            if !kinds.iter().any(|k| k == edge_kind) {
                continue;
            }
            if source.file_path.starts_with(from_prefix)
                && target.file_path.starts_with(to_prefix)
                && !source.file_path.starts_with("__ext__:")
            {
                let target_pr = target.pagerank.max(0.001);
                let entry = scores.entry(source.file_path.clone()).or_insert(0.0);
                *entry += target_pr;
            }
        }

        // Apply recency boost per source file.
        if let Some(rmap) = recency_map {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            for (file, score) in scores.iter_mut() {
                if let Some(&commit_ts) = rmap.get(file) {
                    let days_old = ((now - commit_ts) as f64 / 86400.0).max(0.0);
                    let boost = (-0.05 * days_old).exp();
                    *score *= 1.0 + boost as f32;
                }
            }
        }

        let mut ranked: Vec<(String, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        if let Some(lim) = limit {
            ranked.truncate(lim);
        }

        ranked
    }

    /// Evidence-aware variant of [`Self::cross_imports_ranked`] that also
    /// returns the matched import line(s) for each source file.
    ///
    /// Each result is `(source_path, score, evidence)`, where `evidence`
    /// holds `(target_path, raw_import, edge_kind)` for every edge that
    /// contributed to the score. Surfacing the matched line per the
    /// `cross-imports` issue (#101) lets callers verify the boundary
    /// claim without re-grepping the source file.
    pub fn cross_imports_ranked_with_evidence(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        kinds: &[EdgeKind],
    ) -> Vec<CrossImportEvidenceRow> {
        self.cross_imports_ranked_with_evidence_recency(from_prefix, to_prefix, None, kinds)
    }

    /// Recency-aware variant of [`Self::cross_imports_ranked_with_evidence`].
    ///
    /// Mirrors the scoring model used by [`Self::cross_imports_ranked`] so
    /// that the CLI evidence path stays on the same recency-weighted
    /// PageRank ranking. Without the recency multiplier, a long-stale
    /// importer can outrank a recently touched one — a regression noted
    /// during review of the v0.41.2 cross-imports fix.
    pub fn cross_imports_ranked_with_evidence_recency(
        &self,
        from_prefix: &str,
        to_prefix: &str,
        recency_map: Option<&std::collections::HashMap<String, i64>>,
        kinds: &[EdgeKind],
    ) -> Vec<CrossImportEvidenceRow> {
        let mut acc: std::collections::HashMap<String, (f32, Vec<CrossImportEvidence>)> =
            std::collections::HashMap::new();

        for edge in self.graph.edge_references() {
            let source = &self.graph[edge.source()];
            let target = &self.graph[edge.target()];
            let edge_data = edge.weight();
            if !kinds.iter().any(|k| k == &edge_data.kind) {
                continue;
            }
            if source.file_path.starts_with(from_prefix)
                && target.file_path.starts_with(to_prefix)
                && !source.file_path.starts_with("__ext__:")
            {
                let entry = acc
                    .entry(source.file_path.clone())
                    .or_insert((0.0, Vec::new()));
                entry.0 += target.pagerank.max(0.001);
                entry.1.push((
                    target.file_path.clone(),
                    edge_data.raw_import.clone(),
                    edge_data.kind.clone(),
                ));
            }
        }

        // Match `cross_imports_ranked`'s recency multiplier so the evidence
        // path produces the same ranking as the score-only path.
        if let Some(rmap) = recency_map {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            for (file, (score, _)) in acc.iter_mut() {
                if let Some(&commit_ts) = rmap.get(file) {
                    let days_old = ((now - commit_ts) as f64 / 86400.0).max(0.0);
                    let boost = (-0.05 * days_old).exp();
                    *score *= 1.0 + boost as f32;
                }
            }
        }

        let mut ranked: Vec<CrossImportEvidenceRow> = acc
            .into_iter()
            .map(|(path, (score, ev))| (path, score, ev))
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked
    }

    /// Transitive callers (files that transitively depend on `file_path`) up to `depth` hops.
    pub fn transitive_callers(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.transitive_traverse(file_path, depth, |f| self.callers(f))
    }

    /// Transitive callees (files that `file_path` transitively imports) up to `depth` hops.
    pub fn transitive_callees(&self, file_path: &str, depth: usize) -> Vec<String> {
        self.transitive_traverse(file_path, depth, |f| self.callees(f))
    }

    fn transitive_traverse(
        &self,
        file_path: &str,
        depth: usize,
        neighbors: impl Fn(&str) -> Vec<String>,
    ) -> Vec<String> {
        let mut visited = std::collections::HashSet::new();
        let mut frontier = vec![file_path.to_string()];
        for _ in 0..depth {
            let mut next = Vec::new();
            for f in &frontier {
                for nb in neighbors(f) {
                    if visited.insert(nb.clone()) {
                        next.push(nb);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        visited.into_iter().collect()
    }

    /// Get a node by file path.
    pub fn node(&self, file_path: &str) -> Option<&CodeNode> {
        self.path_to_node
            .get(file_path)
            .and_then(|idx| self.graph.node_weight(*idx))
    }

    /// Apply computed PageRank scores back to the graph nodes.
    pub fn apply_pagerank(&mut self, scores: &HashMap<String, f32>) {
        for node in self.graph.node_weights_mut() {
            if let Some(&pr) = scores.get(&node.file_path) {
                node.pagerank = pr;
            }
        }
    }

    /// Serialize to the flat `GraphData` format for persistence.
    pub fn to_flat(&self) -> GraphData {
        let nodes: Vec<CodeNode> = self
            .graph
            .node_weights()
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .cloned()
            .collect();

        let edges: Vec<(String, String, CodeEdge)> = self
            .graph
            .edge_indices()
            .filter_map(|e| {
                let (from_idx, to_idx) = self.graph.edge_endpoints(e)?;
                let edge = self.graph.edge_weight(e)?;
                let from = self.graph.node_weight(from_idx)?;
                let to = self.graph.node_weight(to_idx)?;
                Some((from.file_path.clone(), to.file_path.clone(), edge.clone()))
            })
            .collect();

        GraphData { nodes, edges }
    }

    /// Reconstruct a `CodeGraph` from the flat persistence format.
    pub fn from_flat(data: GraphData) -> Self {
        let mut g = Self::new();
        for node in &data.nodes {
            g.get_or_insert_node(&node.file_path, node.language);
            // Restore persisted PageRank and degree counts.
            if let Some(idx) = g.path_to_node.get(&node.file_path).copied()
                && let Some(n) = g.graph.node_weight_mut(idx)
            {
                n.pagerank = node.pagerank;
                n.out_degree = node.out_degree;
                n.in_degree = node.in_degree;
            }
        }
        for (from, to, edge) in data.edges {
            let from_lang = g
                .path_to_node
                .get(&from)
                .and_then(|idx| g.graph.node_weight(*idx))
                .map(|n| n.language)
                .unwrap_or(Language::Rust);
            let to_lang = g
                .path_to_node
                .get(&to)
                .and_then(|idx| g.graph.node_weight(*idx))
                .map(|n| n.language)
                .unwrap_or(Language::Rust);
            let from_idx = g.get_or_insert_node(&from, from_lang);
            let to_idx = g.get_or_insert_node(&to, to_lang);
            // Fix confidence for legacy data where the field was missing and
            // serde defaulted to Verified regardless of edge kind.
            let mut edge = edge;
            edge.confidence = edge.kind.default_confidence();
            g.graph.add_edge(from_idx, to_idx, edge);
            g.touch_file_graph();
        }
        g.rebuild_real_file_edge_index();
        g
    }

    /// Add a call-site edge between two files.
    ///
    /// Unlike import edges, call edges represent actual function invocations
    /// (as resolved by the symbol table after the parallel parse phase).
    pub fn add_call_edge(
        &mut self,
        from: &str,
        to: &str,
        callee_name: &str,
        from_lang: Language,
        to_lang: Language,
    ) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        let to_idx = self.get_or_insert_node(to, to_lang);
        let kind = EdgeKind::Calls;
        let confidence = kind.default_confidence();
        self.record_real_file_edge(from_idx, to_idx);
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: callee_name.to_string(),
                kind,
                confidence,
            },
        );
        self.touch_file_graph();
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
        if let Some(n) = self.graph.node_weight_mut(to_idx) {
            n.in_degree += 1;
        }
    }

    /// Add a documentation-to-code edge between a doc file and a code file.
    ///
    /// These edges represent that the doc file references a symbol defined in
    /// the target code file (e.g., a backtick identifier in Markdown).
    pub fn add_doc_edge(
        &mut self,
        from: &str,
        to: &str,
        symbol_name: &str,
        from_lang: Language,
        to_lang: Language,
    ) {
        let from_idx = self.get_or_insert_node(from, from_lang);
        let to_idx = self.get_or_insert_node(to, to_lang);
        let kind = EdgeKind::DocumentedBy;
        let confidence = kind.default_confidence();
        self.record_real_file_edge(from_idx, to_idx);
        self.graph.add_edge(
            from_idx,
            to_idx,
            CodeEdge {
                raw_import: symbol_name.to_string(),
                kind,
                confidence,
            },
        );
        self.touch_file_graph();
        if let Some(n) = self.graph.node_weight_mut(from_idx) {
            n.out_degree += 1;
        }
        if let Some(n) = self.graph.node_weight_mut(to_idx) {
            n.in_degree += 1;
        }
    }

    /// Compute graph statistics.
    pub fn stats(&self) -> GraphStats {
        let mut resolved = 0usize;
        let mut external = 0usize;
        let mut calls = 0usize;
        let mut docs = 0usize;
        let mut conf_verified = 0usize;
        let mut conf_high = 0usize;
        let mut conf_medium = 0usize;
        let mut conf_low = 0usize;
        for e in self.graph.edge_weights() {
            match e.kind {
                EdgeKind::Resolved => resolved += 1,
                EdgeKind::External => external += 1,
                EdgeKind::Calls => calls += 1,
                EdgeKind::DocumentedBy => docs += 1,
            }
            match e.confidence {
                EdgeConfidence::Verified => conf_verified += 1,
                EdgeConfidence::High => conf_high += 1,
                EdgeConfidence::Medium => conf_medium += 1,
                EdgeConfidence::Low => conf_low += 1,
            }
        }
        GraphStats {
            node_count: self.graph.node_count(),
            edge_count: self.graph.edge_count(),
            resolved_edges: resolved,
            external_edges: external,
            call_edges: calls,
            doc_edges: docs,
            symbol_nodes: self.inner.node_count(),
            symbol_edges: self.inner.edge_count(),
            confidence_counts: (conf_verified, conf_high, conf_medium, conf_low),
        }
    }

    /// Iterate over all call edges as `(caller_file, callee_name)` tuples.
    pub fn call_edges(&self) -> Vec<(String, String)> {
        self.graph
            .edge_references()
            .filter(|e| e.weight().kind == EdgeKind::Calls)
            .map(|e| {
                let caller = self.graph[e.source()].file_path.clone();
                let callee_name = e.weight().raw_import.clone();
                (caller, callee_name)
            })
            .collect()
    }

    /// Iterate over all real (non-external) nodes sorted by PageRank descending.
    pub fn nodes_by_pagerank(&self) -> Vec<&CodeNode> {
        let mut nodes: Vec<&CodeNode> = self
            .graph
            .node_weights()
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .collect();
        nodes.sort_by(|a, b| {
            b.pagerank
                .partial_cmp(&a.pagerank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        nodes
    }

    /// Query the symbol-level graph for callers of `symbol_name`.
    ///
    /// Returns `(file, caller_symbol_name)` pairs for every symbol that has a
    /// `Call` edge pointing to a node whose name matches `symbol_name`.
    pub fn get_symbol_callers(&self, symbol_name: &str) -> Vec<(String, String)> {
        // Find all nodes matching the target symbol name.
        let target_indices: Vec<NodeIndex> = self
            .inner
            .node_indices()
            .filter(|&idx| {
                self.inner
                    .node_weight(idx)
                    .is_some_and(|n| n.name == symbol_name)
            })
            .collect();

        if target_indices.is_empty() {
            return Vec::new();
        }

        let mut callers = Vec::new();
        for &target in &target_indices {
            for edge in self
                .inner
                .edges_directed(target, petgraph::Direction::Incoming)
            {
                if *edge.weight() == types::ReferenceKind::Call
                    && let Some(caller_node) = self.inner.node_weight(edge.source())
                {
                    callers.push((caller_node.file.clone(), caller_node.name.clone()));
                }
            }
        }
        callers
    }

    /// Query the symbol-level graph for callees of `symbol_name`.
    ///
    /// Returns the names of all symbols that have a `Call` edge FROM
    /// a node whose name matches `symbol_name`.
    pub fn get_symbol_callees(&self, symbol_name: &str) -> Vec<String> {
        let source_indices: Vec<NodeIndex> = self
            .inner
            .node_indices()
            .filter(|&idx| {
                self.inner
                    .node_weight(idx)
                    .is_some_and(|n| n.name == symbol_name)
            })
            .collect();

        if source_indices.is_empty() {
            return Vec::new();
        }

        let mut callees = Vec::new();
        for &src in &source_indices {
            for edge in self
                .inner
                .edges_directed(src, petgraph::Direction::Outgoing)
            {
                if *edge.weight() == types::ReferenceKind::Call
                    && let Some(target_node) = self.inner.node_weight(edge.target())
                {
                    callees.push(target_node.name.clone());
                }
            }
        }
        callees.sort();
        callees.dedup();
        callees
    }

    /// Return the number of nodes in the symbol-level inner graph.
    pub fn symbol_node_count(&self) -> usize {
        self.inner.node_count()
    }

    /// Remove only definitions with one of the supplied resolution names.
    ///
    /// DiGraph swap-removal changes the last live node's index. Removing the
    /// selected indices from highest to lowest ensures a selected node is
    /// never moved before its turn, while the per-file postings are repaired
    /// for every survivor that moves.
    fn remove_file_symbols_named(&mut self, file: &str, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let mut to_remove: Vec<_> = self
            .symbol_nodes_by_file
            .get(file)
            .into_iter()
            .flatten()
            .copied()
            .filter(|index| {
                self.inner.node_weight(*index).is_some_and(|node| {
                    names
                        .binary_search_by(|candidate| candidate.as_str().cmp(&node.name))
                        .is_ok()
                })
            })
            .collect();
        to_remove.sort_unstable_by_key(|index| std::cmp::Reverse(index.index()));

        for index in to_remove {
            let last_index = NodeIndex::new(self.inner.node_count() - 1);
            let moved_file = (index != last_index)
                .then(|| {
                    self.inner
                        .node_weight(last_index)
                        .map(|node| node.file.clone())
                })
                .flatten();

            let removed_postings = self
                .symbol_nodes_by_file
                .get_mut(file)
                .expect("selected symbol node must have a file posting");
            let removed_slot = removed_postings
                .iter()
                .position(|posting| *posting == index)
                .expect("selected symbol node posting must reference its live index");
            removed_postings.swap_remove(removed_slot);

            let removed = self.inner.remove_node(index);
            debug_assert!(removed.is_some_and(|node| node.file == file));

            if let Some(moved_file) = moved_file {
                let moved_postings = self
                    .symbol_nodes_by_file
                    .get_mut(&moved_file)
                    .expect("moved symbol node must have a file posting");
                let moved_slot = moved_postings
                    .iter_mut()
                    .find(|posting| **posting == last_index)
                    .expect("moved symbol node posting must reference its old index");
                *moved_slot = index;
            }
        }

        if self
            .symbol_nodes_by_file
            .get(file)
            .is_some_and(|postings| postings.is_empty())
        {
            self.symbol_nodes_by_file.remove(file);
        }
    }

    /// Remove all symbol nodes (and their edges) for a given file.
    ///
    /// This is used during incremental reindex: before re-extracting
    /// definitions and call edges for a file, we remove the old ones.
    pub fn remove_file_symbols(&mut self, file: &str) {
        let Some(mut to_remove) = self.symbol_nodes_by_file.remove(file) else {
            return;
        };
        // DiGraph uses swap-removal. Descending order guarantees a node from
        // the removed file is never swapped into an index still pending in this
        // list; only the moved survivor's posting needs repair.
        to_remove.sort_unstable_by_key(|index| std::cmp::Reverse(index.index()));
        for index in to_remove {
            let last_index = NodeIndex::new(self.inner.node_count() - 1);
            let moved_file = (index != last_index)
                .then(|| {
                    self.inner
                        .node_weight(last_index)
                        .map(|node| node.file.clone())
                })
                .flatten();
            let removed = self.inner.remove_node(index);
            debug_assert!(removed.is_some_and(|node| node.file == file));

            if let Some(moved_file) = moved_file {
                let moved_postings = self
                    .symbol_nodes_by_file
                    .get_mut(&moved_file)
                    .expect("moved symbol node must have a file posting");
                let moved_slot = moved_postings
                    .iter_mut()
                    .find(|posting| **posting == last_index)
                    .expect("moved symbol node posting must reference its old index");
                *moved_slot = index;
            }
        }
    }

    // -----------------------------------------------------------------
    // Graph analysis methods
    // -----------------------------------------------------------------

    /// Find the shortest path between two files using BFS.
    ///
    /// Returns `None` if either file is missing or no path exists.
    /// The returned vector includes both endpoints.
    pub fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let &from_idx = self.path_to_node.get(from)?;
        let &to_idx = self.path_to_node.get(to)?;

        if from_idx == to_idx {
            return Some(vec![from.to_string()]);
        }

        // BFS tracking predecessors.
        let mut visited: HashMap<NodeIndex, Option<NodeIndex>> = HashMap::new();
        visited.insert(from_idx, None);
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from_idx);

        while let Some(current) = queue.pop_front() {
            if current == to_idx {
                // Reconstruct path.
                let mut path = Vec::new();
                let mut cur = Some(to_idx);
                while let Some(idx) = cur {
                    if let Some(node) = self.graph.node_weight(idx) {
                        path.push(node.file_path.clone());
                    }
                    cur = visited.get(&idx).copied().flatten();
                }
                path.reverse();
                return Some(path);
            }

            // Traverse both outgoing and incoming edges (undirected BFS).
            // Skip __ext__ pseudo-nodes to avoid false paths through shared
            // external imports (e.g., a.rs -> __ext__:serde -> b.rs).
            let neighbors: Vec<NodeIndex> = self
                .graph
                .neighbors_directed(current, petgraph::Direction::Outgoing)
                .chain(
                    self.graph
                        .neighbors_directed(current, petgraph::Direction::Incoming),
                )
                .filter(|&nb| {
                    self.graph
                        .node_weight(nb)
                        .map(|n| !n.file_path.starts_with("__ext__:"))
                        .unwrap_or(false)
                })
                .collect();

            for nb in neighbors {
                if let std::collections::hash_map::Entry::Vacant(e) = visited.entry(nb) {
                    e.insert(Some(current));
                    queue.push_back(nb);
                }
            }
        }

        None // No path found
    }

    /// Run Louvain community detection and store results on nodes.
    pub fn detect_communities(&mut self) -> CommunityResult {
        let result = community::detect_communities(self);
        // Store community assignments on nodes.
        for (path, &community_id) in &result.assignments {
            if let Some(&idx) = self.path_to_node.get(path.as_str())
                && let Some(node) = self.graph.node_weight_mut(idx)
            {
                node.community = Some(community_id);
            }
        }
        result
    }

    // -----------------------------------------------------------------
    // Internal accessors used by community / surprise / html_export modules.
    // -----------------------------------------------------------------

    /// Return all real (non-external) file paths.
    pub fn file_paths(&self) -> Vec<String> {
        self.graph
            .node_weights()
            .filter(|n| !n.file_path.starts_with("__ext__:"))
            .map(|n| n.file_path.clone())
            .collect()
    }

    /// Return the number of file-level nodes (including external).
    pub fn file_node_count(&self) -> usize {
        self.graph.node_count()
    }

    /// Iterate over all edges as `(from_path, to_path, edge_ref)` triples.
    pub fn all_edges(&self) -> Vec<(&str, &str, &CodeEdge)> {
        self.graph
            .edge_references()
            .filter_map(|e| {
                let from = self.graph.node_weight(e.source())?;
                let to = self.graph.node_weight(e.target())?;
                Some((from.file_path.as_str(), to.file_path.as_str(), e.weight()))
            })
            .collect()
    }

    /// Return the set of neighbor paths (both directions) for a given path,
    /// ignoring external nodes. Used by Louvain community detection.
    pub fn undirected_neighbors(&self, file_path: &str) -> Vec<String> {
        let Some(&idx) = self.path_to_node.get(file_path) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        let incoming = self
            .graph
            .neighbors_directed(idx, petgraph::Direction::Incoming);
        let outgoing = self
            .graph
            .neighbors_directed(idx, petgraph::Direction::Outgoing);
        for nb in incoming.chain(outgoing) {
            if let Some(node) = self.graph.node_weight(nb)
                && !node.file_path.starts_with("__ext__:")
            {
                seen.insert(node.file_path.clone());
            }
        }
        seen.into_iter().collect()
    }

    /// Total number of edges between real (non-external) nodes.
    pub fn real_edge_count(&self) -> usize {
        self.graph
            .edge_references()
            .filter(|e| {
                let src = &self.graph[e.source()];
                let tgt = &self.graph[e.target()];
                !src.file_path.starts_with("__ext__:") && !tgt.file_path.starts_with("__ext__:")
            })
            .count()
    }

    /// Count edges between two specific file paths (in either direction).
    pub fn edges_between(&self, a: &str, b: &str) -> usize {
        let (Some(&a_idx), Some(&b_idx)) = (self.path_to_node.get(a), self.path_to_node.get(b))
        else {
            return 0;
        };
        self.graph
            .edge_references()
            .filter(|e| {
                (e.source() == a_idx && e.target() == b_idx)
                    || (e.source() == b_idx && e.target() == a_idx)
            })
            .count()
    }
}

impl Default for CodeGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph_has_zero_stats() {
        let g = CodeGraph::new();
        let s = g.stats();
        assert_eq!(s.node_count, 0);
        assert_eq!(s.edge_count, 0);
    }

    #[test]
    fn add_edge_creates_nodes() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        let s = g.stats();
        assert_eq!(s.node_count, 2);
        assert_eq!(s.edge_count, 1);
        assert_eq!(s.resolved_edges, 1);
    }

    #[test]
    fn callers_and_callees() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/main.rs",
            "src/parser.rs",
            "crate::parser",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/engine.rs",
            "src/parser.rs",
            "crate::parser",
            Language::Rust,
            Language::Rust,
        );

        let callers = g.callers("src/parser.rs");
        assert_eq!(callers.len(), 2);
        assert!(callers.contains(&"src/main.rs".to_string()));
        assert!(callers.contains(&"src/engine.rs".to_string()));

        let callees = g.callees("src/main.rs");
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0], "src/parser.rs");
    }

    #[test]
    fn remove_file_drops_edges() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.remove_file("src/b.rs");
        assert!(g.callees("src/a.rs").is_empty());
    }

    #[test]
    fn remove_file_edges_keeps_node() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.remove_file_edges("src/a.rs");
        // Node still exists, but edge is gone.
        assert!(g.node("src/a.rs").is_some());
        assert!(g.callees("src/a.rs").is_empty());
    }

    #[test]
    fn removing_edges_prunes_only_orphan_external_nodes() {
        let mut g = CodeGraph::new();
        g.add_external_edge("src/a.rs", "old_unique_import", Language::Rust);
        g.add_external_edge("src/b.rs", "shared_import", Language::Rust);
        g.add_external_edge("src/c.rs", "shared_import", Language::Rust);
        assert_eq!(g.file_node_count(), 5);

        g.remove_file_edges("src/a.rs");
        assert_eq!(g.file_node_count(), 4, "unique external node leaked");

        g.remove_file_edges("src/b.rs");
        assert_eq!(
            g.file_node_count(),
            4,
            "shared external node was removed while still referenced"
        );
        g.remove_file_edges("src/c.rs");
        assert_eq!(g.file_node_count(), 3, "last shared external node leaked");
    }

    #[test]
    fn removing_file_prunes_its_orphan_external_node() {
        let mut g = CodeGraph::new();
        g.add_external_edge("src/a.rs", "transient_import", Language::Rust);
        assert_eq!(g.file_node_count(), 2);

        g.remove_file("src/a.rs");
        assert_eq!(g.file_node_count(), 0);
    }

    #[test]
    fn flat_round_trip() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.add_external_edge("src/a.rs", "std::collections::HashMap", Language::Rust);

        let flat = g.to_flat();
        let g2 = CodeGraph::from_flat(flat);

        assert_eq!(g2.callees("src/a.rs"), vec!["src/b.rs"]);
    }

    #[test]
    fn symbol_callers_returns_call_edges() {
        let mut g = CodeGraph::new();
        let main_fn = g.add_symbol_with_line("main", "src/main.rs", types::SymbolKind::Function, 0);
        let helper = g.add_symbol_with_line("helper", "src/lib.rs", types::SymbolKind::Function, 5);
        let process =
            g.add_symbol_with_line("process", "src/engine.rs", types::SymbolKind::Function, 10);
        g.add_reference(main_fn, helper, types::ReferenceKind::Call);
        g.add_reference(process, helper, types::ReferenceKind::Call);

        let callers = g.get_symbol_callers("helper");
        assert_eq!(callers.len(), 2);
        // Callers should be (file, symbol_name) pairs.
        let names: Vec<&str> = callers.iter().map(|(_, n)| n.as_str()).collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"process"));
    }

    #[test]
    fn symbol_callees_returns_call_edges() {
        let mut g = CodeGraph::new();
        let main_fn = g.add_symbol_with_line("main", "src/main.rs", types::SymbolKind::Function, 0);
        let helper = g.add_symbol_with_line("helper", "src/lib.rs", types::SymbolKind::Function, 5);
        let process =
            g.add_symbol_with_line("process", "src/engine.rs", types::SymbolKind::Function, 10);
        g.add_reference(main_fn, helper, types::ReferenceKind::Call);
        g.add_reference(main_fn, process, types::ReferenceKind::Call);

        let callees = g.get_symbol_callees("main");
        assert_eq!(callees.len(), 2);
        assert!(callees.contains(&"helper".to_string()));
        assert!(callees.contains(&"process".to_string()));
    }

    #[test]
    fn symbol_node_count_tracks_additions() {
        let mut g = CodeGraph::new();
        assert_eq!(g.symbol_node_count(), 0);
        g.add_symbol_with_line("foo", "a.rs", types::SymbolKind::Function, 0);
        assert_eq!(g.symbol_node_count(), 1);
        g.add_symbol_with_line("bar", "b.rs", types::SymbolKind::Function, 5);
        assert_eq!(g.symbol_node_count(), 2);
    }

    #[test]
    fn remove_file_symbols_cleans_up() {
        let mut g = CodeGraph::new();
        g.add_symbol_with_line("foo", "a.rs", types::SymbolKind::Function, 0);
        g.add_symbol_with_line("bar", "a.rs", types::SymbolKind::Function, 10);
        g.add_symbol_with_line("baz", "b.rs", types::SymbolKind::Function, 0);
        assert_eq!(g.symbol_node_count(), 3);

        g.remove_file_symbols("a.rs");
        assert_eq!(g.symbol_node_count(), 1);
    }

    #[test]
    fn file_semantic_state_visits_only_owned_symbols_and_outgoing_edges() {
        let mut g = CodeGraph::new();
        let first =
            g.add_symbol_with_line("first", "src/target.rs", types::SymbolKind::Function, 1);
        let second =
            g.add_symbol_with_line("second", "src/target.rs", types::SymbolKind::Function, 2);
        g.add_reference(first, second, types::ReferenceKind::Call);

        let mut previous = None;
        for index in 0..10_000 {
            let node = g.add_symbol_with_line(
                &format!("unrelated_{index}"),
                &format!("src/unrelated_{index}.rs"),
                types::SymbolKind::Function,
                index,
            );
            if let Some(previous) = previous {
                g.add_reference(previous, node, types::ReferenceKind::Call);
            }
            previous = Some(node);
        }

        let mut visited_nodes = 0;
        let mut visited_edges = 0;
        let state = g.file_semantic_state_observing(
            "src/target.rs",
            || visited_nodes += 1,
            || visited_edges += 1,
        );

        assert_eq!(visited_nodes, 2);
        assert_eq!(visited_edges, 1);
        assert_eq!(state.symbol_nodes.len(), 2);
        assert_eq!(state.symbol_edges.len(), 1);
    }

    #[test]
    fn file_semantic_state_ignores_high_fan_in() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/target.rs", Language::Rust);
        let target =
            g.add_symbol_with_line("target", "src/target.rs", types::SymbolKind::Function, 1);
        let before = g.file_semantic_state("src/target.rs");

        for index in 0..10_000 {
            let caller_file = format!("src/caller_{index}.rs");
            g.add_edge(
                &caller_file,
                "src/target.rs",
                "target",
                Language::Rust,
                Language::Rust,
            );
            let caller = g.add_symbol_with_line(
                &format!("caller_{index}"),
                &caller_file,
                types::SymbolKind::Function,
                index,
            );
            g.add_reference(caller, target, types::ReferenceKind::Call);
        }

        assert_eq!(
            g.file_semantic_state("src/target.rs"),
            before,
            "incoming repository fan-in is not state owned by the target file"
        );
    }

    #[test]
    fn refresh_file_symbols_removes_retained_outgoing_edges_swap_safely() {
        let mut g = CodeGraph::new();
        let retained =
            g.add_symbol_with_line("retained", "src/edited.rs", types::SymbolKind::Function, 1);
        let first_target =
            g.add_symbol_with_line("first", "src/first.rs", types::SymbolKind::Function, 1);
        let unrelated = g.add_symbol_with_line(
            "unrelated",
            "src/unrelated.rs",
            types::SymbolKind::Function,
            1,
        );
        let unrelated_target = g.add_symbol_with_line(
            "unrelated_target",
            "src/unrelated_target.rs",
            types::SymbolKind::Function,
            1,
        );
        let second_target =
            g.add_symbol_with_line("second", "src/second.rs", types::SymbolKind::Function, 1);

        // Interleave an unrelated edge so removing the first retained edge
        // swap-moves another pending retained edge into its old slot.
        g.add_reference(retained, first_target, types::ReferenceKind::Call);
        g.add_reference(unrelated, unrelated_target, types::ReferenceKind::Call);
        g.add_reference(retained, second_target, types::ReferenceKind::Call);

        g.refresh_file_symbols(
            "src/edited.rs",
            &[FileSymbolDefinition {
                name: "retained".to_string(),
                kind: types::SymbolKind::Function,
                line: 2,
            }],
            &[],
        );

        assert!(g.get_symbol_callees("retained").is_empty());
        assert_eq!(
            g.get_symbol_callees("unrelated"),
            vec!["unrelated_target".to_string()]
        );
    }

    #[test]
    fn symbol_file_index_survives_swap_removal() {
        let mut g = CodeGraph::new();
        g.add_symbol("remove_a", "remove.rs", types::SymbolKind::Function);
        let keep_a = g.add_symbol("keep_a", "keep/a.rs", types::SymbolKind::Function);
        g.add_symbol("remove_b", "remove.rs", types::SymbolKind::Function);
        let keep_b = g.add_symbol("keep_b", "keep/b.rs", types::SymbolKind::Function);
        g.add_symbol("remove_c", "remove.rs", types::SymbolKind::Function);
        let keep_c = g.add_symbol("keep_c", "keep/c.rs", types::SymbolKind::Function);
        g.add_reference(keep_a, keep_b, types::ReferenceKind::Call);
        g.add_reference(keep_b, keep_c, types::ReferenceKind::Call);

        g.remove_file_symbols("remove.rs");

        assert_eq!(g.symbol_node_count(), 3);
        assert_eq!(g.file_semantic_state("keep/a.rs").symbol_nodes.len(), 1);
        assert_eq!(g.file_semantic_state("keep/b.rs").symbol_edges.len(), 1);
        assert_eq!(g.file_semantic_state("keep/c.rs").symbol_nodes.len(), 1);

        g.remove_file_symbols("keep/b.rs");
        assert_eq!(g.symbol_node_count(), 2);
        assert!(g.file_semantic_state("keep/b.rs").symbol_nodes.is_empty());
        assert!(g.file_semantic_state("keep/a.rs").symbol_edges.is_empty());
        assert!(g.file_semantic_state("keep/c.rs").symbol_edges.is_empty());
    }

    #[test]
    fn stats_includes_symbol_counts() {
        let mut g = CodeGraph::new();
        let a = g.add_symbol_with_line("a", "a.rs", types::SymbolKind::Function, 0);
        let b = g.add_symbol_with_line("b", "b.rs", types::SymbolKind::Function, 5);
        g.add_reference(a, b, types::ReferenceKind::Call);
        let s = g.stats();
        assert_eq!(s.symbol_nodes, 2);
        assert_eq!(s.symbol_edges, 1);
    }

    #[test]
    fn cross_imports_ranked_by_pagerank() {
        let mut g = CodeGraph::new();
        // Two gateway files import from two target files with different PageRank.
        g.add_edge(
            "src/gateway/a.rs",
            "src/auth/high_rank.rs",
            "crate::auth::high_rank",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/gateway/b.rs",
            "src/auth/low_rank.rs",
            "crate::auth::low_rank",
            Language::Rust,
            Language::Rust,
        );
        // Assign pagerank: high_rank.rs gets 0.5, low_rank.rs gets 0.001 (min).
        let mut scores = std::collections::HashMap::new();
        scores.insert("src/auth/high_rank.rs".to_string(), 0.5f32);
        scores.insert("src/auth/low_rank.rs".to_string(), 0.001f32);
        g.apply_pagerank(&scores);

        let ranked = g.cross_imports_ranked("src/gateway", "src/auth", None, None);
        assert_eq!(ranked.len(), 2);
        // gateway/a.rs imports the high-rank target, so it should score higher.
        assert_eq!(ranked[0].0, "src/gateway/a.rs");
        assert_eq!(ranked[1].0, "src/gateway/b.rs");
        assert!(ranked[0].1 > ranked[1].1);
    }

    #[test]
    fn cross_imports_ranked_excludes_call_edges_by_default() {
        // Regression for #101: a call-graph edge from a file in `from`
        // to a symbol defined under `to` must NOT register as a cross
        // import. Only import-graph edges (Resolved + External) should
        // count. Pre-fix the EZKeel report flagged
        // internal/web/sampler/sampler.go because a call edge happened
        // to land on cli/internal/* even though no actual import existed.
        let mut g = CodeGraph::new();
        g.add_edge(
            "internal/web/api/handler.go",
            "cli/internal/auth/jwt.go",
            "github.com/example/cli/internal/auth",
            Language::Go,
            Language::Go,
        );
        g.add_call_edge(
            "internal/web/sampler/sampler.go",
            "cli/internal/util/strings.go",
            "Sanitize",
            Language::Go,
            Language::Go,
        );

        let ranked = g.cross_imports_ranked("internal/web", "cli/internal", None, None);
        let names: Vec<&str> = ranked.iter().map(|(p, _)| p.as_str()).collect();
        assert!(
            names.contains(&"internal/web/api/handler.go"),
            "real importer must remain in results: {names:?}"
        );
        assert!(
            !names.contains(&"internal/web/sampler/sampler.go"),
            "call-only edge must not appear in cross-imports: {names:?}"
        );

        // Opt-in: kinds=[Calls] surfaces the call edge again.
        let with_calls = g.cross_imports_ranked_with_kinds(
            "internal/web",
            "cli/internal",
            None,
            None,
            &[EdgeKind::Resolved, EdgeKind::Calls],
        );
        let names_with_calls: Vec<&str> = with_calls.iter().map(|(p, _)| p.as_str()).collect();
        assert!(names_with_calls.contains(&"internal/web/sampler/sampler.go"));
    }

    #[test]
    fn cross_imports_evidence_applies_recency_boost() {
        // Regression for review of #101: when the CLI moved to the
        // evidence-aware path it lost the recency multiplier from
        // `cross_imports_ranked`, so older importers could outrank
        // freshly modified ones. The recency-aware evidence variant
        // restores parity with the score-only path.
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/gateway/fresh.rs",
            "src/auth/jwt.rs",
            "crate::auth::jwt",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/gateway/stale.rs",
            "src/auth/jwt.rs",
            "crate::auth::jwt",
            Language::Rust,
            Language::Rust,
        );

        // Equalize raw PageRank so recency is the only differentiator.
        let mut pr = std::collections::HashMap::new();
        pr.insert("src/auth/jwt.rs".to_string(), 0.5f32);
        g.apply_pagerank(&pr);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut recency = std::collections::HashMap::new();
        recency.insert("src/gateway/fresh.rs".to_string(), now); // today
        recency.insert("src/gateway/stale.rs".to_string(), now - 200 * 86_400); // ~7 months ago

        let ranked = g.cross_imports_ranked_with_evidence_recency(
            "src/gateway",
            "src/auth",
            Some(&recency),
            DEFAULT_IMPORT_BOUNDARY_KINDS,
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(
            ranked[0].0, "src/gateway/fresh.rs",
            "freshly modified importer should rank first"
        );
        assert!(ranked[0].1 > ranked[1].1);
    }

    #[test]
    fn cross_imports_evidence_includes_raw_import() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "internal/web/api/handler.go",
            "cli/internal/auth/jwt.go",
            "github.com/example/cli/internal/auth",
            Language::Go,
            Language::Go,
        );

        let evidence = g.cross_imports_ranked_with_evidence(
            "internal/web",
            "cli/internal",
            DEFAULT_IMPORT_BOUNDARY_KINDS,
        );
        assert_eq!(evidence.len(), 1);
        let (file, _score, edges) = &evidence[0];
        assert_eq!(file, "internal/web/api/handler.go");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].0, "cli/internal/auth/jwt.go");
        assert_eq!(edges[0].1, "github.com/example/cli/internal/auth");
        assert_eq!(edges[0].2, EdgeKind::Resolved);
    }

    #[test]
    fn cross_imports_ranked_respects_limit() {
        let mut g = CodeGraph::new();
        for i in 0..5 {
            g.add_edge(
                &format!("src/gateway/file_{i}.rs"),
                "src/auth/mod.rs",
                "crate::auth",
                Language::Rust,
                Language::Rust,
            );
        }

        let ranked = g.cross_imports_ranked("src/gateway", "src/auth", None, Some(3));
        assert_eq!(ranked.len(), 3);
    }

    // --- Edge confidence tests ---

    #[test]
    fn edge_confidence_auto_derived_from_kind() {
        assert_eq!(
            EdgeKind::Resolved.default_confidence(),
            EdgeConfidence::Verified
        );
        assert_eq!(EdgeKind::Calls.default_confidence(), EdgeConfidence::High);
        assert_eq!(
            EdgeKind::DocumentedBy.default_confidence(),
            EdgeConfidence::Medium
        );
        assert_eq!(EdgeKind::External.default_confidence(), EdgeConfidence::Low);
    }

    #[test]
    fn add_edge_sets_confidence() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        let flat = g.to_flat();
        assert_eq!(flat.edges.len(), 1);
        assert_eq!(flat.edges[0].2.confidence, EdgeConfidence::Verified);
    }

    #[test]
    fn call_edge_has_high_confidence() {
        let mut g = CodeGraph::new();
        g.add_call_edge(
            "src/a.rs",
            "src/b.rs",
            "foo",
            Language::Rust,
            Language::Rust,
        );
        let flat = g.to_flat();
        assert_eq!(flat.edges[0].2.confidence, EdgeConfidence::High);
    }

    #[test]
    fn doc_edge_has_medium_confidence() {
        let mut g = CodeGraph::new();
        g.add_doc_edge(
            "docs/guide.md",
            "src/engine.rs",
            "Engine",
            Language::Rust,
            Language::Rust,
        );
        let flat = g.to_flat();
        assert_eq!(flat.edges[0].2.confidence, EdgeConfidence::Medium);
    }

    #[test]
    fn stats_includes_confidence_counts() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );
        g.add_call_edge(
            "src/a.rs",
            "src/c.rs",
            "foo",
            Language::Rust,
            Language::Rust,
        );
        g.add_doc_edge("docs/x.md", "src/a.rs", "A", Language::Rust, Language::Rust);
        g.add_external_edge("src/a.rs", "serde", Language::Rust);

        let stats = g.stats();
        assert_eq!(stats.confidence_counts, (1, 1, 1, 1));
    }

    // --- Shortest path tests ---

    #[test]
    fn shortest_path_direct_edge() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/b.rs",
            "crate::b",
            Language::Rust,
            Language::Rust,
        );

        let path = g.shortest_path("src/a.rs", "src/b.rs");
        assert_eq!(
            path,
            Some(vec!["src/a.rs".to_string(), "src/b.rs".to_string()])
        );
    }

    #[test]
    fn shortest_path_multi_hop() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        g.add_edge("src/b.rs", "src/c.rs", "c", Language::Rust, Language::Rust);

        let path = g.shortest_path("src/a.rs", "src/c.rs");
        assert_eq!(
            path,
            Some(vec![
                "src/a.rs".to_string(),
                "src/b.rs".to_string(),
                "src/c.rs".to_string()
            ])
        );
    }

    #[test]
    fn shortest_path_same_node() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        let path = g.shortest_path("src/a.rs", "src/a.rs");
        assert_eq!(path, Some(vec!["src/a.rs".to_string()]));
    }

    #[test]
    fn shortest_path_no_path() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        g.get_or_insert_node("src/b.rs", Language::Rust);
        // No edge between them.
        let path = g.shortest_path("src/a.rs", "src/b.rs");
        assert_eq!(path, None);
    }

    #[test]
    fn shortest_path_missing_node() {
        let g = CodeGraph::new();
        assert_eq!(g.shortest_path("nonexistent.rs", "also_missing.rs"), None);
    }

    #[test]
    fn shortest_path_reverse_direction() {
        let mut g = CodeGraph::new();
        // Edge goes a -> b, but BFS is undirected so b -> a should work too.
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        let path = g.shortest_path("src/b.rs", "src/a.rs");
        assert_eq!(
            path,
            Some(vec!["src/b.rs".to_string(), "src/a.rs".to_string()])
        );
    }

    #[test]
    fn caller_count_deduplicates_parallel_edges() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/a.rs",
            "src/target.rs",
            "target",
            Language::Rust,
            Language::Rust,
        );
        g.add_call_edge(
            "src/a.rs",
            "src/target.rs",
            "target",
            Language::Rust,
            Language::Rust,
        );
        g.add_edge(
            "src/b.rs",
            "src/target.rs",
            "target",
            Language::Rust,
            Language::Rust,
        );

        assert_eq!(g.node("src/target.rs").unwrap().in_degree, 3);
        assert_eq!(g.caller_count("src/target.rs"), 2);
    }

    #[test]
    fn bounded_callees_is_exact_for_parallel_edges_below_cap() {
        let mut g = CodeGraph::new();
        g.add_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);
        g.add_call_edge("src/a.rs", "src/b.rs", "b", Language::Rust, Language::Rust);

        let (callees, degree) = g.bounded_callees("src/a.rs", 8);
        assert_eq!(callees, vec!["src/b.rs"]);
        assert_eq!(degree, 1, "parallel edges must share one transition");
    }

    #[test]
    fn bounded_callees_skip_noisy_and_external_edges() {
        let mut g = CodeGraph::new();
        for index in 0..6 {
            g.add_edge(
                "src/source.rs",
                &format!("src/callee_{index}.rs"),
                "callee",
                Language::Rust,
                Language::Rust,
            );
        }
        for _ in 0..300 {
            g.add_call_edge(
                "src/source.rs",
                "src/noisy.rs",
                "noisy",
                Language::Rust,
                Language::Rust,
            );
        }
        for index in 0..80 {
            g.add_external_edge(
                "src/source.rs",
                &format!("external_dependency_{index}"),
                Language::Rust,
            );
        }

        let (callees, degree) = g.bounded_callees("src/source.rs", 8);
        assert_eq!(callees.len(), 7);
        assert_eq!(degree, 7);
        assert!(callees.contains(&"src/noisy.rs".to_string()));
        assert!(callees.contains(&"src/callee_0.rs".to_string()));

        let (sampled, sampled_degree) = g.bounded_callees("src/source.rs", 6);
        assert_eq!(sampled.len(), 6);
        assert_eq!(
            sampled_degree, 7,
            "the exact distinct degree must preserve omitted transition mass"
        );
    }

    #[test]
    fn bounded_callees_never_walk_parallel_or_external_hub_edges() {
        let mut g = CodeGraph::new();
        g.add_edge(
            "src/source.rs",
            "src/useful.rs",
            "useful",
            Language::Rust,
            Language::Rust,
        );
        for _ in 0..MAX_BOUNDED_EDGE_SCANS * 3 {
            g.add_call_edge(
                "src/source.rs",
                "src/useful.rs",
                "useful",
                Language::Rust,
                Language::Rust,
            );
        }
        for index in 0..MAX_BOUNDED_EDGE_SCANS * 3 {
            g.add_external_edge(
                "src/source.rs",
                &format!("external_dependency_{index}"),
                Language::Rust,
            );
        }

        let (callees, degree, edge_visits) =
            g.bounded_callees_with_edge_visits("src/source.rs", 32);
        assert_eq!(callees, vec!["src/useful.rs"]);
        assert_eq!(degree, 1, "parallel and external edges are not transitions");
        assert_eq!(
            edge_visits, 1,
            "bounded traversal must visit the derived distinct adjacency only"
        );
    }

    #[test]
    fn bounded_adjacency_is_deterministic_above_cap() {
        fn sample(reverse: bool) -> (Vec<String>, usize) {
            let mut graph = CodeGraph::new();
            let total = MAX_BOUNDED_EDGE_SCANS + 2;
            let indices: Box<dyn Iterator<Item = usize>> = if reverse {
                Box::new((0..total).rev())
            } else {
                Box::new(0..total)
            };
            for index in indices {
                graph.add_edge(
                    "src/source.rs",
                    &format!("src/callee_{index:05}.rs"),
                    "callee",
                    Language::Rust,
                    Language::Rust,
                );
            }
            graph.bounded_callees("src/source.rs", usize::MAX)
        }

        let forward = sample(false);
        let reverse = sample(true);
        assert_eq!(forward, reverse, "sample must not depend on edge order");
        assert_eq!(forward.0.len(), MAX_BOUNDED_EDGE_SCANS);
        assert_eq!(forward.1, MAX_BOUNDED_EDGE_SCANS + 2);
        assert_eq!(forward.0.first().unwrap(), "src/callee_00000.rs");
        assert_eq!(
            forward.0.last().unwrap(),
            &format!("src/callee_{:05}.rs", MAX_BOUNDED_EDGE_SCANS - 1)
        );
    }

    #[test]
    fn bounded_adjacency_backfills_sampled_removals() {
        let total = MAX_BOUNDED_EDGE_SCANS + 2;
        let mut outgoing = CodeGraph::new();
        for index in 0..total {
            outgoing.add_edge(
                "src/source.rs",
                &format!("src/callee_{index:05}.rs"),
                "callee",
                Language::Rust,
                Language::Rust,
            );
        }
        outgoing.remove_file("src/callee_00000.rs");
        let (callees, degree) = outgoing.bounded_callees("src/source.rs", usize::MAX);
        assert_eq!(degree, total - 1);
        assert_eq!(callees.len(), MAX_BOUNDED_EDGE_SCANS);
        assert_eq!(callees.first().unwrap(), "src/callee_00001.rs");
        assert_eq!(
            callees.last().unwrap(),
            &format!("src/callee_{:05}.rs", MAX_BOUNDED_EDGE_SCANS)
        );

        let mut incoming = CodeGraph::new();
        for index in 0..total {
            incoming.add_edge(
                &format!("src/caller_{index:05}.rs"),
                "src/target.rs",
                "target",
                Language::Rust,
                Language::Rust,
            );
        }
        incoming.remove_file("src/caller_00000.rs");
        let callers = incoming.bounded_callers("src/target.rs", usize::MAX);
        assert_eq!(incoming.caller_count("src/target.rs"), total - 1);
        assert_eq!(callers.len(), MAX_BOUNDED_EDGE_SCANS);
        assert_eq!(callers.first().unwrap(), "src/caller_00001.rs");
        assert_eq!(
            callers.last().unwrap(),
            &format!("src/caller_{:05}.rs", MAX_BOUNDED_EDGE_SCANS)
        );
    }

    #[test]
    fn bounded_callers_and_count_cap_dependency_hubs() {
        let mut g = CodeGraph::new();
        for i in 0..6 {
            let caller = format!("src/caller_{i}.rs");
            g.add_edge(
                &caller,
                "src/target.rs",
                "target",
                Language::Rust,
                Language::Rust,
            );
        }
        g.add_call_edge(
            "src/caller_0.rs",
            "src/target.rs",
            "target",
            Language::Rust,
            Language::Rust,
        );

        let callers = g.bounded_callers("src/target.rs", 3);
        assert_eq!(callers.len(), 3);
        assert!(callers.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(g.caller_count("src/target.rs"), 6);
    }

    #[test]
    fn bounded_callers_skip_parallel_edges_without_fabricating_count() {
        let mut g = CodeGraph::new();
        for index in 0..6 {
            g.add_edge(
                &format!("src/caller_{index}.rs"),
                "src/target.rs",
                "target",
                Language::Rust,
                Language::Rust,
            );
        }
        // petgraph visits the newest incoming edges first. The bounded helpers
        // must skip these parallel edges and continue until they have observed
        // the requested number of distinct callers or exhausted the adjacency.
        for _ in 0..300 {
            g.add_call_edge(
                "src/noisy.rs",
                "src/target.rs",
                "target",
                Language::Rust,
                Language::Rust,
            );
        }

        assert_eq!(g.caller_count("src/target.rs"), 7);
        assert_eq!(g.caller_count("src/target.rs"), 7);
        assert_eq!(g.bounded_callers("src/target.rs", 6).len(), 6);
        assert_eq!(g.bounded_callers("src/target.rs", 8).len(), 7);
        assert_eq!(g.bounded_callers("src/target.rs", 6).len(), 6);
    }

    // --- Community field on node ---

    #[test]
    fn community_defaults_to_none() {
        let mut g = CodeGraph::new();
        g.get_or_insert_node("src/a.rs", Language::Rust);
        assert_eq!(g.node("src/a.rs").unwrap().community, None);
    }
}
