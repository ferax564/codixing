//! Integration tests for the cAST chunker: coverage, size limits, and ID uniqueness.

mod common;

use std::collections::HashSet;
use std::fs;

use codeforge_core::chunker::cast::CastChunker;
use codeforge_core::chunker::{Chunker, non_ws_chars};
use codeforge_core::config::ChunkConfig;
use codeforge_core::language::detect_language;
use tempfile::tempdir;

/// Parse a file with tree-sitter and return chunks.
fn chunks_for_file(
    file_path: &str,
    source: &[u8],
    config: &ChunkConfig,
) -> Vec<codeforge_core::chunker::Chunk> {
    let path = std::path::Path::new(file_path);
    let language = detect_language(path).expect("unsupported language in test");

    // Set up a tree-sitter parser for the detected language.
    let mut parser = tree_sitter::Parser::new();
    let ts_lang: tree_sitter::Language = match language {
        codeforge_core::language::Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        codeforge_core::language::Language::Python => tree_sitter_python::LANGUAGE.into(),
        codeforge_core::language::Language::TypeScript => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        codeforge_core::language::Language::Go => tree_sitter_go::LANGUAGE.into(),
        _ => panic!("language not set up in test helper: {:?}", language),
    };

    parser.set_language(&ts_lang).expect("set_language failed");
    let tree = parser
        .parse(source, None)
        .expect("tree-sitter parse failed");

    let chunker = CastChunker;
    chunker.chunk(file_path, source, &tree, language, config)
}

#[test]
fn all_bytes_covered_in_chunks() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let config = ChunkConfig::default();

    let test_files = [
        ("src/main.rs", "src/main.rs"),
        ("src/lib.rs", "src/lib.rs"),
        ("src/utils.py", "src/utils.py"),
        ("src/index.ts", "src/index.ts"),
        ("src/server.go", "src/server.go"),
    ];

    for (rel_path, chunk_path) in &test_files {
        let abs_path = root.join(rel_path);
        let source = fs::read(&abs_path).unwrap();
        let chunks = chunks_for_file(chunk_path, &source, &config);

        assert!(
            !chunks.is_empty(),
            "expected at least 1 chunk for {}",
            rel_path
        );

        // Build coverage bitmap.
        let mut covered = vec![false; source.len()];
        for chunk in &chunks {
            for item in covered
                .iter_mut()
                .take(chunk.byte_end)
                .skip(chunk.byte_start)
            {
                *item = true;
            }
        }

        // Verify every non-whitespace byte is covered.
        for (i, &is_covered) in covered.iter().enumerate() {
            if !source[i].is_ascii_whitespace() && !is_covered {
                panic!(
                    "Byte {} in {} ('{}') not covered by any chunk",
                    i, rel_path, source[i] as char
                );
            }
        }

        // Verify no overlaps between consecutive chunks.
        let mut sorted_chunks: Vec<_> = chunks.iter().collect();
        sorted_chunks.sort_by_key(|c| c.byte_start);
        for window in sorted_chunks.windows(2) {
            assert!(
                window[0].byte_end <= window[1].byte_start,
                "Overlap detected in {}: chunk [{}, {}) overlaps [{}, {})",
                rel_path,
                window[0].byte_start,
                window[0].byte_end,
                window[1].byte_start,
                window[1].byte_end,
            );
        }
    }
}

#[test]
fn chunks_respect_size_limit() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let config = ChunkConfig::default();
    let max = config.max_chars;

    let test_files = ["src/main.rs", "src/lib.rs", "src/utils.py", "src/index.ts"];

    for rel_path in &test_files {
        let abs_path = root.join(rel_path);
        let source = fs::read(&abs_path).unwrap();
        let chunks = chunks_for_file(rel_path, &source, &config);

        for (i, chunk) in chunks.iter().enumerate() {
            let nws = non_ws_chars(chunk.content.as_bytes());
            // The cAST algorithm allows oversized chunks only for indivisible leaf
            // nodes. Our test files are small, so all chunks should fit.
            assert!(
                nws <= max,
                "Chunk {} in {} has {} non-ws chars, exceeding max of {}\nContent:\n{}",
                i,
                rel_path,
                nws,
                max,
                chunk.content,
            );
        }
    }
}

#[test]
fn chunk_ids_are_unique() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    common::setup_multi_language_project(root);

    let config = ChunkConfig::default();

    let test_files = [
        "src/main.rs",
        "src/lib.rs",
        "src/utils.py",
        "src/index.ts",
        "src/server.go",
    ];

    let mut all_ids = HashSet::new();
    let mut total_chunks = 0;

    for rel_path in &test_files {
        let abs_path = root.join(rel_path);
        let source = fs::read(&abs_path).unwrap();
        let chunks = chunks_for_file(rel_path, &source, &config);

        for chunk in &chunks {
            all_ids.insert(chunk.id);
            total_chunks += 1;
        }
    }

    assert_eq!(
        all_ids.len(),
        total_chunks,
        "expected all {} chunk IDs to be unique, but got only {} distinct IDs",
        total_chunks,
        all_ids.len()
    );
}
