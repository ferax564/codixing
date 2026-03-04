use crate::retriever::vector::cosine_similarity;

use super::SearchResult;

/// Select a diverse subset of results using Maximal Marginal Relevance (MMR).
///
/// MMR iteratively selects documents that maximise:
/// ```text
/// λ · sim(query, d) − (1 − λ) · max_{s ∈ S} sim(d, s)
/// ```
/// where `S` is the set of already-selected documents and `λ ∈ [0,1]`
/// controls the relevance/diversity trade-off (1.0 = pure relevance).
///
/// # Parameters
///
/// * `results`      — candidate results, each carrying a pre-embedded vector
/// * `query_vec`    — embedding of the search query
/// * `embeddings`   — per-result embedding vectors (same order as `results`)
/// * `lambda`       — diversity weight (`0.7` is a good default)
/// * `k`            — number of results to select
///
/// # Returns
///
/// Up to `k` results in MMR-ranked order.
pub fn mmr_select(
    results: Vec<SearchResult>,
    query_vec: &[f32],
    embeddings: &[Vec<f32>],
    lambda: f32,
    k: usize,
) -> Vec<SearchResult> {
    if results.is_empty() || k == 0 {
        return Vec::new();
    }

    let n = results.len().min(embeddings.len());
    let mut remaining: Vec<usize> = (0..n).collect();
    let mut selected: Vec<usize> = Vec::with_capacity(k);

    while selected.len() < k && !remaining.is_empty() {
        let best = remaining.iter().copied().max_by(|&i, &j| {
            let mmr_i = mmr_score(i, query_vec, embeddings, &selected, lambda);
            let mmr_j = mmr_score(j, query_vec, embeddings, &selected, lambda);
            mmr_i
                .partial_cmp(&mmr_j)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some(idx) = best {
            selected.push(idx);
            remaining.retain(|&x| x != idx);
        } else {
            break;
        }
    }

    selected
        .into_iter()
        .filter_map(|i| results.get(i).cloned())
        .collect()
}

/// Compute the MMR score for candidate `i`.
fn mmr_score(
    i: usize,
    query_vec: &[f32],
    embeddings: &[Vec<f32>],
    selected: &[usize],
    lambda: f32,
) -> f32 {
    let relevance = cosine_similarity(query_vec, &embeddings[i]);

    let max_sim = selected
        .iter()
        .map(|&s| cosine_similarity(&embeddings[i], &embeddings[s]))
        .fold(0.0f32, f32::max);

    lambda * relevance - (1.0 - lambda) * max_sim
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(id: &str) -> SearchResult {
        SearchResult {
            chunk_id: id.to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "Rust".to_string(),
            score: 1.0,
            line_start: 0,
            line_end: 5,
            signature: String::new(),
            scope_chain: Vec::new(),
            content: format!("content {id}"),
        }
    }

    fn unit(dims: usize, i: usize) -> Vec<f32> {
        let mut v = vec![0.01f32; dims];
        v[i % dims] = 1.0;
        v
    }

    #[test]
    fn mmr_selects_k_results() {
        let results = vec![
            make_result("a"),
            make_result("b"),
            make_result("c"),
            make_result("d"),
        ];
        let query = unit(4, 0);
        let embeddings = vec![unit(4, 0), unit(4, 1), unit(4, 2), unit(4, 3)];
        let selected = mmr_select(results, &query, &embeddings, 0.7, 2);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn mmr_prefers_diverse_results() {
        // Scenario: three candidates for a query q = [1, 0, 0].
        //   a     = [0.9, 0.436, 0]  — most relevant to q (cos=0.9)
        //   a_dup = [0.88, 0.475, 0] — nearly identical to a (cos(a,a_dup)≈0.999)
        //   b     = [0.6, -0.8, 0]   — less relevant than a (cos=0.6) but diverse (cos(a,b)≈0.19)
        //
        // Round 1 (no selected yet, λ=0.7):
        //   MMR(a)=0.7×0.9=0.630, MMR(a_dup)=0.7×0.88=0.616, MMR(b)=0.7×0.6=0.420 → a wins
        //
        // Round 2 (a selected):
        //   MMR(a_dup)=0.7×0.88 − 0.3×0.999=0.616−0.300=0.316
        //   MMR(b)   =0.7×0.60 − 0.3×0.191=0.420−0.057=0.363 → b wins (diversity premium)
        let results = vec![make_result("a"), make_result("a_dup"), make_result("b")];
        let query = vec![1.0f32, 0.0, 0.0];
        let embed_a = vec![0.9f32, 0.436, 0.0];
        let embed_a_dup = vec![0.88f32, 0.475, 0.0];
        let embed_b = vec![0.6f32, -0.8, 0.0];

        let embeddings = vec![embed_a, embed_a_dup, embed_b];
        let selected = mmr_select(results, &query, &embeddings, 0.7, 2);
        assert_eq!(selected.len(), 2);

        // First pick: "a" (highest relevance).
        assert_eq!(
            selected[0].chunk_id, "a",
            "first pick should be most relevant"
        );
        // Second pick: "b" (diverse from "a"), not "a_dup" (near-duplicate of "a").
        assert_eq!(
            selected[1].chunk_id, "b",
            "second pick should be most diverse"
        );
    }

    #[test]
    fn mmr_empty_input() {
        let selected = mmr_select(vec![], &[1.0f32], &[], 0.7, 5);
        assert!(selected.is_empty());
    }
}
