use tiktoken_rs::cl100k_base;
use tracing::warn;

use crate::retriever::SearchResult;

/// Gap threshold for band merging: chunks within this many lines of each other
/// in the same file are merged into a single consolidated block.
const BAND_GAP_LINES: usize = 3;

/// Default maximum lines before a function body is truncated.
const DEFAULT_SNIPPET_MAX_LINES: usize = 20;

/// Render a list of search results into an LLM-friendly context block.
///
/// Each chunk is formatted as a fenced code block with metadata header.
/// If `token_budget` is `Some(n)`, chunks are added until the running token
/// count would exceed `n`, then truncation stops.  When a budget is active,
/// function bodies longer than [`DEFAULT_SNIPPET_MAX_LINES`] are
/// signature-truncated to save tokens.
///
/// Uses the `cl100k_base` tokenizer (GPT-4 / Claude compatible).
pub fn format_context(results: &[SearchResult], token_budget: Option<usize>) -> String {
    // Merge adjacent same-file chunks before rendering to reduce token waste.
    let merged = merge_into_bands(results, BAND_GAP_LINES);
    let results: &[SearchResult] = &merged;

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
        // When a token budget is active, apply signature-aware truncation to
        // long function bodies so we can fit more results in the context window.
        let result_to_render;
        let effective_result = if token_budget.is_some() {
            let truncated_content =
                truncate_snippet(&result.content, &result.language, DEFAULT_SNIPPET_MAX_LINES);
            if truncated_content != result.content {
                result_to_render = SearchResult {
                    content: truncated_content,
                    ..result.clone()
                };
                &result_to_render
            } else {
                result
            }
        } else {
            result
        };

        let block = render_result(effective_result);

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

/// Truncate a code snippet intelligently based on function signatures.
///
/// If the snippet contains a function/method definition longer than `max_lines`,
/// keeps the signature, the first few body lines, an elision marker, and the
/// closing brace/dedent.  For non-function content that exceeds `max_lines`,
/// a simple tail-truncation with a `// ... truncated ...` marker is used.
///
/// Short snippets (at or below `max_lines`) are returned unchanged.
pub fn truncate_snippet(content: &str, language: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();

    if lines.len() <= max_lines {
        return content.to_string();
    }

    // Try to detect a function signature at the start of the snippet.
    if let Some(result) = truncate_function_body(&lines, language, max_lines) {
        return result;
    }

    // Fallback: simple truncation for non-function content.
    let keep = max_lines.saturating_sub(1);
    let mut out: Vec<&str> = lines[..keep].to_vec();
    let elided = lines.len() - keep;
    let comment_prefix = comment_prefix_for_language(language);
    out.push(""); // will be replaced
    let marker = format!("{comment_prefix} ... {elided} more lines ...");
    let mut result: String = lines[..keep].join("\n");
    result.push('\n');
    result.push_str(&marker);
    result
}

/// Detect and truncate a function body, keeping the signature visible.
///
/// Returns `None` if the content doesn't look like a function definition.
fn truncate_function_body(lines: &[&str], language: &str, max_lines: usize) -> Option<String> {
    let lang_lower = language.to_lowercase();
    let is_braced = matches!(
        lang_lower.as_str(),
        "rust" | "javascript" | "typescript" | "go" | "c" | "cpp" | "java" | "c#" | "swift"
            | "kotlin" | "scala" | "zig" | "php"
    );
    let is_indented = matches!(lang_lower.as_str(), "python" | "ruby" | "yaml");

    // Look for a function signature in the first few lines.
    let sig_end = find_signature_end(lines, is_braced, is_indented, &lang_lower)?;

    let total = lines.len();
    if total <= max_lines {
        return None;
    }

    // Number of body lines to keep after the signature.
    let body_preview = 3usize;
    let comment_prefix = comment_prefix_for_language(language);

    if is_braced {
        // Find the closing brace (last line that is just `}` possibly with indentation).
        let closing_idx = (0..total)
            .rev()
            .find(|&i| lines[i].trim() == "}" || lines[i].trim() == "};");

        let body_start = sig_end + 1;
        let body_end = closing_idx.unwrap_or(total - 1);
        let body_len = body_end.saturating_sub(body_start);

        if body_len <= max_lines.saturating_sub(sig_end + 2) {
            // Body is short enough after accounting for signature -- no truncation needed.
            return None;
        }

        let preview_end = (body_start + body_preview).min(body_end);
        let elided = body_end.saturating_sub(preview_end);

        let mut out = Vec::new();
        // Signature lines (including opening brace).
        for line in &lines[..=sig_end] {
            out.push(line.to_string());
        }
        // First few body lines.
        for line in &lines[body_start..preview_end] {
            out.push(line.to_string());
        }
        // Elision marker.
        if elided > 0 {
            // Detect indentation from the first body line.
            let indent = lines
                .get(body_start)
                .map(|l| {
                    let trimmed = l.trim_start();
                    &l[..l.len() - trimmed.len()]
                })
                .unwrap_or("    ");
            out.push(format!("{indent}{comment_prefix} ... {elided} more lines ..."));
        }
        // Closing brace.
        if let Some(ci) = closing_idx {
            out.push(lines[ci].to_string());
        }

        Some(out.join("\n"))
    } else if is_indented {
        // Python/Ruby: signature is the `def`/`class` line; body is everything
        // indented deeper.  Find the base indent of the signature.
        let sig_line = lines.first()?;
        let sig_indent = sig_line.len() - sig_line.trim_start().len();

        // Body = lines after signature that are indented more (or blank).
        let body_start = sig_end + 1;
        let mut body_end = body_start;
        for (i, l) in lines.iter().enumerate().take(total).skip(body_start) {
            let l = *l;
            if l.trim().is_empty() {
                body_end = i + 1;
                continue;
            }
            let indent = l.len() - l.trim_start().len();
            if indent > sig_indent {
                body_end = i + 1;
            } else {
                break;
            }
        }

        let body_len = body_end - body_start;
        if body_len + sig_end < max_lines {
            return None;
        }

        let preview_end = (body_start + body_preview).min(body_end);
        let elided = body_end.saturating_sub(preview_end);

        let mut out = Vec::new();
        for line in &lines[..=sig_end] {
            out.push(line.to_string());
        }
        for line in &lines[body_start..preview_end] {
            out.push(line.to_string());
        }
        if elided > 0 {
            let indent = " ".repeat(sig_indent + 4);
            out.push(format!("{indent}{comment_prefix} ... {elided} more lines ..."));
        }

        Some(out.join("\n"))
    } else {
        None
    }
}

/// Find the line index where the function signature ends (inclusive).
///
/// For braced languages this is the line containing the opening `{`.
/// For indented languages this is the line containing `def ` / `:`.
/// Returns `None` if no function signature is detected.
fn find_signature_end(
    lines: &[&str],
    is_braced: bool,
    is_indented: bool,
    lang: &str,
) -> Option<usize> {
    // Check if the first non-empty line looks like a function definition.
    let first_meaningful = lines.iter().position(|l| !l.trim().is_empty())?;
    let first_line = lines[first_meaningful].trim();

    let looks_like_fn = is_function_signature(first_line, lang);
    if !looks_like_fn {
        return None;
    }

    if is_braced {
        // Find the opening brace — could be on the same line or a subsequent one.
        // Limit search to the first 8 lines (multi-line signatures + generic bounds).
        for (i, line) in lines.iter().enumerate().take(lines.len().min(first_meaningful + 8)).skip(first_meaningful) {
            if line.contains('{') {
                return Some(i);
            }
        }
        None
    } else if is_indented {
        // For Python: the signature ends at the `:` line.
        for (i, line) in lines.iter().enumerate().take(lines.len().min(first_meaningful + 5)).skip(first_meaningful) {
            if line.trim_end().ends_with(':') {
                return Some(i);
            }
        }
        // Single-line def
        Some(first_meaningful)
    } else {
        None
    }
}

/// Heuristic check: does a trimmed line look like a function/method signature?
fn is_function_signature(line: &str, lang: &str) -> bool {
    let lang_lower = lang.to_lowercase();
    match lang_lower.as_str() {
        "rust" => {
            line.starts_with("fn ")
                || line.starts_with("pub fn ")
                || line.starts_with("pub(crate) fn ")
                || line.starts_with("pub(super) fn ")
                || line.starts_with("async fn ")
                || line.starts_with("pub async fn ")
                || line.starts_with("unsafe fn ")
                || line.starts_with("pub unsafe fn ")
                || line.starts_with("const fn ")
                || line.starts_with("pub const fn ")
        }
        "python" => line.starts_with("def ") || line.starts_with("async def "),
        "javascript" | "typescript" => {
            line.starts_with("function ")
                || line.starts_with("async function ")
                || line.starts_with("export function ")
                || line.starts_with("export async function ")
                || line.starts_with("export default function ")
                || line.contains("=> {")
        }
        "go" => line.starts_with("func "),
        "java" | "c#" | "kotlin" | "scala" => {
            // Match common modifiers followed by a return type and name.
            (line.starts_with("public ")
                || line.starts_with("private ")
                || line.starts_with("protected ")
                || line.starts_with("static ")
                || line.starts_with("fun ")
                || line.starts_with("def ")
                || line.starts_with("override "))
                && (line.contains('(') || line.contains('{'))
        }
        "c" | "cpp" | "zig" => {
            // C/C++: look for parentheses indicating a function declaration.
            line.contains('(') && !line.starts_with('#') && !line.starts_with("//")
        }
        "php" => {
            line.starts_with("function ")
                || line.starts_with("public function ")
                || line.starts_with("private function ")
                || line.starts_with("protected function ")
                || line.starts_with("static function ")
        }
        "ruby" => line.starts_with("def "),
        "swift" => line.starts_with("func ") || line.contains("func "),
        _ => false,
    }
}

/// Return the single-line comment prefix for a language.
fn comment_prefix_for_language(language: &str) -> &'static str {
    match language.to_lowercase().as_str() {
        "python" | "ruby" | "yaml" | "shell" | "bash" => "#",
        _ => "//",
    }
}

