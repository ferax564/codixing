pub mod persistence;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::language::{EntityKind, Language};

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
}

/// Concurrent symbol table backed by `DashMap`.
///
/// Keys are symbol names; values are vectors of all symbols sharing that name
/// (there may be multiple definitions across files).
pub struct SymbolTable {
    symbols: DashMap<String, Vec<Symbol>>,
}

impl SymbolTable {
    /// Create an empty symbol table.
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
    ///
    /// Iterates every entry and filters out symbols matching `file_path`.
    /// Entries left with an empty Vec are removed entirely.
    pub fn remove_file(&self, file_path: &str) {
        // Collect keys to avoid holding shard locks during mutation
        let keys: Vec<String> = self.symbols.iter().map(|r| r.key().clone()).collect();
        for key in keys {
            self.symbols.entry(key).and_modify(|syms| {
                syms.retain(|s| s.file_path != file_path);
            });
        }
        // Remove entries with empty Vecs
        self.symbols.retain(|_, v| !v.is_empty());
    }

    /// Exact name lookup. Returns all symbols with the given name.
    pub fn lookup(&self, name: &str) -> Vec<Symbol> {
        self.symbols
            .get(name)
            .map(|r| r.value().clone())
            .unwrap_or_default()
    }

    /// Prefix lookup -- find all symbols whose name starts with `prefix`.
    pub fn lookup_prefix(&self, prefix: &str) -> Vec<Symbol> {
        let mut results = Vec::new();
        for entry in self.symbols.iter() {
            if entry.key().starts_with(prefix) {
                results.extend(entry.value().clone());
            }
        }
        results
    }

    /// Filter symbols by name pattern and optional file path.
    ///
    /// Performs case-insensitive substring matching on symbol names.
    /// If `file` is provided, also filters by file_path.
    pub fn filter(&self, pattern: &str, file: Option<&str>) -> Vec<Symbol> {
        let pattern_lower = pattern.to_lowercase();
        let mut results = Vec::new();
        for entry in self.symbols.iter() {
            if entry.key().to_lowercase().contains(&pattern_lower) {
                for sym in entry.value() {
                    if let Some(f) = file {
                        if sym.file_path == f {
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

    /// Build a symbol table from a flat Vec (after deserialization).
    pub fn from_symbols(symbols: Vec<Symbol>) -> Self {
        let table = Self::new();
        for symbol in symbols {
            table.insert(symbol);
        }
        table
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
}
