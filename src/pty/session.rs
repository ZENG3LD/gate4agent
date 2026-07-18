//! Async PTY session with tokio broadcast fan-out.

use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::ChildKiller;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::agent::{
    builtin_registry, plan_draft_launch, plan_launch, prepare_agent_command, prepare_input,
    AgentCommandMode, AgentId, AgentSpec, InputAction, LaunchPlan, LaunchRequest, PreparedInput,
    PreparedInputKind, PromptFraming, PromptPayload, ReadinessIntent, ReadinessPermit,
    TERMINAL_WRITE_DELAY_MAX_MS,
};
use crate::core::error::AgentError;
use crate::core::types::{AgentEvent, CliTool, SessionConfig};
use crate::pty::cli::traits::{MessageClass, StartupAction};
use crate::pty::cli::{create_pipeline, create_submitter};
use crate::pty::rate_limit::RateLimitDetector;
use crate::pty::vte::VteParser;

use super::event::{
    PtyAttachError, PtyAttachment, PtyEvent, PtyEventPublisher, PtyEventReceiver, PtyReplayCursor,
    PtySignal, PtySignalOutcome, PtySize, PtyTerminalSnapshot, DEFAULT_PTY_REPLAY_BYTES,
    PTY_PROVIDER_PROTOCOL_REVISION,
};
use super::os_process::PtyForegroundObservation;
use super::process_tree::{terminate_process_tree, PtyTreeTerminationReport};
use super::wrapper::{PtyError, PtyReadEvent, PtyWrapper};

const PTY_EVENT_CHANNEL_CAPACITY: usize = 64;
const PTY_POST_EXIT_DRAIN_QUIET_MS: u64 = 100;
pub const PTY_SHUTDOWN_TIMEOUT_MS: u64 = 8_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PtyShutdownOutcome {
    pub exit_code: Option<i32>,
    pub termination: Option<PtyTreeTerminationReport>,
}

/// Conversion from the internal PtyError to the public AgentError.
impl From<PtyError> for AgentError {
    fn from(e: PtyError) -> Self {
        match e {
            PtyError::CreateFailed(s) => AgentError::PtyCreate(s),
            PtyError::SpawnFailed(s) => AgentError::PtySpawn(s),
            PtyError::Io(e) => AgentError::PtyIo { source: e },
            PtyError::Pty(s) => AgentError::Pty(s),
            PtyError::UnsafeWindowsCommandArgument { index } => AgentError::PtySpawn(format!(
                "Windows command wrapper argument {index} contains shell metacharacters"
            )),
            PtyError::ReaderJoinTimedOut { timeout_ms } => {
                AgentError::PtyShutdownTimedOut { timeout_ms }
            }
            PtyError::ReaderPanicked => AgentError::Pty("PTY OS reader thread panicked".into()),
        }
    }
}

/// Opaque write handle for sending input to a PTY.
pub struct PtyWriteHandle {
    inner: Arc<Mutex<PtyWrapper>>,
}

impl PtyWriteHandle {
    /// Write raw bytes to the PTY.
    pub fn write(&self, data: &str) -> Result<(), AgentError> {
        let mut pty = self
            .inner
            .lock()
            .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
        pty.write(data).map_err(|e| AgentError::Pty(e.to_string()))
    }

    /// Write raw bytes to the PTY.
    pub fn write_bytes(&self, data: &[u8]) -> Result<(), AgentError> {
        let mut pty = self
            .inner
            .lock()
            .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
        pty.write_bytes(data).map_err(AgentError::from)
    }

    /// Resize the PTY.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), AgentError> {
        let pty = self
            .inner
            .lock()
            .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
        pty.resize(rows, cols).map_err(AgentError::from)
    }
}

