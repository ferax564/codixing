use std::time::Duration;

/// Timing and throughput statistics produced by an embedding run.
#[derive(Debug, Clone)]
pub struct EmbedTimingStats {
    /// Total number of chunks that were embedded.
    pub embedded_chunks: usize,
    /// Total number of source files processed.
    pub total_files: usize,
    /// Wall-clock time from start to finish of the embedding pass.
    pub wall_clock: Duration,
    /// Number of parallel embedding workers used (1 for the sync path).
    pub workers: usize,
    /// Number of files that used late chunking (whole-file transformer pass).
    pub late_chunking_files: usize,
    /// Number of files that fell back to independent per-chunk embedding.
    pub fallback_files: usize,
}

impl EmbedTimingStats {
    /// Embedding throughput in chunks per second.
    pub fn chunks_per_sec(&self) -> f64 {
        if self.wall_clock.as_secs_f64() > 0.0 {
            self.embedded_chunks as f64 / self.wall_clock.as_secs_f64()
        } else {
            0.0
        }
    }

    /// Fraction of files that used late chunking (0.0–1.0).
    pub fn late_chunking_rate(&self) -> f64 {
        if self.total_files > 0 {
            self.late_chunking_files as f64 / self.total_files as f64
        } else {
            0.0
        }
    }

    /// Serialize to a JSON value for `--json` output.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "embedded_chunks": self.embedded_chunks,
            "total_files": self.total_files,
            "wall_clock_secs": self.wall_clock.as_secs_f64(),
            "chunks_per_sec": self.chunks_per_sec(),
            "workers": self.workers,
            "late_chunking_files": self.late_chunking_files,
            "fallback_files": self.fallback_files,
            "late_chunking_rate": self.late_chunking_rate(),
        })
    }
}
