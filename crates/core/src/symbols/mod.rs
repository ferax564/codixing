pub mod mmap;
pub mod persistence;
pub mod writer;

use dashmap::mapref::entry::Entry;
use dashmap::{DashMap, DashSet};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::language::{EntityKind, Language, TypeRelation, Visibility};

/// A symbol extracted from source code, representing a named semantic entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    /// The symbol name (e.g., function name, struct name).
    pub name: String,
    /// What kind of entity this symbol represents.
    pub kind: EntityKind,
    /// The programming language of the source file.
    pub language: Language,
    /// Path to the source file containing this symbol.
    pub file_path: String,
    /// First line of the symbol definition (0-indexed).
    pub line_start: usize,
    /// Last line of the symbol definition (0-indexed).
    pub line_end: usize,
    /// Byte offset where the symbol starts.
    pub byte_start: usize,
    /// Byte offset where the symbol ends.
    pub byte_end: usize,
    /// Optional signature (e.g., `fn foo(x: i32) -> bool`).
    pub signature: Option<String>,
    /// Scope chain from outermost to innermost.
    pub scope: Vec<String>,
    /// Optional doc comment text.
    pub doc_comment: Option<String>,
    /// Visibility level (public, crate-internal, private).
    pub visibility: Visibility,
    /// Type relationships extracted from AST.
    pub type_relations: Vec<TypeRelation>,
}

/// Mutable, concurrent symbol table backed by `DashMap`.
///
/// Used during `init` and `sync` when the table needs to be mutated.
/// Converted to/from the mmap format for persistence.
pub struct InMemorySymbolTable {
    pub(crate) symbols: DashMap<Arc<str>, Vec<Symbol>>,
    symbols_by_file: DashMap<String, Vec<Arc<str>>>,
    sorted_names: std::sync::RwLock<Vec<String>>,
    sorted_names_dirty: std::sync::atomic::AtomicBool,
    public_lines_by_file: std::sync::RwLock<std::collections::HashMap<String, Vec<usize>>>,
    public_lines_dirty: std::sync::atomic::AtomicBool,
}

/// Changed-file overlay for an immutable mmap symbol snapshot.
///
/// Reindexing one file tombstones that file in the base and stores only its
/// replacement symbols in `additions`. This keeps the editor hot path
/// proportional to the changed file instead of decoding the whole table.
pub struct MmapSymbolOverlay {
    base: mmap::MmapSymbolTable,
    additions: InMemorySymbolTable,
    removed_files: DashSet<String>,
}

impl MmapSymbolOverlay {
    fn new(base: mmap::MmapSymbolTable) -> Self {
        Self {
            base,
            additions: InMemorySymbolTable::new(),
            removed_files: DashSet::new(),
        }
    }

    fn from_file_replacements(
        base: mmap::MmapSymbolTable,
        replacements: Vec<persistence::SymbolFileReplacement>,
    ) -> Self {
        let overlay = Self::new(base);
        for (file_path, symbols) in replacements {
            overlay.remove_file(&file_path);
            for symbol in symbols {
                debug_assert_eq!(symbol.file_path, file_path);
                overlay.insert(symbol);
            }
        }
        overlay
    }

    fn insert(&self, symbol: Symbol) {
        self.additions.insert(symbol);
    }

    fn remove_file(&self, file_path: &str) {
        self.removed_files.insert(file_path.to_string());
        self.additions.remove_file(file_path);
    }

    fn merge_visible(&self, mut base: Vec<Symbol>, additions: Vec<Symbol>) -> Vec<Symbol> {
        base.retain(|symbol| !self.removed_files.contains(&symbol.file_path));
        base.extend(additions);
        base
    }

    fn lookup(&self, name: &str) -> Vec<Symbol> {
        self.merge_visible(
            self.base.lookup(name),
            self.additions.lookup_case_insensitive(name),
        )
    }

    fn lookup_prefix(&self, prefix: &str) -> Vec<Symbol> {
        self.merge_visible(
            self.base.lookup_prefix(prefix),
            self.additions.lookup_prefix(prefix),
        )
    }

    fn has_public_symbol_in_range(&self, file_path: &str, line_start: u64, line_end: u64) -> bool {
        self.additions
            .has_public_symbol_in_range(file_path, line_start, line_end)
            || (!self.removed_files.contains(file_path)
                && self
                    .base
                    .has_public_symbol_in_range(file_path, line_start, line_end))
    }

    fn filter(&self, pattern: &str, file: Option<&str>) -> Vec<Symbol> {
        self.merge_visible(
            self.base.filter(pattern, file),
            self.additions.filter(pattern, file),
        )
    }

    fn symbols_in_file(&self, file_path: &str, name_pattern: Option<&str>) -> Vec<Symbol> {
        if self.removed_files.contains(file_path) {
            return self.additions.symbols_in_file(file_path, name_pattern);
        }
        let mut symbols = self.base.symbols_in_file(file_path, name_pattern);
        symbols.extend(self.additions.symbols_in_file(file_path, name_pattern));
        symbols
    }

    fn supports_exact_file_postings(&self) -> bool {
        self.base.supports_exact_file_postings()
    }

    fn visit_symbols(&self, mut visitor: impl FnMut(&Symbol)) {
        self.base.visit_symbols(|symbol| {
            if !self.removed_files.contains(&symbol.file_path) {
                visitor(symbol);
            }
        });
        self.additions.visit_symbols(visitor);
    }

    fn all_symbols(&self) -> Vec<Symbol> {
        let mut symbols = Vec::new();
        self.visit_symbols(|symbol| symbols.push(symbol.clone()));
        symbols
    }

