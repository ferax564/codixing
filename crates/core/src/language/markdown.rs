//! Markdown document parser using comrak.
//!
//! Parses Markdown into `DocSection`s with heading hierarchy, section paths,
//! element type detection, and backtick symbol reference extraction.

use comrak::nodes::{AstNode, NodeValue};
use comrak::{Arena, Options, parse_document};

use super::Language;
use super::doc::{DocElement, DocLanguageSupport, DocSection, SymbolRef};

/// Markdown language support using comrak for AST-based section parsing.
pub struct MarkdownLanguage;

impl DocLanguageSupport for MarkdownLanguage {
    fn language(&self) -> Language {
        Language::Markdown
    }

    fn parse_sections(&self, source: &[u8]) -> Vec<DocSection> {
        let text = String::from_utf8_lossy(source);
        parse_markdown_sections(&text)
    }

    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef> {
        let text = String::from_utf8_lossy(source);
        extract_backtick_refs(&text)
    }
}

/// Parse markdown text into a flat list of `DocSection`s.
///
/// Each heading becomes a section boundary. Text before the first heading
/// is collected as a level-0 preamble section. Heading hierarchy is tracked
/// to build `section_path` breadcrumbs.
pub fn parse_markdown_sections(text: &str) -> Vec<DocSection> {
    let arena = Arena::new();
    let options = Options::default();
    let root = parse_document(&arena, text, &options);

    // Collect heading positions and text from the AST.
    struct HeadingInfo {
        level: u8,
        heading_text: String,
        /// 1-indexed source line of the heading node.
        start_line: usize,
    }

    let mut headings: Vec<HeadingInfo> = Vec::new();
    for node in root.descendants() {
        let data = node.data.borrow();
        if let NodeValue::Heading(ref h) = data.value {
            let level = h.level;
            let heading_text = collect_text(node);
            let start_line = data.sourcepos.start.line;
            headings.push(HeadingInfo {
                level,
                heading_text,
                start_line,
            });
        }
    }

    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    // Build sections from headings.
    // heading_stack tracks (level, heading_text) pairs for section_path building.
    let mut heading_stack: Vec<(u8, String)> = Vec::new();
    let mut sections: Vec<DocSection> = Vec::new();

    // Determine content line ranges for each section.
    // A section spans from its heading line to just before the next heading (exclusive),
    // or to the end of the document.
    //
    // Sections are defined by the start line of each heading (1-indexed from comrak).
    // We convert to 0-indexed for our line_range.

    let num_headings = headings.len();

    // No headings at all: entire document is a single level-0 section.
    if headings.is_empty() {
        let content = text.to_string();
        let byte_end = text.len();
        let element_types = detect_elements_in_text(&content);
        return vec![DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content,
            byte_range: 0..byte_end,
            line_range: 0..total_lines,
            element_types,
        }];
    }

    // Preamble: lines before the first heading (if any).
    let first_heading_line = headings.first().map(|h| h.start_line).unwrap_or(0);

    if first_heading_line > 1 {
        // There is content before the first heading (lines 1..first_heading_line-1, 1-indexed)
        let preamble_start = 0usize; // 0-indexed
        let preamble_end = first_heading_line - 1; // exclusive, 0-indexed
        let content = lines[preamble_start..preamble_end.min(total_lines)].join("\n");
        let byte_start = 0;
        let byte_end = line_to_byte_offset(text, preamble_end.min(total_lines));
        let element_types = detect_elements_in_text(&content);
        sections.push(DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content,
            byte_range: byte_start..byte_end,
            line_range: preamble_start..preamble_end,
            element_types,
        });
    }

    for (i, heading) in headings.iter().enumerate() {
        // Pop the stack to the right parent level.
        heading_stack.retain(|(lvl, _)| *lvl < heading.level);
        heading_stack.push((heading.level, heading.heading_text.clone()));

        let section_path: Vec<String> = heading_stack.iter().map(|(_, t)| t.clone()).collect();

        // Content lines: from after the heading line to before the next heading (or EOF).
        // heading.start_line is 1-indexed; content starts at the next line.
        let content_start_line = heading.start_line; // 0-indexed exclusive start = heading line (0-indexed) + 1
        let content_end_line = if i + 1 < num_headings {
            headings[i + 1].start_line - 1 // lines up to but not including the next heading
        } else {
            total_lines
        };

        // Include heading line in content for context.
        let section_start_line = heading.start_line - 1; // 0-indexed
        let section_end_line = content_end_line;

        let content = lines[section_start_line..section_end_line.min(total_lines)].join("\n");

        let byte_start = line_to_byte_offset(text, section_start_line);
        let byte_end = line_to_byte_offset(text, section_end_line.min(total_lines));

        let element_types = detect_elements(root, content_start_line, content_end_line);

        sections.push(DocSection {
            heading: heading.heading_text.clone(),
            level: heading.level,
            section_path,
            content,
            byte_range: byte_start..byte_end,
            line_range: section_start_line..section_end_line,
            element_types,
        });
    }

    sections
}

