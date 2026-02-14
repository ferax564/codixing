use serde::{Deserialize, Serialize};

/// The kind of symbol represented by a node in the code graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Trait,
    Module,
    Const,
    Type,
}

/// The kind of reference represented by an edge in the code graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferenceKind {
    Call,
    Import,
    Inherit,
    FieldAccess,
    TypeRef,
}

/// A symbol node in the code graph, representing a named code entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolNode {
    pub name: String,
    pub file: String,
    pub kind: SymbolKind,
    pub line: Option<usize>,
}
