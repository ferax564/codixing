//! Dockerfile language support — line-based symbol extraction.
//!
//! Extracts:
//! - `FROM image AS stage` -> Module (the stage name)
//! - `EXPOSE port` -> Variable
//! - `ENV key=value` / `ENV key value` -> Variable
//! - `ARG name` / `ARG name=default` -> Variable
//! - `LABEL key=value` -> Variable
//! - `ENTRYPOINT` / `CMD` -> Function

use super::{ConfigLanguageSupport, EntityKind, Language, SemanticEntity, Visibility};

pub struct DockerfileLanguage;

impl ConfigLanguageSupport for DockerfileLanguage {
    fn language(&self) -> Language {
        Language::Dockerfile
    }

    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity> {
        let text = String::from_utf8_lossy(source);
        extract_dockerfile_entities(&text)
    }
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

fn extract_dockerfile_entities(text: &str) -> Vec<SemanticEntity> {
    let lines: Vec<&str> = text.lines().collect();
    let mut entities = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let byte_start: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        let byte_end = byte_start + line.len();
        let doc_comment = preceding_comment(&lines, i);

        let upper = trimmed.to_uppercase();

        // FROM image AS stage
        if upper.starts_with("FROM ") {
            let rest = trimmed[5..].trim();
            // Check for AS alias.
            if let Some(as_pos) = rest.to_uppercase().find(" AS ") {
                let stage = rest[as_pos + 4..].trim();
                if !stage.is_empty() {
                    entities.push(SemanticEntity {
                        kind: EntityKind::Module,
                        name: stage.to_string(),
                        signature: Some(trimmed.to_string()),
                        doc_comment: doc_comment.clone(),
                        byte_range: byte_start..byte_end,
                        line_range: i..i + 1,
                        scope: vec![],
                        visibility: Visibility::default(),
                        type_relations: Vec::new(),
                    });
                }
            }
            // Also emit the FROM image as a Variable for searchability.
            let image = if let Some(as_pos) = rest.to_uppercase().find(" AS ") {
                rest[..as_pos].trim()
            } else {
                rest
            };
            if !image.is_empty() {
                entities.push(SemanticEntity {
                    kind: EntityKind::Variable,
                    name: format!("FROM {}", image),
                    signature: Some(trimmed.to_string()),
                    doc_comment,
                    byte_range: byte_start..byte_end,
                    line_range: i..i + 1,
                    scope: vec![],
                    visibility: Visibility::default(),
                    type_relations: Vec::new(),
                });
            }
            continue;
        }

        // EXPOSE port
        if upper.starts_with("EXPOSE ") {
            let port = trimmed[7..].trim();
            entities.push(SemanticEntity {
                kind: EntityKind::Variable,
                name: format!("EXPOSE {}", port),
                signature: Some(trimmed.to_string()),
                doc_comment,
                byte_range: byte_start..byte_end,
                line_range: i..i + 1,
                scope: vec![],
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
            continue;
        }

        // ENV key=value or ENV key value
        if upper.starts_with("ENV ") {
            let rest = trimmed[4..].trim();
            for env_entry in parse_env_entries(rest) {
                entities.push(SemanticEntity {
                    kind: EntityKind::Variable,
                    name: env_entry.clone(),
                    signature: Some(format!("ENV {}", env_entry)),
                    doc_comment: doc_comment.clone(),
                    byte_range: byte_start..byte_end,
                    line_range: i..i + 1,
                    scope: vec![],
                    visibility: Visibility::default(),
                    type_relations: Vec::new(),
                });
            }
            continue;
        }

        // ARG name or ARG name=default
        if upper.starts_with("ARG ") {
            let rest = trimmed[4..].trim();
            let name = if let Some(eq) = rest.find('=') {
                &rest[..eq]
            } else {
                rest
            };
            if !name.is_empty() {
                entities.push(SemanticEntity {
                    kind: EntityKind::Variable,
                    name: name.to_string(),
                    signature: Some(trimmed.to_string()),
                    doc_comment,
                    byte_range: byte_start..byte_end,
                    line_range: i..i + 1,
                    scope: vec![],
                    visibility: Visibility::default(),
                    type_relations: Vec::new(),
                });
            }
            continue;
        }

        // LABEL key=value
        if upper.starts_with("LABEL ") {
            let rest = trimmed[6..].trim();
            if let Some(eq) = rest.find('=') {
                let key = rest[..eq].trim();
                if !key.is_empty() {
                    entities.push(SemanticEntity {
                        kind: EntityKind::Variable,
                        name: format!("LABEL {}", key),
                        signature: Some(trimmed.to_string()),
                        doc_comment,
                        byte_range: byte_start..byte_end,
                        line_range: i..i + 1,
                        scope: vec![],
                        visibility: Visibility::default(),
                        type_relations: Vec::new(),
                    });
                }
            }
            continue;
        }

        // ENTRYPOINT
        if upper.starts_with("ENTRYPOINT") {
            let rest = trimmed.get(10..).unwrap_or("").trim();
            entities.push(SemanticEntity {
                kind: EntityKind::Function,
                name: "ENTRYPOINT".to_string(),
                signature: Some(format!("ENTRYPOINT {}", rest)),
                doc_comment,
                byte_range: byte_start..byte_end,
                line_range: i..i + 1,
                scope: vec![],
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
            continue;
        }

        // CMD
        if upper.starts_with("CMD")
            && (trimmed.len() == 3
                || trimmed.as_bytes().get(3) == Some(&b' ')
                || trimmed.as_bytes().get(3) == Some(&b'['))
        {
            let rest = trimmed.get(3..).unwrap_or("").trim();
            entities.push(SemanticEntity {
                kind: EntityKind::Function,
                name: "CMD".to_string(),
                signature: Some(format!("CMD {}", rest)),
                doc_comment,
                byte_range: byte_start..byte_end,
                line_range: i..i + 1,
                scope: vec![],
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
        }
    }

    entities
}

/// Parse ENV entries. Supports both `ENV KEY=value` and `ENV KEY value` forms.
fn parse_env_entries(rest: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(eq) = rest.find('=') {
        // KEY=value form, possibly multiple: KEY1=val1 KEY2=val2
        for part in rest.split_whitespace() {
            if let Some(eq_pos) = part.find('=') {
                let key = &part[..eq_pos];
                if !key.is_empty() {
                    names.push(key.to_string());
                }
            }
        }
        // If no = was found in split parts, use the whole key before first =.
        if names.is_empty() {
            let key = rest[..eq].trim();
            if !key.is_empty() {
                names.push(key.to_string());
            }
        }
    } else {
        // ENV KEY value form.
        let key = rest.split_whitespace().next().unwrap_or("");
        if !key.is_empty() {
            names.push(key.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_stage_build() {
        let src = r#"# Build stage
FROM rust:1.75 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim AS runtime
COPY --from=builder /app/target/release/myapp /usr/local/bin/
ENTRYPOINT ["myapp"]
CMD ["--help"]
"#;
        let entities = extract_dockerfile_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        // Stages.
        assert!(
            names.contains(&"builder"),
            "missing stage builder, got: {:?}",
            names
        );
        assert!(names.contains(&"runtime"), "missing stage runtime");

        let builder = entities.iter().find(|e| e.name == "builder").unwrap();
        assert_eq!(builder.kind, EntityKind::Module);
        assert_eq!(builder.doc_comment.as_deref(), Some("Build stage"));

        // ENTRYPOINT and CMD.
        let ep = entities.iter().find(|e| e.name == "ENTRYPOINT").unwrap();
        assert_eq!(ep.kind, EntityKind::Function);
        let cmd = entities.iter().find(|e| e.name == "CMD").unwrap();
        assert_eq!(cmd.kind, EntityKind::Function);
    }

    #[test]
    fn env_and_arg_extraction() {
        let src = r#"FROM python:3.12
ARG VERSION=1.0
ARG BUILD_TYPE
ENV APP_HOME=/app
ENV DEBUG=true LOG_LEVEL=info
EXPOSE 8080
EXPOSE 443/tcp
LABEL maintainer="dev@example.com"
"#;
        let entities = extract_dockerfile_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"VERSION"), "missing ARG VERSION");
        assert!(names.contains(&"BUILD_TYPE"), "missing ARG BUILD_TYPE");
        assert!(names.contains(&"APP_HOME"), "missing ENV APP_HOME");
        assert!(names.contains(&"DEBUG"), "missing ENV DEBUG");
        assert!(names.contains(&"LOG_LEVEL"), "missing ENV LOG_LEVEL");
        assert!(
            names.contains(&"EXPOSE 8080"),
            "missing EXPOSE 8080, got: {:?}",
            names
        );
        assert!(names.contains(&"EXPOSE 443/tcp"));
        assert!(names.contains(&"LABEL maintainer"));

        // All ENV/ARG should be Variable.
        for entity in &entities {
            if entity.name == "VERSION" || entity.name == "APP_HOME" {
                assert_eq!(entity.kind, EntityKind::Variable);
            }
        }
    }

    #[test]
    fn from_images_recorded() {
        let src = "FROM node:20-alpine\n";
        let entities = extract_dockerfile_entities(src);
        let from = entities
            .iter()
            .find(|e| e.name == "FROM node:20-alpine")
            .unwrap();
        assert_eq!(from.kind, EntityKind::Variable);
    }
}
