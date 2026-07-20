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

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

/// v3 mmap format version (dense u32 ordinals plus a stable-u64 ID table).
const MMAP_VERSION_V3: u32 = 3;

/// Default constant for v1 writes — kept for the original [`TrigramIndex::save_mmap_binary`] path.
const MMAP_VERSION: u32 = MMAP_VERSION_V1;

/// v1 header size: magic(4) + version(4) + trigram_count(4) + chunk_count(4) + total_postings(4).
const MMAP_HEADER_SIZE: usize = 20;

/// v2 header size: v1 header + encoding_flags(4).
const MMAP_V2_HEADER_SIZE: usize = 24;

/// v3 header size: v2 header + stable_id_count(4) + stable_id_checksum(4).
const MMAP_V3_HEADER_SIZE: usize = 32;

/// Size of one v1 trigram index entry: trigram(3) + pad(1) + posting_start(4) + posting_count(4).
const MMAP_ENTRY_SIZE: usize = 12;

/// Size of one v2 trigram index entry: trigram(3) + pad(1) + posting_byte_off(4)
/// + posting_count(4) + posting_byte_size(4).
const MMAP_V2_ENTRY_SIZE: usize = 16;

/// v2 encoding flag bit selecting the posting codec (0 = DeltaVarint, 1 = Roaring).
const ENCODING_FLAG_CODEC_BIT: u32 = 0x1;

/// FNV-1a offset basis for the v3 stable-ID table checksum.
const STABLE_ID_CHECKSUM_OFFSET: u32 = 0x811C_9DC5;

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

    fn from_flag_bits(flags: u32) -> Result<Self> {
        if flags & !ENCODING_FLAG_CODEC_BIT != 0 {
            return Err(CodixingError::Serialization(format!(
                "unsupported trigram encoding flags: 0x{flags:08X}"
            )));
        }
        Ok(if flags & ENCODING_FLAG_CODEC_BIT != 0 {
            PostingCodec::Roaring
        } else {
            PostingCodec::DeltaVarint
        })
    }
}

/// Memory-mapped backing store for zero-deserialization trigram loading.
struct MmapBacking {
    mmap: Mmap,
    trigram_count: u32,
    index_offset: usize,
    postings_offset: usize,
    /// File format version (1, 2, or 3).
    version: u32,
    /// Posting codec for v2/v3 backings (unused/`DeltaVarint` for v1).
    codec: PostingCodec,
    /// Per-entry stride for the trigram index section. Cached for perf:
    /// `MMAP_ENTRY_SIZE` (12) for v1, `MMAP_V2_ENTRY_SIZE` (16) for v2.
    entry_size: usize,
    /// Start of the v3 ordinal-to-stable-ID table. Zero for v1/v2.
    id_table_offset: usize,
    /// Number of stable u64 IDs in the v3 table. Zero for v1/v2.
    id_count: u32,
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

type MaterializedTrigramEntries<'a> = Vec<([u8; 3], Cow<'a, [u64]>)>;

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

                // A structurally valid mmap was checked at load time. If a
                // posting blob is later found to be corrupt, keep the
                // affected trigram empty rather than panicking while turning
                // the index mutable; the next complete rebuild heals it.
                let ids = Self::mmap_read_entry_ids(&backing, off).unwrap_or_default();

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
        // v1 vs v2/v3 — see the posting-list readers below.
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

        let read = |pr: &PostingRef| -> Option<Vec<u64>> {
            match backing.version {
                MMAP_VERSION_V1 => Self::mmap_read_posting_list(backing, pr.payload_a, pr.count),
                MMAP_VERSION_V2 => {
                    Self::mmap_read_posting_list_v2(backing, pr.payload_a, pr.count, pr.payload_b)
                        .map(|ids| ids.into_iter().map(u64::from).collect())
                }
                MMAP_VERSION_V3 => {
                    Self::mmap_read_posting_list_v3(backing, pr.payload_a, pr.count, pr.payload_b)
                }
                _ => None,
            }
        };

        let Some(mut candidates) = read(&posting_refs[0]) else {
            return Vec::new();
        };
        for pr in &posting_refs[1..] {
            let Some(list) = read(pr) else {
                return Vec::new();
            };
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
    /// - **v2/v3**: `payload_a = posting_byte_off`, `payload_b = posting_byte_size`.
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
    /// Returns `None` if the requested range exceeds the mmap bounds
    /// (corrupted or partially written index).
    fn mmap_read_posting_list(backing: &MmapBacking, start: u32, count: u32) -> Option<Vec<u64>> {
        let data = &backing.mmap[..];
        let byte_start = (start as usize).checked_mul(8)?;
        let base = backing.postings_offset.checked_add(byte_start)?;
        let byte_count = (count as usize).checked_mul(8)?;
        let end = base.checked_add(byte_count)?;
        if end > data.len() {
            return None;
        }
        Some(
            (0..count as usize)
                .map(|i| {
                    let off = base + i * 8;
                    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
                })
                .collect(),
        )
    }

    /// Read a v2 posting list (variable-length codec blob) from the mmap
    /// postings section. Returns `None` on truncation or decode failure.
    fn mmap_read_posting_list_v2(
        backing: &MmapBacking,
        byte_off: u32,
        count: u32,
        byte_size: u32,
    ) -> Option<Vec<u32>> {
        let blob = Self::mmap_posting_blob(backing, byte_off, byte_size)?;
        decode_posting_blob(blob, count as usize, backing.codec)
    }

    /// Read a v3 posting list and translate its generation-local ordinals
    /// directly to stable external u64 IDs. Delta-varint decoding writes into
    /// the final `Vec<u64>` so query-time heap use is no higher than v1's raw
    /// u64 reader; the mmap-backed ID table is never materialized on the heap.
    fn mmap_read_posting_list_v3(
        backing: &MmapBacking,
        byte_off: u32,
        count: u32,
        byte_size: u32,
    ) -> Option<Vec<u64>> {
        let blob = Self::mmap_posting_blob(backing, byte_off, byte_size)?;
        match backing.codec {
            PostingCodec::DeltaVarint => {
                let mut out = Vec::with_capacity(count as usize);
                let mut last = 0u32;
                let mut pos = 0usize;
                for i in 0..count as usize {
                    let (delta, consumed) = decode_varint_u32(blob.get(pos..)?)?;
                    pos = pos.checked_add(consumed)?;
                    let ordinal = if i == 0 {
                        delta
                    } else {
                        let next = last.checked_add(delta)?;
                        if next <= last {
                            return None;
                        }
                        next
                    };
                    out.push(Self::mmap_stable_id(backing, ordinal)?);
                    last = ordinal;
                }
                (pos == blob.len()).then_some(out)
            }
            PostingCodec::Roaring => {
                let bitmap = RoaringBitmap::deserialize_from(blob).ok()?;
                if bitmap.len() != u64::from(count) {
                    return None;
                }
                bitmap
                    .iter()
                    .map(|ordinal| Self::mmap_stable_id(backing, ordinal))
                    .collect()
            }
        }
    }

    fn mmap_posting_blob(backing: &MmapBacking, byte_off: u32, byte_size: u32) -> Option<&[u8]> {
        let data = &backing.mmap[..];
        let start = backing.postings_offset.checked_add(byte_off as usize)?;
        let end = start.checked_add(byte_size as usize)?;
        data.get(start..end)
    }

    fn mmap_stable_id(backing: &MmapBacking, ordinal: u32) -> Option<u64> {
        if backing.version != MMAP_VERSION_V3 || ordinal >= backing.id_count {
            return None;
        }
        let rel = (ordinal as usize).checked_mul(8)?;
        let off = backing.id_table_offset.checked_add(rel)?;
        let bytes = backing.mmap.get(off..off.checked_add(8)?)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    fn mmap_read_entry_ids(backing: &MmapBacking, off: usize) -> Option<Vec<u64>> {
        let data = &backing.mmap[..];
        let payload_a = u32::from_le_bytes(data.get(off + 4..off + 8)?.try_into().ok()?);
        let count = u32::from_le_bytes(data.get(off + 8..off + 12)?.try_into().ok()?);
        match backing.version {
            MMAP_VERSION_V1 => Self::mmap_read_posting_list(backing, payload_a, count),
            MMAP_VERSION_V2 | MMAP_VERSION_V3 => {
                let payload_b = u32::from_le_bytes(data.get(off + 12..off + 16)?.try_into().ok()?);
                if backing.version == MMAP_VERSION_V2 {
                    Self::mmap_read_posting_list_v2(backing, payload_a, count, payload_b)
                        .map(|ids| ids.into_iter().map(u64::from).collect())
                } else {
                    Self::mmap_read_posting_list_v3(backing, payload_a, count, payload_b)
                }
            }
            _ => None,
        }
    }

    /// Return sorted trigram entries without forcing a loaded mmap into the
    /// live in-memory index. In-memory postings are borrowed; mmap postings
    /// are owned only while a format migration is being written.
    fn materialized_entries(&self) -> Result<MaterializedTrigramEntries<'_>> {
        let mut entries = if let Some(backing) = self.mmap.as_ref() {
            let data = &backing.mmap[..];
            let mut entries = Vec::with_capacity(backing.trigram_count as usize);
            for i in 0..backing.trigram_count as usize {
                let off = backing.index_offset + i * backing.entry_size;
                let trigram = [data[off], data[off + 1], data[off + 2]];
                let ids = Self::mmap_read_entry_ids(backing, off).ok_or_else(|| {
                    CodixingError::Serialization(format!(
                        "cannot rewrite corrupt trigram posting for {:?}",
                        trigram
                    ))
                })?;
                entries.push((trigram, Cow::Owned(ids)));
            }
            entries
        } else {
            self.index
                .iter()
                .map(|(trigram, ids)| (*trigram, Cow::Borrowed(ids.as_slice())))
                .collect()
        };
        entries.sort_by_key(|(trigram, _)| *trigram);
        Ok(entries)
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
        crate::persistence::atomic_write(path, bytes)?;
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
        let entries = self.materialized_entries()?;
        let trigram_count = u32::try_from(entries.len()).map_err(|_| {
            CodixingError::Serialization("trigram v1 trigram count exceeds u32::MAX".to_string())
        })?;
        let total_postings_u64: u64 = entries.iter().map(|(_, ids)| ids.len() as u64).sum();
        let total_postings = u32::try_from(total_postings_u64).map_err(|_| {
            CodixingError::Serialization("trigram v1 posting count exceeds u32::MAX".to_string())
        })?;
        let chunk_count = u32::try_from(self.chunk_count).map_err(|_| {
            CodixingError::Serialization("trigram v1 chunk count exceeds u32::MAX".to_string())
        })?;

        let total_size = MMAP_HEADER_SIZE
            + (trigram_count as usize) * MMAP_ENTRY_SIZE
            + (total_postings as usize) * 8;
        let mut buf = Vec::with_capacity(total_size);

        // Header.
        buf.extend_from_slice(&MMAP_MAGIC.to_le_bytes());
        buf.extend_from_slice(&MMAP_VERSION.to_le_bytes());
        buf.extend_from_slice(&trigram_count.to_le_bytes());
        buf.extend_from_slice(&chunk_count.to_le_bytes());
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
            for &id in ids.iter() {
                buf.extend_from_slice(&id.to_le_bytes());
            }
        }

        crate::persistence::atomic_write(path, buf)?;
        Ok(())
    }

    /// Whether any chunk ID in this index exceeds `u32::MAX`. The v2 format
    /// stores u32 IDs, so the v2 writer falls back to v1 when this is true.
    /// Chunk IDs are not dense ordinals — hash-derived u64 IDs are routine —
    /// so this is a real, common case, not a 4-billion-chunk corner.
    fn max_id_exceeds_u32(&self) -> Result<bool> {
        const U32_MAX: u64 = u32::MAX as u64;
        Ok(self
            .materialized_entries()?
            .iter()
            .any(|(_, ids)| ids.iter().any(|&id| id > U32_MAX)))
    }

    /// Save the trigram index in the **v2** mmap-friendly binary format with
    /// the chosen posting [`PostingCodec`].
    ///
    /// v2 stores chunk IDs as `u32` and replaces the fixed-stride
    /// 8-bytes-per-id posting layout with a variable-length, codec-tagged
    /// blob per trigram. Indexes whose chunk IDs exceed `u32::MAX`
    /// (hash-derived IDs) are written in the v1 format instead — the loader
    /// dispatches on the version header, so readers are unaffected.
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
        //
        // v2 stores u32 IDs, but chunk IDs are not guaranteed to be dense
        // ordinals — hash-derived u64 IDs are routine. When any ID exceeds
        // u32::MAX, fall back to the v1 format (raw u64 postings) instead of
        // failing: the loader handles both versions transparently.
        if self.max_id_exceeds_u32()? {
            return self.save_mmap_binary(path);
        }

        let entries: Vec<([u8; 3], Vec<u32>)> = self
            .materialized_entries()?
            .into_iter()
            .map(|(trigram, ids)| (trigram, ids.iter().map(|&id| id as u32).collect::<Vec<_>>()))
            .collect();

        // Encode each posting list according to the chosen codec.
        let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(entries.len());
        let mut total_blob_bytes: usize = 0;
        let mut total_postings_u64: u64 = 0;
        for (_tri, ids) in &entries {
            let blob = encode_posting_blob(ids, codec);
            total_blob_bytes += blob.len();
            total_postings_u64 += ids.len() as u64;
            blobs.push(blob);
        }

        let to_u32 = |what: &str, v: u64| -> Result<u32> {
            u32::try_from(v).map_err(|_| {
                CodixingError::Serialization(format!(
                    "trigram v2 {what} {v} exceeds u32::MAX — index too large for v2 format"
                ))
            })
        };

        let trigram_count = to_u32("trigram_count", entries.len() as u64)?;
        let chunk_count_u32 = to_u32("chunk_count", self.chunk_count as u64)?;
        let total_postings = to_u32("total_postings", total_postings_u64)?;
        let total_size =
            MMAP_V2_HEADER_SIZE + (trigram_count as usize) * MMAP_V2_ENTRY_SIZE + total_blob_bytes;
        // Validate the postings section fits in a u32 byte offset — the
        // per-entry `posting_byte_off` is u32.
        let _ = to_u32("postings section byte size", total_blob_bytes as u64)?;
        let mut buf = Vec::with_capacity(total_size);

        // Header.
        buf.extend_from_slice(&MMAP_MAGIC.to_le_bytes());
        buf.extend_from_slice(&MMAP_VERSION_V2.to_le_bytes());
        buf.extend_from_slice(&trigram_count.to_le_bytes());
        buf.extend_from_slice(&chunk_count_u32.to_le_bytes());
        buf.extend_from_slice(&total_postings.to_le_bytes());
        buf.extend_from_slice(&codec.to_flag_bits().to_le_bytes());

        // Trigram index entries — write fixed-stride entries with byte
        // offsets/sizes that point into the postings section that follows.
        let mut byte_off: u64 = 0;
        for ((trigram, ids), blob) in entries.iter().zip(blobs.iter()) {
            let off_u32 = to_u32("posting_byte_off", byte_off)?;
            let count_u32 = to_u32("posting_count", ids.len() as u64)?;
            let blob_len_u32 = to_u32("posting_byte_size", blob.len() as u64)?;
            buf.extend_from_slice(trigram);
            buf.push(0); // padding
            buf.extend_from_slice(&off_u32.to_le_bytes());
            buf.extend_from_slice(&count_u32.to_le_bytes());
            buf.extend_from_slice(&blob_len_u32.to_le_bytes());
            byte_off += blob.len() as u64;
        }

        // Postings (codec blobs).
        for blob in &blobs {
            buf.extend_from_slice(blob);
        }

        crate::persistence::atomic_write(path, buf)?;
        Ok(())
    }

    /// Save the trigram index in the **v3** compact mmap format.
    ///
    /// v3 keeps public chunk IDs stable and lossless while encoding posting
    /// lists with generation-local dense u32 ordinals. A sorted ordinal-to-u64
    /// table is memory-mapped alongside the postings, so hash-derived IDs no
    /// longer force the raw-u64 v1 fallback and the table adds no query-time
    /// heap residency.
    ///
    /// ## Binary format
    ///
    /// ```text
    /// [Header: 32 bytes, little-endian]
    ///   magic:           u32 = 0x5452474D ("TRGM")
    ///   version:         u32 = 3
    ///   trigram_count:   u32
    ///   chunk_count:     u32
    ///   total_postings:  u32
    ///   encoding_flags:  u32   (bit 0 selects the posting codec)
    ///   stable_id_count:    u32
    ///   stable_id_checksum: u32   (FNV-1a over little-endian stable IDs)
    ///
    /// [Trigram Index: trigram_count x 16 bytes]
    /// [Stable ID table: stable_id_count x u64, strictly ascending]
    /// [Posting blobs: sorted dense u32 ordinals]
    /// ```
    pub fn save_mmap_binary_v3(&self, path: &Path, codec: PostingCodec) -> Result<()> {
        let entries = self.materialized_entries()?;

        // Build the generation-local ID space without copying every posting.
        // Only one entry per distinct chunk is resident in this temporary set.
        let mut stable_id_set = HashSet::new();
        for (_, ids) in &entries {
            stable_id_set.extend(ids.iter().copied());
        }
        let mut stable_ids: Vec<u64> = stable_id_set.into_iter().collect();
        stable_ids.sort_unstable();

        let to_u32 = |what: &str, value: u64| -> Result<u32> {
            u32::try_from(value).map_err(|_| {
                CodixingError::Serialization(format!("trigram v3 {what} {value} exceeds u32::MAX"))
            })
        };

        let stable_id_count = to_u32("stable_id_count", stable_ids.len() as u64)?;
        let stable_id_checksum = stable_ids
            .iter()
            .fold(STABLE_ID_CHECKSUM_OFFSET, |checksum, &stable_id| {
                update_stable_id_checksum(checksum, stable_id)
            });
        let chunk_count = to_u32("chunk_count", self.chunk_count as u64)?;
        let id_to_ordinal: HashMap<u64, u32> = stable_ids
            .iter()
            .enumerate()
            .map(|(ordinal, &stable_id)| Ok((stable_id, to_u32("ordinal", ordinal as u64)?)))
            .collect::<Result<_>>()?;

        struct EncodedEntry {
            trigram: [u8; 3],
            posting_count: u32,
            blob: Vec<u8>,
        }

        let mut encoded_entries = Vec::with_capacity(entries.len());
        let mut total_postings_u64 = 0u64;
        let mut total_blob_bytes = 0usize;
        for (trigram, ids) in entries {
            let mut ordinals: Vec<u32> = ids
                .iter()
                .map(|stable_id| {
                    id_to_ordinal.get(stable_id).copied().ok_or_else(|| {
                        CodixingError::Serialization(format!(
                            "trigram v3 stable ID {stable_id} is missing from the ordinal table"
                        ))
                    })
                })
                .collect::<Result<_>>()?;
            ordinals.sort_unstable();
            ordinals.dedup();
            if ordinals.is_empty() {
                continue;
            }

            let posting_count = to_u32("posting_count", ordinals.len() as u64)?;
            let blob = encode_posting_blob(&ordinals, codec);
            total_postings_u64 = total_postings_u64
                .checked_add(ordinals.len() as u64)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "trigram v3 total posting count overflow".to_string(),
                    )
                })?;
            total_blob_bytes = total_blob_bytes.checked_add(blob.len()).ok_or_else(|| {
                CodixingError::Serialization("trigram v3 posting bytes overflow".to_string())
            })?;
            encoded_entries.push(EncodedEntry {
                trigram,
                posting_count,
                blob,
            });
        }

        let trigram_count = to_u32("trigram_count", encoded_entries.len() as u64)?;
        let total_postings = to_u32("total_postings", total_postings_u64)?;
        let _ = to_u32("postings section byte size", total_blob_bytes as u64)?;
        let index_bytes = encoded_entries
            .len()
            .checked_mul(MMAP_V2_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("trigram v3 index size overflow".to_string())
            })?;
        let id_table_bytes = stable_ids.len().checked_mul(8).ok_or_else(|| {
            CodixingError::Serialization("trigram v3 ID table size overflow".to_string())
        })?;
        let total_size = MMAP_V3_HEADER_SIZE
            .checked_add(index_bytes)
            .and_then(|size| size.checked_add(id_table_bytes))
            .and_then(|size| size.checked_add(total_blob_bytes))
            .ok_or_else(|| {
                CodixingError::Serialization("trigram v3 file size overflow".to_string())
            })?;

        let mut buf = Vec::with_capacity(total_size);
        buf.extend_from_slice(&MMAP_MAGIC.to_le_bytes());
        buf.extend_from_slice(&MMAP_VERSION_V3.to_le_bytes());
        buf.extend_from_slice(&trigram_count.to_le_bytes());
        buf.extend_from_slice(&chunk_count.to_le_bytes());
        buf.extend_from_slice(&total_postings.to_le_bytes());
        buf.extend_from_slice(&codec.to_flag_bits().to_le_bytes());
        buf.extend_from_slice(&stable_id_count.to_le_bytes());
        buf.extend_from_slice(&stable_id_checksum.to_le_bytes());

        let mut byte_off = 0u64;
        for entry in &encoded_entries {
            let posting_byte_off = to_u32("posting_byte_off", byte_off)?;
            let posting_byte_size = to_u32("posting_byte_size", entry.blob.len() as u64)?;
            buf.extend_from_slice(&entry.trigram);
            buf.push(0);
            buf.extend_from_slice(&posting_byte_off.to_le_bytes());
            buf.extend_from_slice(&entry.posting_count.to_le_bytes());
            buf.extend_from_slice(&posting_byte_size.to_le_bytes());
            byte_off = byte_off
                .checked_add(entry.blob.len() as u64)
                .ok_or_else(|| {
                    CodixingError::Serialization("trigram v3 posting offset overflow".to_string())
                })?;
        }

        for stable_id in stable_ids {
            buf.extend_from_slice(&stable_id.to_le_bytes());
        }
        for entry in encoded_entries {
            buf.extend_from_slice(&entry.blob);
        }

        debug_assert_eq!(buf.len(), total_size);
        crate::persistence::atomic_write(path, buf)?;
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
    /// Dispatches on the `version` field in the header. v1 (raw u64), v2
    /// (u32), and v3 (dense u32 ordinal + stable u64 table) remain readable.
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
                        id_table_offset: 0,
                        id_count: 0,
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
                let codec = PostingCodec::from_flag_bits(encoding_flags)?;

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
                        id_table_offset: 0,
                        id_count: 0,
                    }),
                })
            }
            MMAP_VERSION_V3 => {
                if file_len < MMAP_V3_HEADER_SIZE {
                    return Err(CodixingError::Serialization(
                        "trigram mmap v3 file too small for header".to_string(),
                    ));
                }
                let trigram_count = u32::from_le_bytes(mmap[8..12].try_into().unwrap());
                let chunk_count = u32::from_le_bytes(mmap[12..16].try_into().unwrap());
                let total_postings = u32::from_le_bytes(mmap[16..20].try_into().unwrap());
                let encoding_flags = u32::from_le_bytes(mmap[20..24].try_into().unwrap());
                let codec = PostingCodec::from_flag_bits(encoding_flags)?;
                let id_count = u32::from_le_bytes(mmap[24..28].try_into().unwrap());
                let expected_id_checksum = u32::from_le_bytes(mmap[28..32].try_into().unwrap());
                if id_count > chunk_count {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v3 stable ID count {id_count} exceeds chunk count {chunk_count}"
                    )));
                }
                if total_postings > 0 && id_count == 0 {
                    return Err(CodixingError::Serialization(
                        "trigram mmap v3 has postings but no stable ID table".to_string(),
                    ));
                }

                let index_offset = MMAP_V3_HEADER_SIZE;
                let index_bytes = (trigram_count as usize)
                    .checked_mul(MMAP_V2_ENTRY_SIZE)
                    .ok_or_else(|| {
                        CodixingError::Serialization(
                            "trigram mmap v3 index size overflow".to_string(),
                        )
                    })?;
                let id_table_offset = index_offset.checked_add(index_bytes).ok_or_else(|| {
                    CodixingError::Serialization(
                        "trigram mmap v3 ID table offset overflow".to_string(),
                    )
                })?;
                let id_table_bytes = (id_count as usize).checked_mul(8).ok_or_else(|| {
                    CodixingError::Serialization(
                        "trigram mmap v3 ID table size overflow".to_string(),
                    )
                })?;
                let postings_offset =
                    id_table_offset.checked_add(id_table_bytes).ok_or_else(|| {
                        CodixingError::Serialization(
                            "trigram mmap v3 postings offset overflow".to_string(),
                        )
                    })?;
                if file_len < postings_offset {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v3 file truncated: fixed sections extend to {postings_offset} bytes, got {file_len}"
                    )));
                }

                // Structural validation is O(number of trigrams), but does
                // not decode or allocate posting lists. Enforcing canonical
                // ordering and contiguous blobs makes the format deterministic
                // and catches offset overlap/gaps before the mmap is exposed.
                let mut previous_trigram: Option<[u8; 3]> = None;
                let mut expected_blob_offset = 0usize;
                let mut counted_postings = 0u64;
                for i in 0..trigram_count as usize {
                    let off = index_offset + i * MMAP_V2_ENTRY_SIZE;
                    let trigram = [mmap[off], mmap[off + 1], mmap[off + 2]];
                    if mmap[off + 3] != 0 {
                        return Err(CodixingError::Serialization(format!(
                            "trigram mmap v3 entry {i} has non-zero padding"
                        )));
                    }
                    if previous_trigram.is_some_and(|previous| previous >= trigram) {
                        return Err(CodixingError::Serialization(format!(
                            "trigram mmap v3 entries are not strictly sorted at entry {i}"
                        )));
                    }
                    previous_trigram = Some(trigram);

                    let byte_off =
                        u32::from_le_bytes(mmap[off + 4..off + 8].try_into().unwrap()) as usize;
                    let posting_count =
                        u32::from_le_bytes(mmap[off + 8..off + 12].try_into().unwrap());
                    let byte_size =
                        u32::from_le_bytes(mmap[off + 12..off + 16].try_into().unwrap()) as usize;
                    if byte_off != expected_blob_offset {
                        return Err(CodixingError::Serialization(format!(
                            "trigram mmap v3 entry {i} starts at blob offset {byte_off}, expected {expected_blob_offset}"
                        )));
                    }
                    if posting_count == 0 || byte_size == 0 {
                        return Err(CodixingError::Serialization(format!(
                            "trigram mmap v3 entry {i} has an empty posting list"
                        )));
                    }
                    expected_blob_offset =
                        expected_blob_offset.checked_add(byte_size).ok_or_else(|| {
                            CodixingError::Serialization(
                                "trigram mmap v3 posting size overflow".to_string(),
                            )
                        })?;
                    counted_postings = counted_postings
                        .checked_add(u64::from(posting_count))
                        .ok_or_else(|| {
                            CodixingError::Serialization(
                                "trigram mmap v3 posting count overflow".to_string(),
                            )
                        })?;
                }
                if counted_postings != u64::from(total_postings) {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v3 posting count mismatch: header says {total_postings}, entries say {counted_postings}"
                    )));
                }
                let expected_size = postings_offset
                    .checked_add(expected_blob_offset)
                    .ok_or_else(|| {
                        CodixingError::Serialization(
                            "trigram mmap v3 file size overflow".to_string(),
                        )
                    })?;
                if file_len != expected_size {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v3 size mismatch: expected exactly {expected_size} bytes, got {file_len}"
                    )));
                }

                let mut previous_id = None;
                let mut actual_id_checksum = STABLE_ID_CHECKSUM_OFFSET;
                for ordinal in 0..id_count as usize {
                    let off = id_table_offset + ordinal * 8;
                    let stable_id = u64::from_le_bytes(mmap[off..off + 8].try_into().unwrap());
                    if previous_id.is_some_and(|previous| previous >= stable_id) {
                        return Err(CodixingError::Serialization(format!(
                            "trigram mmap v3 stable ID table is not strictly sorted at ordinal {ordinal}"
                        )));
                    }
                    previous_id = Some(stable_id);
                    actual_id_checksum = update_stable_id_checksum(actual_id_checksum, stable_id);
                }
                if actual_id_checksum != expected_id_checksum {
                    return Err(CodixingError::Serialization(format!(
                        "trigram mmap v3 stable ID checksum mismatch: expected 0x{expected_id_checksum:08X}, got 0x{actual_id_checksum:08X}"
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
                        version: MMAP_VERSION_V3,
                        codec,
                        entry_size: MMAP_V2_ENTRY_SIZE,
                        id_table_offset,
                        id_count,
                    }),
                })
            }
            other => Err(CodixingError::Serialization(format!(
                "unsupported trigram mmap version: expected 1, 2, or 3, got {other}"
            ))),
        }
    }
}

