//! YAML language support — line-based symbol extraction.
//!
//! Extracts top-level and nested keys (up to depth 3) as symbols.
//! Recognises common formats:
//! - Docker Compose: `services.X` -> Module
//! - GitHub Actions: `jobs.X` -> Module, `steps[].name` -> Function
//! - Kubernetes: `kind:` value -> Type

use super::{ConfigLanguageSupport, EntityKind, Language, SemanticEntity, Visibility};

pub struct YamlLanguage;

impl ConfigLanguageSupport for YamlLanguage {
    fn language(&self) -> Language {
        Language::Yaml
    }

    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity> {
        let text = String::from_utf8_lossy(source);
        extract_yaml_entities(&text)
    }
}

/// Compute indentation level (number of leading spaces).
fn indent_level(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Extract the comment on the line immediately preceding `line_idx`, if any.
fn preceding_comment(lines: &[&str], line_idx: usize) -> Option<String> {
    if line_idx == 0 {
        return None;
    }
    let prev = lines[line_idx - 1].trim();
    prev.strip_prefix('#')
        .map(|comment| comment.trim().to_string())
}

fn extract_yaml_entities(text: &str) -> Vec<SemanticEntity> {
    let lines: Vec<&str> = text.lines().collect();
    let mut entities = Vec::new();
    // Track the key path stack: (indent, key_name)
    let mut stack: Vec<(usize, String)> = Vec::new();
    // Track top-level context keys for format detection.
    let mut is_compose = false;
    let mut is_actions = false;
    let mut k8s_kind: Option<String> = None;

    // First pass: detect format from top-level keys.
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = indent_level(line);
        if indent == 0 {
            if trimmed.starts_with("services:") {
                is_compose = true;
            }
            if trimmed.starts_with("jobs:") {
                is_actions = true;
            }
            if let Some(rest) = trimmed.strip_prefix("kind:") {
                let val = rest.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    k8s_kind = Some(val.to_string());
                }
            }
        }
    }

    // Emit Kubernetes kind as a Type entity.
    if let Some(ref kind_val) = k8s_kind {
        // Find the line for byte_range.
        for (i, line) in lines.iter().enumerate() {
            if line.trim().starts_with("kind:") {
                let byte_start = lines[..i].iter().map(|l| l.len() + 1).sum::<usize>();
                let byte_end = byte_start + line.len();
                entities.push(SemanticEntity {
                    kind: EntityKind::Type,
                    name: kind_val.clone(),
                    signature: Some(line.trim().to_string()),
                    doc_comment: preceding_comment(&lines, i),
                    byte_range: byte_start..byte_end,
                    line_range: i..i + 1,
                    scope: vec![],
                    visibility: Visibility::default(),
                    type_relations: Vec::new(),
                });
                break;
            }
        }
    }

    // Second pass: extract keys.
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
            continue;
        }

        // Match `key:` or `key: value`.
        let colon_pos = match trimmed.find(':') {
            Some(pos) => pos,
            None => continue,
        };

        let key = trimmed[..colon_pos].trim();
        // Skip keys that look like values (contain spaces, start with quotes).
        if key.is_empty()
            || key.contains(' ')
            || key.starts_with('"')
            || key.starts_with('\'')
            || key.starts_with('{')
        {
            continue;
        }

        let indent = indent_level(line);

        // Pop stack entries at same or deeper indent level.
        while let Some((prev_indent, _)) = stack.last() {
            if *prev_indent >= indent {
                stack.pop();
            } else {
                break;
            }
        }

        let depth = stack.len();
        // Only extract up to depth 3.
        if depth > 3 {
            stack.push((indent, key.to_string()));
            continue;
        }

        let scope: Vec<String> = stack.iter().map(|(_, k)| k.clone()).collect();
        let dotted_path = if scope.is_empty() {
            key.to_string()
        } else {
            format!("{}.{}", scope.join("."), key)
        };

        let byte_start: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        let byte_end = byte_start + line.len();

        // Determine entity kind based on format and depth.
        let entity_kind =
            if (is_compose && depth == 1 && scope.first().is_some_and(|s| s == "services"))
                || (is_actions && depth == 1 && scope.first().is_some_and(|s| s == "jobs"))
            {
                EntityKind::Module
            } else {
                EntityKind::Variable
            };

        let doc_comment = preceding_comment(&lines, i);
        let value_part = trimmed[colon_pos + 1..].trim();
        let signature = if value_part.is_empty() {
            Some(format!("{}:", dotted_path))
        } else {
            // Truncate long values.
            let display_val = if value_part.len() > 60 {
                format!("{}...", &value_part[..57])
            } else {
                value_part.to_string()
            };
            Some(format!("{}: {}", dotted_path, display_val))
        };

        entities.push(SemanticEntity {
            kind: entity_kind,
            name: dotted_path,
            signature,
            doc_comment,
            byte_range: byte_start..byte_end,
            line_range: i..i + 1,
            scope,
            visibility: Visibility::default(),
            type_relations: Vec::new(),
        });

        stack.push((indent, key.to_string()));
    }

    // GitHub Actions: extract step names.
    if is_actions {
        extract_action_step_names(&lines, &mut entities);
    }

    entities
}

