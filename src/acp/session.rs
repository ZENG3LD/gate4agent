//! High-level ACP session: spawn, handshake, prompt, cancel, kill.
//!
//! [`AcpSession`] is the main public entry point for ACP transport. It
//! manages the subprocess lifecycle, performs the `initialize` + `session/new`
//! handshake, and exposes a simple `prompt()` / `subscribe()` API for callers.
//!
//! ## Lifecycle
//!
//! 1. `AcpSession::spawn()` — starts the process, runs the two-step handshake
//! 2. `session.prompt("...")` — sends `session/prompt`, returns on ack
//! 3. `session.subscribe()` — receives `AgentEvent` broadcast stream
//! 4. `session.cancel()` — sends `session/cancel` notification
//! 5. `session.kill()` — hard-kills the subprocess

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::core::error::AgentError;
use crate::core::types::{AgentEvent, CliTool};
use crate::rpc::id::IdGen;
use crate::rpc::message::{RpcNotification, RpcRequest};
use crate::rpc::pending::PendingRequests;

use super::host::{AcpHostAdapter, AcpHostHandler, FilesystemAcpHandler};
use super::protocol::{
    extract_token_usage, AgentCapabilities, ClientCapabilities, ClientInfo, ContentBlock,
    FsCapabilities, InitializeParams, McpServerConfig, SessionCancelParams, SessionLoadParams,
    SessionLoadResult, SessionNewParams, SessionPromptParams,
};
use super::reader::acp_reader_loop;
use super::spawn::AcpProcess;

// ---------------------------------------------------------------------------
// AcpError
// ---------------------------------------------------------------------------

