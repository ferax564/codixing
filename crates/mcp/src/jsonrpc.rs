//! Core JSON-RPC message loop and dispatch for the MCP protocol.
//!
//! This module handles all JSON-RPC 2.0 message processing: reading requests,
//! dispatching to the correct handler, and writing responses.

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufWriter};
use tracing::{debug, error, info, warn};

use codixing_core::{Engine, FederatedEngine};

use crate::progress;
use crate::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::tools;

/// Maximum newline-delimited JSON-RPC request size. This accommodates the
/// 1 MiB MCP patch limit even when every scalar needs four UTF-8 bytes, while
/// preventing an untrusted client from growing `read_line` without bound.
const MAX_JSONRPC_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_JSONRPC_METHOD_BYTES: usize = 256;
const MAX_JSONRPC_ID_BYTES: usize = 256;
const MAX_MCP_TOOL_NAME_BYTES: usize = 128;

// ---------------------------------------------------------------------------
// MCP profiles
// ---------------------------------------------------------------------------

/// Tool exposure and mutation policy for an MCP server instance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub(crate) enum McpProfile {
    /// Narrow discovery profile: search, symbols, repo map, and meta-tools only.
    #[default]
    Minimal,
    /// Read-only review profile: all read-only tools, no mutation.
    Reviewer,
    /// Editing profile: read-only tools plus non-destructive write helpers.
    Editor,
    /// Full tool profile, including destructive filesystem and shell tools.
    Dangerous,
}

/// Resolve the highest profile a connection may select at runtime.
///
/// Read-only startup profiles may move between `minimal` and `reviewer` for
/// context-efficient discovery, but cannot silently acquire mutation or shell
/// capabilities. Starting explicitly in editor/dangerous keeps that level as
/// the ceiling. `--allow-profile-escalation` is the explicit escape hatch.
pub(crate) fn profile_ceiling(startup: McpProfile, allow_profile_escalation: bool) -> McpProfile {
    if allow_profile_escalation {
        McpProfile::Dangerous
    } else if startup <= McpProfile::Reviewer {
        McpProfile::Reviewer
    } else {
        startup
    }
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

    /// Profiles that expose no mutating tools. Servers running these never
    /// need the Tantivy writer, so they open the engine read-only and leave
    /// the write lock free for CLI syncs running alongside.
    pub(crate) fn is_read_only_profile(self) -> bool {
        matches!(self, Self::Minimal | Self::Reviewer)
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
                "Tool '{name}' is blocked by MCP profile 'reviewer' because it can mutate project state. Start codixing-mcp with --profile editor, or explicitly opt into runtime write-profile upgrades with --allow-profile-escalation."
            ),
            Self::Editor => format!(
                "Tool '{name}' is blocked by MCP profile 'editor' because it is destructive or can execute shell commands. Start with --profile dangerous, or use an escalation-enabled server and set_mcp_profile with profile='dangerous' and confirm_dangerous=true."
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
    mut reader: R,
    mut writer: BufWriter<W>,
    federation: Option<Arc<FederatedEngine>>,
    profile: McpProfile,
    profile_ceiling: McpProfile,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut active_profile = profile;

    while let Some(frame) = read_bounded_frame(&mut reader).await? {
        let line = match frame {
            BoundedFrame::Text(line) => line,
            BoundedFrame::TooLarge => {
                let error = JsonRpcError::invalid_request(
                    Value::Null,
                    &format!(
                        "JSON-RPC request exceeds the {MAX_JSONRPC_FRAME_BYTES}-byte frame limit"
                    ),
                );
                write_line(&mut writer, &error).await?;
                continue;
            }
            BoundedFrame::InvalidUtf8 => {
                let error = JsonRpcError::parse_error("Parse error: request is not valid UTF-8");
                write_line(&mut writer, &error).await?;
                continue;
            }
        };
        let frame_bytes = line.len();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let raw: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                warn!(error = %e, "failed to parse JSON-RPC request");
                let err = JsonRpcError::parse_error(&format!("Parse error: {e}"));
                write_line(&mut writer, &err).await?;
                continue;
            }
        };
        if !raw.get("id").is_none_or(is_valid_bounded_jsonrpc_id) {
            let err = JsonRpcError::invalid_request(
                Value::Null,
                &format!(
                    "Invalid JSON-RPC id: expected null, a number, or a string of at most {MAX_JSONRPC_ID_BYTES} bytes"
                ),
            );
            write_line(&mut writer, &err).await?;
            continue;
        }
        let request_id = raw.get("id").cloned().unwrap_or(Value::Null);
        let req: JsonRpcRequest = match serde_json::from_value(raw) {
            Ok(request) => request,
            Err(error) => {
                let err = JsonRpcError::invalid_request(
                    request_id,
                    &format!("Invalid JSON-RPC request: {error}"),
                );
                write_line(&mut writer, &err).await?;
                continue;
            }
        };
        if req.jsonrpc != "2.0" {
            let err = JsonRpcError::invalid_request(
                req.id.clone().unwrap_or(Value::Null),
                "Invalid JSON-RPC version: expected '2.0'",
            );
            write_line(&mut writer, &err).await?;
            continue;
        }
        if req.method.len() > MAX_JSONRPC_METHOD_BYTES {
            let err = JsonRpcError::invalid_request(
                req.id.clone().unwrap_or(Value::Null),
                &format!(
                    "Invalid JSON-RPC method: maximum length is {MAX_JSONRPC_METHOD_BYTES} bytes"
                ),
            );
            write_line(&mut writer, &err).await?;
            continue;
        }
        let method_summary = bounded_log_text(&req.method, 128);
        let id_summary = jsonrpc_id_summary(req.id.as_ref());
        debug!(method = %method_summary, id = %id_summary, frame_bytes, "received JSON-RPC request");

        let id = match req.id.clone() {
            Some(id) => id,
            None => {
                debug!(method = %method_summary, "ignoring notification");
                continue;
            }
        };

        let response = dispatch(
            DispatchContext {
                engine: &engine,
                federation: &federation,
                writer: &mut writer,
                profile: &mut active_profile,
                profile_ceiling,
            },
            id,
            &req.method,
            req.params,
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

fn bounded_log_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let suffix = if max_bytes >= '…'.len_utf8() {
        "…"
    } else {
        ""
    };
    let mut end = max_bytes.saturating_sub(suffix.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &value[..end])
}

fn jsonrpc_id_summary(id: Option<&Value>) -> String {
    match id {
        None => "notification".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(value)) => bounded_log_text(value, 64),
        Some(Value::Bool(_)) => "<boolean>".to_string(),
        Some(Value::Array(_)) => "<array>".to_string(),
        Some(Value::Object(_)) => "<object>".to_string(),
    }
}

