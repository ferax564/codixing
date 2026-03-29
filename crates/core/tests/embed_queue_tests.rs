//! Integration tests for the RustQueue-backed embedding pipeline.
#![cfg(feature = "rustqueue")]

use codixing_core::engine::embed_queue::push_embed_jobs;
use dashmap::DashMap;
use rustqueue::RustQueue;
use std::sync::Arc;

#[tokio::test]
async fn push_embed_jobs_creates_correct_batch_count() {
    let rq = Arc::new(RustQueue::memory().build().unwrap());

    // 3 pending chunks with batch_size=2 → ceil(3/2) = 2 jobs.
    let pending: DashMap<u64, String> = DashMap::new();
    pending.insert(1, "fn hello() {}".into());
    pending.insert(2, "fn world() {}".into());
    pending.insert(3, "fn foo() {}".into());

    let job_count = push_embed_jobs(&rq, &pending, 2).await.unwrap();
    assert_eq!(job_count, 2);

    let stats = rq.get_queue_stats("embeddings").await.unwrap();
    assert_eq!(stats.waiting, 2);
}

#[tokio::test]
async fn push_embed_jobs_empty_pending_creates_no_jobs() {
    let rq = Arc::new(RustQueue::memory().build().unwrap());
    let pending: DashMap<u64, String> = DashMap::new();

    let job_count = push_embed_jobs(&rq, &pending, 256).await.unwrap();
    assert_eq!(job_count, 0);
}

#[tokio::test]
async fn push_embed_jobs_single_batch() {
    let rq = Arc::new(RustQueue::memory().build().unwrap());

    let pending: DashMap<u64, String> = DashMap::new();
    pending.insert(1, "fn a() {}".into());
    pending.insert(2, "fn b() {}".into());

    // batch_size=10 > 2 items → exactly 1 job.
    let job_count = push_embed_jobs(&rq, &pending, 10).await.unwrap();
    assert_eq!(job_count, 1);

    let stats = rq.get_queue_stats("embeddings").await.unwrap();
    assert_eq!(stats.waiting, 1);
}
