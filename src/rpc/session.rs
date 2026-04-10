//! Bidirectional JSON-RPC 2.0 session over a pipe subprocess.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::core::error::AgentError;
use crate::core::types::{AgentEvent, CliTool};
use crate::pipe::cli::{create_ndjson_parser, CliEvent};
use crate::pipe::process::{PipeProcess, PipeProcessOptions};

use super::handler::{HostHandler, RejectAllHandler};
use super::id::IdGen;
use super::message::{
    classify_line, IncomingMessage, RpcError, RpcId, RpcNotification, RpcRequest, RpcResponse,
};
use super::pending::PendingRequests;

/// Error variants specific to the RPC session.
#[derive(Debug, thiserror::Error)]
pub enum RpcSessionError {
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

    #[error("JSON serialization failed: {source}")]
    Json {
        #[source]
        source: serde_json::Error,
    },

    #[error("Request timed out (method={method})")]
    Timeout { id: RpcId, method: String },

    #[error("Session closed while awaiting response")]
    SessionClosed,

    #[error("Agent returned RPC error: {0}")]
    Agent(#[from] RpcError),
}

/// Options for constructing an [`RpcSession`].
pub struct RpcSessionOptions {
    /// Handler for agent → host requests.
    ///
    /// Defaults to [`RejectAllHandler`] which rejects all requests with
    /// `METHOD_NOT_FOUND` (-32601).
    pub host_handler: Option<Box<dyn HostHandler>>,

    /// When `true`, non-JSON-RPC lines are forwarded to the per-CLI NDJSON
    /// parser and emitted as [`AgentEvent`] on the broadcast channel.
    ///
    /// When `false`, non-JSON-RPC lines are silently dropped.
    ///
    /// Default: `true`.
    pub legacy_fallback: bool,

    /// Broadcast channel capacity. Default: 256.
    pub channel_capacity: usize,
}

impl Default for RpcSessionOptions {
    fn default() -> Self {
        Self {
            host_handler: None,
            legacy_fallback: true,
            channel_capacity: 256,
        }
    }
}

/// A bidirectional JSON-RPC 2.0 session over a pipe subprocess.
///
/// Unlike [`PipeSession`](crate::pipe::PipeSession) (which is a read-only
/// event broadcast), `RpcSession` supports:
///
/// 1. **Receiving agent → host requests** and dispatching them to
///    [`HostHandler`].
/// 2. **Sending host → agent requests** and awaiting typed responses via
///    [`rpc_call`](RpcSession::rpc_call).
/// 3. **Sending notifications** (one-way, no response) via
///    [`notify`](RpcSession::notify).
/// 4. **Broadcasting all events** (RPC notifications and legacy NDJSON) as
///    [`AgentEvent`] to any number of subscribers via
///    [`subscribe`](RpcSession::subscribe).
///
/// # Thread model
///
/// The reader loop runs on a `spawn_blocking` thread (same as `PipeSession`).
/// `rpc_call` is fully async — it serializes and writes the request then
/// awaits a `oneshot::Receiver` for the agent's response.
pub struct RpcSession {
    session_id: String,
    tool: CliTool,
    tx: broadcast::Sender<AgentEvent>,
    /// Shared pipe handle (also used by reader loop for writing responses).
    stdin: Arc<Mutex<Option<PipeProcess>>>,
    pending: PendingRequests,
    id_gen: Arc<IdGen>,
    reader_task: JoinHandle<()>,
}

impl RpcSession {
    /// Spawn a subprocess and start the bidirectional RPC reader loop.
    ///
    /// # Errors
    ///
    /// Returns [`RpcSessionError::Spawn`] if the child process fails to start.
    pub async fn spawn(
        tool: CliTool,
        options: PipeProcessOptions,
        rpc_opts: RpcSessionOptions,
        working_dir: &std::path::Path,
        initial_prompt: &str,
    ) -> Result<Self, RpcSessionError> {
        let pipe = PipeProcess::new_with_options(tool, working_dir, initial_prompt, options)
            .map_err(|e| RpcSessionError::Spawn { source: e })?;

        let session_id = generate_session_id();
        let (tx, _) = broadcast::channel::<AgentEvent>(rpc_opts.channel_capacity);

        let _ = tx.send(AgentEvent::Started {
            session_id: session_id.clone(),
        });

        let stdin = Arc::new(Mutex::new(Some(pipe)));

        let handler: Arc<dyn HostHandler> = match rpc_opts.host_handler {
            Some(h) => Arc::from(h),
            None => Arc::new(RejectAllHandler),
        };

        let pending = PendingRequests::new();
        let id_gen = Arc::new(IdGen::new());

        // Clones for the reader loop.
        let reader_pipe = Arc::clone(&stdin);
        let reader_tx = tx.clone();
        let reader_pending = pending.clone();
        let legacy_fallback = rpc_opts.legacy_fallback;

        let reader_task = tokio::task::spawn_blocking(move || {
            rpc_reader_loop(reader_pipe, reader_tx, reader_pending, handler, tool, legacy_fallback);
        });

        Ok(Self {
            session_id,
            tool,
            tx,
            stdin,
            pending,
            id_gen,
            reader_task,
        })
    }

