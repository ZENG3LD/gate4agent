//! Synchronous host kernel for gate4agent engines.

use gate4agent_catalog::{
    builtin_registry, resolve_capability_probe_for, resolve_session_option_launch_for,
    AgentRegistry,
};
use gate4agent_engine::Gate4AgentEngine;
use gate4agent_types::{
    AdapterBinding, AdapterFamily, AgentId, CommandEnvelope, CommandId, ControlCommand,
    ControlError, ControlEvent, ControlSnapshot, EffectEnvelope, InputAction, ObservationEnvelope,
    ProviderSource, TransportKind, CONTROL_PROTOCOL_VERSION,
};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutcome {
    pub command_id: CommandId,
    pub result: Result<(), KernelCommandError>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum KernelCommandError {
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
}

/// Owns the provider catalog and the single session-state writer.
///
/// Phase order is fixed: consumer commands, provider observations, effect
/// drain, full snapshot, then ordered event drain. No phase awaits or performs
/// external work.
#[derive(Clone, Debug)]
pub struct Gate4AgentKernel {
    catalog: AgentRegistry,
    engine: Gate4AgentEngine,
}

impl Gate4AgentKernel {
    pub fn new(catalog: AgentRegistry) -> Self {
        Self {
            catalog,
            engine: Gate4AgentEngine::new(),
        }
    }

    pub fn with_builtin_catalog() -> Self {
        Self::new(builtin_registry().clone())
    }

    pub fn step(
        &mut self,
        commands: impl IntoIterator<Item = CommandEnvelope>,
        observations: impl IntoIterator<Item = ObservationEnvelope>,
    ) -> KernelStep {
        let mut command_outcomes = Vec::new();
        for command in commands {
            let command_id = command.id;
            let instance_id = command.command.instance_id();
            let result = self.apply_validated_command(command);
            if let Err(error) = &result {
                self.engine
                    .record_command_rejection(command_id, instance_id, error.to_string());
            }
            command_outcomes.push(CommandOutcome { command_id, result });
        }

        for observation in observations {
            self.engine.apply_observation(observation);
        }

        KernelStep {
            command_outcomes,
            effects: self.engine.drain_effects(),
            snapshot: self.engine.snapshot(),
            events: self.engine.drain_events(),
        }
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        self.engine.snapshot()
    }

    pub fn catalog(&self) -> &AgentRegistry {
        &self.catalog
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
            if let Some(session_options) = &request.session_options {
                if let Some(session) = self.engine.session_snapshot(*instance_id) {
                    let spec = self
                        .catalog
                        .get(&session.agent_id)
                        .expect("registered agent must remain in kernel catalog");
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
    use gate4agent_types::{
        AgentInstanceId, CapabilityProbeRequest, ControlObservation, HistoryQuery,
        ObservationEnvelope, ProviderActivity, ProviderEvent, ProviderSource, ResumeLaunchRequest,
        ResumeTarget, SessionOptionSelection, SessionStatus, StartRequest, TerminalSize,
        TransportKind, CONTROL_PROTOCOL_VERSION,
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
}
