//! Native effect execution for gate4agent control-plane sessions.

mod efficiency;
mod provider_supervisor;

pub use efficiency::ShellEfficiencyFacts;
// Re-exported so a caller of `ShellEfficiencyFacts::record_foreground_probe`
// (e.g. `gate4agent-runtime-native`, which does not itself depend on the
// `gate4agent` crate) can name the type of the value it is handing in
// without picking up a new dependency edge for it.
pub use gate4agent::pty::ForegroundProbeTiming;
pub use provider_supervisor::{
    NativeProviderExecutor, NativeProviderExit, NativeProviderOperation,
    NativeProviderOperationError, NativeProviderResultPoll, PhysicalExitAck,
    ProviderOperationKey, ProviderOperationSnapshot, ProviderSupervisor,
    ProviderSupervisorBuildError, ProviderSupervisorFault, ProviderSupervisorFaultKind,
    ProviderStopCause, ProviderSupervisorSnapshot, ProviderSupervisorState,
    ProviderSupervisorTick, DEFAULT_PROVIDER_STOP_GRACE,
    MAX_PROVIDER_FORCE_STOP_ATTEMPTS, MAX_PROVIDER_STOP_SIGNAL_ATTEMPTS,
    MAX_PROVIDER_SUPERVISOR_EVENTS, MAX_PROVIDER_SUPERVISOR_OPERATIONS,
    MAX_PROVIDER_SUPERVISOR_OUTCOMES_PER_TICK, MAX_PROVIDER_SUPERVISOR_TOMBSTONES,
    MAX_PROVIDER_SUPERVISOR_WORK_PER_TICK,
};

use gate4agent::acp::protocol::SessionMode;
use gate4agent::agent::{is_agent_foreground_wrapper, is_expected_agent_process, ReadinessStatus};
use gate4agent::pty::cli::codex::strip_ansi_codes;
use gate4agent::pty::cli::{create_pipeline, ClassificationPipeline, MessageClass, ParsedMessage};
use gate4agent::pty::event::PtyMouseProtocolEncoding;
use gate4agent::pty::{
    PtyAttachment, PtyEvent, PtyEventEnvelope, PtyEventReceiver, PtyForegroundObservation,
    PtyReplayCursor, PtySession, PtyTerminalSnapshot, RateLimitDetector, VteParser,
};
use gate4agent::{
    AcpSession, AcpSessionOptions, AgentEvent, CliTool, HostDecisionAuthority, HostPolicy,
    HostRequestDecision, LaunchRequest, OperatorPermissionChoice, PipeProcessOptions,
    PipeSession, PromptFraming,
    ReadinessIntent, ReadinessPermit, ReadinessTracker, RpcId, RuntimePlatform, SessionConfig,
};
use gate4agent_adapters::{
    build_resume_plan_for_identity, builtin_adapter_registry, AdapterRuntimeRegistry,
    CodexPtySessionIdentityExtractor, KimiPtySessionIdentityExtractor, OneShotSessionPersistence,
};
use gate4agent_catalog::{approval_level_args, AgentRegistry, AgentSpec, EnvMutation};
use gate4agent_shell_one_shot::NativeOneShotSession;
use gate4agent_types::{
    AdapterFamily, AgentCommand, AgentId, AgentInstanceId, ApprovalLevel, CapabilityProbeFailure,
    ContextWindowUsage as ProviderContextWindowUsage, ControlEffect,
    ControlObservation, EffectEnvelope, ForegroundProcess, ForegroundProcessKind,
    ForegroundRequirement, HostDecisionAuthority as ProviderHostDecisionAuthority,
    HostRequestDecision as ProviderHostRequestDecision, InputAction, ObservationEnvelope,
    OperationId, OperatorGateInput,
    OperatorGateKind, OperatorGateOption, OperatorGateOptionSemantics, OperatorGateState,
    OperatorGateSubject, PipeProtocol,
    PreparedInputKind, PromptPayload, ProviderAvailableCommand, ProviderConfigChoice,
    ProviderConfigOption, ProviderConfigOptionKind, ProviderEvent, ProviderInteractionKind,
    ProviderInteractionResponse, ProviderInteractionTarget, ProviderModeInfo, ProviderPlanPriority,
    ProviderPlanStatus, ProviderPlanStep,
    ProviderRateLimitKind, ProviderRuntimeCapability, ProviderRuntimePolicy,
    ProviderSessionIdentity, ProviderSessionKey,
    ProviderSource, PtyScreenState, ResumeLaunchRequest, SessionGeneration, StartRequest,
    TerminalFrame, TerminalMouseProtocolEncoding, TerminalSize, TokenUsage, TransportKind,
    CONTROL_PROTOCOL_VERSION, OPERATOR_GATE_OPTIONS_MAX, WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use uuid::Uuid;

const INSTANCE_LAUNCH_ARGS_MAX: usize = 128;
const INSTANCE_LAUNCH_ARG_MAX_BYTES: usize = 65_536;
const INSTANCE_LAUNCH_ARGS_TOTAL_MAX_BYTES: usize = 262_144;
const RESERVED_CLAUDE_LAUNCH_FLAGS: &[&str] = &[
    "--continue",
    "--print",
    "--prompt",
    "--prompt-interactive",
    "--resume",
    "--session-id",
    "-c",
    "-p",
    "-r",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NativeSessionKey {
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

struct NativeSpawnRequest {
    agent_id: AgentId,
    transport: TransportKind,
    request: StartRequest,
    runtime_policy: ProviderRuntimePolicy,
    launch_extra_args: Vec<OsString>,
    instance_extra_args: Vec<OsString>,
    resumed_provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
    one_shot_session_persistence: OneShotSessionPersistence,
}

struct OwnedPtySession {
    session: PtySession,
    spawn_operation_id: OperationId,
    last_terminal_sequence: u64,
    terminal_stale_published: bool,
    runtime_policy: ProviderRuntimePolicy,
    provider: Option<OwnedPtyProvider>,
    /// Set once at the spawn site and never mutated -- this is what lets
    /// `reclassify_foreground` resolve the session's `AgentSpec` from
    /// `NativeEffectShell::catalog` without holding a borrow of `session`
    /// across the same loop iteration it awaits `observe_foreground` on.
    agent_id: AgentId,
    last_screen_gate: Option<OperatorGateState>,
    last_screen_failure: Option<&'static str>,
    last_foreground_verdict: Option<ForegroundVerdict>,
    last_screen_state: PtyScreenState,
    /// Whether this generation's merged state has ever been `Ready` WITH
    /// something on screen. Text crash/missing-command markers are only
    /// trustworthy before this flips -- see the gate in
    /// `collect_terminal_frames`. The screen-content half is load-bearing:
    /// `Ready` is proved by foreground process identity and so arrives
    /// within a frame or two of spawn, while the terminal is still blank,
    /// and arming on a blank frame retires the failure detector before any
    /// failure text can exist. Never reset in place; a new generation gets
    /// a fresh `OwnedPtySession`, so `false` is simply this field's initial
    /// value at the spawn site.
    ever_reached_ready: bool,
    /// Whether any frame of this generation has carried non-blank screen
    /// text yet. Read by the process-only path in `reclassify_foreground`,
    /// which has no snapshot of its own to test and would otherwise arm
    /// `ever_reached_ready` on a blank screen.
    screen_had_content: bool,
    /// `None` means disarmed -- the session reached `Ready` and stays
    /// unprobed until its text disagrees again. See
    /// `NativeEffectShell::reclassify_foreground` for the cadence this
    /// drives.
    next_foreground_probe: Option<Instant>,
}

struct OwnedPtyProvider {
    source: ProviderSource,
    receiver: PtyEventReceiver,
    replay: VecDeque<PtyEventEnvelope>,
    pending_events: VecDeque<ProviderEvent>,
    utf8: Utf8ChunkDecoder,
    pipeline: Mutex<ClassificationPipeline>,
    rate_limits: RateLimitFeed,
    kimi_identity: Option<KimiPtySessionIdentityExtractor>,
    semantic_events: bool,
    provider_session_started: bool,
    next_provider_sequence: u64,
}

/// Buffers ANSI-stripped PTY output across output chunks before running
/// rate-limit detection over it.
///
/// PTY reads are chunked arbitrarily -- neither the OS nor this crate's
/// own reader loop promises "one call per terminal row" or even "one call
/// per escape sequence". Two separate problems follow from that, and this
/// type exists to solve both rather than hoping a caller's chunk
/// boundaries are ever polite:
///
/// - An ANSI escape sequence split across two chunks must still be
///   recognized as one escape, not leak its tail bytes into the visible
///   text as a false line-break in the middle of `\x1b[m`. `VteParser`
///   already solves this -- its own `vte::Parser` state machine persists
///   across calls, so an escape completes correctly regardless of where
///   a chunk boundary falls -- this type just keeps ONE `VteParser` alive
///   for the provider's whole lifetime instead of a fresh, stateless one
///   per chunk (which would forget an escape's first half).
/// - A LOGICAL LINE (e.g. codex's `/status` quota-state row) can itself
///   be split across chunks at a point that is not inside any escape.
///   `VteParser::parse` only returns the printable text produced by the
///   bytes handed to THAT call, so two separate calls can each return one
///   half of the same row, and neither half alone matches the quota-state
///   pattern. This type accumulates cleaned text across calls and runs
///   detection over everything seen so far, keeping whatever has not yet
///   been confirmed complete (the tail after the last newline) instead of
///   discarding a still-forming row.
struct RateLimitFeed {
    detector: RateLimitDetector,
    vte: VteParser,
    buffer: String,
}

impl RateLimitFeed {
    fn new_for_tool(tool: CliTool) -> Self {
        Self {
            detector: RateLimitDetector::new_for_tool(tool),
            vte: VteParser::new(),
            buffer: String::new(),
        }
    }

    /// Strip ANSI from one more raw PTY chunk, fold it into the pending
    /// line buffer, and run detection over everything accumulated so far
    /// -- not just the newest chunk, since a match can straddle a chunk
    /// boundary. Lines already terminated by `\n` are then dropped from
    /// the buffer regardless of whether they matched: a later chunk
    /// cannot retroactively extend a row that has already ended. The
    /// unterminated tail (a row still in progress) is always kept -- but
    /// see `RATE_LIMIT_FEED_BUFFER_MAX_BYTES` for what happens when that
    /// tail itself grows unreasonably large.
    fn detect(&mut self, raw: &str) -> Option<gate4agent::core::types::RateLimitInfo> {
        let cleaned = self.vte.parse(raw);
        self.buffer.push_str(&cleaned);
        let info = self.detector.detect(&self.buffer);
        if let Some(last_newline) = self.buffer.rfind('\n') {
            self.buffer.drain(..=last_newline);
        } else if self.buffer.len() > RATE_LIMIT_FEED_BUFFER_MAX_BYTES {
            // A full-screen redraw (codex's main screen, not `/status`)
            // can run for a long time using only cursor-addressing escapes
            // and never emit a `\n` at all, so "no newline yet" cannot be
            // trusted to mean "a row is still in progress" indefinitely.
            // No real quota-state or refusal line approaches this length;
            // past it, keep only the tail so an indefinitely long redraw
            // cannot grow this buffer for the life of the session.
            let mut cut = self.buffer.len() - RATE_LIMIT_FEED_BUFFER_MAX_BYTES;
            while !self.buffer.is_char_boundary(cut) {
                cut += 1;
            }
            self.buffer.drain(..cut);
        }
        info
    }
}

/// Well past the longest quota-state or refusal line this detector
/// recognizes (all under 200 bytes); see `RateLimitFeed::detect`.
const RATE_LIMIT_FEED_BUFFER_MAX_BYTES: usize = 8192;

#[derive(Default)]
struct Utf8ChunkDecoder {
    pending: Vec<u8>,
}

impl Utf8ChunkDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut decoded = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    decoded.push_str(text);
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        let text = std::str::from_utf8(&self.pending[..valid])
                            .expect("validated UTF-8 prefix");
                        decoded.push_str(text);
                        self.pending.drain(..valid);
                        continue;
                    }
                    let Some(invalid) = error.error_len() else {
                        break;
                    };
                    decoded.push('\u{fffd}');
                    self.pending.drain(..invalid.min(self.pending.len()));
                }
            }
        }
        decoded
    }

    fn clear(&mut self) {
        self.pending.clear();
    }
}

struct OwnedProviderSession<S> {
    source: ProviderSource,
    session: S,
    events: broadcast::Receiver<AgentEvent>,
    pending_events: VecDeque<AgentEvent>,
    next_provider_sequence: u64,
    observed_exit_code: Option<i32>,
    runtime_policy: ProviderRuntimePolicy,
}

/// Executes native effects and returns exactly one completion observation for
/// each accepted effect. Logical lifecycle state remains owned by the engine.
pub struct NativeEffectShell {
    catalog: AgentRegistry,
    legacy_adapters: AdapterRuntimeRegistry<CliTool>,
    pty_sessions: BTreeMap<NativeSessionKey, OwnedPtySession>,
    pipe_sessions: BTreeMap<NativeSessionKey, OwnedProviderSession<PipeSession>>,
    one_shot_sessions: BTreeMap<NativeSessionKey, OwnedProviderSession<NativeOneShotSession>>,
    acp_sessions: BTreeMap<NativeSessionKey, OwnedProviderSession<AcpSession>>,
    pending_observations: VecDeque<ObservationEnvelope>,
    /// Plain efficiency facts from `collect_terminal_frames` and
    /// `reclassify_foreground` -- see `ShellEfficiencyFacts`'s own doc
    /// comment for why this crate stops at plain facts rather than
    /// computing a distribution itself.
    efficiency_facts: ShellEfficiencyFacts,
}

impl NativeEffectShell {
    pub fn new(catalog: AgentRegistry) -> Self {
        Self::new_with_runtime_adapters(catalog, builtin_legacy_adapter_runtimes())
    }

    /// Builds a native shell with consumer-provided compatibility runtimes.
    ///
    /// The runtime registry is deliberately separate from the declarative
    /// catalog: both the adapter family and revision must resolve before a
    /// process is spawned.
    pub fn new_with_runtime_adapters(
        catalog: AgentRegistry,
        legacy_adapters: AdapterRuntimeRegistry<CliTool>,
    ) -> Self {
        Self {
            catalog,
            legacy_adapters,
            pty_sessions: BTreeMap::new(),
            pipe_sessions: BTreeMap::new(),
            one_shot_sessions: BTreeMap::new(),
            acp_sessions: BTreeMap::new(),
            pending_observations: VecDeque::new(),
            efficiency_facts: ShellEfficiencyFacts::default(),
        }
    }

    /// Hand the caller everything `collect_terminal_frames` and
    /// `reclassify_foreground` recorded since the last call, and reset the
    /// facts back to empty. Called once per worker-loop iteration by
    /// `gate4agent-runtime-native::publish_shell_observations`, the only
    /// place with somewhere to fold these into a distribution.
    pub fn take_efficiency_facts(&mut self) -> ShellEfficiencyFacts {
        self.efficiency_facts.take()
    }

    pub fn active_session_count(&self) -> usize {
        self.pty_sessions.len()
            + self.pipe_sessions.len()
            + self.one_shot_sessions.len()
            + self.acp_sessions.len()
    }

    pub fn spawn_operation_id(&self, key: NativeSessionKey) -> Option<OperationId> {
        self.pty_sessions
            .get(&key)
            .map(|owned| owned.spawn_operation_id)
    }

    pub fn terminal_snapshot(&self, key: NativeSessionKey) -> Result<PtyTerminalSnapshot, String> {
        self.pty_sessions
            .get(&key)
            .ok_or_else(|| missing_session_message(key))?
            .session
            .terminal_snapshot()
            .map_err(|error| error.to_string())
    }

    pub async fn execute(&mut self, envelope: EffectEnvelope) -> ObservationEnvelope {
        self.execute_with_environment(envelope, Vec::new()).await
    }

    /// Backward-compatible name for PTY-only callers. OneShot launches now
    /// receive the same shell-owned mutations.
    pub async fn execute_with_pty_env(
        &mut self,
        envelope: EffectEnvelope,
        pty_env: Vec<EnvMutation>,
    ) -> ObservationEnvelope {
        self.execute_with_environment(envelope, pty_env).await
    }

    /// Execute an effect with shell-owned environment injected only into a
    /// newly spawned provider process. The canonical start request cannot set
    /// these authority variables itself.
    pub async fn execute_with_environment(
        &mut self,
        envelope: EffectEnvelope,
        environment: Vec<EnvMutation>,
    ) -> ObservationEnvelope {
        self.execute_with_launch_overlay(envelope, environment, Vec::new())
            .await
    }

    /// Execute an effect with host-only environment and optional PTY argv
    /// applied only to a newly spawned provider process.
    pub async fn execute_with_launch_overlay(
        &mut self,
        envelope: EffectEnvelope,
        environment: Vec<EnvMutation>,
        extra_args: Vec<OsString>,
    ) -> ObservationEnvelope {
        self.execute_with_launch_overlay_and_persistence(
            envelope,
            environment,
            extra_args,
            OneShotSessionPersistence::Ephemeral,
        )
        .await
    }

    /// Execute an effect with host-only launch mutations and an explicit
    /// OneShotText session persistence policy.
    pub async fn execute_with_launch_overlay_and_persistence(
        &mut self,
        envelope: EffectEnvelope,
        environment: Vec<EnvMutation>,
        extra_args: Vec<OsString>,
        one_shot_session_persistence: OneShotSessionPersistence,
    ) -> ObservationEnvelope {
        self.execute_with_launch_context(
            envelope,
            environment,
            extra_args,
            one_shot_session_persistence,
        )
        .await
    }

    pub async fn execute_with_launch_context(
        &mut self,
        envelope: EffectEnvelope,
        environment: Vec<EnvMutation>,
        extra_args: Vec<OsString>,
        one_shot_session_persistence: OneShotSessionPersistence,
    ) -> ObservationEnvelope {
        let EffectEnvelope {
            protocol_version,
            operation_id,
            instance_id,
            generation,
            effect,
        } = envelope;
        let key = NativeSessionKey {
            instance_id,
            generation,
        };

        let observation = if protocol_version != CONTROL_PROTOCOL_VERSION {
            effect_failure(
                &effect,
                format!(
                    "effect protocol version {protocol_version} is unsupported; expected {CONTROL_PROTOCOL_VERSION}"
                ),
            )
        } else {
            match effect {
                ControlEffect::Spawn {
                    agent_id,
                    transport,
                    runtime_policy,
                    request,
                } => {
                    self.spawn_native(
                        key,
                        operation_id,
                        NativeSpawnRequest {
                            agent_id,
                            transport,
                            request,
                            runtime_policy,
                            launch_extra_args: Vec::new(),
                            instance_extra_args: extra_args,
                            resumed_provider_session: None,
                            one_shot_session_persistence,
                        },
                        environment,
                    )
                    .await
                }
                ControlEffect::SpawnResume {
                    agent_id,
                    transport,
                    provider_session,
                    runtime_policy,
                    request,
                } => {
                    self.spawn_resume(
                        key,
                        operation_id,
                        agent_id,
                        transport,
                        provider_session,
                        runtime_policy,
                        request,
                        environment,
                        extra_args,
                        one_shot_session_persistence,
                    )
                    .await
                }
                ControlEffect::Stop { force } => self.stop_native(key, force).await,
                ControlEffect::WriteInput {
                    input,
                    required_foreground,
                } => match self.pty_sessions.get(&key) {
                    Some(owned) => match required_foreground {
                        ForegroundRequirement::Any
                            if matches!(
                                input.kind(),
                                PreparedInputKind::TerminalText
                                    | PreparedInputKind::TerminalBytes
                                    | PreparedInputKind::TerminalControl
                            ) =>
                        {
                            match owned.session.send_terminal_input(input).await {
                                Ok(()) => ControlObservation::InputCompleted,
                                Err(error) => ControlObservation::InputFailed {
                                    message: error.to_string(),
                                },
                            }
                        }
                        ForegroundRequirement::Shell
                            if input.kind() == PreparedInputKind::ShellCommand =>
                        {
                            if !owned.runtime_policy.semantic_readiness {
                                ControlObservation::InputFailed {
                                    message: "semantic shell input is not admitted by the provider runtime policy"
                                        .to_owned(),
                                }
                            } else {
                                match owned.session.send_shell_input(input).await {
                                    Ok(()) => ControlObservation::InputCompleted,
                                    Err(error) => ControlObservation::InputFailed {
                                        message: error.to_string(),
                                    },
                                }
                            }
                        }
                        ForegroundRequirement::Agent { agent_id }
                            if matches!(
                                input.kind(),
                                PreparedInputKind::InsertDraft
                                    | PreparedInputKind::SubmitPrompt
                                    | PreparedInputKind::AgentCommand
                            ) && &agent_id == owned.session.agent_id() =>
                        {
                            if !owned.runtime_policy.semantic_readiness
                                || !owned.runtime_policy.structured_prompt
                            {
                                return completion_observation(
                                    operation_id,
                                    instance_id,
                                    generation,
                                    ControlObservation::InputFailed {
                                        message: "semantic input is not admitted by the provider runtime policy"
                                            .to_owned(),
                                    },
                                );
                            }
                            let intent = match input.kind() {
                                PreparedInputKind::InsertDraft
                                | PreparedInputKind::AgentCommand => ReadinessIntent::DraftPaste,
                                PreparedInputKind::SubmitPrompt => ReadinessIntent::FollowupPrompt,
                                PreparedInputKind::ShellCommand
                                | PreparedInputKind::TerminalText
                                | PreparedInputKind::TerminalBytes
                                | PreparedInputKind::TerminalControl => unreachable!(),
                            };
                            let Some(spec) = self.catalog.get(&agent_id).cloned() else {
                                return completion_observation(
                                    operation_id,
                                    instance_id,
                                    generation,
                                    ControlObservation::InputFailed {
                                        message: "session agent disappeared from native catalog"
                                            .to_owned(),
                                    },
                                );
                            };
                            match wait_for_readiness(&owned.session, &spec, intent, false).await {
                                Ok(permit) => {
                                    let result = if input.kind() == PreparedInputKind::AgentCommand
                                    {
                                        owned.session.send_agent_command_input(input, permit).await
                                    } else {
                                        owned.session.send_prepared_input(input, permit).await
                                    };
                                    match result {
                                        Ok(()) => ControlObservation::InputCompleted,
                                        Err(error) => ControlObservation::InputFailed {
                                            message: error.to_string(),
                                        },
                                    }
                                }
                                Err(message) => ControlObservation::InputFailed { message },
                            }
                        }
                        required_foreground => ControlObservation::InputFailed {
                            message: format!(
                                "prepared input kind {:?} does not satisfy route {:?}",
                                input.kind(),
                                required_foreground
                            ),
                        },
                    },
                    None => ControlObservation::InputFailed {
                        message: "typed PTY input requires a PTY session".to_owned(),
                    },
                },
                ControlEffect::SubmitPrompt { prompt } => match self.acp_sessions.get(&key) {
                    Some(owned) if !owned.runtime_policy.structured_prompt => {
                        ControlObservation::InputFailed {
                            message: "structured prompt is not admitted by the provider runtime policy"
                                .to_owned(),
                        }
                    }
                    Some(owned) => match owned.session.start_prompt(&prompt).await {
                        Ok(()) => ControlObservation::InputCompleted,
                        Err(error) => ControlObservation::InputFailed {
                            message: error.to_string(),
                        },
                    },
                    None => ControlObservation::InputFailed {
                        message: "semantic follow-up prompts require an ACP session".to_owned(),
                    },
                },
                ControlEffect::Interrupt => match self.acp_sessions.get(&key) {
                    Some(owned) => match owned.session.cancel().await {
                        Ok(()) => ControlObservation::InputCompleted,
                        Err(error) => ControlObservation::InputFailed {
                            message: error.to_string(),
                        },
                    },
                    None => ControlObservation::InputFailed {
                        message: "semantic interrupt requires an ACP session".to_owned(),
                    },
                },
                ControlEffect::ResolveInteraction { target, response } => {
                    resolve_acp_interaction_observation(
                        self.acp_sessions.get(&key).map(|owned| &owned.session),
                        target,
                        response,
                    )
                }
                ControlEffect::SetSessionMode { mode_id } => {
                    set_acp_session_mode_observation(
                        self.acp_sessions.get(&key).map(|owned| &owned.session),
                        mode_id,
                    )
                    .await
                }
                ControlEffect::SetSessionConfigOption {
                    option_id,
                    value_json,
                } => {
                    set_acp_session_config_option_observation(
                        self.acp_sessions.get(&key).map(|owned| &owned.session),
                        option_id,
                        value_json,
                    )
                    .await
                }
                ControlEffect::SetSessionModel { model_id } => {
                    set_acp_session_model_observation(
                        self.acp_sessions.get(&key).map(|owned| &owned.session),
                        model_id,
                    )
                }
                ControlEffect::Resize { size } if !size.is_valid() => {
                    ControlObservation::ResizeFailed {
                        message: "terminal size is outside the supported range".to_owned(),
                    }
                }
                ControlEffect::Resize { size } => match self.pty_sessions.get(&key) {
                    Some(owned) => match owned.session.resize(size.rows, size.columns).await {
                        Ok(()) => ControlObservation::ResizeCompleted { size },
                        Err(error) => ControlObservation::ResizeFailed {
                            message: error.to_string(),
                        },
                    },
                    None => ControlObservation::ResizeFailed {
                        message: "terminal resize requires a PTY session".to_owned(),
                    },
                },
                ControlEffect::ObserveForeground => match self.pty_sessions.get(&key) {
                    Some(owned) => match owned.session.observe_foreground().await {
                        Ok(observation) => ControlObservation::ForegroundObserved {
                            process: canonical_foreground(
                                owned.session.agent_id().clone(),
                                &observation,
                            ),
                        },
                        Err(error) => ControlObservation::ForegroundFailed {
                            message: error.to_string(),
                        },
                    },
                    None => ControlObservation::ForegroundFailed {
                        message: "foreground observation requires a PTY session".to_owned(),
                    },
                },
                ControlEffect::ProbeCapabilities { .. } => {
                    ControlObservation::CapabilityProbeFailed {
                        failure: CapabilityProbeFailure::ExecutorUnavailable,
                    }
                }
                ControlEffect::DiscoverHistory { .. } | ControlEffect::LoadHistory { .. } => {
                    ControlObservation::HistoryFailed {
                        message: "history effects require the dedicated native history authority"
                            .to_owned(),
                    }
                }
                ControlEffect::AuthorizeResume { .. } => ControlObservation::ResumeFailed {
                    message: "resume authorization requires the dedicated native authority"
                        .to_owned(),
                },
            }
        };

        completion_observation(operation_id, instance_id, generation, observation)
    }

