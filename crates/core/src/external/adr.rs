//! Architecture Decision Record (ADR) importer.
//!
//! Accepts either a single Markdown file or a directory of them (the common
//! `docs/adr/` or `doc/adr/` layout). Each `.md`/`.markdown` file becomes one
//! [`ExternalDocument`]. Supports the two prevalent ADR styles:
//!
//! - **Nygard** — `# 1. Title` heading, with `## Status`, `## Context`,
//!   `## Decision`, `## Consequences` sections.
//! - **MADR** — `# Title` heading with a `* Status: accepted` (or
//!   `Status: accepted`) line near the top.
//!
//! The title is taken from the first ATX (`#`) heading; a leading record number
//! (`# 0007. Title` / `# 7: Title`) is stripped from the title and reused as the
//! id when present. Otherwise the filename stem is the id.

use std::path::Path;

use super::ExternalDocument;
use crate::error::Result;

/// Parse an ADR file or directory at `path` into documents.
pub fn parse(path: &Path) -> Result<Vec<ExternalDocument>> {
    if path.is_dir() {
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| is_markdown(p))
            .collect();
        files.sort();
        let mut docs = Vec::with_capacity(files.len());
        for file in &files {
            if let Some(doc) = parse_file(file)? {
                docs.push(doc);
            }
        }
        Ok(docs)
    } else {
        Ok(parse_file(path)?.into_iter().collect())
    }
}

fn is_markdown(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    )
}

/// Parse one ADR file. Returns `None` for an empty file.
fn parse_file(path: &Path) -> Result<Option<ExternalDocument>> {
    let text = std::fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("adr")
        .to_string();

    let (heading_number, title) = first_heading(&text)
        .map(|h| split_record_number(&h))
        .unwrap_or((None, stem.clone()));

    // Prefer an explicit record number from the heading; fall back to a number
    // embedded in the filename (e.g. `0007-use-rust.md`); else the stem.
    let id = heading_number
        .or_else(|| leading_number(&stem))
        .unwrap_or_else(|| stem.clone());

    let status = extract_status(&text).unwrap_or_default();

    let doc = ExternalDocument::new("adr", id, title, text)
        .with_meta("status", status)
        .with_meta("file", stem);
    Ok(Some(doc))
}

/// The text of the first ATX heading (`# …`), trimmed.
fn first_heading(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let heading = rest.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }
    None
}

/// Split a leading record number off a heading: `"7. Use Rust"` →
/// `(Some("7"), "Use Rust")`. Accepts `.`, `:`, `-`, or `)` separators.
fn split_record_number(heading: &str) -> (Option<String>, String) {
    let bytes = heading.as_bytes();
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return (None, heading.to_string());
    }
    let (num, rest) = heading.split_at(digits);
    let rest = rest.trim_start_matches(['.', ':', '-', ')', ' ']);
    if rest.is_empty() {
        (Some(num.to_string()), heading.to_string())
    } else {
        (Some(num.to_string()), rest.to_string())
    }
}

/// A leading numeric run in a filename stem (`0007-use-rust` → `0007`).
fn leading_number(stem: &str) -> Option<String> {
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// Find an ADR status value. Matches a `## Status` section's first non-empty
/// line (Nygard) or an inline `Status: …` / `* Status: …` line (MADR).
fn extract_status(text: &str) -> Option<String> {
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();

        // Nygard: `## Status` heading, value on a following line.
        if lower.trim_start_matches('#').trim() == "status" && lower.starts_with('#') {
            for next in lines.by_ref() {
                let val = next.trim();
                if !val.is_empty() {
                    return Some(val.trim_start_matches(['*', '-', ' ']).trim().to_string());
                }
            }
            return None;
        }

        // MADR: `Status: accepted` or `* Status: accepted`.
        let inline = trimmed.trim_start_matches(['*', '-', ' ']);
        if let Some(rest) = inline.to_ascii_lowercase().strip_prefix("status:") {
            let _ = rest;
            if let Some(idx) = inline.to_ascii_lowercase().find("status:") {
                let val = inline[idx + "status:".len()..].trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn splits_nygard_record_number() {
        assert_eq!(
            split_record_number("7. Use Rust"),
            (Some("7".to_string()), "Use Rust".to_string())
        );
        assert_eq!(
            split_record_number("0007: Use Rust"),
            (Some("0007".to_string()), "Use Rust".to_string())
        );
        assert_eq!(
            split_record_number("No number here"),
            (None, "No number here".to_string())
        );
    }

    #[test]
    fn extracts_nygard_status_section() {
        let text = "# 1. Decision\n\n## Status\n\nAccepted\n\n## Context\n\nstuff\n";
        assert_eq!(extract_status(text).as_deref(), Some("Accepted"));
    }

    #[test]
    fn extracts_madr_inline_status() {
        let text = "# Use Rust\n\n* Status: accepted\n\n## Context\n";
        assert_eq!(extract_status(text).as_deref(), Some("accepted"));
    }

    #[test]
    fn parses_directory_of_adrs() {
        let dir = tempfile::tempdir().unwrap();
        let mut f1 = std::fs::File::create(dir.path().join("0001-use-rust.md")).unwrap();
        writeln!(
            f1,
            "# 1. Use Rust\n\n## Status\n\nAccepted\n\nWe pick `Engine`."
        )
        .unwrap();
        let mut f2 = std::fs::File::create(dir.path().join("0002-use-tantivy.md")).unwrap();
        writeln!(f2, "# 2. Use Tantivy\n\n* Status: proposed\n").unwrap();
        // A non-markdown file is ignored.
        std::fs::File::create(dir.path().join("README.txt")).unwrap();

        let docs = parse(dir.path()).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].id, "1");
        assert_eq!(docs[0].title, "Use Rust");
        assert!(
            docs[0]
                .metadata
                .iter()
                .any(|(k, v)| k == "status" && v == "Accepted")
        );
        assert_eq!(docs[1].id, "2");
        assert!(
            docs[1]
                .metadata
                .iter()
                .any(|(k, v)| k == "status" && v == "proposed")
        );
    }

    #[test]
    fn single_file_without_heading_uses_stem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "Just some prose, no heading.").unwrap();
        let docs = parse(&path).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id, "notes");
        assert_eq!(docs[0].title, "notes");
    }
}
