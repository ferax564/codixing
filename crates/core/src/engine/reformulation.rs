//! Project-specific learned query reformulations.
//!
//! Supplements the hardcoded synonym map (~80 groups in `engine/synonyms.rs`)
//! with vocabulary learned from the project's own codebase:
//!
//! 1. **Term co-occurrence** — identifiers that share a file are related.
//! 2. **Doc-to-code bridge** — doc comment words map to symbol names.
//!
//! Built during `Engine::init()` and loaded from disk during `Engine::open()`.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::concepts::{decompose_identifier, extract_ranked_concept_words};

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

    pub(super) fn encode_persisted(&self) -> std::result::Result<Vec<u8>, String> {
        let compact = CompactReformulations::from_reformulations(self)?;
        let payload = bitcode::serialize(&compact).map_err(|error| error.to_string())?;
        let mut bytes = Vec::with_capacity(REFORMULATION_FORMAT_MAGIC.len() + payload.len());
        bytes.extend_from_slice(REFORMULATION_FORMAT_MAGIC);
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    pub(super) fn decode_persisted(bytes: &[u8]) -> std::result::Result<Self, String> {
        if let Some(payload) = bytes.strip_prefix(REFORMULATION_FORMAT_MAGIC) {
            let compact: CompactReformulations =
                bitcode::deserialize(payload).map_err(|error| error.to_string())?;
            compact.into_reformulations()
        } else {
            bitcode::deserialize(bytes).map_err(|error| error.to_string())
        }
    }
}

const REFORMULATION_FORMAT_MAGIC: &[u8] = b"CXRF2\0";

#[derive(Debug, Serialize, Deserialize)]
struct CompactReformulations {
    strings: Vec<String>,
    term_expansions: Vec<(u32, Vec<u32>)>,
    doc_to_code: Vec<(u32, Vec<u32>)>,
    session_expansions: Vec<(u32, Vec<u32>)>,
}

impl CompactReformulations {
    fn from_reformulations(value: &LearnedReformulations) -> std::result::Result<Self, String> {
        let mut strings = BTreeSet::new();
        for map in [
            &value.term_expansions,
            &value.doc_to_code,
            &value.session_expansions,
        ] {
            for (term, expansions) in map {
                strings.insert(term.clone());
                strings.extend(expansions.iter().cloned());
            }
        }
        let strings: Vec<String> = strings.into_iter().collect();
        let ids: BTreeMap<&str, u32> = strings
            .iter()
            .enumerate()
            .map(|(idx, value)| {
                u32::try_from(idx)
                    .map(|idx| (value.as_str(), idx))
                    .map_err(|_| "reformulation string table exceeds u32".to_string())
            })
            .collect::<std::result::Result<_, _>>()?;
        let term_expansions = encode_expansion_map(&value.term_expansions, &ids)?;
        let doc_to_code = encode_expansion_map(&value.doc_to_code, &ids)?;
        let session_expansions = encode_expansion_map(&value.session_expansions, &ids)?;
        Ok(Self {
            strings,
            term_expansions,
            doc_to_code,
            session_expansions,
        })
    }

    fn into_reformulations(self) -> std::result::Result<LearnedReformulations, String> {
        Ok(LearnedReformulations {
            term_expansions: decode_expansion_map(&self.strings, self.term_expansions)?,
            doc_to_code: decode_expansion_map(&self.strings, self.doc_to_code)?,
            session_expansions: decode_expansion_map(&self.strings, self.session_expansions)?,
        })
    }
}

fn encode_expansion_map(
    map: &HashMap<String, Vec<String>>,
    ids: &BTreeMap<&str, u32>,
) -> std::result::Result<Vec<(u32, Vec<u32>)>, String> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(term, expansions)| {
            let term = ids
                .get(term.as_str())
                .copied()
                .ok_or_else(|| format!("missing interned reformulation term: {term}"))?;
            let expansions = expansions
                .iter()
                .map(|expansion| {
                    ids.get(expansion.as_str()).copied().ok_or_else(|| {
                        format!("missing interned reformulation expansion: {expansion}")
                    })
                })
                .collect::<std::result::Result<_, _>>()?;
            Ok((term, expansions))
        })
        .collect()
}