// ── v2/v3 codec helpers ──────────────────────────────────────────────────────

fn update_stable_id_checksum(mut checksum: u32, stable_id: u64) -> u32 {
    for byte in stable_id.to_le_bytes() {
        checksum ^= u32::from(byte);
        checksum = checksum.wrapping_mul(0x0100_0193);
    }
    checksum
}

/// Encode a sorted list of u32 chunk IDs/ordinals into a posting blob using the
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

/// Decode a posting blob back into a sorted `Vec<u32>` of chunk IDs/ordinals.
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
                pos = pos.checked_add(consumed)?;
                let next = last.checked_add(delta)?;
                if !out.is_empty() && next <= last {
                    return None;
                }
                out.push(next);
                last = next;
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
        // Reject payloads that don't fit in the remaining `32 - shift` bits.
        // `checked_shl` alone only validates the shift amount; a payload like
        // 0x7F at shift=28 would silently drop bits because 0x7F << 28 > u32::MAX.
        let bits_available = 32 - shift;
        if bits_available < 7 && (payload >> bits_available) != 0 {
            return None;
        }
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

/// Magic bytes for the mmap file-level trigram format: `FTRI`.
const FILE_TRIGRAM_MMAP_MAGIC: u32 = u32::from_le_bytes(*b"FTRI");
const FILE_TRIGRAM_MMAP_VERSION: u32 = 1;
const FILE_TRIGRAM_MMAP_HEADER_SIZE: usize = 32;
const FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE: usize = 8;
const FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE: usize = 16;
const FILE_TRIGRAM_POSTING_VALIDATION_CACHE_CAPACITY: usize = 256;

/// Maximum number of complete changed-path replacements retained beside an
/// immutable file-trigram base before the next checkpoint compacts it.
pub(crate) const FILE_TRIGRAM_DELTA_MAX_PATHS: usize = 4096;
/// Maximum encoded size of the durable changed-path overlay.
pub(crate) const FILE_TRIGRAM_DELTA_MAX_BYTES: usize = 8 * 1024 * 1024;
const FILE_TRIGRAM_DELTA_MAGIC: u32 = u32::from_le_bytes(*b"FTDL");
const FILE_TRIGRAM_DELTA_VERSION: u32 = 1;

#[cfg(test)]
thread_local! {
    static FILE_TRIGRAM_CHECKPOINT_SCRATCH_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FILE_TRIGRAM_POSTING_VALUES_VALIDATED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FILE_TRIGRAM_OVERLAY_MEMBERSHIPS_VISITED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Default)]
struct FilePostingValidationCache {
    validated: HashSet<u32>,
    order: VecDeque<u32>,
}

/// Zero-copy backing for a persisted [`FileTrigramIndex`].
///
/// File paths and posting lists stay in the mapped generation until an actual
/// mutation requires the ordinary in-memory representation. Exact search only
/// touches the few posting pages required by its query.
struct FileTrigramMmap {
    mmap: Mmap,
    /// Canonical path of the immutable file backing this mapping.
    source_path: PathBuf,
    file_count: u32,
    trigram_count: u32,
    file_table_offset: usize,
    trigram_index_offset: usize,
    string_pool_offset: usize,
    postings_offset: usize,
    posting_validation: Mutex<FilePostingValidationCache>,
}

#[derive(Clone, Copy)]
struct FilePostingRef {
    byte_offset: u32,
    count: u32,
}

/// Changed-file state layered over an immutable mapped generation.
///
/// `exclude_base` distinguishes replacement (`true`) from the historical
/// additive `add` contract (`false`). `trigrams == None` is a removal.
struct FileTrigramDelta {
    exclude_base: bool,
    trigrams: Option<Vec<[u8; 3]>>,
}

/// Deterministic on-disk representation of the bounded mapped-base overlay.
/// Entries are required to be strictly sorted by normalized relative path.
#[derive(Serialize, Deserialize)]
struct FileTrigramDeltaCheckpoint {
    entries: Vec<FileTrigramDeltaEntry>,
}

#[derive(Serialize, Deserialize)]
struct FileTrigramDeltaEntry {
    path: String,
    exclude_base: bool,
    trigrams: Option<Vec<[u8; 3]>>,
}

