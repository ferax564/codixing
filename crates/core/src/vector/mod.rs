pub mod qdrant;

#[cfg(not(feature = "usearch"))]
use std::collections::BinaryHeap;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(not(feature = "usearch"))]
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

#[cfg(feature = "usearch")]
use usearch::{Index, IndexOptions, MetricKind, ScalarKind, new_index};

use crate::error::{CodixingError, Result};

const VECTOR_GENERATION_FORMAT: u32 = 1;
pub(crate) const VECTOR_ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
const VECTOR_PUBLICATION_LOCK_SUFFIX: &str = ".publication.lock";
const VECTOR_PUBLICATION_LOCK_FILE: &str = "vector-publication.lock";
const CODIXING_CONTROL_DIR_NAME: &str = ".codixing";
const INDEX_GENERATIONS_DIR_NAME: &str = "generations";
const VECTOR_ARTIFACT_DIR_NAME: &str = "vectors";
const INDEX_GENERATION_LEASE_FILE: &str = "generation.lease";
static GENERATION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A generation manifest is the publication point for a vector checkpoint.
///
/// The index and file-to-chunk map are written and synced first. A uniquely
/// named manifest is then renamed into place, so readers see either the prior
/// complete generation or the new complete generation, never a mixed pair.
#[derive(Debug, Serialize, Deserialize)]
struct VectorGenerationManifest {
    format_version: u32,
    generation: String,
    index_file: String,
    file_chunks_file: String,
    vector_count: u64,
}

#[derive(Debug)]
struct GenerationArtifacts {
    manifest_path: PathBuf,
    index_path: PathBuf,
    file_chunks_path: PathBuf,
    vector_count: usize,
}

#[derive(Debug)]
struct PublicationCleanup {
    generations: Vec<GenerationArtifacts>,
    legacy_index: bool,
    legacy_file_chunks: bool,
}

#[derive(Debug)]
struct StoreVectorLayout {
    control_dir: PathBuf,
    index_dir: PathBuf,
}

#[derive(Debug)]
struct PublicationLockGuard {
    _lock: File,
    _generation_lease: Option<File>,
}

fn path_file_name(path: &Path) -> Result<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| {
            CodixingError::VectorIndex(format!(
                "vector artifact path has no file name: {}",
                path.display()
            ))
        })
}

fn store_vector_layout(index_path: &Path) -> Option<StoreVectorLayout> {
    let vectors_dir = index_path.parent()?;
    if vectors_dir.file_name()?.to_str()? != VECTOR_ARTIFACT_DIR_NAME {
        return None;
    }

    let index_dir = vectors_dir.parent()?;
    if index_dir.file_name()?.to_str()? == CODIXING_CONTROL_DIR_NAME {
        return Some(StoreVectorLayout {
            control_dir: index_dir.to_path_buf(),
            index_dir: index_dir.to_path_buf(),
        });
    }

    let generations_dir = index_dir.parent()?;
    if generations_dir.file_name()?.to_str()? != INDEX_GENERATIONS_DIR_NAME {
        return None;
    }
    let control_dir = generations_dir.parent()?;
    if control_dir.file_name()?.to_str()? != CODIXING_CONTROL_DIR_NAME {
        return None;
    }

    Some(StoreVectorLayout {
        control_dir: control_dir.to_path_buf(),
        index_dir: index_dir.to_path_buf(),
    })
}

fn publication_lock_path(index_path: &Path) -> Result<PathBuf> {
    if let Some(layout) = store_vector_layout(index_path) {
        return Ok(layout.control_dir.join(VECTOR_PUBLICATION_LOCK_FILE));
    }

    let parent = index_path.parent().ok_or_else(|| {
        CodixingError::VectorIndex(format!(
            "vector index path has no parent: {}",
            index_path.display()
        ))
    })?;
    Ok(parent.join(format!(
        "{}{VECTOR_PUBLICATION_LOCK_SUFFIX}",
        path_file_name(index_path)?
    )))
}

fn open_publication_lock(index_path: &Path) -> Result<File> {
    let path = publication_lock_path(index_path)?;
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(CodixingError::VectorIndex(format!(
                    "vector publication lock is not a real file: {}",
                    path.display()
                )));
            }
            Ok(OpenOptions::new().read(true).write(true).open(path)?)
        }
        Err(error) => Err(error.into()),
    }
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CodixingError::VectorIndex(format!(
            "could not inspect {label} at {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(CodixingError::VectorIndex(format!(
            "{label} is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn acquire_generation_lease(index_dir: &Path) -> Result<File> {
    let path = index_dir.join(INDEX_GENERATION_LEASE_FILE);
    let lease = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(CodixingError::VectorIndex(format!(
                    "index generation lease is not a real file: {}",
                    path.display()
                )));
            }
            OpenOptions::new().read(true).write(true).open(path)?
        }
        Err(error) => return Err(error.into()),
    };
    FileExt::lock_shared(&lease)?;
    Ok(lease)
}

fn acquire_publication_lock(
    index_path: &Path,
    artifact_parent: &Path,
) -> Result<PublicationLockGuard> {
    let layout = store_vector_layout(index_path);
    if let Some(layout) = layout.as_ref() {
        // A repository-wide control lock coordinates legacy and generational
        // vector paths without becoming a versioned COW artifact. Requiring
        // the data root to exist also prevents a stale publisher from
        // recreating an outer generation after it has been retired.
        require_real_directory(&layout.control_dir, "index control directory")?;
        require_real_directory(&layout.index_dir, "index generation directory")?;
    } else {
        fs::create_dir_all(artifact_parent)?;
    }

    let lock = open_publication_lock(index_path)?;
    FileExt::lock_shared(&lock)?;
    let generation_lease = layout
        .as_ref()
        .map(|layout| acquire_generation_lease(&layout.index_dir))
        .transpose()?;

    if layout.is_some() {
        match fs::create_dir(artifact_parent) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        require_real_directory(artifact_parent, "vector artifact directory")?;
    }

    Ok(PublicationLockGuard {
        _lock: lock,
        _generation_lease: generation_lease,
    })
}

fn artifact_parent(index_path: &Path, file_chunks_path: &Path) -> Result<PathBuf> {
    let index_parent = index_path.parent().ok_or_else(|| {
        CodixingError::VectorIndex(format!(
            "vector index path has no parent: {}",
            index_path.display()
        ))
    })?;
    let chunks_parent = file_chunks_path.parent().ok_or_else(|| {
        CodixingError::VectorIndex(format!(
            "file-chunks path has no parent: {}",
            file_chunks_path.display()
        ))
    })?;
    if index_parent != chunks_parent {
        return Err(CodixingError::VectorIndex(format!(
            "vector index and file-chunks artifacts must share a directory: {} vs {}",
            index_parent.display(),
            chunks_parent.display()
        )));
    }
    Ok(index_parent.to_path_buf())
}

fn manifest_prefix(index_path: &Path) -> Result<String> {
    Ok(format!(
        "{}.manifest.generation-",
        path_file_name(index_path)?
    ))
}

fn generation_from_manifest_path(index_path: &Path, manifest_path: &Path) -> Option<String> {
    let prefix = manifest_prefix(index_path).ok()?;
    let name = manifest_path.file_name()?.to_string_lossy();
    name.strip_prefix(&prefix)
        .and_then(|rest| rest.strip_suffix(".json"))
        .filter(|generation| !generation.is_empty())
        .map(ToOwned::to_owned)
}

fn generation_paths(index_path: &Path, generation: &str) -> Result<GenerationArtifacts> {
    let parent = index_path.parent().ok_or_else(|| {
        CodixingError::VectorIndex(format!(
            "vector index path has no parent: {}",
            index_path.display()
        ))
    })?;
    let index_name = path_file_name(index_path)?;
    Ok(GenerationArtifacts {
        manifest_path: parent.join(format!(
            "{index_name}.manifest.generation-{generation}.json"
        )),
        index_path: parent.join(format!("{index_name}.generation-{generation}")),
        file_chunks_path: parent.join(format!("{index_name}.file-chunks.generation-{generation}")),
        vector_count: 0,
    })
}

fn manifest_paths(index_path: &Path) -> Result<Vec<PathBuf>> {
    let parent = index_path.parent().ok_or_else(|| {
        CodixingError::VectorIndex(format!(
            "vector index path has no parent: {}",
            index_path.display()
        ))
    })?;
    let prefix = manifest_prefix(index_path)?;
    let mut paths = Vec::new();
    match fs::read_dir(parent) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && name.ends_with(".json") {
                    paths.push(entry.path());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error.into()),
    }
    // Generation IDs start with a fixed-width monotonic timestamp. Sorting by
    // file name therefore selects the newest publication without a mutable
    // "current" pointer that would need cross-platform replacement semantics.
    paths.sort();
    paths.reverse();
    Ok(paths)
}

fn valid_generation_id(generation: &str) -> bool {
    let mut parts = generation.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(timestamp), Some(process), Some(nonce), None)
            if timestamp.len() == 32
                && process.len() == 8
                && nonce.len() == 16
                && timestamp.bytes().all(|byte| byte.is_ascii_hexdigit())
                && process.bytes().all(|byte| byte.is_ascii_hexdigit())
                && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    )
}

fn generation_from_unpublished_artifact_name(
    index_path: &Path,
    artifact_name: &str,
) -> Option<String> {
    let index_name = path_file_name(index_path).ok()?;
    let index_prefix = format!("{index_name}.generation-");
    let chunks_prefix = format!("{index_name}.file-chunks.generation-");
    let manifest_prefix = format!("{index_name}.manifest.generation-");
    let generation = artifact_name
        .strip_prefix(&index_prefix)
        .or_else(|| artifact_name.strip_prefix(&chunks_prefix))
        .or_else(|| {
            artifact_name
                .strip_prefix(&manifest_prefix)
                .and_then(|rest| rest.strip_suffix(".json.tmp"))
        })?;
    valid_generation_id(generation).then(|| generation.to_string())
}

/// Only exact generation-named regular files with no published manifest are
/// candidates. A generation is removed only when every artifact bearing that
/// ID is older than the conservative grace period; a young index, chunks file,
/// or temporary manifest therefore protects the whole in-progress generation.
/// The exclusive vector publication lock is the liveness proof that makes an
/// age-based repair safe. Ordinary open and publication paths never invoke it.
pub(crate) fn cleanup_abandoned_unpublished_generations(
    index_path: &Path,
    grace: Duration,
) -> Result<Vec<PathBuf>> {
    let parent = index_path.parent().ok_or_else(|| {
        CodixingError::VectorIndex(format!(
            "vector index path has no parent: {}",
            index_path.display()
        ))
    })?;

    let parent_metadata = match fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if !parent_metadata.file_type().is_dir() || parent_metadata.file_type().is_symlink() {
        return Err(CodixingError::VectorIndex(format!(
            "refusing vector repair through non-directory or symlinked path: {}",
            parent.display()
        )));
    }

    let maintenance_lock = open_publication_lock(index_path)?;
    if !FileExt::try_lock_exclusive(&maintenance_lock)? {
        return Err(CodixingError::VectorIndex(format!(
            "cannot repair vector artifacts while a publication is active beside {}",
            index_path.display()
        )));
    }

    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut candidates: HashMap<String, Vec<PathBuf>> = HashMap::new();
    let mut protected = HashSet::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(generation) = generation_from_unpublished_artifact_name(index_path, name) else {
            continue;
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
            candidates.entry(generation).or_default().push(entry.path());
        } else {
            // A recognized symlink, directory, or other special entry may be a
            // path trick, but it may also denote state we cannot prove dead.
            // Protect the whole generation without following or unlinking it.
            protected.insert(generation);
        }
    }

    let now = SystemTime::now();
    let mut generations: Vec<_> = candidates.into_iter().collect();
    generations.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    let mut removed = Vec::new();
    for (generation, mut artifacts) in generations {
        if protected.contains(&generation) {
            continue;
        }
        artifacts.sort_unstable();
        let expected = generation_paths(index_path, &generation)?;
        match fs::symlink_metadata(&expected.manifest_path) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let expired = artifacts.iter().try_fold(true, |expired, artifact| {
            let metadata = fs::symlink_metadata(artifact)?;
            let artifact_expired =
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() {
                    now.duration_since(metadata.modified()?)
                        .is_ok_and(|age| age >= grace)
                } else {
                    false
                };
            Ok::<_, std::io::Error>(expired && artifact_expired)
        })?;
        if !expired {
            continue;
        }

        // Recheck the publication point immediately before unlinking any data.
        match fs::symlink_metadata(&expected.manifest_path) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        for artifact in artifacts {
            let metadata = match fs::symlink_metadata(&artifact) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                continue;
            }
            match fs::remove_file(&artifact) {
                Ok(()) => removed.push(artifact),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    if !removed.is_empty() {
        sync_directory(parent)?;
    }
    removed.sort_unstable();
    Ok(removed)
}

fn next_generation(index_path: &Path) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let newest_sequence = manifest_paths(index_path)?
        .into_iter()
        .filter_map(|path| generation_from_manifest_path(index_path, &path))
        .filter_map(|generation| {
            generation
                .split('-')
                .next()
                .and_then(|part| u128::from_str_radix(part, 16).ok())
        })
        .max()
        .unwrap_or(0);
    let sequence = now.max(newest_sequence.saturating_add(1));
    let nonce = GENERATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(format!(
        "{sequence:032x}-{:08x}-{nonce:016x}",
        std::process::id()
    ))
}

