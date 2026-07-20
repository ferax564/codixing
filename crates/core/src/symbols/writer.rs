//! Writer for the `symbols_v2.bin` flat binary format.
//!
//! Converts an `InMemorySymbolTable` (the current `DashMap`-backed table)
//! into the memory-mappable format described in [`super::mmap`].

use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::{BufWriter, Seek, Write};
use std::path::Path;

use crate::error::{CodixingError, Result};
use crate::symbols::mmap::{
    FORMAT_VERSION, HEADER_SIZE, MAGIC, NAME_INDEX_ENTRY_SIZE, PREFIX_INDEX_ENTRY_SIZE,
    SYMBOL_FILE_INDEX_ENTRY_SIZE, entity_kind_to_u8, language_to_u8, type_relation_kind_to_u8,
    visibility_to_u8,
};
use crate::symbols::{InMemorySymbolTable, Symbol, SymbolTable};

trait SymbolBucketSource {
    fn names(&self) -> Vec<String>;
    fn symbols_for_exact_name(&self, name: &str) -> Vec<Symbol>;
}

impl SymbolBucketSource for InMemorySymbolTable {
    fn names(&self) -> Vec<String> {
        self.symbols
            .iter()
            .map(|entry| entry.key().to_string())
            .collect()
    }

    fn symbols_for_exact_name(&self, name: &str) -> Vec<Symbol> {
        self.lookup(name)
    }
}

impl SymbolBucketSource for SymbolTable {
    fn names(&self) -> Vec<String> {
        self.checkpoint_names()
    }

    fn symbols_for_exact_name(&self, name: &str) -> Vec<Symbol> {
        self.checkpoint_symbols_for_exact_name(name)
    }
}

#[derive(Debug, Default)]
struct WriteStats {
    name_count: usize,
    symbol_count: usize,
    peak_bucket_symbols: usize,
    final_output_buffer_bytes: usize,
}

/// Write the mmap-format `symbols_v2.bin` from an in-memory symbol table.
pub fn write_mmap_symbols(table: &InMemorySymbolTable, path: &Path) -> Result<()> {
    write_mmap_symbols_from_source(table, path).map(|_| ())
}

/// Write an mmap checkpoint directly from any symbol-table representation.
///
/// Mmap overlays are merged one exact-name bucket at a time, so a one-file
/// checkpoint never constructs an `InMemorySymbolTable` for the whole repo.
pub(crate) fn write_mmap_symbol_table(table: &SymbolTable, path: &Path) -> Result<()> {
    write_mmap_symbols_from_source(table, path).map(|_| ())
}

#[cfg(test)]
fn write_mmap_symbol_table_with_stats(table: &SymbolTable, path: &Path) -> Result<WriteStats> {
    write_mmap_symbols_from_source(table, path)
}

