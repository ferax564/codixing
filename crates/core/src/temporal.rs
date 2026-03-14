//! Git-based temporal analysis: hotspots, change history, blame.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// A file ranked by change frequency and recency.
#[derive(Debug, Clone)]
pub struct Hotspot {
    /// Relative file path.
    pub file_path: String,
    /// Total number of commits that touched this file.
    pub commit_count: usize,
    /// Number of distinct authors.
    pub author_count: usize,
    /// Composite score combining frequency and recency.
    pub score: f32,
}

/// A single change event from git log.
#[derive(Debug, Clone)]
pub struct ChangeEntry {
    /// Commit hash (short).
    pub commit: String,
    /// Author name.
    pub author: String,
    /// Relative timestamp string (e.g. "2 days ago").
    pub date_relative: String,
    /// ISO date string.
    pub date_iso: String,
    /// Commit subject line.
    pub subject: String,
    /// Files touched by this commit.
    pub files: Vec<String>,
}

/// Blame information for a line range.
#[derive(Debug, Clone)]
pub struct BlameLine {
    /// Commit hash (short).
    pub commit: String,
    /// Author name.
    pub author: String,
    /// ISO date.
    pub date: String,
    /// Line number (1-indexed).
    pub line_number: u64,
    /// Line content.
    pub content: String,
}

/// Compute file hotspots by analyzing git log for change frequency.
///
/// Returns files sorted by a composite score that weights recent changes
/// more heavily than old ones.
pub fn get_hotspots(root: &Path, limit: usize, days: u64) -> Vec<Hotspot> {
    let since = format!("--since={days} days ago");
    let out = Command::new("git")
        .args(["log", "--format=%H %an", "--name-only", &since])
        .current_dir(root)
        .output();

    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&out.stdout);

    // Parse git log output: alternating "hash author" and file name lines.
    let mut file_commits: HashMap<String, Vec<String>> = HashMap::new();
    let mut file_authors: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut current_author = String::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Lines with a space and 40-char hex prefix are commit headers.
        if line.len() > 41 && line.chars().take(40).all(|c| c.is_ascii_hexdigit()) {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                current_author = parts[1].to_string();
            }
        } else if !current_author.is_empty() {
            // File path line.
            file_commits
                .entry(line.to_string())
                .or_default()
                .push(current_author.clone());
            file_authors
                .entry(line.to_string())
                .or_default()
                .insert(current_author.clone());
        }
    }

    // Get total commit count to normalize recency.
    let total_commits_out = Command::new("git")
        .args(["rev-list", "--count", "HEAD", &since])
        .current_dir(root)
        .output();
    let total_commits = total_commits_out
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(1.0)
        .max(1.0);

    let mut hotspots: Vec<Hotspot> = file_commits
        .into_iter()
        .map(|(file_path, commits)| {
            let commit_count = commits.len();
            let author_count = file_authors.get(&file_path).map(|a| a.len()).unwrap_or(1);
            // Score: frequency normalized + author diversity bonus.
            let freq_score = commit_count as f32 / total_commits;
            let author_bonus = (author_count as f32).ln().max(0.0) * 0.1;
            let score = freq_score + author_bonus;
            Hotspot {
                file_path,
                commit_count,
                author_count,
                score,
            }
        })
        .collect();

    hotspots.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hotspots.truncate(limit);
    hotspots
}

/// Search recent changes using git log.
///
/// If `file_filter` is provided, restricts to commits touching that file.
/// If `query` is provided, searches commit messages.
pub fn search_changes(
    root: &Path,
    query: Option<&str>,
    file_filter: Option<&str>,
    limit: usize,
) -> Vec<ChangeEntry> {
    let mut args = vec![
        "log".to_string(),
        format!("--max-count={limit}"),
        "--format=%h|%an|%ar|%aI|%s".to_string(),
        "--name-only".to_string(),
    ];

    if let Some(q) = query {
        args.push(format!("--grep={q}"));
        args.push("-i".to_string());
    }

    args.push("--".to_string());

    if let Some(f) = file_filter {
        args.push(f.to_string());
    }

    let out = Command::new("git").args(&args).current_dir(root).output();

    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let mut entries: Vec<ChangeEntry> = Vec::new();
    let mut current: Option<ChangeEntry> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() == 5 {
            // Push previous entry.
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            current = Some(ChangeEntry {
                commit: parts[0].to_string(),
                author: parts[1].to_string(),
                date_relative: parts[2].to_string(),
                date_iso: parts[3].to_string(),
                subject: parts[4].to_string(),
                files: Vec::new(),
            });
        } else if let Some(ref mut entry) = current {
            entry.files.push(line.to_string());
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }

    entries
}

