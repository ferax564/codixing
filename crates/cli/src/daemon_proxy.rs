//! Blocking daemon proxy helper for CLI commands.
//!
//! The MCP server (`codixing-mcp`) can run as a background daemon that holds
//! the engine in memory, serving JSON-RPC requests over:
//! - a Unix domain socket at `<root>/.codixing/daemon.sock` (Unix)
//! - a named pipe at `\\.\pipe\codixing-<hash>` (Windows)
//!
//! Once the daemon is warm, tool calls cost ~5-40 ms instead of ~4 s cold
//! process startup on a 2 GB hybrid index (measured on the Linux kernel).
//!
//! ## Flow
//!
//! 1. CLI command calls [`try_tools_call`] with the tool name + arguments.
//! 2. Helper checks for a live daemon endpoint at the platform-specific path.
//! 3. If found, opens a connection, sends an initialize + tools/call pair,
//!    reads responses until it finds the one with `id == 2`, extracts the
//!    text body, returns `Some(text)`.
//! 4. If no daemon, stale endpoint, or any I/O step fails, returns `None`
//!    and the caller falls back to its existing `Engine::open()` path.
//!
//! ## Why blocking std instead of tokio
//!
//! The CLI has no other async work. Pulling in tokio for one code path would
//! more than double the binary's async runtime surface for a pure-sequential
//! request/response loop. Both platforms use blocking stdlib primitives:
//! `std::os::unix::net::UnixStream` on Unix, `std::fs::OpenOptions` on
//! Windows (named pipes behave like bidirectional files once opened).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

/// How long to wait for the daemon to accept a connection before giving up.
/// Kept intentionally short: if the daemon is contended or unhealthy we'd
/// rather run locally than hang.
#[allow(dead_code)]
const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long to wait for the daemon to respond after we've written the
/// request. Larger than connect: some tool calls (graph --map, audit) legit
/// take seconds even warm.
#[allow(dead_code)]
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Attempt to call an MCP tool through the running daemon.
///
/// Returns:
/// - `Some(text)` if the daemon handled the request and returned a text body.
///   The caller should print `text` and treat the command as done.
/// - `None` if the daemon is not running, the endpoint is stale, the tool
///   returned an error, or any I/O step failed. The caller should fall back
///   to its existing in-process path.
pub fn try_tools_call(root: &Path, tool_name: &str, arguments: Value) -> Option<String> {
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
    let request = format!(
        "{}\n{}\n",
        serde_json::to_string(&initialize).ok()?,
        serde_json::to_string(&call).ok()?
    );

    let response_reader = send_jsonrpc(root, request.as_bytes())?;
    parse_response(response_reader)
}

/// Parse the daemon's response stream, looking for the tools/call reply.
fn parse_response<R: BufRead>(mut reader: R) -> Option<String> {
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

// ---------------------------------------------------------------------------
// Unix: connect via domain socket at `<root>/.codixing/daemon.sock`
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn send_jsonrpc(root: &Path, request: &[u8]) -> Option<Box<dyn BufRead + Send>> {
    use std::os::unix::net::UnixStream;

    let socket_path = root.join(".codixing").join("daemon.sock");
    if !socket_path.exists() {
        return None;
    }

    let addr = std::os::unix::net::SocketAddr::from_pathname(&socket_path).ok()?;
    let stream = UnixStream::connect_addr(&addr).ok()?;
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT)).ok()?;

    let mut writer = stream.try_clone().ok()?;
    writer.write_all(request).ok()?;
    // Half-close the write side so the daemon knows no more requests are
    // coming. Without this, the daemon reader blocks forever.
    writer.shutdown(std::net::Shutdown::Write).ok()?;

    Some(Box::new(BufReader::new(stream)))
}

// ---------------------------------------------------------------------------
// Windows: connect via named pipe at `\\.\pipe\codixing-<hash>`
// ---------------------------------------------------------------------------

/// Derive the named-pipe name from the project root, mirroring
/// `crates/mcp/src/daemon_windows.rs::pipe_name_for_root`.
///
/// Must match the server-side hashing or connections silently fail.
#[cfg(windows)]
fn pipe_name_for_root(root: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let hash = hasher.finish();
    format!(r"\\.\pipe\codixing-{hash:016x}")
}

