//! Blocking daemon proxy helper for CLI commands.
//!
//! The MCP server (`codixing-mcp`) can run as a background daemon that holds
//! the engine in memory, serving JSON-RPC requests over:
//! - a Unix domain socket at `<root>/.codixing/daemon-minimal.sock` (Unix)
//! - a named pipe at `\\.\pipe\codixing-<hash>-minimal` (Windows)
//!
//! Once the daemon is warm, tool calls cost ~5-40 ms instead of ~4 s cold
//! process startup on a 2 GB hybrid index (measured on the Linux kernel).
//!
//! ## Flow
//!
//! 1. CLI command calls [`try_tools_call`] with the tool name + arguments.
//! 2. Helper checks for a live daemon endpoint at the platform-specific path.
//! 3. If found, opens a connection, initializes it, switches that connection
//!    to the read-only `reviewer` profile, then sends the requested tool call.
//!    The daemon itself remains minimal for MCP clients, while all read-only
//!    CLI proxy wrappers can still use the warm engine.
//! 4. Reads responses until it finds the tool call reply, extracts the text
//!    body, and returns `Some(text)`.
//! 5. If no daemon, stale endpoint, or any I/O step fails, returns `None`
//!    and the caller falls back to a direct read-only engine open.
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

const DEFAULT_DAEMON_PROFILE: &str = "minimal";
const REVIEWER_PROFILE: &str = "reviewer";
const INITIALIZE_RESPONSE_ID: u64 = 1;
const PROFILE_RESPONSE_ID: u64 = 2;
const CALL_RESPONSE_ID: u64 = 3;

/// Attempt to call an MCP tool through the running daemon.
///
/// Returns:
/// - `Some(text)` if the daemon handled the request and returned a text body.
///   The caller should print `text` and treat the command as done.
/// - `None` if the daemon is not running, the endpoint is stale, the tool
///   returned an error, or any I/O step failed. The caller should fall back
///   to its existing in-process path.
pub fn try_tools_call(root: &Path, tool_name: &str, arguments: Value) -> Option<String> {
    if daemon_disabled() {
        return None;
    }
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": INITIALIZE_RESPONSE_ID,
        "method": "initialize",
        "params": { "capabilities": {} }
    });
    let select_reviewer = json!({
        "jsonrpc": "2.0",
        "id": PROFILE_RESPONSE_ID,
        "method": "tools/call",
        "params": {
            "name": "set_mcp_profile",
            "arguments": { "profile": REVIEWER_PROFILE },
        }
    });
    let call = json!({
        "jsonrpc": "2.0",
        "id": CALL_RESPONSE_ID,
        "method": "tools/call",
        "params": {
            "name": tool_name,
            "arguments": arguments,
        }
    });
    let request = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&initialize).ok()?,
        serde_json::to_string(&select_reviewer).ok()?,
        serde_json::to_string(&call).ok()?
    );

    let response_reader = send_jsonrpc(root, request.as_bytes())?;
    parse_response(response_reader)
}

