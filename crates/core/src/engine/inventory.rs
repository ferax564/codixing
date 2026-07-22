//! Pre-index inventory: count indexable sources without writing an index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::IndexConfig;
use crate::error::Result;
use crate::language::detect_language;

use super::indexing::walk_source_files;

/// One language (or extension) bucket in a source inventory.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InventoryBucket {
    /// Display name (`Rust`, `Python`, …) or file extension (`rs`, `py`, …).
    pub name: String,
    /// Number of files in this bucket.
    pub file_count: usize,
    /// Sum of on-disk byte sizes for files in this bucket.
    pub total_bytes: u64,
}

/// Inventory of files that `codixing init` would index under a given config.
///
/// Produced by [`inventory_source_tree`]. Does not write any index artifacts.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceInventory {
    /// Project root that was scanned.
    pub root: PathBuf,
    /// Number of indexable source files discovered.
    pub file_count: usize,
    /// Sum of on-disk byte sizes for those files.
    pub total_bytes: u64,
    /// Per-language breakdown, sorted by file count descending then name.
    pub by_language: Vec<InventoryBucket>,
    /// Per-extension breakdown, sorted by file count descending then name.
    pub by_extension: Vec<InventoryBucket>,
    /// Extra roots that were also walked (from `IndexConfig::extra_roots`).
    pub extra_roots: Vec<PathBuf>,
}

/// Walk the same source surface as `init` and summarise what would be indexed.
///
/// Reuses [`walk_source_files`] so ignore rules, language filters, symlink
/// policy, and exclude patterns match a real build. File sizes come from
/// metadata; oversized files that the indexer later skips at read time are
/// still counted here (they remain walk candidates).
pub fn inventory_source_tree(root: &Path, config: &IndexConfig) -> Result<SourceInventory> {
    let files = walk_source_files(root, config)?;

    let mut lang_counts: HashMap<String, (usize, u64)> = HashMap::new();
    let mut ext_counts: HashMap<String, (usize, u64)> = HashMap::new();
    let mut total_bytes = 0u64;

    for path in &files {
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        total_bytes = total_bytes.saturating_add(bytes);

        let language = detect_language(path)
            .map(|lang| lang.name().to_string())
            .unwrap_or_else(|| "Unknown".to_string());
        let entry = lang_counts.entry(language).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(bytes);

        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_else(|| "(none)".to_string());
        let entry = ext_counts.entry(extension).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(bytes);
    }

    Ok(SourceInventory {
        root: root.to_path_buf(),
        file_count: files.len(),
        total_bytes,
        by_language: buckets_from_map(lang_counts),
        by_extension: buckets_from_map(ext_counts),
        extra_roots: config.extra_roots.clone(),
    })
}

fn buckets_from_map(map: HashMap<String, (usize, u64)>) -> Vec<InventoryBucket> {
    let mut buckets: Vec<InventoryBucket> = map
        .into_iter()
        .map(|(name, (file_count, total_bytes))| InventoryBucket {
            name,
            file_count,
            total_bytes,
        })
        .collect();
    buckets.sort_by(|a, b| {
        b.file_count
            .cmp(&a.file_count)
            .then_with(|| a.name.cmp(&b.name))
    });
    buckets
}

/// Disk capacity snapshot for the filesystem containing a path.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DiskSpace {
    /// Bytes available to non-privileged processes (≈ free for the current user).
    pub available_bytes: u64,
    /// Total filesystem size in bytes.
    pub total_bytes: u64,
}

/// Probe free/total disk space for the filesystem containing `path`.
///
/// Returns `None` when the probe fails (missing path, unsupported platform
/// stats, permission errors). Safe for doctor/init dry-run reporting. Walks
/// up to an existing ancestor so callers can pass a not-yet-created
/// `.codixing/` destination by using the project root.
pub fn probe_disk_space(path: &Path) -> Option<DiskSpace> {
    let mut probe = path.to_path_buf();
    loop {
        if probe.exists() {
            return disk_space_at(&probe);
        }
        if !probe.pop() {
            return disk_space_at(Path::new("."));
        }
    }
}

fn disk_space_at(path: &Path) -> Option<DiskSpace> {
    let available_bytes = fs4::available_space(path).ok()?;
    let total_bytes = fs4::total_space(path).ok()?;
    Some(DiskSpace {
        available_bytes,
        total_bytes,
    })
}

/// Available free disk space (bytes) for the filesystem containing `path`.
///
/// Thin wrapper over [`probe_disk_space`] for callers that only need free space.
pub fn available_disk_space(path: &Path) -> Option<u64> {
    probe_disk_space(path).map(|space| space.available_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::tempdir;

    #[test]
    fn inventory_counts_files_bytes_and_languages() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), b"pub fn a() {}\n").unwrap();
        fs::write(root.join("src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(root.join("README.md"), b"# hi\n").unwrap();
        fs::write(root.join("script.py"), b"print(1)\n").unwrap();

        let config = IndexConfig::new(&root);
        let inv = inventory_source_tree(&root, &config).unwrap();

        assert_eq!(inv.file_count, 4);
        assert!(inv.total_bytes > 0);
        assert!(
            inv.by_language
                .iter()
                .any(|b| b.name == "Rust" && b.file_count == 2),
            "expected Rust=2 in {:?}",
            inv.by_language
        );
        assert!(
            inv.by_language
                .iter()
                .any(|b| b.name == "Python" && b.file_count == 1),
            "expected Python=1 in {:?}",
            inv.by_language
        );
        assert!(
            inv.by_extension
                .iter()
                .any(|b| b.name == "rs" && b.file_count == 2),
            "expected rs=2 in {:?}",
            inv.by_extension
        );
        // Largest language first.
        assert_eq!(inv.by_language[0].name, "Rust");
    }

    #[test]
    fn inventory_respects_language_filter() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.rs"), b"fn a() {}\n").unwrap();
        fs::write(root.join("b.py"), b"x=1\n").unwrap();

        let mut config = IndexConfig::new(&root);
        config.languages.insert("rust".to_string());
        let inv = inventory_source_tree(&root, &config).unwrap();
        assert_eq!(inv.file_count, 1);
        assert_eq!(inv.by_language.len(), 1);
        assert_eq!(inv.by_language[0].name, "Rust");
    }

    #[test]
    fn available_disk_space_reports_some_bytes_for_existing_path() {
        let dir = tempdir().unwrap();
        let space = available_disk_space(dir.path());
        assert!(space.is_some(), "expected free space for tempdir");
        assert!(space.unwrap() > 0);
    }
}
