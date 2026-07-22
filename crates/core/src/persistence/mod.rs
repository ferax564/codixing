use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

use crate::config::IndexConfig;
use crate::error::{CodixingError, Result};
use crate::graph::GraphData;

/// Process-global counter making atomic-write temp filenames unique across threads.
static ATOMIC_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the repository-wide writer lease for doctor and lock UX.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriterLockStatus {
    pub path: String,
    pub held: bool,
    pub pid: Option<u32>,
    pub exe: Option<String>,
    pub pid_alive: Option<bool>,
    pub detail: Option<String>,
}

fn write_writer_lock_identity(lock: &fs::File) {
    use std::io::{Seek, SeekFrom};
    let pid = std::process::id();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let body = format!("{pid}\n{exe}\n");
    // Best-effort identity only — exclusive lock is the real ownership signal.
    let _ = (|| -> std::io::Result<()> {
        let mut file = lock.try_clone()?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(body.as_bytes())?;
        file.sync_data()?;
        Ok(())
    })();
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // signal 0 checks existence without delivering a signal.
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            return true;
        }
        // EPERM means the process exists but we cannot signal it.
        let err = std::io::Error::last_os_error();
        err.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output();
        match output {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                text.contains(&pid.to_string())
            }
            Err(_) => false,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Process-global counter making generation names unique when multiple rebuilds
/// start within the same clock tick.
static GENERATION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically write `contents` to `path`: write a sibling temp file, fsync it,
/// then rename it over the destination. A crash, SIGKILL, OOM, or power loss
/// mid-write leaves either the previous file or the complete new file intact —
/// never a truncated or zero-length one. The rename is an atomic same-filesystem
/// replace on both Unix and Windows.
///
/// This replaces the bare `fs::write` (truncate-then-write) previously used by
/// every `save_*` helper, which could corrupt `config.json` / `meta.json` /
/// `graph.bin` / `symbols.bin` into an unloadable state on an ill-timed crash.
pub(crate) fn atomic_write(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    use std::io::Write;
    atomic_write_with_parent_sync(path, ParentDirectorySync::Immediate, |file| {
        file.write_all(contents.as_ref())
    })
}

/// Atomically replace `path` while producing the contents incrementally.
///
/// This is the streaming counterpart to [`atomic_write`]: callers can write
/// large artifacts without first assembling a second corpus-sized byte
/// buffer. The temporary file is synced before rename and the containing
/// directory is synced afterwards, preserving the same durability contract.
pub(crate) fn atomic_write_with(
    path: impl AsRef<Path>,
    write: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    atomic_write_with_parent_sync(path, ParentDirectorySync::Immediate, write)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParentDirectorySync {
    Immediate,
    PublicationBarrier,
}

fn atomic_write_with_parent_sync(
    path: impl AsRef<Path>,
    parent_sync: ParentDirectorySync,
    write: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{stem}.tmp.{}.{seq}", std::process::id()));
    let write_result = (|| {
        let mut f = fs::File::create(&tmp)?;
        write(&mut f)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    match fs::rename(&tmp, path) {
        Ok(()) => {
            if parent_sync == ParentDirectorySync::Immediate {
                sync_directory(dir)?;
            }
            Ok(())
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Atomic write with parent-directory durability for transaction markers.
/// `atomic_write` now provides the same guarantee for every artifact; retain
/// this named helper so the mutation-journal call sites state their intent.
fn atomic_write_durable(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    atomic_write(path, contents)
}

/// Persist a directory entry after an atomic rename. Directory fsync is
/// supported on Unix; on Windows the file fsync plus atomic rename is the
/// strongest portable guarantee available through `std`.
#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Validate an unpublished generation and persist its directory entries.
///
/// Artifact writers are responsible for syncing every new or modified file
/// before returning: Codixing sidecars file-sync before atomic rename, Tantivy
/// commits sync their segment/control files, and checkpoint fallback copies
/// are synced by [`copy_checkpoint_file`]. Files inherited through hard links
/// already belong to the durable active generation. Re-syncing every one of
/// those immutable inodes here made a one-file checkpoint scale with total
/// index size without adding crash safety. The publication barrier therefore
/// fsyncs directories only, including the parent entries intentionally batched
/// by unpublished-generation sidecar writes, after recursively rejecting
/// symlinks and special files.
fn sync_publication_directories(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "refusing to publish symlinked index artifact: {}",
                path.display()
            ),
        ));
    }
    if metadata.is_file() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            sync_publication_directories(&entry?.path())?;
        }
        return sync_directory(path);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "refusing to publish unsupported index artifact: {}",
            path.display()
        ),
    ))
}

fn remove_file_durable(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_directory(path.parent().unwrap_or_else(|| Path::new("."))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Directory name for the Codixing index store.
const CODEFORGE_DIR: &str = ".codixing";
const CONFIG_FILE: &str = "config.json";
const META_FILE: &str = "meta.json";
const TANTIVY_DIR: &str = "tantivy";
const SYMBOLS_FILE: &str = "symbols.bin";
const TREE_HASHES_FILE: &str = "tree_hashes.bin";
const TREE_HASHES_V2_FILE: &str = "tree_hashes_v2.bin";
/// Tiny write-ahead journal of paths whose index artifacts are mid-mutation.
/// It is intentionally separate from the O(repo-files) hash snapshot so an
/// editor save does not rewrite that snapshot twice just for crash intent.
const DIRTY_PATHS_FILE: &str = "dirty_paths.bin";
/// Small overlay of successfully published incremental mutations. Full sync
/// folds it into `tree_hashes_v2.bin`; watcher edits avoid rewriting the
/// repository-sized baseline on every save.
const TREE_HASH_DELTA_FILE: &str = "tree_hash_delta.bin";
const VECTORS_DIR: &str = "vectors";
const VECTOR_INDEX_FILE: &str = "index.usearch";
const FILE_CHUNKS_FILE: &str = "file_chunks.bin";
const CHUNK_META_FILE: &str = "chunk_meta.bin";
const GRAPH_DIR: &str = "graph";
const GRAPH_FILE: &str = "graph.bin";
const SCHEMA_VERSION_FILE: &str = "schema.version";
const MMAP_VECTOR_FILE: &str = "vectors.mmap";
const FILE_TRIGRAM_FILE: &str = "file_trigram.bin";
const FILE_TRIGRAM_DELTA_FILE: &str = "file_trigram_delta.bin";
const CHUNK_TRIGRAM_FILE: &str = "chunk_trigram.bin";
const SYMBOLS_V2_FILE: &str = "symbols_v2.bin";
const SYMBOLS_DELTA_FILE: &str = "symbols_delta.bin";
const CONCEPTS_FILE: &str = "concepts.bin";
const REFORMULATIONS_FILE: &str = "reformulations.bin";
const GENERATIONS_DIR: &str = "generations";
const ACTIVE_GENERATION_FILE: &str = "active-generation.json";
const GENERATION_PREFIX: &str = "gen-";
const GENERATION_LAYOUT_VERSION: u32 = 1;
const REBUILD_LOCK_FILE: &str = "rebuild.lock";
const WRITER_LOCK_FILE: &str = "writer.lock";
const TANTIVY_MUTABLE_CONTROL_FILES: [&str; 2] = ["meta.json", ".managed.json"];
/// Hard links are the normal checkpoint COW mechanism. If an unusual
/// filesystem rejects them, bound the aggregate fallback copy so a tiny
/// metadata checkpoint can never silently duplicate a multi-GB index.
const MAX_CHECKPOINT_FALLBACK_COPY_BYTES: u64 = 64 * 1024 * 1024;
const GENERATION_LEASE_FILE: &str = "generation.lease";
/// Sidecar file mapping each indexed file to its signature fingerprint (a stable
/// hash over symbol signatures / imports / exports). Stored separately from
/// `tree_hashes_v2.bin` so the existing `FileHashEntry` bitcode layout is never
/// touched — old indexes simply lack this file and are treated as STRUCTURAL on
/// the first sync. See `engine::fingerprint` and `Engine::sync`.
const TREE_SIGNATURES_FILE: &str = "tree_signatures.bin";
const TREE_SIGNATURES_LOCK_FILE: &str = "tree_signatures.lock";

struct TreeSignaturesLock {
    path: PathBuf,
}

impl Drop for TreeSignaturesLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Extended file hash entry storing content hash alongside filesystem metadata
/// (mtime and size) for fast pre-filtering during sync.
///
/// During `sync()`, if both `mtime` and `size` match the cached values, the
/// file is assumed unchanged and the expensive xxh3 content hash is skipped.
/// This eliminates ~95% of file reads on a typical sync where few files changed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileHashEntry {
    /// xxh3-64 hash of the file contents.
    pub content_hash: u64,
    /// Last modification time (seconds since UNIX epoch + nanos).
    /// Stored as `(secs, nanos)` for bitcode compatibility since `SystemTime`
    /// is not directly serializable.
    pub mtime_secs: u64,
    pub mtime_nanos: u32,
    /// File size in bytes.
    pub size: u64,
}

impl FileHashEntry {
    /// Create a new entry from a content hash and filesystem metadata.
    pub fn new(content_hash: u64, mtime: Option<SystemTime>, size: u64) -> Self {
        let (secs, nanos) = mtime
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| (d.as_secs(), d.subsec_nanos()))
            .unwrap_or((0, 0));
        Self {
            content_hash,
            mtime_secs: secs,
            mtime_nanos: nanos,
            size,
        }
    }

    /// Reconstruct the `SystemTime` from stored seconds/nanos.
    pub fn mtime(&self) -> Option<SystemTime> {
        if self.mtime_secs == 0 && self.mtime_nanos == 0 {
            return None;
        }
        Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(self.mtime_secs, self.mtime_nanos))
    }

    /// Quick check: does the file's current mtime+size match cached values?
    /// Returns `true` if the file might have changed (needs content hash).
    pub fn file_might_have_changed(
        &self,
        current_mtime: Option<SystemTime>,
        current_size: u64,
    ) -> bool {
        if current_size != self.size {
            return true;
        }
        match (self.mtime(), current_mtime) {
            (Some(cached), Some(current)) => cached != current,
            _ => true, // if we can't compare, assume changed
        }
    }
}

/// Index metadata persisted alongside the index.
///
/// Tracks version, counts, and the last indexing timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexMeta {
    /// Version of the index format.
    pub version: String,
    /// Number of files indexed.
    pub file_count: usize,
    /// Number of code chunks produced.
    pub chunk_count: usize,
    /// Number of symbols extracted.
    pub symbol_count: usize,
    /// ISO 8601 timestamp of the last indexing run.
    pub last_indexed: String,
    /// Git commit hash recorded at last index build/sync.
    ///
    /// `None` if git is unavailable or the project is not in a git repo.
    /// Used by [`Engine::git_sync`] to compute the minimal diff since the
    /// last indexed commit, enabling sub-second re-opens after `git pull`.
    #[serde(default)]
    pub git_commit: Option<String>,
}

impl Default for IndexMeta {
    fn default() -> Self {
        Self {
            version: "0.1.0".to_string(),
            file_count: 0,
            chunk_count: 0,
            symbol_count: 0,
            last_indexed: String::new(),
            git_commit: None,
        }
    }
}

/// Atomically-swapped pointer to the complete index generation readers should
/// open. Keeping this tiny pointer separate from the generated data makes a
/// rebuild an all-or-nothing operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct GenerationManifest {
    layout_version: u32,
    active: String,
    /// Distinguishes generations published with the mandatory trigram
    /// base+delta pair from older manifests where a base-only artifact is a
    /// valid legacy representation.
    #[serde(default)]
    file_trigram_delta_required: bool,
}

fn new_generation_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = GENERATION_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{GENERATION_PREFIX}{nanos}-{}-{seq}", std::process::id())
}

fn validate_generation_name(name: &str) -> Result<()> {
    let suffix = name
        .strip_prefix(GENERATION_PREFIX)
        .ok_or_else(|| CodixingError::Config(format!("invalid index generation name: {name:?}")))?;
    if suffix.is_empty()
        || name.len() > 128
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(CodixingError::Config(format!(
            "invalid index generation name: {name:?}"
        )));
    }
    Ok(())
}

fn read_generation_manifest(control_dir: &Path) -> Result<Option<GenerationManifest>> {
    let path = control_dir.join(ACTIVE_GENERATION_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    let manifest: GenerationManifest = serde_json::from_slice(&bytes).map_err(|e| {
        CodixingError::Serialization(format!(
            "failed to deserialize active generation manifest {}: {e}",
            path.display()
        ))
    })?;
    if manifest.layout_version != GENERATION_LAYOUT_VERSION {
        return Err(CodixingError::Config(format!(
            "unsupported index generation layout version {} in {}",
            manifest.layout_version,
            path.display()
        )));
    }
    validate_generation_name(&manifest.active)?;
    Ok(Some(manifest))
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CodixingError::Config(format!(
            "{label} {} is unavailable: {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(CodixingError::Config(format!(
            "{label} must be a real directory, not a symlink: {}",
            path.display()
        )));
    }
    Ok(())
}

