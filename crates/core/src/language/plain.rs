//! Plain-text document parser.
//!
//! Fallback for `.txt` files and bare filenames like `README`, `AUTHORS`,
//! `LICENSE` (no extension). Treats each blank-line-separated paragraph
//! as a `DocSection`, with a soft size cap — paragraphs longer than
//! `MAX_PARA_BYTES` are split into sub-sections on sentence boundaries so
//! retrieval chunks stay focused.

use super::Language;
use super::doc::{
    DocElement, DocLanguageSupport, DocSection, SymbolRef, build_line_offsets, line_byte_offset,
};
use super::markdown::{clean_symbol_name, is_likely_symbol};

/// Soft cap for a plain-text section body.
const MAX_PARA_BYTES: usize = 2_000;

/// Plain-text language support.
pub struct PlainTextLanguage;

impl DocLanguageSupport for PlainTextLanguage {
    fn language(&self) -> Language {
        Language::PlainText
    }

    fn parse_sections(&self, source: &[u8], _file_name: Option<&str>) -> Vec<DocSection> {
        let text = String::from_utf8_lossy(source);
        parse_plain_sections(&text)
    }

    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef> {
        let text = String::from_utf8_lossy(source);
        extract_plain_refs(&text)
    }
}

/// Parse plain text into one `DocSection` per blank-line-separated paragraph.
///
/// Paragraphs longer than `MAX_PARA_BYTES` are split further on sentence
/// boundaries (`. `, `! `, `? `) so no chunk is unbounded.
pub fn parse_plain_sections(text: &str) -> Vec<DocSection> {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    if lines.is_empty() {
        return vec![DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content: String::new(),
            byte_range: 0..0,
            line_range: 0..0,
            element_types: vec![],
        }];
    }

    let mut sections: Vec<DocSection> = Vec::new();
    let line_offsets = build_line_offsets(text);

    // Group consecutive non-blank lines into paragraphs.
    let mut para_start: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            if let Some(start) = para_start.take() {
                emit_paragraph(&lines, start, idx, text, &line_offsets, &mut sections);
            }
        } else if para_start.is_none() {
            para_start = Some(idx);
        }
    }
    if let Some(start) = para_start {
        emit_paragraph(
            &lines,
            start,
            total_lines,
            text,
            &line_offsets,
            &mut sections,
        );
    }

    if sections.is_empty() {
        // File was entirely blank — emit a single empty section so the
        // file still appears in the index (for e.g. empty `LICENSE` stubs).
        sections.push(DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content: text.to_string(),
            byte_range: 0..text.len(),
            line_range: 0..total_lines,
            element_types: vec![],
        });
    }

    sections
}

/// Emit one or more `DocSection`s for the paragraph spanning
/// `start_line..end_line` (exclusive end). Splits on sentence boundaries
/// when the paragraph exceeds `MAX_PARA_BYTES`.
fn emit_paragraph(
    lines: &[&str],
    start_line: usize,
    end_line: usize,
    text: &str,
    line_offsets: &[usize],
    out: &mut Vec<DocSection>,
) {
    let content = lines[start_line..end_line].join("\n");
    let byte_start = line_byte_offset(line_offsets, start_line, text.len());
    let byte_end = line_byte_offset(line_offsets, end_line, text.len());

    if content.len() <= MAX_PARA_BYTES {
        out.push(DocSection {
            heading: derive_heading(&content),
            level: 0,
            section_path: vec![],
            content: content.clone(),
            byte_range: byte_start..byte_end,
            line_range: start_line..end_line,
            element_types: vec![DocElement::Paragraph],
        });
        return;
    }

    // Long paragraph → split on sentence boundaries.
    let mut cursor = 0usize;
    while cursor < content.len() {
        let slice_end = (cursor + MAX_PARA_BYTES).min(content.len());
        let slice = &content[cursor..slice_end];
        // Try to end at a sentence boundary near the end of the slice.
        let break_at = slice
            .rfind(". ")
            .or_else(|| slice.rfind("! "))
            .or_else(|| slice.rfind("? "))
            .map(|i| i + 2) // include the `. ` delimiter
            .unwrap_or_else(|| {
                if slice_end == content.len() {
                    slice.len()
                } else {
                    // No boundary found → fall back to a byte-safe split.
                    let mut i = slice.len();
                    while i > 0 && !content.is_char_boundary(cursor + i) {
                        i -= 1;
                    }
                    i.max(1)
                }
            });
        let piece = &content[cursor..cursor + break_at];
        let piece_byte_start = byte_start + cursor;
        let piece_byte_end = byte_start + cursor + break_at;
        out.push(DocSection {
            heading: derive_heading(piece),
            level: 0,
            section_path: vec![],
            content: piece.to_string(),
            byte_range: piece_byte_start..piece_byte_end,
            // Approximate line range — plain-text split by bytes; not critical.
            line_range: start_line..end_line,
            element_types: vec![DocElement::Paragraph],
        });
        cursor += break_at;
    }
}

