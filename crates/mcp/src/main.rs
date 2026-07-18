#![recursion_limit = "512"]
//! Codixing MCP server — exposes code search and graph tools via the
//! Model Context Protocol (JSON-RPC 2.0 over stdin/stdout, Unix socket, or
//! Windows named pipe).
//!
//! **Daemon mode** (`--daemon`):
//!   Loads the engine once and serves it over a Unix domain socket
//!   (`.codixing/daemon-<profile>[-escalating].sock`) or a policy-specific
//!   Windows named pipe
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
use tokio::io::{BufReader, BufWriter};
use tracing::info;
use tracing_subscriber::EnvFilter;

use codixing_core::{
    EmbeddingConfig, Engine, FederatedEngine, FederationConfig, IndexConfig, SessionState,
};

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
    /// concurrently. The Unix socket and Windows named-pipe names include the
    /// startup profile and escalation policy so distinct safety ceilings cannot
    /// accidentally share one daemon.
    /// Subsequent `codixing-mcp` invocations will auto-proxy through the daemon.
    #[arg(long)]
    daemon: bool,

    /// Path to the Unix socket used by daemon mode (Unix only).
    /// Defaults to `<root>/.codixing/daemon-<profile>[-escalating].sock` on Unix
    /// and a policy-specific Codixing named pipe on Windows.
    /// On Windows, the pipe name is derived automatically from the root path.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Disable session tracking entirely. When set, no session events are
    /// recorded and no session-based search boosting is applied.
    #[arg(long)]
    no_session: bool,

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

    /// Initial MCP tool exposure profile. Agents can switch per connection
    /// at runtime within the startup safety ceiling.
    #[arg(long, value_enum, default_value_t = jsonrpc::McpProfile::Minimal)]
    profile: jsonrpc::McpProfile,

    /// Shortcut for `--profile editor`: expose non-destructive write tools.
    #[arg(long)]
    allow_write_tools: bool,

    /// Allow runtime profile upgrades beyond the startup safety ceiling.
    /// Without this explicit opt-in, minimal/reviewer servers stay read-only.
    #[arg(long)]
    allow_profile_escalation: bool,
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
    let profile = effective_profile(args.profile, args.allow_write_tools)?;
    let runtime_profile_ceiling = jsonrpc::profile_ceiling(profile, args.allow_profile_escalation);

    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("root path not found: {}", args.root.display()))?;

    #[cfg(unix)]
    let socket_path = args
        .socket
        .unwrap_or_else(|| default_socket_path(&root, profile, args.allow_profile_escalation));

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
        let mut engine = load_engine(&root, profile).await?;
        if args.no_session {
            engine.set_session(Arc::new(SessionState::new(false)));
        }
        let engine = Arc::new(RwLock::new(engine));

        #[cfg(unix)]
        {
            daemon::run_daemon(
                engine,
                &socket_path,
                federation,
                profile,
                runtime_profile_ceiling,
            )
            .await
        }
        #[cfg(windows)]
        {
            let pipe_name =
                daemon_windows::pipe_name_for_root(&root, profile, args.allow_profile_escalation);
            daemon_windows::run_daemon(
                engine,
                &pipe_name,
                federation,
                profile,
                runtime_profile_ceiling,
            )
            .await
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
                root.to_str()
                    .ok_or_else(|| anyhow::anyhow!("root path is not valid UTF-8"))?
                    .to_string(),
                "--daemon".to_string(),
                "--socket".to_string(),
                socket_path
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("socket path is not valid UTF-8"))?
                    .to_string(),
            ];
            if args.no_session {
                daemon_args.push("--no-session".to_string());
            }
            if let Some(ref fed_path) = args.federation {
                daemon_args.push("--federation".to_string());
                daemon_args.push(
                    fed_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("federation path is not valid UTF-8"))?
                        .to_string(),
                );
            }
            daemon_args.push("--profile".to_string());
            daemon_args.push(profile.as_str().to_string());
            if args.allow_profile_escalation {
                daemon_args.push("--allow-profile-escalation".to_string());
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
        let pipe_name =
            daemon_windows::pipe_name_for_root(&root, profile, args.allow_profile_escalation);
        #[cfg(windows)]
        if !args.no_daemon_fork && !daemon_windows::pipe_alive(&pipe_name).await {
            info!("auto-starting daemon on pipe {}", pipe_name);
            let exe = std::env::current_exe()?;
            let mut daemon_args = vec![
                "--root".to_string(),
                root.to_str()
                    .ok_or_else(|| anyhow::anyhow!("root path is not valid UTF-8"))?
                    .to_string(),
                "--daemon".to_string(),
            ];
            if args.no_session {
                daemon_args.push("--no-session".to_string());
            }
            if let Some(ref fed_path) = args.federation {
                daemon_args.push("--federation".to_string());
                daemon_args.push(
                    fed_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("federation path is not valid UTF-8"))?
                        .to_string(),
                );
            }
            daemon_args.push("--profile".to_string());
            daemon_args.push(profile.as_str().to_string());
            if args.allow_profile_escalation {
                daemon_args.push("--allow-profile-escalation".to_string());
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
            info!(socket = %socket_path.display(), "daemon detected — proxying through socket");
            return daemon::run_proxy(&socket_path).await;
        }

        // Try to proxy through an existing daemon (Windows).
        #[cfg(windows)]
        if daemon_windows::pipe_alive(&pipe_name).await {
            info!(pipe = %pipe_name, "daemon detected — proxying through named pipe");
            return daemon_windows::run_proxy(&pipe_name).await;
        }

        // No daemon available — run directly on stdin/stdout.
        {
            let mut engine = load_engine(&root, profile).await?;
            if args.no_session {
                engine.set_session(Arc::new(SessionState::new(false)));
            }
            let engine = Arc::new(RwLock::new(engine));
            info!("Codixing MCP server ready — listening on stdin");
            let stdin = tokio::io::stdin();
            let stdout = tokio::io::stdout();
            jsonrpc::run_jsonrpc_loop(
                engine,
                BufReader::new(stdin),
                BufWriter::new(stdout),
                federation,
                profile,
                runtime_profile_ceiling,
            )
            .await
        }
    }
}