    fn removed_base_names(&self) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        for file_path in self.removed_files.iter() {
            for symbol in self.base.symbols_in_file(file_path.key(), None) {
                names.insert(symbol.name);
            }
        }
        names
    }

    fn base_name_is_visible(&self, name: &str) -> bool {
        self.base
            .symbols_for_exact_name(name)
            .into_iter()
            .any(|symbol| !self.removed_files.contains(&symbol.file_path))
    }

    /// Exact visible names for checkpoint persistence.
    ///
    /// The mmap name list is unavoidable in the output format. Additional
    /// working memory is bounded by names touched in the changed files rather
    /// than by every decoded symbol in the repository.
    fn checkpoint_names(&self) -> Vec<String> {
        let removed_base_names = self.removed_base_names();
        let addition_names: std::collections::HashSet<String> = self
            .additions
            .symbols
            .iter()
            .map(|entry| entry.key().to_string())
            .collect();
        let mut names = self.base.names();
        names.retain(|name| {
            !removed_base_names.contains(name)
                || self.base_name_is_visible(name)
                || addition_names.contains(name)
        });
        names.extend(addition_names);
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Complete changed-file replacements for the durable overlay sidecar.
    /// Paths are sorted so serialization is deterministic. A path with no
    /// additions is retained as a deletion tombstone.
    fn checkpoint_file_replacements(&self) -> Vec<persistence::SymbolFileReplacement> {
        let mut paths = std::collections::BTreeSet::new();
        paths.extend(
            self.removed_files
                .iter()
                .map(|file_path| file_path.key().clone()),
        );
        paths.extend(
            self.additions
                .symbols_by_file
                .iter()
                .map(|entry| entry.key().clone()),
        );
        paths
            .into_iter()
            .map(|file_path| {
                let symbols = self.additions.symbols_in_file(&file_path, None);
                (file_path, symbols)
            })
            .collect()
    }

    /// Decode and merge one exact name bucket for checkpoint persistence.
    fn symbols_for_exact_name(&self, name: &str) -> Vec<Symbol> {
        let mut symbols = self.base.symbols_for_exact_name(name);
        symbols.retain(|symbol| !self.removed_files.contains(&symbol.file_path));
        symbols.extend(self.additions.lookup(name));
        symbols
    }

    fn len(&self) -> usize {
        let removed_base_names = self.removed_base_names();
        let removed_count = removed_base_names
            .iter()
            .filter(|name| !self.base_name_is_visible(name))
            .count();
        let added_count = self
            .additions
            .symbols
            .iter()
            .filter(|entry| {
                let name = entry.key().as_ref();
                !self.base.contains_exact_name(name)
                    || (removed_base_names.contains(name) && !self.base_name_is_visible(name))
            })
            .count();
        self.base
            .len()
            .saturating_sub(removed_count)
            .saturating_add(added_count)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl InMemorySymbolTable {
    /// Create an empty in-memory symbol table.
    pub fn new() -> Self {
        Self {
            symbols: DashMap::new(),
            symbols_by_file: DashMap::new(),
            sorted_names: std::sync::RwLock::new(Vec::new()),
            sorted_names_dirty: std::sync::atomic::AtomicBool::new(true),
            public_lines_by_file: std::sync::RwLock::new(std::collections::HashMap::new()),
            public_lines_dirty: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Insert a symbol. Appends to the Vec for this name.
    pub fn insert(&self, symbol: Symbol) {
        let file_path = symbol.file_path.clone();
        let mut bucket = self
            .symbols
            .entry(Arc::<str>::from(symbol.name.as_str()))
            // Most repository symbols have a unique name. Vec's default first
            // growth reserves four full Symbol records, which multiplies into
            // hundreds of megabytes on generated/large repositories. Start at
            // one and retain normal geometric growth for actual overloads.
            .or_insert_with(|| Vec::with_capacity(1));
        let name = bucket.key().clone();
        bucket.push(symbol);
        drop(bucket);
        let mut names = self.symbols_by_file.entry(file_path).or_default();
        if let Err(index) =
            names.binary_search_by(|candidate| candidate.as_ref().cmp(name.as_ref()))
        {
            names.insert(index, name);
        }
        // Publish invalidation after the mutation. If a reader rebuilds while
        // the write is in flight it may observe the previous snapshot once,
        // but the post-write flag guarantees the next lookup rebuilds instead
        // of leaving a permanently stale cache.
        self.sorted_names_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.public_lines_dirty
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Remove all symbols originating from a given file path.
    pub fn remove_file(&self, file_path: &str) {
        self.remove_file_symbols(file_path);
        self.sorted_names_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.public_lines_dirty
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Remove one file by visiting only the name buckets recorded for it.
    ///
    /// The returned count is useful for regression tests that guard the
    /// changed-file complexity bound. Name postings are defensive-deduplicated
    /// so tables constructed by older in-process code cannot revisit a bucket.
    fn remove_file_symbols(&self, file_path: &str) -> usize {
        let Some((_, mut names)) = self.symbols_by_file.remove(file_path) else {
            return 0;
        };
        names.sort_unstable_by(|left, right| left.as_ref().cmp(right.as_ref()));
        names.dedup_by(|left, right| left.as_ref() == right.as_ref());
        let visited = names.len();

        for name in names {
            match self.symbols.entry(name) {
                Entry::Occupied(mut entry) => {
                    let empty = {
                        let symbols = entry.get_mut();
                        symbols.retain(|symbol| symbol.file_path != file_path);
                        symbols.is_empty()
                    };
                    if empty {
                        entry.remove();
                    }
                }
                Entry::Vacant(_) => {}
            }
        }
        visited
    }

    /// Exact name lookup.
    pub fn lookup(&self, name: &str) -> Vec<Symbol> {
        self.symbols
            .get(name)
            .map(|r| r.value().to_vec())
            .unwrap_or_default()
    }

    /// Case-insensitive exact-name lookup used to preserve mmap lookup
    /// semantics while changed symbols live in an in-memory overlay.
    fn lookup_case_insensitive(&self, name: &str) -> Vec<Symbol> {
        let name_lower = name.to_lowercase();
        self.lookup_prefix(name)
            .into_iter()
            .filter(|symbol| symbol.name.to_lowercase() == name_lower)
            .collect()
    }

    /// Prefix lookup.
    pub fn lookup_prefix(&self, prefix: &str) -> Vec<Symbol> {
        self.ensure_sorted_names();
        let prefix_lower = prefix.to_lowercase();
        let sorted_names = self
            .sorted_names
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let mut low = 0usize;
        let mut high = sorted_names.len();
        while low < high {
            let mid = low + (high - low) / 2;
            if sorted_names[mid].to_lowercase() < prefix_lower {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        let matching_names: Vec<String> = sorted_names[low..]
            .iter()
            .take_while(|name| name.to_lowercase().starts_with(&prefix_lower))
            .cloned()
            .collect();
        drop(sorted_names);
        let mut results = Vec::new();
        for name in matching_names {
            if let Some(entry) = self.symbols.get(name.as_str()) {
                results.extend(entry.value().to_vec());
            }
        }
        results
    }

    pub fn has_public_symbol_in_range(
        &self,
        file_path: &str,
        line_start: u64,
        line_end: u64,
    ) -> bool {
        if line_start >= line_end {
            return false;
        }
        self.ensure_public_lines();
        let public_lines_by_file = self
            .public_lines_by_file
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let Some(lines) = public_lines_by_file.get(file_path) else {
            return false;
        };
        let index = lines.partition_point(|line| (*line as u64) < line_start);
        lines
            .get(index)
            .is_some_and(|line| (*line as u64) < line_end)
    }

    /// Filter by name pattern and optional file path (case-insensitive).
    pub fn filter(&self, pattern: &str, file: Option<&str>) -> Vec<Symbol> {
        let pattern_lower = pattern.to_lowercase();
        let mut results = Vec::new();
        for entry in self.symbols.iter() {
            if entry.key().to_lowercase().contains(&pattern_lower) {
                for sym in entry.value() {
                    if let Some(f) = file {
                        if sym.file_path.contains(f) {
                            results.push(sym.clone());
                        }
                    } else {
                        results.push(sym.clone());
                    }
                }
            }
        }
        results
    }

    /// Return symbols from one exact file, optionally filtering names by a
    /// case-insensitive substring. Work is proportional to that file's
    /// symbols through a compact secondary index.
    pub fn symbols_in_file(&self, file_path: &str, name_pattern: Option<&str>) -> Vec<Symbol> {
        let pattern_lower = name_pattern.map(str::to_lowercase);
        let Some(file_names) = self.symbols_by_file.get(file_path) else {
            return Vec::new();
        };
        let mut names = file_names.value().clone();
        drop(file_names);
        names.sort_unstable_by(|left, right| left.as_ref().cmp(right.as_ref()));
        names.dedup_by(|left, right| left.as_ref() == right.as_ref());

        let mut symbols = Vec::with_capacity(names.len());
        for name in names {
            if pattern_lower
                .as_ref()
                .is_some_and(|pattern| !name.to_lowercase().contains(pattern))
            {
                continue;
            }
            let Some(bucket) = self.symbols.get(name.as_ref()) else {
                continue;
            };
            symbols.extend(
                bucket
                    .iter()
                    .filter(|symbol| symbol.file_path == file_path)
                    .cloned(),
            );
        }
        symbols
    }

    /// Total number of unique symbol names.
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    /// Returns `true` if the table contains no symbols.
    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    /// All symbols as a flat Vec (for persistence).
    pub fn all_symbols(&self) -> Vec<Symbol> {
        let mut all = Vec::new();
        for entry in self.symbols.iter() {
            all.extend(entry.value().clone());
        }
        all
    }

    /// Visit every symbol without cloning the complete table into a flat Vec.
    pub(crate) fn visit_symbols(&self, mut visitor: impl FnMut(&Symbol)) {
        for entry in self.symbols.iter() {
            for symbol in entry.value() {
                visitor(symbol);
            }
        }
    }

    fn ensure_sorted_names(&self) {
        if !self
            .sorted_names_dirty
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }

        let mut sorted_names = self
            .sorted_names
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if !self
            .sorted_names_dirty
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }

        *sorted_names = self
            .symbols
            .iter()
            .map(|entry| entry.key().to_string())
            .collect();
        sorted_names.sort_by_cached_key(|name| name.to_lowercase());
    }

    fn ensure_public_lines(&self) {
        if !self
            .public_lines_dirty
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return;
        }

        let mut public_lines_by_file = self
            .public_lines_by_file
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if !self
            .public_lines_dirty
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }

        public_lines_by_file.clear();
        for entry in &self.symbols {
            for symbol in entry.value() {
                if symbol.visibility == Visibility::Public {
                    public_lines_by_file
                        .entry(symbol.file_path.clone())
                        .or_default()
                        .push(symbol.line_start);
                }
            }
        }
        for lines in public_lines_by_file.values_mut() {
            lines.sort_unstable();
            lines.dedup();
        }
    }

    /// Build from a flat Vec (after deserialization).
    pub fn from_symbols(symbols: Vec<Symbol>) -> Self {
        let table = Self::new();
        for symbol in symbols {
            table.insert(symbol);
        }
        table
    }
}

impl Default for InMemorySymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Symbol table that is either mutable in-memory (`DashMap`) or read-only
/// memory-mapped (`symbols_v2.bin`).
///
/// All read methods dispatch to the active variant. Mutation methods work on
/// `InMemory` and `Overlay`; callers with a raw mmap table must first call
/// [`SymbolTable::ensure_mutable`].
pub enum SymbolTable {
    /// Mutable variant used during `init` and `sync`.
    InMemory(InMemorySymbolTable),
    /// Read-only memory-mapped variant used during `open`.
    Mmap(mmap::MmapSymbolTable),
    /// Mutable changed-file overlay over an mmap base.
    Overlay(MmapSymbolOverlay),
}

impl SymbolTable {
    /// Create a new empty (in-memory) symbol table.
    pub fn new() -> Self {
        Self::InMemory(InMemorySymbolTable::new())
    }

    /// Insert a symbol into a mutable variant.
    ///
    /// # Panics
    ///
    /// Panics if this is the raw `Mmap` variant. Call [`ensure_mutable`] first.
    pub fn insert(&self, symbol: Symbol) {
        match self {
            Self::InMemory(t) => t.insert(symbol),
            Self::Mmap(_) => panic!("cannot insert into mmap symbol table — call ensure_mutable()"),
            Self::Overlay(t) => t.insert(symbol),
        }
    }

    /// Remove all symbols for a file (requires InMemory variant).
    ///
    /// # Panics
    ///
    /// Panics if this is the `Mmap` variant. Call [`ensure_mutable`] first.
    pub fn remove_file(&self, file_path: &str) {
        match self {
            Self::InMemory(t) => t.remove_file(file_path),
            Self::Mmap(_) => {
                panic!("cannot remove from mmap symbol table — call ensure_mutable()")
            }
            Self::Overlay(t) => t.remove_file(file_path),
        }
    }

    /// Exact name lookup.
    pub fn lookup(&self, name: &str) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.lookup(name),
            Self::Mmap(t) => t.lookup(name),
            Self::Overlay(t) => t.lookup(name),
        }
    }

    /// Prefix lookup.
    pub fn lookup_prefix(&self, prefix: &str) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.lookup_prefix(prefix),
            Self::Mmap(t) => t.lookup_prefix(prefix),
            Self::Overlay(t) => t.lookup_prefix(prefix),
        }
    }

    /// Return whether a public definition starts inside an exact file/range.
    pub fn has_public_symbol_in_range(
        &self,
        file_path: &str,
        line_start: u64,
        line_end: u64,
    ) -> bool {
        match self {
            Self::InMemory(table) => {
                table.has_public_symbol_in_range(file_path, line_start, line_end)
            }
            Self::Mmap(table) => table.has_public_symbol_in_range(file_path, line_start, line_end),
            Self::Overlay(table) => {
                table.has_public_symbol_in_range(file_path, line_start, line_end)
            }
        }
    }

    /// Filter by name pattern and optional file path.
    pub fn filter(&self, pattern: &str, file: Option<&str>) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.filter(pattern, file),
            Self::Mmap(t) => t.filter(pattern, file),
            Self::Overlay(t) => t.filter(pattern, file),
        }
    }

    /// Return symbols from one exact file, optionally filtering symbol names
    /// by a case-insensitive substring.
    pub fn symbols_in_file(&self, file_path: &str, name_pattern: Option<&str>) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.symbols_in_file(file_path, name_pattern),
            Self::Mmap(t) => t.symbols_in_file(file_path, name_pattern),
            Self::Overlay(t) => t.symbols_in_file(file_path, name_pattern),
        }
    }

    /// Whether [`Self::symbols_in_file`] is bounded by the selected file.
    ///
    /// Legacy mmap formats remain queryable through a full-table compatibility
    /// scan, so callers planning repeated file-local lookups should choose a
    /// single global fallback when this returns `false`.
    pub fn supports_exact_file_postings(&self) -> bool {
        match self {
            Self::InMemory(_) => true,
            Self::Mmap(table) => table.supports_exact_file_postings(),
            Self::Overlay(table) => table.supports_exact_file_postings(),
        }
    }

    /// Total number of unique symbol names.
    pub fn len(&self) -> usize {
        match self {
            Self::InMemory(t) => t.len(),
            Self::Mmap(t) => t.len(),
            Self::Overlay(t) => t.len(),
        }
    }

    /// Returns `true` if the table contains no symbols.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::InMemory(t) => t.is_empty(),
            Self::Mmap(t) => t.is_empty(),
            Self::Overlay(t) => t.is_empty(),
        }
    }

    /// All symbols as a flat Vec.
    pub fn all_symbols(&self) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.all_symbols(),
            Self::Mmap(t) => t.all_symbols(),
            Self::Overlay(t) => t.all_symbols(),
        }
    }

    /// Visit every symbol without materializing an additional full-table copy.
    pub(crate) fn visit_symbols(&self, visitor: impl FnMut(&Symbol)) {
        match self {
            Self::InMemory(table) => table.visit_symbols(visitor),
            Self::Mmap(table) => table.visit_symbols(visitor),
            Self::Overlay(table) => table.visit_symbols(visitor),
        }
    }

    /// Exact names used by the streaming mmap checkpoint writer.
    pub(crate) fn checkpoint_names(&self) -> Vec<String> {
        match self {
            Self::InMemory(table) => table
                .symbols
                .iter()
                .map(|entry| entry.key().to_string())
                .collect(),
            Self::Mmap(table) => table.names(),
            Self::Overlay(table) => table.checkpoint_names(),
        }
    }

    /// Decode or clone one exact name bucket for checkpoint persistence.
    pub(crate) fn checkpoint_symbols_for_exact_name(&self, name: &str) -> Vec<Symbol> {
        match self {
            Self::InMemory(table) => table.lookup(name),
            Self::Mmap(table) => table.symbols_for_exact_name(name),
            Self::Overlay(table) => table.symbols_for_exact_name(name),
        }
    }

    /// Build from a flat Vec (creates InMemory variant).
    pub fn from_symbols(symbols: Vec<Symbol>) -> Self {
        Self::InMemory(InMemorySymbolTable::from_symbols(symbols))
    }

    /// Restore a bounded changed-file overlay over an immutable mmap base.
    /// The replacements must already have passed durable-format validation.
    pub(crate) fn from_mmap_with_file_replacements(
        base: mmap::MmapSymbolTable,
        replacements: Vec<persistence::SymbolFileReplacement>,
    ) -> Self {
        if replacements.is_empty() {
            Self::Mmap(base)
        } else {
            Self::Overlay(MmapSymbolOverlay::from_file_replacements(
                base,
                replacements,
            ))
        }
    }

    /// Return the complete persisted overlay when this table has an mmap base.
    /// `None` means the table must be materialized as a new mmap checkpoint.
    pub(crate) fn checkpoint_file_replacements(
        &self,
    ) -> Option<Vec<persistence::SymbolFileReplacement>> {
        match self {
            Self::Mmap(table) => table.preserves_full_fidelity().then(Vec::new),
            Self::Overlay(overlay) => overlay
                .base
                .preserves_full_fidelity()
                .then(|| overlay.checkpoint_file_replacements()),
            Self::InMemory(_) => None,
        }
    }

    /// Convert an `Mmap` variant to a changed-file overlay so mutation works.
    ///
    /// If already mutable, this is a no-op. The mmap base remains mapped and
    /// only later additions/tombstones consume heap memory.
    pub fn ensure_mutable(&mut self) {
        if matches!(self, Self::InMemory(_) | Self::Overlay(_)) {
            return;
        }
        let old = std::mem::replace(self, Self::InMemory(InMemorySymbolTable::new()));
        if let Self::Mmap(base) = old {
            *self = Self::Overlay(MmapSymbolOverlay::new(base));
        }
    }

    /// Returns `true` if this is the in-memory variant.
    pub fn is_in_memory(&self) -> bool {
        matches!(self, Self::InMemory(_) | Self::Overlay(_))
    }

    /// Returns a reference to the inner `InMemorySymbolTable`, if applicable.
    pub fn as_in_memory(&self) -> Option<&InMemorySymbolTable> {
        match self {
            Self::InMemory(t) => Some(t),
            Self::Mmap(_) | Self::Overlay(_) => None,
        }
    }
}

