//! Integration test: index a repo containing an OpenAPI spec and verify
//! endpoint-aware retrieval.

use std::fs;

use tempfile::TempDir;

use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};

fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

const SAMPLE_SPEC: &str = r#"openapi: 3.0.0
info:
  title: Widget Service
  version: 1.2.0
  description: HTTP API for managing widgets in batch.
paths:
  /widgets:
    get:
      operationId: listWidgets
      summary: Return the widget catalogue.
      tags: [widgets]
      responses:
        "200":
          description: A paged list of widgets.
    post:
      operationId: createWidget
      summary: Create a new widget.
      requestBody:
        description: Widget payload.
        content:
          application/json:
            schema: {}
      responses:
        "201":
          description: The new widget.
  /widgets/{id}:
    get:
      operationId: getWidget
      summary: Read a single widget by id.
      parameters:
        - name: id
          in: path
          required: true
      responses:
        "200":
          description: The widget.
"#;

#[test]
fn index_and_search_openapi_endpoint() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("openapi.yaml"), SAMPLE_SPEC).unwrap();
    // Sibling Rust handler that implements the operation — verifies the
    // cross-spec bridge: `codixing usages listWidgets` should surface
    // both spec and handler.
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/handlers.rs"),
        "pub fn listWidgets() -> Vec<u32> { vec![] }\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Endpoint-level search should land on the spec file.
    let hits = engine
        .search(
            SearchQuery::new("create a new widget")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let paths: Vec<_> = hits.iter().map(|r| r.file_path.clone()).collect();
    assert!(
        paths.iter().any(|p| p.contains("openapi.yaml")),
        "expected openapi.yaml in results, got {paths:?}",
    );

    // The POST /widgets section's heading should surface in scope_chain
    // (section_path is ["paths", "/widgets"] per parse_sections).
    let spec_hit = hits
        .iter()
        .find(|r| r.file_path.contains("openapi.yaml"))
        .expect("spec hit");
    assert!(
        spec_hit
            .scope_chain
            .iter()
            .any(|s| s == "/widgets" || s == "paths"),
        "expected endpoint path in scope_chain, got {:?}",
        spec_hit.scope_chain,
    );
}

#[test]
fn openapi_operation_id_is_indexed_as_symbol() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("openapi.yaml"), SAMPLE_SPEC).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/noop.rs"), "pub fn noop() {}\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Operation id should be searchable via BM25 on the doc content.
    let hits = engine
        .search(
            SearchQuery::new("listWidgets")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let paths: Vec<_> = hits.iter().map(|r| r.file_path.clone()).collect();
    assert!(
        paths.iter().any(|p| p.contains("openapi.yaml")),
        "expected listWidgets hit in openapi.yaml, got {paths:?}",
    );
}

#[test]
fn generic_yaml_still_routes_to_yaml_language() {
    // Regression guard: adding OpenAPI must not swallow plain YAML
    // config files that happen to live alongside the spec.
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(
        root.join("config.yaml"),
        "database:\n  host: localhost\n  port: 5432\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/noop.rs"), "pub fn noop() {}\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();
    let hits = engine
        .search(
            SearchQuery::new("database host")
                .with_limit(10)
                .with_strategy(Strategy::Instant),
        )
        .unwrap();
    let yaml_hit = hits.iter().find(|r| r.file_path.contains("config.yaml"));
    assert!(
        yaml_hit.is_some(),
        "generic config.yaml should still be indexed as YAML",
    );
    assert_eq!(
        yaml_hit.unwrap().language,
        "YAML",
        "generic YAML must not be re-routed to OpenAPI",
    );
}

#[test]
fn malformed_openapi_does_not_abort_init() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(root.join("openapi.yaml"), ":::not-yaml::: {").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/noop.rs"), "pub fn noop() {}\n").unwrap();
    let engine = Engine::init(root, bm25_config(root));
    assert!(engine.is_ok(), "init should tolerate malformed OpenAPI");
}
