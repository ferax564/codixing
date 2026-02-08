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
            let line_start = doc
                .get_first(fields.line_start)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let line_end = doc
                .get_first(fields.line_end)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            results.push(SearchResult {
                chunk_id,
                file_path,
                language,
                score,
                line_start,
                line_end,
                signature,
                content,
            });
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
}
