//! ACP host-side handler trait, host policy, and bridge adapter.
//!
//! Rather than forcing callers to implement the low-level [`HostHandler`] with
//! raw string/JSON matching, this module provides a typed trait
//! [`AcpHostHandler`] with safe defaults. Internally [`AcpHostAdapter`]
//! bridges from [`HostHandler`] → [`AcpHostHandler`], so the reader loop can
//! use the existing RPC infrastructure unchanged.
//!
//! [`HostPolicy`] is the one production implementation's decision function:
//! it picks what `initialize` declares in `clientCapabilities` AND how
//! `session/request_permission` answers, so the two can never drift apart
//! (a policy that declares `terminal: false` also never selects an allow
//! option for an `execute` tool call, and vice versa).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::core::types::HostDecisionAuthority;
use crate::rpc::handler::{HostHandler, HostOutcome};
use crate::rpc::message::RpcError;

use super::gate::{self, DangerousCommandGate};
use super::protocol::{
    ClientCapabilities, FsCapabilities, FsReadParams, FsWriteParams, PermissionOption,
    PermissionOptionKind, PermissionOutcome, PermissionRequestParams, PermissionToolCall,
    TerminalCreateParams, TerminalExitStatus, TerminalIdParams, TerminalOutputResult, ToolKind,
};
use super::terminal::TerminalStore;

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
pub(crate) trait AcpHostHandler: Send + Sync {
    /// Agent wants to read a file from the host filesystem.
    fn fs_read_text_file(&self, params: &FsReadParams) -> Result<String, String> {
        Err(format!("fs/read_text_file not supported: {}", params.path))
    }

    /// Agent wants to write (fully replace) a file on the host filesystem.
    fn fs_write_text_file(&self, params: &FsWriteParams) -> Result<(), String> {
        Err(format!("fs/write_text_file not supported: {}", params.path))
    }

    /// Agent wants to run a command on the host and get a terminal id back.
    fn terminal_create(&self, params: &TerminalCreateParams) -> Result<String, String> {
        let _ = params;
        Err("terminal/create not supported".to_string())
    }

    /// Agent wants the accumulated output of a terminal it created earlier.
    fn terminal_output(&self, params: &TerminalIdParams) -> Result<TerminalOutputResult, String> {
        let _ = params;
        Err("terminal/output not supported".to_string())
    }

    /// Agent wants to block until a terminal's command exits.
    fn terminal_wait_for_exit(
        &self,
        params: &TerminalIdParams,
    ) -> Result<TerminalExitStatus, String> {
        let _ = params;
        Err("terminal/wait_for_exit not supported".to_string())
    }

    /// Agent wants to kill a terminal's command without releasing the id.
    fn terminal_kill(&self, params: &TerminalIdParams) -> Result<(), String> {
        let _ = params;
        Err("terminal/kill not supported".to_string())
    }

    /// Agent is done with a terminal — kill it if still running and forget it.
    fn terminal_release(&self, params: &TerminalIdParams) -> Result<(), String> {
        let _ = params;
        Err("terminal/release not supported".to_string())
    }

    /// Agent is requesting permission to perform a tool call.
    ///
    /// Per the ACP spec, a client (host) "MAY automatically allow or reject
    /// permission requests according to the user['s] settings" -- this is
    /// not a UI hook, it's a decision function. The default is the safest
    /// possible answer: never select any of the offered options.
    fn request_permission(&self, params: &PermissionRequestParams) -> PermissionOutcome {
        let _ = params;
        PermissionOutcome::Cancelled
    }

    /// Whether the dangerous-command gate (`super::gate`) would block this
    /// `session/request_permission` call outright, independent of whatever
    /// [`request_permission`](Self::request_permission) itself would go on
    /// to decide.
    ///
    /// The default answer is `false` -- only [`PolicyHostHandler`] (the one
    /// implementation the gate is actually wired into) overrides it.
    /// [`AcpHostAdapter::handle_deferrable`] calls this BEFORE deciding
    /// whether to defer a `session/request_permission` call to an operator,
    /// so a gate block is always decided immediately and is NEVER left
    /// waiting on one -- the dangerous-command gate outranks deferral
    /// exactly as it already outranks [`HostPolicy`] (see the gate's own
    /// module doc comment). `request_permission` checks the same condition
    /// for the immediate path, so the two can never disagree about whether
    /// a given call is gate-blocked.
    fn permission_blocked_by_gate(&self, tool_call: &PermissionToolCall) -> bool {
        let _ = tool_call;
        false
    }

    /// Whether the dangerous-command gate would block this `terminal/
    /// create` call outright, independent of whatever
    /// [`terminal_create`](Self::terminal_create) itself would go on to
    /// decide -- the `terminal/create` counterpart to
    /// [`permission_blocked_by_gate`](Self::permission_blocked_by_gate).
    ///
    /// The default answer is `false` -- only [`PolicyHostHandler`] overrides
    /// it. [`AcpHostAdapter::decision_authority`] calls this purely to
    /// classify WHO decided a completed `terminal/create` call for
    /// `AgentEvent::RpcIncomingRequest`'s audit trail; it is never consulted
    /// to change what `terminal_create` itself decides -- that call runs the
    /// exact same check on its own, so the two can never disagree about
    /// whether a given call is gate-blocked.
    fn terminal_create_blocked_by_gate(&self, params: &TerminalCreateParams) -> bool {
        let _ = params;
        false
    }
}

// ---------------------------------------------------------------------------
// HostPolicy
// ---------------------------------------------------------------------------