/// Get blame information for a file, optionally restricted to a line range.
pub fn get_blame(
    root: &Path,
    file: &str,
    line_start: Option<u64>,
    line_end: Option<u64>,
) -> Vec<BlameLine> {
    let mut args = vec!["blame".to_string(), "--porcelain".to_string()];

    if let (Some(start), Some(end)) = (line_start, line_end) {
        args.push(format!("-L{start},{end}"));
    }

    args.push("--".to_string());
    args.push(file.to_string());

    let out = Command::new("git").args(&args).current_dir(root).output();

    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<BlameLine> = Vec::new();
    let mut current_commit = String::new();
    let mut current_author = String::new();
    let mut current_date = String::new();
    let mut current_line_no: u64 = 0;

    for line in text.lines() {
        if let Some(content) = line.strip_prefix('\t') {
            // Content line (prefixed with tab in porcelain format).
            lines.push(BlameLine {
                commit: current_commit.clone(),
                author: current_author.clone(),
                date: current_date.clone(),
                line_number: current_line_no,
                content: content.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix("author ") {
            current_author = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("author-time ") {
            // Convert unix timestamp to ISO date.
            if let Ok(ts) = rest.parse::<i64>() {
                let secs_per_day = 86400;
                let days_since_epoch = ts / secs_per_day;
                // Simple date computation (approximate, good enough for display).
                let (y, m, d) = days_to_ymd(days_since_epoch);
                current_date = format!("{y:04}-{m:02}-{d:02}");
            }
        } else {
            // First line of a blame entry: "<hash> <orig_line> <final_line> [<num_lines>]"
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3
                && parts[0].len() >= 8
                && parts[0].chars().all(|c| c.is_ascii_hexdigit())
            {
                current_commit = parts[0][..8].to_string();
                current_line_no = parts[2].parse().unwrap_or(0);
            }
        }
    }

    lines
}

/// Get the change frequency for a specific file over a time window.
pub fn file_change_frequency(root: &Path, file: &str, days: u64) -> (usize, Vec<String>) {
    let since = format!("--since={days} days ago");
    let out = Command::new("git")
        .args(["log", "--format=%an", &since, "--follow", "--", file])
        .current_dir(root)
        .output();

    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return (0, Vec::new()),
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let authors: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();
    let commit_count = authors.len();
    let unique_authors: Vec<String> = {
        let mut set = std::collections::BTreeSet::new();
        for a in &authors {
            set.insert(a.clone());
        }
        set.into_iter().collect()
    };
    (commit_count, unique_authors)
}

/// Simple days-since-epoch to (year, month, day) conversion.
fn days_to_ymd(days: i64) -> (i64, i64, i64) {
    // Algorithm from Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn setup_git_repo() -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        let root = dir.path();

        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .unwrap();

        // Configure git user for commits.
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(root)
            .output()
            .unwrap();

        // Create initial file and commit.
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(root)
            .output()
            .unwrap();

        // Second commit touching main.rs.
        fs::write(root.join("main.rs"), "fn main() { println!(\"hello\"); }\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "update main"])
            .current_dir(root)
            .output()
            .unwrap();

        // Third commit adding a new file.
        fs::write(root.join("lib.rs"), "pub fn helper() {}\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(root)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add helper"])
            .current_dir(root)
            .output()
            .unwrap();

        dir
    }

    #[test]
    fn test_get_hotspots() {
        let dir = setup_git_repo();
        let hotspots = get_hotspots(dir.path(), 10, 365);
        assert!(!hotspots.is_empty(), "expected at least one hotspot");
        // main.rs should be the top hotspot (2 commits vs 1 for lib.rs).
        assert_eq!(hotspots[0].file_path, "main.rs");
        assert!(hotspots[0].commit_count >= 2);
    }

    #[test]
    fn test_search_changes() {
        let dir = setup_git_repo();
        let entries = search_changes(dir.path(), Some("helper"), None, 10);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].subject.contains("helper"));
    }

    #[test]
    fn test_search_changes_file_filter() {
        let dir = setup_git_repo();
        let entries = search_changes(dir.path(), None, Some("main.rs"), 10);
        assert!(
            entries.len() >= 2,
            "expected at least 2 commits for main.rs"
        );
    }

    #[test]
    fn test_get_blame() {
        let dir = setup_git_repo();
        let blame = get_blame(dir.path(), "main.rs", None, None);
        assert!(!blame.is_empty(), "expected blame output");
        assert!(blame[0].content.contains("main"));
    }

    #[test]
    fn test_file_change_frequency() {
        let dir = setup_git_repo();
        let (count, authors) = file_change_frequency(dir.path(), "main.rs", 365);
        assert!(count >= 2, "expected at least 2 commits for main.rs");
        assert!(!authors.is_empty());
    }

    #[test]
    fn test_days_to_ymd() {
        // 2024-01-01 is day 19723 since epoch.
        let (y, m, d) = days_to_ymd(19723);
        assert_eq!(y, 2024);
        assert_eq!(m, 1);
        assert_eq!(d, 1);
    }
}