/// Collect all text content under an AST node (recursively).
fn collect_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut result = String::new();
    for descendant in node.descendants() {
        let data = descendant.data.borrow();
        match &data.value {
            NodeValue::Text(s) => result.push_str(s),
            NodeValue::Code(c) => result.push_str(&c.literal),
            NodeValue::SoftBreak | NodeValue::LineBreak => result.push(' '),
            _ => {}
        }
    }
    result
}

/// Detect element types within a line range of the AST.
fn detect_elements<'a>(
    root: &'a AstNode<'a>,
    start_line: usize,
    end_line: usize,
) -> Vec<DocElement> {
    let mut elements: Vec<DocElement> = Vec::new();
    for node in root.descendants() {
        let data = node.data.borrow();
        let line = data.sourcepos.start.line;
        // 1-indexed: start_line is the line after the heading (1-indexed)
        if line < start_line || line >= end_line.saturating_add(1) {
            continue;
        }
        match &data.value {
            NodeValue::Paragraph if !elements.contains(&DocElement::Paragraph) => {
                elements.push(DocElement::Paragraph);
            }
            NodeValue::CodeBlock(cb) => {
                let lang = if cb.info.is_empty() {
                    None
                } else {
                    Some(cb.info.clone())
                };
                let el = DocElement::CodeBlock { language: lang };
                if !elements.contains(&el) {
                    elements.push(el);
                }
            }
            NodeValue::Table(_) if !elements.contains(&DocElement::Table) => {
                elements.push(DocElement::Table);
            }
            NodeValue::List(_) if !elements.contains(&DocElement::List) => {
                elements.push(DocElement::List);
            }
            NodeValue::BlockQuote if !elements.contains(&DocElement::BlockQuote) => {
                elements.push(DocElement::BlockQuote);
            }
            NodeValue::Image(link) => {
                let alt = collect_text(node);
                let el = DocElement::Image { alt };
                // Images deduplicated by alt text is complex; allow duplicates for now
                // unless already present with same alt.
                let url = &link.url;
                let _ = url; // suppress unused warning
                if !elements
                    .iter()
                    .any(|e| matches!(e, DocElement::Image { .. }))
                {
                    elements.push(el);
                }
            }
            _ => {}
        }
    }
    elements
}

/// Detect element types from raw text (used for preamble/no-heading documents).
fn detect_elements_in_text(text: &str) -> Vec<DocElement> {
    let arena = Arena::new();
    let options = Options::default();
    let root = parse_document(&arena, text, &options);
    detect_elements(root, 0, usize::MAX)
}

/// Convert a 0-indexed line number to a byte offset in the text.
///
/// Returns the byte offset of the start of the given line.
/// If `line >= total_lines`, returns `text.len()`.
fn line_to_byte_offset(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut current_line = 0;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            current_line += 1;
            if current_line == line {
                return i + 1;
            }
        }
    }
    text.len()
}

