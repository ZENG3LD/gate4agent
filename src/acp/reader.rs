//! Blocking reader loop for the ACP transport.
//!
//! Runs on a `spawn_blocking` thread. Polls `AcpProcess::try_recv()` for
//! stdout lines, classifies each as JSON-RPC request / response / notification,
//! and dispatches accordingly:
//!
//! - **Request** (agent → host): calls `AcpHostAdapter::dispatch_deferrable`.
//!   When it answers `Immediate`, this writes the response back via
//!   `AcpProcess::write_line` and broadcasts `RpcIncomingRequest` carrying a
//!   decided [`crate::core::types::HostRequestDecision`] (`Granted`/
//!   `Denied`) and [`crate::core::types::HostRequestOutcome`] (whether a
//!   `Granted` call actually ran cleanly or failed doing so -- an execution
//!   failure on an authorized call is never reported as `Denied`), exactly
//!   as a plain `HostHandler::handle` call always has for the wire response
//!   itself. When it answers `Deferred` (today, only `session/request_permission` on
//!   a session with deferral enabled -- see `AcpSessionOptions::
//!   defer_permission_requests`), this writes **no** response, broadcasts
//!   `RpcIncomingRequest` with `HostRequestDecision::Deferred` instead, and
//!   records the request in the session's `PendingHostRequests` map
//!   alongside a deadline, then keeps reading — the eventual response is
//!   written later, out of band, by `AcpSession::resolve_pending_request` or
//!   `AcpSession::expire_deadlines`, through the very same
//!   `write_line_to_process` this loop uses, each broadcasting its own
//!   follow-up `RpcIncomingRequest` for the SAME `id` once a decision
//!   actually exists. See §4a of
//!   `docs/gate4agent/plans/gate4agent-acp-control-plane-on-the-wire-2026-09-02.md`
//!   for why this loop must never block waiting on an operator itself.
//! - **Response** (agent → host reply): resolves in `PendingRequests`.
//! - **Notification**: maps `session/update` subtypes to `AgentEvent` via
//!   `protocol::update_to_event`; unknown notifications become
//!   `AgentEvent::RpcNotification`.
//! - **Legacy / non-JSON line**: discarded silently (ACP is pure JSON-RPC 2.0).
//! - **Every non-empty line, regardless of the above**: fires `turn_activity`
//!   (a `tokio::sync::Notify`), the liveness signal `AcpSession::start_prompt`'s
//!   idle watchdog resets its silence window on -- see `acp_reader_loop`'s own
//!   doc comment.
//! - **Process exit**: cancels all pending requests, emits `SessionEnd` (if
//!   not yet received via `session_complete`) + `Exited`. Deferred
//!   `session/request_permission` requests are NOT touched here — they are
//!   host → nothing yet on the wire (no response was ever withheld from a
//!   waiting future the way `pending.cancel_all` wakes), so there is nothing
//!   to cancel; they simply become unreachable once the process is gone,
//!   the same as any other state tied to a dead session.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{broadcast, Notify};

use crate::core::types::{AgentEvent, HostDecisionAuthority, HostRequestDecision, HostRequestOutcome};
use crate::rpc::message::{classify_line, IncomingMessage, RpcError, RpcId, RpcResponse};
use crate::rpc::pending::PendingRequests;

use super::host::{AcpHostAdapter, HostCallOutcome, HostDispatchOutcome};
use super::protocol::{
    apply_session_update, parse_vendor_notification, update_to_event, PermissionOptionKind,
    PermissionOutcome, PermissionRequestParams, SessionState, SessionUpdateParams,
};
use super::session::{PendingHostRequests, PendingPermissionRequest};
use super::spawn::AcpProcess;

