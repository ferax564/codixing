//! Jira issue importer.
//!
//! Accepts either:
//!
//! - **CSV** — the Jira issue-navigator "Export → CSV" output. Jira repeats
//!   column headers (`Labels`, `Component`, …) for multi-value fields, which
//!   the CSV reader's [`crate::external::csv::Table::cols`] handles.
//! - **JSON** — a REST API search response (`GET /rest/api/2/search` →
//!   `{ "issues": [ { "key", "fields": { … } } ] }`) or a bare array of issue
//!   objects. Descriptions in the v3 REST API use Atlassian Document Format
//!   (ADF, a JSON tree); their text is extracted best-effort.
//!
//! Format is auto-detected from the first non-whitespace byte (`{`/`[` → JSON).

use std::path::Path;

use serde_json::Value;

use super::ExternalDocument;
use super::csv::Table;
use crate::error::{CodixingError, Result};

/// Parse a Jira export at `path` into documents.
pub fn parse(path: &Path) -> Result<Vec<ExternalDocument>> {
    let text = std::fs::read_to_string(path)?;
    parse_str(&text)
}

/// Parse Jira export text, auto-detecting JSON vs CSV.
pub fn parse_str(text: &str) -> Result<Vec<ExternalDocument>> {
    match text.trim_start().chars().next() {
        Some('{') | Some('[') => parse_json(text),
        _ => parse_csv(text),
    }
}

fn parse_json(text: &str) -> Result<Vec<ExternalDocument>> {
    let root: Value = serde_json::from_str(text)
        .map_err(|e| CodixingError::Import(format!("invalid Jira JSON: {e}")))?;
    let issues = match &root {
        Value::Array(items) => items.clone(),
        Value::Object(map) => match map.get("issues") {
            Some(Value::Array(items)) => items.clone(),
            _ => {
                return Err(CodixingError::Import(
                    "expected a Jira search response with an `issues` array".to_string(),
                ));
            }
        },
        _ => {
            return Err(CodixingError::Import(
                "expected a Jira JSON object or array".to_string(),
            ));
        }
    };
    Ok(issues.iter().filter_map(json_issue).collect())
}

