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
        let mut line_spans = Vec::new();
        let mut line_start = 0usize;
        for (idx, &byte) in source.iter().enumerate() {
            if byte == b'\n' {
                let line_end = if idx > line_start && source[idx - 1] == b'\r' {
                    idx - 1
                } else {
                    idx
                };
                line_spans.push((line_start, line_end));
                line_start = idx + 1;
            }
        }
        if line_start < source.len() {
            line_spans.push((line_start, source.len()));
        }

        if line_spans.is_empty() {
            return Vec::new();
        }

        // Estimate lines per chunk based on max_chars.
        // Assume ~40 chars per line average.
        let lines_per_chunk = (config.max_chars / 40).max(10);
        let mut chunks = Vec::new();

        for (chunk_idx, line_chunk) in line_spans.chunks(lines_per_chunk).enumerate() {
            let line_start = chunk_idx * lines_per_chunk;
            let line_end = line_start + line_chunk.len();
            let byte_start = line_chunk.first().map(|(start, _)| *start).unwrap_or(0);
            let byte_end = line_chunk.last().map(|(_, end)| *end).unwrap_or(byte_start);
            let content = String::from_utf8_lossy(&source[byte_start..byte_end]).to_string();

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

    #[test]
    fn crlf_offsets_use_real_byte_widths() {
        let src = (0..11)
            .map(|i| format!("l{i:02}"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let chunks = chunk_lines(&src, 1);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].byte_start, 0);
        assert_eq!(chunks[0].byte_end, 48);
        assert_eq!(
            &src.as_bytes()[chunks[0].byte_start..chunks[0].byte_end],
            b"l00\r\nl01\r\nl02\r\nl03\r\nl04\r\nl05\r\nl06\r\nl07\r\nl08\r\nl09"
        );
        assert_eq!(chunks[1].byte_start, 50);
        assert_eq!(chunks[1].byte_end, src.len());
        assert_eq!(
            &src.as_bytes()[chunks[1].byte_start..chunks[1].byte_end],
            b"l10"
        );
    }
}