/// Async PTY session. Spawns a CLI tool in a real PTY and broadcasts `AgentEvent`
/// to all subscribers via a tokio broadcast channel.
///
/// The reader loop runs on a blocking thread (via `spawn_blocking`) because PTY
/// I/O is inherently blocking. The loop bridges to async consumers via the
/// broadcast channel.
///
/// # Broadcast semantics
///
/// `subscribe()` preserves the legacy best-effort `AgentEvent` channel.
/// Resilient consumers use `subscribe_events()` or `attach_events()`: those
/// streams expose generation/sequence, convert lag into `DataGap`, and provide
/// bounded replay plus a sequence-pinned terminal snapshot.
pub struct PtySession {
    session_id: String,
    agent_id: AgentId,
    process_spec: AgentSpec,
    legacy_tool: Option<CliTool>,
    agent_command_mode: Option<AgentCommandMode>,
    tx: broadcast::Sender<AgentEvent>,
    pty: Arc<Mutex<PtyWrapper>>,
    root_pid: Option<u32>,
    reader_task: Option<JoinHandle<()>>,
    killer: Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>,
    events: Arc<PtyEventPublisher>,
    pending_followup_prompt: Option<String>,
    pending_followup_draft: Option<String>,
    typed_input_lock: tokio::sync::Mutex<()>,
}

impl PtySession {
    /// Spawn a CLI tool in a PTY and start broadcasting events.
    ///
    /// Uses a 24x80 terminal size (compact). To control size, use `spawn_with_size`.
    pub async fn spawn(config: SessionConfig) -> Result<Self, AgentError> {
        Self::spawn_with_size(config, 24, 80).await
    }

    /// Spawn a CLI tool in a PTY with a specific terminal size.
    pub async fn spawn_with_size(
        config: SessionConfig,
        rows: u16,
        cols: u16,
    ) -> Result<Self, AgentError> {
        let tool = config.tool;
        let agent_id = AgentId::from(tool);
        let process_spec = builtin_registry()
            .get(&agent_id)
            .expect("legacy CLI tools have built-in process specifications")
            .clone();
        let pty =
            PtyWrapper::new_with_env(tool, &config.working_dir, &config.env_vars, rows, cols)?;
        Ok(Self::start(
            agent_id,
            process_spec,
            Some(tool),
            match tool {
                CliTool::OpenCode => None,
                CliTool::ClaudeCode | CliTool::Codex | CliTool::Gemini => {
                    Some(AgentCommandMode::SlashLine)
                }
            },
            pty,
            None,
            None,
            rows,
            cols,
        ))
    }

    /// Spawn any registered agent from its shell-free launch specification.
    pub async fn spawn_agent(spec: &AgentSpec, request: LaunchRequest) -> Result<Self, AgentError> {
        Self::spawn_agent_with_size(spec, request, 24, 80).await
    }

    /// Spawn any registered agent with a specific PTY size.
    pub async fn spawn_agent_with_size(
        spec: &AgentSpec,
        request: LaunchRequest,
        rows: u16,
        cols: u16,
    ) -> Result<Self, AgentError> {
        let plan = plan_launch(spec, request)?;
        Self::spawn_generic_plan(
            plan,
            spec.clone(),
            spec.capabilities.agent_commands,
            rows,
            cols,
        )
    }

    /// Spawn an agent with a reviewable draft that is never auto-submitted.
    pub async fn spawn_agent_draft(
        spec: &AgentSpec,
        request: LaunchRequest,
        draft: String,
    ) -> Result<Self, AgentError> {
        Self::spawn_agent_draft_with_size(spec, request, draft, 24, 80).await
    }

    /// Spawn an agent with a reviewable draft and a specific PTY size.
    pub async fn spawn_agent_draft_with_size(
        spec: &AgentSpec,
        request: LaunchRequest,
        draft: String,
        rows: u16,
        cols: u16,
    ) -> Result<Self, AgentError> {
        let plan = plan_draft_launch(spec, request, draft)?;
        Self::spawn_generic_plan(
            plan,
            spec.clone(),
            spec.capabilities.agent_commands,
            rows,
            cols,
        )
    }