    async fn spawn_native(
        &mut self,
        key: NativeSessionKey,
        operation_id: OperationId,
        spawn: NativeSpawnRequest,
        pty_env: Vec<EnvMutation>,
    ) -> ControlObservation {
        let NativeSpawnRequest {
            agent_id,
            transport,
            request,
            runtime_policy,
            mut launch_extra_args,
            instance_extra_args,
            resumed_provider_session,
            one_shot_session_persistence,
        } = spawn;
        if let Err(message) =
            validate_instance_launch_arguments(&agent_id, transport, &instance_extra_args)
        {
            return ControlObservation::SpawnFailed { message };
        }
        if let Err(message) = validate_spawn_runtime_policy(
            runtime_policy,
            transport,
            request.initial_prompt.is_some(),
            resumed_provider_session.is_some(),
        ) {
            return ControlObservation::SpawnFailed { message };
        }
        if self.session_exists(key) {
            return ControlObservation::SpawnFailed {
                message: format!(
                    "native session {:?}/{:?} already exists",
                    key.instance_id, key.generation
                ),
            };
        }
        if !request.terminal_size.is_valid() {
            return ControlObservation::SpawnFailed {
                message: "terminal size is outside the supported range".to_owned(),
            };
        }
        if request.working_directory.is_empty()
            || request.working_directory.len() > WORKING_DIRECTORY_MAX_BYTES
            || request.working_directory.contains('\0')
        {
            return ControlObservation::SpawnFailed {
                message: "working directory is invalid".to_owned(),
            };
        }
        let Some(spec) = self.catalog.get(&agent_id).cloned() else {
            return ControlObservation::SpawnFailed {
                message: format!("agent '{agent_id}' is absent from native catalog"),
            };
        };
        let working_dir = PathBuf::from(&request.working_directory);

        match transport {
            TransportKind::Pty if !spec.capabilities.transports.pty => {
                ControlObservation::SpawnFailed {
                    message: format!("agent '{agent_id}' does not support PTY transport"),
                }
            }
            TransportKind::Pty => {
                // Only the interactive PTY path renders anything to color;
                // the Pipe/OneShotText arm below spawns a plain pipe with
                // no terminal device behind it, so `pty_env` is left as the
                // caller passed it there.
                let pty_env = with_pty_terminal_capability_defaults(pty_env);
                let fresh_provider_session = prepare_fresh_pty_provider_session(
                    spec.capabilities.transports.pty_adapter.as_ref(),
                    resumed_provider_session.is_some(),
                    runtime_policy.provider_session_identity,
                    &mut launch_extra_args,
                );
                launch_extra_args.extend(instance_extra_args);
                let mut authoritative_provider_session = resumed_provider_session
                    .clone()
                    .or(fresh_provider_session);
                let probe_kimi_identity = should_probe_pty_identity(
                    runtime_policy,
                    spec.capabilities.transports.pty_adapter.as_ref(),
                    authoritative_provider_session.is_some(),
                    "kimi",
                );
                let probe_codex_identity = should_probe_pty_identity(
                    runtime_policy,
                    spec.capabilities.transports.pty_adapter.as_ref(),
                    authoritative_provider_session.is_some(),
                    "codex",
                );
                // This is the last point before the OS-level PTY spawn where
                // the program and its arguments are still plain, structured
                // data (`spec.launch.program`/`fixed_args` are catalog
                // constants; `launch_extra_args` is the dynamic, per-session
                // portion). Provider secrets are environment-only by this
                // repo's own convention (never argv, see `gate4agent/CLAUDE.md`),
                // so `fixed_args` needs no redaction; `launch_extra_args`
                // still gets a defensive per-token credential-shape check
                // in case a provider CLI's own argv convention differs.
                // The initial prompt itself is arbitrary-length user text,
                // not an operational argument, so only its presence is
                // logged, never its contents.
                tracing::info!(
                    agent_id = %agent_id,
                    instance_id = ?key.instance_id,
                    generation = ?key.generation,
                    terminal_size = ?request.terminal_size,
                    program = %spec.launch.program,
                    fixed_args = ?spec.launch.fixed_args,
                    extra_args = ?redact_provider_arguments(&launch_extra_args),
                    has_initial_prompt = request.initial_prompt.is_some(),
                    "spawning provider process over a PTY",
                );
                match PtySession::spawn_agent_with_size(
                    &spec,
                    LaunchRequest {
                        working_dir,
                        env: pty_env,
                        platform: RuntimePlatform::current(),
                        prompt: request.initial_prompt,
                        session_options: request.session_options,
                        extra_args: launch_extra_args,
                    },
                    request.terminal_size.rows,
                    request.terminal_size.columns,
                )
                .await
                {
                Ok(mut session) => {
                    if probe_kimi_identity {
                        match probe_fresh_kimi_session_identity(&session, &spec).await {
                            Ok(Some(identity)) => authoritative_provider_session = Some(identity),
                            Ok(None) => {}
                            Err(error) => {
                                let message = match session.shutdown().await {
                                    Ok(_) => error,
                                    Err(shutdown_error) => {
                                        format!("{error}; PTY cleanup failed: {shutdown_error}")
                                    }
                                };
                                return ControlObservation::SpawnFailed { message };
                            }
                        }
                    }
                    if probe_codex_identity {
                        match probe_fresh_codex_session_identity(&session, &spec).await {
                            Ok(Some(identity)) => authoritative_provider_session = Some(identity),
                            Ok(None) => {}
                            Err(error) => {
                                let message = match session.shutdown().await {
                                    Ok(_) => error,
                                    Err(shutdown_error) => {
                                        format!("{error}; PTY cleanup failed: {shutdown_error}")
                                    }
                                };
                                return ControlObservation::SpawnFailed { message };
                            }
                        }
                    }
                    if let Err(error) = deliver_pending_initial_prompt(&mut session, &spec).await {
                        let message = match session.shutdown().await {
                            Ok(_) => error,
                            Err(shutdown_error) => {
                                format!("{error}; PTY cleanup failed: {shutdown_error}")
                            }
                        };
                        return ControlObservation::SpawnFailed { message };
                    }
                    let process_id = session.root_pid();
                    let provider = match spec.capabilities.transports.pty_adapter.as_ref() {
                        _ if !should_attach_pty_provider_stream(runtime_policy) => None,
                        Some(adapter) => {
                            let tool = match self
                                .legacy_adapters
                                .resolve(AdapterFamily::PtySemantic, adapter)
                            {
                                Ok(tool) => *tool,
                                Err(error) => {
                                    let _ = session.shutdown().await;
                                    return ControlObservation::SpawnFailed {
                                        message: error.to_string(),
                                    };
                                }
                            };
                            match session.attach_events(session.beginning_cursor()) {
                                Ok(attachment) => {
                                    let mut pending_events = VecDeque::new();
                                    let mut provider_session_started = false;
                                    if let Some(identity) = authoritative_provider_session {
                                        pending_events.push_back(ProviderEvent::SessionStarted {
                                            session_id: identity.id.clone(),
                                            model: String::new(),
                                            tools: Vec::new(),
                                        });
                                        pending_events.push_back(
                                            ProviderEvent::SessionIdentityObserved { identity },
                                        );
                                        provider_session_started = true;
                                    }
                                    let is_kimi = tool == CliTool::KimiCode;
                                    Some(OwnedPtyProvider {
                                        source: ProviderSource {
                                            family: AdapterFamily::PtySemantic,
                                            binding: adapter.clone(),
                                        },
                                        receiver: attachment.receiver,
                                        replay: attachment.replay.into(),
                                        pending_events,
                                        utf8: Utf8ChunkDecoder::default(),
                                        pipeline: Mutex::new(create_pipeline(tool)),
                                        rate_limits: RateLimitFeed::new_for_tool(tool),
                                        kimi_identity: (runtime_policy.provider_session_identity
                                            && is_kimi
                                            && !provider_session_started)
                                            .then(KimiPtySessionIdentityExtractor::default),
                                        semantic_events: runtime_policy.semantic_readiness,
                                        provider_session_started,
                                        next_provider_sequence: 1,
                                    })
                                }
                                Err(error) => {
                                    let _ = session.shutdown().await;
                                    return ControlObservation::SpawnFailed {
                                        message: error.to_string(),
                                    };
                                }
                            }
                        }
                        None => None,
                    };
                    self.pty_sessions.insert(
                        key,
                        OwnedPtySession {
                            session,
                            spawn_operation_id: operation_id,
                            last_terminal_sequence: 0,
                            terminal_stale_published: false,
                            runtime_policy,
                            provider,
                            agent_id,
                            last_screen_gate: None,
                            last_screen_failure: None,
                            last_foreground_verdict: None,
                            last_screen_state: PtyScreenState::default(),
                            ever_reached_ready: false,
                            screen_had_content: false,
                            // Armed immediately -- the first
                            // `reclassify_foreground` tick after spawn
                            // probes this session right away rather than
                            // waiting a full `FOREGROUND_RECLASSIFY_INTERVAL`.
                            next_foreground_probe: Some(Instant::now()),
                        },
                    );
                    ControlObservation::Spawned { process_id }
                }
                    Err(error) => {
                        let message = error.to_string();
                        tracing::warn!(
                            agent_id = %agent_id,
                            instance_id = ?key.instance_id,
                            generation = ?key.generation,
                            program = %spec.launch.program,
                            fixed_args = ?spec.launch.fixed_args,
                            cause = %message,
                            "provider process failed to start",
                        );
                        ControlObservation::SpawnFailed { message }
                    }
                }
            }
            TransportKind::Pipe => {
                let Some(pipe_spec) = spec.capabilities.transports.pipe.as_ref() else {
                    return ControlObservation::SpawnFailed {
                        message: format!("agent '{agent_id}' does not support Pipe transport"),
                    };
                };
                let prompt = request.initial_prompt.unwrap_or_default();
                if pipe_spec.protocol == PipeProtocol::OneShotText {
                    let Some(binding) = spec.capabilities.adapters.one_shot.as_ref() else {
                        return ControlObservation::SpawnFailed {
                            message: format!(
                                "agent '{agent_id}' does not declare OneShot capability"
                            ),
                        };
                    };
                    if binding != &pipe_spec.adapter {
                        return ControlObservation::SpawnFailed {
                            message: format!(
                                "agent '{agent_id}' has mismatched OneShot transport bindings"
                            ),
                        };
                    }
                    return match NativeOneShotSession::spawn_with_environment_and_persistence(
                        &spec,
                        binding,
                        &prompt,
                        request.session_options.as_ref(),
                        &working_dir,
                        &pty_env,
                        one_shot_session_persistence,
                    )
                    .await
                    {
                        Ok(session) => {
                            let process_id = session.process_id();
                            let events = session.subscribe();
                            self.one_shot_sessions.insert(
                                key,
                                OwnedProviderSession {
                                    source: ProviderSource {
                                        family: AdapterFamily::OneShot,
                                        binding: binding.clone(),
                                    },
                                    session,
                                    events,
                                    pending_events: VecDeque::new(),
                                    next_provider_sequence: 1,
                                    observed_exit_code: None,
                                    runtime_policy,
                                },
                            );
                            ControlObservation::Spawned { process_id }
                        }
                        Err(error) => ControlObservation::SpawnFailed {
                            message: error.to_string(),
                        },
                    };
                }
                let tool = match self
                    .legacy_adapters
                    .resolve(AdapterFamily::Pipe, &pipe_spec.adapter)
                {
                    Ok(tool) => *tool,
                    Err(error) => {
                        return ControlObservation::SpawnFailed {
                            message: error.to_string(),
                        }
                    }
                };
                let source = ProviderSource {
                    family: AdapterFamily::Pipe,
                    binding: pipe_spec.adapter.clone(),
                };
                let config = SessionConfig {
                    tool,
                    working_dir,
                    env_vars: Vec::new(),
                    name: None,
                };
                if resumed_provider_session.is_some() && pipe_spec.launch_override.is_some() {
                    return ControlObservation::SpawnFailed {
                        message: format!(
                            "agent '{agent_id}' cannot resume through a catalog launch override"
                        ),
                    };
                }
                let mut options = PipeProcessOptions::default();
                if let Some(identity) = resumed_provider_session.as_ref() {
                    options.claude.resume_session_id = Some(identity.id.clone());
                }
                let spawned = match pipe_spec.launch_override.as_ref() {
                    Some(launch) => {
                        PipeSession::spawn_with_launch(
                            config,
                            &prompt,
                            launch,
                            pipe_spec.prompt_delivery,
                        )
                        .await
                    }
                    None => {
                        PipeSession::spawn(config, &prompt, options).await
                    }
                };
                match spawned {
                    Ok(session) => {
                        let process_id = session.process_id();
                        let events = session.subscribe();
                        let pending_events = if pipe_spec.protocol == PipeProtocol::SemanticNdjson {
                            VecDeque::from([AgentEvent::SessionStart {
                                session_id: session.session_id().to_owned(),
                                model: String::new(),
                                tools: Vec::new(),
                            }])
                        } else {
                            VecDeque::new()
                        };
                        self.pipe_sessions.insert(
                            key,
                            OwnedProviderSession {
                                source,
                                session,
                                events,
                                pending_events,
                                next_provider_sequence: 1,
                                observed_exit_code: None,
                                runtime_policy,
                            },
                        );
                        ControlObservation::Spawned { process_id }
                    }
                    Err(error) => ControlObservation::SpawnFailed {
                        message: error.to_string(),
                    },
                }
            }
            TransportKind::Acp => {
                let Some(acp_spec) = spec.capabilities.transports.acp else {
                    return ControlObservation::SpawnFailed {
                        message: format!("agent '{agent_id}' does not support ACP transport"),
                    };
                };
                let tool = match self
                    .legacy_adapters
                    .resolve(AdapterFamily::Acp, &acp_spec.adapter)
                {
                    Ok(tool) => *tool,
                    Err(error) => {
                        return ControlObservation::SpawnFailed {
                            message: error.to_string(),
                        }
                    }
                };
                let source = ProviderSource {
                    family: AdapterFamily::Acp,
                    binding: acp_spec.adapter.clone(),
                };
                // ACP is the transport this project opens programmatically
                // -- unlike a PTY, there is no human at a terminal to pick
                // permission flags, so the request's `approval_level` has to
                // steer both the spawned process's own argv
                // (`approval_level_args`, applied inside `AcpSession` only
                // for a provider it spawns as its own binary rather than
                // through an adapter wrapper -- see `src/acp/spawn.rs`) and
                // the host side answering the session's permission requests
                // (`host_policy_for_approval_level`).
                let acp_options = AcpSessionOptions {
                    host_policy: host_policy_for_approval_level(request.approval_level),
                    approval_level_args: approval_level_args(&agent_id, request.approval_level),
                    defer_permission_requests: defers_permission_requests(request.approval_level),
                    ..AcpSessionOptions::default()
                };
                let spawned = match acp_spec.launch_override.as_ref() {
                    Some(launch) => {
                        AcpSession::spawn_with_launch(tool, &working_dir, acp_options, launch)
                            .await
                    }
                    None => AcpSession::spawn(tool, &working_dir, acp_options).await,
                };
                match spawned {
                    Ok(session) => {
                        let process_id = session.process_id();
                        let events = session.subscribe();
                        let session_id = session
                            .acp_session_id()
                            .await
                            .unwrap_or_else(|| session.session_id().to_owned());
                        if let Some(prompt) = request.initial_prompt {
                            if let Err(error) = session.start_prompt(&prompt).await {
                                let _ = session.kill().await;
                                return ControlObservation::SpawnFailed {
                                    message: error.to_string(),
                                };
                            }
                        }
                        self.acp_sessions.insert(
                            key,
                            OwnedProviderSession {
                                source,
                                session,
                                events,
                                pending_events: VecDeque::from([AgentEvent::SessionStart {
                                    session_id,
                                    model: String::new(),
                                    tools: Vec::new(),
                                }]),
                                next_provider_sequence: 1,
                                observed_exit_code: None,
                                runtime_policy,
                            },
                        );
                        ControlObservation::Spawned { process_id }
                    }
                    Err(error) => ControlObservation::SpawnFailed {
                        message: error.to_string(),
                    },
                }
            }
        }
    }

    async fn spawn_resume(
        &mut self,
        key: NativeSessionKey,
        operation_id: OperationId,
        agent_id: AgentId,
        transport: TransportKind,
        provider_session: gate4agent_types::ProviderSessionIdentity,
        runtime_policy: ProviderRuntimePolicy,
        request: ResumeLaunchRequest,
        pty_env: Vec<EnvMutation>,
        instance_extra_args: Vec<OsString>,
        one_shot_session_persistence: OneShotSessionPersistence,
    ) -> ControlObservation {
        if let Err(message) = validate_spawn_runtime_policy(
            runtime_policy,
            transport,
            request.initial_prompt.is_some(),
            true,
        ) {
            return ControlObservation::SpawnFailed { message };
        }
        if let Err(error) = request.validate() {
            return ControlObservation::SpawnFailed {
                message: error.to_string(),
            };
        }
        let Some(spec) = self.catalog.get(&agent_id) else {
            return ControlObservation::SpawnFailed {
                message: format!("agent '{agent_id}' is absent from native catalog"),
            };
        };
        let Some(binding) = spec.capabilities.adapters.resume.as_ref() else {
            return ControlObservation::SpawnFailed {
                message: format!("agent '{agent_id}' does not declare Resume capability"),
            };
        };
        let plan = match build_resume_plan_for_identity(&binding.id, &provider_session) {
            Ok(Some(plan)) => plan,
            Ok(None) => {
                return ControlObservation::SpawnFailed {
                    message: format!("agent '{agent_id}' has no live Resume plan"),
                }
            }
            Err(error) => {
                return ControlObservation::SpawnFailed {
                    message: error.to_string(),
                }
            }
        };
        if transport == TransportKind::Pipe {
            let Some(pipe) = spec.capabilities.transports.pipe.as_ref() else {
                return ControlObservation::SpawnFailed {
                    message: format!("agent '{agent_id}' does not support Pipe transport"),
                };
            };
            if pipe.protocol != PipeProtocol::StructuredJsonl {
                return ControlObservation::SpawnFailed {
                    message: format!(
                        "agent '{agent_id}' does not expose a resumable structured Pipe contract"
                    ),
                };
            }
        }
        let start = StartRequest {
            working_directory: request.working_directory,
            terminal_size: request.terminal_size,
            initial_prompt: request.initial_prompt,
            // A resume reattaches to a provider session that already has a
            // process running with whatever argv it was originally spawned
            // with -- there is no fresh CLI invocation here to apply an
            // approval level to, the same reasoning that leaves
            // `session_options` unset on a resume above.
            session_options: None,
            approval_level: ApprovalLevel::default(),
        };
        self.spawn_native(
            key,
            operation_id,
            NativeSpawnRequest {
                agent_id,
                transport,
                request: start,
                runtime_policy,
                launch_extra_args: if transport == TransportKind::Pty {
                    plan.args.into_iter().map(OsString::from).collect()
                } else {
                    Vec::new()
                },
                instance_extra_args,
                resumed_provider_session: Some(provider_session),
                one_shot_session_persistence,
            },
            pty_env,
        )
        .await
    }

    async fn stop_native(&mut self, key: NativeSessionKey, force: bool) -> ControlObservation {
        if let Some(owned) = self.pty_sessions.remove(&key) {
            let shutdown = owned.session.shutdown().await;
            return match shutdown {
                Ok(outcome) => ControlObservation::StopCompleted {
                    forced: force || outcome.termination.is_some(),
                    exit_code: outcome.exit_code,
                    // The last classification this session ever computed --
                    // stamped through unchanged, same as every other frame;
                    // nothing observes this session again after this point.
                    final_terminal: Some(terminal_frame(outcome.terminal, owned.last_screen_state.clone())),
                },
                Err(error) => {
                    eprintln!(
                        "[gate4agent-shell-native] PTY stop failed for {key:?} (force={force}): {error}",
                    );
                    ControlObservation::StopFailed {
                        message: error.to_string(),
                    }
                }
            };
        }
        if let Some(owned) = self.pipe_sessions.remove(&key) {
            return match owned.session.kill().await {
                Ok(()) => ControlObservation::StopCompleted {
                    forced: true,
                    exit_code: owned.observed_exit_code,
                    final_terminal: None,
                },
                Err(_) if owned.session.reader_finished() => ControlObservation::StopCompleted {
                    forced: false,
                    exit_code: owned.observed_exit_code,
                    final_terminal: None,
                },
                Err(error) => ControlObservation::StopFailed {
                    message: error.to_string(),
                },
            };
        }
        if let Some(mut owned) = self.one_shot_sessions.remove(&key) {
            return match owned.session.kill().await {
                Ok(()) => ControlObservation::StopCompleted {
                    forced: true,
                    exit_code: owned.observed_exit_code,
                    final_terminal: None,
                },
                Err(_) if owned.session.reader_finished() => ControlObservation::StopCompleted {
                    forced: false,
                    exit_code: owned.observed_exit_code,
                    final_terminal: None,
                },
                Err(error) => ControlObservation::StopFailed {
                    message: error.to_string(),
                },
            };
        }
        if let Some(owned) = self.acp_sessions.remove(&key) {
            if !force {
                let _ = owned.session.cancel().await;
            }
            return match owned.session.kill().await {
                Ok(()) => ControlObservation::StopCompleted {
                    forced: true,
                    exit_code: owned.observed_exit_code,
                    final_terminal: None,
                },
                Err(_) if owned.session.reader_finished() => ControlObservation::StopCompleted {
                    forced: false,
                    exit_code: owned.observed_exit_code,
                    final_terminal: None,
                },
                Err(error) => ControlObservation::StopFailed {
                    message: error.to_string(),
                },
            };
        }
        // A kill this function skips entirely -- nothing owns `key` in any
        // transport map -- must say so out loud: silently returning
        // `StopFailed` here left no trail at the point where the skip
        // actually happened, only a message string several layers removed
        // from anyone watching this process's own output.
        eprintln!("[gate4agent-shell-native] stop skipped: no session owns {key:?} in any transport map");
        ControlObservation::StopFailed {
            message: missing_session_message(key),
        }
    }

    fn session_exists(&self, key: NativeSessionKey) -> bool {
        self.pty_sessions.contains_key(&key)
            || self.pipe_sessions.contains_key(&key)
            || self.one_shot_sessions.contains_key(&key)
            || self.acp_sessions.contains_key(&key)
    }

