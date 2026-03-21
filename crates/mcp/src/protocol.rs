//! JSON-RPC 2.0 wire types for the MCP protocol.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// An incoming JSON-RPC 2.0 request or notification.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    /// `None` means this is a notification (no response expected).
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// A successful JSON-RPC 2.0 response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub result: Value,
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub jsonrpc: String,
    pub id: Value,
    pub error: RpcError,
}

/// JSON-RPC error detail object.
#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn new(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        }
    }
}

impl JsonRpcError {
    /// -32601 Method not found.
    pub fn method_not_found(id: Value, method: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            error: RpcError {
                code: -32601,
                message: format!("Method not found: {method}"),
            },
        }
    }

    /// -32602 Invalid params.
    pub fn invalid_params(id: Value, msg: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            error: RpcError {
                code: -32602,
                message: msg.to_string(),
            },
        }
    }

    /// -32603 Internal error.
    pub fn internal_error(id: Value, msg: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            error: RpcError {
                code: -32603,
                message: msg.to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Progress notifications (MCP `notifications/progress`)
// ---------------------------------------------------------------------------

/// A progress notification sent from server to client during a long-running
/// tool call.  This is a JSON-RPC notification (no `id` field).
#[derive(Debug, Clone)]
pub struct ProgressNotification {
    pub progress_token: String,
    pub progress: u32,
    pub total: u32,
    pub message: String,
}

impl ProgressNotification {
    /// Serialize to the JSON-RPC wire format.
    pub fn to_json(&self) -> Value {
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": self.progress_token,
                "progress": self.progress,
                "total": self.total,
                "message": self.message,
            }
        })
    }
}
