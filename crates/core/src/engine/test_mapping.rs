//! Test-to-code mapping engine methods.

use std::collections::HashMap;

use crate::test_mapping::{
    FileLookup, TestMapping, TestMappingOptions, discover_test_mappings, is_test_file,
    sources_for_test, tests_for_source_indexed,
};

use super::Engine;

impl Engine {
    /// Build the complete test mapping for the project.
    ///
    /// Combines naming conventions, directory conventions, and import graph
    /// analysis to link test files to their corresponding source files.
    pub fn build_test_map(&self, options: TestMappingOptions) -> Vec<TestMapping> {
        let all_files: Vec<String> = self.file_chunk_ids.keys().cloned().collect();

        // Build import dependency map from the graph (if available). Only test
        // files are consulted by the mapping, so only their imports are read.
        let import_deps: Option<HashMap<String, Vec<String>>> = self.graph_ref().map(|g| {
            let mut deps: HashMap<String, Vec<String>> = HashMap::new();
            for file in all_files.iter().filter(|f| is_test_file(f)) {
                let callees = g.callees(file);
                if !callees.is_empty() {
                    deps.insert(file.clone(), callees);
                }
            }
            deps
        });

        discover_test_mappings(&all_files, import_deps.as_ref(), &options)
    }

    /// Given a source file, find its tests.
    ///
    /// Computed directly for this one file. Building the project-wide map and
    /// filtering it cost a whole-repository pass per call and, because that
    /// map is capped at its top 200 entries, returned nothing for most files
    /// of a large repository.
    pub fn find_tests_for_file(&self, file_path: &str) -> Vec<TestMapping> {
        self.find_tests_for_files(&[file_path])
            .into_iter()
            .next()
            .map(|(_, mappings)| mappings)
            .unwrap_or_default()
    }

    /// [`find_tests_for_file`](Self::find_tests_for_file) for several source
    /// files, sharing one snapshot of the indexed file list.
    pub fn find_tests_for_files(&self, file_paths: &[&str]) -> Vec<(String, Vec<TestMapping>)> {
        let all_files: Vec<String> = self.file_chunk_ids.keys().cloned().collect();
        let lookup = FileLookup::new(&all_files);
        file_paths
            .iter()
            .map(|path| {
                let importers = self
                    .graph
                    .as_ref()
                    .map(|g| g.callers(path))
                    .unwrap_or_default();
                (
                    (*path).to_string(),
                    tests_for_source_indexed(path, &all_files, &lookup, &importers),
                )
            })
            .collect()
    }

    /// Given a test file, find what source code it tests.
    pub fn find_source_for_test(&self, test_file: &str) -> Vec<TestMapping> {
        let all_files: Vec<String> = self.file_chunk_ids.keys().cloned().collect();
        let imports = self
            .graph
            .as_ref()
            .map(|g| g.callees(test_file))
            .unwrap_or_default();
        sources_for_test(test_file, &all_files, &imports)
    }

    /// Check if a file is a test file.
    pub fn is_test_file(&self, path: &str) -> bool {
        is_test_file(path)
    }
}
