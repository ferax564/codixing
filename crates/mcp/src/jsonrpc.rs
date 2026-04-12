//! Core JSON-RPC message loop and dispatch for the MCP protocol.
//!
//! This module handles all JSON-RPC 2.0 message processing: reading requests,
//! dispatching to the correct handler, and writing responses.

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tracing::{debug, error, info, warn};

use codixing_core::{Engine, FederatedEngine};

use crate::ListingMode;
use crate::progress;
use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::tools;

// ---------------------------------------------------------------------------
// Core JSON-RPC message loop (generic over any AsyncRead + AsyncWrite)
// ---------------------------------------------------------------------------

pub(crate) async fn run_jsonrpc_loop<R, W>(
    engine: Arc<RwLock<Engine>>,
    mut reader: tokio::io::Lines<BufReader<R>>,
    mut writer: BufWriter<W>,
    mut listing_mode: ListingMode,
    federation: Option<Arc<FederatedEngine>>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    // In --compact mode, track whether we have already sent a
    // `notifications/tools/list_changed` notification so we only send it once.
    let mut compact_notification_sent = false;

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
                if handle_internal_notification(&req.method, req.params.as_ref(), &mut listing_mode)
                {
                    continue;
                }
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
            &mut compact_notification_sent,
        )
        .await;
        write_line(&mut writer, &response).await?;

        // Refresh daemon idle-timeout watchdog on every request, not just
        // on accept(). A long-lived MCP session (single socket) would
        // otherwise let the daemon exit after 30 min of "inactivity".
        #[cfg(unix)]
        crate::daemon::touch_activity();
    }

    info!("client disconnected");
    Ok(())
}

fn handle_internal_notification(
    method: &str,
    params: Option<&Value>,
    listing_mode: &mut ListingMode,
) -> bool {
    if method != crate::LISTING_MODE_NOTIFICATION_METHOD {
        return false;
    }

    let Some(mode) = params
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        .and_then(ListingMode::from_wire_value)
    else {
        warn!("ignoring invalid listing mode override notification");
        return true;
    };

    *listing_mode = mode;
    debug!(
        mode = mode.as_wire_value(),
        "updated per-connection listing mode"
    );
    true
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn dispatch<W>(
    engine: &Arc<RwLock<Engine>>,
    id: Value,
    method: &str,
    params: Option<Value>,
    listing_mode: ListingMode,
    federation: &Option<Arc<FederatedEngine>>,
    writer: &mut BufWriter<W>,
    compact_notification_sent: &mut bool,
) -> Value
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match method {
        "initialize" => handle_initialize(id, params),
        "initialized" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => handle_tools_list(id, listing_mode, federation.is_some()),
        "tools/call" => {
            handle_tools_call(
                engine,
                id,
                params,
                federation,
                writer,
                listing_mode,
                compact_notification_sent,
            )
            .await
        }
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
    listing_mode: ListingMode,
    compact_notification_sent: &mut bool,
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

    // In --compact mode, notify the client once when a non-meta tool is called
    // so it can re-fetch tools/list and discover all available tools.
    if listing_mode == ListingMode::Compact
        && !*compact_notification_sent
        && !tools::is_meta_tool(&tool_name)
    {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed"
        });
        if let Err(e) = write_line(writer, &notification).await {
            warn!(error = %e, "failed to write tools/list_changed notification");
        } else {
            *compact_notification_sent = true;
        }
    }

    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    let engine_arc = Arc::clone(engine);
    let tool_name_clone = tool_name.clone();
    let read_only = tools::is_read_only_tool(&tool_name);
    let fed_clone = federation.clone();

    // Create a progress channel if the caller provided a progressToken in _meta.
    let caller_progress_token = progress::extract_progress_token(&params);
    let mut progress_bridge = progress::bridge_channel(caller_progress_token);

    if let Some(ref mut bridge) = progress_bridge {
        progress::drain_buffered(bridge, writer).await;
    }

    let reporter_for_blocking = progress_bridge.as_ref().map(|b| b.reporter.clone());

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
    if let Some(bridge) = progress_bridge {
        let result = progress::drain_during_call(bridge, writer, call_result).await;
        return build_tool_response(id, tool_name, result);
    }

    let call_result = call_result.await;
    build_tool_response(id, tool_name, call_result)
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

