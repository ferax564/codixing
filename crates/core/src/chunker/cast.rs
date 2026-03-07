//! cAST: Chunking via Abstract Syntax Trees.
//!
//! Recursive split-then-merge algorithm:
//! 1. If the entire file fits in budget → single chunk.
//! 2. Walk top-level named children:
//!    a. If node fits and accumulator has room → accumulate.
//!    b. If adding exceeds budget → flush accumulator, start new.
//!    c. If node alone exceeds budget → recursively decompose.
//! 3. Post-pass: merge adjacent undersized chunks.

use tree_sitter::Node;

use crate::config::ChunkConfig;
use crate::language::Language;

use super::{Chunk, Chunker, chunk_id, non_ws_chars};

/// The cAST chunker: AST-aware, recursive split-then-merge.
pub struct CastChunker;

impl Chunker for CastChunker {
    fn chunk(
        &self,
        file_path: &str,
        source: &[u8],
        tree: &tree_sitter::Tree,
        language: Language,
        config: &ChunkConfig,
    ) -> Vec<Chunk> {
        let root = tree.root_node();

        // If entire file fits in budget, return as single chunk.
        if non_ws_chars(source) <= config.max_chars {
            return vec![make_chunk(
                file_path,
                source,
                language,
                0,
                source.len(),
                &[],
            )];
        }

        // Split phase: recursively decompose the AST into raw spans.
        let mut raw_spans = Vec::new();
        split_node(&root, source, config.max_chars, &[], &mut raw_spans);

        // If split produced nothing (e.g., root has no named children), fallback to whole file.
        if raw_spans.is_empty() {
            return vec![make_chunk(
                file_path,
                source,
                language,
                0,
                source.len(),
                &[],
            )];
        }

        // Merge phase: combine adjacent small spans.
        let merged = merge_spans(&raw_spans, source, config.max_chars, config.min_chars);

        // Build final chunks.
        merged
            .into_iter()
            .map(|span| {
                make_chunk(
                    file_path,
                    source,
                    language,
                    span.byte_start,
                    span.byte_end,
                    &span.scope,
                )
            })
            .collect()
    }
}

/// A raw span produced by the split phase.
#[derive(Debug, Clone)]
struct RawSpan {
    byte_start: usize,
    byte_end: usize,
    scope: Vec<String>,
}

impl RawSpan {
    fn size(&self, source: &[u8]) -> usize {
        non_ws_chars(&source[self.byte_start..self.byte_end])
    }
}

/// Recursively split an AST node into spans that fit within max_chars.
fn split_node(
    node: &Node,
    source: &[u8],
    max_chars: usize,
    scope: &[String],
    spans: &mut Vec<RawSpan>,
) {
    let node_size = non_ws_chars(&source[node.start_byte()..node.end_byte()]);

    // If the node fits, emit it as a single span.
    if node_size <= max_chars {
        spans.push(RawSpan {
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            scope: scope.to_vec(),
        });
        return;
    }

    // Node is too large. Try to decompose into named children.
    let mut cursor = node.walk();
    let children: Vec<Node> = node.named_children(&mut cursor).collect();

    if children.is_empty() {
        // Leaf node exceeds budget — include it as-is (unavoidable).
        spans.push(RawSpan {
            byte_start: node.start_byte(),
            byte_end: node.end_byte(),
            scope: scope.to_vec(),
        });
        return;
    }

    // Build child scope: if this node has a name or type field, add it.
    let child_scope = {
        let name = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("type"))
            .and_then(|n| n.utf8_text(source).ok())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            scope.to_vec()
        } else {
            let mut s = scope.to_vec();
            s.push(name);
            s
        }
    };

    // Accumulate children into groups that fit within max_chars.
    let mut accum: Vec<&Node> = Vec::new();
    let mut accum_size: usize = 0;

    for child in &children {
        let child_size = non_ws_chars(&source[child.start_byte()..child.end_byte()]);

        if child_size > max_chars {
            // Flush accumulator first.
            flush_accum(&accum, source, &child_scope, spans);
            accum.clear();
            accum_size = 0;

            // Recursively decompose the oversized child.
            split_node(child, source, max_chars, &child_scope, spans);
        } else if accum_size + child_size > max_chars {
            // Adding this child would exceed budget. Flush and start new group.
            flush_accum(&accum, source, &child_scope, spans);
            accum.clear();

            accum.push(child);
            accum_size = child_size;
        } else {
            accum.push(child);
            accum_size += child_size;
        }
    }

    // Flush remaining accumulator.
    flush_accum(&accum, source, &child_scope, spans);
}

