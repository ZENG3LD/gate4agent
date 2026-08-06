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
use gate4agent::pty::{
    PtyEvent, PtyEventEnvelope, PtyEventReceiver, PtyForegroundObservation, PtySession,
    PtyTerminalSnapshot, RateLimitDetector,
};
use gate4agent::{
    AcpSession, AcpSessionOptions, AgentEvent, CliTool, LaunchRequest, PipeProcessOptions,
    PipeSession, PromptFraming, ReadinessIntent, ReadinessPermit, ReadinessTracker, RuntimePlatform,
    SessionConfig,
};
use gate4agent_adapters::{
    build_resume_plan_for_identity, builtin_adapter_registry, AdapterRuntimeRegistry,
};
use gate4agent_catalog::{AgentRegistry, AgentSpec, EnvMutation};
use gate4agent_shell_one_shot::NativeOneShotSession;
use gate4agent_types::{
    AdapterFamily, AgentId, AgentInstanceId, CapabilityProbeFailure, ControlEffect,
    ControlObservation, EffectEnvelope, ForegroundProcess, ForegroundProcessKind,
    ForegroundRequirement, ObservationEnvelope, OperationId, PipeProtocol, PreparedInputKind,
    ProviderEvent, ProviderInteractionKind, ProviderSource, ResumeLaunchRequest, SessionGeneration,
    StartRequest, TerminalFrame, TerminalSize, TokenUsage, TransportKind, CONTROL_PROTOCOL_VERSION,
    WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NativeSessionKey {
    pub instance_id: AgentInstanceId,
    pub generation: SessionGeneration,
}

struct NativeSpawnRequest {
    agent_id: AgentId,
    transport: TransportKind,
    request: StartRequest,
    launch_extra_args: Vec<OsString>,
    resumed_provider_session: Option<gate4agent_types::ProviderSessionIdentity>,
}

struct OwnedPtySession {
    session: PtySession,
    spawn_operation_id: OperationId,
    last_terminal_sequence: u64,
    terminal_stale_published: bool,
    provider: Option<OwnedPtyProvider>,
}

struct OwnedPtyProvider {
    source: ProviderSource,
    receiver: PtyEventReceiver,
    replay: VecDeque<PtyEventEnvelope>,
    pending_events: VecDeque<ProviderEvent>,
    utf8: Utf8ChunkDecoder,
    pipeline: Mutex<ClassificationPipeline>,
    rate_limits: RateLimitDetector,
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
        self.execute_with_pty_env(envelope, Vec::new()).await
    }

    /// Execute an effect with shell-owned environment injected only into a
    /// newly spawned PTY process. The canonical start request cannot set these
    /// authority variables itself.
    pub async fn execute_with_pty_env(
        &mut self,
        envelope: EffectEnvelope,
        pty_env: Vec<EnvMutation>,
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
                    request,
                } => {
                    self.spawn_native(
                        key,
                        operation_id,
                        NativeSpawnRequest {
                            agent_id,
                            transport,
                            request,
                            launch_extra_args: Vec::new(),
                            resumed_provider_session: None,
                        },
                        pty_env,
                    )
                    .await
                }
                ControlEffect::SpawnResume {
                    agent_id,
                    transport,
                    provider_session,
                    request,
                } => {
                    self.spawn_resume(
                        key,
                        operation_id,
                        agent_id,
                        transport,
                        provider_session,
                        request,
                        pty_env,
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
                            match owned.session.send_shell_input(input).await {
                                Ok(()) => ControlObservation::InputCompleted,
                                Err(error) => ControlObservation::InputFailed {
                                    message: error.to_string(),
                                },
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
                            let intent = match input.kind() {
                                PreparedInputKind::InsertDraft
                                | PreparedInputKind::AgentCommand => ReadinessIntent::DraftPaste,
                                PreparedInputKind::SubmitPrompt => ReadinessIntent::FollowupPrompt,
                                PreparedInputKind::ShellCommand
                                | PreparedInputKind::TerminalText
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
            launch_extra_args,
            resumed_provider_session,
        } = spawn;
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
            TransportKind::Pty => match PtySession::spawn_agent_with_size(
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
                    let session_id = session.session_id().to_owned();
                    let provider = match spec.capabilities.transports.pty_adapter.as_ref() {
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
                                    let mut pending_events =
                                        VecDeque::from([ProviderEvent::SessionStarted {
                                            session_id,
                                            model: String::new(),
                                            tools: Vec::new(),
                                        }]);
                                    if let Some(identity) = resumed_provider_session {
                                        pending_events.push_back(
                                            ProviderEvent::SessionIdentityObserved { identity },
                                        );
                                    }
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
                            provider,
                        },
                    );
                    ControlObservation::Spawned { process_id }
                }
                Err(error) => ControlObservation::SpawnFailed {
                    message: error.to_string(),
                },
            },
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
                    return match NativeOneShotSession::spawn(
                        &spec,
                        binding,
                        &prompt,
                        request.session_options.as_ref(),
                        &working_dir,
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
        request: ResumeLaunchRequest,
        pty_env: Vec<EnvMutation>,
    ) -> ControlObservation {
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
                launch_extra_args: if transport == TransportKind::Pty {
                    plan.args.into_iter().map(OsString::from).collect()
                } else {
                    Vec::new()
                },
                resumed_provider_session: Some(provider_session),
            },
            pty_env,
        )
        .await
    }

    async fn stop_native(&mut self, key: NativeSessionKey, force: bool) -> ControlObservation {
        if let Some(owned) = self.pty_sessions.remove(&key) {
            return match owned.session.shutdown().await {
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
            let owned = self
                .pty_sessions
                .remove(&key)
                .expect("completed key came from the owned session map");
            let (exit_code, final_terminal) = match owned.session.shutdown().await {
                Ok(outcome) => (outcome.exit_code, Some(terminal_frame(outcome.terminal))),
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
        let mut observations = Vec::new();
        for (key, owned) in &mut self.pty_sessions {
            if let Some(provider) = &mut owned.provider {
                drain_pty_provider(*key, provider, &mut observations);
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
                if let Some(info) = provider.rate_limits.detect(&raw) {
                    push_provider_observation(key, provider, rate_limit_event(info), observations);
                }
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
            PtyEvent::DataGap {
                from_sequence,
                to_sequence,
                ..
            } => {
                provider.utf8.clear();
                provider
                    .pipeline
                    .get_mut()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clear();
                observations.push(ObservationEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: None,
                    instance_id: key.instance_id,
                    generation: key.generation,
                    observation: ControlObservation::ProviderGap {
                        source: provider.source.clone(),
                        missed: to_sequence.saturating_sub(from_sequence).saturating_add(1),
                    },
                });
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
                observations.push(ObservationEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: None,
                    instance_id: key.instance_id,
                    generation: key.generation,
                    observation: ControlObservation::ProviderGap {
                        source: source.clone(),
                        missed,
                    },
                });
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
    let kimi_pipe = builtin_adapter_registry()
        .binding(AdapterFamily::Pipe, "kimi")
        .expect("missing built-in Pipe adapter kimi")
        .clone();
    runtimes
        .insert(AdapterFamily::Pipe, kimi_pipe, CliTool::KimiCode)
        .expect("built-in Kimi Pipe adapter runtime must be unique");
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
    let attachment = session
        .attach_retained_events()
        .map_err(|error| error.to_string())?;
    let mut receiver = attachment.receiver;
    let mut tracker = ReadinessTracker::new(spec, RuntimePlatform::current(), intent);
    let mut diagnostics = ReadinessDiagnostics {
        draft_signal: Some(spec.readiness.draft_signal),
        ..ReadinessDiagnostics::default()
    };
    seed_readiness_from_terminal(
        session,
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
                    let attachment = session
                        .attach_retained_events()
                        .map_err(|error| error.to_string())?;
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
                        session,
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

fn seed_readiness_from_terminal(
    session: &PtySession,
    tracker: &mut ReadinessTracker<'_>,
    diagnostics: &mut ReadinessDiagnostics,
    elapsed_ms: u64,
) -> Result<(), String> {
    const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
    let terminal = session.terminal_state().map_err(|error| error.to_string())?;
    if terminal.bracketed_paste {
        diagnostics.observe_output(ENABLE_BRACKETED_PASTE);
        tracker.observe_output(ENABLE_BRACKETED_PASTE, elapsed_ms);
    }
    let contents = terminal.contents.as_bytes();
    if !contents.is_empty() {
        diagnostics.observe_output(contents);
        tracker.observe_output(contents, elapsed_ms);
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
            return Err(format!(
                "agent '{}' initial prompt paste was not rendered before submit; Enter was not sent",
                spec.id
            ));
        }
        tokio::time::sleep(Duration::from_millis(STARTUP_GATE_POLL_MS)).await;
    }
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
            "draft_signal={:?} output_bytes={} output_chunks={} named_foreground={} bracketed_paste={} cursor_show={} cursor_hide={} alternate_screen={} clear_screen={} claude_composer={} codex_composer={}",
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
        )
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
        prompt_render_probe, prompt_rendered, startup_operator_gate, ReadinessDiagnostics,
        Utf8ChunkDecoder,
    };

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
        }
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
}
