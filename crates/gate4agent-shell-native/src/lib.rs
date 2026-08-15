//! Native effect execution for gate4agent control-plane sessions.

mod provider_supervisor;

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

use gate4agent::agent::ReadinessStatus;
use gate4agent::pty::cli::codex::strip_ansi_codes;
use gate4agent::pty::cli::{create_pipeline, ClassificationPipeline, MessageClass, ParsedMessage};
use gate4agent::pty::event::PtyMouseProtocolEncoding;
use gate4agent::pty::{
    PtyAttachment, PtyEvent, PtyEventEnvelope, PtyEventReceiver, PtyForegroundObservation,
    PtyReplayCursor, PtySession, PtyTerminalSnapshot, RateLimitDetector,
};
use gate4agent::{
    AcpSession, AcpSessionOptions, AgentEvent, CliTool, LaunchRequest, PipeProcessOptions,
    PipeSession, PromptFraming, ReadinessIntent, ReadinessPermit, ReadinessTracker, RuntimePlatform,
    SessionConfig,
};
use gate4agent_adapters::{
    build_resume_plan_for_identity, builtin_adapter_registry, AdapterRuntimeRegistry,
    CodexPtySessionIdentityExtractor, KimiPtySessionIdentityExtractor, OneShotSessionPersistence,
    QwenDualOutputLine, QwenDualOutputParser, QWEN_DUAL_OUTPUT_MAX_LINE_BYTES,
};
use gate4agent_catalog::{AgentRegistry, AgentSpec, EnvMutation};
use gate4agent_shell_one_shot::NativeOneShotSession;
use gate4agent_types::{
    AdapterFamily, AgentCommand, AgentId, AgentInstanceId, CapabilityProbeFailure,
    ContextWindowUsage as ProviderContextWindowUsage, ControlEffect,
    ControlObservation, EffectEnvelope, ForegroundProcess, ForegroundProcessKind,
    ForegroundRequirement, InputAction, ObservationEnvelope, OperationId, PipeProtocol,
    PreparedInputKind, PromptPayload, ProviderEvent, ProviderInteractionKind,
    ProviderRuntimeCapability, ProviderRuntimePolicy, ProviderSessionIdentity, ProviderSessionKey,
    ProviderSource, ResumeLaunchRequest, SessionGeneration, StartRequest, TerminalFrame,
    TerminalMouseProtocolEncoding, TerminalSize, TokenUsage, TransportKind,
    CONTROL_PROTOCOL_VERSION, WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use uuid::Uuid;

const INSTANCE_LAUNCH_ARGS_MAX: usize = 128;
const INSTANCE_LAUNCH_ARG_MAX_BYTES: usize = 65_536;
const INSTANCE_LAUNCH_ARGS_TOTAL_MAX_BYTES: usize = 262_144;
const QWEN_SIDECAR_READ_MAX_BYTES_PER_TICK: usize = 262_144;
const QWEN_SIDECAR_READ_CHUNK_BYTES: usize = 16_384;
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
const RESERVED_QWEN_SIDECAR_FLAGS: &[&str] = &["--json-fd", "--json-file"];

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
    qwen_sidecar: Option<QwenDualOutputLaunch>,
}

struct OwnedPtySession {
    session: PtySession,
    spawn_operation_id: OperationId,
    last_terminal_sequence: u64,
    terminal_stale_published: bool,
    runtime_policy: ProviderRuntimePolicy,
    provider: Option<OwnedPtyProvider>,
    qwen_sidecar: Option<OwnedQwenDualOutput>,
}

pub struct QwenDualOutputLaunch {
    binding: gate4agent_types::AdapterBinding,
    directory: Option<PathBuf>,
    output_file: Option<PathBuf>,
    initial_gap: bool,
}

impl QwenDualOutputLaunch {
    pub fn prepare(binding: gate4agent_types::AdapterBinding) -> Result<Self, String> {
        let directory = std::env::temp_dir().join(format!(
            "gate4agent-qwen-sidecar-{}",
            Uuid::new_v4()
        ));
        create_private_directory(&directory).map_err(|error| {
            format!("failed to create private Qwen dual-output directory: {error}")
        })?;
        let output_file = directory.join("events.jsonl");
        if let Err(error) = File::create(&output_file) {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(format!("failed to create Qwen dual-output file: {error}"));
        }
        Ok(Self {
            binding,
            directory: Some(directory),
            output_file: Some(output_file),
            initial_gap: false,
        })
    }

    pub fn unavailable(binding: gate4agent_types::AdapterBinding) -> Self {
        Self {
            binding,
            directory: None,
            output_file: None,
            initial_gap: true,
        }
    }

    pub fn append_launch_arguments(&self, arguments: &mut Vec<OsString>) {
        if let Some(path) = &self.output_file {
            arguments.push(OsString::from("--json-file"));
            arguments.push(path.as_os_str().to_owned());
        }
    }

    #[cfg(test)]
    fn output_file(&self) -> Option<&Path> {
        self.output_file.as_deref()
    }
}