/// Error variants for ACP session operations.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("Process spawn failed: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    #[error("Stdin write failed: {source}")]
    Write {
        #[source]
        source: std::io::Error,
    },

    #[error("JSON error: {source}")]
    Json {
        #[source]
        source: serde_json::Error,
    },

    #[error("Handshake timed out (step={step})")]
    HandshakeTimeout { step: &'static str },

    #[error("Handshake failed: {message}")]
    HandshakeFailed { message: String },

    #[error("Agent returned RPC error: {0}")]
    Agent(#[from] crate::rpc::message::RpcError),

    #[error("Request timed out (method={method})")]
    Timeout { method: String },

    #[error("Session not initialized — call session_new() first")]
    NoSession,

    #[error("Session closed while awaiting response")]
    SessionClosed,
}

// ---------------------------------------------------------------------------
// AcpSessionOptions
// ---------------------------------------------------------------------------

/// Options for constructing an [`AcpSession`].
pub struct AcpSessionOptions {
    /// Handler for agent → host requests. Default: [`FilesystemAcpHandler`] with no root restrictions.
    pub host_handler: Option<Box<dyn AcpHostHandler>>,

    /// Broadcast channel capacity. Default: 256.
    pub channel_capacity: usize,

    /// Timeout for `initialize` + `session/new` handshake. Default: 30 s.
    pub handshake_timeout: Duration,

    /// Timeout for `session/prompt` calls. Default: 120 s.
    pub prompt_timeout: Duration,

    /// MCP servers to pass to the agent on session creation. Default: empty.
    pub mcp_servers: Vec<McpServerConfig>,
}

impl Default for AcpSessionOptions {
    fn default() -> Self {
        Self {
            host_handler: None,
            channel_capacity: 256,
            handshake_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(120),
            mcp_servers: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// AcpSession
// ---------------------------------------------------------------------------

/// ACP session over a stdio JSON-RPC 2.0 transport.
///
/// Spawns a CLI tool in ACP mode, performs the `initialize` + `session/new`
/// handshake, and exposes multi-turn `prompt()` calls. All streaming events
/// arrive on the broadcast channel returned by [`subscribe()`](AcpSession::subscribe).
pub struct AcpSession {
    /// Local gate4agent session ID (UUID-style, NOT the ACP sessionId).
    local_session_id: String,
    /// ACP sessionId returned by `session/new` (required for subsequent requests).
    acp_session_id: Arc<tokio::sync::Mutex<Option<String>>>,
    tool: CliTool,
    tx: broadcast::Sender<AgentEvent>,
    /// Shared write handle to the process stdin (also used by reader loop for responses).
    process: Arc<Mutex<AcpProcess>>,
    pending: PendingRequests,
    id_gen: Arc<IdGen>,
    reader_task: JoinHandle<()>,
    prompt_timeout: Duration,
    /// Capabilities reported by the agent during `initialize`.
    agent_caps: AgentCapabilities,
}

impl AcpSession {
    /// Spawn the CLI tool in ACP mode and perform the `initialize` + `session/new` handshake.
    ///
    /// Blocks (async) until the handshake completes or `options.handshake_timeout` elapses.
    ///
    /// # Errors
    ///
    /// - [`AcpError::Spawn`] — child process failed to start
    /// - [`AcpError::HandshakeTimeout`] — `initialize` or `session/new` timed out
    /// - [`AcpError::HandshakeFailed`] — agent returned an RPC error during handshake
    pub async fn spawn(
        tool: CliTool,
        working_dir: &std::path::Path,
        options: AcpSessionOptions,
    ) -> Result<Self, AcpError> {
        // Extract mcp_servers before options fields are partially consumed below.
        let mcp_servers = options.mcp_servers;
        let options = AcpSessionOptions { mcp_servers: vec![], ..options };

        let proc = AcpProcess::spawn(tool, working_dir, &[])
            .map_err(|e| AcpError::Spawn { source: e })?;

        let local_session_id = generate_session_id();
        let (tx, _) = broadcast::channel::<AgentEvent>(options.channel_capacity);

        // Emit Started immediately so subscribers can see lifecycle from the start.
        let _ = tx.send(AgentEvent::Started {
            session_id: local_session_id.clone(),
        });

        let process = Arc::new(Mutex::new(proc));

        // Build the HostHandler for the reader loop.
        let handler: Arc<dyn crate::rpc::handler::HostHandler> = match options.host_handler {
            Some(h) => Arc::new(AcpHostAdapter(Arc::from(h))),
            None => Arc::new(AcpHostAdapter(Arc::new(FilesystemAcpHandler { allowed_roots: None }))),
        };

        let pending = PendingRequests::new();
        let id_gen = Arc::new(IdGen::new());

        // Clones for the reader loop task.
        let reader_process = Arc::clone(&process);
        let reader_tx = tx.clone();
        let reader_pending = pending.clone();

        let reader_task = tokio::task::spawn_blocking(move || {
            acp_reader_loop(reader_process, reader_tx, reader_pending, handler);
        });

        let acp_session_id = Arc::new(tokio::sync::Mutex::new(None::<String>));

        let mut session = Self {
            local_session_id: local_session_id.clone(),
            acp_session_id: Arc::clone(&acp_session_id),
            tool,
            tx: tx.clone(),
            process,
            pending,
            id_gen,
            reader_task,
            prompt_timeout: options.prompt_timeout,
            agent_caps: AgentCapabilities::default(),
        };

        // --- Handshake step 1: initialize (id=0 per ACP convention) ---
        let init_params = InitializeParams {
            protocol_version: 1,
            client_capabilities: ClientCapabilities {
                fs: FsCapabilities { read_text_file: true, write_text_file: true },
                terminal: true,
            },
            client_info: ClientInfo {
                name: "gate4agent",
                title: Some("Gate4Agent"),
                version: env!("CARGO_PKG_VERSION"),
            },
        };
        let caps: AgentCapabilities = session
            .rpc_call_typed("initialize", json!(init_params), options.handshake_timeout, true)
            .await
            .map_err(|e| match e {
                AcpError::Timeout { .. } => AcpError::HandshakeTimeout { step: "initialize" },
                AcpError::Agent(rpc_err) => AcpError::HandshakeFailed {
                    message: rpc_err.to_string(),
                },
                other => other,
            })?;
        session.agent_caps = caps;

        // --- Handshake step 2: session/new ---
        let new_params = SessionNewParams {
            cwd: working_dir.to_str().unwrap_or(".").to_string(),
            mcp_servers,
        };
        let new_result = session
            .rpc_call("session/new", Some(json!(new_params)), options.handshake_timeout)
            .await
            .map_err(|e| match e {
                AcpError::Timeout { .. } => AcpError::HandshakeTimeout { step: "session/new" },
                AcpError::Agent(rpc_err) => AcpError::HandshakeFailed {
                    message: rpc_err.to_string(),
                },
                other => other,
            })?;

        let acp_sid = new_result
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or(&local_session_id)
            .to_owned();

        {
            let mut guard = acp_session_id.lock().await;
            *guard = Some(acp_sid.clone());
        }

        let _ = tx.send(AgentEvent::SessionStart {
            session_id: acp_sid,
            model: "".to_string(),
            tools: vec![],
        });

        Ok(session)
    }

    /// Send a prompt to the agent.
    ///
    /// Returns once the agent acknowledges the `session/prompt` request.
    /// Streaming `session/update` notifications arrive asynchronously on the
    /// broadcast channel; wait for `TurnComplete` or `SessionEnd` to know
    /// when the agent has finished.
    ///
    /// # Errors
    ///
    /// - [`AcpError::NoSession`] — handshake not complete (should not happen via public API)
    /// - [`AcpError::Timeout`] — no ack within `prompt_timeout`
    pub async fn prompt(&self, text: &str) -> Result<(), AcpError> {
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };

        let params = SessionPromptParams {
            session_id,
            prompt: vec![ContentBlock::Text { text: text.to_owned() }],
        };

        let result = self
            .rpc_call("session/prompt", Some(json!(params)), self.prompt_timeout)
            .await?;

        // The session/prompt response arrives only after the full turn completes.
        // Emit TurnComplete so broadcast subscribers see the turn boundary even
        // if the agent didn't send a separate `stop` session/update notification.
        let stop_reason = result
            .get("stopReason")
            .and_then(|v| v.as_str())
            .unwrap_or("end_turn")
            .to_owned();
        let (input_tokens, output_tokens) = extract_token_usage(&result);
        let _ = self.tx.send(AgentEvent::TurnComplete { input_tokens, output_tokens });
        let _ = self.tx.send(AgentEvent::SessionEnd {
            result: stop_reason,
            cost_usd: None,
            is_error: false,
        });

        Ok(())
    }

    /// Send `session/cancel` notification (no response expected).
    pub async fn cancel(&self) -> Result<(), AcpError> {
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };

        let params = SessionCancelParams { session_id };
        self.notify("session/cancel", Some(json!(params))).await
    }

    /// Subscribe to all future `AgentEvent` values from this session.
    ///
    /// Events emitted before this call are not replayed.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Local gate4agent session ID (not the ACP `sessionId`).
    pub fn session_id(&self) -> &str {
        &self.local_session_id
    }

    /// CLI tool type.
    pub fn tool(&self) -> CliTool {
        self.tool
    }

    /// ACP `sessionId` returned during the handshake.
    ///
    /// Returns `None` if called before the handshake has completed (only
    /// possible if stored before `spawn()` returns, which is not possible
    /// with the current API).
    pub async fn acp_session_id(&self) -> Option<String> {
        self.acp_session_id.lock().await.clone()
    }

    /// Kill the subprocess immediately.
    pub async fn kill(&self) -> Result<(), AgentError> {
        self.reader_task.abort();
        let process = Arc::clone(&self.process);
        tokio::task::spawn_blocking(move || {
            let mut guard = process
                .lock()
                .map_err(|_| AgentError::Pty("acp process mutex poisoned".into()))?;
            guard.kill().map_err(|e| AgentError::Spawn { source: e })
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }

    /// Whether this agent supports session resumption via `session/load`.
    pub fn supports_load_session(&self) -> bool {
        self.agent_caps.agent_capabilities.load_session
    }

    /// Resume a prior ACP session by replaying its history.
    ///
    /// Sends `session/load` with `prior_session_id`. On success, updates the
    /// stored `acp_session_id`.
    ///
    /// # Errors
    ///
    /// - [`AcpError::HandshakeFailed`] — agent does not advertise `loadSession` capability
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error
    pub async fn load_session(&self, prior_session_id: &str) -> Result<(), AcpError> {
        if !self.supports_load_session() {
            return Err(AcpError::HandshakeFailed {
                message: "agent does not support loadSession".to_string(),
            });
        }

        let params = SessionLoadParams { session_id: prior_session_id.to_owned() };

        let result: SessionLoadResult = self
            .rpc_call_typed("session/load", json!(params), self.prompt_timeout, false)
            .await?;

        let new_sid = if result.session_id.is_empty() {
            prior_session_id.to_owned()
        } else {
            result.session_id
        };

        {
            let mut guard = self.acp_session_id.lock().await;
            *guard = Some(new_sid);
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Send a JSON-RPC request and await the response, deserializing the result.
    async fn rpc_call_typed<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        _id_zero: bool,
    ) -> Result<T, AcpError> {
        let raw = self.rpc_call(method, Some(params), timeout).await?;
        serde_json::from_value(raw).map_err(|e| AcpError::Json { source: e })
    }

    /// Send a JSON-RPC request and await the raw response `Value`.
    async fn rpc_call(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, AcpError> {
        let id = self.id_gen.next();
        let rx = self.pending.register(id.clone());

        let request = RpcRequest::new(id.clone(), method, params);
        let line = serde_json::to_string(&request).map_err(|e| AcpError::Json { source: e })?;
        self.write_line(line).await?;

        tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| AcpError::Timeout {
                method: method.to_owned(),
            })?
            .map_err(|_| AcpError::SessionClosed)?
            .map_err(AcpError::Agent)
    }

    /// Send a JSON-RPC notification (no response).
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), AcpError> {
        let notif = RpcNotification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        };
        let line = serde_json::to_string(&notif).map_err(|e| AcpError::Json { source: e })?;
        self.write_line(line).await
    }

    /// Write a serialized line to stdin via `spawn_blocking`.
    async fn write_line(&self, line: String) -> Result<(), AcpError> {
        let process = Arc::clone(&self.process);
        tokio::task::spawn_blocking(move || {
            let mut guard = process.lock().map_err(|_| AcpError::Write {
                source: std::io::Error::new(std::io::ErrorKind::Other, "mutex poisoned"),
            })?;
            guard
                .write_line(&line)
                .map_err(|e| AcpError::Write { source: e })
        })
        .await
        .map_err(|_| AcpError::Write {
            source: std::io::Error::new(std::io::ErrorKind::Other, "spawn_blocking panicked"),
        })?
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("acp-{:x}", t)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_error_display_messages() {
        let e = AcpError::HandshakeTimeout { step: "initialize" };
        assert!(e.to_string().contains("initialize"));

        let e = AcpError::Timeout { method: "session/prompt".into() };
        assert!(e.to_string().contains("session/prompt"));

        let e = AcpError::NoSession;
        assert!(!e.to_string().is_empty());

        let e = AcpError::SessionClosed;
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn acp_session_options_default_compiles() {
        let opts = AcpSessionOptions::default();
        assert_eq!(opts.channel_capacity, 256);
        assert_eq!(opts.handshake_timeout, Duration::from_secs(30));
        assert_eq!(opts.prompt_timeout, Duration::from_secs(120));
        assert!(opts.host_handler.is_none());
        assert!(opts.mcp_servers.is_empty(), "default mcp_servers must be empty");
    }
}
