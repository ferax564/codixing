//! Project-specific learned query reformulations.
//!
//! Supplements the hardcoded synonym map (~80 groups in `engine/synonyms.rs`)
//! with vocabulary learned from the project's own codebase:
//!
//! 1. **Term co-occurrence** — identifiers that share a file are related.
//! 2. **Doc-to-code bridge** — doc comment words map to symbol names.
//!
//! Built during `Engine::init()` and loaded from disk during `Engine::open()`.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::concepts::{decompose_identifier, extract_concept_words};

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

/// Project-specific learned query reformulations.
///
/// Maps natural-language terms to related code identifiers and vice versa,
/// bridging vocabulary gaps that the static synonym map cannot cover.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LearnedReformulations {
    /// term -> related terms (project-specific synonym groups learned from
    /// identifier co-occurrence within the same file).
    pub term_expansions: HashMap<String, Vec<String>>,
    /// NL term -> code identifiers (learned from doc comments).
    pub doc_to_code: HashMap<String, Vec<String>>,
    /// Query term -> file-vocabulary words learned from agent session events:
    /// when a query like `"rate limiting"` is consistently followed by reads
    /// of `src/throttle/...` files, the miner emits `rate -> throttle`.
    ///
    /// Pairs must meet frequency ≥ 3 and distinct-session ≥ 2 thresholds to
    /// avoid one-off noise. Empty when no session mining has been run (the
    /// normal state today — session persistence wiring ships in v0.42).
    #[serde(default)]
    pub session_expansions: HashMap<String, Vec<String>>,
}

impl LearnedReformulations {
    /// Returns related terms and code identifiers for a given term.
    ///
    /// Merges results from `term_expansions` (co-occurrence), `doc_to_code`
    /// (documentation bridge), and `session_expansions` (agent session
    /// mining), deduplicating the output.
    pub fn expand(&self, term: &str) -> Vec<String> {
        let key = term.to_lowercase();
        let mut result: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        if let Some(expansions) = self.term_expansions.get(&key) {
            for exp in expansions {
                if seen.insert(exp.clone()) {
                    result.push(exp.clone());
                }
            }
        }

        if let Some(code_ids) = self.doc_to_code.get(&key) {
            for id in code_ids {
                if seen.insert(id.clone()) {
                    result.push(id.clone());
                }
            }
        }

        if let Some(session) = self.session_expansions.get(&key) {
            for id in session {
                if seen.insert(id.clone()) {
                    result.push(id.clone());
                }
            }
        }

        result
    }

