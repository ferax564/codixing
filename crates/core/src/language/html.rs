//! HTML document parser using scraper.
//!
//! Parses HTML into `DocSection`s with heading hierarchy, section paths,
//! element type detection, and `<code>` tag symbol reference extraction.

use scraper::{ElementRef, Html, Selector};

use super::Language;
use super::doc::{DocElement, DocLanguageSupport, DocSection, SymbolRef};

/// HTML language support using scraper for section parsing.
pub struct HtmlLanguage;

impl DocLanguageSupport for HtmlLanguage {
    fn language(&self) -> Language {
        Language::Html
    }

    fn parse_sections(&self, source: &[u8]) -> Vec<DocSection> {
        let text = String::from_utf8_lossy(source);
        parse_html_sections(&text)
    }

    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef> {
        let text = String::from_utf8_lossy(source);
        let document = Html::parse_document(&text);
        // Combine backtick refs from raw text and <code> tag refs, deduplicated.
        let mut refs = super::markdown::extract_backtick_refs(&text);
        let code_refs = extract_code_tag_refs_from_doc(&document);
        for r in code_refs {
            if !refs.iter().any(|existing| existing.name == r.name) {
                refs.push(r);
            }
        }
        refs
    }
}

/// Parse HTML text into a flat list of `DocSection`s.
///
/// Each h1–h6 element becomes a section boundary. The heading hierarchy is
/// tracked to build `section_path` breadcrumbs (same algorithm as markdown).
/// If the document contains no headings, the entire body text is a single
/// level-0 section.
pub fn parse_html_sections(html: &str) -> Vec<DocSection> {
    let document = Html::parse_document(html);

    // --- Check whether the document has any headings ---
    let heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6").expect("valid selector");
    let has_headings = document.select(&heading_sel).next().is_some();

    // --- No-heading fallback ---
    if !has_headings {
        let body_sel = Selector::parse("body").expect("valid selector");
        let content = if let Some(body) = document.select(&body_sel).next() {
            body.text().collect::<Vec<_>>().join(" ").trim().to_string()
        } else {
            // Strip all tags naively by collecting all text nodes.
            document
                .root_element()
                .text()
                .collect::<String>()
                .trim()
                .to_string()
        };
        let byte_end = content.len();
        return vec![DocSection {
            heading: String::new(),
            level: 0,
            section_path: vec![],
            content,
            byte_range: 0..byte_end,
            line_range: 0..1,
            element_types: vec![DocElement::Paragraph],
        }];
    }

    // --- Build sections from heading hierarchy ---
    // Walk the document's *direct* children of <body> (or root) collecting text
    // between heading elements.  We use scraper's recursive `descendants()` on
    // the document and bucket text/elements into sections based on which heading
    // they follow.
    //
    // Strategy: serialise sections by iterating over children of <body> (or the
    // document root) in order, switching to a new section each time we encounter
    // an h1–h6.

    let mut heading_stack: Vec<(u8, String)> = Vec::new();
    let mut sections: Vec<DocSection> = Vec::new();

    // Build a set of heading element IDs so we can detect them while iterating.
    let heading_names: std::collections::HashSet<&str> = ["h1", "h2", "h3", "h4", "h5", "h6"]
        .iter()
        .copied()
        .collect();

    // We iterate over the root element's descendants and accumulate content
    // between heading boundaries.
    let root_el = document.root_element();

    // Current accumulator state.
    let mut current_heading: Option<(u8, String)> = None;
    let mut current_content_parts: Vec<String> = Vec::new();
    let mut current_elements: Vec<DocElement> = Vec::new();

    // We need a way to detect *direct* block-level children of the body/root
    // to interleave headings with content correctly.  We'll iterate over all
    // children of <html>/<body> at depth 1-2.
    //
    // Collect top-level block nodes under <body>.
    let body_sel = Selector::parse("body").expect("valid selector");
    let container: ElementRef = document.select(&body_sel).next().unwrap_or(root_el);

    for child in container.children() {
        if let Some(el) = ElementRef::wrap(child) {
            let tag = el.value().name();
            if heading_names.contains(tag) {
                // Flush previous section (if any).
                if let Some((lvl, heading_text)) = current_heading.take() {
                    // Update heading stack
                    heading_stack.retain(|(l, _)| *l < lvl);
                    heading_stack.push((lvl, heading_text.clone()));
                    let section_path: Vec<String> =
                        heading_stack.iter().map(|(_, t)| t.clone()).collect();
                    let content = current_content_parts.join(" ").trim().to_string();
                    let byte_end = content.len();
                    sections.push(DocSection {
                        heading: heading_text,
                        level: lvl,
                        section_path,
                        content,
                        byte_range: 0..byte_end,
                        line_range: 0..1,
                        element_types: std::mem::take(&mut current_elements),
                    });
                    current_content_parts.clear();
                }
                let lvl: u8 = tag.trim_start_matches('h').parse().unwrap_or(1);
                current_heading = Some((lvl, el.text().collect::<String>().trim().to_string()));
            } else {
                // Accumulate content.
                let text = el.text().collect::<String>();
                if !text.trim().is_empty() {
                    current_content_parts.push(text.trim().to_string());
                }
                // Detect element type.
                let el_type = detect_element_type(tag, &el);
                if let Some(et) = el_type {
                    if !current_elements.contains(&et) {
                        current_elements.push(et);
                    }
                }
            }
        } else if let Some(text) = child.value().as_text() {
            let t = text.trim();
            if !t.is_empty() {
                current_content_parts.push(t.to_string());
            }
        }
    }

    // Flush the last section.
    if let Some((lvl, heading_text)) = current_heading.take() {
        heading_stack.retain(|(l, _)| *l < lvl);
        heading_stack.push((lvl, heading_text.clone()));
        let section_path: Vec<String> = heading_stack.iter().map(|(_, t)| t.clone()).collect();
        let content = current_content_parts.join(" ").trim().to_string();
        let byte_end = content.len();
        sections.push(DocSection {
            heading: heading_text,
            level: lvl,
            section_path,
            content,
            byte_range: 0..byte_end,
            line_range: 0..1,
            element_types: std::mem::take(&mut current_elements),
        });
    }

    // If we found headings in the selector but the body-children loop produced
    // no sections (e.g., flat non-body document without <body> wrapper), fall
    // back to building sections purely from the heading list with adjacent text.
    if sections.is_empty() && has_headings {
        return build_sections_from_headings_list(&headings_info_from_doc(&document));
    }

    sections
}

