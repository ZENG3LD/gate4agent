use gate4agent_kernel::{
    BackendIngress, KernelProviderError, ProviderRuntimeCommandOutcome, ProviderRuntimeTransition,
};
use gate4agent_tool_protocol::{
    CapabilityEffect, CapabilityEffectEnvelope, CapabilityObservation,
    CapabilityObservationEnvelope, CapabilityRequestKey, CapabilityResult, InvocationCancelReason,
    ObservationIgnoredReason, ProviderBindingId, ProviderBoundCapabilityEffectEnvelope,
    ProviderRuntimeCommand, ProviderRuntimeEnvelope, ProviderRuntimeSnapshot, ResourceScopeId,
    ToolCapabilityId, ToolFailure, ToolOperationId, ToolProviderId, CAPABILITY_PROTOCOL_VERSION,
};
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::AddAssign;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use thiserror::Error;

pub const MAX_PROVIDER_RUNTIMES: usize = 64;
pub const MAX_PROVIDER_EFFECT_CAPACITY: usize = 1_024;

#[derive(Clone)]
/// In-process authority that reserves provider binding identities and attaches
/// one local executor per provider.
pub struct ProviderRuntimeAuthorityHandle {
    state: Arc<ProviderAuthorityState>,
}

/// Non-cloneable receive side for one exact provider binding.
pub struct ProviderRuntimeHandle {
    state: Arc<ProviderBindingState>,
    work_rx: Receiver<ProviderWork>,
    observation_outcome_rx: Receiver<ProviderObservationOutcome>,
}

#[derive(Clone)]
/// Cloneable completion sender for worker tasks spawned by one runtime.
pub struct ProviderCompletionHandle {
    state: Arc<ProviderBindingState>,
}

#[derive(Clone)]
/// Local cancellation signal for either a whole binding or one invocation.
///
/// `is_cancelled` does not acknowledge physical subprocess or browser
/// termination; concrete provider supervisors own that stronger contract.
pub struct ProviderCancellationToken {
    state: Weak<ProviderBindingState>,
    operation: Option<Arc<ProviderOperationState>>,
}

/// Bounded executor work emitted after canonical kernel reduction.
pub enum ProviderWork {
    Invoke(ProviderInvocation),
    Cancel(ProviderCancellation),
}

/// Opaque invocation ticket whose correlation identity cannot be rewritten by
/// the provider adapter.
pub struct ProviderInvocation {
    state: Weak<ProviderBindingState>,
    operation: Arc<ProviderOperationState>,
    binding_id: ProviderBindingId,
    effect: CapabilityEffectEnvelope,
    submitted: bool,
}