#[cfg(windows)]
fn send_jsonrpc(root: &Path, request: &[u8]) -> Option<Box<dyn BufRead + Send>> {
    use std::fs::OpenOptions;
    use std::io::Read;

    let pipe_name = pipe_name_for_root(root);

    // Windows named pipes are opened like files. If the daemon is not
    // running the path doesn't exist; if it's busy serving another client
    // we get ERROR_PIPE_BUSY and would need to WaitNamedPipe, but since
    // the daemon spawns a fresh pipe instance per connection we usually
    // don't hit that case. Keep it simple: try once, fall back on any
    // error.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&pipe_name)
        .ok()?;

    file.write_all(request).ok()?;
    // No half-close on Windows named pipes — the daemon's JSON-RPC loop
    // handles framing by reading line-by-line, so once it sees both our
    // JSON messages (initialize + tools/call) it will respond. We
    // explicitly read until we find our id==2 reply.

    // Read the full response into a buffer. The daemon writes at most a
    // few KB for our tool calls, and reading into a Vec<u8> lets us close
    // the handle cleanly before parsing.
    let mut buf = Vec::with_capacity(8192);
    // Read with a short overall budget — if the daemon hangs we fall
    // back to in-process rather than blocking the CLI.
    let read_start = std::time::Instant::now();
    let max_wait = READ_TIMEOUT;
    loop {
        let mut chunk = [0u8; 4096];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Stop once we see the id==2 response — it's on its own
                // line so we can search for a newline and try to parse.
                if contains_id2_response(&buf) {
                    break;
                }
                if read_start.elapsed() > max_wait {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }

    Some(Box::new(BufReader::new(std::io::Cursor::new(buf))))
}

/// Fast-path check: has the buffer already received a JSON-RPC response
/// with `"id":2`? Used to avoid waiting for EOF on Windows where the
/// daemon keeps the pipe open after responding.
#[cfg(windows)]
fn contains_id2_response(buf: &[u8]) -> bool {
    let s = std::str::from_utf8(buf).unwrap_or("");
    for line in s.lines() {
        if line.contains("\"id\":2") || line.contains("\"id\": 2") {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Non-Unix, non-Windows platforms: no daemon support
// ---------------------------------------------------------------------------

#[cfg(not(any(unix, windows)))]
fn send_jsonrpc(_root: &Path, _request: &[u8]) -> Option<Box<dyn BufRead + Send>> {
    None
}

// ---------------------------------------------------------------------------
// Per-command convenience wrappers
// ---------------------------------------------------------------------------

/// Convenience wrapper for `code_search` — the highest-traffic CLI path.
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

/// Proxy `codixing symbols <name>` through the daemon's `find_symbol` tool.
pub fn try_symbols(root: &Path, name: &str, file_filter: Option<&str>) -> Option<String> {
    let mut args = serde_json::Map::new();
    args.insert("name".into(), json!(name));
    if let Some(f) = file_filter {
        args.insert("file".into(), json!(f));
    }
    try_tools_call(root, "find_symbol", Value::Object(args))
}

/// Proxy `codixing usages <symbol>` through the daemon's `search_usages` tool.
pub fn try_usages(root: &Path, symbol: &str) -> Option<String> {
    try_tools_call(root, "search_usages", json!({ "symbol": symbol }))
}

/// Proxy `codixing impact <file>` through the daemon's `change_impact` tool.
pub fn try_impact(root: &Path, file: &str) -> Option<String> {
    try_tools_call(root, "change_impact", json!({ "file": file }))
}

/// Proxy `codixing graph --map` through the daemon's `get_repo_map` tool.
pub fn try_repo_map(root: &Path, token_budget: Option<usize>) -> Option<String> {
    let mut args = serde_json::Map::new();
    if let Some(b) = token_budget {
        args.insert("token_budget".into(), json!(b));
    }
    try_tools_call(root, "get_repo_map", Value::Object(args))
}

/// Proxy `codixing callers <file>` through the daemon's `file_callers` tool.
///
/// Returns a newline-separated list of file paths, or `None` if the daemon is
/// not running or does not respond.
pub fn try_callers(root: &Path, file: &str) -> Option<String> {
    try_tools_call(root, "file_callers", json!({ "path": file }))
}

/// Proxy `codixing callees <file>` through the daemon's `file_callees` tool.
///
/// Returns a newline-separated list of file paths, or `None` if the daemon is
/// not running or does not respond.
pub fn try_callees(root: &Path, file: &str) -> Option<String> {
    try_tools_call(root, "file_callees", json!({ "path": file }))
}

/// Proxy `codixing grep <pattern>` through the daemon's `grep_code` tool.
///
/// The daemon handler formats matches itself; on success we hand back the
/// pre-rendered text body. Returns `None` if no daemon is running or the
/// tool call failed — caller should fall back to in-process.
#[allow(clippy::too_many_arguments)]
pub fn try_grep(
    root: &Path,
    pattern: &str,
    literal: bool,
    case_insensitive: bool,
    invert: bool,
    file_glob: Option<&str>,
    before_context: usize,
    after_context: usize,
    count_only: bool,
    files_with_matches: bool,
    limit: usize,
) -> Option<String> {
    let mut args = serde_json::Map::new();
    args.insert("pattern".into(), json!(pattern));
    if literal {
        args.insert("literal".into(), json!(true));
    }
    if case_insensitive {
        args.insert("case_insensitive".into(), json!(true));
    }
    if invert {
        args.insert("invert".into(), json!(true));
    }
    if let Some(g) = file_glob {
        args.insert("file_glob".into(), json!(g));
    }
    if before_context > 0 {
        args.insert("before_context".into(), json!(before_context));
    }
    if after_context > 0 {
        args.insert("after_context".into(), json!(after_context));
    }
    if count_only {
        args.insert("count_only".into(), json!(true));
    }
    if files_with_matches {
        args.insert("files_with_matches".into(), json!(true));
    }
    args.insert("limit".into(), json!(limit));

    try_tools_call(root, "grep_code", Value::Object(args))
}
