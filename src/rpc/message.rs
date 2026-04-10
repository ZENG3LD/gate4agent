//! JSON-RPC 2.0 wire-level types and line classifier.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 request/response ID — number or string per spec.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(u64),
    String(String),
}

/// JSON-RPC 2.0 request (host → agent or agent → host).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: RpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: RpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("JSON-RPC error {code}: {message}")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 notification (no id, no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcError {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    /// ACP-specific codes (per spec section 2.5).
    pub const NOT_FOUND: i32 = -32001;
    pub const PERMISSION_DENIED: i32 = -32002;
    pub const INVALID_STATE: i32 = -32003;
    pub const UNSUPPORTED: i32 = -32004;

    /// Create a "Method not found" error.
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: Self::METHOD_NOT_FOUND,
            message: format!("Method not found: {}", method),
            data: None,
        }
    }

    /// Create an internal error.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: Self::INTERNAL_ERROR,
            message: msg.into(),
            data: None,
        }
    }
}

impl RpcResponse {
    /// Create a success response.
    pub fn success(id: RpcId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error_response(id: RpcId, err: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(err),
        }
    }
}

impl RpcRequest {
    /// Create a new request.
    pub fn new(id: impl Into<RpcId>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

impl From<u64> for RpcId {
    fn from(n: u64) -> Self {
        RpcId::Number(n)
    }
}

impl From<String> for RpcId {
    fn from(s: String) -> Self {
        RpcId::String(s)
    }
}

impl From<&str> for RpcId {
    fn from(s: &str) -> Self {
        RpcId::String(s.to_owned())
    }
}

/// Discriminated incoming message — result of parsing one stdout line.
#[derive(Debug)]
pub enum IncomingMessage {
    /// Agent sent a request (expects a response from host).
    Request { id: RpcId, method: String, params: Option<Value> },
    /// Agent sent a notification (no response expected).
    Notification { method: String, params: Option<Value> },
    /// Agent responded to one of host's pending requests.
    Response { id: RpcId, result: Option<Value>, error: Option<RpcError> },
    /// Not a JSON-RPC line — pass to legacy NdjsonParser.
    Legacy(String),
}

/// Parse a raw stdout line into an [`IncomingMessage`].
///
/// Classification rules (per JSON-RPC 2.0 spec):
/// - Must have `"jsonrpc": "2.0"` field to be considered JSON-RPC.
/// - Has `"id"` + `"method"` → Request.
/// - Has `"method"` but no `"id"` → Notification.
/// - Has `"id"` but no `"method"` → Response (success or error).
/// - Otherwise → Legacy (forward to NdjsonParser).
pub fn classify_line(line: &str) -> IncomingMessage {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return IncomingMessage::Legacy(line.to_owned());
    }

    let val: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return IncomingMessage::Legacy(line.to_owned()),
    };

    if val.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return IncomingMessage::Legacy(line.to_owned());
    }

    let id = val.get("id").and_then(parse_rpc_id);
    let method = val
        .get("method")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let params = val.get("params").cloned();

    match (id, method) {
        (Some(id), Some(method)) => IncomingMessage::Request { id, method, params },
        (None, Some(method)) => IncomingMessage::Notification { method, params },
        (Some(id), None) => {
            let result = val.get("result").cloned();
            let error = val
                .get("error")
                .and_then(|e| serde_json::from_value(e.clone()).ok());
            IncomingMessage::Response { id, result, error }
        }
        (None, None) => IncomingMessage::Legacy(line.to_owned()),
    }
}

fn parse_rpc_id(val: &Value) -> Option<RpcId> {
    match val {
        Value::Number(n) => n.as_u64().map(RpcId::Number),
        Value::String(s) => Some(RpcId::String(s.clone())),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_request() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{"path":"/tmp/x"}}"#;
        match classify_line(line) {
            IncomingMessage::Request { id, method, params } => {
                assert_eq!(id, RpcId::Number(1));
                assert_eq!(method, "fs/read_text_file");
                assert!(params.is_some());
            }
            other => panic!("expected Request, got {:?}", other),
        }
    }

    #[test]
    fn classify_request_string_id() {
        let line = r#"{"jsonrpc":"2.0","id":"abc","method":"ping","params":null}"#;
        match classify_line(line) {
            IncomingMessage::Request { id, .. } => {
                assert_eq!(id, RpcId::String("abc".into()));
            }
            other => panic!("expected Request, got {:?}", other),
        }
    }

    #[test]
    fn classify_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"session/update","params":{"type":"text"}}"#;
        match classify_line(line) {
            IncomingMessage::Notification { method, params } => {
                assert_eq!(method, "session/update");
                assert!(params.is_some());
            }
            other => panic!("expected Notification, got {:?}", other),
        }
    }

    #[test]
    fn classify_response_success() {
        let line = r#"{"jsonrpc":"2.0","id":42,"result":{"content":"hello"}}"#;
        match classify_line(line) {
            IncomingMessage::Response { id, result, error } => {
                assert_eq!(id, RpcId::Number(42));
                assert!(result.is_some());
                assert!(error.is_none());
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn classify_response_error() {
        let line = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Method not found"}}"#;
        match classify_line(line) {
            IncomingMessage::Response { id, result, error } => {
                assert_eq!(id, RpcId::Number(7));
                assert!(result.is_none());
                let err = error.unwrap();
                assert_eq!(err.code, -32601);
            }
            other => panic!("expected Response, got {:?}", other),
        }
    }

    #[test]
    fn classify_legacy_non_json() {
        let line = "this is plain text";
        assert!(matches!(classify_line(line), IncomingMessage::Legacy(_)));
    }

    #[test]
    fn classify_legacy_json_no_jsonrpc_field() {
        let line = r#"{"type":"text","content":"hello"}"#;
        assert!(matches!(classify_line(line), IncomingMessage::Legacy(_)));
    }

    #[test]
    fn classify_legacy_jsonrpc_v1() {
        let line = r#"{"jsonrpc":"1.0","id":1,"method":"test"}"#;
        assert!(matches!(classify_line(line), IncomingMessage::Legacy(_)));
    }

    #[test]
    fn classify_empty_line() {
        assert!(matches!(classify_line(""), IncomingMessage::Legacy(_)));
        assert!(matches!(classify_line("   "), IncomingMessage::Legacy(_)));
    }

    #[test]
    fn rpc_response_no_null_fields_serialized() {
        let resp = RpcResponse::success(RpcId::Number(1), json!({"ok": true}));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(!s.contains("\"error\""), "error field should be absent");
        assert!(s.contains("\"result\""));
    }

    #[test]
    fn rpc_error_response_no_null_result() {
        let resp = RpcResponse::error_response(RpcId::Number(1), RpcError::method_not_found("foo"));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(!s.contains("\"result\""), "result field should be absent");
        assert!(s.contains("\"error\""));
    }
}
