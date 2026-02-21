//! Product Quantization (PQ) for memory-efficient approximate vector search.
//!
//! Splits high-dimensional vectors into subspaces, trains k-means centroids per
//! subspace, and encodes vectors as compact byte codes. Search uses Asymmetric
//! Distance Computation (ADC) for fast approximate nearest-neighbor retrieval.

use super::simd_distance;

/// Configuration for product quantization training.
#[derive(Debug, Clone)]
pub struct PqConfig {
    /// Number of subspaces to split vectors into. Must divide vector dimension evenly.
    pub num_subspaces: usize,
    /// Number of centroids per subspace. Max 256 (encoded as `u8`).
    pub num_centroids: usize,
    /// Number of k-means iterations during training.
    pub kmeans_iters: usize,
}

impl Default for PqConfig {
    fn default() -> Self {
        Self {
            num_subspaces: 48,
            num_centroids: 256,
            kmeans_iters: 20,
        }
    }
}

/// Product quantizer: encodes vectors as compact byte codes and supports
/// fast approximate nearest-neighbor search via ADC (Asymmetric Distance
/// Computation).
pub struct ProductQuantizer {
    num_subspaces: usize,
    sub_dim: usize,
    /// Centroids per subspace: `centroids[s][c]` is a `sub_dim`-length vector
    /// for subspace `s`, centroid `c`.
    centroids: Vec<Vec<Vec<f32>>>,
    /// Encoded vectors: `codes[i][s]` is the centroid index for vector `i`,
    /// subspace `s`.
    codes: Vec<Vec<u8>>,
}

impl ProductQuantizer {
    /// Train a product quantizer on the given vectors.
    ///
    /// # Panics
    /// - If `vectors` is empty.
    /// - If the vector dimension is not evenly divisible by `config.num_subspaces`.
    /// - If `config.num_centroids` exceeds 256.
    /// - If `config.num_subspaces` is 0.
    pub fn train(vectors: &[Vec<f32>], config: &PqConfig) -> Self {
        assert!(!vectors.is_empty(), "cannot train PQ on empty vectors");
        assert!(
            config.num_subspaces > 0,
            "num_subspaces must be greater than 0"
        );
        assert!(
            config.num_centroids <= 256,
            "num_centroids must be <= 256 for u8 encoding"
        );

        let dim = vectors[0].len();
        assert!(
            dim % config.num_subspaces == 0,
            "vector dimension {dim} is not divisible by num_subspaces {}",
            config.num_subspaces
        );

        let sub_dim = dim / config.num_subspaces;
        let n = vectors.len();
        let k = config.num_centroids.min(n); // cannot have more centroids than vectors

        // Train centroids for each subspace independently.
        let mut centroids: Vec<Vec<Vec<f32>>> = Vec::with_capacity(config.num_subspaces);

        for s in 0..config.num_subspaces {
            let offset = s * sub_dim;

            // Extract subspace data for all vectors.
            let sub_vectors: Vec<&[f32]> = vectors
                .iter()
                .map(|v| &v[offset..offset + sub_dim])
                .collect();

            let sub_centroids = kmeans(&sub_vectors, k, sub_dim, config.kmeans_iters);
            centroids.push(sub_centroids);
        }

        // Encode all training vectors.
        let codes = encode_all(vectors, &centroids, config.num_subspaces, sub_dim);

        Self {
            num_subspaces: config.num_subspaces,
            sub_dim,
            centroids,
            codes,
        }
    }

    /// Encode a single vector into PQ codes.
    ///
    /// Returns a vector of `num_subspaces` bytes, each identifying the nearest
    /// centroid in that subspace.
    pub fn encode(&self, vector: &[f32]) -> Vec<u8> {
        encode_vector(vector, &self.centroids, self.num_subspaces, self.sub_dim)
    }

