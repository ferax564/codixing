//! Trigram index for fast exact substring search over code chunks.
//!
//! Builds an inverted index mapping 3-byte substrings (trigrams) to chunk IDs.
//! Search intersects posting lists for all query trigrams, then verifies exact
//! matches in the original text. This provides O(1) candidate filtering for
//! exact substring queries.
//!
//! Also exposes [`FileTrigramIndex`] — a file-level variant used by
//! [`crate::engine::Engine::grep_code`] to skip files that cannot possibly
//! match before doing any disk I/O, [`QueryPlan`] for boolean trigram
//! queries with OR support, and [`build_query_plan`] to decompose a regex
//! pattern into a query plan using the Russ Cox / trigrep technique.

use std::collections::HashMap;
use std::path::Path;

use memmap2::Mmap;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

use crate::error::{CodixingError, Result};

// ── Mmap format constants ────────────────────────────────────────────────────

/// Magic bytes for the mmap trigram format: "TRGM" as little-endian u32.
const MMAP_MAGIC: u32 = 0x5452474D;

/// Legacy v1 mmap format version (raw u64 postings).
const MMAP_VERSION_V1: u32 = 1;

/// v0.37 v2 mmap format version (delta+varint or roaring postings, u32 IDs).
const MMAP_VERSION_V2: u32 = 2;

/// Default constant for v1 writes — kept for the original [`TrigramIndex::save_mmap_binary`] path.
const MMAP_VERSION: u32 = MMAP_VERSION_V1;

/// v1 header size: magic(4) + version(4) + trigram_count(4) + chunk_count(4) + total_postings(4).
const MMAP_HEADER_SIZE: usize = 20;

/// v2 header size: v1 header + encoding_flags(4).
const MMAP_V2_HEADER_SIZE: usize = 24;

/// Size of one v1 trigram index entry: trigram(3) + pad(1) + posting_start(4) + posting_count(4).
const MMAP_ENTRY_SIZE: usize = 12;

/// Size of one v2 trigram index entry: trigram(3) + pad(1) + posting_byte_off(4)
/// + posting_count(4) + posting_byte_size(4).
const MMAP_V2_ENTRY_SIZE: usize = 16;

/// v2 encoding flag bit selecting the posting codec (0 = DeltaVarint, 1 = Roaring).
const ENCODING_FLAG_CODEC_BIT: u32 = 0x1;

/// Posting-list codec used by the v2 mmap format.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum PostingCodec {
    /// Delta-encoded chunk IDs serialized as unsigned LEB128 (varint) bytes.
    /// Default for v0.37 — proven scheme from Russ Cox's codesearch.
    #[default]
    DeltaVarint,
    /// Roaring bitmap (`roaring::RoaringBitmap` serialized format).
    Roaring,
}

impl PostingCodec {
    fn to_flag_bits(self) -> u32 {
        match self {
            PostingCodec::DeltaVarint => 0,
            PostingCodec::Roaring => ENCODING_FLAG_CODEC_BIT,
        }
    }

    fn from_flag_bits(flags: u32) -> Self {
        if flags & ENCODING_FLAG_CODEC_BIT != 0 {
            PostingCodec::Roaring
        } else {
            PostingCodec::DeltaVarint
        }
    }
}

/// Memory-mapped backing store for zero-deserialization trigram loading.
struct MmapBacking {
    mmap: Mmap,
    trigram_count: u32,
    index_offset: usize,
    postings_offset: usize,
    /// File format version (1 or 2).
    version: u32,
    /// Posting codec for v2 backings (unused/`DeltaVarint` for v1).
    codec: PostingCodec,
    /// Per-entry stride for the trigram index section. Cached for perf:
    /// `MMAP_ENTRY_SIZE` (12) for v1, `MMAP_V2_ENTRY_SIZE` (16) for v2.
    entry_size: usize,
}

/// Serializable representation of the trigram index data (v2: no content).
#[derive(Serialize, Deserialize)]
struct TrigramIndexData {
    /// Posting lists keyed by trigram bytes.
    index: Vec<([u8; 3], Vec<u64>)>,
}

/// Legacy format with content (for backward-compatible loading).
#[derive(Deserialize)]
struct TrigramIndexDataLegacy {
    index: Vec<([u8; 3], Vec<u64>)>,
    #[allow(dead_code)]
    chunks: Vec<(u64, String)>,
}

/// An inverted index mapping 3-byte substrings to chunk IDs for fast exact search.
///
/// Content is NOT stored here — callers must verify candidate matches using
/// an external content source (chunk_meta or Tantivy stored fields).
///
/// Supports two storage backends:
/// - **In-memory** (`HashMap`): used during index construction and bitcode-loaded indexes.
/// - **Memory-mapped** (`MmapBacking`): zero-deserialization load from the mmap binary format.
///   Eliminates the 55-second cold start on large repos (e.g. 175 MB chunk_trigram.bin).
pub struct TrigramIndex {
    /// Mapping from trigram to sorted list of chunk IDs containing that trigram.
    /// Empty when `mmap` is `Some` (search dispatches to the mmap path).
    index: HashMap<[u8; 3], Vec<u64>>,
    /// Number of distinct chunks indexed (for len/is_empty).
    chunk_count: usize,
    /// Memory-mapped backing for zero-copy search (set by `load_mmap`).
    mmap: Option<MmapBacking>,
}

impl TrigramIndex {
    /// Creates a new empty trigram index.
    pub fn new() -> Self {
        Self {
            index: HashMap::new(),
            chunk_count: 0,
            mmap: None,
        }
    }

