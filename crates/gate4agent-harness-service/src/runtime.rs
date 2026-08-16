use crate::{
    c2::{
        AcceptedSpawnBindingProof, ActivatedHarnessMcpReservationProof,
        ArmedHarnessMcpReservationProof,
        ContextPackExportStart, ExportContextPackOutcome, HarnessC2Adapter,
        HarnessC2Error, HarnessC2EventReceiver,
        HarnessObservationResync, PendingNativeHistoryRequest, PendingRunRead,
        PreparedRunRead, RunReadCompletion,
        SpawnDispatchOutcome, SpawnProfileRevisionProof,
        StagedDeliveryProof,
    },
    credential::{CredentialAuthority, CredentialBindingV1, CredentialError},
    read::{
        execute_exact_binding_read, execute_operator_monitor, execute_operator_timeline,
        execute_read, verify_observation_credential_binding,
    },
    HarnessApplyOutcome, HarnessMutationV1, HarnessService, HarnessServiceError,
};
use crate::dispatch::{
    deterministic_dispatch_ids, deterministic_lifecycle_authority_ids,
    exact_bound_control_lifecycle,
    HarnessLaunchCatalog, HarnessLifecycleEventKindV1, HarnessLifecycleProjectionV1,
};
use gate4agent_harness_delivery::DeliveryCatalogV2;
use gate4agent_c2_protocol::{
    C2ManagedSessionRecord, C2NodeEvent, C2ObservationSupport, C2SessionStatus,
    NodeRoute, RoutedNodeEvent,
};
use gate4agent_observation_api::{
    ManagedRecordLink, ManagedSessionKey, NodeCursor, NodeId, NodeIncarnationId,
    ObservationGap, ObservationIngressEnvelope, ObservationIngressPayload,
    ObservationResyncBatch, ObservationTarget, ObservationTransport, RuntimeSessionKey,
};
use gate4agent_observation_service::{ObservationService, ObservationServiceError};
use gate4agent_harness_api::{
    HarnessInlineRunSessionV1, HarnessManagedRunSessionV1,
    HarnessLaunchPlanPageV1, HarnessLaunchPlanSummaryV1,
    HarnessNodeIncarnationV1,
    HarnessOperatorCredential, HarnessOperatorEnvelopeV1, HarnessOperatorHostErrorV1,
    HarnessOperatorIntentV1,
    HarnessOperatorMutationOutcomeV1, HarnessOperatorReplyV1, HarnessOperatorRequestV1,
    HarnessOperatorResponseV1, HarnessReadCredential, HarnessReadEnvelopeV1,
    HarnessRunContextTransferV1, HarnessRunContinuationTransferV1,
    HarnessRunCorrelationAvailabilityV1, HarnessRunCorrelationV1,
    HarnessRunDeliveryTransferV1, HarnessRunTransferSummaryV1,
    HarnessRunSessionViewV1, HarnessRunWorktreeViewV1,
    HarnessRuntimeInventoryPageV1, HarnessRuntimeInventoryV1,
    HarnessRuntimeManagedModeV1, HarnessRuntimeManagedSessionV1,
    HarnessRuntimeManagedStateV1, HarnessRuntimeNodeInventoryV1,
    HarnessRuntimeSessionBindingV1, HarnessRuntimeSessionStatusV1,
    HarnessRuntimeSessionV1, HarnessRuntimeTerminalSizeV1,
    HarnessRuntimeTransportV1, HarnessRuntimeWorkspaceV1,
    HarnessReadHostErrorV1, HarnessReadReplyV1, RedactedBindingStateV1,
    RedactedRunIntentV1, RedactedRunV1, RedactedTaskV1, RedactedWorktreeIntentV1,
    RunPageV1, TaskCreatorCategoryV1, TaskPageV1,
    HARNESS_OPERATOR_RESPONSE_MAX_BYTES, HARNESS_READ_REQUEST_MAX_BYTES,
    HARNESS_READ_RESPONSE_MAX_BYTES,
};
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessDispatchIntentV1, HarnessExecutionModeV1,
    HarnessFailureCategoryV1,
    HarnessIdempotencyRef,
    HarnessFailureV1, HarnessOperationId, HarnessOperationKindV1,
    HarnessOperationStateV1, HarnessOperationV1,
    HarnessOutcomeUnknownReasonV1, HarnessResultDispositionV1, HarnessRevision,
    HarnessRunLifecycleV1, HarnessSelectorV1, HarnessTaskStateV1,
    HarnessRuntimeIdentityV1, HarnessSessionBindingV1, HarnessSessionIdentityV1,
    HarnessWorktreeIntentV1,
};
use gate4agent_node_wire::{local_hmac_sha256, proofs_match};
use gate4agent_node_protocol::{
    HarnessMcpActivationDigest, HarnessMcpCallId, HarnessMcpLocalReplyV1,
    HarnessMcpRejectReasonV1, HarnessMcpReplyChunkHexV1, HarnessMcpReservationId,
    NodeFailureCode, SessionAddress,
    SessionRecordId, SpawnBundleId, SpawnContextId, SpawnProfileId,
    MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES, MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES,
    MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE,
};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, Semaphore},
    task::JoinHandle,
    time::{interval_at, timeout, Instant, MissedTickBehavior},
};

const HOST_COMMAND_CAPACITY: usize = 64;
const HOST_CONNECTION_LIMIT: usize = 32;
const HOST_DEADLINE: Duration = Duration::from_secs(3);
const HOST_NATIVE_HISTORY_RESPONSE_DEADLINE: Duration = Duration::from_secs(40);
const HOST_RUN_READ_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
const HOST_CONNECTION_DEADLINE: Duration = Duration::from_secs(45);
const OBSERVATION_RECOVERY_RETRY: Duration = Duration::from_secs(1);
const OBSERVATION_RECOVERY_MAX_IN_FLIGHT: usize = 8;
const OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX: usize = 64;
const OBSERVATION_RECOVERY_BUFFERED_BYTES_MAX: usize = 1024 * 1024;
const HARNESS_MCP_ABORT_RETRY_MAX_MS: u64 = 30_000;
const HARNESS_MCP_NETWORK_WORKERS_MAX: usize = 8;
const HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX: usize =
    HARNESS_MCP_NETWORK_WORKERS_MAX - 1;
const NATIVE_HISTORY_WORKERS_MAX: usize = 8;
const RUN_READ_WORKERS_MAX: usize = 8;
const OPERATOR_CREDENTIAL_DIGEST_DOMAIN: &[u8] = b"gate4agent-harness-operator-credential-digest-v1";
const OPERATOR_INTENT_OPERATION_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-operator-intent-operation-id-v1";
const OPERATOR_INTENT_IDEMPOTENCY_REF_DOMAIN: &[u8] =
    b"gate4agent-harness-operator-intent-idempotency-ref-v1";
const OPERATOR_INTENT_TASK_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-operator-intent-task-id-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HarnessHostEndpoint(SocketAddr);

#[derive(Clone, Debug, Default)]
pub struct HarnessRuntimeCatalogs {
    pub launch: HarnessLaunchCatalog,
    pub delivery: DeliveryCatalogV2,
}

impl HarnessRuntimeCatalogs {
    pub fn new(
        launch: HarnessLaunchCatalog,
        delivery: DeliveryCatalogV2,
    ) -> Result<Self, HarnessRuntimeError> {
        launch.validate_delivery_catalog(&delivery)
            .map_err(|_| HarnessRuntimeError::LaunchCatalog)?;
        Ok(Self { launch, delivery })
    }
}

impl HarnessHostEndpoint {
    pub fn socket_addr(self) -> SocketAddr { self.0 }
}

#[derive(Clone)]
pub struct HarnessHostHandle {
    endpoint: HarnessHostEndpoint,
    commands: mpsc::Sender<HostCommand>,
}

impl HarnessHostHandle {
    pub fn endpoint(&self) -> HarnessHostEndpoint { self.endpoint }

    pub async fn mint_credential(
        &self,
        binding: CredentialBindingV1,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    ) -> Result<HarnessReadCredential, HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::Mint {
            binding,
            issued_at_unix_ms,
            expires_at_unix_ms,
            reply,
        }).await.map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn shutdown(&self) -> Result<(), HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::Shutdown { reply }).await
            .map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn apply_harness_mutation(
        &self,
        mutation: HarnessMutationV1,
    ) -> Result<HarnessApplyOutcome, HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::ApplyHarnessMutation { mutation, reply }).await
            .map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn activate_harness_mcp(
        &self,
        reservation_id: HarnessMcpReservationId,
    ) -> Result<(), HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::ActivateHarnessMcp { reservation_id, reply })
            .await.map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }

    pub async fn revoke_harness_mcp(
        &self,
        reservation_id: HarnessMcpReservationId,
    ) -> Result<(), HarnessRuntimeError> {
        let (reply, receive) = oneshot::channel();
        self.commands.send(HostCommand::RevokeHarnessMcp { reservation_id, reply })
            .await.map_err(|_| HarnessRuntimeError::HostStopped)?;
        receive.await.map_err(|_| HarnessRuntimeError::HostStopped)?
    }
}

enum HostCommand {
    Mint {
        binding: CredentialBindingV1,
        issued_at_unix_ms: u64,
        expires_at_unix_ms: u64,
        reply: oneshot::Sender<Result<HarnessReadCredential, HarnessRuntimeError>>,
    },
    Read {
        envelope: HarnessReadEnvelopeV1,
        reply: oneshot::Sender<HarnessReadReplyV1>,
    },
    Operator {
        request: HarnessOperatorRequestV1,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
    },
    ApplyHarnessMutation {
        mutation: HarnessMutationV1,
        reply: oneshot::Sender<Result<HarnessApplyOutcome, HarnessRuntimeError>>,
    },
    ActivateHarnessMcp {
        reservation_id: HarnessMcpReservationId,
        reply: oneshot::Sender<Result<(), HarnessRuntimeError>>,
    },
    RevokeHarnessMcp {
        reservation_id: HarnessMcpReservationId,
        reply: oneshot::Sender<Result<(), HarnessRuntimeError>>,
    },
    DispatchPreflightFinished {
        intent: HarnessDispatchIntentV1,
        result: Result<SpawnProfileRevisionProof, HarnessC2Error>,
    },
    DispatchFinished {
        operation_id: HarnessOperationId,
        result: CoordinatorSpawnResult,
    },
    DeliveryStageFinished {
        operation_id: HarnessOperationId,
        result: Result<StagedDeliveryProof, HarnessC2Error>,
    },
    ContinuationExportFinished {
        operation_id: HarnessOperationId,
        result: Result<ExportContextPackOutcome, HarnessC2Error>,
    },
    HarnessMcpArmFinished {
        operation_id: HarnessOperationId,
        spec: gate4agent_node_protocol::SpawnSpec,
        profile: SpawnProfileRevisionProof,
        result: Result<ArmedHarnessMcpReservationProof, HarnessC2Error>,
    },
    HarnessMcpActivationFinished {
        reservation_id: HarnessMcpReservationId,
        attempt_id: u64,
        expected_revision: HarnessRevision,
        result: Result<ActivatedHarnessMcpReservationProof, HarnessC2Error>,
    },
    HarnessMcpAbortFinished {
        reservation_id: HarnessMcpReservationId,
        attempt_id: u64,
        result: Result<(), HarnessC2Error>,
    },
    HarnessMcpRelayFinished {
        reservation_id: HarnessMcpReservationId,
        call_id: HarnessMcpCallId,
        attempt_id: u64,
        result: Result<(), HarnessRuntimeError>,
    },
    ObservationRecoveryFinished {
        route: NodeRoute,
        attempt_id: u64,
        requested_after: u64,
        result: Result<HarnessObservationResync, HarnessC2Error>,
    },
    NativeHistoryWorkerFinished,
    RunReadFinished {
        completion: RunReadCompletion,
        reply: oneshot::Sender<HarnessOperatorReplyV1>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), HarnessRuntimeError>>,
    },
}

