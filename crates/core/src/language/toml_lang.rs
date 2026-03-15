//! TOML language support — line-based symbol extraction.
//!
//! Extracts `[section]` / `[[array]]` headers and top-level `key = value` pairs.
//! Recognises Cargo.toml: `[dependencies.X]` -> Module, `[package]` name -> Type.

use super::{ConfigLanguageSupport, EntityKind, Language, SemanticEntity};

pub struct TomlLanguage;

impl ConfigLanguageSupport for TomlLanguage {
    fn language(&self) -> Language {
        Language::Toml
    }

    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity> {
        let text = String::from_utf8_lossy(source);
        extract_toml_entities(&text)
    }
}

/// Extract the comment on the line immediately preceding `line_idx`, if any.
fn preceding_comment(lines: &[&str], line_idx: usize) -> Option<String> {
    if line_idx == 0 {
        return None;
    }
    let prev = lines[line_idx - 1].trim();
    prev.strip_prefix('#').map(|comment| comment.trim().to_string())
}

fn extract_toml_entities(text: &str) -> Vec<SemanticEntity> {
    let lines: Vec<&str> = text.lines().collect();
    let mut entities = Vec::new();
    let mut current_section: Option<String> = None;
    let mut is_package_section = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let byte_start: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        let byte_end = byte_start + line.len();
        let doc_comment = preceding_comment(&lines, i);

        // `[[array.of.tables]]`
        if trimmed.starts_with("[[") && trimmed.ends_with("]]") {
            let section = trimmed[2..trimmed.len() - 2].trim();
            current_section = Some(section.to_string());
            is_package_section = false;

            entities.push(SemanticEntity {
                kind: EntityKind::Module,
                name: section.to_string(),
                signature: Some(trimmed.to_string()),
                doc_comment,
                byte_range: byte_start..byte_end,
                line_range: i..i + 1,
                scope: vec![],
            });
            continue;
        }

        // `[section]` or `[section.subsection]`
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = trimmed[1..trimmed.len() - 1].trim();
            is_package_section = section == "package";
            current_section = Some(section.to_string());

            let kind = EntityKind::Module;

            entities.push(SemanticEntity {
                kind,
                name: section.to_string(),
                signature: Some(trimmed.to_string()),
                doc_comment,
                byte_range: byte_start..byte_end,
                line_range: i..i + 1,
                scope: vec![],
            });
            continue;
        }

        // `key = value` at any nesting level.
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim();
            let value = trimmed[eq_pos + 1..].trim();

            // Skip keys with unusual characters.
            if key.is_empty() || key.contains(' ') && !key.starts_with('"') {
                continue;
            }
            let clean_key = key.trim_matches('"');
            if clean_key.is_empty() {
                continue;
            }

            let scope: Vec<String> = current_section
                .as_ref()
                .map(|s| vec![s.clone()])
                .unwrap_or_default();

            let full_name = if let Some(ref section) = current_section {
                format!("{}.{}", section, clean_key)
            } else {
                clean_key.to_string()
            };

            // Cargo.toml [package] name -> Type
            let kind = if is_package_section && clean_key == "name" {
                EntityKind::Type
            } else {
                EntityKind::Variable
            };

            // Truncate long values for signature.
            let display_val = if value.len() > 60 {
                format!("{}...", &value[..57])
            } else {
                value.to_string()
            };

            entities.push(SemanticEntity {
                kind,
                name: full_name,
                signature: Some(format!("{} = {}", clean_key, display_val)),
                doc_comment,
                byte_range: byte_start..byte_end,
                line_range: i..i + 1,
                scope,
            });
        }
    }

    entities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_toml() {
        let src = r#"[package]
name = "codixing"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
tokio = "1"

[dependencies.clap]
version = "4"
features = ["derive"]
"#;
        let entities = extract_toml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"package"), "missing [package]");
        assert!(
            names.contains(&"package.name"),
            "missing package.name, got: {:?}",
            names
        );
        assert!(names.contains(&"package.version"));
        assert!(names.contains(&"dependencies"));
        assert!(names.contains(&"dependencies.clap"));

        // package.name should be Type.
        let pkg_name = entities.iter().find(|e| e.name == "package.name").unwrap();
        assert_eq!(pkg_name.kind, EntityKind::Type);

        // [dependencies.clap] should be Module.
        let clap = entities.iter().find(|e| e.name == "dependencies.clap").unwrap();
        assert_eq!(clap.kind, EntityKind::Module);
    }

    #[test]
    fn pyproject_toml() {
        let src = r#"[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[project]
name = "my-package"
version = "0.1.0"

[project.optional-dependencies]
dev = ["pytest", "ruff"]
"#;
        let entities = extract_toml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"build-system"));
        assert!(names.contains(&"project"));
        assert!(names.contains(&"project.optional-dependencies"));
    }

    #[test]
    fn generic_toml() {
        let src = r#"# Global title
title = "My Config"

[database]
host = "localhost"
port = 5432

[[servers]]
name = "alpha"
ip = "10.0.0.1"

[[servers]]
name = "beta"
ip = "10.0.0.2"
"#;
        let entities = extract_toml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"title"), "missing top-level title");
        assert!(names.contains(&"database"));
        assert!(names.contains(&"database.host"));
        assert!(names.contains(&"servers"));

        // title should have doc comment.
        let title = entities.iter().find(|e| e.name == "title").unwrap();
        assert_eq!(title.doc_comment.as_deref(), Some("Global title"));

        // [[servers]] should be Module.
        let servers = entities
            .iter()
            .find(|e| e.name == "servers" && e.kind == EntityKind::Module)
            .unwrap();
        assert!(servers.signature.as_ref().unwrap().contains("[[servers]]"));
    }
}
