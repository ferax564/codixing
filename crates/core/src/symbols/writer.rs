//! Writer for the `symbols_v2.bin` flat binary format.
//!
//! Converts an `InMemorySymbolTable` (the current `DashMap`-backed table)
//! into the memory-mappable format described in [`super::mmap`].

use std::collections::HashMap;
use std::path::Path;

use crate::error::{CodixingError, Result};
use crate::language::Visibility;
use crate::symbols::InMemorySymbolTable;
use crate::symbols::mmap::{
    FILE_INDEX_ENTRY_SIZE, FORMAT_VERSION, HEADER_SIZE, MAGIC, NAME_INDEX_ENTRY_SIZE,
    PREFIX_INDEX_ENTRY_SIZE, entity_kind_to_u8, language_to_u8, type_relation_kind_to_u8,
    visibility_to_u8,
};

/// Write the mmap-format `symbols_v2.bin` from an in-memory symbol table.
pub fn write_mmap_symbols(table: &InMemorySymbolTable, path: &Path) -> Result<()> {
    // ── 1. Collect all unique strings into a pool ────────────────────────
    let mut string_pool = StringPool::new();

    // Pre-register all strings in the pool.
    for entry in &table.symbols {
        string_pool.intern(entry.key())?;
        for sym in entry.value() {
            string_pool.intern(&sym.file_path)?;
            if let Some(sig) = &sym.signature {
                string_pool.intern(sig)?;
            }
            if let Some(doc_comment) = &sym.doc_comment {
                string_pool.intern(doc_comment)?;
            }
            for scope_entry in &sym.scope {
                string_pool.intern(scope_entry)?;
            }
            for relation in &sym.type_relations {
                string_pool.intern(&relation.target)?;
            }
        }
    }

    // ── 2. Build name index sorted by xxh3(lowercase_name) ──────────────
    struct NameEntry {
        hash: u64,
        lower_name: String,
        name: String,
    }

    // Keep only one name per bucket. Symbol records remain borrowed from the
    // DashMap while writing instead of cloning the complete corpus in memory.
    let mut name_entries: Vec<NameEntry> = table
        .symbols
        .iter()
        .map(|entry| {
            let name = entry.key().clone();
            let lower_name = name.to_lowercase();
            let hash = xxhash_rust::xxh3::xxh3_64(lower_name.as_bytes());
            NameEntry {
                hash,
                lower_name,
                name,
            }
        })
        .collect();

    name_entries.sort_by(|left, right| {
        left.hash
            .cmp(&right.hash)
            .then_with(|| left.lower_name.cmp(&right.lower_name))
            .then_with(|| left.name.cmp(&right.name))
    });

    let name_count = u32::try_from(name_entries.len()).map_err(|_| {
        CodixingError::Serialization("too many symbol names for mmap format".to_string())
    })?;
    let symbol_count = table.symbols.iter().try_fold(0u32, |total, entry| {
        let bucket = u32::try_from(entry.value().len()).map_err(|_| {
            CodixingError::Serialization("too many symbols for mmap format".to_string())
        })?;
        total.checked_add(bucket).ok_or_else(|| {
            CodixingError::Serialization("too many symbols for mmap format".to_string())
        })
    })?;

    let mut prefix_slots: Vec<usize> = (0..name_entries.len()).collect();
    prefix_slots.sort_by(|left, right| {
        name_entries[*left]
            .lower_name
            .cmp(&name_entries[*right].lower_name)
            .then_with(|| name_entries[*left].name.cmp(&name_entries[*right].name))
    });
    let prefix_slots: Vec<u32> = prefix_slots
        .into_iter()
        .map(|slot| {
            u32::try_from(slot).map_err(|_| {
                CodixingError::Serialization("too many symbol names for prefix index".to_string())
            })
        })
        .collect::<Result<_>>()?;

    struct PublicFileEntry {
        hash: u64,
        path: String,
        lines: Vec<u32>,
    }
    let mut public_lines_by_file: HashMap<String, Vec<u32>> = HashMap::new();
    for entry in &table.symbols {
        for symbol in entry.value() {
            if symbol.visibility == Visibility::Public {
                public_lines_by_file
                    .entry(symbol.file_path.clone())
                    .or_default()
                    .push(u32_field(symbol.line_start, "line_start", &symbol.name)?);
            }
        }
    }
    let mut public_files: Vec<PublicFileEntry> = public_lines_by_file
        .into_iter()
        .map(|(path, mut lines)| {
            lines.sort_unstable();
            lines.dedup();
            PublicFileEntry {
                hash: xxhash_rust::xxh3::xxh3_64(path.as_bytes()),
                path,
                lines,
            }
        })
        .collect();
    public_files.sort_by(|left, right| {
        left.hash
            .cmp(&right.hash)
            .then_with(|| left.path.cmp(&right.path))
    });

    // ── 3. Build symbol data blob ────────────────────────────────────────
    let mut symbol_data = Vec::new();
    let mut symbol_offsets: Vec<u32> = Vec::with_capacity(name_entries.len());

    for entry in &name_entries {
        let offset = u32::try_from(symbol_data.len()).map_err(|_| {
            CodixingError::Serialization("symbol data exceeds mmap format limit".to_string())
        })?;
        symbol_offsets.push(offset);

        // Write count.
        let symbols = table.symbols.get(&entry.name).ok_or_else(|| {
            CodixingError::Serialization(format!(
                "symbol bucket {} disappeared while writing mmap",
                entry.name
            ))
        })?;
        let count = u32::try_from(symbols.value().len()).map_err(|_| {
            CodixingError::Serialization(format!(
                "symbol name {} has too many definitions for mmap format",
                entry.name
            ))
        })?;
        symbol_data.extend_from_slice(&count.to_le_bytes());

        for sym in symbols.value() {
            // kind: u8
            symbol_data.push(entity_kind_to_u8(&sym.kind));
            // language: u8
            symbol_data.push(language_to_u8(sym.language));
            // file_path_offset: u32
            let fp_off = string_pool.offset(&sym.file_path);
            symbol_data.extend_from_slice(&fp_off.to_le_bytes());
            // line_start: u32
            symbol_data.extend_from_slice(
                &u32_field(sym.line_start, "line_start", &sym.name)?.to_le_bytes(),
            );
            // line_end: u32
            symbol_data
                .extend_from_slice(&u32_field(sym.line_end, "line_end", &sym.name)?.to_le_bytes());
            // byte_start: u32
            symbol_data.extend_from_slice(
                &u32_field(sym.byte_start, "byte_start", &sym.name)?.to_le_bytes(),
            );
            // byte_end: u32
            symbol_data
                .extend_from_slice(&u32_field(sym.byte_end, "byte_end", &sym.name)?.to_le_bytes());
            // signature_offset: u32 (0 = None)
            let sig_off = match &sym.signature {
                Some(sig) => string_pool.offset(sig),
                None => 0u32,
            };
            symbol_data.extend_from_slice(&sig_off.to_le_bytes());
            // scope_count: u8
            let scope_count = u8::try_from(sym.scope.len()).map_err(|_| {
                CodixingError::Serialization(format!(
                    "symbol {} has too many scope components for mmap format",
                    sym.name
                ))
            })?;
            symbol_data.push(scope_count);
            // scope_offsets: [u32; scope_count]
            for scope_entry in &sym.scope {
                let soff = string_pool.offset(scope_entry);
                symbol_data.extend_from_slice(&soff.to_le_bytes());
            }
            // doc_comment_offset: u32 (0 = None)
            let doc_comment_off = match &sym.doc_comment {
                Some(doc_comment) => string_pool.offset(doc_comment),
                None => 0u32,
            };
            symbol_data.extend_from_slice(&doc_comment_off.to_le_bytes());
            // visibility: u8
            symbol_data.push(visibility_to_u8(&sym.visibility));
            // type_relation_count: u16
            let relation_count = u16::try_from(sym.type_relations.len()).map_err(|_| {
                CodixingError::Serialization(format!(
                    "symbol {} has too many type relations for mmap format",
                    sym.name
                ))
            })?;
            symbol_data.extend_from_slice(&relation_count.to_le_bytes());
            // type relations: kind(u8) + target_offset(u32)
            for relation in &sym.type_relations {
                symbol_data.push(type_relation_kind_to_u8(&relation.kind));
                let target_off = string_pool.offset(&relation.target);
                symbol_data.extend_from_slice(&target_off.to_le_bytes());
            }
        }
    }

    let file_count = u32::try_from(public_files.len()).map_err(|_| {
        CodixingError::Serialization("too many public-symbol files for mmap format".to_string())
    })?;
    let mut public_line_data = Vec::new();
    let mut public_line_offsets = Vec::with_capacity(public_files.len());
    let mut public_line_counts = Vec::with_capacity(public_files.len());
    for entry in &public_files {
        public_line_offsets.push(u32::try_from(public_line_data.len()).map_err(|_| {
            CodixingError::Serialization(
                "public symbol line index exceeds mmap format limit".to_string(),
            )
        })?);
        public_line_counts.push(u32::try_from(entry.lines.len()).map_err(|_| {
            CodixingError::Serialization(
                "too many public symbol lines in one file for mmap format".to_string(),
            )
        })?);
        for line in &entry.lines {
            public_line_data.extend_from_slice(&line.to_le_bytes());
        }
    }

    // ── 4. Collect name offsets before consuming the string pool ────────
    let name_offsets: Vec<u32> = name_entries
        .iter()
        .map(|entry| string_pool.offset(&entry.name))
        .collect();
    let public_file_path_offsets: Vec<u32> = public_files
        .iter()
        .map(|entry| string_pool.offset(&entry.path))
        .collect();

    // ── 5. Build the string pool blob ────────────────────────────────────
    let pool_bytes = string_pool.into_bytes();

    // ── 6. Assemble the file ─────────────────────────────────────────────
    let name_index_size = (name_count as usize)
        .checked_mul(NAME_INDEX_ENTRY_SIZE)
        .ok_or_else(|| {
            CodixingError::Serialization("symbol name index size overflow".to_string())
        })?;
    let prefix_index_size = prefix_slots
        .len()
        .checked_mul(PREFIX_INDEX_ENTRY_SIZE)
        .ok_or_else(|| {
            CodixingError::Serialization("symbol prefix index size overflow".to_string())
        })?;
    let file_index_size = public_files
        .len()
        .checked_mul(FILE_INDEX_ENTRY_SIZE)
        .ok_or_else(|| {
            CodixingError::Serialization("public symbol file index size overflow".to_string())
        })?;
    let total_size = HEADER_SIZE
        .checked_add(name_index_size)
        .and_then(|size| size.checked_add(prefix_index_size))
        .and_then(|size| size.checked_add(4))
        .and_then(|size| size.checked_add(file_index_size))
        .and_then(|size| size.checked_add(8))
        .and_then(|size| size.checked_add(pool_bytes.len()))
        .and_then(|size| size.checked_add(symbol_data.len()))
        .and_then(|size| size.checked_add(public_line_data.len()))
        .ok_or_else(|| CodixingError::Serialization("mmap symbol size overflow".to_string()))?;

    let mut buf = Vec::with_capacity(total_size);

    // Header.
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&name_count.to_le_bytes());
    buf.extend_from_slice(&symbol_count.to_le_bytes());

    // Name index.
    for (i, entry) in name_entries.iter().enumerate() {
        buf.extend_from_slice(&entry.hash.to_le_bytes());
        buf.extend_from_slice(&name_offsets[i].to_le_bytes());
        buf.extend_from_slice(&symbol_offsets[i].to_le_bytes());
    }

    // Case-insensitive prefix index into the hash-sorted name index.
    for slot in &prefix_slots {
        buf.extend_from_slice(&slot.to_le_bytes());
    }

    // Public-definition file/range index.
    buf.extend_from_slice(&file_count.to_le_bytes());
    for (index, entry) in public_files.iter().enumerate() {
        buf.extend_from_slice(&entry.hash.to_le_bytes());
        buf.extend_from_slice(&public_file_path_offsets[index].to_le_bytes());
        buf.extend_from_slice(&public_line_offsets[index].to_le_bytes());
        buf.extend_from_slice(&public_line_counts[index].to_le_bytes());
    }

    // Section sizes + bytes.
    let pool_size = u32::try_from(pool_bytes.len()).map_err(|_| {
        CodixingError::Serialization("symbol string pool exceeds mmap format limit".to_string())
    })?;
    let symbol_data_size = u32::try_from(symbol_data.len()).map_err(|_| {
        CodixingError::Serialization("symbol data exceeds mmap format limit".to_string())
    })?;
    buf.extend_from_slice(&pool_size.to_le_bytes());
    buf.extend_from_slice(&symbol_data_size.to_le_bytes());
    buf.extend_from_slice(&pool_bytes);

    // Symbol data.
    buf.extend_from_slice(&symbol_data);

    // Sorted public definition lines, grouped by file index entry.
    buf.extend_from_slice(&public_line_data);

    debug_assert_eq!(buf.len(), total_size);

    // ── 6. Write atomically ──────────────────────────────────────────────
    crate::persistence::atomic_write(path, &buf).map_err(|e| {
        CodixingError::Serialization(format!("failed to write symbols_v2.bin: {e}"))
    })?;

    Ok(())
}