/// `CODIXING_NO_DAEMON=1`: answer every command in-process and never start
/// a daemon. For benchmarks and bug reports, where the result must not
/// depend on what an earlier command left running.
pub fn daemon_disabled() -> bool {
    std::env::var_os("CODIXING_NO_DAEMON").is_some_and(|value| !value.is_empty() && value != "0")
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
        // A daemon left running by another release ranks and formats with
        // that release's code, and may not understand this release's index.
        // Answer in-process instead; the old daemon exits when idle.
        if resp.get("id") == Some(&json!(INITIALIZE_RESPONSE_ID))
            && let Some(version) = resp
                .get("result")
                .and_then(|r| r.get("serverInfo"))
                .and_then(|info| info.get("version"))
                .and_then(|v| v.as_str())
            && version != env!("CARGO_PKG_VERSION")
        {
            return None;
        }
        // Skip notifications plus initialize/profile-switch responses.
        if resp.get("id") != Some(&json!(CALL_RESPONSE_ID)) {
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
// Unix: connect via domain socket at `<root>/.codixing/daemon-<profile>.sock`
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn send_jsonrpc(root: &Path, request: &[u8]) -> Option<Box<dyn BufRead + Send>> {
    use std::os::unix::net::UnixStream;

    let socket_path = default_daemon_endpoint(root);
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

#[cfg(unix)]
pub(crate) fn default_daemon_endpoint(root: &Path) -> std::path::PathBuf {
    // Same derivation as the daemon (`codixing-mcp`): the index directory
    // when the path is short enough to bind, else a per-user runtime path.
    codixing_core::process::daemon_socket_path(
        root,
        &format!("daemon-{DEFAULT_DAEMON_PROFILE}.sock"),
    )
}

// ---------------------------------------------------------------------------
// Windows: connect via named pipe at `\\.\pipe\codixing-<hash>-<profile>`
// ---------------------------------------------------------------------------

/// Derive the named-pipe name from the project root, mirroring
/// `crates/mcp/src/daemon_windows.rs::pipe_name_for_root`.
///
/// Must match the server-side hashing or connections silently fail.
#[cfg(windows)]
fn pipe_name_for_root(root: &Path) -> String {
    let hash = stable_root_hash(root);
    format!(r"\\.\pipe\codixing-{hash:016x}-{DEFAULT_DAEMON_PROFILE}")
}

#[cfg(windows)]
pub(crate) fn default_daemon_endpoint(root: &Path) -> String {
    pipe_name_for_root(root)
}

#[cfg(windows)]
fn stable_root_hash(root: &Path) -> u64 {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hash = 0xcbf29ce484222325u64;
    for byte in canonical.to_string_lossy().to_ascii_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(windows)]
fn send_jsonrpc(root: &Path, request: &[u8]) -> Option<Box<dyn BufRead + Send>> {
    use std::fs::OpenOptions;
    use std::io::Read;

    let pipe_name = default_daemon_endpoint(root);

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
    // explicitly read until we find our tool-call reply.

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
                // Stop once we see the tool-call response — it's on its own
                // line so we can search for a newline and try to parse.
                if contains_call_response(&buf) {
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
/// with the final call id? Used to avoid waiting for EOF on Windows where the
/// daemon keeps the pipe open after responding.
#[cfg_attr(not(windows), allow(dead_code))]
fn contains_call_response(buf: &[u8]) -> bool {
    let s = std::str::from_utf8(buf).unwrap_or("");
    for line in s.split_inclusive('\n') {
        // A named-pipe read can split a large JSON response immediately after
        // the id. Do not stop until the complete newline-delimited frame has
        // arrived and parsed successfully.
        if !line.ends_with('\n') {
            continue;
        }
        let Ok(response) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if response.get("id") == Some(&json!(CALL_RESPONSE_ID)) {
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
            // Same one-line-per-hit rendering as the in-process path.
            "compact": true,
        }),
    )
}

/// Proxy `codixing ask` through the daemon's `agent_context_pack` tool.
///
/// The daemon renders with the same core code as the in-process command.
/// `token_budget` is the budget of the pack itself; the tool reserves a
/// quarter of its own budget for the response envelope, so it is scaled up
/// here to give the pack the same room either way.
#[allow(clippy::too_many_arguments)]
pub fn try_ask(
    root: &Path,
    task: &str,
    mode: &str,
    token_budget: usize,
    changed_files: &[String],
    branch: Option<&str>,
    risk_level: Option<&str>,
    json_output: bool,
) -> Option<String> {
    let mut args = serde_json::Map::new();
    args.insert("task".into(), json!(task));
    args.insert("mode".into(), json!(mode));
    args.insert(
        "token_budget".into(),
        json!(token_budget.saturating_mul(4).div_ceil(3)),
    );
    args.insert(
        "format".into(),
        json!(if json_output { "json" } else { "text" }),
    );
    if !changed_files.is_empty() {
        args.insert("changed_files".into(), json!(changed_files));
    }
    if let Some(branch) = branch {
        args.insert("branch".into(), json!(branch));
    }
    if let Some(risk_level) = risk_level {
        args.insert("risk_level".into(), json!(risk_level));
    }
    try_tools_call(root, "agent_context_pack", Value::Object(args))
}

// ---------------------------------------------------------------------------
// Daemon lifecycle (Unix)
// ---------------------------------------------------------------------------

/// Indexes with at least this many files get a background daemon started
/// automatically: below it a cold process is fast enough, above it every
/// command pays seconds to reopen the index.
pub const AUTO_DAEMON_MIN_FILES: usize = 20_000;

/// Whether a daemon for `root` is accepting connections.
#[cfg(unix)]
pub fn daemon_is_alive(root: &Path) -> bool {
    let socket_path = default_daemon_endpoint(root);
    socket_path.exists() && std::os::unix::net::UnixStream::connect(&socket_path).is_ok()
}

#[cfg(not(unix))]
pub fn daemon_is_alive(_root: &Path) -> bool {
    false
}

/// Locate the `codixing-mcp` binary: next to this executable, else on `PATH`.
#[cfg_attr(not(unix), allow(dead_code))]
fn find_mcp_binary() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) {
        "codixing-mcp.exe"
    } else {
        "codixing-mcp"
    };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(name);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Why a daemon could not be started.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum DaemonStartError {
    /// `codixing-mcp` was found neither next to the CLI nor on `PATH`.
    BinaryNotFound,
    /// Spawning the process failed.
    Spawn(std::io::Error),
    /// Background daemons are only managed on Unix.
    #[cfg(not(unix))]
    Unsupported,
}

impl std::fmt::Display for DaemonStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BinaryNotFound => write!(
                f,
                "codixing-mcp not found next to the codixing binary or on PATH"
            ),
            Self::Spawn(error) => write!(f, "failed to start codixing-mcp: {error}"),
            #[cfg(not(unix))]
            Self::Unsupported => write!(f, "background daemons are managed on Unix only"),
        }
    }
}

