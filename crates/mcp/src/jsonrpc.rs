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

use crate::progress;
use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::tools;

// ---------------------------------------------------------------------------
// MCP profiles
// ---------------------------------------------------------------------------

/// Tool exposure and mutation policy for an MCP server instance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum McpProfile {
    /// Narrow discovery profile: search, symbols, repo map, and meta-tools only.
    Minimal,
    /// Read-only review profile: all read-only tools, no mutation.
    #[default]
    Reviewer,
    /// Editing profile: read-only tools plus non-destructive write helpers.
    Editor,
    /// Full tool profile, including destructive filesystem and shell tools.
    Dangerous,
}

impl McpProfile {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Reviewer => "reviewer",
            Self::Editor => "editor",
            Self::Dangerous => "dangerous",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "minimal" => Some(Self::Minimal),
            "reviewer" => Some(Self::Reviewer),
            "editor" => Some(Self::Editor),
            "dangerous" => Some(Self::Dangerous),
            _ => None,
        }
    }

    fn allows_tool(self, name: &str) -> bool {
        if is_profile_management_tool(name) {
            return true;
        }

        if !tools::is_known_tool(name) {
            return true;
        }

        match self {
            Self::Minimal => is_minimal_tool(name),
            Self::Reviewer => tools::is_read_only_tool(name),
            Self::Editor => tools::is_read_only_tool(name) || !is_dangerous_write_tool(name),
            Self::Dangerous => true,
        }
    }

    fn blocked_message(self, name: &str) -> String {
        match self {
            Self::Minimal => format!(
                "Tool '{name}' is not available in MCP profile 'minimal'. Call set_mcp_profile with profile='reviewer' to enable the full read-only tool set without restarting the MCP server."
            ),
            Self::Reviewer => format!(
                "Tool '{name}' is blocked by MCP profile 'reviewer' because it can mutate project state. Call set_mcp_profile with profile='editor' to enable non-destructive write tools without restarting the MCP server."
            ),
            Self::Editor => format!(
                "Tool '{name}' is blocked by MCP profile 'editor' because it is destructive or can execute shell commands. Call set_mcp_profile with profile='dangerous' and confirm_dangerous=true to expose it without restarting the MCP server."
            ),
            Self::Dangerous => format!("Tool '{name}' is unexpectedly blocked."),
        }
    }
}

fn is_profile_management_tool(name: &str) -> bool {
    matches!(name, "get_mcp_profile" | "set_mcp_profile")
}

fn is_minimal_tool(name: &str) -> bool {
    matches!(
        name,
        "search_tools"
            | "get_tool_schema"
            | "get_mcp_profile"
            | "set_mcp_profile"
            | "index_status"
            | "agent_context_pack"
            | "code_search"
            | "find_symbol"
            | "read_symbol"
            | "get_repo_map"
    )
}

fn is_dangerous_write_tool(name: &str) -> bool {
    matches!(name, "delete_file" | "run_tests")
}

// ---------------------------------------------------------------------------
// Core JSON-RPC message loop (generic over any AsyncRead + AsyncWrite)
// ---------------------------------------------------------------------------

pub(crate) async fn run_jsonrpc_loop<R, W>(
    engine: Arc<RwLock<Engine>>,
    mut reader: tokio::io::Lines<BufReader<R>>,
    mut writer: BufWriter<W>,
    federation: Option<Arc<FederatedEngine>>,
    profile: McpProfile,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut active_profile = profile;

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
            &federation,
            &mut writer,
            &mut active_profile,
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

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn dispatch<W>(
    engine: &Arc<RwLock<Engine>>,
    id: Value,
    method: &str,
    params: Option<Value>,
    federation: &Option<Arc<FederatedEngine>>,
    writer: &mut BufWriter<W>,
    profile: &mut McpProfile,
) -> Value
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match method {
        "initialize" => handle_initialize(id, params),
        "initialized" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => handle_tools_list(id, federation.is_some(), *profile),
        "tools/call" => handle_tools_call(engine, id, params, federation, writer, profile).await,
        _ => {
            let err = JsonRpcError::method_not_found(id, method);
            serde_json::to_value(err).unwrap_or(Value::Null)
        }
    }
}

fn handle_initialize(id: Value, _params: Option<Value>) -> Value {
    let result = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": { "listChanged": true } },
        "serverInfo": { "name": "codixing", "version": env!("CARGO_PKG_VERSION") }
    });
    serde_json::to_value(JsonRpcResponse::new(id, result)).unwrap_or(Value::Null)
}

