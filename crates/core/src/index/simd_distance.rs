//! SIMD-accelerated distance computation for vector similarity search.
//!
//! Provides three distance functions:
//! - [`cosine_similarity`] — cosine similarity in [-1.0, 1.0]
//! - [`dot_product`] — dot product of two slices
//! - [`l2_distance_squared`] — squared Euclidean distance
//!
//! Dispatch strategy:
//! - **x86_64 with AVX2+FMA**: processes 8 floats/iteration via `_mm256_fmadd_ps`
//! - **aarch64 with NEON**: processes 4 floats/iteration via `vfmaq_f32`
//! - **Scalar fallback**: iterator-based, always available
//!
//! Feature detection is cached in a `LazyLock<bool>` static to avoid repeated
//! `cpuid` queries on every distance call.

#[cfg(target_arch = "x86_64")]
use std::sync::LazyLock;

/// Cached AVX2+FMA feature detection result. Evaluated once on first access.
#[cfg(target_arch = "x86_64")]
static HAS_AVX2_FMA: LazyLock<bool> =
    LazyLock::new(|| is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma"));

/// Scalar (non-SIMD) implementations, always available.
pub mod scalar {
    /// Cosine similarity between two vectors (scalar).
    ///
    /// Returns a value in [-1.0, 1.0]. Returns 0.0 if either vector has zero magnitude.
    ///
    /// # Panics
    /// Panics if `a.len() != b.len()`.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(
            a.len(),
            b.len(),
            "vector length mismatch: {} vs {}",
            a.len(),
            b.len()
        );
        let (dot, norm_a, norm_b) = dot_norm_norm(a, b);
        let denom = norm_a.sqrt() * norm_b.sqrt();
        if denom == 0.0 {
            return 0.0;
        }
        dot / denom
    }

    /// Dot product of two vectors (scalar).
    ///
    /// # Panics
    /// Panics if `a.len() != b.len()`.
    pub fn dot_product(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(
            a.len(),
            b.len(),
            "vector length mismatch: {} vs {}",
            a.len(),
            b.len()
        );
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    /// Squared L2 (Euclidean) distance between two vectors (scalar).
    ///
    /// # Panics
    /// Panics if `a.len() != b.len()`.
    pub fn l2_distance_squared(a: &[f32], b: &[f32]) -> f32 {
        assert_eq!(
            a.len(),
            b.len(),
            "vector length mismatch: {} vs {}",
            a.len(),
            b.len()
        );
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| {
                let d = x - y;
                d * d
            })
            .sum()
    }

    /// Compute (dot_product, norm_a_squared, norm_b_squared) in a single pass.
    #[inline]
    pub(super) fn dot_norm_norm(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
        let mut dot = 0.0f32;
        let mut norm_a = 0.0f32;
        let mut norm_b = 0.0f32;
        for (x, y) in a.iter().zip(b.iter()) {
            dot += x * y;
            norm_a += x * x;
            norm_b += y * y;
        }
        (dot, norm_a, norm_b)
    }
}

