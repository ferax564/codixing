//! Writer for the `symbols_v2.bin` flat binary format.
//!
//! Converts an `InMemorySymbolTable` (the current `DashMap`-backed table)
//! into the memory-mappable format described in [`super::mmap`].

use std::collections::HashMap;
use std::path::Path;

use crate::error::{CodixingError, Result};
use crate::symbols::InMemorySymbolTable;
use crate::symbols::mmap::{
    FORMAT_VERSION, HEADER_SIZE, MAGIC, NAME_INDEX_ENTRY_SIZE, entity_kind_to_u8, language_to_u8,
};

/// Write the mmap-format `symbols_v2.bin` from an in-memory symbol table.
pub fn write_mmap_symbols(table: &InMemorySymbolTable, path: &Path) -> Result<()> {
    // ── 1. Collect all unique strings into a pool ────────────────────────
    let mut string_pool = StringPool::new();

    // Gather entries: (name, Vec<Symbol>).
    // We iterate the DashMap and collect so we have a stable view.
    let mut entries: Vec<(String, Vec<crate::symbols::Symbol>)> = Vec::new();
    for entry in table.symbols.iter() {
        entries.push((entry.key().clone(), entry.value().clone()));
    }

    // Pre-register all strings in the pool.
    for (name, syms) in &entries {
        string_pool.intern(name);
        for sym in syms {
            string_pool.intern(&sym.file_path);
            if let Some(sig) = &sym.signature {
                string_pool.intern(sig);
            }
            for scope_entry in &sym.scope {
                string_pool.intern(scope_entry);
            }
        }
    }

    // ── 2. Build name index sorted by xxh3(lowercase_name) ──────────────
    struct NameEntry {
        hash: u64,
        name: String,
        symbols: Vec<crate::symbols::Symbol>,
    }

    let mut name_entries: Vec<NameEntry> = entries
        .into_iter()
        .map(|(name, symbols)| {
            let hash = xxhash_rust::xxh3::xxh3_64(name.to_lowercase().as_bytes());
            NameEntry {
                hash,
                name,
                symbols,
            }
        })
        .collect();

    name_entries.sort_by_key(|e| e.hash);

    let name_count = name_entries.len() as u32;
    let symbol_count: u32 = name_entries.iter().map(|e| e.symbols.len() as u32).sum();

    // ── 3. Build symbol data blob ────────────────────────────────────────
    let mut symbol_data = Vec::new();
    let mut symbol_offsets: Vec<u32> = Vec::with_capacity(name_entries.len());

    for entry in &name_entries {
        let offset = symbol_data.len() as u32;
        symbol_offsets.push(offset);

        // Write count.
        let count = entry.symbols.len() as u16;
        symbol_data.extend_from_slice(&count.to_le_bytes());

        for sym in &entry.symbols {
            // kind: u8
            symbol_data.push(entity_kind_to_u8(&sym.kind));
            // language: u8
            symbol_data.push(language_to_u8(sym.language));
            // file_path_offset: u32
            let fp_off = string_pool.offset(&sym.file_path);
            symbol_data.extend_from_slice(&fp_off.to_le_bytes());
            // line_start: u32
            symbol_data.extend_from_slice(&(sym.line_start as u32).to_le_bytes());
            // line_end: u32
            symbol_data.extend_from_slice(&(sym.line_end as u32).to_le_bytes());
            // byte_start: u32
            symbol_data.extend_from_slice(&(sym.byte_start as u32).to_le_bytes());
            // byte_end: u32
            symbol_data.extend_from_slice(&(sym.byte_end as u32).to_le_bytes());
            // signature_offset: u32 (0 = None)
            let sig_off = match &sym.signature {
                Some(sig) => string_pool.offset(sig),
                None => 0u32,
            };
            symbol_data.extend_from_slice(&sig_off.to_le_bytes());
            // scope_count: u8
            symbol_data.push(sym.scope.len() as u8);
            // scope_offsets: [u32; scope_count]
            for scope_entry in &sym.scope {
                let soff = string_pool.offset(scope_entry);
                symbol_data.extend_from_slice(&soff.to_le_bytes());
            }
        }
    }

    // ── 4. Collect name offsets before consuming the string pool ────────
    let name_offsets: Vec<u32> = name_entries
        .iter()
        .map(|entry| string_pool.offset(&entry.name))
        .collect();

    // ── 5. Build the string pool blob ────────────────────────────────────
    let pool_bytes = string_pool.into_bytes();

    // ── 6. Assemble the file ─────────────────────────────────────────────
    let name_index_size = name_count as usize * NAME_INDEX_ENTRY_SIZE;
    // After name index: string_pool_size(4) + pool_bytes + symbol_data.
    let total_size = HEADER_SIZE + name_index_size + 4 + pool_bytes.len() + symbol_data.len();

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

    // String pool size + bytes.
    buf.extend_from_slice(&(pool_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&pool_bytes);

    // Symbol data.
    buf.extend_from_slice(&symbol_data);

    debug_assert_eq!(buf.len(), total_size);

    // ── 6. Write atomically ──────────────────────────────────────────────
    std::fs::write(path, &buf).map_err(|e| {
        CodixingError::Serialization(format!("failed to write symbols_v2.bin: {e}"))
    })?;

    Ok(())
}

// ── String pool builder ──────────────────────────────────────────────────

/// Builds a deduplicated pool of length-prefixed UTF-8 strings.
///
/// Each string is stored as `u16 len + bytes`. The pool tracks offsets
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
            // string lands at offset 0. We accomplish this by writing a 2-byte
            // dummy (len=0) at the start.
            buf: vec![0u8, 0u8],
        }
    }

    /// Intern a string, returning its offset. If already interned, returns
    /// the existing offset.
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&off) = self.offsets.get(s) {
            return off;
        }
        let offset = self.buf.len() as u32;
        let len = s.len() as u16;
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(s.as_bytes());
        self.offsets.insert(s.to_string(), offset);
        offset
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_pool_deduplication() {
        let mut pool = StringPool::new();
        let off1 = pool.intern("hello");
        let off2 = pool.intern("world");
        let off3 = pool.intern("hello");
        assert_eq!(off1, off3); // same string -> same offset
        assert_ne!(off1, off2); // different strings -> different offsets
        // offset 0 is the sentinel
        assert!(off1 >= 2);
    }
}
