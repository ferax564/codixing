use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Config, EventKind, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

use crate::config::IndexConfig;
use crate::error::Result;
use crate::language::detect_language;

type BackendWatcher = notify::RecommendedWatcher;

/// Default debounce window — events within this period are coalesced.
///
/// 500ms strikes a good balance: fast enough to feel responsive, but long
/// enough to coalesce rapid editor auto-saves and multi-file operations
/// (e.g. `git checkout`, formatter runs) into a single batch, avoiding
/// redundant reindex cycles.
const DEBOUNCE_MS: u64 = 500;
/// Give backend watchers a brief moment to attach before callers start
/// mutating the filesystem. This avoids dropping the first write on macOS.
const STARTUP_SETTLE_MS: u64 = 150;

/// A debounced file-system watcher that emits batches of changed paths.
///
/// Uses `notify` under the hood. Events are debounced
/// so that a burst of rapid saves produces a single batch callback.
pub struct FileWatcher {
    _watcher: BackendWatcher,
    receiver: mpsc::Receiver<notify::Result<notify::Event>>,
    root: PathBuf,
    exclude_patterns: HashSet<String>,
    languages: HashSet<String>,
}

/// The kind of change detected for a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// File was created or modified.
    Modified,
    /// File was deleted.
    Removed,
    /// A directory was deleted or moved; Engine expands its indexed prefix.
    RemovedDirectory,
    /// A directory was created or moved; Engine expands it off the event loop.
    CreatedDirectory,
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

        let mut watcher = BackendWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            Config::default(),
        )?;

        watcher.watch(root, RecursiveMode::Recursive)?;
        std::thread::sleep(Duration::from_millis(STARTUP_SETTLE_MS));

        let exclude_patterns: HashSet<String> = config.exclude_patterns.iter().cloned().collect();

        info!(root = %root.display(), "file watcher started");

        Ok(Self {
            _watcher: watcher,
            receiver: rx,
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            exclude_patterns,
            languages: config.languages.clone(),
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
        use notify::event::{ModifyKind, RenameMode};

        let mut changes = HashMap::<PathBuf, ChangeKind>::new();
        for event in events {
            match &event.kind {
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) if event.paths.len() == 1 => {
                    let path = &event.paths[0];
                    self.record_change(&mut changes, path, ChangeKind::RemovedDirectory);
                    continue;
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) if event.paths.len() == 1 => {
                    self.record_created_path(&mut changes, &event.paths[0]);
                    continue;
                }
                EventKind::Modify(ModifyKind::Name(_)) if event.paths.len() >= 2 => {
                    let old_path = &event.paths[0];
                    let new_path = &event.paths[event.paths.len() - 1];
                    self.record_rename(&mut changes, old_path, new_path);
                    continue;
                }
                _ => {}
            }

            let rename_event = matches!(&event.kind, EventKind::Modify(ModifyKind::Name(_)));
            let create_event = matches!(&event.kind, EventKind::Create(_));
            let default_kind = match &event.kind {
                EventKind::Create(_) | EventKind::Modify(_) => ChangeKind::Modified,
                EventKind::Remove(notify::event::RemoveKind::File) => ChangeKind::Removed,
                EventKind::Remove(_) => ChangeKind::RemovedDirectory,
                _ => continue,
            };

            for path in event.paths {
                if (rename_event || create_event) && path.is_dir() {
                    self.record_created_path(&mut changes, &path);
                    continue;
                }
                let kind = if rename_event {
                    // Backends may split a rename into one-path From/To events.
                    // The current filesystem state disambiguates them safely.
                    if path.exists() {
                        ChangeKind::Modified
                    } else {
                        ChangeKind::RemovedDirectory
                    }
                } else {
                    default_kind.clone()
                };
                self.record_change(&mut changes, &path, kind);
            }
        }

        changes
            .into_iter()
            .map(|(path, kind)| FileChange { path, kind })
            .collect()
    }

    fn record_rename(
        &self,
        changes: &mut HashMap<PathBuf, ChangeKind>,
        old_path: &Path,
        new_path: &Path,
    ) {
        // The source may already be gone, so its file type is unknowable. A
        // conservative directory hint is safe: Engine turns an exact indexed
        // file key into an O(1) removal and prefix-expands only otherwise.
        self.record_change(changes, old_path, ChangeKind::RemovedDirectory);
        if new_path.is_dir() {
            // Removal and destination enumeration are deliberately independent.
            // Split/interleaved rename events cannot pair the wrong trees, and
            // a destination read failure still leaves the old prefix removable.
            self.record_directory_create(changes, new_path);
        } else {
            self.record_change(changes, new_path, ChangeKind::Modified);
        }
    }

    fn record_created_path(&self, changes: &mut HashMap<PathBuf, ChangeKind>, path: &Path) {
        if path.is_dir() {
            self.record_directory_create(changes, path);
        } else {
            self.record_change(changes, path, ChangeKind::Modified);
        }
    }

    /// Record a directory intent without walking it on notify's event-drain path.
    /// Engine expands it with the same ignore, language, size, and path-safety
    /// rules as a full index build.
    fn record_directory_create(&self, changes: &mut HashMap<PathBuf, ChangeKind>, new_root: &Path) {
        // FSEvents can attach a rename flag to the watched root as aggregate
        // metadata. Expanding that would turn one moved directory into a full
        // repository rescan.
        if new_root == self.root {
            return;
        }
        self.record_change(changes, new_root, ChangeKind::CreatedDirectory);
    }

    fn record_change(
        &self,
        changes: &mut HashMap<PathBuf, ChangeKind>,
        path: &Path,
        kind: ChangeKind,
    ) {
        if kind == ChangeKind::Modified && path.is_dir() {
            return;
        }
        if self.is_excluded(path) {
            debug!(path = %path.display(), "ignored excluded path");
            return;
        }
        if kind == ChangeKind::Modified {
            let Some(language) = detect_language(path) else {
                return;
            };
            if !self.languages.is_empty()
                && !self.languages.contains(&language.name().to_lowercase())
            {
                return;
            }
        }

        // Later events for the same path override earlier ones.
        changes.insert(path.to_path_buf(), kind);
    }

    /// Check if a path should be excluded based on config patterns.
    fn is_excluded(&self, path: &Path) -> bool {
        for component in path.components() {
            if let std::path::Component::Normal(name) = component
                && let Some(name_str) = name.to_str()
                && self.exclude_patterns.contains(name_str)
            {
                return true;
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

    #[test]
    fn paired_rename_removes_old_path_and_indexes_new_path() {
        use notify::event::{ModifyKind, RenameMode};

        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);
        let watcher = FileWatcher::new(root, &config).unwrap();
        let old_path = root.join("old.rs");
        let new_path = root.join("new.rs");
        fs::write(&new_path, "fn renamed() {}").unwrap();

        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(old_path.clone())
            .add_path(new_path.clone());
        let changes = watcher.process_events(vec![event]);

        assert!(changes.iter().any(|change| {
            change.path == old_path && change.kind == ChangeKind::RemovedDirectory
        }));
        assert!(
            changes
                .iter()
                .any(|change| { change.path == new_path && change.kind == ChangeKind::Modified })
        );
    }

    #[test]
    fn split_rename_removes_old_path_and_indexes_new_path() {
        use notify::event::{ModifyKind, RenameMode};

        let dir = tempdir().unwrap();
        let root = dir.path();
        let watcher = FileWatcher::new(root, &IndexConfig::new(root)).unwrap();
        let old_path = root.join("old.rs");
        let new_path = root.join("new.rs");
        fs::write(&new_path, "fn renamed() {}").unwrap();

        let from = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path(old_path.clone());
        let to = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
            .add_path(new_path.clone());
        let changes = watcher.process_events(vec![from, to]);

        assert!(changes.iter().any(|change| {
            change.path == old_path && change.kind == ChangeKind::RemovedDirectory
        }));
        assert!(
            changes
                .iter()
                .any(|change| { change.path == new_path && change.kind == ChangeKind::Modified })
        );
    }

    #[test]
    fn paired_directory_rename_emits_constant_size_directory_intent() {
        use notify::event::{ModifyKind, RenameMode};

        let dir = tempdir().unwrap();
        let root = dir.path();
        let watcher = FileWatcher::new(root, &IndexConfig::new(root)).unwrap();
        let old_dir = root.join("old_module");
        let new_dir = root.join("new_module");
        let new_nested = new_dir.join("nested").join("code.rs");
        fs::create_dir_all(new_nested.parent().unwrap()).unwrap();
        fs::write(&new_nested, "fn renamed_directory_file() {}").unwrap();
        fs::write(new_dir.join("ignored.png"), [0_u8; 8]).unwrap();

        let event = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(old_dir.clone())
            .add_path(new_dir.clone());
        let changes = watcher.process_events(vec![event]);
        assert!(changes.iter().any(|change| {
            change.path == old_dir && change.kind == ChangeKind::RemovedDirectory
        }));
        assert!(changes.iter().any(|change| {
            change.path == new_dir && change.kind == ChangeKind::CreatedDirectory
        }));
        assert!(!changes.iter().any(|change| change.path == new_nested));
        assert!(
            !changes
                .iter()
                .any(|change| change.path.ends_with("ignored.png"))
        );
    }

    #[test]
    fn unpaired_directory_to_emits_directory_intent() {
        use notify::event::{ModifyKind, RenameMode};

        let dir = tempdir().unwrap();
        let root = dir.path();
        let watcher = FileWatcher::new(root, &IndexConfig::new(root)).unwrap();
        let new_dir = root.join("new_module");
        let nested = new_dir.join("nested.rs");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(&nested, "fn arrived_in_later_window() {}").unwrap();

        let to = notify::Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
            .add_path(new_dir.clone());
        let changes = watcher.process_events(vec![to]);

        assert!(changes.iter().any(|change| {
            change.path == new_dir && change.kind == ChangeKind::CreatedDirectory
        }));
        assert!(!changes.iter().any(|change| change.path == nested));
    }

    #[test]
    fn generic_folder_create_emits_directory_intent() {
        use notify::event::CreateKind;

        let dir = tempdir().unwrap();
        let root = dir.path();
        let watcher = FileWatcher::new(root, &IndexConfig::new(root)).unwrap();
        let new_dir = root.join("generated_tree");
        let nested = new_dir.join("nested.rs");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(&nested, "fn created_folder_file() {}").unwrap();

        let event =
            notify::Event::new(EventKind::Create(CreateKind::Folder)).add_path(new_dir.clone());
        let changes = watcher.process_events(vec![event]);

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, new_dir);
        assert_eq!(changes[0].kind, ChangeKind::CreatedDirectory);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_watcher_reports_nested_directory_rename() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let old_dir = root.join("old_module");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("nested.rs"), "fn native_rename() {}").unwrap();
        let watcher = FileWatcher::new(root, &IndexConfig::new(root)).unwrap();

        let new_dir = root.join("new_module");
        fs::rename(&old_dir, &new_dir).unwrap();
        // FSEvents documents the two sides of a rename as independent events;
        // they can legitimately land in adjacent debounce windows. Drain a
        // few bounded batches just like the daemon loop does.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut changes = Vec::new();
        while Instant::now() < deadline {
            changes.extend(watcher.poll_changes(Duration::from_secs(1)));
            let saw_old = changes.iter().any(|change| {
                ((change.path == old_dir || change.path.ends_with("old_module"))
                    && change.kind == ChangeKind::RemovedDirectory)
                    || (change.path.ends_with("old_module/nested.rs")
                        && change.kind == ChangeKind::Removed)
            });
            let saw_new = changes.iter().any(|change| {
                ((change.path == new_dir || change.path.ends_with("new_module"))
                    && change.kind == ChangeKind::CreatedDirectory)
                    || (change.path.ends_with("new_module/nested.rs")
                        && change.kind == ChangeKind::Modified)
            });
            if saw_old && saw_new {
                break;
            }
        }

        assert!(
            changes.iter().any(|change| {
                ((change.path == old_dir || change.path.ends_with("old_module"))
                    && change.kind == ChangeKind::RemovedDirectory)
                    || (change.path.ends_with("old_module/nested.rs")
                        && change.kind == ChangeKind::Removed)
            }),
            "missing old rename side: {changes:?}"
        );
        assert!(changes.iter().any(|change| {
            ((change.path == new_dir || change.path.ends_with("new_module"))
                && change.kind == ChangeKind::CreatedDirectory)
                || (change.path.ends_with("new_module/nested.rs")
                    && change.kind == ChangeKind::Modified)
        }));
    }
}