/// Fallback: build sections from a simple list of (level, text) pairs.
/// Used when there is no <body> to iterate over.
fn build_sections_from_headings_list(headings: &[(u8, String, String)]) -> Vec<DocSection> {
    let mut sections = Vec::new();
    let mut heading_stack: Vec<(u8, String)> = Vec::new();
    for (lvl, text, content) in headings {
        heading_stack.retain(|(l, _)| *l < *lvl);
        heading_stack.push((*lvl, text.clone()));
        let section_path: Vec<String> = heading_stack.iter().map(|(_, t)| t.clone()).collect();
        let byte_end = content.len();
        sections.push(DocSection {
            heading: text.clone(),
            level: *lvl,
            section_path,
            content: content.clone(),
            byte_range: 0..byte_end,
            line_range: 0..1,
            element_types: vec![],
        });
    }
    sections
}

/// Collect headings with adjacent text for fallback mode.
fn headings_info_from_doc(document: &Html) -> Vec<(u8, String, String)> {
    let heading_sel = Selector::parse("h1, h2, h3, h4, h5, h6").expect("valid selector");
    document
        .select(&heading_sel)
        .map(|el| {
            let tag = el.value().name();
            let lvl: u8 = tag.trim_start_matches('h').parse().unwrap_or(1);
            let text = el.text().collect::<String>().trim().to_string();
            // Collect text from immediately following siblings until the next heading.
            let mut content_parts: Vec<String> = Vec::new();
            let heading_names: std::collections::HashSet<&str> =
                ["h1", "h2", "h3", "h4", "h5", "h6"]
                    .iter()
                    .copied()
                    .collect();
            let mut sib = el.next_sibling();
            while let Some(node) = sib {
                if let Some(sib_el) = ElementRef::wrap(node) {
                    if heading_names.contains(sib_el.value().name()) {
                        break;
                    }
                    let t = sib_el.text().collect::<String>();
                    if !t.trim().is_empty() {
                        content_parts.push(t.trim().to_string());
                    }
                } else if let Some(text_node) = node.value().as_text() {
                    let t = text_node.trim();
                    if !t.is_empty() {
                        content_parts.push(t.to_string());
                    }
                }
                sib = node.next_sibling();
            }
            (lvl, text, content_parts.join(" "))
        })
        .collect()
}