/// The host's authority mode for one ACP session — governs both what
/// `clientCapabilities` `initialize` declares and how `session/request_
/// permission` is answered. Modes exist primarily so a parent agent can
/// bound a child agent's authority, not to prompt a human: every mode here
/// resolves permission requests on its own, per the ACP spec's explicit
/// allowance for a client to decide automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPolicy {
    /// Everything allowed, no distinctions. Prefers `allow_always` options
    /// so an agent that respects them stops asking for the same class of
    /// operation again this session.
    Yolo,
    /// Reads, writes, and command execution are all allowed without
    /// prompting — the default. Prefers `allow_once` options over
    /// `allow_always`, so each grant stays an individually observable
    /// decision (see `AgentEvent::RpcIncomingRequest`) rather than one the
    /// agent stops asking about.
    Auto,
    /// Only read-shaped operations (`fs/read_text_file`, and tool calls of
    /// kind `read`/`search`/`think`/`fetch`) are allowed; writes, deletes,
    /// moves, and command execution are refused.
    ReadOnly,
    /// Refuse everything. Today's behavior prior to `HostPolicy` existing;
    /// kept reachable because it is load-bearing for the fail-closed tests.
    Deny,
}

impl Default for HostPolicy {
    /// `Auto` — the owner's decision: default to answering permission
    /// requests without a human in the loop, not to fail closed.
    fn default() -> Self {
        HostPolicy::Auto
    }
}

// ---------------------------------------------------------------------------
// PermissionDeferral
// ---------------------------------------------------------------------------

/// Session-level policy for [`AcpHostAdapter::handle_deferrable`]: whether
/// `session/request_permission` may return [`HostOutcome::Deferred`] instead
/// of being decided by the wrapped [`AcpHostHandler`] the instant it arrives.
///
/// This is a THIRD axis alongside [`HostPolicy`] and
/// [`super::gate::DangerousCommandGate`] -- and, like the gate, it is
/// switched independently of `HostPolicy` rather than folded into it: a
/// permissive `HostPolicy` (`Yolo`, `Auto`) still answers every permission
/// request itself when deferral is `Disabled`, and a restrictive one
/// (`ReadOnly`, `Deny`) still gets deferred when it is `Enabled` -- "how much
/// authority does this session have" and "should an operator get a chance to
/// answer before that authority decides" are independent questions. Default:
/// `Disabled`, so a caller that never sets
/// [`crate::acp::session::AcpSessionOptions::defer_permission_requests`]
/// gets exactly today's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionDeferral {
    /// [`AcpHostAdapter::handle_deferrable`] is byte-for-byte
    /// `Immediate(self.handle(...))` for every method, matching the
    /// [`HostHandler`] trait's own default.
    Disabled,
    /// `session/request_permission` returns `Deferred` UNLESS the
    /// dangerous-command gate would block it -- see
    /// [`AcpHostHandler::permission_blocked_by_gate`]. Every other method is
    /// still decided immediately regardless.
    Enabled,
}

impl Default for PermissionDeferral {
    fn default() -> Self {
        PermissionDeferral::Disabled
    }
}

impl HostPolicy {
    /// The `clientCapabilities` this policy declares at `initialize`.
    pub(crate) fn client_capabilities(self) -> ClientCapabilities {
        match self {
            HostPolicy::Yolo | HostPolicy::Auto => ClientCapabilities {
                fs: FsCapabilities { read_text_file: true, write_text_file: true },
                terminal: true,
            },
            HostPolicy::ReadOnly => ClientCapabilities {
                fs: FsCapabilities { read_text_file: true, write_text_file: false },
                terminal: false,
            },
            HostPolicy::Deny => ClientCapabilities {
                fs: FsCapabilities { read_text_file: false, write_text_file: false },
                terminal: false,
            },
        }
    }

    /// Whether this policy allows real file writes and command execution.
    fn allows_mutation(self) -> bool {
        matches!(self, HostPolicy::Yolo | HostPolicy::Auto)
    }

    /// Whether this policy allows real file reads.
    fn allows_read(self) -> bool {
        !matches!(self, HostPolicy::Deny)
    }

    /// Pick one of the agent's offered `options` for a `session/request_
    /// permission` call, or decline to pick any of them.
    ///
    /// `options` is whatever subset of the four
    /// [`PermissionOptionKind`] values the agent chose to offer -- this
    /// walks a preference order and returns the first offered kind that
    /// matches, so it tolerates an agent that omits some (or three) of
    /// them.
    pub(crate) fn select_permission_option(
        self,
        tool_call: &PermissionToolCall,
        options: &[PermissionOption],
    ) -> PermissionOutcome {
        let prefer_allow = match self {
            HostPolicy::Yolo | HostPolicy::Auto => true,
            HostPolicy::ReadOnly => tool_call.kind.is_read_only(),
            HostPolicy::Deny => false,
        };
        let preference: &[PermissionOptionKind] = if prefer_allow {
            if matches!(self, HostPolicy::Yolo) {
                &[PermissionOptionKind::AllowAlways, PermissionOptionKind::AllowOnce]
            } else {
                &[PermissionOptionKind::AllowOnce, PermissionOptionKind::AllowAlways]
            }
        } else {
            &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
        };
        select_offered_option(options, preference)
    }
}

/// Walk `preference` in order and return the first kind the agent actually
/// offered, as a `Selected` outcome naming that option's own id; `Cancelled`
/// if it offered none of them.
///
/// Shared deliberately between [`HostPolicy::select_permission_option`] and
/// [`super::session::AcpSession::resolve_pending_request_as`]: policy and
/// operator must pick from the same list by the same rule, or the same intent
/// would mean two different things depending on who acted on it. Neither
/// caller may invent an `option_id` — an id the agent never offered is not
/// refused by the agent so much as ignored by it, which would grant nothing
/// while reporting success.
pub(crate) fn select_offered_option(
    options: &[PermissionOption],
    preference: &[PermissionOptionKind],
) -> PermissionOutcome {
    for kind in preference {
        if let Some(option) = options.iter().find(|option| option.kind == *kind) {
            return PermissionOutcome::Selected { option_id: option.option_id.clone() };
        }
    }
    PermissionOutcome::Cancelled
}

// ---------------------------------------------------------------------------
// PolicyHostHandler — the one production AcpHostHandler
// ---------------------------------------------------------------------------

