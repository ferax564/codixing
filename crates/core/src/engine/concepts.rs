//! Semantic concept graph: maps domain concepts to symbol clusters.
//!
//! Bridges the vocabulary gap between natural-language queries and code
//! identifiers by clustering related symbols under shared domain concepts.
//!
//! Three concept sources feed the graph:
//!
//! 1. **Doc comment mining** — extracts meaningful words from documentation
//!    and maps them as concept labels for the documented symbol.
//! 2. **Import co-occurrence** — files that import each other share concepts,
//!    expanding cluster file sets with co-occurring files.
//! 3. **Identifier decomposition** — splits `camelCase`/`snake_case`
//!    identifiers into parts and groups symbols sharing common parts.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Stop words for doc comment mining
// ---------------------------------------------------------------------------

/// Common English stop words plus code-doc noise words.
/// Used by `extract_concept_words` to filter non-discriminative terms.
const STOP_WORDS: &[&str] = &[
    // Articles, prepositions, conjunctions
    "the", "a", "an", "in", "of", "for", "to", "on", "at", "by", "with", "from", "as", "is", "it",
    "or", "and", "but", "not", "if", "be", "are", "was", "has", "had", "have", "will", "can",
    "may", "do", "did", "its", "this", "that", "than", "then", "so", "no", "all",
    // Code doc noise
    "returns", "return", "see", "also", "note", "todo", "fixme", "hack", "xxx", "param", "type",
    "self", "none", "true", "false", "some", "err", "new", "use", "used", "uses", "using", "get",
    "set", "into", "which", "when", "each", "given", "whether", "should", "must", "will", "would",
    "could", "about", "been", "more", "only", "other", "such",
];

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// A cluster of related symbols sharing a domain concept.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConceptCluster {
    /// Concept label, e.g. "authentication", "caching".
    pub name: String,
    /// Symbol names belonging to this cluster, e.g. `["login", "verify_token", "AuthGuard"]`.
    pub symbols: Vec<String>,
    /// File paths containing these symbols, e.g. `["src/auth.rs", "src/middleware.rs"]`.
    pub files: Vec<String>,
    /// Confidence/cohesion score in `[0.0, 1.0]`.
    pub score: f32,
}

/// Inverted index from concept terms to symbol clusters.
///
/// Produced by [`ConceptIndexBuilder::build`] and serializable for persistence.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConceptIndex {
    /// All clusters in insertion order.
    pub clusters: Vec<ConceptCluster>,
    /// Term → indices into `clusters`.
    term_to_clusters: HashMap<String, Vec<usize>>,
}