/// Detect the `DocElement` type for an HTML element.
fn detect_element_type(tag: &str, el: &ElementRef<'_>) -> Option<DocElement> {
    match tag {
        "p" => Some(DocElement::Paragraph),
        "pre" | "code" => {
            // Try to find a language hint from a class like "language-rust".
            let lang = el.value().attr("class").and_then(|cls| {
                cls.split_whitespace()
                    .find(|c| c.starts_with("language-"))
                    .map(|c| c.trim_start_matches("language-").to_string())
            });
            Some(DocElement::CodeBlock { language: lang })
        }
        "table" => Some(DocElement::Table),
        "ul" | "ol" => Some(DocElement::List),
        "blockquote" => Some(DocElement::BlockQuote),
        "img" => {
            let alt = el.value().attr("alt").unwrap_or("").to_string();
            Some(DocElement::Image { alt })
        }
        _ => None,
    }
}

/// Extract symbol references from `<code>` tags in a pre-parsed HTML document.
///
/// Uses `is_likely_symbol` and `clean_symbol_name` from the markdown module.
/// Deduplicates by name.
fn extract_code_tag_refs_from_doc(document: &Html) -> Vec<SymbolRef> {
    let code_sel = Selector::parse("code").expect("valid selector");

    let mut refs: Vec<SymbolRef> = Vec::new();
    for el in document.select(&code_sel) {
        let text = el.text().collect::<String>();
        let trimmed = text.trim();
        if super::markdown::is_likely_symbol(trimmed) {
            let name = super::markdown::clean_symbol_name(trimmed);
            if !refs.iter().any(|r: &SymbolRef| r.name == name) {
                refs.push(SymbolRef {
                    name,
                    byte_range: 0..trimmed.len(),
                });
            }
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_html_sections() {
        let html = "<html><body><h1>Title</h1><p>Intro.</p><h2>Section A</h2><p>Content A.</p></body></html>";
        let sections = parse_html_sections(html);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "Title");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[0].section_path, vec!["Title"]);
        assert_eq!(sections[1].heading, "Section A");
        assert_eq!(sections[1].section_path, vec!["Title", "Section A"]);
    }

    #[test]
    fn parse_html_no_headings() {
        let html = "<html><body><p>Just text.</p></body></html>";
        let sections = parse_html_sections(html);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].level, 0);
        assert!(sections[0].content.contains("Just text"));
    }

    #[test]
    fn extract_code_tag_symbols() {
        let html = "<p>Use <code>Engine::init()</code> to start. See <code>add_chunk</code>.</p>";
        let document = Html::parse_document(html);
        let refs = extract_code_tag_refs_from_doc(&document);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].name, "Engine::init");
        assert_eq!(refs[1].name, "add_chunk");
    }

    #[test]
    fn html_nested_headings() {
        let html = "<h1>Root</h1><h2>Child</h2><h3>Grand</h3><h2>Sib</h2>";
        let sections = parse_html_sections(html);
        assert_eq!(sections.len(), 4);
        assert_eq!(sections[2].section_path, vec!["Root", "Child", "Grand"]);
        assert_eq!(sections[3].section_path, vec!["Root", "Sib"]);
    }
}
