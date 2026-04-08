//! Pipe runner — spawns a CLI process and drives the NDJSON reader loop.
//!
//! Phase 5 deliverable. This module lifts the reader-loop logic out of
//! `pipe/session.rs` and adds **SessionEnd synthesis**: when a child process
//! exits and the parser has not emitted a `SessionEnd` event (e.g. Codex, which
//! has no terminal event by design), the runner synthesizes one automatically:
//!
//! ```text
//! AgentEvent::SessionEnd { result: "exit_code=N", cost_usd: None, is_error: N != 0 }
//! ```
//!
//! This ensures all PIPE and DaemonHarness consumers see exactly one
//! `SessionEnd` per session, regardless of which CLI produced it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;

use crate::error::AgentError;
use crate::ndjson::{create_ndjson_parser, CliEvent};
use crate::pipe::process::{PipeProcess, PipeProcessOptions};
use crate::transport::SpawnOptions;
use crate::types::{AgentEvent, CliTool};

// ---------------------------------------------------------------------------
// PipeRunnerHandle — public handle returned to callers
// ---------------------------------------------------------------------------

/// Handle to a running pipe-based CLI session.
///
/// Returned by [`run_pipe`]. Provides event subscription, prompt sending,
/// and process termination. The reader loop and SessionEnd synthesis run
/// in a background blocking thread managed by this handle.
pub struct PipeRunnerHandle {
    pub(crate) tx: broadcast::Sender<AgentEvent>,
    /// Guard held for as long as the child process is alive.
    /// Wrapped in `Option` so we can move it out on `kill()`.
    pub(crate) process: Arc<Mutex<Option<PipeProcess>>>,
    pub(crate) session_id: String,
}

impl PipeRunnerHandle {
    /// Subscribe to all future `AgentEvent` values from this session.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// The session ID assigned at spawn time.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a follow-up prompt via stdin (for CLIs that support multi-turn pipe mode).
    pub async fn send_prompt(&self, prompt: &str) -> Result<(), AgentError> {
        let prompt = prompt.to_owned();
        let process = self.process.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = process
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

    /// Kill the underlying process.
    pub async fn kill(&self) -> Result<(), AgentError> {
        let process = self.process.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = process
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
// run_pipe — spawn + reader thread entry point
// ---------------------------------------------------------------------------

/// Spawn a pipe-based CLI process and return a [`PipeRunnerHandle`].
///
/// The initial prompt is embedded in `opts` (via `SpawnOptions::prompt`).
/// The reader loop starts immediately in a blocking background thread.
///
/// # SessionEnd synthesis
///
/// When the child process exits, if the parser has not already emitted a
/// `AgentEvent::SessionEnd`, the runner synthesizes one:
/// ```text
/// AgentEvent::SessionEnd { result: "exit_code=N", cost_usd: None, is_error: N != 0 }
/// ```
/// This covers Codex, which exits with code 0 but never emits a terminal event.
pub fn run_pipe(
    tool: CliTool,
    opts: SpawnOptions,
) -> Result<PipeRunnerHandle, AgentError> {
    let session_id = uuid_v4();

    // Convert SpawnOptions → PipeProcessOptions for the legacy PipeProcess.
    let pipe_opts = spawn_opts_to_pipe_opts(&opts);

    let pipe = PipeProcess::new_with_options(tool, &opts.working_dir, &opts.prompt, pipe_opts)
        .map_err(|e| AgentError::Spawn { source: e })?;

    let (tx, _) = broadcast::channel::<AgentEvent>(256);

    // Broadcast the Started lifecycle event immediately.
    let _ = tx.send(AgentEvent::Started {
        session_id: session_id.clone(),
    });

    let process = Arc::new(Mutex::new(Some(pipe)));
    let process_clone = process.clone();
    let tx_clone = tx.clone();
    let sid_clone = session_id.clone();

    // Spawn the reader loop on a dedicated blocking thread.
    tokio::task::spawn_blocking(move || {
        reader_loop(process_clone, tx_clone, tool, sid_clone);
    });

    Ok(PipeRunnerHandle {
        tx,
        process,
        session_id,
    })
}

// ---------------------------------------------------------------------------
// Reader loop (blocking thread)
// ---------------------------------------------------------------------------

fn reader_loop(
    process: Arc<Mutex<Option<PipeProcess>>>,
    tx: broadcast::Sender<AgentEvent>,
    tool: CliTool,
    _session_id: String,
) {
    let mut parser = create_ndjson_parser(tool);
    // Tracks whether the parser has ever emitted SessionEnd.
    // If false when the child exits, we synthesize one.
    let mut parser_emitted_session_end = false;

    loop {
        let line = {
            match process.lock() {
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
                // No data available; check if the child is still running.
                let still_running = process
                    .lock()
                    .ok()
                    .and_then(|mut g| g.as_mut().map(|p| p.is_running()))
                    .unwrap_or(false);
                if !still_running {
                    // Child exited — get exit code and synthesize SessionEnd if needed.
                    let exit_code = get_exit_code(&process);
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
            // Track whether the parser produced a SessionEnd.
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
// CliEvent → AgentEvent mapping (shared with pipe/session.rs)
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

fn spawn_opts_to_pipe_opts(opts: &SpawnOptions) -> PipeProcessOptions {
    PipeProcessOptions {
        extra_args: opts.extra_args.clone(),
        claude: crate::pipe::process::ClaudeOptions {
            resume_session_id: opts.resume_session_id.clone(),
            model: opts.model.clone(),
            append_system_prompt: opts.append_system_prompt.clone(),
        },
    }
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("runner-{:x}", t)
}

// ---------------------------------------------------------------------------
// Unit tests for SessionEnd synthesis
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndjson::traits::NdjsonParser;

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
    ///
    /// Simulates a reader loop with the given parser and lines, then returns
    /// all AgentEvent values that would be emitted (excluding broadcast overhead).
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

        // Simulate process exit.
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

        // Verify the synthetic event has the right shape.
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
        // Feed one line so the parser emits its SessionEnd.
        let events = simulate_reader(&mut parser, &["anything"], 0);

        let session_end_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::SessionEnd { .. }))
            .count();
        assert_eq!(
            session_end_count, 1,
            "expected exactly one SessionEnd when parser already emitted one"
        );

        // The emitted one should be the parser's, not the synthetic one.
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

        // SessionEnd must come before Exited.
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