struct FileTrigramMergePlan<'a> {
    removed_base_paths: Vec<&'a str>,
    added_paths: Vec<&'a str>,
    overlay_files: Vec<(&'a str, &'a [[u8; 3]])>,
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
    /// Immutable mapped base. Changed files remain in `delta` until checkpoint.
    mmap: Option<FileTrigramMmap>,
    /// Sorted cumulative changed-path overlay since the last base compaction.
    delta: BTreeMap<String, FileTrigramDelta>,
    /// Corrupt persisted overlays disable prefiltering so callers perform
    /// correctness-preserving full scans instead of trusting a partial base.
    disabled: bool,
}

impl Default for FileTrigramIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTrigramIndex {
    #[cfg(test)]
    fn record_checkpoint_scratch(items: usize) {
        FILE_TRIGRAM_CHECKPOINT_SCRATCH_PEAK.with(|peak| peak.set(peak.get().max(items)));
    }

    #[cfg(not(test))]
    fn record_checkpoint_scratch(_items: usize) {}

    /// Creates an empty index.
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            file_index: HashMap::new(),
            index: HashMap::new(),
            mmap: None,
            delta: BTreeMap::new(),
            disabled: false,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            disabled: true,
            ..Self::new()
        }
    }

    fn delta_checkpoint(entries: Vec<FileTrigramDeltaEntry>) -> FileTrigramDeltaCheckpoint {
        FileTrigramDeltaCheckpoint { entries }
    }

    fn serialize_delta_checkpoint(entries: Vec<FileTrigramDeltaEntry>) -> Result<Vec<u8>> {
        let payload = bitcode::serialize(&Self::delta_checkpoint(entries)).map_err(|error| {
            CodixingError::Serialization(format!("failed to serialize file trigram delta: {error}"))
        })?;
        let mut bytes = Vec::with_capacity(8 + payload.len());
        bytes.extend_from_slice(&FILE_TRIGRAM_DELTA_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&FILE_TRIGRAM_DELTA_VERSION.to_le_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    /// Canonical empty overlay paired with every freshly built or compacted
    /// `file_trigram.bin` generation.
    pub(crate) fn empty_delta_checkpoint() -> Result<Vec<u8>> {
        Self::serialize_delta_checkpoint(Vec::new())
    }

    /// Encode the cumulative mapped-base overlay when it remains within both
    /// checkpoint limits. `None` asks the caller to compact a new base.
    pub(crate) fn delta_checkpoint_bytes(&self) -> Result<Option<Vec<u8>>> {
        if self.disabled {
            return Err(CodixingError::Serialization(
                "cannot checkpoint a disabled file trigram index".to_string(),
            ));
        }
        if self.mmap.is_none() || self.delta.len() > FILE_TRIGRAM_DELTA_MAX_PATHS {
            return Ok(None);
        }
        let raw_overlay_bytes = self.delta.iter().fold(8usize, |bytes, (path, change)| {
            bytes.saturating_add(path.len()).saturating_add(
                change
                    .trigrams
                    .as_ref()
                    .map_or(0, |trigrams| trigrams.len().saturating_mul(3)),
            )
        });
        if raw_overlay_bytes > FILE_TRIGRAM_DELTA_MAX_BYTES {
            return Ok(None);
        }
        let entries = self
            .delta
            .iter()
            .map(|(path, change)| FileTrigramDeltaEntry {
                path: path.clone(),
                exclude_base: change.exclude_base,
                trigrams: change.trigrams.clone(),
            })
            .collect::<Vec<_>>();
        let backing = self.mmap.as_ref().expect("mapped delta checkpoint base");
        for entry in &entries {
            Self::validate_delta_entry(backing, entry)?;
        }
        let bytes = Self::serialize_delta_checkpoint(entries)?;
        Ok((bytes.len() <= FILE_TRIGRAM_DELTA_MAX_BYTES).then_some(bytes))
    }

    fn is_safe_normalized_relative_path(path: &str) -> bool {
        if path.is_empty()
            || path.starts_with('/')
            || path.ends_with('/')
            || path.contains('\\')
            || path.contains('\0')
        {
            return false;
        }
        let bytes = path.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return false;
        }
        let mut components = path.split('/');
        let Some(first) = components.next() else {
            return false;
        };
        if first.is_empty() || matches!(first, "." | "..") {
            return false;
        }
        components.all(|component| !component.is_empty() && !matches!(component, "." | ".."))
    }

    fn validate_delta_entry(
        backing: &FileTrigramMmap,
        entry: &FileTrigramDeltaEntry,
    ) -> Result<()> {
        if !Self::is_safe_normalized_relative_path(&entry.path) {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta path is not a safe normalized relative path: {:?}",
                entry.path
            )));
        }
        if !entry.exclude_base && entry.trigrams.is_none() {
            return Err(CodixingError::Serialization(
                "file trigram delta removal does not exclude the base".to_string(),
            ));
        }
        if entry.exclude_base && Self::mmap_file_id(backing, &entry.path).is_none() {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta replacement has no base path: {:?}",
                entry.path
            )));
        }
        if let Some(trigrams) = &entry.trigrams
            && trigrams.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta trigrams are not strictly sorted for {:?}",
                entry.path
            )));
        }
        Ok(())
    }

    fn apply_delta_checkpoint(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > FILE_TRIGRAM_DELTA_MAX_BYTES {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta is {} bytes; maximum is {}",
                bytes.len(),
                FILE_TRIGRAM_DELTA_MAX_BYTES
            )));
        }
        if bytes.len() < 8 {
            return Err(CodixingError::Serialization(
                "file trigram delta is smaller than its header".to_string(),
            ));
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if magic != FILE_TRIGRAM_DELTA_MAGIC || version != FILE_TRIGRAM_DELTA_VERSION {
            return Err(CodixingError::Serialization(
                "unsupported file trigram delta format".to_string(),
            ));
        }
        let checkpoint: FileTrigramDeltaCheckpoint =
            bitcode::deserialize(&bytes[8..]).map_err(|error| {
                CodixingError::Serialization(format!(
                    "failed to deserialize file trigram delta: {error}"
                ))
            })?;
        if checkpoint.entries.len() > FILE_TRIGRAM_DELTA_MAX_PATHS {
            return Err(CodixingError::Serialization(format!(
                "file trigram delta contains {} paths; maximum is {}",
                checkpoint.entries.len(),
                FILE_TRIGRAM_DELTA_MAX_PATHS
            )));
        }
        if self.mmap.is_none() {
            return Err(CodixingError::Serialization(
                "file trigram delta requires an mmap file_trigram.bin base".to_string(),
            ));
        }

        let mut delta = BTreeMap::<String, FileTrigramDelta>::new();
        for entry in checkpoint.entries {
            if delta
                .last_key_value()
                .is_some_and(|(previous, _)| previous.as_str() >= entry.path.as_str())
            {
                return Err(CodixingError::Serialization(
                    "file trigram delta paths are not strictly sorted".to_string(),
                ));
            }
            Self::validate_delta_entry(self.mmap.as_ref().unwrap(), &entry)?;
            delta.insert(
                entry.path,
                FileTrigramDelta {
                    exclude_base: entry.exclude_base,
                    trigrams: entry.trigrams,
                },
            );
        }
        self.delta = delta;
        Ok(())
    }

    fn mmap_file_path(backing: &FileTrigramMmap, file_id: u32) -> Option<&str> {
        if file_id >= backing.file_count {
            return None;
        }
        let file_entry_offset =
            (file_id as usize).checked_mul(FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE)?;
        let entry = backing.file_table_offset.checked_add(file_entry_offset)?;
        let offset_end = entry.checked_add(4)?;
        let entry_end = entry.checked_add(FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE)?;
        let relative_offset =
            u32::from_le_bytes(backing.mmap.get(entry..offset_end)?.try_into().ok()?) as usize;
        let len =
            u32::from_le_bytes(backing.mmap.get(offset_end..entry_end)?.try_into().ok()?) as usize;
        let start = backing.string_pool_offset.checked_add(relative_offset)?;
        let bytes = backing.mmap.get(start..start.checked_add(len)?)?;
        std::str::from_utf8(bytes).ok()
    }

    fn mmap_file_id(backing: &FileTrigramMmap, path: &str) -> Option<u32> {
        let mut low = 0u32;
        let mut high = backing.file_count;
        while low < high {
            let middle = low + (high - low) / 2;
            let candidate = Self::mmap_file_path(backing, middle)?;
            match candidate.cmp(path) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(middle),
            }
        }
        None
    }

    fn mmap_lookup_trigram(backing: &FileTrigramMmap, trigram: &[u8; 3]) -> Option<FilePostingRef> {
        let mut low = 0usize;
        let mut high = backing.trigram_count as usize;
        while low < high {
            let middle = low + (high - low) / 2;
            let relative = middle.checked_mul(FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE)?;
            let entry = backing.trigram_index_offset.checked_add(relative)?;
            let key = [
                *backing.mmap.get(entry)?,
                *backing.mmap.get(entry.checked_add(1)?)?,
                *backing.mmap.get(entry.checked_add(2)?)?,
            ];
            match key.cmp(trigram) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    return Some(FilePostingRef {
                        byte_offset: u32::from_le_bytes(
                            backing
                                .mmap
                                .get(entry.checked_add(4)?..entry.checked_add(8)?)?
                                .try_into()
                                .ok()?,
                        ),
                        count: u32::from_le_bytes(
                            backing
                                .mmap
                                .get(entry.checked_add(8)?..entry.checked_add(12)?)?
                                .try_into()
                                .ok()?,
                        ),
                    });
                }
            }
        }
        None
    }

    fn mmap_posting_value(
        backing: &FileTrigramMmap,
        posting: FilePostingRef,
        index: usize,
    ) -> Option<u32> {
        if index >= posting.count as usize {
            return None;
        }
        let relative = (posting.byte_offset as usize).checked_add(index.checked_mul(4)?)?;
        let offset = backing.postings_offset.checked_add(relative)?;
        let end = offset.checked_add(4)?;
        Some(u32::from_le_bytes(
            backing.mmap.get(offset..end)?.try_into().ok()?,
        ))
    }

    fn mmap_posting_contains(
        backing: &FileTrigramMmap,
        posting: FilePostingRef,
        needle: u32,
    ) -> bool {
        let mut low = 0usize;
        let mut high = posting.count as usize;
        while low < high {
            let middle = low + (high - low) / 2;
            let Some(value) = Self::mmap_posting_value(backing, posting, middle) else {
                return false;
            };
            match value.cmp(&needle) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    fn validate_mmap_posting(backing: &FileTrigramMmap, posting: FilePostingRef) -> Result<()> {
        let mut previous = None;
        for posting_index in 0..posting.count as usize {
            #[cfg(test)]
            FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| count.set(count.get() + 1));
            let file_id =
                Self::mmap_posting_value(backing, posting, posting_index).ok_or_else(|| {
                    CodixingError::Serialization(
                        "file trigram posting value is truncated".to_string(),
                    )
                })?;
            if file_id >= backing.file_count {
                return Err(CodixingError::Serialization(format!(
                    "file trigram posting file ID {file_id} exceeds file count {}",
                    backing.file_count
                )));
            }
            if previous.is_some_and(|prior| prior >= file_id) {
                return Err(CodixingError::Serialization(
                    "file trigram posting IDs are not strictly increasing".to_string(),
                ));
            }
            previous = Some(file_id);
        }
        Ok(())
    }

    fn validate_all_mmap_postings(backing: &FileTrigramMmap) -> Result<()> {
        for index in 0..backing.trigram_count as usize {
            let relative = index
                .checked_mul(FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "file trigram validation index offset overflow".to_string(),
                    )
                })?;
            let entry = backing
                .trigram_index_offset
                .checked_add(relative)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "file trigram validation index entry overflow".to_string(),
                    )
                })?;
            let posting = FilePostingRef {
                byte_offset: u32::from_le_bytes(
                    backing
                        .mmap
                        .get(entry + 4..entry + 8)
                        .ok_or_else(|| {
                            CodixingError::Serialization(
                                "file trigram validation index is truncated".to_string(),
                            )
                        })?
                        .try_into()
                        .unwrap(),
                ),
                count: u32::from_le_bytes(
                    backing
                        .mmap
                        .get(entry + 8..entry + 12)
                        .ok_or_else(|| {
                            CodixingError::Serialization(
                                "file trigram validation index is truncated".to_string(),
                            )
                        })?
                        .try_into()
                        .unwrap(),
                ),
            };
            Self::validate_mmap_posting(backing, posting)?;
        }
        Ok(())
    }

    fn validate_mmap_posting_cached(
        backing: &FileTrigramMmap,
        posting: FilePostingRef,
    ) -> Result<()> {
        let key = posting.byte_offset;
        {
            let cache = backing.posting_validation.lock().map_err(|_| {
                CodixingError::Serialization(
                    "file trigram posting validation cache is poisoned".to_string(),
                )
            })?;
            if cache.validated.contains(&key) {
                return Ok(());
            }
        }

        Self::validate_mmap_posting(backing, posting)?;
        let mut cache = backing.posting_validation.lock().map_err(|_| {
            CodixingError::Serialization(
                "file trigram posting validation cache is poisoned".to_string(),
            )
        })?;
        if cache.validated.insert(key) {
            cache.order.push_back(key);
            if cache.order.len() > FILE_TRIGRAM_POSTING_VALIDATION_CACHE_CAPACITY
                && let Some(evicted) = cache.order.pop_front()
            {
                cache.validated.remove(&evicted);
            }
        }
        Ok(())
    }

    fn validated_mmap_lookup_trigram(
        backing: &FileTrigramMmap,
        trigram: &[u8; 3],
    ) -> Result<Option<FilePostingRef>> {
        let Some(posting) = Self::mmap_lookup_trigram(backing, trigram) else {
            return Ok(None);
        };
        Self::validate_mmap_posting_cached(backing, posting)?;
        Ok(Some(posting))
    }

    /// Visit mapped candidates directly from the shortest posting list.
    ///
    /// This never allocates a candidate vector proportional to the number of
    /// files sharing a common trigram.
    fn visit_mmap_candidate_ids(
        backing: &FileTrigramMmap,
        trigrams: &[[u8; 3]],
        mut visit: impl FnMut(u32) -> Result<()>,
    ) -> Result<()> {
        let mut postings = Vec::with_capacity(trigrams.len());
        for trigram in trigrams {
            let Some(posting) = Self::validated_mmap_lookup_trigram(backing, trigram)? else {
                return Ok(());
            };
            postings.push(posting);
        }
        postings.sort_unstable_by_key(|posting| posting.count);

        let Some((shortest, remaining)) = postings.split_first() else {
            return Ok(());
        };
        for index in 0..shortest.count as usize {
            let candidate =
                Self::mmap_posting_value(backing, *shortest, index).ok_or_else(|| {
                    CodixingError::Serialization(
                        "validated file trigram posting became truncated".to_string(),
                    )
                })?;
            if remaining
                .iter()
                .all(|posting| Self::mmap_posting_contains(backing, *posting, candidate))
            {
                visit(candidate)?;
            }
        }
        Ok(())
    }

    fn delta_matches(change: &FileTrigramDelta, trigrams: &[[u8; 3]]) -> bool {
        change.trigrams.as_ref().is_some_and(|available| {
            trigrams
                .iter()
                .all(|trigram| available.binary_search(trigram).is_ok())
        })
    }

    fn mapped_change_matches(
        backing: &FileTrigramMmap,
        path: &str,
        change: &FileTrigramDelta,
        trigrams: &[[u8; 3]],
    ) -> Result<bool> {
        if change.exclude_base {
            return Ok(Self::delta_matches(change, trigrams));
        }
        let base_file_id = Self::mmap_file_id(backing, path);
        for trigram in trigrams {
            let in_delta = change
                .trigrams
                .as_ref()
                .is_some_and(|available| available.binary_search(trigram).is_ok());
            if in_delta {
                continue;
            }
            let in_base = if let Some(file_id) = base_file_id {
                Self::validated_mmap_lookup_trigram(backing, trigram)?
                    .is_some_and(|posting| Self::mmap_posting_contains(backing, posting, file_id))
            } else {
                false
            };
            if !in_base {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Merge mapped candidates with the bounded changed-path overlay.
    ///
    /// Base matches for touched paths are delayed until the overlay is checked,
    /// which makes replacement/removal exact and preserves additive `add`
    /// semantics without ever copying the mapped corpus.
    fn visit_mapped_candidates<'a>(
        &'a self,
        trigrams: &[[u8; 3]],
        mut visit: impl FnMut(&'a str) -> Result<()>,
    ) -> Result<()> {
        let backing = self.mmap.as_ref().expect("mapped candidate base");
        let mut changed_matches = Vec::with_capacity(self.delta.len());
        for (path, change) in &self.delta {
            if Self::mapped_change_matches(backing, path, change, trigrams)? {
                changed_matches.push(path.as_str());
            }
        }
        let mut changed_index = 0usize;
        Self::visit_mmap_candidate_ids(backing, trigrams, |file_id| {
            let path = Self::mmap_file_path(backing, file_id).ok_or_else(|| {
                CodixingError::Serialization(
                    "validated file trigram candidate path is missing".to_string(),
                )
            })?;
            while changed_matches
                .get(changed_index)
                .is_some_and(|changed| *changed < path)
            {
                visit(changed_matches[changed_index])?;
                changed_index += 1;
            }
            if self.delta.contains_key(path) {
                if changed_matches
                    .get(changed_index)
                    .is_some_and(|changed| *changed == path)
                {
                    visit(changed_matches[changed_index])?;
                    changed_index += 1;
                }
                return Ok(());
            }
            visit(path)
        })?;

        for path in &changed_matches[changed_index..] {
            visit(path)?;
        }
        Ok(())
    }

    /// Index all trigrams from `content` under `path`.
    ///
    /// Safe to call multiple times with the same `path` (e.g. once per chunk):
    /// duplicate `(trigram, file_index)` pairs are deduplicated.
    pub fn add(&mut self, path: &str, content: &[u8]) {
        let trigrams = Self::prepare_trigrams(content);
        self.add_prepared(path, &trigrams);
    }

    /// Scan, sort, and deduplicate one file's trigrams without touching the
    /// shared index. Init workers use this before taking the short merge lock.
    pub(crate) fn prepare_trigrams(content: &[u8]) -> Vec<[u8; 3]> {
        Self::prepare_contents(std::iter::once(content))
    }

    /// Scan several representations of one file and return their deduplicated
    /// trigram union. Exact search uses this to cover parser-produced chunk
    /// text while grep retains the trigrams from the original source bytes.
    pub(crate) fn prepare_contents<'a>(
        contents: impl IntoIterator<Item = &'a [u8]>,
    ) -> Vec<[u8; 3]> {
        let mut trigrams = Vec::new();
        for content in contents {
            trigrams.extend(
                content
                    .windows(3)
                    .map(|window| [window[0], window[1], window[2]]),
            );
        }
        trigrams.sort_unstable();
        trigrams.dedup();
        trigrams
    }

    /// Merge an already prepared full-file trigram set into the index.
    pub(crate) fn add_prepared(&mut self, path: &str, trigrams: &[[u8; 3]]) {
        if self.mmap.is_some() {
            let change = self
                .delta
                .entry(path.to_string())
                .or_insert_with(|| FileTrigramDelta {
                    exclude_base: false,
                    trigrams: Some(Vec::new()),
                });
            let available = change.trigrams.get_or_insert_with(Vec::new);
            available.extend_from_slice(trigrams);
            available.sort_unstable();
            available.dedup();
            return;
        }
        let file_idx = if let Some(&idx) = self.file_index.get(path) {
            idx
        } else {
            let idx = self.files.len() as u32;
            self.files.push(path.to_string());
            self.file_index.insert(path.to_string(), idx);
            idx
        };

        for &tri in trigrams {
            let list = self.index.entry(tri).or_default();
            // Newly assigned file IDs are monotonic, so the normal indexing
            // path appends in O(1). Updates can revisit an older ID and retain
            // the sorted/deduplicated invariant through the binary-search path.
            match list.last().copied() {
                None => list.push(file_idx),
                Some(last) if last < file_idx => list.push(file_idx),
                Some(last) if last == file_idx => {}
                Some(_) => match list.binary_search(&file_idx) {
                    Ok(_) => {}
                    Err(pos) => list.insert(pos, file_idx),
                },
            }
        }
    }

    /// Returns candidate file paths for a **literal** pattern.
    ///
    /// Returns `None` when trigram pre-filtering cannot be applied safely —
    /// either the literal is shorter than 3 bytes or a lazily validated mapped
    /// posting is corrupt. The caller must fall back to a full scan. Returns
    /// `Some([])` only when a valid index proves that no file can match.
    pub fn candidates_for_literal<'a>(&'a self, literal: &[u8]) -> Option<Vec<&'a str>> {
        if self.disabled || literal.len() < 3 {
            return None;
        }
        let mut trigrams: Vec<[u8; 3]> = (0..literal.len() - 2)
            .map(|i| [literal[i], literal[i + 1], literal[i + 2]])
            .collect();
        trigrams.sort_unstable();
        trigrams.dedup();

        if self.mmap.is_some() {
            let mut candidates = Vec::new();
            let result = self.visit_mapped_candidates(&trigrams, |path| {
                candidates.push(path);
                Ok(())
            });
            if let Err(error) = result {
                tracing::warn!(%error, "invalid mapped file-trigram posting; disabling literal prefilter");
                return None;
            }
            return Some(candidates);
        }

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

    /// Visit candidate file paths for a literal without materializing a
    /// corpus-sized candidate vector.
    ///
    /// Returns `Ok(None)` when `literal` is shorter than three bytes and a
    /// trigram pre-filter cannot be applied. Otherwise every live file that
    /// contains all required trigrams is passed to `visit` exactly once.
    /// Callers must still verify the full literal against stored content,
    /// because trigram intersection can produce false positives.
    pub(crate) fn visit_literal_candidates<'a>(
        &'a self,
        literal: &[u8],
        mut visit: impl FnMut(&'a str) -> Result<()>,
    ) -> Result<Option<()>> {
        if self.disabled || literal.len() < 3 {
            return Ok(None);
        }

        let mut trigrams: Vec<[u8; 3]> = (0..literal.len() - 2)
            .map(|i| [literal[i], literal[i + 1], literal[i + 2]])
            .collect();
        trigrams.sort_unstable();
        trigrams.dedup();

        if self.mmap.is_some() {
            self.visit_mapped_candidates(&trigrams, visit)?;
            return Ok(Some(()));
        }

        let mut lists: Vec<&Vec<u32>> = Vec::with_capacity(trigrams.len());
        for trigram in &trigrams {
            let Some(list) = self.index.get(trigram) else {
                return Ok(Some(()));
            };
            lists.push(list);
        }
        lists.sort_unstable_by_key(|list| list.len());

        let (shortest, remaining) = lists.split_first().expect("literal has a trigram");
        for &file_id in shortest.iter() {
            if remaining
                .iter()
                .all(|list| list.binary_search(&file_id).is_ok())
            {
                let path = self.files[file_id as usize].as_str();
                if !path.is_empty() {
                    visit(path)?;
                }
            }
        }

        Ok(Some(()))
    }

    /// Returns candidate file paths given a set of trigrams that **all** must
    /// be present in any matching file (AND semantics).
    ///
    /// Typically fed the output of [`extract_required_trigrams`]. Returns
    /// `None` when the trigram set is empty or mapped posting validation fails,
    /// so callers conservatively scan without a prefilter.
    pub fn candidates_for_trigrams<'a>(&'a self, trigrams: &[[u8; 3]]) -> Option<Vec<&'a str>> {
        if self.disabled || trigrams.is_empty() {
            return None;
        }

        if self.mmap.is_some() {
            let mut trigrams = trigrams.to_vec();
            trigrams.sort_unstable();
            trigrams.dedup();
            let mut candidates = Vec::new();
            let result = self.visit_mapped_candidates(&trigrams, |path| {
                candidates.push(path);
                Ok(())
            });
            if let Err(error) = result {
                tracing::warn!(%error, "invalid mapped file-trigram posting; disabling regex prefilter");
                return None;
            }
            return Some(candidates);
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
    /// Tombstones the file in O(1). Posting-list compaction is deliberately
    /// deferred to the checkpoint boundary, so one editor save never scans the
    /// repository-wide trigram map.
    pub fn remove_file(&mut self, path: &str) {
        if let Some(backing) = self.mmap.as_ref() {
            if Self::mmap_file_id(backing, path).is_some() {
                self.delta.insert(
                    path.to_string(),
                    FileTrigramDelta {
                        exclude_base: true,
                        trigrams: None,
                    },
                );
            } else {
                self.delta.remove(path);
            }
            return;
        }
        let file_idx = match self.file_index.remove(path) {
            Some(idx) => idx,
            None => return,
        };
        self.files[file_idx as usize] = String::new();
    }

    /// Remove tombstoned file IDs from every posting list and densely remap
    /// survivors. This global O(index) work belongs at the durable checkpoint,
    /// never on the changed-file hot path.
    pub(crate) fn compact_tombstones(&mut self) {
        // Current mmap files are already canonical and cannot contain
        // tombstones. Avoid materializing a read-only generation for a no-op
        // checkpoint/save.
        if self.mmap.is_some() {
            return;
        }
        if self.files.len() == self.file_index.len() {
            return;
        }

        let mut remap = vec![None; self.files.len()];
        let mut files = Vec::with_capacity(self.file_index.len());
        for (old_idx, path) in self.files.iter().enumerate() {
            if path.is_empty() {
                continue;
            }
            let new_idx = files.len() as u32;
            remap[old_idx] = Some(new_idx);
            files.push(path.clone());
        }

        self.index.retain(|_, list| {
            let mut write = 0usize;
            for read in 0..list.len() {
                if let Some(new_idx) = remap.get(list[read] as usize).copied().flatten() {
                    list[write] = new_idx;
                    write += 1;
                }
            }
            list.truncate(write);
            !list.is_empty()
        });

        self.file_index = files
            .iter()
            .enumerate()
            .map(|(idx, path)| (path.clone(), idx as u32))
            .collect();
        self.files = files;
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
                    result.extend(self.execute_plan(sub)?);
                }
                Some(result.into_iter().collect())
            }
        }
    }

    /// Number of files currently in the index.
    pub fn file_count(&self) -> usize {
        self.mmap.as_ref().map_or_else(
            || self.file_index.len(),
            |backing| {
                self.delta
                    .iter()
                    .fold(backing.file_count as usize, |count, (path, change)| {
                        let in_base = Self::mmap_file_id(backing, path).is_some();
                        match (in_base, change.trigrams.is_some()) {
                            (true, false) => count - 1,
                            (false, true) => count + 1,
                            _ => count,
                        }
                    })
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn is_mmap_backed_for_test(&self) -> bool {
        self.mmap.is_some()
    }

    #[cfg(test)]
    pub(crate) fn pending_delta_len_for_test(&self) -> usize {
        self.delta.len()
    }

    /// Save the file trigram index to its mmap-friendly binary format.
    pub fn save_binary(&self, path: &Path) -> Result<()> {
        if self.disabled {
            return Err(CodixingError::Serialization(
                "cannot persist a disabled file trigram index".to_string(),
            ));
        }
        if let Some(backing) = self.mmap.as_ref() {
            if !self.delta.is_empty() {
                return self.save_mmap_with_delta(path, backing);
            }
            return Self::save_mmap_bytes(path, backing);
        }
        let data = Self::canonical_data(
            self.files.clone(),
            self.index
                .iter()
                .map(|(trigram, postings)| (*trigram, postings.clone())),
        )?;
        Self::save_data(path, &data)
    }

    /// Persist and consume a freshly-built index without cloning every posting.
    ///
    /// Initialization does not retain this in-memory representation after the
    /// durable file is published, so moving its vectors into the serializer
    /// avoids stacking a corpus-sized clone on top of the live build state.
    pub(crate) fn save_binary_consuming(self, path: &Path) -> Result<()> {
        let Self {
            files,
            file_index,
            index,
            mmap,
            delta,
            disabled,
        } = self;
        debug_assert!(
            !disabled,
            "disabled file-trigram indexes are never persisted"
        );
        debug_assert!(delta.is_empty(), "fresh in-memory saves have no mmap delta");
        if let Some(backing) = mmap.as_ref() {
            return Self::save_mmap_bytes(path, backing);
        }
        drop(file_index);
        let data = Self::canonical_data(files, index)?;
        Self::save_data(path, &data)
    }

    /// Persist an unchanged mapping when a checkpoint targets another file.
    ///
    /// The source generation itself is already durable, so saving back to that
    /// same existing file is a no-op. Every other destination receives the
    /// validated bytes through the normal crash-safe atomic writer.
    fn save_mmap_bytes(path: &Path, backing: &FileTrigramMmap) -> Result<()> {
        let is_source = std::fs::canonicalize(path)
            .ok()
            .is_some_and(|destination| destination == backing.source_path);
        if is_source {
            return Ok(());
        }
        Self::validate_all_mmap_postings(backing)?;
        crate::persistence::atomic_write(path, &backing.mmap[..])?;
        Ok(())
    }

    #[cfg(all(unix, test))]
    fn paths_alias(left: &Path, right: &Path) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt;

        let left = std::fs::metadata(left)?;
        let right = std::fs::metadata(right)?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }

    #[cfg(windows)]
    fn paths_alias(left: &Path, right: &Path) -> std::io::Result<bool> {
        use std::ffi::c_void;
        use std::mem::MaybeUninit;
        use std::os::windows::io::AsRawHandle;

        #[repr(C)]
        #[allow(dead_code)]
        struct FileTime {
            low_date_time: u32,
            high_date_time: u32,
        }

        #[repr(C)]
        #[allow(dead_code)]
        struct ByHandleFileInformation {
            file_attributes: u32,
            creation_time: FileTime,
            last_access_time: FileTime,
            last_write_time: FileTime,
            volume_serial_number: u32,
            file_size_high: u32,
            file_size_low: u32,
            number_of_links: u32,
            file_index_high: u32,
            file_index_low: u32,
        }

        #[link(name = "kernel32")]
        unsafe extern "system" {
            #[link_name = "GetFileInformationByHandle"]
            fn get_file_information_by_handle(
                file: *mut c_void,
                information: *mut ByHandleFileInformation,
            ) -> i32;
        }

        fn identity(path: &Path) -> std::io::Result<(u32, u64)> {
            let file = std::fs::File::open(path)?;
            let mut information = MaybeUninit::<ByHandleFileInformation>::uninit();
            // SAFETY: `information` points to writable storage for the exact
            // Windows structure, and `file` remains open for the whole call.
            let success = unsafe {
                get_file_information_by_handle(
                    file.as_raw_handle().cast::<c_void>(),
                    information.as_mut_ptr(),
                )
            };
            if success == 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: a successful Win32 call initialized every field.
            let information = unsafe { information.assume_init() };
            let file_index = (u64::from(information.file_index_high) << 32)
                | u64::from(information.file_index_low);
            Ok((information.volume_serial_number, file_index))
        }

        Ok(identity(left)? == identity(right)?)
    }

    fn mmap_file_lower_bound(backing: &FileTrigramMmap, path: &str) -> u32 {
        let mut low = 0u32;
        let mut high = backing.file_count;
        while low < high {
            let middle = low + (high - low) / 2;
            let candidate =
                Self::mmap_file_path(backing, middle).expect("validated file-trigram path table");
            if candidate < path {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        low
    }

    fn build_merge_plan<'a>(&'a self, backing: &FileTrigramMmap) -> FileTrigramMergePlan<'a> {
        let mut removed_base_paths = Vec::new();
        let mut added_paths = Vec::new();
        let mut overlay_files = Vec::new();
        for (path, change) in &self.delta {
            let in_base = Self::mmap_file_id(backing, path).is_some();
            match (in_base, change.trigrams.as_ref()) {
                (true, None) => removed_base_paths.push(path.as_str()),
                (false, Some(_)) => added_paths.push(path.as_str()),
                _ => {}
            }
            if let Some(trigrams) = &change.trigrams {
                overlay_files.push((path.as_str(), trigrams.as_slice()));
            }
        }
        Self::record_checkpoint_scratch(
            overlay_files
                .len()
                .max(removed_base_paths.len() + added_paths.len()),
        );
        FileTrigramMergePlan {
            removed_base_paths,
            added_paths,
            overlay_files,
        }
    }

    fn final_file_id(
        backing: &FileTrigramMmap,
        plan: &FileTrigramMergePlan<'_>,
        path: &str,
    ) -> Result<u32> {
        let base_before = Self::mmap_file_lower_bound(backing, path) as usize;
        let removed_before = plan
            .removed_base_paths
            .partition_point(|removed| *removed < path);
        let added_before = plan.added_paths.partition_point(|added| *added < path);
        u32::try_from(base_before - removed_before + added_before).map_err(|_| {
            CodixingError::Serialization("file trigram merged file ID exceeds u32::MAX".to_string())
        })
    }

    fn remapped_base_file_id(plan: &FileTrigramMergePlan<'_>, old_file_id: u32, path: &str) -> u32 {
        let removed_before = plan
            .removed_base_paths
            .partition_point(|removed| *removed < path);
        let added_before = plan.added_paths.partition_point(|added| *added < path);
        old_file_id - removed_before as u32 + added_before as u32
    }

    fn visit_final_files(
        &self,
        backing: &FileTrigramMmap,
        plan: &FileTrigramMergePlan<'_>,
        mut visit: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let mut added = plan.added_paths.iter().peekable();
        for file_id in 0..backing.file_count {
            let path =
                Self::mmap_file_path(backing, file_id).expect("validated file-trigram path table");
            while added.peek().is_some_and(|candidate| **candidate < path) {
                visit(added.next().unwrap())?;
            }
            if self
                .delta
                .get(path)
                .is_some_and(|change| change.trigrams.is_none())
            {
                continue;
            }
            visit(path)?;
        }
        for path in added {
            visit(path)?;
        }
        Ok(())
    }

    fn visit_final_posting(
        &self,
        backing: &FileTrigramMmap,
        plan: &FileTrigramMergePlan<'_>,
        trigram: &[u8; 3],
        overlay: &[&str],
        mut visit: impl FnMut(u32) -> Result<()>,
    ) -> Result<()> {
        let mut overlay_index = 0usize;

        if let Some(posting) = Self::mmap_lookup_trigram(backing, trigram) {
            let mut previous_file_id = None;
            for posting_index in 0..posting.count as usize {
                let old_file_id = Self::mmap_posting_value(backing, posting, posting_index)
                    .ok_or_else(|| {
                        CodixingError::Serialization(
                            "file trigram checkpoint found a truncated posting".to_string(),
                        )
                    })?;
                if old_file_id >= backing.file_count {
                    return Err(CodixingError::Serialization(format!(
                        "file trigram checkpoint posting ID {old_file_id} exceeds file count {}",
                        backing.file_count
                    )));
                }
                if previous_file_id.is_some_and(|previous| previous >= old_file_id) {
                    return Err(CodixingError::Serialization(
                        "file trigram checkpoint posting IDs are not strictly increasing"
                            .to_string(),
                    ));
                }
                previous_file_id = Some(old_file_id);
                let path = Self::mmap_file_path(backing, old_file_id).ok_or_else(|| {
                    CodixingError::Serialization(
                        "file trigram checkpoint posting path is invalid".to_string(),
                    )
                })?;
                while overlay
                    .get(overlay_index)
                    .is_some_and(|overlay_path| *overlay_path < path)
                {
                    let overlay_path = overlay[overlay_index];
                    visit(Self::final_file_id(backing, plan, overlay_path)?)?;
                    overlay_index += 1;
                }
                if overlay
                    .get(overlay_index)
                    .is_some_and(|overlay_path| *overlay_path == path)
                {
                    visit(Self::final_file_id(backing, plan, path)?)?;
                    overlay_index += 1;
                    continue;
                }
                if self
                    .delta
                    .get(path)
                    .is_some_and(|change| change.exclude_base)
                {
                    continue;
                }
                visit(Self::remapped_base_file_id(plan, old_file_id, path))?;
            }
        }
        for path in &overlay[overlay_index..] {
            visit(Self::final_file_id(backing, plan, path)?)?;
        }
        Ok(())
    }

    fn next_overlay_group<'a>(
        plan: &FileTrigramMergePlan<'a>,
        heap: &mut BinaryHeap<Reverse<([u8; 3], usize, usize)>>,
        paths: &mut Vec<&'a str>,
    ) -> Option<[u8; 3]> {
        paths.clear();
        let Reverse((trigram, file_index, position)) = heap.pop()?;
        paths.push(plan.overlay_files[file_index].0);
        #[cfg(test)]
        FILE_TRIGRAM_OVERLAY_MEMBERSHIPS_VISITED.with(|count| count.set(count.get() + 1));
        let trigrams = plan.overlay_files[file_index].1;
        if let Some(next) = trigrams.get(position + 1) {
            heap.push(Reverse((*next, file_index, position + 1)));
        }
        while heap.peek().is_some_and(|entry| entry.0.0 == trigram) {
            let Reverse((_, duplicate_file, duplicate_position)) = heap.pop().unwrap();
            paths.push(plan.overlay_files[duplicate_file].0);
            #[cfg(test)]
            FILE_TRIGRAM_OVERLAY_MEMBERSHIPS_VISITED.with(|count| count.set(count.get() + 1));
            let duplicate_trigrams = plan.overlay_files[duplicate_file].1;
            if let Some(next) = duplicate_trigrams.get(duplicate_position + 1) {
                heap.push(Reverse((*next, duplicate_file, duplicate_position + 1)));
            }
        }
        Self::record_checkpoint_scratch(heap.len().max(paths.len()));
        Some(trigram)
    }

    fn visit_final_trigrams(
        &self,
        backing: &FileTrigramMmap,
        plan: &FileTrigramMergePlan<'_>,
        mut visit: impl FnMut([u8; 3], &[&str]) -> Result<()>,
    ) -> Result<()> {
        let mut overlay_heap: BinaryHeap<Reverse<([u8; 3], usize, usize)>> = BinaryHeap::new();
        for (file_index, (_, trigrams)) in plan.overlay_files.iter().enumerate() {
            if let Some(first) = trigrams.first() {
                overlay_heap.push(Reverse((*first, file_index, 0)));
            }
        }
        Self::record_checkpoint_scratch(overlay_heap.len());
        let mut overlay_paths = Vec::with_capacity(plan.overlay_files.len());

        let mut base_index = 0usize;
        let mut overlay = Self::next_overlay_group(plan, &mut overlay_heap, &mut overlay_paths);
        while base_index < backing.trigram_count as usize || overlay.is_some() {
            let base = (base_index < backing.trigram_count as usize).then(|| {
                let entry =
                    backing.trigram_index_offset + base_index * FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE;
                [
                    backing.mmap[entry],
                    backing.mmap[entry + 1],
                    backing.mmap[entry + 2],
                ]
            });
            match (base, overlay) {
                (Some(base), Some(overlay_trigram)) if base < overlay_trigram => {
                    base_index += 1;
                    visit(base, &[])?;
                }
                (Some(base), Some(overlay_trigram)) if overlay_trigram < base => {
                    visit(overlay_trigram, &overlay_paths)?;
                    overlay = Self::next_overlay_group(plan, &mut overlay_heap, &mut overlay_paths);
                }
                (Some(base), Some(_)) => {
                    base_index += 1;
                    visit(base, &overlay_paths)?;
                    overlay = Self::next_overlay_group(plan, &mut overlay_heap, &mut overlay_paths);
                }
                (Some(base), None) => {
                    base_index += 1;
                    visit(base, &[])?;
                }
                (None, Some(overlay_trigram)) => {
                    visit(overlay_trigram, &overlay_paths)?;
                    overlay = Self::next_overlay_group(plan, &mut overlay_heap, &mut overlay_paths);
                }
                (None, None) => break,
            }
        }
        Ok(())
    }

    /// Stream a mapped base plus bounded changed-file overlay into canonical
    /// FTRI v1 without collecting corpus-sized paths, postings, or output bytes.
    fn save_mmap_with_delta(&self, path: &Path, backing: &FileTrigramMmap) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};

        #[cfg(windows)]
        if path.exists() && Self::paths_alias(path, &backing.source_path)? {
            return Err(CodixingError::Serialization(
                "cannot checkpoint a mapped file-trigram delta onto its Windows source or a hard-link alias"
                    .to_string(),
            ));
        }

        let plan = self.build_merge_plan(backing);
        let to_u32 = |what: &str, value: usize| -> Result<u32> {
            u32::try_from(value).map_err(|_| {
                CodixingError::Serialization(format!(
                    "file trigram {what} {value} exceeds u32::MAX"
                ))
            })
        };

        let mut file_count = 0usize;
        let mut string_pool_bytes = 0usize;
        self.visit_final_files(backing, &plan, |file_path| {
            file_count = file_count.checked_add(1).ok_or_else(|| {
                CodixingError::Serialization("file trigram file count overflow".to_string())
            })?;
            string_pool_bytes =
                string_pool_bytes
                    .checked_add(file_path.len())
                    .ok_or_else(|| {
                        CodixingError::Serialization("file trigram path bytes overflow".to_string())
                    })?;
            Ok(())
        })?;

        let mut trigram_count = 0usize;
        let mut total_postings = 0usize;
        self.visit_final_trigrams(backing, &plan, |trigram, overlay| {
            let mut posting_count = 0usize;
            self.visit_final_posting(backing, &plan, &trigram, overlay, |_| {
                posting_count += 1;
                Ok(())
            })?;
            if posting_count != 0 {
                trigram_count += 1;
                total_postings = total_postings.checked_add(posting_count).ok_or_else(|| {
                    CodixingError::Serialization("file trigram posting count overflow".to_string())
                })?;
            }
            Ok(())
        })?;

        let file_count_u32 = to_u32("file count", file_count)?;
        let trigram_count_u32 = to_u32("trigram count", trigram_count)?;
        let total_postings_u32 = to_u32("posting count", total_postings)?;
        let string_pool_bytes_u32 = to_u32("path bytes", string_pool_bytes)?;
        let posting_bytes = total_postings.checked_mul(4).ok_or_else(|| {
            CodixingError::Serialization("file trigram posting bytes overflow".to_string())
        })?;
        let posting_bytes_u32 = to_u32("posting bytes", posting_bytes)?;
        let file_table_bytes = file_count
            .checked_mul(FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram table size overflow".to_string())
            })?;
        let trigram_index_bytes = trigram_count
            .checked_mul(FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram index size overflow".to_string())
            })?;
        let trigram_index_offset = FILE_TRIGRAM_MMAP_HEADER_SIZE + file_table_bytes;
        let string_pool_offset = trigram_index_offset + trigram_index_bytes;
        let postings_offset = string_pool_offset + string_pool_bytes;
        let total_bytes = postings_offset + posting_bytes;

        crate::persistence::atomic_write_with(path, |file| {
            let write_result = (|| -> Result<()> {
                // The merge walks millions of compact postings. Buffer each
                // run so a four-byte posting does not become its own kernel
                // write; seeks still flush at the trigram-table boundaries.
                let mut file = std::io::BufWriter::new(file);
                file.write_all(&FILE_TRIGRAM_MMAP_MAGIC.to_le_bytes())?;
                file.write_all(&FILE_TRIGRAM_MMAP_VERSION.to_le_bytes())?;
                file.write_all(&file_count_u32.to_le_bytes())?;
                file.write_all(&trigram_count_u32.to_le_bytes())?;
                file.write_all(&total_postings_u32.to_le_bytes())?;
                file.write_all(&string_pool_bytes_u32.to_le_bytes())?;
                file.write_all(&posting_bytes_u32.to_le_bytes())?;
                file.write_all(&0u32.to_le_bytes())?;

                let mut path_offset = 0usize;
                self.visit_final_files(backing, &plan, |file_path| {
                    file.write_all(&to_u32("path offset", path_offset)?.to_le_bytes())?;
                    file.write_all(&to_u32("path length", file_path.len())?.to_le_bytes())?;
                    path_offset += file_path.len();
                    Ok(())
                })?;
                file.seek(SeekFrom::Start(string_pool_offset as u64))?;
                self.visit_final_files(backing, &plan, |file_path| {
                    file.write_all(file_path.as_bytes())?;
                    Ok(())
                })?;

                let mut index_position = trigram_index_offset as u64;
                let mut posting_offset = 0usize;
                file.seek(SeekFrom::Start(postings_offset as u64))?;
                self.visit_final_trigrams(backing, &plan, |trigram, overlay| {
                    let mut posting_count = 0usize;
                    self.visit_final_posting(backing, &plan, &trigram, overlay, |file_id| {
                        file.write_all(&file_id.to_le_bytes())?;
                        posting_count += 1;
                        Ok(())
                    })?;
                    if posting_count == 0 {
                        return Ok(());
                    }
                    let posting_end = file.stream_position()?;
                    file.seek(SeekFrom::Start(index_position))?;
                    file.write_all(&trigram)?;
                    file.write_all(&[0])?;
                    file.write_all(&to_u32("posting offset", posting_offset)?.to_le_bytes())?;
                    file.write_all(&to_u32("posting length", posting_count)?.to_le_bytes())?;
                    file.write_all(&0u32.to_le_bytes())?;
                    index_position += FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE as u64;
                    posting_offset += posting_count * 4;
                    file.seek(SeekFrom::Start(posting_end))?;
                    Ok(())
                })?;
                file.flush()?;
                file.get_mut().set_len(total_bytes as u64)?;
                Ok(())
            })();
            write_result.map_err(|error| std::io::Error::other(error.to_string()))
        })?;
        Ok(())
    }

    /// Canonicalize file IDs and trigram ordering before persistence.
    ///
    /// Init workers merge files in scheduler order, while incremental indexes
    /// can contain tombstoned IDs. Sorting live paths and remapping postings
    /// makes equivalent indexes byte-identical without cloning posting vectors
    /// in the consuming save path.
    fn canonical_data(
        files: Vec<String>,
        index: impl IntoIterator<Item = ([u8; 3], Vec<u32>)>,
    ) -> Result<FileTrigramIndexData> {
        let original_file_count = files.len();
        let mut live_files: Vec<(String, usize)> = files
            .into_iter()
            .enumerate()
            .filter_map(|(old_id, path)| (!path.is_empty()).then_some((path, old_id)))
            .collect();
        live_files.sort_unstable_by(|left, right| left.0.cmp(&right.0));

        let mut remap = vec![None; original_file_count];
        let mut files = Vec::with_capacity(live_files.len());
        for (new_id, (path, old_id)) in live_files.into_iter().enumerate() {
            remap[old_id] = Some(u32::try_from(new_id).map_err(|_| {
                CodixingError::Serialization(
                    "file trigram index exceeds the u32 file-ID limit".to_string(),
                )
            })?);
            files.push(path);
        }

        let mut index: Vec<_> = index
            .into_iter()
            .filter_map(|(trigram, mut postings)| {
                let mut write = 0usize;
                for read in 0..postings.len() {
                    let Some(new_id) = postings
                        .get(read)
                        .and_then(|old_id| remap.get(*old_id as usize))
                        .copied()
                        .flatten()
                    else {
                        continue;
                    };
                    postings[write] = new_id;
                    write += 1;
                }
                postings.truncate(write);
                postings.sort_unstable();
                postings.dedup();
                (!postings.is_empty()).then_some((trigram, postings))
            })
            .collect();
        index.sort_unstable_by_key(|(trigram, _)| *trigram);

        Ok(FileTrigramIndexData { files, index })
    }

    fn save_data(path: &Path, data: &FileTrigramIndexData) -> Result<()> {
        let to_u32 = |what: &str, value: usize| -> Result<u32> {
            u32::try_from(value).map_err(|_| {
                CodixingError::Serialization(format!(
                    "file trigram {what} {value} exceeds u32::MAX"
                ))
            })
        };

        let file_count = to_u32("file count", data.files.len())?;
        let trigram_count = to_u32("trigram count", data.index.len())?;
        let string_pool_bytes = data.files.iter().try_fold(0usize, |total, path| {
            total.checked_add(path.len()).ok_or_else(|| {
                CodixingError::Serialization("file trigram path bytes overflow".to_string())
            })
        })?;
        let total_postings = data.index.iter().try_fold(0usize, |total, (_, postings)| {
            total.checked_add(postings.len()).ok_or_else(|| {
                CodixingError::Serialization("file trigram posting count overflow".to_string())
            })
        })?;
        let posting_bytes = total_postings.checked_mul(4).ok_or_else(|| {
            CodixingError::Serialization("file trigram posting bytes overflow".to_string())
        })?;

        let file_table_bytes = data
            .files
            .len()
            .checked_mul(FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram table size overflow".to_string())
            })?;
        let trigram_index_bytes = data
            .index
            .len()
            .checked_mul(FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram index size overflow".to_string())
            })?;
        let total_bytes = FILE_TRIGRAM_MMAP_HEADER_SIZE
            .checked_add(file_table_bytes)
            .and_then(|size| size.checked_add(trigram_index_bytes))
            .and_then(|size| size.checked_add(string_pool_bytes))
            .and_then(|size| size.checked_add(posting_bytes))
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram file size overflow".to_string())
            })?;

        let string_pool_bytes_u32 = to_u32("path bytes", string_pool_bytes)?;
        let total_postings_u32 = to_u32("posting count", total_postings)?;
        let posting_bytes_u32 = to_u32("posting bytes", posting_bytes)?;
        let mut bytes = Vec::with_capacity(total_bytes);
        bytes.extend_from_slice(&FILE_TRIGRAM_MMAP_MAGIC.to_le_bytes());
        bytes.extend_from_slice(&FILE_TRIGRAM_MMAP_VERSION.to_le_bytes());
        bytes.extend_from_slice(&file_count.to_le_bytes());
        bytes.extend_from_slice(&trigram_count.to_le_bytes());
        bytes.extend_from_slice(&total_postings_u32.to_le_bytes());
        bytes.extend_from_slice(&string_pool_bytes_u32.to_le_bytes());
        bytes.extend_from_slice(&posting_bytes_u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let mut path_offset = 0usize;
        for file_path in &data.files {
            bytes.extend_from_slice(&to_u32("path offset", path_offset)?.to_le_bytes());
            bytes.extend_from_slice(&to_u32("path length", file_path.len())?.to_le_bytes());
            path_offset += file_path.len();
        }

        let mut posting_offset = 0usize;
        for (trigram, postings) in &data.index {
            bytes.extend_from_slice(trigram);
            bytes.push(0);
            bytes.extend_from_slice(&to_u32("posting offset", posting_offset)?.to_le_bytes());
            bytes.extend_from_slice(&to_u32("posting length", postings.len())?.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            posting_offset += postings.len() * 4;
        }

        for file_path in &data.files {
            bytes.extend_from_slice(file_path.as_bytes());
        }
        for (_, postings) in &data.index {
            for file_id in postings {
                bytes.extend_from_slice(&file_id.to_le_bytes());
            }
        }

        debug_assert_eq!(bytes.len(), total_bytes);
        crate::persistence::atomic_write(path, bytes)?;
        Ok(())
    }

    /// Load a file trigram index, mmaping current files and accepting legacy
    /// bitcode generations for backwards compatibility.
    pub fn load_binary(path: &Path) -> Result<Self> {
        let mut magic = [0u8; 4];
        {
            use std::io::Read;
            let mut file = std::fs::File::open(path)?;
            if file.read(&mut magic).unwrap_or(0) == magic.len()
                && magic == FILE_TRIGRAM_MMAP_MAGIC.to_le_bytes()
            {
                return Self::load_mmap_binary(path);
            }
        }

        // Existing bitcode generations remain readable and materialize only
        // for this compatibility path. The next checkpoint writes mmap v1.
        let bytes = std::fs::read(path)?;
        let data: FileTrigramIndexData = bitcode::deserialize(&bytes).map_err(|e| {
            CodixingError::Serialization(format!("failed to deserialize file trigram index: {e}"))
        })?;
        let mut seen_trigrams = HashSet::with_capacity(data.index.len());
        for (trigram, postings) in &data.index {
            if !seen_trigrams.insert(*trigram) {
                return Err(CodixingError::Serialization(format!(
                    "legacy file trigram index contains duplicate key {trigram:?}"
                )));
            }
            if postings.is_empty() {
                return Err(CodixingError::Serialization(format!(
                    "legacy file trigram posting {trigram:?} is empty"
                )));
            }
            let mut previous = None;
            for &file_id in postings {
                if file_id as usize >= data.files.len() {
                    return Err(CodixingError::Serialization(format!(
                        "legacy file trigram posting {trigram:?} has out-of-range file ID {file_id}"
                    )));
                }
                if previous.is_some_and(|prior| prior >= file_id) {
                    return Err(CodixingError::Serialization(format!(
                        "legacy file trigram posting {trigram:?} is not strictly sorted"
                    )));
                }
                previous = Some(file_id);
            }
        }
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
            mmap: None,
            delta: BTreeMap::new(),
            disabled: false,
        })
    }

    /// Load the immutable base and its complete changed-path overlay as one
    /// logical index. A missing sidecar is accepted only for legacy active
    /// generations; new publications require the canonical empty artifact.
    pub(crate) fn load_binary_with_delta(path: &Path, delta_bytes: Option<&[u8]>) -> Result<Self> {
        let mut index = Self::load_binary(path)?;
        if let Some(bytes) = delta_bytes {
            index.apply_delta_checkpoint(bytes)?;
        }
        Ok(index)
    }

    fn load_mmap_binary(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let source_path = std::fs::canonicalize(path).map_err(|error| {
            CodixingError::Serialization(format!(
                "failed to resolve file trigram mmap source {}: {error}",
                path.display()
            ))
        })?;
        let file_len = usize::try_from(file.metadata()?.len()).map_err(|_| {
            CodixingError::Serialization("file trigram mmap length exceeds usize".to_string())
        })?;
        if file_len < FILE_TRIGRAM_MMAP_HEADER_SIZE {
            return Err(CodixingError::Serialization(
                "file trigram mmap is smaller than its header".to_string(),
            ));
        }

        // SAFETY: the mapping is read-only and the generation file is opened
        // read-only. Index generations are immutable while an Engine uses them.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|error| {
            CodixingError::Serialization(format!("failed to mmap file trigram index: {error}"))
        })?;
        let read_u32 = |offset: usize| -> Result<u32> {
            let end = offset.checked_add(4).ok_or_else(|| {
                CodixingError::Serialization("file trigram header offset overflow".to_string())
            })?;
            let bytes = mmap.get(offset..end).ok_or_else(|| {
                CodixingError::Serialization("truncated file trigram header".to_string())
            })?;
            Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
        };

        let magic = read_u32(0)?;
        if magic != FILE_TRIGRAM_MMAP_MAGIC {
            return Err(CodixingError::Serialization(format!(
                "invalid file trigram magic 0x{magic:08X}"
            )));
        }
        let version = read_u32(4)?;
        if version != FILE_TRIGRAM_MMAP_VERSION {
            return Err(CodixingError::Serialization(format!(
                "unsupported file trigram mmap version {version}"
            )));
        }

        let file_count = read_u32(8)?;
        let trigram_count = read_u32(12)?;
        let total_postings = read_u32(16)?;
        let string_pool_bytes = read_u32(20)? as usize;
        let posting_bytes = read_u32(24)? as usize;
        if read_u32(28)? != 0 {
            return Err(CodixingError::Serialization(
                "file trigram header reserved field is non-zero".to_string(),
            ));
        }
        let expected_posting_bytes = (total_postings as usize).checked_mul(4).ok_or_else(|| {
            CodixingError::Serialization("file trigram posting byte count overflow".to_string())
        })?;
        if posting_bytes != expected_posting_bytes {
            return Err(CodixingError::Serialization(
                "file trigram posting byte count does not match its header".to_string(),
            ));
        }

        let file_table_offset = FILE_TRIGRAM_MMAP_HEADER_SIZE;
        let file_table_bytes = (file_count as usize)
            .checked_mul(FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram table size overflow".to_string())
            })?;
        let trigram_index_offset =
            file_table_offset
                .checked_add(file_table_bytes)
                .ok_or_else(|| {
                    CodixingError::Serialization("file trigram index offset overflow".to_string())
                })?;
        let trigram_index_bytes = (trigram_count as usize)
            .checked_mul(FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram index size overflow".to_string())
            })?;
        let string_pool_offset = trigram_index_offset
            .checked_add(trigram_index_bytes)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram path offset overflow".to_string())
            })?;
        let postings_offset = string_pool_offset
            .checked_add(string_pool_bytes)
            .ok_or_else(|| {
                CodixingError::Serialization("file trigram posting offset overflow".to_string())
            })?;
        let expected_len = postings_offset.checked_add(posting_bytes).ok_or_else(|| {
            CodixingError::Serialization("file trigram mmap length overflow".to_string())
        })?;
        if expected_len != file_len {
            return Err(CodixingError::Serialization(format!(
                "file trigram mmap length mismatch: expected {expected_len}, found {file_len}"
            )));
        }

        let backing = FileTrigramMmap {
            mmap,
            source_path,
            file_count,
            trigram_count,
            file_table_offset,
            trigram_index_offset,
            string_pool_offset,
            postings_offset,
            posting_validation: Mutex::new(FilePostingValidationCache::default()),
        };

        let mut previous_path: Option<&str> = None;
        let mut expected_path_offset = 0usize;
        for file_id in 0..file_count {
            let relative_entry = (file_id as usize)
                .checked_mul(FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "file trigram path table offset overflow".to_string(),
                    )
                })?;
            let entry = file_table_offset
                .checked_add(relative_entry)
                .ok_or_else(|| {
                    CodixingError::Serialization("file trigram path entry overflow".to_string())
                })?;
            let path_offset =
                u32::from_le_bytes(backing.mmap[entry..entry + 4].try_into().unwrap()) as usize;
            let path_len =
                u32::from_le_bytes(backing.mmap[entry + 4..entry + 8].try_into().unwrap()) as usize;
            if path_offset != expected_path_offset {
                return Err(CodixingError::Serialization(
                    "file trigram path ranges are not canonical and contiguous".to_string(),
                ));
            }
            expected_path_offset = expected_path_offset.checked_add(path_len).ok_or_else(|| {
                CodixingError::Serialization("file trigram path range overflow".to_string())
            })?;
            if expected_path_offset > string_pool_bytes {
                return Err(CodixingError::Serialization(
                    "file trigram path range exceeds its string pool".to_string(),
                ));
            }
            let path = Self::mmap_file_path(&backing, file_id).ok_or_else(|| {
                CodixingError::Serialization(format!(
                    "invalid UTF-8 or range for file trigram path {file_id}"
                ))
            })?;
            if path.is_empty() || previous_path.is_some_and(|previous| previous >= path) {
                return Err(CodixingError::Serialization(
                    "file trigram paths are not strictly sorted".to_string(),
                ));
            }
            previous_path = Some(path);
        }
        if expected_path_offset != string_pool_bytes {
            return Err(CodixingError::Serialization(
                "file trigram path ranges do not cover the string pool".to_string(),
            ));
        }

        let mut previous_trigram: Option<[u8; 3]> = None;
        let mut expected_posting_offset = 0usize;
        let mut counted_postings = 0u64;
        for index in 0..trigram_count as usize {
            let relative_entry = index
                .checked_mul(FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "file trigram index entry offset overflow".to_string(),
                    )
                })?;
            let entry = trigram_index_offset
                .checked_add(relative_entry)
                .ok_or_else(|| {
                    CodixingError::Serialization("file trigram index entry overflow".to_string())
                })?;
            let trigram = [
                backing.mmap[entry],
                backing.mmap[entry + 1],
                backing.mmap[entry + 2],
            ];
            if backing.mmap[entry + 3] != 0
                || u32::from_le_bytes(backing.mmap[entry + 12..entry + 16].try_into().unwrap()) != 0
            {
                return Err(CodixingError::Serialization(
                    "file trigram index reserved field is non-zero".to_string(),
                ));
            }
            if previous_trigram.is_some_and(|previous| previous >= trigram) {
                return Err(CodixingError::Serialization(
                    "file trigram keys are not strictly sorted".to_string(),
                ));
            }
            previous_trigram = Some(trigram);
            let posting_offset =
                u32::from_le_bytes(backing.mmap[entry + 4..entry + 8].try_into().unwrap()) as usize;
            let posting_count =
                u32::from_le_bytes(backing.mmap[entry + 8..entry + 12].try_into().unwrap())
                    as usize;
            if posting_count == 0 {
                return Err(CodixingError::Serialization(
                    "file trigram posting list is empty".to_string(),
                ));
            }
            if !posting_offset.is_multiple_of(4) {
                return Err(CodixingError::Serialization(
                    "file trigram posting offset is not u32-aligned".to_string(),
                ));
            }
            if posting_offset != expected_posting_offset {
                return Err(CodixingError::Serialization(
                    "file trigram posting ranges are not canonical and non-overlapping".to_string(),
                ));
            }
            let posting_len = posting_count.checked_mul(4).ok_or_else(|| {
                CodixingError::Serialization("file trigram posting range overflow".to_string())
            })?;
            let posting_end = posting_offset.checked_add(posting_len).ok_or_else(|| {
                CodixingError::Serialization("file trigram posting range overflow".to_string())
            })?;
            if posting_end > posting_bytes {
                return Err(CodixingError::Serialization(
                    "file trigram posting range exceeds its mmap".to_string(),
                ));
            }
            expected_posting_offset = posting_end;
            counted_postings = counted_postings
                .checked_add(posting_count as u64)
                .ok_or_else(|| {
                    CodixingError::Serialization("file trigram posting count overflow".to_string())
                })?;
        }
        if expected_posting_offset != posting_bytes {
            return Err(CodixingError::Serialization(
                "file trigram posting ranges do not cover the posting section".to_string(),
            ));
        }
        if counted_postings != u64::from(total_postings) {
            return Err(CodixingError::Serialization(
                "file trigram posting count does not match its index".to_string(),
            ));
        }

        Ok(Self {
            files: Vec::new(),
            file_index: HashMap::new(),
            index: HashMap::new(),
            mmap: Some(backing),
            delta: BTreeMap::new(),
            disabled: false,
        })
    }
}

