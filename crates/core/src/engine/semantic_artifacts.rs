//! Bounded construction and crash-safe persistence for auxiliary semantic data.
//!
//! These artifacts improve recall but are never authoritative. Mutation paths
//! therefore invalidate both files first: a failed or interrupted rebuild can
//! temporarily disable semantic expansion, but can never serve stale mappings.

use std::fs;
use std::path::Path;

use tracing::info;

use super::concepts::{ConceptIndex, ConceptIndexBuilder};
use super::reformulation::{LearnedReformulations, ReformulationBuilder};
use crate::compressed_artifact::{
    ArtifactKind, read_compressed_or_legacy, write_compressed_artifact,
};
use crate::error::{CodixingError, Result};
use crate::graph::CodeGraph;
use crate::persistence::IndexStore;
use crate::symbols::SymbolTable;

/// Rebuild both semantic artifacts from the current authoritative symbol/graph
/// state. Existing artifacts are removed before construction so concurrent
/// readers either see fresh data or no auxiliary data, never stale data.
pub(super) fn rebuild_semantic_artifacts(
    store: &IndexStore,
    symbols: &SymbolTable,
    graph: Option<&CodeGraph>,
) -> Result<()> {
    invalidate_semantic_artifacts(store)?;

    let mut concept_builder = ConceptIndexBuilder::new();
    symbols.visit_symbols(|symbol| {
        concept_builder.add_symbol(
            &symbol.name,
            &symbol.file_path,
            symbol.doc_comment.as_deref(),
        );
    });
    if let Some(graph) = graph {
        // Query adjacency one source file at a time. `to_flat()` clones the
        // complete graph; only files retained by the bounded concept source
        // reservoir can contribute to a cluster.
        for from in concept_builder.source_files() {
            for to in graph.callees(&from) {
                concept_builder.add_cooccurrence(&from, &to);
            }
        }
    }
    let concept_index = concept_builder.build();
    let concept_bytes = if concept_index.is_empty() {
        0
    } else {
        let bytes = concept_index.encode_persisted().map_err(|error| {
            CodixingError::Serialization(format!("failed to encode concept index: {error}"))
        })?;
        write_compressed_artifact(&store.concepts_path(), ArtifactKind::Concepts, &bytes)?;
        bytes.len()
    };
    drop(concept_index);

    let mut reformulation_builder = ReformulationBuilder::new();
    // A second streaming pass keeps the concept and reformulation source
    // reservoirs from overlapping and never clones the complete symbol table.
    symbols.visit_symbols(|symbol| {
        reformulation_builder.add_identifier(&symbol.name, &symbol.file_path);
        if let Some(doc) = symbol.doc_comment.as_deref() {
            reformulation_builder.add_documented_symbol(&symbol.name, doc);
        }
    });
    let reformulations = reformulation_builder.build();
    let reformulation_bytes = if reformulations.is_empty() {
        0
    } else {
        let bytes = reformulations.encode_persisted().map_err(|error| {
            CodixingError::Serialization(format!("failed to encode reformulations: {error}"))
        })?;
        write_compressed_artifact(
            &store.reformulations_path(),
            ArtifactKind::Reformulations,
            &bytes,
        )?;
        bytes.len()
    };

    info!(
        concept_bytes,
        reformulation_bytes, "rebuilt bounded semantic artifacts"
    );
    Ok(())
}

pub(super) fn invalidate_semantic_artifacts(store: &IndexStore) -> Result<()> {
    remove_if_present(&store.concepts_path())?;
    remove_if_present(&store.reformulations_path())?;
    Ok(())
}

pub(super) fn load_concept_index(path: &Path) -> Result<ConceptIndex> {
    let bytes = read_compressed_or_legacy(path, ArtifactKind::Concepts)?;
    ConceptIndex::decode_persisted(&bytes).map_err(|error| {
        CodixingError::Serialization(format!("failed to decode concept index: {error}"))
    })
}

pub(super) fn load_reformulations(path: &Path) -> Result<LearnedReformulations> {
    let bytes = read_compressed_or_legacy(path, ArtifactKind::Reformulations)?;
    LearnedReformulations::decode_persisted(&bytes).map_err(|error| {
        CodixingError::Serialization(format!("failed to decode reformulations: {error}"))
    })
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn semantic_loaders_accept_compressed_and_legacy_artifacts() {
        let directory = tempdir().unwrap();

        let mut concept_builder = ConceptIndexBuilder::new();
        concept_builder.add_symbol("auth_login", "src/auth.rs", None);
        concept_builder.add_symbol("auth_token", "src/token.rs", None);
        let concepts = concept_builder.build();
        let concept_bytes = concepts.encode_persisted().unwrap();
        let concept_path = directory.path().join("concepts.bin");
        write_compressed_artifact(&concept_path, ArtifactKind::Concepts, &concept_bytes).unwrap();
        assert_eq!(
            load_concept_index(&concept_path)
                .unwrap()
                .encode_persisted()
                .unwrap(),
            concept_bytes
        );
        fs::write(&concept_path, &concept_bytes).unwrap();
        assert_eq!(
            load_concept_index(&concept_path)
                .unwrap()
                .encode_persisted()
                .unwrap(),
            concept_bytes
        );

        let mut reformulation_builder = ReformulationBuilder::new();
        reformulation_builder.add_identifier("parse_json", "src/parser.rs");
        reformulation_builder.add_identifier("json_decode", "src/parser.rs");
        let reformulations = reformulation_builder.build();
        let reformulation_bytes = reformulations.encode_persisted().unwrap();
        let reformulation_path = directory.path().join("reformulations.bin");
        write_compressed_artifact(
            &reformulation_path,
            ArtifactKind::Reformulations,
            &reformulation_bytes,
        )
        .unwrap();
        assert_eq!(
            load_reformulations(&reformulation_path)
                .unwrap()
                .encode_persisted()
                .unwrap(),
            reformulation_bytes
        );
        fs::write(&reformulation_path, &reformulation_bytes).unwrap();
        assert_eq!(
            load_reformulations(&reformulation_path)
                .unwrap()
                .encode_persisted()
                .unwrap(),
            reformulation_bytes
        );
    }
}
