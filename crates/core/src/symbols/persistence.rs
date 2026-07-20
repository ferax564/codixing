use crate::error::{CodixingError, Result};
use crate::symbols::{Symbol, SymbolTable};

const SYMBOL_DELTA_MAGIC: [u8; 8] = *b"CDXSYMDL";
const SYMBOL_DELTA_VERSION: u32 = 1;

/// Keep reopen work and corruption exposure bounded. Checkpoints above either
/// limit must compact into a new `symbols_v2.bin` instead of publishing an
/// ever-growing overlay.
pub(crate) const SYMBOL_DELTA_COMPACT_FILES: usize = 4_096;
pub(crate) const SYMBOL_DELTA_MAX_BYTES: usize = 8 * 1024 * 1024;

pub(crate) type SymbolFileReplacement = (String, Vec<Symbol>);

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

/// Serialize complete replacements for the bounded set of files changed since
/// the mmap symbol base was last compacted. An empty symbol list is a durable
/// deletion tombstone for that file.
pub(crate) fn serialize_symbol_delta(replacements: &[SymbolFileReplacement]) -> Result<Vec<u8>> {
    encode_symbol_delta_checkpoint(replacements)?.ok_or_else(|| {
        CodixingError::Serialization(format!(
            "symbol delta exceeds checkpoint limits ({SYMBOL_DELTA_COMPACT_FILES} files or {SYMBOL_DELTA_MAX_BYTES} bytes)"
        ))
    })
}

/// Encode a bounded checkpoint overlay. `None` asks the caller to compact the
/// complete symbol table into a new mmap base instead of failing the sync.
pub(crate) fn encode_symbol_delta_checkpoint(
    replacements: &[SymbolFileReplacement],
) -> Result<Option<Vec<u8>>> {
    let replacements = canonical_symbol_delta(replacements.to_vec())?;
    if replacements.len() > SYMBOL_DELTA_COMPACT_FILES {
        return Ok(None);
    }

    let payload = bitcode::serialize(&replacements)
        .map_err(|error| CodixingError::Serialization(error.to_string()))?;
    let total_len = SYMBOL_DELTA_MAGIC
        .len()
        .checked_add(std::mem::size_of::<u32>())
        .and_then(|header| header.checked_add(payload.len()))
        .ok_or_else(|| CodixingError::Serialization("symbol delta size overflow".to_string()))?;
    if total_len > SYMBOL_DELTA_MAX_BYTES {
        return Ok(None);
    }

    let mut bytes = Vec::with_capacity(total_len);
    bytes.extend_from_slice(&SYMBOL_DELTA_MAGIC);
    bytes.extend_from_slice(&SYMBOL_DELTA_VERSION.to_le_bytes());
    bytes.extend_from_slice(&payload);
    Ok(Some(bytes))
}

/// Decode and validate a symbol overlay. Callers must reject files larger than
/// [`SYMBOL_DELTA_MAX_BYTES`] before reading them so hostile length fields
/// cannot turn a small checkpoint into unbounded reopen work.
pub(crate) fn deserialize_symbol_delta(bytes: &[u8]) -> Result<Vec<SymbolFileReplacement>> {
    if bytes.len() > SYMBOL_DELTA_MAX_BYTES {
        return Err(CodixingError::Serialization(format!(
            "symbol delta is {} bytes; maximum is {SYMBOL_DELTA_MAX_BYTES}",
            bytes.len()
        )));
    }
    let header_len = SYMBOL_DELTA_MAGIC.len() + std::mem::size_of::<u32>();
    if bytes.len() < header_len || bytes[..SYMBOL_DELTA_MAGIC.len()] != SYMBOL_DELTA_MAGIC {
        return Err(CodixingError::Serialization(
            "invalid symbol delta header".to_string(),
        ));
    }
    let version_start = SYMBOL_DELTA_MAGIC.len();
    let version = u32::from_le_bytes(
        bytes[version_start..header_len]
            .try_into()
            .expect("symbol delta version slice has fixed width"),
    );
    if version != SYMBOL_DELTA_VERSION {
        return Err(CodixingError::Serialization(format!(
            "unsupported symbol delta version {version}"
        )));
    }

    let replacements: Vec<SymbolFileReplacement> = bitcode::deserialize(&bytes[header_len..])
        .map_err(|error| {
            CodixingError::Serialization(format!("failed to decode symbol delta: {error}"))
        })?;
    if replacements.len() > SYMBOL_DELTA_COMPACT_FILES {
        return Err(CodixingError::Serialization(format!(
            "symbol delta contains {} files; maximum is {SYMBOL_DELTA_COMPACT_FILES}",
            replacements.len()
        )));
    }
    canonical_symbol_delta(replacements)
}

