//! Deterministic single-writer lifecycle engine for gate4agent sessions.

use gate4agent_types::{
    normalize_semantic_prompt, prepare_agent_command, prepare_input, prepare_shell_command,
    validate_candidate_id, validate_capability_models, validate_history_error,
    validate_resume_error, ActiveProviderTool, AgentInstanceId, CapabilityProbeRequest,
    CapabilitySnapshot, CommandEnvelope, CommandId, ControlCommand, ControlEffect, ControlError,
    ControlEvent, ControlEventKind, ControlHealth, ControlObservation, ControlSnapshot,
    EffectEnvelope, ForegroundAuthority, ForegroundRequirement, ForegroundSnapshot,
    HistoryOperation, HistoryQuery, HistorySnapshot, InputAction, ObservationEnvelope,
    ObservationIgnoredReason, OperationId, PendingCapabilityProbe, PendingHistoryOperation,
    PendingResumeOperation, PreparedInputKind, ProviderActivity, ProviderEvent,
    ProviderInteraction, ProviderInteractionId, ProviderInteractionKind,
    ProviderInteractionOutcome, ProviderInteractionResponse, ProviderInteractionResponseKind,
    ProviderInteractionStatus, ProviderInteractionTarget, ProviderSessionIdentity,
    ProviderRuntimeCapability, ProviderRuntimePolicy, ProviderSessionKey, ProviderSnapshot,
    ProviderSource, ProviderSourceCursor, ProviderSubagent,
    ResumeAuthorityTarget, ResumeLaunchRequest, ResumePhase, ResumeSessionSummary, ResumeSnapshot,
    ResumeTarget, SessionGeneration, SessionSnapshot, SessionStatus, StartRequest, TerminalControl,
    TerminalSize, TokenUsage, TransportKind, CONTROL_INSTANCE_IDENTITIES_CAPACITY,
    CONTROL_INSTANCE_IDENTITIES_MAX, CONTROL_PROTOCOL_VERSION, CONTROL_SESSIONS_MAX,
    PROVIDER_INGRESS_EVENTS_MAX, PROVIDER_INTERACTIONS_MAX, PROVIDER_INTERACTION_FAILURE_MAX_BYTES,
    PROVIDER_SUBAGENTS_MAX, WORKING_DIRECTORY_MAX_BYTES,
};
use std::collections::{BTreeMap, BTreeSet};

const CONTROL_REVISION_HEADROOM: u64 = PROVIDER_INGRESS_EVENTS_MAX as u64 + 1;
const CONTROL_EVENT_HEADROOM: u64 = PROVIDER_INGRESS_EVENTS_MAX as u64
    * (PROVIDER_INTERACTIONS_MAX as u64 + 1)
    + PROVIDER_INTERACTIONS_MAX as u64
    + 2;

#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionState {
    snapshot: SessionSnapshot,
    runtime_policy: ProviderRuntimePolicy,
    pending_terminal_size: Option<TerminalSize>,
    pending_interrupt: bool,
    pending_resume_identity: Option<ProviderSessionIdentity>,
}

/// Owns logical session lifecycle state. External work is emitted as effects
/// and can change observed state only after a matching observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Gate4AgentEngine {
    sessions: BTreeMap<AgentInstanceId, SessionState>,
    generation_watermarks: BTreeMap<AgentInstanceId, SessionGeneration>,
    next_operation_id: Option<u64>,
    next_event_sequence: Option<u64>,
    revision: u64,
    counter_error: Option<ControlError>,
    effects: Vec<EffectEnvelope>,
    events: Vec<ControlEvent>,
}

impl Gate4AgentEngine {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            generation_watermarks: BTreeMap::new(),
            next_operation_id: Some(1),
            next_event_sequence: Some(1),
            revision: 0,
            counter_error: None,
            effects: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn apply_command(&mut self, envelope: CommandEnvelope) -> Result<(), ControlError> {
        if self.has_counter_headroom() {
            let result = self.apply_command_in_place(envelope);
            debug_assert!(
                self.counter_error.is_none(),
                "bounded command exceeded reserved control counter headroom"
            );
            return result;
        }
        let mut candidate = self.clone();
        candidate.apply_command_in_place(envelope)?;
        if let Some(error) = candidate.counter_error.take() {
            self.retire_exhausted_counter(&error);
            return Err(error);
        }
        *self = candidate;
        Ok(())
    }

