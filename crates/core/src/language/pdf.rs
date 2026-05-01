//! PDF text-extraction language support (feature-gated: `--features pdf`).
//!
//! Backed by the pure-Rust `pdf-extract` crate — no C dependencies, so
//! the opt-in build stays compatible with the project's "one Rust
//! binary, zero deps" promise.
//!
//! Output shape: one [`DocSection`] per page. Page boundaries are
//! detected via the form-feed `\x0C` character `pdf-extract` inserts
//! between pages. Section headings read `"Page N"`. Byte ranges point
//! into the extracted text (not the raw PDF bytes), so they are useful
//! for navigation but NOT for edits back into the PDF.
//!
//! Out of scope (future work):
//! - Image / figure extraction (OCR is deferred).
//! - Structure-aware heading detection via the PDF outline tree.
//! - Encrypted PDFs — pdf-extract returns an error, which we map to
//!   an empty section list.

use std::cell::RefCell;
use std::sync::Arc;

use super::Language;
use super::doc::{DocElement, DocLanguageSupport, DocSection, SymbolRef, build_line_offsets};
use super::markdown::is_likely_symbol;

/// PDF document language support.
pub struct PdfLanguage;

impl DocLanguageSupport for PdfLanguage {
    fn language(&self) -> Language {
        Language::Pdf
    }

    fn parse_sections(&self, source: &[u8], _file_name: Option<&str>) -> Vec<DocSection> {
        let Some(text) = extract_cached(source) else {
            return Vec::new();
        };
        parse_pdf_sections(&text)
    }

    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef> {
        let Some(text) = extract_cached(source) else {
            return Vec::new();
        };
        extract_pdf_symbol_refs(&text)
    }
}

/// Form-feed — pdf-extract's per-page separator.
const PAGE_SEPARATOR: char = '\x0C';

/// One-entry per-thread memoization of the last extracted PDF.
///
/// The indexer calls `parse_sections` and `extract_symbol_refs` back-to-back
/// on the same `source` (see `engine::indexing::process_doc_file`). Without
/// this cache the 50-400 ms `pdf-extract` call would run twice per PDF.
/// Keyed on xxh3 of the source bytes — safe across pointer reuse.
fn extract_cached(source: &[u8]) -> Option<Arc<str>> {
    thread_local! {
        static CACHE: RefCell<Option<(u64, Arc<str>)>> = const { RefCell::new(None) };
    }

    let hash = xxhash_rust::xxh3::xxh3_64(source);
    if let Some(hit) = CACHE.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|(h, s)| (*h == hash).then(|| s.clone()))
    }) {
        return Some(hit);
    }

    let mut text = pdf_extract::extract_text_from_mem(source).ok()?;
    // Drop trailing whitespace in-place so the last page's byte range
    // doesn't overshoot the real text.
    let trimmed_len = text.trim_end().len();
    text.truncate(trimmed_len);
    let arc: Arc<str> = Arc::from(text);

    CACHE.with(|c| *c.borrow_mut() = Some((hash, arc.clone())));
    Some(arc)
}

/// Split the extracted text on form-feed boundaries and emit one
/// [`DocSection`] per non-empty page.
fn parse_pdf_sections(text: &str) -> Vec<DocSection> {
    if text.is_empty() {
        return Vec::new();
    }

    let offsets = build_line_offsets(text);
    let mut sections = Vec::new();
    let mut page_number: usize = 1;
    let mut byte_cursor: usize = 0;

    for raw_page_text in text.split(PAGE_SEPARATOR) {
        let raw_len = raw_page_text.len();
        let page_text = raw_page_text.trim();

        if !page_text.is_empty() {
            let byte_start = byte_cursor;
            let byte_end = byte_cursor + raw_len;
            let line_start = line_byte_offset_to_line(&offsets, byte_start);
            let line_end = line_byte_offset_to_line(&offsets, byte_end);

            let heading = format!("Page {page_number}");
            sections.push(DocSection {
                heading: heading.clone(),
                level: 1,
                section_path: vec![heading],
                content: page_text.to_string(),
                byte_range: byte_start..byte_end,
                line_range: line_start..line_end,
                element_types: vec![DocElement::Paragraph],
            });
        }

        byte_cursor += raw_len + PAGE_SEPARATOR.len_utf8();
        page_number += 1;
    }

    sections
}

/// Byte offset → 0-indexed line number using the precomputed table
/// from [`build_line_offsets`]. Clamps to the text length (consumed
/// via `line_byte_offset`) so end-of-file queries don't panic.
fn line_byte_offset_to_line(offsets: &[usize], byte: usize) -> usize {
    // Binary search in the offsets table; Ok(i) is an exact match, Err(i)
    // is the insertion point, so the line containing `byte` is i-1.
    match offsets.binary_search(&byte) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
    .min(offsets.len().saturating_sub(1))
}

/// Scan the extracted text for tokens that look like code symbols.
///
/// PDF text rarely preserves backticks through extraction, so we can't
/// reuse [`markdown::extract_backtick_refs`]. Instead, tokenize on
/// byte boundaries (`_`, `:`, ASCII alphanumerics are all < 0x80 so
/// multi-byte UTF-8 characters naturally act as separators without
/// breaking byte-offset arithmetic) and filter with
/// [`is_likely_symbol`] — the same heuristic the markdown, RST, and
/// HTML parsers apply.
fn extract_pdf_symbol_refs(text: &str) -> Vec<SymbolRef> {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut refs = Vec::new();
    let mut i = 0usize;

    while i < len {
        // Skip non-token bytes.
        while i < len && !is_token_byte(bytes[i]) {
            i += 1;
        }
        let start = i;
        while i < len && is_token_byte(bytes[i]) {
            i += 1;
        }
        if start == i {
            break;
        }
        let token = &text[start..i];
        if is_likely_symbol(token) {
            refs.push(SymbolRef {
                name: token.to_string(),
                byte_range: start..i,
            });
        }
    }

    refs
}

