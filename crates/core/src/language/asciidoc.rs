//! AsciiDoc document parser.
//!
//! Line-based section parser for `.adoc` / `.asciidoc` files. AsciiDoc uses
//! `= ` prefix counts for section levels (matching the old AsciiDoc 1.x
//! "atx-style" convention, which Asciidoctor preserves):
//!
//! - `= Title` → level 1 (document title)
//! - `== Title` → level 2
//! - `=== Title` → level 3
//! - …up to level 6.
//!
//! Out of scope (future work): cross-refs (`<<anchor,label>>`), includes
//! (`include::file.adoc[]`), attribute substitution, tables, callouts.

use super::Language;
use super::doc::{
    DocElement, DocLanguageSupport, DocSection, SymbolRef, build_line_offsets, line_byte_offset,
};
use super::markdown::{clean_symbol_name, is_likely_symbol};

/// AsciiDoc language support.
pub struct AsciiDocLanguage;

impl DocLanguageSupport for AsciiDocLanguage {
    fn language(&self) -> Language {
        Language::AsciiDoc
    }

    fn parse_sections(&self, source: &[u8], _file_name: Option<&str>) -> Vec<DocSection> {
        let text = String::from_utf8_lossy(source);
        parse_asciidoc_sections(&text)
    }

    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef> {
        let text = String::from_utf8_lossy(source);
        extract_asciidoc_refs(&text)
    }
}

/// Parse AsciiDoc text into `DocSection`s.
///
/// Heading detection: a line starting with 1–6 `=` characters followed by
/// a space and non-empty title text. Hierarchy follows the `=` count.
pub fn parse_asciidoc_sections(text: &str) -> Vec<DocSection> {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    struct HeadingInfo {
        level: u8,
        title: String,
        /// 0-indexed source line.
        line_idx: usize,
    }

    let mut headings: Vec<HeadingInfo> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some((level, title)) = parse_asciidoc_heading(line) {
            headings.push(HeadingInfo {
                level,
                title,
                line_idx: idx,
            });
        }
    }

    if headings.is_empty() {
        let content = text.to_string();
        let byte_end = text.len();
        return vec![DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content,
            byte_range: 0..byte_end,
            line_range: 0..total_lines,
            element_types: detect_elements_in_text(text),
        }];
    }

    let mut sections: Vec<DocSection> = Vec::new();
    let line_offsets = build_line_offsets(text);

    // Preamble before the first heading.
    let first_line = headings[0].line_idx;
    if first_line > 0 {
        let content = lines[0..first_line].join("\n");
        let byte_end = line_byte_offset(&line_offsets, first_line, text.len());
        sections.push(DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content: content.clone(),
            byte_range: 0..byte_end,
            line_range: 0..first_line,
            element_types: detect_elements_in_text(&content),
        });
    }

    let mut heading_stack: Vec<(u8, String)> = Vec::new();
    let num_headings = headings.len();

    for idx in 0..num_headings {
        let h = &headings[idx];
        heading_stack.retain(|(lvl, _)| *lvl < h.level);
        heading_stack.push((h.level, h.title.clone()));
        let section_path: Vec<String> = heading_stack.iter().map(|(_, t)| t.clone()).collect();

        let section_start_line = h.line_idx;
        let section_end_line = if idx + 1 < num_headings {
            headings[idx + 1].line_idx
        } else {
            total_lines
        };

        let content = lines[section_start_line..section_end_line.min(total_lines)].join("\n");
        let byte_start = line_byte_offset(&line_offsets, section_start_line, text.len());
        let byte_end =
            line_byte_offset(&line_offsets, section_end_line.min(total_lines), text.len());

        sections.push(DocSection {
            heading: h.title.clone(),
            level: h.level,
            section_path,
            content: content.clone(),
            byte_range: byte_start..byte_end,
            line_range: section_start_line..section_end_line,
            element_types: detect_elements_in_text(&content),
        });
    }

    sections
}

/// Parse an AsciiDoc heading line. Returns `(level, title)` on success.
fn parse_asciidoc_heading(line: &str) -> Option<(u8, String)> {
    let bytes = line.as_bytes();
    if bytes.is_empty() || bytes[0] != b'=' {
        return None;
    }
    let mut level = 0u8;
    for b in bytes {
        if *b == b'=' {
            level += 1;
            if level > 6 {
                return None;
            }
        } else {
            break;
        }
    }
    if level == 0 {
        return None;
    }
    // Must be followed by at least one space, then non-empty title.
    let rest = &line[level as usize..];
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title.to_string()))
}