fn tracked_id_count(file_chunks: &HashMap<String, Vec<u64>>) -> Result<usize> {
    file_chunks.values().try_fold(0usize, |total, ids| {
        total.checked_add(ids.len()).ok_or_else(|| {
            CodixingError::VectorIndex("tracked vector count overflowed usize".to_string())
        })
    })
}

fn validate_vector_counts(
    actual_count: usize,
    file_chunks: &HashMap<String, Vec<u64>>,
    manifest_count: Option<usize>,
) -> Result<()> {
    let tracked_count = tracked_id_count(file_chunks)?;
    if actual_count != tracked_count {
        return Err(CodixingError::VectorIndex(format!(
            "inconsistent vector artifacts: index contains {actual_count} vectors but file-chunks tracks {tracked_count} IDs"
        )));
    }
    if let Some(expected) = manifest_count
        && actual_count != expected
    {
        return Err(CodixingError::VectorIndex(format!(
            "inconsistent vector generation: manifest declares {expected} vectors but index contains {actual_count}"
        )));
    }
    Ok(())
}

fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_file(path: &Path) -> Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    // Windows does not permit opening directories through std::fs::File. The
    // two data files and manifest itself are still flushed before publication.
    Ok(())
}

fn resolve_manifest(index_path: &Path, manifest_path: &Path) -> Result<GenerationArtifacts> {
    let generation = generation_from_manifest_path(index_path, manifest_path).ok_or_else(|| {
        CodixingError::VectorIndex(format!(
            "invalid vector generation manifest name: {}",
            manifest_path.display()
        ))
    })?;
    let bytes = fs::read(manifest_path)?;
    let manifest: VectorGenerationManifest = serde_json::from_slice(&bytes).map_err(|error| {
        CodixingError::Serialization(format!(
            "failed to deserialize vector manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    if manifest.format_version != VECTOR_GENERATION_FORMAT {
        return Err(CodixingError::VectorIndex(format!(
            "unsupported vector generation format {} in {}",
            manifest.format_version,
            manifest_path.display()
        )));
    }
    if manifest.generation != generation {
        return Err(CodixingError::VectorIndex(format!(
            "vector manifest generation mismatch in {}",
            manifest_path.display()
        )));
    }

    let mut expected = generation_paths(index_path, &generation)?;
    if path_file_name(&expected.index_path)? != manifest.index_file
        || path_file_name(&expected.file_chunks_path)? != manifest.file_chunks_file
    {
        return Err(CodixingError::VectorIndex(format!(
            "vector manifest references unexpected artifact names in {}",
            manifest_path.display()
        )));
    }
    expected.vector_count = usize::try_from(manifest.vector_count).map_err(|_| {
        CodixingError::VectorIndex(format!(
            "vector count in {} does not fit this platform",
            manifest_path.display()
        ))
    })?;
    Ok(expected)
}

fn load_published_generation<T>(
    index_path: &Path,
    file_chunks_path: &Path,
    mut load_pair: impl FnMut(&Path, &Path, Option<usize>) -> Result<T>,
) -> Result<T> {
    artifact_parent(index_path, file_chunks_path)?;
    let mut last_error = None;

    // A second scan closes the small race where a read started before a new
    // manifest was published and its old generation was cleaned up meanwhile.
    for _ in 0..2 {
        for manifest_path in manifest_paths(index_path)? {
            let loaded = resolve_manifest(index_path, &manifest_path).and_then(|artifacts| {
                load_pair(
                    &artifacts.index_path,
                    &artifacts.file_chunks_path,
                    Some(artifacts.vector_count),
                )
            });
            match loaded {
                Ok(index) => return Ok(index),
                Err(error) => last_error = Some(error),
            }
        }

        // Legacy indexes used one canonical pair written in place. Keep this
        // fallback so upgrades do not force a full re-embed.
        if index_path.exists() && file_chunks_path.exists() {
            match load_pair(index_path, file_chunks_path, None) {
                Ok(index) => return Ok(index),
                Err(error) => last_error = Some(error),
            }
        }
    }

    if let Some(error) = last_error {
        Err(CodixingError::VectorIndex(format!(
            "no valid vector generation could be loaded: {error}"
        )))
    } else {
        Err(CodixingError::VectorIndex(format!(
            "no published vector artifacts found beside {}",
            index_path.display()
        )))
    }
}

fn artifacts_exist(index_path: &Path, file_chunks_path: &Path) -> bool {
    if artifact_parent(index_path, file_chunks_path).is_err() {
        return false;
    }
    if let Ok(paths) = manifest_paths(index_path) {
        for manifest_path in paths {
            if let Ok(artifacts) = resolve_manifest(index_path, &manifest_path)
                && artifacts.index_path.is_file()
                && artifacts.file_chunks_path.is_file()
            {
                return true;
            }
        }
    }
    index_path.is_file() && file_chunks_path.is_file()
}

/// Stable identity for the newest complete vector publication.
///
/// Generation manifests have monotonic names and are published only after both
/// vector artifacts are durable. Legacy in-place artifacts fall back to a
/// metadata fingerprint so long-lived readers can still notice replacements.
pub(crate) fn publication_token(index_path: &Path, file_chunks_path: &Path) -> Option<String> {
    if artifact_parent(index_path, file_chunks_path).is_err() {
        return None;
    }
    if let Ok(paths) = manifest_paths(index_path) {
        for manifest_path in paths {
            if let Ok(artifacts) = resolve_manifest(index_path, &manifest_path)
                && artifacts.index_path.is_file()
                && artifacts.file_chunks_path.is_file()
            {
                return artifacts
                    .manifest_path
                    .file_name()
                    .map(|name| format!("generation:{}", name.to_string_lossy()));
            }
        }
    }

    let index_metadata = fs::metadata(index_path).ok()?;
    let file_chunks_metadata = fs::metadata(file_chunks_path).ok()?;
    let modified_nanos = |metadata: &fs::Metadata| {
        metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    };
    Some(format!(
        "legacy:{}:{}:{}:{}",
        index_metadata.len(),
        modified_nanos(&index_metadata),
        file_chunks_metadata.len(),
        modified_nanos(&file_chunks_metadata)
    ))
}

fn publication_cleanup_snapshot(
    index_path: &Path,
    file_chunks_path: &Path,
) -> Result<PublicationCleanup> {
    let generations = manifest_paths(index_path)?
        .into_iter()
        .filter_map(|manifest_path| {
            let generation = generation_from_manifest_path(index_path, &manifest_path)?;
            generation_paths(index_path, &generation).ok()
        })
        .collect();
    Ok(PublicationCleanup {
        generations,
        legacy_index: index_path.is_file(),
        legacy_file_chunks: file_chunks_path.is_file(),
    })
}

fn cleanup_after_publication(
    index_path: &Path,
    file_chunks_path: &Path,
    cleanup: PublicationCleanup,
) {
    let Some(parent) = index_path.parent() else {
        return;
    };

    // Delete only generations whose manifests were visible before this save
    // started. A live directory sweep can remove another publisher's data
    // files after it writes them but before it publishes its manifest. Leaving
    // unpublished crash orphans for an explicit maintenance pass is safer than
    // racing an active cross-process publisher.
    for generation in cleanup.generations {
        let _ = fs::remove_file(generation.manifest_path);
        let _ = fs::remove_file(generation.index_path);
        let _ = fs::remove_file(generation.file_chunks_path);
    }

    // Legacy artifacts can be removed only now that a complete generation is
    // durably published, and only when they predated this save. Failures are
    // harmless and retried on a later save.
    if cleanup.legacy_index {
        let _ = fs::remove_file(index_path);
    }
    if cleanup.legacy_file_chunks {
        let _ = fs::remove_file(file_chunks_path);
    }
    let _ = sync_directory(parent);
}

fn publish_generation(
    index_path: &Path,
    file_chunks_path: &Path,
    file_chunks: &HashMap<String, Vec<u64>>,
    vector_count: usize,
    write_index: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    validate_vector_counts(vector_count, file_chunks, Some(vector_count))?;
    let parent = artifact_parent(index_path, file_chunks_path)?;
    // Public VectorIndex::save callers do not necessarily hold IndexStore's
    // writer lease. This shared control lock spans the complete publication so
    // explicit repair can prove that no paused publisher is still live. Store
    // paths also retain their generation lease until publication completes.
    let _publication_lock = acquire_publication_lock(index_path, &parent)?;
    // Never sweep unpublished generation files here: age cannot distinguish a
    // crashed writer from a live publisher paused before manifest publication.
    // The snapshot below contains published manifests only, so cleanup remains
    // limited to complete generations visible before this save began.
    let cleanup = publication_cleanup_snapshot(index_path, file_chunks_path)?;

    let generation = next_generation(index_path)?;
    let mut artifacts = generation_paths(index_path, &generation)?;
    artifacts.vector_count = vector_count;

    write_index(&artifacts.index_path)?;
    sync_file(&artifacts.index_path)?;

    let file_chunks_bytes = bitcode::serialize(file_chunks).map_err(|error| {
        CodixingError::Serialization(format!("failed to serialize file_chunks: {error}"))
    })?;
    write_new_file(&artifacts.file_chunks_path, &file_chunks_bytes)?;

    let manifest = VectorGenerationManifest {
        format_version: VECTOR_GENERATION_FORMAT,
        generation,
        index_file: path_file_name(&artifacts.index_path)?,
        file_chunks_file: path_file_name(&artifacts.file_chunks_path)?,
        vector_count: u64::try_from(vector_count).map_err(|_| {
            CodixingError::VectorIndex("vector count does not fit in u64".to_string())
        })?,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|error| {
        CodixingError::Serialization(format!("failed to serialize vector manifest: {error}"))
    })?;
    let manifest_tmp = artifacts.manifest_path.with_extension("json.tmp");
    write_new_file(&manifest_tmp, &manifest_bytes)?;
    fs::rename(&manifest_tmp, &artifacts.manifest_path)?;
    sync_directory(&parent)?;

    cleanup_after_publication(index_path, file_chunks_path, cleanup);
    Ok(())
}

/// Pluggable vector search backend.
///
/// Implement this trait to provide alternative storage for code chunk vectors.
/// The default implementation is [`VectorIndex`] (usearch HNSW, in-process).
/// An optional Qdrant backend is available behind `#[cfg(feature = "qdrant")]`.
pub trait VectorBackend: Send + Sync {
    /// Add a vector associated with `chunk_id` and `file_path`.
    fn add(&self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()>;

    /// Search for the `k` nearest vectors to `query`.
    ///
    /// Returns `(chunk_id, score)` pairs.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>>;

    /// Remove all vectors associated with `file_path`.
    fn remove_file(&mut self, file_path: &str) -> Result<()>;

    /// Total number of indexed vectors.
    fn size(&self) -> usize;

    /// Return the file-to-chunk-id mapping (owned clone, suitable for diagnostics).
    fn file_chunks_owned(&self) -> HashMap<String, Vec<u64>>;

    /// Persist the index to `dir`.
    fn save(&self, dir: &Path) -> Result<()>;
}

// ===========================================================================
// usearch-backed VectorIndex (default on Linux / macOS)
// ===========================================================================

#[cfg(feature = "usearch")]
mod usearch_impl {
    use super::*;

    /// Approximate nearest-neighbour HNSW index backed by usearch.
    ///
    /// Wraps a usearch [`Index`] and maintains a per-file chunk map
    /// (`file_chunks`) so entire files can be efficiently removed.
    pub struct VectorIndex {
        inner: Index,
        /// Maps file path -> list of chunk IDs stored in this index.
        file_chunks: HashMap<String, Vec<u64>>,
        /// Vector dimensionality (must match the embedder).
        pub dims: usize,
    }

    impl VectorIndex {
        fn validate_dims(&self, vector: &[f32]) -> Result<()> {
            if vector.len() != self.dims {
                return Err(CodixingError::VectorIndex(format!(
                    "vector dimension mismatch: expected {}, got {}",
                    self.dims,
                    vector.len()
                )));
            }
            Ok(())
        }

        /// Create a new empty index with the given vector dimensionality.
        ///
        /// When `quantize` is `true` the HNSW graph stores vectors as int8 instead
        /// of float32, reducing memory usage by 8x -- critical for repos with 1 M+
        /// LoC where the vector index alone can exceed 2 GB at full precision.
        pub fn new(dims: usize, quantize: bool) -> Result<Self> {
            let options = IndexOptions {
                dimensions: dims,
                metric: MetricKind::Cos,
                quantization: if quantize {
                    ScalarKind::I8
                } else {
                    ScalarKind::F32
                },
                connectivity: 0,
                expansion_add: 0,
                expansion_search: 0,
                multi: false,
            };
            let inner = new_index(&options)
                .map_err(|e| CodixingError::VectorIndex(format!("failed to create index: {e}")))?;
            Ok(Self {
                inner,
                file_chunks: HashMap::new(),
                dims,
            })
        }

        /// Add a vector to the index, associating it with `chunk_id`.
        ///
        /// `file_path` is tracked so the chunk can be removed when the file
        /// is removed from the index.
        pub fn add(&self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            self.validate_dims(vector)?;
            // Reserve additional capacity if needed (usearch grows, but an explicit
            // reserve of +1 keeps performance predictable).
            let needed = self.inner.size() + 1;
            self.inner
                .reserve(needed)
                .map_err(|e| CodixingError::VectorIndex(format!("reserve failed: {e}")))?;
            self.inner
                .add(chunk_id, vector)
                .map_err(|e| CodixingError::VectorIndex(format!("add failed: {e}")))?;
            // Caller is responsible for updating file_chunks (needs &mut self).
            let _ = file_path; // acknowledged here; see add_mut below
            Ok(())
        }

        /// Add a vector and record the file->chunk mapping (requires `&mut self`).
        pub fn add_mut(&mut self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            self.validate_dims(vector)?;
            let needed = self.inner.size() + 1;
            self.inner
                .reserve(needed)
                .map_err(|e| CodixingError::VectorIndex(format!("reserve failed: {e}")))?;
            self.inner
                .add(chunk_id, vector)
                .map_err(|e| CodixingError::VectorIndex(format!("add failed: {e}")))?;
            self.file_chunks
                .entry(file_path.to_string())
                .or_default()
                .push(chunk_id);
            Ok(())
        }

        /// Search for the `k` nearest vectors to `query`.
        ///
        /// Returns a list of `(chunk_id, distance)` pairs sorted by ascending distance.
        pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            if self.inner.size() == 0 {
                return Ok(Vec::new());
            }
            let matches = self
                .inner
                .search(query, k)
                .map_err(|e| CodixingError::VectorIndex(format!("search failed: {e}")))?;
            Ok(matches.keys.into_iter().zip(matches.distances).collect())
        }

        /// Remove all vectors belonging to the given file.
        pub fn remove_file(&mut self, file_path: &str) -> Result<()> {
            if let Some(chunk_ids) = self.file_chunks.remove(file_path) {
                for id in chunk_ids {
                    // Ignore errors for individual removes (chunk may not be present).
                    let _ = self.inner.remove(id);
                }
            }
            Ok(())
        }

        /// Total number of vectors currently in the index.
        pub fn len(&self) -> usize {
            self.inner.size()
        }

        /// Returns `true` if the index contains no vectors.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// Persist the HNSW graph and file-chunk map as one published generation.
        ///
        /// `index_path` and `file_chunks_path` are retained as legacy path
        /// anchors; newly saved data uses immutable generation files beside
        /// them and a unique manifest as the atomic publication point.
        pub fn save(&self, index_path: &Path, file_chunks_path: &Path) -> Result<()> {
            publish_generation(
                index_path,
                file_chunks_path,
                &self.file_chunks,
                self.len(),
                |generation_index_path| {
                    self.inner
                        .save(generation_index_path.to_string_lossy().as_ref())
                        .map_err(|error| {
                            CodixingError::VectorIndex(format!(
                                "save vector generation failed: {error}"
                            ))
                        })
                },
            )
        }

        /// Return whether a published generation or legacy artifact pair exists.
        pub fn artifacts_exist(index_path: &Path, file_chunks_path: &Path) -> bool {
            super::artifacts_exist(index_path, file_chunks_path)
        }

        /// Load an existing index from disk.
        ///
        /// Creates a fresh usearch `Index` with matching options then loads the
        /// persisted graph and the file-chunk map.  `quantize` must match the
        /// setting used when the index was originally created.
        pub fn load(
            index_path: &Path,
            file_chunks_path: &Path,
            dims: usize,
            quantize: bool,
        ) -> Result<Self> {
            load_published_generation(
                index_path,
                file_chunks_path,
                |idx_path, fc_path, expected| {
                    let idx = Self::new(dims, quantize)?;
                    idx.inner
                        .load(idx_path.to_string_lossy().as_ref())
                        .map_err(|error| {
                            CodixingError::VectorIndex(format!("load index failed: {error}"))
                        })?;

                    let bytes = fs::read(fc_path)?;
                    let file_chunks: HashMap<String, Vec<u64>> = bitcode::deserialize(&bytes)
                        .map_err(|error| {
                            CodixingError::Serialization(format!(
                                "failed to deserialize file_chunks: {error}"
                            ))
                        })?;
                    validate_vector_counts(idx.inner.size(), &file_chunks, expected)?;

                    Ok(Self {
                        inner: idx.inner,
                        file_chunks,
                        dims,
                    })
                },
            )
        }

        /// Access the file-chunk map (for persistence).
        pub fn file_chunks(&self) -> &HashMap<String, Vec<u64>> {
            &self.file_chunks
        }

        /// Retrieve the stored vector for the given `chunk_id`.
        ///
        /// Returns `None` if the chunk is not in the index.  Uses the usearch
        /// `get` API to read the vector back from the HNSW graph.
        pub fn get_vector(&self, chunk_id: u64) -> Option<Vec<f32>> {
            let mut buf = vec![0.0f32; self.dims];
            match self.inner.get(chunk_id, &mut buf) {
                Ok(found) if found > 0 => Some(buf),
                _ => None,
            }
        }
    }

    impl VectorBackend for VectorIndex {
        fn add(&self, chunk_id: u64, vector: &[f32], _file_path: &str) -> Result<()> {
            // Shared-reference add (no file_chunks tracking). Use add_mut for full tracking.
            self.validate_dims(vector)?;
            let needed = self.inner.size() + 1;
            self.inner
                .reserve(needed)
                .map_err(|e| CodixingError::VectorIndex(format!("reserve failed: {e}")))?;
            self.inner
                .add(chunk_id, vector)
                .map_err(|e| CodixingError::VectorIndex(format!("add failed: {e}")))?;
            Ok(())
        }

        fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            VectorIndex::search(self, query, k)
        }

        fn remove_file(&mut self, file_path: &str) -> Result<()> {
            VectorIndex::remove_file(self, file_path)
        }

        fn size(&self) -> usize {
            self.len()
        }

        fn file_chunks_owned(&self) -> HashMap<String, Vec<u64>> {
            self.file_chunks.clone()
        }

        fn save(&self, dir: &Path) -> Result<()> {
            let index_path = dir.join("vectors.usearch");
            let file_chunks_path = dir.join("file_chunks.bin");
            VectorIndex::save(self, &index_path, &file_chunks_path)
        }
    }
}

#[cfg(feature = "usearch")]
pub use usearch_impl::VectorIndex;

// ===========================================================================
// Brute-force fallback VectorIndex (used on Windows / --no-default-features)
// ===========================================================================

#[cfg(not(feature = "usearch"))]
mod brute_force_impl {
    use super::*;
    use xxhash_rust::xxh3::Xxh3;

    const CDXV_MAGIC: &[u8; 4] = b"CDXV";
    const CDXV_V1_FORMAT_VERSION: u32 = 1;
    const CDXV_FORMAT_VERSION: u32 = 2;
    const CDXV_V1_HEADER_SIZE: u64 = 20;
    const CDXV_V2_HEADER_SIZE: u64 = 28;

    #[derive(Clone, Copy, Debug)]
    struct EntryLocation {
        entry_pos: usize,
        file_slot: usize,
        file_pos: usize,
    }

    #[derive(Clone, Copy, Debug)]
    struct SearchCandidate {
        chunk_id: u64,
        distance: f32,
    }

    impl PartialEq for SearchCandidate {
        fn eq(&self, other: &Self) -> bool {
            self.chunk_id == other.chunk_id && self.distance.to_bits() == other.distance.to_bits()
        }
    }

    impl Eq for SearchCandidate {}

    impl PartialOrd for SearchCandidate {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    impl Ord for SearchCandidate {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.distance
                .total_cmp(&other.distance)
                .then_with(|| self.chunk_id.cmp(&other.chunk_id))
        }
    }

    #[derive(Deserialize)]
    struct LegacyVectorData {
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        dims: Option<usize>,
        entries: Vec<LegacyVectorEntry>,
    }

    #[derive(Deserialize)]
    struct LegacyVectorEntry {
        chunk_id: u64,
        vector: Vec<f32>,
    }

    /// Brute-force vector index using cosine similarity.
    ///
    /// Drop-in replacement for the usearch-backed `VectorIndex` when the
    /// `usearch` feature is disabled (e.g. on Windows where usearch uses
    /// POSIX `MAP_FAILED`). O(N) per query but works on all platforms.
    pub struct VectorIndex {
        /// Per-chunk vectors, keyed by chunk ID.
        entries: Vec<(u64, Vec<f32>)>,
        /// Cached inverse L2 norms, position-aligned with `entries`.
        inverse_norms: Vec<f32>,
        /// Derived ID -> vector/file positions for constant-time mutation.
        locations: HashMap<u64, EntryLocation>,
        /// Maps file path -> list of chunk IDs stored in this index.
        file_chunks: HashMap<String, Vec<u64>>,
        /// Reusable file-path slots referenced by `locations`.
        file_slots: Vec<Option<String>>,
        /// Derived file path -> stable slot lookup.
        file_slot_by_path: HashMap<String, usize>,
        /// Vacant `file_slots` positions available for reuse.
        free_file_slots: Vec<usize>,
        /// Vector dimensionality (must match the embedder).
        pub dims: usize,
    }

    impl VectorIndex {
        /// Create a new empty index with the given vector dimensionality.
        ///
        /// The `quantize` parameter is accepted for API compatibility with
        /// the usearch backend but is ignored (brute-force always uses f32).
        pub fn new(dims: usize, _quantize: bool) -> Result<Self> {
            Ok(Self {
                entries: Vec::new(),
                inverse_norms: Vec::new(),
                locations: HashMap::new(),
                file_chunks: HashMap::new(),
                file_slots: Vec::new(),
                file_slot_by_path: HashMap::new(),
                free_file_slots: Vec::new(),
                dims,
            })
        }

        fn inverse_norm(vector: &[f32]) -> f32 {
            let squared = crate::index::simd_distance::dot_product(vector, vector);
            if squared.is_finite() && squared > 0.0 {
                squared.sqrt().recip()
            } else {
                0.0
            }
        }

        fn ensure_file_slot(&mut self, file_path: &str) -> Result<usize> {
            if let Some(slot) = self.file_slot_by_path.get(file_path) {
                return Ok(*slot);
            }

            let slot = if let Some(slot) = self.free_file_slots.pop() {
                let vacant = self.file_slots.get_mut(slot).ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state has an invalid free file slot {slot}"
                    ))
                })?;
                if vacant.is_some() {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector lookup state tried to reuse occupied file slot {slot}"
                    )));
                }
                *vacant = Some(file_path.to_string());
                slot
            } else {
                let slot = self.file_slots.len();
                self.file_slots.push(Some(file_path.to_string()));
                slot
            };
            self.file_slot_by_path.insert(file_path.to_string(), slot);
            Ok(slot)
        }

        fn release_file_slot(&mut self, file_path: &str, expected_slot: usize) -> Result<()> {
            let mapped_slot = self
                .file_slot_by_path
                .get(file_path)
                .copied()
                .ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state is missing file slot for {file_path}"
                    ))
                })?;
            if mapped_slot != expected_slot {
                return Err(CodixingError::VectorIndex(format!(
                    "vector lookup state points {file_path} at the wrong file slot"
                )));
            }
            let stored_path = self
                .file_slots
                .get(expected_slot)
                .and_then(Option::as_deref);
            if stored_path != Some(file_path) {
                return Err(CodixingError::VectorIndex(format!(
                    "vector lookup state has an invalid file slot for {file_path}"
                )));
            }

            self.file_slot_by_path.remove(file_path);
            self.file_slots[expected_slot] = None;
            self.free_file_slots.push(expected_slot);
            Ok(())
        }

        fn from_persisted(
            entries: Vec<(u64, Vec<f32>)>,
            file_chunks: HashMap<String, Vec<u64>>,
            dims: usize,
            expected: Option<usize>,
        ) -> Result<Self> {
            validate_vector_counts(entries.len(), &file_chunks, expected)?;

            let mut locations = HashMap::new();
            locations.try_reserve(entries.len()).map_err(|error| {
                CodixingError::VectorIndex(format!(
                    "failed to reserve persisted vector lookup state: {error}"
                ))
            })?;
            for (entry_pos, (chunk_id, vector)) in entries.iter().enumerate() {
                if vector.len() != dims {
                    return Err(CodixingError::VectorIndex(format!(
                        "persisted vector dimension mismatch: expected {dims}, got {}",
                        vector.len()
                    )));
                }
                if locations
                    .insert(
                        *chunk_id,
                        EntryLocation {
                            entry_pos,
                            file_slot: usize::MAX,
                            file_pos: usize::MAX,
                        },
                    )
                    .is_some()
                {
                    return Err(CodixingError::VectorIndex(format!(
                        "persisted vector index contains duplicate chunk ID {chunk_id}"
                    )));
                }
            }

            let mut file_slots = Vec::new();
            file_slots
                .try_reserve_exact(file_chunks.len())
                .map_err(|error| {
                    CodixingError::VectorIndex(format!(
                        "failed to reserve persisted vector file slots: {error}"
                    ))
                })?;
            let mut file_slot_by_path = HashMap::new();
            file_slot_by_path
                .try_reserve(file_chunks.len())
                .map_err(|error| {
                    CodixingError::VectorIndex(format!(
                        "failed to reserve persisted vector file lookup: {error}"
                    ))
                })?;
            for (file_path, chunk_ids) in &file_chunks {
                let file_slot = file_slots.len();
                file_slots.push(Some(file_path.clone()));
                file_slot_by_path.insert(file_path.clone(), file_slot);

                for (file_pos, chunk_id) in chunk_ids.iter().enumerate() {
                    let location = locations.get_mut(chunk_id).ok_or_else(|| {
                        CodixingError::VectorIndex(format!(
                            "file-chunks maps {file_path} to missing chunk ID {chunk_id}"
                        ))
                    })?;
                    if location.file_slot != usize::MAX {
                        return Err(CodixingError::VectorIndex(format!(
                            "chunk ID {chunk_id} is owned by more than one file"
                        )));
                    }
                    location.file_slot = file_slot;
                    location.file_pos = file_pos;
                }
            }

            if let Some((chunk_id, _)) = locations
                .iter()
                .find(|(_, location)| location.file_slot == usize::MAX)
            {
                return Err(CodixingError::VectorIndex(format!(
                    "persisted chunk ID {chunk_id} has no file owner"
                )));
            }

            let mut inverse_norms = Vec::new();
            inverse_norms
                .try_reserve_exact(entries.len())
                .map_err(|error| {
                    CodixingError::VectorIndex(format!(
                        "failed to reserve persisted vector norms: {error}"
                    ))
                })?;
            inverse_norms.extend(entries.iter().map(|(_, vector)| Self::inverse_norm(vector)));
            let index = Self {
                entries,
                inverse_norms,
                locations,
                file_chunks,
                file_slots,
                file_slot_by_path,
                free_file_slots: Vec::new(),
                dims,
            };
            index.validate_derived_state()?;
            Ok(index)
        }

        fn validate_derived_state(&self) -> Result<()> {
            validate_vector_counts(
                self.entries.len(),
                &self.file_chunks,
                Some(self.entries.len()),
            )?;
            if self.inverse_norms.len() != self.entries.len()
                || self.locations.len() != self.entries.len()
            {
                return Err(CodixingError::VectorIndex(
                    "vector lookup state has inconsistent lengths".to_string(),
                ));
            }

            let mut free_seen = Vec::new();
            free_seen
                .try_reserve_exact(self.file_slots.len())
                .map_err(|error| {
                    CodixingError::VectorIndex(format!(
                        "failed to reserve vector file-slot validation state: {error}"
                    ))
                })?;
            free_seen.resize(self.file_slots.len(), false);
            for slot in &self.free_file_slots {
                let seen = free_seen.get_mut(*slot).ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state has an invalid free file slot {slot}"
                    ))
                })?;
                if *seen {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector lookup state lists free file slot {slot} more than once"
                    )));
                }
                *seen = true;
            }
            for (slot, file_path) in self.file_slots.iter().enumerate() {
                match file_path {
                    Some(file_path)
                        if free_seen[slot]
                            || self.file_slot_by_path.get(file_path) != Some(&slot)
                            || !self.file_chunks.contains_key(file_path) =>
                    {
                        return Err(CodixingError::VectorIndex(format!(
                            "vector lookup state has an invalid active file slot for {file_path}"
                        )));
                    }
                    Some(_) => {}
                    None if !free_seen[slot] => {
                        return Err(CodixingError::VectorIndex(format!(
                            "vector lookup state lost vacant file slot {slot}"
                        )));
                    }
                    None => {}
                }
            }
            if self.file_slot_by_path.len() + self.free_file_slots.len() != self.file_slots.len() {
                return Err(CodixingError::VectorIndex(
                    "vector lookup state has inconsistent file-slot counts".to_string(),
                ));
            }

            for (entry_pos, (chunk_id, vector)) in self.entries.iter().enumerate() {
                if vector.len() != self.dims {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector dimension mismatch: expected {}, got {}",
                        self.dims,
                        vector.len()
                    )));
                }
                let location = self.locations.get(chunk_id).ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state is missing chunk ID {chunk_id}"
                    ))
                })?;
                if location.entry_pos != entry_pos {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector lookup state points chunk ID {chunk_id} at the wrong entry"
                    )));
                }
                let file_path = self
                    .file_slots
                    .get(location.file_slot)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| {
                        CodixingError::VectorIndex(format!(
                            "vector lookup state has an invalid file slot for chunk ID {chunk_id}"
                        ))
                    })?;
                if self
                    .file_chunks
                    .get(file_path)
                    .and_then(|ids| ids.get(location.file_pos))
                    != Some(chunk_id)
                {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector lookup state has an invalid file position for chunk ID {chunk_id}"
                    )));
                }
            }

            for (file_path, chunk_ids) in &self.file_chunks {
                let file_slot = self.file_slot_by_path.get(file_path).ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state is missing file slot for {file_path}"
                    ))
                })?;
                for (file_pos, chunk_id) in chunk_ids.iter().enumerate() {
                    let location = self.locations.get(chunk_id).ok_or_else(|| {
                        CodixingError::VectorIndex(format!(
                            "file-chunks maps {file_path} to missing chunk ID {chunk_id}"
                        ))
                    })?;
                    if location.file_slot != *file_slot || location.file_pos != file_pos {
                        return Err(CodixingError::VectorIndex(format!(
                            "vector lookup state disagrees with the owner of chunk ID {chunk_id}"
                        )));
                    }
                }
            }
            Ok(())
        }

        fn write_cdxv(&self, path: &Path) -> Result<()> {
            let dims = u32::try_from(self.dims).map_err(|_| {
                CodixingError::VectorIndex("vector dimensions do not fit in u32".to_string())
            })?;
            let count = u64::try_from(self.entries.len()).map_err(|_| {
                CodixingError::VectorIndex("vector count does not fit in u64".to_string())
            })?;
            let file = OpenOptions::new().write(true).create_new(true).open(path)?;
            let mut writer = BufWriter::new(file);
            writer.write_all(CDXV_MAGIC)?;
            writer.write_all(&CDXV_FORMAT_VERSION.to_le_bytes())?;
            writer.write_all(&dims.to_le_bytes())?;
            writer.write_all(&count.to_le_bytes())?;
            writer.write_all(&0_u64.to_le_bytes())?;
            let mut checksum = Xxh3::new();
            for (chunk_id, _) in &self.entries {
                let bytes = chunk_id.to_le_bytes();
                checksum.update(&bytes);
                writer.write_all(&bytes)?;
            }
            for (_, vector) in &self.entries {
                for value in vector {
                    let bytes = value.to_le_bytes();
                    checksum.update(&bytes);
                    writer.write_all(&bytes)?;
                }
            }
            writer.seek(SeekFrom::Start(CDXV_V1_HEADER_SIZE))?;
            writer.write_all(&checksum.digest().to_le_bytes())?;
            writer.flush()?;
            Ok(())
        }

        fn load_cdxv(
            path: &Path,
            dims: usize,
            manifest_count: Option<usize>,
        ) -> Result<Vec<(u64, Vec<f32>)>> {
            let file = File::open(path)?;
            let file_len = file.metadata()?.len();
            let mut reader = BufReader::new(file);
            let mut magic = [0_u8; 4];
            reader.read_exact(&mut magic)?;
            if &magic != CDXV_MAGIC {
                return Err(CodixingError::VectorIndex(
                    "invalid CDXV vector magic".to_string(),
                ));
            }

            let mut version_bytes = [0_u8; 4];
            reader.read_exact(&mut version_bytes)?;
            let version = u32::from_le_bytes(version_bytes);
            if version != CDXV_V1_FORMAT_VERSION && version != CDXV_FORMAT_VERSION {
                return Err(CodixingError::VectorIndex(format!(
                    "unsupported CDXV vector format version {version}"
                )));
            }

            let mut dims_bytes = [0_u8; 4];
            reader.read_exact(&mut dims_bytes)?;
            let stored_dims = u32::from_le_bytes(dims_bytes);
            if u32::try_from(dims).ok() != Some(stored_dims) {
                return Err(CodixingError::VectorIndex(format!(
                    "persisted vector dimension mismatch: expected {dims}, got {stored_dims}"
                )));
            }

            let mut count_bytes = [0_u8; 8];
            reader.read_exact(&mut count_bytes)?;
            let count = u64::from_le_bytes(count_bytes);
            if let Some(manifest_count) = manifest_count {
                let manifest_count = u64::try_from(manifest_count).map_err(|_| {
                    CodixingError::VectorIndex(
                        "manifest vector count does not fit in u64".to_string(),
                    )
                })?;
                if count != manifest_count {
                    return Err(CodixingError::VectorIndex(format!(
                        "inconsistent vector generation: manifest declares {manifest_count} vectors but CDXV header declares {count}"
                    )));
                }
            }
            let stored_checksum = if version == CDXV_FORMAT_VERSION {
                let mut checksum_bytes = [0_u8; 8];
                reader.read_exact(&mut checksum_bytes)?;
                Some(u64::from_le_bytes(checksum_bytes))
            } else {
                None
            };
            let row_bytes = u64::from(stored_dims).checked_mul(4).ok_or_else(|| {
                CodixingError::VectorIndex("CDXV vector row size overflow".to_string())
            })?;
            let header_size = if version == CDXV_FORMAT_VERSION {
                CDXV_V2_HEADER_SIZE
            } else {
                CDXV_V1_HEADER_SIZE
            };
            let expected_size = header_size
                .checked_add(count.checked_mul(8).ok_or_else(|| {
                    CodixingError::VectorIndex("CDXV ID table size overflow".to_string())
                })?)
                .and_then(|size| {
                    count
                        .checked_mul(row_bytes)
                        .and_then(|rows| size.checked_add(rows))
                })
                .ok_or_else(|| {
                    CodixingError::VectorIndex("CDXV vector file size overflow".to_string())
                })?;
            if file_len != expected_size {
                return Err(CodixingError::VectorIndex(format!(
                    "invalid CDXV vector file size: expected {expected_size} bytes, got {file_len}"
                )));
            }

            let count = usize::try_from(count).map_err(|_| {
                CodixingError::VectorIndex("CDXV vector count does not fit in usize".to_string())
            })?;
            let row_bytes = usize::try_from(row_bytes).map_err(|_| {
                CodixingError::VectorIndex("CDXV vector row does not fit in usize".to_string())
            })?;
            let mut entries = Vec::new();
            entries.try_reserve_exact(count).map_err(|error| {
                CodixingError::VectorIndex(format!(
                    "failed to reserve CDXV vector entries: {error}"
                ))
            })?;
            let mut checksum = Xxh3::new();
            for _ in 0..count {
                let mut id_bytes = [0_u8; 8];
                reader.read_exact(&mut id_bytes)?;
                checksum.update(&id_bytes);
                let mut vector = Vec::new();
                vector.try_reserve_exact(dims).map_err(|error| {
                    CodixingError::VectorIndex(format!(
                        "failed to reserve CDXV vector row: {error}"
                    ))
                })?;
                entries.push((u64::from_le_bytes(id_bytes), vector));
            }

            let mut row = Vec::new();
            row.try_reserve_exact(row_bytes).map_err(|error| {
                CodixingError::VectorIndex(format!("failed to reserve CDXV input row: {error}"))
            })?;
            row.resize(row_bytes, 0);
            for (_, vector) in &mut entries {
                reader.read_exact(&mut row)?;
                checksum.update(&row);
                vector.extend(
                    row.chunks_exact(4)
                        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
                );
            }
            if let Some(stored_checksum) = stored_checksum {
                let actual_checksum = checksum.digest();
                if actual_checksum != stored_checksum {
                    return Err(CodixingError::VectorIndex(format!(
                        "CDXV vector payload checksum mismatch: expected {stored_checksum:016x}, got {actual_checksum:016x}"
                    )));
                }
            }
            Ok(entries)
        }

        fn load_legacy_json(path: &Path, dims: usize) -> Result<Vec<(u64, Vec<f32>)>> {
            let file = File::open(path)?;
            let data: LegacyVectorData =
                serde_json::from_reader(BufReader::new(file)).map_err(|error| {
                    CodixingError::VectorIndex(format!(
                        "failed to deserialize legacy vector index: {error}"
                    ))
                })?;
            if let Some(kind) = data.kind
                && kind != "brute_force"
            {
                return Err(CodixingError::VectorIndex(format!(
                    "unsupported legacy vector index type {kind}"
                )));
            }
            if let Some(stored_dims) = data.dims
                && stored_dims != dims
            {
                return Err(CodixingError::VectorIndex(format!(
                    "persisted vector dimension mismatch: expected {dims}, got {stored_dims}"
                )));
            }
            data.entries
                .into_iter()
                .map(|entry| {
                    if entry.vector.len() != dims {
                        return Err(CodixingError::VectorIndex(format!(
                            "persisted vector dimension mismatch: expected {dims}, got {}",
                            entry.vector.len()
                        )));
                    }
                    Ok((entry.chunk_id, entry.vector))
                })
                .collect()
        }

        fn load_entries(
            path: &Path,
            dims: usize,
            manifest_count: Option<usize>,
        ) -> Result<Vec<(u64, Vec<f32>)>> {
            let mut file = File::open(path)?;
            let mut magic = [0_u8; 4];
            match file.read_exact(&mut magic) {
                Ok(()) if &magic == CDXV_MAGIC => Self::load_cdxv(path, dims, manifest_count),
                Ok(()) => Self::load_legacy_json(path, dims),
                Err(error) => Err(CodixingError::VectorIndex(format!(
                    "vector index is too small to identify: {error}"
                ))),
            }
        }

        /// Add a vector to the index, associating it with `chunk_id`.
        ///
        /// `file_path` is tracked so the chunk can be removed when the file
        /// is removed from the index.
        pub fn add(&self, _chunk_id: u64, _vector: &[f32], _file_path: &str) -> Result<()> {
            // Shared-reference add is not supported in the brute-force backend.
            // The engine always uses add_mut, so this is only for VectorBackend trait compat.
            Err(CodixingError::VectorIndex(
                "brute-force VectorIndex requires &mut self; use add_mut instead".to_string(),
            ))
        }

        /// Add a vector and record the file->chunk mapping (requires `&mut self`).
        pub fn add_mut(&mut self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            if vector.len() != self.dims {
                return Err(CodixingError::VectorIndex(format!(
                    "vector dimension mismatch: expected {}, got {}",
                    self.dims,
                    vector.len()
                )));
            }
            let inverse_norm = Self::inverse_norm(vector);
            let new_file_slot = self.ensure_file_slot(file_path)?;

            if let Some(mut location) = self.locations.get(&chunk_id).copied() {
                let old_file_slot = location.file_slot;
                let old_file_path = self
                    .file_slots
                    .get(location.file_slot)
                    .and_then(Option::as_ref)
                    .cloned()
                    .ok_or_else(|| {
                        CodixingError::VectorIndex(format!(
                            "vector lookup state has an invalid owner for chunk ID {chunk_id}"
                        ))
                    })?;
                if self
                    .file_chunks
                    .get(&old_file_path)
                    .and_then(|ids| ids.get(location.file_pos))
                    != Some(&chunk_id)
                {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector lookup state has an invalid file position for chunk ID {chunk_id}"
                    )));
                }

                if location.file_slot != new_file_slot {
                    let (moved_id, old_file_empty) = {
                        let old_ids =
                            self.file_chunks.get_mut(&old_file_path).ok_or_else(|| {
                                CodixingError::VectorIndex(format!(
                                    "vector lookup state is missing owner {old_file_path}"
                                ))
                            })?;
                        old_ids.swap_remove(location.file_pos);
                        (old_ids.get(location.file_pos).copied(), old_ids.is_empty())
                    };
                    if let Some(moved_id) = moved_id {
                        let moved_location =
                            self.locations.get_mut(&moved_id).ok_or_else(|| {
                                CodixingError::VectorIndex(format!(
                                    "vector lookup state is missing moved chunk ID {moved_id}"
                                ))
                            })?;
                        moved_location.file_pos = location.file_pos;
                    }
                    if old_file_empty {
                        self.file_chunks.remove(&old_file_path);
                    }

                    let new_ids = self.file_chunks.entry(file_path.to_string()).or_default();
                    location.file_slot = new_file_slot;
                    location.file_pos = new_ids.len();
                    new_ids.push(chunk_id);
                    self.locations.insert(chunk_id, location);
                    if old_file_empty {
                        self.release_file_slot(&old_file_path, old_file_slot)?;
                    }
                }

                self.entries[location.entry_pos].1.copy_from_slice(vector);
                self.inverse_norms[location.entry_pos] = inverse_norm;
            } else {
                let entry_pos = self.entries.len();
                let new_ids = self.file_chunks.entry(file_path.to_string()).or_default();
                let file_pos = new_ids.len();
                new_ids.push(chunk_id);
                self.entries.push((chunk_id, vector.to_vec()));
                self.inverse_norms.push(inverse_norm);
                self.locations.insert(
                    chunk_id,
                    EntryLocation {
                        entry_pos,
                        file_slot: new_file_slot,
                        file_pos,
                    },
                );
            }
            Ok(())
        }

        /// Search for the `k` nearest vectors to `query`.
        ///
        /// Returns a list of `(chunk_id, distance)` pairs sorted by ascending
        /// cosine distance (0.0 = identical, 1.0 = orthogonal), matching the
        /// usearch backend's return convention.
        pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            if self.entries.is_empty() || k == 0 {
                return Ok(Vec::new());
            }
            if query.len() != self.dims {
                return Err(CodixingError::VectorIndex(format!(
                    "vector dimension mismatch: expected {}, got {}",
                    self.dims,
                    query.len()
                )));
            }

            const BRUTE_FORCE_WARN_THRESHOLD: usize = 50_000;
            if self.entries.len() > BRUTE_FORCE_WARN_THRESHOLD {
                tracing::warn!(
                    count = self.entries.len(),
                    "brute-force vector search over {} vectors — consider enabling the \
                     `usearch` feature for sub-linear ANN search",
                    self.entries.len()
                );
            }

            let query_inverse_norm = Self::inverse_norm(query);
            let limit = k.min(self.entries.len());
            let mut nearest = BinaryHeap::with_capacity(limit);
            for ((chunk_id, vector), vector_inverse_norm) in
                self.entries.iter().zip(&self.inverse_norms)
            {
                let similarity = if query_inverse_norm == 0.0 || *vector_inverse_norm == 0.0 {
                    0.0
                } else {
                    crate::index::simd_distance::dot_product(query, vector)
                        * query_inverse_norm
                        * vector_inverse_norm
                };
                let raw_distance = 1.0 - similarity;
                let candidate = SearchCandidate {
                    chunk_id: *chunk_id,
                    distance: if raw_distance.is_finite() {
                        raw_distance
                    } else {
                        f32::INFINITY
                    },
                };
                if nearest.len() < limit {
                    nearest.push(candidate);
                } else if nearest.peek().is_some_and(|worst| candidate < *worst) {
                    nearest.pop();
                    nearest.push(candidate);
                }
            }

            Ok(nearest
                .into_sorted_vec()
                .into_iter()
                .map(|candidate| (candidate.chunk_id, candidate.distance))
                .collect())
        }

        /// Remove all vectors belonging to the given file.
        pub fn remove_file(&mut self, file_path: &str) -> Result<()> {
            let Some(chunk_ids) = self.file_chunks.get(file_path) else {
                return Ok(());
            };
            let file_slot = self
                .file_slot_by_path
                .get(file_path)
                .copied()
                .ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state is missing file slot for {file_path}"
                    ))
                })?;
            for (file_pos, chunk_id) in chunk_ids.iter().enumerate() {
                let location = self.locations.get(chunk_id).ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state is missing chunk ID {chunk_id}"
                    ))
                })?;
                if self
                    .file_slots
                    .get(location.file_slot)
                    .and_then(Option::as_deref)
                    != Some(file_path)
                    || location.file_pos != file_pos
                    || self.entries.get(location.entry_pos).map(|entry| entry.0) != Some(*chunk_id)
                {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector lookup state is inconsistent for chunk ID {chunk_id}"
                    )));
                }
            }

            let chunk_ids = self.file_chunks.remove(file_path).unwrap_or_default();
            for chunk_id in chunk_ids {
                let location = self.locations.remove(&chunk_id).ok_or_else(|| {
                    CodixingError::VectorIndex(format!(
                        "vector lookup state is missing chunk ID {chunk_id}"
                    ))
                })?;
                let removed = self.entries.swap_remove(location.entry_pos);
                self.inverse_norms.swap_remove(location.entry_pos);
                if removed.0 != chunk_id {
                    return Err(CodixingError::VectorIndex(format!(
                        "vector lookup state removed the wrong chunk ID for {chunk_id}"
                    )));
                }
                if let Some((moved_id, _)) = self.entries.get(location.entry_pos) {
                    let moved_location = self.locations.get_mut(moved_id).ok_or_else(|| {
                        CodixingError::VectorIndex(format!(
                            "vector lookup state is missing moved chunk ID {moved_id}"
                        ))
                    })?;
                    moved_location.entry_pos = location.entry_pos;
                }
            }
            self.release_file_slot(file_path, file_slot)?;
            Ok(())
        }

        #[cfg(test)]
        pub(super) fn file_slot_storage_counts(&self) -> (usize, usize, usize) {
            (
                self.file_slots.len(),
                self.file_slot_by_path.len(),
                self.free_file_slots.len(),
            )
        }

        /// Total number of vectors currently in the index.
        pub fn len(&self) -> usize {
            self.entries.len()
        }

        /// Returns `true` if the index contains no vectors.
        pub fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        /// Persist the index and file-chunk map as one published generation.
        pub fn save(&self, index_path: &Path, file_chunks_path: &Path) -> Result<()> {
            self.validate_derived_state()?;
            publish_generation(
                index_path,
                file_chunks_path,
                &self.file_chunks,
                self.len(),
                |generation_index_path| self.write_cdxv(generation_index_path),
            )
        }

        /// Return whether a published generation or legacy artifact pair exists.
        pub fn artifacts_exist(index_path: &Path, file_chunks_path: &Path) -> bool {
            super::artifacts_exist(index_path, file_chunks_path)
        }

        /// Load an existing index from disk.
        ///
        /// The `quantize` parameter is accepted for API compatibility but ignored.
        pub fn load(
            index_path: &Path,
            file_chunks_path: &Path,
            dims: usize,
            _quantize: bool,
        ) -> Result<Self> {
            load_published_generation(
                index_path,
                file_chunks_path,
                |idx_path, fc_path, expected| {
                    let entries = Self::load_entries(idx_path, dims, expected)?;
                    let fc_bytes = fs::read(fc_path)?;
                    let file_chunks: HashMap<String, Vec<u64>> = bitcode::deserialize(&fc_bytes)
                        .map_err(|error| {
                            CodixingError::Serialization(format!(
                                "failed to deserialize file_chunks: {error}"
                            ))
                        })?;
                    Self::from_persisted(entries, file_chunks, dims, expected)
                },
            )
        }

        /// Access the file-chunk map (for persistence).
        pub fn file_chunks(&self) -> &HashMap<String, Vec<u64>> {
            &self.file_chunks
        }

        /// Retrieve the stored vector for the given `chunk_id`.
        ///
        /// Returns `None` if the chunk is not in the index.
        pub fn get_vector(&self, chunk_id: u64) -> Option<Vec<f32>> {
            self.locations
                .get(&chunk_id)
                .and_then(|location| self.entries.get(location.entry_pos))
                .map(|(_, vector)| vector.clone())
        }
    }

    impl VectorBackend for VectorIndex {
        fn add(&self, chunk_id: u64, vector: &[f32], file_path: &str) -> Result<()> {
            VectorIndex::add(self, chunk_id, vector, file_path)
        }

        fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u64, f32)>> {
            VectorIndex::search(self, query, k)
        }

        fn remove_file(&mut self, file_path: &str) -> Result<()> {
            VectorIndex::remove_file(self, file_path)
        }

        fn size(&self) -> usize {
            self.len()
        }

        fn file_chunks_owned(&self) -> HashMap<String, Vec<u64>> {
            self.file_chunks.clone()
        }

        fn save(&self, dir: &Path) -> Result<()> {
            let index_path = dir.join("vectors.usearch");
            let file_chunks_path = dir.join("file_chunks.bin");
            VectorIndex::save(self, &index_path, &file_chunks_path)
        }
    }
}

