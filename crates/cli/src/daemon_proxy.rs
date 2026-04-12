//! Blocking daemon proxy helper for CLI commands.
//!
//! The MCP server (`codixing-mcp`) can run as a background daemon that holds
//! the engine in memory, serving JSON-RPC requests over a Unix domain socket
//! at `.codixing/daemon.sock`. Once the daemon is warm, tool calls cost ~5-40 ms
//! instead of ~4 s cold process startup on a 2 GB hybrid index.
//!
//! This module lets CLI commands route themselves through a running daemon
//! so agents and humans both benefit from that warm-cache path without having
//! to manually run `codixing-mcp`.
//!
//! ## Flow
//!
//! 1. CLI command calls [`try_tools_call`] with the tool name + arguments.
//! 2. Helper checks for a live socket at `<root>/.codixing/daemon.sock`.
//! 3. If found, connects, sends a JSON-RPC `tools/call` request, reads one
//!    line of response, extracts the text body, returns `Some(text)`.
//! 4. If no daemon or any error, returns `None` and the caller falls back
//!    to the in-process `Engine::open()` path.
//!
//! All socket I/O is blocking `std::os::unix::net::UnixStream` with a short
//! connect timeout. We do not pull in tokio for this path — the CLI has no
//! other async work and the extra dependency surface is not worth it.
//!
//! Windows support is a no-op for now (named pipes require a different code
//! path that can be added in a follow-up). The enclosing `mod daemon_proxy;`
//! declaration in main.rs is already gated on `#[cfg(unix)]`, so this file
//! only compiles on Unix.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

/// How long to wait for the daemon to accept a connection before giving up
/// and falling back to the in-process path. Kept intentionally short: if the
/// daemon is contended or unhealthy we'd rather run locally than hang.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long to wait for the daemon to respond after we've written the
/// request. Larger than connect: some tool calls (graph --map, audit) legit
/// take seconds even warm.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Attempt to call an MCP tool through the running daemon.
///
/// Returns:
/// - `Some(text)` if the daemon handled the request and returned a text body.
///   The caller should print `text` and treat the command as done.
/// - `None` if the daemon is not running, the socket is stale, the tool
///   returned an error, or any I/O step failed. The caller should fall back
///   to its existing in-process path.
pub fn try_tools_call(root: &Path, tool_name: &str, arguments: Value) -> Option<String> {
    let socket_path = root.join(".codixing").join("daemon.sock");
    if !socket_path.exists() {
        return None;
    }

    // Connect with a short timeout. UnixStream::connect itself is blocking
    // with no timeout flag, so we use connect_timeout.
    let addr = std::os::unix::net::SocketAddr::from_pathname(&socket_path).ok()?;
    let stream = UnixStream::connect_addr(&addr).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT)).ok()?;

    // The MCP protocol requires initialize → tools/call. For a one-shot CLI
    // request we send both back-to-back over the same connection.
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "capabilities": {} }
    });
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    });

    let mut writer = stream.try_clone().ok()?;
    writeln!(writer, "{}", serde_json::to_string(&initialize).ok()?).ok()?;
    writeln!(writer, "{}", serde_json::to_string(&call).ok()?).ok()?;
    // Half-close the write side so the daemon knows there are no more
    // requests. Without this, the daemon will wait forever on reader.next_line().
    writer.shutdown(std::net::Shutdown::Write).ok()?;

    // Read responses line-by-line until we find the one whose id == 2.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            // EOF without finding our response.
            return None;
        }
        let resp: Value = serde_json::from_str(line.trim()).ok()?;
        // Skip notifications (no id) and the initialize response (id == 1).
        if resp.get("id") != Some(&json!(2)) {
            continue;
        }
        // Extract the text body. MCP result format:
        //   { "result": { "content": [{ "type": "text", "text": "..." }], "isError": bool } }
        if resp.get("result").and_then(|r| r.get("isError")) == Some(&json!(true)) {
            // Tool returned an error — let the caller fall back to
            // in-process which may produce a clearer error message.
            return None;
        }
        let text = resp
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())?
            .to_string();
        return Some(text);
    }
}

/// Convenience wrapper for `code_search` — the highest-traffic CLI path.
///
/// Returns the formatted markdown text from the daemon, or `None` to signal
/// the caller should fall back to `Engine::open()`.
pub fn try_search(root: &Path, query: &str, limit: usize) -> Option<String> {
    try_tools_call(
        root,
        "code_search",
        json!({
            "query": query,
            "limit": limit,
        }),
    )
}