fn is_valid_bounded_jsonrpc_id(id: &Value) -> bool {
    match id {
        Value::Null | Value::Number(_) => true,
        Value::String(value) => value.len() <= MAX_JSONRPC_ID_BYTES,
        Value::Bool(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

enum BoundedFrame {
    Text(String),
    TooLarge,
    InvalidUtf8,
}

/// Read and drain one LF-delimited frame while retaining at most the configured
/// limit. Oversized frames are discarded through the newline without growing
/// memory further, keeping the next request aligned.
async fn read_bounded_frame<R>(reader: &mut R) -> Result<Option<BoundedFrame>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::with_capacity(8 * 1024);
    let mut saw_input = false;
    let mut too_large = false;

    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if !saw_input {
                return Ok(None);
            }
            break;
        }
        saw_input = true;

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let segment_len = newline.unwrap_or(buffer.len());
        let consumed = newline.map_or(segment_len, |position| position + 1);

        if !too_large {
            if bytes.len().saturating_add(segment_len) > MAX_JSONRPC_FRAME_BYTES {
                too_large = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&buffer[..segment_len]);
            }
        }
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }

    if too_large {
        return Ok(Some(BoundedFrame::TooLarge));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Some(BoundedFrame::Text(text))),
        Err(_) => Ok(Some(BoundedFrame::InvalidUtf8)),
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

struct DispatchContext<'a, W> {
    engine: &'a Arc<RwLock<Engine>>,
    federation: &'a Option<Arc<FederatedEngine>>,
    writer: &'a mut BufWriter<W>,
    profile: &'a mut McpProfile,
    profile_ceiling: McpProfile,
}

