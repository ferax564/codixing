//! reStructuredText document parser.
//!
//! Parses `.rst` files into `DocSection`s with section-hierarchy tracking
//! following RST's dynamic-level rule: the first punctuation character used
//! as an underline defines level 1, the next distinct character defines
//! level 2, and so on.
//!
//! Out of scope (future work): directive expansion (`.. toctree::`),
//! cross-file `:ref:` resolution, Sphinx extensions.

use super::Language;
use super::doc::{DocElement, DocLanguageSupport, DocSection, SymbolRef};
use super::markdown::{clean_symbol_name, is_likely_symbol};

/// reStructuredText language support.
pub struct RstLanguage;

impl DocLanguageSupport for RstLanguage {
    fn language(&self) -> Language {
        Language::Rst
    }

    fn parse_sections(&self, source: &[u8]) -> Vec<DocSection> {
        let text = String::from_utf8_lossy(source);
        parse_rst_sections(&text)
    }

    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef> {
        let text = String::from_utf8_lossy(source);
        extract_rst_refs(&text)
    }
}

/// Valid RST section-adornment characters per the spec.
///
/// RST accepts any printable non-alphanumeric ASCII punctuation, but this
/// set covers everything used in practice in Sphinx / Python / Linux docs.
fn is_adornment_char(c: char) -> bool {
    matches!(
        c,
        '=' | '-'
            | '~'
            | '^'
            | '"'
            | '\''
            | '+'
            | '*'
            | '#'
            | '`'
            | ':'
            | '.'
            | '_'
            | '!'
            | '<'
            | '>'
            | '?'
            | '@'
    )
}

/// Is `line` an adornment line (same punctuation character repeated ≥ 2 times)?
/// Returns the adornment char if so.
fn adornment_char_of(line: &str) -> Option<char> {
    let trimmed = line.trim_end();
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if !is_adornment_char(first) {
        return None;
    }
    let mut count = 1usize;
    for c in chars {
        if c != first {
            return None;
        }
        count += 1;
    }
    if count < 2 { None } else { Some(first) }
}

/// Could `line` be a section title? Rejects blank/adornment-only lines.
fn is_candidate_title(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    // A line that is itself a run of adornment chars is not a title.
    if adornment_char_of(line).is_some() {
        return false;
    }
    // Reject directive lines like `.. note::` — they're not titles.
    if t.starts_with("..") {
        return false;
    }
    true
}