    /// Materialize mmap-backed data into the in-memory HashMap for mutation.
    ///
    /// Called automatically before any mutating operation (add, remove, save).
    /// No-op if already in-memory.
    fn ensure_mutable(&mut self) {
        if let Some(backing) = self.mmap.take() {
            let data = &backing.mmap[..];
            self.index.reserve(backing.trigram_count as usize);
            for i in 0..backing.trigram_count as usize {
                let off = backing.index_offset + i * backing.entry_size;
                let trigram = [data[off], data[off + 1], data[off + 2]];

                let ids: Vec<u64> = if backing.version == MMAP_VERSION_V1 {
                    let start =
                        u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
                    let count =
                        u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()) as usize;

                    let posting_base = backing.postings_offset + start * 8;
                    (0..count)
                        .map(|j| {
                            let o = posting_base + j * 8;
                            u64::from_le_bytes(data[o..o + 8].try_into().unwrap())
                        })
                        .collect()
                } else {
                    // v2: posting_byte_off / posting_count / posting_byte_size.
                    let byte_off =
                        u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
                    let count =
                        u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()) as usize;
                    let byte_size =
                        u32::from_le_bytes(data[off + 12..off + 16].try_into().unwrap()) as usize;
                    let blob_start = backing.postings_offset + byte_off;
                    let blob_end = blob_start + byte_size;
                    if blob_end > data.len() {
                        Vec::new()
                    } else {
                        let blob = &data[blob_start..blob_end];
                        let u32s =
                            decode_posting_blob(blob, count, backing.codec).unwrap_or_default();
                        u32s.into_iter().map(u64::from).collect()
                    }
                };

                self.index.insert(trigram, ids);
            }
            // backing (and its Mmap) is dropped here
        }
    }

    /// Adds a chunk to the index. Extracts all trigrams from the content and
    /// updates posting lists. Content shorter than 3 bytes produces no trigrams.
    ///
    /// For bulk loading, prefer [`build_batch`] which defers sorting to the end.
    pub fn add(&mut self, chunk_id: u64, content: &str) {
        self.ensure_mutable();
        self.chunk_count += 1;
        self.add_trigrams(chunk_id, content.as_bytes());
        // Sort + dedup posting lists touched by this chunk.
        let bytes = content.as_bytes();
        if bytes.len() < 3 {
            return;
        }
        for i in 0..bytes.len() - 2 {
            let tri = [bytes[i], bytes[i + 1], bytes[i + 2]];
            if let Some(list) = self.index.get_mut(&tri) {
                list.sort_unstable();
                list.dedup();
            }
        }
    }

    /// Insert trigrams for a chunk without sorting posting lists.
    fn add_trigrams(&mut self, chunk_id: u64, bytes: &[u8]) {
        if bytes.len() < 3 {
            return;
        }
        let mut chunk_trigrams: Vec<[u8; 3]> = (0..bytes.len() - 2)
            .map(|i| [bytes[i], bytes[i + 1], bytes[i + 2]])
            .collect();
        chunk_trigrams.sort_unstable();
        chunk_trigrams.dedup();
        for trigram in chunk_trigrams {
            self.index.entry(trigram).or_default().push(chunk_id);
        }
    }

    /// Bulk-build the trigram index from an iterator of (chunk_id, content) pairs.
    ///
    /// Much faster than calling `add()` repeatedly because posting lists are
    /// sorted and deduplicated only once at the end, avoiding O(N²) re-sorting.
    pub fn build_batch(&mut self, chunks: impl Iterator<Item = (u64, impl AsRef<str>)>) {
        let mut count = 0usize;
        for (chunk_id, content) in chunks {
            self.add_trigrams(chunk_id, content.as_ref().as_bytes());
            count += 1;
        }
        self.chunk_count += count;
        // Single sort+dedup pass over all posting lists.
        for list in self.index.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
    }

    /// Removes a chunk from the index, cleaning up all posting list entries.
    ///
    /// The caller must provide the chunk's content so we know which trigrams
    /// to clean up from the posting lists.
    pub fn remove(&mut self, chunk_id: u64, content: &str) {
        self.ensure_mutable();
        let bytes = content.as_bytes();
        if bytes.len() < 3 {
            self.chunk_count = self.chunk_count.saturating_sub(1);
            return;
        }
        for i in 0..bytes.len() - 2 {
            let trigram = [bytes[i], bytes[i + 1], bytes[i + 2]];
            if let Some(list) = self.index.get_mut(&trigram) {
                list.retain(|&id| id != chunk_id);
                if list.is_empty() {
                    self.index.remove(&trigram);
                }
            }
        }
        self.chunk_count = self.chunk_count.saturating_sub(1);
    }

    /// Returns candidate chunk IDs that may contain `query` as a substring.
    ///
    /// Intersects trigram posting lists for fast candidate filtering.
    /// **Callers must verify** actual substring matches in the chunk content
    /// (from chunk_meta or Tantivy stored fields) since trigram intersection
    /// can produce false positives.
    ///
    /// Returns empty if the query is shorter than 3 bytes.
    pub fn search(&self, query: &str) -> Vec<u64> {
        if let Some(ref backing) = self.mmap {
            return Self::mmap_search(backing, query);
        }
        Self::inmemory_search(&self.index, query)
    }

    /// In-memory search: intersect HashMap posting lists.
    fn inmemory_search(index: &HashMap<[u8; 3], Vec<u64>>, query: &str) -> Vec<u64> {
        let query_bytes = query.as_bytes();
        if query_bytes.len() < 3 {
            return Vec::new();
        }

        let mut trigrams = Vec::with_capacity(query_bytes.len() - 2);
        for i in 0..query_bytes.len() - 2 {
            trigrams.push([query_bytes[i], query_bytes[i + 1], query_bytes[i + 2]]);
        }

        let mut posting_lists: Vec<&Vec<u64>> =
            trigrams.iter().filter_map(|t| index.get(t)).collect();
        if posting_lists.len() != trigrams.len() {
            return Vec::new();
        }

        posting_lists.sort_by_key(|l| l.len());
        let mut candidates = posting_lists[0].clone();
        for list in &posting_lists[1..] {
            candidates.retain(|id| list.binary_search(id).is_ok());
            if candidates.is_empty() {
                break;
            }
        }

        candidates
    }

    /// Mmap-backed search: binary-search the sorted trigram index, read posting
    /// lists directly from mapped memory.
    fn mmap_search(backing: &MmapBacking, query: &str) -> Vec<u64> {
        let query_bytes = query.as_bytes();
        if query_bytes.len() < 3 {
            return Vec::new();
        }

        let mut trigrams = Vec::with_capacity(query_bytes.len() - 2);
        for i in 0..query_bytes.len() - 2 {
            trigrams.push([query_bytes[i], query_bytes[i + 1], query_bytes[i + 2]]);
        }

        // For each query trigram, binary-search the index to find its posting
        // list location. The two `u32` payload words mean different things in
        // v1 vs v2 — see `mmap_read_posting_list` / `mmap_read_posting_list_v2`.
        struct PostingRef {
            payload_a: u32,
            payload_b: u32,
            count: u32,
        }
        let mut posting_refs: Vec<PostingRef> = Vec::with_capacity(trigrams.len());
        for tri in &trigrams {
            match Self::mmap_lookup_trigram(backing, tri) {
                Some((a, b, count)) => posting_refs.push(PostingRef {
                    payload_a: a,
                    payload_b: b,
                    count,
                }),
                None => return Vec::new(), // trigram absent → no matches
            }
        }

        // Intersect posting lists, starting from the shortest.
        posting_refs.sort_by_key(|r| r.count);

        let read = |pr: &PostingRef| -> Vec<u64> {
            if backing.version == MMAP_VERSION_V1 {
                Self::mmap_read_posting_list(backing, pr.payload_a, pr.count)
            } else {
                Self::mmap_read_posting_list_v2(backing, pr.payload_a, pr.count, pr.payload_b)
                    .into_iter()
                    .map(u64::from)
                    .collect()
            }
        };

        let mut candidates = read(&posting_refs[0]);
        for pr in &posting_refs[1..] {
            let list = read(pr);
            candidates.retain(|id| list.binary_search(id).is_ok());
            if candidates.is_empty() {
                break;
            }
        }

        candidates
    }

    /// Binary-search the mmap trigram index for a specific trigram.
    ///
    /// Returns `(payload_a, payload_b, count)` if found, where the meaning of
    /// `payload_a`/`payload_b` depends on the format version:
    ///
    /// - **v1**: `payload_a = posting_start` (u64-units offset), `payload_b = 0`.
    /// - **v2**: `payload_a = posting_byte_off`, `payload_b = posting_byte_size`.
    ///
    /// `count` is always the logical number of chunk IDs that decode from the
    /// posting list.
    fn mmap_lookup_trigram(backing: &MmapBacking, trigram: &[u8; 3]) -> Option<(u32, u32, u32)> {
        let data = &backing.mmap[..];
        let count = backing.trigram_count as usize;
        let base = backing.index_offset;
        let entry_size = backing.entry_size;

        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = base + mid * entry_size;
            if off + entry_size > data.len() {
                return None; // corrupted index
            }
            let key = [data[off], data[off + 1], data[off + 2]];
            match key.cmp(trigram) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let a = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
                    let cnt = u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap());
                    let b = if backing.version == MMAP_VERSION_V1 {
                        0u32
                    } else {
                        u32::from_le_bytes(data[off + 12..off + 16].try_into().unwrap())
                    };
                    return Some((a, b, cnt));
                }
            }
        }
        None
    }

    /// Read a v1 posting list from the mmap postings section.
    ///
    /// Returns empty if the requested range exceeds the mmap bounds
    /// (corrupted or partially written index).
    fn mmap_read_posting_list(backing: &MmapBacking, start: u32, count: u32) -> Vec<u64> {
        let data = &backing.mmap[..];
        let base = backing.postings_offset + (start as usize) * 8;
        let end = base + (count as usize) * 8;
        if end > data.len() {
            return Vec::new();
        }
        (0..count as usize)
            .map(|i| {
                let off = base + i * 8;
                u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
            })
            .collect()
    }

    /// Read a v2 posting list (variable-length codec blob) from the mmap
    /// postings section. Returns empty on truncation or decode failure.
    fn mmap_read_posting_list_v2(
        backing: &MmapBacking,
        byte_off: u32,
        count: u32,
        byte_size: u32,
    ) -> Vec<u32> {
        let data = &backing.mmap[..];
        let start = backing.postings_offset + byte_off as usize;
        let end = start + byte_size as usize;
        if end > data.len() {
            return Vec::new();
        }
        let blob = &data[start..end];
        decode_posting_blob(blob, count as usize, backing.codec).unwrap_or_default()
    }

    /// Returns the number of indexed chunks.
    pub fn len(&self) -> usize {
        self.chunk_count
    }

    /// Returns true if the index contains no chunks.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Save the trigram index to a binary (bitcode) file.
    pub fn save_binary(&self, path: &Path) -> Result<()> {
        // If still mmap-backed, no mutations happened — file on disk is current.
        if self.mmap.is_some() {
            return Ok(());
        }
        let data = TrigramIndexData {
            index: self.index.iter().map(|(k, v)| (*k, v.clone())).collect(),
        };
        let bytes = bitcode::serialize(&data).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize trigram index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Save the trigram index in the mmap-friendly binary format.
    ///
    /// The format is designed for zero-deserialization loading via memory mapping:
    /// sorted trigram entries with offsets into a contiguous postings array.
    /// Load time drops from ~55s (bitcode + HashMap rebuild) to near-zero.
    ///
    /// ## Binary format
    ///
    /// ```text
    /// [Header: 20 bytes, little-endian]
    ///   magic:          u32 = 0x5452474D ("TRGM")
    ///   version:        u32 = 1
    ///   trigram_count:  u32
    ///   chunk_count:    u32
    ///   total_postings: u32
    ///
    /// [Trigram Index: trigram_count × 12 bytes, sorted by trigram bytes]
    ///   trigram:        [u8; 3]
    ///   _pad:           u8
    ///   posting_start:  u32 (index into postings array, in u64 units)
    ///   posting_count:  u32
    ///
    /// [Postings: total_postings × 8 bytes]
    ///   Contiguous u64 chunk IDs (sorted within each list)
    /// ```
    pub fn save_mmap_binary(&self, path: &Path) -> Result<()> {
        // If still mmap-backed, no mutations happened — file on disk is current.
        if self.mmap.is_some() {
            return Ok(());
        }
        // Sort trigrams for binary search at load time.
        let mut entries: Vec<([u8; 3], &Vec<u64>)> =
            self.index.iter().map(|(k, v)| (*k, v)).collect();
        entries.sort_by_key(|(k, _)| *k);

        let trigram_count = entries.len() as u32;
        let total_postings: u32 = entries.iter().map(|(_, v)| v.len() as u32).sum();

        let total_size = MMAP_HEADER_SIZE
            + (trigram_count as usize) * MMAP_ENTRY_SIZE
            + (total_postings as usize) * 8;
        let mut buf = Vec::with_capacity(total_size);

        // Header.
        buf.extend_from_slice(&MMAP_MAGIC.to_le_bytes());
        buf.extend_from_slice(&MMAP_VERSION.to_le_bytes());
        buf.extend_from_slice(&trigram_count.to_le_bytes());
        buf.extend_from_slice(&(self.chunk_count as u32).to_le_bytes());
        buf.extend_from_slice(&total_postings.to_le_bytes());

        // Trigram index entries.
        let mut posting_offset = 0u32;
        for (trigram, ids) in &entries {
            buf.extend_from_slice(trigram);
            buf.push(0); // padding
            buf.extend_from_slice(&posting_offset.to_le_bytes());
            buf.extend_from_slice(&(ids.len() as u32).to_le_bytes());
            posting_offset += ids.len() as u32;
        }

        // Postings.
        for (_, ids) in &entries {
            for &id in *ids {
                buf.extend_from_slice(&id.to_le_bytes());
            }
        }

        std::fs::write(path, buf)?;
        Ok(())
    }

    /// Save the trigram index in the **v2** mmap-friendly binary format with
    /// the chosen posting [`PostingCodec`].
    ///
    /// v2 stores chunk IDs as `u32` (verified to fit on the Linux kernel:
    /// 881K chunks ≪ 2³²) and replaces the fixed-stride 8-bytes-per-id
    /// posting layout with a variable-length, codec-tagged blob per trigram.
    ///
    /// ## Binary format
    ///
    /// ```text
    /// [Header: 24 bytes, little-endian]
    ///   magic:           u32 = 0x5452474D ("TRGM")
    ///   version:         u32 = 2
    ///   trigram_count:   u32
    ///   chunk_count:     u32
    ///   total_postings:  u32   (sum of posting_count across all trigrams)
    ///   encoding_flags:  u32
    ///     bit 0:  codec (0 = DeltaVarint, 1 = Roaring)
    ///     bits 1-31: reserved, must be zero
    ///
    /// [Trigram Index: trigram_count × 16 bytes, sorted by trigram bytes]
    ///   trigram:            [u8; 3]
    ///   _pad:               u8
    ///   posting_byte_off:   u32   (byte offset into postings section)
    ///   posting_count:      u32   (logical IDs decoded from this blob)
    ///   posting_byte_size:  u32   (length in bytes of this trigram's blob)
    ///
    /// [Postings]
    ///   Variable-length codec blob per trigram, contiguous.
    /// ```
    pub fn save_mmap_binary_v2(&self, path: &Path, codec: PostingCodec) -> Result<()> {
        // If still mmap-backed *and* the on-disk file is already v2 with the
        // requested codec, we could skip — but `mmap.is_some()` only proves
        // the file wasn't mutated, not its format. The previous v1 fast-path
        // assumed the file matched the writer; for the v2 transition we always
        // re-write so a v1 file on disk gets upgraded.
        let mut owner: Option<TrigramIndex>;
        let entries: Vec<([u8; 3], Vec<u32>)> = if self.mmap.is_some() {
            // Materialize without mutating self by cloning into a local index.
            let mut tmp = TrigramIndex::new();
            // Reuse load_binary's mmap data via a fresh ensure_mutable on a clone:
            // simpler — just walk the mmap directly here.
            let backing = self.mmap.as_ref().unwrap();
            tmp.chunk_count = self.chunk_count;
            let data = &backing.mmap[..];
            for i in 0..backing.trigram_count as usize {
                let off = backing.index_offset + i * backing.entry_size;
                let trigram = [data[off], data[off + 1], data[off + 2]];
                let ids: Vec<u64> = if backing.version == MMAP_VERSION_V1 {
                    let start =
                        u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
                    let count =
                        u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()) as usize;
                    let posting_base = backing.postings_offset + start * 8;
                    (0..count)
                        .map(|j| {
                            let o = posting_base + j * 8;
                            u64::from_le_bytes(data[o..o + 8].try_into().unwrap())
                        })
                        .collect()
                } else {
                    let byte_off =
                        u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()) as usize;
                    let count =
                        u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()) as usize;
                    let byte_size =
                        u32::from_le_bytes(data[off + 12..off + 16].try_into().unwrap()) as usize;
                    let blob_start = backing.postings_offset + byte_off;
                    let blob_end = blob_start + byte_size;
                    if blob_end > data.len() {
                        Vec::new()
                    } else {
                        decode_posting_blob(&data[blob_start..blob_end], count, backing.codec)
                            .unwrap_or_default()
                            .into_iter()
                            .map(u64::from)
                            .collect()
                    }
                };
                tmp.index.insert(trigram, ids);
            }
            owner = Some(tmp);
            let owned = owner.as_ref().unwrap();
            let mut entries: Vec<([u8; 3], Vec<u32>)> = owned
                .index
                .iter()
                .map(|(k, v)| (*k, v.iter().map(|&id| id as u32).collect()))
                .collect();
            entries.sort_by_key(|(k, _)| *k);
            entries
        } else {
            owner = None;
            let mut entries: Vec<([u8; 3], Vec<u32>)> = self
                .index
                .iter()
                .map(|(k, v)| (*k, v.iter().map(|&id| id as u32).collect()))
                .collect();
            entries.sort_by_key(|(k, _)| *k);
            entries
        };
        // Drop unused variable to silence warnings on the no-mmap path.
        let _ = owner.take();

        // Encode each posting list according to the chosen codec.
        let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
        let mut total_blob_bytes: usize = 0;
        let mut total_postings: u32 = 0;
        for (_tri, ids) in &entries {
            let blob = encode_posting_blob(ids, codec);
            total_blob_bytes += blob.len();
            total_postings += ids.len() as u32;
            blobs.push(blob);
        }

        let trigram_count = entries.len() as u32;
        let total_size =
            MMAP_V2_HEADER_SIZE + (trigram_count as usize) * MMAP_V2_ENTRY_SIZE + total_blob_bytes;
        let mut buf = Vec::with_capacity(total_size);

        // Header.
        buf.extend_from_slice(&MMAP_MAGIC.to_le_bytes());
        buf.extend_from_slice(&MMAP_VERSION_V2.to_le_bytes());
        buf.extend_from_slice(&trigram_count.to_le_bytes());
        buf.extend_from_slice(&(self.chunk_count as u32).to_le_bytes());
        buf.extend_from_slice(&total_postings.to_le_bytes());
        buf.extend_from_slice(&codec.to_flag_bits().to_le_bytes());

        // Trigram index entries — write fixed-stride entries with byte
        // offsets/sizes that point into the postings section that follows.
        let mut byte_off: u32 = 0;
        for ((trigram, ids), blob) in entries.iter().zip(blobs.iter()) {
            buf.extend_from_slice(trigram);
            buf.push(0); // padding
            buf.extend_from_slice(&byte_off.to_le_bytes());
            buf.extend_from_slice(&(ids.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(blob.len() as u32).to_le_bytes());
            byte_off += blob.len() as u32;
        }

        // Postings (codec blobs).
        for blob in &blobs {
            buf.extend_from_slice(blob);
        }

        std::fs::write(path, buf)?;
        Ok(())
    }

    /// Load the trigram index from a binary file.
    ///
    /// Detects the format by peeking at magic bytes:
    /// - `TRGM` magic → mmap format (zero-copy, near-instant load)
    /// - Otherwise → bitcode deserialization (legacy/v2)
    pub fn load_binary(path: &Path) -> Result<Self> {
        // Peek at first 4 bytes to detect format.
        let mut magic = [0u8; 4];
        {
            use std::io::Read;
            let mut f = std::fs::File::open(path)?;
            if f.read(&mut magic).unwrap_or(0) == 4 && magic == MMAP_MAGIC.to_le_bytes() {
                return Self::load_mmap(path);
            }
        }

        // Fall back to bitcode deserialization.
        let bytes = std::fs::read(path)?;
        if let Ok(data) = bitcode::deserialize::<TrigramIndexData>(&bytes) {
            let chunk_count = data
                .index
                .iter()
                .flat_map(|(_, ids)| ids.iter())
                .collect::<std::collections::HashSet<_>>()
                .len();
            Ok(Self {
                index: data.index.into_iter().collect(),
                chunk_count,
                mmap: None,
            })
        } else if let Ok(data) = bitcode::deserialize::<TrigramIndexDataLegacy>(&bytes) {
            let chunk_count = data
                .index
                .iter()
                .flat_map(|(_, ids)| ids.iter())
                .collect::<std::collections::HashSet<_>>()
                .len();
            Ok(Self {
                index: data.index.into_iter().collect(),
                chunk_count,
                mmap: None,
            })
        } else {
            Err(CodixingError::Serialization(
                "failed to deserialize trigram index: unknown format".to_string(),
            ))
        }
    }

    /// Load the trigram index via memory mapping (zero deserialization).
    ///
    /// Dispatches on the `version` field in the header. Both v1 (raw u64
    /// postings) and v2 (codec-tagged variable-length blobs) are supported.
    fn load_mmap(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let file_len = file.metadata()?.len() as usize;

        if file_len < MMAP_HEADER_SIZE {
            return Err(CodixingError::Serialization(
                "trigram mmap file too small for header".to_string(),
            ));
        }

        // SAFETY: Read-only Mmap over a file we opened read-only.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| {
            CodixingError::Serialization(format!("failed to mmap trigram file: {e}"))
        })?;

        let magic = u32::from_le_bytes(mmap[0..4].try_into().unwrap());
        if magic != MMAP_MAGIC {
            return Err(CodixingError::Serialization(format!(
                "invalid trigram magic: expected 0x{MMAP_MAGIC:08X}, got 0x{magic:08X}"
            )));
        }
        let version = u32::from_le_bytes(mmap[4..8].try_into().unwrap());

        match version {
            MMAP_VERSION_V1 => {
                let trigram_count = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
                let chunk_count = u32::from_le_bytes(mmap[12..16].try_into().unwrap());
                let total_postings = u32::from_le_bytes(mmap[16..20].try_into().unwrap());

                let index_offset = MMAP_HEADER_SIZE;
                let postings_offset = index_offset + (trigram_count as usize) * MMAP_ENTRY_SIZE;
                let expected_size = postings_offset + (total_postings as usize) * 8;

                if file_len < expected_size {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v1 file truncated: expected {expected_size} bytes, got {file_len}"
                    )));
                }

                Ok(Self {
                    index: HashMap::new(),
                    chunk_count: chunk_count as usize,
                    mmap: Some(MmapBacking {
                        mmap,
                        trigram_count,
                        index_offset,
                        postings_offset,
                        version: MMAP_VERSION_V1,
                        codec: PostingCodec::DeltaVarint, // unused in v1 path
                        entry_size: MMAP_ENTRY_SIZE,
                    }),
                })
            }
            MMAP_VERSION_V2 => {
                if file_len < MMAP_V2_HEADER_SIZE {
                    return Err(CodixingError::Serialization(
                        "trigram mmap v2 file too small for header".to_string(),
                    ));
                }
                let trigram_count = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
                let chunk_count = u32::from_le_bytes(mmap[12..16].try_into().unwrap());
                let _total_postings = u32::from_le_bytes(mmap[16..20].try_into().unwrap());
                let encoding_flags = u32::from_le_bytes(mmap[20..24].try_into().unwrap());
                let codec = PostingCodec::from_flag_bits(encoding_flags);

                let index_offset = MMAP_V2_HEADER_SIZE;
                let postings_offset = index_offset + (trigram_count as usize) * MMAP_V2_ENTRY_SIZE;

                if file_len < postings_offset {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v2 file truncated: index extends to {postings_offset} bytes, got {file_len}"
                    )));
                }

                // Compute expected total blob size by walking the index entries
                // and summing posting_byte_size.
                let mut expected_blobs: usize = 0;
                for i in 0..trigram_count as usize {
                    let off = index_offset + i * MMAP_V2_ENTRY_SIZE;
                    let byte_size =
                        u32::from_le_bytes(mmap[off + 12..off + 16].try_into().unwrap()) as usize;
                    expected_blobs += byte_size;
                }
                let expected_size = postings_offset + expected_blobs;
                if file_len < expected_size {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v2 file truncated: expected {expected_size} bytes, got {file_len}"
                    )));
                }

                Ok(Self {
                    index: HashMap::new(),
                    chunk_count: chunk_count as usize,
                    mmap: Some(MmapBacking {
                        mmap,
                        trigram_count,
                        index_offset,
                        postings_offset,
                        version: MMAP_VERSION_V2,
                        codec,
                        entry_size: MMAP_V2_ENTRY_SIZE,
                    }),
                })
            }
            other => Err(CodixingError::Serialization(format!(
                "unsupported trigram mmap version: expected 1 or 2, got {other}"
            ))),
        }
    }
}

