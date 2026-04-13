//! Assembly language support — line-based label and directive extraction.
//!
//! Assembly is grep-first: the primary goal is making `.S` / `.s` / `.asm`
//! files visible to `codixing grep` so kernel/embedded repos have full
//! coverage. Symbol extraction is intentionally lightweight:
//!
//! - `label:` on its own line → Function entity
//! - `.globl name` / `.global name` → marks visibility (no separate entity)
//! - `#` / `//` / `;` / `/* ... */` preceding a label → doc comment
//!
//! More sophisticated extraction (cfi directives, macro definitions,
//! expanded AT&T vs Intel syntax) can be layered on top later.

use super::{ConfigLanguageSupport, EntityKind, Language, SemanticEntity, Visibility};

pub struct AssemblyLanguage;

impl ConfigLanguageSupport for AssemblyLanguage {
    fn language(&self) -> Language {
        Language::Assembly
    }

    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity> {
        let text = String::from_utf8_lossy(source);
        extract_assembly_entities(&text)
    }
}

fn extract_assembly_entities(text: &str) -> Vec<SemanticEntity> {
    // Walk the raw source bytes so `byte_range` lines up with the real
    // content regardless of line ending flavour (LF, CRLF, or mixed).
    // `str::lines()` strips trailing `\r\n` or `\n`, so computing offsets
    // from `line.len() + 1` would undercount on CRLF input.
    let mut lines: Vec<&str> = Vec::new();
    let mut line_starts: Vec<usize> = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    line_starts.push(0);
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let line_end = if i > 0 && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            let start = *line_starts.last().unwrap();
            lines.push(&text[start..line_end]);
            line_starts.push(i + 1);
        }
        i += 1;
    }
    // Final line (no trailing newline).
    let start = *line_starts.last().unwrap();
    if start <= bytes.len() {
        lines.push(&text[start..bytes.len()]);
    }
    let mut entities = Vec::new();

    // First pass: collect names marked `.globl` / `.global` (space or tab).
    let mut global_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in &lines {
        let trimmed = line.trim();
        let rest = [".global", ".globl"].iter().find_map(|dir| {
            let stripped = trimmed.strip_prefix(dir)?;
            // Must be followed by whitespace to avoid matching `.globalize`,
            // `.globlike`, etc. GAS accepts space OR tab.
            match stripped.chars().next() {
                Some(c) if c == ' ' || c == '\t' => Some(stripped.trim_start()),
                None => Some(""),
                _ => None,
            }
        });
        if let Some(rest) = rest {
            for name in rest.split(|c: char| c == ',' || c.is_whitespace()) {
                let name = name.trim();
                if !name.is_empty() {
                    global_names.insert(name.to_string());
                }
            }
        }
    }

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || is_comment_line(trimmed) {
            continue;
        }

        // Label: identifier followed by `:` with optional trailing content.
        // Must start at column 0 (labels are unindented in gas syntax).
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Some(label_name) = parse_label(trimmed) {
                // Skip local labels (numeric or starting with `.L`).
                if label_name.starts_with(".L") || label_name.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }

                // `line_starts[i]` points at this line's first byte in the
                // original source. `line.len()` excludes the line terminator,
                // so `start + len` lands on the LF / CR without including it.
                let byte_start = line_starts[i];
                let byte_end = byte_start + line.len();

                let visibility = if global_names.contains(label_name) {
                    Visibility::Public
                } else {
                    Visibility::Private
                };

                entities.push(SemanticEntity {
                    kind: EntityKind::Function,
                    name: label_name.to_string(),
                    signature: Some(format!("{label_name}:")),
                    doc_comment: preceding_comment(&lines, i),
                    byte_range: byte_start..byte_end,
                    line_range: i..i + 1,
                    scope: vec![],
                    visibility,
                    type_relations: Vec::new(),
                });
            }
        }
    }

    entities
}

/// Return the label name if `line` is of the form `name:` (with optional
/// trailing content after the colon).
fn parse_label(line: &str) -> Option<&str> {
    let colon = line.find(':')?;
    let name = line[..colon].trim();
    if name.is_empty() {
        return None;
    }
    // Assembly label names: letters, digits, `_`, `.`, `$`. Must start with
    // a letter / `_` / `.` (to match gas syntax).
    let first = name.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_' || first == '.') {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$')
    {
        return None;
    }
    Some(name)
}

fn is_comment_line(trimmed: &str) -> bool {
    trimmed.starts_with('#')
        || trimmed.starts_with("//")
        || trimmed.starts_with(';')
        || trimmed.starts_with("/*")
}

fn preceding_comment(lines: &[&str], line_idx: usize) -> Option<String> {
    if line_idx == 0 {
        return None;
    }
    let mut comments = Vec::new();
    let mut j = line_idx;
    while j > 0 {
        j -= 1;
        let prev = lines[j].trim();
        let comment = if let Some(c) = prev.strip_prefix('#') {
            c.trim().to_string()
        } else if let Some(c) = prev.strip_prefix("//") {
            c.trim().to_string()
        } else if let Some(c) = prev.strip_prefix(';') {
            c.trim().to_string()
        } else {
            break;
        };
        comments.push(comment);
    }
    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    Some(comments.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_labels_and_globals() {
        let src = r#"# Entry point
.globl _start, main

.text

# Reset handler
_start:
    mov x0, #0
    b main

# Local branch — should NOT be extracted
.L_loop:
    nop
    b .L_loop

main:
    ret
"#;
        let entities = extract_assembly_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&"_start"), "missing _start, got: {names:?}");
        assert!(names.contains(&"main"), "missing main, got: {names:?}");
        assert!(
            !names.contains(&".L_loop"),
            "local label .L_loop should be skipped"
        );

        let start = entities.iter().find(|e| e.name == "_start").unwrap();
        assert_eq!(start.visibility, Visibility::Public);
        assert_eq!(start.doc_comment.as_deref(), Some("Reset handler"));

        let main = entities.iter().find(|e| e.name == "main").unwrap();
        assert_eq!(main.visibility, Visibility::Public);
    }

    #[test]
    fn globl_directive_with_tab_separator() {
        // GAS accepts tabs after `.globl` / `.global`. Earlier code only
        // stripped the literal-space form and missed tab-separated forms,
        // causing the label to be extracted as Private instead of Public.
        let src = "\t.globl\tentry_tab\nentry_tab:\n\tret\n";
        let entities = extract_assembly_entities(src);
        let entry = entities
            .iter()
            .find(|e| e.name == "entry_tab")
            .expect("entry_tab label should be extracted");
        assert_eq!(
            entry.visibility,
            Visibility::Public,
            "tab-separated .globl should mark symbol Public"
        );
    }

    #[test]
    fn byte_range_handles_crlf_line_endings() {
        // v0.37: use source byte offsets (not `line.len() + 1`) so CRLF
        // doesn't offset by one per line.
        let src = "# header\r\nfoo:\r\n\tret\r\n";
        let entities = extract_assembly_entities(src);
        let foo = entities.iter().find(|e| e.name == "foo").unwrap();
        // Expected: "# header\r\n" = 10 bytes, then "foo:" starts at byte 10.
        assert_eq!(foo.byte_range.start, 10);
        assert_eq!(foo.byte_range.end, 14); // "foo:" is 4 bytes
    }

    #[test]
    fn skips_numeric_and_local_labels() {
        let src = r#"foo:
    b 1f
1:
    nop
2:
    b 2b
.L42:
    ret
"#;
        let entities = extract_assembly_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["foo"]);
    }
}
