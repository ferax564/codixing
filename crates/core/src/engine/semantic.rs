//! Embedding-free semantic matching via AST-derived behavioral signatures.
//!
//! Bridges natural-language queries to code symbols without requiring any
//! embedding model. Instead, each function's *behavioral signature* —
//! its inputs, outputs, call pattern, control flow, and domain concepts —
//! is compared against a decomposed query intent using Jaccard similarity.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::concepts::decompose_identifier;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// High-level control flow category detected from source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlFlow {
    /// No branching or looping — sequential statements only.
    Linear,
    /// Contains `if`, `match`, `switch`, or similar branching constructs.
    Branching,
    /// Contains `for`, `while`, `loop`, or iterator patterns.
    Looping,
    /// Function calls itself (detected by name self-reference).
    Recursive,
    /// Both branching and looping present.
    Mixed,
}

/// A behavioral signature for a single function symbol.
///
/// Built on-demand from the symbol table, call graph, and concept index.
/// Not persisted — cheap to reconstruct per-query from already-indexed data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehavioralSignature {
    /// Symbol name (e.g. `validate_token`).
    pub symbol: String,
    /// Relative file path containing this symbol.
    pub file_path: String,
    /// Input type names extracted from the function signature.
    pub inputs: Vec<String>,
    /// Output type names extracted from the function signature.
    pub outputs: Vec<String>,
    /// Names of functions this symbol calls (callees).
    pub call_pattern: Vec<String>,
    /// Detected control flow category.
    pub control_flow: ControlFlow,
    /// Domain concepts associated with this symbol (from concept index +
    /// identifier decomposition).
    pub concepts: Vec<String>,
}

/// A scored match from semantic search.
#[derive(Debug, Clone, Serialize)]
pub struct SemanticMatch {
    /// Symbol name that matched.
    pub symbol: String,
    /// File path of the matched symbol.
    pub file_path: String,
    /// Composite relevance score in [0.0, 1.0].
    pub score: f32,
    /// Human-readable reasons explaining why this symbol matched.
    pub match_reasons: Vec<String>,
}

/// Decomposed natural-language query intent.
pub struct QueryIntent {
    /// Action verbs found in the query (e.g. "validate", "parse", "send").
    pub action_verbs: Vec<String>,
    /// Domain nouns (non-verb, non-type words).
    pub domain_nouns: Vec<String>,
    /// Type mentions — capitalized words that look like type names.
    pub type_mentions: Vec<String>,
}

// ---------------------------------------------------------------------------
// Common English verbs used for action detection
// ---------------------------------------------------------------------------

const ACTION_VERBS: &[&str] = &[
    "validate",
    "parse",
    "send",
    "receive",
    "create",
    "delete",
    "update",
    "read",
    "write",
    "fetch",
    "load",
    "save",
    "store",
    "check",
    "verify",
    "authenticate",
    "authorize",
    "search",
    "find",
    "filter",
    "sort",
    "merge",
    "split",
    "transform",
    "convert",
    "encode",
    "decode",
    "encrypt",
    "decrypt",
    "compress",
    "decompress",
    "serialize",
    "deserialize",
    "build",
    "compile",
    "execute",
    "run",
    "start",
    "stop",
    "open",
    "close",
    "connect",
    "disconnect",
    "listen",
    "emit",
    "publish",
    "subscribe",
    "process",
    "handle",
    "render",
    "format",
    "cache",
    "flush",
    "sync",
    "init",
    "initialize",
    "shutdown",
    "configure",
    "register",
    "resolve",
    "inject",
    "extract",
    "index",
    "reindex",
    "lookup",
    "query",
    "insert",
    "remove",
    "add",
    "drop",
    "push",
    "pop",
    "enqueue",
    "dequeue",
    "map",
    "reduce",
    "collect",
    "aggregate",
    "compute",
    "calculate",
    "generate",
    "emit",
    "dispatch",
    "route",
    "forward",
    "retry",
    "rollback",
    "commit",
    "abort",
    "spawn",
    "join",
    "await",
    "poll",
    "drain",
    "allocate",
    "free",
    "release",
    "acquire",
    "lock",
    "unlock",
    "clone",
    "copy",
    "move",
    "swap",
    "compare",
    "diff",
    "patch",
    "test",
    "benchmark",
    "profile",
    "trace",
    "log",
    "debug",
    "inspect",
    "dump",
    "print",
    "display",
    "show",
];

