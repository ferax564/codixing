//! Embedding inference for code retrieval.
//!
//! This module defines the [`Embedder`] trait for converting text into dense
//! vector representations, plus concrete implementations:
//!
//! - [`MockEmbedder`] -- a deterministic, zero-dependency test embedder
//!   (always available).
//! - [`OnnxEmbedder`] -- ONNX Runtime inference using `all-MiniLM-L6-v2`
//!   (requires the `vector` feature).

#[cfg(feature = "vector")]
mod onnx;

#[cfg(test)]
mod tests;

#[cfg(feature = "vector")]
pub use onnx::OnnxEmbedder;

use crate::error::CodeforgeError;

/// Trait for embedding text into dense float vectors.
///
/// Implementations must be `Send + Sync` so they can be shared across async
/// tasks and worker threads.
pub trait Embedder: Send + Sync {
    /// Embed a single text string into a vector.
    fn embed(&self, text: &str) -> Result<Vec<f32>, CodeforgeError>;

    /// Embed a batch of texts.
    ///
    /// The default implementation calls [`embed`](Embedder::embed) in a loop.
    /// Implementations backed by a batched runtime should override this for
    /// better throughput.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CodeforgeError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Return the embedding dimensionality.
    fn dimension(&self) -> usize;
}

/// A deterministic test embedder that does not require model files.
///
/// Produces a repeatable pseudo-random vector seeded from the input text hash.
/// Useful for unit tests and development without downloading ONNX models.
pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    /// Create a new mock embedder with the given output dimensionality.
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

impl Embedder for MockEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>, CodeforgeError> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let seed = hasher.finish();
        let mut rng_state = seed;

        Ok((0..self.dim)
            .map(|_| {
                rng_state = rng_state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                ((rng_state >> 33) as f32) / (u32::MAX as f32) - 0.5
            })
            .collect())
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}