/// Parse an RST string into a flat list of `DocSection`s.
///
/// Detects two title forms:
///
/// - Single underline:
///   ```text
///   Title
///   =====
///   ```
/// - Overline + underline (same char, length ≥ title):
///   ```text
///   =====
///   Title
///   =====
///   ```
///
/// Hierarchy is discovered dynamically: the first distinct adornment
/// character encountered becomes level 1, the next becomes level 2, etc.
pub fn parse_rst_sections(text: &str) -> Vec<DocSection> {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // Discover heading positions.
    struct HeadingInfo {
        level: u8,
        title: String,
        /// 0-indexed line of the first line of the section block (overline or title).
        block_start_line: usize,
    }

    // Adornment char -> level (discovered in order of first appearance).
    let mut level_map: Vec<char> = Vec::new();
    let mut headings: Vec<HeadingInfo> = Vec::new();

    let mut i = 0usize;
    while i < total_lines {
        let line = lines[i];

        // Overline + title + underline pattern.
        if let Some(over_char) = adornment_char_of(line)
            && i + 2 < total_lines
            && is_candidate_title(lines[i + 1])
            && let Some(under_char) = adornment_char_of(lines[i + 2])
            && under_char == over_char
            && lines[i + 2].trim_end().chars().count() >= lines[i + 1].trim_end().chars().count()
            && line.trim_end().chars().count() >= lines[i + 1].trim_end().chars().count()
        {
            let level = level_of(&mut level_map, over_char);
            headings.push(HeadingInfo {
                level,
                title: lines[i + 1].trim().to_string(),
                block_start_line: i,
            });
            i += 3;
            continue;
        }

        // Single underline pattern.
        if is_candidate_title(line)
            && i + 1 < total_lines
            && let Some(under_char) = adornment_char_of(lines[i + 1])
            && lines[i + 1].trim_end().chars().count() >= line.trim_end().chars().count()
        {
            let level = level_of(&mut level_map, under_char);
            headings.push(HeadingInfo {
                level,
                title: line.trim().to_string(),
                block_start_line: i,
            });
            i += 2;
            continue;
        }

        i += 1;
    }

    // No headings → single level-0 section with full text.
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

    // Preamble: text before the first heading block.
    let first_block_start = headings[0].block_start_line;
    if first_block_start > 0 {
        let content = lines[0..first_block_start].join("\n");
        let byte_end = line_to_byte_offset(text, first_block_start);
        sections.push(DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content: content.clone(),
            byte_range: 0..byte_end,
            line_range: 0..first_block_start,
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

        let section_start_line = h.block_start_line;
        let section_end_line = if idx + 1 < num_headings {
            headings[idx + 1].block_start_line
        } else {
            total_lines
        };

        let content = lines[section_start_line..section_end_line.min(total_lines)].join("\n");
        let byte_start = line_to_byte_offset(text, section_start_line);
        let byte_end = line_to_byte_offset(text, section_end_line.min(total_lines));

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

/// Look up (or register) the level for an adornment character.
fn level_of(level_map: &mut Vec<char>, c: char) -> u8 {
    for (i, existing) in level_map.iter().enumerate() {
        if *existing == c {
            return (i + 1) as u8;
        }
    }
    level_map.push(c);
    level_map.len() as u8
}

/// Detect element types inside a block of RST text (used for each section).
///
/// Element detection here is intentionally coarse; we only report the
/// presence of a few high-level element kinds so the search layer can
/// filter code-vs-prose. Precise element tracking is not required for
/// retrieval.
fn detect_elements_in_text(text: &str) -> Vec<DocElement> {
    let mut elements: Vec<DocElement> = Vec::new();

    // Code block: `.. code-block:: <lang>` directive OR indented literal block
    // following a line ending in `::`.
    let mut seen_code = false;
    let mut seen_list = false;
    let mut seen_paragraph = false;
    let mut seen_table = false;
    let mut seen_block_quote = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if !seen_code
            && (trimmed.starts_with(".. code-block::") || trimmed.starts_with(".. code::"))
        {
            let lang = trimmed
                .split("::")
                .nth(1)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            elements.push(DocElement::CodeBlock { language: lang });
            seen_code = true;
        } else if !seen_list
            && (trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed.starts_with("#. "))
        {
            elements.push(DocElement::List);
            seen_list = true;
        } else if !seen_table && (trimmed.starts_with("+-") || trimmed.starts_with("| ")) {
            elements.push(DocElement::Table);
            seen_table = true;
        } else if !seen_block_quote && line.starts_with("    ") && !trimmed.is_empty() {
            // Indented block → treat as blockquote/literal.
            elements.push(DocElement::BlockQuote);
            seen_block_quote = true;
        } else if !seen_paragraph && !trimmed.is_empty() && adornment_char_of(line).is_none() {
            elements.push(DocElement::Paragraph);
            seen_paragraph = true;
        }
    }

    elements
}

/// 0-indexed line → byte offset of the start of that line.
fn line_to_byte_offset(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut current = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            current += 1;
            if current == line {
                return i + 1;
            }
        }
    }
    text.len()
}

/// Extract code-symbol references from RST text.
///
/// RST uses double backticks for literal code (``Engine::init``). Single
/// backticks denote "interpreted text" which may reference anything — we
/// also scan those but filter with `is_likely_symbol` to keep precision
/// high.
pub(crate) fn extract_rst_refs(text: &str) -> Vec<SymbolRef> {
    let mut refs = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    while i < len {
        if bytes[i] == b'`' {
            let tick_start = i;
            let mut tick_count = 0usize;
            while i < len && bytes[i] == b'`' {
                tick_count += 1;
                i += 1;
            }
            if tick_count > 2 {
                // More than `` means literal or malformed — skip.
                continue;
            }
            if let Some(close_end) = find_closing_backticks(bytes, i, tick_count) {
                let content_start = i;
                let content_end = close_end - tick_count;
                if let Ok(content) = std::str::from_utf8(&bytes[content_start..content_end]) {
                    let content = content.trim();
                    // Skip RST role prefixes: `:func:`foo`` — the leading :role:
                    // pattern consumes the wrapping backticks, we just keep the
                    // inner portion of the content.
                    let content = strip_rst_role(content);
                    if is_likely_symbol(content) {
                        refs.push(SymbolRef {
                            name: clean_symbol_name(content),
                            byte_range: tick_start..close_end,
                        });
                    }
                }
                i = close_end;
                continue;
            }
        }
        i += 1;
    }

    refs
}