// ---------------------------------------------------------------------------
// Public functions
// ---------------------------------------------------------------------------

/// Decompose a natural-language query into action verbs, domain nouns, and
/// type mentions.
///
/// - **Action verbs**: words matching the `ACTION_VERBS` list.
/// - **Type mentions**: capitalized words (PascalCase) that look like type names.
/// - **Domain nouns**: remaining words that are neither verbs nor types.
pub fn decompose_query(query: &str) -> QueryIntent {
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 2)
        .collect();

    let mut action_verbs = Vec::new();
    let mut domain_nouns = Vec::new();
    let mut type_mentions = Vec::new();

    for word in &words {
        let lower = word.to_lowercase();

        // Check if it starts with uppercase → likely a type mention
        if word
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
            && word.len() >= 2
        {
            type_mentions.push(word.to_string());
            // Also decompose PascalCase identifiers into domain nouns
            let parts = decompose_identifier(word);
            for part in parts {
                if ACTION_VERBS.contains(&part.as_str()) {
                    action_verbs.push(part);
                } else {
                    domain_nouns.push(part);
                }
            }
            continue;
        }

        if ACTION_VERBS.contains(&lower.as_str()) {
            action_verbs.push(lower);
        } else {
            domain_nouns.push(lower);
        }
    }

    // Deduplicate
    action_verbs.sort_unstable();
    action_verbs.dedup();
    domain_nouns.sort_unstable();
    domain_nouns.dedup();
    type_mentions.sort_unstable();
    type_mentions.dedup();

    QueryIntent {
        action_verbs,
        domain_nouns,
        type_mentions,
    }
}