    /// Subscribe to receive all future [`AgentEvent`] values from this session.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Session ID assigned at spawn time.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// CLI tool type.
    pub fn tool(&self) -> CliTool {
        self.tool
    }

    /// Send a JSON-RPC notification to the agent (no response expected).
    ///
    /// Suitable for: follow-up prompts in non-ACP pipe mode, `session/cancel`.
    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<(), RpcSessionError> {
        let notif = RpcNotification {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        };
        let line = serde_json::to_string(&notif).map_err(|e| RpcSessionError::Json { source: e })?;
        self.write_line(line).await
    }

    /// Send a JSON-RPC request to the agent and await its response.
    ///
    /// Returns `Ok(Value)` on success, `Err(RpcSessionError::Agent)` if the
    /// agent returned a JSON-RPC error response, or `Err(RpcSessionError::Timeout)`
    /// if no response arrived within `timeout`.
    pub async fn rpc_call(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, RpcSessionError> {
        let id = self.id_gen.next();
        let rx = self.pending.register(id.clone());

        let request = RpcRequest::new(id.clone(), method, params);
        let line = serde_json::to_string(&request)
            .map_err(|e| RpcSessionError::Json { source: e })?;
        self.write_line(line).await?;

        tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| RpcSessionError::Timeout {
                id,
                method: method.to_owned(),
            })?
            .map_err(|_| RpcSessionError::SessionClosed)?
            .map_err(RpcSessionError::Agent)
    }

    /// Send raw text to stdin (legacy compatibility — for non-RPC CLIs).
    pub async fn send_raw(&self, text: &str) -> Result<(), RpcSessionError> {
        self.write_line(text.to_owned()).await
    }

