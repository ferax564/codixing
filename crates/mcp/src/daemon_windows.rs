//! Daemon mode: Windows named pipe server and proxy.
//!
//! All functions in this module are `#[cfg(windows)]` — they require Windows
//! named pipes and are not available on Unix.
//!
//! Named pipes are the Windows equivalent of Unix domain sockets. The daemon
//! listens on `\\.\pipe\codixing-<hash>` (where `<hash>` is derived from the
//! project root) and serves multiple clients concurrently. The JSON-RPC loop
//! is shared with the Unix daemon via `run_jsonrpc_loop()`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tracing::{info, warn};

use codixing_core::{Engine, FederatedEngine};

use crate::jsonrpc::{McpProfile, run_jsonrpc_loop};

// ---------------------------------------------------------------------------
// Pipe name derivation
// ---------------------------------------------------------------------------

/// Derive a unique pipe name from the project root path.
///
/// Uses a hash of the canonicalized root path to avoid collisions between
/// multiple project daemons. Format: `\\.\pipe\codixing-<hex_hash>`.
pub(crate) fn pipe_name_for_root(root: &Path, profile: McpProfile) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let hash = hasher.finish();
    format!(r"\\.\pipe\codixing-{hash:016x}-{}", profile.as_str())
}

// ---------------------------------------------------------------------------
// Idle timeout watchdog
// ---------------------------------------------------------------------------

/// Timestamp (millis since UNIX epoch) of the last client activity.
static LAST_ACTIVITY: AtomicU64 = AtomicU64::new(0);

/// Idle timeout: daemon exits after 30 minutes with no client connections.
const IDLE_TIMEOUT_MS: u64 = 30 * 60 * 1000;

