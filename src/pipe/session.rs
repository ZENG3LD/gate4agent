//! Async pipe session with tokio broadcast fan-out.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::error::AgentError;
use crate::ndjson::{create_ndjson_parser, CliEvent};
use crate::types::{AgentEvent, CliTool, SessionConfig};

use super::process::{PipeProcess, PipeProcessOptions};

/// Async pipe session. Spawns a CLI tool in headless pipe mode and broadcasts
/// NDJSON events as `AgentEvent` to all subscribers via a tokio broadcast channel.
///
/// This is the machine-readable counterpart to `PtySession`. Use this for
/// Telegram bots, web UIs, HTTP SSE, Discord bots, etc.
pub struct PipeSession {
    session_id: String,
    tx: broadcast::Sender<AgentEvent>,
    stdin: Arc<Mutex<Option<PipeProcess>>>,
    reader_task: JoinHandle<()>,
}

impl PipeSession {
    /// Spawn an agent in headless pipe mode and start broadcasting NDJSON events.
    pub async fn spawn(
        config: SessionConfig,
        initial_prompt: &str,
        options: PipeProcessOptions,
    ) -> Result<Self, AgentError> {
        let tool = config.tool;
        let session_id = uuid_v4();

        let pipe = PipeProcess::new_with_options(
            tool,
            &config.working_dir,
            initial_prompt,
            options,
        )
        .map_err(|e| AgentError::Spawn { source: e })?;

        let (tx, _) = broadcast::channel::<AgentEvent>(256);

        let _ = tx.send(AgentEvent::Started {
            session_id: session_id.clone(),
        });

        let pipe = Arc::new(Mutex::new(Some(pipe)));
        let pipe_clone = pipe.clone();
        let tx_clone = tx.clone();
        let sid_clone = session_id.clone();

        let reader_task = tokio::task::spawn_blocking(move || {
            reader_loop(pipe_clone, tx_clone, tool, sid_clone);
        });

        Ok(Self {
            session_id,
            tx,
            stdin: pipe,
            reader_task,
        })
    }

    /// Subscribe to receive all future `AgentEvent` values from this session.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Send a follow-up prompt via stdin (for persistent session mode).
    pub async fn send_prompt(&self, prompt: &str) -> Result<(), AgentError> {
        let prompt = prompt.to_owned();
        let pipe = self.stdin.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = pipe
                .lock()
                .map_err(|_| AgentError::Pty("pipe mutex poisoned".into()))?;
            if let Some(ref mut p) = *guard {
                p.write(&prompt)
                    .map_err(|e| AgentError::Spawn { source: e })?;
            }
            Ok::<(), AgentError>(())
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }

    /// Session ID assigned at spawn time.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Kill the pipe process.
    pub async fn kill(&self) -> Result<(), AgentError> {
        self.reader_task.abort();
        let pipe = self.stdin.clone();
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
}

// ---------------------------------------------------------------------------
// Reader loop (runs on blocking thread)
// ---------------------------------------------------------------------------

fn reader_loop(
    pipe: Arc<Mutex<Option<PipeProcess>>>,
    tx: broadcast::Sender<AgentEvent>,
    tool: CliTool,
    _session_id: String,
) {
    let mut parser = create_ndjson_parser(tool);

    loop {
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
                let still_running = pipe
                    .lock()
                    .ok()
                    .and_then(|mut g| g.as_mut().map(|p| p.is_running()))
                    .unwrap_or(false);
                if !still_running {
                    let _ = tx.send(AgentEvent::Exited { code: 0 });
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };

        let events = parser.parse_line(&line);
        for event in events {
            let agent_event = map_cli_event(event);
            let _ = tx.send(agent_event);
        }
    }
}

/// Map a CliEvent from the NDJSON parser to the unified AgentEvent.
fn map_cli_event(event: CliEvent) -> AgentEvent {
    match event {
        CliEvent::SessionStart {
            session_id,
            model,
            tools,
        } => AgentEvent::PipeSessionStart {
            session_id,
            model,
            tools,
        },
        CliEvent::AssistantText { text, is_delta } => AgentEvent::PipeText { text, is_delta },
        CliEvent::ToolCallStart { id, name, input } => {
            AgentEvent::PipeToolStart { id, name, input }
        }
        CliEvent::ToolCallResult {
            id,
            output,
            is_error,
            duration_ms,
        } => AgentEvent::PipeToolResult {
            id,
            output,
            is_error,
            duration_ms,
        },
        CliEvent::Thinking { text } => AgentEvent::PipeThinking { text },
        CliEvent::TurnComplete {
            input_tokens,
            output_tokens,
        } => AgentEvent::PipeTurnComplete {
            input_tokens,
            output_tokens,
        },
        CliEvent::SessionEnd {
            result,
            cost_usd,
            is_error,
        } => AgentEvent::PipeSessionEnd {
            result,
            cost_usd,
            is_error,
        },
        CliEvent::Error { message } => AgentEvent::Error { message },
    }
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("pipe-{:x}", t)
}