// ── v2 codec helpers ─────────────────────────────────────────────────────────

/// Encode a sorted list of u32 chunk IDs into a v2 posting blob using the
/// chosen codec.
fn encode_posting_blob(ids: &[u32], codec: PostingCodec) -> Vec<u8> {
    match codec {
        PostingCodec::DeltaVarint => {
            // ids are expected to be sorted; emit absolute first value then
            // unsigned deltas, each LEB128-encoded.
            let mut buf = Vec::with_capacity(ids.len() * 2);
            let mut last: u32 = 0;
            for &id in ids {
                let delta = id.wrapping_sub(last);
                encode_varint_u32(delta, &mut buf);
                last = id;
            }
            buf
        }
        PostingCodec::Roaring => {
            let mut bm = RoaringBitmap::new();
            for &id in ids {
                bm.insert(id);
            }
            let mut buf = Vec::with_capacity(bm.serialized_size());
            // serialize_into is infallible for Vec<u8>.
            bm.serialize_into(&mut buf)
                .expect("RoaringBitmap::serialize_into into Vec is infallible");
            buf
        }
    }
}

/// Decode a v2 posting blob back into a sorted `Vec<u32>` of chunk IDs.
/// Returns `None` on malformed input.
fn decode_posting_blob(
    blob: &[u8],
    expected_count: usize,
    codec: PostingCodec,
) -> Option<Vec<u32>> {
    match codec {
        PostingCodec::DeltaVarint => {
            let mut out = Vec::with_capacity(expected_count);
            let mut last: u32 = 0;
            let mut pos = 0usize;
            while pos < blob.len() {
                let (delta, consumed) = decode_varint_u32(&blob[pos..])?;
                pos += consumed;
                last = last.wrapping_add(delta);
                out.push(last);
            }
            if out.len() != expected_count {
                return None;
            }
            Some(out)
        }
        PostingCodec::Roaring => {
            let bm = RoaringBitmap::deserialize_from(blob).ok()?;
            let out: Vec<u32> = bm.iter().collect();
            if out.len() != expected_count {
                return None;
            }
            Some(out)
        }
    }
}