fn canonical_symbol_delta(
    mut replacements: Vec<SymbolFileReplacement>,
) -> Result<Vec<SymbolFileReplacement>> {
    for (file_path, symbols) in &mut replacements {
        let bytes = file_path.as_bytes();
        let windows_drive_prefix =
            bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        let normalized_relative = !file_path.is_empty()
            && !file_path.contains('\\')
            && !file_path.contains('\0')
            && !windows_drive_prefix
            && file_path
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != "..");
        if !normalized_relative {
            return Err(CodixingError::Serialization(format!(
                "symbol delta contains unsafe or non-normalized file path {file_path:?}"
            )));
        }
        if let Some(symbol) = symbols.iter().find(|symbol| symbol.file_path != *file_path) {
            return Err(CodixingError::Serialization(format!(
                "symbol delta entry {file_path:?} contains symbol {:?} from {:?}",
                symbol.name, symbol.file_path
            )));
        }

        // Full serialized records provide a total, future-proof ordering even
        // when new Symbol fields do not implement `Ord`.
        let mut keyed = Vec::with_capacity(symbols.len());
        for symbol in std::mem::take(symbols) {
            let key = bitcode::serialize(&symbol)
                .map_err(|error| CodixingError::Serialization(error.to_string()))?;
            keyed.push((key, symbol));
        }
        keyed.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        *symbols = keyed.into_iter().map(|(_, symbol)| symbol).collect();
    }

    replacements.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    if let Some(duplicate) = replacements
        .windows(2)
        .find(|pair| pair[0].0 == pair[1].0)
        .map(|pair| pair[0].0.as_str())
    {
        return Err(CodixingError::Serialization(format!(
            "symbol delta contains duplicate file path {duplicate:?}"
        )));
    }
    Ok(replacements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::{EntityKind, Language, Visibility};

    fn symbol(file_path: &str) -> Symbol {
        Symbol {
            name: "target".to_string(),
            kind: EntityKind::Function,
            language: Language::Rust,
            file_path: file_path.to_string(),
            line_start: 1,
            line_end: 2,
            byte_start: 3,
            byte_end: 4,
            signature: None,
            scope: Vec::new(),
            doc_comment: None,
            visibility: Visibility::Private,
            type_relations: Vec::new(),
        }
    }

    fn unchecked_bytes(replacements: &[SymbolFileReplacement]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&SYMBOL_DELTA_MAGIC);
        bytes.extend_from_slice(&SYMBOL_DELTA_VERSION.to_le_bytes());
        bytes.extend_from_slice(&bitcode::serialize(replacements).unwrap());
        bytes
    }

    #[test]
    fn symbol_delta_decoder_rejects_duplicate_paths_and_path_mismatches() {
        let duplicates = unchecked_bytes(&[
            ("src/same.rs".to_string(), Vec::new()),
            ("src/same.rs".to_string(), Vec::new()),
        ]);
        assert!(deserialize_symbol_delta(&duplicates).is_err());

        let mismatched =
            unchecked_bytes(&[("src/expected.rs".to_string(), vec![symbol("src/other.rs")])]);
        assert!(deserialize_symbol_delta(&mismatched).is_err());
    }

    #[test]
    fn symbol_delta_decoder_rejects_unsafe_or_non_normalized_paths() {
        for file_path in [
            "",
            "/absolute.rs",
            "C:/windows.rs",
            "./src/lib.rs",
            "src/../lib.rs",
            "src//lib.rs",
            "src\\lib.rs",
            "src/nu\0l.rs",
        ] {
            let bytes = unchecked_bytes(&[(file_path.to_string(), Vec::new())]);
            assert!(
                deserialize_symbol_delta(&bytes).is_err(),
                "unsafe symbol delta path unexpectedly accepted: {file_path:?}"
            );
        }
        let valid = unchecked_bytes(&[("src/nested/lib.rs".to_string(), Vec::new())]);
        assert_eq!(
            deserialize_symbol_delta(&valid).unwrap()[0].0,
            "src/nested/lib.rs"
        );
    }

    #[test]
    fn symbol_delta_decoder_rejects_oversized_input_before_parsing() {
        let bytes = vec![0; SYMBOL_DELTA_MAX_BYTES + 1];
        let error = deserialize_symbol_delta(&bytes).unwrap_err();
        assert!(error.to_string().contains("maximum"));
    }

    #[test]
    fn symbol_delta_checkpoint_encoder_requests_compaction_above_file_limit() {
        let replacements = (0..=SYMBOL_DELTA_COMPACT_FILES)
            .map(|index| (format!("src/{index}.rs"), Vec::new()))
            .collect::<Vec<_>>();
        assert!(
            encode_symbol_delta_checkpoint(&replacements)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn symbol_delta_checkpoint_encoder_requests_compaction_above_byte_limit() {
        let mut oversized = symbol("src/large.rs");
        oversized.doc_comment = Some("x".repeat(SYMBOL_DELTA_MAX_BYTES));
        assert!(
            encode_symbol_delta_checkpoint(&[("src/large.rs".to_string(), vec![oversized])])
                .unwrap()
                .is_none()
        );
    }
}
