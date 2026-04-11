use regex::Regex;
use serde::Deserialize;

use crate::error::{CodixingError, Result};

// ── Public types ─────────────────────────────────────────────────────────────

/// A compiled filter rule, ready to match and apply.
#[derive(Debug)]
pub struct FilterRule {
    pub name: String,
    pub match_tool: String,
    pub match_output: Option<Regex>,
    pub match_min_lines: Option<usize>,
    pub disabled: bool,
    pub stages: Vec<Stage>,
}

/// A single transformation stage applied to a tool's output.
#[derive(Debug)]
pub enum Stage {
    StripAnsi,
    KeepLines { pattern: Regex },
    StripLines { pattern: Regex },
    Replace { pattern: Regex, replacement: String },
    Head { lines: usize },
    Tail { lines: usize },
    MaxLines { lines: usize },
    DedupLines,
    OnEmpty { message: String },
    Truncate { max_chars: usize },
}

/// The result of applying a filter pipeline to tool output.
#[derive(Debug, Clone)]
pub struct FilterResult {
    pub output: String,
    pub tee_path: Option<String>,
    pub was_filtered: bool,
    pub rule_name: Option<String>,
}

// ── TOML serde types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FilterConfig {
    schema_version: u32,
    #[serde(default)]
    rules: Vec<RawFilterRule>,
}