#[cfg(test)]
pub(crate) fn corrupt_file_posting_for_test(path: &Path, trigram: [u8; 3], file_id: u32) {
    let mut bytes = std::fs::read(path).expect("read file-trigram test artifact");
    let read_u32 =
        |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
    let file_count = read_u32(8);
    let trigram_count = read_u32(12);
    let string_pool_bytes = read_u32(20);
    assert!(file_count > 0 && trigram_count > 0);
    let trigram_index_offset =
        FILE_TRIGRAM_MMAP_HEADER_SIZE + file_count * FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE;
    let postings_offset = trigram_index_offset
        + trigram_count * FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE
        + string_pool_bytes;
    let entry = (0..trigram_count)
        .map(|index| trigram_index_offset + index * FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE)
        .find(|entry| bytes[*entry..*entry + 3] == trigram)
        .expect("test trigram must exist");
    let posting_offset = read_u32(entry + 4);
    let target = postings_offset + posting_offset;
    bytes[target..target + 4].copy_from_slice(&file_id.to_le_bytes());
    std::fs::write(path, bytes).expect("corrupt file-trigram test posting");
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
    fn streaming_file_candidates_match_collecting_api() {
        let mut idx = FileTrigramIndex::new();
        idx.add("src/a.rs", b"aaaa shared_literal");
        idx.add("src/b.rs", b"prefix shared_literal suffix");
        idx.add("src/c.rs", b"unrelated");

        let mut expected = idx
            .candidates_for_literal(b"shared_literal")
            .unwrap()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut streamed = Vec::new();
        assert!(
            idx.visit_literal_candidates(b"shared_literal", |path| {
                streamed.push(path.to_string());
                Ok(())
            })
            .unwrap()
            .is_some()
        );
        expected.sort_unstable();
        streamed.sort_unstable();

        assert_eq!(streamed, expected);
    }

    #[test]
    fn streaming_file_candidates_deduplicate_repeated_trigrams_and_skip_tombstones() {
        let mut idx = FileTrigramIndex::new();
        idx.add("src/removed.rs", b"aaaaaaaa");
        idx.add("src/live.rs", b"aaaaaaaa");
        idx.remove_file("src/removed.rs");

        let mut streamed = Vec::new();
        idx.visit_literal_candidates(b"aaaaaa", |path| {
            streamed.push(path.to_string());
            Ok(())
        })
        .unwrap();

        assert_eq!(streamed, vec!["src/live.rs"]);
    }

    #[test]
    fn streaming_file_candidates_report_short_literal_fallback() {
        let mut idx = FileTrigramIndex::new();
        idx.add("src/a.rs", b"ab");
        let mut visited = false;

        let applied = idx
            .visit_literal_candidates(b"ab", |_| {
                visited = true;
                Ok(())
            })
            .unwrap();

        assert!(applied.is_none());
        assert!(!visited);
    }

    #[test]
    fn file_trigram_unions_raw_and_transformed_content_and_replaces_the_path() {
        let mut idx = FileTrigramIndex::new();
        let raw = b"encoded: decoded\\u0020marker".as_slice();
        let transformed = b"decoded marker".as_slice();
        let trigrams = FileTrigramIndex::prepare_contents([raw, transformed]);
        idx.add_prepared("analysis.ipynb", &trigrams);

        assert_eq!(
            idx.candidates_for_literal(b"decoded marker").unwrap(),
            vec!["analysis.ipynb"]
        );
        assert_eq!(
            idx.candidates_for_literal(b"decoded\\u0020marker").unwrap(),
            vec!["analysis.ipynb"]
        );

        idx.remove_file("analysis.ipynb");
        let replacement = FileTrigramIndex::prepare_contents([
            b"encoded: replacement\\u0020marker".as_slice(),
            b"replacement marker",
        ]);
        idx.add_prepared("analysis.ipynb", &replacement);

        assert!(
            idx.candidates_for_literal(b"decoded marker")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            idx.candidates_for_literal(b"replacement marker").unwrap(),
            vec!["analysis.ipynb"]
        );
    }

    #[test]
    fn file_trigram_fast_append_falls_back_for_out_of_order_updates() {
        let mut idx = FileTrigramIndex::new();
        idx.add_prepared("first.rs", &[]);
        idx.add_prepared("second.rs", &[*b"abc"]);
        idx.add_prepared("first.rs", &[*b"abc"]);
        idx.add_prepared("first.rs", &[*b"abc"]);

        assert_eq!(idx.index.get(b"abc").unwrap().as_slice(), &[0, 1]);
        assert_eq!(
            idx.candidates_for_literal(b"abc").unwrap(),
            vec!["first.rs", "second.rs"]
        );
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

    #[test]
    fn v2_save_with_u64_hash_ids_falls_back_to_v1_round_trip() {
        // Chunk IDs are hash-derived u64s in production — far above u32::MAX.
        // save_mmap_binary_v2 must persist them (via the v1 fallback) instead
        // of erroring with a misleading "> 4B chunks" diagnosis.
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram_hash_ids.bin");

        let big_a: u64 = 14_034_699_640_371_163_533; // > u32::MAX
        let big_b: u64 = u64::from(u32::MAX) + 1;
        let mut idx = TrigramIndex::new();
        idx.add(big_a, "fn process_batch(items: &[Item]) { todo!() }");
        idx.add(big_b, "fn main() { process_batch(&items); }");
        idx.add(7, "fn unrelated_function() {}");

        idx.save_mmap_binary_v2(&path, PostingCodec::DeltaVarint)
            .expect("u64 hash IDs must persist via the v1 fallback");
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        let mut hits = loaded.search("process_batch");
        hits.sort_unstable();
        let mut expected = vec![big_a, big_b];
        expected.sort_unstable();
        assert_eq!(hits, expected);
        assert!(!loaded.search("unrelated_function").contains(&big_a));
    }

    #[test]
    fn v2_save_with_small_ids_still_writes_v2() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("trigram_small_ids.bin");

        let mut idx = TrigramIndex::new();
        idx.add(1, "fn process_batch() {}");
        idx.add(2, "fn other_batch() {}");
        idx.save_mmap_binary_v2(&path, PostingCodec::DeltaVarint)
            .unwrap();

        // Version field in the header must read 2 (no fallback for u32 IDs).
        let bytes = std::fs::read(&path).unwrap();
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(version, MMAP_VERSION_V2);

        let loaded = TrigramIndex::load_binary(&path).unwrap();
        let mut hits = loaded.search("batch");
        hits.sort_unstable();
        assert_eq!(hits, vec![1, 2]);
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
    fn file_trigram_tombstones_compact_at_checkpoint_and_round_trip() {
        let mut idx = FileTrigramIndex::new();
        idx.add("removed.rs", b"fn obsolete_target() {}");
        idx.add("kept.rs", b"fn obsolete_target() { live(); }");

        idx.remove_file("removed.rs");
        assert_eq!(
            idx.candidates_for_literal(b"obsolete_target").unwrap(),
            vec!["kept.rs"]
        );
        idx.add("removed.rs", b"fn replacement_target() {}");
        assert_eq!(
            idx.candidates_for_literal(b"obsolete_target").unwrap(),
            vec!["kept.rs"]
        );
        assert_eq!(
            idx.candidates_for_literal(b"replacement_target").unwrap(),
            vec!["removed.rs"]
        );

        idx.compact_tombstones();
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        idx.save_binary(&path).unwrap();
        let loaded = FileTrigramIndex::load_binary(&path).unwrap();
        assert!(loaded.mmap.is_some(), "current format must stay zero-copy");
        assert!(loaded.files.is_empty());
        assert!(loaded.file_index.is_empty());
        assert!(loaded.index.is_empty());
        assert_eq!(loaded.file_count(), 2);
        assert_eq!(
            loaded.candidates_for_literal(b"obsolete_target").unwrap(),
            vec!["kept.rs"]
        );
        assert_eq!(
            loaded
                .candidates_for_literal(b"replacement_target")
                .unwrap(),
            vec!["removed.rs"]
        );
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
    fn file_trigram_consuming_save_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");

        let mut idx = FileTrigramIndex::new();
        idx.add("a.rs", b"fn process_batch() {}");
        idx.add("b.rs", b"fn main() { process_batch(); }");
        idx.save_binary_consuming(&path).unwrap();

        let loaded = FileTrigramIndex::load_binary(&path).unwrap();
        assert_eq!(loaded.file_count(), 2);
        let candidates = loaded.candidates_for_literal(b"process_batch").unwrap();
        assert!(candidates.contains(&"a.rs"));
        assert!(candidates.contains(&"b.rs"));
    }

    #[test]
    fn file_trigram_serialization_is_canonical_across_insertion_order_and_tombstones() {
        fn build(order: &[&str]) -> FileTrigramIndex {
            let mut index = FileTrigramIndex::new();
            for path in order {
                let content: &[u8] = match *path {
                    "a.rs" => b"fn alpha_target() { shared_target(); }",
                    "b.rs" => b"fn beta_target() { shared_target(); }",
                    "removed.rs" => b"fn removed_target() {}",
                    _ => unreachable!(),
                };
                index.add(path, content);
            }
            index.remove_file("removed.rs");
            index
        }

        let dir = tempdir().unwrap();
        let borrowed_path = dir.path().join("borrowed.bin");
        let consuming_path = dir.path().join("consuming.bin");
        let borrowed = build(&["removed.rs", "b.rs", "a.rs"]);
        let consuming = build(&["a.rs", "removed.rs", "b.rs"]);

        borrowed.save_binary(&borrowed_path).unwrap();
        consuming.save_binary_consuming(&consuming_path).unwrap();

        let borrowed_bytes = std::fs::read(&borrowed_path).unwrap();
        let consuming_bytes = std::fs::read(&consuming_path).unwrap();
        assert_eq!(borrowed_bytes, consuming_bytes);

        let loaded = FileTrigramIndex::load_binary(&consuming_path).unwrap();
        assert!(loaded.mmap.is_some());
        let backing = loaded.mmap.as_ref().unwrap();
        let paths: Vec<_> = (0..backing.file_count)
            .map(|file_id| FileTrigramIndex::mmap_file_path(backing, file_id).unwrap())
            .collect();
        assert_eq!(paths, vec!["a.rs", "b.rs"]);
        assert_eq!(
            loaded.candidates_for_literal(b"shared_target").unwrap(),
            vec!["a.rs", "b.rs"]
        );
        assert!(
            loaded
                .candidates_for_literal(b"removed_target")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn file_trigram_mmap_load_defers_posting_validation_and_caches_queries() {
        const FILES: usize = 128;
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        let mut original = FileTrigramIndex::new();
        for index in 0..FILES {
            original.add(
                &format!("src/file_{index:03}.rs"),
                b"fn shared_lazy_validation_target() {}",
            );
        }
        original.save_binary_consuming(&path).unwrap();

        FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| count.set(0));
        let loaded = FileTrigramIndex::load_binary(&path).unwrap();
        FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| assert_eq!(count.get(), 0));

        assert_eq!(
            loaded
                .candidates_for_literal(b"lazy_validation_target")
                .unwrap()
                .len(),
            FILES
        );
        let first_validation_count =
            FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| count.get());
        assert!(first_validation_count > 0);
        assert_eq!(
            loaded
                .candidates_for_literal(b"lazy_validation_target")
                .unwrap()
                .len(),
            FILES
        );
        FILE_TRIGRAM_POSTING_VALUES_VALIDATED
            .with(|count| assert_eq!(count.get(), first_validation_count));
    }

    #[test]
    fn file_trigram_mmap_streaming_visit_does_not_collect_candidates() {
        const FILES: usize = 256;
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        let mut original = FileTrigramIndex::new();
        for index in 0..FILES {
            original.add(
                &format!("src/file_{index:03}.rs"),
                b"fn shared_streaming_target() {}",
            );
        }
        original.save_binary_consuming(&path).unwrap();

        let loaded = FileTrigramIndex::load_binary(&path).unwrap();
        assert!(loaded.mmap.is_some());

        let mut visited = std::collections::HashSet::new();
        loaded
            .visit_literal_candidates(b"shared_streaming_target", |path| {
                assert!(visited.insert(path.to_string()));
                Ok(())
            })
            .unwrap();
        assert_eq!(visited.len(), FILES);

        assert_eq!(
            loaded
                .candidates_for_literal(b"shared_streaming_target")
                .unwrap()
                .len(),
            FILES
        );
        assert!(loaded.is_mmap_backed_for_test());
    }

    #[test]
    fn file_trigram_mmap_mutation_stays_in_bounded_delta() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");

        let mut original = FileTrigramIndex::new();
        original.add("a.rs", b"fn shared_target() {}");
        original.add("b.rs", b"fn shared_target() { old_target(); }");
        original.save_binary_consuming(&path).unwrap();

        let mut loaded = FileTrigramIndex::load_binary(&path).unwrap();
        assert!(loaded.mmap.is_some());
        assert_eq!(
            loaded.candidates_for_literal(b"shared_target").unwrap(),
            vec!["a.rs", "b.rs"]
        );
        assert!(loaded.mmap.is_some(), "reads must not materialize the mmap");

        loaded.remove_file("a.rs");
        assert!(loaded.mmap.is_some(), "mutation must retain the mmap base");
        loaded.remove_file("b.rs");
        loaded.add("b.rs", b"fn replacement_target() {}");
        loaded.add("c.rs", b"fn shared_target() { new_target(); }");
        assert_eq!(loaded.pending_delta_len_for_test(), 3);
        assert_eq!(
            loaded.candidates_for_literal(b"shared_target").unwrap(),
            vec!["c.rs"]
        );
        assert!(
            loaded
                .candidates_for_literal(b"old_target")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            loaded
                .candidates_for_literal(b"replacement_target")
                .unwrap(),
            vec!["b.rs"]
        );
        assert!(
            loaded
                .candidates_for_literal(b"new_target")
                .unwrap()
                .contains(&"c.rs")
        );

        let checkpoint = dir.path().join("checkpoint.bin");
        loaded.save_binary(&checkpoint).unwrap();
        let checkpointed = FileTrigramIndex::load_binary(&checkpoint).unwrap();
        assert!(checkpointed.is_mmap_backed_for_test());
        assert_eq!(checkpointed.file_count(), 2);
        assert_eq!(
            checkpointed
                .candidates_for_literal(b"replacement_target")
                .unwrap(),
            vec!["b.rs"]
        );
        assert_eq!(
            checkpointed
                .candidates_for_literal(b"shared_target")
                .unwrap(),
            vec!["c.rs"]
        );
        assert!(
            checkpointed
                .candidates_for_literal(b"old_target")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn file_trigram_mmap_delta_candidates_stay_sorted_across_checkpoint() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("base.bin");
        let checkpoint = dir.path().join("checkpoint.bin");
        let mut original = FileTrigramIndex::new();
        for path in ["a.rs", "c.rs", "e.rs"] {
            original.add(path, b"shared_sorted_target");
        }
        original.save_binary_consuming(&source).unwrap();

        let mut loaded = FileTrigramIndex::load_binary(&source).unwrap();
        loaded.remove_file("c.rs");
        loaded.add("b.rs", b"shared_sorted_target");
        loaded.add("d.rs", b"shared_sorted_target");
        loaded.remove_file("e.rs");
        loaded.add("e.rs", b"replacement shared_sorted_target");
        let expected = vec!["a.rs", "b.rs", "d.rs", "e.rs"];

        assert_eq!(
            loaded
                .candidates_for_literal(b"shared_sorted_target")
                .unwrap(),
            expected
        );
        let mut streamed = Vec::new();
        loaded
            .visit_literal_candidates(b"shared_sorted_target", |path| {
                streamed.push(path);
                Ok(())
            })
            .unwrap();
        assert_eq!(streamed, expected);

        loaded.save_binary(&checkpoint).unwrap();
        let checkpointed = FileTrigramIndex::load_binary(&checkpoint).unwrap();
        assert_eq!(
            checkpointed
                .candidates_for_literal(b"shared_sorted_target")
                .unwrap(),
            expected
        );
    }

    #[test]
    fn file_trigram_checkpoint_scratch_is_bounded_by_changed_files() {
        const BASE_FILES: usize = 512;
        let dir = tempdir().unwrap();
        let source = dir.path().join("base.bin");
        let checkpoint = dir.path().join("checkpoint.bin");
        let mut original = FileTrigramIndex::new();
        for index in 0..BASE_FILES {
            original.add(
                &format!("src/file_{index:03}.rs"),
                b"common_corpus_wide_target",
            );
        }
        original.save_binary_consuming(&source).unwrap();

        let mut loaded = FileTrigramIndex::load_binary(&source).unwrap();
        let changes: [(&str, &[u8]); 3] = [
            (
                "src/file_000.rs",
                b"delta_alpha_unique_target abcdefghijklmnopqrstuvwxyz 0123456789",
            ),
            (
                "src/file_001.rs",
                b"delta_beta_unique_target zyxwvutsrqponmlkjihgfedcba 9876543210",
            ),
            (
                "src/file_002.rs",
                b"delta_gamma_unique_target the_quick_brown_fox_jumps_over_the_lazy_dog",
            ),
        ];
        for (path, content) in changes {
            loaded.remove_file(path);
            loaded.add(path, content);
        }
        let delta_trigram_memberships: usize = loaded
            .delta
            .values()
            .filter_map(|change| change.trigrams.as_ref())
            .map(Vec::len)
            .sum();
        FILE_TRIGRAM_CHECKPOINT_SCRATCH_PEAK.with(|peak| peak.set(0));
        FILE_TRIGRAM_OVERLAY_MEMBERSHIPS_VISITED.with(|count| count.set(0));
        loaded.save_binary(&checkpoint).unwrap();

        FILE_TRIGRAM_CHECKPOINT_SCRATCH_PEAK.with(|peak| {
            assert!(
                peak.get() <= changes.len(),
                "checkpoint scratch {} must scale with {} changed paths, not their trigrams",
                peak.get(),
                changes.len()
            )
        });
        FILE_TRIGRAM_OVERLAY_MEMBERSHIPS_VISITED.with(|count| {
            assert_eq!(
                count.get(),
                delta_trigram_memberships * 2,
                "the sizing and writing passes must each visit every delta trigram once, independent of the base trigram count"
            )
        });
        assert!(loaded.is_mmap_backed_for_test());
        assert_eq!(loaded.pending_delta_len_for_test(), changes.len());
        let checkpointed = FileTrigramIndex::load_binary(&checkpoint).unwrap();
        assert!(checkpointed.is_mmap_backed_for_test());
        assert_eq!(
            checkpointed
                .candidates_for_literal(b"delta_alpha_unique_target")
                .unwrap(),
            vec!["src/file_000.rs"]
        );
    }

    #[test]
    fn file_trigram_mmap_add_preserves_union_semantics_across_checkpoint() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("base.bin");
        let checkpoint = dir.path().join("checkpoint.bin");
        let mut original = FileTrigramIndex::new();
        original.add("a.rs", b"abc");
        original.save_binary_consuming(&source).unwrap();

        let mut loaded = FileTrigramIndex::load_binary(&source).unwrap();
        loaded.add("a.rs", b"def");
        let required = [*b"abc", *b"def"];
        assert_eq!(
            loaded.candidates_for_trigrams(&required).unwrap(),
            vec!["a.rs"]
        );
        loaded.save_binary(&checkpoint).unwrap();
        let checkpointed = FileTrigramIndex::load_binary(&checkpoint).unwrap();
        assert_eq!(
            checkpointed.candidates_for_trigrams(&required).unwrap(),
            vec!["a.rs"]
        );
    }

    #[test]
    fn file_trigram_delta_replaces_deletes_and_reopens_cumulatively() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("base.bin");
        let mut original = FileTrigramIndex::new();
        original.add("a.rs", b"old_alpha_target");
        original.add("b.rs", b"deleted_beta_target");
        original.add("c.rs", b"old_gamma_target");
        original.save_binary_consuming(&base).unwrap();

        let mut first = FileTrigramIndex::load_binary(&base).unwrap();
        first.remove_file("a.rs");
        first.add("a.rs", b"new_alpha_target");
        first.remove_file("b.rs");
        first.add("d.rs", b"added_delta_target");
        first.add("e.rs", b"added_epsilon_target");
        let first_bytes = first.delta_checkpoint_bytes().unwrap().unwrap();

        let mut reopened =
            FileTrigramIndex::load_binary_with_delta(&base, Some(&first_bytes)).unwrap();
        assert_eq!(reopened.pending_delta_len_for_test(), 4);
        assert!(
            reopened
                .candidates_for_literal(b"old_alpha_target")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .candidates_for_literal(b"new_alpha_target")
                .unwrap(),
            vec!["a.rs"]
        );
        assert!(
            reopened
                .candidates_for_literal(b"deleted_beta_target")
                .unwrap()
                .is_empty()
        );

        reopened.remove_file("c.rs");
        reopened.add("c.rs", b"new_gamma_target");
        // Paths introduced only by the prior sidecar must still support both
        // replacement and deletion after a reopen.
        reopened.remove_file("d.rs");
        reopened.add("d.rs", b"replaced_delta_target");
        reopened.remove_file("e.rs");
        let cumulative = reopened.delta_checkpoint_bytes().unwrap().unwrap();
        let final_index =
            FileTrigramIndex::load_binary_with_delta(&base, Some(&cumulative)).unwrap();
        assert_eq!(final_index.pending_delta_len_for_test(), 4);
        for (query, path) in [
            (b"new_alpha_target".as_slice(), "a.rs"),
            (b"new_gamma_target".as_slice(), "c.rs"),
            (b"replaced_delta_target".as_slice(), "d.rs"),
        ] {
            assert_eq!(
                final_index.candidates_for_literal(query).unwrap(),
                vec![path]
            );
        }
        assert!(
            final_index
                .candidates_for_literal(b"added_delta_target")
                .unwrap()
                .is_empty()
        );
        assert!(
            final_index
                .candidates_for_literal(b"added_epsilon_target")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn file_trigram_delta_encoding_is_deterministic() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("base.bin");
        let mut original = FileTrigramIndex::new();
        original.add("a.rs", b"old_target");
        original.save_binary_consuming(&base).unwrap();

        let mut left = FileTrigramIndex::load_binary(&base).unwrap();
        left.remove_file("a.rs");
        left.add("a.rs", b"new_target");
        left.add("z.rs", b"added_target");
        left.add("module:/logical.rs", b"colon_path_target");

        let mut right = FileTrigramIndex::load_binary(&base).unwrap();
        right.add("module:/logical.rs", b"colon_path_target");
        right.add("z.rs", b"added_target");
        right.remove_file("a.rs");
        right.add("a.rs", b"new_target");

        assert_eq!(
            left.delta_checkpoint_bytes().unwrap(),
            right.delta_checkpoint_bytes().unwrap()
        );
    }

    #[test]
    fn file_trigram_delta_preserves_live_empty_trigram_sets() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("base.bin");
        let mut original = FileTrigramIndex::new();
        original.add("replaced.rs", b"old_target");
        original.save_binary_consuming(&base).unwrap();

        let mut loaded = FileTrigramIndex::load_binary(&base).unwrap();
        loaded.remove_file("replaced.rs");
        loaded.add("replaced.rs", b"x");
        loaded.add("new.rs", b"y");
        let bytes = loaded.delta_checkpoint_bytes().unwrap().unwrap();
        let reopened = FileTrigramIndex::load_binary_with_delta(&base, Some(&bytes)).unwrap();

        assert_eq!(reopened.file_count(), 2);
        assert!(
            reopened
                .candidates_for_literal(b"old_target")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn file_trigram_delta_rejects_corrupt_paths_and_trigram_order() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("base.bin");
        let mut original = FileTrigramIndex::new();
        original.add("a.rs", b"base_target");
        original.save_binary_consuming(&base).unwrap();

        for (checkpoint, expected) in [
            (
                FileTrigramIndex::delta_checkpoint(vec![FileTrigramDeltaEntry {
                    path: "../escape.rs".to_string(),
                    exclude_base: false,
                    trigrams: Some(vec![*b"abc"]),
                }]),
                "safe normalized relative path",
            ),
            (
                FileTrigramIndex::delta_checkpoint(vec![FileTrigramDeltaEntry {
                    path: "new.rs".to_string(),
                    exclude_base: false,
                    trigrams: Some(vec![*b"def", *b"abc"]),
                }]),
                "trigrams are not strictly sorted",
            ),
            (
                FileTrigramIndex::delta_checkpoint(vec![FileTrigramDeltaEntry {
                    path: "C:escape.rs".to_string(),
                    exclude_base: false,
                    trigrams: Some(vec![*b"abc"]),
                }]),
                "safe normalized relative path",
            ),
        ] {
            let bytes = FileTrigramIndex::serialize_delta_checkpoint(checkpoint.entries).unwrap();
            let error = FileTrigramIndex::load_binary_with_delta(&base, Some(&bytes))
                .err()
                .unwrap();
            assert!(error.to_string().contains(expected), "{error}");
        }

        let mut unsafe_encoder = FileTrigramIndex::load_binary(&base).unwrap();
        unsafe_encoder.add("../escape.rs", b"abc");
        assert!(unsafe_encoder.delta_checkpoint_bytes().is_err());

        let mut wrong_magic = FileTrigramIndex::empty_delta_checkpoint().unwrap();
        wrong_magic[0] ^= 0xFF;
        assert!(FileTrigramIndex::load_binary_with_delta(&base, Some(&wrong_magic)).is_err());
    }

    #[test]
    fn file_trigram_delta_path_threshold_requests_compaction() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("base.bin");
        let compacted = dir.path().join("compacted.bin");
        FileTrigramIndex::new()
            .save_binary_consuming(&base)
            .unwrap();
        let mut loaded = FileTrigramIndex::load_binary(&base).unwrap();
        for index in 0..=FILE_TRIGRAM_DELTA_MAX_PATHS {
            loaded.add(&format!("src/new_{index:04}.rs"), b"abc");
        }
        assert!(loaded.delta_checkpoint_bytes().unwrap().is_none());
        loaded.save_binary(&compacted).unwrap();
        let empty = FileTrigramIndex::empty_delta_checkpoint().unwrap();
        let reopened = FileTrigramIndex::load_binary_with_delta(&compacted, Some(&empty)).unwrap();
        assert_eq!(reopened.pending_delta_len_for_test(), 0);
        assert_eq!(
            reopened.candidates_for_literal(b"abc").unwrap().len(),
            FILE_TRIGRAM_DELTA_MAX_PATHS + 1
        );
    }

    #[test]
    fn file_trigram_delta_rejects_legacy_base_even_when_empty() {
        let dir = tempdir().unwrap();
        let legacy = dir.path().join("legacy.bin");
        let data = FileTrigramIndexData {
            files: vec!["a.rs".to_string()],
            index: vec![(*b"abc", vec![0])],
        };
        std::fs::write(&legacy, bitcode::serialize(&data).unwrap()).unwrap();
        let empty = FileTrigramIndex::empty_delta_checkpoint().unwrap();
        assert!(FileTrigramIndex::load_binary_with_delta(&legacy, Some(&empty)).is_err());
    }

    #[test]
    fn file_trigram_legacy_bitcode_remains_readable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy_file_trigram.bin");
        let data = FileTrigramIndexData {
            files: vec!["legacy.rs".to_string()],
            index: FileTrigramIndex::prepare_trigrams(b"fn legacy_target() {}")
                .into_iter()
                .map(|trigram| (trigram, vec![0]))
                .collect(),
        };
        std::fs::write(&path, bitcode::serialize(&data).unwrap()).unwrap();

        let loaded = FileTrigramIndex::load_binary(&path).unwrap();
        assert!(loaded.mmap.is_none());
        assert_eq!(
            loaded.candidates_for_literal(b"legacy_target").unwrap(),
            vec!["legacy.rs"]
        );
    }

    #[test]
    fn file_trigram_legacy_bitcode_rejects_invalid_posting_ids() {
        let dir = tempdir().unwrap();
        for (case, postings) in [
            ("out_of_range", vec![2]),
            ("unsorted", vec![1, 0]),
            ("duplicate", vec![0, 0]),
        ] {
            let path = dir.path().join(format!("legacy_{case}.bin"));
            let data = FileTrigramIndexData {
                files: vec!["a.rs".to_string(), "b.rs".to_string()],
                index: vec![(*b"abc", postings)],
            };
            std::fs::write(&path, bitcode::serialize(&data).unwrap()).unwrap();
            assert!(
                FileTrigramIndex::load_binary(&path).is_err(),
                "legacy {case} postings must fail closed"
            );
        }
    }

    #[test]
    fn file_trigram_legacy_bitcode_rejects_duplicate_keys_and_empty_postings() {
        let dir = tempdir().unwrap();
        for (case, index) in [
            (
                "duplicate_key",
                vec![(*b"abc", vec![0]), (*b"abc", vec![0])],
            ),
            ("empty_posting", vec![(*b"abc", Vec::new())]),
        ] {
            let path = dir.path().join(format!("legacy_{case}.bin"));
            let data = FileTrigramIndexData {
                files: vec!["a.rs".to_string()],
                index,
            };
            std::fs::write(&path, bitcode::serialize(&data).unwrap()).unwrap();
            assert!(
                FileTrigramIndex::load_binary(&path).is_err(),
                "legacy {case} must fail closed"
            );
        }
    }

    #[test]
    fn file_trigram_mmap_rejects_truncated_posting_range() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        let mut index = FileTrigramIndex::new();
        index.add("a.rs", b"abc");
        index.save_binary_consuming(&path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let trigram_entry = FILE_TRIGRAM_MMAP_HEADER_SIZE + FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE;
        bytes[trigram_entry + 8..trigram_entry + 12].copy_from_slice(&2u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let error = FileTrigramIndex::load_binary(&path).err().unwrap();
        assert!(error.to_string().contains("posting range exceeds"));
    }

    fn file_trigram_test_layout(bytes: &[u8]) -> (usize, usize) {
        let read_u32 = |offset: usize| {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
        };
        let file_count = read_u32(8);
        let trigram_count = read_u32(12);
        let string_pool_bytes = read_u32(20);
        let trigram_index_offset =
            FILE_TRIGRAM_MMAP_HEADER_SIZE + file_count * FILE_TRIGRAM_MMAP_FILE_ENTRY_SIZE;
        let postings_offset = trigram_index_offset
            + trigram_count * FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE
            + string_pool_bytes;
        (trigram_index_offset, postings_offset)
    }

    #[test]
    fn file_trigram_mmap_save_to_new_file_survives_source_deletion() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let destination = dir.path().join("destination.bin");
        let mut original = FileTrigramIndex::new();
        original.add("src/a.rs", b"fn durable_target() {}");
        original.save_binary_consuming(&source).unwrap();

        let loaded = FileTrigramIndex::load_binary(&source).unwrap();
        loaded.save_binary(&destination).unwrap();
        assert_eq!(
            std::fs::read(&source).unwrap(),
            std::fs::read(&destination).unwrap()
        );

        drop(loaded);
        std::fs::remove_file(&source).unwrap();
        let copied = FileTrigramIndex::load_binary(&destination).unwrap();
        assert_eq!(
            copied.candidates_for_literal(b"durable_target").unwrap(),
            vec!["src/a.rs"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_trigram_paths_alias_detects_hard_links() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let alias = dir.path().join("alias.bin");
        let other = dir.path().join("other.bin");
        std::fs::write(&source, b"source").unwrap();
        std::fs::hard_link(&source, &alias).unwrap();
        std::fs::write(&other, b"source").unwrap();

        assert!(FileTrigramIndex::paths_alias(&source, &alias).unwrap());
        assert!(!FileTrigramIndex::paths_alias(&source, &other).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn file_trigram_windows_delta_checkpoint_rejects_source_aliases() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let alias = dir.path().join("alias.bin");
        let mut original = FileTrigramIndex::new();
        original.add("a.rs", b"durable_target");
        original.save_binary_consuming(&source).unwrap();
        std::fs::hard_link(&source, &alias).unwrap();

        let mut loaded = FileTrigramIndex::load_binary(&source).unwrap();
        loaded.add("b.rs", b"delta_target");
        assert!(loaded.save_binary(&source).is_err());
        assert!(loaded.save_binary(&alias).is_err());
        assert_eq!(
            FileTrigramIndex::load_binary(&source)
                .unwrap()
                .candidates_for_literal(b"durable_target")
                .unwrap(),
            vec!["a.rs"]
        );
    }

    #[test]
    fn file_trigram_mmap_detects_out_of_range_file_id_lazily() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        let checkpoint = dir.path().join("checkpoint.bin");
        let mut index = FileTrigramIndex::new();
        index.add("a.rs", b"target");
        index.save_binary_consuming(&path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let (_, postings_offset) = file_trigram_test_layout(&bytes);
        bytes[postings_offset..postings_offset + 4].copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| count.set(0));
        let mut loaded = FileTrigramIndex::load_binary(&path).unwrap();
        FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| assert_eq!(count.get(), 0));
        let error = loaded
            .visit_literal_candidates(b"target", |_| Ok(()))
            .err()
            .unwrap();
        assert!(error.to_string().contains("file ID 1 exceeds file count 1"));
        assert!(
            loaded.candidates_for_literal(b"target").is_none(),
            "the infallible wrapper must disable the prefilter so grep scans all files"
        );

        std::fs::write(&checkpoint, b"unchanged").unwrap();
        assert!(loaded.save_binary(&checkpoint).is_err());
        assert_eq!(std::fs::read(&checkpoint).unwrap(), b"unchanged");
        loaded.add("b.rs", b"checkpoint_delta");
        std::fs::write(&checkpoint, b"unchanged").unwrap();
        assert!(loaded.save_binary(&checkpoint).is_err());
        assert_eq!(std::fs::read(&checkpoint).unwrap(), b"unchanged");
    }

    #[test]
    fn file_trigram_mmap_detects_unsorted_or_duplicate_file_ids_lazily() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        let checkpoint = dir.path().join("checkpoint.bin");
        let mut index = FileTrigramIndex::new();
        index.add("a.rs", b"target");
        index.add("b.rs", b"target");
        index.save_binary_consuming(&path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let (_, postings_offset) = file_trigram_test_layout(&bytes);
        bytes[postings_offset..postings_offset + 4].copy_from_slice(&1u32.to_le_bytes());
        bytes[postings_offset + 4..postings_offset + 8].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| count.set(0));
        let mut loaded = FileTrigramIndex::load_binary(&path).unwrap();
        FILE_TRIGRAM_POSTING_VALUES_VALIDATED.with(|count| assert_eq!(count.get(), 0));
        let error = loaded
            .visit_literal_candidates(b"target", |_| Ok(()))
            .err()
            .unwrap();
        assert!(error.to_string().contains("not strictly increasing"));
        assert!(
            loaded.candidates_for_literal(b"target").is_none(),
            "the infallible wrapper must disable the prefilter so grep scans all files"
        );

        std::fs::write(&checkpoint, b"unchanged").unwrap();
        assert!(loaded.save_binary(&checkpoint).is_err());
        assert_eq!(std::fs::read(&checkpoint).unwrap(), b"unchanged");
        loaded.add("c.rs", b"checkpoint_delta");
        std::fs::write(&checkpoint, b"unchanged").unwrap();
        assert!(loaded.save_binary(&checkpoint).is_err());
        assert_eq!(std::fs::read(&checkpoint).unwrap(), b"unchanged");
    }

    #[test]
    fn file_trigram_mmap_rejects_misaligned_posting_range() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        let mut index = FileTrigramIndex::new();
        index.add("a.rs", b"target");
        index.save_binary_consuming(&path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let (trigram_index_offset, _) = file_trigram_test_layout(&bytes);
        bytes[trigram_index_offset + 4..trigram_index_offset + 8]
            .copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let error = FileTrigramIndex::load_binary(&path).err().unwrap();
        assert!(error.to_string().contains("not u32-aligned"));
    }

    #[test]
    fn file_trigram_mmap_rejects_overlapping_posting_ranges() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("file_trigram.bin");
        let mut index = FileTrigramIndex::new();
        index.add("a.rs", b"target");
        index.save_binary_consuming(&path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let (trigram_index_offset, _) = file_trigram_test_layout(&bytes);
        let second_entry = trigram_index_offset + FILE_TRIGRAM_MMAP_INDEX_ENTRY_SIZE;
        bytes[second_entry + 4..second_entry + 8].copy_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let error = FileTrigramIndex::load_binary(&path).err().unwrap();
        assert!(error.to_string().contains("canonical and non-overlapping"));
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
