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
//!   version: u32 = 1
//!   name_count: u32     // distinct symbol names
//!   symbol_count: u32   // total symbols
//!
//! [Name Index: name_count × 16 bytes, sorted by name_hash]
//!   name_hash: u64      // xxh3 of lowercase name
//!   name_offset: u32    // byte offset into String Pool
//!   symbols_offset: u32 // byte offset into Symbol Data
//!
//! [String Pool: variable]
//!   Sequence of length-prefixed UTF-8 strings: u16 len + bytes
//!
//! [Symbol Data: variable]
//!   Per name entry: u16 count + count × SymbolRecord
//! ```

use std::path::Path;

use memmap2::Mmap;

use crate::error::{CodixingError, Result};
use crate::language::{EntityKind, Language, Visibility};
use crate::symbols::Symbol;

/// Magic bytes: "SYMB" as little-endian u32.
pub const MAGIC: u32 = 0x53594D42;

/// Current format version.
pub const FORMAT_VERSION: u32 = 1;

/// Header size in bytes: magic(4) + version(4) + name_count(4) + symbol_count(4).
pub const HEADER_SIZE: usize = 16;

/// Size of one name-index entry: hash(8) + name_offset(4) + symbols_offset(4).
pub const NAME_INDEX_ENTRY_SIZE: usize = 16;

/// Memory-mapped symbol table that provides O(log N) lookup by name
/// and O(N) filter scans without any deserialization.
pub struct MmapSymbolTable {
    mmap: Mmap,
    name_count: u32,
    _symbol_count: u32,
    name_index_offset: usize,
    string_pool_offset: usize,
    symbol_data_offset: usize,
}

impl MmapSymbolTable {
    /// Load a memory-mapped symbol table from `symbols_v2.bin`.
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;
        let file_len = file.metadata()?.len() as usize;

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
        if version != FORMAT_VERSION {
            return Err(CodixingError::Serialization(format!(
                "unsupported symbols_v2.bin version: expected {FORMAT_VERSION}, got {version}"
            )));
        }

        let name_count = read_u32(&mmap, 8);
        let symbol_count = read_u32(&mmap, 12);

        let name_index_offset = HEADER_SIZE;
        let string_pool_offset = name_index_offset + (name_count as usize) * NAME_INDEX_ENTRY_SIZE;

        // The symbol_data_offset is stored after the string pool.
        // We need to find it by reading the string pool size from the header area.
        // Actually, we encoded it as the first 4 bytes after the name index
        // just before the string pool. Let me reconsider the layout.
        //
        // Better approach: store string_pool_size in the header area, or compute
        // the symbol_data_offset from the last name entry's symbols_offset.
        //
        // Simplest: we stored string_pool_size as an extra u32 right after the
        // header (before the name index). Let me adjust.
        //
        // Actually, let's store it cleanly: the writer writes string_pool_size
        // into bytes 16..20 of the file, shifting the name index to offset 20.
        // But that changes the header. Let's instead use a different approach:
        //
        // After the name index, store: string_pool_size(u32) + string_pool_bytes + symbol_data.
        // This is cleaner and doesn't change the 16-byte header.

        if file_len < string_pool_offset + 4 {
            return Err(CodixingError::Serialization(
                "symbols_v2.bin truncated: missing string pool size".to_string(),
            ));
        }

        let string_pool_size = read_u32(&mmap, string_pool_offset) as usize;
        let actual_string_pool_offset = string_pool_offset + 4;
        let symbol_data_offset = actual_string_pool_offset + string_pool_size;

        Ok(Self {
            mmap,
            name_count,
            _symbol_count: symbol_count,
            name_index_offset,
            string_pool_offset: actual_string_pool_offset,
            symbol_data_offset,
        })
    }

    /// Exact name lookup via binary search on the name hash index. O(log N).
    pub fn lookup(&self, name: &str) -> Vec<Symbol> {
        let hash = xxhash_rust::xxh3::xxh3_64(name.to_lowercase().as_bytes());
        self.lookup_by_hash(hash, name)
    }

    /// Prefix lookup -- find all symbols whose name starts with `prefix`.
    ///
    /// This is O(N) since we must scan all names in the string pool.
    pub fn lookup_prefix(&self, prefix: &str) -> Vec<Symbol> {
        let prefix_lower = prefix.to_lowercase();
        let mut results = Vec::new();

        for i in 0..self.name_count as usize {
            let entry_offset = self.name_index_offset + i * NAME_INDEX_ENTRY_SIZE;
            let name_off = read_u32(&self.mmap, entry_offset + 8) as usize;
            let syms_off = read_u32(&self.mmap, entry_offset + 12) as usize;

            let stored_name = self.read_string_pool(name_off);
            if stored_name.to_lowercase().starts_with(&prefix_lower) {
                results.extend(self.read_symbols_at(syms_off, &stored_name));
            }
        }

        results
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

            let stored_name = self.read_string_pool(name_off);
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

            let name = self.read_string_pool(name_off);
            all.extend(self.read_symbols_at(syms_off, &name));
        }
        all
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

            let stored_name = self.read_string_pool(name_off);
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

    /// Read a length-prefixed string from the string pool at the given
    /// relative offset (relative to string_pool_offset).
    fn read_string_pool(&self, rel_offset: usize) -> String {
        let abs = self.string_pool_offset + rel_offset;
        let len = read_u16(&self.mmap, abs) as usize;
        let bytes = &self.mmap[abs + 2..abs + 2 + len];
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Read all symbols for a given name entry from the symbol data section.
    /// `rel_offset` is relative to `symbol_data_offset`.
    fn read_symbols_at(&self, rel_offset: usize, name: &str) -> Vec<Symbol> {
        let abs = self.symbol_data_offset + rel_offset;
        let count = read_u16(&self.mmap, abs) as usize;
        let mut pos = abs + 2;
        let mut symbols = Vec::with_capacity(count);

        for _ in 0..count {
            let kind_u8 = self.mmap[pos];
            let lang_u8 = self.mmap[pos + 1];
            let file_path_off = read_u32(&self.mmap, pos + 2) as usize;
            let line_start = read_u32(&self.mmap, pos + 6) as usize;
            let line_end = read_u32(&self.mmap, pos + 10) as usize;
            let byte_start = read_u32(&self.mmap, pos + 14) as usize;
            let byte_end = read_u32(&self.mmap, pos + 18) as usize;
            let sig_off = read_u32(&self.mmap, pos + 22) as usize;
            let scope_count = self.mmap[pos + 26] as usize;
            pos += 27;

            let mut scope = Vec::with_capacity(scope_count);
            for _ in 0..scope_count {
                let soff = read_u32(&self.mmap, pos) as usize;
                scope.push(self.read_string_pool(soff));
                pos += 4;
            }

            let file_path = self.read_string_pool(file_path_off);
            let signature = if sig_off == 0 {
                None
            } else {
                Some(self.read_string_pool(sig_off))
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
                doc_comment: None,
                visibility: Visibility::default(),
                type_relations: Vec::new(),
            });
        }

        symbols
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
        _ => Language::Rust, // fallback
    }
}

// ── Low-level read helpers ───────────────────────────────────────────────

#[inline]
fn read_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(buf[offset..offset + 2].try_into().unwrap())
}

#[inline]
fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap())
}

#[inline]
fn read_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap())
}
