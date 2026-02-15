use std::path::Path;
use std::sync::Mutex;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Value};
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Term, doc};

use crate::chunker::Chunk;
use crate::error::{CodeforgeError, Result};
use crate::index::schema::{SchemaFields, build_schema};

// ---------------------------------------------------------------------------
// CodeTokenizer — splits code identifiers into searchable sub-tokens
// ---------------------------------------------------------------------------

/// A custom tokenizer that understands code identifiers.
///
/// It splits `camelCase`, `snake_case`, `PascalCase`, `SCREAMING_CASE`,
/// `dot.path.names`, and mixed forms into their constituent parts while also
/// emitting the original (lowercased) token.
///
/// # Examples
///
/// | Input         | Tokens                            |
/// |---------------|-----------------------------------|
/// | `camelCase`   | `camel`, `case`, `camelcase`      |
/// | `snake_case`  | `snake`, `case`, `snake_case`     |
/// | `HTTPServer`  | `http`, `server`, `httpserver`    |
/// | `dot.path.x`  | `dot`, `path`, `x`, `dot.path.x` |
#[derive(Clone)]
pub struct CodeTokenizer;

/// Token stream produced by [`CodeTokenizer`].
pub struct CodeTokenStream {
    tokens: Vec<Token>,
    index: usize,
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = CodeTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let tokens = tokenize_code(text);
        CodeTokenStream {
            tokens,
            index: usize::MAX, // will wrap to 0 on first advance()
        }
    }
}

impl TokenStream for CodeTokenStream {
    fn advance(&mut self) -> bool {
        if self.index == usize::MAX {
            self.index = 0;
        } else {
            self.index += 1;
        }
        self.index < self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.index]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.index]
    }
}

/// Split a single word on camelCase / PascalCase / SCREAMING boundaries.
///
/// For `"HTTPServer"` this yields `["HTTP", "Server"]`.
/// For `"camelCase"` this yields `["camel", "Case"]`.
fn split_camel(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    if chars.len() <= 1 {
        return vec![word.to_string()];
    }

    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    current.push(chars[0]);

    for i in 1..chars.len() {
        let prev = chars[i - 1];
        let cur = chars[i];
        let next = chars.get(i + 1);

        // Transition: lowercase -> uppercase (camelCase boundary)
        if prev.is_lowercase() && cur.is_uppercase() {
            parts.push(std::mem::take(&mut current));
            current.push(cur);
            continue;
        }

        // Transition: uppercase -> uppercase -> lowercase (HTTPServer: split before 'S')
        if prev.is_uppercase() && cur.is_uppercase() {
            if let Some(&n) = next {
                if n.is_lowercase() {
                    parts.push(std::mem::take(&mut current));
                    current.push(cur);
                    continue;
                }
            }
        }

        current.push(cur);
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

/// Core tokenization: takes full input text, produces all tokens.
fn tokenize_code(text: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut position: usize = 0;

    // First, split the text on whitespace to get top-level words with offsets.
    let words = split_on_boundaries(text);

    for (word, offset_from, offset_to) in words {
        if word.is_empty() {
            continue;
        }

        let lower_word = word.to_lowercase();
        let mut sub_tokens: Vec<String> = Vec::new();

        // Check for dot-separated identifiers (e.g., `path.to.module`).
        if word.contains('.') {
            let dot_parts: Vec<&str> = word.split('.').filter(|p| !p.is_empty()).collect();
            if dot_parts.len() > 1 {
                for part in &dot_parts {
                    // Each dot-part may itself be camelCase.
                    for camel_part in split_camel(part) {
                        let lp = camel_part.to_lowercase();
                        if !lp.is_empty() && !sub_tokens.contains(&lp) {
                            sub_tokens.push(lp);
                        }
                    }
                }
                // Also add the full dot-path (lowercased).
                if !sub_tokens.contains(&lower_word) {
                    sub_tokens.push(lower_word.clone());
                }
            } else {
                // Single segment with leading/trailing dots — treat as a plain word.
                for camel_part in split_camel(&word) {
                    let lp = camel_part.to_lowercase();
                    if !lp.is_empty() && !sub_tokens.contains(&lp) {
                        sub_tokens.push(lp);
                    }
                }
                if sub_tokens.len() > 1 && !sub_tokens.contains(&lower_word) {
                    sub_tokens.push(lower_word.clone());
                }
            }
        } else if word.contains('_') {
            // snake_case / SCREAMING_CASE
            let parts: Vec<&str> = word.split('_').filter(|p| !p.is_empty()).collect();
            if parts.len() > 1 {
                for part in &parts {
                    for camel_part in split_camel(part) {
                        let lp = camel_part.to_lowercase();
                        if !lp.is_empty() && !sub_tokens.contains(&lp) {
                            sub_tokens.push(lp);
                        }
                    }
                }
                // Also add the full underscore-joined form.
                if !sub_tokens.contains(&lower_word) {
                    sub_tokens.push(lower_word.clone());
                }
            } else {
                // Single segment with leading/trailing underscores.
                for camel_part in split_camel(&word) {
                    let lp = camel_part.to_lowercase();
                    if !lp.is_empty() && !sub_tokens.contains(&lp) {
                        sub_tokens.push(lp);
                    }
                }
                if sub_tokens.len() > 1 && !sub_tokens.contains(&lower_word) {
                    sub_tokens.push(lower_word.clone());
                }
            }
        } else {
            // Plain word — try camelCase splitting.
            let camel_parts = split_camel(&word);
            if camel_parts.len() > 1 {
                for part in &camel_parts {
                    let lp = part.to_lowercase();
                    if !lp.is_empty() && !sub_tokens.contains(&lp) {
                        sub_tokens.push(lp);
                    }
                }
                // Add the joined form without separators.
                if !sub_tokens.contains(&lower_word) {
                    sub_tokens.push(lower_word.clone());
                }
            } else {
                sub_tokens.push(lower_word.clone());
            }
        }

        // Emit tokens with proper positions.
        for tok_text in sub_tokens {
            tokens.push(Token {
                offset_from,
                offset_to,
                position,
                text: tok_text,
                position_length: 1,
            });
        }
        position += 1;
    }

    tokens
}

/// Split text into words, splitting on whitespace and non-alphanumeric chars
/// (except `_` and `.` which are handled later). Returns `(word, byte_start, byte_end)`.
fn split_on_boundaries(text: &str) -> Vec<(String, usize, usize)> {
    let mut words: Vec<(String, usize, usize)> = Vec::new();
    let mut current = String::new();
    let mut start: usize = 0;

    for (byte_idx, ch) in text.char_indices() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' {
            if current.is_empty() {
                start = byte_idx;
            }
            current.push(ch);
        } else {
            // Separator character — flush current word.
            if !current.is_empty() {
                let end = byte_idx;
                words.push((std::mem::take(&mut current), start, end));
            }
        }
    }

    // Flush trailing word.
    if !current.is_empty() {
        words.push((current, start, text.len()));
    }

    words
}

