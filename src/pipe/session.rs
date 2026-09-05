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
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::core::error::AgentError;
use crate::core::types::{AgentEvent, CliTool, SessionConfig};
use crate::pipe::cli::{create_ndjson_parser, CliEvent};
use crate::pipe::process::{PipeOutput, PipeProcess, PipeProcessOptions, PIPE_TIMEOUT_SECONDS};
use gate4agent_types::{LaunchSpec, PipePromptDelivery};

/// How long [`PipeSession::stop`] with `force = false` waits for the process
/// to exit on its own before falling back to a hard kill.
///
/// Measured live 2026-09-05 against the piped ACP adapters this project
/// spawns (`AcpProcess`/`acp_command`) as a proxy for "does closing stdin let
/// a provider process end itself, and how long does that take": spawn,
/// complete the `initialize` handshake, close stdin, time the exit.
/// `claude-agent-acp@0.74.0` exited 0.05s after stdin close, `codex-
/// acp@1.10.0` (over quota, but `initialize` still ran) 4.81s, `grok agent
/// stdio` 3.22s — all exit code 0. `PipeSession`'s own children (`claude -p`,
/// `codex exec --json`, ...) already have stdin closed by the time a session
/// exists (`PipeProcess::spawn_command` writes the prompt and drops the
/// write half immediately), so a graceful stop here means "let the run
/// already under way finish" rather than sending a fresh signal — 10s leaves
/// better than 2x margin over the slowest process measured.
pub const PIPE_GRACEFUL_STOP_BOUND_SECS: u64 = 10;

/// Outcome of [`PipeSession::stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipeStopOutcome {
    /// The process's real exit code — known only when it exited on its own
    /// within the graceful bound. `None` when the stop performed (or fell
    /// back to) a hard kill: `kill_tree` does not itself observe an exit
    /// code, and the caller already has its own authoritative source for one
    /// (the broadcast `AgentEvent::Exited`).
    pub exit_code: Option<i32>,
    /// `true` when the process was killed rather than left to exit on its
    /// own — either because the caller asked for `force = true`, or the
    /// graceful bound elapsed while the process was still running.
    pub forced: bool,
}

/// Async pipe session. Spawns a CLI tool in headless pipe mode and broadcasts
/// NDJSON events as `AgentEvent` to all subscribers via a tokio broadcast channel.
///
/// This is the machine-readable counterpart to `PtySession`. Use this for
/// Telegram bots, web UIs, HTTP SSE, Discord bots, etc.
pub struct PipeSession {
    session_id: String,
    tx: broadcast::Sender<AgentEvent>,
    initial_events: Mutex<Option<broadcast::Receiver<AgentEvent>>>,
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
        let session_id = uuid_v4();

        let pipe =
            PipeProcess::new_with_options(tool, &config.working_dir, initial_prompt, options)
                .map_err(|e| AgentError::Spawn { source: e })?;

