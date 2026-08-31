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

use crate::rpc::handler::HostHandler;
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
    fn select_permission_option(
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
        for kind in preference {
            if let Some(option) = options.iter().find(|option| option.kind == *kind) {
                return PermissionOutcome::Selected { option_id: option.option_id.clone() };
            }
        }
        PermissionOutcome::Cancelled
    }
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
        if self.dangerous_command_gate == DangerousCommandGate::Enforced {
            let cwd: PathBuf = params
                .cwd
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.working_dir.clone());
            let verdict = gate::evaluate_command(&params.command, &params.args, &cwd);
            if let Some(refusal) = verdict.refusal_message() {
                return Err(format!("terminal/create {refusal}"));
            }
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
        // Same gate, same "above the policy" placement as `terminal_create`.
        // A `Block` verdict here has no wire field to carry its reason on
        // (`PermissionOutcome` is `Selected`/`Cancelled`, not an error), so
        // it is answered the same way `HostPolicy::Deny` answers any
        // execute-kind request: prefer a reject-kind option the agent
        // offered, else decline to pick any of them.
        if self.dangerous_command_gate == DangerousCommandGate::Enforced
            && params.tool_call.kind == ToolKind::Execute
            && gate::evaluate_permission_tool_call(&params.tool_call, &self.working_dir).is_blocked()
        {
            return HostPolicy::Deny.select_permission_option(&params.tool_call, &params.options);
        }
        self.policy.select_permission_option(&params.tool_call, &params.options)
    }
}

// ---------------------------------------------------------------------------
// AcpHostAdapter — bridges AcpHostHandler → HostHandler
// ---------------------------------------------------------------------------

/// Bridges [`AcpHostHandler`] → [`HostHandler`] for use in the reader loop.
///
/// Wraps `Arc<dyn AcpHostHandler>` so it can be cloned cheaply without
/// requiring `'static + Clone` bounds on the trait.
pub(crate) struct AcpHostAdapter(pub std::sync::Arc<dyn AcpHostHandler>);

impl HostHandler for AcpHostAdapter {
    fn handle(&self, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
        match method {
            "fs/read_text_file" => {
                let p: FsReadParams = parse_params(params)?;
                self.0
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
                self.0
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
                self.0
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
                self.0
                    .terminal_output(&p)
                    .map(|result| serde_json::to_value(result).unwrap_or(Value::Null))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "terminal/wait_for_exit" => {
                let p: TerminalIdParams = parse_params(params)?;
                self.0
                    .terminal_wait_for_exit(&p)
                    .map(|result| serde_json::to_value(result).unwrap_or(Value::Null))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "terminal/kill" => {
                let p: TerminalIdParams = parse_params(params)?;
                self.0
                    .terminal_kill(&p)
                    .map(|()| json!({}))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "terminal/release" => {
                let p: TerminalIdParams = parse_params(params)?;
                self.0
                    .terminal_release(&p)
                    .map(|()| json!({}))
                    .map_err(|msg| RpcError { code: RpcError::NOT_FOUND, message: msg, data: None })
            }

            "session/request_permission" => {
                let p: PermissionRequestParams = parse_params(params)?;
                let outcome = self.0.request_permission(&p);
                Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
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
        AcpHostAdapter(Arc::new(PolicyHostHandler::new(
            policy,
            std::env::temp_dir(),
            DangerousCommandGate::Enforced,
        )))
    }

    fn adapter_with_gate(policy: HostPolicy, gate: DangerousCommandGate) -> AcpHostAdapter {
        AcpHostAdapter(Arc::new(PolicyHostHandler::new(policy, std::env::temp_dir(), gate)))
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
}
