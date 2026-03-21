#![recursion_limit = "512"]
//! Codixing MCP server — exposes code search and graph tools via the
//! Model Context Protocol (JSON-RPC 2.0 over stdin/stdout or Unix socket).
//!
//! **Daemon mode** (`--daemon`):
//!   Loads the engine once and serves it over a Unix domain socket at
//!   `.codixing/daemon.sock`. Subsequent `codixing-mcp` invocations
//!   detect the live socket and proxy their stdin/stdout through it,
//!   making per-call latency ~1 ms instead of ~30 ms.
//!
//! **Normal mode** (no flag):
//!   Checks for a live daemon socket first. If found, proxies all traffic
//!   to it. If not, falls back to loading the engine directly (existing
//!   behaviour, also triggers auto-init if no index exists yet).
//!
//! **Logging**: always directed to *stderr* — stdout is the JSON-RPC channel.

mod protocol;
mod tools;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
#[cfg(unix)]
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use codixing_core::{
    EmbeddingConfig, Engine, FederatedEngine, FederationConfig, IndexConfig, SessionState,
};

use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

// ---------------------------------------------------------------------------
// Tool listing mode
// ---------------------------------------------------------------------------

/// Controls which tools are returned by `tools/list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListingMode {
    /// Return all tools (~48 definitions, ~6600 tokens).
    Full,
    /// Return a curated subset of ~15 most-used tools.
    /// All tools remain callable via `tools/call`.
    Medium,
    /// Return only the 2 meta-tools (`search_tools`, `get_tool_schema`).
    /// All tools remain callable via `tools/call`.
    Compact,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Codixing MCP server — JSON-RPC 2.0 over stdin/stdout (or Unix socket in daemon mode).
#[derive(Parser)]
#[command(name = "codixing-mcp", version, about)]
struct Args {
    /// Root directory containing the `.codixing` index (created by `codixing init`).
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Start in daemon mode: load the engine once, listen on
    /// `.codixing/daemon.sock`, and serve multiple clients concurrently.
    /// Subsequent `codixing-mcp` invocations will auto-proxy through this socket.
    #[arg(long)]
    daemon: bool,

    /// Path to the Unix socket used by daemon mode.
    /// Defaults to `<root>/.codixing/daemon.sock`.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Disable session tracking entirely. When set, no session events are
    /// recorded and no session-based search boosting is applied.
    #[arg(long)]
    no_session: bool,

    /// Enable compact tool listing: `tools/list` returns only the 2 meta-tools
    /// (`search_tools`, `get_tool_schema`) instead of all tools. All tools remain
    /// callable via `tools/call`. Reduces initial token usage by ~90%.
    #[arg(long, conflicts_with = "medium")]
    compact: bool,

    /// Enable medium tool listing: `tools/list` returns a curated set of ~15
    /// most-used tools instead of all tools. All tools remain callable via
    /// `tools/call`. Useful for MCP clients that cannot do dynamic tool
    /// discovery (e.g. Codex CLI).
    #[arg(long, conflicts_with = "compact")]
    medium: bool,

