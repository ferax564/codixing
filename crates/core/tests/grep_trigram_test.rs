//! Integration tests for `grep_code` with trigram pre-filtering.
//!
//! These tests verify the full pipeline: Engine::init → FileTrigramIndex built
//! → grep_code uses trigram pre-filter → correct results.

mod common;

use std::fs;

use codixing_core::{Engine, GrepOptions, IndexConfig};
use tempfile::tempdir;

/// Create an `IndexConfig` with embeddings disabled (BM25-only mode).
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut cfg = IndexConfig::new(root);
    cfg.embedding.enabled = false;
    cfg
}

// ── E2E grep_code with trigram pre-filter ────────────────────────────────────

#[test]
fn grep_literal_uses_trigram_prefilter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("auth.rs"),
        "fn authenticate_user(token: &str) -> bool { true }\n",
    )
    .unwrap();
    fs::write(
        src.join("config.rs"),
        "fn load_config(path: &str) -> Config { todo!() }\n",
    )
    .unwrap();
    fs::write(
        src.join("handler.rs"),
        "fn handle_request(req: Request) { authenticate_user(&req.token); }\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Literal search: only auth.rs and handler.rs contain "authenticate_user"
    let results = engine
        .grep_code("authenticate_user", true, None, 0, 50)
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected grep results for 'authenticate_user'"
    );
    let files: Vec<&str> = results.iter().map(|m| m.file_path.as_str()).collect();
    assert!(
        files.iter().any(|f| f.contains("auth.rs")),
        "expected auth.rs in results, got: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("handler.rs")),
        "expected handler.rs in results, got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("config.rs")),
        "config.rs should NOT match 'authenticate_user', got: {files:?}"
    );
}

#[test]
fn grep_regex_or_pattern_uses_query_plan() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("tokio_runtime.rs"),
        "use tokio::runtime::Runtime;\nfn run_tokio() {}\n",
    )
    .unwrap();
    fs::write(
        src.join("asyncstd_tasks.rs"),
        "use async_std::task;\nfn run_async_std() {}\n",
    )
    .unwrap();
    fs::write(
        src.join("blocking.rs"),
        "fn run_blocking() { std::thread::sleep(Duration::from_secs(1)); }\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // OR pattern: (tokio|async_std) should find both async files via QueryPlan OR
    let results = engine
        .grep_code("(tokio|async_std)", false, None, 0, 50)
        .unwrap();

    let files: Vec<&str> = results.iter().map(|m| m.file_path.as_str()).collect();
    assert!(
        files.iter().any(|f| f.contains("tokio_runtime.rs")),
        "expected tokio_runtime.rs in results, got: {files:?}"
    );
    assert!(
        files.iter().any(|f| f.contains("asyncstd_tasks.rs")),
        "expected asyncstd_tasks.rs in results, got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("blocking.rs")),
        "blocking.rs should NOT match, got: {files:?}"
    );
}

#[test]
fn grep_literal_with_regex_metacharacters() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("array.rs"),
        "let val = items[0].field;\nlet other = items[1].field;\n",
    )
    .unwrap();
    fs::write(
        src.join("unrelated.rs"),
        "fn process(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Literal mode: "items[0].field" should NOT be interpreted as regex
    let results = engine
        .grep_code("items[0].field", true, None, 0, 50)
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected grep results for literal 'items[0].field'"
    );
    let files: Vec<&str> = results.iter().map(|m| m.file_path.as_str()).collect();
    assert!(
        files.iter().any(|f| f.contains("array.rs")),
        "expected array.rs in results, got: {files:?}"
    );
    assert!(
        !files.iter().any(|f| f.contains("unrelated.rs")),
        "unrelated.rs should NOT match, got: {files:?}"
    );
}

// ── Glob + trigram interaction ───────────────────────────────────────────────

