//! Usage example mining: find how symbols are used across the codebase.
//!
//! Three sources of examples, ranked by priority:
//! 1. **Test examples** — from test files that cover the symbol's defining file.
//! 2. **Call site examples** — precise callers found via the symbol graph / AST.
//! 3. **Doc block examples** — code fences extracted from the symbol's own doc comment.

use serde::Serialize;

use super::Engine;

/// Kind of usage example, ordered by value (Test=0 is highest priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(u8)]
pub enum ExampleKind {
    Test = 0,
    CallSite = 1,
    DocBlock = 2,
}

/// A single usage example showing how a symbol is used.
#[derive(Debug, Clone, Serialize)]
pub struct UsageExample {
    /// File where the usage was found.
    pub file_path: String,
    /// First line of the example snippet (0-indexed).
    pub line_start: usize,
    /// Last line of the example snippet (0-indexed).
    pub line_end: usize,
    /// What kind of example this is.
    pub kind: ExampleKind,
    /// The source code snippet.
    pub context: String,
    /// Graph distance from the symbol definition (0 = same file).
    pub distance: usize,
}

/// Extract code blocks from a doc comment string.
///
/// Finds ``` fenced code blocks and returns the content between the fences.
pub fn extract_doc_code_blocks(doc: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut current_block = String::new();

    for line in doc.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_block {
                // End of code block.
                blocks.push(current_block.clone());
                current_block.clear();
                in_block = false;
            } else {
                // Start of code block.
                in_block = true;
            }
        } else if in_block {
            if !current_block.is_empty() {
                current_block.push('\n');
            }
            current_block.push_str(line);
        }
    }

    blocks
}

impl Engine {
    /// Find usage examples for a symbol from tests, call sites, and doc blocks.
    ///
    /// Results are sorted by `(kind, distance, line_start)` so that test
    /// examples appear first, followed by call sites, then doc blocks.
    pub fn find_usage_examples(&self, symbol: &str, max: usize) -> Vec<UsageExample> {
        let mut examples = Vec::new();

        // Look up the symbol definition(s).
        let definitions = self.symbols.filter(symbol, None);

        // --- Source 1: Test examples ---
        for def in &definitions {
            let test_mappings = self.find_tests_for_file(&def.file_path);
            for mapping in &test_mappings {
                let abs_path = self
                    .config
                    .resolve_path(&mapping.test_file)
                    .unwrap_or_else(|| self.config.root.join(&mapping.test_file));

                let source = match std::fs::read_to_string(&abs_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                let lines: Vec<&str> = source.lines().collect();
                // Find lines that mention the symbol.
                for (i, line) in lines.iter().enumerate() {
                    if line.contains(symbol) {
                        let window_start = i.saturating_sub(5);
                        let window_end = (i + 5).min(lines.len().saturating_sub(1));
                        let snippet: String = lines[window_start..=window_end].join("\n");

                        examples.push(UsageExample {
                            file_path: mapping.test_file.clone(),
                            line_start: window_start,
                            line_end: window_end,
                            kind: ExampleKind::Test,
                            context: snippet,
                            distance: 0,
                        });
                        // One example per test file is enough.
                        break;
                    }
                }
            }
        }

        // --- Source 2: Call site examples ---
        let callers = self.symbol_callers_precise(symbol, max * 2);
        for caller in &callers {
            examples.push(UsageExample {
                file_path: caller.file_path.clone(),
                line_start: caller.line,
                line_end: caller.line,
                kind: ExampleKind::CallSite,
                context: caller.context.clone(),
                distance: 1,
            });
        }

        // --- Source 3: Doc block examples ---
        for def in &definitions {
            if let Some(ref doc) = def.doc_comment {
                let blocks = extract_doc_code_blocks(doc);
                for block in blocks {
                    examples.push(UsageExample {
                        file_path: def.file_path.clone(),
                        line_start: def.line_start,
                        line_end: def.line_end,
                        kind: ExampleKind::DocBlock,
                        context: block,
                        distance: 0,
                    });
                }
            }
        }

        // Sort by (kind priority, distance, line_start) and truncate.
        examples.sort_by_key(|e| (e.kind as u8, e.distance, e.line_start));
        examples.truncate(max);
        examples
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_kind_ordering() {
        assert!((ExampleKind::Test as u8) < (ExampleKind::CallSite as u8));
        assert!((ExampleKind::CallSite as u8) < (ExampleKind::DocBlock as u8));
    }

    #[test]
    fn extract_doc_code_blocks_basic() {
        let doc = "Some docs\n```rust\nfn example() {}\n```\nMore text";
        let blocks = extract_doc_code_blocks(doc);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].contains("fn example()"));
    }

    #[test]
    fn extract_doc_code_blocks_empty() {
        let doc = "No code blocks here";
        let blocks = extract_doc_code_blocks(doc);
        assert!(blocks.is_empty());
    }

    #[test]
    fn extract_doc_code_blocks_multiple() {
        let doc = "Header\n```\nblock one\n```\nMiddle\n```python\nblock two\n```\nEnd";
        let blocks = extract_doc_code_blocks(doc);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], "block one");
        assert_eq!(blocks[1], "block two");
    }

    #[test]
    fn sort_examples_test_first() {
        let mut examples = [
            UsageExample {
                file_path: "src/caller.rs".into(),
                line_start: 10,
                line_end: 20,
                kind: ExampleKind::CallSite,
                context: "caller".into(),
                distance: 1,
            },
            UsageExample {
                file_path: "tests/test.rs".into(),
                line_start: 5,
                line_end: 15,
                kind: ExampleKind::Test,
                context: "test".into(),
                distance: 0,
            },
        ];
        examples.sort_by_key(|e| (e.kind as u8, e.distance, e.line_start));
        assert_eq!(examples[0].kind, ExampleKind::Test);
    }
}