/// Encode a `u32` as unsigned LEB128 (varint) into `buf`.
fn encode_varint_u32(mut value: u32, buf: &mut Vec<u8>) {
    while value >= 0x80 {
        buf.push(((value as u8) & 0x7F) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Decode an unsigned LEB128 (varint) `u32` from the start of `data`.
/// Returns `(value, bytes_consumed)`, or `None` if the input is truncated
/// or the encoded value overflows `u32`.
fn decode_varint_u32(data: &[u8]) -> Option<(u32, usize)> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in data.iter().enumerate() {
        if shift >= 32 {
            return None;
        }
        let payload = (byte & 0x7F) as u32;
        // Detect overflow: if we've already accumulated bits beyond the shift
        // window, the value can't fit.
        let shifted = payload.checked_shl(shift)?;
        result |= shifted;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
    }
    None
}

impl Default for TrigramIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ── File-level trigram index ──────────────────────────────────────────────────

/// Serializable representation of the file trigram index.
#[derive(Serialize, Deserialize)]
struct FileTrigramIndexData {
    files: Vec<String>,
    index: Vec<([u8; 3], Vec<u32>)>,
}

/// A boolean query plan over trigrams, produced by [`build_query_plan`].
///
/// Execution semantics (see [`FileTrigramIndex::execute_plan`]):
/// - `MatchAll` → no pre-filtering possible (fall back to full scan)
/// - `Trigrams(t)` → intersect posting lists for all trigrams (AND)
/// - `And(plans)` → intersect candidate sets of all sub-plans
/// - `Or(plans)` → union candidate sets of all sub-plans
#[derive(Debug, Clone)]
pub enum QueryPlan {
    /// No trigrams extractable — must scan all files.
    MatchAll,
    /// A set of trigrams that must ALL appear (leaf AND node).
    Trigrams(Vec<[u8; 3]>),
    /// All sub-plans must match (intersection).
    And(Vec<QueryPlan>),
    /// At least one sub-plan must match (union).
    Or(Vec<QueryPlan>),
}

/// A file-level trigram index for fast grep pre-filtering.
///
/// Maps 3-byte substrings to the set of files whose indexed content contains
/// those trigrams. Used by [`crate::engine::Engine::grep_code`] to skip files
/// that cannot possibly contain a pattern, avoiding unnecessary disk I/O.
///
/// The index is built from chunk content. Trigrams that straddle a chunk
/// boundary are not captured, but this affects < 0.1 % of real-world patterns.
pub struct FileTrigramIndex {
    /// file index → relative path (empty string = tombstoned / removed)
    files: Vec<String>,
    /// path → file index
    file_index: HashMap<String, u32>,
    /// trigram → sorted list of file indices containing that trigram
    index: HashMap<[u8; 3], Vec<u32>>,
}

impl Default for FileTrigramIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTrigramIndex {
    /// Creates an empty index.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            file_index: HashMap::new(),
            index: HashMap::new(),
        }
    }

    /// Index all trigrams from `content` under `path`.
    ///
    /// Safe to call multiple times with the same `path` (e.g. once per chunk):
    /// duplicate `(trigram, file_index)` pairs are deduplicated.
    pub fn add(&mut self, path: &str, content: &[u8]) {
        let file_idx = if let Some(&idx) = self.file_index.get(path) {
            idx
        } else {
            let idx = self.files.len() as u32;
            self.files.push(path.to_string());
            self.file_index.insert(path.to_string(), idx);
            idx
        };

        if content.len() < 3 {
            return;
        }

        // Collect unique trigrams via sort+dedup (avoids per-call HashSet alloc).
        let mut trigrams: Vec<[u8; 3]> = (0..content.len() - 2)
            .map(|i| [content[i], content[i + 1], content[i + 2]])
            .collect();
        trigrams.sort_unstable();
        trigrams.dedup();

        for tri in trigrams {
            let list = self.index.entry(tri).or_default();
            // Maintain sorted order; dedup on insert to avoid duplicates
            // when `add` is called multiple times for the same file.
            if list.last() != Some(&file_idx) {
                match list.binary_search(&file_idx) {
                    Ok(_) => {} // already present
                    Err(pos) => list.insert(pos, file_idx),
                }
            }
        }
    }

    /// Returns candidate file paths for a **literal** pattern.
    ///
    /// Returns `None` when the literal is shorter than 3 bytes and trigram
    /// pre-filtering cannot be applied — the caller should fall back to a full
    /// scan.  Returns `Some([])` when the literal contains a trigram absent
    /// from the index, meaning no file can match.
    pub fn candidates_for_literal<'a>(&'a self, literal: &[u8]) -> Option<Vec<&'a str>> {
        if literal.len() < 3 {
            return None;
        }
        let trigrams: Vec<[u8; 3]> = (0..literal.len() - 2)
            .map(|i| [literal[i], literal[i + 1], literal[i + 2]])
            .collect();

        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(trigrams.len());
        for t in &trigrams {
            match self.index.get(t) {
                Some(l) => lists.push(l),
                None => return Some(Vec::new()), // trigram absent → no matches
            }
        }

        // Intersect posting lists starting from the shortest.
        lists.sort_unstable_by_key(|l| l.len());
        let mut candidates = lists[0].clone();
        for list in &lists[1..] {
            candidates.retain(|id| list.binary_search(id).is_ok());
            if candidates.is_empty() {
                break;
            }
        }

        Some(
            candidates
                .iter()
                .filter_map(|&i| {
                    let p = self.files[i as usize].as_str();
                    if p.is_empty() { None } else { Some(p) }
                })
                .collect(),
        )
    }

    /// Returns candidate file paths given a set of trigrams that **all** must
    /// be present in any matching file (AND semantics).
    ///
    /// Typically fed the output of [`extract_required_trigrams`].
    /// Returns `None` when the trigram set is empty (can't pre-filter).
    pub fn candidates_for_trigrams<'a>(&'a self, trigrams: &[[u8; 3]]) -> Option<Vec<&'a str>> {
        if trigrams.is_empty() {
            return None;
        }

        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(trigrams.len());
        for t in trigrams {
            match self.index.get(t) {
                Some(l) => lists.push(l),
                None => return Some(Vec::new()), // required trigram absent → no match
            }
        }

        lists.sort_unstable_by_key(|l| l.len());
        let mut candidates = lists[0].clone();
        for list in &lists[1..] {
            candidates.retain(|id| list.binary_search(id).is_ok());
            if candidates.is_empty() {
                break;
            }
        }

        Some(
            candidates
                .iter()
                .filter_map(|&i| {
                    let p = self.files[i as usize].as_str();
                    if p.is_empty() { None } else { Some(p) }
                })
                .collect(),
        )
    }

    /// Remove a single file from the index.
    ///
    /// Removes the file from all posting lists and tombstones its entry.
    /// This is O(unique_trigrams_in_index) — fast enough for single-file
    /// operations; batch operations should rebuild from scratch instead.
    pub fn remove_file(&mut self, path: &str) {
        let file_idx = match self.file_index.remove(path) {
            Some(idx) => idx,
            None => return,
        };
        // Tombstone the files entry.
        self.files[file_idx as usize] = String::new();
        // Remove from all posting lists; drop empty ones.
        self.index.retain(|_, list| {
            list.retain(|&id| id != file_idx);
            !list.is_empty()
        });
    }

    /// Execute a [`QueryPlan`] against this index.
    ///
    /// Returns `None` when the plan is `MatchAll` (can't pre-filter).
    /// Otherwise returns the set of candidate file paths.
    pub fn execute_plan<'a>(&'a self, plan: &QueryPlan) -> Option<Vec<&'a str>> {
        match plan {
            QueryPlan::MatchAll => None,
            QueryPlan::Trigrams(tris) => self.candidates_for_trigrams(tris),
            QueryPlan::And(subs) => {
                // Intersect all sub-plan results.  Skip MatchAll branches.
                let mut result: Option<std::collections::HashSet<&str>> = None;
                for sub in subs {
                    match self.execute_plan(sub) {
                        None => continue, // MatchAll → doesn't constrain
                        Some(candidates) => {
                            let set: std::collections::HashSet<&str> =
                                candidates.into_iter().collect();
                            result = Some(match result {
                                None => set,
                                Some(acc) => acc.intersection(&set).copied().collect(),
                            });
                        }
                    }
                }
                result.map(|s| s.into_iter().collect())
            }
            QueryPlan::Or(subs) => {
                // Union all sub-plan results.  If ANY branch is MatchAll,
                // the whole OR matches everything.
                let mut result: std::collections::HashSet<&str> = std::collections::HashSet::new();
                for sub in subs {
                    match self.execute_plan(sub) {
                        None => return None, // MatchAll branch → can't pre-filter
                        Some(candidates) => result.extend(candidates),
                    }
                }
                Some(result.into_iter().collect())
            }
        }
    }

    /// Number of files currently in the index.
    pub fn file_count(&self) -> usize {
        self.file_index.len()
    }

    /// Save the file trigram index to a binary (bitcode) file.
    pub fn save_binary(&self, path: &Path) -> Result<()> {
        let data = FileTrigramIndexData {
            files: self.files.clone(),
            index: self.index.iter().map(|(k, v)| (*k, v.clone())).collect(),
        };
        let bytes = bitcode::serialize(&data).map_err(|e| {
            CodixingError::Serialization(format!("failed to serialize file trigram index: {e}"))
        })?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Load the file trigram index from a binary (bitcode) file.
    pub fn load_binary(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let data: FileTrigramIndexData = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize file trigram index: {e}"))
        })?;
        let mut file_index = HashMap::new();
        for (i, f) in data.files.iter().enumerate() {
            if !f.is_empty() {
                file_index.insert(f.clone(), i as u32);
            }
        }
        Ok(Self {
            files: data.files,
            file_index,
            index: data.index.into_iter().collect(),
        })
    }
}

