#![recursion_limit = "512"]
//! Codixing MCP server — exposes code search and graph tools via the
//! Model Context Protocol (JSON-RPC 2.0 over stdin/stdout, Unix socket, or
//! Windows named pipe).
//!
//! **Daemon mode** (`--daemon`):
//!   Loads the engine once and serves it over a Unix domain socket
//!   (`.codixing/daemon.sock`) or a Windows named pipe
//!   (`\\.\pipe\codixing-<hash>`). Subsequent `codixing-mcp` invocations
//!   detect the live daemon and proxy their stdin/stdout through it,
//!   making per-call latency ~1 ms instead of ~30 ms.
//!
//! **Normal mode** (no flag):
//!   Checks for a live daemon first. If found, proxies all traffic
//!   to it. If not, falls back to loading the engine directly (existing
//!   behaviour, also triggers auto-init if no index exists yet).
//!
//! **Logging**: always directed to *stderr* — stdout is the JSON-RPC channel.

#[cfg(unix)]
mod daemon;
#[cfg(windows)]
mod daemon_windows;
mod jsonrpc;
mod progress;
mod protocol;
mod tools;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader, BufWriter};
use tracing::info;
use tracing_subscriber::EnvFilter;

use codixing_core::{
    EmbeddingConfig, Engine, FederatedEngine, FederationConfig, IndexConfig, SessionState,
};

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
    /// Useful for MCP clients that cannot do dynamic tool discovery (e.g. Codex CLI).
    Medium,
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Codixing MCP server — JSON-RPC 2.0 over stdin/stdout (or Unix socket / Windows named pipe
/// in daemon mode).
#[derive(Parser)]
#[command(name = "codixing-mcp", version, about)]
struct Args {
    /// Root directory containing the `.codixing` index (created by `codixing init`).
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Start in daemon mode: load the engine once and serve multiple clients
    /// concurrently. On Unix, listens on a domain socket (`.codixing/daemon.sock`).
    /// On Windows, listens on a named pipe (`\\.\pipe\codixing-<hash>`).
    /// Subsequent `codixing-mcp` invocations will auto-proxy through the daemon.
    #[arg(long)]
    daemon: bool,

    /// Path to the Unix socket used by daemon mode (Unix only).
    /// Defaults to `<root>/.codixing/daemon.sock`.
    /// On Windows, the pipe name is derived automatically from the root path.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Disable session tracking entirely. When set, no session events are
    /// recorded and no session-based search boosting is applied.
    #[arg(long)]
    no_session: bool,

    /// Enable medium tool listing: `tools/list` returns a curated set of ~15
    /// most-used tools instead of all tools. All tools remain callable via
    /// `tools/call`. Useful for MCP clients that cannot do dynamic tool
    /// discovery (e.g. Codex CLI).
    #[arg(long)]
    medium: bool,

    /// Path to a `codixing-federation.json` config file for cross-repo
    /// federation.  When provided, a `FederatedEngine` is created alongside
    /// the primary engine, enabling the `list_projects` tool and federated
    /// search across multiple indexed projects.
    #[arg(long)]
    federation: Option<PathBuf>,

    /// Disable automatic daemon forking. When set, the server always runs
    /// in direct (non-daemon) mode even when no daemon is running.
    #[arg(long)]
    no_daemon_fork: bool,
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

    let listing_mode = if args.medium {
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
        // ── Daemon mode ───────────────────────────────────────────────────
        let mut engine = load_engine(&root).await?;
        if args.no_session {
            engine.set_session(Arc::new(SessionState::new(false)));
        }
        let engine = Arc::new(RwLock::new(engine));

        #[cfg(unix)]
        {
            daemon::run_daemon(engine, &socket_path, listing_mode, federation).await
        }
        #[cfg(windows)]
        {
            let pipe_name = daemon_windows::pipe_name_for_root(&root);
            daemon_windows::run_daemon(engine, &pipe_name, listing_mode, federation).await
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = engine;
            anyhow::bail!(
                "daemon mode is not supported on this platform. Use stdin/stdout mode instead."
            );
        }
    } else {
        // ── Normal mode: try proxy, fall back to direct ────────────────────

        // Auto-fork a daemon if none is running (Unix).
        #[cfg(unix)]
        if !args.no_daemon_fork && !daemon::socket_alive(&socket_path).await {
            info!("auto-starting daemon at {}", socket_path.display());
            let exe = std::env::current_exe()?;
            let mut daemon_args = vec![
                "--root".to_string(),
                root.to_str().unwrap().to_string(),
                "--daemon".to_string(),
                "--socket".to_string(),
                socket_path.to_str().unwrap().to_string(),
            ];
            if args.medium {
                daemon_args.push("--medium".to_string());
            }
            if args.no_session {
                daemon_args.push("--no-session".to_string());
            }
            if let Some(ref fed_path) = args.federation {
                daemon_args.push("--federation".to_string());
                daemon_args.push(fed_path.to_str().unwrap().to_string());
            }
            std::process::Command::new(&exe)
                .args(&daemon_args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("failed to fork daemon")?;

            // Wait briefly for the daemon to bind the socket.
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if daemon::socket_alive(&socket_path).await {
                    break;
                }
            }
        }

        // Auto-fork a daemon if none is running (Windows).
        #[cfg(windows)]
        let pipe_name = daemon_windows::pipe_name_for_root(&root);
        #[cfg(windows)]
        if !args.no_daemon_fork && !daemon_windows::pipe_alive(&pipe_name).await {
            info!("auto-starting daemon on pipe {}", pipe_name);
            let exe = std::env::current_exe()?;
            let mut daemon_args = vec![
                "--root".to_string(),
                root.to_str().unwrap().to_string(),
                "--daemon".to_string(),
            ];
            if args.medium {
                daemon_args.push("--medium".to_string());
            }
            if args.no_session {
                daemon_args.push("--no-session".to_string());
            }
            if let Some(ref fed_path) = args.federation {
                daemon_args.push("--federation".to_string());
                daemon_args.push(fed_path.to_str().unwrap().to_string());
            }
            std::process::Command::new(&exe)
                .args(&daemon_args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("failed to fork daemon")?;

            // Wait briefly for the daemon to create the pipe.
            for _ in 0..20 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if daemon_windows::pipe_alive(&pipe_name).await {
                    break;
                }
            }
        }

        // Try to proxy through an existing daemon (Unix).
        #[cfg(unix)]
        if daemon::socket_alive(&socket_path).await {
            if args.medium {
                tracing::warn!(
                    "proxying through existing daemon — the daemon may have been started \
                     with different --medium setting; restart the daemon to change the mode"
                );
            }
            info!(socket = %socket_path.display(), "daemon detected — proxying through socket");
            return daemon::run_proxy(&socket_path).await;
        }

        // Try to proxy through an existing daemon (Windows).
        #[cfg(windows)]
        if daemon_windows::pipe_alive(&pipe_name).await {
            if args.medium {
                tracing::warn!(
                    "proxying through existing daemon — the daemon may have been started \
                     with different --medium setting; restart the daemon to change the mode"
                );
            }
            info!(pipe = %pipe_name, "daemon detected — proxying through named pipe");
            return daemon_windows::run_proxy(&pipe_name).await;
        }

        // No daemon available — run directly on stdin/stdout.
        {
            let mut engine = load_engine(&root).await?;
            if args.no_session {
                engine.set_session(Arc::new(SessionState::new(false)));
            }
            let engine = Arc::new(RwLock::new(engine));
            info!("Codixing MCP server ready — listening on stdin");
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();
            jsonrpc::run_jsonrpc_loop(
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
            tracing::warn!(
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
