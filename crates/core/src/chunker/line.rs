//! Line-based fallback chunker for testing and unsupported languages.

use crate::config::ChunkConfig;
use crate::language::Language;

use super::{Chunk, Chunker, chunk_id};

/// Simple line-based chunker: splits every N lines.
pub struct LineChunker;

impl Chunker for LineChunker {
    fn chunk(
        &self,
        file_path: &str,
        source: &[u8],
        _tree: Option<&tree_sitter::Tree>,
        language: Language,
        config: &ChunkConfig,
    ) -> Vec<Chunk> {
        let text = String::from_utf8_lossy(source);
        let lines: Vec<&str> = text.lines().collect();

        if lines.is_empty() {
            return Vec::new();
        }

        // Estimate lines per chunk based on max_chars.
        // Assume ~40 chars per line average.
        let lines_per_chunk = (config.max_chars / 40).max(10);
        let mut chunks = Vec::new();

        for (chunk_idx, line_chunk) in lines.chunks(lines_per_chunk).enumerate() {
            let line_start = chunk_idx * lines_per_chunk;
            let line_end = line_start + line_chunk.len();
            let content = line_chunk.join("\n");

            // Compute byte offsets.
            let byte_start: usize = lines[..line_start].iter().map(|l| l.len() + 1).sum();
            let byte_end = byte_start + content.len();

            chunks.push(Chunk {
                id: chunk_id(file_path, byte_start, byte_end),
                file_path: file_path.to_string(),
                language,
                content,
                byte_start,
                byte_end,
                line_start,
                line_end,
                scope_chain: Vec::new(),
                signatures: Vec::new(),
                entity_names: Vec::new(),
                doc_comments: String::new(),
            });
        }

        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChunkConfig;

    fn chunk_lines(source: &str, max_chars: usize) -> Vec<Chunk> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let config = ChunkConfig {
            max_chars,
            min_chars: 0,
            overlap_ratio: 0.0,
        };
        LineChunker.chunk(
            "test.rs",
            source.as_bytes(),
            Some(&tree),
            Language::Rust,
            &config,
        )
    }

    #[test]
    fn basic_line_chunking() {
        let src = (0..100)
            .map(|i| format!("let x{i} = {i};"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = chunk_lines(&src, 400);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert_eq!(c.file_path, "test.rs");
        }
    }

    #[test]
    fn empty_source() {
        let chunks = chunk_lines("", 400);
        assert!(chunks.is_empty());
    }
}