/// Exact cancellation notice for an invocation previously issued by the same
/// runtime binding.
pub struct ProviderCancellation {
    binding_id: ProviderBindingId,
    effect: CapabilityEffectEnvelope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Correlated receipt for a provider observation already reduced by the
/// canonical kernel.
pub struct ProviderObservationOutcome {
    pub sequence: u64,
    pub operation_id: ToolOperationId,
    pub request_key: CapabilityRequestKey,
    pub status: ProviderObservationStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Canonical disposition of a provider observation.
pub enum ProviderObservationStatus {
    Applied,
    Ignored { reason: ObservationIgnoredReason },
    Rejected(KernelProviderError),
    ContractViolation,
    RuntimeClosed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Public lifecycle projection for one provider runtime binding.
pub enum ProviderRuntimeState {
    Attaching,
    Active,
    Closing,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderEffectPublishReport {
    pub delivered: usize,
    pub queue_full: usize,
    pub disconnected: usize,
    pub unbound: usize,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProviderRuntimeError {
    #[error("provider runtime registry is full")]
    CapacityExceeded,
    #[error("provider already has a runtime binding")]
    AlreadyBound,
    #[error("provider runtime counter '{counter}' is exhausted")]
    CounterExhausted { counter: &'static str },
    #[error("invocation belongs to another provider runtime binding")]
    ForeignInvocation,
    #[error("provider runtime binding is not active")]
    Inactive,
    #[error("provider invocation already submitted a terminal observation")]
    AlreadyCompleted,
    #[error("provider observation is invalid")]
    InvalidObservation,
    #[error("provider runtime ingress is full")]
    Full,
    #[error("provider runtime ingress is disconnected")]
    Disconnected,
}

pub(crate) struct ProviderRuntimePort {
    state: Arc<ProviderAuthorityState>,
}

struct ProviderAuthorityState {
    ingress_tx: SyncSender<BackendIngress>,
    inner: Mutex<ProviderAuthorityInner>,
}

struct ProviderAuthorityInner {
    closed: bool,
    next_sequence: u64,
    sequence_exhausted: bool,
    bindings: BTreeMap<ToolProviderId, Arc<ProviderBindingState>>,
    foreign_bindings: BTreeMap<ToolProviderId, ProviderBindingId>,
}

struct ProviderBindingState {
    provider_id: ToolProviderId,
    binding_id: ProviderBindingId,
    authority: Weak<ProviderAuthorityState>,
    lifecycle: Mutex<ProviderBindingLifecycle>,
    work_tx: Mutex<Option<SyncSender<ProviderWork>>>,
    observation_outcome_tx: Mutex<Option<SyncSender<ProviderObservationOutcome>>>,
    pending_observations: Mutex<BTreeMap<u64, PendingObservation>>,
    operations: Mutex<BTreeMap<ToolOperationId, Arc<ProviderOperationState>>>,
    cancelled: AtomicBool,
}

struct ProviderOperationState {
    request_key: CapabilityRequestKey,
    instance_id: AgentInstanceId,
    generation: SessionGeneration,
    deadline_tick: u64,
    cancelled: AtomicBool,
    settled: AtomicBool,
}

struct PendingObservation {
    operation_id: ToolOperationId,
    request_key: CapabilityRequestKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderBindingLifecycle {
    AttachSent { sequence: u64 },
    Active,
    ClosingRetry { attach_sequence: Option<u64> },
    DetachSent { sequence: u64 },
    Closed,
}

impl ProviderOperationState {
    fn from_effect(effect: &CapabilityEffectEnvelope) -> Self {
        Self {
            request_key: effect.request_key.clone(),
            instance_id: effect.instance_id,
            generation: effect.generation,
            deadline_tick: effect.deadline_tick,
            cancelled: AtomicBool::new(false),
            settled: AtomicBool::new(false),
        }
    }

    fn matches(&self, effect: &CapabilityEffectEnvelope) -> bool {
        self.request_key == effect.request_key
            && self.instance_id == effect.instance_id
            && self.generation == effect.generation
            && self.deadline_tick == effect.deadline_tick
    }
}

pub(crate) fn provider_runtime(
    ingress_tx: SyncSender<BackendIngress>,
) -> (ProviderRuntimeAuthorityHandle, ProviderRuntimePort) {
    let state = Arc::new(ProviderAuthorityState {
        ingress_tx,
        inner: Mutex::new(ProviderAuthorityInner {
            closed: false,
            next_sequence: 1,
            sequence_exhausted: false,
            bindings: BTreeMap::new(),
            foreign_bindings: BTreeMap::new(),
        }),
    });
    (
        ProviderRuntimeAuthorityHandle {
            state: Arc::clone(&state),
        },
        ProviderRuntimePort { state },
    )
}

impl ProviderRuntimeAuthorityHandle {
    pub fn bind_provider(
        &self,
        provider_id: ToolProviderId,
        effect_capacity: usize,
    ) -> Result<ProviderRuntimeHandle, ProviderRuntimeError> {
        let mut authority = lock(&self.state.inner);
        if authority.closed {
            return Err(ProviderRuntimeError::Disconnected);
        }
        if authority.sequence_exhausted {
            return Err(ProviderRuntimeError::CounterExhausted {
                counter: "provider-runtime-sequence",
            });
        }
        if authority.bindings.len() >= MAX_PROVIDER_RUNTIMES {
            return Err(ProviderRuntimeError::CapacityExceeded);
        }
        if authority.bindings.contains_key(&provider_id)
            || authority.foreign_bindings.contains_key(&provider_id)
        {
            return Err(ProviderRuntimeError::AlreadyBound);
        }
        let sequence = authority.next_sequence;
        let binding_id = ProviderBindingId(sequence);
        let (work_tx, work_rx) = sync_channel(bounded_capacity(
            effect_capacity,
            MAX_PROVIDER_EFFECT_CAPACITY,
        ));
        let (observation_outcome_tx, observation_outcome_rx) = sync_channel(bounded_capacity(
            effect_capacity,
            MAX_PROVIDER_EFFECT_CAPACITY,
        ));
        let state = Arc::new(ProviderBindingState {
            provider_id: provider_id.clone(),
            binding_id,
            authority: Arc::downgrade(&self.state),
            lifecycle: Mutex::new(ProviderBindingLifecycle::AttachSent { sequence }),
            work_tx: Mutex::new(Some(work_tx)),
            observation_outcome_tx: Mutex::new(Some(observation_outcome_tx)),
            pending_observations: Mutex::new(BTreeMap::new()),
            operations: Mutex::new(BTreeMap::new()),
            cancelled: AtomicBool::new(false),
        });
        let envelope = ProviderRuntimeEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence,
            command: ProviderRuntimeCommand::Attach {
                binding_id,
                provider_id: provider_id.clone(),
            },
        };
        if let Err(error) = send_ingress(&self.state.ingress_tx, envelope) {
            if error == ProviderRuntimeError::Disconnected {
                let bindings = close_authority_locked(&mut authority);
                drop(authority);
                retire_binding(&state);
                for binding in bindings {
                    retire_binding(&binding);
                }
            }
            return Err(error);
        }
        commit_sequence(&mut authority, sequence);
        authority.bindings.insert(provider_id, Arc::clone(&state));
        Ok(ProviderRuntimeHandle {
            state,
            work_rx,
            observation_outcome_rx,
        })
    }

    pub fn active_binding_count(&self) -> usize {
        lock(&self.state.inner)
            .bindings
            .values()
            .filter(|state| matches!(*lock(&state.lifecycle), ProviderBindingLifecycle::Active))
            .count()
    }

    pub fn sequence_exhausted(&self) -> bool {
        lock(&self.state.inner).sequence_exhausted
    }
}

impl ProviderRuntimeHandle {
    pub fn provider_id(&self) -> &ToolProviderId {
        &self.state.provider_id
    }

    pub fn binding_id(&self) -> ProviderBindingId {
        self.state.binding_id
    }

    pub fn state(&self) -> ProviderRuntimeState {
        public_lifecycle(*lock(&self.state.lifecycle))
    }

    pub fn completion_handle(&self) -> ProviderCompletionHandle {
        ProviderCompletionHandle {
            state: Arc::clone(&self.state),
        }
    }

    pub fn cancellation_token(&self) -> ProviderCancellationToken {
        ProviderCancellationToken {
            state: Arc::downgrade(&self.state),
            operation: None,
        }
    }

    pub fn try_recv(&self) -> Result<ProviderWork, TryRecvError> {
        let lifecycle = lock(&self.state.lifecycle);
        if !matches!(*lifecycle, ProviderBindingLifecycle::Active) {
            return Err(TryRecvError::Disconnected);
        }
        self.work_rx.try_recv()
    }

    pub fn try_recv_observation_outcome(&self) -> Result<ProviderObservationOutcome, TryRecvError> {
        self.observation_outcome_rx.try_recv()
    }

    /// Fences work immediately and non-blockingly requests canonical detach.
    /// A full ingress leaves the detach retryable in `Closing` state.
    pub fn close(&self) -> Result<u64, ProviderRuntimeError> {
        request_close(&self.state)
    }

    pub fn try_complete(
        &self,
        invocation: &mut ProviderInvocation,
        observation: &CapabilityObservation,
    ) -> Result<u64, ProviderRuntimeError> {
        self.completion_handle()
            .try_complete(invocation, observation)
    }

    pub fn try_succeed(
        &self,
        invocation: &mut ProviderInvocation,
        result: &CapabilityResult,
    ) -> Result<u64, ProviderRuntimeError> {
        self.completion_handle().try_succeed(invocation, result)
    }

    pub fn try_fail(
        &self,
        invocation: &mut ProviderInvocation,
        failure: &ToolFailure,
    ) -> Result<u64, ProviderRuntimeError> {
        self.completion_handle().try_fail(invocation, failure)
    }
}

impl Drop for ProviderRuntimeHandle {
    fn drop(&mut self) {
        let _ = request_close(&self.state);
    }
}

impl ProviderCompletionHandle {
    /// Enqueues a terminal observation; the returned sequence is not an
    /// acknowledgement. Read `ProviderObservationOutcome` for canonical
    /// applied/ignored/rejected disposition.
    pub fn try_complete(
        &self,
        invocation: &mut ProviderInvocation,
        observation: &CapabilityObservation,
    ) -> Result<u64, ProviderRuntimeError> {
        if invocation.submitted {
            return Err(ProviderRuntimeError::AlreadyCompleted);
        }
        observation
            .validate()
            .map_err(|_| ProviderRuntimeError::InvalidObservation)?;
        let Some(invocation_state) = invocation.state.upgrade() else {
            return Err(ProviderRuntimeError::Inactive);
        };
        if !Arc::ptr_eq(&self.state, &invocation_state)
            || invocation.binding_id != self.state.binding_id
        {
            return Err(ProviderRuntimeError::ForeignInvocation);
        }
        let sequence =
            submit_observation(&self.state, invocation.effect.clone(), observation.clone())?;
        invocation.submitted = true;
        Ok(sequence)
    }

    pub fn try_succeed(
        &self,
        invocation: &mut ProviderInvocation,
        result: &CapabilityResult,
    ) -> Result<u64, ProviderRuntimeError> {
        self.try_complete(
            invocation,
            &CapabilityObservation::Succeeded {
                result: result.clone(),
            },
        )
    }

    pub fn try_fail(
        &self,
        invocation: &mut ProviderInvocation,
        failure: &ToolFailure,
    ) -> Result<u64, ProviderRuntimeError> {
        self.try_complete(
            invocation,
            &CapabilityObservation::Failed {
                failure: failure.clone(),
            },
        )
    }
}

impl ProviderInvocation {
    pub fn binding_id(&self) -> ProviderBindingId {
        self.binding_id
    }

    pub fn operation_id(&self) -> ToolOperationId {
        self.effect.operation_id
    }

    pub fn request_key(&self) -> &CapabilityRequestKey {
        &self.effect.request_key
    }

    pub fn capability_id(&self) -> &ToolCapabilityId {
        match &self.effect.effect {
            CapabilityEffect::Invoke { capability_id, .. } => capability_id,
            CapabilityEffect::Cancel { .. } => unreachable!("invoke ticket contains invoke effect"),
        }
    }

    pub fn deadline_tick(&self) -> u64 {
        self.effect.deadline_tick
    }

    pub fn resource_scope_id(&self) -> &ResourceScopeId {
        match &self.effect.effect {
            CapabilityEffect::Invoke {
                resource_scope_id, ..
            } => resource_scope_id,
            CapabilityEffect::Cancel { .. } => unreachable!("invoke ticket contains invoke effect"),
        }
    }

    pub fn cancellation_token(&self) -> ProviderCancellationToken {
        ProviderCancellationToken {
            state: self.state.clone(),
            operation: Some(Arc::clone(&self.operation)),
        }
    }

    pub fn payload(&self) -> &[u8] {
        match &self.effect.effect {
            CapabilityEffect::Invoke { payload, .. } => payload,
            CapabilityEffect::Cancel { .. } => unreachable!("invoke ticket contains invoke effect"),
        }
    }
}

impl ProviderCancellationToken {
    pub fn is_cancelled(&self) -> bool {
        self.operation
            .as_ref()
            .is_some_and(|operation| operation.cancelled.load(Ordering::Acquire))
            || self
                .state
                .upgrade()
                .is_none_or(|state| state.cancelled.load(Ordering::Acquire))
    }
}

impl fmt::Debug for ProviderInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderInvocation")
            .field("binding_id", &self.binding_id)
            .field("effect", &self.effect)
            .field("submitted", &self.submitted)
            .finish()
    }
}

impl ProviderCancellation {
    pub fn binding_id(&self) -> ProviderBindingId {
        self.binding_id
    }

