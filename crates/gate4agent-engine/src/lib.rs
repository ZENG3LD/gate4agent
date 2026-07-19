//! Deterministic single-writer lifecycle engine for gate4agent sessions.

use gate4agent_types::{
    normalize_semantic_prompt, prepare_agent_command, prepare_input, prepare_shell_command,
    ActiveProviderTool, AgentInstanceId, CommandEnvelope, CommandId, ControlCommand, ControlEffect,
    ControlError, ControlEvent, ControlEventKind, ControlObservation, ControlSnapshot,
    EffectEnvelope, ForegroundAuthority, ForegroundRequirement, ForegroundSnapshot, InputAction,
    ObservationEnvelope, ObservationIgnoredReason, OperationId, PreparedInputKind,
    ProviderActivity, ProviderEvent, ProviderSnapshot, ProviderSource, ProviderSourceCursor,
    SessionGeneration, SessionSnapshot, SessionStatus, StartRequest, TerminalControl, TerminalSize,
    TokenUsage, TransportKind, CONTROL_PROTOCOL_VERSION, PROVIDER_INGRESS_EVENTS_MAX,
    WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionState {
    snapshot: SessionSnapshot,
    pending_terminal_size: Option<TerminalSize>,
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
                            terminal_size: None,
                            terminal_frame: None,
                            terminal_stale: None,
                            foreground: ForegroundSnapshot::default(),
                            provider: ProviderSnapshot::default(),
                        },
                        pending_terminal_size: None,
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
            ControlCommand::Start {
                instance_id,
                request,
            } => self.start(command_id, instance_id, request),
            ControlCommand::Stop { instance_id, force } => {
                self.stop(command_id, instance_id, force)
            }
            ControlCommand::SendInput {
                instance_id,
                action,
            } => self.send_input(command_id, instance_id, action),
            ControlCommand::Resize { instance_id, size } => {
                self.resize(command_id, instance_id, size)
            }
            ControlCommand::RefreshForeground { instance_id } => {
                self.refresh_foreground(command_id, instance_id)
            }
            ControlCommand::IngestProvider {
                instance_id,
                generation,
                source,
                source_sequence,
                events,
            } => self.ingest_provider(
                command_id,
                instance_id,
                generation,
                source,
                source_sequence,
                events,
            ),
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
            self.emit_ignored(
                instance_id,
                generation,
                ObservationIgnoredReason::UnknownInstance,
            );
            return;
        };

        if current.generation != generation {
            self.emit_ignored(
                instance_id,
                generation,
                ObservationIgnoredReason::StaleGeneration,
            );
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

        if let ControlObservation::ForegroundObserved { process } = &envelope.observation {
            if !process.is_valid_for(&current.agent_id)
                || current
                    .process_id
                    .is_some_and(|root_process_id| root_process_id != process.root_process_id)
            {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::InvalidForegroundObservation,
                );
                return;
            }
        }

        let valid = matches!(
            (&current.status, &envelope.observation),
            (SessionStatus::Starting, ControlObservation::Spawned { .. })
                | (
                    SessionStatus::Starting,
                    ControlObservation::SpawnFailed { .. }
                )
                | (
                    SessionStatus::Stopping,
                    ControlObservation::StopCompleted { .. }
                )
                | (
                    SessionStatus::Stopping,
                    ControlObservation::StopFailed { .. }
                )
                | (SessionStatus::Running, ControlObservation::InputCompleted)
                | (
                    SessionStatus::Running,
                    ControlObservation::InputFailed { .. }
                )
                | (
                    SessionStatus::Running,
                    ControlObservation::ResizeCompleted { .. }
                )
                | (
                    SessionStatus::Running,
                    ControlObservation::ResizeFailed { .. }
                )
                | (
                    SessionStatus::Running,
                    ControlObservation::ForegroundObserved { .. }
                        | ControlObservation::ForegroundFailed { .. },
                )
                | (
                    SessionStatus::Running | SessionStatus::Stopping,
                    ControlObservation::TerminalFrame { .. }
                        | ControlObservation::TerminalStale { .. },
                )
                | (
                    SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping,
                    ControlObservation::ProviderEvent { .. }
                        | ControlObservation::ProviderGap { .. },
                )
                | (
                    SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping,
                    ControlObservation::ProcessExited { .. },
                )
        );
        if !valid {
            self.emit_ignored(
                instance_id,
                generation,
                ObservationIgnoredReason::InvalidState,
            );
            return;
        }

        if let ControlObservation::TerminalFrame { frame } = &envelope.observation {
            if current
                .terminal_frame
                .as_ref()
                .is_some_and(|existing| existing.sequence >= frame.sequence)
            {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::StaleTerminalFrame,
                );
                return;
            }
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.snapshot.terminal_size = Some(frame.size);
            state.snapshot.terminal_frame = Some(frame.clone());
            state.snapshot.terminal_stale = None;
            self.bump_revision();
            return;
        }
        if let ControlObservation::TerminalStale { message } = &envelope.observation {
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.snapshot.terminal_stale = Some(message.clone());
            invalidate_foreground(&mut state.snapshot, message.clone());
            self.bump_revision();
            self.emit_event(
                None,
                instance_id,
                generation,
                ControlEventKind::TerminalStale {
                    message: message.clone(),
                },
            );
            return;
        }
        if let ControlObservation::ProviderEvent {
            source,
            sequence,
            event,
        } = &envelope.observation
        {
            if provider_source_sequence(&current.provider, source) >= *sequence {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::StaleProviderEvent,
                );
                return;
            }
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            let canonical_sequence = reduce_provider_event(
                &mut state.snapshot.provider,
                source,
                *sequence,
                event.clone(),
            );
            self.bump_revision();
            self.emit_event(
                None,
                instance_id,
                generation,
                ControlEventKind::ProviderEvent {
                    sequence: canonical_sequence,
                    source: source.clone(),
                    source_sequence: *sequence,
                    event: event.clone(),
                },
            );
            return;
        }
        if let ControlObservation::ProviderGap { source, missed } = &envelope.observation {
            let missed = *missed;
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            let canonical_sequence =
                reduce_provider_gap(&mut state.snapshot.provider, source, missed);
            self.bump_revision();
            self.emit_event(
                None,
                instance_id,
                generation,
                ControlEventKind::ProviderGap {
                    sequence: canonical_sequence,
                    source: source.clone(),
                    missed,
                },
            );
            return;
        }

        let pending_input = current.pending_input;
        let event = match envelope.observation {
            ControlObservation::Spawned { process_id } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                let terminal_size = state.pending_terminal_size.take();
                let session = &mut state.snapshot;
                session.status = SessionStatus::Running;
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = process_id;
                session.terminal_size = terminal_size;
                ControlEventKind::Running { process_id }
            }
            ControlObservation::SpawnFailed { message } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_terminal_size = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Failed {
                    message: message.clone(),
                };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                ControlEventKind::Failed { message }
            }
            ControlObservation::ProcessExited {
                exit_code,
                final_terminal,
            } => {
                self.effects.retain(|effect| {
                    effect.instance_id != instance_id || effect.generation != generation
                });
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_terminal_size = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Exited { exit_code };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                if let Some(frame) = final_terminal.filter(|frame| {
                    session
                        .terminal_frame
                        .as_ref()
                        .is_none_or(|existing| existing.sequence < frame.sequence)
                }) {
                    session.terminal_size = Some(frame.size);
                    session.terminal_frame = Some(frame);
                    session.terminal_stale = None;
                }
                ControlEventKind::Exited {
                    exit_code,
                    forced: false,
                }
            }
            ControlObservation::StopCompleted {
                forced,
                exit_code,
                final_terminal,
            } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_terminal_size = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Exited { exit_code };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                if let Some(frame) = final_terminal.filter(|frame| {
                    session
                        .terminal_frame
                        .as_ref()
                        .is_none_or(|existing| existing.sequence < frame.sequence)
                }) {
                    session.terminal_size = Some(frame.size);
                    session.terminal_frame = Some(frame);
                    session.terminal_stale = None;
                }
                ControlEventKind::Exited { exit_code, forced }
            }
            ControlObservation::StopFailed { message } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_terminal_size = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Failed {
                    message: message.clone(),
                };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                ControlEventKind::Failed { message }
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
                let input_kind =
                    pending_input.expect("validated input failure must have a pending input kind");
                let session = self.session_mut(instance_id);
                session.pending_operation = None;
                session.pending_input = None;
                ControlEventKind::InputFailed {
                    input_kind,
                    message,
                }
            }
            ControlObservation::ResizeCompleted { size } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_terminal_size = None;
                let session = &mut state.snapshot;
                session.pending_operation = None;
                session.terminal_size = Some(size);
                ControlEventKind::Resized { size }
            }
            ControlObservation::ResizeFailed { message } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_terminal_size = None;
                let session = &mut state.snapshot;
                session.pending_operation = None;
                ControlEventKind::ResizeFailed { message }
            }
            ControlObservation::ForegroundObserved { process } => {
                let session = self.session_mut(instance_id);
                session.pending_operation = None;
                session.foreground = ForegroundSnapshot {
                    authority: ForegroundAuthority::Confirmed,
                    process: Some(process.clone()),
                    stale_reason: None,
                };
                ControlEventKind::ForegroundObserved { process }
            }
            ControlObservation::ForegroundFailed { message } => {
                let session = self.session_mut(instance_id);
                session.pending_operation = None;
                invalidate_foreground(session, message.clone());
                ControlEventKind::ForegroundFailed { message }
            }
            ControlObservation::TerminalFrame { .. }
            | ControlObservation::TerminalStale { .. }
            | ControlObservation::ProviderEvent { .. }
            | ControlObservation::ProviderGap { .. } => {
                unreachable!("terminal observations return before lifecycle event reduction")
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

    pub fn session_snapshot(&self, instance_id: AgentInstanceId) -> Option<&SessionSnapshot> {
        self.sessions.get(&instance_id).map(|state| &state.snapshot)
    }

    pub fn record_command_rejection(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        message: String,
    ) {
        let generation = self
            .sessions
            .get(&instance_id)
            .map(|state| state.snapshot.generation)
            .unwrap_or_default();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::CommandRejected { message },
        );
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
        mut request: StartRequest,
    ) -> Result<(), ControlError> {
        if !request.terminal_size.is_valid() {
            return Err(ControlError::InvalidTerminalSize);
        }
        if request.working_directory.is_empty()
            || request.working_directory.len() > WORKING_DIRECTORY_MAX_BYTES
            || request.working_directory.contains('\0')
        {
            return Err(ControlError::InvalidWorkingDirectory);
        }
        let session = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        let status = session.snapshot.status.clone();
        let transport = session.snapshot.transport;
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

        request.initial_prompt = request
            .initial_prompt
            .as_deref()
            .map(normalize_semantic_prompt)
            .transpose()
            .map_err(|error| ControlError::InputRejected { error })?;
        if transport == TransportKind::Pipe
            && request.initial_prompt.as_deref().is_none_or(str::is_empty)
        {
            return Err(ControlError::MissingInitialPrompt);
        }

        let operation_id = self.allocate_operation();
        let (generation, agent_id, transport) = {
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.pending_terminal_size = Some(request.terminal_size);
            let session = &mut state.snapshot;
            session.generation = SessionGeneration(session.generation.0.saturating_add(1));
            session.status = SessionStatus::Starting;
            session.pending_operation = Some(operation_id);
            session.pending_input = None;
            session.process_id = None;
            session.terminal_size = None;
            session.terminal_frame = None;
            session.terminal_stale = None;
            session.foreground = ForegroundSnapshot::default();
            session.provider = ProviderSnapshot::default();
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
                request,
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
            invalidate_foreground(session, "stop requested".to_owned());
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
        let transport = state.snapshot.transport;
        let (effect, input_kind) = match (transport, action) {
            (TransportKind::Pty, InputAction::AgentCommand(command)) => {
                let input = prepare_agent_command(command, &agent_id)
                    .map_err(|error| ControlError::InputRejected { error })?;
                let input_kind = input.kind();
                (
                    ControlEffect::WriteInput {
                        input,
                        required_foreground: ForegroundRequirement::Agent { agent_id },
                    },
                    input_kind,
                )
            }
            (TransportKind::Pty, InputAction::ShellCommand(command)) => {
                let input = prepare_shell_command(command)
                    .map_err(|error| ControlError::InputRejected { error })?;
                let input_kind = input.kind();
                (
                    ControlEffect::WriteInput {
                        input,
                        required_foreground: ForegroundRequirement::Shell,
                    },
                    input_kind,
                )
            }
            (TransportKind::Pty, action) => {
                let input =
                    prepare_input(action).map_err(|error| ControlError::InputRejected { error })?;
                let input_kind = input.kind();
                let required_foreground = match input_kind {
                    PreparedInputKind::InsertDraft | PreparedInputKind::SubmitPrompt => {
                        ForegroundRequirement::Agent { agent_id }
                    }
                    PreparedInputKind::TerminalText | PreparedInputKind::TerminalControl => {
                        ForegroundRequirement::Any
                    }
                    PreparedInputKind::AgentCommand | PreparedInputKind::ShellCommand => {
                        unreachable!("dispatcher-only input cannot be prepared generically")
                    }
                };
                (
                    ControlEffect::WriteInput {
                        input,
                        required_foreground,
                    },
                    input_kind,
                )
            }
            (TransportKind::Acp, InputAction::SubmitPrompt(prompt)) => {
                let prompt = normalize_semantic_prompt(&prompt.text)
                    .map_err(|error| ControlError::InputRejected { error })?;
                (
                    ControlEffect::SubmitPrompt { prompt },
                    PreparedInputKind::SubmitPrompt,
                )
            }
            (TransportKind::Acp, InputAction::TerminalControl(TerminalControl::Interrupt)) => {
                (ControlEffect::Interrupt, PreparedInputKind::TerminalControl)
            }
            (transport, _) => {
                return Err(ControlError::UnsupportedTransportOperation {
                    transport,
                    action: "this input action".to_owned(),
                });
            }
        };

        let operation_id = self.allocate_operation();
        let generation = {
            let session = self.session_mut(instance_id);
            session.pending_operation = Some(operation_id);
            session.pending_input = Some(input_kind);
            invalidate_foreground(session, "input requested".to_owned());
            session.generation
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect,
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

    fn resize(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        size: TerminalSize,
    ) -> Result<(), ControlError> {
        if !size.is_valid() {
            return Err(ControlError::InvalidTerminalSize);
        }
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if state.snapshot.transport != TransportKind::Pty {
            return Err(ControlError::UnsupportedTransportOperation {
                transport: state.snapshot.transport,
                action: "terminal resize".to_owned(),
            });
        }
        if state.snapshot.status != SessionStatus::Running {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "resize".to_owned(),
                status: state.snapshot.status.clone(),
            });
        }
        if let Some(operation_id) = state.snapshot.pending_operation {
            return Err(ControlError::OperationPending {
                instance_id,
                operation_id,
            });
        }

        let operation_id = self.allocate_operation();
        let generation = {
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.snapshot.pending_operation = Some(operation_id);
            state.pending_terminal_size = Some(size);
            state.snapshot.generation
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::Resize { size },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::ResizeRequested { operation_id, size },
        );
        Ok(())
    }

    fn refresh_foreground(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
    ) -> Result<(), ControlError> {
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if state.snapshot.transport != TransportKind::Pty {
            return Err(ControlError::UnsupportedTransportOperation {
                transport: state.snapshot.transport,
                action: "foreground refresh".to_owned(),
            });
        }
        if state.snapshot.status != SessionStatus::Running {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "refresh foreground".to_owned(),
                status: state.snapshot.status.clone(),
            });
        }
        if let Some(operation_id) = state.snapshot.pending_operation {
            return Err(ControlError::OperationPending {
                instance_id,
                operation_id,
            });
        }

        let operation_id = self.allocate_operation();
        let generation = {
            let session = self.session_mut(instance_id);
            session.pending_operation = Some(operation_id);
            invalidate_foreground(session, "foreground refresh pending".to_owned());
            session.generation
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::ObserveForeground,
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::ForegroundRefreshRequested { operation_id },
        );
        Ok(())
    }

    fn ingest_provider(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        source: ProviderSource,
        source_sequence: u64,
        events: Vec<ProviderEvent>,
    ) -> Result<(), ControlError> {
        source
            .binding
            .validate()
            .map_err(|error| ControlError::InvalidProviderEvent {
                message: error.to_string(),
            })?;
        if source_sequence == 0 || events.is_empty() || events.len() > PROVIDER_INGRESS_EVENTS_MAX {
            return Err(ControlError::InvalidProviderBatch {
                max: PROVIDER_INGRESS_EVENTS_MAX,
            });
        }
        for event in &events {
            event
                .validate_ingress()
                .map_err(|error| ControlError::InvalidProviderEvent {
                    message: error.to_string(),
                })?;
        }

        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if state.snapshot.generation != generation {
            return Err(ControlError::StaleProviderGeneration {
                expected: state.snapshot.generation,
                actual: generation,
            });
        }
        if !matches!(
            state.snapshot.status,
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping
        ) {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "ingest provider events".to_owned(),
                status: state.snapshot.status.clone(),
            });
        }
        let current_source_sequence = provider_source_sequence(&state.snapshot.provider, &source);
        if source_sequence <= current_source_sequence {
            return Err(ControlError::StaleProviderSequence);
        }

        let missed = source_sequence
            .saturating_sub(current_source_sequence)
            .saturating_sub(1);
        if missed > 0 {
            let canonical_sequence = {
                let snapshot = &mut self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session")
                    .snapshot
                    .provider;
                reduce_provider_gap(snapshot, &source, missed)
            };
            self.bump_revision();
            self.emit_event(
                Some(command_id),
                instance_id,
                generation,
                ControlEventKind::ProviderGap {
                    sequence: canonical_sequence,
                    source: source.clone(),
                    missed,
                },
            );
        }

        for event in events {
            let canonical_sequence = {
                let snapshot = &mut self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session")
                    .snapshot
                    .provider;
                reduce_provider_event(snapshot, &source, source_sequence, event.clone())
            };
            self.bump_revision();
            self.emit_event(
                Some(command_id),
                instance_id,
                generation,
                ControlEventKind::ProviderEvent {
                    sequence: canonical_sequence,
                    source: source.clone(),
                    source_sequence,
                    event,
                },
            );
        }
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