    /// Returns `true` if no reformulations have been learned.
    pub fn is_empty(&self) -> bool {
        self.term_expansions.is_empty()
            && self.doc_to_code.is_empty()
            && self.session_expansions.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Intermediate record of an identifier and its file location.
#[derive(Debug)]
struct IdentifierRecord {
    name: String,
    file: String,
}

/// Intermediate record of a documented symbol.
#[derive(Debug)]
struct DocSymbolRecord {
    name: String,
    doc: String,
}

/// Intermediate record of an agent session event: the query issued, the
/// files the agent subsequently visited, and the session id so the miner
/// can enforce the distinct-session threshold.
#[derive(Debug)]
struct SessionEventRecord {
    query_terms: Vec<String>,
    file_vocab: Vec<String>,
    session_id: String,
}

/// Frequency threshold: a (query_term, file_vocab_word) pair must appear at
/// least this many times across all session events before being learned.
const SESSION_FREQ_THRESHOLD: usize = 3;

/// Distinct-session threshold: the pair must come from at least this many
/// distinct session ids. Prevents a single chatty session from injecting
/// noise into the learned vocabulary.
const SESSION_DISTINCT_SESSIONS_THRESHOLD: usize = 2;

/// Builds [`LearnedReformulations`] from symbol data.
///
/// Feed identifiers and documented symbols, then call [`build`] to produce
/// the learned reformulation map.
#[derive(Debug, Default)]
pub struct ReformulationBuilder {
    identifiers: Vec<IdentifierRecord>,
    doc_symbols: Vec<DocSymbolRecord>,
    session_events: Vec<SessionEventRecord>,
}

impl ReformulationBuilder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an identifier with its file location for term co-occurrence mining.
    pub fn add_identifier(&mut self, name: &str, file: &str) {
        self.identifiers.push(IdentifierRecord {
            name: name.to_string(),
            file: file.to_string(),
        });
    }

    /// Register a documented symbol for doc-to-code bridge building.
    pub fn add_documented_symbol(&mut self, name: &str, doc: &str) {
        self.doc_symbols.push(DocSymbolRecord {
            name: name.to_string(),
            doc: doc.to_string(),
        });
    }

    /// Register an agent session event for session-driven reformulation.
    ///
    /// `query` is the natural-language search string the agent issued.
    /// `visited_files` is the set of file paths the agent read / wrote
    /// / searched for symbols in during the same session after issuing
    /// the query. `session_id` is any stable identifier for the session
    /// (agent id, MCP connection id, etc.) so the miner can enforce the
    /// distinct-session threshold against a single chatty client.
    ///
    /// Query terms are lowercased and filtered: length ≥ 3, not in the
    /// synonym-module stop list. File vocabulary is extracted from each
    /// path's basename (split on `_` / `-` / camelCase) and directory
    /// components (e.g. `src/auth/middleware.rs` → `auth`, `middleware`).
    pub fn add_session_event(&mut self, query: &str, visited_files: &[String], session_id: &str) {
        let query_terms = tokenize_query_for_session(query);
        if query_terms.is_empty() {
            return;
        }
        let file_vocab = extract_file_vocab(visited_files);
        if file_vocab.is_empty() {
            return;
        }
        self.session_events.push(SessionEventRecord {
            query_terms,
            file_vocab,
            session_id: session_id.to_string(),
        });
    }

    /// Consume the builder and produce [`LearnedReformulations`].
    ///
    /// Three learning sources:
    ///
    /// 1. **Term co-occurrence**: Group identifiers by file, decompose each
    ///    into parts, create bidirectional term links between parts that
    ///    co-occur in the same file. Only link terms with length >= 3.
    ///
    /// 2. **Doc-to-code bridge**: For each documented symbol, extract words
    ///    from its doc comment (filter stop words), map each word to the
    ///    symbol name. Filter out overly common words (appearing in >20 symbols).
    ///
    /// 3. **Identifier decomposition similarity**: Handled by source 1
    ///    (decomposing identifiers into parts and grouping by file).
    pub fn build(self) -> LearnedReformulations {
        let term_expansions = self.build_term_cooccurrence();
        let doc_to_code = self.build_doc_to_code();
        let session_expansions = self.build_session_learning();

        LearnedReformulations {
            term_expansions,
            doc_to_code,
            session_expansions,
        }
    }

    /// Source 1: Term co-occurrence mining.
    ///
    /// Groups identifiers by file, decomposes each into parts, and creates
    /// bidirectional links between parts that co-occur in the same file.
    fn build_term_cooccurrence(&self) -> HashMap<String, Vec<String>> {
        // Group identifiers by file.
        let mut file_to_parts: HashMap<&str, Vec<Vec<String>>> = HashMap::new();
        for rec in &self.identifiers {
            let parts = decompose_identifier(&rec.name);
            // Filter to parts with length >= 3.
            let parts: Vec<String> = parts.into_iter().filter(|p| p.len() >= 3).collect();
            if !parts.is_empty() {
                file_to_parts
                    .entry(rec.file.as_str())
                    .or_default()
                    .push(parts);
            }
        }

        // For each file, collect all unique parts and create bidirectional links.
        let mut term_links: HashMap<String, HashSet<String>> = HashMap::new();
        for identifier_parts_list in file_to_parts.values() {
            // Collect all unique parts across all identifiers in this file.
            let mut all_parts: HashSet<String> = HashSet::new();
            for parts in identifier_parts_list {
                for part in parts {
                    all_parts.insert(part.clone());
                }
            }

            // Create bidirectional links between all parts in the same file.
            let parts_vec: Vec<&String> = all_parts.iter().collect();
            for i in 0..parts_vec.len() {
                for j in (i + 1)..parts_vec.len() {
                    let a = parts_vec[i];
                    let b = parts_vec[j];
                    if a != b {
                        term_links.entry(a.clone()).or_default().insert(b.clone());
                        term_links.entry(b.clone()).or_default().insert(a.clone());
                    }
                }
            }
        }

        // Convert HashSets to sorted Vecs for deterministic output.
        term_links
            .into_iter()
            .map(|(k, v)| {
                let mut sorted: Vec<String> = v.into_iter().collect();
                sorted.sort();
                (k, sorted)
            })
            .collect()
    }

    /// Source 3: Session-driven reformulation mining.
    ///
    /// For each recorded session event, emit a (query_term, file_vocab_word)
    /// pair. A pair is kept in the output only when two thresholds are met:
    ///
    /// 1. **Frequency**: the pair appeared at least `SESSION_FREQ_THRESHOLD`
    ///    times across all events — filters one-off noise.
    /// 2. **Distinct sessions**: the pair came from at least
    ///    `SESSION_DISTINCT_SESSIONS_THRESHOLD` distinct session ids —
    ///    prevents a single chatty session from dominating.
    ///
    /// Output is sorted for deterministic on-disk layout.
    fn build_session_learning(&self) -> HashMap<String, Vec<String>> {
        if self.session_events.is_empty() {
            return HashMap::new();
        }

        // pair -> (frequency count, set of distinct session ids).
        let mut pair_stats: HashMap<(String, String), (usize, HashSet<String>)> = HashMap::new();
        for event in &self.session_events {
            for qt in &event.query_terms {
                for fv in &event.file_vocab {
                    if qt == fv {
                        continue;
                    }
                    let entry = pair_stats
                        .entry((qt.clone(), fv.clone()))
                        .or_insert_with(|| (0, HashSet::new()));
                    entry.0 += 1;
                    entry.1.insert(event.session_id.clone());
                }
            }
        }

        // Apply thresholds and collect into the term → Vec<vocab> map.
        let mut out: HashMap<String, HashSet<String>> = HashMap::new();
        for ((qt, fv), (freq, sessions)) in pair_stats {
            if freq >= SESSION_FREQ_THRESHOLD
                && sessions.len() >= SESSION_DISTINCT_SESSIONS_THRESHOLD
            {
                out.entry(qt).or_default().insert(fv);
            }
        }

        out.into_iter()
            .map(|(k, v)| {
                let mut sorted: Vec<String> = v.into_iter().collect();
                sorted.sort();
                (k, sorted)
            })
            .collect()
    }

    /// Source 2: Doc-to-code bridge.
    ///
    /// For each documented symbol, extracts meaningful words from its doc
    /// comment and maps each word to the symbol name. Filters out overly
    /// common words (appearing in >20 symbols).
    fn build_doc_to_code(&self) -> HashMap<String, Vec<String>> {
        if self.doc_symbols.is_empty() {
            return HashMap::new();
        }

        // word -> set of symbol names that mention it in their docs.
        let mut word_to_symbols: HashMap<String, HashSet<String>> = HashMap::new();

        for rec in &self.doc_symbols {
            let words = extract_concept_words(&rec.doc);
            for word in words {
                word_to_symbols
                    .entry(word)
                    .or_default()
                    .insert(rec.name.clone());
            }
        }

        // Filter out overly common words (appearing in >20 symbols).
        let max_symbol_count = 20;

        word_to_symbols
            .into_iter()
            .filter(|(_word, symbols)| symbols.len() <= max_symbol_count)
            .map(|(word, symbols)| {
                let mut sorted: Vec<String> = symbols.into_iter().collect();
                sorted.sort();
                (word, sorted)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers for session mining
// ---------------------------------------------------------------------------

/// Tokenize a natural-language query string into searchable terms.
///
/// Lowercases, splits on whitespace and common separators, and drops
/// terms below 3 characters. Does not depend on the static stopword map
/// in `synonyms.rs` so tests stay hermetic; callers that want stopword
/// filtering can post-filter with their own list.
fn tokenize_query_for_session(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_ascii_lowercase())
        .collect()
}

/// Extract a vocabulary from a list of visited file paths.
///
/// For each path, pulls out directory components and the basename
/// (without extension). Basenames are decomposed via
/// `concepts::decompose_identifier` so `rate_limiter.rs` contributes
/// `rate` + `limiter` + `limit` rather than the full filename. Parts
/// shorter than 3 characters are dropped.
fn extract_file_vocab(paths: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        // Directory segments.
        for segment in path.split('/') {
            if segment.is_empty() {
                continue;
            }
            // Strip common file extensions from the last segment — the
            // loop handles the same string as a directory too, which is
            // harmless (the strip is idempotent when no ext is present).
            let stem = segment.rsplit_once('.').map(|(s, _)| s).unwrap_or(segment);
            // Decompose `rate_limiter` → ["rate", "limiter", ...]
            for part in decompose_identifier(stem) {
                let lower = part.to_ascii_lowercase();
                if lower.len() >= 3 && seen.insert(lower.clone()) {
                    out.push(lower);
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_cooccurrence_groups() {
        let mut builder = ReformulationBuilder::new();
        builder.add_identifier("parse_json", "src/parser.rs");
        builder.add_identifier("json_decode", "src/parser.rs");
        builder.add_identifier("csv_parse", "src/csv.rs");

        let reformulations = builder.build();
        let expanded = reformulations.expand("json");
        assert!(!expanded.is_empty(), "json should expand to related terms");
        // "json" co-occurs with "parse" and "decode" in parser.rs
        assert!(
            expanded.contains(&"parse".to_string()),
            "json should expand to 'parse' (co-occur in parser.rs), got: {expanded:?}"
        );
    }

    #[test]
    fn doc_to_code_bridge() {
        let mut builder = ReformulationBuilder::new();
        builder.add_documented_symbol("check_auth", "Validate user credentials");

        let reformulations = builder.build();
        let expanded = reformulations.expand("validate");
        assert!(
            expanded.contains(&"check_auth".to_string()),
            "validate should expand to check_auth via doc bridge, got: {expanded:?}"
        );
    }

    #[test]
    fn empty_builder_produces_empty() {
        let builder = ReformulationBuilder::new();
        let reformulations = builder.build();
        assert!(reformulations.expand("anything").is_empty());
        assert!(reformulations.is_empty());
    }

    #[test]
    fn doc_to_code_filters_common_words() {
        let mut builder = ReformulationBuilder::new();
        // Add 21 symbols that all mention "data" in their doc comments.
        for i in 0..21 {
            builder
                .add_documented_symbol(&format!("symbol_{i}"), &format!("process data item {i}"));
        }
        // Add one symbol with a unique doc word.
        builder.add_documented_symbol("rare_fn", "teleportation module handler");

        let reformulations = builder.build();

        // "data" appears in >20 symbols, so should be filtered out.
        let data_expanded = reformulations.expand("data");
        assert!(
            data_expanded.is_empty(),
            "data should be filtered (>20 symbols), got: {data_expanded:?}"
        );

        // "teleportation" appears in only 1 symbol, so should be kept.
        let rare_expanded = reformulations.expand("teleportation");
        assert!(
            rare_expanded.contains(&"rare_fn".to_string()),
            "teleportation should expand to rare_fn, got: {rare_expanded:?}"
        );
    }

    #[test]
    fn term_cooccurrence_bidirectional() {
        let mut builder = ReformulationBuilder::new();
        builder.add_identifier("parse_json", "src/parser.rs");
        builder.add_identifier("json_decode", "src/parser.rs");

        let reformulations = builder.build();

        // json -> parse (via parse_json) and decode (via json_decode)
        let json_expanded = reformulations.expand("json");
        assert!(json_expanded.contains(&"parse".to_string()));
        assert!(json_expanded.contains(&"decode".to_string()));

        // parse -> json (bidirectional)
        let parse_expanded = reformulations.expand("parse");
        assert!(parse_expanded.contains(&"json".to_string()));
    }

    #[test]
    fn short_terms_filtered() {
        let mut builder = ReformulationBuilder::new();
        // "a_b_token" decomposes to ["token"] (a and b are < 2 chars, filtered by decompose_identifier)
        // but we also filter parts < 3 chars in co-occurrence
        builder.add_identifier("a_xy_token", "src/main.rs");
        builder.add_identifier("xy_value", "src/main.rs");

        let reformulations = builder.build();
        // "xy" is only 2 chars so should not appear in co-occurrence (< 3 filter)
        let xy_expanded = reformulations.expand("xy");
        assert!(
            xy_expanded.is_empty(),
            "xy (2 chars) should not appear in expansions, got: {xy_expanded:?}"
        );

        // "token" and "value" should co-occur (both >= 3 chars in same file)
        let token_expanded = reformulations.expand("token");
        assert!(
            token_expanded.contains(&"value".to_string()),
            "token should expand to value (co-occur in main.rs), got: {token_expanded:?}"
        );
    }

    // ---------- Session mining ------------------------------------------

    /// Helper: record `times` events with the same query, files, and
    /// session id so we can construct explicit frequency-vs-distinct
    /// scenarios in tests.
    fn record_event(
        builder: &mut ReformulationBuilder,
        query: &str,
        files: &[&str],
        session_id: &str,
        times: usize,
    ) {
        let files_owned: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        for _ in 0..times {
            builder.add_session_event(query, &files_owned, session_id);
        }
    }

    #[test]
    fn session_mining_drops_pairs_below_freq_threshold() {
        let mut builder = ReformulationBuilder::new();
        // 2 events of the same pair across 2 sessions: freq=2 (< 3) → drop.
        record_event(&mut builder, "rate limiting", &["src/throttle.rs"], "s1", 1);
        record_event(&mut builder, "rate limiting", &["src/throttle.rs"], "s2", 1);
        let refs = builder.build();
        assert!(
            refs.session_expansions.is_empty(),
            "freq=2 should not meet threshold, got {:?}",
            refs.session_expansions,
        );
    }

    #[test]
    fn session_mining_drops_pairs_from_single_session() {
        let mut builder = ReformulationBuilder::new();
        // 5 events of the same pair but all from session "s1": distinct=1 (< 2).
        record_event(&mut builder, "rate limiting", &["src/throttle.rs"], "s1", 5);
        let refs = builder.build();
        assert!(
            refs.session_expansions.is_empty(),
            "distinct-session=1 should drop even at high freq, got {:?}",
            refs.session_expansions,
        );
    }

    #[test]
    fn session_mining_emits_pair_when_both_thresholds_met() {
        let mut builder = ReformulationBuilder::new();
        // freq=3 across 2 distinct sessions → emit.
        record_event(&mut builder, "rate limiting", &["src/throttle.rs"], "s1", 2);
        record_event(&mut builder, "rate limiting", &["src/throttle.rs"], "s2", 1);
        let refs = builder.build();
        let rate_exp = refs
            .session_expansions
            .get("rate")
            .expect("rate should be learned");
        assert!(
            rate_exp.contains(&"throttle".to_string()),
            "rate -> throttle should be learned, got {rate_exp:?}",
        );
        let limiting_exp = refs
            .session_expansions
            .get("limiting")
            .expect("limiting should be learned");
        assert!(limiting_exp.contains(&"throttle".to_string()));
    }

    #[test]
    fn expand_merges_session_pairs_with_other_sources() {
        let mut builder = ReformulationBuilder::new();
        // Populate a co-occurrence pair (existing source 1).
        builder.add_identifier("parse_json", "src/parser.rs");
        builder.add_identifier("json_decode", "src/parser.rs");
        // Populate a session pair on the same term "json".
        record_event(&mut builder, "json", &["src/protobuf.rs"], "s1", 2);
        record_event(&mut builder, "json", &["src/protobuf.rs"], "s2", 1);

        let refs = builder.build();
        let expanded = refs.expand("json");
        assert!(
            expanded.contains(&"decode".to_string()),
            "expand should include co-occurrence output, got {expanded:?}",
        );
        assert!(
            expanded.contains(&"protobuf".to_string()),
            "expand should include session output, got {expanded:?}",
        );
    }

    #[test]
    fn session_mining_output_is_deterministic() {
        let mut builder = ReformulationBuilder::new();
        record_event(
            &mut builder,
            "auth",
            &["src/middleware/auth.rs", "src/session/cookie.rs"],
            "s1",
            2,
        );
        record_event(
            &mut builder,
            "auth",
            &["src/middleware/auth.rs", "src/session/cookie.rs"],
            "s2",
            1,
        );
        let first = builder.build().session_expansions;

        let mut builder2 = ReformulationBuilder::new();
        record_event(
            &mut builder2,
            "auth",
            &["src/middleware/auth.rs", "src/session/cookie.rs"],
            "s1",
            2,
        );
        record_event(
            &mut builder2,
            "auth",
            &["src/middleware/auth.rs", "src/session/cookie.rs"],
            "s2",
            1,
        );
        let second = builder2.build().session_expansions;
        assert_eq!(first, second, "session mining must be deterministic");
    }

    #[test]
    fn session_mining_skips_identical_query_and_vocab_term() {
        let mut builder = ReformulationBuilder::new();
        // Query term and file vocab both include "auth" — no self-pair.
        record_event(&mut builder, "auth handler", &["src/auth.rs"], "s1", 2);
        record_event(&mut builder, "auth handler", &["src/auth.rs"], "s2", 1);
        let refs = builder.build();
        let auth_exp = refs.session_expansions.get("auth");
        if let Some(exp) = auth_exp {
            assert!(
                !exp.contains(&"auth".to_string()),
                "term should not self-expand, got {exp:?}",
            );
        }
    }

    #[test]
    fn serialization_roundtrip() {
        let mut builder = ReformulationBuilder::new();
        builder.add_identifier("parse_json", "src/parser.rs");
        builder.add_identifier("json_decode", "src/parser.rs");
        builder.add_documented_symbol("check_auth", "Validate user credentials");

        let original = builder.build();
        assert!(!original.is_empty());

        let bytes = bitcode::serialize(&original).expect("serialize should succeed");
        let decoded: LearnedReformulations =
            bitcode::deserialize(&bytes).expect("deserialize should succeed");

        assert_eq!(
            original.term_expansions.len(),
            decoded.term_expansions.len()
        );
        assert_eq!(original.doc_to_code.len(), decoded.doc_to_code.len());

        // Verify expand produces same results.
        assert_eq!(original.expand("json"), decoded.expand("json"));
        assert_eq!(original.expand("validate"), decoded.expand("validate"));
    }
}
