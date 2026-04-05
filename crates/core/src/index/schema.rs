use tantivy::schema::{
    FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions,
};

/// All named fields in the Tantivy schema.
#[derive(Debug, Clone)]
pub struct SchemaFields {
    /// Deterministic chunk identifier (xxh3 hash as string).
    pub chunk_id: Field,
    /// Path of the source file (code-tokenized for search).
    pub file_path: Field,
    /// Exact file path (STRING, for deletion by term).
    pub file_path_exact: Field,
    /// Programming language name.
    pub language: Field,
    /// Source code content of the chunk.
    pub content: Field,
    /// AST scope chain (e.g. `"MyModule MyClass my_method"`).
    pub scope_chain: Field,
    /// Entity signatures (e.g. `fn foo(x: i32) -> bool`).
    pub signature: Field,
    /// Entity names contained in this chunk.
    pub entity_names: Field,
    /// First line of the chunk (0-indexed).
    pub line_start: Field,
    /// Last line of the chunk (exclusive, 0-indexed).
    pub line_end: Field,
    /// Byte offset of the chunk start.
    pub byte_start: Field,
    /// Byte offset of the chunk end.
    pub byte_end: Field,
    /// Doc comments extracted from entities in the chunk (stemmed, stored).
    pub doc_comment: Field,
    /// Camel/snake-split words from entity names (stemmed, unstored).
    pub identifier_words: Field,
    /// Directory/filename segments from the file path (stemmed, unstored).
    pub path_segments: Field,
    /// Document type: "code", "config", or "doc".
    pub doc_type: Field,
}

/// Build the Tantivy schema and return it together with field handles.
///
/// Text fields that contain code identifiers use the custom `"code"` tokenizer,
/// which must be registered on the index *after* creation.
pub fn build_schema() -> (Schema, SchemaFields) {
    let mut builder = Schema::builder();

    // Code-aware text options: tokenized with the custom "code" tokenizer,
    // positions stored so phrase queries work.
    let code_text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("code")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();

    // Code text without stored (for fields we do not need to retrieve).
    let code_text_unstored = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("code")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );

    // Stemmed text (stored) for doc_comment — retrievable for display/reranking.
    let code_stemmed_stored = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("code_stemmed")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    // Stemmed text (unstored) for search-only fields.
    let code_stemmed_unstored = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("code_stemmed")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );

    let chunk_id = builder.add_text_field("chunk_id", STRING | STORED);
    let file_path = builder.add_text_field("file_path", code_text.clone());
    let file_path_exact = builder.add_text_field("file_path_exact", STRING);
    let language = builder.add_text_field("language", STRING | STORED);
    let content = builder.add_text_field("content", code_text.clone());
    // scope_chain is stored so BM25Retriever can populate SearchResult.scope_chain.
    let scope_chain = builder.add_text_field("scope_chain", code_text.clone());
    let signature = builder.add_text_field("signature", code_text.clone());
    let entity_names = builder.add_text_field("entity_names", code_text_unstored);
    let line_start = builder.add_u64_field("line_start", STORED | FAST);
    let line_end = builder.add_u64_field("line_end", STORED | FAST);
    let byte_start = builder.add_u64_field("byte_start", STORED | FAST);
    let byte_end = builder.add_u64_field("byte_end", STORED | FAST);
    let doc_comment = builder.add_text_field("doc_comment", code_stemmed_stored);
    let identifier_words =
        builder.add_text_field("identifier_words", code_stemmed_unstored.clone());
    let path_segments = builder.add_text_field("path_segments", code_stemmed_unstored);
    let doc_type = builder.add_text_field("doc_type", STRING | STORED);

    let schema = builder.build();

    let fields = SchemaFields {
        chunk_id,
        file_path,
        file_path_exact,
        language,
        content,
        scope_chain,
        signature,
        entity_names,
        line_start,
        line_end,
        byte_start,
        byte_end,
        doc_comment,
        identifier_words,
        path_segments,
        doc_type,
    };

    (schema, fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_expected_fields() {
        let (schema, fields) = build_schema();
        // Verify all fields are distinct and retrievable by name.
        assert_eq!(schema.get_field("chunk_id").unwrap(), fields.chunk_id);
        assert_eq!(schema.get_field("file_path").unwrap(), fields.file_path);
        assert_eq!(schema.get_field("language").unwrap(), fields.language);
        assert_eq!(schema.get_field("content").unwrap(), fields.content);
        assert_eq!(schema.get_field("line_start").unwrap(), fields.line_start);
        assert_eq!(schema.get_field("doc_comment").unwrap(), fields.doc_comment);
        assert_eq!(
            schema.get_field("identifier_words").unwrap(),
            fields.identifier_words
        );
        assert_eq!(
            schema.get_field("path_segments").unwrap(),
            fields.path_segments
        );
    }

    #[test]
    fn schema_has_doc_type_field() {
        let (schema, fields) = build_schema();
        assert_eq!(schema.get_field("doc_type").unwrap(), fields.doc_type);
    }
}