/// Token byte: ASCII alphanumeric, `_`, or `:`. Non-ASCII bytes
/// (≥ 0x80) always return false, which correctly treats multi-byte
/// UTF-8 characters as separators in the byte-level scan.
#[inline]
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_pdf_returns_empty_not_panic() {
        let sections = PdfLanguage.parse_sections(b"not a pdf at all", None);
        assert!(sections.is_empty());
        let refs = PdfLanguage.extract_symbol_refs(b"not a pdf at all");
        assert!(refs.is_empty());
    }

    #[test]
    fn empty_input_returns_empty() {
        let sections = PdfLanguage.parse_sections(b"", None);
        assert!(sections.is_empty());
    }

    #[test]
    fn page_separator_splits_multi_page_text() {
        let synth = format!(
            "Page one content.\n\n{FF}Page two content.\n\n{FF}Page three content.",
            FF = PAGE_SEPARATOR
        );
        let sections = parse_pdf_sections(&synth);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Page 1");
        assert!(sections[0].content.starts_with("Page one"));
        assert_eq!(sections[1].heading, "Page 2");
        assert!(sections[1].content.starts_with("Page two"));
        assert_eq!(sections[2].heading, "Page 3");
        assert!(sections[2].content.starts_with("Page three"));
        // Ranges stay strictly increasing.
        assert!(sections[0].byte_range.end <= sections[1].byte_range.start);
        assert!(sections[1].byte_range.end <= sections[2].byte_range.start);
    }

    #[test]
    fn symbol_refs_extracted_from_prose() {
        let text = "See Engine::init and the add_chunk helper. The ChunkConfig \
                    struct has a max_chars field.";
        let refs = extract_pdf_symbol_refs(text);
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"Engine::init"), "got {names:?}");
        assert!(names.contains(&"add_chunk"), "got {names:?}");
        assert!(names.contains(&"ChunkConfig"), "got {names:?}");
        assert!(names.contains(&"max_chars"), "got {names:?}");
        // All-caps words and plain English do not leak in.
        assert!(!names.contains(&"PDF"));
        assert!(!names.contains(&"struct"));
    }

    #[test]
    fn symbol_ref_byte_ranges_are_utf8_safe() {
        // Em-dashes and curly quotes are 3-byte and 3-byte UTF-8 sequences
        // respectively — common in extracted PDF prose. The scanner must
        // treat them as separators without drifting byte offsets.
        let text = "Use add_chunk — it returns Ok. ChunkConfig\u{2019}s max_chars wins.";
        let refs = extract_pdf_symbol_refs(text);
        for r in &refs {
            assert_eq!(
                &text[r.byte_range.clone()],
                r.name,
                "byte_range must slice back to the name exactly",
            );
        }
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"add_chunk"));
        assert!(names.contains(&"ChunkConfig"));
        assert!(names.contains(&"max_chars"));
    }

    /// Real round-trip test against a checked-in PDF generated by reportlab.
    ///
    /// `crates/core/tests/fixtures/minimal.pdf` is a 2-page PDF with real
    /// code references. Synthetic byte-fixture PDFs were attempted in the
    /// v0.41 ship but didn't parse reliably through `pdf-extract`; only a
    /// real binary fixture catches a `pdf-extract` version regression
    /// where extraction silently returns the empty string.
    ///
    /// Note: `pdf-extract` 0.9 does NOT emit form-feed page separators
    /// for reportlab-produced PDFs (verified against this fixture; pages
    /// concatenate with `\n\n`). Other generators (poppler, tools that
    /// embed `\x0C` in the content stream) do trigger the splitter. The
    /// `parse_pdf_sections` page-split logic stays correct for the latter
    /// — it just degrades to a single section here. We assert the
    /// page-merged behavior we actually observe, plus the symbol-ref
    /// extraction across the full text, so a regression in either path
    /// fails this test.
    #[test]
    fn real_pdf_round_trip_extracts_content_and_symbol_refs() {
        let bytes = include_bytes!("../../tests/fixtures/minimal.pdf");

        let sections = PdfLanguage.parse_sections(bytes, Some("minimal.pdf"));
        assert!(
            !sections.is_empty(),
            "pdf-extract regression: no sections emitted"
        );

        // All canonical phrases land somewhere in the extracted text.
        let combined: String = sections.iter().map(|s| s.content.as_str()).collect();
        for needle in [
            "Engine::init",
            "add_chunk",
            "ChunkConfig",
            "sync_index",
            "SymbolTable::insert",
        ] {
            assert!(
                combined.contains(needle),
                "missing `{needle}` in extracted PDF text:\n{combined}"
            );
        }

        // Symbol-ref scanner finds the same 5 symbols.
        let refs = PdfLanguage.extract_symbol_refs(bytes);
        let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
        for expected in [
            "Engine::init",
            "add_chunk",
            "ChunkConfig",
            "sync_index",
            "SymbolTable::insert",
        ] {
            assert!(
                names.contains(&expected),
                "missing {expected} in extracted refs: {names:?}"
            );
        }
    }
}