/// Start a detached read-only daemon for `root`.
///
/// The daemon keeps the index in memory, serves CLI commands over its
/// socket, follows index updates, and exits by itself after 30 idle minutes.
/// Returns once the process is spawned; it needs as long as a cold index
/// open before it accepts connections.
#[cfg(unix)]
pub fn spawn_daemon(root: &Path) -> Result<(), DaemonStartError> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let binary = find_mcp_binary().ok_or(DaemonStartError::BinaryNotFound)?;
    // Own process group: the daemon must outlive this command and must not
    // receive the terminal's Ctrl-C.
    Command::new(binary)
        .arg("--root")
        .arg(root)
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(|_| ())
        .map_err(DaemonStartError::Spawn)
}

#[cfg(not(unix))]
pub fn spawn_daemon(_root: &Path) -> Result<(), DaemonStartError> {
    Err(DaemonStartError::Unsupported)
}

/// Start a daemon in the background when the index is large and none is
/// running, so the *next* command is served warm.
///
/// Never blocks and never fails the calling command. Opt out with
/// `CODIXING_NO_AUTO_DAEMON=1`. A start marker keeps concurrent commands from
/// each launching their own daemon while the first one is still loading.
pub fn autostart_daemon_if_large(root: &Path, indexed_files: usize) {
    if indexed_files < AUTO_DAEMON_MIN_FILES
        || daemon_disabled()
        || std::env::var_os("CODIXING_NO_AUTO_DAEMON").is_some_and(|v| !v.is_empty() && v != "0")
        || daemon_is_alive(root)
    {
        return;
    }

    // One start attempt per minute at most.
    let marker = root.join(".codixing").join("daemon-start.marker");
    let recently_started = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < std::time::Duration::from_secs(60));
    if recently_started {
        return;
    }
    if std::fs::write(&marker, b"").is_err() {
        return;
    }

    if spawn_daemon(root).is_ok() {
        eprintln!(
            "codixing: large index ({indexed_files} files) — started a background daemon so the next commands answer in milliseconds. It exits after 30 idle minutes; set CODIXING_NO_AUTO_DAEMON=1 to disable."
        );
    }
}

/// Proxy `codixing symbols <name>` through the daemon's `find_symbol` tool.
pub fn try_symbols(
    root: &Path,
    name: &str,
    file_filter: Option<&str>,
    include_imports: bool,
    limit: usize,
) -> Option<String> {
    let mut args = serde_json::Map::new();
    args.insert("name".into(), json!(name));
    args.insert("include_imports".into(), json!(include_imports));
    args.insert("limit".into(), json!(limit));
    // Bare listing, exactly as the in-process command prints it.
    args.insert("cli".into(), json!(true));
    if let Some(f) = file_filter {
        args.insert("file".into(), json!(f));
    }
    try_tools_call(root, "find_symbol", Value::Object(args))
}