fn decode_expansion_map(
    strings: &[String],
    entries: Vec<(u32, Vec<u32>)>,
) -> std::result::Result<HashMap<String, Vec<String>>, String> {
    let resolve = |id: u32| {
        strings
            .get(id as usize)
            .cloned()
            .ok_or_else(|| format!("reformulation string id {id} is out of bounds"))
    };
    let mut map = HashMap::with_capacity(entries.len());
    for (term, expansions) in entries {
        map.insert(
            resolve(term)?,
            expansions
                .into_iter()
                .map(resolve)
                .collect::<std::result::Result<_, _>>()?,
        );
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Intermediate record of an identifier and its file location.
#[derive(Debug)]
struct IdentifierRecord {
    name: String,
    file: String,
    part_count: usize,
}

/// Intermediate record of a documented symbol.
#[derive(Debug)]
struct DocSymbolRecord {
    name: String,
    doc_words: Vec<String>,
    word_count: usize,
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

/// Hard limits for learned vocabulary construction. These are intentionally
/// constants rather than user-facing knobs: they are safety invariants, and
/// tuning them per repository would make index size and query behaviour hard
/// to predict.
///
/// The old builder connected every vocabulary word in a file to every other
/// word (`O(k^2)`) and retained every resulting edge. A generated file with
/// thousands of identifiers could therefore dominate both RAM and disk. The
/// ranked limits below retain the terms with the strongest within-file and
/// cross-file evidence while making every stage explicitly bounded.
pub const MAX_REFORMULATION_TERMS: usize = 16_384;
pub const MAX_TERMS_PER_FILE: usize = 32;
pub const MAX_CANDIDATES_PER_TERM: usize = 24;
pub const MAX_EXPANSIONS_PER_TERM: usize = 12;
pub const MAX_DOC_TERMS: usize = 16_384;
pub const MAX_DOC_WORDS_PER_SYMBOL: usize = 8;
pub const MAX_DOC_SYMBOLS_PER_TERM: usize = 8;
const MAX_SOURCE_IDENTIFIERS: usize = 200_000;
const MAX_DOCUMENTED_SYMBOLS: usize = 100_000;
const MAX_SESSION_EVENTS: usize = 10_000;
const MAX_SESSION_QUERY_TERMS: usize = 16;
const MAX_SESSION_FILE_TERMS: usize = 32;

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
            part_count: decompose_identifier(name).len(),
        });
        if self.identifiers.len() >= MAX_SOURCE_IDENTIFIERS.saturating_mul(2) {
            retain_best_identifiers(&mut self.identifiers);
        }
    }

    /// Register a documented symbol for doc-to-code bridge building.
    pub fn add_documented_symbol(&mut self, name: &str, doc: &str) {
        let doc_words = extract_ranked_concept_words(doc, MAX_DOC_WORDS_PER_SYMBOL);
        self.doc_symbols.push(DocSymbolRecord {
            name: name.to_string(),
            word_count: doc_words.len(),
            doc_words,
        });
        if self.doc_symbols.len() >= MAX_DOCUMENTED_SYMBOLS.saturating_mul(2) {
            retain_best_doc_symbols(&mut self.doc_symbols);
        }
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
        if self.session_events.len() >= MAX_SESSION_EVENTS {
            return;
        }
        let mut query_terms = tokenize_query_for_session(query);
        query_terms.sort_by_key(|term| (Reverse(term.len()), term.clone()));
        query_terms.dedup();
        query_terms.truncate(MAX_SESSION_QUERY_TERMS);
        if query_terms.is_empty() {
            return;
        }
        let mut file_vocab = extract_file_vocab(visited_files);
        file_vocab.sort_by_key(|term| (Reverse(term.len()), term.clone()));
        file_vocab.dedup();
        file_vocab.truncate(MAX_SESSION_FILE_TERMS);
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
    pub fn build(mut self) -> LearnedReformulations {
        retain_best_identifiers(&mut self.identifiers);
        retain_best_doc_symbols(&mut self.doc_symbols);
        self.identifiers
            .sort_by(|a, b| (&a.file, &a.name).cmp(&(&b.file, &b.name)));
        self.identifiers
            .dedup_by(|a, b| a.file == b.file && a.name == b.name);
        self.doc_symbols
            .sort_by(|a, b| (&a.name, &a.doc_words).cmp(&(&b.name, &b.doc_words)));
        self.doc_symbols
            .dedup_by(|a, b| a.name == b.name && a.doc_words == b.doc_words);
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
        // Group identifiers by file and count within-file evidence. A BTreeMap
        // makes the later bounded pass independent of HashMap random seeding.
        let mut file_to_parts: BTreeMap<&str, HashMap<String, u16>> = BTreeMap::new();
        for rec in &self.identifiers {
            let mut parts = decompose_identifier(&rec.name);
            parts.retain(|p| p.len() >= 3);
            parts.sort();
            parts.dedup();
            let counts = file_to_parts.entry(rec.file.as_str()).or_default();
            for part in parts {
                let count = counts.entry(part).or_default();
                *count = count.saturating_add(1);
            }
        }

        // Cross-file document frequency ranks vocabulary by repeated evidence.
        // Rare generated terms are discarded before any pair enumeration.
        let mut document_frequency: HashMap<String, u32> = HashMap::new();
        for counts in file_to_parts.values() {
            for term in counts.keys() {
                *document_frequency.entry(term.clone()).or_default() += 1;
            }
        }
        let mut vocabulary: Vec<(String, u32)> = document_frequency.into_iter().collect();
        vocabulary.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.0.len().cmp(&a.0.len()))
                .then_with(|| a.0.cmp(&b.0))
        });
        vocabulary.truncate(MAX_REFORMULATION_TERMS);
        let vocabulary: HashSet<String> = vocabulary.into_iter().map(|(term, _)| term).collect();

        // Each file contributes at most MAX_TERMS_PER_FILE terms, ranked by
        // within-file frequency and then lexical order. Pair generation is now
        // O(files * MAX_TERMS_PER_FILE^2), never O(unbounded k^2).
        let mut term_links: HashMap<String, HashMap<String, u32>> = HashMap::new();
        for counts in file_to_parts.values() {
            let mut parts_vec: Vec<(&String, u16)> = counts
                .iter()
                .filter(|(term, _)| vocabulary.contains(*term))
                .map(|(term, &count)| (term, count))
                .collect();
            parts_vec.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then_with(|| b.0.len().cmp(&a.0.len()))
                    .then_with(|| a.0.cmp(b.0))
            });
            parts_vec.truncate(MAX_TERMS_PER_FILE);
            for i in 0..parts_vec.len() {
                for j in (i + 1)..parts_vec.len() {
                    let a = parts_vec[i].0;
                    let b = parts_vec[j].0;
                    if a != b {
                        increment_bounded_candidate(&mut term_links, a, b);
                        increment_bounded_candidate(&mut term_links, b, a);
                    }
                }
            }
        }

        // Keep the strongest co-occurrences. Lexical tie-breaking makes the
        // compact persisted bytes reproducible across processes.
        term_links
            .into_iter()
            .map(|(term, candidates)| {
                let mut ranked: Vec<(String, u32)> = candidates.into_iter().collect();
                ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                ranked.truncate(MAX_EXPANSIONS_PER_TERM);
                (
                    term,
                    ranked.into_iter().map(|(candidate, _)| candidate).collect(),
                )
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

        // Apply thresholds, rank by repeated/distinct-session evidence, and
        // keep the same per-term bound as the other reformulation sources.
        let mut out: HashMap<String, Vec<(String, usize, usize)>> = HashMap::new();
        for ((qt, fv), (freq, sessions)) in pair_stats {
            if freq >= SESSION_FREQ_THRESHOLD
                && sessions.len() >= SESSION_DISTINCT_SESSIONS_THRESHOLD
            {
                out.entry(qt).or_default().push((fv, freq, sessions.len()));
            }
        }

        out.into_iter()
            .map(|(term, mut candidates)| {
                candidates.sort_by(|a, b| {
                    b.1.cmp(&a.1)
                        .then_with(|| b.2.cmp(&a.2))
                        .then_with(|| a.0.cmp(&b.0))
                });
                candidates.truncate(MAX_EXPANSIONS_PER_TERM);
                (
                    term,
                    candidates
                        .into_iter()
                        .map(|(candidate, _, _)| candidate)
                        .collect(),
                )
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
            for word in &rec.doc_words {
                let symbols = word_to_symbols.entry(word.clone()).or_default();
                // 21 is the generic-term sentinel used by the filter below.
                if symbols.len() <= 20 {
                    symbols.insert(rec.name.clone());
                }
            }
        }

        // Filter out overly common words (appearing in >20 symbols).
        let max_symbol_count = 20;

        let mut ranked_terms: Vec<(String, HashSet<String>)> = word_to_symbols
            .into_iter()
            .filter(|(_word, symbols)| symbols.len() <= max_symbol_count)
            .collect();
        ranked_terms.sort_by(|a, b| {
            b.1.len()
                .cmp(&a.1.len())
                .then_with(|| b.0.len().cmp(&a.0.len()))
                .then_with(|| a.0.cmp(&b.0))
        });
        ranked_terms.truncate(MAX_DOC_TERMS);
        ranked_terms
            .into_iter()
            .map(|(word, symbols)| {
                let mut sorted: Vec<String> = symbols.into_iter().collect();
                sorted.sort();
                sorted.truncate(MAX_DOC_SYMBOLS_PER_TERM);
                (word, sorted)
            })
            .collect()
    }
}

fn retain_best_identifiers(records: &mut Vec<IdentifierRecord>) {
    if records.len() <= MAX_SOURCE_IDENTIFIERS {
        return;
    }
    records.sort_by(|a, b| {
        b.part_count
            .cmp(&a.part_count)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });
    records.truncate(MAX_SOURCE_IDENTIFIERS);
}

fn retain_best_doc_symbols(records: &mut Vec<DocSymbolRecord>) {
    if records.len() <= MAX_DOCUMENTED_SYMBOLS {
        return;
    }
    records.sort_by(|a, b| {
        b.word_count
            .cmp(&a.word_count)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.doc_words.cmp(&b.doc_words))
    });
    records.truncate(MAX_DOCUMENTED_SYMBOLS);
}