/// Flush a group of accumulated nodes as a single span.
fn flush_accum(nodes: &[&Node], source: &[u8], scope: &[String], spans: &mut Vec<RawSpan>) {
    if nodes.is_empty() {
        return;
    }

    let byte_start = nodes.first().unwrap().start_byte();
    let byte_end = nodes.last().unwrap().end_byte();

    // Include any text between the accumulated nodes (whitespace, comments, etc.)
    // by spanning from the first node's start to the last node's end.
    let _ = source; // used for range validation
    spans.push(RawSpan {
        byte_start,
        byte_end,
        scope: scope.to_vec(),
    });
}

/// Merge adjacent small spans until they meet the min_chars threshold or would exceed max_chars.
fn merge_spans(
    spans: &[RawSpan],
    source: &[u8],
    max_chars: usize,
    min_chars: usize,
) -> Vec<RawSpan> {
    if spans.is_empty() {
        return Vec::new();
    }

    let mut merged: Vec<RawSpan> = Vec::new();
    let mut current = spans[0].clone();

    for next in &spans[1..] {
        let current_size = current.size(source);
        let next_size = next.size(source);

        // If both are small and combining them fits, merge.
        let combined_size = non_ws_chars(&source[current.byte_start..next.byte_end]);
        if current_size < min_chars && combined_size <= max_chars {
            current.byte_end = next.byte_end;
            // Keep the scope from whichever has more context (longer scope).
            if next.scope.len() > current.scope.len() {
                current.scope = next.scope.clone();
            }
        } else if next_size < min_chars && combined_size <= max_chars {
            current.byte_end = next.byte_end;
            if next.scope.len() > current.scope.len() {
                current.scope = next.scope.clone();
            }
        } else {
            merged.push(current);
            current = next.clone();
        }
    }
    merged.push(current);

    merged
}

/// Build a Chunk from a byte range.
fn make_chunk(
    file_path: &str,
    source: &[u8],
    language: Language,
    byte_start: usize,
    byte_end: usize,
    scope: &[String],
) -> Chunk {
    let content = String::from_utf8_lossy(&source[byte_start..byte_end]).to_string();

    // Compute line range.
    let line_start = source[..byte_start].iter().filter(|&&b| b == b'\n').count();
    let line_end = source[..byte_end].iter().filter(|&&b| b == b'\n').count() + 1;

    // Extract entity names and signatures from the content (simple heuristic).
    let entity_names = extract_entity_names_from_content(&content, language);
    let signatures = extract_signatures_from_content(&content, language);

    Chunk {
        id: chunk_id(file_path, byte_start, byte_end),
        file_path: file_path.to_string(),
        language,
        content,
        byte_start,
        byte_end,
        line_start,
        line_end,
        scope_chain: scope.to_vec(),
        signatures,
        entity_names,
    }
}

