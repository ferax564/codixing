//! Token counting and budget management for context retrieval.
//!
//! Provides a [`TokenCounter`] trait for pluggable token estimation strategies
//! and a [`ContextBudget`] manager that greedily packs ranked snippets into a
//! caller-supplied token budget.

/// Token counter interface.
pub trait TokenCounter: Send + Sync {
    /// Count the number of tokens in the given text.
    fn count_tokens(&self, text: &str) -> usize;
}

/// Approximate token counter using character-based estimation.
///
/// Uses the industry-standard heuristic of ~4 characters per token.
/// This is a reasonable default when a model-specific tokenizer is unavailable.
pub struct ApproxTokenCounter {
    chars_per_token: f32,
}

impl Default for ApproxTokenCounter {
    fn default() -> Self {
        Self {
            chars_per_token: 4.0,
        }
    }
}

impl ApproxTokenCounter {
    /// Create a counter with a custom characters-per-token ratio.
    pub fn new(chars_per_token: f32) -> Self {
        Self { chars_per_token }
    }
}

impl TokenCounter for ApproxTokenCounter {
    fn count_tokens(&self, text: &str) -> usize {
        (text.len() as f32 / self.chars_per_token).ceil() as usize
    }
}

/// A single snippet collected by [`ContextBudget`].
pub struct ContextSnippet {
    /// Path to the source file.
    pub file_path: String,
    /// Programming language name.
    pub language: String,
    /// The source code content.
    pub content: String,
    /// Start line (0-indexed).
    pub line_start: u64,
    /// End line (0-indexed, exclusive).
    pub line_end: u64,
    /// Relevance score from the retriever.
    pub score: f32,
    /// Number of tokens consumed by this snippet.
    pub token_count: usize,
}

/// Greedy budget manager that accumulates snippets within a token limit.
///
/// Snippets should be offered in ranked order (highest score first). Each call
/// to [`try_add`](ContextBudget::try_add) either accepts the snippet (returning
/// `true`) or rejects it when the remaining budget is insufficient.
pub struct ContextBudget {
    max_tokens: usize,
    used_tokens: usize,
    counter: Box<dyn TokenCounter>,
    snippets: Vec<ContextSnippet>,
}