fn invalidate_foreground(snapshot: &mut SessionSnapshot, reason: String) {
    snapshot.foreground.authority = ForegroundAuthority::Stale;
    snapshot.foreground.stale_reason = Some(reason);
}

fn provider_source_sequence(snapshot: &ProviderSnapshot, source: &ProviderSource) -> u64 {
    snapshot
        .sources
        .iter()
        .find(|cursor| cursor.source == *source)
        .map_or(0, |cursor| cursor.sequence)
}

fn provider_source_cursor_mut<'a>(
    snapshot: &'a mut ProviderSnapshot,
    source: &ProviderSource,
) -> &'a mut ProviderSourceCursor {
    if let Some(index) = snapshot
        .sources
        .iter()
        .position(|cursor| cursor.source == *source)
    {
        return &mut snapshot.sources[index];
    }
    snapshot.sources.push(ProviderSourceCursor {
        source: source.clone(),
        sequence: 0,
        gap_count: 0,
        stale: false,
    });
    snapshot
        .sources
        .last_mut()
        .expect("provider source was just inserted")
}

fn reduce_provider_event(
    snapshot: &mut ProviderSnapshot,
    source: &ProviderSource,
    source_sequence: u64,
    event: ProviderEvent,
) -> u64 {
    match &event {
        ProviderEvent::SessionStarted {
            session_id,
            model,
            tools,
        } => {
            snapshot.session_id = Some(session_id.clone());
            snapshot.model = (!model.is_empty()).then(|| model.clone());
            snapshot.tools = tools.clone();
            snapshot.activity = ProviderActivity::Idle;
            snapshot.current_prompt = None;
            snapshot.active_tools.clear();
        }
        ProviderEvent::TurnStarted { prompt } => {
            snapshot.activity = ProviderActivity::Working;
            snapshot.current_prompt = prompt.clone();
            snapshot.active_tools.clear();
        }
        ProviderEvent::ToolStarted {
            id,
            name,
            input_json,
        } => {
            snapshot.activity = ProviderActivity::Working;
            if let Some(active) = snapshot.active_tools.iter_mut().find(|tool| tool.id == *id) {
                active.name = name.clone();
                active.input_json = input_json.clone();
            } else {
                snapshot.active_tools.push(ActiveProviderTool {
                    id: id.clone(),
                    name: name.clone(),
                    input_json: input_json.clone(),
                });
            }
        }
        ProviderEvent::ToolCompleted { id, .. } => {
            snapshot.active_tools.retain(|tool| tool.id != *id);
        }
        ProviderEvent::TurnCompleted {
            usage,
            is_cumulative,
        } => {
            snapshot.completed_turns = snapshot.completed_turns.saturating_add(1);
            if *is_cumulative {
                snapshot.usage = usage.clone();
            } else {
                add_token_usage(&mut snapshot.usage, usage);
            }
            snapshot.activity = ProviderActivity::Idle;
            snapshot.current_prompt = None;
            snapshot.active_tools.clear();
        }
        ProviderEvent::ApprovalRequested { .. } => {
            snapshot.activity = ProviderActivity::WaitingForInput;
        }
        ProviderEvent::RateLimited { .. } | ProviderEvent::Error { .. } => {
            snapshot.activity = ProviderActivity::Blocked;
        }
        ProviderEvent::Ready => {
            snapshot.activity = ProviderActivity::Idle;
        }
        ProviderEvent::SessionEnded { .. } => {
            snapshot.activity = ProviderActivity::Idle;
            snapshot.current_prompt = None;
            snapshot.active_tools.clear();
        }
        ProviderEvent::Text { .. } | ProviderEvent::Thinking { .. } => {}
    }
    provider_source_cursor_mut(snapshot, source).sequence = source_sequence;
    provider_source_cursor_mut(snapshot, source).stale = false;
    snapshot.sequence = snapshot.sequence.saturating_add(1);
    snapshot.last_event = Some(event);
    snapshot.stale = snapshot.sources.iter().any(|cursor| cursor.stale);
    snapshot.sequence
}