    /// Convert naturally exited PTY children into generation-bound lifecycle
    /// observations. Runtime ticks call this before accepting new commands.
    pub async fn collect_exits(&mut self) -> Vec<ObservationEnvelope> {
        let completed: Vec<_> = self
            .pty_sessions
            .iter()
            .filter_map(|(key, owned)| owned.session.reader_finished().then_some(*key))
            .collect();
        let mut observations = Vec::with_capacity(completed.len());
        for key in completed {
            let owned = self
                .pty_sessions
                .remove(&key)
                .expect("completed key came from the owned session map");
            let last_screen_state = owned.last_screen_state.clone();
            let (exit_code, final_terminal) = match owned.session.shutdown().await {
                Ok(outcome) => (
                    outcome.exit_code,
                    Some(terminal_frame(outcome.terminal, last_screen_state)),
                ),
                Err(_) => (None, None),
            };
            observations.push(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: key.instance_id,
                generation: key.generation,
                observation: ControlObservation::ProcessExited {
                    exit_code,
                    final_terminal,
                },
            });
        }
        collect_provider_exits(&mut self.pipe_sessions, &mut observations, |session| {
            session.reader_finished()
        });
        collect_provider_exits(&mut self.one_shot_sessions, &mut observations, |session| {
            session.reader_finished()
        });
        collect_provider_exits(&mut self.acp_sessions, &mut observations, |session| {
            session.reader_finished()
        });
        observations
    }

    /// Drain normalized provider events without mixing them with replaceable
    /// terminal frames. Broadcast lag is converted into an explicit stale gap.
    pub fn collect_provider_events(&mut self) -> Vec<ObservationEnvelope> {
        let mut observations = self.pending_observations.drain(..).collect::<Vec<_>>();
        for (key, owned) in &mut self.pty_sessions {
            if let Some(provider) = &mut owned.provider {
                drain_pty_provider(*key, provider, &mut observations);
            }
        }
        collect_provider_map(&mut self.pipe_sessions, &mut observations, |_| Vec::new());
        collect_provider_map(&mut self.one_shot_sessions, &mut observations, |_| Vec::new());
        // Retire deferred permission requests whose deadline has passed,
        // BEFORE draining -- so the `RpcIncomingRequest` each expiry emits is
        // picked up by the very same drain rather than waiting a whole tick
        // to be reported.
        //
        // This call is what makes the deadline real. Without it a deferred
        // request has a `deadline` field nobody ever reads, and a session
        // whose operator never answers waits forever on a promise the code
        // makes and never keeps -- the agent blocked on a permission it will
        // never be told about. Deferral is only safe because this runs on
        // every tick; nothing else in the process calls it.
        for owned in self.acp_sessions.values() {
            owned.session.expire_deadlines();
        }
        collect_provider_map(&mut self.acp_sessions, &mut observations, AcpSession::available_modes);
        observations
    }

    /// Capture only changed terminal frames. Snapshot failures become an
    /// explicit stale observation once, until a later successful frame heals it.
    ///
    /// The sequence is checked BEFORE the capture, not after. This runs on
    /// the per-session worker tick (20ms, so ~50 times a second per live
    /// PTY session), and `terminal_state` is not a cheap read: it renders
    /// the visible screen twice and clones the whole `vt100::Screen` to
    /// walk its scrollback row by row. Asking it first and comparing
    /// sequences afterwards meant every idle session rebuilt its entire
    /// screen fifty times a second purely to discover nothing had changed,
    /// and then dropped the result -- with a couple of dozen sessions open
    /// that is the machine's time, all of it wasted. `terminal_sequence`
    /// answers the same question by reading one integer.
    pub fn collect_terminal_frames(&mut self) -> Vec<ObservationEnvelope> {
        let mut observations = Vec::new();
        // Split borrows taken up front: the loop below needs a mutable
        // borrow of `pty_sessions` for its whole body, and `efficiency_facts`
        // is a disjoint field this function also writes to on every branch
        // -- see `ShellEfficiencyFacts`'s own doc comment for why this crate
        // is the one recording facts rather than a distribution.
        let pty_sessions = &mut self.pty_sessions;
        let efficiency_facts = &mut self.efficiency_facts;
        for (key, owned) in pty_sessions {
            if terminal_state_capture_should_skip(
                owned.session.terminal_sequence(),
                owned.last_terminal_sequence,
            ) {
                efficiency_facts.record_terminal_state_skip();
                continue;
            }
            let capture_start = Instant::now();
            let terminal_state = owned.session.terminal_state();
            efficiency_facts.record_terminal_state_capture(capture_start.elapsed());
            match terminal_state {
                Ok(snapshot) if snapshot.sequence > owned.last_terminal_sequence => {
                    owned.last_terminal_sequence = snapshot.sequence;
                    owned.terminal_stale_published = false;

                    // `snapshot.contents` is already in memory for the
                    // `TerminalFrame` below, so both text matchers run for
                    // free here -- no extra syscall, no extra capture.
                    owned.last_screen_gate = startup_operator_gate(&snapshot.contents);
                    // `screen_failure`'s markers are crash SHAPES, and as
                    // raw bytes those are indistinguishable from an agent
                    // choosing to render the same text while explaining or
                    // running someone else's failure. Before this
                    // generation has ever rendered its own UI, nothing else
                    // could have put a crash banner on the screen, so the
                    // marker genuinely means the CLI failed to come up;
                    // once it has been `Ready`, the identical bytes are
                    // ordinary content, not state, so the matcher is not
                    // even run.
                    owned.last_screen_failure =
                        screen_failure_for_generation(&snapshot.contents, owned.ever_reached_ready);
                    let merged = classify_pty_screen_state(
                        owned.last_foreground_verdict.as_ref(),
                        owned.last_screen_gate.as_ref(),
                        owned.last_screen_failure,
                    );
                    // Arm the crash-marker window only on a `Ready` that had
                    // SOMETHING on screen. `Ready` is proved by foreground
                    // process identity, so it lands within a frame or two of
                    // spawn, while the terminal is still blank -- measured at
                    // frame 2 for all four providers. Arming on that blank
                    // frame closed the window before any failure text could
                    // exist: Grok without a key prints `API key required` at
                    // frame 4 and sat at `Ready` forever, because the matcher
                    // for it was never reached. A blank screen is not evidence
                    // the agent came up, so it must not retire the detector.
                    if !snapshot.contents.trim().is_empty() {
                        owned.screen_had_content = true;
                    }
                    if merged == PtyScreenState::Ready && owned.screen_had_content {
                        owned.ever_reached_ready = true;
                    }
                    if merged != owned.last_screen_state {
                        if foreground_probe_rearms_immediately(&owned.last_screen_state, &merged) {
                            owned.next_foreground_probe = Some(Instant::now());
                        }
                        owned.last_screen_state = merged.clone();
                        observations.push(ObservationEnvelope {
                            protocol_version: CONTROL_PROTOCOL_VERSION,
                            operation_id: None,
                            instance_id: key.instance_id,
                            generation: key.generation,
                            observation: ControlObservation::ScreenState {
                                state: merged.clone(),
                            },
                        });
                    }

                    let frame = terminal_frame(snapshot, merged);
                    efficiency_facts.record_terminal_frame_published(terminal_frame_byte_len(&frame));
                    observations.push(ObservationEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id: None,
                        instance_id: key.instance_id,
                        generation: key.generation,
                        observation: ControlObservation::TerminalFrame { frame },
                    });
                }
                Ok(_) => {}
                Err(error) if !owned.terminal_stale_published => {
                    owned.terminal_stale_published = true;
                    observations.push(ObservationEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id: None,
                        instance_id: key.instance_id,
                        generation: key.generation,
                        observation: ControlObservation::TerminalStale {
                            message: error.to_string(),
                        },
                    });
                }
                Err(_) => {}
            }
        }
        observations
    }

    /// Refresh the foreground half of `PtyScreenState` for whichever
    /// sessions are due, per `next_foreground_probe`.
    ///
    /// Deliberately not folded into `collect_terminal_frames`: that method
    /// is synchronous and only pays for a real capture when the terminal
    /// sequence says the screen changed, but
    /// `PtySession::observe_foreground_timed` is async and walks the live OS
    /// process tree
    /// (`CreateToolhelp32Snapshot` on Windows) unconditionally every time
    /// it is called. Running that walk once per changed frame would scale
    /// a syscall with output rate -- exactly the cost
    /// `collect_terminal_frames`'s own sequence gate exists to avoid. The
    /// cost here is bounded by the count of sessions NOT currently
    /// `Ready`, never by output rate and never by total session count: a
    /// session that reaches `Ready` disarms itself (see
    /// `foreground_probe_schedule`) and is never probed again until its
    /// text disagrees.
    pub async fn reclassify_foreground(&mut self) -> Vec<ObservationEnvelope> {
        let catalog = &self.catalog;
        let now = Instant::now();
        let due: Vec<NativeSessionKey> = self
            .pty_sessions
            .iter()
            .filter(|(_, owned)| owned.next_foreground_probe.is_some_and(|at| now >= at))
            .map(|(key, _)| *key)
            .collect();

        let mut observations = Vec::new();
        for key in due {
            let Some(owned) = self.pty_sessions.get_mut(&key) else {
                continue;
            };
            let probe_result = owned.session.observe_foreground_timed().await;
            match probe_result {
                Ok((observation, timing)) => {
                    // Recorded on success only: `timing` covers all three
                    // components (queue, lock wait, walk) exactly when the
                    // walk itself ran to completion and returned a real
                    // observation. On error (see below) the walk may have
                    // failed partway through, in the mutex, or never been
                    // dispatched at all (`spawn_blocking` panicked), so
                    // there is no single component that is reliably "the
                    // cost of this probe" to attribute a failure to -- unlike
                    // `terminal_state_capture`, which always completes or
                    // never starts, a foreground probe can fail after
                    // dispatch, after acquiring the lock, or during the walk,
                    // and only the success path knows which.
                    self.efficiency_facts.record_foreground_probe(timing);
                    let verdict = match catalog.get(&owned.agent_id) {
                        Some(spec) => resolve_foreground_verdict(
                            spec,
                            &observation,
                            RuntimePlatform::current(),
                        ),
                        // The catalog is loaded once at startup and does not
                        // shrink at runtime; this branch exists only so a
                        // hypothetical gap fails toward the conservative
                        // "not confirmed as the agent" reading rather than
                        // panicking or silently keeping a stale verdict.
                        None => ForegroundVerdict::Foreign {
                            process: observation.observed_process.clone(),
                        },
                    };
                    owned.last_foreground_verdict = Some(verdict);
                    let merged = classify_pty_screen_state(
                        owned.last_foreground_verdict.as_ref(),
                        owned.last_screen_gate.as_ref(),
                        owned.last_screen_failure,
                    );
                    // A foreground-only transition into `Ready` (text was
                    // already clean; only the process signal was missing)
                    // must ALSO close the `screen_failure` window for
                    // future text passes -- see `collect_terminal_frames`.
                    if merged == PtyScreenState::Ready && owned.screen_had_content {
                        owned.ever_reached_ready = true;
                    }
                    if merged != owned.last_screen_state {
                        owned.last_screen_state = merged.clone();
                        observations.push(ObservationEnvelope {
                            protocol_version: CONTROL_PROTOCOL_VERSION,
                            operation_id: None,
                            instance_id: key.instance_id,
                            generation: key.generation,
                            observation: ControlObservation::ScreenState { state: merged },
                        });
                    }
                    owned.next_foreground_probe =
                        match foreground_probe_schedule(&owned.last_screen_state) {
                            ForegroundProbeSchedule::Disarmed => None,
                            ForegroundProbeSchedule::Armed => {
                                Some(now + FOREGROUND_RECLASSIFY_INTERVAL)
                            }
                        };
                }
                Err(_) => {
                    // A failed OS process-tree walk is `Unknown`'s territory
                    // -- it says nothing was confirmed, not that the
                    // process is wrong. Leave whatever verdict/state is
                    // already recorded alone and simply try again next
                    // cadence rather than fabricating a `NotAgent` verdict
                    // or a `Ready` one.
                    owned.next_foreground_probe = Some(now + FOREGROUND_RECLASSIFY_INTERVAL);
                }
            }
        }
        observations
    }
}

/// ConPTY does not populate `TERM`/`COLORTERM` in the environment it hands
/// to a spawned child; a provider CLI reads exactly those two variables to
/// decide whether the terminal in front of it renders color, so a
/// Windows-hosted PTY session comes up monochrome unless something else
/// supplies them. These are the values truecolor-capable terminals
/// (xterm.js, WezTerm, ...) advertise about themselves.
const PTY_TERM_DEFAULT_KEY: &str = "TERM";
const PTY_TERM_DEFAULT_VALUE: &str = "xterm-256color";
const PTY_COLORTERM_DEFAULT_KEY: &str = "COLORTERM";
const PTY_COLORTERM_DEFAULT_VALUE: &str = "truecolor";

/// Fill in the PTY terminal-capability defaults above for whichever of the
/// two keys the caller has not already mutated. A caller-supplied
/// `EnvMutation` for `TERM`/`COLORTERM` -- whether it sets or removes the
/// variable -- always wins: this only appends a default into a gap, it
/// never overwrites or reorders an existing entry.
fn with_pty_terminal_capability_defaults(mut pty_env: Vec<EnvMutation>) -> Vec<EnvMutation> {
    let already_mutated = |env: &[EnvMutation], key: &str| {
        env.iter()
            .any(|mutation| mutation.key.as_os_str() == OsStr::new(key))
    };
    if !already_mutated(&pty_env, PTY_TERM_DEFAULT_KEY) {
        pty_env.push(EnvMutation {
            key: OsString::from(PTY_TERM_DEFAULT_KEY),
            value: Some(OsString::from(PTY_TERM_DEFAULT_VALUE)),
        });
    }
    if !already_mutated(&pty_env, PTY_COLORTERM_DEFAULT_KEY) {
        pty_env.push(EnvMutation {
            key: OsString::from(PTY_COLORTERM_DEFAULT_KEY),
            value: Some(OsString::from(PTY_COLORTERM_DEFAULT_VALUE)),
        });
    }
    pty_env
}

fn missing_session_message(key: NativeSessionKey) -> String {
    format!(
        "native session {:?}/{:?} does not exist",
        key.instance_id, key.generation
    )
}

/// Map the spawn request's `ApprovalLevel` to the ACP host authority mode
/// that answers `session/request_permission` for the SAME session -- the
/// PTY transport lets a human at the keyboard pick their own agent's
/// permission flags (see `gate4agent_catalog::LaunchRequest`, which carries
/// no approval axis at all), but ACP is opened by this process with nobody
/// at a terminal, so it is the one transport where `approval_level` also has
/// to steer the host side of the session, not just the spawned process's own
/// argv (`approval_level_args`).
///
/// `FullAuto` must never land on a refusing `HostPolicy` (`ReadOnly` or
/// `Deny`): that combination launches the process believing it holds full
/// autonomy while every host permission request it makes gets silently
/// refused, which is worse than either policy alone -- so `FullAuto` maps to
/// `HostPolicy::Yolo`, the one mode that grants everything without asking.
/// `ReadOnly` maps to `HostPolicy::ReadOnly` for the same reason in reverse.
///
/// `Moderate` has no dedicated `HostPolicy` variant -- ACP's four host modes
/// are coarser than the three managed `ApprovalLevel` steps -- so it falls
/// back to `HostPolicy::Auto`, the same mode `Unmanaged` ("impose nothing")
/// uses. Both already mean "allow reads, writes, and execution, but decide
/// per request rather than blanket-granting" (see `HostPolicy::Auto`'s own
/// doc comment), which is the correct non-extreme default whether the
/// request asked for a deliberate middle ground or asked for nothing to be
/// imposed at all.
fn host_policy_for_approval_level(level: ApprovalLevel) -> HostPolicy {
    match level {
        ApprovalLevel::FullAuto => HostPolicy::Yolo,
        ApprovalLevel::ReadOnly => HostPolicy::ReadOnly,
        ApprovalLevel::Moderate | ApprovalLevel::Unmanaged => HostPolicy::Auto,
    }
}

/// Whether a session at this approval level parks a `session/request_
/// permission` for an operator instead of answering it from policy on the
/// spot (`AcpSessionOptions::defer_permission_requests`).
///
/// Deliberately the same split as `host_policy_for_approval_level` above:
/// exactly the two levels that map to `HostPolicy::Auto` defer, and for the
/// same reason those two are `Auto` in the first place.
///
/// `FullAuto` never defers. Its own doc says restricting is opt-in and not
/// asking a human is the norm; it is the level chosen precisely so nobody has
/// to be present, and parking its requests would stall unattended work
/// waiting for an operator who was never expected to be there.
///
/// `ReadOnly` never defers either, and this one is a rule rather than a
/// convenience: the level is a restriction, not a question. Offering an
/// operator the chance to approve a write on a read-only session would let
/// the answer widen the level past what was asked for at launch -- the
/// restriction has to be un-negotiable from inside the session, or it is not
/// a restriction.
///
/// `Moderate` is literally the provider's own "ask for approval on request"
/// mode, and `Unmanaged` imposes nothing and so cannot predict what the CLI
/// will ask. Both are levels where a question is expected, so both park it.
fn defers_permission_requests(level: ApprovalLevel) -> bool {
    match level {
        ApprovalLevel::FullAuto | ApprovalLevel::ReadOnly => false,
        ApprovalLevel::Moderate | ApprovalLevel::Unmanaged => true,
    }
}

fn validate_spawn_runtime_policy(
    policy: ProviderRuntimePolicy,
    transport: TransportKind,
    has_initial_prompt: bool,
    is_resume: bool,
) -> Result<(), String> {
    policy
        .validate()
        .map_err(|error| format!("provider runtime policy is invalid: {error}"))?;
    // ACP speaks a structured protocol over stdio, not a PTY -- none of the
    // capabilities below describe anything that exists for it; they all
    // gate inferring provider state from terminal text. The transport-
    // support gate for ACP already lives in the kernel
    // (`spec.capabilities.transports.acp.is_some()`), so this PTY-semantic
    // policy simply does not apply here. Mirrors `gate4agent-runtime-native`'s
    // `validate_effect_runtime_policy`, which enforces the same rule one
    // layer up.
    if transport != TransportKind::Acp {
        require_runtime_capability(policy, ProviderRuntimeCapability::RawPtyLifecycle)?;
        if transport != TransportKind::Pty {
            require_runtime_capability(policy, ProviderRuntimeCapability::SemanticReadiness)?;
        }
        if has_initial_prompt {
            require_runtime_capability(policy, ProviderRuntimeCapability::SemanticReadiness)?;
            require_runtime_capability(policy, ProviderRuntimeCapability::StructuredPrompt)?;
        }
        if is_resume && has_initial_prompt {
            require_runtime_capability(policy, ProviderRuntimeCapability::ProviderSessionIdentity)?;
            require_runtime_capability(policy, ProviderRuntimeCapability::SemanticResume)?;
        }
    }
    Ok(())
}

fn validate_instance_launch_arguments(
    agent_id: &AgentId,
    transport: TransportKind,
    arguments: &[OsString],
) -> Result<(), String> {
    if arguments.is_empty() {
        return Ok(());
    }
    if transport != TransportKind::Pty {
        return Err("native instance launch arguments require PTY transport".to_owned());
    }
    if arguments.len() > INSTANCE_LAUNCH_ARGS_MAX {
        return Err("native instance launch argument count exceeds its bound".to_owned());
    }
    let mut total_bytes = 0usize;
    for argument in arguments {
        let argument = argument.to_string_lossy();
        if argument.contains('\0') {
            return Err("native instance launch argument is invalid".to_owned());
        }
        if argument.len() > INSTANCE_LAUNCH_ARG_MAX_BYTES {
            return Err("native instance launch argument exceeds its bound".to_owned());
        }
        total_bytes = total_bytes.saturating_add(argument.len());
        if total_bytes > INSTANCE_LAUNCH_ARGS_TOTAL_MAX_BYTES {
            return Err("native instance launch argument payload exceeds its bound".to_owned());
        }
        if agent_id.as_str() == "claude"
            && RESERVED_CLAUDE_LAUNCH_FLAGS.iter().any(|reserved| {
                argument == *reserved
                    || (reserved.starts_with("--")
                        && argument
                            .strip_prefix(reserved)
                            .is_some_and(|suffix| suffix.starts_with('=')))
                    || (reserved.len() == 2
                        && argument
                            .strip_prefix(reserved)
                            .is_some_and(|suffix| {
                                !suffix.is_empty() && !suffix.starts_with('-')
                            }))
            })
        {
            return Err(
                "native instance launch arguments conflict with Claude session, resume, or prompt authority"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// True if `value` has the shape of a bearer credential rather than an
/// ordinary CLI flag, path, or session identifier.
///
/// Gate4Agent's own secrets are environment-only and never argv (see
/// `gate4agent/CLAUDE.md`), so this should not fire for anything this repo
/// itself constructs. It exists as defense-in-depth against a provider CLI
/// whose own argv convention accepts a credential positionally. The check is
/// deliberately keyword/prefix-based rather than an entropy heuristic: a
/// generic "long hex/base64 string" rule would also catch legitimate,
/// diagnostically valuable arguments such as a resume session UUID.
fn argument_looks_like_credential(value: &str) -> bool {
    const CREDENTIAL_PREFIXES: &[&str] = &[
        "sk-", "sk_", "ghp_", "gho_", "ghs_", "xox", "g4aho_", "bearer ",
    ];
    const CREDENTIAL_MARKERS: &[&str] = &[
        "apikey", "api_key", "api-key", "secret", "password", "passwd", "token=",
    ];
    let lower = value.to_ascii_lowercase();
    CREDENTIAL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || CREDENTIAL_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
}

/// Renders one provider-CLI argument for a log line: verbatim unless it
/// matches [`argument_looks_like_credential`], in which case it is replaced
/// with a fixed placeholder rather than printed.
fn redact_provider_argument(value: &OsStr) -> String {
    let text = value.to_string_lossy();
    if argument_looks_like_credential(&text) {
        "[redacted-credential-shaped-argument]".to_owned()
    } else {
        text.into_owned()
    }
}

/// Renders a full provider-CLI argument list for a log line, redacting any
/// individual argument that looks like a credential.
fn redact_provider_arguments(values: &[OsString]) -> Vec<String> {
    values.iter().map(|value| redact_provider_argument(value)).collect()
}

fn require_runtime_capability(
    policy: ProviderRuntimePolicy,
    capability: ProviderRuntimeCapability,
) -> Result<(), String> {
    if policy.admits(capability) {
        Ok(())
    } else {
        Err(format!(
            "provider runtime capability {capability:?} is not admitted"
        ))
    }
}

fn should_attach_pty_provider_stream(policy: ProviderRuntimePolicy) -> bool {
    policy.semantic_readiness || policy.provider_session_identity
}

fn should_probe_pty_identity(
    policy: ProviderRuntimePolicy,
    adapter: Option<&gate4agent_types::AdapterBinding>,
    authoritative_identity_present: bool,
    expected_adapter: &str,
) -> bool {
    !authoritative_identity_present
        && policy.semantic_readiness
        && policy.structured_prompt
        && policy.provider_session_identity
        && adapter.is_some_and(|adapter| adapter.id.as_str() == expected_adapter)
}

fn prepare_fresh_pty_provider_session(
    adapter: Option<&gate4agent_types::AdapterBinding>,
    is_resume: bool,
    identity_permitted: bool,
    launch_extra_args: &mut Vec<OsString>,
) -> Option<ProviderSessionIdentity> {
    let adapter = adapter?;
    if is_resume || !identity_permitted || adapter.id.as_str() != "claude-code" {
        return None;
    }
    let identity = ProviderSessionIdentity {
        key: ProviderSessionKey::SessionId,
        id: Uuid::new_v4().to_string(),
        transcript_path: None,
    };
    launch_extra_args.push(OsString::from("--session-id"));
    launch_extra_args.push(OsString::from(&identity.id));
    Some(identity)
}

fn drain_pty_provider(
    key: NativeSessionKey,
    provider: &mut OwnedPtyProvider,
    observations: &mut Vec<ObservationEnvelope>,
) {
    while let Some(event) = provider.pending_events.pop_front() {
        push_provider_observation(key, provider, event, observations);
    }

    loop {
        let envelope = match provider.replay.pop_front() {
            Some(envelope) => Some(envelope),
            None => provider.receiver.try_recv().unwrap_or_default(),
        };
        let Some(envelope) = envelope else {
            break;
        };
        match envelope.event {
            PtyEvent::Output(data) => {
                let raw = provider.utf8.push(&data);
                if raw.is_empty() {
                    continue;
                }
                if provider.semantic_events {
                    if let Some(info) = provider.rate_limits.detect(&raw) {
                        push_provider_observation(
                            key,
                            provider,
                            rate_limit_event(info),
                            observations,
                        );
                    }
                }
                let identity = provider
                    .kimi_identity
                    .as_mut()
                    .and_then(|extractor| extractor.push(&raw));
                if let Some(identity) = identity {
                    if !provider.provider_session_started {
                        push_provider_observation(
                            key,
                            provider,
                            ProviderEvent::SessionStarted {
                                session_id: identity.id.clone(),
                                model: String::new(),
                                tools: Vec::new(),
                            },
                            observations,
                        );
                        provider.provider_session_started = true;
                    }
                    push_provider_observation(
                        key,
                        provider,
                        ProviderEvent::SessionIdentityObserved { identity },
                        observations,
                    );
                }
                if provider.semantic_events {
                    let messages = provider
                        .pipeline
                        .get_mut()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .process(&raw);
                    for message in messages {
                        if let Some(event) = parsed_provider_event(message) {
                            push_provider_observation(key, provider, event, observations);
                        }
                    }
                }
            }
            PtyEvent::DataGap {
                from_sequence,
                to_sequence,
                ..
            } => {
                provider.utf8.clear();
                if let Some(extractor) = &mut provider.kimi_identity {
                    extractor.reset_stream();
                }
                provider
                    .pipeline
                    .get_mut()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                let missed = to_sequence
                    .checked_sub(from_sequence)
                    .and_then(|difference| difference.checked_add(1));
                if let Some((source_sequence, missed)) = missed.and_then(|missed| {
                    reserve_provider_gap_sequence(&mut provider.next_provider_sequence, missed)
                        .map(|source_sequence| (source_sequence, missed))
                }) {
                    observations.push(ObservationEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id: None,
                        instance_id: key.instance_id,
                        generation: key.generation,
                        observation: ControlObservation::ProviderGap {
                            source: provider.source.clone(),
                            source_sequence,
                            missed,
                        },
                    });
                }
            }
            PtyEvent::ReaderError { message } | PtyEvent::OperatorActionRequired { message } => {
                push_provider_observation(
                    key,
                    provider,
                    ProviderEvent::Error { message },
                    observations,
                );
            }
            PtyEvent::Started
            | PtyEvent::Resized(_)
            | PtyEvent::ForegroundProcess(_)
            | PtyEvent::SnapshotAvailable { .. }
            | PtyEvent::Exited { .. } => {}
        }
    }
}

fn push_provider_observation(
    key: NativeSessionKey,
    provider: &mut OwnedPtyProvider,
    event: ProviderEvent,
    observations: &mut Vec<ObservationEnvelope>,
) {
    let sequence = provider.next_provider_sequence;
    provider.next_provider_sequence = provider.next_provider_sequence.saturating_add(1);
    observations.push(ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: None,
        instance_id: key.instance_id,
        generation: key.generation,
        observation: ControlObservation::ProviderEvent {
            source: provider.source.clone(),
            sequence,
            event,
        },
    });
}

fn reserve_provider_gap_sequence(next_sequence: &mut u64, missed: u64) -> Option<u64> {
    if missed == 0 {
        return None;
    }
    let Some(source_sequence) = missed
        .checked_sub(1)
        .and_then(|offset| next_sequence.checked_add(offset))
    else {
        *next_sequence = u64::MAX;
        return None;
    };
    let Some(next) = source_sequence.checked_add(1) else {
        *next_sequence = u64::MAX;
        return None;
    };
    *next_sequence = next;
    Some(source_sequence)
}

fn parsed_provider_event(message: ParsedMessage) -> Option<ProviderEvent> {
    match message.class {
        MessageClass::AiResponse => Some(ProviderEvent::Text {
            text: message.content,
            is_delta: message.metadata.is_partial,
        }),
        MessageClass::ThinkingIndicator => Some(ProviderEvent::Thinking {
            text: message.content,
        }),
        MessageClass::Error => Some(ProviderEvent::Error {
            message: message.content,
        }),
        MessageClass::PromptReady => Some(ProviderEvent::Ready),
        MessageClass::ToolApproval => Some(ProviderEvent::InteractionRequested {
            request_id: None,
            interaction_kind: ProviderInteractionKind::Approval,
            tool_name: message
                .metadata
                .tool_name
                .unwrap_or_else(|| "unknown".to_owned()),
            prompt: message.content,
            agent_id: None,
        }),
        MessageClass::InfoMessage
        | MessageClass::UiElement
        | MessageClass::UserEcho
        | MessageClass::Menu
        | MessageClass::Raw => None,
    }
}

fn rate_limit_event(info: gate4agent::core::types::RateLimitInfo) -> ProviderEvent {
    ProviderEvent::RateLimited {
        limit_type: provider_rate_limit_kind(info.limit_type),
        resets_at: info.resets_at.map(|value| value.to_rfc3339()),
        usage_percent: info.usage_percent.map(|value| value.to_string()),
        raw_message: info.raw_message,
    }
}

/// Map `gate4agent`'s `HostRequestDecision`/`HostDecisionAuthority` onto
/// this crate's wire-typed `ProviderHostRequestDecision`/
/// `ProviderHostDecisionAuthority` -- `gate4agent-types` cannot depend on
/// `gate4agent` (see that crate's own `CLAUDE.md`), so the conversion lives
/// here, the shell that already depends on both. A plain match, not
/// `format!("{:?}", ..)`: the wire carries the typed value itself.
fn provider_host_request_decision(decision: HostRequestDecision) -> ProviderHostRequestDecision {
    match decision {
        HostRequestDecision::Granted { by } => {
            ProviderHostRequestDecision::Granted { by: provider_host_decision_authority(by) }
        }
        HostRequestDecision::Denied { by } => {
            ProviderHostRequestDecision::Denied { by: provider_host_decision_authority(by) }
        }
        HostRequestDecision::Deferred => ProviderHostRequestDecision::Deferred,
    }
}

fn provider_host_decision_authority(by: HostDecisionAuthority) -> ProviderHostDecisionAuthority {
    match by {
        HostDecisionAuthority::Gate => ProviderHostDecisionAuthority::Gate,
        HostDecisionAuthority::Policy => ProviderHostDecisionAuthority::Policy,
        HostDecisionAuthority::Operator => ProviderHostDecisionAuthority::Operator,
        HostDecisionAuthority::DeadlinePolicy => ProviderHostDecisionAuthority::DeadlinePolicy,
    }
}

/// Map the detector's own `RateLimitType` onto the wire-typed
/// `ProviderRateLimitKind`. A plain match, not `format!("{:?}", ..)`: the
/// wire carries the typed value itself, not a Debug-rendering of it.
fn provider_rate_limit_kind(
    limit_type: gate4agent::core::types::RateLimitType,
) -> ProviderRateLimitKind {
    use gate4agent::core::types::RateLimitType;
    match limit_type {
        RateLimitType::Session => ProviderRateLimitKind::Session,
        RateLimitType::Daily => ProviderRateLimitKind::Daily,
        RateLimitType::Weekly => ProviderRateLimitKind::Weekly,
        RateLimitType::Unknown => ProviderRateLimitKind::Unknown,
    }
}

/// `mode_catalogue` reads the mode catalogue off `owned.session` for
/// transports that have one -- only `AcpSession::available_modes()` does
/// (see `provider_event`'s doc comment); `collect_provider_map`'s two
/// non-ACP call sites pass `|_| Vec::new()` since their `AgentEvent` stream
/// never carries `ModeChanged` in the first place.
fn collect_provider_map<S>(
    sessions: &mut BTreeMap<NativeSessionKey, OwnedProviderSession<S>>,
    observations: &mut Vec<ObservationEnvelope>,
    mode_catalogue: impl Fn(&S) -> Vec<SessionMode>,
) {
    for (key, owned) in sessions {
        let available_modes = mode_catalogue(&owned.session);
        drain_provider_stream(
            *key,
            &owned.source,
            &mut owned.events,
            &mut owned.pending_events,
            &mut owned.next_provider_sequence,
            Some(&mut owned.observed_exit_code),
            observations,
            &available_modes,
        );
    }
}

fn drain_provider_stream(
    key: NativeSessionKey,
    source: &ProviderSource,
    events: &mut broadcast::Receiver<AgentEvent>,
    pending_events: &mut VecDeque<AgentEvent>,
    next_provider_sequence: &mut u64,
    mut observed_exit_code: Option<&mut Option<i32>>,
    observations: &mut Vec<ObservationEnvelope>,
    available_modes: &[SessionMode],
) {
    loop {
        let next = match pending_events.pop_front() {
            Some(event) => Ok(event),
            None => events.try_recv(),
        };
        match next {
            Ok(AgentEvent::Exited { code }) => {
                if let Some(exit_code) = observed_exit_code.as_deref_mut() {
                    *exit_code = Some(code);
                }
            }
            Ok(event) => {
                let Some(event) = provider_event(event, available_modes) else {
                    continue;
                };
                let sequence = *next_provider_sequence;
                *next_provider_sequence = next_provider_sequence.saturating_add(1);
                observations.push(ObservationEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: None,
                    instance_id: key.instance_id,
                    generation: key.generation,
                    observation: ControlObservation::ProviderEvent {
                        source: source.clone(),
                        sequence,
                        event,
                    },
                });
            }
            Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                if let Some(source_sequence) =
                    reserve_provider_gap_sequence(next_provider_sequence, missed)
                {
                    observations.push(ObservationEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id: None,
                        instance_id: key.instance_id,
                        generation: key.generation,
                        observation: ControlObservation::ProviderGap {
                            source: source.clone(),
                            source_sequence,
                            missed,
                        },
                    });
                }
            }
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break
            }
        }
    }
}

