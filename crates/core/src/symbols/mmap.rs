//! Memory-mapped symbol table — zero-deserialization loading from a flat binary.
//!
//! The `symbols_v2.bin` format stores all symbol data in a flat binary layout
//! that can be memory-mapped and queried directly without any deserialization
//! step. For the Linux kernel (935K symbols, 386MB in bitcode), this reduces
//! load time from ~1.5s to near-zero.
//!
//! ## Binary format
//!
//! ```text
//! [Header: 16 bytes]
//!   magic: u32 = 0x53594D42 ("SYMB")
//!   version: u32 = 2
//!   name_count: u32     // distinct symbol names
//!   symbol_count: u32   // total symbols
//!
//! [Name Index: name_count × 16 bytes, sorted by name_hash]
//!   name_hash: u64      // xxh3 of lowercase name
//!   name_offset: u32    // byte offset into String Pool
//!   symbols_offset: u32 // byte offset into Symbol Data
//!
//! [Prefix Index: name_count × 4 bytes, sorted by lowercase name]
//!   name_index_slot: u32 // slot in Name Index
//!
//! [Public File Index]
//!   file_count: u32
//!   file_count × (path_hash: u64, path_offset: u32,
//!                 lines_offset: u32, line_count: u32)
//!
//! [String Pool: variable]
//!   Sequence of length-prefixed UTF-8 strings: u32 len + bytes
//!
//! [Symbol Data: variable]
//!   Per name entry: u32 count + count × SymbolRecord
//! ```

use std::path::Path;

use memmap2::Mmap;

use crate::error::{CodixingError, Result};
use crate::language::{EntityKind, Language, TypeRelation, TypeRelationKind, Visibility};
use crate::symbols::Symbol;

/// Magic bytes: "SYMB" as little-endian u32.
pub const MAGIC: u32 = 0x53594D42;

/// Current format version.
/// Current mmap symbol format. Version 2 extends every symbol record with the
/// fields that were previously available only in `symbols.bin`.
pub const FORMAT_VERSION: u32 = 2;
const LEGACY_FORMAT_VERSION: u32 = 1;

/// Header size in bytes: magic(4) + version(4) + name_count(4) + symbol_count(4).
pub const HEADER_SIZE: usize = 16;

/// Size of one name-index entry: hash(8) + name_offset(4) + symbols_offset(4).
pub const NAME_INDEX_ENTRY_SIZE: usize = 16;
pub(crate) const PREFIX_INDEX_ENTRY_SIZE: usize = 4;
pub(crate) const FILE_INDEX_ENTRY_SIZE: usize = 20;

/// Memory-mapped symbol table that provides O(log N) lookup by name,
/// O(log N + matches) prefix lookup in v2, and O(N) general filter scans
/// without any eager deserialization.
pub struct MmapSymbolTable {
    mmap: Mmap,
    format_version: u32,
    name_count: u32,
    _symbol_count: u32,
    name_index_offset: usize,
    prefix_index_offset: Option<usize>,
    file_index_offset: Option<usize>,
    file_count: u32,
    string_pool_offset: usize,
    symbol_data_offset: usize,
    symbol_data_end: usize,
    public_line_data_offset: usize,
}

impl MmapSymbolTable {
    /// Load a memory-mapped symbol table from `symbols_v2.bin`.
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let file_len = usize::try_from(file.metadata()?.len()).map_err(|_| {
            CodixingError::Serialization(
                "symbols_v2.bin is too large for this platform".to_string(),
            )
        })?;

        if file_len < HEADER_SIZE {
            return Err(CodixingError::Serialization(
                "symbols_v2.bin too small for header".to_string(),
            ));
        }

