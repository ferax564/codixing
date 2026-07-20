//! Versioned, bounded compression for fully materialized index sidecars.
//!
//! Mmap-backed artifacts deliberately do not use this wrapper: reading one of
//! those through zstd would forfeit lazy paging and random access.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use xxhash_rust::xxh3::xxh3_64;

use crate::error::{CodixingError, Result};

const MAGIC: [u8; 8] = *b"CDXZART\0";
const VERSION: u16 = 1;
const HEADER_LEN: usize = 32;
const ZSTD_LEVEL: i32 = 1;
const GIB: u64 = 1024 * 1024 * 1024;
const INITIAL_DECODE_CAPACITY: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ArtifactKind {
    SymbolGraph = 1,
    Concepts = 2,
    Reformulations = 3,
}

impl ArtifactKind {
    fn label(self) -> &'static str {
        match self {
            Self::SymbolGraph => "symbol graph",
            Self::Concepts => "concept index",
            Self::Reformulations => "reformulations",
        }
    }

    fn max_decoded_bytes(self) -> u64 {
        match self {
            // Symbol graphs are intentionally unbounded by repository file
            // count, so retain ample headroom for multi-million-symbol repos.
            Self::SymbolGraph => 16 * GIB,
            // Current semantic builders are tightly bounded, but legacy
            // indexes in the wild exceed 2 GiB and must remain readable.
            Self::Concepts | Self::Reformulations => 4 * GIB,
        }
    }
}

/// Atomically write one zstd-compressed artifact envelope.
pub(crate) fn write_compressed_artifact(
    path: &Path,
    kind: ArtifactKind,
    decoded: &[u8],
) -> Result<()> {
    let decoded_len = u64::try_from(decoded.len()).map_err(|_| {
        invalid_artifact(
            kind,
            "decoded content length cannot be represented by the file format",
        )
    })?;
    ensure_bounded(kind, decoded_len)?;

    let header = encode_header(kind, decoded_len, xxh3_64(decoded));
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;
    crate::persistence::atomic_write_with(path, |file| {
        file.write_all(&header)?;
        let mut encoder = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)?;
        encoder.include_checksum(true)?;
        encoder.write_all(decoded)?;
        encoder.finish()?;
        Ok(())
    })?;
    Ok(())
}

/// Read a compressed artifact, or a legacy unwrapped artifact when the magic
/// is absent. Once the magic is recognized, every envelope field is mandatory
/// and corruption never falls back to the legacy decoder.
pub(crate) fn read_compressed_or_legacy(path: &Path, kind: ArtifactKind) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let physical_len = file.metadata()?.len();
    if physical_len < MAGIC.len() as u64 {
        return read_legacy(file, kind, physical_len);
    }

    let mut magic = [0_u8; MAGIC.len()];
    file.read_exact(&mut magic)?;
    if magic != MAGIC {
        file.seek(SeekFrom::Start(0))?;
        return read_legacy(file, kind, physical_len);
    }
    if physical_len < HEADER_LEN as u64 {
        return Err(invalid_artifact(kind, "truncated envelope header"));
    }

    let mut header = [0_u8; HEADER_LEN];
    header[..MAGIC.len()].copy_from_slice(&magic);
    file.read_exact(&mut header[MAGIC.len()..])?;
    let (decoded_len, expected_checksum) = decode_header(kind, &header)?;
    ensure_bounded(kind, decoded_len)?;
    let expected_decoded_len = usize::try_from(decoded_len)
        .map_err(|_| invalid_artifact(kind, "decoded length exceeds this platform's limits"))?;

    // Decode at most one byte beyond the advertised size. That proves the
    // exact decoded length without allowing a corrupt frame to grow the Vec
    // beyond the checked bound.
    let mut decoder = zstd::stream::read::Decoder::new(file)?.single_frame();
    let mut decoded = Vec::with_capacity(initial_decode_capacity(decoded_len));
    decoder
        .by_ref()
        .take(decoded_len.saturating_add(1))
        .read_to_end(&mut decoded)?;
    if decoded.len() != expected_decoded_len {
        return Err(invalid_artifact(
            kind,
            format!(
                "decoded length mismatch: header says {decoded_len}, frame produced {}",
                decoded.len()
            ),
        ));
    }

    let mut extra = [0_u8; 1];
    if decoder.read(&mut extra)? != 0 {
        return Err(invalid_artifact(kind, "frame exceeds its decoded length"));
    }
    let mut compressed_input = decoder.finish();
    if compressed_input.read(&mut extra)? != 0 {
        return Err(invalid_artifact(kind, "trailing bytes after zstd frame"));
    }

    let actual_checksum = xxh3_64(&decoded);
    if actual_checksum != expected_checksum {
        return Err(invalid_artifact(
            kind,
            format!(
                "checksum mismatch: expected {expected_checksum:016x}, got {actual_checksum:016x}"
            ),
        ));
    }
    Ok(decoded)
}

fn encode_header(kind: ArtifactKind, decoded_len: u64, checksum: u64) -> [u8; HEADER_LEN] {
    let mut header = [0_u8; HEADER_LEN];
    header[..MAGIC.len()].copy_from_slice(&MAGIC);
    header[8..10].copy_from_slice(&VERSION.to_le_bytes());
    header[10] = kind as u8;
    header[12..20].copy_from_slice(&decoded_len.to_le_bytes());
    header[20..28].copy_from_slice(&checksum.to_le_bytes());
    header
}

