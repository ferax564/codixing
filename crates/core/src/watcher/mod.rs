use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

use crate::config::IndexConfig;
use crate::error::Result;
use crate::language::detect_language;

/// Default debounce window — events within this period are coalesced.
const DEBOUNCE_MS: u64 = 100;

/// A debounced file-system watcher that emits batches of changed paths.
///
/// Uses `notify::RecommendedWatcher` under the hood. Events are debounced
/// so that a burst of rapid saves produces a single batch callback.
pub struct FileWatcher {
    _watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<notify::Result<notify::Event>>,
    exclude_patterns: HashSet<String>,
}

/// The kind of change detected for a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// File was created or modified.
    Modified,
    /// File was deleted.
    Removed,
}

/// A single file-change event after debouncing.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Absolute path to the changed file.
    pub path: PathBuf,
    /// What happened.
    pub kind: ChangeKind,
}

impl FileWatcher {
    /// Start watching `root` recursively.
    ///
    /// Events are collected internally and can be drained with [`Self::poll_changes`].
    pub fn new(root: &Path, config: &IndexConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel();

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            Config::default(),
        )?;

        watcher.watch(root, RecursiveMode::Recursive)?;

        let exclude_patterns: HashSet<String> = config.exclude_patterns.iter().cloned().collect();

        info!(root = %root.display(), "file watcher started");

        Ok(Self {
            _watcher: watcher,
            receiver: rx,
            exclude_patterns,
        })
    }

    /// Drain pending events with debouncing.
    ///
    /// Blocks for up to `timeout` waiting for the first event, then collects
    /// all events that arrive within the debounce window. Returns a deduplicated
    /// batch of [`FileChange`]s.
    ///
    /// Returns an empty vec if no events arrive within `timeout`.
    pub fn poll_changes(&self, timeout: Duration) -> Vec<FileChange> {
        let mut raw_events = Vec::new();

        // Wait for the first event (or timeout).
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(event)) => raw_events.push(event),
            Ok(Err(e)) => {
                warn!(error = %e, "watcher error");
                return Vec::new();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => return Vec::new(),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Vec::new(),
        }

        // Debounce: collect all events within the debounce window.
        let debounce = Duration::from_millis(DEBOUNCE_MS);
        let deadline = Instant::now() + debounce;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(Ok(event)) => raw_events.push(event),
                Ok(Err(e)) => {
                    warn!(error = %e, "watcher error during debounce");
                }
                Err(_) => break,
            }
        }

        self.process_events(raw_events)
    }

    /// Convert raw notify events into deduplicated `FileChange`s.
    fn process_events(&self, events: Vec<notify::Event>) -> Vec<FileChange> {
        let mut changes = std::collections::HashMap::<PathBuf, ChangeKind>::new();

        for event in events {
            let kind = match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => ChangeKind::Modified,
                EventKind::Remove(_) => ChangeKind::Removed,
                _ => continue,
            };

            for path in event.paths {
                // Skip directories.
                if path.is_dir() {
                    continue;
                }

                // Skip excluded paths.
                if self.is_excluded(&path) {
                    debug!(path = %path.display(), "ignored excluded path");
                    continue;
                }

                // Skip unsupported file types.
                if kind == ChangeKind::Modified && detect_language(&path).is_none() {
                    continue;
                }

                // Later events for the same path override earlier ones.
                changes.insert(path, kind.clone());
            }
        }

        changes
            .into_iter()
            .map(|(path, kind)| FileChange { path, kind })
            .collect()
    }

    /// Check if a path should be excluded based on config patterns.
    fn is_excluded(&self, path: &Path) -> bool {
        for component in path.components() {
            if let std::path::Component::Normal(name) = component {
                if let Some(name_str) = name.to_str() {
                    if self.exclude_patterns.contains(name_str) {
                        return true;
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn watcher_detects_file_creation() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let watcher = FileWatcher::new(root, &config).unwrap();

        // Create a new Rust file.
        fs::write(root.join("new.rs"), "fn new_func() {}").unwrap();

        let changes = watcher.poll_changes(Duration::from_secs(2));
        assert!(
            !changes.is_empty(),
            "expected at least one change event after file creation"
        );
        assert!(changes.iter().any(|c| c.path.ends_with("new.rs")));
        assert!(changes.iter().any(|c| c.kind == ChangeKind::Modified));
    }

    #[test]
    fn watcher_detects_modification() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        // Create a file before watching starts.
        let file_path = root.join("existing.rs");
        fs::write(&file_path, "fn v1() {}").unwrap();

        let watcher = FileWatcher::new(root, &config).unwrap();

        // Modify the file.
        fs::write(&file_path, "fn v2() {} fn v2b() {}").unwrap();

        let changes = watcher.poll_changes(Duration::from_secs(2));
        assert!(!changes.is_empty());
        assert!(changes.iter().any(|c| c.path.ends_with("existing.rs")));
    }

    #[test]
    fn watcher_ignores_excluded_dirs() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let watcher = FileWatcher::new(root, &config).unwrap();

        // Create files in excluded directories.
        let git_dir = root.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("config.rs"), "fn git_internal() {}").unwrap();

        let target_dir = root.join("target");
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(target_dir.join("build.rs"), "fn build() {}").unwrap();

        // Also create a valid file to ensure watcher is working.
        fs::write(root.join("valid.rs"), "fn valid() {}").unwrap();

        let changes = watcher.poll_changes(Duration::from_secs(2));
        // Should only see valid.rs, not the excluded dirs.
        for change in &changes {
            assert!(
                !change.path.to_string_lossy().contains(".git"),
                "should not see .git files: {:?}",
                change.path
            );
            assert!(
                !change.path.to_string_lossy().contains("target"),
                "should not see target files: {:?}",
                change.path
            );
        }
    }

    #[test]
    fn watcher_ignores_unsupported_extensions() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let watcher = FileWatcher::new(root, &config).unwrap();

        // Create files with unsupported extensions.
        fs::write(root.join("data.txt"), "hello").unwrap();
        fs::write(root.join("image.png"), [0x89, 0x50, 0x4E, 0x47]).unwrap();

        // Also create a valid file.
        fs::write(root.join("code.rs"), "fn code() {}").unwrap();

        let changes = watcher.poll_changes(Duration::from_secs(2));
        for change in &changes {
            if change.kind == ChangeKind::Modified {
                assert!(
                    detect_language(&change.path).is_some(),
                    "should not see unsupported file: {:?}",
                    change.path
                );
            }
        }
    }
}
