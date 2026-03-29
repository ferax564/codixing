//! Queue-based embedding pipeline v2 backed by RustQueue.
//!
//! File-grouped jobs with late chunking support. Each job represents one
//! source file and its chunk IDs. The worker reads the file, attempts late
//! chunking, and falls back to per-chunk embedding. Unfinished jobs survive
//! crashes and resume on restart.

use crate::embedder::Embedder;
use crate::error::{CodixingError, Result};
use crate::retriever::ChunkMeta;
use crate::vector::VectorIndex;
use dashmap::DashMap;
use rustqueue::RustQueue;
use std::path::Path;
use std::sync::Arc;

/// Repos with fewer pending chunks than this skip the queue entirely
/// and use the direct sync path (no redb I/O, no JSON serialization).
pub const QUEUE_THRESHOLD: usize = 1000;

/// Map RustQueue errors into CodixingError::Embedding.
fn queue_err(e: impl std::fmt::Display) -> CodixingError {
    CodixingError::Embedding(format!("queue error: {e}"))
}

/// Run an async future from synchronous code, regardless of whether
/// a tokio runtime is already active.
pub(super) fn block_on_async<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
            rt.block_on(fut)
        }
    }
}

/// Push one job per source file to the `"embeddings"` queue.
///
/// Groups pending chunk IDs by file path from `chunk_meta`, then pushes
/// a `"late-chunk-file"` job for each file. Returns the number of jobs pushed.
pub async fn push_file_embed_jobs(
    rq: &Arc<RustQueue>,
    pending: &DashMap<u64, String>,
    chunk_meta: &DashMap<u64, ChunkMeta>,
) -> Result<usize> {
    let mut file_chunks: std::collections::HashMap<String, Vec<u64>> =
        std::collections::HashMap::new();
    for entry in pending.iter() {
        if let Some(meta) = chunk_meta.get(entry.key()) {
            file_chunks
                .entry(meta.file_path.clone())
                .or_default()
                .push(*entry.key());
        }
    }

    let mut job_count = 0;
    for (file_path, chunk_ids) in &file_chunks {
        rq.push(
            "embeddings",
            "late-chunk-file",
            serde_json::json!({ "file": file_path, "ids": chunk_ids }),
            None,
        )
        .await
        .map_err(queue_err)?;
        job_count += 1;
    }

    Ok(job_count)
}

/// Drain the embedding queue, processing all pending jobs.
///
/// Pulls jobs in batches of 50, embeds each file using late chunking
/// (with per-chunk fallback via `embed_single_file`), and inserts vectors.
/// Returns the total number of chunks embedded.
pub async fn drain_embed_queue(
    rq: &Arc<RustQueue>,
    embedder: &Embedder,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    root: &Path,
) -> Result<usize> {
    let mut total_embedded = 0;

    loop {
        let jobs = rq.pull("embeddings", 50).await.map_err(queue_err)?;
        if jobs.is_empty() {
            break;
        }

        for job in &jobs {
            let file_path: String = serde_json::from_value(job.data["file"].clone())
                .map_err(|e| CodixingError::Embedding(format!("bad job payload: {e}")))?;
            let chunk_ids: Vec<u64> = serde_json::from_value(job.data["ids"].clone())
                .map_err(|e| CodixingError::Embedding(format!("bad job payload: {e}")))?;

            match super::indexing::embed_single_file(
                embedder, chunk_meta, vec_idx, contextual, root, &file_path, &chunk_ids,
            ) {
                Ok(n) => {
                    total_embedded += n;
                    rq.ack(job.id, None).await.map_err(queue_err)?;
                }
                Err(e) => {
                    let _ = rq.fail(job.id, &e.to_string()).await;
                }
            }
        }
    }

    Ok(total_embedded)
}
