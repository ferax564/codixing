//! Linear issue importer.
//!
//! Accepts either:
//!
//! - **CSV** — Linear's "Export issues" CSV (columns `ID`, `Title`,
//!   `Description`, `Status`, `Assignee`, `Labels`, `Priority`, `Created`, …).
//! - **JSON** — a Linear API response. Issues may appear as a bare array, under
//!   `{ "issues": [...] }`, or under the GraphQL `{ "data": { "issues":
//!   { "nodes": [...] } } }` shape. Each issue uses `identifier`, `title`,
//!   `description`, `state.name`, `assignee.name`, `labels.nodes[].name`,
//!   `url`, `createdAt`, `updatedAt`.
//!
//! Format is auto-detected from the first non-whitespace byte (`{`/`[` → JSON).

use std::path::Path;

use serde_json::Value;

use super::ExternalDocument;
use super::csv::Table;
use crate::error::{CodixingError, Result};

/// Parse a Linear export at `path` into documents.
pub fn parse(path: &Path) -> Result<Vec<ExternalDocument>> {
    let text = std::fs::read_to_string(path)?;
    parse_str(&text)
}

/// Parse Linear export text, auto-detecting JSON vs CSV.
pub fn parse_str(text: &str) -> Result<Vec<ExternalDocument>> {
    match text.trim_start().chars().next() {
        Some('{') | Some('[') => parse_json(text),
        _ => parse_csv(text),
    }
}

fn parse_json(text: &str) -> Result<Vec<ExternalDocument>> {
    let root: Value = serde_json::from_str(text)
        .map_err(|e| CodixingError::Import(format!("invalid Linear JSON: {e}")))?;
    let issues = extract_issue_array(&root).ok_or_else(|| {
        CodixingError::Import(
            "expected a Linear issue array (bare, `issues`, or `data.issues.nodes`)".to_string(),
        )
    })?;
    Ok(issues.iter().filter_map(json_issue).collect())
}

/// Locate the issue array across Linear's response shapes.
fn extract_issue_array(root: &Value) -> Option<Vec<Value>> {
    match root {
        Value::Array(items) => Some(items.clone()),
        Value::Object(map) => {
            if let Some(Value::Array(items)) = map.get("issues") {
                return Some(items.clone());
            }
            // GraphQL: data.issues.nodes
            if let Some(nodes) = map
                .get("data")
                .and_then(|d| d.get("issues"))
                .and_then(|i| i.get("nodes"))
                .and_then(Value::as_array)
            {
                return Some(nodes.clone());
            }
            // Connection shape: { "nodes": [...] }
            if let Some(Value::Array(nodes)) = map.get("nodes") {
                return Some(nodes.clone());
            }
            None
        }
        _ => None,
    }
}

fn json_issue(item: &Value) -> Option<ExternalDocument> {
    let obj = item.as_object()?;
    let identifier = obj
        .get("identifier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if identifier.is_empty() && title.is_empty() {
        return None;
    }

    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let status = obj
        .get("state")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .or_else(|| obj.get("status").and_then(Value::as_str))
        .unwrap_or_default();
    let assignee = obj
        .get("assignee")
        .and_then(|a| a.get("name").or_else(|| a.get("displayName")))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let priority = obj
        .get("priorityLabel")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            obj.get("priority")
                .and_then(Value::as_u64)
                .map(|p| p.to_string())
        })
        .unwrap_or_default();
    let labels = label_names(obj.get("labels")).join(", ");
    let url = obj.get("url").and_then(Value::as_str).unwrap_or_default();
    let created = obj
        .get("createdAt")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let updated = obj
        .get("updatedAt")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let id = if identifier.is_empty() {
        super::slugify(&title)
    } else {
        identifier.to_string()
    };
    Some(build(
        id,
        title,
        description,
        LinearMeta {
            status,
            priority: &priority,
            assignee,
            labels: &labels,
            url,
            created,
            updated,
        },
    ))
}