// ---------------------------------------------------------------------------
// AVX2 + FMA (x86_64)
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    /// Compute (dot_product, norm_a_sq, norm_b_sq) using AVX2+FMA intrinsics.
    ///
    /// # Safety
    /// Caller must ensure AVX2 and FMA CPU features are available.
    #[target_feature(enable = "avx2,fma")]
    pub(super) unsafe fn dot_norm_norm(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
        let n = a.len();
        let chunks = n / 8;
        let remainder = n % 8;

        unsafe {
            let mut dot_acc = _mm256_setzero_ps();
            let mut norm_a_acc = _mm256_setzero_ps();
            let mut norm_b_acc = _mm256_setzero_ps();

            let a_ptr = a.as_ptr();
            let b_ptr = b.as_ptr();

            for i in 0..chunks {
                let offset = i * 8;
                let va = _mm256_loadu_ps(a_ptr.add(offset));
                let vb = _mm256_loadu_ps(b_ptr.add(offset));

                // dot += a * b (fused multiply-add)
                dot_acc = _mm256_fmadd_ps(va, vb, dot_acc);
                // norm_a += a * a
                norm_a_acc = _mm256_fmadd_ps(va, va, norm_a_acc);
                // norm_b += b * b
                norm_b_acc = _mm256_fmadd_ps(vb, vb, norm_b_acc);
            }

            // Horizontal sum of 256-bit registers
            let dot = hsum256(dot_acc);
            let norm_a = hsum256(norm_a_acc);
            let norm_b = hsum256(norm_b_acc);

            // Handle remainder with scalar
            let tail_start = chunks * 8;
            let (dot_tail, norm_a_tail, norm_b_tail) = super::scalar::dot_norm_norm(
                &a[tail_start..tail_start + remainder],
                &b[tail_start..tail_start + remainder],
            );

            (dot + dot_tail, norm_a + norm_a_tail, norm_b + norm_b_tail)
        }
    }

    /// Horizontal sum of all 8 floats in a __m256.
    ///
    /// # Safety
    /// Caller must ensure AVX2 feature is available.
    /// Horizontal sum of all 8 floats in a `__m256`.
    ///
    /// This is an `unsafe fn` with `#[target_feature(enable = "avx2")]`, which
    /// makes the function body an implicit unsafe context (required for the
    /// SIMD intrinsics). No additional `unsafe` blocks are needed inside.
    #[target_feature(enable = "avx2")]
    unsafe fn hsum256(v: __m256) -> f32 {
        // Extract high 128 and add to low 128
        let high = _mm256_extractf128_ps(v, 1);
        let low = _mm256_castps256_ps128(v);
        let sum128 = _mm_add_ps(high, low);
        // Shuffle and add within 128-bit lane
        let shuf = _mm_movehdup_ps(sum128); // [1,1,3,3]
        let sums = _mm_add_ps(sum128, shuf); // [0+1, _, 2+3, _]
        let shuf2 = _mm_movehl_ps(sums, sums); // [2+3, _, ...]
        let result = _mm_add_ss(sums, shuf2); // [0+1+2+3, ...]
        _mm_cvtss_f32(result)
    }

    /// Squared L2 (Euclidean) distance using AVX2+FMA.
    ///
    /// Computes sum((a[i] - b[i])^2) as FMA: diff*diff + acc.
    ///
    /// # Safety
    /// Caller must ensure AVX2 and FMA CPU features are available.
    #[target_feature(enable = "avx2,fma")]
    pub(super) unsafe fn l2_distance_squared(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let chunks = n / 8;
        let remainder = n % 8;

        unsafe {
            let mut acc = _mm256_setzero_ps();
            let a_ptr = a.as_ptr();
            let b_ptr = b.as_ptr();

            for i in 0..chunks {
                let offset = i * 8;
                let va = _mm256_loadu_ps(a_ptr.add(offset));
                let vb = _mm256_loadu_ps(b_ptr.add(offset));
                let diff = _mm256_sub_ps(va, vb);
                // acc += diff * diff (FMA)
                acc = _mm256_fmadd_ps(diff, diff, acc);
            }

            let mut result = hsum256(acc);

            // Handle remainder with scalar
            let tail_start = chunks * 8;
            for j in 0..remainder {
                let d = a[tail_start + j] - b[tail_start + j];
                result += d * d;
            }

            result
        }
    }

    /// Dot product using AVX2+FMA.
    ///
    /// # Safety
    /// Caller must ensure AVX2 and FMA CPU features are available.
    #[target_feature(enable = "avx2,fma")]
    pub(super) unsafe fn dot_product(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let chunks = n / 8;
        let remainder = n % 8;

        unsafe {
            let mut acc = _mm256_setzero_ps();
            let a_ptr = a.as_ptr();
            let b_ptr = b.as_ptr();

            for i in 0..chunks {
                let offset = i * 8;
                let va = _mm256_loadu_ps(a_ptr.add(offset));
                let vb = _mm256_loadu_ps(b_ptr.add(offset));
                acc = _mm256_fmadd_ps(va, vb, acc);
            }

            let mut result = hsum256(acc);

            // Handle remainder
            let tail_start = chunks * 8;
            for j in 0..remainder {
                result += a[tail_start + j] * b[tail_start + j];
            }

            result
        }
    }
}

