use gate4agent_tool_protocol::*;
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

#[derive(Clone, Eq, PartialEq)]
struct RequestState {
    request: AcceptedRequest,
    snapshot: CapabilityRequestSnapshot,
}

#[derive(Clone, Eq, PartialEq)]
struct AcceptedRequest {
    key: CapabilityRequestKey,
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    provider_id: ToolProviderId,
    capability_id: ToolCapabilityId,
    resource_scope_id: ResourceScopeId,
    approval_summary: String,
    deadline_tick: u64,
    payload: Vec<u8>,
}

impl From<ConsumerBoundCapabilityRequest> for AcceptedRequest {
    fn from(envelope: ConsumerBoundCapabilityRequest) -> Self {
        Self {
            key: envelope.key(),
            instance_id: envelope.request.instance_id,
            generation: envelope.request.generation,
            provider_id: envelope.request.provider_id,
            capability_id: envelope.request.capability_id,
            resource_scope_id: envelope.request.resource_scope_id,
            approval_summary: envelope.request.approval_summary,
            deadline_tick: envelope.request.deadline_tick,
            payload: envelope.request.payload,
        }
    }
}

#[derive(Clone, Copy)]
enum RequestCloseKind {
    Instance,
    Client,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolEngineError {
    Validation(ToolValidationError),
    DuplicateProvider {
        provider_id: ToolProviderId,
    },
    DuplicateRequest {
        request_key: CapabilityRequestKey,
    },
    ProviderCapacityExceeded,
    PolicyCapacityExceeded,
    RequestCapacityExceeded,
    ClientRequestCapacityExceeded {
        consumer_id: ConsumerId,
        actor_id: ToolActorId,
        max: usize,
    },
    EffectCapacityExceeded,
    EffectSequenceExhausted,
    UnknownPolicyProvider {
        provider_id: ToolProviderId,
    },
    UnknownPolicyCapability {
        provider_id: ToolProviderId,
        capability_id: ToolCapabilityId,
    },
    ProviderOwnerMismatch {
        provider_id: ToolProviderId,
        owner: ConsumerId,
        requested_consumer: ConsumerId,
    },
    UnknownPolicyInstance {
        instance_id: AgentInstanceId,
    },
    InactivePolicyInstance {
        instance_id: AgentInstanceId,
    },
    PolicyGenerationMismatch {
        instance_id: AgentInstanceId,
        current: SessionGeneration,
        requested: SessionGeneration,
    },
    UnknownRequest {
        request_key: CapabilityRequestKey,
    },
    RequestNotAwaitingApproval {
        request_key: CapabilityRequestKey,
    },
    ApprovalScopeMismatch {
        request_key: CapabilityRequestKey,
    },
    ApprovalNonceMismatch {
        request_key: CapabilityRequestKey,
        expected: u64,
        actual: u64,
    },
    ApprovalGenerationStale {
        current: SessionGeneration,
        actual: SessionGeneration,
    },
    ApprovalDeadlineElapsed {
        request_key: CapabilityRequestKey,
    },
    ClockRegressed {
        current_tick: u64,
        requested_tick: u64,
    },
    GenerationRegressed {
        instance_id: AgentInstanceId,
        current: SessionGeneration,
        requested: SessionGeneration,
    },
    AuthoritySequenceRegressed {
        current: u64,
        requested: u64,
    },
    CounterExhausted {
        counter: &'static str,
    },
}

impl From<ToolValidationError> for ToolEngineError {
    fn from(error: ToolValidationError) -> Self {
        Self::Validation(error)
    }
}

impl fmt::Display for ToolEngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "tool engine rejected transition: {self:?}")
    }
}

impl std::error::Error for ToolEngineError {}

/// Pure single-writer authority for Gate-owned capability requests.
///
/// Callers advance a logical clock and session generations explicitly. The
/// engine performs no I/O; only drained effects may be handed to a provider.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolEngine {
    revision: u64,
    current_tick: u64,
    generations: BTreeMap<AgentInstanceId, SessionGeneration>,
    instance_states: BTreeMap<AgentInstanceId, ToolInstanceState>,
    providers: BTreeMap<ToolProviderId, CapabilityProviderDescriptor>,
    grants: BTreeMap<PolicyKey, GrantMode>,
    requests: BTreeMap<CapabilityRequestKey, RequestState>,
    effects: Vec<CapabilityEffectEnvelope>,
    completions: Vec<CapabilityCompletionEnvelope>,
    audit_events: VecDeque<ToolAuditEvent>,
    dropped_audit_events: u64,
    revision_overflow_count: u64,
    dropped_completions: u64,
    dropped_completions_since_drain: u64,
    last_authority_sequence: u64,
    effect_sequence_exhausted: bool,
    completion_sequence_exhausted: bool,
    audit_sequence_exhausted: bool,
    next_request_sequence: u64,
    next_operation_id: u64,
    next_effect_sequence: u64,
    next_completion_sequence: u64,
    next_audit_sequence: u64,
}

impl ToolEngine {
    pub fn new() -> Self {
        Self {
            revision: 0,
            current_tick: 0,
            generations: BTreeMap::new(),
            instance_states: BTreeMap::new(),
            providers: BTreeMap::new(),
            grants: BTreeMap::new(),
            requests: BTreeMap::new(),
            effects: Vec::new(),
            completions: Vec::new(),
            audit_events: VecDeque::new(),
            dropped_audit_events: 0,
            revision_overflow_count: 0,
            dropped_completions: 0,
            dropped_completions_since_drain: 0,
            last_authority_sequence: 0,
            effect_sequence_exhausted: false,
            completion_sequence_exhausted: false,
            audit_sequence_exhausted: false,
            next_request_sequence: 1,
            next_operation_id: 1,
            next_effect_sequence: 1,
            next_completion_sequence: 1,
            next_audit_sequence: 1,
        }
    }

    pub fn register_provider(
        &mut self,
        mut descriptor: CapabilityProviderDescriptor,
    ) -> Result<(), ToolEngineError> {
        descriptor.validate()?;
        if self.providers.contains_key(&descriptor.id) {
            return Err(ToolEngineError::DuplicateProvider {
                provider_id: descriptor.id,
            });
        }
        if self.providers.len() >= TOOL_PROVIDERS_MAX {
            return Err(ToolEngineError::ProviderCapacityExceeded);
        }
        descriptor
            .capabilities
            .sort_by(|left, right| left.id.cmp(&right.id));
        let provider_id = descriptor.id.clone();
        let owner = descriptor.owner.clone();
        let capability_count = descriptor.capabilities.len();
        self.providers.insert(provider_id.clone(), descriptor);
        self.bump_revision();
        self.emit_audit(
            None,
            ToolAuditEventKind::ProviderRegistered {
                provider_id,
                owner,
                capability_count,
            },
        );
        Ok(())
    }

    fn set_grant(&mut self, grant: PolicyGrant) -> Result<(), ToolEngineError> {
        let Some(provider) = self.providers.get(&grant.key.provider_id) else {
            return Err(ToolEngineError::UnknownPolicyProvider {
                provider_id: grant.key.provider_id,
            });
        };
        if !provider.has_capability(&grant.key.capability_id) {
            return Err(ToolEngineError::UnknownPolicyCapability {
                provider_id: grant.key.provider_id,
                capability_id: grant.key.capability_id,
            });
        }
        if let CapabilityOwner::Consumer(owner) = &provider.owner {
            if owner != &grant.key.consumer_id {
                return Err(ToolEngineError::ProviderOwnerMismatch {
                    provider_id: provider.id.clone(),
                    owner: owner.clone(),
                    requested_consumer: grant.key.consumer_id,
                });
            }
        }
        let Some(current_generation) = self.generations.get(&grant.key.instance_id).copied() else {
            return Err(ToolEngineError::UnknownPolicyInstance {
                instance_id: grant.key.instance_id,
            });
        };
        if self.instance_states.get(&grant.key.instance_id) != Some(&ToolInstanceState::Active) {
            return Err(ToolEngineError::InactivePolicyInstance {
                instance_id: grant.key.instance_id,
            });
        }
        if current_generation != grant.key.generation {
            return Err(ToolEngineError::PolicyGenerationMismatch {
                instance_id: grant.key.instance_id,
                current: current_generation,
                requested: grant.key.generation,
            });
        }
        let previous = self.grants.get(&grant.key).copied();
        if previous == Some(grant.mode) {
            return Ok(());
        }
        if previous.is_none() && self.grants.len() >= TOOL_POLICIES_MAX {
            return Err(ToolEngineError::PolicyCapacityExceeded);
        }
        if previous.is_some() {
            let targets = self.active_requests_for_key(&grant.key);
            for request_id in targets {
                self.revoke_request(&request_id);
            }
        }
        self.grants.insert(grant.key.clone(), grant.mode);
        self.bump_revision();
        self.emit_audit(None, ToolAuditEventKind::GrantSet { grant });
        Ok(())
    }

    fn revoke_grant(&mut self, key: &PolicyKey) -> Result<bool, ToolEngineError> {
        if !self.grants.contains_key(key) {
            return Ok(false);
        }
        let targets = self.active_requests_for_key(key);
        self.grants.remove(key);
        for request_id in targets {
            self.revoke_request(&request_id);
        }
        self.bump_revision();
        self.emit_audit(None, ToolAuditEventKind::GrantRevoked { key: key.clone() });
        Ok(true)
    }

    pub fn set_generation(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
    ) -> Result<(), ToolEngineError> {
        let previous = self.generations.get(&instance_id).copied();
        if let Some(current) = previous {
            if generation.0 < current.0 {
                return Err(ToolEngineError::GenerationRegressed {
                    instance_id,
                    current,
                    requested: generation,
                });
            }
            if generation == current {
                return Ok(());
            }
        }

        let stale_grants = self
            .grants
            .keys()
            .filter(|key| key.instance_id == instance_id && key.generation != generation)
            .cloned()
            .collect::<Vec<_>>();
        let purged_grant_count = stale_grants.len();
        for key in stale_grants {
            self.grants.remove(&key);
        }
        let targets = self
            .requests
            .iter()
            .filter_map(|(request_id, state)| {
                (state.request.instance_id == instance_id
                    && state.request.generation != generation
                    && !state.snapshot.status.is_terminal())
                .then_some(request_id.clone())
            })
            .collect::<Vec<_>>();
        self.generations.insert(instance_id, generation);
        self.instance_states
            .entry(instance_id)
            .or_insert(ToolInstanceState::Active);
        for request_id in targets {
            self.supersede_request(&request_id, generation);
        }
        self.bump_revision();
        self.emit_audit(
            None,
            ToolAuditEventKind::GenerationAdvanced {
                instance_id,
                previous,
                current: generation,
                purged_grant_count,
            },
        );
        Ok(())
    }

    pub fn set_instance_state(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        state: ToolInstanceState,
    ) -> Result<(), ToolEngineError> {
        let Some(current_generation) = self.generations.get(&instance_id).copied() else {
            return Err(ToolEngineError::UnknownPolicyInstance { instance_id });
        };
        if current_generation != generation {
            return Err(ToolEngineError::PolicyGenerationMismatch {
                instance_id,
                current: current_generation,
                requested: generation,
            });
        }
        if self.instance_states.get(&instance_id) == Some(&state) {
            return Ok(());
        }
        let mut purged_grant_count = 0;
        if state == ToolInstanceState::Inactive {
            purged_grant_count = self.purge_instance_grants(instance_id);
            let targets = self
                .requests
                .iter()
                .filter_map(|(request_key, request_state)| {
                    (request_state.request.instance_id == instance_id
                        && !request_state.snapshot.status.is_terminal())
                    .then_some(request_key.clone())
                })
                .collect::<Vec<_>>();
            self.instance_states.insert(instance_id, state);
            for request_key in targets {
                self.close_request(&request_key, RequestCloseKind::Instance);
            }
        } else {
            self.instance_states.insert(instance_id, state);
        }
        self.bump_revision();
        self.emit_audit(
            None,
            ToolAuditEventKind::InstanceStateChanged {
                instance_id,
                generation,
                state,
                purged_grant_count,
            },
        );
        Ok(())
    }