#[cfg(not(feature = "usearch"))]
pub use brute_force_impl::VectorIndex;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_vec(dims: usize, dominant: usize) -> Vec<f32> {
        let mut v = vec![0.01f32; dims];
        v[dominant] = 1.0;
        v
    }

    fn write_test_manifest(
        index_path: &Path,
        generation: &str,
        vector_count: usize,
    ) -> GenerationArtifacts {
        let mut artifacts = generation_paths(index_path, generation).unwrap();
        artifacts.vector_count = vector_count;
        let manifest = VectorGenerationManifest {
            format_version: VECTOR_GENERATION_FORMAT,
            generation: generation.to_string(),
            index_file: path_file_name(&artifacts.index_path).unwrap(),
            file_chunks_file: path_file_name(&artifacts.file_chunks_path).unwrap(),
            vector_count: vector_count as u64,
        };
        fs::write(
            &artifacts.manifest_path,
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        artifacts
    }

    #[cfg(not(feature = "usearch"))]
    fn write_test_cdxv(path: &Path, dims: u32, entries: &[(u64, Vec<f32>)]) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"CDXV");
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&dims.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (chunk_id, _) in entries {
            bytes.extend_from_slice(&chunk_id.to_le_bytes());
        }
        for (_, vector) in entries {
            for value in vector {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
        fs::write(path, bytes).unwrap();
    }

    #[cfg(not(feature = "usearch"))]
    fn write_test_file_chunks(path: &Path, file_chunks: &HashMap<String, Vec<u64>>) {
        fs::write(path, bitcode::serialize(file_chunks).unwrap()).unwrap();
    }

    #[test]
    fn add_and_search() {
        let mut idx = VectorIndex::new(4, false).unwrap();
        let a = unit_vec(4, 0);
        let b = unit_vec(4, 1);
        let c = unit_vec(4, 2);
        idx.add_mut(1, &a, "a.rs").unwrap();
        idx.add_mut(2, &b, "b.rs").unwrap();
        idx.add_mut(3, &c, "c.rs").unwrap();

        // Query close to 'a' -- should rank chunk 1 first.
        let results = idx.search(&a, 3).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn remove_file_drops_vectors() {
        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(10, &unit_vec(4, 0), "x.rs").unwrap();
        idx.add_mut(11, &unit_vec(4, 1), "y.rs").unwrap();

        idx.remove_file("x.rs").unwrap();

        // x.rs chunks should be gone; y.rs still present.
        assert!(!idx.file_chunks().contains_key("x.rs"));
        assert!(idx.file_chunks().contains_key("y.rs"));
    }

    #[test]
    fn empty_index_search_returns_empty() {
        let idx = VectorIndex::new(4, false).unwrap();
        let results = idx.search(&unit_vec(4, 0), 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn add_mut_rejects_wrong_dimensions() {
        let mut idx = VectorIndex::new(4, false).unwrap();
        let err = idx.add_mut(1, &[1.0, 0.0, 0.0], "a.rs").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("vector dimension mismatch: expected 4, got 3"),
            "unexpected error: {msg}"
        );
        assert_eq!(idx.len(), 0);
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_update_moves_file_mapping_without_duplication() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("updated.usearch");
        let fc_path = dir.path().join("updated_file_chunks.bin");
        let mut idx = VectorIndex::new(4, false).unwrap();

        idx.add_mut(5, &unit_vec(4, 0), "old.rs").unwrap();
        idx.add_mut(5, &unit_vec(4, 1), "new.rs").unwrap();

        assert_eq!(idx.len(), 1);
        assert!(!idx.file_chunks().contains_key("old.rs"));
        assert_eq!(idx.file_chunks().get("new.rs"), Some(&vec![5]));
        idx.save(&idx_path, &fc_path).unwrap();
        let loaded = VectorIndex::load(&idx_path, &fc_path, 4, false).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.file_chunks().get("new.rs"), Some(&vec![5]));
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_top_k_matches_deterministic_full_sort() {
        let dims = 8;
        let query: Vec<f32> = (0..dims).map(|i| (i as f32 + 1.0) / 11.0).collect();
        let mut source = Vec::new();
        let mut idx = VectorIndex::new(dims, false).unwrap();
        for chunk_id in 0..257_u64 {
            let vector: Vec<f32> = (0..dims)
                .map(|dimension| {
                    ((chunk_id as usize * 17 + dimension * 29 + 3) % 101) as f32 / 101.0
                })
                .collect();
            idx.add_mut(chunk_id, &vector, &format!("file_{}.rs", chunk_id % 13))
                .unwrap();
            source.push((chunk_id, vector));
        }

        let inverse_norm = |vector: &[f32]| {
            let squared = crate::index::simd_distance::dot_product(vector, vector);
            if squared > 0.0 {
                squared.sqrt().recip()
            } else {
                0.0
            }
        };
        let query_inverse_norm = inverse_norm(&query);
        let mut expected: Vec<(u64, f32)> = source
            .iter()
            .map(|(chunk_id, vector)| {
                let distance = 1.0
                    - crate::index::simd_distance::dot_product(&query, vector)
                        * query_inverse_norm
                        * inverse_norm(vector);
                (*chunk_id, distance)
            })
            .collect();
        expected.sort_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| left.0.cmp(&right.0))
        });

        assert!(idx.search(&[0.0], 0).unwrap().is_empty());
        for k in [1, 7, 32, source.len(), source.len() + 10] {
            let actual = idx.search(&query, k).unwrap();
            let expected = &expected[..k.min(expected.len())];
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!(actual.0, expected.0);
                assert!((actual.1 - expected.1).abs() <= 1e-6);
            }
        }
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_top_k_ties_are_stable_and_dimensions_are_checked() {
        let mut idx = VectorIndex::new(3, false).unwrap();
        for chunk_id in [30, 10, 20] {
            idx.add_mut(chunk_id, &[1.0, 0.0, 0.0], "ties.rs").unwrap();
        }
        let results = idx.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(
            results.iter().map(|result| result.0).collect::<Vec<_>>(),
            vec![10, 20]
        );

        let error = idx.search(&[1.0, 0.0], 1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("vector dimension mismatch: expected 3, got 2")
        );
        assert!(idx.search(&[1.0], 0).unwrap().is_empty());
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_updates_moves_and_swap_removes_repair_all_lookups() {
        let mut idx = VectorIndex::new(3, false).unwrap();
        idx.add_mut(1, &[1.0, 0.0, 0.0], "a.rs").unwrap();
        idx.add_mut(2, &[0.9, 0.1, 0.0], "a.rs").unwrap();
        idx.add_mut(3, &[0.8, 0.2, 0.0], "a.rs").unwrap();
        idx.add_mut(4, &[0.0, 1.0, 0.0], "b.rs").unwrap();
        idx.add_mut(5, &[0.0, 0.0, 1.0], "b.rs").unwrap();

        idx.add_mut(2, &[0.0, 0.5, 0.5], "a.rs").unwrap();
        idx.add_mut(3, &[0.5, 0.5, 0.0], "b.rs").unwrap();
        idx.add_mut(4, &[0.25, 0.75, 0.0], "b.rs").unwrap();

        assert_eq!(idx.len(), 5);
        assert_eq!(idx.file_chunks().get("a.rs"), Some(&vec![1, 2]));
        assert_eq!(idx.file_chunks().get("b.rs"), Some(&vec![4, 5, 3]));
        assert_eq!(idx.get_vector(2), Some(vec![0.0, 0.5, 0.5]));
        assert_eq!(idx.get_vector(4), Some(vec![0.25, 0.75, 0.0]));

        idx.remove_file("a.rs").unwrap();
        assert_eq!(idx.len(), 3);
        assert!(idx.get_vector(1).is_none());
        assert!(idx.get_vector(2).is_none());
        for chunk_id in [3, 4, 5] {
            assert!(idx.get_vector(chunk_id).is_some());
        }

        idx.remove_file("b.rs").unwrap();
        assert!(idx.is_empty());
        assert!(idx.file_chunks().is_empty());
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_reuses_file_slots_under_delete_and_rename_churn() {
        let mut idx = VectorIndex::new(3, false).unwrap();
        for chunk_id in 0..1_000_u64 {
            let file_path = format!("deleted_{chunk_id}.rs");
            idx.add_mut(chunk_id, &[1.0, 0.0, 0.0], &file_path).unwrap();
            idx.remove_file(&file_path).unwrap();
        }
        assert_eq!(idx.file_slot_storage_counts(), (1, 0, 1));

        idx.add_mut(10_000, &[1.0, 0.0, 0.0], "rename_0.rs")
            .unwrap();
        for revision in 1..1_000 {
            idx.add_mut(10_000, &[1.0, 0.0, 0.0], &format!("rename_{revision}.rs"))
                .unwrap();
        }
        assert_eq!(idx.file_slot_storage_counts(), (2, 1, 1));
        idx.remove_file("rename_999.rs").unwrap();
        assert_eq!(idx.file_slot_storage_counts(), (2, 0, 2));
    }

    #[test]
    fn vector_index_implements_backend_trait() {
        // Compile-time check: VectorIndex must satisfy VectorBackend.
        fn _assert_backend<T: VectorBackend>() {}
        _assert_backend::<VectorIndex>();
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("test.usearch");
        let fc_path = dir.path().join("file_chunks.bin");

        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(42, &unit_vec(4, 0), "foo.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        assert!(VectorIndex::artifacts_exist(&idx_path, &fc_path));
        assert!(!idx_path.exists(), "new saves must not use the legacy path");
        assert!(!fc_path.exists(), "new saves must not use the legacy path");
        assert_eq!(manifest_paths(&idx_path).unwrap().len(), 1);

        let loaded = VectorIndex::load(&idx_path, &fc_path, 4, false).unwrap();
        assert_eq!(loaded.len(), 1);
        let results = loaded.search(&unit_vec(4, 0), 1).unwrap();
        assert_eq!(results[0].0, 42);
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_save_uses_cdxv_and_rebuilds_derived_state() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("binary.usearch");
        let fc_path = dir.path().join("binary_file_chunks.bin");
        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(1, &unit_vec(4, 0), "a.rs").unwrap();
        idx.add_mut(2, &unit_vec(4, 1), "a.rs").unwrap();
        idx.add_mut(3, &unit_vec(4, 2), "b.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        let manifest = manifest_paths(&idx_path).unwrap().pop().unwrap();
        let artifacts = resolve_manifest(&idx_path, &manifest).unwrap();
        let bytes = fs::read(&artifacts.index_path).unwrap();
        assert_eq!(&bytes[..4], b"CDXV");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 4);

        let mut loaded = VectorIndex::load(&idx_path, &fc_path, 4, false).unwrap();
        loaded.add_mut(2, &[0.0, 0.0, 0.0, 1.0], "b.rs").unwrap();
        loaded.remove_file("a.rs").unwrap();
        assert!(loaded.get_vector(1).is_none());
        assert!(loaded.get_vector(2).is_some());
        assert!(loaded.get_vector(3).is_some());
        assert_eq!(loaded.file_chunks().get("b.rs"), Some(&vec![3, 2]));
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_loads_cdxv_v1() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("v1.usearch");
        let fc_path = dir.path().join("v1-file-chunks.bin");
        write_test_cdxv(
            &idx_path,
            3,
            &[(7, vec![1.0, 0.0, 0.0]), (8, vec![0.0, 1.0, 0.0])],
        );
        write_test_file_chunks(
            &fc_path,
            &HashMap::from([("v1.rs".to_string(), vec![7, 8])]),
        );

        let loaded = VectorIndex::load(&idx_path, &fc_path, 3, false).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.search(&[1.0, 0.0, 0.0], 1).unwrap()[0].0, 7);
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_rejects_cdxv_v2_payload_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("checksum.usearch");
        let fc_path = dir.path().join("checksum-file-chunks.bin");
        let mut idx = VectorIndex::new(3, false).unwrap();
        idx.add_mut(7, &[1.0, 0.0, 0.0], "checksum.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        let manifest_path = manifest_paths(&idx_path).unwrap().pop().unwrap();
        let artifacts = resolve_manifest(&idx_path, &manifest_path).unwrap();
        let mut bytes = fs::read(&artifacts.index_path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
        let last = bytes.last_mut().unwrap();
        *last ^= 1;
        fs::write(&artifacts.index_path, bytes).unwrap();

        let error = match VectorIndex::load(&idx_path, &fc_path, 3, false) {
            Ok(_) => panic!("corrupt CDXV payload should be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("payload checksum mismatch"));
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_rejects_manifest_count_before_cdxv_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("manifest-count.usearch");
        let fc_path = dir.path().join("manifest-count-file-chunks.bin");
        let mut idx = VectorIndex::new(3, false).unwrap();
        idx.add_mut(7, &[1.0, 0.0, 0.0], "count.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        let manifest_path = manifest_paths(&idx_path).unwrap().pop().unwrap();
        let mut manifest: VectorGenerationManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.vector_count = 2;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let error = match VectorIndex::load(&idx_path, &fc_path, 3, false) {
            Ok(_) => panic!("manifest/CDXV count mismatch should be rejected"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("manifest declares 2 vectors but CDXV header declares 1")
        );
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_loads_legacy_json_and_migrates_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("legacy-json.usearch");
        let fc_path = dir.path().join("legacy-json-file-chunks.bin");
        let legacy = serde_json::json!({
            "type": "brute_force",
            "dims": 3,
            "entries": [
                { "chunk_id": 7, "vector": [1.0, 0.0, 0.0] },
                { "chunk_id": 8, "vector": [0.0, 1.0, 0.0] }
            ]
        });
        fs::write(&idx_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        write_test_file_chunks(
            &fc_path,
            &HashMap::from([("a.rs".to_string(), vec![7]), ("b.rs".to_string(), vec![8])]),
        );

        let loaded = VectorIndex::load(&idx_path, &fc_path, 3, false).unwrap();
        assert_eq!(loaded.search(&[1.0, 0.0, 0.0], 1).unwrap()[0].0, 7);
        loaded.save(&idx_path, &fc_path).unwrap();

        assert!(
            !idx_path.exists(),
            "migration should retire the legacy pair"
        );
        assert!(!fc_path.exists(), "migration should retire the legacy pair");
        let manifest = manifest_paths(&idx_path).unwrap().pop().unwrap();
        let artifacts = resolve_manifest(&idx_path, &manifest).unwrap();
        assert_eq!(&fs::read(artifacts.index_path).unwrap()[..4], b"CDXV");
        assert_eq!(
            VectorIndex::load(&idx_path, &fc_path, 3, false)
                .unwrap()
                .len(),
            2
        );
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_rejects_corrupt_binary_dimensions_and_duplicate_ids() {
        let dir = tempfile::tempdir().unwrap();

        let truncated_idx = dir.path().join("truncated.usearch");
        let truncated_fc = dir.path().join("truncated-file-chunks.bin");
        let mut truncated = Vec::new();
        truncated.extend_from_slice(b"CDXV");
        truncated.extend_from_slice(&1_u32.to_le_bytes());
        truncated.extend_from_slice(&3_u32.to_le_bytes());
        truncated.extend_from_slice(&1_u64.to_le_bytes());
        fs::write(&truncated_idx, truncated).unwrap();
        write_test_file_chunks(
            &truncated_fc,
            &HashMap::from([("a.rs".to_string(), vec![1])]),
        );
        let error = match VectorIndex::load(&truncated_idx, &truncated_fc, 3, false) {
            Ok(_) => panic!("truncated CDXV file should be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("invalid CDXV vector file size"));

        let wrong_dims_idx = dir.path().join("wrong-dims.usearch");
        let wrong_dims_fc = dir.path().join("wrong-dims-file-chunks.bin");
        write_test_cdxv(&wrong_dims_idx, 2, &[(1, vec![1.0, 0.0])]);
        write_test_file_chunks(
            &wrong_dims_fc,
            &HashMap::from([("a.rs".to_string(), vec![1])]),
        );
        let error = match VectorIndex::load(&wrong_dims_idx, &wrong_dims_fc, 3, false) {
            Ok(_) => panic!("CDXV dimensions should be validated"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("persisted vector dimension mismatch: expected 3, got 2")
        );

        let duplicate_idx = dir.path().join("duplicate.usearch");
        let duplicate_fc = dir.path().join("duplicate-file-chunks.bin");
        write_test_cdxv(
            &duplicate_idx,
            2,
            &[(7, vec![1.0, 0.0]), (7, vec![0.0, 1.0])],
        );
        write_test_file_chunks(
            &duplicate_fc,
            &HashMap::from([("a.rs".to_string(), vec![7, 7])]),
        );
        let error = match VectorIndex::load(&duplicate_idx, &duplicate_fc, 2, false) {
            Ok(_) => panic!("duplicate vector IDs should be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("duplicate chunk ID 7"));
    }

    #[cfg(not(feature = "usearch"))]
    #[test]
    fn brute_force_rejects_missing_or_duplicate_file_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("owners.usearch");
        let fc_path = dir.path().join("owners-file-chunks.bin");
        write_test_cdxv(&idx_path, 2, &[(7, vec![1.0, 0.0]), (8, vec![0.0, 1.0])]);

        write_test_file_chunks(&fc_path, &HashMap::from([("a.rs".to_string(), vec![7, 9])]));
        let missing = match VectorIndex::load(&idx_path, &fc_path, 2, false) {
            Ok(_) => panic!("missing vector ownership should be rejected"),
            Err(error) => error,
        };
        assert!(missing.to_string().contains("missing chunk ID 9"));

        write_test_file_chunks(
            &fc_path,
            &HashMap::from([("a.rs".to_string(), vec![7]), ("b.rs".to_string(), vec![7])]),
        );
        let duplicate = match VectorIndex::load(&idx_path, &fc_path, 2, false) {
            Ok(_) => panic!("duplicate vector ownership should be rejected"),
            Err(error) => error,
        };
        assert!(
            duplicate
                .to_string()
                .contains("owned by more than one file")
        );
    }

    #[test]
    fn publication_token_changes_for_each_complete_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("checkpoint.usearch");
        let fc_path = dir.path().join("checkpoint_file_chunks.bin");
        let mut idx = VectorIndex::new(4, false).unwrap();

        idx.add_mut(1, &unit_vec(4, 0), "a.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();
        let first = publication_token(&idx_path, &fc_path).unwrap();

        idx.add_mut(2, &unit_vec(4, 1), "b.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();
        let second = publication_token(&idx_path, &fc_path).unwrap();

        assert_ne!(first, second);
        assert_eq!(
            VectorIndex::load(&idx_path, &fc_path, 4, false)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn unpublished_generation_is_ignored_with_legacy_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("legacy.usearch");
        let fc_path = dir.path().join("legacy_file_chunks.bin");

        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(7, &unit_vec(4, 2), "legacy.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        let published_manifest = manifest_paths(&idx_path).unwrap().pop().unwrap();
        let published = resolve_manifest(&idx_path, &published_manifest).unwrap();
        fs::copy(&published.index_path, &idx_path).unwrap();
        fs::copy(&published.file_chunks_path, &fc_path).unwrap();
        fs::remove_file(&published_manifest).unwrap();

        // These immutable data files model a crash before manifest publication.
        // With no manifest they must not shadow the complete legacy pair.
        assert!(published.index_path.exists());
        assert!(published.file_chunks_path.exists());
        let loaded = VectorIndex::load(&idx_path, &fc_path, 4, false).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.search(&unit_vec(4, 2), 1).unwrap()[0].0, 7);
    }

    #[test]
    fn invalid_newest_manifest_falls_back_to_valid_generation() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("fallback.usearch");
        let fc_path = dir.path().join("fallback_file_chunks.bin");

        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(11, &unit_vec(4, 1), "valid.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        // Publish a lexically newer manifest whose data files are absent,
        // modeling a damaged external copy. The loader must continue to the
        // newest generation that validates completely.
        let bad_generation = "ffffffffffffffffffffffffffffffff-ffffffff-ffffffffffffffff";
        write_test_manifest(&idx_path, bad_generation, 99);

        let loaded = VectorIndex::load(&idx_path, &fc_path, 4, false).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.search(&unit_vec(4, 1), 1).unwrap()[0].0, 11);
    }

    #[test]
    fn inconsistent_generation_count_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("count.usearch");
        let fc_path = dir.path().join("count_file_chunks.bin");

        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(42, &unit_vec(4, 0), "foo.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        let manifest_path = manifest_paths(&idx_path).unwrap().pop().unwrap();
        let artifacts = resolve_manifest(&idx_path, &manifest_path).unwrap();
        let inconsistent: HashMap<String, Vec<u64>> =
            HashMap::from([("foo.rs".to_string(), vec![42, 999])]);
        fs::write(
            &artifacts.file_chunks_path,
            bitcode::serialize(&inconsistent).unwrap(),
        )
        .unwrap();

        let error = match VectorIndex::load(&idx_path, &fc_path, 4, false) {
            Ok(_) => panic!("inconsistent vector generation should be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("tracks 2 IDs"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn successful_save_cleans_only_previously_published_generations() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("cleanup.usearch");
        let fc_path = dir.path().join("cleanup_file_chunks.bin");

        let mut idx = VectorIndex::new(4, false).unwrap();
        idx.add_mut(1, &unit_vec(4, 0), "a.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();
        let first_manifest = manifest_paths(&idx_path).unwrap().pop().unwrap();
        let first = resolve_manifest(&idx_path, &first_manifest).unwrap();

        let orphan_generation = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-eeeeeeee-eeeeeeeeeeeeeeee";
        let orphan = generation_paths(&idx_path, orphan_generation).unwrap();
        fs::write(&orphan.index_path, b"partial").unwrap();
        fs::write(&orphan.file_chunks_path, b"partial").unwrap();
        fs::write(&idx_path, b"legacy").unwrap();
        fs::write(&fc_path, b"legacy").unwrap();

        idx.add_mut(2, &unit_vec(4, 1), "b.rs").unwrap();
        idx.save(&idx_path, &fc_path).unwrap();

        assert_eq!(manifest_paths(&idx_path).unwrap().len(), 1);
        assert!(!first.manifest_path.exists());
        assert!(!first.index_path.exists());
        assert!(!first.file_chunks_path.exists());
        assert!(
            orphan.index_path.exists(),
            "unpublished data may belong to a concurrent publisher"
        );
        assert!(
            orphan.file_chunks_path.exists(),
            "unpublished data may belong to a concurrent publisher"
        );
        assert!(!idx_path.exists());
        assert!(!fc_path.exists());
    }

    #[test]
    fn abandoned_unpublished_generation_sweep_recovers_partial_states() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("orphan-cleanup.usearch");
        let fc_path = dir.path().join("orphan-cleanup-file-chunks.bin");
        fs::write(&idx_path, b"legacy-index").unwrap();
        fs::write(&fc_path, b"legacy-chunks").unwrap();

        let index_only = generation_paths(
            &idx_path,
            "11111111111111111111111111111111-11111111-1111111111111111",
        )
        .unwrap();
        fs::write(&index_only.index_path, b"index-only").unwrap();

        let chunks_only = generation_paths(
            &idx_path,
            "22222222222222222222222222222222-22222222-2222222222222222",
        )
        .unwrap();
        fs::write(&chunks_only.file_chunks_path, b"chunks-only").unwrap();

        let both = generation_paths(
            &idx_path,
            "33333333333333333333333333333333-33333333-3333333333333333",
        )
        .unwrap();
        fs::write(&both.index_path, b"index").unwrap();
        fs::write(&both.file_chunks_path, b"chunks").unwrap();

        let removed = cleanup_abandoned_unpublished_generations(&idx_path, Duration::ZERO).unwrap();

        assert_eq!(removed.len(), 4);
        assert!(!index_only.index_path.exists());
        assert!(!chunks_only.file_chunks_path.exists());
        assert!(!both.index_path.exists());
        assert!(!both.file_chunks_path.exists());
        assert!(idx_path.exists(), "legacy index must never be swept");
        assert!(fc_path.exists(), "legacy chunks must never be swept");
    }

    #[test]
    fn abandoned_unpublished_generation_sweep_preserves_young_publication() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("young.usearch");
        let young = generation_paths(
            &idx_path,
            "44444444444444444444444444444444-44444444-4444444444444444",
        )
        .unwrap();
        fs::write(&young.index_path, b"in-progress-index").unwrap();
        fs::write(&young.file_chunks_path, b"in-progress-chunks").unwrap();
        fs::write(
            young.manifest_path.with_extension("json.tmp"),
            b"in-progress-manifest",
        )
        .unwrap();

        let removed =
            cleanup_abandoned_unpublished_generations(&idx_path, VECTOR_ORPHAN_GRACE).unwrap();

        assert!(removed.is_empty());
        assert!(young.index_path.exists());
        assert!(young.file_chunks_path.exists());
        assert!(young.manifest_path.with_extension("json.tmp").exists());
    }

    #[test]
    fn abandoned_unpublished_generation_sweep_preserves_manifested_generation() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("published.usearch");
        let generation = "55555555555555555555555555555555-55555555-5555555555555555";
        let published = write_test_manifest(&idx_path, generation, 0);
        fs::write(&published.index_path, b"published-index").unwrap();
        fs::write(&published.file_chunks_path, b"published-chunks").unwrap();

        let removed = cleanup_abandoned_unpublished_generations(&idx_path, Duration::ZERO).unwrap();

        assert!(removed.is_empty());
        assert!(published.manifest_path.exists());
        assert!(published.index_path.exists());
        assert!(published.file_chunks_path.exists());
    }

    #[test]
    fn abandoned_sweep_treats_any_final_manifest_entry_as_protected() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("manifest-entry.usearch");
        let generation = "66666666666666666666666666666666-66666666-6666666666666666";
        let artifacts = generation_paths(&idx_path, generation).unwrap();
        fs::write(&artifacts.index_path, b"index").unwrap();
        fs::write(&artifacts.file_chunks_path, b"chunks").unwrap();
        fs::create_dir(&artifacts.manifest_path).unwrap();

        let removed = cleanup_abandoned_unpublished_generations(&idx_path, Duration::ZERO).unwrap();

        assert!(removed.is_empty());
        assert!(artifacts.index_path.exists());
        assert!(artifacts.file_chunks_path.exists());
        assert!(artifacts.manifest_path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn abandoned_sweep_never_follows_symlink_or_invalid_artifact_names() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("symlink-safe.usearch");
        let victim = dir.path().join("external-victim.bin");
        fs::write(&victim, b"keep").unwrap();

        let generation = "77777777777777777777777777777777-77777777-7777777777777777";
        let artifacts = generation_paths(&idx_path, generation).unwrap();
        symlink(&victim, &artifacts.index_path).unwrap();
        fs::write(&artifacts.file_chunks_path, b"old chunks").unwrap();

        let invalid = dir
            .path()
            .join("symlink-safe.usearch.generation-not-a-valid-generation");
        fs::write(&invalid, b"unrelated").unwrap();

        let removed = cleanup_abandoned_unpublished_generations(&idx_path, Duration::ZERO).unwrap();

        assert!(removed.is_empty());
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(artifacts.index_path.is_symlink());
        assert!(artifacts.file_chunks_path.exists());
        assert!(invalid.exists());
    }

    #[test]
    fn active_raw_publisher_blocks_explicit_orphan_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("maintenance-lock.usearch");
        let fc_path = dir.path().join("maintenance-lock-file-chunks.bin");
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let publisher_idx_path = idx_path.clone();
        let publisher_fc_path = fc_path.clone();

        let publisher = std::thread::spawn(move || {
            publish_generation(
                &publisher_idx_path,
                &publisher_fc_path,
                &HashMap::new(),
                0,
                |path| {
                    write_new_file(path, b"paused publisher")?;
                    paused_tx.send(path.to_path_buf()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(())
                },
            )
        });

        let unpublished_index = paused_rx.recv().unwrap();
        let error = cleanup_abandoned_unpublished_generations(&idx_path, Duration::ZERO)
            .expect_err("active publisher must make maintenance fail closed");
        assert!(error.to_string().contains("publication is active"));
        assert!(unpublished_index.exists());

        resume_tx.send(()).unwrap();
        publisher.join().unwrap().unwrap();
    }

    #[test]
    fn store_publication_coordinates_legacy_and_generational_paths() {
        let dir = tempfile::tempdir().unwrap();
        let control_dir = dir.path().join(CODIXING_CONTROL_DIR_NAME);
        let generation_dir = control_dir
            .join(INDEX_GENERATIONS_DIR_NAME)
            .join("generation-a");
        let generation_vectors = generation_dir.join(VECTOR_ARTIFACT_DIR_NAME);
        let legacy_vectors = control_dir.join(VECTOR_ARTIFACT_DIR_NAME);
        fs::create_dir_all(&generation_vectors).unwrap();
        fs::create_dir(&legacy_vectors).unwrap();

        let generation_index = generation_vectors.join("usearch.bin");
        let generation_chunks = generation_vectors.join("file_chunks.bin");
        let legacy_index = legacy_vectors.join("usearch.bin");
        assert_eq!(
            publication_lock_path(&generation_index).unwrap(),
            control_dir.join(VECTOR_PUBLICATION_LOCK_FILE)
        );
        assert_eq!(
            publication_lock_path(&legacy_index).unwrap(),
            control_dir.join(VECTOR_PUBLICATION_LOCK_FILE)
        );

        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let publisher = std::thread::spawn(move || {
            publish_generation(
                &generation_index,
                &generation_chunks,
                &HashMap::new(),
                0,
                |path| {
                    write_new_file(path, b"paused store publisher")?;
                    paused_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(())
                },
            )
        });

        paused_rx.recv().unwrap();
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(generation_dir.join(INDEX_GENERATION_LEASE_FILE))
            .unwrap();
        assert!(
            !FileExt::try_lock_exclusive(&lease).unwrap(),
            "a store vector publication must keep its outer generation leased"
        );
        let error = cleanup_abandoned_unpublished_generations(&legacy_index, Duration::ZERO)
            .expect_err("legacy maintenance must see a generational publisher");
        assert!(error.to_string().contains("publication is active"));
        assert!(
            !generation_vectors
                .join("usearch.bin.publication.lock")
                .exists()
        );

        resume_tx.send(()).unwrap();
        publisher.join().unwrap().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn store_publication_refuses_symlinked_stable_lock() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let control_dir = dir.path().join(CODIXING_CONTROL_DIR_NAME);
        let vectors_dir = control_dir
            .join(INDEX_GENERATIONS_DIR_NAME)
            .join("generation-b")
            .join(VECTOR_ARTIFACT_DIR_NAME);
        fs::create_dir_all(&vectors_dir).unwrap();
        let victim = dir.path().join("external-lock-victim");
        fs::write(&victim, b"unchanged").unwrap();
        symlink(&victim, control_dir.join(VECTOR_PUBLICATION_LOCK_FILE)).unwrap();

        let error = publish_generation(
            &vectors_dir.join("usearch.bin"),
            &vectors_dir.join("file_chunks.bin"),
            &HashMap::new(),
            0,
            |path| write_new_file(path, b"must not be written"),
        )
        .expect_err("a symlinked stable publication lock must fail closed");

        assert!(error.to_string().contains("lock is not a real file"));
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
        assert!(
            manifest_paths(&vectors_dir.join("usearch.bin"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paused_old_publisher_is_not_deleted_by_automatic_publication() {
        use filetime::{FileTime, set_file_mtime};

        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("concurrent.usearch");
        let fc_path = dir.path().join("concurrent_file_chunks.bin");
        let empty_file_chunks = HashMap::new();

        // Give both publishers the same already-published predecessor. Each
        // may delete this seed generation, but neither may delete the other's
        // in-progress generation.
        publish_generation(&idx_path, &fc_path, &empty_file_chunks, 0, |path| {
            write_new_file(path, b"seed")
        })
        .unwrap();

        let (index_written_tx, index_written_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let publisher_idx_path = idx_path.clone();
        let publisher_fc_path = fc_path.clone();
        let publisher = std::thread::spawn(move || {
            let empty_file_chunks = HashMap::new();
            publish_generation(
                &publisher_idx_path,
                &publisher_fc_path,
                &empty_file_chunks,
                0,
                |path| {
                    write_new_file(path, b"publisher-a")?;
                    set_file_mtime(path, FileTime::from_unix_time(0, 0))?;
                    index_written_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(())
                },
            )
        });

        // Publisher A has written an index old enough to exceed any age-based
        // grace period, but has not yet published a manifest. Publisher B must
        // complete without treating that paused publisher's index as an orphan.
        index_written_rx.recv().unwrap();
        publish_generation(&idx_path, &fc_path, &empty_file_chunks, 0, |path| {
            write_new_file(path, b"publisher-b")
        })
        .unwrap();
        resume_tx.send(()).unwrap();
        publisher.join().unwrap().unwrap();

        let manifests = manifest_paths(&idx_path).unwrap();
        assert_eq!(manifests.len(), 2);
        for manifest_path in manifests {
            let artifacts = resolve_manifest(&idx_path, &manifest_path).unwrap();
            assert!(artifacts.index_path.is_file());
            assert!(artifacts.file_chunks_path.is_file());
        }
    }

    /// Verify 384d vectors (BgeSmallEn) work with and without quantization.
    #[test]
    fn dims_384_f32_and_quantized() {
        for quantize in [false, true] {
            let mut idx = VectorIndex::new(384, quantize).unwrap();
            let a = unit_vec(384, 0);
            let b = unit_vec(384, 100);
            idx.add_mut(1, &a, "a.rs").unwrap();
            idx.add_mut(2, &b, "b.rs").unwrap();
            assert_eq!(idx.len(), 2, "quantize={quantize}: expected 2 vectors");
            let results = idx.search(&a, 2).unwrap();
            assert_eq!(
                results[0].0, 1,
                "quantize={quantize}: nearest should be chunk 1"
            );
        }
    }

    /// Verify 768d vectors (BgeBaseEn) work with and without quantization.
    #[test]
    fn dims_768_f32_and_quantized() {
        for quantize in [false, true] {
            let mut idx = VectorIndex::new(768, quantize).unwrap();
            let a = unit_vec(768, 0);
            let b = unit_vec(768, 500);
            idx.add_mut(1, &a, "a.rs").unwrap();
            idx.add_mut(2, &b, "b.rs").unwrap();
            assert_eq!(idx.len(), 2, "quantize={quantize}: expected 2 vectors");
            let results = idx.search(&a, 2).unwrap();
            assert_eq!(
                results[0].0, 1,
                "quantize={quantize}: nearest should be chunk 1"
            );
        }
    }

    /// Verify 1024d vectors (BgeLargeEn / SnowflakeArctic / Qwen3) work.
    ///
    /// This is a regression test for the 1024d vector index bug where adds
    /// silently failed for high-dimension vectors.
    #[test]
    fn dims_1024_f32_and_quantized() {
        for quantize in [false, true] {
            let mut idx = VectorIndex::new(1024, quantize).unwrap();
            let a = unit_vec(1024, 0);
            let b = unit_vec(1024, 512);
            let c = unit_vec(1024, 1023);
            idx.add_mut(1, &a, "a.rs").unwrap();
            idx.add_mut(2, &b, "b.rs").unwrap();
            idx.add_mut(3, &c, "c.rs").unwrap();
            assert_eq!(idx.len(), 3, "quantize={quantize}: expected 3 vectors");
            let results = idx.search(&a, 3).unwrap();
            assert_eq!(
                results[0].0, 1,
                "quantize={quantize}: nearest should be chunk 1"
            );
        }
    }

    /// Verify 1024d save/load round-trip works correctly.
    #[test]
    fn dims_1024_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let idx_path = dir.path().join("test_1024.usearch");
        let fc_path = dir.path().join("fc_1024.bin");

        let mut idx = VectorIndex::new(1024, true).unwrap();
        for i in 0..50u64 {
            let mut v = vec![0.01f32; 1024];
            v[(i as usize) % 1024] = 1.0;
            idx.add_mut(i, &v, &format!("file_{i}.rs")).unwrap();
        }
        assert_eq!(idx.len(), 50);
        idx.save(&idx_path, &fc_path).unwrap();

        let loaded = VectorIndex::load(&idx_path, &fc_path, 1024, true).unwrap();
        assert_eq!(loaded.len(), 50);
        let results = loaded.search(&unit_vec(1024, 0), 5).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0); // chunk 0 has dominant dim 0
    }

    #[test]
    fn get_vector_retrieves_stored() {
        let mut idx = VectorIndex::new(4, false).unwrap();
        let a = unit_vec(4, 0);
        idx.add_mut(42, &a, "a.rs").unwrap();

        let retrieved = idx.get_vector(42).expect("vector should exist");
        // Cosine distance is used, so the retrieved vector should be very close.
        for (orig, got) in a.iter().zip(retrieved.iter()) {
            assert!(
                (orig - got).abs() < 0.01,
                "vector mismatch: orig={orig} got={got}"
            );
        }

        // Non-existent chunk should return None.
        assert!(idx.get_vector(999).is_none());
    }

    // -----------------------------------------------------------------------
    // Brute-force-specific tests (run on both backends to ensure parity)
    // -----------------------------------------------------------------------

    #[test]
    fn brute_force_search_returns_nearest() {
        let mut idx = VectorIndex::new(3, false).unwrap();
        idx.add_mut(1, &[1.0, 0.0, 0.0], "a.rs").unwrap();
        idx.add_mut(2, &[0.0, 1.0, 0.0], "b.rs").unwrap();
        idx.add_mut(3, &[0.9, 0.1, 0.0], "c.rs").unwrap();

        let results = idx.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(results.len(), 2);
        // Chunk 1 is an exact match, should be first (lowest distance).
        assert_eq!(results[0].0, 1);
        assert!(
            results[0].1 < 0.01,
            "exact match should have near-zero distance"
        );
    }

    #[test]
    fn brute_force_add_remove() {
        let mut idx = VectorIndex::new(3, false).unwrap();
        idx.add_mut(10, &[1.0, 0.0, 0.0], "a.rs").unwrap();
        idx.add_mut(20, &[0.0, 1.0, 0.0], "a.rs").unwrap();
        idx.add_mut(30, &[0.0, 0.0, 1.0], "b.rs").unwrap();
        assert_eq!(idx.len(), 3);

        idx.remove_file("a.rs").unwrap();
        assert_eq!(idx.len(), 1);

        let results = idx.search(&[1.0, 0.0, 0.0], 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 30);
    }

    #[test]
    fn brute_force_empty_search() {
        let idx = VectorIndex::new(4, false).unwrap();
        let results = idx.search(&[1.0, 0.0, 0.0, 0.0], 10).unwrap();
        assert!(results.is_empty());
    }
}