/// Derive a pseudo-heading from the first non-empty line of the paragraph
/// (first 80 chars, trimmed).
fn derive_heading(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let trimmed = first.trim();
    if trimmed.len() <= 80 {
        trimmed.to_string()
    } else {
        let mut cut = 80;
        while !trimmed.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        format!("{}…", &trimmed[..cut])
    }
}

/// Extract symbol references from plain text. Accepts backticked spans
/// (since plain-text readme-style files often include `` `command` ``
/// notation even without explicit markdown framing).
pub(crate) fn extract_plain_refs(text: &str) -> Vec<SymbolRef> {
    // Reuse the markdown backtick scanner — plain text and markdown share
    // the same single-backtick convention for code references.
    super::markdown::extract_backtick_refs(text)
        .into_iter()
        .filter(|r| is_likely_symbol(&r.name) || is_likely_symbol(&clean_symbol_name(&r.name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_paragraphs_on_blank_lines() {
        let text = "First paragraph text.\nMore of first.\n\nSecond paragraph.\n\nThird.\n";
        let sections = parse_plain_sections(text);
        assert_eq!(sections.len(), 3);
        assert!(sections[0].content.contains("First paragraph"));
        assert!(sections[1].content.contains("Second paragraph"));
        assert!(sections[2].content.contains("Third"));
    }

    #[test]
    fn single_paragraph_single_section() {
        let text = "Only one paragraph here.\nSecond line of same paragraph.\n";
        let sections = parse_plain_sections(text);
        assert_eq!(sections.len(), 1);
    }

    #[test]
    fn empty_file_still_indexed() {
        let text = "";
        let sections = parse_plain_sections(text);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].content.is_empty());
    }

    #[test]
    fn blank_only_file_still_indexed() {
        let text = "\n\n\n";
        let sections = parse_plain_sections(text);
        // All lines blank → fall through to the single-empty-section fallback.
        assert_eq!(sections.len(), 1);
    }

    #[test]
    fn long_paragraph_splits_on_sentence_boundary() {
        let long_sentence = "This is a sentence. ".repeat(200); // ~4000 bytes
        let sections = parse_plain_sections(&long_sentence);
        assert!(
            sections.len() > 1,
            "expected long paragraph to split, got {} section(s)",
            sections.len()
        );
        for s in &sections {
            assert!(
                s.content.len() <= MAX_PARA_BYTES + 2,
                "split piece exceeded cap: {}",
                s.content.len()
            );
        }
    }

    #[test]
    fn heading_derived_from_first_line() {
        let text = "A short title line\nAnd body content below.\n";
        let sections = parse_plain_sections(text);
        assert_eq!(sections[0].heading, "A short title line");
    }

    #[test]
    fn long_first_line_truncates_heading() {
        let long = "A".repeat(200);
        let sections = parse_plain_sections(&long);
        assert!(sections[0].heading.ends_with('…'));
        assert!(sections[0].heading.chars().count() <= 81);
    }

    #[test]
    fn backtick_refs_extracted() {
        let text = "Run the `make_index()` command. See `Engine::init` too.";
        let refs = extract_plain_refs(text);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "make_index");
        assert_eq!(refs[1].name, "Engine::init");
    }
}