fn effective_profile(
    profile: jsonrpc::McpProfile,
    allow_write_tools: bool,
) -> Result<jsonrpc::McpProfile> {
    if !allow_write_tools {
        return Ok(profile);
    }
    match profile {
        jsonrpc::McpProfile::Minimal | jsonrpc::McpProfile::Reviewer => {
            Ok(jsonrpc::McpProfile::Editor)
        }
        jsonrpc::McpProfile::Editor | jsonrpc::McpProfile::Dangerous => Ok(profile),
    }
}

#[cfg(unix)]
fn default_socket_path(
    root: &Path,
    profile: jsonrpc::McpProfile,
    allow_profile_escalation: bool,
) -> PathBuf {
    let policy_suffix = if allow_profile_escalation {
        "-escalating"
    } else {
        ""
    };
    root.join(".codixing")
        .join(format!("daemon-{}{policy_suffix}.sock", profile.as_str()))
}

// ---------------------------------------------------------------------------
// Engine loader (shared by daemon + direct modes)
// ---------------------------------------------------------------------------

async fn load_engine(root: &Path, profile: jsonrpc::McpProfile) -> Result<Engine> {
    if Engine::index_exists(root) {
        // Read-only profiles expose no mutating tools — open without the
        // writer so the Tantivy write lock stays free for CLI syncs running
        // alongside. An allowed set_mcp_profile upgrade re-acquires the writer.
        if profile.is_read_only_profile() {
            info!(
                root = %root.display(),
                profile = profile.as_str(),
                "opening existing Codixing index read-only (read-only profile)"
            );
            return Engine::open_read_only(root).with_context(|| {
                format!(
                    "failed to open index at {} — index may be corrupt; \
                     delete .codixing/ and restart to rebuild",
                    root.display()
                )
            });
        }
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
        let engine = Engine::init(root, config).with_context(|| {
            format!(
                "auto-init failed at {} — ensure the directory exists and contains source files",
                root.display()
            )
        })?;
        if profile.is_read_only_profile() {
            // Init needed the writer; hand the index back read-only so the
            // server matches its profile and frees the write lock.
            drop(engine);
            return Engine::open_read_only(root)
                .with_context(|| format!("failed to reopen fresh index at {}", root.display()));
        }
        Ok(engine)
    }
}

#[cfg(test)]
mod load_engine_tests {
    use super::*;

    #[test]
    fn cli_default_profile_is_minimal() {
        let args = Args::try_parse_from(["codixing-mcp"]).expect("default args should parse");
        assert_eq!(args.profile, jsonrpc::McpProfile::Minimal);
        assert!(!args.allow_profile_escalation);
        assert_eq!(
            jsonrpc::profile_ceiling(args.profile, args.allow_profile_escalation),
            jsonrpc::McpProfile::Reviewer
        );
    }

    #[test]
    fn profile_escalation_requires_explicit_startup_flag() {
        let args = Args::try_parse_from(["codixing-mcp", "--allow-profile-escalation"])
            .expect("escalation flag should parse");
        assert!(args.allow_profile_escalation);
        assert_eq!(
            jsonrpc::profile_ceiling(args.profile, args.allow_profile_escalation),
            jsonrpc::McpProfile::Dangerous
        );
    }

    #[cfg(unix)]
    #[test]
    fn escalating_daemon_uses_a_distinct_socket() {
        let root = Path::new("/tmp/project");
        assert_ne!(
            default_socket_path(root, jsonrpc::McpProfile::Minimal, false),
            default_socket_path(root, jsonrpc::McpProfile::Minimal, true)
        );
    }

    #[test]
    fn allow_write_tools_shortcut_still_selects_editor_from_default() {
        assert_eq!(
            effective_profile(jsonrpc::McpProfile::default(), true).unwrap(),
            jsonrpc::McpProfile::Editor
        );
    }

    fn make_index(dir: &Path) {
        std::fs::write(
            dir.join("lib.rs"),
            "pub fn hello() -> &'static str { \"world\" }\n",
        )
        .unwrap();
        let mut config = IndexConfig::new(dir);
        config.embedding = EmbeddingConfig {
            enabled: false,
            ..EmbeddingConfig::default()
        };
        drop(Engine::init(dir, config).expect("engine init should succeed"));
    }

    #[tokio::test]
    async fn read_only_profile_opens_engine_without_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        make_index(dir.path());

        let engine = load_engine(dir.path(), jsonrpc::McpProfile::Reviewer)
            .await
            .unwrap();
        assert!(
            engine.is_read_only(),
            "reviewer profile must open the engine read-only"
        );

        // The writer lock must remain free so a CLI sync can run alongside.
        let writer = Engine::open(dir.path()).unwrap();
        assert!(
            !writer.is_read_only(),
            "CLI must be able to acquire the writer while a read-only-profile server is up"
        );
    }

    #[tokio::test]
    async fn write_profile_keeps_writer_engine() {
        let dir = tempfile::tempdir().unwrap();
        make_index(dir.path());

        let engine = load_engine(dir.path(), jsonrpc::McpProfile::Editor)
            .await
            .unwrap();
        assert!(!engine.is_read_only(), "editor profile keeps the writer");
    }
}
