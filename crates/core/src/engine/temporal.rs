use crate::temporal::{
    self, BlameLine, ChangeEntry, Hotspot,
};

use super::Engine;

impl Engine {
    // -------------------------------------------------------------------------
    // Temporal analysis public API (Phase 13b)
    // -------------------------------------------------------------------------

    /// Return the project root path.
    pub fn root(&self) -> &std::path::Path {
        self.store.root()
    }

    /// Get file hotspots — files that change most frequently.
    ///
    /// Uses `git log` to count commits per file over the given time window,
    /// weighted by recency and author diversity.
    pub fn get_hotspots(&self, limit: usize, days: u64) -> Vec<Hotspot> {
        temporal::get_hotspots(self.store.root(), limit, days)
    }

    /// Search recent changes using `git log`.
    ///
    /// Optionally filter by commit message query and/or file path.
    pub fn search_changes(
        &self,
        query: Option<&str>,
        file_filter: Option<&str>,
        limit: usize,
    ) -> Vec<ChangeEntry> {
        temporal::search_changes(self.store.root(), query, file_filter, limit)
    }

    /// Get blame information for a file.
    ///
    /// If `line_start` and `line_end` are provided, restricts to that range.
    pub fn get_blame(
        &self,
        file: &str,
        line_start: Option<u64>,
        line_end: Option<u64>,
    ) -> Vec<BlameLine> {
        temporal::get_blame(self.store.root(), file, line_start, line_end)
    }

    /// Get change frequency and unique authors for a file.
    pub fn file_change_frequency(&self, file: &str, days: u64) -> (usize, Vec<String>) {
        temporal::file_change_frequency(self.store.root(), file, days)
    }
}
