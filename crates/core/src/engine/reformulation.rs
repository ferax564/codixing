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
}

impl LearnedReformulations {
    /// Returns related terms and code identifiers for a given term.
    ///
    /// Merges results from both `term_expansions` (co-occurrence) and
    /// `doc_to_code` (documentation bridge), deduplicating the output.
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

        result
    }

    /// Returns `true` if no reformulations have been learned.
    pub fn is_empty(&self) -> bool {
        self.term_expansions.is_empty() && self.doc_to_code.is_empty()
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

/// Builds [`LearnedReformulations`] from symbol data.
///
/// Feed identifiers and documented symbols, then call [`build`] to produce
/// the learned reformulation map.
#[derive(Debug, Default)]
pub struct ReformulationBuilder {
    identifiers: Vec<IdentifierRecord>,
    doc_symbols: Vec<DocSymbolRecord>,
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

        LearnedReformulations {
            term_expansions,
            doc_to_code,
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