/// The production [`AcpHostHandler`]: serves real filesystem reads/writes
/// and real terminal execution, gated by a [`HostPolicy`]. This is the only
/// handler `AcpSession` ever constructs outside tests.
pub(crate) struct PolicyHostHandler {
    policy: HostPolicy,
    working_dir: PathBuf,
    terminals: TerminalStore,
    /// Whether the dangerous-command gate (`super::gate`) runs ahead of
    /// `policy` for `terminal/create` and `execute`-kind `session/request_
    /// permission` -- a decision independent of `policy` itself, see
    /// [`DangerousCommandGate`].
    dangerous_command_gate: DangerousCommandGate,
}

impl PolicyHostHandler {
    pub(crate) fn new(
        policy: HostPolicy,
        working_dir: PathBuf,
        dangerous_command_gate: DangerousCommandGate,
    ) -> Self {
        Self { policy, working_dir, terminals: TerminalStore::new(), dangerous_command_gate }
    }

    /// The directory a `terminal/create` call actually runs in: its own
    /// `cwd` override when present, otherwise the session's working
    /// directory -- shared by `AcpHostHandler::terminal_create` and
    /// `AcpHostHandler::terminal_create_blocked_by_gate` so the two never
    /// evaluate the gate against different directories.
    fn terminal_create_cwd(&self, cwd_override: Option<&str>) -> PathBuf {
        cwd_override
            .map(PathBuf::from)
            .unwrap_or_else(|| self.working_dir.clone())
    }
}

/// Apply an ACP `fs/read_text_file` line window (`line` 1-based, `limit` a
/// line count) to already-read file content. Absent both, the whole file.
fn windowed_read(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return content.to_owned();
    }
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let lines = content.split_inclusive('\n').skip(start);
    match limit {
        Some(n) => lines.take(n as usize).collect(),
        None => lines.collect(),
    }
}

impl AcpHostHandler for PolicyHostHandler {
    fn fs_read_text_file(&self, params: &FsReadParams) -> Result<String, String> {
        if !self.policy.allows_read() {
            return Err(format!("fs/read_text_file denied by host policy: {}", params.path));
        }
        let content = std::fs::read_to_string(&params.path).map_err(|e| e.to_string())?;
        Ok(windowed_read(&content, params.line, params.limit))
    }

    fn fs_write_text_file(&self, params: &FsWriteParams) -> Result<(), String> {
        if !self.policy.allows_mutation() {
            return Err(format!("fs/write_text_file denied by host policy: {}", params.path));
        }
        std::fs::write(&params.path, &params.content).map_err(|e| e.to_string())
    }

    fn terminal_create(&self, params: &TerminalCreateParams) -> Result<String, String> {
        // The dangerous-command gate runs before `policy` gets a say, and
        // is not skipped by any `HostPolicy` value including `Yolo` -- see
        // `DangerousCommandGate`'s doc comment for why this is a second,
        // separately-switched axis rather than folded into the policy.
        if self.terminal_create_blocked_by_gate(params) {
            let cwd = self.terminal_create_cwd(params.cwd.as_deref());
            let verdict = gate::evaluate_command(&params.command, &params.args, &cwd);
            // `terminal_create_blocked_by_gate` already proved this is a
            // `Block` verdict -- `refusal_message()` only returns `None`
            // for `Allow`/`Uncertain`, neither of which `is_blocked()`
            // reports as blocked.
            let refusal = verdict.refusal_message().unwrap_or_default();
            return Err(format!("terminal/create {refusal}"));
        }
        if !self.policy.allows_mutation() {
            return Err("terminal/create denied by host policy".to_string());
        }
        self.terminals.create(&self.working_dir, params)
    }

    fn terminal_output(&self, params: &TerminalIdParams) -> Result<TerminalOutputResult, String> {
        self.terminals.output(&params.terminal_id)
    }

    fn terminal_wait_for_exit(
        &self,
        params: &TerminalIdParams,
    ) -> Result<TerminalExitStatus, String> {
        self.terminals.wait_for_exit(&params.terminal_id)
    }

    fn terminal_kill(&self, params: &TerminalIdParams) -> Result<(), String> {
        self.terminals.kill(&params.terminal_id)
    }

    fn terminal_release(&self, params: &TerminalIdParams) -> Result<(), String> {
        self.terminals.release(&params.terminal_id)
    }

    fn request_permission(&self, params: &PermissionRequestParams) -> PermissionOutcome {
        // Same gate, same "above the policy" placement as `terminal_create`
        // -- and now the single source of truth `AcpHostAdapter::
        // handle_deferrable` also consults (via `permission_blocked_by_gate`
        // below) before it will even consider deferring this call to an
        // operator, so the two paths can never disagree about whether a
        // given call is gate-blocked. A `Block` verdict here has no wire
        // field to carry its reason on (`PermissionOutcome` is
        // `Selected`/`Cancelled`, not an error), so it is answered the same
        // way `HostPolicy::Deny` answers any execute-kind request: prefer a
        // reject-kind option the agent offered, else decline to pick any of
        // them.
        if self.permission_blocked_by_gate(&params.tool_call) {
            return HostPolicy::Deny.select_permission_option(&params.tool_call, &params.options);
        }
        self.policy.select_permission_option(&params.tool_call, &params.options)
    }

    fn permission_blocked_by_gate(&self, tool_call: &PermissionToolCall) -> bool {
        self.dangerous_command_gate == DangerousCommandGate::Enforced
            && tool_call.kind == ToolKind::Execute
            && gate::evaluate_permission_tool_call(tool_call, &self.working_dir).is_blocked()
    }

    fn terminal_create_blocked_by_gate(&self, params: &TerminalCreateParams) -> bool {
        self.dangerous_command_gate == DangerousCommandGate::Enforced
            && gate::evaluate_command(
                &params.command,
                &params.args,
                &self.terminal_create_cwd(params.cwd.as_deref()),
            )
            .is_blocked()
    }
}

// ---------------------------------------------------------------------------
// AcpHostAdapter — bridges AcpHostHandler → HostHandler
// ---------------------------------------------------------------------------