    fn spawn_generic_plan(
        plan: LaunchPlan,
        process_spec: AgentSpec,
        agent_command_mode: Option<AgentCommandMode>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, AgentError> {
        let agent_id = plan.agent_id.clone();
        let pending_followup_prompt = plan.followup_prompt.clone();
        let pending_followup_draft = plan.followup_draft.clone();
        // Generic registry entries never acquire a semantic adapter merely by
        // reusing a built-in ID. Adapter registration is a separate capability
        // boundary; the legacy four-tool constructor retains existing behavior.
        let legacy_tool = None;
        let pty = PtyWrapper::from_launch_plan(plan, legacy_tool, rows, cols)?;
        Ok(Self::start(
            agent_id,
            process_spec,
            legacy_tool,
            agent_command_mode,
            pty,
            pending_followup_prompt,
            pending_followup_draft,
            rows,
            cols,
        ))
    }

    fn start(
        agent_id: AgentId,
        process_spec: AgentSpec,
        legacy_tool: Option<CliTool>,
        agent_command_mode: Option<AgentCommandMode>,
        pty: PtyWrapper,
        pending_followup_prompt: Option<String>,
        pending_followup_draft: Option<String>,
        rows: u16,
        cols: u16,
    ) -> Self {
        let session_id = uuid_v4();
        let provider_revision = format!(
            "{PTY_PROVIDER_PROTOCOL_REVISION}:{}:{}",
            agent_id.as_str(),
            process_spec.revision
        );
        let root_pid = pty.root_pid();
        let killer = Arc::new(Mutex::new(pty.clone_killer()));
        let pty = Arc::new(Mutex::new(pty));
        let (tx, _) = broadcast::channel::<AgentEvent>(4096);
        let events = PtyEventPublisher::new(
            session_id.clone(),
            provider_revision,
            1,
            PTY_EVENT_CHANNEL_CAPACITY,
            DEFAULT_PTY_REPLAY_BYTES,
            rows,
            cols,
        );

        let _ = tx.send(AgentEvent::Started {
            session_id: session_id.clone(),
        });
        events.publish(PtyEvent::Started);

        let pty_clone = pty.clone();
        let tx_clone = tx.clone();
        let events_clone = events.clone();
        let reader_task = tokio::task::spawn_blocking(move || {
            reader_loop(pty_clone, tx_clone, events_clone, legacy_tool);
        });

        Self {
            session_id,
            agent_id,
            process_spec,
            legacy_tool,
            agent_command_mode,
            tx,
            pty,
            root_pid,
            reader_task: Some(reader_task),
            killer,
            events,
            pending_followup_prompt,
            pending_followup_draft,
            typed_input_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Subscribe to receive all future `AgentEvent` values from this session.
    ///
    /// Note: events that occurred before subscribing will not be received.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    /// Subscribe to sequenced PTY runtime events from the next event onward.
    pub fn subscribe_events(&self) -> Result<PtyEventReceiver, PtyAttachError> {
        self.events.subscribe()
    }

    /// Atomically obtain bounded replay and subscribe after the replay boundary.
    pub fn attach_events(&self, cursor: PtyReplayCursor) -> Result<PtyAttachment, PtyAttachError> {
        self.events.attach(cursor)
    }

    /// Capture terminal state and the exact event sequence it incorporates.
    pub fn terminal_snapshot(&self) -> Result<PtyTerminalSnapshot, PtyAttachError> {
        let snapshot = self.events.snapshot()?;
        self.events.publish(PtyEvent::SnapshotAvailable {
            snapshot_sequence: snapshot.sequence,
        });
        Ok(snapshot)
    }

    /// Take a fresh, bounded OS process-table observation for readiness.
    pub async fn observe_foreground(&self) -> Result<PtyForegroundObservation, AgentError> {
        let pty = self.pty.clone();
        let spec = self.process_spec.clone();
        let observation = tokio::task::spawn_blocking(move || {
            let guard = pty
                .lock()
                .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
            guard.observe_foreground(&spec).map_err(AgentError::from)
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))??;
        self.events
            .publish(PtyEvent::ForegroundProcess(observation.clone()));
        Ok(observation)
    }

    /// Get the write handle for sending input to the PTY.
    pub fn write_handle(&self) -> PtyWriteHandle {
        PtyWriteHandle {
            inner: self.pty.clone(),
        }
    }

    /// Send raw bytes to the PTY.
    pub async fn write(&self, data: &str) -> Result<(), AgentError> {
        let data = data.to_owned();
        let pty = self.pty.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = pty
                .lock()
                .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
            guard
                .write(&data)
                .map_err(|e| AgentError::Pty(e.to_string()))
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }

    /// Prepare and write a typed terminal action after readiness is proven.
    pub async fn send_input_action(
        &self,
        action: InputAction,
        permit: ReadinessPermit,
    ) -> Result<(), AgentError> {
        let prepared = match action {
            InputAction::AgentCommand(command) => {
                self.validate_permit(&permit, Some(ReadinessIntent::DraftPaste))?;
                if self.agent_command_mode != Some(AgentCommandMode::SlashLine) {
                    return Err(AgentError::AgentCapabilityUnsupported {
                        agent: self.agent_id.clone(),
                        capability: "agent-commands",
                    });
                }
                prepare_agent_command(command, &self.agent_id)?
            }
            action => prepare_input(action)?,
        };
        self.send_prepared_input(prepared, permit).await
    }

    /// Write an already prepared operation after readiness is proven.
    ///
    /// Typed operations are serialized. Submit remains a distinct delayed write
    /// so TUI paste handlers cannot consume Enter as part of the body.
    pub async fn send_prepared_input(
        &self,
        input: PreparedInput,
        permit: ReadinessPermit,
    ) -> Result<(), AgentError> {
        let required_intent = match input.kind() {
            PreparedInputKind::InsertDraft | PreparedInputKind::AgentCommand => {
                Some(ReadinessIntent::DraftPaste)
            }
            PreparedInputKind::SubmitPrompt => Some(ReadinessIntent::FollowupPrompt),
            PreparedInputKind::TerminalText | PreparedInputKind::TerminalControl => None,
        };
        self.validate_permit(&permit, required_intent)?;
        let _serialized = self.typed_input_lock.lock().await;
        for write in input.into_writes() {
            if write.delay_before_ms > TERMINAL_WRITE_DELAY_MAX_MS {
                return Err(AgentError::Pty(format!(
                    "prepared write delay {}ms exceeds {}ms",
                    write.delay_before_ms, TERMINAL_WRITE_DELAY_MAX_MS
                )));
            }
            if write.delay_before_ms > 0 {
                tokio::time::sleep(Duration::from_millis(write.delay_before_ms)).await;
            }
            let pty = self.pty.clone();
            tokio::task::spawn_blocking(move || {
                let mut guard = pty
                    .lock()
                    .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
                guard.write_bytes(&write.bytes).map_err(AgentError::from)
            })
            .await
            .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))??;
        }
        Ok(())
    }

    /// Prompt deferred by the launch plan, if one has not been submitted yet.
    pub fn pending_followup_prompt(&self) -> Option<&str> {
        self.pending_followup_prompt.as_deref()
    }

    /// Reviewable draft deferred by the launch plan, if any.
    pub fn pending_followup_draft(&self) -> Option<&str> {
        self.pending_followup_draft.as_deref()
    }

    /// Insert the launch plan's deferred draft exactly once without submitting.
    pub async fn insert_pending_followup_draft(
        &mut self,
        framing: PromptFraming,
        permit: ReadinessPermit,
    ) -> Result<bool, AgentError> {
        self.validate_permit(&permit, Some(ReadinessIntent::DraftPaste))?;
        let Some(draft) = self.pending_followup_draft.clone() else {
            return Ok(false);
        };
        self.send_input_action(
            InputAction::InsertDraft(PromptPayload {
                text: draft,
                framing,
            }),
            permit,
        )
        .await?;
        self.pending_followup_draft = None;
        Ok(true)
    }

    /// Submit the launch plan's deferred prompt exactly once.
    pub async fn submit_pending_followup(
        &mut self,
        framing: PromptFraming,
        permit: ReadinessPermit,
    ) -> Result<bool, AgentError> {
        self.validate_permit(&permit, Some(ReadinessIntent::FollowupPrompt))?;
        let Some(prompt) = self.pending_followup_prompt.clone() else {
            return Ok(false);
        };
        self.send_input_action(
            InputAction::SubmitPrompt(PromptPayload {
                text: prompt,
                framing,
            }),
            permit,
        )
        .await?;
        self.pending_followup_prompt = None;
        Ok(true)
    }

    fn validate_permit(
        &self,
        permit: &ReadinessPermit,
        required_intent: Option<ReadinessIntent>,
    ) -> Result<(), AgentError> {
        if permit.agent_id() != &self.agent_id {
            return Err(AgentError::PtyReadinessAgentMismatch {
                session_agent: self.agent_id.clone(),
                permit_agent: permit.agent_id().clone(),
            });
        }
        if let Some(required) = required_intent {
            if permit.intent() != required {
                return Err(AgentError::PtyReadinessIntentMismatch {
                    required,
                    actual: permit.intent(),
                });
            }
        }
        Ok(())
    }

    /// Send a prompt char-by-char (required for Ink-based TUI tools like Claude Code).
    ///
    /// Sends each character with a small delay to avoid overwhelming the TUI's raw-mode
    /// input processing. Ends with a carriage return.
    pub async fn send_prompt(&self, prompt: &str) -> Result<(), AgentError> {
        if self.legacy_tool.is_none() {
            return Err(AgentError::Pty(
                "legacy send_prompt is unavailable for generic agent sessions; use typed readiness-gated input"
                    .into(),
            ));
        }
        for ch in prompt.chars() {
            let s = ch.to_string();
            self.write(&s).await?;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        self.write("\r").await
    }

    /// Resize the PTY.
    pub async fn resize(&self, rows: u16, cols: u16) -> Result<(), AgentError> {
        let pty = self.pty.clone();
        tokio::task::spawn_blocking(move || {
            let guard = pty
                .lock()
                .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
            guard.resize(rows, cols).map_err(AgentError::from)
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))??;
        self.events
            .publish(PtyEvent::Resized(PtySize { rows, cols }));
        Ok(())
    }

    /// Deliver a platform-neutral PTY control or request process termination.
    pub async fn signal(&self, signal: PtySignal) -> Result<PtySignalOutcome, AgentError> {
        match signal {
            PtySignal::InterruptKey => {
                let pty = self.pty.clone();
                tokio::task::spawn_blocking(move || {
                    let mut guard = pty
                        .lock()
                        .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
                    guard.write_bytes(b"\x03").map_err(AgentError::from)
                })
                .await
                .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))??;
                Ok(PtySignalOutcome::ControlWritten)
            }
            PtySignal::EndOfFileKey => {
                let pty = self.pty.clone();
                tokio::task::spawn_blocking(move || {
                    let mut guard = pty
                        .lock()
                        .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
                    guard.write_bytes(b"\x04").map_err(AgentError::from)
                })
                .await
                .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))??;
                Ok(PtySignalOutcome::ControlWritten)
            }
            PtySignal::TerminateProcess => {
                self.kill().await?;
                Ok(PtySignalOutcome::TerminationRequested)
            }
        }
    }

    /// Session ID assigned at spawn time.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Current event-stream generation.
    pub fn generation(&self) -> u64 {
        self.events.generation()
    }

    /// Revision pinned into event envelopes, cursors, and snapshots.
    pub fn provider_revision(&self) -> &str {
        self.events.provider_revision()
    }

    /// Cursor for replay from the beginning of this generation.
    pub fn beginning_cursor(&self) -> PtyReplayCursor {
        PtyReplayCursor::beginning(self.provider_revision(), self.generation())
    }

    /// Stable agent identity associated with the session.
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    /// Whether the output reader has reached a terminal state.
    pub fn reader_finished(&self) -> bool {
        self.reader_task
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }

    /// Snapshot and terminate agent descendants before killing the root handle.
    /// The returned report makes any root-only degradation explicit.
    pub async fn terminate_tree(&self) -> Result<PtyTreeTerminationReport, AgentError> {
        let root_pid = self.root_pid;
        let killer = self.killer.clone();
        tokio::task::spawn_blocking(move || {
            terminate_process_tree(root_pid, || {
                let mut killer = killer
                    .lock()
                    .map_err(|_| "PTY killer mutex poisoned".to_owned())?;
                match killer.kill() {
                    Ok(()) => Ok(()),
                    #[cfg(windows)]
                    Err(error) if error.raw_os_error() == Some(0) => Ok(()),
                    Err(error) => Err(error.to_string()),
                }
            })
            .map_err(AgentError::from)
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))?
    }

    /// Kill the process tree. The reader remains alive long enough to drain
    /// accepted PTY output and publish the ordered exit event.
    pub async fn kill(&self) -> Result<(), AgentError> {
        self.terminate_tree().await.map(|_| ())
    }

    /// Consume the session, terminate owned processes, and join the ordered
    /// reader/exit path within a bounded deadline.
    pub async fn shutdown(mut self) -> Result<PtyShutdownOutcome, AgentError> {
        let shutdown_started = Instant::now();
        let termination = if self.reader_finished() {
            None
        } else {
            Some(self.terminate_tree().await?)
        };
        if let Some(mut reader_task) = self.reader_task.take() {
            tokio::time::timeout(
                Duration::from_millis(PTY_SHUTDOWN_TIMEOUT_MS),
                &mut reader_task,
            )
            .await
            .map_err(|_| AgentError::PtyShutdownTimedOut {
                timeout_ms: PTY_SHUTDOWN_TIMEOUT_MS,
            })?
            .map_err(|_| AgentError::Pty("PTY reader task panicked".into()))?;
        }
        let remaining = Duration::from_millis(PTY_SHUTDOWN_TIMEOUT_MS)
            .saturating_sub(shutdown_started.elapsed());
        let pty = self.pty.clone();
        let exit_code = tokio::task::spawn_blocking(move || {
            let mut guard = pty
                .lock()
                .map_err(|_| AgentError::Pty("PTY mutex poisoned".into()))?;
            guard.close_and_join_reader(remaining)?;
            Ok::<_, AgentError>(guard.try_exit_code().map(|code| code as i32))
        })
        .await
        .map_err(|_| AgentError::Pty("spawn_blocking panicked".into()))??;
        Ok(PtyShutdownOutcome {
            exit_code,
            termination,
        })
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self
            .reader_task
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
        {
            return;
        }
        let mut killer = match self.killer.lock() {
            Ok(killer) => killer,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = killer.kill();
    }
}

