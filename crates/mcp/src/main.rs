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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use codixing_core::{EmbeddingConfig, Engine, IndexConfig, SessionState};

use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

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

    let socket_path = args
        .socket
        .unwrap_or_else(|| root.join(".codixing/daemon.sock"));

    if args.daemon {
        // ── Daemon mode ────────────────────────────────────────────────────
        let mut engine = load_engine(&root).await?;
        if args.no_session {
            engine.set_session(Arc::new(SessionState::new(false)));
        }
        let engine = Arc::new(Mutex::new(engine));
        run_daemon(engine, &socket_path).await
    } else {
        // ── Normal mode: try proxy, fall back to direct ────────────────────
        if socket_alive(&socket_path).await {
            info!(socket = %socket_path.display(), "daemon detected — proxying through socket");
            run_proxy(&socket_path).await
        } else {
            let mut engine = load_engine(&root).await?;
            if args.no_session {
                engine.set_session(Arc::new(SessionState::new(false)));
            }
            let engine = Arc::new(Mutex::new(engine));
            info!("Codixing MCP server ready — listening on stdin");
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();
            run_jsonrpc_loop(
                engine,
                BufReader::new(stdin).lines(),
                BufWriter::new(stdout),
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
        Engine::open(root).with_context(|| {
            format!(
                "failed to open index at {} — index may be corrupt; \
                 delete .codixing/ and restart to rebuild",
                root.display()
            )
        })
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
// Daemon: Unix socket server
// ---------------------------------------------------------------------------

async fn run_daemon(engine: Arc<Mutex<Engine>>, socket_path: &Path) -> Result<()> {
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
    let engine_for_watch = Arc::clone(&engine);
    tokio::task::spawn_blocking(move || {
        let config = engine_for_watch
            .lock()
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

            info!(
                count = changes.len(),
                "daemon: file changes detected, updating index"
            );
            let mut eng = engine_for_watch.lock().expect("engine lock poisoned");
            if let Err(e) = eng.apply_changes(&changes) {
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
        tokio::spawn(async move {
            if let Err(e) = handle_socket_connection(stream, engine_clone).await {
                warn!(error = %e, "daemon: connection error");
            }
        });
    }
}

/// Handle one client connection: run a JSON-RPC loop over the socket stream.
async fn handle_socket_connection(stream: UnixStream, engine: Arc<Mutex<Engine>>) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    run_jsonrpc_loop(
        engine,
        BufReader::new(read_half).lines(),
        BufWriter::new(write_half),
    )
    .await
}

/// RAII guard that removes the socket file when dropped.
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

// ---------------------------------------------------------------------------
// Proxy: pipe stdin/stdout through an existing daemon socket
// ---------------------------------------------------------------------------

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
    engine: Arc<Mutex<Engine>>,
    mut reader: tokio::io::Lines<BufReader<R>>,
    mut writer: BufWriter<W>,
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

        let response = dispatch(&engine, id, &req.method, req.params).await;
        write_line(&mut writer, &response).await?;
    }

    info!("client disconnected");
    Ok(())
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn dispatch(
    engine: &Arc<Mutex<Engine>>,
    id: Value,
    method: &str,
    params: Option<Value>,
) -> Value {
    match method {
        "initialize" => handle_initialize(id, params),
        "initialized" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => handle_tools_list(id),
        "tools/call" => handle_tools_call(engine, id, params).await,
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

fn handle_tools_list(id: Value) -> Value {
    let result = json!({ "tools": tools::tool_definitions() });
    serde_json::to_value(JsonRpcResponse::new(id, result)).unwrap_or(Value::Null)
}

async fn handle_tools_call(engine: &Arc<Mutex<Engine>>, id: Value, params: Option<Value>) -> Value {
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

    let call_result = tokio::task::spawn_blocking(move || {
        let mut engine = match engine_arc.lock() {
            Ok(e) => e,
            Err(e) => return (format!("Engine lock poisoned: {e}"), true),
        };
        tools::dispatch_tool(&mut engine, &tool_name_clone, &args)
    })
    .await;

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