#[test]
fn grep_with_glob_and_trigram_filter() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // Both files contain "parse_config" but only the .py file matches the glob
    fs::write(
        src.join("config.rs"),
        "fn parse_config(path: &str) -> Config { todo!() }\n",
    )
    .unwrap();
    fs::write(
        src.join("config.py"),
        "def parse_config(path: str) -> dict:\n    return {}\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    // Trigram narrows to both files, glob further narrows to .py only
    let results = engine
        .grep_code("parse_config", true, Some("*.py"), 0, 50)
        .unwrap();

    assert!(
        !results.is_empty(),
        "expected grep results for 'parse_config' with *.py glob"
    );
    for m in &results {
        assert!(
            m.file_path.ends_with(".py"),
            "expected only .py results, got: {}",
            m.file_path
        );
    }
}

// ── GrepOptions: case-insensitive, invert, asymmetric context ────────────────

fn opts(pattern: &str) -> GrepOptions {
    GrepOptions::from_simple(pattern, true, None, 0, 50)
}

#[test]
fn grep_case_insensitive_literal_matches_mixed_case() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("mixed.rs"),
        "struct WidgetFactory;\nfn make_widgetfactory() {}\nfn WIDGETFACTORY_PANIC() {}\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = opts("widgetfactory");
    o.case_insensitive = true;
    let results = engine.grep_code_opts(&o).unwrap();

    assert_eq!(
        results.len(),
        3,
        "expected 3 case-insensitive matches, got {}: {results:?}",
        results.len()
    );
}

#[test]
fn grep_case_insensitive_regex_builds_via_regex_builder() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("todo.rs"),
        "// TODO: fix\n// todo(alice): here\n// Normal line\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = GrepOptions::from_simple("todo", false, None, 0, 50);
    o.case_insensitive = true;
    let results = engine.grep_code_opts(&o).unwrap();

    assert_eq!(results.len(), 2);
}

#[test]
fn grep_invert_returns_non_matching_lines() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    fs::write(
        src.join("mixed.rs"),
        "use foo::bar;\nfn a() {}\nuse baz::qux;\nfn b() {}\n",
    )
    .unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = GrepOptions::from_simple("^use ", false, None, 0, 50);
    o.invert = true;
    let results = engine.grep_code_opts(&o).unwrap();

    let lines: Vec<&str> = results.iter().map(|m| m.line.as_str()).collect();
    assert!(lines.iter().all(|l| !l.starts_with("use ")), "{lines:?}");
    assert!(lines.iter().any(|l| l.contains("fn a()")));
    assert!(lines.iter().any(|l| l.contains("fn b()")));
}

#[test]
fn grep_asymmetric_before_after_context() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    let content = (0..10)
        .map(|i| format!("line_{i}_content"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(src.join("numbered.rs"), content).unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let mut o = opts("line_5_content");
    o.before_context = 2;
    o.after_context = 4;
    let results = engine.grep_code_opts(&o).unwrap();

    assert_eq!(results.len(), 1);
    let m = &results[0];
    assert_eq!(m.before.len(), 2, "expected 2 before-context lines");
    assert_eq!(m.after.len(), 4, "expected 4 after-context lines");
    assert_eq!(m.before[0], "line_3_content");
    assert_eq!(m.before[1], "line_4_content");
    assert_eq!(m.after[0], "line_6_content");
    assert_eq!(m.after[3], "line_9_content");
}

// ── Chunk boundary trigrams ──────────────────────────────────────────────────

#[test]
fn grep_finds_pattern_near_chunk_boundary() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // Create a large file where the target pattern appears deep into the content.
    // This tests that the file-level trigram index (built from full content)
    // correctly includes trigrams regardless of how the chunker splits the file.
    let mut content = String::new();
    for i in 0..200 {
        content.push_str(&format!("fn function_{i}(x: i32) -> i32 {{ x + {i} }}\n"));
    }
    // Insert the target pattern after many functions
    content.push_str("fn unique_boundary_marker_xyz() -> bool { true }\n");
    for i in 200..400 {
        content.push_str(&format!("fn function_{i}(x: i32) -> i32 {{ x + {i} }}\n"));
    }

    fs::write(src.join("large.rs"), &content).unwrap();
    fs::write(src.join("small.rs"), "fn unrelated() -> bool { false }\n").unwrap();

    let engine = Engine::init(root, bm25_config(root)).unwrap();

    let results = engine
        .grep_code("unique_boundary_marker_xyz", true, None, 0, 50)
        .unwrap();

    assert_eq!(
        results.len(),
        1,
        "expected exactly 1 match for unique pattern, got: {}",
        results.len()
    );
    assert!(
        results[0].file_path.contains("large.rs"),
        "expected match in large.rs, got: {}",
        results[0].file_path
    );
}

