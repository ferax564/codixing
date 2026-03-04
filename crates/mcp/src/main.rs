//! CodeForge MCP server — exposes code search and graph tools via the
//! Model Context Protocol (JSON-RPC 2.0 over stdin/stdout).
//!
//! **Important**: all tracing output is directed to *stderr* so that stdout
//! remains a clean JSON-RPC channel.

mod protocol;
mod tools;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use codeforge_core::{EmbeddingConfig, Engine, IndexConfig};

use protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};

/// CodeForge MCP server — JSON-RPC 2.0 over stdin/stdout.
#[derive(Parser)]
#[command(name = "codeforge-mcp", version, about)]
struct Args {
    /// Root directory containing the `.codeforge` index (created by `codeforge init`).
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Direct all tracing output to stderr — stdout is the JSON-RPC channel.
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

    // Auto-init: if no .codeforge/ index exists, build a BM25-only index on the
    // spot so `codeforge-mcp --root .` works out-of-the-box without a manual
    // `codeforge init` step.
    let engine = if Engine::index_exists(&root) {
        info!(root = %root.display(), "opening existing CodeForge index");
        Engine::open(&root).with_context(|| {
            format!(
                "failed to open index at {} — index directory exists but may be corrupt; \
                 delete .codeforge/ and restart to rebuild",
                root.display()
            )
        })?
    } else {
        info!(
            root = %root.display(),
            "no .codeforge/ index found — running automatic BM25-only init (no embeddings)"
        );
        let mut config = IndexConfig::new(&root);
        config.embedding = EmbeddingConfig {
            enabled: false,
            ..EmbeddingConfig::default()
        };
        Engine::init(&root, config).with_context(|| {
            format!(
                "auto-init failed at {} — ensure the directory exists and contains source files",
                root.display()
            )
        })?
    };

    let engine = Arc::new(Mutex::new(engine));

    info!("CodeForge MCP server ready — listening on stdin");

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();
    let mut writer = BufWriter::new(stdout);

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
                // Use null id for parse errors (we can't know the id).
                let err = JsonRpcError::internal_error(Value::Null, &format!("Parse error: {e}"));
                write_line(&mut writer, &err).await?;
                continue;
            }
        };

        // Notifications (no id) are ignored per JSON-RPC spec.
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

    info!("stdin closed, shutting down");
    Ok(())
}

/// Dispatch a JSON-RPC request to the appropriate handler.
///
/// Returns a serializable response (either `JsonRpcResponse` or `JsonRpcError`).
/// Encoded as `serde_json::Value` for uniform writing.
async fn dispatch(
    engine: &Arc<Mutex<Engine>>,
    id: Value,
    method: &str,
    params: Option<Value>,
) -> Value {
    match method {
        "initialize" => handle_initialize(id, params),
        "initialized" => {
            // Client notification — no response needed (but we already filtered notifs above).
            json!({"jsonrpc": "2.0", "id": id, "result": {}})
        }
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
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "codeforge",
            "version": "0.4.0"
        }
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
        let engine = match engine_arc.lock() {
            Ok(e) => e,
            Err(e) => {
                return (format!("Engine lock poisoned: {e}"), true);
            }
        };
        tools::dispatch_tool(&engine, &tool_name_clone, &args)
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

/// Serialize a value to a single JSON line and write it to the writer.
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
    writer.flush().await.context("failed to flush stdout")?;
    Ok(())
}