    /// Kill the subprocess.
    pub async fn kill(&self) -> Result<(), AgentError> {
        self.reader_task.abort();
        let pipe = Arc::clone(&self.stdin);
        tokio::task::spawn_blocking(move || {
            let mut guard = pipe
                .lock()
                .map_err(|_| AgentError::Pty("pipe mutex poisoned".into()))?;
            if let Some(ref mut p) = *guard {
                p.kill().map_err(|e| AgentError::Spawn { source: e })?;
            }
            Ok::<(), AgentError>(())
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }

    // ---------------------------------------------------------------------------
    // Private helpers
    // ---------------------------------------------------------------------------

    /// Write a line (with trailing newline) to the pipe stdin on a blocking thread.
    async fn write_line(&self, line: String) -> Result<(), RpcSessionError> {
        let pipe = Arc::clone(&self.stdin);
        tokio::task::spawn_blocking(move || {
            let mut guard = pipe
                .lock()
                .map_err(|_| RpcSessionError::Write {
                    source: std::io::Error::new(std::io::ErrorKind::Other, "mutex poisoned"),
                })?;
            if let Some(ref mut p) = *guard {
                p.write(&format!("{}\n", line))
                    .map_err(|e| RpcSessionError::Write { source: e })?;
            }
            Ok::<(), RpcSessionError>(())
        })
        .await
        .map_err(|_| RpcSessionError::Write {
            source: std::io::Error::new(std::io::ErrorKind::Other, "spawn_blocking panicked"),
        })?
    }
}

// ---------------------------------------------------------------------------
// Reader loop (runs on blocking thread via spawn_blocking)
// ---------------------------------------------------------------------------

fn rpc_reader_loop(
    pipe: Arc<Mutex<Option<PipeProcess>>>,
    tx: broadcast::Sender<AgentEvent>,
    pending: PendingRequests,
    handler: Arc<dyn HostHandler>,
    tool: CliTool,
    legacy_fallback: bool,
) {
    let mut ndjson_parser = if legacy_fallback {
        Some(create_ndjson_parser(tool))
    } else {
        None
    };

    let mut parser_emitted_session_end = false;

    loop {
        // Non-blocking recv attempt.
        let line = {
            match pipe.lock() {
                Ok(guard) => {
                    if let Some(ref p) = *guard {
                        p.try_recv()
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        };

        let line = match line {
            Some(l) => l,
            None => {
                // No line available — check if process is still alive.
                let still_running = pipe
                    .lock()
                    .ok()
                    .and_then(|mut g| g.as_mut().map(|p| p.is_running()))
                    .unwrap_or(false);
                if !still_running {
                    let exit_code = get_exit_code(&pipe);
                    if !parser_emitted_session_end {
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
                // Agent → host: call the handler (synchronously, on this thread),
                // then write the response back. We must NOT hold the pipe mutex
                // during handler execution to avoid deadlock.
                let result = handler.handle(&method, params.clone());

                let response = match result {
                    Ok(val) => RpcResponse::success(id.clone(), val),
                    Err(err) => RpcResponse::error_response(id.clone(), err),
                };

                if let Ok(json) = serde_json::to_string(&response) {
                    write_to_pipe(&pipe, &format!("{}\n", json));
                }

                // Broadcast so observers can audit agent→host calls.
                let _ = tx.send(AgentEvent::RpcIncomingRequest {
                    id: id.clone(),
                    method,
                    params,
                });
            }

            IncomingMessage::Response { id, result, error } => {
                // Agent responded to one of our pending host→agent requests.
                let rpc_result = match error {
                    Some(e) => Err(e),
                    None => Ok(result.unwrap_or(Value::Null)),
                };
                // Silently ignore stale/unsolicited responses.
                let _ = pending.resolve(id, rpc_result);
            }

            IncomingMessage::Notification { method, params } => {
                // Map known ACP notifications to structured AgentEvent variants;
                // unknown notifications go through as RpcNotification passthrough.
                let event = notification_to_event(method, params);
                let _ = tx.send(event);
            }

            IncomingMessage::Legacy(raw) => {
                // Feed to per-CLI NDJSON parser if legacy fallback is enabled.
                if let Some(ref mut parser) = ndjson_parser {
                    let events = parser.parse_line(&raw);
                    for ev in events {
                        if matches!(ev, CliEvent::SessionEnd { .. }) {
                            parser_emitted_session_end = true;
                        }
                        let _ = tx.send(map_cli_event(ev));
                    }
                }
            }
        }
    }

    // Cancel all pending requests so rpc_call futures don't hang.
    pending.cancel_all("session closed");
}

// ---------------------------------------------------------------------------
// Notification → AgentEvent mapping
// ---------------------------------------------------------------------------

/// Convert an incoming JSON-RPC notification to an [`AgentEvent`].
///
/// Maps well-known ACP `session/update` subtypes to structured events.
/// Unknown notifications are emitted as `AgentEvent::RpcNotification`.
fn notification_to_event(method: String, params: Option<Value>) -> AgentEvent {
    if method == "session/update" {
        if let Some(ref p) = params {
            let update_type = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match update_type {
                "agent_message_chunk" => {
                    let text = p
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    return AgentEvent::Text { text, is_delta: true };
                }
                "agent_thought_chunk" => {
                    let text = p
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    return AgentEvent::Thinking { text };
                }
                "tool_call" => {
                    let id = p
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let name = p
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let input = p.get("input").cloned().unwrap_or(Value::Null);
                    return AgentEvent::ToolStart { id, name, input };
                }
                "tool_call_update" => {
                    let id = p
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let output = p
                        .get("output")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let is_error = p
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let duration_ms = p.get("duration_ms").and_then(|v| v.as_u64());
                    return AgentEvent::ToolResult {
                        id,
                        output,
                        is_error,
                        duration_ms,
                    };
                }
                _ => {}
            }
        }
    }

    // Generic passthrough for all other notifications.
    AgentEvent::RpcNotification {
        method,
        params: params.unwrap_or(Value::Null),
    }
}

// ---------------------------------------------------------------------------
// CliEvent → AgentEvent (reused from PipeSession; kept local to avoid coupling)
// ---------------------------------------------------------------------------

fn map_cli_event(event: CliEvent) -> AgentEvent {
    match event {
        CliEvent::SessionStart {
            session_id,
            model,
            tools,
        } => AgentEvent::SessionStart {
            session_id,
            model,
            tools,
        },
        CliEvent::AssistantText { text, is_delta } => AgentEvent::Text { text, is_delta },
        CliEvent::ToolCallStart { id, name, input } => AgentEvent::ToolStart { id, name, input },
        CliEvent::ToolCallResult {
            id,
            output,
            is_error,
            duration_ms,
        } => AgentEvent::ToolResult {
            id,
            output,
            is_error,
            duration_ms,
        },
        CliEvent::Thinking { text } => AgentEvent::Thinking { text },
        CliEvent::TurnComplete {
            input_tokens,
            output_tokens,
        } => AgentEvent::TurnComplete {
            input_tokens,
            output_tokens,
        },
        CliEvent::SessionEnd {
            result,
            cost_usd,
            is_error,
        } => AgentEvent::SessionEnd {
            result,
            cost_usd,
            is_error,
        },
        CliEvent::Error { message } => AgentEvent::Error { message },
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write a line to the pipe stdin. Silently ignores errors (process may have
/// exited by the time a response is ready).
fn write_to_pipe(pipe: &Arc<Mutex<Option<PipeProcess>>>, line: &str) {
    if let Ok(mut guard) = pipe.lock() {
        if let Some(ref mut p) = *guard {
            let _ = p.write(line);
        }
    }
}

/// Collect child process exit code. Falls back to 0 on any error.
fn get_exit_code(process: &Arc<Mutex<Option<PipeProcess>>>) -> i32 {
    process
        .lock()
        .ok()
        .and_then(|mut g| {
            g.as_mut().and_then(|p| {
                p.wait()
                    .map(|status| status.map(|s| s.code().unwrap_or(0)).unwrap_or(0))
                    .ok()
            })
        })
        .unwrap_or(0)
}

fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("rpc-{:x}", t)
}