#[test]
fn grep_finds_literal_spanning_two_non_overlapping_chunks() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();

    // Force each top-level function into a distinct, non-overlapping chunk.
    // The literal `}fn omega` exists only across their shared boundary, so a
    // file trigram index assembled from independent chunk bodies would miss it.
    fs::write(
        src.join("boundary.rs"),
        "fn alpha() { let x = 1; }fn omega() { let y = 2; }\n",
    )
    .unwrap();

    let mut config = bm25_config(root);
    config.chunk.max_chars = 28;
    config.chunk.min_chars = 1;
    config.chunk.overlap_ratio = 0.0;
    let engine = Engine::init(root, config).unwrap();

    let results = engine.grep_code("}fn omega", true, None, 0, 50).unwrap();
    assert_eq!(
        results.len(),
        1,
        "boundary-spanning literal was filtered out"
    );
    assert!(results[0].file_path.ends_with("boundary.rs"));
}

// ── v0.37 trigram v2 format tests ────────────────────────────────────────────

mod trigram_mmap_tests {
    use codixing_core::index::trigram::{PostingCodec, TrigramIndex};
    use tempfile::tempdir;

    /// Tiny deterministic LCG so we don't pull in `rand` for tests.
    struct Lcg(u64);
    impl Lcg {
        fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }
    }

    /// Build a trigram index with `chunk_count` synthetic chunks and inject a
    /// known literal into roughly half of them so we have something to search.
    fn build_index_with_literal(chunk_count: usize, literal: &str, seed: u64) -> TrigramIndex {
        let mut lcg = Lcg::new(seed);
        let mut idx = TrigramIndex::new();
        for chunk_id in 0..chunk_count as u64 {
            // ~140 bytes of pseudo-code per chunk.
            let mut content = String::with_capacity(160);
            for _ in 0..20 {
                let word_id = lcg.next() % 50;
                content.push_str(&format!("ident_{word_id} "));
            }
            if lcg.next().is_multiple_of(2) {
                content.push_str(literal);
                content.push(' ');
            }
            idx.add(chunk_id, &content);
        }
        idx
    }

    /// Reference candidate set straight from the in-memory index.
    fn reference_candidates(idx: &TrigramIndex, query: &str) -> std::collections::BTreeSet<u64> {
        idx.search(query).into_iter().collect()
    }

    /// SplitMix64 is a bijection over u64; setting the high bit mirrors the
    /// hash-derived production IDs that cannot fit in v2's u32 postings.
    fn stable_hash_id(value: u64) -> u64 {
        let mut mixed = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (mixed ^ (mixed >> 31)) | (1u64 << 63)
    }

    fn build_hash_id_index(chunk_count: usize) -> TrigramIndex {
        let mut idx = TrigramIndex::new();
        for chunk in 0..chunk_count as u64 {
            let content = format!(
                "pub fn service_{bucket}() {{ shared_runtime(); parse_request(); validate_input(); }}\n\
                 fn helper_{bucket}() {{ shared_runtime(); emit_response(); }}\n\
                 const REQUEST_MARKER: &str = \"dense_ordinal_marker\";",
                bucket = chunk % 64
            );
            idx.add(stable_hash_id(chunk), &content);
        }
        idx
    }

    #[test]
    fn v2_delta_varint_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk_trigram_v2_dv.bin");

        let original = build_index_with_literal(500, "needle_marker_xyz", 0xDEADBEEF);
        let expected = reference_candidates(&original, "needle_marker_xyz");
        assert!(
            !expected.is_empty(),
            "test setup bug: literal should appear in some chunks"
        );

        original
            .save_mmap_binary_v2(&path, PostingCodec::DeltaVarint)
            .unwrap();

        let loaded = TrigramIndex::load_binary(&path).unwrap();
        let got: std::collections::BTreeSet<u64> =
            loaded.search("needle_marker_xyz").into_iter().collect();

        assert_eq!(got, expected, "v2 delta+varint round-trip lost candidates");
    }

    #[test]
    fn v2_roaring_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk_trigram_v2_roar.bin");

        let original = build_index_with_literal(500, "needle_marker_xyz", 0xCAFEBABE);
        let expected = reference_candidates(&original, "needle_marker_xyz");
        assert!(
            !expected.is_empty(),
            "test setup bug: literal should appear in some chunks"
        );

        original
            .save_mmap_binary_v2(&path, PostingCodec::Roaring)
            .unwrap();

        let loaded = TrigramIndex::load_binary(&path).unwrap();
        let got: std::collections::BTreeSet<u64> =
            loaded.search("needle_marker_xyz").into_iter().collect();

        assert_eq!(got, expected, "v2 roaring round-trip lost candidates");
    }

    #[test]
    fn v2_delta_varint_smaller_than_v1() {
        let dir = tempdir().unwrap();
        let v1_path = dir.path().join("chunk_trigram_v1.bin");
        let v2_path = dir.path().join("chunk_trigram_v2_dv.bin");

        // 200 realistic-looking chunks with repeated identifiers — produces
        // long, dense posting lists, which is what delta+varint compresses.
        let mut idx = TrigramIndex::new();
        let mut lcg = Lcg::new(0x1234_5678_9ABC_DEF0);
        for chunk_id in 0..200u64 {
            let mut content = String::new();
            for _ in 0..30 {
                let word = lcg.next() % 40;
                content.push_str(&format!(
                    "fn helper_{word}() {{ helper_{word}_inner(); }}\n"
                ));
            }
            idx.add(chunk_id, &content);
        }

        idx.save_mmap_binary(&v1_path).unwrap();
        idx.save_mmap_binary_v2(&v2_path, PostingCodec::DeltaVarint)
            .unwrap();

        let v1_size = std::fs::metadata(&v1_path).unwrap().len();
        let v2_size = std::fs::metadata(&v2_path).unwrap().len();

        assert!(
            v2_size < v1_size,
            "v2 ({v2_size} bytes) should be smaller than v1 ({v1_size} bytes)"
        );
        let ratio = (v2_size as f64) / (v1_size as f64);
        assert!(
            ratio <= 0.70,
            "v2 should be at least 30% smaller than v1, got ratio {ratio:.3} (v1={v1_size}, v2={v2_size})"
        );
        eprintln!(
            "v1={v1_size} v2={v2_size} ratio={ratio:.3} ({:.1}% smaller)",
            (1.0 - ratio) * 100.0
        );
    }

    #[test]
    fn v1_backwards_compat_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk_trigram_v1_compat.bin");

        let original = build_index_with_literal(300, "legacy_marker_abc", 0xFEEDFACE);
        let expected = reference_candidates(&original, "legacy_marker_abc");
        assert!(!expected.is_empty(), "test setup bug");

        // Save with the legacy v1 writer.
        original.save_mmap_binary(&path).unwrap();

        let loaded = TrigramIndex::load_binary(&path).unwrap();
        let got: std::collections::BTreeSet<u64> =
            loaded.search("legacy_marker_abc").into_iter().collect();

        assert_eq!(got, expected, "v1 backwards-compat search drifted");
    }

    #[test]
    fn v3_hash_ids_round_trip_through_dense_ordinals() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk_trigram_v3.bin");
        let original = build_hash_id_index(500);
        let expected = reference_candidates(&original, "dense_ordinal_marker");

        original
            .save_mmap_binary_v3(&path, PostingCodec::DeltaVarint)
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 3);

        let loaded = TrigramIndex::load_binary(&path).unwrap();
        let got = reference_candidates(&loaded, "dense_ordinal_marker");
        assert_eq!(got, expected);
        assert!(got.iter().all(|id| *id > u64::from(u32::MAX)));
    }

    #[test]
    fn v3_roaring_round_trip_preserves_stable_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk_trigram_v3_roaring.bin");
        let original = build_hash_id_index(300);
        let expected = reference_candidates(&original, "shared_runtime");

        original
            .save_mmap_binary_v3(&path, PostingCodec::Roaring)
            .unwrap();
        let loaded = TrigramIndex::load_binary(&path).unwrap();

        assert_eq!(reference_candidates(&loaded, "shared_runtime"), expected);
    }

    #[test]
    fn v1_hash_id_index_migrates_to_v3_without_search_drift() {
        let dir = tempdir().unwrap();
        let v1_path = dir.path().join("chunk_trigram_v1.bin");
        let v3_path = dir.path().join("chunk_trigram_v3.bin");
        let original = build_hash_id_index(300);
        let expected = reference_candidates(&original, "validate_input");

        original.save_mmap_binary(&v1_path).unwrap();
        let legacy = TrigramIndex::load_binary(&v1_path).unwrap();
        legacy
            .save_mmap_binary_v3(&v3_path, PostingCodec::DeltaVarint)
            .unwrap();
        let migrated = TrigramIndex::load_binary(&v3_path).unwrap();

        assert_eq!(reference_candidates(&migrated, "validate_input"), expected);
        let bytes = std::fs::read(&v3_path).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 3);
    }

    #[test]
    fn v3_output_is_deterministic_across_insertion_order() {
        let dir = tempdir().unwrap();
        let first_path = dir.path().join("first.bin");
        let second_path = dir.path().join("second.bin");
        let mut first = TrigramIndex::new();
        let mut second = TrigramIndex::new();

        for chunk in 0..128u64 {
            let content = format!("fn stable_{chunk}() {{ common_call(); }}");
            first.add(stable_hash_id(chunk), &content);
        }
        for chunk in (0..128u64).rev() {
            let content = format!("fn stable_{chunk}() {{ common_call(); }}");
            second.add(stable_hash_id(chunk), &content);
        }

        first
            .save_mmap_binary_v3(&first_path, PostingCodec::DeltaVarint)
            .unwrap();
        second
            .save_mmap_binary_v3(&second_path, PostingCodec::DeltaVarint)
            .unwrap();
        assert_eq!(
            std::fs::read(first_path).unwrap(),
            std::fs::read(second_path).unwrap()
        );
    }

    #[test]
    fn v3_hash_id_corpus_is_at_least_half_the_v1_size() {
        let dir = tempdir().unwrap();
        let v1_path = dir.path().join("representative_v1.bin");
        let v3_path = dir.path().join("representative_v3.bin");
        let idx = build_hash_id_index(1_000);

        idx.save_mmap_binary(&v1_path).unwrap();
        idx.save_mmap_binary_v3(&v3_path, PostingCodec::DeltaVarint)
            .unwrap();

        let v1_size = std::fs::metadata(v1_path).unwrap().len();
        let v3_size = std::fs::metadata(v3_path).unwrap().len();
        assert!(
            v3_size * 2 <= v1_size,
            "v3 must use at most half the bytes on a representative hash-ID corpus (v1={v1_size}, v3={v3_size})"
        );
    }

    #[test]
    fn v3_rejects_corrupt_stable_id_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk_trigram_v3_corrupt_ids.bin");
        let idx = build_hash_id_index(20);
        idx.save_mmap_binary_v3(&path, PostingCodec::DeltaVarint)
            .unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let trigram_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let id_table_offset = 32 + trigram_count * 16;
        bytes[id_table_offset] ^= 1;
        std::fs::write(&path, bytes).unwrap();

        let error = TrigramIndex::load_binary(&path)
            .err()
            .expect("corrupt v3 stable ID table must be rejected");
        assert!(
            format!("{error:?}").contains("stable ID checksum"),
            "unexpected corruption error: {error:?}"
        );
    }

    #[test]
    fn v2_unknown_version_returns_serialization_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("chunk_trigram_v2_corrupt.bin");

        let idx = build_index_with_literal(50, "any_marker", 0x9999);
        idx.save_mmap_binary_v2(&path, PostingCodec::DeltaVarint)
            .unwrap();

        // Bump version field (offset 4..8) to 99.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let err = TrigramIndex::load_binary(&path)
            .err()
            .expect("expected load_binary to fail on bumped version");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("Serialization") || msg.to_lowercase().contains("version"),
            "expected serialization/version error, got: {msg}"
        );
    }
}
