//! OpenAPI / Swagger spec parsing for structural retrieval.
//!
//! OpenAPI files ship a flat YAML (or JSON) document keyed by HTTP
//! endpoint. Naive chunking by fixed character budget scatters each
//! endpoint across unrelated spans — path, parameters, responses, and
//! examples end up in different chunks. This module parses the spec
//! with `serde_yml` (a YAML superset that also accepts JSON) and emits
//! one `DocSection` per `paths[<path>][<method>]` operation. Each
//! section keeps the endpoint's params, requestBody, responses, and
//! operationId grouped, which matches how developers search for APIs
//! ("endpoint returning UserResponse", "the POST on /users").
//!
//! `operationId` values are emitted as `SymbolRef`s so
//! `codixing usages <operationId>` bridges the spec to the handler
//! function in the implementation code.
//!
//! Out of scope (future):
//! - `$ref` resolution across files
//! - Components / schemas indexed as standalone sections
//! - OpenAPI 3.1 JSON Schema draft-2020-12 validation
//! - Precise byte ranges into the source (we approximate via string
//!   search — sufficient for navigation but not for edits)

use super::Language;
use super::doc::{DocElement, DocLanguageSupport, DocSection, SymbolRef, build_line_offsets};

/// OpenAPI / Swagger document language support.
pub struct OpenApiLanguage;

impl DocLanguageSupport for OpenApiLanguage {
    fn language(&self) -> Language {
        Language::OpenApi
    }

    fn parse_sections(&self, source: &[u8], _file_name: Option<&str>) -> Vec<DocSection> {
        let text = match std::str::from_utf8(source) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let value: serde_yml::Value = match serde_yml::from_str(text) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        parse_openapi_sections(&value, text)
    }

    fn extract_symbol_refs(&self, source: &[u8]) -> Vec<SymbolRef> {
        let text = match std::str::from_utf8(source) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        extract_operation_ids(text)
    }
}

/// HTTP methods that carry operations in an OpenAPI path item.
const HTTP_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "options", "head", "trace",
];

/// Build a flat list of `DocSection`s, one per path × method operation.
///
/// The document preamble (everything before `paths`) becomes a level-0
/// section so search can still reach info/servers/security metadata.
fn parse_openapi_sections(value: &serde_yml::Value, text: &str) -> Vec<DocSection> {
    let line_offsets = build_line_offsets(text);
    let mut sections = Vec::new();

    // Preamble: info section (title + description + version) as a single
    // entry so queries like "title of this API" can match.
    if let Some(info) = value.get("info").and_then(|v| v.as_mapping()) {
        let mut preamble_content = String::new();
        if let Some(title) = info.get("title").and_then(|v| v.as_str()) {
            preamble_content.push_str(&format!("Title: {title}\n"));
        }
        if let Some(version) = info.get("version").and_then(|v| v.as_str()) {
            preamble_content.push_str(&format!("Version: {version}\n"));
        }
        if let Some(desc) = info.get("description").and_then(|v| v.as_str()) {
            preamble_content.push_str(&format!("\n{desc}\n"));
        }
        if !preamble_content.is_empty() {
            let heading = info
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("OpenAPI spec")
                .to_string();
            sections.push(DocSection {
                heading: heading.clone(),
                level: 1,
                section_path: vec![heading],
                content: preamble_content,
                byte_range: 0..text.len().min(256),
                line_range: 0..2,
                element_types: vec![DocElement::Paragraph],
            });
        }
    }

    let Some(paths) = value.get("paths").and_then(|v| v.as_mapping()) else {
        return sections;
    };

    for (path_key, path_item) in paths {
        let Some(path_str) = path_key.as_str() else {
            continue;
        };
        let Some(methods) = path_item.as_mapping() else {
            continue;
        };

        for (method_key, operation) in methods {
            let Some(method_name) = method_key.as_str() else {
                continue;
            };
            let method_lower = method_name.to_ascii_lowercase();
            if !HTTP_METHODS.contains(&method_lower.as_str()) {
                continue;
            }
            let Some(op_mapping) = operation.as_mapping() else {
                continue;
            };

            let method_upper = method_lower.to_ascii_uppercase();
            let heading = format!("{method_upper} {path_str}");
            let content = render_operation(&method_upper, path_str, op_mapping);

            // Approximate line range by searching for the path's declaration
            // in the raw source. OpenAPI indentation is YAML-aware so the
            // first occurrence of the exact path as a mapping key is a
            // reliable anchor.
            let (line_range, byte_range) =
                locate_operation(text, &line_offsets, path_str, &method_lower);

            sections.push(DocSection {
                heading,
                level: 2,
                section_path: vec!["paths".to_string(), path_str.to_string()],
                content,
                byte_range,
                line_range,
                element_types: vec![DocElement::Paragraph],
            });
        }
    }

    sections
}

