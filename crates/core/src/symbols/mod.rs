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
}

impl InMemorySymbolTable {
    /// Create an empty in-memory symbol table.
    pub fn new() -> Self {
        Self {
            symbols: DashMap::new(),
        }
    }

    /// Insert a symbol. Appends to the Vec for this name.
    pub fn insert(&self, symbol: Symbol) {
        self.symbols
            .entry(symbol.name.clone())
            .or_default()
            .push(symbol);
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
        let mut results = Vec::new();
        for entry in self.symbols.iter() {
            if entry.key().starts_with(prefix) {
                results.extend(entry.value().clone());
            }
        }
        results
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
            doc_comment: None,
            visibility: Visibility::default(),
            type_relations: Vec::new(),
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
}