fn collect_provider_exits<S>(
    sessions: &mut BTreeMap<NativeSessionKey, OwnedProviderSession<S>>,
    observations: &mut Vec<ObservationEnvelope>,
    finished: impl Fn(&S) -> bool,
) {
    let completed: Vec<_> = sessions
        .iter()
        .filter_map(|(key, owned)| {
            (owned.observed_exit_code.is_some() || finished(&owned.session)).then_some(*key)
        })
        .collect();
    for key in completed {
        let owned = sessions
            .remove(&key)
            .expect("completed provider key came from the owned session map");
        observations.push(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: key.instance_id,
            generation: key.generation,
            observation: ControlObservation::ProcessExited {
                exit_code: owned.observed_exit_code,
                final_terminal: None,
            },
        });
    }
}

/// Prefix tags [`encode_rpc_request_id`] / [`decode_rpc_request_id`] use to
/// keep an `RpcId::Number` and an `RpcId::String` from colliding once both
/// are flattened into the bare `Option<String>` that
/// `ProviderEvent::InteractionRequested::request_id` and
/// `ProviderInteractionTarget::provider_request_id` carry.
const RPC_REQUEST_ID_NUMBER_PREFIX: &str = "number:";
const RPC_REQUEST_ID_STRING_PREFIX: &str = "string:";

/// Encode a JSON-RPC id as the `request_id` string carried on
/// `ProviderEvent::InteractionRequested` -- minted once, in `provider_event`'s
/// `AgentEvent::RpcIncomingRequest` arm, the moment a `session/request_
/// permission` call is first reported `HostRequestDecision::Deferred`. Read
/// back by [`decode_rpc_request_id`] when a later `ResolveInteraction`
/// effect needs to find this exact pending request again in
/// `AcpSession::resolve_pending_request` (`gate4agent::acp::session`).
///
/// Tagged rather than a bare `to_string()`: an agent-issued
/// `RpcId::Number(42)` and `RpcId::String("42".into())` are different ids on
/// the wire and must not collide into the same encoded string.
fn encode_rpc_request_id(id: &RpcId) -> String {
    match id {
        RpcId::Number(number) => format!("{RPC_REQUEST_ID_NUMBER_PREFIX}{number}"),
        RpcId::String(value) => format!("{RPC_REQUEST_ID_STRING_PREFIX}{value}"),
    }
}

/// Inverse of [`encode_rpc_request_id`]. `None` covers both an unrecognized
/// tag and a `number:` tag whose remainder does not parse as a `u64` --
/// either way, not an id this process ever minted, so there is nothing to
/// look up in `AcpSession`'s pending-request map.
fn decode_rpc_request_id(raw: &str) -> Option<RpcId> {
    if let Some(number) = raw.strip_prefix(RPC_REQUEST_ID_NUMBER_PREFIX) {
        return number.parse::<u64>().ok().map(RpcId::Number);
    }
    raw.strip_prefix(RPC_REQUEST_ID_STRING_PREFIX)
        .map(|value| RpcId::String(value.to_owned()))
}

/// Resolve a `ControlEffect::ResolveInteraction` naming exactly why it
/// could not be routed, when it could not be -- see
/// `docs/gate4agent/plans/gate4agent-acp-control-plane-on-the-wire-2026-09-02.md`
/// §4a-§5, where this is called "the deferral organ, not the plumbing" that
/// was still missing.
///
/// `session` is `None` for every transport this crate owns other than a
/// live ACP session for `key` -- PTY and pipe sessions never defer a
/// `session/request_permission` call in the first place (only
/// `AcpSessionOptions::defer_permission_requests` does, and that field only
/// exists on `AcpSessionOptions`), so there is never anything for them to
/// resolve here.
fn resolve_acp_interaction_observation(
    session: Option<&AcpSession>,
    target: ProviderInteractionTarget,
    response: ProviderInteractionResponse,
) -> ControlObservation {
    let interaction_id = target.interaction_id;
    let fail = |message: String| ControlObservation::InteractionResolutionFailed {
        interaction_id,
        message,
    };
    let Some(session) = session else {
        return fail("semantic interaction resolution requires an ACP session".to_owned());
    };
    let Some(provider_request_id) = target.provider_request_id.as_deref() else {
        return fail("ACP interaction resolution requires a provider request id".to_owned());
    };
    let Some(id) = decode_rpc_request_id(provider_request_id) else {
        return fail(format!(
            "ACP interaction resolution request id {provider_request_id:?} is not a valid JSON-RPC id"
        ));
    };
    match resolve_acp_permission_interaction(session, &id, response) {
        Ok(()) => ControlObservation::InteractionResolutionCompleted { interaction_id },
        Err(message) => fail(message),
    }
}

/// Route an operator's `ProviderInteractionResponse` for a deferred ACP
/// `session/request_permission` call into
/// `AcpSession::resolve_pending_request_as` (`gate4agent::acp::session`).
///
/// The operator states an intent, not an option id, and that is deliberate:
/// an ACP agent offers whatever subset of the four `PermissionOptionKind`
/// values it likes, and an `optionId` it never offered is ignored rather
/// than refused -- so a fabricated one would grant nothing while reporting
/// success. Selecting a REAL offered option is therefore
/// `AcpSession`'s job, since it is the side that holds the pending request's
/// own params; it walks the same preference order `HostPolicy` walks,
/// through one shared selector, so an approval means the same thing whether
/// policy or a human produced it. If the agent offered nothing in the
/// requested direction the answer is `Cancelled`, which every ACP agent
/// accepts as complete.
///
/// This is also why the wire verb carries a `ProviderInteractionResponse` --
/// the same approve/deny vocabulary a PTY interaction uses -- rather than an
/// ACP-specific option id: all three chains say the same thing to an
/// operator, and only the ACP path has to know what an option id is.
///
/// `Answer` never applies here: ACP `session/request_permission` only ever
/// offers allow/deny options, never a free-text one.
fn resolve_acp_permission_interaction(
    session: &AcpSession,
    id: &RpcId,
    response: ProviderInteractionResponse,
) -> Result<(), String> {
    let choice = match response {
        ProviderInteractionResponse::ApproveOnce => OperatorPermissionChoice::Approve,
        ProviderInteractionResponse::Deny => OperatorPermissionChoice::Reject,
        ProviderInteractionResponse::Answer { .. } => {
            return Err("ACP permission interactions do not accept free-text answers".to_owned())
        }
    };
    session.resolve_pending_request_as(id, choice).map_err(|error| error.to_string())
}

/// Resolve a `ControlEffect::SetSessionMode` against a live ACP session's
/// own advertised mode catalogue, naming exactly why it could not be
/// routed when it could not be -- same refusal-by-name discipline as
/// [`resolve_acp_interaction_observation`].
///
/// There is no `supports_*` probe for session modes (unlike
/// `AcpSession::supports_session_list` and its siblings): the ACP
/// `sessionCapabilities` flags block carries no `modes` key at all, so the
/// mode catalogue seeded at handshake (`AcpSession::available_modes`) IS
/// the capability signal -- empty means the agent never advertised any
/// modes, non-empty-but-missing-this-id means it advertised modes but not
/// this one.
async fn set_acp_session_mode_observation(
    session: Option<&AcpSession>,
    mode_id: String,
) -> ControlObservation {
    let Some(session) = session else {
        return ControlObservation::SessionModeSetFailed {
            message: "session mode switch requires an ACP session".to_owned(),
        };
    };
    let catalogue = session.available_modes();
    if catalogue.is_empty() {
        return ControlObservation::SessionModeSetFailed {
            message: "the provider announces no session-mode capability".to_owned(),
        };
    }
    if !catalogue.iter().any(|mode| mode.id == mode_id) {
        return ControlObservation::SessionModeSetFailed {
            message: format!("the provider never offered mode {mode_id:?}"),
        };
    }
    match session.set_mode(&mode_id).await {
        Ok(()) => ControlObservation::SessionModeSet { mode_id },
        Err(error) => ControlObservation::SessionModeSetFailed {
            message: error.to_string(),
        },
    }
}

/// Resolve a `ControlEffect::SetSessionConfigOption` against a live ACP
/// session's own advertised config-option catalogue -- same capability
/// reasoning as [`set_acp_session_mode_observation`] (no dedicated
/// `supports_*` probe; the catalogue seeded at handshake, `AcpSession::
/// config_options`, is the capability signal).
///
/// `value_json` arrives already validated as parseable JSON at the wire
/// boundary (`gate4agent-node-protocol`'s `deserialize_acp_config_value_
/// json`) and bounds-checked again at the engine
/// (`gate4agent_types::validate_session_config_value_json`), but is only
/// ever carried as text up to here -- `gate4agent-types` does not depend on
/// `serde_json`. Parsing it into the `serde_json::Value` `AcpSession::
/// set_config_option` actually takes is this function's job; the target
/// type is inferred from that call rather than named directly, so this
/// crate never needs its own `serde_json` dependency.
async fn set_acp_session_config_option_observation(
    session: Option<&AcpSession>,
    option_id: String,
    value_json: String,
) -> ControlObservation {
    let Some(session) = session else {
        return ControlObservation::SessionConfigOptionSetFailed {
            message: "session config-option switch requires an ACP session".to_owned(),
        };
    };
    let catalogue = session.config_options();
    if catalogue.is_empty() {
        return ControlObservation::SessionConfigOptionSetFailed {
            message: "the provider announces no session config-option capability".to_owned(),
        };
    }
    if !catalogue.iter().any(|option| option.id == option_id) {
        return ControlObservation::SessionConfigOptionSetFailed {
            message: format!("the provider never offered config option {option_id:?}"),
        };
    }
    let value = match value_json.parse() {
        Ok(value) => value,
        Err(error) => {
            return ControlObservation::SessionConfigOptionSetFailed {
                message: format!("session config option value is not valid JSON: {error}"),
            }
        }
    };
    match session.set_config_option(&option_id, value).await {
        Ok(()) => ControlObservation::SessionConfigOptionSet { option_id },
        Err(error) => ControlObservation::SessionConfigOptionSetFailed {
            message: error.to_string(),
        },
    }
}

/// Resolve a `ControlEffect::SetSessionModel`. Unlike session modes and
/// config options, `AcpSession` exposes no setter at all for this --
/// `available_models`/`current_model_id` observe Grok's vendor `_x.ai/
/// models/update` catalogue, but no live capture has ever shown a
/// host-invocable RPC to request a switch (ACP proper has no `session/
/// set_model`, and no vendor extension for one has been verified). Refuses
/// by name rather than fabricate a call the crate documented it would
/// never invent -- see `ControlCommand::SetSessionModel`'s doc comment.
fn set_acp_session_model_observation(
    session: Option<&AcpSession>,
    model_id: String,
) -> ControlObservation {
    if session.is_none() {
        return ControlObservation::SessionModelSetFailed {
            message: "session model switch requires an ACP session".to_owned(),
        };
    }
    ControlObservation::SessionModelSetFailed {
        message: format!(
            "this provider offers no host-invocable model-switch method (requested model {model_id:?})"
        ),
    }
}

/// Maps one `AgentEvent` onto the wire-typed `ProviderEvent`. `available_
/// modes` is used ONLY by the `AgentEvent::ModeChanged` arm -- it carries
/// the mode catalogue `AcpSession::available_modes()` read at handshake
/// (`session/new`), so a mode change reports the SAME catalogue the agent
/// actually offered, not just the id that changed. Every other arm ignores
/// it; passing `&[]` from a non-ACP transport (which never emits
/// `ModeChanged` in the first place -- see `update_to_event`'s
/// `CurrentModeUpdate` arm, `gate4agent`'s `src/acp/protocol.rs`) changes
/// nothing observable.
fn provider_event(event: AgentEvent, available_modes: &[SessionMode]) -> Option<ProviderEvent> {
    match event {
        AgentEvent::SessionStart {
            session_id,
            model,
            tools,
        } => Some(ProviderEvent::SessionStarted {
            session_id,
            model,
            tools,
        }),
        AgentEvent::Text { text, is_delta } => Some(ProviderEvent::Text { text, is_delta }),
        AgentEvent::Thinking { text } => Some(ProviderEvent::Thinking { text }),
        AgentEvent::ToolStart { id, name, input } => Some(ProviderEvent::ToolStarted {
            id,
            name,
            input_json: input.to_string(),
            agent_id: None,
        }),
        AgentEvent::ToolResult {
            id,
            output,
            is_error,
            duration_ms,
        } => Some(ProviderEvent::ToolCompleted {
            id,
            output,
            is_error,
            duration_ms,
            agent_id: None,
        }),
        AgentEvent::TurnComplete {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            context_window,
            is_cumulative,
        } => Some(ProviderEvent::TurnCompleted {
            usage: TokenUsage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                context_window,
            },
            is_cumulative,
        }),
        AgentEvent::ContextWindowUsage { usage } => {
            Some(ProviderEvent::ContextWindowUsage {
                usage: ProviderContextWindowUsage {
                    uncached_input_tokens: usage.uncached_input_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                    cache_write_tokens: usage.cache_write_tokens,
                    output_tokens: usage.output_tokens,
                    unattributed_tokens: usage.unattributed_tokens,
                    used_tokens: usage.used_tokens,
                    capacity_tokens: usage.capacity_tokens,
                },
            })
        }
        AgentEvent::SessionEnd {
            result,
            cost_usd,
            is_error,
        } => Some(ProviderEvent::SessionEnded {
            result,
            cost_usd: cost_usd.map(|cost| cost.to_string()),
            is_error,
        }),
        AgentEvent::Error { message } => Some(ProviderEvent::Error { message }),
        AgentEvent::PtyParsed(message) => parsed_provider_event(message),
        AgentEvent::PtyReady => Some(ProviderEvent::Ready),
        AgentEvent::PtyToolApproval {
            tool_name,
            description,
        } => Some(ProviderEvent::InteractionRequested {
            request_id: None,
            interaction_kind: ProviderInteractionKind::Approval,
            tool_name,
            prompt: description.unwrap_or_default(),
            agent_id: None,
        }),
        AgentEvent::RateLimit(info) => Some(rate_limit_event(info)),
        // A `session/request_permission` call the host left `Deferred` (see
        // `AcpSessionOptions::defer_permission_requests`) is the one host
        // request an operator can actually act on -- so, ONLY for that
        // first, deferred sighting of a given id, this becomes an
        // `InteractionRequested` carrying the id (`encode_rpc_request_id`)
        // instead of the audit-only `HostRequestObserved` every other host
        // request becomes. The SAME id's eventual `Granted`/`Denied`
        // (`HostRequestDecision`'s doc comment on `Deferred`) still falls
        // through to `HostRequestObserved` below, exactly as before -- see
        // `docs/gate4agent/plans/gate4agent-acp-control-plane-on-the-wire-2026-09-02.md`
        // §4a.2, which is what this replaces (`id: _`, the id silently
        // dropped, so an ACP permission request never became an
        // `InteractionRequested` at all).
        AgentEvent::RpcIncomingRequest {
            id,
            method,
            params,
            decision,
        } if method == "session/request_permission"
            && matches!(decision, HostRequestDecision::Deferred) =>
        {
            let tool_call = params.as_ref().and_then(|params| params.get("toolCall"));
            let field = |name: &str| {
                tool_call
                    .and_then(|tool_call| tool_call.get(name))
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            };
            // `kind` is the tool's CLASS (`execute`, `edit`, `read`, ...) and
            // `title` is the human sentence describing this particular call.
            // They are two different things and the operator needs both: the
            // class is what the node hashes into `tool_class` for the
            // observation, and the title is the actual question. Putting the
            // title in `tool_name` and leaving `prompt` empty -- which is
            // what this did -- showed the operator a correlation id and a
            // sentence-shaped tool name, and never the question itself.
            Some(ProviderEvent::InteractionRequested {
                request_id: Some(encode_rpc_request_id(&id)),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: field("kind").unwrap_or(method),
                prompt: field("title").unwrap_or_default(),
                agent_id: None,
            })
        }
        AgentEvent::RpcIncomingRequest {
            id: _,
            method,
            params,
            decision,
        } => Some(ProviderEvent::HostRequestObserved {
            method,
            params_json: params.map(|value| value.to_string()).unwrap_or_default(),
            decision: provider_host_request_decision(decision),
        }),
        AgentEvent::RpcNotification { method, params } => {
            Some(ProviderEvent::UnrecognizedNotification {
                method,
                payload_json: params.to_string(),
            })
        }
        AgentEvent::Started { .. } | AgentEvent::Exited { .. } | AgentEvent::PtyRaw { .. } => None,
        AgentEvent::UserMessage { text, is_delta } => {
            Some(ProviderEvent::UserMessage { text, is_delta })
        }
        AgentEvent::Plan { steps } => Some(ProviderEvent::Plan {
            steps: steps.into_iter().map(provider_plan_step).collect(),
        }),
        AgentEvent::AvailableCommandsUpdate { commands } => {
            Some(ProviderEvent::AvailableCommandsUpdated {
                commands: commands
                    .into_iter()
                    .map(|command| ProviderAvailableCommand {
                        name: command.name,
                        description: command.description,
                        input_hint: command.input_hint,
                    })
                    .collect(),
            })
        }
        AgentEvent::ModeChanged { mode_id } => Some(ProviderEvent::ModeChanged {
            mode_id,
            available: available_modes.iter().cloned().map(provider_mode_info).collect(),
        }),
        AgentEvent::SessionInfoUpdate { title } => {
            Some(ProviderEvent::SessionInfoUpdated { title })
        }
        AgentEvent::UsageUpdate {
            used_tokens,
            context_window,
            cost_amount,
            cost_currency,
        } => Some(ProviderEvent::UsageUpdated {
            used_tokens,
            context_window,
            cost_amount: cost_amount.map(|amount| amount.to_string()),
            cost_currency,
        }),
        AgentEvent::ConfigOptionsUpdate { options } => Some(ProviderEvent::ConfigOptionsUpdated {
            options: options.into_iter().map(provider_config_option).collect(),
        }),
        // Grok vendor `_x.ai/*` extensions (see `gate4agent::acp::protocol::
        // parse_vendor_notification`) -- not yet bridged to `ProviderEvent`.
        // No `ProviderEvent` variant exists for any of these today; adding
        // one is a separate, larger change across this crate's validation
        // (`ProviderEventValidationError`), the observation pipeline, and
        // the TUI, not part of parsing the ACP wire itself.
        AgentEvent::ModelsUpdate { .. }
        | AgentEvent::ProviderModelChanged { .. }
        | AgentEvent::SettingsUpdate { .. }
        | AgentEvent::HookExecutionUpdate { .. }
        | AgentEvent::McpServersUpdate { .. }
        | AgentEvent::McpInitProgress { .. }
        | AgentEvent::McpInitialized { .. }
        | AgentEvent::AnnouncementsUpdate { .. } => None,
    }
}

/// Map the ACP wire priority onto the typed `ProviderPlanPriority`. A
/// plain match, not `format!("{:?}", ..)` -- see `provider_rate_limit_kind`
/// for why: the wire carries the typed value itself.
fn provider_plan_priority(priority: gate4agent::core::types::PlanStepPriority) -> ProviderPlanPriority {
    use gate4agent::core::types::PlanStepPriority;
    match priority {
        PlanStepPriority::High => ProviderPlanPriority::High,
        PlanStepPriority::Medium => ProviderPlanPriority::Medium,
        PlanStepPriority::Low => ProviderPlanPriority::Low,
    }
}

/// Map the ACP wire status onto the typed `ProviderPlanStatus`.
fn provider_plan_status(status: gate4agent::core::types::PlanStepStatus) -> ProviderPlanStatus {
    use gate4agent::core::types::PlanStepStatus;
    match status {
        PlanStepStatus::Pending => ProviderPlanStatus::Pending,
        PlanStepStatus::InProgress => ProviderPlanStatus::InProgress,
        PlanStepStatus::Completed => ProviderPlanStatus::Completed,
    }
}

fn provider_plan_step(step: gate4agent::core::types::PlanStep) -> ProviderPlanStep {
    ProviderPlanStep {
        content: step.content,
        priority: provider_plan_priority(step.priority),
        status: provider_plan_status(step.status),
    }
}

/// Maps one ACP `SessionMode` (`gate4agent::acp::protocol::SessionMode`,
/// `session/new`'s `modes.availableModes`) onto the wire-typed
/// `ProviderModeInfo` carried on `ProviderEvent::ModeChanged::available`.
fn provider_mode_info(mode: SessionMode) -> ProviderModeInfo {
    ProviderModeInfo {
        id: mode.id,
        name: mode.name,
        description: mode.description,
    }
}

/// Map the ACP wire config-option kind onto the typed
/// `ProviderConfigOptionKind`.
fn provider_config_option_kind(
    kind: gate4agent::core::types::ConfigOptionKind,
) -> ProviderConfigOptionKind {
    use gate4agent::core::types::ConfigOptionKind;
    match kind {
        ConfigOptionKind::Select => ProviderConfigOptionKind::Select,
        ConfigOptionKind::Boolean => ProviderConfigOptionKind::Boolean,
        ConfigOptionKind::Unknown => ProviderConfigOptionKind::Unknown,
    }
}

/// `gate4agent-types` is a pure data contract and does not depend on
/// `serde_json`, so the option's current value and each choice's value --
/// both `serde_json::Value` in `gate4agent::core::types::ConfigOptionInfo`
/// -- cross the boundary pre-serialized to JSON text, same convention as
/// `ToolStarted::input_json` above.
fn provider_config_option(option: gate4agent::core::types::ConfigOptionInfo) -> ProviderConfigOption {
    ProviderConfigOption {
        id: option.id,
        name: option.name,
        description: option.description,
        category: option.category,
        kind: provider_config_option_kind(option.kind),
        value_json: option.value.to_string(),
        choices: option
            .choices
            .into_iter()
            .map(|choice| ProviderConfigChoice {
                value_json: choice.value.to_string(),
                label: choice.label,
            })
            .collect(),
    }
}

fn builtin_legacy_adapter_runtimes() -> AdapterRuntimeRegistry<CliTool> {
    // Each of the four fleet providers registers only the transport families
    // it actually declares in the built-in adapter registry -- Grok, for
    // instance, is ACP-only and has no `PtySemantic`/`Pipe` descriptor.
    // Absence of a family for a given provider is not an error; only a
    // duplicate registration is.
    let definitions = [
        ("claude-code", CliTool::ClaudeCode),
        ("codex", CliTool::Codex),
        ("kimi", CliTool::KimiCode),
        ("grok", CliTool::Grok),
    ];
    let mut runtimes = AdapterRuntimeRegistry::default();
    for (id, tool) in definitions {
        for family in [
            AdapterFamily::PtySemantic,
            AdapterFamily::Pipe,
            AdapterFamily::Acp,
        ] {
            let Some(binding) = builtin_adapter_registry().binding(family, id) else {
                continue;
            };
            runtimes
                .insert(family, binding.clone(), tool)
                .expect("built-in native adapter runtime must be unique");
        }
    }

    runtimes
}

fn terminal_frame(snapshot: PtyTerminalSnapshot, screen_state: PtyScreenState) -> TerminalFrame {
    TerminalFrame {
        sequence: snapshot.sequence,
        size: TerminalSize {
            rows: snapshot.size.rows,
            columns: snapshot.size.cols,
        },
        cursor_row: snapshot.cursor.0,
        cursor_column: snapshot.cursor.1,
        contents: snapshot.contents,
        formatted: snapshot.formatted,
        scrollback_formatted: snapshot.scrollback_formatted,
        alternate_screen: snapshot.alternate_screen,
        mouse_protocol_enabled: snapshot.mouse_protocol_enabled,
        mouse_protocol_encoding: match snapshot.mouse_protocol_encoding {
            PtyMouseProtocolEncoding::Default => TerminalMouseProtocolEncoding::Default,
            PtyMouseProtocolEncoding::Utf8 => TerminalMouseProtocolEncoding::Utf8,
            PtyMouseProtocolEncoding::Sgr => TerminalMouseProtocolEncoding::Sgr,
        },
        // Carried through unchanged from the PTY snapshot -- this crate does
        // not restamp it, see `TerminalFrame::produced_at_unix_ms`'s own doc.
        produced_at_unix_ms: snapshot.produced_at_unix_ms,
        screen_state,
        // Known at capture time: `PtyTerminalSnapshot::bracketed_paste` is
        // read off the same `vt100::Screen` as `contents`/`formatted`, so
        // `Some` here always means "sampled", never "unknown" -- see
        // `TerminalFrame::bracketed_paste`'s own doc for what `None` means.
        bracketed_paste: Some(snapshot.bracketed_paste),
    }
}

/// Whether the cheap `terminal_sequence()` gate should skip the expensive
/// `terminal_state()` call outright -- true exactly when the sequence read
/// is no newer than what this session already captured. An `Err` from the
/// cheap read does NOT skip: it falls through so the real call is attempted
/// (and reports its own failure through the normal `terminal_state()`
/// error path), matching the pre-instrumentation behaviour of the
/// `matches!` this replaces.
fn terminal_state_capture_should_skip<E>(sequence: Result<u64, E>, last_captured: u64) -> bool {
    matches!(sequence, Ok(sequence) if sequence <= last_captured)
}

/// Total wire-relevant byte size of one [`TerminalFrame`] -- `formatted`
/// plus every row of `scrollback_formatted` summed. This is exactly what
/// gets handed to the c2 relay/harness/operator wire per frame (`contents`
/// never leaves the node), which is why it is the number the frame-bytes
/// distribution reports.
fn terminal_frame_byte_len(frame: &TerminalFrame) -> u64 {
    let scrollback_bytes: usize = frame.scrollback_formatted.iter().map(Vec::len).sum();
    (frame.formatted.len() + scrollback_bytes) as u64
}

