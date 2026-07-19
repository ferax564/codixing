pub mod qdrant;

use std::collections::HashMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(feature = "usearch")]
use usearch::{Index, IndexOptions, MetricKind, ScalarKind, new_index};

use crate::error::{CodixingError, Result};

const VECTOR_GENERATION_FORMAT: u32 = 1;
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
    fs::create_dir_all(&parent)?;
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

    /// Brute-force vector index using cosine similarity.
    ///
    /// Drop-in replacement for the usearch-backed `VectorIndex` when the
    /// `usearch` feature is disabled (e.g. on Windows where usearch uses
    /// POSIX `MAP_FAILED`). O(N) per query but works on all platforms.
    pub struct VectorIndex {
        /// Per-chunk vectors, keyed by chunk ID.
        entries: Vec<(u64, Vec<f32>)>,
        /// Maps file path -> list of chunk IDs stored in this index.
        file_chunks: HashMap<String, Vec<u64>>,
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
                file_chunks: HashMap::new(),
                dims,
            })
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
            // Update existing entry or push new one. When an ID is updated,
            // remove every stale file association before recording its new
            // owner; otherwise tracked IDs can outnumber actual vectors and
            // make a checkpoint internally inconsistent.
            let replaced =
                if let Some(entry) = self.entries.iter_mut().find(|(id, _)| *id == chunk_id) {
                    entry.1 = vector.to_vec();
                    true
                } else {
                    self.entries.push((chunk_id, vector.to_vec()));
                    false
                };
            if replaced {
                self.file_chunks.retain(|_, ids| {
                    ids.retain(|id| *id != chunk_id);
                    !ids.is_empty()
                });
            }
            self.file_chunks
                .entry(file_path.to_string())
                .or_default()
                .push(chunk_id);
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

            const BRUTE_FORCE_WARN_THRESHOLD: usize = 50_000;
            if self.entries.len() > BRUTE_FORCE_WARN_THRESHOLD {
                tracing::warn!(
                    count = self.entries.len(),
                    "brute-force vector search over {} vectors — consider enabling the \
                     `usearch` feature for sub-linear ANN search",
                    self.entries.len()
                );
            }

            let mut scored: Vec<(u64, f32)> = self
                .entries
                .iter()
                .map(|(id, vec)| {
                    let sim = cosine_similarity(query, vec);
                    // Convert to distance: usearch returns cosine distance, not similarity.
                    (*id, 1.0 - sim)
                })
                .collect();

            // Sort by ascending distance (lowest = most similar).
            scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            scored.truncate(k);
            Ok(scored)
        }

        /// Remove all vectors belonging to the given file.
        pub fn remove_file(&mut self, file_path: &str) -> Result<()> {
            if let Some(chunk_ids) = self.file_chunks.remove(file_path) {
                let id_set: std::collections::HashSet<u64> = chunk_ids.into_iter().collect();
                self.entries.retain(|(id, _)| !id_set.contains(id));
            }
            Ok(())
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
            publish_generation(
                index_path,
                file_chunks_path,
                &self.file_chunks,
                self.len(),
                |generation_index_path| {
                    // Save vectors as JSON for cross-platform portability.
                    let data = serde_json::json!({
                        "type": "brute_force",
                        "dims": self.dims,
                        "entries": self.entries.iter().map(|(id, vec)| {
                            serde_json::json!({ "chunk_id": id, "vector": vec })
                        }).collect::<Vec<_>>(),
                    });
                    let bytes = serde_json::to_vec(&data).map_err(|error| {
                        CodixingError::VectorIndex(format!(
                            "failed to serialize vector index: {error}"
                        ))
                    })?;
                    write_new_file(generation_index_path, &bytes)
                },
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
                    let bytes = fs::read(idx_path)?;
                    let data: serde_json::Value =
                        serde_json::from_slice(&bytes).map_err(|error| {
                            CodixingError::VectorIndex(format!(
                                "failed to deserialize vector index: {error}"
                            ))
                        })?;

                    let mut entries = Vec::new();
                    if let Some(arr) = data["entries"].as_array() {
                        for entry in arr {
                            let chunk_id = entry["chunk_id"].as_u64().unwrap_or(0);
                            let vector: Vec<f32> = entry["vector"]
                                .as_array()
                                .map(|a| {
                                    a.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect()
                                })
                                .unwrap_or_default();
                            if vector.len() != dims {
                                return Err(CodixingError::VectorIndex(format!(
                                    "persisted vector dimension mismatch: expected {dims}, got {}",
                                    vector.len()
                                )));
                            }
                            entries.push((chunk_id, vector));
                        }
                    }
                    let fc_bytes = fs::read(fc_path)?;
                    let file_chunks: HashMap<String, Vec<u64>> = bitcode::deserialize(&fc_bytes)
                        .map_err(|error| {
                            CodixingError::Serialization(format!(
                                "failed to deserialize file_chunks: {error}"
                            ))
                        })?;
                    validate_vector_counts(entries.len(), &file_chunks, expected)?;

                    Ok(Self {
                        entries,
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
        /// Returns `None` if the chunk is not in the index.
        pub fn get_vector(&self, chunk_id: u64) -> Option<Vec<f32>> {
            self.entries
                .iter()
                .find(|(id, _)| *id == chunk_id)
                .map(|(_, vec)| vec.clone())
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

    /// Cosine similarity between two vectors.
    ///
    /// Returns a value in [-1.0, 1.0]. Returns 0.0 if either vector has zero magnitude.
    fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let mut dot = 0.0f32;
        let mut norm_a = 0.0f32;
        let mut norm_b = 0.0f32;
        for (x, y) in a.iter().zip(b.iter()) {
            dot += x * y;
            norm_a += x * x;
            norm_b += y * y;
        }
        let denom = norm_a.sqrt() * norm_b.sqrt();
        if denom == 0.0 { 0.0 } else { dot / denom }
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
    fn concurrent_publishers_do_not_delete_unpublished_generation() {
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
                    index_written_tx.send(()).unwrap();
                    resume_rx.recv().unwrap();
                    Ok(())
                },
            )
        });

        // Publisher A has written its index but has not published a manifest.
        // Publisher B completes an entire save in that window. A live cleanup
        // sweep here would unlink A's index and make its subsequent sync fail.
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
