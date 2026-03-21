//! Daemon mode: Unix socket server and proxy.
//!
//! All functions in this module are `#[cfg(unix)]` — they require Unix domain
//! sockets and are not available on Windows.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tracing::{info, warn};

use codixing_core::{Engine, FederatedEngine};

use crate::ListingMode;
use crate::jsonrpc::run_jsonrpc_loop;

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
// Daemon: Unix socket server
// ---------------------------------------------------------------------------

pub(crate) async fn run_daemon(
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

    loop {
        let (stream, _addr) = listener.accept().await.context("daemon: accept failed")?;
        touch_activity();

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
pub(crate) async fn handle_socket_connection(
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
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).ok();
    }
}

// ---------------------------------------------------------------------------
// Proxy: pipe stdin/stdout through an existing daemon socket
// ---------------------------------------------------------------------------

pub(crate) async fn run_proxy(socket_path: &Path) -> Result<()> {
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
            .context("proxy: socket→stdout copy failed")?;
        Ok(())
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
pub(crate) async fn socket_alive(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    matches!(
        tokio::time::timeout(Duration::from_millis(100), UnixStream::connect(path)).await,
        Ok(Ok(_))
    )
}
