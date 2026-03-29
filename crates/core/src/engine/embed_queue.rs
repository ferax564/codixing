//! Queue-based embedding pipeline backed by RustQueue.
//!
//! Instead of embedding chunks synchronously, this module pushes batches
//! as jobs to a RustQueue instance. A worker loop pulls and processes them.
//! Unfinished jobs survive crashes and resume on restart.

use crate::embedder::Embedder;
use crate::error::{CodixingError, Result};
use crate::retriever::ChunkMeta;
use crate::vector::VectorIndex;
use dashmap::DashMap;
use rustqueue::RustQueue;
use std::sync::Arc;

/// Map RustQueue errors into CodixingError::Embedding.
fn queue_err(e: impl std::fmt::Display) -> CodixingError {
    CodixingError::Embedding(format!("queue error: {e}"))
}

/// Run an async future from synchronous code, regardless of whether
/// a tokio runtime is already active.
///
/// - Inside an existing runtime: uses `block_in_place` + `block_on`.
/// - Outside any runtime: spins up a temporary current-thread runtime.
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

/// Push embedding work as batched jobs to the `"embeddings"` queue.
///
/// Each job payload contains a list of chunk IDs to embed.
/// Returns the number of jobs pushed.
pub async fn push_embed_jobs(
    rq: &Arc<RustQueue>,
    pending: &DashMap<u64, String>,
    batch_size: usize,
) -> Result<usize> {
    let chunk_ids: Vec<u64> = pending.iter().map(|r| *r.key()).collect();
    let mut job_count = 0;

    for batch in chunk_ids.chunks(batch_size) {
        let ids: Vec<u64> = batch.to_vec();
        rq.push(
            "embeddings",
            "embed-batch",
            serde_json::json!({ "chunk_ids": ids }),
            None,
        )
        .await
        .map_err(queue_err)?;
        job_count += 1;
    }

    Ok(job_count)
}

/// Process a single round of embedding jobs from the queue.
///
/// Pulls up to `max_jobs` from the `"embeddings"` queue, embeds each batch,
/// and stores vectors in the index. Acks on success, fails on error.
///
/// Returns the number of chunks embedded in this round.
pub async fn run_embed_worker_batch(
    rq: &Arc<RustQueue>,
    embedder: &Embedder,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    max_jobs: u32,
) -> Result<usize> {
    let jobs = rq.pull("embeddings", max_jobs).await.map_err(queue_err)?;
    let mut total_embedded = 0;

    for job in &jobs {
        let chunk_ids: Vec<u64> = serde_json::from_value(job.data["chunk_ids"].clone())
            .map_err(|e| CodixingError::Embedding(format!("malformed job payload: {e}")))?;

        let texts: Vec<String> = chunk_ids
            .iter()
            .filter_map(|id| {
                chunk_meta
                    .get(id)
                    .map(|m| super::indexing::make_embed_text(&m, contextual))
            })
            .collect();

        if texts.is_empty() {
            rq.ack(job.id, None).await.map_err(queue_err)?;
            continue;
        }

        match embedder.embed(texts) {
            Ok(embeddings) => {
                for (chunk_id, embedding) in chunk_ids.iter().zip(embeddings.into_iter()) {
                    let file_path = chunk_meta
                        .get(chunk_id)
                        .map(|m| m.file_path.clone())
                        .unwrap_or_default();
                    if let Err(e) = vec_idx.add_mut(*chunk_id, &embedding, &file_path) {
                        tracing::warn!(error = %e, chunk_id, "failed to add vector");
                    }
                    total_embedded += 1;
                }
                rq.ack(job.id, None).await.map_err(queue_err)?;
            }
            Err(e) => {
                let _ = rq.fail(job.id, &e.to_string()).await;
            }
        }
    }

    Ok(total_embedded)
}
