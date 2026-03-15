//! Test-to-code mapping engine methods.

use std::collections::HashMap;

use crate::test_mapping::{TestMapping, TestMappingOptions, discover_test_mappings, is_test_file};

use super::Engine;

impl Engine {
    /// Build the complete test mapping for the project.
    ///
    /// Combines naming conventions, directory conventions, and import graph
    /// analysis to link test files to their corresponding source files.
    pub fn build_test_map(&self, options: TestMappingOptions) -> Vec<TestMapping> {
        let all_files: Vec<String> = self.file_chunk_counts.keys().cloned().collect();

        // Build import dependency map from the graph (if available).
        let import_deps: Option<HashMap<String, Vec<String>>> = self.graph.as_ref().map(|g| {
            let mut deps: HashMap<String, Vec<String>> = HashMap::new();
            for file in &all_files {
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
    /// Returns all test mappings where `source_file` matches the given path.
    pub fn find_tests_for_file(&self, file_path: &str) -> Vec<TestMapping> {
        let mappings = self.build_test_map(TestMappingOptions::default());
        mappings
            .into_iter()
            .filter(|m| m.source_file == file_path)
            .collect()
    }

    /// Given a test file, find what source code it tests.
    ///
    /// Returns all test mappings where `test_file` matches the given path.
    pub fn find_source_for_test(&self, test_file: &str) -> Vec<TestMapping> {
        let mappings = self.build_test_map(TestMappingOptions::default());
        mappings
            .into_iter()
            .filter(|m| m.test_file == test_file)
            .collect()
    }

    /// Check if a file is a test file.
    pub fn is_test_file(&self, path: &str) -> bool {
        is_test_file(path)
    }
}