    /// Path to a `codixing-federation.json` config file for cross-repo
    /// federation.  When provided, a `FederatedEngine` is created alongside
    /// the primary engine, enabling the `list_projects` tool and federated
    /// search across multiple indexed projects.
    #[arg(long)]
    federation: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("root path not found: {}", args.root.display()))?;

    #[cfg(unix)]
    let socket_path = args
        .socket
        .unwrap_or_else(|| root.join(".codixing/daemon.sock"));

    let listing_mode = if args.compact {
        ListingMode::Compact
    } else if args.medium {
        ListingMode::Medium
    } else {
        ListingMode::Full
    };

    // Optionally load a federated engine for cross-repo search.
    let federation: Option<Arc<FederatedEngine>> = match &args.federation {
        Some(config_path) => {
            let cfg = FederationConfig::load(config_path).with_context(|| {
                format!(
                    "failed to load federation config from {}",
                    config_path.display()
                )
            })?;
            let fed =
                FederatedEngine::new(cfg).with_context(|| "failed to create FederatedEngine")?;
            info!(
                "federation enabled — {} project(s) registered",
                fed.projects().len()
            );
            Some(Arc::new(fed))
        }
        None => None,
    };

    if args.daemon {
        // ── Daemon mode (Unix only -- requires Unix sockets) ──────────────
        #[cfg(not(unix))]
        {
            anyhow::bail!(
                "daemon mode requires Unix sockets and is not available on Windows. Use stdin/stdout mode instead."
            );
        }
        #[cfg(unix)]
        {
            let mut engine = load_engine(&root).await?;
            if args.no_session {
                engine.set_session(Arc::new(SessionState::new(false)));
            }
            let engine = Arc::new(RwLock::new(engine));
            run_daemon(engine, &socket_path, listing_mode, federation).await
        }
    } else {
        // ── Normal mode: try proxy, fall back to direct ────────────────────
        #[cfg(unix)]
        if socket_alive(&socket_path).await {
            info!(socket = %socket_path.display(), "daemon detected — proxying through socket");
            return run_proxy(&socket_path).await;
        }
        {
            let mut engine = load_engine(&root).await?;
            if args.no_session {
                engine.set_session(Arc::new(SessionState::new(false)));
            }
            let engine = Arc::new(RwLock::new(engine));
            info!("Codixing MCP server ready — listening on stdin");
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();
            run_jsonrpc_loop(
                engine,
                BufReader::new(stdin).lines(),
                BufWriter::new(stdout),
                listing_mode,
                federation,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Engine loader (shared by daemon + direct modes)
// ---------------------------------------------------------------------------

async fn load_engine(root: &Path) -> Result<Engine> {
    if Engine::index_exists(root) {
        info!(root = %root.display(), "opening existing Codixing index");
        let engine = Engine::open(root).with_context(|| {
            format!(
                "failed to open index at {} — index may be corrupt; \
                 delete .codixing/ and restart to rebuild",
                root.display()
            )
        })?;
        if engine.is_read_only() {
            warn!(
                "engine opened in read-only mode — another instance holds the write lock; \
                 search tools work, write tools (edit_file, write_file, etc.) will return errors"
            );
        }
        Ok(engine)
    } else {
        info!(
            root = %root.display(),
            "no .codixing/ index found — running automatic BM25-only init"
        );
        let mut config = IndexConfig::new(root);
        config.embedding = EmbeddingConfig {
            enabled: false,
            ..EmbeddingConfig::default()
        };
        Engine::init(root, config).with_context(|| {
            format!(
                "auto-init failed at {} — ensure the directory exists and contains source files",
                root.display()
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Daemon: Unix socket server (Unix only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn run_daemon(
    engine: Arc<RwLock<Engine>>,
    socket_path: &Path,
    listing_mode: ListingMode,
    federation: Option<Arc<FederatedEngine>>,
) -> Result<()> {
    // Remove stale socket file if it exists.
    if socket_path.exists() {
        std::fs::remove_file(socket_path).ok();
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind daemon socket at {}", socket_path.display()))?;

    // Remove the socket file on process exit.
    let socket_path_owned = socket_path.to_path_buf();
    let _guard = SocketGuard(socket_path_owned);

    info!(socket = %socket_path.display(), "daemon listening");

    // Spawn a background task that watches the project directory and keeps the
    // in-memory engine up to date when source files change.
    //
    // Uses a two-level debounce strategy:
    // 1. The FileWatcher's internal 500ms debounce coalesces rapid filesystem
    //    events (e.g. editor auto-save, formatter runs).
    // 2. This loop adds a secondary 500ms settlement window: after receiving
    //    events, it keeps polling for more events for 500ms before acquiring
    //    the write lock. This further batches multi-file operations like
    //    `git checkout` and reduces the number of write-lock acquisitions
    //    (which block search queries).
    let engine_for_watch = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        let config = engine_for_watch
            .read()
            .expect("engine lock poisoned")
            .config()
            .clone();

        let watcher = match codixing_core::watcher::FileWatcher::new(&config.root, &config) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "daemon: failed to start file watcher — index will not auto-update");
                return;
            }
        };

        info!(root = %config.root.display(), "daemon: file watcher started");

        loop {
            // Poll with a 2-second timeout so the thread isn't pinned at 100% CPU.
            let changes = watcher.poll_changes(Duration::from_secs(2));
            if changes.is_empty() {
                continue;
            }

            // Secondary settlement window: keep collecting events for up to
            // 500ms to batch multi-file operations. This reduces write-lock
            // acquisitions during rapid editing or VCS operations.
            let mut all_changes = changes;
            let settle_deadline = std::time::Instant::now() + Duration::from_millis(500);
            loop {
                let remaining =
                    settle_deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let more = watcher.poll_changes(remaining);
                if more.is_empty() {
                    break;
                }
                all_changes.extend(more);
            }

            // Deduplicate: if the same path appears multiple times, keep the
            // last occurrence (latest state).
            {
                let mut seen = std::collections::HashSet::new();
                all_changes.retain(|c| seen.insert(c.path.clone()));
            }

            info!(
                count = all_changes.len(),
                "daemon: file changes detected, updating index"
            );
            let mut eng = engine_for_watch.write().expect("engine lock poisoned");
            if let Err(e) = eng.apply_changes(&all_changes) {
                warn!(error = %e, "daemon: apply_changes failed");
            }
            if let Err(e) = eng.save() {
                warn!(error = %e, "daemon: save after watcher update failed");
            }
        }
    });

    loop {
        let (stream, _addr) = listener.accept().await.context("daemon: accept failed")?;

        let engine_clone = Arc::clone(&engine);
        let fed_clone = federation.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_socket_connection(stream, engine_clone, listing_mode, fed_clone).await
            {
                warn!(error = %e, "daemon: connection error");
            }
        });
    }
}

/// Handle one client connection: run a JSON-RPC loop over the socket stream.
#[cfg(unix)]
async fn handle_socket_connection(
    stream: UnixStream,
    engine: Arc<RwLock<Engine>>,
    listing_mode: ListingMode,
    federation: Option<Arc<FederatedEngine>>,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    run_jsonrpc_loop(
        engine,
        BufReader::new(read_half).lines(),
        BufWriter::new(write_half),
        listing_mode,
        federation,
    )
    .await
}

/// RAII guard that removes the socket file when dropped.
#[cfg(unix)]
struct SocketGuard(PathBuf);
#[cfg(unix)]
impl Drop for SocketGuard {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

// ---------------------------------------------------------------------------
// Proxy: pipe stdin/stdout through an existing daemon socket (Unix only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn run_proxy(socket_path: &Path) -> Result<()> {
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("failed to connect to daemon at {}", socket_path.display()))?;

    // Use into_split() so we can call shutdown() on the write half.
    let (mut sock_read, mut sock_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // Forward stdin → socket, then half-close the write side so the daemon
    // gets EOF and knows no more requests are coming.
    let to_socket = async {
        tokio::io::copy(&mut stdin, &mut sock_write)
            .await
            .context("proxy: stdin→socket copy failed")?;
        sock_write
            .shutdown()
            .await
            .context("proxy: socket write shutdown failed")
    };

    // Forward socket → stdout until the daemon closes its end (after getting
    // our EOF and flushing all pending responses).
    let from_socket = async {
        tokio::io::copy(&mut sock_read, &mut stdout)
            .await
            .context("proxy: socket→stdout copy failed")
    };

    // Run both directions concurrently; from_socket will complete naturally
    // once to_socket shuts down the write half and the daemon closes.
    tokio::try_join!(to_socket, from_socket)?;
    Ok(())
}

/// Return true if the Unix socket at `path` accepts connections within 100 ms.
///
/// Uses `Ok(Ok(_))` matching to distinguish a live daemon from a stale socket
/// file: a "Connection refused" error returns `Ok(Err(_))` which `.is_ok()`
/// would incorrectly treat as alive.
#[cfg(unix)]
async fn socket_alive(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    matches!(
        tokio::time::timeout(Duration::from_millis(100), UnixStream::connect(path)).await,
        Ok(Ok(_))
    )
}

// ---------------------------------------------------------------------------
// Core JSON-RPC message loop (generic over any AsyncRead + AsyncWrite)
// ---------------------------------------------------------------------------

async fn run_jsonrpc_loop<R, W>(
    engine: Arc<RwLock<Engine>>,
    mut reader: tokio::io::Lines<BufReader<R>>,
    mut writer: BufWriter<W>,
    listing_mode: ListingMode,
    federation: Option<Arc<FederatedEngine>>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Some(line) = reader.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        debug!(line = %line, "received request");

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "failed to parse JSON-RPC request");
                let err = JsonRpcError::internal_error(Value::Null, &format!("Parse error: {e}"));
                write_line(&mut writer, &err).await?;
                continue;
            }
        };

        let id = match req.id.clone() {
            Some(id) => id,
            None => {
                debug!(method = %req.method, "ignoring notification");
                continue;
            }
        };

        let response = dispatch(
            &engine,
            id,
            &req.method,
            req.params,
            listing_mode,
            &federation,
            &mut writer,
        )
        .await;
        write_line(&mut writer, &response).await?;
    }

    info!("client disconnected");
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn dispatch<W>(
    engine: &Arc<RwLock<Engine>>,
    id: Value,
    method: &str,
    params: Option<Value>,
    listing_mode: ListingMode,
    federation: &Option<Arc<FederatedEngine>>,
    writer: &mut BufWriter<W>,
) -> Value
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match method {
        "initialize" => handle_initialize(id, params),
        "initialized" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => handle_tools_list(id, listing_mode, federation.is_some()),
        "tools/call" => handle_tools_call(engine, id, params, federation, writer).await,
        _ => {
            let err = JsonRpcError::method_not_found(id, method);
            serde_json::to_value(err).unwrap_or(Value::Null)
        }
    }
}

