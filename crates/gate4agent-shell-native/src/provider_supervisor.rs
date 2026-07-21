use gate4agent_handle::{
    ProviderCancellation, ProviderCancellationToken, ProviderInvocation,
    ProviderObservationOutcome, ProviderObservationStatus, ProviderRuntimeError,
    ProviderRuntimeHandle, ProviderRuntimeState, ProviderWork,
};
use gate4agent_tool_protocol::{
    CapabilityObservation, CapabilityProviderDescriptor, CapabilityRequestKey,
    InvocationCancelReason, ProviderBindingId, ToolCapabilityId, ToolFailure, ToolFailureKind,
    ToolOperationId, ToolProviderId,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

pub const MAX_PROVIDER_SUPERVISOR_EVENTS: usize = 1_024;
pub const MAX_PROVIDER_SUPERVISOR_OPERATIONS: usize = 1_024;
pub const MAX_PROVIDER_SUPERVISOR_TOMBSTONES: usize =
    MAX_PROVIDER_SUPERVISOR_OPERATIONS * 2;
pub const MAX_PROVIDER_SUPERVISOR_OUTCOMES_PER_TICK: usize = 64;
pub const MAX_PROVIDER_SUPERVISOR_WORK_PER_TICK: usize = 64;
pub const MAX_PROVIDER_STOP_SIGNAL_ATTEMPTS: u8 = 3;
pub const MAX_PROVIDER_FORCE_STOP_ATTEMPTS: u8 = 3;
pub const DEFAULT_PROVIDER_STOP_GRACE: Duration = Duration::from_secs(5);

/// A reviewed provider adapter with fixed launch policy.
///
/// The supervisor deliberately offers no command, shell, filesystem, or
/// working-directory launch surface. Implementations map the already-admitted
/// invocation to one provider-specific operation.
pub trait NativeProviderExecutor: Send {
    fn start(
        &mut self,
        invocation: &ProviderInvocation,
    ) -> Result<Box<dyn NativeProviderOperation>, ToolFailure>;
}

/// Provider-specific ownership of one physical operation.
///
/// `request_stop` asks for cooperative shutdown. `request_force_stop` must
/// terminate the entire provider-owned resource tree after the bounded grace
/// window. `try_wait` is the sole source of physical-exit truth and must
/// return `Some` only after every owned OS resource has been reaped. A failed
/// stop request does not release the implementer from continuing to service
/// `try_wait`. Both stop methods must be idempotent.
///
/// Implementations must also make their own `Drop` fail-safe for live native
/// resources. Dropping [`ProviderSupervisor`] makes one best-effort force-stop
/// request, but is not the clean shutdown protocol and cannot prove reaping.
pub trait NativeProviderOperation: Send {
    fn request_stop(&mut self) -> Result<(), NativeProviderOperationError>;

    fn request_force_stop(&mut self) -> Result<(), NativeProviderOperationError>;

    fn try_wait(&mut self) -> Result<Option<NativeProviderExit>, NativeProviderOperationError>;

    fn try_poll_result(
        &mut self,
    ) -> Result<NativeProviderResultPoll, NativeProviderOperationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeProviderOperationError {
    pub code: &'static str,
}

impl NativeProviderOperationError {
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeProviderResultPoll {
    Pending,
    Ready(CapabilityObservation),
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeProviderExit {
    pub success: bool,
    pub code: Option<i32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderOperationKey {
    pub binding_id: ProviderBindingId,
    pub operation_id: ToolOperationId,
    pub request_key: CapabilityRequestKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicalExitAck {
    pub provider_id: ToolProviderId,
    pub binding_id: ProviderBindingId,
    pub operation: ProviderOperationKey,
    pub exit: NativeProviderExit,
    pub stop_signal_attempted: bool,
    pub stop_signalled: bool,
    pub force_stop_attempted: bool,
    pub force_stop_signalled: bool,
    pub stop_cause: Option<ProviderStopCause>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStopCause {
    Cancellation {
        reason: InvocationCancelReason,
    },
    Retirement,
    SupervisorFault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSupervisorState {
    Running,
    Draining,
    Closing,
    Closed,
    Faulted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderOperationSnapshot {
    pub operation: ProviderOperationKey,
    pub capability_id: ToolCapabilityId,
    pub deadline_tick: u64,
    pub stop_signal_attempted: bool,
    pub stop_signalled: bool,
    pub force_stop_attempted: bool,
    pub force_stop_signalled: bool,
    pub stop_cause: Option<ProviderStopCause>,
    pub physical_exit: Option<NativeProviderExit>,
    pub result_ready: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSupervisorSnapshot {
    pub provider_id: ToolProviderId,
    pub binding_id: ProviderBindingId,
    pub state: ProviderSupervisorState,
    pub runtime_state: ProviderRuntimeState,
    pub operations: Vec<ProviderOperationSnapshot>,
    pub close_sequence: Option<u64>,
    pub buffered_exit_acks: usize,
    pub buffered_faults: usize,
    pub dropped_faults: u64,
    pub exit_ack_backpressured: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSupervisorBuildError {
    InvalidDescriptor,
    ProviderMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSupervisorFaultKind {
    InvalidExecutorFailure,
    InvalidExecutorObservation,
    StopSignalFailed,
    ForceStopFailed,
    WaitFailed,
    ResultPollFailed,
    OperationCapacityExceeded,
    ConflictingInvocation,
    UnknownCancellation,
    CancellationIdentityMismatch,
    ObservationOutcomeRejected,
    ObservationOutcomeContractViolation,
    ObservationOutcomeRuntimeClosed,
    ObservationOutcomeMismatch,
    TombstoneCapacityExceeded,
    RuntimeQuiesceFailed(ProviderRuntimeError),
    RuntimeCompletionFailed(ProviderRuntimeError),
    RuntimeCloseFailed(ProviderRuntimeError),
    FaultBufferOverflow { dropped: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSupervisorFault {
    pub provider_id: ToolProviderId,
    pub binding_id: ProviderBindingId,
    pub operation: Option<ProviderOperationKey>,
    pub kind: ProviderSupervisorFaultKind,
    pub executor_error_code: Option<&'static str>,
    pub blocks_detach: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProviderSupervisorTick {
    pub work_received: usize,
    pub observation_outcomes_received: usize,
    pub operations_started: usize,
    pub stop_signals: usize,
    pub force_stop_signals: usize,
    pub physical_exit_acks: usize,
    pub observations_submitted: usize,
    pub faults_recorded: usize,
    pub close_requested: bool,
}

/// Owns native provider operations through physical exit and canonical
/// observation disposition.
///
/// Clean shutdown requires `begin_retirement` followed by ticks until
/// [`ProviderSupervisorState::Closed`]. `Drop` only sends one best-effort
/// force-stop request to each still-live physical owner; it does not wait or
/// prove that an OS resource was reaped.
pub struct ProviderSupervisor {
    descriptor: CapabilityProviderDescriptor,
    owned: BTreeMap<ToolOperationId, OwnedProviderOperation>,
    executor: Box<dyn NativeProviderExecutor>,
    runtime: ProviderRuntimeHandle,
    stop_grace: Duration,
    observation_capacity: usize,
    outstanding_observations: usize,
    lifecycle: ProviderSupervisorState,
    retirement_requested: bool,
    detach_blocked: bool,
    tombstones: VecDeque<CompletedOperationTombstone>,
    close_sequence: Option<u64>,
    exit_acks: VecDeque<PhysicalExitAck>,
    faults: VecDeque<ProviderSupervisorFault>,
    dropped_faults: u64,
}

enum OwnedProviderOperation {
    Physical(OwnedPhysicalOperation),
    PendingCompletion(PendingCompletion),
    AwaitingObservationOutcome(AwaitingObservationOutcome),
    AwaitingCancellation(AwaitingCancellation),
}

struct OwnedPhysicalOperation {
    key: ProviderOperationKey,
    capability_id: ToolCapabilityId,
    deadline_tick: u64,
    invocation: ProviderInvocation,
    operation: Box<dyn NativeProviderOperation>,
    stop_signal_attempted: bool,
    stop_signal_attempts: u8,
    stop_signalled: bool,
    stop_signalled_at: Option<Instant>,
    force_stop_attempts: u8,
    force_stop_signalled: bool,
    stop_cause: Option<ProviderStopCause>,
    cancellation_received: bool,
    result: Option<CapabilityObservation>,
    result_closed: bool,
    physical_exit: Option<NativeProviderExit>,
    exit_ack_emitted: bool,
}

struct PendingCompletion {
    key: ProviderOperationKey,
    capability_id: ToolCapabilityId,
    deadline_tick: u64,
    invocation: ProviderInvocation,
    observation: CapabilityObservation,
}

struct AwaitingObservationOutcome {
    key: ProviderOperationKey,
    capability_id: ToolCapabilityId,
    deadline_tick: u64,
    sequence: u64,
    cancellation_token: ProviderCancellationToken,
    cancellation_reason: Option<InvocationCancelReason>,
}

struct AwaitingCancellation {
    key: ProviderOperationKey,
    capability_id: ToolCapabilityId,
    deadline_tick: u64,
}

struct CompletedOperationTombstone {
    key: ProviderOperationKey,
}

impl ProviderSupervisor {
    pub fn new(
        descriptor: CapabilityProviderDescriptor,
        runtime: ProviderRuntimeHandle,
        executor: Box<dyn NativeProviderExecutor>,
    ) -> Result<Self, ProviderSupervisorBuildError> {
        Self::new_with_stop_grace(descriptor, runtime, executor, DEFAULT_PROVIDER_STOP_GRACE)
    }

    pub fn new_with_stop_grace(
        descriptor: CapabilityProviderDescriptor,
        runtime: ProviderRuntimeHandle,
        executor: Box<dyn NativeProviderExecutor>,
        stop_grace: Duration,
    ) -> Result<Self, ProviderSupervisorBuildError> {
        descriptor
            .validate()
            .map_err(|_| ProviderSupervisorBuildError::InvalidDescriptor)?;
        if &descriptor.id != runtime.provider_id() {
            return Err(ProviderSupervisorBuildError::ProviderMismatch);
        }
        let observation_capacity = runtime.effect_capacity();
        Ok(Self {
            descriptor,
            owned: BTreeMap::new(),
            executor,
            runtime,
            stop_grace: if stop_grace.is_zero() {
                Duration::from_millis(1)
            } else {
                stop_grace
            },
            observation_capacity,
            outstanding_observations: 0,
            lifecycle: ProviderSupervisorState::Running,
            retirement_requested: false,
            detach_blocked: false,
            tombstones: VecDeque::new(),
            close_sequence: None,
            exit_acks: VecDeque::new(),
            faults: VecDeque::new(),
            dropped_faults: 0,
        })
    }

    pub fn descriptor(&self) -> &CapabilityProviderDescriptor {
        &self.descriptor
    }

    pub fn begin_retirement(&mut self) -> Result<(), ProviderRuntimeError> {
        if self.retirement_requested {
            return Ok(());
        }
        self.retirement_requested = true;
        if matches!(self.runtime.state(), ProviderRuntimeState::Closed) && self.owned.is_empty() {
            self.detach_blocked = false;
            self.lifecycle = ProviderSupervisorState::Closed;
            return Ok(());
        }
        self.lifecycle = ProviderSupervisorState::Draining;
        for owned in self.owned.values_mut() {
            if let OwnedProviderOperation::Physical(owned) = owned {
                if owned.physical_exit.is_none()
                    && !owned.stop_signal_attempted
                    && owned.stop_cause.is_none()
                {
                    owned.stop_cause = Some(ProviderStopCause::Retirement);
                }
            }
        }
        if matches!(self.runtime.state(), ProviderRuntimeState::Closed) {
            return Ok(());
        }
        if matches!(self.runtime.state(), ProviderRuntimeState::Attaching)
            && self.owned.is_empty()
        {
            let mut report = ProviderSupervisorTick::default();
            return self.request_runtime_close(&mut report);
        }
        match self.runtime.begin_quiesce() {
            Ok(()) => Ok(()),
            Err(ProviderRuntimeError::Inactive)
                if matches!(self.runtime.state(), ProviderRuntimeState::Closing) =>
            {
                Ok(())
            }
            Err(error) => {
                let mut report = ProviderSupervisorTick::default();
                self.record_blocking_fault(
                    None,
                    ProviderSupervisorFaultKind::RuntimeQuiesceFailed(error),
                    None,
                    &mut report,
                );
                Err(error)
            }
        }
    }

    pub fn snapshot(&self) -> ProviderSupervisorSnapshot {
        ProviderSupervisorSnapshot {
            provider_id: self.descriptor.id.clone(),
            binding_id: self.runtime.binding_id(),
            state: self.state(),
            runtime_state: self.runtime.state(),
            operations: self
                .owned
                .values()
                .map(OwnedProviderOperation::snapshot)
                .collect(),
            close_sequence: self.close_sequence,
            buffered_exit_acks: self.exit_acks.len(),
            buffered_faults: self.faults.len() + usize::from(self.dropped_faults != 0),
            dropped_faults: self.dropped_faults,
            exit_ack_backpressured: self
                .owned
                .values()
                .any(OwnedProviderOperation::exit_ack_backpressured),
        }
    }

    pub fn state(&self) -> ProviderSupervisorState {
        if self.detach_blocked {
            ProviderSupervisorState::Faulted
        } else {
            self.lifecycle
        }
    }

    pub fn drain_exit_acks(&mut self, limit: usize) -> Vec<PhysicalExitAck> {
        drain_queue(&mut self.exit_acks, limit)
    }

    pub fn drain_faults(&mut self, limit: usize) -> Vec<ProviderSupervisorFault> {
        if limit == 0 {
            return Vec::new();
        }
        let mut drained = Vec::new();
        if self.dropped_faults != 0 {
            let dropped = std::mem::take(&mut self.dropped_faults);
            drained.push(ProviderSupervisorFault {
                provider_id: self.descriptor.id.clone(),
                binding_id: self.runtime.binding_id(),
                operation: None,
                kind: ProviderSupervisorFaultKind::FaultBufferOverflow { dropped },
                executor_error_code: None,
                blocks_detach: false,
            });
        }
        drained.extend(drain_queue(&mut self.faults, limit - drained.len()));
        drained
    }

    pub fn tick(&mut self) -> ProviderSupervisorTick {
        let mut report = ProviderSupervisorTick::default();
        if matches!(self.lifecycle, ProviderSupervisorState::Closed) {
            return report;
        }

        self.drain_observation_outcomes(&mut report);

        let runtime_closed = matches!(self.runtime.state(), ProviderRuntimeState::Closed);
        if runtime_closed {
            if !self.owned.is_empty() && !self.detach_blocked {
                self.record_blocking_fault(
                    None,
                    ProviderSupervisorFaultKind::RuntimeCompletionFailed(
                        ProviderRuntimeError::Inactive,
                    ),
                    None,
                    &mut report,
                );
            }
            self.outstanding_observations = 0;
        }

        let operation_ids = self.owned.keys().copied().collect::<Vec<_>>();
        for operation_id in operation_ids {
            let Some(mut owned) = self.owned.remove(&operation_id) else {
                continue;
            };
            if runtime_closed && !owned.has_physical_owner() {
                continue;
            }
            if runtime_closed {
                owned.fence_for_runtime_close(self.retirement_requested);
            }
            if let Some(owned) = self.tick_owned(owned, &mut report) {
                self.owned.insert(operation_id, owned);
            }
        }

        if runtime_closed {
            if self.owned.is_empty() {
                self.lifecycle = ProviderSupervisorState::Closed;
                if self.retirement_requested {
                    self.detach_blocked = false;
                }
            }
            return report;
        }

        if self.retirement_requested {
            self.lifecycle = if self.close_sequence.is_some() {
                ProviderSupervisorState::Closing
            } else {
                ProviderSupervisorState::Draining
            };
            if !matches!(
                self.runtime.state(),
                ProviderRuntimeState::Attaching | ProviderRuntimeState::Closed
            ) {
                self.drain_work(&mut report);
            }
            if self.owned.is_empty() {
                self.detach_blocked = false;
                let _ = self.request_runtime_close(&mut report);
            }
            return report;
        }

        if !matches!(
            self.runtime.state(),
            ProviderRuntimeState::Attaching | ProviderRuntimeState::Closed
        ) {
            self.drain_work(&mut report);
        }
        report
    }

    fn drain_observation_outcomes(&mut self, report: &mut ProviderSupervisorTick) {
        for _ in 0..MAX_PROVIDER_SUPERVISOR_OUTCOMES_PER_TICK {
            match self.runtime.try_recv_observation_outcome() {
                Ok(outcome) => {
                    report.observation_outcomes_received += 1;
                    self.handle_observation_outcome(outcome, report);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    fn handle_observation_outcome(
        &mut self,
        outcome: ProviderObservationOutcome,
        report: &mut ProviderSupervisorTick,
    ) {
        if self.outstanding_observations == 0 {
            self.record_blocking_fault(
                None,
                ProviderSupervisorFaultKind::ObservationOutcomeMismatch,
                None,
                report,
            );
        } else {
            self.outstanding_observations -= 1;
        }
        let operation_id = outcome.operation_id;
        let Some(mut owned) = self.owned.remove(&operation_id) else {
            self.record_blocking_fault(
                None,
                ProviderSupervisorFaultKind::ObservationOutcomeMismatch,
                None,
                report,
            );
            return;
        };
        let OwnedProviderOperation::AwaitingObservationOutcome(awaiting) = owned else {
            owned.fence_for_fault(self.retirement_requested);
            let operation = owned.key().clone();
            self.owned.insert(operation_id, owned);
            self.record_blocking_fault(
                Some(operation),
                ProviderSupervisorFaultKind::ObservationOutcomeMismatch,
                None,
                report,
            );
            return;
        };
        if awaiting.sequence != outcome.sequence
            || awaiting.key.operation_id != outcome.operation_id
            || awaiting.key.request_key != outcome.request_key
        {
            let operation = awaiting.key.clone();
            if awaiting.cancellation_reason.is_none()
                && awaiting.cancellation_token.is_cancelled()
            {
                self.push_tombstone(awaiting.key, report);
            }
            self.record_blocking_fault(
                Some(operation),
                ProviderSupervisorFaultKind::ObservationOutcomeMismatch,
                None,
                report,
            );
            return;
        }

        match outcome.status {
            ProviderObservationStatus::Applied | ProviderObservationStatus::Ignored { .. } => {
                if awaiting.cancellation_reason.is_none()
                    && awaiting.cancellation_token.is_cancelled()
                {
                    self.push_tombstone(awaiting.key, report);
                }
            }
            status => {
                let kind = match status {
                    ProviderObservationStatus::Rejected(_) => {
                        ProviderSupervisorFaultKind::ObservationOutcomeRejected
                    }
                    ProviderObservationStatus::ContractViolation => {
                        ProviderSupervisorFaultKind::ObservationOutcomeContractViolation
                    }
                    ProviderObservationStatus::RuntimeClosed => {
                        ProviderSupervisorFaultKind::ObservationOutcomeRuntimeClosed
                    }
                    ProviderObservationStatus::Applied
                    | ProviderObservationStatus::Ignored { .. } => unreachable!(),
                };
                let operation = awaiting.key.clone();
                if awaiting.cancellation_reason.is_none()
                    && awaiting.cancellation_token.is_cancelled()
                {
                    self.push_tombstone(awaiting.key, report);
                }
                self.record_blocking_fault(Some(operation), kind, None, report);
            }
        }
    }

    fn tick_owned(
        &mut self,
        owned: OwnedProviderOperation,
        report: &mut ProviderSupervisorTick,
    ) -> Option<OwnedProviderOperation> {
        match owned {
            OwnedProviderOperation::Physical(mut owned) => {
                if self.retirement_requested
                    && owned.physical_exit.is_none()
                    && !owned.stop_signal_attempted
                    && owned.stop_cause.is_none()
                {
                    owned.stop_cause = Some(ProviderStopCause::Retirement);
                }
                if self.detach_blocked
                    && owned.physical_exit.is_none()
                    && !owned.stop_signal_attempted
                    && owned.stop_cause.is_none()
                {
                    owned.stop_cause = Some(ProviderStopCause::SupervisorFault);
                }
                let cancellation_requested =
                    owned.invocation.cancellation_token().is_cancelled();
                let stop_required = self.retirement_requested
                    || self.detach_blocked
                    || cancellation_requested
                    || owned.stop_cause.is_some();
                if owned.physical_exit.is_none()
                    && stop_required
                    && !owned.stop_signalled
                    && owned.stop_signal_attempts < MAX_PROVIDER_STOP_SIGNAL_ATTEMPTS
                {
                    owned.stop_signal_attempted = true;
                    owned.stop_signal_attempts += 1;
                    report.stop_signals += 1;
                    match owned.operation.request_stop() {
                        Ok(()) => {
                            owned.stop_signalled = true;
                            owned.stop_signalled_at = Some(Instant::now());
                        }
                        Err(error) => {
                            if owned.stop_signal_attempts
                                == MAX_PROVIDER_STOP_SIGNAL_ATTEMPTS
                            {
                                self.record_blocking_fault(
                                    Some(owned.key.clone()),
                                    ProviderSupervisorFaultKind::StopSignalFailed,
                                    Some(error.code),
                                    report,
                                );
                            } else {
                                self.record_fault(
                                    Some(owned.key.clone()),
                                    ProviderSupervisorFaultKind::StopSignalFailed,
                                    Some(error.code),
                                    false,
                                    report,
                                );
                            }
                        }
                    }
                }
                let force_stop_required = owned.physical_exit.is_none()
                    && stop_required
                    && !owned.force_stop_signalled
                    && ((!owned.stop_signalled
                        && owned.stop_signal_attempts
                            == MAX_PROVIDER_STOP_SIGNAL_ATTEMPTS)
                        || owned
                            .stop_signalled_at
                            .is_some_and(|started| started.elapsed() >= self.stop_grace));
                if force_stop_required
                    && owned.force_stop_attempts < MAX_PROVIDER_FORCE_STOP_ATTEMPTS
                {
                    owned.force_stop_attempts += 1;
                    report.force_stop_signals += 1;
                    match owned.operation.request_force_stop() {
                        Ok(()) => owned.force_stop_signalled = true,
                        Err(error) => {
                            if owned.force_stop_attempts
                                == MAX_PROVIDER_FORCE_STOP_ATTEMPTS
                            {
                                self.record_blocking_fault(
                                    Some(owned.key.clone()),
                                    ProviderSupervisorFaultKind::ForceStopFailed,
                                    Some(error.code),
                                    report,
                                );
                            } else {
                                self.record_fault(
                                    Some(owned.key.clone()),
                                    ProviderSupervisorFaultKind::ForceStopFailed,
                                    Some(error.code),
                                    false,
                                    report,
                                );
                            }
                        }
                    }
                }

                if !cancellation_requested
                    && !owned.stop_signal_attempted
                    && owned.stop_cause.is_none()
                    && owned.result.is_none()
                    && !owned.result_closed
                {
                    match owned.operation.try_poll_result() {
                        Ok(NativeProviderResultPoll::Pending) => {}
                        Ok(NativeProviderResultPoll::Closed) => owned.result_closed = true,
                        Ok(NativeProviderResultPoll::Ready(observation)) => {
                            if observation.validate().is_ok() {
                                owned.result = Some(observation);
                            } else {
                                owned.result = Some(contract_violation_observation());
                                self.record_fault(
                                    Some(owned.key.clone()),
                                    ProviderSupervisorFaultKind::InvalidExecutorObservation,
                                    None,
                                    false,
                                    report,
                                );
                            }
                        }
                        Err(error) => {
                            owned.result = Some(contract_violation_observation());
                            owned.result_closed = true;
                            if !self.retirement_requested && owned.stop_cause.is_none() {
                                owned.stop_cause = Some(ProviderStopCause::SupervisorFault);
                            }
                            self.record_blocking_fault(
                                Some(owned.key.clone()),
                                ProviderSupervisorFaultKind::ResultPollFailed,
                                Some(error.code),
                                report,
                            );
                        }
                    }
                }

                if owned.physical_exit.is_none() {
                    match owned.operation.try_wait() {
                        Ok(Some(exit)) => owned.physical_exit = Some(exit),
                        Ok(None) => {}
                        Err(error) => {
                            if !self.retirement_requested
                                && !owned.stop_signal_attempted
                                && owned.stop_cause.is_none()
                            {
                                owned.stop_cause = Some(ProviderStopCause::SupervisorFault);
                            }
                            self.record_blocking_fault(
                                Some(owned.key.clone()),
                                ProviderSupervisorFaultKind::WaitFailed,
                                Some(error.code),
                                report,
                            );
                            return Some(OwnedProviderOperation::Physical(owned));
                        }
                    }
                }

                if let Some(exit) = owned.physical_exit {
                    if !owned.exit_ack_emitted {
                        if owned.stop_signal_attempted && owned.stop_cause.is_none() {
                            return Some(OwnedProviderOperation::Physical(owned));
                        }
                        if self.exit_acks.len() >= MAX_PROVIDER_SUPERVISOR_EVENTS {
                            return Some(OwnedProviderOperation::Physical(owned));
                        }
                        self.exit_acks.push_back(PhysicalExitAck {
                            provider_id: self.descriptor.id.clone(),
                            binding_id: self.runtime.binding_id(),
                            operation: owned.key.clone(),
                            exit,
                            stop_signal_attempted: owned.stop_signal_attempted,
                            stop_signalled: owned.stop_signalled,
                            force_stop_attempted: owned.force_stop_attempts != 0,
                            force_stop_signalled: owned.force_stop_signalled,
                            stop_cause: owned.stop_cause,
                        });
                        owned.exit_ack_emitted = true;
                        report.physical_exit_acks += 1;
                    }

                    if self.retirement_requested
                        || self.detach_blocked
                        || owned.cancellation_received
                        || owned.stop_cause.is_some()
                    {
                        if !owned.cancellation_received {
                            self.push_tombstone(owned.key, report);
                        }
                        return None;
                    }
                    if owned.result.is_none() && owned.result_closed {
                        owned.result = Some(contract_violation_observation());
                        self.record_fault(
                            Some(owned.key.clone()),
                            ProviderSupervisorFaultKind::InvalidExecutorObservation,
                            None,
                            false,
                            report,
                        );
                    }
                    if let Some(observation) = owned.result.take() {
                        return Some(OwnedProviderOperation::PendingCompletion(
                            PendingCompletion {
                                key: owned.key,
                                capability_id: owned.capability_id,
                                deadline_tick: owned.deadline_tick,
                                invocation: owned.invocation,
                                observation,
                            },
                        ));
                    }
                }
                Some(OwnedProviderOperation::Physical(owned))
            }
            OwnedProviderOperation::PendingCompletion(mut pending) => {
                if self.retirement_requested {
                    self.push_tombstone(pending.key, report);
                    return None;
                }
                if pending.invocation.cancellation_token().is_cancelled() {
                    return Some(OwnedProviderOperation::AwaitingCancellation(
                        AwaitingCancellation {
                            key: pending.key,
                            capability_id: pending.capability_id,
                            deadline_tick: pending.deadline_tick,
                        },
                    ));
                }
                if self.outstanding_observations >= self.observation_capacity {
                    return Some(OwnedProviderOperation::PendingCompletion(pending));
                }
                let cancellation_token = pending.invocation.cancellation_token();
                match self
                    .runtime
                    .try_complete(&mut pending.invocation, &pending.observation)
                {
                    Ok(sequence) => {
                        self.outstanding_observations += 1;
                        report.observations_submitted += 1;
                        Some(OwnedProviderOperation::AwaitingObservationOutcome(
                            AwaitingObservationOutcome {
                                key: pending.key,
                                capability_id: pending.capability_id,
                                deadline_tick: pending.deadline_tick,
                                sequence,
                                cancellation_token,
                                cancellation_reason: None,
                            },
                        ))
                    }
                    Err(ProviderRuntimeError::Full) => {
                        Some(OwnedProviderOperation::PendingCompletion(pending))
                    }
                    Err(error) => {
                        let operation = pending.key.clone();
                        self.push_tombstone(pending.key, report);
                        self.record_blocking_fault(
                            Some(operation),
                            ProviderSupervisorFaultKind::RuntimeCompletionFailed(error),
                            None,
                            report,
                        );
                        None
                    }
                }
            }
            OwnedProviderOperation::AwaitingObservationOutcome(awaiting) => {
                Some(OwnedProviderOperation::AwaitingObservationOutcome(awaiting))
            }
            OwnedProviderOperation::AwaitingCancellation(awaiting) => {
                if self.retirement_requested {
                    self.push_tombstone(awaiting.key, report);
                    None
                } else {
                    Some(OwnedProviderOperation::AwaitingCancellation(awaiting))
                }
            }
        }
    }

    fn drain_work(&mut self, report: &mut ProviderSupervisorTick) {
        for _ in 0..MAX_PROVIDER_SUPERVISOR_WORK_PER_TICK {
            match self.runtime.try_recv() {
                Ok(ProviderWork::Cancel(cancellation)) => {
                    report.work_received += 1;
                    self.handle_cancellation(cancellation, report);
                }
                Ok(ProviderWork::Invoke(invocation)) => {
                    report.work_received += 1;
                    if self.retirement_requested || self.detach_blocked {
                        self.push_tombstone(operation_key(&invocation), report);
                    } else {
                        self.start_invocation(invocation, report);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    fn start_invocation(
        &mut self,
        invocation: ProviderInvocation,
        report: &mut ProviderSupervisorTick,
    ) {
        let key = operation_key(&invocation);
        let capability_id = invocation.capability_id().clone();
        let deadline_tick = invocation.deadline_tick();
        if self.owned.contains_key(&key.operation_id)
            || self
                .tombstones
                .iter()
                .any(|tombstone| tombstone.key.operation_id == key.operation_id)
        {
            self.record_blocking_fault(
                Some(key),
                ProviderSupervisorFaultKind::ConflictingInvocation,
                None,
                report,
            );
            return;
        }
        if self.owned.len() >= MAX_PROVIDER_SUPERVISOR_OPERATIONS {
            self.record_blocking_fault(
                Some(key),
                ProviderSupervisorFaultKind::OperationCapacityExceeded,
                None,
                report,
            );
            return;
        }
        if !self.descriptor.has_capability(&capability_id) {
            self.record_fault(
                Some(key.clone()),
                ProviderSupervisorFaultKind::InvalidExecutorObservation,
                None,
                false,
                report,
            );
            self.owned.insert(key.operation_id, OwnedProviderOperation::PendingCompletion(
                PendingCompletion {
                    key: key.clone(),
                    capability_id,
                    deadline_tick,
                    invocation,
                    observation: contract_violation_observation(),
                },
            ));
            return;
        }

        if invocation.cancellation_token().is_cancelled() {
            self.owned.insert(
                key.operation_id,
                OwnedProviderOperation::AwaitingCancellation(AwaitingCancellation {
                    key,
                    capability_id,
                    deadline_tick,
                }),
            );
            return;
        }

        match self.executor.start(&invocation) {
            Ok(operation) => {
                report.operations_started += 1;
                self.owned.insert(key.operation_id, OwnedProviderOperation::Physical(OwnedPhysicalOperation {
                    key: key.clone(),
                    capability_id,
                    deadline_tick,
                    invocation,
                    operation,
                    stop_signal_attempted: false,
                    stop_signal_attempts: 0,
                    stop_signalled: false,
                    stop_signalled_at: None,
                    force_stop_attempts: 0,
                    force_stop_signalled: false,
                    stop_cause: None,
                    cancellation_received: false,
                    result: None,
                    result_closed: false,
                    physical_exit: None,
                    exit_ack_emitted: false,
                }));
            }
            Err(failure) => {
                let observation = if failure.validate().is_ok() {
                    CapabilityObservation::Failed { failure }
                } else {
                    self.record_fault(
                        Some(key.clone()),
                        ProviderSupervisorFaultKind::InvalidExecutorFailure,
                        None,
                        false,
                        report,
                    );
                    contract_violation_observation()
                };
                self.owned.insert(key.operation_id, OwnedProviderOperation::PendingCompletion(
                    PendingCompletion {
                        key: key.clone(),
                        capability_id,
                        deadline_tick,
                        invocation,
                        observation,
                    },
                ));
            }
        }
    }

    fn handle_cancellation(
        &mut self,
        cancellation: ProviderCancellation,
        report: &mut ProviderSupervisorTick,
    ) {
        let key = ProviderOperationKey {
            binding_id: cancellation.binding_id(),
            operation_id: cancellation.operation_id(),
            request_key: cancellation.request_key().clone(),
        };
        let Some(mut owned) = self.owned.remove(&key.operation_id) else {
            if let Some(index) = self
                .tombstones
                .iter()
                .position(|tombstone| tombstone.key.operation_id == key.operation_id)
            {
                if self.tombstones[index].key != key {
                    self.record_blocking_fault(
                        Some(key),
                        ProviderSupervisorFaultKind::CancellationIdentityMismatch,
                        None,
                        report,
                    );
                } else {
                    self.tombstones.remove(index);
                }
                return;
            }
            self.record_blocking_fault(
                Some(key),
                ProviderSupervisorFaultKind::UnknownCancellation,
                None,
                report,
            );
            return;
        };
        if owned.key() != &key {
            owned.fence_for_fault(self.retirement_requested);
            self.owned.insert(key.operation_id, owned);
            self.record_blocking_fault(
                Some(key),
                ProviderSupervisorFaultKind::CancellationIdentityMismatch,
                None,
                report,
            );
            return;
        }

        match owned {
            OwnedProviderOperation::Physical(mut owned) => {
                let stop_cause = ProviderStopCause::Cancellation {
                    reason: cancellation.reason(),
                };
                owned.cancellation_received = true;
                if owned.stop_cause.is_none()
                    && (owned.physical_exit.is_none() || owned.stop_signal_attempted)
                {
                    owned.stop_cause = Some(stop_cause);
                }
                if owned.exit_ack_emitted {
                    if let Some(ack) = self
                        .exit_acks
                        .iter_mut()
                        .find(|ack| ack.operation == key)
                    {
                        if ack.stop_cause.is_none()
                            && owned.stop_cause == Some(stop_cause)
                        {
                            ack.stop_cause = owned.stop_cause;
                        }
                    }
                }
                if let Some(owned) =
                    self.tick_owned(OwnedProviderOperation::Physical(owned), report)
                {
                    self.owned.insert(key.operation_id, owned);
                }
            }
            OwnedProviderOperation::PendingCompletion(_)
            | OwnedProviderOperation::AwaitingCancellation(_) => {}
            OwnedProviderOperation::AwaitingObservationOutcome(mut awaiting) => {
                awaiting.cancellation_reason = Some(cancellation.reason());
                self.owned.insert(
                    key.operation_id,
                    OwnedProviderOperation::AwaitingObservationOutcome(awaiting),
                );
            }
        }
    }

    fn push_tombstone(
        &mut self,
        key: ProviderOperationKey,
        report: &mut ProviderSupervisorTick,
    ) {
        if self.tombstones.len() >= MAX_PROVIDER_SUPERVISOR_TOMBSTONES {
            self.record_blocking_fault(
                Some(key),
                ProviderSupervisorFaultKind::TombstoneCapacityExceeded,
                None,
                report,
            );
            return;
        }
        self.tombstones.push_back(CompletedOperationTombstone { key });
    }

    fn request_runtime_close(
        &mut self,
        report: &mut ProviderSupervisorTick,
    ) -> Result<(), ProviderRuntimeError> {
        match self.runtime.close() {
            Ok(sequence) => {
                self.close_sequence = Some(sequence);
                self.lifecycle = ProviderSupervisorState::Closing;
                report.close_requested = true;
                Ok(())
            }
            Err(ProviderRuntimeError::Full) => {
                self.lifecycle = ProviderSupervisorState::Closing;
                Ok(())
            }
            Err(ProviderRuntimeError::Inactive)
                if matches!(self.runtime.state(), ProviderRuntimeState::Closed) =>
            {
                self.lifecycle = ProviderSupervisorState::Closed;
                Ok(())
            }
            Err(error) => {
                self.record_blocking_fault(
                    None,
                    ProviderSupervisorFaultKind::RuntimeCloseFailed(error),
                    None,
                    report,
                );
                Err(error)
            }
        }
    }

    fn record_blocking_fault(
        &mut self,
        operation: Option<ProviderOperationKey>,
        kind: ProviderSupervisorFaultKind,
        executor_error_code: Option<&'static str>,
        report: &mut ProviderSupervisorTick,
    ) {
        self.detach_blocked = true;
        for owned in self.owned.values_mut() {
            owned.fence_for_fault(self.retirement_requested);
        }
        let _ = self.runtime.begin_quiesce();
        self.record_fault(operation, kind, executor_error_code, true, report);
    }

    fn record_fault(
        &mut self,
        operation: Option<ProviderOperationKey>,
        kind: ProviderSupervisorFaultKind,
        executor_error_code: Option<&'static str>,
        blocks_detach: bool,
        report: &mut ProviderSupervisorTick,
    ) {
        if self.faults.len() >= MAX_PROVIDER_SUPERVISOR_EVENTS {
            self.dropped_faults = self.dropped_faults.saturating_add(1);
            report.faults_recorded += 1;
            return;
        }
        self.faults.push_back(ProviderSupervisorFault {
            provider_id: self.descriptor.id.clone(),
            binding_id: self.runtime.binding_id(),
            operation,
            kind,
            executor_error_code,
            blocks_detach,
        });
        report.faults_recorded += 1;
    }
}

impl OwnedProviderOperation {
    fn has_physical_owner(&self) -> bool {
        matches!(self, Self::Physical(_))
    }

    fn fence_for_fault(&mut self, retirement_requested: bool) {
        if let Self::Physical(owned) = self {
            if retirement_requested {
                if owned.physical_exit.is_none()
                    && !owned.stop_signal_attempted
                    && owned.stop_cause.is_none()
                {
                    owned.stop_cause = Some(ProviderStopCause::Retirement);
                }
            } else if owned.physical_exit.is_none()
                && !owned.stop_signal_attempted
                && owned.stop_cause.is_none()
            {
                owned.stop_cause = Some(ProviderStopCause::SupervisorFault);
            }
        }
    }

    fn fence_for_runtime_close(&mut self, retirement_requested: bool) {
        let Self::Physical(owned) = self else {
            return;
        };
        if owned.stop_cause.is_some() {
            return;
        }
        if owned.stop_signal_attempted {
            owned.stop_cause = Some(ProviderStopCause::SupervisorFault);
        } else if owned.physical_exit.is_none() {
            owned.stop_cause = Some(if retirement_requested {
                ProviderStopCause::Retirement
            } else {
                ProviderStopCause::SupervisorFault
            });
        }
    }

    fn key(&self) -> &ProviderOperationKey {
        match self {
            Self::Physical(owned) => &owned.key,
            Self::PendingCompletion(pending) => &pending.key,
            Self::AwaitingObservationOutcome(awaiting) => &awaiting.key,
            Self::AwaitingCancellation(awaiting) => &awaiting.key,
        }
    }

    fn snapshot(&self) -> ProviderOperationSnapshot {
        match self {
            Self::Physical(owned) => ProviderOperationSnapshot {
                operation: owned.key.clone(),
                capability_id: owned.capability_id.clone(),
                deadline_tick: owned.deadline_tick,
                stop_signal_attempted: owned.stop_signal_attempted,
                stop_signalled: owned.stop_signalled,
                force_stop_attempted: owned.force_stop_attempts != 0,
                force_stop_signalled: owned.force_stop_signalled,
                stop_cause: owned.stop_cause,
                physical_exit: owned.physical_exit,
                result_ready: owned.result.is_some(),
            },
            Self::PendingCompletion(pending) => ProviderOperationSnapshot {
                operation: pending.key.clone(),
                capability_id: pending.capability_id.clone(),
                deadline_tick: pending.deadline_tick,
                stop_signal_attempted: false,
                stop_signalled: false,
                force_stop_attempted: false,
                force_stop_signalled: false,
                stop_cause: None,
                physical_exit: None,
                result_ready: true,
            },
            Self::AwaitingObservationOutcome(awaiting) => ProviderOperationSnapshot {
                operation: awaiting.key.clone(),
                capability_id: awaiting.capability_id.clone(),
                deadline_tick: awaiting.deadline_tick,
                stop_signal_attempted: false,
                stop_signalled: false,
                force_stop_attempted: false,
                force_stop_signalled: false,
                stop_cause: awaiting
                    .cancellation_reason
                    .map(|reason| ProviderStopCause::Cancellation { reason }),
                physical_exit: None,
                result_ready: true,
            },
            Self::AwaitingCancellation(awaiting) => ProviderOperationSnapshot {
                operation: awaiting.key.clone(),
                capability_id: awaiting.capability_id.clone(),
                deadline_tick: awaiting.deadline_tick,
                stop_signal_attempted: false,
                stop_signalled: false,
                force_stop_attempted: false,
                force_stop_signalled: false,
                stop_cause: None,
                physical_exit: None,
                result_ready: false,
            },
        }
    }

    fn exit_ack_backpressured(&self) -> bool {
        matches!(
            self,
            Self::Physical(OwnedPhysicalOperation {
                physical_exit: Some(_),
                exit_ack_emitted: false,
                ..
            })
        )
    }
}

impl Drop for ProviderSupervisor {
    fn drop(&mut self) {
        for owned in self.owned.values_mut() {
            let OwnedProviderOperation::Physical(owned) = owned else {
                continue;
            };
            if owned.physical_exit.is_none() {
                owned.stop_signal_attempted = true;
                if owned.stop_cause.is_none() {
                    owned.stop_cause = Some(ProviderStopCause::Retirement);
                }
                let _ = owned.operation.request_force_stop();
            }
        }
    }
}

fn operation_key(invocation: &ProviderInvocation) -> ProviderOperationKey {
    ProviderOperationKey {
        binding_id: invocation.binding_id(),
        operation_id: invocation.operation_id(),
        request_key: invocation.request_key().clone(),
    }
}

fn contract_violation_observation() -> CapabilityObservation {
    CapabilityObservation::Failed {
        failure: ToolFailure {
            kind: ToolFailureKind::ProviderContractViolation,
            redacted_message: None,
        },
    }
}

fn drain_queue<T>(queue: &mut VecDeque<T>, limit: usize) -> Vec<T> {
    let count = limit.min(queue.len());
    queue.drain(..count).collect()
}