/// Detect element types inside a block of AsciiDoc text.
fn detect_elements_in_text(text: &str) -> Vec<DocElement> {
    let mut elements: Vec<DocElement> = Vec::new();
    let mut in_source_block = false;
    let mut source_lang: Option<String> = None;
    let mut seen_code = false;
    let mut seen_list = false;
    let mut seen_table = false;
    let mut seen_paragraph = false;

    for line in text.lines() {
        let trimmed = line.trim_start();

        // `[source,python]` or `[source, python]` → next `----` opens a code block.
        if !seen_code && trimmed.starts_with("[source") {
            source_lang = trimmed
                .trim_start_matches("[source")
                .trim_start_matches(',')
                .trim_end_matches(']')
                .trim()
                .to_string()
                .into();
            if source_lang.as_ref().is_some_and(|s| s.is_empty()) {
                source_lang = None;
            }
            continue;
        }

        if trimmed.starts_with("----") {
            in_source_block = !in_source_block;
            if in_source_block && !seen_code {
                elements.push(DocElement::CodeBlock {
                    language: source_lang.take(),
                });
                seen_code = true;
            }
            continue;
        }

        if in_source_block {
            continue;
        }

        if !seen_list
            && (trimmed.starts_with("* ") || trimmed.starts_with("- ") || trimmed.starts_with(". "))
        {
            elements.push(DocElement::List);
            seen_list = true;
        } else if !seen_table && trimmed.starts_with("|===") {
            elements.push(DocElement::Table);
            seen_table = true;
        } else if !seen_paragraph && !trimmed.is_empty() && parse_asciidoc_heading(line).is_none() {
            elements.push(DocElement::Paragraph);
            seen_paragraph = true;
        }
    }

    elements
}

/// Extract `` `symbol` `` backtick references from AsciiDoc text.
///
/// AsciiDoc uses single backticks for `monospace` and plus signs
/// (`+literal+`) for passthrough. We scan backticks only — matches
/// Markdown semantics and covers the common case.
pub(crate) fn extract_asciidoc_refs(text: &str) -> Vec<SymbolRef> {
    let mut refs = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        if bytes[i] == b'`' {
            let tick_start = i;
            i += 1;
            // Find closing backtick.
            let content_start = i;
            while i < len && bytes[i] != b'`' && bytes[i] != b'\n' {
                i += 1;
            }
            if i >= len || bytes[i] != b'`' {
                continue;
            }
            let content_end = i;
            i += 1; // consume closing backtick
            if let Ok(content) = std::str::from_utf8(&bytes[content_start..content_end]) {
                let content = content.trim();
                if is_likely_symbol(content) {
                    refs.push(SymbolRef {
                        name: clean_symbol_name(content),
                        byte_range: tick_start..i,
                    });
                }
            }
        } else {
            i += 1;
        }
    }

    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_headings() {
        let adoc = "= Document Title\n\nIntro.\n\n== Section A\n\nContent A.\n\n== Section B\n\nContent B.\n";
        let sections = parse_asciidoc_sections(adoc);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Document Title");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[1].heading, "Section A");
        assert_eq!(sections[1].level, 2);
        assert_eq!(
            sections[1].section_path,
            vec!["Document Title", "Section A"]
        );
        assert_eq!(sections[2].heading, "Section B");
    }

    #[test]
    fn parse_nested_headings() {
        let adoc =
            "= Root\n\n== Child\n\n=== Grandchild\n\nDeep.\n\n== Sibling\n\nSibling content.\n";
        let sections = parse_asciidoc_sections(adoc);
        assert_eq!(sections.len(), 4);
        assert_eq!(
            sections[2].section_path,
            vec!["Root", "Child", "Grandchild"]
        );
        assert_eq!(sections[3].section_path, vec!["Root", "Sibling"]);
    }

    #[test]
    fn parse_preamble_before_title() {
        let adoc = "Some preamble.\n\n= Real Title\n\nBody.\n";
        let sections = parse_asciidoc_sections(adoc);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].level, 0);
        assert!(sections[0].content.contains("preamble"));
        assert_eq!(sections[1].heading, "Real Title");
    }

    #[test]
    fn parse_no_headings_single_section() {
        let adoc = "Just text.\n\nNo headings.\n";
        let sections = parse_asciidoc_sections(adoc);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].level, 0);
    }

    #[test]
    fn heading_parser_rejects_bad_formats() {
        assert!(parse_asciidoc_heading("=Title").is_none());
        assert!(parse_asciidoc_heading("= ").is_none());
        assert!(parse_asciidoc_heading("========= TooDeep").is_none());
        assert!(parse_asciidoc_heading("plain text").is_none());
        assert_eq!(
            parse_asciidoc_heading("== Valid"),
            Some((2, "Valid".to_string()))
        );
    }

    #[test]
    fn source_block_detected_with_language() {
        let adoc = "= Intro\n\n[source,python]\n----\nprint('hi')\n----\n";
        let sections = parse_asciidoc_sections(adoc);
        assert_eq!(sections.len(), 1);
        let has_code = sections[0].element_types.iter().any(|e| {
            matches!(
                e,
                DocElement::CodeBlock {
                    language: Some(lang)
                } if lang == "python"
            )
        });
        assert!(has_code, "expected Python code block detection");
    }

    #[test]
    fn extract_backtick_refs_filtered() {
        let adoc =
            "Use `Engine::init()` to start. See `add_chunk` for details. The `ChunkConfig` struct.";
        let refs = extract_asciidoc_refs(adoc);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].name, "Engine::init");
    }
}