type ObservationRecoveryRouteKey = (NodeId, NodeIncarnationId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ObservationRecoveryAttempt {
    attempt_id: u64,
    requested_after: u64,
}

#[derive(Debug)]
struct RouteObservationRecovery {
    route: NodeRoute,
    attempt: Option<ObservationRecoveryAttempt>,
    buffered: BTreeMap<u64, RoutedNodeEvent>,
    buffered_bytes: usize,
    overflowed: bool,
    refresh_after_completion: bool,
    retry_after: Instant,
}

impl RouteObservationRecovery {
    fn new(route: NodeRoute) -> Self {
        Self {
            route,
            attempt: None,
            buffered: BTreeMap::new(),
            buffered_bytes: 0,
            overflowed: false,
            refresh_after_completion: false,
            retry_after: Instant::now(),
        }
    }

    fn accepts_completion(
        &self,
        route: &NodeRoute,
        attempt_id: u64,
        requested_after: u64,
    ) -> bool {
        self.route == *route
            && self.attempt == Some(ObservationRecoveryAttempt {
                attempt_id,
                requested_after,
            })
    }

    fn buffer(&mut self, routed: RoutedNodeEvent) {
        if self.overflowed || self.buffered.contains_key(&routed.cursor.sequence) {
            return;
        }
        let encoded_len = match serde_json::to_vec(&routed) {
            Ok(encoded) => encoded.len(),
            Err(_) => {
                self.buffered.clear();
                self.buffered_bytes = 0;
                self.overflowed = true;
                self.refresh_after_completion = true;
                return;
            }
        };
        if self.buffered.len() >= OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX
            || self.buffered_bytes.saturating_add(encoded_len)
                > OBSERVATION_RECOVERY_BUFFERED_BYTES_MAX
        {
            self.buffered.clear();
            self.buffered_bytes = 0;
            self.overflowed = true;
            self.refresh_after_completion = true;
            return;
        }
        self.buffered_bytes += encoded_len;
        self.buffered.insert(routed.cursor.sequence, routed);
    }

    fn prepare_follow_up(&mut self) {
        self.overflowed = false;
        self.refresh_after_completion = false;
        self.retry_after = Instant::now();
    }
}

#[derive(Debug, Default)]
struct ObservationRecoveryRegistry {
    routes: BTreeMap<ObservationRecoveryRouteKey, RouteObservationRecovery>,
    next_attempt_id: u64,
}

impl ObservationRecoveryRegistry {
    fn key(route: &NodeRoute) -> ObservationRecoveryRouteKey {
        (route.node_id.clone(), route.expected_incarnation_id)
    }

    fn ensure_route(&mut self, route: NodeRoute) -> &mut RouteObservationRecovery {
        self.routes.entry(Self::key(&route))
            .or_insert_with(|| RouteObservationRecovery::new(route))
    }

    fn contains(&self, route: &NodeRoute) -> bool {
        self.routes.contains_key(&Self::key(route))
    }

    fn remove(&mut self, route: &NodeRoute) {
        self.routes.remove(&Self::key(route));
    }

    fn reconcile_topology(&mut self, current: &[NodeRoute]) {
        self.routes.retain(|_, recovery| {
            current.iter().any(|route| route == &recovery.route)
        });
        for route in current {
            let recovery = self.ensure_route(route.clone());
            if recovery.attempt.is_some() {
                recovery.refresh_after_completion = true;
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.routes.values().filter(|recovery| recovery.attempt.is_some()).count()
    }

    fn allocate_attempt_id(&mut self) -> u64 {
        self.next_attempt_id = self.next_attempt_id.wrapping_add(1).max(1);
        self.next_attempt_id
    }
}

struct ActiveHarnessMcpActivation {
    attempt_id: u64,
    expected_revision: HarnessRevision,
    updated_at_unix_ms: u64,
    reply: Option<oneshot::Sender<Result<(), HarnessRuntimeError>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveHarnessMcpRelay {
    attempt_id: u64,
}

#[derive(Default)]
struct HarnessMcpWorkerRegistry {
    next_attempt_id: u64,
    activations: BTreeMap<HarnessMcpReservationId, ActiveHarnessMcpActivation>,
    relays: BTreeMap<(HarnessMcpReservationId, HarnessMcpCallId), ActiveHarnessMcpRelay>,
}

#[derive(Default)]
struct NativeHistoryWorkerRegistry {
    in_flight: usize,
}

#[derive(Default)]
struct RunReadWorkerRegistry {
    in_flight: usize,
}

impl NativeHistoryWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= NATIVE_HISTORY_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl RunReadWorkerRegistry {
    fn try_start(&mut self) -> bool {
        if self.in_flight >= RUN_READ_WORKERS_MAX { return false; }
        self.in_flight += 1;
        true
    }

    fn finish(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }
}

impl HarnessMcpWorkerRegistry {
    fn in_flight(
        &self,
        pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    ) -> usize {
        self.activations.len()
            + self.relays.len()
            + pending_aborts.values().filter(|pending| pending.attempt_id.is_some()).count()
    }

    fn has_capacity(
        &self,
        pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    ) -> bool {
        self.in_flight(pending_aborts) < HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX
    }

    fn allocate_attempt_id(&mut self) -> u64 {
        self.next_attempt_id = self.next_attempt_id.wrapping_add(1).max(1);
        self.next_attempt_id
    }

    fn accepts_activation(
        &self,
        reservation_id: &HarnessMcpReservationId,
        attempt_id: u64,
        expected_revision: HarnessRevision,
    ) -> bool {
        self.activations.get(reservation_id).is_some_and(|active| {
            active.attempt_id == attempt_id
                && active.expected_revision == expected_revision
        })
    }

    fn accepts_relay(
        &self,
        reservation_id: &HarnessMcpReservationId,
        call_id: &HarnessMcpCallId,
        attempt_id: u64,
    ) -> bool {
        self.relays.get(&(reservation_id.clone(), call_id.clone()))
            == Some(&ActiveHarnessMcpRelay { attempt_id })
    }
}

enum CoordinatorSpawnResult {
    Accepted(AcceptedSpawnBindingProof),
    Rejected(NodeFailureCode),
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinatorPreDispatchResult {
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContinuationResumeAction {
    BeginExport,
    RecoverOutcomeUnknown,
    Preflight,
    FinishOutcomeUnknown,
    FinishFailed,
    Reject,
}

fn continuation_resume_action(
    state: gate4agent_harness_protocol::HarnessContinuationStateV1,
) -> ContinuationResumeAction {
    match state {
        gate4agent_harness_protocol::HarnessContinuationStateV1::Prepared => {
            ContinuationResumeAction::BeginExport
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Exporting => {
            ContinuationResumeAction::RecoverOutcomeUnknown
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Exported => {
            ContinuationResumeAction::Preflight
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::OutcomeUnknown => {
            ContinuationResumeAction::FinishOutcomeUnknown
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Expired => {
            ContinuationResumeAction::FinishFailed
        }
        gate4agent_harness_protocol::HarnessContinuationStateV1::Bound => {
            ContinuationResumeAction::Reject
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinatorDispatchPhase {
    Delivery,
    Continuation,
    Preflight,
    HarnessMcpArm,
    Spawn,
}

enum CoordinatorPreflightStart {
    Spawn {
        route: NodeRoute,
        pending: crate::c2::PendingSpawnDispatch,
    },
    HarnessMcpArm,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ActiveDispatchJob {
    operation_id: HarnessOperationId,
    phase: CoordinatorDispatchPhase,
}

impl ActiveDispatchJob {
    fn new(operation_id: HarnessOperationId, phase: CoordinatorDispatchPhase) -> Self {
        Self { operation_id, phase }
    }

    fn is(&self, operation_id: &HarnessOperationId, phase: CoordinatorDispatchPhase) -> bool {
        &self.operation_id == operation_id && self.phase == phase
    }
}

fn next_runtime_revision(revision: HarnessRevision) -> Result<HarnessRevision, HarnessRuntimeError> {
    HarnessRevision::new(
        revision.get().checked_add(1).ok_or(HarnessRuntimeError::DispatchPreparation)?,
    ).map_err(|_| HarnessRuntimeError::DispatchPreparation)
}

fn specialized_spawn_spec(
    harness: &HarnessService,
    plan: &crate::dispatch::HarnessLaunchPlanV1,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
    mut spec: gate4agent_node_protocol::SpawnSpec,
) -> Result<gate4agent_node_protocol::SpawnSpec, HarnessRuntimeError> {
    if plan.delivery.is_some() {
        let delivery = harness.engine().delivery_for_run(run_id)
            .ok_or(HarnessRuntimeError::DispatchPreparation)?;
        let stage = delivery.stage_receipt.as_ref()
            .ok_or(HarnessRuntimeError::DispatchPreparation)?;
        spec.overrides.bundle_id = gate4agent_node_protocol::SpawnOverride::Set {
            value: SpawnBundleId::new(stage.bundle.bundle_id.as_str())
                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
        };
    }
    if plan.continuation == crate::dispatch::HarnessContinuationPolicyV1::ParentRun {
        let continuation = harness.engine().continuation_for_run(run_id)
            .ok_or(HarnessRuntimeError::DispatchPreparation)?;
        let context = continuation.context.as_ref()
            .ok_or(HarnessRuntimeError::DispatchPreparation)?;
        spec.overrides.context_id = gate4agent_node_protocol::SpawnOverride::Set {
            value: SpawnContextId::new(context.id.as_str())
                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
        };
    }
    Ok(spec)
}

fn apply_spawn_result(
    harness: &mut HarnessService,
    launch_catalog: &HarnessLaunchCatalog,
    operation_id: &HarnessOperationId,
    result: CoordinatorSpawnResult,
    now_unix_ms: u64,
) -> Result<Option<HarnessMcpReservationId>, HarnessRuntimeError> {
    let operation = harness.engine().operation(operation_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
    let run_id = operation.run_id.as_ref()
        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
    let task_id = operation.task_id.as_ref()
        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
    let run = harness.engine().run(run_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
    let task = harness.engine().task(task_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
    if run.lifecycle != HarnessRunLifecycleV1::Dispatching
        || operation.state != HarnessOperationStateV1::Dispatching
        || task.state != HarnessTaskStateV1::Running
    {
        return Err(HarnessRuntimeError::DispatchPreparation);
    }

    let mut next_run = run.clone();
    next_run.revision = next_runtime_revision(run.revision)?;
    next_run.updated_at_unix_ms = now_unix_ms;
    let mut next_operation = operation.clone();
    next_operation.revision = next_runtime_revision(operation.revision)?;
    next_operation.updated_at_unix_ms = now_unix_ms;
    match result {
        CoordinatorSpawnResult::Accepted(proof) => {
            let (instance_id, generation) = proof.runtime_identity();
            next_run.lifecycle = HarnessRunLifecycleV1::Running;
            next_run.binding = Some(HarnessSessionBindingV1 {
                node_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                    proof.node_id().as_str(),
                ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
                node_incarnation: gate4agent_harness_protocol::HarnessSelectorV1::new(
                    proof.incarnation_id().to_string(),
                ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
                workspace_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                    proof.workspace_id().as_str(),
                ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
                session: HarnessSessionIdentityV1::Managed {
                    record_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                        proof.record_id().as_str(),
                    ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
                    active_session: Some(HarnessRuntimeIdentityV1 {
                        instance_id,
                        generation,
                    }),
                },
            });
            next_operation.state = HarnessOperationStateV1::Succeeded;
            next_operation.finished_at_unix_ms = Some(now_unix_ms);
            let scheduled = harness.scheduled_launch(operation_id)
                .ok_or(HarnessRuntimeError::DispatchPreparation)?;
            let plan = launch_catalog.resolve_scheduled(scheduled)
                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
            if plan.harness_mcp == crate::dispatch::HarnessMcpPolicyV1::GrantBound {
                let has_continuation = plan.continuation
                    == crate::dispatch::HarnessContinuationPolicyV1::ParentRun;
                let continuation = has_continuation.then(|| {
                    harness.engine().continuation_for_run(run_id)
                        .cloned()
                        .ok_or(HarnessRuntimeError::DispatchPreparation)
                }).transpose()?;
                if let Some(continuation) = &continuation {
                    next_run.continuation_receipt = Some(continuation.receipt_ref.clone());
                }
                if plan.delivery.is_some() {
                    let ids = deterministic_dispatch_ids(operation_id, plan)
                        .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                    let receipt_ref = ids.delivery_receipt_ref
                        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                    next_run.delivery_receipt = Some(receipt_ref.clone());
                    let binding = next_run.binding.clone()
                        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                    let mut delivery = harness.engine().delivery_for_run(run_id)
                        .ok_or(HarnessRuntimeError::DispatchPreparation)?
                        .clone();
                    let expected_delivery_revision = delivery.revision;
                    delivery.revision = next_runtime_revision(delivery.revision)?;
                    delivery.state = gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed;
                    delivery.receipt = Some(crate::delivery::terminal_receipt(
                        &delivery,
                        receipt_ref,
                        binding,
                        now_unix_ms,
                    )?);
                    delivery.updated_at_unix_ms = now_unix_ms;
                    if let Some(continuation) = continuation {
                        harness.transition_run_with_accepted_harness_mcp_spawn_delivery_and_continuation(
                            run.revision,
                            next_run,
                            operation.revision,
                            next_operation,
                            expected_delivery_revision,
                            delivery,
                            &continuation.continuation_ref,
                            continuation.revision,
                            &proof,
                            now_unix_ms,
                        ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                    } else {
                        harness.transition_run_with_accepted_harness_mcp_spawn_and_delivery(
                            run.revision,
                            next_run,
                            operation.revision,
                            next_operation,
                            expected_delivery_revision,
                            delivery,
                            &proof,
                            now_unix_ms,
                        ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                    }
                } else if let Some(continuation) = continuation {
                    harness.transition_run_with_accepted_harness_mcp_spawn_and_continuation(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        &continuation.continuation_ref,
                        continuation.revision,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                } else {
                    harness.transition_run_with_accepted_harness_mcp_spawn(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
            } else if plan.continuation == crate::dispatch::HarnessContinuationPolicyV1::ParentRun
                && plan.harness_mcp == crate::dispatch::HarnessMcpPolicyV1::Disabled
            {
                let continuation = harness.engine().continuation_for_run(run_id)
                    .ok_or(HarnessRuntimeError::DispatchPreparation)?
                    .clone();
                let continuation_ref = continuation.continuation_ref.clone();
                let expected_continuation_revision = continuation.revision;
                next_run.continuation_receipt = Some(continuation.receipt_ref.clone());
                if plan.delivery.is_some() {
                    let ids = deterministic_dispatch_ids(operation_id, plan)
                        .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                    let receipt_ref = ids.delivery_receipt_ref
                        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                    next_run.delivery_receipt = Some(receipt_ref.clone());
                    let binding = next_run.binding.clone()
                        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                    let mut delivery = harness.engine().delivery_for_run(run_id)
                        .ok_or(HarnessRuntimeError::DispatchPreparation)?
                        .clone();
                    let expected_delivery_revision = delivery.revision;
                    delivery.revision = next_runtime_revision(delivery.revision)?;
                    delivery.state = gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed;
                    delivery.receipt = Some(crate::delivery::terminal_receipt(
                        &delivery,
                        receipt_ref,
                        binding,
                        now_unix_ms,
                    )?);
                    delivery.updated_at_unix_ms = now_unix_ms;
                    harness.transition_run_with_accepted_spawn_delivery_and_continuation(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        expected_delivery_revision,
                        delivery,
                        &continuation_ref,
                        expected_continuation_revision,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                } else {
                    harness.transition_run_with_accepted_spawn_and_continuation(
                        run.revision,
                        next_run,
                        operation.revision,
                        next_operation,
                        &continuation_ref,
                        expected_continuation_revision,
                        &proof,
                        now_unix_ms,
                    ).map(|()| None).map_err(HarnessRuntimeError::Harness)
                }
            } else if plan.delivery.is_some()
                && plan.continuation == crate::dispatch::HarnessContinuationPolicyV1::None
                && plan.harness_mcp == crate::dispatch::HarnessMcpPolicyV1::Disabled
            {
                let ids = deterministic_dispatch_ids(operation_id, plan)
                    .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                let receipt_ref = ids.delivery_receipt_ref
                    .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                next_run.delivery_receipt = Some(receipt_ref.clone());
                let binding = next_run.binding.clone()
                    .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                let mut delivery = harness.engine().delivery_for_run(run_id)
                    .ok_or(HarnessRuntimeError::DispatchPreparation)?
                    .clone();
                let expected_delivery_revision = delivery.revision;
                delivery.revision = next_runtime_revision(delivery.revision)?;
                delivery.state = gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed;
                delivery.receipt = Some(crate::delivery::terminal_receipt(
                    &delivery,
                    receipt_ref,
                    binding,
                    now_unix_ms,
                )?);
                delivery.updated_at_unix_ms = now_unix_ms;
                harness.transition_run_with_accepted_spawn_and_delivery(
                    run.revision,
                    next_run,
                    operation.revision,
                    next_operation,
                    expected_delivery_revision,
                    delivery,
                    &proof,
                ).map(|()| None).map_err(HarnessRuntimeError::Harness)
            } else {
                harness.transition_run_with_accepted_spawn(
                    run.revision,
                    next_run,
                    operation.revision,
                    next_operation,
                    &proof,
                ).map(|()| None).map_err(HarnessRuntimeError::Harness)
            }
        }
        CoordinatorSpawnResult::Rejected(_) | CoordinatorSpawnResult::Failed => {
            let failure = HarnessFailureV1 {
                category: HarnessFailureCategoryV1::Rejected,
                retryable: false,
            };
            next_run.lifecycle = HarnessRunLifecycleV1::Failed;
            next_run.result_disposition = Some(HarnessResultDispositionV1::Failed);
            next_run.failure = Some(failure.clone());
            next_operation.state = HarnessOperationStateV1::Failed;
            next_operation.failure = Some(failure);
            next_operation.finished_at_unix_ms = Some(now_unix_ms);
            let mut next_task = task.clone();
            next_task.revision = next_runtime_revision(task.revision)?;
            next_task.state = HarnessTaskStateV1::Failed;
            next_task.updated_at_unix_ms = now_unix_ms;
            harness.commit_scheduled_pre_dispatch_outcome(
                run.revision,
                next_run,
                operation.revision,
                next_operation,
                task.revision,
                next_task,
            ).map_err(HarnessRuntimeError::Harness)
        }
        CoordinatorSpawnResult::OutcomeUnknown => {
            next_run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
            next_operation.state = HarnessOperationStateV1::OutcomeUnknown;
            next_operation.outcome_unknown_reason = Some(
                HarnessOutcomeUnknownReasonV1::ReplyLost,
            );
            let mut next_task = task.clone();
            next_task.revision = next_runtime_revision(task.revision)?;
            next_task.state = HarnessTaskStateV1::Waiting;
            next_task.updated_at_unix_ms = now_unix_ms;
            harness.commit_scheduled_pre_dispatch_outcome(
                run.revision,
                next_run,
                operation.revision,
                next_operation,
                task.revision,
                next_task,
            ).map_err(HarnessRuntimeError::Harness)
        }
    }
}

fn apply_pre_dispatch_result(
    harness: &mut HarnessService,
    operation_id: &HarnessOperationId,
    result: CoordinatorPreDispatchResult,
    now_unix_ms: u64,
) -> Result<Option<HarnessMcpReservationId>, HarnessRuntimeError> {
    let operation = harness.engine().operation(operation_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?
        .clone();
    let run_id = operation.run_id.as_ref()
        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
    let run = harness.engine().run(run_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?
        .clone();
    let task = harness.engine().task(&run.task_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?
        .clone();
    if run.lifecycle != HarnessRunLifecycleV1::Requested
        || operation.state != HarnessOperationStateV1::Prepared
        || task.state != HarnessTaskStateV1::Running
    {
        return Err(HarnessRuntimeError::DispatchPreparation);
    }
    let mut next_run = run.clone();
    next_run.revision = next_runtime_revision(run.revision)?;
    next_run.updated_at_unix_ms = now_unix_ms;
    let mut next_operation = operation.clone();
    next_operation.revision = next_runtime_revision(operation.revision)?;
    next_operation.updated_at_unix_ms = now_unix_ms;
    let mut next_task = task.clone();
    next_task.revision = next_runtime_revision(task.revision)?;
    next_task.updated_at_unix_ms = now_unix_ms;
    match result {
        CoordinatorPreDispatchResult::Failed => {
            let failure = HarnessFailureV1 {
                category: HarnessFailureCategoryV1::Rejected,
                retryable: false,
            };
            next_run.lifecycle = HarnessRunLifecycleV1::Failed;
            next_run.result_disposition = Some(HarnessResultDispositionV1::Failed);
            next_run.failure = Some(failure.clone());
            next_operation.state = HarnessOperationStateV1::Failed;
            next_operation.failure = Some(failure);
            next_operation.finished_at_unix_ms = Some(now_unix_ms);
            next_task.state = HarnessTaskStateV1::Failed;
        }
        CoordinatorPreDispatchResult::OutcomeUnknown => {
            next_run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
            next_operation.state = HarnessOperationStateV1::OutcomeUnknown;
            next_operation.outcome_unknown_reason = Some(
                HarnessOutcomeUnknownReasonV1::ReplyLost,
            );
            next_task.state = HarnessTaskStateV1::Waiting;
        }
    }
    harness.commit_scheduled_pre_dispatch_outcome(
        run.revision,
        next_run,
        operation.revision,
        next_operation,
        task.revision,
        next_task,
    ).map_err(HarnessRuntimeError::Harness)
}

fn delivery_pre_dispatch_result(error: &HarnessC2Error) -> CoordinatorPreDispatchResult {
    if matches!(error, HarnessC2Error::DeliveryTransport(_)) {
        CoordinatorPreDispatchResult::OutcomeUnknown
    } else {
        CoordinatorPreDispatchResult::Failed
    }
}

fn delivery_stage_completion_result(
    error: &HarnessRuntimeError,
) -> CoordinatorPreDispatchResult {
    if matches!(
        error,
        HarnessRuntimeError::Harness(HarnessServiceError::InvalidStagedDeliveryProof(_))
            | HarnessRuntimeError::C2(HarnessC2Error::DeliveryTransport(_))
    ) {
        CoordinatorPreDispatchResult::OutcomeUnknown
    } else {
        CoordinatorPreDispatchResult::Failed
    }
}

fn preflight_pre_dispatch_result(_error: &HarnessC2Error) -> CoordinatorPreDispatchResult {
    CoordinatorPreDispatchResult::Failed
}

fn dispatching_start_error_result(error: &HarnessRuntimeError) -> CoordinatorSpawnResult {
    match error {
        HarnessRuntimeError::C2(error) if error.start_failure_category().is_some() => {
            CoordinatorSpawnResult::Failed
        }
        _ => CoordinatorSpawnResult::OutcomeUnknown,
    }
}

fn harness_mcp_arm_finish_result(error: &HarnessC2Error) -> CoordinatorSpawnResult {
    if matches!(error, HarnessC2Error::HarnessMcpRejected { .. }) {
        CoordinatorSpawnResult::Failed
    } else {
        CoordinatorSpawnResult::OutcomeUnknown
    }
}

fn delivery_needs_staging(
    state: gate4agent_harness_protocol::HarnessDeliveryStateV1,
) -> Result<bool, HarnessRuntimeError> {
    match state {
        gate4agent_harness_protocol::HarnessDeliveryStateV1::Prepared => Ok(true),
        gate4agent_harness_protocol::HarnessDeliveryStateV1::Staged => Ok(false),
        gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed => {
            Err(HarnessRuntimeError::DispatchPreparation)
        }
    }
}

fn commit_lifecycle_projection(
    harness: &mut HarnessService,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
    node_id: &NodeId,
    incarnation_id: NodeIncarnationId,
    event_sequence: u64,
    kind: HarnessLifecycleEventKindV1,
    projection: HarnessLifecycleProjectionV1,
    now_unix_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    let run = harness.engine().run(run_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
    if !matches!(run.lifecycle, HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting) {
        return Ok(());
    }
    let task = harness.engine().task(&run.task_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
    let ids = deterministic_lifecycle_authority_ids(
        &run.run_id,
        node_id,
        &incarnation_id,
        event_sequence,
        kind,
    ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
    if harness.engine().operation(&ids.operation_id).is_some() {
        return Ok(());
    }
    let committed_at = now_unix_ms.max(run.updated_at_unix_ms).max(task.updated_at_unix_ms);
    let mut next_run = run.clone();
    next_run.revision = next_runtime_revision(run.revision)?;
    next_run.updated_at_unix_ms = committed_at;
    next_run.lifecycle = match projection {
        HarnessLifecycleProjectionV1::Running => HarnessRunLifecycleV1::Running,
        HarnessLifecycleProjectionV1::Waiting => HarnessRunLifecycleV1::Waiting,
        HarnessLifecycleProjectionV1::CompletedReview => HarnessRunLifecycleV1::Completed,
        HarnessLifecycleProjectionV1::Failed => HarnessRunLifecycleV1::Failed,
        HarnessLifecycleProjectionV1::Cancelled => HarnessRunLifecycleV1::Cancelled,
    };
    match projection {
        HarnessLifecycleProjectionV1::CompletedReview => {
            next_run.result_disposition = Some(HarnessResultDispositionV1::Succeeded);
        }
        HarnessLifecycleProjectionV1::Failed => {
            next_run.result_disposition = Some(HarnessResultDispositionV1::Failed);
            next_run.failure = Some(HarnessFailureV1 {
                category: HarnessFailureCategoryV1::Internal,
                retryable: false,
            });
        }
        HarnessLifecycleProjectionV1::Cancelled => {
            next_run.result_disposition = Some(HarnessResultDispositionV1::Cancelled);
        }
        HarnessLifecycleProjectionV1::Running | HarnessLifecycleProjectionV1::Waiting => {}
    }
    let mut next_task = task.clone();
    next_task.revision = next_runtime_revision(task.revision)?;
    next_task.updated_at_unix_ms = committed_at;
    next_task.state = match projection {
        HarnessLifecycleProjectionV1::Running => HarnessTaskStateV1::Running,
        HarnessLifecycleProjectionV1::Waiting => HarnessTaskStateV1::Waiting,
        HarnessLifecycleProjectionV1::CompletedReview => HarnessTaskStateV1::Review,
        HarnessLifecycleProjectionV1::Failed => HarnessTaskStateV1::Failed,
        HarnessLifecycleProjectionV1::Cancelled => HarnessTaskStateV1::Cancelled,
    };
    let operation = HarnessOperationV1 {
        operation_id: ids.operation_id,
        revision: HarnessRevision::new(1)
            .map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
        actor: HarnessActorV1::ParentRun { run_id: run.run_id.clone() },
        kind: HarnessOperationKindV1::MutateRun,
        state: HarnessOperationStateV1::Succeeded,
        task_id: None,
        run_id: Some(run.run_id.clone()),
        grant_id: None,
        reconciles_operation_id: None,
        expected_revision: Some(run.revision),
        request_digest: ids.request_digest,
        idempotency_ref: ids.idempotency_ref,
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: committed_at,
        updated_at_unix_ms: committed_at,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: Some(committed_at),
    };
    harness.commit_run_event(
        operation,
        run.revision,
        next_run,
        task.revision,
        next_task,
    ).map_err(HarnessRuntimeError::Harness)
}

fn apply_exact_control_lifecycle(
    harness: &mut HarnessService,
    routed: &RoutedNodeEvent,
    now_unix_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    let matches = harness.engine().runs().filter_map(|run| {
        exact_bound_control_lifecycle(run, routed).map(|(sequence, kind, projection)| {
            (run.run_id.clone(), sequence, kind, projection)
        })
    }).collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(HarnessRuntimeError::DispatchPreparation);
    }
    let Some((run_id, sequence, kind, projection)) = matches.into_iter().next() else {
        return Ok(());
    };
    commit_lifecycle_projection(
        harness,
        &run_id,
        &routed.node_id,
        routed.cursor.incarnation_id,
        sequence,
        kind,
        projection,
        now_unix_ms,
    )
}

fn freeze_bound_route_waiting(
    harness: &mut HarnessService,
    route: &NodeRoute,
    event_sequence: u64,
    now_unix_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    let runs = harness.engine().runs().filter(|run| {
        run.lifecycle == HarnessRunLifecycleV1::Running
            && run.binding.as_ref().is_some_and(|binding| {
                binding.node_id.as_str() == route.node_id.as_str()
                    && binding.node_incarnation.as_str()
                        == route.expected_incarnation_id.to_string()
            })
    }).map(|run| run.run_id.clone()).collect::<Vec<_>>();
    for run_id in runs {
        commit_lifecycle_projection(
            harness,
            &run_id,
            &route.node_id,
            route.expected_incarnation_id,
            event_sequence,
            HarnessLifecycleEventKindV1::GapWaiting,
            HarnessLifecycleProjectionV1::Waiting,
            now_unix_ms,
        )?;
    }
    Ok(())
}

fn start_dispatch_preflight(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    intent: HarnessDispatchIntentV1,
) -> Result<(), HarnessRuntimeError> {
    let node_id = gate4agent_node_protocol::NodeId::new(intent.intent.node_id.as_str())
        .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
    let route = adapter.exact_route(&node_id)?;
    let profile_id = SpawnProfileId::new(intent.intent.provider_profile.as_str())
        .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
    let adapter = adapter.clone();
    let commands = commands.clone();
    tokio::spawn(async move {
        let result = adapter.preflight_spawn_profile(&route, &profile_id).await;
        let _ = commands.send(HostCommand::DispatchPreflightFinished { intent, result }).await;
    });
    Ok(())
}

fn start_native_history_worker(
    pending: PendingNativeHistoryRequest,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
) {
    tokio::spawn(async move {
        let reply_value = match pending.finish().await {
            Ok(response) => HarnessOperatorReplyV1::Ok { response },
            Err(error) => HarnessOperatorReplyV1::Error {
                error: map_native_history_error(error),
            },
        };
        let _ = reply.send(reply_value);
        let _ = commands.send(HostCommand::NativeHistoryWorkerFinished).await;
    });
}

fn start_run_read_worker(
    pending: PendingRunRead,
    commands: mpsc::Sender<HostCommand>,
    reply: oneshot::Sender<HarnessOperatorReplyV1>,
) {
    tokio::spawn(async move {
        let completion = pending.finish().await;
        let _ = commands.send(HostCommand::RunReadFinished { completion, reply }).await;
    });
}

fn map_run_read_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidRunReadRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessC2Error::RunReadUnbound => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::RunReadEnqueue(gate4agent_c2_client::C2ControlError::QueueFull) => {
            HarnessOperatorHostErrorV1::Busy
        }
        HarnessC2Error::RunReadEnqueue(_)
        | HarnessC2Error::RunReadTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_) => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::RunReadRouteMismatch => HarnessOperatorHostErrorV1::Conflict,
        HarnessC2Error::RunReadDeadline => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::RunReadTooLarge => HarnessOperatorHostErrorV1::TooLarge,
        HarnessC2Error::RunReadRejected { code } => match code {
            NodeFailureCode::InvalidRequest
            | NodeFailureCode::InvalidRepositoryPath => {
                HarnessOperatorHostErrorV1::InvalidRequest
            }
            NodeFailureCode::UnknownWorkspace
            | NodeFailureCode::RepositoryFileNotFound
            | NodeFailureCode::RepositoryParentNotFound => {
                HarnessOperatorHostErrorV1::NotFound
            }
            NodeFailureCode::BindingMismatch
            | NodeFailureCode::RepositoryFileRevisionConflict
            | NodeFailureCode::RepositoryFileNotRegular
            | NodeFailureCode::RepositoryPathUnsafe
            | NodeFailureCode::NotGitRepository
            | NodeFailureCode::StaleGeneration => HarnessOperatorHostErrorV1::Conflict,
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            NodeFailureCode::RepositoryFileReadTimedOut
            | NodeFailureCode::GitReadTimedOut
            | NodeFailureCode::HostDirectoryReadTimedOut
            | NodeFailureCode::SpawnDeadlineExceeded => {
                HarnessOperatorHostErrorV1::Deadline
            }
            NodeFailureCode::ResponseTooLarge => HarnessOperatorHostErrorV1::TooLarge,
            NodeFailureCode::UnsupportedCapability
            | NodeFailureCode::RepositoryFileReadFailed
            | NodeFailureCode::GitReadFailed
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::BackendOperationFailed
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        HarnessC2Error::InvalidRunReadBinding
        | HarnessC2Error::RunReadCorrelationMismatch
        | HarnessC2Error::RunReadProjection => HarnessOperatorHostErrorV1::Internal,
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

fn map_native_history_error(error: HarnessC2Error) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessC2Error::InvalidNativeHistoryRequest => {
            HarnessOperatorHostErrorV1::InvalidRequest
        }
        HarnessC2Error::NativeHistoryEnqueue(
            gate4agent_c2_client::C2ControlError::QueueFull,
        ) => HarnessOperatorHostErrorV1::Busy,
        HarnessC2Error::NativeHistoryEnqueue(_)
        | HarnessC2Error::NativeHistoryTransport(_)
        | HarnessC2Error::UnknownNode(_)
        | HarnessC2Error::NodeOffline(_)
        | HarnessC2Error::MissingIncarnation(_) => HarnessOperatorHostErrorV1::Unavailable,
        HarnessC2Error::IncarnationChanged { .. }
        | HarnessC2Error::NativeHistoryRouteMismatch => {
            HarnessOperatorHostErrorV1::Conflict
        }
        HarnessC2Error::NativeHistoryDeadline => HarnessOperatorHostErrorV1::Deadline,
        HarnessC2Error::NativeHistoryRejected { code } => match code {
            NodeFailureCode::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
            NodeFailureCode::StaleNativeSessionCatalog => {
                HarnessOperatorHostErrorV1::Conflict
            }
            NodeFailureCode::ControllerBusy
            | NodeFailureCode::WorkspaceBusy
            | NodeFailureCode::BackendBusy => HarnessOperatorHostErrorV1::Busy,
            NodeFailureCode::UnsupportedCapability
            | NodeFailureCode::UnknownWorkspace
            | NodeFailureCode::BackendDisconnected
            | NodeFailureCode::ShuttingDown => HarnessOperatorHostErrorV1::Unavailable,
            _ => HarnessOperatorHostErrorV1::Internal,
        },
        HarnessC2Error::NativeHistoryCorrelationMismatch => {
            HarnessOperatorHostErrorV1::Internal
        }
        _ => HarnessOperatorHostErrorV1::Internal,
    }
}

fn is_native_history_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::CatalogNativeSessions { .. }
            | HarnessOperatorRequestV1::PageNativeSessions { .. }
            | HarnessOperatorRequestV1::PreviewNativeSession { .. }
    )
}

fn is_run_read_request(request: &HarnessOperatorRequestV1) -> bool {
    matches!(
        request,
        HarnessOperatorRequestV1::InspectRunWorkspace { .. }
            | HarnessOperatorRequestV1::ReadRunWorkspaceFile { .. }
            | HarnessOperatorRequestV1::ReadRunGitHistory { .. }
            | HarnessOperatorRequestV1::ReadRunGitDiff { .. }
    )
}

fn operator_response_deadline(request: &HarnessOperatorRequestV1) -> Duration {
    if is_native_history_request(request) {
        HOST_NATIVE_HISTORY_RESPONSE_DEADLINE
    } else if is_run_read_request(request) {
        HOST_RUN_READ_RESPONSE_DEADLINE
    } else {
        HOST_DEADLINE
    }
}

fn run_read_run_id(request: &HarnessOperatorRequestV1) -> Option<&gate4agent_harness_protocol::HarnessRunId> {
    match request {
        HarnessOperatorRequestV1::InspectRunWorkspace { run_id }
        | HarnessOperatorRequestV1::ReadRunWorkspaceFile { run_id, .. }
        | HarnessOperatorRequestV1::ReadRunGitHistory { run_id, .. }
        | HarnessOperatorRequestV1::ReadRunGitDiff { run_id, .. } => Some(run_id),
        _ => None,
    }
}

fn validate_run_read_completion_origin(
    current: Option<&gate4agent_harness_protocol::HarnessRunV1>,
    prepared: &PreparedRunRead,
) -> Result<(), HarnessOperatorHostErrorV1> {
    let current = current.ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    if current.binding.as_ref() != Some(prepared.binding()) {
        return Err(HarnessOperatorHostErrorV1::Conflict);
    }
    Ok(())
}

fn start_delivery_stage_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    lease: crate::c2::PreparedDeliveryStageLease,
) {
    tokio::spawn(async move {
        let result = adapter.stage_compiled_delivery(lease).await;
        let _ = commands.send(HostCommand::DeliveryStageFinished {
            operation_id,
            result,
        }).await;
    });
}

fn start_continuation_export_finish(
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    start: ContextPackExportStart,
) {
    tokio::spawn(async move {
        let result = match start {
            ContextPackExportStart::Enqueued(pending) => pending.finish().await,
            ContextPackExportStart::NotEnqueued(outcome) => Ok(outcome),
        };
        let _ = commands.send(HostCommand::ContinuationExportFinished {
            operation_id,
            result,
        }).await;
    });
}

fn start_harness_mcp_arm_finish(
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    spec: gate4agent_node_protocol::SpawnSpec,
    profile: SpawnProfileRevisionProof,
    pending: crate::c2::PendingHarnessMcpArm,
) {
    tokio::spawn(async move {
        let result = pending.finish().await;
        let _ = commands.send(HostCommand::HarnessMcpArmFinished {
            operation_id,
            spec,
            profile,
            result,
        }).await;
    });
}

fn start_harness_mcp_activation_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
    record_id: SessionRecordId,
    session: SessionAddress,
    attempt_id: u64,
    expected_revision: HarnessRevision,
) {
    let reservation_id = reservation.reservation_id.clone();
    tokio::spawn(async move {
        let result = adapter.activate_harness_mcp_reservation(
            &route,
            reservation,
            record_id,
            session,
        ).await;
        let _ = commands.send(HostCommand::HarnessMcpActivationFinished {
            reservation_id,
            attempt_id,
            expected_revision,
            result,
        }).await;
    });
}

fn start_harness_mcp_abort_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    cleanup: PendingHarnessMcpAbort,
    attempt_id: u64,
) {
    tokio::spawn(async move {
        let result = adapter.abort_harness_mcp_reservation(
            &cleanup.route,
            &cleanup.reservation_id,
            &cleanup.activation_digest,
        ).await;
        let _ = commands.send(HostCommand::HarnessMcpAbortFinished {
            reservation_id: cleanup.reservation_id,
            attempt_id,
            result,
        }).await;
    });
}

fn start_harness_mcp_relay_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    plan: HarnessMcpRelayPlan,
    attempt_id: u64,
) {
    let reservation_id = plan.reservation_id.clone();
    let call_id = plan.call_id.clone();
    tokio::spawn(async move {
        let result = relay_harness_mcp_read_call(&adapter, plan).await;
        let _ = commands.send(HostCommand::HarnessMcpRelayFinished {
            reservation_id,
            call_id,
            attempt_id,
            result,
        }).await;
    });
}

fn start_harness_mcp_reject_worker(
    adapter: HarnessC2Adapter,
    mut rejects: mpsc::Receiver<HarnessMcpRelayPlan>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(plan) = rejects.recv().await {
            let _ = relay_harness_mcp_read_call(&adapter, plan).await;
        }
    })
}

fn start_or_resume_dispatch_job(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    catalogs: &HarnessRuntimeCatalogs,
    intent: HarnessDispatchIntentV1,
) -> Result<Option<ActiveDispatchJob>, HarnessRuntimeError> {
    let scheduled = harness.scheduled_launch(&intent.operation_id)
        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
    let plan = catalogs.launch.resolve_scheduled(scheduled)
        .map_err(|_| HarnessRuntimeError::DispatchPreparation)?
        .clone();
    if plan.is_ordinary_dispatch() {
        start_dispatch_preflight(adapter, commands, intent.clone())?;
        return Ok(Some(ActiveDispatchJob::new(
            intent.operation_id,
            CoordinatorDispatchPhase::Preflight,
        )));
    }
    harness.prepare_scheduled_specialized_authorities(
        &catalogs.launch,
        &catalogs.delivery,
        &intent.operation_id,
        unix_time_ms(),
    )?;
    if let Some(policy) = &plan.delivery {
        let delivery = harness.engine().delivery_for_run(&intent.run_id)
            .ok_or(HarnessRuntimeError::DispatchPreparation)?;
        if delivery_needs_staging(delivery.state)? {
            let delivery_ref = delivery.delivery_ref.clone();
            let compiled = catalogs.delivery.get(&policy.bundle_id)
                .ok_or(HarnessRuntimeError::DispatchPreparation)?
                .clone();
            let node_id = NodeId::new(intent.intent.node_id.as_str())
                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
            let route = adapter.exact_route(&node_id)?;
            let operation_id = intent.operation_id.clone();
            let lease = harness.issue_delivery_staging_lease(
                &delivery_ref,
                route,
                compiled,
            )?;
            start_delivery_stage_finish(
                adapter.clone(),
                commands.clone(),
                operation_id.clone(),
                lease,
            );
            return Ok(Some(ActiveDispatchJob::new(
                operation_id,
                CoordinatorDispatchPhase::Delivery,
            )));
        }
    }
    if plan.continuation == crate::dispatch::HarnessContinuationPolicyV1::ParentRun {
        let continuation = harness.engine().continuation_for_run(&intent.run_id)
            .ok_or(HarnessRuntimeError::DispatchPreparation)?
            .clone();
        match continuation_resume_action(continuation.state) {
            ContinuationResumeAction::BeginExport => {
                let prepared = harness.begin_continuation_export(
                    &continuation.continuation_ref,
                    continuation.revision,
                    unix_time_ms(),
                )?;
                let start = match adapter.start_context_pack_export(prepared) {
                    Ok(start) => start,
                    Err(_) => {
                        let exporting = harness.engine().continuation_for_run(&intent.run_id)
                            .ok_or(HarnessRuntimeError::DispatchPreparation)?
                            .clone();
                        harness.recover_exporting_continuation_outcome_unknown(
                            &exporting.continuation_ref,
                            exporting.revision,
                            unix_time_ms(),
                        )?;
                        apply_pre_dispatch_result(
                            harness,
                            &intent.operation_id,
                            CoordinatorPreDispatchResult::OutcomeUnknown,
                            unix_time_ms(),
                        )?;
                        return Ok(None);
                    }
                };
                let operation_id = intent.operation_id.clone();
                start_continuation_export_finish(
                    commands.clone(),
                    operation_id.clone(),
                    start,
                );
                return Ok(Some(ActiveDispatchJob::new(
                    operation_id,
                    CoordinatorDispatchPhase::Continuation,
                )));
            }
            ContinuationResumeAction::RecoverOutcomeUnknown => {
                harness.recover_exporting_continuation_outcome_unknown(
                    &continuation.continuation_ref,
                    continuation.revision,
                    unix_time_ms(),
                )?;
                apply_pre_dispatch_result(
                    harness,
                    &intent.operation_id,
                    CoordinatorPreDispatchResult::OutcomeUnknown,
                    unix_time_ms(),
                )?;
                return Ok(None);
            }
            ContinuationResumeAction::FinishOutcomeUnknown => {
                apply_pre_dispatch_result(
                    harness,
                    &intent.operation_id,
                    CoordinatorPreDispatchResult::OutcomeUnknown,
                    unix_time_ms(),
                )?;
                return Ok(None);
            }
            ContinuationResumeAction::FinishFailed => {
                apply_pre_dispatch_result(
                    harness,
                    &intent.operation_id,
                    CoordinatorPreDispatchResult::Failed,
                    unix_time_ms(),
                )?;
                return Ok(None);
            }
            ContinuationResumeAction::Preflight => {}
            ContinuationResumeAction::Reject => {
                return Err(HarnessRuntimeError::DispatchPreparation);
            }
        }
    }
    start_dispatch_preflight(adapter, commands, intent.clone())?;
    Ok(Some(ActiveDispatchJob::new(
        intent.operation_id,
        CoordinatorDispatchPhase::Preflight,
    )))
}

fn dispatch_start_pre_dispatch_result(
    error: &HarnessRuntimeError,
) -> CoordinatorPreDispatchResult {
    match error {
        HarnessRuntimeError::C2(
            HarnessC2Error::DeliveryTransport(_)
            | HarnessC2Error::ContextExportTransport(_),
        ) => CoordinatorPreDispatchResult::OutcomeUnknown,
        _ => CoordinatorPreDispatchResult::Failed,
    }
}

fn start_or_terminalize_dispatch_job(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    catalogs: &HarnessRuntimeCatalogs,
    pending_harness_mcp_aborts: &mut BTreeMap<
        HarnessMcpReservationId,
        PendingHarnessMcpAbort,
    >,
    intent: HarnessDispatchIntentV1,
) -> Result<Option<ActiveDispatchJob>, HarnessRuntimeError> {
    let operation_id = intent.operation_id.clone();
    match start_or_resume_dispatch_job(harness, adapter, commands, catalogs, intent) {
        Ok(job) => Ok(job),
        Err(error) => {
            let operation_state = harness.engine().operation(&operation_id)
                .map(|operation| operation.state);
            let reservation_id = match operation_state {
                Some(HarnessOperationStateV1::Prepared) => apply_pre_dispatch_result(
                    harness,
                    &operation_id,
                    dispatch_start_pre_dispatch_result(&error),
                    unix_time_ms(),
                )?,
                Some(HarnessOperationStateV1::Dispatching) => apply_spawn_result(
                    harness,
                    &catalogs.launch,
                    &operation_id,
                    match dispatch_start_pre_dispatch_result(&error) {
                        CoordinatorPreDispatchResult::Failed => CoordinatorSpawnResult::Failed,
                        CoordinatorPreDispatchResult::OutcomeUnknown => {
                            CoordinatorSpawnResult::OutcomeUnknown
                        }
                    },
                    unix_time_ms(),
                )?,
                Some(
                    HarnessOperationStateV1::Succeeded
                    | HarnessOperationStateV1::Failed
                    | HarnessOperationStateV1::OutcomeUnknown
                    | HarnessOperationStateV1::Reconciled,
                ) => None,
                None => return Err(HarnessRuntimeError::DispatchPreparation),
            };
            if let Some(reservation_id) = reservation_id {
                if let Some(cleanup) = harness
                    .harness_mcp_reservation(&reservation_id)
                    .and_then(pending_harness_mcp_abort)
                {
                    pending_harness_mcp_aborts
                        .entry(reservation_id)
                        .or_insert(cleanup);
                }
            }
            Ok(None)
        }
    }
}

fn start_dispatch_finish(
    adapter: HarnessC2Adapter,
    commands: mpsc::Sender<HostCommand>,
    operation_id: HarnessOperationId,
    route: NodeRoute,
    pending: crate::c2::PendingSpawnDispatch,
) {
    tokio::spawn(async move {
        let result = match pending.finish().await {
            Ok(SpawnDispatchOutcome::Accepted(accepted)) => {
                match adapter.resolve_accepted_receipt(&route, &accepted).await {
                    Ok(proof) => CoordinatorSpawnResult::Accepted(proof),
                    Err(_) => CoordinatorSpawnResult::OutcomeUnknown,
                }
            }
            Ok(SpawnDispatchOutcome::Rejected { code }) => {
                CoordinatorSpawnResult::Rejected(code)
            }
            Ok(SpawnDispatchOutcome::OutcomeUnknown { .. }) | Err(_) => {
                CoordinatorSpawnResult::OutcomeUnknown
            }
        };
        let _ = commands.send(HostCommand::DispatchFinished { operation_id, result }).await;
    });
}

pub async fn start_harness_host(
    harness: HarnessService,
    observation: ObservationService,
    adapter: HarnessC2Adapter,
    events: HarnessC2EventReceiver,
    bind: SocketAddr,
) -> Result<(HarnessHostHandle, JoinHandle<Result<(), HarnessRuntimeError>>), HarnessRuntimeError> {
    start_harness_host_with_operator(
        harness,
        observation,
        adapter,
        events,
        bind,
        None,
    ).await
}

pub async fn start_harness_host_with_operator(
    harness: HarnessService,
    observation: ObservationService,
    adapter: HarnessC2Adapter,
    events: HarnessC2EventReceiver,
    bind: SocketAddr,
    operator_credential: Option<HarnessOperatorCredential>,
) -> Result<(HarnessHostHandle, JoinHandle<Result<(), HarnessRuntimeError>>), HarnessRuntimeError> {
    start_harness_host_with_operator_and_catalogs(
        harness,
        observation,
        adapter,
        events,
        bind,
        operator_credential,
        HarnessRuntimeCatalogs::default(),
    ).await
}

pub async fn start_harness_host_with_operator_and_catalogs(
    mut harness: HarnessService,
    mut observation: ObservationService,
    adapter: HarnessC2Adapter,
    mut events: HarnessC2EventReceiver,
    bind: SocketAddr,
    operator_credential: Option<HarnessOperatorCredential>,
    catalogs: HarnessRuntimeCatalogs,
) -> Result<(HarnessHostHandle, JoinHandle<Result<(), HarnessRuntimeError>>), HarnessRuntimeError> {
    catalogs.launch.validate_delivery_catalog(&catalogs.delivery)
        .map_err(|_| HarnessRuntimeError::LaunchCatalog)?;
    if bind.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(HarnessRuntimeError::NonLoopbackBind);
    }
    let listener = TcpListener::bind(bind).await.map_err(|_| HarnessRuntimeError::BindFailed)?;
    let endpoint = HarnessHostEndpoint(
        listener.local_addr().map_err(|_| HarnessRuntimeError::BindFailed)?,
    );
    let mut support = ObservationSupportRegistry::default();
    let mut runtime_inventory = HarnessRuntimeInventoryCache::default();
    let mut topology = adapter.topology_receiver();
    recover_all_routes(
        &adapter,
        &mut harness,
        &mut observation,
        &mut support,
        &mut runtime_inventory,
    ).await?;
    let mut pending_harness_mcp_aborts = durable_harness_mcp_abort_cleanup(&harness);
    let harness_mcp_actions = prepare_harness_mcp_reconcile(
        &mut harness,
        &adapter,
        &observation,
        &support,
    )?;
    execute_harness_mcp_reconcile(
        &mut harness,
        &adapter,
        harness_mcp_actions,
        &mut pending_harness_mcp_aborts,
    ).await?;
    retry_pending_harness_mcp_aborts(
        &adapter,
        &mut pending_harness_mcp_aborts,
    ).await;
    let authority = CredentialAuthority::new()?;
    let operator_authority = operator_credential
        .map(HarnessOperatorCredentialAuthority::new)
        .transpose()?;
    let (commands, mut command_rx) = mpsc::channel(HOST_COMMAND_CAPACITY);
    let handle = HarnessHostHandle { endpoint, commands: commands.clone() };
    let connections = Arc::new(Semaphore::new(HOST_CONNECTION_LIMIT));
    let task = tokio::spawn(async move {
        let mut active_dispatch = None;
        let mut harness_mcp_workers = HarnessMcpWorkerRegistry::default();
        let mut native_history_workers = NativeHistoryWorkerRegistry::default();
        let mut run_read_workers = RunReadWorkerRegistry::default();
        let (harness_mcp_rejects, harness_mcp_reject_rx) = mpsc::channel(
            MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE,
        );
        let _harness_mcp_reject_worker = start_harness_mcp_reject_worker(
            adapter.clone(),
            harness_mcp_reject_rx,
        );
        let stranded_dispatches = harness.engine().operations()
            .filter(|operation| {
                operation.state == HarnessOperationStateV1::Dispatching
                    && harness.scheduled_launch(&operation.operation_id).is_some()
            })
            .map(|operation| operation.operation_id.clone())
            .collect::<Vec<_>>();
        for operation_id in stranded_dispatches {
            if let Some(reservation_id) = apply_spawn_result(
                &mut harness,
                &catalogs.launch,
                &operation_id,
                CoordinatorSpawnResult::OutcomeUnknown,
                unix_time_ms(),
            )? {
                if let Some(cleanup) = harness
                    .harness_mcp_reservation(&reservation_id)
                    .and_then(pending_harness_mcp_abort)
                {
                    pending_harness_mcp_aborts
                        .entry(reservation_id)
                        .or_insert(cleanup);
                }
            }
        }
        if let Some(intent) = harness.pending_scheduled_dispatch()? {
            active_dispatch = start_or_terminalize_dispatch_job(
                &mut harness,
                &adapter,
                &commands,
                &catalogs,
                &mut pending_harness_mcp_aborts,
                intent,
            )?;
        }
        let mut events_open = true;
        let mut topology_open = true;
        let mut recovery_retry = interval_at(
            Instant::now() + OBSERVATION_RECOVERY_RETRY,
            OBSERVATION_RECOVERY_RETRY,
        );
        recovery_retry.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut observation_recovery = ObservationRecoveryRegistry::default();
        loop {
            tokio::select! {
                command = command_rx.recv() => {
                    match command {
                        Some(HostCommand::Mint {
                            binding,
                            issued_at_unix_ms,
                            expires_at_unix_ms,
                            reply,
                        }) => {
                            let result = ensure_current_topology_binding(&adapter, &binding)
                                .and_then(|_| verify_observation_credential_binding(
                                    &observation,
                                    &support,
                                    &binding,
                                ).map_err(|_| HarnessRuntimeError::CredentialBinding))
                                .and_then(|_| authority.mint(
                                    harness.engine(),
                                    binding,
                                    issued_at_unix_ms,
                                    expires_at_unix_ms,
                                ).map_err(HarnessRuntimeError::Credential));
                            let _ = reply.send(result);
                        }
                        Some(HostCommand::Read { envelope, reply }) => {
                            let response = authority.verify(
                                harness.engine(),
                                &envelope.credential,
                                unix_time_ms(),
                            ).map_err(|_| HarnessReadHostErrorV1::Unauthorized)
                                .and_then(|claims| {
                                    ensure_current_topology_binding(
                                        &adapter,
                                        &claims.binding,
                                    ).map_err(|_| HarnessReadHostErrorV1::Unauthorized)?;
                                    verify_observation_credential_binding(
                                        &observation,
                                        &support,
                                        &claims.binding,
                                    )?;
                                    let response = execute_read(
                                        &harness,
                                        &observation,
                                        &support,
                                        &claims,
                                        envelope.request,
                                    )?;
                                    response.validate()
                                        .map_err(|_| HarnessReadHostErrorV1::Internal)?;
                                    Ok(response)
                                });
                            let reply_value = match response {
                                Ok(response) => HarnessReadReplyV1::Ok { response },
                                Err(error) => HarnessReadReplyV1::Error { error },
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::Operator { request, reply }) => {
                            if is_run_read_request(&request) {
                                let prepared = run_read_run_id(&request)
                                    .ok_or(HarnessOperatorHostErrorV1::InvalidRequest)
                                    .and_then(|run_id| {
                                        harness.engine().run(run_id).cloned()
                                            .ok_or(HarnessOperatorHostErrorV1::NotFound)
                                    })
                                    .and_then(|run| {
                                        PreparedRunRead::from_operator_request(&run, request)
                                            .map_err(map_run_read_error)
                                    });
                                let prepared = match prepared {
                                    Ok(prepared) => prepared,
                                    Err(error) => {
                                        let _ = reply.send(HarnessOperatorReplyV1::Error { error });
                                        continue;
                                    }
                                };
                                if !run_read_workers.try_start() {
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_prepared_run_read(prepared) {
                                    Ok(pending) => start_run_read_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                    ),
                                    Err(error) => {
                                        run_read_workers.finish();
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_run_read_error(error),
                                        });
                                    }
                                }
                                continue;
                            }
                            if is_native_history_request(&request) {
                                if !native_history_workers.try_start() {
                                    let _ = reply.send(HarnessOperatorReplyV1::Error {
                                        error: HarnessOperatorHostErrorV1::Busy,
                                    });
                                    continue;
                                }
                                match adapter.start_native_history_request(request) {
                                    Ok(pending) => start_native_history_worker(
                                        pending,
                                        commands.clone(),
                                        reply,
                                    ),
                                    Err(error) => {
                                        native_history_workers.finish();
                                        let _ = reply.send(HarnessOperatorReplyV1::Error {
                                            error: map_native_history_error(error),
                                        });
                                    }
                                }
                                continue;
                            }
                            let response = execute_operator_request(
                                &mut harness,
                                &observation,
                                &support,
                                &catalogs.launch,
                                &runtime_inventory,
                                request,
                            );
                            let scheduled_dispatch = response.as_ref().ok()
                                .and_then(scheduled_dispatch_from_operator_response);
                            let reply_value = match response {
                                Ok(response) => HarnessOperatorReplyV1::Ok { response },
                                Err(error) => HarnessOperatorReplyV1::Error { error },
                            };
                            let _ = reply.send(reply_value);
                            if active_dispatch.is_none() {
                                if let Some(intent) = scheduled_dispatch {
                                    active_dispatch = start_or_terminalize_dispatch_job(
                                        &mut harness,
                                        &adapter,
                                        &commands,
                                        &catalogs,
                                        &mut pending_harness_mcp_aborts,
                                        intent,
                                    )?;
                                }
                            }
                        }
                        Some(HostCommand::ApplyHarnessMutation { mutation, reply }) => {
                            let prior = non_revoked_harness_mcp_abort_cleanup(&harness);
                            let result = harness.apply(mutation).map_err(HarnessRuntimeError::Harness);
                            if result.is_ok() {
                                enqueue_newly_revoked_harness_mcp_aborts(
                                    &harness,
                                    prior,
                                    &mut pending_harness_mcp_aborts,
                                );
                                start_pending_harness_mcp_abort_workers(
                                    &adapter,
                                    &commands,
                                    &mut harness_mcp_workers,
                                    &mut pending_harness_mcp_aborts,
                                );
                            }
                            let _ = reply.send(result);
                        }
                        Some(HostCommand::ActivateHarnessMcp { reservation_id, reply }) => {
                            let authority = harness.validate_activatable_harness_mcp_authority(
                                &reservation_id,
                                unix_time_ms(),
                            ).map_err(|_| HarnessRuntimeError::HarnessMcpAuthority);
                            match authority {
                                Ok((reservation, record_id, session)) => {
                                    let route = reservation_route(&reservation)?;
                                    if let Err(reply) = schedule_harness_mcp_activation(
                                        &adapter,
                                        &commands,
                                        &mut harness_mcp_workers,
                                        &pending_harness_mcp_aborts,
                                        route,
                                        reservation,
                                        record_id,
                                        session,
                                        unix_time_ms(),
                                        Some(reply),
                                    ) {
                                        if let Some(reply) = reply {
                                            let _ = reply.send(Err(
                                                HarnessRuntimeError::HarnessMcpAuthority,
                                            ));
                                        }
                                    }
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        Some(HostCommand::RevokeHarnessMcp { reservation_id, reply }) => {
                            let cleanup = harness.harness_mcp_reservation(&reservation_id)
                                .and_then(pending_harness_mcp_abort)
                                .ok_or(HarnessRuntimeError::HarnessMcpAuthority);
                            let result = match cleanup {
                                Ok(cleanup) => harness.revoke_harness_mcp_reservation(
                                    &reservation_id,
                                    unix_time_ms(),
                                ).map_err(HarnessRuntimeError::Harness)
                                    .map(|()| {
                                        pending_harness_mcp_aborts
                                            .entry(reservation_id.clone())
                                            .or_insert(cleanup);
                                    }),
                                Err(error) => Err(error),
                            };
                            if result.is_ok() {
                                start_pending_harness_mcp_abort_workers(
                                    &adapter,
                                    &commands,
                                    &mut harness_mcp_workers,
                                    &mut pending_harness_mcp_aborts,
                                );
                            }
                            let _ = reply.send(result);
                        }
                        Some(HostCommand::DispatchPreflightFinished { intent, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(
                                    &intent.operation_id,
                                    CoordinatorDispatchPhase::Preflight,
                                )
                            }) {
                                continue;
                            }
                            let profile = match result {
                                Ok(profile) => profile,
                                Err(error) => {
                                    active_dispatch = None;
                                    if let Some(reservation_id) = apply_pre_dispatch_result(
                                        &mut harness,
                                        &intent.operation_id,
                                        preflight_pre_dispatch_result(&error),
                                        unix_time_ms(),
                                    )? {
                                        if let Some(cleanup) = harness
                                            .harness_mcp_reservation(&reservation_id)
                                            .and_then(pending_harness_mcp_abort)
                                        {
                                            pending_harness_mcp_aborts
                                                .entry(reservation_id)
                                                .or_insert(cleanup);
                                        }
                                    }
                                    continue;
                                }
                            };
                            let preparation = (|| -> Result<_, HarnessRuntimeError> {
                            let run = harness.engine().run(&intent.run_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
                            let operation = harness.engine().operation(&intent.operation_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
                            let task = harness.engine().task(&intent.task_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
                            let scheduled = harness.scheduled_launch(&intent.operation_id)
                                .ok_or(HarnessRuntimeError::DispatchPreparation)?.clone();
                            let plan = catalogs.launch.resolve_scheduled(&scheduled)
                                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                            let spec = plan.spawn_spec(
                                &intent,
                                &task,
                                profile.revision().clone(),
                            )
                                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                            let spec = specialized_spawn_spec(
                                &harness,
                                plan,
                                &intent.run_id,
                                spec,
                            )?;
                            let spec = profile.bind_spec(spec)
                                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                            let fingerprint = crate::c2::spawn_spec_fingerprint(&spec)
                                .map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                            let now = unix_time_ms();
                            let route = profile.route().clone();
                            let context = crate::HarnessDispatchContextV1 {
                                operation_id: intent.operation_id.clone(),
                                node_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                                    route.node_id.as_str(),
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
                                node_incarnation_id: gate4agent_harness_protocol::HarnessSelectorV1::new(
                                    route.expected_incarnation_id.to_string(),
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
                                workspace_id: intent.intent.workspace_id.clone(),
                                provider_profile: intent.intent.provider_profile.clone(),
                                expected_provider: gate4agent_harness_protocol::HarnessSelectorV1::new(
                                    plan.provider.as_str(),
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?,
                                mode: intent.intent.mode,
                                baseline_record_ids: Vec::new(),
                                spawn_spec_fingerprint: fingerprint,
                                dispatched_at_unix_ms: now,
                                idempotency_ref: intent.idempotency_ref.clone(),
                            };
                            let mut dispatching_run = run.clone();
                            dispatching_run.revision = next_runtime_revision(run.revision)?;
                            dispatching_run.lifecycle = HarnessRunLifecycleV1::Dispatching;
                            dispatching_run.updated_at_unix_ms = now;
                            let mut dispatching_operation = operation.clone();
                            dispatching_operation.revision = next_runtime_revision(
                                operation.revision,
                            )?;
                            dispatching_operation.state = HarnessOperationStateV1::Dispatching;
                            dispatching_operation.updated_at_unix_ms = now;
                            dispatching_operation.dispatched_at_unix_ms = Some(now);
                            if plan.harness_mcp
                                == crate::dispatch::HarnessMcpPolicyV1::GrantBound
                            {
                                let ids = deterministic_dispatch_ids(
                                    &intent.operation_id,
                                    plan,
                                ).map_err(|_| HarnessRuntimeError::DispatchPreparation)?;
                                let reservation_id = ids.harness_mcp_reservation_id
                                    .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                                let (grant_id, grant_revision) = match &plan.grant {
                                    crate::dispatch::HarnessGrantPolicyV1::Exact {
                                        grant_id,
                                        revision,
                                    } => (grant_id.clone(), *revision),
                                    crate::dispatch::HarnessGrantPolicyV1::Operator => {
                                        return Err(HarnessRuntimeError::DispatchPreparation);
                                    }
                                };
                                let expires_at_unix_ms = now.checked_add(
                                    plan.deadline_ms.min(
                                        gate4agent_node_protocol::MAX_HARNESS_MCP_RESERVATION_TTL_MS,
                                    ),
                                ).ok_or(HarnessRuntimeError::DispatchPreparation)?;
                                let prepared = harness.begin_run_dispatch_with_harness_mcp(
                                    run.revision,
                                    dispatching_run,
                                    operation.revision,
                                    dispatching_operation,
                                    context,
                                    &spec,
                                    reservation_id,
                                    grant_id,
                                    grant_revision,
                                    expires_at_unix_ms,
                                )?;
                                let pending = adapter.start_arm_harness_mcp_reservation(
                                    &harness,
                                    &route,
                                    prepared,
                                    &spec,
                                )?;
                                start_harness_mcp_arm_finish(
                                    commands.clone(),
                                    intent.operation_id.clone(),
                                    spec,
                                    profile,
                                    pending,
                                );
                                Ok(CoordinatorPreflightStart::HarnessMcpArm)
                            } else {
                                let prepared = harness.issue_spawn_lease(
                                    &catalogs.launch,
                                    run.revision,
                                    dispatching_run,
                                    operation.revision,
                                    dispatching_operation,
                                    context,
                                    spec,
                                )?;
                                let pending = adapter.start_prepared_spawn(prepared, profile)?;
                                Ok(CoordinatorPreflightStart::Spawn { route, pending })
                            }
                            })();
                            match preparation {
                                Ok(CoordinatorPreflightStart::Spawn { route, pending }) => {
                                    start_dispatch_finish(
                                        adapter.clone(),
                                        commands.clone(),
                                        intent.operation_id.clone(),
                                        route,
                                        pending,
                                    );
                                    active_dispatch = Some(ActiveDispatchJob::new(
                                        intent.operation_id,
                                        CoordinatorDispatchPhase::Spawn,
                                    ));
                                }
                                Ok(CoordinatorPreflightStart::HarnessMcpArm) => {
                                    active_dispatch = Some(ActiveDispatchJob::new(
                                        intent.operation_id,
                                        CoordinatorDispatchPhase::HarnessMcpArm,
                                    ));
                                }
                                Err(error) => {
                                    active_dispatch = None;
                                    if harness.engine().operation(&intent.operation_id)
                                        .is_some_and(|operation| {
                                            operation.state
                                                == HarnessOperationStateV1::Dispatching
                                        })
                                    {
                                        if let Some(reservation_id) = apply_spawn_result(
                                            &mut harness,
                                            &catalogs.launch,
                                            &intent.operation_id,
                                            dispatching_start_error_result(&error),
                                            unix_time_ms(),
                                        )? {
                                            if let Some(cleanup) = harness
                                                .harness_mcp_reservation(&reservation_id)
                                                .and_then(pending_harness_mcp_abort)
                                            {
                                                pending_harness_mcp_aborts
                                                    .entry(reservation_id)
                                                    .or_insert(cleanup);
                                            }
                                        }
                                    } else if let Some(reservation_id) = apply_pre_dispatch_result(
                                        &mut harness,
                                        &intent.operation_id,
                                        CoordinatorPreDispatchResult::Failed,
                                        unix_time_ms(),
                                    )? {
                                        if let Some(cleanup) = harness
                                            .harness_mcp_reservation(&reservation_id)
                                            .and_then(pending_harness_mcp_abort)
                                        {
                                            pending_harness_mcp_aborts
                                                .entry(reservation_id)
                                                .or_insert(cleanup);
                                        }
                                    }
                                }
                            }
                        }
                        Some(HostCommand::HarnessMcpActivationFinished {
                            reservation_id,
                            attempt_id,
                            expected_revision,
                            result,
                        }) => {
                            if !harness_mcp_workers.accepts_activation(
                                &reservation_id,
                                attempt_id,
                                expected_revision,
                            ) {
                                continue;
                            }
                            let active = harness_mcp_workers.activations
                                .remove(&reservation_id)
                                .expect("accepted activation remains present");
                            let current_is_exact = harness
                                .harness_mcp_reservation(&reservation_id)
                                .is_some_and(|reservation| {
                                    reservation.revision == expected_revision
                                });
                            let outcome = if !current_is_exact {
                                Err(HarnessRuntimeError::HarnessMcpAuthority)
                            } else {
                                match result {
                                    Ok(proof) if proof.reservation_id() == &reservation_id => {
                                        harness.record_harness_mcp_active(
                                            proof,
                                            unix_time_ms().max(active.updated_at_unix_ms),
                                        ).map_err(HarnessRuntimeError::Harness)
                                    }
                                    Ok(_) => Err(HarnessRuntimeError::HarnessMcpAuthority),
                                    Err(error) => Err(HarnessRuntimeError::C2(error)),
                                }
                            };
                            if let Some(reply) = active.reply {
                                let _ = reply.send(outcome);
                            }
                            start_pending_harness_mcp_abort_workers(
                                &adapter,
                                &commands,
                                &mut harness_mcp_workers,
                                &mut pending_harness_mcp_aborts,
                            );
                        }
                        Some(HostCommand::HarnessMcpAbortFinished {
                            reservation_id,
                            attempt_id,
                            result,
                        }) => {
                            let accepts = pending_harness_mcp_aborts
                                .get(&reservation_id)
                                .is_some_and(|pending| pending.accepts_completion(attempt_id));
                            if !accepts { continue; }
                            if result.is_ok() || matches!(
                                result,
                                Err(HarnessC2Error::HarnessMcpRejected {
                                    code: NodeFailureCode::ReservationNotFound,
                                })
                            ) {
                                pending_harness_mcp_aborts.remove(&reservation_id);
                            } else if let Some(cleanup) = pending_harness_mcp_aborts
                                .get_mut(&reservation_id)
                            {
                                cleanup.attempt_id = None;
                                defer_harness_mcp_abort(cleanup, unix_time_ms());
                            }
                            start_pending_harness_mcp_abort_workers(
                                &adapter,
                                &commands,
                                &mut harness_mcp_workers,
                                &mut pending_harness_mcp_aborts,
                            );
                        }
                        Some(HostCommand::HarnessMcpRelayFinished {
                            reservation_id,
                            call_id,
                            attempt_id,
                            result,
                        }) => {
                            if !harness_mcp_workers.accepts_relay(
                                &reservation_id,
                                &call_id,
                                attempt_id,
                            ) {
                                continue;
                            }
                            harness_mcp_workers.relays.remove(&(
                                reservation_id,
                                call_id,
                            ));
                            let _ = result;
                            start_pending_harness_mcp_abort_workers(
                                &adapter,
                                &commands,
                                &mut harness_mcp_workers,
                                &mut pending_harness_mcp_aborts,
                            );
                        }
                        Some(HostCommand::DispatchFinished { operation_id, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::Spawn)
                            }) {
                                continue;
                            }
                            active_dispatch = None;
                            if let Some(reservation_id) = apply_spawn_result(
                                &mut harness,
                                &catalogs.launch,
                                &operation_id,
                                result,
                                unix_time_ms(),
                            )? {
                                if let Some(cleanup) = harness
                                    .harness_mcp_reservation(&reservation_id)
                                    .and_then(pending_harness_mcp_abort)
                                {
                                    pending_harness_mcp_aborts
                                        .entry(reservation_id)
                                        .or_insert(cleanup);
                                }
                            }
                        }
                        Some(HostCommand::HarnessMcpArmFinished {
                            operation_id,
                            spec,
                            profile,
                            result,
                        }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::HarnessMcpArm)
                            }) {
                                continue;
                            }
                            active_dispatch = None;
                            match result {
                                Ok(proof) => {
                                    let pending = (|| -> Result<_, HarnessRuntimeError> {
                                        let prepared = harness
                                            .record_harness_mcp_armed_and_issue_spawn_lease(
                                            &catalogs.launch,
                                            proof,
                                            unix_time_ms(),
                                            spec,
                                        )?;
                                        Ok(adapter.start_prepared_spawn(prepared, profile)?)
                                    })();
                                    match pending {
                                        Ok(pending) => {
                                            let route = pending_harness_mcp_spawn_route(
                                                &harness,
                                                &operation_id,
                                            )?;
                                            start_dispatch_finish(
                                                adapter.clone(),
                                                commands.clone(),
                                                operation_id.clone(),
                                                route,
                                                pending,
                                            );
                                            active_dispatch = Some(ActiveDispatchJob::new(
                                                operation_id,
                                                CoordinatorDispatchPhase::Spawn,
                                            ));
                                        }
                                        Err(error) => {
                                            if let Some(reservation_id) = apply_spawn_result(
                                                &mut harness,
                                                &catalogs.launch,
                                                &operation_id,
                                                dispatching_start_error_result(&error),
                                                unix_time_ms(),
                                            )? {
                                                if let Some(cleanup) = harness
                                                    .harness_mcp_reservation(&reservation_id)
                                                    .and_then(pending_harness_mcp_abort)
                                                {
                                                    pending_harness_mcp_aborts
                                                        .entry(reservation_id)
                                                        .or_insert(cleanup);
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    if let Some(reservation_id) = apply_spawn_result(
                                        &mut harness,
                                        &catalogs.launch,
                                        &operation_id,
                                        harness_mcp_arm_finish_result(&error),
                                        unix_time_ms(),
                                    )? {
                                        if let Some(cleanup) = harness
                                            .harness_mcp_reservation(&reservation_id)
                                            .and_then(pending_harness_mcp_abort)
                                        {
                                            pending_harness_mcp_aborts
                                                .entry(reservation_id)
                                                .or_insert(cleanup);
                                        }
                                    }
                                }
                            }
                        }
                        Some(HostCommand::DeliveryStageFinished { operation_id, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::Delivery)
                            }) {
                                continue;
                            }
                            active_dispatch = None;
                            match result {
                                Ok(proof) => {
                                    let stage_result = (|| {
                                        let delivery = harness.engine().deliveries()
                                            .find(|delivery| {
                                                delivery.operation_id == operation_id
                                            })
                                            .ok_or(HarnessRuntimeError::DispatchPreparation)?
                                            .clone();
                                        harness.stage_delivery_with_proof(
                                            delivery.revision,
                                            &delivery.delivery_ref,
                                            unix_time_ms(),
                                            &adapter,
                                            proof,
                                        ).map_err(HarnessRuntimeError::Harness)
                                    })();
                                    if let Err(error) = stage_result {
                                        if let Some(reservation_id) = apply_pre_dispatch_result(
                                            &mut harness,
                                            &operation_id,
                                            delivery_stage_completion_result(&error),
                                            unix_time_ms(),
                                        )? {
                                            if let Some(cleanup) = harness
                                                .harness_mcp_reservation(&reservation_id)
                                                .and_then(pending_harness_mcp_abort)
                                            {
                                                pending_harness_mcp_aborts
                                                    .entry(reservation_id)
                                                    .or_insert(cleanup);
                                            }
                                        }
                                        continue;
                                    }
                                    let intent = harness.pending_scheduled_dispatch()?
                                        .filter(|intent| intent.operation_id == operation_id)
                                        .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                                    active_dispatch = start_or_terminalize_dispatch_job(
                                        &mut harness,
                                        &adapter,
                                        &commands,
                                        &catalogs,
                                        &mut pending_harness_mcp_aborts,
                                        intent,
                                    )?;
                                }
                                Err(error) => {
                                    if let Some(reservation_id) = apply_pre_dispatch_result(
                                        &mut harness,
                                        &operation_id,
                                        delivery_pre_dispatch_result(&error),
                                        unix_time_ms(),
                                    )? {
                                        if let Some(cleanup) = harness
                                            .harness_mcp_reservation(&reservation_id)
                                            .and_then(pending_harness_mcp_abort)
                                        {
                                            pending_harness_mcp_aborts
                                                .entry(reservation_id)
                                                .or_insert(cleanup);
                                        }
                                    }
                                }
                            }
                        }
                        Some(HostCommand::ContinuationExportFinished { operation_id, result }) => {
                            if !active_dispatch.as_ref().is_some_and(|job| {
                                job.is(&operation_id, CoordinatorDispatchPhase::Continuation)
                            }) {
                                continue;
                            }
                            active_dispatch = None;
                            match result {
                                Ok(outcome) => {
                                    harness.apply_continuation_export_outcome(
                                        outcome,
                                        unix_time_ms(),
                                    )?;
                                    let run_id = harness.engine().operation(&operation_id)
                                        .and_then(|operation| operation.run_id.as_ref())
                                        .ok_or(HarnessRuntimeError::DispatchPreparation)?
                                        .clone();
                                    let continuation = harness.engine()
                                        .continuation_for_run(&run_id)
                                        .ok_or(HarnessRuntimeError::DispatchPreparation)?
                                        .clone();
                                    match continuation.state {
                                        gate4agent_harness_protocol::HarnessContinuationStateV1::Exported => {
                                            let intent = harness.pending_scheduled_dispatch()?
                                                .filter(|intent| {
                                                    intent.operation_id == operation_id
                                                })
                                                .ok_or(HarnessRuntimeError::DispatchPreparation)?;
                                            active_dispatch = start_or_terminalize_dispatch_job(
                                                &mut harness,
                                                &adapter,
                                                &commands,
                                                &catalogs,
                                                &mut pending_harness_mcp_aborts,
                                                intent,
                                            )?;
                                        }
                                        gate4agent_harness_protocol::HarnessContinuationStateV1::OutcomeUnknown => {
                                            if let Some(reservation_id) = apply_pre_dispatch_result(
                                                &mut harness,
                                                &operation_id,
                                                CoordinatorPreDispatchResult::OutcomeUnknown,
                                                unix_time_ms(),
                                            )? {
                                                if let Some(cleanup) = harness
                                                    .harness_mcp_reservation(&reservation_id)
                                                    .and_then(pending_harness_mcp_abort)
                                                {
                                                    pending_harness_mcp_aborts
                                                        .entry(reservation_id)
                                                        .or_insert(cleanup);
                                                }
                                            }
                                        }
                                        gate4agent_harness_protocol::HarnessContinuationStateV1::Expired => {
                                            if let Some(reservation_id) = apply_pre_dispatch_result(
                                                &mut harness,
                                                &operation_id,
                                                CoordinatorPreDispatchResult::Failed,
                                                unix_time_ms(),
                                            )? {
                                                if let Some(cleanup) = harness
                                                    .harness_mcp_reservation(&reservation_id)
                                                    .and_then(pending_harness_mcp_abort)
                                                {
                                                    pending_harness_mcp_aborts
                                                        .entry(reservation_id)
                                                        .or_insert(cleanup);
                                                }
                                            }
                                        }
                                        _ => return Err(HarnessRuntimeError::DispatchPreparation),
                                    }
                                }
                                Err(_) => {
                                    let run_id = harness.engine().operation(&operation_id)
                                        .and_then(|operation| operation.run_id.as_ref())
                                        .ok_or(HarnessRuntimeError::DispatchPreparation)?
                                        .clone();
                                    let continuation = harness.engine()
                                        .continuation_for_run(&run_id)
                                        .ok_or(HarnessRuntimeError::DispatchPreparation)?
                                        .clone();
                                    if continuation.state
                                        == gate4agent_harness_protocol::HarnessContinuationStateV1::Exporting
                                    {
                                        harness.recover_exporting_continuation_outcome_unknown(
                                            &continuation.continuation_ref,
                                            continuation.revision,
                                            unix_time_ms(),
                                        )?;
                                    }
                                    apply_pre_dispatch_result(
                                        &mut harness,
                                        &operation_id,
                                        CoordinatorPreDispatchResult::OutcomeUnknown,
                                        unix_time_ms(),
                                    )?;
                                }
                            }
                        }
                        Some(HostCommand::ObservationRecoveryFinished {
                            route,
                            attempt_id,
                            requested_after,
                            result,
                        }) => {
                            finish_observation_recovery(
                                &mut observation_recovery,
                                &mut harness,
                                &mut observation,
                                &mut support,
                                &mut runtime_inventory,
                                route,
                                attempt_id,
                                requested_after,
                                result,
                            )?;
                        }
                        Some(HostCommand::RunReadFinished { completion, reply }) => {
                            run_read_workers.finish();
                            let (prepared, result) = completion.into_parts();
                            let reply_value = match validate_run_read_completion_origin(
                                harness.engine().run(prepared.run_id()),
                                &prepared,
                            ) {
                                Err(error) => HarnessOperatorReplyV1::Error { error },
                                Ok(()) => match result {
                                    Ok(response) => HarnessOperatorReplyV1::Ok { response },
                                    Err(error) => HarnessOperatorReplyV1::Error {
                                        error: map_run_read_error(error),
                                    },
                                },
                            };
                            let _ = reply.send(reply_value);
                        }
                        Some(HostCommand::NativeHistoryWorkerFinished) => {
                            native_history_workers.finish();
                        }
                        Some(HostCommand::Shutdown { reply }) => {
                            let result = harness.flush().map_err(HarnessRuntimeError::Harness)
                                .and_then(|_| observation.flush().map_err(HarnessRuntimeError::Observation));
                            let successful = result.is_ok();
                            let _ = reply.send(result);
                            if successful { return Ok(()); }
                            return Err(HarnessRuntimeError::FlushFailed);
                        }
                        None => return Err(HarnessRuntimeError::HostStopped),
                    }
                }
                event = events.recv(), if events_open => {
                    match event {
                        Some(event) => {
                            let result = if matches!(
                                event.event,
                                C2NodeEvent::HarnessMcpReadCall { .. }
                            ) {
                                let plan = prepare_harness_mcp_read_call(
                                    &adapter,
                                    &harness,
                                    &observation,
                                    &support,
                                    event,
                                )?;
                                let _ = schedule_harness_mcp_relay(
                                    &adapter,
                                    &commands,
                                    &mut harness_mcp_workers,
                                    &pending_harness_mcp_aborts,
                                    &harness_mcp_rejects,
                                    plan,
                                )?;
                                Ok(())
                            } else {
                                apply_or_buffer_host_live_event(
                                    &adapter,
                                    &mut harness,
                                    &mut observation,
                                    &mut support,
                                    &mut observation_recovery,
                                    &mut runtime_inventory,
                                    event,
                                )
                            };
                            if let Err(error) = result {
                                if !matches!(error, HarnessRuntimeError::C2(_)) {
                                    return Err(error);
                                }
                            }
                        }
                        None => {
                            support.mark_all_unhealthy();
                            events_open = false;
                        }
                    }
                }
                changed = topology.changed(), if topology_open => {
                    match changed {
                        Ok(routes) => {
                            support.mark_all_unhealthy();
                            let routes = routes.into_iter()
                                .map(|observation_route| observation_route.route().clone())
                                .collect::<Vec<_>>();
                            observation_recovery.reconcile_topology(&routes);
                            runtime_inventory.reconcile_topology(&routes);
                            for route in &routes {
                                support.mark_unhealthy(
                                    &route.node_id,
                                    route.expected_incarnation_id,
                                );
                            }
                            if active_dispatch.is_none() {
                                if let Some(intent) = harness.pending_scheduled_dispatch()? {
                                    active_dispatch = start_or_terminalize_dispatch_job(
                                        &mut harness,
                                        &adapter,
                                        &commands,
                                        &catalogs,
                                        &mut pending_harness_mcp_aborts,
                                        intent,
                                    )?;
                                }
                            }
                        }
                        Err(_) => {
                            support.mark_all_unhealthy();
                            topology_open = false;
                        }
                    }
                }
                _ = recovery_retry.tick() => {
                    let routes = adapter.observation_routes();
                    support.reconcile_current_routes(&routes);
                    for route in routes {
                        if support.is_authoritative(
                            &route.node_id,
                            route.expected_incarnation_id,
                        ) {
                            continue;
                        }
                        support.mark_unhealthy(
                            &route.node_id,
                            route.expected_incarnation_id,
                        );
                        observation_recovery.ensure_route(route);
                    }
                    let harness_mcp_actions = prepare_harness_mcp_reconcile(
                        &mut harness,
                        &adapter,
                        &observation,
                        &support,
                    )?;
                    schedule_harness_mcp_reconcile_workers(
                        &adapter,
                        &commands,
                        &mut harness_mcp_workers,
                        harness_mcp_actions,
                        &mut pending_harness_mcp_aborts,
                    );
                    start_pending_harness_mcp_abort_workers(
                        &adapter,
                        &commands,
                        &mut harness_mcp_workers,
                        &mut pending_harness_mcp_aborts,
                    );
                    if active_dispatch.is_none() {
                        if let Some(intent) = harness.pending_scheduled_dispatch()? {
                            active_dispatch = start_or_terminalize_dispatch_job(
                                &mut harness,
                                &adapter,
                                &commands,
                                &catalogs,
                                &mut pending_harness_mcp_aborts,
                                intent,
                            )?;
                        }
                    }
                }
                accepted = listener.accept() => {
                    let (stream, peer) = accepted.map_err(|_| HarnessRuntimeError::AcceptFailed)?;
                    if !peer.ip().is_loopback() { continue; }
                    let Ok(permit) = connections.clone().try_acquire_owned() else { continue; };
                    let request_commands = commands.clone();
                    let request_operator_authority = operator_authority.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _ = handle_connection(
                            stream,
                            request_commands,
                            request_operator_authority,
                        ).await;
                    });
                }
            }
            start_pending_observation_recoveries(
                &adapter,
                &commands,
                &observation,
                &mut observation_recovery,
            );
        }
    });
    Ok((handle, task))
}

fn scheduled_dispatch_from_operator_response(
    response: &HarnessOperatorResponseV1,
) -> Option<HarnessDispatchIntentV1> {
    match response {
        HarnessOperatorResponseV1::Schedule(
            gate4agent_harness_protocol::HarnessScheduleOutcomeV1::Dispatch(intent),
        ) => Some(intent.clone()),
        HarnessOperatorResponseV1::TaskStarted(outcome) if !outcome.replayed => {
            Some(outcome.dispatch.clone())
        }
        _ => None,
    }
}

async fn execute_harness_mcp_reconcile(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    actions: Vec<HarnessMcpReconcileAction>,
    pending_aborts: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) -> Result<(), HarnessRuntimeError> {
    for action in actions {
        match action {
            HarnessMcpReconcileAction::Activate {
                route,
                reservation,
                record_id,
                session,
                updated_at_unix_ms,
            } => {
                if let Ok(proof) = adapter.activate_harness_mcp_reservation(
                    &route,
                    reservation,
                    record_id,
                    session,
                ).await {
                    harness.record_harness_mcp_active(
                        proof,
                        unix_time_ms().max(updated_at_unix_ms),
                    )?;
                }
            }
            HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            } => {
                pending_aborts.entry(reservation_id.clone()).or_insert(
                    PendingHarnessMcpAbort {
                        route,
                        reservation_id,
                        activation_digest,
                        attempts: 0,
                        retry_after_unix_ms: 0,
                        attempt_id: None,
                    },
                );
            }
        }
    }
    Ok(())
}

fn schedule_harness_mcp_reconcile_workers(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    actions: Vec<HarnessMcpReconcileAction>,
    pending_aborts: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    for action in actions {
        match action {
            HarnessMcpReconcileAction::Activate {
                route,
                reservation,
                record_id,
                session,
                updated_at_unix_ms,
            } => {
                let _ = schedule_harness_mcp_activation(
                    adapter,
                    commands,
                    workers,
                    pending_aborts,
                    route,
                    reservation,
                    record_id,
                    session,
                    updated_at_unix_ms,
                    None,
                );
            }
            HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            } => {
                pending_aborts.entry(reservation_id.clone()).or_insert(
                    PendingHarnessMcpAbort {
                        route,
                        reservation_id,
                        activation_digest,
                        attempts: 0,
                        retry_after_unix_ms: 0,
                        attempt_id: None,
                    },
                );
            }
        }
    }
}

#[derive(Clone)]
struct PendingHarnessMcpAbort {
    route: NodeRoute,
    reservation_id: HarnessMcpReservationId,
    activation_digest: HarnessMcpActivationDigest,
    attempts: u8,
    retry_after_unix_ms: u64,
    attempt_id: Option<u64>,
}

impl PendingHarnessMcpAbort {
    fn accepts_completion(&self, attempt_id: u64) -> bool {
        self.attempt_id == Some(attempt_id)
    }
}

fn schedule_harness_mcp_activation(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    route: NodeRoute,
    reservation: crate::HarnessMcpReservationV1,
    record_id: SessionRecordId,
    session: SessionAddress,
    updated_at_unix_ms: u64,
    reply: Option<oneshot::Sender<Result<(), HarnessRuntimeError>>>,
) -> Result<(), Option<oneshot::Sender<Result<(), HarnessRuntimeError>>>> {
    let reservation_id = reservation.reservation_id.clone();
    if workers.activations.contains_key(&reservation_id)
        || !workers.has_capacity(pending_aborts)
    {
        return Err(reply);
    }
    let attempt_id = workers.allocate_attempt_id();
    let expected_revision = reservation.revision;
    workers.activations.insert(
        reservation_id,
        ActiveHarnessMcpActivation {
            attempt_id,
            expected_revision,
            updated_at_unix_ms,
            reply,
        },
    );
    start_harness_mcp_activation_finish(
        adapter.clone(),
        commands.clone(),
        route,
        reservation,
        record_id,
        session,
        attempt_id,
        expected_revision,
    );
    Ok(())
}

fn start_pending_harness_mcp_abort_workers(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    pending: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    let current_routes = adapter.observation_routes();
    let now = unix_time_ms();
    let reservation_ids = pending.keys().cloned().collect::<Vec<_>>();
    for reservation_id in reservation_ids {
        if !workers.has_capacity(pending) { break; }
        let Some(cleanup) = pending.get(&reservation_id) else { continue; };
        if cleanup.attempt_id.is_some() || now < cleanup.retry_after_unix_ms {
            continue;
        }
        let Some(current_route) = current_routes.iter()
            .find(|route| route.node_id == cleanup.route.node_id) else {
                continue;
            };
        if current_route != &cleanup.route {
            pending.remove(&reservation_id);
            continue;
        }
        let attempt_id = workers.allocate_attempt_id();
        let cleanup = pending.get_mut(&reservation_id)
            .expect("selected pending abort remains present");
        cleanup.attempt_id = Some(attempt_id);
        start_harness_mcp_abort_finish(
            adapter.clone(),
            commands.clone(),
            cleanup.clone(),
            attempt_id,
        );
    }
}

fn schedule_harness_mcp_relay(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    workers: &mut HarnessMcpWorkerRegistry,
    pending_aborts: &BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    capacity_rejects: &mpsc::Sender<HarnessMcpRelayPlan>,
    plan: HarnessMcpRelayPlan,
) -> Result<bool, HarnessRuntimeError> {
    let key = (plan.reservation_id.clone(), plan.call_id.clone());
    if workers.relays.contains_key(&key) {
        return Ok(false);
    }
    if !workers.has_capacity(pending_aborts) {
        enqueue_harness_mcp_capacity_rejection(capacity_rejects, plan)?;
        return Ok(true);
    }
    let attempt_id = workers.allocate_attempt_id();
    workers.relays.insert(key, ActiveHarnessMcpRelay { attempt_id });
    start_harness_mcp_relay_finish(
        adapter.clone(),
        commands.clone(),
        plan,
        attempt_id,
    );
    Ok(true)
}

fn enqueue_harness_mcp_capacity_rejection(
    capacity_rejects: &mpsc::Sender<HarnessMcpRelayPlan>,
    mut plan: HarnessMcpRelayPlan,
) -> Result<(), HarnessRuntimeError> {
    plan.outcome = Err(HarnessMcpRejectReasonV1::Internal);
    capacity_rejects.try_send(plan)
        .map_err(|_| HarnessRuntimeError::HarnessMcpRejectQueueFull)
}

fn durable_harness_mcp_abort_cleanup(
    harness: &HarnessService,
) -> BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort> {
    harness.harness_mcp_reservations.values()
        .filter(|reservation| {
            reservation.state == crate::HarnessMcpReservationStateV1::Revoked
        })
        .filter_map(|reservation| pending_harness_mcp_abort(reservation))
        .map(|pending| (pending.reservation_id.clone(), pending))
        .collect()
}

fn non_revoked_harness_mcp_abort_cleanup(
    harness: &HarnessService,
) -> BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort> {
    harness.harness_mcp_reservations.values()
        .filter(|reservation| {
            reservation.state != crate::HarnessMcpReservationStateV1::Revoked
        })
        .filter_map(|reservation| pending_harness_mcp_abort(reservation))
        .map(|pending| (pending.reservation_id.clone(), pending))
        .collect()
}

fn pending_harness_mcp_abort(
    reservation: &crate::HarnessMcpReservationV1,
) -> Option<PendingHarnessMcpAbort> {
    Some(PendingHarnessMcpAbort {
        route: reservation_route(reservation).ok()?,
        reservation_id: reservation.reservation_id.clone(),
        activation_digest: reservation.activation_digest.clone(),
        attempts: 0,
        retry_after_unix_ms: 0,
        attempt_id: None,
    })
}

fn enqueue_newly_revoked_harness_mcp_aborts(
    harness: &HarnessService,
    prior: BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
    pending: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    for (reservation_id, cleanup) in prior {
        if harness.harness_mcp_reservation_state(&reservation_id)
            == Some(crate::HarnessMcpReservationStateV1::Revoked)
        {
            pending.entry(reservation_id).or_insert(cleanup);
        }
    }
}

async fn retry_pending_harness_mcp_aborts(
    adapter: &HarnessC2Adapter,
    pending: &mut BTreeMap<HarnessMcpReservationId, PendingHarnessMcpAbort>,
) {
    let current_routes = adapter.observation_routes();
    let now = unix_time_ms();
    let reservation_ids = pending.keys().cloned().collect::<Vec<_>>();
    for reservation_id in reservation_ids {
        let Some(cleanup) = pending.get(&reservation_id).cloned() else { continue; };
        if now < cleanup.retry_after_unix_ms {
            continue;
        }
        let Some(current_route) = current_routes.iter()
            .find(|route| route.node_id == cleanup.route.node_id) else {
                continue;
            };
        if current_route != &cleanup.route {
            pending.remove(&reservation_id);
            continue;
        }
        let result = adapter.abort_harness_mcp_reservation(
            &cleanup.route,
            &cleanup.reservation_id,
            &cleanup.activation_digest,
        ).await;
        if result.is_ok() || matches!(
            result,
            Err(HarnessC2Error::HarnessMcpRejected {
                code: NodeFailureCode::ReservationNotFound,
            })
        ) {
            pending.remove(&reservation_id);
            continue;
        }
        if let Some(cleanup) = pending.get_mut(&reservation_id) {
            defer_harness_mcp_abort(cleanup, now);
        }
    }
}

fn defer_harness_mcp_abort(cleanup: &mut PendingHarnessMcpAbort, now_unix_ms: u64) {
    cleanup.attempts = cleanup.attempts.saturating_add(1);
    let shift = u32::from(cleanup.attempts.min(5));
    let retry_ms = 1_000_u64.checked_shl(shift)
        .unwrap_or(HARNESS_MCP_ABORT_RETRY_MAX_MS)
        .min(HARNESS_MCP_ABORT_RETRY_MAX_MS);
    cleanup.retry_after_unix_ms = now_unix_ms.saturating_add(retry_ms);
}

enum HarnessMcpReconcileAction {
    Activate {
        route: NodeRoute,
        reservation: crate::HarnessMcpReservationV1,
        record_id: SessionRecordId,
        session: SessionAddress,
        updated_at_unix_ms: u64,
    },
    Abort {
        route: NodeRoute,
        reservation_id: HarnessMcpReservationId,
        activation_digest: HarnessMcpActivationDigest,
    },
}

fn prepare_harness_mcp_reconcile(
    harness: &mut HarnessService,
    adapter: &HarnessC2Adapter,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
) -> Result<Vec<HarnessMcpReconcileAction>, HarnessRuntimeError> {
    let now = unix_time_ms();
    let current_routes = adapter.observation_routes();
    let mut actions = Vec::new();
    let candidates = harness.harness_mcp_reservations.values()
        .map(|reservation| (
            reservation.reservation_id.clone(),
            reservation.state,
            reservation.activation_digest.clone(),
            reservation.expires_at_unix_ms,
            reservation.updated_at_unix_ms,
        ))
        .collect::<Vec<_>>();
    for (
        reservation_id,
        state,
        activation_digest,
        expires_at_unix_ms,
        updated_at_unix_ms,
    ) in candidates {
        let route = harness.harness_mcp_reservation(&reservation_id)
            .and_then(|reservation| reservation_route(reservation).ok());
        let bootstrap_expired = matches!(
            state,
            crate::HarnessMcpReservationStateV1::Prepared
                | crate::HarnessMcpReservationStateV1::Armed
                | crate::HarnessMcpReservationStateV1::Bound
        ) && now >= expires_at_unix_ms;
        if bootstrap_expired {
            harness.revoke_harness_mcp_reservation(
                &reservation_id,
                now.max(updated_at_unix_ms),
            )?;
            if let Some(route) = route {
                actions.push(HarnessMcpReconcileAction::Abort {
                    route,
                    reservation_id,
                    activation_digest,
                });
            }
            continue;
        }
        if !matches!(
            state,
            crate::HarnessMcpReservationStateV1::Bound
                | crate::HarnessMcpReservationStateV1::Active
        ) {
            continue;
        }
        let route = match route {
            Some(route) => route,
            None => {
                harness.revoke_harness_mcp_reservation(
                    &reservation_id,
                    now.max(updated_at_unix_ms),
                )?;
                continue;
            }
        };
        let Some(current_route) = current_routes.iter()
            .find(|current| current.node_id == route.node_id) else {
                continue;
            };
        if current_route != &route {
            harness.revoke_harness_mcp_reservation(
                &reservation_id,
                now.max(updated_at_unix_ms),
            )?;
            actions.push(HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            });
            continue;
        }
        let binding = match harness.harness_mcp_binding_for_reconcile(&reservation_id) {
            Ok(binding) => binding,
            Err(_) => {
                harness.revoke_harness_mcp_reservation(
                    &reservation_id,
                    now.max(updated_at_unix_ms),
                )?;
                actions.push(HarnessMcpReconcileAction::Abort {
                    route,
                    reservation_id,
                    activation_digest,
                });
                continue;
            }
        };
        if !support.is_authoritative(&route.node_id, route.expected_incarnation_id) {
            continue;
        }
        if verify_observation_credential_binding(observation, support, &binding).is_err() {
            harness.revoke_harness_mcp_reservation(
                &reservation_id,
                now.max(updated_at_unix_ms),
            )?;
            actions.push(HarnessMcpReconcileAction::Abort {
                route,
                reservation_id,
                activation_digest,
            });
            continue;
        }
        let Ok((reservation, record_id, session)) = harness
            .validate_activatable_harness_mcp_authority(&reservation_id, now) else {
            continue;
        };
        actions.push(HarnessMcpReconcileAction::Activate {
            route,
            reservation,
            record_id,
            session,
            updated_at_unix_ms,
        });
    }
    Ok(actions)
}

fn reservation_route(
    reservation: &crate::HarnessMcpReservationV1,
) -> Result<NodeRoute, HarnessRuntimeError> {
    Ok(NodeRoute {
        node_id: NodeId::new(reservation.node_id.as_str())
            .map_err(|_| HarnessRuntimeError::HarnessMcpAuthority)?,
        expected_incarnation_id: reservation.node_incarnation_id.as_str().parse()
            .map_err(|_| HarnessRuntimeError::HarnessMcpAuthority)?,
    })
}

fn pending_harness_mcp_spawn_route(
    harness: &HarnessService,
    operation_id: &HarnessOperationId,
) -> Result<NodeRoute, HarnessRuntimeError> {
    let reservation = harness.harness_mcp_reservations.values()
        .find(|reservation| &reservation.operation_id == operation_id)
        .ok_or(HarnessRuntimeError::HarnessMcpAuthority)?;
    reservation_route(reservation)
}

#[derive(Clone)]
struct HarnessOperatorCredentialAuthority {
    digest: [u8; 32],
}

impl HarnessOperatorCredentialAuthority {
    fn new(credential: HarnessOperatorCredential) -> Result<Self, HarnessRuntimeError> {
        let digest = operator_credential_digest(&credential)?;
        drop(credential);
        Ok(Self { digest })
    }

    fn verify(&self, credential: &HarnessOperatorCredential) -> Result<bool, HarnessRuntimeError> {
        let actual = operator_credential_digest(credential)?;
        Ok(proofs_match(&actual, &self.digest))
    }
}

#[derive(Default)]
pub(crate) struct HarnessRuntimeInventoryCache {
    nodes: BTreeMap<NodeId, HarnessRuntimeNodeInventoryV1>,
}

impl HarnessRuntimeInventoryCache {
    fn refresh(&mut self, resync: &HarnessObservationResync, observed_at_unix_ms: u64) {
        let route = resync.route();
        self.nodes.insert(route.node_id.clone(), HarnessRuntimeNodeInventoryV1 {
            node_id: route.node_id.as_str().to_owned(),
            incarnation_id: route.expected_incarnation_id.to_string(),
            observed_at_unix_ms,
            event_sequence: resync.event_sequence(),
            inventory: redact_runtime_inventory(
                gate4agent_c2_protocol::SlimNodeInventory::from_c2_snapshot(resync.snapshot()),
            ),
        });
    }

    fn reconcile_topology(&mut self, routes: &[NodeRoute]) {
        self.nodes.retain(|node_id, inventory| {
            routes.iter().any(|route| {
                &route.node_id == node_id
                    && route.expected_incarnation_id.to_string() == inventory.incarnation_id
            })
        });
    }

    fn invalidate(&mut self, route: &NodeRoute) {
        if self.nodes.get(&route.node_id).is_some_and(|inventory| {
            inventory.incarnation_id == route.expected_incarnation_id.to_string()
        }) {
            self.nodes.remove(&route.node_id);
        }
    }

    fn page(
        &self,
        after_node_id: Option<&str>,
        limit: u16,
    ) -> HarnessRuntimeInventoryPageV1 {
        let mut nodes = self.nodes.values()
            .filter(|node| match after_node_id {
                Some(after) => node.node_id.as_str() > after,
                None => true,
            })
            .take(usize::from(limit) + 1)
            .cloned()
            .collect::<Vec<_>>();
        let has_more = nodes.len() > usize::from(limit);
        if has_more { nodes.pop(); }
        let next_cursor = has_more.then(|| {
            nodes.last().expect("nonzero runtime inventory page limit").node_id.clone()
        });
        HarnessRuntimeInventoryPageV1 { nodes, next_cursor }
    }

    fn correlation_availability(
        &self,
        run: &gate4agent_harness_protocol::HarnessRunV1,
        binding: &HarnessSessionBindingV1,
    ) -> (HarnessRunCorrelationAvailabilityV1, Option<u64>) {
        let Some(node) = self.nodes.values().find(|node| {
            node.node_id == binding.node_id.as_str()
        }) else {
            return (HarnessRunCorrelationAvailabilityV1::NotObserved, None);
        };
        let observed_at = Some(node.observed_at_unix_ms);
        if node.incarnation_id != binding.node_incarnation.as_str() {
            return (
                HarnessRunCorrelationAvailabilityV1::StaleIncarnation,
                observed_at,
            );
        }
        let HarnessSessionIdentityV1::Managed {
            record_id,
            active_session,
        } = &binding.session else {
            return (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at);
        };
        let Some(record) = node.inventory.managed_sessions.iter().find(|record| {
            record.record_id == record_id.as_str()
        }) else {
            return (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at);
        };
        let expected_mode = match run.intent.mode {
            HarnessExecutionModeV1::Pty => HarnessRuntimeManagedModeV1::Pty,
            HarnessExecutionModeV1::Inline => HarnessRuntimeManagedModeV1::Inline,
        };
        if record.workspace_id != binding.workspace_id.as_str()
            || record.mode != expected_mode
        {
            return (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at);
        }
        let exact_active = active_session.as_ref().is_some_and(|active| {
            record.active_binding.as_ref().is_some_and(|current| {
                current.workspace_id == binding.workspace_id.as_str()
                    && current.instance_id == active.instance_id
                    && current.generation == active.generation
            })
        });
        if record.state == HarnessRuntimeManagedStateV1::Live && exact_active {
            return (HarnessRunCorrelationAvailabilityV1::Available, observed_at);
        }
        if active_session.is_none()
            && record.state == HarnessRuntimeManagedStateV1::Dormant
            && record.active_binding.is_none()
        {
            return (HarnessRunCorrelationAvailabilityV1::Dormant, observed_at);
        }
        (HarnessRunCorrelationAvailabilityV1::Unavailable, observed_at)
    }
}

fn project_operator_run_correlation(
    harness: &HarnessService,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
) -> Result<HarnessRunCorrelationV1, HarnessOperatorHostErrorV1> {
    let run = harness.engine().run(run_id)
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    project_run_correlation(run, runtime_inventory)
}

fn project_operator_run_transfer(
    harness: &HarnessService,
    run_id: &gate4agent_harness_protocol::HarnessRunId,
) -> Result<HarnessRunTransferSummaryV1, HarnessOperatorHostErrorV1> {
    let run = harness.engine().run(run_id)
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    project_run_transfer(
        run,
        harness.engine().delivery_for_run(run_id),
        harness.engine().continuation_for_run(run_id),
    )
}

fn project_run_transfer(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    delivery: Option<&gate4agent_harness_protocol::HarnessDeliveryV1>,
    continuation: Option<&gate4agent_harness_protocol::HarnessContinuationV1>,
) -> Result<HarnessRunTransferSummaryV1, HarnessOperatorHostErrorV1> {
    run.validate().map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    let delivery = delivery.map(|delivery| {
        delivery.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
        if delivery.run_id != run.run_id || delivery.task_id != run.task_id {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        Ok(HarnessRunDeliveryTransferV1 {
            delivery_ref: delivery.delivery_ref.clone(),
            revision: delivery.revision,
            state: delivery.state,
            selector: delivery.bundle.selector.clone(),
            bundle_id: delivery.bundle.bundle_id.clone(),
            bundle_revision: delivery.bundle.revision.clone(),
            bundle_digest: delivery.bundle.digest.clone(),
            manifest_digest: delivery.bundle.manifest_digest.clone(),
            receipt_ref: delivery.receipt.as_ref().map(|receipt| receipt.receipt_ref.clone()),
            created_at_unix_ms: delivery.created_at_unix_ms,
            updated_at_unix_ms: delivery.updated_at_unix_ms,
            staged_at_unix_ms: delivery.stage_receipt.as_ref()
                .map(|receipt| receipt.staged_at_unix_ms),
            committed_at_unix_ms: delivery.receipt.as_ref()
                .map(|receipt| receipt.committed_at_unix_ms),
        })
    }).transpose()?;
    let continuation = continuation.map(|continuation| {
        continuation.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
        if continuation.target_run_id != run.run_id {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        let context = continuation.context.as_ref().map(|context| {
            HarnessRunContextTransferV1 {
                context_ref: context.id.clone(),
                digest: context.digest.clone(),
                source_message_count: context.source_message_count,
                retained_message_count: context.retained_message_count,
                byte_len: context.byte_len,
                truncated: context.truncated,
            }
        });
        Ok(HarnessRunContinuationTransferV1 {
            continuation_ref: continuation.continuation_ref.clone(),
            receipt_ref: continuation.receipt_ref.clone(),
            revision: continuation.revision,
            state: continuation.state,
            source_run_id: continuation.source_run_id.clone(),
            target_run_id: continuation.target_run_id.clone(),
            source_provider: continuation.source_provider.clone(),
            context,
            prepared_at_unix_ms: continuation.prepared_at_unix_ms,
            exporting_at_unix_ms: continuation.exporting_at_unix_ms,
            exported_at_unix_ms: continuation.exported_at_unix_ms,
            bound_at_unix_ms: continuation.bound_at_unix_ms,
            expired_at_unix_ms: continuation.expired_at_unix_ms,
            outcome_unknown_at_unix_ms: continuation.outcome_unknown_at_unix_ms,
            outcome_unknown_reason: continuation.outcome_unknown_reason,
            created_at_unix_ms: continuation.created_at_unix_ms,
            updated_at_unix_ms: continuation.updated_at_unix_ms,
        })
    }).transpose()?;
    let transfer = HarnessRunTransferSummaryV1 {
        run_id: run.run_id.clone(),
        run_revision: run.revision,
        delivery,
        continuation,
    };
    transfer.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(transfer)
}

fn project_run_correlation(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    runtime_inventory: &HarnessRuntimeInventoryCache,
) -> Result<HarnessRunCorrelationV1, HarnessOperatorHostErrorV1> {
    run.validate().map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    let binding = run.binding.as_ref()
        .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
    let node_incarnation_id = HarnessNodeIncarnationV1::new(
        binding.node_incarnation.as_str(),
    ).map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    let worktree = match &run.intent.worktree {
        HarnessWorktreeIntentV1::Existing => HarnessRunWorktreeViewV1::Existing,
        HarnessWorktreeIntentV1::Managed { worktree_ref } => {
            HarnessRunWorktreeViewV1::Managed {
                worktree_ref: worktree_ref.clone(),
            }
        }
    };
    let session = match &binding.session {
        HarnessSessionIdentityV1::Managed { record_id, active_session } => {
            HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: record_id.clone(),
                active_session: active_session.clone(),
            })
        }
        HarnessSessionIdentityV1::Inline { inline_ref } => {
            HarnessRunSessionViewV1::Inline(HarnessInlineRunSessionV1 {
                inline_ref: inline_ref.clone(),
            })
        }
    };
    let (availability, observed_at_unix_ms) =
        runtime_inventory.correlation_availability(run, binding);
    let correlation = HarnessRunCorrelationV1 {
        run_id: run.run_id.clone(),
        run_revision: run.revision,
        task_id: run.task_id.clone(),
        node_id: binding.node_id.clone(),
        node_incarnation_id,
        workspace_id: binding.workspace_id.clone(),
        provider_profile: run.intent.provider_profile.clone(),
        mode: run.intent.mode,
        worktree,
        session,
        availability,
        observed_at_unix_ms,
    };
    correlation.validate().map_err(|_| HarnessOperatorHostErrorV1::NotFound)?;
    Ok(correlation)
}

fn redact_runtime_inventory(
    inventory: gate4agent_c2_protocol::SlimNodeInventory,
) -> HarnessRuntimeInventoryV1 {
    let enabled_providers = inventory.enabled_providers.into_iter()
        .map(|provider| provider.as_str().to_owned())
        .collect();
    let workspaces = inventory.workspaces.into_iter().map(|(_, workspace)| {
        let workspace_id = workspace.workspace_id.as_str().to_owned();
        let sessions = workspace.sessions.into_iter().map(|session| HarnessRuntimeSessionV1 {
            instance_id: session.instance_id.0,
            generation: session.generation.0,
            provider: session.agent_id,
            transport: match session.transport {
                gate4agent_types::TransportKind::Pty => HarnessRuntimeTransportV1::Pty,
                gate4agent_types::TransportKind::Pipe => HarnessRuntimeTransportV1::Pipe,
                gate4agent_types::TransportKind::Acp => HarnessRuntimeTransportV1::Acp,
            },
            status: match session.status {
                gate4agent_c2_protocol::SlimSessionStatus::Registered => {
                    HarnessRuntimeSessionStatusV1::Registered
                }
                gate4agent_c2_protocol::SlimSessionStatus::Starting => {
                    HarnessRuntimeSessionStatusV1::Starting
                }
                gate4agent_c2_protocol::SlimSessionStatus::Running => {
                    HarnessRuntimeSessionStatusV1::Running
                }
                gate4agent_c2_protocol::SlimSessionStatus::Stopping => {
                    HarnessRuntimeSessionStatusV1::Stopping
                }
                gate4agent_c2_protocol::SlimSessionStatus::Exited => {
                    HarnessRuntimeSessionStatusV1::Exited
                }
                gate4agent_c2_protocol::SlimSessionStatus::Failed => {
                    HarnessRuntimeSessionStatusV1::Failed
                }
            },
            process_id: session.process_id,
            terminal_size: session.terminal_size.map(|size| HarnessRuntimeTerminalSizeV1 {
                rows: size.rows,
                columns: size.columns,
            }),
            operation_pending: session.operation_pending,
            input_pending: session.input_pending,
        }).collect();
        let redacted = HarnessRuntimeWorkspaceV1 {
            workspace_id: workspace_id.clone(),
            display_root: workspace.canonical_root,
            display_root_truncated: workspace.canonical_root_truncated,
            sessions,
            session_count: workspace.session_count,
            sessions_truncated: workspace.sessions_truncated,
        };
        (workspace_id, redacted)
    }).collect();
    let managed_sessions = inventory.managed_sessions.into_iter().map(|record| {
        HarnessRuntimeManagedSessionV1 {
            record_id: record.record_id.as_str().to_owned(),
            display_name: record.display_name,
            display_name_truncated: record.display_name_truncated,
            provider: record.provider.as_str().to_owned(),
            mode: match record.mode {
                gate4agent_node_protocol::SessionMode::Pty => HarnessRuntimeManagedModeV1::Pty,
                gate4agent_node_protocol::SessionMode::Inline => {
                    HarnessRuntimeManagedModeV1::Inline
                }
            },
            state: match record.state {
                gate4agent_node_protocol::ManagedSessionState::IdentityPending => {
                    HarnessRuntimeManagedStateV1::IdentityPending
                }
                gate4agent_node_protocol::ManagedSessionState::Live => {
                    HarnessRuntimeManagedStateV1::Live
                }
                gate4agent_node_protocol::ManagedSessionState::Dormant => {
                    HarnessRuntimeManagedStateV1::Dormant
                }
                gate4agent_node_protocol::ManagedSessionState::Unavailable => {
                    HarnessRuntimeManagedStateV1::Unavailable
                }
            },
            workspace_id: record.workspace_id.as_str().to_owned(),
            active_binding: record.active_session.map(|address| HarnessRuntimeSessionBindingV1 {
                workspace_id: address.workspace_id.as_str().to_owned(),
                instance_id: address.session.instance_id.0,
                generation: address.session.generation.0,
            }),
            provider_identity_present: record.provider_identity_present,
            updated_at_unix_ms: record.updated_at_unix_ms,
        }
    }).collect();
    HarnessRuntimeInventoryV1 {
        enabled_providers,
        workspaces,
        workspace_count: inventory.workspace_count,
        workspaces_truncated: inventory.workspaces_truncated,
        session_count: inventory.session_count,
        sessions_truncated: inventory.sessions_truncated,
        managed_sessions,
        managed_session_count: inventory.managed_session_count,
        managed_sessions_truncated: inventory.managed_sessions_truncated,
    }
}

fn operator_credential_digest(
    credential: &HarnessOperatorCredential,
) -> Result<[u8; 32], HarnessRuntimeError> {
    local_hmac_sha256(
        OPERATOR_CREDENTIAL_DIGEST_DOMAIN,
        credential.expose().as_bytes(),
    ).map_err(|_| HarnessRuntimeError::OperatorCredentialDigest)
}

fn authorize_operator_intent(
    intent: HarnessOperatorIntentV1,
) -> Result<HarnessOperatorRequestV1, HarnessOperatorHostErrorV1> {
    intent.validate().map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?;
    let operation_digest = local_hmac_sha256(
        OPERATOR_INTENT_OPERATION_ID_DOMAIN,
        intent.request_ref.as_str().as_bytes(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let idempotency_digest = local_hmac_sha256(
        OPERATOR_INTENT_IDEMPOTENCY_REF_DOMAIN,
        intent.request_ref.as_str().as_bytes(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let task_digest = local_hmac_sha256(
        OPERATOR_INTENT_TASK_ID_DOMAIN,
        intent.request_ref.as_str().as_bytes(),
    ).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let authority = gate4agent_harness_protocol::HarnessOperatorAuthorityV1 {
        operation_id: HarnessOperationId::new(format!(
            "hop_{}",
            encode_hex(&operation_digest[..12]),
        )).map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        idempotency_ref: HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            encode_hex(&idempotency_digest[..12]),
        )).map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        actor_id: HarnessSelectorV1::new("harness-operator")
            .map_err(|_| HarnessOperatorHostErrorV1::Internal)?,
        now_unix_ms: intent.submitted_at_unix_ms,
    };
    let create_task_id = gate4agent_harness_protocol::HarnessTaskId::new(format!(
        "htask_{}",
        encode_hex(&task_digest[..12]),
    )).map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    let request = intent.authorize(authority, create_task_id);
    request.validate().map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?;
    Ok(request)
}

fn execute_operator_request(
    harness: &mut HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    launch_catalog: &HarnessLaunchCatalog,
    runtime_inventory: &HarnessRuntimeInventoryCache,
    request: HarnessOperatorRequestV1,
) -> Result<HarnessOperatorResponseV1, HarnessOperatorHostErrorV1> {
    request.validate().map_err(|_| HarnessOperatorHostErrorV1::InvalidRequest)?;
    let request = match request {
        HarnessOperatorRequestV1::SubmitIntent { intent } => authorize_operator_intent(intent)?,
        request => request,
    };
    let response = match request {
        HarnessOperatorRequestV1::MonitorGet { run_id } => {
            HarnessOperatorResponseV1::Monitor(
                execute_operator_monitor(harness, observation, support, &run_id)
                    .map_err(map_operator_read_error)?,
            )
        }
        HarnessOperatorRequestV1::TimelineRead {
            run_id,
            after_sequence,
            limit,
        } => HarnessOperatorResponseV1::Timeline(
            execute_operator_timeline(
                harness,
                observation,
                support,
                &run_id,
                after_sequence,
                limit,
            ).map_err(map_operator_read_error)?,
        ),
        HarnessOperatorRequestV1::TasksList { after_task_id, state, limit } => {
            let mut tasks = harness.engine().tasks()
                .filter(|task| {
                    after_task_id.as_ref().map_or(true, |after| &task.task_id > after)
                })
                .filter(|task| state.map_or(true, |state| task.state == state))
                .map(redact_operator_task)
                .take(usize::from(limit) + 1)
                .collect::<Vec<_>>();
            let has_more = tasks.len() > usize::from(limit);
            if has_more { tasks.pop(); }
            let next_cursor = has_more.then(|| {
                tasks.last().expect("nonzero operator page limit").task_id.clone()
            });
            HarnessOperatorResponseV1::Tasks(TaskPageV1 { tasks, next_cursor })
        }
        HarnessOperatorRequestV1::TaskGet { task_id } => {
            let task = harness.engine().task(&task_id)
                .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
            HarnessOperatorResponseV1::Task(redact_operator_task(task))
        }
        HarnessOperatorRequestV1::RunsList {
            task_id,
            after_run_id,
            lifecycle,
            limit,
        } => {
            let mut runs = harness.engine().runs()
                .filter(|run| {
                    after_run_id.as_ref().map_or(true, |after| &run.run_id > after)
                })
                .filter(|run| task_id.as_ref().map_or(true, |task_id| &run.task_id == task_id))
                .filter(|run| lifecycle.map_or(true, |lifecycle| run.lifecycle == lifecycle))
                .map(redact_operator_run)
                .take(usize::from(limit) + 1)
                .collect::<Vec<_>>();
            let has_more = runs.len() > usize::from(limit);
            if has_more { runs.pop(); }
            let next_cursor = has_more.then(|| {
                runs.last().expect("nonzero operator page limit").run_id.clone()
            });
            HarnessOperatorResponseV1::Runs(RunPageV1 { runs, next_cursor })
        }
        HarnessOperatorRequestV1::RunGet { run_id } => {
            let run = harness.engine().run(&run_id)
                .ok_or(HarnessOperatorHostErrorV1::NotFound)?;
            HarnessOperatorResponseV1::Run(redact_operator_run(run))
        }
        HarnessOperatorRequestV1::RunCorrelationGet { run_id } => {
            HarnessOperatorResponseV1::RunCorrelation(
                project_operator_run_correlation(harness, runtime_inventory, &run_id)?,
            )
        }
        HarnessOperatorRequestV1::RunTransferGet { run_id } => {
            HarnessOperatorResponseV1::RunTransfer(
                project_operator_run_transfer(harness, &run_id)?,
            )
        }
        HarnessOperatorRequestV1::LaunchPlansList { after_plan_id, limit } => {
            let mut plans = launch_catalog.ordinary_plans()
                .filter(|plan| {
                    after_plan_id.as_ref().map_or(true, |after| &plan.plan_id > after)
                })
                .take(usize::from(limit) + 1)
                .map(|plan| {
                    Ok(HarnessLaunchPlanSummaryV1 {
                        scheduled_launch: plan.ordinary_scheduled_ref()?,
                        node_id: plan.node_id.clone(),
                        workspace_id: plan.workspace_id.clone(),
                        worktree: plan.worktree.clone(),
                        provider_profile: plan.provider_profile.clone(),
                        provider_id: HarnessSelectorV1::new(plan.provider.as_str())?,
                        mode: plan.mode,
                    })
                })
                .collect::<Result<Vec<_>, HarnessServiceError>>()
                .map_err(map_operator_service_error)?;
            let has_more = plans.len() > usize::from(limit);
            if has_more { plans.pop(); }
            let next_plan_id = has_more.then(|| {
                plans.last().expect("nonzero launch plan page limit")
                    .scheduled_launch.plan.plan_id.clone()
            });
            HarnessOperatorResponseV1::LaunchPlans(HarnessLaunchPlanPageV1 {
                plans,
                next_plan_id,
            })
        }
        HarnessOperatorRequestV1::TaskExecutionSpecGet { task_id } => {
            HarnessOperatorResponseV1::TaskExecutionSpec(
                harness.task_execution_spec(&task_id).cloned(),
            )
        }
        HarnessOperatorRequestV1::RuntimeInventoryList { after_node_id, limit } => {
            HarnessOperatorResponseV1::RuntimeInventory(
                runtime_inventory.page(after_node_id.as_deref(), limit),
            )
        }
        HarnessOperatorRequestV1::CatalogNativeSessions { .. }
        | HarnessOperatorRequestV1::PageNativeSessions { .. }
        | HarnessOperatorRequestV1::PreviewNativeSession { .. }
        | HarnessOperatorRequestV1::InspectRunWorkspace { .. }
        | HarnessOperatorRequestV1::ReadRunWorkspaceFile { .. }
        | HarnessOperatorRequestV1::ReadRunGitHistory { .. }
        | HarnessOperatorRequestV1::ReadRunGitDiff { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
        HarnessOperatorRequestV1::CreateTask { request } => {
            operator_mutation_response(harness.operator_create_task(request))?
        }
        HarnessOperatorRequestV1::ReplaceTask { request } => {
            operator_mutation_response(harness.operator_replace_task(request))?
        }
        HarnessOperatorRequestV1::MoveTask { request } => {
            operator_mutation_response(harness.operator_move_task(request))?
        }
        HarnessOperatorRequestV1::CancelTask { request } => {
            operator_mutation_response(harness.operator_cancel_task(request))?
        }
        HarnessOperatorRequestV1::RetryTask { request } => {
            operator_mutation_response(harness.operator_retry_task(request))?
        }
        HarnessOperatorRequestV1::ScheduleNext { request } => {
            HarnessOperatorResponseV1::Schedule(
                harness.schedule_next(
                    launch_catalog,
                    request.authority,
                    request.plan_id.as_ref(),
                ).map_err(map_operator_service_error)?,
            )
        }
        HarnessOperatorRequestV1::ReplaceTaskExecutionSpec { request } => {
            let outcome = harness.operator_replace_task_execution_spec(
                launch_catalog,
                request,
            ).map_err(map_operator_service_error)?;
            HarnessOperatorResponseV1::ExecutionSpecMutation(match outcome {
                HarnessApplyOutcome::Applied => HarnessOperatorMutationOutcomeV1::Applied,
                HarnessApplyOutcome::Replayed => HarnessOperatorMutationOutcomeV1::Replayed,
            })
        }
        HarnessOperatorRequestV1::StartTask { request } => {
            HarnessOperatorResponseV1::TaskStarted(
                harness.start_task(launch_catalog, request)
                    .map_err(map_operator_service_error)?,
            )
        }
        HarnessOperatorRequestV1::SubmitIntent { .. } => {
            return Err(HarnessOperatorHostErrorV1::Internal);
        }
    };
    response.validate().map_err(|_| HarnessOperatorHostErrorV1::Internal)?;
    Ok(response)
}

fn operator_mutation_response(
    result: Result<HarnessApplyOutcome, HarnessServiceError>,
) -> Result<HarnessOperatorResponseV1, HarnessOperatorHostErrorV1> {
    let outcome = match result.map_err(map_operator_service_error)? {
        HarnessApplyOutcome::Applied => HarnessOperatorMutationOutcomeV1::Applied,
        HarnessApplyOutcome::Replayed => HarnessOperatorMutationOutcomeV1::Replayed,
    };
    Ok(HarnessOperatorResponseV1::Mutation(outcome))
}

fn map_operator_service_error(error: HarnessServiceError) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessServiceError::Validation(_) => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessServiceError::DispatchPolicy(_) => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessServiceError::Engine(gate4agent_harness_engine::HarnessEngineError::NotFound(_)) => {
            HarnessOperatorHostErrorV1::NotFound
        }
        HarnessServiceError::ExecutionSpecMissing => HarnessOperatorHostErrorV1::NotFound,
        HarnessServiceError::SchedulerBusy => HarnessOperatorHostErrorV1::Busy,
        HarnessServiceError::Poisoned
        | HarnessServiceError::Store(_)
        | HarnessServiceError::Json(_)
        | HarnessServiceError::Corrupt(_)
        | HarnessServiceError::MutationDigest(_) => HarnessOperatorHostErrorV1::Internal,
        _ => HarnessOperatorHostErrorV1::Conflict,
    }
}

fn map_operator_read_error(error: HarnessReadHostErrorV1) -> HarnessOperatorHostErrorV1 {
    match error {
        HarnessReadHostErrorV1::InvalidRequest => HarnessOperatorHostErrorV1::InvalidRequest,
        HarnessReadHostErrorV1::NotFoundOrDenied => HarnessOperatorHostErrorV1::NotFound,
        HarnessReadHostErrorV1::TooLarge => HarnessOperatorHostErrorV1::TooLarge,
        HarnessReadHostErrorV1::Deadline => HarnessOperatorHostErrorV1::Deadline,
        HarnessReadHostErrorV1::Unauthorized | HarnessReadHostErrorV1::Internal => {
            HarnessOperatorHostErrorV1::Internal
        }
    }
}

fn redact_operator_task(
    task: &gate4agent_harness_protocol::HarnessTaskV1,
) -> RedactedTaskV1 {
    RedactedTaskV1 {
        task_id: task.task_id.clone(),
        revision: task.revision,
        title: task.title.clone(),
        body: task.body.clone(),
        creator: match task.creator {
            HarnessActorV1::User { .. } => TaskCreatorCategoryV1::User,
            HarnessActorV1::ParentRun { .. } => TaskCreatorCategoryV1::ParentRun,
        },
        parent_task_id: task.parent_task_id.clone(),
        dependency_ids: task.dependencies.clone(),
        state: task.state,
        run_ids: task.run_ids.clone(),
        references_redacted: false,
        result_refs: task.result_refs.clone(),
        artifact_refs: task.artifact_refs.clone(),
        created_at_unix_ms: task.created_at_unix_ms,
        updated_at_unix_ms: task.updated_at_unix_ms,
    }
}

fn redact_operator_run(
    run: &gate4agent_harness_protocol::HarnessRunV1,
) -> RedactedRunV1 {
    let binding = match run.binding.as_ref().map(|binding| &binding.session) {
        None => RedactedBindingStateV1::None,
        Some(HarnessSessionIdentityV1::Managed { active_session: None, .. }) => {
            RedactedBindingStateV1::ManagedDormant
        }
        Some(HarnessSessionIdentityV1::Managed { active_session: Some(_), .. }) => {
            RedactedBindingStateV1::ManagedActive
        }
        Some(HarnessSessionIdentityV1::Inline { .. }) => RedactedBindingStateV1::Inline,
    };
    RedactedRunV1 {
        run_id: run.run_id.clone(),
        revision: run.revision,
        parent_run_id: run.parent_run_id.clone(),
        task_id: Some(run.task_id.clone()),
        operation_id: Some(run.operation_id.clone()),
        intent: RedactedRunIntentV1 {
            mode: run.intent.mode,
            worktree: match run.intent.worktree {
                HarnessWorktreeIntentV1::Existing => RedactedWorktreeIntentV1::Existing,
                HarnessWorktreeIntentV1::Managed { .. } => RedactedWorktreeIntentV1::Managed,
            },
            has_delivery_bundle: run.intent.delivery_bundle.is_some(),
            has_continuation: run.intent.continuation.is_some(),
        },
        lifecycle: run.lifecycle,
        binding,
        result_disposition: run.result_disposition,
        failure_category: run.failure.as_ref().map(|failure| failure.category),
        references_redacted: false,
        created_at_unix_ms: run.created_at_unix_ms,
        updated_at_unix_ms: run.updated_at_unix_ms,
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    commands: mpsc::Sender<HostCommand>,
    operator_authority: Option<HarnessOperatorCredentialAuthority>,
) -> Result<(), HarnessRuntimeError> {
    let mut operator_frame = false;
    let outcome = timeout(HOST_CONNECTION_DEADLINE, async {
        let frame = timeout(
            HOST_DEADLINE,
            read_single_frame_detecting_operator(&mut stream, &mut operator_frame),
        ).await.map_err(|_| HarnessRuntimeError::Deadline)??;
        operator_frame = frame_is_operator(&frame);
        if operator_frame {
            let envelope: HarnessOperatorEnvelopeV1 = match serde_json::from_slice(&frame) {
                Ok(envelope) => envelope,
                Err(_) => {
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error {
                            error: HarnessOperatorHostErrorV1::InvalidRequest,
                        },
                    ).await?;
                    return Err(HarnessRuntimeError::InvalidFrame);
                }
            };
            if envelope.validate().is_err() {
                write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::InvalidRequest,
                    },
                ).await?;
                return Err(HarnessRuntimeError::InvalidFrame);
            }
            let HarnessOperatorEnvelopeV1 {
                credential,
                request,
                ..
            } = envelope;
            let response_deadline = operator_response_deadline(&request);
            let authorized = match operator_authority.as_ref() {
                Some(authority) => authority.verify(&credential)?,
                None => false,
            };
            drop(credential);
            if !authorized {
                write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::Unauthorized,
                    },
                ).await?;
                return Ok(());
            }
            let (reply, receive) = oneshot::channel();
            commands.send(HostCommand::Operator {
                request,
                reply,
            }).await.map_err(|_| HarnessRuntimeError::HostStopped)?;
            let reply = match timeout(response_deadline, receive).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) => return Err(HarnessRuntimeError::HostStopped),
                Err(_) => {
                    write_operator_reply(
                        &mut stream,
                        HarnessOperatorReplyV1::Error {
                            error: HarnessOperatorHostErrorV1::Deadline,
                        },
                    ).await?;
                    return Err(HarnessRuntimeError::Deadline);
                }
            };
            return match write_operator_reply(&mut stream, reply).await {
                Err(HarnessRuntimeError::ResponseTooLarge) => write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::TooLarge,
                    },
                ).await,
                result => result,
            };
        }
        let envelope: HarnessReadEnvelopeV1 = serde_json::from_slice(&frame)
            .map_err(|_| HarnessRuntimeError::InvalidFrame)?;
        envelope.validate().map_err(|_| HarnessRuntimeError::InvalidFrame)?;
        let (reply, receive) = oneshot::channel();
        commands.send(HostCommand::Read { envelope, reply }).await
            .map_err(|_| HarnessRuntimeError::HostStopped)?;
        let reply = match timeout(HOST_DEADLINE, receive).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => return Err(HarnessRuntimeError::HostStopped),
            Err(_) => {
                write_reply(
                    &mut stream,
                    HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::Deadline },
                ).await?;
                return Err(HarnessRuntimeError::Deadline);
            }
        };
        match write_reply(&mut stream, reply).await {
            Err(HarnessRuntimeError::ResponseTooLarge) => write_reply(
                &mut stream,
                HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::TooLarge },
            ).await,
            result => result,
        }
    }).await;
    match outcome {
        Ok(result) => result,
        Err(_) => {
            if operator_frame {
                let _ = write_operator_reply(
                    &mut stream,
                    HarnessOperatorReplyV1::Error {
                        error: HarnessOperatorHostErrorV1::Deadline,
                    },
                ).await;
            } else {
                let _ = write_reply(
                    &mut stream,
                    HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::Deadline },
                ).await;
            }
            Err(HarnessRuntimeError::Deadline)
        }
    }
}

fn frame_is_operator(frame: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(frame).ok()
        .and_then(|value| value.get("credential")?.as_str().map(str::to_owned))
        .is_some_and(|credential| credential.starts_with("g4aho_"))
}

#[cfg(test)]
async fn read_single_frame(stream: &mut TcpStream) -> Result<Vec<u8>, HarnessRuntimeError> {
    let mut operator_frame = false;
    read_single_frame_detecting_operator(stream, &mut operator_frame).await
}

async fn read_single_frame_detecting_operator(
    stream: &mut TcpStream,
    operator_frame: &mut bool,
) -> Result<Vec<u8>, HarnessRuntimeError> {
    let mut bytes = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.map_err(|_| HarnessRuntimeError::ReadFailed)?;
        if read == 0 { break; }
        if bytes.len().saturating_add(read) > HARNESS_READ_REQUEST_MAX_BYTES {
            return Err(HarnessRuntimeError::RequestTooLarge);
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(b"g4aho_".len()).any(|window| window == b"g4aho_") {
            *operator_frame = true;
        }
    }
    if bytes.len() < 2 || bytes.last() != Some(&b'\n')
        || bytes[..bytes.len() - 1].contains(&b'\n')
    {
        return Err(HarnessRuntimeError::InvalidFrame);
    }
    bytes.pop();
    Ok(bytes)
}

async fn write_reply(
    stream: &mut TcpStream,
    reply: HarnessReadReplyV1,
) -> Result<(), HarnessRuntimeError> {
    reply.validate().map_err(|_| HarnessRuntimeError::InvalidReply)?;
    let mut encoded = serde_json::to_vec(&reply).map_err(|_| HarnessRuntimeError::InvalidReply)?;
    if encoded.len().saturating_add(1) > HARNESS_READ_RESPONSE_MAX_BYTES {
        return Err(HarnessRuntimeError::ResponseTooLarge);
    }
    encoded.push(b'\n');
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.shutdown().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

async fn write_operator_reply(
    stream: &mut TcpStream,
    reply: HarnessOperatorReplyV1,
) -> Result<(), HarnessRuntimeError> {
    reply.validate().map_err(|_| HarnessRuntimeError::InvalidReply)?;
    let mut encoded = serde_json::to_vec(&reply).map_err(|_| HarnessRuntimeError::InvalidReply)?;
    if encoded.len().saturating_add(1) > HARNESS_OPERATOR_RESPONSE_MAX_BYTES {
        return Err(HarnessRuntimeError::ResponseTooLarge);
    }
    encoded.push(b'\n');
    stream.write_all(&encoded).await.map_err(|_| HarnessRuntimeError::WriteFailed)?;
    stream.shutdown().await.map_err(|_| HarnessRuntimeError::WriteFailed)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ObservationSupportRegistry {
    routes: BTreeMap<(NodeId, NodeIncarnationId), RouteObservationAuthority>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RouteObservationAuthority {
    support: Option<C2ObservationSupport>,
    healthy: bool,
}

impl ObservationSupportRegistry {
    pub(crate) fn get(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> Option<Option<C2ObservationSupport>> {
        self.routes.get(&(node_id.clone(), incarnation_id)).map(|route| route.support)
    }

    pub(crate) fn is_authoritative(
        &self,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> bool {
        self.routes.get(&(node_id.clone(), incarnation_id))
            .is_some_and(|route| route.healthy)
    }

    fn replace(
        &mut self,
        node_id: NodeId,
        incarnation_id: NodeIncarnationId,
        support: Option<C2ObservationSupport>,
    ) {
        self.routes.insert(
            (node_id, incarnation_id),
            RouteObservationAuthority { support, healthy: true },
        );
    }

    fn mark_unhealthy(&mut self, node_id: &NodeId, incarnation_id: NodeIncarnationId) {
        self.routes.entry((node_id.clone(), incarnation_id))
            .and_modify(|route| route.healthy = false)
            .or_insert(RouteObservationAuthority { support: None, healthy: false });
    }

    fn mark_all_unhealthy(&mut self) {
        for route in self.routes.values_mut() {
            route.healthy = false;
        }
    }

    fn reconcile_current_routes(&mut self, routes: &[NodeRoute]) {
        for ((node_id, incarnation_id), authority) in self.routes.iter_mut() {
            if !routes.iter().any(|route| {
                &route.node_id == node_id
                    && route.expected_incarnation_id == *incarnation_id
            }) {
                authority.healthy = false;
            }
        }
    }
}

pub async fn run_observation_bridge(
    adapter: HarnessC2Adapter,
    mut events: HarnessC2EventReceiver,
    mut observation: ObservationService,
) -> Result<(), HarnessRuntimeError> {
    let mut support = ObservationSupportRegistry::default();
    recover_all_observation_routes(&adapter, &mut observation, &mut support).await?;
    while let Some(event) = events.recv().await {
        apply_live_event(&adapter, &mut observation, &mut support, event).await?;
    }
    observation.flush()?;
    Ok(())
}

pub(crate) async fn recover_all_routes(
    adapter: &HarnessC2Adapter,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
) -> Result<(), HarnessRuntimeError> {
    for route in adapter.observation_routes() {
        let requested_after = durable_cursor_for(observation, &route).unwrap_or(0);
        if let Err(error) = recover_route(
            adapter,
            harness,
            observation,
            support,
            runtime_inventory,
            &route,
            requested_after,
        ).await {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            if !matches!(error, HarnessRuntimeError::C2(_)) {
                return Err(error);
            }
        }
    }
    Ok(())
}

async fn recover_all_observation_routes(
    adapter: &HarnessC2Adapter,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
) -> Result<(), HarnessRuntimeError> {
    for route in adapter.observation_routes() {
        let requested_after = durable_cursor_for(observation, &route).unwrap_or(0);
        if let Err(error) = recover_observation_route(
            adapter,
            observation,
            support,
            &route,
            requested_after,
        ).await {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            if !matches!(error, HarnessRuntimeError::C2(_)) {
                return Err(error);
            }
        }
    }
    Ok(())
}

pub(crate) async fn apply_live_event(
    adapter: &HarnessC2Adapter,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    routed: RoutedNodeEvent,
) -> Result<(), HarnessRuntimeError> {
    let route = NodeRoute {
        node_id: routed.node_id.clone(),
        expected_incarnation_id: routed.cursor.incarnation_id,
    };
    let prior = durable_cursor_for(observation, &route);
    if prior.is_none() || routed.cursor.sequence > prior.unwrap_or(0).saturating_add(1) {
        recover_observation_route(
            adapter,
            observation,
            support,
            &route,
            prior.unwrap_or(0),
        ).await?;
    }
    if matches!(routed.event, C2NodeEvent::ResyncRequired { .. }) {
        let current = durable_cursor_for(observation, &route).unwrap_or(0);
        recover_observation_route(adapter, observation, support, &route, current).await?;
    }
    if durable_cursor_for(observation, &route).is_some_and(|sequence| {
        sequence >= routed.cursor.sequence
    }) {
        return Ok(());
    }
    let envelope = routed_event_to_ingress(routed, unix_time_ms())?;
    observation.apply_ingress(envelope)?;
    Ok(())
}

fn apply_or_buffer_host_live_event(
    adapter: &HarnessC2Adapter,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    recovery: &mut ObservationRecoveryRegistry,
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
    routed: RoutedNodeEvent,
) -> Result<(), HarnessRuntimeError> {
    let route = NodeRoute {
        node_id: routed.node_id.clone(),
        expected_incarnation_id: routed.cursor.incarnation_id,
    };
    let current_route = match adapter.exact_route(&route.node_id) {
        Ok(current_route) => current_route,
        Err(error) => {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            recovery.remove(&route);
            return Err(error.into());
        }
    };
    if current_route != route {
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        recovery.remove(&route);
        return Ok(());
    }
    let prior = durable_cursor_for(observation, &route);
    if prior.is_some_and(|sequence| sequence >= routed.cursor.sequence) {
        return Ok(());
    }
    let inventory_refresh_required = invalidate_runtime_inventory_for_event(
        runtime_inventory,
        recovery,
        &route,
        &routed.event,
    );
    let recovery_required = inventory_refresh_required
        || prior.is_none()
        || routed.cursor.sequence > prior.unwrap_or(0).saturating_add(1)
        || matches!(routed.event, C2NodeEvent::ResyncRequired { .. });
    if recovery_required {
        freeze_bound_route_waiting(
            harness,
            &route,
            routed.cursor.sequence,
            unix_time_ms(),
        )?;
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
    }
    let already_recovering = recovery.contains(&route);
    if recovery_required || already_recovering {
        let is_resync_required = matches!(routed.event, C2NodeEvent::ResyncRequired { .. });
        let route_recovery = recovery.ensure_route(route);
        if is_resync_required && already_recovering {
            route_recovery.refresh_after_completion = true;
        }
        route_recovery.buffer(routed);
        return Ok(());
    }
    let received_at = unix_time_ms();
    apply_exact_control_lifecycle(harness, &routed, received_at)?;
    if durable_cursor_for(observation, &route).is_some_and(|sequence| {
        sequence >= routed.cursor.sequence
    }) {
        return Ok(());
    }
    observation.apply_ingress(routed_event_to_ingress(routed, received_at)?)?;
    Ok(())
}

fn event_affects_runtime_inventory(event: &C2NodeEvent) -> bool {
    matches!(
        event,
        C2NodeEvent::Control { .. }
            | C2NodeEvent::WorkspaceAdded { .. }
            | C2NodeEvent::WorkspaceRemoved { .. }
            | C2NodeEvent::SessionRecordUpserted { .. }
            | C2NodeEvent::SessionRecordRemoved { .. }
            | C2NodeEvent::ManagedWorktreeUpserted { .. }
            | C2NodeEvent::ManagedWorktreeRemoved { .. }
            | C2NodeEvent::ResyncRequired { .. }
    )
}

fn invalidate_runtime_inventory_for_event(
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
    recovery: &mut ObservationRecoveryRegistry,
    route: &NodeRoute,
    event: &C2NodeEvent,
) -> bool {
    if !event_affects_runtime_inventory(event) {
        return false;
    }
    runtime_inventory.invalidate(route);
    let route_recovery = recovery.ensure_route(route.clone());
    if route_recovery.attempt.is_some() {
        route_recovery.refresh_after_completion = true;
    }
    true
}

fn start_pending_observation_recoveries(
    adapter: &HarnessC2Adapter,
    commands: &mpsc::Sender<HostCommand>,
    observation: &ObservationService,
    recovery: &mut ObservationRecoveryRegistry,
) {
    let available = OBSERVATION_RECOVERY_MAX_IN_FLIGHT
        .saturating_sub(recovery.in_flight());
    if available == 0 {
        return;
    }
    let now = Instant::now();
    let routes = recovery.routes.values()
        .filter(|route| route.attempt.is_none() && route.retry_after <= now)
        .take(available)
        .map(|route| route.route.clone())
        .collect::<Vec<_>>();
    for route in routes {
        let requested_after = durable_cursor_for(observation, &route).unwrap_or(0);
        let attempt_id = recovery.allocate_attempt_id();
        let route_recovery = recovery.routes.get_mut(
            &ObservationRecoveryRegistry::key(&route),
        ).expect("selected recovery route must remain registered");
        route_recovery.attempt = Some(ObservationRecoveryAttempt {
            attempt_id,
            requested_after,
        });
        let worker_adapter = adapter.clone();
        let worker_commands = commands.clone();
        tokio::spawn(async move {
            let result = worker_adapter.observation_resync(&route, requested_after).await;
            let _ = worker_commands.send(HostCommand::ObservationRecoveryFinished {
                route,
                attempt_id,
                requested_after,
                result,
            }).await;
        });
    }
}

fn finish_observation_recovery(
    recovery: &mut ObservationRecoveryRegistry,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
    route: NodeRoute,
    attempt_id: u64,
    requested_after: u64,
    result: Result<HarnessObservationResync, HarnessC2Error>,
) -> Result<(), HarnessRuntimeError> {
    let key = ObservationRecoveryRegistry::key(&route);
    let Some(route_recovery) = recovery.routes.get_mut(&key) else {
        return Ok(());
    };
    if !route_recovery.accepts_completion(&route, attempt_id, requested_after) {
        return Ok(());
    }
    route_recovery.attempt = None;
    let resync = match result {
        Ok(resync) => resync,
        Err(_) => {
            route_recovery.retry_after = Instant::now() + OBSERVATION_RECOVERY_RETRY;
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            return Ok(());
        }
    };
    if resync.route() != &route
        || resync.requested_after_sequence() != requested_after
    {
        route_recovery.retry_after = Instant::now() + OBSERVATION_RECOVERY_RETRY;
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Ok(());
    }

    let received_at = unix_time_ms();
    apply_resync_lifecycle(harness, &resync, received_at)?;
    commit_observation_resync(observation, support, &resync, received_at)?;
    runtime_inventory.refresh(&resync, received_at);

    let requires_follow_up = route_recovery.overflowed
        || route_recovery.refresh_after_completion;
    if requires_follow_up {
        route_recovery.prepare_follow_up();
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Ok(());
    }

    let buffered = std::mem::take(&mut route_recovery.buffered);
    route_recovery.buffered_bytes = 0;
    let mut follow_up = false;
    for (_, routed) in buffered {
        let prior = durable_cursor_for(observation, &route).unwrap_or(0);
        if prior >= routed.cursor.sequence {
            continue;
        }
        if routed.cursor.sequence > prior.saturating_add(1) {
            follow_up = true;
            break;
        }
        apply_exact_control_lifecycle(harness, &routed, received_at)?;
        observation.apply_ingress(routed_event_to_ingress(routed, received_at)?)?;
    }
    if follow_up {
        route_recovery.prepare_follow_up();
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
    } else {
        recovery.routes.remove(&key);
    }
    Ok(())
}

struct HarnessMcpRelayPlan {
    route: NodeRoute,
    reservation_id: gate4agent_node_protocol::HarnessMcpReservationId,
    activation_digest: gate4agent_node_protocol::HarnessMcpActivationDigest,
    record_id: gate4agent_node_protocol::SessionRecordId,
    session: gate4agent_node_protocol::SessionAddress,
    call_id: gate4agent_node_protocol::HarnessMcpCallId,
    deadline_unix_ms: u64,
    outcome: Result<Vec<u8>, HarnessMcpRejectReasonV1>,
}

fn prepare_harness_mcp_read_call(
    adapter: &HarnessC2Adapter,
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    routed: RoutedNodeEvent,
) -> Result<HarnessMcpRelayPlan, HarnessRuntimeError> {
    let route = NodeRoute {
        node_id: routed.node_id.clone(),
        expected_incarnation_id: routed.cursor.incarnation_id,
    };
    let C2NodeEvent::HarnessMcpReadCall {
        reservation_id,
        activation_digest,
        record_id,
        session,
        call_id,
        request,
        deadline_unix_ms,
    } = routed.event else {
        return Err(HarnessRuntimeError::InvalidHarnessMcpEvent);
    };
    let now = unix_time_ms();
    let current_route = adapter.exact_route(&route.node_id)?;
    let authorization = if current_route != route || now >= deadline_unix_ms {
        Err(HarnessReadHostErrorV1::Unauthorized)
    } else {
        harness.authorize_harness_mcp_call(
            &route,
            &reservation_id,
            &activation_digest,
            &record_id,
            &session,
        ).map_err(|_| HarnessReadHostErrorV1::Unauthorized)
    };
    let response = authorization.and_then(|binding| {
        verify_observation_credential_binding(observation, support, &binding)?;
        execute_exact_binding_read(harness, observation, support, &binding, request)
    });
    let outcome = match response {
        Ok(response) => {
            let reply = HarnessMcpLocalReplyV1::Ok { response };
            if reply.validate().is_err() {
                Err(HarnessMcpRejectReasonV1::ResponseTooLarge)
            } else {
                let encoded = serde_json::to_vec(&reply)
                    .map_err(|_| HarnessRuntimeError::InvalidReply)?;
                if encoded.len() > MAX_HARNESS_MCP_AGGREGATE_REPLY_BYTES {
                    Err(HarnessMcpRejectReasonV1::ResponseTooLarge)
                } else {
                    Ok(encoded)
                }
            }
        }
        Err(error) => Err(reject_reason(error)),
    };
    Ok(HarnessMcpRelayPlan {
        route,
        reservation_id,
        activation_digest,
        record_id,
        session,
        call_id,
        deadline_unix_ms,
        outcome,
    })
}

async fn relay_harness_mcp_read_call(
    adapter: &HarnessC2Adapter,
    plan: HarnessMcpRelayPlan,
) -> Result<(), HarnessRuntimeError> {
    let HarnessMcpRelayPlan {
        route,
        reservation_id,
        activation_digest,
        record_id,
        session,
        call_id,
        deadline_unix_ms,
        outcome,
    } = plan;
    let encoded = match outcome {
        Ok(encoded) => encoded,
        Err(reason) => {
            reject_harness_mcp_before_deadline(
                adapter, &route, &reservation_id, &activation_digest, &record_id,
                &session, &call_id, reason, deadline_unix_ms,
            ).await;
            return Ok(());
        }
    };
    let chunks = encoded.chunks(MAX_HARNESS_MCP_REPLY_CHUNK_RAW_BYTES)
        .collect::<Vec<_>>();
    let mut offset = 0u32;
    for (index, chunk) in chunks.iter().enumerate() {
        let now = unix_time_ms();
        if now >= deadline_unix_ms {
            return Ok(());
        }
        let chunk_hex = HarnessMcpReplyChunkHexV1::new(encode_hex(chunk))
            .map_err(|_| HarnessRuntimeError::InvalidReply)?;
        let remaining = deadline_unix_ms - now;
        offset = match timeout(
            Duration::from_millis(remaining),
            adapter.put_harness_mcp_reply_chunk(
                &route,
                &reservation_id,
                &activation_digest,
                &record_id,
                &session,
                &call_id,
                offset,
                index + 1 == chunks.len(),
                chunk_hex,
            ),
        ).await {
            Ok(result) => result?,
            Err(_) => return Ok(()),
        };
    }
    Ok(())
}

async fn reject_harness_mcp_before_deadline(
    adapter: &HarnessC2Adapter,
    route: &NodeRoute,
    reservation_id: &gate4agent_node_protocol::HarnessMcpReservationId,
    activation_digest: &gate4agent_node_protocol::HarnessMcpActivationDigest,
    record_id: &gate4agent_node_protocol::SessionRecordId,
    session: &gate4agent_node_protocol::SessionAddress,
    call_id: &gate4agent_node_protocol::HarnessMcpCallId,
    reason: HarnessMcpRejectReasonV1,
    deadline_unix_ms: u64,
) {
    let now = unix_time_ms();
    if now >= deadline_unix_ms { return; }
    let _ = timeout(
        Duration::from_millis(deadline_unix_ms - now),
        adapter.reject_harness_mcp_call(
            route,
            reservation_id,
            activation_digest,
            record_id,
            session,
            call_id,
            reason,
        ),
    ).await;
}

fn reject_reason(error: HarnessReadHostErrorV1) -> HarnessMcpRejectReasonV1 {
    match error {
        HarnessReadHostErrorV1::Unauthorized => HarnessMcpRejectReasonV1::Unauthorized,
        HarnessReadHostErrorV1::InvalidRequest => HarnessMcpRejectReasonV1::InvalidRequest,
        HarnessReadHostErrorV1::NotFoundOrDenied => HarnessMcpRejectReasonV1::NotFoundOrDenied,
        HarnessReadHostErrorV1::TooLarge => HarnessMcpRejectReasonV1::ResponseTooLarge,
        HarnessReadHostErrorV1::Deadline => HarnessMcpRejectReasonV1::Deadline,
        HarnessReadHostErrorV1::Internal => HarnessMcpRejectReasonV1::Internal,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

async fn recover_route(
    adapter: &HarnessC2Adapter,
    harness: &mut HarnessService,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    runtime_inventory: &mut HarnessRuntimeInventoryCache,
    route: &NodeRoute,
    requested_after: u64,
) -> Result<(), HarnessRuntimeError> {
    let resync = match adapter.observation_resync(route, requested_after).await {
        Ok(resync) => resync,
        Err(error) => {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            return Err(error.into());
        }
    };
    let received_at_ms = unix_time_ms();
    if let Err(error) = apply_resync_lifecycle(harness, &resync, received_at_ms) {
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Err(error);
    }
    commit_observation_resync(observation, support, &resync, received_at_ms)?;
    runtime_inventory.refresh(&resync, received_at_ms);
    Ok(())
}

async fn recover_observation_route(
    adapter: &HarnessC2Adapter,
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    route: &NodeRoute,
    requested_after: u64,
) -> Result<(), HarnessRuntimeError> {
    let resync = match adapter.observation_resync(route, requested_after).await {
        Ok(resync) => resync,
        Err(error) => {
            support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
            return Err(error.into());
        }
    };
    commit_observation_resync(observation, support, &resync, unix_time_ms())
}

fn apply_resync_lifecycle(
    harness: &mut HarnessService,
    resync: &HarnessObservationResync,
    received_at_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    let eviction_gap_sequence = resync.has_eviction_gap()
        .then_some(resync.oldest_available_sequence() - 1);
    apply_replayed_lifecycle_events(
        harness,
        resync.route(),
        eviction_gap_sequence,
        resync.lifecycle_control_events(),
        received_at_ms,
    )?;
    apply_snapshot_lifecycle(
        harness,
        resync.route(),
        resync.event_sequence(),
        resync.snapshot(),
        resync.lifecycle_control_events(),
        received_at_ms,
    )
}

fn apply_snapshot_lifecycle(
    harness: &mut HarnessService,
    route: &NodeRoute,
    event_sequence: u64,
    snapshot: &gate4agent_c2_protocol::C2NodeSnapshot,
    lifecycle_control_events: &[gate4agent_c2_protocol::C2NodeEventEnvelope],
    received_at_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    if event_sequence == 0 {
        return Ok(());
    }
    let matches = harness.engine().runs().filter_map(|run| {
        if run.lifecycle != HarnessRunLifecycleV1::Waiting
            || replay_contains_exact_lifecycle(run, route, lifecycle_control_events)
        {
            return None;
        }
        exact_snapshot_lifecycle(run, route, snapshot).map(|(kind, projection)| {
            (run.run_id.clone(), kind, projection)
        })
    }).collect::<Vec<_>>();
    for (run_id, kind, projection) in matches {
        commit_lifecycle_projection(
            harness,
            &run_id,
            &route.node_id,
            route.expected_incarnation_id,
            event_sequence,
            kind,
            projection,
            received_at_ms,
        )?;
    }
    Ok(())
}

fn replay_contains_exact_lifecycle(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    route: &NodeRoute,
    events: &[gate4agent_c2_protocol::C2NodeEventEnvelope],
) -> bool {
    events.iter().any(|event| {
        exact_bound_control_lifecycle(
            run,
            &RoutedNodeEvent {
                node_id: route.node_id.clone(),
                cursor: NodeCursor {
                    incarnation_id: route.expected_incarnation_id,
                    sequence: event.sequence,
                },
                event: event.event.clone(),
            },
        ).is_some()
    })
}

fn exact_snapshot_lifecycle(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    route: &NodeRoute,
    snapshot: &gate4agent_c2_protocol::C2NodeSnapshot,
) -> Option<(HarnessLifecycleEventKindV1, HarnessLifecycleProjectionV1)> {
    let binding = run.binding.as_ref()?;
    let HarnessSessionIdentityV1::Managed { record_id, active_session: Some(active) } =
        &binding.session
    else {
        return None;
    };
    if binding.node_id.as_str() != route.node_id.as_str()
        || binding.node_incarnation.as_str() != route.expected_incarnation_id.to_string()
        || snapshot.node_id != route.node_id
    {
        return None;
    }
    let mut records = snapshot.session_records.iter().filter(|record| {
        record.record_id.as_str() == record_id.as_str()
            && record.workspace_id.as_str() == binding.workspace_id.as_str()
    });
    let record = records.next()?;
    if records.next().is_some()
        || record.state != gate4agent_node_protocol::ManagedSessionState::Live
        || !snapshot_record_matches_binding(record, binding, active)
    {
        return None;
    }
    let mut sessions = snapshot.workspaces.iter()
        .filter(|workspace| workspace.workspace_id.as_str() == binding.workspace_id.as_str())
        .flat_map(|workspace| workspace.sessions.iter())
        .filter(|session| {
            session.instance_id.0 == active.instance_id
                && session.generation.0 == active.generation
        });
    let status = &sessions.next()?.status;
    if sessions.next().is_some() {
        return None;
    }
    match status {
        C2SessionStatus::Running => Some((
            HarnessLifecycleEventKindV1::Running,
            HarnessLifecycleProjectionV1::Running,
        )),
        C2SessionStatus::Failed => Some((
            HarnessLifecycleEventKindV1::Failed,
            HarnessLifecycleProjectionV1::Failed,
        )),
        C2SessionStatus::Registered
        | C2SessionStatus::Starting
        | C2SessionStatus::Stopping
        | C2SessionStatus::Exited { .. } => None,
    }
}

fn snapshot_record_matches_binding(
    record: &C2ManagedSessionRecord,
    binding: &HarnessSessionBindingV1,
    active: &HarnessRuntimeIdentityV1,
) -> bool {
    record.active_session.as_ref().is_some_and(|address| {
        address.workspace_id.as_str() == binding.workspace_id.as_str()
            && address.session.instance_id.0 == active.instance_id
            && address.session.generation.0 == active.generation
    })
}

fn apply_replayed_lifecycle_events(
    harness: &mut HarnessService,
    route: &NodeRoute,
    eviction_gap_sequence: Option<u64>,
    events: &[gate4agent_c2_protocol::C2NodeEventEnvelope],
    received_at_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    if let Some(gap_sequence) = eviction_gap_sequence {
        freeze_bound_route_waiting(harness, route, gap_sequence, received_at_ms)?;
    }
    for event in events {
        apply_exact_control_lifecycle(
            harness,
            &RoutedNodeEvent {
                node_id: route.node_id.clone(),
                cursor: NodeCursor {
                    incarnation_id: route.expected_incarnation_id,
                    sequence: event.sequence,
                },
                event: event.event.clone(),
            },
            received_at_ms,
        )?;
    }
    Ok(())
}

fn commit_observation_resync(
    observation: &mut ObservationService,
    support: &mut ObservationSupportRegistry,
    resync: &HarnessObservationResync,
    received_at_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    let route = resync.route();
    let batch = observation_resync_batch(resync, received_at_ms)?;
    if let Err(error) = observation.apply_resync(batch) {
        support.mark_unhealthy(&route.node_id, route.expected_incarnation_id);
        return Err(error.into());
    }
    support.replace(
        route.node_id.clone(),
        route.expected_incarnation_id,
        resync.observation_support(),
    );
    Ok(())
}

fn observation_resync_batch(
    resync: &HarnessObservationResync,
    received_at_ms: u64,
) -> Result<ObservationResyncBatch, HarnessRuntimeError> {
    if received_at_ms == 0 {
        return Err(HarnessRuntimeError::ZeroReceiveTime);
    }
    let route = resync.route();
    let support = resync.observation_support();
    let records_complete = support.is_some_and(|support| {
        support.events && support.managed_target
    });
    let records = if records_complete {
        resync.managed_inventory().iter().map(|record| {
            ManagedRecordLink {
                managed: ManagedSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    record_id: record.record_id.clone(),
                },
                runtime: record.active_session.as_ref().map(|address| RuntimeSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    workspace_id: address.workspace_id.clone(),
                    instance_id: address.session.instance_id,
                    generation: address.session.generation,
                }),
            }
        }).collect()
    } else {
        Vec::new()
    };
    let gaps = resync.has_eviction_gap().then(|| ObservationGap {
        first_sequence: resync.requested_after_sequence().saturating_add(1),
        last_sequence: resync.oldest_available_sequence() - 1,
    }).into_iter().collect();
    let events = resync.observation_events().iter().cloned().map(|event| {
        routed_event_to_ingress(RoutedNodeEvent {
            node_id: route.node_id.clone(),
            cursor: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence: event.sequence,
            },
            event: event.event,
        }, received_at_ms)
    }).collect::<Result<Vec<_>, _>>()?;
    Ok(ObservationResyncBatch {
        node_id: route.node_id.clone(),
        incarnation_id: route.expected_incarnation_id,
        requested_after: resync.requested_after_sequence(),
        high_watermark: NodeCursor {
            incarnation_id: route.expected_incarnation_id,
            sequence: resync.event_sequence(),
        },
        oldest_available_sequence: resync.oldest_available_sequence(),
        records,
        records_complete,
        gaps,
        events,
    })
}

fn durable_cursor_for(observation: &ObservationService, route: &NodeRoute) -> Option<u64> {
    observation.durable_resume_cursors().into_iter().find_map(|(node_id, cursor)| {
        (node_id == route.node_id && cursor.incarnation_id == route.expected_incarnation_id)
            .then_some(cursor.sequence)
    })
}

fn ensure_current_topology_binding(
    adapter: &HarnessC2Adapter,
    binding: &CredentialBindingV1,
) -> Result<(), HarnessRuntimeError> {
    let node_id = gate4agent_node_protocol::NodeId::new(binding.node_id.as_str())
        .map_err(|_| HarnessRuntimeError::CredentialBinding)?;
    let route = adapter.exact_route(&node_id)
        .map_err(|_| HarnessRuntimeError::CredentialBinding)?;
    if !topology_binding_matches_route(binding, &route) {
        return Err(HarnessRuntimeError::CredentialBinding);
    }
    Ok(())
}

fn topology_binding_matches_route(
    binding: &CredentialBindingV1,
    route: &NodeRoute,
) -> bool {
    binding.node_id.as_str() == route.node_id.as_str()
        && binding.node_incarnation.as_str().parse::<NodeIncarnationId>().ok()
            == Some(route.expected_incarnation_id)
}

pub fn apply_routed_observation_event(
    observation: &mut ObservationService,
    routed: RoutedNodeEvent,
    received_at_ms: u64,
) -> Result<(), HarnessRuntimeError> {
    observation.apply_ingress(routed_event_to_ingress(routed, received_at_ms)?)?;
    Ok(())
}

fn routed_event_to_ingress(
    routed: RoutedNodeEvent,
    received_at_ms: u64,
) -> Result<ObservationIngressEnvelope, HarnessRuntimeError> {
    if received_at_ms == 0 {
        return Err(HarnessRuntimeError::ZeroReceiveTime);
    }
    let RoutedNodeEvent { node_id, cursor, event } = routed;
    let payload = match event {
        C2NodeEvent::Observation { address, observation } => ObservationIngressPayload::Observation {
            address: ObservationTarget::Runtime { key: RuntimeSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                workspace_id: address.workspace_id,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
            } },
            observation,
        },
        C2NodeEvent::ManagedObservation { record_id, observation } => {
            ObservationIngressPayload::Observation {
                address: ObservationTarget::Managed { key: ManagedSessionKey {
                    node_id: node_id.clone(),
                    incarnation_id: cursor.incarnation_id,
                    record_id,
                } },
                observation,
            }
        }
        C2NodeEvent::HarnessMcpReadCall { .. } => {
            return Err(HarnessRuntimeError::InvalidHarnessMcpEvent);
        }
        C2NodeEvent::SessionRecordUpserted { record } => {
            let runtime = record.active_session.map(|address| RuntimeSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                workspace_id: address.workspace_id,
                instance_id: address.session.instance_id,
                generation: address.session.generation,
            });
            ObservationIngressPayload::ManagedRecordUpserted { link: ManagedRecordLink {
                managed: ManagedSessionKey {
                    node_id: node_id.clone(),
                    incarnation_id: cursor.incarnation_id,
                    record_id: record.record_id,
                },
                runtime,
            } }
        }
        C2NodeEvent::SessionRecordRemoved { record_id } => {
            ObservationIngressPayload::ManagedRecordRemoved { key: ManagedSessionKey {
                node_id: node_id.clone(),
                incarnation_id: cursor.incarnation_id,
                record_id,
            } }
        }
        C2NodeEvent::ResyncRequired { oldest_available_sequence } => {
            ObservationIngressPayload::ResyncRequired { oldest: NodeCursor {
                incarnation_id: cursor.incarnation_id,
                sequence: oldest_available_sequence,
            } }
        }
        C2NodeEvent::Control { .. }
        | C2NodeEvent::TerminalFrame { .. }
        | C2NodeEvent::ControllerChanged { .. }
        | C2NodeEvent::WorkspaceAdded { .. }
        | C2NodeEvent::WorkspaceRemoved { .. }
        | C2NodeEvent::ManagedWorktreeUpserted { .. }
        | C2NodeEvent::ManagedWorktreeRemoved { .. } => ObservationIngressPayload::CursorOnly,
    };
    Ok(ObservationIngressEnvelope {
        node_id,
        cursor,
        received_at_ms,
        transport: ObservationTransport::C2,
        payload,
    })
}

fn unix_time_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(1).max(1)
}

#[derive(Debug, Error)]
pub enum HarnessRuntimeError {
    #[error("observation receive timestamp must be nonzero")]
    ZeroReceiveTime,
    #[error(transparent)]
    C2(#[from] HarnessC2Error),
    #[error(transparent)]
    Observation(#[from] ObservationServiceError),
    #[error(transparent)]
    Harness(#[from] HarnessServiceError),
    #[error("Harness launch catalog is invalid")]
    LaunchCatalog,
    #[error("Harness dispatch preparation failed")]
    DispatchPreparation,
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error("read host must bind exact IPv4 loopback")]
    NonLoopbackBind,
    #[error("read host bind failed")]
    BindFailed,
    #[error("read host accept failed")]
    AcceptFailed,
    #[error("read host stopped")]
    HostStopped,
    #[error("read host request frame is invalid")]
    InvalidFrame,
    #[error("read host request exceeds its bound")]
    RequestTooLarge,
    #[error("read host response exceeds its bound")]
    ResponseTooLarge,
    #[error("read host response is invalid")]
    InvalidReply,
    #[error("read host read failed")]
    ReadFailed,
    #[error("read host write failed")]
    WriteFailed,
    #[error("read host request deadline elapsed")]
    Deadline,
    #[error("credential binding is not current in observation authority")]
    CredentialBinding,
    #[error("operator credential digest failed")]
    OperatorCredentialDigest,
    #[error("durable services could not be flushed")]
    FlushFailed,
    #[error("transient harness MCP event reached observation persistence")]
    InvalidHarnessMcpEvent,
    #[error("harness MCP durable authority is unavailable")]
    HarnessMcpAuthority,
    #[error("bounded harness MCP capacity-rejection queue is full or closed")]
    HarnessMcpRejectQueueFull,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_c2_protocol::{
        C2ControlEvent, C2ControlEventKind, C2ManagedSessionRecord,
        C2NodeEventEnvelope, C2NodeSnapshot, C2ObservationSupport,
        C2SessionSnapshot, C2SessionStatus, C2WorkspaceSnapshot,
    };
    use gate4agent_node_protocol::{
        ManagedSessionState, NodeCursor, OpaqueHostPath, ProviderRuntimeStatuses,
        SessionMode, SessionRecordId,
    };
    use gate4agent_observation_protocol::{
        ObservationEvidenceV1, ObservationKindV1, ObservationV1,
    };
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};
    use gate4agent_harness_protocol::{
        HarnessContextPackLineageV1, HarnessContinuationCleanupStateV1,
        HarnessContinuationRef, HarnessContinuationStateV1, HarnessContinuationV1,
        HarnessCreateTaskRequestV1, HarnessDeliveryBundleDigestV1,
        HarnessDeliveryBundleIdV1, HarnessDeliveryBundleRevisionV1,
        HarnessDeliveryBundleV1, HarnessDeliveryManifestDigestV2,
        HarnessDeliveryRef, HarnessDeliveryStateV1, HarnessDeliveryV1,
        HarnessExecutionModeV1, HarnessIdempotencyRef,
        HarnessOperationId, HarnessOperatorAuthorityV1, HarnessRequestDigest,
        HarnessReceiptRef, HarnessResolvedContextPackReceiptV1,
        HarnessRevision, HarnessRunId, HarnessRunIntentV1, HarnessRunV1,
        HarnessRunLifecycleV1, HarnessSelectorV1, HarnessTaskId, HarnessTaskStateV1,
        HarnessTaskV1,
        SessionGrantId, SessionGrantStateV1,
    };
    use gate4agent_harness_engine::{
        HarnessEngine, HarnessEngineCheckpointV1, HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
    };
    use gate4agent_types::{
        AgentId, AgentInstanceId, ProviderActivity, SessionGeneration, TransportKind,
    };

    fn database_path() -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!(
            "gate4agent-harness-production-bridge-{}-{nonce}.sqlite",
            std::process::id(),
        ))
    }

    fn selector(value: &str) -> HarnessSelectorV1 {
        HarnessSelectorV1::new(value).unwrap()
    }

    fn operator_create_request() -> HarnessCreateTaskRequestV1 {
        HarnessCreateTaskRequestV1 {
            authority: HarnessOperatorAuthorityV1 {
                operation_id: HarnessOperationId::new(format!(
                    "hop_{}",
                    "a".repeat(24),
                )).unwrap(),
                idempotency_ref: HarnessIdempotencyRef::new(format!(
                    "hidem_{}",
                    "a".repeat(24),
                )).unwrap(),
                actor_id: selector("operator"),
                now_unix_ms: 10,
            },
            task_id: HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap(),
            title: "Operator task".to_owned(),
            body: "Bounded operator wire".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Backlog,
        }
    }

    fn task_start_dispatch_intent() -> HarnessDispatchIntentV1 {
        HarnessDispatchIntentV1 {
            task_id: HarnessTaskId::new(format!("htask_{}", "d".repeat(24))).unwrap(),
            task_revision: HarnessRevision::new(2).unwrap(),
            run_id: HarnessRunId::new(format!("hrun_{}", "d".repeat(24))).unwrap(),
            run_revision: HarnessRevision::new(1).unwrap(),
            operation_id: HarnessOperationId::new(format!(
                "hop_{}",
                "d".repeat(24),
            )).unwrap(),
            operation_revision: HarnessRevision::new(1).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "d".repeat(24),
            )).unwrap(),
            parent_run_id: None,
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: gate4agent_harness_protocol::HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("codex-default"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
        }
    }

    #[test]
    fn task_started_replay_reply_does_not_schedule_or_start_dispatch_again() {
        let dispatch = task_start_dispatch_intent();
        let applied = HarnessOperatorResponseV1::TaskStarted(
            gate4agent_harness_protocol::HarnessTaskStartOutcomeV1 {
                dispatch: dispatch.clone(),
                replayed: false,
            },
        );
        let replay = HarnessOperatorResponseV1::TaskStarted(
            gate4agent_harness_protocol::HarnessTaskStartOutcomeV1 {
                dispatch: dispatch.clone(),
                replayed: true,
            },
        );
        let schedule_next = HarnessOperatorResponseV1::Schedule(
            gate4agent_harness_protocol::HarnessScheduleOutcomeV1::Dispatch(
                dispatch.clone(),
            ),
        );

        assert_eq!(
            scheduled_dispatch_from_operator_response(&applied),
            Some(dispatch.clone()),
        );
        assert_eq!(scheduled_dispatch_from_operator_response(&replay), None);
        assert_eq!(
            scheduled_dispatch_from_operator_response(&schedule_next),
            Some(dispatch),
        );
    }

    fn operator_create_intent(
        body: &str,
        submitted_at_unix_ms: u64,
    ) -> gate4agent_harness_api::HarnessOperatorIntentV1 {
        gate4agent_harness_api::HarnessOperatorIntentV1 {
            request_ref: gate4agent_harness_api::HarnessOperatorRequestRefV1::new(format!(
                "hireq_{}",
                "7".repeat(24),
            )).unwrap(),
            submitted_at_unix_ms,
            action: gate4agent_harness_api::HarnessOperatorActionV1::CreateTask {
                title: "Harness-owned task".to_owned(),
                body: body.to_owned(),
                parent_task_id: None,
                dependencies: Vec::new(),
                initial_state: HarnessTaskStateV1::Backlog,
            },
        }
    }

    fn running_harness_fixture() -> (
        HarnessService,
        HarnessTaskId,
        HarnessRunId,
        NodeRoute,
    ) {
        let task_id = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let create_operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "b".repeat(24),
        )).unwrap();
        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let actor = HarnessActorV1::User { actor_id: selector("operator") };
        let task = HarnessTaskV1 {
            task_id: task_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "Lifecycle task".to_owned(),
            body: "Exact live control event".to_owned(),
            creator: actor.clone(),
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Running,
            run_ids: vec![run_id.clone()],
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 5,
        };
        let run = HarnessRunV1 {
            run_id: run_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id.clone(),
            operation_id: create_operation_id.clone(),
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("profile-a"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            binding: Some(HarnessSessionBindingV1 {
                node_id: selector("node-a"),
                node_incarnation: selector(&incarnation.to_string()),
                workspace_id: selector("workspace-a"),
                session: HarnessSessionIdentityV1::Managed {
                    record_id: selector("record-a"),
                    active_session: Some(HarnessRuntimeIdentityV1 {
                        instance_id: 7,
                        generation: 3,
                    }),
                },
            }),
            lifecycle: HarnessRunLifecycleV1::Running,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 5,
        };
        let create_operation = HarnessOperationV1 {
            operation_id: create_operation_id,
            revision: HarnessRevision::new(1).unwrap(),
            actor,
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "b".repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 5,
            dispatched_at_unix_ms: Some(4),
            finished_at_unix_ms: Some(5),
        };
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![task],
            runs: vec![run],
            grants: Vec::new(),
            operations: vec![create_operation],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap();
        let harness = HarnessService::from_engine_for_test(engine);
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: incarnation,
        };
        (harness, task_id, run_id, route)
    }

    fn lifecycle_event(
        sequence: u64,
        kind: C2ControlEventKind,
    ) -> C2NodeEventEnvelope {
        C2NodeEventEnvelope {
            sequence,
            event: C2NodeEvent::Control {
                address: SessionAddress {
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    session: gate4agent_node_protocol::SessionKey {
                        instance_id: gate4agent_types::AgentInstanceId(7),
                        generation: gate4agent_types::SessionGeneration(3),
                    },
                },
                event: C2ControlEvent {
                    protocol_version: gate4agent_types::CONTROL_PROTOCOL_VERSION,
                    sequence,
                    command_id: None,
                    instance_id: gate4agent_types::AgentInstanceId(7),
                    generation: gate4agent_types::SessionGeneration(3),
                    event: kind,
                },
            },
        }
    }

    fn bound_snapshot(
        node_id: &NodeId,
        record_state: ManagedSessionState,
        record_active_session: Option<SessionAddress>,
        status: C2SessionStatus,
    ) -> C2NodeSnapshot {
        C2NodeSnapshot {
                node_id: node_id.clone(),
                enabled_providers: vec![AgentId::new("kimi").unwrap()],
                provider_runtime_statuses: ProviderRuntimeStatuses::default(),
                workspaces: vec![C2WorkspaceSnapshot {
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    canonical_root: OpaqueHostPath::utf8(
                        r"C:\fixture\workspace-a".to_owned(),
                    ).unwrap(),
                    sessions: vec![C2SessionSnapshot {
                        instance_id: AgentInstanceId(7),
                        agent_id: AgentId::new("kimi").unwrap(),
                        transport: TransportKind::Pty,
                        generation: SessionGeneration(3),
                        status,
                        pending_operation: None,
                        pending_input: None,
                        process_id: Some(700),
                        terminal_size: None,
                        terminal_frame: None,
                        provider_activity: ProviderActivity::Working,
                        provider_interaction_pending: false,
                        provider_identity_present: true,
                    }],
                    worktree_service_mode: None,
                    managed_worktree_profiles: None,
                }],
                session_records: vec![C2ManagedSessionRecord {
                    record_id: SessionRecordId::new("record-a").unwrap(),
                    display_name: "Bound recovery fixture".to_owned(),
                    provider: AgentId::new("kimi").unwrap(),
                    mode: SessionMode::Pty,
                    state: record_state,
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    active_session: record_active_session,
                    environment_profile: None,
                    bundle: None,
                    context_id: None,
                    context: None,
                    task_binding: None,
                    provider_identity_present: true,
                    created_at_unix_ms: 1,
                    updated_at_unix_ms: 9,
                }],
                agent_progress: Vec::new(),
                managed_worktrees: Vec::new(),
                launch_inventory: None,
                observation_support: Some(C2ObservationSupport {
                    events: true,
                    managed_target: true,
                    workflow_detail: false,
                }),
        }
    }

    fn bound_session_address() -> SessionAddress {
        SessionAddress {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            session: gate4agent_node_protocol::SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(3),
            },
        }
    }

    fn correlation_inventory(
        route: NodeRoute,
        snapshot: C2NodeSnapshot,
        observed_at_unix_ms: u64,
    ) -> HarnessRuntimeInventoryCache {
        let resync = HarnessObservationResync::test_fixture(route, 5, snapshot);
        let mut cache = HarnessRuntimeInventoryCache::default();
        cache.refresh(&resync, observed_at_unix_ms);
        cache
    }

    #[test]
    fn run_correlation_projects_exact_stored_active_binding_and_current_availability() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );
        let cache = correlation_inventory(route, snapshot, 20);
        let observation_path = database_path();
        let observation = ObservationService::open(&observation_path).unwrap();
        let response = execute_operator_request(
            &mut harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &HarnessLaunchCatalog::default(),
            &cache,
            HarnessOperatorRequestV1::RunCorrelationGet {
                run_id: run_id.clone(),
            },
        ).unwrap();
        let HarnessOperatorResponseV1::RunCorrelation(correlation) = response else {
            panic!("run correlation response expected");
        };
        assert_eq!(correlation.run_id, run_id);
        assert_eq!(correlation.task_id, task_id);
        assert_eq!(correlation.run_revision, HarnessRevision::new(1).unwrap());
        assert_eq!(correlation.node_id.as_str(), "node-a");
        assert_eq!(correlation.workspace_id.as_str(), "workspace-a");
        assert_eq!(correlation.provider_profile.as_str(), "profile-a");
        assert_eq!(
            correlation.availability,
            HarnessRunCorrelationAvailabilityV1::Available,
        );
        assert_eq!(correlation.observed_at_unix_ms, Some(20));
        assert_eq!(
            correlation.session,
            HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: selector("record-a"),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 7,
                    generation: 3,
                }),
            }),
        );
        correlation.validate().unwrap();
        observation.close().unwrap();
        for candidate in [
            observation_path.clone(),
            PathBuf::from(format!("{}-wal", observation_path.display())),
            PathBuf::from(format!("{}-shm", observation_path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn run_transfer_reads_only_durable_records_for_the_exact_run() {
        let (mut harness, _, run_id, _) = running_harness_fixture();
        let observation_path = database_path();
        let observation = ObservationService::open(&observation_path).unwrap();
        let response = execute_operator_request(
            &mut harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &HarnessLaunchCatalog::default(),
            &HarnessRuntimeInventoryCache::default(),
            HarnessOperatorRequestV1::RunTransferGet {
                run_id: run_id.clone(),
            },
        ).unwrap();
        let HarnessOperatorResponseV1::RunTransfer(transfer) = response else {
            panic!("run transfer response expected");
        };
        assert_eq!(transfer.run_id, run_id);
        assert_eq!(transfer.run_revision, HarnessRevision::new(1).unwrap());
        assert_eq!(transfer.delivery, None);
        assert_eq!(transfer.continuation, None);
        transfer.validate().unwrap();

        let missing = execute_operator_request(
            &mut harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &HarnessLaunchCatalog::default(),
            &HarnessRuntimeInventoryCache::default(),
            HarnessOperatorRequestV1::RunTransferGet {
                run_id: HarnessRunId::new(format!("hrun_{}", "f".repeat(24))).unwrap(),
            },
        );
        assert_eq!(missing, Err(HarnessOperatorHostErrorV1::NotFound));
        observation.close().unwrap();
        for candidate in [
            observation_path.clone(),
            PathBuf::from(format!("{}-wal", observation_path.display())),
            PathBuf::from(format!("{}-shm", observation_path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn run_transfer_projects_only_bounded_delivery_and_context_receipt_facts() {
        let (harness, task_id, run_id, route) = running_harness_fixture();
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "d".repeat(24))).unwrap();
        let operation_id = HarnessOperationId::new(format!("hop_{}", "e".repeat(24))).unwrap();
        let bundle = HarnessDeliveryBundleV1 {
            selector: selector("reviewed-skill-bundle"),
            bundle_id: HarnessDeliveryBundleIdV1::new("bundle-a").unwrap(),
            revision: HarnessDeliveryBundleRevisionV1::new("r7").unwrap(),
            digest: HarnessDeliveryBundleDigestV1::new(format!(
                "sha256:{}",
                "a".repeat(64),
            )).unwrap(),
            manifest_digest: HarnessDeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "b".repeat(64),
            )).unwrap(),
        };
        let delivery = HarnessDeliveryV1 {
            delivery_ref: HarnessDeliveryRef::new(format!(
                "hdelivery_{}",
                "d".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            grant_id: grant_id.clone(),
            grant_revision: HarnessRevision::new(1).unwrap(),
            task_id,
            run_id: run_id.clone(),
            operation_id: operation_id.clone(),
            bundle,
            state: HarnessDeliveryStateV1::Prepared,
            stage_receipt: None,
            receipt: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        };
        let source_run_id = HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap();
        let source_binding = HarnessSessionBindingV1 {
            node_id: selector(route.node_id.as_str()),
            node_incarnation: selector(&route.expected_incarnation_id.to_string()),
            workspace_id: selector("workspace-a"),
            session: HarnessSessionIdentityV1::Managed {
                record_id: selector("source-record"),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 9,
                    generation: 2,
                }),
            },
        };
        let continuation = HarnessContinuationV1 {
            continuation_ref: HarnessContinuationRef::new(format!(
                "hcontinuation_{}",
                "e".repeat(24),
            )).unwrap(),
            receipt_ref: HarnessReceiptRef::new(format!(
                "hreceipt_{}",
                "e".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(3).unwrap(),
            state: HarnessContinuationStateV1::Exported,
            grant_id,
            grant_revision: HarnessRevision::new(1).unwrap(),
            source_run_id: source_run_id.clone(),
            target_run_id: run_id.clone(),
            operation_id,
            node_id: source_binding.node_id.clone(),
            node_incarnation: source_binding.node_incarnation.clone(),
            workspace_id: source_binding.workspace_id.clone(),
            source_provider: selector("claude"),
            source_binding,
            context: Some(HarnessResolvedContextPackReceiptV1 {
                id: selector("context-a"),
                digest: format!("sha256:{}", "c".repeat(64)),
                lineage: HarnessContextPackLineageV1 {
                    source_node_id: selector(route.node_id.as_str()),
                    source_workspace_id: selector("workspace-a"),
                    source_instance_id: 9,
                    source_generation: 2,
                    source_provider: selector("claude"),
                },
                source_message_count: 7,
                retained_message_count: 5,
                byte_len: 4096,
                truncated: true,
            }),
            target_binding: None,
            prepared_at_unix_ms: 10,
            exporting_at_unix_ms: Some(11),
            exported_at_unix_ms: Some(12),
            bound_at_unix_ms: None,
            expired_at_unix_ms: None,
            outcome_unknown_at_unix_ms: None,
            outcome_unknown_reason: None,
            cleanup_state: HarnessContinuationCleanupStateV1::Retained,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 12,
        };
        let transfer = project_run_transfer(
            harness.engine().run(&run_id).unwrap(),
            Some(&delivery),
            Some(&continuation),
        ).unwrap();
        assert_eq!(transfer.delivery.as_ref().unwrap().selector.as_str(), "reviewed-skill-bundle");
        let context = transfer.continuation.as_ref().unwrap().context.as_ref().unwrap();
        assert_eq!(context.source_message_count, 7);
        assert_eq!(context.retained_message_count, 5);
        assert_eq!(context.byte_len, 4096);
        assert!(context.truncated);
        assert_eq!(transfer.continuation.as_ref().unwrap().source_run_id, source_run_id);
        let encoded = serde_json::to_string(&transfer).unwrap();
        for private in ["source-record", "private-prompt", "provider-session-id", "C:\\\\private"] {
            assert!(!encoded.contains(private), "private field leaked: {private}");
        }
        transfer.validate().unwrap();
    }

    #[test]
    fn run_correlation_preserves_dormant_and_inline_historical_identities() {
        let (harness, _, run_id, route) = running_harness_fixture();
        let mut dormant = harness.engine().run(&run_id).unwrap().clone();
        dormant.lifecycle = HarnessRunLifecycleV1::Completed;
        dormant.result_disposition = Some(HarnessResultDispositionV1::Detached);
        dormant.binding.as_mut().unwrap().session = HarnessSessionIdentityV1::Managed {
            record_id: selector("record-a"),
            active_session: None,
        };
        let dormant_snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Dormant,
            None,
            C2SessionStatus::Exited { exit_code: Some(0) },
        );
        let cache = correlation_inventory(route.clone(), dormant_snapshot, 21);
        let dormant_view = project_run_correlation(&dormant, &cache).unwrap();
        assert_eq!(
            dormant_view.availability,
            HarnessRunCorrelationAvailabilityV1::Dormant,
        );
        assert_eq!(
            dormant_view.session,
            HarnessRunSessionViewV1::Managed(HarnessManagedRunSessionV1 {
                record_id: selector("record-a"),
                active_session: None,
            }),
        );

        let mut inline = dormant;
        inline.intent.mode = HarnessExecutionModeV1::Inline;
        let inline_ref = gate4agent_harness_protocol::HarnessInlineRef::new(format!(
            "hinline_{}",
            "c".repeat(24),
        )).unwrap();
        inline.binding.as_mut().unwrap().session = HarnessSessionIdentityV1::Inline {
            inline_ref: inline_ref.clone(),
        };
        let inline_view = project_run_correlation(&inline, &cache).unwrap();
        assert_eq!(
            inline_view.session,
            HarnessRunSessionViewV1::Inline(HarnessInlineRunSessionV1 { inline_ref }),
        );
        assert_eq!(
            inline_view.availability,
            HarnessRunCorrelationAvailabilityV1::Unavailable,
        );
        assert_eq!(inline_view.observed_at_unix_ms, Some(21));
    }

    #[test]
    fn run_correlation_reports_missing_and_replaced_inventory_without_rewriting_binding() {
        let (harness, _, run_id, route) = running_harness_fixture();
        let stored = harness.engine().run(&run_id).unwrap();
        let not_observed = project_run_correlation(
            stored,
            &HarnessRuntimeInventoryCache::default(),
        ).unwrap();
        assert_eq!(
            not_observed.availability,
            HarnessRunCorrelationAvailabilityV1::NotObserved,
        );
        assert_eq!(not_observed.observed_at_unix_ms, None);

        let replacement_route = NodeRoute {
            node_id: route.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([8; 16]),
        };
        let replacement_snapshot = bound_snapshot(
            &replacement_route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );
        let cache = correlation_inventory(replacement_route, replacement_snapshot, 22);
        let stale = project_run_correlation(stored, &cache).unwrap();
        assert_eq!(
            stale.availability,
            HarnessRunCorrelationAvailabilityV1::StaleIncarnation,
        );
        assert_eq!(stale.observed_at_unix_ms, Some(22));
        assert_eq!(
            stale.node_incarnation_id.as_str(),
            route.expected_incarnation_id.to_string(),
        );
        assert_eq!(stale.provider_profile.as_str(), "profile-a");
    }

    #[test]
    fn run_correlation_fails_closed_for_missing_or_malformed_stored_binding() {
        let (harness, _, run_id, _) = running_harness_fixture();
        let mut missing = harness.engine().run(&run_id).unwrap().clone();
        missing.lifecycle = HarnessRunLifecycleV1::Requested;
        missing.binding = None;
        assert_eq!(
            project_run_correlation(&missing, &HarnessRuntimeInventoryCache::default()),
            Err(HarnessOperatorHostErrorV1::NotFound),
        );

        let mut malformed = harness.engine().run(&run_id).unwrap().clone();
        malformed.binding.as_mut().unwrap().node_incarnation = selector("not-hex");
        assert_eq!(
            project_run_correlation(&malformed, &HarnessRuntimeInventoryCache::default()),
            Err(HarnessOperatorHostErrorV1::NotFound),
        );
    }

    #[test]
    fn resync_replays_host_down_exit_success_to_completed_review() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        let events = [lifecycle_event(
            5,
            C2ControlEventKind::Exited { exit_code: Some(0), forced: false },
        )];
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &events, 10)
            .unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Completed,
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().state,
            HarnessTaskStateV1::Review,
        );
        assert_eq!(
            harness.engine().run(&run_id).unwrap().result_disposition,
            Some(HarnessResultDispositionV1::Succeeded),
        );
    }

    #[test]
    fn resync_exit_duplicate_and_checkpoint_reopen_are_unchanged() {
        let (mut harness, _task_id, _run_id, route) = running_harness_fixture();
        let events = [lifecycle_event(
            5,
            C2ControlEventKind::Exited { exit_code: Some(0), forced: false },
        )];
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &events, 10)
            .unwrap();
        let after_first_replay = harness.engine().checkpoint();
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &events, 10)
            .unwrap();
        assert_eq!(harness.engine().checkpoint(), after_first_replay);

        let reopened_engine = HarnessEngine::restore(after_first_replay).unwrap();
        let mut reopened = HarnessService::from_engine_for_test(reopened_engine);
        let before_reopen_replay = reopened.engine().checkpoint();
        apply_replayed_lifecycle_events(&mut reopened, &route, Some(4), &events, 10)
            .unwrap();
        assert_eq!(reopened.engine().checkpoint(), before_reopen_replay);
    }

    #[test]
    fn resync_eviction_without_exact_terminal_freezes_running_as_waiting() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &[], 11)
            .unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Waiting,
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().state,
            HarnessTaskStateV1::Waiting,
        );
        let after_gap = harness.engine().checkpoint();
        apply_replayed_lifecycle_events(&mut harness, &route, Some(4), &[], 11)
            .unwrap();
        assert_eq!(harness.engine().checkpoint(), after_gap);
    }

    #[test]
    fn fresh_observation_recovery_restores_exact_running_snapshot_after_gap() {
        let (mut harness, task_id, run_id, route) = running_harness_fixture();
        freeze_bound_route_waiting(&mut harness, &route, 9, 10).unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Waiting,
        );
        let path = database_path();
        let mut observation = ObservationService::open(&path).unwrap();
        assert_eq!(durable_cursor_for(&observation, &route), None);
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(bound_session_address()),
            C2SessionStatus::Running,
        );

        apply_snapshot_lifecycle(
            &mut harness,
            &route,
            9,
            &snapshot,
            &[],
            11,
        ).unwrap();
        observation.apply_resync(ObservationResyncBatch {
            node_id: route.node_id.clone(),
            incarnation_id: route.expected_incarnation_id,
            requested_after: 0,
            high_watermark: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence: 9,
            },
            oldest_available_sequence: 1,
            records: vec![ManagedRecordLink {
                managed: ManagedSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    record_id: SessionRecordId::new("record-a").unwrap(),
                },
                runtime: Some(RuntimeSessionKey {
                    node_id: route.node_id.clone(),
                    incarnation_id: route.expected_incarnation_id,
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(3),
                }),
            }],
            records_complete: true,
            gaps: Vec::new(),
            events: Vec::new(),
        }).unwrap();

        assert_eq!(durable_cursor_for(&observation, &route), Some(9));
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Running,
        );
        assert_eq!(
            harness.engine().task(&task_id).unwrap().state,
            HarnessTaskStateV1::Running,
        );
        let reopened = HarnessEngine::restore(harness.engine().checkpoint()).unwrap();
        assert_eq!(
            reopened.run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Running,
        );
        assert_eq!(
            reopened.task(&task_id).unwrap().state,
            HarnessTaskStateV1::Running,
        );
        observation.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn snapshot_recovery_keeps_unproven_runtime_states_waiting() {
        let cases = [
            (
                ManagedSessionState::Live,
                Some(SessionAddress {
                    workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                        "workspace-a",
                    ).unwrap(),
                    session: gate4agent_node_protocol::SessionKey {
                        instance_id: AgentInstanceId(8),
                        generation: SessionGeneration(3),
                    },
                }),
                C2SessionStatus::Running,
            ),
            (ManagedSessionState::Live, None, C2SessionStatus::Running),
            (ManagedSessionState::Dormant, None, C2SessionStatus::Running),
            (
                ManagedSessionState::Live,
                Some(bound_session_address()),
                C2SessionStatus::Exited { exit_code: None },
            ),
            (
                ManagedSessionState::Live,
                Some(bound_session_address()),
                C2SessionStatus::Exited { exit_code: Some(0) },
            ),
            (
                ManagedSessionState::Live,
                Some(bound_session_address()),
                C2SessionStatus::Exited { exit_code: Some(7) },
            ),
        ];
        for (record_state, active_session, status) in cases {
            let (mut harness, task_id, run_id, route) = running_harness_fixture();
            freeze_bound_route_waiting(&mut harness, &route, 9, 10).unwrap();
            let snapshot = bound_snapshot(
                &route.node_id,
                record_state,
                active_session,
                status,
            );
            apply_snapshot_lifecycle(&mut harness, &route, 9, &snapshot, &[], 11).unwrap();
            assert_eq!(
                harness.engine().run(&run_id).unwrap().lifecycle,
                HarnessRunLifecycleV1::Waiting,
            );
            assert_eq!(
                harness.engine().task(&task_id).unwrap().state,
                HarnessTaskStateV1::Waiting,
            );
        }
    }

    #[test]
    fn durable_revoked_cleanup_backoff_never_drops_pending_authority() {
        let mut cleanup = PendingHarnessMcpAbort {
            route: NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            },
            reservation_id: HarnessMcpReservationId::new(format!(
                "hmcpres_{}",
                "a".repeat(24),
            )).unwrap(),
            activation_digest: HarnessMcpActivationDigest::new(format!(
                "sha256:{}",
                "b".repeat(64),
            )).unwrap(),
            attempts: 0,
            retry_after_unix_ms: 0,
            attempt_id: None,
        };
        for attempt in 1..=12 {
            let now = cleanup.retry_after_unix_ms.max(1);
            defer_harness_mcp_abort(&mut cleanup, now);
            assert_eq!(cleanup.attempts, attempt);
            assert!(cleanup.retry_after_unix_ms > now);
            assert!(cleanup.retry_after_unix_ms - now <= HARNESS_MCP_ABORT_RETRY_MAX_MS);
        }
        assert_eq!(cleanup.reservation_id.as_str(), format!(
            "hmcpres_{}",
            "a".repeat(24),
        ));
    }

    fn apply_managed_link(
        service: &mut ObservationService,
        incarnation_id: NodeIncarnationId,
        sequence: u64,
        generation: u64,
    ) {
        service.apply_ingress(ObservationIngressEnvelope {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor { incarnation_id, sequence },
            received_at_ms: 10 + sequence,
            transport: ObservationTransport::C2,
            payload: ObservationIngressPayload::ManagedRecordUpserted {
                link: ManagedRecordLink {
                    managed: ManagedSessionKey {
                        node_id: NodeId::new("node-a").unwrap(),
                        incarnation_id,
                        record_id: SessionRecordId::new("record-a").unwrap(),
                    },
                    runtime: Some(RuntimeSessionKey {
                        node_id: NodeId::new("node-a").unwrap(),
                        incarnation_id,
                        workspace_id: gate4agent_node_protocol::WorkspaceId::new(
                            "workspace-a",
                        ).unwrap(),
                        instance_id: gate4agent_types::AgentInstanceId(7),
                        generation: gate4agent_types::SessionGeneration(generation),
                    }),
                },
            },
        }).unwrap();
    }

    #[test]
    fn production_bridge_commits_managed_observation_without_harness_authority() {
        let path = database_path();
        let node_id = NodeId::new("node-a").unwrap();
        let incarnation_id = NodeIncarnationId::from_bytes([3; 16]);
        let record_id = SessionRecordId::new("record-a").unwrap();
        let mut service = ObservationService::open(&path).unwrap();
        apply_routed_observation_event(
            &mut service,
            RoutedNodeEvent {
                node_id: node_id.clone(),
                cursor: NodeCursor { incarnation_id, sequence: 1 },
                event: C2NodeEvent::ManagedObservation {
                    record_id: record_id.clone(),
                    observation: ObservationV1 {
                        source_sequence: 1,
                        observed_at_unix_ms: Some(10),
                        evidence: ObservationEvidenceV1::NodeLifecycle,
                        kind: ObservationKindV1::Ready,
                        truncated: false,
                    },
                },
            },
            11,
        ).unwrap();
        let target = ObservationTarget::Managed {
            key: ManagedSessionKey { node_id, incarnation_id, record_id },
        };
        assert_eq!(service.projection(&target).unwrap().timeline.len(), 1);
        service.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[tokio::test]
    async fn read_host_rejects_second_frame_after_newline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(b"{}\n{}\n").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let (mut server, _) = listener.accept().await.unwrap();
        assert!(matches!(
            read_single_frame(&mut server).await,
            Err(HarnessRuntimeError::InvalidFrame),
        ));
        client.await.unwrap();
    }

    #[tokio::test]
    async fn read_host_rejects_oversized_frame_before_dispatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(&vec![b'x'; HARNESS_READ_REQUEST_MAX_BYTES + 1]).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let (mut server, _) = listener.accept().await.unwrap();
        assert!(matches!(
            read_single_frame(&mut server).await,
            Err(HarnessRuntimeError::RequestTooLarge),
        ));
        client.await.unwrap();
    }

    #[tokio::test]
    async fn read_host_slow_incomplete_frame_returns_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (commands, _receiver) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                handle_connection(stream, commands, None).await,
                Err(HarnessRuntimeError::Deadline),
            ));
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"{}\n").await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let reply: HarnessReadReplyV1 = serde_json::from_slice(
            response.strip_suffix(b"\n").unwrap(),
        ).unwrap();
        assert_eq!(
            reply,
            HarnessReadReplyV1::Error { error: HarnessReadHostErrorV1::Deadline },
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn operator_slow_incomplete_frame_returns_operator_deadline_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (commands, _receiver) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let authority = HarnessOperatorCredentialAuthority::new(credential.clone()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                handle_connection(stream, commands, Some(authority)).await,
                Err(HarnessRuntimeError::Deadline),
            ));
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(format!(
            "{{\"version\":1,\"credential\":\"{}\"",
            credential.expose(),
        ).as_bytes()).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let reply: HarnessOperatorReplyV1 = serde_json::from_slice(
            response.strip_suffix(b"\n").unwrap(),
        ).unwrap();
        assert_eq!(
            reply,
            HarnessOperatorReplyV1::Error {
                error: HarnessOperatorHostErrorV1::Deadline,
            },
        );
        server.await.unwrap();
    }

    #[test]
    fn operator_auth_is_digest_only_constant_time_and_separate_from_agent_read() {
        let credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let changed = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "b".repeat(64),
        )).unwrap();
        let authority = HarnessOperatorCredentialAuthority::new(credential.clone()).unwrap();
        assert!(authority.verify(&credential).unwrap());
        assert!(!authority.verify(&changed).unwrap());
        assert!(HarnessOperatorCredential::parse(
            format!("g4ah2_aa.{}", "0".repeat(64)),
        ).is_err());
        assert_eq!(format!("{credential:?}"), "HarnessOperatorCredential([REDACTED])");
    }

    #[test]
    fn operator_wire_preserves_exact_mutation_replay_and_categorical_conflict() {
        let harness_path = database_path();
        let observation_path = database_path();
        let mut harness = HarnessService::open(&harness_path).unwrap();
        let observation = ObservationService::open(&observation_path).unwrap();
        let support = ObservationSupportRegistry::default();
        let launch_catalog = HarnessLaunchCatalog::default();
        let runtime_inventory = HarnessRuntimeInventoryCache::default();
        let request = operator_create_request();
        let first = execute_operator_request(
            &mut harness,
            &observation,
            &support,
            &launch_catalog,
            &runtime_inventory,
            HarnessOperatorRequestV1::CreateTask { request: request.clone() },
        ).unwrap();
        let replay = execute_operator_request(
            &mut harness,
            &observation,
            &support,
            &launch_catalog,
            &runtime_inventory,
            HarnessOperatorRequestV1::CreateTask { request: request.clone() },
        ).unwrap();
        assert_eq!(
            first,
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
        );
        assert_eq!(
            replay,
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Replayed),
        );
        let mut changed = request;
        changed.body = "changed intent".to_owned();
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &runtime_inventory,
                HarnessOperatorRequestV1::CreateTask { request: changed },
            ),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        harness.close().unwrap();
        observation.close().unwrap();
        for path in [harness_path, observation_path] {
            for candidate in [
                path.clone(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
        }
    }

    #[test]
    fn operator_intent_replays_across_reopen_and_credential_rotation() {
        let harness_path = database_path();
        let observation_path = database_path();
        let observation = ObservationService::open(&observation_path).unwrap();
        let support = ObservationSupportRegistry::default();
        let launch_catalog = HarnessLaunchCatalog::default();
        let runtime_inventory = HarnessRuntimeInventoryCache::default();
        let intent = operator_create_intent("Stable typed intent", 10);

        let first_credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "a".repeat(64),
        )).unwrap();
        let rotated_credential = HarnessOperatorCredential::parse(format!(
            "g4aho_{}",
            "b".repeat(64),
        )).unwrap();
        let first_auth = HarnessOperatorCredentialAuthority::new(first_credential.clone()).unwrap();
        let rotated_auth = HarnessOperatorCredentialAuthority::new(rotated_credential.clone()).unwrap();
        assert!(first_auth.verify(&first_credential).unwrap());
        assert!(rotated_auth.verify(&rotated_credential).unwrap());
        assert!(!rotated_auth.verify(&first_credential).unwrap());

        let authorized_before_rotation = authorize_operator_intent(intent.clone()).unwrap();
        let expected_task_id = match &authorized_before_rotation {
            HarnessOperatorRequestV1::CreateTask { request } => request.task_id.clone(),
            _ => panic!("create intent must authorize as create task"),
        };
        assert_eq!(
            authorized_before_rotation,
            authorize_operator_intent(intent.clone()).unwrap(),
        );

        let mut harness = HarnessService::open(&harness_path).unwrap();
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &runtime_inventory,
                HarnessOperatorRequestV1::SubmitIntent { intent: intent.clone() },
            ).unwrap(),
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Applied),
        );
        assert!(harness.engine().task(&expected_task_id).is_some());
        harness.close().unwrap();

        let mut harness = HarnessService::open(&harness_path).unwrap();
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &runtime_inventory,
                HarnessOperatorRequestV1::SubmitIntent { intent: intent.clone() },
            ).unwrap(),
            HarnessOperatorResponseV1::Mutation(HarnessOperatorMutationOutcomeV1::Replayed),
        );
        let mut changed_payload = intent.clone();
        changed_payload.action = gate4agent_harness_api::HarnessOperatorActionV1::CreateTask {
            title: "Harness-owned task".to_owned(),
            body: "Changed typed intent".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Backlog,
        };
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &runtime_inventory,
                HarnessOperatorRequestV1::SubmitIntent { intent: changed_payload },
            ),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        let mut changed_time = intent;
        changed_time.submitted_at_unix_ms += 1;
        assert_eq!(
            execute_operator_request(
                &mut harness,
                &observation,
                &support,
                &launch_catalog,
                &runtime_inventory,
                HarnessOperatorRequestV1::SubmitIntent { intent: changed_time },
            ),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        harness.close().unwrap();
        observation.close().unwrap();
        for path in [harness_path, observation_path] {
            for candidate in [
                path.clone(),
                PathBuf::from(format!("{}-wal", path.display())),
                PathBuf::from(format!("{}-shm", path.display())),
            ] {
                let _ = fs::remove_file(candidate);
            }
        }
    }

    #[test]
    fn runtime_inventory_live_event_invalidates_and_requests_exact_resync_refresh() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let active_session = SessionAddress {
            workspace_id: gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap(),
            session: gate4agent_node_protocol::SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(3),
            },
        };
        let snapshot = bound_snapshot(
            &route.node_id,
            ManagedSessionState::Live,
            Some(active_session),
            C2SessionStatus::Running,
        );
        let resync = HarnessObservationResync::test_fixture(
            route.clone(),
            5,
            snapshot.clone(),
        );
        let mut cache = HarnessRuntimeInventoryCache::default();
        cache.refresh(&resync, 10);
        assert_eq!(cache.page(None, 1).nodes.len(), 1);

        let mut recovery = ObservationRecoveryRegistry::default();
        let event = C2NodeEvent::SessionRecordUpserted {
            record: snapshot.session_records[0].clone(),
        };
        assert!(invalidate_runtime_inventory_for_event(
            &mut cache,
            &mut recovery,
            &route,
            &event,
        ));
        assert!(cache.page(None, 1).nodes.is_empty());
        assert!(recovery.contains(&route));

        let refreshed = HarnessObservationResync::test_fixture(route, 6, snapshot);
        cache.refresh(&refreshed, 11);
        let page = cache.page(None, 1);
        page.validate().unwrap();
        assert_eq!(page.nodes[0].event_sequence, 6);
        assert_eq!(page.nodes[0].inventory.managed_sessions.len(), 1);
        assert_eq!(page.nodes[0].inventory.session_count, 1);
    }

    #[test]
    fn recovery_registry_retries_unhealthy_current_routes_and_freezes_absent_routes() {
        let node_a = NodeId::new("node-a").unwrap();
        let node_b = NodeId::new("node-b").unwrap();
        let incarnation_a = NodeIncarnationId::from_bytes([1; 16]);
        let incarnation_b = NodeIncarnationId::from_bytes([2; 16]);
        let mut support = ObservationSupportRegistry::default();
        support.replace(node_a.clone(), incarnation_a, None);
        support.replace(node_b.clone(), incarnation_b, None);
        support.mark_unhealthy(&node_a, incarnation_a);
        support.reconcile_current_routes(&[NodeRoute {
            node_id: node_a.clone(),
            expected_incarnation_id: incarnation_a,
        }]);
        assert!(!support.is_authoritative(&node_a, incarnation_a));
        assert!(!support.is_authoritative(&node_b, incarnation_b));
        support.replace(node_a.clone(), incarnation_a, None);
        assert!(support.is_authoritative(&node_a, incarnation_a));
        assert!(!support.is_authoritative(&node_b, incarnation_b));
    }

    #[test]
    fn specialized_delivery_failure_classifies_transport_as_unknown_and_local_as_failed() {
        assert_eq!(
            delivery_pre_dispatch_result(&HarnessC2Error::DeliveryTransport(
                gate4agent_c2_client::C2ControlError::Closed,
            )),
            CoordinatorPreDispatchResult::OutcomeUnknown,
        );
        assert_eq!(
            delivery_pre_dispatch_result(&HarnessC2Error::UnknownNode(
                NodeId::new("node-a").unwrap(),
            )),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_stale_delivery_completion_terminalizes_without_stopping_host() {
        assert_eq!(
            delivery_stage_completion_result(&HarnessRuntimeError::Harness(
                HarnessServiceError::InvalidStagedDeliveryProof(
                    "staged delivery proof is not from the current authoritative Node route",
                ),
            )),
            CoordinatorPreDispatchResult::OutcomeUnknown,
        );
        assert_eq!(
            delivery_stage_completion_result(&HarnessRuntimeError::DispatchPreparation),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_dispatch_start_failure_is_terminally_classified() {
        assert_eq!(
            dispatch_start_pre_dispatch_result(&HarnessRuntimeError::C2(
                HarnessC2Error::ContextExportTransport(
                    gate4agent_c2_client::C2ControlError::Closed,
                ),
            )),
            CoordinatorPreDispatchResult::OutcomeUnknown,
        );
        assert_eq!(
            dispatch_start_pre_dispatch_result(&HarnessRuntimeError::C2(
                HarnessC2Error::UnknownNode(NodeId::new("node-a").unwrap()),
            )),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_staged_restart_skips_restage_and_selects_preflight() {
        assert_eq!(
            delivery_needs_staging(
                gate4agent_harness_protocol::HarnessDeliveryStateV1::Prepared,
            ).unwrap(),
            true,
        );
        assert_eq!(
            delivery_needs_staging(
                gate4agent_harness_protocol::HarnessDeliveryStateV1::Staged,
            ).unwrap(),
            false,
        );
        assert!(delivery_needs_staging(
            gate4agent_harness_protocol::HarnessDeliveryStateV1::Committed,
        ).is_err());
    }

    #[test]
    fn specialized_phase_completion_rejects_wrong_operation_or_phase() {
        let operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "d".repeat(24),
        )).unwrap();
        let other = HarnessOperationId::new(format!(
            "hop_{}",
            "e".repeat(24),
        )).unwrap();
        let active = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Delivery,
        );
        assert!(active.is(&operation_id, CoordinatorDispatchPhase::Delivery));
        assert!(!active.is(&other, CoordinatorDispatchPhase::Delivery));
        assert!(!active.is(&operation_id, CoordinatorDispatchPhase::Preflight));
    }

    #[test]
    fn specialized_phase_order_is_delivery_then_continuation_then_preflight() {
        let operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "f".repeat(24),
        )).unwrap();
        let delivery = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Delivery,
        );
        let continuation = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Continuation,
        );
        let preflight = ActiveDispatchJob::new(
            operation_id.clone(),
            CoordinatorDispatchPhase::Preflight,
        );
        assert!(delivery.is(&operation_id, CoordinatorDispatchPhase::Delivery));
        assert!(continuation.is(
            &operation_id,
            CoordinatorDispatchPhase::Continuation,
        ));
        assert!(preflight.is(&operation_id, CoordinatorDispatchPhase::Preflight));
        assert!(!delivery.is(
            &operation_id,
            CoordinatorDispatchPhase::Continuation,
        ));
        assert!(!continuation.is(
            &operation_id,
            CoordinatorDispatchPhase::Preflight,
        ));
    }

    #[test]
    fn specialized_continuation_restart_cuts_never_reexport() {
        use gate4agent_harness_protocol::HarnessContinuationStateV1;

        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Prepared),
            ContinuationResumeAction::BeginExport,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Exporting),
            ContinuationResumeAction::RecoverOutcomeUnknown,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Exported),
            ContinuationResumeAction::Preflight,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::OutcomeUnknown),
            ContinuationResumeAction::FinishOutcomeUnknown,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Expired),
            ContinuationResumeAction::FinishFailed,
        );
        assert_eq!(
            continuation_resume_action(HarnessContinuationStateV1::Bound),
            ContinuationResumeAction::Reject,
        );
    }

    #[test]
    fn specialized_preflight_definite_failure_is_pre_dispatch_failed() {
        assert_eq!(
            preflight_pre_dispatch_result(&HarnessC2Error::UnknownNode(
                NodeId::new("node-a").unwrap(),
            )),
            CoordinatorPreDispatchResult::Failed,
        );
    }

    #[test]
    fn specialized_harness_mcp_start_failure_is_failed_not_unknown() {
        assert!(matches!(
            dispatching_start_error_result(&HarnessRuntimeError::C2(
                HarnessC2Error::HarnessMcpArmEnqueue(
                    gate4agent_c2_client::C2ControlError::QueueFull,
                ),
            )),
            CoordinatorSpawnResult::Failed,
        ));
        assert!(matches!(
            dispatching_start_error_result(&HarnessRuntimeError::C2(
                HarnessC2Error::SpawnEnqueue(
                    gate4agent_c2_client::C2ControlError::Closed,
                ),
            )),
            CoordinatorSpawnResult::Failed,
        ));
    }

    #[test]
    fn specialized_harness_mcp_arm_finish_separates_rejection_from_lost_reply() {
        assert!(matches!(
            harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpRejected {
                code: NodeFailureCode::HarnessMcpUnavailable,
            }),
            CoordinatorSpawnResult::Failed,
        ));
        assert!(matches!(
            harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpTransport(
                gate4agent_c2_client::C2ControlError::Closed,
            )),
            CoordinatorSpawnResult::OutcomeUnknown,
        ));
        assert!(matches!(
            harness_mcp_arm_finish_result(&HarnessC2Error::HarnessMcpCorrelationMismatch),
            CoordinatorSpawnResult::OutcomeUnknown,
        ));
    }

    fn mcp_reservation_id(byte: char) -> HarnessMcpReservationId {
        HarnessMcpReservationId::new(format!("hmcpres_{}", byte.to_string().repeat(24)))
            .unwrap()
    }

    fn mcp_call_id(byte: char) -> HarnessMcpCallId {
        HarnessMcpCallId::new(format!("hmcpcall_{}", byte.to_string().repeat(24)))
            .unwrap()
    }

    #[test]
    fn harness_mcp_worker_registry_rejects_stale_and_duplicate_completions() {
        let reservation_id = mcp_reservation_id('a');
        let call_id = mcp_call_id('b');
        let revision = HarnessRevision::new(3).unwrap();
        let mut workers = HarnessMcpWorkerRegistry::default();
        workers.activations.insert(
            reservation_id.clone(),
            ActiveHarnessMcpActivation {
                attempt_id: 7,
                expected_revision: revision,
                updated_at_unix_ms: 11,
                reply: None,
            },
        );
        workers.relays.insert(
            (reservation_id.clone(), call_id.clone()),
            ActiveHarnessMcpRelay { attempt_id: 9 },
        );
        assert!(workers.accepts_activation(&reservation_id, 7, revision));
        assert!(!workers.accepts_activation(&reservation_id, 8, revision));
        assert!(!workers.accepts_activation(
            &reservation_id,
            7,
            HarnessRevision::new(4).unwrap(),
        ));
        assert!(workers.accepts_relay(&reservation_id, &call_id, 9));
        assert!(!workers.accepts_relay(&reservation_id, &call_id, 10));
        workers.activations.remove(&reservation_id);
        workers.relays.remove(&(reservation_id.clone(), call_id.clone()));
        assert!(!workers.accepts_activation(&reservation_id, 7, revision));
        assert!(!workers.accepts_relay(&reservation_id, &call_id, 9));

        let mut abort = PendingHarnessMcpAbort {
            route: NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            },
            reservation_id,
            activation_digest: HarnessMcpActivationDigest::new(format!(
                "sha256:{}",
                "c".repeat(64),
            )).unwrap(),
            attempts: 0,
            retry_after_unix_ms: 0,
            attempt_id: Some(12),
        };
        assert!(abort.accepts_completion(12));
        assert!(!abort.accepts_completion(13));
        abort.attempt_id = None;
        assert!(!abort.accepts_completion(12));
    }

    #[test]
    fn harness_mcp_worker_registry_enforces_one_global_bounded_cap() {
        let mut workers = HarnessMcpWorkerRegistry::default();
        let pending = BTreeMap::new();
        for index in 0..HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX {
            let digit = char::from(b'0' + index as u8);
            workers.relays.insert(
                (mcp_reservation_id(digit), mcp_call_id(digit)),
                ActiveHarnessMcpRelay { attempt_id: index as u64 + 1 },
            );
        }
        assert_eq!(
            workers.in_flight(&pending),
            HARNESS_MCP_GENERAL_NETWORK_WORKERS_MAX,
        );
        assert!(!workers.has_capacity(&pending));
    }

    #[test]
    fn native_history_worker_cap_and_failure_mapping_are_typed() {
        let mut workers = NativeHistoryWorkerRegistry::default();
        for _ in 0..NATIVE_HISTORY_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());
        assert_eq!(
            map_native_history_error(HarnessC2Error::NativeHistoryEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_native_history_error(HarnessC2Error::NodeOffline(
                gate4agent_node_protocol::NodeId::new("node-a").unwrap(),
            )),
            HarnessOperatorHostErrorV1::Unavailable,
        );
        assert_eq!(
            map_native_history_error(HarnessC2Error::IncarnationChanged {
                node_id: gate4agent_node_protocol::NodeId::new("node-a").unwrap(),
            }),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_native_history_error(HarnessC2Error::NativeHistoryDeadline),
            HarnessOperatorHostErrorV1::Deadline,
        );
    }

    #[test]
    fn run_read_worker_cap_deadline_and_failure_mapping_are_typed() {
        let mut workers = RunReadWorkerRegistry::default();
        for _ in 0..RUN_READ_WORKERS_MAX { assert!(workers.try_start()); }
        assert!(!workers.try_start());
        workers.finish();
        assert!(workers.try_start());

        let request = HarnessOperatorRequestV1::InspectRunWorkspace {
            run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
        };
        assert_eq!(
            operator_response_deadline(&request),
            HOST_RUN_READ_RESPONSE_DEADLINE,
        );
        assert_eq!(HOST_RUN_READ_RESPONSE_DEADLINE, Duration::from_secs(12));
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadEnqueue(
                gate4agent_c2_client::C2ControlError::QueueFull,
            )),
            HarnessOperatorHostErrorV1::Busy,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadDeadline),
            HarnessOperatorHostErrorV1::Deadline,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadRouteMismatch),
            HarnessOperatorHostErrorV1::Conflict,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadRejected {
                code: NodeFailureCode::RepositoryFileNotFound,
            }),
            HarnessOperatorHostErrorV1::NotFound,
        );
        assert_eq!(
            map_run_read_error(HarnessC2Error::RunReadRejected {
                code: NodeFailureCode::ResponseTooLarge,
            }),
            HarnessOperatorHostErrorV1::TooLarge,
        );
    }

    #[test]
    fn run_read_completion_accepts_lifecycle_only_change_and_rejects_binding_change() {
        let (harness, _, run_id, _) = running_harness_fixture();
        let captured = harness.engine().run(&run_id).unwrap();
        let prepared = PreparedRunRead::from_operator_request(
            captured,
            HarnessOperatorRequestV1::InspectRunWorkspace {
                run_id: run_id.clone(),
            },
        ).unwrap();

        let mut lifecycle_only = captured.clone();
        lifecycle_only.revision = HarnessRevision::new(2).unwrap();
        lifecycle_only.lifecycle = HarnessRunLifecycleV1::Waiting;
        lifecycle_only.updated_at_unix_ms += 1;
        assert_eq!(
            validate_run_read_completion_origin(Some(&lifecycle_only), &prepared),
            Ok(()),
        );

        let mut changed_binding = lifecycle_only;
        changed_binding.binding.as_mut().unwrap().workspace_id = selector("workspace-b");
        assert_eq!(
            validate_run_read_completion_origin(Some(&changed_binding), &prepared),
            Err(HarnessOperatorHostErrorV1::Conflict),
        );
        assert_eq!(
            validate_run_read_completion_origin(None, &prepared),
            Err(HarnessOperatorHostErrorV1::NotFound),
        );
    }

    #[test]
    fn harness_mcp_worker_cap_saturation_enqueues_typed_rejection() {
        use gate4agent_node_protocol::{SessionKey, WorkspaceId};
        use gate4agent_types::{AgentInstanceId, SessionGeneration};

        let (rejects, mut reject_rx) = mpsc::channel(
            MAX_HARNESS_MCP_PENDING_CALLS_PER_NODE,
        );
        let reservation_id = mcp_reservation_id('f');
        let call_id = mcp_call_id('e');
        let plan = HarnessMcpRelayPlan {
            route: NodeRoute {
                node_id: NodeId::new("node-a").unwrap(),
                expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
            },
            reservation_id: reservation_id.clone(),
            activation_digest: HarnessMcpActivationDigest::new(format!(
                "sha256:{}",
                "a".repeat(64),
            )).unwrap(),
            record_id: SessionRecordId::new("record-a").unwrap(),
            session: SessionAddress {
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(7),
                    generation: SessionGeneration(1),
                },
            },
            call_id: call_id.clone(),
            deadline_unix_ms: u64::MAX,
            outcome: Ok(vec![1, 2, 3]),
        };
        enqueue_harness_mcp_capacity_rejection(&rejects, plan).unwrap();
        let rejected = reject_rx.try_recv().unwrap();
        assert_eq!(rejected.reservation_id, reservation_id);
        assert_eq!(rejected.call_id, call_id);
        assert_eq!(rejected.outcome, Err(HarnessMcpRejectReasonV1::Internal));
    }

    #[derive(Clone, Copy)]
    enum BlockedHarnessMcpWorkerKind { Activation, Abort, Relay }

    async fn assert_actor_inputs_responsive_with_blocked_mcp_worker(
        kind: BlockedHarnessMcpWorkerKind,
    ) {
        let reservation_id = mcp_reservation_id('d');
        let call_id = mcp_call_id('e');
        let (commands, mut command_rx) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let (events, mut event_rx) = mpsc::channel(1);
        let (release, blocked) = oneshot::channel::<()>();
        let worker_commands = commands.clone();
        let worker_reservation_id = reservation_id.clone();
        let worker_call_id = call_id.clone();
        let worker = tokio::spawn(async move {
            let _ = blocked.await;
            let command = match kind {
                BlockedHarnessMcpWorkerKind::Activation => {
                    HostCommand::HarnessMcpActivationFinished {
                        reservation_id: worker_reservation_id,
                        attempt_id: 1,
                        expected_revision: HarnessRevision::new(1).unwrap(),
                        result: Err(HarnessC2Error::TopologyClosed),
                    }
                }
                BlockedHarnessMcpWorkerKind::Abort => {
                    HostCommand::HarnessMcpAbortFinished {
                        reservation_id: worker_reservation_id,
                        attempt_id: 1,
                        result: Err(HarnessC2Error::TopologyClosed),
                    }
                }
                BlockedHarnessMcpWorkerKind::Relay => {
                    HostCommand::HarnessMcpRelayFinished {
                        reservation_id: worker_reservation_id,
                        call_id: worker_call_id,
                        attempt_id: 1,
                        result: Err(HarnessRuntimeError::HostStopped),
                    }
                }
            };
            worker_commands.send(command).await.unwrap();
        });
        assert!(timeout(Duration::from_millis(10), command_rx.recv()).await.is_err());
        let (reply, _receive) = oneshot::channel();
        commands.send(HostCommand::Operator {
            request: HarnessOperatorRequestV1::CreateTask {
                request: operator_create_request(),
            },
            reply,
        }).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_millis(50), command_rx.recv()).await.unwrap(),
            Some(HostCommand::Operator { .. }),
        ));
        events.send(7u8).await.unwrap();
        assert_eq!(
            timeout(Duration::from_millis(50), event_rx.recv()).await.unwrap(),
            Some(7),
        );
        release.send(()).unwrap();
        assert!(timeout(Duration::from_millis(50), command_rx.recv())
            .await.unwrap().is_some());
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn operator_and_event_inputs_remain_responsive_while_activation_worker_is_blocked() {
        assert_actor_inputs_responsive_with_blocked_mcp_worker(
            BlockedHarnessMcpWorkerKind::Activation,
        ).await;
    }

    #[tokio::test]
    async fn operator_and_event_inputs_remain_responsive_while_abort_worker_is_blocked() {
        assert_actor_inputs_responsive_with_blocked_mcp_worker(
            BlockedHarnessMcpWorkerKind::Abort,
        ).await;
    }

    #[tokio::test]
    async fn operator_and_event_inputs_remain_responsive_while_relay_worker_is_blocked() {
        assert_actor_inputs_responsive_with_blocked_mcp_worker(
            BlockedHarnessMcpWorkerKind::Relay,
        ).await;
    }

    fn recovery_event(route: &NodeRoute, sequence: u64) -> RoutedNodeEvent {
        RoutedNodeEvent {
            node_id: route.node_id.clone(),
            cursor: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence,
            },
            event: C2NodeEvent::ResyncRequired {
                oldest_available_sequence: sequence,
            },
        }
    }

    #[test]
    fn recovery_buffer_overflow_clears_events_and_requires_immediate_followup() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let mut recovery = RouteObservationRecovery::new(route.clone());
        for sequence in 1..=OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX as u64 {
            recovery.buffer(recovery_event(&route, sequence));
        }
        assert_eq!(
            recovery.buffered.len(),
            OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX,
        );
        recovery.buffer(recovery_event(
            &route,
            OBSERVATION_RECOVERY_BUFFERED_EVENTS_MAX as u64 + 1,
        ));
        assert!(recovery.buffered.is_empty());
        assert_eq!(recovery.buffered_bytes, 0);
        assert!(recovery.overflowed);
        assert!(recovery.refresh_after_completion);
        recovery.prepare_follow_up();
        assert!(!recovery.overflowed);
        assert!(!recovery.refresh_after_completion);
        assert!(recovery.retry_after <= Instant::now());
    }

    #[test]
    fn recovery_stale_completion_is_rejected_by_attempt_route_and_cursor() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let changed = NodeRoute {
            node_id: route.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([2; 16]),
        };
        let mut recovery = RouteObservationRecovery::new(route.clone());
        recovery.attempt = Some(ObservationRecoveryAttempt {
            attempt_id: 7,
            requested_after: 11,
        });
        assert!(recovery.accepts_completion(&route, 7, 11));
        assert!(!recovery.accepts_completion(&route, 8, 11));
        assert!(!recovery.accepts_completion(&route, 7, 12));
        assert!(!recovery.accepts_completion(&changed, 7, 11));
    }

    #[test]
    fn recovery_applies_lifecycle_before_cursor_and_buffer_drain() {
        let (mut harness, _task_id, run_id, route) = running_harness_fixture();
        let path = database_path();
        let mut observation = ObservationService::open(&path).unwrap();
        let mut recovery = RouteObservationRecovery::new(route.clone());
        recovery.buffer(RoutedNodeEvent {
            node_id: route.node_id.clone(),
            cursor: NodeCursor {
                incarnation_id: route.expected_incarnation_id,
                sequence: 6,
            },
            event: C2NodeEvent::ManagedObservation {
                record_id: SessionRecordId::new("record-a").unwrap(),
                observation: ObservationV1 {
                    source_sequence: 6,
                    observed_at_unix_ms: Some(12),
                    evidence: ObservationEvidenceV1::NodeLifecycle,
                    kind: ObservationKindV1::Ready,
                    truncated: false,
                },
            },
        });

        apply_replayed_lifecycle_events(
            &mut harness,
            &route,
            Some(4),
            &[lifecycle_event(
                5,
                C2ControlEventKind::Exited { exit_code: Some(0), forced: false },
            )],
            11,
        ).unwrap();
        assert_eq!(
            harness.engine().run(&run_id).unwrap().lifecycle,
            HarnessRunLifecycleV1::Completed,
        );
        assert_eq!(durable_cursor_for(&observation, &route), None);

        for (_, routed) in std::mem::take(&mut recovery.buffered) {
            apply_routed_observation_event(&mut observation, routed, 12).unwrap();
        }
        assert_eq!(durable_cursor_for(&observation, &route), Some(6));
        observation.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn recovery_topology_refresh_invalidates_old_route_and_refetches_same_route() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let replacement = NodeRoute {
            node_id: route.node_id.clone(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([2; 16]),
        };
        let mut registry = ObservationRecoveryRegistry::default();
        registry.ensure_route(route.clone()).attempt = Some(ObservationRecoveryAttempt {
            attempt_id: 1,
            requested_after: 3,
        });
        registry.reconcile_topology(&[route.clone()]);
        assert!(registry.routes.get(&ObservationRecoveryRegistry::key(&route))
            .unwrap().refresh_after_completion);

        registry.reconcile_topology(&[replacement.clone()]);
        assert!(!registry.contains(&route));
        assert!(registry.contains(&replacement));
        assert!(registry.routes.get(&ObservationRecoveryRegistry::key(&replacement))
            .unwrap().attempt.is_none());
    }

    #[tokio::test]
    async fn operator_command_remains_responsive_while_recovery_completion_is_blocked() {
        let route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([1; 16]),
        };
        let (commands, mut receive) = mpsc::channel(HOST_COMMAND_CAPACITY);
        let (release, blocked) = oneshot::channel::<()>();
        let worker_commands = commands.clone();
        let worker_route = route.clone();
        let worker = tokio::spawn(async move {
            let _ = blocked.await;
            worker_commands.send(HostCommand::ObservationRecoveryFinished {
                route: worker_route.clone(),
                attempt_id: 1,
                requested_after: 0,
                result: Err(HarnessC2Error::UnknownNode(worker_route.node_id)),
            }).await.unwrap();
        });
        let (reply, _reply_receive) = oneshot::channel();
        commands.send(HostCommand::Operator {
            request: HarnessOperatorRequestV1::CreateTask {
                request: operator_create_request(),
            },
            reply,
        }).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_millis(50), receive.recv()).await.unwrap(),
            Some(HostCommand::Operator { .. }),
        ));
        release.send(()).unwrap();
        worker.await.unwrap();
        assert!(matches!(
            receive.recv().await,
            Some(HostCommand::ObservationRecoveryFinished {
                route: completed_route,
                attempt_id: 1,
                requested_after: 0,
                ..
            }) if completed_route == route,
        ));
    }

    #[test]
    fn credential_mint_binding_requires_current_observation_generation() {
        let path = database_path();
        let node_id = NodeId::new("node-a").unwrap();
        let incarnation_id = NodeIncarnationId::from_bytes([4; 16]);
        let record_id = SessionRecordId::new("record-a").unwrap();
        let workspace_id = gate4agent_node_protocol::WorkspaceId::new("workspace-a").unwrap();
        let mut service = ObservationService::open(&path).unwrap();
        service.apply_ingress(ObservationIngressEnvelope {
            node_id: node_id.clone(),
            cursor: NodeCursor { incarnation_id, sequence: 1 },
            received_at_ms: 10,
            transport: ObservationTransport::C2,
            payload: ObservationIngressPayload::ManagedRecordUpserted {
                link: ManagedRecordLink {
                    managed: ManagedSessionKey {
                        node_id,
                        incarnation_id,
                        record_id,
                    },
                    runtime: Some(RuntimeSessionKey {
                        node_id: NodeId::new("node-a").unwrap(),
                        incarnation_id,
                        workspace_id,
                        instance_id: gate4agent_types::AgentInstanceId(7),
                        generation: gate4agent_types::SessionGeneration(2),
                    }),
                },
            },
        }).unwrap();
        let mut binding = CredentialBindingV1 {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "a".repeat(24))).unwrap(),
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            node_incarnation: selector(&incarnation_id.to_string()),
            record_id: selector("record-a"),
            instance_id: 7,
            generation: 1,
        };
        let mut support = ObservationSupportRegistry::default();
        support.replace(
            NodeId::new("node-a").unwrap(),
            incarnation_id,
            Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: true,
            }),
        );
        assert_eq!(
            verify_observation_credential_binding(
                &service,
                &support,
                &binding,
            ),
            Err(HarnessReadHostErrorV1::Unauthorized),
        );
        binding.generation = 2;
        assert_eq!(
            verify_observation_credential_binding(&service, &support, &binding),
            Ok(()),
        );
        support.mark_unhealthy(&NodeId::new("node-a").unwrap(), incarnation_id);
        assert_eq!(
            verify_observation_credential_binding(&service, &support, &binding),
            Err(HarnessReadHostErrorV1::Unauthorized),
        );
        service.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn credential_read_rejects_observation_generation_replaced_after_mint() {
        let path = database_path();
        let incarnation_id = NodeIncarnationId::from_bytes([4; 16]);
        let harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let mut observation = ObservationService::open(&path).unwrap();
        apply_managed_link(&mut observation, incarnation_id, 1, 1);
        let mut support = ObservationSupportRegistry::default();
        support.replace(
            NodeId::new("node-a").unwrap(),
            incarnation_id,
            Some(C2ObservationSupport {
                events: true,
                managed_target: true,
                workflow_detail: true,
            }),
        );
        let authority = CredentialAuthority::new().unwrap();
        let credential = authority.mint(
            harness.engine(),
            crate::credential::tests::binding(1, 1),
            100,
            200,
        ).unwrap();
        apply_managed_link(&mut observation, incarnation_id, 2, 2);
        assert_eq!(
            crate::read::verify_and_execute_read(
                &harness,
                &observation,
                &support,
                &authority,
                &credential,
                150,
                gate4agent_harness_api::HarnessReadRequestV1::ContextGet,
            ),
            Err(HarnessReadHostErrorV1::Unauthorized),
        );
        observation.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    #[test]
    fn topology_change_ready_before_command_denies_stale_binding() {
        let binding = crate::credential::tests::binding(1, 1);
        let changed_route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([5; 16]),
        };
        assert!(!topology_binding_matches_route(&binding, &changed_route));
        let current_route = NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([4; 16]),
        };
        assert!(topology_binding_matches_route(&binding, &current_route));
    }
}
