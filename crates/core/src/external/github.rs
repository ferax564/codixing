//! GitHub issue / pull-request importer.
//!
//! Accepts a JSON file produced by either:
//!
//! - the GitHub CLI: `gh issue list --json number,title,body,state,author,labels,createdAt,updatedAt,url,comments`
//!   (also `gh pr list --json …`), or
//! - the GitHub REST API (`GET /repos/{owner}/{repo}/issues`) — a JSON array of
//!   issue objects with `user`, `html_url`, `created_at`, … fields.
//!
//! The two shapes use different field names for the same data, so parsing is
//! defensive: each field is looked up under every known alias. A top-level
//! array is expected; an object with an `"items"` array (GitHub search API) is
//! also accepted.

use std::path::Path;

use serde_json::Value;

use super::ExternalDocument;
use crate::error::{CodixingError, Result};

/// Parse a GitHub issues/PRs JSON export at `path` into documents.
pub fn parse(path: &Path) -> Result<Vec<ExternalDocument>> {
    let bytes = std::fs::read(path)?;
    parse_bytes(&bytes)
}

/// Parse raw JSON bytes (separated for testability).
pub fn parse_bytes(bytes: &[u8]) -> Result<Vec<ExternalDocument>> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| CodixingError::Import(format!("invalid GitHub JSON: {e}")))?;

    let items = match &root {
        Value::Array(items) => items.clone(),
        // GitHub search API wraps results in { "items": [...] }.
        Value::Object(map) => match map.get("items") {
            Some(Value::Array(items)) => items.clone(),
            _ => {
                return Err(CodixingError::Import(
                    "expected a JSON array of issues/PRs (or an object with an `items` array)"
                        .to_string(),
                ));
            }
        },
        _ => {
            return Err(CodixingError::Import(
                "expected a JSON array of issues/PRs".to_string(),
            ));
        }
    };

    Ok(items.iter().filter_map(parse_item).collect())
}

/// Convert a single issue/PR JSON object into an [`ExternalDocument`].
/// Returns `None` for objects with neither a number nor a title.
fn parse_item(item: &Value) -> Option<ExternalDocument> {
    let obj = item.as_object()?;

    let number = obj.get("number").and_then(Value::as_u64);
    let title = str_field(obj, &["title"]).unwrap_or_default();
    if number.is_none() && title.is_empty() {
        return None;
    }

    // A pull request is an issue object carrying a `pull_request` key (REST) or
    // an `isPullRequest` flag; `gh pr list` items have a `headRefName`.
    let is_pr = obj.contains_key("pull_request")
        || obj.get("isPullRequest").and_then(Value::as_bool) == Some(true)
        || obj.contains_key("headRefName");
    let kind = if is_pr { "pr" } else { "issue" };

    let id = match number {
        Some(n) => format!("{kind}-{n}"),
        None => format!("{kind}-{}", super::slugify(&title)),
    };

    let body = str_field(obj, &["body", "bodyText"]).unwrap_or_default();
    let state = str_field(obj, &["state"]).unwrap_or_default();
    let author = author_login(obj);
    let labels = label_names(obj);
    let url = str_field(obj, &["url", "html_url", "htmlUrl"]).unwrap_or_default();
    let created = str_field(obj, &["createdAt", "created_at"]).unwrap_or_default();
    let updated = str_field(obj, &["updatedAt", "updated_at"]).unwrap_or_default();

    // Append comments (if exported) as a trailing section so their text is
    // searchable and their code references feed the doc→code graph.
    let mut full_body = body;
    let comments = comment_blocks(obj);
    if !comments.is_empty() {
        if !full_body.is_empty() {
            full_body.push_str("\n\n");
        }
        full_body.push_str("## Comments\n\n");
        full_body.push_str(&comments.join("\n\n"));
    }

    let kind_label = if is_pr { "pull request" } else { "issue" };
    Some(
        ExternalDocument::new("github", id, title, full_body)
            .with_meta("type", kind_label)
            .with_meta("state", state.to_lowercase())
            .with_meta("author", author)
            .with_meta("labels", labels.join(", "))
            .with_meta("url", url)
            .with_meta("created", created)
            .with_meta("updated", updated),
    )
}