    /// Search for the `k` nearest vectors using Asymmetric Distance Computation.
    ///
    /// Returns `(vector_index, distance)` pairs sorted by ascending L2 distance.
    pub fn search(&self, query: &[f32], k: usize) -> Vec<(usize, f32)> {
        // Build ADC distance table: for each subspace, precompute L2 distance
        // from query subvector to every centroid.
        let mut dist_table: Vec<Vec<f32>> = Vec::with_capacity(self.num_subspaces);
        for s in 0..self.num_subspaces {
            let offset = s * self.sub_dim;
            let query_sub = &query[offset..offset + self.sub_dim];
            let centroid_dists: Vec<f32> = self.centroids[s]
                .iter()
                .map(|c| simd_distance::l2_distance_squared(query_sub, c))
                .collect();
            dist_table.push(centroid_dists);
        }

        // For each encoded vector, sum distance table lookups.
        let mut candidates: Vec<(usize, f32)> = self
            .codes
            .iter()
            .enumerate()
            .map(|(i, code)| {
                let dist: f32 = code
                    .iter()
                    .enumerate()
                    .map(|(s, &c)| dist_table[s][c as usize])
                    .sum();
                (i, dist)
            })
            .collect();

        // Partial sort: find top-k by smallest distance.
        let k = k.min(candidates.len());
        candidates.select_nth_unstable_by(k.saturating_sub(1), |a, b| {
            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        candidates.truncate(k);
        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        candidates
    }

    /// Number of encoded vectors.
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// Whether the quantizer has no encoded vectors.
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Memory used by the encoded vectors in bytes.
    ///
    /// This is `num_vectors * num_subspaces` (each code is 1 byte per subspace).
    pub fn encoded_memory_bytes(&self) -> usize {
        self.codes.len() * self.num_subspaces
    }
}

/// Encode all vectors into PQ codes.
fn encode_all(
    vectors: &[Vec<f32>],
    centroids: &[Vec<Vec<f32>>],
    num_subspaces: usize,
    sub_dim: usize,
) -> Vec<Vec<u8>> {
    vectors
        .iter()
        .map(|v| encode_vector(v, centroids, num_subspaces, sub_dim))
        .collect()
}

/// Encode a single vector into PQ codes.
fn encode_vector(
    vector: &[f32],
    centroids: &[Vec<Vec<f32>>],
    num_subspaces: usize,
    sub_dim: usize,
) -> Vec<u8> {
    let mut code = Vec::with_capacity(num_subspaces);
    for (s, subspace_centroids) in centroids.iter().enumerate().take(num_subspaces) {
        let offset = s * sub_dim;
        let sub = &vector[offset..offset + sub_dim];
        let nearest = nearest_centroid(sub, subspace_centroids);
        code.push(nearest as u8);
    }
    code
}

/// Find the index of the nearest centroid to a subvector (L2 distance).
fn nearest_centroid(sub: &[f32], centroids: &[Vec<f32>]) -> usize {
    let mut best_idx = 0;
    let mut best_dist = f32::MAX;
    for (i, c) in centroids.iter().enumerate() {
        let dist = simd_distance::l2_distance_squared(sub, c);
        if dist < best_dist {
            best_dist = dist;
            best_idx = i;
        }
    }
    best_idx
}

/// Simple k-means clustering on subspace vectors.
///
/// Initializes centroids from the first `k` input vectors, then iterates
/// assignment and recomputation steps.
fn kmeans(vectors: &[&[f32]], k: usize, dim: usize, max_iters: usize) -> Vec<Vec<f32>> {
    let n = vectors.len();
    let k = k.min(n);

    // Initialize centroids from evenly-spaced vectors for better coverage.
    let mut centroids: Vec<Vec<f32>> = if k >= n {
        vectors.iter().map(|v| v.to_vec()).collect()
    } else {
        (0..k)
            .map(|i| {
                let idx = (i as u64 * n as u64 / k as u64) as usize;
                vectors[idx].to_vec()
            })
            .collect()
    };

    let mut assignments = vec![0usize; n];

    for _ in 0..max_iters {
        // Assignment step: assign each vector to nearest centroid.
        let mut changed = false;
        for (i, v) in vectors.iter().enumerate() {
            let mut best_idx = 0;
            let mut best_dist = f32::MAX;
            for (j, c) in centroids.iter().enumerate() {
                let dist = l2_squared_slice(v, c);
                if dist < best_dist {
                    best_dist = dist;
                    best_idx = j;
                }
            }
            if assignments[i] != best_idx {
                assignments[i] = best_idx;
                changed = true;
            }
        }

        if !changed {
            break;
        }

        // Recomputation step: update centroids as mean of assigned vectors.
        let mut sums = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];

        for (i, v) in vectors.iter().enumerate() {
            let c = assignments[i];
            counts[c] += 1;
            for (d, val) in v.iter().enumerate() {
                sums[c][d] += val;
            }
        }

        for j in 0..k {
            if counts[j] > 0 {
                let count = counts[j] as f32;
                for d in 0..dim {
                    centroids[j][d] = sums[j][d] / count;
                }
            }
            // If a centroid has no assigned vectors, keep it unchanged.
        }
    }

    centroids
}

/// Inline L2 squared distance for slices (used in k-means inner loop to avoid
/// the assertion overhead of the public function on every call).
#[inline]
fn l2_squared_slice(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = x - y;
            d * d
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random vector generator.
    fn random_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut vectors = Vec::with_capacity(n);
        let mut state = seed;
        for _ in 0..n {
            let mut v = Vec::with_capacity(dim);
            for _ in 0..dim {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                v.push(((state >> 33) as f32) / (u32::MAX as f32) - 0.5);
            }
            vectors.push(v);
        }
        vectors
    }