fn write_mmap_symbols_from_source(
    source: &impl SymbolBucketSource,
    path: &Path,
) -> Result<WriteStats> {
    // ── 1. Collect all unique strings into a pool ────────────────────────
    let mut string_pool = StringPool::new();
    let mut write_stats = WriteStats::default();

    // ── 2. Build name index sorted by xxh3(lowercase_name) ──────────────
    struct NameEntry {
        hash: u64,
        lower_name: String,
        name: String,
    }

    // Keep only one name per bucket. Symbol records remain borrowed from the
    // DashMap while writing instead of cloning the complete corpus in memory.
    let mut name_entries: Vec<NameEntry> = source
        .names()
        .into_iter()
        .map(|name| {
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
    write_stats.name_count = name_count as usize;

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

    // ── 3. Build symbol data blob ────────────────────────────────────────
    let mut symbol_data = Vec::new();
    let mut symbol_offsets: Vec<u32> = Vec::with_capacity(name_entries.len());
    let mut symbol_postings_by_file: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    let mut symbol_count = 0u32;

    for (name_slot, entry) in name_entries.iter().enumerate() {
        string_pool.intern(&entry.name)?;
        let offset = u32::try_from(symbol_data.len()).map_err(|_| {
            CodixingError::Serialization("symbol data exceeds mmap format limit".to_string())
        })?;
        symbol_offsets.push(offset);

        // Decode and sort just this exact-name bucket. Peak decoded Symbol
        // storage is therefore bounded by the largest overload set, not the
        // repository's total symbol count.
        let mut symbols = source.symbols_for_exact_name(&entry.name);
        write_stats.peak_bucket_symbols = write_stats.peak_bucket_symbols.max(symbols.len());
        symbols.sort_unstable_by(compare_symbols);
        let count = u32::try_from(symbols.len()).map_err(|_| {
            CodixingError::Serialization(format!(
                "symbol name {} has too many definitions for mmap format",
                entry.name
            ))
        })?;
        symbol_count = symbol_count.checked_add(count).ok_or_else(|| {
            CodixingError::Serialization("too many symbols for mmap format".to_string())
        })?;
        // Preserve the canonical string-pool order while decoding each bucket
        // only once. Offsets are stable after interning, so the second loop can
        // serialize the same sorted bucket without retaining corpus-wide data.
        for sym in &symbols {
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
        symbol_data.extend_from_slice(&count.to_le_bytes());

        let name_slot = u32::try_from(name_slot).map_err(|_| {
            CodixingError::Serialization("too many symbol names for mmap format".to_string())
        })?;
        for sym in &symbols {
            let record_offset = u32::try_from(symbol_data.len()).map_err(|_| {
                CodixingError::Serialization("symbol data exceeds mmap format limit".to_string())
            })?;
            symbol_postings_by_file
                .entry(sym.file_path.clone())
                .or_default()
                .push((name_slot, record_offset));
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
    write_stats.symbol_count = symbol_count as usize;

    struct SymbolFileEntry {
        hash: u64,
        path: String,
        postings: Vec<(u32, u32)>,
    }
    let mut symbol_files: Vec<SymbolFileEntry> = symbol_postings_by_file
        .into_iter()
        .map(|(path, mut postings)| {
            postings.sort_unstable();
            SymbolFileEntry {
                hash: xxhash_rust::xxh3::xxh3_64(path.as_bytes()),
                path,
                postings,
            }
        })
        .collect();
    symbol_files.sort_by(|left, right| {
        left.hash
            .cmp(&right.hash)
            .then_with(|| left.path.cmp(&right.path))
    });

    let symbol_file_count = u32::try_from(symbol_files.len()).map_err(|_| {
        CodixingError::Serialization("too many symbol files for mmap format".to_string())
    })?;
    let mut symbol_file_posting_offsets = Vec::with_capacity(symbol_files.len());
    let mut symbol_file_posting_counts = Vec::with_capacity(symbol_files.len());
    let mut symbol_file_posting_count = 0u32;
    let mut symbol_file_posting_data_size = 0usize;
    for entry in &symbol_files {
        symbol_file_posting_offsets.push(u32::try_from(symbol_file_posting_data_size).map_err(
            |_| {
                CodixingError::Serialization(
                    "symbol file postings exceed mmap format limit".to_string(),
                )
            },
        )?);
        let count = u32::try_from(entry.postings.len()).map_err(|_| {
            CodixingError::Serialization("too many symbols in one file for mmap format".to_string())
        })?;
        symbol_file_posting_counts.push(count);
        symbol_file_posting_count =
            symbol_file_posting_count
                .checked_add(count)
                .ok_or_else(|| {
                    CodixingError::Serialization(
                        "too many symbol file postings for mmap format".to_string(),
                    )
                })?;
        let entry_size = entry
            .postings
            .len()
            .checked_mul(crate::symbols::mmap::SYMBOL_FILE_POSTING_SIZE)
            .ok_or_else(|| {
                CodixingError::Serialization("symbol file postings size overflow".to_string())
            })?;
        symbol_file_posting_data_size = symbol_file_posting_data_size
            .checked_add(entry_size)
            .ok_or_else(|| {
                CodixingError::Serialization("symbol file postings size overflow".to_string())
            })?;
    }

    // ── 4. Collect name offsets before consuming the string pool ────────
    let name_offsets: Vec<u32> = name_entries
        .iter()
        .map(|entry| string_pool.offset(&entry.name))
        .collect();
    let symbol_file_path_offsets: Vec<u32> = symbol_files
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
    let symbol_file_index_size = symbol_files
        .len()
        .checked_mul(SYMBOL_FILE_INDEX_ENTRY_SIZE)
        .ok_or_else(|| {
            CodixingError::Serialization("symbol file index size overflow".to_string())
        })?;
    let total_size = HEADER_SIZE
        .checked_add(name_index_size)
        .and_then(|size| size.checked_add(prefix_index_size))
        .and_then(|size| size.checked_add(4))
        .and_then(|size| size.checked_add(symbol_file_index_size))
        .and_then(|size| size.checked_add(4))
        .and_then(|size| size.checked_add(symbol_file_posting_data_size))
        .and_then(|size| size.checked_add(8))
        .and_then(|size| size.checked_add(pool_bytes.len()))
        .and_then(|size| size.checked_add(symbol_data.len()))
        .ok_or_else(|| CodixingError::Serialization("mmap symbol size overflow".to_string()))?;
    let pool_size = u32::try_from(pool_bytes.len()).map_err(|_| {
        CodixingError::Serialization("symbol string pool exceeds mmap format limit".to_string())
    })?;
    let symbol_data_size = u32::try_from(symbol_data.len()).map_err(|_| {
        CodixingError::Serialization("symbol data exceeds mmap format limit".to_string())
    })?;
    write_stats.final_output_buffer_bytes = 0;

    // ── 6. Stream directly into the durable atomic scratch file ─────────
    crate::persistence::atomic_write_with(path, move |file| {
        // Symbol tables contain hundreds of thousands of small fixed-width
        // fields. Writing each one directly to `File` turns checkpointing into
        // one syscall per field on large repositories. Keep the streaming
        // memory bound while coalescing those fields into normal buffered I/O.
        let mut writer = BufWriter::new(file);
        writer.write_all(&MAGIC.to_le_bytes())?;
        writer.write_all(&FORMAT_VERSION.to_le_bytes())?;
        writer.write_all(&name_count.to_le_bytes())?;
        writer.write_all(&symbol_count.to_le_bytes())?;

        for (index, entry) in name_entries.iter().enumerate() {
            writer.write_all(&entry.hash.to_le_bytes())?;
            writer.write_all(&name_offsets[index].to_le_bytes())?;
            writer.write_all(&symbol_offsets[index].to_le_bytes())?;
        }
        for slot in &prefix_slots {
            writer.write_all(&slot.to_le_bytes())?;
        }

        writer.write_all(&symbol_file_count.to_le_bytes())?;
        for (index, entry) in symbol_files.iter().enumerate() {
            writer.write_all(&entry.hash.to_le_bytes())?;
            writer.write_all(&symbol_file_path_offsets[index].to_le_bytes())?;
            writer.write_all(&symbol_file_posting_offsets[index].to_le_bytes())?;
            writer.write_all(&symbol_file_posting_counts[index].to_le_bytes())?;
        }
        writer.write_all(&symbol_file_posting_count.to_le_bytes())?;
        for entry in symbol_files {
            for (name_slot, record_offset) in entry.postings {
                writer.write_all(&name_slot.to_le_bytes())?;
                writer.write_all(&record_offset.to_le_bytes())?;
            }
        }

        writer.write_all(&pool_size.to_le_bytes())?;
        writer.write_all(&symbol_data_size.to_le_bytes())?;
        writer.write_all(&pool_bytes)?;
        writer.write_all(&symbol_data)?;

        writer.flush()?;
        debug_assert_eq!(writer.stream_position()?, total_size as u64);
        Ok(())
    })
    .map_err(|e| CodixingError::Serialization(format!("failed to write symbols_v2.bin: {e}")))?;

    Ok(write_stats)
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

fn compare_symbols(left: &Symbol, right: &Symbol) -> Ordering {
    left.file_path
        .cmp(&right.file_path)
        .then_with(|| left.line_start.cmp(&right.line_start))
        .then_with(|| left.line_end.cmp(&right.line_end))
        .then_with(|| left.byte_start.cmp(&right.byte_start))
        .then_with(|| left.byte_end.cmp(&right.byte_end))
        .then_with(|| entity_kind_to_u8(&left.kind).cmp(&entity_kind_to_u8(&right.kind)))
        .then_with(|| language_to_u8(left.language).cmp(&language_to_u8(right.language)))
        .then_with(|| left.signature.cmp(&right.signature))
        .then_with(|| left.scope.cmp(&right.scope))
        .then_with(|| left.doc_comment.cmp(&right.doc_comment))
        .then_with(|| visibility_to_u8(&left.visibility).cmp(&visibility_to_u8(&right.visibility)))
        .then_with(|| compare_type_relations(left, right))
}

fn compare_type_relations(left: &Symbol, right: &Symbol) -> Ordering {
    for (left_relation, right_relation) in left.type_relations.iter().zip(&right.type_relations) {
        let ordering = type_relation_kind_to_u8(&left_relation.kind)
            .cmp(&type_relation_kind_to_u8(&right_relation.kind))
            .then_with(|| left_relation.target.cmp(&right_relation.target));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.type_relations.len().cmp(&right.type_relations.len())
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
    use crate::language::{EntityKind, Language, Visibility};
    use crate::symbols::mmap::MmapSymbolTable;
    use std::cell::Cell;
    use tempfile::tempdir;

    fn symbol(name: &str, file_path: &str, line: usize) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: EntityKind::Function,
            language: Language::Rust,
            file_path: file_path.to_string(),
            line_start: line,
            line_end: line + 1,
            byte_start: line * 10,
            byte_end: line * 10 + 5,
            signature: Some(format!("fn {name}()")),
            scope: Vec::new(),
            doc_comment: None,
            visibility: Visibility::Public,
            type_relations: Vec::new(),
        }
    }

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

    #[test]
    fn mmap_writer_decodes_each_name_bucket_once_with_byte_parity() {
        struct CountingSource {
            names: Vec<String>,
            buckets: HashMap<String, Vec<Symbol>>,
            bucket_reads: Cell<usize>,
        }

        impl SymbolBucketSource for CountingSource {
            fn names(&self) -> Vec<String> {
                self.names.clone()
            }

            fn symbols_for_exact_name(&self, name: &str) -> Vec<Symbol> {
                self.bucket_reads.set(self.bucket_reads.get() + 1);
                self.buckets.get(name).cloned().unwrap_or_default()
            }
        }

        let symbols = vec![
            symbol("same", "src/b.rs", 3),
            symbol("Alpha", "src/a.rs", 1),
            symbol("same", "src/a.rs", 2),
        ];
        let expected = InMemorySymbolTable::new();
        let mut buckets: HashMap<String, Vec<Symbol>> = HashMap::new();
        for symbol in symbols {
            expected.insert(symbol.clone());
            buckets.entry(symbol.name.clone()).or_default().push(symbol);
        }
        let source = CountingSource {
            names: vec!["same".to_string(), "Alpha".to_string()],
            buckets,
            bucket_reads: Cell::new(0),
        };

        let dir = tempdir().unwrap();
        let expected_path = dir.path().join("expected.bin");
        let actual_path = dir.path().join("actual.bin");
        write_mmap_symbols(&expected, &expected_path).unwrap();
        write_mmap_symbols_from_source(&source, &actual_path).unwrap();

        assert_eq!(source.bucket_reads.get(), source.names.len());
        assert_eq!(
            std::fs::read(expected_path).unwrap(),
            std::fs::read(actual_path).unwrap()
        );
    }

    #[test]
    fn mmap_writer_streams_without_a_corpus_sized_final_buffer_or_leftover_scratch() {
        let table = SymbolTable::new();
        for index in 0..256 {
            table.insert(symbol(
                &format!("symbol_{index:04}"),
                &format!("src/file_{index:04}.rs"),
                index + 1,
            ));
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");

        let stats = write_mmap_symbol_table_with_stats(&table, &path).unwrap();

        assert_eq!(stats.final_output_buffer_bytes, 0);
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        let leftover_scratch: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover_scratch.is_empty());
    }

    #[test]
    fn overlay_checkpoint_matches_materialized_table_exactly() {
        let dir = tempdir().unwrap();
        let base_path = dir.path().join("base.bin");
        let overlay_path = dir.path().join("overlay.bin");
        let expected_path = dir.path().join("expected.bin");

        let base = InMemorySymbolTable::new();
        base.insert(symbol("removed", "src/changed.rs", 1));
        base.insert(symbol("shared", "src/changed.rs", 2));
        base.insert(symbol("shared", "src/keep.rs", 3));
        base.insert(symbol("Untouched", "src/keep.rs", 4));
        base.insert(symbol("untouched", "src/keep.rs", 5));
        write_mmap_symbols(&base, &base_path).unwrap();

        let mut overlay = SymbolTable::Mmap(MmapSymbolTable::load(&base_path).unwrap());
        overlay.ensure_mutable();
        overlay.remove_file("src/changed.rs");
        overlay.insert(symbol("replacement", "src/changed.rs", 10));
        overlay.insert(symbol("shared", "src/changed.rs", 11));
        write_mmap_symbol_table(&overlay, &overlay_path).unwrap();

        let expected = InMemorySymbolTable::new();
        expected.insert(symbol("shared", "src/keep.rs", 3));
        expected.insert(symbol("Untouched", "src/keep.rs", 4));
        expected.insert(symbol("untouched", "src/keep.rs", 5));
        expected.insert(symbol("replacement", "src/changed.rs", 10));
        expected.insert(symbol("shared", "src/changed.rs", 11));
        write_mmap_symbols(&expected, &expected_path).unwrap();

        assert_eq!(
            std::fs::read(&overlay_path).unwrap(),
            std::fs::read(&expected_path).unwrap()
        );
        let restored = MmapSymbolTable::load(&overlay_path).unwrap();
        assert_eq!(restored.len(), 4);
        assert_eq!(restored.symbols_in_file("src/changed.rs", None).len(), 2);
        assert!(restored.lookup("removed").is_empty());
    }

    #[test]
    fn overlay_checkpoint_decodes_at_most_one_name_bucket() {
        const BASE_NAMES: usize = 4_096;
        let dir = tempdir().unwrap();
        let base_path = dir.path().join("base.bin");
        let checkpoint_path = dir.path().join("checkpoint.bin");

        let base = InMemorySymbolTable::new();
        for index in 0..BASE_NAMES {
            base.insert(symbol(
                &format!("symbol_{index:05}"),
                &format!("src/file_{index:05}.rs"),
                index + 1,
            ));
        }
        write_mmap_symbols(&base, &base_path).unwrap();

        let mut overlay = SymbolTable::Mmap(MmapSymbolTable::load(&base_path).unwrap());
        overlay.ensure_mutable();
        overlay.remove_file("src/file_02048.rs");
        overlay.insert(symbol("symbol_02048", "src/file_02048.rs", 9_000));
        overlay.insert(symbol("brand_new", "src/new.rs", 9_001));

        assert_eq!(overlay.len(), BASE_NAMES + 1);
        let stats = write_mmap_symbol_table_with_stats(&overlay, &checkpoint_path).unwrap();
        assert_eq!(stats.name_count, BASE_NAMES + 1);
        assert_eq!(stats.symbol_count, BASE_NAMES + 1);
        assert_eq!(
            stats.peak_bucket_symbols, 1,
            "checkpointing must not decode the full mmap corpus"
        );
        assert_eq!(stats.final_output_buffer_bytes, 0);
    }
}