fn handle_tools_list(id: Value, has_federation: bool, profile: McpProfile) -> Value {
    let mut tool_defs = tools::tool_definitions_with_federation(has_federation);
    if let Some(arr) = tool_defs.as_array_mut() {
        arr.retain(|tool| {
            tool.get("name")
                .and_then(|v| v.as_str())
                .map(|name| profile.allows_tool(name))
                .unwrap_or(true)
        });
    }
    let result = json!({ "tools": tool_defs });
    serde_json::to_value(JsonRpcResponse::new(id, result)).unwrap_or(Value::Null)
}

async fn handle_tools_call<W>(
    engine: &Arc<RwLock<Engine>>,
    id: Value,
    params: Option<Value>,
    federation: &Option<Arc<FederatedEngine>>,
    writer: &mut BufWriter<W>,
    profile: &mut McpProfile,
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

    if is_profile_management_tool(&tool_name) {
        return handle_profile_management_tool(id, &tool_name, &args, profile, writer).await;
    }

    if !profile.allows_tool(&tool_name) {
        return build_tool_response(
            id,
            tool_name.clone(),
            Ok((profile.blocked_message(&tool_name), true)),
        );
    }

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

async fn handle_profile_management_tool<W>(
    id: Value,
    tool_name: &str,
    args: &Value,
    profile: &mut McpProfile,
    writer: &mut BufWriter<W>,
) -> Value
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match tool_name {
        "get_mcp_profile" => build_tool_response(
            id,
            tool_name.to_string(),
            Ok((profile_status_json(*profile, None, false), false)),
        ),
        "set_mcp_profile" => {
            let requested = match args.get("profile").and_then(|v| v.as_str()) {
                Some(profile) => profile,
                None => {
                    return build_tool_response(
                        id,
                        tool_name.to_string(),
                        Ok((
                            "Missing required parameter 'profile' (minimal, reviewer, editor, dangerous).".to_string(),
                            true,
                        )),
                    );
                }
            };

            let Some(next_profile) = McpProfile::from_name(requested) else {
                return build_tool_response(
                    id,
                    tool_name.to_string(),
                    Ok((
                        format!(
                            "Unknown MCP profile '{requested}'. Expected one of: minimal, reviewer, editor, dangerous."
                        ),
                        true,
                    )),
                );
            };

            if next_profile == McpProfile::Dangerous
                && !args
                    .get("confirm_dangerous")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            {
                return build_tool_response(
                    id,
                    tool_name.to_string(),
                    Ok((
                        "Switching to MCP profile 'dangerous' exposes destructive filesystem and shell tools. Re-run with confirm_dangerous=true to confirm.".to_string(),
                        true,
                    )),
                );
            }

            let previous = *profile;
            let changed = previous != next_profile;
            *profile = next_profile;

            if changed {
                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed"
                });
                if let Err(err) = write_line(writer, &notification).await {
                    warn!(error = %err, "failed to notify client that MCP tool list changed");
                }
            }

            build_tool_response(
                id,
                tool_name.to_string(),
                Ok((
                    profile_status_json(*profile, Some(previous), changed),
                    false,
                )),
            )
        }
        _ => build_tool_response(
            id,
            tool_name.to_string(),
            Ok((
                format!("Unknown profile management tool: {tool_name}"),
                true,
            )),
        ),
    }
}