        let (tx, initial_events) = broadcast::channel::<AgentEvent>(256);

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
            initial_events: Mutex::new(Some(initial_events)),
            stdin: pipe,
            reader_task,
        })
    }

    /// Spawn a catalog-declared command while retaining a verified provider
    /// NDJSON adapter. Used for providers with a distinct headless executable
    /// and for controlled process fixtures.
    pub async fn spawn_with_launch(
        config: SessionConfig,
        initial_prompt: &str,
        launch: &LaunchSpec,
        prompt_delivery: PipePromptDelivery,
    ) -> Result<Self, AgentError> {
        let tool = config.tool;
        let session_id = uuid_v4();
        let pipe = PipeProcess::new_with_launch(
            tool,
            &config.working_dir,
            initial_prompt,
            launch,
            prompt_delivery,
        )
        .map_err(|source| AgentError::Spawn { source })?;
        let (tx, initial_events) = broadcast::channel::<AgentEvent>(256);
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
            initial_events: Mutex::new(Some(initial_events)),
            stdin: pipe,
            reader_task,
        })
    }

    /// Subscribe to receive all future `AgentEvent` values from this session.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.initial_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .unwrap_or_else(|| self.tx.subscribe())
    }

    /// Attempt to send a follow-up prompt.
    ///
    /// Current Pipe adapters are one-shot and return `BrokenPipe`; use a new
    /// process with the provider-native resume option for another turn.
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

    pub fn process_id(&self) -> Option<u32> {
        self.stdin
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(PipeProcess::process_id))
    }

    pub fn reader_finished(&self) -> bool {
        self.reader_task.is_finished()
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

    /// Stop the pipe process.
    ///
    /// `force = true` kills the process tree immediately — identical to
    /// [`PipeSession::kill`]. `force = false` waits up to
    /// [`PIPE_GRACEFUL_STOP_BOUND_SECS`] for the process to exit on its own
    /// (stdin is already closed by spawn time for every supported CLI, so
    /// this is "let the run finish" rather than a new signal), reporting the
    /// real exit code when it does, and only kills if the bound elapses.
    ///
    /// If the process has already exited on its own by the time this is
    /// called — including the case where the reader loop's own timeout or
    /// output-limit guard already terminated it — `reader_finished()` is
    /// already `true` and this returns immediately with `forced: false`.
    pub async fn stop(&self, force: bool) -> Result<PipeStopOutcome, AgentError> {
        if !force {
            let deadline = Instant::now() + Duration::from_secs(PIPE_GRACEFUL_STOP_BOUND_SECS);
            loop {
                if self.reader_finished() {
                    return Ok(PipeStopOutcome {
                        exit_code: Some(get_exit_code(&self.stdin)),
                        forced: false,
                    });
                }
                if Instant::now() >= deadline {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
        self.kill().await?;
        Ok(PipeStopOutcome {
            exit_code: None,
            forced: true,
        })
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
    let deadline = Instant::now() + Duration::from_secs(PIPE_TIMEOUT_SECONDS);

    loop {
        if Instant::now() >= deadline {
            terminate_pipe(
                &pipe,
                &tx,
                &mut parser_emitted_session_end,
                "pipe process timed out",
                124,
            );
            break;
        }

        let output = {
            match pipe.lock() {
                Ok(guard) => {
                    if let Some(ref p) = *guard {
                        p.try_recv_output()
                    } else {
                        break;
                    }
                }
                Err(_) => break,
            }
        };

        let line = match output {
            Ok(PipeOutput::Stdout(line)) => line,
            Ok(PipeOutput::LimitExceeded) => {
                terminate_pipe(
                    &pipe,
                    &tx,
                    &mut parser_emitted_session_end,
                    "pipe output limit exceeded",
                    125,
                );
                break;
            }
            Ok(PipeOutput::ReadFailed) => {
                terminate_pipe(
                    &pipe,
                    &tx,
                    &mut parser_emitted_session_end,
                    "pipe output read failed",
                    1,
                );
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let still_running = pipe
                    .lock()
                    .ok()
                    .and_then(|mut g| g.as_mut().map(|p| p.is_running()))
                    .unwrap_or(false);
                if still_running {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let exit_code = get_exit_code(&pipe);
                if !parser_emitted_session_end {
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

fn terminate_pipe(
    pipe: &Arc<Mutex<Option<PipeProcess>>>,
    tx: &broadcast::Sender<AgentEvent>,
    parser_emitted_session_end: &mut bool,
    message: &str,
    exit_code: i32,
) {
    if let Ok(mut guard) = pipe.lock() {
        if let Some(process) = guard.as_mut() {
            let _ = process.kill_tree();
        }
    }
    let _ = tx.send(AgentEvent::Error {
        message: message.to_owned(),
    });
    if !*parser_emitted_session_end {
        let _ = tx.send(AgentEvent::SessionEnd {
            result: message.to_owned(),
            cost_usd: None,
            is_error: true,
            stop_reason: None,
        });
        *parser_emitted_session_end = true;
    }
    let _ = tx.send(AgentEvent::Exited { code: exit_code });
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
            non_execution_kind: None,
        },
        CliEvent::Thinking { text } => AgentEvent::Thinking { text },
        CliEvent::TurnComplete {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            context_window,
            is_cumulative,
        } => AgentEvent::TurnComplete {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            context_window,
            is_cumulative,
        },
        CliEvent::ContextWindowUsage { usage } => AgentEvent::ContextWindowUsage { usage },
        CliEvent::SessionEnd {
            result,
            cost_usd,
            is_error,
        } => AgentEvent::SessionEnd {
            result,
            cost_usd,
            is_error,
            stop_reason: None,
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
    use crate::core::types::ContextWindowUsage;
    use crate::pipe::cli::NdjsonParser;

    #[test]
    fn exact_context_usage_maps_without_becoming_turn_usage() {
        let usage = ContextWindowUsage {
            uncached_input_tokens: 70,
            cache_read_tokens: 20,
            cache_write_tokens: 0,
            output_tokens: 10,
            unattributed_tokens: 5,
            used_tokens: 105,
            capacity_tokens: 100,
        };
        assert!(matches!(
            map_cli_event(CliEvent::ContextWindowUsage { usage: usage.clone() }),
            AgentEvent::ContextWindowUsage { usage: mapped } if mapped == usage
        ));
    }

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
                stop_reason: None,
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
        if let AgentEvent::SessionEnd {
            result,
            is_error,
            cost_usd,
            ..
        } = session_end
        {
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
        if let AgentEvent::SessionEnd {
            result, is_error, ..
        } = session_end
        {
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
            assert_eq!(
                result, "parser_emitted",
                "should be parser's SessionEnd, not synthetic"
            );
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

    // -----------------------------------------------------------------------
    // PipeSession::stop — graceful vs. forced, against real fixture children.
    //
    // Both fixtures are spawned through `spawn_with_launch`/`StdinClose`:
    // `PipeProcess::spawn_command` writes the prompt then drops the write
    // half immediately, so by the time `PipeSession::stop` runs, stdin is
    // already closed for both — the only variable is whether the child
    // itself waits on stdin (graceful fixture) or ignores it entirely
    // (ignore-stdin fixture), matching the real ACP adapters measured live
    // (see `PIPE_GRACEFUL_STOP_BOUND_SECS`'s doc comment).
    // -----------------------------------------------------------------------

    #[cfg(windows)]
    fn graceful_exit_launch() -> LaunchSpec {
        LaunchSpec {
            program: "powershell.exe".to_owned(),
            fixed_args: vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                "[Console]::In.ReadToEnd() | Out-Null; exit 0".to_owned(),
            ],
        }
    }

    #[cfg(not(windows))]
    fn graceful_exit_launch() -> LaunchSpec {
        LaunchSpec {
            program: "sh".to_owned(),
            fixed_args: vec!["-c".to_owned(), "cat >/dev/null; exit 0".to_owned()],
        }
    }

    #[cfg(windows)]
    fn ignore_stdin_launch() -> LaunchSpec {
        LaunchSpec {
            program: "powershell.exe".to_owned(),
            fixed_args: vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                "Start-Sleep -Seconds 30".to_owned(),
            ],
        }
    }

    #[cfg(not(windows))]
    fn ignore_stdin_launch() -> LaunchSpec {
        LaunchSpec {
            program: "sh".to_owned(),
            fixed_args: vec!["-c".to_owned(), "sleep 30".to_owned()],
        }
    }

    #[tokio::test]
    async fn stop_graceful_reports_the_real_exit_code_for_a_process_that_ends_itself() {
        let launch = graceful_exit_launch();
        let session = PipeSession::spawn_with_launch(
            SessionConfig::default(),
            "prompt",
            &launch,
            PipePromptDelivery::StdinClose,
        )
        .await
        .expect("graceful-exit fixture must spawn");

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
    async fn stop_graceful_falls_back_to_a_kill_once_the_bound_elapses() {
        let launch = ignore_stdin_launch();
        let session = PipeSession::spawn_with_launch(
            SessionConfig::default(),
            "prompt",
            &launch,
            PipePromptDelivery::StdinClose,
        )
        .await
        .expect("ignore-stdin fixture must spawn");

        let outcome = session
            .stop(false)
            .await
            .expect("stop(force=false) must still succeed by falling back to a kill");

        assert!(
            outcome.forced,
            "a process that ignores stdin must be force-killed once the graceful bound elapses"
        );
        assert!(outcome.exit_code.is_none(), "outcome: {outcome:?}");
    }

    #[tokio::test]
    async fn stop_force_true_kills_immediately_without_waiting() {
        let launch = ignore_stdin_launch();
        let session = PipeSession::spawn_with_launch(
            SessionConfig::default(),
            "prompt",
            &launch,
            PipePromptDelivery::StdinClose,
        )
        .await
        .expect("ignore-stdin fixture must spawn");

        let start = Instant::now();
        let outcome = session
            .stop(true)
            .await
            .expect("stop(force=true) must succeed");
        assert!(outcome.forced);
        assert!(
            start.elapsed() < Duration::from_secs(PIPE_GRACEFUL_STOP_BOUND_SECS),
            "force=true must not wait out the graceful bound"
        );
    }
}