    pub fn remove_instance(
        &mut self,
        instance_id: AgentInstanceId,
    ) -> Result<bool, ToolEngineError> {
        let Some(previous_generation) = self.generations.get(&instance_id).copied() else {
            return Ok(false);
        };
        let purged_grant_count = self.purge_instance_grants(instance_id);
        let targets = self
            .requests
            .iter()
            .filter_map(|(request_key, state)| {
                (state.request.instance_id == instance_id && !state.snapshot.status.is_terminal())
                    .then_some(request_key.clone())
            })
            .collect::<Vec<_>>();
        for request_key in targets {
            self.close_request(&request_key, RequestCloseKind::Instance);
        }
        self.generations.remove(&instance_id);
        self.instance_states.remove(&instance_id);
        self.bump_revision();
        self.emit_audit(
            None,
            ToolAuditEventKind::InstanceRemoved {
                instance_id,
                previous_generation,
                purged_grant_count,
            },
        );
        Ok(true)
    }

    fn close_client(
        &mut self,
        consumer_id: &ConsumerId,
        actor_id: &ToolActorId,
    ) -> ToolAuthorityOutcome {
        let grant_keys = self
            .grants
            .keys()
            .filter(|key| &key.consumer_id == consumer_id && &key.actor_id == actor_id)
            .cloned()
            .collect::<Vec<_>>();
        let purged_grant_count = grant_keys.len();
        for key in grant_keys {
            self.grants.remove(&key);
        }
        let targets = self
            .requests
            .iter()
            .filter_map(|(request_key, state)| {
                (&request_key.consumer_id == consumer_id
                    && &request_key.actor_id == actor_id
                    && !state.snapshot.status.is_terminal())
                .then_some(request_key.clone())
            })
            .collect::<Vec<_>>();
        let closed_request_count = targets.len();
        for request_key in targets {
            self.close_request(&request_key, RequestCloseKind::Client);
        }
        self.bump_revision();
        self.emit_audit(
            None,
            ToolAuditEventKind::ClientClosed {
                consumer_id: consumer_id.clone(),
                actor_id: actor_id.clone(),
                purged_grant_count,
                closed_request_count,
            },
        );
        ToolAuthorityOutcome::ClientClosed {
            purged_grant_count,
            closed_request_count,
        }
    }

