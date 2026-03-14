//! Shared constants and helpers for MCP tool handlers.

use std::sync::LazyLock;

/// Regex matching function-call patterns in source code: `identifier(`.
///
/// Captures the function name in group 1.  Callers should filter out
/// language keywords before using the match.
pub(crate) static CALL_PATTERN: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b([a-z_][a-zA-Z0-9_]*)\s*\(").unwrap());
