//! Orphan detection engine methods.

use crate::orphans::{OrphanConfidence, OrphanFile, OrphanOptions, is_entry_point, is_test_file};

use super::Engine;

impl Engine {
    /// Find orphan files — files with zero in-degree in the dependency graph.
    pub fn find_orphans(&self, options: OrphanOptions) -> Vec<OrphanFile> {
        // Gather all indexed files from the file_chunk_counts map.
        let all_files: Vec<String> = self.file_chunk_counts.keys().cloned().collect();
        let mut orphans = Vec::new();

        for file in &all_files {
            // Apply include/exclude filters
            if !options.include_patterns.is_empty() {
                let matches_include = options.include_patterns.iter().any(|p| file.contains(p));
                if !matches_include {
                    continue;
                }
            }
            if options.exclude_patterns.iter().any(|p| file.contains(p)) {
                continue;
            }

            // Check in-degree
            let callers = self.callers(file);
            if !callers.is_empty() {
                continue;
            }

            // Determine confidence
            let confidence = if is_entry_point(file) || is_test_file(file) {
                OrphanConfidence::Moderate
            } else if options.check_dynamic_refs {
                let stem = std::path::Path::new(file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if stem.len() >= 3 {
                    match self.search_usages(stem, 1) {
                        Ok(results) if !results.is_empty() => OrphanConfidence::High,
                        _ => OrphanConfidence::Certain,
                    }
                } else {
                    OrphanConfidence::Certain
                }
            } else {
                OrphanConfidence::Certain
            };

            let reason = match &confidence {
                OrphanConfidence::Certain => {
                    "No references found in graph or text search.".to_string()
                }
                OrphanConfidence::High => {
                    "No graph references, but filename appears in text search.".to_string()
                }
                OrphanConfidence::Moderate => {
                    if is_entry_point(file) {
                        "Entry point — expected to have zero in-degree.".to_string()
                    } else {
                        "Test file — expected to have zero in-degree.".to_string()
                    }
                }
                OrphanConfidence::Low => {
                    "Inconclusive — may be referenced by external code.".to_string()
                }
            };

            let symbol_count = self.symbols("", Some(file)).map(|s| s.len()).unwrap_or(0);

            orphans.push(OrphanFile {
                file_path: file.clone(),
                confidence,
                reason,
                symbol_count,
                lines: 0,
            });
        }

        orphans.sort_by(|a, b| {
            let conf_order = |c: &OrphanConfidence| match c {
                OrphanConfidence::Certain => 0,
                OrphanConfidence::High => 1,
                OrphanConfidence::Moderate => 2,
                OrphanConfidence::Low => 3,
            };
            conf_order(&a.confidence)
                .cmp(&conf_order(&b.confidence))
                .then(b.symbol_count.cmp(&a.symbol_count))
        });

        orphans.truncate(options.limit);
        orphans
    }
}
