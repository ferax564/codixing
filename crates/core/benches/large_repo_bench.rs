use std::fs;
use std::path::PathBuf;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

use codixing_core::{Engine, IndexConfig, SearchQuery, Strategy};

/// Generate `n` synthetic Rust files distributed into subdirectories (100 files
/// per subdir). Each file has ~30 lines with a struct, impl, function, and test
/// module.
fn generate_files(dir: &std::path::Path, n: usize) {
    for i in 0..n {
        let subdir_idx = i / 100;
        let subdir = dir.join(format!("sub_{subdir_idx}"));
        fs::create_dir_all(&subdir).unwrap();

        let content = format!(
            r#"/// Widget {i} — generated synthetic module.
///
/// Provides processing utilities for widget instances.
pub struct Widget_{i} {{
    pub id: u64,
    pub name: String,
    pub active: bool,
}}

impl Widget_{i} {{
    /// Create a new Widget_{i} with default settings.
    pub fn new(id: u64, name: impl Into<String>) -> Self {{
        Self {{
            id,
            name: name.into(),
            active: true,
        }}
    }}

    /// Process this widget and return a result string.
    pub fn process(&self) -> String {{
        format!("processed_{{}}_{i}", self.name)
    }}
}}

/// Standalone process function for Widget_{i}.
pub fn process_widget_{i}(w: &Widget_{i}) -> bool {{
    w.active && !w.name.is_empty()
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn test_widget_{i}_new() {{
        let w = Widget_{i}::new({i}, "test");
        assert!(w.active);
    }}

    #[test]
    fn test_process_widget_{i}() {{
        let w = Widget_{i}::new({i}, "hello");
        assert!(process_widget_{i}(&w));
    }}
}}
"#,
        );
        fs::write(subdir.join(format!("widget_{i}.rs")), content).unwrap();
    }
}

/// Build a BM25-only `IndexConfig` rooted at `root`.
fn bm25_config(root: &std::path::Path) -> IndexConfig {
    let mut config = IndexConfig::new(root);
    config.embedding.enabled = false;
    config
}

fn bench_init(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_repo_init");
    group.sample_size(10);

    for &size in &[1_000usize, 10_000usize] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &n| {
            b.iter_with_setup(
                || {
                    let dir = tempfile::tempdir().unwrap();
                    let root: PathBuf = dir.path().to_path_buf();
                    generate_files(&root, n);
                    (dir, root)
                },
                |(dir, root)| {
                    let config = bm25_config(&root);
                    let engine = Engine::init(&root, config).unwrap();
                    // Keep dir alive until after init completes.
                    drop(dir);
                    engine
                },
            )
        });
    }

    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_repo_search");

    for &size in &[1_000usize, 10_000usize] {
        // Build the engine once outside the timed loop.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        generate_files(&root, size);
        let config = bm25_config(&root);
        let engine = Engine::init(&root, config).unwrap();

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| {
                engine
                    .search(
                        SearchQuery::new("Widget process")
                            .with_limit(10)
                            .with_strategy(Strategy::Instant),
                    )
                    .unwrap()
            })
        });

        drop(dir);
    }

    group.finish();
}

fn bench_grep(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_repo_grep");

    for &size in &[1_000usize, 10_000usize] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        generate_files(&root, size);
        let config = bm25_config(&root);
        let engine = Engine::init(&root, config).unwrap();

        // Literal grep — trigram narrows to ~1 file out of thousands
        group.bench_with_input(BenchmarkId::new("trigram_literal", size), &size, |b, _| {
            b.iter(|| engine.grep_code("Widget_500", true, None, 0, 50).unwrap())
        });
        group.bench_with_input(
            BenchmarkId::new("full_scan_literal", size),
            &size,
            |b, _| {
                b.iter(|| {
                    engine
                        .grep_code_full_scan("Widget_500", true, None, 0, 50)
                        .unwrap()
                })
            },
        );

        // Regex grep — trigram extracts "process_widget" trigrams
        group.bench_with_input(BenchmarkId::new("trigram_regex", size), &size, |b, _| {
            b.iter(|| {
                engine
                    .grep_code("process_widget_\\d+", false, None, 0, 50)
                    .unwrap()
            })
        });
        group.bench_with_input(BenchmarkId::new("full_scan_regex", size), &size, |b, _| {
            b.iter(|| {
                engine
                    .grep_code_full_scan("process_widget_\\d+", false, None, 0, 50)
                    .unwrap()
            })
        });

        drop(dir);
    }

    group.finish();
}

criterion_group!(benches, bench_init, bench_search, bench_grep);
criterion_main!(benches);
