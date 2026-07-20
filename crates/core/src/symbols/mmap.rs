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
//!   version: u32 = 4
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
//! [Exact File Symbol Index] (v3+)
//!   file_count: u32
//!   file_count × (path_hash: u64, path_offset: u32,
//!                 postings_offset: u32, postings_count: u32)
//!   posting_count: u32
//!   posting_count × (name_index_slot: u32, symbol_record_offset: u32)
//!
//! [Section Sizes]
//!   string_pool_size: u32
//!   symbol_data_size: u32
//!
//! [String Pool: variable]
//!   Sequence of length-prefixed UTF-8 strings: u32 len + bytes
//!
//! [Symbol Data: variable]
//!   Per name entry: u32 count + count × SymbolRecord
//! ```
//!
//! Versions 2 and 3 additionally stored a public-file index before the exact
//! index (v3) or section sizes (v2), plus its line array after symbol data.

use std::path::Path;

use memmap2::Mmap;

use crate::error::{CodixingError, Result};
use crate::language::{EntityKind, Language, TypeRelation, TypeRelationKind, Visibility};
use crate::symbols::Symbol;

/// Magic bytes: "SYMB" as little-endian u32.
pub const MAGIC: u32 = 0x53594D42;

/// Current mmap symbol format. Version 2 extended every symbol record with the
/// fields that were previously available only in `symbols.bin`; version 3 added
/// exact-file postings, and version 4 removed the redundant public-file index.
pub const FORMAT_VERSION: u32 = 4;
const FULL_FIDELITY_FORMAT_VERSION: u32 = 2;
const EXACT_FILE_FORMAT_VERSION: u32 = 3;
const LAST_PUBLIC_FILE_FORMAT_VERSION: u32 = 3;
const LEGACY_FORMAT_VERSION: u32 = 1;

/// Header size in bytes: magic(4) + version(4) + name_count(4) + symbol_count(4).
pub const HEADER_SIZE: usize = 16;

/// Size of one name-index entry: hash(8) + name_offset(4) + symbols_offset(4).
pub const NAME_INDEX_ENTRY_SIZE: usize = 16;
pub(crate) const PREFIX_INDEX_ENTRY_SIZE: usize = 4;
pub(crate) const FILE_INDEX_ENTRY_SIZE: usize = 20;
pub(crate) const SYMBOL_FILE_INDEX_ENTRY_SIZE: usize = 20;
pub(crate) const SYMBOL_FILE_POSTING_SIZE: usize = 8;

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
    symbol_file_index_offset: Option<usize>,
    symbol_file_count: u32,
    symbol_file_postings_offset: Option<usize>,
    symbol_file_posting_count: u32,
    string_pool_offset: usize,
    symbol_data_offset: usize,
    symbol_data_end: usize,
    public_line_data_offset: usize,
}

struct StringPoolLayout {
    starts: Vec<u8>,
    len: usize,
}

impl StringPoolLayout {
    fn contains(&self, offset: usize) -> bool {
        offset < self.len
            && self
                .starts
                .get(offset / 8)
                .is_some_and(|byte| byte & (1 << (offset % 8)) != 0)
    }
}

#[derive(Clone, Copy)]
struct ValidatedRecord {
    relative_offset: u32,
    name_slot: u32,
    file_path_offset: u32,
}

