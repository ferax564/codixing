use tiktoken_rs::cl100k_base;
use tracing::warn;

use crate::retriever::SearchResult;

/// Render a list of search results into an LLM-friendly context block.
///
/// Each chunk is formatted as a fenced code block with metadata header.
/// If `token_budget` is `Some(n)`, chunks are added until the running token
/// count would exceed `n`, then truncation stops.
///
/// Uses the `cl100k_base` tokenizer (GPT-4 / Claude compatible).
pub fn format_context(results: &[SearchResult], token_budget: Option<usize>) -> String {
    let bpe = match cl100k_base() {
        Ok(b) => Some(b),
        Err(e) => {
            warn!(error = %e, "failed to load cl100k tokenizer; skipping token budget");
            None
        }
    };

    let mut output = String::new();
    let mut token_count: usize = 0;

    for result in results {
        let block = render_result(result);

        if let (Some(bpe), Some(budget)) = (bpe.as_ref(), token_budget) {
            let block_tokens = bpe.encode_with_special_tokens(&block).len();
            if token_count + block_tokens > budget {
                output.push_str(&format!(
                    "\n<!-- truncated: token budget of {budget} reached -->\n"
                ));
                break;
            }
            token_count += block_tokens;
        }

        output.push_str(&block);
    }

    output
}

/// Count the number of cl100k tokens in a string.
///
/// Returns 0 if the tokenizer fails to load (logged as a warning).
pub fn count_tokens(text: &str) -> usize {
    match cl100k_base() {
        Ok(bpe) => bpe.encode_with_special_tokens(text).len(),
        Err(e) => {
            warn!(error = %e, "failed to load cl100k tokenizer");
            0
        }
    }
}

/// Format a single [`SearchResult`] as a markdown code block with metadata.
fn render_result(r: &SearchResult) -> String {
    let scope = if r.scope_chain.is_empty() {
        String::new()
    } else {
        format!(" ({})", r.scope_chain.join(" > "))
    };

    let sig = if r.signature.is_empty() {
        String::new()
    } else {
        format!("\n// {}", r.signature.lines().next().unwrap_or(""))
    };

    format!(
        "// File: {} [L{}-L{}]{}{}\n```{}\n{}\n```\n\n",
        r.file_path,
        r.line_start,
        r.line_end,
        scope,
        sig,
        r.language.to_lowercase(),
        r.content.trim_end(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(id: &str, content: &str) -> SearchResult {
        SearchResult {
            chunk_id: id.to_string(),
            file_path: "src/lib.rs".to_string(),
            language: "Rust".to_string(),
            score: 1.0,
            line_start: 10,
            line_end: 20,
            signature: "fn example()".to_string(),
            scope_chain: vec!["MyMod".to_string()],
            content: content.to_string(),
        }
    }

    #[test]
    fn format_context_includes_file_path() {
        let results = vec![make_result("1", "fn foo() {}")];
        let ctx = format_context(&results, None);
        assert!(ctx.contains("src/lib.rs"));
        assert!(ctx.contains("fn foo()"));
    }

    #[test]
    fn format_context_scope_chain_rendered() {
        let results = vec![make_result("1", "fn bar() {}")];
        let ctx = format_context(&results, None);
        assert!(ctx.contains("MyMod"));
    }

    #[test]
    fn count_tokens_non_zero() {
        let n = count_tokens("hello world this is a test sentence");
        assert!(n > 0, "expected non-zero token count");
    }

    #[test]
    fn format_context_token_budget_truncates() {
        // Create many results; budget should cut off before all are included.
        let results: Vec<SearchResult> = (0..20)
            .map(|i| {
                make_result(
                    &i.to_string(),
                    "fn placeholder_function_body() { /* ... */ }",
                )
            })
            .collect();

        let full = format_context(&results, None);
        let budgeted = format_context(&results, Some(50));

        assert!(
            budgeted.len() < full.len(),
            "token budget should truncate output"
        );
        assert!(budgeted.contains("truncated"));
    }
}
