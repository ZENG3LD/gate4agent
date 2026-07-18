//! Deterministic single-writer lifecycle engine for gate4agent sessions.

use gate4agent_types::{
    prepare_agent_command, prepare_input, AgentInstanceId, CommandEnvelope, CommandId,
    ControlCommand, ControlEffect, ControlError, ControlEvent, ControlEventKind,
    ControlObservation, ControlSnapshot, EffectEnvelope, InputAction, ObservationEnvelope,
    ObservationIgnoredReason, OperationId, SessionGeneration, SessionSnapshot, SessionStatus,
    CONTROL_PROTOCOL_VERSION,
};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionState {
    snapshot: SessionSnapshot,
}

/// Owns logical session lifecycle state. External work is emitted as effects
/// and can change observed state only after a matching observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Gate4AgentEngine {
    sessions: BTreeMap<AgentInstanceId, SessionState>,
    next_operation_id: u64,
    next_event_sequence: u64,
    revision: u64,
    effects: Vec<EffectEnvelope>,
    events: Vec<ControlEvent>,
}

impl Gate4AgentEngine {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            next_operation_id: 1,
            next_event_sequence: 1,
            revision: 0,
            effects: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn apply_command(&mut self, envelope: CommandEnvelope) -> Result<(), ControlError> {
        if envelope.protocol_version != CONTROL_PROTOCOL_VERSION {
            return Err(ControlError::UnsupportedProtocolVersion {
                expected: CONTROL_PROTOCOL_VERSION,
                actual: envelope.protocol_version,
            });
        }
        let command_id = envelope.id;
        match envelope.command {
            ControlCommand::Register {
                instance_id,
                agent_id,
                transport,
            } => {
                if self.sessions.contains_key(&instance_id) {
                    return Err(ControlError::DuplicateInstance { instance_id });
                }
                let generation = SessionGeneration::default();
                self.sessions.insert(
                    instance_id,
                    SessionState {
                        snapshot: SessionSnapshot {
                            instance_id,
                            agent_id,
                            transport,
                            generation,
                            status: SessionStatus::Registered,
                            pending_operation: None,
                            pending_input: None,
                            process_id: None,
                        },
                    },
                );
                self.bump_revision();
                self.emit_event(
                    Some(command_id),
                    instance_id,
                    generation,
                    ControlEventKind::Registered,
                );
                Ok(())
            }
            ControlCommand::Start { instance_id } => self.start(command_id, instance_id),
            ControlCommand::Stop { instance_id, force } => {
                self.stop(command_id, instance_id, force)
            }
            ControlCommand::SendInput {
                instance_id,
                action,
            } => {
                self.send_input(command_id, instance_id, action)
            }
            ControlCommand::Remove { instance_id } => self.remove(command_id, instance_id),
        }
    }

