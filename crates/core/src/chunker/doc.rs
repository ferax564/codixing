//! Document-aware chunker: header-based splitting with merge pass.

use crate::config::ChunkConfig;
use crate::language::Language;
use crate::language::doc::DocSection;

use super::{Chunk, chunk_id, non_ws_chars};

fn clamp_to_char_boundary(s: &str, offset: usize) -> usize {
    if offset >= s.len() {
        return s.len();
    }

    let mut pos = offset;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

/// Chunk a parsed document using its section tree.
///
/// Algorithm:
/// 1. Each section becomes a candidate chunk (with scope_chain = section_path).
/// 2. Oversized sections are split at paragraph boundaries (`\n\n`).
/// 3. Adjacent undersized sections at the same level are merged.
pub fn chunk_doc(
    file_path: &str,
    source: &[u8],
    sections: &[DocSection],
    language: Language,
    config: &ChunkConfig,
) -> Vec<Chunk> {
    // If source is empty, return nothing.
    if source.is_empty() {
        return Vec::new();
    }

    // If no sections, return a single whole-file chunk with no scope context.
    if sections.is_empty() {
        let content = String::from_utf8_lossy(source).into_owned();
        return vec![Chunk {
            id: chunk_id(file_path, 0, source.len()),
            file_path: file_path.to_string(),
            language,
            content,
            byte_start: 0,
            byte_end: source.len(),
            line_start: 0,
            line_end: source.iter().filter(|&&b| b == b'\n').count(),
            scope_chain: Vec::new(),
            signatures: Vec::new(),
            entity_names: Vec::new(),
            doc_comments: String::new(),
        }];
    }

    // Phase 1 — Section split: each DocSection becomes one or more candidate chunks.
    struct RawChunk {
        content: String,
        byte_start: usize,
        byte_end: usize,
        line_start: usize,
        line_end: usize,
        scope_chain: Vec<String>,
    }

    let mut raw: Vec<RawChunk> = Vec::new();

    for section in sections {
        let content = &section.content;
        let byte_start = section.byte_range.start;

        if non_ws_chars(content.as_bytes()) <= config.max_chars {
            // Section fits — emit as a single candidate.
            raw.push(RawChunk {
                content: content.clone(),
                byte_start,
                byte_end: section.byte_range.end,
                line_start: section.line_range.start,
                line_end: section.line_range.end,
                scope_chain: section.section_path.clone(),
            });
        } else {
            // Section is oversized — split at paragraph boundaries.
            let paragraphs: Vec<&str> = content.split("\n\n").collect();
            let mut acc = String::new();
            let mut acc_byte_start = byte_start;
            // Track byte offset within the section content.
            let mut offset_in_content: usize = 0;

            for (i, para) in paragraphs.iter().enumerate() {
                let para_bytes = para.len();
                let separator = if i + 1 < paragraphs.len() { 2 } else { 0 }; // "\n\n"

                let para_nws = non_ws_chars(para.as_bytes());
                let combined_nws = non_ws_chars(acc.as_bytes()) + para_nws;

                if !acc.is_empty() && combined_nws > config.max_chars {
                    // Flush accumulator.
                    let acc_byte_end = byte_start + offset_in_content;
                    let acc_prefix_start = clamp_to_char_boundary(
                        content,
                        offset_in_content.saturating_sub(acc.len()),
                    );
                    let acc_line_start = section.line_range.start
                        + content[..acc_prefix_start]
                            .chars()
                            .filter(|&c| c == '\n')
                            .count();
                    let acc_line_end = acc_line_start + acc.chars().filter(|&c| c == '\n').count();
                    raw.push(RawChunk {
                        content: acc.clone(),
                        byte_start: acc_byte_start,
                        byte_end: acc_byte_end,
                        line_start: acc_line_start,
                        line_end: acc_line_end,
                        scope_chain: section.section_path.clone(),
                    });
                    acc_byte_start = acc_byte_end;
                    acc.clear();
                }

                if !acc.is_empty() {
                    acc.push_str("\n\n");
                }
                acc.push_str(para);
                offset_in_content += para_bytes + separator;
            }

            // Flush remaining.
            if !acc.is_empty() {
                let acc_byte_end = section.byte_range.end;
                let acc_prefix_start =
                    clamp_to_char_boundary(content, content.len().saturating_sub(acc.len()));
                let acc_line_start = section.line_range.start
                    + content[..acc_prefix_start]
                        .chars()
                        .filter(|&c| c == '\n')
                        .count();
                let acc_line_end = section.line_range.end;
                raw.push(RawChunk {
                    content: acc,
                    byte_start: acc_byte_start,
                    byte_end: acc_byte_end,
                    line_start: acc_line_start,
                    line_end: acc_line_end,
                    scope_chain: section.section_path.clone(),
                });
            }
        }
    }

    if raw.is_empty() {
        return Vec::new();
    }

    // Phase 2 — Merge pass: adjacent undersized chunks are merged if combined <= max_chars.
    let mut merged: Vec<RawChunk> = Vec::new();

    for chunk in raw {
        let chunk_nws = non_ws_chars(chunk.content.as_bytes());

        if let Some(last) = merged.last_mut() {
            let last_nws = non_ws_chars(last.content.as_bytes());

            if last_nws < config.min_chars
                && chunk_nws < config.min_chars
                && last_nws + chunk_nws <= config.max_chars
            {
                // Merge: append content and extend ranges.
                last.content.push_str("\n\n");
                last.content.push_str(&chunk.content);
                last.byte_end = chunk.byte_end;
                last.line_end = chunk.line_end;
                // Keep the deeper (longer) section path.
                if chunk.scope_chain.len() > last.scope_chain.len() {
                    last.scope_chain = chunk.scope_chain;
                }
                continue;
            }
        }

        merged.push(chunk);
    }

    // Phase 3 — Convert to Chunk structs.
    merged
        .into_iter()
        .map(|rc| Chunk {
            id: chunk_id(file_path, rc.byte_start, rc.byte_end),
            file_path: file_path.to_string(),
            language,
            content: rc.content,
            byte_start: rc.byte_start,
            byte_end: rc.byte_end,
            line_start: rc.line_start,
            line_end: rc.line_end,
            scope_chain: rc.scope_chain,
            signatures: Vec::new(),
            entity_names: Vec::new(),
            doc_comments: String::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::doc::{DocElement, DocSection};

    fn default_config() -> ChunkConfig {
        ChunkConfig {
            max_chars: 200,
            min_chars: 50,
            overlap_ratio: 0.0,
        }
    }

    #[test]
    fn single_small_doc_is_one_chunk() {
        let source = b"# Hello\n\nSmall doc.\n";
        let sections = vec![DocSection {
            heading: "Hello".to_string(),
            level: 1,
            section_path: vec!["Hello".to_string()],
            content: "# Hello\n\nSmall doc.\n".to_string(),
            byte_range: 0..20,
            line_range: 0..3,
            element_types: vec![DocElement::Paragraph],
        }];
        let chunks = chunk_doc(
            "README.md",
            source,
            &sections,
            Language::Markdown,
            &default_config(),
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].scope_chain, vec!["Hello"]);
    }

    #[test]
    fn large_section_splits_at_paragraphs() {
        let para = "A".repeat(150);
        let content = format!("# Big\n\n{para}\n\n{para}\n");
        let source = content.as_bytes();
        let sections = vec![DocSection {
            heading: "Big".to_string(),
            level: 1,
            section_path: vec!["Big".to_string()],
            content: content.clone(),
            byte_range: 0..source.len(),
            line_range: 0..5,
            element_types: vec![DocElement::Paragraph],
        }];
        let chunks = chunk_doc(
            "doc.md",
            source,
            &sections,
            Language::Markdown,
            &default_config(),
        );
        assert!(
            chunks.len() >= 2,
            "Expected split, got {} chunks",
            chunks.len()
        );
    }

    #[test]
    fn adjacent_small_sections_merged() {
        let sections = vec![
            DocSection {
                heading: "A".to_string(),
                level: 2,
                section_path: vec!["Root".to_string(), "A".to_string()],
                content: "## A\n\nTiny.".to_string(),
                byte_range: 0..11,
                line_range: 0..3,
                element_types: vec![DocElement::Paragraph],
            },
            DocSection {
                heading: "B".to_string(),
                level: 2,
                section_path: vec!["Root".to_string(), "B".to_string()],
                content: "## B\n\nAlso tiny.".to_string(),
                byte_range: 11..27,
                line_range: 3..6,
                element_types: vec![DocElement::Paragraph],
            },
        ];
        let source = b"## A\n\nTiny.\n## B\n\nAlso tiny.";
        let chunks = chunk_doc(
            "doc.md",
            source,
            &sections,
            Language::Markdown,
            &default_config(),
        );
        // Both sections are under min_chars (50), should be merged.
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn scope_chain_carries_section_path() {
        let sections = vec![DocSection {
            heading: "Install".to_string(),
            level: 2,
            section_path: vec!["Guide".to_string(), "Install".to_string()],
            content: "## Install\n\nRun the script.".to_string(),
            byte_range: 0..27,
            line_range: 0..3,
            element_types: vec![DocElement::Paragraph],
        }];
        let source = b"## Install\n\nRun the script.";
        let chunks = chunk_doc(
            "guide.md",
            source,
            &sections,
            Language::Markdown,
            &default_config(),
        );
        assert_eq!(chunks[0].scope_chain, vec!["Guide", "Install"]);
    }

    #[test]
    fn large_section_with_multibyte_text_splits_without_panicking() {
        let content = "# Cafes\n\nCafe et the.\n\nEmoji 😀 section.\n".to_string();
        let sections = vec![DocSection {
            heading: "Cafes".to_string(),
            level: 1,
            section_path: vec!["Cafes".to_string()],
            content: content.clone(),
            byte_range: 0..content.len(),
            line_range: 0..5,
            element_types: vec![DocElement::Paragraph],
        }];
        let config = ChunkConfig {
            max_chars: 10,
            min_chars: 1,
            overlap_ratio: 0.0,
        };

        let chunks = chunk_doc(
            "doc.md",
            content.as_bytes(),
            &sections,
            Language::Markdown,
            &config,
        );

        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.content.contains("Emoji 😀"))
        );
    }
}