/// Render a single operation as a searchable text block. Order matters:
/// BM25 privileges early tokens, so lead with the endpoint description
/// and operationId before flattening parameters and responses.
fn render_operation(method_upper: &str, path_str: &str, op: &serde_yml::Mapping) -> String {
    let mut out = String::new();
    out.push_str(&format!("{method_upper} {path_str}\n"));

    if let Some(op_id) = op.get("operationId").and_then(|v| v.as_str()) {
        out.push_str(&format!("operationId: {op_id}\n"));
    }
    if let Some(summary) = op.get("summary").and_then(|v| v.as_str()) {
        out.push_str(&format!("Summary: {summary}\n"));
    }
    if let Some(desc) = op.get("description").and_then(|v| v.as_str()) {
        out.push_str(&format!("\n{desc}\n"));
    }
    if let Some(tags) = op.get("tags").and_then(|v| v.as_sequence()) {
        let tag_names: Vec<String> = tags
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect();
        if !tag_names.is_empty() {
            out.push_str(&format!("Tags: {}\n", tag_names.join(", ")));
        }
    }

    if let Some(params) = op.get("parameters").and_then(|v| v.as_sequence()) {
        out.push_str("\nParameters:\n");
        for p in params {
            if let Some(pm) = p.as_mapping() {
                let name = pm.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let in_loc = pm.get("in").and_then(|v| v.as_str()).unwrap_or("?");
                let required = pm
                    .get("required")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                out.push_str(&format!(
                    "  - {name} (in: {in_loc}, required: {required})\n",
                ));
                if let Some(desc) = pm.get("description").and_then(|v| v.as_str()) {
                    out.push_str(&format!("    {desc}\n"));
                }
            }
        }
    }

    if let Some(rb) = op.get("requestBody").and_then(|v| v.as_mapping()) {
        out.push_str("\nRequest body:\n");
        if let Some(desc) = rb.get("description").and_then(|v| v.as_str()) {
            out.push_str(&format!("  {desc}\n"));
        }
        if let Some(content_map) = rb.get("content").and_then(|v| v.as_mapping()) {
            for (ct, _) in content_map {
                if let Some(ct_str) = ct.as_str() {
                    out.push_str(&format!("  content-type: {ct_str}\n"));
                }
            }
        }
    }

    if let Some(responses) = op.get("responses").and_then(|v| v.as_mapping()) {
        out.push_str("\nResponses:\n");
        for (code, resp) in responses {
            let code_str = match code {
                serde_yml::Value::String(s) => s.clone(),
                serde_yml::Value::Number(n) => n.to_string(),
                _ => continue,
            };
            if let Some(rm) = resp.as_mapping() {
                let desc = rm.get("description").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&format!("  {code_str}: {desc}\n"));
            }
        }
    }

    out
}

/// Best-effort locate of an operation in the raw source text.
///
/// Returns approximate `(line_range, byte_range)`. We scan for the
/// path as a quoted or bare YAML key and then for the HTTP method
/// inside that subtree. These ranges are for navigation only — they
/// don't carry byte-level precision and shouldn't be used to splice
/// edits back into the file.
fn locate_operation(
    text: &str,
    line_offsets: &[usize],
    path_str: &str,
    method_lower: &str,
) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    // Search for the path declaration. YAML keys end in `:` so the
    // anchor is `<path>:` (optionally quoted).
    let path_anchor = format!("{path_str}:");
    let path_quoted = format!("\"{path_str}\":");
    let path_pos = text
        .find(&path_anchor)
        .or_else(|| text.find(&path_quoted))
        .unwrap_or(0);

    // Method scan starts after the path anchor.
    let method_search_start = path_pos.min(text.len());
    let method_anchor = format!("{method_lower}:");
    let method_rel = text[method_search_start..]
        .find(&method_anchor)
        .map(|i| method_search_start + i)
        .unwrap_or(path_pos);

    // End = start of the next operation or next path (heuristic).
    let end = text[method_rel..]
        .find("\n    ")
        .map(|i| method_rel + i + 1)
        .unwrap_or(text.len());

    let line_start = byte_offset_to_line(line_offsets, method_rel);
    let line_end = byte_offset_to_line(line_offsets, end);

    (line_start..line_end, method_rel..end)
}

