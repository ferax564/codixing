use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::filter_pipeline::rule::Stage;

// ── ANSI escape sequence pattern ─────────────────────────────────────────────

static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]|\x1b\].*?\x07").unwrap());

// ── Public functions ──────────────────────────────────────────────────────────

/// Apply a single stage to `input` and return the transformed string.
pub fn apply_stage(stage: &Stage, input: &str) -> String {
    match stage {
        Stage::StripAnsi => strip_ansi(input),
        Stage::KeepLines { pattern } => keep_lines(pattern, input),
        Stage::StripLines { pattern } => strip_lines(pattern, input),
        Stage::Replace {
            pattern,
            replacement,
        } => replace(pattern, replacement, input),
        Stage::Head { lines } => head(*lines, input),
        Stage::Tail { lines } => tail(*lines, input),
        Stage::MaxLines { lines } => max_lines(*lines, input),
        Stage::DedupLines => dedup_lines(input),
        Stage::OnEmpty { message } => on_empty(message, input),
        Stage::Truncate { max_chars } => truncate(*max_chars, input),
    }
}

/// Apply all stages in order, threading the output of each into the next.
pub fn apply_stages(stages: &[Stage], input: &str) -> String {
    stages
        .iter()
        .fold(input.to_owned(), |acc, stage| apply_stage(stage, &acc))
}

// ── Stage implementations ─────────────────────────────────────────────────────

fn strip_ansi(input: &str) -> String {
    ANSI_RE.replace_all(input, "").into_owned()
}