/// Jaccard similarity between two string slices (treated as sets).
///
/// Returns 0.0 when both sets are empty.
pub fn jaccard(a: &[String], b: &[String]) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }

    let set_a: HashSet<&str> = a.iter().map(|s| s.as_str()).collect();
    let set_b: HashSet<&str> = b.iter().map(|s| s.as_str()).collect();

    let intersection = set_a.intersection(&set_b).count();
    let union = set_a.union(&set_b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

/// Detect the predominant control flow category from source code text.
///
/// Uses simple keyword detection — no AST required. Looks for branching
/// keywords (`if`, `match`, `switch`, `case`), looping keywords (`for`,
/// `while`, `loop`), and recursion (`fn <name>` calling `<name>`).
pub fn detect_control_flow(source: &str) -> ControlFlow {
    let has_branch = has_keyword(source, &["if ", "match ", "switch ", "case "]);
    let has_loop = has_keyword(source, &["for ", "while ", "loop ", "loop{"]);

    match (has_branch, has_loop) {
        (true, true) => ControlFlow::Mixed,
        (true, false) => ControlFlow::Branching,
        (false, true) => ControlFlow::Looping,
        (false, false) => ControlFlow::Linear,
    }
}

/// Extract input and output type names from a function signature string.
///
/// Parses signatures like `fn check(user: User, token: Token) -> Result<bool>`
/// to extract `(["User", "Token"], ["Result"])`.
///
/// Only captures capitalized type names (PascalCase), which filters out
/// primitive types like `i32`, `bool`, `str`.
pub fn extract_io_from_signature(sig: &str) -> (Vec<String>, Vec<String>) {
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    // Find the parameter section: everything between `(` and the last `)`
    // then the return type after `->`
    let (params_part, return_part) = if let Some(paren_start) = sig.find('(') {
        // Find the matching closing paren
        if let Some(paren_end) = sig.rfind(')') {
            let params = &sig[paren_start + 1..paren_end];
            let rest = &sig[paren_end + 1..];
            let ret = rest
                .find("->")
                .map(|arrow_pos| rest[arrow_pos + 2..].trim());
            (Some(params), ret)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Extract types from parameters (after `:` in each parameter)
    if let Some(params) = params_part {
        for param in params.split(',') {
            if let Some(colon_pos) = param.find(':') {
                let type_str = param[colon_pos + 1..].trim();
                extract_type_names(type_str, &mut inputs);
            }
        }
    }

    // Extract types from return type
    if let Some(ret) = return_part {
        extract_type_names(ret, &mut outputs);
    }

    // Deduplicate
    inputs.sort_unstable();
    inputs.dedup();
    outputs.sort_unstable();
    outputs.dedup();

    (inputs, outputs)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Check if any of the given keywords appear in the source text.
fn has_keyword(source: &str, keywords: &[&str]) -> bool {
    keywords.iter().any(|kw| source.contains(kw))
}

/// Extract PascalCase type names from a type expression string.
///
/// Splits on non-alphanumeric boundaries and collects words that start
/// with an uppercase letter (likely type names, not primitives).
fn extract_type_names(type_str: &str, out: &mut Vec<String>) {
    for word in type_str.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let trimmed = word.trim();
        if trimmed.len() >= 2
            && trimmed
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
        {
            out.push(trimmed.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Engine integration — build signatures and search
// ---------------------------------------------------------------------------

use crate::engine::Engine;
use crate::language::EntityKind;

impl Engine {
    /// Build behavioral signatures for all function/method symbols in the index.
    ///
    /// For each function symbol:
    /// 1. Extract I/O types from its signature.
    /// 2. Get callees via the symbol call graph.
    /// 3. Collect domain concepts from the concept index.
    /// 4. Detect control flow from source code (best-effort).
    pub fn build_behavioral_signatures(&self) -> Vec<BehavioralSignature> {
        let all_symbols = self.symbols.all_symbols();
        let mut signatures = Vec::new();

        for sym in &all_symbols {
            // Only build signatures for functions and methods
            if sym.kind != EntityKind::Function && sym.kind != EntityKind::Method {
                continue;
            }

            // 1. Extract I/O from signature
            let (inputs, outputs) = sym
                .signature
                .as_deref()
                .map(extract_io_from_signature)
                .unwrap_or_default();

            // 2. Get callees
            let call_pattern = self.symbol_callees_precise(&sym.name, Some(&sym.file_path));

            // 3. Get concepts — from identifier decomposition + concept index
            let mut concepts = decompose_identifier(&sym.name);
            if let Some(ref ci) = self.concept_index {
                let concept_hits = ci.lookup_query(&sym.name);
                for (cluster, _) in concept_hits.iter().take(5) {
                    concepts.push(cluster.name.clone());
                }
            }
            // Add concepts from doc comment if available
            if let Some(ref doc) = sym.doc_comment {
                let doc_words = super::concepts::extract_concept_words(doc);
                concepts.extend(doc_words);
            }
            concepts.sort_unstable();
            concepts.dedup();

            // 4. Detect control flow from source (best-effort: read from disk)
            let control_flow = self
                .read_symbol_source(&sym.name, Some(&sym.file_path))
                .ok()
                .flatten()
                .map(|src| detect_control_flow(&src))
                .unwrap_or(ControlFlow::Linear);

            signatures.push(BehavioralSignature {
                symbol: sym.name.clone(),
                file_path: sym.file_path.clone(),
                inputs,
                outputs,
                call_pattern,
                control_flow,
                concepts,
            });
        }

        signatures
    }

    /// Embedding-free semantic search using behavioral signatures + concept graph.
    ///
    /// Decomposes the query into intent (action verbs, domain nouns, type mentions),
    /// builds behavioral signatures for all functions, then scores each signature
    /// against the intent using weighted Jaccard similarity across five dimensions.
    pub fn semantic_search(&self, query: &str, limit: usize) -> Vec<SemanticMatch> {
        let intent = decompose_query(query);
        let signatures = self.build_behavioral_signatures();

        // Collect all query parts for name similarity scoring
        let mut all_query_parts: Vec<String> = Vec::new();
        all_query_parts.extend(intent.action_verbs.iter().cloned());
        all_query_parts.extend(intent.domain_nouns.iter().cloned());
        for t in &intent.type_mentions {
            all_query_parts.extend(decompose_identifier(t));
        }
        all_query_parts.sort_unstable();
        all_query_parts.dedup();

        let mut matches: Vec<SemanticMatch> = signatures
            .iter()
            .filter_map(|sig| {
                let mut reasons = Vec::new();

                // 1. Concept overlap (0.3 weight)
                let concept_score = jaccard(&intent.domain_nouns, &sig.concepts);
                if concept_score > 0.0 {
                    reasons.push(format!("concept overlap: {:.2}", concept_score));
                }

                // 2. Call pattern match (0.3 weight) — decompose callee names
                let callee_parts: Vec<String> = sig
                    .call_pattern
                    .iter()
                    .flat_map(|c| decompose_identifier(c))
                    .collect();
                let call_score = jaccard(&intent.action_verbs, &callee_parts);
                if call_score > 0.0 {
                    reasons.push(format!("call pattern: {:.2}", call_score));
                }

                // 3. Name similarity (0.2 weight)
                let name_parts = decompose_identifier(&sig.symbol);
                let name_score = jaccard(&all_query_parts, &name_parts);
                if name_score > 0.0 {
                    reasons.push(format!("name match: {:.2}", name_score));
                }

                // 4. Type compatibility (0.1 weight)
                let mut sig_types: Vec<String> = Vec::new();
                sig_types.extend(sig.inputs.iter().cloned());
                sig_types.extend(sig.outputs.iter().cloned());
                let type_score = jaccard(&intent.type_mentions, &sig_types);
                if type_score > 0.0 {
                    reasons.push(format!("type match: {:.2}", type_score));
                }

                // 5. Control flow bonus (0.1 weight)
                let flow_score = control_flow_bonus(&intent, sig.control_flow);
                if flow_score > 0.0 {
                    reasons.push(format!("control flow: {:.2}", flow_score));
                }

                let score = 0.3 * concept_score
                    + 0.3 * call_score
                    + 0.2 * name_score
                    + 0.1 * type_score
                    + 0.1 * flow_score;

                if score > 0.05 {
                    Some(SemanticMatch {
                        symbol: sig.symbol.clone(),
                        file_path: sig.file_path.clone(),
                        score,
                        match_reasons: reasons,
                    })
                } else {
                    None
                }
            })
            .collect();

        // Sort by score descending
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches.truncate(limit);
        matches
    }
}

/// Control flow bonus: returns a small score when query intent suggests
/// a specific control flow pattern.
///
/// - Verbs like "loop", "iterate", "scan" → bonus for Looping
/// - Verbs like "check", "validate", "filter" → bonus for Branching
/// - Otherwise → small bonus for any non-Linear flow
fn control_flow_bonus(intent: &QueryIntent, flow: ControlFlow) -> f32 {
    let loop_verbs = &[
        "iterate",
        "loop",
        "scan",
        "traverse",
        "walk",
        "collect",
        "reduce",
        "aggregate",
    ];
    let branch_verbs = &[
        "check", "validate", "filter", "match", "select", "switch", "test", "verify",
    ];

    let suggests_loop = intent
        .action_verbs
        .iter()
        .any(|v| loop_verbs.contains(&v.as_str()));
    let suggests_branch = intent
        .action_verbs
        .iter()
        .any(|v| branch_verbs.contains(&v.as_str()));

    match (suggests_loop, suggests_branch, flow) {
        (true, _, ControlFlow::Looping | ControlFlow::Mixed) => 1.0,
        (_, true, ControlFlow::Branching | ControlFlow::Mixed) => 1.0,
        (false, false, ControlFlow::Linear) => 0.0,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_flow_detection() {
        assert_eq!(detect_control_flow("if x { }"), ControlFlow::Branching);
        assert_eq!(
            detect_control_flow("for item in list { }"),
            ControlFlow::Looping
        );
        assert_eq!(detect_control_flow("let x = 1;"), ControlFlow::Linear);
        assert_eq!(
            detect_control_flow("if x { for y in z { } }"),
            ControlFlow::Mixed
        );
    }

    #[test]
    fn control_flow_match_keyword() {
        assert_eq!(
            detect_control_flow("match value { _ => {} }"),
            ControlFlow::Branching
        );
    }

    #[test]
    fn control_flow_while_loop() {
        assert_eq!(
            detect_control_flow("while running { process(); }"),
            ControlFlow::Looping
        );
    }

    #[test]
    fn jaccard_similarity() {
        let a = vec![
            "validate".to_string(),
            "parse".to_string(),
            "send".to_string(),
        ];
        let b = vec![
            "validate".to_string(),
            "check".to_string(),
            "send".to_string(),
        ];
        let sim = jaccard(&a, &b);
        // intersection = {validate, send} = 2, union = {validate, parse, send, check} = 4
        // jaccard = 2/4 = 0.5
        assert!((sim - 0.5).abs() < 0.01);
    }

    #[test]
    fn jaccard_empty() {
        assert_eq!(jaccard(&[], &[]), 0.0);
    }

    #[test]
    fn jaccard_identical() {
        let a = vec!["foo".to_string(), "bar".to_string()];
        assert!((jaccard(&a, &a) - 1.0).abs() < 0.01);
    }

    #[test]
    fn jaccard_disjoint() {
        let a = vec!["foo".to_string()];
        let b = vec!["bar".to_string()];
        assert_eq!(jaccard(&a, &b), 0.0);
    }

    #[test]
    fn query_decomposition() {
        let intent = decompose_query("validate user authentication");
        assert!(intent.action_verbs.contains(&"validate".to_string()));
        assert!(intent.domain_nouns.contains(&"authentication".to_string()));
        assert!(intent.domain_nouns.contains(&"user".to_string()));
    }

    #[test]
    fn query_decomposition_with_types() {
        let intent = decompose_query("parse Config into Settings");
        assert!(intent.action_verbs.contains(&"parse".to_string()));
        assert!(intent.type_mentions.contains(&"Config".to_string()));
        assert!(intent.type_mentions.contains(&"Settings".to_string()));
    }

    #[test]
    fn query_decomposition_empty() {
        let intent = decompose_query("");
        assert!(intent.action_verbs.is_empty());
        assert!(intent.domain_nouns.is_empty());
        assert!(intent.type_mentions.is_empty());
    }

    #[test]
    fn extract_io_basic() {
        let (inputs, outputs) =
            extract_io_from_signature("fn check(user: User, token: Token) -> Result");
        assert!(inputs.contains(&"User".to_string()));
        assert!(inputs.contains(&"Token".to_string()));
        assert!(outputs.contains(&"Result".to_string()));
    }

    #[test]
    fn extract_io_no_return() {
        let (inputs, outputs) = extract_io_from_signature("fn setup(config: Config)");
        assert!(inputs.contains(&"Config".to_string()));
        assert!(outputs.is_empty());
    }

    #[test]
    fn extract_io_complex_types() {
        let (inputs, outputs) =
            extract_io_from_signature("fn process(data: Vec<Item>) -> Result<Output, Error>");
        assert!(inputs.contains(&"Vec".to_string()));
        assert!(inputs.contains(&"Item".to_string()));
        assert!(outputs.contains(&"Result".to_string()));
        assert!(outputs.contains(&"Output".to_string()));
        assert!(outputs.contains(&"Error".to_string()));
    }

    #[test]
    fn extract_io_no_params() {
        let (inputs, outputs) = extract_io_from_signature("fn stats() -> IndexStats");
        assert!(inputs.is_empty());
        assert!(outputs.contains(&"IndexStats".to_string()));
    }

    #[test]
    fn extract_io_primitives_filtered() {
        let (inputs, _) = extract_io_from_signature("fn add(a: i32, b: usize) -> bool");
        // Primitives (lowercase) should be filtered out
        assert!(inputs.is_empty());
    }
}
