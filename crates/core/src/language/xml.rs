//! XML/Draw.io language support — line-based symbol extraction using `quick-xml`.
//!
//! Extracts elements with id/name/label attributes from XML files, and
//! recognises Draw.io (`.drawio`) format by detecting the `mxGraphModel` tag.

use quick_xml::events::Event;
use quick_xml::reader::Reader;

use super::{ConfigLanguageSupport, EntityKind, Language, SemanticEntity, Visibility};

pub struct XmlLanguage;

impl ConfigLanguageSupport for XmlLanguage {
    fn language(&self) -> Language {
        Language::Xml
    }

    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity> {
        let text = String::from_utf8_lossy(source);
        extract_xml_entities(&text)
    }
}

/// Extract relevant attributes from an element's attributes list.
///
/// Returns `(id, name, label, tag_name)`.
fn extract_attrs(
    e: &quick_xml::events::BytesStart<'_>,
) -> (Option<String>, Option<String>, Option<String>, String) {
    let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
    let mut id = None;
    let mut name = None;
    let mut label = None;

    for attr in e.attributes().filter_map(|a| a.ok()) {
        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
        let val = String::from_utf8_lossy(&attr.value).to_string();
        match key.as_str() {
            "id" => id = Some(val),
            "name" => name = Some(val),
            "label" => label = Some(val),
            "value" if tag_name == "mxCell" => label = Some(val),
            _ => {}
        }
    }

    (id, name, label, tag_name)
}

/// Emit a `SemanticEntity` for an XML element if it has useful attributes.
fn emit_entity(
    id: Option<String>,
    name: Option<String>,
    label: Option<String>,
    tag_name: &str,
    scope: &[String],
    is_drawio: bool,
    offset: usize,
) -> Option<SemanticEntity> {
    if is_drawio {
        // In Draw.io mode, only emit mxCell elements with meaningful labels.
        if tag_name != "mxCell" {
            return None;
        }
        let raw_label = label.as_deref().unwrap_or("");
        if raw_label.is_empty() {
            return None;
        }
        let clean = strip_html_tags(raw_label);
        if clean.is_empty() {
            return None;
        }
        let entity_name = if let Some(ref id_val) = id {
            format!("{id_val}: {clean}")
        } else {
            clean.clone()
        };
        return Some(SemanticEntity {
            kind: EntityKind::Variable,
            name: entity_name,
            signature: Some(format!("<mxCell value=\"{clean}\" />")),
            doc_comment: None,
            byte_range: offset..offset + 1,
            line_range: 0..1,
            scope: scope.to_vec(),
            visibility: Visibility::default(),
            type_relations: Vec::new(),
        });
    }

    // General XML: emit elements that have an id, name, or label attribute.
    let display_name = name
        .as_deref()
        .or(label.as_deref())
        .or(id.as_deref())
        .unwrap_or("");
    if display_name.is_empty() {
        return None;
    }
    let sig = if let Some(ref id_val) = id {
        format!("<{tag_name} id=\"{id_val}\">")
    } else {
        format!("<{tag_name} name=\"{display_name}\">")
    };
    Some(SemanticEntity {
        kind: EntityKind::Variable,
        name: display_name.to_string(),
        signature: Some(sig),
        doc_comment: None,
        byte_range: offset..offset + 1,
        line_range: 0..1,
        scope: scope.to_vec(),
        visibility: Visibility::default(),
        type_relations: Vec::new(),
    })
}

fn extract_xml_entities(text: &str) -> Vec<SemanticEntity> {
    let mut reader = Reader::from_str(text);
    let mut entities = Vec::new();
    let mut scope: Vec<String> = Vec::new();
    let mut is_drawio = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let (id, name, label, tag_name) = extract_attrs(e);

                // Detect Draw.io format.
                if tag_name == "mxGraphModel" {
                    is_drawio = true;
                }

                let offset = reader.buffer_position() as usize;
                if let Some(entity) =
                    emit_entity(id, name, label, &tag_name, &scope, is_drawio, offset)
                {
                    entities.push(entity);
                }

                // Push to scope stack for Start elements (NOT Empty).
                scope.push(tag_name);
            }
            Ok(Event::Empty(ref e)) => {
                let (id, name, label, tag_name) = extract_attrs(e);
                let offset = reader.buffer_position() as usize;
                if let Some(entity) =
                    emit_entity(id, name, label, &tag_name, &scope, is_drawio, offset)
                {
                    entities.push(entity);
                }
                // Self-closing elements do NOT push to scope.
            }
            Ok(Event::End(_)) => {
                scope.pop();
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    entities
}

/// Strip HTML tags from a string (used for Draw.io cell labels).
fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_xml_elements() {
        let src = r#"<?xml version="1.0"?>
<project>
    <module id="core" name="Core Engine">
        <component name="Parser" />
    </module>
    <module id="cli" name="CLI" />
</project>
"#;
        let entities = extract_xml_entities(src);
        assert!(
            entities.len() >= 3,
            "expected at least 3 entities, got {} — {:?}",
            entities.len(),
            entities.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("Core Engine")),
            "missing Core Engine in {:?}",
            names
        );
        assert!(
            names.iter().any(|n| n.contains("Parser")),
            "missing Parser in {:?}",
            names
        );
        assert!(
            names.iter().any(|n| n.contains("CLI")),
            "missing CLI in {:?}",
            names
        );
    }

    #[test]
    fn extract_drawio_cells() {
        let src = r#"<?xml version="1.0"?>
<mxGraphModel>
    <root>
        <mxCell id="0" />
        <mxCell id="1" parent="0" />
        <mxCell id="2" value="User Service" parent="1" vertex="1" />
        <mxCell id="3" value="Database" parent="1" vertex="1" />
        <mxCell id="4" value="" parent="1" edge="1" />
    </root>
</mxGraphModel>
"#;
        let entities = extract_xml_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        // Should extract cells with non-empty values.
        assert!(
            names.iter().any(|n| n.contains("User Service")),
            "missing User Service in {:?}",
            names
        );
        assert!(
            names.iter().any(|n| n.contains("Database")),
            "missing Database in {:?}",
            names
        );
        // The empty-value cell and edge should NOT appear.
        assert_eq!(
            entities.len(),
            2,
            "expected 2 drawio entities, got {}",
            entities.len()
        );
    }

    #[test]
    fn strip_html() {
        assert_eq!(strip_html_tags("<b>Hello</b> World"), "Hello World");
        assert_eq!(strip_html_tags("<div><p>Test</p></div>"), "Test");
        assert_eq!(strip_html_tags("No tags"), "No tags");
        assert_eq!(strip_html_tags("<br/>"), "");
    }
}
