//! Jupyter notebook (`.ipynb`) parser.
//!
//! Notebooks are JSON documents holding an ordered list of cells. Each cell
//! has a `cell_type` (`code` / `markdown` / `raw`) and a `source` field that
//! is either a string or an array of strings. The notebook's kernel
//! language lives at `metadata.kernelspec.language`.
//!
//! This module extracts cells so the engine-level dispatcher can route:
//! - `code` cells → tree-sitter per `metadata.kernelspec.language`
//! - `markdown` cells → `markdown.rs`
//! - `raw` cells → skipped (callers decide)
//! - output cells → never emitted (noisy, may contain secrets)
//!
//! Cell IDs use nbformat ≥ 4.5's `id` field when available; older notebooks
//! fall back to a synthetic `cell-{index}`.

/// The kind of cell carried by a notebook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Code,
    Markdown,
    Raw,
}

/// A single cell extracted from a notebook.
#[derive(Debug, Clone)]
pub struct NotebookCell {
    pub kind: CellKind,
    /// nbformat ≥ 4.5 cell id, or a synthetic `cell-{index}` fallback.
    pub id: String,
    /// Zero-based position of the cell in the notebook.
    pub index: usize,
    /// Concatenated cell source (joined across string arrays).
    pub source: String,
    /// Notebook-level `metadata.kernelspec.language`, if present.
    pub kernel_language: Option<String>,
}

/// Parse a `.ipynb` payload into a list of cells.
///
/// Output cells are dropped unconditionally. Unknown `cell_type` values
/// are skipped rather than rejected so malformed notebooks index
/// gracefully. Returns `Err` only when the top-level JSON fails to
/// parse or is not a JSON object.
pub fn parse_notebook(source: &[u8]) -> Result<Vec<NotebookCell>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(source).map_err(|e| format!("invalid notebook JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "notebook root is not a JSON object".to_string())?;

    let kernel_language = obj
        .get("metadata")
        .and_then(|m| m.get("kernelspec"))
        .and_then(|k| k.get("language"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase());

    let cells = obj.get("cells").and_then(|c| c.as_array()).cloned();
    let Some(cells) = cells else {
        return Ok(Vec::new());
    };

    let mut out = Vec::with_capacity(cells.len());
    for (index, cell) in cells.into_iter().enumerate() {
        let Some(cell_obj) = cell.as_object() else {
            continue;
        };
        let kind = match cell_obj.get("cell_type").and_then(|v| v.as_str()) {
            Some("code") => CellKind::Code,
            Some("markdown") => CellKind::Markdown,
            Some("raw") => CellKind::Raw,
            _ => continue,
        };
        let id = cell_obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("cell-{index}"));
        let source = extract_source_string(cell_obj.get("source"));
        out.push(NotebookCell {
            kind,
            id,
            index,
            source,
            kernel_language: kernel_language.clone(),
        });
    }
    Ok(out)
}

/// nbformat permits `source` as either a string or an array of strings.
/// Concatenate array forms without adding separators (the source itself
/// already carries newlines when the array represents per-line splits).
fn extract_source_string(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => {
            let mut buf = String::new();
            for part in parts {
                if let Some(s) = part.as_str() {
                    buf.push_str(s);
                }
            }
            buf
        }
        _ => String::new(),
    }
}

