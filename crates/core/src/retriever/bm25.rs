use tantivy::schema::Value;

use crate::error::Result;
use crate::index::TantivyIndex;

use super::{Retriever, SearchQuery, SearchResult};

/// BM25-based retriever backed by Tantivy.
///
/// Wraps a [`TantivyIndex`] and extracts stored fields from search hits
/// to build rich [`SearchResult`] objects.
pub struct BM25Retriever<'a> {
    index: &'a TantivyIndex,
}

impl<'a> BM25Retriever<'a> {
    /// Create a new BM25 retriever wrapping the given Tantivy index.
    pub fn new(index: &'a TantivyIndex) -> Self {
        Self { index }
    }
}

impl Retriever for BM25Retriever<'_> {
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>> {
        let docs = self.index.search_documents(&query.query, query.limit)?;
        let fields = self.index.fields();

        let mut results = Vec::with_capacity(docs.len());
        for (score, doc) in docs {
            let chunk_id = doc
                .get_first(fields.chunk_id)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let file_path = doc
                .get_first(fields.file_path)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let language = doc
                .get_first(fields.language)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = doc
                .get_first(fields.content)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let signature = doc
                .get_first(fields.signature)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let scope_chain_str = doc
                .get_first(fields.scope_chain)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let line_start = doc
                .get_first(fields.line_start)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let line_end = doc
                .get_first(fields.line_end)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            // Reconstruct scope_chain Vec from the space-joined stored string.
            let scope_chain: Vec<String> = scope_chain_str
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();

            results.push(SearchResult {
                chunk_id,
                file_path,
                language,
                score,
                line_start,
                line_end,
                signature,
                scope_chain,
                content,
            });
        }

        // Deduplicate by chunk_id — keep the first (highest-scoring) occurrence.
        // Tantivy returns results sorted by score, so first-seen is best.
        {
            let mut seen = std::collections::HashSet::new();
            results.retain(|r| seen.insert(r.chunk_id.clone()));
        }

        // Apply file filter if present.
        if let Some(ref filter) = query.file_filter {
            results.retain(|r| r.file_path.contains(filter));
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::Chunk;
    use crate::language::Language;

    fn make_chunk(id: u64, file_path: &str, content: &str) -> Chunk {
        Chunk {
            id,
            file_path: file_path.to_string(),
            language: Language::Rust,
            content: content.to_string(),
            byte_start: 0,
            byte_end: content.len(),
            line_start: 0,
            line_end: 5,
            scope_chain: vec!["module".to_string()],
            signatures: vec!["fn example() -> bool".to_string()],
            entity_names: vec!["example".to_string()],
        }
    }

    #[test]
    fn bm25_search_returns_results() {
        let idx = TantivyIndex::create_in_ram().unwrap();
        let chunk = make_chunk(
            42,
            "src/main.rs",
            "fn hello_world() { println!(\"hello\"); }",
        );
        idx.add_chunk(&chunk).unwrap();
        idx.commit().unwrap();

        let retriever = BM25Retriever::new(&idx);
        let query = SearchQuery::new("hello_world").with_limit(5);
        let results = retriever.search(&query).unwrap();

        assert!(!results.is_empty());
        assert_eq!(results[0].chunk_id, "42");
        assert_eq!(results[0].file_path, "src/main.rs");
        assert_eq!(results[0].language, "Rust");
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn bm25_search_with_file_filter() {
        let idx = TantivyIndex::create_in_ram().unwrap();
        idx.add_chunk(&make_chunk(1, "src/main.rs", "fn alpha() {}"))
            .unwrap();
        idx.add_chunk(&make_chunk(2, "src/lib.rs", "fn alpha() {}"))
            .unwrap();
        idx.commit().unwrap();

        let retriever = BM25Retriever::new(&idx);

        // Without filter: should find both.
        let all = retriever
            .search(&SearchQuery::new("alpha").with_limit(10))
            .unwrap();
        assert_eq!(all.len(), 2);

        // With filter: only lib.rs.
        let filtered = retriever
            .search(
                &SearchQuery::new("alpha")
                    .with_limit(10)
                    .with_file_filter("lib.rs"),
            )
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].file_path.contains("lib.rs"));
    }

    #[test]
    fn bm25_search_populates_scope_chain() {
        let idx = TantivyIndex::create_in_ram().unwrap();
        let mut chunk = make_chunk(99, "src/lib.rs", "fn scoped() {}");
        chunk.scope_chain = vec!["MyModule".to_string(), "MyClass".to_string()];
        idx.add_chunk(&chunk).unwrap();
        idx.commit().unwrap();

        let retriever = BM25Retriever::new(&idx);
        let results = retriever
            .search(&SearchQuery::new("scoped").with_limit(5))
            .unwrap();

        assert!(!results.is_empty());
        assert_eq!(results[0].scope_chain, vec!["MyModule", "MyClass"]);
    }

    #[test]
    fn bm25_deduplicates_by_chunk_id() {
        let idx = TantivyIndex::create_in_ram().unwrap();
        // Add the same chunk twice (simulates re-indexing without full cleanup).
        let chunk = make_chunk(
            42,
            "src/main.rs",
            "fn hello_world() { println!(\"hello\"); }",
        );
        idx.add_chunk(&chunk).unwrap();
        idx.add_chunk(&chunk).unwrap();
        idx.commit().unwrap();

        let retriever = BM25Retriever::new(&idx);
        let results = retriever
            .search(&SearchQuery::new("hello_world").with_limit(10))
            .unwrap();

        // Should return only 1 result despite 2 matching documents.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, "42");
    }
}
