//! Synchronous host kernel for gate4agent engines.

use gate4agent_catalog::{builtin_registry, AgentRegistry};
use gate4agent_engine::Gate4AgentEngine;
use gate4agent_types::{
    AgentId, CommandEnvelope, CommandId, ControlCommand, ControlError, ControlEvent,
    ControlSnapshot, EffectEnvelope, InputAction, ObservationEnvelope, CONTROL_PROTOCOL_VERSION,
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
                self.engine.record_command_rejection(
                    command_id,
                    instance_id,
                    error.to_string(),
                );
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
        command: CommandEnvelope,
    ) -> Result<(), KernelCommandError> {
        if command.protocol_version != CONTROL_PROTOCOL_VERSION {
            return Err(ControlError::UnsupportedProtocolVersion {
                expected: CONTROL_PROTOCOL_VERSION,
                actual: command.protocol_version,
            }
            .into());
        }
        if let ControlCommand::Register { agent_id, .. } = &command.command {
            if self.catalog.get(agent_id).is_none() {
                return Err(KernelCommandError::UnknownAgent {
                    agent_id: agent_id.clone(),
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
        self.engine.apply_command(command).map_err(Into::into)
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
        AgentInstanceId, ControlObservation, ObservationEnvelope, SessionStatus,
        StartRequest, TerminalSize, TransportKind, CONTROL_PROTOCOL_VERSION,
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