impl Drop for QwenDualOutputLaunch {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

struct OwnedQwenDualOutput {
    source: ProviderSource,
    directory: Option<PathBuf>,
    output_file: Option<PathBuf>,
    offset: u64,
    pending: Vec<u8>,
    discarding_oversized_line: bool,
    parser: QwenDualOutputParser,
    next_provider_sequence: u64,
    gap_pending: u64,
    tail_unavailable: bool,
}

impl From<QwenDualOutputLaunch> for OwnedQwenDualOutput {
    fn from(mut launch: QwenDualOutputLaunch) -> Self {
        Self {
            source: ProviderSource {
                family: AdapterFamily::Pipe,
                binding: launch.binding.clone(),
            },
            directory: launch.directory.take(),
            output_file: launch.output_file.take(),
            offset: 0,
            pending: Vec::new(),
            discarding_oversized_line: false,
            parser: QwenDualOutputParser::default(),
            next_provider_sequence: 1,
            gap_pending: u64::from(launch.initial_gap),
            tail_unavailable: launch.initial_gap,
        }
    }
}

impl Drop for OwnedQwenDualOutput {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir(path)
}

struct OwnedPtyProvider {
    source: ProviderSource,
    receiver: PtyEventReceiver,
    replay: VecDeque<PtyEventEnvelope>,
    pending_events: VecDeque<ProviderEvent>,
    utf8: Utf8ChunkDecoder,
    pipeline: Mutex<ClassificationPipeline>,
    rate_limits: RateLimitDetector,
    kimi_identity: Option<KimiPtySessionIdentityExtractor>,
    semantic_events: bool,
    provider_session_started: bool,
    next_provider_sequence: u64,
}

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
        }
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
            None,
        )
        .await
    }

    pub async fn execute_with_launch_context(
        &mut self,
        envelope: EffectEnvelope,
        environment: Vec<EnvMutation>,
        extra_args: Vec<OsString>,
        one_shot_session_persistence: OneShotSessionPersistence,
        qwen_sidecar: Option<QwenDualOutputLaunch>,
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
                            qwen_sidecar,
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
                        qwen_sidecar,
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
                ControlEffect::ResolveInteraction { target, .. } => {
                    ControlObservation::InteractionResolutionFailed {
                        interaction_id: target.interaction_id,
                        message: "native interaction resolution authority is not configured"
                            .to_owned(),
                    }
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
            qwen_sidecar,
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
        if let Some(sidecar) = &qwen_sidecar {
            if transport != TransportKind::Pty
                || spec.capabilities.adapters.pty_sidecar.as_ref() != Some(&sidecar.binding)
            {
                return ControlObservation::SpawnFailed {
                    message: "Qwen dual-output sidecar requires its exact catalog PTY binding"
                        .to_owned(),
                };
            }
        }
        let working_dir = PathBuf::from(&request.working_directory);

        match transport {
            TransportKind::Pty if !spec.capabilities.transports.pty => {
                ControlObservation::SpawnFailed {
                    message: format!("agent '{agent_id}' does not support PTY transport"),
                }
            }
            TransportKind::Pty => {
                if let Some(sidecar) = &qwen_sidecar {
                    sidecar.append_launch_arguments(&mut launch_extra_args);
                }
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
                                        rate_limits: RateLimitDetector::new_for_tool(tool),
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
                            qwen_sidecar: qwen_sidecar.map(OwnedQwenDualOutput::from),
                        },
                    );
                    ControlObservation::Spawned { process_id }
                }
                    Err(error) => ControlObservation::SpawnFailed {
                        message: error.to_string(),
                    },
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
                let spawned = match acp_spec.launch_override.as_ref() {
                    Some(launch) => {
                        AcpSession::spawn_with_launch(
                            tool,
                            &working_dir,
                            AcpSessionOptions::default(),
                            launch,
                        )
                        .await
                    }
                    None => {
                        AcpSession::spawn(tool, &working_dir, AcpSessionOptions::default()).await
                    }
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
        qwen_sidecar: Option<QwenDualOutputLaunch>,
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
            session_options: None,
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
                qwen_sidecar,
            },
            pty_env,
        )
        .await
    }

    async fn stop_native(&mut self, key: NativeSessionKey, force: bool) -> ControlObservation {
        if let Some(mut owned) = self.pty_sessions.remove(&key) {
            let shutdown = owned.session.shutdown().await;
            if let Some(sidecar) = &mut owned.qwen_sidecar {
                self.pending_observations.extend(drain_qwen_sidecar(key, sidecar));
                self.pending_observations.extend(finish_qwen_sidecar(key, sidecar));
            }
            return match shutdown {
                Ok(outcome) => ControlObservation::StopCompleted {
                    forced: force || outcome.termination.is_some(),
                    exit_code: outcome.exit_code,
                    final_terminal: Some(terminal_frame(outcome.terminal)),
                },
                Err(error) => ControlObservation::StopFailed {
                    message: error.to_string(),
                },
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
            let mut owned = self
                .pty_sessions
                .remove(&key)
                .expect("completed key came from the owned session map");
            let (exit_code, final_terminal) = match owned.session.shutdown().await {
                Ok(outcome) => (outcome.exit_code, Some(terminal_frame(outcome.terminal))),
                Err(_) => (None, None),
            };
            if let Some(sidecar) = &mut owned.qwen_sidecar {
                observations.extend(drain_qwen_sidecar(key, sidecar));
                observations.extend(finish_qwen_sidecar(key, sidecar));
            }
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
            if let Some(sidecar) = &mut owned.qwen_sidecar {
                observations.extend(drain_qwen_sidecar(*key, sidecar));
            }
        }
        collect_provider_map(&mut self.pipe_sessions, &mut observations);
        collect_provider_map(&mut self.one_shot_sessions, &mut observations);
        collect_provider_map(&mut self.acp_sessions, &mut observations);
        observations
    }

    /// Capture only changed terminal frames. Snapshot failures become an
    /// explicit stale observation once, until a later successful frame heals it.
    pub fn collect_terminal_frames(&mut self) -> Vec<ObservationEnvelope> {
        let mut observations = Vec::new();
        for (key, owned) in &mut self.pty_sessions {
            match owned.session.terminal_state() {
                Ok(snapshot) if snapshot.sequence > owned.last_terminal_sequence => {
                    owned.last_terminal_sequence = snapshot.sequence;
                    owned.terminal_stale_published = false;
                    observations.push(ObservationEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id: None,
                        instance_id: key.instance_id,
                        generation: key.generation,
                        observation: ControlObservation::TerminalFrame {
                            frame: terminal_frame(snapshot),
                        },
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
}

fn missing_session_message(key: NativeSessionKey) -> String {
    format!(
        "native session {:?}/{:?} does not exist",
        key.instance_id, key.generation
    )
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
        if agent_id.as_str() == "qwen-code"
            && RESERVED_QWEN_SIDECAR_FLAGS.iter().any(|reserved| {
                argument == *reserved
                    || argument
                        .strip_prefix(reserved)
                        .is_some_and(|suffix| suffix.starts_with('='))
            })
        {
            return Err(
                "native instance launch arguments conflict with Qwen sidecar output authority"
                    .to_owned(),
            );
        }
    }
    Ok(())
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

fn drain_qwen_sidecar(
    key: NativeSessionKey,
    sidecar: &mut OwnedQwenDualOutput,
) -> Vec<ObservationEnvelope> {
    let mut observations = Vec::new();
    publish_qwen_pending_gap(key, sidecar, &mut observations);
    let Some(path) = sidecar.output_file.clone() else {
        return observations;
    };
    let length = match std::fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(_) => {
            publish_qwen_tail_failure(key, sidecar, &mut observations);
            return observations;
        }
    };
    if length < sidecar.offset {
        sidecar.offset = 0;
        sidecar.pending.clear();
        sidecar.discarding_oversized_line = false;
        sidecar.gap_pending = sidecar.gap_pending.saturating_add(1);
    }
    let mut file = match OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(_) => {
            publish_qwen_tail_failure(key, sidecar, &mut observations);
            return observations;
        }
    };
    if file.seek(SeekFrom::Start(sidecar.offset)).is_err() {
        publish_qwen_tail_failure(key, sidecar, &mut observations);
        return observations;
    }
    sidecar.tail_unavailable = false;
    let mut read_total = 0usize;
    let mut chunk = [0u8; QWEN_SIDECAR_READ_CHUNK_BYTES];
    while read_total < QWEN_SIDECAR_READ_MAX_BYTES_PER_TICK {
        let limit = (QWEN_SIDECAR_READ_MAX_BYTES_PER_TICK - read_total).min(chunk.len());
        let read = match file.read(&mut chunk[..limit]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => {
                publish_qwen_tail_failure(key, sidecar, &mut observations);
                break;
            }
        };
        read_total += read;
        sidecar.offset = sidecar.offset.saturating_add(read as u64);
        consume_qwen_bytes(key, sidecar, &chunk[..read], &mut observations);
    }
    publish_qwen_pending_gap(key, sidecar, &mut observations);
    observations
}

fn finish_qwen_sidecar(
    key: NativeSessionKey,
    sidecar: &mut OwnedQwenDualOutput,
) -> Vec<ObservationEnvelope> {
    let mut observations = Vec::new();
    if !sidecar.pending.is_empty() {
        sidecar.pending.clear();
        sidecar.gap_pending = sidecar.gap_pending.saturating_add(1);
    }
    if !sidecar.parser.clean_end_seen() {
        sidecar.gap_pending = sidecar.gap_pending.saturating_add(1);
    }
    publish_qwen_pending_gap(key, sidecar, &mut observations);
    observations
}

fn consume_qwen_bytes(
    key: NativeSessionKey,
    sidecar: &mut OwnedQwenDualOutput,
    bytes: &[u8],
    observations: &mut Vec<ObservationEnvelope>,
) {
    for byte in bytes {
        if sidecar.discarding_oversized_line {
            if *byte == b'\n' {
                sidecar.discarding_oversized_line = false;
            }
            continue;
        }
        if *byte == b'\n' {
            if sidecar.pending.last() == Some(&b'\r') {
                sidecar.pending.pop();
            }
            let line = std::mem::take(&mut sidecar.pending);
            match sidecar.parser.parse_line(&line) {
                QwenDualOutputLine::Events(events) => {
                    publish_qwen_pending_gap(key, sidecar, observations);
                    for event in events {
                        push_qwen_provider_observation(key, sidecar, event, observations);
                    }
                }
                QwenDualOutputLine::Ignored => {}
                QwenDualOutputLine::Gap => {
                    sidecar.gap_pending = sidecar.gap_pending.saturating_add(1);
                    publish_qwen_pending_gap(key, sidecar, observations);
                }
            }
        } else if sidecar.pending.len() == QWEN_DUAL_OUTPUT_MAX_LINE_BYTES {
            sidecar.pending.clear();
            sidecar.discarding_oversized_line = true;
            sidecar.gap_pending = sidecar.gap_pending.saturating_add(1);
            publish_qwen_pending_gap(key, sidecar, observations);
        } else {
            sidecar.pending.push(*byte);
        }
    }
}

fn publish_qwen_tail_failure(
    key: NativeSessionKey,
    sidecar: &mut OwnedQwenDualOutput,
    observations: &mut Vec<ObservationEnvelope>,
) {
    if !sidecar.tail_unavailable {
        sidecar.tail_unavailable = true;
        sidecar.gap_pending = sidecar.gap_pending.saturating_add(1);
    }
    publish_qwen_pending_gap(key, sidecar, observations);
}

fn publish_qwen_pending_gap(
    key: NativeSessionKey,
    sidecar: &mut OwnedQwenDualOutput,
    observations: &mut Vec<ObservationEnvelope>,
) {
    let missed = std::mem::take(&mut sidecar.gap_pending);
    let Some(source_sequence) = reserve_provider_gap_sequence(
        &mut sidecar.next_provider_sequence,
        missed,
    ) else {
        return;
    };
    observations.push(ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: None,
        instance_id: key.instance_id,
        generation: key.generation,
        observation: ControlObservation::ProviderGap {
            source: sidecar.source.clone(),
            source_sequence,
            missed,
        },
    });
}

fn push_qwen_provider_observation(
    key: NativeSessionKey,
    sidecar: &mut OwnedQwenDualOutput,
    event: ProviderEvent,
    observations: &mut Vec<ObservationEnvelope>,
) {
    let sequence = sidecar.next_provider_sequence;
    sidecar.next_provider_sequence = sidecar.next_provider_sequence.saturating_add(1);
    observations.push(ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: None,
        instance_id: key.instance_id,
        generation: key.generation,
        observation: ControlObservation::ProviderEvent {
            source: sidecar.source.clone(),
            sequence,
            event,
        },
    });
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
        limit_type: format!("{:?}", info.limit_type),
        resets_at: info.resets_at.map(|value| value.to_rfc3339()),
        usage_percent: info.usage_percent.map(|value| value.to_string()),
        raw_message: info.raw_message,
    }
}