impl ConceptIndex {
    /// Look up a single term and return matching clusters.
    pub fn lookup(&self, term: &str) -> Vec<&ConceptCluster> {
        let key = term.to_lowercase();
        match self.term_to_clusters.get(&key) {
            Some(indices) => indices
                .iter()
                .filter_map(|&i| self.clusters.get(i))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Multi-word query lookup: returns `(cluster, hit_count)` pairs sorted by
    /// descending hit count.
    ///
    /// Each query word is looked up independently; clusters are ranked by how
    /// many distinct query words matched them.
    pub fn lookup_query(&self, query: &str) -> Vec<(&ConceptCluster, usize)> {
        let words: Vec<String> = query.split_whitespace().map(|w| w.to_lowercase()).collect();

        if words.is_empty() {
            return Vec::new();
        }

        // cluster_index → number of distinct query words that hit it
        let mut hit_counts: HashMap<usize, usize> = HashMap::new();
        for word in &words {
            if let Some(indices) = self.term_to_clusters.get(word.as_str()) {
                for &idx in indices {
                    *hit_counts.entry(idx).or_insert(0) += 1;
                }
            }
        }

        let mut results: Vec<(&ConceptCluster, usize)> = hit_counts
            .into_iter()
            .filter_map(|(idx, count)| self.clusters.get(idx).map(|c| (c, count)))
            .collect();

        results.sort_by_key(|b| std::cmp::Reverse(b.1));
        results
    }

    /// Returns `true` if the index contains no clusters.
    pub fn is_empty(&self) -> bool {
        self.clusters.is_empty()
    }

    /// Encode the v2 persisted representation. Strings are interned once and
    /// all maps are emitted in lexical order, cutting repeated path/symbol
    /// storage while making bytes reproducible across process hash seeds.
    pub(super) fn encode_persisted(&self) -> std::result::Result<Vec<u8>, String> {
        let compact = CompactConceptIndex::from_index(self)?;
        let payload = bitcode::serialize(&compact).map_err(|error| error.to_string())?;
        let mut bytes = Vec::with_capacity(CONCEPT_FORMAT_MAGIC.len() + payload.len());
        bytes.extend_from_slice(CONCEPT_FORMAT_MAGIC);
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// Decode v2, falling back to the legacy directly-serialized structure so
    /// existing indexes remain readable until the next sync rewrites them.
    pub(super) fn decode_persisted(bytes: &[u8]) -> std::result::Result<Self, String> {
        if let Some(payload) = bytes.strip_prefix(CONCEPT_FORMAT_MAGIC) {
            let compact: CompactConceptIndex =
                bitcode::deserialize(payload).map_err(|error| error.to_string())?;
            compact.into_index()
        } else {
            bitcode::deserialize(bytes).map_err(|error| error.to_string())
        }
    }
}

const CONCEPT_FORMAT_MAGIC: &[u8] = b"CXCP2\0";

#[derive(Debug, Serialize, Deserialize)]
struct CompactConceptCluster {
    name: u32,
    symbols: Vec<u32>,
    files: Vec<u32>,
    score: f32,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompactConceptIndex {
    strings: Vec<String>,
    clusters: Vec<CompactConceptCluster>,
    terms: Vec<(u32, Vec<u32>)>,
}

impl CompactConceptIndex {
    fn from_index(index: &ConceptIndex) -> std::result::Result<Self, String> {
        let mut strings = BTreeSet::new();
        for cluster in &index.clusters {
            strings.insert(cluster.name.clone());
            strings.extend(cluster.symbols.iter().cloned());
            strings.extend(cluster.files.iter().cloned());
        }
        strings.extend(index.term_to_clusters.keys().cloned());
        let strings: Vec<String> = strings.into_iter().collect();
        let ids: BTreeMap<&str, u32> = strings
            .iter()
            .enumerate()
            .map(|(idx, value)| {
                u32::try_from(idx)
                    .map(|idx| (value.as_str(), idx))
                    .map_err(|_| "concept string table exceeds u32".to_string())
            })
            .collect::<std::result::Result<_, _>>()?;
        let id = |value: &str| {
            ids.get(value)
                .copied()
                .ok_or_else(|| format!("missing interned concept string: {value}"))
        };
        let clusters = index
            .clusters
            .iter()
            .map(|cluster| {
                Ok(CompactConceptCluster {
                    name: id(&cluster.name)?,
                    symbols: cluster
                        .symbols
                        .iter()
                        .map(|value| id(value))
                        .collect::<std::result::Result<_, _>>()?,
                    files: cluster
                        .files
                        .iter()
                        .map(|value| id(value))
                        .collect::<std::result::Result<_, _>>()?,
                    score: cluster.score,
                })
            })
            .collect::<std::result::Result<_, String>>()?;
        let mut term_entries: Vec<_> = index.term_to_clusters.iter().collect();
        term_entries.sort_by(|a, b| a.0.cmp(b.0));
        let terms = term_entries
            .into_iter()
            .map(|(term, clusters)| {
                let clusters = clusters
                    .iter()
                    .map(|&idx| {
                        u32::try_from(idx)
                            .map_err(|_| "concept cluster index exceeds u32".to_string())
                    })
                    .collect::<std::result::Result<_, _>>()?;
                Ok((id(term)?, clusters))
            })
            .collect::<std::result::Result<_, String>>()?;
        Ok(Self {
            strings,
            clusters,
            terms,
        })
    }

    fn into_index(self) -> std::result::Result<ConceptIndex, String> {
        let resolve = |id: u32| {
            self.strings
                .get(id as usize)
                .cloned()
                .ok_or_else(|| format!("concept string id {id} is out of bounds"))
        };
        let clusters = self
            .clusters
            .into_iter()
            .map(|cluster| {
                Ok(ConceptCluster {
                    name: resolve(cluster.name)?,
                    symbols: cluster
                        .symbols
                        .into_iter()
                        .map(resolve)
                        .collect::<std::result::Result<_, _>>()?,
                    files: cluster
                        .files
                        .into_iter()
                        .map(resolve)
                        .collect::<std::result::Result<_, _>>()?,
                    score: cluster.score,
                })
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        let mut term_to_clusters = HashMap::new();
        for (term, indices) in self.terms {
            let term = resolve(term)?;
            let mut decoded = Vec::with_capacity(indices.len());
            for idx in indices {
                let idx = idx as usize;
                if idx >= clusters.len() {
                    return Err(format!("concept cluster id {idx} is out of bounds"));
                }
                decoded.push(idx);
            }
            term_to_clusters.insert(term, decoded);
        }
        Ok(ConceptIndex {
            clusters,
            term_to_clusters,
        })
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Intermediate symbol record collected before clustering.
#[derive(Debug)]
struct SymbolRecord {
    name: String,
    file: String,
    doc_words: Vec<String>,
    part_count: usize,
}

/// Safety limits for the derived semantic concept artifact.
///
/// These caps are deliberately fixed invariants. Concept data is an auxiliary
/// search signal, so allowing generated identifiers or documentation to grow it
/// without bound is a poor trade: the primary BM25/symbol indexes remain
/// complete while this layer keeps the strongest, most discriminative evidence.
pub const MAX_CONCEPT_SOURCE_SYMBOLS: usize = 100_000;
pub const MAX_CONCEPT_CLUSTERS: usize = 16_384;
pub const MAX_SYMBOLS_PER_CLUSTER: usize = 32;
pub const MAX_FILES_PER_CLUSTER: usize = 16;
pub const MAX_DOC_WORDS_PER_SYMBOL: usize = 8;
pub const MAX_IDENTIFIER_PARTS_PER_SYMBOL: usize = 8;
pub const MAX_CONCEPT_TERMS: usize = 32_768;
pub const MAX_CLUSTERS_PER_TERM: usize = 8;
const MAX_CONCEPT_WORD_BYTES: usize = 64;
const MAX_COOCCURRENCES: usize = 262_144;
const MAX_EMBEDDING_BUCKET_SIZE: usize = 64;

/// Phase-4 embedding-cluster merge parameters.
///
/// Tuned on the openclaw repo during the v0.40 concept-graph work:
/// τ = 0.82 keeps merges tight enough that false-positive cluster unions are
/// rare while still catching synonym pairs like `login / authenticate`.
/// Cap = 32 prevents runaway single-link chains.
#[derive(Debug, Clone, Copy)]
pub struct EmbedClusterConfig {
    pub threshold: f32,
    pub cap: usize,
}

impl Default for EmbedClusterConfig {
    fn default() -> Self {
        Self {
            threshold: 0.82,
            cap: 32,
        }
    }
}

/// Builds a [`ConceptIndex`] from symbol data and co-occurrence information.
#[derive(Debug, Default)]
pub struct ConceptIndexBuilder {
    symbols: Vec<SymbolRecord>,
    /// Pairs of files that import each other / co-occur.
    cooccurrences: Vec<(String, String)>,
    /// Phase-4 state. `None` = merge disabled (Phase 4 is a no-op).
    embed_cluster: Option<(HashMap<String, Vec<f32>>, EmbedClusterConfig)>,
}

impl ConceptIndexBuilder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a symbol with its file location and optional doc comment.
    pub fn add_symbol(&mut self, name: &str, file: &str, doc_comment: Option<&str>) {
        let doc_words = doc_comment
            .map(|doc| extract_ranked_concept_words(doc, MAX_DOC_WORDS_PER_SYMBOL))
            .unwrap_or_default();
        self.symbols.push(SymbolRecord {
            name: name.to_string(),
            file: file.to_string(),
            doc_words,
            part_count: decompose_identifier(name).len(),
        });
        // Prune in batches so peak construction memory is at most twice the
        // documented source cap. Richly documented/decomposable symbols rank
        // ahead of generated one-part names; lexical ties are deterministic.
        if self.symbols.len() >= MAX_CONCEPT_SOURCE_SYMBOLS.saturating_mul(2) {
            retain_best_symbol_records(&mut self.symbols);
        }
    }

    /// Return the file set from the bounded source reservoir. This lets graph
    /// enrichment scan only files that can still contribute a concept cluster.
    pub(super) fn source_files(&mut self) -> Vec<String> {
        retain_best_symbol_records(&mut self.symbols);
        self.symbols
            .iter()
            .map(|record| record.file.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Record that two files co-occur (e.g. one imports the other).
    pub fn add_cooccurrence(&mut self, file_a: &str, file_b: &str) {
        self.cooccurrences
            .push((file_a.to_string(), file_b.to_string()));
        if self.cooccurrences.len() >= MAX_COOCCURRENCES.saturating_mul(2) {
            retain_best_cooccurrences(&mut self.cooccurrences);
        }
    }

    /// Enable Phase 4 cluster merging with the supplied symbol embeddings
    /// and default [`EmbedClusterConfig`] (τ = 0.82, cap = 32).
    ///
    /// If this method is not called (or an empty map is passed), Phase 4 is
    /// a no-op and the output is identical to the pre-v0.40 three-phase
    /// build — the `embedding.enabled = false` code path is unchanged.
    pub fn with_symbol_embeddings(self, embeddings: HashMap<String, Vec<f32>>) -> Self {
        self.with_embed_cluster(embeddings, EmbedClusterConfig::default())
    }

    /// Enable Phase 4 cluster merging with explicit configuration.
    pub fn with_embed_cluster(
        mut self,
        embeddings: HashMap<String, Vec<f32>>,
        config: EmbedClusterConfig,
    ) -> Self {
        if embeddings.is_empty() {
            self.embed_cluster = None;
        } else {
            self.embed_cluster = Some((embeddings, config));
        }
        self
    }

    /// Consume the builder and produce a [`ConceptIndex`].
    ///
    /// Clustering proceeds in three phases:
    ///
    /// 1. **Identifier decomposition** — split every symbol name into parts and
    ///    group by shared parts (skip singletons).
    /// 2. **Doc comment concepts** — add doc-derived concept words to the
    ///    clusters containing the documented symbol.
    /// 3. **Import co-occurrence** — expand each cluster's file set with
    ///    co-occurring files.
    pub fn build(mut self) -> ConceptIndex {
        retain_best_symbol_records(&mut self.symbols);
        retain_best_cooccurrences(&mut self.cooccurrences);
        self.symbols.sort_by(|a, b| {
            (&a.file, &a.name, &a.doc_words).cmp(&(&b.file, &b.name, &b.doc_words))
        });
        self.symbols
            .dedup_by(|a, b| a.file == b.file && a.name == b.name);
        if self.symbols.is_empty() {
            return ConceptIndex::default();
        }

        let total_symbols = self.symbols.len();

        // -----------------------------------------------------------------
        // Phase 1: Group symbols by shared decomposed identifier parts
        // -----------------------------------------------------------------

        // term → list of symbol indices that contain this term
        let mut term_to_symbol_indices: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, rec) in self.symbols.iter().enumerate() {
            let mut parts = decompose_identifier(&rec.name);
            parts.sort();
            parts.dedup();
            parts.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
            parts.truncate(MAX_IDENTIFIER_PARTS_PER_SYMBOL);
            for part in parts {
                term_to_symbol_indices.entry(part).or_default().push(idx);
            }
        }

        // Build clusters from shared terms (skip singletons)
        let mut clusters: Vec<ConceptCluster> = Vec::new();
        let mut seen_term_sets: HashMap<String, usize> = HashMap::new(); // cluster dedup

        let mut identifier_terms: Vec<(&String, &Vec<usize>)> =
            term_to_symbol_indices.iter().collect();
        identifier_terms.sort_by(|a, b| {
            b.1.len()
                .cmp(&a.1.len())
                .then_with(|| b.0.len().cmp(&a.0.len()))
                .then_with(|| a.0.cmp(b.0))
        });
        identifier_terms.truncate(MAX_CONCEPT_CLUSTERS / 2);

        for (term, sym_indices) in identifier_terms {
            if sym_indices.len() < 2 {
                continue; // skip singleton terms
            }

            // Dedup key: sorted symbol indices
            let mut sorted_indices = sym_indices.clone();
            sorted_indices.sort_unstable();
            sorted_indices.dedup();
            ranked_symbol_indices(&mut sorted_indices, &self.symbols);
            sorted_indices.truncate(MAX_SYMBOLS_PER_CLUSTER);
            sorted_indices.sort_unstable();
            let dedup_key = format!("{sorted_indices:?}");

            if let Some(&existing_idx) = seen_term_sets.get(&dedup_key) {
                // Same symbol set — just add this term as an alias (it will be
                // indexed in the term_to_clusters map later)
                let cluster = &mut clusters[existing_idx];
                // The cluster name stays as the first term found; we just need
                // the term→cluster mapping which happens in Phase 4 below.
                let _ = cluster; // no-op, mapping handled later
                continue;
            }

            let mut symbol_names: Vec<String> = sorted_indices
                .iter()
                .map(|&i| self.symbols[i].name.clone())
                .collect();
            symbol_names.sort();
            symbol_names.dedup();

            let mut file_set: Vec<String> = sorted_indices
                .iter()
                .map(|&i| self.symbols[i].file.clone())
                .collect();
            file_set.sort_unstable();
            file_set.dedup();

            let score = (symbol_names.len() as f32 / total_symbols as f32).min(1.0);

            let cluster_idx = clusters.len();
            seen_term_sets.insert(dedup_key, cluster_idx);

            clusters.push(ConceptCluster {
                name: term.clone(),
                symbols: symbol_names,
                files: file_set,
                score,
            });
        }

        // -----------------------------------------------------------------
        // Phase 2: Doc comment concepts — enrich clusters with doc words
        // -----------------------------------------------------------------

        // For each symbol that has doc words, find which cluster(s) it belongs
        // to and add the doc words as additional cluster concept terms.
        // Also create new clusters for doc terms that group multiple symbols.
        let mut doc_term_to_symbols: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, rec) in self.symbols.iter().enumerate() {
            for word in &rec.doc_words {
                doc_term_to_symbols
                    .entry(word.clone())
                    .or_default()
                    .push(idx);
            }
        }

        let mut doc_terms: Vec<(&String, &Vec<usize>)> = doc_term_to_symbols.iter().collect();
        doc_terms.sort_by(|a, b| {
            b.1.len()
                .cmp(&a.1.len())
                .then_with(|| b.0.len().cmp(&a.0.len()))
                .then_with(|| a.0.cmp(b.0))
        });
        doc_terms.truncate(MAX_CONCEPT_CLUSTERS / 2);

        for (doc_term, sym_indices) in doc_terms {
            if sym_indices.len() < 2 {
                // Even single-symbol doc terms are useful — they create a
                // concept→symbol bridge that wouldn't exist from identifiers
                // alone. But we still skip if the term is already covered.
            }

            let mut sorted_indices = sym_indices.clone();
            sorted_indices.sort_unstable();
            sorted_indices.dedup();
            ranked_symbol_indices(&mut sorted_indices, &self.symbols);
            sorted_indices.truncate(MAX_SYMBOLS_PER_CLUSTER);
            sorted_indices.sort_unstable();

            // Check if a cluster with exactly this symbol set already exists
            let dedup_key = format!("{sorted_indices:?}");
            if seen_term_sets.contains_key(&dedup_key) {
                continue; // already covered by an identifier-based cluster
            }

            let mut symbol_names: Vec<String> = sorted_indices
                .iter()
                .map(|&i| self.symbols[i].name.clone())
                .collect();
            symbol_names.sort();
            symbol_names.dedup();

            let mut file_set: Vec<String> = sorted_indices
                .iter()
                .map(|&i| self.symbols[i].file.clone())
                .collect();
            file_set.sort_unstable();
            file_set.dedup();

            let score = (symbol_names.len() as f32 / total_symbols as f32).min(1.0);

            let cluster_idx = clusters.len();
            seen_term_sets.insert(dedup_key, cluster_idx);

            clusters.push(ConceptCluster {
                name: doc_term.clone(),
                symbols: symbol_names,
                files: file_set,
                score,
            });
            if clusters.len() >= MAX_CONCEPT_CLUSTERS {
                break;
            }
        }

        // -----------------------------------------------------------------
        // Phase 3: Import co-occurrence — expand cluster file sets
        // -----------------------------------------------------------------

        // Build a file → co-occurring files map
        let mut cooccur_map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (a, b) in &self.cooccurrences {
            cooccur_map.entry(a.as_str()).or_default().push(b.as_str());
            cooccur_map.entry(b.as_str()).or_default().push(a.as_str());
        }

        for cluster in &mut clusters {
            let mut expanded_files = cluster.files.clone();
            for file in &cluster.files {
                if let Some(neighbors) = cooccur_map.get(file.as_str()) {
                    for &neighbor in neighbors {
                        expanded_files.push(neighbor.to_string());
                    }
                }
            }
            expanded_files.sort_unstable();
            expanded_files.dedup();
            expanded_files.truncate(MAX_FILES_PER_CLUSTER);
            cluster.files = expanded_files;
        }

        // -----------------------------------------------------------------
        // Phase 4: Embedding-based cluster merge (v0.40)
        // -----------------------------------------------------------------
        //
        // Deterministic sign-projection buckets with bounded local comparisons
        // replace the former all-pairs scan. `cap` still limits the combined
        // symbol count of any merged cluster.
        if let Some((embeddings, config)) = self.embed_cluster.as_ref() {
            merge_clusters_by_embedding(&mut clusters, embeddings, config.threshold, config.cap);
        }

        clusters.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.symbols.cmp(&b.symbols))
                .then_with(|| a.files.cmp(&b.files))
        });
        clusters.truncate(MAX_CONCEPT_CLUSTERS);

        // -----------------------------------------------------------------
        // Phase 5: Build term → cluster index
        // -----------------------------------------------------------------
        //
        // Phase 4 may have removed or renumbered clusters, so the
        // pre-merge `seen_term_sets` (dedup_key → cluster_idx) is no longer
        // a reliable lookup. Build a fresh `symbol_name → Vec<cluster_idx>`
        // map from the final clusters and drive Phase 5 off of it; the
        // dedup_key path becomes a fallback for the pre-merge (no Phase 4)
        // case when a symbol belongs to a single cluster only.

        let mut term_to_clusters: HashMap<String, Vec<usize>> = HashMap::new();

        // Post-merge symbol → cluster lookup, used to translate pre-merge
        // symbol-index-sets into the current cluster numbering.
        let mut name_to_clusters: HashMap<&str, Vec<usize>> = HashMap::new();
        for (idx, cluster) in clusters.iter().enumerate() {
            for name in &cluster.symbols {
                name_to_clusters.entry(name.as_str()).or_default().push(idx);
            }
        }

        // Return only clusters that contain EVERY name in `sym_indices` —
        // matches the pre-merge `seen_term_sets` dedup-key semantics so
        // lookups land on the one cluster that actually groups these
        // symbols, not every cluster that contains any single member.
        let resolve_clusters = |sym_indices: &[usize]| -> Vec<usize> {
            let target_count = sym_indices.len();
            if target_count == 0 {
                return Vec::new();
            }
            let mut cluster_counts: HashMap<usize, usize> = HashMap::new();
            for &sym_idx in sym_indices {
                if let Some(name) = self.symbols.get(sym_idx).map(|s| s.name.as_str())
                    && let Some(cluster_indices) = name_to_clusters.get(name)
                {
                    for &c_idx in cluster_indices {
                        *cluster_counts.entry(c_idx).or_insert(0) += 1;
                    }
                }
            }
            let mut out: Vec<usize> = cluster_counts
                .into_iter()
                .filter_map(|(c_idx, count)| (count == target_count).then_some(c_idx))
                .collect();
            out.sort_unstable();
            out
        };

        for (idx, cluster) in clusters.iter().enumerate() {
            // Index by cluster name
            term_to_clusters
                .entry(cluster.name.clone())
                .or_default()
                .push(idx);

            // Index by each symbol's decomposed parts
            for sym_name in &cluster.symbols {
                for part in decompose_identifier(sym_name) {
                    term_to_clusters.entry(part).or_default().push(idx);
                }
            }
        }

        // Also index doc terms → their (possibly merged) clusters.
        for (doc_term, sym_indices) in &doc_term_to_symbols {
            for cluster_idx in resolve_clusters(sym_indices) {
                term_to_clusters
                    .entry(doc_term.clone())
                    .or_default()
                    .push(cluster_idx);
            }
        }

        // Index all identifier-decomposition terms → their clusters.
        for (term, sym_indices) in &term_to_symbol_indices {
            if sym_indices.len() < 2 {
                continue;
            }
            for cluster_idx in resolve_clusters(sym_indices) {
                term_to_clusters
                    .entry(term.clone())
                    .or_default()
                    .push(cluster_idx);
            }
        }

        // `seen_term_sets` is no longer read after Phase 4; silence unused warning.
        let _ = seen_term_sets;

        // Dedup the cluster index lists
        // Rank and cap every posting list. Generic terms no longer point at
        // thousands of clusters, but retain the highest-cohesion matches.
        for indices in term_to_clusters.values_mut() {
            indices.sort_by(|&a, &b| {
                clusters[b]
                    .score
                    .total_cmp(&clusters[a].score)
                    .then_with(|| clusters[a].name.cmp(&clusters[b].name))
            });
            indices.dedup();
            indices.truncate(MAX_CLUSTERS_PER_TERM);
        }

        if term_to_clusters.len() > MAX_CONCEPT_TERMS {
            let mut ranked: Vec<(String, Vec<usize>)> = term_to_clusters.into_iter().collect();
            ranked.sort_by(|a, b| {
                b.1.len()
                    .cmp(&a.1.len())
                    .then_with(|| b.0.len().cmp(&a.0.len()))
                    .then_with(|| a.0.cmp(&b.0))
            });
            ranked.truncate(MAX_CONCEPT_TERMS);
            term_to_clusters = ranked.into_iter().collect();
        }

        ConceptIndex {
            clusters,
            term_to_clusters,
        }
    }
}

fn retain_best_symbol_records(records: &mut Vec<SymbolRecord>) {
    if records.len() <= MAX_CONCEPT_SOURCE_SYMBOLS {
        return;
    }
    records.sort_by(|a, b| {
        b.doc_words
            .len()
            .cmp(&a.doc_words.len())
            .then_with(|| b.part_count.cmp(&a.part_count))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.doc_words.cmp(&b.doc_words))
    });
    records.truncate(MAX_CONCEPT_SOURCE_SYMBOLS);
}

fn retain_best_cooccurrences(cooccurrences: &mut Vec<(String, String)>) {
    for (a, b) in cooccurrences.iter_mut() {
        if b < a {
            std::mem::swap(a, b);
        }
    }
    cooccurrences.sort();
    cooccurrences.dedup();
    cooccurrences.truncate(MAX_COOCCURRENCES);
}

fn ranked_symbol_indices(indices: &mut [usize], symbols: &[SymbolRecord]) {
    indices.sort_by(|&a, &b| {
        symbols[b]
            .doc_words
            .len()
            .cmp(&symbols[a].doc_words.len())
            .then_with(|| symbols[a].name.cmp(&symbols[b].name))
            .then_with(|| symbols[a].file.cmp(&symbols[b].file))
    });
}

/// Bounded embedding merge on concept clusters using chunk embeddings.
///
/// Centroids are grouped by a deterministic sign-projection bucket and compared
/// in chunks of at most [`MAX_EMBEDDING_BUCKET_SIZE`]. This retains the useful
/// synonym merge while replacing the old unbounded `O(C^2)` all-pairs scan with
/// `O(C * MAX_EMBEDDING_BUCKET_SIZE)` comparisons.
///
/// Symbols without an embedding vector in `embeddings` are skipped — their
/// cluster still participates but contributes no centroid information.
fn merge_clusters_by_embedding(
    clusters: &mut Vec<ConceptCluster>,
    embeddings: &HashMap<String, Vec<f32>>,
    threshold: f32,
    cap: usize,
) {
    if clusters.len() < 2 {
        return;
    }

    let centroids: Vec<Option<Vec<f32>>> = clusters
        .iter()
        .map(|c| cluster_centroid(c, embeddings))
        .collect();

    let mut buckets: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (idx, centroid) in centroids.iter().enumerate() {
        let Some(centroid) = centroid else {
            continue;
        };
        let mut bucket = 0u16;
        for (dimension, &value) in centroid.iter().take(12).enumerate() {
            if value >= 0.0 {
                bucket |= 1 << dimension;
            }
        }
        buckets.entry(bucket).or_default().push(idx);
    }

    let mut parent: Vec<usize> = (0..clusters.len()).collect();
    let mut sizes: Vec<usize> = clusters
        .iter()
        .map(|cluster| cluster.symbols.len())
        .collect();
    let effective_cap = cap.min(MAX_SYMBOLS_PER_CLUSTER);

    for indices in buckets.values_mut() {
        indices.sort_by(|&a, &b| clusters[a].name.cmp(&clusters[b].name));
        for group in indices.chunks(MAX_EMBEDDING_BUCKET_SIZE) {
            for i in 0..group.len() {
                for j in (i + 1)..group.len() {
                    let left = group[i];
                    let right = group[j];
                    let left_root = union_find_root(&mut parent, left);
                    let right_root = union_find_root(&mut parent, right);
                    if left_root == right_root
                        || sizes[left_root] + sizes[right_root] > effective_cap
                    {
                        continue;
                    }
                    let (Some(left_centroid), Some(right_centroid)) =
                        (centroids[left].as_ref(), centroids[right].as_ref())
                    else {
                        continue;
                    };
                    if cosine_similarity(left_centroid, right_centroid) < threshold {
                        continue;
                    }
                    let (keep, merge) = if sizes[left_root] > sizes[right_root]
                        || (sizes[left_root] == sizes[right_root]
                            && clusters[left_root].name <= clusters[right_root].name)
                    {
                        (left_root, right_root)
                    } else {
                        (right_root, left_root)
                    };
                    parent[merge] = keep;
                    sizes[keep] += sizes[merge];
                }
            }
        }
    }

    let mut merged: BTreeMap<usize, ConceptCluster> = BTreeMap::new();
    for (idx, mut cluster) in std::mem::take(clusters).into_iter().enumerate() {
        let root = union_find_root(&mut parent, idx);
        match merged.get_mut(&root) {
            Some(target) => {
                target.symbols.append(&mut cluster.symbols);
                target.files.append(&mut cluster.files);
                target.score = target.score.max(cluster.score);
            }
            None => {
                merged.insert(root, cluster);
            }
        }
    }
    for cluster in merged.values_mut() {
        cluster.symbols.sort();
        cluster.symbols.dedup();
        cluster.symbols.truncate(effective_cap);
        cluster.files.sort();
        cluster.files.dedup();
        cluster.files.truncate(MAX_FILES_PER_CLUSTER);
    }
    *clusters = merged.into_values().collect();
}

fn union_find_root(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        parent[node] = parent[parent[node]];
        node = parent[node];
    }
    node
}

/// Compute the centroid (element-wise mean) of the embedding vectors for
/// every symbol in `cluster` that appears in `embeddings`. Returns `None`
/// when no cluster symbol has an embedding.
///
/// Seeds a zero vector from the first matching embedding's length, then
/// accumulates in place by reference — no per-symbol cloning.
fn cluster_centroid(
    cluster: &ConceptCluster,
    embeddings: &HashMap<String, Vec<f32>>,
) -> Option<Vec<f32>> {
    let mut centroid: Option<Vec<f32>> = None;
    let mut count: usize = 0;
    for sym in &cluster.symbols {
        let Some(vec) = embeddings.get(sym) else {
            continue;
        };
        let acc = centroid.get_or_insert_with(|| vec![0.0; vec.len()]);
        if acc.len() != vec.len() {
            continue;
        }
        for (a, b) in acc.iter_mut().zip(vec.iter()) {
            *a += *b;
        }
        count += 1;
    }
    let mut centroid = centroid?;
    // `centroid?` returning `Some` implies the accumulation branch ran at
    // least once, so `count >= 1` here — no zero-divisor risk.
    let inv = 1.0 / count as f32;
    for v in centroid.iter_mut() {
        *v *= inv;
    }
    Some(centroid)
}

/// Cosine similarity wrapper that guards the length-mismatch case (the
/// shared [`crate::index::simd_distance::cosine_similarity`] panics on
/// mismatch; concept-graph inputs are untrusted vs. this crate's invariants).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    crate::index::simd_distance::cosine_similarity(a, b)
}

// ---------------------------------------------------------------------------
// Helper functions (pub — reused by Tasks 10, 13)
// ---------------------------------------------------------------------------

/// Extract meaningful concept words from a doc comment or text block.
///
/// Filters stop words, requires length >= 3, and lowercases everything.
///
/// # Examples
///
/// ```
/// use codixing_core::engine::concepts::extract_concept_words;
///
/// let words = extract_concept_words("Verifies the JWT authentication token");
/// assert!(words.contains(&"verifies".to_string()));
/// assert!(words.contains(&"jwt".to_string()));
/// assert!(words.contains(&"authentication".to_string()));
/// assert!(words.contains(&"token".to_string()));
/// // "the" is a stop word and is excluded
/// assert!(!words.contains(&"the".to_string()));
/// ```
pub fn extract_concept_words(doc: &str) -> Vec<String> {
    doc.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 3)
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect()
}

