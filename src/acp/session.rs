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
//! 5. `session.stop(force)` — `force = false` closes stdin and waits for the
//!    adapter to exit on its own (see `AcpSession::stop`); `force = true` is the
//!    same hard kill as `session.kill()`
//! 6. `session.kill()` — hard-kills the subprocess immediately, no grace period

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

use crate::core::error::AgentError;
use crate::core::types::{
    AgentEvent, CliTool, HostDecisionAuthority, HostRequestDecision, HostRequestOutcome, StopReason,
};
use crate::rpc::id::IdGen;
use crate::rpc::message::{RpcId, RpcNotification, RpcRequest, RpcResponse};
use crate::rpc::pending::{PendingRequests, RpcResult};

use super::gate::DangerousCommandGate;
use super::host::{AcpHostAdapter, HostPolicy, PermissionDeferral, PolicyHostHandler};
use super::protocol::{
    extract_token_usage, AgentCapabilities, AvailableCommand, ClientInfo, ContentBlock,
    InitializeParams, McpServerConfig, PermissionOption, PermissionOptionKind, PermissionOutcome,
    PermissionRequestParams, SessionCancelParams, SessionCloseParams, SessionCloseResult,
    SessionConfigOption, SessionDeleteParams, SessionForkParams, SessionListParams,
    SessionListResult, SessionLoadParams, SessionLoadResult, SessionMode, SessionModel,
    SessionNewParams, SessionPromptParams, SessionPromptResult, SessionSetConfigOptionParams,
    SessionSetModeParams, SessionState, SessionSummary, SessionUsage,
};
use super::reader::{acp_reader_loop, write_line_to_process};
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
// AcpSession::stop -- bound and outcome
// ---------------------------------------------------------------------------

/// How long `stop(force = false)` waits for the ACP-spawned process to exit
/// on its own, after its stdin is closed, before falling back to a kill.
///
/// Measured live 2026-09-05 by spawning each pinned ACP adapter
/// (`acp_command`/`AcpSpawnSpec`, `acp/spawn.rs`), completing the
/// `initialize` handshake, closing stdin, and timing the exit:
/// `claude-agent-acp@0.74.0` exited 0.05s after stdin close,
/// `codex-acp@1.10.0` (over its usage quota, but `initialize` still ran)
/// 4.81s, `grok agent stdio` 3.22s -- all exit code 0. Mirrors
/// `crate::pipe::PIPE_GRACEFUL_STOP_BOUND_SECS`'s value and reasoning (the
/// same better-than-2x margin over the slowest process measured); kept as
/// its own constant rather than a shared one because the two bounds cover
/// different transports and are free to diverge if either measurement
/// ever does.
pub const ACP_GRACEFUL_STOP_BOUND_SECS: u64 = 10;

/// Outcome of [`AcpSession::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpStopOutcome {
    /// The process's real exit code -- known only when it exited on its own
    /// within the graceful bound. `None` when the stop performed (or fell
    /// back to) a hard kill.
    pub exit_code: Option<i32>,
    /// `true` when the process was killed rather than left to exit on its
    /// own -- either because the caller asked for `force = true`, or the
    /// graceful bound elapsed while the process was still running.
    pub forced: bool,
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

    /// How long [`AcpSession::start_prompt`]'s watchdog will wait through
    /// COMPLETE silence from the agent before deciding the turn itself --
    /// not merely the process -- is dead. Unlike `prompt_timeout` above
    /// (a fixed total-duration bound, still used everywhere else in this
    /// type), this window is reset every time the reader loop observes ANY
    /// line from the agent belonging to the live conversation -- a
    /// streaming `session/update`, a tool-call permission request, or the
    /// `session/prompt` response itself -- so a normal long-running coding
    /// turn that keeps streaming tool calls is never interrupted purely for
    /// running long. It only fires on genuine silence.
    ///
    /// Measured live 2026-09-09: a fixed 120 s `prompt_timeout` on this
    /// watchdog fired on a healthy claude-agent-acp turn whose tools
    /// (`Search`, `Task`, `Shell`) were still running and completing well
    /// past that mark, and because the old watchdog only emitted a LOCAL
    /// `TurnInterrupted` without telling the agent to stop, the real turn
    /// kept running and the session was refused as `TurnInFlight` for the
    /// rest of its life once its next tool event flipped node-side activity
    /// back to `Working`. This field exists to fix both halves of that: it
    /// measures silence rather than total duration, and firing it now also
    /// sends the agent a real `session/cancel` before synthesizing the
    /// event -- see [`AcpSession::start_prompt`]'s watchdog for the
    /// mechanism.
    ///
    /// Default: 10 minutes -- generous for a coding agent that may run a
    /// long tool (a build, a test suite, a multi-file edit) without
    /// producing an intermediate `session/update` in between, while still
    /// catching a genuinely wedged agent well inside an operator's
    /// patience.
    pub prompt_idle_timeout: Duration,

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

    /// MCP servers to advertise on `session/new` via `mcpServers` -- the
    /// spec-sanctioned door a spawned agent uses to reach tools the host
    /// exposes, most notably a caller-prepared MCP-server launch overlay
    /// (a stdio helper invoked with whatever args and environment the
    /// caller assembled) when one was prepared for this spawn. This crate
    /// only forwards whatever the caller already assembled; it never
    /// constructs an entry itself (that is `gate4agent-shell-native`'s job,
    /// reusing the exact program/args/environment the PTY transport's
    /// environment overlay already carries). Default: empty, which sends
    /// `"mcpServers":[]` exactly as before this field existed.
    pub mcp_servers: Vec<McpServerConfig>,

    /// Whether `session/request_permission` may be **deferred** to a later,
    /// out-of-band answer -- an operator resolving it by id via
    /// [`AcpSession::resolve_pending_request`] -- instead of being decided
    /// by `host_policy` the instant the agent asks. See
    /// [`super::host::PermissionDeferral`] for the underlying mechanism and
    /// §4a of
    /// `docs/gate4agent/plans/gate4agent-acp-control-plane-on-the-wire-2026-09-02.md`
    /// for why the reader loop cannot simply await an operator inline (it is
    /// a synchronous, single-threaded parse loop; blocking it would stall
    /// every other `session/update` the agent sends while the question sits
    /// unanswered).
    ///
    /// Default: `false`. Every existing caller that never sets this field
    /// keeps EXACTLY today's behavior: `host_policy` decides every
    /// `session/request_permission` call the instant it arrives, and
    /// [`AcpSession::resolve_pending_request`] /
    /// [`AcpSession::expire_deadlines`] simply never find anything pending
    /// to act on. Intended to be set per session, at spawn, from the
    /// approval level already resolved by the caller -- a session launched
    /// to run unattended should leave this `false` and keep deciding by
    /// policy alone, never defer.
    pub defer_permission_requests: bool,

    /// How long a deferred `session/request_permission` request may wait
    /// for [`AcpSession::resolve_pending_request`] before
    /// [`AcpSession::expire_deadlines`] decides it unattended -- via
    /// `host_policy`, exactly as it would have been decided immediately had
    /// deferral never been enabled. Only consulted when
    /// `defer_permission_requests` is `true`.
    ///
    /// Default: 5 minutes -- long enough for a human to notice a prompt and
    /// answer it, short enough that a session with nobody watching never
    /// stalls indefinitely on an absent one. `HostPolicy` exists precisely
    /// so an unattended session never has to wait on a human at all; a
    /// deferred request that times out is answered exactly the way it would
    /// have been answered on arrival, just later.
    pub permission_request_deadline: Duration,
}

impl Default for AcpSessionOptions {
    fn default() -> Self {
        Self {
            channel_capacity: 256,
            handshake_timeout: Duration::from_secs(30),
            prompt_timeout: Duration::from_secs(120),
            prompt_idle_timeout: Duration::from_secs(600),
            host_policy: HostPolicy::default(),
            dangerous_command_gate: DangerousCommandGate::default(),
            approval_level_args: Vec::new(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
            defer_permission_requests: false,
            permission_request_deadline: Duration::from_secs(300),
        }
    }
}

// ---------------------------------------------------------------------------
// Deferred host permission requests
// ---------------------------------------------------------------------------

/// One `session/request_permission` request the reader loop deferred
/// instead of deciding immediately -- see
/// [`crate::rpc::handler::HostOutcome::Deferred`], returned by
/// `AcpHostAdapter::handle_deferrable` (`acp/host.rs`) only when
/// [`AcpSessionOptions::defer_permission_requests`] is `true`.
#[derive(Debug, Clone)]
pub(crate) struct PendingPermissionRequest {
    /// The full original agent → host request: what is being asked, and
    /// the concrete options the agent will accept a decision from.
    pub(crate) params: PermissionRequestParams,
    /// Wall-clock point past which [`AcpSession::expire_deadlines`] decides
    /// this request unattended.
    pub(crate) deadline: Instant,
}

/// What an operator decided about a deferred `session/request_permission`,
/// stated as intent rather than as one of the agent's option ids.
///
/// The operator answers a question ("may it do this?"); translating that into
/// whichever of the four [`PermissionOptionKind`] values a given agent
/// happened to offer is [`AcpSession::resolve_pending_request_as`]'s job, not
/// the operator's and not the wire's. This is why the wire verb carries a
/// `ProviderInteractionResponse` — the same approve/deny/answer vocabulary a
/// PTY interaction uses — instead of an ACP-specific option id: the three
/// chains say the same thing to an operator, and only the ACP path has to
/// know what an option id is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorPermissionChoice {
    Approve,
    Reject,
}