fn canonical_foreground(
    agent_id: AgentId,
    observation: &PtyForegroundObservation,
) -> ForegroundProcess {
    let kind = if observation.readiness.process_name.as_deref() == Some(agent_id.as_str()) {
        ForegroundProcessKind::Agent { agent_id }
    } else if observation.readiness.is_shell {
        ForegroundProcessKind::Shell
    } else {
        ForegroundProcessKind::Other
    };
    ForegroundProcess {
        root_process_id: observation.root_pid,
        process_id: observation.observed_pid,
        process_name: observation.observed_process.clone(),
        kind,
    }
}

fn effect_failure(effect: &ControlEffect, message: String) -> ControlObservation {
    match effect {
        ControlEffect::Spawn { .. } => ControlObservation::SpawnFailed { message },
        ControlEffect::Stop { .. } => ControlObservation::StopFailed { message },
        ControlEffect::WriteInput { .. } => ControlObservation::InputFailed { message },
        ControlEffect::SubmitPrompt { .. } | ControlEffect::Interrupt => {
            ControlObservation::InputFailed { message }
        }
        ControlEffect::ResolveInteraction { target, .. } => {
            ControlObservation::InteractionResolutionFailed {
                interaction_id: target.interaction_id,
                message,
            }
        }
        ControlEffect::SetSessionMode { .. } => ControlObservation::SessionModeSetFailed { message },
        ControlEffect::SetSessionConfigOption { .. } => {
            ControlObservation::SessionConfigOptionSetFailed { message }
        }
        ControlEffect::SetSessionModel { .. } => ControlObservation::SessionModelSetFailed { message },
        ControlEffect::Resize { .. } => ControlObservation::ResizeFailed { message },
        ControlEffect::ObserveForeground => ControlObservation::ForegroundFailed { message },
        ControlEffect::ProbeCapabilities { .. } => ControlObservation::CapabilityProbeFailed {
            failure: CapabilityProbeFailure::ExecutorUnavailable,
        },
        ControlEffect::DiscoverHistory { .. } | ControlEffect::LoadHistory { .. } => {
            ControlObservation::HistoryFailed { message }
        }
        ControlEffect::AuthorizeResume { .. } => ControlObservation::ResumeFailed { message },
        ControlEffect::SpawnResume { .. } => ControlObservation::SpawnFailed { message },
    }
}

fn completion_observation(
    operation_id: OperationId,
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    observation: ControlObservation,
) -> ObservationEnvelope {
    ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: Some(operation_id),
        instance_id,
        generation,
        observation,
    }
}

async fn wait_for_readiness(
    session: &PtySession,
    spec: &AgentSpec,
    intent: ReadinessIntent,
    detect_startup_gates: bool,
) -> Result<ReadinessPermit, String> {
    let started = Instant::now();
    let (terminal, attachment) = attach_readiness_boundary(session)?;
    let mut receiver = attachment.receiver;
    let mut tracker = ReadinessTracker::new(spec, RuntimePlatform::current(), intent);
    let mut diagnostics = ReadinessDiagnostics {
        draft_signal: Some(spec.readiness.draft_signal),
        ..ReadinessDiagnostics::default()
    };
    seed_readiness_from_terminal(
        &terminal,
        &mut tracker,
        &mut diagnostics,
        elapsed_ms(started),
    )?;
    let interval = Duration::from_millis(spec.readiness.poll_interval_ms.max(1));
    let mut next_probe = Instant::now();

    for event in attachment.replay {
        observe_readiness_event(
            &mut tracker,
            &mut diagnostics,
            event.event,
            elapsed_ms(started),
        )?;
        if detect_startup_gates {
            ensure_no_readiness_operator_gate(&diagnostics, spec)?;
            ensure_no_startup_operator_gate(session, spec)?;
        }
    }

    loop {
        if detect_startup_gates {
            ensure_no_startup_operator_gate(session, spec)?;
        }
        if Instant::now() >= next_probe {
            let foreground = session
                .observe_foreground()
                .await
                .map_err(|error| error.to_string())?;
            diagnostics.observe_foreground(&foreground.readiness);
            tracker.observe_foreground(&foreground.readiness, elapsed_ms(started));
            next_probe = Instant::now() + interval;
        }
        tracker.poll(elapsed_ms(started));
        if readiness_complete(tracker.status(), &diagnostics)? {
            if detect_startup_gates {
                ensure_no_startup_operator_gate(session, spec)?;
            }
            return tracker
                .into_permit()
                .ok_or_else(|| "ready tracker did not issue a permit".to_owned());
        }

        let wait = next_probe.saturating_duration_since(Instant::now());
        match tokio::time::timeout(wait, receiver.recv()).await {
            Ok(Ok(event)) => {
                if matches!(&event.event, PtyEvent::DataGap { .. }) {
                    let (terminal, attachment) = attach_readiness_boundary(session)?;
                    receiver = attachment.receiver;
                    tracker = ReadinessTracker::new(
                        spec,
                        RuntimePlatform::current(),
                        intent,
                    );
                    diagnostics = ReadinessDiagnostics {
                        draft_signal: Some(spec.readiness.draft_signal),
                        ..ReadinessDiagnostics::default()
                    };
                    seed_readiness_from_terminal(
                        &terminal,
                        &mut tracker,
                        &mut diagnostics,
                        elapsed_ms(started),
                    )?;
                    for retained in attachment.replay {
                        observe_readiness_event(
                            &mut tracker,
                            &mut diagnostics,
                            retained.event,
                            elapsed_ms(started),
                        )?;
                    }
                } else {
                    observe_readiness_event(
                        &mut tracker,
                        &mut diagnostics,
                        event.event,
                        elapsed_ms(started),
                    )?;
                }
                if detect_startup_gates {
                    ensure_no_readiness_operator_gate(&diagnostics, spec)?;
                    ensure_no_startup_operator_gate(session, spec)?;
                }
            }
            Ok(Err(error)) => return Err(error.to_string()),
            Err(_) => {
                tracker.poll(elapsed_ms(started));
            }
        }
        if readiness_complete(tracker.status(), &diagnostics)? {
            if detect_startup_gates {
                ensure_no_startup_operator_gate(session, spec)?;
            }
            return tracker
                .into_permit()
                .ok_or_else(|| "ready tracker did not issue a permit".to_owned());
        }
    }
}

fn attach_readiness_boundary(
    session: &PtySession,
) -> Result<(PtyTerminalSnapshot, PtyAttachment), String> {
    let terminal = session.terminal_state().map_err(|error| error.to_string())?;
    let cursor = PtyReplayCursor {
        provider_revision: terminal.provider_revision.clone(),
        generation: terminal.generation,
        next_sequence: terminal.sequence.saturating_add(1).max(1),
    };
    let attachment = session
        .attach_events(cursor)
        .map_err(|error| error.to_string())?;
    Ok((terminal, attachment))
}

fn seed_readiness_from_terminal(
    terminal: &PtyTerminalSnapshot,
    tracker: &mut ReadinessTracker<'_>,
    diagnostics: &mut ReadinessDiagnostics,
    elapsed_ms: u64,
) -> Result<(), String> {
    const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
    if terminal.bracketed_paste {
        diagnostics.observe_output(ENABLE_BRACKETED_PASTE);
        tracker.observe_output(ENABLE_BRACKETED_PASTE, elapsed_ms);
    }
    if !terminal.formatted.is_empty() {
        diagnostics.observe_output(&terminal.formatted);
        tracker.observe_output(&terminal.formatted, elapsed_ms);
    }
    Ok(())
}

const STARTUP_GATE_SETTLE_MS: u64 = 350;
const STARTUP_GATE_POLL_MS: u64 = 25;
/// Steady-state cadence for `NativeEffectShell::reclassify_foreground`.
///
/// `wait_for_readiness` already polls the foreground far tighter than this
/// during startup (`spec.readiness.poll_interval_ms`, 150ms by default),
/// because a session takes at most a few seconds to come up and getting
/// that window right matters. This constant is not that -- it governs the
/// STEADY STATE: a session that has been sitting non-`Ready` for a long
/// time, which is exactly the incident this module fixes (an update
/// screen with nobody watching it). A session that reaches `Ready` stops
/// being probed at all (see `foreground_probe_schedule`), so the cost of
/// this interval is bounded by the count of sessions that are NOT
/// `Ready`, never by output rate and never by total session count. One
/// process-tree walk a second for a handful of stuck sessions is ample.
const FOREGROUND_RECLASSIFY_INTERVAL: Duration = Duration::from_secs(1);
// Codex rust-v0.144.0 keeps Enter in newline mode for 120 ms after
// Windows paste-burst activity. Wait beyond that window only after the TUI
// visibly incorporates the deferred initial prompt.
const CODEX_PASTE_ENTER_SUPPRESSION_MS: u64 = 120;
const CODEX_POST_RENDER_MARGIN_MS: u64 = 30;
const CODEX_SESSION_STATUS_PROBE_TIMEOUT_MS: u64 = 5_000;
const KIMI_SESSION_STATUS_PROBE_TIMEOUT_MS: u64 = 5_000;

async fn probe_fresh_codex_session_identity(
    session: &PtySession,
    spec: &AgentSpec,
) -> Result<Option<ProviderSessionIdentity>, String> {
    let permit = match wait_for_readiness(session, spec, ReadinessIntent::DraftPaste, true).await {
        Ok(permit) => permit,
        Err(_) => return Ok(None),
    };
    if wait_for_startup_operator_gate(session, spec).await.is_err() {
        return Ok(None);
    }
    let baseline = session
        .terminal_state()
        .map_err(|error| error.to_string())?;
    let mut extractor = CodexPtySessionIdentityExtractor::default();
    session
        .send_input_action(
            InputAction::AgentCommand(AgentCommand {
                agent_id: spec.id.clone(),
                name: "status".to_owned(),
                arguments: Vec::new(),
            }),
            permit,
        )
        .await
        .map_err(|error| format!("Codex /status identity probe failed: {error}"))?;

    let deadline = Instant::now()
        + Duration::from_millis(
            spec.readiness
                .timeout_ms
                .min(CODEX_SESSION_STATUS_PROBE_TIMEOUT_MS)
                .max(1),
        );
    loop {
        let snapshot = session
            .terminal_state()
            .map_err(|error| error.to_string())?;
        if snapshot.sequence > baseline.sequence {
            if let Some(identity) = extractor.observe_screen(&snapshot.contents) {
                return Ok(Some(identity));
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(STARTUP_GATE_POLL_MS)).await;
    }
}

async fn probe_fresh_kimi_session_identity(
    session: &PtySession,
    spec: &AgentSpec,
) -> Result<Option<ProviderSessionIdentity>, String> {
    let permit = match wait_for_readiness(
        session,
        spec,
        ReadinessIntent::FollowupPrompt,
        true,
    )
    .await
    {
        Ok(permit) => permit,
        Err(_) => return Ok(None),
    };
    if wait_for_startup_operator_gate(session, spec).await.is_err() {
        return Ok(None);
    }
    let baseline = session
        .terminal_state()
        .map_err(|error| error.to_string())?;
    let mut extractor = KimiPtySessionIdentityExtractor::default();
    if let Some(identity) = extractor.observe_screen(&baseline.contents) {
        return Ok(Some(identity));
    }
    session
        .send_input_action(
            InputAction::SubmitPrompt(PromptPayload {
                text: "/status".to_owned(),
                framing: PromptFraming::BracketedPaste,
            }),
            permit,
        )
        .await
        .map_err(|error| format!("Kimi /status identity probe failed: {error}"))?;

    let deadline = Instant::now()
        + Duration::from_millis(
            spec.readiness
                .timeout_ms
                .min(KIMI_SESSION_STATUS_PROBE_TIMEOUT_MS)
                .max(1),
        );
    loop {
        let snapshot = session
            .terminal_state()
            .map_err(|error| error.to_string())?;
        if snapshot.sequence > baseline.sequence {
            if let Some(identity) = extractor.observe_screen(&snapshot.contents) {
                return Ok(Some(identity));
            }
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(STARTUP_GATE_POLL_MS)).await;
    }
}

async fn deliver_pending_initial_prompt(
    session: &mut PtySession,
    spec: &AgentSpec,
) -> Result<(), String> {
    if session.pending_followup_prompt().is_none() {
        return Ok(());
    }
    let mut permit =
        wait_for_readiness(session, spec, ReadinessIntent::FollowupPrompt, true).await?;
    wait_for_startup_operator_gate(session, spec).await?;
    let render_confirmed_submit = spec.id.as_str() == "claude"
        || (RuntimePlatform::current() == RuntimePlatform::Windows
            && spec.id.as_str() == "codex");
    if render_confirmed_submit {
        let prompt = session
            .pending_followup_prompt()
            .ok_or_else(|| "deferred initial prompt disappeared before paste".to_owned())?
            .to_owned();
        let baseline = session
            .terminal_state()
            .map_err(|error| error.to_string())?;
        let inserted = session
            .insert_pending_followup_prompt(PromptFraming::BracketedPaste, &permit)
            .await
            .map_err(|error| error.to_string())?;
        if !inserted {
            return Err("deferred initial prompt was not inserted".to_owned());
        }
        wait_for_prompt_render(
            session,
            spec,
            &prompt,
            &baseline,
            spec.readiness.timeout_ms,
        )
        .await?;
        if RuntimePlatform::current() == RuntimePlatform::Windows && spec.id.as_str() == "codex" {
            tokio::time::sleep(Duration::from_millis(
                CODEX_PASTE_ENTER_SUPPRESSION_MS + CODEX_POST_RENDER_MARGIN_MS,
            ))
            .await;
        }
        permit = wait_for_readiness(session, spec, ReadinessIntent::FollowupPrompt, true).await?;
        wait_for_startup_operator_gate(session, spec).await?;
    }
    ensure_no_startup_operator_gate(session, spec)?;
    let submitted = session
        .submit_pending_followup(PromptFraming::BracketedPaste, permit)
        .await
        .map_err(|error| error.to_string())?;
    if !submitted {
        return Err("deferred initial prompt disappeared before delivery".to_owned());
    }
    Ok(())
}

async fn wait_for_prompt_render(
    session: &PtySession,
    spec: &AgentSpec,
    prompt: &str,
    baseline: &PtyTerminalSnapshot,
    timeout_ms: u64,
) -> Result<(), String> {
    let probe = prompt_render_probe(&gate4agent_types::sanitize_prompt_text(prompt));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    loop {
        let snapshot = session.terminal_state().map_err(|error| error.to_string())?;
        if let Some(gate) = startup_operator_gate(&snapshot.contents) {
            return Err(startup_operator_error(spec, &gate));
        }
        if prompt_rendered(&snapshot, baseline, &probe) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let tail = terminal_tail(&snapshot.contents);
            let compact_tail = compact_alphanumeric(&tail);
            let baseline_tail = terminal_tail(&baseline.contents).to_ascii_lowercase();
            let normalized_tail = tail.to_ascii_lowercase();
            let retained = render_ack_event_summary(session, baseline.sequence);
            let foreground = session.observe_foreground().await.ok();
            let foreground_name = foreground
                .as_ref()
                .and_then(|observation| observation.readiness.process_name.as_deref())
                .map(safe_process_label)
                .unwrap_or_else(|| "none".to_owned());
            let child_live = retained.exit_code.is_none() && foreground.is_some();
            return Err(format!(
                "agent '{}' initial prompt paste was not rendered before submit; Enter was not sent (baseline_sequence={} current_sequence={} sequence_delta={} cursor_changed={} bracketed_baseline={} bracketed_current={} tail_chars={} probe_chars={} probe_match={} placeholder_baseline={} placeholder_current={} child_live={} foreground={} retained_output={} retained_foreground={} retained_snapshots={} retained_resized={} retained_gaps={} retained_reader_errors={} retained_operator_actions={} retained_exit_code={} output_flags={})",
                spec.id,
                baseline.sequence,
                snapshot.sequence,
                snapshot.sequence.saturating_sub(baseline.sequence),
                snapshot.cursor != baseline.cursor,
                baseline.bracketed_paste,
                snapshot.bracketed_paste,
                tail.chars().count(),
                probe.chars().count(),
                !probe.is_empty() && compact_tail.contains(&probe),
                paste_placeholder_visible(&baseline_tail),
                paste_placeholder_visible(&normalized_tail),
                child_live,
                foreground_name,
                retained.output,
                retained.foreground,
                retained.snapshots,
                retained.resized,
                retained.gaps,
                retained.reader_errors,
                retained.operator_actions,
                retained
                    .exit_code
                    .map_or_else(|| "none".to_owned(), |code| code.to_string()),
                if retained.output_flags.is_empty() {
                    "none".to_owned()
                } else {
                    retained.output_flags.join(",")
                },
            ));
        }
        tokio::time::sleep(Duration::from_millis(STARTUP_GATE_POLL_MS)).await;
    }
}

#[derive(Default)]
struct RenderAckEventSummary {
    output: usize,
    foreground: usize,
    snapshots: usize,
    resized: usize,
    gaps: usize,
    reader_errors: usize,
    operator_actions: usize,
    exit_code: Option<i32>,
    output_flags: Vec<&'static str>,
}

fn render_ack_event_summary(session: &PtySession, baseline_sequence: u64) -> RenderAckEventSummary {
    let mut summary = RenderAckEventSummary::default();
    let Ok(attachment) = session.attach_retained_events() else {
        return summary;
    };
    for envelope in attachment
        .replay
        .into_iter()
        .filter(|envelope| envelope.sequence > baseline_sequence)
    {
        match envelope.event {
            PtyEvent::Output(bytes) => {
                summary.output = summary.output.saturating_add(1);
                observe_render_ack_output_flags(&mut summary.output_flags, &bytes);
            }
            PtyEvent::ForegroundProcess(_) => {
                summary.foreground = summary.foreground.saturating_add(1);
            }
            PtyEvent::SnapshotAvailable { .. } => {
                summary.snapshots = summary.snapshots.saturating_add(1);
            }
            PtyEvent::Resized(_) => {
                summary.resized = summary.resized.saturating_add(1);
            }
            PtyEvent::DataGap { .. } => {
                summary.gaps = summary.gaps.saturating_add(1);
            }
            PtyEvent::ReaderError { .. } => {
                summary.reader_errors = summary.reader_errors.saturating_add(1);
            }
            PtyEvent::OperatorActionRequired { .. } => {
                summary.operator_actions = summary.operator_actions.saturating_add(1);
            }
            PtyEvent::Exited { code } => summary.exit_code = Some(code),
            PtyEvent::Started => {}
        }
    }
    summary
}

fn observe_render_ack_output_flags(flags: &mut Vec<&'static str>, bytes: &[u8]) {
    let normalized = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    for (label, marker) in [
        ("error", "error"),
        ("panic", "panic"),
        ("fatal", "fatal"),
        ("login", "login"),
        ("auth", "auth"),
        ("permission", "permission"),
        ("rate-limit", "rate limit"),
        ("usage-limit", "usage limit"),
        ("update", "update"),
        ("working", "working"),
        ("thinking", "thinking"),
    ] {
        if normalized.contains(marker) && !flags.contains(&label) {
            flags.push(label);
        }
    }
}

fn safe_process_label(process_name: &str) -> String {
    process_name
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '?'
            }
        })
        .collect()
}

fn prompt_rendered(
    snapshot: &PtyTerminalSnapshot,
    baseline: &PtyTerminalSnapshot,
    probe: &str,
) -> bool {
    if snapshot.sequence <= baseline.sequence {
        return false;
    }
    let tail = terminal_tail(&snapshot.contents);
    let compact = compact_alphanumeric(&tail);
    if !probe.is_empty() && compact.contains(probe) {
        return true;
    }
    let normalized_tail = tail.to_ascii_lowercase();
    let normalized_baseline = terminal_tail(&baseline.contents).to_ascii_lowercase();
    paste_placeholder_visible(&normalized_tail)
        && !paste_placeholder_visible(&normalized_baseline)
}

fn paste_placeholder_visible(normalized_tail: &str) -> bool {
    // Codex collapses larger pastes as `[Pasted Content ...]`; Claude 2.1.223
    // uses `[Pasted text #N ...]`. Treat only those vendor-owned render
    // markers as proof that the TUI consumed the bracketed paste.
    normalized_tail.contains("[pasted content")
        || normalized_tail.contains("[pasted text #")
}

fn terminal_tail(contents: &str) -> String {
    let mut lines = contents.lines().rev().take(12).collect::<Vec<_>>();
    lines.reverse();
    lines.join("\n")
}

fn prompt_render_probe(prompt: &str) -> String {
    let compact = compact_alphanumeric(prompt);
    let chars = compact.chars().collect::<Vec<_>>();
    chars[chars.len().saturating_sub(32)..].iter().collect()
}

fn compact_alphanumeric(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

async fn wait_for_startup_operator_gate(
    session: &PtySession,
    spec: &AgentSpec,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(STARTUP_GATE_SETTLE_MS);
    loop {
        ensure_no_startup_operator_gate(session, spec)?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        tokio::time::sleep(
            deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(STARTUP_GATE_POLL_MS)),
        )
        .await;
    }
}

/// Refuses a prompt submission while a known operator gate is on screen.
///
/// Deliberately checks gates ONLY, never `screen_failure`. A crash banner
/// means "the CLI fell over" solely before the agent has rendered its own
/// UI; afterwards the identical bytes mean "the agent is showing you a
/// crash" -- a test it ran, a traceback it was asked to explain. What
/// separates the two is not the text but WHEN it appeared, and this
/// function has no way to date its evidence: it holds a screen snapshot
/// and a spec, and is called from seven sites spanning both the startup
/// window and long-running follow-up delivery. A check that cannot tell
/// those apart would refuse work from a perfectly healthy session every
/// time its pane happened to show a failing subprocess.
///
/// The `Failing` classification is not lost by this -- it is computed
/// where the window IS known (`collect_terminal_frames`, gated on
/// `OwnedPtySession::ever_reached_ready`), published upstream, and enforced
/// by the callers that gate dispatch on `PtyScreenState`. Gates are
/// different in kind and stay here: a trust or update prompt is legitimate
/// at any point in a session's life, so it needs no window to be believed.
fn ensure_no_startup_operator_gate(session: &PtySession, spec: &AgentSpec) -> Result<(), String> {
    let snapshot = session.terminal_state().map_err(|error| error.to_string())?;
    match startup_operator_gate(&snapshot.contents) {
        Some(gate) => Err(startup_operator_error(spec, &gate)),
        None => Ok(()),
    }
}

fn startup_operator_error(spec: &AgentSpec, gate: &OperatorGateState) -> String {
    format!(
        "agent '{}' requires operator action at startup ({gate}); initial prompt was not submitted",
        spec.id
    )
}

fn ensure_no_readiness_operator_gate(
    diagnostics: &ReadinessDiagnostics,
    spec: &AgentSpec,
) -> Result<(), String> {
    match &diagnostics.operator_gate {
        Some(gate) => Err(startup_operator_error(spec, gate)),
        None => Ok(()),
    }
}

