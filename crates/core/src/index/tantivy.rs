use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Value};
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Term, doc};

use crate::chunker::Chunk;
use crate::config::Bm25Config;
use crate::error::{CodixingError, Result};
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
        // Use a HashSet for O(1) dedup checks instead of Vec::contains() which is O(M).
        let mut seen: HashSet<String> = HashSet::new();
        let mut sub_tokens: Vec<String> = Vec::new();

        /// Insert a token into `sub_tokens` if not already seen. O(1) lookup.
        macro_rules! insert_unique {
            ($seen:expr, $sub_tokens:expr, $val:expr) => {
                if !$val.is_empty() && $seen.insert($val.clone()) {
                    $sub_tokens.push($val);
                }
            };
        }

        // Check for dot-separated identifiers (e.g., `path.to.module`).
        if word.contains('.') {
            let dot_parts: Vec<&str> = word.split('.').filter(|p| !p.is_empty()).collect();
            if dot_parts.len() > 1 {
                for part in &dot_parts {
                    // Each dot-part may itself be camelCase.
                    for camel_part in split_camel(part) {
                        let lp = camel_part.to_lowercase();
                        insert_unique!(seen, sub_tokens, lp);
                    }
                }
                // Also add the full dot-path (lowercased).
                insert_unique!(seen, sub_tokens, lower_word.clone());
            } else {
                // Single segment with leading/trailing dots — treat as a plain word.
                for camel_part in split_camel(&word) {
                    let lp = camel_part.to_lowercase();
                    insert_unique!(seen, sub_tokens, lp);
                }
                if sub_tokens.len() > 1 {
                    insert_unique!(seen, sub_tokens, lower_word.clone());
                }
            }
        } else if word.contains('_') {
            // snake_case / SCREAMING_CASE
            let parts: Vec<&str> = word.split('_').filter(|p| !p.is_empty()).collect();
            if parts.len() > 1 {
                for part in &parts {
                    for camel_part in split_camel(part) {
                        let lp = camel_part.to_lowercase();
                        insert_unique!(seen, sub_tokens, lp);
                    }
                }
                // Also add the full underscore-joined form.
                insert_unique!(seen, sub_tokens, lower_word.clone());
            } else {
                // Single segment with leading/trailing underscores.
                for camel_part in split_camel(&word) {
                    let lp = camel_part.to_lowercase();
                    insert_unique!(seen, sub_tokens, lp);
                }
                if sub_tokens.len() > 1 {
                    insert_unique!(seen, sub_tokens, lower_word.clone());
                }
            }
        } else {
            // Plain word — try camelCase splitting.
            let camel_parts = split_camel(&word);
            if camel_parts.len() > 1 {
                for part in &camel_parts {
                    let lp = part.to_lowercase();
                    insert_unique!(seen, sub_tokens, lp);
                }
                // Add the joined form without separators.
                insert_unique!(seen, sub_tokens, lower_word.clone());
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
///
/// When opened in read-only mode (`writer` is `None`), all read operations
/// work normally but write operations return [`CodixingError::ReadOnly`].
/// This allows concurrent instances to share the index directory.
pub struct TantivyIndex {
    index: Index,
    reader: IndexReader,
    writer: Option<Mutex<IndexWriter>>,
    fields: SchemaFields,
    bm25_config: Bm25Config,
}

/// Register the custom `"code"` tokenizer on a [`tantivy::Index`].
fn register_code_tokenizer(index: &Index) {
    let analyzer = tantivy::tokenizer::TextAnalyzer::builder(CodeTokenizer).build();
    index.tokenizers().register("code", analyzer);
}

/// Register the stemmed `"code_stemmed"` tokenizer on a [`tantivy::Index`].
///
/// Like `"code"` but with an English stemmer filter appended.
/// Used for NL-oriented fields (doc_comment, identifier_words, path_segments).
fn register_code_stemmed_tokenizer(index: &Index) {
    let analyzer = tantivy::tokenizer::TextAnalyzer::builder(CodeTokenizer)
        .filter(tantivy::tokenizer::Stemmer::new(
            tantivy::tokenizer::Language::English,
        ))
        .build();
    index.tokenizers().register("code_stemmed", analyzer);
}

/// Split a code identifier into constituent words for the `identifier_words` field.
///
/// E.g., `"createRateLimiter"` → `["create", "rate", "limiter", "createratelimiter"]`
fn split_identifier_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    for part in name.split(['_', '.']).filter(|p| !p.is_empty()) {
        for sub in split_camel(part) {
            let lower = sub.to_lowercase();
            if !lower.is_empty() {
                words.push(lower);
            }
        }
    }
    words.push(name.to_lowercase());
    words
}

/// Generate path segment tokens from a file path.
///
/// E.g., `"src/cron/fixed-window-rate-limit.ts"` → `"src cron fixed window rate limit"`
fn generate_path_segments(file_path: &str) -> String {
    file_path
        .split('/')
        .flat_map(|segment| {
            let name = segment.rsplit_once('.').map_or(segment, |(name, _)| name);
            name.split(['-', '_'])
                .filter(|p| !p.is_empty())
                .map(|p| p.to_lowercase())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl TantivyIndex {
    /// Create a new **in-memory** index (useful for tests).
    pub fn create_in_ram() -> Result<Self> {
        Self::create_in_ram_with_config(Bm25Config::default())
    }

    /// Create a new **in-memory** index with custom BM25 field boost weights.
    pub fn create_in_ram_with_config(bm25_config: Bm25Config) -> Result<Self> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        register_code_tokenizer(&index);
        register_code_stemmed_tokenizer(&index);
        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: Some(Mutex::new(writer)),
            fields,
            bm25_config,
        })
    }

    /// Create or open a persistent index at the given directory.
    ///
    /// If the directory does not exist it will be created.
    pub fn create_in_dir(path: &Path) -> Result<Self> {
        Self::create_in_dir_with_config(path, Bm25Config::default())
    }

    /// Create or open a persistent index with custom BM25 field boost weights.
    pub fn create_in_dir_with_config(path: &Path, bm25_config: Bm25Config) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let (schema, fields) = build_schema();
        let dir = tantivy::directory::MmapDirectory::open(path)
            .map_err(|e| CodixingError::Index(e.to_string()))?;
        let index = Index::open_or_create(dir, schema)?;
        register_code_tokenizer(&index);
        register_code_stemmed_tokenizer(&index);
        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: Some(Mutex::new(writer)),
            fields,
            bm25_config,
        })
    }

    /// Open an existing persistent index (fails if no index exists).
    pub fn open_in_dir(path: &Path) -> Result<Self> {
        Self::open_in_dir_with_config(path, Bm25Config::default())
    }

    /// Open an existing persistent index with custom BM25 field boost weights.
    pub fn open_in_dir_with_config(path: &Path, bm25_config: Bm25Config) -> Result<Self> {
        if !path.exists() {
            return Err(CodixingError::IndexNotFound {
                path: path.to_path_buf(),
            });
        }
        let index = Index::open_in_dir(path)?;
        register_code_tokenizer(&index);
        register_code_stemmed_tokenizer(&index);

        // Reconstruct field handles from the existing schema.
        let schema = index.schema();
        let field = |name: &str| -> Result<Field> {
            schema
                .get_field(name)
                .map_err(|e| CodixingError::Index(e.to_string()))
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
            doc_comment: field("doc_comment")?,
            identifier_words: field("identifier_words")?,
            path_segments: field("path_segments")?,
            // Graceful fallback for indexes created before this field was added.
            doc_type: schema.get_field("doc_type").ok(),
        };

        let writer = index.writer(50_000_000)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: Some(Mutex::new(writer)),
            fields,
            bm25_config,
        })
    }

    /// Open an existing persistent index in **read-only** mode.
    ///
    /// No [`IndexWriter`] is acquired, so this will not conflict with another
    /// process that already holds the write lock on the same directory.
    /// Search (`search`, `search_with_filter`, `search_by_file`, `lookup_chunk`,
    /// etc.) work normally; write operations (`add_chunk`, `remove_file`,
    /// `commit`) return [`CodixingError::ReadOnly`].
    pub fn open_read_only(path: &Path) -> Result<Self> {
        Self::open_read_only_with_config(path, Bm25Config::default())
    }

    /// Open an existing persistent index in read-only mode with custom BM25 field boost weights.
    pub fn open_read_only_with_config(path: &Path, bm25_config: Bm25Config) -> Result<Self> {
        if !path.exists() {
            return Err(CodixingError::IndexNotFound {
                path: path.to_path_buf(),
            });
        }
        let index = Index::open_in_dir(path)?;
        register_code_tokenizer(&index);
        register_code_stemmed_tokenizer(&index);

        // Reconstruct field handles from the existing schema.
        let schema = index.schema();
        let field = |name: &str| -> Result<Field> {
            schema
                .get_field(name)
                .map_err(|e| CodixingError::Index(e.to_string()))
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
            doc_comment: field("doc_comment")?,
            identifier_words: field("identifier_words")?,
            path_segments: field("path_segments")?,
            // Graceful fallback for indexes created before this field was added.
            doc_type: schema.get_field("doc_type").ok(),
        };

        // No writer — read-only mode.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer: None,
            fields,
            bm25_config,
        })
    }

    /// Return `true` if this index was opened in read-only mode (no writer).
    pub fn is_read_only(&self) -> bool {
        self.writer.is_none()
    }

    /// Add a [`Chunk`] to the index.
    ///
    /// The document is staged but not committed — call [`Self::commit`] to
    /// make it visible to readers.
    pub fn add_chunk(&self, chunk: &Chunk) -> Result<()> {
        let writer_mutex = self.writer.as_ref().ok_or(CodixingError::ReadOnly)?;
        let scope_chain_text = chunk.scope_chain.join(" ");
        let signatures_text = chunk.signatures.join("\n");
        let entity_names_text = chunk.entity_names.join(" ");
        let chunk_id_str = chunk.id.to_string();

        // BM25F synthetic fields for concept retrieval.
        let identifier_words_text = chunk
            .entity_names
            .iter()
            .flat_map(|name| split_identifier_words(name))
            .collect::<Vec<_>>()
            .join(" ");
        let path_segments_text = generate_path_segments(&chunk.file_path);

        let doc_type_str = if chunk.language.is_doc() {
            "doc"
        } else if chunk.language.is_tree_sitter() {
            "code"
        } else {
            "config"
        };

        let writer = writer_mutex
            .lock()
            .map_err(|e| CodixingError::Index(format!("writer lock poisoned: {e}")))?;

        let mut document = doc!(
            self.fields.chunk_id => chunk_id_str,
            self.fields.file_path => chunk.file_path.as_str(),
            self.fields.file_path_exact => chunk.file_path.as_str(),
            self.fields.language => chunk.language.name(),
            self.fields.content => chunk.content.as_str(),
            self.fields.scope_chain => scope_chain_text,
            self.fields.signature => signatures_text,
            self.fields.entity_names => entity_names_text,
            self.fields.doc_comment => chunk.doc_comments.as_str(),
            self.fields.identifier_words => identifier_words_text.as_str(),
            self.fields.path_segments => path_segments_text.as_str(),
            self.fields.line_start => chunk.line_start as u64,
            self.fields.line_end => chunk.line_end as u64,
            self.fields.byte_start => chunk.byte_start as u64,
            self.fields.byte_end => chunk.byte_end as u64,
        );
        // Only write doc_type when the field exists (old indexes may not have it).
        if let Some(doc_type_field) = self.fields.doc_type {
            document.add_field_value(doc_type_field, doc_type_str);
        }
        writer.add_document(document)?;

        Ok(())
    }

    /// Remove all documents belonging to a file path from the index.
    ///
    /// Like `add_chunk`, the delete is staged until [`Self::commit`].
    pub fn remove_file(&self, file_path: &str) -> Result<()> {
        let writer_mutex = self.writer.as_ref().ok_or(CodixingError::ReadOnly)?;
        let writer = writer_mutex
            .lock()
            .map_err(|e| CodixingError::Index(format!("writer lock poisoned: {e}")))?;
        writer.delete_term(Term::from_field_text(
            self.fields.file_path_exact,
            file_path,
        ));
        Ok(())
    }

    /// Commit all staged additions and deletions, making them visible to readers.
    pub fn commit(&self) -> Result<()> {
        let writer_mutex = self.writer.as_ref().ok_or(CodixingError::ReadOnly)?;
        let mut writer = writer_mutex
            .lock()
            .map_err(|e| CodixingError::Index(format!("writer lock poisoned: {e}")))?;
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Reload the reader to pick up changes made by another writer process.
    ///
    /// Useful for read-only instances that want to see new segments committed
    /// by a concurrent writer.
    pub fn refresh_reader(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }

    /// Search the index using BM25 ranking with configurable field boost weights.
    ///
    /// Returns up to `limit` results as `(chunk_id, score)` pairs sorted by
    /// descending relevance. Field weights are controlled by [`Bm25Config`].
    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<(String, f32)>> {
        let searcher = self.reader.searcher();

        let mut query_parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.content,
                self.fields.entity_names,
                self.fields.signature,
                self.fields.scope_chain,
                self.fields.doc_comment,
                self.fields.identifier_words,
                self.fields.path_segments,
            ],
        );
        // Field-weighted BM25: configurable boosts so symbol lookups rank above
        // raw content hits — mirrors what Elasticsearch `multi_match boost` does.
        query_parser.set_field_boost(
            self.fields.entity_names,
            self.bm25_config.entity_names_boost,
        );
        query_parser.set_field_boost(self.fields.signature, self.bm25_config.signature_boost);
        query_parser.set_field_boost(self.fields.scope_chain, self.bm25_config.scope_chain_boost);
        query_parser.set_field_boost(self.fields.content, self.bm25_config.content_boost);
        query_parser.set_field_boost(self.fields.doc_comment, self.bm25_config.doc_comment_boost);
        query_parser.set_field_boost(
            self.fields.identifier_words,
            self.bm25_config.identifier_words_boost,
        );
        query_parser.set_field_boost(
            self.fields.path_segments,
            self.bm25_config.path_segments_boost,
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
    /// Uses the same configurable field boost weights as [`Self::search`].
    pub fn search_documents(
        &self,
        query_str: &str,
        limit: usize,
    ) -> Result<Vec<(f32, tantivy::TantivyDocument)>> {
        let searcher = self.reader.searcher();

        let mut query_parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.content,
                self.fields.entity_names,
                self.fields.signature,
                self.fields.scope_chain,
                self.fields.doc_comment,
                self.fields.identifier_words,
                self.fields.path_segments,
            ],
        );
        query_parser.set_field_boost(
            self.fields.entity_names,
            self.bm25_config.entity_names_boost,
        );
        query_parser.set_field_boost(self.fields.signature, self.bm25_config.signature_boost);
        query_parser.set_field_boost(self.fields.scope_chain, self.bm25_config.scope_chain_boost);
        query_parser.set_field_boost(self.fields.content, self.bm25_config.content_boost);
        query_parser.set_field_boost(self.fields.doc_comment, self.bm25_config.doc_comment_boost);
        query_parser.set_field_boost(
            self.fields.identifier_words,
            self.bm25_config.identifier_words_boost,
        );
        query_parser.set_field_boost(
            self.fields.path_segments,
            self.bm25_config.path_segments_boost,
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

    /// Look up documents by a set of chunk IDs.
    ///
    /// Scans all segments and returns full stored documents for any chunk whose
    /// ID appears in `chunk_ids`. Used by the trigram search path to build
    /// rich `SearchResult` objects from trigram-matched chunk IDs.
    pub fn lookup_chunks_by_ids(
        &self,
        chunk_ids: &std::collections::HashSet<u64>,
    ) -> Result<Vec<tantivy::TantivyDocument>> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }
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
                if chunk_ids.contains(&chunk_id) {
                    results.push(doc);
                }
            }
        }

        Ok(results)
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

    /// Read all (file_path, content) pairs from the index.
    ///
    /// Used to rebuild file-level trigram indexes when chunk_meta content is
    /// empty (compact persistence mode).
    pub fn all_file_path_content_pairs(&self) -> Result<Vec<(String, String)>> {
        let searcher = self.reader.searcher();
        let mut results = Vec::new();

        for segment_reader in searcher.segment_readers() {
            let store_reader = segment_reader.get_store_reader(1)?;
            for doc_id in 0..segment_reader.max_doc() {
                if segment_reader.is_deleted(doc_id) {
                    continue;
                }
                let doc: tantivy::TantivyDocument = store_reader.get(doc_id)?;
                let file_path = doc
                    .get_first(self.fields.file_path)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = doc
                    .get_first(self.fields.content)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !file_path.is_empty() && !content.is_empty() {
                    results.push((file_path, content));
                }
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
            doc_comments: String::new(),
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

    // --- Field-weighted BM25 tests ---

    #[test]
    fn entity_name_match_ranks_above_content_only() {
        let idx = TantivyIndex::create_in_ram().unwrap();

        // Chunk 1: "FooParser" appears only in entity_names (via the chunk).
        let entity_chunk = Chunk {
            id: 1,
            file_path: "src/parser.rs".to_string(),
            language: Language::Rust,
            content: "impl FooParser { fn parse(&self) {} }".to_string(),
            byte_start: 0,
            byte_end: 40,
            line_start: 0,
            line_end: 5,
            scope_chain: vec!["module".to_string()],
            signatures: vec![],
            entity_names: vec!["FooParser".to_string()],
            doc_comments: String::new(),
        };

        // Chunk 2: "FooParser" appears only in content, NOT in entity_names.
        let content_chunk = Chunk {
            id: 2,
            file_path: "src/utils.rs".to_string(),
            language: Language::Rust,
            content: "// This helper is used by FooParser for validation".to_string(),
            byte_start: 0,
            byte_end: 50,
            line_start: 0,
            line_end: 5,
            scope_chain: vec!["module".to_string()],
            signatures: vec![],
            entity_names: vec!["validate".to_string()],
            doc_comments: String::new(),
        };

        idx.add_chunk(&entity_chunk).unwrap();
        idx.add_chunk(&content_chunk).unwrap();
        idx.commit().unwrap();

        let results = idx.search("FooParser", 10).unwrap();
        assert!(
            results.len() >= 2,
            "expected at least 2 results, got {}",
            results.len()
        );
        // The entity_names match (chunk 1) should rank higher than content-only (chunk 2).
        assert_eq!(
            results[0].0, "1",
            "entity_names match should rank first (got chunk {})",
            results[0].0
        );
    }

    #[test]
    fn signature_match_ranks_above_content_only() {
        let idx = TantivyIndex::create_in_ram().unwrap();

        // Chunk 1: "compute_pagerank" appears in signature field.
        let sig_chunk = Chunk {
            id: 10,
            file_path: "src/graph.rs".to_string(),
            language: Language::Rust,
            content: "fn compute_pagerank(g: &Graph) -> Vec<f64> { vec![] }".to_string(),
            byte_start: 0,
            byte_end: 55,
            line_start: 0,
            line_end: 5,
            scope_chain: vec!["graph".to_string()],
            signatures: vec!["fn compute_pagerank(g: &Graph) -> Vec<f64>".to_string()],
            entity_names: vec!["compute_pagerank".to_string()],
            doc_comments: String::new(),
        };

        // Chunk 2: "compute_pagerank" appears only in content (a comment reference).
        let ref_chunk = Chunk {
            id: 11,
            file_path: "src/engine.rs".to_string(),
            language: Language::Rust,
            content: "// After indexing, we call compute_pagerank to rank nodes".to_string(),
            byte_start: 0,
            byte_end: 60,
            line_start: 0,
            line_end: 5,
            scope_chain: vec!["engine".to_string()],
            signatures: vec![],
            entity_names: vec!["index_files".to_string()],
            doc_comments: String::new(),
        };

        idx.add_chunk(&sig_chunk).unwrap();
        idx.add_chunk(&ref_chunk).unwrap();
        idx.commit().unwrap();

        let results = idx.search("compute_pagerank", 10).unwrap();
        assert!(
            results.len() >= 2,
            "expected at least 2 results, got {}",
            results.len()
        );
        assert_eq!(
            results[0].0, "10",
            "signature match should rank first (got chunk {})",
            results[0].0
        );
    }

    #[test]
    fn custom_bm25_config_changes_ranking() {
        use crate::config::Bm25Config;

        // Create index with entity_names boost of 10.0 (very high) — ensures entity match wins.
        let high_entity = Bm25Config {
            entity_names_boost: 10.0,
            signature_boost: 1.0,
            scope_chain_boost: 1.0,
            content_boost: 1.0,
            ..Default::default()
        };
        let idx = TantivyIndex::create_in_ram_with_config(high_entity).unwrap();

        let entity_chunk = Chunk {
            id: 100,
            file_path: "src/a.rs".to_string(),
            language: Language::Rust,
            content: "struct Widget {}".to_string(),
            byte_start: 0,
            byte_end: 16,
            line_start: 0,
            line_end: 1,
            scope_chain: vec![],
            signatures: vec![],
            entity_names: vec!["Widget".to_string()],
            doc_comments: String::new(),
        };
        let content_chunk = Chunk {
            id: 101,
            file_path: "src/b.rs".to_string(),
            language: Language::Rust,
            content: "// Widget is used here for rendering Widget data to Widget output"
                .to_string(),
            byte_start: 0,
            byte_end: 65,
            line_start: 0,
            line_end: 1,
            scope_chain: vec![],
            signatures: vec![],
            entity_names: vec!["render".to_string()],
            doc_comments: String::new(),
        };

        idx.add_chunk(&entity_chunk).unwrap();
        idx.add_chunk(&content_chunk).unwrap();
        idx.commit().unwrap();

        let results = idx.search("Widget", 10).unwrap();
        assert!(results.len() >= 2, "expected at least 2 results");
        // With entity_names_boost=10.0, entity match should dominate.
        assert_eq!(
            results[0].0, "100",
            "high entity_names_boost should make entity match rank first"
        );
    }

    #[test]
    fn split_identifier_words_camel_case() {
        let words = split_identifier_words("createRateLimiter");
        assert!(words.contains(&"create".to_string()));
        assert!(words.contains(&"rate".to_string()));
        assert!(words.contains(&"limiter".to_string()));
        assert!(words.contains(&"createratelimiter".to_string()));
    }

    #[test]
    fn split_identifier_words_snake_case() {
        let words = split_identifier_words("fixed_window_rate_limit");
        assert!(words.contains(&"fixed".to_string()));
        assert!(words.contains(&"window".to_string()));
        assert!(words.contains(&"rate".to_string()));
        assert!(words.contains(&"limit".to_string()));
    }

    #[test]
    fn generate_path_segments_strips_extension() {
        let segments = generate_path_segments("src/cron/fixed-window-rate-limit.ts");
        assert!(segments.contains("cron"));
        assert!(segments.contains("fixed"));
        assert!(segments.contains("window"));
        assert!(segments.contains("rate"));
        assert!(segments.contains("limit"));
        assert!(!segments.contains("ts"));
    }
}