// ---------------------------------------------------------------------------
// NEON (aarch64)
// ---------------------------------------------------------------------------

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    /// Compute (dot_product, norm_a_sq, norm_b_sq) using NEON intrinsics.
    ///
    /// # Safety
    /// NEON is always available on aarch64, but intrinsics are still unsafe.
    pub(super) unsafe fn dot_norm_norm(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
        let n = a.len();
        let chunks = n / 4;
        let remainder = n % 4;

        unsafe {
            let mut dot_acc = vdupq_n_f32(0.0);
            let mut norm_a_acc = vdupq_n_f32(0.0);
            let mut norm_b_acc = vdupq_n_f32(0.0);

            let a_ptr = a.as_ptr();
            let b_ptr = b.as_ptr();

            for i in 0..chunks {
                let offset = i * 4;
                let va = vld1q_f32(a_ptr.add(offset));
                let vb = vld1q_f32(b_ptr.add(offset));

                dot_acc = vfmaq_f32(dot_acc, va, vb);
                norm_a_acc = vfmaq_f32(norm_a_acc, va, va);
                norm_b_acc = vfmaq_f32(norm_b_acc, vb, vb);
            }

            let dot = vaddvq_f32(dot_acc);
            let norm_a = vaddvq_f32(norm_a_acc);
            let norm_b = vaddvq_f32(norm_b_acc);

            // Handle remainder with scalar
            let tail_start = chunks * 4;
            let (dot_tail, norm_a_tail, norm_b_tail) = super::scalar::dot_norm_norm(
                &a[tail_start..tail_start + remainder],
                &b[tail_start..tail_start + remainder],
            );

            (dot + dot_tail, norm_a + norm_a_tail, norm_b + norm_b_tail)
        }
    }

    /// Squared L2 (Euclidean) distance using NEON intrinsics.
    ///
    /// # Safety
    /// NEON is always available on aarch64, but intrinsics are still unsafe.
    pub(super) unsafe fn l2_distance_squared(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let chunks = n / 4;
        let remainder = n % 4;

        unsafe {
            let mut acc = vdupq_n_f32(0.0);
            let a_ptr = a.as_ptr();
            let b_ptr = b.as_ptr();

            for i in 0..chunks {
                let offset = i * 4;
                let va = vld1q_f32(a_ptr.add(offset));
                let vb = vld1q_f32(b_ptr.add(offset));
                let diff = vsubq_f32(va, vb);
                acc = vfmaq_f32(acc, diff, diff);
            }

            let mut result = vaddvq_f32(acc);

            let tail_start = chunks * 4;
            for j in 0..remainder {
                let d = a[tail_start + j] - b[tail_start + j];
                result += d * d;
            }

            result
        }
    }

    /// Dot product using NEON intrinsics.
    ///
    /// # Safety
    /// NEON is always available on aarch64, but intrinsics are still unsafe.
    pub(super) unsafe fn dot_product(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len();
        let chunks = n / 4;
        let remainder = n % 4;

        unsafe {
            let mut acc = vdupq_n_f32(0.0);
            let a_ptr = a.as_ptr();
            let b_ptr = b.as_ptr();

            for i in 0..chunks {
                let offset = i * 4;
                let va = vld1q_f32(a_ptr.add(offset));
                let vb = vld1q_f32(b_ptr.add(offset));
                acc = vfmaq_f32(acc, va, vb);
            }

            let mut result = vaddvq_f32(acc);

            let tail_start = chunks * 4;
            for j in 0..remainder {
                result += a[tail_start + j] * b[tail_start + j];
            }

            result
        }
    }
}