#[derive(Deserialize)]
struct RawFilterRule {
    name: String,
    match_tool: String,
    #[serde(default)]
    match_output: Option<String>,
    #[serde(default)]
    match_min_lines: Option<usize>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    stages: Vec<RawStage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawStage {
    StripAnsi,
    KeepLines {
        pattern: String,
    },
    StripLines {
        pattern: String,
    },
    Replace {
        pattern: String,
        replacement: String,
    },
    Head {
        lines: usize,
    },
    Tail {
        lines: usize,
    },
    MaxLines {
        lines: usize,
    },
    DedupLines,
    OnEmpty {
        message: String,
    },
    Truncate {
        max_chars: usize,
    },
}

// ── Public functions ──────────────────────────────────────────────────────────

/// Parse TOML configuration and compile all regex patterns into `FilterRule`s.
pub fn parse_filter_rules(toml_str: &str) -> Result<Vec<FilterRule>> {
    let config: FilterConfig = toml::from_str(toml_str)
        .map_err(|e| CodixingError::Config(format!("failed to parse filter config: {e}")))?;

    if config.schema_version != 1 {
        return Err(CodixingError::Config(format!(
            "unsupported filter config schema_version {}; expected 1",
            config.schema_version
        )));
    }

    config.rules.into_iter().map(compile_rule).collect()
}

// ── impl FilterRule ───────────────────────────────────────────────────────────

impl FilterRule {
    /// Returns `true` when this rule applies to the given tool output.
    ///
    /// Checks (in order, short-circuiting):
    /// 0. `disabled` — disabled rules never match.
    /// 1. `match_tool` — exact match or `"*"` wildcard.
    /// 2. `match_output` — regex must match anywhere in `output` (if set).
    /// 3. `match_min_lines` — `output` must have at least this many lines (if set).
    pub fn matches(&self, tool_name: &str, output: &str) -> bool {
        if self.disabled {
            return false;
        }
        if self.match_tool != "*" && self.match_tool != tool_name {
            return false;
        }
        if let Some(re) = &self.match_output {
            if !re.is_match(output) {
                return false;
            }
        }
        if let Some(min) = self.match_min_lines {
            if output.lines().take(min).count() < min {
                return false;
            }
        }
        true
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn compile_rule(raw: RawFilterRule) -> Result<FilterRule> {
    let match_output = raw
        .match_output
        .map(|pat| {
            Regex::new(&pat).map_err(|e| {
                CodixingError::Config(format!(
                    "rule {:?}: invalid match_output regex {:?}: {e}",
                    raw.name, pat
                ))
            })
        })
        .transpose()?;

    let stages = raw
        .stages
        .into_iter()
        .map(compile_stage)
        .collect::<Result<Vec<_>>>()?;

    Ok(FilterRule {
        name: raw.name,
        match_tool: raw.match_tool,
        match_output,
        match_min_lines: raw.match_min_lines,
        disabled: raw.disabled,
        stages,
    })
}

fn compile_stage(raw: RawStage) -> Result<Stage> {
    match raw {
        RawStage::StripAnsi => Ok(Stage::StripAnsi),
        RawStage::KeepLines { pattern } => {
            let re = Regex::new(&pattern).map_err(|e| {
                CodixingError::Config(format!("keep_lines: invalid regex {:?}: {e}", pattern))
            })?;
            Ok(Stage::KeepLines { pattern: re })
        }
        RawStage::StripLines { pattern } => {
            let re = Regex::new(&pattern).map_err(|e| {
                CodixingError::Config(format!("strip_lines: invalid regex {:?}: {e}", pattern))
            })?;
            Ok(Stage::StripLines { pattern: re })
        }
        RawStage::Replace {
            pattern,
            replacement,
        } => {
            let re = Regex::new(&pattern).map_err(|e| {
                CodixingError::Config(format!("replace: invalid regex {:?}: {e}", pattern))
            })?;
            Ok(Stage::Replace {
                pattern: re,
                replacement,
            })
        }
        RawStage::Head { lines } => Ok(Stage::Head { lines }),
        RawStage::Tail { lines } => Ok(Stage::Tail { lines }),
        RawStage::MaxLines { lines } => Ok(Stage::MaxLines { lines }),
        RawStage::DedupLines => Ok(Stage::DedupLines),
        RawStage::OnEmpty { message } => Ok(Stage::OnEmpty { message }),
        RawStage::Truncate { max_chars } => Ok(Stage::Truncate { max_chars }),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC_TOML: &str = r#"
schema_version = 1

[[rules]]
name = "trim-search"
match_tool = "search"
match_output = "results"
match_min_lines = 2

[[rules.stages]]
type = "strip_ansi"

[[rules.stages]]
type = "head"
lines = 20

[[rules.stages]]
type = "strip_lines"
pattern = "^\\s*$"

[[rules.stages]]
type = "truncate"
max_chars = 120
"#;

    #[test]
    fn parse_basic_filter_rule() {
        let rules = parse_filter_rules(BASIC_TOML).expect("should parse");
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        assert_eq!(r.name, "trim-search");
        assert_eq!(r.match_tool, "search");
        assert!(r.match_output.is_some());
        assert_eq!(r.match_min_lines, Some(2));
        assert!(!r.disabled);
        assert_eq!(r.stages.len(), 4);
    }

    #[test]
    fn parse_disabled_rule() {
        let toml = r#"
schema_version = 1
[[rules]]
name = "off"
match_tool = "*"
disabled = true
"#;
        let rules = parse_filter_rules(toml).expect("should parse");
        assert_eq!(rules.len(), 1);
        assert!(rules[0].disabled);
    }

    #[test]
    fn parse_wrong_schema_version() {
        let toml = r#"
schema_version = 99
"#;
        let err = parse_filter_rules(toml).expect_err("should error on unknown schema version");
        assert!(err.to_string().contains("schema_version") || err.to_string().contains("99"));
    }

    #[test]
    fn parse_invalid_regex_errors() {
        let toml = r#"
schema_version = 1
[[rules]]
name = "bad"
match_tool = "*"
match_output = "["
"#;
        let err = parse_filter_rules(toml).expect_err("should error on invalid regex");
        assert!(err.to_string().contains("regex") || err.to_string().contains("invalid"));
    }

    #[test]
    fn parse_unknown_stage_type_errors() {
        let toml = r#"
schema_version = 1
[[rules]]
name = "bad-stage"
match_tool = "*"

[[rules.stages]]
type = "does_not_exist"
"#;
        let err = parse_filter_rules(toml).expect_err("should error on unknown stage type");
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn rule_matches_tool_name() {
        let rules = parse_filter_rules(BASIC_TOML).unwrap();
        let r = &rules[0];
        let output = "results\nline2\nline3";
        assert!(r.matches("search", output));
        assert!(!r.matches("other_tool", output));
    }

    #[test]
    fn rule_matches_wildcard() {
        let toml = r#"
schema_version = 1
[[rules]]
name = "catch-all"
match_tool = "*"
"#;
        let rules = parse_filter_rules(toml).unwrap();
        let r = &rules[0];
        assert!(r.matches("anything", "any output"));
        assert!(r.matches("other", ""));
    }

    #[test]
    fn rule_matches_output_pattern() {
        let rules = parse_filter_rules(BASIC_TOML).unwrap();
        let r = &rules[0];
        // requires "results" in output AND at least 2 lines
        assert!(r.matches("search", "results\nline2"));
        assert!(!r.matches("search", "no match here\nline2"));
    }

    #[test]
    fn rule_matches_min_lines() {
        let rules = parse_filter_rules(BASIC_TOML).unwrap();
        let r = &rules[0];
        // needs at least 2 lines; single line should fail
        assert!(!r.matches("search", "results"));
        assert!(r.matches("search", "results\nsecond line"));
    }

    #[test]
    fn parse_built_in_defaults() {
        let defaults = include_str!("defaults.toml");
        let rules = parse_filter_rules(defaults).unwrap();
        assert_eq!(rules.len(), 5);
        let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"cargo-test-failures"));
        assert!(names.contains(&"pytest-failures"));
        assert!(names.contains(&"test-output-generic"));
        assert!(names.contains(&"git-diff-large"));
        assert!(names.contains(&"grep-high-volume"));
    }

    #[test]
    fn parse_all_stage_types() {
        let toml = r#"
schema_version = 1
[[rules]]
name = "all-stages"
match_tool = "*"

[[rules.stages]]
type = "strip_ansi"

[[rules.stages]]
type = "keep_lines"
pattern = "foo"

[[rules.stages]]
type = "strip_lines"
pattern = "bar"

[[rules.stages]]
type = "replace"
pattern = "old"
replacement = "new"

[[rules.stages]]
type = "head"
lines = 10

[[rules.stages]]
type = "tail"
lines = 5

[[rules.stages]]
type = "max_lines"
lines = 30

[[rules.stages]]
type = "dedup_lines"

[[rules.stages]]
type = "on_empty"
message = "no output"

[[rules.stages]]
type = "truncate"
max_chars = 80
"#;
        let rules = parse_filter_rules(toml).expect("all stage types should parse");
        assert_eq!(rules[0].stages.len(), 10);
        assert!(matches!(rules[0].stages[0], Stage::StripAnsi));
        assert!(matches!(rules[0].stages[1], Stage::KeepLines { .. }));
        assert!(matches!(rules[0].stages[2], Stage::StripLines { .. }));
        assert!(matches!(rules[0].stages[3], Stage::Replace { .. }));
        assert!(matches!(rules[0].stages[4], Stage::Head { lines: 10 }));
        assert!(matches!(rules[0].stages[5], Stage::Tail { lines: 5 }));
        assert!(matches!(rules[0].stages[6], Stage::MaxLines { lines: 30 }));
        assert!(matches!(rules[0].stages[7], Stage::DedupLines));
        assert!(matches!(rules[0].stages[8], Stage::OnEmpty { .. }));
        assert!(matches!(
            rules[0].stages[9],
            Stage::Truncate { max_chars: 80 }
        ));
    }
}