pub(crate) async fn write_line<W, T>(writer: &mut BufWriter<W>, value: &T) -> Result<()>
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

    use std::path::Path;

    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    use codixing_core::{EmbeddingConfig, IndexConfig};

    #[cfg(unix)]
    use std::time::Duration;
    #[cfg(unix)]
    use tokio::net::UnixListener;
    #[cfg(unix)]
    use tokio::net::UnixStream;

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
        let listener = match UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping daemon_socket_roundtrip: cannot bind unix socket: {err}");
                return;
            }
            Err(err) => panic!("failed to bind test daemon socket: {err}"),
        };

        // Start the daemon listener in a background task.
        let engine_clone = Arc::clone(&engine);
        let daemon_handle = tokio::spawn(async move {
            // Accept exactly one connection.
            let (stream, _) = listener.accept().await.unwrap();
            crate::daemon::handle_socket_connection(stream, engine_clone, ListingMode::Full, None)
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

    // -----------------------------------------------------------------------
    // Compact mode notification tests
    // -----------------------------------------------------------------------

    /// Helper: run requests with a specific listing mode and collect ALL output
    /// lines (both responses and notifications).
    async fn run_requests_with_mode(
        engine: Engine,
        requests: &[Value],
        listing_mode: ListingMode,
    ) -> Vec<Value> {
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
                listing_mode,
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
    async fn compact_mode_sends_list_changed_on_first_tool_call() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        // In compact mode: initialize, then two tool calls.
        let all_output = run_requests_with_mode(
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
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "code_search",
                        "arguments": { "query": "hello", "limit": 3 }
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {
                        "name": "code_search",
                        "arguments": { "query": "world", "limit": 3 }
                    }
                }),
            ],
            ListingMode::Compact,
        )
        .await;

        // Collect tools/list_changed notifications.
        let list_changed: Vec<&Value> = all_output
            .iter()
            .filter(|v| {
                v.get("method").and_then(|m| m.as_str()) == Some("notifications/tools/list_changed")
            })
            .collect();

        // The notification must be sent exactly once (first non-meta tool call).
        assert_eq!(
            list_changed.len(),
            1,
            "expected exactly one tools/list_changed notification, got {}. All output: {all_output:?}",
            list_changed.len()
        );

        // Verify notification structure (no id, no result/error).
        let notif = list_changed[0];
        assert_eq!(notif["jsonrpc"], "2.0");
        assert_eq!(notif["method"], "notifications/tools/list_changed");
        assert!(
            notif.get("id").is_none(),
            "notification must not have an id"
        );

        // Verify the two tool responses are still present.
        let responses: Vec<&Value> = all_output
            .iter()
            .filter(|v| v.get("id").is_some())
            .collect();
        assert_eq!(
            responses.len(),
            3,
            "expected 3 responses (init + 2 tool calls)"
        );
        assert_eq!(responses[1]["result"]["isError"], false);
        assert_eq!(responses[2]["result"]["isError"], false);
    }

    #[tokio::test]
    async fn compact_mode_no_notification_for_meta_tools() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        // Call only meta-tools in compact mode — no notification should be sent.
        let all_output = run_requests_with_mode(
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
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "search_tools",
                        "arguments": { "query": "search" }
                    }
                }),
            ],
            ListingMode::Compact,
        )
        .await;

        let list_changed: Vec<&Value> = all_output
            .iter()
            .filter(|v| {
                v.get("method").and_then(|m| m.as_str()) == Some("notifications/tools/list_changed")
            })
            .collect();

        assert!(
            list_changed.is_empty(),
            "expected no list_changed notification when only meta-tools are called, got: {list_changed:?}"
        );
    }

    #[tokio::test]
    async fn full_mode_no_list_changed_notification() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        // In Full mode, no notification should ever be sent.
        let all_output = run_requests_with_mode(
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
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "code_search",
                        "arguments": { "query": "hello", "limit": 3 }
                    }
                }),
            ],
            ListingMode::Full,
        )
        .await;

        let list_changed: Vec<&Value> = all_output
            .iter()
            .filter(|v| {
                v.get("method").and_then(|m| m.as_str()) == Some("notifications/tools/list_changed")
            })
            .collect();

        assert!(
            list_changed.is_empty(),
            "expected no list_changed notification in Full mode, got: {list_changed:?}"
        );
    }
}