    pub fn operation_id(&self) -> ToolOperationId {
        self.effect.operation_id
    }

    pub fn request_key(&self) -> &CapabilityRequestKey {
        &self.effect.request_key
    }

    pub fn reason(&self) -> InvocationCancelReason {
        match self.effect.effect {
            CapabilityEffect::Cancel { reason } => reason,
            CapabilityEffect::Invoke { .. } => unreachable!("cancel ticket contains cancel effect"),
        }
    }
}

impl fmt::Debug for ProviderCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCancellation")
            .field("binding_id", &self.binding_id)
            .field("effect", &self.effect)
            .finish()
    }
}

impl fmt::Debug for ProviderWork {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invoke(invocation) => invocation.fmt(formatter),
            Self::Cancel(cancellation) => cancellation.fmt(formatter),
        }
    }
}

impl ProviderRuntimePort {
    pub(crate) fn authority_handle(&self) -> ProviderRuntimeAuthorityHandle {
        ProviderRuntimeAuthorityHandle {
            state: Arc::clone(&self.state),
        }
    }

    pub(crate) fn flush_closing_bindings(&self) {
        let bindings = lock(&self.state.inner)
            .bindings
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for binding in bindings {
            if matches!(
                *lock(&binding.lifecycle),
                ProviderBindingLifecycle::ClosingRetry { .. }
            ) {
                let _ = request_close(&binding);
            }
        }
    }

    pub(crate) fn publish_outcome(&self, outcome: &ProviderRuntimeCommandOutcome) {
        let state = lock(&self.state.inner)
            .bindings
            .get(&outcome.provider_id)
            .filter(|state| state.binding_id == outcome.binding_id)
            .cloned();
        let Some(state) = state else {
            return;
        };
        publish_observation_outcome(&state, outcome);
        let mut remove = false;
        let mut lifecycle = lock(&state.lifecycle);
        match (&*lifecycle, &outcome.result) {
            (
                ProviderBindingLifecycle::AttachSent { sequence },
                Ok(ProviderRuntimeTransition::Attached),
            ) if *sequence == outcome.sequence => {
                *lifecycle = ProviderBindingLifecycle::Active;
            }
            (ProviderBindingLifecycle::AttachSent { sequence }, Err(_))
                if *sequence == outcome.sequence =>
            {
                *lifecycle = ProviderBindingLifecycle::Closed;
                remove = true;
            }
            (
                ProviderBindingLifecycle::ClosingRetry { .. }
                | ProviderBindingLifecycle::DetachSent { .. },
                Err(_),
            ) if outcome.sequence == state.binding_id.0 => {
                // A handle may be dropped before its attach outcome is
                // published. If attach failed, no detach can ever succeed.
                *lifecycle = ProviderBindingLifecycle::Closed;
                remove = true;
            }
            (
                ProviderBindingLifecycle::DetachSent { sequence },
                Ok(ProviderRuntimeTransition::Detached { .. }),
            ) if *sequence == outcome.sequence => {
                *lifecycle = ProviderBindingLifecycle::Closed;
                remove = true;
            }
            (
                ProviderBindingLifecycle::DetachSent { sequence },
                Err(
                    KernelProviderError::IntegrationBlocked
                    | KernelProviderError::SequenceRegressed { .. },
                ),
            ) if *sequence == outcome.sequence => {
                *lifecycle = ProviderBindingLifecycle::ClosingRetry {
                    attach_sequence: None,
                };
            }
            (ProviderBindingLifecycle::DetachSent { sequence }, Err(_))
                if *sequence == outcome.sequence =>
            {
                // Snapshot reconciliation below distinguishes confirmed
                // absence from a foreign or poisoned kernel binding.
                *lifecycle = ProviderBindingLifecycle::Closed;
                remove = true;
            }
            _ => {}
        }
        drop(lifecycle);
        if remove {
            retire_binding(&state);
            let mut authority = lock(&self.state.inner);
            if authority
                .bindings
                .get(&state.provider_id)
                .is_some_and(|current| Arc::ptr_eq(current, &state))
            {
                authority.bindings.remove(&state.provider_id);
            }
        }
    }

    pub(crate) fn reconcile_snapshot(&self, snapshot: &ProviderRuntimeSnapshot) {
        let canonical = snapshot
            .bindings
            .iter()
            .map(|binding| (binding.provider_id.clone(), binding.binding_id))
            .collect::<BTreeMap<_, _>>();
        let mut authority = lock(&self.state.inner);
        if authority.closed {
            return;
        }
        authority.sequence_exhausted |= snapshot.sequence_exhausted;
        if !authority.sequence_exhausted && snapshot.last_sequence >= authority.next_sequence {
            if let Some(next_sequence) = snapshot.last_sequence.checked_add(1) {
                authority.next_sequence = next_sequence;
            } else {
                authority.sequence_exhausted = true;
            }
        }
        authority.foreign_bindings.clear();

        let mut remove = Vec::new();
        let mut retired = Vec::new();
        for (provider_id, state) in &authority.bindings {
            let exact = canonical.get(provider_id) == Some(&state.binding_id);
            let lifecycle = *lock(&state.lifecycle);
            let command_was_reduced = match lifecycle {
                ProviderBindingLifecycle::AttachSent { sequence }
                | ProviderBindingLifecycle::DetachSent { sequence } => {
                    snapshot.last_sequence >= sequence
                }
                ProviderBindingLifecycle::ClosingRetry { attach_sequence } => {
                    attach_sequence.is_none_or(|sequence| snapshot.last_sequence >= sequence)
                }
                ProviderBindingLifecycle::Active | ProviderBindingLifecycle::Closed => true,
            };
            if snapshot.sequence_exhausted || (!exact && command_was_reduced) {
                remove.push(provider_id.clone());
                retired.push(Arc::clone(state));
            }
        }
        for provider_id in remove {
            authority.bindings.remove(&provider_id);
        }
        for (provider_id, binding_id) in canonical {
            let locally_owned = authority
                .bindings
                .get(&provider_id)
                .is_some_and(|state| state.binding_id == binding_id);
            if !locally_owned {
                authority.foreign_bindings.insert(provider_id, binding_id);
            }
        }
        drop(authority);
        for state in retired {
            retire_binding(&state);
        }
    }