fn json_issue(item: &Value) -> Option<ExternalDocument> {
    let obj = item.as_object()?;
    let key = obj.get("key").and_then(Value::as_str).unwrap_or_default();
    let fields = obj.get("fields").and_then(Value::as_object);

    let summary = fields
        .and_then(|f| f.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if key.is_empty() && summary.is_empty() {
        return None;
    }

    let description = fields
        .and_then(|f| f.get("description"))
        .map(adf_text)
        .unwrap_or_default();
    let status = fields
        .and_then(|f| f.get("status"))
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let assignee = person(fields.and_then(|f| f.get("assignee")));
    let reporter = person(fields.and_then(|f| f.get("reporter")));
    let issue_type = fields
        .and_then(|f| f.get("issuetype"))
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let priority = fields
        .and_then(|f| f.get("priority"))
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let labels = fields
        .and_then(|f| f.get("labels"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let created = fields
        .and_then(|f| f.get("created"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let updated = fields
        .and_then(|f| f.get("updated"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let url = browse_url(obj.get("self").and_then(Value::as_str), key);

    let id = if key.is_empty() {
        super::slugify(&summary)
    } else {
        key.to_string()
    };
    Some(build(
        id,
        summary,
        description,
        JiraMeta {
            issue_type,
            status,
            priority,
            assignee: &assignee,
            reporter: &reporter,
            labels: &labels,
            created,
            updated,
            url: &url,
        },
    ))
}

fn parse_csv(text: &str) -> Result<Vec<ExternalDocument>> {
    let table = Table::from_csv(text)
        .ok_or_else(|| CodixingError::Import("empty Jira CSV (no header row)".to_string()))?;
    let key_col = table.col(&["issue key", "key"]);
    let summary_col = table.col(&["summary"]);
    let desc_col = table.col(&["description"]);
    let type_col = table.col(&["issue type", "issuetype"]);
    let status_col = table.col(&["status"]);
    let priority_col = table.col(&["priority"]);
    let assignee_col = table.col(&["assignee"]);
    let reporter_col = table.col(&["reporter"]);
    let created_col = table.col(&["created"]);
    let updated_col = table.col(&["updated"]);
    let label_cols = table.cols(&["labels"]);

    let mut docs = Vec::with_capacity(table.rows.len());
    for row in &table.rows {
        let key = table.get(row, key_col);
        let summary = table.get(row, summary_col);
        if key.is_empty() && summary.is_empty() {
            continue;
        }
        let labels = label_cols
            .iter()
            .filter_map(|&i| row.get(i).map(|s| s.trim()))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let id = if key.is_empty() {
            super::slugify(summary)
        } else {
            key.to_string()
        };
        docs.push(build(
            id,
            summary.to_string(),
            table.get(row, desc_col).to_string(),
            JiraMeta {
                issue_type: table.get(row, type_col),
                status: table.get(row, status_col),
                priority: table.get(row, priority_col),
                assignee: table.get(row, assignee_col),
                reporter: table.get(row, reporter_col),
                labels: &labels,
                created: table.get(row, created_col),
                updated: table.get(row, updated_col),
                url: "",
            },
        ));
    }
    Ok(docs)
}

struct JiraMeta<'a> {
    issue_type: &'a str,
    status: &'a str,
    priority: &'a str,
    assignee: &'a str,
    reporter: &'a str,
    labels: &'a str,
    created: &'a str,
    updated: &'a str,
    url: &'a str,
}

fn build(id: String, summary: String, description: String, m: JiraMeta<'_>) -> ExternalDocument {
    ExternalDocument::new("jira", id, summary, description)
        .with_meta("type", m.issue_type)
        .with_meta("status", m.status)
        .with_meta("priority", m.priority)
        .with_meta("assignee", m.assignee)
        .with_meta("reporter", m.reporter)
        .with_meta("labels", m.labels)
        .with_meta("created", m.created)
        .with_meta("updated", m.updated)
        .with_meta("url", m.url)
}

/// Extract a display name from a Jira user object (or a bare string).
fn person(v: Option<&Value>) -> String {
    match v {
        Some(Value::Object(o)) => o
            .get("displayName")
            .or_else(|| o.get("name"))
            .or_else(|| o.get("emailAddress"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Derive a `<base>/browse/<KEY>` link from a REST `self` URL when possible,
/// else fall back to the raw `self` URL.
fn browse_url(self_url: Option<&str>, key: &str) -> String {
    let Some(s) = self_url else {
        return String::new();
    };
    if key.is_empty() {
        return s.to_string();
    }
    // self looks like https://host/rest/api/2/issue/10001 — take scheme+host.
    if let Some(idx) = s.find("/rest/") {
        return format!("{}/browse/{key}", &s[..idx]);
    }
    s.to_string()
}

/// Flatten Atlassian Document Format (or any JSON) to text by collecting every
/// `"text"` string value in document order. Plain-string descriptions pass
/// through unchanged.
fn adf_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(o) => {
            // A leaf text node: {"type":"text","text":"…"}.
            if let Some(Value::String(t)) = o.get("text") {
                return t.clone();
            }
            // Only descend into nested structures; ignore scalar metadata fields
            // like `"type": "doc"` so they don't leak into the extracted text.
            let mut parts: Vec<String> = Vec::new();
            for val in o.values() {
                if val.is_array() || val.is_object() {
                    let t = adf_text(val);
                    if !t.is_empty() {
                        parts.push(t);
                    }
                }
            }
            parts.join(" ")
        }
        Value::Array(a) => a
            .iter()
            .map(adf_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rest_search_json_with_adf_description() {
        let json = r#"{
          "issues": [
            {
              "key": "PROJ-7",
              "self": "https://acme.atlassian.net/rest/api/2/issue/10007",
              "fields": {
                "summary": "Throttler drops requests",
                "description": {"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"The `add_chunk` path leaks."}]}]},
                "status": {"name": "In Progress"},
                "assignee": {"displayName": "Alice"},
                "reporter": {"displayName": "Bob"},
                "issuetype": {"name": "Bug"},
                "priority": {"name": "High"},
                "labels": ["perf", "p1"],
                "created": "2026-01-01",
                "updated": "2026-01-02"
              }
            }
          ]
        }"#;
        let docs = parse_str(json).unwrap();
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.source, "jira");
        assert_eq!(d.id, "PROJ-7");
        assert_eq!(d.title, "Throttler drops requests");
        assert!(d.body.contains("add_chunk"));
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "status" && v == "In Progress")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "assignee" && v == "Alice")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "labels" && v == "perf, p1")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "url" && v == "https://acme.atlassian.net/browse/PROJ-7")
        );
    }

    #[test]
    fn parses_csv_with_repeated_label_columns() {
        let csv = "Issue key,Summary,Description,Status,Assignee,Labels,Labels\n\
                   PROJ-1,Fix login,\"Users cannot log in. See `auth`.\",Open,Carol,bug,security\n";
        let docs = parse_str(csv).unwrap();
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.id, "PROJ-1");
        assert_eq!(d.title, "Fix login");
        assert!(d.body.contains("auth"));
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "labels" && v == "bug, security")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "assignee" && v == "Carol")
        );
    }

    #[test]
    fn adf_text_flattens_nested_content() {
        let v: Value = serde_json::from_str(
            r#"{"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}]}"#,
        )
        .unwrap();
        assert_eq!(adf_text(&v), "hello world");
    }
}
