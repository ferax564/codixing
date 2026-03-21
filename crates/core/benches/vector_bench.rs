use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use rand::Rng;

const DIMS: usize = 384; // BGE-small-en dimensionality

fn random_vector(dims: usize) -> Vec<f32> {
    let mut rng = rand::rng();
    (0..dims).map(|_| rng.random_range(-1.0..1.0)).collect()
}

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

fn bench_brute_force_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_search");

    for &size in &[1_000, 5_000, 10_000, 50_000] {
        let vectors: Vec<Vec<f32>> = (0..size).map(|_| random_vector(DIMS)).collect();
        let query = random_vector(DIMS);

        group.bench_with_input(
            BenchmarkId::new("brute_force", size),
            &(&vectors, &query),
            |b, (vecs, q)| {
                b.iter(|| {
                    let mut scores: Vec<(usize, f32)> = vecs
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (i, cosine_sim(q, v)))
                        .collect();
                    scores.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                    scores.truncate(10);
                    scores
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_brute_force_search);
criterion_main!(benches);
