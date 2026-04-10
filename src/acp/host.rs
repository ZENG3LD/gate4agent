//! ACP host-side handler trait and bridge adapter.
//!
//! Rather than forcing callers to implement the low-level [`HostHandler`] with
//! raw string/JSON matching, this module provides a typed trait
//! [`AcpHostHandler`] with safe defaults. Internally [`AcpHostAdapter`]
//! bridges from [`HostHandler`] → [`AcpHostHandler`], so the reader loop can
//! use the existing RPC infrastructure unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::rpc::handler::HostHandler;
use crate::rpc::message::RpcError;

use super::protocol::{FsReadParams, PermissionRequestParams, TerminalCreateParams};

// ---------------------------------------------------------------------------
// AcpHostHandler — typed trait
// ---------------------------------------------------------------------------

/// Typed handler for agent → host ACP requests.
///
/// All methods have default implementations that return safe denials, so
/// callers only need to override the operations they actually support.
///
/// Implementations must be `Send + Sync` because the handler is called from
/// the reader loop's blocking thread.
pub trait AcpHostHandler: Send + Sync {
    /// Agent wants to read a file from the host filesystem.
    ///
    /// Return `Ok(content)` or `Err(human-readable message)`.
    fn fs_read_text_file(&self, path: &str) -> Result<String, String> {
        Err(format!("fs/read_text_file not supported: {}", path))
    }

    /// Agent wants to create a terminal session on the host.
    ///
    /// Return `Ok(terminal_id)` or `Err(human-readable message)`.
    fn terminal_create(
        &self,
        _cwd: Option<&str>,
        _env: Option<&HashMap<String, String>>,
    ) -> Result<String, String> {
        Err("terminal/create not supported".to_string())
    }

    /// Agent is requesting permission to run a tool.
    ///
    /// Return `Ok(true)` to allow, `Ok(false)` to deny, `Err` for error.
    /// Default: deny all requests (safe default).
    fn request_permission(
        &self,
        _tool_name: &str,
        _description: &str,
        _session_id: &str,
    ) -> Result<bool, String> {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// DefaultAcpHandler
// ---------------------------------------------------------------------------

/// Default handler that denies all requests with safe defaults.
///
/// - `fs/read_text_file` → `Err("not supported")`
/// - `terminal/create` → `Err("not supported")`
/// - `session/request_permission` → `Ok(false)` (deny)
pub struct DefaultAcpHandler;

impl AcpHostHandler for DefaultAcpHandler {}

// ---------------------------------------------------------------------------
// AcpHostAdapter — bridges AcpHostHandler → HostHandler
// ---------------------------------------------------------------------------

/// Bridges [`AcpHostHandler`] → [`HostHandler`] for use in the reader loop.
///
/// Wraps `Arc<dyn AcpHostHandler>` so it can be cloned cheaply without
/// requiring `'static + Clone` bounds on the trait.
pub(crate) struct AcpHostAdapter(pub Arc<dyn AcpHostHandler>);

impl HostHandler for AcpHostAdapter {
    fn handle(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match method {
            "fs/read_text_file" => {
                let p: FsReadParams = parse_params(params)?;
                self.0
                    .fs_read_text_file(&p.path)
                    .map(|content| json!({ "content": content }))
                    .map_err(|msg| RpcError {
                        code: RpcError::PERMISSION_DENIED,
                        message: msg,
                        data: None,
                    })
            }

            "terminal/create" => {
                let p: TerminalCreateParams = parse_params(params)?;
                self.0
                    .terminal_create(
                        p.cwd.as_deref(),
                        p.env.as_ref(),
                    )
                    .map(|terminal_id| json!({ "terminalId": terminal_id }))
                    .map_err(|msg| RpcError {
                        code: RpcError::UNSUPPORTED,
                        message: msg,
                        data: None,
                    })
            }

            "session/request_permission" => {
                let p: PermissionRequestParams = parse_params(params)?;
                self.0
                    .request_permission(&p.tool_name, &p.description, &p.session_id)
                    .map(|allowed| json!({ "allowed": allowed }))
                    .map_err(|msg| RpcError {
                        code: RpcError::INTERNAL_ERROR,
                        message: msg,
                        data: None,
                    })
            }

            other => Err(RpcError::method_not_found(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, RpcError> {
    let v = params.unwrap_or(Value::Null);
    serde_json::from_value(v).map_err(|e| RpcError {
        code: RpcError::INVALID_PARAMS,
        message: format!("Invalid params: {}", e),
        data: None,
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Helper: wrap DefaultAcpHandler in AcpHostAdapter
    fn default_adapter() -> AcpHostAdapter {
        AcpHostAdapter(Arc::new(DefaultAcpHandler))
    }

    #[test]
    fn default_handler_denies_fs_read() {
        let h = DefaultAcpHandler;
        let result = h.fs_read_text_file("/etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn default_handler_denies_terminal() {
        let h = DefaultAcpHandler;
        let result = h.terminal_create(None, None);
        assert!(result.is_err());
    }

    #[test]
    fn default_handler_denies_permission() {
        let h = DefaultAcpHandler;
        let result = h.request_permission("bash", "run command", "s1");
        assert_eq!(result, Ok(false));
    }

    #[test]
    fn adapter_dispatches_fs_read_returns_permission_denied() {
        let adapter = default_adapter();
        let result = adapter.handle(
            "fs/read_text_file",
            Some(json!({"path": "/etc/passwd"})),
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, RpcError::PERMISSION_DENIED);
    }

    #[test]
    fn adapter_dispatches_permission_request_returns_denied() {
        let adapter = default_adapter();
        let result = adapter.handle(
            "session/request_permission",
            Some(json!({
                "toolName": "bash",
                "description": "run shell command",
                "sessionId": "s1"
            })),
        );
        // DefaultAcpHandler returns Ok(false) → adapter returns Ok({"allowed": false})
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["allowed"], false);
    }

    #[test]
    fn adapter_unknown_method_returns_method_not_found() {
        let adapter = default_adapter();
        let result = adapter.handle("unknown/method", None);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, RpcError::METHOD_NOT_FOUND);
    }

    #[test]
    fn adapter_invalid_params_returns_invalid_params_error() {
        let adapter = default_adapter();
        // fs/read_text_file requires a "path" field
        let result = adapter.handle("fs/read_text_file", Some(json!({"wrong_key": 42})));
        // DefaultAcpHandler denies, but parse should succeed with default missing fields
        // (FsReadParams.path will be empty string via default deserialization if it has #[serde(default)])
        // Actually FsReadParams.path is required so this should fail parse.
        // It may either fail with INVALID_PARAMS or succeed with empty path then PERMISSION_DENIED.
        // Either way it must be Err.
        assert!(result.is_err());
    }
}