fn resolve_index_dir(root: &Path) -> Result<(PathBuf, Option<String>, bool)> {
    let control_dir = root.join(CODEFORGE_DIR);
    require_real_directory(&control_dir, "index control directory")?;
    let Some(manifest) = read_generation_manifest(&control_dir)? else {
        return Ok((control_dir, None, false));
    };
    let index_dir = control_dir.join(GENERATIONS_DIR).join(&manifest.active);
    let metadata = fs::symlink_metadata(&index_dir).map_err(|e| {
        CodixingError::Config(format!(
            "active index generation {} is unavailable: {e}",
            index_dir.display()
        ))
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(CodixingError::Config(format!(
            "active index generation is not a real directory: {}",
            index_dir.display()
        )));
    }
    Ok((
        index_dir,
        Some(manifest.active),
        manifest.file_trigram_delta_required,
    ))
}

fn remove_generation_if_safe(control_dir: &Path, generation: &str) {
    if validate_generation_name(generation).is_err() {
        return;
    }
    let generations_dir = control_dir.join(GENERATIONS_DIR);
    if require_real_directory(&generations_dir, "index generations directory").is_err() {
        return;
    }
    let path = generations_dir.join(generation);
    if path.parent() != Some(generations_dir.as_path()) {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return;
    };
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        let lease_path = path.join(GENERATION_LEASE_FILE);
        let Ok(lease) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lease_path)
        else {
            return;
        };
        // Every open engine holds a shared lease. An inactive generation is
        // deleted only after all of those readers have gone away; otherwise it
        // remains fully searchable and a later cleanup retries.
        if !FileExt::try_lock_exclusive(&lease).unwrap_or(false) {
            return;
        }
        drop(lease);
        if let Err(error) = fs::remove_dir_all(&path) {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not remove inactive index generation"
            );
        }
    }
}

fn acquire_generation_lease(index_dir: &Path) -> Result<fs::File> {
    // Readers only need an existing handle for a shared advisory lock. Opening
    // read-only keeps `Engine::open_read_only` usable on immutable mounts and
    // ensures merely observing an index never creates a lease artifact.
    let lease = fs::OpenOptions::new()
        .read(true)
        .open(index_dir.join(GENERATION_LEASE_FILE))?;
    FileExt::lock_shared(&lease)?;
    Ok(lease)
}

fn acquire_or_create_generation_lease(index_dir: &Path) -> Result<fs::File> {
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(index_dir.join(GENERATION_LEASE_FILE))?;
    FileExt::lock_shared(&lease)?;
    Ok(lease)
}

fn cleanup_abandoned_generations(control_dir: &Path, grace: std::time::Duration) {
    let generations_dir = control_dir.join(GENERATIONS_DIR);
    let active = match read_generation_manifest(control_dir) {
        Ok(manifest) => manifest.map(|manifest| manifest.active),
        Err(error) => {
            // If the pointer is unreadable, no generation can be proven
            // inactive. Leave every directory in place; a subsequent complete
            // publication can safely supersede the broken pointer.
            tracing::warn!(
                error = %error,
                "skipping abandoned-generation cleanup because the active manifest is unreadable"
            );
            return;
        }
    };
    let Ok(entries) = fs::read_dir(&generations_dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if active.as_deref() == Some(name.as_str()) || validate_generation_name(&name).is_err() {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= grace);
        if old_enough {
            remove_generation_if_safe(control_dir, &name);
        }
    }
}

fn cleanup_legacy_artifacts(control_dir: &Path) {
    const LEGACY_FILES: &[&str] = &[
        CONFIG_FILE,
        META_FILE,
        SYMBOLS_FILE,
        SYMBOLS_V2_FILE,
        SYMBOLS_DELTA_FILE,
        FILE_TRIGRAM_DELTA_FILE,
        TREE_HASHES_FILE,
        TREE_HASHES_V2_FILE,
        TREE_SIGNATURES_FILE,
        TREE_SIGNATURES_LOCK_FILE,
        CHUNK_META_FILE,
        MMAP_VECTOR_FILE,
        FILE_TRIGRAM_FILE,
        CHUNK_TRIGRAM_FILE,
        CONCEPTS_FILE,
        REFORMULATIONS_FILE,
    ];
    const LEGACY_DIRS: &[&str] = &[TANTIVY_DIR, VECTORS_DIR, GRAPH_DIR];

    // A legacy index opened by this Codixing version is protected exactly like
    // a generation. Older binaries do not know about leases, so cleanup remains
    // best-effort for readers from before the migration feature existed.
    let Ok(exclusive) = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(control_dir.join(GENERATION_LEASE_FILE))
    else {
        return;
    };
    if !FileExt::try_lock_exclusive(&exclusive).unwrap_or(false) {
        return;
    }

    for name in LEGACY_FILES {
        let path = control_dir.join(name);
        if path.parent() == Some(control_dir) {
            let _ = fs::remove_file(path);
        }
    }
    for name in LEGACY_DIRS {
        let path = control_dir.join(name);
        if path.parent() != Some(control_dir) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let result = if metadata.file_type().is_symlink() {
            fs::remove_file(&path)
        } else if metadata.file_type().is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if let Err(error) = result {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not remove migrated legacy index artifact"
            );
        }
    }
}