/// Shared, thread-safe registry of one [`AcpSession`]'s deferred
/// `session/request_permission` requests, keyed by JSON-RPC request id.
///
/// A plain `std::sync::Mutex`, like [`SessionState`]: `acp_reader_loop`
/// (`acp/reader.rs`) inserts into this from its blocking thread, not an
/// async context, and every lock taken here -- on either side -- is held
/// only long enough to touch the map, never across an `.await`.
#[derive(Clone, Default)]
pub(crate) struct PendingHostRequests {
    inner: Arc<Mutex<HashMap<RpcId, PendingPermissionRequest>>>,
}

impl PendingHostRequests {
    /// Record a newly deferred request. Called only by `acp_reader_loop`
    /// the moment `HostHandler::handle_deferrable` returns `Deferred`.
    pub(crate) fn insert(&self, id: RpcId, request: PendingPermissionRequest) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(id, request);
        }
    }

    /// Remove and return one pending request by id, if it is still pending.
    /// `None` covers both "never deferred" and "already answered" --
    /// [`AcpSession::resolve_pending_request`] turns that into a named
    /// error rather than silently doing nothing.
    fn remove(&self, id: &RpcId) -> Option<PendingPermissionRequest> {
        self.inner.lock().ok().and_then(|mut guard| guard.remove(id))
    }

    /// Remove and return every request whose deadline is at or before
    /// `now`. Called by [`AcpSession::expire_deadlines`].
    fn take_expired(&self, now: Instant) -> Vec<(RpcId, PendingPermissionRequest)> {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let expired_ids: Vec<RpcId> = guard
            .iter()
            .filter(|(_, request)| request.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        expired_ids
            .into_iter()
            .filter_map(|id| {
                let request = guard.remove(&id)?;
                Some((id, request))
            })
            .collect()
    }
}

/// Distinguishes, for [`AcpSession::write_pending_response`], whether a
/// deferred request's response came from an operator
/// ([`AcpSession::resolve_pending_request`]) or from an unattended deadline
/// expiry ([`AcpSession::expire_deadlines`]) -- carried straight into the
/// broadcast `RpcIncomingRequest`'s `HostRequestDecision::{Granted,Denied}
/// { by }` as `HostDecisionAuthority::Operator` or `::DeadlinePolicy`
/// respectively, so an observer never mistakes an unattended timeout for a
/// human's choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingRequestResolution {
    Answered,
    TimedOut,
}

/// Error returned by [`AcpSession::resolve_pending_request`].
#[derive(Debug, thiserror::Error)]
pub enum PendingRequestError {
    /// `id` was never deferred (not a `session/request_permission` call, or
    /// deferral was disabled for this session), or it already left the
    /// pending map -- answered once already, by
    /// [`AcpSession::resolve_pending_request`] or
    /// [`AcpSession::expire_deadlines`]. Not a panic: a caller racing an
    /// operator's answer against a deadline expiry is an expected outcome,
    /// not a programming error.
    #[error("no pending host request with id {id:?} — never deferred, or already answered")]
    NotFound { id: RpcId },
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
    /// The `sessionId` LITERALLY carried by `session/new`'s wire response --
    /// `None` when the agent's response carried none at all (an absent or
    /// empty `sessionId`, the shape `SessionLoadResult::session_id`'s
    /// `#[serde(default)]` collapses an omitted field to). Distinct from
    /// `acp_session_id` above, which is the EFFECTIVE id this session uses
    /// for every subsequent ACP call and falls back to `local_session_id`
    /// precisely in the case this field is `None` -- that fallback is what
    /// makes `acp_session_id` alone unable to answer "did the agent actually
    /// report one", which is exactly what
    /// [`provider_reported_session_id`](Self::provider_reported_session_id)
    /// exists to answer instead.
    provider_reported_session_id: Arc<tokio::sync::Mutex<Option<String>>>,
    tool: CliTool,
    tx: broadcast::Sender<AgentEvent>,
    /// Shared write handle to the process stdin (also used by reader loop for responses).
    process: Arc<Mutex<AcpProcess>>,
    pending: PendingRequests,
    id_gen: Arc<IdGen>,
    reader_task: JoinHandle<()>,
    prompt_timeout: Duration,
    /// See [`AcpSessionOptions::prompt_idle_timeout`] -- consulted only by
    /// [`start_prompt`](Self::start_prompt)'s idle watchdog, never by
    /// `prompt_timeout` above's total-duration call sites.
    prompt_idle_timeout: Duration,
    /// Signalled once by the reader loop (`acp_reader_loop`, `acp/reader.rs`)
    /// for every line it reads from the agent while this session is alive --
    /// a `session/update` notification, a host → agent request such as
    /// `session/request_permission`, or the eventual `session/prompt`
    /// response itself. [`start_prompt`](Self::start_prompt)'s idle watchdog
    /// is the sole reader: each signal resets its idle window, so it measures
    /// silence from the agent rather than the total wall-clock length of a
    /// turn. A `tokio::sync::Notify` rather than a shared `Instant` because
    /// the reader loop runs on a blocking OS thread and firing a `Notify` is
    /// a single non-blocking call from there, with no lock to hold and no
    /// clock-skew arithmetic for the watchdog side to redo; `notify_one`'s
    /// single-stored-permit semantics also mean a burst of lines between two
    /// watchdog polls collapses to exactly the "still alive" fact the
    /// watchdog needs, never a queue it would have to drain.
    turn_activity: Arc<Notify>,
    /// The host authority mode this session was constructed with -- see
    /// [`AcpSessionOptions::host_policy`]. Kept as its own field (rather
    /// than read back out of the handler) so
    /// [`expire_deadlines`](Self::expire_deadlines) can answer a timed-out
    /// deferred request with EXACTLY the policy that would have answered it
    /// immediately; nothing here can overrule `host_policy`, only ask it
    /// again, later.
    host_policy: HostPolicy,
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
    /// Deferred `session/request_permission` requests awaiting an
    /// operator's answer or a deadline expiry -- see
    /// [`resolve_pending_request`](Self::resolve_pending_request) and
    /// [`expire_deadlines`](Self::expire_deadlines). Empty for the whole
    /// life of a session with [`AcpSessionOptions::defer_permission_requests`]
    /// left at its default `false`.
    pending_host_requests: PendingHostRequests,
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

        // Deferral is a THIRD axis, independent of `host_policy` and the
        // dangerous-command gate -- see `super::host::PermissionDeferral`.
        let deferral = if options.defer_permission_requests {
            PermissionDeferral::Enabled
        } else {
            PermissionDeferral::Disabled
        };

        // ACP host authority is `options.host_policy` -- it governs both the
        // `clientCapabilities` declared below and how `session/request_
        // permission` gets answered, so the two can never drift apart.
        //
        // Concretely `Arc<AcpHostAdapter>`, not the generic `Arc<dyn
        // HostHandler>` -- the reader loop calls `handle_deferrable` on it
        // through the `HostHandler` trait exactly as before, but ALSO calls
        // the inherent `AcpHostAdapter::decision_authority` to classify
        // `HostRequestDecision`'s `by`, which is not part of that generic
        // trait (see `acp::host::AcpHostAdapter::decision_authority`'s doc
        // comment for why it stays inherent rather than joining the trait).
        let handler: Arc<AcpHostAdapter> = Arc::new(AcpHostAdapter::new(
            Arc::new(PolicyHostHandler::new(
                options.host_policy,
                working_dir.to_path_buf(),
                options.dangerous_command_gate,
            )),
            deferral,
        ));

        let pending = PendingRequests::new();
        let id_gen = Arc::new(IdGen::new());
        let session_state = Arc::new(Mutex::new(SessionState::default()));
        let pending_host_requests = PendingHostRequests::default();
        // See `AcpSession::turn_activity`'s own doc comment -- the reader
        // loop signals this, `start_prompt`'s idle watchdog waits on it.
        let turn_activity = Arc::new(Notify::new());

        // Clones for the reader loop task.
        let reader_process = Arc::clone(&process);
        let reader_tx = tx.clone();
        let reader_pending = pending.clone();
        let reader_session_state = Arc::clone(&session_state);
        let reader_pending_host_requests = pending_host_requests.clone();
        let reader_turn_activity = Arc::clone(&turn_activity);
        let permission_request_deadline = options.permission_request_deadline;

        let reader_task = tokio::task::spawn_blocking(move || {
            acp_reader_loop(
                reader_process,
                reader_tx,
                reader_pending,
                handler,
                reader_session_state,
                reader_pending_host_requests,
                permission_request_deadline,
                reader_turn_activity,
            );
        });

        let acp_session_id = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let provider_reported_session_id = Arc::new(tokio::sync::Mutex::new(None::<String>));

