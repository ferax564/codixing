//! Progress notification bridge: sync `mpsc` → async `tokio::sync::mpsc`.
//!
//! When an MCP client provides a `progressToken` on a `tools/call` request,
//! we create a [`bridge_channel`] that wires a sync [`ProgressReporter`]
//! (used inside `spawn_blocking`) to an async receiver that can write
//! progress notifications to the JSON-RPC output stream.

use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufWriter};
use tracing::debug;

use crate::protocol::ProgressNotification;
use crate::tools::ProgressReporter;

// ---------------------------------------------------------------------------
// Bridge channel
// ---------------------------------------------------------------------------

/// The async side of the progress bridge — receives [`ProgressNotification`]
/// values that were sent from the sync `ProgressReporter`.
pub(crate) struct ProgressBridge {
    pub reporter: ProgressReporter,
    pub rx: tokio::sync::mpsc::UnboundedReceiver<ProgressNotification>,
}

/// Create a progress bridge for the given `token`.
///
/// Returns `None` if `token` is `None` (i.e. the caller didn't request
/// progress).
pub(crate) fn bridge_channel(token: Option<String>) -> Option<ProgressBridge> {
    let token = token?;

    let (tx, rx) = std::sync::mpsc::channel();
    let reporter = ProgressReporter::new(token, tx, 100);

    // Spawn a background OS thread that drains the sync receiver and
    // forwards each notification into the async unbounded channel.
    let (bridge_tx, bridge_rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(notification) = rx.recv() {
            if bridge_tx.send(notification).is_err() {
                break;
            }
        }
    });

    Some(ProgressBridge {
        reporter,
        rx: bridge_rx,
    })
}

// ---------------------------------------------------------------------------
// Drain helpers
// ---------------------------------------------------------------------------

/// Write a single progress notification to the JSON-RPC output stream.
async fn write_progress<W>(
    writer: &mut BufWriter<W>,
    notification: &ProgressNotification,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let json_val = notification.to_json();
    let mut line =
        serde_json::to_string(&json_val).map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("write: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("flush: {e}"))?;
    Ok(())
}

/// Drain any buffered progress notifications that arrived *before* the tool
/// call started (or while it was being set up).
pub(crate) async fn drain_buffered<W>(bridge: &mut ProgressBridge, writer: &mut BufWriter<W>)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Ok(notification) = bridge.rx.try_recv() {
        if let Err(e) = write_progress(writer, &notification).await {
            debug!(error = %e, "failed to write progress notification");
        }
    }
}

/// Run a tool call future concurrently with progress draining.
///
/// Returns the tool call result once the future completes, after flushing
/// all remaining progress notifications.
pub(crate) async fn drain_during_call<W, F>(
    mut bridge: ProgressBridge,
    writer: &mut BufWriter<W>,
    call_result: F,
) -> std::result::Result<(String, bool), tokio::task::JoinError>
where
    W: tokio::io::AsyncWrite + Unpin,
    F: std::future::Future<Output = std::result::Result<(String, bool), tokio::task::JoinError>>,
{
    tokio::pin!(call_result);

    let result = loop {
        tokio::select! {
            result = &mut call_result => break result,
            msg = bridge.rx.recv() => {
                match msg {
                    Some(notification) => {
                        if let Err(e) = write_progress(writer, &notification).await {
                            debug!(error = %e, "failed to write progress notification");
                        }
                    }
                    None => {
                        // Sender dropped — drain branch completed normally.
                    }
                }
            }
        }
    };

    // Drain any remaining progress notifications after the tool call finishes.
    bridge.rx.close();
    while let Some(notification) = bridge.rx.recv().await {
        let _ = write_progress(writer, &notification).await;
    }

    result
}

/// Extract the progress token from `params._meta.progressToken`.
pub(crate) fn extract_progress_token(params: &Value) -> Option<String> {
    params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
