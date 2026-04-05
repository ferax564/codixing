//! Query routing — classifies queries as code-oriented, doc-oriented, or hybrid,
//! and adjusts search result scores accordingly.

/// The detected intent of a search query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryIntent {
    /// Query looks code-oriented: symbol names, path separators, function syntax.
    Code,
    /// Query looks doc-oriented: natural language questions, concept terms.
    Docs,
    /// Ambiguous or mixed: don't adjust scores.
    Hybrid,
}

/// Classify a query string to determine whether it targets code or documentation.
pub fn classify_query(query: &str) -> QueryIntent {
    let has_code_signals = has_camel_case(query)
        || has_snake_case(query)
        || query.contains("::")
        || query.contains("()")
        || query.contains("->")
        || query.contains("fn ")
        || query.contains("def ")
        || query.contains("func ")
        || query.contains("class ")
        || query.contains("struct ");

    let q = query.to_lowercase();
    let has_doc_signals = q.starts_with("how ")
        || q.starts_with("what ")
        || q.starts_with("why ")
        || q.starts_with("when ")
        || q.contains("how to")
        || q.contains("what is")
        || q.contains("example of")
        || q.contains("guide")
        || q.contains("tutorial")
        || q.contains("documentation");

    match (has_code_signals, has_doc_signals) {
        (true, false) => QueryIntent::Code,
        (false, true) => QueryIntent::Docs,
        _ => QueryIntent::Hybrid,
    }
}

/// Apply doc_type-based score adjustment to a search result.
pub fn apply_routing_boost(score: f32, doc_type: &str, intent: QueryIntent) -> f32 {
    match intent {
        QueryIntent::Hybrid => score,
        QueryIntent::Code => {
            if doc_type == "doc" {
                score * 0.3
            } else {
                score
            }
        }
        QueryIntent::Docs => {
            if doc_type == "doc" {
                score
            } else {
                score * 0.3
            }
        }
    }
}

fn has_camel_case(s: &str) -> bool {
    let mut prev_lower = false;
    for c in s.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            return true;
        }
        prev_lower = c.is_ascii_lowercase();
    }
    false
}

fn has_snake_case(s: &str) -> bool {
    s.contains('_')
        && s.split('_').filter(|p| !p.is_empty()).count() >= 2
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_code_queries() {
        assert_eq!(classify_query("ChunkConfig"), QueryIntent::Code);
        assert_eq!(classify_query("add_chunk"), QueryIntent::Code);
        assert_eq!(classify_query("Engine::init"), QueryIntent::Code);
        assert_eq!(classify_query("foo()"), QueryIntent::Code);
        assert_eq!(classify_query("fn main"), QueryIntent::Code);
    }

    #[test]
    fn classify_doc_queries() {
        assert_eq!(classify_query("how to install"), QueryIntent::Docs);
        assert_eq!(classify_query("what is rate limiting"), QueryIntent::Docs);
        assert_eq!(classify_query("why does the graph work"), QueryIntent::Docs);
        assert_eq!(classify_query("getting started guide"), QueryIntent::Docs);
    }

    #[test]
    fn classify_hybrid_queries() {
        assert_eq!(classify_query("rate limiting"), QueryIntent::Hybrid);
        assert_eq!(classify_query("error handling"), QueryIntent::Hybrid);
        assert_eq!(classify_query("search"), QueryIntent::Hybrid);
    }

    #[test]
    fn classify_mixed_signals_as_hybrid() {
        assert_eq!(
            classify_query("how to use ChunkConfig"),
            QueryIntent::Hybrid
        );
    }

    #[test]
    fn routing_boost_code_intent() {
        assert_eq!(apply_routing_boost(1.0, "code", QueryIntent::Code), 1.0);
        assert_eq!(apply_routing_boost(1.0, "doc", QueryIntent::Code), 0.3);
        assert_eq!(apply_routing_boost(1.0, "config", QueryIntent::Code), 1.0);
    }

    #[test]
    fn routing_boost_docs_intent() {
        assert_eq!(apply_routing_boost(1.0, "doc", QueryIntent::Docs), 1.0);
        assert_eq!(apply_routing_boost(1.0, "code", QueryIntent::Docs), 0.3);
    }

    #[test]
    fn routing_boost_hybrid_no_change() {
        assert_eq!(apply_routing_boost(1.0, "code", QueryIntent::Hybrid), 1.0);
        assert_eq!(apply_routing_boost(1.0, "doc", QueryIntent::Hybrid), 1.0);
    }
}