    #[test]
    fn pq_train_and_encode() {
        let vectors = random_vectors(500, 384, 42);
        let pq = ProductQuantizer::train(&vectors, &PqConfig::default());
        assert_eq!(pq.len(), 500);
        let codes = pq.encode(&vectors[0]);
        assert_eq!(codes.len(), 48); // 48 subspaces = 48 bytes
    }

    #[test]
    fn pq_memory_reduction() {
        let vectors = random_vectors(1000, 384, 42);
        let pq = ProductQuantizer::train(&vectors, &PqConfig::default());
        let raw_bytes = 1000 * 384 * 4;
        let pq_bytes = pq.encoded_memory_bytes();
        let ratio = raw_bytes as f64 / pq_bytes as f64;
        assert!(
            ratio > 20.0,
            "expected >20x compression, got {ratio:.1}x (raw={raw_bytes}, pq={pq_bytes})"
        );
    }

    /// Generate clustered vectors: `num_clusters` cluster centers with
    /// `n / num_clusters` points each, Gaussian-like noise around each center.
    fn clustered_vectors(n: usize, dim: usize, num_clusters: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut state = seed;
        let mut next_f32 = || -> f32 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as f32) / (u32::MAX as f32) - 0.5
        };

        // Generate cluster centers spread apart.
        let centers: Vec<Vec<f32>> = (0..num_clusters)
            .map(|_| (0..dim).map(|_| next_f32() * 10.0).collect())
            .collect();

        let per_cluster = n / num_clusters;
        let mut vectors = Vec::with_capacity(n);
        for center in &centers {
            for _ in 0..per_cluster {
                let v: Vec<f32> = center
                    .iter()
                    .map(|c| c + next_f32() * 0.3) // small noise around center
                    .collect();
                vectors.push(v);
            }
        }
        // Fill remainder if n % num_clusters != 0
        while vectors.len() < n {
            let v: Vec<f32> = centers[0].iter().map(|c| c + next_f32() * 0.3).collect();
            vectors.push(v);
        }
        vectors
    }

    #[test]
    fn pq_search_recall_at_10() {
        // Use clustered data so PQ centroids can learn meaningful structure.
        let vectors = clustered_vectors(2000, 384, 50, 42);
        let config = PqConfig {
            num_subspaces: 48,
            num_centroids: 256,
            kmeans_iters: 20,
        };
        let pq = ProductQuantizer::train(&vectors, &config);
        let query = &vectors[0];
        let k = 10;

        // Exact brute-force using L2 distance (same metric as PQ ADC).
        let mut exact: Vec<(usize, f32)> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let dist = simd_distance::l2_distance_squared(query, v);
                (i, dist)
            })
            .collect();
        exact.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let exact_ids: Vec<usize> = exact[..k].iter().map(|(i, _)| *i).collect();

        let pq_results = pq.search(query, k);
        let pq_ids: Vec<usize> = pq_results.iter().map(|(i, _)| *i).collect();

        let recall = exact_ids.iter().filter(|id| pq_ids.contains(id)).count() as f64 / k as f64;
        eprintln!("PQ recall@10 = {recall:.2}");
        assert!(recall > 0.5, "PQ recall@10 = {recall:.2}, expected >0.5");
    }

    #[test]
    fn pq_config_must_divide_dimension() {
        let vectors = random_vectors(100, 384, 42);
        let config = PqConfig {
            num_subspaces: 48,
            ..Default::default()
        };
        let pq = ProductQuantizer::train(&vectors, &config);
        assert_eq!(pq.len(), 100);
    }

    #[test]
    #[should_panic(expected = "not divisible")]
    fn pq_rejects_indivisible_subspaces() {
        let vectors = random_vectors(100, 384, 42);
        let config = PqConfig {
            num_subspaces: 50, // 384 % 50 != 0
            ..Default::default()
        };
        ProductQuantizer::train(&vectors, &config);
    }

    #[test]
    fn pq_is_empty_on_no_vectors() {
        // Cannot train on empty, but we can check `is_empty` on a trained set.
        let vectors = random_vectors(10, 384, 42);
        let pq = ProductQuantizer::train(&vectors, &PqConfig::default());
        assert!(!pq.is_empty());
    }

    #[test]
    fn pq_fewer_vectors_than_centroids() {
        // When N < K, k-means should use N centroids.
        let vectors = random_vectors(10, 384, 42);
        let config = PqConfig {
            num_centroids: 256,
            ..Default::default()
        };
        let pq = ProductQuantizer::train(&vectors, &config);
        assert_eq!(pq.len(), 10);
        // All codes should be valid (< 10 since only 10 centroids trained).
        for code in &pq.codes {
            for &c in code {
                assert!((c as usize) < 10, "code {c} exceeds centroid count of 10");
            }
        }
    }

    #[test]
    fn pq_search_returns_k_results() {
        let vectors = random_vectors(200, 384, 42);
        let pq = ProductQuantizer::train(&vectors, &PqConfig::default());
        let results = pq.search(&vectors[0], 5);
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn pq_search_results_sorted_by_distance() {
        let vectors = random_vectors(200, 384, 42);
        let pq = ProductQuantizer::train(&vectors, &PqConfig::default());
        let results = pq.search(&vectors[0], 10);
        for w in results.windows(2) {
            assert!(
                w[0].1 <= w[1].1,
                "results not sorted: {} > {}",
                w[0].1,
                w[1].1
            );
        }
    }

    #[test]
    fn pq_compression_ratio_calculation() {
        let vectors = random_vectors(500, 384, 42);
        let pq = ProductQuantizer::train(&vectors, &PqConfig::default());
        // 500 vectors * 48 subspaces * 1 byte = 24,000 bytes
        assert_eq!(pq.encoded_memory_bytes(), 500 * 48);
        // Raw: 500 * 384 * 4 = 768,000 bytes
        // Ratio: 768,000 / 24,000 = 32x
        let ratio = (500 * 384 * 4) as f64 / pq.encoded_memory_bytes() as f64;
        assert!((ratio - 32.0).abs() < 0.1);
    }
}
