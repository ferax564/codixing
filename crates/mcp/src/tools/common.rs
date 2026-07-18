//! Shared constants and helpers for MCP tool handlers.

use std::sync::LazyLock;

use serde_json::Value;

use crate::protocol::ProgressNotification;

/// Regex matching function-call patterns in source code: `identifier(`.
///
/// Captures the function name in group 1.  Callers should filter out
/// language keywords before using the match.
pub(crate) static CALL_PATTERN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b([a-z_][a-zA-Z0-9_]*)\s*\(").unwrap());

// ---------------------------------------------------------------------------
// Progress reporting for long-running tool calls
// ---------------------------------------------------------------------------

/// Best-effort progress reporter for long-running tool calls.
///
/// Wraps a bounded `std::sync::mpsc::SyncSender` (not tokio) because tool
/// dispatch runs inside `spawn_blocking`. Full/disconnected channels drop
/// best-effort updates instead of growing memory behind a slow client.
#[derive(Clone)]
pub struct ProgressReporter {
    token: Value,
    sender: std::sync::mpsc::SyncSender<ProgressNotification>,
    total: u32,
}

impl ProgressReporter {
    /// Create a new reporter.
    pub fn new(
        token: Value,
        sender: std::sync::mpsc::SyncSender<ProgressNotification>,
        total: u32,
    ) -> Self {
        Self {
            token,
            sender,
            total,
        }
    }

    /// Send a progress notification.  Best-effort: silently ignores send errors.
    pub fn report(&self, progress: u32, message: &str) {
        let _ = self.sender.try_send(ProgressNotification {
            progress_token: self.token.clone(),
            progress,
            total: self.total,
            message: message.to_string(),
            data: None,
        });
    }

    /// Send a progress notification with attached structured data (e.g. partial
    /// search results).  Best-effort: silently ignores send errors.
    pub fn report_with_data(&self, progress: u32, message: &str, data: Value) {
        let _ = self.sender.try_send(ProgressNotification {
            progress_token: self.token.clone(),
            progress,
            total: self.total,
            message: message.to_string(),
            data: Some(data),
        });
    }
}