// ---------------------------------------------------------------------------
// TantivyIndex — thin wrapper around a tantivy::Index
// ---------------------------------------------------------------------------

/// BM25 full-text search index backed by Tantivy.
///
/// Thread-safe: the writer is behind a [`Mutex`] so concurrent calls to
/// `add_chunk` / `remove_file` / `commit` are serialized.
pub struct TantivyIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: SchemaFields,
}

/// Register the custom `"code"` tokenizer on a [`tantivy::Index`].
fn register_code_tokenizer(index: &Index) {
    let analyzer = tantivy::tokenizer::TextAnalyzer::builder(CodeTokenizer).build();
    index.tokenizers().register("code", analyzer);
}

impl TantivyIndex {
    /// Create a new **in-memory** index (useful for tests).
    pub fn create_in_ram() -> Result<Self> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        register_code_tokenizer(&index);
        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
        })
    }

    /// Create or open a persistent index at the given directory.
    ///
    /// If the directory does not exist it will be created.
    pub fn create_in_dir(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let (schema, fields) = build_schema();
        let dir = tantivy::directory::MmapDirectory::open(path)
            .map_err(|e| CodeforgeError::Index(e.to_string()))?;
        let index = Index::open_or_create(dir, schema)?;
        register_code_tokenizer(&index);
        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
        })
    }

    /// Open an existing persistent index (fails if no index exists).
    pub fn open_in_dir(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(CodeforgeError::IndexNotFound {
                path: path.to_path_buf(),
            });
        }
        let index = Index::open_in_dir(path)?;
        register_code_tokenizer(&index);

        // Reconstruct field handles from the existing schema.
        let schema = index.schema();
        let field = |name: &str| -> Result<Field> {
            schema
                .get_field(name)
                .map_err(|e| CodeforgeError::Index(e.to_string()))
        };
        let fields = SchemaFields {
            chunk_id: field("chunk_id")?,
            file_path: field("file_path")?,
            file_path_exact: field("file_path_exact")?,
            language: field("language")?,
            content: field("content")?,
            scope_chain: field("scope_chain")?,
            signature: field("signature")?,
            entity_names: field("entity_names")?,
            line_start: field("line_start")?,
            line_end: field("line_end")?,
            byte_start: field("byte_start")?,
            byte_end: field("byte_end")?,
        };

        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
        })
    }

    /// Add a [`Chunk`] to the index.
    ///
    /// The document is staged but not committed — call [`Self::commit`] to
    /// make it visible to readers.
    pub fn add_chunk(&self, chunk: &Chunk) -> Result<()> {
        let scope_chain_text = chunk.scope_chain.join(" ");
        let signatures_text = chunk.signatures.join("\n");
        let entity_names_text = chunk.entity_names.join(" ");
        let chunk_id_str = chunk.id.to_string();

        let writer = self
            .writer
            .lock()
            .map_err(|e| CodeforgeError::Index(format!("writer lock poisoned: {e}")))?;

        writer.add_document(doc!(
            self.fields.chunk_id => chunk_id_str,
            self.fields.file_path => chunk.file_path.as_str(),
            self.fields.file_path_exact => chunk.file_path.as_str(),
            self.fields.language => chunk.language.name(),
            self.fields.content => chunk.content.as_str(),
            self.fields.scope_chain => scope_chain_text,
            self.fields.signature => signatures_text,
            self.fields.entity_names => entity_names_text,
            self.fields.line_start => chunk.line_start as u64,
            self.fields.line_end => chunk.line_end as u64,
            self.fields.byte_start => chunk.byte_start as u64,
            self.fields.byte_end => chunk.byte_end as u64,
        ))?;

        Ok(())
    }

    /// Remove all documents belonging to a file path from the index.
    ///
    /// Like `add_chunk`, the delete is staged until [`Self::commit`].
    pub fn remove_file(&self, file_path: &str) -> Result<()> {
        let writer = self
            .writer
            .lock()
            .map_err(|e| CodeforgeError::Index(format!("writer lock poisoned: {e}")))?;
        writer.delete_term(Term::from_field_text(
            self.fields.file_path_exact,
            file_path,
        ));
        Ok(())
    }

    /// Commit all staged additions and deletions, making them visible to readers.
    pub fn commit(&self) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|e| CodeforgeError::Index(format!("writer lock poisoned: {e}")))?;
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Search the index using BM25 ranking.
    ///
    /// Returns up to `limit` results as `(chunk_id, score)` pairs sorted by
    /// descending relevance.
    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<(String, f32)>> {
        let searcher = self.reader.searcher();

        let query_parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.content,
                self.fields.entity_names,
                self.fields.signature,
                self.fields.scope_chain,
            ],
        );

        let query = query_parser.parse_query(query_str)?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let retrieved_doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            if let Some(chunk_id_value) = retrieved_doc.get_first(self.fields.chunk_id) {
                if let Some(chunk_id_str) = chunk_id_value.as_str() {
                    results.push((chunk_id_str.to_string(), score));
                }
            }
        }

        Ok(results)
    }

    /// Search the index and return full document data for each hit.
    ///
    /// Unlike [`Self::search`], this extracts all stored fields from each
    /// matching document, allowing callers to build rich result objects.
    pub fn search_documents(
        &self,
        query_str: &str,
        limit: usize,
    ) -> Result<Vec<(f32, tantivy::TantivyDocument)>> {
        let searcher = self.reader.searcher();

        let query_parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.content,
                self.fields.entity_names,
                self.fields.signature,
                self.fields.scope_chain,
            ],
        );

        let query = query_parser.parse_query(query_str)?;
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut results = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(doc_address)?;
            results.push((score, doc));
        }

        Ok(results)
    }

    /// Get a reference to the schema fields.
    pub fn fields(&self) -> &SchemaFields {
        &self.fields
    }

    /// Read all chunk IDs and their content from the index.
    ///
    /// Used during init to batch-embed chunks into a vector index.
    pub fn all_chunk_ids_and_content(&self) -> Result<Vec<(u64, String)>> {
        let searcher = self.reader.searcher();
        let mut results = Vec::new();

        for segment_reader in searcher.segment_readers() {
            let store_reader = segment_reader.get_store_reader(1)?;
            for doc_id in 0..segment_reader.max_doc() {
                if segment_reader.is_deleted(doc_id) {
                    continue;
                }
                let doc: tantivy::TantivyDocument = store_reader.get(doc_id)?;
                let chunk_id = doc
                    .get_first(self.fields.chunk_id)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let content = doc
                    .get_first(self.fields.content)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                results.push((chunk_id, content));
            }
        }

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::Language;

    /// Helper: collect all token texts from the CodeTokenizer.
    fn tokenize(text: &str) -> Vec<String> {
        let mut tokenizer = CodeTokenizer;
        let mut stream = tokenizer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    /// Helper: build a test chunk.
    fn make_chunk(id: u64, file_path: &str, content: &str) -> Chunk {
        Chunk {
            id,
            file_path: file_path.to_string(),
            language: Language::Rust,
            content: content.to_string(),
            byte_start: 0,
            byte_end: content.len(),
            line_start: 0,
            line_end: 10,
            scope_chain: vec!["module".to_string()],
            signatures: vec!["fn example() -> bool".to_string()],
            entity_names: vec!["example".to_string()],
        }
    }

    // --- Tokenizer tests ---

    #[test]
    fn tokenizer_camel_case() {
        let tokens = tokenize("camelCase");
        assert!(tokens.contains(&"camel".to_string()));
        assert!(tokens.contains(&"case".to_string()));
        assert!(tokens.contains(&"camelcase".to_string()));
    }

    #[test]
    fn tokenizer_snake_case() {
        let tokens = tokenize("snake_case");
        assert!(tokens.contains(&"snake".to_string()));
        assert!(tokens.contains(&"case".to_string()));
        assert!(tokens.contains(&"snake_case".to_string()));
    }

    #[test]
    fn tokenizer_dot_path() {
        let tokens = tokenize("dot.path.name");
        assert!(tokens.contains(&"dot".to_string()));
        assert!(tokens.contains(&"path".to_string()));
        assert!(tokens.contains(&"name".to_string()));
        assert!(tokens.contains(&"dot.path.name".to_string()));
    }

    #[test]
    fn tokenizer_screaming_acronym() {
        let tokens = tokenize("HTTPServer");
        assert!(tokens.contains(&"http".to_string()));
        assert!(tokens.contains(&"server".to_string()));
        assert!(tokens.contains(&"httpserver".to_string()));
    }

    #[test]
    fn tokenizer_all_lowercase() {
        let tokens = tokenize("CamelCase UPPER");
        for tok in &tokens {
            assert_eq!(tok, &tok.to_lowercase(), "token not lowercased: {tok}");
        }
    }

    #[test]
    fn tokenizer_non_alnum_split() {
        let tokens = tokenize("foo::bar->baz");
        assert!(tokens.contains(&"foo".to_string()));
        assert!(tokens.contains(&"bar".to_string()));
        assert!(tokens.contains(&"baz".to_string()));
    }

    // --- Index tests ---

    #[test]
    fn add_chunk_and_search() {
        let idx = TantivyIndex::create_in_ram().unwrap();
        let chunk = make_chunk(
            42,
            "src/main.rs",
            "fn hello_world() { println!(\"hello\"); }",
        );
        idx.add_chunk(&chunk).unwrap();
        idx.commit().unwrap();

        let results = idx.search("hello_world", 10).unwrap();
        assert!(!results.is_empty(), "expected at least one search result");
        assert_eq!(results[0].0, "42");
    }

    #[test]
    fn remove_file_clears_results() {
        let idx = TantivyIndex::create_in_ram().unwrap();
        let chunk = make_chunk(99, "src/lib.rs", "struct UniqueWidget { field: i32 }");
        idx.add_chunk(&chunk).unwrap();
        idx.commit().unwrap();

        // Sanity: should find it first.
        let results = idx.search("UniqueWidget", 10).unwrap();
        assert!(!results.is_empty());

        // Remove and re-search.
        idx.remove_file("src/lib.rs").unwrap();
        idx.commit().unwrap();

        let results = idx.search("UniqueWidget", 10).unwrap();
        assert!(results.is_empty(), "expected no results after remove_file");
    }

    #[test]
    fn persist_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tantivy_test_idx");

        // Create and populate.
        {
            let idx = TantivyIndex::create_in_dir(&path).unwrap();
            let chunk = make_chunk(
                7,
                "src/persist.rs",
                "fn persist_test_unique_identifier() {}",
            );
            idx.add_chunk(&chunk).unwrap();
            idx.commit().unwrap();
        }

        // Reopen and search.
        {
            let idx = TantivyIndex::open_in_dir(&path).unwrap();
            let results = idx.search("persist_test_unique_identifier", 10).unwrap();
            assert!(!results.is_empty(), "expected results after reopen");
            assert_eq!(results[0].0, "7");
        }
    }
}
