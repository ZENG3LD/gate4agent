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

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::core::error::AgentError;
use crate::core::types::{AgentEvent, CliTool};
use crate::rpc::id::IdGen;
use crate::rpc::message::{RpcNotification, RpcRequest};
use crate::rpc::pending::PendingRequests;

use super::gate::DangerousCommandGate;
use super::host::{AcpHostAdapter, HostPolicy, PolicyHostHandler};
use super::protocol::{
    extract_token_usage, AgentCapabilities, AvailableCommand, ClientInfo, ContentBlock,
    InitializeParams, SessionCancelParams, SessionCloseParams, SessionConfigOption,
    SessionDeleteParams, SessionForkParams, SessionListParams, SessionListResult,
    SessionLoadParams, SessionLoadResult, SessionMode, SessionModel, SessionNewParams,
    SessionPromptParams, SessionSetConfigOptionParams, SessionSetModeParams, SessionState,
    SessionSummary, SessionUsage,
};
use super::reader::acp_reader_loop;
use super::spawn::AcpProcess;
use gate4agent_types::LaunchSpec;

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

    /// The ACP process exited during the handshake without ever answering,
    /// and its stderr matched the one recognized "needs authentication"
    /// signature (see `acp::reader::detect_authentication_required`) --
    /// e.g. an unauthenticated `grok agent stdio` printing "API key
    /// required" and exiting. `vendor_message` is that stderr line
    /// verbatim: it is the only text that tells the operator WHAT to do
    /// (which env var, which flag, which settings file), and callers must
    /// surface it rather than dropping it in favor of the variant name.
    #[error("Authentication required: {vendor_message}")]
    AuthenticationRequired { vendor_message: String },

    #[error("Agent returned RPC error: {0}")]
    Agent(#[from] crate::rpc::message::RpcError),

    #[error("Request timed out (method={method})")]
    Timeout { method: String },

    #[error("Session not initialized — call session_new() first")]
    NoSession,

    #[error("Session closed while awaiting response")]
    SessionClosed,

    /// The agent's `initialize` response never advertised the named
    /// `sessionCapabilities` key (see [`super::protocol::SessionCapabilities`]),
    /// so this build refuses to send the corresponding request rather
    /// than let it fail on the wire with a vendor-specific "unknown
    /// method" error.
    #[error("Agent does not advertise the '{capability}' session capability")]
    UnsupportedCapability { capability: &'static str },
}

// ---------------------------------------------------------------------------
// AcpSessionOptions
// ---------------------------------------------------------------------------

/// Options for constructing an [`AcpSession`].
pub struct AcpSessionOptions {
    /// Broadcast channel capacity. Default: 256.
    pub channel_capacity: usize,

    /// Timeout for `initialize` + `session/new` handshake. Default: 30 s.
    pub handshake_timeout: Duration,

    /// Timeout for `session/prompt` calls. Default: 120 s.
    pub prompt_timeout: Duration,

    /// The host authority mode for this session — governs both the
    /// `clientCapabilities` declared at `initialize` and how `session/
    /// request_permission` is answered. Default: [`HostPolicy::Auto`].
    /// Set this to bound a child agent's authority (e.g. a parent agent
    /// spawning a subordinate one under [`HostPolicy::ReadOnly`] or
    /// [`HostPolicy::Deny`]), not to route decisions to a human operator —
    /// every mode resolves permission requests on its own.
    pub host_policy: HostPolicy,

    /// Whether the dangerous-command gate runs ahead of `host_policy` for
    /// `terminal/create` and `execute`-kind `session/request_permission` --
    /// a decision independent of `host_policy`, including from
    /// [`HostPolicy::Yolo`]. Default: [`DangerousCommandGate::Enforced`].
    /// Set to [`DangerousCommandGate::Disabled`] only as its own explicit
    /// choice, never as a side effect of picking a permissive `host_policy`.
    pub dangerous_command_gate: DangerousCommandGate,

    /// Approval-level CLI flags to append to the spawned ACP process's own
    /// argv -- see `gate4agent_catalog::approval_level_args`, the single
    /// source of truth for the level -> flag mapping.
    ///
    /// [`AcpSession::spawn`] only applies these when the tool it spawns is
    /// the vendor's own binary (`grok agent stdio`, `kimi acp`); a tool that
    /// instead goes through an `npx` adapter-wrapper package (`claude`,
    /// `codex`) never gets them, because this crate cannot verify whether
    /// the wrapper forwards argv through to the agent it wraps -- see
    /// `src/acp/spawn.rs`'s `applicable_approval_args`.
    ///
    /// [`AcpSession::spawn_with_launch`] ignores this field entirely: its
    /// caller-supplied `LaunchSpec` may substitute a program that is not the
    /// vendor's own binary at all (a test fixture, for one), the same
    /// "don't invent a flag for an unverified program" reasoning
    /// `gate4agent_catalog::plan_launch` applies to the PTY transport.
    ///
    /// Default: empty.
    pub approval_level_args: Vec<String>,

    /// Extra directories, beyond `working_dir`, to advertise on
    /// `session/new` via `additionalDirectories` -- the outbound side of
    /// the `sessionCapabilities.additionalDirectories` flag every
    /// captured agent advertises. Sent regardless of whether the agent
    /// declared support for it (same precedent as `mcpServers`, which
    /// this file already always sends); see
    /// [`super::protocol::SessionNewParams`] for why the wire shape is
    /// unverified. Default: empty.
    pub additional_directories: Vec<String>,
}

impl Default for AcpSessionOptions {
    fn default() -> Self {
        Self {
            channel_capacity: 256,
            handshake_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(120),
            host_policy: HostPolicy::default(),
            dangerous_command_gate: DangerousCommandGate::default(),
            approval_level_args: Vec::new(),
            additional_directories: Vec::new(),
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
    /// Live session state (modes, command catalog, config options, usage)
    /// -- seeded from the `session/new`/`session/load` handshake result
    /// and kept current by the reader loop applying `session/update`
    /// notifications. A plain `std::sync::Mutex` because the reader loop
    /// that writes it runs on a blocking thread, not async; every lock is
    /// held only long enough to read or clone the state, never across an
    /// `.await`.
    session_state: Arc<Mutex<SessionState>>,
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
    /// - [`AcpError::AuthenticationRequired`] — the process exited before answering,
    ///   with stderr matching the one recognized "needs authentication" signature
    /// - [`AcpError::HandshakeFailed`] — agent returned an RPC error during handshake
    pub async fn spawn(
        tool: CliTool,
        working_dir: &std::path::Path,
        options: AcpSessionOptions,
    ) -> Result<Self, AcpError> {
        let process = AcpProcess::spawn(tool, working_dir, &[], &options.approval_level_args)
            .map_err(|source| AcpError::Spawn { source })?;
        Self::spawn_process(tool, working_dir, options, process).await
    }

    /// Spawn a catalog-declared ACP command. The command must implement the
    /// standard ACP stdio protocol; provider identity still selects the
    /// canonical adapter and capability policy.
    pub async fn spawn_with_launch(
        tool: CliTool,
        working_dir: &std::path::Path,
        options: AcpSessionOptions,
        launch: &LaunchSpec,
    ) -> Result<Self, AcpError> {
        let process = AcpProcess::spawn_with_launch(working_dir, &[], launch)
            .map_err(|source| AcpError::Spawn { source })?;
        Self::spawn_process(tool, working_dir, options, process).await
    }

    async fn spawn_process(
        tool: CliTool,
        working_dir: &std::path::Path,
        options: AcpSessionOptions,
        proc: AcpProcess,
    ) -> Result<Self, AcpError> {
        let local_session_id = generate_session_id();
        let (tx, _) = broadcast::channel::<AgentEvent>(options.channel_capacity);

        // Emit Started immediately so subscribers can see lifecycle from the start.
        let _ = tx.send(AgentEvent::Started {
            session_id: local_session_id.clone(),
        });

        let process = Arc::new(Mutex::new(proc));

        // ACP host authority is `options.host_policy` -- it governs both the
        // `clientCapabilities` declared below and how `session/request_
        // permission` gets answered, so the two can never drift apart.
        let handler: Arc<dyn crate::rpc::handler::HostHandler> = Arc::new(AcpHostAdapter(Arc::new(
            PolicyHostHandler::new(
                options.host_policy,
                working_dir.to_path_buf(),
                options.dangerous_command_gate,
            ),
        )));

        let pending = PendingRequests::new();
        let id_gen = Arc::new(IdGen::new());
        let session_state = Arc::new(Mutex::new(SessionState::default()));

        // Clones for the reader loop task.
        let reader_process = Arc::clone(&process);
        let reader_tx = tx.clone();
        let reader_pending = pending.clone();
        let reader_session_state = Arc::clone(&session_state);

        let reader_task = tokio::task::spawn_blocking(move || {
            acp_reader_loop(reader_process, reader_tx, reader_pending, handler, reader_session_state);
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
            session_state,
        };

        // --- Handshake step 1: initialize (id=0 per ACP convention) ---
        let init_params = InitializeParams {
            protocol_version: 1,
            client_capabilities: options.host_policy.client_capabilities(),
            client_info: ClientInfo {
                name: "gate4agent",
                title: Some("Gate4Agent"),
                version: env!("CARGO_PKG_VERSION"),
            },
        };
        let caps: AgentCapabilities = session
            .rpc_call_typed("initialize", json!(init_params), options.handshake_timeout, true)
            .await
            .map_err(|e| map_handshake_error("initialize", e))?;
        session.agent_caps = caps;

        // --- Handshake step 2: session/new ---
        let new_params = SessionNewParams {
            cwd: working_dir.to_str().unwrap_or(".").to_string(),
            mcp_servers: vec![],
            additional_directories: options.additional_directories.clone(),
        };
        let new_result: SessionLoadResult = session
            .rpc_call_typed("session/new", json!(new_params), options.handshake_timeout, false)
            .await
            .map_err(|e| map_handshake_error("session/new", e))?;

        let acp_sid = if new_result.session_id.is_empty() {
            local_session_id.clone()
        } else {
            new_result.session_id.clone()
        };

        {
            let mut guard = acp_session_id.lock().await;
            *guard = Some(acp_sid.clone());
        }

        {
            let mut state = session.state();
            *state = SessionState::from_handshake(&new_result);
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

        emit_prompt_result(&self.tx, &result);

        Ok(())
    }

    /// Write a prompt request and complete immediately after the request is on
    /// stdin. The ACP response is awaited in a background task so streaming
    /// provider notifications remain observable while the turn runs.
    pub async fn start_prompt(&self, text: &str) -> Result<(), AcpError> {
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };
        let params = SessionPromptParams {
            session_id,
            prompt: vec![ContentBlock::Text { text: text.to_owned() }],
        };
        let id = self.id_gen.next();
        let request = RpcRequest::new(id.clone(), "session/prompt", Some(json!(params)));
        let line = serde_json::to_string(&request).map_err(|source| AcpError::Json { source })?;
        let receiver = self.pending.register(id.clone());
        if let Err(error) = self.write_line(line).await {
            self.pending.remove(&id);
            return Err(error);
        }

        let tx = self.tx.clone();
        let timeout = self.prompt_timeout;
        tokio::spawn(async move {
            match tokio::time::timeout(timeout, receiver).await {
                Ok(Ok(Ok(result))) => emit_prompt_result(&tx, &result),
                Ok(Ok(Err(error))) => {
                    let _ = tx.send(AgentEvent::Error {
                        message: error.to_string(),
                    });
                }
                Ok(Err(_)) => {
                    let _ = tx.send(AgentEvent::Error {
                        message: "ACP session closed while awaiting prompt response".to_owned(),
                    });
                }
                Err(_) => {
                    let _ = tx.send(AgentEvent::Error {
                        message: "ACP session/prompt timed out".to_owned(),
                    });
                }
            }
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

    pub fn process_id(&self) -> Option<u32> {
        self.process.lock().ok().map(|guard| guard.process_id())
    }

    pub fn reader_finished(&self) -> bool {
        self.reader_task.is_finished()
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
    ///
    /// Reads BOTH signals a live capture has shown so far: the original
    /// top-level `loadSession` boolean, and the newer
    /// `sessionCapabilities.resume` flag -- claude-agent-acp 0.71.0,
    /// codex-acp 1.8.0, and Kimi Code CLI 0.39.1 all advertise both
    /// simultaneously (`acp-claude.jsonl`, `acp-codex.jsonl`,
    /// `acp-kimi.jsonl`). No live capture ever invokes a method distinct
    /// from `session/load` for "resume", so this build treats both flags
    /// as gating the one method it already implements rather than
    /// inventing a separate `session/resume`.
    pub fn supports_load_session(&self) -> bool {
        self.agent_caps.agent_capabilities.load_session
            || self.agent_caps.agent_capabilities.session_capabilities.resume.is_some()
    }

    /// Whether the agent advertises `sessionCapabilities.list`
    /// (`session/list`).
    pub fn supports_session_list(&self) -> bool {
        self.agent_caps.agent_capabilities.session_capabilities.list.is_some()
    }

    /// Whether the agent advertises `sessionCapabilities.close`
    /// (`session/close`).
    pub fn supports_session_close(&self) -> bool {
        self.agent_caps.agent_capabilities.session_capabilities.close.is_some()
    }

    /// Whether the agent advertises `sessionCapabilities.delete`
    /// (`session/delete`).
    pub fn supports_session_delete(&self) -> bool {
        self.agent_caps.agent_capabilities.session_capabilities.delete.is_some()
    }

    /// Whether the agent advertises `sessionCapabilities.fork`
    /// (`session/fork`).
    pub fn supports_session_fork(&self) -> bool {
        self.agent_caps.agent_capabilities.session_capabilities.fork.is_some()
    }

    /// Whether the agent advertises `sessionCapabilities.
    /// additionalDirectories` -- accepting extra directories beyond `cwd`
    /// on `session/new` (see [`AcpSessionOptions::additional_directories`]).
    pub fn supports_additional_directories(&self) -> bool {
        self.agent_caps
            .agent_capabilities
            .session_capabilities
            .additional_directories
            .is_some()
    }

    /// Whether the agent advertises `sessionCapabilities.subagents` --
    /// that it may spawn subordinate agent turns during a session.
    ///
    /// Verified live as a capability FLAG on claude-agent-acp 0.71.0 and
    /// codex-acp 1.8.0 (`acp-claude.jsonl`, `acp-codex.jsonl`); this is
    /// real, machine-readable evidence that an ACP host CAN detect
    /// up-front whether an agent may spawn subagents. What is NOT
    /// verified is any wire event or request shape for subagent
    /// lifecycle -- neither capture exercises a full prompt turn, so no
    /// live capture has ever shown a subagent starting, streaming, or
    /// stopping over the wire. This build therefore surfaces the
    /// capability flag but implements no subagent-specific event
    /// handling; a session started against an agent that returns `true`
    /// here still receives only the generic `session/update` and
    /// `RpcNotification` events this build already understands.
    pub fn supports_subagents(&self) -> bool {
        self.agent_caps.agent_capabilities.session_capabilities.subagents.is_some()
    }

    /// The agent's model catalog, as seeded from `session/new`/`session/
    /// load` and kept current by Grok's vendor `_x.ai/models/update`
    /// notification.
    pub fn available_models(&self) -> Vec<SessionModel> {
        self.state().models.available_models.clone()
    }

    /// The session's currently selected model id, if the agent reports
    /// one (see [`available_models`](Self::available_models)).
    pub fn current_model_id(&self) -> Option<String> {
        self.state().models.current_model_id.clone()
    }

    /// List prior sessions via `session/list`.
    ///
    /// UNVERIFIED wire shape -- see [`super::protocol::SessionListParams`].
    ///
    /// # Errors
    ///
    /// - [`AcpError::UnsupportedCapability`] — agent does not advertise `sessionCapabilities.list`
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, AcpError> {
        if !self.supports_session_list() {
            return Err(AcpError::UnsupportedCapability { capability: "list" });
        }
        let result: SessionListResult = self
            .rpc_call_typed(
                "session/list",
                json!(SessionListParams::default()),
                self.prompt_timeout,
                false,
            )
            .await?;
        Ok(result.sessions)
    }

    /// Close the current session via `session/close`.
    ///
    /// UNVERIFIED wire shape -- see [`super::protocol::SessionCloseParams`].
    /// Does not kill the subprocess (use [`kill`](Self::kill) for that) and
    /// does not clear the locally cached `acp_session_id` -- it only tells
    /// the agent the session is done; whether the agent then rejects
    /// further calls on this id is up to the agent.
    ///
    /// # Errors
    ///
    /// - [`AcpError::UnsupportedCapability`] — agent does not advertise `sessionCapabilities.close`
    /// - [`AcpError::NoSession`] — handshake not complete
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error
    pub async fn close_session(&self) -> Result<(), AcpError> {
        if !self.supports_session_close() {
            return Err(AcpError::UnsupportedCapability { capability: "close" });
        }
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };
        let params = SessionCloseParams { session_id };
        self.rpc_call("session/close", Some(json!(params)), self.prompt_timeout).await?;
        Ok(())
    }

    /// Delete a (not necessarily current) session's persisted history via
    /// `session/delete`.
    ///
    /// UNVERIFIED wire shape -- see [`super::protocol::SessionDeleteParams`].
    ///
    /// # Errors
    ///
    /// - [`AcpError::UnsupportedCapability`] — agent does not advertise `sessionCapabilities.delete`
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error
    pub async fn delete_session(&self, session_id: &str) -> Result<(), AcpError> {
        if !self.supports_session_delete() {
            return Err(AcpError::UnsupportedCapability { capability: "delete" });
        }
        let params = SessionDeleteParams { session_id: session_id.to_owned() };
        self.rpc_call("session/delete", Some(json!(params)), self.prompt_timeout).await?;
        Ok(())
    }

    /// Fork the current session into a new, independent one via
    /// `session/fork`. Returns the new session's id; does NOT switch this
    /// [`AcpSession`] to track it -- the original session stays current.
    ///
    /// UNVERIFIED wire shape -- see [`super::protocol::SessionForkParams`].
    /// The result is parsed with the same handshake-result shape
    /// `session/new`/`session/load` use, since a fork's whole point is
    /// handing back a second ready-to-use session.
    ///
    /// # Errors
    ///
    /// - [`AcpError::UnsupportedCapability`] — agent does not advertise `sessionCapabilities.fork`
    /// - [`AcpError::NoSession`] — handshake not complete
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error
    pub async fn fork_session(&self) -> Result<String, AcpError> {
        if !self.supports_session_fork() {
            return Err(AcpError::UnsupportedCapability { capability: "fork" });
        }
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };
        let params = SessionForkParams { session_id };
        let result: SessionLoadResult = self
            .rpc_call_typed("session/fork", json!(params), self.prompt_timeout, false)
            .await?;
        Ok(result.session_id)
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
            result.session_id.clone()
        };

        {
            let mut guard = self.acp_session_id.lock().await;
            *guard = Some(new_sid);
        }

        {
            let mut state = self.state();
            *state = SessionState::from_handshake(&result);
        }

        Ok(())
    }

    /// Modes the agent advertised at handshake time, kept current by
    /// `current_mode_update`.
    pub fn available_modes(&self) -> Vec<SessionMode> {
        self.state().modes.available_modes.clone()
    }

    /// The session's currently active mode id, if the agent supports
    /// session modes.
    pub fn current_mode_id(&self) -> Option<String> {
        self.state().modes.current_mode_id.clone()
    }

    /// The agent's slash-command catalog, kept current by
    /// `available_commands_update`.
    pub fn available_commands(&self) -> Vec<AvailableCommand> {
        self.state().available_commands.clone()
    }

    /// The session's current configuration options (model, reasoning
    /// effort, ...), kept current by `config_option_update`.
    pub fn config_options(&self) -> Vec<SessionConfigOption> {
        self.state().config_options.clone()
    }

    /// Context-window consumption and cost, as last reported by a
    /// `usage_update`. `None` if the agent has not sent one.
    pub fn usage(&self) -> Option<SessionUsage> {
        self.state().usage.clone()
    }

    /// Session title, as last reported by `session_info_update`. `None`
    /// if the agent has not sent one.
    pub fn session_title(&self) -> Option<String> {
        self.state().title.clone()
    }

    /// Switch the agent's current session mode. Per the ACP spec this may
    /// be called at any time on a live session, including mid-generation.
    ///
    /// On success, updates the locally cached `current_mode_id`
    /// immediately rather than waiting for a `current_mode_update`
    /// notification -- an agent is not required to also send one after
    /// acking this call.
    ///
    /// # Errors
    ///
    /// - [`AcpError::NoSession`] — handshake not complete
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error (e.g. unknown mode id)
    pub async fn set_mode(&self, mode_id: &str) -> Result<(), AcpError> {
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };
        let params = SessionSetModeParams { session_id, mode_id: mode_id.to_owned() };
        self.rpc_call("session/set_mode", Some(json!(params)), self.prompt_timeout)
            .await?;
        self.state().modes.current_mode_id = Some(mode_id.to_owned());
        Ok(())
    }

    /// Set a session configuration option -- the mechanism that
    /// supersedes session modes for settings such as model selection and
    /// reasoning effort. Callable at any time on a live session, like
    /// [`set_mode`](Self::set_mode).
    ///
    /// On success, updates the locally cached option's `value`
    /// immediately if `option_id` matches one already known from the
    /// handshake or a prior `config_option_update`; an unknown
    /// `option_id` is still sent to the agent (it owns validation) but
    /// leaves no matching local entry to update.
    ///
    /// # Errors
    ///
    /// - [`AcpError::NoSession`] — handshake not complete
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error (e.g. unknown option id)
    pub async fn set_config_option(&self, option_id: &str, value: Value) -> Result<(), AcpError> {
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };
        let params = SessionSetConfigOptionParams {
            session_id,
            option_id: option_id.to_owned(),
            value: value.clone(),
        };
        self.rpc_call("session/set_config_option", Some(json!(params)), self.prompt_timeout)
            .await?;
        let mut state = self.state();
        if let Some(option) = state.config_options.iter_mut().find(|o| o.id == option_id) {
            option.value = value;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Lock the live [`SessionState`] -- readers just clone a field back
    /// out; writers assign through the guard's `DerefMut`. Recovers from a
    /// poisoned mutex rather than panicking: this state is a best-effort
    /// cache of agent-reported facts, not a correctness-critical
    /// invariant, so a panic on some OTHER thread while holding this lock
    /// must not cascade into every subsequent accessor call failing too.
    fn state(&self) -> MutexGuard<'_, SessionState> {
        self.session_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
        let request = RpcRequest::new(id.clone(), method, params);
        let line = serde_json::to_string(&request).map_err(|e| AcpError::Json { source: e })?;
        let rx = self.pending.register(id.clone());
        if let Err(error) = self.write_line(line).await {
            self.pending.remove(&id);
            return Err(error);
        }

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

/// Classify a handshake-step failure into the error a caller should see.
///
/// Shared by both handshake steps (`initialize`, `session/new`) so the
/// classification lives in exactly one place. A bare RPC timeout becomes
/// [`AcpError::HandshakeTimeout`] naming `step`. An RPC-level failure is, in
/// the common case, [`AcpError::HandshakeFailed`] -- but when the reader
/// loop tagged it with `RpcError::AUTHENTICATION_REQUIRED` (the process
/// exited and its stderr matched the recognized vendor signature, see
/// `acp::reader::detect_authentication_required`), it becomes
/// [`AcpError::AuthenticationRequired`] instead, carrying the vendor's own
/// stderr line out of `rpc_err.data` rather than the generic handshake
/// message.
fn map_handshake_error(step: &'static str, error: AcpError) -> AcpError {
    match error {
        AcpError::Timeout { .. } => AcpError::HandshakeTimeout { step },
        AcpError::Agent(rpc_err)
            if rpc_err.code == crate::rpc::message::RpcError::AUTHENTICATION_REQUIRED =>
        {
            let vendor_message = rpc_err
                .data
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or(&rpc_err.message)
                .to_owned();
            AcpError::AuthenticationRequired { vendor_message }
        }
        AcpError::Agent(rpc_err) => AcpError::HandshakeFailed {
            message: rpc_err.to_string(),
        },
        other => other,
    }
}

fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("acp-{:x}", t)
}

fn emit_prompt_result(tx: &broadcast::Sender<AgentEvent>, result: &Value) {
    let stop_reason = result
        .get("stopReason")
        .and_then(|value| value.as_str())
        .unwrap_or("end_turn")
        .to_owned();
    let (input_tokens, output_tokens) = extract_token_usage(result);
    let _ = tx.send(AgentEvent::TurnComplete {
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        context_window: None,
        is_cumulative: false,
    });
    let _ = tx.send(AgentEvent::SessionEnd {
        result: stop_reason,
        cost_usd: None,
        is_error: false,
    });
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

        let e = AcpError::AuthenticationRequired {
            vendor_message: "API key required".to_owned(),
        };
        assert!(e.to_string().contains("API key required"));
    }

    #[test]
    fn map_handshake_error_recognizes_the_authentication_signature() {
        use crate::rpc::message::RpcError;

        let rpc_err = RpcError {
            code: RpcError::AUTHENTICATION_REQUIRED,
            message: "acp process exited (code=1); stderr: \u{274c} Error: API key required."
                .to_owned(),
            data: Some(Value::String("\u{274c} Error: API key required.".to_owned())),
        };
        match map_handshake_error("initialize", AcpError::Agent(rpc_err)) {
            AcpError::AuthenticationRequired { vendor_message } => {
                assert_eq!(vendor_message, "\u{274c} Error: API key required.");
            }
            other => panic!("expected AuthenticationRequired, got {other:?}"),
        }
    }

    #[test]
    fn map_handshake_error_keeps_other_rpc_failures_generic() {
        use crate::rpc::message::RpcError;

        let rpc_err = RpcError::internal("acp process exited (code=1)");
        match map_handshake_error("session/new", AcpError::Agent(rpc_err)) {
            AcpError::HandshakeFailed { message } => {
                assert!(message.contains("acp process exited"));
            }
            other => panic!("expected HandshakeFailed, got {other:?}"),
        }
    }

    #[test]
    fn map_handshake_error_maps_timeout_with_the_given_step() {
        let error = AcpError::Timeout {
            method: "initialize".into(),
        };
        match map_handshake_error("initialize", error) {
            AcpError::HandshakeTimeout { step } => assert_eq!(step, "initialize"),
            other => panic!("expected HandshakeTimeout, got {other:?}"),
        }
    }

    #[test]
    fn acp_session_options_default_compiles() {
        let opts = AcpSessionOptions::default();
        assert_eq!(opts.channel_capacity, 256);
        assert_eq!(opts.handshake_timeout, Duration::from_secs(30));
        assert_eq!(opts.prompt_timeout, Duration::from_secs(120));
        assert_eq!(opts.host_policy, HostPolicy::Auto);
        assert!(opts.approval_level_args.is_empty());
        assert!(opts.additional_directories.is_empty());
    }

    #[test]
    fn supports_load_session_reads_either_the_legacy_bool_or_the_new_capability_flag() {
        use super::super::protocol::{AgentCapabilities, AgentCapabilityFlags, SessionCapabilities};

        let mut caps = AgentCapabilities::default();
        assert!(!fake_session_supports_load(&caps));

        caps.agent_capabilities = AgentCapabilityFlags { load_session: true, ..Default::default() };
        assert!(fake_session_supports_load(&caps));

        caps.agent_capabilities = AgentCapabilityFlags {
            load_session: false,
            session_capabilities: SessionCapabilities {
                resume: Some(serde_json::json!({})),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(fake_session_supports_load(&caps));

        // Helper mirrors `AcpSession::supports_load_session`'s logic
        // without constructing a whole live session for a pure capability
        // check.
        fn fake_session_supports_load(caps: &AgentCapabilities) -> bool {
            caps.agent_capabilities.load_session
                || caps.agent_capabilities.session_capabilities.resume.is_some()
        }
    }
}
