use crate::model::{
    ApprovalDecision, ApprovalResolution, CancellationDisposition, CapabilityCompletionEnvelope,
    CapabilityEffect, CapabilityEffectEnvelope, CapabilityObservation,
    CapabilityObservationEnvelope, CapabilityOwner, CapabilityProviderDescriptor,
    CapabilityRequest, CapabilityRequestId, CapabilityRequestSnapshot, CapabilityRequestStatus,
    GrantMode, InvocationCancelReason, ObservationIgnoredReason, PolicyDecision, PolicyDenial,
    PolicyGrant, PolicyKey, ToolAuditEvent, ToolAuditEventKind, ToolAuditSubject, ToolEngineError,
    ToolEngineSnapshot, ToolFailure, ToolOperationId, ToolProviderId, TOOL_AUDIT_EVENTS_MAX,
    TOOL_COMPLETIONS_MAX, TOOL_EFFECTS_MAX, TOOL_POLICIES_MAX, TOOL_PROVIDERS_MAX,
    TOOL_REQUESTS_MAX,
};
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

#[derive(Clone, Eq, PartialEq)]
struct RequestState {
    request: CapabilityRequest,
    snapshot: CapabilityRequestSnapshot,
}

/// Pure single-writer authority for Gate-owned capability requests.
///
/// Callers advance a logical clock and session generations explicitly. The
/// engine performs no I/O; only drained effects may be handed to a provider.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolEngine {
    revision: u64,
    current_tick: u64,
    generations: BTreeMap<AgentInstanceId, SessionGeneration>,
    providers: BTreeMap<ToolProviderId, CapabilityProviderDescriptor>,
    grants: BTreeMap<PolicyKey, GrantMode>,
    requests: BTreeMap<CapabilityRequestId, RequestState>,
    effects: Vec<CapabilityEffectEnvelope>,
    completions: Vec<CapabilityCompletionEnvelope>,
    audit_events: VecDeque<ToolAuditEvent>,
    dropped_audit_events: u64,
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
            providers: BTreeMap::new(),
            grants: BTreeMap::new(),
            requests: BTreeMap::new(),
            effects: Vec::new(),
            completions: Vec::new(),
            audit_events: VecDeque::new(),
            dropped_audit_events: 0,
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

    pub fn set_grant(&mut self, grant: PolicyGrant) -> Result<(), ToolEngineError> {
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
                self.revoke_request(request_id);
            }
        }
        self.grants.insert(grant.key.clone(), grant.mode);
        self.bump_revision();
        self.emit_audit(None, ToolAuditEventKind::GrantSet { grant });
        Ok(())
    }

    pub fn revoke_grant(&mut self, key: &PolicyKey) -> Result<bool, ToolEngineError> {
        if !self.grants.contains_key(key) {
            return Ok(false);
        }
        let targets = self.active_requests_for_key(key);
        self.grants.remove(key);
        for request_id in targets {
            self.revoke_request(request_id);
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
                .then_some(*request_id)
            })
            .collect::<Vec<_>>();
        self.generations.insert(instance_id, generation);
        for request_id in targets {
            self.supersede_request(request_id, generation);
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

    pub fn request(
        &mut self,
        request: CapabilityRequest,
    ) -> Result<PolicyDecision, ToolEngineError> {
        request.validate(self.current_tick)?;
        if self.requests.contains_key(&request.id) {
            return Err(ToolEngineError::DuplicateRequest {
                request_id: request.id,
            });
        }
        let decision = self.evaluate_policy(&request);
        self.ensure_correlation_capacity(decision == PolicyDecision::Allow)?;
        if decision == PolicyDecision::Allow {
            self.ensure_effect_capacity(1)?;
        }
        self.ensure_request_capacity()?;
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
            id: request.id,
            accepted_sequence,
            accepted_at_tick: self.current_tick,
            instance_id: request.instance_id,
            generation: request.generation,
            consumer_id: request.consumer_id.clone(),
            actor_id: request.actor_id.clone(),
            provider_id: request.provider_id.clone(),
            capability_id: request.capability_id.clone(),
            resource_scope_id: request.resource_scope_id.clone(),
            approval_summary: request.approval_summary.clone(),
            deadline_tick: request.deadline_tick,
            payload_bytes: request.payload.len(),
            policy_decision: decision,
            status,
        };
        let subject = subject_for(&request, accepted_sequence);
        let mut stored_request = request.clone();
        if decision != PolicyDecision::RequireApproval {
            stored_request.payload.clear();
        }
        self.requests.insert(
            request.id,
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
                payload_bytes: request.payload.len(),
            },
        );
        if let Some(operation_id) = operation_id {
            self.emit_invoke(&request, operation_id);
            self.emit_audit(
                Some(subject),
                ToolAuditEventKind::InvocationDispatched { operation_id },
            );
        }
        Ok(decision)
    }

    pub fn resolve_approval(
        &mut self,
        resolution: ApprovalResolution,
    ) -> Result<(), ToolEngineError> {
        let Some(state) = self.requests.get(&resolution.request_id) else {
            return Err(ToolEngineError::UnknownRequest {
                request_id: resolution.request_id,
            });
        };
        if resolution.accepted_sequence != state.snapshot.accepted_sequence {
            return Err(ToolEngineError::ApprovalNonceMismatch {
                request_id: resolution.request_id,
                expected: state.snapshot.accepted_sequence,
                actual: resolution.accepted_sequence,
            });
        }
        let current = self
            .generations
            .get(&state.request.instance_id)
            .copied()
            .unwrap_or(state.request.generation);
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
                request_id: resolution.request_id,
            });
        }
        if !matches!(
            state.snapshot.status,
            CapabilityRequestStatus::AwaitingApproval
        ) {
            return Err(ToolEngineError::RequestNotAwaitingApproval {
                request_id: resolution.request_id,
            });
        }
        if state.request.deadline_tick <= self.current_tick {
            self.timeout_request(resolution.request_id);
            self.bump_revision();
            return Err(ToolEngineError::ApprovalDeadlineElapsed {
                request_id: resolution.request_id,
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
                .get_mut(&resolution.request_id)
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
                    .then_some(*request_id)
            })
            .collect::<Vec<_>>();
        self.current_tick = tick;
        for request_id in expired {
            self.timeout_request(request_id);
        }
        self.bump_revision();
        Ok(())
    }

    pub fn apply_observation(
        &mut self,
        envelope: CapabilityObservationEnvelope,
    ) -> Result<(), ToolEngineError> {
        let Some(state) = self.requests.get(&envelope.request_id) else {
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
            self.timeout_request(envelope.request_id);
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
                .get_mut(&envelope.request_id)
                .expect("observation request checked above");
            state.snapshot.status = CapabilityRequestStatus::Failed {
                operation_id,
                failure: failure.clone(),
            };
            state.request.payload.clear();
            self.bump_revision();
            self.emit_audit(
                Some(subject),
                ToolAuditEventKind::InvocationFailed {
                    operation_id,
                    failure_kind: failure.kind,
                },
            );
            return Ok(());
        }
        if matches!(
            &envelope.observation,
            CapabilityObservation::Succeeded { .. }
        ) {
            self.ensure_completion_capacity(1)?;
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
                    .get_mut(&envelope.request_id)
                    .expect("observation request checked above");
                state.snapshot.status = CapabilityRequestStatus::Succeeded {
                    operation_id,
                    result: metadata,
                };
                state.request.payload.clear();
                self.push_completion(
                    operation_id,
                    envelope.request_id,
                    envelope.instance_id,
                    envelope.generation,
                    envelope.provider_id.clone(),
                    result,
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
                    .get_mut(&envelope.request_id)
                    .expect("observation request checked above");
                state.snapshot.status = CapabilityRequestStatus::Failed {
                    operation_id,
                    failure,
                };
                state.request.payload.clear();
                event
            }
        };
        self.bump_revision();
        self.emit_audit(Some(subject), event);
        Ok(())
    }

    pub fn snapshot(&self) -> ToolEngineSnapshot {
        ToolEngineSnapshot {
            revision: self.revision,
            current_tick: self.current_tick,
            generations: self
                .generations
                .iter()
                .map(|(instance_id, generation)| (*instance_id, *generation))
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
        }
    }

    pub fn drain_effects(&mut self) -> Vec<CapabilityEffectEnvelope> {
        std::mem::take(&mut self.effects)
    }

    /// Releases bounded successful results to the requesting shell. Raw inline
    /// bytes and opaque provider references exist only in this queue, never in
    /// snapshots or audit state.
    pub fn drain_completions(&mut self) -> Vec<CapabilityCompletionEnvelope> {
        std::mem::take(&mut self.completions)
    }

    fn evaluate_policy(&self, request: &CapabilityRequest) -> PolicyDecision {
        let Some(current_generation) = self.generations.get(&request.instance_id).copied() else {
            return PolicyDecision::Deny(PolicyDenial::UnknownInstance);
        };
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
            CapabilityOwner::Consumer(owner) if owner != &request.consumer_id
        ) {
            return PolicyDecision::Deny(PolicyDenial::ProviderOwnerMismatch);
        }
        if !provider.has_capability(&request.capability_id) {
            return PolicyDecision::Deny(PolicyDenial::UnknownCapability);
        }
        let key = PolicyKey {
            consumer_id: request.consumer_id.clone(),
            actor_id: request.actor_id.clone(),
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

    fn active_requests_for_key(&self, key: &PolicyKey) -> Vec<CapabilityRequestId> {
        self.requests
            .iter()
            .filter_map(|(request_id, state)| {
                (request_matches_key(&state.request, key) && !state.snapshot.status.is_terminal())
                    .then_some(*request_id)
            })
            .collect()
    }

    fn supersede_request(
        &mut self,
        request_id: CapabilityRequestId,
        current_generation: SessionGeneration,
    ) {
        let (request, subject, operation_id) = {
            let state = self
                .requests
                .get_mut(&request_id)
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
            .get_mut(&request_id)
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
    }

    fn timeout_request(&mut self, request_id: CapabilityRequestId) {
        let (request, subject, operation_id) = {
            let state = self
                .requests
                .get_mut(&request_id)
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
            .get_mut(&request_id)
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
    }

    fn revoke_request(&mut self, request_id: CapabilityRequestId) {
        let (request, subject, operation_id) = {
            let state = self
                .requests
                .get_mut(&request_id)
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
            .get_mut(&request_id)
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

    fn emit_invoke(&mut self, request: &CapabilityRequest, operation_id: ToolOperationId) {
        self.push_effect(
            request,
            operation_id,
            CapabilityEffect::Invoke {
                consumer_id: request.consumer_id.clone(),
                actor_id: request.actor_id.clone(),
                capability_id: request.capability_id.clone(),
                resource_scope_id: request.resource_scope_id.clone(),
                payload: request.payload.clone(),
            },
        );
    }

    fn cancel_best_effort(
        &mut self,
        request: &CapabilityRequest,
        operation_id: ToolOperationId,
        reason: InvocationCancelReason,
    ) -> CancellationDisposition {
        let queued_invoke = self.effects.iter().position(|effect| {
            effect.request_id == request.id
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
        self.emit_cancel(request, operation_id, reason);
        CancellationDisposition::CancelQueuedUnconfirmed
    }

    fn emit_cancel(
        &mut self,
        request: &CapabilityRequest,
        operation_id: ToolOperationId,
        reason: InvocationCancelReason,
    ) {
        self.push_effect(request, operation_id, CapabilityEffect::Cancel { reason });
    }

    fn push_effect(
        &mut self,
        request: &CapabilityRequest,
        operation_id: ToolOperationId,
        effect: CapabilityEffect,
    ) {
        let sequence = self.next_effect_sequence;
        self.next_effect_sequence = self.next_effect_sequence.saturating_add(1);
        self.effects.push(CapabilityEffectEnvelope {
            sequence,
            operation_id,
            request_id: request.id,
            instance_id: request.instance_id,
            generation: request.generation,
            provider_id: request.provider_id.clone(),
            deadline_tick: request.deadline_tick,
            effect,
        });
    }

    fn push_completion(
        &mut self,
        operation_id: ToolOperationId,
        request_id: CapabilityRequestId,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        provider_id: ToolProviderId,
        result: crate::model::CapabilityResult,
    ) {
        let sequence = self.next_completion_sequence;
        self.next_completion_sequence = self.next_completion_sequence.saturating_add(1);
        self.completions.push(CapabilityCompletionEnvelope {
            sequence,
            operation_id,
            request_id,
            instance_id,
            generation,
            provider_id,
            result,
        });
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
            .map(|(request_id, _)| *request_id);
        if let Some(request_id) = oldest_terminal {
            self.requests.remove(&request_id);
            return Ok(());
        }
        Err(ToolEngineError::RequestCapacityExceeded)
    }

    fn ensure_effect_capacity(&self, additional: usize) -> Result<(), ToolEngineError> {
        if additional <= TOOL_EFFECTS_MAX.saturating_sub(self.effects.len()) {
            Ok(())
        } else {
            Err(ToolEngineError::EffectCapacityExceeded)
        }
    }

    fn ensure_completion_capacity(&self, additional: usize) -> Result<(), ToolEngineError> {
        if additional <= TOOL_COMPLETIONS_MAX.saturating_sub(self.completions.len()) {
            Ok(())
        } else {
            Err(ToolEngineError::CompletionCapacityExceeded)
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
        self.revision = self.revision.saturating_add(1);
    }

    fn emit_audit(&mut self, subject: Option<ToolAuditSubject>, event: ToolAuditEventKind) {
        if self.audit_events.len() == TOOL_AUDIT_EVENTS_MAX {
            self.audit_events.pop_front();
            self.dropped_audit_events = self.dropped_audit_events.saturating_add(1);
        }
        let sequence = self.next_audit_sequence;
        self.next_audit_sequence = self.next_audit_sequence.saturating_add(1);
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

fn subject_for(request: &CapabilityRequest, accepted_sequence: u64) -> ToolAuditSubject {
    ToolAuditSubject {
        request_id: request.id,
        accepted_sequence,
        instance_id: request.instance_id,
        generation: request.generation,
        consumer_id: request.consumer_id.clone(),
        actor_id: request.actor_id.clone(),
        provider_id: request.provider_id.clone(),
        capability_id: request.capability_id.clone(),
        resource_scope_id: request.resource_scope_id.clone(),
    }
}

fn request_matches_key(request: &CapabilityRequest, key: &PolicyKey) -> bool {
    request.consumer_id == key.consumer_id
        && request.actor_id == key.actor_id
        && request.instance_id == key.instance_id
        && request.generation == key.generation
        && request.provider_id == key.provider_id
        && request.capability_id == key.capability_id
        && request.resource_scope_id == key.resource_scope_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CancellationDisposition, CapabilityClass, CapabilityDescriptor, CapabilityOwner,
        CapabilityResult, CapabilityResultDelivery, CapabilityResultMetadata, ConsumerId,
        ResourceScopeId, ToolActorId, ToolCapabilityId, ToolProviderId, ToolValidationError,
        TOOL_EFFECTS_MAX, TOOL_PAYLOAD_MAX_BYTES, TOOL_POLICIES_MAX, TOOL_REQUESTS_MAX,
    };

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
        CapabilityRequest {
            id: CapabilityRequestId(id),
            consumer_id: consumer(),
            instance_id: instance(),
            generation: SessionGeneration(generation),
            actor_id: actor(),
            provider_id: provider_id(),
            capability_id: capability_id(),
            resource_scope_id: resource_scope(),
            approval_summary: "Read active page state".to_owned(),
            deadline_tick,
            payload: br#"{"scope":"active-page"}"#.to_vec(),
        }
    }

    fn accepted_sequence(engine: &ToolEngine, request_id: CapabilityRequestId) -> u64 {
        engine
            .requests
            .get(&request_id)
            .unwrap()
            .snapshot
            .accepted_sequence
    }

    fn dummy_effect() -> CapabilityEffectEnvelope {
        CapabilityEffectEnvelope {
            sequence: 9_999,
            operation_id: ToolOperationId(9_999),
            request_id: CapabilityRequestId(9_999),
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
                operation_id: effect.operation_id,
                request_id: effect.request_id,
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
                request_id: CapabilityRequestId(1),
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
            .get(&CapabilityRequestId(1))
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
        other_resource.resource_scope_id =
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
        exact_request.provider_id = gate_provider_id();
        assert_eq!(
            engine.request(exact_request).unwrap(),
            PolicyDecision::Allow
        );
        engine.drain_effects();
        let mut other_consumer = request(2, 1, 10);
        other_consumer.provider_id = gate_provider_id();
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
            .get(&CapabilityRequestId(1))
            .unwrap()
            .request
            .payload
            .is_empty());
        assert!(engine.revoke_grant(&grant(GrantMode::Allow).key).unwrap());
        assert!(engine
            .requests
            .get(&CapabilityRequestId(1))
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
                request_id: CapabilityRequestId(1),
                accepted_sequence: accepted_sequence(&engine, CapabilityRequestId(1)),
                instance_id: instance(),
                generation: generation(1),
                decision: ApprovalDecision::ApproveOnce,
            }),
            Err(ToolEngineError::RequestNotAwaitingApproval { .. })
        ));
        assert!(engine.drain_effects().is_empty());
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
        let completion = engine.drain_completions().pop().unwrap();
        assert_eq!(completion.operation_id, effect.operation_id);
        assert!(matches!(
            completion.result.delivery,
            CapabilityResultDelivery::Inline { ref bytes } if bytes == b"{}"
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
    }

    #[test]
    fn oversized_input_is_rejected_before_policy_or_effect() {
        let mut engine = configured(Some(GrantMode::Allow));
        let mut oversized = request(1, 1, 10);
        oversized.payload = vec![0; TOOL_PAYLOAD_MAX_BYTES + 1];
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
                request_id: CapabilityRequestId(1),
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
                request_id: CapabilityRequestId(1),
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
        assert!(engine.requests.contains_key(&CapabilityRequestId(1)));
    }

    #[test]
    fn debug_redacts_raw_payload_and_control_characters_are_rejected() {
        let secret_payload = vec![13, 37, 201, 222, 173, 190, 239];
        let secret_payload_debug = format!("{secret_payload:?}");
        let mut raw_request = request(1, 1, 10);
        raw_request.payload = secret_payload.clone();
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
            sequence: 1,
            operation_id: ToolOperationId(1),
            request_id: CapabilityRequestId(1),
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
            sequence: 1,
            operation_id: ToolOperationId(1),
            request_id: CapabilityRequestId(1),
            instance_id: instance(),
            generation: generation(1),
            provider_id: provider_id(),
            result: inline_result.clone(),
        };
        let inline_observation = CapabilityObservation::Succeeded {
            result: inline_result.clone(),
        };
        let inline_observation_envelope = CapabilityObservationEnvelope {
            operation_id: ToolOperationId(1),
            request_id: CapabilityRequestId(1),
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
            sequence: 2,
            operation_id: ToolOperationId(2),
            request_id: CapabilityRequestId(2),
            instance_id: instance(),
            generation: generation(1),
            provider_id: provider_id(),
            result: reference_result.clone(),
        };
        let reference_observation = CapabilityObservation::Succeeded {
            result: reference_result.clone(),
        };
        let reference_observation_envelope = CapabilityObservationEnvelope {
            operation_id: ToolOperationId(2),
            request_id: CapabilityRequestId(2),
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
        unsafe_summary.approval_summary = "read page\u{1b}[31m".to_owned();
        assert!(matches!(
            engine.request(unsafe_summary),
            Err(ToolEngineError::Validation(
                ToolValidationError::ControlCharacter {
                    field: "tool approval summary"
                }
            ))
        ));
        let mut whitespace_summary = request(3, 1, 10);
        whitespace_summary.approval_summary = "  \t  ".to_owned();
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
}