/// Manages the `.codixing/` directory layout on disk.
///
/// Provides creation, opening, and persistence of the index configuration,
/// metadata, symbol tables, and tree hashes.
#[derive(Debug)]
pub struct IndexStore {
    root: PathBuf,
    index_dir: PathBuf,
    generation: Option<String>,
    file_trigram_delta_required: bool,
    defer_artifact_directory_sync: bool,
    rebuild_lock: Option<fs::File>,
    generation_lease: Option<fs::File>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MutationJournal {
    version: u32,
    base_generation: Option<String>,
    working_generation: Option<String>,
    paths: Vec<PathBuf>,
    /// Paths that still need replay if `working_generation` becomes active.
    /// Before the outcome is known this conservatively contains every path.
    retry_after_publish: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MutationJournalV1 {
    version: u32,
    base_generation: Option<String>,
    working_generation: Option<String>,
    paths: Vec<PathBuf>,
}

const MUTATION_JOURNAL_VERSION: u32 = 2;

fn decode_mutation_journal(bytes: &[u8]) -> Result<MutationJournal> {
    if let Ok(journal) = bitcode::deserialize::<MutationJournal>(bytes) {
        if journal.version != MUTATION_JOURNAL_VERSION {
            return Err(CodixingError::Serialization(format!(
                "unsupported mutation journal version {}",
                journal.version
            )));
        }
        return Ok(journal);
    }
    if let Ok(journal) = bitcode::deserialize::<MutationJournalV1>(bytes)
        && journal.version == 1
    {
        // Version 1 could not distinguish a crash before publication from one
        // after publication. Replaying all paths is conservative and prevents
        // an upgrade from losing a failed mutation.
        return Ok(MutationJournal {
            version: MUTATION_JOURNAL_VERSION,
            base_generation: journal.base_generation,
            working_generation: journal.working_generation,
            retry_after_publish: journal.paths.clone(),
            paths: journal.paths,
        });
    }
    if let Ok(paths) = bitcode::deserialize::<Vec<PathBuf>>(bytes) {
        return Ok(MutationJournal {
            version: MUTATION_JOURNAL_VERSION,
            base_generation: None,
            working_generation: None,
            retry_after_publish: paths.clone(),
            paths,
        });
    }
    Err(CodixingError::Serialization(
        "failed to deserialize mutation journal".to_string(),
    ))
}

fn clone_tree_copy_on_write(
    source: &Path,
    destination: &Path,
    legacy_root: bool,
    fallback_copy_bytes: &mut u64,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".lock")
            || name_str == GENERATION_LEASE_FILE
            || (legacy_root
                && (name_str == GENERATIONS_DIR
                    || name_str == ACTIVE_GENERATION_FILE
                    || name_str == DIRTY_PATHS_FILE))
        {
            continue;
        }

        let source_path = entry.path();
        let destination_path = destination.join(&name);
        let metadata = fs::symlink_metadata(&source_path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(CodixingError::Config(format!(
                "refusing to clone symlinked index artifact: {}",
                source_path.display()
            )));
        }
        if file_type.is_dir() {
            fs::create_dir(&destination_path)?;
            clone_tree_copy_on_write(&source_path, &destination_path, false, fallback_copy_bytes)?;
        } else if file_type.is_file() {
            // Tantivy 0.25 replaces these controls atomically, creates segment
            // files with `create_new`, and removes segments by unlinking. Copy
            // the controls anyway so a future truncating writer cannot reach
            // A through a shared inode; immutable segments remain hard-linked.
            let tantivy_control = source
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == TANTIVY_DIR)
                && TANTIVY_MUTABLE_CONTROL_FILES
                    .iter()
                    .any(|control| name_str.as_ref() == *control);
            // Windows cannot atomically replace a hard-link alias while the
            // active generation's mmap is live. Detach the two fixed-name
            // mmap artifacts before a checkpoint can rewrite them; immutable
            // files and Unix rename semantics still retain cheap hard links.
            let windows_mapped_rewrite_target =
                cfg!(windows) && matches!(name_str.as_ref(), FILE_TRIGRAM_FILE | SYMBOLS_V2_FILE);
            if windows_mapped_rewrite_target {
                // This copy is mandatory even when hard links work. It is not
                // fallback amplification, so large resident sidecars must not
                // be rejected by the hard-link-failure safety budget.
                copy_checkpoint_file(
                    &source_path,
                    &destination_path,
                    metadata.len(),
                    fallback_copy_bytes,
                    true,
                )?;
            } else if tantivy_control || fs::hard_link(&source_path, &destination_path).is_err() {
                copy_checkpoint_file(
                    &source_path,
                    &destination_path,
                    metadata.len(),
                    fallback_copy_bytes,
                    false,
                )?;
            }
        } else {
            return Err(CodixingError::Config(format!(
                "unsupported index artifact type: {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

fn copy_checkpoint_file(
    source: &Path,
    destination: &Path,
    bytes: u64,
    fallback_copy_bytes: &mut u64,
    required_detachment: bool,
) -> Result<()> {
    let copied = checkpoint_fallback_copy_total(*fallback_copy_bytes, bytes, required_detachment)?;
    fs::copy(source, destination)?;
    // Unlike a hard link, a fallback copy creates a new inode whose contents
    // are not durable until explicitly flushed. The directory entry is synced
    // by the publication barrier after the complete generation is validated.
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(destination)?
        .sync_all()?;
    *fallback_copy_bytes = copied;
    Ok(())
}

fn checkpoint_fallback_copy_total(
    current: u64,
    bytes: u64,
    required_detachment: bool,
) -> Result<u64> {
    if required_detachment {
        return Ok(current);
    }
    let copied = current.checked_add(bytes).ok_or_else(|| {
        CodixingError::Config(
            "checkpoint fallback copy size overflowed; run a full rebuild".to_string(),
        )
    })?;
    if copied > MAX_CHECKPOINT_FALLBACK_COPY_BYTES {
        return Err(CodixingError::Config(format!(
            "checkpoint would copy more than {} MiB because hard links are unavailable (run `codixing init` for a fresh generation)",
            MAX_CHECKPOINT_FALLBACK_COPY_BYTES / (1024 * 1024)
        )));
    }
    Ok(copied)
}

/// An extra shared lease retained by work that can outlive its [`Engine`],
/// such as background embedding. When the work finishes, an inactive
/// generation is reclaimed immediately instead of waiting for another init.
#[derive(Debug)]
pub(crate) struct BackgroundGenerationLease {
    control_dir: PathBuf,
    generation: String,
    lease: Option<fs::File>,
}

impl Drop for BackgroundGenerationLease {
    fn drop(&mut self) {
        self.lease.take();
        let inactive = match read_generation_manifest(&self.control_dir) {
            Ok(Some(manifest)) => manifest.active != self.generation,
            Ok(None) => true,
            Err(_) => false,
        };
        if inactive {
            remove_generation_if_safe(&self.control_dir, &self.generation);
        }
    }
}

impl Drop for IndexStore {
    fn drop(&mut self) {
        let Some(generation) = self.generation.clone() else {
            return;
        };
        let should_remove = match read_generation_manifest(&self.control_dir()) {
            Ok(Some(manifest)) => manifest.active != generation,
            Ok(None) => true,
            // An unreadable pointer means no directory can be proven inactive.
            Err(_) => false,
        };
        if should_remove {
            // Normal errors unwind through here and reclaim the unpublished
            // generation while the rebuild lock is still held. A hard process
            // interruption releases the OS lock; the next builder then safely
            // performs the same cleanup.
            self.generation_lease.take();
            remove_generation_if_safe(&self.control_dir(), &generation);
        }
    }
}

impl IndexStore {
    /// Initialize a new `.codixing/` directory structure at the given root.
    ///
    /// Creates the directory layout and writes default config and metadata files.
    /// Returns an error if the directory already exists.
    pub fn init(root: &Path, config: &IndexConfig) -> Result<Self> {
        let codixing_dir = root.join(CODEFORGE_DIR);
        let mut store = Self {
            root: root.to_path_buf(),
            index_dir: codixing_dir,
            generation: None,
            file_trigram_delta_required: false,
            defer_artifact_directory_sync: false,
            rebuild_lock: None,
            generation_lease: None,
        };
        store.initialize_layout(config)?;
        store.generation_lease = Some(acquire_or_create_generation_lease(&store.index_dir)?);
        Ok(store)
    }

    /// Start a clean rebuild in a new, unpublished generation.
    ///
    /// The currently-active index is not modified. Call
    /// [`Self::publish_generation`] only after every artifact has been written;
    /// dropping this store (or crashing) leaves the previous generation active.
    pub fn begin_generation(root: &Path, config: &IndexConfig) -> Result<Self> {
        let control_dir = root.join(CODEFORGE_DIR);
        let generations_dir = control_dir.join(GENERATIONS_DIR);
        fs::create_dir_all(&control_dir)?;
        require_real_directory(&control_dir, "index control directory")?;
        match fs::create_dir(&generations_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        require_real_directory(&generations_dir, "index generations directory")?;

        let rebuild_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(control_dir.join(REBUILD_LOCK_FILE))?;
        FileExt::lock_exclusive(&rebuild_lock)?;

        // The cross-platform OS lock is released automatically on process
        // death. Once acquired, no live builder can own an unpublished
        // generation, so interrupted/abandoned directories are safe to remove.
        cleanup_abandoned_generations(&control_dir, std::time::Duration::ZERO);

        let generation = new_generation_name();
        let index_dir = generations_dir.join(&generation);
        fs::create_dir(&index_dir)?;
        let generation_lease = acquire_or_create_generation_lease(&index_dir)?;

        let store = Self {
            root: root.to_path_buf(),
            index_dir,
            generation: Some(generation),
            file_trigram_delta_required: true,
            defer_artifact_directory_sync: true,
            rebuild_lock: Some(rebuild_lock),
            generation_lease: Some(generation_lease),
        };
        store.initialize_layout(config)?;
        Ok(store)
    }

    /// Acquire the repository-wide writer lease without blocking.
    ///
    /// Tantivy's own lock is generation-local, so it cannot prevent an older
    /// writer from mutating generation A while another process prepares or
    /// publishes generation B. This stable control-directory lock serializes
    /// every writable Engine across generation switches.
    pub(crate) fn try_acquire_writer_lock(root: &Path) -> Result<Option<fs::File>> {
        let control_dir = root.join(CODEFORGE_DIR);
        fs::create_dir_all(&control_dir)?;
        require_real_directory(&control_dir, "index control directory")?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(control_dir.join(WRITER_LOCK_FILE))?;
        if FileExt::try_lock_exclusive(&lock)? {
            write_writer_lock_identity(&lock);
            Ok(Some(lock))
        } else {
            Ok(None)
        }
    }

    /// Acquire the repository-wide writer lease, waiting for the current
    /// writer to finish. Full initialization uses this blocking form.
    pub(crate) fn acquire_writer_lock(root: &Path) -> Result<fs::File> {
        let control_dir = root.join(CODEFORGE_DIR);
        fs::create_dir_all(&control_dir)?;
        require_real_directory(&control_dir, "index control directory")?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(control_dir.join(WRITER_LOCK_FILE))?;
        FileExt::lock_exclusive(&lock)?;
        write_writer_lock_identity(&lock);
        Ok(lock)
    }

    /// Best-effort identity of the process holding `writer.lock`.
    ///
    /// The OS exclusive lock is authoritative; the file body is advisory so
    /// doctor and lock-error messages can name the holder without racing the
    /// lock itself.
    pub fn writer_lock_status(root: &Path) -> WriterLockStatus {
        let path = root.join(CODEFORGE_DIR).join(WRITER_LOCK_FILE);
        if !path.exists() {
            return WriterLockStatus {
                path: path.display().to_string(),
                held: false,
                pid: None,
                exe: None,
                pid_alive: None,
                detail: Some("lock file not present".to_string()),
            };
        }

        let held = match fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => match FileExt::try_lock_exclusive(&file) {
                Ok(true) => {
                    // We briefly acquired the lock — nobody else holds it.
                    let _ = FileExt::unlock(&file);
                    false
                }
                Ok(false) => true,
                Err(error) => {
                    return WriterLockStatus {
                        path: path.display().to_string(),
                        held: true,
                        pid: None,
                        exe: None,
                        pid_alive: None,
                        detail: Some(format!("lock probe failed: {error}")),
                    };
                }
            },
            Err(error) => {
                return WriterLockStatus {
                    path: path.display().to_string(),
                    held: true,
                    pid: None,
                    exe: None,
                    pid_alive: None,
                    detail: Some(format!("cannot open lock file: {error}")),
                };
            }
        };

        let body = fs::read_to_string(&path).unwrap_or_default();
        let mut lines = body.lines();
        let pid = lines
            .next()
            .and_then(|line| line.trim().parse::<u32>().ok());
        let exe = lines
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let pid_alive = pid.map(process_is_alive);
        WriterLockStatus {
            path: path.display().to_string(),
            held,
            pid,
            exe,
            pid_alive,
            detail: None,
        }
    }

    /// Fork the currently loaded snapshot into an unpublished working
    /// generation. Regular files are hard-linked when the filesystem permits,
    /// so the fork is metadata-proportional; all writers replace files
    /// atomically, providing file-level copy-on-write isolation.
    pub(crate) fn begin_checkpoint(&self) -> Result<Self> {
        let control_dir = self.control_dir();
        let generations_dir = control_dir.join(GENERATIONS_DIR);
        fs::create_dir_all(&generations_dir)?;
        require_real_directory(&control_dir, "index control directory")?;
        require_real_directory(&generations_dir, "index generations directory")?;

        let rebuild_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(control_dir.join(REBUILD_LOCK_FILE))?;
        FileExt::lock_exclusive(&rebuild_lock)?;

        let active_generation = Self::active_generation(&self.root)?;
        if active_generation != self.generation {
            return Err(CodixingError::Config(
                "active index generation changed before checkpoint fork; reopen and retry"
                    .to_string(),
            ));
        }

        cleanup_abandoned_generations(&control_dir, std::time::Duration::ZERO);
        let generation = new_generation_name();
        let index_dir = generations_dir.join(&generation);
        fs::create_dir(&index_dir)?;
        let mut fallback_copy_bytes = 0u64;
        if let Err(error) = clone_tree_copy_on_write(
            &self.index_dir,
            &index_dir,
            self.generation.is_none(),
            &mut fallback_copy_bytes,
        ) {
            let _ = fs::remove_dir_all(&index_dir);
            return Err(error);
        }
        // The lease file is intentionally excluded from the COW clone. This is
        // a brand-new unpublished generation, so create its independent lease
        // before exposing the working store to the engine.
        let generation_lease = acquire_or_create_generation_lease(&index_dir)?;

        Ok(Self {
            root: self.root.clone(),
            index_dir,
            generation: Some(generation),
            file_trigram_delta_required: self.file_trigram_delta_required,
            defer_artifact_directory_sync: true,
            rebuild_lock: Some(rebuild_lock),
            generation_lease: Some(generation_lease),
        })
    }

    pub(crate) fn owns_rebuild_lock(&self) -> bool {
        self.rebuild_lock.is_some()
    }

    fn artifact_parent_directory_sync(&self, path: &Path) -> ParentDirectorySync {
        if !self.defer_artifact_directory_sync
            || self.rebuild_lock.is_none()
            || self.generation.is_none()
        {
            return ParentDirectorySync::Immediate;
        }

        let Ok(relative) = path.strip_prefix(&self.index_dir) else {
            return ParentDirectorySync::Immediate;
        };
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            return ParentDirectorySync::Immediate;
        }

        ParentDirectorySync::PublicationBarrier
    }

    /// Replace an index artifact without mutating a hard-linked active inode.
    ///
    /// A working generation still syncs each file before rename. Its parent
    /// directory sync may be batched into `publish_generation`'s recursive
    /// directory barrier because a crash before that barrier leaves the old
    /// manifest authoritative. Active, legacy, control-directory, and
    /// out-of-generation paths retain an immediate parent-directory sync.
    fn atomic_write_artifact(
        &self,
        path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> std::io::Result<()> {
        let path = path.as_ref();
        let parent_sync = self.artifact_parent_directory_sync(path);
        atomic_write_with_parent_sync(path, parent_sync, |file| file.write_all(contents.as_ref()))
    }

    /// Retry best-effort cleanup after callers release mmap-backed artifacts
    /// from the superseded generation. Windows keeps mapped files undeletable
    /// until those mappings are dropped, which can happen just after the
    /// publication commit point.
    pub(crate) fn retry_inactive_generation_cleanup(&self) {
        cleanup_abandoned_generations(&self.control_dir(), std::time::Duration::ZERO);
    }

    /// Hold this generation open for background work that may outlive the
    /// owning engine. The returned guard also performs deferred cleanup if a
    /// newer generation becomes active before the work finishes.
    pub(crate) fn background_generation_lease(&self) -> Result<BackgroundGenerationLease> {
        let generation = self.generation.clone().ok_or_else(|| {
            CodixingError::Config(
                "background generation work requires a generational index".to_string(),
            )
        })?;
        Ok(BackgroundGenerationLease {
            control_dir: self.control_dir(),
            generation,
            lease: Some(acquire_generation_lease(&self.index_dir)?),
        })
    }

    fn initialize_layout(&self, config: &IndexConfig) -> Result<()> {
        fs::create_dir_all(self.codixing_dir())?;
        fs::create_dir_all(self.tantivy_dir())?;
        fs::create_dir_all(self.vectors_dir())?;
        fs::create_dir_all(self.graph_dir())?;
        self.save_config(config)?;
        self.save_meta(&IndexMeta::default())?;
        Ok(())
    }

    /// Validate and atomically activate a fully-built generation.
    ///
    /// The manifest replacement is the commit point: before it, all readers
    /// see the old index; after it, all new readers see this one. Publication
    /// never renames or mutates the old generation. Once activation succeeds,
    /// known legacy artifacts and the superseded generation are removed on a
    /// best-effort basis to keep steady-state disk usage to one generation.
    pub fn publish_generation(&mut self) -> Result<()> {
        if self.rebuild_lock.is_none() {
            return Err(CodixingError::Config(
                "index generation publication requires the active rebuild lock".to_string(),
            ));
        }
        let generation = self.generation.as_deref().ok_or_else(|| {
            CodixingError::Config("cannot publish a legacy index store as a generation".to_string())
        })?;
        validate_generation_name(generation)?;

        // A legacy index may have only the v1 content-hash baseline. Direct
        // incremental checkpoints write their successful changes to the small
        // delta, so materialize the supported v1 baseline once before the v2
        // publication validator runs. Missing both formats remains corruption
        // and is still rejected below.
        if !self.tree_hashes_v2_path().is_file() && self.tree_hashes_path().is_file() {
            let hashes = self.load_tree_hashes_v2()?;
            self.save_tree_hashes_v2(&hashes)?;
        }
        self.validate_for_publication()?;

        // Every new or modified file has already been flushed by its writer.
        // Persist the generation's directory entries, including hard links to
        // immutable active artifacts, before publishing the pointer. Sync the
        // parent as well so the generation directory itself survives a crash.
        sync_publication_directories(&self.index_dir)?;
        if let Some(generations_dir) = self.index_dir.parent() {
            sync_directory(generations_dir)?;
        }
        // From this point on the generation's directory entries are durable.
        // Never defer another artifact rename, even if the manifest rename
        // succeeds and its parent-directory sync subsequently reports an
        // error: the working store may already be visible to new readers.
        self.defer_artifact_directory_sync = false;

        let control_dir = self.control_dir();
        let old_active = read_generation_manifest(&control_dir)
            .ok()
            .flatten()
            .map(|manifest| manifest.active);
        let manifest = GenerationManifest {
            layout_version: GENERATION_LAYOUT_VERSION,
            active: generation.to_string(),
            file_trigram_delta_required: true,
        };
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| {
            CodixingError::Serialization(format!(
                "failed to serialize active generation manifest: {e}"
            ))
        })?;
        // The directory fsync is part of the commit point. Otherwise a crash
        // could roll the rename back after cleanup removed the old active
        // generation.
        atomic_write_durable(control_dir.join(ACTIVE_GENERATION_FILE), bytes)?;
        self.file_trigram_delta_required = true;

        // Cleanup is deliberately non-fatal after the commit point. A failed
        // cleanup costs disk but must not turn a successful atomic activation
        // into a reported failure.
        if let Some(old) = old_active {
            if old != generation {
                remove_generation_if_safe(&control_dir, &old);
            }
        } else {
            cleanup_legacy_artifacts(&control_dir);
        }
        cleanup_abandoned_generations(&control_dir, std::time::Duration::ZERO);
        // Drop the OS lock only after activation and cleanup are complete.
        self.rebuild_lock.take();
        Ok(())
    }

    /// Ensure every artifact required for a complete lexical index can be
    /// opened before the generation becomes visible.
    pub fn validate_for_publication(&self) -> Result<()> {
        let config = self.load_config()?;
        self.load_meta()?;

        let required = [
            self.symbols_v2_path(),
            self.chunk_meta_path(),
            self.file_trigram_path(),
            self.file_trigram_delta_path(),
            self.tree_hashes_v2_path(),
        ];
        let missing: Vec<PathBuf> = required
            .into_iter()
            .filter(|path| !path.is_file())
            .collect();
        if !missing.is_empty() {
            return Err(CodixingError::PartialIndex {
                root: self.root.clone(),
                missing,
            });
        }

        let index = crate::index::TantivyIndex::open_read_only_with_config(
            &self.tantivy_dir(),
            config.bm25,
        )?;
        drop(index);
        let delta = self.load_file_trigram_delta_bytes()?.ok_or_else(|| {
            CodixingError::Serialization(
                "published generation is missing file_trigram_delta.bin".to_string(),
            )
        })?;
        let file_trigram = crate::index::trigram::FileTrigramIndex::load_binary_with_delta(
            &self.file_trigram_path(),
            Some(&delta),
        )?;
        drop(file_trigram);
        Ok(())
    }

    /// Open an existing `.codixing/` directory.
    ///
    /// Returns [`CodixingError::IndexNotFound`] if the directory does not exist
    /// at all, or [`CodixingError::PartialIndex`] if the directory exists but
    /// is missing the metadata files needed to bring up the engine
    /// (`config.json` and/or `meta.json`). The latter case happens after a
    /// partial deletion or when an older index format is missing newer
    /// required files, and is the failure mode addressed by `codixing repair`.
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_lease_access(root, true)
    }

    /// Open an existing index without creating or opening any artifact for
    /// write access. Generational indexes hold an existing shared lease;
    /// legacy layouts without a lease remain readable and are never modified.
    pub fn open_read_only(root: &Path) -> Result<Self> {
        Self::open_with_lease_access(root, false)
    }

    fn open_with_lease_access(root: &Path, writable: bool) -> Result<Self> {
        let codixing_dir = root.join(CODEFORGE_DIR);
        if !codixing_dir.is_dir() {
            return Err(CodixingError::IndexNotFound {
                path: root.to_path_buf(),
            });
        }

        for _ in 0..3 {
            let (index_dir, generation, file_trigram_delta_required) = resolve_index_dir(root)?;
            let lease_result = if writable {
                acquire_or_create_generation_lease(&index_dir).map(Some)
            } else {
                acquire_generation_lease(&index_dir).map(Some)
            };
            let generation_lease = match lease_result {
                Ok(lease) => lease,
                Err(CodixingError::Io(error))
                    if !writable
                        && generation.is_none()
                        && error.kind() == std::io::ErrorKind::NotFound =>
                {
                    // Pre-generation indexes have no lease. A read-only open
                    // must not create one merely to observe the legacy layout.
                    None
                }
                Err(CodixingError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Publication can race the resolve/lease window. Recheck the tiny
            // manifest while holding the lease: if it changed, release this
            // snapshot and retry against the new active generation.
            let (confirmed_dir, confirmed_generation, confirmed_trigram_requirement) =
                resolve_index_dir(root)?;
            if index_dir == confirmed_dir
                && generation == confirmed_generation
                && file_trigram_delta_required == confirmed_trigram_requirement
            {
                let missing: Vec<PathBuf> = [CONFIG_FILE, META_FILE]
                    .into_iter()
                    .map(|name| index_dir.join(name))
                    .filter(|path| !path.is_file())
                    .collect();
                if !missing.is_empty() {
                    return Err(CodixingError::PartialIndex {
                        root: root.to_path_buf(),
                        missing,
                    });
                }
                return Ok(Self {
                    root: root.to_path_buf(),
                    index_dir,
                    generation,
                    file_trigram_delta_required,
                    defer_artifact_directory_sync: false,
                    rebuild_lock: None,
                    generation_lease,
                });
            }
        }
        Err(CodixingError::Config(
            "active index generation changed repeatedly while opening; retry the operation"
                .to_string(),
        ))
    }

    /// Upgrade a legacy read-only snapshot to a mutation-capable lease only
    /// after the caller has acquired its writer. Generational snapshots
    /// already hold an existing shared lease and need no filesystem mutation.
    pub(crate) fn ensure_generation_lease_for_mutation(&mut self) -> Result<()> {
        if self.generation_lease.is_none() {
            self.generation_lease = Some(acquire_or_create_generation_lease(&self.index_dir)?);
        }
        Ok(())
    }

    /// Inspect a `.codixing/` directory layout without instantiating the engine.
    ///
    /// Reports which structural artifacts are present or missing so the CLI
    /// can tell users whether to run `codixing repair` (rebuild the missing
    /// pieces) or `codixing init` (build and atomically activate a clean
    /// generation).
    pub fn audit_layout(root: &Path) -> LayoutAudit {
        let control_dir = root.join(CODEFORGE_DIR);
        let dir_exists = control_dir.is_dir();
        let mut layout_error = None;
        let (index_dir, active_generation) = if dir_exists {
            match resolve_index_dir(root) {
                Ok((index_dir, generation, _)) => (index_dir, generation),
                Err(error) => {
                    layout_error = Some(error.to_string());
                    (control_dir.clone(), None)
                }
            }
        } else {
            (control_dir.clone(), None)
        };

        let mut essentials_present = Vec::new();
        let mut essentials_missing = Vec::new();
        if dir_exists {
            let tantivy_dir = index_dir.join(TANTIVY_DIR);
            for (path, expect_directory) in [
                (index_dir.join(CONFIG_FILE), false),
                (index_dir.join(META_FILE), false),
                (tantivy_dir.clone(), true),
                (tantivy_dir.join(META_FILE), false),
            ] {
                // `symlink_metadata` is deliberate: readiness requires real
                // regular files and a real Tantivy directory, never symlinks
                // or lookalike paths of the wrong type.
                let present = fs::symlink_metadata(&path).is_ok_and(|metadata| {
                    if expect_directory {
                        metadata.file_type().is_dir()
                    } else {
                        metadata.file_type().is_file()
                    }
                });
                if present {
                    essentials_present.push(path);
                } else {
                    essentials_missing.push(path);
                }
            }
        }

        // Optional artifacts — useful to mention in the repair report so
        // users know whether embeddings/graph/symbol sidecars were preserved.
        let mut optional_present = Vec::new();
        if dir_exists {
            for sub in &[VECTORS_DIR, GRAPH_DIR] {
                let path = index_dir.join(sub);
                if path.exists() {
                    optional_present.push(path);
                }
            }
            for file in &[
                SYMBOLS_FILE,
                SYMBOLS_V2_FILE,
                SYMBOLS_DELTA_FILE,
                FILE_TRIGRAM_FILE,
                FILE_TRIGRAM_DELTA_FILE,
                CHUNK_META_FILE,
                CONCEPTS_FILE,
                REFORMULATIONS_FILE,
            ] {
                let path = index_dir.join(file);
                if path.exists() {
                    optional_present.push(path);
                }
            }
        }

        // Best-effort: read the version field from meta.json if it survived
        // so `codixing repair` can mention "indexed by 0.40.x, current 0.41.x".
        let meta_version = index_dir
            .join(META_FILE)
            .exists()
            .then(|| fs::read_to_string(index_dir.join(META_FILE)).ok())
            .flatten()
            .and_then(|s| serde_json::from_str::<IndexMeta>(&s).ok())
            .map(|m| m.version);

        let mut generation_count = 0;
        let mut abandoned_generations = Vec::new();
        if let Ok(entries) = fs::read_dir(control_dir.join(GENERATIONS_DIR)) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(metadata) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if validate_generation_name(&name).is_err()
                    || !metadata.file_type().is_dir()
                    || metadata.file_type().is_symlink()
                {
                    continue;
                }
                generation_count += 1;
                if active_generation.as_deref() != Some(name.as_str()) {
                    abandoned_generations.push(path);
                }
            }
        }
        abandoned_generations.sort();

        LayoutAudit {
            dir_exists,
            essentials_present,
            essentials_missing,
            optional_present,
            meta_version,
            layout_kind: if !dir_exists {
                "missing"
            } else if layout_error.is_some() {
                "invalid"
            } else if active_generation.is_some() {
                "generational"
            } else {
                "legacy"
            },
            active_generation,
            generation_count,
            abandoned_generations,
            layout_error,
        }
    }

    /// Rebuild missing metadata files in place using safe defaults.
    ///
    /// Preserves any published artifacts (tantivy/, vectors/, graph/, symbols
    /// blobs), and reclaims expired unpublished vector generations while the
    /// repository writer, rebuild, and vector-publication locks are exclusive.
    /// Writes a default [`IndexConfig`] and [`IndexMeta`] when those files are
    /// absent. Returns created and reclaimed paths for the recovery report.
    ///
    /// This does *not* re-index source files — call `codixing sync` (or
    /// `Engine::open` followed by sync) afterwards to repopulate counts.
    pub fn repair(root: &Path) -> Result<RepairReport> {
        let control_dir = root.join(CODEFORGE_DIR);
        fs::create_dir_all(&control_dir)?;
        let _writer_lock = Self::try_acquire_writer_lock(root)?.ok_or_else(|| {
            CodixingError::Config(
                "cannot repair the index while another writer or background embedding task is active"
                    .to_string(),
            )
        })?;
        let rebuild_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(control_dir.join(REBUILD_LOCK_FILE))?;
        if !FileExt::try_lock_exclusive(&rebuild_lock)? {
            return Err(CodixingError::Config(
                "cannot repair the index while a rebuild or checkpoint is active".to_string(),
            ));
        }
        let (index_dir, generation, file_trigram_delta_required) = resolve_index_dir(root)?;

        let generation_lease = Some(acquire_or_create_generation_lease(&index_dir)?);
        let store = Self {
            root: root.to_path_buf(),
            index_dir,
            generation,
            file_trigram_delta_required,
            defer_artifact_directory_sync: false,
            rebuild_lock: Some(rebuild_lock),
            generation_lease,
        };
        fs::create_dir_all(store.codixing_dir())?;
        fs::create_dir_all(store.tantivy_dir())?;
        fs::create_dir_all(store.vectors_dir())?;
        require_real_directory(&store.vectors_dir(), "vector artifact directory")?;
        fs::create_dir_all(store.graph_dir())?;

        let removed_vector_artifacts = crate::vector::cleanup_abandoned_unpublished_generations(
            &store.vector_index_path(),
            crate::vector::VECTOR_ORPHAN_GRACE,
        )?;

        let mut created = Vec::new();
        let config_path = store.codixing_dir().join(CONFIG_FILE);
        if !config_path.exists() {
            store.save_config(&IndexConfig::new(root))?;
            created.push(config_path);
        }
        let meta_path = store.codixing_dir().join(META_FILE);
        if !meta_path.exists() {
            store.save_meta(&IndexMeta::default())?;
            created.push(meta_path);
        }

        Ok(RepairReport {
            created,
            removed_vector_artifacts,
        })
    }
}

/// Snapshot of which `.codixing/` artifacts are present, missing, or salvageable.
#[derive(Debug, Clone)]
pub struct LayoutAudit {
    pub dir_exists: bool,
    pub essentials_present: Vec<PathBuf>,
    pub essentials_missing: Vec<PathBuf>,
    pub optional_present: Vec<PathBuf>,
    pub meta_version: Option<String>,
    /// `legacy` for pre-generation indexes, `generational` after the first
    /// successful atomic rebuild.
    pub layout_kind: &'static str,
    pub active_generation: Option<String>,
    pub generation_count: usize,
    pub abandoned_generations: Vec<PathBuf>,
    pub layout_error: Option<String>,
}

impl LayoutAudit {
    /// True when the minimum structural index artifacts are real and present.
    pub fn is_complete(&self) -> bool {
        self.dir_exists && self.essentials_missing.is_empty() && self.layout_error.is_none()
    }
}

/// What `IndexStore::repair` rewrote (or left alone) on disk.
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// Paths of the files this call created. Empty when the layout was already
    /// complete and nothing needed to be recreated.
    pub created: Vec<PathBuf>,
    /// Expired unpublished vector artifacts reclaimed under the exclusive
    /// repair locks. Published and possibly-active generations are excluded.
    pub removed_vector_artifacts: Vec<PathBuf>,
}

impl IndexStore {
    /// Check whether root contains a structurally complete index layout.
    ///
    /// A stray control directory, an unpublished generation, or a layout with
    /// missing structural artifacts is not an existing index from a caller's
    /// perspective. This intentionally avoids parsing Tantivy internals.
    pub fn exists(root: &Path) -> bool {
        Self::audit_layout(root).is_complete()
    }

    /// Return the generation currently named by the atomic manifest.
    ///
    /// `None` denotes a compatible legacy flat index. Long-lived read-only
    /// engines can compare this value with [`Self::generation`] and reopen the
    /// complete engine when publication switches generations.
    pub fn active_generation(root: &Path) -> Result<Option<String>> {
        let control_dir = root.join(CODEFORGE_DIR);
        require_real_directory(&control_dir, "index control directory")?;
        Ok(read_generation_manifest(&control_dir)?.map(|manifest| manifest.active))
    }

    /// Return the project root path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the active index data directory.
    ///
    /// Legacy indexes return `<root>/.codixing`; generational indexes return
    /// `<root>/.codixing/generations/<active>`. Repo-local control files such as
    /// filters and shared-session logs belong in [`Self::control_dir`].
    pub fn codixing_dir(&self) -> PathBuf {
        self.index_dir.clone()
    }

    /// Stable `.codixing/` control directory shared by every generation.
    pub fn control_dir(&self) -> PathBuf {
        self.root.join(CODEFORGE_DIR)
    }

    /// Active generation name, or `None` for a compatible legacy flat index.
    pub fn generation(&self) -> Option<&str> {
        self.generation.as_deref()
    }

    /// Whether the active manifest guarantees that this generation was
    /// published with the mandatory file-trigram base+delta pair.
    ///
    /// Older manifests deserialize this capability as `false`, preserving
    /// base-only compatibility without allowing a missing sidecar from a new
    /// generation to masquerade as legacy state.
    pub(crate) fn file_trigram_delta_required(&self) -> bool {
        self.file_trigram_delta_required
    }

    /// Path to the tantivy index directory.
    pub fn tantivy_dir(&self) -> PathBuf {
        self.codixing_dir().join(TANTIVY_DIR)
    }

    /// Path to the `symbols.bin` file.
    pub fn symbols_path(&self) -> PathBuf {
        self.codixing_dir().join(SYMBOLS_FILE)
    }

    /// Path to the `symbols_v2.bin` file (mmap format).
    pub fn symbols_v2_path(&self) -> PathBuf {
        self.codixing_dir().join(SYMBOLS_V2_FILE)
    }

    /// Path to the bounded changed-file overlay over `symbols_v2.bin`.
    pub fn symbols_delta_path(&self) -> PathBuf {
        self.codixing_dir().join(SYMBOLS_DELTA_FILE)
    }

    /// Path to the `tree_hashes.bin` file (legacy v1 format).
    pub fn tree_hashes_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASHES_FILE)
    }