    pub(crate) fn active_bindings(&self) -> BTreeMap<ToolProviderId, ProviderBindingId> {
        lock(&self.state.inner)
            .bindings
            .iter()
            .filter_map(|(provider_id, state)| {
                matches!(*lock(&state.lifecycle), ProviderBindingLifecycle::Active)
                    .then_some((provider_id.clone(), state.binding_id))
            })
            .collect()
    }

    pub(crate) fn publish_effect(
        &self,
        bound: ProviderBoundCapabilityEffectEnvelope,
    ) -> ProviderEffectPublishReport {
        let provider_id = bound.effect.provider_id.clone();
        let state = lock(&self.state.inner)
            .bindings
            .get(&provider_id)
            .filter(|state| state.binding_id == bound.binding_id)
            .cloned();
        let Some(state) = state else {
            return ProviderEffectPublishReport {
                unbound: 1,
                ..ProviderEffectPublishReport::default()
            };
        };
        let mut lifecycle = lock(&state.lifecycle);
        if !matches!(*lifecycle, ProviderBindingLifecycle::Active) {
            return ProviderEffectPublishReport {
                disconnected: 1,
                ..ProviderEffectPublishReport::default()
            };
        }
        let effect = bound.effect;
        let work = if matches!(&effect.effect, CapabilityEffect::Invoke { .. }) {
            let operation = Arc::new(ProviderOperationState::from_effect(&effect));
            let mut operations = lock(&state.operations);
            if operations.contains_key(&effect.operation_id) {
                drop(operations);
                fault_binding_locked(&state, &mut lifecycle);
                return ProviderEffectPublishReport {
                    disconnected: 1,
                    ..ProviderEffectPublishReport::default()
                };
            }
            operations.insert(effect.operation_id, Arc::clone(&operation));
            drop(operations);
            ProviderWork::Invoke(ProviderInvocation {
                state: Arc::downgrade(&state),
                operation,
                binding_id: bound.binding_id,
                effect,
                submitted: false,
            })
        } else {
            let mut operations = lock(&state.operations);
            let exact = operations
                .get(&effect.operation_id)
                .filter(|operation| operation.matches(&effect))
                .cloned();
            let Some(operation) = exact else {
                drop(operations);
                fault_binding_locked(&state, &mut lifecycle);
                return ProviderEffectPublishReport {
                    disconnected: 1,
                    ..ProviderEffectPublishReport::default()
                };
            };
            operation.cancelled.store(true, Ordering::Release);
            operations.remove(&effect.operation_id);
            drop(operations);
            ProviderWork::Cancel(ProviderCancellation {
                binding_id: bound.binding_id,
                effect,
            })
        };
        let mut sender = lock(&state.work_tx);
        let Some(active_sender) = sender.as_ref() else {
            drop(sender);
            fault_binding_locked(&state, &mut lifecycle);
            return ProviderEffectPublishReport {
                disconnected: 1,
                ..ProviderEffectPublishReport::default()
            };
        };
        match active_sender.try_send(work) {
            Ok(()) => ProviderEffectPublishReport {
                delivered: 1,
                ..ProviderEffectPublishReport::default()
            },
            Err(TrySendError::Full(_)) => {
                state.cancelled.store(true, Ordering::Release);
                *lifecycle = ProviderBindingLifecycle::ClosingRetry {
                    attach_sequence: None,
                };
                *sender = None;
                drop(sender);
                cancel_all_operations(&state);
                ProviderEffectPublishReport {
                    queue_full: 1,
                    ..ProviderEffectPublishReport::default()
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                state.cancelled.store(true, Ordering::Release);
                *lifecycle = ProviderBindingLifecycle::ClosingRetry {
                    attach_sequence: None,
                };
                *sender = None;
                drop(sender);
                cancel_all_operations(&state);
                ProviderEffectPublishReport {
                    disconnected: 1,
                    ..ProviderEffectPublishReport::default()
                }
            }
        }
    }

    pub(crate) fn finish_step(&self) {
        let bindings = lock(&self.state.inner)
            .bindings
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for binding in bindings {
            lock(&binding.operations)
                .retain(|_, operation| !operation.settled.load(Ordering::Acquire));
        }
    }
}

impl Drop for ProviderRuntimePort {
    fn drop(&mut self) {
        let bindings = {
            let mut authority = lock(&self.state.inner);
            close_authority_locked(&mut authority)
        };
        for binding in bindings {
            retire_binding(&binding);
        }
    }
}

impl AddAssign for ProviderEffectPublishReport {
    fn add_assign(&mut self, other: Self) {
        self.delivered += other.delivered;
        self.queue_full += other.queue_full;
        self.disconnected += other.disconnected;
        self.unbound += other.unbound;
    }
}

fn publish_observation_outcome(
    state: &Arc<ProviderBindingState>,
    outcome: &ProviderRuntimeCommandOutcome,
) {
    let pending = lock(&state.pending_observations).remove(&outcome.sequence);
    let Some(pending) = pending else {
        return;
    };
    let status = match &outcome.result {
        Ok(ProviderRuntimeTransition::ObservationApplied {
            operation_id,
            request_key,
        }) if *operation_id == pending.operation_id && *request_key == pending.request_key => {
            ProviderObservationStatus::Applied
        }
        Ok(ProviderRuntimeTransition::ObservationIgnored {
            operation_id,
            request_key,
            reason,
        }) if *operation_id == pending.operation_id && *request_key == pending.request_key => {
            ProviderObservationStatus::Ignored { reason: *reason }
        }
        Ok(_) => ProviderObservationStatus::ContractViolation,
        Err(error) => ProviderObservationStatus::Rejected(error.clone()),
    };
    let should_fault = !matches!(
        status,
        ProviderObservationStatus::Applied | ProviderObservationStatus::Ignored { .. }
    );
    if !should_fault {
        if let Some(operation) = lock(&state.operations)
            .get(&pending.operation_id)
            .filter(|operation| operation.request_key == pending.request_key)
            .cloned()
        {
            operation.settled.store(true, Ordering::Release);
        }
    }
    let receipt = ProviderObservationOutcome {
        sequence: outcome.sequence,
        operation_id: pending.operation_id,
        request_key: pending.request_key,
        status,
    };
    if should_fault {
        // The binding fence must be visible before a provider can observe a
        // rejected receipt and attempt another completion.
        mark_binding_fault(state);
    }
    let delivery_failed = lock(&state.observation_outcome_tx)
        .as_ref()
        .is_none_or(|sender| sender.try_send(receipt).is_err());
    if delivery_failed && !should_fault {
        mark_binding_fault(state);
    }
}

fn retire_binding(state: &Arc<ProviderBindingState>) {
    state.cancelled.store(true, Ordering::Release);
    *lock(&state.lifecycle) = ProviderBindingLifecycle::Closed;
    cancel_all_operations(state);
    *lock(&state.work_tx) = None;
    let pending = std::mem::take(&mut *lock(&state.pending_observations));
    let mut outcome_sender = lock(&state.observation_outcome_tx);
    if let Some(sender) = outcome_sender.as_ref() {
        for (sequence, pending) in pending {
            let _ = sender.try_send(ProviderObservationOutcome {
                sequence,
                operation_id: pending.operation_id,
                request_key: pending.request_key,
                status: ProviderObservationStatus::RuntimeClosed,
            });
        }
    }
    *outcome_sender = None;
}

fn submit_observation(
    state: &Arc<ProviderBindingState>,
    effect: CapabilityEffectEnvelope,
    observation: CapabilityObservation,
) -> Result<u64, ProviderRuntimeError> {
    let Some(authority) = state.authority.upgrade() else {
        return Err(ProviderRuntimeError::Disconnected);
    };
    let mut inner = lock(&authority.inner);
    if inner.closed {
        drop(inner);
        retire_binding(state);
        return Err(ProviderRuntimeError::Disconnected);
    }
    if inner.sequence_exhausted {
        return Err(ProviderRuntimeError::CounterExhausted {
            counter: "provider-runtime-sequence",
        });
    }
    let mut lifecycle = lock(&state.lifecycle);
    if !inner
        .bindings
        .get(&state.provider_id)
        .is_some_and(|current| Arc::ptr_eq(current, state))
        || !matches!(*lifecycle, ProviderBindingLifecycle::Active)
    {
        return Err(ProviderRuntimeError::Inactive);
    }
    let sequence = inner.next_sequence;
    let pending = PendingObservation {
        operation_id: effect.operation_id,
        request_key: effect.request_key.clone(),
    };
    let raw = CapabilityObservationEnvelope {
        protocol_version: CAPABILITY_PROTOCOL_VERSION,
        operation_id: effect.operation_id,
        request_key: effect.request_key,
        instance_id: effect.instance_id,
        generation: effect.generation,
        provider_id: effect.provider_id,
        observation,
    };
    let envelope = ProviderRuntimeEnvelope {
        protocol_version: CAPABILITY_PROTOCOL_VERSION,
        sequence,
        command: ProviderRuntimeCommand::Observe {
            binding_id: state.binding_id,
            observation: raw,
        },
    };
    if let Err(error) = send_ingress(&authority.ingress_tx, envelope) {
        drop(lifecycle);
        if error == ProviderRuntimeError::Disconnected {
            let bindings = close_authority_locked(&mut inner);
            drop(inner);
            for binding in bindings {
                retire_binding(&binding);
            }
        }
        return Err(error);
    }
    lock(&state.pending_observations).insert(sequence, pending);
    commit_sequence(&mut inner, sequence);
    if sequence == u64::MAX {
        state.cancelled.store(true, Ordering::Release);
        *lifecycle = ProviderBindingLifecycle::ClosingRetry {
            attach_sequence: None,
        };
        cancel_all_operations(state);
        *lock(&state.work_tx) = None;
    }
    Ok(sequence)
}

fn request_close(state: &Arc<ProviderBindingState>) -> Result<u64, ProviderRuntimeError> {
    state.cancelled.store(true, Ordering::Release);
    cancel_all_operations(state);
    let Some(authority) = state.authority.upgrade() else {
        retire_binding(state);
        return Err(ProviderRuntimeError::Disconnected);
    };
    let mut inner = lock(&authority.inner);
    if inner.closed {
        drop(inner);
        retire_binding(state);
        return Err(ProviderRuntimeError::Disconnected);
    }
    if !inner
        .bindings
        .get(&state.provider_id)
        .is_some_and(|current| Arc::ptr_eq(current, state))
    {
        drop(inner);
        retire_binding(state);
        return Err(ProviderRuntimeError::Inactive);
    }
    let mut lifecycle = lock(&state.lifecycle);
    if inner.sequence_exhausted {
        if !matches!(*lifecycle, ProviderBindingLifecycle::Closed) {
            *lifecycle = closing_retry(*lifecycle);
            *lock(&state.work_tx) = None;
        }
        return Err(ProviderRuntimeError::CounterExhausted {
            counter: "provider-runtime-sequence",
        });
    }
    match *lifecycle {
        ProviderBindingLifecycle::DetachSent { sequence } => return Ok(sequence),
        ProviderBindingLifecycle::Closed => return Err(ProviderRuntimeError::Inactive),
        ProviderBindingLifecycle::AttachSent { .. }
        | ProviderBindingLifecycle::Active
        | ProviderBindingLifecycle::ClosingRetry { .. } => {}
    }
    let sequence = inner.next_sequence;
    let envelope = ProviderRuntimeEnvelope {
        protocol_version: CAPABILITY_PROTOCOL_VERSION,
        sequence,
        command: ProviderRuntimeCommand::Detach {
            binding_id: state.binding_id,
            provider_id: state.provider_id.clone(),
        },
    };
    match authority
        .ingress_tx
        .try_send(BackendIngress::ToolProvider(envelope))
    {
        Ok(()) => {
            commit_sequence(&mut inner, sequence);
            *lifecycle = ProviderBindingLifecycle::DetachSent { sequence };
            *lock(&state.work_tx) = None;
            Ok(sequence)
        }
        Err(TrySendError::Full(_)) => {
            *lifecycle = closing_retry(*lifecycle);
            *lock(&state.work_tx) = None;
            Err(ProviderRuntimeError::Full)
        }
        Err(TrySendError::Disconnected(_)) => {
            drop(lifecycle);
            let bindings = close_authority_locked(&mut inner);
            drop(inner);
            for binding in bindings {
                retire_binding(&binding);
            }
            Err(ProviderRuntimeError::Disconnected)
        }
    }
}

fn mark_binding_fault(state: &Arc<ProviderBindingState>) {
    let mut lifecycle = lock(&state.lifecycle);
    fault_binding_locked(state, &mut lifecycle);
}

fn fault_binding_locked(
    state: &Arc<ProviderBindingState>,
    lifecycle: &mut ProviderBindingLifecycle,
) {
    state.cancelled.store(true, Ordering::Release);
    if matches!(
        *lifecycle,
        ProviderBindingLifecycle::AttachSent { .. }
            | ProviderBindingLifecycle::Active
            | ProviderBindingLifecycle::ClosingRetry { .. }
    ) {
        *lifecycle = closing_retry(*lifecycle);
    }
    cancel_all_operations(state);
    *lock(&state.work_tx) = None;
}

fn cancel_all_operations(state: &Arc<ProviderBindingState>) {
    let operations = std::mem::take(&mut *lock(&state.operations));
    for operation in operations.into_values() {
        operation.cancelled.store(true, Ordering::Release);
    }
}

fn close_authority_locked(inner: &mut ProviderAuthorityInner) -> Vec<Arc<ProviderBindingState>> {
    inner.closed = true;
    inner.foreign_bindings.clear();
    std::mem::take(&mut inner.bindings).into_values().collect()
}

fn send_ingress(
    sender: &SyncSender<BackendIngress>,
    envelope: ProviderRuntimeEnvelope,
) -> Result<(), ProviderRuntimeError> {
    sender
        .try_send(BackendIngress::ToolProvider(envelope))
        .map_err(|error| match error {
            TrySendError::Full(_) => ProviderRuntimeError::Full,
            TrySendError::Disconnected(_) => ProviderRuntimeError::Disconnected,
        })
}

fn closing_retry(lifecycle: ProviderBindingLifecycle) -> ProviderBindingLifecycle {
    let attach_sequence = match lifecycle {
        ProviderBindingLifecycle::AttachSent { sequence } => Some(sequence),
        ProviderBindingLifecycle::ClosingRetry { attach_sequence } => attach_sequence,
        ProviderBindingLifecycle::Active
        | ProviderBindingLifecycle::DetachSent { .. }
        | ProviderBindingLifecycle::Closed => None,
    };
    ProviderBindingLifecycle::ClosingRetry { attach_sequence }
}

fn public_lifecycle(lifecycle: ProviderBindingLifecycle) -> ProviderRuntimeState {
    match lifecycle {
        ProviderBindingLifecycle::AttachSent { .. } => ProviderRuntimeState::Attaching,
        ProviderBindingLifecycle::Active => ProviderRuntimeState::Active,
        ProviderBindingLifecycle::ClosingRetry { .. }
        | ProviderBindingLifecycle::DetachSent { .. } => ProviderRuntimeState::Closing,
        ProviderBindingLifecycle::Closed => ProviderRuntimeState::Closed,
    }
}

fn commit_sequence(inner: &mut ProviderAuthorityInner, sequence: u64) {
    if sequence == u64::MAX {
        inner.sequence_exhausted = true;
    } else {
        inner.next_sequence = sequence + 1;
    }
}

fn bounded_capacity(requested: usize, max: usize) -> usize {
    requested.clamp(1, max)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_tool_protocol::{
        CapabilityRequestId, CapabilityResultDelivery, CapabilityResultMetadata, ConsumerId,
        ProviderRuntimeBindingSnapshot, ProviderRuntimeSnapshot, ToolActorId,
    };
    use gate4agent_types::{AgentInstanceId, SessionGeneration};

    fn provider_id() -> ToolProviderId {
        ToolProviderId::new("gate.provider.unit").unwrap()
    }

    fn bound_invoke(binding_id: ProviderBindingId) -> ProviderBoundCapabilityEffectEnvelope {
        bound_invoke_with_operation(binding_id, 1)
    }

    fn bound_invoke_with_operation(
        binding_id: ProviderBindingId,
        operation: u64,
    ) -> ProviderBoundCapabilityEffectEnvelope {
        let consumer_id = ConsumerId::new("gate-consumer-unit").unwrap();
        let actor_id = ToolActorId::new("gate-actor-unit").unwrap();
        ProviderBoundCapabilityEffectEnvelope {
            binding_id,
            effect: CapabilityEffectEnvelope {
                protocol_version: CAPABILITY_PROTOCOL_VERSION,
                sequence: operation,
                operation_id: ToolOperationId(operation),
                request_key: CapabilityRequestKey {
                    consumer_id: consumer_id.clone(),
                    actor_id: actor_id.clone(),
                    local_id: CapabilityRequestId(operation),
                },
                instance_id: AgentInstanceId(1),
                generation: SessionGeneration(1),
                provider_id: provider_id(),
                deadline_tick: 10,
                effect: CapabilityEffect::Invoke {
                    consumer_id,
                    actor_id,
                    capability_id: ToolCapabilityId::new("browser.snapshot").unwrap(),
                    resource_scope_id: ResourceScopeId::new("page.active").unwrap(),
                    payload: b"{}".to_vec(),
                },
            },
        }
    }

    fn bound_cancel(
        invoke: &ProviderBoundCapabilityEffectEnvelope,
        sequence: u64,
        reason: InvocationCancelReason,
    ) -> ProviderBoundCapabilityEffectEnvelope {
        let mut cancel = invoke.clone();
        cancel.effect.sequence = sequence;
        cancel.effect.effect = CapabilityEffect::Cancel { reason };
        cancel
    }

    fn result() -> CapabilityResult {
        CapabilityResult {
            metadata: CapabilityResultMetadata {
                byte_len: 2,
                media_type: Some("application/json".to_owned()),
                truncated: false,
                redacted_summary: None,
            },
            delivery: CapabilityResultDelivery::Inline {
                bytes: b"{}".to_vec(),
            },
        }
    }

    fn activate(
        authority: &ProviderRuntimeAuthorityHandle,
        port: &ProviderRuntimePort,
        ingress: &Receiver<BackendIngress>,
    ) -> ProviderRuntimeHandle {
        let runtime = authority.bind_provider(provider_id(), 2).unwrap();
        let BackendIngress::ToolProvider(attach) = ingress.try_recv().unwrap() else {
            panic!("expected attach ingress");
        };
        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence: attach.sequence,
            binding_id: runtime.binding_id(),
            provider_id: provider_id(),
            result: Ok(ProviderRuntimeTransition::Attached),
        });
        assert_eq!(runtime.state(), ProviderRuntimeState::Active);
        runtime
    }

    #[test]
    fn rejected_observation_is_correlated_and_faults_binding() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        assert_eq!(
            port.publish_effect(bound_invoke(runtime.binding_id()))
                .delivered,
            1
        );
        let ProviderWork::Invoke(mut invocation) = runtime.try_recv().unwrap() else {
            panic!("expected invocation");
        };
        let sequence = runtime.try_succeed(&mut invocation, &result()).unwrap();
        let BackendIngress::ToolProvider(observation) = ingress_rx.try_recv().unwrap() else {
            panic!("expected observation ingress");
        };
        assert_eq!(observation.sequence, sequence);

        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence,
            binding_id: runtime.binding_id(),
            provider_id: provider_id(),
            result: Err(KernelProviderError::IntegrationBlocked),
        });
        let receipt = runtime.try_recv_observation_outcome().unwrap();
        assert_eq!(receipt.sequence, sequence);
        assert_eq!(
            receipt.status,
            ProviderObservationStatus::Rejected(KernelProviderError::IntegrationBlocked)
        );
        assert_eq!(runtime.state(), ProviderRuntimeState::Closing);
        assert!(runtime.cancellation_token().is_cancelled());
    }

    #[test]
    fn full_observation_ingress_is_retryable_without_consuming_ticket() {
        let (ingress_tx, ingress_rx) = sync_channel(1);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        assert_eq!(
            port.publish_effect(bound_invoke(runtime.binding_id()))
                .delivered,
            1
        );
        assert_eq!(
            port.publish_effect(bound_invoke_with_operation(runtime.binding_id(), 2))
                .delivered,
            1
        );
        let ProviderWork::Invoke(mut first) = runtime.try_recv().unwrap() else {
            panic!("expected first invocation");
        };
        let ProviderWork::Invoke(mut second) = runtime.try_recv().unwrap() else {
            panic!("expected second invocation");
        };

        assert_eq!(runtime.try_succeed(&mut first, &result()).unwrap(), 2);
        assert_eq!(
            runtime.try_succeed(&mut second, &result()),
            Err(ProviderRuntimeError::Full)
        );
        assert!(matches!(
            ingress_rx.try_recv().unwrap(),
            BackendIngress::ToolProvider(ProviderRuntimeEnvelope { sequence: 2, .. })
        ));
        assert_eq!(runtime.try_succeed(&mut second, &result()).unwrap(), 3);
        assert!(matches!(
            ingress_rx.try_recv().unwrap(),
            BackendIngress::ToolProvider(ProviderRuntimeEnvelope { sequence: 3, .. })
        ));
    }

    #[test]
    fn closing_attach_survives_prefix_snapshot_until_attach_is_reduced() {
        let (ingress_tx, ingress_rx) = sync_channel(1);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = authority.bind_provider(provider_id(), 1).unwrap();
        let binding_id = runtime.binding_id();
        assert_eq!(runtime.close(), Err(ProviderRuntimeError::Full));
        assert_eq!(runtime.state(), ProviderRuntimeState::Closing);

        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: 0,
            sequence_exhausted: false,
            bindings: Vec::new(),
        });
        assert_eq!(runtime.state(), ProviderRuntimeState::Closing);
        assert!(matches!(
            authority.bind_provider(provider_id(), 1),
            Err(ProviderRuntimeError::AlreadyBound)
        ));

        let BackendIngress::ToolProvider(attach) = ingress_rx.try_recv().unwrap() else {
            panic!("expected retained attach");
        };
        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence: attach.sequence,
            binding_id,
            provider_id: provider_id(),
            result: Ok(ProviderRuntimeTransition::Attached),
        });
        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: attach.sequence,
            sequence_exhausted: false,
            bindings: vec![ProviderRuntimeBindingSnapshot {
                binding_id,
                provider_id: provider_id(),
            }],
        });
        assert_eq!(runtime.state(), ProviderRuntimeState::Closing);

        port.flush_closing_bindings();
        let BackendIngress::ToolProvider(detach) = ingress_rx.try_recv().unwrap() else {
            panic!("expected detach after attach reduction");
        };
        assert_eq!(detach.sequence, 2);
        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence: detach.sequence,
            binding_id,
            provider_id: provider_id(),
            result: Ok(ProviderRuntimeTransition::Detached {
                closed_request_count: 0,
            }),
        });
        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: detach.sequence,
            sequence_exhausted: false,
            bindings: Vec::new(),
        });
        assert_eq!(runtime.state(), ProviderRuntimeState::Closed);
    }

    #[test]
    fn prequeued_work_is_fenced_as_soon_as_close_linearizes() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        assert_eq!(
            port.publish_effect(bound_invoke(runtime.binding_id()))
                .delivered,
            1
        );
        runtime.close().unwrap();
        assert!(runtime.cancellation_token().is_cancelled());
        assert!(matches!(
            runtime.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn maximum_sequence_retires_binding_without_local_zombie() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        lock(&authority.state.inner).next_sequence = u64::MAX;

        assert_eq!(runtime.close().unwrap(), u64::MAX);
        assert!(authority.sequence_exhausted());
        let BackendIngress::ToolProvider(detach) = ingress_rx.try_recv().unwrap() else {
            panic!("expected terminal detach ingress");
        };
        assert_eq!(detach.sequence, u64::MAX);

        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: u64::MAX,
            sequence_exhausted: true,
            bindings: Vec::new(),
        });
        assert_eq!(runtime.state(), ProviderRuntimeState::Closed);
        assert_eq!(authority.active_binding_count(), 0);
        assert!(matches!(
            authority.bind_provider(provider_id(), 1),
            Err(ProviderRuntimeError::CounterExhausted {
                counter: "provider-runtime-sequence"
            })
        ));
    }

    #[test]
    fn foreign_binding_is_quarantined_until_canonical_absence() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: 9,
            sequence_exhausted: false,
            bindings: vec![ProviderRuntimeBindingSnapshot {
                binding_id: ProviderBindingId(9),
                provider_id: provider_id(),
            }],
        });
        assert!(matches!(
            authority.bind_provider(provider_id(), 1),
            Err(ProviderRuntimeError::AlreadyBound)
        ));

        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: 9,
            sequence_exhausted: false,
            bindings: Vec::new(),
        });
        let replacement = authority.bind_provider(provider_id(), 1).unwrap();
        assert_eq!(replacement.binding_id(), ProviderBindingId(10));
        let BackendIngress::ToolProvider(attach) = ingress_rx.try_recv().unwrap() else {
            panic!("expected reconciled attach ingress");
        };
        assert_eq!(attach.sequence, 10);
    }

    #[test]
    fn cancel_marks_only_exact_operation_before_cancel_work_is_received() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        let first_effect = bound_invoke_with_operation(runtime.binding_id(), 1);
        let second_effect = bound_invoke_with_operation(runtime.binding_id(), 2);
        assert_eq!(port.publish_effect(first_effect.clone()).delivered, 1);
        assert_eq!(port.publish_effect(second_effect).delivered, 1);
        let ProviderWork::Invoke(first) = runtime.try_recv().unwrap() else {
            panic!("expected first invocation");
        };
        let ProviderWork::Invoke(second) = runtime.try_recv().unwrap() else {
            panic!("expected second invocation");
        };
        let first_token = first.cancellation_token();
        let second_token = second.cancellation_token();
        let binding_token = runtime.cancellation_token();

        let cancel = bound_cancel(&first_effect, 3, InvocationCancelReason::GrantRevoked);
        assert_eq!(port.publish_effect(cancel).delivered, 1);
        assert!(first_token.is_cancelled());
        assert!(!second_token.is_cancelled());
        assert!(!binding_token.is_cancelled());
        let ProviderWork::Cancel(cancel) = runtime.try_recv().unwrap() else {
            panic!("expected exact cancellation");
        };
        assert_eq!(cancel.operation_id(), first.operation_id());
        assert_eq!(cancel.request_key(), first.request_key());
        assert_eq!(cancel.reason(), InvocationCancelReason::GrantRevoked);
    }

    #[test]
    fn ignored_observation_is_pruned_only_after_same_step_cancel_is_published() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        let effect = bound_invoke(runtime.binding_id());
        assert_eq!(port.publish_effect(effect.clone()).delivered, 1);
        let ProviderWork::Invoke(mut invocation) = runtime.try_recv().unwrap() else {
            panic!("expected invocation");
        };
        let token = invocation.cancellation_token();
        let sequence = runtime.try_succeed(&mut invocation, &result()).unwrap();
        let _observation = ingress_rx.try_recv().unwrap();

        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence,
            binding_id: runtime.binding_id(),
            provider_id: provider_id(),
            result: Ok(ProviderRuntimeTransition::ObservationIgnored {
                operation_id: invocation.operation_id(),
                request_key: invocation.request_key().clone(),
                reason: ObservationIgnoredReason::RequestNotDispatched,
            }),
        });
        assert!(!token.is_cancelled());
        assert_eq!(lock(&runtime.state.operations).len(), 1);
        assert_eq!(
            port.publish_effect(bound_cancel(
                &effect,
                effect.effect.sequence + 1,
                InvocationCancelReason::GrantRevoked,
            ))
            .delivered,
            1
        );
        assert!(token.is_cancelled());
        port.finish_step();
        assert!(lock(&runtime.state.operations).is_empty());
        assert_eq!(runtime.state(), ProviderRuntimeState::Active);
        assert!(!runtime.cancellation_token().is_cancelled());
    }

    #[test]
    fn dropping_provider_runtime_port_retires_retained_handles_and_pending_receipts() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        let first_effect = bound_invoke_with_operation(runtime.binding_id(), 1);
        let second_effect = bound_invoke_with_operation(runtime.binding_id(), 2);
        assert_eq!(port.publish_effect(first_effect).delivered, 1);
        assert_eq!(port.publish_effect(second_effect).delivered, 1);
        let ProviderWork::Invoke(mut submitted) = runtime.try_recv().unwrap() else {
            panic!("expected submitted invocation");
        };
        let ProviderWork::Invoke(mut retained) = runtime.try_recv().unwrap() else {
            panic!("expected retained invocation");
        };
        let submitted_token = submitted.cancellation_token();
        let retained_token = retained.cancellation_token();
        let completion = runtime.completion_handle();
        let pending_sequence = runtime.try_succeed(&mut submitted, &result()).unwrap();
        let _observation = ingress_rx.try_recv().unwrap();

        drop(port);

        assert_eq!(runtime.state(), ProviderRuntimeState::Closed);
        assert!(runtime.cancellation_token().is_cancelled());
        assert!(submitted_token.is_cancelled());
        assert!(retained_token.is_cancelled());
        let receipt = runtime.try_recv_observation_outcome().unwrap();
        assert_eq!(receipt.sequence, pending_sequence);
        assert_eq!(receipt.status, ProviderObservationStatus::RuntimeClosed);
        assert!(matches!(
            runtime.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
        assert_eq!(
            completion.try_succeed(&mut retained, &result()),
            Err(ProviderRuntimeError::Disconnected)
        );
        assert!(matches!(
            authority.bind_provider(provider_id(), 1),
            Err(ProviderRuntimeError::Disconnected)
        ));
    }

    #[test]
    fn disconnected_ingress_fences_all_provider_bindings() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        let effect = bound_invoke(runtime.binding_id());
        assert_eq!(port.publish_effect(effect).delivered, 1);
        let ProviderWork::Invoke(mut invocation) = runtime.try_recv().unwrap() else {
            panic!("expected invocation");
        };
        let token = invocation.cancellation_token();
        drop(ingress_rx);

        assert_eq!(
            runtime.try_succeed(&mut invocation, &result()),
            Err(ProviderRuntimeError::Disconnected)
        );
        assert_eq!(runtime.state(), ProviderRuntimeState::Closed);
        assert!(token.is_cancelled());
        assert!(matches!(
            authority.bind_provider(provider_id(), 1),
            Err(ProviderRuntimeError::Disconnected)
        ));
    }

    #[test]
    fn detach_retries_after_block_and_sequence_resync_until_canonical_absence() {
        let (ingress_tx, ingress_rx) = sync_channel(16);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        let binding_id = runtime.binding_id();
        let first_detach = runtime.close().unwrap();
        let _first = ingress_rx.try_recv().unwrap();
        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence: first_detach,
            binding_id,
            provider_id: provider_id(),
            result: Err(KernelProviderError::IntegrationBlocked),
        });
        assert_eq!(runtime.state(), ProviderRuntimeState::Closing);

        port.flush_closing_bindings();
        let BackendIngress::ToolProvider(second) = ingress_rx.try_recv().unwrap() else {
            panic!("expected retried detach");
        };
        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence: second.sequence,
            binding_id,
            provider_id: provider_id(),
            result: Err(KernelProviderError::SequenceRegressed {
                current: 9,
                requested: second.sequence,
            }),
        });
        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: 9,
            sequence_exhausted: false,
            bindings: vec![ProviderRuntimeBindingSnapshot {
                binding_id,
                provider_id: provider_id(),
            }],
        });
        port.flush_closing_bindings();
        let BackendIngress::ToolProvider(final_detach) = ingress_rx.try_recv().unwrap() else {
            panic!("expected resynchronized detach");
        };
        assert_eq!(final_detach.sequence, 10);
        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence: final_detach.sequence,
            binding_id,
            provider_id: provider_id(),
            result: Ok(ProviderRuntimeTransition::Detached {
                closed_request_count: 0,
            }),
        });
        port.reconcile_snapshot(&ProviderRuntimeSnapshot {
            last_sequence: 10,
            sequence_exhausted: false,
            bindings: Vec::new(),
        });
        assert_eq!(runtime.state(), ProviderRuntimeState::Closed);
        assert_eq!(authority.active_binding_count(), 0);
    }

    #[test]
    fn mismatched_cancel_correlation_faults_binding_without_retargeting() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        let effect = bound_invoke(runtime.binding_id());
        assert_eq!(port.publish_effect(effect.clone()).delivered, 1);
        let ProviderWork::Invoke(invocation) = runtime.try_recv().unwrap() else {
            panic!("expected invocation");
        };
        let token = invocation.cancellation_token();
        let mut mismatched = bound_cancel(&effect, 2, InvocationCancelReason::GrantRevoked);
        mismatched.effect.request_key.local_id = CapabilityRequestId(99);

        let report = port.publish_effect(mismatched);
        assert_eq!(report.disconnected, 1);
        assert_eq!(runtime.state(), ProviderRuntimeState::Closing);
        assert!(runtime.cancellation_token().is_cancelled());
        assert!(token.is_cancelled());
        assert!(lock(&runtime.state.operations).is_empty());
        assert!(matches!(
            runtime.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn work_queue_overflow_cancels_registered_operations_without_zombies() {
        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = authority.bind_provider(provider_id(), 1).unwrap();
        let BackendIngress::ToolProvider(attach) = ingress_rx.try_recv().unwrap() else {
            panic!("expected attach ingress");
        };
        port.publish_outcome(&ProviderRuntimeCommandOutcome {
            sequence: attach.sequence,
            binding_id: runtime.binding_id(),
            provider_id: provider_id(),
            result: Ok(ProviderRuntimeTransition::Attached),
        });
        assert_eq!(
            port.publish_effect(bound_invoke_with_operation(runtime.binding_id(), 1))
                .delivered,
            1
        );
        let ProviderWork::Invoke(first) = runtime.try_recv().unwrap() else {
            panic!("expected first invocation");
        };
        let first_token = first.cancellation_token();
        assert_eq!(
            port.publish_effect(bound_invoke_with_operation(runtime.binding_id(), 2))
                .delivered,
            1
        );

        let report = port.publish_effect(bound_invoke_with_operation(runtime.binding_id(), 3));
        assert_eq!(report.queue_full, 1);
        assert_eq!(runtime.state(), ProviderRuntimeState::Closing);
        assert!(first_token.is_cancelled());
        assert!(runtime.cancellation_token().is_cancelled());
        assert!(lock(&runtime.state.operations).is_empty());
    }

    #[test]
    fn observation_submission_racing_port_drop_is_linearizable() {
        use std::sync::Barrier;

        let (ingress_tx, ingress_rx) = sync_channel(8);
        let (authority, port) = provider_runtime(ingress_tx);
        let runtime = activate(&authority, &port, &ingress_rx);
        assert_eq!(
            port.publish_effect(bound_invoke(runtime.binding_id()))
                .delivered,
            1
        );
        let ProviderWork::Invoke(mut invocation) = runtime.try_recv().unwrap() else {
            panic!("expected invocation");
        };
        let token = invocation.cancellation_token();
        let completion = runtime.completion_handle();
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);

        let completion_result = std::thread::scope(|scope| {
            let worker = scope.spawn(move || {
                worker_barrier.wait();
                completion.try_succeed(&mut invocation, &result())
            });
            barrier.wait();
            drop(port);
            worker.join().unwrap()
        });

        assert!(matches!(
            completion_result,
            Ok(_) | Err(ProviderRuntimeError::Disconnected)
        ));
        assert_eq!(runtime.state(), ProviderRuntimeState::Closed);
        assert!(token.is_cancelled());
        assert!(matches!(
            authority.bind_provider(provider_id(), 1),
            Err(ProviderRuntimeError::Disconnected)
        ));
    }
}
