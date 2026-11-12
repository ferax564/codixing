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

        // Look up the symbol definition(s): the exact name first. A substring
        // match is only a fallback, and either way a handful of definitions
        // is enough — on a large repository a substring lookup can return
        // thousands of symbols, each of which used to trigger a test lookup.
        const MAX_DEFINITIONS: usize = 5;
        let mut definitions = self.symbols.lookup(symbol);
        if definitions.is_empty() {
            definitions = self.symbols.filter(symbol, None);
        }
        definitions.retain(|s| !matches!(s.kind, crate::language::EntityKind::Import));
        crate::symbols::sort_for_lookup(&mut definitions, symbol);
        definitions.truncate(MAX_DEFINITIONS);

        // --- Source 1: Test examples ---
        let mut definition_files: Vec<&str> =
            definitions.iter().map(|d| d.file_path.as_str()).collect();
        definition_files.dedup();
        let mut seen_tests = std::collections::HashSet::new();
        for (_, test_mappings) in self.find_tests_for_files(&definition_files) {
            for mapping in &test_mappings {
                if examples.len() >= max || !seen_tests.insert(mapping.test_file.clone()) {
                    continue;
                }
                // Test mappings are persisted index state. Treat them as
                // untrusted on load so a corrupt index cannot escape the
                // configured roots while assembling a read-only MCP result.
                let Some(abs_path) = self.config.resolve_path(&mapping.test_file) else {
                    continue;
                };

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
        // Whole-word occurrences from the trigram index: exact, and far
        // cheaper than re-parsing ranked candidate files.
        let mut callers: Vec<_> = self
            .exact_symbol_references(symbol)
            .into_iter()
            .filter(|r| r.kind == "call")
            .collect();
        callers.sort_by(|a, b| {
            crate::symbols::is_test_like_path(&a.file_path)
                .cmp(&crate::symbols::is_test_like_path(&b.file_path))
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.line.cmp(&b.line))
        });
        callers.truncate(max * 2);
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