/// First non-empty string value among the candidate keys.
fn str_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = obj.get(*key).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Resolve the author login across `gh` (`author.login`) and REST (`user.login`)
/// shapes; falls back to a bare `author` string.
fn author_login(obj: &serde_json::Map<String, Value>) -> String {
    for key in ["author", "user"] {
        match obj.get(key) {
            Some(Value::Object(a)) => {
                if let Some(login) = a.get("login").and_then(Value::as_str) {
                    if !login.is_empty() {
                        return login.to_string();
                    }
                }
            }
            Some(Value::String(s)) if !s.is_empty() => return s.clone(),
            _ => {}
        }
    }
    String::new()
}

/// Extract label names from `[{ "name": "bug" }]` (both `gh` and REST use this).
fn label_names(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    let Some(Value::Array(labels)) = obj.get("labels") else {
        return Vec::new();
    };
    labels
        .iter()
        .filter_map(|l| match l {
            Value::Object(m) => m.get("name").and_then(Value::as_str).map(str::to_string),
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Render exported comments as `**login**: body` blocks.
fn comment_blocks(obj: &serde_json::Map<String, Value>) -> Vec<String> {
    let Some(Value::Array(comments)) = obj.get("comments") else {
        return Vec::new();
    };
    comments
        .iter()
        .filter_map(|c| {
            let m = c.as_object()?;
            let body = str_field(m, &["body", "bodyText"])?;
            let who = author_login(m);
            if who.is_empty() {
                Some(body)
            } else {
                Some(format!("**{who}**: {body}"))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gh_cli_shape() {
        let json = r#"[
          {
            "number": 42,
            "title": "Crash in Engine::init",
            "body": "Calling `Engine::init` panics on empty repos.",
            "state": "OPEN",
            "author": {"login": "alice"},
            "labels": [{"name": "bug"}, {"name": "p1"}],
            "url": "https://github.com/o/r/issues/42",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-02T00:00:00Z",
            "comments": [{"author": {"login": "bob"}, "body": "Confirmed on main."}]
          }
        ]"#;
        let docs = parse_bytes(json.as_bytes()).unwrap();
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.source, "github");
        assert_eq!(d.id, "issue-42");
        assert_eq!(d.title, "Crash in Engine::init");
        assert!(d.body.contains("Engine::init"));
        assert!(d.body.contains("## Comments"));
        assert!(d.body.contains("**bob**: Confirmed on main."));
        assert!(d.metadata.iter().any(|(k, v)| k == "state" && v == "open"));
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "author" && v == "alice")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "labels" && v == "bug, p1")
        );
        assert!(d.metadata.iter().any(|(k, v)| k == "type" && v == "issue"));
    }

    #[test]
    fn parses_rest_api_shape_and_detects_pr() {
        let json = r#"[
          {
            "number": 7,
            "title": "Add import command",
            "body": "Implements `import`.",
            "state": "closed",
            "user": {"login": "carol"},
            "labels": [{"name": "feature"}],
            "html_url": "https://github.com/o/r/pull/7",
            "created_at": "2026-02-01T00:00:00Z",
            "updated_at": "2026-02-03T00:00:00Z",
            "pull_request": {"url": "..."}
          }
        ]"#;
        let docs = parse_bytes(json.as_bytes()).unwrap();
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.id, "pr-7");
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "type" && v == "pull request")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "author" && v == "carol")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "url" && v.contains("pull/7"))
        );
    }

    #[test]
    fn accepts_search_api_items_wrapper() {
        let json = r#"{"total_count": 1, "items": [{"number": 1, "title": "X", "state": "open"}]}"#;
        let docs = parse_bytes(json.as_bytes()).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].id, "issue-1");
    }

    #[test]
    fn skips_empty_items_and_rejects_non_array() {
        let docs = parse_bytes(br#"[{"labels": []}]"#).unwrap();
        assert!(docs.is_empty());
        assert!(parse_bytes(b"\"nope\"").is_err());
        assert!(parse_bytes(b"not json").is_err());
    }
}
