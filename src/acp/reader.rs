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
use crate::rpc::message::{
    classify_line, IncomingMessage, RpcResponse,
};
use crate::rpc::pending::PendingRequests;

use super::protocol::{update_to_event, SessionUpdateParams};
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

                let response = match result {
                    Ok(val) => RpcResponse::success(id.clone(), val),
                    Err(err) => RpcResponse::error_response(id.clone(), err),
                };

                if let Ok(json) = serde_json::to_string(&response) {
                    write_line_to_process(&process, &format!("{}\n", json));
                }

                // Broadcast so observers can audit agent → host calls.
                let _ = tx.send(AgentEvent::RpcIncomingRequest { id, method, params });
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

    // Cancel all in-flight host → agent requests so callers don't hang.
    pending.cancel_all("acp session closed");
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

/// Collect the child process exit code. Falls back to 0 on any error.
fn collect_exit_code(process: &Arc<Mutex<AcpProcess>>) -> i32 {
    process
        .lock()
        .ok()
        .map(|mut g| g.exit_code())
        .unwrap_or(0)
}