impl Default for SymbolTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_symbol(name: &str, file: &str, kind: EntityKind, lang: Language) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            language: lang,
            file_path: file.to_string(),
            line_start: 0,
            line_end: 10,
            byte_start: 0,
            byte_end: 100,
            signature: Some(format!("fn {}()", name)),
            scope: vec![],
            doc_comment: None,
            visibility: Visibility::default(),
            type_relations: Vec::new(),
        }
    }

    fn downgrade_v4_to_v3(mut bytes: Vec<u8>, public_lines: &[(&str, &[u32])]) -> Vec<u8> {
        let read_u32 = |bytes: &[u8], offset: usize| {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
        };
        let name_count = read_u32(&bytes, 8);
        let exact_file_count_offset = mmap::HEADER_SIZE
            + name_count * mmap::NAME_INDEX_ENTRY_SIZE
            + name_count * mmap::PREFIX_INDEX_ENTRY_SIZE;
        let exact_file_count = read_u32(&bytes, exact_file_count_offset);
        let exact_file_index_offset = exact_file_count_offset + 4;

        let mut entries: Vec<_> = public_lines
            .iter()
            .map(|(path, lines)| {
                let hash = xxhash_rust::xxh3::xxh3_64(path.as_bytes());
                let path_offset = (0..exact_file_count)
                    .find_map(|index| {
                        let offset =
                            exact_file_index_offset + index * mmap::SYMBOL_FILE_INDEX_ENTRY_SIZE;
                        let entry_hash =
                            u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap());
                        (entry_hash == hash).then(|| read_u32(&bytes, offset + 8) as u32)
                    })
                    .expect("public compatibility path must exist in exact-file index");
                let mut lines = lines.to_vec();
                lines.sort_unstable();
                lines.dedup();
                (hash, (*path).to_string(), path_offset, lines)
            })
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        let mut public_index = Vec::new();
        public_index.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut line_data = Vec::new();
        for (hash, _, path_offset, lines) in entries {
            public_index.extend_from_slice(&hash.to_le_bytes());
            public_index.extend_from_slice(&path_offset.to_le_bytes());
            public_index.extend_from_slice(&(line_data.len() as u32).to_le_bytes());
            public_index.extend_from_slice(&(lines.len() as u32).to_le_bytes());
            for line in lines {
                line_data.extend_from_slice(&line.to_le_bytes());
            }
        }

        let exact_and_data = bytes.split_off(exact_file_count_offset);
        bytes.extend_from_slice(&public_index);
        bytes.extend_from_slice(&exact_and_data);
        bytes.extend_from_slice(&line_data);
        bytes[4..8].copy_from_slice(&3u32.to_le_bytes());
        bytes
    }

    fn downgrade_v3_to_v2(mut bytes: Vec<u8>) -> Vec<u8> {
        let read_u32 = |offset: usize| {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
        };
        let name_count = read_u32(8);
        let public_file_count_offset = mmap::HEADER_SIZE
            + name_count * mmap::NAME_INDEX_ENTRY_SIZE
            + name_count * mmap::PREFIX_INDEX_ENTRY_SIZE;
        let public_file_count = read_u32(public_file_count_offset);
        let v2_sizes_offset =
            public_file_count_offset + 4 + public_file_count * mmap::FILE_INDEX_ENTRY_SIZE;
        let symbol_file_count = read_u32(v2_sizes_offset);
        let posting_count_offset =
            v2_sizes_offset + 4 + symbol_file_count * mmap::SYMBOL_FILE_INDEX_ENTRY_SIZE;
        let posting_count = read_u32(posting_count_offset);
        let v3_sizes_offset =
            posting_count_offset + 4 + posting_count * mmap::SYMBOL_FILE_POSTING_SIZE;
        let tail = bytes.split_off(v3_sizes_offset);
        bytes.truncate(v2_sizes_offset);
        bytes.extend_from_slice(&tail);
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        bytes
    }

    #[test]
    fn insert_and_lookup_exact() {
        let table = SymbolTable::new();
        let sym = make_symbol(
            "my_func",
            "src/main.rs",
            EntityKind::Function,
            Language::Rust,
        );
        table.insert(sym.clone());

        let found = table.lookup("my_func");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "my_func");
        assert_eq!(found[0].file_path, "src/main.rs");

        // Looking up a non-existent symbol returns empty
        assert!(table.lookup("nonexistent").is_empty());
    }

    #[test]
    fn first_unique_symbol_bucket_reserves_one_record() {
        let table = InMemorySymbolTable::new();
        table.insert(make_symbol(
            "unique_name",
            "src/unique.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let bucket = table.symbols.get("unique_name").unwrap();
        assert_eq!(bucket.len(), 1);
        assert_eq!(bucket.capacity(), 1);
    }

    #[test]
    fn lookup_prefix_returns_matching() {
        let table = SymbolTable::new();
        table.insert(make_symbol(
            "parse_config",
            "src/config.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "parse_args",
            "src/cli.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "build_index",
            "src/index.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let mut found = table.lookup_prefix("parse_");
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "parse_args");
        assert_eq!(found[1].name, "parse_config");

        // Prefix that matches nothing
        assert!(table.lookup_prefix("zzz_").is_empty());
    }

    #[test]
    fn remove_file_removes_only_target() {
        let table = SymbolTable::new();
        table.insert(make_symbol(
            "foo",
            "src/a.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "foo",
            "src/b.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "bar",
            "src/a.rs",
            EntityKind::Struct,
            Language::Rust,
        ));

        assert_eq!(table.lookup("foo").len(), 2);
        assert_eq!(table.lookup("bar").len(), 1);

        table.remove_file("src/a.rs");

        // "foo" from a.rs is gone, but b.rs remains
        let foo = table.lookup("foo");
        assert_eq!(foo.len(), 1);
        assert_eq!(foo[0].file_path, "src/b.rs");
        assert_eq!(table.symbols_in_file("src/b.rs", None).len(), 1);

        // "bar" was only in a.rs, so it's completely gone
        assert!(table.lookup("bar").is_empty());
        assert_eq!(table.len(), 1); // only "foo" key remains
    }

    #[test]
    fn filter_with_pattern_and_file_constraint() {
        let table = SymbolTable::new();
        table.insert(make_symbol(
            "MyStruct",
            "src/model.rs",
            EntityKind::Struct,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "my_function",
            "src/model.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "my_function",
            "src/other.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "unrelated",
            "src/model.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        // Case-insensitive pattern "my" matches MyStruct and my_function
        let all_my = table.filter("my", None);
        assert_eq!(all_my.len(), 3); // MyStruct + my_function(x2)

        // With file constraint
        let my_in_model = table.filter("my", Some("src/model.rs"));
        assert_eq!(my_in_model.len(), 2); // MyStruct + my_function in model.rs

        // Pattern that matches nothing
        assert!(table.filter("zzz", None).is_empty());
    }

    #[test]
    fn exact_file_symbols_match_in_memory_and_mmap_without_partial_paths() {
        let in_mem = InMemorySymbolTable::new();
        for (name, file) in [
            ("ConfigLoader", "src/config.rs"),
            ("AppConfig", "src/config.rs"),
            ("unrelated", "src/config.rs"),
            ("ConfigLoader", "tests/src/config.rs"),
        ] {
            in_mem.insert(make_symbol(
                name,
                file,
                EntityKind::Function,
                Language::Rust,
            ));
        }

        let collect = |table: &SymbolTable, pattern| {
            let mut symbols: Vec<_> = table
                .symbols_in_file("src/config.rs", pattern)
                .into_iter()
                .map(|symbol| (symbol.name, symbol.file_path))
                .collect();
            symbols.sort();
            symbols
        };
        let expected_all = vec![
            ("AppConfig".to_string(), "src/config.rs".to_string()),
            ("ConfigLoader".to_string(), "src/config.rs".to_string()),
            ("unrelated".to_string(), "src/config.rs".to_string()),
        ];
        let expected_filtered = vec![
            ("AppConfig".to_string(), "src/config.rs".to_string()),
            ("ConfigLoader".to_string(), "src/config.rs".to_string()),
        ];

        let in_memory_table = SymbolTable::InMemory(in_mem);
        assert!(in_memory_table.supports_exact_file_postings());
        assert_eq!(collect(&in_memory_table, None), expected_all);
        assert_eq!(collect(&in_memory_table, Some("CONFIG")), expected_filtered);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(in_memory_table.as_in_memory().unwrap(), &path).unwrap();
        let mmap_table = SymbolTable::Mmap(mmap::MmapSymbolTable::load(&path).unwrap());
        assert!(mmap_table.supports_exact_file_postings());
        assert_eq!(collect(&mmap_table, None), expected_all);
        assert_eq!(collect(&mmap_table, Some("CONFIG")), expected_filtered);
        assert!(mmap_table.symbols_in_file("config.rs", None).is_empty());
    }

    #[test]
    fn exact_file_name_postings_return_each_overload_once() {
        let table = InMemorySymbolTable::new();
        let mut first = make_symbol(
            "overloaded",
            "src/overloads.rs",
            EntityKind::Function,
            Language::Rust,
        );
        first.line_start = 10;
        let mut second = first.clone();
        second.line_start = 20;
        table.insert(first);
        table.insert(second);
        table.insert(make_symbol(
            "overloaded",
            "src/other.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let file_names = table.symbols_by_file.get("src/overloads.rs").unwrap();
        assert_eq!(file_names.len(), 1, "one name posting per file/name pair");
        drop(file_names);

        let mut lines: Vec<_> = table
            .symbols_in_file("src/overloads.rs", Some("LOAD"))
            .into_iter()
            .map(|symbol| symbol.line_start)
            .collect();
        lines.sort_unstable();
        assert_eq!(lines, vec![10, 20]);
    }

    #[test]
    fn targeted_file_removal_visits_only_affected_name_buckets() {
        let table = InMemorySymbolTable::new();
        for index in 0..128 {
            table.insert(make_symbol(
                &format!("unrelated_{index}"),
                "src/keep.rs",
                EntityKind::Function,
                Language::Rust,
            ));
        }
        table.insert(make_symbol(
            "first_target",
            "src/remove.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "second_target",
            "src/remove.rs",
            EntityKind::Struct,
            Language::Rust,
        ));

        assert_eq!(table.remove_file_symbols("src/remove.rs"), 2);
        assert!(table.symbols_in_file("src/remove.rs", None).is_empty());
        assert_eq!(table.symbols_in_file("src/keep.rs", None).len(), 128);
    }

    #[test]
    fn concurrent_file_replacements_leave_unique_complete_postings() {
        use rayon::prelude::*;

        let table = InMemorySymbolTable::new();
        let files: Vec<_> = (0..128)
            .map(|index| format!("src/generated_{index}.rs"))
            .collect();
        for file in &files {
            table.insert(make_symbol(
                "stale",
                file,
                EntityKind::Function,
                Language::Rust,
            ));
        }

        files.par_iter().enumerate().for_each(|(index, file)| {
            table.remove_file(file);
            let mut first = make_symbol(
                "shared_overload",
                file,
                EntityKind::Function,
                Language::Rust,
            );
            first.line_start = index * 10;
            let mut second = first.clone();
            second.line_start += 1;
            table.insert(first);
            table.insert(second);
            table.insert(make_symbol(
                &format!("unique_{index}"),
                file,
                EntityKind::Struct,
                Language::Rust,
            ));
        });

        for (index, file) in files.iter().enumerate() {
            let mut found: Vec<_> = table
                .symbols_in_file(file, None)
                .into_iter()
                .map(|symbol| (symbol.name, symbol.line_start))
                .collect();
            found.sort();
            let mut expected = vec![
                ("shared_overload".to_string(), index * 10),
                ("shared_overload".to_string(), index * 10 + 1),
                (format!("unique_{index}"), 0),
            ];
            expected.sort();
            assert_eq!(found, expected, "incomplete postings for {file}");

            let names = table.symbols_by_file.get(file).unwrap();
            assert_eq!(names.len(), 2, "duplicate name postings for {file}");
        }
    }

    #[test]
    fn roundtrip_bitcode_serialization() {
        let table = SymbolTable::new();
        table.insert(make_symbol(
            "alpha",
            "src/lib.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "Beta",
            "src/types.py",
            EntityKind::Class,
            Language::Python,
        ));
        table.insert(make_symbol(
            "gamma",
            "src/lib.rs",
            EntityKind::Struct,
            Language::Rust,
        ));

        let bytes = persistence::serialize_symbols(&table).expect("serialization should succeed");
        assert!(!bytes.is_empty());

        let restored =
            persistence::deserialize_symbols(&bytes).expect("deserialization should succeed");

        assert_eq!(restored.len(), table.len());

        // Verify individual symbols survived the round-trip
        let alpha = restored.lookup("alpha");
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].file_path, "src/lib.rs");
        assert_eq!(alpha[0].kind, EntityKind::Function);

        let beta = restored.lookup("Beta");
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].language, Language::Python);
        assert_eq!(beta[0].kind, EntityKind::Class);
    }

    #[test]
    fn len_and_is_empty() {
        let table = SymbolTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);

        table.insert(make_symbol(
            "x",
            "a.rs",
            EntityKind::Constant,
            Language::Rust,
        ));
        assert!(!table.is_empty());
        assert_eq!(table.len(), 1);

        // Same name, different file -- still one unique name
        table.insert(make_symbol(
            "x",
            "b.rs",
            EntityKind::Constant,
            Language::Rust,
        ));
        assert_eq!(table.len(), 1);

        table.insert(make_symbol("y", "a.rs", EntityKind::Static, Language::Rust));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn all_symbols_returns_flat_list() {
        let table = SymbolTable::new();
        table.insert(make_symbol(
            "a",
            "x.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol(
            "a",
            "y.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        table.insert(make_symbol("b", "x.rs", EntityKind::Struct, Language::Rust));

        let all = table.all_symbols();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn from_symbols_rebuilds_table() {
        let symbols = vec![
            make_symbol("one", "a.rs", EntityKind::Function, Language::Rust),
            make_symbol("two", "a.rs", EntityKind::Struct, Language::Rust),
            make_symbol("one", "b.rs", EntityKind::Function, Language::Go),
        ];

        let table = SymbolTable::from_symbols(symbols);
        assert_eq!(table.len(), 2); // "one" and "two"
        assert_eq!(table.lookup("one").len(), 2);
        assert_eq!(table.lookup("two").len(), 1);
    }

    #[test]
    fn ensure_mutable_converts_mmap_to_inmemory() {
        // Just test that ensure_mutable on InMemory is a no-op
        let mut table = SymbolTable::new();
        table.insert(make_symbol(
            "x",
            "a.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        assert!(table.is_in_memory());
        table.ensure_mutable();
        assert!(table.is_in_memory());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn mmap_roundtrip() {
        let in_mem = InMemorySymbolTable::new();
        in_mem.insert(make_symbol(
            "alpha",
            "src/lib.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        in_mem.insert(make_symbol(
            "Beta",
            "src/types.py",
            EntityKind::Class,
            Language::Python,
        ));
        in_mem.insert(make_symbol(
            "alpha",
            "src/other.rs",
            EntityKind::Method,
            Language::Go,
        ));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &path).unwrap();

        let loaded = mmap::MmapSymbolTable::load(&path).unwrap();
        let table = SymbolTable::Mmap(loaded);

        // Check counts
        assert_eq!(table.len(), 2); // "alpha" and "Beta"

        // Exact lookup
        let alphas = table.lookup("alpha");
        assert_eq!(alphas.len(), 2);

        let betas = table.lookup("Beta");
        assert_eq!(betas.len(), 1);
        assert_eq!(betas[0].language, Language::Python);
        assert_eq!(betas[0].kind, EntityKind::Class);
        assert_eq!(betas[0].file_path, "src/types.py");

        // Case-insensitive lookup works (hash uses lowercase)
        let betas_lower = table.lookup("beta");
        assert_eq!(betas_lower.len(), 1);

        // Filter
        let a_filter = table.filter("alph", None);
        assert_eq!(a_filter.len(), 2);

        // Filter with file constraint
        let a_in_lib = table.filter("alpha", Some("src/lib.rs"));
        assert_eq!(a_in_lib.len(), 1);
        assert_eq!(a_in_lib[0].file_path, "src/lib.rs");

        // Non-existent lookup
        assert!(table.lookup("nonexistent").is_empty());
    }

    #[test]
    fn mmap_empty_table_roundtrip() {
        let in_mem = InMemorySymbolTable::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &path).unwrap();

        let loaded = mmap::MmapSymbolTable::load(&path).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.len(), 0);
        assert!(loaded.lookup("anything").is_empty());
        assert!(loaded.filter("", None).is_empty());
    }

    #[test]
    fn mmap_preserves_all_symbol_fields() {
        let in_mem = InMemorySymbolTable::new();
        in_mem.insert(Symbol {
            name: "complex_func".to_string(),
            kind: EntityKind::Function,
            language: Language::Rust,
            file_path: "src/engine/mod.rs".to_string(),
            line_start: 42,
            line_end: 100,
            byte_start: 1234,
            byte_end: 5678,
            signature: Some("fn complex_func(x: i32, y: &str) -> Result<()>".to_string()),
            scope: vec!["engine".to_string(), "Engine".to_string()],
            doc_comment: Some("Builds a complete search context.".to_string()),
            visibility: Visibility::Public,
            type_relations: vec![TypeRelation {
                kind: crate::language::TypeRelationKind::Returns,
                target: "SearchContext".to_string(),
            }],
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &path).unwrap();

        let loaded = mmap::MmapSymbolTable::load(&path).unwrap();
        let syms = loaded.lookup("complex_func");
        assert_eq!(syms.len(), 1);
        let s = &syms[0];
        assert_eq!(s.name, "complex_func");
        assert_eq!(s.kind, EntityKind::Function);
        assert_eq!(s.language, Language::Rust);
        assert_eq!(s.file_path, "src/engine/mod.rs");
        assert_eq!(s.line_start, 42);
        assert_eq!(s.line_end, 100);
        assert_eq!(s.byte_start, 1234);
        assert_eq!(s.byte_end, 5678);
        assert_eq!(
            s.signature.as_deref(),
            Some("fn complex_func(x: i32, y: &str) -> Result<()>")
        );
        assert_eq!(s.scope, vec!["engine", "Engine"]);
        assert_eq!(
            s.doc_comment.as_deref(),
            Some("Builds a complete search context.")
        );
        assert_eq!(s.visibility, Visibility::Public);
        assert_eq!(s.type_relations.len(), 1);
        assert_eq!(
            s.type_relations[0].kind,
            crate::language::TypeRelationKind::Returns
        );
        assert_eq!(s.type_relations[0].target, "SearchContext");
    }

    #[test]
    fn mmap_lookup_prefix() {
        let in_mem = InMemorySymbolTable::new();
        in_mem.insert(make_symbol(
            "parse_config",
            "src/config.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        in_mem.insert(make_symbol(
            "parse_args",
            "src/cli.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        in_mem.insert(make_symbol(
            "build_index",
            "src/index.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &path).unwrap();

        let loaded = mmap::MmapSymbolTable::load(&path).unwrap();
        let table = SymbolTable::Mmap(loaded);

        let mut found = table.lookup_prefix("parse_");
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "parse_args");
        assert_eq!(found[1].name, "parse_config");

        assert!(table.lookup_prefix("zzz_").is_empty());
    }

    #[test]
    fn streaming_visit_matches_in_memory_and_mmap_tables() {
        let in_mem = InMemorySymbolTable::new();
        in_mem.insert(make_symbol(
            "parse_config",
            "src/config.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        in_mem.insert(make_symbol(
            "parse_config",
            "tests/config.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        in_mem.insert(make_symbol(
            "build_index",
            "src/index.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &path).unwrap();

        let collect = |table: &SymbolTable| {
            let mut symbols = Vec::new();
            table.visit_symbols(|symbol| {
                symbols.push((symbol.name.clone(), symbol.file_path.clone()));
            });
            symbols.sort();
            symbols
        };

        let expected = collect(&SymbolTable::InMemory(in_mem));
        let mmap = mmap::MmapSymbolTable::load(&path).unwrap();
        assert_eq!(collect(&SymbolTable::Mmap(mmap)), expected);
    }

    #[test]
    fn mmap_no_signature() {
        let in_mem = InMemorySymbolTable::new();
        in_mem.insert(Symbol {
            name: "nosig".to_string(),
            kind: EntityKind::Variable,
            language: Language::Python,
            file_path: "x.py".to_string(),
            line_start: 1,
            line_end: 1,
            byte_start: 0,
            byte_end: 10,
            signature: None,
            scope: vec![],
            doc_comment: None,
            visibility: Visibility::default(),
            type_relations: Vec::new(),
        });

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &path).unwrap();

        let loaded = mmap::MmapSymbolTable::load(&path).unwrap();
        let syms = loaded.lookup("nosig");
        assert_eq!(syms.len(), 1);
        assert!(syms[0].signature.is_none());
    }

    #[test]
    fn mmap_ensure_mutable_uses_a_filtered_overlay() {
        let in_mem = InMemorySymbolTable::new();
        let mut removed_public = make_symbol("x", "a.rs", EntityKind::Function, Language::Rust);
        removed_public.visibility = Visibility::Public;
        in_mem.insert(removed_public);
        in_mem.insert(make_symbol(
            "x",
            "keep.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &path).unwrap();

        let loaded = mmap::MmapSymbolTable::load(&path).unwrap();
        let mut table = SymbolTable::Mmap(loaded);

        assert!(!table.is_in_memory());
        table.ensure_mutable();
        assert!(table.is_in_memory());
        assert_eq!(table.len(), 1);
        assert!(matches!(table, SymbolTable::Overlay(_)));
        assert!(table.supports_exact_file_postings());
        assert!(table.has_public_symbol_in_range("a.rs", 0, 10));
        assert_eq!(table.symbols_in_file("a.rs", None).len(), 1);

        // Mutations stay in the bounded overlay: a base file is tombstoned,
        // unaffected mmap symbols remain visible, and additions are merged.
        table.remove_file("a.rs");
        assert!(!table.has_public_symbol_in_range("a.rs", 0, 10));
        assert!(table.symbols_in_file("a.rs", None).is_empty());
        let mut added_public = make_symbol("y", "b.rs", EntityKind::Struct, Language::Rust);
        added_public.visibility = Visibility::Public;
        table.insert(added_public);
        assert!(table.has_public_symbol_in_range("b.rs", 0, 10));
        assert_eq!(table.len(), 2);
        let x = table.lookup("x");
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].file_path, "keep.rs");
        assert_eq!(table.lookup("y").len(), 1);
        assert_eq!(table.symbols_in_file("b.rs", Some("Y")).len(), 1);
        assert!(table.all_symbols().iter().all(|s| s.file_path != "a.rs"));
    }

    #[test]
    fn mmap_overlay_keeps_case_insensitive_lookup_parity_after_checkpoint() {
        let base = InMemorySymbolTable::new();
        base.insert(make_symbol(
            "unrelated",
            "src/base.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("base.bin");
        let checkpoint_path = dir.path().join("checkpoint.bin");
        writer::write_mmap_symbols(&base, &base_path).unwrap();

        let mut overlay = SymbolTable::Mmap(mmap::MmapSymbolTable::load(&base_path).unwrap());
        overlay.ensure_mutable();
        overlay.insert(make_symbol(
            "Foo",
            "src/upper.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        overlay.insert(make_symbol(
            "foo",
            "src/lower.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let matches = |table: &SymbolTable, query: &str| {
            let mut matches: Vec<_> = table
                .lookup(query)
                .into_iter()
                .map(|symbol| (symbol.name, symbol.file_path))
                .collect();
            matches.sort_unstable();
            matches
        };
        let expected = vec![
            ("Foo".to_string(), "src/upper.rs".to_string()),
            ("foo".to_string(), "src/lower.rs".to_string()),
        ];
        for query in ["foo", "FOO", "fOo"] {
            assert_eq!(matches(&overlay, query), expected);
        }

        writer::write_mmap_symbol_table(&overlay, &checkpoint_path).unwrap();
        let checkpoint = SymbolTable::Mmap(mmap::MmapSymbolTable::load(&checkpoint_path).unwrap());
        for query in ["foo", "FOO", "fOo"] {
            assert_eq!(matches(&checkpoint, query), expected);
            assert_eq!(matches(&checkpoint, query), matches(&overlay, query));
        }
    }

    #[test]
    fn bitcode_preserves_doc_comment_visibility_type_relations() {
        use crate::language::{TypeRelation, TypeRelationKind};

        let table = SymbolTable::new();
        table.insert(Symbol {
            name: "Widget".to_string(),
            kind: EntityKind::Struct,
            language: Language::Rust,
            file_path: "src/widget.rs".to_string(),
            line_start: 10,
            line_end: 50,
            byte_start: 200,
            byte_end: 1200,
            signature: Some("pub struct Widget".to_string()),
            scope: vec!["ui".to_string()],
            doc_comment: Some("A reusable UI widget component.".to_string()),
            visibility: Visibility::Public,
            type_relations: vec![
                TypeRelation {
                    kind: TypeRelationKind::Implements,
                    target: "Display".to_string(),
                },
                TypeRelation {
                    kind: TypeRelationKind::Contains,
                    target: "WidgetState".to_string(),
                },
            ],
        });
        table.insert(Symbol {
            name: "internal_helper".to_string(),
            kind: EntityKind::Function,
            language: Language::Rust,
            file_path: "src/helpers.rs".to_string(),
            line_start: 1,
            line_end: 5,
            byte_start: 0,
            byte_end: 100,
            signature: Some("fn internal_helper()".to_string()),
            scope: vec![],
            doc_comment: None,
            visibility: Visibility::CrateInternal,
            type_relations: vec![],
        });

        let bytes = persistence::serialize_symbols(&table).expect("serialize");
        let restored = persistence::deserialize_symbols(&bytes).expect("deserialize");

        // Verify Widget retains doc_comment, visibility, and type_relations.
        let widgets = restored.lookup("Widget");
        assert_eq!(widgets.len(), 1);
        let w = &widgets[0];
        assert_eq!(
            w.doc_comment.as_deref(),
            Some("A reusable UI widget component.")
        );
        assert_eq!(w.visibility, Visibility::Public);
        assert_eq!(w.type_relations.len(), 2);
        assert_eq!(w.type_relations[0].kind, TypeRelationKind::Implements);
        assert_eq!(w.type_relations[0].target, "Display");
        assert_eq!(w.type_relations[1].kind, TypeRelationKind::Contains);
        assert_eq!(w.type_relations[1].target, "WidgetState");

        // Verify internal_helper retains CrateInternal visibility.
        let helpers = restored.lookup("internal_helper");
        assert_eq!(helpers.len(), 1);
        assert!(helpers[0].doc_comment.is_none());
        assert_eq!(helpers[0].visibility, Visibility::CrateInternal);
        assert!(helpers[0].type_relations.is_empty());
    }

    #[test]
    fn mmap_and_bitcode_preserve_new_fields() {
        use crate::language::{TypeRelation, TypeRelationKind};

        let in_mem = InMemorySymbolTable::new();
        in_mem.insert(Symbol {
            name: "ApiEndpoint".to_string(),
            kind: EntityKind::Function,
            language: Language::Rust,
            file_path: "src/api.rs".to_string(),
            line_start: 5,
            line_end: 20,
            byte_start: 100,
            byte_end: 500,
            signature: Some("pub fn api_endpoint()".to_string()),
            scope: vec![],
            doc_comment: Some("Handles /api/v1/users.".to_string()),
            visibility: Visibility::Public,
            type_relations: vec![TypeRelation {
                kind: TypeRelationKind::Returns,
                target: "Response".to_string(),
            }],
        });

        let dir = tempfile::tempdir().unwrap();

        // Write and load via mmap — all fields remain queryable in place.
        let mmap_path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&in_mem, &mmap_path).unwrap();
        let mmap_table = mmap::MmapSymbolTable::load(&mmap_path).unwrap();
        let mmap_syms = mmap_table.lookup("ApiEndpoint");
        assert_eq!(mmap_syms.len(), 1);
        assert_eq!(
            mmap_syms[0].doc_comment.as_deref(),
            Some("Handles /api/v1/users.")
        );
        assert_eq!(mmap_syms[0].visibility, Visibility::Public);
        assert_eq!(mmap_syms[0].type_relations.len(), 1);
        assert_eq!(
            mmap_syms[0].type_relations[0].kind,
            TypeRelationKind::Returns
        );

        // Write and load via bitcode — all fields preserved.
        let bitcode_table =
            SymbolTable::InMemory(InMemorySymbolTable::from_symbols(in_mem.all_symbols()));
        let bytes = persistence::serialize_symbols(&bitcode_table).unwrap();
        let restored = persistence::deserialize_symbols(&bytes).unwrap();
        let bc_syms = restored.lookup("ApiEndpoint");
        assert_eq!(bc_syms.len(), 1);
        assert_eq!(
            bc_syms[0].doc_comment.as_deref(),
            Some("Handles /api/v1/users.")
        );
        assert_eq!(bc_syms[0].visibility, Visibility::Public);
        assert_eq!(bc_syms[0].type_relations.len(), 1);
        assert_eq!(bc_syms[0].type_relations[0].kind, TypeRelationKind::Returns);
    }

    #[test]
    fn secondary_indexes_match_in_memory_and_mmap() {
        let in_mem = InMemorySymbolTable::new();
        for (name, file, line, visibility) in [
            ("ParseConfig", "src/config.rs", 10, Visibility::Public),
            ("PrivateConfig", "src/config.rs", 12, Visibility::Private),
            ("parse_args", "src/args.rs", 20, Visibility::Private),
            ("parserState", "src/parser.rs", 30, Visibility::Public),
            ("build_index", "src/index.rs", 40, Visibility::Public),
        ] {
            let mut symbol = make_symbol(name, file, EntityKind::Function, Language::Rust);
            symbol.line_start = line;
            symbol.line_end = line + 1;
            symbol.visibility = visibility;
            if name == "ParseConfig" {
                symbol.scope = vec!["config".to_string(), "parser".to_string()];
            }
            in_mem.insert(symbol);
        }

        let prefixed_names = |table: &SymbolTable| {
            let mut names: Vec<_> = table
                .lookup_prefix("PAR")
                .into_iter()
                .map(|symbol| symbol.name)
                .collect();
            names.sort();
            names
        };
        let expected = vec![
            "ParseConfig".to_string(),
            "parse_args".to_string(),
            "parserState".to_string(),
        ];
        let in_memory_table = SymbolTable::InMemory(in_mem);
        assert_eq!(prefixed_names(&in_memory_table), expected);
        assert!(in_memory_table.has_public_symbol_in_range("src/config.rs", 10, 11));
        assert!(!in_memory_table.has_public_symbol_in_range("src/config.rs", 11, 20));
        assert!(!in_memory_table.has_public_symbol_in_range("src/config.rs", 12, 13));
        assert!(!in_memory_table.has_public_symbol_in_range("src/args.rs", 20, 21));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(in_memory_table.as_in_memory().unwrap(), &path).unwrap();
        let mmap_table = SymbolTable::Mmap(mmap::MmapSymbolTable::load(&path).unwrap());
        assert_eq!(prefixed_names(&mmap_table), expected);
        assert!(mmap_table.has_public_symbol_in_range("src/config.rs", 10, 11));
        assert!(!mmap_table.has_public_symbol_in_range("src/config.rs", 11, 20));
        assert!(!mmap_table.has_public_symbol_in_range("src/config.rs", 12, 13));
        assert!(!mmap_table.has_public_symbol_in_range("src/args.rs", 20, 21));
        assert!(!mmap_table.has_public_symbol_in_range("src/missing.rs", 0, 100));
    }

    #[test]
    fn mmap_v2_supports_more_than_u16_definitions_per_name() {
        let table = InMemorySymbolTable::new();
        let mut symbol = make_symbol("main", "generated.rs", EntityKind::Function, Language::Rust);
        for line in 0..=u16::MAX as usize {
            symbol.line_start = line;
            symbol.line_end = line + 1;
            table.insert(symbol.clone());
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&table, &path).unwrap();
        let mmap = mmap::MmapSymbolTable::load(&path).unwrap();
        assert_eq!(mmap.lookup("main").len(), u16::MAX as usize + 1);
    }

    #[test]
    fn in_memory_secondary_indexes_invalidate_after_mutation() {
        let table = InMemorySymbolTable::new();
        assert!(table.lookup_prefix("new").is_empty());
        assert!(!table.has_public_symbol_in_range("new.rs", 5, 6));

        let mut symbol = make_symbol(
            "new_public_api",
            "new.rs",
            EntityKind::Function,
            Language::Rust,
        );
        symbol.line_start = 5;
        symbol.visibility = Visibility::Public;
        table.insert(symbol);

        assert_eq!(table.lookup_prefix("NEW").len(), 1);
        assert!(table.has_public_symbol_in_range("new.rs", 5, 6));
    }

    #[test]
    fn mmap_reads_legacy_v1_symbols() {
        let mut pool = vec![0u8, 0u8];
        let (name_offset, file_offset) = {
            let mut intern = |value: &str| {
                let offset = pool.len() as u32;
                pool.extend_from_slice(&(value.len() as u16).to_le_bytes());
                pool.extend_from_slice(value.as_bytes());
                offset
            };
            (intern("LegacyApi"), intern("legacy.rs"))
        };
        let name = "LegacyApi";

        let mut symbol_data = Vec::new();
        symbol_data.extend_from_slice(&1u16.to_le_bytes());
        symbol_data.push(mmap::entity_kind_to_u8(&EntityKind::Function));
        symbol_data.push(mmap::language_to_u8(Language::Rust));
        symbol_data.extend_from_slice(&file_offset.to_le_bytes());
        symbol_data.extend_from_slice(&3u32.to_le_bytes());
        symbol_data.extend_from_slice(&4u32.to_le_bytes());
        symbol_data.extend_from_slice(&10u32.to_le_bytes());
        symbol_data.extend_from_slice(&20u32.to_le_bytes());
        symbol_data.extend_from_slice(&0u32.to_le_bytes());
        symbol_data.push(0);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&mmap::MAGIC.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(
            &xxhash_rust::xxh3::xxh3_64(name.to_lowercase().as_bytes()).to_le_bytes(),
        );
        bytes.extend_from_slice(&name_offset.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(pool.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&pool);
        bytes.extend_from_slice(&symbol_data);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v1.bin");
        std::fs::write(&path, bytes).unwrap();
        let table = mmap::MmapSymbolTable::load(&path).unwrap();
        assert!(!table.preserves_full_fidelity());
        assert!(!table.supports_exact_file_postings());
        let symbols = table.lookup(name);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].file_path, "legacy.rs");
        assert_eq!(symbols[0].line_start, 3);
        assert_eq!(symbols[0].visibility, Visibility::Private);
        assert!(!table.has_public_symbol_in_range("legacy.rs", 3, 4));
        assert_eq!(table.symbols_in_file("legacy.rs", Some("API")).len(), 1);
    }

    #[test]
    fn mmap_reads_full_fidelity_v2_symbols_without_v3_postings() {
        let table = InMemorySymbolTable::new();
        table.insert(Symbol {
            name: "CompatApi".to_string(),
            kind: EntityKind::Function,
            language: Language::Rust,
            file_path: "compat.rs".to_string(),
            line_start: 7,
            line_end: 9,
            byte_start: 20,
            byte_end: 80,
            signature: Some("pub fn compat_api()".to_string()),
            scope: vec!["compat".to_string()],
            doc_comment: Some("Version two compatibility.".to_string()),
            visibility: Visibility::Public,
            type_relations: vec![TypeRelation {
                kind: crate::language::TypeRelationKind::Returns,
                target: "Response".to_string(),
            }],
        });

        let dir = tempfile::tempdir().unwrap();
        let v4_path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &v4_path).unwrap();
        let v3_bytes =
            downgrade_v4_to_v3(std::fs::read(&v4_path).unwrap(), &[("compat.rs", &[7u32])]);
        let v2_bytes = downgrade_v3_to_v2(v3_bytes);
        let v2_path = dir.path().join("symbols_v2.bin");
        std::fs::write(&v2_path, v2_bytes).unwrap();

        let table = mmap::MmapSymbolTable::load(&v2_path).unwrap();
        assert!(table.preserves_full_fidelity());
        assert!(!table.supports_exact_file_postings());
        let symbols = table.symbols_in_file("compat.rs", Some("API"));
        assert_eq!(symbols.len(), 1);
        assert_eq!(
            symbols[0].doc_comment.as_deref(),
            Some("Version two compatibility.")
        );
        assert_eq!(symbols[0].visibility, Visibility::Public);
        assert_eq!(symbols[0].type_relations.len(), 1);
        assert!(table.has_public_symbol_in_range("compat.rs", 7, 8));
    }

    #[test]
    fn mmap_reads_v3_public_index_and_exact_file_postings() {
        let table = InMemorySymbolTable::new();
        let mut public = make_symbol(
            "PublicCompat",
            "src/compat.rs",
            EntityKind::Function,
            Language::Rust,
        );
        public.line_start = 42;
        public.line_end = 43;
        public.visibility = Visibility::Public;
        table.insert(public);
        table.insert(make_symbol(
            "PrivateCompat",
            "src/compat.rs",
            EntityKind::Function,
            Language::Rust,
        ));

        let dir = tempfile::tempdir().unwrap();
        let v4_path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &v4_path).unwrap();
        let v3_bytes = downgrade_v4_to_v3(
            std::fs::read(v4_path).unwrap(),
            &[("src/compat.rs", &[42u32])],
        );
        let v3_path = dir.path().join("symbols_v3.bin");
        std::fs::write(&v3_path, v3_bytes).unwrap();

        let mmap = mmap::MmapSymbolTable::load(&v3_path).unwrap();
        assert!(mmap.preserves_full_fidelity());
        assert!(mmap.supports_exact_file_postings());
        assert_eq!(mmap.symbols_in_file("src/compat.rs", None).len(), 2);
        assert!(mmap.has_public_symbol_in_range("src/compat.rs", 42, 43));
        assert!(!mmap.has_public_symbol_in_range("src/compat.rs", 43, 44));
    }

    #[test]
    fn mmap_v4_output_is_deterministic_across_insertion_orders() {
        let symbols = vec![
            make_symbol("same", "src/b.rs", EntityKind::Method, Language::Go),
            make_symbol("Alpha", "src/a.rs", EntityKind::Struct, Language::Rust),
            make_symbol("same", "src/a.rs", EntityKind::Function, Language::Rust),
        ];
        let first = InMemorySymbolTable::new();
        for symbol in &symbols {
            first.insert(symbol.clone());
        }
        let second = InMemorySymbolTable::new();
        for symbol in symbols.iter().rev() {
            second.insert(symbol.clone());
        }

        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("first.bin");
        let second_path = dir.path().join("second.bin");
        writer::write_mmap_symbols(&first, &first_path).unwrap();
        writer::write_mmap_symbols(&second, &second_path).unwrap();
        let first_bytes = std::fs::read(first_path).unwrap();
        let second_bytes = std::fs::read(second_path).unwrap();
        assert_eq!(
            u32::from_le_bytes(first_bytes[4..8].try_into().unwrap()),
            mmap::FORMAT_VERSION
        );
        assert_eq!(first_bytes, second_bytes);
    }

    #[test]
    fn mmap_v4_removes_public_index_bytes_and_projects_over_three_mb_at_100k_files() {
        const FILES: usize = 1_000;
        const PUBLIC_SYMBOLS_PER_FILE: usize = 3;
        let table = InMemorySymbolTable::new();
        let mut public_lines = Vec::with_capacity(FILES);
        for file_index in 0..FILES {
            let path = format!("src/generated/file_{file_index:05}.rs");
            let mut lines = Vec::with_capacity(PUBLIC_SYMBOLS_PER_FILE);
            for symbol_index in 0..PUBLIC_SYMBOLS_PER_FILE {
                let line = symbol_index + 1;
                let mut symbol = make_symbol(
                    &format!("public_{file_index:05}_{symbol_index}"),
                    &path,
                    EntityKind::Function,
                    Language::Rust,
                );
                symbol.line_start = line;
                symbol.line_end = line + 1;
                symbol.visibility = Visibility::Public;
                table.insert(symbol);
                lines.push(line as u32);
            }
            public_lines.push((path, lines));
        }

        let dir = tempfile::tempdir().unwrap();
        let v4_path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &v4_path).unwrap();
        let v4_bytes = std::fs::read(v4_path).unwrap();
        let public_line_refs: Vec<_> = public_lines
            .iter()
            .map(|(path, lines)| (path.as_str(), lines.as_slice()))
            .collect();
        let v3_bytes = downgrade_v4_to_v3(v4_bytes.clone(), &public_line_refs);

        let expected_saving = 4
            + FILES * mmap::FILE_INDEX_ENTRY_SIZE
            + FILES * PUBLIC_SYMBOLS_PER_FILE * std::mem::size_of::<u32>();
        assert_eq!(v3_bytes.len() - v4_bytes.len(), expected_saving);
        let projected_100k_saving = 4
            + 100_000 * mmap::FILE_INDEX_ENTRY_SIZE
            + 100_000 * PUBLIC_SYMBOLS_PER_FILE * std::mem::size_of::<u32>();
        assert!(projected_100k_saving > 3_000_000);
    }

    #[test]
    fn malformed_mmap_symbol_offsets_are_rejected_on_load() {
        let table = InMemorySymbolTable::new();
        table.insert(make_symbol(
            "malformed",
            "bad.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&table, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        bytes[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(mmap::MmapSymbolTable::load(&path).is_err());
    }

    #[test]
    fn malformed_file_posting_offsets_are_rejected_on_load() {
        let table = InMemorySymbolTable::new();
        table.insert(make_symbol(
            "malformed",
            "bad.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let read_u32 = |bytes: &[u8], offset: usize| {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
        };
        let name_count = read_u32(&bytes, 8);
        let symbol_file_count_offset = mmap::HEADER_SIZE
            + name_count * mmap::NAME_INDEX_ENTRY_SIZE
            + name_count * mmap::PREFIX_INDEX_ENTRY_SIZE;
        let symbol_file_index_offset = symbol_file_count_offset + 4;
        bytes[symbol_file_index_offset + 12..symbol_file_index_offset + 16]
            .copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        assert!(mmap::MmapSymbolTable::load(&path).is_err());
    }

    #[test]
    fn malformed_mmap_prefix_slot_is_rejected_on_load() {
        let table = InMemorySymbolTable::new();
        table.insert(make_symbol(
            "prefix_target",
            "prefix.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &path).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let prefix_offset = mmap::HEADER_SIZE + mmap::NAME_INDEX_ENTRY_SIZE;
        bytes[prefix_offset..prefix_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        assert!(mmap::MmapSymbolTable::load(&path).is_err());
    }

    #[test]
    fn malformed_mmap_string_boundary_and_name_hash_are_rejected_on_load() {
        let table = InMemorySymbolTable::new();
        table.insert(make_symbol(
            "string_target",
            "string.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &path).unwrap();
        let original = std::fs::read(&path).unwrap();

        let mut bad_boundary = original.clone();
        bad_boundary[mmap::HEADER_SIZE + 8..mmap::HEADER_SIZE + 12]
            .copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, bad_boundary).unwrap();
        assert!(mmap::MmapSymbolTable::load(&path).is_err());

        let mut bad_hash = original;
        bad_hash[mmap::HEADER_SIZE] ^= 1;
        std::fs::write(&path, bad_hash).unwrap();
        assert!(mmap::MmapSymbolTable::load(&path).is_err());
    }

    #[test]
    fn malformed_v4_header_symbol_count_and_trailing_bytes_are_rejected_on_load() {
        let table = InMemorySymbolTable::new();
        let mut symbol = make_symbol(
            "public_target",
            "public.rs",
            EntityKind::Function,
            Language::Rust,
        );
        symbol.visibility = Visibility::Public;
        table.insert(symbol);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &path).unwrap();
        let original = std::fs::read(&path).unwrap();

        let mut bad_count = original.clone();
        bad_count[12..16].copy_from_slice(&2u32.to_le_bytes());
        std::fs::write(&path, bad_count).unwrap();
        assert!(mmap::MmapSymbolTable::load(&path).is_err());

        let mut impossible_count = original.clone();
        impossible_count[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, impossible_count).unwrap();
        assert!(mmap::MmapSymbolTable::load(&path).is_err());

        let mut trailing_bytes = original;
        trailing_bytes.push(0);
        std::fs::write(&path, trailing_bytes).unwrap();
        assert!(mmap::MmapSymbolTable::load(&path).is_err());
    }

    #[test]
    fn malformed_v3_public_line_ranges_are_rejected_on_load() {
        let table = InMemorySymbolTable::new();
        let mut symbol = make_symbol(
            "public_target",
            "public.rs",
            EntityKind::Function,
            Language::Rust,
        );
        symbol.visibility = Visibility::Public;
        table.insert(symbol);
        let dir = tempfile::tempdir().unwrap();
        let v4_path = dir.path().join("symbols_v4.bin");
        writer::write_mmap_symbols(&table, &v4_path).unwrap();
        let mut bad_lines =
            downgrade_v4_to_v3(std::fs::read(v4_path).unwrap(), &[("public.rs", &[0u32])]);
        let public_file_count_offset =
            mmap::HEADER_SIZE + mmap::NAME_INDEX_ENTRY_SIZE + mmap::PREFIX_INDEX_ENTRY_SIZE;
        let public_file_index_offset = public_file_count_offset + 4;
        bad_lines[public_file_index_offset + 12..public_file_index_offset + 16]
            .copy_from_slice(&1u32.to_le_bytes());
        let v3_path = dir.path().join("symbols_v3.bin");
        std::fs::write(&v3_path, bad_lines).unwrap();
        assert!(mmap::MmapSymbolTable::load(&v3_path).is_err());
    }

    #[test]
    fn symbol_delta_round_trip_restores_replacements_deletions_and_full_fidelity() {
        use crate::language::{TypeRelation, TypeRelationKind};

        let base = InMemorySymbolTable::new();
        base.insert(make_symbol(
            "shared",
            "src/changed.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        base.insert(make_symbol(
            "old_only",
            "src/changed.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        base.insert(make_symbol(
            "shared",
            "src/keep.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        base.insert(make_symbol(
            "deleted",
            "src/deleted.rs",
            EntityKind::Struct,
            Language::Rust,
        ));

        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(&base, &base_path).unwrap();
        let mut live = SymbolTable::Mmap(mmap::MmapSymbolTable::load(&base_path).unwrap());
        live.ensure_mutable();
        live.remove_file("src/changed.rs");

        let mut replacement = make_symbol(
            "shared",
            "src/changed.rs",
            EntityKind::Method,
            Language::Rust,
        );
        replacement.line_start = 41;
        replacement.line_end = 47;
        replacement.byte_start = 410;
        replacement.byte_end = 470;
        replacement.signature = Some("pub fn shared(&self) -> Widget".to_string());
        replacement.scope = vec!["Container".to_string(), "impl Widget".to_string()];
        replacement.doc_comment = Some("Replacement docs".to_string());
        replacement.visibility = Visibility::Public;
        replacement.type_relations = vec![TypeRelation {
            kind: TypeRelationKind::Returns,
            target: "Widget".to_string(),
        }];
        live.insert(replacement);
        live.insert(make_symbol(
            "added",
            "src/changed.rs",
            EntityKind::Function,
            Language::Rust,
        ));
        live.remove_file("src/deleted.rs");
        live.remove_file("src/new.rs");
        live.insert(make_symbol(
            "brand_new",
            "src/new.rs",
            EntityKind::Struct,
            Language::Rust,
        ));

        let replacements = live.checkpoint_file_replacements().unwrap();
        assert_eq!(
            replacements
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            vec!["src/changed.rs", "src/deleted.rs", "src/new.rs"]
        );
        assert!(replacements[1].1.is_empty(), "deletion tombstone lost");

        let bytes = persistence::serialize_symbol_delta(&replacements).unwrap();
        let decoded = persistence::deserialize_symbol_delta(&bytes).unwrap();
        let restored = SymbolTable::from_mmap_with_file_replacements(
            mmap::MmapSymbolTable::load(&base_path).unwrap(),
            decoded,
        );

        assert!(restored.lookup("old_only").is_empty());
        assert!(restored.lookup("deleted").is_empty());
        assert_eq!(restored.lookup_prefix("brand_").len(), 1);
        assert_eq!(restored.filter("added", Some("changed.rs")).len(), 1);
        assert_eq!(restored.symbols_in_file("src/changed.rs", None).len(), 2);
        let shared = restored.lookup("shared");
        assert_eq!(shared.len(), 2, "unaffected overload must remain visible");
        let changed = shared
            .iter()
            .find(|symbol| symbol.file_path == "src/changed.rs")
            .unwrap();
        assert_eq!(changed.kind, EntityKind::Method);
        assert_eq!(changed.line_start, 41);
        assert_eq!(changed.line_end, 47);
        assert_eq!(changed.byte_start, 410);
        assert_eq!(changed.byte_end, 470);
        assert_eq!(
            changed.signature.as_deref(),
            Some("pub fn shared(&self) -> Widget")
        );
        assert_eq!(changed.scope, ["Container", "impl Widget"]);
        assert_eq!(changed.doc_comment.as_deref(), Some("Replacement docs"));
        assert_eq!(changed.visibility, Visibility::Public);
        assert_eq!(changed.type_relations.len(), 1);
        assert_eq!(changed.type_relations[0].kind, TypeRelationKind::Returns);
        assert_eq!(changed.type_relations[0].target, "Widget");

        let reserialized =
            persistence::serialize_symbol_delta(&restored.checkpoint_file_replacements().unwrap())
                .unwrap();
        assert_eq!(bytes, reserialized, "symbol delta must be canonical");
    }

    #[test]
    fn symbol_delta_rejects_duplicate_mismatched_and_malformed_entries() {
        let symbol = make_symbol(
            "target",
            "src/right.rs",
            EntityKind::Function,
            Language::Rust,
        );
        assert!(
            persistence::serialize_symbol_delta(&[("src/wrong.rs".to_string(), vec![symbol])])
                .is_err()
        );
        assert!(
            persistence::serialize_symbol_delta(&[
                ("src/duplicate.rs".to_string(), Vec::new()),
                ("src/duplicate.rs".to_string(), Vec::new()),
            ])
            .is_err()
        );
        assert!(persistence::deserialize_symbol_delta(b"not-a-symbol-delta").is_err());
    }
}