// ---------------------------------------------------------------------------
// Public dispatch functions
// ---------------------------------------------------------------------------

/// Cosine similarity between two vectors.
///
/// Returns a value in \[-1.0, 1.0\]. Returns 0.0 if either vector has zero magnitude.
///
/// Uses AVX2+FMA on x86_64 (runtime-detected) or NEON on aarch64, with a scalar
/// fallback on all other platforms.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(
        a.len(),
        b.len(),
        "vector length mismatch: {} vs {}",
        a.len(),
        b.len()
    );

    let (dot, norm_a_sq, norm_b_sq) = dot_norm_norm_dispatch(a, b);
    let denom = norm_a_sq.sqrt() * norm_b_sq.sqrt();
    if denom == 0.0 {
        return 0.0;
    }
    dot / denom
}

/// Dot product of two vectors.
///
/// Uses AVX2+FMA on x86_64 (cached runtime detection) or NEON on aarch64,
/// with a scalar fallback on all other platforms.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
pub fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(
        a.len(),
        b.len(),
        "vector length mismatch: {} vs {}",
        a.len(),
        b.len()
    );

    #[cfg(target_arch = "x86_64")]
    {
        if *HAS_AVX2_FMA {
            // SAFETY: HAS_AVX2_FMA confirmed AVX2+FMA are available.
            return unsafe { avx2::dot_product(a, b) };
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is guaranteed on aarch64.
        return unsafe { neon::dot_product(a, b) };
    }

    #[allow(unreachable_code)]
    scalar::dot_product(a, b)
}

