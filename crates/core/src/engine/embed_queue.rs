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

/// Number of jobs to pull per batch when draining the queue.
const DRAIN_PULL_BATCH: u32 = 50;

/// Number of jobs each parallel worker pulls per iteration.
const WORKER_PULL_BATCH: u32 = 10;

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

/// Parse a `"late-chunk-file"` job payload into `(file_path, chunk_ids)`.
fn parse_file_job(job: &rustqueue::Job) -> Result<(String, Vec<u64>)> {
    let file_path: String = serde_json::from_value(job.data["file"].clone())
        .map_err(|e| CodixingError::Embedding(format!("bad job payload: {e}")))?;
    let chunk_ids: Vec<u64> = serde_json::from_value(job.data["ids"].clone())
        .map_err(|e| CodixingError::Embedding(format!("bad job payload: {e}")))?;
    Ok((file_path, chunk_ids))
}

/// Drain the embedding queue, processing all pending jobs.
///
/// Pulls jobs in batches of [`DRAIN_PULL_BATCH`], embeds each file using late chunking
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
        let jobs = rq
            .pull("embeddings", DRAIN_PULL_BATCH)
            .await
            .map_err(queue_err)?;
        if jobs.is_empty() {
            break;
        }

        for job in &jobs {
            let (file_path, chunk_ids) = parse_file_job(job)?;

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

/// Drain the embedding queue with N parallel workers.
///
/// Each worker creates its own `Embedder` (separate ONNX session) and pulls
/// jobs from the queue concurrently. Embeddings are collected in thread-local
/// buffers, then bulk-inserted into the VectorIndex after all workers finish.
///
/// Memory cost: ~200 MB per worker (BGE-Small ONNX session).
/// Expected speedup: close to N× on the embedding step.
pub fn drain_embed_queue_parallel(
    rq: &Arc<RustQueue>,
    model: &crate::config::EmbeddingModel,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    root: &Path,
    num_workers: usize,
) -> Result<usize> {
    if num_workers <= 1 {
        // Single worker: create an embedder and use the simple async drain.
        let embedder = Embedder::new(model)?;
        return block_on_async(drain_embed_queue(
            rq, &embedder, chunk_meta, vec_idx, contextual, root,
        ));
    }

    tracing::info!(workers = num_workers, "starting parallel embedding drain");

    // Phase 1: N workers embed in parallel, collect (id, vec, path) tuples.
    let collected: Vec<(u64, Vec<f32>, String)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..num_workers)
            .map(|worker_id| {
                s.spawn(move || {
                    let embedder = match Embedder::new(model) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::warn!(worker = worker_id, error = %e, "failed to create embedder");
                            return Ok(Vec::new());
                        }
                    };
                    tracing::debug!(worker = worker_id, "embedding worker started");

                    let mut local: Vec<(u64, Vec<f32>, String)> = Vec::new();
                    loop {
                        let jobs = block_on_async(async {
                            rq.pull("embeddings", WORKER_PULL_BATCH).await.map_err(queue_err)
                        })?;
                        if jobs.is_empty() {
                            break;
                        }
                        for job in &jobs {
                            let (file_path, chunk_ids) = parse_file_job(job)?;

                            match super::indexing::embed_file_collect(
                                &embedder,
                                chunk_meta,
                                contextual,
                                root,
                                &file_path,
                                &chunk_ids,
                            ) {
                                Ok(vecs) => {
                                    local.extend(vecs);
                                    block_on_async(async {
                                        rq.ack(job.id, None).await.map_err(queue_err)
                                    })?;
                                }
                                Err(e) => {
                                    let _ = block_on_async(async {
                                        rq.fail(job.id, &e.to_string()).await
                                    });
                                }
                            }
                        }
                    }
                    tracing::debug!(worker = worker_id, chunks = local.len(), "worker done");
                    Ok::<_, CodixingError>(local)
                })
            })
            .collect();

        let mut all = Vec::new();
        for h in handles {
            match h.join() {
                Ok(Ok(vecs)) => all.extend(vecs),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(CodixingError::Embedding("worker thread panicked".into())),
            }
        }
        Ok(all)
    })?;

    let total = collected.len();
    tracing::info!(
        chunks = total,
        "parallel embedding complete, inserting vectors"
    );

    // Phase 2: Sequential bulk insert into VectorIndex.
    for (chunk_id, embedding, file_path) in &collected {
        if let Err(e) = vec_idx.add_mut(*chunk_id, embedding, file_path) {
            tracing::warn!(error = %e, chunk_id, "failed to add vector");
        }
    }

    Ok(total)
}

/// Embed pending chunks using the best available path.
///
/// - If `rq` is available and `pending.len() >= QUEUE_THRESHOLD`: use the queue
///   with parallel workers.
/// - Otherwise: use the direct sync path via `embed_and_index_chunks`.
#[allow(clippy::too_many_arguments)]
pub fn embed_pending(
    rq: Option<&Arc<RustQueue>>,
    pending: &DashMap<u64, String>,
    chunk_meta: &DashMap<u64, ChunkMeta>,
    embedder: &Embedder,
    vec_idx: &mut VectorIndex,
    contextual: bool,
    root: &Path,
    model: &crate::config::EmbeddingModel,
) -> Result<()> {
    if let Some(rq) = rq {
        if pending.len() >= QUEUE_THRESHOLD {
            let num_workers = std::thread::available_parallelism()
                .map(|n| n.get().min(4))
                .unwrap_or(1);
            block_on_async(async {
                let pushed = push_file_embed_jobs(rq, pending, chunk_meta).await?;
                tracing::info!(jobs = pushed, "embedding jobs queued");
                Ok::<(), crate::error::CodixingError>(())
            })?;
            let total = drain_embed_queue_parallel(
                rq,
                model,
                chunk_meta,
                vec_idx,
                contextual,
                root,
                num_workers,
            )?;
            tracing::info!(
                chunks = total,
                workers = num_workers,
                "embedding complete via queue"
            );
            return Ok(());
        }
    }
    super::indexing::embed_and_index_chunks(
        pending, chunk_meta, embedder, vec_idx, contextual, root,
    )
}