    pub fn apply_observation(&mut self, envelope: ObservationEnvelope) {
        let instance_id = envelope.instance_id;
        let generation = envelope.generation;
        if envelope.protocol_version != CONTROL_PROTOCOL_VERSION {
            self.emit_ignored(
                instance_id,
                generation,
                ObservationIgnoredReason::UnsupportedProtocolVersion,
            );
            return;
        }
        let Some(current) = self.sessions.get(&instance_id).map(|state| &state.snapshot) else {
            self.emit_ignored(instance_id, generation, ObservationIgnoredReason::UnknownInstance);
            return;
        };

        if current.generation != generation {
            self.emit_ignored(instance_id, generation, ObservationIgnoredReason::StaleGeneration);
            return;
        }

        if envelope.observation.requires_operation_id() {
            let Some(operation_id) = envelope.operation_id else {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::MissingOperation,
                );
                return;
            };
            if current.pending_operation != Some(operation_id) {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::OperationMismatch,
                );
                return;
            }
        }

        let valid = match (&current.status, &envelope.observation) {
            (SessionStatus::Starting, ControlObservation::Spawned { .. })
            | (SessionStatus::Starting, ControlObservation::SpawnFailed { .. })
            | (SessionStatus::Stopping, ControlObservation::StopCompleted { .. })
            | (SessionStatus::Running, ControlObservation::InputCompleted)
            | (SessionStatus::Running, ControlObservation::InputFailed { .. }) => true,
            (
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping,
                ControlObservation::ProcessExited { .. },
            ) => true,
            _ => false,
        };
        if !valid {
            self.emit_ignored(instance_id, generation, ObservationIgnoredReason::InvalidState);
            return;
        }

        let pending_input = current.pending_input;
        let event = match envelope.observation {
            ControlObservation::Spawned { process_id } => {
                let session = self.session_mut(instance_id);
                session.status = SessionStatus::Running;
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = process_id;
                ControlEventKind::Running { process_id }
            }
            ControlObservation::SpawnFailed { message } => {
                let session = self.session_mut(instance_id);
                session.status = SessionStatus::Failed {
                    message: message.clone(),
                };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                ControlEventKind::Failed { message }
            }
            ControlObservation::ProcessExited { exit_code } => {
                let session = self.session_mut(instance_id);
                session.status = SessionStatus::Exited { exit_code };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                ControlEventKind::Exited {
                    exit_code,
                    forced: false,
                }
            }
            ControlObservation::StopCompleted { forced } => {
                let session = self.session_mut(instance_id);
                session.status = SessionStatus::Exited { exit_code: None };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                ControlEventKind::Exited {
                    exit_code: None,
                    forced,
                }
            }
            ControlObservation::InputCompleted => {
                let input_kind = pending_input
                    .expect("validated input completion must have a pending input kind");
                let session = self.session_mut(instance_id);
                session.pending_operation = None;
                session.pending_input = None;
                ControlEventKind::InputCompleted { input_kind }
            }
            ControlObservation::InputFailed { message } => {
                let input_kind = pending_input
                    .expect("validated input failure must have a pending input kind");
                let session = self.session_mut(instance_id);
                session.pending_operation = None;
                session.pending_input = None;
                ControlEventKind::InputFailed {
                    input_kind,
                    message,
                }
            }
        };
        self.bump_revision();
        self.emit_event(None, instance_id, generation, event);
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        ControlSnapshot {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            revision: self.revision,
            sessions: self
                .sessions
                .values()
                .map(|state| state.snapshot.clone())
                .collect(),
        }
    }

    pub fn drain_effects(&mut self) -> Vec<EffectEnvelope> {
        std::mem::take(&mut self.effects)
    }

    pub fn drain_events(&mut self) -> Vec<ControlEvent> {
        std::mem::take(&mut self.events)
    }

    fn start(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
    ) -> Result<(), ControlError> {
        let status = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?
            .snapshot
            .status
            .clone();
        if !matches!(
            status,
            SessionStatus::Registered | SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
        ) {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "start".to_owned(),
                status,
            });
        }

        let operation_id = self.allocate_operation();
        let (generation, agent_id, transport) = {
            let session = self.session_mut(instance_id);
            session.generation = SessionGeneration(session.generation.0.saturating_add(1));
            session.status = SessionStatus::Starting;
            session.pending_operation = Some(operation_id);
            session.pending_input = None;
            session.process_id = None;
            (
                session.generation,
                session.agent_id.clone(),
                session.transport,
            )
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::Spawn {
                agent_id,
                transport,
            },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::StartRequested { operation_id },
        );
        Ok(())
    }

    fn stop(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        force: bool,
    ) -> Result<(), ControlError> {
        let status = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?
            .snapshot
            .status
            .clone();
        if !matches!(status, SessionStatus::Starting | SessionStatus::Running) {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "stop".to_owned(),
                status,
            });
        }

        let operation_id = self.allocate_operation();
        let generation = {
            let session = self.session_mut(instance_id);
            session.status = SessionStatus::Stopping;
            session.pending_operation = Some(operation_id);
            session.pending_input = None;
            session.generation
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::Stop { force },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::StopRequested {
                operation_id,
                force,
            },
        );
        Ok(())
    }

    fn send_input(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        action: InputAction,
    ) -> Result<(), ControlError> {
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if state.snapshot.status != SessionStatus::Running {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "send input".to_owned(),
                status: state.snapshot.status.clone(),
            });
        }
        if let Some(operation_id) = state.snapshot.pending_operation {
            return Err(ControlError::OperationPending {
                instance_id,
                operation_id,
            });
        }

        let agent_id = state.snapshot.agent_id.clone();
        let input = match action {
            InputAction::AgentCommand(command) => prepare_agent_command(command, &agent_id),
            action => prepare_input(action),
        }
        .map_err(|error| ControlError::InputRejected { error })?;

        let operation_id = self.allocate_operation();
        let input_kind = input.kind();
        let generation = {
            let session = self.session_mut(instance_id);
            session.pending_operation = Some(operation_id);
            session.pending_input = Some(input_kind);
            session.generation
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::WriteInput { input },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::InputRequested {
                operation_id,
                input_kind,
            },
        );
        Ok(())
    }

    fn remove(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
    ) -> Result<(), ControlError> {
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if matches!(
            state.snapshot.status,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping
        ) {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "remove".to_owned(),
                status: state.snapshot.status.clone(),
            });
        }
        let generation = state.snapshot.generation;
        self.sessions.remove(&instance_id);
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::Removed,
        );
        Ok(())
    }

    fn allocate_operation(&mut self) -> OperationId {
        let id = OperationId(self.next_operation_id);
        self.next_operation_id = self.next_operation_id.saturating_add(1);
        id
    }

    fn session_mut(&mut self, instance_id: AgentInstanceId) -> &mut SessionSnapshot {
        &mut self
            .sessions
            .get_mut(&instance_id)
            .expect("validated instance must remain registered")
            .snapshot
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn emit_ignored(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        reason: ObservationIgnoredReason,
    ) {
        self.emit_event(
            None,
            instance_id,
            generation,
            ControlEventKind::ObservationIgnored { reason },
        );
    }

    fn emit_event(
        &mut self,
        command_id: Option<CommandId>,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        event: ControlEventKind,
    ) {
        let sequence = self.next_event_sequence;
        self.next_event_sequence = self.next_event_sequence.saturating_add(1);
        self.events.push(ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence,
            command_id,
            instance_id,
            generation,
            event,
        });
    }
}

