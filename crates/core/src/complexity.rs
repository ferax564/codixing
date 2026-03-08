//! Cyclomatic complexity (McCabe 1976) analysis for code.
//!
//! Provides both a fast text-based heuristic and risk band classification.
//! Used by the LSP server (diagnostics) and MCP tools (get_complexity).

/// Count cyclomatic complexity for a function spanning the given line range.
///
/// Uses a text-based heuristic: counts decision-point keywords (`if`, `for`,
/// `while`, `match`, `=>`, `&&`, `||`, `catch`, `case`, `loop`) on trimmed
/// lines. Starts at a base complexity of 1.
///
/// `start` and `end` are 1-based line numbers.
pub fn count_cyclomatic_complexity(lines: &[&str], start: usize, end: usize) -> usize {
    let mut cc = 1;
    for line in lines
        .iter()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start) + 1)
    {
        let t = line.trim();
        cc += t.matches("if ").count();
        cc += t.matches("else if").count();
        cc += t.matches("for ").count();
        cc += t.matches("while ").count();
        if t.contains("loop {") || t.trim() == "loop" {
            cc += 1;
        }
        cc += t.matches("match ").count();
        cc += t.matches("=>").count();
        cc += t.matches(" && ").count();
        cc += t.matches(" || ").count();
        cc += t.matches("catch").count();
        cc += t.matches("case ").count();
    }
    cc
}

/// Classify complexity into a risk band.
pub fn risk_band(cc: usize) -> &'static str {
    match cc {
        1..=5 => "low",
        6..=10 => "moderate",
        11..=25 => "high",
        _ => "critical",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_function_is_one() {
        let lines = vec!["fn foo() {", "    return 42;", "}"];
        assert_eq!(count_cyclomatic_complexity(&lines, 1, 3), 1);
    }

    #[test]
    fn counts_if_and_logical_ops() {
        let lines = vec![
            "fn check(x: i32) -> bool {",
            "    if x > 0 && x < 100 {",
            "        return true;",
            "    }",
            "    false",
            "}",
        ];
        assert_eq!(count_cyclomatic_complexity(&lines, 1, 6), 3);
    }

    #[test]
    fn counts_match_arms() {
        let lines = vec![
            "fn classify(x: i32) -> &str {",
            "    match x {",
            "        0 => \"zero\",",
            "        1..=9 => \"small\",",
            "        _ => \"large\",",
            "    }",
            "}",
        ];
        assert_eq!(count_cyclomatic_complexity(&lines, 1, 7), 5);
    }

    #[test]
    fn counts_loops_and_catch() {
        let lines = vec![
            "fn process() {",
            "    for item in list {",
            "        while running {",
            "            try { something() } catch { handle() }",
            "        }",
            "    }",
            "}",
        ];
        assert_eq!(count_cyclomatic_complexity(&lines, 1, 7), 4);
    }

    #[test]
    fn risk_band_categories() {
        assert_eq!(risk_band(1), "low");
        assert_eq!(risk_band(5), "low");
        assert_eq!(risk_band(6), "moderate");
        assert_eq!(risk_band(10), "moderate");
        assert_eq!(risk_band(11), "high");
        assert_eq!(risk_band(25), "high");
        assert_eq!(risk_band(26), "critical");
    }
}
