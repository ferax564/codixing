pub mod mmap;
pub mod persistence;
pub mod writer;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

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
    pub(crate) symbols: DashMap<String, Vec<Symbol>>,
    sorted_names: std::sync::RwLock<Vec<String>>,
    sorted_names_dirty: std::sync::atomic::AtomicBool,
    public_lines_by_file: std::sync::RwLock<std::collections::HashMap<String, Vec<usize>>>,
    public_lines_dirty: std::sync::atomic::AtomicBool,
}

impl InMemorySymbolTable {
    /// Create an empty in-memory symbol table.
    pub fn new() -> Self {
        Self {
            symbols: DashMap::new(),
            sorted_names: std::sync::RwLock::new(Vec::new()),
            sorted_names_dirty: std::sync::atomic::AtomicBool::new(true),
            public_lines_by_file: std::sync::RwLock::new(std::collections::HashMap::new()),
            public_lines_dirty: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Insert a symbol. Appends to the Vec for this name.
    pub fn insert(&self, symbol: Symbol) {
        self.symbols
            .entry(symbol.name.clone())
            .or_default()
            .push(symbol);
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
        let keys: Vec<String> = self.symbols.iter().map(|r| r.key().clone()).collect();
        for key in keys {
            self.symbols.entry(key).and_modify(|syms| {
                syms.retain(|s| s.file_path != file_path);
            });
        }
        self.symbols.retain(|_, v| !v.is_empty());
        self.sorted_names_dirty
            .store(true, std::sync::atomic::Ordering::Release);
        self.public_lines_dirty
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Exact name lookup.
    pub fn lookup(&self, name: &str) -> Vec<Symbol> {
        self.symbols
            .get(name)
            .map(|r| r.value().clone())
            .unwrap_or_default()
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
            if let Some(entry) = self.symbols.get(&name) {
                results.extend(entry.value().clone());
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
            .map(|entry| entry.key().clone())
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
/// All read methods dispatch to the active variant. Mutation methods
/// (`insert`, `remove_file`) require the `InMemory` variant and will
/// panic if called on the `Mmap` variant (callers must convert first
/// via [`SymbolTable::ensure_mutable`]).
pub enum SymbolTable {
    /// Mutable variant used during `init` and `sync`.
    InMemory(InMemorySymbolTable),
    /// Read-only memory-mapped variant used during `open`.
    Mmap(mmap::MmapSymbolTable),
}

impl SymbolTable {
    /// Create a new empty (in-memory) symbol table.
    pub fn new() -> Self {
        Self::InMemory(InMemorySymbolTable::new())
    }

    /// Insert a symbol (requires InMemory variant).
    ///
    /// # Panics
    ///
    /// Panics if this is the `Mmap` variant. Call [`ensure_mutable`] first.
    pub fn insert(&self, symbol: Symbol) {
        match self {
            Self::InMemory(t) => t.insert(symbol),
            Self::Mmap(_) => panic!("cannot insert into mmap symbol table — call ensure_mutable()"),
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
        }
    }

    /// Exact name lookup.
    pub fn lookup(&self, name: &str) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.lookup(name),
            Self::Mmap(t) => t.lookup(name),
        }
    }

    /// Prefix lookup.
    pub fn lookup_prefix(&self, prefix: &str) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.lookup_prefix(prefix),
            Self::Mmap(t) => t.lookup_prefix(prefix),
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
        }
    }

    /// Filter by name pattern and optional file path.
    pub fn filter(&self, pattern: &str, file: Option<&str>) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.filter(pattern, file),
            Self::Mmap(t) => t.filter(pattern, file),
        }
    }

    /// Total number of unique symbol names.
    pub fn len(&self) -> usize {
        match self {
            Self::InMemory(t) => t.len(),
            Self::Mmap(t) => t.len(),
        }
    }

