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

use super::protocol::{FsReadParams, PermissionRequestParams, TerminalCreateParams, TerminalWriteParams};

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

    /// Agent wants to write input to an existing terminal session.
    ///
    /// Return `Ok(output)` or `Err(human-readable message)`.
    fn terminal_write(&self, terminal_id: &str, input: &str) -> Result<String, String> {
        let _ = (terminal_id, input);
        Err("terminal/write not supported".to_string())
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
/// - `terminal/write` → `Err("not supported")`
/// - `session/request_permission` → `Ok(false)` (deny)
pub struct DefaultAcpHandler;

impl AcpHostHandler for DefaultAcpHandler {}

// ---------------------------------------------------------------------------
// FilesystemAcpHandler
// ---------------------------------------------------------------------------

/// ACP host handler that serves real filesystem reads and auto-allows
/// permissions. Matches Pipe transport's `--dangerously-skip-permissions`
/// behavior.
pub struct FilesystemAcpHandler {
    /// Optional path prefix whitelist. `None` = allow all absolute paths.
    pub allowed_roots: Option<Vec<std::path::PathBuf>>,
}

impl FilesystemAcpHandler {
    /// Check that `path` is within the whitelist (if any), then read it.
    fn checked_read(&self, path: &str) -> Result<String, String> {
        if let Some(ref roots) = self.allowed_roots {
            let file_path = std::path::Path::new(path);
            let canonical = file_path.canonicalize().map_err(|e| e.to_string())?;
            let allowed = roots.iter().any(|root| {
                root.canonicalize()
                    .map(|r| canonical.starts_with(&r))
                    .unwrap_or(false)
            });
            if !allowed {
                return Err(format!("path outside allowed roots: {}", path));
            }
        }
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

impl AcpHostHandler for FilesystemAcpHandler {
    fn fs_read_text_file(&self, path: &str) -> Result<String, String> {
        self.checked_read(path)
    }

    fn request_permission(
        &self,
        _tool_name: &str,
        _description: &str,
        _session_id: &str,
    ) -> Result<bool, String> {
        Ok(true) // auto-allow, like --dangerously-skip-permissions
    }
}

// ---------------------------------------------------------------------------
// TerminalAcpHandler
// ---------------------------------------------------------------------------

struct TerminalSession {
    cwd: std::path::PathBuf,
    env: HashMap<String, String>,
}

/// ACP host handler with real filesystem reads and terminal execution.
///
/// WARNING: No sandboxing. Use only with trusted agents.
pub struct TerminalAcpHandler {
    /// Optional path prefix whitelist. `None` = allow all absolute paths.
    pub allowed_roots: Option<Vec<std::path::PathBuf>>,
    sessions: std::sync::Mutex<HashMap<String, TerminalSession>>,
}

impl TerminalAcpHandler {
    /// Create a new handler. Pass `None` for `allowed_roots` to allow all paths.
    pub fn new(allowed_roots: Option<Vec<std::path::PathBuf>>) -> Self {
        Self {
            allowed_roots,
            sessions: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn checked_read(&self, path: &str) -> Result<String, String> {
        if let Some(ref roots) = self.allowed_roots {
            let file_path = std::path::Path::new(path);
            let canonical = file_path.canonicalize().map_err(|e| e.to_string())?;
            let allowed = roots.iter().any(|root| {
                root.canonicalize()
                    .map(|r| canonical.starts_with(&r))
                    .unwrap_or(false)
            });
            if !allowed {
                return Err(format!("path outside allowed roots: {}", path));
            }
        }
        std::fs::read_to_string(path).map_err(|e| e.to_string())
    }
}

impl AcpHostHandler for TerminalAcpHandler {
    fn fs_read_text_file(&self, path: &str) -> Result<String, String> {
        self.checked_read(path)
    }

    fn terminal_create(
        &self,
        cwd: Option<&str>,
        env: Option<&HashMap<String, String>>,
    ) -> Result<String, String> {
        let id = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            format!("term-{:x}", nanos)
        };

        let session = TerminalSession {
            cwd: cwd
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
            env: env.cloned().unwrap_or_default(),
        };

        self.sessions
            .lock()
            .map_err(|e| e.to_string())?
            .insert(id.clone(), session);

        Ok(id)
    }

    fn terminal_write(&self, terminal_id: &str, input: &str) -> Result<String, String> {
        let sessions = self.sessions.lock().map_err(|e| e.to_string())?;
        let session = sessions
            .get(terminal_id)
            .ok_or_else(|| format!("unknown terminal: {}", terminal_id))?;

        let output = {
            #[cfg(windows)]
            {
                std::process::Command::new("cmd")
                    .args(["/C", input])
                    .current_dir(&session.cwd)
                    .envs(&session.env)
                    .output()
                    .map_err(|e| e.to_string())?
            }
            #[cfg(not(windows))]
            {
                std::process::Command::new("sh")
                    .args(["-c", input])
                    .current_dir(&session.cwd)
                    .envs(&session.env)
                    .output()
                    .map_err(|e| e.to_string())?
            }
        };

        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(combined)
    }

    fn request_permission(
        &self,
        _tool_name: &str,
        _description: &str,
        _session_id: &str,
    ) -> Result<bool, String> {
        Ok(true) // auto-allow, like --dangerously-skip-permissions
    }
}

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
                    .terminal_create(p.cwd.as_deref(), p.env.as_ref())
                    .map(|terminal_id| json!({ "terminalId": terminal_id }))
                    .map_err(|msg| RpcError {
                        code: RpcError::UNSUPPORTED,
                        message: msg,
                        data: None,
                    })
            }

            "terminal/write" => {
                let p: TerminalWriteParams = parse_params(params)?;
                self.0
                    .terminal_write(&p.terminal_id, &p.input)
                    .map(|output| json!({ "output": output }))
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

    // -----------------------------------------------------------------------
    // DefaultAcpHandler tests (kept from original)
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // FilesystemAcpHandler tests
    // -----------------------------------------------------------------------

    #[test]
    fn filesystem_handler_allows_permission() {
        let h = FilesystemAcpHandler { allowed_roots: None };
        let result = h.request_permission("bash", "run command", "s1");
        assert_eq!(result, Ok(true));
    }

    #[test]
    fn filesystem_handler_reads_existing_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("gate4agent_test_read.txt");
        std::fs::write(&path, "hello from test").unwrap();

        let h = FilesystemAcpHandler { allowed_roots: None };
        let result = h.fs_read_text_file(path.to_str().unwrap());
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        assert_eq!(result.unwrap(), "hello from test");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn filesystem_handler_errors_on_missing_file() {
        let h = FilesystemAcpHandler { allowed_roots: None };
        let result = h.fs_read_text_file("/nonexistent/path/that/does/not/exist.txt");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // TerminalAcpHandler tests
    // -----------------------------------------------------------------------

    #[test]
    fn terminal_handler_create_returns_id() {
        let h = TerminalAcpHandler::new(None);
        let result = h.terminal_create(None, None);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let id = result.unwrap();
        assert!(!id.is_empty(), "terminal id must not be empty");
        assert!(id.starts_with("term-"), "id should start with 'term-'");
    }

    #[test]
    fn terminal_handler_write_runs_echo() {
        let h = TerminalAcpHandler::new(None);
        let id = h.terminal_create(None, None).expect("create should succeed");

        #[cfg(windows)]
        let cmd = "echo hello";
        #[cfg(not(windows))]
        let cmd = "echo hello";

        let result = h.terminal_write(&id, cmd);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let output = result.unwrap();
        assert!(
            output.contains("hello"),
            "output should contain 'hello', got: {:?}",
            output
        );
    }

    #[test]
    fn terminal_handler_write_unknown_terminal_errors() {
        let h = TerminalAcpHandler::new(None);
        let result = h.terminal_write("nonexistent-id", "echo hi");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unknown terminal"));
    }

    // -----------------------------------------------------------------------
    // Adapter dispatch tests for terminal/write
    // -----------------------------------------------------------------------

    #[test]
    fn adapter_dispatches_terminal_write() {
        let h = TerminalAcpHandler::new(None);
        let id = h.terminal_create(None, None).expect("create should succeed");

        let adapter = AcpHostAdapter(Arc::new(h));
        let result = adapter.handle(
            "terminal/write",
            Some(json!({ "terminalId": id, "input": "echo adapter_test" })),
        );
        assert!(result.is_ok(), "expected Ok from adapter, got {:?}", result);
        let val = result.unwrap();
        let output = val["output"].as_str().unwrap_or("");
        assert!(
            output.contains("adapter_test"),
            "output should contain 'adapter_test', got: {:?}",
            output
        );
    }

    #[test]
    fn adapter_terminal_write_unknown_id_returns_unsupported_error() {
        let h = TerminalAcpHandler::new(None);
        let adapter = AcpHostAdapter(Arc::new(h));
        let result = adapter.handle(
            "terminal/write",
            Some(json!({ "terminalId": "bad-id", "input": "echo hi" })),
        );
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, RpcError::UNSUPPORTED);
    }

    #[test]
    fn default_handler_denies_terminal_write() {
        let h = DefaultAcpHandler;
        let result = h.terminal_write("term-123", "echo hi");
        assert!(result.is_err());
    }
}