/// Bridges [`AcpHostHandler`] → [`HostHandler`] for use in the reader loop.
///
/// Wraps `Arc<dyn AcpHostHandler>` so it can be cloned cheaply without
/// requiring `'static + Clone` bounds on the trait, plus the session's
/// [`PermissionDeferral`] policy -- the one piece `handle_deferrable` needs
/// that a bare `Arc<dyn AcpHostHandler>` cannot answer on its own (whether
/// THIS session is even allowed to leave a `session/request_permission`
/// call unanswered for a while).
pub(crate) struct AcpHostAdapter {
    inner: std::sync::Arc<dyn AcpHostHandler>,
    deferral: PermissionDeferral,
}

impl AcpHostAdapter {
    pub(crate) fn new(
        inner: std::sync::Arc<dyn AcpHostHandler>,
        deferral: PermissionDeferral,
    ) -> Self {
        Self { inner, deferral }
    }

    /// Classify WHO decided (or will decide) `method`/`params`, for
    /// [`crate::core::types::HostRequestDecision`]'s `by` field.
    ///
    /// Purely observational: it re-runs the exact same gate query
    /// [`handle`](HostHandler::handle)/[`handle_deferrable`] already
    /// consulted (`permission_blocked_by_gate`/
    /// `terminal_create_blocked_by_gate`) so the two can never disagree
    /// about whether a given call was gate-blocked, but it never feeds back
    /// into what either of those methods decides.
    ///
    /// Only `terminal/create` and `session/request_permission` can ever
    /// come back `Gate` -- every other method answers `Policy` here, which
    /// is correct even for `terminal/output`/`terminal/wait_for_exit`/
    /// `terminal/kill`/`terminal/release` (no `HostPolicy` value gates
    /// those beyond `terminal/create` itself, so "decided immediately, not
    /// gate-blocked, not deferred" -- `Policy`'s own definition -- is the
    /// honest answer for them too). `Deferred`, `Operator`, and
    /// `DeadlinePolicy` are never returned here -- a caller reaches this
    /// only for a request that is being decided NOW, on this thread, and
    /// those three describe request states this call never sees (see
    /// `crate::core::types::HostDecisionAuthority`'s own doc comment for
    /// why `Operator`/`DeadlinePolicy` only ever come from `AcpSession::
    /// resolve_pending_request`/`expire_deadlines` instead).
    pub(crate) fn decision_authority(
        &self,
        method: &str,
        params: Option<&Value>,
    ) -> HostDecisionAuthority {
        let gate_blocked = match method {
            "terminal/create" => params
                .and_then(|v| serde_json::from_value::<TerminalCreateParams>(v.clone()).ok())
                .is_some_and(|p| self.inner.terminal_create_blocked_by_gate(&p)),
            "session/request_permission" => params
                .and_then(|v| serde_json::from_value::<PermissionRequestParams>(v.clone()).ok())
                .is_some_and(|p| self.inner.permission_blocked_by_gate(&p.tool_call)),
            _ => false,
        };
        if gate_blocked {
            HostDecisionAuthority::Gate
        } else {
            HostDecisionAuthority::Policy
        }
    }
}