    /// Returns `true` if the table contains no symbols.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::InMemory(t) => t.is_empty(),
            Self::Mmap(t) => t.is_empty(),
        }
    }

    /// All symbols as a flat Vec.
    pub fn all_symbols(&self) -> Vec<Symbol> {
        match self {
            Self::InMemory(t) => t.all_symbols(),
            Self::Mmap(t) => t.all_symbols(),
        }
    }

    /// Visit every symbol without materializing an additional full-table copy.
    pub(crate) fn visit_symbols(&self, visitor: impl FnMut(&Symbol)) {
        match self {
            Self::InMemory(table) => table.visit_symbols(visitor),
            Self::Mmap(table) => table.visit_symbols(visitor),
        }
    }

    /// Build from a flat Vec (creates InMemory variant).
    pub fn from_symbols(symbols: Vec<Symbol>) -> Self {
        Self::InMemory(InMemorySymbolTable::from_symbols(symbols))
    }

    /// Convert an `Mmap` variant to `InMemory` so mutation methods work.
    ///
    /// If already `InMemory`, this is a no-op. If `Mmap`, reads all symbols
    /// from the mmap and rebuilds the `DashMap`.
    pub fn ensure_mutable(&mut self) {
        if matches!(self, Self::InMemory(_)) {
            return;
        }
        let all = self.all_symbols();
        *self = Self::from_symbols(all);
    }

    /// Returns `true` if this is the in-memory variant.
    pub fn is_in_memory(&self) -> bool {
        matches!(self, Self::InMemory(_))
    }

    /// Returns a reference to the inner `InMemorySymbolTable`, if applicable.
    pub fn as_in_memory(&self) -> Option<&InMemorySymbolTable> {
        match self {
            Self::InMemory(t) => Some(t),
            Self::Mmap(_) => None,
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
    fn mmap_ensure_mutable_converts() {
        let in_mem = InMemorySymbolTable::new();
        in_mem.insert(make_symbol(
            "x",
            "a.rs",
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

        // Now we can mutate
        table.insert(make_symbol("y", "b.rs", EntityKind::Struct, Language::Rust));
        assert_eq!(table.len(), 2);
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
            ("parse_args", "src/args.rs", 20, Visibility::Private),
            ("parserState", "src/parser.rs", 30, Visibility::Public),
            ("build_index", "src/index.rs", 40, Visibility::Public),
        ] {
            let mut symbol = make_symbol(name, file, EntityKind::Function, Language::Rust);
            symbol.line_start = line;
            symbol.line_end = line + 1;
            symbol.visibility = visibility;
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
        assert!(!in_memory_table.has_public_symbol_in_range("src/args.rs", 20, 21));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("symbols_v2.bin");
        writer::write_mmap_symbols(in_memory_table.as_in_memory().unwrap(), &path).unwrap();
        let mmap_table = SymbolTable::Mmap(mmap::MmapSymbolTable::load(&path).unwrap());
        assert_eq!(prefixed_names(&mmap_table), expected);
        assert!(mmap_table.has_public_symbol_in_range("src/config.rs", 10, 11));
        assert!(!mmap_table.has_public_symbol_in_range("src/config.rs", 11, 20));
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
        let mut intern = |value: &str| {
            let offset = pool.len() as u32;
            pool.extend_from_slice(&(value.len() as u16).to_le_bytes());
            pool.extend_from_slice(value.as_bytes());
            offset
        };
        let name = "LegacyApi";
        let name_offset = intern(name);
        let file_offset = intern("legacy.rs");
        drop(intern);

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
        let symbols = table.lookup(name);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].file_path, "legacy.rs");
        assert_eq!(symbols[0].line_start, 3);
        assert_eq!(symbols[0].visibility, Visibility::Private);
        assert!(!table.has_public_symbol_in_range("legacy.rs", 3, 4));
    }

    #[test]
    fn malformed_mmap_offsets_do_not_panic_during_lookup() {
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
        let mmap = mmap::MmapSymbolTable::load(&path).unwrap();
        assert!(mmap.lookup("malformed").is_empty());
    }
}