/// Reader loop for ACP transport — runs on a `spawn_blocking` thread.
///
/// The loop polls `AcpProcess::try_recv()` (non-blocking) every 10 ms when
/// no line is available, checking `is_running()` to detect process exit.
/// This mirrors the pattern in `rpc/session.rs::rpc_reader_loop`.
///
/// Every non-empty line read from the agent -- whatever it classifies as --
/// also fires `turn_activity`, a liveness signal `AcpSession::start_prompt`'s
/// idle watchdog (`session::run_prompt_watchdog`) resets its silence window
/// on. This loop makes no attempt to scope that signal to "the current
/// turn" specifically: a session has at most one `session/prompt` in flight
/// at a time (`gate4agent-node`'s `TurnInFlight` precondition), so any line
/// arriving while a turn is open is, in practice, that turn's own traffic --
/// a streaming update, a tool-call permission request, or the eventual
/// response -- and firing on every line rather than filtering by method
/// keeps this loop's dispatch unchanged.
pub(crate) fn acp_reader_loop(
    process: Arc<Mutex<AcpProcess>>,
    tx: broadcast::Sender<AgentEvent>,
    pending: PendingRequests,
    handler: Arc<AcpHostAdapter>,
    session_state: Arc<Mutex<SessionState>>,
    pending_host_requests: PendingHostRequests,
    permission_request_deadline: Duration,
    turn_activity: Arc<Notify>,
) {
    let mut received_session_end = false;
    // Error handed to any still-pending request when the loop exits. Only
    // enriched with stderr when the process itself exited (see below) — a
    // reader-side mutex poison keeps the generic message. Starts as a plain
    // internal error; the process-exit branch below may replace it with
    // `RpcError::AUTHENTICATION_REQUIRED` once it recognizes a vendor
    // stderr signature, so the classification survives the trip through
    // `pending.cancel_all()` instead of being flattened to a string first.
    let mut close_error = RpcError::internal("acp session closed");

    loop {
        // Non-blocking line poll — hold the lock for the minimum duration.
        let line = {
            match process.lock() {
                Ok(guard) => guard.try_recv(),
                Err(_) => break, // mutex poisoned
            }
        };

        let line = match line {
            Some(l) => l,
            None => {
                // No line — check if the process is still alive.
                let still_running = process
                    .lock()
                    .ok()
                    .map(|mut g| g.is_running())
                    .unwrap_or(false);

                if !still_running {
                    let exit_code = collect_exit_code(&process);
                    let stderr_tail = process
                        .lock()
                        .ok()
                        .map(|guard| guard.stderr_tail())
                        .unwrap_or_default();
                    let close_reason = if stderr_tail.is_empty() {
                        format!("acp process exited (code={exit_code})")
                    } else {
                        format!(
                            "acp process exited (code={exit_code}); stderr: {}",
                            stderr_tail.join(" | ")
                        )
                    };
                    close_error = match detect_authentication_required(&stderr_tail) {
                        Some(vendor_message) => RpcError {
                            code: RpcError::AUTHENTICATION_REQUIRED,
                            message: close_reason,
                            data: Some(Value::String(vendor_message.to_owned())),
                        },
                        None => RpcError::internal(close_reason),
                    };

                    if !received_session_end {
                        let _ = tx.send(AgentEvent::SessionEnd {
                            result: format!("exit_code={}", exit_code),
                            cost_usd: None,
                            is_error: exit_code != 0,
                            stop_reason: None,
                        });
                    }
                    let _ = tx.send(AgentEvent::Exited { code: exit_code });
                    break;
                }

                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Any well-formed line at all is proof the agent process is still
        // producing output -- see this function's doc comment for why this
        // fires unconditionally rather than only for lines this loop can
        // attribute to a specific in-flight turn.
        turn_activity.notify_one();

        match classify_line(trimmed) {
            IncomingMessage::Request { id, method, params } => {
                // Call the host handler (synchronously on this thread — must
                // not block for long). Do NOT hold `process` mutex during this
                // call to avoid deadlock with `write_line`.
                //
                // `dispatch_deferrable`'s default path for every method
                // except `session/request_permission` under deferral is
                // exactly the old direct `handle` call for the wire
                // response, plus a `HostCallOutcome` computed from the SAME
                // call -- see `acp::host::AcpHostAdapter::dispatch_deferrable`.
                match handler.dispatch_deferrable(&method, params.clone()) {
                    HostDispatchOutcome::Immediate { response: result, outcome: call_outcome } => {
                        let by = handler.decision_authority(&method, params.as_ref());
                        let (decision, host_outcome, reason) = classify_immediate_request(
                            &handler,
                            &method,
                            by,
                            &result,
                            call_outcome,
                            params.as_ref(),
                        );

                        let wire_response = match result {
                            Ok(val) => RpcResponse::success(id.clone(), val),
                            Err(err) => RpcResponse::error_response(id.clone(), err),
                        };

                        if let Ok(json) = serde_json::to_string(&wire_response) {
                            write_line_to_process(&process, &format!("{}\n", json));
                        }

                        // Broadcast so observers can audit agent → host calls.
                        let _ = tx.send(AgentEvent::RpcIncomingRequest {
                            id,
                            method,
                            params,
                            decision,
                            outcome: host_outcome,
                            reason,
                        });
                    }

                    HostDispatchOutcome::Deferred => {
                        defer_request(
                            &process,
                            &tx,
                            &pending_host_requests,
                            permission_request_deadline,
                            id,
                            method,
                            params,
                        );
                    }
                }
            }

            IncomingMessage::Response { id, result, error } => {
                let rpc_result = match error {
                    Some(e) => Err(e),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
                // Silently ignore stale / unsolicited responses.
                let _ = pending.resolve(id, rpc_result);
            }

            IncomingMessage::Notification { method, params } => {
                if method == "session/update" {
                    // Try to parse as typed SessionUpdateParams.
                    if let Some(ref p) = params {
                        if let Ok(sup) = serde_json::from_value::<SessionUpdateParams>(p.clone()) {
                            {
                                let mut state = session_state
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                apply_session_update(&mut state, &sup.update);
                            }
                            let events = update_to_event(&sup);
                            if events.is_empty() {
                                // Unknown update type — pass through as generic notification.
                                let _ = tx.send(AgentEvent::RpcNotification {
                                    method,
                                    params: p.clone(),
                                });
                            } else {
                                for ev in &events {
                                    if matches!(ev, AgentEvent::SessionEnd { .. }) {
                                        received_session_end = true;
                                    }
                                }
                                for ev in events {
                                    let _ = tx.send(ev);
                                }
                            }
                            continue;
                        }
                    }
                } else if let Some(ref p) = params {
                    // Vendor-namespaced notification (currently only
                    // Grok's `_x.ai/*` methods) -- see
                    // `protocol::parse_vendor_notification` for the full
                    // set this build recognizes. Any method it does not
                    // recognize falls through to the generic passthrough
                    // below exactly like an unrecognized `session/update`
                    // kind does.
                    let event = {
                        let mut state =
                            session_state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                        parse_vendor_notification(&mut state, &method, p)
                    };
                    if let Some(event) = event {
                        let _ = tx.send(event);
                        continue;
                    }
                }
                // Generic passthrough for all other notifications.
                let _ = tx.send(AgentEvent::RpcNotification {
                    method,
                    params: params.unwrap_or(Value::Null),
                });
            }

            // ACP is pure JSON-RPC 2.0 — non-JSON lines are discarded.
            IncomingMessage::Legacy(_raw) => {
                // Silently discard. Non-JSON-RPC lines are not expected in
                // ACP mode (no legacy NDJSON fallback).
            }
        }
    }

    // Cancel all in-flight host → agent requests so callers don't hang. When
    // the process exited before answering, `close_error` carries its stderr
    // tail so a failed handshake reports why (e.g. an unauthenticated CLI),
    // not just that the pipe closed -- and, when the stderr matched the
    // recognized authentication signature, `close_error.code` carries that
    // classification too (see `detect_authentication_required`).
    pending.cancel_all(close_error);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a pre-formatted line to the ACP process stdin.
/// Errors are silently ignored — the process may have exited by the time a
/// response is ready (same pattern as `rpc/session.rs::write_to_pipe`).
///
/// `pub(crate)` rather than private: `AcpSession::resolve_pending_request`
/// and `AcpSession::expire_deadlines` (`acp/session.rs`) write a deferred
/// request's eventual response through this exact same path, from outside
/// this loop entirely -- safe to do because this loop never holds `process`
/// locked across the handler call that produced the deferral in the first
/// place (see the module doc comment).
pub(crate) fn write_line_to_process(process: &Arc<Mutex<AcpProcess>>, line: &str) {
    if let Ok(mut guard) = process.lock() {
        let _ = guard.write_line(line.trim_end_matches('\n'));
    }
}

/// Record a `session/request_permission` request the handler chose not to
/// decide yet (`HostDispatchOutcome::Deferred`), and tell an observer that a
/// request arrived without claiming a decision that has not happened.
///
/// `dispatch_deferrable` (`acp/host.rs`) already parsed `params` into
/// [`PermissionRequestParams`] once, to run the dangerous-command gate ahead
/// of the decision to defer -- but per [`HostDispatchOutcome`]'s own
/// contract, `Deferred` carries no payload, so that parsed value does not
/// travel back here. This function parses `params` again for storage. If
/// that reparse ever fails, `dispatch_deferrable`'s own parse must have
/// failed identically (it is the exact same input, the exact same type),
/// which per its logic means it would have returned
/// `Immediate { response: Err(INVALID_PARAMS), .. }`, never `Deferred` -- so
/// this is a defensive branch, not a path this build's own handler can
/// reach. It fails safe: answer `Cancelled` immediately rather than leave
/// the agent waiting on a request nobody actually recorded.
///
/// That fail-safe answer is reported as `Denied { by: Policy }`, not a new
/// authority of its own: it is decided immediately, on this thread, exactly
/// like every other `Policy`-authority decision -- the fact that it hardcodes
/// `Cancelled` rather than consulting `HostPolicy::select_permission_option`
/// is an implementation detail of an unreachable branch, not a distinct
/// decision-making entity an observer needs a fifth bucket to name.
fn defer_request(
    process: &Arc<Mutex<AcpProcess>>,
    tx: &broadcast::Sender<AgentEvent>,
    pending_host_requests: &PendingHostRequests,
    deadline: Duration,
    id: RpcId,
    method: String,
    params: Option<Value>,
) {
    let parsed: Result<PermissionRequestParams, _> =
        serde_json::from_value(params.clone().unwrap_or(Value::Null));

    let typed_params = match parsed {
        Ok(p) => p,
        Err(_) => {
            let value = serde_json::to_value(PermissionOutcome::Cancelled).unwrap_or(Value::Null);
            let response = RpcResponse::success(id.clone(), value);
            if let Ok(json) = serde_json::to_string(&response) {
                write_line_to_process(process, &format!("{}\n", json));
            }
            let decision = HostRequestDecision::Denied { by: HostDecisionAuthority::Policy };
            let _ = tx.send(AgentEvent::RpcIncomingRequest {
                id,
                method,
                params,
                decision,
                // Nothing ever ran -- this is decided before any execution
                // step, same as every other `Denied`.
                outcome: HostRequestOutcome::Executed,
                reason: Some(
                    "session/request_permission request could not be re-parsed for deferral"
                        .to_string(),
                ),
            });
            return;
        }
    };

    pending_host_requests.insert(
        id.clone(),
        PendingPermissionRequest { params: typed_params, deadline: Instant::now() + deadline },
    );

    // Audit that the request ARRIVED, honestly marked `Deferred` -- no
    // decision has been made yet. The real `Granted`/`Denied` decision for
    // THIS request (same `id`) is broadcast later, once one actually exists
    // -- see `AcpSession::resolve_pending_request` and `AcpSession::
    // expire_deadlines` (`acp/session.rs`).
    let _ = tx.send(AgentEvent::RpcIncomingRequest {
        id,
        method,
        params,
        decision: HostRequestDecision::Deferred,
        // Nothing has run yet either -- deferral means no decision exists
        // to authorize an execution step in the first place.
        outcome: HostRequestOutcome::Executed,
        // Nothing has decided this request yet -- there is no reason to
        // report until a later `RpcIncomingRequest` for the same `id`
        // carries the eventual `Granted`/`Denied`.
        reason: None,
    });
}

/// Classify one `HostDispatchOutcome::Immediate` result into the
/// `(decision, outcome, reason)` triple `AgentEvent::RpcIncomingRequest`
/// broadcasts -- the one place that reads `HostCallOutcome::{Denied,
/// Granted, GrantedButFailed}` off the SAME dispatch that already produced
/// the wire `result`, so a `terminal/create` (or `fs/read_text_file`,
/// `fs/write_text_file`, or the other `terminal/*` methods) call that was
/// AUTHORIZED and then failed EXECUTING (a spawn error, a missing file)
/// reads back as `Granted` with `HostRequestOutcome::Failed`, never as
/// `Denied` -- the bug this function exists to close: an `Err` used to mean
/// "refused" unconditionally, minting a policy/gate block for a call the
/// gate and `HostPolicy` never even saw.
///
/// `session/request_permission` is the one exception: it has no execution
/// step to fail (selecting or cancelling an offered option is not I/O), so
/// its `Granted`/`Denied` classification is recovered from the selected
/// `optionId` via [`request_granted`] exactly as before this function
/// existed, ignoring `call_outcome` entirely (`dispatch` answers `Granted`
/// for it unconditionally -- see [`HostCallOutcome`]'s own doc comment,
/// not a claim about what was actually decided).
fn classify_immediate_request(
    handler: &AcpHostAdapter,
    method: &str,
    by: HostDecisionAuthority,
    result: &Result<Value, RpcError>,
    call_outcome: HostCallOutcome,
    params: Option<&Value>,
) -> (HostRequestDecision, HostRequestOutcome, Option<String>) {
    if method == "session/request_permission" {
        let granted = request_granted(method, params, result);
        let decision =
            if granted { HostRequestDecision::Granted { by } } else { HostRequestDecision::Denied { by } };
        let reason = if granted { None } else { handler.permission_refusal_reason(params) };
        return (decision, HostRequestOutcome::Executed, reason);
    }

    match call_outcome {
        HostCallOutcome::Denied => {
            let reason = result.as_ref().err().map(|err| err.message.clone());
            (HostRequestDecision::Denied { by }, HostRequestOutcome::Executed, reason)
        }
        HostCallOutcome::Granted => {
            (HostRequestDecision::Granted { by }, HostRequestOutcome::Executed, None)
        }
        HostCallOutcome::GrantedButFailed { error } => {
            (HostRequestDecision::Granted { by }, HostRequestOutcome::Failed { error }, None)
        }
    }
}

/// Read a `session/request_permission` call's actual decision off the same
/// `Result` the reader loop already computed by dispatching it -- see
/// `AgentEvent::RpcIncomingRequest`'s doc comment for the full rationale.
/// The caller wraps this bool in a [`HostRequestDecision::Granted`] or
/// [`HostRequestDecision::Denied`] alongside the `by` authority from
/// `AcpHostAdapter::decision_authority`.
///
/// Only ever called for `session/request_permission` -- every other method
/// models its refusal as `HostCallOutcome::Denied`/`GrantedButFailed`
/// instead (`acp::host::AcpHostAdapter::dispatch`), read directly by this
/// function's one caller without going through here at all. ACP models a
/// `session/request_permission` decline as a normal `outcome` response, not
/// an RPC error, and the host may have deliberately selected a
/// `reject_once`/`reject_always` option rather than declining to answer at
/// all (`Cancelled`) -- so `granted` is recovered by looking the chosen
/// `optionId` back up in the ORIGINAL request's `options` list (still
/// available here as `params`) and reading its `kind`; an `outcome` of
/// `selected` alone does not mean granted, since a `Deny` policy answers by
/// selecting a reject-kind option, not by erroring. An `Err` here (e.g. a
/// params parse failure) is the one case this method still has no
/// execution-failure concept for -- it stays `false`/not-granted, exactly
/// as before this change.
fn request_granted(method: &str, params: Option<&Value>, result: &Result<Value, RpcError>) -> bool {
    assert_eq!(method, "session/request_permission", "request_granted is only valid for session/request_permission");
    let Ok(outcome) = result else { return false };
    permission_outcome_is_granted(params, outcome)
}

/// See [`request_granted`]. Cross-references the selected `optionId` (if
/// any) against the original request's `options` to recover whether the
/// selection was an allow-kind or reject-kind option.
fn permission_outcome_is_granted(params: Option<&Value>, outcome: &Value) -> bool {
    let Some(option_id) = outcome.get("optionId").and_then(Value::as_str) else {
        return false; // Cancelled, or a malformed outcome -- never granted.
    };
    let Some(params) = params else { return false };
    let Ok(request) = serde_json::from_value::<PermissionRequestParams>(params.clone()) else {
        return false;
    };
    request
        .options
        .iter()
        .find(|option| option.option_id == option_id)
        .is_some_and(|option| {
            matches!(option.kind, PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways)
        })
}

/// Collect the child process exit code. Falls back to 0 on any error.
fn collect_exit_code(process: &Arc<Mutex<AcpProcess>>) -> i32 {
    process
        .lock()
        .ok()
        .map(|mut g| g.exit_code())
        .unwrap_or(0)
}

/// Recognize a "needs authentication" condition in the stderr tail of an ACP
/// process that exited before producing any JSON-RPC line on stdout.
///
/// This is the ONLY signature this function knows -- captured live from an
/// unauthenticated `grok agent stdio` run (xAI's Grok CLI, no `GROK_API_KEY`
/// set, no `~/.grok/user-settings.json` key configured): the process prints
/// nothing on stdout, exits with code 1, and stderr carries exactly this
/// line:
///
/// ```text
/// ❌ Error: API key required. Set GROK_API_KEY environment variable, use
/// --api-key flag, or set "apiKey" field in ~/.grok/user-settings.json
/// ```
///
/// The match is a combination of three stable markers found in that one
/// line -- the `❌ Error:` prefix, the phrase fragment `"API key"`, and the
/// word `"required"` -- rather than the whole sentence, so trailing wording
/// this function does not need (the env var name, the flag, the settings
/// path) can vary without breaking the match. It is deliberately NOT a
/// single generic word like `"error"` or `"key"` alone, which would
/// misclassify unrelated startup failures as an auth prompt.
///
/// No other vendor's unauthenticated-exit text has been observed through
/// this path. Extend the check only from another live capture, never from a
/// guessed phrase -- a false "not recognized" just falls through to the
/// existing generic handshake failure; a false positive would tell an
/// operator to re-authenticate when the real cause was something else.
fn detect_authentication_required(stderr_tail: &[String]) -> Option<&str> {
    stderr_tail.iter().map(String::as_str).find(|line| {
        line.contains("❌ Error:") && line.contains("API key") && line.contains("required")
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim capture from an unauthenticated `grok agent stdio` run (see
    /// `detect_authentication_required`'s doc comment for provenance).
    const GROK_MISSING_KEY_STDERR: &str = "❌ Error: API key required. Set GROK_API_KEY environment variable, use --api-key flag, or set \"apiKey\" field in ~/.grok/user-settings.json";

    #[test]
    fn detects_the_captured_grok_signature() {
        let tail = vec![GROK_MISSING_KEY_STDERR.to_owned()];
        assert_eq!(
            detect_authentication_required(&tail),
            Some(GROK_MISSING_KEY_STDERR)
        );
    }

    #[test]
    fn does_not_flag_an_unrelated_failure() {
        let tail = vec!["Error: connection refused".to_owned()];
        assert_eq!(detect_authentication_required(&tail), None);
    }

    #[test]
    fn empty_stderr_is_not_authentication_required() {
        let tail: Vec<String> = Vec::new();
        assert_eq!(detect_authentication_required(&tail), None);
    }

    #[test]
    fn partial_markers_alone_do_not_match() {
        // Has "required" but neither the error prefix nor "API key" --
        // a combination, not a single word, decides the match.
        let tail = vec!["a value is required here".to_owned()];
        assert_eq!(detect_authentication_required(&tail), None);
    }

    // -----------------------------------------------------------------------
    // classify_immediate_request -- Denied (gate/policy) vs Granted-then-
    // Failed (an authorized call that failed EXECUTING). The measured bug
    // this whole change fixes: an ACP `terminal/create` I/O error (a spawn
    // failure) used to read back as `Denied { by: Gate | Policy }`, minting
    // a policy block that never happened.
    // -----------------------------------------------------------------------

    use crate::acp::gate::DangerousCommandGate;
    use crate::acp::host::{HostPolicy, PermissionDeferral, PolicyHostHandler};
    use serde_json::json;
    use std::sync::Arc;

    fn test_adapter(policy: HostPolicy, gate: DangerousCommandGate) -> AcpHostAdapter {
        AcpHostAdapter::new(
            Arc::new(PolicyHostHandler::new(policy, std::env::temp_dir(), gate)),
            PermissionDeferral::Disabled,
        )
    }

    /// Dispatches `method`/`params` through the exact same path
    /// `acp_reader_loop` uses (`dispatch_deferrable` then
    /// `classify_immediate_request`), for methods that never defer.
    fn classify(
        handler: &AcpHostAdapter,
        method: &str,
        params: Value,
    ) -> (HostRequestDecision, HostRequestOutcome, Option<String>) {
        let (result, call_outcome) = match handler.dispatch_deferrable(method, Some(params.clone())) {
            HostDispatchOutcome::Immediate { response, outcome } => (response, outcome),
            HostDispatchOutcome::Deferred => panic!("{method} is not expected to defer in this test"),
        };
        let by = handler.decision_authority(method, Some(&params));
        classify_immediate_request(handler, method, by, &result, call_outcome, Some(&params))
    }

    /// A `terminal/create` call the gate/policy AUTHORIZED, but whose
    /// `Command::spawn` then failed (the command does not exist) -- must be
    /// `Granted`, with the OS-level failure text on `outcome`, never on
    /// `reason` and never as `Denied`.
    #[test]
    fn terminal_create_spawn_failure_is_granted_and_failed_never_denied() {
        let handler = test_adapter(HostPolicy::Auto, DangerousCommandGate::Enforced);
        let params = json!({
            "command": "g4a-gate-probe-nonexistent-command-zzz",
            "args": [],
        });

        let (decision, outcome, reason) = classify(&handler, "terminal/create", params);

        assert_eq!(decision, HostRequestDecision::Granted { by: HostDecisionAuthority::Policy });
        match outcome {
            HostRequestOutcome::Failed { error } => {
                assert!(!error.is_empty(), "failure text must not be empty");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(reason, None, "a Granted decision must never carry a reason");
    }

    /// The dangerous-command gate stands above `HostPolicy` -- a gate-
    /// blocked `terminal/create` is decided BEFORE anything executes, so it
    /// stays `Denied { by: Gate }`, carrying the gate's own rule-and-
    /// argument text on `reason` -- and that text is never an I/O error,
    /// because nothing ever ran to produce one.
    #[test]
    fn terminal_create_gate_block_is_denied_with_the_gate_text() {
        let handler = test_adapter(HostPolicy::Yolo, DangerousCommandGate::Enforced);
        let params = json!({"command": "rm", "args": ["-rf", "/"]});

        let (decision, outcome, reason) = classify(&handler, "terminal/create", params);

        assert_eq!(decision, HostRequestDecision::Denied { by: HostDecisionAuthority::Gate });
        assert_eq!(outcome, HostRequestOutcome::Executed, "a Denied request never executed anything");
        let reason = reason.expect("a gate block must carry its refusal text");
        assert!(reason.contains("dangerous-command gate"), "reason was: {reason}");
        assert!(reason.contains("filesystem-wipe"), "reason was: {reason}");
        assert!(!reason.to_lowercase().contains("os error"), "reason was: {reason}");
    }

    /// A `terminal/create` call `HostPolicy` refused outright (no gate
    /// involved) -- `Denied { by: Policy }`, same non-I/O refusal text as
    /// today, never `Failed`.
    #[test]
    fn terminal_create_policy_denial_is_denied_never_failed() {
        let handler = test_adapter(HostPolicy::ReadOnly, DangerousCommandGate::Enforced);
        let params = json!({"command": "echo", "args": ["hello"]});

        let (decision, outcome, reason) = classify(&handler, "terminal/create", params);

        assert_eq!(decision, HostRequestDecision::Denied { by: HostDecisionAuthority::Policy });
        assert_eq!(outcome, HostRequestOutcome::Executed);
        assert_eq!(reason, Some("terminal/create denied by host policy".to_owned()));
    }

    /// A `terminal/create` call that is fully authorized and actually runs
    /// -- `Granted`, `Executed`, no reason. The ordinary, unremarkable case.
    #[test]
    fn terminal_create_success_is_granted_and_executed() {
        let handler = test_adapter(HostPolicy::Auto, DangerousCommandGate::Enforced);
        #[cfg(windows)]
        let params = json!({"command": "cmd", "args": ["/C", "echo hi"]});
        #[cfg(not(windows))]
        let params = json!({"command": "sh", "args": ["-c", "echo hi"]});

        let (decision, outcome, reason) = classify(&handler, "terminal/create", params);

        assert_eq!(decision, HostRequestDecision::Granted { by: HostDecisionAuthority::Policy });
        assert_eq!(outcome, HostRequestOutcome::Executed);
        assert_eq!(reason, None);
    }
}