    /// Applies the only public policy/approval/client mutation lane.
    /// Successfully reduced envelopes consume their monotonic authority sequence.
    pub fn apply_authority(
        &mut self,
        envelope: ToolAuthorityEnvelope,
    ) -> Result<ToolAuthorityOutcome, ToolEngineError> {
        envelope.validate()?;
        if envelope.sequence <= self.last_authority_sequence {
            return Err(ToolEngineError::AuthoritySequenceRegressed {
                current: self.last_authority_sequence,
                requested: envelope.sequence,
            });
        }
        let outcome = match envelope.command {
            ToolAuthorityCommand::SetGrant { grant } => {
                self.set_grant(grant)?;
                ToolAuthorityOutcome::GrantSet
            }
            ToolAuthorityCommand::RevokeGrant { key } => ToolAuthorityOutcome::GrantRevoked {
                existed: self.revoke_grant(&key)?,
            },
            ToolAuthorityCommand::ResolveApproval { resolution } => {
                match self.resolve_approval(resolution.clone()) {
                    Ok(()) => ToolAuthorityOutcome::ApprovalResolved,
                    Err(ToolEngineError::ApprovalDeadlineElapsed { .. }) => {
                        ToolAuthorityOutcome::ApprovalExpired {
                            request_key: resolution.request_key,
                            accepted_sequence: resolution.accepted_sequence,
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            ToolAuthorityCommand::CloseClient {
                consumer_id,
                actor_id,
            } => self.close_client(&consumer_id, &actor_id),
        };
        self.last_authority_sequence = envelope.sequence;
        Ok(outcome)
    }

    pub fn request(
        &mut self,
        envelope: ConsumerBoundCapabilityRequest,
    ) -> Result<PolicyDecision, ToolEngineError> {
        envelope.validate(self.current_tick)?;
        let request_key = envelope.key();
        let reuses_terminal_key = self
            .requests
            .get(&request_key)
            .map(|state| state.snapshot.status.is_terminal())
            .unwrap_or(false);
        if self.requests.contains_key(&request_key) && !reuses_terminal_key {
            return Err(ToolEngineError::DuplicateRequest { request_key });
        }
        let request = AcceptedRequest::from(envelope);
        let decision = self.evaluate_policy(&request);
        self.ensure_correlation_capacity(decision == PolicyDecision::Allow)?;
        if decision == PolicyDecision::Allow {
            self.ensure_effect_capacity(1)?;
        }
        if !matches!(decision, PolicyDecision::Deny(_)) {
            self.ensure_client_request_capacity(&request.key)?;
        }
        if !reuses_terminal_key {
            self.ensure_request_capacity()?;
        }
        let accepted_sequence = self.allocate_request_sequence();
        let operation_id = (decision == PolicyDecision::Allow).then(|| self.allocate_operation());
        let status = match decision {
            PolicyDecision::Deny(reason) => CapabilityRequestStatus::Denied { reason },
            PolicyDecision::RequireApproval => CapabilityRequestStatus::AwaitingApproval,
            PolicyDecision::Allow => CapabilityRequestStatus::Dispatched {
                operation_id: operation_id.expect("operation allocated for allowed request"),
            },
        };
        let snapshot = CapabilityRequestSnapshot {
            key: request.key.clone(),
            accepted_sequence,
            accepted_at_tick: self.current_tick,
            instance_id: request.instance_id,
            generation: request.generation,
            provider_id: request.provider_id.clone(),
            capability_id: request.capability_id.clone(),
            resource_scope_id: request.resource_scope_id.clone(),
            approval_summary: request.approval_summary.clone(),
            approval_summary_bytes: request.approval_summary.len(),
            deadline_tick: request.deadline_tick,
            payload_bytes: request.payload.len(),
            policy_decision: decision,
            status,
        };
        let subject = subject_for(&request, accepted_sequence);
        let payload_bytes = request.payload.len();
        let mut stored_request = request.clone();
        if decision != PolicyDecision::RequireApproval {
            stored_request.payload.clear();
        }
        if reuses_terminal_key {
            self.requests.remove(&request.key);
        }
        self.requests.insert(
            request.key.clone(),
            RequestState {
                request: stored_request,
                snapshot,
            },
        );
        self.bump_revision();
        self.emit_audit(
            Some(subject.clone()),
            ToolAuditEventKind::RequestEvaluated {
                decision,
                payload_bytes,
            },
        );
        match (decision, operation_id) {
            (PolicyDecision::Deny(reason), None) => self.push_terminal_completion(
                &request,
                accepted_sequence,
                None,
                CapabilityTerminalOutcome::PolicyDenied { reason },
            ),
            (PolicyDecision::Allow, Some(operation_id)) => {
                self.emit_invoke(&request, operation_id);
                self.emit_audit(
                    Some(subject),
                    ToolAuditEventKind::InvocationDispatched { operation_id },
                );
            }
            _ => {}
        }
        Ok(decision)
    }

    fn resolve_approval(&mut self, resolution: ApprovalResolution) -> Result<(), ToolEngineError> {
        let Some(state) = self.requests.get(&resolution.request_key) else {
            return Err(ToolEngineError::UnknownRequest {
                request_key: resolution.request_key,
            });
        };
        if resolution.accepted_sequence != state.snapshot.accepted_sequence {
            return Err(ToolEngineError::ApprovalNonceMismatch {
                request_key: resolution.request_key,
                expected: state.snapshot.accepted_sequence,
                actual: resolution.accepted_sequence,
            });
        }
        let Some(current) = self.generations.get(&state.request.instance_id).copied() else {
            return Err(ToolEngineError::UnknownPolicyInstance {
                instance_id: state.request.instance_id,
            });
        };
        if self.instance_states.get(&state.request.instance_id) != Some(&ToolInstanceState::Active)
        {
            return Err(ToolEngineError::InactivePolicyInstance {
                instance_id: state.request.instance_id,
            });
        }
        if resolution.generation != current {
            return Err(ToolEngineError::ApprovalGenerationStale {
                current,
                actual: resolution.generation,
            });
        }
        if resolution.instance_id != state.request.instance_id
            || resolution.generation != state.request.generation
        {
            return Err(ToolEngineError::ApprovalScopeMismatch {
                request_key: resolution.request_key,
            });
        }
        if !matches!(
            state.snapshot.status,
            CapabilityRequestStatus::AwaitingApproval
        ) {
            return Err(ToolEngineError::RequestNotAwaitingApproval {
                request_key: resolution.request_key,
            });
        }
        if state.request.deadline_tick <= self.current_tick {
            self.timeout_request(&resolution.request_key);
            self.bump_revision();
            return Err(ToolEngineError::ApprovalDeadlineElapsed {
                request_key: resolution.request_key,
            });
        }
        if resolution.decision == ApprovalDecision::ApproveOnce {
            self.ensure_operation_capacity()?;
            self.ensure_effect_capacity(1)?;
        }

        let operation_id = (resolution.decision == ApprovalDecision::ApproveOnce)
            .then(|| self.allocate_operation());
        let (request, subject) = {
            let state = self
                .requests
                .get_mut(&resolution.request_key)
                .expect("approval request checked above");
            let request = state.request.clone();
            state.snapshot.status = match operation_id {
                Some(operation_id) => CapabilityRequestStatus::Dispatched { operation_id },
                None => CapabilityRequestStatus::ApprovalDenied,
            };
            state.request.payload.clear();
            (
                request,
                subject_for(&state.request, state.snapshot.accepted_sequence),
            )
        };
        self.bump_revision();
        self.emit_audit(
            Some(subject.clone()),
            ToolAuditEventKind::ApprovalResolved {
                decision: resolution.decision,
            },
        );
        if let Some(operation_id) = operation_id {
            self.emit_invoke(&request, operation_id);
            self.emit_audit(
                Some(subject),
                ToolAuditEventKind::InvocationDispatched { operation_id },
            );
        } else {
            self.push_terminal_completion(
                &request,
                resolution.accepted_sequence,
                None,
                CapabilityTerminalOutcome::ApprovalDenied,
            );
        }
        Ok(())
    }

    pub fn advance_time(&mut self, tick: u64) -> Result<(), ToolEngineError> {
        if tick < self.current_tick {
            return Err(ToolEngineError::ClockRegressed {
                current_tick: self.current_tick,
                requested_tick: tick,
            });
        }
        if tick == self.current_tick {
            return Ok(());
        }
        let expired = self
            .requests
            .iter()
            .filter_map(|(request_id, state)| {
                (!state.snapshot.status.is_terminal() && state.request.deadline_tick <= tick)
                    .then_some(request_id.clone())
            })
            .collect::<Vec<_>>();
        self.current_tick = tick;
        for request_id in expired {
            self.timeout_request(&request_id);
        }
        self.bump_revision();
        Ok(())
    }

    pub fn apply_observation(
        &mut self,
        envelope: CapabilityObservationEnvelope,
    ) -> Result<(), ToolEngineError> {
        validate_protocol_version(envelope.protocol_version)?;
        if envelope.operation_id.0 == 0 {
            return Err(ToolValidationError::ZeroIdentifier {
                field: "tool operation id",
            }
            .into());
        }
        let Some(state) = self.requests.get(&envelope.request_key) else {
            self.ignore_observation(&envelope, None, ObservationIgnoredReason::UnknownRequest);
            return Ok(());
        };
        let subject = subject_for(&state.request, state.snapshot.accepted_sequence);
        let current_generation = self.generations.get(&state.request.instance_id).copied();
        if current_generation != Some(envelope.generation)
            || envelope.generation != state.request.generation
        {
            self.ignore_observation(
                &envelope,
                Some(subject),
                ObservationIgnoredReason::StaleGeneration,
            );
            return Ok(());
        }
        if envelope.instance_id != state.request.instance_id {
            self.ignore_observation(
                &envelope,
                Some(subject),
                ObservationIgnoredReason::InstanceMismatch,
            );
            return Ok(());
        }
        if envelope.provider_id != state.request.provider_id {
            self.ignore_observation(
                &envelope,
                Some(subject),
                ObservationIgnoredReason::ProviderMismatch,
            );
            return Ok(());
        }
        let operation_id = match state.snapshot.status {
            CapabilityRequestStatus::Dispatched { operation_id } => operation_id,
            _ => {
                self.ignore_observation(
                    &envelope,
                    Some(subject),
                    ObservationIgnoredReason::RequestNotDispatched,
                );
                return Ok(());
            }
        };
        if envelope.operation_id != operation_id {
            self.ignore_observation(
                &envelope,
                Some(subject),
                ObservationIgnoredReason::OperationMismatch,
            );
            return Ok(());
        }
        if state.request.deadline_tick <= self.current_tick {
            self.timeout_request(&envelope.request_key);
            self.bump_revision();
            self.emit_audit(
                Some(subject),
                ToolAuditEventKind::ObservationIgnored {
                    operation_id,
                    reason: ObservationIgnoredReason::DeadlineElapsed,
                },
            );
            return Ok(());
        }

        if envelope.observation.validate().is_err() {
            let failure = ToolFailure::provider_contract_violation();
            let state = self
                .requests
                .get_mut(&envelope.request_key)
                .expect("observation request checked above");
            state.snapshot.status = CapabilityRequestStatus::Failed {
                operation_id,
                failure: failure.clone(),
            };
            state.request.payload.clear();
            let accepted_sequence = state.snapshot.accepted_sequence;
            let request = state.request.clone();
            self.bump_revision();
            self.emit_audit(
                Some(subject),
                ToolAuditEventKind::InvocationFailed {
                    operation_id,
                    failure_kind: failure.kind,
                },
            );
            self.push_terminal_completion(
                &request,
                accepted_sequence,
                Some(operation_id),
                CapabilityTerminalOutcome::Failed { failure },
            );
            return Ok(());
        }

        let event = match envelope.observation {
            CapabilityObservation::Succeeded { result } => {
                let event = ToolAuditEventKind::InvocationSucceeded {
                    operation_id,
                    result_bytes: result.metadata.byte_len,
                    truncated: result.metadata.truncated,
                };
                let metadata = result.metadata.clone();
                let state = self
                    .requests
                    .get_mut(&envelope.request_key)
                    .expect("observation request checked above");
                state.snapshot.status = CapabilityRequestStatus::Succeeded {
                    operation_id,
                    result: metadata,
                };
                state.request.payload.clear();
                let accepted_sequence = state.snapshot.accepted_sequence;
                let request = state.request.clone();
                self.push_terminal_completion(
                    &request,
                    accepted_sequence,
                    Some(operation_id),
                    CapabilityTerminalOutcome::Succeeded { result },
                );
                event
            }
            CapabilityObservation::Failed { failure } => {
                let event = ToolAuditEventKind::InvocationFailed {
                    operation_id,
                    failure_kind: failure.kind,
                };
                let state = self
                    .requests
                    .get_mut(&envelope.request_key)
                    .expect("observation request checked above");
                state.snapshot.status = CapabilityRequestStatus::Failed {
                    operation_id,
                    failure: failure.clone(),
                };
                state.request.payload.clear();
                let accepted_sequence = state.snapshot.accepted_sequence;
                let request = state.request.clone();
                self.push_terminal_completion(
                    &request,
                    accepted_sequence,
                    Some(operation_id),
                    CapabilityTerminalOutcome::Failed { failure },
                );
                event
            }
        };
        self.bump_revision();
        self.emit_audit(Some(subject), event);
        Ok(())
    }

    pub fn snapshot(&self) -> ToolEngineSnapshot {
        ToolEngineSnapshot {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            revision: self.revision,
            current_tick: self.current_tick,
            generations: self
                .generations
                .iter()
                .map(|(instance_id, generation)| (*instance_id, *generation))
                .collect(),
            instance_states: self
                .instance_states
                .iter()
                .map(|(instance_id, state)| (*instance_id, *state))
                .collect(),
            providers: self.providers.values().cloned().collect(),
            grants: self
                .grants
                .iter()
                .map(|(key, mode)| PolicyGrant {
                    key: key.clone(),
                    mode: *mode,
                })
                .collect(),
            requests: self
                .requests
                .values()
                .map(|state| state.snapshot.clone())
                .collect(),
            audit_events: self.audit_events.iter().cloned().collect(),
            dropped_audit_events: self.dropped_audit_events,
            revision_overflow_count: self.revision_overflow_count,
            next_completion_sequence: self.next_completion_sequence,
            dropped_completions: self.dropped_completions,
            effect_sequence_exhausted: self.effect_sequence_exhausted,
            completion_sequence_exhausted: self.completion_sequence_exhausted,
            audit_sequence_exhausted: self.audit_sequence_exhausted,
        }
    }

    pub fn request_snapshot(
        &self,
        request_key: &CapabilityRequestKey,
    ) -> Option<&CapabilityRequestSnapshot> {
        self.requests.get(request_key).map(|state| &state.snapshot)
    }

    pub fn instance_ids(&self) -> impl Iterator<Item = AgentInstanceId> + '_ {
        self.generations.keys().copied()
    }

    pub fn drain_effects(&mut self) -> Vec<CapabilityEffectEnvelope> {
        std::mem::take(&mut self.effects)
    }

    /// Releases bounded terminal outcomes to the requesting shell. Raw inline
    /// bytes and opaque provider references exist only in this queue, never in
    /// snapshots or audit state.
    pub fn drain_completions(&mut self) -> CapabilityCompletionBatch {
        let dropped_since_last_drain = std::mem::take(&mut self.dropped_completions_since_drain);
        CapabilityCompletionBatch {
            completions: std::mem::take(&mut self.completions),
            dropped_since_last_drain,
            total_dropped: self.dropped_completions,
            next_sequence: self.next_completion_sequence,
            sequence_exhausted: self.completion_sequence_exhausted,
        }
    }

    fn evaluate_policy(&self, request: &AcceptedRequest) -> PolicyDecision {
        let Some(current_generation) = self.generations.get(&request.instance_id).copied() else {
            return PolicyDecision::Deny(PolicyDenial::UnknownInstance);
        };
        if self.instance_states.get(&request.instance_id) != Some(&ToolInstanceState::Active) {
            return PolicyDecision::Deny(PolicyDenial::InactiveInstance);
        }
        if current_generation != request.generation {
            return PolicyDecision::Deny(PolicyDenial::StaleGeneration {
                current: current_generation,
            });
        }
        let Some(provider) = self.providers.get(&request.provider_id) else {
            return PolicyDecision::Deny(PolicyDenial::UnknownProvider);
        };
        if matches!(
            &provider.owner,
            CapabilityOwner::Consumer(owner) if owner != &request.key.consumer_id
        ) {
            return PolicyDecision::Deny(PolicyDenial::ProviderOwnerMismatch);
        }
        if !provider.has_capability(&request.capability_id) {
            return PolicyDecision::Deny(PolicyDenial::UnknownCapability);
        }
        let key = PolicyKey {
            consumer_id: request.key.consumer_id.clone(),
            actor_id: request.key.actor_id.clone(),
            instance_id: request.instance_id,
            generation: request.generation,
            provider_id: request.provider_id.clone(),
            capability_id: request.capability_id.clone(),
            resource_scope_id: request.resource_scope_id.clone(),
        };
        match self.grants.get(&key) {
            Some(GrantMode::Allow) => PolicyDecision::Allow,
            Some(GrantMode::RequireApproval) => PolicyDecision::RequireApproval,
            None => PolicyDecision::Deny(PolicyDenial::MissingGrant),
        }
    }

    fn active_requests_for_key(&self, key: &PolicyKey) -> Vec<CapabilityRequestKey> {
        self.requests
            .iter()
            .filter_map(|(request_id, state)| {
                (request_matches_key(&state.request, key) && !state.snapshot.status.is_terminal())
                    .then_some(request_id.clone())
            })
            .collect()
    }

    fn supersede_request(
        &mut self,
        request_key: &CapabilityRequestKey,
        current_generation: SessionGeneration,
    ) {
        let (request, subject, accepted_sequence, operation_id) = {
            let state = self
                .requests
                .get_mut(request_key)
                .expect("supersede target collected from request map");
            let request = state.request.clone();
            let operation_id = match state.snapshot.status {
                CapabilityRequestStatus::Dispatched { operation_id } => Some(operation_id),
                _ => None,
            };
            state.request.payload.clear();
            (
                request,
                subject_for(&state.request, state.snapshot.accepted_sequence),
                state.snapshot.accepted_sequence,
                operation_id,
            )
        };
        let cancellation = match operation_id {
            Some(operation_id) => self.cancel_best_effort(
                &request,
                operation_id,
                InvocationCancelReason::GenerationSuperseded,
            ),
            None => CancellationDisposition::NotRequired,
        };
        self.requests
            .get_mut(request_key)
            .expect("supersede target remains in request map")
            .snapshot
            .status = CapabilityRequestStatus::Superseded {
            operation_id,
            cancellation,
            current_generation,
        };
        self.emit_audit(
            Some(subject),
            ToolAuditEventKind::RequestSuperseded {
                operation_id,
                cancellation,
                current_generation,
            },
        );
        self.push_terminal_completion(
            &request,
            accepted_sequence,
            operation_id,
            CapabilityTerminalOutcome::Superseded {
                current_generation,
                cancellation,
            },
        );
    }

    fn timeout_request(&mut self, request_key: &CapabilityRequestKey) {
        let (request, subject, accepted_sequence, operation_id) = {
            let state = self
                .requests
                .get_mut(request_key)
                .expect("timeout target collected from request map");
            let request = state.request.clone();
            let operation_id = match state.snapshot.status {
                CapabilityRequestStatus::Dispatched { operation_id } => Some(operation_id),
                _ => None,
            };
            state.request.payload.clear();
            (
                request,
                subject_for(&state.request, state.snapshot.accepted_sequence),
                state.snapshot.accepted_sequence,
                operation_id,
            )
        };
        let cancellation = match operation_id {
            Some(operation_id) => self.cancel_best_effort(
                &request,
                operation_id,
                InvocationCancelReason::DeadlineElapsed,
            ),
            None => CancellationDisposition::NotRequired,
        };
        self.requests
            .get_mut(request_key)
            .expect("timeout target remains in request map")
            .snapshot
            .status = CapabilityRequestStatus::TimedOut {
            operation_id,
            cancellation,
        };
        self.emit_audit(
            Some(subject),
            ToolAuditEventKind::RequestTimedOut {
                operation_id,
                cancellation,
            },
        );
        self.push_terminal_completion(
            &request,
            accepted_sequence,
            operation_id,
            CapabilityTerminalOutcome::TimedOut { cancellation },
        );
    }

    fn revoke_request(&mut self, request_key: &CapabilityRequestKey) {
        let (request, subject, operation_id) = {
            let state = self
                .requests
                .get_mut(request_key)
                .expect("revocation target collected from request map");
            let request = state.request.clone();
            let operation_id = match state.snapshot.status {
                CapabilityRequestStatus::Dispatched { operation_id } => Some(operation_id),
                _ => None,
            };
            state.request.payload.clear();
            (
                request,
                subject_for(&state.request, state.snapshot.accepted_sequence),
                operation_id,
            )
        };
        let cancellation = match operation_id {
            Some(operation_id) => self.cancel_best_effort(
                &request,
                operation_id,
                InvocationCancelReason::GrantRevoked,
            ),
            None => CancellationDisposition::NotRequired,
        };
        self.requests
            .get_mut(request_key)
            .expect("revocation target remains in request map")
            .snapshot
            .status = CapabilityRequestStatus::GrantRevoked {
            operation_id,
            cancellation,
        };
        self.emit_audit(
            Some(subject),
            ToolAuditEventKind::RequestGrantRevoked {
                operation_id,
                cancellation,
            },
        );
        self.push_terminal_completion(
            &request,
            self.requests[request_key].snapshot.accepted_sequence,
            operation_id,
            CapabilityTerminalOutcome::GrantRevoked { cancellation },
        );
    }

    fn close_request(&mut self, request_key: &CapabilityRequestKey, kind: RequestCloseKind) {
        let (request, subject, accepted_sequence, operation_id) = {
            let state = self
                .requests
                .get_mut(request_key)
                .expect("close target collected from request map");
            let request = state.request.clone();
            let operation_id = match state.snapshot.status {
                CapabilityRequestStatus::Dispatched { operation_id } => Some(operation_id),
                _ => None,
            };
            state.request.payload.clear();
            (
                request,
                subject_for(&state.request, state.snapshot.accepted_sequence),
                state.snapshot.accepted_sequence,
                operation_id,
            )
        };
        let reason = match kind {
            RequestCloseKind::Instance => InvocationCancelReason::InstanceClosed,
            RequestCloseKind::Client => InvocationCancelReason::ClientClosed,
        };
        let cancellation = match operation_id {
            Some(operation_id) => self.cancel_best_effort(&request, operation_id, reason),
            None => CancellationDisposition::NotRequired,
        };
        let (status, outcome, audit) = match kind {
            RequestCloseKind::Instance => (
                CapabilityRequestStatus::InstanceClosed {
                    operation_id,
                    cancellation,
                },
                CapabilityTerminalOutcome::InstanceClosed { cancellation },
                ToolAuditEventKind::RequestInstanceClosed {
                    operation_id,
                    cancellation,
                },
            ),
            RequestCloseKind::Client => (
                CapabilityRequestStatus::ClientClosed {
                    operation_id,
                    cancellation,
                },
                CapabilityTerminalOutcome::ClientClosed { cancellation },
                ToolAuditEventKind::RequestClientClosed {
                    operation_id,
                    cancellation,
                },
            ),
        };
        self.requests
            .get_mut(request_key)
            .expect("close target remains in request map")
            .snapshot
            .status = status;
        self.emit_audit(Some(subject), audit);
        self.push_terminal_completion(&request, accepted_sequence, operation_id, outcome);
    }

    fn purge_instance_grants(&mut self, instance_id: AgentInstanceId) -> usize {
        let keys = self
            .grants
            .keys()
            .filter(|key| key.instance_id == instance_id)
            .cloned()
            .collect::<Vec<_>>();
        let count = keys.len();
        for key in keys {
            self.grants.remove(&key);
        }
        count
    }

    fn ignore_observation(
        &mut self,
        envelope: &CapabilityObservationEnvelope,
        subject: Option<ToolAuditSubject>,
        reason: ObservationIgnoredReason,
    ) {
        self.bump_revision();
        self.emit_audit(
            subject,
            ToolAuditEventKind::ObservationIgnored {
                operation_id: envelope.operation_id,
                reason,
            },
        );
    }

    fn emit_invoke(&mut self, request: &AcceptedRequest, operation_id: ToolOperationId) {
        let emitted = self.push_effect(
            request,
            operation_id,
            CapabilityEffect::Invoke {
                consumer_id: request.key.consumer_id.clone(),
                actor_id: request.key.actor_id.clone(),
                capability_id: request.capability_id.clone(),
                resource_scope_id: request.resource_scope_id.clone(),
                payload: request.payload.clone(),
            },
        );
        debug_assert!(emitted, "invoke effect sequence was preflighted");
    }

    fn cancel_best_effort(
        &mut self,
        request: &AcceptedRequest,
        operation_id: ToolOperationId,
        reason: InvocationCancelReason,
    ) -> CancellationDisposition {
        let queued_invoke = self.effects.iter().position(|effect| {
            effect.request_key == request.key
                && effect.operation_id == operation_id
                && matches!(&effect.effect, CapabilityEffect::Invoke { .. })
        });
        if let Some(position) = queued_invoke {
            self.effects.remove(position);
            return CancellationDisposition::QueuedInvokeRemoved;
        }
        if self.effects.len() >= TOOL_EFFECTS_MAX {
            return CancellationDisposition::DroppedQueueFull;
        }
        if self.emit_cancel(request, operation_id, reason) {
            CancellationDisposition::CancelQueuedUnconfirmed
        } else {
            CancellationDisposition::DroppedSequenceExhausted
        }
    }

    fn emit_cancel(
        &mut self,
        request: &AcceptedRequest,
        operation_id: ToolOperationId,
        reason: InvocationCancelReason,
    ) -> bool {
        self.push_effect(request, operation_id, CapabilityEffect::Cancel { reason })
    }

    fn push_effect(
        &mut self,
        request: &AcceptedRequest,
        operation_id: ToolOperationId,
        effect: CapabilityEffect,
    ) -> bool {
        if self.effect_sequence_exhausted {
            return false;
        }
        let sequence = self.next_effect_sequence;
        if sequence == u64::MAX {
            self.effect_sequence_exhausted = true;
        } else {
            self.next_effect_sequence += 1;
        }
        self.effects.push(CapabilityEffectEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence,
            operation_id,
            request_key: request.key.clone(),
            instance_id: request.instance_id,
            generation: request.generation,
            provider_id: request.provider_id.clone(),
            deadline_tick: request.deadline_tick,
            effect,
        });
        true
    }

    fn push_terminal_completion(
        &mut self,
        request: &AcceptedRequest,
        accepted_sequence: u64,
        operation_id: Option<ToolOperationId>,
        outcome: CapabilityTerminalOutcome,
    ) {
        if self.completion_sequence_exhausted {
            self.record_completion_drop(
                request,
                accepted_sequence,
                None,
                CompletionDropReason::SequenceExhausted,
            );
            return;
        }
        let sequence = self.next_completion_sequence;
        if sequence == u64::MAX {
            self.completion_sequence_exhausted = true;
        } else {
            self.next_completion_sequence += 1;
        }
        let completion = CapabilityCompletionEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence,
            accepted_sequence,
            operation_id,
            request_key: request.key.clone(),
            instance_id: request.instance_id,
            generation: request.generation,
            provider_id: request.provider_id.clone(),
            outcome,
        };
        if self.completions.len() >= TOOL_COMPLETIONS_MAX {
            self.record_completion_drop(
                request,
                accepted_sequence,
                Some(sequence),
                CompletionDropReason::QueueFull,
            );
            return;
        }
        self.completions.push(completion);
    }

    fn record_completion_drop(
        &mut self,
        request: &AcceptedRequest,
        accepted_sequence: u64,
        completion_sequence: Option<u64>,
        reason: CompletionDropReason,
    ) {
        self.dropped_completions = self.dropped_completions.saturating_add(1);
        self.dropped_completions_since_drain =
            self.dropped_completions_since_drain.saturating_add(1);
        self.emit_audit(
            Some(subject_for(request, accepted_sequence)),
            ToolAuditEventKind::CompletionDropped {
                completion_sequence,
                reason,
            },
        );
    }

    fn ensure_request_capacity(&mut self) -> Result<(), ToolEngineError> {
        if self.requests.len() < TOOL_REQUESTS_MAX {
            return Ok(());
        }
        let oldest_terminal = self
            .requests
            .iter()
            .filter(|(_, state)| state.snapshot.status.is_terminal())
            .min_by_key(|(_, state)| state.snapshot.accepted_sequence)
            .map(|(request_key, _)| request_key.clone());
        if let Some(request_key) = oldest_terminal {
            self.requests.remove(&request_key);
            return Ok(());
        }
        Err(ToolEngineError::RequestCapacityExceeded)
    }

    fn ensure_effect_capacity(&self, additional: usize) -> Result<(), ToolEngineError> {
        if self.effect_sequence_exhausted {
            return Err(ToolEngineError::EffectSequenceExhausted);
        }
        if additional <= TOOL_EFFECTS_MAX.saturating_sub(self.effects.len()) {
            Ok(())
        } else {
            Err(ToolEngineError::EffectCapacityExceeded)
        }
    }

    fn ensure_client_request_capacity(
        &self,
        request_key: &CapabilityRequestKey,
    ) -> Result<(), ToolEngineError> {
        let active_count = self
            .requests
            .iter()
            .filter(|(key, state)| {
                key.consumer_id == request_key.consumer_id
                    && key.actor_id == request_key.actor_id
                    && !state.snapshot.status.is_terminal()
            })
            .count();
        if active_count < TOOL_ACTIVE_REQUESTS_PER_CLIENT_MAX {
            Ok(())
        } else {
            Err(ToolEngineError::ClientRequestCapacityExceeded {
                consumer_id: request_key.consumer_id.clone(),
                actor_id: request_key.actor_id.clone(),
                max: TOOL_ACTIVE_REQUESTS_PER_CLIENT_MAX,
            })
        }
    }

    fn ensure_correlation_capacity(&self, needs_operation: bool) -> Result<(), ToolEngineError> {
        if self.next_request_sequence == u64::MAX {
            return Err(ToolEngineError::CounterExhausted {
                counter: "tool request sequence",
            });
        }
        if needs_operation {
            self.ensure_operation_capacity()?;
        }
        Ok(())
    }

    fn ensure_operation_capacity(&self) -> Result<(), ToolEngineError> {
        if self.next_operation_id == u64::MAX {
            Err(ToolEngineError::CounterExhausted {
                counter: "tool operation id",
            })
        } else {
            Ok(())
        }
    }

    fn allocate_request_sequence(&mut self) -> u64 {
        let sequence = self.next_request_sequence;
        self.next_request_sequence += 1;
        sequence
    }

    fn allocate_operation(&mut self) -> ToolOperationId {
        let operation_id = ToolOperationId(self.next_operation_id);
        self.next_operation_id += 1;
        operation_id
    }

    fn bump_revision(&mut self) {
        if self.revision == u64::MAX {
            self.revision_overflow_count = self.revision_overflow_count.saturating_add(1);
        } else {
            self.revision += 1;
        }
    }

    fn emit_audit(&mut self, subject: Option<ToolAuditSubject>, event: ToolAuditEventKind) {
        if self.audit_sequence_exhausted {
            self.dropped_audit_events = self.dropped_audit_events.saturating_add(1);
            return;
        }
        if self.audit_events.len() == TOOL_AUDIT_EVENTS_MAX {
            self.audit_events.pop_front();
            self.dropped_audit_events = self.dropped_audit_events.saturating_add(1);
        }
        let sequence = self.next_audit_sequence;
        if sequence == u64::MAX {
            self.audit_sequence_exhausted = true;
        } else {
            self.next_audit_sequence += 1;
        }
        self.audit_events.push_back(ToolAuditEvent {
            sequence,
            tick: self.current_tick,
            subject,
            event,
        });
    }
}

impl Default for ToolEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ToolEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolEngine")
            .field("snapshot", &self.snapshot())
            .field("pending_effect_count", &self.effects.len())
            .field("pending_completion_count", &self.completions.len())
            .finish()
    }
}