/// Extract `- name: <value>` entries inside steps arrays as Function entities.
fn extract_action_step_names(lines: &[&str], entities: &mut Vec<SemanticEntity>) {
    let mut in_steps = false;
    let mut steps_indent = 0;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let indent = indent_level(line);

        if trimmed.starts_with("steps:") {
            in_steps = true;
            steps_indent = indent;
            continue;
        }

        if in_steps {
            // We've left the steps block.
            if indent <= steps_indent && !trimmed.is_empty() && !trimmed.starts_with('#') {
                in_steps = false;
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("- name:") {
                let name = rest.trim().trim_matches('"').trim_matches('\'');
                if !name.is_empty() {
                    let byte_start: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
                    let byte_end = byte_start + line.len();
                    entities.push(SemanticEntity {
                        kind: EntityKind::Function,
                        name: name.to_string(),
                        signature: Some(format!("step: {}", name)),
                        doc_comment: None,
                        byte_range: byte_start..byte_end,
                        line_range: i..i + 1,
                        scope: vec!["steps".to_string()],
                        visibility: Visibility::default(),
                        type_relations: Vec::new(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_compose() {
        let src = r#"version: "3.8"
services:
  web:
    image: nginx:latest
    ports:
      - "80:80"
  db:
    image: postgres:15
    environment:
      POSTGRES_DB: mydb
"#;
        let entities = extract_yaml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"version"),
            "missing version, got: {:?}",
            names
        );
        assert!(names.contains(&"services"), "missing services");
        assert!(names.contains(&"services.web"), "missing services.web");
        assert!(names.contains(&"services.db"), "missing services.db");
        assert!(
            names.contains(&"services.web.image"),
            "missing services.web.image"
        );

        // services.web and services.db should be Module kind.
        let web = entities.iter().find(|e| e.name == "services.web").unwrap();
        assert_eq!(web.kind, EntityKind::Module);
        let db = entities.iter().find(|e| e.name == "services.db").unwrap();
        assert_eq!(db.kind, EntityKind::Module);
    }

    #[test]
    fn github_actions() {
        let src = r#"name: CI
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - name: Checkout
        uses: actions/checkout@v4
      - name: Run tests
        run: cargo test
  lint:
    runs-on: ubuntu-latest
    steps:
      - name: Clippy
        run: cargo clippy
"#;
        let entities = extract_yaml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"jobs.test"), "missing jobs.test");
        assert!(names.contains(&"jobs.lint"), "missing jobs.lint");

        // jobs.test should be Module.
        let test_job = entities.iter().find(|e| e.name == "jobs.test").unwrap();
        assert_eq!(test_job.kind, EntityKind::Module);

        // Step names should be Function.
        let steps: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert!(
            steps.iter().any(|e| e.name == "Checkout"),
            "missing step Checkout"
        );
        assert!(
            steps.iter().any(|e| e.name == "Run tests"),
            "missing step 'Run tests'"
        );
    }

    #[test]
    fn kubernetes_manifest() {
        let src = r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx-deployment
  labels:
    app: nginx
spec:
  replicas: 3
  selector:
    matchLabels:
      app: nginx
"#;
        let entities = extract_yaml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"apiVersion"),
            "missing apiVersion, got: {:?}",
            names
        );
        // kind: Deployment should produce a Type entity.
        let dep = entities
            .iter()
            .find(|e| e.kind == EntityKind::Type)
            .unwrap();
        assert_eq!(dep.name, "Deployment");
    }

    #[test]
    fn plain_yaml_depth_limit() {
        let src = r#"a:
  b:
    c:
      d:
        e:
          f: deep
"#;
        let entities = extract_yaml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        // Depth 0..3 should be extracted (a, a.b, a.b.c, a.b.c.d).
        assert!(names.contains(&"a"));
        assert!(names.contains(&"a.b"));
        assert!(names.contains(&"a.b.c"));
        assert!(names.contains(&"a.b.c.d"));
        // Depth 4+ should NOT be extracted.
        assert!(!names.iter().any(|n| n.contains('e')));
    }

    #[test]
    fn yaml_comment_as_doc() {
        let src = r#"# The application port
port: 8080
"#;
        let entities = extract_yaml_entities(src);
        let port = entities.iter().find(|e| e.name == "port").unwrap();
        assert_eq!(port.doc_comment.as_deref(), Some("The application port"));
    }
}