        let mut session = Self {
            local_session_id: local_session_id.clone(),
            acp_session_id: Arc::clone(&acp_session_id),
            provider_reported_session_id: Arc::clone(&provider_reported_session_id),
            tool,
            tx: tx.clone(),
            process,
            pending,
            id_gen,
            reader_task,
            prompt_timeout: options.prompt_timeout,
            prompt_idle_timeout: options.prompt_idle_timeout,
            turn_activity,
            host_policy: options.host_policy,
            agent_caps: AgentCapabilities::default(),
            session_state,
            pending_host_requests,
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
            mcp_servers: options.mcp_servers.clone(),
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
            let mut guard = provider_reported_session_id.lock().await;
            *guard = (!new_result.session_id.is_empty()).then(|| new_result.session_id.clone());
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
    ///
    /// That background task is an IDLE watchdog, not a total-duration one --
    /// see [`AcpSessionOptions::prompt_idle_timeout`] and
    /// [`run_prompt_watchdog`] for the full mechanism and the live incident
    /// that shaped it.
    pub async fn start_prompt(&self, text: &str) -> Result<(), AcpError> {
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };
        let params = SessionPromptParams {
            session_id: session_id.clone(),
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

        tokio::spawn(run_prompt_watchdog(
            self.tx.clone(),
            self.pending.clone(),
            id,
            receiver,
            Arc::clone(&self.turn_activity),
            self.prompt_idle_timeout,
            Arc::clone(&self.process),
            session_id,
        ));
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

    /// The `sessionId` LITERALLY reported by `session/new`'s wire response,
    /// or `None` when the agent's response carried none at all.
    ///
    /// ACP's `session/new` MUST return a `sessionId` per the protocol
    /// specification, and both shipped adapters map that id onto the
    /// provider's own durable session identity (claude-agent-acp's is the
    /// Claude Code session id and on-disk transcript filename; codex-acp's is
    /// the Codex thread id) -- `None` here names an agent that violated that
    /// MUST, distinctly from [`acp_session_id`](Self::acp_session_id), which
    /// is the id this session actually uses for every subsequent ACP call
    /// and is never `None` once the handshake completes: it silently falls
    /// back to the host-local session id in exactly the case this method
    /// reports `None` for. A caller that needs to know whether a REAL
    /// provider-issued identity exists (e.g. to decide whether to publish a
    /// `ProviderEvent::SessionIdentityObserved`) must read this method, not
    /// `acp_session_id`, which cannot make that distinction on its own.
    ///
    /// Returns `None` if called before the handshake has completed, the same
    /// constraint documented on `acp_session_id`.
    pub async fn provider_reported_session_id(&self) -> Option<String> {
        self.provider_reported_session_id.lock().await.clone()
    }

    /// Kill the subprocess immediately -- no grace period, no chance for the
    /// adapter to exit on its own. This is the `force = true` path
    /// [`AcpSession::stop`] calls straight through to; `stop(false)` is the
    /// graceful alternative that closes stdin and waits before falling back
    /// to this.
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

    /// Stop the subprocess.
    ///
    /// `force = true` kills immediately -- identical to [`AcpSession::kill`].
    /// `force = false` closes the write half of the process's stdin (the
    /// adapter observes EOF on its own input and can choose to exit cleanly)
    /// and waits up to [`ACP_GRACEFUL_STOP_BOUND_SECS`] for it to exit on its
    /// own, reporting the real exit code when it does. The reader loop keeps
    /// draining stdout/stderr the whole time -- it is only ever stopped by
    /// `kill()`'s `reader_task.abort()` -- so nothing the process still
    /// writes while shutting down is lost. Only falls back to a kill if the
    /// bound elapses with the process still running.
    ///
    /// If the process has already exited on its own by the time this is
    /// called, `reader_finished()` is already `true` and this returns
    /// immediately with `forced: false`.
    pub async fn stop(&self, force: bool) -> Result<AcpStopOutcome, AgentError> {
        if !force {
            {
                let process = Arc::clone(&self.process);
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(mut guard) = process.lock() {
                        guard.close_stdin();
                    }
                })
                .await;
            }
            let deadline = Instant::now() + Duration::from_secs(ACP_GRACEFUL_STOP_BOUND_SECS);
            loop {
                if self.reader_finished() {
                    return Ok(AcpStopOutcome {
                        exit_code: Some(acp_exit_code(&self.process)),
                        forced: false,
                    });
                }
                if Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        self.kill().await?;
        Ok(AcpStopOutcome {
            exit_code: None,
            forced: true,
        })
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

    /// List prior sessions via `session/list`, optionally filtered to
    /// those under `cwd`.
    ///
    /// Verified live by direct invocation on claude-agent-acp 0.71.0 and
    /// Grok CLI 1.0.13 -- see [`super::protocol::SessionListParams`] and
    /// [`super::protocol::SessionSummary`] for exactly what was observed
    /// on each provider. `cwd: None` sends `{}`, the exact shape both
    /// providers were called with; `cwd: Some(_)` sends `{"cwd": "..."}`,
    /// which Grok answered identically to the filterless call in the live
    /// invocation.
    ///
    /// # Errors
    ///
    /// - [`AcpError::UnsupportedCapability`] — agent does not advertise `sessionCapabilities.list`
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error
    pub async fn list_sessions(&self, cwd: Option<&str>) -> Result<Vec<SessionSummary>, AcpError> {
        if !self.supports_session_list() {
            return Err(AcpError::UnsupportedCapability { capability: "list" });
        }
        let params = SessionListParams { cwd: cwd.map(str::to_owned) };
        let result: SessionListResult = self
            .rpc_call_typed("session/list", json!(params), self.prompt_timeout, false)
            .await?;
        Ok(result.sessions)
    }

    /// Close the current session via `session/close`.
    ///
    /// Verified live by direct invocation on claude-agent-acp 0.71.0 and
    /// Grok CLI 1.0.13 -- see [`super::protocol::SessionCloseParams`] and
    /// [`super::protocol::SessionCloseResult`]. Does not kill the
    /// subprocess (use [`kill`](Self::kill) for that) and does not clear
    /// the locally cached `acp_session_id` -- it only tells the agent the
    /// session is done; whether the agent then rejects further calls on
    /// this id is up to the agent.
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
        let _: SessionCloseResult = self
            .rpc_call_typed("session/close", json!(params), self.prompt_timeout, false)
            .await?;
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

    /// Fork the current session into a new, independent one rooted at
    /// `cwd`, via `session/fork`. Returns the new session's id; does NOT
    /// switch this [`AcpSession`] to track it -- the original session
    /// stays current.
    ///
    /// `cwd` is REQUIRED, verified live by direct invocation on
    /// claude-agent-acp 0.71.0: calling with only `{"sessionId": "<id>"}`
    /// (this file's earlier guess) was rejected with a `-32602 Invalid
    /// params` error naming `cwd` as the missing field -- see
    /// [`super::protocol::SessionForkParams`] for the exact error payload.
    /// Forking creates a new session rooted at a working directory; it
    /// does not clone the original purely by id. The SUCCESS response
    /// shape remains UNVERIFIED (the only live call made errored before
    /// returning one) and is still parsed with the same handshake-result
    /// shape `session/new`/`session/load` use.
    ///
    /// # Errors
    ///
    /// - [`AcpError::UnsupportedCapability`] — agent does not advertise `sessionCapabilities.fork`
    /// - [`AcpError::NoSession`] — handshake not complete
    /// - [`AcpError::Timeout`] — no response within `prompt_timeout`
    /// - [`AcpError::Agent`] — agent returned an RPC error (e.g. a missing/invalid `cwd`)
    pub async fn fork_session(&self, cwd: &str) -> Result<String, AcpError> {
        if !self.supports_session_fork() {
            return Err(AcpError::UnsupportedCapability { capability: "fork" });
        }
        let session_id = {
            let guard = self.acp_session_id.lock().await;
            guard.clone().ok_or(AcpError::NoSession)?
        };
        let params = SessionForkParams { session_id, cwd: cwd.to_owned() };
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
    /// acking this call (codex-acp, live-measured, never does: it acks
    /// `session/set_mode` and stays silent). The ack itself is the protocol
    /// fact that the switch happened, so this also broadcasts the SAME
    /// [`AgentEvent::ModeChanged`] a `current_mode_update` notification
    /// would have produced, carrying the catalogue an operator needs to see
    /// the new current mode rather than leaving the last-announced one
    /// looking current. A `current_mode_update` that does arrive later
    /// simply broadcasts its own `ModeChanged` afterward, superseding this
    /// one the same way any other repeated state update does -- same
    /// field, newer value, no special case for who sent it.
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
        let _ = self.tx.send(AgentEvent::ModeChanged { mode_id: mode_id.to_owned() });
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

    /// Answer one deferred `session/request_permission` request by its
    /// JSON-RPC `id`, on an operator's behalf, with either a chosen
    /// [`PermissionOption`] (`Some`) or an explicit cancellation (`None`,
    /// mirroring the agent's own [`PermissionOutcome::Cancelled`]). Writes
    /// the response straight to the agent's stdin via the same
    /// `write_line_to_process` path `acp_reader_loop` itself uses for every
    /// other response (`acp/reader.rs`) -- safe to call from any task,
    /// because the reader loop is documented never to hold the process
    /// mutex across the handler call that produced this pending request in
    /// the first place.
    ///
    /// Removes the entry from the pending map on success, so a second call
    /// with the same `id` -- a slow operator UI double-submitting, or a
    /// race against [`expire_deadlines`](Self::expire_deadlines) -- gets
    /// [`PendingRequestError::NotFound`] rather than answering the agent
    /// twice.
    ///
    /// This is a way to ASK an operator, never a way to overrule
    /// `host_policy` or the dangerous-command gate: a request only ever
    /// reaches the pending map after both have already had their say (see
    /// `AcpHostAdapter::handle_deferrable`, `acp/host.rs`) and left it
    /// undecided on purpose.
    ///
    /// # Errors
    ///
    /// - [`PendingRequestError::NotFound`] — `id` was never deferred (this
    ///   session never called `handle_deferrable` with it, or deferral was
    ///   disabled), or it was already answered. Never panics on an unknown
    ///   or stale id.
    /// Answer a deferred request the way an OPERATOR asked for, rather than
    /// with an option the caller had to construct.
    ///
    /// The operator says approve or reject. Which of the agent's offered
    /// options that becomes is not the caller's to guess: an agent offers
    /// whatever subset of the four [`PermissionOptionKind`] values it likes,
    /// and an `option_id` it never offered is ignored rather than refused —
    /// so a fabricated one grants nothing while reporting success. This reads
    /// the options off the pending request's own stored params and walks the
    /// same preference order [`HostPolicy::select_permission_option`] walks,
    /// through the one shared selector, so an approval means the same thing
    /// whether policy or a human produced it.
    ///
    /// `Approve` prefers a once-only grant over a standing one: an operator
    /// answering one question has consented to one thing, and silently
    /// upgrading that to `AllowAlways` because the agent happened not to
    /// offer `AllowOnce` would widen a decision they did not make. `Reject`
    /// prefers the narrower refusal for the mirror-image reason. If the agent
    /// offered nothing in the requested direction the outcome is `Cancelled`,
    /// which every ACP agent accepts as a complete answer.
    pub fn resolve_pending_request_as(
        &self,
        id: &RpcId,
        choice: OperatorPermissionChoice,
    ) -> Result<(), PendingRequestError> {
        let request = self
            .pending_host_requests
            .remove(id)
            .ok_or_else(|| PendingRequestError::NotFound { id: id.clone() })?;
        let preference: &[PermissionOptionKind] = match choice {
            OperatorPermissionChoice::Approve => {
                &[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]
            }
            OperatorPermissionChoice::Reject => {
                &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
            }
        };
        let outcome = super::host::select_offered_option(&request.params.options, preference);
        self.write_pending_response(id, &request.params, outcome, PendingRequestResolution::Answered);
        Ok(())
    }

    pub fn resolve_pending_request(
        &self,
        id: &RpcId,
        option: Option<PermissionOption>,
    ) -> Result<(), PendingRequestError> {
        let request = self
            .pending_host_requests
            .remove(id)
            .ok_or_else(|| PendingRequestError::NotFound { id: id.clone() })?;
        let outcome = match option {
            Some(option) => PermissionOutcome::Selected { option_id: option.option_id },
            None => PermissionOutcome::Cancelled,
        };
        self.write_pending_response(id, &request.params, outcome, PendingRequestResolution::Answered);
        Ok(())
    }

    /// Answer every deferred request whose deadline has passed, exactly as
    /// `host_policy` (the value this session was constructed with -- see
    /// [`AcpSessionOptions::host_policy`]) would have answered it had
    /// deferral never been enabled. The dangerous-command gate has ALREADY
    /// had its say by the time a request is recorded here (see
    /// `AcpHostAdapter::handle_deferrable`, `acp/host.rs`) -- a gate-blocked
    /// call is never deferred in the first place -- so nothing left pending
    /// is ever gate-blocked; only `host_policy` gets asked again, later,
    /// exactly as it would have been asked immediately.
    ///
    /// Cheap and safe to call repeatedly, e.g. from a periodic timer in a
    /// caller: a session with nothing pending, or nothing yet past its
    /// deadline, does nothing.
    ///
    /// The fact that a request was decided by timeout rather than by an
    /// operator rides in the SAME [`AgentEvent::RpcIncomingRequest`] audit
    /// event as the decision itself -- its `HostRequestDecision`'s `by` is
    /// `HostDecisionAuthority::DeadlinePolicy`, never `::Operator`, so an
    /// observer can never mistake an unattended timeout for a human's
    /// choice by looking at that one event alone. An unattended session
    /// must never stall on an absent human; that is the whole reason
    /// `HostPolicy` exists, and this is what keeps a deferred request from
    /// becoming a hang.
    pub fn expire_deadlines(&self) {
        let expired = self.pending_host_requests.take_expired(Instant::now());
        for (id, request) in expired {
            let outcome = self
                .host_policy
                .select_permission_option(&request.params.tool_call, &request.params.options);
            self.write_pending_response(&id, &request.params, outcome, PendingRequestResolution::TimedOut);
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Write a `session/request_permission` response for a request that was
    /// deferred, then broadcast the decision. Shared by
    /// [`resolve_pending_request`](Self::resolve_pending_request) (an
    /// operator's choice) and [`expire_deadlines`](Self::expire_deadlines)
    /// (a timeout); `resolution` is what tells the two apart on the wire
    /// out to observers -- see [`PendingRequestResolution`].
    fn write_pending_response(
        &self,
        id: &RpcId,
        params: &PermissionRequestParams,
        outcome: PermissionOutcome,
        resolution: PendingRequestResolution,
    ) {
        let value = serde_json::to_value(&outcome).unwrap_or(Value::Null);
        let response = RpcResponse::success(id.clone(), value);
        if let Ok(json) = serde_json::to_string(&response) {
            write_line_to_process(&self.process, &format!("{}\n", json));
        }

        // The same `RpcIncomingRequest` audit shape the reader loop would
        // have broadcast had this call never been deferred -- now honest,
        // because the decision now actually exists. `by` is what tells an
        // operator's answer apart from an unattended deadline expiry (see
        // `PendingRequestResolution`); neither resolution can ever be
        // `Gate` -- a gate-blocked call is never deferred in the first
        // place (see `expire_deadlines`'s doc comment).
        let by = match resolution {
            PendingRequestResolution::Answered => HostDecisionAuthority::Operator,
            PendingRequestResolution::TimedOut => HostDecisionAuthority::DeadlinePolicy,
        };
        let granted = permission_outcome_grants(&outcome, &params.options);
        let decision = if granted {
            HostRequestDecision::Granted { by }
        } else {
            HostRequestDecision::Denied { by }
        };
        let _ = self.tx.send(AgentEvent::RpcIncomingRequest {
            id: id.clone(),
            method: "session/request_permission".to_owned(),
            params: Some(serde_json::to_value(params).unwrap_or(Value::Null)),
            decision,
            // Selecting/cancelling a `session/request_permission` option is
            // not I/O -- there is no execution step here that can fail the
            // way a `terminal/create` spawn can (see
            // `crate::core::types::HostRequestOutcome`'s own doc comment).
            outcome: HostRequestOutcome::Executed,
            // Neither `OperatorPermissionChoice` nor an unattended deadline
            // expiry carries any free text -- an operator answers
            // approve/reject, never a comment, and `expire_deadlines` never
            // asks anyone anything. There is genuinely no reason to report
            // here, so this stays `None` rather than inventing one.
            reason: None,
        });
    }

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

/// Kill the subprocess if the reader loop is still running when a session is
/// dropped without an explicit [`kill`](AcpSession::kill)/[`stop`](AcpSession::stop).
///
/// `acp_reader_loop` (`acp/reader.rs`) is a `spawn_blocking` loop that polls
/// the process every 10 ms and only returns once the child stops running --
/// it holds its own `Arc` clone of `process`, so a still-alive child keeps
/// that loop, and the blocking-pool thread running it, alive too. Without
/// this, an `AcpSession` dropped without a kill leaks both the child process
/// and that thread forever. Mirrors `PtySession`'s `Drop` (`pty/session.rs`):
/// the kill itself is the same synchronous `AcpProcess::kill` call
/// [`kill`](AcpSession::kill) performs inside `spawn_blocking`, run directly
/// here because `Drop` cannot `.await`; a poisoned mutex is recovered via
/// `into_inner` rather than skipped, so a session that panicked mid-operation
/// still does not leak its child.
impl Drop for AcpSession {
    fn drop(&mut self) {
        if self.reader_task.is_finished() {
            return;
        }
        let mut guard = match self.process.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = guard.kill();
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

/// Best-effort exit code for a still-tracked ACP process. Falls back to
/// `0` if the lock is poisoned -- same convention as
/// `pipe::session::get_exit_code`.
fn acp_exit_code(process: &Arc<Mutex<AcpProcess>>) -> i32 {
    process
        .lock()
        .ok()
        .map(|mut guard| guard.exit_code())
        .unwrap_or(0)
}

/// Whether `outcome` grants the request it answers -- mirrors
/// `acp::reader::permission_outcome_is_granted`'s reasoning (a `Selected`
/// outcome only grants when the chosen option's kind is allow-shaped;
/// `Cancelled` never grants) but works directly on the typed
/// [`PermissionOutcome`]/[`PermissionOption`] values this module already
/// has in hand, rather than round-tripping through `Value` the way the
/// reader loop must (it only ever sees the raw JSON-RPC response, not the
/// typed request that produced it).
fn permission_outcome_grants(outcome: &PermissionOutcome, options: &[PermissionOption]) -> bool {
    let PermissionOutcome::Selected { option_id } = outcome else {
        return false;
    };
    options
        .iter()
        .find(|option| &option.option_id == option_id)
        .is_some_and(|option| {
            matches!(option.kind, PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways)
        })
}

/// The idle-silence watchdog spawned by [`AcpSession::start_prompt`] for one
/// `session/prompt` call.
///
/// Waits for exactly one of three things to happen first:
///
/// - `receiver` resolves -- the agent's real `session/prompt` response (or
///   the session closing mid-call). The ordinary, expected outcome; ends the
///   turn via [`emit_prompt_result`] / [`report_agent_rpc_error`] /
///   [`interrupt_turn`] exactly as before this watchdog existed.
/// - `turn_activity` fires -- the reader loop (`acp::reader::acp_reader_loop`)
///   observed SOME line from the agent since this watchdog last checked: a
///   streaming `session/update`, a tool-call permission request, or the
///   `session/prompt` response arriving concurrently with other traffic.
///   Resets the idle window and keeps waiting -- a turn that is still
///   visibly working is never cut off purely for running long.
/// - `idle_timeout` elapses with NEITHER of the above -- genuine silence,
///   not merely a long turn. Only this branch sends the agent a real
///   `session/cancel` ([`send_cancel_notification`]) and removes `id` from
///   `pending` BEFORE synthesizing [`AgentEvent::TurnInterrupted`], so a
///   response that arrives after the fact can neither resolve a receiver
///   nobody is waiting on nor be mistaken for an answer to a request this
///   session no longer considers open.
///
/// Measured live 2026-09-09 against claude-agent-acp: the watchdog this
/// replaced used a fixed 120 s total-duration bound and, on firing, only
/// emitted the local `TurnInterrupted` event below -- it never told the
/// agent to stop. The real turn (tools `Search`, `Task`, `Shell`) kept
/// running past that mark, and its next tool event flipped node-side
/// activity back to `Working`, so every later prompt was refused as
/// `TurnInFlight` for the rest of the session's life. This watchdog fixes
/// both halves: it only fires on silence, and firing it actually ends the
/// remote turn.
async fn run_prompt_watchdog(
    tx: broadcast::Sender<AgentEvent>,
    pending: PendingRequests,
    id: RpcId,
    receiver: tokio::sync::oneshot::Receiver<RpcResult>,
    turn_activity: Arc<Notify>,
    idle_timeout: Duration,
    process: Arc<Mutex<AcpProcess>>,
    session_id: String,
) {
    tokio::pin!(receiver);
    loop {
        tokio::select! {
            // Checked first: if the real response and an idle-timeout both
            // happen to be ready in the same poll (the response arrives
            // right as the window elapses), the real answer wins rather
            // than a cancel racing it.
            biased;

            result = &mut receiver => {
                match result {
                    Ok(Ok(result)) => emit_prompt_result(&tx, &result),
                    Ok(Err(error)) => report_agent_rpc_error(&tx, &error),
                    Err(_) => interrupt_turn(
                        &tx,
                        "ACP session closed while awaiting prompt response".to_owned(),
                    ),
                }
                return;
            }

            _ = turn_activity.notified() => {
                continue;
            }

            _ = tokio::time::sleep(idle_timeout) => {
                send_cancel_notification(&process, session_id);
                pending.remove(&id);
                interrupt_turn(
                    &tx,
                    format!(
                        "session/prompt idle for {idle_timeout:?} with no activity from the agent -- turn cancelled"
                    ),
                );
                return;
            }
        }
    }
}

/// Send `session/cancel` for `session_id` straight to the process stdin --
/// used by [`run_prompt_watchdog`] when its idle window elapses, which runs
/// inside a detached `tokio::spawn` task with no `&AcpSession` through which
/// to call the public [`AcpSession::cancel`] method. Builds the exact same
/// wire shape `cancel()` sends (a `session/cancel` notification carrying
/// [`SessionCancelParams`]), so the agent sees an ordinary cancel regardless
/// of which path produced it. Best-effort, like every other write in this
/// module: the process may already be gone by the time this runs, and a
/// failed write here is not itself an error worth reporting -- the turn is
/// being torn down either way.
fn send_cancel_notification(process: &Arc<Mutex<AcpProcess>>, session_id: String) {
    let notif = RpcNotification {
        jsonrpc: "2.0".into(),
        method: "session/cancel".into(),
        params: Some(json!(SessionCancelParams { session_id })),
    };
    if let Ok(json) = serde_json::to_string(&notif) {
        write_line_to_process(process, &format!("{}\n", json));
    }
}

/// Report a `session/prompt` call started by [`AcpSession::start_prompt`]
/// that ended WITHOUT a `session/prompt` success response -- an agent RPC
/// error, the session closing mid-call, or [`run_prompt_watchdog`]'s idle
/// window elapsing on complete silence (see that function's three branches).
/// The idle-window branch has already sent the agent a real `session/cancel`
/// and de-registered the pending request by the time this runs -- this
/// function only ever synthesizes the LOCAL event pair. Sends both `Error`
/// (the failure text, for anything that only wants to display it) and
/// `TurnInterrupted` (the signal that the turn itself is over) so a caller
/// that only reads `Error` sees no change in behavior, while `TurnInterrupted`
/// gives downstream turn-state tracking (`gate4agent-engine`'s snapshot
/// reducer) something to reset `ProviderActivity` away from `Blocked` with --
/// see `AgentEvent::TurnInterrupted`'s own doc comment for the stuck-forever
/// bug this closes.
fn interrupt_turn(tx: &broadcast::Sender<AgentEvent>, reason: String) {
    let _ = tx.send(AgentEvent::Error { message: reason.clone() });
    let _ = tx.send(AgentEvent::TurnInterrupted { reason });
}

/// Report a `session/prompt` call that ended with a JSON-RPC error FROM
/// THE AGENT ITSELF -- e.g. codex-acp's `-32603` "usage limit exceeded"
/// when the account's quota is exhausted (measured live 2026-09-05,
/// `error: {"code": -32603, "message": "Internal error", "data":
/// {"message": "You've hit your usage limit...", "codexErrorInfo":
/// "usageLimitExceeded"}}`). Sends the same `Error`+`TurnInterrupted` pair
/// [`interrupt_turn`] already sends (nothing that depends on that pair
/// changes), PLUS a `SessionEnd` carrying `StopReason::ProviderError` --
/// the one place downstream (`gate4agent-shell-native`'s `provider_event`,
/// `gate4agent-node`'s mint) can classify a vendor code into a typed
/// `ActionBlocked`/`ProviderQuota` fact, rather than only the human-
/// readable string `TurnInterrupted::reason` already carried.
fn report_agent_rpc_error(tx: &broadcast::Sender<AgentEvent>, error: &crate::rpc::message::RpcError) {
    let reason = error.to_string();
    let _ = tx.send(AgentEvent::Error { message: reason.clone() });
    let _ = tx.send(AgentEvent::TurnInterrupted { reason: reason.clone() });
    let (message, vendor_code) = extract_rpc_error_detail(error);
    let _ = tx.send(AgentEvent::SessionEnd {
        result: reason,
        cost_usd: None,
        is_error: true,
        stop_reason: Some(StopReason::ProviderError {
            code: error.code,
            message,
            vendor_code,
        }),
    });
}

/// Pulls the verbatim message and the vendor's own machine code out of an
/// agent RPC error's `data` -- Codex's shape verified live 2026-09-05:
/// `data: {"message": "<real human text>", "codexErrorInfo":
/// "usageLimitExceeded"}`. `error.message` itself is only the generic
/// "Internal error" on that shape, so `data.message` is preferred when
/// present; `data.codexErrorInfo` is the vendor code, or -- absent that
/// exact key -- any OTHER string-valued field found directly on `data`
/// (bounded, single-level scan; never invented beyond what the wire
/// actually named). Both are cut to `PROVIDER_EVENT_TEXT_MAX_BYTES` at a
/// safe UTF-8 boundary before this build carries them any further.
fn extract_rpc_error_detail(error: &crate::rpc::message::RpcError) -> (String, Option<String>) {
    let Some(data) = error.data.as_ref() else {
        return (error.message.clone(), None);
    };
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| data.as_str())
        .unwrap_or(&error.message);
    let vendor_code = data
        .get("codexErrorInfo")
        .and_then(Value::as_str)
        .or_else(|| {
            data.as_object().and_then(|obj| {
                obj.iter()
                    .find(|(key, value)| key.as_str() != "message" && value.is_string())
                    .and_then(|(_, value)| value.as_str())
            })
        });
    (
        crate::utils::truncate_str(message, gate4agent_types::PROVIDER_EVENT_TEXT_MAX_BYTES)
            .to_owned(),
        vendor_code
            .map(|s| crate::utils::truncate_str(s, gate4agent_types::PROVIDER_EVENT_TEXT_MAX_BYTES).to_owned()),
    )
}

/// Fold a `session/prompt` response into the two events every caller of
/// `prompt()`/`start_prompt()` waits on.
///
/// Token usage rides in different places depending on the provider --
/// see [`SessionPromptResult`]'s own doc comment. Claude's `usage` is
/// tried first, then Grok's `_meta` breakdown, then the older
/// multi-shape [`extract_token_usage`] scan as a last resort for any
/// provider/shape neither one matches; Codex and Kimi send neither and
/// fall all the way through to zeroed counts, which is the honest answer
/// -- they never report per-turn usage on this response at all.
fn emit_prompt_result(tx: &broadcast::Sender<AgentEvent>, result: &Value) {
    let parsed: SessionPromptResult = serde_json::from_value(result.clone()).unwrap_or_default();
    let stop_reason = parsed.stop_reason.unwrap_or(StopReason::EndTurn);
    let is_error = stop_reason.is_refusal();
    let stop_reason_text = stop_reason.as_wire_str().to_owned();

    let (input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens) =
        if let Some(usage) = parsed.usage {
            (
                usage.input_tokens.unwrap_or(0),
                usage.output_tokens.unwrap_or(0),
                usage.cached_read_tokens.unwrap_or(0),
                usage.cached_write_tokens.unwrap_or(0),
                0,
            )
        } else if let Some(meta) = parsed.meta {
            (
                meta.input_tokens.unwrap_or(0),
                meta.output_tokens.unwrap_or(0),
                meta.cached_read_tokens.unwrap_or(0),
                0,
                meta.reasoning_tokens.unwrap_or(0),
            )
        } else {
            let (input, output) = extract_token_usage(result);
            (input, output, 0, 0, 0)
        };

    let _ = tx.send(AgentEvent::TurnComplete {
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens,
        context_window: None,
        is_cumulative: false,
    });
    let _ = tx.send(AgentEvent::SessionEnd {
        result: stop_reason_text,
        cost_usd: None,
        is_error,
        stop_reason: Some(stop_reason),
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
        assert_eq!(opts.prompt_idle_timeout, Duration::from_secs(600));
        assert_eq!(opts.host_policy, HostPolicy::Auto);
        assert!(opts.approval_level_args.is_empty());
        assert!(opts.additional_directories.is_empty());
        assert!(opts.mcp_servers.is_empty());
        assert!(!opts.defer_permission_requests, "deferral must default to off");
        assert_eq!(opts.permission_request_deadline, Duration::from_secs(300));
    }

    // -----------------------------------------------------------------------
    // PendingHostRequests — the deferred-permission-request map
    // -----------------------------------------------------------------------

    fn fake_permission_request(session_id: &str) -> PermissionRequestParams {
        use crate::acp::protocol::PermissionToolCall;
        PermissionRequestParams {
            session_id: session_id.to_owned(),
            tool_call: PermissionToolCall::default(),
            options: vec![
                PermissionOption { option_id: "ao".to_owned(), name: "Allow once".to_owned(), kind: PermissionOptionKind::AllowOnce },
                PermissionOption { option_id: "ro".to_owned(), name: "Reject once".to_owned(), kind: PermissionOptionKind::RejectOnce },
            ],
        }
    }

    #[test]
    fn pending_host_requests_insert_remove_roundtrip() {
        let map = PendingHostRequests::default();
        let id = RpcId::Number(1);
        map.insert(
            id.clone(),
            PendingPermissionRequest {
                params: fake_permission_request("s1"),
                deadline: Instant::now() + Duration::from_secs(60),
            },
        );
        let removed = map.remove(&id).expect("was inserted");
        assert_eq!(removed.params.session_id, "s1");
        assert!(map.remove(&id).is_none(), "removing twice must not resurrect the entry");
    }

    #[test]
    fn pending_host_requests_take_expired_only_removes_past_deadline() {
        let map = PendingHostRequests::default();
        let now = Instant::now();
        let expired_id = RpcId::Number(1);
        let live_id = RpcId::Number(2);
        map.insert(
            expired_id.clone(),
            PendingPermissionRequest { params: fake_permission_request("expired"), deadline: now },
        );
        map.insert(
            live_id.clone(),
            PendingPermissionRequest {
                params: fake_permission_request("live"),
                deadline: now + Duration::from_secs(600),
            },
        );

        let expired = map.take_expired(now);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, expired_id);
        assert_eq!(expired[0].1.params.session_id, "expired");
        assert!(map.remove(&live_id).is_some(), "the non-expired entry must still be present");
    }

    #[test]
    fn permission_outcome_grants_selected_allow_kinds_only() {
        let options = vec![
            PermissionOption { option_id: "ao".to_owned(), name: "Allow once".to_owned(), kind: PermissionOptionKind::AllowOnce },
            PermissionOption { option_id: "ro".to_owned(), name: "Reject once".to_owned(), kind: PermissionOptionKind::RejectOnce },
        ];
        assert!(permission_outcome_grants(&PermissionOutcome::Selected { option_id: "ao".to_owned() }, &options));
        assert!(!permission_outcome_grants(&PermissionOutcome::Selected { option_id: "ro".to_owned() }, &options));
        assert!(!permission_outcome_grants(&PermissionOutcome::Cancelled, &options));
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

    // -----------------------------------------------------------------------
    // emit_prompt_result -- `session/prompt` response, all four providers
    // -----------------------------------------------------------------------

    #[test]
    fn emit_prompt_result_claude_verbatim_populates_cache_tokens() {
        // `turn-claude.jsonl` line 9 -- the id=2 result, verbatim.
        let raw: Value = serde_json::from_str(
            r#"{"stopReason":"end_turn","usage":{"inputTokens":2,"outputTokens":4,"cachedReadTokens":15320,"cachedWriteTokens":17081,"totalTokens":32407},"_meta":{"quota":{}}}"#,
        )
        .expect("valid json literal");
        let (tx, mut rx) = broadcast::channel(8);
        emit_prompt_result(&tx, &raw);

        match rx.try_recv().expect("TurnComplete event") {
            AgentEvent::TurnComplete {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                ..
            } => {
                assert_eq!(input_tokens, 2);
                assert_eq!(output_tokens, 4);
                assert_eq!(cache_read_tokens, 15320);
                assert_eq!(cache_write_tokens, 17081);
                assert_eq!(reasoning_tokens, 0);
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
        match rx.try_recv().expect("SessionEnd event") {
            AgentEvent::SessionEnd { result, .. } => assert_eq!(result, "end_turn"),
            other => panic!("expected SessionEnd, got {other:?}"),
        }
    }

    #[test]
    fn emit_prompt_result_grok_verbatim_reads_meta_not_usage() {
        // `turn-grok.jsonl` -- the id=2 result, verbatim: no top-level
        // `usage`, breakdown on `_meta` instead.
        let raw: Value = serde_json::from_str(
            r#"{"stopReason":"end_turn","_meta":{"sessionId":"01a05e6a-aa4d-7a13-9e9c-2077aa244389","requestId":"97238a1c-461a-4f9a-ba2f-4acc677b762b","promptId":"97238a1c-461a-4f9a-ba2f-4acc677b762b","totalTokens":19885,"modelId":"grok-4.6","inputTokens":19807,"outputTokens":78,"cachedReadTokens":1408,"reasoningTokens":73}}"#,
        )
        .expect("valid json literal");
        let (tx, mut rx) = broadcast::channel(8);
        emit_prompt_result(&tx, &raw);

        match rx.try_recv().expect("TurnComplete event") {
            AgentEvent::TurnComplete {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                ..
            } => {
                assert_eq!(input_tokens, 19807);
                assert_eq!(output_tokens, 78);
                assert_eq!(cache_read_tokens, 1408);
                assert_eq!(cache_write_tokens, 0);
                assert_eq!(reasoning_tokens, 73);
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
    }

    #[test]
    fn emit_prompt_result_kimi_and_codex_verbatim_bare_stop_reason_zeroes_usage() {
        // `turn-kimi.jsonl` / `turn-codex.jsonl` -- neither sends `usage`
        // or a token-bearing `_meta`; the honest answer is zeroed counts,
        // not a made-up number.
        let raw: Value = serde_json::from_str(r#"{"stopReason":"end_turn"}"#).expect("valid json literal");
        let (tx, mut rx) = broadcast::channel(8);
        emit_prompt_result(&tx, &raw);

        match rx.try_recv().expect("TurnComplete event") {
            AgentEvent::TurnComplete { input_tokens, output_tokens, .. } => {
                assert_eq!(input_tokens, 0);
                assert_eq!(output_tokens, 0);
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
        match rx.try_recv().expect("SessionEnd event") {
            AgentEvent::SessionEnd { result, .. } => assert_eq!(result, "end_turn"),
            other => panic!("expected SessionEnd, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // interrupt_turn -- the fix for a `session/prompt` failure (agent RPC
    // error, session closed mid-call, or the idle watchdog firing) leaving
    // the turn stuck rather than ended. Measured live against codex-acp
    // 1.10.0 on 2026-09-05: an RPC error response landed within seconds, but
    // because `start_prompt`'s failure branches used to emit only `Error`
    // (which `gate4agent-engine`'s snapshot reducer maps to
    // `ProviderActivity::Blocked`, not `Idle`), every subsequent prompt was
    // refused as "turn in flight" for the rest of the session's life.
    // -----------------------------------------------------------------------

    #[test]
    fn interrupt_turn_emits_both_error_and_turn_interrupted_with_the_same_reason() {
        let (tx, mut rx) = broadcast::channel(8);
        interrupt_turn(&tx, "session/prompt timed out after 120s".to_owned());

        match rx.try_recv().expect("Error event") {
            AgentEvent::Error { message } => {
                assert_eq!(message, "session/prompt timed out after 120s");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        match rx.try_recv().expect("TurnInterrupted event") {
            AgentEvent::TurnInterrupted { reason } => {
                assert_eq!(reason, "session/prompt timed out after 120s");
            }
            other => panic!("expected TurnInterrupted, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no third event should follow");
    }

    #[test]
    fn report_agent_rpc_error_codex_quota_transcript_verbatim() {
        // Live fixture, codex-acp 1.10.0, 2026-09-05 (account over quota):
        // `{"jsonrpc":"2.0","id":4,"error":{"code":-32603,"message":
        // "Internal error","data":{"message":"You've hit your usage
        // limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit
        // https://chatgpt.com/codex/settings/usage to purchase more
        // credits or try again at Sep 7th, 2026 6:19 PM.","codexErrorInfo":
        // "usageLimitExceeded"}}}`.
        use crate::rpc::message::RpcError;
        let error = RpcError {
            code: RpcError::INTERNAL_ERROR,
            message: "Internal error".to_owned(),
            data: Some(json!({
                "message": "You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 7th, 2026 6:19 PM.",
                "codexErrorInfo": "usageLimitExceeded",
            })),
        };
        let (tx, mut rx) = broadcast::channel(8);
        report_agent_rpc_error(&tx, &error);

        assert!(matches!(rx.try_recv().unwrap(), AgentEvent::Error { .. }));
        assert!(matches!(rx.try_recv().unwrap(), AgentEvent::TurnInterrupted { .. }));
        match rx.try_recv().expect("SessionEnd event") {
            AgentEvent::SessionEnd { is_error, stop_reason, .. } => {
                assert!(is_error);
                match stop_reason {
                    Some(StopReason::ProviderError { code, message, vendor_code }) => {
                        assert_eq!(code, RpcError::INTERNAL_ERROR);
                        assert!(message.starts_with("You've hit your usage limit"));
                        assert_eq!(vendor_code.as_deref(), Some("usageLimitExceeded"));
                    }
                    other => panic!("expected StopReason::ProviderError, got {other:?}"),
                }
            }
            other => panic!("expected SessionEnd, got {other:?}"),
        }
    }

    // -------------------------------------------------------------------
    // AcpSession::stop -- graceful (stdin close) vs. forced.
    //
    // Fixture mirrors `gate4agent_testkit::acp_fixture_launch`'s verified
    // handshake responder (PowerShell on Windows, `python3` on Unix): it
    // answers `initialize` and `session/new` with the minimal
    // all-`#[serde(default)]` result shape both `AgentCapabilities` and
    // `SessionLoadResult` accept, then either exits 0 on stdin EOF
    // (graceful fixture) or spins ignoring EOF (ignore-EOF fixture) --
    // exactly the two outcomes measured live against the real pinned ACP
    // adapters (see `ACP_GRACEFUL_STOP_BOUND_SECS`'s doc comment).
    // -------------------------------------------------------------------

    #[cfg(windows)]
    const WINDOWS_GRACEFUL_SCRIPT: &str = r#"[Console]::OutputEncoding=[Text.Encoding]::UTF8
function Write-JsonLine($value) { [Console]::WriteLine(($value | ConvertTo-Json -Compress -Depth 12)) }
$initialize = [Console]::ReadLine() | ConvertFrom-Json
Write-JsonLine @{jsonrpc='2.0';id=$initialize.id;result=@{}}
$newSession = [Console]::ReadLine() | ConvertFrom-Json
Write-JsonLine @{jsonrpc='2.0';id=$newSession.id;result=@{sessionId='fixture-acp-session'}}
while ($true) {
    $line = [Console]::ReadLine()
    if ($null -eq $line) { exit 0 }
}"#;
    #[cfg(windows)]
    const WINDOWS_IGNORE_EOF_SCRIPT: &str = r#"[Console]::OutputEncoding=[Text.Encoding]::UTF8
function Write-JsonLine($value) { [Console]::WriteLine(($value | ConvertTo-Json -Compress -Depth 12)) }
$initialize = [Console]::ReadLine() | ConvertFrom-Json
Write-JsonLine @{jsonrpc='2.0';id=$initialize.id;result=@{}}
$newSession = [Console]::ReadLine() | ConvertFrom-Json
Write-JsonLine @{jsonrpc='2.0';id=$newSession.id;result=@{sessionId='fixture-acp-session'}}
while ($true) {
    $line = [Console]::ReadLine()
    if ($null -eq $line) { Start-Sleep -Milliseconds 200 }
}"#;
    #[cfg(not(windows))]
    const UNIX_GRACEFUL_SCRIPT: &str = r#"import json,sys
def read_message():
 line=sys.stdin.readline()
 if not line: return None
 return json.loads(line)
def write_message(message):
 print(json.dumps(message),flush=True)

initialize=read_message()
write_message({'jsonrpc':'2.0','id':initialize.get('id'),'result':{}})
new_session=read_message()
write_message({'jsonrpc':'2.0','id':new_session.get('id'),'result':{'sessionId':'fixture-acp-session'}})
while True:
 msg=read_message()
 if msg is None:
  sys.exit(0)"#;
    #[cfg(not(windows))]
    const UNIX_IGNORE_EOF_SCRIPT: &str = r#"import json,sys,time
def read_message():
 line=sys.stdin.readline()
 if not line: return None
 return json.loads(line)
def write_message(message):
 print(json.dumps(message),flush=True)

initialize=read_message()
write_message({'jsonrpc':'2.0','id':initialize.get('id'),'result':{}})
new_session=read_message()
write_message({'jsonrpc':'2.0','id':new_session.get('id'),'result':{'sessionId':'fixture-acp-session'}})
while True:
 msg=read_message()
 if msg is None:
  time.sleep(0.2)"#;

    #[cfg(windows)]
    fn acp_graceful_exit_launch() -> LaunchSpec {
        LaunchSpec {
            program: "powershell.exe".to_owned(),
            fixed_args: vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-Command".to_owned(),
                WINDOWS_GRACEFUL_SCRIPT.to_owned(),
            ],
        }
    }

    #[cfg(not(windows))]
    fn acp_graceful_exit_launch() -> LaunchSpec {
        LaunchSpec {
            program: "python3".to_owned(),
            fixed_args: vec!["-u".to_owned(), "-c".to_owned(), UNIX_GRACEFUL_SCRIPT.to_owned()],
        }
    }

    #[cfg(windows)]
    fn acp_ignore_eof_launch() -> LaunchSpec {
        LaunchSpec {
            program: "powershell.exe".to_owned(),
            fixed_args: vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-ExecutionPolicy".to_owned(),
                "Bypass".to_owned(),
                "-Command".to_owned(),
                WINDOWS_IGNORE_EOF_SCRIPT.to_owned(),
            ],
        }
    }

    #[cfg(not(windows))]
    fn acp_ignore_eof_launch() -> LaunchSpec {
        LaunchSpec {
            program: "python3".to_owned(),
            fixed_args: vec!["-u".to_owned(), "-c".to_owned(), UNIX_IGNORE_EOF_SCRIPT.to_owned()],
        }
    }

    async fn spawn_fixture_acp_session(launch: LaunchSpec) -> AcpSession {
        AcpSession::spawn_with_launch(
            CliTool::ClaudeCode,
            &std::env::current_dir().expect("cwd"),
            AcpSessionOptions::default(),
            &launch,
        )
        .await
        .expect("fixture ACP handshake must succeed")
    }

    #[tokio::test]
    async fn acp_stop_graceful_reports_the_real_exit_code_for_a_process_that_ends_itself() {
        let session = spawn_fixture_acp_session(acp_graceful_exit_launch()).await;

        let outcome = session
            .stop(false)
            .await
            .expect("stop(force=false) must succeed for a process that exits on its own");

        assert_eq!(outcome.exit_code, Some(0), "outcome: {outcome:?}");
        assert!(
            !outcome.forced,
            "a process that already exited on its own must not be reported as forced"
        );
    }

    #[tokio::test]
    async fn acp_stop_graceful_falls_back_to_a_kill_once_the_bound_elapses() {
        let session = spawn_fixture_acp_session(acp_ignore_eof_launch()).await;

        let outcome = session
            .stop(false)
            .await
            .expect("stop(force=false) must still succeed by falling back to a kill");

        assert!(
            outcome.forced,
            "a process that ignores stdin EOF must be force-killed once the graceful bound elapses"
        );
        assert!(outcome.exit_code.is_none(), "outcome: {outcome:?}");
    }

    // -------------------------------------------------------------------
    // run_prompt_watchdog -- idle-silence semantics for `start_prompt`.
    //
    // Replaces a fixed 120s TOTAL-DURATION bound (which fired on a healthy,
    // still-working turn -- see `AcpSessionOptions::prompt_idle_timeout`'s
    // doc comment for the live incident this closes) with an IDLE bound:
    // the watchdog only fires on complete silence, and every signal on
    // `turn_activity` resets its window. Both tests call the watchdog
    // function directly (as `start_prompt` itself does via `tokio::spawn`)
    // against a manually registered `pending` entry, rather than driving a
    // full `session/prompt` round trip through a real agent -- the fixture
    // process (`acp_ignore_eof_launch`) only needs to exist so
    // `send_cancel_notification`'s write has a real process to target; it
    // never needs to answer. Both use `tokio::time::pause`/`advance`, never
    // a real sleep, per the task's own requirement.
    //
    // Observing a paused-clock timer actually fire needs a genuine suspend,
    // not a `try_recv`/`yield_now` spin: `tokio::time::advance` only moves
    // the frozen clock forward (plus one internal `yield_now`, which just
    // re-queues the calling task without ever emptying the run queue).
    // Tokio's timer driver only checks a paused clock's new value against
    // pending timers when the runtime actually parks (goes idle with
    // nothing left ready to run) -- a self-requeuing `yield_now` spin never
    // reaches that state, so no number of extra yields after `advance` can
    // observe a timer-driven fire (measured live 2026-09-25: a bare
    // `tokio::spawn(sleep(..).await)` behind `pause`+`advance`+100
    // `yield_now`s never completed either, ruling out anything specific to
    // this watchdog's own `select!`). `recv_bounded_by_real_time` below
    // fixes this by racing the real event against a background OS thread's
    // real (wall-clock, never paused) timer inside a `select!` -- a genuine
    // suspend that lets the runtime park, while still bounding the wait so
    // a genuinely broken watchdog fails the test instead of hanging it.
    // -------------------------------------------------------------------

    /// Await the next broadcast event, bounded by a REAL wall-clock
    /// duration rather than the paused tokio clock -- see the section
    /// comment above for why a `try_recv`/`yield_now` spin cannot observe
    /// a paused-clock timer's fire at all, no matter how many iterations.
    /// Racing `events.recv()` against a real background-thread timer forces
    /// the calling task to genuinely suspend (both branches legitimately
    /// `Pending`), which is what lets the runtime park and its timer driver
    /// notice an already-elapsed `tokio::time::sleep` and wake it.
    async fn recv_bounded_by_real_time(
        events: &mut broadcast::Receiver<AgentEvent>,
        real_bound: Duration,
    ) -> Option<AgentEvent> {
        let (bail_tx, bail_rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(real_bound);
            let _ = bail_tx.send(());
        });
        tokio::select! {
            event = events.recv() => event.ok(),
            _ = bail_rx => None,
        }
    }

    #[tokio::test]
    async fn start_prompt_watchdog_survives_activity_past_the_old_120s_mark() {
        let session = spawn_fixture_acp_session(acp_ignore_eof_launch()).await;
        let mut events = session.subscribe();
        let id = RpcId::Number(9001);
        let receiver = session.pending.register(id.clone());
        let turn_activity = Arc::new(Notify::new());
        let idle_timeout = Duration::from_secs(20);

        tokio::time::pause();
        let watchdog = tokio::spawn(run_prompt_watchdog(
            session.tx.clone(),
            session.pending.clone(),
            id.clone(),
            receiver,
            Arc::clone(&turn_activity),
            idle_timeout,
            Arc::clone(&session.process),
            "fixture-acp-session".to_owned(),
        ));
        // Let the watchdog register its first idle window before advancing.
        tokio::task::yield_now().await;

        // Nine 15s gaps, each well under the 20s idle window, total 135s --
        // more than the old fixed 120s bound this watchdog replaced. Each
        // `notify_one` is a DIRECT waker fire, not timer-driven, so (unlike
        // `advance` alone) it reliably drives the watchdog's `select!` to
        // consume the activity branch and restart its idle window fresh.
        for _ in 0..9 {
            tokio::time::advance(Duration::from_secs(15)).await;
            turn_activity.notify_one();
            tokio::task::yield_now().await;
        }

        // Proof this test can actually fail (is not vacuous): advance to
        // just short of the fresh 20s window the last reset opened, and
        // confirm nothing has fired yet using a real, bounded, fully-parked
        // wait -- then advance PAST the window and confirm the SAME
        // watchdog now does fire. Without this second phase, "no event
        // arrived" would be true regardless of whether activity resets the
        // window at all, since a broken watchdog that never fires under any
        // circumstance would pass the first assertion just as easily.
        tokio::time::advance(Duration::from_secs(19)).await;
        assert!(
            recv_bounded_by_real_time(&mut events, Duration::from_secs(2))
                .await
                .is_none(),
            "activity that keeps resetting the idle window must never interrupt the turn before it elapses"
        );
        assert_eq!(
            session.pending.len(),
            1,
            "the request must still be pending -- the watchdog must not have fired yet"
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        let fired = recv_bounded_by_real_time(&mut events, Duration::from_secs(5))
            .await
            .expect(
                "the watchdog must be ABLE to fire once its idle window elapses -- otherwise \
                 the earlier 'nothing fired' assertion would be vacuous",
            );
        assert!(
            matches!(&fired, AgentEvent::Error { message } if message.contains("idle")),
            "expected an idle Error event, got {fired:?}"
        );

        watchdog.abort();
        let _ = session.kill().await;
    }

    #[tokio::test]
    async fn start_prompt_watchdog_fires_on_full_silence_and_deregisters_the_pending_request() {
        let session = spawn_fixture_acp_session(acp_ignore_eof_launch()).await;
        let mut events = session.subscribe();
        let id = RpcId::Number(9002);
        let receiver = session.pending.register(id.clone());
        let turn_activity = Arc::new(Notify::new());
        let idle_timeout = Duration::from_secs(10);

        tokio::time::pause();
        let _watchdog = tokio::spawn(run_prompt_watchdog(
            session.tx.clone(),
            session.pending.clone(),
            id.clone(),
            receiver,
            Arc::clone(&turn_activity),
            idle_timeout,
            Arc::clone(&session.process),
            "fixture-acp-session".to_owned(),
        ));
        tokio::task::yield_now().await;

        // No activity at all for the entire idle window. See
        // `recv_bounded_by_real_time`'s doc comment: `advance` alone never
        // drives the watchdog's `sleep` branch to completion, so the wait
        // for the resulting event must be a genuine suspend, bounded by
        // real time rather than the (now frozen) tokio clock. Advancing by
        // exactly `idle_timeout` lands the paused clock AT the sleep's
        // deadline, not past it -- tokio's timer wheel treats that as not
        // yet elapsed (measured live 2026-09-25: advancing by exactly
        // `idle_timeout` never fired the watchdog even once fully parked;
        // one extra millisecond past it fired reliably), so advance one
        // millisecond past the deadline rather than exactly onto it.
        tokio::time::advance(idle_timeout + Duration::from_millis(1)).await;
        let first_event = recv_bounded_by_real_time(&mut events, Duration::from_secs(5))
            .await
            .expect("watchdog did not fire even once fully parked after the idle window elapsed");

        match first_event {
            AgentEvent::Error { message } => {
                assert!(message.contains("idle"), "message was: {message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        match events.recv().await.expect("TurnInterrupted event") {
            AgentEvent::TurnInterrupted { reason } => {
                assert!(reason.contains("idle"), "reason was: {reason}");
            }
            other => panic!("expected TurnInterrupted, got {other:?}"),
        }
        assert!(
            session.pending.is_empty(),
            "the pending request must be de-registered once the watchdog fires"
        );

        let _ = session.kill().await;
    }

    // -------------------------------------------------------------------
    // Drop -- a session dropped without kill()/stop() must still reap its
    // child. `acp_reader_loop` only exits once the child stops running, so
    // without `Drop` a forgotten `AcpSession` leaks both the process and
    // the blocking-pool thread polling it forever.
    // -------------------------------------------------------------------

    /// Whether `pid` is in the OS process table. A probe failure panics
    /// instead of reading as "gone": an unreadable table must not let the
    /// drop test below pass without having looked.
    fn process_is_listed(pid: u32) -> bool {
        crate::pty::os_process::query_process_tree_rows()
            .expect("the OS process table must be readable")
            .iter()
            .any(|row| row.pid == pid)
    }

    #[tokio::test]
    async fn dropping_a_session_without_kill_reaps_the_child_process() {
        let session = spawn_fixture_acp_session(acp_ignore_eof_launch()).await;
        let pid = session.process_id().expect("fixture process must report a pid");
        // Dropped on purpose without `kill()`/`stop()` -- exercising exactly
        // the leak `Drop` exists to close.
        assert!(
            process_is_listed(pid),
            "the probe must see the live fixture (pid={pid}) before the drop, or the check below proves nothing"
        );
        drop(session);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !process_is_listed(pid) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("dropping the AcpSession did not reap its child process (pid={pid})");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