        // SAFETY: We only create a read-only Mmap over a file we opened read-only.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|e| {
            CodixingError::Serialization(format!("failed to mmap symbols_v2.bin: {e}"))
        })?;

        // Parse header.
        let magic = read_u32(&mmap, 0);
        if magic != MAGIC {
            return Err(CodixingError::Serialization(format!(
                "invalid symbols_v2.bin magic: expected 0x{MAGIC:08X}, got 0x{magic:08X}"
            )));
        }

        let version = read_u32(&mmap, 4);
        if version != LEGACY_FORMAT_VERSION && version != FORMAT_VERSION {
            return Err(CodixingError::Serialization(format!(
                "unsupported symbols_v2.bin version: expected {LEGACY_FORMAT_VERSION} or {FORMAT_VERSION}, got {version}"
            )));
        }

        let name_count = read_u32(&mmap, 8);
        let symbol_count = read_u32(&mmap, 12);

        let name_index_offset = HEADER_SIZE;
        let name_index_size = (name_count as usize)
            .checked_mul(NAME_INDEX_ENTRY_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("symbols_v2.bin name index size overflow".to_string())
            })?;
        let name_index_end = name_index_offset
            .checked_add(name_index_size)
            .ok_or_else(|| {
                CodixingError::Serialization(
                    "symbols_v2.bin name index offset overflow".to_string(),
                )
            })?;
        let (
            prefix_index_offset,
            file_index_offset,
            file_count,
            string_pool_size_offset,
            actual_string_pool_offset,
            symbol_data_size,
        ) = if version >= FORMAT_VERSION {
            let prefix_index_size = (name_count as usize)
                .checked_mul(PREFIX_INDEX_ENTRY_SIZE)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "symbols_v2.bin prefix index size overflow".to_string(),
                    )
                })?;
            let prefix_end = name_index_end
                .checked_add(prefix_index_size)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "symbols_v2.bin prefix index offset overflow".to_string(),
                    )
                })?;
            let file_index_start = prefix_end.checked_add(4).ok_or_else(|| {
                CodixingError::Serialization(
                    "symbols_v2.bin file index offset overflow".to_string(),
                )
            })?;
            if file_index_start > file_len {
                return Err(CodixingError::Serialization(
                    "symbols_v2.bin truncated: missing file index count".to_string(),
                ));
            }
            let file_count = read_u32(&mmap, prefix_end);
            let file_index_size = (file_count as usize)
                .checked_mul(FILE_INDEX_ENTRY_SIZE)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "symbols_v2.bin file index size overflow".to_string(),
                    )
                })?;
            let sizes_offset = file_index_start
                .checked_add(file_index_size)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "symbols_v2.bin file index offset overflow".to_string(),
                    )
                })?;
            let pool_offset = sizes_offset.checked_add(8).ok_or_else(|| {
                CodixingError::Serialization(
                    "symbols_v2.bin string pool offset overflow".to_string(),
                )
            })?;
            if pool_offset > file_len {
                return Err(CodixingError::Serialization(
                    "symbols_v2.bin truncated: missing section sizes".to_string(),
                ));
            }
            (
                Some(name_index_end),
                Some(file_index_start),
                file_count,
                sizes_offset,
                pool_offset,
                Some(read_u32(&mmap, sizes_offset + 4) as usize),
            )
        } else {
            let pool_offset = name_index_end.checked_add(4).ok_or_else(|| {
                CodixingError::Serialization(
                    "symbols_v2.bin string pool offset overflow".to_string(),
                )
            })?;
            if pool_offset > file_len {
                return Err(CodixingError::Serialization(
                    "symbols_v2.bin truncated: missing string pool size".to_string(),
                ));
            }
            (None, None, 0, name_index_end, pool_offset, None)
        };

        let string_pool_size = read_u32(&mmap, string_pool_size_offset) as usize;
        let symbol_data_offset = actual_string_pool_offset
            .checked_add(string_pool_size)
            .filter(|offset| *offset <= file_len)
            .ok_or_else(|| {
                CodixingError::Serialization(
                    "symbols_v2.bin truncated or has an invalid string pool size".to_string(),
                )
            })?;
        let symbol_data_end = match symbol_data_size {
            Some(size) => symbol_data_offset
                .checked_add(size)
                .filter(|offset| *offset <= file_len)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "symbols_v2.bin truncated or has an invalid symbol data size".to_string(),
                    )
                })?,
            None => file_len,
        };

        Ok(Self {
            mmap,
            format_version: version,
            name_count,
            _symbol_count: symbol_count,
            name_index_offset,
            prefix_index_offset,
            file_index_offset,
            file_count,
            string_pool_offset: actual_string_pool_offset,
            symbol_data_offset,
            symbol_data_end,
            public_line_data_offset: symbol_data_end,
        })
    }

    /// Whether this mmap contains every field represented by [`Symbol`].
    ///
    /// Legacy v1 files remain readable, but callers that also have
    /// `symbols.bin` should prefer that fallback rather than silently losing
    /// documentation, visibility, and type-relation data.
    pub fn preserves_full_fidelity(&self) -> bool {
        self.format_version >= FORMAT_VERSION
    }

    /// Exact name lookup via binary search on the name hash index. O(log N).
    pub fn lookup(&self, name: &str) -> Vec<Symbol> {
        let hash = xxhash_rust::xxh3::xxh3_64(name.to_lowercase().as_bytes());
        self.lookup_by_hash(hash, name)
    }

    /// Prefix lookup -- find all symbols whose name starts with `prefix`.
    ///
    /// Version 2 uses a persisted sorted prefix index for O(log N + matches)
    /// discovery. Legacy version 1 files retain the O(N) scan fallback.
    pub fn lookup_prefix(&self, prefix: &str) -> Vec<Symbol> {
        let prefix_lower = prefix.to_lowercase();
        let Some(_) = self.prefix_index_offset else {
            return self.lookup_prefix_legacy(&prefix_lower);
        };

        let mut low = 0usize;
        let mut high = self.name_count as usize;
        while low < high {
            let mid = low + (high - low) / 2;
            let Some((_, name, _)) = self.prefix_entry(mid) else {
                return Vec::new();
            };
            if name.to_lowercase() < prefix_lower {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        let mut matches = Vec::new();
        for index in low..self.name_count as usize {
            let Some((name_slot, name, symbols_offset)) = self.prefix_entry(index) else {
                break;
            };
            if !name.to_lowercase().starts_with(&prefix_lower) {
                break;
            }
            matches.push((name_slot, name, symbols_offset));
        }

        // The legacy scan emitted hash-index order. Retaining that order keeps
        // the public API deterministic while the candidate discovery itself is
        // O(log N + matches).
        matches.sort_unstable_by_key(|(slot, _, _)| *slot);
        let mut results = Vec::new();
        for (_, name, symbols_offset) in matches {
            results.extend(self.read_symbols_at(symbols_offset, &name));
        }
        results
    }

    /// Return whether a public definition starts inside an exact file/range.
    /// Version 2 resolves this through a compact mmap secondary index without
    /// decoding or scanning symbol buckets.
    pub fn has_public_symbol_in_range(
        &self,
        file_path: &str,
        line_start: u64,
        line_end: u64,
    ) -> bool {
        if line_start >= line_end || self.file_index_offset.is_none() {
            return false;
        }

        let hash = xxhash_rust::xxh3::xxh3_64(file_path.as_bytes());
        let mut low = 0usize;
        let mut high = self.file_count as usize;
        while low < high {
            let mid = low + (high - low) / 2;
            let Some(entry_hash) = self.file_entry_hash(mid) else {
                return false;
            };
            if entry_hash < hash {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        for index in low..self.file_count as usize {
            let Some((entry_hash, path_offset, lines_offset, line_count)) = self.file_entry(index)
            else {
                return false;
            };
            if entry_hash != hash {
                break;
            }
            if self.read_string_pool(path_offset).as_deref() == Some(file_path)
                && self.public_lines_overlap(lines_offset, line_count, line_start, line_end)
            {
                return true;
            }
        }
        false
    }

    /// Filter symbols by name pattern and optional file path.
    ///
    /// Case-insensitive substring match on names; optionally filters by file.
    /// O(N) — scans all name entries.
    pub fn filter(&self, pattern: &str, file: Option<&str>) -> Vec<Symbol> {
        let pattern_lower = pattern.to_lowercase();
        let mut results = Vec::new();

        for i in 0..self.name_count as usize {
            let entry_offset = self.name_index_offset + i * NAME_INDEX_ENTRY_SIZE;
            let name_off = read_u32(&self.mmap, entry_offset + 8) as usize;
            let syms_off = read_u32(&self.mmap, entry_offset + 12) as usize;

            let Some(stored_name) = self.read_string_pool(name_off) else {
                continue;
            };
            if stored_name.to_lowercase().contains(&pattern_lower) {
                let syms = self.read_symbols_at(syms_off, &stored_name);
                if let Some(f) = file {
                    for sym in syms {
                        if sym.file_path.contains(f) {
                            results.push(sym);
                        }
                    }
                } else {
                    results.extend(syms);
                }
            }
        }

        results
    }

    /// Total number of unique symbol names.
    pub fn len(&self) -> usize {
        self.name_count as usize
    }

    /// Returns `true` if the table has no symbols.
    pub fn is_empty(&self) -> bool {
        self.name_count == 0
    }

    /// All symbols as a flat Vec.
    pub fn all_symbols(&self) -> Vec<Symbol> {
        let mut all = Vec::new();
        for i in 0..self.name_count as usize {
            let entry_offset = self.name_index_offset + i * NAME_INDEX_ENTRY_SIZE;
            let name_off = read_u32(&self.mmap, entry_offset + 8) as usize;
            let syms_off = read_u32(&self.mmap, entry_offset + 12) as usize;

            let Some(name) = self.read_string_pool(name_off) else {
                continue;
            };
            all.extend(self.read_symbols_at(syms_off, &name));
        }
        all
    }

    /// Visit symbols one name bucket at a time instead of retaining a complete
    /// decoded copy of the memory-mapped table.
    pub(crate) fn visit_symbols(&self, mut visitor: impl FnMut(&Symbol)) {
        for i in 0..self.name_count as usize {
            let entry_offset = self.name_index_offset + i * NAME_INDEX_ENTRY_SIZE;
            let name_off = read_u32(&self.mmap, entry_offset + 8) as usize;
            let syms_off = read_u32(&self.mmap, entry_offset + 12) as usize;

            let Some(name) = self.read_string_pool(name_off) else {
                continue;
            };
            for symbol in self.read_symbols_at(syms_off, &name) {
                visitor(&symbol);
            }
        }
    }

    // ── Private helpers ──────────────────────────────────────────────────

    /// Lookup by pre-computed hash. Handles hash collisions by checking the
    /// actual name in the string pool.
    fn lookup_by_hash(&self, hash: u64, name: &str) -> Vec<Symbol> {
        let name_lower = name.to_lowercase();

        // Binary search for the hash in the sorted name index.
        let count = self.name_count as usize;
        let mut lo = 0usize;
        let mut hi = count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry_hash = self.read_name_hash(mid);
            if entry_hash < hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }

        // There may be multiple entries with the same hash (collisions).
        // Scan forward from `lo` while the hash matches.
        let mut results = Vec::new();
        let mut idx = lo;
        while idx < count {
            let entry_hash = self.read_name_hash(idx);
            if entry_hash != hash {
                break;
            }

            let entry_offset = self.name_index_offset + idx * NAME_INDEX_ENTRY_SIZE;
            let name_off = read_u32(&self.mmap, entry_offset + 8) as usize;
            let syms_off = read_u32(&self.mmap, entry_offset + 12) as usize;

            let Some(stored_name) = self.read_string_pool(name_off) else {
                idx += 1;
                continue;
            };
            if stored_name.to_lowercase() == name_lower {
                results.extend(self.read_symbols_at(syms_off, &stored_name));
            }

            idx += 1;
        }

        results
    }

    /// Read the name hash at the given index in the name index.
    fn read_name_hash(&self, idx: usize) -> u64 {
        let offset = self.name_index_offset + idx * NAME_INDEX_ENTRY_SIZE;
        read_u64(&self.mmap, offset)
    }

    fn lookup_prefix_legacy(&self, prefix_lower: &str) -> Vec<Symbol> {
        let mut results = Vec::new();
        for index in 0..self.name_count as usize {
            let entry_offset = self.name_index_offset + index * NAME_INDEX_ENTRY_SIZE;
            let name_offset = read_u32(&self.mmap, entry_offset + 8) as usize;
            let symbols_offset = read_u32(&self.mmap, entry_offset + 12) as usize;
            let Some(name) = self.read_string_pool(name_offset) else {
                continue;
            };
            if name.to_lowercase().starts_with(prefix_lower) {
                results.extend(self.read_symbols_at(symbols_offset, &name));
            }
        }
        results
    }

    fn prefix_entry(&self, index: usize) -> Option<(usize, String, usize)> {
        let offset = self
            .prefix_index_offset?
            .checked_add(index.checked_mul(PREFIX_INDEX_ENTRY_SIZE)?)?;
        let name_slot = read_u32_checked(&self.mmap, offset)? as usize;
        if name_slot >= self.name_count as usize {
            return None;
        }
        let name_entry_offset = self
            .name_index_offset
            .checked_add(name_slot.checked_mul(NAME_INDEX_ENTRY_SIZE)?)?;
        let name_offset = read_u32_checked(&self.mmap, name_entry_offset + 8)? as usize;
        let symbols_offset = read_u32_checked(&self.mmap, name_entry_offset + 12)? as usize;
        Some((
            name_slot,
            self.read_string_pool(name_offset)?,
            symbols_offset,
        ))
    }

    fn file_entry_hash(&self, index: usize) -> Option<u64> {
        let offset = self
            .file_index_offset?
            .checked_add(index.checked_mul(FILE_INDEX_ENTRY_SIZE)?)?;
        read_u64_checked(&self.mmap, offset)
    }

    fn file_entry(&self, index: usize) -> Option<(u64, usize, usize, usize)> {
        let offset = self
            .file_index_offset?
            .checked_add(index.checked_mul(FILE_INDEX_ENTRY_SIZE)?)?;
        Some((
            read_u64_checked(&self.mmap, offset)?,
            read_u32_checked(&self.mmap, offset + 8)? as usize,
            read_u32_checked(&self.mmap, offset + 12)? as usize,
            read_u32_checked(&self.mmap, offset + 16)? as usize,
        ))
    }

    fn public_lines_overlap(
        &self,
        lines_offset: usize,
        line_count: usize,
        line_start: u64,
        line_end: u64,
    ) -> bool {
        let Some(start) = self.public_line_data_offset.checked_add(lines_offset) else {
            return false;
        };
        let Some(end) = line_count
            .checked_mul(4)
            .and_then(|size| start.checked_add(size))
            .filter(|end| *end <= self.mmap.len())
        else {
            return false;
        };
        let mut low = 0usize;
        let mut high = line_count;
        while low < high {
            let mid = low + (high - low) / 2;
            let offset = start + mid * 4;
            let Some(line) = read_u32_checked(&self.mmap[..end], offset) else {
                return false;
            };
            if (line as u64) < line_start {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low < line_count
            && read_u32_checked(&self.mmap[..end], start + low * 4)
                .is_some_and(|line| (line as u64) < line_end)
    }

    /// Read a length-prefixed string from the string pool at the given
    /// relative offset (relative to string_pool_offset).
    fn read_string_pool(&self, rel_offset: usize) -> Option<String> {
        let abs = self.string_pool_offset.checked_add(rel_offset)?;
        let (length_size, len) = if self.format_version >= FORMAT_VERSION {
            (4, read_u32_checked(&self.mmap, abs)? as usize)
        } else {
            (2, read_u16_checked(&self.mmap, abs)? as usize)
        };
        let start = abs.checked_add(length_size)?;
        let end = start.checked_add(len)?;
        if end > self.symbol_data_offset {
            return None;
        }
        Some(String::from_utf8_lossy(&self.mmap[start..end]).into_owned())
    }

    /// Read all symbols for a given name entry from the symbol data section.
    /// `rel_offset` is relative to `symbol_data_offset`.
    fn read_symbols_at(&self, rel_offset: usize, name: &str) -> Vec<Symbol> {
        self.try_read_symbols_at(rel_offset, name)
            .unwrap_or_default()
    }

    fn try_read_symbols_at(&self, rel_offset: usize, name: &str) -> Option<Vec<Symbol>> {
        let abs = self.symbol_data_offset.checked_add(rel_offset)?;
        let (count, count_size) = if self.format_version >= FORMAT_VERSION {
            (read_u32_checked(&self.mmap, abs)? as usize, 4)
        } else {
            (read_u16_checked(&self.mmap, abs)? as usize, 2)
        };
        let mut pos = abs.checked_add(count_size)?;
        let min_record_size = if self.format_version >= FORMAT_VERSION {
            34
        } else {
            27
        };
        if count > self.symbol_data_end.saturating_sub(pos) / min_record_size {
            return None;
        }
        let mut symbols = Vec::with_capacity(count);

        for _ in 0..count {
            let fixed_end = pos.checked_add(27)?;
            if fixed_end > self.symbol_data_end {
                return None;
            }
            let kind_u8 = self.mmap[pos];
            let lang_u8 = self.mmap[pos + 1];
            let file_path_off = read_u32(&self.mmap, pos + 2) as usize;
            let line_start = read_u32(&self.mmap, pos + 6) as usize;
            let line_end = read_u32(&self.mmap, pos + 10) as usize;
            let byte_start = read_u32(&self.mmap, pos + 14) as usize;
            let byte_end = read_u32(&self.mmap, pos + 18) as usize;
            let sig_off = read_u32(&self.mmap, pos + 22) as usize;
            let scope_count = self.mmap[pos + 26] as usize;
            pos = fixed_end;

            let mut scope = Vec::with_capacity(scope_count);
            for _ in 0..scope_count {
                if pos.checked_add(4)? > self.symbol_data_end {
                    return None;
                }
                let soff = read_u32(&self.mmap, pos) as usize;
                scope.push(self.read_string_pool(soff)?);
                pos += 4;
            }

            let (doc_comment, visibility, type_relations) = if self.format_version >= FORMAT_VERSION
            {
                if pos.checked_add(7)? > self.symbol_data_end {
                    return None;
                }
                let doc_comment_off = read_u32(&self.mmap, pos) as usize;
                let visibility = u8_to_visibility(self.mmap[pos + 4]);
                let relation_count = read_u16(&self.mmap, pos + 5) as usize;
                pos += 7;

                if relation_count > self.symbol_data_end.saturating_sub(pos) / 5 {
                    return None;
                }

                let mut type_relations = Vec::with_capacity(relation_count);
                for _ in 0..relation_count {
                    let kind = u8_to_type_relation_kind(self.mmap[pos]);
                    let target_off = read_u32(&self.mmap, pos + 1) as usize;
                    pos += 5;
                    type_relations.push(TypeRelation {
                        kind,
                        target: self.read_string_pool(target_off)?,
                    });
                }

                let doc_comment = if doc_comment_off == 0 {
                    None
                } else {
                    Some(self.read_string_pool(doc_comment_off)?)
                };
                (doc_comment, visibility, type_relations)
            } else {
                (None, Visibility::default(), Vec::new())
            };

            let file_path = self.read_string_pool(file_path_off)?;
            let signature = if sig_off == 0 {
                None
            } else {
                Some(self.read_string_pool(sig_off)?)
            };

            symbols.push(Symbol {
                name: name.to_string(),
                kind: u8_to_entity_kind(kind_u8),
                language: u8_to_language(lang_u8),
                file_path,
                line_start,
                line_end,
                byte_start,
                byte_end,
                signature,
                scope,
                doc_comment,
                visibility,
                type_relations,
            });
        }

        Some(symbols)
    }
}

pub(crate) fn visibility_to_u8(visibility: &Visibility) -> u8 {
    match visibility {
        Visibility::Public => 1,
        Visibility::CrateInternal => 2,
        Visibility::Private => 3,
    }
}

fn u8_to_visibility(value: u8) -> Visibility {
    match value {
        1 => Visibility::Public,
        2 => Visibility::CrateInternal,
        _ => Visibility::Private,
    }
}

pub(crate) fn type_relation_kind_to_u8(kind: &TypeRelationKind) -> u8 {
    match kind {
        TypeRelationKind::Implements => 1,
        TypeRelationKind::Extends => 2,
        TypeRelationKind::Returns => 3,
        TypeRelationKind::Contains => 4,
    }
}

fn u8_to_type_relation_kind(value: u8) -> TypeRelationKind {
    match value {
        1 => TypeRelationKind::Implements,
        2 => TypeRelationKind::Extends,
        3 => TypeRelationKind::Returns,
        _ => TypeRelationKind::Contains,
    }
}

// ── Conversion helpers ───────────────────────────────────────────────────

/// Map `EntityKind` to a u8 discriminant.
pub fn entity_kind_to_u8(kind: &EntityKind) -> u8 {
    match kind {
        EntityKind::Function => 0,
        EntityKind::Method => 1,
        EntityKind::Class => 2,
        EntityKind::Struct => 3,
        EntityKind::Enum => 4,
        EntityKind::Interface => 5,
        EntityKind::Trait => 6,
        EntityKind::TypeAlias => 7,
        EntityKind::Constant => 8,
        EntityKind::Static => 9,
        EntityKind::Module => 10,
        EntityKind::Import => 11,
        EntityKind::Impl => 12,
        EntityKind::Namespace => 13,
        EntityKind::Variable => 14,
        EntityKind::Type => 15,
    }
}

/// Map a u8 discriminant back to `EntityKind`.
pub fn u8_to_entity_kind(v: u8) -> EntityKind {
    match v {
        0 => EntityKind::Function,
        1 => EntityKind::Method,
        2 => EntityKind::Class,
        3 => EntityKind::Struct,
        4 => EntityKind::Enum,
        5 => EntityKind::Interface,
        6 => EntityKind::Trait,
        7 => EntityKind::TypeAlias,
        8 => EntityKind::Constant,
        9 => EntityKind::Static,
        10 => EntityKind::Module,
        11 => EntityKind::Import,
        12 => EntityKind::Impl,
        13 => EntityKind::Namespace,
        14 => EntityKind::Variable,
        15 => EntityKind::Type,
        _ => EntityKind::Function, // fallback
    }
}

/// Map `Language` to a u8 discriminant.
pub fn language_to_u8(lang: Language) -> u8 {
    match lang {
        Language::Rust => 0,
        Language::Python => 1,
        Language::TypeScript => 2,
        Language::Tsx => 3,
        Language::JavaScript => 4,
        Language::Go => 5,
        Language::Java => 6,
        Language::C => 7,
        Language::Cpp => 8,
        Language::CSharp => 9,
        Language::Ruby => 10,
        Language::Swift => 11,
        Language::Kotlin => 12,
        Language::Scala => 13,
        Language::Zig => 14,
        Language::Php => 15,
        Language::Bash => 16,
        Language::Matlab => 17,
        Language::Yaml => 18,
        Language::Toml => 19,
        Language::Dockerfile => 20,
        Language::Makefile => 21,
        Language::Mermaid => 22,
        Language::Xml => 23,
        Language::Markdown => 24,
        Language::Html => 25,
        Language::Assembly => 26,
        Language::Rst => 27,
        Language::AsciiDoc => 28,
        Language::PlainText => 29,
        Language::Jupyter => 30,
        Language::OpenApi => 31,
        Language::Pdf => 32,
    }
}

/// Map a u8 discriminant back to `Language`.
pub fn u8_to_language(v: u8) -> Language {
    match v {
        0 => Language::Rust,
        1 => Language::Python,
        2 => Language::TypeScript,
        3 => Language::Tsx,
        4 => Language::JavaScript,
        5 => Language::Go,
        6 => Language::Java,
        7 => Language::C,
        8 => Language::Cpp,
        9 => Language::CSharp,
        10 => Language::Ruby,
        11 => Language::Swift,
        12 => Language::Kotlin,
        13 => Language::Scala,
        14 => Language::Zig,
        15 => Language::Php,
        16 => Language::Bash,
        17 => Language::Matlab,
        18 => Language::Yaml,
        19 => Language::Toml,
        20 => Language::Dockerfile,
        21 => Language::Makefile,
        22 => Language::Mermaid,
        23 => Language::Xml,
        24 => Language::Markdown,
        25 => Language::Html,
        26 => Language::Assembly,
        27 => Language::Rst,
        28 => Language::AsciiDoc,
        29 => Language::PlainText,
        30 => Language::Jupyter,
        31 => Language::OpenApi,
        32 => Language::Pdf,
        _ => Language::Rust, // fallback
    }
}

// ── Low-level read helpers ───────────────────────────────────────────────

#[inline]
fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap())
}

fn read_u16_checked(buf: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    Some(u16::from_le_bytes(buf.get(offset..end)?.try_into().ok()?))
}

fn read_u32_checked(buf: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    Some(u32::from_le_bytes(buf.get(offset..end)?.try_into().ok()?))
}

fn read_u64_checked(buf: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    Some(u64::from_le_bytes(buf.get(offset..end)?.try_into().ok()?))
}

#[inline]
fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

#[inline]
fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}
