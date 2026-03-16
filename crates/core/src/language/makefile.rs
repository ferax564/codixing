//! Makefile language support — line-based symbol extraction.
//!
//! Extracts:
//! - `target:` (not indented, contains `:`) -> Function
//! - `VARIABLE = value` / `VARIABLE := value` / `VARIABLE ?= value` -> Variable
//! - `.PHONY: targets` -> marks targets
//! - `include other.mk` -> Import

use super::{ConfigLanguageSupport, EntityKind, Language, SemanticEntity};

pub struct MakefileLanguage;

impl ConfigLanguageSupport for MakefileLanguage {
    fn language(&self) -> Language {
        Language::Makefile
    }

    fn extract_entities(&self, source: &[u8]) -> Vec<SemanticEntity> {
        let text = String::from_utf8_lossy(source);
        extract_makefile_entities(&text)
    }
}

/// Extract the comment on the line immediately preceding `line_idx`, if any.
fn preceding_comment(lines: &[&str], line_idx: usize) -> Option<String> {
    if line_idx == 0 {
        return None;
    }
    let mut comments = Vec::new();
    let mut j = line_idx;
    while j > 0 {
        j -= 1;
        let prev = lines[j].trim();
        if let Some(comment) = prev.strip_prefix('#') {
            comments.push(comment.trim().to_string());
        } else {
            break;
        }
    }
    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    Some(comments.join("\n"))
}

fn extract_makefile_entities(text: &str) -> Vec<SemanticEntity> {
    let lines: Vec<&str> = text.lines().collect();
    let mut entities = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let byte_start: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
        let byte_end = byte_start + line.len();

        // `include other.mk` or `-include other.mk`
        let include_line = trimmed
            .strip_prefix("include ")
            .or_else(|| trimmed.strip_prefix("-include "));
        if let Some(rest) = include_line {
            let path = rest.trim();
            if !path.is_empty() {
                entities.push(SemanticEntity {
                    kind: EntityKind::Import,
                    name: path.to_string(),
                    signature: Some(trimmed.to_string()),
                    doc_comment: preceding_comment(&lines, i),
                    byte_range: byte_start..byte_end,
                    line_range: i..i + 1,
                    scope: vec![],
                });
            }
            continue;
        }

        // Variable assignment: must not start with tab (recipe line) and must
        // contain one of =, :=, ?=, +=, !=
        if !line.starts_with('\t') {
            if let Some((var_name, op)) = parse_variable_assignment(trimmed) {
                if !var_name.is_empty() && !var_name.starts_with('.') && !var_name.contains(':') {
                    let doc_comment = preceding_comment(&lines, i);
                    let value_start = trimmed.find(op).unwrap_or(0) + op.len();
                    let value = trimmed.get(value_start..).unwrap_or("").trim();
                    let display_val = if value.len() > 60 {
                        format!("{}...", &value[..57])
                    } else {
                        value.to_string()
                    };
                    entities.push(SemanticEntity {
                        kind: EntityKind::Variable,
                        name: var_name.to_string(),
                        signature: Some(format!("{} {} {}", var_name, op, display_val)),
                        doc_comment,
                        byte_range: byte_start..byte_end,
                        line_range: i..i + 1,
                        scope: vec![],
                    });
                    continue;
                }
            }
        }

        // Target: non-indented line containing `:` that's not a variable assignment.
        // Must not start with a tab.
        if !line.starts_with('\t') && !line.starts_with(' ') {
            if let Some(colon_pos) = trimmed.find(':') {
                // Skip if it looks like a variable assignment (we already handled those above).
                let after_colon = &trimmed[colon_pos..];
                if after_colon.starts_with(":=") || after_colon.starts_with("::") {
                    continue;
                }

                let target_part = trimmed[..colon_pos].trim();
                // Skip .PHONY itself but extract its targets list.
                if target_part == ".PHONY" {
                    let phony_targets = trimmed[colon_pos + 1..].trim();
                    // The actual targets will be picked up as target rules elsewhere.
                    // Just note the .PHONY declaration.
                    if !phony_targets.is_empty() {
                        entities.push(SemanticEntity {
                            kind: EntityKind::Variable,
                            name: ".PHONY".to_string(),
                            signature: Some(trimmed.to_string()),
                            doc_comment: preceding_comment(&lines, i),
                            byte_range: byte_start..byte_end,
                            line_range: i..i + 1,
                            scope: vec![],
                        });
                    }
                    continue;
                }

                // Skip other special targets (.DEFAULT, .SUFFIXES, etc.) except as entities.
                if target_part.starts_with('.') {
                    continue;
                }

                // Could be multiple targets: `all clean: ...`
                for target in target_part.split_whitespace() {
                    // Skip variables like $(FOO).
                    if target.starts_with('$') {
                        continue;
                    }
                    let deps = trimmed[colon_pos + 1..].trim();
                    let sig = if deps.is_empty() {
                        format!("{}:", target)
                    } else {
                        let display_deps = if deps.len() > 60 {
                            format!("{}...", &deps[..57])
                        } else {
                            deps.to_string()
                        };
                        format!("{}: {}", target, display_deps)
                    };
                    entities.push(SemanticEntity {
                        kind: EntityKind::Function,
                        name: target.to_string(),
                        signature: Some(sig),
                        doc_comment: preceding_comment(&lines, i),
                        byte_range: byte_start..byte_end,
                        line_range: i..i + 1,
                        scope: vec![],
                    });
                }
            }
        }
    }

    entities
}