/// Whitespace-collapse and lowercase raw PTY screen text before pattern
/// matching. Shared by `startup_operator_gate` and `screen_failure` so the
/// two agree on what "the same phrase" means -- if they normalized
/// independently, a line-wrap difference or a run of extra spaces could
/// make one matcher see a phrase the other misses for no reason connected
/// to the actual screen content.
fn normalize_screen_text(contents: &str) -> String {
    contents
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Phrases showing a package manager actively installing or updating a
/// package. Generic across npm/pip/yarn/pnpm rather than any one vendor's
/// package name, so a wrapper script fronting any provider CLI is
/// recognized the same way -- see the `PACKAGE_MANAGER_UPDATE_COMPLETION_MARKERS`
/// doc below for why a bare occurrence of one of these alone is not enough.
const PACKAGE_MANAGER_INSTALL_MARKERS: &[&str] = &[
    "npm install -g",
    "npm i -g",
    "npm update -g",
    "pip install --upgrade",
    "pip install -u",
    "yarn global add",
    "pnpm add -g",
];

/// Phrases showing the install/update above reached a terminal state and
/// wants the CLI relaunched. Required to co-occur with a marker from
/// `PACKAGE_MANAGER_INSTALL_MARKERS` so a screen that merely MENTIONS a
/// package-manager command (an agent explaining how to install something,
/// for instance) never matches on the install phrase alone.
const PACKAGE_MANAGER_UPDATE_COMPLETION_MARKERS: &[&str] = &[
    "update ran successfully",
    "please restart",
    "update complete",
    "updated successfully",
];

/// Builds the classified `OperatorGateState` for a matched `kind`/`subject`
/// pair, filling `input`/`options` from whatever `parse_operator_gate_options`
/// can read off the RAW (un-normalized, multi-line) screen text. Every
/// `startup_operator_gate` match arm goes through this one function so
/// option-list recognition is uniform across every gate kind rather than
/// hand-wired per branch -- a kind this module has not yet seen an option
/// layout for simply gets `OperatorGateInput::Unknown` and no options,
/// exactly like a kind whose options this parser fails to recognize on a
/// given screen; there is no special-casing between "not implemented yet"
/// and "not recognized this time".
fn operator_gate(kind: OperatorGateKind, subject: OperatorGateSubject, contents: &str) -> OperatorGateState {
    let (input, options) = parse_operator_gate_options(contents);
    OperatorGateState::new(kind)
        .with_subject(subject)
        .with_options(input, options)
}

fn startup_operator_gate(contents: &str) -> Option<OperatorGateState> {
    let normalized = normalize_screen_text(contents);
    if [
        "trust this folder",
        "trust the files in this folder",
        "trust the contents of this directory",
        "do you trust this directory",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        return Some(operator_gate(
            OperatorGateKind::WorkspaceTrust,
            OperatorGateSubject::Directory { path: None },
            contents,
        ));
    }
    if normalized.contains("quick safety check")
        && (normalized.contains("yes, i trust this folder")
            || normalized.contains("continue without these permissions"))
    {
        return Some(operator_gate(
            OperatorGateKind::WorkspaceTrust,
            OperatorGateSubject::Directory { path: None },
            contents,
        ));
    }
    // A CLI's own hook-trust prompt at startup (seen from Codex): a set of
    // shell hooks it discovered need to be reviewed/trusted before they are
    // allowed to run, distinct from `WorkspaceTrust` above -- that gate is
    // about trusting the PROJECT DIRECTORY, this one is about trusting
    // SHELL HOOKS the CLI found inside it, and an operator reading `kind`
    // should be able to tell which question is being asked. Required to
    // co-occur with the screen's own "decline" option rather than matching
    // on "hooks need review" alone, so an agent's ordinary narration that
    // merely uses the word "hooks" (explaining a git hook, a React hook, a
    // build hook) never matches: real narration essentially never also
    // contains the literal refusal phrasing "continue without trusting"
    // this same prompt renders. The number of hooks reported (which varies
    // run to run, and is not read into `subject`'s `count` -- no matcher
    // here parses it off the screen) plays no part in the match.
    if normalized.contains("hooks need review") && normalized.contains("continue without trusting")
    {
        return Some(operator_gate(
            OperatorGateKind::HookTrust,
            OperatorGateSubject::Hooks { count: None },
            contents,
        ));
    }
    if [
        "select authentication method",
        "choose how to authenticate",
        "no auth type is selected",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        return Some(operator_gate(
            OperatorGateKind::Authentication,
            OperatorGateSubject::Account,
            contents,
        ));
    }
    if normalized.contains("sign in")
        && (normalized.contains("openai")
            || normalized.contains("chatgpt")
            || normalized.contains("codex"))
    {
        return Some(operator_gate(
            OperatorGateKind::Authentication,
            OperatorGateSubject::Account,
            contents,
        ));
    }
    // Claude's own login-method chooser: a numbered list ("1. Claude ...",
    // "2. API usage billing", "3. 3rd-party platform ...") rendered under
    // "Select login method:". It shares neither "sign in" nor
    // "openai"/"chatgpt"/"codex" with the branch above, so that branch
    // never catches it. The phrase itself is specific enough (no ordinary
    // agent narration renders this exact prompt line) to stand alone,
    // matching the precedent set by "select authentication method" and
    // "choose how to authenticate" above.
    if normalized.contains("select login method") {
        return Some(operator_gate(
            OperatorGateKind::Authentication,
            OperatorGateSubject::Account,
            contents,
        ));
    }
    // Claude's OAuth wait screen: a URL to open in a browser plus a slot to
    // paste back the authorization code once that flow completes. Required
    // to co-occur with "paste code" (the screen's own field label) rather
    // than matching on "sign in" alone -- "sign in" by itself is common
    // enough in an agent's ordinary narration to be untrustworthy, the same
    // reasoning the branch above already applies via its own companion
    // words. No option list is rendered here -- it is one free-text slot,
    // not a choice from a list -- so `input` is set directly to
    // `TextEntry` rather than routed through `operator_gate`'s option-list
    // parser, which would find nothing on this screen shape and fall back
    // to `Unknown`.
    if normalized.contains("sign in") && normalized.contains("paste code") {
        return Some(
            OperatorGateState::new(OperatorGateKind::Authentication)
                .with_subject(OperatorGateSubject::Account)
                .with_options(OperatorGateInput::TextEntry, Vec::new()),
        );
    }
    if normalized.contains("kimi code update available")
        && normalized.contains("install update now")
    {
        return Some(operator_gate(
            OperatorGateKind::VendorUpdate,
            OperatorGateSubject::Unknown,
            contents,
        ));
    }
    // A wrapper script self-updating via a package manager before the real
    // CLI ever launches -- the case that produced the incident this module
    // fixes: an npm-driven wrapper ran an update to completion and printed
    // nothing that looked like the agent, while every consumer still saw a
    // live, `status: running` PTY. This shares `VendorUpdate` with the Kimi
    // in-app case just above on purpose, not by omission: both are "the CLI
    // is updating itself", the only difference is WHICH process drives it
    // (the agent's own composer vs. a wrapper script's package-manager
    // install), and an operator reading `kind` should see one meaning for
    // that fact regardless of which vendor's update mechanism produced it.
    if PACKAGE_MANAGER_INSTALL_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
        && PACKAGE_MANAGER_UPDATE_COMPLETION_MARKERS
            .iter()
            .any(|marker| normalized.contains(marker))
    {
        return Some(operator_gate(
            OperatorGateKind::VendorUpdate,
            OperatorGateSubject::Unknown,
            contents,
        ));
    }
    if normalized.contains("choose the text style that looks best with your terminal") {
        return Some(operator_gate(
            OperatorGateKind::TerminalAppearance,
            OperatorGateSubject::Appearance,
            contents,
        ));
    }
    if normalized.contains("welcome to claude code for")
        && (normalized.contains("open files") || normalized.contains("selected lines"))
    {
        return Some(operator_gate(
            OperatorGateKind::Onboarding,
            OperatorGateSubject::Unknown,
            contents,
        ));
    }
    if normalized.contains("welcome to claude code")
        && (normalized.contains("press enter") || normalized.contains("enter to continue"))
    {
        return Some(operator_gate(
            OperatorGateKind::Onboarding,
            OperatorGateSubject::Unknown,
            contents,
        ));
    }
    if normalized.contains("migration") && normalized.contains("enter confirm") && normalized.contains("esc")
    {
        return Some(operator_gate(
            OperatorGateKind::ConfigurationMigration,
            OperatorGateSubject::Unknown,
            contents,
        ));
    }
    None
}

/// Reads the recognized on-screen choice list, if any, off the RAW
/// (multi-line, un-normalized) screen text -- called uniformly by every
/// `startup_operator_gate` match arm through `operator_gate`. Recognizes
/// exactly two rendered shapes, both observed verbatim on a live stand:
///
/// - a NUMBERED list (Codex's hook-trust prompt): each option line starts,
///   after an optional leading cursor glyph (`›`) and whitespace, with
///   `N.`; `Some` lines from `parse_numbered_gate_option_line` win outright
///   over the arrow shape below, so a screen that happens to also contain an
///   arrow glyph elsewhere (a Codex composer prompt sharing the pane, say)
///   is still read as the numbered list it actually is.
/// - an ARROW list (Kimi's workspace-trust prompt): the currently selected
///   option carries a leading `❯`; the other options carry no glyph at all,
///   so they are told apart from the description line rendered under each
///   one (`"Enable project MCP servers. Remembered for this folder."` under
///   `"Trust this folder"`, for instance) by `parse_arrow_gate_option_line`'s
///   own filter -- see that function's doc comment for the exact rule.
///
/// Neither line is stable across CLIs, and this parser recognizes nothing
/// else: a screen matching neither shape returns `(Unknown, vec![])`, never
/// a guess built from partial matches.
fn parse_operator_gate_options(contents: &str) -> (OperatorGateInput, Vec<OperatorGateOption>) {
    let numbered: Vec<OperatorGateOption> = contents
        .lines()
        .filter_map(parse_numbered_gate_option_line)
        .take(OPERATOR_GATE_OPTIONS_MAX)
        .collect();
    if !numbered.is_empty() {
        return (OperatorGateInput::NumberedList, numbered);
    }
    let arrow: Vec<OperatorGateOption> = contents
        .lines()
        .filter_map(parse_arrow_gate_option_line)
        .take(OPERATOR_GATE_OPTIONS_MAX)
        .collect();
    if !arrow.is_empty() {
        return (OperatorGateInput::ArrowList, arrow);
    }
    (OperatorGateInput::Unknown, Vec::new())
}

/// Parses one line of a numbered option list -- `"› 1. Review hooks"`,
/// `"  2. Trust all and continue"` -- into `(selected, text)`. `selected` is
/// true only when the line carries the leading `›` cursor glyph Codex draws
/// on the highlighted row; every other line in the same list has none.
/// Returns `None` for any line that, once a leading glyph is stripped, does
/// not start with `<digits>.` -- an instruction line like `"Press enter to
/// confirm or esc to go back"` is exactly this shape and is meant to fall
/// through untouched.
/// Cursor glyphs vendors draw ahead of the selected row of an option list.
/// Kept as one set shared by both list parsers: which glyph a vendor picks
/// says nothing about whether its list is numbered or marker-only, and
/// teaching one parser a glyph the other does not know is exactly how the
/// selected row went missing before.
const GATE_CURSOR_GLYPHS: [char; 2] = ['\u{203a}', '\u{276f}'];

fn parse_numbered_gate_option_line(line: &str) -> Option<OperatorGateOption> {
    let trimmed = line.trim_start();
    // Vendors mark the cursor row of a NUMBERED list with the marker BEFORE
    // the number (`> 2. Dark mode`), and they do not agree on the glyph:
    // Codex draws U+203A, Claude draws U+276F, and both appear ahead of the
    // digit rather than ahead of the text. Recognizing only one of them cost
    // the whole row -- the cursor line failed to parse as an option at all,
    // so the list came back one item short and with nothing marked selected,
    // observed live on Claude's theme chooser and Codex's login chooser.
    let (selected, rest) = match trimmed.strip_prefix(GATE_CURSOR_GLYPHS) {
        Some(stripped) => (true, stripped.trim_start()),
        None => (false, trimmed),
    };
    let digits_end = rest.find(|character: char| !character.is_ascii_digit()).unwrap_or(0);
    if digits_end == 0 {
        return None;
    }
    let text = rest[digits_end..].strip_prefix('.')?.trim();
    if text.is_empty() {
        return None;
    }
    Some(OperatorGateOption {
        semantics: classify_operator_gate_option_semantics(text),
        text: text.to_owned(),
        selected,
    })
}

/// Parses one line of an arrow-navigated option list -- `"❯ Don't trust"`,
/// `"  Trust this folder"` -- into `(selected, text)`. `selected` is true
/// only when the line carries the leading `❯` cursor glyph.
///
/// Distinguishing an option TITLE from the description/header/instruction
/// prose around it (`"Enable project MCP servers. Remembered for this
/// folder."` under `"Trust this folder"`; `"Do you trust the files in this
/// folder?"` above it; `"Press enter to confirm or esc to go back"` below a
/// DIFFERENT screen's numbered list entirely) cannot rely on the glyph
/// alone, since only the currently-selected title ever carries one. A
/// candidate line is accepted as an option only if EITHER it carries the
/// `❯` marker (the screen's own cursor is unambiguous proof this is a real,
/// currently-selected option, whatever its wording), OR it both looks like
/// a short menu label rather than a sentence (no `.`, `?`, `!`, or `:`
/// anywhere in it -- a description or instruction is a full sentence
/// carrying one of these; an option label like `"Trust this folder"` or
/// `"Don't trust"` never does) AND its own text uses one of the recognized
/// option verbs (`classify_operator_gate_option_semantics` returns
/// something other than `Unknown`) -- an unmarked line whose wording this
/// module does not recognize as a choice-verb is left as prose rather than
/// guessed into an option with `semantics: Unknown`. The arrow-key legend
/// itself (`"↑↓ navigate · Enter select · Esc exit"`) is excluded
/// explicitly rather than relying on either rule catching it.
fn parse_arrow_gate_option_line(line: &str) -> Option<OperatorGateOption> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|character| matches!(character, '.' | '?' | '!' | ':'))
    {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains('\u{2191}') || lower.contains('\u{2193}') || lower.contains("navigate") {
        return None;
    }
    let (selected, rest) = match trimmed.strip_prefix(GATE_CURSOR_GLYPHS) {
        Some(stripped) => (true, stripped.trim()),
        None => (false, trimmed),
    };
    if rest.is_empty() || !rest.chars().any(|character| character.is_alphabetic()) {
        return None;
    }
    let semantics = classify_operator_gate_option_semantics(rest);
    if !selected && semantics == OperatorGateOptionSemantics::Unknown {
        return None;
    }
    Some(OperatorGateOption {
        semantics,
        text: rest.to_owned(),
        selected,
    })
}

/// Infers what choosing a given on-screen option does from the verb in its
/// OWN text -- never from its position or number, since neither is stable
/// across CLIs or screen wraps. Decline phrasing is checked before accept
/// phrasing so `"Continue without trusting"` (contains both "continue" and
/// "without trusting") reads as `Decline`, matching what the option
/// actually does; an option whose text uses none of these verbs classifies
/// `Unknown` rather than a guessed default.
fn classify_operator_gate_option_semantics(text: &str) -> OperatorGateOptionSemantics {
    let lower = text.to_ascii_lowercase();
    if lower.contains("review") {
        OperatorGateOptionSemantics::Inspect
    } else if lower.contains("don't trust")
        || lower.contains("do not trust")
        || lower.contains("without trusting")
        || lower.contains("decline")
    {
        OperatorGateOptionSemantics::Decline
    } else if lower.contains("trust") || lower.contains("continue") || lower.contains("proceed") {
        OperatorGateOptionSemantics::Accept
    } else if lower.contains("exit") || lower.contains("quit") || lower.contains("cancel") {
        OperatorGateOptionSemantics::Exit
    } else {
        OperatorGateOptionSemantics::Unknown
    }
}

/// True if `line` ends in a bare shell-prompt character -- used only to
/// raise confidence on the one `screen_failure` marker
/// (`"no such file or directory"`) that is otherwise too generic to trust
/// alone; see that bucket's own comment.
fn looks_like_shell_prompt_line(line: &str) -> bool {
    matches!(line.trim_end().chars().last(), Some('$' | '%' | '#' | '>'))
}

/// Screens where the foreground process already matches the agent (this is
/// NOT the `NotAgent` process-mismatch case) but the screen itself shows
/// the process came up wrong or fell over. Nothing typed into THIS screen
/// fixes it, which is the line that separates `Failing` from
/// `OperatorGate`: a gate is resolved by typing an answer into the pane, a
/// failure is not.
///
/// Every marker here is a crash SHAPE -- a stack-trace banner, a shell's
/// own "command not found" line -- and as raw bytes a crash shape is
/// indistinguishable from an agent CHOOSING to render that same text while
/// explaining, or running, someone else's failure (`cargo test` hitting a
/// failing assertion, a script the agent ran that threw, a bug report it
/// was asked to read). Text alone cannot tell "the CLI crashed" from "the
/// CLI is showing you a crash" -- both put identical bytes on screen. The
/// one window where these markers ARE trustworthy is before this
/// generation's session has ever reached `Ready`: with no prior render of
/// the agent's own UI, there is nothing else on the screen that could have
/// produced a crash banner, so it genuinely means the launch failed. That
/// is why this function's caller in `collect_terminal_frames` stops
/// calling it at all once `Ready`, rather than trying to make the matching
/// itself tell healthy narration apart from a real crash -- no marker set
/// can do that from bytes alone.
fn screen_failure(contents: &str) -> Option<&'static str> {
    let normalized = normalize_screen_text(contents);

    // A language runtime's own uncaught-exception banner -- fixed strings
    // a runtime prints verbatim at the head of a crash dump, not phrasing
    // a chat transcript would casually reproduce in this exact shape.
    // `"fatal error:"` is deliberately NOT included: it is also what a
    // C/C++ compiler prints for an ordinary missing-header build error, and
    // a wrapper script's build step can emit that while the real CLI still
    // comes up fine afterwards -- untrustworthy even in the startup window
    // without a second signal this function does not have.
    if ["panicked at", "unhandled exception", "traceback (most recent call last)"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        return Some("crash");
    }

    // A shell reporting that the launch command itself does not exist.
    // The first two are OS/shell-owned sentence shapes on their own. The
    // third, `"no such file or directory"`, is common enough in ordinary
    // ENOENT discussion that it needs a companion signal -- a line that
    // itself ends in a bare shell prompt -- before it counts.
    if normalized.contains("command not found")
        || normalized.contains("is not recognized as an internal or external command")
        || (normalized.contains("no such file or directory")
            && contents.lines().any(looks_like_shell_prompt_line))
    {
        return Some("missing command");
    }

    // A provider CLI's own missing-credential banner at launch -- Grok with
    // no key configured prints exactly this one line and never renders
    // anything else again, which makes it a crash SHAPE in the same sense
    // as the buckets above: nothing typed into this screen fixes it, only a
    // restart with a credential configured. Required to co-occur with the
    // literal `GROK_API_KEY` env var name rather than matching "api key
    // required" alone -- that phrase in the abstract is common enough in an
    // agent's own prose (explaining a DIFFERENT provider's setup, say) to be
    // untrustworthy alone; the exact env var name is specific to this one
    // banner.
    if normalized.contains("api key required") && normalized.contains("grok_api_key") {
        return Some("missing api key");
    }

    None
}

/// The window gate on `screen_failure`, factored into its own pure
/// function so the "stop trusting crash markers once this generation has
/// ever been `Ready`" rule is unit-testable without constructing a live
/// `OwnedPtySession` (which owns a real PTY and cannot be built in a unit
/// test). `collect_terminal_frames` calls this, not `screen_failure`
/// directly.
fn screen_failure_for_generation(contents: &str, ever_reached_ready: bool) -> Option<&'static str> {
    if ever_reached_ready {
        None
    } else {
        screen_failure(contents)
    }
}

/// The foreground half of the classification, resolved by the caller so
/// the merge itself stays a pure function.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ForegroundVerdict {
    /// Foreground is the agent's own binary, or a tolerated wrapper.
    Agent,
    /// Foreground is something else. Carries what was actually seen.
    Foreign { process: String },
}

/// Merge the foreground and text signals into one screen classification.
///
/// The ORDER below, not the branches, is the part worth reading closely:
///
/// 1. A foreign foreground wins outright, even over a matching gate or
///    failure text pattern, because the process signal is structurally
///    exhaustive in the one way that matters -- it never needs to
///    recognize an updater's specific wording to say "this is not the
///    agent". A vendor wrapper whose text this module's matchers do not
///    yet know about still gets caught here.
/// 2. With no foreign foreground, `Failing` outranks `OperatorGate`: a
///    crashed screen can still have a leftover gate prompt sitting in
///    view above the crash dump, and "broken" is the more urgent of the
///    two truths for an operator to hear -- reporting "waiting for input"
///    about a session that has actually fallen over sends someone to type
///    into a pane that cannot use it.
/// 3. `OperatorGate` next, for the "resolvable by typing" reason
///    documented on `PtyScreenState` itself.
/// 4. With no foreground observation at all, the answer is `Unknown`, not
///    `Ready` -- a clean text read is NEVER, by itself, proof the screen
///    is the agent's. `Ready` requires the foreground to have been
///    checked AND to have matched, per the asymmetry documented on
///    `PtyScreenState::Ready`.
/// 5. Only once foreground is confirmed AND no gate/failure text matched
///    does the merge land on `Ready`.
fn classify_pty_screen_state(
    foreground: Option<&ForegroundVerdict>,
    gate: Option<&OperatorGateState>,
    failure: Option<&'static str>,
) -> PtyScreenState {
    if let Some(ForegroundVerdict::Foreign { process }) = foreground {
        return PtyScreenState::NotAgent {
            observed_process: process.clone(),
        };
    }
    if let Some(reason) = failure {
        return PtyScreenState::Failing {
            reason: reason.to_owned(),
        };
    }
    if let Some(gate) = gate {
        return PtyScreenState::OperatorGate { gate: gate.clone() };
    }
    if foreground.is_some() {
        PtyScreenState::Ready
    } else {
        PtyScreenState::Unknown
    }
}

/// Resolve what an OS process-tree observation means for classification,
/// reusing the exact matchers `ReadinessTracker::observe_foreground` trusts
/// at startup so this module's tolerance for a spawning wrapper
/// (`node`/`python`/`python3`) never drifts from readiness's own -- a false
/// `NotAgent` on a legitimate wrapper would be a regression on what
/// readiness already tolerates.
///
/// Deliberately looser than `ReadinessTracker::observe_foreground` in one
/// respect: it does not gate the wrapper tolerance behind
/// `has_child_processes`/a poll-count threshold. Those exist there to
/// decide "confident enough to flip session status to Running" from a
/// process signal ALONE. Here the text signal still has to independently
/// agree before the merge can reach `Ready` (see `classify_pty_screen_state`),
/// so being more permissive about which process counts as plausible never
/// lets a bad screen through on the process signal by itself.
fn resolve_foreground_verdict(
    spec: &AgentSpec,
    observation: &PtyForegroundObservation,
    platform: RuntimePlatform,
) -> ForegroundVerdict {
    let process_name = observation
        .readiness
        .process_name
        .as_deref()
        .unwrap_or(observation.observed_process.as_str());
    if is_expected_agent_process(spec, process_name, platform)
        || is_agent_foreground_wrapper(process_name, platform)
    {
        ForegroundVerdict::Agent
    } else {
        ForegroundVerdict::Foreign {
            process: observation.observed_process.clone(),
        }
    }
}

/// Whether `NativeEffectShell::reclassify_foreground` should schedule
/// another probe after a fresh classification, factored out as a small
/// pure function so the decision is testable without constructing a live
/// `OwnedPtySession` (which owns a real PTY and cannot be built in a unit
/// test).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForegroundProbeSchedule {
    /// The session reached `Ready` -- stop probing until text disagrees.
    Disarmed,
    /// Still unresolved, or freshly non-ready -- probe again after the interval.
    Armed,
}

fn foreground_probe_schedule(state: &PtyScreenState) -> ForegroundProbeSchedule {
    if matches!(state, PtyScreenState::Ready) {
        ForegroundProbeSchedule::Disarmed
    } else {
        ForegroundProbeSchedule::Armed
    }
}

/// Whether a text-only reclassification that just changed the merged state
/// (inside `collect_terminal_frames`) should force the foreground probe due
/// immediately, rather than waiting for its normal cadence. Factored out
/// for the same testability reason as `foreground_probe_schedule`: a
/// session that was `Ready` and whose text just started matching a gate or
/// failure pattern needs its foreground re-checked promptly, since the
/// last thing anyone probed was clean and the screen no longer agrees.
fn foreground_probe_rearms_immediately(previous: &PtyScreenState, next: &PtyScreenState) -> bool {
    matches!(previous, PtyScreenState::Ready) && !matches!(next, PtyScreenState::Ready)
}

fn observe_readiness_event(
    tracker: &mut ReadinessTracker<'_>,
    diagnostics: &mut ReadinessDiagnostics,
    event: PtyEvent,
    elapsed_ms: u64,
) -> Result<(), String> {
    match event {
        PtyEvent::Output(data) => {
            diagnostics.observe_output(&data);
            tracker.observe_output(&data, elapsed_ms);
        }
        PtyEvent::ForegroundProcess(observation) => {
            diagnostics.observe_foreground(&observation.readiness);
            tracker.observe_foreground(&observation.readiness, elapsed_ms);
        }
        PtyEvent::DataGap { .. } => {
            return Err("PTY replay gap prevents positive readiness proof".to_owned());
        }
        PtyEvent::ReaderError { message } | PtyEvent::OperatorActionRequired { message } => {
            return Err(message);
        }
        PtyEvent::Exited { code } => {
            return Err(format!("PTY exited with code {code} before readiness"));
        }
        PtyEvent::Started | PtyEvent::Resized(_) | PtyEvent::SnapshotAvailable { .. } => {}
    }
    Ok(())
}

#[derive(Default)]
struct ReadinessDiagnostics {
    draft_signal: Option<gate4agent_types::DraftReadySignal>,
    output_bytes: usize,
    output_chunks: usize,
    tail: Vec<u8>,
    saw_bracketed_paste: bool,
    saw_cursor_show: bool,
    saw_cursor_hide: bool,
    saw_alternate_screen: bool,
    saw_clear_screen: bool,
    saw_claude_composer: bool,
    saw_codex_composer: bool,
    saw_named_foreground: bool,
    operator_gate: Option<OperatorGateState>,
}

impl ReadinessDiagnostics {
    fn observe_output(&mut self, data: &[u8]) {
        const SIGNAL_TAIL_BYTES: usize = 4_096;
        self.output_bytes = self.output_bytes.saturating_add(data.len());
        self.output_chunks = self.output_chunks.saturating_add(1);
        let mut combined = Vec::with_capacity(self.tail.len().saturating_add(data.len()));
        combined.extend_from_slice(&self.tail);
        combined.extend_from_slice(data);
        self.saw_bracketed_paste |= readiness_bytes_contain(&combined, b"\x1b[?2004h");
        self.saw_cursor_show |= readiness_bytes_contain(&combined, b"\x1b[?25h");
        self.saw_cursor_hide |= readiness_bytes_contain(&combined, b"\x1b[?25l");
        self.saw_alternate_screen |= readiness_bytes_contain(&combined, b"\x1b[?1049h");
        self.saw_clear_screen |= readiness_bytes_contain(&combined, b"\x1b[2J");
        self.saw_claude_composer |= readiness_bytes_contain(&combined, "❯".as_bytes());
        self.saw_codex_composer |= readiness_bytes_contain(&combined, "›".as_bytes());
        let text = String::from_utf8_lossy(&combined);
        self.operator_gate = self
            .operator_gate
            .take()
            .or_else(|| startup_operator_gate(&strip_ansi_codes(&text)));
        self.tail = combined[combined.len().saturating_sub(SIGNAL_TAIL_BYTES)..].to_vec();
    }

    fn observe_foreground(&mut self, foreground: &gate4agent::agent::ForegroundObservation) {
        self.saw_named_foreground |= foreground.process_name.is_some();
    }

    fn summary(&self) -> String {
        format!(
            "draft_signal={:?} output_bytes={} output_chunks={} named_foreground={} bracketed_paste={} cursor_show={} cursor_hide={} alternate_screen={} clear_screen={} claude_composer={} codex_composer={} csi={}",
            self.draft_signal,
            self.output_bytes,
            self.output_chunks,
            self.saw_named_foreground,
            self.saw_bracketed_paste,
            self.saw_cursor_show,
            self.saw_cursor_hide,
            self.saw_alternate_screen,
            self.saw_clear_screen,
            self.saw_claude_composer,
            self.saw_codex_composer,
            readiness_csi_signatures(&self.tail),
        )
    }
}

fn readiness_csi_signatures(bytes: &[u8]) -> String {
    let mut signatures = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() && signatures.len() < 32 {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let mut end = index + 2;
        while end < bytes.len() && end.saturating_sub(index) <= 24 {
            let byte = bytes[end];
            if (0x40..=0x7e).contains(&byte) {
                let signature = String::from_utf8_lossy(&bytes[index + 2..=end]).into_owned();
                if !signatures.iter().any(|existing| existing == &signature) {
                    signatures.push(signature);
                }
                index = end;
                break;
            }
            if !(0x20..=0x3f).contains(&byte) {
                break;
            }
            end += 1;
        }
        index += 1;
    }
    if signatures.is_empty() {
        "none".to_owned()
    } else {
        signatures.join("|")
    }
}