/// Extract the strongest unique concept words while retaining at most `limit`
/// small strings. Builders use this instead of materializing arbitrarily large
/// generated documentation vocabularies.
pub(super) fn extract_ranked_concept_words(doc: &str, limit: usize) -> Vec<String> {
    if limit == 0 {
        return Vec::new();
    }

    let mut words = Vec::with_capacity(limit + 1);
    for raw in doc.split(|c: char| !c.is_alphanumeric()) {
        if raw.len() < 3 || raw.len() > MAX_CONCEPT_WORD_BYTES {
            continue;
        }
        let word = raw.to_lowercase();
        if STOP_WORDS.contains(&word.as_str()) || words.contains(&word) {
            continue;
        }
        words.push(word);
        words.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        words.truncate(limit);
    }
    words
}

/// Split a `camelCase` or `snake_case` identifier into lowercase parts.
///
/// Handles:
/// - `snake_case` → `["snake", "case"]`
/// - `camelCase` → `["camel", "case"]`
/// - `HTTPClient` → `["http", "client"]`
/// - `verify_jwt_token` → `["verify", "jwt", "token"]`
/// - `BGESmallEn` → `["bge", "small", "en"]`
///
/// Parts shorter than 2 characters are dropped.
///
/// # Examples
///
/// ```
/// use codixing_core::engine::concepts::decompose_identifier;
///
/// assert_eq!(decompose_identifier("verify_jwt_token"), vec!["verify", "jwt", "token"]);
/// assert_eq!(decompose_identifier("HTTPClient"), vec!["http", "client"]);
/// assert_eq!(decompose_identifier("camelCase"), vec!["camel", "case"]);
/// ```
pub fn decompose_identifier(name: &str) -> Vec<String> {
    let mut parts = Vec::new();

    // First split by underscores (handles snake_case)
    for segment in name.split('_') {
        if segment.is_empty() {
            continue;
        }
        // Then split by camelCase boundaries within each segment
        split_camel_case(segment, &mut parts);
    }

    // Lowercase everything and filter short parts
    parts
        .into_iter()
        .map(|p| p.to_lowercase())
        .filter(|p| p.len() >= 2)
        .collect()
}