impl HostHandler for AcpHostAdapter {
    fn handle(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match method {
            "fs/read_text_file" => {
                let p: FsReadParams = parse_params(params)?;
                self.inner
                    .fs_read_text_file(&p)
                    .map(|content| json!({ "content": content }))
                    .map_err(|msg| RpcError {
                        code: RpcError::PERMISSION_DENIED,
                        message: msg,
                        data: None,
                    })
            }

            "fs/write_text_file" => {
                let p: FsWriteParams = parse_params(params)?;
                self.inner
                    .fs_write_text_file(&p)
                    .map(|()| json!({}))
                    .map_err(|msg| RpcError {
                        code: RpcError::PERMISSION_DENIED,
                        message: msg,
                        data: None,
                    })
            }

            "terminal/create" => {
                let p: TerminalCreateParams = parse_params(params)?;
                self.inner
                    .terminal_create(&p)
                    .map(|terminal_id| json!({ "terminalId": terminal_id }))
                    .map_err(|msg| RpcError {
                        code: RpcError::PERMISSION_DENIED,
                        message: msg,
                        data: None,
                    })
            }

            "terminal/output" => {
                let p: TerminalIdParams = parse_params(params)?;
                self.inner
                    .terminal_output(&p)
                    .map(|result| serde_json::to_value(result).unwrap_or(Value::Null))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "terminal/wait_for_exit" => {
                let p: TerminalIdParams = parse_params(params)?;
                self.inner
                    .terminal_wait_for_exit(&p)
                    .map(|result| serde_json::to_value(result).unwrap_or(Value::Null))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "terminal/kill" => {
                let p: TerminalIdParams = parse_params(params)?;
                self.inner
                    .terminal_kill(&p)
                    .map(|()| json!({}))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "terminal/release" => {
                let p: TerminalIdParams = parse_params(params)?;
                self.inner
                    .terminal_release(&p)
                    .map(|()| json!({}))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "session/request_permission" => {
                let p: PermissionRequestParams = parse_params(params)?;
                let outcome = self.inner.request_permission(&p);
                Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
            }

            other => Err(RpcError::method_not_found(other)),
        }
    }

    /// Only `session/request_permission`, and only when this session's
    /// [`PermissionDeferral`] is `Enabled`, can return
    /// [`HostOutcome::Deferred`] -- every other method, and every session
    /// with deferral `Disabled`, is `Immediate(self.handle(method, params))`
    /// with today's behavior, byte for byte.
    ///
    /// A gate block is decided immediately even under `Enabled`: the
    /// dangerous-command gate outranks the operator (see
    /// `AcpHostHandler::permission_blocked_by_gate`'s doc comment), so
    /// nobody is ever asked to approve what the gate has already refused.
    fn handle_deferrable(&self, method: &str, params: Option<Value>) -> HostOutcome {
        if method != "session/request_permission" || self.deferral == PermissionDeferral::Disabled {
            return HostOutcome::Immediate(self.handle(method, params));
        }

        // Parse once, here, purely to run the gate check ahead of the
        // deferral decision. A params blob that fails to parse takes the
        // immediate path too -- `handle`'s own `session/request_permission`
        // arm parses `params` again independently and reports the exact
        // same `INVALID_PARAMS` either way, so a parse failure is never
        // silently swallowed into a `Deferred` that can never be answered.
        let Some(parsed) = params
            .clone()
            .and_then(|v| serde_json::from_value::<PermissionRequestParams>(v).ok())
        else {
            return HostOutcome::Immediate(self.handle(method, params));
        };

        if self.inner.permission_blocked_by_gate(&parsed.tool_call) {
            return HostOutcome::Immediate(self.handle(method, params));
        }

        HostOutcome::Deferred
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
    use crate::acp::protocol::ToolCallLocation;
    use serde_json::json;
    use std::sync::Arc;

    /// The trait's own baked-in defaults (deny everything), independent of
    /// any concrete policy — proves nobody weakened the safe fallback.
    struct Bare;
    impl AcpHostHandler for Bare {}

    #[test]
    fn trait_defaults_deny_fs_read() {
        let params = FsReadParams { path: "/etc/passwd".to_owned(), session_id: String::new(), line: None, limit: None };
        assert!(Bare.fs_read_text_file(&params).is_err());
    }

    #[test]
    fn trait_defaults_deny_terminal_create() {
        let params = TerminalCreateParams {
            session_id: String::new(),
            command: "ls".to_owned(),
            args: vec![],
            env: vec![],
            cwd: None,
            output_byte_limit: None,
        };
        assert!(Bare.terminal_create(&params).is_err());
    }

    #[test]
    fn trait_defaults_cancel_permission() {
        let params = PermissionRequestParams {
            session_id: "s1".to_owned(),
            tool_call: PermissionToolCall::default(),
            options: vec![PermissionOption {
                option_id: "a1".to_owned(),
                name: "Allow".to_owned(),
                kind: PermissionOptionKind::AllowOnce,
            }],
        };
        assert_eq!(Bare.request_permission(&params), PermissionOutcome::Cancelled);
    }

    fn adapter(policy: HostPolicy) -> AcpHostAdapter {
        AcpHostAdapter::new(
            Arc::new(PolicyHostHandler::new(policy, std::env::temp_dir(), DangerousCommandGate::Enforced)),
            PermissionDeferral::Disabled,
        )
    }

    fn adapter_with_gate(policy: HostPolicy, gate: DangerousCommandGate) -> AcpHostAdapter {
        AcpHostAdapter::new(
            Arc::new(PolicyHostHandler::new(policy, std::env::temp_dir(), gate)),
            PermissionDeferral::Disabled,
        )
    }

    fn adapter_with_deferral(
        policy: HostPolicy,
        gate: DangerousCommandGate,
        deferral: PermissionDeferral,
    ) -> AcpHostAdapter {
        AcpHostAdapter::new(Arc::new(PolicyHostHandler::new(policy, std::env::temp_dir(), gate)), deferral)
    }

    // -----------------------------------------------------------------------
    // Capabilities per policy
    // -----------------------------------------------------------------------

    #[test]
    fn yolo_and_auto_declare_full_capabilities() {
        for policy in [HostPolicy::Yolo, HostPolicy::Auto] {
            let caps = policy.client_capabilities();
            assert!(caps.fs.read_text_file);
            assert!(caps.fs.write_text_file);
            assert!(caps.terminal);
        }
    }

    #[test]
    fn read_only_declares_read_but_not_write_or_terminal() {
        let caps = HostPolicy::ReadOnly.client_capabilities();
        assert!(caps.fs.read_text_file);
        assert!(!caps.fs.write_text_file);
        assert!(!caps.terminal);
    }

    #[test]
    fn deny_declares_nothing() {
        let caps = HostPolicy::Deny.client_capabilities();
        assert!(!caps.fs.read_text_file);
        assert!(!caps.fs.write_text_file);
        assert!(!caps.terminal);
    }

    #[test]
    fn default_policy_is_auto() {
        assert_eq!(HostPolicy::default(), HostPolicy::Auto);
    }

    // -----------------------------------------------------------------------
    // fs/read_text_file, fs/write_text_file — real I/O
    // -----------------------------------------------------------------------

    #[test]
    fn auto_reads_and_writes_a_real_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("gate4agent_host_test_rw.txt");
        std::fs::write(&path, "before").unwrap();

        let handler =
            PolicyHostHandler::new(HostPolicy::Auto, dir.clone(), DangerousCommandGate::Enforced);
        let read = handler
            .fs_read_text_file(&FsReadParams {
                path: path.to_string_lossy().into_owned(),
                session_id: String::new(),
                line: None,
                limit: None,
            })
            .unwrap();
        assert_eq!(read, "before");

        handler
            .fs_write_text_file(&FsWriteParams {
                path: path.to_string_lossy().into_owned(),
                session_id: String::new(),
                content: "after".to_owned(),
            })
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_only_reads_but_refuses_to_write() {
        let dir = std::env::temp_dir();
        let path = dir.join("gate4agent_host_test_readonly.txt");
        std::fs::write(&path, "content").unwrap();

        let handler =
            PolicyHostHandler::new(HostPolicy::ReadOnly, dir, DangerousCommandGate::Enforced);
        assert!(handler
            .fs_read_text_file(&FsReadParams {
                path: path.to_string_lossy().into_owned(),
                session_id: String::new(),
                line: None,
                limit: None,
            })
            .is_ok());
        assert!(handler
            .fs_write_text_file(&FsWriteParams {
                path: path.to_string_lossy().into_owned(),
                session_id: String::new(),
                content: "nope".to_owned(),
            })
            .is_err());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn deny_refuses_read_and_write() {
        let dir = std::env::temp_dir();
        let handler = PolicyHostHandler::new(HostPolicy::Deny, dir, DangerousCommandGate::Enforced);
        assert!(handler
            .fs_read_text_file(&FsReadParams {
                path: "/etc/passwd".to_owned(),
                session_id: String::new(),
                line: None,
                limit: None,
            })
            .is_err());
        assert!(handler
            .fs_write_text_file(&FsWriteParams {
                path: "/tmp/should-not-be-written".to_owned(),
                session_id: String::new(),
                content: "x".to_owned(),
            })
            .is_err());
    }

    #[test]
    fn windowed_read_applies_line_and_limit() {
        let content = "one\ntwo\nthree\nfour\n";
        assert_eq!(windowed_read(content, None, None), content);
        assert_eq!(windowed_read(content, Some(2), None), "two\nthree\nfour\n");
        assert_eq!(windowed_read(content, Some(2), Some(1)), "two\n");
    }

    // -----------------------------------------------------------------------
    // session/request_permission selection per policy
    // -----------------------------------------------------------------------

    fn options_all_four() -> Vec<PermissionOption> {
        vec![
            PermissionOption { option_id: "ao".to_owned(), name: "Allow once".to_owned(), kind: PermissionOptionKind::AllowOnce },
            PermissionOption { option_id: "aa".to_owned(), name: "Allow always".to_owned(), kind: PermissionOptionKind::AllowAlways },
            PermissionOption { option_id: "ro".to_owned(), name: "Reject once".to_owned(), kind: PermissionOptionKind::RejectOnce },
            PermissionOption { option_id: "ra".to_owned(), name: "Reject always".to_owned(), kind: PermissionOptionKind::RejectAlways },
        ]
    }

    fn tool_call(kind: crate::acp::protocol::ToolKind) -> PermissionToolCall {
        PermissionToolCall {
            tool_call_id: "tc1".to_owned(),
            title: "test".to_owned(),
            kind,
            locations: vec![ToolCallLocation { path: "/repo/file.txt".to_owned(), line: None }],
            raw_input: Value::Null,
        }
    }

    #[test]
    fn yolo_prefers_allow_always() {
        let outcome = HostPolicy::Yolo
            .select_permission_option(&tool_call(crate::acp::protocol::ToolKind::Execute), &options_all_four());
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "aa".to_owned() });
    }

    #[test]
    fn auto_prefers_allow_once_over_allow_always() {
        let outcome = HostPolicy::Auto
            .select_permission_option(&tool_call(crate::acp::protocol::ToolKind::Execute), &options_all_four());
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "ao".to_owned() });
    }

    #[test]
    fn read_only_allows_a_read_kind_tool_call() {
        let outcome = HostPolicy::ReadOnly
            .select_permission_option(&tool_call(crate::acp::protocol::ToolKind::Read), &options_all_four());
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "ao".to_owned() });
    }