fn handle_initialize(id: Value, _params: Option<Value>) -> Value {
    let result = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "codixing", "version": "0.4.0" }
    });
    serde_json::to_value(JsonRpcResponse::new(id, result)).unwrap_or(Value::Null)
}

fn handle_tools_list(id: Value, listing_mode: ListingMode, has_federation: bool) -> Value {
    let tool_defs = match listing_mode {
        ListingMode::Compact => tools::compact_tool_definitions(),
        ListingMode::Medium => {
            let mut defs = tools::medium_tool_definitions();
            // When federation is active, include the list_projects tool.
            if has_federation {
                if let Some(arr) = defs.as_array_mut() {
                    arr.push(tools::list_projects_tool_definition());
                }
            }
            defs
        }
        ListingMode::Full => tools::tool_definitions_with_federation(has_federation),
    };
    let result = json!({ "tools": tool_defs });
    serde_json::to_value(JsonRpcResponse::new(id, result)).unwrap_or(Value::Null)
}

async fn handle_tools_call<W>(
    engine: &Arc<RwLock<Engine>>,
    id: Value,
    params: Option<Value>,
    federation: &Option<Arc<FederatedEngine>>,
    writer: &mut BufWriter<W>,
) -> Value
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let params = match params {
        Some(p) => p,
        None => {
            let err = JsonRpcError::invalid_params(id, "tools/call requires params");
            return serde_json::to_value(err).unwrap_or(Value::Null);
        }
    };

    let tool_name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            let err = JsonRpcError::invalid_params(id, "missing 'name' in tools/call params");
            return serde_json::to_value(err).unwrap_or(Value::Null);
        }
    };

    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    let engine_arc = Arc::clone(engine);
    let tool_name_clone = tool_name.clone();
    let read_only = tools::is_read_only_tool(&tool_name);
    let fed_clone = federation.clone();

    // Create a progress channel if the caller provided a progressToken in _meta.
    // Per MCP spec, the progressToken comes from `params._meta.progressToken`
    // on each tools/call request, not from initialize capabilities.
    let caller_progress_token = params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let progress_reporter = if let Some(token) = caller_progress_token {
        let (tx, rx) = std::sync::mpsc::channel();
        let reporter = tools::ProgressReporter::new(token, tx, 100);

        // Spawn a task that drains the receiver and writes progress
        // notifications to the output stream. We use a tokio::sync::mpsc
        // bridge so we can await in the async world.
        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            while let Ok(notification) = rx.recv() {
                if bridge_tx.send(notification).is_err() {
                    break;
                }
            }
        });

        // Drain any progress notifications that arrived before the tool finished.
        while let Ok(notification) = bridge_rx.try_recv() {
            let json_val = notification.to_json();
            if let Err(e) = futures_lite_write_line(writer, &json_val).await {
                debug!(error = %e, "failed to write progress notification");
            }
        }

        // Keep the bridge_rx alive through the tool call by storing it.
        Some((reporter, bridge_rx))
    } else {
        None
    };

    let reporter_for_blocking = progress_reporter.as_ref().map(|(r, _)| r.clone());

    let call_result = tokio::task::spawn_blocking(move || {
        let progress_ref = reporter_for_blocking.as_ref();
        if read_only {
            let engine = match engine_arc.read() {
                Ok(e) => e,
                Err(e) => return (format!("Engine lock poisoned: {e}"), true),
            };
            tools::dispatch_tool_ref_with_progress(
                &engine,
                &tool_name_clone,
                &args,
                fed_clone.as_deref(),
                progress_ref,
            )
        } else {
            let mut engine = match engine_arc.write() {
                Ok(e) => e,
                Err(e) => return (format!("Engine lock poisoned: {e}"), true),
            };
            tools::dispatch_tool_with_progress(
                &mut engine,
                &tool_name_clone,
                &args,
                fed_clone.as_deref(),
                progress_ref,
            )
        }
    });

    // While the tool call is running, drain progress notifications from the
    // bridge channel and write them to the output stream.
    if let Some((_, mut bridge_rx)) = progress_reporter {
        tokio::pin!(call_result);
        let call_result = loop {
            tokio::select! {
                result = &mut call_result => break result,
                // This branch drains progress notifications while the tool runs.
                // It can complete when the sender is dropped (tool finished and
                // ProgressReporter was dropped before we polled call_result).
                msg = bridge_rx.recv() => {
                    match msg {
                        Some(notification) => {
                            let json_val = notification.to_json();
                            if let Err(e) = futures_lite_write_line(writer, &json_val).await {
                                debug!(error = %e, "failed to write progress notification");
                            }
                        }
                        None => {
                            // Sender dropped — drain branch completed normally.
                            // Continue the loop; next iteration will pick up
                            // call_result since bridge_rx is exhausted.
                        }
                    }
                }
            }
        };

        // Drain any remaining progress notifications after the tool call finishes.
        bridge_rx.close();
        while let Some(notification) = bridge_rx.recv().await {
            let json_val = notification.to_json();
            let _ = futures_lite_write_line(writer, &json_val).await;
        }

        return build_tool_response(id, tool_name, call_result);
    }

    let call_result = call_result.await;
    build_tool_response(id, tool_name, call_result)
}