/// Split a single segment (no underscores) at CamelCase boundaries.
fn split_camel_case(segment: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = segment.chars().collect();
    if chars.is_empty() {
        return;
    }

    let mut current = String::new();
    current.push(chars[0]);

    for i in 1..chars.len() {
        let c = chars[i];
        let prev = chars[i - 1];

        // Insert a split at CamelCase boundaries:
        // - lowercase followed by uppercase (camelCase)
        // - uppercase followed by uppercase+lowercase (HTTPClient → HTTP + Client)
        let boundary = (prev.is_ascii_lowercase() && c.is_ascii_uppercase())
            || (prev.is_ascii_uppercase()
                && c.is_ascii_uppercase()
                && i + 1 < chars.len()
                && chars[i + 1].is_ascii_lowercase());

        if boundary && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }

        current.push(c);
    }

    if !current.is_empty() {
        out.push(current);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_from_doc_comments() {
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol(
            "login",
            "src/auth.rs",
            Some("Handle user authentication login"),
        );
        builder.add_symbol(
            "verify_token",
            "src/auth.rs",
            Some("Verify the authentication token"),
        );
        builder.add_symbol("parse_json", "src/parser.rs", Some("Parse a JSON document"));

        let index = builder.build();
        assert!(!index.is_empty());

        // "authentication" appears in doc comments for both login and verify_token
        let auth_clusters = index.lookup("authentication");
        assert!(
            !auth_clusters.is_empty(),
            "should find clusters for 'authentication'"
        );

        let auth = &auth_clusters[0];
        assert!(
            auth.symbols.contains(&"login".to_string()),
            "auth cluster should contain 'login'"
        );
        assert!(
            auth.symbols.contains(&"verify_token".to_string()),
            "auth cluster should contain 'verify_token'"
        );
    }

    #[test]
    fn build_from_identifier_decomposition() {
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol("parseJson", "src/parser.rs", None);
        builder.add_symbol("parseXml", "src/parser.rs", None);
        builder.add_symbol("parseCsv", "src/parser.rs", None);
        builder.add_symbol("renderHtml", "src/renderer.rs", None);

        let index = builder.build();

        // "parse" is shared by 3 symbols → should form a cluster
        let parse_clusters = index.lookup("parse");
        assert!(
            !parse_clusters.is_empty(),
            "should find clusters for 'parse'"
        );

        let cluster = &parse_clusters[0];
        assert!(
            cluster.symbols.len() >= 3,
            "parse cluster should have at least 3 symbols, got {}",
            cluster.symbols.len()
        );
        assert!(cluster.symbols.contains(&"parseJson".to_string()));
        assert!(cluster.symbols.contains(&"parseXml".to_string()));
        assert!(cluster.symbols.contains(&"parseCsv".to_string()));
        // renderHtml should NOT be in the parse cluster
        assert!(!cluster.symbols.contains(&"renderHtml".to_string()));
    }

    #[test]
    fn build_from_import_cooccurrence() {
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol("AuthGuard", "src/auth.rs", None);
        builder.add_symbol("AuthMiddleware", "src/middleware.rs", None);
        // These two share the "auth" identifier part, so they form a cluster.
        // The co-occurrence should expand the file set.
        builder.add_cooccurrence("src/auth.rs", "src/middleware.rs");
        builder.add_cooccurrence("src/auth.rs", "src/routes.rs");

        let index = builder.build();

        let auth_clusters = index.lookup("auth");
        assert!(!auth_clusters.is_empty(), "should find clusters for 'auth'");

        let cluster = &auth_clusters[0];
        // The co-occurrence with routes.rs should expand the file set
        assert!(
            cluster.files.contains(&"src/routes.rs".to_string()),
            "co-occurrence should expand file set to include src/routes.rs, got {:?}",
            cluster.files
        );
        assert!(cluster.files.contains(&"src/auth.rs".to_string()));
        assert!(cluster.files.contains(&"src/middleware.rs".to_string()));
    }

    #[test]
    fn lookup_returns_empty_for_unknown() {
        let index = ConceptIndex::default();
        assert!(index.lookup("nonexistent").is_empty());
        assert!(index.lookup_query("something unknown").is_empty());
        assert!(index.is_empty());

        // Also test with a populated index
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol("parseCsv", "src/parser.rs", None);
        builder.add_symbol("parseJson", "src/parser.rs", None);
        let index = builder.build();
        assert!(index.lookup("zzzzzzz").is_empty());
        assert!(
            index
                .lookup_query("completely unknown terms xyz")
                .is_empty()
        );
    }

    #[test]
    fn concept_index_serialization_roundtrip() {
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol("login", "src/auth.rs", Some("Handle authentication"));
        builder.add_symbol(
            "verify_token",
            "src/auth.rs",
            Some("Verify authentication token"),
        );
        builder.add_symbol("parseJson", "src/parser.rs", None);
        builder.add_symbol("parseXml", "src/parser.rs", None);
        builder.add_cooccurrence("src/auth.rs", "src/parser.rs");

        let original = builder.build();
        assert!(!original.is_empty());

        // bitcode roundtrip
        let bytes = bitcode::serialize(&original).expect("serialize should succeed");
        let decoded: ConceptIndex =
            bitcode::deserialize(&bytes).expect("deserialize should succeed");

        assert_eq!(original.clusters.len(), decoded.clusters.len());
        for (a, b) in original.clusters.iter().zip(decoded.clusters.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.symbols, b.symbols);
            assert_eq!(a.files, b.files);
            assert!((a.score - b.score).abs() < f32::EPSILON);
        }

        // Verify lookups work identically after roundtrip
        let orig_parse = original.lookup("parse");
        let decoded_parse = decoded.lookup("parse");
        assert_eq!(orig_parse.len(), decoded_parse.len());
    }

    // ----- Helper function unit tests -----

    #[test]
    fn decompose_snake_case() {
        assert_eq!(
            decompose_identifier("verify_jwt_token"),
            vec!["verify", "jwt", "token"]
        );
    }

    #[test]
    fn decompose_camel_case() {
        assert_eq!(decompose_identifier("camelCase"), vec!["camel", "case"]);
        assert_eq!(
            decompose_identifier("parseJsonDocument"),
            vec!["parse", "json", "document"]
        );
    }

    #[test]
    fn decompose_acronym_boundary() {
        assert_eq!(decompose_identifier("HTTPClient"), vec!["http", "client"]);
        assert_eq!(
            decompose_identifier("BGESmallEn"),
            vec!["bge", "small", "en"]
        );
    }

    #[test]
    fn decompose_mixed() {
        assert_eq!(
            decompose_identifier("parse_jsonDocument"),
            vec!["parse", "json", "document"]
        );
    }

    #[test]
    fn decompose_single_word() {
        assert_eq!(decompose_identifier("parser"), vec!["parser"]);
    }

    #[test]
    fn decompose_short_parts_dropped() {
        // Single-char parts should be dropped (filter >= 2)
        assert_eq!(decompose_identifier("a_b_token"), vec!["token"]);
    }

    #[test]
    fn extract_words_filters_stop_words() {
        let words = extract_concept_words("the quick authentication for a user");
        assert!(words.contains(&"quick".to_string()));
        assert!(words.contains(&"authentication".to_string()));
        assert!(words.contains(&"user".to_string()));
        assert!(!words.contains(&"the".to_string()));
        assert!(!words.contains(&"for".to_string()));
        assert!(!words.contains(&"a".to_string())); // "a" is < 3 chars anyway
    }

    #[test]
    fn extract_words_filters_short() {
        let words = extract_concept_words("it is ok to go");
        // All words are <= 2 chars or stop words
        assert!(words.is_empty());
    }

    #[test]
    fn ranked_concept_words_bound_generated_documentation() {
        let oversized = "x".repeat(10_000);
        let doc = format!(
            "{oversized} authentication authorization validation network session storage cache"
        );
        let words = extract_ranked_concept_words(&doc, 4);
        assert_eq!(
            words,
            vec!["authentication", "authorization", "validation", "network"]
        );
        assert!(
            words
                .iter()
                .all(|word| word.len() <= MAX_CONCEPT_WORD_BYTES)
        );
    }

    #[test]
    fn lookup_query_ranks_by_hit_count() {
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol(
            "verify_auth_token",
            "src/auth.rs",
            Some("Verify authentication token"),
        );
        builder.add_symbol(
            "validate_auth_code",
            "src/auth.rs",
            Some("Validate authentication code"),
        );
        builder.add_symbol("parse_token", "src/parser.rs", Some("Parse a token string"));

        let index = builder.build();

        let results = index.lookup_query("auth token");
        assert!(!results.is_empty(), "should find results for 'auth token'");

        // Clusters matching both "auth" AND "token" should rank higher
        // than clusters matching only one term.
        if results.len() >= 2 {
            assert!(
                results[0].1 >= results[1].1,
                "first result should have >= hit count than second"
            );
        }
    }

    // -----------------------------------------------------------------
    // v0.40 Phase 4: embedding-cluster merge
    // -----------------------------------------------------------------

    fn vec_of(v: &[f32]) -> Vec<f32> {
        v.to_vec()
    }

    #[test]
    fn cosine_similarity_basic() {
        // Identical vectors
        let sim = cosine_similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0, 0.0]);
        assert!((sim - 1.0).abs() < 1e-6);
        // Orthogonal
        let sim = cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]);
        assert!(sim.abs() < 1e-6);
        // Mismatched dim → 0
        let sim = cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn phase_4_merges_highly_similar_clusters() {
        // Three clusters: A and B have near-identical centroids; C is
        // orthogonal. After phase 4, A and B collapse into one.
        let mut builder = ConceptIndexBuilder::new();
        // Cluster "login / authenticate" (will share decomposed parts).
        builder.add_symbol("login_user", "src/auth.rs", None);
        builder.add_symbol("login_session", "src/auth.rs", None);
        // A second two-symbol cluster that by itself is unrelated.
        builder.add_symbol("authenticate_user", "src/auth.rs", None);
        builder.add_symbol("authenticate_session", "src/auth.rs", None);
        // Plus an orthogonal cluster.
        builder.add_symbol("render_page", "src/view.rs", None);
        builder.add_symbol("render_layout", "src/view.rs", None);

        // Embeddings: login_* and authenticate_* share the same vector
        // (synthetic 'synonym'). render_* points the other way.
        let mut emb: HashMap<String, Vec<f32>> = HashMap::new();
        emb.insert("login_user".into(), vec_of(&[1.0, 0.0]));
        emb.insert("login_session".into(), vec_of(&[1.0, 0.0]));
        emb.insert("authenticate_user".into(), vec_of(&[1.0, 0.0]));
        emb.insert("authenticate_session".into(), vec_of(&[1.0, 0.0]));
        emb.insert("render_page".into(), vec_of(&[0.0, 1.0]));
        emb.insert("render_layout".into(), vec_of(&[0.0, 1.0]));

        let index_with = builder.with_symbol_embeddings(emb.clone()).build();

        // Control: same build with no embeddings — clusters remain separate.
        let mut control = ConceptIndexBuilder::new();
        control.add_symbol("login_user", "src/auth.rs", None);
        control.add_symbol("login_session", "src/auth.rs", None);
        control.add_symbol("authenticate_user", "src/auth.rs", None);
        control.add_symbol("authenticate_session", "src/auth.rs", None);
        control.add_symbol("render_page", "src/view.rs", None);
        control.add_symbol("render_layout", "src/view.rs", None);
        let index_control = control.build();

        // Phase 4 should reduce cluster count because login↔authenticate
        // get merged, but render stays orthogonal.
        assert!(
            index_with.clusters.len() < index_control.clusters.len(),
            "phase 4 should merge at least one cluster pair \
             (with={} clusters, control={} clusters)",
            index_with.clusters.len(),
            index_control.clusters.len()
        );

        // At least one cluster after phase 4 should contain a login_*
        // symbol AND an authenticate_* symbol (proof of merge).
        let merged = index_with.clusters.iter().any(|c| {
            c.symbols.iter().any(|s| s.starts_with("login_"))
                && c.symbols.iter().any(|s| s.starts_with("authenticate_"))
        });
        assert!(
            merged,
            "expected a cluster containing both login_* and authenticate_* after merge"
        );
    }

    #[test]
    fn phase_4_respects_cluster_cap() {
        // Two clusters with NO shared identifier parts (so Phase 1 doesn't
        // pre-merge them). Each has 4 symbols. With cap = 6, Phase 4 must
        // NOT merge them even though their centroids are identical.
        let mut builder = ConceptIndexBuilder::new();
        let alpha_names = ["alpha_foo", "alpha_bar", "alpha_baz", "alpha_qux"];
        let zeta_names = ["zeta_wombat", "zeta_kiwi", "zeta_plum", "zeta_fig"];
        for name in alpha_names.iter() {
            builder.add_symbol(name, "src/a.rs", None);
        }
        for name in zeta_names.iter() {
            builder.add_symbol(name, "src/b.rs", None);
        }

        let mut emb: HashMap<String, Vec<f32>> = HashMap::new();
        for name in alpha_names.iter().chain(zeta_names.iter()) {
            emb.insert((*name).to_string(), vec_of(&[1.0, 0.0]));
        }

        let index = builder
            .with_embed_cluster(
                emb,
                EmbedClusterConfig {
                    threshold: 0.5,
                    cap: 6, // too small for 4+4 merge
                },
            )
            .build();

        // No cluster should contain both an alpha_* and a zeta_*.
        let merged = index.clusters.iter().any(|c| {
            c.symbols.iter().any(|s| s.starts_with("alpha_"))
                && c.symbols.iter().any(|s| s.starts_with("zeta_"))
        });
        assert!(!merged, "cap=6 must prevent 4+4 merge");
    }

    #[test]
    fn phase_4_preserves_doc_term_lookups_after_merge() {
        // Regression test for the Phase 4 → Phase 5 index-invalidation bug:
        // doc-only concept terms must still resolve to their cluster after
        // embedding merge collapses the original cluster indices.
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol(
            "login_user",
            "src/auth.rs",
            Some("Verify a telemetry user session"),
        );
        builder.add_symbol(
            "login_session",
            "src/auth.rs",
            Some("Verify a telemetry user session"),
        );
        builder.add_symbol(
            "authenticate_user",
            "src/auth.rs",
            Some("Verify a telemetry user session"),
        );
        builder.add_symbol(
            "authenticate_session",
            "src/auth.rs",
            Some("Verify a telemetry user session"),
        );

        // Embeddings that cause Phase 4 to merge login_* and authenticate_*.
        let mut emb: HashMap<String, Vec<f32>> = HashMap::new();
        for name in [
            "login_user",
            "login_session",
            "authenticate_user",
            "authenticate_session",
        ] {
            emb.insert(name.to_string(), vec_of(&[1.0, 0.0]));
        }

        let index = builder.with_symbol_embeddings(emb).build();

        // After merge, the doc term "telemetry" must still land on the
        // surviving cluster — the bug made `seen_term_sets` point at an
        // already-removed cluster, losing the lookup entirely.
        let hits = index.lookup("telemetry");
        assert!(
            !hits.is_empty(),
            "doc-only concept term should still resolve post-merge"
        );
    }

    #[test]
    fn phase_4_no_op_when_embeddings_absent() {
        // Calling build() without `with_symbol_embeddings` should produce
        // the exact same cluster count as before v0.40 — backwards compat.
        let mut b1 = ConceptIndexBuilder::new();
        b1.add_symbol("login_user", "src/auth.rs", None);
        b1.add_symbol("login_session", "src/auth.rs", None);
        b1.add_symbol("render_page", "src/view.rs", None);
        b1.add_symbol("render_layout", "src/view.rs", None);
        let without = b1.build();

        let mut b2 = ConceptIndexBuilder::new();
        b2.add_symbol("login_user", "src/auth.rs", None);
        b2.add_symbol("login_session", "src/auth.rs", None);
        b2.add_symbol("render_page", "src/view.rs", None);
        b2.add_symbol("render_layout", "src/view.rs", None);
        // Empty embeddings map → no-op.
        let with_empty = b2.with_symbol_embeddings(HashMap::new()).build();

        assert_eq!(without.clusters.len(), with_empty.clusters.len());
    }

    #[test]
    fn bounded_clusters_keep_ranked_useful_members() {
        let mut builder = ConceptIndexBuilder::new();
        for idx in 0..200 {
            builder.add_symbol(
                &format!("auth_token_handler_{idx}"),
                &format!("src/auth/module_{idx}.rs"),
                Some("Authenticate and validate credential tokens for users"),
            );
            builder.add_cooccurrence(
                &format!("src/auth/module_{idx}.rs"),
                &format!("src/routes/route_{idx}.rs"),
            );
        }
        let index = builder.build();
        let auth = index.lookup("auth");
        assert!(!auth.is_empty(), "high-evidence auth concept must survive");
        assert!(index.clusters.len() <= MAX_CONCEPT_CLUSTERS);
        assert!(index.term_to_clusters.len() <= MAX_CONCEPT_TERMS);
        assert!(index.clusters.iter().all(|cluster| {
            cluster.symbols.len() <= MAX_SYMBOLS_PER_CLUSTER
                && cluster.files.len() <= MAX_FILES_PER_CLUSTER
        }));
        assert!(
            index
                .term_to_clusters
                .values()
                .all(|indices| indices.len() <= MAX_CLUSTERS_PER_TERM)
        );
    }

    #[test]
    fn persisted_concepts_are_deterministic_across_input_order() {
        fn build(reverse: bool) -> ConceptIndex {
            let mut symbols = vec![
                ("auth_login", "src/auth.rs", "Authenticate a credential"),
                ("auth_token", "src/token.rs", "Validate a credential"),
                ("cache_read", "src/cache.rs", "Read cached values"),
                ("cache_write", "src/cache.rs", "Write cached values"),
            ];
            if reverse {
                symbols.reverse();
            }
            let mut builder = ConceptIndexBuilder::new();
            for (name, file, doc) in symbols {
                builder.add_symbol(name, file, Some(doc));
            }
            builder.add_cooccurrence("src/auth.rs", "src/token.rs");
            builder.build()
        }
        let forward = build(false).encode_persisted().unwrap();
        let reverse = build(true).encode_persisted().unwrap();
        assert_eq!(forward, reverse);
    }

    #[test]
    fn interned_concept_format_is_less_than_half_legacy_size() {
        let symbols: Vec<String> = (0..32).map(|idx| format!("shared_symbol_{idx}")).collect();
        let files: Vec<String> = (0..16)
            .map(|idx| format!("src/shared/module_{idx}.rs"))
            .collect();
        let clusters: Vec<ConceptCluster> = (0..256)
            .map(|idx| ConceptCluster {
                name: format!("concept_{idx}"),
                symbols: symbols.clone(),
                files: files.clone(),
                score: 0.5,
            })
            .collect();
        let term_to_clusters = clusters
            .iter()
            .enumerate()
            .map(|(idx, cluster)| (cluster.name.clone(), vec![idx]))
            .collect();
        let index = ConceptIndex {
            clusters,
            term_to_clusters,
        };
        let legacy = bitcode::serialize(&index).unwrap();
        let compact = index.encode_persisted().unwrap();
        assert!(
            compact.len().saturating_mul(2) <= legacy.len(),
            "compact={} legacy={}",
            compact.len(),
            legacy.len()
        );
        let decoded = ConceptIndex::decode_persisted(&compact).unwrap();
        assert_eq!(decoded.clusters.len(), index.clusters.len());
        assert_eq!(decoded.lookup("concept_42").len(), 1);
    }

    #[test]
    fn persisted_concept_decoder_accepts_legacy_bytes() {
        let mut builder = ConceptIndexBuilder::new();
        builder.add_symbol("auth_login", "src/auth.rs", None);
        builder.add_symbol("auth_token", "src/auth.rs", None);
        let index = builder.build();
        let legacy = bitcode::serialize(&index).unwrap();
        let decoded = ConceptIndex::decode_persisted(&legacy).unwrap();
        assert!(!decoded.lookup("auth").is_empty());
    }
}