async fn dispatch<W>(
    context: DispatchContext<'_, W>,
    id: Value,
    method: &str,
    params: Option<Value>,
) -> Value
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match method {
        "initialize" => handle_initialize(id, params),
        "initialized" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => handle_tools_list(id, context.federation.is_some(), *context.profile),
        "tools/call" => {
            handle_tools_call(
                context.engine,
                id,
                params,
                context.federation,
                context.writer,
                context.profile,
                context.profile_ceiling,
            )
            .await
        }
        _ => {
            let err = JsonRpcError::method_not_found(id, &bounded_log_text(method, 128));
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
    profile_ceiling: McpProfile,
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
        Some(n) if n.len() <= MAX_MCP_TOOL_NAME_BYTES => n.to_string(),
        Some(_) => {
            let err = JsonRpcError::invalid_params(
                id,
                &format!("tools/call name exceeds {MAX_MCP_TOOL_NAME_BYTES} bytes"),
            );
            return serde_json::to_value(err).unwrap_or(Value::Null);
        }
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
        return handle_profile_management_tool(
            id,
            &tool_name,
            &args,
            profile,
            profile_ceiling,
            engine,
            writer,
        )
        .await;
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

/// When switching to a write-capable profile, swap a read-only engine for a
/// writer if the Tantivy write lock is free.
///
/// Read-only-profile servers open the engine without the writer (see
/// [`McpProfile::is_read_only_profile`]); once the agent upgrades to
/// `editor`/`dangerous`, the write tools need a real writer. Returns a note
/// for the profile-switch response when the engine state is worth mentioning.
pub(crate) fn upgrade_engine_for_profile(
    engine: &Arc<RwLock<Engine>>,
    next: McpProfile,
) -> Option<String> {
    if next.is_read_only_profile() {
        return None;
    }
    let root = {
        let guard = engine.read().unwrap_or_else(|e| e.into_inner());
        if !guard.is_read_only() {
            return None;
        }
        guard.root().to_path_buf()
    };
    match Engine::open(&root) {
        Ok(writer) if !writer.is_read_only() => {
            *engine.write().unwrap_or_else(|e| e.into_inner()) = writer;
            info!("engine upgraded to read-write for profile switch");
            Some("Engine upgraded to read-write; mutation tools are fully functional.".to_string())
        }
        Ok(_) => Some(
            "Engine remains read-only — another process holds the write lock; \
             mutation tools will return errors until it exits."
                .to_string(),
        ),
        Err(err) => Some(format!("Engine remains read-only — reopen failed: {err}")),
    }
}

async fn handle_profile_management_tool<W>(
    id: Value,
    tool_name: &str,
    args: &Value,
    profile: &mut McpProfile,
    profile_ceiling: McpProfile,
    engine: &Arc<RwLock<Engine>>,
    writer: &mut BufWriter<W>,
) -> Value
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match tool_name {
        "get_mcp_profile" => build_tool_response(
            id,
            tool_name.to_string(),
            Ok((
                profile_status_json(*profile, None, false, profile_ceiling),
                false,
            )),
        ),
        "set_mcp_profile" => {
            let requested = match args.get("profile").and_then(|v| v.as_str()) {
                Some(profile) if profile.len() <= 32 => profile,
                Some(_) => {
                    return build_tool_response(
                        id,
                        tool_name.to_string(),
                        Ok(("Profile name exceeds 32 bytes.".to_string(), true)),
                    );
                }
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

            if next_profile > profile_ceiling {
                return build_tool_response(
                    id,
                    tool_name.to_string(),
                    Ok((
                        format!(
                            "MCP profile '{}' exceeds this server's '{}' runtime ceiling. Restart codixing-mcp with --allow-profile-escalation to permit write-profile upgrades.",
                            next_profile.as_str(),
                            profile_ceiling.as_str(),
                        ),
                        true,
                    )),
                );
            }

            let previous = *profile;
            let changed = previous != next_profile;
            *profile = next_profile;

            // A read-only-profile server opened the engine without the writer;
            // moving to a write-capable profile needs a real writer for the
            // mutation tools. Run in spawn_blocking — Engine::open does I/O.
            let engine_note = if changed {
                let engine_clone = Arc::clone(engine);
                tokio::task::spawn_blocking(move || {
                    upgrade_engine_for_profile(&engine_clone, next_profile)
                })
                .await
                .unwrap_or(None)
            } else {
                None
            };

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
                    profile_status_json_with_note(
                        *profile,
                        Some(previous),
                        changed,
                        engine_note.as_deref(),
                        profile_ceiling,
                    ),
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
    profile_ceiling: McpProfile,
) -> String {
    profile_status_json_with_note(
        active_profile,
        previous_profile,
        changed,
        None,
        profile_ceiling,
    )
}

fn profile_status_json_with_note(
    active_profile: McpProfile,
    previous_profile: Option<McpProfile>,
    changed: bool,
    engine_note: Option<&str>,
    profile_ceiling: McpProfile,
) -> String {
    let base_message = if changed {
        "MCP profile updated. Clients should refresh tools/list; a notifications/tools/list_changed event was emitted."
    } else {
        "MCP profile unchanged."
    };
    let message = match engine_note {
        Some(note) => format!("{base_message} {note}"),
        None => base_message.to_string(),
    };

    let payload = json!({
        "schema_version": 1,
        "active_profile": active_profile.as_str(),
        "maximum_profile": profile_ceiling.as_str(),
        "write_profile_escalation_enabled": profile_ceiling == McpProfile::Dangerous,
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
    use tokio::io::{AsyncReadExt, BufReader};

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

    #[test]
    fn read_only_profiles_classified() {
        assert!(McpProfile::Minimal.is_read_only_profile());
        assert!(McpProfile::Reviewer.is_read_only_profile());
        assert!(!McpProfile::Editor.is_read_only_profile());
        assert!(!McpProfile::Dangerous.is_read_only_profile());
    }

    #[test]
    fn minimal_profile_is_the_library_default() {
        assert_eq!(McpProfile::default(), McpProfile::Minimal);
    }

    #[test]
    fn read_only_startup_profiles_have_a_read_only_runtime_ceiling() {
        assert_eq!(
            profile_ceiling(McpProfile::Minimal, false),
            McpProfile::Reviewer
        );
        assert_eq!(
            profile_ceiling(McpProfile::Reviewer, false),
            McpProfile::Reviewer
        );
        assert_eq!(
            profile_ceiling(McpProfile::Editor, false),
            McpProfile::Editor
        );
        assert_eq!(
            profile_ceiling(McpProfile::Minimal, true),
            McpProfile::Dangerous
        );
    }

    #[tokio::test]
    async fn bounded_frame_discards_oversized_input_and_realigns() {
        let mut input = vec![b'x'; MAX_JSONRPC_FRAME_BYTES + 1];
        input.extend_from_slice(b"\n{}\n");
        let mut reader = BufReader::new(input.as_slice());

        assert!(matches!(
            read_bounded_frame(&mut reader).await.unwrap(),
            Some(BoundedFrame::TooLarge)
        ));
        match read_bounded_frame(&mut reader).await.unwrap() {
            Some(BoundedFrame::Text(frame)) => assert_eq!(frame, "{}"),
            _ => panic!("reader did not realign after oversized frame"),
        }
    }

    #[test]
    fn debug_summaries_are_utf8_safe_and_strictly_bounded() {
        let summary = bounded_log_text(&"é".repeat(100), 64);
        assert!(summary.len() <= 64);
        assert!(summary.ends_with('…'));
        assert_eq!(
            jsonrpc_id_summary(Some(&json!({"large": "secret"}))),
            "<object>"
        );
    }

    #[test]
    fn reviewer_profile_allows_every_cli_daemon_proxy_tool() {
        // Keep this list aligned with crates/cli/src/daemon_proxy.rs. The CLI
        // upgrades only its connection from minimal to reviewer before using
        // these wrappers, so every target must remain read-only and available
        // in that profile.
        const CLI_PROXY_TOOLS: [&str; 8] = [
            "code_search",
            "find_symbol",
            "search_usages",
            "change_impact",
            "get_repo_map",
            "file_callers",
            "file_callees",
            "grep_code",
        ];

        for tool in CLI_PROXY_TOOLS {
            assert!(
                tools::is_read_only_tool(tool),
                "CLI daemon proxy target {tool} must remain read-only"
            );
            assert!(
                McpProfile::Reviewer.allows_tool(tool),
                "reviewer must allow CLI daemon proxy target {tool}"
            );
        }
    }

    #[test]
    fn federation_config_mutators_require_at_least_editor_profile() {
        const FEDERATION_MUTATORS: [&str; 4] = [
            "federation_init",
            "federation_add_project",
            "federation_remove_project",
            "federation_discover",
        ];

        for tool in FEDERATION_MUTATORS {
            assert!(
                !tools::is_read_only_tool(tool),
                "{tool} writes config state"
            );
            assert!(
                !McpProfile::Reviewer.allows_tool(tool),
                "reviewer must block federation mutator {tool}"
            );
            assert!(
                McpProfile::Editor.allows_tool(tool),
                "editor should allow federation mutator {tool}"
            );
        }
    }

    #[test]
    fn upgrade_to_write_profile_swaps_read_only_engine_for_writer() {
        let dir = tempfile::tempdir().unwrap();
        drop(make_test_engine(dir.path()));

        let ro = Engine::open_read_only(dir.path()).unwrap();
        assert!(ro.is_read_only());
        let engine = Arc::new(RwLock::new(ro));

        upgrade_engine_for_profile(&engine, McpProfile::Editor);

        assert!(
            !engine.read().unwrap().is_read_only(),
            "switching to a write-capable profile must acquire the writer when the lock is free"
        );
    }

    #[test]
    fn upgrade_skipped_when_target_profile_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        drop(make_test_engine(dir.path()));

        let ro = Engine::open_read_only(dir.path()).unwrap();
        let engine = Arc::new(RwLock::new(ro));

        upgrade_engine_for_profile(&engine, McpProfile::Minimal);

        assert!(
            engine.read().unwrap().is_read_only(),
            "read-only target profiles must not grab the writer lock"
        );
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
        run_requests_with_ceiling(engine, requests, profile, profile_ceiling(profile, false)).await
    }

    async fn run_requests_with_ceiling(
        engine: Engine,
        requests: &[Value],
        profile: McpProfile,
        ceiling: McpProfile,
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
                BufReader::new(server_read),
                BufWriter::new(server_write),
                None,
                profile,
                ceiling,
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
    async fn read_only_runtime_ceiling_blocks_write_profile_escalation() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());
        let responses = run_requests_with_profile(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "set_mcp_profile",
                    "arguments": { "profile": "editor" }
                }
            })],
            McpProfile::Minimal,
        )
        .await;

        assert_eq!(responses[0]["result"]["isError"], true);
        let text = responses[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("runtime ceiling"), "unexpected: {text}");
    }

    #[tokio::test]
    async fn explicit_escalation_policy_allows_editor_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());
        let responses = run_requests_with_ceiling(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "set_mcp_profile",
                    "arguments": { "profile": "editor" }
                }
            })],
            McpProfile::Minimal,
            McpProfile::Dangerous,
        )
        .await;

        let response = responses
            .iter()
            .find(|response| response["id"] == 1)
            .expect("missing profile escalation response");
        assert_eq!(response["result"]["isError"], false);
    }

    #[tokio::test]
    async fn invalid_jsonrpc_version_returns_invalid_request() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());
        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "1.0",
                "id": 7,
                "method": "tools/list"
            })],
        )
        .await;

        assert_eq!(responses[0]["error"]["code"], -32600);
        assert_eq!(responses[0]["id"], 7);
    }

    #[tokio::test]
    async fn invalid_jsonrpc_identifier_type_is_not_echoed() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_test_engine(dir.path());
        let responses = run_requests(
            engine,
            &[json!({
                "jsonrpc": "2.0",
                "id": {"attacker": "controlled"},
                "method": "tools/list"
            })],
        )
        .await;

        assert_eq!(responses[0]["error"]["code"], -32600);
        assert_eq!(responses[0]["id"], Value::Null);
        assert!(!responses[0].to_string().contains("attacker"));
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error_and_null_id() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Arc::new(RwLock::new(make_test_engine(dir.path())));
        let (client_stream, server_stream) = tokio::io::duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_stream);
        let (mut client_read, mut client_write) = tokio::io::split(client_stream);

        client_write.write_all(b"{not-json}\n").await.unwrap();
        client_write.shutdown().await.unwrap();
        let loop_handle = tokio::spawn(async move {
            run_jsonrpc_loop(
                engine,
                BufReader::new(server_read),
                BufWriter::new(server_write),
                None,
                McpProfile::Minimal,
                McpProfile::Reviewer,
            )
            .await
            .unwrap();
        });

        let mut output = Vec::new();
        client_read.read_to_end(&mut output).await.unwrap();
        loop_handle.await.unwrap();
        let response: Value = serde_json::from_slice(
            output
                .split(|byte| *byte == b'\n')
                .find(|line| !line.is_empty())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(response["error"]["code"], -32700);
        assert_eq!(response["id"], Value::Null);
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
                profile_ceiling(McpProfile::default(), false),
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
                BufReader::new(server_read),
                BufWriter::new(server_write),
                None,
                McpProfile::default(),
                profile_ceiling(McpProfile::default(), false),
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
