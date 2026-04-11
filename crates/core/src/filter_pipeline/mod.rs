//! TOML-based output filter pipeline with tee recovery.

pub mod rule;
pub mod stages;
pub mod tee;

pub use rule::{FilterResult, FilterRule, Stage, parse_filter_rules};
pub use stages::{apply_stage, apply_stages};
pub use tee::{cleanup_tee, clear_tee, write_tee};

use std::path::{Path, PathBuf};
use tracing::{debug, warn};

pub struct FilterPipeline {
    rules: Vec<FilterRule>,
    tee_dir: PathBuf,
}

impl FilterPipeline {
    /// Load built-in defaults + overlay .codixing/filters.toml if present.
    /// Repo-local rules with same name as built-in replace it.
    pub fn load(codixing_dir: &Path) -> Self {
        let tee_dir = codixing_dir.join("tee");
        let mut rules = match parse_filter_rules(include_str!("defaults.toml")) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to parse built-in filter defaults");
                Vec::new()
            }
        };
        let local_path = codixing_dir.join("filters.toml");
        if local_path.is_file() {
            match std::fs::read_to_string(&local_path) {
                Ok(content) => match parse_filter_rules(&content) {
                    Ok(local_rules) => {
                        for local in local_rules {
                            if let Some(pos) = rules.iter().position(|r| r.name == local.name) {
                                rules[pos] = local;
                            } else {
                                rules.push(local);
                            }
                        }
                        debug!(path = %local_path.display(), "loaded repo-local filter rules");
                    }
                    Err(e) => warn!(error = %e, "failed to parse filters.toml"),
                },
                Err(e) => warn!(error = %e, "failed to read filters.toml"),
            }
        }
        Self { rules, tee_dir }
    }

    pub fn from_toml(toml_str: &str, tee_dir: PathBuf) -> crate::error::Result<Self> {
        let rules = parse_filter_rules(toml_str)?;
        Ok(Self { rules, tee_dir })
    }

    pub fn noop(tee_dir: PathBuf) -> Self {
        Self {
            rules: Vec::new(),
            tee_dir,
        }
    }

    /// Apply first matching rule. Write tee if output is reduced.
    pub fn apply(&self, output: &str, tool_name: &str) -> FilterResult {
        let matched = self
            .rules
            .iter()
            .find(|r| !r.disabled && r.matches(tool_name, output));
        let rule = match matched {
            Some(r) => r,
            None => {
                return FilterResult {
                    output: output.to_string(),
                    tee_path: None,
                    was_filtered: false,
                    rule_name: None,
                };
            }
        };

        let original_lines = output.lines().count();
        let filtered = apply_stages(&rule.stages, output);
        let filtered_lines = filtered.lines().count();

        let tee_path = if filtered_lines < original_lines {
            write_tee(&self.tee_dir, tool_name, output)
        } else {
            None
        };

        let final_output = match &tee_path {
            Some(path) => format!("{filtered}\n<!-- full output: {path} -->"),
            None => filtered,
        };

        FilterResult {
            output: final_output,
            tee_path,
            was_filtered: true,
            rule_name: Some(rule.name.clone()),
        }
    }

    /// Write tee for output truncated by non-pipeline code.
    pub fn tee_if_truncated(&self, full_output: &str, tool_name: &str) -> String {
        match write_tee(&self.tee_dir, tool_name, full_output) {
            Some(path) => format!("\n<!-- full output: {path} -->"),
            None => String::new(),
        }
    }

    pub fn cleanup(&self) {
        cleanup_tee(&self.tee_dir);
    }

    pub fn clear(&self) {
        clear_tee(&self.tee_dir);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn make_tee_dir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    const RUN_TESTS_RULE: &str = r#"
schema_version = 1
[[rules]]
name = "test-head"
match_tool = "run_tests"
[[rules.stages]]
type = "head"
lines = 3
"#;

    #[test]
    fn no_matching_rule_passes_through() {
        let dir = make_tee_dir();
        let pipeline = FilterPipeline::from_toml(RUN_TESTS_RULE, dir.path().to_path_buf()).unwrap();

        let output = "line1\nline2\nline3";
        let result = pipeline.apply(output, "git_diff");

        assert_eq!(result.output, output);
        assert!(!result.was_filtered);
        assert!(result.rule_name.is_none());
        assert!(result.tee_path.is_none());
    }

    #[test]
    fn matching_rule_applies_stages() {
        let dir = make_tee_dir();
        let pipeline = FilterPipeline::from_toml(RUN_TESTS_RULE, dir.path().to_path_buf()).unwrap();

        // 10 lines → head 3 should truncate, tee written, hint appended
        let output = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = pipeline.apply(&output, "run_tests");

        assert!(result.was_filtered);
        assert_eq!(result.rule_name.as_deref(), Some("test-head"));
        assert!(result.tee_path.is_some());
        assert!(result.output.contains("<!-- full output:"));
        assert!(result.output.contains("line1"));
        // Head keeps first 3 lines and elides rest
        assert!(result.output.contains("line3"));
    }

    #[test]
    fn first_match_wins() {
        let dir = make_tee_dir();
        let toml = r#"
schema_version = 1
[[rules]]
name = "first"
match_tool = "run_tests"
[[rules.stages]]
type = "head"
lines = 2

[[rules]]
name = "second"
match_tool = "run_tests"
[[rules.stages]]
type = "head"
lines = 5
"#;
        let pipeline = FilterPipeline::from_toml(toml, dir.path().to_path_buf()).unwrap();

        let output = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = pipeline.apply(&output, "run_tests");

        assert_eq!(result.rule_name.as_deref(), Some("first"));
    }

    #[test]
    fn disabled_rule_skipped() {
        let dir = make_tee_dir();
        let toml = r#"
schema_version = 1
[[rules]]
name = "disabled-rule"
match_tool = "run_tests"
disabled = true
[[rules.stages]]
type = "head"
lines = 1

[[rules]]
name = "fallback"
match_tool = "run_tests"
[[rules.stages]]
type = "head"
lines = 5
"#;
        let pipeline = FilterPipeline::from_toml(toml, dir.path().to_path_buf()).unwrap();

        let output = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = pipeline.apply(&output, "run_tests");

        assert_eq!(result.rule_name.as_deref(), Some("fallback"));
    }

    #[test]
    fn no_tee_when_output_not_reduced() {
        let dir = make_tee_dir();
        // strip_ansi on plain text (no ANSI codes) → same line count → no tee
        let toml = r#"
schema_version = 1
[[rules]]
name = "ansi-only"
match_tool = "build"
[[rules.stages]]
type = "strip_ansi"
"#;
        let pipeline = FilterPipeline::from_toml(toml, dir.path().to_path_buf()).unwrap();

        let output = "plain text\nno ansi here\nline3";
        let result = pipeline.apply(output, "build");

        assert!(result.was_filtered);
        assert!(result.tee_path.is_none());
        assert!(!result.output.contains("<!-- full output:"));
    }

    #[test]
    fn noop_pipeline_passes_through() {
        let dir = make_tee_dir();
        let pipeline = FilterPipeline::noop(dir.path().to_path_buf());

        let output = "some output\nline2\nline3";
        let result = pipeline.apply(output, "any_tool");

        assert_eq!(result.output, output);
        assert!(!result.was_filtered);
        assert!(result.tee_path.is_none());
        assert!(result.rule_name.is_none());
    }

    #[test]
    fn tee_if_truncated_writes_file() {
        let dir = make_tee_dir();
        let pipeline = FilterPipeline::noop(dir.path().to_path_buf());

        let output = "full output content";
        let hint = pipeline.tee_if_truncated(output, "some_tool");

        assert!(hint.contains("<!-- full output:"));
        assert!(hint.contains(".codixing/tee/"));
    }
}