fn collect_provider_map<S>(
    sessions: &mut BTreeMap<NativeSessionKey, OwnedProviderSession<S>>,
    observations: &mut Vec<ObservationEnvelope>,
) {
    for (key, owned) in sessions {
        drain_provider_stream(
            *key,
            &owned.source,
            &mut owned.events,
            &mut owned.pending_events,
            &mut owned.next_provider_sequence,
            Some(&mut owned.observed_exit_code),
            observations,
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
                let Some(event) = provider_event(event) else {
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

fn provider_event(event: AgentEvent) -> Option<ProviderEvent> {
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
        AgentEvent::Started { .. }
        | AgentEvent::Exited { .. }
        | AgentEvent::PtyRaw { .. }
        | AgentEvent::RpcNotification { .. }
        | AgentEvent::RpcIncomingRequest { .. } => None,
    }
}

fn builtin_legacy_adapter_runtimes() -> AdapterRuntimeRegistry<CliTool> {
    let definitions = [
        ("claude-code", CliTool::ClaudeCode),
        ("codex", CliTool::Codex),
        ("gemini", CliTool::Gemini),
        ("opencode", CliTool::OpenCode),
        ("kimi", CliTool::KimiCode),
    ];
    let mut runtimes = AdapterRuntimeRegistry::default();
    for (id, tool) in definitions {
        for family in [AdapterFamily::PtySemantic, AdapterFamily::Pipe] {
            let binding = builtin_adapter_registry()
                .binding(family, id)
                .unwrap_or_else(|| panic!("missing built-in {family:?} adapter {id}"))
                .clone();
            runtimes
                .insert(family, binding, tool)
                .expect("built-in native adapter runtime must be unique");
        }
        if let Some(binding) = builtin_adapter_registry().binding(AdapterFamily::Acp, id) {
            runtimes
                .insert(AdapterFamily::Acp, binding.clone(), tool)
                .expect("built-in native ACP adapter runtime must be unique");
        }
    }
    runtimes
}

fn terminal_frame(snapshot: PtyTerminalSnapshot) -> TerminalFrame {
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
    }
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
            return Err(startup_operator_error(spec, gate));
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

fn ensure_no_startup_operator_gate(session: &PtySession, spec: &AgentSpec) -> Result<(), String> {
    let snapshot = session.terminal_state().map_err(|error| error.to_string())?;
    match startup_operator_gate(&snapshot.contents) {
        Some(gate) => Err(startup_operator_error(spec, gate)),
        None => Ok(()),
    }
}

fn startup_operator_error(spec: &AgentSpec, gate: &str) -> String {
    format!(
        "agent '{}' requires operator action at startup ({gate}); initial prompt was not submitted",
        spec.id
    )
}

fn ensure_no_readiness_operator_gate(
    diagnostics: &ReadinessDiagnostics,
    spec: &AgentSpec,
) -> Result<(), String> {
    match diagnostics.operator_gate {
        Some(gate) => Err(startup_operator_error(spec, gate)),
        None => Ok(()),
    }
}

fn startup_operator_gate(contents: &str) -> Option<&'static str> {
    let normalized = contents
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    if [
        "trust this folder",
        "trust the files in this folder",
        "trust the contents of this directory",
        "do you trust this directory",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        return Some("workspace trust");
    }
    if normalized.contains("quick safety check")
        && (normalized.contains("yes, i trust this folder")
            || normalized.contains("continue without these permissions"))
    {
        return Some("workspace trust");
    }
    if [
        "select authentication method",
        "choose how to authenticate",
        "no auth type is selected",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
    {
        return Some("authentication");
    }
    if normalized.contains("sign in")
        && (normalized.contains("openai")
            || normalized.contains("chatgpt")
            || normalized.contains("codex"))
    {
        return Some("authentication");
    }
    if normalized.contains("kimi code update available")
        && normalized.contains("install update now")
    {
        return Some("vendor update");
    }
    if normalized.contains("choose the text style that looks best with your terminal") {
        return Some("terminal appearance setup");
    }
    if normalized.contains("welcome to claude code for")
        && (normalized.contains("open files") || normalized.contains("selected lines"))
    {
        return Some("IDE onboarding");
    }
    if normalized.contains("welcome to claude code")
        && (normalized.contains("press enter") || normalized.contains("enter to continue"))
    {
        return Some("Claude onboarding");
    }
    Some("configuration migration").filter(|_| {
        normalized.contains("migration")
            && normalized.contains("enter confirm")
            && normalized.contains("esc")
    })
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
    operator_gate: Option<&'static str>,
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
        drain_qwen_sidecar, finish_qwen_sidecar, prepare_fresh_pty_provider_session,
        prompt_render_probe, prompt_rendered, reserve_provider_gap_sequence, NativeSessionKey,
        NativeEffectShell, OwnedQwenDualOutput, QwenDualOutputLaunch,
        QWEN_SIDECAR_READ_MAX_BYTES_PER_TICK,
        should_attach_pty_provider_stream, should_probe_pty_identity, startup_operator_gate,
        terminal_frame, validate_instance_launch_arguments, validate_spawn_runtime_policy,
        ReadinessDiagnostics, Utf8ChunkDecoder,
    };
    use gate4agent_adapters::builtin_adapter_registry;
    use gate4agent::core::types::{
        AgentEvent, ContextWindowUsage as AgentContextWindowUsage,
    };
    use gate4agent::pty::event::PtyMouseProtocolEncoding;
    use gate4agent_types::{
        AdapterFamily, AgentId, AgentInstanceId, ControlEffect, ControlObservation, EffectEnvelope,
        OperationId, ProviderEvent, ProviderRuntimePolicy, SessionGeneration, StartRequest,
        TerminalMouseProtocolEncoding, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
    };
    use std::ffi::OsString;
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

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
        }
    }

    #[test]
    fn terminal_frame_preserves_scrollback_and_terminal_input_metadata() {
        let mut snapshot = snapshot(7, "visible");
        snapshot.scrollback_formatted = vec![b"older".to_vec()];
        snapshot.alternate_screen = true;
        snapshot.mouse_protocol_enabled = true;
        snapshot.mouse_protocol_encoding = PtyMouseProtocolEncoding::Sgr;

        let frame = terminal_frame(snapshot);
        assert_eq!(frame.scrollback_formatted, vec![b"older".to_vec()]);
        assert!(frame.alternate_screen);
        assert!(frame.mouse_protocol_enabled);
        assert_eq!(frame.mouse_protocol_encoding, TerminalMouseProtocolEncoding::Sgr);
    }

    #[test]
    fn structured_context_usage_maps_without_pty_inference() {
        let mapped = super::provider_event(AgentEvent::ContextWindowUsage {
            usage: AgentContextWindowUsage {
                uncached_input_tokens: 70,
                cache_read_tokens: 20,
                cache_write_tokens: 0,
                output_tokens: 10,
                unattributed_tokens: 5,
                used_tokens: 105,
                capacity_tokens: 100,
            },
        });
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
        assert!(super::provider_event(AgentEvent::PtyRaw { data: b"105/100".to_vec() }).is_none());
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

    #[test]
    fn startup_operator_gates_are_classified_without_returning_terminal_text() {
        assert_eq!(
            startup_operator_gate(" Trust this\nfolder? "),
            Some("workspace trust")
        );
        assert_eq!(
            startup_operator_gate("No auth type is selected"),
            Some("authentication")
        );
        assert_eq!(
            startup_operator_gate("Sign in with OpenAI to use Codex"),
            Some("authentication")
        );
        assert_eq!(
            startup_operator_gate(
                "Kimi Code Update Available\nInstall update now (0.32.0)\nEnter confirm"
            ),
            Some("vendor update")
        );
        assert_eq!(
            startup_operator_gate(
                "Welcome to Claude Code\nChoose the text style that looks best with your terminal"
            ),
            Some("terminal appearance setup")
        );
        assert_eq!(
            startup_operator_gate(
                "Welcome to Claude Code for VS Code\nClaude has context of open files and selected lines"
            ),
            Some("IDE onboarding")
        );
        assert_eq!(
            startup_operator_gate("Welcome to Claude Code\n❯ Press Enter to continue"),
            Some("Claude onboarding")
        );
        assert_eq!(
            startup_operator_gate("Welcome to Claude Code\n❯ ready\nEnter to send"),
            None
        );
        assert_eq!(
            startup_operator_gate(
                "Quick safety check: Is this a project you trust?\nYes, I trust this folder\nNo, continue without these permissions"
            ),
            Some("workspace trust")
        );
        assert_eq!(startup_operator_gate("ready for a prompt"), None);
    }

    #[test]
    fn readiness_diagnostics_detects_an_ansi_split_gate_without_exposing_text() {
        let mut diagnostics = ReadinessDiagnostics::default();
        diagnostics.observe_output(b"\x1b[31mNo auth ");
        diagnostics.observe_output(b"\x1b[0mtype is selected");
        assert_eq!(diagnostics.operator_gate, Some("authentication"));
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
            ProviderRuntimePolicy::new(true, true, false, true, false)
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
            ProviderRuntimePolicy::new(true, true, true, true, false)
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

        let resume_without_prompt = ProviderRuntimePolicy::new(true, false, false, true, true)
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

    fn qwen_sidecar_fixture() -> (OwnedQwenDualOutput, PathBuf, PathBuf) {
        let binding = builtin_adapter_registry()
            .binding(AdapterFamily::Pipe, "qwen-code")
            .unwrap()
            .clone();
        let launch = QwenDualOutputLaunch::prepare(binding).unwrap();
        let path = launch.output_file().unwrap().to_owned();
        let directory = launch.directory.as_ref().unwrap().clone();
        (launch.into(), path, directory)
    }

    fn qwen_key() -> NativeSessionKey {
        NativeSessionKey {
            instance_id: AgentInstanceId(91),
            generation: SessionGeneration(3),
        }
    }

    fn append(path: &Path, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn qwen_sidecar_split_utf8_and_partial_line_emit_only_complete_facts() {
        let (mut sidecar, path, directory) = qwen_sidecar_fixture();
        append(&path, b"{\"type\":\"system\",\"subtype\":\"session_start\"}\n");
        let line = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-utf8\",\"name\":\"read_файл\",\"input\":{}}]}}\n";
        let split = line.find('ф').unwrap() + 1;
        append(&path, &line.as_bytes()[..split]);
        let first = drain_qwen_sidecar(qwen_key(), &mut sidecar);
        assert_eq!(first.len(), 1);
        assert!(matches!(
            first[0].observation,
            ControlObservation::ProviderEvent { event: ProviderEvent::Ready, .. }
        ));
        append(&path, &line.as_bytes()[split..]);
        let second = drain_qwen_sidecar(qwen_key(), &mut sidecar);
        assert!(second.iter().any(|observation| matches!(
            &observation.observation,
            ControlObservation::ProviderEvent {
                event: ProviderEvent::ToolStarted { id, name, input_json, .. },
                ..
            } if id == "tool-utf8" && name == "read_файл" && input_json.is_empty()
        )));
        drop(sidecar);
        assert!(!directory.exists());
    }

    #[test]
    fn qwen_sidecar_malformed_oversize_truncation_and_backlog_are_bounded_gaps() {
        let (mut sidecar, path, _) = qwen_sidecar_fixture();
        append(&path, b"malformed\n");
        append(
            &path,
            &vec![b'x'; gate4agent_adapters::QWEN_DUAL_OUTPUT_MAX_LINE_BYTES + 1],
        );
        append(&path, b"\n{\"type\":\"system\",\"subtype\":\"session_start\"}\n");
        let mut observed = Vec::new();
        for tick in 1..=8 {
            observed.extend(drain_qwen_sidecar(qwen_key(), &mut sidecar));
            assert!(
                sidecar.offset
                    <= tick * QWEN_SIDECAR_READ_MAX_BYTES_PER_TICK as u64
            );
            if sidecar.parser.handshake_seen() {
                break;
            }
        }
        assert!(sidecar.parser.handshake_seen());
        assert_eq!(
            observed
                .iter()
                .filter_map(|observation| match observation.observation {
                    ControlObservation::ProviderGap { source_sequence, .. } => {
                        Some(source_sequence)
                    }
                    ControlObservation::ProviderEvent { sequence, .. } => Some(sequence),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            observed
                .iter()
                .filter(|observation| matches!(
                    observation.observation,
                    ControlObservation::ProviderGap { .. }
                ))
                .count(),
            2
        );
        File::create(&path)
            .unwrap()
            .write_all(b"{\"type\":\"system\",\"subtype\":\"session_end\"}\n")
            .unwrap();
        let after_truncation = drain_qwen_sidecar(qwen_key(), &mut sidecar);
        assert!(matches!(
            after_truncation[0].observation,
            ControlObservation::ProviderGap { .. }
        ));
        assert!(after_truncation.iter().any(|observation| matches!(
            observation.observation,
            ControlObservation::ProviderEvent { event: ProviderEvent::SessionEnded { .. }, .. }
        )));
    }

    #[test]
    fn qwen_sidecar_abrupt_eof_is_gap_but_clean_session_end_is_not() {
        let (mut abrupt, abrupt_path, _) = qwen_sidecar_fixture();
        append(&abrupt_path, b"{\"type\":\"system\",\"subtype\":\"session_start\"}\n");
        assert_eq!(drain_qwen_sidecar(qwen_key(), &mut abrupt).len(), 1);
        let abrupt_end = finish_qwen_sidecar(qwen_key(), &mut abrupt);
        assert_eq!(abrupt_end.len(), 1);
        assert!(matches!(
            abrupt_end[0].observation,
            ControlObservation::ProviderGap { .. }
        ));

        let (mut clean, clean_path, _) = qwen_sidecar_fixture();
        append(&clean_path, b"{\"type\":\"system\",\"subtype\":\"session_start\"}\n{\"type\":\"system\",\"subtype\":\"session_end\"}\n");
        assert_eq!(drain_qwen_sidecar(qwen_key(), &mut clean).len(), 2);
        assert!(finish_qwen_sidecar(qwen_key(), &mut clean).is_empty());
    }

    #[test]
    fn unavailable_qwen_sidecar_emits_one_gap_without_launch_arguments() {
        let binding = builtin_adapter_registry()
            .binding(AdapterFamily::Pipe, "qwen-code")
            .unwrap()
            .clone();
        let launch = QwenDualOutputLaunch::unavailable(binding);
        let mut arguments = Vec::new();
        launch.append_launch_arguments(&mut arguments);
        assert!(arguments.is_empty());
        let mut sidecar = OwnedQwenDualOutput::from(launch);
        assert_eq!(drain_qwen_sidecar(qwen_key(), &mut sidecar).len(), 1);
        assert!(drain_qwen_sidecar(qwen_key(), &mut sidecar).is_empty());
    }

    #[test]
    fn qwen_sidecar_launch_cleanup_covers_pre_spawn_and_owned_lifetimes() {
        let binding = builtin_adapter_registry()
            .binding(AdapterFamily::Pipe, "qwen-code")
            .unwrap()
            .clone();
        let launch = QwenDualOutputLaunch::prepare(binding.clone()).unwrap();
        let pre_spawn_directory = launch.directory.as_ref().unwrap().clone();
        drop(launch);
        assert!(!pre_spawn_directory.exists());

        let launch = QwenDualOutputLaunch::prepare(binding).unwrap();
        let owned_directory = launch.directory.as_ref().unwrap().clone();
        let owned = OwnedQwenDualOutput::from(launch);
        drop(owned);
        assert!(!owned_directory.exists());
    }

    #[tokio::test]
    async fn qwen_sidecar_spawn_failure_cleans_private_directory() {
        let binding = builtin_adapter_registry()
            .binding(AdapterFamily::Pipe, "qwen-code")
            .unwrap()
            .clone();
        let mut spec = gate4agent_testkit::interactive_agent_spec();
        spec.id = AgentId::new("qwen-code").unwrap();
        spec.capabilities.adapters.pty_sidecar = Some(binding.clone());
        let mut shell = NativeEffectShell::new(
            gate4agent_catalog::AgentRegistry::new([spec]).unwrap(),
        );
        let launch = QwenDualOutputLaunch::prepare(binding).unwrap();
        let directory = launch.directory.as_ref().unwrap().clone();
        let failed = shell
            .execute_with_launch_context(
                EffectEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: OperationId(80),
                    instance_id: AgentInstanceId(90),
                    generation: SessionGeneration(1),
                    effect: ControlEffect::Spawn {
                        agent_id: AgentId::new("qwen-code").unwrap(),
                        transport: TransportKind::Pty,
                        runtime_policy: ProviderRuntimePolicy::raw_pty(),
                        request: StartRequest {
                            working_directory: String::new(),
                            terminal_size: TerminalSize { rows: 24, columns: 80 },
                            initial_prompt: None,
                            session_options: None,
                        },
                    },
                },
                Vec::new(),
                Vec::new(),
                gate4agent_adapters::OneShotSessionPersistence::Ephemeral,
                Some(launch),
            )
            .await;
        assert!(matches!(failed.observation, ControlObservation::SpawnFailed { .. }));
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn controlled_qwen_pty_fixture_delivers_sidecar_provider_events_and_cleans_up() {
        gate4agent_testkit::suppress_windows_fault_dialogs_for_test();
        gate4agent_testkit::require_windows_headless_supervisor_for_test();
        let binding = builtin_adapter_registry()
            .binding(AdapterFamily::Pipe, "qwen-code")
            .unwrap()
            .clone();
        let mut spec = gate4agent_testkit::interactive_agent_spec();
        spec.id = AgentId::new("qwen-code").unwrap();
        spec.capabilities.adapters.pty_sidecar = Some(binding.clone());
        #[cfg(windows)]
        {
            *spec.launch.fixed_args.last_mut().unwrap() = r#"& { param([Parameter(ValueFromRemainingArguments=$true)][string[]]$sidecarArgs) $index=[Array]::IndexOf($sidecarArgs,'--json-file'); if ($index -lt 0 -or $index + 1 -ge $sidecarArgs.Count) { exit 41 }; $path=$sidecarArgs[$index+1]; [IO.File]::AppendAllText($path, '{"type":"system","subtype":"session_start","data":{"protocol_version":2}}' + [Environment]::NewLine + '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"fixture-tool","name":"read_file","input":{"path":"private"}}]}}' + [Environment]::NewLine + '{"type":"control_request","request_id":"fixture-approval","request":{"subtype":"can_use_tool","tool_name":"read_file","tool_use_id":"fixture-tool","input":{"path":"private"}}}' + [Environment]::NewLine + '{"type":"control_response","response":{"subtype":"success","request_id":"fixture-approval","response":{"allowed":true}}}' + [Environment]::NewLine + '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"fixture-tool","content":"private","is_error":false}]}}' + [Environment]::NewLine + '{"type":"result","subtype":"success","usage":{"input_tokens":2,"output_tokens":3}}' + [Environment]::NewLine + '{"type":"system","subtype":"session_end"}' + [Environment]::NewLine); [Console]::Write('fixture-qwen-sidecar'); Start-Sleep -Milliseconds 200 }"#.to_owned();
        }
        #[cfg(not(windows))]
        {
            *spec.launch.fixed_args.last_mut().unwrap() = r#"printf '%s\n' '{"type":"system","subtype":"session_start","data":{"protocol_version":2}}' '{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"fixture-tool","name":"read_file","input":{"path":"private"}}]}}' '{"type":"control_request","request_id":"fixture-approval","request":{"subtype":"can_use_tool","tool_name":"read_file","tool_use_id":"fixture-tool","input":{"path":"private"}}}' '{"type":"control_response","response":{"subtype":"success","request_id":"fixture-approval","response":{"allowed":true}}}' '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"fixture-tool","content":"private","is_error":false}]}}' '{"type":"result","subtype":"success","usage":{"input_tokens":2,"output_tokens":3}}' '{"type":"system","subtype":"session_end"}' > "$1"; printf 'fixture-qwen-sidecar'; sleep 0.2"#.to_owned();
        }
        let catalog = gate4agent_catalog::AgentRegistry::new([spec]).unwrap();
        let mut shell = NativeEffectShell::new(catalog);
        let launch = QwenDualOutputLaunch::prepare(binding).unwrap();
        let directory = launch.directory.as_ref().unwrap().clone();
        let key = qwen_key();
        let spawned = shell
            .execute_with_launch_context(
                EffectEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: OperationId(81),
                    instance_id: key.instance_id,
                    generation: key.generation,
                    effect: ControlEffect::Spawn {
                        agent_id: AgentId::new("qwen-code").unwrap(),
                        transport: TransportKind::Pty,
                        runtime_policy: ProviderRuntimePolicy::raw_pty(),
                        request: StartRequest {
                            working_directory: std::env::current_dir()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                            terminal_size: TerminalSize { rows: 24, columns: 80 },
                            initial_prompt: None,
                            session_options: None,
                        },
                    },
                },
                Vec::new(),
                Vec::new(),
                gate4agent_adapters::OneShotSessionPersistence::Ephemeral,
                Some(launch),
            )
            .await;
        assert!(matches!(spawned.observation, ControlObservation::Spawned { .. }));

        let mut provider = Vec::new();
        let mut exited = false;
        for _ in 0..200 {
            provider.extend(shell.collect_provider_events());
            let exit_observations = shell.collect_exits().await;
            if !exit_observations.is_empty() {
                provider.extend(exit_observations);
                provider.extend(shell.collect_provider_events());
                exited = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !exited {
            let _ = shell
                .execute(EffectEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: OperationId(82),
                    instance_id: key.instance_id,
                    generation: key.generation,
                    effect: ControlEffect::Stop { force: true },
                })
                .await;
        }
        assert!(exited);
        assert!(!directory.exists());
        let events = provider
            .iter()
            .filter_map(|observation| match &observation.observation {
                ControlObservation::ProviderEvent { source, sequence, event } => {
                    assert_eq!(source.family, AdapterFamily::Pipe);
                    assert_eq!(source.binding.id.as_str(), "qwen-code");
                    Some((*sequence, event))
                }
                ControlObservation::ProviderGap { .. } => {
                    panic!("clean fixture emitted a gap: {provider:?}")
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            events.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(),
            (1..=events.len() as u64).collect::<Vec<_>>()
        );
        assert!(events.iter().any(|(_, event)| matches!(event, ProviderEvent::Ready)));
        assert!(events.iter().any(|(_, event)| matches!(event, ProviderEvent::ToolStarted { .. })));
        assert!(events.iter().any(|(_, event)| matches!(
            event,
            ProviderEvent::InteractionRequested {
                request_id: Some(request_id),
                prompt,
                ..
            } if request_id == "fixture-approval" && prompt.is_empty()
        )));
        assert!(events.iter().any(|(_, event)| matches!(
            event,
            ProviderEvent::InteractionResolved {
                request_id,
                outcome: gate4agent_types::ProviderInteractionOutcome::Approved,
            } if request_id == "fixture-approval"
        )));
        assert!(events.iter().any(|(_, event)| matches!(event, ProviderEvent::ToolCompleted { .. })));
        assert!(events.iter().any(|(_, event)| matches!(event, ProviderEvent::TurnCompleted { .. })));
        assert!(events.iter().any(|(_, event)| matches!(event, ProviderEvent::SessionEnded { .. })));
    }
}