fn invalid_structure(message: impl Into<String>) -> CodixingError {
    CodixingError::Serialization(format!("invalid symbols_v2.bin: {}", message.into()))
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
        if !(LEGACY_FORMAT_VERSION..=FORMAT_VERSION).contains(&version) {
            return Err(CodixingError::Serialization(format!(
                "unsupported symbols_v2.bin version: expected {LEGACY_FORMAT_VERSION}..={FORMAT_VERSION}, got {version}"
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
            symbol_file_index_offset,
            symbol_file_count,
            symbol_file_postings_offset,
            symbol_file_posting_count,
            string_pool_size_offset,
            actual_string_pool_offset,
            symbol_data_size,
        ) = if version >= FULL_FIDELITY_FORMAT_VERSION {
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
            let (file_index_offset, file_count, section_offset) =
                if version <= LAST_PUBLIC_FILE_FORMAT_VERSION {
                    let file_count_end = prefix_end.checked_add(4).ok_or_else(|| {
                        CodixingError::Serialization(
                            "symbols_v2.bin file index offset overflow".to_string(),
                        )
                    })?;
                    if file_count_end > file_len {
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
                    let file_index_end =
                        file_count_end.checked_add(file_index_size).ok_or_else(|| {
                            CodixingError::Serialization(
                                "symbols_v2.bin file index offset overflow".to_string(),
                            )
                        })?;
                    (Some(file_count_end), file_count, file_index_end)
                } else {
                    (None, 0, prefix_end)
                };
            let (
                symbol_file_index_offset,
                symbol_file_count,
                symbol_file_postings_offset,
                symbol_file_posting_count,
                sizes_offset,
            ) = if version >= EXACT_FILE_FORMAT_VERSION {
                let symbol_file_count_end = section_offset.checked_add(4).ok_or_else(|| {
                    CodixingError::Serialization(
                        "symbols_v2.bin exact-file count offset overflow".to_string(),
                    )
                })?;
                if symbol_file_count_end > file_len {
                    return Err(CodixingError::Serialization(
                        "symbols_v2.bin truncated: missing exact-file count".to_string(),
                    ));
                }
                let symbol_file_count = read_u32(&mmap, section_offset);
                let symbol_file_index_offset = symbol_file_count_end;
                let symbol_file_index_size = (symbol_file_count as usize)
                    .checked_mul(SYMBOL_FILE_INDEX_ENTRY_SIZE)
                    .ok_or_else(|| {
                        CodixingError::Serialization(
                            "symbols_v2.bin exact-file index size overflow".to_string(),
                        )
                    })?;
                let posting_count_offset = symbol_file_index_offset
                    .checked_add(symbol_file_index_size)
                    .ok_or_else(|| {
                        CodixingError::Serialization(
                            "symbols_v2.bin exact-file index offset overflow".to_string(),
                        )
                    })?;
                let posting_count_end = posting_count_offset.checked_add(4).ok_or_else(|| {
                    CodixingError::Serialization(
                        "symbols_v2.bin exact-file posting count overflow".to_string(),
                    )
                })?;
                if posting_count_end > file_len {
                    return Err(CodixingError::Serialization(
                        "symbols_v2.bin truncated: missing exact-file postings".to_string(),
                    ));
                }
                let symbol_file_posting_count = read_u32(&mmap, posting_count_offset);
                let symbol_file_postings_offset = posting_count_end;
                let posting_size = (symbol_file_posting_count as usize)
                    .checked_mul(SYMBOL_FILE_POSTING_SIZE)
                    .ok_or_else(|| {
                        CodixingError::Serialization(
                            "symbols_v2.bin exact-file postings size overflow".to_string(),
                        )
                    })?;
                let sizes_offset = symbol_file_postings_offset
                    .checked_add(posting_size)
                    .ok_or_else(|| {
                        CodixingError::Serialization(
                            "symbols_v2.bin exact-file postings offset overflow".to_string(),
                        )
                    })?;
                (
                    Some(symbol_file_index_offset),
                    symbol_file_count,
                    Some(symbol_file_postings_offset),
                    symbol_file_posting_count,
                    sizes_offset,
                )
            } else {
                (None, 0, None, 0, section_offset)
            };
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
                file_index_offset,
                file_count,
                symbol_file_index_offset,
                symbol_file_count,
                symbol_file_postings_offset,
                symbol_file_posting_count,
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
            (
                None,
                None,
                0,
                None,
                0,
                None,
                0,
                name_index_end,
                pool_offset,
                None,
            )
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
        let minimum_record_size = if version >= FULL_FIDELITY_FORMAT_VERSION {
            34
        } else {
            27
        };
        let maximum_symbol_count = (symbol_data_end - symbol_data_offset) / minimum_record_size;
        if symbol_count as usize > maximum_symbol_count {
            return Err(invalid_structure(format!(
                "header symbol count {symbol_count} exceeds the symbol-data capacity {maximum_symbol_count}"
            )));
        }
        if version >= EXACT_FILE_FORMAT_VERSION && symbol_count != symbol_file_posting_count {
            return Err(invalid_structure(format!(
                "header symbol count {symbol_count} does not match exact-file posting count {symbol_file_posting_count}"
            )));
        }
        if version >= FORMAT_VERSION && symbol_data_end != file_len {
            return Err(invalid_structure(
                "version 4 symbol data does not end at the file boundary",
            ));
        }

        let table = Self {
            mmap,
            format_version: version,
            name_count,
            _symbol_count: symbol_count,
            name_index_offset,
            prefix_index_offset,
            file_index_offset,
            file_count,
            symbol_file_index_offset,
            symbol_file_count,
            symbol_file_postings_offset,
            symbol_file_posting_count,
            string_pool_offset: actual_string_pool_offset,
            symbol_data_offset,
            symbol_data_end,
            public_line_data_offset: symbol_data_end,
        };
        table.validate_structure()?;
        Ok(table)
    }

    fn validate_string_pool(&self) -> Result<StringPoolLayout> {
        let pool_len = self
            .symbol_data_offset
            .checked_sub(self.string_pool_offset)
            .ok_or_else(|| invalid_structure("string pool offsets are reversed"))?;
        let mut starts = vec![0u8; pool_len.div_ceil(8)];
        let length_size = if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
            4
        } else {
            2
        };
        let mut relative = 0usize;
        while relative < pool_len {
            starts[relative / 8] |= 1 << (relative % 8);
            let absolute = self
                .string_pool_offset
                .checked_add(relative)
                .ok_or_else(|| invalid_structure("string pool offset overflow"))?;
            let length = if length_size == 4 {
                read_u32_checked(&self.mmap, absolute)
                    .map(|value| value as usize)
                    .ok_or_else(|| invalid_structure("truncated string length"))?
            } else {
                read_u16_checked(&self.mmap, absolute)
                    .map(|value| value as usize)
                    .ok_or_else(|| invalid_structure("truncated legacy string length"))?
            };
            let string_start = relative
                .checked_add(length_size)
                .ok_or_else(|| invalid_structure("string start overflow"))?;
            let next = string_start
                .checked_add(length)
                .filter(|next| *next <= pool_len)
                .ok_or_else(|| invalid_structure("string extends beyond the string pool"))?;
            std::str::from_utf8(
                &self.mmap[self.string_pool_offset + string_start..self.string_pool_offset + next],
            )
            .map_err(|_| invalid_structure("string pool contains invalid UTF-8"))?;
            relative = next;
        }
        if relative != pool_len {
            return Err(invalid_structure("string pool is not contiguous"));
        }
        Ok(StringPoolLayout {
            starts,
            len: pool_len,
        })
    }

    fn validated_string<'a>(
        &'a self,
        pool: &StringPoolLayout,
        relative: usize,
        context: &str,
    ) -> Result<&'a str> {
        if !pool.contains(relative) {
            return Err(invalid_structure(format!(
                "{context} does not reference a string boundary"
            )));
        }
        let absolute = self
            .string_pool_offset
            .checked_add(relative)
            .ok_or_else(|| invalid_structure(format!("{context} string offset overflow")))?;
        let (length_size, length) = if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
            (
                4,
                read_u32_checked(&self.mmap, absolute)
                    .map(|value| value as usize)
                    .ok_or_else(|| invalid_structure(format!("{context} string is truncated")))?,
            )
        } else {
            (
                2,
                read_u16_checked(&self.mmap, absolute)
                    .map(|value| value as usize)
                    .ok_or_else(|| invalid_structure(format!("{context} string is truncated")))?,
            )
        };
        let start = absolute
            .checked_add(length_size)
            .ok_or_else(|| invalid_structure(format!("{context} string start overflow")))?;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= self.symbol_data_offset)
            .ok_or_else(|| invalid_structure(format!("{context} string is out of bounds")))?;
        std::str::from_utf8(&self.mmap[start..end])
            .map_err(|_| invalid_structure(format!("{context} string is invalid UTF-8")))
    }

    fn validate_symbol_record(&self, pos: &mut usize, pool: &StringPoolLayout) -> Result<u32> {
        let fixed_end = (*pos)
            .checked_add(27)
            .filter(|end| *end <= self.symbol_data_end)
            .ok_or_else(|| invalid_structure("symbol record fixed fields are truncated"))?;
        let kind = self.mmap[*pos];
        let language = self.mmap[*pos + 1];
        if kind > 15 {
            return Err(invalid_structure(format!(
                "symbol record has unknown entity kind {kind}"
            )));
        }
        if language > 32 {
            return Err(invalid_structure(format!(
                "symbol record has unknown language {language}"
            )));
        }

        let file_path_offset = read_u32_checked(&self.mmap, *pos + 2)
            .ok_or_else(|| invalid_structure("symbol file path offset is truncated"))?;
        self.validated_string(pool, file_path_offset as usize, "symbol file path")?;
        let line_start = read_u32_checked(&self.mmap, *pos + 6)
            .ok_or_else(|| invalid_structure("symbol line start is truncated"))?;
        let line_end = read_u32_checked(&self.mmap, *pos + 10)
            .ok_or_else(|| invalid_structure("symbol line end is truncated"))?;
        let byte_start = read_u32_checked(&self.mmap, *pos + 14)
            .ok_or_else(|| invalid_structure("symbol byte start is truncated"))?;
        let byte_end = read_u32_checked(&self.mmap, *pos + 18)
            .ok_or_else(|| invalid_structure("symbol byte end is truncated"))?;
        if line_start > line_end || byte_start > byte_end {
            return Err(invalid_structure("symbol source range is reversed"));
        }
        let signature_offset = read_u32_checked(&self.mmap, *pos + 22)
            .ok_or_else(|| invalid_structure("symbol signature offset is truncated"))?;
        if signature_offset != 0 {
            self.validated_string(pool, signature_offset as usize, "symbol signature")?;
        }
        let scope_count = self.mmap[*pos + 26] as usize;
        *pos = fixed_end;

        let scope_bytes = scope_count
            .checked_mul(4)
            .and_then(|size| (*pos).checked_add(size))
            .filter(|end| *end <= self.symbol_data_end)
            .ok_or_else(|| invalid_structure("symbol scope offsets are truncated"))?;
        while *pos < scope_bytes {
            let offset = read_u32_checked(&self.mmap, *pos)
                .ok_or_else(|| invalid_structure("symbol scope offset is truncated"))?;
            self.validated_string(pool, offset as usize, "symbol scope")?;
            *pos += 4;
        }

        if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
            let extension_end = (*pos)
                .checked_add(7)
                .filter(|end| *end <= self.symbol_data_end)
                .ok_or_else(|| invalid_structure("full-fidelity symbol fields are truncated"))?;
            let doc_comment_offset = read_u32_checked(&self.mmap, *pos)
                .ok_or_else(|| invalid_structure("symbol documentation offset is truncated"))?;
            if doc_comment_offset != 0 {
                self.validated_string(pool, doc_comment_offset as usize, "symbol documentation")?;
            }
            let visibility = self.mmap[*pos + 4];
            if !(1..=3).contains(&visibility) {
                return Err(invalid_structure(format!(
                    "symbol record has unknown visibility {visibility}"
                )));
            }
            let relation_count = read_u16_checked(&self.mmap, *pos + 5)
                .ok_or_else(|| invalid_structure("symbol relation count is truncated"))?
                as usize;
            *pos = extension_end;
            let relations_end = relation_count
                .checked_mul(5)
                .and_then(|size| (*pos).checked_add(size))
                .filter(|end| *end <= self.symbol_data_end)
                .ok_or_else(|| invalid_structure("symbol type relations are truncated"))?;
            while *pos < relations_end {
                let kind = self.mmap[*pos];
                if !(1..=4).contains(&kind) {
                    return Err(invalid_structure(format!(
                        "symbol record has unknown relation kind {kind}"
                    )));
                }
                let target_offset = read_u32_checked(&self.mmap, *pos + 1)
                    .ok_or_else(|| invalid_structure("symbol relation target is truncated"))?;
                self.validated_string(pool, target_offset as usize, "symbol relation target")?;
                *pos += 5;
            }
        }
        Ok(file_path_offset)
    }

    fn validated_name_for_slot<'a>(
        &'a self,
        pool: &StringPoolLayout,
        slot: usize,
    ) -> Result<&'a str> {
        if slot >= self.name_count as usize {
            return Err(invalid_structure(format!(
                "name slot {slot} is out of bounds"
            )));
        }
        let entry = self
            .name_index_offset
            .checked_add(
                slot.checked_mul(NAME_INDEX_ENTRY_SIZE)
                    .ok_or_else(|| invalid_structure("name slot offset overflow"))?,
            )
            .ok_or_else(|| invalid_structure("name slot offset overflow"))?;
        let name_offset = read_u32_checked(&self.mmap, entry + 8)
            .ok_or_else(|| invalid_structure("name offset is truncated"))?;
        self.validated_string(pool, name_offset as usize, "symbol name")
    }

    fn validate_names_and_records(&self, pool: &StringPoolLayout) -> Result<Vec<ValidatedRecord>> {
        let mut previous: Option<(u64, String, String)> = None;
        let mut expected_symbol_offset = 0usize;
        let mut total_symbols = 0u64;
        let mut records = Vec::new();
        if self.symbol_file_index_offset.is_some() {
            records
                .try_reserve_exact(self.symbol_file_posting_count as usize)
                .map_err(|_| invalid_structure("exact-file validation records are too large"))?;
        }

        for name_slot in 0..self.name_count as usize {
            let entry = self
                .name_index_offset
                .checked_add(
                    name_slot
                        .checked_mul(NAME_INDEX_ENTRY_SIZE)
                        .ok_or_else(|| invalid_structure("name index offset overflow"))?,
                )
                .ok_or_else(|| invalid_structure("name index offset overflow"))?;
            let hash = read_u64_checked(&self.mmap, entry)
                .ok_or_else(|| invalid_structure("name hash is truncated"))?;
            let name_offset = read_u32_checked(&self.mmap, entry + 8)
                .ok_or_else(|| invalid_structure("name string offset is truncated"))?;
            let symbols_offset = read_u32_checked(&self.mmap, entry + 12)
                .ok_or_else(|| invalid_structure("name symbol offset is truncated"))?
                as usize;
            let name = self.validated_string(pool, name_offset as usize, "symbol name")?;
            let lower_name = name.to_lowercase();
            let expected_hash = xxhash_rust::xxh3::xxh3_64(lower_name.as_bytes());
            if hash != expected_hash {
                return Err(invalid_structure(format!(
                    "name hash does not match {name:?}"
                )));
            }
            if let Some((previous_hash, previous_lower, previous_name)) = &previous {
                let ordering = hash
                    .cmp(previous_hash)
                    .then_with(|| lower_name.cmp(previous_lower))
                    .then_with(|| name.cmp(previous_name));
                if ordering != std::cmp::Ordering::Greater {
                    return Err(invalid_structure("name index is not canonically sorted"));
                }
            }
            previous = Some((hash, lower_name, name.to_string()));

            if symbols_offset != expected_symbol_offset {
                return Err(invalid_structure(format!(
                    "name {name:?} has non-contiguous symbol data offset {symbols_offset}"
                )));
            }
            let absolute = self
                .symbol_data_offset
                .checked_add(symbols_offset)
                .filter(|offset| *offset <= self.symbol_data_end)
                .ok_or_else(|| invalid_structure("symbol bucket offset is out of bounds"))?;
            let (count, count_size) = if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
                (
                    read_u32_checked(&self.mmap, absolute)
                        .ok_or_else(|| invalid_structure("symbol bucket count is truncated"))?
                        as usize,
                    4,
                )
            } else {
                (
                    read_u16_checked(&self.mmap, absolute).ok_or_else(|| {
                        invalid_structure("legacy symbol bucket count is truncated")
                    })? as usize,
                    2,
                )
            };
            let mut pos = absolute
                .checked_add(count_size)
                .ok_or_else(|| invalid_structure("symbol bucket start overflow"))?;
            let minimum_record_size = if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
                34
            } else {
                27
            };
            if count > self.symbol_data_end.saturating_sub(pos) / minimum_record_size {
                return Err(invalid_structure("symbol bucket count exceeds its data"));
            }
            total_symbols = total_symbols
                .checked_add(count as u64)
                .ok_or_else(|| invalid_structure("symbol count overflow"))?;

            for _ in 0..count {
                let relative_offset = pos
                    .checked_sub(self.symbol_data_offset)
                    .and_then(|offset| u32::try_from(offset).ok())
                    .ok_or_else(|| invalid_structure("symbol record offset exceeds u32"))?;
                let file_path_offset = self.validate_symbol_record(&mut pos, pool)?;
                if self.symbol_file_index_offset.is_some() {
                    records.push(ValidatedRecord {
                        relative_offset,
                        name_slot: u32::try_from(name_slot)
                            .map_err(|_| invalid_structure("name slot exceeds u32"))?,
                        file_path_offset,
                    });
                }
            }
            expected_symbol_offset = pos
                .checked_sub(self.symbol_data_offset)
                .ok_or_else(|| invalid_structure("symbol bucket ended before symbol data"))?;
        }

        if expected_symbol_offset != self.symbol_data_end - self.symbol_data_offset {
            return Err(invalid_structure(
                "symbol buckets do not consume the complete symbol-data section",
            ));
        }
        if total_symbols != self._symbol_count as u64 {
            return Err(invalid_structure(format!(
                "header declares {} symbols but records contain {total_symbols}",
                self._symbol_count
            )));
        }
        Ok(records)
    }

    fn validate_prefix_index(&self, pool: &StringPoolLayout) -> Result<()> {
        let Some(prefix_offset) = self.prefix_index_offset else {
            return Ok(());
        };
        let mut seen = vec![false; self.name_count as usize];
        let mut previous: Option<(String, String)> = None;
        for index in 0..self.name_count as usize {
            let offset = prefix_offset
                .checked_add(
                    index
                        .checked_mul(PREFIX_INDEX_ENTRY_SIZE)
                        .ok_or_else(|| invalid_structure("prefix index offset overflow"))?,
                )
                .ok_or_else(|| invalid_structure("prefix index offset overflow"))?;
            let slot = read_u32_checked(&self.mmap, offset)
                .ok_or_else(|| invalid_structure("prefix slot is truncated"))?
                as usize;
            let already_seen = seen
                .get_mut(slot)
                .ok_or_else(|| invalid_structure(format!("prefix slot {slot} is out of bounds")))?;
            if *already_seen {
                return Err(invalid_structure(format!(
                    "prefix slot {slot} appears more than once"
                )));
            }
            *already_seen = true;
            let name = self.validated_name_for_slot(pool, slot)?;
            let lower_name = name.to_lowercase();
            if let Some((previous_lower, previous_name)) = &previous {
                let ordering = lower_name
                    .cmp(previous_lower)
                    .then_with(|| name.cmp(previous_name));
                if ordering != std::cmp::Ordering::Greater {
                    return Err(invalid_structure("prefix index is not canonically sorted"));
                }
            }
            previous = Some((lower_name, name.to_string()));
        }
        Ok(())
    }

    fn validate_public_file_index(&self, pool: &StringPoolLayout) -> Result<()> {
        let Some(file_index_offset) = self.file_index_offset else {
            return Ok(());
        };
        let line_data_len = self
            .mmap
            .len()
            .checked_sub(self.public_line_data_offset)
            .ok_or_else(|| invalid_structure("public line data offset is out of bounds"))?;
        let mut expected_line_offset = 0usize;
        let mut previous: Option<(u64, String)> = None;

        for index in 0..self.file_count as usize {
            let offset = file_index_offset
                .checked_add(
                    index
                        .checked_mul(FILE_INDEX_ENTRY_SIZE)
                        .ok_or_else(|| invalid_structure("public file index offset overflow"))?,
                )
                .ok_or_else(|| invalid_structure("public file index offset overflow"))?;
            let hash = read_u64_checked(&self.mmap, offset)
                .ok_or_else(|| invalid_structure("public file hash is truncated"))?;
            let path_offset = read_u32_checked(&self.mmap, offset + 8)
                .ok_or_else(|| invalid_structure("public file path offset is truncated"))?;
            let lines_offset = read_u32_checked(&self.mmap, offset + 12)
                .ok_or_else(|| invalid_structure("public line offset is truncated"))?
                as usize;
            let line_count = read_u32_checked(&self.mmap, offset + 16)
                .ok_or_else(|| invalid_structure("public line count is truncated"))?
                as usize;
            let path = self.validated_string(pool, path_offset as usize, "public file path")?;
            let expected_hash = xxhash_rust::xxh3::xxh3_64(path.as_bytes());
            if hash != expected_hash {
                return Err(invalid_structure(format!(
                    "public file hash does not match {path:?}"
                )));
            }
            if let Some((previous_hash, previous_path)) = &previous {
                let ordering = hash
                    .cmp(previous_hash)
                    .then_with(|| path.cmp(previous_path.as_str()));
                if ordering != std::cmp::Ordering::Greater {
                    return Err(invalid_structure(
                        "public file index is not canonically sorted",
                    ));
                }
            }
            previous = Some((hash, path.to_string()));

            if lines_offset != expected_line_offset || !lines_offset.is_multiple_of(4) {
                return Err(invalid_structure(
                    "public line ranges are not canonical and contiguous",
                ));
            }
            let end = line_count
                .checked_mul(4)
                .and_then(|size| lines_offset.checked_add(size))
                .filter(|end| *end <= line_data_len)
                .ok_or_else(|| invalid_structure("public line range is out of bounds"))?;
            let absolute = self
                .public_line_data_offset
                .checked_add(lines_offset)
                .ok_or_else(|| invalid_structure("public line offset overflow"))?;
            let mut previous_line = None;
            for line_index in 0..line_count {
                let line = read_u32_checked(&self.mmap, absolute + line_index * 4)
                    .ok_or_else(|| invalid_structure("public line value is truncated"))?;
                if previous_line.is_some_and(|previous| previous >= line) {
                    return Err(invalid_structure(
                        "public definition lines are not strictly sorted",
                    ));
                }
                previous_line = Some(line);
            }
            expected_line_offset = end;
        }

        if expected_line_offset != line_data_len {
            return Err(invalid_structure(
                "public file ranges do not consume the public-line section",
            ));
        }
        Ok(())
    }

    fn validate_exact_file_index(
        &self,
        pool: &StringPoolLayout,
        records: &[ValidatedRecord],
    ) -> Result<()> {
        let (Some(file_index_offset), Some(postings_offset)) = (
            self.symbol_file_index_offset,
            self.symbol_file_postings_offset,
        ) else {
            return Ok(());
        };
        if self.symbol_file_posting_count as usize != records.len() {
            return Err(invalid_structure(format!(
                "exact-file postings contain {} records but symbol data contains {}",
                self.symbol_file_posting_count,
                records.len()
            )));
        }
        let postings_len = (self.symbol_file_posting_count as usize)
            .checked_mul(SYMBOL_FILE_POSTING_SIZE)
            .ok_or_else(|| invalid_structure("exact-file posting bytes overflow"))?;
        let postings_end = postings_offset
            .checked_add(postings_len)
            .filter(|end| *end <= self.mmap.len())
            .ok_or_else(|| invalid_structure("exact-file postings are out of bounds"))?;
        let mut expected_posting_offset = 0usize;
        let mut previous_file: Option<(u64, String)> = None;
        let mut seen_records = vec![false; records.len()];

        for index in 0..self.symbol_file_count as usize {
            let offset = file_index_offset
                .checked_add(
                    index
                        .checked_mul(SYMBOL_FILE_INDEX_ENTRY_SIZE)
                        .ok_or_else(|| invalid_structure("exact-file index offset overflow"))?,
                )
                .ok_or_else(|| invalid_structure("exact-file index offset overflow"))?;
            let hash = read_u64_checked(&self.mmap, offset)
                .ok_or_else(|| invalid_structure("exact-file hash is truncated"))?;
            let path_offset = read_u32_checked(&self.mmap, offset + 8)
                .ok_or_else(|| invalid_structure("exact-file path offset is truncated"))?;
            let entry_posting_offset = read_u32_checked(&self.mmap, offset + 12)
                .ok_or_else(|| invalid_structure("exact-file posting offset is truncated"))?
                as usize;
            let posting_count = read_u32_checked(&self.mmap, offset + 16)
                .ok_or_else(|| invalid_structure("exact-file posting count is truncated"))?
                as usize;
            let path = self.validated_string(pool, path_offset as usize, "exact-file path")?;
            let expected_hash = xxhash_rust::xxh3::xxh3_64(path.as_bytes());
            if hash != expected_hash {
                return Err(invalid_structure(format!(
                    "exact-file hash does not match {path:?}"
                )));
            }
            if let Some((previous_hash, previous_path)) = &previous_file {
                let ordering = hash
                    .cmp(previous_hash)
                    .then_with(|| path.cmp(previous_path.as_str()));
                if ordering != std::cmp::Ordering::Greater {
                    return Err(invalid_structure(
                        "exact-file index is not canonically sorted",
                    ));
                }
            }
            previous_file = Some((hash, path.to_string()));

            if entry_posting_offset != expected_posting_offset
                || !entry_posting_offset.is_multiple_of(SYMBOL_FILE_POSTING_SIZE)
            {
                return Err(invalid_structure(
                    "exact-file posting ranges are not canonical and contiguous",
                ));
            }
            let entry_end = posting_count
                .checked_mul(SYMBOL_FILE_POSTING_SIZE)
                .and_then(|size| entry_posting_offset.checked_add(size))
                .filter(|end| *end <= postings_len)
                .ok_or_else(|| invalid_structure("exact-file posting range is out of bounds"))?;
            let absolute = postings_offset
                .checked_add(entry_posting_offset)
                .ok_or_else(|| invalid_structure("exact-file posting offset overflow"))?;
            let mut previous_posting = None;
            for posting_index in 0..posting_count {
                let posting = absolute + posting_index * SYMBOL_FILE_POSTING_SIZE;
                let name_slot = read_u32_checked(&self.mmap[..postings_end], posting)
                    .ok_or_else(|| invalid_structure("exact-file name slot is truncated"))?;
                if name_slot >= self.name_count {
                    return Err(invalid_structure(format!(
                        "exact-file name slot {name_slot} is out of bounds"
                    )));
                }
                let record_offset = read_u32_checked(&self.mmap[..postings_end], posting + 4)
                    .ok_or_else(|| invalid_structure("exact-file record offset is truncated"))?;
                if previous_posting.is_some_and(|previous| previous >= (name_slot, record_offset)) {
                    return Err(invalid_structure(
                        "exact-file postings are not strictly sorted",
                    ));
                }
                previous_posting = Some((name_slot, record_offset));

                let record_index = records
                    .binary_search_by_key(&record_offset, |record| record.relative_offset)
                    .map_err(|_| {
                        invalid_structure(format!(
                            "exact-file record offset {record_offset} is not a record boundary"
                        ))
                    })?;
                let record = records[record_index];
                if record.name_slot != name_slot {
                    return Err(invalid_structure(
                        "exact-file posting points into a different name bucket",
                    ));
                }
                let record_path = self.validated_string(
                    pool,
                    record.file_path_offset as usize,
                    "posted symbol file path",
                )?;
                if record_path != path {
                    return Err(invalid_structure(
                        "exact-file posting points to a symbol from another file",
                    ));
                }
                if std::mem::replace(&mut seen_records[record_index], true) {
                    return Err(invalid_structure(
                        "symbol record appears in exact-file postings more than once",
                    ));
                }
            }
            expected_posting_offset = entry_end;
        }

        if expected_posting_offset != postings_len || seen_records.iter().any(|seen| !seen) {
            return Err(invalid_structure(
                "exact-file postings do not cover every symbol record exactly once",
            ));
        }
        Ok(())
    }

    fn validate_structure(&self) -> Result<()> {
        let pool = self.validate_string_pool()?;
        let records = self.validate_names_and_records(&pool)?;
        self.validate_prefix_index(&pool)?;
        self.validate_public_file_index(&pool)?;
        self.validate_exact_file_index(&pool, &records)
    }

    /// Whether this mmap contains every field represented by [`Symbol`].
    ///
    /// Legacy v1 files remain readable, but callers that also have
    /// `symbols.bin` should prefer that fallback rather than silently losing
    /// documentation, visibility, and type-relation data.
    pub fn preserves_full_fidelity(&self) -> bool {
        self.format_version >= FULL_FIDELITY_FORMAT_VERSION
    }

    /// Whether exact-file symbol queries use persisted file postings.
    ///
    /// Versions 1 and 2 remain readable, but their compatibility path scans
    /// every name bucket and must not be used as a bounded query primitive.
    pub fn supports_exact_file_postings(&self) -> bool {
        self.symbol_file_index_offset.is_some()
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
    ///
    /// Versions 2 and 3 use their legacy public-line index. Version 4 derives
    /// the answer from the exact-file postings and the referenced symbol
    /// records, without a corpus scan or a duplicate on-disk line index.
    pub fn has_public_symbol_in_range(
        &self,
        file_path: &str,
        line_start: u64,
        line_end: u64,
    ) -> bool {
        if line_start >= line_end {
            return false;
        }

        if self.file_index_offset.is_none() {
            let Some(_) = self.symbol_file_index_offset else {
                return false;
            };
            let hash = xxhash_rust::xxh3::xxh3_64(file_path.as_bytes());
            let mut low = 0usize;
            let mut high = self.symbol_file_count as usize;
            while low < high {
                let mid = low + (high - low) / 2;
                let Some(entry_hash) = self.symbol_file_entry_hash(mid) else {
                    return false;
                };
                if entry_hash < hash {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }

            for index in low..self.symbol_file_count as usize {
                let Some((entry_hash, path_offset, postings_offset, postings_count)) =
                    self.symbol_file_entry(index)
                else {
                    return false;
                };
                if entry_hash != hash {
                    break;
                }
                if self.read_string_pool(path_offset).as_deref() == Some(file_path) {
                    return self.public_symbol_in_postings(
                        postings_offset,
                        postings_count,
                        line_start,
                        line_end,
                    );
                }
            }
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

    /// Return symbols from one exact file, optionally filtering names by a
    /// case-insensitive substring.
    ///
    /// Version 3 and later resolve the file through persisted postings and
    /// decode only matching symbol records. Versions 1 and 2 remain readable
    /// through a full-table compatibility scan.
    pub fn symbols_in_file(&self, file_path: &str, name_pattern: Option<&str>) -> Vec<Symbol> {
        let pattern_lower = name_pattern.map(str::to_lowercase);
        let Some(_) = self.symbol_file_index_offset else {
            return self.symbols_in_file_legacy(file_path, pattern_lower.as_deref());
        };

        let hash = xxhash_rust::xxh3::xxh3_64(file_path.as_bytes());
        let mut low = 0usize;
        let mut high = self.symbol_file_count as usize;
        while low < high {
            let mid = low + (high - low) / 2;
            let Some(entry_hash) = self.symbol_file_entry_hash(mid) else {
                return Vec::new();
            };
            if entry_hash < hash {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        for index in low..self.symbol_file_count as usize {
            let Some((entry_hash, path_offset, postings_offset, postings_count)) =
                self.symbol_file_entry(index)
            else {
                return Vec::new();
            };
            if entry_hash != hash {
                break;
            }
            if self.read_string_pool(path_offset).as_deref() != Some(file_path) {
                continue;
            }
            return self.read_file_symbol_postings(
                file_path,
                postings_offset,
                postings_count,
                pattern_lower.as_deref(),
            );
        }
        Vec::new()
    }

    /// Total number of unique symbol names.
    pub fn len(&self) -> usize {
        self.name_count as usize
    }

    /// Returns `true` if the table has no symbols.
    pub fn is_empty(&self) -> bool {
        self.name_count == 0
    }

    /// Return the exact, case-preserving names in the mmap index.
    ///
    /// This materializes one string per name, but never decodes symbol
    /// records. Checkpoint writers use it to merge a small changed-file
    /// overlay without cloning the repository's complete symbol corpus.
    pub(crate) fn names(&self) -> Vec<String> {
        (0..self.name_count as usize)
            .filter_map(|index| self.name_entry(index).map(|(name, _)| name))
            .collect()
    }

    /// Whether one exact, case-preserving name bucket exists.
    pub(crate) fn contains_exact_name(&self, name: &str) -> bool {
        self.exact_name_entry(name).is_some()
    }

    /// Decode one exact, case-preserving name bucket.
    ///
    /// Unlike [`Self::lookup`], this does not combine names that differ only
    /// by case. That distinction is required when rebuilding the persisted
    /// name index deterministically.
    pub(crate) fn symbols_for_exact_name(&self, name: &str) -> Vec<Symbol> {
        let Some((stored_name, symbols_offset)) = self.exact_name_entry(name) else {
            return Vec::new();
        };
        self.read_symbols_at(symbols_offset, &stored_name)
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

    fn exact_name_entry(&self, name: &str) -> Option<(String, usize)> {
        let lower_name = name.to_lowercase();
        let hash = xxhash_rust::xxh3::xxh3_64(lower_name.as_bytes());
        let mut low = 0usize;
        let mut high = self.name_count as usize;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.read_name_hash(mid) < hash {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        for index in low..self.name_count as usize {
            if self.read_name_hash(index) != hash {
                break;
            }
            let Some((stored_name, symbols_offset)) = self.name_entry(index) else {
                continue;
            };
            if stored_name == name {
                return Some((stored_name, symbols_offset));
            }
        }
        None
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

    fn name_entry(&self, name_slot: usize) -> Option<(String, usize)> {
        if name_slot >= self.name_count as usize {
            return None;
        }
        let offset = self
            .name_index_offset
            .checked_add(name_slot.checked_mul(NAME_INDEX_ENTRY_SIZE)?)?;
        let name_offset = read_u32_checked(&self.mmap, offset + 8)? as usize;
        let symbols_offset = read_u32_checked(&self.mmap, offset + 12)? as usize;
        Some((self.read_string_pool(name_offset)?, symbols_offset))
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

    fn symbol_file_entry_hash(&self, index: usize) -> Option<u64> {
        let offset = self
            .symbol_file_index_offset?
            .checked_add(index.checked_mul(SYMBOL_FILE_INDEX_ENTRY_SIZE)?)?;
        read_u64_checked(&self.mmap, offset)
    }

    fn symbol_file_entry(&self, index: usize) -> Option<(u64, usize, usize, usize)> {
        let offset = self
            .symbol_file_index_offset?
            .checked_add(index.checked_mul(SYMBOL_FILE_INDEX_ENTRY_SIZE)?)?;
        Some((
            read_u64_checked(&self.mmap, offset)?,
            read_u32_checked(&self.mmap, offset + 8)? as usize,
            read_u32_checked(&self.mmap, offset + 12)? as usize,
            read_u32_checked(&self.mmap, offset + 16)? as usize,
        ))
    }

    fn read_file_symbol_postings(
        &self,
        file_path: &str,
        postings_offset: usize,
        postings_count: usize,
        pattern_lower: Option<&str>,
    ) -> Vec<Symbol> {
        let Some(postings_base) = self.symbol_file_postings_offset else {
            return Vec::new();
        };
        if !postings_offset.is_multiple_of(SYMBOL_FILE_POSTING_SIZE) {
            return Vec::new();
        }
        let Some(postings_end) = postings_count
            .checked_mul(SYMBOL_FILE_POSTING_SIZE)
            .and_then(|size| postings_offset.checked_add(size))
            .filter(|end| {
                *end <= (self.symbol_file_posting_count as usize)
                    .saturating_mul(SYMBOL_FILE_POSTING_SIZE)
            })
            .and_then(|end| postings_base.checked_add(end))
            .filter(|end| *end <= self.mmap.len())
        else {
            return Vec::new();
        };
        let Some(start) = postings_base.checked_add(postings_offset) else {
            return Vec::new();
        };

        let mut symbols = Vec::with_capacity(postings_count);
        for offset in (start..postings_end).step_by(SYMBOL_FILE_POSTING_SIZE) {
            let Some(name_slot) = read_u32_checked(&self.mmap, offset).map(|value| value as usize)
            else {
                return Vec::new();
            };
            let Some(record_offset) =
                read_u32_checked(&self.mmap, offset + 4).map(|value| value as usize)
            else {
                return Vec::new();
            };
            let Some((name, _)) = self.name_entry(name_slot) else {
                continue;
            };
            if pattern_lower.is_some_and(|pattern| !name.to_lowercase().contains(pattern)) {
                continue;
            }
            let Some(symbol) = self.try_read_symbol_at(record_offset, &name) else {
                continue;
            };
            if symbol.file_path == file_path {
                symbols.push(symbol);
            }
        }
        symbols
    }

    fn public_symbol_in_postings(
        &self,
        postings_offset: usize,
        postings_count: usize,
        line_start: u64,
        line_end: u64,
    ) -> bool {
        let Some(postings_base) = self.symbol_file_postings_offset else {
            return false;
        };
        if !postings_offset.is_multiple_of(SYMBOL_FILE_POSTING_SIZE) {
            return false;
        }
        let Some(postings_end) = postings_count
            .checked_mul(SYMBOL_FILE_POSTING_SIZE)
            .and_then(|size| postings_offset.checked_add(size))
            .filter(|end| {
                *end <= (self.symbol_file_posting_count as usize)
                    .saturating_mul(SYMBOL_FILE_POSTING_SIZE)
            })
            .and_then(|end| postings_base.checked_add(end))
            .filter(|end| *end <= self.mmap.len())
        else {
            return false;
        };
        let Some(start) = postings_base.checked_add(postings_offset) else {
            return false;
        };

        for posting in (start..postings_end).step_by(SYMBOL_FILE_POSTING_SIZE) {
            let Some(record_offset) = read_u32_checked(&self.mmap, posting + 4) else {
                return false;
            };
            let Some(record) = self
                .symbol_data_offset
                .checked_add(record_offset as usize)
                .filter(|offset| *offset < self.symbol_data_end)
            else {
                return false;
            };
            let Some(fixed_end) = record
                .checked_add(27)
                .filter(|end| *end <= self.symbol_data_end)
            else {
                return false;
            };
            let Some(symbol_line) = read_u32_checked(&self.mmap, record + 6) else {
                return false;
            };
            if (symbol_line as u64) < line_start || (symbol_line as u64) >= line_end {
                continue;
            }
            let scope_count = self.mmap[record + 26] as usize;
            let Some(visibility_offset) = scope_count
                .checked_mul(4)
                .and_then(|scope_bytes| fixed_end.checked_add(scope_bytes))
                .and_then(|offset| offset.checked_add(4))
                .filter(|offset| *offset < self.symbol_data_end)
            else {
                return false;
            };
            if self.mmap[visibility_offset] == visibility_to_u8(&Visibility::Public) {
                return true;
            }
        }
        false
    }

    fn symbols_in_file_legacy(&self, file_path: &str, pattern_lower: Option<&str>) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        for name_slot in 0..self.name_count as usize {
            let Some((name, symbols_offset)) = self.name_entry(name_slot) else {
                continue;
            };
            if pattern_lower.is_some_and(|pattern| !name.to_lowercase().contains(pattern)) {
                continue;
            }
            symbols.extend(
                self.read_symbols_at(symbols_offset, &name)
                    .into_iter()
                    .filter(|symbol| symbol.file_path == file_path),
            );
        }
        symbols
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
        let (length_size, len) = if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
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
        let (count, count_size) = if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
            (read_u32_checked(&self.mmap, abs)? as usize, 4)
        } else {
            (read_u16_checked(&self.mmap, abs)? as usize, 2)
        };
        let mut pos = abs.checked_add(count_size)?;
        let min_record_size = if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
            34
        } else {
            27
        };
        if count > self.symbol_data_end.saturating_sub(pos) / min_record_size {
            return None;
        }
        let mut symbols = Vec::with_capacity(count);

        for _ in 0..count {
            symbols.push(self.try_read_symbol_record(&mut pos, name)?);
        }

        Some(symbols)
    }

    fn try_read_symbol_at(&self, rel_offset: usize, name: &str) -> Option<Symbol> {
        let mut pos = self.symbol_data_offset.checked_add(rel_offset)?;
        self.try_read_symbol_record(&mut pos, name)
    }

    fn try_read_symbol_record(&self, pos: &mut usize, name: &str) -> Option<Symbol> {
        let fixed_end = (*pos).checked_add(27)?;
        if fixed_end > self.symbol_data_end {
            return None;
        }
        let kind_u8 = self.mmap[*pos];
        let lang_u8 = self.mmap[*pos + 1];
        let file_path_off = read_u32(&self.mmap, *pos + 2) as usize;
        let line_start = read_u32(&self.mmap, *pos + 6) as usize;
        let line_end = read_u32(&self.mmap, *pos + 10) as usize;
        let byte_start = read_u32(&self.mmap, *pos + 14) as usize;
        let byte_end = read_u32(&self.mmap, *pos + 18) as usize;
        let sig_off = read_u32(&self.mmap, *pos + 22) as usize;
        let scope_count = self.mmap[*pos + 26] as usize;
        *pos = fixed_end;

        let mut scope = Vec::with_capacity(scope_count);
        for _ in 0..scope_count {
            if (*pos).checked_add(4)? > self.symbol_data_end {
                return None;
            }
            let soff = read_u32(&self.mmap, *pos) as usize;
            scope.push(self.read_string_pool(soff)?);
            *pos += 4;
        }

        let (doc_comment, visibility, type_relations) =
            if self.format_version >= FULL_FIDELITY_FORMAT_VERSION {
                if (*pos).checked_add(7)? > self.symbol_data_end {
                    return None;
                }
                let doc_comment_off = read_u32(&self.mmap, *pos) as usize;
                let visibility = u8_to_visibility(self.mmap[*pos + 4]);
                let relation_count = read_u16(&self.mmap, *pos + 5) as usize;
                *pos += 7;

                if relation_count > self.symbol_data_end.saturating_sub(*pos) / 5 {
                    return None;
                }

                let mut type_relations = Vec::with_capacity(relation_count);
                for _ in 0..relation_count {
                    let kind = u8_to_type_relation_kind(self.mmap[*pos]);
                    let target_off = read_u32(&self.mmap, *pos + 1) as usize;
                    *pos += 5;
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

        Some(Symbol {
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
        })
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