/// Write a serialized JSON value as a line to the writer.
async fn futures_lite_write_line<W, T>(writer: &mut BufWriter<W>, value: &T) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: serde::Serialize,
{
    write_line(writer, value).await
}

/// Build the final JSON-RPC response for a tools/call result.
fn build_tool_response(
    id: Value,
    tool_name: String,
    call_result: std::result::Result<(String, bool), tokio::task::JoinError>,
) -> Value {
    let (text, is_error) = match call_result {
        Ok(result) => result,
        Err(e) => {
            error!(tool = %tool_name, error = %e, "spawn_blocking panicked");
            (
                format!("Internal error executing tool '{tool_name}': {e}"),
                true,
            )
        }
    };

    let result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    });

    serde_json::to_value(JsonRpcResponse::new(id, result)).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// I/O helper
// ---------------------------------------------------------------------------

async fn write_line<W, T>(writer: &mut BufWriter<W>, value: &T) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut line = serde_json::to_string(value).context("failed to serialize response")?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .context("failed to write response")?;
    writer.flush().await.context("failed to flush")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, BufReader};

    /// Create a BM25-only engine in a temp directory.
    fn make_test_engine(dir: &Path) -> Engine {
        // Write a small Rust file so the index has something to search.
        let src = dir.join("lib.rs");
        std::fs::write(&src, "pub fn hello() -> &'static str { \"world\" }\n").unwrap();

        let mut config = IndexConfig::new(dir);
        config.embedding = EmbeddingConfig {
            enabled: false,
            ..EmbeddingConfig::default()
        };
        Engine::init(dir, config).expect("engine init should succeed")
    }

    /// Send JSON-RPC request lines into the loop and collect all response lines.
    async fn run_requests(engine: Engine, requests: &[Value]) -> Vec<Value> {
        // Build the request payload (one JSON line per request).
        let mut input = Vec::new();
        for req in requests {
            serde_json::to_writer(&mut input, req).unwrap();
            input.push(b'\n');
        }

        let engine = Arc::new(RwLock::new(engine));

        // Use a duplex channel as the transport.
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_stream);
        let (mut client_read, mut client_write) = tokio::io::split(client_stream);

        // Write all requests then close the write side so the loop sees EOF.
        tokio::spawn(async move {
            client_write.write_all(&input).await.unwrap();
            client_write.shutdown().await.unwrap();
        });

        // Run the JSON-RPC loop on the server side (Full listing mode, no federation for tests).
        let loop_handle = tokio::spawn(async move {
            run_jsonrpc_loop(
                engine,
                BufReader::new(server_read).lines(),
                BufWriter::new(server_write),
                ListingMode::Full,
                None,
            )
            .await
            .unwrap();
        });

        // Read all responses from the server side.
        let mut output = Vec::new();
        client_read.read_to_end(&mut output).await.unwrap();
        loop_handle.await.unwrap();

        // Parse each line as a JSON value.
        output
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("response should be valid JSON"))
            .collect()
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "capabilities": {} }
            })],
        )
        .await;

        assert_eq!(responses.len(), 1);
        let result = &responses[0]["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "codixing");
    }

    #[tokio::test]
    async fn tools_list_returns_tool_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            })],
        )
        .await;

        assert_eq!(responses.len(), 1);
        let tools = responses[0]["result"]["tools"].as_array().unwrap();
        assert!(
            tools.len() >= 10,
            "should have many tools, got {}",
            tools.len()
        );

        // Check that well-known tools exist.
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"code_search"), "missing code_search tool");
        assert!(names.contains(&"find_symbol"), "missing find_symbol tool");
        assert!(names.contains(&"get_repo_map"), "missing get_repo_map tool");
    }

    #[tokio::test]
    async fn tools_call_code_search() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "code_search",
                    "arguments": { "query": "hello", "limit": 5 }
                }
            })],
        )
        .await;

        assert_eq!(responses.len(), 1);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("hello"),
            "search result should contain 'hello', got: {text}"
        );
    }

    #[tokio::test]
    async fn tools_call_find_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "find_symbol",
                    "arguments": { "name": "hello" }
                }
            })],
        )
        .await;

        assert_eq!(responses.len(), 1);
        let result = &responses[0]["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("hello"),
            "find_symbol should locate 'hello', got: {text}"
        );
    }

    #[tokio::test]
    async fn unknown_method_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "nonexistent/method"
            })],
        )
        .await;

        assert_eq!(responses.len(), 1);
        let err = &responses[0]["error"];
        assert_eq!(err["code"], -32601);
        assert!(err["message"].as_str().unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn notification_produces_no_response() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[
                // Notification (no id) — should not produce a response.
                json!({
                    "jsonrpc": "2.0",
                    "method": "initialized"
                }),
                // Normal request to verify the loop still works.
                json!({
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "initialize",
                    "params": {}
                }),
            ],
        )
        .await;

        // Only one response (for the request with id=6).
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0]["id"], 6);
    }

    #[tokio::test]
    async fn tools_call_missing_params_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call"
            })],
        )
        .await;

        assert_eq!(responses.len(), 1);
        let err = &responses[0]["error"];
        assert_eq!(err["code"], -32602);
    }

    #[tokio::test]
    async fn multi_request_session() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests(
            engine,
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": { "capabilities": {} }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "method": "initialized"
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list"
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {
                        "name": "code_search",
                        "arguments": { "query": "hello" }
                    }
                }),
            ],
        )
        .await;

        // 3 responses (initialize, tools/list, tools/call — no response for notification)
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[2]["id"], 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_socket_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());
        let engine = Arc::new(RwLock::new(engine));

        let socket_path = dir.path().join("test_daemon.sock");

        // Start the daemon listener in a background task.
        let engine_clone = Arc::clone(&engine);
        let socket_clone = socket_path.clone();
        let daemon_handle = tokio::spawn(async move {
            let listener = UnixListener::bind(&socket_clone).unwrap();
            // Accept exactly one connection.
            let (stream, _) = listener.accept().await.unwrap();
            handle_socket_connection(stream, engine_clone, ListingMode::Full, None)
                .await
                .unwrap();
        });

        // Give the listener a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect as a client.
        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();

        // Send requests.
        let requests = vec![
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ];
        for req in &requests {
            let mut line = serde_json::to_string(req).unwrap();
            line.push('\n');
            write_half.write_all(line.as_bytes()).await.unwrap();
        }
        // Signal EOF so the daemon's loop exits.
        write_half.shutdown().await.unwrap();

        // Read responses.
        let mut output = Vec::new();
        let mut reader = BufReader::new(read_half);
        reader.read_to_end(&mut output).await.unwrap();

        let responses: Vec<Value> = output
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["result"]["serverInfo"]["name"], "codixing");

        let tools = responses[1]["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 10);

        daemon_handle.await.unwrap();
    }

    // -----------------------------------------------------------------------
    // Progress notification tests
    // -----------------------------------------------------------------------

    /// Helper: send JSON-RPC request lines into the loop and collect ALL output
    /// lines (both responses and progress notifications).
    async fn run_requests_raw(engine: Engine, requests: &[Value]) -> Vec<Value> {
        let mut input = Vec::new();
        for req in requests {
            serde_json::to_writer(&mut input, req).unwrap();
            input.push(b'\n');
        }

        let engine = Arc::new(RwLock::new(engine));

        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_stream);
        let (mut client_read, mut client_write) = tokio::io::split(client_stream);

        tokio::spawn(async move {
            client_write.write_all(&input).await.unwrap();
            client_write.shutdown().await.unwrap();
        });

        let loop_handle = tokio::spawn(async move {
            run_jsonrpc_loop(
                engine,
                BufReader::new(server_read).lines(),
                BufWriter::new(server_write),
                ListingMode::Full,
                None,
            )
            .await
            .unwrap();
        });

        let mut output = Vec::new();
        client_read.read_to_end(&mut output).await.unwrap();
        loop_handle.await.unwrap();

        output
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("output should be valid JSON"))
            .collect()
    }

    #[tokio::test]
    async fn progress_notifications_sent_for_deep_search() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        // Send an initialize, then a deep code_search with a progressToken in _meta.
        let all_output = run_requests_raw(
            engine,
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "capabilities": {}
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "_meta": { "progressToken": "test-progress-1" },
                        "name": "code_search",
                        "arguments": { "query": "hello", "strategy": "deep" }
                    }
                }),
            ],
        )
        .await;

        // There should be at least the 2 responses (initialize + tools/call).
        assert!(
            all_output.len() >= 2,
            "expected at least 2 output lines, got {}",
            all_output.len()
        );

        // Separate progress notifications from responses.
        let progress_msgs: Vec<&Value> = all_output
            .iter()
            .filter(|v| v.get("method").and_then(|m| m.as_str()) == Some("notifications/progress"))
            .collect();

        let responses: Vec<&Value> = all_output
            .iter()
            .filter(|v| v.get("id").is_some())
            .collect();

        // We should have at least one progress notification.
        assert!(
            !progress_msgs.is_empty(),
            "expected progress notifications for deep search, got none. All output: {all_output:?}"
        );

        // Verify progress notification structure.
        for p in &progress_msgs {
            assert_eq!(p["jsonrpc"], "2.0");
            assert!(p["params"]["progressToken"].is_string());
            assert!(p["params"]["progress"].is_number());
            assert!(p["params"]["total"].is_number());
            assert!(p["params"]["message"].is_string());
        }

        // Verify we still got the actual tool response.
        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses (init + tool call)"
        );
        let tool_response = responses[1];
        assert_eq!(tool_response["result"]["isError"], false);
    }

    #[tokio::test]
    async fn no_progress_when_no_progress_token() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        // Initialize, then do a deep search WITHOUT _meta.progressToken.
        let all_output = run_requests_raw(
            engine,
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "capabilities": {}
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "code_search",
                        "arguments": { "query": "hello", "strategy": "deep" }
                    }
                }),
            ],
        )
        .await;

        // Should have exactly 2 output lines (initialize response + tool call response).
        let progress_msgs: Vec<&Value> = all_output
            .iter()
            .filter(|v| v.get("method").and_then(|m| m.as_str()) == Some("notifications/progress"))
            .collect();

        assert!(
            progress_msgs.is_empty(),
            "expected no progress notifications without progressToken, got: {progress_msgs:?}"
        );

        let responses: Vec<&Value> = all_output
            .iter()
            .filter(|v| v.get("id").is_some())
            .collect();
        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses (init + tool call)"
        );
    }
}
