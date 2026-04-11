//! Async pipe session with tokio broadcast fan-out.
//!
//! `PipeSession` spawns a CLI tool in headless pipe mode and broadcasts
//! NDJSON events as `AgentEvent` to all subscribers via a tokio broadcast channel.
//!
//! # SessionEnd synthesis
//!
//! When a child process exits without having emitted a `SessionEnd` event
//! (e.g. Codex, which exits with code 0 but never emits a terminal event),
//! the reader loop synthesizes one automatically:
//!
//! ```text
//! AgentEvent::SessionEnd { result: "exit_code=N", cost_usd: None, is_error: N != 0 }
//! ```
//!
//! This guarantees exactly one `SessionEnd` per session regardless of CLI.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::core::error::AgentError;
use crate::pipe::cli::{create_ndjson_parser, CliEvent};
use crate::pipe::process::{PipeProcess, PipeProcessOptions};
use crate::core::types::{AgentEvent, CliTool, SessionConfig};

/// Async pipe session. Spawns a CLI tool in headless pipe mode and broadcasts
/// NDJSON events as `AgentEvent` to all subscribers via a tokio broadcast channel.
///
/// This is the machine-readable counterpart to `PtySession`. Use this for
/// Telegram bots, web UIs, HTTP SSE, Discord bots, etc.
pub struct PipeSession {
    session_id: String,
    tool: CliTool,
    /// Model override passed at spawn time (from `PipeProcessOptions::claude::model`).
    /// `None` means the tool's built-in default was used.
    model: Option<String>,
    tx: broadcast::Sender<AgentEvent>,
    stdin: Arc<Mutex<Option<PipeProcess>>>,
    reader_task: JoinHandle<()>,
}

impl PipeSession {
    /// Spawn an agent in headless pipe mode and start broadcasting NDJSON events.
    ///
    /// # Errors
    ///
    /// - `AgentError::Spawn` — the child process failed to start
    pub async fn spawn(
        config: SessionConfig,
        initial_prompt: &str,
        options: PipeProcessOptions,
    ) -> Result<Self, AgentError> {
        let tool = config.tool;
        let model = options.claude.model.clone();
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
            tool,
            model,
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

    /// CLI tool type this session was spawned with.
    pub fn tool(&self) -> CliTool {
        self.tool
    }

    /// Model override passed at spawn time, if any.
    ///
    /// Returns `None` when no explicit model was requested and the tool's
    /// built-in default is in use.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
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
    let mut parser_emitted_session_end = false;

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

        let events = parser.parse_line(&line);
        for event in events {
            if matches!(event, CliEvent::SessionEnd { .. }) {
                parser_emitted_session_end = true;
            }
            let agent_event = map_cli_event(event);
            let _ = tx.send(agent_event);
        }
    }
}

/// Attempt to collect the child process exit code.
/// Falls back to 0 if the lock is poisoned or `wait` fails.
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

// ---------------------------------------------------------------------------
// CliEvent → AgentEvent mapping
// ---------------------------------------------------------------------------

pub(crate) fn map_cli_event(event: CliEvent) -> AgentEvent {
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

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("pipe-{:x}", t)
}

// ---------------------------------------------------------------------------
// Unit tests for SessionEnd synthesis
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::cli::NdjsonParser;

    /// A fake parser that never emits SessionEnd.
    struct NeverEndsParser;
    impl NdjsonParser for NeverEndsParser {
        fn parse_line(&mut self, _line: &str) -> Vec<CliEvent> {
            vec![]
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
    }

    /// A fake parser that always emits SessionEnd on the first line.
    struct AlwaysEndsParser {
        emitted: bool,
    }
    impl AlwaysEndsParser {
        fn new() -> Self {
            Self { emitted: false }
        }
    }
    impl NdjsonParser for AlwaysEndsParser {
        fn parse_line(&mut self, _line: &str) -> Vec<CliEvent> {
            if !self.emitted {
                self.emitted = true;
                vec![CliEvent::SessionEnd {
                    result: "parser_emitted".to_string(),
                    cost_usd: None,
                    is_error: false,
                }]
            } else {
                vec![]
            }
        }
        fn session_id(&self) -> Option<&str> {
            None
        }
    }

    /// Helper: drive the synthesis logic directly without spawning a real process.
    fn simulate_reader(
        parser: &mut dyn NdjsonParser,
        lines: &[&str],
        exit_code: i32,
    ) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let mut parser_emitted_session_end = false;

        for line in lines {
            let cli_events = parser.parse_line(line);
            for ev in cli_events {
                if matches!(ev, CliEvent::SessionEnd { .. }) {
                    parser_emitted_session_end = true;
                }
                events.push(map_cli_event(ev));
            }
        }

        if !parser_emitted_session_end {
            events.push(AgentEvent::SessionEnd {
                result: format!("exit_code={}", exit_code),
                cost_usd: None,
                is_error: exit_code != 0,
            });
        }
        events.push(AgentEvent::Exited { code: exit_code });
        events
    }

    #[test]
    fn codex_exit_triggers_synthetic_session_end() {
        let mut parser = NeverEndsParser;
        let events = simulate_reader(&mut parser, &[], 0);

        let session_end_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::SessionEnd { .. }))
            .count();
        assert_eq!(
            session_end_count, 1,
            "expected exactly one SessionEnd when parser never emits one"
        );

        let session_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::SessionEnd { .. }))
            .unwrap();
        if let AgentEvent::SessionEnd { result, is_error, cost_usd } = session_end {
            assert_eq!(result, "exit_code=0");
            assert!(!is_error);
            assert!(cost_usd.is_none());
        }
    }

    #[test]
    fn non_zero_exit_code_marks_session_end_as_error() {
        let mut parser = NeverEndsParser;
        let events = simulate_reader(&mut parser, &[], 1);

        let session_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::SessionEnd { .. }))
            .unwrap();
        if let AgentEvent::SessionEnd { result, is_error, .. } = session_end {
            assert_eq!(result, "exit_code=1");
            assert!(is_error);
        }
    }

    #[test]
    fn parser_emitted_session_end_not_duplicated() {
        let mut parser = AlwaysEndsParser::new();
        let events = simulate_reader(&mut parser, &["anything"], 0);

        let session_end_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::SessionEnd { .. }))
            .count();
        assert_eq!(
            session_end_count, 1,
            "expected exactly one SessionEnd when parser already emitted one"
        );

        let session_end = events
            .iter()
            .find(|e| matches!(e, AgentEvent::SessionEnd { .. }))
            .unwrap();
        if let AgentEvent::SessionEnd { result, .. } = session_end {
            assert_eq!(result, "parser_emitted", "should be parser's SessionEnd, not synthetic");
        }
    }

    #[test]
    fn exited_event_always_emitted_after_session_end() {
        let mut parser = NeverEndsParser;
        let events = simulate_reader(&mut parser, &[], 0);

        let session_end_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::SessionEnd { .. }))
            .expect("SessionEnd must be present");
        let exited_pos = events
            .iter()
            .position(|e| matches!(e, AgentEvent::Exited { .. }))
            .expect("Exited must be present");
        assert!(
            session_end_pos < exited_pos,
            "SessionEnd must precede Exited"
        );
    }
}
