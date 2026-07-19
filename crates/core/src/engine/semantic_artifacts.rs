//! Bounded construction and crash-safe persistence for auxiliary semantic data.
//!
//! These artifacts improve recall but are never authoritative. Mutation paths
//! therefore invalidate both files first: a failed or interrupted rebuild can
//! temporarily disable semantic expansion, but can never serve stale mappings.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::info;

use super::concepts::{ConceptIndex, ConceptIndexBuilder};
use super::reformulation::{LearnedReformulations, ReformulationBuilder};
use crate::error::{CodixingError, Result};
use crate::graph::CodeGraph;
use crate::persistence::IndexStore;
use crate::symbols::SymbolTable;

static SEMANTIC_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
        atomic_write(&store.concepts_path(), &bytes)?;
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
        atomic_write(&store.reformulations_path(), &bytes)?;
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
    let bytes = fs::read(path)?;
    ConceptIndex::decode_persisted(&bytes).map_err(|error| {
        CodixingError::Serialization(format!("failed to decode concept index: {error}"))
    })
}

pub(super) fn load_reformulations(path: &Path) -> Result<LearnedReformulations> {
    let bytes = fs::read(path)?;
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

fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("semantic");
    let sequence = SEMANTIC_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".{name}.tmp.{}.{}", std::process::id(), sequence));
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}