/// Simple heuristic: extract entity names from chunk content.
fn extract_entity_names_from_content(content: &str, language: Language) -> Vec<String> {
    let mut names = Vec::new();
    let keyword_patterns: &[&str] = match language {
        Language::Rust => &[
            "fn ", "struct ", "enum ", "trait ", "impl ", "type ", "const ", "mod ",
        ],
        Language::Python => &["def ", "class "],
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            &["function ", "class ", "interface ", "type "]
        }
        Language::Go => &["func ", "type "],
        Language::Java => &["class ", "interface ", "enum "],
        Language::C | Language::Cpp => &["struct ", "enum ", "class ", "namespace "],
        Language::CSharp => &["class ", "interface ", "struct ", "enum ", "namespace "],
        Language::Ruby => &["def ", "class ", "module "],
        Language::Swift => &["func ", "class ", "struct ", "protocol ", "extension "],
        Language::Kotlin => &["fun ", "class ", "object ", "interface "],
        Language::Scala => &["def ", "class ", "object ", "trait ", "val "],
        Language::Zig => &["fn ", "const ", "var ", "pub "],
        Language::Php => &["function ", "class "],
    };

    for line in content.lines() {
        let trimmed = line.trim();
        for pattern in keyword_patterns {
            if let Some(rest) = trimmed.strip_prefix("pub ").or(Some(trimmed)) {
                if let Some(after) = rest.strip_prefix(pattern) {
                    // Extract the identifier (up to first non-alphanumeric-underscore).
                    let name: String = after
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        names.push(name);
                    }
                }
            }
        }
    }
    names
}

