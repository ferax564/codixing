pub mod cast;
pub mod line;

use serde::{Deserialize, Serialize};

use crate::config::ChunkConfig;
use crate::language::Language;

/// A chunk of source code with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    /// Deterministic ID (xxh3 of file_path + byte_range).
    pub id: u64,
    /// File this chunk belongs to.
    pub file_path: String,
    /// Language of the source file.
    pub language: Language,
    /// The actual source text.
    pub content: String,
    /// Byte range in the original source.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Line range (0-indexed, inclusive start, exclusive end).
    pub line_start: usize,
    pub line_end: usize,
    /// AST scope chain (e.g., ["MyModule", "MyClass", "my_method"]).
    pub scope_chain: Vec<String>,
    /// Signatures of entities contained in this chunk.
    pub signatures: Vec<String>,
    /// Names of entities contained in this chunk.
    pub entity_names: Vec<String>,
}

/// Count non-whitespace characters in a byte slice.
pub fn non_ws_chars(s: &[u8]) -> usize {
    s.iter().filter(|b| !b.is_ascii_whitespace()).count()
}

/// Compute a deterministic chunk ID from file path and byte range.
pub fn chunk_id(file_path: &str, byte_start: usize, byte_end: usize) -> u64 {
    use xxhash_rust::xxh3::xxh3_64;
    let mut buf = Vec::with_capacity(file_path.len() + 16);
    buf.extend_from_slice(file_path.as_bytes());
    buf.extend_from_slice(&byte_start.to_le_bytes());
    buf.extend_from_slice(&byte_end.to_le_bytes());
    xxh3_64(&buf)
}

/// Trait for swappable chunking strategies.
pub trait Chunker: Send + Sync {
    fn chunk(
        &self,
        file_path: &str,
        source: &[u8],
        tree: Option<&tree_sitter::Tree>,
        language: Language,
        config: &ChunkConfig,
    ) -> Vec<Chunk>;
}