// ── String pool builder ──────────────────────────────────────────────────

/// Builds a deduplicated pool of length-prefixed UTF-8 strings.
///
/// Each string is stored as `u32 len + bytes`. The pool tracks offsets
/// so that writers can reference strings by their byte offset.
struct StringPool {
    /// Map from string content to its byte offset in the pool.
    offsets: HashMap<String, u32>,
    /// The raw pool bytes.
    buf: Vec<u8>,
}

impl StringPool {
    fn new() -> Self {
        Self {
            offsets: HashMap::new(),
            // Reserve offset 0 for "no value" sentinel.
            // We write a zero-length string at offset 0 to handle the edge case
            // where signature_offset=0 means None. We need to make sure no real
            // string lands at offset 0. We accomplish this by writing a 4-byte
            // dummy (len=0) at the start.
            buf: vec![0u8; 4],
        }
    }

    /// Intern a string, returning its offset. If already interned, returns
    /// the existing offset.
    fn intern(&mut self, s: &str) -> Result<u32> {
        if let Some(&off) = self.offsets.get(s) {
            return Ok(off);
        }
        let offset = u32::try_from(self.buf.len()).map_err(|_| {
            CodixingError::Serialization("symbol string pool exceeds mmap format limit".to_string())
        })?;
        let len = u32::try_from(s.len()).map_err(|_| {
            CodixingError::Serialization("symbol string exceeds mmap format limit".to_string())
        })?;
        let new_len = self
            .buf
            .len()
            .checked_add(4)
            .and_then(|size| size.checked_add(s.len()))
            .filter(|size| *size <= u32::MAX as usize)
            .ok_or_else(|| {
                CodixingError::Serialization(
                    "symbol string pool exceeds mmap format limit".to_string(),
                )
            })?;
        self.buf.reserve(new_len - self.buf.len());
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(s.as_bytes());
        self.offsets.insert(s.to_string(), offset);
        Ok(offset)
    }

    /// Get the offset of a previously interned string.
    fn offset(&self, s: &str) -> u32 {
        *self.offsets.get(s).expect("string not interned")
    }

    /// Consume the pool and return the raw bytes.
    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

fn u32_field(value: usize, field: &str, symbol: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| {
        CodixingError::Serialization(format!(
            "symbol {symbol} has {field} outside the mmap format range"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_pool_deduplication() {
        let mut pool = StringPool::new();
        let off1 = pool.intern("hello").unwrap();
        let off2 = pool.intern("world").unwrap();
        let off3 = pool.intern("hello").unwrap();
        assert_eq!(off1, off3); // same string -> same offset
        assert_ne!(off1, off2); // different strings -> different offsets
        // offset 0 is the sentinel
        assert!(off1 >= 4);
    }
}