/// Squared L2 (Euclidean) distance between two vectors.
///
/// Uses AVX2+FMA on x86_64 (cached runtime detection) or NEON on aarch64,
/// with a scalar fallback on all other platforms.
///
/// # Panics
/// Panics if `a.len() != b.len()`.
pub fn l2_distance_squared(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(
        a.len(),
        b.len(),
        "vector length mismatch: {} vs {}",
        a.len(),
        b.len()
    );

    #[cfg(target_arch = "x86_64")]
    {
        if *HAS_AVX2_FMA {
            // SAFETY: HAS_AVX2_FMA confirmed AVX2+FMA are available.
            return unsafe { avx2::l2_distance_squared(a, b) };
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is guaranteed on aarch64.
        return unsafe { neon::l2_distance_squared(a, b) };
    }

    #[allow(unreachable_code)]
    scalar::l2_distance_squared(a, b)
}

/// Internal dispatch for `(dot, norm_a_sq, norm_b_sq)` -- the core computation
/// shared by `cosine_similarity` and potentially other callers.
#[inline]
fn dot_norm_norm_dispatch(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    #[cfg(target_arch = "x86_64")]
    {
        if *HAS_AVX2_FMA {
            // SAFETY: HAS_AVX2_FMA confirmed AVX2+FMA are available.
            return unsafe { avx2::dot_norm_norm(a, b) };
        }
    }

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is guaranteed on aarch64.
        return unsafe { neon::dot_norm_norm(a, b) };
    }

    #[allow(unreachable_code)]
    scalar::dot_norm_norm(a, b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_vectors() {
        let a = vec![1.0f32; 384];
        let sim = cosine_similarity(&a, &a);
        assert!(
            (sim - 1.0).abs() < 1e-5,
            "identical vectors should have sim ~1.0, got {sim}"
        );
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let mut a = vec![0.0f32; 384];
        let mut b = vec![0.0f32; 384];
        a[0] = 1.0;
        b[1] = 1.0;
        let sim = cosine_similarity(&a, &b);
        assert!(
            sim.abs() < 1e-5,
            "orthogonal vectors should have sim ~0.0, got {sim}"
        );
    }

    #[test]
    fn cosine_opposite_vectors() {
        let a: Vec<f32> = (0..384).map(|i| i as f32 * 0.01).collect();
        let b: Vec<f32> = a.iter().map(|x| -x).collect();
        let sim = cosine_similarity(&a, &b);
        assert!(
            (sim + 1.0).abs() < 1e-4,
            "opposite vectors should have sim ~-1.0, got {sim}"
        );
    }

    #[test]
    fn cosine_matches_scalar() {
        let a: Vec<f32> = (0..384).map(|i| (i as f32 * 0.137).sin()).collect();
        let b: Vec<f32> = (0..384).map(|i| (i as f32 * 0.251).cos()).collect();
        let scalar_result = scalar::cosine_similarity(&a, &b);
        let simd_result = cosine_similarity(&a, &b);
        assert!(
            (scalar_result - simd_result).abs() < 1e-4,
            "SIMD {simd_result} != scalar {scalar_result}"
        );
    }

    #[test]
    fn dot_product_basic() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        let dot = dot_product(&a, &b);
        assert!((dot - 32.0).abs() < 1e-5);
    }

    #[test]
    fn l2_distance_zero_for_identical() {
        let a: Vec<f32> = (0..384).map(|i| i as f32).collect();
        let dist = l2_distance_squared(&a, &a);
        assert!(dist.abs() < 1e-5);
    }

    #[test]
    fn l2_distance_matches_scalar() {
        let a: Vec<f32> = (0..384).map(|i| (i as f32 * 0.137).sin()).collect();
        let b: Vec<f32> = (0..384).map(|i| (i as f32 * 0.251).cos()).collect();
        let scalar_result = scalar::l2_distance_squared(&a, &b);
        let simd_result = l2_distance_squared(&a, &b);
        assert!(
            (scalar_result - simd_result).abs() < 1e-3,
            "SIMD L2 {simd_result} != scalar L2 {scalar_result}"
        );
    }

    #[test]
    fn l2_distance_non_power_of_8_length() {
        let a: Vec<f32> = (0..100).map(|i| (i as f32 * 0.1).sin()).collect();
        let b: Vec<f32> = (0..100).map(|i| (i as f32 * 0.2).cos()).collect();
        let scalar_result = scalar::l2_distance_squared(&a, &b);
        let simd_result = l2_distance_squared(&a, &b);
        assert!(
            (scalar_result - simd_result).abs() < 1e-3,
            "SIMD L2 {simd_result} != scalar L2 {scalar_result} for 100-dim"
        );
    }

    #[test]
    fn l2_distance_basic() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        // (4-1)^2 + (5-2)^2 + (6-3)^2 = 9 + 9 + 9 = 27
        let dist = l2_distance_squared(&a, &b);
        assert!((dist - 27.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_zero_vector_returns_zero() {
        let a = vec![0.0f32; 384];
        let b: Vec<f32> = (0..384).map(|i| i as f32).collect();
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    #[should_panic]
    fn cosine_mismatched_lengths_panics() {
        let a = vec![1.0f32; 384];
        let b = vec![1.0f32; 256];
        cosine_similarity(&a, &b);
    }

    #[test]
    fn cosine_non_power_of_8_length() {
        let a: Vec<f32> = (0..100).map(|i| (i as f32 * 0.1).sin()).collect();
        let b: Vec<f32> = (0..100).map(|i| (i as f32 * 0.2).cos()).collect();
        let scalar_result = scalar::cosine_similarity(&a, &b);
        let simd_result = cosine_similarity(&a, &b);
        assert!((scalar_result - simd_result).abs() < 1e-4);
    }

    #[test]
    fn throughput_1m_distances() {
        // Note: cargo test runs in debug mode, so we use fewer iterations
        // and a generous threshold. Real benchmarks should use `cargo bench`.
        let a = vec![0.1f32; 384];
        let b = vec![0.2f32; 384];
        let iterations = if cfg!(debug_assertions) {
            10_000
        } else {
            1_000_000
        };
        let max_ms = if cfg!(debug_assertions) { 5_000 } else { 500 };
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(cosine_similarity(
                std::hint::black_box(&a),
                std::hint::black_box(&b),
            ));
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < max_ms,
            "{iterations} distance computations took {elapsed:?}, expected < {max_ms}ms"
        );
    }
}