/// Simple heuristic: extract signatures from chunk content.
fn extract_signatures_from_content(content: &str, language: Language) -> Vec<String> {
    let sig_starters: &[&str] = match language {
        Language::Rust => &["fn ", "pub fn ", "pub(crate) fn "],
        Language::Python => &["def "],
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            &["function ", "export function "]
        }
        Language::Go => &["func "],
        Language::Java | Language::CSharp => &["public ", "private ", "protected ", "static "],
        Language::C | Language::Cpp => &[],
        Language::Ruby => &["def "],
        Language::Swift => &["func ", "public func ", "private func ", "internal func "],
        Language::Kotlin => &["fun ", "public fun ", "private fun ", "internal fun "],
        Language::Scala => &["def "],
        Language::Zig => &["fn ", "pub fn "],
        Language::Php => &["function ", "public function ", "private function ", "protected function "],
    };

    let mut sigs = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        for starter in sig_starters {
            if trimmed.starts_with(starter) {
                // Take up to opening brace or end of line.
                let sig = if let Some(brace) = trimmed.find('{') {
                    trimmed[..brace].trim()
                } else {
                    trimmed.trim_end_matches(';').trim_end_matches(':')
                };
                if !sig.is_empty() {
                    sigs.push(sig.to_string());
                    break;
                }
            }
        }
    }
    sigs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_rust(source: &str, max_chars: usize, min_chars: usize) -> Vec<Chunk> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source.as_bytes(), None).unwrap();
        let config = ChunkConfig {
            max_chars,
            min_chars,
        };
        let chunker = CastChunker;
        chunker.chunk("test.rs", source.as_bytes(), &tree, Language::Rust, &config)
    }

    #[test]
    fn small_file_single_chunk() {
        let src = "fn hello() { 42 }\n";
        let chunks = chunk_rust(src, 1500, 200);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, src);
    }

    #[test]
    fn no_chunk_exceeds_max_chars() {
        let src = r#"
fn alpha() {
    let x = 1;
    let y = 2;
    let z = x + y;
    println!("{}", z);
}

fn beta() {
    let a = "hello";
    let b = "world";
    println!("{} {}", a, b);
}

fn gamma() {
    for i in 0..100 {
        println!("{}", i);
    }
}

fn delta() {
    let v: Vec<i32> = (0..50).collect();
    let sum: i32 = v.iter().sum();
    println!("{}", sum);
}
"#;
        let max = 150;
        let chunks = chunk_rust(src, max, 30);
        for (i, chunk) in chunks.iter().enumerate() {
            let size = non_ws_chars(chunk.content.as_bytes());
            // Allow oversized only for indivisible leaf nodes.
            if size > max {
                // This should only happen for genuinely atomic nodes.
                panic!(
                    "Chunk {i} exceeds max_chars: {size} > {max}\n{}",
                    chunk.content
                );
            }
        }
    }

    #[test]
    fn every_byte_covered() {
        let src = r#"
fn one() { 1 }
fn two() { 2 }
fn three() { 3 }
fn four() { 4 }
fn five() { 5 }
"#;
        let chunks = chunk_rust(src, 60, 10);

        // Verify all non-whitespace content appears in some chunk.
        let mut covered = vec![false; src.len()];
        for chunk in &chunks {
            for item in covered
                .iter_mut()
                .take(chunk.byte_end)
                .skip(chunk.byte_start)
            {
                *item = true;
            }
        }
        // Every non-whitespace byte should be covered.
        for (i, &is_covered) in covered.iter().enumerate() {
            if !src.as_bytes()[i].is_ascii_whitespace() && !is_covered {
                panic!(
                    "Byte {} ('{}') not covered by any chunk",
                    i,
                    src.as_bytes()[i] as char
                );
            }
        }
    }

    #[test]
    fn functions_not_split() {
        let src = r#"
fn complete_function() {
    let a = 1;
    let b = 2;
    a + b
}

fn another_function() {
    println!("hello");
}
"#;
        let chunks = chunk_rust(src, 300, 50);
        // Each function should be entirely within one chunk.
        for chunk in &chunks {
            let content = &chunk.content;
            let open_braces = content.matches('{').count();
            let close_braces = content.matches('}').count();
            // Balanced braces means no split mid-function.
            assert_eq!(
                open_braces, close_braces,
                "Unbalanced braces in chunk:\n{}",
                content
            );
        }
    }

    #[test]
    fn merge_combines_small_chunks() {
        let src = r#"
const A: i32 = 1;
const B: i32 = 2;
const C: i32 = 3;
const D: i32 = 4;
const E: i32 = 5;
"#;
        // Each const is ~18 chars. With min=50 they should be merged.
        let chunks = chunk_rust(src, 200, 50);
        // Should merge into fewer chunks than 5 consts.
        assert!(
            chunks.len() < 5,
            "Expected merging, got {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn chunk_ids_are_deterministic() {
        let src = "fn foo() {}\nfn bar() {}\n";
        let c1 = chunk_rust(src, 100, 10);
        let c2 = chunk_rust(src, 100, 10);
        assert_eq!(c1.len(), c2.len());
        for (a, b) in c1.iter().zip(c2.iter()) {
            assert_eq!(a.id, b.id);
        }
    }

    #[test]
    fn scope_chain_populated() {
        // A large impl block that will need splitting.
        let src = r#"
impl MyStruct {
    fn method_a() {
        let x = 1;
        let y = 2;
        let z = 3;
        println!("{} {} {}", x, y, z);
    }

    fn method_b() {
        let a = "hello";
        let b = "world";
        println!("{} {}", a, b);
    }

    fn method_c() {
        for i in 0..100 {
            println!("{}", i);
        }
    }
}
"#;
        let chunks = chunk_rust(src, 150, 30);
        // At least one chunk should have scope info.
        let has_scope = chunks.iter().any(|c| !c.scope_chain.is_empty());
        assert!(
            has_scope,
            "Expected at least one chunk with scope_chain populated"
        );
    }

    #[test]
    fn python_chunking_works() {
        let src = r#"
def hello():
    print("hello")

def world():
    print("world")

class Foo:
    def bar(self):
        return 42
"#;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(src.as_bytes(), None).unwrap();
        let config = ChunkConfig {
            max_chars: 100,
            min_chars: 20,
        };
        let chunker = CastChunker;
        let chunks = chunker.chunk("test.py", src.as_bytes(), &tree, Language::Python, &config);
        assert!(!chunks.is_empty());
        // Verify all chunks have correct file path.
        for c in &chunks {
            assert_eq!(c.file_path, "test.py");
            assert_eq!(c.language, Language::Python);
        }
    }
}