    #[test]
    fn read_only_rejects_an_execute_kind_tool_call() {
        let outcome = HostPolicy::ReadOnly
            .select_permission_option(&tool_call(crate::acp::protocol::ToolKind::Execute), &options_all_four());
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "ro".to_owned() });
    }

    #[test]
    fn deny_always_rejects() {
        let outcome = HostPolicy::Deny
            .select_permission_option(&tool_call(crate::acp::protocol::ToolKind::Read), &options_all_four());
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "ro".to_owned() });
    }

    #[test]
    fn deny_cancels_when_the_agent_offers_only_allow_options() {
        let options = vec![
            PermissionOption { option_id: "ao".to_owned(), name: "Allow once".to_owned(), kind: PermissionOptionKind::AllowOnce },
        ];
        let outcome = HostPolicy::Deny
            .select_permission_option(&tool_call(crate::acp::protocol::ToolKind::Execute), &options);
        assert_eq!(outcome, PermissionOutcome::Cancelled);
    }

    #[test]
    fn auto_falls_back_to_allow_always_when_agent_omits_allow_once() {
        // Regression for "agent does not send all four kinds" — here it only
        // offers allow_always and reject_once.
        let options = vec![
            PermissionOption { option_id: "aa".to_owned(), name: "Allow always".to_owned(), kind: PermissionOptionKind::AllowAlways },
            PermissionOption { option_id: "ro".to_owned(), name: "Reject once".to_owned(), kind: PermissionOptionKind::RejectOnce },
        ];
        let outcome = HostPolicy::Auto
            .select_permission_option(&tool_call(crate::acp::protocol::ToolKind::Execute), &options);
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "aa".to_owned() });
    }

    // -----------------------------------------------------------------------
    // Adapter dispatch
    // -----------------------------------------------------------------------

    #[test]
    fn adapter_dispatches_fs_read_success() {
        let dir = std::env::temp_dir();
        let path = dir.join("gate4agent_host_adapter_read.txt");
        std::fs::write(&path, "adapter content").unwrap();

        let result = adapter(HostPolicy::Auto)
            .handle("fs/read_text_file", Some(json!({"path": path.to_string_lossy()})));
        assert_eq!(result.unwrap()["content"], "adapter content");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn adapter_dispatches_fs_read_denied_under_deny() {
        let result = adapter(HostPolicy::Deny)
            .handle("fs/read_text_file", Some(json!({"path": "/etc/passwd"})));
        assert_eq!(result.unwrap_err().code, RpcError::PERMISSION_DENIED);
    }

    #[test]
    fn adapter_dispatches_permission_request() {
        let result = adapter(HostPolicy::Yolo).handle(
            "session/request_permission",
            Some(json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "tc1", "kind": "execute"},
                "options": [
                    {"optionId": "ao", "name": "Allow once", "kind": "allow_once"}
                ]
            })),
        );
        let value = result.unwrap();
        assert_eq!(value["outcome"], "selected");
        assert_eq!(value["optionId"], "ao");
    }

    #[test]
    fn adapter_unknown_method_returns_method_not_found() {
        let result = adapter(HostPolicy::Auto).handle("unknown/method", None);
        assert_eq!(result.unwrap_err().code, RpcError::METHOD_NOT_FOUND);
    }

    #[test]
    fn adapter_invalid_params_returns_invalid_params_error() {
        // terminal/create requires a "command" field.
        let result = adapter(HostPolicy::Auto).handle("terminal/create", Some(json!({})));
        assert_eq!(result.unwrap_err().code, RpcError::INVALID_PARAMS);
    }

    #[test]
    fn adapter_terminal_lifecycle_end_to_end() {
        let handler = adapter(HostPolicy::Auto);
        #[cfg(windows)]
        let create_params = json!({"command": "cmd", "args": ["/C", "echo lifecycle"]});
        #[cfg(not(windows))]
        let create_params = json!({"command": "sh", "args": ["-c", "echo lifecycle"]});

        let created = handler.handle("terminal/create", Some(create_params)).unwrap();
        let terminal_id = created["terminalId"].as_str().unwrap().to_owned();

        let waited = handler
            .handle("terminal/wait_for_exit", Some(json!({"terminalId": terminal_id})))
            .unwrap();
        assert_eq!(waited["exitCode"], 0);

        let output = handler
            .handle("terminal/output", Some(json!({"terminalId": terminal_id})))
            .unwrap();
        assert!(output["output"].as_str().unwrap().contains("lifecycle"));

        handler
            .handle("terminal/release", Some(json!({"terminalId": terminal_id})))
            .unwrap();

        let after_release =
            handler.handle("terminal/output", Some(json!({"terminalId": terminal_id})));
        assert_eq!(after_release.unwrap_err().code, RpcError::NOT_FOUND);
    }

    #[test]
    fn adapter_terminal_create_denied_under_read_only() {
        let result = adapter(HostPolicy::ReadOnly)
            .handle("terminal/create", Some(json!({"command": "echo", "args": []})));
        assert_eq!(result.unwrap_err().code, RpcError::PERMISSION_DENIED);
    }

    // -----------------------------------------------------------------------
    // Dangerous-command gate — stands above `HostPolicy`
    //
    // Every case here uses a command that is dangerous BY CONSTRUCTION
    // (`rm -rf /`) but never lets it reach `TerminalStore::create` — the
    // gate check returns `Err` before that call. Tests that need to prove
    // the gate was NOT consulted (the `Disabled` cases) pair it with
    // `HostPolicy::Deny`, whose OWN refusal keeps the test safe even if the
    // gate really were skipped, while the assertion tells the two refusals
    // apart by message.
    // -----------------------------------------------------------------------

    fn dangerous_terminal_params() -> TerminalCreateParams {
        TerminalCreateParams {
            session_id: String::new(),
            command: "rm".to_owned(),
            args: vec!["-rf".to_owned(), "/".to_owned()],
            env: vec![],
            cwd: None,
            output_byte_limit: None,
        }
    }

    fn dangerous_execute_tool_call() -> PermissionToolCall {
        PermissionToolCall {
            tool_call_id: "tc1".to_owned(),
            title: "Run rm -rf /".to_owned(),
            kind: crate::acp::protocol::ToolKind::Execute,
            locations: vec![],
            raw_input: json!({"command": "rm", "args": ["-rf", "/"]}),
        }
    }

    #[test]
    fn dangerous_command_gate_blocks_terminal_create_even_under_yolo_policy() {
        let handler =
            PolicyHostHandler::new(HostPolicy::Yolo, std::env::temp_dir(), DangerousCommandGate::Enforced);
        let message = handler.terminal_create(&dangerous_terminal_params()).unwrap_err();
        assert!(message.contains("dangerous-command gate"), "message was: {message}");
        assert!(message.contains("filesystem-wipe"), "message was: {message}");
        assert!(message.contains("rule="), "refusal must name the rule: {message}");
        assert!(message.contains("argument="), "refusal must name the offending argument: {message}");
    }

    #[test]
    fn dangerous_command_gate_disabled_skips_straight_to_policy_for_terminal_create() {
        let handler =
            PolicyHostHandler::new(HostPolicy::Deny, std::env::temp_dir(), DangerousCommandGate::Disabled);
        let message = handler.terminal_create(&dangerous_terminal_params()).unwrap_err();
        // The POLICY's refusal text, not the gate's -- proves the gate
        // itself never ran when explicitly disabled.
        assert_eq!(message, "terminal/create denied by host policy");
    }

    #[test]
    fn dangerous_command_gate_blocks_execute_permission_request_even_under_yolo_policy() {
        let handler =
            PolicyHostHandler::new(HostPolicy::Yolo, std::env::temp_dir(), DangerousCommandGate::Enforced);
        let params = PermissionRequestParams {
            session_id: "s1".to_owned(),
            tool_call: dangerous_execute_tool_call(),
            options: options_all_four(),
        };
        // Yolo alone would select allow_always ("aa") -- the gate must
        // override that and land on a reject-kind option instead.
        let outcome = handler.request_permission(&params);
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "ro".to_owned() });
    }

    #[test]
    fn dangerous_command_gate_disabled_leaves_execute_permission_request_to_policy() {
        let handler =
            PolicyHostHandler::new(HostPolicy::Yolo, std::env::temp_dir(), DangerousCommandGate::Disabled);
        let params = PermissionRequestParams {
            session_id: "s1".to_owned(),
            tool_call: dangerous_execute_tool_call(),
            options: options_all_four(),
        };
        let outcome = handler.request_permission(&params);
        assert_eq!(outcome, PermissionOutcome::Selected { option_id: "aa".to_owned() });
    }

    #[test]
    fn dangerous_command_gate_allows_a_benign_terminal_create_under_yolo() {
        let handler =
            PolicyHostHandler::new(HostPolicy::Yolo, std::env::temp_dir(), DangerousCommandGate::Enforced);
        let params = TerminalCreateParams {
            session_id: String::new(),
            command: "echo".to_owned(),
            args: vec!["hello".to_owned()],
            env: vec![],
            cwd: None,
            output_byte_limit: None,
        };
        // Reaches TerminalStore -- a real (harmless) process is fine here.
        assert!(handler.terminal_create(&params).is_ok());
    }

    #[test]
    fn adapter_dispatches_terminal_create_blocked_by_gate_as_permission_denied() {
        let result = adapter_with_gate(HostPolicy::Auto, DangerousCommandGate::Enforced)
            .handle("terminal/create", Some(json!({"command": "rm", "args": ["-rf", "/"]})));
        let error = result.unwrap_err();
        assert_eq!(error.code, RpcError::PERMISSION_DENIED);
        assert!(error.message.contains("filesystem-wipe"), "message was: {}", error.message);
    }

    // -----------------------------------------------------------------------
    // handle_deferrable — PermissionDeferral
    // -----------------------------------------------------------------------

    #[test]
    fn handle_deferrable_never_defers_when_disabled() {
        let handler = adapter(HostPolicy::Auto);
        let outcome = handler.handle_deferrable(
            "session/request_permission",
            Some(json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "tc1", "kind": "read"},
                "options": []
            })),
        );
        assert!(matches!(outcome, HostOutcome::Immediate(_)));
    }

    #[test]
    fn handle_deferrable_never_defers_methods_other_than_request_permission() {
        let handler =
            adapter_with_deferral(HostPolicy::Auto, DangerousCommandGate::Enforced, PermissionDeferral::Enabled);
        let outcome = handler.handle_deferrable("fs/read_text_file", Some(json!({"path": "/etc/passwd"})));
        assert!(matches!(outcome, HostOutcome::Immediate(_)));
    }

    #[test]
    fn handle_deferrable_defers_a_non_blocked_permission_request_when_enabled() {
        let handler =
            adapter_with_deferral(HostPolicy::Auto, DangerousCommandGate::Enforced, PermissionDeferral::Enabled);
        let outcome = handler.handle_deferrable(
            "session/request_permission",
            Some(json!({
                "sessionId": "s1",
                "toolCall": {"toolCallId": "tc1", "kind": "read"},
                "options": [{"optionId": "ao", "name": "Allow once", "kind": "allow_once"}]
            })),
        );
        assert!(matches!(outcome, HostOutcome::Deferred));
    }

    #[test]
    fn handle_deferrable_decides_a_gate_blocked_request_immediately_even_when_enabled() {
        let handler =
            adapter_with_deferral(HostPolicy::Yolo, DangerousCommandGate::Enforced, PermissionDeferral::Enabled);
        let params = PermissionRequestParams {
            session_id: "s1".to_owned(),
            tool_call: dangerous_execute_tool_call(),
            options: options_all_four(),
        };
        let outcome = handler.handle_deferrable(
            "session/request_permission",
            Some(serde_json::to_value(&params).expect("PermissionRequestParams serializes")),
        );
        match outcome {
            HostOutcome::Immediate(Ok(value)) => {
                assert_eq!(value["outcome"], "selected");
                // Yolo alone would pick "aa" (allow_always) -- the gate
                // overrides that to a reject-kind option, exactly as
                // `dangerous_command_gate_blocks_execute_permission_request_even_under_yolo_policy` proves for `request_permission` directly.
                assert_eq!(value["optionId"], "ro");
            }
            other => panic!("expected an immediate gate-blocked decision, got {other:?}"),
        }
    }

    #[test]
    fn handle_deferrable_falls_through_to_handle_on_unparsable_params() {
        let handler =
            adapter_with_deferral(HostPolicy::Auto, DangerousCommandGate::Enforced, PermissionDeferral::Enabled);
        let outcome = handler.handle_deferrable("session/request_permission", Some(json!("not an object")));
        match outcome {
            HostOutcome::Immediate(Err(err)) => assert_eq!(err.code, RpcError::INVALID_PARAMS),
            other => panic!("expected an immediate INVALID_PARAMS error, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // decision_authority — classifies WHO decided, for HostRequestDecision
    // -----------------------------------------------------------------------

    #[test]
    fn decision_authority_is_gate_for_a_gate_blocked_terminal_create() {
        let handler = adapter_with_gate(HostPolicy::Yolo, DangerousCommandGate::Enforced);
        let params = json!({"command": "rm", "args": ["-rf", "/"]});
        assert_eq!(
            handler.decision_authority("terminal/create", Some(&params)),
            HostDecisionAuthority::Gate
        );
    }

    #[test]
    fn decision_authority_is_policy_for_a_benign_terminal_create() {
        let handler = adapter_with_gate(HostPolicy::Yolo, DangerousCommandGate::Enforced);
        let params = json!({"command": "echo", "args": ["hello"]});
        assert_eq!(
            handler.decision_authority("terminal/create", Some(&params)),
            HostDecisionAuthority::Policy
        );
    }

    #[test]
    fn decision_authority_is_policy_for_terminal_create_when_gate_disabled() {
        let handler = adapter_with_gate(HostPolicy::Yolo, DangerousCommandGate::Disabled);
        let params = json!({"command": "rm", "args": ["-rf", "/"]});
        assert_eq!(
            handler.decision_authority("terminal/create", Some(&params)),
            HostDecisionAuthority::Policy
        );
    }

    #[test]
    fn decision_authority_is_gate_for_a_gate_blocked_permission_request() {
        let handler = adapter_with_gate(HostPolicy::Yolo, DangerousCommandGate::Enforced);
        let params = PermissionRequestParams {
            session_id: "s1".to_owned(),
            tool_call: dangerous_execute_tool_call(),
            options: options_all_four(),
        };
        let value = serde_json::to_value(&params).expect("PermissionRequestParams serializes");
        assert_eq!(
            handler.decision_authority("session/request_permission", Some(&value)),
            HostDecisionAuthority::Gate
        );
    }

    #[test]
    fn decision_authority_is_policy_for_a_non_blocked_permission_request() {
        let handler = adapter_with_gate(HostPolicy::Yolo, DangerousCommandGate::Enforced);
        let params = PermissionRequestParams {
            session_id: "s1".to_owned(),
            tool_call: tool_call(crate::acp::protocol::ToolKind::Read),
            options: options_all_four(),
        };
        let value = serde_json::to_value(&params).expect("PermissionRequestParams serializes");
        assert_eq!(
            handler.decision_authority("session/request_permission", Some(&value)),
            HostDecisionAuthority::Policy
        );
    }

    #[test]
    fn decision_authority_is_policy_for_every_other_method() {
        let handler = adapter_with_gate(HostPolicy::Yolo, DangerousCommandGate::Enforced);
        for method in [
            "fs/read_text_file",
            "fs/write_text_file",
            "terminal/output",
            "terminal/wait_for_exit",
            "terminal/kill",
            "terminal/release",
        ] {
            assert_eq!(
                handler.decision_authority(method, None),
                HostDecisionAuthority::Policy,
                "method was: {method}"
            );
        }
    }
}