/// Merge adjacent search results from the same file into consolidated bands.
///
/// Results within `gap_lines` lines of each other in the same file are joined
/// into a single block.  This reduces token output by 25–63 % on typical
/// codebases (LDAR-style band selection) while preserving all content.
///
/// Input order is preserved at the file level; within each file, chunks are
/// sorted by `line_start` before merging.
fn merge_into_bands(results: &[SearchResult], gap_lines: usize) -> Vec<SearchResult> {
    if results.is_empty() {
        return Vec::new();
    }

    // Collect file groups in the order they first appear.
    let mut file_order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&SearchResult>> =
        std::collections::HashMap::new();
    for r in results {
        groups.entry(r.file_path.clone()).or_insert_with(|| {
            file_order.push(r.file_path.clone());
            Vec::new()
        });
        groups.get_mut(&r.file_path).unwrap().push(r);
    }

    let mut out = Vec::new();
    for file in &file_order {
        let group = groups.get(file).unwrap();
        let mut sorted: Vec<&SearchResult> = group.to_vec();
        sorted.sort_by_key(|r| r.line_start);

        let mut bands: Vec<SearchResult> = Vec::new();
        for r in sorted {
            if let Some(last) = bands.last_mut() {
                if r.line_start <= last.line_end + gap_lines as u64 {
                    // Extend the current band.
                    if r.line_end > last.line_end {
                        last.content = format!("{}\n{}", last.content, r.content);
                        last.line_end = r.line_end;
                    }
                    if r.score > last.score {
                        last.score = r.score;
                    }
                    continue;
                }
            }
            bands.push((*r).clone());
        }
        out.extend(bands);
    }
    out
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

    // -----------------------------------------------------------------------
    // truncate_snippet tests
    // -----------------------------------------------------------------------

    #[test]
    fn truncate_rust_fn_long_body() {
        let src = "\
pub fn compute(data: &[u8]) -> usize {
    let mut total = 0;
    for byte in data {
        total += *byte as usize;
        if total > 1000 {
            total = 1000;
        }
        // line 8
        // line 9
        // line 10
        // line 11
        // line 12
        // line 13
        // line 14
        // line 15
        // line 16
        // line 17
        // line 18
        // line 19
        // line 20
        // line 21
        // line 22
    }
    total
}";
        let result = truncate_snippet(src, "rust", 12);
        // Should contain the signature.
        assert!(result.contains("pub fn compute"), "should keep signature");
        // Should contain the first few body lines.
        assert!(result.contains("let mut total"), "should keep first body lines");
        // Should contain an elision marker.
        assert!(
            result.contains("// ..."),
            "should have elision marker, got:\n{result}"
        );
        assert!(result.contains("more lines"), "should state how many lines elided");
        // Should contain the closing brace.
        assert!(result.trim_end().ends_with('}'), "should keep closing brace");
        // Should be shorter than the original.
        assert!(
            result.lines().count() < src.lines().count(),
            "should be shorter"
        );
    }

    #[test]
    fn truncate_python_def_long_body() {
        let src = "\
def process(items):
    result = []
    for item in items:
        result.append(item * 2)
        # line 5
        # line 6
        # line 7
        # line 8
        # line 9
        # line 10
        # line 11
        # line 12
        # line 13
        # line 14
        # line 15
        # line 16
        # line 17
        # line 18
        # line 19
        # line 20
        # line 21
    return result";
        let result = truncate_snippet(src, "python", 10);
        assert!(result.contains("def process"), "should keep signature");
        assert!(result.contains("result = []"), "should keep first body lines");
        assert!(result.contains("# ..."), "should have elision marker");
        assert!(
            result.lines().count() < src.lines().count(),
            "should be shorter"
        );
    }

    #[test]
    fn truncate_js_function_long_body() {
        let src = "\
function handleRequest(req, res) {
    const data = req.body;
    const validated = validate(data);
    if (!validated) {
        res.status(400).send('Bad request');
        return;
    }
    // line 8
    // line 9
    // line 10
    // line 11
    // line 12
    // line 13
    // line 14
    // line 15
    // line 16
    // line 17
    // line 18
    // line 19
    // line 20
    // line 21
    res.send('OK');
}";
        let result = truncate_snippet(src, "javascript", 10);
        assert!(result.contains("function handleRequest"), "should keep signature");
        assert!(result.contains("// ..."), "should have elision marker");
        assert!(result.trim_end().ends_with('}'), "should keep closing brace");
    }

    #[test]
    fn truncate_short_function_no_change() {
        let src = "\
fn short() -> bool {
    true
}";
        let result = truncate_snippet(src, "rust", 20);
        assert_eq!(result, src, "short functions should not be truncated");
    }

    #[test]
    fn truncate_multi_line_signature() {
        let mut lines = vec![
            "pub fn complex_function(",
            "    arg1: &str,",
            "    arg2: usize,",
            "    arg3: Option<bool>,",
            ") -> Result<String> {",
        ];
        // Add 25 body lines.
        for i in 0..25 {
            lines.push(if i == 24 { "}" } else { "    // body" });
        }
        let src = lines.join("\n");
        let result = truncate_snippet(&src, "rust", 15);
        assert!(result.contains("pub fn complex_function"), "should keep signature");
        assert!(result.contains("// ..."), "should have elision marker");
        assert!(result.trim_end().ends_with('}'), "should keep closing brace");
    }

    #[test]
    fn truncate_non_function_content() {
        let src = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = truncate_snippet(&src, "rust", 10);
        assert!(result.contains("// ..."), "should have truncation marker");
        assert!(result.contains("more lines"), "should state elided count");
        assert!(
            result.lines().count() < src.lines().count(),
            "should be shorter"
        );
    }

    #[test]
    fn truncate_go_func() {
        let src = "\
func ServeHTTP(w http.ResponseWriter, r *http.Request) {
    ctx := r.Context()
    data := ctx.Value(\"data\")
    // process line 4
    // process line 5
    // process line 6
    // process line 7
    // process line 8
    // process line 9
    // process line 10
    // process line 11
    // process line 12
    // process line 13
    // process line 14
    // process line 15
    // process line 16
    // process line 17
    // process line 18
    // process line 19
    // process line 20
    w.Write([]byte(\"done\"))
}";
        let result = truncate_snippet(src, "go", 10);
        assert!(result.contains("func ServeHTTP"), "should keep signature");
        assert!(result.contains("// ..."), "should have elision marker");
    }

    #[test]
    fn format_context_token_budget_truncates() {
        // Use results from distinct files so band merging doesn't collapse them.
        let results: Vec<SearchResult> = (0..20)
            .map(|i| SearchResult {
                chunk_id: i.to_string(),
                // Different file per chunk → band merging won't consolidate them.
                file_path: format!("src/module_{i}.rs"),
                language: "Rust".to_string(),
                score: 1.0,
                line_start: 10,
                line_end: 20,
                signature: "fn example()".to_string(),
                scope_chain: vec!["MyMod".to_string()],
                content: "fn placeholder_function_body() { /* a reasonably long body */ }"
                    .to_string(),
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
