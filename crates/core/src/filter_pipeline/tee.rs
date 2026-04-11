use std::path::Path;
use std::time::{Duration, SystemTime};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum size of a single tee file (1 MB).
pub const TEE_MAX_FILE_BYTES: usize = 1024 * 1024;

/// Maximum total size of the tee directory (20 MB).
pub const TEE_MAX_DIR_BYTES: u64 = 20 * 1024 * 1024;

/// Maximum age of a tee file before it is cleaned up (2 hours).
pub const TEE_AGE_SECS: u64 = 2 * 60 * 60;

// ── Public functions ──────────────────────────────────────────────────────────

/// Write `full_output` to a tee file under `tee_dir`.
///
/// Returns the relative path `.codixing/tee/{filename}` on success, or
/// `None` if the write fails for any reason (tee is best-effort).
///
/// Behaviour:
/// - Hash-based dedup: if a file with the same content hash already exists
///   the existing path is returned without writing again.
/// - File size cap: if `full_output` exceeds `TEE_MAX_FILE_BYTES` it is
///   truncated to the cap before writing.
/// - Directory size cap: after writing, if the tee directory exceeds
///   `TEE_MAX_DIR_BYTES` the oldest files are evicted until it fits.
pub fn write_tee(tee_dir: &Path, tool_name: &str, full_output: &str) -> Option<String> {
    std::fs::create_dir_all(tee_dir).ok()?;

    let hash = content_hash(full_output);
    let filename = format!("{tool_name}-{hash}.txt");
    let file_path = tee_dir.join(&filename);

    if file_path.exists() {
        return Some(format!(".codixing/tee/{filename}"));
    }

    // Cap content at TEE_MAX_FILE_BYTES (byte-boundary safe).
    let content = cap_bytes(full_output, TEE_MAX_FILE_BYTES);

    std::fs::write(&file_path, content).ok()?;

    // Enforce directory size cap.
    enforce_dir_cap(tee_dir);

    Some(format!(".codixing/tee/{filename}"))
}

/// Remove tee files older than `TEE_AGE_SECS` from `tee_dir`.
pub fn cleanup_tee(tee_dir: &Path) {
    let max_age = Duration::from_secs(TEE_AGE_SECS);
    let now = SystemTime::now();

    let Ok(entries) = std::fs::read_dir(tee_dir) else {
        return;
    };

    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if !meta.is_file() {
                continue;
            }
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = now.duration_since(modified) {
                    if age > max_age {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
}

/// Remove all files from `tee_dir` (used by `codixing init`).
/// The directory itself is kept.
pub fn clear_tee(tee_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(tee_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Compute a 16-hex-char content hash using xxh3-64 (stable, fast).
fn content_hash(s: &str) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(s.as_bytes()))
}

/// Return a byte slice of `s` capped at `max_bytes`, respecting UTF-8 char
/// boundaries.
fn cap_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    // Walk backwards from max_bytes to find a valid char boundary.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Evict the oldest files from `tee_dir` until the total size is at or below
/// `TEE_MAX_DIR_BYTES`.
fn enforce_dir_cap(tee_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(tee_dir) else {
        return;
    };

    let mut files: Vec<(SystemTime, std::path::PathBuf, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta.modified().ok()?;
            Some((modified, e.path(), meta.len()))
        })
        .collect();

    let total: u64 = files.iter().map(|(_, _, sz)| sz).sum();
    if total <= TEE_MAX_DIR_BYTES {
        return;
    }

    files.sort_by_key(|(t, _, _)| *t);

    let mut remaining = total;
    for (_, path, size) in files {
        if remaining <= TEE_MAX_DIR_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            remaining = remaining.saturating_sub(size);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Write;

    use filetime::{FileTime, set_file_mtime};
    use tempfile::TempDir;

    use super::*;

    fn make_tee_dir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn write_and_read_tee() {
        let dir = make_tee_dir();
        let content = "hello tee output\nline two";
        let rel_path = write_tee(dir.path(), "search", content).expect("write_tee should succeed");

        assert!(rel_path.starts_with(".codixing/tee/"));
        let filename = rel_path.strip_prefix(".codixing/tee/").unwrap();
        let full_path = dir.path().join(filename);
        assert!(full_path.exists());
        let read_back = std::fs::read_to_string(&full_path).unwrap();
        assert_eq!(read_back, content);
    }

    #[test]
    fn write_tee_dedup() {
        let dir = make_tee_dir();
        let content = "same content";
        let path1 = write_tee(dir.path(), "tool", content).unwrap();
        let path2 = write_tee(dir.path(), "tool", content).unwrap();
        assert_eq!(path1, path2);

        // Only one file in the directory.
        let count = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn write_tee_large_file_capped() {
        let dir = make_tee_dir();
        // 2 MB of 'x'
        let big = "x".repeat(2 * 1024 * 1024);
        let rel_path = write_tee(dir.path(), "tool", &big).unwrap();
        let filename = rel_path.strip_prefix(".codixing/tee/").unwrap();
        let full_path = dir.path().join(filename);
        let size = std::fs::metadata(&full_path).unwrap().len() as usize;
        assert!(
            size <= TEE_MAX_FILE_BYTES,
            "file should be capped at 1 MB, got {size}"
        );
        assert!(size > 0, "file should not be empty");
    }

    #[test]
    fn cleanup_removes_old_files() {
        let dir = make_tee_dir();

        // Write two files.
        let old_path = dir.path().join("old-file.txt");
        let new_path = dir.path().join("new-file.txt");
        std::fs::File::create(&old_path)
            .unwrap()
            .write_all(b"old")
            .unwrap();
        std::fs::File::create(&new_path)
            .unwrap()
            .write_all(b"new")
            .unwrap();

        // Set old file's mtime to 3 hours ago.
        let three_hours_ago = FileTime::from_unix_time(
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                - (3 * 60 * 60),
            0,
        );
        set_file_mtime(&old_path, three_hours_ago).unwrap();

        cleanup_tee(dir.path());

        assert!(!old_path.exists(), "old file should have been removed");
        assert!(new_path.exists(), "recent file should remain");
    }

    #[test]
    fn clear_tee_removes_all() {
        let dir = make_tee_dir();

        for i in 0..5 {
            let p = dir.path().join(format!("file{i}.txt"));
            std::fs::write(p, "data").unwrap();
        }

        clear_tee(dir.path());

        let count = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(count, 0, "all files should be removed");
        assert!(dir.path().exists(), "directory itself should remain");
    }

    #[test]
    fn content_hash_deterministic() {
        let h1 = content_hash("hello");
        let h2 = content_hash("hello");
        let h3 = content_hash("world");
        assert_eq!(h1, h2, "same input must produce same hash");
        assert_ne!(h1, h3, "different input should produce different hash");
        assert_eq!(h1.len(), 16, "hash should be 16 hex chars");
    }
}