/// Proxy `codixing usages <symbol>` through the daemon's `search_usages` tool.
pub fn try_usages(root: &Path, symbol: &str, limit: usize) -> Option<String> {
    try_tools_call(
        root,
        "search_usages",
        // Bare listing, exactly as the in-process command prints it.
        json!({ "symbol": symbol, "limit": limit, "cli": true }),
    )
}

/// Proxy `codixing impact <file>` through the daemon's `change_impact` tool.
pub fn try_impact(root: &Path, file: &str, full: bool) -> Option<String> {
    try_tools_call(root, "change_impact", json!({ "file": file, "full": full }))
}

/// Proxy `codixing graph --map` through the daemon's `get_repo_map` tool.
pub fn try_repo_map(root: &Path, token_budget: Option<usize>) -> Option<String> {
    let mut args = serde_json::Map::new();
    if let Some(b) = token_budget {
        args.insert("token_budget".into(), json!(b));
    }
    try_tools_call(root, "get_repo_map", Value::Object(args))
}

/// Proxy `codixing context <file> --line N` through the daemon's
/// `assemble_location_context` tool. Both sides render with the same core
/// formatter, so the text is identical to the in-process command.
pub fn try_context(
    root: &Path,
    file: &str,
    line: u64,
    token_budget: usize,
    budget_mode: &str,
) -> Option<String> {
    try_tools_call(
        root,
        "assemble_location_context",
        json!({
            "file": file,
            "line": line,
            "token_budget": token_budget,
            "budget_mode": budget_mode,
        }),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_proxy_targets_the_minimal_default_profile() {
        assert_eq!(DEFAULT_DAEMON_PROFILE, "minimal");

        #[cfg(unix)]
        assert!(
            default_daemon_endpoint(Path::new("/tmp/project"))
                .ends_with(".codixing/daemon-minimal.sock")
        );
    }

    #[test]
    fn proxy_switches_connection_to_reviewer_before_read_only_call() {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": {} }
        });
        let select_reviewer = json!({
            "jsonrpc": "2.0",
            "id": PROFILE_RESPONSE_ID,
            "method": "tools/call",
            "params": {
                "name": "set_mcp_profile",
                "arguments": { "profile": REVIEWER_PROFILE },
            }
        });
        let call = json!({
            "jsonrpc": "2.0",
            "id": CALL_RESPONSE_ID,
            "method": "tools/call",
            "params": {
                "name": "search_usages",
                "arguments": { "symbol": "Engine" },
            }
        });

        let request = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&initialize).unwrap(),
            serde_json::to_string(&select_reviewer).unwrap(),
            serde_json::to_string(&call).unwrap()
        );
        let messages: Vec<Value> = request
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["params"]["name"], "set_mcp_profile");
        assert_eq!(messages[1]["params"]["arguments"]["profile"], "reviewer");
        assert_eq!(messages[2]["id"], CALL_RESPONSE_ID);
        assert_eq!(messages[2]["params"]["name"], "search_usages");
    }

    #[test]
    fn parser_ignores_profile_response_and_returns_final_call() {
        let responses = format!(
            "{}\n{}\n{}\n",
            json!({"jsonrpc":"2.0","id":1,"result":{}}),
            json!({"jsonrpc":"2.0","id":PROFILE_RESPONSE_ID,"result":{"content":[{"type":"text","text":"reviewer"}]}}),
            json!({"jsonrpc":"2.0","id":CALL_RESPONSE_ID,"result":{"content":[{"type":"text","text":"warm result"}],"isError":false}})
        );

        assert_eq!(
            parse_response(std::io::Cursor::new(responses)),
            Some("warm result".to_string())
        );
    }

    #[test]
    fn call_response_detector_waits_for_complete_json_line() {
        let response = format!(
            "{}\n",
            json!({
                "jsonrpc": "2.0",
                "id": CALL_RESPONSE_ID,
                "result": {"content": [{"type": "text", "text": "x".repeat(8192)}]}
            })
        );
        let split = response.find("\"result\"").unwrap();

        assert!(response[..split].contains(&format!("\"id\":{CALL_RESPONSE_ID}")));
        assert!(
            !contains_call_response(&response.as_bytes()[..split]),
            "an id in an incomplete pipe frame must not terminate the read"
        );
        assert!(contains_call_response(response.as_bytes()));
    }
}