fn increment_bounded_candidate(
    links: &mut HashMap<String, HashMap<String, u32>>,
    term: &str,
    candidate: &str,
) {
    let candidates = links.entry(term.to_string()).or_default();
    let count = candidates.entry(candidate.to_string()).or_default();
    *count = count.saturating_add(1);
    if candidates.len() > MAX_CANDIDATES_PER_TERM.saturating_mul(2) {
        let mut ranked: Vec<(String, u32)> = candidates.drain().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(MAX_CANDIDATES_PER_TERM);
        candidates.extend(ranked);
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

    #[test]
    fn pathological_file_vocabulary_is_ranked_and_bounded() {
        let mut builder = ReformulationBuilder::new();
        for idx in 0..500 {
            // `shared` has the strongest evidence and must survive alongside a
            // bounded selection of generated one-off vocabulary.
            builder.add_identifier(&format!("shared_generated_term_{idx}"), "src/generated.rs");
        }
        builder.add_identifier("shared_auth_token", "src/generated.rs");
        let reformulations = builder.build();
        assert!(reformulations.term_expansions.len() <= MAX_REFORMULATION_TERMS);
        assert!(
            reformulations
                .term_expansions
                .values()
                .all(|values| values.len() <= MAX_EXPANSIONS_PER_TERM)
        );
        assert!(
            !reformulations.expand("shared").is_empty(),
            "the repeated high-evidence term should retain useful neighbors"
        );
    }

    #[test]
    fn compact_reformulations_are_deterministic_across_input_order() {
        fn build(reverse: bool) -> LearnedReformulations {
            let mut symbols = vec![
                ("auth_login", "src/auth.rs", "Authenticate credentials"),
                ("auth_token", "src/auth.rs", "Validate credentials"),
                ("cache_read", "src/cache.rs", "Read cached values"),
                ("cache_write", "src/cache.rs", "Write cached values"),
            ];
            if reverse {
                symbols.reverse();
            }
            let mut builder = ReformulationBuilder::new();
            for (name, file, doc) in symbols {
                builder.add_identifier(name, file);
                builder.add_documented_symbol(name, doc);
            }
            builder.build()
        }
        assert_eq!(
            build(false).encode_persisted().unwrap(),
            build(true).encode_persisted().unwrap()
        );
    }

    #[test]
    fn interned_reformulation_format_is_less_than_half_legacy_size() {
        let shared: Vec<String> = (0..12).map(|idx| format!("shared_term_{idx}")).collect();
        let reformulations = LearnedReformulations {
            term_expansions: (0..2_000)
                .map(|idx| (format!("source_term_{idx}"), shared.clone()))
                .collect(),
            doc_to_code: (0..1_000)
                .map(|idx| (format!("doc_term_{idx}"), shared[..8].to_vec()))
                .collect(),
            session_expansions: HashMap::new(),
        };
        let legacy = bitcode::serialize(&reformulations).unwrap();
        let compact = reformulations.encode_persisted().unwrap();
        assert!(
            compact.len().saturating_mul(2) <= legacy.len(),
            "compact={} legacy={}",
            compact.len(),
            legacy.len()
        );
        let decoded = LearnedReformulations::decode_persisted(&compact).unwrap();
        assert_eq!(
            decoded.expand("source_term_42"),
            reformulations.expand("source_term_42")
        );
    }

    #[test]
    fn persisted_reformulation_decoder_accepts_legacy_bytes() {
        let mut builder = ReformulationBuilder::new();
        builder.add_identifier("parse_json", "src/parser.rs");
        builder.add_identifier("json_decode", "src/parser.rs");
        let original = builder.build();
        let legacy = bitcode::serialize(&original).unwrap();
        let decoded = LearnedReformulations::decode_persisted(&legacy).unwrap();
        assert_eq!(decoded.expand("json"), original.expand("json"));
    }
}
