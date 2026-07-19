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
use std::collections::{HashMap, HashSet};
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

        std::fs::write(path, buf)?;
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

        std::fs::write(path, buf)?;
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
        if literal.len() < 3 {
            return Ok(None);
        }

        let mut trigrams: Vec<[u8; 3]> = (0..literal.len() - 2)
            .map(|i| [literal[i], literal[i + 1], literal[i + 2]])
            .collect();
        trigrams.sort_unstable();
        trigrams.dedup();

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
                    result.extend(self.execute_plan(sub)?);
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