impl Default for Gate4AgentEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_types::{
        AgentId, InputAction, PreparedInputKind, PromptFraming, PromptPayload, TransportKind,
    };

    fn instance() -> AgentInstanceId {
        AgentInstanceId(7)
    }

    fn register(command_id: u64) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(command_id),
            command: ControlCommand::Register {
                instance_id: instance(),
                agent_id: AgentId::new("claude").unwrap(),
                transport: TransportKind::Pty,
            },
        }
    }

    fn start(command_id: u64) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(command_id),
            command: ControlCommand::Start {
                instance_id: instance(),
            },
        }
    }

    fn running_engine() -> (Gate4AgentEngine, EffectEnvelope) {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine.drain_events();
        engine.apply_command(start(2)).unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::Spawned {
                process_id: Some(42),
            },
        });
        (engine, effect)
    }

    #[test]
    fn spawn_is_not_reported_running_before_observation() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine.apply_command(start(2)).unwrap();

        let snapshot = engine.snapshot();
        assert_eq!(snapshot.sessions[0].status, SessionStatus::Starting);
        assert_eq!(snapshot.sessions[0].process_id, None);
        assert_eq!(engine.drain_effects().len(), 1);
    }

    #[test]
    fn unsupported_command_protocol_is_rejected_without_state_change() {
        let mut engine = Gate4AgentEngine::new();
        let mut command = register(1);
        command.protocol_version = CONTROL_PROTOCOL_VERSION + 1;

        assert!(matches!(
            engine.apply_command(command),
            Err(ControlError::UnsupportedProtocolVersion { .. })
        ));
        assert_eq!(engine.snapshot(), ControlSnapshot::default());
        assert!(engine.drain_effects().is_empty());
        assert!(engine.drain_events().is_empty());
    }

    #[test]
    fn matching_spawn_observation_confirms_running() {
        let (engine, _) = running_engine();
        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(session.process_id, Some(42));
        assert_eq!(session.pending_operation, None);
    }

    #[test]
    fn stop_remains_pending_until_observed() {
        let (mut engine, _) = running_engine();
        engine.drain_events();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::Stop {
                    instance_id: instance(),
                    force: true,
                },
            })
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        assert_eq!(engine.snapshot().sessions[0].status, SessionStatus::Stopping);

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::StopCompleted { forced: true },
        });
        assert_eq!(
            engine.snapshot().sessions[0].status,
            SessionStatus::Exited { exit_code: None }
        );
    }

    #[test]
    fn typed_input_is_an_effect_until_executor_confirms_it() {
        let (mut engine, _) = running_engine();
        engine.drain_events();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(7),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::SubmitPrompt(PromptPayload {
                        text: "inspect the lifecycle".to_owned(),
                        framing: PromptFraming::BracketedPaste,
                    }),
                },
            })
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        assert!(matches!(effect.effect, ControlEffect::WriteInput { .. }));
        assert_eq!(
            engine.snapshot().sessions[0].pending_input,
            Some(PreparedInputKind::SubmitPrompt)
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::InputCompleted,
        });
        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(session.pending_operation, None);
        assert_eq!(session.pending_input, None);
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::InputCompleted {
                input_kind: PreparedInputKind::SubmitPrompt
            }
        )));
    }

    #[test]
    fn provider_command_cannot_target_another_running_agent() {
        let (mut engine, _) = running_engine();
        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(8),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::AgentCommand(gate4agent_types::AgentCommand {
                        agent_id: AgentId::new("kimi").unwrap(),
                        name: "help".to_owned(),
                        arguments: Vec::new(),
                    }),
                },
            })
            .unwrap_err();

        assert!(matches!(error, ControlError::InputRejected { .. }));
        assert!(engine.drain_effects().is_empty());
        assert_eq!(engine.snapshot().sessions[0].pending_operation, None);
    }

    #[test]
    fn stale_generation_cannot_mutate_restarted_session() {
        let (mut engine, first_spawn) = running_engine();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: first_spawn.generation,
            observation: ControlObservation::ProcessExited { exit_code: Some(0) },
        });
        engine.apply_command(start(4)).unwrap();
        let second_spawn = engine.drain_effects().pop().unwrap();
        assert!(second_spawn.generation.0 > first_spawn.generation.0);

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(first_spawn.operation_id),
            instance_id: instance(),
            generation: first_spawn.generation,
            observation: ControlObservation::Spawned {
                process_id: Some(99),
            },
        });
        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.generation, second_spawn.generation);
        assert_eq!(session.status, SessionStatus::Starting);
        assert_eq!(session.process_id, None);
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::StaleGeneration
            }
        )));
    }

    #[test]
    fn active_instance_cannot_be_removed_through_a_second_door() {
        let (mut engine, _) = running_engine();
        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(5),
                command: ControlCommand::Remove {
                    instance_id: instance(),
                },
            })
            .unwrap_err();
        assert!(matches!(error, ControlError::InvalidTransition { .. }));
        assert_eq!(engine.snapshot().sessions.len(), 1);
    }

    #[test]
    fn replay_is_deterministic() {
        fn replay() -> (ControlSnapshot, Vec<EffectEnvelope>, Vec<ControlEvent>) {
            let mut engine = Gate4AgentEngine::new();
            engine.apply_command(register(1)).unwrap();
            engine.apply_command(start(2)).unwrap();
            let effect = engine.drain_effects().pop().unwrap();
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(effect.operation_id),
                instance_id: effect.instance_id,
                generation: effect.generation,
                observation: ControlObservation::Spawned {
                    process_id: Some(42),
                },
            });
            (engine.snapshot(), vec![effect], engine.drain_events())
        }

        assert_eq!(replay(), replay());
    }
}