fn decode_header(kind: ArtifactKind, header: &[u8; HEADER_LEN]) -> Result<(u64, u64)> {
    let version = u16::from_le_bytes([header[8], header[9]]);
    if version != VERSION {
        return Err(invalid_artifact(
            kind,
            format!("unsupported envelope version {version}"),
        ));
    }
    if header[10] != kind as u8 {
        return Err(invalid_artifact(
            kind,
            format!("artifact kind {} does not match", header[10]),
        ));
    }
    if header[11] != 0 || header[28..].iter().any(|byte| *byte != 0) {
        return Err(invalid_artifact(kind, "unsupported non-zero header flags"));
    }

    let decoded_len = u64::from_le_bytes(header[12..20].try_into().expect("fixed header field"));
    let checksum = u64::from_le_bytes(header[20..28].try_into().expect("fixed header field"));
    Ok((decoded_len, checksum))
}

fn read_legacy(file: File, kind: ArtifactKind, physical_len: u64) -> Result<Vec<u8>> {
    ensure_bounded(kind, physical_len)?;
    let max_decoded_bytes = kind.max_decoded_bytes();
    let mut decoded = Vec::with_capacity(initial_decode_capacity(physical_len));
    file.take(max_decoded_bytes.saturating_add(1))
        .read_to_end(&mut decoded)?;
    if decoded.len() as u64 > max_decoded_bytes {
        return Err(invalid_artifact(
            kind,
            "legacy artifact exceeds decoded size limit",
        ));
    }
    Ok(decoded)
}

fn ensure_bounded(kind: ArtifactKind, decoded_len: u64) -> Result<()> {
    let max_decoded_bytes = kind.max_decoded_bytes();
    if decoded_len > max_decoded_bytes {
        return Err(invalid_artifact(
            kind,
            format!("decoded length {decoded_len} exceeds the {max_decoded_bytes}-byte limit"),
        ));
    }
    Ok(())
}

fn initial_decode_capacity(decoded_len: u64) -> usize {
    decoded_len.min(INITIAL_DECODE_CAPACITY as u64) as usize
}

fn invalid_artifact(kind: ArtifactKind, message: impl std::fmt::Display) -> CodixingError {
    CodixingError::Serialization(format!("invalid {} artifact: {message}", kind.label()))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn compressed_artifact_round_trip() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("artifact.bin");
        let payload = b"repeated payload repeated payload repeated payload";

        write_compressed_artifact(&path, ArtifactKind::Concepts, payload).unwrap();

        let persisted = fs::read(&path).unwrap();
        assert_eq!(&persisted[..MAGIC.len()], &MAGIC);
        assert_eq!(
            read_compressed_or_legacy(&path, ArtifactKind::Concepts).unwrap(),
            payload
        );
    }

    #[test]
    fn legacy_artifact_is_accepted_only_without_magic() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("artifact.bin");
        let legacy = b"legacy raw bytes";
        fs::write(&path, legacy).unwrap();

        assert_eq!(
            read_compressed_or_legacy(&path, ArtifactKind::Reformulations).unwrap(),
            legacy
        );

        fs::write(&path, MAGIC).unwrap();
        assert!(read_compressed_or_legacy(&path, ArtifactKind::Reformulations).is_err());
    }

    #[test]
    fn envelope_requires_matching_kind_length_and_checksum() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("artifact.bin");
        let payload = b"bounded payload";
        write_compressed_artifact(&path, ArtifactKind::SymbolGraph, payload).unwrap();
        let pristine = fs::read(&path).unwrap();

        assert!(read_compressed_or_legacy(&path, ArtifactKind::Concepts).is_err());

        let mut wrong_length = pristine.clone();
        wrong_length[12..20].copy_from_slice(&((payload.len() + 1) as u64).to_le_bytes());
        fs::write(&path, wrong_length).unwrap();
        assert!(read_compressed_or_legacy(&path, ArtifactKind::SymbolGraph).is_err());

        let mut wrong_checksum = pristine;
        wrong_checksum[20] ^= 1;
        fs::write(&path, wrong_checksum).unwrap();
        assert!(read_compressed_or_legacy(&path, ArtifactKind::SymbolGraph).is_err());
    }

    #[test]
    fn envelope_rejects_oversized_lengths_and_trailing_bytes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("artifact.bin");
        write_compressed_artifact(&path, ArtifactKind::Concepts, b"payload").unwrap();
        let pristine = fs::read(&path).unwrap();

        let mut oversized = pristine.clone();
        oversized[12..20].copy_from_slice(
            &ArtifactKind::Concepts
                .max_decoded_bytes()
                .saturating_add(1)
                .to_le_bytes(),
        );
        fs::write(&path, oversized).unwrap();
        assert!(read_compressed_or_legacy(&path, ArtifactKind::Concepts).is_err());

        fs::write(&path, pristine).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"trailing")
            .unwrap();
        assert!(read_compressed_or_legacy(&path, ArtifactKind::Concepts).is_err());
    }

    #[test]
    fn huge_advertised_length_uses_only_small_initial_capacity() {
        let huge_length = ArtifactKind::Concepts.max_decoded_bytes();
        assert!(huge_length > u32::MAX as u64);
        assert_eq!(
            initial_decode_capacity(huge_length),
            INITIAL_DECODE_CAPACITY
        );

        let directory = tempdir().unwrap();
        let path = directory.path().join("artifact.bin");
        write_compressed_artifact(&path, ArtifactKind::Concepts, b"small frame").unwrap();
        let mut persisted = fs::read(&path).unwrap();
        persisted[12..20].copy_from_slice(&huge_length.to_le_bytes());
        fs::write(&path, persisted).unwrap();

        assert!(read_compressed_or_legacy(&path, ArtifactKind::Concepts).is_err());
    }
}