/// Extract backtick symbol references from markdown text.
///
/// Scans for backtick-delimited spans and filters them with `is_likely_symbol()`.
/// Returns `SymbolRef` for each match with its byte range.
pub(crate) fn extract_backtick_refs(text: &str) -> Vec<SymbolRef> {
    let mut refs = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if bytes[i] == b'`' {
            // Count consecutive backticks to determine the delimiter.
            let tick_start = i;
            let mut tick_count = 0;
            while i < len && bytes[i] == b'`' {
                tick_count += 1;
                i += 1;
            }
            // Find matching closing backticks.
            if let Some(close_end) = find_closing_backticks(bytes, i, tick_count) {
                let content_start = i;
                let content_end = close_end - tick_count;
                if let Ok(content) = std::str::from_utf8(&bytes[content_start..content_end]) {
                    let content = content.trim();
                    if is_likely_symbol(content) {
                        let cleaned = clean_symbol_name(content);
                        refs.push(SymbolRef {
                            name: cleaned,
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

/// Find the end position (exclusive) of closing backticks.
///
/// Starting at `start`, looks for `count` consecutive backticks and returns
/// the byte index just after the last closing backtick.
fn find_closing_backticks(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
    let len = bytes.len();
    let mut i = start;
    while i < len {
        if bytes[i] == b'`' {
            let mut tick_count = 0;
            while i < len && bytes[i] == b'`' {
                tick_count += 1;
                i += 1;
            }
            if tick_count == count {
                return Some(i);
            }
            // Wrong number of backticks — keep searching.
        } else {
            i += 1;
        }
    }
    None
}

/// Heuristic: does this backtick-delimited string look like a code symbol?
///
/// Returns `true` if any of these patterns match:
/// - Contains `::` (qualified path: `Engine::init`)
/// - Contains `_` (snake_case: `add_chunk`)
/// - Is CamelCase (starts uppercase, has a lowercase after: `ChunkConfig`)
/// - Ends with `()` (function call: `foo()`)
///
/// Returns `false` for plain English words, short strings, or common keywords.
pub(crate) fn is_likely_symbol(s: &str) -> bool {
    if s.is_empty() || s.len() < 2 {
        return false;
    }
    // Reject if contains spaces (not a symbol).
    if s.contains(' ') {
        return false;
    }
    // Common keywords/literals that aren't symbols.
    let non_symbols = [
        "true",
        "false",
        "null",
        "nil",
        "none",
        "undefined",
        "void",
        "int",
        "str",
        "bool",
        "float",
        "char",
        "byte",
    ];
    if non_symbols.iter().any(|&kw| s.eq_ignore_ascii_case(kw)) {
        return false;
    }

    // Qualified path (Rust/C++/TypeScript).
    if s.contains("::") {
        return true;
    }
    // snake_case.
    if s.contains('_') {
        return true;
    }
    // Ends with () — function call.
    if s.ends_with("()") {
        return true;
    }
    // CamelCase: first char uppercase, has at least one lowercase after.
    if s.starts_with(|c: char| c.is_uppercase()) && s.chars().any(|c| c.is_lowercase()) {
        return true;
    }

    false
}

/// Clean a symbol name extracted from backtick content.
///
/// - Strips trailing `()`
/// - Strips leading `&` or `*`
pub(crate) fn clean_symbol_name(s: &str) -> String {
    let s = s.trim_start_matches('&').trim_start_matches('*');
    let s = s.strip_suffix("()").unwrap_or(s);
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_markdown_sections() {
        let md = "# Title\n\nIntro paragraph.\n\n## Section A\n\nContent A.\n\n## Section B\n\nContent B.\n";
        let sections = parse_markdown_sections(md);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Title");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[0].section_path, vec!["Title"]);
        assert_eq!(sections[1].heading, "Section A");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[1].section_path, vec!["Title", "Section A"]);
        assert_eq!(sections[2].heading, "Section B");
        assert_eq!(sections[2].level, 2);
        assert_eq!(sections[2].section_path, vec!["Title", "Section B"]);
    }

    #[test]
    fn parse_nested_headings() {
        let md = "# Root\n\n## Child\n\n### Grandchild\n\nDeep content.\n\n## Sibling\n\nSibling content.\n";
        let sections = parse_markdown_sections(md);
        assert_eq!(sections.len(), 4);
        assert_eq!(
            sections[2].section_path,
            vec!["Root", "Child", "Grandchild"]
        );
        assert_eq!(sections[3].section_path, vec!["Root", "Sibling"]);
    }

    #[test]
    fn parse_preamble_before_first_heading() {
        let md = "Some preamble text.\n\n# First Heading\n\nContent.\n";
        let sections = parse_markdown_sections(md);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].level, 0);
        assert!(sections[0].content.contains("preamble"));
        assert_eq!(sections[1].heading, "First Heading");
    }

    #[test]
    fn parse_no_headings() {
        let md = "Just some text.\n\nWith paragraphs.\n";
        let sections = parse_markdown_sections(md);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].level, 0);
        assert!(sections[0].content.contains("Just some text"));
    }

    #[test]
    fn detect_code_block_element() {
        let md = "# API\n\n```rust\nfn main() {}\n```\n";
        let sections = parse_markdown_sections(md);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].element_types.contains(&DocElement::CodeBlock {
            language: Some("rust".to_string()),
        }));
    }

    #[test]
    fn extract_backtick_symbol_refs() {
        let md =
            "Use `Engine::init()` to start. See `add_chunk` for details. The `ChunkConfig` struct.";
        let refs = extract_backtick_refs(md);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].name, "Engine::init");
        assert_eq!(refs[1].name, "add_chunk");
        assert_eq!(refs[2].name, "ChunkConfig");
    }

    #[test]
    fn skip_non_symbol_backticks() {
        let md = "Run `npm install` then `cd src`. Use `true` and `false`.";
        let refs = extract_backtick_refs(md);
        assert!(refs.is_empty());
    }

    #[test]
    fn is_likely_symbol_heuristics() {
        assert!(is_likely_symbol("Engine::init"));
        assert!(is_likely_symbol("add_chunk"));
        assert!(is_likely_symbol("ChunkConfig"));
        assert!(is_likely_symbol("foo()"));
        assert!(!is_likely_symbol("npm install"));
        assert!(!is_likely_symbol("true"));
        assert!(!is_likely_symbol(""));
        assert!(!is_likely_symbol("a"));
    }
}
