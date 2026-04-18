//! Rubric assertion: every MCP tool description must contain an activation
//! trigger phrase so agents can tell WHEN to pick this tool over a neighbour.
//!
//! Background and full rubric: `crates/mcp/TOOL_DESCRIPTION_RUBRIC.md`.
//! The paper that motivated the audit: arXiv 2602.14878 — +5.85 pp task
//! success, +15.12 % evaluator score from disciplined tool descriptions.
//!
//! This test enforces the single check that's cheap to automate: trigger
//! phrase presence. The other three rubric checks (action-first purpose,
//! behavioural parameter semantics, limitations called out inline) are
//! human review only.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Phrases any of which is accepted as an activation signal.
const TRIGGER_PHRASES: &[&str] = &[
    "use when",
    "use this",
    "useful when",
    "useful for",
    "essential for",
    "ideal for",
    "unlike",
    "instead of",
    "prefer this",
    "tip:",
    "when you",
    "when the",
    "needed for",
    "returns ", // catches "Returns …" openers on strictly read-only tools
];

/// Path to the tool_defs directory relative to the mcp crate.
fn tool_defs_dir() -> &'static Path {
    Path::new("tool_defs")
}

/// Very small TOML section walker — we only care about `[[tools]]` blocks
/// and their `name = "..."` / `description = "..."` keys. Pulling in the
/// full `toml` crate for a test would bloat the dev-dep graph; a manual
/// scan is enough and avoids needing to mirror the production parser.
fn collect_tool_descriptions() -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let dir = tool_defs_dir();
    let entries = fs::read_dir(dir).expect("tool_defs directory must exist next to the tests");

    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("readable tool def");

        let mut current_name: Option<String> = None;
        let mut current_description: Option<String> = None;
        let mut in_tool = false;
        let mut seen_params = false;

        for raw_line in text.lines() {
            let trimmed = raw_line.trim();

            // New [[tools]] block — flush any previous.
            if trimmed == "[[tools]]" {
                if let (Some(n), Some(d)) = (current_name.take(), current_description.take()) {
                    out.insert(n, d);
                }
                in_tool = true;
                seen_params = false;
                continue;
            }

            // Entering a params subsection stops us from accidentally reading
            // the *parameter* description as the tool description.
            if in_tool && trimmed.starts_with("[tools.params.") {
                seen_params = true;
                continue;
            }

            // Any other non-tools section terminates the current block.
            if trimmed.starts_with('[') && !trimmed.starts_with("[[tools]]") && !seen_params {
                if let (Some(n), Some(d)) = (current_name.take(), current_description.take()) {
                    out.insert(n, d);
                }
                in_tool = false;
                continue;
            }

            if !in_tool || seen_params {
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("name = ") {
                current_name = Some(unquote(rest));
            } else if let Some(rest) = trimmed.strip_prefix("description = ") {
                current_description = Some(unquote(rest));
            }
        }

        if let (Some(n), Some(d)) = (current_name, current_description) {
            out.insert(n, d);
        }
    }

    out
}

/// Strip wrapping quotes + decode common `\u...` escapes from a TOML literal.
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let s = trimmed.trim_matches('"');
    // Decode the handful of unicode escapes the tool defs actually use.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'u') {
            chars.next();
            let hex: String = (0..4).filter_map(|_| chars.next()).collect();
            if let Ok(code) = u32::from_str_radix(&hex, 16)
                && let Some(decoded) = char::from_u32(code)
            {
                out.push(decoded);
                continue;
            }
            out.push('\\');
            out.push('u');
            out.push_str(&hex);
        } else if c == '\\' && chars.peek() == Some(&'n') {
            chars.next();
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

fn has_trigger(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    TRIGGER_PHRASES.iter().any(|p| lower.contains(p))
}

#[test]
fn every_tool_description_includes_trigger_phrase() {
    let descs = collect_tool_descriptions();
    assert!(
        !descs.is_empty(),
        "no tools parsed from tool_defs/ — parser regression?"
    );

    let mut missing: Vec<(String, String)> = Vec::new();
    for (name, desc) in &descs {
        if !has_trigger(desc) {
            missing.push((name.clone(), desc.clone()));
        }
    }

    if !missing.is_empty() {
        let mut msg = format!(
            "{} tool description(s) missing an activation trigger phrase.\n\
             See `crates/mcp/TOOL_DESCRIPTION_RUBRIC.md` §2 for the phrase list.\n\n",
            missing.len()
        );
        for (name, desc) in &missing {
            msg.push_str(&format!("  - {name}\n    {desc}\n\n"));
        }
        panic!("{msg}");
    }
}

#[test]
fn parser_sanity_check_all_67_tools_visible() {
    let descs = collect_tool_descriptions();
    assert!(
        descs.len() >= 65,
        "expected ~67 tools, parser found only {}; did the TOML layout change?",
        descs.len()
    );
}