fn profile_status_json(
    active_profile: McpProfile,
    previous_profile: Option<McpProfile>,
    changed: bool,
) -> String {
    let message = if changed {
        "MCP profile updated. Clients should refresh tools/list; a notifications/tools/list_changed event was emitted."
    } else {
        "MCP profile unchanged."
    };

    let payload = json!({
        "schema_version": 1,
        "active_profile": active_profile.as_str(),
        "previous_profile": previous_profile.map(McpProfile::as_str),
        "changed": changed,
        "tool_list_changed": changed,
        "available_profiles": [
            {
                "name": "minimal",
                "description": "Narrow discovery profile: context pack, search, symbols, repo map, and meta-tools."
            },
            {
                "name": "reviewer",
                "description": "Read-only review profile: all read-only tools, no mutation."
            },
            {
                "name": "editor",
                "description": "Read-only tools plus non-destructive write helpers."
            },
            {
                "name": "dangerous",
                "description": "Full tool profile, including destructive filesystem and shell tools."
            }
        ],
        "message": message
    });

    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_string())
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
        run_requests_with_profile(engine, requests, McpProfile::default()).await
    }

    async fn run_requests_with_profile(
        engine: Engine,
        requests: &[Value],
        profile: McpProfile,
    ) -> Vec<Value> {
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
                None,
                profile,
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
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
        assert_eq!(result["serverInfo"]["name"], "codixing");
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
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
    async fn reviewer_profile_hides_and_blocks_write_tools() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests_with_profile(
            engine,
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list"
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "write_file",
                        "arguments": {
                            "file": "blocked.rs",
                            "content": "pub fn blocked() {}"
                        }
                    }
                }),
            ],
            McpProfile::Reviewer,
        )
        .await;

        let tools = responses[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"code_search"));
        assert!(!names.contains(&"write_file"));

        let result = &responses[1]["result"];
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("profile 'reviewer'"));
        assert!(!dir.path().join("blocked.rs").exists());
    }

    #[tokio::test]
    async fn editor_profile_hides_destructive_tools_but_keeps_edit_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests_with_profile(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })],
            McpProfile::Editor,
        )
        .await;

        let tools = responses[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(!names.contains(&"delete_file"));
        assert!(!names.contains(&"run_tests"));
    }

    #[tokio::test]
    async fn minimal_profile_exposes_only_context_entrypoints() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests_with_profile(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            })],
            McpProfile::Minimal,
        )
        .await;

        let tools = responses[0]["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"search_tools"));
        assert!(names.contains(&"get_mcp_profile"));
        assert!(names.contains(&"set_mcp_profile"));
        assert!(names.contains(&"code_search"));
        assert!(names.contains(&"find_symbol"));
        assert!(names.contains(&"get_repo_map"));
        assert!(!names.contains(&"read_file"));
        assert!(!names.contains(&"write_file"));
    }

    #[tokio::test]
    async fn set_mcp_profile_expands_tools_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests_with_profile(
            engine,
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list"
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": "set_mcp_profile",
                        "arguments": { "profile": "reviewer" }
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/list"
                }),
            ],
            McpProfile::Minimal,
        )
        .await;

        let before = responses
            .iter()
            .find(|response| response["id"] == 1)
            .expect("missing initial tools/list response");
        let before_tools = before["result"]["tools"].as_array().unwrap();
        let before_names: Vec<&str> = before_tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(!before_names.contains(&"read_file"));

        let changed_notification = responses
            .iter()
            .any(|response| response["method"] == "notifications/tools/list_changed");
        assert!(
            changed_notification,
            "profile switch should ask clients to refresh tools/list"
        );

        let set_response = responses
            .iter()
            .find(|response| response["id"] == 2)
            .expect("missing set_mcp_profile response");
        assert_eq!(set_response["result"]["isError"], false);
        let profile_text = set_response["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let profile_json: Value = serde_json::from_str(profile_text).unwrap();
        assert_eq!(profile_json["active_profile"], "reviewer");
        assert_eq!(profile_json["previous_profile"], "minimal");
        assert_eq!(profile_json["changed"], true);

        let after = responses
            .iter()
            .find(|response| response["id"] == 3)
            .expect("missing refreshed tools/list response");
        let after_tools = after["result"]["tools"].as_array().unwrap();
        let after_names: Vec<&str> = after_tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(after_names.contains(&"read_file"));
        assert!(!after_names.contains(&"write_file"));
    }

    #[tokio::test]
    async fn dangerous_profile_requires_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());

        let responses = run_requests_with_profile(
            engine,
            &[
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "set_mcp_profile",
                        "arguments": { "profile": "dangerous" }
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list"
                }),
            ],
            McpProfile::Reviewer,
        )
        .await;

        let set_response = responses
            .iter()
            .find(|response| response["id"] == 1)
            .expect("missing set_mcp_profile response");
        assert_eq!(set_response["result"]["isError"], true);
        let text = set_response["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("confirm_dangerous=true"));

        let tools_response = responses
            .iter()
            .find(|response| response["id"] == 2)
            .expect("missing tools/list response");
        let tools = tools_response["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(!names.contains(&"delete_file"));
        assert!(!names.contains(&"run_tests"));
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
            crate::daemon::handle_socket_connection(
                stream,
                engine_clone,
                None,
                McpProfile::default(),
            )
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
        assert_eq!(
            responses[0]["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );

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
                None,
                McpProfile::default(),
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