fn reduce_provider_gap(
    snapshot: &mut ProviderSnapshot,
    source: &ProviderSource,
    missed: u64,
) -> u64 {
    let cursor = provider_source_cursor_mut(snapshot, source);
    cursor.gap_count = cursor.gap_count.saturating_add(missed);
    cursor.stale = true;
    snapshot.gap_count = snapshot.gap_count.saturating_add(missed);
    snapshot.stale = true;
    snapshot.sequence = snapshot.sequence.saturating_add(1);
    snapshot.sequence
}

fn add_token_usage(total: &mut TokenUsage, delta: &TokenUsage) {
    total.input_tokens = total.input_tokens.saturating_add(delta.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(delta.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(delta.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(delta.cache_write_tokens);
    total.reasoning_tokens = total
        .reasoning_tokens
        .saturating_add(delta.reasoning_tokens);
    if delta.context_window.is_some() {
        total.context_window = delta.context_window;
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
        AdapterBinding, AdapterFamily, AdapterId, AdapterVerification, AgentId,
        ForegroundAuthority, ForegroundProcess, ForegroundProcessKind, InputAction,
        PreparedInputKind, PromptFraming, PromptPayload, ShellCommand, TerminalFrame,
        TransportKind,
    };

    fn instance() -> AgentInstanceId {
        AgentInstanceId(7)
    }

    fn provider_source() -> ProviderSource {
        ProviderSource {
            family: AdapterFamily::PtySemantic,
            binding: AdapterBinding::new(
                AdapterId::new("codex").unwrap(),
                "test/v1",
                AdapterVerification::SyntheticFixture,
            )
            .unwrap(),
        }
    }

    fn hook_source() -> ProviderSource {
        ProviderSource {
            family: AdapterFamily::Hook,
            binding: AdapterBinding::new(
                AdapterId::new("grok").unwrap(),
                "test/v1",
                AdapterVerification::SyntheticFixture,
            )
            .unwrap(),
        }
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
                request: StartRequest {
                    working_directory: ".".to_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                },
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
    fn unbounded_terminal_geometry_is_rejected_before_effect_creation() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::Start {
                    instance_id: instance(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 1_001,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                },
            })
            .unwrap_err();
        assert_eq!(error, ControlError::InvalidTerminalSize);
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn provider_events_reduce_usage_and_reject_stale_sequence() {
        let (mut engine, spawn) = running_engine();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::TurnCompleted {
                    usage: TokenUsage {
                        input_tokens: 3,
                        output_tokens: 5,
                        context_window: Some(100),
                        ..TokenUsage::default()
                    },
                    is_cumulative: false,
                },
            },
        });
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderGap {
                source: provider_source(),
                missed: 2,
            },
        });
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::TurnCompleted {
                    usage: TokenUsage {
                        input_tokens: 99,
                        ..TokenUsage::default()
                    },
                    is_cumulative: false,
                },
            },
        });

        let snapshot = engine.snapshot();
        let provider = &snapshot.sessions[0].provider;
        assert_eq!(provider.completed_turns, 1);
        assert_eq!(provider.usage.input_tokens, 3);
        assert_eq!(provider.usage.output_tokens, 5);
        assert_eq!(provider.gap_count, 2);
        assert!(provider.stale);
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::StaleProviderEvent
            }
        )));
    }

    #[test]
    fn provider_activity_tracks_turn_tools_and_attention_without_late_text_resurrection() {
        let (mut engine, spawn) = running_engine();
        let events = [
            ProviderEvent::TurnStarted {
                prompt: Some("fix tests".to_owned()),
            },
            ProviderEvent::ToolStarted {
                id: "tool-1".to_owned(),
                name: "shell".to_owned(),
                input_json: "{\"command\":\"cargo test\"}".to_owned(),
            },
            ProviderEvent::ApprovalRequested {
                tool_name: "shell".to_owned(),
                description: Some("approve".to_owned()),
            },
            ProviderEvent::ToolCompleted {
                id: "tool-1".to_owned(),
                output: "ok".to_owned(),
                is_error: false,
                duration_ms: None,
            },
            ProviderEvent::TurnCompleted {
                usage: TokenUsage::default(),
                is_cumulative: false,
            },
            ProviderEvent::Text {
                text: "late final text".to_owned(),
                is_delta: false,
            },
        ];

        for (index, event) in events.into_iter().enumerate() {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: u64::try_from(index + 1).unwrap(),
                    event,
                },
            });
            let provider = &engine.snapshot().sessions[0].provider;
            match index {
                0 => {
                    assert_eq!(provider.activity, ProviderActivity::Working);
                    assert_eq!(provider.current_prompt.as_deref(), Some("fix tests"));
                }
                1 => assert_eq!(provider.active_tools.len(), 1),
                2 => assert_eq!(provider.activity, ProviderActivity::WaitingForInput),
                3 => assert!(provider.active_tools.is_empty()),
                4 | 5 => assert_eq!(provider.activity, ProviderActivity::Idle),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn external_ingress_merges_sources_with_canonical_sequence_and_automatic_gap() {
        let (mut engine, spawn) = running_engine();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::Ready,
            },
        });

        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(30),
                command: ControlCommand::IngestProvider {
                    instance_id: spawn.instance_id,
                    generation: spawn.generation,
                    source: hook_source(),
                    source_sequence: 1,
                    events: vec![
                        ProviderEvent::TurnStarted {
                            prompt: Some("first".to_owned()),
                        },
                        ProviderEvent::ToolStarted {
                            id: "tool-1".to_owned(),
                            name: "shell".to_owned(),
                            input_json: "{}".to_owned(),
                        },
                    ],
                },
            })
            .unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(31),
                command: ControlCommand::IngestProvider {
                    instance_id: spawn.instance_id,
                    generation: spawn.generation,
                    source: hook_source(),
                    source_sequence: 3,
                    events: vec![ProviderEvent::TurnCompleted {
                        usage: TokenUsage::default(),
                        is_cumulative: false,
                    }],
                },
            })
            .unwrap();

        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(provider.sequence, 5);
        assert_eq!(provider.sources.len(), 2);
        assert_eq!(provider.gap_count, 1);
        assert!(!provider.stale);
        assert_eq!(provider.completed_turns, 1);
        assert!(engine.drain_effects().is_empty());
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ProviderGap {
                sequence: 4,
                missed: 1,
                ..
            }
        )));

        let stale = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(32),
                command: ControlCommand::IngestProvider {
                    instance_id: spawn.instance_id,
                    generation: spawn.generation,
                    source: hook_source(),
                    source_sequence: 3,
                    events: vec![ProviderEvent::Ready],
                },
            })
            .unwrap_err();
        assert_eq!(stale, ControlError::StaleProviderSequence);
    }

    #[test]
    fn external_ingress_rejects_stale_generation_empty_batches_and_oversized_events() {
        let (mut engine, spawn) = running_engine();
        let command = |id, generation, events| CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(id),
            command: ControlCommand::IngestProvider {
                instance_id: spawn.instance_id,
                generation,
                source: hook_source(),
                source_sequence: 1,
                events,
            },
        };

        assert!(matches!(
            engine.apply_command(command(
                40,
                SessionGeneration(0),
                vec![ProviderEvent::Ready]
            )),
            Err(ControlError::StaleProviderGeneration { .. })
        ));
        assert_eq!(
            engine.apply_command(command(41, spawn.generation, Vec::new())),
            Err(ControlError::InvalidProviderBatch {
                max: PROVIDER_INGRESS_EVENTS_MAX
            })
        );
        assert!(matches!(
            engine.apply_command(command(
                42,
                spawn.generation,
                vec![ProviderEvent::Text {
                    text: "x".repeat(gate4agent_types::PROVIDER_EVENT_TEXT_MAX_BYTES + 1),
                    is_delta: false,
                }]
            )),
            Err(ControlError::InvalidProviderEvent { .. })
        ));
    }

    #[test]
    fn pipe_requires_initial_prompt_and_rejects_followup_input() {
        let mut engine = Gate4AgentEngine::new();
        let mut register = register(1);
        if let ControlCommand::Register { transport, .. } = &mut register.command {
            *transport = TransportKind::Pipe;
        }
        engine.apply_command(register).unwrap();
        assert_eq!(
            engine.apply_command(start(2)).unwrap_err(),
            ControlError::MissingInitialPrompt
        );

        let mut request = start(3);
        if let ControlCommand::Start { request, .. } = &mut request.command {
            request.initial_prompt = Some("hello".to_owned());
        }
        engine.apply_command(request).unwrap();
        let spawn = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(spawn.operation_id),
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::Spawned {
                process_id: Some(1),
            },
        });
        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(4),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::SubmitPrompt(PromptPayload {
                        text: "again".to_owned(),
                        framing: PromptFraming::Literal,
                    }),
                },
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ControlError::UnsupportedTransportOperation {
                transport: TransportKind::Pipe,
                ..
            }
        ));
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
        assert_eq!(
            session.terminal_size,
            Some(TerminalSize {
                rows: 24,
                columns: 80,
            })
        );
    }

    #[test]
    fn resize_is_pending_until_observed() {
        let (mut engine, _) = running_engine();
        let size = TerminalSize {
            rows: 40,
            columns: 120,
        };
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(9),
                command: ControlCommand::Resize {
                    instance_id: instance(),
                    size,
                },
            })
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        assert_eq!(
            engine.snapshot().sessions[0].terminal_size,
            Some(TerminalSize {
                rows: 24,
                columns: 80,
            })
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::ResizeCompleted { size },
        });
        assert_eq!(engine.snapshot().sessions[0].terminal_size, Some(size));
    }

    #[test]
    fn foreground_refresh_is_generation_bound_replaceable_authority() {
        let (mut engine, _) = running_engine();
        engine.drain_events();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(10),
                command: ControlCommand::RefreshForeground {
                    instance_id: instance(),
                },
            })
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        assert_eq!(effect.effect, ControlEffect::ObserveForeground);
        assert_eq!(
            engine.snapshot().sessions[0].foreground.authority,
            ForegroundAuthority::Stale
        );

        let process = ForegroundProcess {
            root_process_id: 42,
            process_id: 84,
            process_name: "claude".to_owned(),
            kind: ForegroundProcessKind::Agent {
                agent_id: AgentId::new("claude").unwrap(),
            },
        };
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::ForegroundObserved {
                process: process.clone(),
            },
        });

        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.pending_operation, None);
        assert_eq!(session.foreground.authority, ForegroundAuthority::Confirmed);
        assert_eq!(session.foreground.process.as_ref(), Some(&process));
        assert_eq!(session.foreground.stale_reason, None);
        assert!(engine.drain_events().iter().any(|event| matches!(
            &event.event,
            ControlEventKind::ForegroundObserved { process: observed } if observed == &process
        )));
    }

    #[test]
    fn foreground_refresh_rejects_another_root_process() {
        let (mut engine, _) = running_engine();
        engine.drain_events();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(11),
                command: ControlCommand::RefreshForeground {
                    instance_id: instance(),
                },
            })
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::ForegroundObserved {
                process: ForegroundProcess {
                    root_process_id: 999,
                    process_id: 1_000,
                    process_name: "claude".to_owned(),
                    kind: ForegroundProcessKind::Agent {
                        agent_id: AgentId::new("claude").unwrap(),
                    },
                },
            },
        });

        assert_eq!(
            engine.snapshot().sessions[0].foreground.authority,
            ForegroundAuthority::Stale
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::InvalidForegroundObservation
            }
        )));
    }

    #[test]
    fn terminal_frames_are_replaceable_and_never_move_backwards() {
        let (mut engine, spawn) = running_engine();
        engine.drain_events();
        let frame = TerminalFrame {
            sequence: 10,
            size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            cursor_row: 1,
            cursor_column: 2,
            contents: "current".to_owned(),
            formatted: b"current".to_vec(),
        };
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::TerminalFrame {
                frame: frame.clone(),
            },
        });
        assert_eq!(engine.snapshot().sessions[0].terminal_frame, Some(frame));
        assert!(engine.drain_events().is_empty());

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::TerminalFrame {
                frame: TerminalFrame {
                    sequence: 9,
                    size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    cursor_row: 0,
                    cursor_column: 0,
                    contents: "stale".to_owned(),
                    formatted: Vec::new(),
                },
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0]
                .terminal_frame
                .as_ref()
                .unwrap()
                .sequence,
            10
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::StaleTerminalFrame
            }
        )));
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
        assert_eq!(
            engine.snapshot().sessions[0].status,
            SessionStatus::Stopping
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::StopCompleted {
                forced: true,
                exit_code: None,
                final_terminal: None,
            },
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
        assert!(matches!(
            &effect.effect,
            ControlEffect::WriteInput {
                required_foreground: ForegroundRequirement::Agent { agent_id },
                ..
            } if agent_id.as_str() == "claude"
        ));
        assert_eq!(
            engine.snapshot().sessions[0].pending_input,
            Some(PreparedInputKind::SubmitPrompt)
        );
        assert_eq!(
            engine.snapshot().sessions[0].foreground.authority,
            ForegroundAuthority::Stale
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
    fn shell_command_effect_requires_fresh_shell_routing() {
        let (mut engine, _) = running_engine();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(70),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::ShellCommand(ShellCommand {
                        text: "git status --short".to_owned(),
                    }),
                },
            })
            .unwrap();

        let effect = engine.drain_effects().pop().unwrap();
        assert!(matches!(
            effect.effect,
            ControlEffect::WriteInput {
                input,
                required_foreground: ForegroundRequirement::Shell,
            } if input.kind() == PreparedInputKind::ShellCommand
        ));
        assert_eq!(
            engine.snapshot().sessions[0].pending_input,
            Some(PreparedInputKind::ShellCommand)
        );
    }

    #[test]
    fn agent_command_effect_is_bound_to_the_session_agent_route() {
        let (mut engine, _) = running_engine();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(71),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::AgentCommand(gate4agent_types::AgentCommand {
                        agent_id: AgentId::new("claude").unwrap(),
                        name: "review".to_owned(),
                        arguments: vec!["routing".to_owned()],
                    }),
                },
            })
            .unwrap();

        let effect = engine.drain_effects().pop().unwrap();
        assert!(matches!(
            effect.effect,
            ControlEffect::WriteInput {
                input,
                required_foreground: ForegroundRequirement::Agent { agent_id },
            } if input.kind() == PreparedInputKind::AgentCommand && agent_id.as_str() == "claude"
        ));
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
            observation: ControlObservation::ProcessExited {
                exit_code: Some(0),
                final_terminal: None,
            },
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
