//! Document language support — structured parsing for Markdown and HTML.
//!
//! Produces a section tree (not flat entities) for hierarchical chunking,
//! and extracts code symbol references for doc-to-code graph edges.

use std::ops::Range;

use super::Language;

/// A section of a document, identified by a heading.
#[derive(Debug, Clone)]
pub struct DocSection {
    /// The heading text (e.g., "Installation").
    pub heading: String,
    /// Heading level: 1 for `#`, 2 for `##`, etc. 0 for preamble (text before first heading).
    pub level: u8,
    /// Full section path from root (e.g., `["Getting Started", "Installation"]`).
    pub section_path: Vec<String>,
    /// The full text content of this section (including sub-content).
    pub content: String,
    /// Byte range in the original source.
    pub byte_range: Range<usize>,
    /// Line range (0-indexed, start inclusive, end exclusive).
    pub line_range: Range<usize>,
    /// Types of elements contained in this section.
    pub element_types: Vec<DocElement>,
}

/// A structural element within a document section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocElement {
    Paragraph,
    CodeBlock { language: Option<String> },
    Table,
    List,
    BlockQuote,
    Image { alt: String },
}

/// A reference to a code symbol found in documentation text.
#[derive(Debug, Clone)]
pub struct SymbolRef {
    /// The symbol name (e.g., "Engine::init", "add_chunk").
    pub name: String,
    /// Byte range in the original source.
    pub byte_range: Range<usize>,
}

/// Precompute line → byte-offset lookup for a text block.
///
/// Returns a `Vec` where index `i` holds the byte offset of the start of
/// 0-indexed line `i`, plus one extra entry equal to `text.len()` so
/// callers can use `offsets[end_line]` uniformly without bounds checks.
///
/// Shared helper for the line-based doc parsers (RST, AsciiDoc, plain text,
/// CHANGELOG mode) — avoids O(sections × N) re-scans of the source inside
/// per-section loops.
pub(crate) fn build_line_offsets(text: &str) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(text.len() / 40 + 1);
    offsets.push(0);
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            offsets.push(i + 1);
        }
    }
    if *offsets.last().unwrap_or(&0) != text.len() {
        offsets.push(text.len());
    }
    offsets
}

/// Look up the byte offset of a 0-indexed line in a table produced by
/// [`build_line_offsets`]. Clamps to `text_len` when the line is past EOF.
pub(crate) fn line_byte_offset(offsets: &[usize], line: usize, text_len: usize) -> usize {
    if line >= offsets.len() {
        text_len
    } else {
        offsets[line]
    }
}

/// Document-aware parsing for Markdown, HTML, and future doc formats.
///
/// Produces a section tree (not flat entities) for hierarchical chunking.
pub trait DocLanguageSupport: Send + Sync {
    /// Which language this implementation handles.
    fn language(&self) -> Language;

    /// Parse source into a flat list of sections with heading hierarchy metadata.
    ///
    /// `file_name` is the basename of the file (e.g. `"CHANGELOG.md"`,
    /// `"README"`). Impls may use it for filename-driven heuristics —
    /// currently Markdown enters a changelog-aware mode when the file
    /// name matches `CHANGELOG*` / `HISTORY*` / `RELEASES*`.
    fn parse_sections(&self, source: &[u8], file_name: Option<&str>) -> Vec<DocSection>;

    /// Extract code symbol references (backticked identifiers, fenced code blocks).
    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_section_stores_heading_and_path() {
        let section = DocSection {
            heading: "Installation".to_string(),
            level: 2,
            section_path: vec!["Getting Started".to_string(), "Installation".to_string()],
            content: "Run the install script.".to_string(),
            byte_range: 0..50,
            line_range: 0..3,
            element_types: vec![DocElement::Paragraph],
        };
        assert_eq!(section.level, 2);
        assert_eq!(section.section_path.len(), 2);
        assert_eq!(section.section_path[1], "Installation");
    }

    #[test]
    fn symbol_ref_stores_name_and_range() {
        let sym = SymbolRef {
            name: "Engine::init".to_string(),
            byte_range: 10..22,
        };
        assert_eq!(sym.name, "Engine::init");
        assert_eq!(sym.byte_range, 10..22);
    }

    #[test]
    fn doc_element_equality() {
        assert_eq!(DocElement::Paragraph, DocElement::Paragraph);
        assert_eq!(
            DocElement::CodeBlock {
                language: Some("rust".to_string())
            },
            DocElement::CodeBlock {
                language: Some("rust".to_string())
            },
        );
        assert_ne!(DocElement::Paragraph, DocElement::Table);
    }
}
