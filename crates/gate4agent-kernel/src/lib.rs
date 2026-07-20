//! Synchronous host kernel for gate4agent engines.

use gate4agent_catalog::{
    builtin_registry, resolve_capability_probe_for, resolve_one_shot_plan,
    resolve_session_option_launch_for, AgentRegistry,
};
use gate4agent_engine::Gate4AgentEngine;
use gate4agent_tool_engine::{ToolEngine, ToolEngineError};
use gate4agent_tool_protocol::{
    CapabilityCompletionBatch, CapabilityEffectEnvelope, CapabilityObservationEnvelope,
    CapabilityProviderDescriptor, CapabilityRequestKey, ConsumerBoundCapabilityRequest,
    PolicyDecision, ToolAuthorityEnvelope, ToolAuthorityOutcome, ToolEngineSnapshot,
    ToolInstanceState,
};
use gate4agent_types::{
    AdapterBinding, AdapterFamily, AgentId, AgentInstanceId, CommandEnvelope, CommandId,
    ControlCommand, ControlError, ControlEvent, ControlHealth, ControlSnapshot, EffectEnvelope,
    InputAction, ObservationEnvelope, PipeProtocol, ProviderSource, SessionStatus, TransportKind,
    CONTROL_PROTOCOL_VERSION,
};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendIngress {
    Control(CommandEnvelope),
    ToolRequest(ConsumerBoundCapabilityRequest),
    ToolAuthority(ToolAuthorityEnvelope),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequestOutcome {
    pub request_key: CapabilityRequestKey,
    pub accepted_sequence: Option<u64>,
    pub result: Result<PolicyDecision, KernelToolError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolAuthorityCommandOutcome {
    pub sequence: u64,
    pub result: Result<ToolAuthorityOutcome, KernelToolError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendIngressOutcome {
    Control(CommandOutcome),
    ToolRequest(ToolRequestOutcome),
    ToolAuthority(ToolAuthorityCommandOutcome),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolObservationOutcome {
    pub operation_id: gate4agent_tool_protocol::ToolOperationId,
    pub request_key: CapabilityRequestKey,
    pub result: Result<(), KernelToolError>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum KernelToolError {
    #[error(transparent)]
    Engine(#[from] ToolEngineError),
    #[error("tool lane is blocked by a control/tool integration failure")]
    IntegrationBlocked,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum KernelIntegrationError {
    #[error("kernel logical tick exhausted at {current_tick}")]
    LogicalTickExhausted { current_tick: u64 },
    #[error("kernel snapshot revision exhausted at {current_revision}")]
    BackendRevisionExhausted { current_revision: u64 },
    #[error("control engine entered terminal counter exhaustion: {health:?}")]
    ControlHealthExhausted { health: ControlHealth },
    #[error("control observation for {instance_id:?} failed: {source}")]
    ControlObservation {
        instance_id: AgentInstanceId,
        #[source]
        source: ControlError,
    },
    #[error("tool clock advance failed: {source}")]
    ToolClock {
        #[source]
        source: ToolEngineError,
    },
    #[error("tool instance sync failed for {instance_id:?}: {source}")]
    ToolInstanceSync {
        instance_id: AgentInstanceId,
        #[source]
        source: ToolEngineError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendSnapshot {
    pub revision: u64,
    pub logical_tick: u64,
    pub control: Arc<ControlSnapshot>,
    pub tools: Arc<ToolEngineSnapshot>,
}

impl Default for BackendSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            logical_tick: 0,
            control: Arc::new(ControlSnapshot {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                revision: 0,
                health: ControlHealth::default(),
                sessions: Vec::new(),
            }),
            tools: Arc::new(ToolEngine::new().snapshot()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutcome {
    pub command_id: CommandId,
    pub result: Result<(), KernelCommandError>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum KernelCommandError {
    #[error("kernel control plane is blocked: {reason}")]
    IntegrationBlocked { reason: KernelIntegrationError },
    #[error("agent '{agent_id}' is not present in the kernel catalog")]
    UnknownAgent { agent_id: AgentId },
    #[error("agent '{agent_id}' does not declare capability '{capability}'")]
    UnsupportedCapability {
        agent_id: AgentId,
        capability: &'static str,
    },
    #[error("agent '{agent_id}' does not support transport {transport:?}")]
    UnsupportedTransport {
        agent_id: AgentId,
        transport: TransportKind,
    },
    #[error(
        "agent '{agent_id}' does not declare {family:?} provider source '{adapter_id}' at revision '{revision}'"
    )]
    InvalidProviderSource {
        agent_id: AgentId,
        family: AdapterFamily,
        adapter_id: String,
        revision: String,
    },
    #[error("agent '{agent_id}' session options are invalid: {message}")]
    InvalidSessionOptions { agent_id: AgentId, message: String },
    #[error("agent '{agent_id}' capability probe is invalid: {message}")]
    InvalidCapabilityProbe { agent_id: AgentId, message: String },
    #[error(transparent)]
    Control(#[from] ControlError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelStep {
    pub command_outcomes: Vec<CommandOutcome>,
    pub effects: Vec<EffectEnvelope>,
    pub snapshot: ControlSnapshot,
    pub events: Vec<ControlEvent>,
    pub ingress_outcomes: Vec<BackendIngressOutcome>,
    pub tool_observation_outcomes: Vec<ToolObservationOutcome>,
    pub tool_effects: Vec<CapabilityEffectEnvelope>,
    pub tool_completions: CapabilityCompletionBatch,
    pub backend_snapshot: BackendSnapshot,
    pub integration_errors: Vec<KernelIntegrationError>,
}

/// Owns the provider catalog and the single session-state writer.
///
/// Phase order is fixed: clock advance, ordered ingress, control observations,
/// tool observations, effect/completion drains, atomic snapshot, then ordered
/// event drain. No phase awaits or performs external work.
#[derive(Clone)]
pub struct Gate4AgentKernel {
    catalog: AgentRegistry,
    engine: Gate4AgentEngine,
    tool_engine: ToolEngine,
    logical_tick: u64,
    backend_revision: u64,
}

impl fmt::Debug for Gate4AgentKernel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Gate4AgentKernel")
            .field("catalog", &self.catalog)
            .field("engine", &self.engine)
            .field("tools", &self.tool_engine.snapshot())
            .field("logical_tick", &self.logical_tick)
            .field("backend_revision", &self.backend_revision)
            .finish()
    }
}

impl Gate4AgentKernel {
    pub fn new(catalog: AgentRegistry) -> Self {
        Self {
            catalog,
            engine: Gate4AgentEngine::new(),
            tool_engine: ToolEngine::new(),
            logical_tick: 0,
            backend_revision: 0,
        }
    }

    pub fn with_tool_providers(
        catalog: AgentRegistry,
        providers: impl IntoIterator<Item = CapabilityProviderDescriptor>,
    ) -> Result<Self, ToolEngineError> {
        let mut kernel = Self::new(catalog);
        for provider in providers {
            kernel.tool_engine.register_provider(provider)?;
        }
        Ok(kernel)
    }

    pub fn with_builtin_catalog() -> Self {
        Self::new(builtin_registry().clone())
    }

    pub fn step(
        &mut self,
        commands: impl IntoIterator<Item = CommandEnvelope>,
        observations: impl IntoIterator<Item = ObservationEnvelope>,
    ) -> KernelStep {
        self.step_control_plane(
            commands.into_iter().map(BackendIngress::Control),
            observations,
            std::iter::empty(),
        )
    }

    /// Reduces one backend tick without performing external work.
    ///
    /// The phase order is fixed: advance the kernel-owned logical clock;
    /// reduce ordered control/tool ingress; reduce control observations while
    /// synchronizing the affected tool instance after every observation;
    /// reduce tool observations; then drain effects, completions, snapshots,
    /// and events. A tool integration failure blocks tool ingress and effect
    /// release for the rest of the tick.
    pub fn step_control_plane(
        &mut self,
        ingress: impl IntoIterator<Item = BackendIngress>,
        control_observations: impl IntoIterator<Item = ObservationEnvelope>,
        tool_observations: impl IntoIterator<Item = CapabilityObservationEnvelope>,
    ) -> KernelStep {
        let ingress = ingress.into_iter().collect::<Vec<_>>();
        let control_observations = control_observations.into_iter().collect::<Vec<_>>();
        let tool_observations = tool_observations.into_iter().collect::<Vec<_>>();

        if let Err(error) = self.advance_backend_clock() {
            return self.blocked_step(ingress, tool_observations, error);
        }

        let mut command_outcomes = Vec::new();
        let mut ingress_outcomes = Vec::new();
        let mut tool_observation_outcomes = Vec::new();
        let mut integration_errors = Vec::new();
        let mut control_lane_open = true;
        let mut tool_lane_open = true;
        self.block_on_control_health(
            &mut control_lane_open,
            &mut tool_lane_open,
            &mut integration_errors,
        );
        if control_lane_open {
            self.reconcile_or_block(&mut tool_lane_open, &mut integration_errors);
        }

        for item in ingress {
            match item {
                BackendIngress::Control(command) => {
                    let command_id = command.id;
                    let instance_id = command.command.instance_id();
                    let attempted = control_lane_open;
                    let result = if attempted {
                        self.apply_validated_command(command)
                    } else {
                        Err(KernelCommandError::IntegrationBlocked {
                            reason: self
                                .control_health_error()
                                .expect("closed control lane has terminal health"),
                        })
                    };
                    if attempted {
                        if let Err(error) = &result {
                            self.engine.record_command_rejection(
                                command_id,
                                instance_id,
                                error.to_string(),
                            );
                        }
                    }
                    let outcome = CommandOutcome { command_id, result };
                    command_outcomes.push(outcome.clone());
                    ingress_outcomes.push(BackendIngressOutcome::Control(outcome));
                    if attempted {
                        self.sync_or_block(
                            instance_id,
                            &mut tool_lane_open,
                            &mut integration_errors,
                        );
                        self.block_on_control_health(
                            &mut control_lane_open,
                            &mut tool_lane_open,
                            &mut integration_errors,
                        );
                    }
                }
                BackendIngress::ToolRequest(request) => {
                    let request_key = request.key();
                    let result = if tool_lane_open {
                        self.tool_engine.request(request).map_err(Into::into)
                    } else {
                        Err(KernelToolError::IntegrationBlocked)
                    };
                    let accepted_sequence = result.as_ref().ok().and_then(|_| {
                        self.tool_engine
                            .request_snapshot(&request_key)
                            .map(|snapshot| snapshot.accepted_sequence)
                    });
                    ingress_outcomes.push(BackendIngressOutcome::ToolRequest(ToolRequestOutcome {
                        request_key,
                        accepted_sequence,
                        result,
                    }));
                }
                BackendIngress::ToolAuthority(authority) => {
                    let sequence = authority.sequence;
                    let result = if tool_lane_open {
                        self.tool_engine
                            .apply_authority(authority)
                            .map_err(Into::into)
                    } else {
                        Err(KernelToolError::IntegrationBlocked)
                    };
                    ingress_outcomes.push(BackendIngressOutcome::ToolAuthority(
                        ToolAuthorityCommandOutcome { sequence, result },
                    ));
                }
            }
        }

        for observation in control_observations {
            let instance_id = observation.instance_id;
            match self.engine.try_apply_observation(observation) {
                Ok(()) => {
                    self.sync_or_block(instance_id, &mut tool_lane_open, &mut integration_errors)
                }
                Err(source) => {
                    control_lane_open = false;
                    tool_lane_open = false;
                    integration_errors.push(KernelIntegrationError::ControlObservation {
                        instance_id,
                        source,
                    });
                }
            }
            self.block_on_control_health(
                &mut control_lane_open,
                &mut tool_lane_open,
                &mut integration_errors,
            );
        }

        for observation in tool_observations {
            let operation_id = observation.operation_id;
            let request_key = observation.request_key.clone();
            let result = if tool_lane_open {
                self.tool_engine
                    .apply_observation(observation)
                    .map_err(Into::into)
            } else {
                Err(KernelToolError::IntegrationBlocked)
            };
            tool_observation_outcomes.push(ToolObservationOutcome {
                operation_id,
                request_key,
                result,
            });
        }

        let effects = self.engine.drain_effects();
        let tool_effects = if tool_lane_open {
            self.tool_engine.drain_effects()
        } else {
            Vec::new()
        };
        let tool_completions = self.tool_engine.drain_completions();
        let backend_snapshot = self.backend_snapshot();
        let snapshot = (*backend_snapshot.control).clone();
        let events = self.engine.drain_events();

        KernelStep {
            command_outcomes,
            effects,
            snapshot,
            events,
            ingress_outcomes,
            tool_observation_outcomes,
            tool_effects,
            tool_completions,
            backend_snapshot,
            integration_errors,
        }
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        self.engine.snapshot()
    }

    pub fn tool_snapshot(&self) -> ToolEngineSnapshot {
        self.tool_engine.snapshot()
    }

    pub fn backend_snapshot(&self) -> BackendSnapshot {
        BackendSnapshot {
            revision: self.backend_revision,
            logical_tick: self.logical_tick,
            control: Arc::new(self.engine.snapshot()),
            tools: Arc::new(self.tool_engine.snapshot()),
        }
    }

    pub fn catalog(&self) -> &AgentRegistry {
        &self.catalog
    }

    fn advance_backend_clock(&mut self) -> Result<(), KernelIntegrationError> {
        let next_tick = self.logical_tick.checked_add(1).ok_or(
            KernelIntegrationError::LogicalTickExhausted {
                current_tick: self.logical_tick,
            },
        )?;
        let next_revision = self.backend_revision.checked_add(1).ok_or(
            KernelIntegrationError::BackendRevisionExhausted {
                current_revision: self.backend_revision,
            },
        )?;
        self.tool_engine
            .advance_time(next_tick)
            .map_err(|source| KernelIntegrationError::ToolClock { source })?;
        self.logical_tick = next_tick;
        self.backend_revision = next_revision;
        Ok(())
    }

    fn sync_or_block(
        &mut self,
        instance_id: AgentInstanceId,
        tool_lane_open: &mut bool,
        integration_errors: &mut Vec<KernelIntegrationError>,
    ) {
        if let Err(error) = self.sync_control_instance(instance_id) {
            *tool_lane_open = false;
            integration_errors.push(error);
        }
    }

    fn control_health_error(&self) -> Option<KernelIntegrationError> {
        terminal_control_health_error(self.engine.health())
    }

    fn block_on_control_health(
        &self,
        control_lane_open: &mut bool,
        tool_lane_open: &mut bool,
        integration_errors: &mut Vec<KernelIntegrationError>,
    ) {
        block_lanes_on_control_health(
            self.engine.health(),
            control_lane_open,
            tool_lane_open,
            integration_errors,
        );
    }

    fn reconcile_or_block(
        &mut self,
        tool_lane_open: &mut bool,
        integration_errors: &mut Vec<KernelIntegrationError>,
    ) {
        let control_instance_ids = self.engine.session_instance_ids().collect::<BTreeSet<_>>();
        for instance_id in &control_instance_ids {
            self.sync_or_block(*instance_id, tool_lane_open, integration_errors);
        }
        let tool_instance_ids = self.tool_engine.instance_ids().collect::<Vec<_>>();
        for instance_id in tool_instance_ids {
            if !control_instance_ids.contains(&instance_id) {
                self.sync_or_block(instance_id, tool_lane_open, integration_errors);
            }
        }
    }

    fn sync_control_instance(
        &mut self,
        instance_id: AgentInstanceId,
    ) -> Result<(), KernelIntegrationError> {
        let Some(session) = self.engine.session_snapshot(instance_id).cloned() else {
            self.tool_engine
                .remove_instance(instance_id)
                .map_err(|source| KernelIntegrationError::ToolInstanceSync {
                    instance_id,
                    source,
                })?;
            return Ok(());
        };

        self.tool_engine
            .set_generation(instance_id, session.generation)
            .map_err(|source| KernelIntegrationError::ToolInstanceSync {
                instance_id,
                source,
            })?;
        let state = if session.status == SessionStatus::Running {
            ToolInstanceState::Active
        } else {
            ToolInstanceState::Inactive
        };
        self.tool_engine
            .set_instance_state(instance_id, session.generation, state)
            .map_err(|source| KernelIntegrationError::ToolInstanceSync {
                instance_id,
                source,
            })
    }

    fn blocked_step(
        &self,
        ingress: Vec<BackendIngress>,
        tool_observations: Vec<CapabilityObservationEnvelope>,
        reason: KernelIntegrationError,
    ) -> KernelStep {
        let mut command_outcomes = Vec::new();
        let mut ingress_outcomes = Vec::new();
        for item in ingress {
            match item {
                BackendIngress::Control(command) => {
                    let outcome = CommandOutcome {
                        command_id: command.id,
                        result: Err(KernelCommandError::IntegrationBlocked {
                            reason: reason.clone(),
                        }),
                    };
                    command_outcomes.push(outcome.clone());
                    ingress_outcomes.push(BackendIngressOutcome::Control(outcome));
                }
                BackendIngress::ToolRequest(request) => {
                    ingress_outcomes.push(BackendIngressOutcome::ToolRequest(ToolRequestOutcome {
                        request_key: request.key(),
                        accepted_sequence: None,
                        result: Err(KernelToolError::IntegrationBlocked),
                    }));
                }
                BackendIngress::ToolAuthority(authority) => {
                    ingress_outcomes.push(BackendIngressOutcome::ToolAuthority(
                        ToolAuthorityCommandOutcome {
                            sequence: authority.sequence,
                            result: Err(KernelToolError::IntegrationBlocked),
                        },
                    ));
                }
            }
        }
        let tool_observation_outcomes = tool_observations
            .into_iter()
            .map(|observation| ToolObservationOutcome {
                operation_id: observation.operation_id,
                request_key: observation.request_key,
                result: Err(KernelToolError::IntegrationBlocked),
            })
            .collect();
        let backend_snapshot = self.backend_snapshot();
        let snapshot = (*backend_snapshot.control).clone();
        let tool_completions = CapabilityCompletionBatch {
            completions: Vec::new(),
            dropped_since_last_drain: 0,
            total_dropped: backend_snapshot.tools.dropped_completions,
            next_sequence: backend_snapshot.tools.next_completion_sequence,
            sequence_exhausted: backend_snapshot.tools.completion_sequence_exhausted,
        };

        KernelStep {
            command_outcomes,
            effects: Vec::new(),
            snapshot,
            events: Vec::new(),
            ingress_outcomes,
            tool_observation_outcomes,
            tool_effects: Vec::new(),
            tool_completions,
            backend_snapshot,
            integration_errors: vec![reason],
        }
    }

    fn apply_validated_command(
        &mut self,
        mut command: CommandEnvelope,
    ) -> Result<(), KernelCommandError> {
        if command.protocol_version != CONTROL_PROTOCOL_VERSION {
            return Err(ControlError::UnsupportedProtocolVersion {
                expected: CONTROL_PROTOCOL_VERSION,
                actual: command.protocol_version,
            }
            .into());
        }
        if let ControlCommand::Register {
            agent_id,
            transport,
            ..
        } = &command.command
        {
            let Some(spec) = self.catalog.get(agent_id) else {
                return Err(KernelCommandError::UnknownAgent {
                    agent_id: agent_id.clone(),
                });
            };
            let supported = match transport {
                TransportKind::Pty => spec.capabilities.transports.pty,
                TransportKind::Pipe => spec.capabilities.transports.pipe.is_some(),
                TransportKind::Acp => spec.capabilities.transports.acp.is_some(),
            };
            if !supported {
                return Err(KernelCommandError::UnsupportedTransport {
                    agent_id: agent_id.clone(),
                    transport: *transport,
                });
            }
        }
        if let ControlCommand::SendInput {
            instance_id,
            action: InputAction::AgentCommand(_),
        } = &command.command
        {
            if let Some(session) = self.engine.session_snapshot(*instance_id) {
                let supports_agent_commands = self
                    .catalog
                    .get(&session.agent_id)
                    .is_some_and(|spec| spec.capabilities.agent_commands.is_some());
                if !supports_agent_commands {
                    return Err(KernelCommandError::UnsupportedCapability {
                        agent_id: session.agent_id.clone(),
                        capability: "agent-commands",
                    });
                }
            }
        }
        if let ControlCommand::DiscoverHistory { instance_id, .. }
        | ControlCommand::LoadHistory { instance_id, .. } = &command.command
        {
            if let Some(session) = self.engine.session_snapshot(*instance_id) {
                let supports_history = self
                    .catalog
                    .get(&session.agent_id)
                    .is_some_and(|spec| spec.capabilities.adapters.history.is_some());
                if !supports_history {
                    return Err(KernelCommandError::UnsupportedCapability {
                        agent_id: session.agent_id.clone(),
                        capability: "history",
                    });
                }
            }
        }
        if let ControlCommand::ProbeCapabilities { instance_id, .. } = &command.command {
            if let Some(session) = self.engine.session_snapshot(*instance_id) {
                let spec = self
                    .catalog
                    .get(&session.agent_id)
                    .expect("registered agent must remain in kernel catalog");
                if spec.capabilities.adapters.capability_probe.is_none() {
                    return Err(KernelCommandError::UnsupportedCapability {
                        agent_id: session.agent_id.clone(),
                        capability: "capability-probe",
                    });
                }
                resolve_capability_probe_for(spec).map_err(|error| {
                    KernelCommandError::InvalidCapabilityProbe {
                        agent_id: session.agent_id.clone(),
                        message: error.to_string(),
                    }
                })?;
            }
        }
        if let ControlCommand::Resume { instance_id, .. } = &command.command {
            if let Some(session) = self.engine.session_snapshot(*instance_id) {
                let supports_resume = self
                    .catalog
                    .get(&session.agent_id)
                    .is_some_and(|spec| spec.capabilities.adapters.resume.is_some());
                if !supports_resume || session.transport != TransportKind::Pty {
                    return Err(KernelCommandError::UnsupportedCapability {
                        agent_id: session.agent_id.clone(),
                        capability: "pty-resume",
                    });
                }
            }
        }
        if let ControlCommand::Start {
            instance_id,
            request,
        } = &mut command.command
        {
            if let Some(session) = self.engine.session_snapshot(*instance_id) {
                let spec = self
                    .catalog
                    .get(&session.agent_id)
                    .expect("registered agent must remain in kernel catalog");
                let one_shot = spec
                    .capabilities
                    .transports
                    .pipe
                    .as_ref()
                    .filter(|transport| transport.protocol == PipeProtocol::OneShotText);
                if session.transport == TransportKind::Pipe && one_shot.is_some() {
                    let prompt = request.initial_prompt.as_deref().unwrap_or_default();
                    let binding = spec
                        .capabilities
                        .adapters
                        .one_shot
                        .as_ref()
                        .expect("validated one-shot transport binding");
                    let resolved = resolve_one_shot_plan(
                        &binding.id,
                        &spec.launch,
                        prompt,
                        request.session_options.as_ref(),
                    )
                    .map_err(|error| {
                        KernelCommandError::InvalidSessionOptions {
                            agent_id: session.agent_id.clone(),
                            message: error.to_string(),
                        }
                    })?;
                    request.session_options = Some(resolved.applied);
                } else if let Some(session_options) = &request.session_options {
                    if session.transport != TransportKind::Pty
                        || spec.capabilities.adapters.session_options.is_none()
                    {
                        return Err(KernelCommandError::UnsupportedCapability {
                            agent_id: session.agent_id.clone(),
                            capability: "pty-session-options",
                        });
                    }
                    let resolved = resolve_session_option_launch_for(spec, session_options, &[])
                        .map_err(|error| KernelCommandError::InvalidSessionOptions {
                            agent_id: session.agent_id.clone(),
                            message: error.to_string(),
                        })?;
                    request.session_options = resolved.applied;
                }
            }
        }
        if let ControlCommand::IngestProvider {
            instance_id,
            source,
            ..
        } = &command.command
        {
            if let Some(session) = self.engine.session_snapshot(*instance_id) {
                let spec = self
                    .catalog
                    .get(&session.agent_id)
                    .expect("registered agent must remain in kernel catalog");
                if declared_provider_binding(spec, source) != Some(&source.binding) {
                    return Err(KernelCommandError::InvalidProviderSource {
                        agent_id: session.agent_id.clone(),
                        family: source.family,
                        adapter_id: source.binding.id.to_string(),
                        revision: source.binding.revision.clone(),
                    });
                }
            }
        }
        self.engine.apply_command(command).map_err(Into::into)
    }
}

fn terminal_control_health_error(health: ControlHealth) -> Option<KernelIntegrationError> {
    (health.operation_id_exhausted
        || health.event_sequence_exhausted
        || health.revision_exhausted
        || health.provider_sequence_exhausted_sessions > 0)
        .then_some(KernelIntegrationError::ControlHealthExhausted { health })
}

fn block_lanes_on_control_health(
    health: ControlHealth,
    control_lane_open: &mut bool,
    tool_lane_open: &mut bool,
    integration_errors: &mut Vec<KernelIntegrationError>,
) {
    let Some(error) = terminal_control_health_error(health) else {
        return;
    };
    *control_lane_open = false;
    *tool_lane_open = false;
    if !integration_errors.contains(&error) {
        integration_errors.push(error);
    }
}

fn declared_provider_binding<'a>(
    spec: &'a gate4agent_catalog::AgentSpec,
    source: &ProviderSource,
) -> Option<&'a AdapterBinding> {
    match source.family {
        AdapterFamily::PtySemantic => spec.capabilities.transports.pty_adapter.as_ref(),
        AdapterFamily::Pipe => spec
            .capabilities
            .transports
            .pipe
            .as_ref()
            .map(|transport| &transport.adapter),
        AdapterFamily::Acp => spec
            .capabilities
            .transports
            .acp
            .as_ref()
            .map(|transport| &transport.adapter),
        AdapterFamily::OneShot => spec.capabilities.adapters.one_shot.as_ref(),
        AdapterFamily::Hook => spec.capabilities.adapters.hook.as_ref(),
        AdapterFamily::History
        | AdapterFamily::Resume
        | AdapterFamily::SessionOptions
        | AdapterFamily::CapabilityProbe
        | AdapterFamily::ManagedHook => None,
    }
}

impl Default for Gate4AgentKernel {
    fn default() -> Self {
        Self::with_builtin_catalog()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_tool_protocol::{
        CapabilityClass, CapabilityDescriptor, CapabilityOwner, CapabilityRequestId,
        CapabilityRequestInput, CapabilityTerminalOutcome, ConsumerId, GrantMode, PolicyDenial,
        PolicyGrant, PolicyKey, ResourceScopeId, ToolActorId, ToolAuthorityCommand,
        ToolCapabilityId, ToolProviderId, CAPABILITY_PROTOCOL_VERSION,
    };
    use gate4agent_types::{
        AgentInstanceId, CapabilityProbeRequest, ControlObservation, HistoryQuery,
        ObservationEnvelope, ProviderActivity, ProviderEvent, ProviderSource, ResumeLaunchRequest,
        ResumeTarget, SessionGeneration, SessionOptionSelection, SessionStatus, StartRequest,
        TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
    };

    fn instance() -> AgentInstanceId {
        AgentInstanceId(11)
    }

    fn command(id: u64, command: ControlCommand) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(id),
            command,
        }
    }

    fn register(id: u64, agent: &str) -> CommandEnvelope {
        command(
            id,
            ControlCommand::Register {
                instance_id: instance(),
                agent_id: AgentId::new(agent).unwrap(),
                transport: TransportKind::Pty,
            },
        )
    }

    fn tool_consumer() -> ConsumerId {
        ConsumerId::new("kernel-test-consumer").unwrap()
    }

    fn tool_actor() -> ToolActorId {
        ToolActorId::new("kernel-test-actor").unwrap()
    }

    fn tool_provider_id() -> ToolProviderId {
        ToolProviderId::new("kernel-browser-provider").unwrap()
    }

    fn tool_capability_id() -> ToolCapabilityId {
        ToolCapabilityId::new("browser.snapshot").unwrap()
    }

    fn tool_resource_scope() -> ResourceScopeId {
        ResourceScopeId::new("active-page").unwrap()
    }

    fn tool_provider() -> CapabilityProviderDescriptor {
        CapabilityProviderDescriptor {
            id: tool_provider_id(),
            owner: CapabilityOwner::Gate,
            capabilities: vec![CapabilityDescriptor::new(
                tool_capability_id(),
                CapabilityClass::Browser,
                "Return active page metadata",
            )
            .unwrap()],
        }
    }

    fn tool_request(
        local_id: u64,
        generation: SessionGeneration,
    ) -> ConsumerBoundCapabilityRequest {
        ConsumerBoundCapabilityRequest::new(
            tool_consumer(),
            tool_actor(),
            CapabilityRequestInput {
                local_id: CapabilityRequestId(local_id),
                instance_id: instance(),
                generation,
                provider_id: tool_provider_id(),
                capability_id: tool_capability_id(),
                resource_scope_id: tool_resource_scope(),
                approval_summary: "Read active page metadata".to_owned(),
                deadline_tick: 100,
                payload: br#"{"scope":"active-page"}"#.to_vec(),
            },
        )
    }

    fn tool_policy_grant(generation: SessionGeneration) -> PolicyGrant {
        PolicyGrant {
            key: PolicyKey {
                consumer_id: tool_consumer(),
                actor_id: tool_actor(),
                instance_id: instance(),
                generation,
                provider_id: tool_provider_id(),
                capability_id: tool_capability_id(),
                resource_scope_id: tool_resource_scope(),
            },
            mode: GrantMode::Allow,
        }
    }

    fn tool_grant(generation: SessionGeneration, sequence: u64) -> ToolAuthorityEnvelope {
        ToolAuthorityEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence,
            command: ToolAuthorityCommand::SetGrant {
                grant: tool_policy_grant(generation),
            },
        }
    }

    fn start_running(kernel: &mut Gate4AgentKernel) -> SessionGeneration {
        let starting = kernel.step(
            [
                register(1, "claude"),
                command(
                    2,
                    ControlCommand::Start {
                        instance_id: instance(),
                        request: StartRequest {
                            working_directory: ".".to_owned(),
                            terminal_size: TerminalSize {
                                rows: 24,
                                columns: 80,
                            },
                            initial_prompt: None,
                            session_options: None,
                        },
                    },
                ),
            ],
            [],
        );
        let spawn = starting.effects[0].clone();
        let running = kernel.step(
            [],
            [ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(spawn.operation_id),
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::Spawned {
                    process_id: Some(123),
                },
            }],
        );
        assert!(running.integration_errors.is_empty());
        assert_eq!(running.snapshot.sessions[0].status, SessionStatus::Running);
        running.snapshot.sessions[0].generation
    }

    #[test]
    fn unknown_provider_is_rejected_before_engine_mutation() {
        let mut kernel = Gate4AgentKernel::default();
        let step = kernel.step([register(1, "unknown-agent")], []);

        assert!(matches!(
            step.command_outcomes[0].result,
            Err(KernelCommandError::UnknownAgent { .. })
        ));
        assert!(step.snapshot.sessions.is_empty());
        assert!(step.effects.is_empty());
        assert!(matches!(
            step.events[0].event,
            gate4agent_types::ControlEventKind::CommandRejected { .. }
        ));
    }

    #[test]
    fn session_options_require_a_declared_pty_catalog_and_cross_the_effect_boundary() {
        let mut kernel = Gate4AgentKernel::default();
        kernel.step([register(1, "cursor")], []);
        let selection = SessionOptionSelection::new("gpt-5.3-codex")
            .with_value("effort", "high")
            .with_value("fastMode", true);
        let accepted = kernel.step(
            [command(
                2,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                        session_options: Some(selection.clone()),
                    },
                },
            )],
            [],
        );
        assert_eq!(accepted.command_outcomes[0].result, Ok(()));
        assert!(matches!(
            &accepted.effects[0].effect,
            gate4agent_types::ControlEffect::Spawn { request, .. }
                if request.session_options.as_ref() == Some(&selection)
        ));

        let mut unsupported = Gate4AgentKernel::default();
        unsupported.step([register(1, "opencode")], []);
        let rejected = unsupported.step(
            [command(
                2,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                        session_options: Some(SessionOptionSelection::new("opus")),
                    },
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedCapability {
                capability: "pty-session-options",
                ..
            })
        ));
        assert!(rejected.effects.is_empty());

        let mut expanded = Gate4AgentKernel::default();
        expanded.step([register(1, "claude")], []);
        let started = expanded.step(
            [command(
                2,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                        session_options: Some(SessionOptionSelection::new("opus")),
                    },
                },
            )],
            [],
        );
        let expected = SessionOptionSelection::new("opus").with_value("effort", "high");
        assert_eq!(
            started.snapshot.sessions[0].session_options.as_ref(),
            Some(&expected)
        );
        assert!(matches!(
            &started.effects[0].effect,
            gate4agent_types::ControlEffect::Spawn { request, .. }
                if request.session_options.as_ref() == Some(&expected)
        ));

        let mut pipe = Gate4AgentKernel::default();
        pipe.step(
            [command(
                1,
                ControlCommand::Register {
                    instance_id: instance(),
                    agent_id: AgentId::new("gemini").unwrap(),
                    transport: TransportKind::Pipe,
                },
            )],
            [],
        );
        let rejected = pipe.step(
            [command(
                2,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: Some("hello".to_owned()),
                        session_options: Some(SessionOptionSelection::new("gemini-3-pro-preview")),
                    },
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedCapability {
                capability: "pty-session-options",
                ..
            })
        ));
    }

    #[test]
    fn one_shot_pipe_defaults_and_validates_options_before_effect_creation() {
        let mut claude = Gate4AgentKernel::default();
        claude.step(
            [command(
                1,
                ControlCommand::Register {
                    instance_id: instance(),
                    agent_id: AgentId::new("claude").unwrap(),
                    transport: TransportKind::Pipe,
                },
            )],
            [],
        );
        let started = claude.step(
            [command(
                2,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: Some("summarize".to_owned()),
                        session_options: None,
                    },
                },
            )],
            [],
        );
        let expected = SessionOptionSelection::new("sonnet").with_value("thinking-level", "low");
        assert_eq!(
            started.snapshot.sessions[0].session_options.as_ref(),
            Some(&expected)
        );
        assert!(matches!(
            &started.effects[0].effect,
            gate4agent_types::ControlEffect::Spawn {
                transport: TransportKind::Pipe,
                request,
                ..
            } if request.session_options.as_ref() == Some(&expected)
        ));

        let mut amp = Gate4AgentKernel::default();
        amp.step(
            [command(
                3,
                ControlCommand::Register {
                    instance_id: instance(),
                    agent_id: AgentId::new("amp").unwrap(),
                    transport: TransportKind::Pipe,
                },
            )],
            [],
        );
        let rejected = amp.step(
            [command(
                4,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: Some("summarize".to_owned()),
                        session_options: Some(SessionOptionSelection::new("unknown-model")),
                    },
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::InvalidSessionOptions { .. })
        ));
        assert!(rejected.effects.is_empty());

        let mut missing_prompt = Gate4AgentKernel::default();
        missing_prompt.step(
            [command(
                5,
                ControlCommand::Register {
                    instance_id: instance(),
                    agent_id: AgentId::new("codex").unwrap(),
                    transport: TransportKind::Pipe,
                },
            )],
            [],
        );
        let rejected = missing_prompt.step(
            [command(
                6,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                        session_options: None,
                    },
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::InvalidSessionOptions { .. })
        ));
        assert!(rejected.effects.is_empty());
    }

    #[test]
    fn capability_probe_requires_the_declared_cursor_adapter_before_effect_creation() {
        let mut cursor = Gate4AgentKernel::default();
        cursor.step([register(1, "cursor")], []);
        let accepted = cursor.step(
            [command(
                2,
                ControlCommand::ProbeCapabilities {
                    instance_id: instance(),
                    request: CapabilityProbeRequest {
                        working_directory: ".".to_owned(),
                    },
                },
            )],
            [],
        );
        assert!(accepted.command_outcomes[0].result.is_ok());
        assert!(matches!(
            accepted.effects[0].effect,
            gate4agent_types::ControlEffect::ProbeCapabilities { ref agent_id, .. }
                if agent_id.as_str() == "cursor"
        ));

        let mut kimi = Gate4AgentKernel::default();
        kimi.step([register(3, "kimi")], []);
        let rejected = kimi.step(
            [command(
                4,
                ControlCommand::ProbeCapabilities {
                    instance_id: instance(),
                    request: CapabilityProbeRequest {
                        working_directory: ".".to_owned(),
                    },
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedCapability {
                capability: "capability-probe",
                ..
            })
        ));
        assert!(rejected.effects.is_empty());
    }

    #[test]
    fn external_provider_ingress_requires_the_declared_family_binding() {
        let mut kernel = Gate4AgentKernel::default();
        kernel.step([register(1, "grok")], []);
        let started = kernel.step(
            [command(
                2,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                        session_options: None,
                    },
                },
            )],
            [],
        );
        let generation = started.snapshot.sessions[0].generation;
        let grok_hook = kernel
            .catalog()
            .get_by_id("grok")
            .unwrap()
            .capabilities
            .adapters
            .hook
            .clone()
            .unwrap();
        let accepted = kernel.step(
            [command(
                3,
                ControlCommand::IngestProvider {
                    instance_id: instance(),
                    generation,
                    source: ProviderSource {
                        family: AdapterFamily::Hook,
                        binding: grok_hook,
                    },
                    source_sequence: 1,
                    events: vec![ProviderEvent::TurnStarted {
                        prompt: Some("ground hook".to_owned()),
                    }],
                },
            )],
            [],
        );
        assert_eq!(accepted.command_outcomes[0].result, Ok(()));
        assert_eq!(
            accepted.snapshot.sessions[0].provider.activity,
            ProviderActivity::Working
        );

        let kimi_hook = kernel
            .catalog()
            .get_by_id("kimi")
            .unwrap()
            .capabilities
            .adapters
            .hook
            .clone()
            .unwrap();
        let rejected = kernel.step(
            [command(
                4,
                ControlCommand::IngestProvider {
                    instance_id: instance(),
                    generation,
                    source: ProviderSource {
                        family: AdapterFamily::Hook,
                        binding: kimi_hook,
                    },
                    source_sequence: 2,
                    events: vec![ProviderEvent::Ready],
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::InvalidProviderSource { .. })
        ));
    }

    #[test]
    fn unsupported_provider_transport_is_rejected_before_registration() {
        let mut kernel = Gate4AgentKernel::default();
        let outcome = kernel.step(
            [command(
                1,
                ControlCommand::Register {
                    instance_id: instance(),
                    agent_id: AgentId::new("grok").unwrap(),
                    transport: TransportKind::Pipe,
                },
            )],
            [],
        );
        assert!(matches!(
            &outcome.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedTransport {
                transport: TransportKind::Pipe,
                ..
            })
        ));
        assert!(outcome.snapshot.sessions.is_empty());
    }

    #[test]
    fn history_commands_require_a_declared_adapter_before_effect_creation() {
        let mut supported = Gate4AgentKernel::default();
        supported.step([register(1, "grok")], []);
        let accepted = supported.step(
            [command(
                2,
                ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: None,
                        limit: 8,
                    },
                },
            )],
            [],
        );
        assert_eq!(accepted.command_outcomes[0].result, Ok(()));
        assert!(matches!(
            accepted.effects[0].effect,
            gate4agent_types::ControlEffect::DiscoverHistory { .. }
        ));

        let mut unsupported = Gate4AgentKernel::default();
        unsupported.step([register(1, "qwen-code")], []);
        let rejected = unsupported.step(
            [command(
                2,
                ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: None,
                        limit: 8,
                    },
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedCapability {
                capability: "history",
                ..
            })
        ));
        assert!(rejected.effects.is_empty());
    }

    #[test]
    fn resume_requires_a_declared_adapter_and_pty_before_engine_mutation() {
        let resume = || {
            command(
                2,
                ControlCommand::Resume {
                    instance_id: instance(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                    },
                },
            )
        };

        let mut unsupported = Gate4AgentKernel::default();
        unsupported.step([register(1, "kimi")], []);
        let rejected = unsupported.step([resume()], []);
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedCapability {
                capability: "pty-resume",
                ..
            })
        ));
        assert!(rejected.effects.is_empty());

        let mut wrong_transport = Gate4AgentKernel::default();
        wrong_transport.step(
            [command(
                1,
                ControlCommand::Register {
                    instance_id: instance(),
                    agent_id: AgentId::new("codex").unwrap(),
                    transport: TransportKind::Pipe,
                },
            )],
            [],
        );
        let rejected = wrong_transport.step([resume()], []);
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedCapability {
                capability: "pty-resume",
                ..
            })
        ));
        assert!(rejected.effects.is_empty());
    }

    #[test]
    fn undeclared_agent_commands_are_rejected_before_effect_creation() {
        let mut kernel = Gate4AgentKernel::default();
        let started = kernel.step(
            [
                register(1, "grok"),
                command(
                    2,
                    ControlCommand::Start {
                        instance_id: instance(),
                        request: StartRequest {
                            working_directory: ".".to_owned(),
                            terminal_size: TerminalSize {
                                rows: 24,
                                columns: 80,
                            },
                            initial_prompt: None,
                            session_options: None,
                        },
                    },
                ),
            ],
            [],
        );
        let spawn = started.effects[0].clone();
        kernel.step(
            [],
            [ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(spawn.operation_id),
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::Spawned {
                    process_id: Some(123),
                },
            }],
        );

        let rejected = kernel.step(
            [command(
                3,
                ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::AgentCommand(gate4agent_types::AgentCommand {
                        agent_id: AgentId::new("grok").unwrap(),
                        name: "help".to_owned(),
                        arguments: Vec::new(),
                    }),
                },
            )],
            [],
        );
        assert!(matches!(
            rejected.command_outcomes[0].result,
            Err(KernelCommandError::UnsupportedCapability {
                capability: "agent-commands",
                ..
            })
        ));
        assert!(rejected.effects.is_empty());
    }

    #[test]
    fn command_phase_precedes_observation_phase() {
        let mut kernel = Gate4AgentKernel::default();
        let first = kernel.step(
            [
                register(1, "claude"),
                command(
                    2,
                    ControlCommand::Start {
                        instance_id: instance(),
                        request: StartRequest {
                            working_directory: ".".to_owned(),
                            terminal_size: TerminalSize {
                                rows: 24,
                                columns: 80,
                            },
                            initial_prompt: None,
                            session_options: None,
                        },
                    },
                ),
            ],
            [],
        );
        let spawn = first.effects[0].clone();
        let running = kernel.step(
            [],
            [ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(spawn.operation_id),
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::Spawned {
                    process_id: Some(123),
                },
            }],
        );
        assert_eq!(running.snapshot.sessions[0].status, SessionStatus::Running);

        let raced = kernel.step(
            [command(
                3,
                ControlCommand::Stop {
                    instance_id: instance(),
                    force: false,
                },
            )],
            [ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: instance(),
                generation: spawn.generation,
                observation: ControlObservation::ProcessExited {
                    exit_code: Some(0),
                    final_terminal: None,
                },
            }],
        );

        assert!(raced.command_outcomes[0].result.is_ok());
        assert!(raced.effects.is_empty());
        assert_eq!(
            raced.snapshot.sessions[0].status,
            SessionStatus::Exited { exit_code: Some(0) }
        );
    }

    #[test]
    fn identical_batches_produce_identical_step() {
        fn run() -> KernelStep {
            let mut kernel = Gate4AgentKernel::default();
            kernel.step(
                [
                    register(1, "claude"),
                    command(
                        2,
                        ControlCommand::Start {
                            instance_id: instance(),
                            request: StartRequest {
                                working_directory: ".".to_owned(),
                                terminal_size: TerminalSize {
                                    rows: 24,
                                    columns: 80,
                                },
                                initial_prompt: None,
                                session_options: None,
                            },
                        },
                    ),
                ],
                [],
            )
        }

        assert_eq!(run(), run());
    }

    #[test]
    fn non_running_control_sessions_are_inactive_in_the_tool_engine() {
        let mut kernel =
            Gate4AgentKernel::with_tool_providers(builtin_registry().clone(), [tool_provider()])
                .unwrap();
        let registered = kernel.step([register(1, "claude")], []);
        assert_eq!(
            registered.backend_snapshot.tools.instance_states,
            vec![(instance(), ToolInstanceState::Inactive)]
        );

        let starting = kernel.step(
            [command(
                2,
                ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                        session_options: None,
                    },
                },
            )],
            [],
        );
        assert_eq!(
            starting.backend_snapshot.tools.instance_states,
            vec![(instance(), ToolInstanceState::Inactive)]
        );
    }

    #[test]
    fn tool_ingress_precedes_control_observation_activation() {
        let mut kernel =
            Gate4AgentKernel::with_tool_providers(builtin_registry().clone(), [tool_provider()])
                .unwrap();
        let starting = kernel.step(
            [
                register(1, "claude"),
                command(
                    2,
                    ControlCommand::Start {
                        instance_id: instance(),
                        request: StartRequest {
                            working_directory: ".".to_owned(),
                            terminal_size: TerminalSize {
                                rows: 24,
                                columns: 80,
                            },
                            initial_prompt: None,
                            session_options: None,
                        },
                    },
                ),
            ],
            [],
        );
        let spawn = starting.effects[0].clone();
        let step = kernel.step_control_plane(
            [BackendIngress::ToolRequest(tool_request(
                1,
                spawn.generation,
            ))],
            [ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(spawn.operation_id),
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::Spawned {
                    process_id: Some(123),
                },
            }],
            [],
        );

        let BackendIngressOutcome::ToolRequest(outcome) = &step.ingress_outcomes[0] else {
            panic!("expected tool request outcome");
        };
        assert_eq!(
            outcome.result,
            Ok(PolicyDecision::Deny(PolicyDenial::InactiveInstance))
        );
        assert!(outcome.accepted_sequence.is_some());
        assert_eq!(
            step.backend_snapshot.tools.instance_states,
            vec![(instance(), ToolInstanceState::Active)]
        );
        assert!(step.tool_effects.is_empty());
        assert!(matches!(
            step.tool_completions.completions[0].outcome,
            CapabilityTerminalOutcome::PolicyDenied {
                reason: PolicyDenial::InactiveInstance
            }
        ));
    }

    #[test]
    fn ordered_authority_and_request_outcomes_are_exactly_correlated() {
        let mut kernel =
            Gate4AgentKernel::with_tool_providers(builtin_registry().clone(), [tool_provider()])
                .unwrap();
        let generation = start_running(&mut kernel);
        let request = tool_request(7, generation);
        let request_key = request.key();
        let step = kernel.step_control_plane(
            [
                BackendIngress::ToolAuthority(tool_grant(generation, 1)),
                BackendIngress::ToolRequest(request),
            ],
            [],
            [],
        );

        assert!(matches!(
            &step.ingress_outcomes[0],
            BackendIngressOutcome::ToolAuthority(ToolAuthorityCommandOutcome {
                sequence: 1,
                result: Ok(ToolAuthorityOutcome::GrantSet),
            })
        ));
        let BackendIngressOutcome::ToolRequest(outcome) = &step.ingress_outcomes[1] else {
            panic!("expected tool request outcome");
        };
        assert_eq!(outcome.request_key, request_key);
        assert!(outcome.accepted_sequence.is_some());
        assert_eq!(outcome.result, Ok(PolicyDecision::Allow));
        assert_eq!(step.tool_effects.len(), 1);
        assert_eq!(step.tool_effects[0].request_key, request_key);

        let regressed = kernel.step_control_plane(
            [BackendIngress::ToolAuthority(tool_grant(generation, 1))],
            [],
            [],
        );
        assert!(matches!(
            &regressed.ingress_outcomes[0],
            BackendIngressOutcome::ToolAuthority(ToolAuthorityCommandOutcome {
                sequence: 1,
                result: Err(KernelToolError::Engine(
                    ToolEngineError::AuthoritySequenceRegressed {
                        current: 1,
                        requested: 1,
                    }
                )),
            })
        ));
    }

    #[test]
    fn stop_fences_queued_tool_work_then_remove_register_advances_generation() {
        let mut kernel =
            Gate4AgentKernel::with_tool_providers(builtin_registry().clone(), [tool_provider()])
                .unwrap();
        let generation = start_running(&mut kernel);
        let granted = kernel.step_control_plane(
            [BackendIngress::ToolAuthority(tool_grant(generation, 1))],
            [],
            [],
        );
        assert!(granted.integration_errors.is_empty());
        let request = tool_request(9, generation);
        let request_key = request.key();

        let stopped = kernel.step_control_plane(
            [
                BackendIngress::ToolRequest(request),
                BackendIngress::Control(command(
                    3,
                    ControlCommand::Stop {
                        instance_id: instance(),
                        force: false,
                    },
                )),
            ],
            [],
            [],
        );

        assert!(stopped.integration_errors.is_empty());
        assert!(stopped.tool_effects.is_empty());
        let completion = &stopped.tool_completions.completions[0];
        assert_eq!(completion.request_key, request_key);
        assert!(matches!(
            completion.outcome,
            CapabilityTerminalOutcome::InstanceClosed { .. }
        ));

        let exited = kernel.step(
            [],
            [ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: instance(),
                generation,
                observation: ControlObservation::ProcessExited {
                    exit_code: Some(0),
                    final_terminal: None,
                },
            }],
        );
        assert_eq!(
            exited.snapshot.sessions[0].status,
            SessionStatus::Exited { exit_code: Some(0) }
        );

        let step = kernel.step_control_plane(
            [
                BackendIngress::Control(command(
                    4,
                    ControlCommand::Remove {
                        instance_id: instance(),
                    },
                )),
                BackendIngress::Control(register(5, "claude")),
            ],
            [],
            [],
        );
        assert!(step.integration_errors.is_empty());
        assert_eq!(step.snapshot.sessions.len(), 1);
        assert_eq!(step.snapshot.sessions[0].generation, SessionGeneration(2));
        assert_eq!(
            step.backend_snapshot.tools.generations,
            vec![(instance(), SessionGeneration(2))]
        );
        assert_eq!(
            step.backend_snapshot.tools.instance_states,
            vec![(instance(), ToolInstanceState::Inactive)]
        );
    }

    #[test]
    fn kernel_without_bootstrap_providers_denies_tool_requests() {
        let mut kernel = Gate4AgentKernel::default();
        let generation = start_running(&mut kernel);
        let step = kernel.step_control_plane(
            [BackendIngress::ToolRequest(tool_request(1, generation))],
            [],
            [],
        );
        let BackendIngressOutcome::ToolRequest(outcome) = &step.ingress_outcomes[0] else {
            panic!("expected tool request outcome");
        };
        assert_eq!(
            outcome.result,
            Ok(PolicyDecision::Deny(PolicyDenial::UnknownProvider))
        );
        assert!(step.tool_effects.is_empty());
        assert!(step.backend_snapshot.tools.providers.is_empty());
    }

    #[test]
    fn reconciliation_failure_never_releases_retained_tool_effects() {
        let mut kernel =
            Gate4AgentKernel::with_tool_providers(builtin_registry().clone(), [tool_provider()])
                .unwrap();
        start_running(&mut kernel);
        let divergent_generation = SessionGeneration(99);
        kernel
            .tool_engine
            .set_generation(instance(), divergent_generation)
            .unwrap();
        kernel
            .tool_engine
            .set_instance_state(instance(), divergent_generation, ToolInstanceState::Active)
            .unwrap();
        kernel
            .tool_engine
            .apply_authority(tool_grant(divergent_generation, 1))
            .unwrap();
        assert_eq!(
            kernel
                .tool_engine
                .request(tool_request(99, divergent_generation))
                .unwrap(),
            PolicyDecision::Allow
        );

        let first = kernel.step_control_plane([], [], []);
        assert!(matches!(
            first.integration_errors[0],
            KernelIntegrationError::ToolInstanceSync {
                source: ToolEngineError::GenerationRegressed { .. },
                ..
            }
        ));
        assert!(first.tool_effects.is_empty());

        let second = kernel.step_control_plane([], [], []);
        assert!(matches!(
            second.integration_errors[0],
            KernelIntegrationError::ToolInstanceSync {
                source: ToolEngineError::GenerationRegressed { .. },
                ..
            }
        ));
        assert!(second.tool_effects.is_empty());
    }

    #[test]
    fn logical_tick_exhaustion_is_correlated_and_fail_closed() {
        let mut kernel = Gate4AgentKernel {
            logical_tick: u64::MAX,
            ..Gate4AgentKernel::default()
        };
        let step = kernel.step([register(1, "claude")], []);

        assert_eq!(
            step.integration_errors,
            vec![KernelIntegrationError::LogicalTickExhausted {
                current_tick: u64::MAX,
            }]
        );
        assert!(matches!(
            step.command_outcomes[0].result,
            Err(KernelCommandError::IntegrationBlocked {
                reason: KernelIntegrationError::LogicalTickExhausted { .. }
            })
        ));
        assert!(step.snapshot.sessions.is_empty());
        assert!(step.effects.is_empty());
        assert!(step.tool_effects.is_empty());
        assert_eq!(step.backend_snapshot.logical_tick, u64::MAX);
    }

    #[test]
    fn terminal_control_health_blocks_both_lanes_once() {
        let mut control_lane_open = true;
        let mut tool_lane_open = true;
        let mut errors = Vec::new();
        let healthy_capacity = ControlHealth {
            retained_instance_identities: 4_096,
            ..ControlHealth::default()
        };

        block_lanes_on_control_health(
            healthy_capacity,
            &mut control_lane_open,
            &mut tool_lane_open,
            &mut errors,
        );
        assert!(control_lane_open);
        assert!(tool_lane_open);
        assert!(errors.is_empty());

        let exhausted = ControlHealth {
            provider_sequence_exhausted_sessions: 1,
            ..healthy_capacity
        };
        block_lanes_on_control_health(
            exhausted,
            &mut control_lane_open,
            &mut tool_lane_open,
            &mut errors,
        );
        block_lanes_on_control_health(
            exhausted,
            &mut control_lane_open,
            &mut tool_lane_open,
            &mut errors,
        );

        assert!(!control_lane_open);
        assert!(!tool_lane_open);
        assert_eq!(
            errors,
            vec![KernelIntegrationError::ControlHealthExhausted { health: exhausted }]
        );
    }

    #[test]
    fn identical_unified_batches_produce_identical_steps() {
        fn run() -> KernelStep {
            let mut kernel = Gate4AgentKernel::with_tool_providers(
                builtin_registry().clone(),
                [tool_provider()],
            )
            .unwrap();
            let generation = start_running(&mut kernel);
            kernel.step_control_plane(
                [
                    BackendIngress::ToolAuthority(tool_grant(generation, 1)),
                    BackendIngress::ToolRequest(tool_request(1, generation)),
                ],
                [],
                [],
            )
        }

        assert_eq!(run(), run());
    }
}