fn find_closing_backticks(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
    let len = bytes.len();
    let mut i = start;
    while i < len {
        if bytes[i] == b'`' {
            let mut tick_count = 0usize;
            while i < len && bytes[i] == b'`' {
                tick_count += 1;
                i += 1;
            }
            if tick_count == count {
                return Some(i);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// `:role:`text`` → `text`. Otherwise returns input unchanged.
fn strip_rst_role(content: &str) -> &str {
    // Our backtick scanner already consumed the wrapping ticks. A role
    // prefix never appears inside the backtick content itself, so this
    // function is a no-op for now; kept as a hook for future extensions.
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_underline_sections() {
        let rst = "Title\n=====\n\nIntro paragraph.\n\nSection A\n---------\n\nContent A.\n\nSection B\n---------\n\nContent B.\n";
        let sections = parse_rst_sections(rst);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Title");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[1].heading, "Section A");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].section_path, vec!["Title", "Section A"]);
        assert_eq!(sections[2].heading, "Section B");
        assert_eq!(sections[2].level, 2);
    }

    #[test]
    fn parse_overline_sections() {
        let rst = "======\nHeader\n======\n\nBody text.\n\n------\nSubhdr\n------\n\nDetails.\n";
        let sections = parse_rst_sections(rst);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Header");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[1].heading, "Subhdr");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].section_path, vec!["Header", "Subhdr"]);
    }

    #[test]
    fn parse_dynamic_hierarchy_ties_char_to_level() {
        // First-encountered adornment = level 1. Underline `~` here first,
        // then `=` should become level 2.
        let rst = "Topic\n~~~~~\n\nP.\n\nDeep\n====\n\nQ.\n";
        let sections = parse_rst_sections(rst);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[1].level, 2);
    }

    #[test]
    fn parse_preamble_before_first_heading() {
        let rst = "Preface text.\n\nAnd more.\n\nTitle\n=====\n\nBody.\n";
        let sections = parse_rst_sections(rst);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].level, 0);
        assert!(sections[0].content.contains("Preface"));
        assert_eq!(sections[1].heading, "Title");
    }

    #[test]
    fn parse_no_headings_single_section() {
        let rst = "Just some RST text.\n\nNo headings here.\n";
        let sections = parse_rst_sections(rst);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].level, 0);
        assert!(sections[0].content.contains("Just some RST"));
    }

    #[test]
    fn adornment_char_rejects_short_runs() {
        assert!(adornment_char_of("=").is_none());
        assert_eq!(adornment_char_of("=="), Some('='));
        assert_eq!(adornment_char_of("------"), Some('-'));
        assert!(adornment_char_of("=x=").is_none());
        assert!(adornment_char_of("").is_none());
    }

    #[test]
    fn directive_lines_are_not_titles() {
        let rst = ".. note::\n\n   Boxed note.\n\nReal Title\n==========\n\nBody.\n";
        let sections = parse_rst_sections(rst);
        // Only one real heading should be found.
        let named: Vec<&str> = sections
            .iter()
            .filter(|s| !s.heading.is_empty())
            .map(|s| s.heading.as_str())
            .collect();
        assert_eq!(named, vec!["Real Title"]);
    }

    #[test]
    fn extract_double_backtick_symbols() {
        let rst = "Use ``Engine::init()`` to start. See ``add_chunk`` for details. The ``ChunkConfig`` struct.";
        let refs = extract_rst_refs(rst);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].name, "Engine::init");
        assert_eq!(refs[1].name, "add_chunk");
        assert_eq!(refs[2].name, "ChunkConfig");
    }

    #[test]
    fn extract_single_backtick_filters_noise() {
        let rst = "Some `plain text`. Also `snake_case_ref`.";
        let refs = extract_rst_refs(rst);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].name, "snake_case_ref");
    }

    #[test]
    fn code_block_directive_is_detected() {
        let rst = "Intro\n=====\n\nSome prose.\n\n.. code-block:: python\n\n   print('hi')\n";
        let sections = parse_rst_sections(rst);
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
    fn title_shorter_than_underline_is_accepted() {
        // RST requires underline >= title length. Longer underline is valid.
        let rst = "Hi\n=========\n\nBody.\n";
        let sections = parse_rst_sections(rst);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Hi");
    }

    #[test]
    fn underline_shorter_than_title_is_rejected() {
        // Not a valid RST title — underline too short.
        let rst = "Long Title\n===\n\nBody.\n";
        let sections = parse_rst_sections(rst);
        // Should fall through to preamble-only (no heading).
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].level, 0);
    }
}
