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

use std::collections::HashMap;

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
}

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
        let doc_words = doc_comment.map(extract_concept_words).unwrap_or_default();
        self.symbols.push(SymbolRecord {
            name: name.to_string(),
            file: file.to_string(),
            doc_words,
        });
    }

    /// Record that two files co-occur (e.g. one imports the other).
    pub fn add_cooccurrence(&mut self, file_a: &str, file_b: &str) {
        self.cooccurrences
            .push((file_a.to_string(), file_b.to_string()));
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
    pub fn build(self) -> ConceptIndex {
        if self.symbols.is_empty() {
            return ConceptIndex::default();
        }

        let total_symbols = self.symbols.len();

        // -----------------------------------------------------------------
        // Phase 1: Group symbols by shared decomposed identifier parts
        // -----------------------------------------------------------------

        // term → list of symbol indices that contain this term
        let mut term_to_symbol_indices: HashMap<String, Vec<usize>> = HashMap::new();
        let decomposed: Vec<Vec<String>> = self
            .symbols
            .iter()
            .enumerate()
            .map(|(idx, rec)| {
                let parts = decompose_identifier(&rec.name);
                for part in &parts {
                    term_to_symbol_indices
                        .entry(part.clone())
                        .or_default()
                        .push(idx);
                }
                parts
            })
            .collect();

        // Build clusters from shared terms (skip singletons)
        let mut clusters: Vec<ConceptCluster> = Vec::new();
        let mut seen_term_sets: HashMap<String, usize> = HashMap::new(); // cluster dedup

        for (term, sym_indices) in &term_to_symbol_indices {
            if sym_indices.len() < 2 {
                continue; // skip singleton terms
            }

            // Dedup key: sorted symbol indices
            let mut sorted_indices = sym_indices.clone();
            sorted_indices.sort_unstable();
            sorted_indices.dedup();
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

        for (doc_term, sym_indices) in &doc_term_to_symbols {
            if sym_indices.len() < 2 {
                // Even single-symbol doc terms are useful — they create a
                // concept→symbol bridge that wouldn't exist from identifiers
                // alone. But we still skip if the term is already covered.
            }

            let mut sorted_indices = sym_indices.clone();
            sorted_indices.sort_unstable();
            sorted_indices.dedup();

            // Check if a cluster with exactly this symbol set already exists
            let dedup_key = format!("{sorted_indices:?}");
            if seen_term_sets.contains_key(&dedup_key) {
                continue; // already covered by an identifier-based cluster
            }

            let mut symbol_names: Vec<String> = sorted_indices
                .iter()
                .map(|&i| self.symbols[i].name.clone())
                .collect();
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
        }

        // -----------------------------------------------------------------
        // Phase 3: Import co-occurrence — expand cluster file sets
        // -----------------------------------------------------------------

        // Build a file → co-occurring files map
        let mut cooccur_map: HashMap<&str, Vec<&str>> = HashMap::new();
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
            cluster.files = expanded_files;
        }

        // -----------------------------------------------------------------
        // Phase 4: Embedding-based cluster merge (v0.40)
        // -----------------------------------------------------------------
        //
        // Single-link agglomerative merge at centroid-cosine >= threshold,
        // cap = max combined symbol count. O(C^2) — on linux-kernel scale
        // a HNSW top-K probe is the documented follow-up.
        if let Some((embeddings, config)) = self.embed_cluster.as_ref() {
            merge_clusters_by_embedding(&mut clusters, embeddings, config.threshold, config.cap);
        }

        // -----------------------------------------------------------------
        // Phase 5: Build term → cluster index
        // -----------------------------------------------------------------

        let mut term_to_clusters: HashMap<String, Vec<usize>> = HashMap::new();

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

        // Also index doc terms → their clusters
        for (doc_term, sym_indices) in &doc_term_to_symbols {
            let mut sorted_indices = sym_indices.clone();
            sorted_indices.sort_unstable();
            sorted_indices.dedup();
            let dedup_key = format!("{sorted_indices:?}");
            if let Some(&cluster_idx) = seen_term_sets.get(&dedup_key) {
                term_to_clusters
                    .entry(doc_term.clone())
                    .or_default()
                    .push(cluster_idx);
            }
        }

        // Index all identifier-decomposition terms → their clusters
        for (term, sym_indices) in &term_to_symbol_indices {
            if sym_indices.len() < 2 {
                continue;
            }
            let mut sorted_indices = sym_indices.clone();
            sorted_indices.sort_unstable();
            sorted_indices.dedup();
            let dedup_key = format!("{sorted_indices:?}");
            if let Some(&cluster_idx) = seen_term_sets.get(&dedup_key) {
                term_to_clusters
                    .entry(term.clone())
                    .or_default()
                    .push(cluster_idx);
            }
        }

        // Dedup the cluster index lists
        for indices in term_to_clusters.values_mut() {
            indices.sort_unstable();
            indices.dedup();
        }

        // Drop the intermediate decomposed data.
        drop(decomposed);

        ConceptIndex {
            clusters,
            term_to_clusters,
        }
    }
}

/// Single-link agglomerative merge on concept clusters using chunk embeddings.
///
/// For each pair of clusters, compute the cosine similarity of their
/// centroid vectors (mean of member-symbol embeddings). If the similarity
/// exceeds `threshold` AND the combined member count fits under `cap`,
/// merge the smaller into the larger and re-centroid. Iterate until no
/// merge fires in a full pass.
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

    // Precompute centroid for every cluster up front.
    let mut centroids: Vec<Option<Vec<f32>>> = clusters
        .iter()
        .map(|c| cluster_centroid(c, embeddings))
        .collect();

    loop {
        let mut merged_this_pass = false;

        'outer: for i in 0..clusters.len() {
            let Some(c_i) = centroids[i].as_ref() else {
                continue;
            };
            for j in (i + 1)..clusters.len() {
                let Some(c_j) = centroids[j].as_ref() else {
                    continue;
                };
                if clusters[i].symbols.len() + clusters[j].symbols.len() > cap {
                    continue;
                }
                let sim = cosine_similarity(c_i, c_j);
                if sim < threshold {
                    continue;
                }

                // Merge j into i. Prefer keeping the larger cluster's name.
                let (keep, drop_idx) = if clusters[i].symbols.len() >= clusters[j].symbols.len() {
                    (i, j)
                } else {
                    (j, i)
                };

                // Take (not clone) the dropped cluster's vecs — it's about to be
                // removed anyway. Dedup once after the append, not per push.
                let mut drop_cluster = clusters.remove(drop_idx);
                centroids.remove(drop_idx);
                let keep = if drop_idx < keep { keep - 1 } else { keep };

                let keep_cluster = &mut clusters[keep];
                keep_cluster.symbols.append(&mut drop_cluster.symbols);
                keep_cluster.symbols.sort();
                keep_cluster.symbols.dedup();
                keep_cluster.files.append(&mut drop_cluster.files);
                keep_cluster.files.sort();
                keep_cluster.files.dedup();
                keep_cluster.score = keep_cluster.score.max(drop_cluster.score);

                centroids[keep] = cluster_centroid(keep_cluster, embeddings);
                merged_this_pass = true;
                break 'outer;
            }
        }

        if !merged_this_pass {
            break;
        }
    }
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
}
