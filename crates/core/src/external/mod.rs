//! External-context importers.
//!
//! Codixing's default surface indexes code, docs, and specs found on the
//! filesystem. Real projects also carry context that lives *outside* the
//! source tree: GitHub issues and pull requests, architecture decision
//! records (ADRs), and tracker exports (Jira/Linear). This module ingests
//! local exports of that context into the same index as first-class,
//! searchable documents — preserving Codixing's offline, no-SaaS story.
//!
//! ## Model
//!
//! Each imported item is normalized into an [`ExternalDocument`]: a `source`
//! namespace (e.g. `"github"`), a stable `id`, a `title`, a `body`, and an
//! ordered list of `metadata` key/value pairs. The engine renders each
//! document to a self-contained Markdown blob and indexes it under a virtual
//! path (`_external/<source>/<id>.md`) via the existing document pipeline, so
//! imported context flows through BM25, trigram, vector search, the doc→code
//! symbol graph, and persistence with no parallel store.
//!
//! ## Sources
//!
//! - [`github`] — GitHub issues/PRs, from `gh issue list --json …` output or a
//!   GitHub REST API JSON array.
//! - [`adr`] — a folder (or single file) of Markdown architecture decision
//!   records.
//!
//! Jira/Linear CSV/JSON exports are a planned follow-up; the
//! [`ExternalDocument`] shape and [`parse_source`] dispatch are designed so a
//! new parser slots in without touching the engine.

pub mod adr;
pub mod github;

use std::path::Path;

use crate::error::{CodixingError, Result};

/// Virtual path prefix under which all imported documents are indexed.
///
/// Imported documents are not on the filesystem, so they are stored under a
/// reserved prefix that the normal file walk and `sync` removal detection
/// never produce. This keeps them invisible to disk-based change detection
/// (they survive `sync`) while remaining indistinguishable from real
/// Markdown docs to search, grep, and the graph.
pub const EXTERNAL_PATH_PREFIX: &str = "_external/";

/// A normalized unit of external project context.
///
/// Produced by the source parsers ([`github`], [`adr`]) and consumed by
/// [`crate::engine::Engine::import_external`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalDocument {
    /// Source namespace, e.g. `"github"`, `"adr"`. Becomes the second path
    /// segment of the virtual path and the value matched by `--source`.
    pub source: String,
    /// Stable identifier within the source (e.g. `"issue-123"`, `"0007"`).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Body / description text (Markdown or plain text).
    pub body: String,
    /// Ordered metadata rendered into a header block (state, author, labels,
    /// url, timestamps, …). Order is preserved for stable output.
    pub metadata: Vec<(String, String)>,
}

impl ExternalDocument {
    /// Build a document with no metadata.
    pub fn new(
        source: impl Into<String>,
        id: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            id: id.into(),
            title: title.into(),
            body: body.into(),
            metadata: Vec::new(),
        }
    }

    /// Add a metadata key/value pair (skipped when the value is empty).
    pub fn with_meta(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.is_empty() {
            self.metadata.push((key.into(), value));
        }
        self
    }

    /// Virtual relative path used as the index `file_path` for this document,
    /// e.g. `_external/github/issue-123.md`. The `id` is sanitized to a safe
    /// path slug.
    pub fn virtual_path(&self) -> String {
        format!(
            "{EXTERNAL_PATH_PREFIX}{}/{}.md",
            slugify(&self.source),
            slugify(&self.id)
        )
    }

    /// Render the document to a self-contained Markdown blob: an `# <title>`
    /// heading, a metadata bullet list, and the body. The Markdown layout lets
    /// the document pipeline extract backticked code symbols from the body for
    /// doc→code graph edges.
    pub fn to_markdown(&self) -> String {
        let mut out = String::with_capacity(self.body.len() + 256);
        let title = self.title.trim();
        if title.is_empty() {
            out.push_str(&format!("# {}\n\n", self.id));
        } else {
            out.push_str(&format!("# {title}\n\n"));
        }
        if !self.metadata.is_empty() {
            for (k, v) in &self.metadata {
                out.push_str(&format!("- **{k}**: {v}\n"));
            }
            out.push('\n');
        }
        let body = self.body.trim();
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        out
    }
}

/// Parse a source export into normalized documents.
///
/// `source` selects the parser; `path` is a file or directory depending on the
/// source. Returns an error for unknown source names so the CLI can report the
/// supported set.
pub fn parse_source(source: &str, path: &Path) -> Result<Vec<ExternalDocument>> {
    match source.to_ascii_lowercase().as_str() {
        "github" | "github-issues" | "gh" => github::parse(path),
        "adr" | "adrs" => adr::parse(path),
        other => Err(CodixingError::Import(format!(
            "unknown import source '{other}' (supported: github, adr)"
        ))),
    }
}

/// Sanitize an arbitrary string into a path-safe slug: keep alphanumerics,
/// `.`, `_`, `-`; collapse everything else to `-`; lowercase; trim dashes.
pub(crate) fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "item".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_path_uses_source_and_slugged_id() {
        let doc = ExternalDocument::new("github", "issue-123", "Crash on init", "body");
        assert_eq!(doc.virtual_path(), "_external/github/issue-123.md");
    }

    #[test]
    fn virtual_path_slugs_unsafe_ids() {
        let doc = ExternalDocument::new("Jira", "PROJ 42/sub", "T", "b");
        assert_eq!(doc.virtual_path(), "_external/jira/proj-42-sub.md");
    }

    #[test]
    fn to_markdown_renders_title_metadata_and_body() {
        let doc = ExternalDocument::new("github", "issue-7", "Fix `Engine::init`", "It panics.")
            .with_meta("state", "open")
            .with_meta("author", "alice")
            .with_meta("labels", "");
        let md = doc.to_markdown();
        assert!(md.starts_with("# Fix `Engine::init`\n\n"));
        assert!(md.contains("- **state**: open\n"));
        assert!(md.contains("- **author**: alice\n"));
        // Empty metadata values are skipped.
        assert!(!md.contains("labels"));
        assert!(md.contains("It panics."));
    }

    #[test]
    fn to_markdown_falls_back_to_id_when_title_blank() {
        let doc = ExternalDocument::new("adr", "0007", "   ", "Decision text");
        assert!(doc.to_markdown().starts_with("# 0007\n\n"));
    }

    #[test]
    fn slugify_handles_empty_and_symbols() {
        assert_eq!(slugify("///"), "item");
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("issue_42.v2"), "issue_42.v2");
    }

    #[test]
    fn parse_source_rejects_unknown() {
        let err = parse_source("notion", Path::new("/tmp/x")).unwrap_err();
        assert!(err.to_string().contains("unknown import source"));
    }
}