fn readiness_bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn readiness_complete(
    status: ReadinessStatus,
    diagnostics: &ReadinessDiagnostics,
) -> Result<bool, String> {
    match status {
        ReadinessStatus::Waiting => Ok(false),
        ReadinessStatus::Ready(_) => Ok(true),
        ReadinessStatus::TimedOut => Err(format!(
            "PTY readiness timed out ({})",
            diagnostics.summary()
        )),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        classify_operator_gate_option_semantics,
        classify_pty_screen_state,
        foreground_probe_rearms_immediately, foreground_probe_schedule,
        host_policy_for_approval_level,
        parse_operator_gate_options, prepare_fresh_pty_provider_session,
        prompt_render_probe, prompt_rendered,
        reserve_provider_gap_sequence, resolve_foreground_verdict, screen_failure,
        screen_failure_for_generation, with_pty_terminal_capability_defaults,
        should_attach_pty_provider_stream, should_probe_pty_identity, startup_operator_gate,
        terminal_frame, terminal_frame_byte_len, terminal_state_capture_should_skip,
        validate_instance_launch_arguments, validate_spawn_runtime_policy,
        ForegroundProbeSchedule, ForegroundVerdict, ReadinessDiagnostics, RateLimitFeed,
        Utf8ChunkDecoder,
    };
    use gate4agent::HostPolicy;
    use gate4agent_adapters::builtin_adapter_registry;
    use gate4agent_catalog::EnvMutation;
    use gate4agent::agent::ForegroundObservation;
    use gate4agent::core::types::{
        AgentEvent, ContextWindowUsage as AgentContextWindowUsage, HostDecisionAuthority,
        HostRequestDecision, RateLimitType,
    };
    use gate4agent::pty::event::PtyMouseProtocolEncoding;
    use gate4agent::pty::{PtyForegroundObservation, PtyForegroundSource, RateLimitDetector};
    use gate4agent::CliTool;
    use gate4agent_types::{
        AdapterFamily, AgentId, ApprovalLevel,
        HostDecisionAuthority as ProviderHostDecisionAuthority,
        HostRequestDecision as ProviderHostRequestDecision, OperatorGateInput,
        OperatorGateKind, OperatorGateOptionSemantics, OperatorGateState, OperatorGateSubject,
        ProviderEvent, ProviderInteractionKind, ProviderRuntimePolicy, PtyScreenState,
        RuntimePlatform, TerminalMouseProtocolEncoding, TransportKind,
    };
    use std::ffi::{OsStr, OsString};

    fn snapshot(sequence: u64, contents: &str) -> super::PtyTerminalSnapshot {
        super::PtyTerminalSnapshot {
            pty_id: "fixture".to_owned(),
            provider_revision: "fixture-r1".to_owned(),
            generation: 1,
            sequence,
            size: gate4agent::pty::PtySize { rows: 24, cols: 80 },
            cursor: (0, 0),
            bracketed_paste: false,
            contents: contents.to_owned(),
            formatted: Vec::new(),
            scrollback_formatted: Vec::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: PtyMouseProtocolEncoding::Default,
            produced_at_unix_ms: 0,
        }
    }

    #[test]
    fn terminal_frame_preserves_scrollback_and_terminal_input_metadata() {
        let mut snapshot = snapshot(7, "visible");
        snapshot.scrollback_formatted = vec![b"older".to_vec()];
        snapshot.alternate_screen = true;
        snapshot.mouse_protocol_enabled = true;
        snapshot.mouse_protocol_encoding = PtyMouseProtocolEncoding::Sgr;
        snapshot.produced_at_unix_ms = 1_700_000_000_000;

        let frame = terminal_frame(snapshot, PtyScreenState::default());
        assert_eq!(frame.scrollback_formatted, vec![b"older".to_vec()]);
        assert!(frame.alternate_screen);
        assert!(frame.mouse_protocol_enabled);
        assert_eq!(frame.mouse_protocol_encoding, TerminalMouseProtocolEncoding::Sgr);
        // `terminal_frame` must carry the stamp through, never recompute it.
        assert_eq!(frame.produced_at_unix_ms, 1_700_000_000_000);
    }

    #[test]
    fn terminal_frame_byte_len_sums_formatted_and_every_scrollback_row() {
        let mut snapshot = snapshot(9, "visible");
        snapshot.formatted = b"\x1b[2Jvisible".to_vec();
        snapshot.scrollback_formatted =
            vec![b"row one".to_vec(), b"row two, longer".to_vec()];
        let expected = snapshot.formatted.len()
            + snapshot.scrollback_formatted[0].len()
            + snapshot.scrollback_formatted[1].len();

        let frame = terminal_frame(snapshot, PtyScreenState::default());
        assert_eq!(terminal_frame_byte_len(&frame), expected as u64);
    }

    #[test]
    fn the_sequence_gate_skips_only_when_the_sequence_did_not_advance() {
        assert!(terminal_state_capture_should_skip(Ok::<u64, ()>(5), 5));
        assert!(terminal_state_capture_should_skip(Ok::<u64, ()>(4), 5));
        assert!(!terminal_state_capture_should_skip(Ok::<u64, ()>(6), 5));
        // A failed cheap read never skips -- the real call is attempted so
        // its own error path (stale-published bookkeeping) still runs.
        assert!(!terminal_state_capture_should_skip(Err::<u64, ()>(()), 5));
    }

    #[test]
    fn structured_context_usage_maps_without_pty_inference() {
        let mapped = super::provider_event(
            AgentEvent::ContextWindowUsage {
                usage: AgentContextWindowUsage {
                    uncached_input_tokens: 70,
                    cache_read_tokens: 20,
                    cache_write_tokens: 0,
                    output_tokens: 10,
                    unattributed_tokens: 5,
                    used_tokens: 105,
                    capacity_tokens: 100,
                },
            },
            &[],
        );
        assert_eq!(
            mapped,
            Some(ProviderEvent::ContextWindowUsage {
                usage: gate4agent_types::ContextWindowUsage {
                    uncached_input_tokens: 70,
                    cache_read_tokens: 20,
                    cache_write_tokens: 0,
                    output_tokens: 10,
                    unattributed_tokens: 5,
                    used_tokens: 105,
                    capacity_tokens: 100,
                }
            })
        );
        assert_eq!(
            ProviderEvent::ContextWindowUsage {
                usage: gate4agent_types::ContextWindowUsage {
                    uncached_input_tokens: 70,
                    cache_read_tokens: 20,
                    cache_write_tokens: 0,
                    output_tokens: 10,
                    unattributed_tokens: 5,
                    used_tokens: 104,
                    capacity_tokens: 100,
                },
            }
            .validate_ingress(),
            Err(gate4agent_types::ProviderEventValidationError::ContextWindowSegmentsMismatch {
                segment_sum: 105,
                used_tokens: 104,
            })
        );
        assert!(
            super::provider_event(AgentEvent::PtyRaw { data: b"105/100".to_vec() }, &[]).is_none()
        );
    }

    #[test]
    fn host_request_reaches_the_operator_with_method_and_host_decision() {
        let denied = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::Number(1),
                method: "fs/read_text_file".to_owned(),
                params: None,
                decision: HostRequestDecision::Denied { by: HostDecisionAuthority::Policy },
            },
            &[],
        );
        assert_eq!(
            denied,
            Some(ProviderEvent::HostRequestObserved {
                method: "fs/read_text_file".to_owned(),
                params_json: String::new(),
                decision: ProviderHostRequestDecision::Denied {
                    by: ProviderHostDecisionAuthority::Policy
                },
            })
        );

        let granted = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::Number(2),
                method: "terminal/create".to_owned(),
                params: None,
                decision: HostRequestDecision::Granted { by: HostDecisionAuthority::Policy },
            },
            &[],
        );
        assert_eq!(
            granted,
            Some(ProviderEvent::HostRequestObserved {
                method: "terminal/create".to_owned(),
                params_json: String::new(),
                decision: ProviderHostRequestDecision::Granted {
                    by: ProviderHostDecisionAuthority::Policy
                },
            })
        );

        // Every authority carries through, not just the outcome -- a gate
        // block and an operator's answer must remain distinguishable on the
        // wire, and a deadline expiry must never be mistaken for one.
        let gate_denied = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::Number(3),
                method: "terminal/create".to_owned(),
                params: None,
                decision: HostRequestDecision::Denied { by: HostDecisionAuthority::Gate },
            },
            &[],
        );
        assert_eq!(
            gate_denied,
            Some(ProviderEvent::HostRequestObserved {
                method: "terminal/create".to_owned(),
                params_json: String::new(),
                decision: ProviderHostRequestDecision::Denied {
                    by: ProviderHostDecisionAuthority::Gate
                },
            })
        );

        let operator_granted = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::Number(4),
                method: "session/request_permission".to_owned(),
                params: None,
                decision: HostRequestDecision::Granted { by: HostDecisionAuthority::Operator },
            },
            &[],
        );
        assert_eq!(
            operator_granted,
            Some(ProviderEvent::HostRequestObserved {
                method: "session/request_permission".to_owned(),
                params_json: String::new(),
                decision: ProviderHostRequestDecision::Granted {
                    by: ProviderHostDecisionAuthority::Operator
                },
            })
        );

        let deadline_denied = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::Number(5),
                method: "session/request_permission".to_owned(),
                params: None,
                decision: HostRequestDecision::Denied { by: HostDecisionAuthority::DeadlinePolicy },
            },
            &[],
        );
        assert_eq!(
            deadline_denied,
            Some(ProviderEvent::HostRequestObserved {
                method: "session/request_permission".to_owned(),
                params_json: String::new(),
                decision: ProviderHostRequestDecision::Denied {
                    by: ProviderHostDecisionAuthority::DeadlinePolicy
                },
            })
        );

        // `session/request_permission` + `Deferred` is deliberately absent
        // here -- that combination becomes an `InteractionRequested`, not a
        // `HostRequestObserved`; see
        // `deferred_acp_permission_request_becomes_an_interaction_with_its_id`
        // below.
    }

    #[test]
    fn deferred_acp_permission_request_becomes_an_interaction_with_its_id() {
        let with_title = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::Number(6),
                method: "session/request_permission".to_owned(),
                params: Some(serde_json::json!({
                    "sessionId": "s1",
                    "toolCall": {"toolCallId": "t1", "kind": "edit", "title": "Edit src/main.rs"},
                    "options": [],
                })),
                decision: HostRequestDecision::Deferred,
            },
            &[],
        );
        // `kind` is the tool CLASS and `title` is the question. They are two
        // different things and the operator needs both: putting the title in
        // `tool_name` and leaving `prompt` empty -- which this asserted
        // before -- showed a correlation id and a sentence-shaped tool name,
        // and never the question itself.
        assert_eq!(
            with_title,
            Some(ProviderEvent::InteractionRequested {
                request_id: Some("number:6".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "edit".to_owned(),
                prompt: "Edit src/main.rs".to_owned(),
                agent_id: None,
            })
        );

        // No `kind` on the wire (or no params at all) -- falls back to the
        // method name rather than an empty `tool_name`, which
        // `ProviderEvent::validate_ingress` rejects. An absent `title`
        // leaves `prompt` empty rather than inventing a question.
        let without_title = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::String("agent-7".to_owned()),
                method: "session/request_permission".to_owned(),
                params: None,
                decision: HostRequestDecision::Deferred,
            },
            &[],
        );
        assert_eq!(
            without_title,
            Some(ProviderEvent::InteractionRequested {
                request_id: Some("string:agent-7".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "session/request_permission".to_owned(),
                prompt: String::new(),
                agent_id: None,
            })
        );

        // The SAME id's eventual outcome (not itself `Deferred` anymore)
        // still reaches the operator as the ordinary audit trail, exactly
        // as every other host request does.
        let resolved = super::provider_event(
            AgentEvent::RpcIncomingRequest {
                id: gate4agent::rpc::message::RpcId::Number(6),
                method: "session/request_permission".to_owned(),
                params: None,
                decision: HostRequestDecision::Denied { by: HostDecisionAuthority::Operator },
            },
            &[],
        );
        assert_eq!(
            resolved,
            Some(ProviderEvent::HostRequestObserved {
                method: "session/request_permission".to_owned(),
                params_json: String::new(),
                decision: ProviderHostRequestDecision::Denied {
                    by: ProviderHostDecisionAuthority::Operator
                },
            })
        );
    }

    #[test]
    fn unrecognized_notification_reaches_the_operator_as_a_raw_event_instead_of_vanishing() {
        let mapped = super::provider_event(
            AgentEvent::RpcNotification {
                method: "session/update".to_owned(),
                params: Default::default(),
            },
            &[],
        );
        assert_eq!(
            mapped,
            Some(ProviderEvent::UnrecognizedNotification {
                method: "session/update".to_owned(),
                payload_json: "null".to_owned(),
            })
        );
    }

    #[test]
    fn pty_terminal_capability_defaults_never_override_a_caller_supplied_term() {
        let caller_supplied = vec![EnvMutation {
            key: OsString::from("TERM"),
            value: Some(OsString::from("dumb")),
        }];
        let filled = with_pty_terminal_capability_defaults(caller_supplied);

        let term_values: Vec<_> = filled
            .iter()
            .filter(|mutation| mutation.key.as_os_str() == OsStr::new("TERM"))
            .collect();
        assert_eq!(term_values.len(), 1, "TERM must not be duplicated");
        assert_eq!(term_values[0].value.as_deref(), Some(OsStr::new("dumb")));

        let colorterm = filled
            .iter()
            .find(|mutation| mutation.key.as_os_str() == OsStr::new("COLORTERM"))
            .expect("COLORTERM default is filled in when the caller left it unset");
        assert_eq!(colorterm.value.as_deref(), Some(OsStr::new("truecolor")));
    }

    #[test]
    fn native_instance_launch_arguments_fail_closed_before_shell_spawn() {
        let claude = AgentId::new("claude").unwrap();
        assert_eq!(
            validate_instance_launch_arguments(
                &claude,
                TransportKind::Pipe,
                &[OsString::from("--bundle-mode")],
            )
            .unwrap_err(),
            "native instance launch arguments require PTY transport"
        );
        assert_eq!(
            validate_instance_launch_arguments(
                &claude,
                TransportKind::Pty,
                &[OsString::from("--resume=session-secret")],
            )
            .unwrap_err(),
            "native instance launch arguments conflict with Claude session, resume, or prompt authority"
        );
        assert!(validate_instance_launch_arguments(
            &claude,
            TransportKind::Pty,
            &[
                OsString::from("--permission-mode"),
                OsString::from("default"),
            ],
        )
        .is_ok());
    }

    /// Classifies `contents` and returns just the `kind` -- the same check
    /// every case below cares about, without repeating `.map(|gate|
    /// gate.kind)` at every call site.
    fn kind_of(contents: &str) -> Option<OperatorGateKind> {
        startup_operator_gate(contents).map(|gate| gate.kind)
    }

    #[test]
    fn startup_operator_gates_are_classified_without_returning_terminal_text() {
        assert_eq!(kind_of(" Trust this\nfolder? "), Some(OperatorGateKind::WorkspaceTrust));
        assert_eq!(
            kind_of("No auth type is selected"),
            Some(OperatorGateKind::Authentication)
        );
        assert_eq!(
            kind_of("Sign in with OpenAI to use Codex"),
            Some(OperatorGateKind::Authentication)
        );
        // Claude's login-method chooser and its OAuth code-paste wait
        // screen -- both previously misread as `Ready` because neither
        // contains "openai"/"chatgpt"/"codex", so the branch above never
        // caught them. Verbatim shape from `terminal-read` on a clean
        // macOS arm64 stand with no vendor login.
        assert_eq!(
            kind_of(
                "Claude Code can be used with your Claude subscription or billed based on \
                 API usage through your Console account.\n\
                 Select login method:\n\
                 \u{276f} 1. Claude ...\n\
                 2. API usage billing\n\
                 3. 3rd-party platform \u{b7} Amazon Bedrock ..."
            ),
            Some(OperatorGateKind::Authentication)
        );
        assert_eq!(
            kind_of(
                "Browser didn't open? Use the url below to sign in (c to copy)\n\
                 https://claude.com/oauth/authorize?client_id=abc&redirect_uri=https%3A%2F%2Fconsole.anthropic.com&code_challenge=xyz\n\
                 Paste code here"
            ),
            Some(OperatorGateKind::Authentication)
        );
        // Ordinary narration reusing "sign in" and "paste" separately, but
        // never as the screen's own "paste code" field label, must not
        // false-positive -- the same companion-word discipline the
        // OpenAI/Codex sign-in branch already applies above.
        assert_eq!(
            kind_of(
                "Once you sign in, copy the generated token and paste it into your .env file; \
                 no code entry happens on this screen."
            ),
            None
        );
        assert_eq!(
            kind_of("Kimi Code Update Available\nInstall update now (0.32.0)\nEnter confirm"),
            Some(OperatorGateKind::VendorUpdate)
        );
        assert_eq!(
            kind_of(
                "Welcome to Claude Code\nChoose the text style that looks best with your terminal"
            ),
            Some(OperatorGateKind::TerminalAppearance)
        );
        assert_eq!(
            kind_of(
                "Welcome to Claude Code for VS Code\nClaude has context of open files and selected lines"
            ),
            Some(OperatorGateKind::Onboarding)
        );
        assert_eq!(
            kind_of("Welcome to Claude Code\n❯ Press Enter to continue"),
            Some(OperatorGateKind::Onboarding)
        );
        assert_eq!(kind_of("Welcome to Claude Code\n❯ ready\nEnter to send"), None);
        assert_eq!(
            kind_of(
                "Quick safety check: Is this a project you trust?\nYes, I trust this folder\nNo, continue without these permissions"
            ),
            Some(OperatorGateKind::WorkspaceTrust)
        );
        assert_eq!(kind_of("ready for a prompt"), None);
        // Regression test for the incident this module fixes: a provider
        // CLI's wrapper self-updated via npm before the real agent ever
        // launched, and every consumer saw `status: running` on a live PTY
        // that was actually showing this text. Verbatim transcript.
        assert_eq!(
            kind_of(
                "Updating Codex via `npm install -g @openai/codex`...\n\
                 npm warn cleanup Failed to remove some directories\n\
                 Update ran successfully! Please restart Codex."
            ),
            Some(OperatorGateKind::VendorUpdate)
        );
        // Regression test for the live-stand incident this branch fixes:
        // Codex's startup hook-trust prompt was classified `Ready` because
        // no marker recognized it, so a prompt injected into the PTY landed
        // in this blocking select-list instead of the agent. Verbatim
        // transcript from `terminal-read`, 40x120 -- note this capture wraps
        // the `› 1. Review hooks` marker onto the end of the preceding
        // sentence, so it is deliberately NOT the clean list shape the
        // dedicated option-parsing tests below assert on; this test only
        // pins the `kind`, same as it always has.
        assert_eq!(
            kind_of(
                "  Hooks need review\n\
                   6 hooks are new or changed.\n\
                   Hooks can run outside the sandbox after you trust them.\u{203a} 1. Review hooks\n\
                   2. Trust all and continue\n\
                   3. Continue without trusting (hooks won't run)  Press enter to confirm or esc to go back"
            ),
            Some(OperatorGateKind::HookTrust)
        );
        // Same screen, different hook count -- the match must not depend on
        // the number.
        assert_eq!(
            kind_of(
                "  Hooks need review\n\
                   42 hooks are new or changed.\n\
                   Hooks can run outside the sandbox after you trust them.\u{203a} 1. Review hooks\n\
                   2. Trust all and continue\n\
                   3. Continue without trusting (hooks won't run)  Press enter to confirm or esc to go back"
            ),
            Some(OperatorGateKind::HookTrust)
        );
        // Ordinary agent narration that happens to mention hooks and trust
        // must NOT be classified as this gate -- it lacks the screen's own
        // "continue without trusting" refusal phrasing.
        assert_eq!(
            kind_of(
                "I reviewed the pre-commit hooks in this repo and they look safe to trust; \
                 I'll leave the hooks config as-is and continue with the refactor."
            ),
            None
        );
    }

    /// Codex's hook-trust prompt rendered as a clean numbered list (the
    /// shape actually described by the task this parser was written for,
    /// as opposed to the line-wrapped capture pinned by `kind` alone
    /// above): `parse_operator_gate_options` reads all three choices, marks
    /// the `›`-prefixed one selected, and infers accept/inspect/decline
    /// from each option's own verb -- never from its number or position.
    #[test]
    fn startup_operator_gate_parses_a_numbered_hook_trust_option_list() {
        let contents = "Hooks need review\n\
             \u{203a} 1. Review hooks\n  \
             2. Trust all and continue\n  \
             3. Continue without trusting (hooks won't run)\n\
             Press enter to confirm or esc to go back";
        let gate = startup_operator_gate(contents).expect("hook trust must classify");
        assert_eq!(gate.kind, OperatorGateKind::HookTrust);
        assert_eq!(gate.input, OperatorGateInput::NumberedList);
        assert_eq!(
            gate.options,
            vec![
                gate4agent_types::OperatorGateOption {
                    text: "Review hooks".to_owned(),
                    semantics: OperatorGateOptionSemantics::Inspect,
                    selected: true,
                },
                gate4agent_types::OperatorGateOption {
                    text: "Trust all and continue".to_owned(),
                    semantics: OperatorGateOptionSemantics::Accept,
                    selected: false,
                },
                gate4agent_types::OperatorGateOption {
                    text: "Continue without trusting (hooks won't run)".to_owned(),
                    semantics: OperatorGateOptionSemantics::Decline,
                    selected: false,
                },
            ],
        );
    }

    /// Kimi's workspace-trust prompt rendered as an arrow-navigated list
    /// with a description line under each title: `parse_operator_gate_options`
    /// reads the two TITLES ("Trust this folder", "Don't trust") as options,
    /// marks the `❯`-prefixed one selected, and does NOT mistake either
    /// description line (both full sentences, ending in `.`) for a third
    /// and fourth option.
    #[test]
    fn startup_operator_gate_parses_an_arrow_workspace_trust_option_list() {
        let contents = "Do you trust the files in this folder?\n  \
             Trust this folder\n  \
             Enable project MCP servers. Remembered for this folder.\n\u{276f} \
             Don't trust\n  \
             Exit Kimi Code. Asked again next launch.\n  \
             \u{2191}\u{2193} navigate \u{b7} Enter select \u{b7} Esc exit";
        let gate = startup_operator_gate(contents).expect("workspace trust must classify");
        assert_eq!(gate.kind, OperatorGateKind::WorkspaceTrust);
        assert_eq!(gate.input, OperatorGateInput::ArrowList);
        assert_eq!(
            gate.options,
            vec![
                gate4agent_types::OperatorGateOption {
                    text: "Trust this folder".to_owned(),
                    semantics: OperatorGateOptionSemantics::Accept,
                    selected: false,
                },
                gate4agent_types::OperatorGateOption {
                    text: "Don't trust".to_owned(),
                    semantics: OperatorGateOptionSemantics::Decline,
                    selected: true,
                },
            ],
        );
    }

    /// A matched gate whose screen carries no recognized option list (every
    /// other `kind` this module classifies today) must NOT invent one --
    /// `input` stays `Unknown` and `options` stays empty, never a guess.
    #[test]
    fn startup_operator_gate_without_a_recognized_option_list_reports_unknown_input() {
        let gate = startup_operator_gate("No auth type is selected")
            .expect("authentication must classify");
        assert_eq!(gate.kind, OperatorGateKind::Authentication);
        assert_eq!(gate.input, OperatorGateInput::Unknown);
        assert!(gate.options.is_empty());
    }

    /// Claude's login-method chooser renders its selected row with a `❯`
    /// cursor glyph where Codex draws `›`, and both sit AHEAD of the number rather
    /// than ahead of the text. `parse_numbered_gate_option_line` knew only
    /// Codex's glyph, so the cursor row failed to parse as an option at
    /// all: the list came back one item short AND with nothing marked
    /// selected -- observed live on this screen and on Codex's own login
    /// chooser. Both glyphs are read now, so all three options are present
    /// and the cursor row carries `selected`.
    #[test]
    fn startup_operator_gate_parses_claudes_login_method_chooser_including_its_cursor_row() {
        let contents = "Select login method:\n\
             \u{276f} 1. Claude ...\n\
             2. API usage billing\n\
             3. 3rd-party platform \u{b7} Amazon Bedrock ...";
        let gate = startup_operator_gate(contents).expect("authentication must classify");
        assert_eq!(gate.kind, OperatorGateKind::Authentication);
        assert_eq!(gate.subject, OperatorGateSubject::Account);
        assert_eq!(gate.input, OperatorGateInput::NumberedList);
        assert_eq!(
            gate.options,
            vec![
                gate4agent_types::OperatorGateOption {
                    text: "Claude ...".to_owned(),
                    semantics: OperatorGateOptionSemantics::Unknown,
                    selected: true,
                },
                gate4agent_types::OperatorGateOption {
                    text: "API usage billing".to_owned(),
                    semantics: OperatorGateOptionSemantics::Unknown,
                    selected: false,
                },
                gate4agent_types::OperatorGateOption {
                    text: "3rd-party platform \u{b7} Amazon Bedrock ...".to_owned(),
                    semantics: OperatorGateOptionSemantics::Unknown,
                    selected: false,
                },
            ],
        );
    }

    /// Claude's OAuth wait screen has no option list at all -- a URL to
    /// open and one free-text slot to paste the resulting code back into --
    /// so `input` is `TextEntry` with no options, never routed through the
    /// numbered/arrow option-list parser.
    #[test]
    fn startup_operator_gate_oauth_wait_screen_reports_text_entry_input() {
        let contents = "Browser didn't open? Use the url below to sign in (c to copy)\n\
             https://claude.com/oauth/authorize?client_id=abc&redirect_uri=https%3A%2F%2Fconsole.anthropic.com&code_challenge=xyz\n\
             Paste code here";
        let gate = startup_operator_gate(contents).expect("oauth wait screen must classify");
        assert_eq!(gate.kind, OperatorGateKind::Authentication);
        assert_eq!(gate.subject, OperatorGateSubject::Account);
        assert_eq!(gate.input, OperatorGateInput::TextEntry);
        assert!(gate.options.is_empty());
    }

    /// `parse_operator_gate_options` is the parser both dedicated tests
    /// above exercise indirectly through `startup_operator_gate`; this pins
    /// it directly against the same two shapes so a regression in the
    /// standalone parser is caught even if some future `kind` branch stops
    /// calling it through `operator_gate`.
    #[test]
    fn parse_operator_gate_options_recognizes_numbered_and_arrow_shapes_and_nothing_else() {
        assert_eq!(
            parse_operator_gate_options("Press enter to confirm or esc to go back"),
            (OperatorGateInput::Unknown, Vec::new()),
        );
        let (input, options) = parse_operator_gate_options(
            "\u{203a} 1. Review hooks\n  2. Trust all and continue",
        );
        assert_eq!(input, OperatorGateInput::NumberedList);
        assert_eq!(options.len(), 2);
        let (input, options) = parse_operator_gate_options(
            "  Trust this folder\n  Enable project MCP servers. Remembered for this folder.\n\u{276f} Don't trust",
        );
        assert_eq!(input, OperatorGateInput::ArrowList);
        assert_eq!(options.len(), 2);
    }

    #[test]
    fn classify_operator_gate_option_semantics_reads_the_verb_not_the_position() {
        assert_eq!(
            classify_operator_gate_option_semantics("Review hooks"),
            OperatorGateOptionSemantics::Inspect,
        );
        assert_eq!(
            classify_operator_gate_option_semantics("Trust all and continue"),
            OperatorGateOptionSemantics::Accept,
        );
        assert_eq!(
            classify_operator_gate_option_semantics("Continue without trusting (hooks won't run)"),
            OperatorGateOptionSemantics::Decline,
        );
        assert_eq!(
            classify_operator_gate_option_semantics("Don't trust"),
            OperatorGateOptionSemantics::Decline,
        );
        assert_eq!(
            classify_operator_gate_option_semantics("Exit Kimi Code"),
            OperatorGateOptionSemantics::Exit,
        );
        assert_eq!(
            classify_operator_gate_option_semantics("Something unrecognized"),
            OperatorGateOptionSemantics::Unknown,
        );
    }

    #[test]
    fn screen_failure_recognizes_crash_and_missing_command_banners() {
        assert_eq!(
            screen_failure("thread 'main' panicked at 'index out of bounds', src/main.rs:12:5"),
            Some("crash")
        );
        assert_eq!(
            screen_failure("Traceback (most recent call last):\n  File \"a.py\", line 1"),
            Some("crash")
        );
        assert_eq!(
            screen_failure("bash: fooagent: command not found"),
            Some("missing command")
        );
        assert_eq!(
            screen_failure("C:\\workspace> fooagent\n'fooagent' is not recognized as an internal or external command"),
            Some("missing command")
        );
        assert_eq!(
            screen_failure(
                "workspace/project $ fooagent: no such file or directory\nworkspace/project $"
            ),
            Some("missing command")
        );
        // Bare ENOENT prose with no trailing shell-prompt line is exactly
        // the generic case this bucket must NOT fire on alone.
        assert_eq!(
            screen_failure("The build log mentions no such file or directory near line 40."),
            None
        );
        // `"fatal error:"` was deliberately dropped: a build step inside a
        // wrapper script can print this while the real CLI still comes up
        // fine, so it is not a crash SHAPE this function trusts at all.
        assert_eq!(
            screen_failure("fooagent-installer: fatal error: missing header <stdio.h>"),
            None
        );
    }

    #[test]
    fn screen_failure_recognizes_grok_missing_api_key_banner() {
        // Verbatim transcript: Grok launched with no credential configured
        // prints exactly this one line and never renders anything else --
        // no gate to answer, no crash trace, just a dead PTY a blind
        // `Ready` write would silently swallow.
        assert_eq!(
            screen_failure(
                "\u{274c} Error: API key required. Set GROK_API_KEY environment variable, \
                 use --api-key flag, or set \"apiKey\" field in ~/.grok/user-settings.json"
            ),
            Some("missing api key")
        );
        // Generic prose about needing an API key, with no vendor-specific
        // env var name, must not false-positive -- an agent explaining a
        // DIFFERENT provider's setup routinely says exactly this.
        assert_eq!(
            screen_failure(
                "You'll need an API key for this provider before it works; check the docs \
                 for how to configure one."
            ),
            None
        );
    }

    #[test]
    fn screen_failure_no_longer_has_an_authentication_expired_bucket() {
        // Two common words matched anywhere on one 80x24 screen is not a
        // signal -- an agent's own prose explaining an auth bug ("the
        // token has expired") used to flip a healthy session to `Failing`.
        // The bucket is gone entirely, not narrowed.
        assert_eq!(
            screen_failure("Your session token has expired. Please re-authenticate."),
            None
        );
        assert_eq!(
            screen_failure("The stored credential was revoked by the workspace admin."),
            None
        );
    }

    #[test]
    fn screen_failure_does_not_false_positive_on_an_agent_narrating_an_error() {
        // The false-positive direction matters more than coverage here: an
        // agent CLI renders arbitrary text back at a human, including logs
        // and error messages it was asked to explain. None of the phrases
        // below are in the exact failure SHAPE this module matches on.
        let narration = "I looked at the traceback you pasted -- it's a Python \
             exception, a plain ValueError from a retry loop, not a real crash. \
             Our harness logs the word fatal in its own banner for visibility, \
             but nothing actually panicked. The command definitely exists; it \
             just needed different flags, and your token is still valid.";
        assert_eq!(screen_failure(narration), None);
    }

    #[test]
    fn screen_failure_for_generation_ignores_every_marker_once_the_session_was_ever_ready() {
        // Each of these is a screen from a HEALTHY, already-`Ready` session
        // whose own routine work happens to render a crash-shaped string.
        // This is the regression this whole gate exists to close: before
        // it, every one of these flipped a working session to `Failing`.
        let shell_command_not_found = "$ frobnicate --help\nbash: frobnicate: command not found\n$";
        assert_eq!(
            screen_failure_for_generation(shell_command_not_found, true),
            None
        );

        let python_traceback_from_a_ran_script = "$ python broken.py\n\
             Traceback (most recent call last):\n  File \"broken.py\", line 3, in <module>\n\
             ValueError: bad input\n$";
        assert_eq!(
            screen_failure_for_generation(python_traceback_from_a_ran_script, true),
            None
        );

        let cargo_test_panic = "running 1 test\n\
             error[E0308]: mismatched types\n\
             thread 'tests::it_fails' panicked at 'assertion failed', src/lib.rs:9:5\n\
             test result: FAILED. 0 passed; 1 failed";
        assert_eq!(screen_failure_for_generation(cargo_test_panic, true), None);

        let agent_narrating_an_expired_token = "The API token has expired; \
             I revoked the old credential and issued a new one for you.";
        assert_eq!(
            screen_failure_for_generation(agent_narrating_an_expired_token, true),
            None
        );

        // Pin the narrowing from the other side too: the exact same
        // screens are still real evidence of a broken LAUNCH inside the
        // startup window, before this generation has ever rendered its
        // own UI.
        assert_eq!(
            screen_failure_for_generation(shell_command_not_found, false),
            Some("missing command")
        );
        assert_eq!(
            screen_failure_for_generation(python_traceback_from_a_ran_script, false),
            Some("crash")
        );
        assert_eq!(screen_failure_for_generation(cargo_test_panic, false), Some("crash"));
    }

    #[test]
    fn classify_pty_screen_state_lets_a_foreign_process_win_over_a_matching_gate_text() {
        // The process signal outranks the text signal outright: it does not
        // need to recognize an updater's specific wording to know the
        // screen is not the agent's, so it wins even when the text ALSO
        // happens to match a known gate pattern.
        let foreign = ForegroundVerdict::Foreign {
            process: "npm".to_owned(),
        };
        assert_eq!(
            classify_pty_screen_state(Some(&foreign), None, None),
            PtyScreenState::NotAgent {
                observed_process: "npm".to_owned()
            }
        );
        let vendor_update_gate = OperatorGateState::new(OperatorGateKind::VendorUpdate);
        assert_eq!(
            classify_pty_screen_state(Some(&foreign), Some(&vendor_update_gate), None),
            PtyScreenState::NotAgent {
                observed_process: "npm".to_owned()
            }
        );
    }

    #[test]
    fn classify_pty_screen_state_reports_a_gate_only_when_foreground_matches() {
        let authentication_gate = OperatorGateState::new(OperatorGateKind::Authentication);
        assert_eq!(
            classify_pty_screen_state(Some(&ForegroundVerdict::Agent), Some(&authentication_gate), None),
            PtyScreenState::OperatorGate {
                gate: authentication_gate.clone()
            }
        );
    }

    #[test]
    fn classify_pty_screen_state_reports_failing_for_a_matched_foreground() {
        assert_eq!(
            classify_pty_screen_state(Some(&ForegroundVerdict::Agent), None, Some("crash")),
            PtyScreenState::Failing {
                reason: "crash".to_owned()
            }
        );
    }

    #[test]
    fn classify_pty_screen_state_prefers_failing_over_a_co_occurring_gate() {
        // Pins the precedence: a crashed screen can still carry a leftover
        // gate prompt above the crash dump, and `Failing` is the more
        // urgent of the two truths.
        let authentication_gate = OperatorGateState::new(OperatorGateKind::Authentication);
        assert_eq!(
            classify_pty_screen_state(
                Some(&ForegroundVerdict::Agent),
                Some(&authentication_gate),
                Some("crash")
            ),
            PtyScreenState::Failing {
                reason: "crash".to_owned()
            }
        );
    }

    #[test]
    fn classify_pty_screen_state_reaches_ready_only_with_matched_foreground_and_clean_text() {
        assert_eq!(
            classify_pty_screen_state(Some(&ForegroundVerdict::Agent), None, None),
            PtyScreenState::Ready
        );
    }

    #[test]
    fn classify_pty_screen_state_never_reaches_ready_without_a_foreground_observation() {
        // The one case that must never regress: a clean text read alone is
        // never proof of `Ready`.
        assert_eq!(classify_pty_screen_state(None, None, None), PtyScreenState::Unknown);
    }

    #[test]
    fn resolve_foreground_verdict_tolerates_a_spawning_wrapper_like_readiness_does() {
        // The fixture spec's `expected_processes` does not name "node", so
        // this only passes if the wrapper-tolerance branch (mirroring
        // `ReadinessTracker::observe_foreground`'s own `node`/`python`/
        // `python3` tolerance) is actually what resolves it -- proving the
        // existing wrapper tolerance is not regressed. Fed into
        // `classify_pty_screen_state` with clean text, this is what reaches
        // `Ready` rather than `NotAgent` for a legitimate spawning wrapper.
        let spec = gate4agent_testkit::interactive_agent_spec();
        let observation = PtyForegroundObservation {
            root_pid: 1,
            observed_pid: 2,
            observed_process: "node".to_owned(),
            readiness: ForegroundObservation {
                process_name: Some("node".to_owned()),
                has_child_processes: true,
                is_shell: false,
            },
            source: PtyForegroundSource::ProcessTree,
        };
        let verdict = resolve_foreground_verdict(&spec, &observation, RuntimePlatform::current());
        assert_eq!(verdict, ForegroundVerdict::Agent);
        assert_eq!(
            classify_pty_screen_state(Some(&verdict), None, None),
            PtyScreenState::Ready
        );
    }

    #[test]
    fn foreground_probe_disarms_only_once_ready_and_rearms_on_a_fresh_gate() {
        assert_eq!(
            foreground_probe_schedule(&PtyScreenState::Ready),
            ForegroundProbeSchedule::Disarmed
        );
        assert_eq!(
            foreground_probe_schedule(&PtyScreenState::Unknown),
            ForegroundProbeSchedule::Armed
        );
        assert_eq!(
            foreground_probe_schedule(&PtyScreenState::OperatorGate {
                gate: OperatorGateState::new(OperatorGateKind::VendorUpdate)
            }),
            ForegroundProbeSchedule::Armed
        );
    }

    #[test]
    fn text_only_gate_transition_rearms_the_probe_only_when_leaving_ready() {
        let gate = PtyScreenState::OperatorGate {
            gate: OperatorGateState::new(OperatorGateKind::VendorUpdate),
        };
        assert!(foreground_probe_rearms_immediately(
            &PtyScreenState::Ready,
            &gate
        ));
        // Was never `Ready` in the first place -- nothing to rearm early
        // for, the session's normal cadence already has it covered.
        assert!(!foreground_probe_rearms_immediately(
            &PtyScreenState::Unknown,
            &gate
        ));
    }

    #[test]
    fn readiness_diagnostics_detects_an_ansi_split_gate_without_exposing_text() {
        let mut diagnostics = ReadinessDiagnostics::default();
        diagnostics.observe_output(b"\x1b[31mNo auth ");
        diagnostics.observe_output(b"\x1b[0mtype is selected");
        assert_eq!(
            diagnostics.operator_gate.as_ref().map(|gate| gate.kind),
            Some(OperatorGateKind::Authentication)
        );
        assert!(!diagnostics.summary().contains("No auth"));
    }

    #[test]
    fn semantic_utf8_decoder_preserves_codepoints_split_across_pty_reads() {
        let mut decoder = Utf8ChunkDecoder::default();
        let bytes = "ready Привет".as_bytes();
        let split = bytes
            .windows(2)
            .position(|window| window[0] >= 0x80 && window[1] >= 0x80)
            .expect("Cyrillic text contains adjacent UTF-8 bytes")
            + 1;
        let first = decoder.push(&bytes[..split]);
        let second = decoder.push(&bytes[split..]);
        assert_eq!(format!("{first}{second}"), "ready Привет");
        assert!(!first.contains('\u{fffd}'));
        assert!(!second.contains('\u{fffd}'));
    }

    #[test]
    fn semantic_utf8_decoder_replaces_invalid_bytes_without_stalling() {
        let mut decoder = Utf8ChunkDecoder::default();
        assert_eq!(decoder.push(b"ok\xfftail"), "ok\u{fffd}tail");
        assert_eq!(decoder.push(" Привет".as_bytes()), " Привет");
    }

    #[test]
    fn fresh_claude_pty_preassigns_the_exact_vendor_session_id_argv() {
        let claude = builtin_adapter_registry()
            .binding(AdapterFamily::PtySemantic, "claude-code")
            .expect("Claude PTY binding");
        let mut args = Vec::new();
        let identity = prepare_fresh_pty_provider_session(Some(claude), false, true, &mut args)
            .expect("fresh Claude provider identity");
        let parsed = uuid::Uuid::parse_str(&identity.id).expect("valid Claude UUID");
        assert_eq!(parsed.get_version_num(), 4);
        assert_eq!(identity.key, gate4agent_types::ProviderSessionKey::SessionId);
        assert!(identity.transcript_path.is_none());
        assert_eq!(args, [OsString::from("--session-id"), OsString::from(identity.id)]);
    }

    #[test]
    fn fresh_codex_and_resumed_claude_do_not_preassign_a_second_identity() {
        let adapters = builtin_adapter_registry();
        let codex = adapters
            .binding(AdapterFamily::PtySemantic, "codex")
            .expect("Codex PTY binding");
        let claude = adapters
            .binding(AdapterFamily::PtySemantic, "claude-code")
            .expect("Claude PTY binding");
        let mut codex_args = Vec::new();
        assert!(prepare_fresh_pty_provider_session(Some(codex), false, true, &mut codex_args)
            .is_none());
        assert!(codex_args.is_empty());

        let mut resume_args = vec![OsString::from("--resume"), OsString::from("vendor-id")];
        assert!(prepare_fresh_pty_provider_session(Some(claude), true, true, &mut resume_args)
            .is_none());
        assert_eq!(
            resume_args,
            [OsString::from("--resume"), OsString::from("vendor-id")]
        );
    }

    #[test]
    fn raw_pty_policy_omits_all_identity_and_semantic_startup_paths() {
        let adapters = builtin_adapter_registry();
        let claude = adapters
            .binding(AdapterFamily::PtySemantic, "claude-code")
            .expect("Claude PTY binding");
        let codex = adapters
            .binding(AdapterFamily::PtySemantic, "codex")
            .expect("Codex PTY binding");
        let kimi = adapters
            .binding(AdapterFamily::PtySemantic, "kimi")
            .expect("Kimi PTY binding");
        let policy = ProviderRuntimePolicy::raw_pty();
        let mut args = Vec::new();

        assert!(prepare_fresh_pty_provider_session(Some(claude), false, false, &mut args)
            .is_none());
        assert!(args.is_empty());
        assert!(!should_probe_pty_identity(policy, Some(codex), false, "codex"));
        assert!(!should_probe_pty_identity(policy, Some(kimi), false, "kimi"));
        assert!(!should_attach_pty_provider_stream(policy));
    }

    #[test]
    fn identity_probe_requires_structured_prompt_for_codex_and_kimi() {
        let adapters = builtin_adapter_registry();
        let codex = adapters
            .binding(AdapterFamily::PtySemantic, "codex")
            .expect("Codex PTY binding");
        let kimi = adapters
            .binding(AdapterFamily::PtySemantic, "kimi")
            .expect("Kimi PTY binding");
        let without_structured_prompt =
            ProviderRuntimePolicy::new(true, true, false, true, false, false)
                .expect("identity observation policy without structured prompt");

        assert!(!should_probe_pty_identity(
            without_structured_prompt,
            Some(codex),
            false,
            "codex",
        ));
        assert!(!should_probe_pty_identity(
            without_structured_prompt,
            Some(kimi),
            false,
            "kimi",
        ));

        let with_structured_prompt =
            ProviderRuntimePolicy::new(true, true, true, true, false, false)
                .expect("identity probe policy");
        assert!(should_probe_pty_identity(
            with_structured_prompt,
            Some(codex),
            false,
            "codex",
        ));
        assert!(should_probe_pty_identity(
            with_structured_prompt,
            Some(kimi),
            false,
            "kimi",
        ));
    }

    #[test]
    fn runtime_policy_admits_raw_native_resume_but_denies_unverified_prompt_injection() {
        let raw = ProviderRuntimePolicy::raw_pty();
        assert!(validate_spawn_runtime_policy(raw, TransportKind::Pty, false, false).is_ok());
        assert!(validate_spawn_runtime_policy(raw, TransportKind::Pty, true, false)
            .unwrap_err()
            .contains("SemanticReadiness"));
        assert!(validate_spawn_runtime_policy(raw, TransportKind::Pty, false, true).is_ok());
        assert!(validate_spawn_runtime_policy(raw, TransportKind::Pty, true, true)
            .unwrap_err()
            .contains("SemanticReadiness"));

        let resume_without_prompt =
            ProviderRuntimePolicy::new(true, false, false, true, true, false)
                .expect("identity and resume policy");
        assert!(validate_spawn_runtime_policy(
            resume_without_prompt,
            TransportKind::Pty,
            false,
            true,
        )
        .is_ok());
        assert!(validate_spawn_runtime_policy(
            resume_without_prompt,
            TransportKind::Pty,
            true,
            true,
        )
        .unwrap_err()
        .contains("SemanticReadiness"));
    }

    #[test]
    fn acp_transport_bypasses_the_pty_semantic_policy_gate_pty_and_pipe_still_enforce_it() {
        // A provider that speaks ACP and nothing else (no raw PTY, no
        // verified terminal semantics) admits none of these capabilities --
        // exactly grok's real policy. `TransportKind::Acp` must not care:
        // ACP has no terminal to infer state from, so none of this policy
        // applies to it.
        let no_pty_capabilities_at_all = ProviderRuntimePolicy::new(
            false, false, false, false, false, false,
        )
        .expect("an all-false policy is internally valid");
        assert!(validate_spawn_runtime_policy(
            no_pty_capabilities_at_all,
            TransportKind::Acp,
            false,
            false,
        )
        .is_ok());
        // The same all-false policy is still correctly refused for Pty and
        // Pipe -- this fix narrows the gate to skip Acp specifically, it
        // does not weaken it for the transports that do need it.
        assert!(validate_spawn_runtime_policy(
            no_pty_capabilities_at_all,
            TransportKind::Pty,
            false,
            false,
        )
        .unwrap_err()
        .contains("RawPtyLifecycle"));
        assert!(validate_spawn_runtime_policy(
            no_pty_capabilities_at_all,
            TransportKind::Pipe,
            false,
            false,
        )
        .unwrap_err()
        .contains("RawPtyLifecycle"));
    }

    #[test]
    fn prompt_render_probe_ignores_terminal_wrapping_and_uses_the_tail() {
        let prompt = "prefix with spaces\nand punctuation: final-render-token-1234567890";
        let probe = prompt_render_probe(prompt);
        assert!("screen prefix with spaces and punctuation final render token 1234567890"
            .chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
            .contains(&probe));
        assert!(probe.chars().count() <= 32);
    }

    #[test]
    fn prompt_render_requires_new_sequence_and_visible_tail_evidence() {
        let probe = prompt_render_probe("final render token");
        let baseline = snapshot(7, "old composer");
        assert!(!prompt_rendered(
            &snapshot(7, "final\nrender\ntoken"),
            &baseline,
            &probe
        ));
        assert!(!prompt_rendered(
            &snapshot(8, "unrelated redraw"),
            &baseline,
            &probe
        ));
        assert!(prompt_rendered(
            &snapshot(8, "composer\nfinal\nrender\ntoken"),
            &baseline,
            &probe
        ));
        assert!(prompt_rendered(
            &snapshot(8, "composer [Pasted Content 4096 chars]"),
            &baseline,
            &probe
        ));
        assert!(prompt_rendered(
            &snapshot(8, "composer [Pasted text #1 +6 lines]"),
            &baseline,
            &probe
        ));
    }

    #[test]
    fn an_existing_paste_placeholder_cannot_pass_on_an_unrelated_redraw() {
        let probe = prompt_render_probe("a new long prompt");
        assert!(!prompt_rendered(
            &snapshot(8, "composer [Pasted Content 4096 chars]\nunrelated redraw"),
            &snapshot(7, "composer [Pasted Content 4096 chars]"),
            &probe
        ));
        assert!(!prompt_rendered(
            &snapshot(8, "composer [Pasted text #1 +6 lines]\nunrelated redraw"),
            &snapshot(7, "composer [Pasted text #1 +6 lines]"),
            &probe
        ));
    }

    #[test]
    fn punctuation_only_prompt_cannot_pass_on_an_unrelated_redraw() {
        let probe = prompt_render_probe("!?---");
        assert!(probe.is_empty());
        assert!(!prompt_rendered(
            &snapshot(2, "unrelated redraw"),
            &snapshot(1, "old composer"),
            &probe
        ));
    }

    #[test]
    fn prompt_probe_matches_the_sanitized_terminal_payload() {
        let prompt = "payload\u{1b}tail";
        let sanitized = gate4agent_types::sanitize_prompt_text(prompt);
        let probe = prompt_render_probe(&sanitized);
        assert!(prompt_rendered(
            &snapshot(2, "composer payload<ESC>tail"),
            &snapshot(1, "old composer"),
            &probe
        ));
    }

    #[test]
    fn lag_and_data_gap_reserve_missed_provider_source_positions() {
        let mut next = 7;
        assert_eq!(reserve_provider_gap_sequence(&mut next, 3), Some(9));
        assert_eq!(next, 10);
        assert_eq!(reserve_provider_gap_sequence(&mut next, 0), None);
        assert_eq!(next, 10);

        next = u64::MAX;
        assert_eq!(reserve_provider_gap_sequence(&mut next, 1), None);
        assert_eq!(next, u64::MAX);
    }

    // --- RateLimitFeed: ANSI-stripping and chunk reassembly ---
    //
    // Regression coverage for a live-stand defect: `provider.rate_limits`
    // used to be a bare `RateLimitDetector` fed the RAW PTY stream, escape
    // codes and all. codex's own `/status` render puts `\x1b[m` between
    // `limit:` and the progress bar, and `\x1b[2m` between `left` and
    // `(resets ...)` -- both inside gaps the quota-state regex's `\s*`
    // cannot see through -- so not one quota-state fact was ever observed
    // on a real run, despite the pattern being correct on clean text.

    /// The exact live bytes from the coordinator's capture (real `\x1b[m`
    /// and `\x1b[2m`, real `\r\n`), transcribed verbatim.
    const CODEX_STATUS_RAW_ANSI: &str = "│  5h limit:             \x1b[m[████████████████████] 100% left\x1b[2m (resets 00:08 on 31 Aug) │\r\n│  Weekly limit:         \x1b[m[████████████████████] 100% left\x1b[2m (resets 19:08 on 6 Sep)  │";

    #[test]
    fn bare_detector_misses_the_live_ansi_sample_that_rate_limit_feed_catches() {
        // This is the defect itself: matching the untouched raw stream
        // fails on real codex output, exactly as observed live.
        let bare = RateLimitDetector::new_for_tool(CliTool::Codex);
        assert!(bare.detect(CODEX_STATUS_RAW_ANSI).is_none());

        // `RateLimitFeed` strips ANSI (via `VteParser`) before matching,
        // so the same bytes must now be recognized.
        let mut feed = RateLimitFeed::new_for_tool(CliTool::Codex);
        let info = feed
            .detect(CODEX_STATUS_RAW_ANSI)
            .expect("quota-state line must be recognized once ANSI is stripped");
        assert_eq!(info.limit_type, RateLimitType::Session);
        assert_eq!(info.usage_percent, Some(0.0));
        assert_eq!(info.resets_at_text.as_deref(), Some("00:08 on 31 Aug"));
    }

    #[test]
    fn rate_limit_feed_reassembles_a_quota_state_line_split_mid_escape_and_mid_row() {
        // A live PTY chunk boundary has no reason to land anywhere
        // convenient -- here it falls INSIDE the `\x1b[2m` escape itself
        // (right after the ESC byte), on top of splitting the row.
        let first =
            "│  5h limit:             \x1b[m[████████████████████] 100% left\x1b";
        let second = "[2m (resets 00:08 on 31 Aug) │\r\n";

        let mut feed = RateLimitFeed::new_for_tool(CliTool::Codex);
        assert!(
            feed.detect(first).is_none(),
            "the row is not complete yet, so nothing should match on the first chunk"
        );
        let info = feed
            .detect(second)
            .expect("the row completes once the second chunk arrives");
        assert_eq!(info.limit_type, RateLimitType::Session);
        assert_eq!(info.resets_at_text.as_deref(), Some("00:08 on 31 Aug"));
    }

    #[test]
    fn rate_limit_feed_strips_ansi_before_matching_a_colored_failure_message() {
        // Failure patterns run through the same `RateLimitFeed::detect`
        // entry point as quota-state ones, so a vendor coloring its own
        // refusal text must not defeat detection either.
        let raw = "\x1b[31mError: rate limit exceeded\x1b[0m. Please wait.";
        let mut feed = RateLimitFeed::new_for_tool(CliTool::Codex);
        let info = feed
            .detect(raw)
            .expect("a colored refusal message must still be recognized");
        assert_eq!(info.limit_type, RateLimitType::Unknown);
    }

    #[test]
    fn rate_limit_feed_bounds_its_buffer_across_a_newline_free_redraw() {
        // A full-screen redraw with no `\n` at all (cursor-addressing
        // only) must not grow the pending-line buffer without bound for
        // as long as it runs -- and a real line must still be caught once
        // it finally arrives.
        let mut feed = RateLimitFeed::new_for_tool(CliTool::Codex);
        let noise = "x".repeat(super::RATE_LIMIT_FEED_BUFFER_MAX_BYTES * 3);
        assert!(feed.detect(&noise).is_none());
        assert!(feed.buffer.len() <= super::RATE_LIMIT_FEED_BUFFER_MAX_BYTES);

        let info = feed
            .detect(CODEX_STATUS_RAW_ANSI)
            .expect("a real quota-state line must still be recognized after the noise");
        assert_eq!(info.limit_type, RateLimitType::Session);
    }

    // -----------------------------------------------------------------------
    // host_policy_for_approval_level -- ApprovalLevel -> HostPolicy for ACP
    // -----------------------------------------------------------------------

    #[test]
    fn full_auto_maps_to_the_permissive_host_policy() {
        // The one combination that must never happen is `FullAuto` paired
        // with a refusing `HostPolicy` -- the process believes it has full
        // autonomy while the host silently refuses it.
        assert_eq!(host_policy_for_approval_level(ApprovalLevel::FullAuto), HostPolicy::Yolo);
    }

    #[test]
    fn read_only_maps_to_the_read_only_host_policy() {
        assert_eq!(
            host_policy_for_approval_level(ApprovalLevel::ReadOnly),
            HostPolicy::ReadOnly
        );
    }

    #[test]
    fn moderate_and_unmanaged_both_fall_back_to_the_default_host_policy() {
        assert_eq!(host_policy_for_approval_level(ApprovalLevel::Moderate), HostPolicy::Auto);
        assert_eq!(host_policy_for_approval_level(ApprovalLevel::Unmanaged), HostPolicy::Auto);
        assert_eq!(HostPolicy::default(), HostPolicy::Auto);
    }

    #[test]
    fn no_approval_level_ever_maps_to_a_refusing_policy_except_read_only_itself() {
        // Guards the "never full auto + refusing host" invariant generally:
        // `Deny` must never be reachable from this mapping at all -- it is
        // not one of the four `ApprovalLevel` outcomes, only `ReadOnly`'s
        // own deliberate restriction is.
        for level in [
            ApprovalLevel::FullAuto,
            ApprovalLevel::Moderate,
            ApprovalLevel::ReadOnly,
            ApprovalLevel::Unmanaged,
        ] {
            assert_ne!(
                host_policy_for_approval_level(level),
                HostPolicy::Deny,
                "{level:?} must never map to HostPolicy::Deny"
            );
        }
    }
}