fn subject_for(request: &AcceptedRequest, accepted_sequence: u64) -> ToolAuditSubject {
    ToolAuditSubject {
        request_key: request.key.clone(),
        accepted_sequence,
        instance_id: request.instance_id,
        generation: request.generation,
        provider_id: request.provider_id.clone(),
        capability_id: request.capability_id.clone(),
        resource_scope_id: request.resource_scope_id.clone(),
    }
}

fn request_matches_key(request: &AcceptedRequest, key: &PolicyKey) -> bool {
    request.key.consumer_id == key.consumer_id
        && request.key.actor_id == key.actor_id
        && request.instance_id == key.instance_id
        && request.generation == key.generation
        && request.provider_id == key.provider_id
        && request.capability_id == key.capability_id
        && request.resource_scope_id == key.resource_scope_id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> AgentInstanceId {
        AgentInstanceId(41)
    }

    fn generation(value: u64) -> SessionGeneration {
        SessionGeneration(value)
    }

    fn actor() -> ToolActorId {
        ToolActorId::new("consumer.agent").unwrap()
    }

    fn consumer() -> ConsumerId {
        ConsumerId::new("station.test").unwrap()
    }

    fn resource_scope() -> ResourceScopeId {
        ResourceScopeId::new("workspace:test/page:active").unwrap()
    }

    fn provider_id() -> ToolProviderId {
        ToolProviderId::new("gate.browser-future").unwrap()
    }

    fn gate_provider_id() -> ToolProviderId {
        ToolProviderId::new("gate.browser-shared").unwrap()
    }

    fn capability_id() -> ToolCapabilityId {
        ToolCapabilityId::new("browser.page.snapshot").unwrap()
    }

    fn provider() -> CapabilityProviderDescriptor {
        CapabilityProviderDescriptor {
            id: provider_id(),
            owner: CapabilityOwner::Consumer(consumer()),
            capabilities: vec![CapabilityDescriptor::new(
                capability_id(),
                CapabilityClass::Browser,
                "Return consumer-owned page state metadata",
            )
            .unwrap()],
        }
    }

    fn gate_provider() -> CapabilityProviderDescriptor {
        CapabilityProviderDescriptor {
            id: gate_provider_id(),
            owner: CapabilityOwner::Gate,
            capabilities: vec![CapabilityDescriptor::new(
                capability_id(),
                CapabilityClass::Browser,
                "Return Gate-owned page state metadata",
            )
            .unwrap()],
        }
    }

    fn grant(mode: GrantMode) -> PolicyGrant {
        PolicyGrant {
            key: PolicyKey {
                consumer_id: consumer(),
                actor_id: actor(),
                instance_id: instance(),
                generation: generation(1),
                provider_id: provider_id(),
                capability_id: capability_id(),
                resource_scope_id: resource_scope(),
            },
            mode,
        }
    }

    fn scoped_grant(
        instance_id: AgentInstanceId,
        session_generation: SessionGeneration,
        resource: String,
        mode: GrantMode,
    ) -> PolicyGrant {
        PolicyGrant {
            key: PolicyKey {
                consumer_id: consumer(),
                actor_id: actor(),
                instance_id,
                generation: session_generation,
                provider_id: provider_id(),
                capability_id: capability_id(),
                resource_scope_id: ResourceScopeId::new(resource).unwrap(),
            },
            mode,
        }
    }

    fn request(id: u64, generation: u64, deadline_tick: u64) -> CapabilityRequest {
        ConsumerBoundCapabilityRequest::new(
            consumer(),
            actor(),
            CapabilityRequestInput {
                local_id: CapabilityRequestId(id),
                instance_id: instance(),
                generation: SessionGeneration(generation),
                provider_id: provider_id(),
                capability_id: capability_id(),
                resource_scope_id: resource_scope(),
                approval_summary: "Read active page state".to_owned(),
                deadline_tick,
                payload: br#"{"scope":"active-page"}"#.to_vec(),
            },
        )
    }

    fn request_key(id: u64) -> CapabilityRequestKey {
        CapabilityRequestKey {
            consumer_id: consumer(),
            actor_id: actor(),
            local_id: CapabilityRequestId(id),
        }
    }

    fn request_for_client(
        id: u64,
        consumer_id: ConsumerId,
        actor_id: ToolActorId,
    ) -> CapabilityRequest {
        let mut request = request(id, 1, 100);
        request.consumer_id = consumer_id;
        request.actor_id = actor_id;
        request
    }

    fn grant_for_client(
        consumer_id: ConsumerId,
        actor_id: ToolActorId,
        mode: GrantMode,
    ) -> PolicyGrant {
        let mut grant = grant(mode);
        grant.key.consumer_id = consumer_id;
        grant.key.actor_id = actor_id;
        grant
    }

    fn accepted_sequence(engine: &ToolEngine, request_id: CapabilityRequestId) -> u64 {
        engine
            .requests
            .get(&request_key(request_id.0))
            .unwrap()
            .snapshot
            .accepted_sequence
    }

    fn dummy_effect() -> CapabilityEffectEnvelope {
        CapabilityEffectEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence: 9_999,
            operation_id: ToolOperationId(9_999),
            request_key: request_key(9_999),
            instance_id: AgentInstanceId(9_999),
            generation: SessionGeneration(9_999),
            provider_id: provider_id(),
            deadline_tick: u64::MAX,
            effect: CapabilityEffect::Cancel {
                reason: InvocationCancelReason::DeadlineElapsed,
            },
        }
    }

    fn fill_effect_queue(engine: &mut ToolEngine) {
        engine.effects.clear();
        engine.effects = vec![dummy_effect(); TOOL_EFFECTS_MAX];
    }

    fn configured(mode: Option<GrantMode>) -> ToolEngine {
        let mut engine = ToolEngine::new();
        engine.register_provider(provider()).unwrap();
        engine.set_generation(instance(), generation(1)).unwrap();
        if let Some(mode) = mode {
            engine.set_grant(grant(mode)).unwrap();
        }
        engine
    }

    struct FakeProvider;

    impl FakeProvider {
        fn succeed(effect: &CapabilityEffectEnvelope) -> CapabilityObservationEnvelope {
            assert!(matches!(effect.effect, CapabilityEffect::Invoke { .. }));
            CapabilityObservationEnvelope {
                protocol_version: CAPABILITY_PROTOCOL_VERSION,
                operation_id: effect.operation_id,
                request_key: effect.request_key.clone(),
                instance_id: effect.instance_id,
                generation: effect.generation,
                provider_id: effect.provider_id.clone(),
                observation: CapabilityObservation::Succeeded {
                    result: CapabilityResult {
                        metadata: CapabilityResultMetadata {
                            byte_len: 2,
                            media_type: Some("application/json".to_owned()),
                            truncated: false,
                            redacted_summary: Some("page snapshot captured".to_owned()),
                        },
                        delivery: CapabilityResultDelivery::Inline {
                            bytes: b"{}".to_vec(),
                        },
                    },
                },
            }
        }
    }

    #[test]
    fn policy_is_deny_by_default_and_releases_no_effect() {
        let mut engine = configured(None);
        assert_eq!(
            engine.request(request(1, 1, 10)).unwrap(),
            PolicyDecision::Deny(PolicyDenial::MissingGrant)
        );
        assert!(engine.drain_effects().is_empty());
        assert!(matches!(
            engine.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::PolicyDenied { .. }
        ));
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.requests[0].payload_bytes, 23);
        assert!(matches!(
            snapshot.requests[0].status,
            CapabilityRequestStatus::Denied {
                reason: PolicyDenial::MissingGrant
            }
        ));
    }

    #[test]
    fn approval_releases_exactly_one_typed_effect() {
        let mut engine = configured(Some(GrantMode::RequireApproval));
        assert_eq!(
            engine.request(request(1, 1, 10)).unwrap(),
            PolicyDecision::RequireApproval
        );
        assert!(engine.drain_effects().is_empty());
        let accepted_sequence = accepted_sequence(&engine, CapabilityRequestId(1));
        engine
            .resolve_approval(ApprovalResolution {
                request_key: request_key(1),
                accepted_sequence,
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            })
            .unwrap();
        let effects = engine.drain_effects();
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0].effect, CapabilityEffect::Invoke { .. }));
        assert!(engine
            .requests
            .get(&request_key(1))
            .unwrap()
            .request
            .payload
            .is_empty());
    }

    #[test]
    fn grant_is_exact_and_does_not_authorize_another_actor() {
        let mut engine = configured(Some(GrantMode::Allow));
        let mut ungranted = request(1, 1, 10);
        ungranted.actor_id = ToolActorId::new("consumer.other-agent").unwrap();
        assert_eq!(
            engine.request(ungranted).unwrap(),
            PolicyDecision::Deny(PolicyDenial::MissingGrant)
        );
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn grant_is_exact_across_consumer_resource_and_session_generation() {
        let mut engine = configured(Some(GrantMode::Allow));
        let mut other_consumer = request(1, 1, 10);
        other_consumer.consumer_id = ConsumerId::new("station.other").unwrap();
        assert_eq!(
            engine.request(other_consumer).unwrap(),
            PolicyDecision::Deny(PolicyDenial::ProviderOwnerMismatch)
        );
        let mut other_resource = request(2, 1, 10);
        other_resource.request.resource_scope_id =
            ResourceScopeId::new("workspace:test/page:other").unwrap();
        assert_eq!(
            engine.request(other_resource).unwrap(),
            PolicyDecision::Deny(PolicyDenial::MissingGrant)
        );
        engine.set_generation(instance(), generation(2)).unwrap();
        assert_eq!(
            engine.request(request(3, 2, 10)).unwrap(),
            PolicyDecision::Deny(PolicyDenial::MissingGrant)
        );
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn consumer_owned_provider_rejects_owner_mismatch_at_grant_and_request() {
        let mut engine = configured(None);
        let mut mismatched_grant = grant(GrantMode::Allow);
        mismatched_grant.key.consumer_id = ConsumerId::new("station.other").unwrap();
        assert!(matches!(
            engine.set_grant(mismatched_grant),
            Err(ToolEngineError::ProviderOwnerMismatch { .. })
        ));

        let mut mismatched_request = request(1, 1, 10);
        mismatched_request.consumer_id = ConsumerId::new("station.other").unwrap();
        assert_eq!(
            engine.request(mismatched_request).unwrap(),
            PolicyDecision::Deny(PolicyDenial::ProviderOwnerMismatch)
        );
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn gate_owned_provider_is_shareable_only_by_exact_grant() {
        let mut engine = ToolEngine::new();
        engine.register_provider(gate_provider()).unwrap();
        engine.set_generation(instance(), generation(1)).unwrap();
        let mut exact_grant = grant(GrantMode::Allow);
        exact_grant.key.provider_id = gate_provider_id();
        engine.set_grant(exact_grant).unwrap();

        let mut exact_request = request(1, 1, 10);
        exact_request.request.provider_id = gate_provider_id();
        assert_eq!(
            engine.request(exact_request).unwrap(),
            PolicyDecision::Allow
        );
        engine.drain_effects();
        let mut other_consumer = request(2, 1, 10);
        other_consumer.request.provider_id = gate_provider_id();
        other_consumer.consumer_id = ConsumerId::new("station.other").unwrap();
        assert_eq!(
            engine.request(other_consumer).unwrap(),
            PolicyDecision::Deny(PolicyDenial::MissingGrant)
        );
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn grant_replacement_revokes_open_approval_and_queued_invoke() {
        let mut approval_engine = configured(Some(GrantMode::RequireApproval));
        approval_engine.request(request(1, 1, 10)).unwrap();
        approval_engine.set_grant(grant(GrantMode::Allow)).unwrap();
        assert!(matches!(
            approval_engine.snapshot().requests[0].status,
            CapabilityRequestStatus::GrantRevoked {
                cancellation: CancellationDisposition::NotRequired,
                ..
            }
        ));
        assert!(approval_engine.drain_effects().is_empty());

        let mut invoke_engine = configured(Some(GrantMode::Allow));
        invoke_engine.request(request(1, 1, 10)).unwrap();
        invoke_engine
            .set_grant(grant(GrantMode::RequireApproval))
            .unwrap();
        assert!(matches!(
            invoke_engine.snapshot().requests[0].status,
            CapabilityRequestStatus::GrantRevoked {
                cancellation: CancellationDisposition::QueuedInvokeRemoved,
                ..
            }
        ));
        assert!(invoke_engine.drain_effects().is_empty());
    }

    #[test]
    fn revoking_grant_closes_pending_approval_and_erases_payload() {
        let mut engine = configured(Some(GrantMode::RequireApproval));
        engine.request(request(1, 1, 10)).unwrap();
        assert!(!engine
            .requests
            .get(&request_key(1))
            .unwrap()
            .request
            .payload
            .is_empty());
        assert!(engine.revoke_grant(&grant(GrantMode::Allow).key).unwrap());
        assert!(engine
            .requests
            .get(&request_key(1))
            .unwrap()
            .request
            .payload
            .is_empty());
        assert!(matches!(
            engine.snapshot().requests[0].status,
            CapabilityRequestStatus::GrantRevoked {
                operation_id: None,
                cancellation: CancellationDisposition::NotRequired,
            }
        ));
        assert!(matches!(
            engine.resolve_approval(ApprovalResolution {
                request_key: request_key(1),
                accepted_sequence: accepted_sequence(&engine, CapabilityRequestId(1)),
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            }),
            Err(ToolEngineError::RequestNotAwaitingApproval { .. })
        ));
        assert!(engine.drain_effects().is_empty());
        assert!(matches!(
            engine.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::GrantRevoked { .. }
        ));
    }

    #[test]
    fn fake_provider_success_closes_only_the_matching_operation() {
        let mut engine = configured(Some(GrantMode::Allow));
        engine.request(request(1, 1, 10)).unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        let mut mismatched = FakeProvider::succeed(&effect);
        mismatched.operation_id = ToolOperationId(effect.operation_id.0 + 1);
        engine.apply_observation(mismatched).unwrap();
        assert!(matches!(
            engine.snapshot().requests[0].status,
            CapabilityRequestStatus::Dispatched { .. }
        ));
        engine
            .apply_observation(FakeProvider::succeed(&effect))
            .unwrap();
        assert!(matches!(
            engine.snapshot().requests[0].status,
            CapabilityRequestStatus::Succeeded { .. }
        ));
        let completion = engine.drain_completions().completions.pop().unwrap();
        assert_eq!(completion.operation_id, Some(effect.operation_id));
        assert!(matches!(
            completion.outcome,
            CapabilityTerminalOutcome::Succeeded {
                result: CapabilityResult {
                    delivery: CapabilityResultDelivery::Inline { ref bytes },
                    ..
                }
            } if bytes == b"{}"
        ));
    }

    #[test]
    fn generation_advance_cancels_and_stale_success_cannot_resurrect_request() {
        let mut engine = configured(Some(GrantMode::Allow));
        engine.request(request(1, 1, 10)).unwrap();
        let invoke = engine.drain_effects().pop().unwrap();
        engine.set_generation(instance(), generation(2)).unwrap();
        let cancel = engine.drain_effects().pop().unwrap();
        assert_eq!(cancel.operation_id, invoke.operation_id);
        assert!(matches!(
            cancel.effect,
            CapabilityEffect::Cancel {
                reason: InvocationCancelReason::GenerationSuperseded
            }
        ));
        engine
            .apply_observation(FakeProvider::succeed(&invoke))
            .unwrap();
        let snapshot = engine.snapshot();
        assert!(matches!(
            snapshot.requests[0].status,
            CapabilityRequestStatus::Superseded {
                current_generation: SessionGeneration(2),
                ..
            }
        ));
        assert!(snapshot.audit_events.iter().any(|event| matches!(
            event.event,
            ToolAuditEventKind::ObservationIgnored {
                reason: ObservationIgnoredReason::StaleGeneration,
                ..
            }
        )));
        assert!(matches!(
            engine.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::Superseded { .. }
        ));
    }

    #[test]
    fn deadline_cancels_dispatched_work_and_late_result_is_ignored() {
        let mut engine = configured(Some(GrantMode::Allow));
        engine.request(request(1, 1, 5)).unwrap();
        let invoke = engine.drain_effects().pop().unwrap();
        engine.advance_time(5).unwrap();
        let cancel = engine.drain_effects().pop().unwrap();
        assert_eq!(cancel.operation_id, invoke.operation_id);
        assert!(matches!(
            cancel.effect,
            CapabilityEffect::Cancel {
                reason: InvocationCancelReason::DeadlineElapsed
            }
        ));
        engine
            .apply_observation(FakeProvider::succeed(&invoke))
            .unwrap();
        assert!(matches!(
            engine.snapshot().requests[0].status,
            CapabilityRequestStatus::TimedOut { .. }
        ));
        assert!(matches!(
            engine.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::TimedOut { .. }
        ));
    }

    #[test]
    fn oversized_input_is_rejected_before_policy_or_effect() {
        let mut engine = configured(Some(GrantMode::Allow));
        let mut oversized = request(1, 1, 10);
        oversized.request.payload = vec![0; TOOL_PAYLOAD_MAX_BYTES + 1];
        assert!(matches!(
            engine.request(oversized),
            Err(ToolEngineError::Validation(ToolValidationError::TooLarge {
                field: "tool request payload",
                ..
            }))
        ));
        assert!(engine.snapshot().requests.is_empty());
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn wire_deserialization_cannot_bypass_bounded_identifier_constructor() {
        let invalid = format!("\"{}\"", "x".repeat(crate::TOOL_ACTOR_ID_MAX_BYTES + 1));
        assert!(serde_json::from_str::<ToolActorId>(&invalid).is_err());
        assert!(serde_json::from_str::<ToolActorId>("\"contains space\"").is_err());
    }

    #[test]
    fn approval_nonce_rejects_aba_after_terminal_eviction_and_id_reuse() {
        let mut engine = configured(Some(GrantMode::RequireApproval));
        engine.request(request(1, 1, 100)).unwrap();
        let old_sequence = accepted_sequence(&engine, CapabilityRequestId(1));
        engine
            .resolve_approval(ApprovalResolution {
                request_key: request_key(1),
                accepted_sequence: old_sequence,
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::Deny,
            })
            .unwrap();
        engine.revoke_grant(&grant(GrantMode::Allow).key).unwrap();
        for id in 2..=TOOL_REQUESTS_MAX as u64 {
            engine.request(request(id, 1, 100)).unwrap();
        }
        engine.set_grant(grant(GrantMode::RequireApproval)).unwrap();
        engine
            .request(request(TOOL_REQUESTS_MAX as u64 + 1, 1, 100))
            .unwrap();
        engine.request(request(1, 1, 100)).unwrap();
        let new_sequence = accepted_sequence(&engine, CapabilityRequestId(1));
        assert_ne!(old_sequence, new_sequence);
        assert!(matches!(
            engine.resolve_approval(ApprovalResolution {
                request_key: request_key(1),
                accepted_sequence: old_sequence,
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            }),
            Err(ToolEngineError::ApprovalNonceMismatch {
                expected,
                actual,
                ..
            }) if expected == new_sequence && actual == old_sequence
        ));
        assert!(engine.drain_effects().is_empty());
    }

    #[test]
    fn safety_transitions_close_authority_even_when_effect_queue_is_full() {
        let mut generation_engine = configured(Some(GrantMode::Allow));
        generation_engine.request(request(1, 1, 10)).unwrap();
        generation_engine.drain_effects();
        fill_effect_queue(&mut generation_engine);
        generation_engine
            .set_generation(instance(), generation(2))
            .unwrap();
        assert_eq!(generation_engine.snapshot().generations[0].1, generation(2));
        assert!(matches!(
            generation_engine.snapshot().requests[0].status,
            CapabilityRequestStatus::Superseded {
                cancellation: CancellationDisposition::DroppedQueueFull,
                ..
            }
        ));

        let mut revoke_engine = configured(Some(GrantMode::Allow));
        revoke_engine.request(request(1, 1, 10)).unwrap();
        revoke_engine.drain_effects();
        fill_effect_queue(&mut revoke_engine);
        assert!(revoke_engine
            .revoke_grant(&grant(GrantMode::Allow).key)
            .unwrap());
        assert!(revoke_engine.snapshot().grants.is_empty());
        assert!(matches!(
            revoke_engine.snapshot().requests[0].status,
            CapabilityRequestStatus::GrantRevoked {
                cancellation: CancellationDisposition::DroppedQueueFull,
                ..
            }
        ));

        let mut time_engine = configured(Some(GrantMode::Allow));
        time_engine.request(request(1, 1, 5)).unwrap();
        while time_engine.effects.len() < TOOL_EFFECTS_MAX {
            time_engine.effects.push(dummy_effect());
        }
        time_engine.advance_time(5).unwrap();
        assert_eq!(time_engine.snapshot().current_tick, 5);
        assert!(matches!(
            time_engine.snapshot().requests[0].status,
            CapabilityRequestStatus::TimedOut {
                cancellation: CancellationDisposition::QueuedInvokeRemoved,
                ..
            }
        ));
        assert_eq!(time_engine.effects.len(), TOOL_EFFECTS_MAX - 1);
    }

    #[test]
    fn generation_advance_purges_only_stale_instance_grants_and_reuses_capacity() {
        let mut engine = configured(None);
        let other_instance = AgentInstanceId(42);
        engine
            .set_generation(other_instance, generation(1))
            .unwrap();
        let other_grant = scoped_grant(
            other_instance,
            generation(1),
            "workspace:other/current".to_owned(),
            GrantMode::Allow,
        );
        engine.set_grant(other_grant.clone()).unwrap();

        let stale_count = TOOL_POLICIES_MAX - 2;
        for index in 0..stale_count {
            engine
                .set_grant(scoped_grant(
                    instance(),
                    generation(1),
                    format!("workspace:test/stale:{index}"),
                    GrantMode::Allow,
                ))
                .unwrap();
        }
        let current_generation_grant = scoped_grant(
            instance(),
            generation(2),
            "workspace:test/current".to_owned(),
            GrantMode::Allow,
        );
        engine.grants.insert(
            current_generation_grant.key.clone(),
            current_generation_grant.mode,
        );
        assert_eq!(engine.grants.len(), TOOL_POLICIES_MAX);

        engine.set_generation(instance(), generation(2)).unwrap();
        assert_eq!(engine.grants.len(), 2);
        assert!(engine.grants.contains_key(&other_grant.key));
        assert!(engine.grants.contains_key(&current_generation_grant.key));
        assert!(engine.snapshot().audit_events.iter().any(|event| matches!(
            event.event,
            ToolAuditEventKind::GenerationAdvanced {
                instance_id,
                current: SessionGeneration(2),
                purged_grant_count,
                ..
            } if instance_id == instance() && purged_grant_count == stale_count
        )));
        engine
            .set_grant(scoped_grant(
                instance(),
                generation(2),
                "workspace:test/reused-capacity".to_owned(),
                GrantMode::Allow,
            ))
            .unwrap();
        assert_eq!(engine.grants.len(), 3);
    }

    #[test]
    fn failed_effect_preflight_does_not_evict_terminal_request() {
        let mut engine = configured(None);
        for id in 1..=TOOL_REQUESTS_MAX as u64 {
            engine.request(request(id, 1, 100)).unwrap();
        }
        engine.set_grant(grant(GrantMode::Allow)).unwrap();
        fill_effect_queue(&mut engine);
        assert!(matches!(
            engine.request(request(TOOL_REQUESTS_MAX as u64 + 1, 1, 100)),
            Err(ToolEngineError::EffectCapacityExceeded)
        ));
        assert_eq!(engine.snapshot().requests.len(), TOOL_REQUESTS_MAX);
        assert!(engine.requests.contains_key(&request_key(1)));
    }

    #[test]
    fn debug_redacts_raw_payload_and_control_characters_are_rejected() {
        let secret_payload = vec![13, 37, 201, 222, 173, 190, 239];
        let secret_payload_debug = format!("{secret_payload:?}");
        let mut raw_request = request(1, 1, 10);
        raw_request.request.payload = secret_payload.clone();
        assert!(!format!("{raw_request:?}").contains(&secret_payload_debug));
        let raw_effect = CapabilityEffect::Invoke {
            consumer_id: consumer(),
            actor_id: actor(),
            capability_id: capability_id(),
            resource_scope_id: resource_scope(),
            payload: secret_payload.clone(),
        };
        assert!(!format!("{raw_effect:?}").contains(&secret_payload_debug));
        let raw_effect_envelope = CapabilityEffectEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence: 1,
            operation_id: ToolOperationId(1),
            request_key: request_key(1),
            instance_id: instance(),
            generation: generation(1),
            provider_id: provider_id(),
            deadline_tick: 10,
            effect: raw_effect,
        };
        assert!(!format!("{raw_effect_envelope:?}").contains(&secret_payload_debug));

        let secret_inline = vec![91, 17, 233, 44, 155];
        let secret_inline_debug = format!("{secret_inline:?}");
        let inline_delivery = CapabilityResultDelivery::Inline {
            bytes: secret_inline.clone(),
        };
        let inline_result = CapabilityResult {
            metadata: CapabilityResultMetadata {
                byte_len: secret_inline.len() as u64,
                media_type: None,
                truncated: false,
                redacted_summary: None,
            },
            delivery: inline_delivery.clone(),
        };
        let inline_completion = CapabilityCompletionEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence: 1,
            accepted_sequence: 1,
            operation_id: Some(ToolOperationId(1)),
            request_key: request_key(1),
            instance_id: instance(),
            generation: generation(1),
            provider_id: provider_id(),
            outcome: CapabilityTerminalOutcome::Succeeded {
                result: inline_result.clone(),
            },
        };
        let inline_observation = CapabilityObservation::Succeeded {
            result: inline_result.clone(),
        };
        let inline_observation_envelope = CapabilityObservationEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            operation_id: ToolOperationId(1),
            request_key: request_key(1),
            instance_id: instance(),
            generation: generation(1),
            provider_id: provider_id(),
            observation: inline_observation.clone(),
        };
        for rendered in [
            format!("{inline_delivery:?}"),
            format!("{inline_result:?}"),
            format!("{inline_completion:?}"),
            format!("{inline_observation:?}"),
            format!("{inline_observation_envelope:?}"),
        ] {
            assert!(!rendered.contains(&secret_inline_debug));
        }

        let secret_reference = "opaque://DO_NOT_FORMAT_REFERENCE";
        let reference_delivery = CapabilityResultDelivery::OpaqueReference {
            reference: secret_reference.to_owned(),
        };
        let reference_result = CapabilityResult {
            metadata: CapabilityResultMetadata {
                byte_len: 128,
                media_type: None,
                truncated: false,
                redacted_summary: None,
            },
            delivery: reference_delivery.clone(),
        };
        let reference_completion = CapabilityCompletionEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence: 2,
            accepted_sequence: 2,
            operation_id: Some(ToolOperationId(2)),
            request_key: request_key(2),
            instance_id: instance(),
            generation: generation(1),
            provider_id: provider_id(),
            outcome: CapabilityTerminalOutcome::Succeeded {
                result: reference_result.clone(),
            },
        };
        let reference_observation = CapabilityObservation::Succeeded {
            result: reference_result.clone(),
        };
        let reference_observation_envelope = CapabilityObservationEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            operation_id: ToolOperationId(2),
            request_key: request_key(2),
            instance_id: instance(),
            generation: generation(1),
            provider_id: provider_id(),
            observation: reference_observation.clone(),
        };
        for rendered in [
            format!("{reference_delivery:?}"),
            format!("{reference_result:?}"),
            format!("{reference_completion:?}"),
            format!("{reference_observation:?}"),
            format!("{reference_observation_envelope:?}"),
        ] {
            assert!(!rendered.contains(secret_reference));
        }

        let mut engine = configured(Some(GrantMode::RequireApproval));
        engine.request(raw_request).unwrap();
        assert!(!format!("{engine:?}").contains(&secret_payload_debug));

        let mut unsafe_summary = request(2, 1, 10);
        unsafe_summary.request.approval_summary = "read page\u{1b}[31m".to_owned();
        assert!(matches!(
            engine.request(unsafe_summary),
            Err(ToolEngineError::Validation(
                ToolValidationError::ControlCharacter {
                    field: "tool approval summary"
                }
            ))
        ));
        let mut whitespace_summary = request(3, 1, 10);
        whitespace_summary.request.approval_summary = "  \t  ".to_owned();
        assert!(matches!(
            engine.request(whitespace_summary),
            Err(ToolEngineError::Validation(ToolValidationError::Required {
                field: "tool approval summary"
            }))
        ));
        assert!(matches!(
            CapabilityResultMetadata {
                byte_len: 0,
                media_type: None,
                truncated: false,
                redacted_summary: Some("unsafe\u{1b}".to_owned()),
            }
            .validate(),
            Err(ToolValidationError::ControlCharacter {
                field: "tool result redacted summary"
            })
        ));
        assert!(matches!(
            ToolFailure {
                kind: crate::ToolFailureKind::Execution,
                redacted_message: Some("unsafe\nmessage".to_owned()),
            }
            .validate(),
            Err(ToolValidationError::ControlCharacter {
                field: "tool failure redacted message"
            })
        ));
    }

    #[test]
    fn capability_admission_rejects_shell_filesystem_and_mcp_namespaces() {
        for id in [
            "shell.exec",
            "filesystem.read",
            "mcp.call",
            "browser.mcp.call",
            "browser.filesystem-read",
        ] {
            assert!(matches!(
                CapabilityDescriptor::new(
                    ToolCapabilityId::new(id).unwrap(),
                    CapabilityClass::Browser,
                    "unsafe capability",
                ),
                Err(ToolValidationError::CapabilityOutsideAdmission { .. })
            ));
        }
        assert!(CapabilityDescriptor::new(
            ToolCapabilityId::new("consumer-state.selection.read").unwrap(),
            CapabilityClass::ConsumerState,
            "Read bounded consumer state",
        )
        .is_ok());
    }

    #[test]
    fn replay_is_deterministic_and_audit_never_contains_payload() {
        fn replay() -> (ToolEngineSnapshot, Vec<CapabilityEffectEnvelope>) {
            let mut engine = configured(Some(GrantMode::Allow));
            engine.request(request(1, 1, 10)).unwrap();
            let effect = engine.drain_effects().pop().unwrap();
            engine
                .apply_observation(FakeProvider::succeed(&effect))
                .unwrap();
            (engine.snapshot(), vec![effect])
        }

        let first = replay();
        let second = replay();
        assert_eq!(first, second);
        assert_eq!(first.0.requests[0].payload_bytes, 23);
    }

    #[test]
    fn provider_failure_and_approval_denial_emit_terminal_completions() {
        let mut failed = configured(Some(GrantMode::Allow));
        failed.request(request(1, 1, 100)).unwrap();
        let effect = failed.drain_effects().pop().unwrap();
        failed
            .apply_observation(CapabilityObservationEnvelope {
                protocol_version: CAPABILITY_PROTOCOL_VERSION,
                operation_id: effect.operation_id,
                request_key: effect.request_key,
                instance_id: effect.instance_id,
                generation: effect.generation,
                provider_id: effect.provider_id,
                observation: CapabilityObservation::Failed {
                    failure: ToolFailure {
                        kind: ToolFailureKind::Execution,
                        redacted_message: Some("provider failed".to_owned()),
                    },
                },
            })
            .unwrap();
        assert!(matches!(
            failed.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::Failed { .. }
        ));

        let mut denied = configured(Some(GrantMode::RequireApproval));
        denied.request(request(1, 1, 100)).unwrap();
        denied
            .resolve_approval(ApprovalResolution {
                request_key: request_key(1),
                accepted_sequence: accepted_sequence(&denied, CapabilityRequestId(1)),
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::Deny,
            })
            .unwrap();
        assert!(matches!(
            denied.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::ApprovalDenied
        ));
    }

    #[test]
    fn same_local_id_is_scoped_by_consumer_and_actor() {
        let consumer_b = ConsumerId::new("station.other").unwrap();
        let actor_b = ToolActorId::new("consumer.other-agent").unwrap();
        let mut engine = ToolEngine::new();
        engine.register_provider(gate_provider()).unwrap();
        engine.set_generation(instance(), generation(1)).unwrap();

        let mut grant_a = grant_for_client(consumer(), actor(), GrantMode::Allow);
        grant_a.key.provider_id = gate_provider_id();
        let mut grant_b = grant_for_client(consumer_b.clone(), actor_b.clone(), GrantMode::Allow);
        grant_b.key.provider_id = gate_provider_id();
        engine.set_grant(grant_a).unwrap();
        engine.set_grant(grant_b).unwrap();

        let mut request_a = request_for_client(7, consumer(), actor());
        request_a.request.provider_id = gate_provider_id();
        let mut request_b = request_for_client(7, consumer_b.clone(), actor_b.clone());
        request_b.request.provider_id = gate_provider_id();
        assert_eq!(engine.request(request_a).unwrap(), PolicyDecision::Allow);
        assert_eq!(engine.request(request_b).unwrap(), PolicyDecision::Allow);
        let effects = engine.drain_effects();
        assert_eq!(effects.len(), 2);
        assert_ne!(effects[0].request_key, effects[1].request_key);
        assert_eq!(
            effects[0].request_key.local_id,
            effects[1].request_key.local_id
        );
    }

    #[test]
    fn approval_target_is_exact_and_forgery_does_not_mutate() {
        let mut engine = configured(Some(GrantMode::RequireApproval));
        engine.request(request(1, 1, 10)).unwrap();
        let before = engine.snapshot();
        let forged_key = CapabilityRequestKey {
            consumer_id: consumer(),
            actor_id: ToolActorId::new("attacker").unwrap(),
            local_id: CapabilityRequestId(1),
        };
        assert!(matches!(
            engine.resolve_approval(ApprovalResolution {
                request_key: forged_key,
                accepted_sequence: before.requests[0].accepted_sequence,
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            }),
            Err(ToolEngineError::UnknownRequest { .. })
        ));
        assert_eq!(engine.snapshot(), before);

        assert!(matches!(
            engine.resolve_approval(ApprovalResolution {
                request_key: request_key(1),
                accepted_sequence: before.requests[0].accepted_sequence,
                instance_id: AgentInstanceId(999),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            }),
            Err(ToolEngineError::ApprovalScopeMismatch { .. })
        ));
        assert_eq!(engine.snapshot(), before);
    }

    #[test]
    fn deactivate_remove_and_reactivate_fail_closed() {
        let mut engine = configured(Some(GrantMode::Allow));
        engine.request(request(1, 1, 100)).unwrap();
        engine.drain_effects();
        engine
            .set_instance_state(instance(), generation(1), ToolInstanceState::Inactive)
            .unwrap();
        assert!(engine.snapshot().grants.is_empty());
        assert!(matches!(
            engine.snapshot().requests[0].status,
            CapabilityRequestStatus::InstanceClosed { .. }
        ));
        assert!(matches!(
            engine.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::InstanceClosed { .. }
        ));
        assert!(matches!(
            engine.resolve_approval(ApprovalResolution {
                request_key: request_key(1),
                accepted_sequence: engine
                    .request_snapshot(&request_key(1))
                    .unwrap()
                    .accepted_sequence,
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            }),
            Err(ToolEngineError::InactivePolicyInstance { .. })
        ));
        assert_eq!(
            engine.request(request(2, 1, 100)).unwrap(),
            PolicyDecision::Deny(PolicyDenial::InactiveInstance)
        );

        engine
            .set_instance_state(instance(), generation(1), ToolInstanceState::Active)
            .unwrap();
        engine.set_grant(grant(GrantMode::Allow)).unwrap();
        assert_eq!(
            engine.request(request(3, 1, 100)).unwrap(),
            PolicyDecision::Allow
        );
        assert!(engine.remove_instance(instance()).unwrap());
        assert!(!engine
            .snapshot()
            .generations
            .iter()
            .any(|(id, _)| *id == instance()));
        assert!(matches!(
            engine.resolve_approval(ApprovalResolution {
                request_key: request_key(3),
                accepted_sequence: engine
                    .request_snapshot(&request_key(3))
                    .unwrap()
                    .accepted_sequence,
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            }),
            Err(ToolEngineError::UnknownPolicyInstance { .. })
        ));
        engine.set_generation(instance(), generation(1)).unwrap();
        assert!(engine
            .snapshot()
            .instance_states
            .contains(&(instance(), ToolInstanceState::Active)));
    }

    #[test]
    fn explicit_client_close_isolated_to_exact_client() {
        let actor_b = ToolActorId::new("consumer.second").unwrap();
        let mut engine = configured(Some(GrantMode::RequireApproval));
        engine
            .set_grant(grant_for_client(
                consumer(),
                actor_b.clone(),
                GrantMode::RequireApproval,
            ))
            .unwrap();
        engine.request(request(1, 1, 100)).unwrap();
        engine
            .request(request_for_client(1, consumer(), actor_b.clone()))
            .unwrap();
        assert!(matches!(
            engine.close_client(&consumer(), &actor()),
            ToolAuthorityOutcome::ClientClosed {
                purged_grant_count: 1,
                closed_request_count: 1,
            }
        ));
        let snapshot = engine.snapshot();
        assert!(snapshot
            .grants
            .iter()
            .any(|grant| grant.key.actor_id == actor_b));
        assert!(snapshot.requests.iter().any(|request| {
            request.key.actor_id == actor_b
                && matches!(request.status, CapabilityRequestStatus::AwaitingApproval)
        }));
        let completions = engine.drain_completions().completions;
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].request_key.actor_id, actor());
    }

    #[test]
    fn per_client_quota_does_not_block_another_client() {
        let actor_b = ToolActorId::new("consumer.second").unwrap();
        let mut engine = configured(Some(GrantMode::RequireApproval));
        for id in 1..=TOOL_ACTIVE_REQUESTS_PER_CLIENT_MAX as u64 {
            engine.request(request(id, 1, 100)).unwrap();
        }
        assert!(matches!(
            engine.request(request(10_000, 1, 100)),
            Err(ToolEngineError::ClientRequestCapacityExceeded { .. })
        ));
        engine
            .set_grant(grant_for_client(
                consumer(),
                actor_b.clone(),
                GrantMode::RequireApproval,
            ))
            .unwrap();
        assert_eq!(
            engine
                .request(request_for_client(1, consumer(), actor_b))
                .unwrap(),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn unsupported_versions_are_rejected_before_mutation() {
        let mut engine = configured(Some(GrantMode::Allow));
        let before_request = engine.snapshot();
        let mut unsupported_request = request(1, 1, 100);
        unsupported_request.protocol_version = CAPABILITY_PROTOCOL_VERSION + 1;
        assert!(matches!(
            engine.request(unsupported_request),
            Err(ToolEngineError::Validation(
                ToolValidationError::UnsupportedProtocolVersion { .. }
            ))
        ));
        assert_eq!(engine.snapshot(), before_request);

        let before_authority = engine.snapshot();
        assert!(matches!(
            engine.apply_authority(ToolAuthorityEnvelope {
                protocol_version: CAPABILITY_PROTOCOL_VERSION + 1,
                sequence: 1,
                command: ToolAuthorityCommand::RevokeGrant {
                    key: grant(GrantMode::Allow).key,
                },
            }),
            Err(ToolEngineError::Validation(
                ToolValidationError::UnsupportedProtocolVersion { .. }
            ))
        ));
        assert_eq!(engine.snapshot(), before_authority);

        engine.request(request(1, 1, 100)).unwrap();
        let effect = engine.drain_effects().pop().unwrap();
        let before_observation = engine.snapshot();
        let mut observation = FakeProvider::succeed(&effect);
        observation.protocol_version = CAPABILITY_PROTOCOL_VERSION + 1;
        assert!(matches!(
            engine.apply_observation(observation),
            Err(ToolEngineError::Validation(
                ToolValidationError::UnsupportedProtocolVersion { .. }
            ))
        ));
        assert_eq!(engine.snapshot(), before_observation);

        let mut zero_operation = FakeProvider::succeed(&effect);
        zero_operation.operation_id = ToolOperationId(0);
        assert!(matches!(
            engine.apply_observation(zero_operation),
            Err(ToolEngineError::Validation(
                ToolValidationError::ZeroIdentifier {
                    field: "tool operation id"
                }
            ))
        ));
        assert_eq!(engine.snapshot(), before_observation);
    }

    #[test]
    fn mutating_expired_approval_consumes_authority_sequence() {
        let mut engine = configured(Some(GrantMode::RequireApproval));
        engine.request(request(1, 1, 10)).unwrap();
        let request_key = request_key(1);
        let accepted_sequence = engine
            .request_snapshot(&request_key)
            .unwrap()
            .accepted_sequence;
        engine.current_tick = 10;

        let outcome = engine
            .apply_authority(ToolAuthorityEnvelope {
                protocol_version: CAPABILITY_PROTOCOL_VERSION,
                sequence: 1,
                command: ToolAuthorityCommand::ResolveApproval {
                    resolution: ApprovalResolution {
                        request_key: request_key.clone(),
                        accepted_sequence,
                        instance_id: instance(),
                        generation: generation(1),
                        decision: ApprovalDecision::ApproveOnce,
                    },
                },
            })
            .unwrap();
        assert_eq!(
            outcome,
            ToolAuthorityOutcome::ApprovalExpired {
                request_key: request_key.clone(),
                accepted_sequence,
            }
        );
        assert!(matches!(
            engine.apply_authority(ToolAuthorityEnvelope {
                protocol_version: CAPABILITY_PROTOCOL_VERSION,
                sequence: 1,
                command: ToolAuthorityCommand::RevokeGrant {
                    key: grant(GrantMode::RequireApproval).key,
                },
            }),
            Err(ToolEngineError::AuthoritySequenceRegressed {
                current: 1,
                requested: 1,
            })
        ));
        assert!(matches!(
            engine.request_snapshot(&request_key).unwrap().status,
            CapabilityRequestStatus::TimedOut { .. }
        ));
        assert!(matches!(
            engine.drain_completions().completions[0].outcome,
            CapabilityTerminalOutcome::TimedOut { .. }
        ));
    }

    #[test]
    fn completion_overflow_is_explicit_and_never_blocks_terminal_transition() {
        let mut engine = configured(None);
        for id in 1..=TOOL_COMPLETIONS_MAX as u64 + 1 {
            assert!(matches!(
                engine.request(request(id, 1, 100)).unwrap(),
                PolicyDecision::Deny(_)
            ));
        }
        assert!(engine
            .snapshot()
            .requests
            .iter()
            .all(|request| request.status.is_terminal()));
        let batch = engine.drain_completions();
        assert_eq!(batch.completions.len(), TOOL_COMPLETIONS_MAX);
        assert_eq!(batch.dropped_since_last_drain, 1);
        assert_eq!(batch.total_dropped, 1);
        let previous_sequence = batch.completions.last().unwrap().sequence;
        engine.request(request(50_000, 1, 100)).unwrap();
        let next = engine.drain_completions();
        assert_eq!(next.completions[0].sequence, previous_sequence + 2);
    }

    #[test]
    fn terminal_key_reuse_is_disambiguated_by_accepted_sequence() {
        let mut engine = configured(None);
        engine.request(request(1, 1, 100)).unwrap();
        let first = engine.drain_completions().completions.pop().unwrap();
        engine.request(request(1, 1, 100)).unwrap();
        let second = engine.drain_completions().completions.pop().unwrap();
        assert_eq!(first.request_key, second.request_key);
        assert_ne!(first.accepted_sequence, second.accepted_sequence);
        assert!(first.sequence < second.sequence);
        assert_eq!(
            engine
                .request_snapshot(&second.request_key)
                .unwrap()
                .accepted_sequence,
            second.accepted_sequence
        );
    }

    #[test]
    fn externally_visible_sequences_never_repeat_after_exhaustion() {
        let mut effects = configured(Some(GrantMode::Allow));
        effects.next_effect_sequence = u64::MAX;
        effects.request(request(1, 1, 100)).unwrap();
        let last_effect = effects.drain_effects().pop().unwrap();
        assert_eq!(last_effect.sequence, u64::MAX);
        assert!(effects.snapshot().effect_sequence_exhausted);
        assert!(matches!(
            effects.request(request(2, 1, 100)),
            Err(ToolEngineError::EffectSequenceExhausted)
        ));

        let mut completions = configured(None);
        completions.next_completion_sequence = u64::MAX;
        completions.request(request(1, 1, 100)).unwrap();
        let last = completions.drain_completions();
        assert_eq!(last.completions[0].sequence, u64::MAX);
        assert!(last.sequence_exhausted);
        completions.request(request(2, 1, 100)).unwrap();
        let dropped = completions.drain_completions();
        assert!(dropped.completions.is_empty());
        assert_eq!(dropped.dropped_since_last_drain, 1);
        assert!(completions
            .snapshot()
            .requests
            .iter()
            .all(|request| request.status.is_terminal()));

        let mut audit = configured(Some(GrantMode::Allow));
        let dropped_before = audit.snapshot().dropped_audit_events;
        audit.next_audit_sequence = u64::MAX;
        audit.revoke_grant(&grant(GrantMode::Allow).key).unwrap();
        audit.set_grant(grant(GrantMode::Allow)).unwrap();
        let snapshot = audit.snapshot();
        assert_eq!(
            snapshot
                .audit_events
                .iter()
                .filter(|event| event.sequence == u64::MAX)
                .count(),
            1
        );
        assert!(snapshot.audit_sequence_exhausted);
        assert!(snapshot.dropped_audit_events > dropped_before);

        audit.revision = u64::MAX;
        audit.revision_overflow_count = 0;
        audit.advance_time(1).unwrap();
        let first = audit.snapshot();
        audit.advance_time(2).unwrap();
        let second = audit.snapshot();
        assert_eq!(first.revision, u64::MAX);
        assert_eq!(second.revision, u64::MAX);
        assert_eq!(first.revision_overflow_count, 1);
        assert_eq!(second.revision_overflow_count, 2);
    }
}