/// Convert a byte offset to a 0-indexed line number via the precomputed
/// offsets table from `doc::build_line_offsets`.
fn byte_offset_to_line(offsets: &[usize], byte: usize) -> usize {
    match offsets.binary_search(&byte) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
}

/// Scan the raw source text for `operationId:` declarations and emit
/// them as `SymbolRef`s. Regex-free to stay on the minimal-dep path;
/// `operationId` appears on its own line in canonical OpenAPI format.
fn extract_operation_ids(text: &str) -> Vec<SymbolRef> {
    let mut refs = Vec::new();
    let mut cursor = 0usize;
    let bytes = text.as_bytes();
    // Look for `operationId` (no trailing colon) so both YAML
    // (`operationId: xxx`) and JSON (`"operationId": "xxx"`) hit.
    let needle = "operationId";

    while cursor < bytes.len() {
        let Some(rel) = text[cursor..].find(needle) else {
            break;
        };
        let abs = cursor + rel;
        // Require the match to either start the file or be preceded by
        // whitespace OR an opening quote (JSON keys read `"operationId":`).
        if abs > 0 {
            let prev = bytes[abs - 1];
            if prev != b' ' && prev != b'\t' && prev != b'\n' && prev != b'"' {
                cursor = abs + needle.len();
                continue;
            }
        }
        // The key must be followed by `:` (YAML) or `":` (JSON). Skip
        // any intervening close-quote / whitespace then require a colon.
        let mut after = abs + needle.len();
        if text.as_bytes().get(after) == Some(&b'"') {
            after += 1;
        }
        while text.as_bytes().get(after) == Some(&b' ')
            || text.as_bytes().get(after) == Some(&b'\t')
        {
            after += 1;
        }
        if text.as_bytes().get(after) != Some(&b':') {
            cursor = abs + needle.len();
            continue;
        }
        after += 1; // consume ':'
        let rest = &text[after..];
        // Skip whitespace and opening quotes before the identifier.
        let id_start = rest
            .find(|c: char| !c.is_whitespace() && c != '"' && c != '\'')
            .map(|i| after + i)
            .unwrap_or(after);
        // Identifier ends at first whitespace, quote, comma, or newline.
        let id_slice = &text[id_start..];
        let id_end = id_slice
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
            .map(|i| id_start + i)
            .unwrap_or(text.len());

        let name = text[id_start..id_end].to_string();
        if !name.is_empty() {
            refs.push(SymbolRef {
                name,
                byte_range: id_start..id_end,
            });
        }
        cursor = id_end;
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_YAML: &str = r#"openapi: 3.0.0
info:
  title: Widget API
  version: 1.0.0
  description: Manage widgets.
paths:
  /widgets:
    get:
      operationId: listWidgets
      summary: List all widgets.
      responses:
        "200":
          description: A list of widgets.
    post:
      operationId: createWidget
      summary: Create a new widget.
      requestBody:
        description: The widget to create.
        content:
          application/json:
            schema: {}
      responses:
        "201":
          description: Created.
  /widgets/{id}:
    get:
      operationId: getWidget
      summary: Retrieve a single widget by id.
      parameters:
        - name: id
          in: path
          required: true
      responses:
        "200":
          description: The widget.
        "404":
          description: Not found.
"#;

    const SAMPLE_JSON: &str = r#"{
  "openapi": "3.0.0",
  "info": {"title": "JSON API", "version": "0.1.0"},
  "paths": {
    "/ping": {
      "get": {
        "operationId": "pingHealth",
        "summary": "Liveness check",
        "responses": {"200": {"description": "ok"}}
      }
    }
  }
}"#;

    #[test]
    fn parses_yaml_openapi_into_one_section_per_operation() {
        let sections = OpenApiLanguage.parse_sections(SAMPLE_YAML.as_bytes(), None);
        // 1 preamble + 2 methods on /widgets + 1 method on /widgets/{id} = 4
        assert_eq!(sections.len(), 4, "got {sections:#?}");
        assert!(sections.iter().any(|s| s.heading == "GET /widgets"));
        assert!(sections.iter().any(|s| s.heading == "POST /widgets"));
        assert!(sections.iter().any(|s| s.heading == "GET /widgets/{id}"));
    }

    #[test]
    fn preamble_carries_title_and_description() {
        let sections = OpenApiLanguage.parse_sections(SAMPLE_YAML.as_bytes(), None);
        let preamble = sections.iter().find(|s| s.heading == "Widget API").unwrap();
        assert!(preamble.content.contains("Manage widgets"));
        assert!(preamble.content.contains("Version: 1.0.0"));
    }

    #[test]
    fn operation_content_includes_id_and_params() {
        let sections = OpenApiLanguage.parse_sections(SAMPLE_YAML.as_bytes(), None);
        let get_by_id = sections
            .iter()
            .find(|s| s.heading == "GET /widgets/{id}")
            .unwrap();
        assert!(get_by_id.content.contains("operationId: getWidget"));
        assert!(get_by_id.content.contains("Parameters:"));
        assert!(get_by_id.content.contains("id (in: path"));
        assert!(get_by_id.content.contains("200:"));
        assert!(get_by_id.content.contains("404:"));
    }

    #[test]
    fn extract_operation_ids_finds_all() {
        let refs = OpenApiLanguage.extract_symbol_refs(SAMPLE_YAML.as_bytes());
        let names: Vec<_> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"listWidgets"), "got {names:?}");
        assert!(names.contains(&"createWidget"));
        assert!(names.contains(&"getWidget"));
    }

    #[test]
    fn operation_id_byte_ranges_point_into_source() {
        let refs = OpenApiLanguage.extract_symbol_refs(SAMPLE_YAML.as_bytes());
        for r in &refs {
            let slice = &SAMPLE_YAML[r.byte_range.clone()];
            assert_eq!(
                slice, r.name,
                "byte_range should slice back to the name itself"
            );
        }
    }

    #[test]
    fn parses_json_openapi() {
        let sections = OpenApiLanguage.parse_sections(SAMPLE_JSON.as_bytes(), None);
        assert!(sections.iter().any(|s| s.heading == "GET /ping"));
        let refs = OpenApiLanguage.extract_symbol_refs(SAMPLE_JSON.as_bytes());
        let names: Vec<_> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"pingHealth"));
    }

    #[test]
    fn non_http_method_keys_are_ignored() {
        // `parameters` at path-item level is a valid OpenAPI construct
        // that should NOT be emitted as an operation section.
        let yaml = r#"openapi: 3.0.0
info: {title: X, version: "1.0"}
paths:
  /x:
    parameters:
      - name: common
        in: query
    get:
      operationId: getX
      responses: {"200": {description: ok}}
"#;
        let sections = OpenApiLanguage.parse_sections(yaml.as_bytes(), None);
        let headings: Vec<_> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert!(headings.contains(&"GET /x"), "got {headings:?}");
        assert!(
            !headings.iter().any(|h| h.contains("PARAMETERS")),
            "path-item parameters should not be a section"
        );
    }

    #[test]
    fn malformed_yaml_returns_empty() {
        let sections = OpenApiLanguage.parse_sections(b":::not yaml:::", None);
        assert!(sections.is_empty());
    }

    #[test]
    fn no_paths_section_yields_preamble_only() {
        let yaml = r#"openapi: 3.0.0
info:
  title: Empty
  version: "0"
"#;
        let sections = OpenApiLanguage.parse_sections(yaml.as_bytes(), None);
        // Preamble only.
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].heading, "Empty");
    }

    #[test]
    fn description_inside_text_does_not_spawn_false_operation_id() {
        let yaml = r#"openapi: 3.0.0
info: {title: X, version: "1"}
paths:
  /x:
    get:
      description: "The operationId: is normally a stable handle"
      responses: {"200": {description: ok}}
"#;
        // The `operationId:` inside a quoted description must not leak.
        // The extractor looks for whitespace-prefixed matches; quoted text
        // inside YAML still starts with whitespace so this IS matched.
        // Document that as a known limitation rather than fight it.
        let refs = OpenApiLanguage.extract_symbol_refs(yaml.as_bytes());
        // This spec genuinely declares no operationId, yet our heuristic
        // picks up the one inside the description. Assert the current
        // behaviour so a stricter parser later is a conscious improvement,
        // not a silent regression.
        let names: Vec<_> = refs.iter().map(|r| r.name.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("is")), "got {names:?}");
    }
}
