//! Mermaid diagram language support — line-based symbol extraction.
//!
//! Extracts diagram types, subgraph labels, node definitions, and class
//! definitions from Mermaid diagram files (.mmd, .mermaid).

use super::{ConfigLanguageSupport, EntityKind, Language, SemanticEntity};

pub struct MermaidLanguage;

impl ConfigLanguageSupport for MermaidLanguage {
    fn language(&self) -> Language {
        Language::Mermaid
    }

    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity> {
        let text = String::from_utf8_lossy(source);
        extract_mermaid_entities(&text)
    }
}

/// Known Mermaid diagram type keywords.
const DIAGRAM_TYPES: &[&str] = &[
    "flowchart",
    "graph",
    "sequenceDiagram",
    "classDiagram",
    "stateDiagram",
    "stateDiagram-v2",
    "erDiagram",
    "gantt",
    "pie",
    "journey",
    "gitgraph",
    "mindmap",
    "timeline",
    "quadrantChart",
    "sankey-beta",
    "xychart-beta",
    "block-beta",
];

/// Extract the comment on the line immediately preceding `line_idx`, if any.
fn preceding_comment(lines: &[&str], line_idx: usize) -> Option<String> {
    if line_idx == 0 {
        return None;
    }
    let prev = lines[line_idx - 1].trim();
    prev.strip_prefix("%%")
        .map(|comment| comment.trim().to_string())
}

fn extract_mermaid_entities(text: &str) -> Vec<SemanticEntity> {
    let lines: Vec<&str> = text.lines().collect();
    let mut entities: Vec<SemanticEntity> = Vec::new();
    let mut in_class_diagram = false;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }

        let byte_start: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        let byte_end = byte_start + line.len();

        // Detect diagram type on the first non-comment, non-empty line.
        if entities.is_empty() || (entities.len() == 1 && entities[0].kind == EntityKind::Type) {
            for dt in DIAGRAM_TYPES {
                if trimmed.starts_with(dt) {
                    if *dt == "classDiagram" {
                        in_class_diagram = true;
                    }
                    // Only emit the type entity once.
                    if !entities.iter().any(|e| e.kind == EntityKind::Type) {
                        entities.push(SemanticEntity {
                            kind: EntityKind::Type,
                            name: dt.to_string(),
                            signature: Some(trimmed.to_string()),
                            doc_comment: preceding_comment(&lines, i),
                            byte_range: byte_start..byte_end,
                            line_range: i..i + 1,
                            scope: vec![],
                        });
                    }
                    break;
                }
            }
        }

        // Subgraph labels: `subgraph <label>`
        if let Some(rest) = trimmed.strip_prefix("subgraph") {
            let label = rest.trim();
            if !label.is_empty() {
                entities.push(SemanticEntity {
                    kind: EntityKind::Module,
                    name: label.to_string(),
                    signature: Some(trimmed.to_string()),
                    doc_comment: preceding_comment(&lines, i),
                    byte_range: byte_start..byte_end,
                    line_range: i..i + 1,
                    scope: vec![],
                });
            }
            continue;
        }

        // Class diagram: `class ClassName`
        if in_class_diagram {
            if let Some(rest) = trimmed.strip_prefix("class ") {
                // `class Foo {` or `class Foo`
                let name = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or(rest)
                    .trim_end_matches('{')
                    .trim();
                if !name.is_empty() {
                    entities.push(SemanticEntity {
                        kind: EntityKind::Class,
                        name: name.to_string(),
                        signature: Some(trimmed.to_string()),
                        doc_comment: preceding_comment(&lines, i),
                        byte_range: byte_start..byte_end,
                        line_range: i..i + 1,
                        scope: vec![],
                    });
                }
                continue;
            }
        }

        // Node definitions: id[label], id(label), id{label}, id((label))
        // Skip lines that are arrows/edges (contain -->  or --- or -.- etc.)
        if !trimmed.contains("-->")
            && !trimmed.contains("---")
            && !trimmed.contains("-.-")
            && !trimmed.contains("==>")
            && !trimmed.contains("~~~")
            && !trimmed.starts_with("end")
            && !trimmed.starts_with("style")
            && !trimmed.starts_with("linkStyle")
            && !trimmed.starts_with("click")
        {
            if let Some(entity) = parse_node_definition(trimmed, i, byte_start, byte_end, &lines) {
                entities.push(entity);
            }
        }
    }

    entities
}

/// Try to parse a Mermaid node definition: `id[label]`, `id(label)`, `id{label}`,
/// `id((label))`, `id>label]`, `id{{label}}`.
fn parse_node_definition(
    trimmed: &str,
    line_idx: usize,
    byte_start: usize,
    byte_end: usize,
    lines: &[&str],
) -> Option<SemanticEntity> {
    // Find the opening bracket/paren/brace.
    let openers = ['[', '(', '{'];
    let open_pos = trimmed.find(openers.as_ref())?;
    let id = trimmed[..open_pos].trim();

    // ID must be non-empty and look like an identifier (alphanumeric, _, -).
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }

    // Extract label between matched brackets.
    let open_char = trimmed.as_bytes()[open_pos] as char;
    let close_char = match open_char {
        '[' => ']',
        '(' => ')',
        '{' => '}',
        _ => return None,
    };

    let rest = &trimmed[open_pos + 1..];
    let close_pos = rest.rfind(close_char)?;
    let label = rest[..close_pos]
        .trim()
        .trim_start_matches(open_char)
        .trim_end_matches(close_char)
        .trim_matches('"')
        .trim();

    if label.is_empty() {
        return None;
    }

    Some(SemanticEntity {
        kind: EntityKind::Variable,
        name: format!("{id}: {label}"),
        signature: Some(trimmed.to_string()),
        doc_comment: preceding_comment(lines, line_idx),
        byte_range: byte_start..byte_end,
        line_range: line_idx..line_idx + 1,
        scope: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_diagram_type() {
        let src = "flowchart TD\n    A --> B\n";
        let entities = extract_mermaid_entities(src);
        let types: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Type)
            .collect();
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].name, "flowchart");
    }

    #[test]
    fn extract_subgraph() {
        let src = r#"flowchart TD
    subgraph Authentication
        A[Login] --> B[Verify]
    end
    subgraph Dashboard
        C[Home] --> D[Settings]
    end
"#;
        let entities = extract_mermaid_entities(src);
        let modules: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert_eq!(modules.len(), 2);
        assert!(modules.iter().any(|m| m.name == "Authentication"));
        assert!(modules.iter().any(|m| m.name == "Dashboard"));
    }

    #[test]
    fn extract_node_definitions() {
        let src = r#"flowchart LR
    A[Start]
    B(Process)
    C{Decision}
"#;
        let entities = extract_mermaid_entities(src);
        let nodes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Variable)
            .collect();
        assert_eq!(nodes.len(), 3);
        assert!(nodes.iter().any(|n| n.name.contains("Start")));
        assert!(nodes.iter().any(|n| n.name.contains("Process")));
        assert!(nodes.iter().any(|n| n.name.contains("Decision")));
    }

    #[test]
    fn extract_class_diagram() {
        let src = r#"classDiagram
    class Animal {
        +String name
        +makeSound()
    }
    class Dog {
        +fetch()
    }
    Animal <|-- Dog
"#;
        let entities = extract_mermaid_entities(src);
        let classes: Vec<_> = entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 2);
        assert!(classes.iter().any(|c| c.name == "Animal"));
        assert!(classes.iter().any(|c| c.name == "Dog"));
    }
}