// ── Regex → QueryPlan (Russ Cox / trigrep technique) ─────────────────────────

/// Build a [`QueryPlan`] from a regex pattern.
///
/// Recursively walks the `regex-syntax` HIR tree:
/// - **Literal** → `Trigrams(all 3-byte windows)`
/// - **Concat** → `And` of children (merges adjacent literal runs for
///   cross-boundary trigrams)
/// - **Alternation** → `Or` of children (union at execution time)
/// - **Repetition(`+`)** → recurse; **`*`/`?`** → `MatchAll`
/// - **Capture** → recurse into sub
/// - **Class** / **Look** / **Empty** → `MatchAll`
///
/// Use with [`FileTrigramIndex::execute_plan`].
pub fn build_query_plan(pattern: &str) -> QueryPlan {
    use regex_syntax::Parser;
    use regex_syntax::hir::{Hir, HirKind};

    let hir = match Parser::new().parse(pattern) {
        Ok(h) => h,
        Err(_) => return QueryPlan::MatchAll,
    };

    fn walk(hir: &Hir) -> QueryPlan {
        match hir.kind() {
            HirKind::Literal(lit) => {
                let tris = trigrams_from_bytes(&lit.0);
                if tris.is_empty() {
                    QueryPlan::MatchAll
                } else {
                    QueryPlan::Trigrams(tris)
                }
            }
            HirKind::Concat(subs) => {
                let mut parts: Vec<QueryPlan> = Vec::new();
                let mut literal_run: Vec<u8> = Vec::new();

                for sub in subs {
                    if let HirKind::Literal(lit) = sub.kind() {
                        literal_run.extend_from_slice(&lit.0);
                    } else {
                        if literal_run.len() >= 3 {
                            parts.push(QueryPlan::Trigrams(trigrams_from_bytes(&literal_run)));
                        }
                        literal_run.clear();
                        let child = walk(sub);
                        if !matches!(child, QueryPlan::MatchAll) {
                            parts.push(child);
                        }
                    }
                }
                if literal_run.len() >= 3 {
                    parts.push(QueryPlan::Trigrams(trigrams_from_bytes(&literal_run)));
                }

                simplify_and(parts)
            }
            HirKind::Alternation(branches) => {
                let plans: Vec<QueryPlan> = branches.iter().map(walk).collect();
                // If ANY branch is MatchAll, the whole OR is MatchAll
                // (that branch matches everything).
                if plans.iter().any(|p| matches!(p, QueryPlan::MatchAll)) {
                    return QueryPlan::MatchAll;
                }
                if plans.is_empty() {
                    return QueryPlan::MatchAll;
                }
                if plans.len() == 1 {
                    return plans.into_iter().next().unwrap();
                }
                QueryPlan::Or(plans)
            }
            HirKind::Repetition(rep) => {
                if rep.min >= 1 {
                    walk(&rep.sub)
                } else {
                    QueryPlan::MatchAll
                }
            }
            HirKind::Capture(cap) => walk(&cap.sub),
            _ => QueryPlan::MatchAll,
        }
    }

    /// Flatten and simplify an AND of sub-plans.
    fn simplify_and(parts: Vec<QueryPlan>) -> QueryPlan {
        let mut trigrams = Vec::new();
        let mut others = Vec::new();
        for p in parts {
            match p {
                QueryPlan::Trigrams(t) => trigrams.extend(t),
                QueryPlan::And(subs) => {
                    for s in subs {
                        match s {
                            QueryPlan::Trigrams(t) => trigrams.extend(t),
                            other => others.push(other),
                        }
                    }
                }
                QueryPlan::MatchAll => {}
                other => others.push(other),
            }
        }
        trigrams.sort_unstable();
        trigrams.dedup();

        if others.is_empty() {
            if trigrams.is_empty() {
                QueryPlan::MatchAll
            } else {
                QueryPlan::Trigrams(trigrams)
            }
        } else {
            if !trigrams.is_empty() {
                others.insert(0, QueryPlan::Trigrams(trigrams));
            }
            if others.len() == 1 {
                others.into_iter().next().unwrap()
            } else {
                QueryPlan::And(others)
            }
        }
    }

    fn trigrams_from_bytes(bytes: &[u8]) -> Vec<[u8; 3]> {
        if bytes.len() < 3 {
            return Vec::new();
        }
        let mut v: Vec<[u8; 3]> = (0..bytes.len() - 2)
            .map(|i| [bytes[i], bytes[i + 1], bytes[i + 2]])
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    walk(&hir)
}

/// Convenience wrapper: extract trigrams that must appear in any match.
///
/// Equivalent to building a query plan and collecting all AND-required
/// trigrams.  Useful when only the flat trigram set is needed.
pub fn extract_required_trigrams(pattern: &str) -> Vec<[u8; 3]> {
    fn collect_and(plan: &QueryPlan) -> Vec<[u8; 3]> {
        match plan {
            QueryPlan::MatchAll => Vec::new(),
            QueryPlan::Trigrams(t) => t.clone(),
            QueryPlan::And(subs) => subs.iter().flat_map(collect_and).collect(),
            QueryPlan::Or(subs) => {
                // Intersection of branches' required trigrams.
                let mut iter = subs.iter().map(|s| {
                    let v = collect_and(s);
                    v.into_iter()
                        .collect::<std::collections::HashSet<[u8; 3]>>()
                });
                let first = match iter.next() {
                    Some(s) => s,
                    None => return Vec::new(),
                };
                let common = iter.fold(first, |acc, set| acc.intersection(&set).copied().collect());
                common.into_iter().collect()
            }
        }
    }
    collect_and(&build_query_plan(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn finds_exact_function_name() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn process_batch(items: &[Item]) { todo!() }");
        idx.add(2, "fn main() { process_batch(&items); }");
        idx.add(3, "fn unrelated_function() {}");
        let candidates = idx.search("process_batch");
        assert!(candidates.contains(&1));
        assert!(candidates.contains(&2));
        assert!(!candidates.contains(&3));
    }

    #[test]
    fn no_matches_returns_empty() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn hello() {}");
        let candidates = idx.search("nonexistent_symbol");
        assert!(candidates.is_empty());
    }

    #[test]
    fn short_query_under_3_chars_returns_empty() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "ab");
        let candidates = idx.search("ab");
        assert!(candidates.is_empty());
    }

    #[test]
    fn case_sensitive_search() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn ProcessBatch() {}");
        idx.add(2, "fn process_batch() {}");
        let candidates = idx.search("ProcessBatch");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], 1);
    }

    #[test]
    fn candidates_include_false_positives() {
        // search() now returns candidates without verification.
        // The caller must verify actual substring matches.
        let mut idx = TrigramIndex::new();
        idx.add(1, "prefix_process_batch_suffix");
        let candidates = idx.search("process_batch");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], 1);
    }

    #[test]
    fn remove_chunk_from_index() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "fn target() {}");
        idx.add(2, "fn target() { call(); }");
        idx.remove(1, "fn target() {}");
        let candidates = idx.search("target");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], 2);
    }

    #[test]
    fn search_10k_chunks_sub_millisecond() {
        let mut idx = TrigramIndex::new();
        for i in 0..10_000u64 {
            idx.add(i, &format!("fn func_{i}(x: i32) -> bool {{ x > {i} }}"));
        }
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(idx.search("func_5000"));
        }
        let elapsed = start.elapsed();
        // Be generous for debug mode
        assert!(
            elapsed.as_secs() < 5,
            "1000 searches on 10K chunks took {elapsed:?}"
        );
    }

    #[test]
    fn multiple_candidates_from_same_chunk() {
        let mut idx = TrigramIndex::new();
        idx.add(1, "HashMap HashMap HashMap");
        let candidates = idx.search("HashMap");
        // search() returns candidate chunk IDs (deduplicated).
        assert_eq!(candidates, vec![1]);
    }

    #[test]
    fn binary_save_and_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram.bin");

        let mut idx = TrigramIndex::new();
        idx.add(1, "fn process_batch(items: &[Item]) { todo!() }");
        idx.add(2, "fn main() { process_batch(&items); }");
        idx.add(3, "fn unrelated_function() {}");

        idx.save_binary(&path).unwrap();
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        assert_eq!(loaded.len(), 3);

        // Verify search candidates are identical after round-trip.
        let mut orig = idx.search("process_batch");
        let mut loaded_ids = loaded.search("process_batch");
        orig.sort();
        loaded_ids.sort();
        assert_eq!(orig, loaded_ids);

        // Verify that unrelated queries still work correctly.
        let no_match = loaded.search("nonexistent_symbol");
        assert!(no_match.is_empty());
    }

    #[test]
    fn binary_empty_index_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram_empty.bin");

        let idx = TrigramIndex::new();
        idx.save_binary(&path).unwrap();
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        assert_eq!(loaded.len(), 0);
        assert!(loaded.is_empty());
    }

    #[test]
    fn mmap_save_and_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram_mmap.bin");

        let mut idx = TrigramIndex::new();
        idx.add(1, "fn process_batch(items: &[Item]) { todo!() }");
        idx.add(2, "fn main() { process_batch(&items); }");
        idx.add(3, "fn unrelated_function() {}");

        idx.save_mmap_binary(&path).unwrap();
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        // Loaded via mmap: search works, HashMap is empty.
        assert!(loaded.mmap.is_some());
        assert_eq!(loaded.len(), 3);

        let mut orig = idx.search("process_batch");
        let mut loaded_ids = loaded.search("process_batch");
        orig.sort();
        loaded_ids.sort();
        assert_eq!(orig, loaded_ids);

        // No-match queries return empty.
        assert!(loaded.search("nonexistent_symbol").is_empty());
        // Short queries return empty.
        assert!(loaded.search("ab").is_empty());
    }

    #[test]
    fn mmap_empty_index_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram_mmap_empty.bin");

        let idx = TrigramIndex::new();
        idx.save_mmap_binary(&path).unwrap();
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        assert_eq!(loaded.len(), 0);
        assert!(loaded.is_empty());
        assert!(loaded.search("anything").is_empty());
    }

    #[test]
    fn mmap_ensure_mutable_preserves_data() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram_mmap_mut.bin");

        let mut idx = TrigramIndex::new();
        idx.add(1, "fn process_batch() {}");
        idx.add(2, "fn other_batch() {}");
        idx.save_mmap_binary(&path).unwrap();

        let mut loaded = TrigramIndex::load_binary(&path).unwrap();
        assert!(loaded.mmap.is_some());

        // Mutate: this triggers ensure_mutable().
        loaded.remove(1, "fn process_batch() {}");
        assert!(loaded.mmap.is_none()); // materialized

        // Only chunk 2 should remain.
        let candidates = loaded.search("batch");
        assert!(!candidates.contains(&1));
        assert!(candidates.contains(&2));
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = TrigramIndex::load_binary(std::path::Path::new("/nonexistent/trigram.bin"));
        assert!(result.is_err());
    }

    // ── FileTrigramIndex tests ────────────────────────────────────────────────

    #[test]
    fn file_trigram_literal_finds_correct_files() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn process_batch() {}");
        idx.add("b.rs", b"fn main() { process_batch(); }");
        idx.add("c.rs", b"fn unrelated() {}");

        let candidates = idx.candidates_for_literal(b"process_batch").unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
    }

    #[test]
    fn file_trigram_absent_trigram_returns_empty() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn hello() {}");

        let candidates = idx.candidates_for_literal(b"nonexistent_sym").unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn file_trigram_short_literal_returns_none() {
        let idx = FileTrigramIndex::new();
        assert!(idx.candidates_for_literal(b"ab").is_none());
    }

    #[test]
    fn file_trigram_multi_chunk_same_file() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn foo() {}"); // chunk 1
        idx.add("a.rs", b"fn bar() {}"); // chunk 2
        idx.add("b.rs", b"fn baz() {}");

        assert_eq!(idx.file_count(), 2);
        let c = idx.candidates_for_literal(b"foo").unwrap();
        assert_eq!(c, vec!["a.rs"]);
    }

    #[test]
    fn file_trigram_remove_file() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn target() {}");
        idx.add("b.rs", b"fn target() { call(); }");
        idx.remove_file("a.rs");
        let candidates = idx.candidates_for_literal(b"target").unwrap();
        assert_eq!(candidates, vec!["b.rs"]);
        assert_eq!(idx.file_count(), 1);
    }

    #[test]
    fn file_trigram_remove_nonexistent_is_noop() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn hello() {}");
        idx.remove_file("nonexistent.rs"); // should not panic
        assert_eq!(idx.file_count(), 1);
    }

    #[test]
    fn file_trigram_save_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn process_batch() {}");
        idx.add("b.rs", b"fn main() { process_batch(); }");
        idx.save_binary(&path).unwrap();

        let loaded = FileTrigramIndex::load_binary(&path).unwrap();
        assert_eq!(loaded.file_count(), 2);
        let c = loaded.candidates_for_literal(b"process_batch").unwrap();
        assert!(c.contains(&"a.rs"));
        assert!(c.contains(&"b.rs"));
    }

    #[test]
    fn file_trigram_candidates_for_trigrams_and_logic() {
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn process_batch() {}");
        idx.add("b.rs", b"fn main() { process_batch(); }");
        idx.add("c.rs", b"fn unrelated() {}");

        // Extract trigrams from "process" — all must be present (AND).
        let trigrams: Vec<[u8; 3]> = b"process".windows(3).map(|w| [w[0], w[1], w[2]]).collect();
        let candidates = idx.candidates_for_trigrams(&trigrams).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
    }

    #[test]
    fn file_trigram_candidates_for_trigrams_empty_returns_none() {
        let idx = FileTrigramIndex::new();
        assert!(idx.candidates_for_trigrams(&[]).is_none());
    }

    // ── extract_required_trigrams tests ────────────────────────────────────────

    #[test]
    fn required_trigrams_literal_pattern() {
        let tris = extract_required_trigrams("process_batch");
        // All trigrams of "process_batch" must be required.
        let expected: std::collections::HashSet<[u8; 3]> = b"process_batch"
            .windows(3)
            .map(|w| [w[0], w[1], w[2]])
            .collect();
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn required_trigrams_concat_with_wildcard() {
        // foo.*bar → trigrams from "foo" AND "bar" both required.
        let tris = extract_required_trigrams("foo.*bar");
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert!(result.contains(b"foo"));
        assert!(result.contains(b"bar"));
    }

    #[test]
    fn required_trigrams_alternation_intersection() {
        // (fooXYZ|fooABC) → only trigrams common to both branches are required.
        // "foo" is common to both.
        let tris = extract_required_trigrams("(fooXYZ|fooABC)");
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert!(result.contains(b"foo"));
        // "XYZ" is NOT in the intersection (only in first branch).
        assert!(!result.contains(b"XYZ"));
    }

    #[test]
    fn required_trigrams_broad_pattern_returns_empty() {
        assert!(extract_required_trigrams(".*").is_empty());
        assert!(extract_required_trigrams("[a-z]+").is_empty());
    }

    #[test]
    fn required_trigrams_case_insensitive_graceful_fallback() {
        // (?i)foo compiles to character classes, not literals — should
        // return empty (graceful fallback to full scan).
        let tris = extract_required_trigrams("(?i)foo");
        assert!(tris.is_empty());
    }

    #[test]
    fn required_trigrams_repetition_plus_vs_star() {
        // (foo)+ requires foo, (foo)* does not.
        let plus_tris = extract_required_trigrams("(foo)+bar");
        let plus: std::collections::HashSet<[u8; 3]> = plus_tris.into_iter().collect();
        assert!(plus.contains(b"foo"));
        assert!(plus.contains(b"bar"));

        let star_tris = extract_required_trigrams("(foo)*bar");
        let star: std::collections::HashSet<[u8; 3]> = star_tris.into_iter().collect();
        // foo is NOT required (can match zero times), but bar is.
        assert!(!star.contains(b"foo"));
        assert!(star.contains(b"bar"));
    }

    #[test]
    fn required_trigrams_anchored_literal() {
        // ^fn main → "fn main" is a literal in a concat.
        let tris = extract_required_trigrams("^fn main");
        let result: std::collections::HashSet<[u8; 3]> = tris.into_iter().collect();
        assert!(result.contains(b"fn "));
        assert!(result.contains(b"mai"));
    }

    // ── QueryPlan + OR support tests ──────────────────────────────────────────

    #[test]
    fn query_plan_or_alternation() {
        // (foo|bar) → Or of two branches; each branch has its own trigrams.
        let plan = build_query_plan("foo|bar");
        assert!(matches!(plan, QueryPlan::Or(_)));

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"use foo::runtime;");
        idx.add("b.rs", b"use bar::task;");
        idx.add("c.rs", b"fn plain_function() {}");

        let candidates = idx.execute_plan(&plan).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
    }

    #[test]
    fn query_plan_or_three_branches() {
        // TODO|FIXME|HACK → Or of three branches.
        let plan = build_query_plan("TODO|FIXME|HACK");

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"// TODO: fix this");
        idx.add("b.rs", b"// FIXME: broken");
        idx.add("c.rs", b"// HACK: workaround");
        idx.add("d.rs", b"fn clean_code() {}");

        let candidates = idx.execute_plan(&plan).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(candidates.contains(&"c.rs"));
        assert!(!candidates.contains(&"d.rs"));
    }

    #[test]
    fn query_plan_concat_with_or() {
        // (foo|bar).*baz → And([Or([foo, bar]), Trigrams([baz])])
        let plan = build_query_plan("(foo|bar).*baz");

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"foo something baz"); // matches
        idx.add("b.rs", b"bar something baz"); // matches
        idx.add("c.rs", b"foo something xyz"); // no baz
        idx.add("d.rs", b"qux something baz"); // no foo/bar

        let candidates = idx.execute_plan(&plan).unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
        assert!(!candidates.contains(&"c.rs"));
        assert!(!candidates.contains(&"d.rs"));
    }

    #[test]
    fn query_plan_matchall_for_broad_patterns() {
        assert!(matches!(build_query_plan(".*"), QueryPlan::MatchAll));
        assert!(matches!(build_query_plan("[a-z]+"), QueryPlan::MatchAll));
        assert!(matches!(build_query_plan("(?i)foo"), QueryPlan::MatchAll));
    }

    #[test]
    fn query_plan_literal_metacharacters() {
        // Literal patterns with regex metacharacters should use raw bytes.
        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"arr[0].field");
        idx.add("b.rs", b"fn something() {}");

        // Literal search for "arr[0]" — raw bytes, not regex.
        let candidates = idx.candidates_for_literal(b"arr[0]").unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(!candidates.contains(&"b.rs"));
    }

    #[test]
    fn query_plan_or_with_matchall_branch() {
        // (foo|.*) → one branch is MatchAll → whole Or is MatchAll.
        let plan = build_query_plan("(foo|.*)");
        assert!(matches!(plan, QueryPlan::MatchAll));
    }

    #[test]
    fn query_plan_large_file_count() {
        let mut idx = FileTrigramIndex::new();
        for i in 0..1000u32 {
            idx.add(
                &format!("file_{i}.rs"),
                format!("fn func_{i}(x: i32) -> bool {{ x > {i} }}").as_bytes(),
            );
        }
        assert_eq!(idx.file_count(), 1000);
        let candidates = idx.candidates_for_literal(b"func_500").unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], "file_500.rs");
    }
}