fn keep_lines(pattern: &Regex, input: &str) -> String {
    input
        .lines()
        .filter(|line| pattern.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_lines(pattern: &Regex, input: &str) -> String {
    input
        .lines()
        .filter(|line| !pattern.is_match(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn replace(pattern: &Regex, replacement: &str, input: &str) -> String {
    input
        .lines()
        .map(|line| pattern.replace_all(line, replacement).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn head(n: usize, input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() <= n {
        return input.to_owned();
    }
    let kept = &lines[..n];
    let remaining = lines.len() - n;
    format!("{}\n... ({} more lines)", kept.join("\n"), remaining)
}

fn tail(n: usize, input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() <= n {
        return input.to_owned();
    }
    let skipped = lines.len() - n;
    let kept = &lines[skipped..];
    format!("({} lines skipped) ...\n{}", skipped, kept.join("\n"))
}

fn max_lines(n: usize, input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    if lines.len() <= n {
        return input.to_owned();
    }
    // head gets 2/3, tail gets 1/3
    let head_count = (n * 2).div_ceil(3);
    let tail_count = n - head_count;
    let skipped = lines.len() - head_count - tail_count;
    let head_part = lines[..head_count].join("\n");
    let tail_part = lines[lines.len() - tail_count..].join("\n");
    format!(
        "{}\n... ({} lines omitted) ...\n{}",
        head_part, skipped, tail_part
    )
}

fn dedup_lines(input: &str) -> String {
    let mut seen = HashSet::new();
    input
        .lines()
        .filter(|line| seen.insert(*line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn on_empty(message: &str, input: &str) -> String {
    if input.trim().is_empty() {
        message.to_owned()
    } else {
        input.to_owned()
    }
}

fn truncate(max_chars: usize, input: &str) -> String {
    input
        .lines()
        .map(|line| {
            if line.chars().count() <= max_chars {
                line.to_owned()
            } else {
                // Walk to the char boundary at position max_chars
                let byte_idx = line
                    .char_indices()
                    .nth(max_chars)
                    .map(|(i, _)| i)
                    .unwrap_or(line.len());
                line[..byte_idx].to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use regex::Regex;

    use super::*;
    use crate::filter_pipeline::rule::Stage;

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let input = "\x1b[31mhello\x1b[0m world";
        let result = apply_stage(&Stage::StripAnsi, input);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn strip_ansi_removes_osc_sequences() {
        let input = "before\x1b]0;title\x07after";
        let result = apply_stage(&Stage::StripAnsi, input);
        assert_eq!(result, "beforeafter");
    }

    #[test]
    fn keep_lines_filters_correctly() {
        let input = "foo bar\nbaz\nfoo qux";
        let stage = Stage::KeepLines {
            pattern: Regex::new("foo").unwrap(),
        };
        let result = apply_stage(&stage, input);
        assert_eq!(result, "foo bar\nfoo qux");
    }

    #[test]
    fn strip_lines_removes_matching() {
        let input = "keep this\nremove me\nkeep that";
        let stage = Stage::StripLines {
            pattern: Regex::new("remove").unwrap(),
        };
        let result = apply_stage(&stage, input);
        assert_eq!(result, "keep this\nkeep that");
    }

    #[test]
    fn replace_substitutes_per_line() {
        let input = "hello world\nhello rust";
        let stage = Stage::Replace {
            pattern: Regex::new("hello").unwrap(),
            replacement: "hi".to_owned(),
        };
        let result = apply_stage(&stage, input);
        assert_eq!(result, "hi world\nhi rust");
    }

    #[test]
    fn head_keeps_first_n_lines_with_elision() {
        let input = "a\nb\nc\nd\ne";
        let stage = Stage::Head { lines: 3 };
        let result = apply_stage(&stage, input);
        assert!(result.starts_with("a\nb\nc"));
        assert!(result.contains("2 more lines"));
    }

    #[test]
    fn head_no_elision_when_within_limit() {
        let input = "a\nb";
        let stage = Stage::Head { lines: 5 };
        let result = apply_stage(&stage, input);
        assert_eq!(result, "a\nb");
    }

    #[test]
    fn tail_keeps_last_n_lines_with_prefix() {
        let input = "a\nb\nc\nd\ne";
        let stage = Stage::Tail { lines: 2 };
        let result = apply_stage(&stage, input);
        assert!(result.contains("3 lines skipped"));
        assert!(result.ends_with("d\ne"));
    }

    #[test]
    fn tail_no_prefix_when_within_limit() {
        let input = "a\nb";
        let stage = Stage::Tail { lines: 5 };
        let result = apply_stage(&stage, input);
        assert_eq!(result, "a\nb");
    }

    #[test]
    fn max_lines_elides_middle() {
        // 10 lines, max 6 → head=4, tail=2, skip=4
        let input = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let stage = Stage::MaxLines { lines: 6 };
        let result = apply_stage(&stage, input.as_str());
        assert!(result.contains("line1"));
        assert!(result.contains("line10"));
        assert!(result.contains("omitted"));
    }

    #[test]
    fn max_lines_no_elision_when_within_limit() {
        let input = "a\nb\nc";
        let stage = Stage::MaxLines { lines: 10 };
        let result = apply_stage(&stage, input);
        assert_eq!(result, "a\nb\nc");
    }

    #[test]
    fn dedup_lines_preserves_order() {
        let input = "b\na\nb\nc\na";
        let stage = Stage::DedupLines;
        let result = apply_stage(&stage, input);
        assert_eq!(result, "b\na\nc");
    }

    #[test]
    fn on_empty_replaces_blank_input() {
        let stage = Stage::OnEmpty {
            message: "nothing here".to_owned(),
        };
        assert_eq!(apply_stage(&stage, ""), "nothing here");
        assert_eq!(apply_stage(&stage, "   \n  "), "nothing here");
    }

    #[test]
    fn on_empty_passes_through_non_empty() {
        let stage = Stage::OnEmpty {
            message: "nothing here".to_owned(),
        };
        let result = apply_stage(&stage, "some output");
        assert_eq!(result, "some output");
    }

    #[test]
    fn truncate_caps_long_lines() {
        let stage = Stage::Truncate { max_chars: 5 };
        let result = apply_stage(&stage, "abcdefgh\nxy");
        assert_eq!(result, "abcde\nxy");
    }

    #[test]
    fn truncate_char_boundary_safe_with_unicode() {
        let stage = Stage::Truncate { max_chars: 3 };
        // "café" is 4 chars; truncate at 3 should give "caf"
        let result = apply_stage(&stage, "café");
        assert_eq!(result, "caf");
    }

    #[test]
    fn apply_stages_chains_correctly() {
        let input = "\x1b[31mfoo\x1b[0m\nbar\nfoo again\nbar";
        let stages = vec![
            Stage::StripAnsi,
            Stage::DedupLines,
            Stage::KeepLines {
                pattern: Regex::new("foo").unwrap(),
            },
        ];
        let result = apply_stages(&stages, input);
        assert_eq!(result, "foo\nfoo again");
    }
}