    /// Path to the `tree_hashes_v2.bin` file (extended format with mtime+size).
    pub fn tree_hashes_v2_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASHES_V2_FILE)
    }

    /// Path to the incremental-mutation write-ahead journal.
    pub fn dirty_paths_path(&self) -> PathBuf {
        self.control_dir().join(DIRTY_PATHS_FILE)
    }

    /// Path to the incremental hash overlay.
    pub fn tree_hash_delta_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_HASH_DELTA_FILE)
    }

    /// Path to the `tree_signatures.bin` sidecar (per-file signature fingerprints).
    pub fn tree_signatures_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_SIGNATURES_FILE)
    }

    fn tree_signatures_lock_path(&self) -> PathBuf {
        self.codixing_dir().join(TREE_SIGNATURES_LOCK_FILE)
    }

    fn acquire_tree_signatures_lock(&self) -> Result<TreeSignaturesLock> {
        use std::io::ErrorKind;
        use std::thread;
        use std::time::{Duration, Instant};

        fs::create_dir_all(self.codixing_dir())?;
        let lock_path = self.tree_signatures_lock_path();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    writeln!(file, "pid={}", std::process::id())?;
                    file.sync_all()?;
                    return Ok(TreeSignaturesLock { path: lock_path });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    return Err(CodixingError::Io(std::io::Error::new(
                        ErrorKind::TimedOut,
                        format!(
                            "timed out waiting for tree signature sidecar lock at {}",
                            lock_path.display()
                        ),
                    )));
                }
                Err(e) => return Err(CodixingError::Io(e)),
            }
        }
    }

    /// Path to the `vectors/` sub-directory.
    pub fn vectors_dir(&self) -> PathBuf {
        self.codixing_dir().join(VECTORS_DIR)
    }

    /// Path to the usearch HNSW index binary.
    pub fn vector_index_path(&self) -> PathBuf {
        self.vectors_dir().join(VECTOR_INDEX_FILE)
    }

    /// Path to the file-chunks map binary.
    pub fn file_chunks_path(&self) -> PathBuf {
        self.vectors_dir().join(FILE_CHUNKS_FILE)
    }

    /// Path to the chunk metadata binary.
    pub fn chunk_meta_path(&self) -> PathBuf {
        self.codixing_dir().join(CHUNK_META_FILE)
    }

    /// Path to the memory-mapped vector index file (`vectors.mmap`).
    pub fn mmap_vector_path(&self) -> PathBuf {
        self.codixing_dir().join(MMAP_VECTOR_FILE)
    }

    /// Path to the file-level trigram index.
    pub fn file_trigram_path(&self) -> PathBuf {
        self.codixing_dir().join(FILE_TRIGRAM_FILE)
    }

    /// Path to the bounded changed-file overlay over `file_trigram.bin`.
    pub fn file_trigram_delta_path(&self) -> PathBuf {
        self.codixing_dir().join(FILE_TRIGRAM_DELTA_FILE)
    }

    /// Path to the chunk-level trigram index (Strategy::Exact fast-path).
    pub fn chunk_trigram_path(&self) -> PathBuf {
        self.codixing_dir().join(CHUNK_TRIGRAM_FILE)
    }

    /// Path to the concept index binary (`concepts.bin`).
    pub fn concepts_path(&self) -> PathBuf {
        self.codixing_dir().join(CONCEPTS_FILE)
    }

    /// Path to the learned reformulations binary (`reformulations.bin`).
    pub fn reformulations_path(&self) -> PathBuf {
        self.codixing_dir().join(REFORMULATIONS_FILE)
    }

    /// Save the [`IndexConfig`] to `config.json`.
    pub fn save_config(&self, config: &IndexConfig) -> Result<()> {
        let path = self.codixing_dir().join(CONFIG_FILE);
        let json = serde_json::to_string_pretty(config).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize config: {e}"))
        })?;
        self.atomic_write_artifact(&path, json)?;
        Ok(())
    }

    /// Load the [`IndexConfig`] from `config.json`.
    pub fn load_config(&self) -> Result<IndexConfig> {
        let path = self.codixing_dir().join(CONFIG_FILE);
        let json = fs::read_to_string(&path)?;
        let config: IndexConfig = serde_json::from_str(&json).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize config: {e}"))
        })?;
        Ok(config)
    }

    /// Save the index metadata to `meta.json`.
    pub fn save_meta(&self, meta: &IndexMeta) -> Result<()> {
        let path = self.codixing_dir().join(META_FILE);
        let json = serde_json::to_string_pretty(meta)
            .map_err(|e| CodixingError::Serialization(format!("failed to serialize meta: {e}")))?;
        self.atomic_write_artifact(&path, json)?;
        Ok(())
    }

    /// Load the index metadata from `meta.json`.
    pub fn load_meta(&self) -> Result<IndexMeta> {
        let path = self.codixing_dir().join(META_FILE);
        let json = fs::read_to_string(&path)?;
        let meta: IndexMeta = serde_json::from_str(&json).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize meta: {e}"))
        })?;
        Ok(meta)
    }

    /// Path to the `graph/` sub-directory.
    pub fn graph_dir(&self) -> PathBuf {
        self.codixing_dir().join(GRAPH_DIR)
    }

    /// Path to the `graph/graph.bin` file.
    pub fn graph_path(&self) -> PathBuf {
        self.graph_dir().join(GRAPH_FILE)
    }

    /// Path to the `graph/schema.version` stamp file.
    pub fn graph_schema_version_path(&self) -> PathBuf {
        self.graph_dir().join(SCHEMA_VERSION_FILE)
    }

    /// Read the graph schema version this index's graph was built with.
    ///
    /// Indexes that predate the stamp (or with an unreadable stamp) report 1,
    /// which is always older than the current [`crate::graph::GRAPH_SCHEMA_VERSION`]
    /// and therefore triggers a one-time rebuild on the next sync.
    pub fn load_graph_schema_version(&self) -> u32 {
        fs::read_to_string(self.graph_schema_version_path())
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(1)
    }

    /// Serialize and persist the dependency graph.
    pub fn save_graph(&self, data: &GraphData) -> Result<()> {
        // Ensure the directory exists (may not on older indexes opened before Phase 3).
        fs::create_dir_all(self.graph_dir())?;
        let bytes = bitcode::serialize(data)
            .map_err(|e| CodixingError::Serialization(format!("failed to serialize graph: {e}")))?;
        self.atomic_write_artifact(self.graph_path(), bytes)?;
        // Stamp the schema version the edges were extracted with, so syncs can
        // detect graphs built by an older extractor/resolver and auto-rebuild.
        self.atomic_write_artifact(
            self.graph_schema_version_path(),
            crate::graph::GRAPH_SCHEMA_VERSION.to_string().into_bytes(),
        )?;
        Ok(())
    }

    /// Load the dependency graph from disk.  Returns `None` if no graph has been saved yet.
    pub fn load_graph(&self) -> Result<Option<GraphData>> {
        let path = self.graph_path();
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let data: GraphData = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize graph: {e}"))
        })?;
        Ok(Some(data))
    }

    /// Path to the `graph/symbol_graph.bin` file for the symbol-level graph.
    pub fn symbol_graph_path(&self) -> PathBuf {
        self.graph_dir().join("symbol_graph.bin")
    }

    /// Persist the symbol-level graph to disk via binary serialization.
    pub fn save_symbol_graph(&self, graph: &crate::graph::CodeGraph) -> Result<()> {
        fs::create_dir_all(self.graph_dir())?;
        crate::graph::persistence::save_graph_binary(graph, &self.symbol_graph_path())
    }

    /// Load the symbol-level graph from disk.
    pub fn load_symbol_graph(&self) -> Result<Option<crate::graph::CodeGraph>> {
        let path = self.symbol_graph_path();
        if !path.exists() {
            return Ok(None);
        }
        crate::graph::persistence::load_graph_binary(&path).map(Some)
    }

    /// Save raw bytes to the `symbols.bin` file.
    pub fn save_symbols_bytes(&self, bytes: &[u8]) -> Result<()> {
        self.atomic_write_artifact(self.symbols_path(), bytes)?;
        Ok(())
    }

    /// Load raw bytes from the `symbols.bin` file.
    pub fn load_symbols_bytes(&self) -> Result<Vec<u8>> {
        let bytes = fs::read(self.symbols_path())?;
        Ok(bytes)
    }

    /// Atomically persist the bounded changed-file file-trigram overlay.
    pub fn save_file_trigram_delta_bytes(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > crate::index::trigram::FILE_TRIGRAM_DELTA_MAX_BYTES {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta is {} bytes; maximum is {}",
                bytes.len(),
                crate::index::trigram::FILE_TRIGRAM_DELTA_MAX_BYTES
            )));
        }
        self.atomic_write_artifact(self.file_trigram_delta_path(), bytes)?;
        Ok(())
    }

    /// Load the file-trigram overlay through one bounded, non-following file
    /// handle so path replacement cannot race validation and allocation.
    pub fn load_file_trigram_delta_bytes(&self) -> Result<Option<Vec<u8>>> {
        use std::io::Read;

        let path = self.file_trigram_delta_path();
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }

        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        #[cfg(windows)]
        let is_reparse_point = {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        };
        #[cfg(not(windows))]
        let is_reparse_point = false;
        if is_reparse_point || !metadata.file_type().is_file() {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta is not a regular file: {}",
                path.display()
            )));
        }
        let maximum = crate::index::trigram::FILE_TRIGRAM_DELTA_MAX_BYTES;
        if metadata.len() > maximum as u64 {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta is {} bytes; maximum is {maximum}",
                metadata.len()
            )));
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(maximum)
                .min(maximum),
        );
        file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta grew beyond the maximum of {maximum} bytes while being read"
            )));
        }
        Ok(Some(bytes))
    }

    /// Atomically persist the bounded changed-file symbol overlay.
    pub fn save_symbol_delta_bytes(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > crate::symbols::persistence::SYMBOL_DELTA_MAX_BYTES {
            return Err(CodixingError::Serialization(format!(
                "symbol delta is {} bytes; maximum is {}",
                bytes.len(),
                crate::symbols::persistence::SYMBOL_DELTA_MAX_BYTES
            )));
        }
        self.atomic_write_artifact(self.symbols_delta_path(), bytes)?;
        Ok(())
    }

    /// Load the symbol overlay without allowing a corrupt or hostile artifact
    /// to allocate unbounded memory before format validation.
    pub fn load_symbol_delta_bytes(&self) -> Result<Option<Vec<u8>>> {
        use std::io::Read;

        let path = self.symbols_delta_path();
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Open the reparse point itself so a symlink cannot redirect the
            // validated handle between path inspection and the bounded read.
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }

        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        #[cfg(windows)]
        let is_reparse_point = {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        };
        #[cfg(not(windows))]
        let is_reparse_point = false;
        if is_reparse_point || !metadata.file_type().is_file() {
            return Err(CodixingError::Serialization(format!(
                "symbol delta is not a regular file: {}",
                path.display()
            )));
        }
        let maximum = crate::symbols::persistence::SYMBOL_DELTA_MAX_BYTES;
        if metadata.len() > maximum as u64 {
            return Err(CodixingError::Serialization(format!(
                "symbol delta is {} bytes; maximum is {maximum}",
                metadata.len()
            )));
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(maximum)
                .min(maximum),
        );
        file.take(maximum as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(CodixingError::Serialization(format!(
                "symbol delta grew beyond the maximum of {maximum} bytes while being read"
            )));
        }
        Ok(Some(bytes))
    }

    /// Save tree hashes (bitcode-serialized `Vec<(PathBuf, u64)>`).
    pub fn save_tree_hashes(&self, hashes: &[(PathBuf, u64)]) -> Result<()> {
        let bytes = bitcode::serialize(hashes).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hashes: {e}"))
        })?;
        self.atomic_write_artifact(self.tree_hashes_path(), bytes)?;
        Ok(())
    }

    /// Load tree hashes from `tree_hashes.bin`.
    pub fn load_tree_hashes(&self) -> Result<Vec<(PathBuf, u64)>> {
        let bytes = fs::read(self.tree_hashes_path())?;
        let hashes: Vec<(PathBuf, u64)> = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize tree hashes: {e}"))
        })?;
        Ok(hashes)
    }

    /// Save extended tree hashes (v2 format with mtime+size) to `tree_hashes_v2.bin`.
    ///
    /// Paths under the primary repository root are stored relative to it. The
    /// freshness map otherwise repeats the same absolute root once per file,
    /// making the artifact size depend on where the repository is checked out.
    /// Extra-root entries remain absolute because `IndexStore` intentionally
    /// does not duplicate the full indexing configuration.
    pub fn save_tree_hashes_v2(&self, hashes: &[(PathBuf, FileHashEntry)]) -> Result<()> {
        let compact: Vec<_> = hashes
            .iter()
            .map(|(path, entry)| (path.strip_prefix(&self.root).unwrap_or(path), entry))
            .collect();
        let bytes = bitcode::serialize(&compact).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hashes v2: {e}"))
        })?;
        self.atomic_write_artifact(self.tree_hashes_v2_path(), bytes)?;
        Ok(())
    }

    /// Load extended tree hashes (v2 format) from `tree_hashes_v2.bin`.
    ///
    /// Falls back to the legacy v1 format if v2 does not exist, converting
    /// entries to `FileHashEntry` with zeroed mtime/size (will trigger a
    /// full content-hash check on the first sync, then v2 is written).
    pub fn load_tree_hashes_v2(&self) -> Result<Vec<(PathBuf, FileHashEntry)>> {
        let v2_path = self.tree_hashes_v2_path();
        let mut hashes = if v2_path.exists() {
            let bytes = fs::read(&v2_path)?;
            let hashes: Vec<(PathBuf, FileHashEntry)> =
                bitcode::deserialize(&bytes).map_err(|e| {
                    CodixingError::Serialization(format!(
                        "failed to deserialize tree hashes v2: {e}"
                    ))
                })?;
            hashes
        } else {
            // Fall back to v1 and upconvert.
            self.load_tree_hashes()
                .unwrap_or_default()
                .into_iter()
                .map(|(path, hash)| {
                    (
                        path,
                        FileHashEntry {
                            content_hash: hash,
                            mtime_secs: 0,
                            mtime_nanos: 0,
                            size: 0,
                        },
                    )
                })
                .collect()
        };

        // New snapshots store primary-root entries relative to avoid repeating
        // the checkout path for every file. Old snapshots already contain
        // absolute paths, so leaving those untouched provides an in-place,
        // format-compatible migration on the next successful write.
        for (path, _) in &mut hashes {
            if path.is_relative() {
                *path = self.root.join(&*path);
            }
        }

        // Incremental watcher publications live in a small overlay until the
        // next full sync. Applying it here keeps every freshness consumer
        // authoritative without an O(repo-files) rewrite per edit.
        let delta = self.load_tree_hash_delta()?;
        if delta.is_empty() {
            return Ok(hashes);
        }
        let mut merged: std::collections::HashMap<PathBuf, FileHashEntry> =
            hashes.into_iter().collect();
        for (path, entry) in delta {
            match entry {
                Some(entry) => {
                    merged.insert(path, entry);
                }
                None => {
                    merged.remove(&path);
                }
            }
        }
        Ok(merged.into_iter().collect())
    }

    /// Load the successfully-published incremental hash overlay.
    pub fn load_tree_hash_delta(&self) -> Result<Vec<(PathBuf, Option<FileHashEntry>)>> {
        let path = self.tree_hash_delta_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(path)?;
        bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize tree hash delta: {e}"))
        })
    }

    /// Atomically merge successful watcher publications into the small overlay.
    pub fn update_tree_hash_delta(
        &self,
        updates: &[(PathBuf, Option<FileHashEntry>)],
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut delta: std::collections::HashMap<PathBuf, Option<FileHashEntry>> =
            self.load_tree_hash_delta()?.into_iter().collect();
        delta.extend(updates.iter().cloned());
        let mut delta: Vec<_> = delta.into_iter().collect();
        delta.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        self.replace_tree_hash_delta(&delta)
    }

    /// Atomically replace the overlay. Full sync uses this to align every
    /// existing overlay key with its authoritative snapshot value before the
    /// baseline rename, making replay idempotent in either crash window.
    pub fn replace_tree_hash_delta(
        &self,
        delta: &[(PathBuf, Option<FileHashEntry>)],
    ) -> Result<()> {
        let mut delta = delta.to_vec();
        delta.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let bytes = bitcode::serialize(&delta).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hash delta: {e}"))
        })?;
        self.atomic_write_artifact(self.tree_hash_delta_path(), bytes)?;
        Ok(())
    }

    /// Clear the overlay after it has been folded into a full hash snapshot.
    pub fn clear_tree_hash_delta(&self) -> Result<()> {
        let empty: Vec<(PathBuf, Option<FileHashEntry>)> = Vec::new();
        let bytes = bitcode::serialize(&empty).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree hash delta: {e}"))
        })?;
        self.atomic_write_artifact(self.tree_hash_delta_path(), bytes)?;
        Ok(())
    }

    /// Load paths whose index mutation has not yet been fully published.
    /// Missing journals are equivalent to an empty set for old indexes.
    pub fn load_dirty_paths(&self) -> Result<Vec<PathBuf>> {
        let path = self.dirty_paths_path();
        if !path.exists() {
            // Migrate the generation-local path-only journal written by older
            // versions. It is intentionally read but not removed until a
            // successful checkpoint publishes the replay.
            let legacy_path = self.codixing_dir().join(DIRTY_PATHS_FILE);
            if legacy_path == path || !legacy_path.exists() {
                return Ok(Vec::new());
            }
            let bytes = fs::read(legacy_path)?;
            return bitcode::deserialize(&bytes).map_err(|e| {
                CodixingError::Serialization(format!(
                    "failed to deserialize legacy dirty paths: {e}"
                ))
            });
        }
        let bytes = fs::read(path)?;
        let journal = decode_mutation_journal(&bytes)?;

        // Publication is the durable commit point. If the process died after
        // switching the manifest but before normalizing the journal, replay
        // only paths whose mutation failed. Before the switch, replay every
        // path because none of the working generation is visible yet.
        let active_generation = Self::active_generation(&self.root)?;
        if journal.working_generation == active_generation {
            if !journal.retry_after_publish.is_empty() {
                return Ok(journal.retry_after_publish);
            }
            let cleanup = (|| -> std::io::Result<()> {
                remove_file_durable(&self.dirty_paths_path())?;
                let legacy_path = self.codixing_dir().join(DIRTY_PATHS_FILE);
                if legacy_path != self.dirty_paths_path() {
                    remove_file_durable(&legacy_path)?;
                }
                Ok(())
            })();
            if let Err(error) = cleanup {
                tracing::warn!(
                    %error,
                    "completed mutation journal could not be removed; ignoring stale journal"
                );
            }
            return Ok(Vec::new());
        }
        if journal.base_generation != active_generation {
            tracing::warn!(
                journal_base = ?journal.base_generation,
                active = ?active_generation,
                "mutation journal base changed; replaying paths against the current active snapshot"
            );
        }
        Ok(journal.paths)
    }

    /// Atomically add paths to the incremental-mutation journal.
    pub fn mark_dirty_paths(&self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut dirty: std::collections::HashSet<PathBuf> =
            self.load_dirty_paths()?.into_iter().collect();
        dirty.extend(paths.iter().cloned());
        let mut dirty: Vec<_> = dirty.into_iter().collect();
        dirty.sort_unstable();
        let journal = MutationJournal {
            version: MUTATION_JOURNAL_VERSION,
            base_generation: Self::active_generation(&self.root)?,
            working_generation: self.generation.clone(),
            retry_after_publish: dirty.clone(),
            paths: dirty,
        };
        let bytes = bitcode::serialize(&journal).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize mutation journal: {e}"))
        })?;
        atomic_write_durable(self.dirty_paths_path(), bytes)?;
        Ok(())
    }

    /// Record the exact retry set before the manifest publication commit.
    ///
    /// Keeping both the original path set and the post-publication retry set
    /// makes the manifest itself the crash-window discriminator: an abandoned
    /// working generation replays all paths, while an activated generation
    /// replays only failed paths.
    pub fn prepare_dirty_paths_for_publication(
        &self,
        published: &std::collections::HashSet<PathBuf>,
    ) -> Result<()> {
        let path = self.dirty_paths_path();
        if !path.exists() {
            return Ok(());
        }
        let bytes = fs::read(&path)?;
        let mut journal = decode_mutation_journal(&bytes)?;
        if journal.working_generation != self.generation {
            return Err(CodixingError::Config(
                "mutation journal does not belong to the working generation".to_string(),
            ));
        }
        journal.retry_after_publish = journal
            .paths
            .iter()
            .filter(|path| !published.contains(*path))
            .cloned()
            .collect();
        journal.retry_after_publish.sort_unstable();
        journal.retry_after_publish.dedup();
        let bytes = bitcode::serialize(&journal).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize mutation journal: {e}"))
        })?;
        atomic_write_durable(path, bytes)?;
        Ok(())
    }

    /// Normalize the retry set after a successful manifest publication.
    pub fn clear_dirty_paths(&self, published: &std::collections::HashSet<PathBuf>) -> Result<()> {
        if published.is_empty() {
            return Ok(());
        }
        let active_generation = Self::active_generation(&self.root)?;
        if self.generation != active_generation {
            return Err(CodixingError::Config(
                "mutation paths may only be cleared after generation publication".to_string(),
            ));
        }
        let mut dirty = self.load_dirty_paths()?;
        dirty.retain(|path| !published.contains(path));
        dirty.sort_unstable();
        if dirty.is_empty() {
            remove_file_durable(&self.dirty_paths_path())?;
            let legacy_path = self.codixing_dir().join(DIRTY_PATHS_FILE);
            if legacy_path != self.dirty_paths_path() {
                remove_file_durable(&legacy_path)?;
            }
        } else {
            let journal = MutationJournal {
                version: MUTATION_JOURNAL_VERSION,
                base_generation: active_generation,
                working_generation: None,
                retry_after_publish: dirty.clone(),
                paths: dirty,
            };
            let bytes = bitcode::serialize(&journal).map_err(|e| {
                CodixingError::Serialization(format!("failed to serialize mutation journal: {e}"))
            })?;
            atomic_write_durable(self.dirty_paths_path(), bytes)?;
        }
        Ok(())
    }

    /// Reset the mutation journal after a complete authoritative rebuild.
    pub fn clear_all_dirty_paths(&self) -> Result<()> {
        remove_file_durable(&self.dirty_paths_path())?;
        let legacy_path = self.codixing_dir().join(DIRTY_PATHS_FILE);
        if legacy_path != self.dirty_paths_path() {
            remove_file_durable(&legacy_path)?;
        }
        Ok(())
    }

    /// Save per-file signature fingerprints to the `tree_signatures.bin` sidecar.
    ///
    /// Stored as a bitcode-serialized `Vec<(PathBuf, u64)>`. This is a *separate*
    /// file from the tree hashes so adding it never alters the existing hash-store
    /// format — an index built before this feature simply has no sidecar.
    pub fn save_tree_signatures(&self, sigs: &[(PathBuf, u64)]) -> Result<()> {
        let _lock = self.acquire_tree_signatures_lock()?;
        self.save_tree_signatures_unlocked(sigs)
    }

    fn save_tree_signatures_unlocked(&self, sigs: &[(PathBuf, u64)]) -> Result<()> {
        let bytes = bitcode::serialize(sigs).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize tree signatures: {e}"))
        })?;
        self.atomic_write_artifact(self.tree_signatures_path(), bytes)?;
        Ok(())
    }

    /// Load per-file signature fingerprints from `tree_signatures.bin`.
    ///
    /// Returns an empty vector if the sidecar does not exist (an index built
    /// before this feature) or fails to deserialize (forward/backward
    /// incompatibility). In both cases the caller treats every changed file as
    /// STRUCTURAL on the first sync — the conservative default — and the sidecar
    /// is rewritten afterwards.
    pub fn load_tree_signatures(&self) -> Result<Vec<(PathBuf, u64)>> {
        self.load_tree_signatures_unlocked()
    }

    fn load_tree_signatures_unlocked(&self) -> Result<Vec<(PathBuf, u64)>> {
        let path = self.tree_signatures_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&path)?;
        match bitcode::deserialize::<Vec<(PathBuf, u64)>>(&bytes) {
            Ok(sigs) => Ok(sigs),
            // A corrupt or incompatible sidecar must not corrupt the index:
            // fall back to "no fingerprints", i.e. treat all changes as structural.
            Err(_) => Ok(Vec::new()),
        }
    }

    /// Atomically update the signature sidecar under a cross-process lock.
    ///
    /// Mutators that perform read-modify-write must use this instead of doing
    /// load/filter/save themselves; otherwise a concurrent sync can resurrect a
    /// fingerprint that another process deliberately invalidated.
    pub fn update_tree_signatures<F>(&self, update: F) -> Result<()>
    where
        F: FnOnce(Vec<(PathBuf, u64)>) -> Vec<(PathBuf, u64)>,
    {
        let _lock = self.acquire_tree_signatures_lock()?;
        let current = self.load_tree_signatures_unlocked()?;
        let next = update(current);
        self.save_tree_signatures_unlocked(&next)
    }

    /// Save the chunk metadata map (bitcode-serialized `Vec<(u64, ChunkMeta)>`).
    ///
    /// Accepts a flat list of `(chunk_id, meta)` pairs rather than the DashMap
    /// directly to avoid depending on DashMap in persistence.
    pub fn save_chunk_meta_bytes(&self, bytes: &[u8]) -> Result<()> {
        self.atomic_write_artifact(self.chunk_meta_path(), bytes)?;
        Ok(())
    }

    /// Load raw bytes from `chunk_meta.bin`.
    pub fn load_chunk_meta_bytes(&self) -> Result<Vec<u8>> {
        let bytes = fs::read(self.chunk_meta_path())?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IndexConfig;
    use crate::index::TantivyIndex;
    use filetime::{FileTime, set_file_mtime};
    use tempfile::tempdir;

    struct TestVectorArtifacts {
        manifest: PathBuf,
        index: PathBuf,
        chunks: PathBuf,
        manifest_tmp: PathBuf,
    }

    fn test_vector_artifacts(store: &IndexStore, generation: &str) -> TestVectorArtifacts {
        let index_path = store.vector_index_path();
        let parent = index_path.parent().unwrap();
        let index_name = index_path.file_name().unwrap().to_string_lossy();
        let manifest = parent.join(format!(
            "{index_name}.manifest.generation-{generation}.json"
        ));
        TestVectorArtifacts {
            manifest_tmp: manifest.with_extension("json.tmp"),
            manifest,
            index: parent.join(format!("{index_name}.generation-{generation}")),
            chunks: parent.join(format!("{index_name}.file-chunks.generation-{generation}")),
        }
    }

    fn mark_vector_artifacts_old(paths: &[&Path]) {
        for path in paths {
            set_file_mtime(path, FileTime::from_unix_time(0, 0)).unwrap();
        }
    }

    fn commit_empty_tantivy(store: &IndexStore) {
        let tantivy = TantivyIndex::create_in_dir(&store.tantivy_dir()).unwrap();
        tantivy.commit().unwrap();
    }

    fn activate_structural_test_generation(root: &Path) -> PathBuf {
        let store = IndexStore::begin_generation(root, &IndexConfig::new(root)).unwrap();
        commit_empty_tantivy(&store);
        let manifest = GenerationManifest {
            layout_version: GENERATION_LAYOUT_VERSION,
            active: store.generation().unwrap().to_owned(),
            file_trigram_delta_required: true,
        };
        let tantivy_dir = store.tantivy_dir();
        atomic_write_durable(
            root.join(CODEFORGE_DIR).join(ACTIVE_GENERATION_FILE),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        drop(store);
        tantivy_dir
    }

    #[test]
    fn init_creates_directory_structure() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        assert!(store.codixing_dir().is_dir());
        assert!(store.tantivy_dir().is_dir());
        assert!(store.codixing_dir().join(CONFIG_FILE).is_file());
        assert!(store.codixing_dir().join(META_FILE).is_file());
        assert!(
            !IndexStore::exists(root),
            "a bare layout is incomplete before Tantivy commits metadata"
        );

        commit_empty_tantivy(&store);
        assert!(IndexStore::exists(root));
    }

    #[test]
    fn unpublished_generation_defers_only_scoped_artifact_directory_sync() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::begin_generation(root, &IndexConfig::new(root)).unwrap();

        assert_eq!(
            store.artifact_parent_directory_sync(&store.chunk_meta_path()),
            ParentDirectorySync::PublicationBarrier
        );
        assert_eq!(
            store.artifact_parent_directory_sync(&store.graph_path()),
            ParentDirectorySync::PublicationBarrier
        );
        assert_eq!(
            store.artifact_parent_directory_sync(&store.dirty_paths_path()),
            ParentDirectorySync::Immediate,
            "control-directory journals must retain their own durability barrier"
        );
        assert_eq!(
            store.artifact_parent_directory_sync(&root.join("outside.bin")),
            ParentDirectorySync::Immediate
        );
        assert_eq!(
            store.artifact_parent_directory_sync(
                &store
                    .codixing_dir()
                    .join("nested")
                    .join("..")
                    .join("escape.bin")
            ),
            ParentDirectorySync::Immediate,
            "lexical traversal must never inherit the publication barrier"
        );

        let active_dir = tempdir().unwrap();
        let active_root = active_dir.path();
        let active = IndexStore::init(active_root, &IndexConfig::new(active_root)).unwrap();
        assert_eq!(
            active.artifact_parent_directory_sync(&active.chunk_meta_path()),
            ParentDirectorySync::Immediate,
            "active and legacy stores must make each rename durable immediately"
        );
    }

    #[test]
    fn deferred_parent_sync_keeps_atomic_file_replacement_contract() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("artifact.bin");

        atomic_write_with_parent_sync(&path, ParentDirectorySync::PublicationBarrier, |file| {
            file.write_all(b"before")
        })
        .unwrap();
        atomic_write_with_parent_sync(&path, ParentDirectorySync::PublicationBarrier, |file| {
            file.write_all(b"after")
        })
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"after");
        assert!(fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.")
        }));
    }

    #[test]
    fn open_existing_store_succeeds() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        IndexStore::init(root, &config).unwrap();
        let store = IndexStore::open(root).unwrap();

        assert!(store.codixing_dir().is_dir());
    }

    #[test]
    fn legacy_read_only_open_does_not_create_generation_lease() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);
        drop(IndexStore::init(root, &config).unwrap());
        let lease_path = root.join(CODEFORGE_DIR).join(GENERATION_LEASE_FILE);
        fs::remove_file(&lease_path).unwrap();

        let mut store = IndexStore::open_read_only(root).unwrap();
        assert!(!lease_path.exists());
        store.ensure_generation_lease_for_mutation().unwrap();
        assert!(lease_path.is_file());
    }

    #[test]
    fn open_nonexistent_returns_index_not_found() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        let err = IndexStore::open(root).unwrap_err();
        assert!(
            matches!(err, CodixingError::IndexNotFound { .. }),
            "expected IndexNotFound, got: {err}"
        );
    }

    #[test]
    fn open_partial_layout_returns_partial_index_error() {
        // Regression for #100: when `.codixing/` exists with index artifacts
        // but `config.json` is missing, `IndexStore::open` must return a
        // PartialIndex error naming the missing file instead of letting the
        // failure surface as a generic `I/O error: No such file or directory`.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);
        IndexStore::init(root, &config).unwrap();

        // Simulate the failure mode reported in the issue.
        fs::remove_file(root.join(CODEFORGE_DIR).join(CONFIG_FILE)).unwrap();

        let err = IndexStore::open(root).unwrap_err();
        match err {
            CodixingError::PartialIndex { ref missing, .. } => {
                assert!(
                    missing.iter().any(|p| p.ends_with(CONFIG_FILE)),
                    "missing list should include config.json: {missing:?}"
                );
                let rendered = format!("{err}");
                assert!(
                    rendered.contains("codixing repair"),
                    "error should suggest repair: {rendered}"
                );
            }
            other => panic!("expected PartialIndex, got: {other:?}"),
        }
    }

    #[test]
    fn repair_recreates_missing_metadata_in_place() {
        // Regression for #100: `IndexStore::repair` must rewrite the missing
        // metadata files using safe defaults while leaving existing
        // artifacts (tantivy/, vectors/, graph/) untouched.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);
        let store = IndexStore::init(root, &config).unwrap();
        commit_empty_tantivy(&store);
        drop(store);

        // Drop the metadata files but keep a marker file under tantivy/ so
        // we can prove repair did not nuke the rest of the index.
        let tantivy_marker = root.join(CODEFORGE_DIR).join(TANTIVY_DIR).join("MARKER");
        fs::write(&tantivy_marker, b"keep me").unwrap();
        fs::remove_file(root.join(CODEFORGE_DIR).join(CONFIG_FILE)).unwrap();
        fs::remove_file(root.join(CODEFORGE_DIR).join(META_FILE)).unwrap();

        let pre = IndexStore::audit_layout(root);
        assert!(!pre.is_complete());

        let report = IndexStore::repair(root).unwrap();
        assert_eq!(report.created.len(), 2, "should have rewritten 2 files");

        let post = IndexStore::audit_layout(root);
        assert!(post.is_complete(), "layout should be complete after repair");
        assert!(
            tantivy_marker.exists(),
            "repair must not delete preserved tantivy artifacts"
        );

        // Sanity: store is now openable.
        IndexStore::open(root).unwrap();
    }

    #[test]
    fn repair_reclaims_expired_unpublished_vector_artifacts_only_explicitly() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        commit_empty_tantivy(&store);
        let artifacts = test_vector_artifacts(
            &store,
            "11111111111111111111111111111111-11111111-1111111111111111",
        );
        fs::write(&artifacts.index, b"abandoned index").unwrap();
        fs::write(&artifacts.chunks, b"abandoned chunks").unwrap();
        fs::write(&artifacts.manifest_tmp, b"abandoned manifest temp").unwrap();
        mark_vector_artifacts_old(&[&artifacts.index, &artifacts.chunks, &artifacts.manifest_tmp]);
        drop(store);

        // Ordinary open is observational and must never run maintenance.
        drop(IndexStore::open(root).unwrap());
        assert!(artifacts.index.exists());
        assert!(artifacts.chunks.exists());
        assert!(artifacts.manifest_tmp.exists());

        let report = IndexStore::repair(root).unwrap();
        assert!(report.created.is_empty());
        assert_eq!(
            report.removed_vector_artifacts,
            vec![artifacts.chunks, artifacts.index, artifacts.manifest_tmp]
        );
    }

    #[test]
    fn repair_preserves_generation_when_any_unpublished_artifact_is_young() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        commit_empty_tantivy(&store);
        let artifacts = test_vector_artifacts(
            &store,
            "22222222222222222222222222222222-22222222-2222222222222222",
        );
        fs::write(&artifacts.index, b"old index").unwrap();
        fs::write(&artifacts.chunks, b"possibly active chunks").unwrap();
        mark_vector_artifacts_old(&[&artifacts.index]);
        drop(store);

        let report = IndexStore::repair(root).unwrap();
        assert!(report.removed_vector_artifacts.is_empty());
        assert!(artifacts.index.exists());
        assert!(artifacts.chunks.exists());
    }

    #[test]
    fn repair_never_reclaims_a_manifested_vector_generation() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        commit_empty_tantivy(&store);
        let artifacts = test_vector_artifacts(
            &store,
            "33333333333333333333333333333333-33333333-3333333333333333",
        );
        fs::write(&artifacts.index, b"published index").unwrap();
        fs::write(&artifacts.chunks, b"published chunks").unwrap();
        fs::write(&artifacts.manifest, b"publication point").unwrap();
        mark_vector_artifacts_old(&[&artifacts.index, &artifacts.chunks, &artifacts.manifest]);
        drop(store);

        let report = IndexStore::repair(root).unwrap();
        assert!(report.removed_vector_artifacts.is_empty());
        assert!(artifacts.manifest.exists());
        assert!(artifacts.index.exists());
        assert!(artifacts.chunks.exists());
    }

    #[cfg(unix)]
    #[test]
    fn repair_rejects_symlinked_vector_directory_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        commit_empty_tantivy(&store);
        let vectors_dir = store.vectors_dir();
        drop(store);

        fs::remove_dir(&vectors_dir).unwrap();
        let outside = root.join("outside-vectors");
        fs::create_dir(&outside).unwrap();
        let victim = outside.join(
            "index.usearch.generation-44444444444444444444444444444444-44444444-4444444444444444",
        );
        fs::write(&victim, b"outside victim").unwrap();
        mark_vector_artifacts_old(&[&victim]);
        symlink(&outside, &vectors_dir).unwrap();

        let error = IndexStore::repair(root)
            .expect_err("repair must reject a symlinked vector artifact directory");
        assert!(error.to_string().contains("vector artifact directory"));
        assert_eq!(fs::read(&victim).unwrap(), b"outside victim");
    }

    #[test]
    fn repair_fails_fast_while_a_writable_engine_is_active() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut config = IndexConfig::new(root);
        config.embedding.enabled = false;
        let engine = crate::Engine::init(root, config).unwrap();

        let error = IndexStore::repair(root)
            .expect_err("repair must not race a live writer or background embedder");
        assert!(error.to_string().contains("another writer"));

        drop(engine);
        IndexStore::repair(root).unwrap();
    }

    #[test]
    fn config_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let loaded = store.load_config().unwrap();
        assert_eq!(config, loaded);

        // Modify and round-trip again
        let mut updated = loaded;
        updated.languages.insert("rust".to_string());
        store.save_config(&updated).unwrap();
        let reloaded = store.load_config().unwrap();
        assert_eq!(updated, reloaded);
    }

    #[test]
    fn meta_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        // Load the default meta written by init
        let default_meta = store.load_meta().unwrap();
        assert_eq!(default_meta.version, "0.1.0");
        assert_eq!(default_meta.file_count, 0);

        // Update and round-trip
        let meta = IndexMeta {
            version: "0.1.0".to_string(),
            file_count: 42,
            chunk_count: 128,
            symbol_count: 500,
            last_indexed: "2026-02-07T12:00:00Z".to_string(),
            git_commit: Some("abc123def456".to_string()),
        };
        store.save_meta(&meta).unwrap();
        let loaded = store.load_meta().unwrap();
        assert_eq!(meta, loaded);
    }

    #[test]
    fn tree_hashes_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let hashes = vec![
            (PathBuf::from("src/main.rs"), 0xDEADBEEF_u64),
            (PathBuf::from("src/lib.rs"), 0xCAFEBABE_u64),
            (PathBuf::from("tests/integration.rs"), 0x12345678_u64),
        ];

        store.save_tree_hashes(&hashes).unwrap();
        let loaded = store.load_tree_hashes().unwrap();
        assert_eq!(hashes, loaded);
    }

    #[test]
    fn symbols_bytes_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let data = vec![1_u8, 2, 3, 4, 5, 42, 255, 0];
        store.save_symbols_bytes(&data).unwrap();
        let loaded = store.load_symbols_bytes().unwrap();
        assert_eq!(data, loaded);
    }

    #[test]
    fn exists_requires_a_structurally_complete_layout() {
        let missing = tempdir().unwrap();
        assert!(!IndexStore::exists(missing.path()));

        let partial_legacy = tempdir().unwrap();
        let partial_store = IndexStore::init(
            partial_legacy.path(),
            &IndexConfig::new(partial_legacy.path()),
        )
        .unwrap();
        commit_empty_tantivy(&partial_store);
        drop(partial_store);
        fs::remove_file(partial_legacy.path().join(CODEFORGE_DIR).join(CONFIG_FILE)).unwrap();
        assert!(!IndexStore::exists(partial_legacy.path()));

        let unpublished = tempdir().unwrap();
        let unpublished_store =
            IndexStore::begin_generation(unpublished.path(), &IndexConfig::new(unpublished.path()))
                .unwrap();
        commit_empty_tantivy(&unpublished_store);
        assert!(!IndexStore::exists(unpublished.path()));
        drop(unpublished_store);

        let legacy = tempdir().unwrap();
        let legacy_store =
            IndexStore::init(legacy.path(), &IndexConfig::new(legacy.path())).unwrap();
        assert!(
            !IndexStore::exists(legacy.path()),
            "missing legacy tantivy/meta.json must be incomplete"
        );
        commit_empty_tantivy(&legacy_store);
        assert!(IndexStore::exists(legacy.path()));
        let legacy_tantivy_dir = legacy_store.tantivy_dir();
        drop(legacy_store);
        fs::remove_dir_all(legacy_tantivy_dir).unwrap();
        assert!(
            !IndexStore::exists(legacy.path()),
            "missing legacy Tantivy directory must be incomplete"
        );

        let active = tempdir().unwrap();
        let active_root = active.path();
        let active_tantivy_dir = activate_structural_test_generation(active_root);
        assert!(IndexStore::exists(active_root));
        fs::remove_file(active_tantivy_dir.join(META_FILE)).unwrap();
        assert!(
            !IndexStore::exists(active_root),
            "missing active-generation tantivy/meta.json must be incomplete"
        );

        let active_without_tantivy = tempdir().unwrap();
        let active_tantivy_dir = activate_structural_test_generation(active_without_tantivy.path());
        fs::remove_dir_all(active_tantivy_dir).unwrap();
        assert!(
            !IndexStore::exists(active_without_tantivy.path()),
            "missing active-generation Tantivy directory must be incomplete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exists_rejects_symlinked_structural_artifacts() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        commit_empty_tantivy(&store);
        assert!(IndexStore::exists(root));

        let config_path = store.codixing_dir().join(CONFIG_FILE);
        let real_config = root.join("real-config.json");
        fs::rename(&config_path, &real_config).unwrap();
        symlink(&real_config, &config_path).unwrap();
        assert!(!IndexStore::exists(root));
        fs::remove_file(&config_path).unwrap();
        fs::rename(&real_config, &config_path).unwrap();

        let tantivy_meta = store.tantivy_dir().join(META_FILE);
        let real_tantivy_meta = root.join("real-tantivy-meta.json");
        fs::rename(&tantivy_meta, &real_tantivy_meta).unwrap();
        symlink(&real_tantivy_meta, &tantivy_meta).unwrap();
        assert!(!IndexStore::exists(root));
        fs::remove_file(&tantivy_meta).unwrap();
        fs::rename(&real_tantivy_meta, &tantivy_meta).unwrap();

        let tantivy_dir = store.tantivy_dir();
        let real_tantivy_dir = root.join("real-tantivy");
        drop(store);
        fs::rename(&tantivy_dir, &real_tantivy_dir).unwrap();
        symlink(&real_tantivy_dir, &tantivy_dir).unwrap();
        assert!(!IndexStore::exists(root));
    }

    #[test]
    fn path_accessors_are_consistent() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        assert_eq!(store.codixing_dir(), root.join(".codixing"));
        assert_eq!(store.tantivy_dir(), root.join(".codixing/tantivy"));
        assert_eq!(store.symbols_path(), root.join(".codixing/symbols.bin"));
        assert_eq!(
            store.tree_hashes_path(),
            root.join(".codixing/tree_hashes.bin")
        );
    }

    #[test]
    fn file_hash_entry_round_trip() {
        let now = SystemTime::now();
        let entry = FileHashEntry::new(0xDEADBEEF, Some(now), 1024);

        // Check that mtime round-trips correctly.
        let recovered = entry.mtime().unwrap();
        let now_duration = now.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let rec_duration = recovered.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(now_duration.as_secs(), rec_duration.as_secs());
        assert_eq!(now_duration.subsec_nanos(), rec_duration.subsec_nanos());

        assert_eq!(entry.content_hash, 0xDEADBEEF);
        assert_eq!(entry.size, 1024);
    }

    #[test]
    fn file_hash_entry_unchanged_detection() {
        let now = SystemTime::now();
        let entry = FileHashEntry::new(0xCAFE, Some(now), 512);

        // Same mtime+size → not changed.
        assert!(!entry.file_might_have_changed(Some(now), 512));

        // Different size → changed.
        assert!(entry.file_might_have_changed(Some(now), 999));

        // Different mtime → changed.
        let later = now + std::time::Duration::from_secs(1);
        assert!(entry.file_might_have_changed(Some(later), 512));

        // No mtime → changed (conservative).
        assert!(entry.file_might_have_changed(None, 512));
    }

    #[test]
    fn tree_hashes_v2_round_trip() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        let now = SystemTime::now();
        let hashes = vec![
            (
                PathBuf::from("src/main.rs"),
                FileHashEntry::new(0xDEADBEEF, Some(now), 1024),
            ),
            (
                PathBuf::from("src/lib.rs"),
                FileHashEntry::new(0xCAFEBABE, Some(now), 2048),
            ),
        ];

        store.save_tree_hashes_v2(&hashes).unwrap();
        let loaded = store.load_tree_hashes_v2().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].0, root.join("src/main.rs"));
        assert_eq!(loaded[1].0, root.join("src/lib.rs"));
        assert_eq!(loaded[0].1.content_hash, hashes[0].1.content_hash);
        assert_eq!(loaded[1].1.size, 2048);
    }

    #[test]
    fn tree_hashes_v2_compacts_primary_root_and_preserves_external_paths() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        let store = IndexStore::init(&root, &IndexConfig::new(&root)).unwrap();
        let primary = root.join("src/lib.rs");
        let external = dir.path().join("shared/src/lib.rs");
        let hashes = vec![
            (primary.clone(), FileHashEntry::new(1, None, 10)),
            (external.clone(), FileHashEntry::new(2, None, 20)),
        ];

        store.save_tree_hashes_v2(&hashes).unwrap();

        let bytes = fs::read(store.tree_hashes_v2_path()).unwrap();
        let persisted: Vec<(PathBuf, FileHashEntry)> = bitcode::deserialize(&bytes).unwrap();
        assert_eq!(persisted[0].0, PathBuf::from("src/lib.rs"));
        assert_eq!(persisted[1].0, external);
        assert_eq!(store.load_tree_hashes_v2().unwrap(), hashes);
    }

    #[test]
    fn symbol_delta_loader_rejects_oversized_regular_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        let file = fs::File::create(store.symbols_delta_path()).unwrap();
        file.set_len(crate::symbols::persistence::SYMBOL_DELTA_MAX_BYTES as u64 + 1)
            .unwrap();

        let error = store.load_symbol_delta_bytes().unwrap_err();
        assert!(error.to_string().contains("maximum"));
    }

    #[test]
    fn file_trigram_delta_loader_rejects_oversized_regular_file() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        let file = fs::File::create(store.file_trigram_delta_path()).unwrap();
        file.set_len(crate::index::trigram::FILE_TRIGRAM_DELTA_MAX_BYTES as u64 + 1)
            .unwrap();

        let error = store.load_file_trigram_delta_bytes().unwrap_err();
        assert!(error.to_string().contains("maximum"));
    }

    #[cfg(unix)]
    #[test]
    fn symbol_delta_loader_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        let target = root.join("outside-symbol-delta.bin");
        fs::write(&target, b"not a sidecar").unwrap();
        symlink(&target, store.symbols_delta_path()).unwrap();

        assert!(store.load_symbol_delta_bytes().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_trigram_delta_loader_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        let target = root.join("outside-file-trigram-delta.bin");
        fs::write(&target, b"not a sidecar").unwrap();
        symlink(&target, store.file_trigram_delta_path()).unwrap();

        assert!(store.load_file_trigram_delta_bytes().is_err());
    }

    #[test]
    fn tree_hashes_v2_loads_legacy_absolute_snapshot() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        let legacy = vec![(
            root.join("src/legacy.rs"),
            FileHashEntry::new(0xABCD, None, 42),
        )];
        let bytes = bitcode::serialize(&legacy).unwrap();
        atomic_write_durable(store.tree_hashes_v2_path(), bytes).unwrap();

        assert_eq!(store.load_tree_hashes_v2().unwrap(), legacy);
    }

    #[test]
    fn tree_hashes_v2_size_is_independent_of_primary_root_length() {
        let dir = tempdir().unwrap();
        let short_root = dir.path().join("a");
        let long_root = dir
            .path()
            .join("a-much-longer-checkout-location-used-for-the-same-repository");
        fs::create_dir(&short_root).unwrap();
        fs::create_dir(&long_root).unwrap();
        let short_store = IndexStore::init(&short_root, &IndexConfig::new(&short_root)).unwrap();
        let long_store = IndexStore::init(&long_root, &IndexConfig::new(&long_root)).unwrap();
        let entry = FileHashEntry::new(0xCAFE, None, 123);

        short_store
            .save_tree_hashes_v2(&[(short_root.join("src/lib.rs"), entry.clone())])
            .unwrap();
        long_store
            .save_tree_hashes_v2(&[(long_root.join("src/lib.rs"), entry)])
            .unwrap();

        assert_eq!(
            fs::metadata(short_store.tree_hashes_v2_path())
                .unwrap()
                .len(),
            fs::metadata(long_store.tree_hashes_v2_path())
                .unwrap()
                .len()
        );
    }

    #[test]
    fn tree_signatures_update_reloads_and_rewrites_under_lock() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();
        store
            .save_tree_signatures(&[
                (PathBuf::from("src/a.rs"), 1),
                (PathBuf::from("src/b.rs"), 2),
            ])
            .unwrap();

        store
            .update_tree_signatures(|sigs| {
                sigs.into_iter()
                    .filter(|(path, _)| path != Path::new("src/a.rs"))
                    .chain(std::iter::once((PathBuf::from("src/c.rs"), 3)))
                    .collect()
            })
            .unwrap();

        let loaded = store.load_tree_signatures().unwrap();
        assert_eq!(
            loaded,
            vec![
                (PathBuf::from("src/b.rs"), 2),
                (PathBuf::from("src/c.rs"), 3)
            ]
        );
        assert!(!store.tree_signatures_lock_path().exists());
    }

    #[test]
    fn tree_hashes_v2_fallback_from_v1() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let config = IndexConfig::new(root);

        let store = IndexStore::init(root, &config).unwrap();

        // Write only v1 hashes.
        let v1_hashes = vec![
            (PathBuf::from("src/main.rs"), 0xDEADBEEF_u64),
            (PathBuf::from("src/lib.rs"), 0xCAFEBABE_u64),
        ];
        store.save_tree_hashes(&v1_hashes).unwrap();

        // load_tree_hashes_v2 should fall back to v1 with zeroed mtime/size.
        let loaded = store.load_tree_hashes_v2().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].1.content_hash, 0xDEADBEEF);
        assert_eq!(loaded[0].1.mtime_secs, 0);
        assert_eq!(loaded[0].1.size, 0);
    }

    #[test]
    fn legacy_flat_path_only_journal_loads_from_the_control_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let store = IndexStore::init(root, &IndexConfig::new(root)).unwrap();
        let expected = vec![root.join("src/lib.rs"), root.join("src/main.rs")];
        let bytes = bitcode::serialize(&expected).unwrap();
        atomic_write_durable(store.dirty_paths_path(), bytes).unwrap();

        assert_eq!(store.load_dirty_paths().unwrap(), expected);
        let published = expected.iter().cloned().collect();
        store.clear_dirty_paths(&published).unwrap();
        assert!(!store.dirty_paths_path().exists());
    }

    #[test]
    fn version_one_mutation_journal_migrates_conservatively() {
        let expected = vec![PathBuf::from("src/lib.rs"), PathBuf::from("src/main.rs")];
        let legacy = MutationJournalV1 {
            version: 1,
            base_generation: Some("generation-a".to_string()),
            working_generation: Some("generation-b".to_string()),
            paths: expected.clone(),
        };

        let decoded = decode_mutation_journal(&bitcode::serialize(&legacy).unwrap()).unwrap();
        assert_eq!(decoded.version, MUTATION_JOURNAL_VERSION);
        assert_eq!(decoded.base_generation, legacy.base_generation);
        assert_eq!(decoded.working_generation, legacy.working_generation);
        assert_eq!(decoded.paths, expected);
        assert_eq!(decoded.retry_after_publish, expected);
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_clone_hardlinks_immutable_files_and_detaches_tantivy_controls() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempdir().unwrap();
        let source = dir.path().join("active");
        let destination = dir.path().join("working");
        let source_tantivy = source.join(TANTIVY_DIR);
        fs::create_dir_all(&source_tantivy).unwrap();
        fs::create_dir(&destination).unwrap();

        let segment = source_tantivy.join("segment.fast");
        let control = source_tantivy.join("meta.json");
        fs::write(&segment, b"immutable segment").unwrap();
        fs::write(&control, b"mutable control").unwrap();

        let mut fallback_copy_bytes = 0;
        clone_tree_copy_on_write(&source, &destination, false, &mut fallback_copy_bytes).unwrap();
        sync_publication_directories(&destination).unwrap();

        let cloned_segment = destination.join(TANTIVY_DIR).join("segment.fast");
        let cloned_control = destination.join(TANTIVY_DIR).join("meta.json");
        let segment_before = fs::metadata(&segment).unwrap();
        let segment_after = fs::metadata(&cloned_segment).unwrap();
        let control_before = fs::metadata(&control).unwrap();
        let control_after = fs::metadata(&cloned_control).unwrap();

        assert_eq!(segment_before.dev(), segment_after.dev());
        assert_eq!(segment_before.ino(), segment_after.ino());
        assert_ne!(control_before.ino(), control_after.ino());
        assert_eq!(fs::read(cloned_control).unwrap(), b"mutable control");
        assert_eq!(fallback_copy_bytes, control_before.len());
    }

    #[test]
    fn required_mmap_detachment_is_not_charged_to_fallback_copy_budget() {
        let over_budget = MAX_CHECKPOINT_FALLBACK_COPY_BYTES + 1;

        assert_eq!(
            checkpoint_fallback_copy_total(7, over_budget, true).unwrap(),
            7,
            "mandatory Windows mmap detachment must remain possible for large sidecars"
        );
        assert!(checkpoint_fallback_copy_total(0, over_budget, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn publication_directory_sync_rejects_nested_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let generation = dir.path().join("generation");
        let nested = generation.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(generation.join("artifact.bin"), b"durable").unwrap();
        symlink(generation.join("artifact.bin"), nested.join("escape.bin")).unwrap();

        let error = sync_publication_directories(&generation).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("symlinked index artifact"));
    }
}
