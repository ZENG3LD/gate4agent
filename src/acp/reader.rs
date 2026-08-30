//! Blocking reader loop for the ACP transport.
//!
//! Runs on a `spawn_blocking` thread. Polls `AcpProcess::try_recv()` for
//! stdout lines, classifies each as JSON-RPC request / response / notification,
//! and dispatches accordingly:
//!
//! - **Request** (agent → host): calls `HostHandler::handle`, writes response
//!   back via `AcpProcess::write_line`, broadcasts `RpcIncomingRequest`.
//! - **Response** (agent → host reply): resolves in `PendingRequests`.
//! - **Notification**: maps `session/update` subtypes to `AgentEvent` via
//!   `protocol::update_to_event`; unknown notifications become
//!   `AgentEvent::RpcNotification`.
//! - **Legacy / non-JSON line**: discarded silently (ACP is pure JSON-RPC 2.0).
//! - **Process exit**: cancels all pending requests, emits `SessionEnd` (if
//!   not yet received via `session_complete`) + `Exited`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;

use crate::core::types::AgentEvent;
use crate::rpc::handler::HostHandler;
use crate::rpc::message::{classify_line, IncomingMessage, RpcError, RpcResponse};
use crate::rpc::pending::PendingRequests;

use super::protocol::{
    update_to_event, PermissionOptionKind, PermissionRequestParams, SessionUpdateParams,
};
use super::spawn::AcpProcess;

/// Reader loop for ACP transport — runs on a `spawn_blocking` thread.
///
/// The loop polls `AcpProcess::try_recv()` (non-blocking) every 10 ms when
/// no line is available, checking `is_running()` to detect process exit.
/// This mirrors the pattern in `rpc/session.rs::rpc_reader_loop`.
pub(crate) fn acp_reader_loop(
    process: Arc<Mutex<AcpProcess>>,
    tx: broadcast::Sender<AgentEvent>,
    pending: PendingRequests,
    handler: Arc<dyn HostHandler>,
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

        match classify_line(trimmed) {
            IncomingMessage::Request { id, method, params } => {
                // Call the host handler (synchronously on this thread — must
                // not block for long). Do NOT hold `process` mutex during this
                // call to avoid deadlock with `write_line`.
                let result = handler.handle(&method, params.clone());
                let granted = request_granted(&method, params.as_ref(), &result);

                let response = match result {
                    Ok(val) => RpcResponse::success(id.clone(), val),
                    Err(err) => RpcResponse::error_response(id.clone(), err),
                };

                if let Ok(json) = serde_json::to_string(&response) {
                    write_line_to_process(&process, &format!("{}\n", json));
                }

                // Broadcast so observers can audit agent → host calls.
                let _ = tx.send(AgentEvent::RpcIncomingRequest {
                    id,
                    method,
                    params,
                    granted,
                });
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
fn write_line_to_process(process: &Arc<Mutex<AcpProcess>>, line: &str) {
    if let Ok(mut guard) = process.lock() {
        let _ = guard.write_line(line.trim_end_matches('\n'));
    }
}

/// Read the host's actual decision off the same `Result` the reader loop
/// already computed by calling `HostHandler::handle` -- see
/// `AgentEvent::RpcIncomingRequest::granted` for the full rationale.
///
/// For every method except `session/request_permission`, `Err` means
/// denied and `Ok` means granted (the whole story: `fs/read_text_file` and
/// `terminal/create` model "no" as an RPC error). `session/request_
/// permission` is different -- ACP models a decline as a normal `outcome`
/// response, not an RPC error, and the host may have deliberately selected
/// a `reject_once`/`reject_always` option rather than declining to answer
/// at all (`Cancelled`). So for that one method, `granted` is recovered by
/// looking the chosen `optionId` back up in the ORIGINAL request's
/// `options` list (still available here as `params`) and reading its
/// `kind` -- an `outcome` of `selected` alone does not mean granted, since
/// a `Deny` policy answers by selecting a reject-kind option, not by
/// erroring.
fn request_granted(method: &str, params: Option<&Value>, result: &Result<Value, RpcError>) -> bool {
    let Ok(outcome) = result else { return false };
    if method != "session/request_permission" {
        return true;
    }
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
}
