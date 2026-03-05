use crate::error::{CodixingError, Result};
use crate::symbols::{Symbol, SymbolTable};

/// Serialize the symbol table to bitcode bytes.
///
/// Extracts all symbols as a flat Vec and serializes via bitcode's serde support.
pub fn serialize_symbols(table: &SymbolTable) -> Result<Vec<u8>> {
    let symbols = table.all_symbols();
    bitcode::serialize(&symbols).map_err(|e| CodixingError::Serialization(e.to_string()))
}

/// Deserialize a symbol table from bitcode bytes.
///
/// Decodes a flat Vec of symbols and rebuilds the indexed `SymbolTable`.
pub fn deserialize_symbols(bytes: &[u8]) -> Result<SymbolTable> {
    let symbols: Vec<Symbol> =
        bitcode::deserialize(bytes).map_err(|e| CodixingError::Serialization(e.to_string()))?;
    Ok(SymbolTable::from_symbols(symbols))
}