/// Try to parse a variable assignment. Returns `(name, operator)` if successful.
fn parse_variable_assignment(line: &str) -> Option<(&str, &str)> {
    // Order matters: check multi-char operators first.
    for op in &[":=", "?=", "+=", "!="] {
        if let Some(pos) = line.find(op) {
            let name = line[..pos].trim();
            // Validate: name should be a simple identifier (no spaces, no colons).
            if !name.is_empty() && !name.contains(' ') && !name.contains('\t') {
                return Some((name, op));
            }
        }
    }
    // Simple `=` (must not be preceded by `:`, `?`, `+`, `!` which we already handled).
    if let Some(pos) = line.find('=') {
        if pos > 0 {
            let before = line.as_bytes()[pos - 1];
            if before == b':' || before == b'?' || before == b'+' || before == b'!' {
                return None;
            }
        }
        let name = line[..pos].trim();
        if !name.is_empty() && !name.contains(' ') && !name.contains('\t') && !name.contains(':') {
            return Some((name, "="));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_and_variables() {
        let src = r#"CC = gcc
CFLAGS := -Wall -Werror

# Build the project
all: main.o utils.o
	$(CC) $(CFLAGS) -o app main.o utils.o

main.o: main.c
	$(CC) $(CFLAGS) -c main.c

clean:
	rm -f *.o app
"#;
        let entities = extract_makefile_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(
            names.contains(&"CC"),
            "missing variable CC, got: {:?}",
            names
        );
        assert!(names.contains(&"CFLAGS"), "missing variable CFLAGS");
        assert!(names.contains(&"all"), "missing target all");
        assert!(names.contains(&"main.o"), "missing target main.o");
        assert!(names.contains(&"clean"), "missing target clean");

        let cc = entities.iter().find(|e| e.name == "CC").unwrap();
        assert_eq!(cc.kind, EntityKind::Variable);

        let all = entities.iter().find(|e| e.name == "all").unwrap();
        assert_eq!(all.kind, EntityKind::Function);
        assert_eq!(all.doc_comment.as_deref(), Some("Build the project"));
    }

    #[test]
    fn phony_and_includes() {
        let src = r#".PHONY: all clean test
include config.mk
-include local.mk

all: build test

test:
	cargo test

build:
	cargo build
"#;
        let entities = extract_makefile_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(names.contains(&".PHONY"), "missing .PHONY");
        assert!(names.contains(&"config.mk"), "missing include config.mk");
        assert!(names.contains(&"local.mk"), "missing include local.mk");
        assert!(names.contains(&"all"), "missing target all");
        assert!(names.contains(&"test"), "missing target test");
        assert!(names.contains(&"build"), "missing target build");

        let config = entities.iter().find(|e| e.name == "config.mk").unwrap();
        assert_eq!(config.kind, EntityKind::Import);

        let local = entities.iter().find(|e| e.name == "local.mk").unwrap();
        assert_eq!(local.kind, EntityKind::Import);
    }

    #[test]
    fn conditional_and_complex_variables() {
        let src = r#"VERSION ?= 1.0.0
PREFIX := /usr/local
INSTALL_DIR = $(PREFIX)/bin

install: all
	install -m 755 app $(INSTALL_DIR)
"#;
        let entities = extract_makefile_entities(src);
        let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();

        assert!(
            names.contains(&"VERSION"),
            "missing VERSION, got: {:?}",
            names
        );
        assert!(names.contains(&"PREFIX"), "missing PREFIX");
        assert!(names.contains(&"INSTALL_DIR"), "missing INSTALL_DIR");
        assert!(names.contains(&"install"), "missing target install");

        let version = entities.iter().find(|e| e.name == "VERSION").unwrap();
        assert_eq!(version.kind, EntityKind::Variable);
        assert!(version.signature.as_ref().unwrap().contains("?="));
    }
}