impl ContextBudget {
    /// Create a budget with the default [`ApproxTokenCounter`].
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            used_tokens: 0,
            counter: Box::new(ApproxTokenCounter::default()),
            snippets: Vec::new(),
        }
    }

    /// Create a budget with a custom [`TokenCounter`] implementation.
    pub fn with_counter(max_tokens: usize, counter: Box<dyn TokenCounter>) -> Self {
        Self {
            max_tokens,
            used_tokens: 0,
            counter,
            snippets: Vec::new(),
        }
    }

    /// Try to add a snippet. Returns `true` if it fits within budget, `false` otherwise.
    pub fn try_add(
        &mut self,
        file_path: String,
        language: String,
        content: String,
        line_start: u64,
        line_end: u64,
        score: f32,
    ) -> bool {
        let token_count = self.counter.count_tokens(&content);
        if self.used_tokens + token_count > self.max_tokens {
            return false;
        }
        self.used_tokens += token_count;
        self.snippets.push(ContextSnippet {
            file_path,
            language,
            content,
            line_start,
            line_end,
            score,
            token_count,
        });
        true
    }

    /// Get remaining token budget.
    pub fn remaining(&self) -> usize {
        self.max_tokens.saturating_sub(self.used_tokens)
    }

    /// Get total tokens used.
    pub fn used(&self) -> usize {
        self.used_tokens
    }

    /// Consume and return all collected snippets.
    pub fn into_snippets(self) -> Vec<ContextSnippet> {
        self.snippets
    }

    /// Reference to collected snippets.
    pub fn snippets(&self) -> &[ContextSnippet] {
        &self.snippets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approx_counter_basic() {
        let counter = ApproxTokenCounter::default();
        // "hello world" = 11 chars / 4.0 = 2.75 → ceil = 3 tokens
        assert_eq!(counter.count_tokens("hello world"), 3);
    }

    #[test]
    fn approx_counter_empty() {
        let counter = ApproxTokenCounter::default();
        assert_eq!(counter.count_tokens(""), 0);
    }

    #[test]
    fn approx_counter_custom_ratio() {
        let counter = ApproxTokenCounter::new(2.0);
        // "abcdef" = 6 chars / 2.0 = 3.0 tokens
        assert_eq!(counter.count_tokens("abcdef"), 3);
    }

    #[test]
    fn approx_counter_unicode() {
        let counter = ApproxTokenCounter::default();
        // Uses byte length, not char count. "cafe\u{0301}" = 6 bytes / 4.0 = 1.5 → ceil = 2
        let text = "caf\u{00e9}"; // "cafe" with e-acute = 5 bytes in UTF-8
        assert_eq!(counter.count_tokens(text), 2);
    }

    #[test]
    fn budget_respects_limit() {
        let mut budget = ContextBudget::new(10); // 10 tokens
        // 40 chars / 4.0 = 10 tokens — exactly fits
        let text = "a".repeat(40);
        assert!(budget.try_add("f.rs".into(), "rust".into(), text, 1, 5, 1.0));
        // Another 40 chars should NOT fit (budget exhausted)
        let text2 = "b".repeat(40);
        assert!(!budget.try_add("g.rs".into(), "rust".into(), text2, 1, 5, 0.5));
    }

    #[test]
    fn budget_rejects_single_oversize_snippet() {
        let mut budget = ContextBudget::new(5); // 5 tokens = 20 chars
        let text = "x".repeat(21); // 21 chars / 4 = 5.25 → 6 tokens > 5
        assert!(!budget.try_add("big.rs".into(), "rust".into(), text, 1, 1, 1.0));
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.remaining(), 5);
    }

    #[test]
    fn budget_tracks_remaining() {
        let mut budget = ContextBudget::new(100);
        budget.try_add(
            "f.rs".into(),
            "rust".into(),
            "hello world".into(),
            1,
            1,
            1.0,
        );
        // "hello world" = 11 bytes / 4.0 = 2.75 → 3 tokens
        assert_eq!(budget.used(), 3);
        assert_eq!(budget.remaining(), 97);
    }

    #[test]
    fn budget_collects_snippets() {
        let mut budget = ContextBudget::new(1000);
        budget.try_add(
            "a.rs".into(),
            "rust".into(),
            "fn main() {}".into(),
            1,
            1,
            2.0,
        );
        budget.try_add(
            "b.rs".into(),
            "rust".into(),
            "fn foo() {}".into(),
            1,
            1,
            1.0,
        );
        let snippets = budget.into_snippets();
        assert_eq!(snippets.len(), 2);
        assert_eq!(snippets[0].file_path, "a.rs");
        assert_eq!(snippets[0].score, 2.0);
        assert_eq!(snippets[1].file_path, "b.rs");
        assert_eq!(snippets[1].score, 1.0);
    }

    #[test]
    fn budget_snippet_token_counts_are_recorded() {
        let mut budget = ContextBudget::new(1000);
        budget.try_add(
            "c.rs".into(),
            "rust".into(),
            "hello world".into(),
            1,
            1,
            1.0,
        );
        assert_eq!(budget.snippets()[0].token_count, 3);
    }

    #[test]
    fn budget_with_custom_counter() {
        struct FixedCounter;
        impl TokenCounter for FixedCounter {
            fn count_tokens(&self, _text: &str) -> usize {
                5 // every snippet costs exactly 5 tokens
            }
        }

        let mut budget = ContextBudget::with_counter(12, Box::new(FixedCounter));
        assert!(budget.try_add("a.rs".into(), "rust".into(), "x".into(), 1, 1, 1.0));
        assert!(budget.try_add("b.rs".into(), "rust".into(), "y".into(), 2, 2, 0.9));
        // 10 used, 2 remaining — next 5-token snippet won't fit
        assert!(!budget.try_add("c.rs".into(), "rust".into(), "z".into(), 3, 3, 0.8));
        assert_eq!(budget.used(), 10);
        assert_eq!(budget.remaining(), 2);
        assert_eq!(budget.snippets().len(), 2);
    }
}