pub(crate) fn touch_activity() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    LAST_ACTIVITY.store(now, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Daemon: Windows named pipe server
// ---------------------------------------------------------------------------

pub(crate) async fn run_daemon(
    engine: Arc<RwLock<Engine>>,
    pipe_name: &str,
    federation: Option<Arc<FederatedEngine>>,
    profile: McpProfile,
) -> Result<()> {
    // Create the first pipe instance with `first_pipe_instance(true)` to
    // ensure we are the only daemon for this project. If another daemon is
    // already running, this will fail with ERROR_ACCESS_DENIED.
    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(pipe_name)
        .with_context(|| {
            format!("failed to create named pipe {pipe_name} — is another daemon already running?")
        })?;

    info!(pipe = %pipe_name, "daemon listening on named pipe");

    // Mark initial activity so the watchdog doesn't fire immediately.
    touch_activity();

    // Spawn an idle-timeout watchdog: check every 60s and exit if no client
    // activity for IDLE_TIMEOUT_MS (30 minutes).
    tokio::spawn(async {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let last = LAST_ACTIVITY.load(Ordering::Relaxed);
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            if now.saturating_sub(last) > IDLE_TIMEOUT_MS {
                info!("daemon idle for >30 min — shutting down");
                std::process::exit(0);
            }
        }
    });

    // Spawn a background task that watches the project directory and keeps the
    // in-memory engine up to date when source files change.
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
            let changes = watcher.poll_changes(Duration::from_secs(2));
            if changes.is_empty() {
                continue;
            }

            // Secondary settlement window: batch multi-file operations.
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

            // Deduplicate: keep the last occurrence of each path.
            {
                all_changes.reverse();
                let mut seen = std::collections::HashSet::new();
                all_changes.retain(|c| seen.insert(c.path.clone()));
                all_changes.reverse();
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

    // Accept loop: each iteration waits for a client to connect, then spawns
    // a task to handle it. A new pipe instance is created before spawning so
    // the next client can connect immediately.
    loop {
        // Wait for a client to connect to the current pipe instance.
        server
            .connect()
            .await
            .context("daemon: pipe connect failed")?;
        touch_activity();

        // Move the connected pipe to the handler and create a fresh instance
        // for the next client.
        let connected = server;
        server = ServerOptions::new()
            .create(pipe_name)
            .with_context(|| format!("failed to create next pipe instance for {pipe_name}"))?;

        let engine_clone = Arc::clone(&engine);
        let fed_clone = federation.clone();
        tokio::spawn(async move {
            if let Err(e) =
                handle_pipe_connection(connected, engine_clone, fed_clone, profile).await
            {
                warn!(error = %e, "daemon: pipe connection error");
            }
        });
    }
}

/// Handle one client connection over a named pipe.
///
/// Named pipes in tokio implement `AsyncRead + AsyncWrite` directly (no
/// `into_split()`), so we use `tokio::io::split()` to get separate read/write
/// halves for the JSON-RPC loop.
async fn handle_pipe_connection(
    pipe: NamedPipeServer,
    engine: Arc<RwLock<Engine>>,
    federation: Option<Arc<FederatedEngine>>,
    profile: McpProfile,
) -> Result<()> {
    let (read_half, write_half) = tokio::io::split(pipe);
    run_jsonrpc_loop(
        engine,
        BufReader::new(read_half).lines(),
        BufWriter::new(write_half),
        federation,
        profile,
    )
    .await
}

// ---------------------------------------------------------------------------
// Proxy: pipe stdin/stdout through an existing daemon named pipe
// ---------------------------------------------------------------------------

pub(crate) async fn run_proxy(pipe_name: &str) -> Result<()> {
    let pipe = ClientOptions::new()
        .open(pipe_name)
        .with_context(|| format!("failed to connect to daemon pipe {pipe_name}"))?;

    let (mut pipe_read, mut pipe_write) = tokio::io::split(pipe);
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    // Forward stdin -> pipe, then shut down the write side.
    let to_pipe = async {
        tokio::io::copy(&mut stdin, &mut pipe_write)
            .await
            .context("proxy: stdin->pipe copy failed")?;
        pipe_write
            .shutdown()
            .await
            .context("proxy: pipe write shutdown failed")
    };

    // Forward pipe -> stdout until the daemon closes its end.
    let from_pipe = async {
        tokio::io::copy(&mut pipe_read, &mut stdout)
            .await
            .context("proxy: pipe->stdout copy failed")?;
        Ok(())
    };

    tokio::try_join!(to_pipe, from_pipe)?;
    Ok(())
}

/// Return true if a named pipe daemon is alive and accepting connections.
///
/// Tries to open the pipe with a short timeout. Named pipes that don't exist
/// will fail immediately with `ERROR_FILE_NOT_FOUND`; pipes that exist but
/// are busy will return `ERROR_PIPE_BUSY`.
pub(crate) async fn pipe_alive(pipe_name: &str) -> bool {
    // Try to connect. If the pipe exists and the server has a pending
    // instance, this succeeds. We immediately drop the connection.
    let name = pipe_name.to_string();
    let result = tokio::task::spawn_blocking(move || ClientOptions::new().open(&name)).await;
    matches!(result, Ok(Ok(_)))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn pipe_name_is_deterministic() {
        let root = PathBuf::from(r"C:\Users\dev\project");
        let name1 = pipe_name_for_root(&root, McpProfile::Reviewer);
        let name2 = pipe_name_for_root(&root, McpProfile::Reviewer);
        assert_eq!(name1, name2);
    }

    #[test]
    fn pipe_name_has_correct_prefix() {
        let root = PathBuf::from(r"C:\Users\dev\project");
        let name = pipe_name_for_root(&root, McpProfile::Reviewer);
        assert!(
            name.starts_with(r"\\.\pipe\codixing-"),
            "expected pipe prefix, got: {name}"
        );
    }

    #[test]
    fn pipe_name_differs_for_different_roots() {
        let root1 = PathBuf::from(r"C:\Users\dev\project1");
        let root2 = PathBuf::from(r"C:\Users\dev\project2");
        let name1 = pipe_name_for_root(&root1, McpProfile::Reviewer);
        let name2 = pipe_name_for_root(&root2, McpProfile::Reviewer);
        assert_ne!(name1, name2);
    }

    #[test]
    fn pipe_name_differs_for_different_profiles() {
        let root = PathBuf::from(r"C:\Users\dev\project");
        let reviewer = pipe_name_for_root(&root, McpProfile::Reviewer);
        let editor = pipe_name_for_root(&root, McpProfile::Editor);
        assert_ne!(reviewer, editor);
    }

    #[test]
    fn pipe_name_hex_suffix_is_16_chars() {
        let root = PathBuf::from(r"C:\some\path");
        let name = pipe_name_for_root(&root, McpProfile::Reviewer);
        let suffix = name
            .strip_prefix(r"\\.\pipe\codixing-")
            .unwrap()
            .strip_suffix("-reviewer")
            .unwrap();
        assert_eq!(suffix.len(), 16, "hex hash should be 16 chars: {suffix}");
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "suffix should be hex: {suffix}"
        );
    }
}
