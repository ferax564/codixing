//! Integration tests for the RustQueue-backed embedding pipeline v2.
#![cfg(feature = "rustqueue")]

use codixing_core::engine::embed_queue::{QUEUE_THRESHOLD, push_file_embed_jobs};
use codixing_core::retriever::ChunkMeta;
use dashmap::DashMap;
use rustqueue::RustQueue;
use std::sync::Arc;

fn mock_chunk(id: u64, file_path: &str, content: &str) -> ChunkMeta {
    ChunkMeta {
        chunk_id: id,
        file_path: file_path.to_string(),
        language: "rust".to_string(),
        line_start: 0,
        line_end: 1,
        signature: String::new(),
        scope_chain: Vec::new(),
        entity_names: Vec::new(),
        content: content.to_string(),
        content_hash: 0,
    }
}

#[tokio::test]
async fn push_file_jobs_groups_by_file() {
    let rq = Arc::new(RustQueue::memory().build().unwrap());

    let chunk_meta: DashMap<u64, ChunkMeta> = DashMap::new();
    chunk_meta.insert(1, mock_chunk(1, "src/a.rs", "fn a() {}"));
    chunk_meta.insert(2, mock_chunk(2, "src/a.rs", "fn b() {}"));
    chunk_meta.insert(3, mock_chunk(3, "src/b.rs", "fn c() {}"));

    let pending: DashMap<u64, String> = DashMap::new();
    pending.insert(1, String::new());
    pending.insert(2, String::new());
    pending.insert(3, String::new());

    let jobs = push_file_embed_jobs(&rq, &pending, &chunk_meta)
        .await
        .unwrap();
    assert_eq!(jobs, 2); // 2 files → 2 jobs

    let stats = rq.get_queue_stats("embeddings").await.unwrap();
    assert_eq!(stats.waiting, 2);
}

#[tokio::test]
async fn push_file_jobs_empty_pending() {
    let rq = Arc::new(RustQueue::memory().build().unwrap());
    let chunk_meta: DashMap<u64, ChunkMeta> = DashMap::new();
    let pending: DashMap<u64, String> = DashMap::new();

    let jobs = push_file_embed_jobs(&rq, &pending, &chunk_meta)
        .await
        .unwrap();
    assert_eq!(jobs, 0);
}

#[test]
fn queue_threshold_is_1000() {
    assert_eq!(QUEUE_THRESHOLD, 1000);
}
