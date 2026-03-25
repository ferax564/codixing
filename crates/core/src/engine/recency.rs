//! Git recency map: maps file paths to their last-commit timestamp.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Build a recency map from git log, bounded to the last `days` days.
///
/// Each entry maps a relative file path to the Unix timestamp of its most
/// recent commit within the window. Files not modified in the last `days`
/// days are absent from the map.
pub fn build_recency_map(root: &Path, days: u64) -> HashMap<String, i64> {
    let since = format!("--since={days} days ago");
    let output = Command::new("git")
        .args([
            "log",
            "--format=%ct",
            "--name-only",
            "--diff-filter=ACMR",
            &since,
            "HEAD",
        ])
        .current_dir(root)
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return HashMap::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut map: HashMap<String, i64> = HashMap::new();
    let mut current_ts: Option<i64> = None;
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(ts) = trimmed.parse::<i64>() {
            current_ts = Some(ts);
        } else if let Some(ts) = current_ts {
            map.entry(trimmed.to_string())
                .and_modify(|e| {
                    if ts > *e {
                        *e = ts;
                    }
                })
                .or_insert(ts);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_recency_map_returns_nonempty_for_git_repo() {
        // The workspace root is a git repo with recent commits.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let map = build_recency_map(root, 365);
        assert!(
            !map.is_empty(),
            "expected non-empty recency map for the git repo"
        );
    }

    #[test]
    fn build_recency_map_returns_empty_for_nonexistent_dir() {
        let map = build_recency_map(Path::new("/tmp/nonexistent_codixing_dir_12345"), 180);
        assert!(
            map.is_empty(),
            "expected empty map for non-existent directory"
        );
    }

    #[test]
    fn build_recency_map_timestamps_are_positive() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let map = build_recency_map(root, 365);
        for (path, ts) in &map {
            assert!(*ts > 0, "expected positive timestamp for {path}, got {ts}");
        }
    }
}