/// Map a kernel language name (e.g. `"python"`, `"rust"`) to the file
/// extension the indexer uses to route to the right
/// `LanguageSupport` impl. Returns `None` when the kernel is absent or
/// unmapped — the caller should then either default to Python (the
/// most common notebook kernel) or skip the cell.
pub fn kernel_language_extension(kernel: Option<&str>) -> Option<&'static str> {
    let kernel = kernel?.to_ascii_lowercase();
    // Normalise common name variants to the canonical extension used
    // by `Language::extensions()`.
    match kernel.as_str() {
        "python" | "python2" | "python3" | "ipython" | "ipython3" => Some("py"),
        "rust" => Some("rs"),
        "typescript" | "ts" | "deno" => Some("ts"),
        "javascript" | "js" | "node" | "nodejs" => Some("js"),
        "go" | "golang" => Some("go"),
        "ruby" => Some("rb"),
        "bash" | "sh" | "shell" => Some("sh"),
        "java" => Some("java"),
        "scala" => Some("scala"),
        "c" => Some("c"),
        "cpp" | "c++" => Some("cpp"),
        "csharp" | "cs" | "c#" => Some("cs"),
        "swift" => Some("swift"),
        "kotlin" => Some("kt"),
        "php" => Some("php"),
        "matlab" | "octave" => Some("m"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nb(kernel: &str, cells: &str) -> String {
        format!(
            r#"{{
                "nbformat": 4, "nbformat_minor": 5,
                "metadata": {{ "kernelspec": {{ "language": "{kernel}" }} }},
                "cells": [{cells}]
            }}"#
        )
    }

    #[test]
    fn parses_code_markdown_raw_cells() {
        // Use r##"..."## so the `"# Title` sequence inside the JSON
        // doesn't prematurely terminate the raw string.
        let src = nb(
            "python",
            r##"
            {"cell_type":"markdown","id":"md1","source":"# Title\n"},
            {"cell_type":"code","id":"c1","source":["import os\n","print(1)\n"]},
            {"cell_type":"raw","id":"r1","source":"verbatim"}
            "##,
        );
        let cells = parse_notebook(src.as_bytes()).unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].kind, CellKind::Markdown);
        assert_eq!(cells[0].id, "md1");
        assert_eq!(cells[0].source, "# Title\n");
        assert_eq!(cells[1].kind, CellKind::Code);
        assert_eq!(cells[1].source, "import os\nprint(1)\n");
        assert_eq!(cells[1].kernel_language.as_deref(), Some("python"));
        assert_eq!(cells[2].kind, CellKind::Raw);
    }

    #[test]
    fn skips_unknown_cell_types_and_outputs() {
        // `output` is not a cell_type in nbformat but a field on code
        // cells. Unknown cell_type values are tolerated by skipping.
        let src = nb(
            "python",
            r##"
            {"cell_type":"code","source":"x = 1"},
            {"cell_type":"heading","source":"not a real type"},
            {"cell_type":"markdown","source":"ok"}
            "##,
        );
        let cells = parse_notebook(src.as_bytes()).unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].kind, CellKind::Code);
        assert_eq!(cells[1].kind, CellKind::Markdown);
    }

    #[test]
    fn synthesises_missing_cell_ids() {
        let src = nb(
            "python",
            r##"
            {"cell_type":"code","source":"a"},
            {"cell_type":"code","source":"b"}
            "##,
        );
        let cells = parse_notebook(src.as_bytes()).unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0].id, "cell-0");
        assert_eq!(cells[1].id, "cell-1");
        assert_eq!(cells[0].index, 0);
        assert_eq!(cells[1].index, 1);
    }

    #[test]
    fn absent_kernel_language_yields_none() {
        let src = r#"{"cells":[{"cell_type":"code","source":"x"}]}"#;
        let cells = parse_notebook(src.as_bytes()).unwrap();
        assert_eq!(cells.len(), 1);
        assert!(cells[0].kernel_language.is_none());
    }

    #[test]
    fn empty_cells_array_returns_empty() {
        let src = r#"{"cells":[]}"#;
        let cells = parse_notebook(src.as_bytes()).unwrap();
        assert!(cells.is_empty());
    }

    #[test]
    fn missing_cells_array_returns_empty() {
        let src = r#"{"metadata":{}}"#;
        let cells = parse_notebook(src.as_bytes()).unwrap();
        assert!(cells.is_empty());
    }

    #[test]
    fn invalid_json_returns_error() {
        let err = parse_notebook(b"not json").unwrap_err();
        assert!(err.contains("invalid notebook JSON"));
    }

    #[test]
    fn non_object_root_returns_error() {
        let err = parse_notebook(b"[1,2,3]").unwrap_err();
        assert!(err.contains("root is not a JSON object"));
    }

    #[test]
    fn kernel_language_extension_maps_common_kernels() {
        assert_eq!(kernel_language_extension(Some("python")), Some("py"));
        assert_eq!(kernel_language_extension(Some("Python3")), Some("py"));
        assert_eq!(kernel_language_extension(Some("rust")), Some("rs"));
        assert_eq!(kernel_language_extension(Some("cpp")), Some("cpp"));
        assert_eq!(kernel_language_extension(Some("C++")), Some("cpp"));
        assert_eq!(kernel_language_extension(Some("node")), Some("js"));
        assert_eq!(kernel_language_extension(None), None);
        assert_eq!(kernel_language_extension(Some("brainfuck")), None);
    }

    #[test]
    fn string_source_and_array_source_both_work() {
        let src = r#"{
            "cells":[
                {"cell_type":"code","source":"a\nb\n"},
                {"cell_type":"code","source":["c\n","d\n"]}
            ]
        }"#;
        let cells = parse_notebook(src.as_bytes()).unwrap();
        assert_eq!(cells[0].source, "a\nb\n");
        assert_eq!(cells[1].source, "c\nd\n");
    }
}