// ---------------------------------------------------------------------------
// Reader loop (runs on blocking thread via spawn_blocking)
// ---------------------------------------------------------------------------

fn reader_loop(
    pty: Arc<Mutex<PtyWrapper>>,
    tx: broadcast::Sender<AgentEvent>,
    events: Arc<PtyEventPublisher>,
    legacy_tool: Option<CliTool>,
) {
    let mut vte_parser = VteParser::new();
    let rate_limit_detector = legacy_tool.map(RateLimitDetector::new_for_tool);
    let mut pipeline = legacy_tool.map(create_pipeline);
    let submitter = legacy_tool.map(create_submitter);
    let mut startup_done = false;
    let mut startup_input_suppressed = false;
    let mut output_closed = false;
    let mut observed_exit: Option<(u32, Instant)> = None;

    loop {
        if output_closed {
            let exit_code = pty.lock().ok().and_then(|mut guard| guard.try_exit_code());
            if let Some(code) = exit_code {
                let code = code as i32;
                let _ = tx.send(AgentEvent::Exited { code });
                events.publish(PtyEvent::Exited { code });
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        let exit_code = pty.lock().ok().and_then(|mut guard| guard.try_exit_code());
        if let Some(code) = exit_code {
            let (_, quiet_since) = observed_exit.get_or_insert((code, Instant::now()));
            if quiet_since.elapsed() >= Duration::from_millis(PTY_POST_EXIT_DRAIN_QUIET_MS) {
                let queue_closed = pty
                    .lock()
                    .ok()
                    .is_some_and(|guard| guard.close_output_if_empty());
                if queue_closed {
                    let code = code as i32;
                    let _ = tx.send(AgentEvent::Exited { code });
                    events.publish(PtyEvent::Exited { code });
                    break;
                }
            }
        }

        let receive = match pty.lock() {
            Ok(guard) => guard.try_recv_result(),
            Err(_) => break,
        };
        let raw = match receive {
            Ok(PtyReadEvent::Output(raw)) => {
                if let Some((_, quiet_since)) = observed_exit.as_mut() {
                    *quiet_since = Instant::now();
                }
                raw
            }
            Ok(PtyReadEvent::Eof) => {
                output_closed = true;
                continue;
            }
            Ok(PtyReadEvent::Error(message)) => {
                let _ = tx.send(AgentEvent::Error {
                    message: format!("PTY reader failed: {message}"),
                });
                events.publish(PtyEvent::ReaderError { message });
                if let Ok(mut guard) = pty.lock() {
                    let _ = guard.kill();
                }
                output_closed = true;
                continue;
            }
            Err(TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(TryRecvError::Disconnected) => {
                let message = "PTY reader channel closed without a terminal event".to_owned();
                let _ = tx.send(AgentEvent::Error {
                    message: message.clone(),
                });
                events.publish(PtyEvent::ReaderError { message });
                if let Ok(mut guard) = pty.lock() {
                    let _ = guard.kill();
                }
                output_closed = true;
                continue;
            }
        };

        // Broadcast raw PTY bytes (for vt100 screen emulation by consumers).
        // Keep as Vec<u8> — never round-trip through String::from_utf8_lossy
        // because that destroys multi-byte UTF-8 sequences split across reads.
        let _ = tx.send(AgentEvent::PtyRaw { data: raw.clone() });
        events.publish(PtyEvent::Output(raw.clone()));

        let Some(pipeline) = pipeline.as_mut() else {
            continue;
        };

        // For text analysis (rate limit detection, classification) we need a
        // String. Lossy conversion is acceptable here because this branch only
        // feeds the heuristic ANSI/text parsers, not the vt100 grid.
        let raw_str = String::from_utf8_lossy(&raw).to_string();

        // Strip ANSI for text analysis
        let cleaned = vte_parser.parse(&raw_str);

        // Rate limit detection
        if let Some(rl_info) = rate_limit_detector
            .as_ref()
            .and_then(|detector| detector.detect(&cleaned))
        {
            let _ = tx.send(AgentEvent::RateLimit(rl_info));
        }

        // Classification pipeline
        let messages = pipeline.process(&raw_str);
        for msg in messages {
            match msg.class {
                MessageClass::PromptReady => {
                    let _ = tx.send(AgentEvent::PtyReady);
                }
                MessageClass::ToolApproval => {
                    let tool_name = msg
                        .metadata
                        .tool_name
                        .clone()
                        .unwrap_or_else(|| "unknown".into());
                    let _ = tx.send(AgentEvent::PtyToolApproval {
                        tool_name,
                        description: None,
                    });
                }
                _ => {}
            }
            let _ = tx.send(AgentEvent::PtyParsed(msg));
        }

        // Startup sequence handling
        if !startup_done {
            let Some(submitter) = submitter.as_ref() else {
                continue;
            };
            let action = submitter.handle_startup(&cleaned);
            match action {
                StartupAction::Ready => {
                    startup_done = true;
                }
                StartupAction::SendInput(_) => {
                    if !startup_input_suppressed {
                        startup_input_suppressed = true;
                        let message =
                            "automatic startup input was suppressed; operator action is required"
                                .to_owned();
                        let _ = tx.send(AgentEvent::Error {
                            message: message.clone(),
                        });
                        events.publish(PtyEvent::OperatorActionRequired { message });
                    }
                }
                StartupAction::Waiting => {}
            }
        }
    }
}

/// Generate a simple UUID-like session ID.
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("pty-{:x}", t)
}