    fn apply_command_in_place(&mut self, envelope: CommandEnvelope) -> Result<(), ControlError> {
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
                if self.sessions.len() >= CONTROL_SESSIONS_MAX {
                    return Err(ControlError::SessionCapacityExceeded {
                        instance_id,
                        max: CONTROL_SESSIONS_MAX,
                    });
                }
                if !self.generation_watermarks.contains_key(&instance_id)
                    && self.generation_watermarks.len() >= CONTROL_INSTANCE_IDENTITIES_MAX
                {
                    return Err(ControlError::InstanceIdentityCapacityExceeded {
                        instance_id,
                        max: CONTROL_INSTANCE_IDENTITIES_MAX,
                    });
                }
                let generation = match self.generation_watermarks.get(&instance_id).copied() {
                    Some(watermark) => checked_next_generation(watermark).ok_or(
                        ControlError::GenerationExhausted {
                            instance_id,
                            generation: watermark,
                        },
                    )?,
                    None => SessionGeneration::default(),
                };
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
                            session_options: None,
                            capabilities: CapabilitySnapshot::default(),
                            history: HistorySnapshot::default(),
                            resume: ResumeSnapshot::default(),
                            foreground: ForegroundSnapshot::default(),
                            provider: ProviderSnapshot::default(),
                        },
                        runtime_policy: ProviderRuntimePolicy::raw_pty(),
                        pending_terminal_size: None,
                        pending_interrupt: false,
                        pending_resume_identity: None,
                    },
                );
                self.generation_watermarks.insert(instance_id, generation);
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
                runtime_policy,
                request,
            } => self.start(command_id, instance_id, runtime_policy, request),
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
            ControlCommand::ProbeCapabilities {
                instance_id,
                request,
            } => self.probe_capabilities(command_id, instance_id, request),
            ControlCommand::DiscoverHistory { instance_id, query } => {
                self.discover_history(command_id, instance_id, query)
            }
            ControlCommand::LoadHistory {
                instance_id,
                candidate_id,
            } => self.load_history(command_id, instance_id, candidate_id),
            ControlCommand::Resume {
                instance_id,
                target,
                runtime_policy,
                request,
            } => self.resume(command_id, instance_id, target, runtime_policy, request),
            ControlCommand::ResolveInteraction {
                instance_id,
                generation,
                interaction_id,
                response,
            } => self.resolve_interaction(
                command_id,
                instance_id,
                generation,
                interaction_id,
                response,
            ),
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
        let _ = self.try_apply_observation(envelope);
    }

    pub fn try_apply_observation(
        &mut self,
        envelope: ObservationEnvelope,
    ) -> Result<(), ControlError> {
        if self.has_counter_headroom() && self.observation_has_provider_headroom(&envelope) {
            self.apply_observation_in_place(envelope);
            debug_assert!(
                self.counter_error.is_none(),
                "bounded observation exceeded reserved control counter headroom"
            );
            return Ok(());
        }
        let mut candidate = self.clone();
        candidate.apply_observation_in_place(envelope);
        if let Some(error) = candidate.counter_error.take() {
            self.retire_exhausted_counter(&error);
            return Err(error);
        }
        *self = candidate;
        Ok(())
    }

    fn apply_observation_in_place(&mut self, envelope: ObservationEnvelope) {
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
        let Some(current_state) = self.sessions.get(&instance_id) else {
            self.emit_ignored(
                instance_id,
                generation,
                ObservationIgnoredReason::UnknownInstance,
            );
            return;
        };
        let runtime_policy = current_state.runtime_policy;
        let current = &current_state.snapshot;

        let capability_generation_matches = is_capability_observation(&envelope.observation)
            && current
                .capabilities
                .pending
                .as_ref()
                .is_some_and(|pending| pending.generation == generation);
        if current.generation != generation && !capability_generation_matches {
            self.emit_ignored(
                instance_id,
                generation,
                ObservationIgnoredReason::StaleGeneration,
            );
            return;
        }

        if let Some(capability) = denied_observation_capability(runtime_policy, &envelope.observation)
        {
            self.emit_ignored(
                instance_id,
                generation,
                ObservationIgnoredReason::ProviderRuntimePolicyDenied { capability },
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
            let expected_operation = if is_capability_observation(&envelope.observation) {
                current
                    .capabilities
                    .pending
                    .as_ref()
                    .map(|pending| pending.operation_id)
            } else if is_history_observation(&envelope.observation) {
                current
                    .history
                    .pending
                    .as_ref()
                    .map(|pending| pending.operation_id)
            } else {
                current.pending_operation
            };
            if expected_operation != Some(operation_id) {
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
                    SessionStatus::Running,
                    ControlObservation::InteractionResolutionCompleted { .. }
                        | ControlObservation::InteractionResolutionFailed { .. },
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
                | (
                    _,
                    ControlObservation::CapabilitiesProbed { .. }
                        | ControlObservation::CapabilityProbeFailed { .. },
                )
                | (
                    _,
                    ControlObservation::HistoryDiscovered { .. }
                        | ControlObservation::HistoryLoaded { .. }
                        | ControlObservation::HistoryFailed { .. },
                )
                | (
                    _,
                    ControlObservation::ResumeAuthorized { .. }
                        | ControlObservation::ResumeDenied { .. }
                        | ControlObservation::ResumeFailed { .. },
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

        if is_capability_observation(&envelope.observation) {
            let pending = current
                .capabilities
                .pending
                .as_ref()
                .expect("capability operation correlation was validated")
                .clone();
            let event = match &envelope.observation {
                ControlObservation::CapabilitiesProbed {
                    session_option_models,
                } if validate_capability_models(session_option_models).is_ok() => {
                    ControlEventKind::CapabilitiesProbed {
                        count: session_option_models.len(),
                    }
                }
                ControlObservation::CapabilityProbeFailed { failure } => {
                    ControlEventKind::CapabilityProbeFailed { failure: *failure }
                }
                _ => {
                    self.emit_ignored(
                        instance_id,
                        generation,
                        ObservationIgnoredReason::InvalidCapabilityObservation,
                    );
                    return;
                }
            };
            let event_generation = self
                .sessions
                .get(&instance_id)
                .expect("validated session")
                .snapshot
                .generation;
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            debug_assert_eq!(state.snapshot.capabilities.pending.as_ref(), Some(&pending));
            state.snapshot.capabilities.pending = None;
            state.snapshot.capabilities.settled = true;
            match envelope.observation {
                ControlObservation::CapabilitiesProbed {
                    session_option_models,
                } => {
                    state.snapshot.capabilities.session_option_models = session_option_models;
                    state.snapshot.capabilities.last_failure = None;
                }
                ControlObservation::CapabilityProbeFailed { failure } => {
                    state.snapshot.capabilities.session_option_models.clear();
                    state.snapshot.capabilities.last_failure = Some(failure);
                }
                _ => unreachable!("capability observation was matched above"),
            }
            self.bump_revision();
            self.emit_event(None, instance_id, event_generation, event);
            return;
        }

        if is_history_observation(&envelope.observation) {
            let pending = current
                .history
                .pending
                .as_ref()
                .expect("history operation correlation was validated")
                .clone();
            let event = match (&envelope.observation, &pending.operation) {
                (
                    ControlObservation::HistoryDiscovered { candidates },
                    HistoryOperation::Discover { query },
                ) if history_candidates_are_valid(candidates, query.limit) => {
                    ControlEventKind::HistoryDiscovered {
                        count: candidates.len(),
                    }
                }
                (ControlObservation::HistoryLoaded { session }, HistoryOperation::Load { .. })
                    if session.validate().is_ok() =>
                {
                    ControlEventKind::HistoryLoaded {
                        session_id: session.session_id.clone(),
                    }
                }
                (ControlObservation::HistoryFailed { message }, _)
                    if validate_history_error(message).is_ok() =>
                {
                    ControlEventKind::HistoryFailed {
                        message: message.clone(),
                    }
                }
                _ => {
                    self.emit_ignored(
                        instance_id,
                        generation,
                        ObservationIgnoredReason::InvalidHistoryObservation,
                    );
                    return;
                }
            };
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.snapshot.history.pending = None;
            match envelope.observation {
                ControlObservation::HistoryDiscovered { candidates } => {
                    state.snapshot.history.candidates = candidates;
                    state.snapshot.history.loaded_candidate_id = None;
                    state.snapshot.history.loaded = None;
                    state.snapshot.history.last_error = None;
                }
                ControlObservation::HistoryLoaded { session } => {
                    state.snapshot.history.loaded_candidate_id = match pending.operation {
                        HistoryOperation::Load { candidate_id } => Some(candidate_id),
                        HistoryOperation::Discover { .. } => {
                            unreachable!("loaded history matched a load operation")
                        }
                    };
                    state.snapshot.history.loaded = Some(session);
                    state.snapshot.history.last_error = None;
                }
                ControlObservation::HistoryFailed { message } => {
                    state.snapshot.history.last_error = Some(message);
                }
                _ => unreachable!("history observation was matched above"),
            }
            self.bump_revision();
            self.emit_event(None, instance_id, generation, event);
            return;
        }

        if is_resume_authority_observation(&envelope.observation) {
            let Some(pending) = current.resume.pending.as_ref().cloned() else {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::InvalidResumeObservation,
                );
                return;
            };
            if pending.phase != ResumePhase::Authorizing {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::InvalidResumeObservation,
                );
                return;
            }
            let operation_id = pending.operation_id;
            match envelope.observation {
                ControlObservation::ResumeAuthorized { provider_session }
                    if provider_session.validate().is_ok()
                        && resume_identity_matches_target(
                            current,
                            &pending.target,
                            &provider_session,
                        ) =>
                {
                    let generation_watermark =
                        self.generation_watermark(instance_id, current.generation);
                    let Some(next_generation) = checked_next_generation(generation_watermark)
                    else {
                        self.emit_ignored(
                            instance_id,
                            generation,
                            ObservationIgnoredReason::GenerationExhausted,
                        );
                        return;
                    };
                    let summary = ResumeSessionSummary::from(&provider_session);
                    let transport = current.transport;
                    let agent_id = {
                        let state = self
                            .sessions
                            .get_mut(&instance_id)
                            .expect("validated session");
                        state.pending_terminal_size = Some(pending.request.terminal_size);
                        state.pending_interrupt = false;
                        state.pending_resume_identity = Some(provider_session.clone());
                        let session = &mut state.snapshot;
                        session.generation = next_generation;
                        session.status = SessionStatus::Starting;
                        session.pending_operation = Some(operation_id);
                        session.pending_input = None;
                        session.process_id = None;
                        session.terminal_size = None;
                        session.terminal_frame = None;
                        session.terminal_stale = None;
                        session.session_options = None;
                        session.history = HistorySnapshot::default();
                        session.foreground = ForegroundSnapshot::default();
                        session.provider = ProviderSnapshot::default();
                        session.resume.pending = Some(PendingResumeOperation {
                            phase: ResumePhase::Spawning,
                            ..pending.clone()
                        });
                        session.resume.last_error = None;
                        session.agent_id.clone()
                    };
                    self.generation_watermarks
                        .insert(instance_id, next_generation);
                    self.effects.push(EffectEnvelope {
                        protocol_version: CONTROL_PROTOCOL_VERSION,
                        operation_id,
                        instance_id,
                        generation: next_generation,
                        effect: ControlEffect::SpawnResume {
                            agent_id,
                            transport,
                            provider_session,
                            runtime_policy,
                            request: pending.request,
                        },
                    });
                    self.bump_revision();
                    self.emit_event(
                        None,
                        instance_id,
                        next_generation,
                        ControlEventKind::ResumeAuthorized { session: summary },
                    );
                }
                ControlObservation::ResumeDenied { reason }
                    if validate_resume_error(&reason).is_ok() =>
                {
                    let state = self
                        .sessions
                        .get_mut(&instance_id)
                        .expect("validated session");
                    state.pending_resume_identity = None;
                    state.snapshot.pending_operation = None;
                    state.snapshot.resume.pending = None;
                    state.snapshot.resume.last_error = Some(reason.clone());
                    self.bump_revision();
                    self.emit_event(
                        None,
                        instance_id,
                        generation,
                        ControlEventKind::ResumeDenied { reason },
                    );
                }
                ControlObservation::ResumeFailed { message }
                    if validate_resume_error(&message).is_ok() =>
                {
                    let state = self
                        .sessions
                        .get_mut(&instance_id)
                        .expect("validated session");
                    state.pending_resume_identity = None;
                    state.snapshot.pending_operation = None;
                    state.snapshot.resume.pending = None;
                    state.snapshot.resume.last_error = Some(message.clone());
                    self.bump_revision();
                    self.emit_event(
                        None,
                        instance_id,
                        generation,
                        ControlEventKind::ResumeFailed { message },
                    );
                }
                _ => {
                    self.emit_ignored(
                        instance_id,
                        generation,
                        ObservationIgnoredReason::InvalidResumeObservation,
                    );
                }
            }
            return;
        }

        if is_interaction_resolution_observation(&envelope.observation) {
            let operation_id = envelope
                .operation_id
                .expect("interaction resolution requires an operation id");
            let (interaction_id, failure) = match &envelope.observation {
                ControlObservation::InteractionResolutionCompleted { interaction_id } => {
                    (*interaction_id, None)
                }
                ControlObservation::InteractionResolutionFailed {
                    interaction_id,
                    message,
                } if interaction_failure_is_valid(message) => {
                    (*interaction_id, Some(message.clone()))
                }
                ControlObservation::InteractionResolutionFailed { .. } => {
                    self.emit_ignored(
                        instance_id,
                        generation,
                        ObservationIgnoredReason::InvalidInteractionObservation,
                    );
                    return;
                }
                _ => unreachable!("interaction observation was classified above"),
            };
            let Some(interaction) = current
                .provider
                .interactions
                .iter()
                .find(|interaction| interaction.id == interaction_id)
            else {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::InvalidInteractionObservation,
                );
                return;
            };
            let ProviderInteractionStatus::Resolving {
                operation_id: interaction_operation_id,
                response_kind,
            } = interaction.status
            else {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::InvalidInteractionObservation,
                );
                return;
            };
            if interaction_operation_id != operation_id {
                self.emit_ignored(
                    instance_id,
                    generation,
                    ObservationIgnoredReason::InvalidInteractionObservation,
                );
                return;
            }
            let resume_activity = interaction.resume_lead_activity;
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.snapshot.pending_operation = None;
            let interaction = state
                .snapshot
                .provider
                .interactions
                .iter_mut()
                .find(|interaction| interaction.id == interaction_id)
                .expect("validated interaction");
            if let Some(message) = failure {
                interaction.status = ProviderInteractionStatus::Pending;
                state.snapshot.provider.lead_activity = ProviderActivity::WaitingForInput;
                refresh_provider_activity(&mut state.snapshot.provider);
                self.bump_revision();
                self.emit_event(
                    None,
                    instance_id,
                    generation,
                    ControlEventKind::InteractionResolutionFailed {
                        interaction_id,
                        message,
                    },
                );
            } else {
                let outcome = interaction_response_outcome(response_kind);
                interaction.status = ProviderInteractionStatus::Resolved { outcome };
                state.snapshot.provider.lead_activity = if state
                    .snapshot
                    .provider
                    .interactions
                    .iter()
                    .any(interaction_is_unresolved)
                {
                    ProviderActivity::WaitingForInput
                } else {
                    resume_activity.unwrap_or(ProviderActivity::Working)
                };
                refresh_provider_activity(&mut state.snapshot.provider);
                self.bump_revision();
                self.emit_event(
                    None,
                    instance_id,
                    generation,
                    ControlEventKind::InteractionResolved {
                        interaction_id,
                        outcome,
                    },
                );
            }
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
            if current.provider.sequence == u64::MAX {
                self.counter_error = Some(ControlError::ProviderSequenceExhausted {
                    instance_id,
                    generation,
                });
                return;
            }
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
            let pending_resolution = pending_interaction_resolution(&state.snapshot);
            let reduction = reduce_provider_event(
                &mut state.snapshot.provider,
                source,
                *sequence,
                event.clone(),
            );
            let superseded_resolution =
                pending_resolution.filter(|(operation_id, interaction_id)| {
                    !interaction_resolution_is_pending(
                        &state.snapshot,
                        *operation_id,
                        *interaction_id,
                    )
                });
            if superseded_resolution.is_some() {
                state.snapshot.pending_operation = None;
            }
            if let Some((operation_id, _)) = superseded_resolution {
                self.effects.retain(|effect| {
                    effect.instance_id != instance_id
                        || effect.generation != generation
                        || effect.operation_id != operation_id
                });
            }
            self.bump_revision();
            self.emit_event(
                None,
                instance_id,
                generation,
                ControlEventKind::ProviderEvent {
                    sequence: reduction.sequence,
                    source: source.clone(),
                    source_sequence: *sequence,
                    event: event.clone(),
                },
            );
            self.emit_interaction_transitions(
                None,
                instance_id,
                generation,
                reduction.interaction_transitions,
            );
            return;
        }
        if let ControlObservation::ProviderGap { source, missed } = &envelope.observation {
            if current.provider.sequence == u64::MAX {
                self.counter_error = Some(ControlError::ProviderSequenceExhausted {
                    instance_id,
                    generation,
                });
                return;
            }
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
        let pending_interrupt = self
            .sessions
            .get(&instance_id)
            .expect("validated session")
            .pending_interrupt;
        let pending_resume = current.resume.pending.clone();
        let pending_resume_identity = self
            .sessions
            .get(&instance_id)
            .expect("validated session")
            .pending_resume_identity
            .clone();
        let mut interaction_transitions = Vec::new();
        let event = match envelope.observation {
            ControlObservation::Spawned { process_id } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_interrupt = false;
                state.pending_resume_identity = None;
                let terminal_size = state.pending_terminal_size.take();
                let session = &mut state.snapshot;
                session.status = SessionStatus::Running;
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = process_id;
                session.terminal_size = terminal_size;
                if pending_resume
                    .as_ref()
                    .is_some_and(|pending| pending.phase == ResumePhase::Spawning)
                {
                    let identity = pending_resume_identity
                        .as_ref()
                        .expect("resume spawn must retain its authorized identity");
                    let summary = ResumeSessionSummary::from(identity);
                    session.provider.session = Some(identity.clone());
                    session.resume.pending = None;
                    session.resume.last_session = Some(summary.clone());
                    session.resume.last_error = None;
                    ControlEventKind::Resumed {
                        session: summary,
                        process_id,
                    }
                } else {
                    ControlEventKind::Running { process_id }
                }
            }
            ControlObservation::SpawnFailed { message } => {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_terminal_size = None;
                state.pending_interrupt = false;
                state.pending_resume_identity = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Failed {
                    message: message.clone(),
                };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                interaction_transitions = resolve_all_pending_interactions(
                    &mut session.provider,
                    ProviderInteractionOutcome::TurnEnded,
                );
                if pending_resume
                    .as_ref()
                    .is_some_and(|pending| pending.phase == ResumePhase::Spawning)
                {
                    session.provider.session = pending_resume_identity.clone();
                    session.resume.pending = None;
                    session.resume.last_error = Some(message.clone());
                    ControlEventKind::ResumeFailed { message }
                } else {
                    ControlEventKind::Failed { message }
                }
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
                state.pending_interrupt = false;
                state.pending_resume_identity = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Exited { exit_code };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                session.resume.pending = None;
                interaction_transitions = resolve_all_pending_interactions(
                    &mut session.provider,
                    ProviderInteractionOutcome::TurnEnded,
                );
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
                state.pending_interrupt = false;
                state.pending_resume_identity = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Exited { exit_code };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                session.resume.pending = None;
                interaction_transitions = resolve_all_pending_interactions(
                    &mut session.provider,
                    ProviderInteractionOutcome::TurnEnded,
                );
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
                state.pending_interrupt = false;
                state.pending_resume_identity = None;
                let session = &mut state.snapshot;
                session.status = SessionStatus::Failed {
                    message: message.clone(),
                };
                session.pending_operation = None;
                session.pending_input = None;
                session.process_id = None;
                session.foreground = ForegroundSnapshot::default();
                session.resume.pending = None;
                interaction_transitions = resolve_all_pending_interactions(
                    &mut session.provider,
                    ProviderInteractionOutcome::TurnEnded,
                );
                ControlEventKind::Failed { message }
            }
            ControlObservation::InputCompleted => {
                let input_kind = pending_input
                    .expect("validated input completion must have a pending input kind");
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_interrupt = false;
                let session = &mut state.snapshot;
                session.pending_operation = None;
                session.pending_input = None;
                if pending_interrupt {
                    interaction_transitions = resolve_all_pending_interactions(
                        &mut session.provider,
                        ProviderInteractionOutcome::Interrupted,
                    );
                    session.provider.lead_activity = ProviderActivity::Idle;
                    refresh_provider_activity(&mut session.provider);
                    session.provider.current_prompt = None;
                    session.provider.active_tools.clear();
                }
                ControlEventKind::InputCompleted { input_kind }
            }
            ControlObservation::InputFailed { message } => {
                let input_kind =
                    pending_input.expect("validated input failure must have a pending input kind");
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                state.pending_interrupt = false;
                let session = &mut state.snapshot;
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
            | ControlObservation::ProviderGap { .. }
            | ControlObservation::CapabilitiesProbed { .. }
            | ControlObservation::CapabilityProbeFailed { .. }
            | ControlObservation::HistoryDiscovered { .. }
            | ControlObservation::HistoryLoaded { .. }
            | ControlObservation::HistoryFailed { .. }
            | ControlObservation::ResumeAuthorized { .. }
            | ControlObservation::ResumeDenied { .. }
            | ControlObservation::ResumeFailed { .. }
            | ControlObservation::InteractionResolutionCompleted { .. }
            | ControlObservation::InteractionResolutionFailed { .. } => {
                unreachable!("stream observations return before lifecycle event reduction")
            }
        };
        self.bump_revision();
        self.emit_event(None, instance_id, generation, event);
        self.emit_interaction_transitions(None, instance_id, generation, interaction_transitions);
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        ControlSnapshot {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            revision: self.revision,
            health: self.health(),
            sessions: self
                .sessions
                .values()
                .map(|state| state.snapshot.clone())
                .collect(),
        }
    }

    pub fn health(&self) -> ControlHealth {
        ControlHealth {
            operation_id_exhausted: self.next_operation_id.is_none(),
            event_sequence_exhausted: self.next_event_sequence.is_none(),
            revision_exhausted: self.revision == u64::MAX,
            provider_sequence_exhausted_sessions: u32::try_from(
                self.sessions
                    .values()
                    .filter(|state| state.snapshot.provider.sequence == u64::MAX)
                    .count(),
            )
            .expect("live session map is bounded below u32::MAX"),
            retained_instance_identities: u32::try_from(self.generation_watermarks.len())
                .expect("retained identity map is bounded below u32::MAX"),
            retained_instance_identity_capacity: CONTROL_INSTANCE_IDENTITIES_CAPACITY,
        }
    }

    pub fn session_snapshot(&self, instance_id: AgentInstanceId) -> Option<&SessionSnapshot> {
        self.sessions.get(&instance_id).map(|state| &state.snapshot)
    }

    pub fn session_instance_ids(&self) -> impl Iterator<Item = AgentInstanceId> + '_ {
        self.sessions.keys().copied()
    }

    pub fn record_command_rejection(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        message: String,
    ) {
        if self.next_event_sequence.is_none() {
            return;
        }
        let generation = self
            .sessions
            .get(&instance_id)
            .map(|state| state.snapshot.generation)
            .or_else(|| self.generation_watermarks.get(&instance_id).copied())
            .unwrap_or_default();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::CommandRejected { message },
        );
        debug_assert!(self.counter_error.is_none());
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
        runtime_policy: ProviderRuntimePolicy,
        mut request: StartRequest,
    ) -> Result<(), ControlError> {
        runtime_policy
            .validate()
            .map_err(|error| ControlError::InvalidProviderRuntimePolicy { error })?;
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
        if transport == TransportKind::Pty {
            require_runtime_capability(runtime_policy, ProviderRuntimeCapability::RawPtyLifecycle)?;
        }
        if let Some(operation_id) = session.snapshot.pending_operation {
            return Err(ControlError::OperationPending {
                instance_id,
                operation_id,
            });
        }
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
        if let Some(session_options) = &request.session_options {
            session_options
                .validate()
                .map_err(|error| ControlError::InvalidSessionOptions {
                    message: error.to_string(),
                })?;
        }
        if transport == TransportKind::Pipe
            && request.initial_prompt.as_deref().is_none_or(str::is_empty)
        {
            return Err(ControlError::MissingInitialPrompt);
        }
        if request.initial_prompt.as_deref().is_some_and(|prompt| !prompt.is_empty()) {
            require_structured_prompt_policy(runtime_policy)?;
        }

        let generation_watermark =
            self.generation_watermark(instance_id, session.snapshot.generation);
        let generation = checked_next_generation(generation_watermark).ok_or(
            ControlError::GenerationExhausted {
                instance_id,
                generation: generation_watermark,
            },
        )?;
        self.purge_generation_bound_history(instance_id);
        let operation_id = self.allocate_operation();
        let (agent_id, transport) = {
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.pending_terminal_size = Some(request.terminal_size);
            state.pending_interrupt = false;
            state.pending_resume_identity = None;
            state.runtime_policy = runtime_policy;
            let session = &mut state.snapshot;
            session.history = HistorySnapshot::default();
            session.resume = ResumeSnapshot::default();
            session.generation = generation;
            session.status = SessionStatus::Starting;
            session.pending_operation = Some(operation_id);
            session.pending_input = None;
            session.process_id = None;
            session.terminal_size = None;
            session.terminal_frame = None;
            session.terminal_stale = None;
            session.session_options = request.session_options.clone();
            session.foreground = ForegroundSnapshot::default();
            session.provider = ProviderSnapshot::default();
            (session.agent_id.clone(), session.transport)
        };
        self.generation_watermarks.insert(instance_id, generation);
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::Spawn {
                agent_id,
                transport,
                runtime_policy,
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
        let pending_resolution = self
            .sessions
            .get(&instance_id)
            .and_then(|state| pending_interaction_resolution(&state.snapshot));
        if !matches!(status, SessionStatus::Starting | SessionStatus::Running) {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "stop".to_owned(),
                status,
            });
        }

        let operation_id = self.allocate_operation();
        let generation = {
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.pending_interrupt = false;
            let session = &mut state.snapshot;
            session.status = SessionStatus::Stopping;
            session.pending_operation = Some(operation_id);
            session.pending_input = None;
            invalidate_foreground(session, "stop requested".to_owned());
            session.generation
        };
        if let Some((pending_operation_id, _)) = pending_resolution {
            self.effects.retain(|effect| {
                effect.instance_id != instance_id
                    || effect.generation != generation
                    || effect.operation_id != pending_operation_id
            });
        }
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
        let runtime_policy = state.runtime_policy;
        if matches!(
            &action,
            InputAction::InsertDraft(_)
                | InputAction::SubmitPrompt(_)
                | InputAction::AgentCommand(_)
        ) {
            require_structured_prompt_policy(runtime_policy)?;
        }
        let interrupt_requested = matches!(
            &action,
            InputAction::TerminalControl(TerminalControl::Interrupt)
        );
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
                    PreparedInputKind::TerminalText
                    | PreparedInputKind::TerminalBytes
                    | PreparedInputKind::TerminalControl => {
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
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.pending_interrupt = interrupt_requested;
            let session = &mut state.snapshot;
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

    fn discover_history(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        query: HistoryQuery,
    ) -> Result<(), ControlError> {
        query
            .validate()
            .map_err(|error| ControlError::InvalidHistoryRequest {
                message: error.to_string(),
            })?;
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if let Some(pending) = &state.snapshot.history.pending {
            return Err(ControlError::HistoryOperationPending {
                operation_id: pending.operation_id,
            });
        }
        let operation_id = self.allocate_operation();
        let (generation, agent_id, operation) = {
            let session = self.session_mut(instance_id);
            let operation = HistoryOperation::Discover {
                query: query.clone(),
            };
            session.history.pending = Some(PendingHistoryOperation {
                operation_id,
                operation: operation.clone(),
            });
            session.history.candidates.clear();
            session.history.loaded_candidate_id = None;
            session.history.loaded = None;
            session.history.last_error = None;
            (session.generation, session.agent_id.clone(), operation)
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::DiscoverHistory { agent_id, query },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::HistoryRequested {
                operation_id,
                operation,
            },
        );
        Ok(())
    }

    fn probe_capabilities(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        request: CapabilityProbeRequest,
    ) -> Result<(), ControlError> {
        request
            .validate()
            .map_err(|error| ControlError::InvalidCapabilityProbeRequest {
                message: error.to_string(),
            })?;
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if let Some(pending) = &state.snapshot.capabilities.pending {
            return Err(ControlError::CapabilityProbeOperationPending {
                operation_id: pending.operation_id,
            });
        }
        if state.snapshot.capabilities.settled {
            return Err(ControlError::CapabilityProbeSettled);
        }

        let operation_id = self.allocate_operation();
        let (generation, agent_id) = {
            let session = self.session_mut(instance_id);
            let generation = session.generation;
            session.capabilities.pending = Some(PendingCapabilityProbe {
                operation_id,
                generation,
                request: request.clone(),
            });
            session.capabilities.session_option_models.clear();
            session.capabilities.last_failure = None;
            (generation, session.agent_id.clone())
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::ProbeCapabilities { agent_id, request },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::CapabilityProbeRequested { operation_id },
        );
        Ok(())
    }

    fn load_history(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        candidate_id: String,
    ) -> Result<(), ControlError> {
        validate_candidate_id(&candidate_id).map_err(|error| {
            ControlError::InvalidHistoryRequest {
                message: error.to_string(),
            }
        })?;
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if let Some(pending) = &state.snapshot.history.pending {
            return Err(ControlError::HistoryOperationPending {
                operation_id: pending.operation_id,
            });
        }
        if state.snapshot.history.candidate(&candidate_id).is_none() {
            return Err(ControlError::UnknownHistoryCandidate);
        }
        let operation_id = self.allocate_operation();
        let (generation, agent_id, operation) = {
            let session = self.session_mut(instance_id);
            let operation = HistoryOperation::Load {
                candidate_id: candidate_id.clone(),
            };
            session.history.pending = Some(PendingHistoryOperation {
                operation_id,
                operation: operation.clone(),
            });
            session.history.loaded = None;
            session.history.loaded_candidate_id = None;
            session.history.last_error = None;
            (session.generation, session.agent_id.clone(), operation)
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::LoadHistory {
                agent_id,
                candidate_id,
            },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::HistoryRequested {
                operation_id,
                operation,
            },
        );
        Ok(())
    }

    fn resume(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        target: ResumeTarget,
        runtime_policy: ProviderRuntimePolicy,
        mut request: ResumeLaunchRequest,
    ) -> Result<(), ControlError> {
        runtime_policy
            .validate()
            .map_err(|error| ControlError::InvalidProviderRuntimePolicy { error })?;
        require_runtime_capability(
            runtime_policy,
            ProviderRuntimeCapability::ProviderSessionIdentity,
        )?;
        require_runtime_capability(runtime_policy, ProviderRuntimeCapability::SemanticResume)?;
        request.initial_prompt = request
            .initial_prompt
            .as_deref()
            .map(normalize_semantic_prompt)
            .transpose()
            .map_err(|error| ControlError::InvalidResumeRequest {
                message: error.to_string(),
            })?;
        request
            .validate()
            .map_err(|error| ControlError::InvalidResumeRequest {
                message: error.to_string(),
            })?;
        if request.initial_prompt.as_deref().is_some_and(|prompt| !prompt.is_empty()) {
            require_structured_prompt_policy(runtime_policy)?;
        }
        target
            .validate()
            .map_err(|error| ControlError::InvalidResumeRequest {
                message: error.to_string(),
            })?;
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if !matches!(
            state.snapshot.transport,
            TransportKind::Pty | TransportKind::Pipe
        ) {
            return Err(ControlError::UnsupportedTransportOperation {
                transport: state.snapshot.transport,
                action: "resume".to_owned(),
            });
        }
        if state.snapshot.transport == TransportKind::Pipe
            && request.initial_prompt.as_deref().is_none_or(str::is_empty)
        {
            return Err(ControlError::MissingInitialPrompt);
        }
        let status = state.snapshot.status.clone();
        if !matches!(
            status,
            SessionStatus::Registered | SessionStatus::Exited { .. } | SessionStatus::Failed { .. }
        ) {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "resume".to_owned(),
                status,
            });
        }
        if let Some(operation_id) = state.snapshot.pending_operation {
            return Err(ControlError::OperationPending {
                instance_id,
                operation_id,
            });
        }

        let authority_target = match &target {
            ResumeTarget::CurrentProvider => {
                let identity = state
                    .snapshot
                    .provider
                    .session
                    .clone()
                    .ok_or(ControlError::MissingProviderSession)?;
                identity
                    .validate()
                    .map_err(|error| ControlError::InvalidResumeRequest {
                        message: error.to_string(),
                    })?;
                ResumeAuthorityTarget::ProviderSession { identity }
            }
            ResumeTarget::ProviderSession { identity } => {
                identity
                    .validate()
                    .map_err(|error| ControlError::InvalidResumeRequest {
                        message: error.to_string(),
                    })?;
                ResumeAuthorityTarget::ProviderSession {
                    identity: identity.clone(),
                }
            }
            ResumeTarget::HistoryCandidate { candidate_id } => {
                if state.snapshot.history.candidate(candidate_id).is_none()
                    || state.snapshot.history.loaded_candidate_id.as_deref()
                        != Some(candidate_id.as_str())
                    || state.snapshot.history.loaded.is_none()
                {
                    return Err(ControlError::HistoryCandidateNotLoaded);
                }
                ResumeAuthorityTarget::HistoryCandidate {
                    candidate_id: candidate_id.clone(),
                }
            }
        };

        let operation_id = self.allocate_operation();
        let (generation, agent_id) = {
            let state = self
                .sessions
                .get_mut(&instance_id)
                .expect("validated session");
            state.pending_resume_identity = None;
            state.runtime_policy = runtime_policy;
            let session = &mut state.snapshot;
            session.pending_operation = Some(operation_id);
            session.resume.pending = Some(PendingResumeOperation {
                operation_id,
                target: target.clone(),
                request: request.clone(),
                phase: ResumePhase::Authorizing,
            });
            session.resume.last_error = None;
            (session.generation, session.agent_id.clone())
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::AuthorizeResume {
                agent_id,
                target: authority_target,
                request,
            },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::ResumeRequested {
                operation_id,
                target,
            },
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
        if current_source_sequence == u64::MAX {
            return Err(ControlError::ProviderSourceSequenceExhausted {
                instance_id,
                generation,
                provider_source: source,
            });
        }
        if source_sequence <= current_source_sequence {
            return Err(ControlError::StaleProviderSequence);
        }

        let missed = source_sequence
            .checked_sub(current_source_sequence)
            .and_then(|difference| difference.checked_sub(1))
            .expect("source sequence ordering was validated");
        if missed > 0
            || events
                .iter()
                .any(|event| !matches!(event, ProviderEvent::SessionIdentityObserved { .. }))
        {
            require_runtime_capability(
                state.runtime_policy,
                ProviderRuntimeCapability::SemanticReadiness,
            )?;
        }
        if events
            .iter()
            .any(provider_event_carries_session_identity)
        {
            require_runtime_capability(
                state.runtime_policy,
                ProviderRuntimeCapability::ProviderSessionIdentity,
            )?;
        }
        let canonical_steps = events.len() as u64 + u64::from(missed > 0);
        if state
            .snapshot
            .provider
            .sequence
            .checked_add(canonical_steps)
            .is_none()
        {
            return Err(ControlError::ProviderSequenceExhausted {
                instance_id,
                generation,
            });
        }
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
            let (reduction, superseded_resolution) = {
                let state = self
                    .sessions
                    .get_mut(&instance_id)
                    .expect("validated session");
                let pending_resolution = pending_interaction_resolution(&state.snapshot);
                let reduction = reduce_provider_event(
                    &mut state.snapshot.provider,
                    &source,
                    source_sequence,
                    event.clone(),
                );
                let superseded_resolution =
                    pending_resolution.filter(|(operation_id, interaction_id)| {
                        !interaction_resolution_is_pending(
                            &state.snapshot,
                            *operation_id,
                            *interaction_id,
                        )
                    });
                if superseded_resolution.is_some() {
                    state.snapshot.pending_operation = None;
                }
                (reduction, superseded_resolution)
            };
            if let Some((operation_id, _)) = superseded_resolution {
                self.effects.retain(|effect| {
                    effect.instance_id != instance_id
                        || effect.generation != generation
                        || effect.operation_id != operation_id
                });
            }
            self.bump_revision();
            self.emit_event(
                Some(command_id),
                instance_id,
                generation,
                ControlEventKind::ProviderEvent {
                    sequence: reduction.sequence,
                    source: source.clone(),
                    source_sequence,
                    event,
                },
            );
            self.emit_interaction_transitions(
                Some(command_id),
                instance_id,
                generation,
                reduction.interaction_transitions,
            );
        }
        Ok(())
    }

    fn resolve_interaction(
        &mut self,
        command_id: CommandId,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        interaction_id: ProviderInteractionId,
        response: ProviderInteractionResponse,
    ) -> Result<(), ControlError> {
        let state = self
            .sessions
            .get(&instance_id)
            .ok_or(ControlError::UnknownInstance { instance_id })?;
        if state.snapshot.generation != generation {
            return Err(ControlError::StaleProviderInteractionGeneration {
                expected: state.snapshot.generation,
                actual: generation,
            });
        }
        if state.snapshot.status != SessionStatus::Running {
            return Err(ControlError::InvalidTransition {
                instance_id,
                action: "resolve provider interaction".to_owned(),
                status: state.snapshot.status.clone(),
            });
        }
        if let Some(operation_id) = state.snapshot.pending_operation {
            return Err(ControlError::OperationPending {
                instance_id,
                operation_id,
            });
        }
        let interaction = state
            .snapshot
            .provider
            .interactions
            .iter()
            .find(|interaction| interaction.id == interaction_id)
            .ok_or(ControlError::UnknownProviderInteraction { interaction_id })?;
        if interaction.status != ProviderInteractionStatus::Pending {
            return Err(ControlError::ProviderInteractionNotPending { interaction_id });
        }
        response
            .validate_for(interaction.interaction_kind)
            .map_err(|error| ControlError::InvalidProviderInteractionResponse {
                message: error.to_string(),
            })?;
        let response_kind = response.kind();
        let target = ProviderInteractionTarget {
            interaction_id,
            source: interaction.source.clone(),
            provider_request_id: interaction.provider_request_id.clone(),
            interaction_kind: interaction.interaction_kind,
            tool_name: interaction.tool_name.clone(),
            agent_id: interaction.agent_id.clone(),
        };

        let operation_id = self.allocate_operation();
        let state = self
            .sessions
            .get_mut(&instance_id)
            .expect("validated session");
        state.snapshot.pending_operation = Some(operation_id);
        state
            .snapshot
            .provider
            .interactions
            .iter_mut()
            .find(|interaction| interaction.id == interaction_id)
            .expect("validated interaction")
            .status = ProviderInteractionStatus::Resolving {
            operation_id,
            response_kind,
        };
        self.effects.push(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id,
            instance_id,
            generation,
            effect: ControlEffect::ResolveInteraction { target, response },
        });
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::InteractionResolutionRequested {
                operation_id,
                interaction_id,
                response_kind,
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
        if let Some(operation_id) = state.snapshot.pending_operation {
            return Err(ControlError::OperationPending {
                instance_id,
                operation_id,
            });
        }
        if let Some(pending) = &state.snapshot.capabilities.pending {
            return Err(ControlError::CapabilityProbeOperationPending {
                operation_id: pending.operation_id,
            });
        }
        if let Some(pending) = &state.snapshot.history.pending {
            return Err(ControlError::HistoryOperationPending {
                operation_id: pending.operation_id,
            });
        }
        let generation = state.snapshot.generation;
        self.sessions.remove(&instance_id);
        self.effects
            .retain(|effect| effect.instance_id != instance_id);
        self.bump_revision();
        self.emit_event(
            Some(command_id),
            instance_id,
            generation,
            ControlEventKind::Removed,
        );
        Ok(())
    }

    fn generation_watermark(
        &self,
        instance_id: AgentInstanceId,
        current: SessionGeneration,
    ) -> SessionGeneration {
        self.generation_watermarks
            .get(&instance_id)
            .copied()
            .map_or(current, |watermark| watermark.max(current))
    }

    fn has_counter_headroom(&self) -> bool {
        self.counter_error.is_none()
            && self.next_operation_id.is_some()
            && self
                .next_event_sequence
                .is_some_and(|sequence| sequence.checked_add(CONTROL_EVENT_HEADROOM - 1).is_some())
            && self
                .revision
                .checked_add(CONTROL_REVISION_HEADROOM)
                .is_some()
    }

    fn observation_has_provider_headroom(&self, envelope: &ObservationEnvelope) -> bool {
        !matches!(
            envelope.observation,
            ControlObservation::ProviderEvent { .. } | ControlObservation::ProviderGap { .. }
        ) || self
            .sessions
            .get(&envelope.instance_id)
            .is_none_or(|state| state.snapshot.provider.sequence < u64::MAX)
    }

    fn retire_exhausted_counter(&mut self, error: &ControlError) {
        match error {
            ControlError::OperationIdExhausted => self.next_operation_id = None,
            ControlError::EventSequenceExhausted => self.next_event_sequence = None,
            ControlError::RevisionExhausted => self.revision = u64::MAX,
            _ => {}
        }
    }

    /// Starting a new generation invalidates history work, while host-scoped
    /// capability discovery intentionally remains valid across that boundary.
    fn purge_generation_bound_history(&mut self, instance_id: AgentInstanceId) {
        self.effects.retain(|effect| {
            effect.instance_id != instance_id
                || !matches!(
                    effect.effect,
                    ControlEffect::DiscoverHistory { .. } | ControlEffect::LoadHistory { .. }
                )
        });
    }

    fn allocate_operation(&mut self) -> OperationId {
        if self.counter_error.is_some() {
            return OperationId(0);
        }
        let Some(next) = self.next_operation_id.take() else {
            self.counter_error = Some(ControlError::OperationIdExhausted);
            return OperationId(0);
        };
        self.next_operation_id = next.checked_add(1);
        OperationId(next)
    }

    fn session_mut(&mut self, instance_id: AgentInstanceId) -> &mut SessionSnapshot {
        &mut self
            .sessions
            .get_mut(&instance_id)
            .expect("validated instance must remain registered")
            .snapshot
    }

    fn bump_revision(&mut self) {
        if self.counter_error.is_some() {
            return;
        }
        let Some(revision) = self.revision.checked_add(1) else {
            self.counter_error = Some(ControlError::RevisionExhausted);
            return;
        };
        self.revision = revision;
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

    fn emit_interaction_transitions(
        &mut self,
        command_id: Option<CommandId>,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        transitions: Vec<ProviderInteractionTransition>,
    ) {
        for transition in transitions {
            let event = match transition {
                ProviderInteractionTransition::Requested(interaction) => {
                    ControlEventKind::InteractionRequested { interaction }
                }
                ProviderInteractionTransition::Resolved {
                    interaction_id,
                    outcome,
                } => ControlEventKind::InteractionResolved {
                    interaction_id,
                    outcome,
                },
            };
            self.emit_event(command_id, instance_id, generation, event);
        }
    }

    fn emit_event(
        &mut self,
        command_id: Option<CommandId>,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        event: ControlEventKind,
    ) {
        if self.counter_error.is_some() {
            return;
        }
        let Some(sequence) = self.next_event_sequence.take() else {
            self.counter_error = Some(ControlError::EventSequenceExhausted);
            return;
        };
        self.next_event_sequence = sequence.checked_add(1);
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

fn checked_next_generation(current: SessionGeneration) -> Option<SessionGeneration> {
    current.0.checked_add(1).map(SessionGeneration)
}

fn invalidate_foreground(snapshot: &mut SessionSnapshot, reason: String) {
    snapshot.foreground.authority = ForegroundAuthority::Stale;
    snapshot.foreground.stale_reason = Some(reason);
}

fn is_history_observation(observation: &ControlObservation) -> bool {
    matches!(
        observation,
        ControlObservation::HistoryDiscovered { .. }
            | ControlObservation::HistoryLoaded { .. }
            | ControlObservation::HistoryFailed { .. }
    )
}

fn is_capability_observation(observation: &ControlObservation) -> bool {
    matches!(
        observation,
        ControlObservation::CapabilitiesProbed { .. }
            | ControlObservation::CapabilityProbeFailed { .. }
    )
}

fn is_resume_authority_observation(observation: &ControlObservation) -> bool {
    matches!(
        observation,
        ControlObservation::ResumeAuthorized { .. }
            | ControlObservation::ResumeDenied { .. }
            | ControlObservation::ResumeFailed { .. }
    )
}

fn require_runtime_capability(
    runtime_policy: ProviderRuntimePolicy,
    capability: ProviderRuntimeCapability,
) -> Result<(), ControlError> {
    if runtime_policy.admits(capability) {
        Ok(())
    } else {
        Err(ControlError::ProviderRuntimePolicyDenied { capability })
    }
}

fn require_structured_prompt_policy(
    runtime_policy: ProviderRuntimePolicy,
) -> Result<(), ControlError> {
    require_runtime_capability(
        runtime_policy,
        ProviderRuntimeCapability::SemanticReadiness,
    )?;
    require_runtime_capability(runtime_policy, ProviderRuntimeCapability::StructuredPrompt)
}

fn denied_observation_capability(
    runtime_policy: ProviderRuntimePolicy,
    observation: &ControlObservation,
) -> Option<ProviderRuntimeCapability> {
    let capability = match observation {
        ControlObservation::ProviderEvent {
            event: ProviderEvent::SessionIdentityObserved { .. },
            ..
        } => ProviderRuntimeCapability::ProviderSessionIdentity,
        ControlObservation::ProviderEvent { event, .. } => {
            if !runtime_policy.admits(ProviderRuntimeCapability::SemanticReadiness) {
                ProviderRuntimeCapability::SemanticReadiness
            } else if provider_event_carries_session_identity(event)
                && !runtime_policy.admits(ProviderRuntimeCapability::ProviderSessionIdentity)
            {
                ProviderRuntimeCapability::ProviderSessionIdentity
            } else {
                return None;
            }
        }
        ControlObservation::ProviderGap { .. } => ProviderRuntimeCapability::SemanticReadiness,
        ControlObservation::ResumeAuthorized { .. }
        | ControlObservation::ResumeDenied { .. }
        | ControlObservation::ResumeFailed { .. } => ProviderRuntimeCapability::SemanticResume,
        _ => return None,
    };
    (!runtime_policy.admits(capability)).then_some(capability)
}

fn provider_event_carries_session_identity(event: &ProviderEvent) -> bool {
    matches!(
        event,
        ProviderEvent::SessionStarted { .. } | ProviderEvent::SessionIdentityObserved { .. }
    )
}

fn is_interaction_resolution_observation(observation: &ControlObservation) -> bool {
    matches!(
        observation,
        ControlObservation::InteractionResolutionCompleted { .. }
            | ControlObservation::InteractionResolutionFailed { .. }
    )
}

fn interaction_failure_is_valid(message: &str) -> bool {
    !message.trim().is_empty()
        && message.len() <= PROVIDER_INTERACTION_FAILURE_MAX_BYTES
        && !message
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn interaction_response_outcome(
    response_kind: ProviderInteractionResponseKind,
) -> ProviderInteractionOutcome {
    match response_kind {
        ProviderInteractionResponseKind::ApproveOnce => ProviderInteractionOutcome::Approved,
        ProviderInteractionResponseKind::Deny => ProviderInteractionOutcome::Denied,
        ProviderInteractionResponseKind::Answer => ProviderInteractionOutcome::Answered,
    }
}

fn interaction_is_unresolved(interaction: &ProviderInteraction) -> bool {
    matches!(
        interaction.status,
        ProviderInteractionStatus::Pending | ProviderInteractionStatus::Resolving { .. }
    )
}

fn pending_interaction_resolution(
    snapshot: &SessionSnapshot,
) -> Option<(OperationId, ProviderInteractionId)> {
    let operation_id = snapshot.pending_operation?;
    snapshot
        .provider
        .interactions
        .iter()
        .find_map(|interaction| {
            matches!(
                interaction.status,
                ProviderInteractionStatus::Resolving {
                    operation_id: interaction_operation_id,
                    ..
                } if interaction_operation_id == operation_id
            )
            .then_some((operation_id, interaction.id))
        })
}

fn interaction_resolution_is_pending(
    snapshot: &SessionSnapshot,
    operation_id: OperationId,
    interaction_id: ProviderInteractionId,
) -> bool {
    snapshot.provider.interactions.iter().any(|interaction| {
        interaction.id == interaction_id
            && matches!(
                interaction.status,
                ProviderInteractionStatus::Resolving {
                    operation_id: interaction_operation_id,
                    ..
                } if interaction_operation_id == operation_id
            )
    })
}

fn resume_identity_matches_target(
    snapshot: &SessionSnapshot,
    target: &ResumeTarget,
    identity: &ProviderSessionIdentity,
) -> bool {
    match target {
        ResumeTarget::CurrentProvider => snapshot.provider.session.as_ref() == Some(identity),
        ResumeTarget::ProviderSession { identity: expected } => expected == identity,
        ResumeTarget::HistoryCandidate { candidate_id } => {
            snapshot.history.loaded_candidate_id.as_deref() == Some(candidate_id.as_str())
                && snapshot
                    .history
                    .loaded
                    .as_ref()
                    .is_some_and(|session| session.session_id == identity.id)
        }
    }
}

fn history_candidates_are_valid(
    candidates: &[gate4agent_types::HistoryCandidateSummary],
    limit: u16,
) -> bool {
    if candidates.len() > usize::from(limit)
        || candidates
            .iter()
            .any(|candidate| candidate.validate().is_err())
    {
        return false;
    }
    let unique = candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect::<BTreeSet<_>>();
    unique.len() == candidates.len()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ProviderInteractionTransition {
    Requested(ProviderInteraction),
    Resolved {
        interaction_id: ProviderInteractionId,
        outcome: ProviderInteractionOutcome,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderReduction {
    sequence: u64,
    interaction_transitions: Vec<ProviderInteractionTransition>,
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
) -> ProviderReduction {
    let canonical_sequence = snapshot
        .sequence
        .checked_add(1)
        .expect("provider sequence capacity must be preflighted");
    let mut interaction_transitions = Vec::new();
    match &event {
        ProviderEvent::SessionStarted {
            session_id,
            model,
            tools,
        } => {
            snapshot.session = Some(ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: session_id.clone(),
                transcript_path: None,
            });
            snapshot.model = (!model.is_empty()).then(|| model.clone());
            snapshot.tools = tools.clone();
            snapshot.lead_activity = ProviderActivity::Idle;
            snapshot.current_prompt = None;
            snapshot.active_tools.clear();
            remove_source_subagents(snapshot, source);
            interaction_transitions.extend(resolve_source_pending_interactions(
                snapshot,
                source,
                ProviderInteractionOutcome::TurnEnded,
            ));
        }
        ProviderEvent::SessionIdentityObserved { identity } => {
            snapshot.session = Some(identity.clone());
        }
        ProviderEvent::TurnStarted { prompt } => {
            interaction_transitions.extend(resolve_source_pending_interactions(
                snapshot,
                source,
                ProviderInteractionOutcome::Superseded,
            ));
            snapshot.lead_activity = ProviderActivity::Working;
            snapshot.current_prompt = prompt.clone();
            snapshot.active_tools.clear();
        }
        ProviderEvent::WorkingObserved => {
            interaction_transitions.extend(resolve_source_pending_interactions_by_kind(
                snapshot, source,
            ));
            snapshot.lead_activity = ProviderActivity::Working;
        }
        ProviderEvent::ToolStarted {
            id,
            name,
            input_json,
            agent_id,
        } => {
            let resume_activity =
                matching_interaction_resume_activity(snapshot, source, id, agent_id.as_deref());
            let resolved =
                resolve_matching_provider_interactions(snapshot, source, id, agent_id.as_deref());
            if !resolved.is_empty() {
                snapshot.lead_activity = resume_activity.unwrap_or(ProviderActivity::Working);
            } else if agent_id.is_none() {
                snapshot.lead_activity = ProviderActivity::Working;
            }
            interaction_transitions.extend(resolved);
            if agent_id.is_none() {
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
        }
        ProviderEvent::ToolCompleted { id, agent_id, .. } => {
            let resume_activity =
                matching_interaction_resume_activity(snapshot, source, id, agent_id.as_deref());
            let resolved =
                resolve_matching_provider_interactions(snapshot, source, id, agent_id.as_deref());
            if !resolved.is_empty() {
                snapshot.lead_activity = resume_activity.unwrap_or(ProviderActivity::Working);
            }
            interaction_transitions.extend(resolved);
            if agent_id.is_none() {
                snapshot.active_tools.retain(|tool| tool.id != *id);
            }
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
            snapshot.lead_activity = ProviderActivity::Idle;
            snapshot.current_prompt = None;
            snapshot.active_tools.clear();
            interaction_transitions.extend(resolve_source_pending_interactions(
                snapshot,
                source,
                ProviderInteractionOutcome::TurnEnded,
            ));
        }
        ProviderEvent::TurnInterrupted => {
            snapshot.lead_activity = ProviderActivity::Idle;
            snapshot.current_prompt = None;
            snapshot.active_tools.clear();
            interaction_transitions.extend(resolve_source_pending_interactions(
                snapshot,
                source,
                ProviderInteractionOutcome::Interrupted,
            ));
        }
        ProviderEvent::InteractionRequested {
            request_id,
            interaction_kind,
            tool_name,
            prompt,
            agent_id,
        } => {
            let inherited_resume_activity = request_id.as_deref().and_then(|request_id| {
                matching_interaction_resume_activity(
                    snapshot,
                    source,
                    request_id,
                    agent_id.as_deref(),
                )
            });
            if let Some(request_id) = request_id {
                interaction_transitions.extend(resolve_provider_request_interactions(
                    snapshot,
                    source,
                    request_id,
                    agent_id.as_deref(),
                    ProviderInteractionOutcome::Superseded,
                ));
            }
            let interaction = ProviderInteraction {
                id: ProviderInteractionId(canonical_sequence),
                source: source.clone(),
                provider_request_id: request_id.clone(),
                interaction_kind: *interaction_kind,
                tool_name: tool_name.clone(),
                prompt: prompt.clone(),
                agent_id: agent_id.clone(),
                resume_lead_activity: agent_id
                    .is_some()
                    .then_some(inherited_resume_activity.unwrap_or(snapshot.lead_activity)),
                status: ProviderInteractionStatus::Pending,
            };
            push_provider_interaction(snapshot, interaction.clone(), &mut interaction_transitions);
            interaction_transitions.push(ProviderInteractionTransition::Requested(interaction));
            snapshot.lead_activity = ProviderActivity::WaitingForInput;
        }
        ProviderEvent::RateLimited { .. } | ProviderEvent::Error { .. } => {
            snapshot.lead_activity = ProviderActivity::Blocked;
        }
        ProviderEvent::Ready => {
            snapshot.lead_activity = if snapshot.interactions.iter().any(interaction_is_unresolved)
            {
                ProviderActivity::WaitingForInput
            } else {
                ProviderActivity::Idle
            };
        }
        ProviderEvent::SessionEnded { .. } => {
            snapshot.lead_activity = ProviderActivity::Idle;
            snapshot.current_prompt = None;
            snapshot.active_tools.clear();
            remove_source_subagents(snapshot, source);
            interaction_transitions.extend(resolve_source_pending_interactions(
                snapshot,
                source,
                ProviderInteractionOutcome::TurnEnded,
            ));
        }
        ProviderEvent::SubagentStarted {
            agent_id,
            agent_type,
            description,
        } => {
            if let Some(existing) = snapshot.subagents.iter_mut().find(|subagent| {
                subagent.source == *source && subagent.provider_agent_id == *agent_id
            }) {
                existing.agent_type = agent_type.clone().or(existing.agent_type.take());
                existing.description = description.clone().or(existing.description.take());
            } else if snapshot.subagents.len() < PROVIDER_SUBAGENTS_MAX {
                snapshot.subagents.push(ProviderSubagent {
                    source: source.clone(),
                    provider_agent_id: agent_id.clone(),
                    agent_type: agent_type.clone(),
                    description: description.clone(),
                });
            }
        }
        ProviderEvent::SubagentStopped { agent_id } => {
            let resume_activity = subagent_interaction_resume_activity(snapshot, source, agent_id);
            interaction_transitions.extend(resolve_subagent_pending_interactions(
                snapshot,
                source,
                agent_id,
                ProviderInteractionOutcome::TurnEnded,
            ));
            if let Some(resume_activity) = resume_activity {
                snapshot.lead_activity = resume_activity;
            }
            snapshot.subagents.retain(|subagent| {
                subagent.source != *source || subagent.provider_agent_id != *agent_id
            });
        }
        ProviderEvent::Text { .. } | ProviderEvent::Thinking { .. } => {}
    }
    refresh_provider_activity(snapshot);
    provider_source_cursor_mut(snapshot, source).sequence = source_sequence;
    provider_source_cursor_mut(snapshot, source).stale = false;
    snapshot.sequence = canonical_sequence;
    snapshot.last_event = Some(event);
    snapshot.stale = snapshot.sources.iter().any(|cursor| cursor.stale);
    ProviderReduction {
        sequence: canonical_sequence,
        interaction_transitions,
    }
}

fn refresh_provider_activity(snapshot: &mut ProviderSnapshot) {
    snapshot.activity =
        if snapshot.lead_activity == ProviderActivity::Idle && !snapshot.subagents.is_empty() {
            ProviderActivity::Working
        } else {
            snapshot.lead_activity
        };
}

fn remove_source_subagents(snapshot: &mut ProviderSnapshot, source: &ProviderSource) {
    snapshot
        .subagents
        .retain(|subagent| subagent.source != *source);
}

fn push_provider_interaction(
    snapshot: &mut ProviderSnapshot,
    interaction: ProviderInteraction,
    transitions: &mut Vec<ProviderInteractionTransition>,
) {
    if snapshot.interactions.len() >= PROVIDER_INTERACTIONS_MAX {
        let remove_index = snapshot
            .interactions
            .iter()
            .position(|existing| {
                matches!(existing.status, ProviderInteractionStatus::Resolved { .. })
            })
            .unwrap_or(0);
        let removed = snapshot.interactions.remove(remove_index);
        if interaction_is_unresolved(&removed) {
            transitions.push(ProviderInteractionTransition::Resolved {
                interaction_id: removed.id,
                outcome: ProviderInteractionOutcome::Superseded,
            });
        }
    }
    snapshot.interactions.push(interaction);
}

fn resolve_matching_provider_interactions(
    snapshot: &mut ProviderSnapshot,
    source: &ProviderSource,
    provider_request_id: &str,
    agent_id: Option<&str>,
) -> Vec<ProviderInteractionTransition> {
    let matching: Vec<_> = snapshot
        .interactions
        .iter()
        .filter_map(|interaction| {
            (interaction.source == *source
                && interaction.provider_request_id.as_deref() == Some(provider_request_id)
                && interaction.agent_id.as_deref() == agent_id)
                .then(|| {
                    provider_progress_outcome(interaction).map(|outcome| (interaction.id, outcome))
                })
                .flatten()
        })
        .collect();
    resolve_interaction_ids(snapshot, matching)
}

fn matching_interaction_resume_activity(
    snapshot: &ProviderSnapshot,
    source: &ProviderSource,
    provider_request_id: &str,
    agent_id: Option<&str>,
) -> Option<ProviderActivity> {
    snapshot.interactions.iter().rev().find_map(|interaction| {
        (interaction.source == *source
            && interaction.provider_request_id.as_deref() == Some(provider_request_id)
            && interaction.agent_id.as_deref() == agent_id
            && interaction_is_unresolved(interaction))
        .then_some(interaction.resume_lead_activity)
        .flatten()
    })
}

fn resolve_provider_request_interactions(
    snapshot: &mut ProviderSnapshot,
    source: &ProviderSource,
    provider_request_id: &str,
    agent_id: Option<&str>,
    outcome: ProviderInteractionOutcome,
) -> Vec<ProviderInteractionTransition> {
    let matching = snapshot
        .interactions
        .iter()
        .filter(|interaction| {
            interaction.source == *source
                && interaction.provider_request_id.as_deref() == Some(provider_request_id)
                && interaction.agent_id.as_deref() == agent_id
                && interaction_is_unresolved(interaction)
        })
        .map(|interaction| (interaction.id, outcome))
        .collect();
    resolve_interaction_ids(snapshot, matching)
}

fn resolve_source_pending_interactions(
    snapshot: &mut ProviderSnapshot,
    source: &ProviderSource,
    outcome: ProviderInteractionOutcome,
) -> Vec<ProviderInteractionTransition> {
    let matching = snapshot
        .interactions
        .iter()
        .filter(|interaction| {
            interaction.source == *source && interaction_is_unresolved(interaction)
        })
        .map(|interaction| (interaction.id, outcome))
        .collect();
    resolve_interaction_ids(snapshot, matching)
}

fn resolve_source_pending_interactions_by_kind(
    snapshot: &mut ProviderSnapshot,
    source: &ProviderSource,
) -> Vec<ProviderInteractionTransition> {
    let matching = snapshot
        .interactions
        .iter()
        .filter_map(|interaction| {
            (interaction.source == *source)
                .then(|| {
                    provider_progress_outcome(interaction).map(|outcome| (interaction.id, outcome))
                })
                .flatten()
        })
        .collect::<Vec<_>>();
    resolve_interaction_ids(snapshot, matching)
}

fn provider_progress_outcome(
    interaction: &ProviderInteraction,
) -> Option<ProviderInteractionOutcome> {
    match interaction.status {
        ProviderInteractionStatus::Pending => Some(match interaction.interaction_kind {
            ProviderInteractionKind::Approval => ProviderInteractionOutcome::Approved,
            ProviderInteractionKind::Question => ProviderInteractionOutcome::Answered,
        }),
        ProviderInteractionStatus::Resolving { response_kind, .. } => {
            Some(interaction_response_outcome(response_kind))
        }
        ProviderInteractionStatus::Resolved { .. } => None,
    }
}

fn resolve_subagent_pending_interactions(
    snapshot: &mut ProviderSnapshot,
    source: &ProviderSource,
    agent_id: &str,
    outcome: ProviderInteractionOutcome,
) -> Vec<ProviderInteractionTransition> {
    let matching = snapshot
        .interactions
        .iter()
        .filter(|interaction| {
            interaction.source == *source
                && interaction.agent_id.as_deref() == Some(agent_id)
                && interaction_is_unresolved(interaction)
        })
        .map(|interaction| (interaction.id, outcome))
        .collect();
    resolve_interaction_ids(snapshot, matching)
}

fn subagent_interaction_resume_activity(
    snapshot: &ProviderSnapshot,
    source: &ProviderSource,
    agent_id: &str,
) -> Option<ProviderActivity> {
    snapshot.interactions.iter().find_map(|interaction| {
        (interaction.source == *source
            && interaction.agent_id.as_deref() == Some(agent_id)
            && interaction_is_unresolved(interaction))
        .then_some(interaction.resume_lead_activity)
        .flatten()
    })
}

fn resolve_all_pending_interactions(
    snapshot: &mut ProviderSnapshot,
    outcome: ProviderInteractionOutcome,
) -> Vec<ProviderInteractionTransition> {
    let matching = snapshot
        .interactions
        .iter()
        .filter(|interaction| interaction_is_unresolved(interaction))
        .map(|interaction| (interaction.id, outcome))
        .collect();
    resolve_interaction_ids(snapshot, matching)
}

fn resolve_interaction_ids(
    snapshot: &mut ProviderSnapshot,
    matching: Vec<(ProviderInteractionId, ProviderInteractionOutcome)>,
) -> Vec<ProviderInteractionTransition> {
    let mut transitions = Vec::with_capacity(matching.len());
    for (interaction_id, outcome) in matching {
        if let Some(interaction) = snapshot
            .interactions
            .iter_mut()
            .find(|interaction| interaction.id == interaction_id)
        {
            interaction.status = ProviderInteractionStatus::Resolved { outcome };
            transitions.push(ProviderInteractionTransition::Resolved {
                interaction_id,
                outcome,
            });
        }
    }
    transitions
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
    snapshot.sequence = snapshot
        .sequence
        .checked_add(1)
        .expect("provider sequence capacity must be preflighted");
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
        CapabilityModelSummary, ForegroundAuthority, ForegroundProcess, ForegroundProcessKind,
        HistoryCandidateSummary, HistoryMessageRecord, HistoryMessageRole, HistoryQuery,
        HistorySessionRecord, InputAction, PreparedInputKind, PromptFraming, PromptPayload,
        SessionOptionSelection, ShellCommand, TerminalFrame, TerminalText, TransportKind,
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

    fn register_instance(command_id: u64, instance_id: AgentInstanceId) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(command_id),
            command: ControlCommand::Register {
                instance_id,
                agent_id: AgentId::new("claude").unwrap(),
                transport: TransportKind::Pty,
            },
        }
    }

    fn register(command_id: u64) -> CommandEnvelope {
        register_instance(command_id, instance())
    }

    fn verified_runtime_policy() -> ProviderRuntimePolicy {
        ProviderRuntimePolicy::new(true, true, true, true, true).unwrap()
    }

    fn start(command_id: u64) -> CommandEnvelope {
        start_with_policy(command_id, verified_runtime_policy())
    }

    fn start_with_policy(
        command_id: u64,
        runtime_policy: ProviderRuntimePolicy,
    ) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(command_id),
            command: ControlCommand::Start {
                instance_id: instance(),
                runtime_policy,
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
        }
    }

    fn terminal_text(command_id: u64, text: &str) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(command_id),
            command: ControlCommand::SendInput {
                instance_id: instance(),
                action: InputAction::TerminalText(TerminalText {
                    text: text.to_owned(),
                }),
            },
        }
    }

    fn running_engine() -> (Gate4AgentEngine, EffectEnvelope) {
        running_engine_with_policy(verified_runtime_policy())
    }

    fn running_engine_with_policy(
        runtime_policy: ProviderRuntimePolicy,
    ) -> (Gate4AgentEngine, EffectEnvelope) {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine.drain_events();
        engine.apply_command(start_with_policy(2, runtime_policy)).unwrap();
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
    fn runtime_policy_keeps_raw_input_and_rejects_structured_prompt() {
        let (mut engine, spawn) = running_engine_with_policy(ProviderRuntimePolicy::raw_pty());
        assert!(matches!(
            &spawn.effect,
            ControlEffect::Spawn { runtime_policy, .. }
                if *runtime_policy == ProviderRuntimePolicy::raw_pty()
        ));
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::TerminalBytes(vec![0x1b, b'[', b'A']),
                },
            })
            .unwrap();
        let raw_input = engine.drain_effects().pop().unwrap();
        assert!(matches!(raw_input.effect, ControlEffect::WriteInput { .. }));
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(raw_input.operation_id),
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::InputCompleted,
        });

        assert_eq!(
            engine.apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(4),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::SubmitPrompt(PromptPayload {
                        text: "semantic prompt".to_owned(),
                        framing: PromptFraming::Literal,
                    }),
                },
            }),
            Err(ControlError::ProviderRuntimePolicyDenied {
                capability: ProviderRuntimeCapability::SemanticReadiness,
            })
        );
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn runtime_policy_fail_closed_provider_observations_and_ingress() {
        let (mut engine, spawn) = running_engine_with_policy(ProviderRuntimePolicy::raw_pty());
        for (event, expected_capability) in [
            (
                ProviderEvent::WorkingObserved,
                ProviderRuntimeCapability::SemanticReadiness,
            ),
            (
                ProviderEvent::SessionIdentityObserved {
                    identity: ProviderSessionIdentity {
                        key: ProviderSessionKey::SessionId,
                        id: "provider-session".to_owned(),
                        transcript_path: None,
                    },
                },
                ProviderRuntimeCapability::ProviderSessionIdentity,
            ),
        ] {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: instance(),
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: 1,
                    event,
                },
            });
            assert!(engine.drain_events().iter().any(|event| matches!(
                event.event,
                ControlEventKind::ObservationIgnored {
                    reason: ObservationIgnoredReason::ProviderRuntimePolicyDenied { capability }
                } if capability == expected_capability
            )));
        }
        assert_eq!(engine.snapshot().sessions[0].provider.sequence, 0);

        assert_eq!(
            engine.apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(5),
                command: ControlCommand::IngestProvider {
                    instance_id: instance(),
                    generation: spawn.generation,
                    source: provider_source(),
                    source_sequence: 1,
                    events: vec![ProviderEvent::WorkingObserved],
                },
            }),
            Err(ControlError::ProviderRuntimePolicyDenied {
                capability: ProviderRuntimeCapability::SemanticReadiness,
            })
        );
        assert_eq!(
            engine.apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(6),
                command: ControlCommand::IngestProvider {
                    instance_id: instance(),
                    generation: spawn.generation,
                    source: provider_source(),
                    source_sequence: 1,
                    events: vec![ProviderEvent::SessionIdentityObserved {
                        identity: ProviderSessionIdentity {
                            key: ProviderSessionKey::SessionId,
                            id: "provider-session".to_owned(),
                            transcript_path: None,
                        },
                    }],
                },
            }),
            Err(ControlError::ProviderRuntimePolicyDenied {
                capability: ProviderRuntimeCapability::ProviderSessionIdentity,
            })
        );
    }

    #[test]
    fn runtime_policy_rejects_invalid_start_contract() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        let invalid = ProviderRuntimePolicy {
            raw_pty_lifecycle: false,
            semantic_readiness: true,
            structured_prompt: false,
            provider_session_identity: false,
            semantic_resume: false,
        };
        assert_eq!(
            engine.apply_command(start_with_policy(2, invalid)),
            Err(ControlError::InvalidProviderRuntimePolicy {
                error: gate4agent_types::ProviderRuntimePolicyError::SemanticCapabilityRequiresRawPty,
            })
        );
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn runtime_policy_requires_resume_identity_and_resume_capability() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        let request = ResumeLaunchRequest {
            working_directory: ".".to_owned(),
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            initial_prompt: None,
        };
        assert_eq!(
            engine.apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    target: ResumeTarget::ProviderSession {
                        identity: ProviderSessionIdentity {
                            key: ProviderSessionKey::SessionId,
                            id: "provider-session".to_owned(),
                            transcript_path: None,
                        },
                    },
                    runtime_policy: ProviderRuntimePolicy::raw_pty(),
                    request,
                },
            }),
            Err(ControlError::ProviderRuntimePolicyDenied {
                capability: ProviderRuntimeCapability::ProviderSessionIdentity,
            })
        );
        assert!(engine.drain_effects().is_empty());
    }

    fn resolving_interaction(
        interaction_kind: ProviderInteractionKind,
        response: ProviderInteractionResponse,
    ) -> (
        Gate4AgentEngine,
        EffectEnvelope,
        OperationId,
        ProviderInteractionId,
    ) {
        let (mut engine, spawn) = running_engine();
        let interaction_id = ProviderInteractionId(1);
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("request-1".to_owned()),
                    interaction_kind,
                    tool_name: "fixture-tool".to_owned(),
                    prompt: "continue?".to_owned(),
                    agent_id: None,
                },
            },
        });
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(90),
                command: ControlCommand::ResolveInteraction {
                    instance_id: instance(),
                    generation: spawn.generation,
                    interaction_id,
                    response,
                },
            })
            .unwrap();
        let operation_id = engine.snapshot().sessions[0]
            .pending_operation
            .expect("interaction resolution operation");
        (engine, spawn, operation_id, interaction_id)
    }

    fn assert_late_interaction_failure_is_ignored(
        engine: &mut Gate4AgentEngine,
        spawn: &EffectEnvelope,
        operation_id: OperationId,
        interaction_id: ProviderInteractionId,
        expected_outcome: ProviderInteractionOutcome,
    ) {
        engine.drain_events();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(operation_id),
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::InteractionResolutionFailed {
                interaction_id,
                message: "late executor failure".to_owned(),
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: expected_outcome,
            }
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::OperationMismatch,
            }
        )));
    }

    fn inactive_engine_with_provider_session() -> (Gate4AgentEngine, ProviderSessionIdentity) {
        let (mut engine, spawn) = running_engine();
        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "provider-session-1".to_owned(),
            transcript_path: None,
        };
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::SessionIdentityObserved {
                    identity: identity.clone(),
                },
            },
        });
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::ProcessExited {
                exit_code: Some(0),
                final_terminal: None,
            },
        });
        engine.drain_events();
        (engine, identity)
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
    fn capability_probe_settles_across_session_generation_without_blocking_start() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::ProbeCapabilities {
                    instance_id: instance(),
                    request: CapabilityProbeRequest {
                        working_directory: ".".to_owned(),
                    },
                },
            })
            .unwrap();
        let probe = engine.drain_effects().pop().unwrap();

        engine.apply_command(start(3)).unwrap();
        let spawn = engine.drain_effects().pop().unwrap();
        assert_ne!(probe.generation, spawn.generation);
        assert_eq!(
            engine.snapshot().sessions[0].status,
            SessionStatus::Starting
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(probe.operation_id),
            instance_id: probe.instance_id,
            generation: probe.generation,
            observation: ControlObservation::CapabilitiesProbed {
                session_option_models: vec![
                    CapabilityModelSummary {
                        id: "duplicate".to_owned(),
                        label: "Duplicate".to_owned(),
                    },
                    CapabilityModelSummary {
                        id: "duplicate".to_owned(),
                        label: "Duplicate again".to_owned(),
                    },
                ],
            },
        });
        assert!(engine.snapshot().sessions[0].capabilities.pending.is_some());

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(probe.operation_id),
            instance_id: probe.instance_id,
            generation: probe.generation,
            observation: ControlObservation::CapabilitiesProbed {
                session_option_models: vec![CapabilityModelSummary {
                    id: "account-model".to_owned(),
                    label: "Account model".to_owned(),
                }],
            },
        });
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.sessions[0].status, SessionStatus::Starting);
        assert_eq!(
            snapshot.sessions[0].pending_operation,
            Some(spawn.operation_id)
        );
        assert!(snapshot.sessions[0].capabilities.settled);
        assert_eq!(
            snapshot.sessions[0].capabilities.session_option_models[0].id,
            "account-model"
        );
        assert_eq!(
            engine
                .apply_command(CommandEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    id: CommandId(4),
                    command: ControlCommand::ProbeCapabilities {
                        instance_id: instance(),
                        request: CapabilityProbeRequest {
                            working_directory: ".".to_owned(),
                        },
                    },
                })
                .unwrap_err(),
            ControlError::CapabilityProbeSettled
        );
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
                    runtime_policy: verified_runtime_policy(),
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 1_001,
                            columns: 80,
                        },
                        initial_prompt: None,
                        session_options: None,
                    },
                },
            })
            .unwrap_err();
        assert_eq!(error, ControlError::InvalidTerminalSize);
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn invalid_session_options_are_rejected_before_effect_creation() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        let mut request = match start(2).command {
            ControlCommand::Start { request, .. } => request,
            _ => unreachable!(),
        };
        request.session_options = Some(SessionOptionSelection::new("bad\nmodel"));
        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::Start {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    request,
                },
            })
            .unwrap_err();
        assert!(matches!(error, ControlError::InvalidSessionOptions { .. }));
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
                agent_id: None,
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("tool-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "shell".to_owned(),
                prompt: "approve".to_owned(),
                agent_id: None,
            },
            ProviderEvent::ToolCompleted {
                id: "tool-1".to_owned(),
                output: "ok".to_owned(),
                is_error: false,
                duration_ms: None,
                agent_id: None,
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
                3 => {
                    assert!(provider.active_tools.is_empty());
                    assert_eq!(provider.activity, ProviderActivity::Working);
                    assert_eq!(
                        provider.interactions[0].status,
                        ProviderInteractionStatus::Resolved {
                            outcome: ProviderInteractionOutcome::Approved
                        }
                    );
                }
                4 | 5 => assert_eq!(provider.activity, ProviderActivity::Idle),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn provider_question_has_canonical_identity_and_matching_tool_resolution() {
        let (mut engine, spawn) = running_engine();
        engine.drain_events();
        let source = provider_source();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: source.clone(),
                sequence: 1,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("question-7".to_owned()),
                    interaction_kind: ProviderInteractionKind::Question,
                    tool_name: "AskUserQuestion".to_owned(),
                    prompt: "{\"question\":\"Continue?\"}".to_owned(),
                    agent_id: None,
                },
            },
        });

        let interaction = engine.snapshot().sessions[0].provider.interactions[0].clone();
        assert_eq!(interaction.id, ProviderInteractionId(1));
        assert_eq!(interaction.source, source);
        assert_eq!(interaction.status, ProviderInteractionStatus::Pending);
        assert!(engine.drain_events().iter().any(|event| matches!(
            &event.event,
            ControlEventKind::InteractionRequested { interaction: observed }
                if observed.id == interaction.id
        )));

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 2,
                event: ProviderEvent::ToolStarted {
                    id: "question-7".to_owned(),
                    name: "AskUserQuestion".to_owned(),
                    input_json: "{}".to_owned(),
                    agent_id: None,
                },
            },
        });

        let interaction = &engine.snapshot().sessions[0].provider.interactions[0];
        assert_eq!(
            interaction.status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Answered
            }
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::InteractionResolved {
                interaction_id: ProviderInteractionId(1),
                outcome: ProviderInteractionOutcome::Answered,
            }
        )));
    }

    #[test]
    fn interrupt_resolves_pending_interactions_only_after_effect_completion() {
        let (mut engine, spawn) = running_engine();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("approval-1".to_owned()),
                    interaction_kind: ProviderInteractionKind::Approval,
                    tool_name: "shell".to_owned(),
                    prompt: "cargo test".to_owned(),
                    agent_id: None,
                },
            },
        });
        engine.drain_events();

        let send_interrupt = |id| CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(id),
            command: ControlCommand::SendInput {
                instance_id: instance(),
                action: InputAction::TerminalControl(TerminalControl::Interrupt),
            },
        };
        engine.apply_command(send_interrupt(80)).unwrap();
        let failed = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(failed.operation_id),
            instance_id: failed.instance_id,
            generation: failed.generation,
            observation: ControlObservation::InputFailed {
                message: "write rejected".to_owned(),
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Pending
        );

        engine.apply_command(send_interrupt(81)).unwrap();
        let completed = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(completed.operation_id),
            instance_id: completed.instance_id,
            generation: completed.generation,
            observation: ControlObservation::InputCompleted,
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Interrupted
            }
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::InteractionResolved {
                interaction_id: ProviderInteractionId(1),
                outcome: ProviderInteractionOutcome::Interrupted,
            }
        )));
    }

    #[test]
    fn canonical_interaction_resolution_is_generation_checked_and_fail_closed() {
        let (mut engine, spawn) = running_engine();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("question-1".to_owned()),
                    interaction_kind: ProviderInteractionKind::Question,
                    tool_name: "AskUserQuestion".to_owned(),
                    prompt: "continue?".to_owned(),
                    agent_id: None,
                },
            },
        });
        engine.drain_events();
        let interaction_id = ProviderInteractionId(1);
        let resolve = |id, generation, response| CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(id),
            command: ControlCommand::ResolveInteraction {
                instance_id: instance(),
                generation,
                interaction_id,
                response,
            },
        };

        assert_eq!(
            engine
                .apply_command(resolve(
                    90,
                    SessionGeneration(spawn.generation.0.saturating_add(1)),
                    ProviderInteractionResponse::Answer {
                        text: "yes".to_owned(),
                    },
                ))
                .unwrap_err(),
            ControlError::StaleProviderInteractionGeneration {
                expected: spawn.generation,
                actual: SessionGeneration(spawn.generation.0.saturating_add(1)),
            }
        );
        assert!(engine.drain_effects().is_empty());
        assert!(matches!(
            engine
                .apply_command(resolve(
                    91,
                    spawn.generation,
                    ProviderInteractionResponse::ApproveOnce,
                ))
                .unwrap_err(),
            ControlError::InvalidProviderInteractionResponse { .. }
        ));

        engine
            .apply_command(resolve(
                92,
                spawn.generation,
                ProviderInteractionResponse::Answer {
                    text: "yes".to_owned(),
                },
            ))
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        assert!(matches!(
            &effect.effect,
            ControlEffect::ResolveInteraction {
                target,
                response: ProviderInteractionResponse::Answer { text },
            } if target.interaction_id == interaction_id
                && target.provider_request_id.as_deref() == Some("question-1")
                && text == "yes"
        ));
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Resolving {
                operation_id: effect.operation_id,
                response_kind: ProviderInteractionResponseKind::Answer,
            }
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::InteractionResolutionRequested {
                operation_id,
                interaction_id: ProviderInteractionId(1),
                response_kind: ProviderInteractionResponseKind::Answer,
            } if operation_id == effect.operation_id
        )));

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::InteractionResolutionCompleted {
                interaction_id: ProviderInteractionId(999),
            },
        });
        assert!(matches!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Resolving { .. }
        ));
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::InvalidInteractionObservation,
            }
        )));

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::InteractionResolutionFailed {
                interaction_id,
                message: "provider rejected response".to_owned(),
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Pending
        );
        assert_eq!(engine.snapshot().sessions[0].pending_operation, None);

        engine
            .apply_command(resolve(
                93,
                spawn.generation,
                ProviderInteractionResponse::Answer {
                    text: "yes".to_owned(),
                },
            ))
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::InteractionResolutionCompleted { interaction_id },
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Answered,
            }
        );
    }

    #[test]
    fn working_progress_settles_denied_resolution_before_late_failure() {
        let (mut engine, spawn, operation_id, interaction_id) = resolving_interaction(
            ProviderInteractionKind::Question,
            ProviderInteractionResponse::Deny,
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 2,
                event: ProviderEvent::WorkingObserved,
            },
        });

        assert_eq!(engine.snapshot().sessions[0].pending_operation, None);
        assert!(engine.drain_effects().is_empty());
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Denied,
            }
        );
        assert_late_interaction_failure_is_ignored(
            &mut engine,
            &spawn,
            operation_id,
            interaction_id,
            ProviderInteractionOutcome::Denied,
        );
    }

    #[test]
    fn exact_tool_progress_settles_resolution_before_late_failure() {
        for progress in [
            ProviderEvent::ToolStarted {
                id: "request-1".to_owned(),
                name: "fixture-tool".to_owned(),
                input_json: "{}".to_owned(),
                agent_id: None,
            },
            ProviderEvent::ToolCompleted {
                id: "request-1".to_owned(),
                output: "done".to_owned(),
                is_error: false,
                duration_ms: Some(1),
                agent_id: None,
            },
        ] {
            let (mut engine, spawn, operation_id, interaction_id) = resolving_interaction(
                ProviderInteractionKind::Approval,
                ProviderInteractionResponse::ApproveOnce,
            );
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: 2,
                    event: progress,
                },
            });

            assert_eq!(engine.snapshot().sessions[0].pending_operation, None);
            assert!(engine.drain_effects().is_empty());
            assert_eq!(
                engine.snapshot().sessions[0].provider.interactions[0].status,
                ProviderInteractionStatus::Resolved {
                    outcome: ProviderInteractionOutcome::Approved,
                }
            );
            assert_late_interaction_failure_is_ignored(
                &mut engine,
                &spawn,
                operation_id,
                interaction_id,
                ProviderInteractionOutcome::Approved,
            );
        }
    }

    #[test]
    fn terminal_transitions_supersede_in_flight_interaction_resolution() {
        for terminate_with_process_exit in [false, true] {
            let (mut engine, spawn) = running_engine();
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: 1,
                    event: ProviderEvent::InteractionRequested {
                        request_id: Some("approval-1".to_owned()),
                        interaction_kind: ProviderInteractionKind::Approval,
                        tool_name: "shell".to_owned(),
                        prompt: "approve".to_owned(),
                        agent_id: None,
                    },
                },
            });
            engine
                .apply_command(CommandEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    id: CommandId(94),
                    command: ControlCommand::ResolveInteraction {
                        instance_id: instance(),
                        generation: spawn.generation,
                        interaction_id: ProviderInteractionId(1),
                        response: ProviderInteractionResponse::ApproveOnce,
                    },
                })
                .unwrap();
            let operation_id = engine.snapshot().sessions[0].pending_operation.unwrap();

            if terminate_with_process_exit {
                engine.apply_observation(ObservationEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: None,
                    instance_id: spawn.instance_id,
                    generation: spawn.generation,
                    observation: ControlObservation::ProcessExited {
                        exit_code: Some(0),
                        final_terminal: None,
                    },
                });
            } else {
                engine.apply_observation(ObservationEnvelope {
                    protocol_version: CONTROL_PROTOCOL_VERSION,
                    operation_id: None,
                    instance_id: spawn.instance_id,
                    generation: spawn.generation,
                    observation: ControlObservation::ProviderEvent {
                        source: provider_source(),
                        sequence: 2,
                        event: ProviderEvent::TurnCompleted {
                            usage: TokenUsage::default(),
                            is_cumulative: false,
                        },
                    },
                });
            }

            assert_eq!(engine.snapshot().sessions[0].pending_operation, None);
            assert!(engine.drain_effects().is_empty());
            assert_eq!(
                engine.snapshot().sessions[0].provider.interactions[0].status,
                ProviderInteractionStatus::Resolved {
                    outcome: ProviderInteractionOutcome::TurnEnded,
                }
            );

            engine.drain_events();
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(operation_id),
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::InteractionResolutionCompleted {
                    interaction_id: ProviderInteractionId(1),
                },
            });
            assert!(engine.drain_events().iter().any(|event| matches!(
                event.event,
                ControlEventKind::ObservationIgnored {
                    reason: ObservationIgnoredReason::OperationMismatch,
                }
            )));
        }

        let (mut engine, spawn) = running_engine();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("approval-stop".to_owned()),
                    interaction_kind: ProviderInteractionKind::Approval,
                    tool_name: "shell".to_owned(),
                    prompt: "approve".to_owned(),
                    agent_id: None,
                },
            },
        });
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(95),
                command: ControlCommand::ResolveInteraction {
                    instance_id: instance(),
                    generation: spawn.generation,
                    interaction_id: ProviderInteractionId(1),
                    response: ProviderInteractionResponse::ApproveOnce,
                },
            })
            .unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(96),
                command: ControlCommand::Stop {
                    instance_id: instance(),
                    force: false,
                },
            })
            .unwrap();
        let effects = engine.drain_effects();
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].effect,
            ControlEffect::Stop { force: false }
        ));
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effects[0].operation_id),
            instance_id: effects[0].instance_id,
            generation: effects[0].generation,
            observation: ControlObservation::StopCompleted {
                forced: false,
                exit_code: Some(0),
                final_terminal: None,
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::TurnEnded,
            }
        );
    }

    #[test]
    fn generic_input_completion_never_resolves_provider_interactions() {
        let (mut engine, spawn) = running_engine();
        for (sequence, event) in [
            ProviderEvent::InteractionRequested {
                request_id: Some("approval-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "shell".to_owned(),
                prompt: "cargo test".to_owned(),
                agent_id: None,
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("question-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Question,
                tool_name: "AskUserQuestion".to_owned(),
                prompt: "{\"question\":\"Continue?\"}".to_owned(),
                agent_id: None,
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("question-2".to_owned()),
                interaction_kind: ProviderInteractionKind::Question,
                tool_name: "AskUserQuestion".to_owned(),
                prompt: "{\"question\":\"Use the latest answer?\"}".to_owned(),
                agent_id: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: u64::try_from(sequence + 1).unwrap(),
                    event,
                },
            });
        }
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 4,
                event: ProviderEvent::Ready,
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.activity,
            ProviderActivity::WaitingForInput
        );

        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(82),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::TerminalControl(TerminalControl::Enter),
                },
            })
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::InputCompleted,
        });

        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(83),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::SubmitPrompt(PromptPayload {
                        text: "continue".to_owned(),
                        framing: PromptFraming::Literal,
                    }),
                },
            })
            .unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(effect.operation_id),
            instance_id: effect.instance_id,
            generation: effect.generation,
            observation: ControlObservation::InputCompleted,
        });

        let snapshot = engine.snapshot();
        let interactions = &snapshot.sessions[0].provider.interactions;
        assert_eq!(interactions[0].status, ProviderInteractionStatus::Pending);
        assert_eq!(interactions[1].status, ProviderInteractionStatus::Pending);
        assert_eq!(interactions[2].status, ProviderInteractionStatus::Pending);
        assert_eq!(
            snapshot.sessions[0].provider.activity,
            ProviderActivity::WaitingForInput
        );
    }

    #[test]
    fn child_owned_interaction_restores_lead_state_without_adopting_child_activity() {
        let (mut engine, spawn) = running_engine();
        let source = provider_source();
        let events = [
            ProviderEvent::TurnCompleted {
                usage: TokenUsage::default(),
                is_cumulative: false,
            },
            ProviderEvent::SubagentStarted {
                agent_id: "child-1".to_owned(),
                agent_type: Some("reviewer".to_owned()),
                description: None,
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("question-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Question,
                tool_name: "AskUserQuestion".to_owned(),
                prompt: "{\"question\":\"Continue?\"}".to_owned(),
                agent_id: Some("child-1".to_owned()),
            },
        ];
        for (index, event) in events.into_iter().enumerate() {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: source.clone(),
                    sequence: u64::try_from(index + 1).unwrap(),
                    event,
                },
            });
        }
        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(provider.lead_activity, ProviderActivity::WaitingForInput);
        assert_eq!(
            provider.interactions[0].resume_lead_activity,
            Some(ProviderActivity::Idle)
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: source.clone(),
                sequence: 4,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("question-1".to_owned()),
                    interaction_kind: ProviderInteractionKind::Question,
                    tool_name: "AskUserQuestion".to_owned(),
                    prompt: "{\"question\":\"Continue?\"}".to_owned(),
                    agent_id: Some("child-1".to_owned()),
                },
            },
        });
        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(
            provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Superseded
            }
        );
        assert_eq!(
            provider.interactions[1].resume_lead_activity,
            Some(ProviderActivity::Idle)
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: source.clone(),
                sequence: 5,
                event: ProviderEvent::ToolStarted {
                    id: "question-1".to_owned(),
                    name: "AskUserQuestion".to_owned(),
                    input_json: "{}".to_owned(),
                    agent_id: Some("child-1".to_owned()),
                },
            },
        });
        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(provider.lead_activity, ProviderActivity::Idle);
        assert_eq!(provider.activity, ProviderActivity::Working);
        assert!(provider.active_tools.is_empty());
        assert_eq!(
            provider.interactions[1].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Answered
            }
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: source.clone(),
                sequence: 6,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("approval-2".to_owned()),
                    interaction_kind: ProviderInteractionKind::Approval,
                    tool_name: "shell".to_owned(),
                    prompt: "approve".to_owned(),
                    agent_id: Some("child-1".to_owned()),
                },
            },
        });

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: source.clone(),
                sequence: 7,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("question-3".to_owned()),
                    interaction_kind: ProviderInteractionKind::Question,
                    tool_name: "AskUserQuestion".to_owned(),
                    prompt: "{\"question\":\"Another?\"}".to_owned(),
                    agent_id: Some("child-1".to_owned()),
                },
            },
        });

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source,
                sequence: 8,
                event: ProviderEvent::SubagentStopped {
                    agent_id: "child-1".to_owned(),
                },
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0].provider.activity,
            ProviderActivity::Idle
        );
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[2].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::TurnEnded
            }
        );
        assert_eq!(
            engine.snapshot().sessions[0].provider.interactions[3].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::TurnEnded
            }
        );
    }

    #[test]
    fn provider_session_identity_observation_does_not_reset_live_turn_state() {
        let (mut engine, spawn) = running_engine();
        for (sequence, event) in [
            ProviderEvent::SubagentStarted {
                agent_id: "child-1".to_owned(),
                agent_type: None,
                description: None,
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("approval-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "shell".to_owned(),
                prompt: "approve".to_owned(),
                agent_id: Some("child-1".to_owned()),
            },
            ProviderEvent::SessionIdentityObserved {
                identity: ProviderSessionIdentity {
                    key: ProviderSessionKey::SessionId,
                    id: "provider-session-1".to_owned(),
                    transcript_path: Some("C:/sessions/provider-session-1.jsonl".to_owned()),
                },
            },
        ]
        .into_iter()
        .enumerate()
        {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: u64::try_from(sequence + 1).unwrap(),
                    event,
                },
            });
        }
        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(
            provider.session,
            Some(ProviderSessionIdentity {
                key: ProviderSessionKey::SessionId,
                id: "provider-session-1".to_owned(),
                transcript_path: Some("C:/sessions/provider-session-1.jsonl".to_owned()),
            })
        );
        assert_eq!(provider.activity, ProviderActivity::WaitingForInput);
        assert_eq!(provider.subagents.len(), 1);
        assert_eq!(provider.interactions.len(), 1);
        assert_eq!(
            provider.interactions[0].status,
            ProviderInteractionStatus::Pending
        );
    }

    #[test]
    fn working_observation_resolves_input_and_preserves_live_turn_context() {
        let (mut engine, spawn) = running_engine();
        for (sequence, event) in [
            ProviderEvent::TurnStarted {
                prompt: Some("ship the fix".to_owned()),
            },
            ProviderEvent::ToolStarted {
                id: "tool-1".to_owned(),
                name: "bash".to_owned(),
                input_json: "{\"command\":\"cargo check\"}".to_owned(),
                agent_id: None,
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("approval-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "bash".to_owned(),
                prompt: "approve cargo check".to_owned(),
                agent_id: None,
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("question-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Question,
                tool_name: "AskUserQuestion".to_owned(),
                prompt: "continue?".to_owned(),
                agent_id: None,
            },
            ProviderEvent::WorkingObserved,
        ]
        .into_iter()
        .enumerate()
        {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: u64::try_from(sequence + 1).unwrap(),
                    event,
                },
            });
        }

        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(provider.activity, ProviderActivity::Working);
        assert_eq!(provider.current_prompt.as_deref(), Some("ship the fix"));
        assert_eq!(provider.active_tools.len(), 1);
        assert_eq!(
            provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Approved
            }
        );
        assert_eq!(
            provider.interactions[1].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Answered
            }
        );
    }

    #[test]
    fn provider_interruption_ends_the_turn_without_counting_completion() {
        let (mut engine, spawn) = running_engine();
        for (sequence, event) in [
            ProviderEvent::TurnStarted {
                prompt: Some("cancel me".to_owned()),
            },
            ProviderEvent::InteractionRequested {
                request_id: Some("approval-1".to_owned()),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "bash".to_owned(),
                prompt: "approve".to_owned(),
                agent_id: None,
            },
            ProviderEvent::TurnInterrupted,
        ]
        .into_iter()
        .enumerate()
        {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: u64::try_from(sequence + 1).unwrap(),
                    event,
                },
            });
        }
        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(provider.activity, ProviderActivity::Idle);
        assert_eq!(provider.completed_turns, 0);
        assert_eq!(provider.current_prompt, None);
        assert_eq!(
            provider.interactions[0].status,
            ProviderInteractionStatus::Resolved {
                outcome: ProviderInteractionOutcome::Interrupted
            }
        );
    }

    #[test]
    fn live_subagent_gates_lead_completion_until_exact_stop() {
        let (mut engine, spawn) = running_engine();
        for (sequence, event) in [
            ProviderEvent::SubagentStarted {
                agent_id: "child-1".to_owned(),
                agent_type: Some("reviewer".to_owned()),
                description: Some("review the reducer".to_owned()),
            },
            ProviderEvent::TurnCompleted {
                usage: TokenUsage::default(),
                is_cumulative: false,
            },
        ]
        .into_iter()
        .enumerate()
        {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: u64::try_from(sequence + 1).unwrap(),
                    event,
                },
            });
        }
        let provider = &engine.snapshot().sessions[0].provider;
        assert_eq!(provider.lead_activity, ProviderActivity::Idle);
        assert_eq!(provider.activity, ProviderActivity::Working);
        assert_eq!(provider.subagents.len(), 1);

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: spawn.instance_id,
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 3,
                event: ProviderEvent::SubagentStopped {
                    agent_id: "child-1".to_owned(),
                },
            },
        });
        let provider = &engine.snapshot().sessions[0].provider;
        assert!(provider.subagents.is_empty());
        assert_eq!(provider.activity, ProviderActivity::Idle);
    }

    #[test]
    fn pending_interaction_roster_is_bounded_with_explicit_supersession() {
        let (mut engine, spawn) = running_engine();
        engine.drain_events();
        for index in 0..=PROVIDER_INTERACTIONS_MAX {
            engine.apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: spawn.instance_id,
                generation: spawn.generation,
                observation: ControlObservation::ProviderEvent {
                    source: provider_source(),
                    sequence: u64::try_from(index + 1).unwrap(),
                    event: ProviderEvent::InteractionRequested {
                        request_id: Some(format!("approval-{index}")),
                        interaction_kind: ProviderInteractionKind::Approval,
                        tool_name: "shell".to_owned(),
                        prompt: String::new(),
                        agent_id: None,
                    },
                },
            });
        }

        let snapshot = engine.snapshot();
        assert_eq!(
            snapshot.sessions[0].provider.interactions.len(),
            PROVIDER_INTERACTIONS_MAX
        );
        assert_eq!(
            snapshot.sessions[0].provider.interactions[0].id,
            ProviderInteractionId(2)
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::InteractionResolved {
                interaction_id: ProviderInteractionId(1),
                outcome: ProviderInteractionOutcome::Superseded,
            }
        )));
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
                            agent_id: None,
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
            scrollback_formatted: Vec::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: Default::default(),
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
                    scrollback_formatted: Vec::new(),
                    alternate_screen: false,
                    mouse_protocol_enabled: false,
                    mouse_protocol_encoding: Default::default(),
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
    fn remove_and_reregister_strictly_advance_the_generation_watermark() {
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
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::Remove {
                    instance_id: instance(),
                },
            })
            .unwrap();
        engine.apply_command(register(4)).unwrap();

        let generation = engine.snapshot().sessions[0].generation;
        assert_eq!(generation.0, first_spawn.generation.0 + 1);
        assert_eq!(
            engine.generation_watermarks.get(&instance()),
            Some(&generation)
        );
    }

    #[test]
    fn reregister_generation_exhaustion_is_atomic() {
        let mut engine = Gate4AgentEngine::new();
        let exhausted = SessionGeneration(u64::MAX);
        engine.generation_watermarks.insert(instance(), exhausted);
        let before = engine.clone();

        let error = engine.apply_command(register(1)).unwrap_err();

        assert_eq!(
            error,
            ControlError::GenerationExhausted {
                instance_id: instance(),
                generation: exhausted,
            }
        );
        assert_eq!(engine, before);
    }

    #[test]
    fn live_session_capacity_rejection_is_atomic() {
        let mut engine = Gate4AgentEngine::new();
        let first_instance = 10_000_u64;
        for offset in 0..CONTROL_SESSIONS_MAX {
            engine
                .apply_command(register_instance(
                    offset as u64 + 1,
                    AgentInstanceId(first_instance + offset as u64),
                ))
                .unwrap();
        }
        engine.drain_events();
        let before = engine.clone();
        let rejected_instance = AgentInstanceId(first_instance + CONTROL_SESSIONS_MAX as u64);

        let error = engine
            .apply_command(register_instance(10_000, rejected_instance))
            .unwrap_err();

        assert_eq!(
            error,
            ControlError::SessionCapacityExceeded {
                instance_id: rejected_instance,
                max: CONTROL_SESSIONS_MAX,
            }
        );
        assert_eq!(engine, before);
    }

    #[test]
    fn retained_identity_capacity_is_bounded_without_weakening_stale_fence() {
        let mut engine = Gate4AgentEngine::new();
        let first_instance = 20_000_u64;
        for offset in 0..CONTROL_INSTANCE_IDENTITIES_MAX {
            engine.generation_watermarks.insert(
                AgentInstanceId(first_instance + offset as u64),
                SessionGeneration::default(),
            );
        }
        let health = engine.snapshot().health;
        assert_eq!(
            health.retained_instance_identities,
            u32::try_from(CONTROL_INSTANCE_IDENTITIES_MAX).unwrap()
        );
        assert_eq!(
            health.retained_instance_identity_capacity,
            u32::try_from(CONTROL_INSTANCE_IDENTITIES_MAX).unwrap()
        );
        let known_instance = AgentInstanceId(first_instance);
        let rejected_instance =
            AgentInstanceId(first_instance + CONTROL_INSTANCE_IDENTITIES_MAX as u64);
        let before = engine.clone();

        let error = engine
            .apply_command(register_instance(1, rejected_instance))
            .unwrap_err();

        assert_eq!(
            error,
            ControlError::InstanceIdentityCapacityExceeded {
                instance_id: rejected_instance,
                max: CONTROL_INSTANCE_IDENTITIES_MAX,
            }
        );
        assert_eq!(engine, before);

        engine
            .apply_command(register_instance(2, known_instance))
            .unwrap();
        let current = engine
            .session_snapshot(known_instance)
            .expect("known retained identity must remain reusable")
            .clone();
        assert_eq!(current.generation, SessionGeneration(1));
        engine.drain_events();

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: known_instance,
            generation: SessionGeneration::default(),
            observation: ControlObservation::ProcessExited {
                exit_code: Some(9),
                final_terminal: None,
            },
        });

        assert_eq!(engine.session_snapshot(known_instance), Some(&current));
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::StaleGeneration
            }
        )));
    }

    #[test]
    fn operation_id_max_is_issued_once_and_later_commands_fail_atomically() {
        let (mut engine, spawn) = running_engine();
        engine.drain_events();
        engine.next_operation_id = Some(u64::MAX);
        engine.apply_command(terminal_text(3, "first")).unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        assert_eq!(effect.operation_id, OperationId(u64::MAX));
        engine
            .try_apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(effect.operation_id),
                instance_id: instance(),
                generation: spawn.generation,
                observation: ControlObservation::InputCompleted,
            })
            .unwrap();
        let before = engine.clone();

        let error = engine
            .apply_command(terminal_text(4, "second"))
            .unwrap_err();

        assert_eq!(error, ControlError::OperationIdExhausted);
        assert_eq!(engine, before);
        assert!(engine.snapshot().health.operation_id_exhausted);
    }

    #[test]
    fn event_sequence_max_is_emitted_once_and_observation_failure_is_atomic() {
        let (mut engine, spawn) = running_engine();
        engine.drain_events();
        engine.next_event_sequence = Some(u64::MAX);
        engine.apply_command(terminal_text(3, "first")).unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        let before = engine.clone();

        let error = engine
            .try_apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(effect.operation_id),
                instance_id: instance(),
                generation: spawn.generation,
                observation: ControlObservation::InputCompleted,
            })
            .unwrap_err();

        assert_eq!(error, ControlError::EventSequenceExhausted);
        assert_eq!(engine, before);
        assert!(engine.snapshot().health.event_sequence_exhausted);
        let events = engine.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, u64::MAX);
    }

    #[test]
    fn revision_max_is_published_once_and_later_observation_is_atomic() {
        let (mut engine, spawn) = running_engine();
        engine.drain_events();
        engine.revision = u64::MAX - 1;
        engine.apply_command(terminal_text(3, "first")).unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        assert_eq!(engine.snapshot().revision, u64::MAX);
        let before = engine.clone();

        let error = engine
            .try_apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(effect.operation_id),
                instance_id: instance(),
                generation: spawn.generation,
                observation: ControlObservation::InputCompleted,
            })
            .unwrap_err();

        assert_eq!(error, ControlError::RevisionExhausted);
        assert_eq!(engine, before);
        assert!(engine.snapshot().health.revision_exhausted);
    }

    #[test]
    fn provider_sequence_capacity_is_preflighted_for_the_whole_ingress_batch() {
        let (mut engine, spawn) = running_engine();
        engine.session_mut(instance()).provider.sequence = u64::MAX - 1;
        let before = engine.clone();

        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::IngestProvider {
                    instance_id: instance(),
                    generation: spawn.generation,
                    source: provider_source(),
                    source_sequence: 2,
                    events: vec![ProviderEvent::Text {
                        text: "bounded".to_owned(),
                        is_delta: false,
                    }],
                },
            })
            .unwrap_err();

        assert_eq!(
            error,
            ControlError::ProviderSequenceExhausted {
                instance_id: instance(),
                generation: spawn.generation,
            }
        );
        assert_eq!(engine, before);
    }

    #[test]
    fn provider_sequence_terminal_state_is_observable_and_blocks_observations() {
        let (mut engine, spawn) = running_engine();
        engine.session_mut(instance()).provider.sequence = u64::MAX;
        let before = engine.clone();
        assert_eq!(
            engine
                .snapshot()
                .health
                .provider_sequence_exhausted_sessions,
            1
        );

        let error = engine
            .try_apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: None,
                instance_id: instance(),
                generation: spawn.generation,
                observation: ControlObservation::ProviderGap {
                    source: provider_source(),
                    missed: 1,
                },
            })
            .unwrap_err();

        assert_eq!(
            error,
            ControlError::ProviderSequenceExhausted {
                instance_id: instance(),
                generation: spawn.generation,
            }
        );
        assert_eq!(engine, before);
    }

    #[test]
    fn provider_source_sequence_exhaustion_is_typed_and_atomic() {
        let (mut engine, spawn) = running_engine();
        let source = provider_source();
        engine
            .session_mut(instance())
            .provider
            .sources
            .push(ProviderSourceCursor {
                source: source.clone(),
                sequence: u64::MAX,
                gap_count: 0,
                stale: false,
            });
        let before = engine.clone();

        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::IngestProvider {
                    instance_id: instance(),
                    generation: spawn.generation,
                    source: source.clone(),
                    source_sequence: u64::MAX,
                    events: vec![ProviderEvent::WorkingObserved],
                },
            })
            .unwrap_err();

        assert_eq!(
            error,
            ControlError::ProviderSourceSequenceExhausted {
                instance_id: instance(),
                generation: spawn.generation,
                provider_source: source,
            }
        );
        assert_eq!(engine, before);
    }

    #[test]
    fn removed_lifecycle_observations_cannot_mutate_a_reregistered_instance() {
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
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::Remove {
                    instance_id: instance(),
                },
            })
            .unwrap();
        engine.apply_command(register(4)).unwrap();
        engine.apply_command(start(5)).unwrap();
        let second_spawn = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(second_spawn.operation_id),
            instance_id: instance(),
            generation: second_spawn.generation,
            observation: ControlObservation::Spawned {
                process_id: Some(84),
            },
        });
        let before = engine.snapshot().sessions[0].clone();
        engine.drain_events();

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: first_spawn.generation,
            observation: ControlObservation::ProcessExited {
                exit_code: Some(9),
                final_terminal: None,
            },
        });
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: first_spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::SessionIdentityObserved {
                    identity: ProviderSessionIdentity {
                        key: ProviderSessionKey::SessionId,
                        id: "stale-provider-session".to_owned(),
                        transcript_path: None,
                    },
                },
            },
        });

        assert_eq!(engine.snapshot().sessions[0], before);
        let ignored = engine
            .drain_events()
            .into_iter()
            .filter(|event| {
                matches!(
                    event.event,
                    ControlEventKind::ObservationIgnored {
                        reason: ObservationIgnoredReason::StaleGeneration
                    }
                )
            })
            .count();
        assert_eq!(ignored, 2);
    }

    #[test]
    fn start_generation_exhaustion_is_atomic() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        let exhausted = SessionGeneration(u64::MAX);
        engine.session_mut(instance()).generation = exhausted;
        engine.generation_watermarks.insert(instance(), exhausted);
        let before = engine.clone();

        let error = engine.apply_command(start(2)).unwrap_err();

        assert_eq!(
            error,
            ControlError::GenerationExhausted {
                instance_id: instance(),
                generation: exhausted,
            }
        );
        assert_eq!(engine, before);
    }

    #[test]
    fn start_generation_exhaustion_preserves_queued_history_effect() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: None,
                        limit: 1,
                    },
                },
            })
            .unwrap();
        let exhausted = SessionGeneration(u64::MAX);
        engine.session_mut(instance()).generation = exhausted;
        engine.generation_watermarks.insert(instance(), exhausted);
        let before = engine.clone();

        let error = engine.apply_command(start(3)).unwrap_err();

        assert_eq!(
            error,
            ControlError::GenerationExhausted {
                instance_id: instance(),
                generation: exhausted,
            }
        );
        assert_eq!(engine, before);
        assert!(matches!(
            engine.drain_effects().as_slice(),
            [EffectEnvelope {
                effect: ControlEffect::DiscoverHistory { .. },
                ..
            }]
        ));
    }

    #[test]
    fn multi_event_rollback_retires_the_event_sequence_terminally() {
        let (mut engine, spawn) = running_engine();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: None,
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::ProviderEvent {
                source: provider_source(),
                sequence: 1,
                event: ProviderEvent::InteractionRequested {
                    request_id: Some("approval-counter-boundary".to_owned()),
                    interaction_kind: ProviderInteractionKind::Approval,
                    tool_name: "shell".to_owned(),
                    prompt: "approve".to_owned(),
                    agent_id: None,
                },
            },
        });
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::SendInput {
                    instance_id: instance(),
                    action: InputAction::TerminalControl(TerminalControl::Interrupt),
                },
            })
            .unwrap();
        let interrupt = engine.drain_effects().pop().unwrap();
        engine.drain_events();
        engine.next_event_sequence = Some(u64::MAX);
        let before = engine.snapshot();

        let error = engine
            .try_apply_observation(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(interrupt.operation_id),
                instance_id: instance(),
                generation: spawn.generation,
                observation: ControlObservation::InputCompleted,
            })
            .unwrap_err();

        assert_eq!(error, ControlError::EventSequenceExhausted);
        let after = engine.snapshot();
        assert_eq!(after.sessions, before.sessions);
        assert_eq!(after.revision, before.revision);
        assert!(!before.health.event_sequence_exhausted);
        assert!(after.health.event_sequence_exhausted);
        assert!(engine.drain_events().is_empty());
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
    fn history_has_independent_correlation_and_full_snapshot_results() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine.drain_events();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: Some("/repo".to_owned()),
                        limit: 4,
                    },
                },
            })
            .unwrap();
        let discovery = engine.drain_effects().pop().unwrap();
        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.pending_operation, None);
        assert_eq!(
            session
                .history
                .pending
                .as_ref()
                .map(|pending| pending.operation_id),
            Some(discovery.operation_id)
        );

        let candidate = HistoryCandidateSummary {
            id: "hist_fixture_1".to_owned(),
            session_id_hint: "session-1".to_owned(),
            modified_at_unix_ms: Some(42),
        };
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(discovery.operation_id),
            instance_id: instance(),
            generation: discovery.generation,
            observation: ControlObservation::HistoryDiscovered {
                candidates: vec![candidate.clone()],
            },
        });
        assert_eq!(
            engine.snapshot().sessions[0].history.candidates,
            vec![candidate]
        );

        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::LoadHistory {
                    instance_id: instance(),
                    candidate_id: "hist_fixture_1".to_owned(),
                },
            })
            .unwrap();
        let load = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(load.operation_id),
            instance_id: instance(),
            generation: load.generation,
            observation: ControlObservation::HistoryLoaded {
                session: HistorySessionRecord {
                    session_id: "session-1".to_owned(),
                    title: Some("title".to_owned()),
                    cwd: Some("/repo".to_owned()),
                    model: Some("model".to_owned()),
                    message_count: 1,
                    total_tokens: 7,
                    messages: vec![HistoryMessageRecord {
                        role: HistoryMessageRole::User,
                        text: "hello".to_owned(),
                    }],
                },
            },
        });
        let history = &engine.snapshot().sessions[0].history;
        assert!(history.pending.is_none());
        assert_eq!(history.loaded.as_ref().unwrap().session_id, "session-1");
        assert!(engine.drain_events().iter().any(|event| matches!(
            &event.event,
            ControlEventKind::HistoryLoaded { session_id } if session_id == "session-1"
        )));
    }

    #[test]
    fn session_start_purges_queued_generation_bound_history_work() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: None,
                        limit: 4,
                    },
                },
            })
            .unwrap();
        let before_start = engine.snapshot().sessions[0].clone();
        let history_operation_id = before_start
            .history
            .pending
            .as_ref()
            .expect("history request must be pending")
            .operation_id;
        engine.apply_command(start(3)).unwrap();
        let effects = engine.drain_effects();
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0].effect, ControlEffect::Spawn { .. }));
        let snapshot = engine.snapshot();
        assert!(snapshot.sessions[0].history.pending.is_none());
        assert!(snapshot.sessions[0].history.candidates.is_empty());

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(history_operation_id),
            instance_id: instance(),
            generation: before_start.generation,
            observation: ControlObservation::HistoryDiscovered {
                candidates: Vec::new(),
            },
        });
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::StaleGeneration
            }
        )));
    }

    #[test]
    fn remove_rejects_pending_capability_probe_without_dropping_its_effect() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::ProbeCapabilities {
                    instance_id: instance(),
                    request: CapabilityProbeRequest {
                        working_directory: ".".to_owned(),
                    },
                },
            })
            .unwrap();
        let pending = engine.snapshot().sessions[0]
            .capabilities
            .pending
            .as_ref()
            .expect("capability probe must be pending")
            .operation_id;
        let before = engine.clone();

        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::Remove {
                    instance_id: instance(),
                },
            })
            .unwrap_err();

        assert_eq!(
            error,
            ControlError::CapabilityProbeOperationPending {
                operation_id: pending,
            }
        );
        assert_eq!(engine, before);
        assert!(matches!(
            engine.drain_effects().as_slice(),
            [EffectEnvelope {
                effect: ControlEffect::ProbeCapabilities { .. },
                ..
            }]
        ));
    }

    #[test]
    fn remove_rejects_pending_history_without_dropping_its_effect() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: None,
                        limit: 1,
                    },
                },
            })
            .unwrap();
        let pending = engine.snapshot().sessions[0]
            .history
            .pending
            .as_ref()
            .expect("history request must be pending")
            .operation_id;
        let before = engine.clone();

        let error = engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::Remove {
                    instance_id: instance(),
                },
            })
            .unwrap_err();

        assert_eq!(
            error,
            ControlError::HistoryOperationPending {
                operation_id: pending,
            }
        );
        assert_eq!(engine, before);
        assert!(matches!(
            engine.drain_effects().as_slice(),
            [EffectEnvelope {
                effect: ControlEffect::DiscoverHistory { .. },
                ..
            }]
        ));
    }

    #[test]
    fn invalid_history_result_cannot_clear_the_correlated_operation() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: None,
                        limit: 1,
                    },
                },
            })
            .unwrap();
        let discovery = engine.drain_effects().pop().unwrap();

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(discovery.operation_id),
            instance_id: instance(),
            generation: discovery.generation,
            observation: ControlObservation::HistoryDiscovered {
                candidates: vec![
                    HistoryCandidateSummary {
                        id: "hist_duplicate".to_owned(),
                        session_id_hint: "session-1".to_owned(),
                        modified_at_unix_ms: None,
                    };
                    2
                ],
            },
        });

        let snapshot = engine.snapshot();
        assert_eq!(
            snapshot.sessions[0]
                .history
                .pending
                .as_ref()
                .map(|pending| pending.operation_id),
            Some(discovery.operation_id)
        );
        assert!(snapshot.sessions[0].history.candidates.is_empty());
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::InvalidHistoryObservation
            }
        )));
    }

    #[test]
    fn resume_is_authorized_before_a_new_generation_can_spawn() {
        let (mut engine, identity) = inactive_engine_with_provider_session();
        let previous_generation = engine.snapshot().sessions[0].generation;
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(10),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                },
            })
            .unwrap();
        let authorize = engine.drain_effects().pop().unwrap();
        assert_eq!(authorize.generation, previous_generation);
        assert!(matches!(
            &authorize.effect,
            ControlEffect::AuthorizeResume {
                target: ResumeAuthorityTarget::ProviderSession { identity: authorized },
                ..
            } if authorized == &identity
        ));
        assert_eq!(
            engine.snapshot().sessions[0]
                .resume
                .pending
                .as_ref()
                .map(|pending| pending.phase),
            Some(ResumePhase::Authorizing)
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeAuthorized {
                provider_session: identity.clone(),
            },
        });
        let spawn = engine.drain_effects().pop().unwrap();
        assert_eq!(spawn.operation_id, authorize.operation_id);
        assert_eq!(spawn.generation.0, previous_generation.0 + 1);
        assert!(matches!(
            &spawn.effect,
            ControlEffect::SpawnResume {
                transport: TransportKind::Pty,
                provider_session,
                runtime_policy,
                ..
            } if provider_session == &identity
                && *runtime_policy == verified_runtime_policy()
        ));
        assert_eq!(
            engine.snapshot().sessions[0].status,
            SessionStatus::Starting
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(spawn.operation_id),
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::Spawned {
                process_id: Some(77),
            },
        });
        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.status, SessionStatus::Running);
        assert!(session.resume.pending.is_none());
        assert_eq!(session.provider.session.as_ref(), Some(&identity));
        assert_eq!(
            session.resume.last_session,
            Some(ResumeSessionSummary::from(&identity))
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            &event.event,
            ControlEventKind::Resumed { session, process_id: Some(77) }
                if session.id == "provider-session-1"
        )));
    }

    #[test]
    fn explicit_provider_session_resume_still_requires_matching_authority() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine.drain_events();
        let identity = ProviderSessionIdentity {
            key: ProviderSessionKey::SessionId,
            id: "durable-provider-session".to_owned(),
            transcript_path: None,
        };
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::ProviderSession {
                        identity: identity.clone(),
                    },
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                },
            })
            .unwrap();
        let authorize = engine.drain_effects().pop().unwrap();
        assert!(matches!(
            &authorize.effect,
            ControlEffect::AuthorizeResume {
                target: ResumeAuthorityTarget::ProviderSession { identity: requested },
                ..
            } if requested == &identity
        ));
        assert_eq!(engine.snapshot().sessions[0].provider.session, None);
        assert_eq!(
            engine.snapshot().sessions[0].status,
            SessionStatus::Registered
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeAuthorized {
                provider_session: ProviderSessionIdentity {
                    key: ProviderSessionKey::SessionId,
                    id: "wrong-provider-session".to_owned(),
                    transcript_path: None,
                },
            },
        });
        assert!(engine.drain_effects().is_empty());
        assert_eq!(
            engine.snapshot().sessions[0].status,
            SessionStatus::Registered
        );

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeAuthorized {
                provider_session: identity.clone(),
            },
        });
        let spawn = engine.drain_effects().pop().unwrap();
        assert_eq!(spawn.generation, SessionGeneration(1));
        assert!(matches!(
            spawn.effect,
            ControlEffect::SpawnResume {
                provider_session,
                ..
            } if provider_session == identity
        ));
    }

    #[test]
    fn pipe_resume_requires_and_preserves_a_normalized_initial_prompt() {
        let (mut engine, identity) = inactive_engine_with_provider_session();
        engine.session_mut(instance()).transport = TransportKind::Pipe;

        let missing_prompt = engine.apply_command(CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(20),
            command: ControlCommand::Resume {
                instance_id: instance(),
                runtime_policy: verified_runtime_policy(),
                target: ResumeTarget::CurrentProvider,
                request: ResumeLaunchRequest {
                    working_directory: ".".to_owned(),
                    terminal_size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                    initial_prompt: None,
                },
            },
        });
        assert_eq!(missing_prompt, Err(ControlError::MissingInitialPrompt));
        assert!(engine.drain_effects().is_empty());

        engine.session_mut(instance()).transport = TransportKind::Acp;
        assert!(matches!(
            engine.apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(21),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: Some("continue".to_owned()),
                    },
                },
            }),
            Err(ControlError::UnsupportedTransportOperation {
                transport: TransportKind::Acp,
                ..
            })
        ));
        engine.session_mut(instance()).transport = TransportKind::Pipe;

        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(22),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: Some("continue\u{0000}now".to_owned()),
                    },
                },
            })
            .unwrap();
        let authorize = engine.drain_effects().pop().unwrap();
        assert!(matches!(
            &authorize.effect,
            ControlEffect::AuthorizeResume { request, .. }
                if request.initial_prompt.as_deref() == Some("continue<U+0000>now")
        ));

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeAuthorized {
                provider_session: identity,
            },
        });
        assert!(matches!(
            engine.drain_effects().pop().unwrap().effect,
            ControlEffect::SpawnResume {
                transport: TransportKind::Pipe,
                request,
                ..
            } if request.initial_prompt.as_deref() == Some("continue<U+0000>now")
        ));
    }

    #[test]
    fn resume_authorization_generation_exhaustion_is_atomic_and_ignored() {
        let (mut engine, identity) = inactive_engine_with_provider_session();
        let exhausted = SessionGeneration(u64::MAX);
        engine.session_mut(instance()).generation = exhausted;
        engine.generation_watermarks.insert(instance(), exhausted);
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(10),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                },
            })
            .unwrap();
        let authorize = engine.drain_effects().pop().unwrap();
        engine.drain_events();
        let before = engine.snapshot().sessions[0].clone();

        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeAuthorized {
                provider_session: identity,
            },
        });

        assert_eq!(engine.snapshot().sessions[0], before);
        assert_eq!(
            engine.generation_watermarks.get(&instance()),
            Some(&exhausted)
        );
        assert!(engine.drain_effects().is_empty());
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::GenerationExhausted
            }
        )));
    }

    #[test]
    fn resume_spawn_failure_retains_the_authorized_identity_for_retry() {
        let (mut engine, identity) = inactive_engine_with_provider_session();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(12),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                },
            })
            .unwrap();
        let authorize = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeAuthorized {
                provider_session: identity.clone(),
            },
        });
        let spawn = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(spawn.operation_id),
            instance_id: instance(),
            generation: spawn.generation,
            observation: ControlObservation::SpawnFailed {
                message: "controlled spawn failure".to_owned(),
            },
        });

        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.provider.session.as_ref(), Some(&identity));
        assert_eq!(
            session.resume.last_error.as_deref(),
            Some("controlled spawn failure")
        );
        assert!(session.resume.last_session.is_none());
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(13),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                },
            })
            .unwrap();
        assert!(matches!(
            engine.drain_effects().pop().unwrap().effect,
            ControlEffect::AuthorizeResume { .. }
        ));
    }

    #[test]
    fn resume_denial_preserves_the_inactive_session_generation_and_status() {
        let (mut engine, _) = inactive_engine_with_provider_session();
        let before = engine.snapshot().sessions[0].clone();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(11),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::CurrentProvider,
                    request: ResumeLaunchRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize {
                            rows: 24,
                            columns: 80,
                        },
                        initial_prompt: None,
                    },
                },
            })
            .unwrap();
        let authorize = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeDenied {
                reason: "vendor login is required".to_owned(),
            },
        });

        let session = &engine.snapshot().sessions[0];
        assert_eq!(session.generation, before.generation);
        assert_eq!(session.status, before.status);
        assert_eq!(
            session.resume.last_error.as_deref(),
            Some("vendor login is required")
        );
        assert!(session.resume.pending.is_none());
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn history_resume_requires_the_loaded_candidate_and_exact_parsed_session() {
        let mut engine = Gate4AgentEngine::new();
        engine.apply_command(register(1)).unwrap();
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(2),
                command: ControlCommand::DiscoverHistory {
                    instance_id: instance(),
                    query: HistoryQuery {
                        working_directory: None,
                        limit: 4,
                    },
                },
            })
            .unwrap();
        let discover = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(discover.operation_id),
            instance_id: instance(),
            generation: discover.generation,
            observation: ControlObservation::HistoryDiscovered {
                candidates: vec![HistoryCandidateSummary {
                    id: "hist_resume_1".to_owned(),
                    session_id_hint: "hint-only".to_owned(),
                    modified_at_unix_ms: None,
                }],
            },
        });
        let request = ResumeLaunchRequest {
            working_directory: ".".to_owned(),
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            initial_prompt: None,
        };
        assert_eq!(
            engine.apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(3),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::HistoryCandidate {
                        candidate_id: "hist_resume_1".to_owned(),
                    },
                    request: request.clone(),
                },
            }),
            Err(ControlError::HistoryCandidateNotLoaded)
        );

        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(4),
                command: ControlCommand::LoadHistory {
                    instance_id: instance(),
                    candidate_id: "hist_resume_1".to_owned(),
                },
            })
            .unwrap();
        let load = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(load.operation_id),
            instance_id: instance(),
            generation: load.generation,
            observation: ControlObservation::HistoryLoaded {
                session: HistorySessionRecord {
                    session_id: "parsed-session-1".to_owned(),
                    title: None,
                    cwd: None,
                    model: None,
                    message_count: 0,
                    total_tokens: 0,
                    messages: Vec::new(),
                },
            },
        });
        engine
            .apply_command(CommandEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                id: CommandId(5),
                command: ControlCommand::Resume {
                    instance_id: instance(),
                    runtime_policy: verified_runtime_policy(),
                    target: ResumeTarget::HistoryCandidate {
                        candidate_id: "hist_resume_1".to_owned(),
                    },
                    request,
                },
            })
            .unwrap();
        let authorize = engine.drain_effects().pop().unwrap();
        engine.apply_observation(ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(authorize.operation_id),
            instance_id: instance(),
            generation: authorize.generation,
            observation: ControlObservation::ResumeAuthorized {
                provider_session: ProviderSessionIdentity {
                    key: ProviderSessionKey::SessionId,
                    id: "wrong-session".to_owned(),
                    transcript_path: None,
                },
            },
        });
        assert!(engine.drain_effects().is_empty());
        assert_eq!(
            engine.snapshot().sessions[0]
                .resume
                .pending
                .as_ref()
                .map(|pending| pending.phase),
            Some(ResumePhase::Authorizing)
        );
        assert!(engine.drain_events().iter().any(|event| matches!(
            event.event,
            ControlEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::InvalidResumeObservation
            }
        )));
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