/// Linear labels appear as `{"nodes":[{"name":..}]}` (GraphQL), a plain array
/// of `{"name":..}`, or an array of strings.
fn label_names(v: Option<&Value>) -> Vec<String> {
    let arr = match v {
        Some(Value::Object(o)) => o.get("nodes").and_then(Value::as_array).cloned(),
        Some(Value::Array(a)) => Some(a.clone()),
        _ => None,
    };
    arr.unwrap_or_default()
        .iter()
        .filter_map(|l| match l {
            Value::Object(o) => o.get("name").and_then(Value::as_str).map(str::to_string),
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_csv(text: &str) -> Result<Vec<ExternalDocument>> {
    let table = Table::from_csv(text)
        .ok_or_else(|| CodixingError::Import("empty Linear CSV (no header row)".to_string()))?;
    let id_col = table.col(&["id", "identifier"]);
    let title_col = table.col(&["title"]);
    let desc_col = table.col(&["description"]);
    let status_col = table.col(&["status", "state"]);
    let assignee_col = table.col(&["assignee"]);
    let priority_col = table.col(&["priority"]);
    let created_col = table.col(&["created", "created at"]);
    let updated_col = table.col(&["updated", "updated at"]);
    let label_cols = table.cols(&["labels", "label"]);

    let mut docs = Vec::with_capacity(table.rows.len());
    for row in &table.rows {
        let id_val = table.get(row, id_col);
        let title = table.get(row, title_col);
        if id_val.is_empty() && title.is_empty() {
            continue;
        }
        // A single Linear CSV "Labels" cell may itself be comma- or
        // semicolon-separated; normalize separators to a clean join.
        let labels = label_cols
            .iter()
            .filter_map(|&i| row.get(i).map(|s| s.trim()))
            .filter(|s| !s.is_empty())
            .flat_map(|s| s.split([',', ';']).map(|p| p.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let id = if id_val.is_empty() {
            super::slugify(title)
        } else {
            id_val.to_string()
        };
        docs.push(build(
            id,
            title.to_string(),
            table.get(row, desc_col).to_string(),
            LinearMeta {
                status: table.get(row, status_col),
                priority: table.get(row, priority_col),
                assignee: table.get(row, assignee_col),
                labels: &labels,
                url: "",
                created: table.get(row, created_col),
                updated: table.get(row, updated_col),
            },
        ));
    }
    Ok(docs)
}

struct LinearMeta<'a> {
    status: &'a str,
    priority: &'a str,
    assignee: &'a str,
    labels: &'a str,
    url: &'a str,
    created: &'a str,
    updated: &'a str,
}

fn build(id: String, title: String, description: String, m: LinearMeta<'_>) -> ExternalDocument {
    ExternalDocument::new("linear", id, title, description)
        .with_meta("status", m.status)
        .with_meta("priority", m.priority)
        .with_meta("assignee", m.assignee)
        .with_meta("labels", m.labels)
        .with_meta("url", m.url)
        .with_meta("created", m.created)
        .with_meta("updated", m.updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_graphql_nodes_json() {
        let json = r#"{
          "data": {"issues": {"nodes": [
            {
              "identifier": "ENG-123",
              "title": "Rate limiter regression",
              "description": "The `throttler` drops requests under load.",
              "state": {"name": "In Progress"},
              "assignee": {"name": "Dana"},
              "priorityLabel": "Urgent",
              "labels": {"nodes": [{"name": "bug"}, {"name": "backend"}]},
              "url": "https://linear.app/acme/issue/ENG-123",
              "createdAt": "2026-03-01",
              "updatedAt": "2026-03-02"
            }
          ]}}
        }"#;
        let docs = parse_str(json).unwrap();
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.source, "linear");
        assert_eq!(d.id, "ENG-123");
        assert_eq!(d.title, "Rate limiter regression");
        assert!(d.body.contains("throttler"));
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "status" && v == "In Progress")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "assignee" && v == "Dana")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "priority" && v == "Urgent")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "labels" && v == "bug, backend")
        );
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "url" && v.contains("ENG-123"))
        );
    }

    #[test]
    fn parses_bare_array_json() {
        let json = r#"[{"identifier":"ENG-1","title":"X","labels":["a","b"]}]"#;
        let docs = parse_str(json).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(
            docs[0]
                .metadata
                .iter()
                .any(|(k, v)| k == "labels" && v == "a, b")
        );
    }

    #[test]
    fn parses_csv_export() {
        let csv = "ID,Title,Description,Status,Assignee,Priority,Labels\n\
                   ENG-9,Crash on save,\"Null deref in `save`.\",Todo,Eve,High,\"bug, ui\"\n";
        let docs = parse_str(csv).unwrap();
        assert_eq!(docs.len(), 1);
        let d = &docs[0];
        assert_eq!(d.id, "ENG-9");
        assert_eq!(d.title, "Crash on save");
        assert!(d.body.contains("save"));
        assert!(d.metadata.iter().any(|(k, v)| k == "status" && v == "Todo"));
        assert!(
            d.metadata
                .iter()
                .any(|(k, v)| k == "labels" && v == "bug, ui")
        );
    }
}
