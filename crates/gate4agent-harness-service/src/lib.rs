//! Single-writer SQLite authority for the adjacent harness kernel.

pub mod agent_stream;
pub mod c2;
pub mod credential;
pub mod delivery;
pub mod dispatch;
pub mod read;
pub mod runtime;
mod store;
pub mod terminal;

use gate4agent_harness_engine::{
    HarnessApplyOutcome, HarnessEngine, HarnessEngineCheckpointV1, HarnessEngineError,
    HarnessMutationV1, PreparedHarnessMutation,
};
use gate4agent_harness_api::{
    HarnessMailSendResultV1, HarnessReplaceTaskExecutionSpecRequestV2,
    HarnessReviewedWorktreeSelectionV1, HarnessStartTaskRequestV2, HarnessTaskLaunchOptionsV1,
    HarnessTaskCreateResultV1, HarnessTaskMoveResultV1,
};
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessCancelTaskRequestV1, HarnessCreateTaskRequestV1,
    HarnessContextPackLineageV1, HarnessContextSourceSelectionV1,
    HarnessContinuationCleanupStateV1,
    HarnessContinuationOutcomeUnknownReasonV1,
    HarnessContinuationRef, HarnessContinuationStateV1, HarnessContinuationV1,
    HarnessDeliveryRef, HarnessDeliveryStateV1, HarnessDeliveryV1, HarnessDispatchIntentV1,
    HarnessExecutionModeV1, HarnessExecutionSpecId,
    HarnessExpectedExecutionSpecRevisionV1, HarnessGrantTargetV1, HarnessIdempotencyRef,
    HarnessMailAddressV1, HarnessMoveTaskRequestV1,
    HarnessOperationId, HarnessOperationKindV1, HarnessOperationStateV1, HarnessOperationV1,
    HarnessOperatorAuthorityV1, HarnessReplaceTaskExecutionSpecRequestV1,
    HarnessReplaceTaskRequestV1, HarnessRequestDigest,
    HarnessRetryTaskRequestV1, HarnessRevision, HarnessRunId, HarnessRunLifecycleV1,
    HarnessResolvedContextPackReceiptV1, HarnessResultRef, HarnessRunGitFactsV1, HarnessRunV1,
    HarnessScheduleOutcomeV1,
    HarnessScheduledLaunchRefV2, HarnessScheduleRequestV1, HarnessSelectorV1,
    HarnessLaunchTargetSelectionV1, HarnessLaunchWorktreeSelectionV1,
    HarnessSessionIdentityV1, HarnessStartTaskRequestV1, HarnessTaskExecutionSpecV1,
    HarnessTaskExecutionSpecV2,
    HarnessTaskId, HarnessTaskLaunchIssuanceId, HarnessTaskLaunchIssuanceV1,
    HarnessTaskLaunchIssuanceRefV1,
    HarnessTaskStartOutcomeV1, HarnessTaskStateV1, HarnessTaskV1,
    HarnessTransferAuthorityRefV1, HarnessValidationError, HarnessWorktreeIntentV1,
    SessionGrantId, HARNESS_RESULTS_MAX,
};
use gate4agent_harness_delivery::{CompiledDeliveryBundleV2, DeliveryCatalogV2};
use gate4agent_node_protocol::{
    HarnessMcpActivationDigest, HarnessMcpReservationId, ResolvedHarnessMcpProxyReceiptV1,
    ManagedWorktreeLeaseId, NodeId, NodeIncarnationId,
    SessionAddress, SessionMode, SessionRecordId, SpawnOverride, SpawnSpec,
    MAX_HARNESS_MCP_RESERVATION_TTL_MS,
    WorktreeProfileId, WorktreeProfileRevision,
};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, path::Path};
use thiserror::Error;

use store::{
    HarnessStore, PersistedEntity, PersistedHarnessState, PersistedMailAck,
    PersistedMailMessage, PersistedOperation,
};
use dispatch::{
    derive_schedule_request, HarnessGrantPolicyV1, HarnessLaunchCatalog,
    HarnessScheduledLaunchRefV1,
};

pub use store::{HarnessStoreError, HARNESS_OPERATION_TAIL_MAX, HARNESS_STORE_SCHEMA_VERSION};

pub const HARNESS_SERVICE_CHECKPOINT_VERSION_V1: u16 = 1;
pub const HARNESS_DISPATCH_BASELINE_MAX: usize = 4_096;
const HARNESS_EXECUTION_SPEC_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-execution-spec-id-v1\0";
const HARNESS_LAUNCH_ISSUANCE_ID_DOMAIN: &[u8] =
    b"gate4agent-harness-launch-issuance-id-v1\0";
const HARNESS_LAUNCH_POLICY_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-launch-policy-digest-v1\0";
const HARNESS_CONTEXT_SOURCE_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-context-source-digest-v1\0";
const HARNESS_LAUNCH_ISSUANCE_DIGEST_DOMAIN: &[u8] =
    b"gate4agent-harness-launch-issuance-digest-v1\0";

pub(crate) enum PreparedScheduledSpawnLease {
    Direct(c2::PreparedSpawnDispatch),
    Managed(c2::PreparedManagedWorktreeSpawnDispatch),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessMcpReservationStateV1 {
    Prepared,
    Armed,
    Bound,
    Active,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarnessMcpReservationV1 {
    pub reservation_id: HarnessMcpReservationId,
    pub revision: HarnessRevision,
    pub state: HarnessMcpReservationStateV1,
    pub activation_digest: HarnessMcpActivationDigest,
    pub grant_id: SessionGrantId,
    pub grant_revision: HarnessRevision,
    pub actor_run_id: HarnessRunId,
    pub operation_id: HarnessOperationId,
    pub node_id: HarnessSelectorV1,
    pub node_incarnation_id: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub provider_profile: HarnessSelectorV1,
    pub expected_provider: HarnessSelectorV1,
    pub mode: HarnessExecutionModeV1,
    pub spawn_spec_fingerprint: HarnessRequestDigest,
    pub idempotency_ref: HarnessIdempotencyRef,
    pub expires_at_unix_ms: u64,
    pub record_id: Option<HarnessSelectorV1>,
    pub instance_id: Option<u64>,
    pub generation: Option<u64>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl HarnessMcpReservationV1 {
    fn validate(&self) -> Result<(), HarnessServiceError> {
        self.revision.validate()?;
        self.grant_id.validate()?;
        self.grant_revision.validate()?;
        self.actor_run_id.validate()?;
        self.operation_id.validate()?;
        self.node_id.validate()?;
        self.node_incarnation_id.validate()?;
        self.workspace_id.validate()?;
        self.provider_profile.validate()?;
        self.expected_provider.validate()?;
        self.spawn_spec_fingerprint.validate()?;
        self.idempotency_ref.validate()?;
        if self.created_at_unix_ms == 0
            || self.updated_at_unix_ms < self.created_at_unix_ms
            || self.expires_at_unix_ms <= self.created_at_unix_ms
            || self.expires_at_unix_ms - self.created_at_unix_ms
                > MAX_HARNESS_MCP_RESERVATION_TTL_MS
        {
            return Err(HarnessServiceError::InvalidHarnessMcpReservation(
                "invalid reservation timestamps",
            ));
        }
        if let Some(record_id) = &self.record_id { record_id.validate()?; }
        let has_binding = self.record_id.is_some()
            && self.instance_id.is_some_and(|value| value != 0)
            && self.generation.is_some_and(|value| value != 0);
        let binding_empty = self.record_id.is_none()
            && self.instance_id.is_none()
            && self.generation.is_none();
        if !match self.state {
            HarnessMcpReservationStateV1::Prepared | HarnessMcpReservationStateV1::Armed => {
                binding_empty
            }
            HarnessMcpReservationStateV1::Bound | HarnessMcpReservationStateV1::Active => {
                has_binding
            }
            HarnessMcpReservationStateV1::Revoked => has_binding || binding_empty,
        } {
            return Err(HarnessServiceError::InvalidHarnessMcpReservation(
                "reservation binding does not match state",
            ));
        }
        Ok(())
    }

    pub(crate) fn proxy_receipt(&self) -> ResolvedHarnessMcpProxyReceiptV1 {
        ResolvedHarnessMcpProxyReceiptV1 {
            reservation_id: self.reservation_id.clone(),
            activation_digest: self.activation_digest.clone(),
        }
    }
}

pub struct PreparedHarnessMcpReservation {
    reservation: HarnessMcpReservationV1,
}

impl PreparedHarnessMcpReservation {
    pub fn reservation_id(&self) -> &HarnessMcpReservationId {
        &self.reservation.reservation_id
    }

    pub fn activation_digest(&self) -> &HarnessMcpActivationDigest {
        &self.reservation.activation_digest
    }

    pub fn expires_at_unix_ms(&self) -> u64 { self.reservation.expires_at_unix_ms }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessDispatchContextV1 {
    pub operation_id: HarnessOperationId,
    pub node_id: HarnessSelectorV1,
    pub node_incarnation_id: HarnessSelectorV1,
    pub workspace_id: HarnessSelectorV1,
    pub provider_profile: HarnessSelectorV1,
    pub expected_provider: HarnessSelectorV1,
    pub mode: HarnessExecutionModeV1,
    pub baseline_record_ids: Vec<HarnessSelectorV1>,
    pub spawn_spec_fingerprint: HarnessRequestDigest,
    pub dispatched_at_unix_ms: u64,
    pub idempotency_ref: HarnessIdempotencyRef,
    #[serde(default)]
    pub managed_worktree_binding: Option<HarnessManagedWorktreeBindingReceiptV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessManagedWorktreeBindingReceiptV1 {
    pub lease_id: ManagedWorktreeLeaseId,
    pub launch_issuance: HarnessTaskLaunchIssuanceRefV1,
    pub source_workspace_id: HarnessSelectorV1,
    pub allocated_workspace_id: HarnessSelectorV1,
    pub profile_id: HarnessSelectorV1,
    pub profile_revision: HarnessSelectorV1,
}

impl HarnessManagedWorktreeBindingReceiptV1 {
    fn validate(&self) -> Result<(), HarnessServiceError> {
        self.launch_issuance.validate()?;
        self.source_workspace_id.validate()?;
        self.allocated_workspace_id.validate()?;
        self.profile_id.validate()?;
        self.profile_revision.validate()?;
        if self.source_workspace_id == self.allocated_workspace_id {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "managed worktree receipt aliases its source workspace",
            ));
        }
        Ok(())
    }
}

impl HarnessDispatchContextV1 {
    pub fn validate(&self) -> Result<(), HarnessServiceError> {
        self.operation_id.validate()?;
        self.node_id.validate()?;
        self.node_incarnation_id.validate()?;
        self.workspace_id.validate()?;
        self.provider_profile.validate()?;
        self.expected_provider.validate()?;
        self.spawn_spec_fingerprint.validate()?;
        self.idempotency_ref.validate()?;
        if let Some(managed_worktree_binding) = &self.managed_worktree_binding {
            managed_worktree_binding.validate()?;
            if managed_worktree_binding.source_workspace_id != self.workspace_id {
                return Err(HarnessServiceError::InvalidDispatchContext(
                    "managed worktree receipt source does not match dispatch source",
                ));
            }
        }
        if self.dispatched_at_unix_ms == 0 {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch timestamp must be nonzero",
            ));
        }
        if self.baseline_record_ids.len() > HARNESS_DISPATCH_BASELINE_MAX {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch baseline is unbounded",
            ));
        }
        for record_id in &self.baseline_record_ids {
            record_id.validate()?;
        }
        if self.baseline_record_ids.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch baseline is not canonical",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HarnessServiceCheckpointV1 {
    version: u16,
    engine: HarnessEngineCheckpointV1,
    dispatch_contexts: Vec<HarnessDispatchContextV1>,
    #[serde(default)]
    deliveries: Vec<HarnessDeliveryV1>,
    #[serde(default)]
    continuations: Vec<HarnessContinuationV1>,
    #[serde(default)]
    harness_mcp_reservations: Vec<HarnessMcpReservationV1>,
    #[serde(default)]
    operator_requests: Vec<HarnessOperatorRequestV1>,
    #[serde(default)]
    scheduled_launches: BTreeMap<HarnessOperationId, HarnessScheduledLaunchRefV1>,
    #[serde(default)]
    issued_launches: BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HarnessOperatorRequestV1 {
    operation_id: HarnessOperationId,
    request_digest: HarnessRequestDigest,
}

#[derive(Clone, Eq, PartialEq)]
pub struct HarnessCommittedSnapshot {
    pub engine: HarnessEngineCheckpointV1,
    pub dispatch_contexts: Vec<HarnessDispatchContextV1>,
    harness_mcp_reservations: Vec<HarnessMcpReservationV1>,
    operator_requests: Vec<HarnessOperatorRequestV1>,
    scheduled_launches: BTreeMap<HarnessOperationId, HarnessScheduledLaunchRefV1>,
    issued_launches: BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
}

impl std::fmt::Debug for HarnessCommittedSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("HarnessCommittedSnapshot")
            .field("engine", &self.engine)
            .field("dispatch_contexts", &self.dispatch_contexts)
            .finish_non_exhaustive()
    }
}

pub struct HarnessService {
    store: Option<HarnessStore>,
    engine: HarnessEngine,
    dispatch_contexts: BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    harness_mcp_reservations: BTreeMap<HarnessMcpReservationId, HarnessMcpReservationV1>,
    operator_requests: BTreeMap<HarnessOperationId, HarnessRequestDigest>,
    scheduled_launches: BTreeMap<HarnessOperationId, HarnessScheduledLaunchRefV1>,
    issued_launches: BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    poisoned: bool,
}

/// Single-use durable authority for exactly one typed C2 ContextPack export.
/// It is intentionally non-cloneable and is issued only after Prepared has
/// atomically transitioned to Exporting.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PreparedContinuationExport {
    continuation: HarnessContinuationV1,
    /// The source run's own durable receipt, when present — lets
    /// `start_context_pack_export` (c2.rs) resolve the pack via the durable
    /// `ResolveDurableContextPack` C2 verb instead of a live session export,
    /// with no second engine lookup.
    source_context_pack: Option<HarnessResolvedContextPackReceiptV1>,
}

impl PreparedContinuationExport {
    pub(crate) fn continuation(&self) -> &HarnessContinuationV1 {
        &self.continuation
    }

    pub(crate) fn source_context_pack(&self) -> Option<&HarnessResolvedContextPackReceiptV1> {
        self.source_context_pack.as_ref()
    }
}

impl HarnessService {
    #[cfg(test)]
    pub(crate) fn from_engine_for_test(engine: HarnessEngine) -> Self {
        Self {
            store: None,
            engine,
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, HarnessServiceError> {
        let store = HarnessStore::open(path)?;
        let (
            engine,
            dispatch_contexts,
            harness_mcp_reservations,
            operator_requests,
            scheduled_launches,
            issued_launches,
        ) = match store.load_checkpoint()? {
            Some(encoded) => {
                let mut checkpoint: HarnessServiceCheckpointV1 = serde_json::from_slice(&encoded)?;
                if checkpoint.version != HARNESS_SERVICE_CHECKPOINT_VERSION_V1 {
                    return Err(HarnessServiceError::UnsupportedCheckpoint(checkpoint.version));
                }
                let engine_deliveries = checkpoint.engine.deliveries.clone();
                let engine_continuations = checkpoint.engine.continuations.clone();
                if !engine_deliveries.is_empty()
                    && !checkpoint.deliveries.is_empty()
                    && engine_deliveries != checkpoint.deliveries
                {
                    return Err(HarnessServiceError::Corrupt(
                        "split delivery checkpoints disagree",
                    ));
                }
                if !engine_continuations.is_empty()
                    && !checkpoint.continuations.is_empty()
                    && engine_continuations != checkpoint.continuations
                {
                    return Err(HarnessServiceError::Corrupt(
                        "split continuation checkpoints disagree",
                    ));
                }
                let normalized_continuations = store.load_continuations()?;
                let checkpoint_continuations = if engine_continuations.is_empty() {
                    &checkpoint.continuations
                } else {
                    &engine_continuations
                };
                let expected_continuations = checkpoint_continuations.iter()
                    .map(|continuation| encode_entity(
                        continuation.continuation_ref.to_string(),
                        continuation.revision.get(),
                        continuation,
                    ))
                    .collect::<Result<Vec<_>, HarnessServiceError>>()?;
                if normalized_continuations != expected_continuations {
                    return Err(HarnessServiceError::Corrupt(
                        "normalized continuations disagree with checkpoint",
                    ));
                }
                if checkpoint.engine.continuations.is_empty() {
                    checkpoint.engine.continuations = checkpoint.continuations.clone();
                }
                let engine = HarnessEngine::restore(checkpoint.engine)?;
                let engine = if engine_deliveries.is_empty() {
                    engine.restore_deliveries(checkpoint.deliveries)?
                } else {
                    engine
                };
                for spec in engine.execution_specs() {
                    validate_service_execution_spec(spec).map_err(|_| {
                        HarnessServiceError::Corrupt(
                            "invalid task execution specification",
                        )
                    })?;
                }
                for spec in engine.task_execution_specs_v2() {
                    let issuance = engine.task_launch_issuance(&spec.task_id).ok_or(
                        HarnessServiceError::Corrupt(
                            "issued execution specification has no issuance",
                        ),
                    )?;
                    validate_service_issued_execution_spec(issuance, spec).map_err(|_| {
                        HarnessServiceError::Corrupt(
                            "invalid issued task execution specification",
                        )
                    })?;
                }
                for issuance in engine.task_launch_issuances() {
                    if engine.task_execution_spec_v2(&issuance.task_id).is_none() {
                        return Err(HarnessServiceError::Corrupt(
                            "task launch issuance has no execution specification",
                        ));
                    }
                }
                let mut contexts = BTreeMap::new();
                for context in checkpoint.dispatch_contexts {
                    context.validate()?;
                    validate_context_operation(&engine, &context)?;
                    if contexts.insert(context.operation_id.clone(), context).is_some() {
                        return Err(HarnessServiceError::Corrupt(
                            "duplicate dispatch context operation id",
                        ));
                    }
                }
                let normalized_reservations = store.load_harness_mcp_reservations()?;
                let expected_reservations = checkpoint.harness_mcp_reservations.iter()
                    .map(|reservation| encode_entity(
                        reservation.reservation_id.as_str().to_owned(),
                        reservation.revision.get(),
                        reservation,
                    ))
                    .collect::<Result<Vec<_>, HarnessServiceError>>()?;
                if normalized_reservations != expected_reservations {
                    return Err(HarnessServiceError::Corrupt(
                        "normalized harness MCP reservations disagree with checkpoint",
                    ));
                }
                let mut reservations = BTreeMap::new();
                for reservation in checkpoint.harness_mcp_reservations {
                    reservation.validate()?;
                    validate_reservation_durable_context(&engine, &contexts, &reservation, true)?;
                    if reservations.insert(reservation.reservation_id.clone(), reservation).is_some() {
                        return Err(HarnessServiceError::Corrupt(
                            "duplicate harness MCP reservation id",
                        ));
                    }
                }
                let mut operator_requests = BTreeMap::new();
                for request in checkpoint.operator_requests {
                    request.operation_id.validate()?;
                    request.request_digest.validate()?;
                    if engine.operation(&request.operation_id).is_none() {
                        return Err(HarnessServiceError::Corrupt(
                            "operator request operation is missing",
                        ));
                    }
                    if operator_requests.insert(
                        request.operation_id,
                        request.request_digest,
                    ).is_some() {
                        return Err(HarnessServiceError::Corrupt(
                            "duplicate operator request operation id",
                        ));
                    }
                }
                for (operation_id, scheduled) in &checkpoint.scheduled_launches {
                    operation_id.validate()?;
                    scheduled.validate().map_err(|_| HarnessServiceError::Corrupt(
                        "invalid scheduled launch reference",
                    ))?;
                    if engine.operation(operation_id).is_none() {
                        return Err(HarnessServiceError::Corrupt(
                            "scheduled launch operation is missing",
                        ));
                    }
                }
                for (operation_id, issuance) in &checkpoint.issued_launches {
                    let operation = engine.operation(operation_id).ok_or(
                        HarnessServiceError::Corrupt(
                            "issued launch operation is missing",
                        ),
                    )?;
                    let run = operation.run_id.as_ref().and_then(|run_id| engine.run(run_id))
                        .ok_or(HarnessServiceError::Corrupt(
                            "issued launch run is missing",
                        ))?;
                    validate_issued_launch_snapshot(issuance, run).map_err(|_| {
                        HarnessServiceError::Corrupt(
                            "issued launch snapshot does not match its run",
                        )
                    })?;
                }
                for run in engine.runs().filter(|run| run.binding.is_some()) {
                    let operation = engine.operation(&run.operation_id).ok_or(
                        HarnessServiceError::Corrupt(
                            "bound dispatch operation is missing",
                        ),
                    )?;
                    validate_authoritative_dispatch_binding(
                        &engine,
                        &contexts,
                        &checkpoint.issued_launches,
                        run,
                        operation,
                    ).map_err(|_| HarnessServiceError::Corrupt(
                        "bound dispatch binding is not authoritative",
                    ))?;
                }
                (
                    engine,
                    contexts,
                    reservations,
                    operator_requests,
                    checkpoint.scheduled_launches,
                    checkpoint.issued_launches,
                )
            }
            None => (
                HarnessEngine::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
            ),
        };
        Ok(Self {
            store: Some(store),
            engine,
            dispatch_contexts,
            harness_mcp_reservations,
            operator_requests,
            scheduled_launches,
            issued_launches,
            poisoned: false,
        })
    }

    pub fn engine(&self) -> &HarnessEngine {
        &self.engine
    }

    pub fn operator_create_task(
        &mut self,
        request: HarnessCreateTaskRequestV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        if !matches!(request.initial_state, HarnessTaskStateV1::Backlog | HarnessTaskStateV1::Ready) {
            return Err(HarnessServiceError::InvalidOperatorTaskTransition {
                from: request.initial_state,
                to: request.initial_state,
            });
        }
        let command_digest = operator_command_digest("create-task", &request)?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::CreateTask,
        )? {
            return Ok(outcome);
        }
        let actor = operator_actor(&request.authority);
        let task = HarnessTaskV1 {
            task_id: request.task_id.clone(),
            revision: HarnessRevision::new(1)?,
            title: request.title,
            body: request.body,
            creator: actor.clone(),
            parent_task_id: request.parent_task_id,
            dependencies: request.dependencies,
            state: request.initial_state,
            run_ids: Vec::new(),
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: request.authority.now_unix_ms,
            updated_at_unix_ms: request.authority.now_unix_ms,
        };
        let operation = operator_task_operation(
            &request.authority,
            actor,
            HarnessOperationKindV1::CreateTask,
            task.task_id.clone(),
            None,
        )?;
        self.commit_operator_mutation(
            HarnessMutationV1::CreateTask { operation, task },
            command_digest,
        )
    }

    pub fn operator_replace_task(
        &mut self,
        request: HarnessReplaceTaskRequestV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest("replace-task", &request)?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::MutateTask,
        )? {
            return Ok(outcome);
        }
        let mut task = self.engine.task(&request.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(request.task_id.to_string()))?
            .clone();
        task.revision = next_harness_revision(task.revision, "task")?;
        task.title = request.title;
        task.body = request.body;
        task.parent_task_id = request.parent_task_id;
        task.dependencies = request.dependencies;
        task.updated_at_unix_ms = request.authority.now_unix_ms;
        let operation = operator_task_operation(
            &request.authority,
            operator_actor(&request.authority),
            HarnessOperationKindV1::MutateTask,
            task.task_id.clone(),
            Some(request.expected_revision),
        )?;
        self.commit_operator_mutation(
            HarnessMutationV1::ReplaceTask {
                operation,
                expected_revision: request.expected_revision,
                task,
            },
            command_digest,
        )
    }

    pub fn operator_move_task(
        &mut self,
        request: HarnessMoveTaskRequestV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest("move-task", &request)?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::MutateTask,
        )? {
            return Ok(outcome);
        }
        let current = self.engine.task(&request.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(request.task_id.to_string()))?;
        validate_operator_move(current.state, request.state)?;
        if request.state != HarnessTaskStateV1::Waiting
            && self.scheduler_task_has_nonterminal_run(current)?
        {
            return Err(HarnessServiceError::TaskHasActiveRun);
        }
        self.operator_replace_task_state(
            &request.authority,
            request.task_id,
            request.expected_revision,
            request.state,
            command_digest,
        )
    }

    pub fn operator_cancel_task(
        &mut self,
        request: HarnessCancelTaskRequestV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest("cancel-task", &request)?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::MutateTask,
        )? {
            return Ok(outcome);
        }
        let current = self.engine.task(&request.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(request.task_id.to_string()))?;
        if matches!(current.state, HarnessTaskStateV1::Done | HarnessTaskStateV1::Cancelled) {
            return Err(HarnessServiceError::InvalidOperatorTaskTransition {
                from: current.state,
                to: HarnessTaskStateV1::Cancelled,
            });
        }
        if self.scheduler_task_has_nonterminal_run(current)? {
            return Err(HarnessServiceError::TaskHasActiveRun);
        }
        self.operator_replace_task_state(
            &request.authority,
            request.task_id,
            request.expected_revision,
            HarnessTaskStateV1::Cancelled,
            command_digest,
        )
    }

    pub fn operator_retry_task(
        &mut self,
        request: HarnessRetryTaskRequestV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest("retry-task", &request)?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::MutateTask,
        )? {
            return Ok(outcome);
        }
        let current = self.engine.task(&request.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(request.task_id.to_string()))?;
        if !matches!(current.state, HarnessTaskStateV1::Failed | HarnessTaskStateV1::Cancelled) {
            return Err(HarnessServiceError::InvalidOperatorTaskTransition {
                from: current.state,
                to: HarnessTaskStateV1::Ready,
            });
        }
        if self.scheduler_task_has_nonterminal_run(current)? {
            return Err(HarnessServiceError::TaskHasActiveRun);
        }
        self.operator_replace_task_state(
            &request.authority,
            request.task_id,
            request.expected_revision,
            HarnessTaskStateV1::Ready,
            command_digest,
        )
    }

    /// The operator's own ack (`docs/gate4agent/plans/
    /// gate4agent-arc-mailbox-and-task-layer-2026-09-02.md` §3 Slice B): an
    /// operator act, applied through the identical `HarnessMutationV1::
    /// AckMail` mutation an agent's own `g4a_mail_ack` uses. `AckMail`'s own
    /// authority is a `mail`-permitted `SessionGrantV1`, not a bare operator
    /// identity (`HarnessOperationKindV1::AckMail`'s `validate_kind_targets`
    /// requires `grant_id: Some(_)`) -- this resolves the most recently
    /// created active, mail-permitted grant currently bound to
    /// `request.record_id` (the record's own run, whichever one that is) to
    /// supply it, while the operation's own `actor` records the operator,
    /// not that grant's run, so the audit trail names who actually acted.
    pub fn operator_ack_mail(
        &mut self,
        request: gate4agent_harness_api::HarnessMailAckOperatorRequestV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest("ack-mail", &request)?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::AckMail,
        )? {
            return Ok(outcome);
        }
        let grant_id = mail_grant_for_record(&self.engine, &request.record_id)
            .ok_or_else(|| HarnessEngineError::NotFound(request.record_id.as_str().to_owned()))?;
        let operation = mail_operation(
            &grant_id,
            HarnessActorV1::User { actor_id: request.authority.actor_id.clone() },
            HarnessOperationKindV1::AckMail,
            request.authority.now_unix_ms,
        )?;
        let ack = gate4agent_harness_protocol::HarnessMailAckV1 {
            message_id: request.message_id,
            reader_record_id: request.record_id,
            acked_at_unix_ms: request.authority.now_unix_ms,
        };
        self.commit_operator_mutation(HarnessMutationV1::AckMail { operation, ack }, command_digest)
    }

    pub fn task_execution_spec(
        &self,
        task_id: &HarnessTaskId,
    ) -> Option<&HarnessTaskExecutionSpecV1> {
        self.engine.execution_spec(task_id)
    }

    pub fn task_execution_spec_v2(
        &self,
        task_id: &HarnessTaskId,
    ) -> Option<&HarnessTaskExecutionSpecV2> {
        self.engine.task_execution_spec_v2(task_id)
    }

    pub(crate) fn operator_replace_task_execution_spec_v2(
        &mut self,
        catalogue: &HarnessFullTaskLaunchCatalogueV1,
        request: HarnessReplaceTaskExecutionSpecRequestV2,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate().map_err(|_| HarnessServiceError::InvalidTaskLaunchSelection {
            task_id: request.task_id.clone(),
            plan_id: Some(request.selection.plan.plan.plan_id.clone()),
            why: "replace-task-execution-spec-v2 request failed structural validation",
        })?;
        let command_digest = operator_command_digest(
            "replace-task-execution-spec-v2",
            &request,
        )?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::MutateExecutionSpec,
        )? {
            return Ok(outcome);
        }
        let task = self.engine.task(&request.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(request.task_id.to_string()))?;
        if !matches!(task.state, HarnessTaskStateV1::Backlog | HarnessTaskStateV1::Ready)
            || self.scheduler_task_has_nonterminal_run(task)?
        {
            return Err(HarnessServiceError::InvalidTaskLaunchSelection {
                task_id: request.task_id.clone(),
                plan_id: Some(request.selection.plan.plan.plan_id.clone()),
                why: "task is not in Backlog/Ready, or has a nonterminal run",
            });
        }
        validate_current_launch_options(catalogue, &request)?;
        let current_spec = self.engine.task_execution_spec_v2(&request.task_id);
        let current_issuance = self.engine.task_launch_issuance(&request.task_id);
        let (revision, created_at_unix_ms) = match (
            request.expected_execution_spec_revision,
            current_spec,
            current_issuance,
        ) {
            (HarnessExpectedExecutionSpecRevisionV1::Absent, None, None) => (
                HarnessRevision::new(1)?,
                request.authority.now_unix_ms,
            ),
            (
                HarnessExpectedExecutionSpecRevisionV1::Exact(expected),
                Some(spec),
                Some(issuance),
            ) if spec.revision == expected && issuance.revision == expected => (
                next_harness_revision(expected, "issued execution spec")?,
                spec.created_at_unix_ms,
            ),
            (_, spec, issuance) => {
                return Err(HarnessServiceError::IssuedExecutionCasMismatch {
                    expected: request.expected_execution_spec_revision,
                    spec: spec.map(|record| record.revision),
                    issuance: issuance.map(|record| record.revision),
                });
            }
        };
        if self.engine.execution_spec(&request.task_id).is_some() {
            return Err(HarnessServiceError::ExecutionSpecLaunchMismatch);
        }
        let worktree = match &request.selection.worktree {
            HarnessReviewedWorktreeSelectionV1::Existing => {
                HarnessLaunchWorktreeSelectionV1::Existing
            }
            HarnessReviewedWorktreeSelectionV1::Managed { profile } => {
                HarnessLaunchWorktreeSelectionV1::Managed {
                    profile_id: profile.profile_id.clone(),
                    expected_profile_revision: profile.profile_revision.clone(),
                }
            }
        };
        let mut issuance = HarnessTaskLaunchIssuanceV1 {
            issuance_id: deterministic_launch_issuance_id(&request.task_id)?,
            revision,
            digest: HarnessRequestDigest::new("0".repeat(64))?,
            task_id: request.task_id.clone(),
            task_revision: request.expected_task_revision,
            plan: request.selection.plan.plan.clone(),
            target: HarnessLaunchTargetSelectionV1 {
                node_id: request.selection.plan.node_id.clone(),
                source_workspace_id: request.selection.plan.source_workspace_id.clone(),
                worktree,
                provider_profile: request.selection.plan.provider_profile.clone(),
                mode: request.selection.plan.mode,
            },
            context_source: request.selection.context_source.clone(),
            delivery: request.selection.delivery.clone(),
            policy_digest: catalogue.policy_digest.clone(),
            created_at_unix_ms,
            updated_at_unix_ms: request.authority.now_unix_ms,
        };
        issuance.digest = task_launch_issuance_digest(&issuance)?;
        let spec = HarnessTaskExecutionSpecV2 {
            execution_spec_id: deterministic_execution_spec_id(&request.task_id)?,
            revision,
            task_id: request.task_id.clone(),
            launch_issuance: issuance.reference(),
            review_policy: request.selection.review_policy,
            created_at_unix_ms,
            updated_at_unix_ms: request.authority.now_unix_ms,
        };
        validate_service_issued_execution_spec(&issuance, &spec)?;
        let operation = operator_task_operation(
            &request.authority,
            operator_actor(&request.authority),
            HarnessOperationKindV1::MutateExecutionSpec,
            request.task_id,
            Some(request.expected_task_revision),
        )?;
        self.commit_operator_mutation(
            HarnessMutationV1::PutIssuedExecutionSpec {
                operation,
                expected_task_revision: request.expected_task_revision,
                expected_spec_revision: request.expected_execution_spec_revision,
                expected_issuance_revision: request.expected_execution_spec_revision,
                issuance,
                spec,
            },
            command_digest,
        )
    }

    pub(crate) fn start_task_v2(
        &mut self,
        catalog: &HarnessLaunchCatalog,
        catalogue: &HarnessFullTaskLaunchCatalogueV1,
        request: HarnessStartTaskRequestV2,
    ) -> Result<HarnessTaskStartOutcomeV1, HarnessServiceError> {
        self.ensure_healthy()?;
        if request.validate().is_err() {
            return Err(HarnessServiceError::InvalidTaskLaunchSelection {
                task_id: request.task_id.clone(),
                plan_id: None,
                why: "start-task-v2 request failed structural validation",
            });
        }
        let command_digest = operator_command_digest("start-task-v2", &request)?;
        if self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::CreateRun,
        )?.is_some() {
            return Ok(HarnessTaskStartOutcomeV1 {
                dispatch: self.replayed_dispatch_intent_for_operation(
                    &request.authority.operation_id,
                )?,
                replayed: true,
            });
        }
        let issuance = self.engine.task_launch_issuance(&request.task_id)
            .ok_or(HarnessServiceError::ExecutionSpecMissing)?
            .clone();
        let spec = self.engine.task_execution_spec_v2(&request.task_id)
            .ok_or(HarnessServiceError::ExecutionSpecMissing)?
            .clone();
        validate_service_issued_execution_spec(&issuance, &spec)?;
        validate_current_issued_launch_options(catalogue, &issuance)?;
        if spec.revision != request.expected_execution_spec_revision
            || issuance.revision != request.expected_execution_spec_revision
            || issuance.reference() != request.expected_launch_issuance
            || issuance.task_revision != request.expected_task_revision
        {
            return Err(HarnessServiceError::IssuedExecutionCasMismatch {
                expected: HarnessExpectedExecutionSpecRevisionV1::Exact(
                    request.expected_execution_spec_revision,
                ),
                spec: Some(spec.revision),
                issuance: Some(issuance.revision),
            });
        }
        if self.scheduler_pending_dispatch()?.is_some() {
            return Err(HarnessServiceError::SchedulerBusy);
        }
        let current_task = self.engine.scheduler_ready_task_by_id(&request.task_id)
            .map_err(map_scheduler_error)?
            .ok_or(HarnessServiceError::TaskNotReady)?
            .clone();
        if current_task.revision != request.expected_task_revision
            || current_task.creator != operator_actor(&request.authority)
        {
            return Err(HarnessServiceError::InvalidTaskLaunchSelection {
                task_id: request.task_id.clone(),
                plan_id: Some(issuance.plan.plan_id.clone()),
                why: "current ready task revision or creator no longer matches the issued launch",
            });
        }
        let plan = catalog.resolve(&issuance.plan)?;
        if !plan.is_ordinary_dispatch()
            || !matches!(plan.grant, HarnessGrantPolicyV1::Operator)
            || plan.node_id != issuance.target.node_id
            || plan.workspace_id != issuance.target.source_workspace_id
            || plan.provider_profile != issuance.target.provider_profile
            || plan.mode != issuance.target.mode
        {
            return Err(HarnessServiceError::InvalidTaskLaunchSelection {
                task_id: request.task_id.clone(),
                plan_id: Some(issuance.plan.plan_id.clone()),
                why: "resolved launch plan no longer matches the issued target",
            });
        }
        let scheduled = plan.scheduled_ref()?;
        let ids = dispatch::deterministic_issued_dispatch_ids(
            &request.authority.operation_id,
            issuance.delivery.is_some(),
            issuance.context_source.is_some(),
        )?;
        let worktree = match &issuance.target.worktree {
            HarnessLaunchWorktreeSelectionV1::Existing => HarnessWorktreeIntentV1::Existing,
            HarnessLaunchWorktreeSelectionV1::Managed {
                profile_id,
                expected_profile_revision,
            } => HarnessWorktreeIntentV1::ManagedProfile {
                profile_id: profile_id.clone(),
                expected_profile_revision: expected_profile_revision.clone(),
            },
        };
        let mut task = current_task.clone();
        task.revision = next_harness_revision(task.revision, "task")?;
        task.state = HarnessTaskStateV1::Running;
        task.updated_at_unix_ms = request.authority.now_unix_ms;
        task.run_ids.push(ids.run_id.clone());
        task.run_ids.sort();
        let intent = gate4agent_harness_protocol::HarnessRunIntentV1 {
            node_id: issuance.target.node_id.clone(),
            workspace_id: issuance.target.source_workspace_id.clone(),
            worktree,
            provider_profile: issuance.target.provider_profile.clone(),
            mode: issuance.target.mode,
            delivery_bundle: issuance.delivery.as_ref()
                .map(|selection| selection.bundle.selector.clone()),
            continuation: issuance.context_source.as_ref()
                .map(|source| HarnessSelectorV1::new(source.source_run_id.as_str()))
                .transpose()?,
        };
        let run = HarnessRunV1 {
            run_id: ids.run_id.clone(),
            revision: HarnessRevision::new(1)?,
            parent_run_id: None,
            task_id: task.task_id.clone(),
            operation_id: request.authority.operation_id.clone(),
            intent,
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Requested,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: request.authority.now_unix_ms,
            updated_at_unix_ms: request.authority.now_unix_ms,
        };
        let authority = HarnessTransferAuthorityRefV1::OperatorIssuance {
            issuance: issuance.reference(),
        };
        let delivery = match (&issuance.delivery, ids.delivery_ref) {
            (Some(selection), Some(delivery_ref)) => Some(HarnessDeliveryV1 {
                delivery_ref,
                revision: HarnessRevision::new(1)?,
                authority: authority.clone(),
                task_id: task.task_id.clone(),
                run_id: run.run_id.clone(),
                operation_id: request.authority.operation_id.clone(),
                bundle: selection.bundle.clone(),
                state: HarnessDeliveryStateV1::Prepared,
                stage_receipt: None,
                receipt: None,
                created_at_unix_ms: request.authority.now_unix_ms,
                updated_at_unix_ms: request.authority.now_unix_ms,
            }),
            (None, None) => None,
            _ => return Err(HarnessServiceError::InvalidTaskLaunchSelection {
                task_id: request.task_id.clone(),
                plan_id: Some(issuance.plan.plan_id.clone()),
                why: "delivery selection shape does not match the issuance",
            }),
        };
        let continuation = match (
            &issuance.context_source,
            ids.continuation_ref,
            ids.continuation_receipt_ref,
        ) {
            (Some(source), Some(continuation_ref), Some(receipt_ref)) => {
                let source_run = self.engine.run(&source.source_run_id)
                    .ok_or_else(|| HarnessServiceError::InvalidTaskLaunchSelection {
                        task_id: request.task_id.clone(),
                        plan_id: Some(issuance.plan.plan_id.clone()),
                        why: "context source run no longer exists",
                    })?;
                let source_binding = source_run.binding.as_ref()
                    .filter(|binding| source_binding_matches_context_selection(binding, source))
                    .ok_or_else(|| HarnessServiceError::InvalidTaskLaunchSelection {
                        task_id: request.task_id.clone(),
                        plan_id: Some(issuance.plan.plan_id.clone()),
                        why: "context source run binding no longer matches the context source",
                    })?;
                let source_context = self.dispatch_contexts.get(&source_run.operation_id)
                    .ok_or_else(|| HarnessServiceError::InvalidTaskLaunchSelection {
                        task_id: request.task_id.clone(),
                        plan_id: Some(issuance.plan.plan_id.clone()),
                        why: "context source dispatch context is missing",
                    })?;
                // For a Durable source, `source_binding.node_incarnation`
                // (the source run's own original spawn-accept binding) is
                // necessarily stale once the Node has restarted.
                // `HarnessContinuationV1::validate()`'s `exact_route`
                // invariant requires `source_binding.node_incarnation` to
                // equal the continuation's own `node_incarnation` set below
                // (the Node's CURRENT incarnation, from
                // `source.node_incarnation` — see `context_source_option`),
                // and the eventual `target_binding` will carry that same
                // current incarnation once run B actually spawns. Record the
                // current incarnation here instead of copying the stale one
                // verbatim; every other field of the binding — node_id,
                // workspace_id, and the managed session identity that
                // actually identifies the source — is unchanged.
                let source_binding = if source.availability
                    == gate4agent_harness_protocol::HarnessContextSourceAvailabilityV1::Durable
                {
                    gate4agent_harness_protocol::HarnessSessionBindingV1 {
                        node_incarnation: source.node_incarnation.clone(),
                        ..source_binding.clone()
                    }
                } else {
                    source_binding.clone()
                };
                Some(HarnessContinuationV1 {
                    continuation_ref,
                    receipt_ref,
                    revision: HarnessRevision::new(1)?,
                    state: HarnessContinuationStateV1::Prepared,
                    authority: authority.clone(),
                    source_run_id: source.source_run_id.clone(),
                    target_run_id: run.run_id.clone(),
                    operation_id: request.authority.operation_id.clone(),
                    node_id: source.node_id.clone(),
                    node_incarnation: source.node_incarnation.clone(),
                    workspace_id: source.workspace_id.clone(),
                    source_provider: source_context.expected_provider.clone(),
                    source_binding,
                    context: None,
                    target_binding: None,
                    prepared_at_unix_ms: request.authority.now_unix_ms,
                    exporting_at_unix_ms: None,
                    exported_at_unix_ms: None,
                    bound_at_unix_ms: None,
                    expired_at_unix_ms: None,
                    outcome_unknown_at_unix_ms: None,
                    outcome_unknown_reason: None,
                    cleanup_state: HarnessContinuationCleanupStateV1::Retained,
                    created_at_unix_ms: request.authority.now_unix_ms,
                    updated_at_unix_ms: request.authority.now_unix_ms,
                })
            }
            (None, None, None) => None,
            _ => return Err(HarnessServiceError::InvalidTaskLaunchSelection {
                task_id: request.task_id.clone(),
                plan_id: Some(issuance.plan.plan_id.clone()),
                why: "continuation selection shape does not match the issuance",
            }),
        };
        let mut operation = HarnessOperationV1 {
            operation_id: request.authority.operation_id.clone(),
            revision: HarnessRevision::new(1)?,
            actor: current_task.creator.clone(),
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Prepared,
            task_id: Some(task.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(current_task.revision),
            request_digest: HarnessRequestDigest::new("0".repeat(64))?,
            idempotency_ref: request.authority.idempotency_ref.clone(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: request.authority.now_unix_ms,
            updated_at_unix_ms: request.authority.now_unix_ms,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: None,
        };
        let mut mutation = HarnessMutationV1::CreateIssuedRun {
            operation: operation.clone(),
            expected_task_revision: request.expected_task_revision,
            expected_execution_spec_revision: request.expected_execution_spec_revision,
            expected_issuance_revision: request.expected_launch_issuance.revision,
            task,
            run,
            delivery,
            continuation,
        };
        operation.request_digest = mutation_request_digest(&mutation)?;
        *mutation.operation_mut() = operation;
        let prepared = self.engine.prepare(mutation)?;
        let dispatch = dispatch_intent_from_prepared(&prepared)?;
        self.commit_operator_prepared_with_launch_and_issuance(
            prepared,
            command_digest,
            Some(scheduled),
            Some(issuance),
        )?;
        Ok(HarnessTaskStartOutcomeV1 { dispatch, replayed: false })
    }

    pub fn operator_replace_task_execution_spec(
        &mut self,
        catalog: &HarnessLaunchCatalog,
        request: HarnessReplaceTaskExecutionSpecRequestV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest("replace-task-execution-spec", &request)?;
        if let Some(outcome) = self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::MutateExecutionSpec,
        )? {
            return Ok(outcome);
        }
        let plan = catalog.resolve_ordinary_scheduled(&request.spec.scheduled_launch)?;
        let scheduled_launch_digest = scheduled_launch_digest(
            &request.spec.scheduled_launch,
        )?;
        let current = self.engine.execution_spec(&request.task_id);
        let (revision, created_at_unix_ms) = match (
            request.expected_execution_spec_revision,
            current,
        ) {
            (HarnessExpectedExecutionSpecRevisionV1::Absent, None) => (
                HarnessRevision::new(1)?,
                request.authority.now_unix_ms,
            ),
            (HarnessExpectedExecutionSpecRevisionV1::Exact(expected), Some(current)) => (
                next_harness_revision(expected, "execution spec")?,
                current.created_at_unix_ms,
            ),
            (HarnessExpectedExecutionSpecRevisionV1::Absent, Some(current)) => {
                return Err(HarnessServiceError::ExecutionSpecRevisionMismatch {
                    expected: None,
                    actual: Some(current.revision),
                });
            }
            (HarnessExpectedExecutionSpecRevisionV1::Exact(expected), current) => {
                return Err(HarnessServiceError::ExecutionSpecRevisionMismatch {
                    expected: Some(expected),
                    actual: current.map(|spec| spec.revision),
                });
            }
        };
        if plan.ordinary_scheduled_ref()? != request.spec.scheduled_launch {
            return Err(HarnessServiceError::ExecutionSpecLaunchMismatch);
        }
        let spec = HarnessTaskExecutionSpecV1 {
            execution_spec_id: deterministic_execution_spec_id(&request.task_id)?,
            revision,
            task_id: request.task_id.clone(),
            scheduled_launch: request.spec.scheduled_launch,
            scheduled_launch_digest,
            review_policy: request.spec.review_policy,
            created_at_unix_ms,
            updated_at_unix_ms: request.authority.now_unix_ms,
        };
        validate_service_execution_spec(&spec)?;
        let operation = operator_task_operation(
            &request.authority,
            operator_actor(&request.authority),
            HarnessOperationKindV1::MutateExecutionSpec,
            request.task_id,
            Some(request.expected_task_revision),
        )?;
        self.commit_operator_mutation(
            HarnessMutationV1::PutExecutionSpec {
                operation,
                expected_task_revision: request.expected_task_revision,
                expected_spec_revision: request.expected_execution_spec_revision,
                spec,
            },
            command_digest,
        )
    }

    pub(crate) fn start_task(
        &mut self,
        catalog: &HarnessLaunchCatalog,
        request: HarnessStartTaskRequestV1,
    ) -> Result<HarnessTaskStartOutcomeV1, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest("start-task", &request)?;
        if self.replay_operator_request(
            &request.authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::CreateRun,
        )?.is_some() {
            return Ok(HarnessTaskStartOutcomeV1 {
                dispatch: self.replayed_dispatch_intent_for_operation(
                    &request.authority.operation_id,
                )?,
                replayed: true,
            });
        }
        let spec = self.engine.execution_spec(&request.task_id)
            .ok_or(HarnessServiceError::ExecutionSpecMissing)?
            .clone();
        validate_service_execution_spec(&spec)?;
        if spec.revision != request.expected_execution_spec_revision {
            return Err(HarnessServiceError::ExecutionSpecRevisionMismatch {
                expected: Some(request.expected_execution_spec_revision),
                actual: Some(spec.revision),
            });
        }
        if spec.scheduled_launch_digest != request.expected_scheduled_launch_digest {
            return Err(HarnessServiceError::ExecutionSpecLaunchMismatch);
        }
        let plan = catalog.resolve_ordinary_scheduled(&spec.scheduled_launch)?;
        if self.scheduler_pending_dispatch()?.is_some() {
            return Err(HarnessServiceError::SchedulerBusy);
        }
        let task = self.engine.scheduler_ready_task_by_id(&request.task_id)
            .map_err(map_scheduler_error)?
            .ok_or(HarnessServiceError::TaskNotReady)?
            .clone();
        if task.revision != request.expected_task_revision {
            return Err(HarnessServiceError::Engine(
                HarnessEngineError::ExpectedRevisionMismatch {
                    entity: "task",
                    expected: request.expected_task_revision.get(),
                    actual: Some(task.revision.get()),
                },
            ));
        }
        let (schedule_request, scheduled) = derive_schedule_request(
            plan,
            &task,
            &request.authority,
        )?;
        let dispatch = self.schedule_exact_task_with_launch(
            schedule_request,
            request.task_id,
            scheduled,
            command_digest,
        )?;
        Ok(HarnessTaskStartOutcomeV1 { dispatch, replayed: false })
    }

    fn schedule_exact_task_with_launch(
        &mut self,
        request: HarnessScheduleRequestV1,
        task_id: HarnessTaskId,
        scheduled: HarnessScheduledLaunchRefV1,
        command_digest: HarnessRequestDigest,
    ) -> Result<HarnessDispatchIntentV1, HarnessServiceError> {
        request.validate()?;
        scheduled.validate().map_err(|_| HarnessServiceError::Corrupt(
            "invalid exact scheduled launch reference",
        ))?;
        let current_task = self.engine.scheduler_ready_task_by_id(&task_id)
            .map_err(map_scheduler_error)?
            .ok_or(HarnessServiceError::TaskNotReady)?
            .clone();
        let mut task = current_task.clone();
        task.revision = next_harness_revision(task.revision, "task")?;
        task.state = HarnessTaskStateV1::Running;
        task.updated_at_unix_ms = request.now_unix_ms;
        task.run_ids.push(request.run_id.clone());
        task.run_ids.sort();
        let mut operation = HarnessOperationV1 {
            operation_id: request.operation_id.clone(),
            revision: HarnessRevision::new(1)?,
            actor: request.actor.clone(),
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Prepared,
            task_id: Some(task.task_id.clone()),
            run_id: Some(request.run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(current_task.revision),
            request_digest: HarnessRequestDigest::new("0".repeat(64))?,
            idempotency_ref: request.idempotency_ref.clone(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: request.now_unix_ms,
            updated_at_unix_ms: request.now_unix_ms,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: None,
        };
        let run = HarnessRunV1 {
            run_id: request.run_id,
            revision: HarnessRevision::new(1)?,
            parent_run_id: request.parent_run_id,
            task_id: task.task_id.clone(),
            operation_id: operation.operation_id.clone(),
            intent: request.intent,
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Requested,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: request.now_unix_ms,
            updated_at_unix_ms: request.now_unix_ms,
        };
        let mut mutation = HarnessMutationV1::CreateRun {
            operation: operation.clone(),
            expected_task_revision: current_task.revision,
            task,
            run,
        };
        operation.request_digest = mutation_request_digest(&mutation)?;
        *mutation.operation_mut() = operation;
        let prepared = self.engine.prepare(mutation)?;
        let intent = dispatch_intent_from_prepared(&prepared)?;
        self.commit_operator_prepared_with_launch(
            prepared,
            command_digest,
            Some(scheduled),
        )?;
        Ok(intent)
    }

    pub(crate) fn schedule_ready_task(
        &mut self,
        request: HarnessScheduleRequestV1,
    ) -> Result<HarnessScheduleOutcomeV1, HarnessServiceError> {
        self.schedule_ready_task_inner(request, None)
    }

    pub(crate) fn schedule_ready_task_with_launch(
        &mut self,
        request: HarnessScheduleRequestV1,
        scheduled: HarnessScheduledLaunchRefV1,
    ) -> Result<HarnessScheduleOutcomeV1, HarnessServiceError> {
        scheduled.validate().map_err(|_| HarnessServiceError::Corrupt(
            "invalid scheduled launch reference",
        ))?;
        self.schedule_ready_task_inner(request, Some(scheduled))
    }

    pub(crate) fn schedule_next(
        &mut self,
        catalog: &HarnessLaunchCatalog,
        authority: HarnessOperatorAuthorityV1,
        plan_id: Option<&HarnessSelectorV1>,
    ) -> Result<HarnessScheduleOutcomeV1, HarnessServiceError> {
        self.ensure_healthy()?;
        authority.validate()?;
        if self.operator_requests.contains_key(&authority.operation_id) {
            return self.replay_schedule_next(catalog, &authority, plan_id);
        }
        if let Some((task, run, operation)) = self.scheduler_pending_dispatch()? {
            let scheduled = self.scheduled_launches.get(&operation.operation_id)
                .ok_or(HarnessServiceError::Corrupt(
                    "production pending dispatch has no scheduled launch reference",
                ))?;
            catalog.resolve_scheduled(scheduled)?;
            return Ok(HarnessScheduleOutcomeV1::Dispatch(
                dispatch_intent(task, run, operation)?,
            ));
        }
        let Some(task) = self.scheduler_ready_task()?.cloned() else {
            return Ok(HarnessScheduleOutcomeV1::Idle);
        };
        let plan = catalog.select(plan_id)?;
        let (request, scheduled) = derive_schedule_request(plan, &task, &authority)?;
        self.schedule_ready_task_with_launch(request, scheduled)
    }

    fn replay_schedule_next(
        &self,
        catalog: &HarnessLaunchCatalog,
        authority: &HarnessOperatorAuthorityV1,
        plan_id: Option<&HarnessSelectorV1>,
    ) -> Result<HarnessScheduleOutcomeV1, HarnessServiceError> {
        let scheduled = self.scheduled_launches.get(&authority.operation_id)
            .ok_or_else(|| HarnessServiceError::OperatorRequestConflict {
                operation_id: authority.operation_id.clone(),
            })?;
        if plan_id.is_some_and(|plan_id| plan_id != &scheduled.plan.plan_id)
            || catalog.resolve_scheduled(scheduled).is_err()
        {
            return Err(HarnessServiceError::OperatorRequestConflict {
                operation_id: authority.operation_id.clone(),
            });
        }
        let operation = self.engine.operation(&authority.operation_id)
            .ok_or(HarnessServiceError::Corrupt("operator request operation is missing"))?;
        let run = operation.run_id.as_ref().and_then(|run_id| self.engine.run(run_id))
            .ok_or(HarnessServiceError::SchedulerInvalidGraph(
                "scheduler replay run is missing",
            ))?;
        let request = HarnessScheduleRequestV1 {
            operation_id: authority.operation_id.clone(),
            idempotency_ref: authority.idempotency_ref.clone(),
            actor: operation.actor.clone(),
            run_id: run.run_id.clone(),
            parent_run_id: run.parent_run_id.clone(),
            intent: run.intent.clone(),
            now_unix_ms: authority.now_unix_ms,
        };
        request.validate()?;
        let command_digest = operator_command_digest(
            "schedule-ready-task-v2",
            &(&request, &Some(scheduled.clone())),
        )?;
        self.replay_operator_request(
            &authority.operation_id,
            &command_digest,
            HarnessOperationKindV1::CreateRun,
        )?;
        Ok(HarnessScheduleOutcomeV1::Dispatch(
            self.replayed_dispatch_intent_for_operation(&authority.operation_id)?,
        ))
    }

    fn schedule_ready_task_inner(
        &mut self,
        request: HarnessScheduleRequestV1,
        scheduled: Option<HarnessScheduledLaunchRefV1>,
    ) -> Result<HarnessScheduleOutcomeV1, HarnessServiceError> {
        self.ensure_healthy()?;
        request.validate()?;
        let command_digest = operator_command_digest(
            "schedule-ready-task-v2",
            &(&request, &scheduled),
        )?;
        if self.operator_requests.contains_key(&request.operation_id) {
            if self.scheduled_launches.get(&request.operation_id) != scheduled.as_ref() {
                return Err(HarnessServiceError::OperatorRequestConflict {
                    operation_id: request.operation_id,
                });
            }
            self.replay_operator_request(
                &request.operation_id,
                &command_digest,
                HarnessOperationKindV1::CreateRun,
            )?;
            return Ok(HarnessScheduleOutcomeV1::Dispatch(
                self.replayed_dispatch_intent_for_operation(&request.operation_id)?,
            ));
        }
        if self.engine.operation(&request.operation_id).is_some() {
            return Err(HarnessServiceError::OperatorRequestConflict {
                operation_id: request.operation_id,
            });
        }
        if let Some((task, run, operation)) = self.scheduler_pending_dispatch()? {
            if scheduled.is_some() && !self.scheduled_launches.contains_key(&operation.operation_id) {
                return Err(HarnessServiceError::Corrupt(
                    "production pending dispatch has no scheduled launch reference",
                ));
            }
            return Ok(HarnessScheduleOutcomeV1::Dispatch(dispatch_intent(task, run, operation)?));
        }
        let Some(current_task) = self.scheduler_ready_task()?.cloned() else {
            return Ok(HarnessScheduleOutcomeV1::Idle);
        };
        let mut task = current_task.clone();
        task.revision = next_harness_revision(task.revision, "task")?;
        task.state = HarnessTaskStateV1::Running;
        task.updated_at_unix_ms = request.now_unix_ms;
        task.run_ids.push(request.run_id.clone());
        task.run_ids.sort();
        let mut operation = HarnessOperationV1 {
            operation_id: request.operation_id.clone(),
            revision: HarnessRevision::new(1)?,
            actor: request.actor.clone(),
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Prepared,
            task_id: Some(task.task_id.clone()),
            run_id: Some(request.run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(current_task.revision),
            request_digest: HarnessRequestDigest::new("0".repeat(64))?,
            idempotency_ref: request.idempotency_ref.clone(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: request.now_unix_ms,
            updated_at_unix_ms: request.now_unix_ms,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: None,
        };
        let run = HarnessRunV1 {
            run_id: request.run_id,
            revision: HarnessRevision::new(1)?,
            parent_run_id: request.parent_run_id,
            task_id: task.task_id.clone(),
            operation_id: operation.operation_id.clone(),
            intent: request.intent,
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Requested,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: request.now_unix_ms,
            updated_at_unix_ms: request.now_unix_ms,
        };
        let mut mutation = HarnessMutationV1::CreateRun {
            operation: operation.clone(),
            expected_task_revision: current_task.revision,
            task,
            run,
        };
        operation.request_digest = mutation_request_digest(&mutation)?;
        *mutation.operation_mut() = operation;
        let prepared = self.engine.prepare(mutation)?;
        let intent = dispatch_intent_from_prepared(&prepared)?;
        self.commit_operator_prepared_with_launch(prepared, command_digest, scheduled)?;
        Ok(HarnessScheduleOutcomeV1::Dispatch(intent))
    }

    pub(crate) fn scheduled_launch(
        &self,
        operation_id: &HarnessOperationId,
    ) -> Option<&HarnessScheduledLaunchRefV1> {
        self.scheduled_launches.get(operation_id)
    }

    pub(crate) fn pending_scheduled_dispatch(
        &self,
    ) -> Result<Option<HarnessDispatchIntentV1>, HarnessServiceError> {
        let Some((task, run, operation)) = self.scheduler_pending_dispatch()? else {
            return Ok(None);
        };
        if !self.scheduled_launches.contains_key(&operation.operation_id) {
            return Err(HarnessServiceError::Corrupt(
                "production pending dispatch has no scheduled launch reference",
            ));
        }
        Ok(Some(dispatch_intent(task, run, operation)?))
    }

    pub(crate) fn prepare_scheduled_specialized_authorities(
        &mut self,
        launch_catalog: &HarnessLaunchCatalog,
        delivery_catalog: &DeliveryCatalogV2,
        operation_id: &HarnessOperationId,
        now_unix_ms: u64,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        let scheduled = self.scheduled_launches.get(operation_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext(
                "scheduled specialized authority has no durable launch reference",
            ))?;
        let plan = launch_catalog.resolve_scheduled(scheduled)?;
        let operation = self.engine.operation(operation_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext(
                "scheduled specialized operation is missing",
            ))?;
        let run = operation.run_id.as_ref()
            .and_then(|run_id| self.engine.run(run_id))
            .ok_or(HarnessServiceError::InvalidDispatchContext(
                "scheduled specialized run is missing",
            ))?;
        let task = self.engine.task(&run.task_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext(
                "scheduled specialized task is missing",
            ))?;
        plan.validate_intent(&run.intent)?;
        let (grant_id, grant_revision) = match &plan.grant {
            HarnessGrantPolicyV1::Exact { grant_id, revision } => (grant_id, *revision),
            HarnessGrantPolicyV1::Operator => return Err(
                dispatch::HarnessDispatchError::OperatorPrivilegedFlow.into(),
            ),
        };
        let grant = self.engine.grant(grant_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext(
                "scheduled exact grant is missing",
            ))?;
        let actor_run_id = match &operation.actor {
            HarnessActorV1::ParentRun { run_id } => run_id,
            HarnessActorV1::User { .. } => return Err(
                HarnessServiceError::InvalidDispatchContext(
                    "scheduled exact grant actor is not a parent run",
                ),
            ),
        };
        // Five independent reasons a scheduled launch may no longer be
        // dispatchable. They are checked one at a time so the refusal
        // names which one fired: collapsed into a single disjunction the
        // error said only "grant, route, or profile changed", which is
        // indistinguishable from a stale revision, a revoked grant, a
        // re-parented run and a workspace the grant never covered.
        if grant.revision != grant_revision {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled exact grant revision moved since the launch was scheduled",
            ));
        }
        if grant.state != gate4agent_harness_protocol::SessionGrantStateV1::Active {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled exact grant is no longer Active",
            ));
        }
        if grant.actor_run_id != *actor_run_id {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled exact grant belongs to a different actor run",
            ));
        }
        if run.parent_run_id.as_ref() != Some(actor_run_id) {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled run's parent is not the grant's actor run",
            ));
        }
        if !grant.allows_target(
            &run.intent.node_id,
            &run.intent.workspace_id,
            &run.intent.provider_profile,
            run.intent.mode,
        ) {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled exact grant does not cover the run's node, workspace, profile or mode",
            ));
        }
        let ids = dispatch::deterministic_dispatch_ids(operation_id, plan)?;
        let delivery = match (&plan.delivery, ids.delivery_ref.clone()) {
            (Some(policy), Some(delivery_ref)) => {
                let compiled = delivery_catalog.get(&policy.bundle_id)
                    .ok_or_else(|| dispatch::HarnessDispatchError::DeliveryBundleMissing(
                        policy.bundle_id.clone(),
                    ))?;
                Some(HarnessDeliveryV1 {
                    delivery_ref,
                    revision: HarnessRevision::new(1)?,
                    authority: HarnessTransferAuthorityRefV1::ParentGrant {
                        grant_id: grant_id.clone(),
                        revision: grant_revision,
                    },
                    task_id: task.task_id.clone(),
                    run_id: run.run_id.clone(),
                    operation_id: operation_id.clone(),
                    bundle: delivery::compiled_bundle_identity(
                        policy.selector.clone(),
                        compiled,
                    )?,
                    state: HarnessDeliveryStateV1::Prepared,
                    stage_receipt: None,
                    receipt: None,
                    created_at_unix_ms: now_unix_ms,
                    updated_at_unix_ms: now_unix_ms,
                })
            }
            (None, None) => None,
            _ => return Err(HarnessServiceError::Corrupt(
                "deterministic delivery identity disagrees with launch plan",
            )),
        };
        let continuation = match (
            plan.continuation,
            ids.continuation_ref.clone(),
            ids.continuation_receipt_ref.clone(),
        ) {
            (
                dispatch::HarnessContinuationPolicyV1::ParentRun,
                Some(continuation_ref),
                Some(receipt_ref),
            ) => {
                let source = self.engine.run(actor_run_id)
                    .ok_or(HarnessServiceError::InvalidContinuationProof(
                        "scheduled continuation source run is missing",
                    ))?;
                let source_binding = source.binding.as_ref()
                    .ok_or(HarnessServiceError::InvalidContinuationProof(
                        "scheduled continuation source binding is missing",
                    ))?;
                let source_context = self.dispatch_contexts.get(&source.operation_id)
                    .ok_or(HarnessServiceError::InvalidContinuationProof(
                        "scheduled continuation source dispatch is missing",
                    ))?;
                if !grant.context_permissions.export
                    || !grant.context_permissions.restore
                    || run.intent.node_id != source_context.node_id
                    || source_binding.node_id != source_context.node_id
                    || source_binding.node_incarnation != source_context.node_incarnation_id
                    || source_binding.workspace_id != source_context.workspace_id
                {
                    return Err(HarnessServiceError::InvalidContinuationProof(
                        "scheduled continuation source or grant changed",
                    ));
                }
                Some(HarnessContinuationV1 {
                    continuation_ref,
                    receipt_ref,
                    revision: HarnessRevision::new(1)?,
                    state: HarnessContinuationStateV1::Prepared,
                    authority: HarnessTransferAuthorityRefV1::ParentGrant {
                        grant_id: grant_id.clone(),
                        revision: grant_revision,
                    },
                    source_run_id: source.run_id.clone(),
                    target_run_id: run.run_id.clone(),
                    operation_id: operation_id.clone(),
                    node_id: source_context.node_id.clone(),
                    node_incarnation: source_context.node_incarnation_id.clone(),
                    workspace_id: source_context.workspace_id.clone(),
                    source_provider: source_context.expected_provider.clone(),
                    source_binding: source_binding.clone(),
                    context: None,
                    target_binding: None,
                    prepared_at_unix_ms: now_unix_ms,
                    exporting_at_unix_ms: None,
                    exported_at_unix_ms: None,
                    bound_at_unix_ms: None,
                    expired_at_unix_ms: None,
                    outcome_unknown_at_unix_ms: None,
                    outcome_unknown_reason: None,
                    cleanup_state: HarnessContinuationCleanupStateV1::Retained,
                    created_at_unix_ms: now_unix_ms,
                    updated_at_unix_ms: now_unix_ms,
                })
            }
            (dispatch::HarnessContinuationPolicyV1::None, None, None) => None,
            _ => return Err(HarnessServiceError::Corrupt(
                "deterministic continuation identity disagrees with launch plan",
            )),
        };
        let current_delivery = self.engine.delivery_for_run(&run.run_id);
        let delivery_present = match (&delivery, current_delivery) {
            (Some(expected), Some(current)) => {
                if current.delivery_ref != expected.delivery_ref
                    || current.authority != expected.authority
                    || current.task_id != expected.task_id
                    || current.run_id != expected.run_id
                    || current.operation_id != expected.operation_id
                    || current.bundle != expected.bundle
                    || current.stage_receipt.as_ref().is_some_and(|receipt| {
                        receipt.node_id != run.intent.node_id
                            || receipt.workspace_id != run.intent.workspace_id
                            || receipt.bundle != expected.bundle
                    })
                    || current.receipt.as_ref().is_some_and(|receipt| {
                        Some(&receipt.receipt_ref) != ids.delivery_receipt_ref.as_ref()
                            || receipt.delivery_ref != expected.delivery_ref
                            || receipt.authority != expected.authority
                            || receipt.task_id != expected.task_id
                            || receipt.run_id != expected.run_id
                            || receipt.operation_id != expected.operation_id
                            || receipt.bundle != expected.bundle
                    })
                {
                    return Err(HarnessServiceError::InvalidDispatchContext(
                        "scheduled delivery authority identity changed",
                    ));
                }
                true
            }
            (Some(_), None) => false,
            (None, None) => false,
            (None, Some(_)) => return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled run has an unexpected delivery authority",
            )),
        };
        let current_continuation = self.engine.continuation_for_run(&run.run_id);
        let continuation_present = match (&continuation, current_continuation) {
            (Some(expected), Some(current)) => {
                if current.continuation_ref != expected.continuation_ref
                    || current.receipt_ref != expected.receipt_ref
                    || current.authority != expected.authority
                    || current.source_run_id != expected.source_run_id
                    || current.target_run_id != expected.target_run_id
                    || current.operation_id != expected.operation_id
                    || current.node_id != expected.node_id
                    || current.node_incarnation != expected.node_incarnation
                    || current.workspace_id != expected.workspace_id
                    || current.source_provider != expected.source_provider
                    || current.source_binding != expected.source_binding
                {
                    return Err(HarnessServiceError::InvalidDispatchContext(
                        "scheduled continuation authority identity changed",
                    ));
                }
                true
            }
            (Some(_), None) => false,
            (None, None) => false,
            (None, Some(_)) => return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled run has an unexpected continuation authority",
            )),
        };
        let expected_authorities = usize::from(delivery.is_some())
            + usize::from(continuation.is_some());
        let present_authorities = usize::from(delivery_present)
            + usize::from(continuation_present);
        if present_authorities == expected_authorities {
            return Ok(HarnessApplyOutcome::Replayed);
        }
        if present_authorities != 0 {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "scheduled specialized authorities are only partially durable",
            ));
        }
        let prepared = self.engine.prepare_scheduled_run_authorities(
            operation_id,
            delivery,
            continuation,
        )?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    pub fn dispatch_context(
        &self,
        operation_id: &HarnessOperationId,
    ) -> Option<&HarnessDispatchContextV1> {
        self.dispatch_contexts.get(operation_id)
    }

    pub fn continuation(
        &self,
        continuation_ref: &HarnessContinuationRef,
    ) -> Option<&HarnessContinuationV1> {
        self.engine.continuation(continuation_ref)
    }

    pub fn prepare_continuation(
        &mut self,
        continuation: HarnessContinuationV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        let source = self.engine.run(&continuation.source_run_id).ok_or(
            HarnessServiceError::InvalidContinuationProof("source run is missing"),
        )?;
        let source_context = self.dispatch_contexts.get(&source.operation_id).ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "source run has no authoritative dispatch context",
            ),
        )?;
        if continuation.source_provider != source_context.expected_provider
            || continuation.node_id != source_context.node_id
            || continuation.node_incarnation != source_context.node_incarnation_id
            || continuation.workspace_id != source_context.workspace_id
            || continuation.source_binding.node_id != source_context.node_id
            || continuation.source_binding.node_incarnation
                != source_context.node_incarnation_id
            || continuation.source_binding.workspace_id != source_context.workspace_id
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "source provider, route, or binding was not derived from durable dispatch",
            ));
        }
        if self.dispatch_contexts.contains_key(&continuation.operation_id) {
            return Err(HarnessServiceError::ContinuationAuthorityWindowClosed);
        }
        let prepared = self.engine.prepare_continuation(continuation)?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    pub(crate) fn begin_continuation_export(
        &mut self,
        continuation_ref: &HarnessContinuationRef,
        expected_revision: HarnessRevision,
        exporting_at_unix_ms: u64,
    ) -> Result<PreparedContinuationExport, HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.engine.continuation(continuation_ref)
            .ok_or(HarnessServiceError::InvalidContinuationProof(
                "continuation authority is missing",
            ))?;
        let source_run = self.engine.run(&current.source_run_id).ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "continuation source run is missing at export lease issuance",
            ),
        )?;
        let target_run = self.engine.run(&current.target_run_id).ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "continuation target run is missing at export lease issuance",
            ),
        )?;
        let source_is_live_managed = matches!(
            &current.source_binding.session,
            HarnessSessionIdentityV1::Managed {
                active_session: Some(_),
                ..
            }
        );
        let source_is_durable = source_run.context_pack.is_some()
            && matches!(
                &current.source_binding.session,
                HarnessSessionIdentityV1::Managed { .. }
            );
        // For a durable source, `current.source_binding` deliberately does
        // not equal `source_run.binding` byte-for-byte: it was synthesized
        // at continuation-creation time (`start_task_v2`) with the Node's
        // then-current incarnation in its `node_incarnation` field, while
        // `source_run.binding` stays fixed at run A's own original
        // spawn-accept binding forever. What must still hold is that
        // `source_run` remains bound to the SAME managed record — node_id,
        // workspace_id, and record_id — the continuation was built against;
        // `active_session`/`node_incarnation` are not signals the durable
        // path can or needs to compare (see
        // `source_binding_matches_context_selection`'s doc comment for the
        // identical reasoning). The Live path is unchanged: it still
        // requires the binding to be byte-for-byte current.
        let source_binding_is_current = match source_run.binding.as_ref() {
            Some(live_binding) if source_is_durable => {
                live_binding.node_id == current.source_binding.node_id
                    && live_binding.workspace_id == current.source_binding.workspace_id
                    && matches!(
                        (&live_binding.session, &current.source_binding.session),
                        (
                            HarnessSessionIdentityV1::Managed { record_id: left, .. },
                            HarnessSessionIdentityV1::Managed { record_id: right, .. },
                        ) if left == right
                    )
            }
            live_binding => live_binding == Some(&current.source_binding),
        };
        let source_has_usable_managed_binding =
            source_binding_is_current && (source_is_live_managed || source_is_durable);
        // Captured now (an owned value, no `self.engine` borrow) so it
        // survives past the `&mut self` commit below.
        let source_context_pack = source_run.context_pack.clone();
        let transfer_authorized = match &current.authority {
            HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                self.engine.grant(grant_id).is_some_and(|grant| {
                    grant.revision == *revision
                        && grant.state
                            == gate4agent_harness_protocol::SessionGrantStateV1::Active
                        && grant.context_permissions.export
                        && grant.context_permissions.restore
                        && grant.actor_run_id == current.source_run_id
                        && target_run.parent_run_id.as_ref() == Some(&current.source_run_id)
                        && grant.allows_target(
                            &target_run.intent.node_id,
                            &target_run.intent.workspace_id,
                            &target_run.intent.provider_profile,
                            target_run.intent.mode,
                        )
                })
            }
            HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                self.issued_launches.get(&target_run.operation_id)
                    .is_some_and(|current_issuance| {
                        current_issuance.reference() == *issuance
                            && current_issuance.context_source.as_ref().is_some_and(|source| {
                                source.source_run_id == current.source_run_id
                                    && source.source_run_revision == source_run.revision
                                    && source.node_id == current.node_id
                                    && source.node_incarnation == current.node_incarnation
                                    && source.workspace_id == current.workspace_id
                                    && source_binding_matches_context_selection(
                                        &current.source_binding,
                                        source,
                                    )
                            })
                    })
            }
        };
        if !transfer_authorized
            || !matches!(
                source_run.lifecycle,
                HarnessRunLifecycleV1::Running
                    | HarnessRunLifecycleV1::Waiting
                    | HarnessRunLifecycleV1::Completed
            )
            || !source_has_usable_managed_binding
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "continuation export lease is not authorized by the current exact grant and source binding",
            ));
        }
        if current.state != HarnessContinuationStateV1::Prepared
            || current.revision != expected_revision
            || exporting_at_unix_ms < current.updated_at_unix_ms
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "continuation is not exact Prepared authority",
            ));
        }
        let mut exporting = current.clone();
        exporting.revision = next_harness_revision(exporting.revision, "continuation")?;
        exporting.state = HarnessContinuationStateV1::Exporting;
        exporting.exporting_at_unix_ms = Some(exporting_at_unix_ms);
        exporting.updated_at_unix_ms = exporting_at_unix_ms;
        let prepared = self.engine.prepare_continuation_export_begin(
            expected_revision,
            exporting.clone(),
        )?;
        self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        Ok(PreparedContinuationExport {
            continuation: exporting,
            source_context_pack,
        })
    }

    pub(crate) fn complete_continuation_export(
        &mut self,
        prepared: &PreparedContinuationExport,
        proof: &c2::ExportedContextPackProof,
        exported_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.engine.continuation(&prepared.continuation.continuation_ref)
            .ok_or(HarnessServiceError::InvalidContinuationProof(
                "exporting continuation is missing",
            ))?;
        if current != &prepared.continuation
            || proof.continuation_ref() != &current.continuation_ref
            || proof.route().node_id.as_str() != current.node_id.as_str()
            || proof.route().expected_incarnation_id.to_string()
                != current.node_incarnation.as_str()
            || proof.source_provider().as_str() != current.source_provider.as_str()
            || proof.source_session() != &continuation_source_session(current)?
            || exported_at_unix_ms < current.updated_at_unix_ms
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "C2 export proof does not match exact durable authority",
            ));
        }
        let mut exported = current.clone();
        exported.revision = next_harness_revision(exported.revision, "continuation")?;
        exported.state = HarnessContinuationStateV1::Exported;
        exported.context = Some(context_receipt_from_node(proof.context())?);
        exported.exported_at_unix_ms = Some(exported_at_unix_ms);
        exported.updated_at_unix_ms = exported_at_unix_ms;
        let prepared_mutation = self.engine.prepare_continuation_export(
            current.revision,
            exported,
        )?;
        self.commit_prepared(prepared_mutation, self.dispatch_contexts.clone())
    }

    pub(crate) fn mark_continuation_export_outcome_unknown(
        &mut self,
        prepared: &PreparedContinuationExport,
        reason: HarnessContinuationOutcomeUnknownReasonV1,
        now_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.engine.continuation(&prepared.continuation.continuation_ref)
            .ok_or(HarnessServiceError::InvalidContinuationProof(
                "exporting continuation is missing",
            ))?;
        if current != &prepared.continuation || now_unix_ms < current.updated_at_unix_ms {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "outcome-unknown proof does not match Exporting authority",
            ));
        }
        let mut unknown = current.clone();
        unknown.revision = next_harness_revision(unknown.revision, "continuation")?;
        unknown.state = HarnessContinuationStateV1::OutcomeUnknown;
        unknown.outcome_unknown_at_unix_ms = Some(now_unix_ms);
        unknown.outcome_unknown_reason = Some(reason);
        unknown.updated_at_unix_ms = now_unix_ms;
        let mutation = self.engine.prepare_continuation_export_outcome_unknown(
            current.revision,
            unknown,
        )?;
        self.commit_prepared(mutation, self.dispatch_contexts.clone())
    }

    /// Restart-only closure for an Exporting authority whose waiter was lost.
    /// The exact export is never reissued; an exact replay can only reproduce
    /// the already committed privacy-safe OutcomeUnknown record.
    pub(crate) fn recover_exporting_continuation_outcome_unknown(
        &mut self,
        continuation_ref: &HarnessContinuationRef,
        expected_revision: HarnessRevision,
        now_unix_ms: u64,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.engine.continuation(continuation_ref)
            .ok_or(HarnessServiceError::InvalidContinuationProof(
                "exporting continuation authority is missing during recovery",
            ))?
            .clone();
        if current.state == HarnessContinuationStateV1::OutcomeUnknown {
            let replay_revision = next_harness_revision(
                expected_revision,
                "continuation recovery replay",
            )?;
            if current.revision != replay_revision
                || current.outcome_unknown_reason
                    != Some(HarnessContinuationOutcomeUnknownReasonV1::Transport)
                || current.outcome_unknown_at_unix_ms != Some(now_unix_ms)
                || current.updated_at_unix_ms != now_unix_ms
            {
                return Err(HarnessServiceError::InvalidContinuationProof(
                    "continuation recovery replay does not match exact lost waiter outcome",
                ));
            }
            let replay = self.engine.prepare_continuation_export_outcome_unknown(
                expected_revision,
                current,
            )?;
            return Ok(replay.outcome());
        }
        if current.state != HarnessContinuationStateV1::Exporting
            || current.revision != expected_revision
            || now_unix_ms < current.updated_at_unix_ms
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "continuation recovery requires exact durable Exporting authority",
            ));
        }
        let mut unknown = current;
        unknown.revision = next_harness_revision(unknown.revision, "continuation")?;
        unknown.state = HarnessContinuationStateV1::OutcomeUnknown;
        unknown.outcome_unknown_at_unix_ms = Some(now_unix_ms);
        unknown.outcome_unknown_reason = Some(
            HarnessContinuationOutcomeUnknownReasonV1::Transport,
        );
        unknown.updated_at_unix_ms = now_unix_ms;
        let prepared = self.engine.prepare_continuation_export_outcome_unknown(
            expected_revision,
            unknown,
        )?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    pub(crate) fn expire_continuation_export(
        &mut self,
        prepared: &PreparedContinuationExport,
        now_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.engine.continuation(&prepared.continuation.continuation_ref)
            .ok_or(HarnessServiceError::InvalidContinuationProof(
                "exporting continuation is missing",
            ))?;
        if current != &prepared.continuation || now_unix_ms < current.updated_at_unix_ms {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "expiry proof does not match Exporting authority",
            ));
        }
        let mut expired = current.clone();
        expired.revision = next_harness_revision(expired.revision, "continuation")?;
        expired.state = HarnessContinuationStateV1::Expired;
        expired.expired_at_unix_ms = Some(now_unix_ms);
        expired.updated_at_unix_ms = now_unix_ms;
        let mutation = self.engine.prepare_continuation_expiry(current.revision, expired)?;
        self.commit_prepared(mutation, self.dispatch_contexts.clone())
    }

    /// Actor-side second phase. No HarnessService borrow is held while C2 is
    /// awaited; the sealed result is applied atomically after it returns.
    pub(crate) fn apply_continuation_export_outcome(
        &mut self,
        outcome: c2::ExportContextPackOutcome,
        completed_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        match outcome {
            c2::ExportContextPackOutcome::Exported { prepared, proof } => {
                self.complete_continuation_export(
                    &prepared,
                    &proof,
                    completed_at_unix_ms,
                )
            }
            c2::ExportContextPackOutcome::OutcomeUnknown { prepared, reason } => {
                let reason = match reason {
                    c2::ContextExportOutcomeUnknownReason::Transport => {
                        HarnessContinuationOutcomeUnknownReasonV1::Transport
                    }
                    c2::ContextExportOutcomeUnknownReason::RouteMismatch => {
                        HarnessContinuationOutcomeUnknownReasonV1::RouteMismatch
                    }
                    c2::ContextExportOutcomeUnknownReason::ReceiptMismatch => {
                        HarnessContinuationOutcomeUnknownReasonV1::ReceiptMismatch
                    }
                    c2::ContextExportOutcomeUnknownReason::UnexpectedResponse => {
                        HarnessContinuationOutcomeUnknownReasonV1::UnexpectedResponse
                    }
                };
                self.mark_continuation_export_outcome_unknown(
                    &prepared,
                    reason,
                    completed_at_unix_ms,
                )
            }
            // Both shapes expire the continuation identically, and the
            // expired record keeps no field for why -- so the node's own
            // refusal code, or the reason the export never left, survives
            // only here.
            c2::ExportContextPackOutcome::Rejected { prepared, code, message } => {
                tracing::warn!(
                    continuation = %prepared.continuation.continuation_ref,
                    node_failure_code = ?code,
                    node_message = %message,
                    "node rejected the context-pack export: the continuation expires",
                );
                self.expire_continuation_export(&prepared, completed_at_unix_ms)
            }
            c2::ExportContextPackOutcome::ExpiredBeforeSend { prepared, reason } => {
                tracing::warn!(
                    continuation = %prepared.continuation.continuation_ref,
                    ?reason,
                    "context-pack export never reached the node: the continuation expires",
                );
                self.expire_continuation_export(&prepared, completed_at_unix_ms)
            }
        }
    }

    /// Fail-closed recovery hook for an unbound authority whose Node has a new
    /// incarnation. Same-incarnation recovery is a no-op and never re-exports.
    pub fn expire_unbound_continuation_on_incarnation_change(
        &mut self,
        continuation_ref: &HarnessContinuationRef,
        current_route: &gate4agent_c2_protocol::NodeRoute,
        now_unix_ms: u64,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.engine.continuation(continuation_ref)
            .ok_or(HarnessServiceError::InvalidContinuationProof(
                "continuation authority is missing",
            ))?;
        if current.node_id.as_str() != current_route.node_id.as_str() {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "current route is for a different Node",
            ));
        }
        if current.node_incarnation.as_str()
            == current_route.expected_incarnation_id.to_string()
        {
            return Ok(HarnessApplyOutcome::Replayed);
        }
        if current.state == HarnessContinuationStateV1::Bound {
            return Ok(HarnessApplyOutcome::Replayed);
        }
        if !matches!(
            current.state,
            HarnessContinuationStateV1::Prepared
                | HarnessContinuationStateV1::Exporting
                | HarnessContinuationStateV1::Exported
                | HarnessContinuationStateV1::OutcomeUnknown
        ) || now_unix_ms < current.updated_at_unix_ms
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "continuation cannot expire from current state",
            ));
        }
        let mut expired = current.clone();
        expired.revision = next_harness_revision(expired.revision, "continuation")?;
        expired.state = HarnessContinuationStateV1::Expired;
        expired.target_binding = None;
        expired.bound_at_unix_ms = None;
        expired.outcome_unknown_at_unix_ms = None;
        expired.outcome_unknown_reason = None;
        expired.expired_at_unix_ms = Some(now_unix_ms);
        expired.updated_at_unix_ms = now_unix_ms;
        let prepared = self.engine.prepare_continuation_expiry(current.revision, expired)?;
        let outcome = prepared.outcome();
        self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        Ok(outcome)
    }

    pub(crate) fn harness_mcp_reservation(
        &self,
        reservation_id: &HarnessMcpReservationId,
    ) -> Option<&HarnessMcpReservationV1> {
        self.harness_mcp_reservations.get(reservation_id)
    }

    pub fn harness_mcp_reservation_state(
        &self,
        reservation_id: &HarnessMcpReservationId,
    ) -> Option<HarnessMcpReservationStateV1> {
        self.harness_mcp_reservations.get(reservation_id)
            .map(|reservation| reservation.state)
    }

    /// Atomically issues the only in-memory proof capable of enqueuing the
    /// continuation SpawnSpec. Restored Dispatching state is deliberately not
    /// sufficient to recreate this proof.
    pub(crate) fn issue_continuation_spawn_lease(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        context: HarnessDispatchContextV1,
        spec: SpawnSpec,
        continuation_ref: &HarnessContinuationRef,
        delivery_ref: Option<&HarnessDeliveryRef>,
    ) -> Result<c2::PreparedContinuationSpawnDispatch, HarnessServiceError> {
        self.ensure_healthy()?;
        let current_run = self.engine.run(&run.run_id).ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "continuation target run is missing before lease issuance",
            ),
        )?;
        let current_operation = self.engine.operation(&operation.operation_id).ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "continuation target operation is missing before lease issuance",
            ),
        )?;
        if current_run.revision != expected_run_revision
            || current_run.lifecycle != HarnessRunLifecycleV1::Requested
            || current_operation.revision != expected_operation_revision
            || current_operation.state != HarnessOperationStateV1::Prepared
            || self.dispatch_contexts.contains_key(&operation.operation_id)
        {
            return Err(HarnessServiceError::ContinuationAuthorityWindowClosed);
        }
        context.validate()?;
        let fingerprint = c2::spawn_spec_fingerprint(&spec)
            .map_err(|_| HarnessServiceError::DispatchFingerprint)?;
        if context.spawn_spec_fingerprint != fingerprint
            || context.operation_id != operation.operation_id
            || context.idempotency_ref != operation.idempotency_ref
        {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "continuation lease does not match exact operation fingerprint",
            ));
        }
        validate_run_dispatch_seam(
            &self.engine,
            &self.issued_launches,
            &run,
            &context,
            &spec,
            &fingerprint,
        )?;
        let continuation = self.engine.continuation(continuation_ref).ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "exported continuation authority is missing",
            ),
        )?.clone();
        if continuation.state != HarnessContinuationStateV1::Exported
            || continuation.target_run_id != run.run_id
            || continuation.operation_id != operation.operation_id
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "continuation lease does not match exact exported authority",
            ));
        }
        let route = gate4agent_c2_protocol::NodeRoute {
            node_id: gate4agent_node_protocol::NodeId::new(context.node_id.as_str())
                .map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "continuation lease has invalid Node id",
                ))?,
            expected_incarnation_id: context.node_incarnation_id.as_str().parse()
                .map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "continuation lease has invalid Node incarnation",
                ))?,
        };
        let prepared = match delivery_ref {
            Some(delivery_ref) => {
                let delivery = self.engine.delivery(delivery_ref).ok_or(
                    HarnessServiceError::InvalidContinuationProof(
                        "combined delivery authority is missing",
                    ),
                )?;
                if delivery.state != HarnessDeliveryStateV1::Staged
                    || delivery.run_id != run.run_id
                    || delivery.operation_id != operation.operation_id
                {
                    return Err(HarnessServiceError::InvalidContinuationProof(
                        "combined delivery lease does not match exact operation",
                    ));
                }
                c2::PreparedContinuationSpawnDispatch::new(
                    route,
                    operation.operation_id.clone(),
                    operation.idempotency_ref.clone(),
                    spec.clone(),
                    fingerprint,
                    delivery,
                    &continuation,
                )
            }
            None => c2::PreparedContinuationSpawnDispatch::continuation_only(
                route,
                operation.operation_id.clone(),
                operation.idempotency_ref.clone(),
                spec.clone(),
                fingerprint,
                &continuation,
            ),
        }.map_err(|_| HarnessServiceError::InvalidContinuationProof(
            "continuation dispatch receipt identity is invalid",
        ))?;
        self.begin_run_dispatch(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
            context,
            &spec,
        )?;
        Ok(prepared)
    }

    pub(crate) fn issue_spawn_lease(
        &mut self,
        catalog: &HarnessLaunchCatalog,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        context: HarnessDispatchContextV1,
        spec: SpawnSpec,
    ) -> Result<PreparedScheduledSpawnLease, HarnessServiceError> {
        self.ensure_healthy()?;
        let scheduled = self.scheduled_launches.get(&operation.operation_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext(
                "ordinary spawn has no durable scheduled launch reference",
            ))?;
        let plan = catalog.resolve_scheduled(scheduled)?;
        let issued = self.issued_launches.get(&run.operation_id)
            .filter(|issuance| issued_run_intent_matches(issuance, &run));
        if issued.is_none() {
            plan.validate_intent(&run.intent)?;
        } else if plan.plan_ref()? != issued.expect("checked issued launch").plan {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "issued spawn plan changed",
            ));
        }
        if plan.harness_mcp != dispatch::HarnessMcpPolicyV1::Disabled {
            return Err(HarnessServiceError::HarnessMcpSpecializedTransitionRequired);
        }
        let task = self.engine.task(&run.task_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext(
                "ordinary spawn task is missing",
            ))?;
        if operation.actor != task.creator {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "ordinary spawn actor does not match durable task creator",
            ));
        }
        match &plan.grant {
            HarnessGrantPolicyV1::Operator => {
                if scheduled.grant.is_some()
                    || !matches!(operation.actor, HarnessActorV1::User { .. })
                    || run.parent_run_id.is_some()
                {
                    return Err(HarnessServiceError::InvalidDispatchContext(
                        "ordinary operator spawn authority changed",
                    ));
                }
            }
            HarnessGrantPolicyV1::Exact { grant_id, revision } => {
                let grant = self.engine.grant(grant_id)
                    .ok_or(HarnessServiceError::InvalidDispatchContext(
                        "ordinary exact grant is missing",
                    ))?;
                let actor_run_id = match &operation.actor {
                    HarnessActorV1::ParentRun { run_id } => run_id,
                    HarnessActorV1::User { .. } => return Err(
                        HarnessServiceError::InvalidDispatchContext(
                            "ordinary exact grant actor is not a parent run",
                        ),
                    ),
                };
                if grant.revision != *revision
                    || grant.actor_run_id != *actor_run_id
                    || run.parent_run_id.as_ref() != Some(actor_run_id)
                    || !grant.allows_target(
                        &run.intent.node_id,
                        &run.intent.workspace_id,
                        &run.intent.provider_profile,
                        run.intent.mode,
                    )
                {
                    return Err(HarnessServiceError::InvalidDispatchContext(
                        "ordinary exact grant is not active for the target",
                    ));
                }
            }
        }
        let current_run = self.engine.run(&run.run_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext("ordinary target run is missing"))?;
        let current_operation = self.engine.operation(&operation.operation_id)
            .ok_or(HarnessServiceError::InvalidDispatchContext("ordinary target operation is missing"))?;
        if current_run.revision != expected_run_revision
            || current_run.lifecycle != HarnessRunLifecycleV1::Requested
            || current_operation.revision != expected_operation_revision
            || current_operation.state != HarnessOperationStateV1::Prepared
            || self.dispatch_contexts.contains_key(&operation.operation_id)
        {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "ordinary spawn authority window is closed",
            ));
        }
        context.validate()?;
        let fingerprint = c2::spawn_spec_fingerprint(&spec)
            .map_err(|_| HarnessServiceError::DispatchFingerprint)?;
        if context.spawn_spec_fingerprint != fingerprint
            || context.operation_id != operation.operation_id
            || context.idempotency_ref != operation.idempotency_ref
        {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "ordinary spawn lease does not match exact operation fingerprint",
            ));
        }
        validate_run_dispatch_seam(
            &self.engine,
            &self.issued_launches,
            &run,
            &context,
            &spec,
            &fingerprint,
        )?;
        let route = gate4agent_c2_protocol::NodeRoute {
            node_id: gate4agent_node_protocol::NodeId::new(context.node_id.as_str())
                .map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "ordinary spawn lease has invalid Node id",
                ))?,
            expected_incarnation_id: context.node_incarnation_id.as_str().parse()
                .map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "ordinary spawn lease has invalid Node incarnation",
                ))?,
        };
        let prepared = prepared_scheduled_spawn_dispatch(
            &self.engine,
            &self.issued_launches,
            plan,
            route,
            &operation,
            spec.clone(),
            fingerprint,
            None,
        )?;
        let prepared = match &run.intent.worktree {
            HarnessWorktreeIntentV1::ManagedProfile {
                profile_id,
                expected_profile_revision,
            } => PreparedScheduledSpawnLease::Managed(
                c2::PreparedManagedWorktreeSpawnDispatch::new(
                    prepared,
                    WorktreeProfileId::new(profile_id.as_str()).map_err(|_| {
                        HarnessServiceError::InvalidDispatchContext(
                            "issued managed worktree profile id is invalid",
                        )
                    })?,
                    WorktreeProfileRevision::new(expected_profile_revision.as_str()).map_err(
                        |_| HarnessServiceError::InvalidDispatchContext(
                            "issued managed worktree profile revision is invalid",
                        ),
                    )?,
                ).map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "issued managed worktree dispatch is not sealed",
                ))?,
            ),
            HarnessWorktreeIntentV1::Existing | HarnessWorktreeIntentV1::Managed { .. } => {
                PreparedScheduledSpawnLease::Direct(prepared)
            }
        };
        self.begin_run_dispatch(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
            context,
            &spec,
        )?;
        Ok(prepared)
    }

    pub fn committed_snapshot(&self) -> HarnessCommittedSnapshot {
        HarnessCommittedSnapshot {
            engine: self.engine.checkpoint(),
            dispatch_contexts: self.dispatch_contexts.values().cloned().collect(),
            harness_mcp_reservations: self.harness_mcp_reservations.values().cloned().collect(),
            operator_requests: operator_request_records(&self.operator_requests),
            scheduled_launches: self.scheduled_launches.clone(),
            issued_launches: self.issued_launches.clone(),
        }
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn apply(
        &mut self,
        mutation: HarnessMutationV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        mutation.validate_payload()?;
        let actual_digest = mutation_request_digest(&mutation)?;
        if mutation.operation().request_digest != actual_digest {
            return Err(HarnessServiceError::MutationDigestMismatch);
        }
        let prepared = self.engine.prepare(mutation)?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Replayed {
            return Ok(outcome);
        }
        self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        Ok(outcome)
    }

    /// D1's `g4a_mail_send`, called under `grant_id`'s own authority
    /// (`ParentRun` actor, the calling run) through `HarnessMutationV1::
    /// SendMail` -- the same `apply` every other mutation kind uses, digest
    /// computed here the same way `resolve_harness_mcp_grant` computes one
    /// for a freshly minted grant. A named refusal (unknown/ended recipient,
    /// no `mail` permission) is `engine.prepare`'s own `HarnessEngineError`
    /// `Display` text, carried back as `HarnessMailSendResultV1::Refused`
    /// rather than propagated as a host error (D1: "a named refusal, never a
    /// silent drop").
    pub(crate) fn send_mail(
        &mut self,
        grant_id: SessionGrantId,
        actor_run_id: HarnessRunId,
        from: gate4agent_harness_protocol::HarnessRecordRef,
        to: HarnessMailAddressV1,
        subject: String,
        body: String,
        reply_to: Option<gate4agent_harness_protocol::HarnessMailMessageId>,
        refs: Vec<gate4agent_harness_protocol::HarnessMailRefV1>,
        now_unix_ms: u64,
    ) -> Result<HarnessMailSendResultV1, HarnessServiceError> {
        self.ensure_healthy()?;
        let message_id = derived_mail_id(
            "hmail_",
            b"gate4agent-harness-mail-message-id-v1",
            &next_mail_nonce_material(&grant_id, now_unix_ms),
            gate4agent_harness_protocol::HarnessMailMessageId::new,
        )?;
        let operation = mail_operation(
            &grant_id,
            HarnessActorV1::ParentRun { run_id: actor_run_id },
            HarnessOperationKindV1::SendMail,
            now_unix_ms,
        )?;
        let message = gate4agent_harness_protocol::HarnessMailMessageV1 {
            message_id: message_id.clone(),
            from,
            to,
            subject,
            body,
            reply_to,
            task_id: None,
            refs,
            created_at_unix_ms: now_unix_ms,
        };
        let mut mutation = HarnessMutationV1::SendMail { operation, message };
        mutation.operation_mut().request_digest = mutation_request_digest(&mutation)?;
        match self.apply(mutation) {
            Ok(_) => Ok(HarnessMailSendResultV1::Accepted { message_id }),
            Err(HarnessServiceError::Engine(error @ (
                HarnessEngineError::MailRecipientUnknown { .. }
                | HarnessEngineError::MailRecipientEnded { .. }
                | HarnessEngineError::MailPermissionDenied { .. }
            ))) => Ok(HarnessMailSendResultV1::Refused { reason: error.to_string() }),
            Err(error) => Err(error),
        }
    }

    /// D1's `g4a_mail_ack`, and the operator's own ack
    /// (`operator_ack_mail`'s call site below passes an operator `actor`
    /// with the record's own resolved grant). Idempotent by construction
    /// (`HarnessEngine::prepare`'s `AckMail` arm): acking an already-acked
    /// `(message_id, reader_record_id)` pair is a no-op success.
    pub(crate) fn ack_mail(
        &mut self,
        grant_id: SessionGrantId,
        actor: HarnessActorV1,
        reader_record_id: HarnessSelectorV1,
        message_id: gate4agent_harness_protocol::HarnessMailMessageId,
        now_unix_ms: u64,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        let operation = mail_operation(&grant_id, actor, HarnessOperationKindV1::AckMail, now_unix_ms)?;
        let ack = gate4agent_harness_protocol::HarnessMailAckV1 {
            message_id,
            reader_record_id,
            acked_at_unix_ms: now_unix_ms,
        };
        let mut mutation = HarnessMutationV1::AckMail { operation, ack };
        mutation.operation_mut().request_digest = mutation_request_digest(&mutation)?;
        self.apply(mutation)
    }

    /// D5's `g4a_task_create` (Slice D): an agent creates a child task
    /// under its own subtree, called under `grant_id`'s own authority
    /// (`ParentRun` actor, the calling run) -- same `apply` every other
    /// mutation kind uses, same digest-then-apply shape `send_mail`
    /// establishes. `parent_task_id: None` means "under my own task";
    /// `Some` must name the caller's own task or one of its strict
    /// descendants (`HarnessEngine::task_is_strict_descendant`) and must not
    /// be terminal -- both are named refusals, never host errors, resolved
    /// BEFORE any mutation is built. The mutation itself is the identical
    /// `HarnessMutationV1::CreateTask` the operator's own
    /// `operator_create_task` builds (`creator`/`actor` both `ParentRun`,
    /// initial state `Backlog`, no dependencies -- `g4a_task_create` never
    /// accepts any). A title/body the engine's own `HarnessTaskV1::validate`
    /// refuses at `apply` time comes back as `HarnessTaskCreateResultV1`'s
    /// own named variant (`TitleInvalid`/`BodyInvalid`/`DependenciesInvalid`),
    /// never propagated as a service error -- the same "a named refusal,
    /// never a silent drop" discipline `send_mail` established for mail.
    pub(crate) fn agent_create_task(
        &mut self,
        actor_run_id: HarnessRunId,
        grant_id: SessionGrantId,
        title: String,
        body: String,
        parent_task_id: Option<HarnessTaskId>,
        now_unix_ms: u64,
    ) -> Result<HarnessTaskCreateResultV1, HarnessServiceError> {
        self.ensure_healthy()?;
        let own_task_id = self.engine.own_task_of_run(&actor_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(actor_run_id.to_string()))?;
        let parent_task_id = parent_task_id.unwrap_or_else(|| own_task_id.clone());
        if parent_task_id != own_task_id
            && !self.engine.task_is_strict_descendant(&parent_task_id, &own_task_id)
        {
            return Ok(HarnessTaskCreateResultV1::ParentOutsideOwnSubtree {
                parent_task_id,
                own_task_id,
            });
        }
        let parent_state = self.engine.task(&parent_task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(parent_task_id.to_string()))?
            .state;
        if matches!(
            parent_state,
            HarnessTaskStateV1::Done | HarnessTaskStateV1::Failed | HarnessTaskStateV1::Cancelled
        ) {
            return Ok(HarnessTaskCreateResultV1::ParentTerminal { parent_task_id, state: parent_state });
        }
        let material = next_mail_nonce_material(&grant_id, now_unix_ms);
        let task_id = derived_mail_id(
            "htask_",
            b"gate4agent-harness-agent-task-create-id-v1",
            &material,
            HarnessTaskId::new,
        )?;
        let actor = HarnessActorV1::ParentRun { run_id: actor_run_id };
        let operation = agent_task_operation(
            &grant_id,
            actor.clone(),
            HarnessOperationKindV1::CreateTask,
            task_id.clone(),
            None,
            now_unix_ms,
        )?;
        let revision = HarnessRevision::new(1)?;
        let task = HarnessTaskV1 {
            task_id: task_id.clone(),
            revision,
            title,
            body,
            creator: actor,
            parent_task_id: Some(parent_task_id),
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Backlog,
            run_ids: Vec::new(),
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: now_unix_ms,
            updated_at_unix_ms: now_unix_ms,
        };
        let mut mutation = HarnessMutationV1::CreateTask { operation, task };
        mutation.operation_mut().request_digest = mutation_request_digest(&mutation)?;
        match self.apply(mutation) {
            Ok(_) => Ok(HarnessTaskCreateResultV1::Created { task_id, revision }),
            Err(HarnessServiceError::Engine(HarnessEngineError::Validation(
                error @ HarnessValidationError::InvalidTitle,
            ))) => Ok(HarnessTaskCreateResultV1::TitleInvalid { why: error.to_string() }),
            Err(HarnessServiceError::Engine(HarnessEngineError::Validation(
                error @ HarnessValidationError::InvalidBody,
            ))) => Ok(HarnessTaskCreateResultV1::BodyInvalid { why: error.to_string() }),
            Err(HarnessServiceError::Engine(HarnessEngineError::Validation(error @ (
                HarnessValidationError::SelfLink { field: "dependencies" }
                | HarnessValidationError::CollectionTooLarge { field: "dependencies", .. }
                | HarnessValidationError::CollectionNotCanonical { field: "dependencies" }
            )))) => Ok(HarnessTaskCreateResultV1::DependenciesInvalid { why: error.to_string() }),
            Err(error) => Err(error),
        }
    }

    /// D5's `g4a_task_move` (Slice D): restricted to a STRICT descendant of
    /// the caller's own task -- never the caller's own task
    /// (`TaskIsOwn`: a session must not end its own oversight by moving
    /// itself). `validate_operator_move` reuses the operator's own
    /// transition graph without duplicating it; a stale `expected_revision`
    /// is `RevisionConflict`. Once past those, the mutation is the identical
    /// `HarnessMutationV1::ReplaceTask` `operator_move_task` builds (only
    /// `state`/`revision`/`updated_at_unix_ms` change; title/body/parent/
    /// dependencies are carried forward verbatim from the current task, so
    /// no title/body validation failure is reachable here the way it is in
    /// `agent_create_task`).
    pub(crate) fn agent_move_task(
        &mut self,
        actor_run_id: HarnessRunId,
        grant_id: SessionGrantId,
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        to: HarnessTaskStateV1,
        now_unix_ms: u64,
    ) -> Result<HarnessTaskMoveResultV1, HarnessServiceError> {
        self.ensure_healthy()?;
        let own_task_id = self.engine.own_task_of_run(&actor_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(actor_run_id.to_string()))?;
        if task_id == own_task_id {
            return Ok(HarnessTaskMoveResultV1::TaskIsOwn { task_id });
        }
        if !self.engine.task_is_strict_descendant(&task_id, &own_task_id) {
            return Ok(HarnessTaskMoveResultV1::TaskOutsideOwnSubtree { task_id, own_task_id });
        }
        let current = self.engine.task(&task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(task_id.to_string()))?
            .clone();
        if validate_operator_move(current.state, to).is_err() {
            return Ok(HarnessTaskMoveResultV1::IllegalTransition { task_id, from: current.state, to });
        }
        if current.revision != expected_revision {
            return Ok(HarnessTaskMoveResultV1::RevisionConflict {
                task_id,
                expected: expected_revision,
                current: current.revision,
            });
        }
        let from = current.state;
        let new_revision = next_harness_revision(current.revision, "task")?;
        let mut task = current;
        task.revision = new_revision;
        task.state = to;
        task.updated_at_unix_ms = now_unix_ms;
        let actor = HarnessActorV1::ParentRun { run_id: actor_run_id };
        let operation = agent_task_operation(
            &grant_id,
            actor,
            HarnessOperationKindV1::MutateTask,
            task_id.clone(),
            Some(expected_revision),
            now_unix_ms,
        )?;
        let mut mutation = HarnessMutationV1::ReplaceTask { operation, expected_revision, task };
        mutation.operation_mut().request_digest = mutation_request_digest(&mutation)?;
        self.apply(mutation)?;
        Ok(HarnessTaskMoveResultV1::Moved { task_id, revision: new_revision, from, to })
    }

    pub fn prepare_delivery_from_compiled(
        &mut self,
        delivery_ref: HarnessDeliveryRef,
        grant_id: SessionGrantId,
        grant_revision: HarnessRevision,
        task_id: HarnessTaskId,
        run_id: HarnessRunId,
        operation_id: HarnessOperationId,
        selector: HarnessSelectorV1,
        compiled: &CompiledDeliveryBundleV2,
        created_at_unix_ms: u64,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        let delivery = HarnessDeliveryV1 {
            delivery_ref,
            revision: HarnessRevision::new(1)?,
            authority: HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id,
                revision: grant_revision,
            },
            task_id,
            run_id,
            operation_id,
            bundle: delivery::compiled_bundle_identity(selector, compiled)?,
            state: HarnessDeliveryStateV1::Prepared,
            stage_receipt: None,
            receipt: None,
            created_at_unix_ms,
            updated_at_unix_ms: created_at_unix_ms,
        };
        self.prepare_delivery(delivery)
    }

    pub(crate) fn prepare_delivery(
        &mut self,
        delivery: HarnessDeliveryV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        if self.dispatch_contexts.contains_key(&delivery.operation_id) {
            return Err(HarnessServiceError::DeliveryAuthorityWindowClosed);
        }
        let prepared = self.engine.prepare_delivery(delivery)?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    /// Issues one owned CAS upload lease after the last grant check. The lease
    /// may finish its exact content-addressed sequence after later revocation;
    /// reopening the service requires a fresh current-grant check.
    pub(crate) fn issue_delivery_staging_lease(
        &self,
        delivery_ref: &HarnessDeliveryRef,
        route: gate4agent_c2_protocol::NodeRoute,
        compiled: CompiledDeliveryBundleV2,
    ) -> Result<c2::PreparedDeliveryStageLease, HarnessServiceError> {
        self.ensure_healthy()?;
        let delivery = self.engine.delivery(delivery_ref).ok_or(
            HarnessServiceError::InvalidStagedDeliveryProof(
                "prepared delivery authority is missing",
            ),
        )?;
        let run = self.engine.run(&delivery.run_id).ok_or(
            HarnessServiceError::InvalidStagedDeliveryProof(
                "prepared delivery run is missing",
            ),
        )?;
        let operation = self.engine.operation(&delivery.operation_id).ok_or(
            HarnessServiceError::InvalidStagedDeliveryProof(
                "prepared delivery operation is missing",
            ),
        )?;
        let exact_bundle = delivery::compiled_bundle_identity(
            delivery.bundle.selector.clone(),
            &compiled,
        )?;
        let transfer_authorized = match &delivery.authority {
            HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                self.engine.grant(grant_id).is_some_and(|grant| {
                    grant.revision == *revision
                        && grant.state
                            == gate4agent_harness_protocol::SessionGrantStateV1::Active
                        && grant.actor_run_id
                            == run.parent_run_id.clone().unwrap_or_else(|| run.run_id.clone())
                        && grant.allows_delivery_bundle(&delivery.bundle.selector)
                        && grant.allows_target(
                            &run.intent.node_id,
                            &run.intent.workspace_id,
                            &run.intent.provider_profile,
                            run.intent.mode,
                        )
                })
            }
            HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                self.issued_launches.get(&run.operation_id)
                    .is_some_and(|current| {
                        current.reference() == *issuance
                            && current.delivery.as_ref().is_some_and(|selection| {
                                selection.bundle == delivery.bundle
                            })
                    })
            }
        };
        if delivery.state != HarnessDeliveryStateV1::Prepared
            || !transfer_authorized
            || delivery.bundle != exact_bundle
            || delivery.task_id != run.task_id
            || run.operation_id != delivery.operation_id
            || operation.operation_id != delivery.operation_id
            || operation.run_id.as_ref() != Some(&run.run_id)
            || operation.state != HarnessOperationStateV1::Prepared
            || run.lifecycle != HarnessRunLifecycleV1::Requested
            || self.dispatch_contexts.contains_key(&delivery.operation_id)
            || route.node_id.as_str() != run.intent.node_id.as_str()
        {
            return Err(HarnessServiceError::InvalidStagedDeliveryProof(
                "delivery staging lease is not authorized by current exact grant and run",
            ));
        }
        let workspace_id = gate4agent_node_protocol::WorkspaceId::new(
            run.intent.workspace_id.as_str(),
        ).map_err(|_| HarnessServiceError::InvalidStagedDeliveryProof(
            "delivery staging lease has invalid workspace id",
        ))?;
        c2::PreparedDeliveryStageLease::new(
            route,
            delivery.operation_id.clone(),
            delivery.run_id.clone(),
            workspace_id,
            delivery.bundle.selector.clone(),
            compiled,
        ).map_err(|_| HarnessServiceError::DeliveryCompilationInvalid)
    }

    pub fn stage_delivery_with_proof(
        &mut self,
        expected_revision: HarnessRevision,
        delivery_ref: &HarnessDeliveryRef,
        staged_at_unix_ms: u64,
        adapter: &c2::HarnessC2Adapter,
        proof: c2::StagedDeliveryProof,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        adapter.validate_current_staged_delivery_proof(&proof).map_err(|_| {
            HarnessServiceError::InvalidStagedDeliveryProof(
                "staged delivery proof is not from the current authoritative Node route",
            )
        })?;
        let current = self.engine.delivery(delivery_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery_ref.to_string()))?
            .clone();
        validate_staged_delivery_proof(&self.engine, &current, &proof)?;
        let stage_receipt = delivery::staged_receipt(
            HarnessSelectorV1::new(proof.node_id().as_str())?,
            HarnessSelectorV1::new(proof.incarnation_id().to_string())?,
            HarnessSelectorV1::new(proof.workspace_id().as_str())?,
            proof.selector().clone(),
            proof.receipt(),
            staged_at_unix_ms,
        )?;
        let next = if current.state == HarnessDeliveryStateV1::Staged
            && current.stage_receipt.as_ref() == Some(&stage_receipt)
        {
            current
        } else if current.state == HarnessDeliveryStateV1::Staged {
            let current_stage = current.stage_receipt.as_ref().ok_or(
                HarnessServiceError::InvalidStagedDeliveryProof(
                    "durable staged delivery has no receipt",
                ),
            )?;
            if current_stage.node_incarnation == stage_receipt.node_incarnation {
                return Err(HarnessServiceError::InvalidStagedDeliveryProof(
                    "same-incarnation staging proof changed",
                ));
            }
            let mut next = current;
            next.revision = HarnessRevision::new(
                expected_revision.get().checked_add(1)
                    .ok_or(HarnessServiceError::InvalidStagedDeliveryProof(
                        "delivery revision overflow",
                    ))?,
            )?;
            next.stage_receipt = Some(stage_receipt);
            next.updated_at_unix_ms = staged_at_unix_ms;
            next
        } else {
            let mut next = current;
            next.revision = HarnessRevision::new(
                expected_revision.get().checked_add(1)
                    .ok_or(HarnessServiceError::InvalidStagedDeliveryProof(
                        "delivery revision overflow",
                    ))?,
            )?;
            next.state = HarnessDeliveryStateV1::Staged;
            next.stage_receipt = Some(stage_receipt);
            next.updated_at_unix_ms = staged_at_unix_ms;
            next
        };
        self.stage_delivery_record(expected_revision, next)
    }

    fn stage_delivery_record(
        &mut self,
        expected_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        if self.dispatch_contexts.contains_key(&delivery.operation_id) {
            return Err(HarnessServiceError::DeliveryAuthorityWindowClosed);
        }
        let prepared = self.engine.prepare_delivery_stage(expected_revision, delivery)?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    pub fn transition_run_with_accepted_spawn_and_delivery(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_delivery_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
        proof: &c2::AcceptedSpawnBindingProof,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if run.intent.continuation.is_some()
            || self.engine.continuation_for_run(&run.run_id).is_some()
        {
            return Err(HarnessServiceError::AtomicContinuationBindRequired);
        }
        if proof.harness_mcp_proxy().is_some() {
            return Err(HarnessServiceError::HarnessMcpSpecializedTransitionRequired);
        }
        validate_delivery_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &delivery,
            &run,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let prepared = self.engine.prepare_accepted_spawn_delivery_commit(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
            expected_delivery_revision,
            delivery,
        )?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, accepted_contexts)?;
        }
        Ok(())
    }

    pub fn transition_operation(
        &mut self,
        expected_revision: HarnessRevision,
        operation: HarnessOperationV1,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if operation.run_id.is_some() {
            return Err(HarnessServiceError::NonAtomicRunOperation);
        }
        if self.engine.operation(&operation.operation_id) == Some(&operation) {
            return Ok(());
        }
        let prepared = self.engine.prepare_operation_transition(expected_revision, operation)?;
        self.commit_prepared(prepared, self.dispatch_contexts.clone())
    }

    pub fn transition_run_operation(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
        {
            return Ok(());
        }
        if self.engine.run(&run.run_id).is_some_and(|current| {
            current.lifecycle
                == gate4agent_harness_protocol::HarnessRunLifecycleV1::Dispatching
                && run.lifecycle
                    == gate4agent_harness_protocol::HarnessRunLifecycleV1::Running
        }) {
            return Err(HarnessServiceError::AcceptedSpawnProofRequired);
        }
        validate_authoritative_dispatch_binding(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
        )?;
        let prepared = self.engine.prepare_run_operation_transition(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
        )?;
        self.commit_prepared(prepared, self.dispatch_contexts.clone())
    }

    pub fn transition_run_with_accepted_spawn(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        proof: &c2::AcceptedSpawnBindingProof,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if run.intent.continuation.is_some()
            || self.engine.continuation_for_run(&run.run_id).is_some()
            || proof.context().is_some()
        {
            return Err(HarnessServiceError::AtomicContinuationBindRequired);
        }
        if proof.harness_mcp_proxy().is_some() {
            return Err(HarnessServiceError::HarnessMcpSpecializedTransitionRequired);
        }
        if run.intent.delivery_bundle.is_some()
            || self.engine.delivery_for_run(&run.run_id).is_some()
        {
            return Err(HarnessServiceError::AtomicDeliveryCommitRequired);
        }
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
        {
            validate_committed_spawn_replay(
                &self.engine,
                &self.dispatch_contexts,
                &self.issued_launches,
                &run,
                &operation,
                proof,
            )?;
            return Ok(());
        }
        validate_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let prepared = self.engine.prepare_run_operation_transition(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
        )?;
        self.commit_prepared(prepared, accepted_contexts)
    }

    pub(crate) fn commit_dispatch_outcome(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let prepared = self.engine.prepare_dispatch_outcome_commit(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
            expected_task_revision,
            task,
        )?;
        if prepared.outcome() == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(())
    }

    pub(crate) fn commit_scheduled_pre_dispatch_outcome(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<Option<HarnessMcpReservationId>, HarnessServiceError> {
        self.ensure_healthy()?;
        let prepared = self.engine.prepare_dispatch_outcome_commit(
            expected_run_revision,
            run.clone(),
            expected_operation_revision,
            operation.clone(),
            expected_task_revision,
            task,
        )?;
        if prepared.outcome() == HarnessApplyOutcome::Replayed {
            return Ok(self.harness_mcp_reservations.values().find(|reservation| {
                reservation.operation_id == operation.operation_id
                    && reservation.state == HarnessMcpReservationStateV1::Revoked
            }).map(|reservation| reservation.reservation_id.clone()));
        }
        let mut reservations = self.harness_mcp_reservations.clone();
        let cleanup_id = reservations.values().find(|reservation| {
            reservation.operation_id == operation.operation_id
                && matches!(
                    reservation.state,
                    HarnessMcpReservationStateV1::Prepared
                        | HarnessMcpReservationStateV1::Armed
                )
        }).map(|reservation| reservation.reservation_id.clone());
        if let Some(reservation_id) = &cleanup_id {
            let reservation = reservations.get_mut(reservation_id)
                .expect("selected reservation remains present");
            reservation.revision = next_revision(reservation.revision)?;
            reservation.state = HarnessMcpReservationStateV1::Revoked;
            reservation.updated_at_unix_ms = run.updated_at_unix_ms;
        }
        self.commit_prepared_state(prepared, self.dispatch_contexts.clone(), reservations)?;
        Ok(cleanup_id)
    }

    pub(crate) fn commit_run_event(
        &mut self,
        operation: HarnessOperationV1,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let prepared = self.engine.prepare_run_event_commit(
            operation,
            expected_run_revision,
            run,
            expected_task_revision,
            task,
        )?;
        if prepared.outcome() == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(())
    }

    /// Settles one `Running`/`Waiting` run to `Failed` because its bound
    /// host incarnation is proven stale -- see
    /// `HarnessEngine::prepare_run_incarnation_settlement`'s doc comment for
    /// the mutation this commits. Structural twin of `commit_run_event`,
    /// against the dedicated engine entry point rather than the generic one.
    pub(crate) fn commit_run_incarnation_settlement(
        &mut self,
        operation: HarnessOperationV1,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let prepared = self.engine.prepare_run_incarnation_settlement(
            operation,
            expected_run_revision,
            run,
            expected_task_revision,
            task,
        )?;
        if prepared.outcome() == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(())
    }

    /// Idempotently records a durable ContextPack receipt on `run_id`,
    /// reusing the run's own already-observed revision rather than requiring
    /// a caller-supplied CAS token. Never touches task state and stays legal
    /// on an already-terminal (Completed/Failed/Cancelled) run — the exact
    /// narrow exception to the generic terminal-run-mutation freeze that
    /// `prepare_run_context_pack_record` (harness-engine) enforces.
    pub(crate) fn record_run_context_pack(
        &mut self,
        run_id: &HarnessRunId,
        receipt: HarnessResolvedContextPackReceiptV1,
        recorded_at_unix_ms: u64,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        receipt.validate()?;
        let current_run = self.engine.run(run_id)
            .ok_or_else(|| HarnessServiceError::Engine(HarnessEngineError::NotFound(
                run_id.to_string(),
            )))?
            .clone();
        if current_run.context_pack.as_ref() == Some(&receipt) {
            return Ok(HarnessApplyOutcome::Replayed);
        }
        let ids = dispatch::deterministic_context_pack_record_ids(
            run_id,
            node_id,
            &incarnation_id,
            &receipt.digest,
        )?;
        let recorded_at = recorded_at_unix_ms.max(current_run.updated_at_unix_ms);
        let mut next_run = current_run.clone();
        next_run.context_pack = Some(receipt);
        next_run.revision = next_revision(current_run.revision)?;
        next_run.updated_at_unix_ms = recorded_at;
        let operation = HarnessOperationV1 {
            operation_id: ids.operation_id,
            revision: HarnessRevision::new(1)?,
            actor: HarnessActorV1::ParentRun { run_id: run_id.clone() },
            kind: HarnessOperationKindV1::RecordRunContextPack,
            state: HarnessOperationStateV1::Succeeded,
            task_id: None,
            run_id: Some(run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(current_run.revision),
            request_digest: ids.request_digest,
            idempotency_ref: ids.idempotency_ref,
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: recorded_at,
            updated_at_unix_ms: recorded_at,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(recorded_at),
        };
        let prepared = self.engine.prepare_run_context_pack_record(
            operation,
            current_run.revision,
            next_run,
        )?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    /// Idempotently records a single, terminal, best-effort git-facts
    /// capture on `run_id`, reusing the run's own already-observed
    /// revision — same idiom as `record_run_context_pack`. Structural twin
    /// of that wrapper; the one behavioral difference is that this mutation
    /// is legal exactly once per run — `prepare_run_git_facts_record`
    /// rejects a second, different capture rather than ever silently
    /// overwriting it.
    pub(crate) fn record_run_git_facts(
        &mut self,
        run_id: &HarnessRunId,
        facts: HarnessRunGitFactsV1,
        recorded_at_unix_ms: u64,
        node_id: &NodeId,
        incarnation_id: NodeIncarnationId,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        facts.validate()?;
        let current_run = self.engine.run(run_id)
            .ok_or_else(|| HarnessServiceError::Engine(HarnessEngineError::NotFound(
                run_id.to_string(),
            )))?
            .clone();
        if current_run.git_facts.as_ref() == Some(&facts) {
            return Ok(HarnessApplyOutcome::Replayed);
        }
        let ids = dispatch::deterministic_run_git_facts_record_ids(
            run_id,
            node_id,
            &incarnation_id,
        )?;
        let recorded_at = recorded_at_unix_ms.max(current_run.updated_at_unix_ms);
        let mut next_run = current_run.clone();
        next_run.git_facts = Some(facts);
        next_run.revision = next_revision(current_run.revision)?;
        next_run.updated_at_unix_ms = recorded_at;
        let operation = HarnessOperationV1 {
            operation_id: ids.operation_id,
            revision: HarnessRevision::new(1)?,
            actor: HarnessActorV1::ParentRun { run_id: run_id.clone() },
            kind: HarnessOperationKindV1::RecordRunGitFacts,
            state: HarnessOperationStateV1::Succeeded,
            task_id: None,
            run_id: Some(run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(current_run.revision),
            request_digest: ids.request_digest,
            idempotency_ref: ids.idempotency_ref,
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: recorded_at,
            updated_at_unix_ms: recorded_at,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(recorded_at),
        };
        let prepared = self.engine.prepare_run_git_facts_record(
            operation,
            current_run.revision,
            next_run,
        )?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    /// Idempotently grows `task_id`'s `result_refs` index by the reference
    /// derived from `run_id`, reusing the task's own already-observed
    /// revision. No-ops to `Replayed` when the ref is already present or the
    /// task is already at `HARNESS_RESULTS_MAX` capacity — both are legal,
    /// cost-free no-ops, never a conflict, matching
    /// `prepare_task_result_ref_record`'s own idempotency posture.
    pub(crate) fn record_task_result_ref(
        &mut self,
        task_id: &HarnessTaskId,
        run_id: &HarnessRunId,
        recorded_at_unix_ms: u64,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        self.ensure_healthy()?;
        let candidate = HarnessResultRef::for_run(run_id);
        let current_task = self.engine.task(task_id)
            .ok_or_else(|| HarnessServiceError::Engine(HarnessEngineError::NotFound(
                task_id.to_string(),
            )))?
            .clone();
        let insert_at = match current_task.result_refs.binary_search(&candidate) {
            Ok(_) => return Ok(HarnessApplyOutcome::Replayed),
            Err(_) if current_task.result_refs.len() >= HARNESS_RESULTS_MAX => {
                return Ok(HarnessApplyOutcome::Replayed);
            }
            Err(index) => index,
        };
        let ids = dispatch::deterministic_task_result_ref_record_ids(task_id, &candidate)?;
        let recorded_at = recorded_at_unix_ms.max(current_task.updated_at_unix_ms);
        let mut next_task = current_task.clone();
        next_task.result_refs.insert(insert_at, candidate);
        next_task.revision = next_revision(current_task.revision)?;
        next_task.updated_at_unix_ms = recorded_at;
        let operation = HarnessOperationV1 {
            operation_id: ids.operation_id,
            revision: HarnessRevision::new(1)?,
            actor: HarnessActorV1::ParentRun { run_id: run_id.clone() },
            kind: HarnessOperationKindV1::RecordTaskResultRef,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id.clone()),
            run_id: None,
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(current_task.revision),
            request_digest: ids.request_digest,
            idempotency_ref: ids.idempotency_ref,
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: recorded_at,
            updated_at_unix_ms: recorded_at,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(recorded_at),
        };
        let prepared = self.engine.prepare_task_result_ref_record(
            operation,
            current_task.revision,
            next_task,
        )?;
        let outcome = prepared.outcome();
        if outcome == HarnessApplyOutcome::Applied {
            self.commit_prepared(prepared, self.dispatch_contexts.clone())?;
        }
        Ok(outcome)
    }

    pub fn transition_run_with_accepted_spawn_and_continuation(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        continuation_ref: &HarnessContinuationRef,
        expected_continuation_revision: HarnessRevision,
        proof: &c2::AcceptedSpawnBindingProof,
        bound_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if run.intent.delivery_bundle.is_some()
            || self.engine.delivery_for_run(&run.run_id).is_some()
            || proof.harness_mcp_proxy().is_some()
        {
            return Err(HarnessServiceError::AtomicContinuationBindRequired);
        }
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
            && self.engine.continuation(continuation_ref)
                .is_some_and(|current| current.state == HarnessContinuationStateV1::Bound)
        {
            validate_committed_continuation_replay(
                &self.engine,
                &self.dispatch_contexts,
                &self.issued_launches,
                &run,
                &operation,
                continuation_ref,
                proof,
            )?;
            return Ok(());
        }
        validate_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let current = self.engine.continuation(continuation_ref)
            .ok_or(HarnessServiceError::InvalidContinuationProof(
                "continuation authority is missing",
            ))?;
        let proof_context = proof.context().ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "accepted spawn has no exact ContextPack receipt",
            ),
        )?;
        let expected_context = current.context.as_ref().ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "exported continuation has no ContextPack receipt",
            ),
        )?;
        let binding = run.binding.as_ref().ok_or(
            HarnessServiceError::InvalidContinuationProof(
                "continued target run has no binding",
            ),
        )?;
        if current.state != HarnessContinuationStateV1::Exported
            || current.revision != expected_continuation_revision
            || current.target_run_id != run.run_id
            || current.operation_id != operation.operation_id
            || run.continuation_receipt.as_ref() != Some(&current.receipt_ref)
            || &context_receipt_from_node(proof_context)? != expected_context
            || bound_at_unix_ms < current.updated_at_unix_ms
        {
            return Err(HarnessServiceError::InvalidContinuationProof(
                "accepted spawn does not match exported continuation authority",
            ));
        }
        let mut bound = current.clone();
        bound.revision = next_harness_revision(bound.revision, "continuation")?;
        bound.state = HarnessContinuationStateV1::Bound;
        bound.target_binding = Some(binding.clone());
        bound.bound_at_unix_ms = Some(bound_at_unix_ms);
        bound.updated_at_unix_ms = bound_at_unix_ms;
        let prepared = self.engine.prepare_accepted_spawn_continuation_bind(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
            expected_continuation_revision,
            bound,
        )?;
        self.commit_prepared(prepared, accepted_contexts)
    }

    pub fn transition_run_with_accepted_spawn_delivery_and_continuation(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_delivery_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
        continuation_ref: &HarnessContinuationRef,
        expected_continuation_revision: HarnessRevision,
        proof: &c2::AcceptedSpawnBindingProof,
        bound_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if proof.harness_mcp_proxy().is_some() {
            return Err(HarnessServiceError::HarnessMcpSpecializedTransitionRequired);
        }
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
            && self.engine.delivery(&delivery.delivery_ref) == Some(&delivery)
            && self.engine.continuation(continuation_ref)
                .is_some_and(|current| current.state == HarnessContinuationStateV1::Bound)
        {
            validate_delivery_accepted_spawn_proof(
                &self.engine,
                &self.dispatch_contexts,
                &self.issued_launches,
                &delivery,
                &run,
                proof,
            )?;
            validate_committed_continuation_replay(
                &self.engine,
                &self.dispatch_contexts,
                &self.issued_launches,
                &run,
                &operation,
                continuation_ref,
                proof,
            )?;
            return Ok(());
        }
        validate_delivery_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &delivery,
            &run,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let continuation = bound_continuation_from_proof(
            &self.engine,
            continuation_ref,
            expected_continuation_revision,
            &run,
            &operation,
            proof,
            bound_at_unix_ms,
        )?;
        let prepared = self.engine
            .prepare_accepted_spawn_delivery_and_continuation_commit(
                expected_run_revision,
                run,
                expected_operation_revision,
                operation,
                expected_delivery_revision,
                delivery,
                expected_continuation_revision,
                continuation,
            )?;
        self.commit_prepared(prepared, accepted_contexts)
    }

    pub fn transition_run_with_accepted_harness_mcp_spawn_delivery_and_continuation(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_delivery_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
        continuation_ref: &HarnessContinuationRef,
        expected_continuation_revision: HarnessRevision,
        proof: &c2::AcceptedSpawnBindingProof,
        bound_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
            && self.engine.delivery(&delivery.delivery_ref) == Some(&delivery)
            && self.engine.continuation(continuation_ref)
                .is_some_and(|current| current.state == HarnessContinuationStateV1::Bound)
        {
            validate_delivery_accepted_spawn_proof(
                &self.engine,
                &self.dispatch_contexts,
                &self.issued_launches,
                &delivery,
                &run,
                proof,
            )?;
            validate_committed_continuation_replay(
                &self.engine,
                &self.dispatch_contexts,
                &self.issued_launches,
                &run,
                &operation,
                continuation_ref,
                proof,
            )?;
            let proxy = proof.harness_mcp_proxy()
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
            let current = self.harness_mcp_reservations.get(&proxy.reservation_id)
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
            let replayed = bind_harness_mcp_reservation(current, proof, bound_at_unix_ms)?;
            if &replayed != current {
                return Err(HarnessServiceError::HarnessMcpProofMismatch);
            }
            return Ok(());
        }
        validate_delivery_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &delivery,
            &run,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let continuation = bound_continuation_from_proof(
            &self.engine,
            continuation_ref,
            expected_continuation_revision,
            &run,
            &operation,
            proof,
            bound_at_unix_ms,
        )?;
        let proxy = proof.harness_mcp_proxy()
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let current_reservation = self.harness_mcp_reservations.get(&proxy.reservation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let bound_reservation = bind_harness_mcp_reservation(
            current_reservation,
            proof,
            bound_at_unix_ms,
        )?;
        let prepared = self.engine
            .prepare_accepted_spawn_delivery_and_continuation_commit(
                expected_run_revision,
                run,
                expected_operation_revision,
                operation,
                expected_delivery_revision,
                delivery,
                expected_continuation_revision,
                continuation,
            )?;
        let mut reservations = self.harness_mcp_reservations.clone();
        reservations.insert(bound_reservation.reservation_id.clone(), bound_reservation);
        self.commit_prepared_state(prepared, accepted_contexts, reservations)
    }

    pub fn transition_run_with_accepted_harness_mcp_spawn_and_continuation(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        continuation_ref: &HarnessContinuationRef,
        expected_continuation_revision: HarnessRevision,
        proof: &c2::AcceptedSpawnBindingProof,
        bound_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if run.intent.delivery_bundle.is_some()
            || self.engine.delivery_for_run(&run.run_id).is_some()
        {
            return Err(HarnessServiceError::AtomicDeliveryCommitRequired);
        }
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
            && self.engine.continuation(continuation_ref)
                .is_some_and(|current| current.state == HarnessContinuationStateV1::Bound)
        {
            validate_committed_continuation_replay(
                &self.engine,
                &self.dispatch_contexts,
                &self.issued_launches,
                &run,
                &operation,
                continuation_ref,
                proof,
            )?;
            let proxy = proof.harness_mcp_proxy()
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
            let current = self.harness_mcp_reservations.get(&proxy.reservation_id)
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
            if &bind_harness_mcp_reservation(current, proof, bound_at_unix_ms)? != current {
                return Err(HarnessServiceError::HarnessMcpProofMismatch);
            }
            return Ok(());
        }
        validate_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let continuation = bound_continuation_from_proof(
            &self.engine,
            continuation_ref,
            expected_continuation_revision,
            &run,
            &operation,
            proof,
            bound_at_unix_ms,
        )?;
        let proxy = proof.harness_mcp_proxy()
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let current = self.harness_mcp_reservations.get(&proxy.reservation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let bound = bind_harness_mcp_reservation(current, proof, bound_at_unix_ms)?;
        let prepared = self.engine.prepare_accepted_spawn_continuation_bind(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
            expected_continuation_revision,
            continuation,
        )?;
        let mut reservations = self.harness_mcp_reservations.clone();
        reservations.insert(bound.reservation_id.clone(), bound);
        self.commit_prepared_state(prepared, accepted_contexts, reservations)
    }

    pub fn transition_run_with_accepted_harness_mcp_spawn(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        proof: &c2::AcceptedSpawnBindingProof,
        bound_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        if run.intent.delivery_bundle.is_some()
            || self.engine.delivery_for_run(&run.run_id).is_some()
        {
            return Err(HarnessServiceError::AtomicDeliveryCommitRequired);
        }
        validate_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let proxy = proof.harness_mcp_proxy()
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let current = self.harness_mcp_reservations.get(&proxy.reservation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let bound = bind_harness_mcp_reservation(current, proof, bound_at_unix_ms)?;
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
            && current == &bound
        {
            return Ok(());
        }
        let prepared = self.engine.prepare_run_operation_transition(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
        )?;
        let mut reservations = self.harness_mcp_reservations.clone();
        reservations.insert(bound.reservation_id.clone(), bound);
        self.commit_prepared_state(prepared, accepted_contexts, reservations)
    }

    pub fn transition_run_with_accepted_harness_mcp_spawn_and_delivery(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_delivery_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
        proof: &c2::AcceptedSpawnBindingProof,
        bound_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        validate_delivery_accepted_spawn_proof(
            &self.engine,
            &self.dispatch_contexts,
            &self.issued_launches,
            &delivery,
            &run,
            proof,
        )?;
        let accepted_contexts = contexts_with_accepted_spawn_binding(
            &self.dispatch_contexts,
            &self.issued_launches,
            &run,
            &operation,
            proof,
        )?;
        let proxy = proof.harness_mcp_proxy()
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let current = self.harness_mcp_reservations.get(&proxy.reservation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        let bound = bind_harness_mcp_reservation(current, proof, bound_at_unix_ms)?;
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
            && self.engine.delivery(&delivery.delivery_ref) == Some(&delivery)
            && current == &bound
        {
            return Ok(());
        }
        let prepared = self.engine.prepare_accepted_spawn_delivery_commit(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
            expected_delivery_revision,
            delivery,
        )?;
        let mut reservations = self.harness_mcp_reservations.clone();
        reservations.insert(bound.reservation_id.clone(), bound);
        self.commit_prepared_state(prepared, accepted_contexts, reservations)
    }

    /// Atomically publishes the Dispatching operation and the privacy-safe
    /// metadata required to reconcile a lost spawn receipt after restart.
    pub fn begin_dispatch(
        &mut self,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        context: HarnessDispatchContextV1,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        context.validate()?;
        if context.managed_worktree_binding.is_some() {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch cannot predeclare a managed worktree receipt",
            ));
        }
        if operation.run_id.is_some() {
            return Err(HarnessServiceError::NonAtomicRunOperation);
        }
        if self.engine.operation(&operation.operation_id) == Some(&operation)
            && self.dispatch_contexts.get(&context.operation_id) == Some(&context)
        {
            return Ok(());
        }
        if operation.operation_id != context.operation_id
            || operation.idempotency_ref != context.idempotency_ref
            || operation.state != HarnessOperationStateV1::Dispatching
            || operation.dispatched_at_unix_ms != Some(context.dispatched_at_unix_ms)
        {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch context does not match operation",
            ));
        }
        let prepared = self.engine.prepare_operation_transition(
            expected_operation_revision,
            operation,
        )?;
        let mut contexts = self.dispatch_contexts.clone();
        if contexts.insert(context.operation_id.clone(), context).is_some() {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch context already exists",
            ));
        }
        self.commit_prepared(prepared, contexts)
    }

    pub fn begin_run_dispatch(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        context: HarnessDispatchContextV1,
        spawn_spec: &SpawnSpec,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        context.validate()?;
        if context.managed_worktree_binding.is_some() {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "run dispatch cannot predeclare a managed worktree receipt",
            ));
        }
        let actual_spawn_spec_fingerprint = c2::spawn_spec_fingerprint(spawn_spec)
            .map_err(|_| HarnessServiceError::DispatchFingerprint)?;
        validate_run_dispatch_seam(
            &self.engine,
            &self.issued_launches,
            &run,
            &context,
            spawn_spec,
            &actual_spawn_spec_fingerprint,
        )?;
        if self.engine.run(&run.run_id) == Some(&run)
            && self.engine.operation(&operation.operation_id) == Some(&operation)
            && self.dispatch_contexts.get(&context.operation_id) == Some(&context)
        {
            return Ok(());
        }
        if run.operation_id != operation.operation_id
            || operation.operation_id != context.operation_id
            || operation.idempotency_ref != context.idempotency_ref
            || operation.state != HarnessOperationStateV1::Dispatching
            || operation.dispatched_at_unix_ms != Some(context.dispatched_at_unix_ms)
            || run.lifecycle != gate4agent_harness_protocol::HarnessRunLifecycleV1::Dispatching
        {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "run dispatch context does not match operation",
            ));
        }
        let prepared = self.engine.prepare_run_operation_transition(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
        )?;
        let mut contexts = self.dispatch_contexts.clone();
        if contexts.insert(context.operation_id.clone(), context).is_some() {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch context already exists",
            ));
        }
        self.commit_prepared(prepared, contexts)
    }

    /// Atomically publishes Dispatching run authority and a privacy-safe H3B
    /// reservation. The returned handle is intentionally non-cloneable and is
    /// useful only for the single Arm request.
    pub fn begin_run_dispatch_with_harness_mcp(
        &mut self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        context: HarnessDispatchContextV1,
        spawn_spec: &SpawnSpec,
        reservation_id: HarnessMcpReservationId,
        grant_id: SessionGrantId,
        grant_revision: HarnessRevision,
        expires_at_unix_ms: u64,
    ) -> Result<PreparedHarnessMcpReservation, HarnessServiceError> {
        self.ensure_healthy()?;
        context.validate()?;
        if context.managed_worktree_binding.is_some() {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "H3B dispatch cannot predeclare a managed worktree receipt",
            ));
        }
        let fingerprint = c2::spawn_spec_fingerprint(spawn_spec)
            .map_err(|_| HarnessServiceError::DispatchFingerprint)?;
        validate_run_dispatch_seam(
            &self.engine,
            &self.issued_launches,
            &run,
            &context,
            spawn_spec,
            &fingerprint,
        )?;
        validate_harness_mcp_grant(
            &self.engine,
            &run,
            &operation,
            &context,
            &grant_id,
            grant_revision,
        )?;
        if expires_at_unix_ms <= context.dispatched_at_unix_ms
            || expires_at_unix_ms - context.dispatched_at_unix_ms
                > MAX_HARNESS_MCP_RESERVATION_TTL_MS
        {
            return Err(HarnessServiceError::InvalidHarnessMcpReservation(
                "reservation expiry is not future and bounded",
            ));
        }
        if run.operation_id != operation.operation_id
            || operation.operation_id != context.operation_id
            || operation.idempotency_ref != context.idempotency_ref
            || operation.state != HarnessOperationStateV1::Dispatching
            || operation.dispatched_at_unix_ms != Some(context.dispatched_at_unix_ms)
            || run.lifecycle != gate4agent_harness_protocol::HarnessRunLifecycleV1::Dispatching
        {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "H3B run dispatch context does not match operation",
            ));
        }
        let activation_digest = harness_mcp_activation_digest(
            &reservation_id,
            &grant_id,
            grant_revision,
            &run.run_id,
            &operation.operation_id,
            &context,
            expires_at_unix_ms,
        )?;
        let reservation = HarnessMcpReservationV1 {
            reservation_id: reservation_id.clone(),
            revision: HarnessRevision::new(1)?,
            state: HarnessMcpReservationStateV1::Prepared,
            activation_digest,
            grant_id,
            grant_revision,
            // `validate_harness_mcp_grant` above already accepted this
            // operation's actor against `run` and its grant, for either H3B
            // shape (a) `ParentRun` or (b) `User` on an operator-started
            // top-level run whose grant was minted with
            // `actor_run_id == run.run_id` -- so the reservation's own
            // `actor_run_id` is the parent for (a) and the run itself for
            // (b), and both already equal `grant.actor_run_id`.
            actor_run_id: match &operation.actor {
                HarnessActorV1::ParentRun { run_id } => run_id.clone(),
                HarnessActorV1::User { .. } => run.run_id.clone(),
            },
            operation_id: operation.operation_id.clone(),
            node_id: context.node_id.clone(),
            node_incarnation_id: context.node_incarnation_id.clone(),
            workspace_id: context.workspace_id.clone(),
            provider_profile: context.provider_profile.clone(),
            expected_provider: context.expected_provider.clone(),
            mode: context.mode,
            spawn_spec_fingerprint: fingerprint,
            idempotency_ref: context.idempotency_ref.clone(),
            expires_at_unix_ms,
            record_id: None,
            instance_id: None,
            generation: None,
            created_at_unix_ms: context.dispatched_at_unix_ms,
            updated_at_unix_ms: context.dispatched_at_unix_ms,
        };
        reservation.validate()?;

        if let Some(current) = self.harness_mcp_reservations.get(&reservation_id) {
            if current == &reservation
                && self.engine.run(&run.run_id) == Some(&run)
                && self.engine.operation(&operation.operation_id) == Some(&operation)
                && self.dispatch_contexts.get(&context.operation_id) == Some(&context)
            {
                return Ok(PreparedHarnessMcpReservation { reservation });
            }
            return Err(HarnessServiceError::HarnessMcpReplayMismatch);
        }
        if self.harness_mcp_reservations.values().any(|existing| {
            existing.operation_id == operation.operation_id
        }) {
            return Err(HarnessServiceError::HarnessMcpReplayMismatch);
        }
        let prepared = self.engine.prepare_run_operation_transition(
            expected_run_revision,
            run,
            expected_operation_revision,
            operation,
        )?;
        let mut contexts = self.dispatch_contexts.clone();
        if contexts.insert(context.operation_id.clone(), context).is_some() {
            return Err(HarnessServiceError::InvalidDispatchContext(
                "dispatch context already exists",
            ));
        }
        let mut reservations = self.harness_mcp_reservations.clone();
        reservations.insert(reservation_id, reservation.clone());
        self.commit_prepared_state(prepared, contexts, reservations)?;
        Ok(PreparedHarnessMcpReservation { reservation })
    }

    pub fn record_harness_mcp_armed(
        &mut self,
        proof: c2::ArmedHarnessMcpReservationProof,
        armed_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.harness_mcp_reservations
            .get(proof.reservation_id())
            .ok_or(HarnessServiceError::HarnessMcpArmDurableLookupMissing {
                missing: "reservation",
                operation_id: None,
                reservation_id: Some(proof.reservation_id().clone()),
            })?;
        proof.validate_record(current)?;
        if current.state == HarnessMcpReservationStateV1::Armed {
            return Ok(());
        }
        if current.state != HarnessMcpReservationStateV1::Prepared
            || armed_at_unix_ms < current.updated_at_unix_ms
            || armed_at_unix_ms >= current.expires_at_unix_ms
        {
            return Err(HarnessServiceError::HarnessMcpArmReservationNotReadyRefused {
                reservation_id: current.reservation_id.clone(),
                state: current.state,
                armed_at_unix_ms,
                updated_at_unix_ms: current.updated_at_unix_ms,
                expires_at_unix_ms: current.expires_at_unix_ms,
            });
        }
        let mut next = current.clone();
        next.revision = next_revision(next.revision)?;
        next.state = HarnessMcpReservationStateV1::Armed;
        next.updated_at_unix_ms = armed_at_unix_ms;
        self.commit_reservation_only(next)
    }

    pub(crate) fn record_harness_mcp_armed_and_issue_spawn_lease(
        &mut self,
        catalog: &HarnessLaunchCatalog,
        proof: c2::ArmedHarnessMcpReservationProof,
        armed_at_unix_ms: u64,
        spec: SpawnSpec,
    ) -> Result<c2::PreparedSpawnDispatch, HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.harness_mcp_reservations
            .get(proof.reservation_id())
            .ok_or(HarnessServiceError::HarnessMcpArmDurableLookupMissing {
                missing: "reservation",
                operation_id: None,
                reservation_id: Some(proof.reservation_id().clone()),
            })?;
        proof.validate_record(current)?;
        if current.state != HarnessMcpReservationStateV1::Prepared
            || armed_at_unix_ms < current.updated_at_unix_ms
            || armed_at_unix_ms >= current.expires_at_unix_ms
        {
            return Err(HarnessServiceError::HarnessMcpArmReservationNotReadyRefused {
                reservation_id: current.reservation_id.clone(),
                state: current.state,
                armed_at_unix_ms,
                updated_at_unix_ms: current.updated_at_unix_ms,
                expires_at_unix_ms: current.expires_at_unix_ms,
            });
        }
        let operation = self.engine.operation(&current.operation_id)
            .ok_or(HarnessServiceError::HarnessMcpArmDurableLookupMissing {
                missing: "operation",
                operation_id: Some(current.operation_id.clone()),
                reservation_id: Some(current.reservation_id.clone()),
            })?;
        let run = operation.run_id.as_ref()
            .and_then(|run_id| self.engine.run(run_id))
            .ok_or(HarnessServiceError::HarnessMcpArmDurableLookupMissing {
                missing: "run",
                operation_id: Some(current.operation_id.clone()),
                reservation_id: Some(current.reservation_id.clone()),
            })?;
        let scheduled = self.scheduled_launches.get(&current.operation_id)
            .ok_or(HarnessServiceError::HarnessMcpArmDurableLookupMissing {
                missing: "scheduled-launch",
                operation_id: Some(current.operation_id.clone()),
                reservation_id: Some(current.reservation_id.clone()),
            })?;
        let plan = catalog.resolve_scheduled(scheduled)?;
        plan.validate_intent(&run.intent)?;
        let context = self.dispatch_contexts.get(&current.operation_id)
            .ok_or(HarnessServiceError::HarnessMcpArmDurableLookupMissing {
                missing: "dispatch-context",
                operation_id: Some(current.operation_id.clone()),
                reservation_id: Some(current.reservation_id.clone()),
            })?;
        let fingerprint = c2::spawn_spec_fingerprint(&spec)
            .map_err(|_| HarnessServiceError::DispatchFingerprint)?;
        validate_run_dispatch_seam(
            &self.engine,
            &self.issued_launches,
            run,
            context,
            &spec,
            &fingerprint,
        )?;
        let route = gate4agent_c2_protocol::NodeRoute {
            node_id: gate4agent_node_protocol::NodeId::new(context.node_id.as_str())
                .map_err(|_| HarnessServiceError::HarnessMcpArmRouteInvalid {
                    operation_id: current.operation_id.clone(),
                    field: "node_id",
                    value: context.node_id.as_str().to_owned(),
                })?,
            expected_incarnation_id: context.node_incarnation_id.as_str().parse()
                .map_err(|_| HarnessServiceError::HarnessMcpArmRouteInvalid {
                    operation_id: current.operation_id.clone(),
                    field: "node_incarnation_id",
                    value: context.node_incarnation_id.as_str().to_owned(),
                })?,
        };
        let mut armed = current.clone();
        armed.revision = next_revision(armed.revision)?;
        armed.state = HarnessMcpReservationStateV1::Armed;
        armed.updated_at_unix_ms = armed_at_unix_ms;
        let prepared = prepared_scheduled_spawn_dispatch(
            &self.engine,
            &self.issued_launches,
            plan,
            route,
            operation,
            spec,
            fingerprint,
            Some(&armed),
        )?;
        self.commit_reservation_only(armed)?;
        Ok(prepared)
    }

    pub fn record_harness_mcp_active(
        &mut self,
        proof: c2::ActivatedHarnessMcpReservationProof,
        activated_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.harness_mcp_reservations.get(proof.reservation_id())
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        proof.validate_record(current)?;
        if current.state == HarnessMcpReservationStateV1::Active {
            return Ok(());
        }
        if current.state != HarnessMcpReservationStateV1::Bound
            || activated_at_unix_ms < current.updated_at_unix_ms
            || activated_at_unix_ms >= current.expires_at_unix_ms
        {
            return Err(HarnessServiceError::HarnessMcpProofMismatch);
        }
        let mut next = current.clone();
        next.revision = next_revision(next.revision)?;
        next.state = HarnessMcpReservationStateV1::Active;
        next.updated_at_unix_ms = activated_at_unix_ms;
        self.commit_reservation_only(next)
    }

    pub fn revoke_harness_mcp_reservation(
        &mut self,
        reservation_id: &HarnessMcpReservationId,
        revoked_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        let current = self.harness_mcp_reservations.get(reservation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        if current.state == HarnessMcpReservationStateV1::Revoked {
            return Ok(());
        }
        if revoked_at_unix_ms < current.updated_at_unix_ms {
            return Err(HarnessServiceError::HarnessMcpProofMismatch);
        }
        let mut next = current.clone();
        next.revision = next_revision(next.revision)?;
        next.state = HarnessMcpReservationStateV1::Revoked;
        next.updated_at_unix_ms = revoked_at_unix_ms;
        self.commit_reservation_only(next)
    }

    /// Closes durable authority before best-effort Node cleanup. A transport
    /// failure can leave only a harmless Node-local revoked cleanup retry; it
    /// can never leave the service authorizing the proxy.
    pub async fn revoke_and_abort_harness_mcp_reservation(
        &mut self,
        adapter: &c2::HarnessC2Adapter,
        route: &gate4agent_c2_protocol::NodeRoute,
        reservation_id: &HarnessMcpReservationId,
        revoked_at_unix_ms: u64,
    ) -> Result<(), HarnessServiceError> {
        let activation_digest = self.harness_mcp_reservations.get(reservation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?
            .activation_digest.clone();
        self.revoke_harness_mcp_reservation(reservation_id, revoked_at_unix_ms)?;
        let _ = adapter.abort_harness_mcp_reservation(
            route,
            reservation_id,
            &activation_digest,
        ).await;
        Ok(())
    }

    pub(crate) fn validate_bound_harness_mcp_authority(
        &self,
        reservation_id: &HarnessMcpReservationId,
    ) -> Result<(HarnessMcpReservationV1, SessionRecordId, SessionAddress), HarnessServiceError> {
        let reservation = self.harness_mcp_reservations.get(reservation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
        if !matches!(
            reservation.state,
            HarnessMcpReservationStateV1::Bound | HarnessMcpReservationStateV1::Active
        ) {
            return Err(HarnessServiceError::HarnessMcpProofMismatch);
        }
        validate_reservation_durable_context(
            &self.engine,
            &self.dispatch_contexts,
            reservation,
            false,
        )?;
        let record_id = SessionRecordId::new(
            reservation.record_id.as_ref()
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?.as_str(),
        ).map_err(|_| HarnessServiceError::HarnessMcpProofMismatch)?;
        let workspace_id = gate4agent_node_protocol::WorkspaceId::new(
            reservation.workspace_id.as_str(),
        ).map_err(|_| HarnessServiceError::HarnessMcpProofMismatch)?;
        let session = SessionAddress {
            workspace_id,
            session: gate4agent_node_protocol::SessionKey {
                instance_id: gate4agent_types::AgentInstanceId(
                    reservation.instance_id.ok_or(HarnessServiceError::HarnessMcpProofMismatch)?,
                ),
                generation: gate4agent_types::SessionGeneration(
                    reservation.generation.ok_or(HarnessServiceError::HarnessMcpProofMismatch)?,
                ),
            },
        };
        Ok((reservation.clone(), record_id, session))
    }

    pub(crate) fn validate_activatable_harness_mcp_authority(
        &self,
        reservation_id: &HarnessMcpReservationId,
        now_unix_ms: u64,
    ) -> Result<(HarnessMcpReservationV1, SessionRecordId, SessionAddress), HarnessServiceError> {
        let authority = self.validate_bound_harness_mcp_authority(reservation_id)?;
        if authority.0.state == HarnessMcpReservationStateV1::Bound
            && now_unix_ms >= authority.0.expires_at_unix_ms
        {
            return Err(HarnessServiceError::HarnessMcpProofMismatch);
        }
        Ok(authority)
    }

    pub(crate) fn authorize_harness_mcp_call(
        &self,
        route: &gate4agent_c2_protocol::NodeRoute,
        reservation_id: &HarnessMcpReservationId,
        activation_digest: &HarnessMcpActivationDigest,
        record_id: &SessionRecordId,
        session: &SessionAddress,
    ) -> Result<credential::CredentialBindingV1, HarnessServiceError> {
        self.ensure_healthy()?;
        let (reservation, expected_record, expected_session) =
            self.validate_bound_harness_mcp_authority(reservation_id)?;
        // Six independent facts decide this, and every one of them answers the
        // caller with the same opaque `HarnessMcpProofMismatch`. Live that
        // reads as "harness read unavailable" at the agent with no way to tell
        // a reservation that has not activated YET from one bound to another
        // node, incarnation, record or session. Name the one that fired.
        let mismatch = if reservation.state != HarnessMcpReservationStateV1::Active {
            Some("reservation is not Active")
        } else if &reservation.activation_digest != activation_digest {
            Some("activation digest differs")
        } else if reservation.node_id.as_str() != route.node_id.as_str() {
            Some("reservation belongs to another node")
        } else if reservation.node_incarnation_id.as_str()
            != route.expected_incarnation_id.to_string()
        {
            Some("reservation belongs to another node incarnation")
        } else if &expected_record != record_id {
            Some("reservation is bound to another session record")
        } else if &expected_session != session {
            Some("reservation is bound to another session address")
        } else {
            None
        };
        if let Some(reason) = mismatch {
            tracing::warn!(
                reservation_id = reservation_id.as_str(),
                reservation_state = ?reservation.state,
                reason,
                "harness MCP call proof mismatch",
            );
            return Err(HarnessServiceError::HarnessMcpProofMismatch);
        }
        Ok(harness_mcp_credential_binding(reservation)?)
    }

    pub(crate) fn harness_mcp_binding_for_reconcile(
        &self,
        reservation_id: &HarnessMcpReservationId,
    ) -> Result<credential::CredentialBindingV1, HarnessServiceError> {
        let (reservation, _, _) = self.validate_bound_harness_mcp_authority(reservation_id)?;
        harness_mcp_credential_binding(reservation)
    }

}

fn prepared_scheduled_spawn_dispatch(
    engine: &HarnessEngine,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    plan: &dispatch::HarnessLaunchPlanV1,
    route: gate4agent_c2_protocol::NodeRoute,
    operation: &HarnessOperationV1,
    spec: SpawnSpec,
    fingerprint: HarnessRequestDigest,
    harness_mcp: Option<&HarnessMcpReservationV1>,
) -> Result<c2::PreparedSpawnDispatch, HarnessServiceError> {
    let run = operation.run_id.as_ref()
        .and_then(|run_id| engine.run(run_id))
        .ok_or(HarnessServiceError::InvalidDispatchContext(
            "scheduled spawn run is missing",
        ))?;
    let grant_authority = match &plan.grant {
        HarnessGrantPolicyV1::Operator => None,
        HarnessGrantPolicyV1::Exact { grant_id, revision } => {
            Some(HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id: grant_id.clone(),
                revision: *revision,
            })
        }
    };
    let mut prepared = c2::PreparedSpawnDispatch::new(
        route,
        operation.operation_id.clone(),
        operation.idempotency_ref.clone(),
        spec,
        fingerprint,
    ).map_err(|_| HarnessServiceError::InvalidDispatchContext(
        "scheduled spawn request is invalid",
    ))?;

    match engine.delivery_for_run(&run.run_id) {
        None if plan.delivery.is_none() => {}
        Some(delivery) => {
            let authority_matches = match &delivery.authority {
                HarnessTransferAuthorityRefV1::ParentGrant { .. } => {
                    grant_authority.as_ref() == Some(&delivery.authority)
                        && plan.delivery.as_ref().is_some_and(|policy| {
                            delivery.bundle.selector == policy.selector
                                && delivery.bundle.bundle_id.as_str()
                                    == policy.bundle_id.as_str()
                        })
                }
                HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                    issued_launches.get(&run.operation_id).is_some_and(|current| {
                        current.reference() == *issuance
                            && current.delivery.as_ref().is_some_and(|selection| {
                                selection.bundle == delivery.bundle
                            })
                    })
                }
            };
            if delivery.state != HarnessDeliveryStateV1::Staged
                || delivery.operation_id != operation.operation_id
                || !authority_matches
            {
                return Err(HarnessServiceError::InvalidDispatchContext(
                    "scheduled delivery authority does not match exact plan and grant",
                ));
            }
            let expected = gate4agent_node_protocol::ResolvedBundleReceipt {
                id: gate4agent_node_protocol::SpawnBundleId::new(
                    delivery.bundle.bundle_id.as_str(),
                ).map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "scheduled delivery bundle id is invalid",
                ))?,
                revision: gate4agent_node_protocol::SpawnBundleRevision::new(
                    delivery.bundle.revision.as_str(),
                ).map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "scheduled delivery revision is invalid",
                ))?,
                digest: gate4agent_node_protocol::SpawnBundleDigest::new(
                    delivery.bundle.digest.as_str(),
                ).map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "scheduled delivery digest is invalid",
                ))?,
            };
            prepared = prepared.with_expected_bundle(expected)
                .map_err(|_| HarnessServiceError::InvalidDispatchContext(
                    "scheduled delivery override is not exact",
                ))?;
        }
        _ => return Err(HarnessServiceError::InvalidDispatchContext(
            "scheduled delivery presence does not match durable plan",
        )),
    }

    match engine.continuation_for_run(&run.run_id) {
        None if plan.continuation == dispatch::HarnessContinuationPolicyV1::None => {}
        Some(continuation) => {
            let authority_matches = match &continuation.authority {
                HarnessTransferAuthorityRefV1::ParentGrant { .. } => {
                    plan.continuation == dispatch::HarnessContinuationPolicyV1::ParentRun
                        && grant_authority.as_ref() == Some(&continuation.authority)
                }
                HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                    issued_launches.get(&run.operation_id).is_some_and(|current| {
                        current.reference() == *issuance
                            && current.context_source.as_ref().is_some_and(|source| {
                                source.source_run_id == continuation.source_run_id
                            })
                    })
                }
            };
            if continuation.state != HarnessContinuationStateV1::Exported
                || continuation.operation_id != operation.operation_id
                || !authority_matches
            {
                return Err(HarnessServiceError::InvalidContinuationProof(
                    "scheduled continuation authority does not match exact plan and grant",
                ));
            }
            let expected = c2::harness_context_to_node(
                continuation.context.as_ref().ok_or(
                    HarnessServiceError::InvalidContinuationProof(
                        "scheduled continuation has no exported context receipt",
                    ),
                )?,
            ).map_err(|_| HarnessServiceError::InvalidContinuationProof(
                "scheduled continuation context receipt is invalid",
            ))?;
            prepared = prepared.with_expected_context(expected)
                .map_err(|_| HarnessServiceError::InvalidContinuationProof(
                    "scheduled continuation override is not exact",
                ))?;
        }
        _ => return Err(HarnessServiceError::InvalidContinuationProof(
            "scheduled continuation presence does not match durable plan",
        )),
    }

    match (plan.harness_mcp, harness_mcp) {
        (dispatch::HarnessMcpPolicyV1::Disabled, None) => {}
        // `plan.grant` names the exact grant an armed H3B reservation must
        // carry only when the plan pins one (`Exact`, shape (a)'s
        // continuation/delivery flows). An `Operator`-policy plan
        // (`grant_authority` is `None` above) mints its grant per dispatch
        // through `runtime::resolve_harness_mcp_grant` instead of naming
        // one ahead of time -- `HarnessLaunchPlanV1::is_ordinary_dispatch`
        // documents this pairing as intentional since gate4agent-arc-
        // mailbox-and-task-layer Slice A(i) -- so there is no exact
        // `(grant_id, revision)` here to compare the reservation against;
        // `begin_run_dispatch_with_harness_mcp`'s own `validate_harness_mcp_
        // grant` already proved that minted grant authoritative for this
        // run and operation before the reservation was ever created, and
        // that identity is immutable for the reservation's lifetime. This
        // match used to require `grant_authority == Some(ParentGrant {..})`
        // unconditionally, which an `Operator`-policy plan can never
        // produce -- so every H3B shape (b) dispatch (an operator-started
        // top-level run) fell to the catch-all refusal below even after the
        // Node armed the reservation ("harness MCP authority proof does not
        // match current durable state", measured live on claude and codex).
        (dispatch::HarnessMcpPolicyV1::GrantBound, Some(reservation)) => {
            if reservation.state != HarnessMcpReservationStateV1::Armed {
                return Err(HarnessServiceError::HarnessMcpLaunchReservationNotArmedRefused {
                    reservation_id: reservation.reservation_id.clone(),
                    state: reservation.state,
                });
            }
            if reservation.operation_id != operation.operation_id {
                return Err(HarnessServiceError::HarnessMcpLaunchOperationRefused {
                    reservation_operation_id: reservation.operation_id.clone(),
                    dispatch_operation_id: operation.operation_id.clone(),
                });
            }
            if let HarnessGrantPolicyV1::Exact { grant_id, revision } = &plan.grant {
                if &reservation.grant_id != grant_id || reservation.grant_revision != *revision {
                    return Err(HarnessServiceError::HarnessMcpLaunchGrantRefused {
                        plan_grant_id: grant_id.clone(),
                        plan_grant_revision: *revision,
                        reservation_grant_id: reservation.grant_id.clone(),
                        reservation_grant_revision: reservation.grant_revision,
                    });
                }
            }
            prepared = prepared.with_harness_mcp(
                reservation,
                reservation.expires_at_unix_ms,
            );
        }
        (plan_policy, reservation) => {
            return Err(HarnessServiceError::HarnessMcpLaunchPolicyRefused {
                plan_policy: match plan_policy {
                    dispatch::HarnessMcpPolicyV1::Disabled => "disabled",
                    dispatch::HarnessMcpPolicyV1::GrantBound => "grant-bound",
                },
                reservation_present: reservation.is_some(),
            });
        }
    }
    Ok(prepared)
}

fn harness_mcp_credential_binding(
    reservation: HarnessMcpReservationV1,
) -> Result<credential::CredentialBindingV1, HarnessServiceError> {
    Ok(credential::CredentialBindingV1 {
            grant_id: reservation.grant_id,
            grant_revision: reservation.grant_revision,
            actor_run_id: reservation.actor_run_id,
            node_id: reservation.node_id,
            workspace_id: reservation.workspace_id,
            node_incarnation: reservation.node_incarnation_id,
            record_id: reservation.record_id
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?,
            instance_id: reservation.instance_id
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?,
            generation: reservation.generation
                .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?,
        })
}

impl HarnessService {

    pub fn flush(&mut self) -> Result<(), HarnessServiceError> {
        self.ensure_healthy()?;
        self.store.as_mut().expect("open harness store").flush()?;
        Ok(())
    }

    pub fn close(mut self) -> Result<(), HarnessServiceError> {
        if let Some(store) = self.store.take() {
            store.close()?;
        }
        Ok(())
    }

    fn ensure_healthy(&self) -> Result<(), HarnessServiceError> {
        if self.poisoned {
            Err(HarnessServiceError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn replay_operator_request(
        &self,
        operation_id: &HarnessOperationId,
        request_digest: &HarnessRequestDigest,
        expected_kind: HarnessOperationKindV1,
    ) -> Result<Option<HarnessApplyOutcome>, HarnessServiceError> {
        let Some(stored_digest) = self.operator_requests.get(operation_id) else {
            if self.engine.operation(operation_id).is_some() {
                return Err(HarnessServiceError::OperatorRequestConflict {
                    operation_id: operation_id.clone(),
                });
            }
            return Ok(None);
        };
        let operation = self.engine.operation(operation_id)
            .ok_or(HarnessServiceError::Corrupt("operator request operation is missing"))?;
        if stored_digest != request_digest || operation.kind != expected_kind {
            return Err(HarnessServiceError::OperatorRequestConflict {
                operation_id: operation_id.clone(),
            });
        }
        Ok(Some(HarnessApplyOutcome::Replayed))
    }

    fn operator_replace_task_state(
        &mut self,
        authority: &HarnessOperatorAuthorityV1,
        task_id: HarnessTaskId,
        expected_revision: HarnessRevision,
        state: HarnessTaskStateV1,
        command_digest: HarnessRequestDigest,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        let mut task = self.engine.task(&task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(task_id.to_string()))?
            .clone();
        task.revision = next_harness_revision(task.revision, "task")?;
        task.state = state;
        task.updated_at_unix_ms = authority.now_unix_ms;
        let operation = operator_task_operation(
            authority,
            operator_actor(authority),
            HarnessOperationKindV1::MutateTask,
            task_id,
            Some(expected_revision),
        )?;
        self.commit_operator_mutation(
            HarnessMutationV1::ReplaceTask {
                operation,
                expected_revision,
                task,
            },
            command_digest,
        )
    }

    fn commit_operator_mutation(
        &mut self,
        mut mutation: HarnessMutationV1,
        command_digest: HarnessRequestDigest,
    ) -> Result<HarnessApplyOutcome, HarnessServiceError> {
        mutation.operation_mut().request_digest = mutation_request_digest(&mutation)?;
        let prepared = self.engine.prepare(mutation)?;
        let outcome = prepared.outcome();
        self.commit_operator_prepared(prepared, command_digest)?;
        Ok(outcome)
    }

    fn commit_operator_prepared(
        &mut self,
        prepared: PreparedHarnessMutation,
        command_digest: HarnessRequestDigest,
    ) -> Result<(), HarnessServiceError> {
        self.commit_operator_prepared_with_launch(prepared, command_digest, None)
    }

    fn commit_operator_prepared_with_launch(
        &mut self,
        prepared: PreparedHarnessMutation,
        command_digest: HarnessRequestDigest,
        scheduled: Option<HarnessScheduledLaunchRefV1>,
    ) -> Result<(), HarnessServiceError> {
        self.commit_operator_prepared_with_launch_and_issuance(
            prepared,
            command_digest,
            scheduled,
            None,
        )
    }

    fn commit_operator_prepared_with_launch_and_issuance(
        &mut self,
        prepared: PreparedHarnessMutation,
        command_digest: HarnessRequestDigest,
        scheduled: Option<HarnessScheduledLaunchRefV1>,
        issued_launch: Option<HarnessTaskLaunchIssuanceV1>,
    ) -> Result<(), HarnessServiceError> {
        let mut operator_requests = self.operator_requests.clone();
        operator_requests.insert(prepared.operation().operation_id.clone(), command_digest);
        let mut scheduled_launches = self.scheduled_launches.clone();
        if let Some(scheduled) = scheduled {
            scheduled_launches.insert(prepared.operation().operation_id.clone(), scheduled);
        }
        let mut issued_launches = self.issued_launches.clone();
        if let Some(issuance) = issued_launch {
            let operation_id = prepared.operation().operation_id.clone();
            let checkpoint = prepared.checkpoint();
            let run = prepared.operation().run_id.as_ref()
                .and_then(|run_id| checkpoint.runs.iter().find(|run| &run.run_id == run_id))
                .ok_or_else(|| HarnessServiceError::InvalidTaskLaunchSelection {
                    task_id: issuance.task_id.clone(),
                    plan_id: Some(issuance.plan.plan_id.clone()),
                    why: "prepared mutation names no run for its own issued launch",
                })?;
            validate_issued_launch_snapshot(&issuance, run)?;
            let issuance_task_id = issuance.task_id.clone();
            let issuance_plan_id = issuance.plan.plan_id.clone();
            if issued_launches.insert(operation_id, issuance).is_some() {
                return Err(HarnessServiceError::InvalidTaskLaunchSelection {
                    task_id: issuance_task_id,
                    plan_id: Some(issuance_plan_id),
                    why: "operation already carries a committed launch issuance",
                });
            }
        }
        let reservations = reconcile_harness_mcp_reservations(
            &prepared,
            &self.dispatch_contexts,
            self.harness_mcp_reservations.clone(),
        )?;
        let checkpoint = HarnessServiceCheckpointV1 {
            version: HARNESS_SERVICE_CHECKPOINT_VERSION_V1,
            engine: prepared.checkpoint(),
            dispatch_contexts: self.dispatch_contexts.values().cloned().collect(),
            deliveries: prepared.checkpoint_deliveries(),
            continuations: prepared.checkpoint_continuations(),
            harness_mcp_reservations: reservations.values().cloned().collect(),
            operator_requests: operator_request_records(&operator_requests),
            scheduled_launches: scheduled_launches.clone(),
            issued_launches: issued_launches.clone(),
        };
        let persisted = encode_persisted_state(&checkpoint)?;
        let tail = encode_operation(prepared.operation())?;
        let result = self.store.as_mut().expect("open harness store").commit(&persisted, &tail);
        if let Err(error) = result {
            if matches!(error, HarnessStoreError::CommitAmbiguous(_)) {
                self.poisoned = true;
            }
            return Err(error.into());
        }
        self.engine.accept(prepared);
        self.harness_mcp_reservations = reservations;
        self.operator_requests = operator_requests;
        self.scheduled_launches = scheduled_launches;
        self.issued_launches = issued_launches;
        Ok(())
    }

    fn scheduler_pending_dispatch(
        &self,
    ) -> Result<Option<(&HarnessTaskV1, &HarnessRunV1, &HarnessOperationV1)>, HarnessServiceError> {
        let operation_ids = self.operator_requests.keys().cloned().collect::<BTreeSet<_>>();
        Ok(self.engine.scheduler_pending_dispatches_for(&operation_ids)
            .map_err(map_scheduler_error)?
            .into_iter()
            .next())
    }

    fn scheduler_ready_task(&self) -> Result<Option<&HarnessTaskV1>, HarnessServiceError> {
        self.engine.scheduler_ready_task().map_err(map_scheduler_error)
    }

    fn scheduler_task_has_nonterminal_run(
        &self,
        task: &HarnessTaskV1,
    ) -> Result<bool, HarnessServiceError> {
        self.engine.task_has_nonterminal_run(task).map_err(map_scheduler_error)
    }

    fn replayed_dispatch_intent_for_operation(
        &self,
        operation_id: &HarnessOperationId,
    ) -> Result<HarnessDispatchIntentV1, HarnessServiceError> {
        let operation = self.engine.operation(operation_id)
            .ok_or(HarnessServiceError::SchedulerInvalidGraph(
                "scheduler operation is missing",
            ))?;
        let run = operation.run_id.as_ref().and_then(|run_id| self.engine.run(run_id))
            .ok_or(HarnessServiceError::SchedulerInvalidGraph(
                "scheduler run is missing",
            ))?;
        let task = self.engine.task(&run.task_id)
            .ok_or(HarnessServiceError::SchedulerInvalidGraph(
                "scheduler task is missing",
            ))?;
        replayed_dispatch_intent(task, run, operation)
    }

    fn commit_prepared(
        &mut self,
        prepared: PreparedHarnessMutation,
        contexts: BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    ) -> Result<(), HarnessServiceError> {
        let reservations = reconcile_harness_mcp_reservations(
            &prepared,
            &contexts,
            self.harness_mcp_reservations.clone(),
        )?;
        self.commit_prepared_state(
            prepared,
            contexts,
            reservations,
        )
    }

    fn commit_prepared_state(
        &mut self,
        prepared: PreparedHarnessMutation,
        contexts: BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
        reservations: BTreeMap<HarnessMcpReservationId, HarnessMcpReservationV1>,
    ) -> Result<(), HarnessServiceError> {
        #[cfg(test)]
        if self.store.is_none() {
            self.engine.accept(prepared);
            self.dispatch_contexts = contexts;
            self.harness_mcp_reservations = reservations;
            return Ok(());
        }
        let checkpoint = HarnessServiceCheckpointV1 {
            version: HARNESS_SERVICE_CHECKPOINT_VERSION_V1,
            engine: prepared.checkpoint(),
            dispatch_contexts: contexts.values().cloned().collect(),
            deliveries: prepared.checkpoint_deliveries(),
            continuations: prepared.checkpoint_continuations(),
            harness_mcp_reservations: reservations.values().cloned().collect(),
            operator_requests: operator_request_records(&self.operator_requests),
            scheduled_launches: self.scheduled_launches.clone(),
            issued_launches: self.issued_launches.clone(),
        };
        let persisted = encode_persisted_state(&checkpoint)?;
        let tail = encode_operation(prepared.operation())?;
        let result = self.store.as_mut().expect("open harness store").commit(&persisted, &tail);
        if let Err(error) = result {
            if matches!(error, HarnessStoreError::CommitAmbiguous(_)) {
                self.poisoned = true;
            }
            return Err(error.into());
        }
        self.engine.accept(prepared);
        self.dispatch_contexts = contexts;
        self.harness_mcp_reservations = reservations;
        Ok(())
    }

    fn commit_reservation_only(
        &mut self,
        reservation: HarnessMcpReservationV1,
    ) -> Result<(), HarnessServiceError> {
        reservation.validate()?;
        validate_reservation_durable_context(
            &self.engine,
            &self.dispatch_contexts,
            &reservation,
            reservation.state == HarnessMcpReservationStateV1::Revoked,
        )?;
        let operation = self.engine.operation(&reservation.operation_id)
            .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?
            .clone();
        let mut reservations = self.harness_mcp_reservations.clone();
        reservations.insert(reservation.reservation_id.clone(), reservation);
        let engine = self.engine.checkpoint();
        let checkpoint = HarnessServiceCheckpointV1 {
            version: HARNESS_SERVICE_CHECKPOINT_VERSION_V1,
            deliveries: engine.deliveries.clone(),
            continuations: engine.continuations.clone(),
            engine,
            dispatch_contexts: self.dispatch_contexts.values().cloned().collect(),
            harness_mcp_reservations: reservations.values().cloned().collect(),
            operator_requests: operator_request_records(&self.operator_requests),
            scheduled_launches: self.scheduled_launches.clone(),
            issued_launches: self.issued_launches.clone(),
        };
        let persisted = encode_persisted_state(&checkpoint)?;
        let tail = encode_operation(&operation)?;
        let result = self.store.as_mut().expect("open harness store").commit(&persisted, &tail);
        if let Err(error) = result {
            if matches!(error, HarnessStoreError::CommitAmbiguous(_)) {
                self.poisoned = true;
            }
            return Err(error.into());
        }
        self.harness_mcp_reservations = reservations;
        Ok(())
    }
}

pub fn mutation_request_digest(
    mutation: &HarnessMutationV1,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    const DOMAIN: &[u8] = b"gate4agent-harness-mutation-request-v1";
    mutation.validate_payload()?;
    let mut canonical = mutation.clone();
    canonical.operation_mut().request_digest = HarnessRequestDigest::new("0".repeat(64))?;
    let encoded = serde_json::to_vec(&canonical)?;
    let digest = gate4agent_node_wire::local_hmac_sha256(DOMAIN, &encoded)
        .map_err(HarnessServiceError::MutationDigest)?;
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(HarnessRequestDigest::new(hex)?)
}

fn operator_command_digest<T: Serialize>(
    kind: &'static str,
    request: &T,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    const DOMAIN: &[u8] = b"gate4agent-harness-operator-request-v1";
    let encoded = serde_json::to_vec(&(kind, request))?;
    let digest = gate4agent_node_wire::local_hmac_sha256(DOMAIN, &encoded)
        .map_err(HarnessServiceError::MutationDigest)?;
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(HarnessRequestDigest::new(hex)?)
}

fn scheduled_launch_digest(
    scheduled: &HarnessScheduledLaunchRefV2,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    const DOMAIN: &[u8] = b"gate4agent-harness-scheduled-launch-ref-v2\0";
    scheduled.validate()?;
    let encoded = serde_json::to_vec(scheduled)?;
    let digest = gate4agent_node_wire::local_hmac_sha256(DOMAIN, &encoded)
        .map_err(HarnessServiceError::MutationDigest)?;
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(HarnessRequestDigest::new(hex)?)
}

fn deterministic_execution_spec_id(
    task_id: &HarnessTaskId,
) -> Result<HarnessExecutionSpecId, HarnessServiceError> {
    task_id.validate()?;
    let digest = gate4agent_node_wire::local_hmac_sha256(
        HARNESS_EXECUTION_SPEC_ID_DOMAIN,
        task_id.as_str().as_bytes(),
    ).map_err(HarnessServiceError::MutationDigest)?;
    let mut nonce = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        write!(&mut nonce, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(HarnessExecutionSpecId::new(format!("hespec_{nonce}"))?)
}

pub(crate) fn deterministic_launch_issuance_id(
    task_id: &HarnessTaskId,
) -> Result<HarnessTaskLaunchIssuanceId, HarnessServiceError> {
    task_id.validate()?;
    let digest = gate4agent_node_wire::local_hmac_sha256(
        HARNESS_LAUNCH_ISSUANCE_ID_DOMAIN,
        task_id.as_str().as_bytes(),
    ).map_err(HarnessServiceError::MutationDigest)?;
    let mut nonce = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        write!(&mut nonce, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(HarnessTaskLaunchIssuanceId::new(format!("hissue_{nonce}"))?)
}

pub(crate) fn task_launch_policy_digest(
    options: &HarnessTaskLaunchOptionsV1,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    #[derive(Serialize)]
    struct CanonicalLaunchPolicy<'a> {
        task_id: &'a HarnessTaskId,
        task_revision: HarnessRevision,
        plans: &'a [gate4agent_harness_api::HarnessOrdinaryLaunchPlanOptionV1],
        managed_worktree_profiles:
            Vec<gate4agent_harness_api::HarnessManagedWorktreeProfileOptionV1>,
        context_sources: Vec<HarnessContextSourceSelectionV1>,
        delivery_bundles:
            &'a [gate4agent_harness_protocol::HarnessDeliveryBundleSelectionV1],
        truncated: bool,
    }

    let managed_worktree_profiles = options.managed_worktree_profiles.iter()
        .cloned()
        .map(|mut profile| {
            profile.observed_at_unix_ms = 0;
            profile
        })
        .collect();
    let context_sources = options.context_sources.iter().cloned().map(|mut source| {
        source.observed_at_unix_ms = 0;
        source
    }).collect();
    let material = CanonicalLaunchPolicy {
        task_id: &options.task_id,
        task_revision: options.task_revision,
        plans: &options.plans,
        managed_worktree_profiles,
        context_sources,
        delivery_bundles: &options.delivery_bundles,
        truncated: options.truncated,
    };
    hmac_request_digest(HARNESS_LAUNCH_POLICY_DIGEST_DOMAIN, &material)
}

pub(crate) fn context_source_metadata_digest(
    source: &HarnessContextSourceSelectionV1,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    let mut canonical = source.clone();
    canonical.metadata_digest = HarnessRequestDigest::new("0".repeat(64))?;
    canonical.observed_at_unix_ms = 0;
    hmac_request_digest(HARNESS_CONTEXT_SOURCE_DIGEST_DOMAIN, &canonical)
}

pub(crate) fn task_launch_issuance_digest(
    issuance: &HarnessTaskLaunchIssuanceV1,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    let mut canonical = issuance.clone();
    canonical.digest = HarnessRequestDigest::new("0".repeat(64))?;
    canonical.validate()?;
    hmac_request_digest(HARNESS_LAUNCH_ISSUANCE_DIGEST_DOMAIN, &canonical)
}

fn hmac_request_digest<T: Serialize>(
    domain: &[u8],
    material: &T,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    let encoded = serde_json::to_vec(material)?;
    let digest = gate4agent_node_wire::local_hmac_sha256(domain, &encoded)
        .map_err(HarnessServiceError::MutationDigest)?;
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(HarnessRequestDigest::new(hex)?)
}

/// The full, unpaged derivation of a task's launch options: every currently
/// valid plan / managed-worktree profile / context source / delivery bundle,
/// with no `HARNESS_TASK_LAUNCH_OPTIONS_MAX` cap. `validate_current_launch_
/// options`/`validate_current_issued_launch_options` check a selection
/// against THIS, never against the operator wire's own
/// `HarnessTaskLaunchOptionsV1` -- that type is one bounded PAGE of the same
/// catalogue for display (`runtime.rs`'s `task_launch_options`), and a plan
/// sitting past that page boundary must still be an acceptable selection. A
/// stack whose derived catalogue exceeds the page size used to have every
/// `spec save` refused outright, because the page's own `truncated: true`
/// was (wrongly) read as "this selection cannot be validated" -- see
/// `runtime.rs`'s `full_task_launch_catalogue`, the untruncated builder this
/// type is constructed from.
pub(crate) struct HarnessFullTaskLaunchCatalogueV1 {
    pub task_id: HarnessTaskId,
    pub task_revision: HarnessRevision,
    pub policy_digest: HarnessRequestDigest,
    pub plans: Vec<gate4agent_harness_api::HarnessOrdinaryLaunchPlanOptionV1>,
    pub managed_worktree_profiles:
        Vec<gate4agent_harness_api::HarnessManagedWorktreeProfileOptionV1>,
    pub context_sources: Vec<HarnessContextSourceSelectionV1>,
    pub delivery_bundles: Vec<gate4agent_harness_protocol::HarnessDeliveryBundleSelectionV1>,
}

impl HarnessFullTaskLaunchCatalogueV1 {
    /// Defensive self-check on the catalogue's own shape: every item is
    /// individually well-formed, each list is sorted and duplicate-free (the
    /// same invariant `HarnessTaskLaunchOptionsV1::validate()` enforces on
    /// its bounded page), and this catalogue actually belongs to `task_id`.
    /// Never returns a reason -- a failure here means the catalogue was
    /// built wrong, not that the caller's selection was wrong.
    fn shape_is_valid_for(&self, task_id: &HarnessTaskId) -> bool {
        &self.task_id == task_id
            && self.task_id.validate().is_ok()
            && self.task_revision.validate().is_ok()
            && self.policy_digest.validate().is_ok()
            && self.plans.iter().all(|plan| plan.validate().is_ok())
            && self.managed_worktree_profiles.iter().all(|profile| profile.validate().is_ok())
            && self.context_sources.iter().all(|source| source.validate().is_ok())
            && self.delivery_bundles.iter().all(|delivery| delivery.validate().is_ok())
            && !self.plans.windows(2).any(|items| {
                (&items[0].plan.plan_id, items[0].plan.revision)
                    >= (&items[1].plan.plan_id, items[1].plan.revision)
            })
            && !self.managed_worktree_profiles.windows(2).any(|items| {
                (
                    &items[0].node_id,
                    &items[0].source_workspace_id,
                    &items[0].profile_id,
                    &items[0].profile_revision,
                ) >= (
                    &items[1].node_id,
                    &items[1].source_workspace_id,
                    &items[1].profile_id,
                    &items[1].profile_revision,
                )
            })
            && !self.context_sources.windows(2).any(|items| {
                (&items[0].source_run_id, items[0].source_run_revision)
                    >= (&items[1].source_run_id, items[1].source_run_revision)
            })
            && !self.delivery_bundles.windows(2).any(|items| {
                (&items[0].bundle.bundle_id, &items[0].bundle.revision)
                    >= (&items[1].bundle.bundle_id, &items[1].bundle.revision)
            })
    }
}

/// The full-catalogue twin of `task_launch_policy_digest`: same canonical
/// hash domain, minus the `truncated` field that function's material used to
/// carry -- a PAGE's truncation is a presentation artifact of the operator
/// read wire, not part of what a launch selection is validated against, so
/// it never belonged in a digest two DIFFERENT catalogue derivations (spec-
/// save time and start time) are compared against each other by.
pub(crate) fn task_launch_policy_digest_full(
    catalogue: &HarnessFullTaskLaunchCatalogueV1,
) -> Result<HarnessRequestDigest, HarnessServiceError> {
    #[derive(Serialize)]
    struct CanonicalFullLaunchPolicy<'a> {
        task_id: &'a HarnessTaskId,
        task_revision: HarnessRevision,
        plans: &'a [gate4agent_harness_api::HarnessOrdinaryLaunchPlanOptionV1],
        managed_worktree_profiles:
            Vec<gate4agent_harness_api::HarnessManagedWorktreeProfileOptionV1>,
        context_sources: Vec<HarnessContextSourceSelectionV1>,
        delivery_bundles:
            &'a [gate4agent_harness_protocol::HarnessDeliveryBundleSelectionV1],
    }

    let managed_worktree_profiles = catalogue.managed_worktree_profiles.iter()
        .cloned()
        .map(|mut profile| {
            profile.observed_at_unix_ms = 0;
            profile
        })
        .collect();
    let context_sources = catalogue.context_sources.iter().cloned().map(|mut source| {
        source.observed_at_unix_ms = 0;
        source
    }).collect();
    let material = CanonicalFullLaunchPolicy {
        task_id: &catalogue.task_id,
        task_revision: catalogue.task_revision,
        plans: &catalogue.plans,
        managed_worktree_profiles,
        context_sources,
        delivery_bundles: &catalogue.delivery_bundles,
    };
    hmac_request_digest(HARNESS_LAUNCH_POLICY_DIGEST_DOMAIN, &material)
}

fn validate_current_launch_options(
    catalogue: &HarnessFullTaskLaunchCatalogueV1,
    request: &HarnessReplaceTaskExecutionSpecRequestV2,
) -> Result<(), HarnessServiceError> {
    let invalid = |why: &'static str| HarnessServiceError::InvalidTaskLaunchSelection {
        task_id: request.task_id.clone(),
        plan_id: Some(request.selection.plan.plan.plan_id.clone()),
        why,
    };
    if !catalogue.shape_is_valid_for(&request.task_id) {
        return Err(invalid("full launch catalogue failed its own shape self-check"));
    }
    if catalogue.task_revision != request.expected_task_revision {
        return Err(invalid("task revision no longer matches the full launch catalogue"));
    }
    if catalogue.policy_digest != task_launch_policy_digest_full(catalogue)? {
        return Err(invalid("policy digest does not match the full launch catalogue's content"));
    }
    if !catalogue.plans.contains(&request.selection.plan) {
        return Err(invalid("selected plan is not a current option in the full catalogue"));
    }
    if request.selection.context_source.as_ref()
        .is_some_and(|source| !catalogue.context_sources.iter()
            .any(|current| context_source_semantically_matches(current, source)))
    {
        return Err(invalid("selected context source is not a current option"));
    }
    if request.selection.delivery.as_ref()
        .is_some_and(|delivery| !catalogue.delivery_bundles.contains(delivery))
    {
        return Err(invalid("selected delivery bundle is not a current option"));
    }
    match &request.selection.worktree {
        HarnessReviewedWorktreeSelectionV1::Existing => {}
        HarnessReviewedWorktreeSelectionV1::Managed { profile } => {
            if !catalogue.managed_worktree_profiles.iter()
                .any(|current| managed_profile_semantically_matches(current, profile))
                || profile.node_id != request.selection.plan.node_id
                || profile.source_workspace_id != request.selection.plan.source_workspace_id
            {
                return Err(invalid("selected managed worktree profile is not a current option"));
            }
        }
    }
    if let Some(source) = &request.selection.context_source {
        if source.metadata_digest != context_source_metadata_digest(source)? {
            return Err(invalid("selected context source metadata digest does not match"));
        }
    }
    Ok(())
}

fn validate_current_issued_launch_options(
    catalogue: &HarnessFullTaskLaunchCatalogueV1,
    issuance: &HarnessTaskLaunchIssuanceV1,
) -> Result<(), HarnessServiceError> {
    let invalid = |why: &'static str| HarnessServiceError::InvalidTaskLaunchSelection {
        task_id: issuance.task_id.clone(),
        plan_id: Some(issuance.plan.plan_id.clone()),
        why,
    };
    if !catalogue.shape_is_valid_for(&issuance.task_id) {
        return Err(invalid("full launch catalogue failed its own shape self-check"));
    }
    if catalogue.task_revision != issuance.task_revision {
        return Err(invalid("task revision no longer matches the full launch catalogue"));
    }
    if catalogue.policy_digest != task_launch_policy_digest_full(catalogue)? {
        return Err(invalid("policy digest does not match the full launch catalogue's content"));
    }
    if catalogue.policy_digest != issuance.policy_digest {
        return Err(invalid("issued launch's policy digest no longer matches the current catalogue"));
    }
    if !catalogue.plans.iter().any(|plan| {
        plan.plan == issuance.plan
            && plan.node_id == issuance.target.node_id
            && plan.source_workspace_id == issuance.target.source_workspace_id
            && plan.provider_profile == issuance.target.provider_profile
            && plan.mode == issuance.target.mode
    }) {
        return Err(invalid("issued plan is not a current option in the full catalogue"));
    }
    if issuance.context_source.as_ref()
        .is_some_and(|source| !catalogue.context_sources.iter()
            .any(|current| context_source_semantically_matches(current, source)))
    {
        return Err(invalid("issued context source is not a current option"));
    }
    if issuance.delivery.as_ref()
        .is_some_and(|delivery| !catalogue.delivery_bundles.contains(delivery))
    {
        return Err(invalid("issued delivery bundle is not a current option"));
    }
    if let HarnessLaunchWorktreeSelectionV1::Managed {
        profile_id,
        expected_profile_revision,
    } = &issuance.target.worktree {
        if !catalogue.managed_worktree_profiles.iter().any(|profile| {
            profile.node_id == issuance.target.node_id
                && profile.source_workspace_id == issuance.target.source_workspace_id
                && profile.profile_id == *profile_id
                && profile.profile_revision == *expected_profile_revision
        }) {
            return Err(invalid("issued managed worktree profile is not a current option"));
        }
    }
    if let Some(source) = &issuance.context_source {
        if source.metadata_digest != context_source_metadata_digest(source)? {
            return Err(invalid("issued context source metadata digest does not match"));
        }
    }
    Ok(())
}

fn managed_profile_semantically_matches(
    left: &gate4agent_harness_api::HarnessManagedWorktreeProfileOptionV1,
    right: &gate4agent_harness_api::HarnessManagedWorktreeProfileOptionV1,
) -> bool {
    left.node_id == right.node_id
        && left.node_incarnation == right.node_incarnation
        && left.source_workspace_id == right.source_workspace_id
        && left.profile_id == right.profile_id
        && left.profile_revision == right.profile_revision
        && left.retention == right.retention
}

fn context_source_semantically_matches(
    left: &HarnessContextSourceSelectionV1,
    right: &HarnessContextSourceSelectionV1,
) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.observed_at_unix_ms = 0;
    right.observed_at_unix_ms = 0;
    left == right
}

/// A `Durable` source (`context_source_option`'s durable branch,
/// `runtime.rs`) carries the Node's CURRENT incarnation and
/// `active_session: None` — the receipt, not a live session, is
/// authoritative, and `HarnessRunV1.binding.session`'s own
/// `node_incarnation`/`active_session` are set once at spawn-accept and
/// never update back in production (empirically confirmed by the A2
/// durable-context-pack E2E: a run that finished cleanly, and whose Node
/// later restarted, still carries its ORIGINAL incarnation and
/// `active_session` in its durable binding). Requiring exact equality on
/// either field for a durable source would therefore reject every durable
/// source once the Node has actually restarted — neither is a signal the
/// durable path can or needs to check; `record_id` (plus node_id and
/// workspace) is what identifies the source. The `Live` path is unchanged:
/// it still requires exact equality on both, since there the live session
/// genuinely is what was observed, on the Node incarnation it is still
/// running under.
fn source_binding_matches_context_selection(
    binding: &gate4agent_harness_protocol::HarnessSessionBindingV1,
    source: &HarnessContextSourceSelectionV1,
) -> bool {
    let is_durable = source.availability
        == gate4agent_harness_protocol::HarnessContextSourceAvailabilityV1::Durable;
    binding.node_id == source.node_id
        && (is_durable || binding.node_incarnation == source.node_incarnation)
        && binding.workspace_id == source.workspace_id
        && matches!(
            &binding.session,
            HarnessSessionIdentityV1::Managed {
                record_id,
                active_session,
            } if record_id == &source.session_record_id
                && (is_durable || active_session.as_ref() == source.active_session.as_ref())
        )
}

fn issued_run_intent_matches(
    issuance: &HarnessTaskLaunchIssuanceV1,
    run: &HarnessRunV1,
) -> bool {
    let worktree_matches = match (&issuance.target.worktree, &run.intent.worktree) {
        (HarnessLaunchWorktreeSelectionV1::Existing, HarnessWorktreeIntentV1::Existing) => true,
        (
            HarnessLaunchWorktreeSelectionV1::Managed {
                profile_id,
                expected_profile_revision,
            },
            HarnessWorktreeIntentV1::ManagedProfile {
                profile_id: actual_profile,
                expected_profile_revision: actual_revision,
            },
        ) => profile_id == actual_profile && expected_profile_revision == actual_revision,
        _ => false,
    };
    issuance.task_id == run.task_id
        && issuance.target.node_id == run.intent.node_id
        && issuance.target.source_workspace_id == run.intent.workspace_id
        && issuance.target.provider_profile == run.intent.provider_profile
        && issuance.target.mode == run.intent.mode
        && worktree_matches
        && issuance.delivery.as_ref().map(|selection| &selection.bundle.selector)
            == run.intent.delivery_bundle.as_ref()
        && issuance.context_source.as_ref().map(|source| source.source_run_id.as_str())
            == run.intent.continuation.as_ref().map(|selector| selector.as_str())
}

fn validate_issued_launch_snapshot(
    issuance: &HarnessTaskLaunchIssuanceV1,
    run: &HarnessRunV1,
) -> Result<(), HarnessServiceError> {
    issuance.validate()?;
    if issuance.issuance_id != deterministic_launch_issuance_id(&issuance.task_id)?
        || issuance.digest != task_launch_issuance_digest(issuance)?
        || !issued_run_intent_matches(issuance, run)
    {
        return Err(HarnessServiceError::ExecutionSpecLaunchMismatch);
    }
    Ok(())
}

fn validate_service_issued_execution_spec(
    issuance: &HarnessTaskLaunchIssuanceV1,
    spec: &HarnessTaskExecutionSpecV2,
) -> Result<(), HarnessServiceError> {
    issuance.validate()?;
    spec.validate()?;
    if issuance.issuance_id != deterministic_launch_issuance_id(&issuance.task_id)?
        || issuance.digest != task_launch_issuance_digest(issuance)?
        || spec.execution_spec_id != deterministic_execution_spec_id(&spec.task_id)?
        || issuance.task_id != spec.task_id
        || issuance.revision != spec.revision
        || issuance.reference() != spec.launch_issuance
        || issuance.created_at_unix_ms != spec.created_at_unix_ms
        || issuance.updated_at_unix_ms != spec.updated_at_unix_ms
    {
        return Err(HarnessServiceError::ExecutionSpecLaunchMismatch);
    }
    Ok(())
}

fn validate_service_execution_spec(
    spec: &HarnessTaskExecutionSpecV1,
) -> Result<(), HarnessServiceError> {
    spec.validate()?;
    if spec.execution_spec_id != deterministic_execution_spec_id(&spec.task_id)?
        || spec.scheduled_launch_digest != scheduled_launch_digest(&spec.scheduled_launch)?
    {
        return Err(HarnessServiceError::ExecutionSpecLaunchMismatch);
    }
    Ok(())
}

fn operator_actor(authority: &HarnessOperatorAuthorityV1) -> HarnessActorV1 {
    HarnessActorV1::User {
        actor_id: authority.actor_id.clone(),
    }
}

/// The mailbox arc's own operation builder for `SendMail`/`AckMail`: unlike
/// `operator_task_operation`, these two kinds carry `grant_id: Some(_)` and
/// no `task_id` (`HarnessOperationKindV1::validate_kind_targets`), so they do
/// not fit that helper's shape. `request_digest` is the same "0".repeat(64)
/// placeholder every other production call site uses before the real digest
/// is computed over the finished mutation (`mutation_request_digest`) --
/// see `send_mail`/`ack_mail`/`operator_ack_mail`'s own call sites.
fn mail_operation(
    grant_id: &SessionGrantId,
    actor: HarnessActorV1,
    kind: HarnessOperationKindV1,
    now_unix_ms: u64,
) -> Result<HarnessOperationV1, HarnessServiceError> {
    let material = next_mail_nonce_material(grant_id, now_unix_ms);
    Ok(HarnessOperationV1 {
        operation_id: derived_mail_id(
            "hop_",
            b"gate4agent-harness-mail-operation-id-v1",
            &material,
            HarnessOperationId::new,
        )?,
        revision: HarnessRevision::new(1)?,
        actor,
        kind,
        state: HarnessOperationStateV1::Succeeded,
        task_id: None,
        run_id: None,
        grant_id: Some(grant_id.clone()),
        reconciles_operation_id: None,
        expected_revision: None,
        request_digest: HarnessRequestDigest::new("0".repeat(64))?,
        idempotency_ref: derived_mail_id(
            "hidem_",
            b"gate4agent-harness-mail-idempotency-ref-v1",
            &material,
            HarnessIdempotencyRef::new,
        )?,
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: Some(now_unix_ms),
    })
}

/// D5, Slice D's own operation builder for `agent_create_task`/
/// `agent_move_task`: unlike `mail_operation`, `CreateTask`/`MutateTask`
/// carry `task_id: Some(_)` and `grant_id: None`
/// (`HarnessOperationKindV1::validate_kind_targets` -- the same rule
/// `operator_task_operation` already satisfies for the operator's own task
/// writes), so this reuses `operator_task_operation`'s field shape but mints
/// its own ids off the calling grant's nonce material the way `mail_operation`
/// does, since the agent path has no `HarnessOperatorAuthorityV1` to draw
/// `operation_id`/`idempotency_ref` from.
fn agent_task_operation(
    grant_id: &SessionGrantId,
    actor: HarnessActorV1,
    kind: HarnessOperationKindV1,
    task_id: HarnessTaskId,
    expected_revision: Option<HarnessRevision>,
    now_unix_ms: u64,
) -> Result<HarnessOperationV1, HarnessServiceError> {
    let material = next_mail_nonce_material(grant_id, now_unix_ms);
    Ok(HarnessOperationV1 {
        operation_id: derived_mail_id(
            "hop_",
            b"gate4agent-harness-agent-task-operation-id-v1",
            &material,
            HarnessOperationId::new,
        )?,
        revision: HarnessRevision::new(1)?,
        actor,
        kind,
        state: HarnessOperationStateV1::Succeeded,
        task_id: Some(task_id),
        run_id: None,
        grant_id: None,
        reconciles_operation_id: None,
        expected_revision,
        request_digest: HarnessRequestDigest::new("0".repeat(64))?,
        idempotency_ref: derived_mail_id(
            "hidem_",
            b"gate4agent-harness-agent-task-idempotency-ref-v1",
            &material,
            HarnessIdempotencyRef::new,
        )?,
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: now_unix_ms,
        updated_at_unix_ms: now_unix_ms,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: Some(now_unix_ms),
    })
}

/// One-shot, process-local nonce material for minting a mail message,
/// operation, or idempotency id: `harnessctl`'s own `random_hex24` doc
/// comment states the bar this meets exactly -- "uniqueness (not
/// unpredictability) is all that is required here." Domain-separated HMAC
/// (`derived_mail_id`'s own `domain` argument) turns this one nonce into as
/// many independent ids as a caller needs from it.
fn next_mail_nonce_material(grant_id: &SessionGrantId, now_unix_ms: u64) -> Vec<u8> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{grant_id}|{now_unix_ms}|{sequence}|{}", std::process::id()).into_bytes()
}

fn derived_mail_id<T>(
    prefix: &str,
    domain: &[u8],
    material: &[u8],
    constructor: impl FnOnce(String) -> Result<T, HarnessValidationError>,
) -> Result<T, HarnessServiceError> {
    let digest = gate4agent_node_wire::local_hmac_sha256(domain, material)
        .map_err(HarnessServiceError::MutationDigest)?;
    let mut nonce = String::with_capacity(24);
    for byte in &digest[..12] {
        use std::fmt::Write as _;
        write!(&mut nonce, "{byte:02x}").expect("writing a hex byte to a String cannot fail");
    }
    Ok(constructor(format!("{prefix}{nonce}"))?)
}

/// Resolves the grant `operator_ack_mail`/the mailbox-inventory fill
/// (`gate4agent-harness-service::runtime::fill_managed_session_blocked_stats`)
/// use to read or ack mail on behalf of a bare `record_id`: the most
/// recently created active, mail-permitted grant whose `actor_run_id` is
/// bound to a run currently claiming that record (the same `Managed`
/// binding match `HarnessEngine::validate_mail_recipient` uses internally,
/// reproduced here from the engine's public `runs()`/`grants()` iterators
/// since this lookup belongs to the operator-wire surface, not the engine
/// itself). `None` when no such grant exists -- callers decide what that
/// means (a `NotFound` refusal for an ack, an empty fallback scan for a
/// count).
pub(crate) fn mail_grant_for_record(
    engine: &HarnessEngine,
    record_id: &HarnessSelectorV1,
) -> Option<SessionGrantId> {
    let bound_runs: BTreeSet<HarnessRunId> = engine.runs()
        .filter(|run| {
            matches!(
                run.binding.as_ref().map(|binding| &binding.session),
                Some(HarnessSessionIdentityV1::Managed { record_id: bound, .. })
                    if bound == record_id
            )
        })
        .map(|run| run.run_id.clone())
        .collect();
    engine.grants()
        .filter(|grant| bound_runs.contains(&grant.actor_run_id))
        .filter(|grant| {
            grant.state == gate4agent_harness_protocol::SessionGrantStateV1::Active && grant.mail
        })
        .max_by_key(|grant| grant.created_at_unix_ms)
        .map(|grant| grant.grant_id.clone())
}

fn operator_task_operation(
    authority: &HarnessOperatorAuthorityV1,
    actor: HarnessActorV1,
    kind: HarnessOperationKindV1,
    task_id: HarnessTaskId,
    expected_revision: Option<HarnessRevision>,
) -> Result<HarnessOperationV1, HarnessServiceError> {
    Ok(HarnessOperationV1 {
        operation_id: authority.operation_id.clone(),
        revision: HarnessRevision::new(1)?,
        actor,
        kind,
        state: HarnessOperationStateV1::Succeeded,
        task_id: Some(task_id),
        run_id: None,
        grant_id: None,
        reconciles_operation_id: None,
        expected_revision,
        request_digest: HarnessRequestDigest::new("0".repeat(64))?,
        idempotency_ref: authority.idempotency_ref.clone(),
        failure: None,
        outcome_unknown_reason: None,
        reconciliation_outcome: None,
        created_at_unix_ms: authority.now_unix_ms,
        updated_at_unix_ms: authority.now_unix_ms,
        dispatched_at_unix_ms: None,
        finished_at_unix_ms: Some(authority.now_unix_ms),
    })
}

fn next_harness_revision(
    revision: HarnessRevision,
    entity: &'static str,
) -> Result<HarnessRevision, HarnessServiceError> {
    let value = revision.get().checked_add(1)
        .ok_or(HarnessEngineError::InvalidNextRevision { entity })?;
    Ok(HarnessRevision::new(value)?)
}

fn validate_operator_move(
    from: HarnessTaskStateV1,
    to: HarnessTaskStateV1,
) -> Result<(), HarnessServiceError> {
    let valid = from != to
        && !matches!(from, HarnessTaskStateV1::Done | HarnessTaskStateV1::Failed | HarnessTaskStateV1::Cancelled)
        && !matches!(
            to,
            HarnessTaskStateV1::Running
                | HarnessTaskStateV1::Failed
                | HarnessTaskStateV1::Cancelled
        )
        && (to != HarnessTaskStateV1::Done || from == HarnessTaskStateV1::Review);
    if valid {
        Ok(())
    } else {
        Err(HarnessServiceError::InvalidOperatorTaskTransition { from, to })
    }
}

fn map_scheduler_error(error: HarnessEngineError) -> HarnessServiceError {
    match error {
        HarnessEngineError::SchedulerResourceExhausted => {
            HarnessServiceError::SchedulerResourceExhausted
        }
        HarnessEngineError::SchedulerInvalidGraph => {
            HarnessServiceError::SchedulerInvalidGraph("durable scheduler graph is incoherent")
        }
        other => HarnessServiceError::Engine(other),
    }
}

fn dispatch_intent(
    task: &HarnessTaskV1,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
) -> Result<HarnessDispatchIntentV1, HarnessServiceError> {
    if task.state != HarnessTaskStateV1::Running
        || run.lifecycle != HarnessRunLifecycleV1::Requested
        || operation.kind != HarnessOperationKindV1::CreateRun
        || operation.state != HarnessOperationStateV1::Prepared
        || operation.task_id.as_ref() != Some(&task.task_id)
        || operation.run_id.as_ref() != Some(&run.run_id)
        || run.task_id != task.task_id
        || run.operation_id != operation.operation_id
        || task.run_ids.binary_search(&run.run_id).is_err()
    {
        return Err(HarnessServiceError::SchedulerInvalidGraph(
            "pending dispatch task/run/operation is incoherent",
        ));
    }
    let intent = HarnessDispatchIntentV1 {
        task_id: task.task_id.clone(),
        task_revision: task.revision,
        run_id: run.run_id.clone(),
        run_revision: run.revision,
        operation_id: operation.operation_id.clone(),
        operation_revision: operation.revision,
        idempotency_ref: operation.idempotency_ref.clone(),
        parent_run_id: run.parent_run_id.clone(),
        intent: run.intent.clone(),
    };
    intent.validate()?;
    Ok(intent)
}

fn replayed_dispatch_intent(
    task: &HarnessTaskV1,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
) -> Result<HarnessDispatchIntentV1, HarnessServiceError> {
    let expected_task_revision = operation.expected_revision
        .ok_or(HarnessServiceError::SchedulerInvalidGraph(
            "scheduler replay operation has no expected task revision",
        ))?;
    let task_revision = next_harness_revision(expected_task_revision, "task")?;
    if operation.kind != HarnessOperationKindV1::CreateRun
        || operation.task_id.as_ref() != Some(&task.task_id)
        || operation.run_id.as_ref() != Some(&run.run_id)
        || run.task_id != task.task_id
        || run.operation_id != operation.operation_id
        || task.run_ids.binary_search(&run.run_id).is_err()
        || task.revision < task_revision
    {
        return Err(HarnessServiceError::SchedulerInvalidGraph(
            "replayed dispatch task/run/operation is incoherent",
        ));
    }
    let intent = HarnessDispatchIntentV1 {
        task_id: task.task_id.clone(),
        task_revision,
        run_id: run.run_id.clone(),
        run_revision: HarnessRevision::new(1)?,
        operation_id: operation.operation_id.clone(),
        operation_revision: HarnessRevision::new(1)?,
        idempotency_ref: operation.idempotency_ref.clone(),
        parent_run_id: run.parent_run_id.clone(),
        intent: run.intent.clone(),
    };
    intent.validate()?;
    Ok(intent)
}

fn dispatch_intent_from_prepared(
    prepared: &PreparedHarnessMutation,
) -> Result<HarnessDispatchIntentV1, HarnessServiceError> {
    let checkpoint = prepared.checkpoint();
    let operation = checkpoint.operations.iter()
        .find(|operation| operation.operation_id == prepared.operation().operation_id)
        .ok_or(HarnessServiceError::SchedulerInvalidGraph(
            "prepared scheduler operation is missing",
        ))?;
    let run = operation.run_id.as_ref()
        .and_then(|run_id| checkpoint.runs.iter().find(|run| &run.run_id == run_id))
        .ok_or(HarnessServiceError::SchedulerInvalidGraph(
            "prepared scheduler run is missing",
        ))?;
    let task = checkpoint.tasks.iter().find(|task| task.task_id == run.task_id)
        .ok_or(HarnessServiceError::SchedulerInvalidGraph(
            "prepared scheduler task is missing",
        ))?;
    dispatch_intent(task, run, operation)
}

fn operator_request_records(
    requests: &BTreeMap<HarnessOperationId, HarnessRequestDigest>,
) -> Vec<HarnessOperatorRequestV1> {
    requests.iter().map(|(operation_id, request_digest)| HarnessOperatorRequestV1 {
        operation_id: operation_id.clone(),
        request_digest: request_digest.clone(),
    }).collect()
}

pub fn harness_mcp_activation_digest(
    reservation_id: &HarnessMcpReservationId,
    grant_id: &SessionGrantId,
    grant_revision: HarnessRevision,
    actor_run_id: &HarnessRunId,
    operation_id: &HarnessOperationId,
    context: &HarnessDispatchContextV1,
    expires_at_unix_ms: u64,
) -> Result<HarnessMcpActivationDigest, HarnessServiceError> {
    const DOMAIN: &[u8] = b"gate4agent-harness-mcp-activation-v1";
    #[derive(Serialize)]
    struct Canonical<'a> {
        reservation_id: &'a HarnessMcpReservationId,
        grant_id: &'a SessionGrantId,
        grant_revision: u64,
        actor_run_id: &'a HarnessRunId,
        operation_id: &'a HarnessOperationId,
        node_id: &'a HarnessSelectorV1,
        node_incarnation_id: &'a HarnessSelectorV1,
        workspace_id: &'a HarnessSelectorV1,
        provider_profile: &'a HarnessSelectorV1,
        expected_provider: &'a HarnessSelectorV1,
        mode: HarnessExecutionModeV1,
        spawn_spec_fingerprint: &'a HarnessRequestDigest,
        idempotency_ref: &'a HarnessIdempotencyRef,
        expires_at_unix_ms: u64,
    }
    let encoded = serde_json::to_vec(&Canonical {
        reservation_id,
        grant_id,
        grant_revision: grant_revision.get(),
        actor_run_id,
        operation_id,
        node_id: &context.node_id,
        node_incarnation_id: &context.node_incarnation_id,
        workspace_id: &context.workspace_id,
        provider_profile: &context.provider_profile,
        expected_provider: &context.expected_provider,
        mode: context.mode,
        spawn_spec_fingerprint: &context.spawn_spec_fingerprint,
        idempotency_ref: &context.idempotency_ref,
        expires_at_unix_ms,
    })?;
    let digest = gate4agent_node_wire::local_hmac_sha256(DOMAIN, &encoded)
        .map_err(HarnessServiceError::MutationDigest)?;
    let mut value = String::from("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    HarnessMcpActivationDigest::new(value)
        .map_err(|_| HarnessServiceError::InvalidHarnessMcpReservation(
            "activation digest is invalid",
        ))
}

fn next_revision(revision: HarnessRevision) -> Result<HarnessRevision, HarnessServiceError> {
    HarnessRevision::new(revision.get().checked_add(1).ok_or(
        HarnessServiceError::InvalidHarnessMcpReservation("reservation revision overflow"),
    )?).map_err(HarnessServiceError::from)
}

/// H3B recognizes two dispatch shapes for the run a harness-MCP grant
/// authorizes: (a) a child/continuation run dispatched under its parent,
/// whose operation actor names that parent (`HarnessActorV1::ParentRun
/// { run_id }` with `run.parent_run_id == Some(run_id)` -- the original
/// H3B slice this path was built for); and (b) an operator-started
/// top-level run dispatching with its own harness-issued default grant
/// (Slice A(i)'s `resolve_harness_mcp_grant` /
/// `SessionGrantV1::default_for_run`, which mints the grant with
/// `actor_run_id == run.run_id` for a run that has no parent). Any actor,
/// parent, or grant combination outside those two shapes is refused by
/// name (`HarnessMcpGrantActorRefused`, naming the actor kind, the run's
/// parent, and the grant's own actor run).
///
/// Once a shape is recognized, the remaining authority proof used to be one
/// bare `InvalidHarnessMcpReservation("H3B grant revision, actor, or
/// target is not authoritative")` folding four independent conditions --
/// so a refusal never said which of them actually fired, nor the two
/// values that disagreed. Each is now its own named, `#[from]`-free
/// `HarnessServiceError` variant carrying both sides of the mismatch:
/// `HarnessMcpGrantOperationLinkRefused` (the dispatch operation already
/// names a different grant), `HarnessMcpGrantRevisionRefused` (the
/// presented revision does not match the grant's durable one),
/// `HarnessMcpGrantLinkRefused` (the grant's own `actor_run_id` does not
/// match the run this dispatch resolved for that shape -- for shape (a)
/// this is the parent run named by the operation actor; for shape (b) the
/// match guard above already forces it to hold, so it never fires there),
/// and `HarnessMcpGrantTargetRefused` (the dispatch's node/workspace/
/// provider-profile/mode is not among the grant's `allowed_targets`).
fn validate_harness_mcp_grant(
    engine: &HarnessEngine,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
    context: &HarnessDispatchContextV1,
    grant_id: &SessionGrantId,
    grant_revision: HarnessRevision,
) -> Result<(), HarnessServiceError> {
    let grant = engine.grant(grant_id)
        .ok_or(HarnessServiceError::InvalidHarnessMcpReservation(
            "H3B grant is missing",
        ))?;
    let actor_run_id = match &operation.actor {
        HarnessActorV1::ParentRun { run_id }
            if run.parent_run_id.as_ref() == Some(run_id) =>
        {
            run_id
        }
        HarnessActorV1::User { .. }
            if run.parent_run_id.is_none() && grant.actor_run_id == run.run_id =>
        {
            &run.run_id
        }
        actor => {
            return Err(HarnessServiceError::HarnessMcpGrantActorRefused {
                actor_kind: match actor {
                    HarnessActorV1::ParentRun { .. } => "parent-run",
                    HarnessActorV1::User { .. } => "user",
                },
                parent_run_id: run.parent_run_id.clone(),
                grant_actor_run_id: grant.actor_run_id.clone(),
            });
        }
    };
    if let Some(existing_grant_id) = &operation.grant_id {
        return Err(HarnessServiceError::HarnessMcpGrantOperationLinkRefused {
            grant_id: grant_id.clone(),
            operation_id: operation.operation_id.clone(),
            existing_grant_id: existing_grant_id.clone(),
        });
    }
    if grant.revision != grant_revision {
        return Err(HarnessServiceError::HarnessMcpGrantRevisionRefused {
            grant_id: grant_id.clone(),
            durable_revision: grant.revision,
            presented_revision: grant_revision,
        });
    }
    if grant.actor_run_id != *actor_run_id {
        return Err(HarnessServiceError::HarnessMcpGrantLinkRefused {
            grant_id: grant_id.clone(),
            grant_actor_run_id: grant.actor_run_id.clone(),
            dispatch_actor_run_id: actor_run_id.clone(),
        });
    }
    if !grant.allows_target(
        &context.node_id,
        &context.workspace_id,
        &context.provider_profile,
        context.mode,
    ) {
        return Err(HarnessServiceError::HarnessMcpGrantTargetRefused {
            grant_id: grant_id.clone(),
            presented_target: HarnessGrantTargetV1 {
                node_id: context.node_id.clone(),
                workspace_id: context.workspace_id.clone(),
                provider_profile: context.provider_profile.clone(),
                mode: context.mode,
            },
            allowed_targets: grant.allowed_targets.clone(),
        });
    }
    Ok(())
}

fn validate_reservation_durable_context(
    engine: &HarnessEngine,
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    reservation: &HarnessMcpReservationV1,
    restoring: bool,
) -> Result<(), HarnessServiceError> {
    if restoring && reservation.state == HarnessMcpReservationStateV1::Revoked {
        return Ok(());
    }
    let operation = engine.operation(&reservation.operation_id).ok_or(
        HarnessServiceError::InvalidHarnessMcpReservation(
            "reservation operation is missing",
        ),
    )?;
    let run = operation.run_id.as_ref().and_then(|id| engine.run(id)).ok_or(
        HarnessServiceError::InvalidHarnessMcpReservation("reservation run is missing"),
    )?;
    let context = contexts.get(&reservation.operation_id).ok_or(
        HarnessServiceError::InvalidHarnessMcpReservation(
            "reservation dispatch context is missing",
        ),
    )?;
    let grant = engine.grant(&reservation.grant_id).ok_or(
        HarnessServiceError::InvalidHarnessMcpReservation("reservation grant is missing"),
    )?;
    let operation_state_valid = match reservation.state {
        HarnessMcpReservationStateV1::Prepared | HarnessMcpReservationStateV1::Armed => {
            operation.state == HarnessOperationStateV1::Dispatching
                && run.lifecycle
                    == gate4agent_harness_protocol::HarnessRunLifecycleV1::Dispatching
        }
        HarnessMcpReservationStateV1::Bound | HarnessMcpReservationStateV1::Active => {
            operation.state == HarnessOperationStateV1::Succeeded
                && matches!(
                    run.lifecycle,
                    gate4agent_harness_protocol::HarnessRunLifecycleV1::Running
                        | gate4agent_harness_protocol::HarnessRunLifecycleV1::Waiting
                )
        }
        HarnessMcpReservationStateV1::Revoked => true,
    };
    let run_binding_valid = match reservation.state {
        HarnessMcpReservationStateV1::Bound | HarnessMcpReservationStateV1::Active => {
            matches!(
                run.binding.as_ref(),
                Some(binding)
                    if binding.node_id == reservation.node_id
                        && binding.node_incarnation == reservation.node_incarnation_id
                        && binding.workspace_id == reservation.workspace_id
                        && matches!(
                            &binding.session,
                            gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed {
                                record_id,
                                active_session: Some(active),
                            } if Some(record_id) == reservation.record_id.as_ref()
                                && Some(active.instance_id) == reservation.instance_id
                                && Some(active.generation) == reservation.generation
                        )
            )
        }
        _ => true,
    };
    if !operation_state_valid
        || !run_binding_valid
        || operation.idempotency_ref != reservation.idempotency_ref
        || context.idempotency_ref != reservation.idempotency_ref
        || context.spawn_spec_fingerprint != reservation.spawn_spec_fingerprint
        || context.node_id != reservation.node_id
        || context.node_incarnation_id != reservation.node_incarnation_id
        || context.workspace_id != reservation.workspace_id
        || context.provider_profile != reservation.provider_profile
        || context.expected_provider != reservation.expected_provider
        || context.mode != reservation.mode
        || grant.revision != reservation.grant_revision
        || grant.actor_run_id != reservation.actor_run_id
        || grant.state != gate4agent_harness_protocol::SessionGrantStateV1::Active
    {
        return Err(HarnessServiceError::InvalidHarnessMcpReservation(
            "reservation does not match durable authority",
        ));
    }
    Ok(())
}

fn bind_harness_mcp_reservation(
    current: &HarnessMcpReservationV1,
    proof: &c2::AcceptedSpawnBindingProof,
    bound_at_unix_ms: u64,
) -> Result<HarnessMcpReservationV1, HarnessServiceError> {
    let proxy = proof.harness_mcp_proxy()
        .ok_or(HarnessServiceError::HarnessMcpProofMismatch)?;
    let (instance_id, generation) = proof.runtime_identity();
    if current.state == HarnessMcpReservationStateV1::Bound
        && current.record_id.as_ref().is_some_and(|record_id| {
            record_id.as_str() == proof.record_id().as_str()
        })
        && current.instance_id == Some(instance_id)
        && current.generation == Some(generation)
        && &current.reservation_id == &proxy.reservation_id
        && &current.activation_digest == &proxy.activation_digest
    {
        return Ok(current.clone());
    }
    if current.state != HarnessMcpReservationStateV1::Armed
        || &current.reservation_id != &proxy.reservation_id
        || &current.activation_digest != &proxy.activation_digest
        || proof.operation_id() != &current.operation_id
        || proof.spawn_spec_fingerprint() != &current.spawn_spec_fingerprint
        || proof.idempotency_ref() != &current.idempotency_ref
        || proof.node_id().as_str() != current.node_id.as_str()
        || proof.incarnation_id().to_string() != current.node_incarnation_id.as_str()
        || proof.workspace_id().as_str() != current.workspace_id.as_str()
        || proof.provider().as_str() != current.expected_provider.as_str()
        || !matches!(
            (proof.mode(), current.mode),
            (SessionMode::Pty, HarnessExecutionModeV1::Pty)
                | (SessionMode::Inline, HarnessExecutionModeV1::Inline)
                | (SessionMode::Acp, HarnessExecutionModeV1::Acp)
        )
        || bound_at_unix_ms < current.updated_at_unix_ms
        || bound_at_unix_ms >= current.expires_at_unix_ms
    {
        return Err(HarnessServiceError::HarnessMcpProofMismatch);
    }
    let mut next = current.clone();
    next.revision = next_revision(next.revision)?;
    next.state = HarnessMcpReservationStateV1::Bound;
    next.record_id = Some(HarnessSelectorV1::new(proof.record_id().as_str())?);
    next.instance_id = Some(instance_id);
    next.generation = Some(generation);
    next.updated_at_unix_ms = bound_at_unix_ms;
    next.validate()?;
    Ok(next)
}

fn reconcile_harness_mcp_reservations(
    prepared: &PreparedHarnessMutation,
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    mut reservations: BTreeMap<HarnessMcpReservationId, HarnessMcpReservationV1>,
) -> Result<BTreeMap<HarnessMcpReservationId, HarnessMcpReservationV1>, HarnessServiceError> {
    let engine = HarnessEngine::restore(prepared.checkpoint())?;
    for reservation in reservations.values_mut() {
        if reservation.state == HarnessMcpReservationStateV1::Revoked {
            continue;
        }
        if validate_reservation_durable_context(&engine, contexts, reservation, false).is_err() {
            reservation.revision = next_revision(reservation.revision)?;
            reservation.state = HarnessMcpReservationStateV1::Revoked;
            reservation.updated_at_unix_ms = engine.operation(&reservation.operation_id)
                .map_or(reservation.updated_at_unix_ms, |operation| {
                    operation.updated_at_unix_ms.max(reservation.updated_at_unix_ms)
                });
        }
    }
    Ok(reservations)
}

fn validate_context_operation(
    engine: &HarnessEngine,
    context: &HarnessDispatchContextV1,
) -> Result<(), HarnessServiceError> {
    let operation = engine.operation(&context.operation_id)
        .ok_or(HarnessServiceError::Corrupt("orphan dispatch context"))?;
    if operation.idempotency_ref != context.idempotency_ref
        || operation.dispatched_at_unix_ms != Some(context.dispatched_at_unix_ms)
        || !matches!(
            operation.state,
            HarnessOperationStateV1::Dispatching
                | HarnessOperationStateV1::OutcomeUnknown
                | HarnessOperationStateV1::Succeeded
                | HarnessOperationStateV1::Failed
                | HarnessOperationStateV1::Reconciled
        )
    {
        return Err(HarnessServiceError::Corrupt(
            "dispatch context does not match durable operation",
        ));
    }
    if let Some(run_id) = &operation.run_id {
        let run = engine.run(run_id)
            .ok_or(HarnessServiceError::Corrupt("dispatch context run is missing"))?;
        validate_run_dispatch_intent(run, context).map_err(|_| HarnessServiceError::Corrupt(
            "dispatch context does not match durable run intent",
        ))?;
    }
    Ok(())
}

fn validate_authoritative_dispatch_binding(
    engine: &HarnessEngine,
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
) -> Result<(), HarnessServiceError> {
    let current = engine.run(&run.run_id)
        .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
    if current != run {
        return Err(HarnessServiceError::InvalidDispatchContext(
            "bound run does not match durable engine state",
        ));
    }
    let context = contexts.get(&operation.operation_id).ok_or(
        HarnessServiceError::InvalidDispatchContext(
            "bound run has no durable dispatch context",
        ),
    )?;
    let binding = run.binding.as_ref().ok_or(
        HarnessServiceError::InvalidDispatchContext("bound run binding is missing"),
    )?;
    let managed_issued = validate_managed_issued_launch(issued_launches, run)?;
    let workspace_matches = if managed_issued {
        context.managed_worktree_binding.as_ref().is_some_and(|receipt| {
            receipt.source_workspace_id == context.workspace_id
                && receipt.allocated_workspace_id == binding.workspace_id
                && issued_launches.get(&run.operation_id).is_some_and(|issuance| {
                    receipt.launch_issuance == issuance.reference()
                })
        })
    } else {
        binding.workspace_id == context.workspace_id
    };
    if binding.node_id != context.node_id
        || binding.node_incarnation != context.node_incarnation_id
        || !workspace_matches
    {
        return Err(HarnessServiceError::InvalidDispatchContext(
            "bound run does not match durable dispatch target",
        ));
    }
    Ok(())
}

fn validate_accepted_spawn_proof(
    engine: &HarnessEngine,
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
    proof: &c2::AcceptedSpawnBindingProof,
) -> Result<(), HarnessServiceError> {
    use gate4agent_harness_protocol::{
        HarnessRunLifecycleV1, HarnessSessionIdentityV1,
    };

    let current = engine.run(&run.run_id)
        .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
    if current.lifecycle != HarnessRunLifecycleV1::Dispatching
        || run.lifecycle != HarnessRunLifecycleV1::Running
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "proof is only valid for Dispatching to Running",
        ));
    }
    let context = validate_accepted_spawn_request_identity(
        contexts,
        operation,
        proof,
    )?;
    let proof_mode = match proof.mode() {
        SessionMode::Pty => HarnessExecutionModeV1::Pty,
        SessionMode::Inline => HarnessExecutionModeV1::Inline,
        SessionMode::Acp => HarnessExecutionModeV1::Acp,
    };
    let managed_accepted = validate_managed_accepted_workspace_authority(
        issued_launches,
        run,
        context,
        proof,
    )?;
    let proof_source_matches = if managed_accepted {
        proof.managed_worktree().is_some_and(|managed| {
            managed.source_workspace_id().as_str() == context.workspace_id.as_str()
        })
    } else {
        proof.workspace_id().as_str() == context.workspace_id.as_str()
    };
    if proof.node_id().as_str() != context.node_id.as_str()
        || proof.incarnation_id().to_string() != context.node_incarnation_id.as_str()
        || !proof_source_matches
        || proof.provider().as_str() != context.expected_provider.as_str()
        || proof_mode != context.mode
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn does not match durable dispatch target",
        ));
    }
    let binding = run.binding.as_ref().ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof("accepted spawn binding is missing"),
    )?;
    let (instance_id, generation) = proof.runtime_identity();
    let session_matches = matches!(
        &binding.session,
        HarnessSessionIdentityV1::Managed {
            record_id,
            active_session: Some(active_session),
        } if record_id.as_str() == proof.record_id().as_str()
            && active_session.instance_id == instance_id
            && active_session.generation == generation
    );
    if binding.node_id.as_str() != proof.node_id().as_str()
        || binding.node_incarnation.as_str() != proof.incarnation_id().to_string()
        || binding.workspace_id.as_str() != proof.workspace_id().as_str()
        || !session_matches
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "run binding does not match accepted spawn",
        ));
    }
    Ok(())
}

fn validate_accepted_spawn_request_identity<'a>(
    contexts: &'a BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    operation: &HarnessOperationV1,
    proof: &c2::AcceptedSpawnBindingProof,
) -> Result<&'a HarnessDispatchContextV1, HarnessServiceError> {
    let context = contexts.get(&operation.operation_id).ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn has no durable dispatch context",
        ),
    )?;
    if proof.operation_id() != &operation.operation_id
        || proof.operation_id() != &context.operation_id
        || proof.spawn_spec_fingerprint() != &context.spawn_spec_fingerprint
        || proof.idempotency_ref() != &operation.idempotency_ref
        || proof.idempotency_ref() != &context.idempotency_ref
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn does not match durable operation request",
        ));
    }
    Ok(context)
}

fn validate_committed_spawn_replay(
    engine: &HarnessEngine,
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
    proof: &c2::AcceptedSpawnBindingProof,
) -> Result<(), HarnessServiceError> {
    let context = validate_accepted_spawn_request_identity(contexts, operation, proof)?;
    let managed = validate_managed_accepted_workspace_authority(
        issued_launches,
        run,
        context,
        proof,
    )?;
    let binding = run.binding.as_ref().ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof(
            "committed accepted spawn has no binding",
        ),
    )?;
    let (instance_id, generation) = proof.runtime_identity();
    let exact_session = matches!(
        &binding.session,
        HarnessSessionIdentityV1::Managed {
            record_id,
            active_session: Some(active),
        } if record_id.as_str() == proof.record_id().as_str()
            && active.instance_id == instance_id
            && active.generation == generation
    );
    let source_matches = if managed {
        context.managed_worktree_binding.as_ref().is_some_and(|receipt| {
            proof.managed_worktree().is_some_and(|managed| {
                receipt.lease_id == *managed.lease_id()
                    && receipt.allocated_workspace_id.as_str() == proof.workspace_id().as_str()
                    && receipt.source_workspace_id == context.workspace_id
            })
        })
    } else {
        context.managed_worktree_binding.is_none()
            && proof.workspace_id().as_str() == context.workspace_id.as_str()
    };
    if engine.run(&run.run_id) != Some(run)
        || run.lifecycle != HarnessRunLifecycleV1::Running
        || binding.node_id.as_str() != proof.node_id().as_str()
        || binding.node_incarnation.as_str() != proof.incarnation_id().to_string()
        || binding.workspace_id.as_str() != proof.workspace_id().as_str()
        || !source_matches
        || !exact_session
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "committed accepted spawn proof does not match durable binding",
        ));
    }
    Ok(())
}

fn validate_delivery_accepted_spawn_proof(
    engine: &HarnessEngine,
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    delivery: &HarnessDeliveryV1,
    run: &HarnessRunV1,
    proof: &c2::AcceptedSpawnBindingProof,
) -> Result<(), HarnessServiceError> {
    use gate4agent_harness_protocol::HarnessSessionIdentityV1;

    let operation = engine.operation(&delivery.operation_id)
        .ok_or_else(|| HarnessEngineError::NotFound(delivery.operation_id.to_string()))?;
    let context = validate_accepted_spawn_request_identity(contexts, operation, proof)?;
    let stage = delivery.stage_receipt.as_ref().ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof("delivery has no staged receipt"),
    )?;
    let binding = run.binding.as_ref().ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof("delivery run binding is missing"),
    )?;
    let proof_bundle = proof.bundle().ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn has no exact delivery bundle receipt",
        ),
    )?;
    let managed_accepted = validate_managed_accepted_workspace_authority(
        issued_launches,
        run,
        context,
        proof,
    )?;
    let proof_source_matches = if managed_accepted {
        proof.managed_worktree().is_some_and(|managed| {
            managed.source_workspace_id().as_str() == stage.workspace_id.as_str()
        })
    } else {
        proof.workspace_id().as_str() == stage.workspace_id.as_str()
    };
    let (instance_id, generation) = proof.runtime_identity();
    let session_matches = matches!(
        &binding.session,
        HarnessSessionIdentityV1::Managed {
            record_id,
            active_session: Some(active_session),
        } if record_id.as_str() == proof.record_id().as_str()
            && active_session.instance_id == instance_id
            && active_session.generation == generation
    );
    if proof.operation_id() != &delivery.operation_id
        || proof.node_id().as_str() != stage.node_id.as_str()
        || proof.incarnation_id().to_string() != stage.node_incarnation.as_str()
        || !proof_source_matches
        || proof_bundle.id.as_str() != delivery.bundle.bundle_id.as_str()
        || proof_bundle.revision.as_str() != delivery.bundle.revision.as_str()
        || proof_bundle.digest.as_str() != delivery.bundle.digest.as_str()
        || context.node_id != stage.node_id
        || context.node_incarnation_id != stage.node_incarnation
        || context.workspace_id != stage.workspace_id
        || binding.node_id != stage.node_id
        || binding.node_incarnation != stage.node_incarnation
        || binding.workspace_id.as_str() != proof.workspace_id().as_str()
        || !session_matches
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn does not match exact staged delivery",
        ));
    }
    Ok(())
}

fn bound_continuation_from_proof(
    engine: &HarnessEngine,
    continuation_ref: &HarnessContinuationRef,
    expected_revision: HarnessRevision,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
    proof: &c2::AcceptedSpawnBindingProof,
    bound_at_unix_ms: u64,
) -> Result<HarnessContinuationV1, HarnessServiceError> {
    let current = engine.continuation(continuation_ref)
        .ok_or(HarnessServiceError::InvalidContinuationProof(
            "continuation authority is missing",
        ))?;
    let proof_context = proof.context().ok_or(
        HarnessServiceError::InvalidContinuationProof(
            "accepted spawn has no exact ContextPack receipt",
        ),
    )?;
    let expected_context = current.context.as_ref().ok_or(
        HarnessServiceError::InvalidContinuationProof(
            "exported continuation has no ContextPack receipt",
        ),
    )?;
    let binding = run.binding.as_ref().ok_or(
        HarnessServiceError::InvalidContinuationProof(
            "continued target run has no binding",
        ),
    )?;
    if current.state != HarnessContinuationStateV1::Exported
        || current.revision != expected_revision
        || current.target_run_id != run.run_id
        || current.operation_id != operation.operation_id
        || run.continuation_receipt.as_ref() != Some(&current.receipt_ref)
        || &context_receipt_from_node(proof_context)? != expected_context
        || bound_at_unix_ms < current.updated_at_unix_ms
    {
        return Err(HarnessServiceError::InvalidContinuationProof(
            "accepted spawn does not match exported continuation authority",
        ));
    }
    let mut bound = current.clone();
    bound.revision = next_harness_revision(bound.revision, "continuation")?;
    bound.state = HarnessContinuationStateV1::Bound;
    bound.target_binding = Some(binding.clone());
    bound.bound_at_unix_ms = Some(bound_at_unix_ms);
    bound.updated_at_unix_ms = bound_at_unix_ms;
    Ok(bound)
}

fn validate_committed_continuation_replay(
    engine: &HarnessEngine,
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
    continuation_ref: &HarnessContinuationRef,
    proof: &c2::AcceptedSpawnBindingProof,
) -> Result<(), HarnessServiceError> {
    let continuation = engine.continuation(continuation_ref).ok_or(
        HarnessServiceError::InvalidContinuationProof(
            "committed continuation authority is missing",
        ),
    )?;
    let context = validate_accepted_spawn_request_identity(contexts, operation, proof)?;
    let binding = run.binding.as_ref().ok_or(
        HarnessServiceError::InvalidContinuationProof(
            "committed continuation run has no binding",
        ),
    )?;
    let proof_context = proof.context().ok_or(
        HarnessServiceError::InvalidContinuationProof(
            "replayed proof has no ContextPack receipt",
        ),
    )?;
    let expected_context = continuation.context.as_ref().ok_or(
        HarnessServiceError::InvalidContinuationProof(
            "committed continuation has no ContextPack receipt",
        ),
    )?;
    let managed_accepted = validate_managed_accepted_workspace_authority(
        issued_launches,
        run,
        context,
        proof,
    )?;
    let (instance_id, generation) = proof.runtime_identity();
    let exact_session = matches!(
        &binding.session,
        HarnessSessionIdentityV1::Managed {
            record_id,
            active_session: Some(active),
        } if record_id.as_str() == proof.record_id().as_str()
            && active.instance_id == instance_id
            && active.generation == generation
    );
    if continuation.state != HarnessContinuationStateV1::Bound
        || continuation.target_run_id != run.run_id
        || continuation.operation_id != operation.operation_id
        || run.continuation_receipt.as_ref() != Some(&continuation.receipt_ref)
        || continuation.target_binding.as_ref() != Some(binding)
        || proof.node_id().as_str() != context.node_id.as_str()
        || proof.incarnation_id().to_string() != context.node_incarnation_id.as_str()
        || if managed_accepted {
            proof.managed_worktree().is_none_or(|managed| {
                managed.source_workspace_id().as_str() != context.workspace_id.as_str()
            })
        } else {
            proof.workspace_id().as_str() != context.workspace_id.as_str()
        }
        || proof.provider().as_str() != context.expected_provider.as_str()
        || &context_receipt_from_node(proof_context)? != expected_context
        || !exact_session
    {
        return Err(HarnessServiceError::InvalidContinuationProof(
            "replayed accepted proof does not match committed continuation",
        ));
    }
    Ok(())
}

fn validate_managed_issued_launch(
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
) -> Result<bool, HarnessServiceError> {
    let HarnessWorktreeIntentV1::ManagedProfile {
        profile_id,
        expected_profile_revision,
    } = &run.intent.worktree else {
        return Ok(false);
    };
    let issuance = issued_launches.get(&run.operation_id).ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof(
            "managed accepted spawn has no run-linked issuance",
        ),
    )?;
    validate_issued_launch_snapshot(issuance, run)?;
    if !matches!(
            &issuance.target.worktree,
            HarnessLaunchWorktreeSelectionV1::Managed {
                profile_id: issued_profile_id,
                expected_profile_revision: issued_profile_revision,
            } if issued_profile_id == profile_id
                && issued_profile_revision == expected_profile_revision
        )
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "managed accepted spawn does not match durable issuance",
        ));
    }
    Ok(true)
}

fn contexts_with_accepted_spawn_binding(
    contexts: &BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
    proof: &c2::AcceptedSpawnBindingProof,
) -> Result<BTreeMap<HarnessOperationId, HarnessDispatchContextV1>, HarnessServiceError> {
    let mut next = contexts.clone();
    let context = next.get_mut(&operation.operation_id).ok_or(
        HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn has no durable dispatch context",
        ),
    )?;
    let receipt = match proof.managed_worktree() {
        Some(managed) => {
            let issuance = issued_launches.get(&run.operation_id).ok_or(
                HarnessServiceError::InvalidAcceptedSpawnProof(
                    "managed accepted spawn has no run-linked issuance",
                ),
            )?;
            validate_issued_launch_snapshot(issuance, run)?;
            Some(HarnessManagedWorktreeBindingReceiptV1 {
                lease_id: managed.lease_id().clone(),
                launch_issuance: issuance.reference(),
                source_workspace_id: HarnessSelectorV1::new(
                    managed.source_workspace_id().as_str(),
                )?,
                allocated_workspace_id: HarnessSelectorV1::new(
                    managed.allocated_workspace_id().as_str(),
                )?,
                profile_id: HarnessSelectorV1::new(managed.profile_id().as_str())?,
                profile_revision: HarnessSelectorV1::new(
                    managed.profile_revision().as_str(),
                )?,
            })
        }
        None => None,
    };
    if context.managed_worktree_binding.is_some()
        && context.managed_worktree_binding != receipt
    {
        return Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn changed durable managed worktree binding",
        ));
    }
    context.managed_worktree_binding = receipt;
    context.validate()?;
    Ok(next)
}

fn validate_managed_accepted_workspace_authority(
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
    context: &HarnessDispatchContextV1,
    proof: &c2::AcceptedSpawnBindingProof,
) -> Result<bool, HarnessServiceError> {
    let managed_issued = validate_managed_issued_launch(issued_launches, run)?;
    match (managed_issued, &run.intent.worktree, proof.managed_worktree()) {
        (
            true,
            HarnessWorktreeIntentV1::ManagedProfile {
                profile_id,
                expected_profile_revision,
            },
            Some(managed),
        ) if managed.source_workspace_id().as_str() == run.intent.workspace_id.as_str()
            && managed.allocated_workspace_id() == proof.workspace_id()
            && managed.source_workspace_id() != managed.allocated_workspace_id()
            && managed.profile_id().as_str() == profile_id.as_str()
            && managed.profile_revision().as_str() == expected_profile_revision.as_str()
            && context.managed_worktree_binding.as_ref().is_none_or(|receipt| {
                issued_launches.get(&run.operation_id).is_some_and(|issuance| {
                    receipt.lease_id == *managed.lease_id()
                        && receipt.launch_issuance == issuance.reference()
                        && receipt.source_workspace_id.as_str()
                            == managed.source_workspace_id().as_str()
                        && receipt.allocated_workspace_id.as_str()
                            == managed.allocated_workspace_id().as_str()
                        && receipt.profile_id.as_str() == managed.profile_id().as_str()
                        && receipt.profile_revision.as_str()
                            == managed.profile_revision().as_str()
                })
            }) => Ok(true),
        (false, HarnessWorktreeIntentV1::Existing | HarnessWorktreeIntentV1::Managed { .. }, None) => {
            Ok(false)
        }
        _ => Err(HarnessServiceError::InvalidAcceptedSpawnProof(
            "accepted spawn worktree does not match sealed durable authority",
        )),
    }
}

fn validate_staged_delivery_proof(
    engine: &HarnessEngine,
    delivery: &HarnessDeliveryV1,
    proof: &c2::StagedDeliveryProof,
) -> Result<(), HarnessServiceError> {
    let run = engine.run(&delivery.run_id)
        .ok_or_else(|| HarnessEngineError::NotFound(delivery.run_id.to_string()))?;
    let operation = engine.operation(&delivery.operation_id)
        .ok_or_else(|| HarnessEngineError::NotFound(delivery.operation_id.to_string()))?;
    let proof_bundle = delivery::resolved_bundle_identity(
        proof.selector().clone(),
        proof.receipt(),
    )?;
    let manifest = proof.manifest();
    if proof.operation_id() != &delivery.operation_id
        || proof.run_id() != &delivery.run_id
        || operation.run_id.as_ref() != Some(&delivery.run_id)
        || run.operation_id != delivery.operation_id
        || proof.node_id().as_str() != run.intent.node_id.as_str()
        || proof.workspace_id().as_str() != run.intent.workspace_id.as_str()
        || proof.selector() != &delivery.bundle.selector
        || proof_bundle != delivery.bundle
        || manifest.bundle_id.as_str() != delivery.bundle.bundle_id.as_str()
        || manifest.revision.as_str() != delivery.bundle.revision.as_str()
        || manifest.bundle_digest.as_str() != delivery.bundle.digest.as_str()
        || manifest.manifest_digest.as_str() != delivery.bundle.manifest_digest.as_str()
    {
        return Err(HarnessServiceError::InvalidStagedDeliveryProof(
            "Node delivery proof does not match prepared authority",
        ));
    }
    Ok(())
}

fn validate_run_dispatch_intent(
    run: &HarnessRunV1,
    context: &HarnessDispatchContextV1,
) -> Result<(), HarnessServiceError> {
    if context.node_id != run.intent.node_id
        || context.workspace_id != run.intent.workspace_id
        || context.provider_profile != run.intent.provider_profile
        || context.mode != run.intent.mode
    {
        return Err(HarnessServiceError::InvalidDispatchContext(
            "dispatch context does not match run intent",
        ));
    }
    Ok(())
}

fn continuation_source_session(
    continuation: &HarnessContinuationV1,
) -> Result<SessionAddress, HarnessServiceError> {
    let active = match &continuation.source_binding.session {
        gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed {
            active_session: Some(active),
            ..
        } => active,
        _ => return Err(HarnessServiceError::InvalidContinuationProof(
            "source binding has no exact active managed session",
        )),
    };
    Ok(SessionAddress {
        workspace_id: gate4agent_node_protocol::WorkspaceId::new(
            continuation.source_binding.workspace_id.as_str(),
        ).map_err(|_| HarnessServiceError::InvalidContinuationProof(
            "source workspace is invalid",
        ))?,
        session: gate4agent_node_protocol::SessionKey {
            instance_id: gate4agent_types::AgentInstanceId(active.instance_id),
            generation: gate4agent_types::SessionGeneration(active.generation),
        },
    })
}

/// `pub`: reused verbatim by `gate4agent-harness-light`'s resource-mutation
/// relay (`ExportContextPack`) via `c2::correlate_resource_mutation_response`,
/// which already calls this function internally -- pure node-receipt-to-wire
/// projection, no task-kernel/SQLite dependency.
pub fn context_receipt_from_node(
    context: &gate4agent_node_protocol::ResolvedContextPackReceipt,
) -> Result<HarnessResolvedContextPackReceiptV1, HarnessServiceError> {
    let receipt = HarnessResolvedContextPackReceiptV1 {
        id: HarnessSelectorV1::new(context.id.as_str())?,
        digest: context.digest.as_str().to_owned(),
        lineage: HarnessContextPackLineageV1 {
            source_node_id: HarnessSelectorV1::new(
                context.lineage.source_node_id.as_str(),
            )?,
            source_workspace_id: HarnessSelectorV1::new(
                context.lineage.source_session.workspace_id.as_str(),
            )?,
            source_instance_id: context.lineage.source_session.session.instance_id.0,
            source_generation: context.lineage.source_session.session.generation.0,
            source_provider: HarnessSelectorV1::new(
                context.lineage.source_provider.as_str(),
            )?,
        },
        source_message_count: context.source_message_count,
        retained_message_count: context.retained_message_count,
        byte_len: context.byte_len,
        truncated: context.truncated,
    };
    receipt.validate()?;
    Ok(receipt)
}

fn validate_run_dispatch_seam(
    engine: &HarnessEngine,
    issued_launches: &BTreeMap<HarnessOperationId, HarnessTaskLaunchIssuanceV1>,
    run: &HarnessRunV1,
    context: &HarnessDispatchContextV1,
    spawn_spec: &SpawnSpec,
    actual_spawn_spec_fingerprint: &HarnessRequestDigest,
) -> Result<(), HarnessServiceError> {
    validate_run_dispatch_intent(run, context)?;
    let mode_matches = match &spawn_spec.overrides.mode {
        SpawnOverride::Set { value: SessionMode::Pty } => {
            context.mode == HarnessExecutionModeV1::Pty
        }
        SpawnOverride::Set { value: SessionMode::Inline } => {
            context.mode == HarnessExecutionModeV1::Inline
        }
        SpawnOverride::Set { value: SessionMode::Acp } => {
            context.mode == HarnessExecutionModeV1::Acp
        }
        SpawnOverride::Inherit | SpawnOverride::Clear => false,
    };
    let provider_matches = match &spawn_spec.overrides.provider {
        SpawnOverride::Set { value } => value.as_str() == context.expected_provider.as_str(),
        SpawnOverride::Inherit | SpawnOverride::Clear => false,
    };
    let worktree_matches = match &run.intent.worktree {
        gate4agent_harness_protocol::HarnessWorktreeIntentV1::Existing => {
            spawn_spec.target.worktree_id.is_none()
        }
        gate4agent_harness_protocol::HarnessWorktreeIntentV1::Managed { worktree_ref } => {
            spawn_spec.target.worktree_id.as_ref()
                .is_some_and(|worktree_id| worktree_id.as_str() == worktree_ref.as_str())
        }
        gate4agent_harness_protocol::HarnessWorktreeIntentV1::ManagedProfile { .. } => {
            spawn_spec.target.worktree_id.is_none()
        }
    };
    let delivery_matches = match &run.intent.delivery_bundle {
        None => engine.delivery_for_run(&run.run_id).is_none()
            && matches!(spawn_spec.overrides.bundle_id, SpawnOverride::Clear),
        Some(selector) => engine.delivery_for_run(&run.run_id).is_some_and(|delivery| {
            let Some(stage) = delivery.stage_receipt.as_ref() else { return false; };
            let authority_current = match &delivery.authority {
                HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                    engine.grant(grant_id).is_some_and(|grant| {
                        grant.revision == *revision
                            && grant.allows_delivery_bundle(selector)
                            && grant.allows_target(
                                &run.intent.node_id,
                                &run.intent.workspace_id,
                                &run.intent.provider_profile,
                                run.intent.mode,
                            )
                    })
                }
                HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                    issued_launches.get(&run.operation_id).is_some_and(|current| {
                        current.reference() == *issuance
                            && current.delivery.as_ref().is_some_and(|selection| {
                                selection.bundle.selector == *selector
                                    && selection.bundle == delivery.bundle
                            })
                    })
                }
            };
            delivery.state == HarnessDeliveryStateV1::Staged
                && authority_current
                && delivery.bundle.selector == *selector
                && stage.bundle == delivery.bundle
                && stage.node_id == context.node_id
                && stage.node_incarnation == context.node_incarnation_id
                && stage.workspace_id == context.workspace_id
                && matches!(
                    &spawn_spec.overrides.bundle_id,
                    SpawnOverride::Set { value }
                        if value.as_str() == delivery.bundle.bundle_id.as_str()
                )
        }),
    };
    let continuation_matches = match (
        &run.intent.continuation,
        &spawn_spec.overrides.context_id,
    ) {
        (None, SpawnOverride::Clear) => engine.continuation_for_run(&run.run_id).is_none(),
        (Some(_), SpawnOverride::Set { value }) => engine
            .continuation_for_run(&run.run_id)
            .is_some_and(|continuation| {
                let authority_current = match &continuation.authority {
                    HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                        engine.grant(grant_id).is_some_and(|grant| {
                            grant.revision == *revision
                                && grant.state
                                    == gate4agent_harness_protocol::SessionGrantStateV1::Active
                                && grant.context_permissions.export
                                && grant.context_permissions.restore
                                && grant.actor_run_id == continuation.source_run_id
                                && run.parent_run_id.as_ref() == Some(&grant.actor_run_id)
                                && grant.allows_target(
                                    &run.intent.node_id,
                                    &run.intent.workspace_id,
                                    &run.intent.provider_profile,
                                    run.intent.mode,
                                )
                        })
                    }
                    HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                        issued_launches.get(&run.operation_id).is_some_and(|current| {
                            current.reference() == *issuance
                                && current.context_source.as_ref().is_some_and(|source| {
                                    source.source_run_id == continuation.source_run_id
                                })
                        })
                    }
                };
                continuation.state == HarnessContinuationStateV1::Exported
                    && continuation.operation_id == run.operation_id
                    && authority_current
                    && run.intent.continuation_source_run_id().ok().flatten().as_ref()
                        == Some(&continuation.source_run_id)
                    && continuation.node_id == context.node_id
                    && continuation.node_incarnation == context.node_incarnation_id
                    && continuation.context.as_ref()
                        .is_some_and(|receipt| receipt.id.as_str() == value.as_str())
            }),
        _ => false,
    };
    if spawn_spec.target.node_id.as_str() != context.node_id.as_str()
        || spawn_spec.target.workspace_id.as_str() != context.workspace_id.as_str()
        || spawn_spec.profile_id.as_str() != context.provider_profile.as_str()
        || !mode_matches
        || !provider_matches
        || !worktree_matches
        || !delivery_matches
        || !continuation_matches
        || &context.spawn_spec_fingerprint != actual_spawn_spec_fingerprint
    {
        return Err(HarnessServiceError::InvalidDispatchContext(
            "dispatch context does not match exact SpawnSpec",
        ));
    }
    Ok(())
}

fn encode_persisted_state(
    checkpoint: &HarnessServiceCheckpointV1,
) -> Result<PersistedHarnessState, HarnessServiceError> {
    let checkpoint_bytes = serde_json::to_vec(checkpoint)?;
    let tasks = checkpoint.engine.tasks.iter()
        .map(|task| encode_entity(task.task_id.to_string(), task.revision.get(), task))
        .collect::<Result<Vec<_>, _>>()?;
    let runs = checkpoint.engine.runs.iter()
        .map(|run| encode_entity(run.run_id.to_string(), run.revision.get(), run))
        .collect::<Result<Vec<_>, _>>()?;
    let grants = checkpoint.engine.grants.iter()
        .map(|grant| encode_entity(grant.grant_id.to_string(), grant.revision.get(), grant))
        .collect::<Result<Vec<_>, _>>()?;
    let dispatches = checkpoint.dispatch_contexts.iter()
        .map(|context| encode_entity(
            context.operation_id.to_string(),
            checkpoint.engine.operations.iter()
                .find(|operation| operation.operation_id == context.operation_id)
                .map(|operation| operation.revision.get())
                .ok_or(HarnessServiceError::Corrupt("orphan dispatch context"))?,
            context,
        ))
        .collect::<Result<Vec<_>, _>>()?;
    let deliveries = checkpoint.deliveries.iter()
        .map(|delivery| encode_entity(
            delivery.delivery_ref.to_string(),
            delivery.revision.get(),
            delivery,
        ))
        .collect::<Result<Vec<_>, _>>()?;
    let continuations = checkpoint.continuations.iter()
        .map(|continuation| encode_entity(
            continuation.continuation_ref.to_string(),
            continuation.revision.get(),
            continuation,
        ))
        .collect::<Result<Vec<_>, _>>()?;
    let operations = checkpoint.engine.operations.iter()
        .map(encode_operation)
        .collect::<Result<Vec<_>, _>>()?;
    let harness_mcp_reservations = checkpoint.harness_mcp_reservations.iter()
        .map(|reservation| encode_entity(
            reservation.reservation_id.as_str().to_owned(),
            reservation.revision.get(),
            reservation,
        ))
        .collect::<Result<Vec<_>, _>>()?;
    let mail_messages = checkpoint.engine.mail_messages.iter()
        .map(encode_mail_message)
        .collect::<Result<Vec<_>, _>>()?;
    let mail_acks = checkpoint.engine.mail_acks.iter()
        .map(encode_mail_ack)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PersistedHarnessState {
        checkpoint: checkpoint_bytes,
        tasks,
        runs,
        grants,
        deliveries,
        continuations,
        dispatches,
        harness_mcp_reservations,
        operations,
        mail_messages,
        mail_acks,
    })
}

fn encode_mail_message(
    message: &gate4agent_harness_protocol::HarnessMailMessageV1,
) -> Result<PersistedMailMessage, HarnessServiceError> {
    let (to_record_id, to_task_id) = match &message.to {
        HarnessMailAddressV1::Session { record_id } => (Some(record_id.as_str().to_owned()), None),
        HarnessMailAddressV1::Task { task_id } => (None, Some(task_id.to_string())),
    };
    Ok(PersistedMailMessage {
        message_id: message.message_id.to_string(),
        to_record_id,
        to_task_id,
        created_at_unix_ms: i64::try_from(message.created_at_unix_ms)
            .map_err(|_| HarnessServiceError::Corrupt("mail message timestamp out of range"))?,
        payload: serde_json::to_vec(message)?,
    })
}

fn encode_mail_ack(
    ack: &gate4agent_harness_protocol::HarnessMailAckV1,
) -> Result<PersistedMailAck, HarnessServiceError> {
    Ok(PersistedMailAck {
        message_id: ack.message_id.to_string(),
        reader_record_id: ack.reader_record_id.as_str().to_owned(),
        acked_at_unix_ms: i64::try_from(ack.acked_at_unix_ms)
            .map_err(|_| HarnessServiceError::Corrupt("mail ack timestamp out of range"))?,
        payload: serde_json::to_vec(ack)?,
    })
}

fn encode_entity<T: Serialize>(
    id: String,
    revision: u64,
    value: &T,
) -> Result<PersistedEntity, HarnessServiceError> {
    Ok(PersistedEntity {
        id,
        revision,
        payload: serde_json::to_vec(value)?,
    })
}

fn encode_operation(
    operation: &HarnessOperationV1,
) -> Result<PersistedOperation, HarnessServiceError> {
    Ok(PersistedOperation {
        id: operation.operation_id.to_string(),
        revision: operation.revision.get(),
        request_digest: operation.request_digest.as_str().to_owned(),
        state: format!("{:?}", operation.state),
        payload: serde_json::to_vec(operation)?,
    })
}

#[derive(Debug, Error)]
pub enum HarnessServiceError {
    #[error("harness service is poisoned; reopen it to replay durable state")]
    Poisoned,
    #[error(transparent)]
    Store(#[from] HarnessStoreError),
    #[error(transparent)]
    Engine(#[from] HarnessEngineError),
    #[error(transparent)]
    Validation(#[from] HarnessValidationError),
    #[error(transparent)]
    DispatchPolicy(#[from] dispatch::HarnessDispatchError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("unsupported harness service checkpoint version {0}")]
    UnsupportedCheckpoint(u16),
    #[error("invalid harness dispatch context: {0}")]
    InvalidDispatchContext(&'static str),
    #[error("harness mutation request digest does not match its canonical request")]
    MutationDigestMismatch,
    #[error("harness mutation digest failed: {0}")]
    MutationDigest(String),
    #[error("unable to fingerprint exact SpawnSpec before durable dispatch")]
    DispatchFingerprint,
    #[error("run operation must mutate its run atomically")]
    NonAtomicRunOperation,
    #[error("Dispatching to Running requires an authoritative accepted-spawn proof")]
    AcceptedSpawnProofRequired,
    #[error("invalid authoritative accepted-spawn proof: {0}")]
    InvalidAcceptedSpawnProof(&'static str),
    #[error("corrupt harness service store: {0}")]
    Corrupt(&'static str),
    #[error("delivery authority must be prepared and staged before dispatch metadata exists")]
    DeliveryAuthorityWindowClosed,
    #[error("compiled delivery bundle failed exact self-verification")]
    DeliveryCompilationInvalid,
    #[error("invalid authoritative staged-delivery proof: {0}")]
    InvalidStagedDeliveryProof(&'static str),
    #[error("accepted spawn with delivery requires one atomic run, operation, and receipt commit")]
    AtomicDeliveryCommitRequired,
    #[error("continuation authority must be prepared/exported before dispatch")]
    ContinuationAuthorityWindowClosed,
    #[error("invalid sealed continuation proof: {0}")]
    InvalidContinuationProof(&'static str),
    #[error("accepted spawn with continuation requires one atomic run, operation, and authority bind")]
    AtomicContinuationBindRequired,
    #[error("invalid harness MCP reservation: {0}")]
    InvalidHarnessMcpReservation(&'static str),
    #[error(
        "H3B grant actor refused: operation actor is {actor_kind}, run parent \
         is {parent_run_id:?}, grant actor run is {grant_actor_run_id}"
    )]
    HarnessMcpGrantActorRefused {
        actor_kind: &'static str,
        parent_run_id: Option<HarnessRunId>,
        grant_actor_run_id: HarnessRunId,
    },
    #[error(
        "H3B grant {grant_id} operation link refused: dispatch operation \
         {operation_id} already names grant {existing_grant_id}"
    )]
    HarnessMcpGrantOperationLinkRefused {
        grant_id: SessionGrantId,
        operation_id: HarnessOperationId,
        existing_grant_id: SessionGrantId,
    },
    #[error(
        "H3B grant {grant_id} revision refused: durable revision \
         {durable_revision:?} vs presented revision {presented_revision:?}"
    )]
    HarnessMcpGrantRevisionRefused {
        grant_id: SessionGrantId,
        durable_revision: HarnessRevision,
        presented_revision: HarnessRevision,
    },
    #[error(
        "H3B grant {grant_id} link refused: grant actor run \
         {grant_actor_run_id} vs dispatch actor run {dispatch_actor_run_id}"
    )]
    HarnessMcpGrantLinkRefused {
        grant_id: SessionGrantId,
        grant_actor_run_id: HarnessRunId,
        dispatch_actor_run_id: HarnessRunId,
    },
    #[error(
        "H3B grant {grant_id} target refused: presented target \
         {presented_target:?} vs allowed targets {allowed_targets:?}"
    )]
    HarnessMcpGrantTargetRefused {
        grant_id: SessionGrantId,
        presented_target: HarnessGrantTargetV1,
        allowed_targets: Vec<HarnessGrantTargetV1>,
    },
    #[error("harness MCP reservation replay changed durable request identity")]
    HarnessMcpReplayMismatch,
    #[error("harness MCP authority proof does not match current durable state")]
    HarnessMcpProofMismatch,
    #[error(
        "H3B arm proof reservation field {field} refused: durable is \
         {durable} vs proof is {proof}"
    )]
    HarnessMcpArmProofReservationFieldRefused {
        field: &'static str,
        durable: String,
        proof: String,
    },
    #[error(
        "H3B arm proof route {field} refused: durable is {durable} vs \
         route is {route}"
    )]
    HarnessMcpArmProofRouteRefused {
        field: &'static str,
        durable: String,
        route: String,
    },
    #[error(
        "H3B arm proof binding {field} refused: reservation is {expected} \
         vs proof is {actual}"
    )]
    HarnessMcpArmProofBindingRefused {
        field: &'static str,
        expected: String,
        actual: String,
    },
    #[error(
        "H3B arm lookup refused: {missing} is missing for operation \
         {operation_id:?} reservation {reservation_id:?}"
    )]
    HarnessMcpArmDurableLookupMissing {
        missing: &'static str,
        operation_id: Option<HarnessOperationId>,
        reservation_id: Option<HarnessMcpReservationId>,
    },
    #[error(
        "H3B arm reservation {reservation_id:?} refused: state is {state:?}, \
         armed_at is {armed_at_unix_ms}, durable updated_at is \
         {updated_at_unix_ms}, expires_at is {expires_at_unix_ms}"
    )]
    HarnessMcpArmReservationNotReadyRefused {
        reservation_id: HarnessMcpReservationId,
        state: HarnessMcpReservationStateV1,
        armed_at_unix_ms: u64,
        updated_at_unix_ms: u64,
        expires_at_unix_ms: u64,
    },
    #[error(
        "H3B arm route refused: operation {operation_id} context {field} \
         {value} does not parse as a Node route field"
    )]
    HarnessMcpArmRouteInvalid {
        operation_id: HarnessOperationId,
        field: &'static str,
        value: String,
    },
    #[error(
        "H3B launch authority refused: plan harness MCP policy is \
         {plan_policy}, armed reservation present is {reservation_present}"
    )]
    HarnessMcpLaunchPolicyRefused {
        plan_policy: &'static str,
        reservation_present: bool,
    },
    #[error(
        "H3B launch reservation {reservation_id:?} refused: state is \
         {state:?}, must be armed"
    )]
    HarnessMcpLaunchReservationNotArmedRefused {
        reservation_id: HarnessMcpReservationId,
        state: HarnessMcpReservationStateV1,
    },
    #[error(
        "H3B launch operation refused: reservation names operation \
         {reservation_operation_id}, dispatch is for operation \
         {dispatch_operation_id}"
    )]
    HarnessMcpLaunchOperationRefused {
        reservation_operation_id: HarnessOperationId,
        dispatch_operation_id: HarnessOperationId,
    },
    #[error(
        "H3B launch grant refused: plan names grant {plan_grant_id} \
         revision {plan_grant_revision:?}, reservation carries grant \
         {reservation_grant_id} revision {reservation_grant_revision:?}"
    )]
    HarnessMcpLaunchGrantRefused {
        plan_grant_id: SessionGrantId,
        plan_grant_revision: HarnessRevision,
        reservation_grant_id: SessionGrantId,
        reservation_grant_revision: HarnessRevision,
    },
    #[error("H3B accepted spawn requires the specialized atomic reservation transition")]
    HarnessMcpSpecializedTransitionRequired,
    #[error("operator request operation id {operation_id} was reused with different typed intent")]
    OperatorRequestConflict { operation_id: HarnessOperationId },
    #[error("invalid operator task transition from {from:?} to {to:?}")]
    InvalidOperatorTaskTransition {
        from: HarnessTaskStateV1,
        to: HarnessTaskStateV1,
    },
    #[error("task mutation requires every linked run to be terminal")]
    TaskHasActiveRun,
    #[error("task execution specification is missing")]
    ExecutionSpecMissing,
    #[error("task execution specification revision mismatch: expected {expected:?}, actual {actual:?}")]
    ExecutionSpecRevisionMismatch {
        expected: Option<HarnessRevision>,
        actual: Option<HarnessRevision>,
    },
    #[error("task execution specification launch identity does not match")]
    ExecutionSpecLaunchMismatch,
    // Names its own inputs -- the wire mapper (`map_operator_service_error`,
    // `runtime.rs`) used to collapse every one of these into a bare
    // `HarnessOperatorHostErrorV1::Conflict` with no further detail, which is
    // exactly why a launch-catalogue ceiling silently refused every `spec
    // save` on a stack with a large derived catalogue: the operator saw
    // "Conflict" and nothing else, for a request that was in fact valid.
    // `why` is always a fixed literal naming which of `validate_current_
    // launch_options`'s/`validate_current_issued_launch_options`'s checks
    // failed, `plan_id` is `Some` whenever the failing check concerns a
    // specific plan (`None` for a worktree/delivery/continuation shape
    // mismatch that is not about plan identity).
    #[error("reviewed task launch selection for task {task_id} (plan {plan_id:?}) is invalid: {why}")]
    InvalidTaskLaunchSelection {
        task_id: HarnessTaskId,
        plan_id: Option<HarnessSelectorV1>,
        why: &'static str,
    },
    #[error("issued execution CAS mismatch: expected {expected:?}, spec {spec:?}, issuance {issuance:?}")]
    IssuedExecutionCasMismatch {
        expected: HarnessExpectedExecutionSpecRevisionV1,
        spec: Option<HarnessRevision>,
        issuance: Option<HarnessRevision>,
    },
    #[error("selected task is not ready with completed dependencies")]
    TaskNotReady,
    #[error("another durable scheduled dispatch is pending")]
    SchedulerBusy,
    #[error("harness scheduler scan exceeded its fixed resource bound")]
    SchedulerResourceExhausted,
    #[error("harness scheduler durable graph is invalid: {0}")]
    SchedulerInvalidGraph(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_engine::HarnessMutationV1;
    use gate4agent_harness_delivery::{
        compile_reviewed_delivery_bundle_v2, ReviewedDeliverySourceV2,
    };
    use gate4agent_harness_protocol::{
        HarnessActorV1, HarnessContextPermissionsV1, HarnessDeliveryBundleDigestV1,
        HarnessContextSourceAvailabilityV1,
        HarnessContinuationCleanupStateV1, HarnessContinuationRef,
        HarnessContinuationStateV1, HarnessContinuationV1,
        HarnessDeliveryBundleIdV1, HarnessDeliveryBundleRevisionV1,
        HarnessDeliveryBundleV1, HarnessDeliveryManifestDigestV2, HarnessDeliveryRef,
        HarnessDeliveryReceiptV1, HarnessDeliveryStageReceiptV1, HarnessDeliveryStateV1,
        HarnessGrantTargetV1,
        HarnessIdempotencyRef, HarnessMonitoringVisibilityV1, HarnessOperationKindV1,
        HarnessOperationTimeoutsV1, HarnessReadPermissionsV1,
        HarnessOperationStateV1, HarnessRunId, HarnessRunIntentV1,
        HarnessResultDispositionV1, HarnessRunGitFactsOutcomeV1, HarnessRunLifecycleV1,
        HarnessRunV1,
        HarnessRuntimeIdentityV1,
        HarnessReceiptRef, HarnessSessionBindingV1, HarnessSessionIdentityV1, HarnessTaskId,
        HarnessTaskPermissionsV1, HarnessTaskStateV1, HarnessTaskV1,
        HarnessWorktreeIntentV1, SessionGrantId, SessionGrantStateV1, SessionGrantV1,
    };
    use gate4agent_node_protocol::{
        CapabilityId, ContextPackLineageReceipt, DeliveryComponentKindV2,
        DeliveryScopeV2, NodeId, NodeIncarnationId,
        ResolvedBundleReceipt, ResolvedContextPackReceipt, SessionKey, SpawnBundleDigest,
        SpawnBundleId, SpawnBundleRevision, SpawnContextDigest, SpawnContextId,
        SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverrides, SpawnProfileId, SpawnPrompt,
        SpawnRequiredCapabilities, SpawnTarget, WorkspaceId,
    };
    use gate4agent_types::{AgentId, AgentInstanceId, ApprovalLevel, SessionGeneration, TerminalSize};
    use rusqlite::Connection;
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    fn database_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!(
            "gate4agent-harness-{label}-{}-{nonce}.sqlite",
            std::process::id(),
        ))
    }

    fn remove_database(path: &Path) {
        for candidate in [
            path.to_path_buf(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            let _ = fs::remove_file(candidate);
        }
    }

    fn phase6_context_source(observed_at_unix_ms: u64) -> HarnessContextSourceSelectionV1 {
        let mut source = HarnessContextSourceSelectionV1 {
            source_run_id: HarnessRunId::new(format!("hrun_{}", "9".repeat(24))).unwrap(),
            source_run_revision: HarnessRevision::new(7).unwrap(),
            observed_at_unix_ms,
            metadata_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("07070707070707070707070707070707")
                .unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session_record_id: HarnessSelectorV1::new("record-a").unwrap(),
            active_session: Some(HarnessRuntimeIdentityV1 {
                instance_id: 9,
                generation: 3,
            }),
            message_count: 11,
            message_count_exact: true,
            completed_turn_count: Some(5),
            total_tokens: Some(1_024),
            availability: HarnessContextSourceAvailabilityV1::Live,
            context_pack: None,
        };
        source.metadata_digest = context_source_metadata_digest(&source).unwrap();
        source
    }

    fn phase6_launch_options(observed_at_unix_ms: u64) -> HarnessTaskLaunchOptionsV1 {
        let mut options = HarnessTaskLaunchOptionsV1 {
            task_id: HarnessTaskId::new(format!("htask_{}", "8".repeat(24))).unwrap(),
            task_revision: HarnessRevision::new(4).unwrap(),
            policy_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            plans: Vec::new(),
            managed_worktree_profiles: vec![
                gate4agent_harness_api::HarnessManagedWorktreeProfileOptionV1 {
                    node_id: HarnessSelectorV1::new("node-a").unwrap(),
                    node_incarnation: HarnessSelectorV1::new(
                        "07070707070707070707070707070707",
                    ).unwrap(),
                    source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                    profile_id: HarnessSelectorV1::new("review-worktree").unwrap(),
                    profile_revision: HarnessSelectorV1::new("revision-7").unwrap(),
                    retention: gate4agent_harness_api::HarnessManagedWorktreeRetentionV1::Retain,
                    observed_at_unix_ms,
                },
            ],
            context_sources: vec![phase6_context_source(observed_at_unix_ms)],
            delivery_bundles: Vec::new(),
            current_issued_spec: None,
            truncated: false,
            next_after: None,
            context_source_exclusions: Vec::new(),
        };
        options.policy_digest = task_launch_policy_digest(&options).unwrap();
        options
    }

    #[test]
    fn phase6_semantic_digests_ignore_observation_time_but_bind_identity_and_counts() {
        let first = phase6_launch_options(100);
        let refreshed = phase6_launch_options(200);
        assert_eq!(first.policy_digest, refreshed.policy_digest);
        assert_eq!(
            first.context_sources[0].metadata_digest,
            refreshed.context_sources[0].metadata_digest,
        );
        assert!(managed_profile_semantically_matches(
            &first.managed_worktree_profiles[0],
            &refreshed.managed_worktree_profiles[0],
        ));
        assert!(context_source_semantically_matches(
            &first.context_sources[0],
            &refreshed.context_sources[0],
        ));

        let mut changed_count = refreshed.clone();
        changed_count.context_sources[0].message_count += 1;
        changed_count.context_sources[0].metadata_digest =
            context_source_metadata_digest(&changed_count.context_sources[0]).unwrap();
        changed_count.policy_digest = task_launch_policy_digest(&changed_count).unwrap();
        assert_ne!(first.policy_digest, changed_count.policy_digest);
        assert!(!context_source_semantically_matches(
            &first.context_sources[0],
            &changed_count.context_sources[0],
        ));

        let mut changed_profile = refreshed;
        changed_profile.managed_worktree_profiles[0].profile_revision =
            HarnessSelectorV1::new("revision-8").unwrap();
        changed_profile.policy_digest = task_launch_policy_digest(&changed_profile).unwrap();
        assert_ne!(first.policy_digest, changed_profile.policy_digest);
        assert!(!managed_profile_semantically_matches(
            &first.managed_worktree_profiles[0],
            &changed_profile.managed_worktree_profiles[0],
        ));
    }

    // The `icacls`-subprocess form of this helper was unreliable under this
    // workspace's sandboxed test hosts: `/grant:r <username>:(F)` names a
    // specific account, and a sandboxed job/AppContainer layer can make the
    // resulting DACL diverge from a strict single-ACE, owner-only shape
    // (`gate4agent_harness_delivery::validate_source_permissions`'s exact
    // requirement) even though `icacls` itself reports success. Setting the
    // DACL directly via `SetNamedSecurityInfoW` with the well-known "Owner
    // Rights" SID (SDDL `OW`) sidesteps account-name/SID resolution
    // entirely — the identical, already-proven approach
    // `gate4agent-harness-delivery`'s own tests use for the same check.
    #[cfg(windows)]
    fn protect_delivery_test_path(path: &Path) {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        };
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION,
        };

        let sddl = OsStr::new("D:P(A;;FA;;;OW)")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        assert!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            } != 0,
            "owner-only test SDDL failed to convert",
        );
        let mut present = 0_i32;
        let mut defaulted = 0_i32;
        let mut dacl = std::ptr::null_mut();
        let has_dacl = unsafe {
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
        };
        assert!(
            has_dacl != 0 && present != 0 && !dacl.is_null(),
            "owner-only test DACL is missing from the converted descriptor",
        );
        let mut wide = path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            )
        };
        unsafe { LocalFree(descriptor); }
        assert_eq!(status, 0, "SetNamedSecurityInfoW failed to protect the test path");
    }

    #[cfg(unix)]
    fn protect_delivery_test_path(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let directory = fs::metadata(path).unwrap().is_dir();
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        ).unwrap();
    }

    fn revoke_grant(
        service: &mut HarnessService,
        grant_id: &SessionGrantId,
        marker: char,
        now_unix_ms: u64,
    ) {
        let current = service.engine().grant(grant_id).unwrap().clone();
        let mut grant = current.clone();
        grant.revision = HarnessRevision::new(current.revision.get() + 1).unwrap();
        grant.state = SessionGrantStateV1::Revoked;
        grant.updated_at_unix_ms = now_unix_ms;
        let operation = HarnessOperationV1 {
            operation_id: HarnessOperationId::new(format!(
                "hop_{}",
                marker.to_string().repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::RevokeGrant,
            state: HarnessOperationStateV1::Succeeded,
            task_id: None,
            run_id: None,
            grant_id: Some(grant_id.clone()),
            reconciles_operation_id: None,
            expected_revision: Some(current.revision),
            request_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                marker.to_string().repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: now_unix_ms,
            updated_at_unix_ms: now_unix_ms,
            dispatched_at_unix_ms: Some(now_unix_ms),
            finished_at_unix_ms: Some(now_unix_ms),
        };
        let mut mutation = HarnessMutationV1::ReplaceGrant {
            operation,
            expected_revision: current.revision,
            grant,
        };
        mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
        assert_eq!(service.apply(mutation).unwrap(), HarnessApplyOutcome::Applied);
    }

    fn task_id() -> HarnessTaskId {
        HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap()
    }

    fn run_id() -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap()
    }

    fn run_operation_id() -> HarnessOperationId {
        HarnessOperationId::new(format!("hop_{}", "b".repeat(24))).unwrap()
    }

    fn operator_authority(hex: char, now_unix_ms: u64) -> HarnessOperatorAuthorityV1 {
        HarnessOperatorAuthorityV1 {
            operation_id: HarnessOperationId::new(format!(
                "hop_{}",
                hex.to_string().repeat(24),
            )).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                hex.to_string().repeat(24),
            )).unwrap(),
            actor_id: HarnessSelectorV1::new("operator").unwrap(),
            now_unix_ms,
        }
    }

    fn create_task_request(state: HarnessTaskStateV1) -> HarnessCreateTaskRequestV1 {
        HarnessCreateTaskRequestV1 {
            authority: operator_authority('1', 10),
            task_id: task_id(),
            title: "Scheduled task".to_owned(),
            body: "Typed operator body".to_owned(),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: state,
        }
    }

    fn schedule_request(operation_hex: char, run_hex: char) -> HarnessScheduleRequestV1 {
        HarnessScheduleRequestV1 {
            operation_id: HarnessOperationId::new(format!(
                "hop_{}",
                operation_hex.to_string().repeat(24),
            )).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                operation_hex.to_string().repeat(24),
            )).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("scheduler").unwrap(),
            },
            run_id: HarnessRunId::new(format!(
                "hrun_{}",
                run_hex.to_string().repeat(24),
            )).unwrap(),
            parent_run_id: None,
            intent: HarnessRunIntentV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: HarnessSelectorV1::new("codex-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            now_unix_ms: 20,
        }
    }

    fn task() -> HarnessTaskV1 {
        HarnessTaskV1 {
            task_id: task_id(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "Durable harness task".to_owned(),
            body: "Explicit operator-owned task body".to_owned(),
            creator: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Backlog,
            run_ids: Vec::new(),
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        }
    }

    fn operation() -> HarnessOperationV1 {
        HarnessOperationV1 {
            operation_id: HarnessOperationId::new(format!("hop_{}", "a".repeat(24))).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::CreateTask,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id()),
            run_id: None,
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: None,
            request_digest: HarnessRequestDigest::new("a".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "a".repeat(24)
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 11,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(11),
        }
    }

    fn task_mutation() -> HarnessMutationV1 {
        let mut mutation = HarnessMutationV1::CreateTask {
            operation: operation(),
            task: task(),
        };
        let digest = mutation_request_digest(&mutation).unwrap();
        mutation.operation_mut().request_digest = digest;
        mutation
    }

    fn apply_task(service: &mut HarnessService) {
        assert_eq!(
            service.apply(task_mutation()).unwrap(),
            HarnessApplyOutcome::Applied,
        );
    }

    fn create_run(service: &mut HarnessService) -> (HarnessRunV1, HarnessOperationV1) {
        let mut linked_task = service.engine().task(&task_id()).unwrap().clone();
        linked_task.revision = HarnessRevision::new(2).unwrap();
        linked_task.run_ids = vec![run_id()];
        linked_task.updated_at_unix_ms = 12;
        let operation = HarnessOperationV1 {
            operation_id: run_operation_id(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Prepared,
            task_id: Some(task_id()),
            run_id: Some(run_id()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "b".repeat(24)
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 12,
            updated_at_unix_ms: 12,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: None,
        };
        let run = HarnessRunV1 {
            run_id: run_id(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id(),
            operation_id: run_operation_id(),
            intent: HarnessRunIntentV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Requested,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 12,
            updated_at_unix_ms: 12,
        };
        let mut mutation = HarnessMutationV1::CreateRun {
            operation,
            expected_task_revision: HarnessRevision::new(1).unwrap(),
            task: linked_task,
            run,
        };
        let digest = mutation_request_digest(&mutation).unwrap();
        mutation.operation_mut().request_digest = digest;
        service.apply(mutation).unwrap();
        (
            service.engine().run(&run_id()).unwrap().clone(),
            service.engine().operation(&run_operation_id()).unwrap().clone(),
        )
    }

    /// One task (`Review`) with one already-`Completed`, already-bound run —
    /// the exact precondition `record_run_git_facts`/`record_task_result_ref`
    /// require (binding present for git-facts; a run the task can index).
    fn terminal_bound_run_fixture() -> HarnessService {
        let mut current_task = task();
        current_task.state = HarnessTaskStateV1::Review;
        current_task.run_ids = vec![run_id()];
        let run = HarnessRunV1 {
            run_id: run_id(),
            revision: HarnessRevision::new(3).unwrap(),
            parent_run_id: None,
            task_id: task_id(),
            operation_id: run_operation_id(),
            intent: HarnessRunIntentV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: Some(HarnessSessionBindingV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                session: HarnessSessionIdentityV1::Managed {
                    record_id: HarnessSelectorV1::new("record-a").unwrap(),
                    active_session: Some(HarnessRuntimeIdentityV1 { instance_id: 1, generation: 1 }),
                },
            }),
            lifecycle: HarnessRunLifecycleV1::Completed,
            result_disposition: Some(HarnessResultDispositionV1::Succeeded),
            failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 13,
        };
        let operation = HarnessOperationV1 {
            operation_id: run_operation_id(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id()),
            run_id: Some(run_id()),
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
            created_at_unix_ms: 10,
            updated_at_unix_ms: 13,
            dispatched_at_unix_ms: Some(12),
            finished_at_unix_ms: Some(13),
        };
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: gate4agent_harness_engine::HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![current_task],
            runs: vec![run],
            grants: Vec::new(),
            operations: vec![operation],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
            mail_messages: Vec::new(),
            mail_acks: Vec::new(),
        }).unwrap();
        HarnessService::from_engine_for_test(engine)
    }

    #[test]
    fn record_run_git_facts_applies_replays_and_conflicts() {
        let mut service = terminal_bound_run_fixture();
        let node_id = NodeId::new("node-a").unwrap();
        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let facts = HarnessRunGitFactsV1 {
            captured_at_unix_ms: 20,
            outcome: HarnessRunGitFactsOutcomeV1::Unavailable,
        };

        let applied = service.record_run_git_facts(
            &run_id(),
            facts.clone(),
            20,
            &node_id,
            incarnation,
        ).unwrap();
        assert_eq!(applied, HarnessApplyOutcome::Applied);
        assert_eq!(service.engine().run(&run_id()).unwrap().git_facts, Some(facts.clone()));

        // A redundant call with the exact same value is a free no-op.
        let replayed = service.record_run_git_facts(
            &run_id(),
            facts.clone(),
            21,
            &node_id,
            incarnation,
        ).unwrap();
        assert_eq!(replayed, HarnessApplyOutcome::Replayed);
        assert_eq!(service.engine().run(&run_id()).unwrap().git_facts, Some(facts));

        // A different value is a hard conflict, never a silent overwrite.
        let different = HarnessRunGitFactsV1 {
            captured_at_unix_ms: 22,
            outcome: HarnessRunGitFactsOutcomeV1::Unavailable,
        };
        assert!(matches!(
            service.record_run_git_facts(&run_id(), different, 22, &node_id, incarnation),
            Err(HarnessServiceError::Engine(HarnessEngineError::TerminalRunMutation)),
        ));
    }

    #[test]
    fn record_task_result_ref_applies_and_replays() {
        let mut service = terminal_bound_run_fixture();
        let candidate = HarnessResultRef::for_run(&run_id());

        let applied = service.record_task_result_ref(&task_id(), &run_id(), 20).unwrap();
        assert_eq!(applied, HarnessApplyOutcome::Applied);
        assert_eq!(
            service.engine().task(&task_id()).unwrap().result_refs,
            vec![candidate.clone()],
        );

        // A redundant call for the same run's ref is a free no-op.
        let replayed = service.record_task_result_ref(&task_id(), &run_id(), 21).unwrap();
        assert_eq!(replayed, HarnessApplyOutcome::Replayed);
        assert_eq!(service.engine().task(&task_id()).unwrap().result_refs, vec![candidate]);
    }

    #[test]
    fn record_task_result_ref_no_ops_at_capacity() {
        let mut full_task = task();
        full_task.state = HarnessTaskStateV1::Review;
        full_task.result_refs = (0..HARNESS_RESULTS_MAX)
            .map(|index| HarnessRunId::new(format!("hrun_{index:024x}")).unwrap())
            .map(|id| HarnessResultRef::for_run(&id))
            .collect();
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: gate4agent_harness_engine::HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![full_task.clone()],
            runs: Vec::new(),
            grants: Vec::new(),
            operations: Vec::new(),
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
            mail_messages: Vec::new(),
            mail_acks: Vec::new(),
        }).unwrap();
        let mut service = HarnessService::from_engine_for_test(engine);
        let overflow_run = HarnessRunId::new(format!("hrun_{HARNESS_RESULTS_MAX:024x}")).unwrap();

        let outcome = service.record_task_result_ref(&task_id(), &overflow_run, 30).unwrap();
        assert_eq!(outcome, HarnessApplyOutcome::Replayed);
        assert_eq!(service.engine().task(&task_id()).unwrap().result_refs, full_task.result_refs);
    }

    fn spawn_spec(prompt: &str) -> SpawnSpec {
        SpawnSpec {
            target: SpawnTarget {
                node_id: NodeId::new("node-a").unwrap(),
                workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                worktree_id: None,
            },
            profile_id: SpawnProfileId::new("claude-default").unwrap(),
            expected_profile_revision: gate4agent_node_protocol::SpawnProfileRevision::new("r1")
                .unwrap(),
            overrides: SpawnOverrides {
                provider: SpawnOverride::Set {
                    value: gate4agent_types::AgentId::new("claude").unwrap(),
                },
                mode: SpawnOverride::Set {
                    value: SessionMode::Pty,
                },
                terminal_size: SpawnOverride::Inherit,
                prompt: SpawnOverride::Set {
                    value: SpawnPrompt::new(prompt).unwrap(),
                },
                bundle_id: SpawnOverride::Clear,
                context_id: SpawnOverride::Clear,
                environment_profile_id: SpawnOverride::Clear,
                approval_level: None,
            },
            deadline_ms: SpawnDeadlineMs::new(20_000).unwrap(),
            idempotency_key: SpawnIdempotencyKey::new("harness-service-seam").unwrap(),
            required_capabilities: SpawnRequiredCapabilities::new(
                std::iter::empty::<CapabilityId>(),
            ).unwrap(),
        }
    }

    fn exact_launch_plan(
        delivery: bool,
        continuation: bool,
        harness_mcp: bool,
    ) -> dispatch::HarnessLaunchPlanV1 {
        dispatch::HarnessLaunchPlanV1 {
            plan_id: HarnessSelectorV1::new("specialized").unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            worktree: HarnessWorktreeIntentV1::Existing,
            provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
            provider: AgentId::new("claude").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            terminal_size: TerminalSize { rows: 40, columns: 120 },
            prompt_source: dispatch::HarnessPromptSourceV1::TaskBody,
            delivery: delivery.then(|| dispatch::HarnessDeliveryPolicyV1 {
                selector: HarnessSelectorV1::new("review-kit").unwrap(),
                bundle_id: SpawnBundleId::new("bundle.review-kit").unwrap(),
            }),
            continuation: if continuation {
                dispatch::HarnessContinuationPolicyV1::ParentRun
            } else {
                dispatch::HarnessContinuationPolicyV1::None
            },
            grant: dispatch::HarnessGrantPolicyV1::Exact {
                grant_id: SessionGrantId::new(format!(
                    "hgrant_{}",
                    "c".repeat(24),
                )).unwrap(),
                revision: HarnessRevision::new(1).unwrap(),
            },
            harness_mcp: if harness_mcp {
                dispatch::HarnessMcpPolicyV1::GrantBound
            } else {
                dispatch::HarnessMcpPolicyV1::Disabled
            },
            approval_level: ApprovalLevel::default(),
            deadline_ms: 20_000,
        }
    }

    fn ordinary_launch_plan(plan_id: &str) -> dispatch::HarnessLaunchPlanV1 {
        let mut plan = exact_launch_plan(false, false, false);
        plan.plan_id = HarnessSelectorV1::new(plan_id).unwrap();
        plan.grant = dispatch::HarnessGrantPolicyV1::Operator;
        plan
    }

    fn create_task_request_for(
        marker: char,
        task_id: HarnessTaskId,
    ) -> HarnessCreateTaskRequestV1 {
        HarnessCreateTaskRequestV1 {
            authority: operator_authority(marker, 10),
            task_id,
            title: format!("Task {marker}"),
            body: format!("Run selected task {marker}"),
            parent_task_id: None,
            dependencies: Vec::new(),
            initial_state: HarnessTaskStateV1::Ready,
        }
    }

    fn replace_execution_spec_request(
        marker: char,
        task_id: HarnessTaskId,
        scheduled_launch: HarnessScheduledLaunchRefV2,
    ) -> HarnessReplaceTaskExecutionSpecRequestV1 {
        HarnessReplaceTaskExecutionSpecRequestV1 {
            authority: operator_authority(marker, 20),
            task_id,
            expected_task_revision: HarnessRevision::new(1).unwrap(),
            expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            spec: gate4agent_harness_protocol::HarnessTaskExecutionSpecInputV1 {
                scheduled_launch,
                review_policy: gate4agent_harness_protocol::HarnessTaskReviewPolicyV1::OperatorReview,
            },
        }
    }

    fn dispatch_context(
        operation: &HarnessOperationV1,
        fingerprint: HarnessRequestDigest,
    ) -> HarnessDispatchContextV1 {
        HarnessDispatchContextV1 {
            operation_id: operation.operation_id.clone(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessSelectorV1::new("incarnation-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
            expected_provider: HarnessSelectorV1::new("claude").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            baseline_record_ids: Vec::new(),
            spawn_spec_fingerprint: fingerprint,
            dispatched_at_unix_ms: 13,
            idempotency_ref: operation.idempotency_ref.clone(),
            managed_worktree_binding: None,
        }
    }

    fn delivery_ref() -> HarnessDeliveryRef {
        HarnessDeliveryRef::new(format!("hdelivery_{}", "d".repeat(24))).unwrap()
    }

    fn delivery_bundle() -> HarnessDeliveryBundleV1 {
        HarnessDeliveryBundleV1 {
            selector: HarnessSelectorV1::new("review-kit").unwrap(),
            bundle_id: HarnessDeliveryBundleIdV1::new("bundle.review-kit").unwrap(),
            revision: HarnessDeliveryBundleRevisionV1::new("rev-7").unwrap(),
            digest: HarnessDeliveryBundleDigestV1::new(format!(
                "sha256:{}",
                "d".repeat(64),
            )).unwrap(),
            manifest_digest: HarnessDeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "e".repeat(64),
            )).unwrap(),
        }
    }

    fn delivery_engine() -> HarnessEngine {
        let mut linked_task = task();
        linked_task.run_ids = vec![run_id()];
        let operation = HarnessOperationV1 {
            operation_id: run_operation_id(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Prepared,
            task_id: Some(task_id()),
            run_id: Some(run_id()),
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
            created_at_unix_ms: 12,
            updated_at_unix_ms: 12,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: None,
        };
        let run = HarnessRunV1 {
            run_id: run_id(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id(),
            operation_id: run_operation_id(),
            intent: HarnessRunIntentV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: Some(HarnessSelectorV1::new("review-kit").unwrap()),
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Requested,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 12,
            updated_at_unix_ms: 12,
        };
        let grant = SessionGrantV1 {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: run_id(),
            allowed_targets: vec![HarnessGrantTargetV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
            }],
            allowed_delivery_bundles: vec![HarnessSelectorV1::new("review-kit").unwrap()],
            maximum_child_count: 1,
            maximum_child_depth: 1,
            operation_timeouts: HarnessOperationTimeoutsV1 {
                dispatch_ms: 1_000,
                wait_ms: 1_000,
                reconciliation_ms: 1_000,
            },
            task_permissions: HarnessTaskPermissionsV1 {
                read: true,
                create: false,
                mutate: false,
                request_run: true,
            },
            read_permissions: HarnessReadPermissionsV1::default(),
            monitoring_visibility: HarnessMonitoringVisibilityV1::None,
            context_permissions: HarnessContextPermissionsV1 {
                export: false,
                restore: false,
            },
            mail: false,
            state: SessionGrantStateV1::Active,
            created_at_unix_ms: 12,
            updated_at_unix_ms: 12,
        };
        HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: gate4agent_harness_engine::HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![linked_task],
            runs: vec![run],
            grants: vec![grant],
            operations: vec![operation],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
            mail_messages: Vec::new(),
            mail_acks: Vec::new(),
        }).unwrap()
    }

    fn continuation_fixture(
        export: bool,
        restore: bool,
    ) -> (
        HarnessEngine,
        BTreeMap<HarnessOperationId, HarnessDispatchContextV1>,
        HarnessContinuationV1,
    ) {
        let source_run_id = HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap();
        let source_operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "c".repeat(24),
        )).unwrap();
        let incarnation = "07".repeat(16);
        let source_binding = HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-source").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 7,
                    generation: 1,
                }),
            },
        };
        let mut linked_task = task();
        linked_task.run_ids = vec![run_id(), source_run_id.clone()];
        let source_operation = HarnessOperationV1 {
            operation_id: source_operation_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id()),
            run_id: Some(source_run_id.clone()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("c".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "c".repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 12,
            dispatched_at_unix_ms: Some(11),
            finished_at_unix_ms: Some(12),
        };
        let source_run = HarnessRunV1 {
            run_id: source_run_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id(),
            operation_id: source_operation_id.clone(),
            intent: HarnessRunIntentV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: Some(source_binding.clone()),
            lifecycle: HarnessRunLifecycleV1::Running,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 12,
        };
        let target_operation = HarnessOperationV1 {
            operation_id: run_operation_id(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::ParentRun { run_id: source_run_id.clone() },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Prepared,
            task_id: Some(task_id()),
            run_id: Some(run_id()),
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
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: None,
        };
        let target_run = HarnessRunV1 {
            run_id: run_id(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: Some(source_run_id.clone()),
            task_id: task_id(),
            operation_id: run_operation_id(),
            intent: HarnessRunIntentV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: Some(HarnessSelectorV1::new(source_run_id.to_string()).unwrap()),
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Requested,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20,
        };
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let grant = SessionGrantV1 {
            grant_id: grant_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: source_run_id.clone(),
            allowed_targets: vec![HarnessGrantTargetV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
            }],
            allowed_delivery_bundles: Vec::new(),
            maximum_child_count: 1,
            maximum_child_depth: 1,
            operation_timeouts: HarnessOperationTimeoutsV1 {
                dispatch_ms: 1_000,
                wait_ms: 1_000,
                reconciliation_ms: 1_000,
            },
            task_permissions: HarnessTaskPermissionsV1 {
                read: true,
                create: false,
                mutate: false,
                request_run: true,
            },
            read_permissions: HarnessReadPermissionsV1::default(),
            monitoring_visibility: HarnessMonitoringVisibilityV1::None,
            context_permissions: HarnessContextPermissionsV1 { export, restore },
            mail: false,
            state: SessionGrantStateV1::Active,
            created_at_unix_ms: 12,
            updated_at_unix_ms: 12,
        };
        let source_context = HarnessDispatchContextV1 {
            operation_id: source_operation_id,
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
            expected_provider: HarnessSelectorV1::new("claude").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            baseline_record_ids: Vec::new(),
            spawn_spec_fingerprint: HarnessRequestDigest::new("d".repeat(64)).unwrap(),
            dispatched_at_unix_ms: 11,
            idempotency_ref: source_operation.idempotency_ref.clone(),
            managed_worktree_binding: None,
        };
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: gate4agent_harness_engine::HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![linked_task],
            runs: vec![target_run, source_run],
            grants: vec![grant],
            operations: vec![target_operation, source_operation],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
            mail_messages: Vec::new(),
            mail_acks: Vec::new(),
        }).unwrap();
        let continuation = HarnessContinuationV1 {
            continuation_ref: HarnessContinuationRef::new(format!(
                "hcontinuation_{}",
                "d".repeat(24),
            )).unwrap(),
            receipt_ref: HarnessReceiptRef::new(format!(
                "hreceipt_{}",
                "e".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            state: HarnessContinuationStateV1::Prepared,
            authority: HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id,
                revision: HarnessRevision::new(1).unwrap(),
            },
            source_run_id,
            target_run_id: run_id(),
            operation_id: run_operation_id(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            source_provider: HarnessSelectorV1::new("claude").unwrap(),
            source_binding,
            context: None,
            target_binding: None,
            prepared_at_unix_ms: 20,
            exporting_at_unix_ms: None,
            exported_at_unix_ms: None,
            bound_at_unix_ms: None,
            expired_at_unix_ms: None,
            outcome_unknown_at_unix_ms: None,
            outcome_unknown_reason: None,
            cleanup_state: HarnessContinuationCleanupStateV1::Retained,
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20,
        };
        (
            engine,
            BTreeMap::from([(source_context.operation_id.clone(), source_context)]),
            continuation,
        )
    }

    fn h3b_dispatch_engine() -> HarnessEngine {
        let mut checkpoint = delivery_engine().checkpoint();
        let parent_run_id = HarnessRunId::new(format!(
            "hrun_{}",
            "c".repeat(24),
        )).unwrap();
        let parent_operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "c".repeat(24),
        )).unwrap();

        checkpoint.tasks[0].run_ids.push(parent_run_id.clone());
        let child_run = &mut checkpoint.runs[0];
        child_run.parent_run_id = Some(parent_run_id.clone());
        child_run.intent.delivery_bundle = None;

        let mut parent_run = child_run.clone();
        parent_run.run_id = parent_run_id.clone();
        parent_run.parent_run_id = None;
        parent_run.operation_id = parent_operation_id.clone();
        checkpoint.runs.push(parent_run);

        let child_operation = &mut checkpoint.operations[0];
        child_operation.actor = HarnessActorV1::ParentRun {
            run_id: parent_run_id.clone(),
        };
        child_operation.grant_id = None;

        let mut parent_operation = child_operation.clone();
        parent_operation.operation_id = parent_operation_id;
        parent_operation.actor = HarnessActorV1::User {
            actor_id: HarnessSelectorV1::new("operator").unwrap(),
        };
        parent_operation.run_id = Some(parent_run_id.clone());
        parent_operation.request_digest = HarnessRequestDigest::new("c".repeat(64)).unwrap();
        parent_operation.idempotency_ref = HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            "c".repeat(24),
        )).unwrap();
        checkpoint.operations.push(parent_operation);

        checkpoint.grants[0].actor_run_id = parent_run_id;
        checkpoint.grants[0].allowed_delivery_bundles.clear();
        HarnessEngine::restore(checkpoint).unwrap()
    }

    /// H3B shape (b): an operator-started top-level run (`run_id()`,
    /// parentless) whose operation actor is `User` and whose grant is
    /// already minted for the run's own id -- structurally identical to
    /// what Slice A(i)'s `resolve_harness_mcp_grant` /
    /// `SessionGrantV1::default_for_run` produces for `HarnessGrantPolicyV1
    /// ::Operator`. `delivery_engine()`'s base fixture already has exactly
    /// that shape; only its delivery-bundle intent is cleared so the
    /// dispatch seam does not also demand a staged delivery unrelated to
    /// harness MCP.
    fn h3b_operator_dispatch_engine() -> HarnessEngine {
        let mut checkpoint = delivery_engine().checkpoint();
        checkpoint.runs[0].intent.delivery_bundle = None;
        HarnessEngine::restore(checkpoint).unwrap()
    }

    fn prepared_delivery() -> HarnessDeliveryV1 {
        HarnessDeliveryV1 {
            delivery_ref: delivery_ref(),
            revision: HarnessRevision::new(1).unwrap(),
            authority: HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id: SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap(),
                revision: HarnessRevision::new(1).unwrap(),
            },
            task_id: task_id(),
            run_id: run_id(),
            operation_id: run_operation_id(),
            bundle: delivery_bundle(),
            state: HarnessDeliveryStateV1::Prepared,
            stage_receipt: None,
            receipt: None,
            created_at_unix_ms: 13,
            updated_at_unix_ms: 13,
        }
    }

    #[test]
    fn harness_service_rejects_changed_payload_with_reused_digest() {
        let path = database_path("digest");
        let mut service = HarnessService::open(&path).unwrap();
        let exact = task_mutation();
        assert_eq!(service.apply(exact.clone()).unwrap(), HarnessApplyOutcome::Applied);
        assert_eq!(service.apply(exact).unwrap(), HarnessApplyOutcome::Replayed);
        let mut changed = task_mutation();
        if let HarnessMutationV1::CreateTask { task, .. } = &mut changed {
            task.body = "changed payload".to_owned();
        }
        assert!(matches!(
            service.apply(changed),
            Err(HarnessServiceError::MutationDigestMismatch)
        ));
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_scheduler_replays_exact_request_and_conflicts_changed_request() {
        let path = database_path("scheduler-replay");
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        let request = schedule_request('2', '2');
        let first = service.schedule_ready_task(request.clone()).unwrap();
        let replay = service.schedule_ready_task(request.clone()).unwrap();
        assert_eq!(first, replay);

        let mut changed = request;
        changed.intent.provider_profile = HarnessSelectorV1::new("other-profile").unwrap();
        assert!(matches!(
            service.schedule_ready_task(changed),
            Err(HarnessServiceError::OperatorRequestConflict { .. }),
        ));
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn task_start_selects_exact_task_b_and_replays_after_reopen() {
        let path = database_path("task-start-selected-b");
        let task_a = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();
        let task_b = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let plan = ordinary_launch_plan("ordinary-b");
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request_for('1', task_a.clone())).unwrap();
        service.operator_create_task(create_task_request_for('2', task_b.clone())).unwrap();
        let replace = replace_execution_spec_request(
            '3',
            task_b.clone(),
            plan.ordinary_scheduled_ref().unwrap(),
        );
        assert_eq!(
            service.operator_replace_task_execution_spec(&catalog, replace).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        let spec = service.task_execution_spec(&task_b).unwrap().clone();
        assert_ne!(spec.scheduled_launch.plan.digest, spec.scheduled_launch_digest);
        let start = HarnessStartTaskRequestV1 {
            authority: operator_authority('4', 30),
            task_id: task_b.clone(),
            expected_task_revision: HarnessRevision::new(1).unwrap(),
            expected_execution_spec_revision: spec.revision,
            expected_scheduled_launch_digest: spec.scheduled_launch_digest.clone(),
        };
        let first = service.start_task(&catalog, start.clone()).unwrap();
        assert!(!first.replayed);
        assert_eq!(first.dispatch.task_id, task_b);
        assert_eq!(
            service.engine().task(&task_a).unwrap().state,
            HarnessTaskStateV1::Ready,
        );
        service.close().unwrap();

        let mut reopened = HarnessService::open(&path).unwrap();
        assert_eq!(reopened.task_execution_spec(&first.dispatch.task_id), Some(&spec));
        let replay = reopened.start_task(&catalog, start.clone()).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.dispatch, first.dispatch);
        let mut changed_authority = start;
        changed_authority.authority.actor_id = HarnessSelectorV1::new("other-operator").unwrap();
        assert!(matches!(
            reopened.start_task(&catalog, changed_authority),
            Err(HarnessServiceError::OperatorRequestConflict { .. }),
        ));
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn phase6_v2_replace_and_start_are_exact_atomic_and_replay_safe() {
        let path = database_path("phase6-v2-start");
        let plan = ordinary_launch_plan("phase6-ordinary");
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        let plan_option = gate4agent_harness_api::HarnessOrdinaryLaunchPlanOptionV1 {
            plan: plan.plan_ref().unwrap(),
            node_id: plan.node_id.clone(),
            source_workspace_id: plan.workspace_id.clone(),
            provider_profile: plan.provider_profile.clone(),
            provider_id: HarnessSelectorV1::new(plan.provider.as_str()).unwrap(),
            mode: plan.mode,
        };
        let mut options = HarnessFullTaskLaunchCatalogueV1 {
            task_id: task_id(),
            task_revision: HarnessRevision::new(1).unwrap(),
            policy_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            plans: vec![plan_option.clone()],
            managed_worktree_profiles: Vec::new(),
            context_sources: Vec::new(),
            delivery_bundles: Vec::new(),
        };
        options.policy_digest = task_launch_policy_digest_full(&options).unwrap();
        let replace = HarnessReplaceTaskExecutionSpecRequestV2 {
            authority: operator_authority('2', 20),
            task_id: task_id(),
            expected_task_revision: HarnessRevision::new(1).unwrap(),
            expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            selection: gate4agent_harness_api::HarnessReviewedTaskLaunchSelectionV1 {
                plan: plan_option,
                worktree: HarnessReviewedWorktreeSelectionV1::Existing,
                context_source: None,
                delivery: None,
                review_policy:
                    gate4agent_harness_protocol::HarnessTaskReviewPolicyV1::OperatorReview,
            },
        };
        assert_eq!(
            service.operator_replace_task_execution_spec_v2(&options, replace.clone()).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        assert_eq!(
            service.operator_replace_task_execution_spec_v2(&options, replace.clone()).unwrap(),
            HarnessApplyOutcome::Replayed,
        );
        let spec = service.task_execution_spec_v2(&task_id()).unwrap().clone();
        let start = HarnessStartTaskRequestV2 {
            authority: operator_authority('3', 30),
            task_id: task_id(),
            expected_task_revision: HarnessRevision::new(1).unwrap(),
            expected_execution_spec_revision: spec.revision,
            expected_launch_issuance: spec.launch_issuance.clone(),
        };
        let first = service.start_task_v2(&catalog, &options, start.clone()).unwrap();
        assert!(!first.replayed);
        assert_eq!(first.dispatch.intent.worktree, HarnessWorktreeIntentV1::Existing);
        assert!(service.engine().delivery_for_run(&first.dispatch.run_id).is_none());
        assert!(service.engine().continuation_for_run(&first.dispatch.run_id).is_none());
        let issued_before_rejected_replace = service.engine()
            .task_launch_issuance(&task_id()).unwrap().clone();
        let historical_before_rejected_replace = service.issued_launches.clone();
        let mut replace_after_start = replace;
        replace_after_start.authority = operator_authority('4', 31);
        replace_after_start.expected_task_revision = HarnessRevision::new(2).unwrap();
        replace_after_start.expected_execution_spec_revision =
            HarnessExpectedExecutionSpecRevisionV1::Exact(spec.revision);
        assert!(matches!(
            service.operator_replace_task_execution_spec_v2(
                &options,
                replace_after_start,
            ),
            Err(HarnessServiceError::InvalidTaskLaunchSelection { .. }),
        ));
        assert_eq!(
            service.engine().task_launch_issuance(&task_id()),
            Some(&issued_before_rejected_replace),
        );
        assert_eq!(service.issued_launches, historical_before_rejected_replace);
        let replay = service.start_task_v2(&catalog, &options, start.clone()).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.dispatch, first.dispatch);

        let mut changed = start;
        changed.authority.actor_id = HarnessSelectorV1::new("other-operator").unwrap();
        assert!(matches!(
            service.start_task_v2(&catalog, &options, changed),
            Err(HarnessServiceError::OperatorRequestConflict { .. }),
        ));
        service.close().unwrap();
        remove_database(&path);
    }

    /// The launch-catalogue ceiling fix's core claim: a full catalogue whose
    /// `plans` list is far larger than the operator wire's own
    /// `HARNESS_TASK_LAUNCH_OPTIONS_MAX` (64) page must still accept a
    /// selection naming a plan past that boundary, and must refuse an
    /// unknown plan by naming it -- never a bare, unqualified `Conflict`.
    #[test]
    fn full_catalogue_past_the_page_ceiling_accepts_a_late_plan_and_names_an_unknown_one() {
        let path = database_path("full-catalogue-past-page-ceiling");
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        let plan_option_count = 70;
        let plans = (0..plan_option_count).map(|index| {
            let plan = ordinary_launch_plan(&format!("plan-{index:03}"));
            gate4agent_harness_api::HarnessOrdinaryLaunchPlanOptionV1 {
                plan: plan.plan_ref().unwrap(),
                node_id: plan.node_id.clone(),
                source_workspace_id: plan.workspace_id.clone(),
                provider_profile: plan.provider_profile.clone(),
                provider_id: HarnessSelectorV1::new(plan.provider.as_str()).unwrap(),
                mode: plan.mode,
            }
        }).collect::<Vec<_>>();
        assert!(plan_option_count > gate4agent_harness_api::HARNESS_TASK_LAUNCH_OPTIONS_MAX);
        let mut catalogue = HarnessFullTaskLaunchCatalogueV1 {
            task_id: task_id(),
            task_revision: HarnessRevision::new(1).unwrap(),
            policy_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            plans,
            managed_worktree_profiles: Vec::new(),
            context_sources: Vec::new(),
            delivery_bundles: Vec::new(),
        };
        catalogue.policy_digest = task_launch_policy_digest_full(&catalogue).unwrap();

        // An unknown plan -- never in the 70-plan catalogue -- is refused
        // BY NAME, not by a bare `Conflict`.
        let unknown = ordinary_launch_plan("plan-unknown");
        let unknown_option = gate4agent_harness_api::HarnessOrdinaryLaunchPlanOptionV1 {
            plan: unknown.plan_ref().unwrap(),
            node_id: unknown.node_id.clone(),
            source_workspace_id: unknown.workspace_id.clone(),
            provider_profile: unknown.provider_profile.clone(),
            provider_id: HarnessSelectorV1::new(unknown.provider.as_str()).unwrap(),
            mode: unknown.mode,
        };
        let refused = service.operator_replace_task_execution_spec_v2(
            &catalogue,
            HarnessReplaceTaskExecutionSpecRequestV2 {
                authority: operator_authority('5', 50),
                task_id: task_id(),
                expected_task_revision: HarnessRevision::new(1).unwrap(),
                expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                selection: gate4agent_harness_api::HarnessReviewedTaskLaunchSelectionV1 {
                    plan: unknown_option,
                    worktree: HarnessReviewedWorktreeSelectionV1::Existing,
                    context_source: None,
                    delivery: None,
                    review_policy:
                        gate4agent_harness_protocol::HarnessTaskReviewPolicyV1::OperatorReview,
                },
            },
        );
        match refused {
            Err(HarnessServiceError::InvalidTaskLaunchSelection { plan_id, why, .. }) => {
                assert_eq!(plan_id.as_ref().map(HarnessSelectorV1::as_str), Some("plan-unknown"));
                assert_eq!(why, "selected plan is not a current option in the full catalogue");
            }
            other => panic!("expected a named InvalidTaskLaunchSelection, got {other:?}"),
        }

        // The chosen plan sits at index 69 -- past the 64-item page ceiling
        // that used to make `truncated: true` refuse every save outright.
        let chosen = catalogue.plans[69].clone();
        assert_eq!(chosen.plan.plan_id.as_str(), "plan-069");
        assert_eq!(
            service.operator_replace_task_execution_spec_v2(
                &catalogue,
                HarnessReplaceTaskExecutionSpecRequestV2 {
                    authority: operator_authority('6', 60),
                    task_id: task_id(),
                    expected_task_revision: HarnessRevision::new(1).unwrap(),
                    expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                    selection: gate4agent_harness_api::HarnessReviewedTaskLaunchSelectionV1 {
                        plan: chosen,
                        worktree: HarnessReviewedWorktreeSelectionV1::Existing,
                        context_source: None,
                        delivery: None,
                        review_policy:
                            gate4agent_harness_protocol::HarnessTaskReviewPolicyV1::OperatorReview,
                    },
                },
            ).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn managed_accepted_spawn_binds_source_context_to_distinct_allocated_workspace() {
        let path = database_path("managed-accepted-source-allocated");
        let plan = ordinary_launch_plan("phase6-managed");
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        let plan_option = gate4agent_harness_api::HarnessOrdinaryLaunchPlanOptionV1 {
            plan: plan.plan_ref().unwrap(),
            node_id: plan.node_id.clone(),
            source_workspace_id: plan.workspace_id.clone(),
            provider_profile: plan.provider_profile.clone(),
            provider_id: HarnessSelectorV1::new(plan.provider.as_str()).unwrap(),
            mode: plan.mode,
        };
        let profile = gate4agent_harness_api::HarnessManagedWorktreeProfileOptionV1 {
            node_id: plan.node_id.clone(),
            node_incarnation: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
            source_workspace_id: plan.workspace_id.clone(),
            profile_id: HarnessSelectorV1::new("review-worktree").unwrap(),
            profile_revision: HarnessSelectorV1::new("revision-7").unwrap(),
            retention: gate4agent_harness_api::HarnessManagedWorktreeRetentionV1::Retain,
            observed_at_unix_ms: 15,
        };
        let mut options = HarnessFullTaskLaunchCatalogueV1 {
            task_id: task_id(),
            task_revision: HarnessRevision::new(1).unwrap(),
            policy_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            plans: vec![plan_option.clone()],
            managed_worktree_profiles: vec![profile.clone()],
            context_sources: Vec::new(),
            delivery_bundles: Vec::new(),
        };
        options.policy_digest = task_launch_policy_digest_full(&options).unwrap();
        service.operator_replace_task_execution_spec_v2(
            &options,
            HarnessReplaceTaskExecutionSpecRequestV2 {
                authority: operator_authority('2', 20),
                task_id: task_id(),
                expected_task_revision: HarnessRevision::new(1).unwrap(),
                expected_execution_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                selection: gate4agent_harness_api::HarnessReviewedTaskLaunchSelectionV1 {
                    plan: plan_option,
                    worktree: HarnessReviewedWorktreeSelectionV1::Managed {
                        profile: profile.clone(),
                    },
                    context_source: None,
                    delivery: None,
                    review_policy:
                        gate4agent_harness_protocol::HarnessTaskReviewPolicyV1::OperatorReview,
                },
            },
        ).unwrap();
        let spec = service.task_execution_spec_v2(&task_id()).unwrap().clone();
        let started = service.start_task_v2(
            &catalog,
            &options,
            HarnessStartTaskRequestV2 {
                authority: operator_authority('3', 30),
                task_id: task_id(),
                expected_task_revision: HarnessRevision::new(1).unwrap(),
                expected_execution_spec_revision: spec.revision,
                expected_launch_issuance: spec.launch_issuance,
            },
        ).unwrap();
        let operation_id = started.dispatch.operation_id.clone();
        let run_id = started.dispatch.run_id.clone();
        let mut dispatching_run = service.engine().run(&run_id).unwrap().clone();
        let mut dispatching_operation = service.engine().operation(&operation_id).unwrap().clone();
        dispatching_run.revision = HarnessRevision::new(2).unwrap();
        dispatching_run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        dispatching_run.updated_at_unix_ms = 31;
        dispatching_operation.revision = HarnessRevision::new(2).unwrap();
        dispatching_operation.state = HarnessOperationStateV1::Dispatching;
        dispatching_operation.dispatched_at_unix_ms = Some(31);
        dispatching_operation.updated_at_unix_ms = 31;
        let spawn = spawn_spec("managed accepted proof");
        let fingerprint = c2::spawn_spec_fingerprint(&spawn).unwrap();
        let context = HarnessDispatchContextV1 {
            operation_id: operation_id.clone(),
            node_id: plan.node_id.clone(),
            node_incarnation_id: profile.node_incarnation.clone(),
            workspace_id: plan.workspace_id.clone(),
            provider_profile: plan.provider_profile.clone(),
            expected_provider: HarnessSelectorV1::new(plan.provider.as_str()).unwrap(),
            mode: plan.mode,
            baseline_record_ids: Vec::new(),
            spawn_spec_fingerprint: fingerprint.clone(),
            dispatched_at_unix_ms: 31,
            idempotency_ref: dispatching_operation.idempotency_ref.clone(),
            managed_worktree_binding: None,
        };
        service.begin_run_dispatch(
            HarnessRevision::new(1).unwrap(),
            dispatching_run,
            HarnessRevision::new(1).unwrap(),
            dispatching_operation,
            context,
            &spawn,
        ).unwrap();

        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let allocated = WorkspaceId::new("workspace-managed-1").unwrap();
        let session = SessionAddress {
            workspace_id: allocated.clone(),
            session: SessionKey {
                instance_id: AgentInstanceId(17),
                generation: SessionGeneration(2),
            },
        };
        let direct_proof = c2::accepted_spawn_binding_proof_for_test(
            operation_id.clone(),
            fingerprint,
            service.engine().operation(&operation_id).unwrap().idempotency_ref.clone(),
            NodeId::new(plan.node_id.as_str()).unwrap(),
            incarnation,
            allocated.clone(),
            plan.provider.clone(),
            SessionMode::Pty,
            SessionRecordId::new("record-managed-1").unwrap(),
            session.clone(),
            None,
            None,
        );
        let proof = c2::accepted_managed_spawn_binding_proof_for_test(
            direct_proof.clone(),
            ManagedWorktreeLeaseId::new("lease-managed-1").unwrap(),
            WorkspaceId::new(plan.workspace_id.as_str()).unwrap(),
            WorktreeProfileId::new(profile.profile_id.as_str()).unwrap(),
            WorktreeProfileRevision::new(profile.profile_revision.as_str()).unwrap(),
        );
        let mut running = service.engine().run(&run_id).unwrap().clone();
        running.revision = HarnessRevision::new(3).unwrap();
        running.lifecycle = HarnessRunLifecycleV1::Running;
        running.binding = Some(HarnessSessionBindingV1 {
            node_id: plan.node_id.clone(),
            node_incarnation: profile.node_incarnation.clone(),
            workspace_id: HarnessSelectorV1::new(allocated.as_str()).unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-managed-1").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: session.session.instance_id.0,
                    generation: session.session.generation.0,
                }),
            },
        });
        running.updated_at_unix_ms = 32;
        let mut succeeded = service.engine().operation(&operation_id).unwrap().clone();
        succeeded.revision = HarnessRevision::new(3).unwrap();
        succeeded.state = HarnessOperationStateV1::Succeeded;
        succeeded.finished_at_unix_ms = Some(32);
        succeeded.updated_at_unix_ms = 32;

        let wrong_source = c2::accepted_managed_spawn_binding_proof_for_test(
            direct_proof.clone(),
            ManagedWorktreeLeaseId::new("lease-managed-1").unwrap(),
            WorkspaceId::new("workspace-wrong-source").unwrap(),
            WorktreeProfileId::new(profile.profile_id.as_str()).unwrap(),
            WorktreeProfileRevision::new(profile.profile_revision.as_str()).unwrap(),
        );
        let wrong_profile = c2::accepted_managed_spawn_binding_proof_for_test(
            direct_proof,
            ManagedWorktreeLeaseId::new("lease-managed-1").unwrap(),
            WorkspaceId::new(plan.workspace_id.as_str()).unwrap(),
            WorktreeProfileId::new(profile.profile_id.as_str()).unwrap(),
            WorktreeProfileRevision::new("revision-8").unwrap(),
        );
        let before = service.committed_snapshot();
        for rejected in [&wrong_source, &wrong_profile] {
            assert!(matches!(
                service.transition_run_with_accepted_spawn(
                    HarnessRevision::new(2).unwrap(),
                    running.clone(),
                    HarnessRevision::new(2).unwrap(),
                    succeeded.clone(),
                    rejected,
                ),
                Err(HarnessServiceError::InvalidAcceptedSpawnProof(_)),
            ));
            assert_eq!(service.committed_snapshot(), before);
        }
        service.transition_run_with_accepted_spawn(
            HarnessRevision::new(2).unwrap(),
            running,
            HarnessRevision::new(2).unwrap(),
            succeeded,
            &proof,
        ).unwrap();
        assert_eq!(
            service.engine().run(&run_id).unwrap().binding.as_ref().unwrap().workspace_id.as_str(),
            allocated.as_str(),
        );
        service.close().unwrap();

        let reopened = HarnessService::open(&path).unwrap();
        let restored_run = reopened.engine().run(&run_id).unwrap();
        let restored_operation = reopened.engine().operation(&operation_id).unwrap();
        let restored_context = reopened.dispatch_context(&operation_id).unwrap();
        assert_eq!(
            restored_context.managed_worktree_binding.as_ref().unwrap()
                .allocated_workspace_id.as_str(),
            allocated.as_str(),
        );
        let mut tampered_contexts = reopened.dispatch_contexts.clone();
        tampered_contexts.get_mut(&operation_id).unwrap()
            .managed_worktree_binding.as_mut().unwrap().allocated_workspace_id =
            HarnessSelectorV1::new("workspace-managed-tampered").unwrap();
        assert!(validate_authoritative_dispatch_binding(
            reopened.engine(),
            &tampered_contexts,
            &reopened.issued_launches,
            restored_run,
            restored_operation,
        ).is_err());

        let historical_issuance = reopened.issued_launches
            .get(&operation_id).unwrap().clone();
        let mut later_attempt_checkpoint = reopened.engine().checkpoint();
        let later_attempt_issuance = later_attempt_checkpoint.issuances
            .iter_mut().find(|issuance| issuance.task_id == task_id()).unwrap();
        later_attempt_issuance.revision = HarnessRevision::new(2).unwrap();
        later_attempt_issuance.updated_at_unix_ms = 40;
        let HarnessLaunchWorktreeSelectionV1::Managed {
            expected_profile_revision,
            ..
        } = &mut later_attempt_issuance.target.worktree else {
            panic!("managed fixture lost managed launch selection");
        };
        *expected_profile_revision = HarnessSelectorV1::new("revision-8").unwrap();
        later_attempt_issuance.digest = task_launch_issuance_digest(
            later_attempt_issuance,
        ).unwrap();
        let later_attempt_spec = later_attempt_checkpoint.execution_specs_v2
            .iter_mut().find(|spec| spec.task_id == task_id()).unwrap();
        later_attempt_spec.revision = HarnessRevision::new(2).unwrap();
        later_attempt_spec.launch_issuance = later_attempt_issuance.reference();
        later_attempt_spec.updated_at_unix_ms = 40;
        let completed_run = later_attempt_checkpoint.runs
            .iter_mut().find(|run| run.run_id == run_id).unwrap();
        completed_run.revision = HarnessRevision::new(4).unwrap();
        completed_run.lifecycle = HarnessRunLifecycleV1::Completed;
        completed_run.result_disposition = Some(HarnessResultDispositionV1::Succeeded);
        completed_run.updated_at_unix_ms = 40;
        let completed_task = later_attempt_checkpoint.tasks
            .iter_mut().find(|task| task.task_id == task_id()).unwrap();
        completed_task.revision = HarnessRevision::new(3).unwrap();
        completed_task.state = HarnessTaskStateV1::Review;
        completed_task.updated_at_unix_ms = 40;
        let later_attempt_engine = HarnessEngine::restore(later_attempt_checkpoint).unwrap();
        assert_ne!(
            later_attempt_engine.task_launch_issuance(&task_id()).unwrap().reference(),
            historical_issuance.reference(),
        );
        validate_authoritative_dispatch_binding(
            &later_attempt_engine,
            &reopened.dispatch_contexts,
            &reopened.issued_launches,
            later_attempt_engine.run(&run_id).unwrap(),
            later_attempt_engine.operation(&operation_id).unwrap(),
        ).unwrap();
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn execution_spec_and_start_reject_stale_revision_digest_and_authority() {
        let path = database_path("task-start-cas");
        let task = task_id();
        let plan = ordinary_launch_plan("ordinary-cas");
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        let replace = replace_execution_spec_request(
            '2',
            task.clone(),
            plan.ordinary_scheduled_ref().unwrap(),
        );
        service.operator_replace_task_execution_spec(&catalog, replace.clone()).unwrap();
        assert!(matches!(
            service.operator_replace_task_execution_spec(
                &catalog,
                HarnessReplaceTaskExecutionSpecRequestV1 {
                    authority: operator_authority('3', 21),
                    ..replace.clone()
                },
            ),
            Err(HarnessServiceError::ExecutionSpecRevisionMismatch { .. }),
        ));
        let mut changed_replay = replace;
        changed_replay.spec.scheduled_launch.plan.digest =
            HarnessRequestDigest::new("f".repeat(64)).unwrap();
        assert!(matches!(
            service.operator_replace_task_execution_spec(&catalog, changed_replay),
            Err(HarnessServiceError::OperatorRequestConflict { .. }),
        ));
        let spec = service.task_execution_spec(&task).unwrap().clone();
        let mut stale_start = HarnessStartTaskRequestV1 {
            authority: operator_authority('4', 30),
            task_id: task.clone(),
            expected_task_revision: HarnessRevision::new(1).unwrap(),
            expected_execution_spec_revision: HarnessRevision::new(2).unwrap(),
            expected_scheduled_launch_digest: spec.scheduled_launch_digest.clone(),
        };
        assert!(matches!(
            service.start_task(&catalog, stale_start.clone()),
            Err(HarnessServiceError::ExecutionSpecRevisionMismatch { .. }),
        ));
        stale_start.expected_execution_spec_revision = spec.revision;
        stale_start.expected_task_revision = HarnessRevision::new(2).unwrap();
        assert!(matches!(
            service.start_task(&catalog, stale_start.clone()),
            Err(HarnessServiceError::Engine(
                HarnessEngineError::ExpectedRevisionMismatch { entity: "task", .. },
            )),
        ));
        stale_start.expected_task_revision = HarnessRevision::new(1).unwrap();
        stale_start.expected_scheduled_launch_digest =
            HarnessRequestDigest::new("e".repeat(64)).unwrap();
        assert!(matches!(
            service.start_task(&catalog, stale_start),
            Err(HarnessServiceError::ExecutionSpecLaunchMismatch),
        ));
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn execution_spec_rejects_delivery_continuation_mcp_and_exact_grant_plans() {
        let path = database_path("execution-spec-privileged");
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        for (index, plan) in [
            exact_launch_plan(true, false, false),
            exact_launch_plan(false, true, false),
            exact_launch_plan(false, false, true),
            exact_launch_plan(false, false, false),
        ].into_iter().enumerate() {
            let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
            let request = replace_execution_spec_request(
                char::from_digit((index + 2) as u32, 10).unwrap(),
                task_id(),
                HarnessScheduledLaunchRefV2 {
                    plan: plan.plan_ref().unwrap(),
                    authority:
                        gate4agent_harness_protocol::HarnessLaunchAuthorityRefV1::OrdinaryOperator,
                },
            );
            assert!(matches!(
                service.operator_replace_task_execution_spec(&catalog, request),
                Err(HarnessServiceError::DispatchPolicy(
                    dispatch::HarnessDispatchError::OperatorPrivilegedFlow,
                )),
            ));
        }
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn execution_spec_reopen_rejects_tampered_scheduled_launch_digest() {
        let path = database_path("execution-spec-tampered-checkpoint");
        let plan = ordinary_launch_plan("ordinary-tamper");
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        service.operator_replace_task_execution_spec(
            &catalog,
            replace_execution_spec_request(
                '2',
                task_id(),
                plan.ordinary_scheduled_ref().unwrap(),
            ),
        ).unwrap();
        service.close().unwrap();

        let connection = Connection::open(&path).unwrap();
        let encoded: Vec<u8> = connection.query_row(
            "SELECT payload FROM harness_checkpoint WHERE singleton = 1",
            [],
            |row| row.get(0),
        ).unwrap();
        let mut checkpoint: HarnessServiceCheckpointV1 =
            serde_json::from_slice(&encoded).unwrap();
        checkpoint.engine.execution_specs[0].scheduled_launch_digest =
            HarnessRequestDigest::new("d".repeat(64)).unwrap();
        connection.execute(
            "UPDATE harness_checkpoint SET payload = ?1 WHERE singleton = 1",
            [serde_json::to_vec(&checkpoint).unwrap()],
        ).unwrap();
        drop(connection);

        assert!(matches!(
            HarnessService::open(&path),
            Err(HarnessServiceError::Corrupt(
                "invalid task execution specification",
            )),
        ));
        remove_database(&path);
    }

    #[test]
    fn task_start_is_busy_on_foreign_pending_dispatch() {
        let path = database_path("task-start-foreign-pending");
        let task_a = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();
        let task_b = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let plan = ordinary_launch_plan("ordinary-busy");
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request_for('1', task_a)).unwrap();
        service.operator_create_task(create_task_request_for('2', task_b.clone())).unwrap();
        service.operator_replace_task_execution_spec(
            &catalog,
            replace_execution_spec_request(
                '3',
                task_b.clone(),
                plan.ordinary_scheduled_ref().unwrap(),
            ),
        ).unwrap();
        service.schedule_ready_task(schedule_request('4', '4')).unwrap();
        let spec = service.task_execution_spec(&task_b).unwrap().clone();
        assert!(matches!(
            service.start_task(
                &catalog,
                HarnessStartTaskRequestV1 {
                    authority: operator_authority('5', 30),
                    task_id: task_b,
                    expected_task_revision: HarnessRevision::new(1).unwrap(),
                    expected_execution_spec_revision: spec.revision,
                    expected_scheduled_launch_digest: spec.scheduled_launch_digest.clone(),
                },
            ),
            Err(HarnessServiceError::SchedulerBusy),
        ));
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_operator_cancel_rejects_active_linked_run() {
        let path = database_path("cancel-active");
        let mut service = HarnessService::open(&path).unwrap();
        service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
        service.schedule_ready_task(schedule_request('2', '2')).unwrap();
        let task = service.engine().task(&task_id()).unwrap();
        let request = HarnessCancelTaskRequestV1 {
            authority: operator_authority('3', 30),
            task_id: task_id(),
            expected_revision: task.revision,
        };
        assert!(matches!(
            service.operator_cancel_task(request),
            Err(HarnessServiceError::TaskHasActiveRun),
        ));
        assert_eq!(
            service.engine().task(&task_id()).unwrap().state,
            HarnessTaskStateV1::Running,
        );
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_operator_mutations_are_typed_cas_and_restart_safe() {
        let path = database_path("operator-mutations");
        let cancel_request = {
            let mut service = HarnessService::open(&path).unwrap();
            service.operator_create_task(create_task_request(HarnessTaskStateV1::Backlog)).unwrap();
            service.operator_replace_task(HarnessReplaceTaskRequestV1 {
                authority: operator_authority('2', 20),
                task_id: task_id(),
                expected_revision: HarnessRevision::new(1).unwrap(),
                title: "Replaced title".to_owned(),
                body: "Replaced body".to_owned(),
                parent_task_id: None,
                dependencies: Vec::new(),
            }).unwrap();
            assert!(matches!(
                service.operator_move_task(HarnessMoveTaskRequestV1 {
                    authority: operator_authority('9', 25),
                    task_id: task_id(),
                    expected_revision: HarnessRevision::new(2).unwrap(),
                    state: HarnessTaskStateV1::Done,
                }),
                Err(HarnessServiceError::InvalidOperatorTaskTransition {
                    from: HarnessTaskStateV1::Backlog,
                    to: HarnessTaskStateV1::Done,
                }),
            ));
            assert!(matches!(
                service.operator_move_task(HarnessMoveTaskRequestV1 {
                    authority: operator_authority('8', 26),
                    task_id: task_id(),
                    expected_revision: HarnessRevision::new(2).unwrap(),
                    state: HarnessTaskStateV1::Failed,
                }),
                Err(HarnessServiceError::InvalidOperatorTaskTransition {
                    from: HarnessTaskStateV1::Backlog,
                    to: HarnessTaskStateV1::Failed,
                }),
            ));
            let request = HarnessCancelTaskRequestV1 {
                authority: operator_authority('3', 30),
                task_id: task_id(),
                expected_revision: HarnessRevision::new(2).unwrap(),
            };
            service.operator_cancel_task(request.clone()).unwrap();
            service.operator_retry_task(HarnessRetryTaskRequestV1 {
                authority: operator_authority('4', 40),
                task_id: task_id(),
                expected_revision: HarnessRevision::new(3).unwrap(),
            }).unwrap();
            service.operator_move_task(HarnessMoveTaskRequestV1 {
                authority: operator_authority('5', 50),
                task_id: task_id(),
                expected_revision: HarnessRevision::new(4).unwrap(),
                state: HarnessTaskStateV1::Review,
            }).unwrap();
            service.operator_move_task(HarnessMoveTaskRequestV1 {
                authority: operator_authority('6', 60),
                task_id: task_id(),
                expected_revision: HarnessRevision::new(5).unwrap(),
                state: HarnessTaskStateV1::Done,
            }).unwrap();
            service.close().unwrap();
            request
        };

        let mut reopened = HarnessService::open(&path).unwrap();
        let task = reopened.engine().task(&task_id()).unwrap();
        assert_eq!(task.title, "Replaced title");
        assert_eq!(task.state, HarnessTaskStateV1::Done);
        assert_eq!(task.revision.get(), 6);
        assert_eq!(
            reopened.operator_cancel_task(cancel_request).unwrap(),
            HarnessApplyOutcome::Replayed,
        );
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_scheduler_restart_recovers_pending_dispatch_before_new_work() {
        let path = database_path("scheduler-restart");
        let first = {
            let mut service = HarnessService::open(&path).unwrap();
            service.operator_create_task(create_task_request(HarnessTaskStateV1::Ready)).unwrap();
            let outcome = service.schedule_ready_task(schedule_request('2', '2')).unwrap();
            service.close().unwrap();
            outcome
        };
        let mut reopened = HarnessService::open(&path).unwrap();
        let recovered = reopened.schedule_ready_task(schedule_request('4', '4')).unwrap();
        assert_eq!(recovered, first);
        assert!(reopened.engine().operation(
            &HarnessOperationId::new(format!("hop_{}", "4".repeat(24))).unwrap(),
        ).is_none());
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_delivery_stage_dispatch_and_restart_are_exact_and_private() {
        const RAW_DELIVERY_CANARY: &str =
            "RAW_DELIVERY_BYTES_C:\\provider-home\\private-prompt.txt";
        let path = database_path("delivery");
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine: delivery_engine(),
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        let prepared = prepared_delivery();
        assert_eq!(
            service.prepare_delivery(prepared.clone()).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        assert_eq!(
            service.prepare_delivery(prepared).unwrap(),
            HarnessApplyOutcome::Replayed,
        );
        let mut staged = prepared_delivery();
        staged.revision = HarnessRevision::new(2).unwrap();
        staged.state = HarnessDeliveryStateV1::Staged;
        staged.updated_at_unix_ms = 14;
        staged.stage_receipt = Some(HarnessDeliveryStageReceiptV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("incarnation-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            bundle: staged.bundle.clone(),
            staged_at_unix_ms: 14,
        });
        assert_eq!(
            service.stage_delivery_record(
                HarnessRevision::new(1).unwrap(),
                staged.clone(),
            ).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        service.close().unwrap();

        let mut service = HarnessService::open(&path).unwrap();
        assert_eq!(service.engine().delivery(&delivery_ref()), Some(&staged));
        let mut run = service.engine().run(&run_id()).unwrap().clone();
        let mut operation = service.engine().operation(&run_operation_id()).unwrap().clone();
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 15;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 15;
        operation.dispatched_at_unix_ms = Some(15);
        let mut spec = spawn_spec(RAW_DELIVERY_CANARY);
        spec.overrides.bundle_id = SpawnOverride::Set {
            value: SpawnBundleId::new("bundle.review-kit").unwrap(),
        };
        let mut context = dispatch_context(
            &operation,
            c2::spawn_spec_fingerprint(&spec).unwrap(),
        );
        context.dispatched_at_unix_ms = 15;
        let mut inherited = spec.clone();
        inherited.overrides.bundle_id = SpawnOverride::Inherit;
        let mut inherited_context = dispatch_context(
            &operation,
            c2::spawn_spec_fingerprint(&inherited).unwrap(),
        );
        inherited_context.dispatched_at_unix_ms = 15;
        let before = service.committed_snapshot();
        assert!(matches!(
            service.begin_run_dispatch(
                HarnessRevision::new(1).unwrap(),
                run.clone(),
                HarnessRevision::new(1).unwrap(),
                operation.clone(),
                inherited_context,
                &inherited,
            ),
            Err(HarnessServiceError::InvalidDispatchContext(_)),
        ));
        assert_eq!(service.committed_snapshot(), before);
        service.begin_run_dispatch(
            HarnessRevision::new(1).unwrap(),
            run,
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
        ).unwrap();
        service.close().unwrap();

        let service = HarnessService::open(&path).unwrap();
        assert_eq!(
            service.engine().delivery(&delivery_ref()).unwrap().state,
            HarnessDeliveryStateV1::Staged,
        );
        assert!(service.dispatch_context(&run_operation_id()).is_some());
        service.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            if let Ok(bytes) = fs::read(candidate) {
                assert!(!String::from_utf8_lossy(&bytes).contains(RAW_DELIVERY_CANARY));
            }
        }
        remove_database(&path);
    }

    #[test]
    fn harness_service_prevalidates_bounds_before_digest_serialization() {
        let mut mutation = task_mutation();
        if let HarnessMutationV1::CreateTask { task, .. } = &mut mutation {
            task.title = "x".repeat(
                gate4agent_harness_protocol::HARNESS_TITLE_MAX_BYTES + 1,
            );
        }
        assert!(matches!(
            mutation_request_digest(&mutation),
            Err(HarnessServiceError::Engine(HarnessEngineError::Validation(
                gate4agent_harness_protocol::HarnessValidationError::InvalidTitle
            )))
        ));
    }

    #[test]
    fn exporting_continuation_reopen_closes_lost_waiter_without_reexport_authority() {
        let path = database_path("continuation-exporting-recovery");
        let (engine, dispatch_contexts, continuation) = continuation_fixture(true, true);
        let continuation_ref = continuation.continuation_ref.clone();
        {
            let mut service = HarnessService {
                store: Some(HarnessStore::open(&path).unwrap()),
                engine,
                dispatch_contexts,
                harness_mcp_reservations: BTreeMap::new(),
                operator_requests: BTreeMap::new(),
                scheduled_launches: BTreeMap::new(),
                issued_launches: BTreeMap::new(),
                poisoned: false,
            };
            assert_eq!(
                service.prepare_continuation(continuation).unwrap(),
                HarnessApplyOutcome::Applied,
            );
            let prepared = service.begin_continuation_export(
                &continuation_ref,
                HarnessRevision::new(1).unwrap(),
                21,
            ).unwrap();
            assert_eq!(
                prepared.continuation().state,
                HarnessContinuationStateV1::Exporting,
            );
            service.close().unwrap();
        }

        let mut reopened = HarnessService::open(&path).unwrap();
        assert_eq!(
            reopened.recover_exporting_continuation_outcome_unknown(
                &continuation_ref,
                HarnessRevision::new(2).unwrap(),
                22,
            ).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        reopened.close().unwrap();

        let mut replayed = HarnessService::open(&path).unwrap();
        let recovered = replayed.continuation(&continuation_ref).unwrap();
        assert_eq!(recovered.state, HarnessContinuationStateV1::OutcomeUnknown);
        assert_eq!(
            recovered.outcome_unknown_reason,
            Some(HarnessContinuationOutcomeUnknownReasonV1::Transport),
        );
        assert_eq!(
            replayed.recover_exporting_continuation_outcome_unknown(
                &continuation_ref,
                HarnessRevision::new(2).unwrap(),
                22,
            ).unwrap(),
            HarnessApplyOutcome::Replayed,
        );
        assert!(replayed.recover_exporting_continuation_outcome_unknown(
            &continuation_ref,
            HarnessRevision::new(2).unwrap(),
            23,
        ).is_err());
        replayed.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn same_node_incarnation_cross_workspace_continuation_dispatch_is_exact() {
        let (engine, _, mut continuation) = continuation_fixture(true, true);
        let source_workspace = continuation.workspace_id.clone();
        let target_workspace = HarnessSelectorV1::new("workspace-b").unwrap();
        let context_receipt = HarnessResolvedContextPackReceiptV1 {
            id: HarnessSelectorV1::new("context-cross-workspace").unwrap(),
            digest: format!("sha256:{}", "a".repeat(64)),
            lineage: HarnessContextPackLineageV1 {
                source_node_id: continuation.node_id.clone(),
                source_workspace_id: source_workspace.clone(),
                source_instance_id: 7,
                source_generation: 1,
                source_provider: continuation.source_provider.clone(),
            },
            source_message_count: 3,
            retained_message_count: 3,
            byte_len: 100,
            truncated: false,
        };
        continuation.revision = HarnessRevision::new(3).unwrap();
        continuation.state = HarnessContinuationStateV1::Exported;
        continuation.context = Some(context_receipt);
        continuation.exporting_at_unix_ms = Some(21);
        continuation.exported_at_unix_ms = Some(22);
        continuation.updated_at_unix_ms = 22;

        let mut checkpoint = engine.checkpoint();
        let target_run = checkpoint.runs.iter_mut()
            .find(|run| run.run_id == run_id()).unwrap();
        target_run.intent.workspace_id = target_workspace.clone();
        target_run.continuation_receipt = Some(continuation.receipt_ref.clone());
        checkpoint.grants[0].allowed_targets[0].workspace_id = target_workspace.clone();
        checkpoint.continuations = vec![continuation];
        let engine = HarnessEngine::restore(checkpoint).unwrap();
        let run = engine.run(&run_id()).unwrap().clone();
        let mut spec = spawn_spec("same Node cross-workspace continuation");
        spec.target.workspace_id = WorkspaceId::new(target_workspace.as_str()).unwrap();
        spec.overrides.context_id = SpawnOverride::Set {
            value: SpawnContextId::new("context-cross-workspace").unwrap(),
        };
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        let context = HarnessDispatchContextV1 {
            operation_id: run_operation_id(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessSelectorV1::new("07".repeat(16)).unwrap(),
            workspace_id: target_workspace,
            provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
            expected_provider: HarnessSelectorV1::new("claude").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            baseline_record_ids: Vec::new(),
            spawn_spec_fingerprint: fingerprint.clone(),
            dispatched_at_unix_ms: 25,
            idempotency_ref: engine.operation(&run_operation_id()).unwrap()
                .idempotency_ref.clone(),
            managed_worktree_binding: None,
        };
        assert_ne!(source_workspace, context.workspace_id);
        assert!(validate_run_dispatch_seam(
            &engine,
            &BTreeMap::new(),
            &run,
            &context,
            &spec,
            &fingerprint,
        ).is_ok());

        for (node_id, incarnation) in [
            ("node-b", "07".repeat(16)),
            ("node-a", "08".repeat(16)),
        ] {
            let mut invalid_context = context.clone();
            invalid_context.node_id = HarnessSelectorV1::new(node_id).unwrap();
            invalid_context.node_incarnation_id =
                HarnessSelectorV1::new(incarnation).unwrap();
            assert!(validate_run_dispatch_seam(
                &engine,
                &BTreeMap::new(),
                &run,
                &invalid_context,
                &spec,
                &fingerprint,
            ).is_err());
        }
    }

    #[test]
    fn schedule_next_exact_replay_survives_state_evolution_and_rejects_changed_authority_or_plan() {
        let path = database_path("specialized-schedule-next-admission");
        let (engine, _, _) = continuation_fixture(true, true);
        let mut checkpoint = engine.checkpoint();
        let source_run_id = checkpoint.grants[0].actor_run_id.clone();
        checkpoint.runs.retain(|run| run.run_id == source_run_id);
        checkpoint.operations.retain(|operation| {
            operation.run_id.as_ref() == Some(&source_run_id)
        });
        checkpoint.tasks[0].run_ids = vec![source_run_id.clone()];

        let ready_task_id = HarnessTaskId::new(format!(
            "htask_{}",
            "f".repeat(24),
        )).unwrap();
        checkpoint.tasks.push(HarnessTaskV1 {
            task_id: ready_task_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "Specialized scheduled task".to_owned(),
            body: "Use the exact grant-bound launch plan".to_owned(),
            creator: HarnessActorV1::ParentRun { run_id: source_run_id.clone() },
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Ready,
            run_ids: Vec::new(),
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20,
        });
        checkpoint.operations.push(HarnessOperationV1 {
            operation_id: HarnessOperationId::new(format!(
                "hop_{}",
                "f".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::ParentRun { run_id: source_run_id },
            kind: HarnessOperationKindV1::CreateTask,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(ready_task_id),
            run_id: None,
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: None,
            request_digest: HarnessRequestDigest::new("f".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "f".repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20,
            dispatched_at_unix_ms: Some(20),
            finished_at_unix_ms: Some(20),
        });
        let plan = exact_launch_plan(false, false, true);
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine: HarnessEngine::restore(checkpoint).unwrap(),
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };

        let exact_authority = operator_authority('7', 30);
        let first = service.schedule_next(
            &catalog,
            exact_authority.clone(),
            Some(&plan.plan_id),
        ).unwrap();
        let HarnessScheduleOutcomeV1::Dispatch(intent) = &first else {
            panic!("specialized ready task must produce dispatch");
        };
        assert_eq!(
            service.scheduled_launch(&intent.operation_id),
            Some(&plan.scheduled_ref().unwrap()),
        );
        assert_eq!(
            service.schedule_next(
                &catalog,
                exact_authority.clone(),
                Some(&plan.plan_id),
            ).unwrap(),
            first,
        );
        assert_eq!(
            service.schedule_next(
                &catalog,
                operator_authority('8', 31),
                Some(&plan.plan_id),
            ).unwrap(),
            first,
        );

        let mut dispatching = service.engine().checkpoint();
        let run = dispatching.runs.iter_mut()
            .find(|run| run.run_id == intent.run_id).unwrap();
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 31;
        let operation = dispatching.operations.iter_mut()
            .find(|operation| operation.operation_id == intent.operation_id).unwrap();
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 31;
        operation.dispatched_at_unix_ms = Some(31);
        service.engine = HarnessEngine::restore(dispatching).unwrap();
        assert_eq!(
            service.schedule_next(
                &catalog,
                exact_authority.clone(),
                Some(&plan.plan_id),
            ).unwrap(),
            first,
        );

        let mut succeeded = service.engine().checkpoint();
        let run = succeeded.runs.iter_mut()
            .find(|run| run.run_id == intent.run_id).unwrap();
        run.revision = HarnessRevision::new(3).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Running;
        run.binding = Some(HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("incarnation-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-scheduled").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 7,
                    generation: 1,
                }),
            },
        });
        run.updated_at_unix_ms = 32;
        let operation = succeeded.operations.iter_mut()
            .find(|operation| operation.operation_id == intent.operation_id).unwrap();
        operation.revision = HarnessRevision::new(3).unwrap();
        operation.state = HarnessOperationStateV1::Succeeded;
        operation.updated_at_unix_ms = 32;
        operation.finished_at_unix_ms = Some(32);
        service.engine = HarnessEngine::restore(succeeded).unwrap();
        assert_eq!(
            service.schedule_next(
                &catalog,
                exact_authority.clone(),
                Some(&plan.plan_id),
            ).unwrap(),
            first,
        );

        let mut changed_authority = exact_authority.clone();
        changed_authority.now_unix_ms = 33;
        assert!(matches!(
            service.schedule_next(&catalog, changed_authority, Some(&plan.plan_id)),
            Err(HarnessServiceError::OperatorRequestConflict { .. }),
        ));
        let mut changed_plan = plan.clone();
        changed_plan.revision = HarnessRevision::new(2).unwrap();
        let changed_catalog = HarnessLaunchCatalog::new([changed_plan]).unwrap();
        assert!(matches!(
            service.schedule_next(
                &changed_catalog,
                exact_authority.clone(),
                Some(&plan.plan_id),
            ),
            Err(HarnessServiceError::OperatorRequestConflict { .. }),
        ));

        let mut invalid = operator_authority('9', 32);
        invalid.now_unix_ms = 0;
        assert!(matches!(
            service.schedule_next(&catalog, invalid, Some(&plan.plan_id)),
            Err(HarnessServiceError::Validation(
                gate4agent_harness_protocol::HarnessValidationError::InvalidTimestamps,
            )),
        ));
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn specialized_authority_replay_preserves_evolved_continuation_exactly() {
        let path = database_path("specialized-authority-evolved-replay");
        let (engine, dispatch_contexts, _) = continuation_fixture(true, true);
        let plan = exact_launch_plan(false, true, false);
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let mut scheduled_launches = BTreeMap::new();
        scheduled_launches.insert(
            run_operation_id(),
            plan.scheduled_ref().unwrap(),
        );
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts,
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches,
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        assert_eq!(
            service.prepare_scheduled_specialized_authorities(
                &catalog,
                &DeliveryCatalogV2::default(),
                &run_operation_id(),
                21,
            ).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        let prepared = service.engine().continuation_for_run(&run_id())
            .unwrap().clone();
        service.begin_continuation_export(
            &prepared.continuation_ref,
            prepared.revision,
            22,
        ).unwrap();
        let evolved = service.engine().continuation_for_run(&run_id())
            .unwrap().clone();
        assert_eq!(evolved.state, HarnessContinuationStateV1::Exporting);
        assert_eq!(
            service.prepare_scheduled_specialized_authorities(
                &catalog,
                &DeliveryCatalogV2::default(),
                &run_operation_id(),
                99,
            ).unwrap(),
            HarnessApplyOutcome::Replayed,
        );
        assert_eq!(
            service.engine().continuation_for_run(&run_id()),
            Some(&evolved),
        );
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn specialized_staged_delivery_issues_exact_spawn_lease() {
        let path = database_path("specialized-staged-delivery-spawn-lease");
        let (engine, dispatch_contexts, _) = continuation_fixture(false, false);
        let plan = exact_launch_plan(true, false, false);
        let catalog = HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let ids = dispatch::deterministic_dispatch_ids(&run_operation_id(), &plan).unwrap();
        let mut checkpoint = engine.checkpoint();
        let source_run_id = checkpoint.runs.iter()
            .find(|run| run.run_id != run_id()).unwrap().run_id.clone();
        let target_task_id = HarnessTaskId::new(format!("htask_{}", "f".repeat(24))).unwrap();
        let source_task = checkpoint.tasks.iter_mut()
            .find(|task| task.task_id == task_id()).unwrap();
        source_task.run_ids.retain(|linked| linked != &run_id());
        let mut target_task = source_task.clone();
        target_task.task_id = target_task_id.clone();
        target_task.creator = HarnessActorV1::ParentRun { run_id: source_run_id.clone() };
        target_task.parent_task_id = Some(task_id());
        target_task.state = HarnessTaskStateV1::Running;
        target_task.run_ids = vec![run_id()];
        checkpoint.tasks.push(target_task);
        let target_run = checkpoint.runs.iter_mut()
            .find(|run| run.run_id == run_id()).unwrap();
        target_run.task_id = target_task_id.clone();
        target_run.intent.delivery_bundle = Some(HarnessSelectorV1::new("review-kit").unwrap());
        target_run.intent.continuation = None;
        checkpoint.operations.iter_mut()
            .find(|operation| operation.operation_id == run_operation_id()).unwrap()
            .task_id = Some(target_task_id.clone());
        let grant = checkpoint.grants.iter_mut().next().unwrap();
        grant.allowed_delivery_bundles = vec![HarnessSelectorV1::new("review-kit").unwrap()];
        let bundle = delivery_bundle();
        checkpoint.deliveries.push(HarnessDeliveryV1 {
            delivery_ref: ids.delivery_ref.clone().unwrap(),
            revision: HarnessRevision::new(2).unwrap(),
            authority: HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id: grant.grant_id.clone(),
                revision: grant.revision,
            },
            task_id: target_task_id.clone(),
            run_id: run_id(),
            operation_id: run_operation_id(),
            bundle: bundle.clone(),
            state: HarnessDeliveryStateV1::Staged,
            stage_receipt: Some(HarnessDeliveryStageReceiptV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                node_incarnation: HarnessSelectorV1::new("07070707070707070707070707070707")
                    .unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                bundle,
                staged_at_unix_ms: 21,
            }),
            receipt: None,
            created_at_unix_ms: 20,
            updated_at_unix_ms: 21,
        });
        let engine = HarnessEngine::restore(checkpoint).unwrap();
        let mut scheduled_launches = BTreeMap::new();
        scheduled_launches.insert(run_operation_id(), plan.scheduled_ref().unwrap());
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts,
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches,
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        let current_run = service.engine().run(&run_id()).unwrap().clone();
        let current_operation = service.engine().operation(&run_operation_id()).unwrap().clone();
        let current_task = service.engine().task(&target_task_id).unwrap().clone();
        let intent = dispatch_intent(&current_task, &current_run, &current_operation).unwrap();
        let mut spec = plan.spawn_spec(
            &intent,
            &current_task,
            gate4agent_node_protocol::SpawnProfileRevision::new("r1").unwrap(),
        ).unwrap();
        spec.overrides.bundle_id = SpawnOverride::Set {
            value: SpawnBundleId::new("bundle.review-kit").unwrap(),
        };
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        let context = HarnessDispatchContextV1 {
            operation_id: current_operation.operation_id.clone(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessSelectorV1::new(
                "07070707070707070707070707070707",
            ).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
            expected_provider: HarnessSelectorV1::new("claude").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            baseline_record_ids: Vec::new(),
            spawn_spec_fingerprint: fingerprint,
            dispatched_at_unix_ms: 22,
            idempotency_ref: current_operation.idempotency_ref.clone(),
            managed_worktree_binding: None,
        };
        let mut dispatching_run = current_run.clone();
        dispatching_run.revision = HarnessRevision::new(2).unwrap();
        dispatching_run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        dispatching_run.updated_at_unix_ms = 22;
        let mut dispatching_operation = current_operation.clone();
        dispatching_operation.revision = HarnessRevision::new(2).unwrap();
        dispatching_operation.state = HarnessOperationStateV1::Dispatching;
        dispatching_operation.updated_at_unix_ms = 22;
        dispatching_operation.dispatched_at_unix_ms = Some(22);

        let _lease = service.issue_spawn_lease(
            &catalog,
            current_run.revision,
            dispatching_run,
            current_operation.revision,
            dispatching_operation,
            context,
            spec,
        ).unwrap();
        assert_eq!(
            service.engine().run(&run_id()).unwrap().lifecycle,
            HarnessRunLifecycleV1::Dispatching,
        );
        assert_eq!(
            service.engine().operation(&run_operation_id()).unwrap().state,
            HarnessOperationStateV1::Dispatching,
        );
        assert_eq!(
            service.engine().delivery_for_run(&run_id()).unwrap().state,
            HarnessDeliveryStateV1::Staged,
        );
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn scheduled_pre_dispatch_failure_is_atomic_reopen_and_replay_exact() {
        let path = database_path("scheduled-pre-dispatch-failure");
        let (failed_run, failed_operation, failed_task) = {
            let mut service = HarnessService::open(&path).unwrap();
            apply_task(&mut service);
            let (mut run, mut operation) = create_run(&mut service);
            let mut checkpoint = service.engine().checkpoint();
            checkpoint.tasks[0].state = HarnessTaskStateV1::Running;
            service.engine = HarnessEngine::restore(checkpoint).unwrap();
            let mut task = service.engine().task(&task_id()).unwrap().clone();
            let failure = gate4agent_harness_protocol::HarnessFailureV1 {
                category: gate4agent_harness_protocol::HarnessFailureCategoryV1::Validation,
                retryable: false,
            };
            run.revision = next_harness_revision(run.revision, "run").unwrap();
            run.lifecycle = HarnessRunLifecycleV1::Failed;
            run.result_disposition = Some(
                gate4agent_harness_protocol::HarnessResultDispositionV1::Failed,
            );
            run.failure = Some(failure.clone());
            run.updated_at_unix_ms = 13;
            operation.revision = next_harness_revision(operation.revision, "operation").unwrap();
            operation.state = HarnessOperationStateV1::Failed;
            operation.failure = Some(failure);
            operation.updated_at_unix_ms = 13;
            operation.finished_at_unix_ms = Some(13);
            task.revision = next_harness_revision(task.revision, "task").unwrap();
            task.state = HarnessTaskStateV1::Failed;
            task.updated_at_unix_ms = 13;
            let before = service.committed_snapshot();
            assert_eq!(service.commit_scheduled_pre_dispatch_outcome(
                HarnessRevision::new(1).unwrap(),
                run.clone(),
                HarnessRevision::new(1).unwrap(),
                operation.clone(),
                HarnessRevision::new(2).unwrap(),
                task.clone(),
            ).unwrap(), None);
            assert_ne!(service.committed_snapshot(), before);
            service.close().unwrap();
            (run, operation, task)
        };
        let mut reopened = HarnessService::open(&path).unwrap();
        assert_eq!(reopened.engine().run(&run_id()), Some(&failed_run));
        assert_eq!(reopened.engine().operation(&run_operation_id()), Some(&failed_operation));
        assert_eq!(reopened.engine().task(&task_id()), Some(&failed_task));
        assert_eq!(reopened.commit_scheduled_pre_dispatch_outcome(
            HarnessRevision::new(1).unwrap(),
            failed_run,
            HarnessRevision::new(1).unwrap(),
            failed_operation,
            HarnessRevision::new(2).unwrap(),
            failed_task,
        ).unwrap(), None);
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn h3b_continuation_accept_is_atomic_reopen_replay_and_cross_proof_safe() {
        let path = database_path("h3b-continuation-accept");
        let (engine, dispatch_contexts, mut continuation) = continuation_fixture(true, true);
        let continuation_ref = continuation.continuation_ref.clone();
        let receipt_ref = continuation.receipt_ref.clone();
        let context_receipt = HarnessResolvedContextPackReceiptV1 {
            id: HarnessSelectorV1::new("context-a").unwrap(),
            digest: format!("sha256:{}", "a".repeat(64)),
            lineage: HarnessContextPackLineageV1 {
                source_node_id: HarnessSelectorV1::new("node-a").unwrap(),
                source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                source_instance_id: 7,
                source_generation: 1,
                source_provider: HarnessSelectorV1::new("claude").unwrap(),
            },
            source_message_count: 3,
            retained_message_count: 3,
            byte_len: 100,
            truncated: false,
        };
        continuation.revision = HarnessRevision::new(3).unwrap();
        continuation.state = HarnessContinuationStateV1::Exported;
        continuation.context = Some(context_receipt.clone());
        continuation.exporting_at_unix_ms = Some(21);
        continuation.exported_at_unix_ms = Some(22);
        continuation.updated_at_unix_ms = 22;
        let mut checkpoint = engine.checkpoint();
        let target_run = checkpoint.runs.iter_mut()
            .find(|run| run.run_id == run_id()).unwrap();
        target_run.continuation_receipt = Some(receipt_ref);
        checkpoint.continuations = vec![continuation];
        let engine = HarnessEngine::restore(checkpoint).unwrap();
        let mut spec = spawn_spec("H3B continuation accept");
        spec.overrides.terminal_size = SpawnOverride::Set {
            value: TerminalSize { rows: 40, columns: 120 },
        };
        spec.overrides.context_id = SpawnOverride::Set {
            value: SpawnContextId::new("context-a").unwrap(),
        };
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        let mut run = engine.run(&run_id()).unwrap().clone();
        let mut operation = engine.operation(&run_operation_id()).unwrap().clone();
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 25;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 25;
        operation.dispatched_at_unix_ms = Some(25);
        let incarnation = "07".repeat(16);
        let context = HarnessDispatchContextV1 {
            operation_id: run_operation_id(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
            expected_provider: HarnessSelectorV1::new("claude").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            baseline_record_ids: Vec::new(),
            spawn_spec_fingerprint: fingerprint.clone(),
            dispatched_at_unix_ms: 25,
            idempotency_ref: operation.idempotency_ref.clone(),
            managed_worktree_binding: None,
        };
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}", "d".repeat(24),
        )).unwrap();
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts,
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        service.begin_run_dispatch_with_harness_mcp(
            HarnessRevision::new(1).unwrap(), run, HarnessRevision::new(1).unwrap(),
            operation, context, &spec, reservation_id.clone(), grant_id,
            HarnessRevision::new(1).unwrap(), 1_000,
        ).unwrap();
        let route = gate4agent_c2_protocol::NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let prepared_record = service.harness_mcp_reservation(&reservation_id).unwrap().clone();
        service.record_harness_mcp_armed(
            c2::armed_harness_mcp_reservation_proof_for_test(route, prepared_record),
            26,
        ).unwrap();
        let armed = service.harness_mcp_reservation(&reservation_id).unwrap().clone();
        let target_session = SessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(8),
                generation: SessionGeneration(1),
            },
        };
        let binding = HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-target").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 8,
                    generation: 1,
                }),
            },
        };
        let mut running = service.engine().run(&run_id()).unwrap().clone();
        running.revision = HarnessRevision::new(3).unwrap();
        running.lifecycle = HarnessRunLifecycleV1::Running;
        running.binding = Some(binding);
        running.updated_at_unix_ms = 30;
        let mut succeeded = service.engine().operation(&run_operation_id()).unwrap().clone();
        succeeded.revision = HarnessRevision::new(3).unwrap();
        succeeded.state = HarnessOperationStateV1::Succeeded;
        succeeded.updated_at_unix_ms = 30;
        succeeded.finished_at_unix_ms = Some(30);
        let node_context = c2::harness_context_to_node(&context_receipt).unwrap();
        let proof = c2::accepted_harness_mcp_spawn_binding_proof_for_test(
            run_operation_id(), fingerprint.clone(), succeeded.idempotency_ref.clone(),
            NodeId::new("node-a").unwrap(), NodeIncarnationId::from_bytes([7; 16]),
            WorkspaceId::new("workspace-a").unwrap(), AgentId::new("claude").unwrap(),
            SessionMode::Pty, SessionRecordId::new("record-target").unwrap(),
            target_session.clone(), None, Some(node_context.clone()), armed.proxy_receipt(),
        );
        service.transition_run_with_accepted_harness_mcp_spawn_and_continuation(
            HarnessRevision::new(2).unwrap(), running.clone(),
            HarnessRevision::new(2).unwrap(), succeeded.clone(), &continuation_ref,
            HarnessRevision::new(3).unwrap(), &proof, 30,
        ).unwrap();
        service.transition_run_with_accepted_harness_mcp_spawn_and_continuation(
            HarnessRevision::new(2).unwrap(), running.clone(),
            HarnessRevision::new(2).unwrap(), succeeded.clone(), &continuation_ref,
            HarnessRevision::new(3).unwrap(), &proof, 30,
        ).unwrap();
        let persisted_reservation_id = reservation_id.clone();
        let wrong_proxy = ResolvedHarnessMcpProxyReceiptV1 {
            reservation_id,
            activation_digest: HarnessMcpActivationDigest::new(format!(
                "sha256:{}", "f".repeat(64),
            )).unwrap(),
        };
        let cross = c2::accepted_harness_mcp_spawn_binding_proof_for_test(
            run_operation_id(), fingerprint, succeeded.idempotency_ref.clone(),
            NodeId::new("node-a").unwrap(), NodeIncarnationId::from_bytes([7; 16]),
            WorkspaceId::new("workspace-a").unwrap(), AgentId::new("claude").unwrap(),
            SessionMode::Pty, SessionRecordId::new("record-target").unwrap(),
            target_session, None, Some(node_context), wrong_proxy,
        );
        assert!(service.transition_run_with_accepted_harness_mcp_spawn_and_continuation(
            HarnessRevision::new(2).unwrap(), running, HarnessRevision::new(2).unwrap(),
            succeeded, &continuation_ref, HarnessRevision::new(3).unwrap(), &cross, 30,
        ).is_err());
        service.close().unwrap();
        let reopened = HarnessService::open(&path).unwrap();
        assert_eq!(reopened.continuation(&continuation_ref).unwrap().state,
            HarnessContinuationStateV1::Bound);
        assert_eq!(reopened.harness_mcp_reservation_state(&persisted_reservation_id),
            Some(HarnessMcpReservationStateV1::Bound));
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn specialized_armed_spawn_lease_is_single_use_across_reopen() {
        let path = database_path("specialized-armed-lease");
        let plan = exact_launch_plan(false, false, true);
        let catalog = dispatch::HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let scheduled = plan.scheduled_ref().unwrap();
        let mut spec = spawn_spec("single-use armed lease");
        spec.overrides.terminal_size = SpawnOverride::Set {
            value: TerminalSize { rows: 40, columns: 120 },
        };
        let engine = h3b_dispatch_engine();
        let mut run = engine.run(&run_id()).unwrap().clone();
        let mut operation = engine.operation(&run_operation_id()).unwrap().clone();
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 13;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 13;
        operation.dispatched_at_unix_ms = Some(13);
        let mut context = dispatch_context(
            &operation,
            c2::spawn_spec_fingerprint(&spec).unwrap(),
        );
        let incarnation = "07".repeat(16);
        context.node_incarnation_id = HarnessSelectorV1::new(&incarnation).unwrap();
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "d".repeat(24),
        )).unwrap();
        let grant_id = SessionGrantId::new(format!(
            "hgrant_{}",
            "c".repeat(24),
        )).unwrap();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::from([(run_operation_id(), scheduled)]),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        service.begin_run_dispatch_with_harness_mcp(
            HarnessRevision::new(1).unwrap(),
            run,
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
            reservation_id.clone(),
            grant_id,
            HarnessRevision::new(1).unwrap(),
            1_000,
        ).unwrap();
        let route = gate4agent_c2_protocol::NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let prepared_record = service.harness_mcp_reservation(&reservation_id)
            .unwrap().clone();
        let proof = c2::armed_harness_mcp_reservation_proof_for_test(
            route.clone(),
            prepared_record.clone(),
        );
        let _lease = service.record_harness_mcp_armed_and_issue_spawn_lease(
            &catalog,
            proof,
            14,
            spec.clone(),
        ).unwrap();
        assert_eq!(
            service.harness_mcp_reservation_state(&reservation_id),
            Some(HarnessMcpReservationStateV1::Armed),
        );
        let replay_proof = c2::armed_harness_mcp_reservation_proof_for_test(
            route.clone(),
            prepared_record,
        );
        assert!(service.record_harness_mcp_armed_and_issue_spawn_lease(
            &catalog,
            replay_proof,
            14,
            spec.clone(),
        ).is_err());
        service.close().unwrap();

        let mut reopened = HarnessService::open(&path).unwrap();
        let armed_record = reopened.harness_mcp_reservation(&reservation_id)
            .unwrap().clone();
        let reopen_proof = c2::armed_harness_mcp_reservation_proof_for_test(
            route,
            armed_record,
        );
        assert!(reopened.record_harness_mcp_armed_and_issue_spawn_lease(
            &catalog,
            reopen_proof,
            15,
            spec,
        ).is_err());
        assert_eq!(
            reopened.harness_mcp_reservation_state(&reservation_id),
            Some(HarnessMcpReservationStateV1::Armed),
        );
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn h3b_dispatch_accepts_protocol_valid_create_run_without_operation_grant_link() {
        let path = database_path("h3b-explicit-grant");
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine: h3b_dispatch_engine(),
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        let mut run = service.engine().run(&run_id()).unwrap().clone();
        let mut operation = service.engine().operation(&run_operation_id()).unwrap().clone();
        assert_eq!(operation.grant_id, None);
        assert!(matches!(operation.actor, HarnessActorV1::ParentRun { .. }));
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 13;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 13;
        operation.dispatched_at_unix_ms = Some(13);
        let spec = spawn_spec("protocol-valid H3B dispatch");
        let context = dispatch_context(
            &operation,
            c2::spawn_spec_fingerprint(&spec).unwrap(),
        );
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "d".repeat(24),
        )).unwrap();
        let grant_id = SessionGrantId::new(format!(
            "hgrant_{}",
            "c".repeat(24),
        )).unwrap();
        let before = service.committed_snapshot();

        // A presented revision (2) that does not match the grant's durable
        // revision (1) is refused by `HarnessMcpGrantRevisionRefused`,
        // naming the grant and both compared revisions -- not folded into
        // a bare "not authoritative" message.
        match service.begin_run_dispatch_with_harness_mcp(
            HarnessRevision::new(1).unwrap(),
            run.clone(),
            HarnessRevision::new(1).unwrap(),
            operation.clone(),
            context.clone(),
            &spec,
            reservation_id.clone(),
            grant_id.clone(),
            HarnessRevision::new(2).unwrap(),
            1_000,
        ) {
            Err(HarnessServiceError::HarnessMcpGrantRevisionRefused {
                grant_id: refused_grant_id,
                durable_revision,
                presented_revision,
            }) => {
                assert_eq!(refused_grant_id, grant_id);
                assert_eq!(durable_revision, HarnessRevision::new(1).unwrap());
                assert_eq!(presented_revision, HarnessRevision::new(2).unwrap());
            }
            Err(other) => panic!("expected a named revision refusal, got {other:?}"),
            Ok(_) => panic!("expected a named revision refusal, but dispatch succeeded"),
        }
        assert_eq!(service.committed_snapshot(), before);
        assert_eq!(service.harness_mcp_reservation_state(&reservation_id), None);

        let prepared = service.begin_run_dispatch_with_harness_mcp(
            HarnessRevision::new(1).unwrap(),
            run,
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
            reservation_id.clone(),
            grant_id.clone(),
            HarnessRevision::new(1).unwrap(),
            1_000,
        ).unwrap();
        assert_eq!(prepared.reservation_id(), &reservation_id);
        assert_eq!(
            service.harness_mcp_reservation_state(&reservation_id),
            Some(HarnessMcpReservationStateV1::Prepared),
        );
        let reservation = service.harness_mcp_reservation(&reservation_id).unwrap();
        assert_eq!(reservation.grant_id, grant_id);
        assert_eq!(reservation.grant_revision, HarnessRevision::new(1).unwrap());

        service.close().unwrap();
        let reopened = HarnessService::open(&path).unwrap();
        let reservation = reopened.harness_mcp_reservation(&reservation_id).unwrap();
        assert_eq!(reservation.state, HarnessMcpReservationStateV1::Prepared);
        assert_eq!(reservation.grant_id, grant_id);
        assert_eq!(reservation.grant_revision, HarnessRevision::new(1).unwrap());
        reopened.close().unwrap();
        remove_database(&path);
    }

    /// H3B accepts a second dispatch shape beyond the original child/
    /// continuation `ParentRun` actor: an operator-started top-level run
    /// dispatching with its own harness-issued default grant (Slice A(i)'s
    /// `resolve_harness_mcp_grant` / `SessionGrantV1::default_for_run`).
    /// `validate_harness_mcp_grant` is exercised directly -- the full
    /// dispatch seam for this shape is covered separately by
    /// `h3b_dispatch_accepts_operator_started_top_level_run_with_default_grant`
    /// below.
    #[test]
    fn validate_harness_mcp_grant_accepts_operator_started_top_level_default_grant() {
        let engine = h3b_operator_dispatch_engine();
        let run = engine.run(&run_id()).unwrap().clone();
        let operation = engine.operation(&run_operation_id()).unwrap().clone();
        assert_eq!(run.parent_run_id, None);
        assert!(matches!(operation.actor, HarnessActorV1::User { .. }));
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let context = dispatch_context(
            &operation,
            HarnessRequestDigest::new("d".repeat(64)).unwrap(),
        );
        assert!(validate_harness_mcp_grant(
            &engine, &run, &operation, &context, &grant_id, HarnessRevision::new(1).unwrap(),
        ).is_ok());
    }

    /// H3B shape (a), the original child/continuation `ParentRun` actor,
    /// is unweakened by accepting shape (b) alongside it.
    #[test]
    fn validate_harness_mcp_grant_accepts_parent_run_child() {
        let engine = h3b_dispatch_engine();
        let run = engine.run(&run_id()).unwrap().clone();
        let operation = engine.operation(&run_operation_id()).unwrap().clone();
        assert!(run.parent_run_id.is_some());
        assert!(matches!(operation.actor, HarnessActorV1::ParentRun { .. }));
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let context = dispatch_context(
            &operation,
            HarnessRequestDigest::new("d".repeat(64)).unwrap(),
        );
        assert!(validate_harness_mcp_grant(
            &engine, &run, &operation, &context, &grant_id, HarnessRevision::new(1).unwrap(),
        ).is_ok());
    }

    /// A `User`-actor run whose named grant was minted for a DIFFERENT run
    /// is refused by name -- neither H3B shape recognizes it -- rather
    /// than the bare "operation actor is not a run" this replaces.
    #[test]
    fn validate_harness_mcp_grant_refuses_user_actor_with_foreign_grant_by_name() {
        let mut checkpoint = h3b_operator_dispatch_engine().checkpoint();
        // The grant's `actor_run_id` must reference a real run for the
        // checkpoint to restore at all (`HarnessEngine::validate_links`), so
        // the "foreign" run this grant was actually minted for has to exist
        // too -- a second, unrelated top-level run cloned off the fixture's
        // own, mirroring how `h3b_dispatch_engine` mints its parent run.
        let foreign_run_id = HarnessRunId::new(format!("hrun_{}", "f".repeat(24))).unwrap();
        let foreign_operation_id = HarnessOperationId::new(format!(
            "hop_{}",
            "f".repeat(24),
        )).unwrap();
        checkpoint.tasks[0].run_ids.push(foreign_run_id.clone());
        let mut foreign_run = checkpoint.runs[0].clone();
        foreign_run.run_id = foreign_run_id.clone();
        foreign_run.operation_id = foreign_operation_id.clone();
        checkpoint.runs.push(foreign_run);
        let mut foreign_operation = checkpoint.operations[0].clone();
        foreign_operation.operation_id = foreign_operation_id.clone();
        foreign_operation.run_id = Some(foreign_run_id.clone());
        foreign_operation.request_digest = HarnessRequestDigest::new("f".repeat(64)).unwrap();
        foreign_operation.idempotency_ref = HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            "f".repeat(24),
        )).unwrap();
        checkpoint.operations.push(foreign_operation);
        let foreign_grant_id = SessionGrantId::new(format!("hgrant_{}", "f".repeat(24))).unwrap();
        let mut foreign_grant = checkpoint.grants[0].clone();
        foreign_grant.grant_id = foreign_grant_id.clone();
        foreign_grant.actor_run_id = foreign_run_id.clone();
        checkpoint.grants.push(foreign_grant);
        let engine = HarnessEngine::restore(checkpoint).unwrap();
        let run = engine.run(&run_id()).unwrap().clone();
        let operation = engine.operation(&run_operation_id()).unwrap().clone();
        assert_eq!(run.parent_run_id, None);
        assert!(matches!(operation.actor, HarnessActorV1::User { .. }));
        let context = dispatch_context(
            &operation,
            HarnessRequestDigest::new("d".repeat(64)).unwrap(),
        );
        match validate_harness_mcp_grant(
            &engine, &run, &operation, &context, &foreign_grant_id,
            HarnessRevision::new(1).unwrap(),
        ) {
            Err(HarnessServiceError::HarnessMcpGrantActorRefused {
                actor_kind, parent_run_id, grant_actor_run_id,
            }) => {
                assert_eq!(actor_kind, "user");
                assert_eq!(parent_run_id, None);
                assert_eq!(grant_actor_run_id, foreign_run_id);
            }
            other => panic!("expected a named actor refusal, got {other:?}"),
        }
    }

    /// A recognized shape (b) whose presented revision does not match the
    /// grant's durable one is refused by `HarnessMcpGrantRevisionRefused`,
    /// naming the grant and both compared revisions -- not folded into the
    /// bare "not authoritative" message this replaces.
    #[test]
    fn validate_harness_mcp_grant_refuses_revision_mismatch_by_name() {
        let engine = h3b_operator_dispatch_engine();
        let run = engine.run(&run_id()).unwrap().clone();
        let operation = engine.operation(&run_operation_id()).unwrap().clone();
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let context = dispatch_context(
            &operation,
            HarnessRequestDigest::new("d".repeat(64)).unwrap(),
        );
        match validate_harness_mcp_grant(
            &engine, &run, &operation, &context, &grant_id, HarnessRevision::new(2).unwrap(),
        ) {
            Err(HarnessServiceError::HarnessMcpGrantRevisionRefused {
                grant_id: refused_grant_id, durable_revision, presented_revision,
            }) => {
                assert_eq!(refused_grant_id, grant_id);
                assert_eq!(durable_revision, HarnessRevision::new(1).unwrap());
                assert_eq!(presented_revision, HarnessRevision::new(2).unwrap());
            }
            other => panic!("expected a named revision refusal, got {other:?}"),
        }
    }

    /// A recognized shape (b) whose dispatch target is not among the
    /// grant's `allowed_targets` is refused by `HarnessMcpGrantTargetRefused`,
    /// naming the grant, the presented target, and the grant's own allowed
    /// targets.
    #[test]
    fn validate_harness_mcp_grant_refuses_target_mismatch_by_name() {
        let engine = h3b_operator_dispatch_engine();
        let run = engine.run(&run_id()).unwrap().clone();
        let operation = engine.operation(&run_operation_id()).unwrap().clone();
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let mut context = dispatch_context(
            &operation,
            HarnessRequestDigest::new("d".repeat(64)).unwrap(),
        );
        context.provider_profile = HarnessSelectorV1::new("other-profile").unwrap();
        match validate_harness_mcp_grant(
            &engine, &run, &operation, &context, &grant_id, HarnessRevision::new(1).unwrap(),
        ) {
            Err(HarnessServiceError::HarnessMcpGrantTargetRefused {
                grant_id: refused_grant_id, presented_target, allowed_targets,
            }) => {
                assert_eq!(refused_grant_id, grant_id);
                assert_eq!(presented_target.provider_profile.as_str(), "other-profile");
                assert_eq!(allowed_targets.len(), 1);
                assert_eq!(allowed_targets[0].provider_profile.as_str(), "claude-default");
            }
            other => panic!("expected a named target refusal, got {other:?}"),
        }
    }

    /// H3B shape (a)'s match guard only checks that the operation actor
    /// names the run's actual parent -- it does not itself check the
    /// grant's own `actor_run_id`, so a grant issued to some OTHER run
    /// (not the parent the operation actor names) still reaches the
    /// generic authority proof, where it is refused by name
    /// (`HarnessMcpGrantLinkRefused`) rather than silently accepted or
    /// folded into a bare message.
    #[test]
    fn validate_harness_mcp_grant_refuses_actor_link_mismatch_by_name() {
        let mut checkpoint = h3b_dispatch_engine().checkpoint();
        let foreign_actor_run_id = run_id();
        checkpoint.grants[0].actor_run_id = foreign_actor_run_id.clone();
        let engine = HarnessEngine::restore(checkpoint).unwrap();
        let run = engine.run(&run_id()).unwrap().clone();
        let operation = engine.operation(&run_operation_id()).unwrap().clone();
        assert!(run.parent_run_id.is_some());
        assert!(matches!(operation.actor, HarnessActorV1::ParentRun { .. }));
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let context = dispatch_context(
            &operation,
            HarnessRequestDigest::new("d".repeat(64)).unwrap(),
        );
        match validate_harness_mcp_grant(
            &engine, &run, &operation, &context, &grant_id, HarnessRevision::new(1).unwrap(),
        ) {
            Err(HarnessServiceError::HarnessMcpGrantLinkRefused {
                grant_id: refused_grant_id, grant_actor_run_id, dispatch_actor_run_id,
            }) => {
                assert_eq!(refused_grant_id, grant_id);
                assert_eq!(grant_actor_run_id, foreign_actor_run_id);
                assert_eq!(dispatch_actor_run_id, run.parent_run_id.unwrap());
            }
            other => panic!("expected a named link refusal, got {other:?}"),
        }
    }

    /// The full `begin_run_dispatch_with_harness_mcp` seam for H3B shape
    /// (b): the resulting reservation's `actor_run_id` is the run itself
    /// (equal to the grant it was validated against), not the parent-run
    /// id the original H3B slice assumed every dispatch would carry.
    #[test]
    fn h3b_dispatch_accepts_operator_started_top_level_run_with_default_grant() {
        let path = database_path("h3b-operator-top-level");
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine: h3b_operator_dispatch_engine(),
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        let mut run = service.engine().run(&run_id()).unwrap().clone();
        let mut operation = service.engine().operation(&run_operation_id()).unwrap().clone();
        assert_eq!(operation.grant_id, None);
        assert!(matches!(operation.actor, HarnessActorV1::User { .. }));
        assert_eq!(run.parent_run_id, None);
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 13;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 13;
        operation.dispatched_at_unix_ms = Some(13);
        let spec = spawn_spec("operator top-level H3B dispatch");
        let context = dispatch_context(
            &operation,
            c2::spawn_spec_fingerprint(&spec).unwrap(),
        );
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "d".repeat(24),
        )).unwrap();
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();

        let prepared = service.begin_run_dispatch_with_harness_mcp(
            HarnessRevision::new(1).unwrap(),
            run.clone(),
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
            reservation_id.clone(),
            grant_id.clone(),
            HarnessRevision::new(1).unwrap(),
            1_000,
        ).unwrap();
        assert_eq!(prepared.reservation_id(), &reservation_id);
        let reservation = service.harness_mcp_reservation(&reservation_id).unwrap();
        assert_eq!(reservation.grant_id, grant_id);
        assert_eq!(reservation.actor_run_id, run.run_id);

        service.close().unwrap();
        remove_database(&path);
    }

    /// The live dispatch coordinator composes `resolve_harness_mcp_grant`
    /// (which mints the run's default grant through a real `CreateGrant`
    /// mutation) directly into `begin_run_dispatch_with_harness_mcp` (which
    /// validates it) -- but neither
    /// `resolve_harness_mcp_grant_mints_default_grant_when_policy_is_operator`
    /// (stops at the mint, in `runtime.rs`) nor
    /// `h3b_dispatch_accepts_operator_started_top_level_run_with_default_grant`
    /// above (validates a hand-seeded grant that was never minted) exercises
    /// that composition. This drives it end to end, up to (not including)
    /// `ArmHarnessMcpReservation`, the way `start_or_resume_dispatch_job`'s
    /// `GrantBound` arm actually does.
    #[test]
    fn h3b_dispatch_accepts_a_grant_minted_by_resolve_harness_mcp_grant() {
        let path = database_path("h3b-derived-default-grant");
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine: h3b_operator_dispatch_engine(),
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        let run = service.engine().run(&run_id()).unwrap().clone();
        let operation = service.engine().operation(&run_operation_id()).unwrap().clone();
        assert_eq!(run.parent_run_id, None);
        assert!(matches!(operation.actor, HarnessActorV1::User { .. }));

        let spec = spawn_spec("derived default grant H3B dispatch");
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        let context = dispatch_context(&operation, fingerprint);

        let (grant_id, grant_revision) = crate::runtime::resolve_harness_mcp_grant(
            &mut service,
            &operation.operation_id,
            &crate::dispatch::HarnessGrantPolicyV1::Operator,
            &run.run_id,
            HarnessGrantTargetV1 {
                node_id: context.node_id.clone(),
                workspace_id: context.workspace_id.clone(),
                provider_profile: context.provider_profile.clone(),
                mode: context.mode,
            },
            13,
        ).unwrap();
        assert_eq!(grant_revision, HarnessRevision::new(1).unwrap());

        let mut run = run;
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 13;
        let mut operation = operation;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 13;
        operation.dispatched_at_unix_ms = Some(13);

        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "d".repeat(24),
        )).unwrap();

        let prepared = service.begin_run_dispatch_with_harness_mcp(
            HarnessRevision::new(1).unwrap(),
            run.clone(),
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
            reservation_id.clone(),
            grant_id.clone(),
            grant_revision,
            1_000,
        ).unwrap();
        assert_eq!(prepared.reservation_id(), &reservation_id);
        let reservation = service.harness_mcp_reservation(&reservation_id).unwrap();
        assert_eq!(reservation.grant_id, grant_id);
        assert_eq!(reservation.grant_revision, grant_revision);
        assert_eq!(reservation.actor_run_id, run.run_id);

        service.close().unwrap();
        remove_database(&path);
    }

    /// H3B shape (b) end to end past the point
    /// `h3b_dispatch_accepts_a_grant_minted_by_resolve_harness_mcp_grant`
    /// above stops at: the Node's `Armed` reply is recorded and the spawn
    /// lease is issued from it. Measured live on claude and codex, this
    /// exact composition -- `resolve_harness_mcp_grant`'s minted `Operator`
    /// grant, armed and recorded through `record_harness_mcp_armed_and_
    /// issue_spawn_lease` -- went `OutcomeUnknown` with `HarnessMcpProofMismatch`
    /// even though the Node had already armed the reservation: the launch
    /// plan's own `grant: Operator` carries no exact `(grant_id, revision)`
    /// to compare against, so `prepared_scheduled_spawn_dispatch`'s harness-
    /// MCP guard -- built only for shape (a)'s `grant: Exact` plans -- could
    /// never match an `Operator`-policy plan's armed reservation at all.
    #[test]
    fn h3b_operator_started_top_level_run_arms_and_issues_spawn_lease() {
        let path = database_path("h3b-operator-arm-and-lease");
        let mut plan = exact_launch_plan(false, false, true);
        plan.grant = crate::dispatch::HarnessGrantPolicyV1::Operator;
        let catalog = dispatch::HarnessLaunchCatalog::new([plan.clone()]).unwrap();
        let scheduled = plan.scheduled_ref().unwrap();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine: h3b_operator_dispatch_engine(),
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::from([(run_operation_id(), scheduled)]),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        let run = service.engine().run(&run_id()).unwrap().clone();
        let operation = service.engine().operation(&run_operation_id()).unwrap().clone();
        assert_eq!(run.parent_run_id, None);
        assert!(matches!(operation.actor, HarnessActorV1::User { .. }));

        let mut spec = spawn_spec("operator top-level H3B arm and lease");
        spec.overrides.terminal_size = SpawnOverride::Set {
            value: TerminalSize { rows: 40, columns: 120 },
        };
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        let mut context = dispatch_context(&operation, fingerprint);
        let incarnation = "07".repeat(16);
        context.node_incarnation_id = HarnessSelectorV1::new(&incarnation).unwrap();

        let (grant_id, grant_revision) = crate::runtime::resolve_harness_mcp_grant(
            &mut service,
            &operation.operation_id,
            &crate::dispatch::HarnessGrantPolicyV1::Operator,
            &run.run_id,
            HarnessGrantTargetV1 {
                node_id: context.node_id.clone(),
                workspace_id: context.workspace_id.clone(),
                provider_profile: context.provider_profile.clone(),
                mode: context.mode,
            },
            13,
        ).unwrap();

        let mut run = run;
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 13;
        let mut operation = operation;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 13;
        operation.dispatched_at_unix_ms = Some(13);

        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "d".repeat(24),
        )).unwrap();

        service.begin_run_dispatch_with_harness_mcp(
            HarnessRevision::new(1).unwrap(),
            run.clone(),
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
            reservation_id.clone(),
            grant_id.clone(),
            grant_revision,
            1_000,
        ).unwrap();

        let route = gate4agent_c2_protocol::NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let prepared_record = service.harness_mcp_reservation(&reservation_id).unwrap().clone();
        assert_eq!(prepared_record.grant_id, grant_id);
        assert_eq!(prepared_record.actor_run_id, run.run_id);
        let proof = c2::armed_harness_mcp_reservation_proof_for_test(route, prepared_record);

        let _lease = service.record_harness_mcp_armed_and_issue_spawn_lease(
            &catalog,
            proof,
            14,
            spec,
        ).unwrap();
        assert_eq!(
            service.harness_mcp_reservation_state(&reservation_id),
            Some(HarnessMcpReservationStateV1::Armed),
        );

        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn h3b_active_grant_revision_rollover_revokes_bound_sessions() {
        const RAW_SPAWN_CANARY: &str =
            "RAW_H3B_ROLLOVER_C:\\provider-home\\private-session-token";
        let path = database_path("h3b-grant-rollover");
        let spec = spawn_spec(RAW_SPAWN_CANARY);
        let mut checkpoint = h3b_dispatch_engine().checkpoint();
        let child_run = checkpoint.runs.iter_mut()
            .find(|run| run.run_id == run_id()).unwrap();
        child_run.revision = HarnessRevision::new(3).unwrap();
        child_run.lifecycle = HarnessRunLifecycleV1::Running;
        child_run.binding = Some(HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("incarnation-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-a").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 7,
                    generation: 11,
                }),
            },
        });
        child_run.updated_at_unix_ms = 14;
        let child_operation = checkpoint.operations.iter_mut()
            .find(|operation| operation.operation_id == run_operation_id()).unwrap();
        child_operation.revision = HarnessRevision::new(3).unwrap();
        child_operation.state = HarnessOperationStateV1::Succeeded;
        child_operation.updated_at_unix_ms = 14;
        child_operation.dispatched_at_unix_ms = Some(13);
        child_operation.finished_at_unix_ms = Some(14);
        let engine = HarnessEngine::restore(checkpoint).unwrap();
        let child_operation = engine.operation(&run_operation_id()).unwrap().clone();
        let context = dispatch_context(
            &child_operation,
            c2::spawn_spec_fingerprint(&spec).unwrap(),
        );
        let reservation_id = HarnessMcpReservationId::new(format!(
            "hmcpres_{}",
            "e".repeat(24),
        )).unwrap();
        let grant_id = SessionGrantId::new(format!(
            "hgrant_{}",
            "c".repeat(24),
        )).unwrap();
        let parent_run_id = HarnessRunId::new(format!(
            "hrun_{}",
            "c".repeat(24),
        )).unwrap();
        let reservation = HarnessMcpReservationV1 {
            reservation_id: reservation_id.clone(),
            revision: HarnessRevision::new(3).unwrap(),
            state: HarnessMcpReservationStateV1::Bound,
            activation_digest: harness_mcp_activation_digest(
                &reservation_id,
                &grant_id,
                HarnessRevision::new(1).unwrap(),
                &run_id(),
                &run_operation_id(),
                &context,
                1_000,
            ).unwrap(),
            grant_id: grant_id.clone(),
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: parent_run_id,
            operation_id: run_operation_id(),
            node_id: context.node_id.clone(),
            node_incarnation_id: context.node_incarnation_id.clone(),
            workspace_id: context.workspace_id.clone(),
            provider_profile: context.provider_profile.clone(),
            expected_provider: context.expected_provider.clone(),
            mode: context.mode,
            spawn_spec_fingerprint: context.spawn_spec_fingerprint.clone(),
            idempotency_ref: context.idempotency_ref.clone(),
            expires_at_unix_ms: 1_000,
            record_id: Some(HarnessSelectorV1::new("record-a").unwrap()),
            instance_id: Some(7),
            generation: Some(11),
            created_at_unix_ms: 13,
            updated_at_unix_ms: 14,
        };
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts: BTreeMap::from([(
                run_operation_id(),
                context,
            )]),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        service.commit_reservation_only(reservation).unwrap();
        assert_eq!(
            service.harness_mcp_reservation_state(&reservation_id),
            Some(HarnessMcpReservationStateV1::Bound),
        );

        let mut next_grant = service.engine().grant(&grant_id).unwrap().clone();
        next_grant.revision = HarnessRevision::new(2).unwrap();
        next_grant.updated_at_unix_ms = 20;
        let grant_operation = HarnessOperationV1 {
            operation_id: HarnessOperationId::new(format!(
                "hop_{}",
                "e".repeat(24),
            )).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::MutateGrant,
            state: HarnessOperationStateV1::Succeeded,
            task_id: None,
            run_id: None,
            grant_id: Some(grant_id.clone()),
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("0".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "e".repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20,
            dispatched_at_unix_ms: Some(20),
            finished_at_unix_ms: Some(20),
        };
        let mut mutation = HarnessMutationV1::ReplaceGrant {
            operation: grant_operation,
            expected_revision: HarnessRevision::new(1).unwrap(),
            grant: next_grant,
        };
        mutation.operation_mut().request_digest = mutation_request_digest(&mutation).unwrap();
        assert_eq!(service.apply(mutation).unwrap(), HarnessApplyOutcome::Applied);
        assert_eq!(
            service.harness_mcp_reservation_state(&reservation_id),
            Some(HarnessMcpReservationStateV1::Revoked),
        );
        assert_eq!(
            service.engine().grant(&grant_id).unwrap().revision,
            HarnessRevision::new(2).unwrap(),
        );

        service.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            if let Ok(bytes) = fs::read(candidate) {
                assert!(!String::from_utf8_lossy(&bytes).contains(RAW_SPAWN_CANARY));
            }
        }
        let reopened = HarnessService::open(&path).unwrap();
        assert_eq!(
            reopened.harness_mcp_reservation_state(&reservation_id),
            Some(HarnessMcpReservationStateV1::Revoked),
        );
        assert_eq!(
            reopened.engine().grant(&grant_id).unwrap().revision,
            HarnessRevision::new(2).unwrap(),
        );
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_run_dispatch_requires_atomic_exact_spawn_spec_and_persists_no_raw_spec() {
        const PRIVATE_SPAWN_CANARY: &str =
            "RAW_SPAWN_PROMPT_C:\\Users\\owner\\.provider-home\\secret.txt";
        let path = database_path("spawn-seam");
        let mut service = HarnessService::open(&path).unwrap();
        apply_task(&mut service);
        let (mut run, mut operation) = create_run(&mut service);
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 13;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 13;
        operation.dispatched_at_unix_ms = Some(13);
        let spec = spawn_spec(PRIVATE_SPAWN_CANARY);
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        let context = dispatch_context(&operation, fingerprint.clone());
        let before = service.committed_snapshot();

        assert!(matches!(
            service.transition_operation(HarnessRevision::new(1).unwrap(), operation.clone()),
            Err(HarnessServiceError::NonAtomicRunOperation)
        ));
        assert!(matches!(
            service.begin_dispatch(
                HarnessRevision::new(1).unwrap(),
                operation.clone(),
                context.clone(),
            ),
            Err(HarnessServiceError::NonAtomicRunOperation)
        ));
        let mut inherited_spec = spec.clone();
        inherited_spec.overrides.provider = SpawnOverride::Inherit;
        let inherited_context = dispatch_context(
            &operation,
            c2::spawn_spec_fingerprint(&inherited_spec).unwrap(),
        );
        assert!(matches!(
            service.begin_run_dispatch(
                HarnessRevision::new(1).unwrap(),
                run.clone(),
                HarnessRevision::new(1).unwrap(),
                operation.clone(),
                inherited_context,
                &inherited_spec,
            ),
            Err(HarnessServiceError::InvalidDispatchContext(_))
        ));
        let mut wrong_profile = context.clone();
        wrong_profile.provider_profile = HarnessSelectorV1::new("codex-default").unwrap();
        assert!(matches!(
            service.begin_run_dispatch(
                HarnessRevision::new(1).unwrap(),
                run.clone(),
                HarnessRevision::new(1).unwrap(),
                operation.clone(),
                wrong_profile,
                &spec,
            ),
            Err(HarnessServiceError::InvalidDispatchContext(_))
        ));
        let mut wrong_context = context.clone();
        wrong_context.spawn_spec_fingerprint = HarnessRequestDigest::new("f".repeat(64)).unwrap();
        assert!(matches!(
            service.begin_run_dispatch(
                HarnessRevision::new(1).unwrap(),
                run.clone(),
                HarnessRevision::new(1).unwrap(),
                operation.clone(),
                wrong_context,
                &spec,
            ),
            Err(HarnessServiceError::InvalidDispatchContext(_))
        ));
        assert_eq!(service.committed_snapshot(), before);

        service.begin_run_dispatch(
            HarnessRevision::new(1).unwrap(),
            run,
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
        ).unwrap();
        let mut running = service.engine().run(&run_id()).unwrap().clone();
        running.revision = HarnessRevision::new(3).unwrap();
        running.lifecycle = HarnessRunLifecycleV1::Running;
        running.updated_at_unix_ms = 14;
        running.binding = Some(HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("incarnation-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("wrong-record").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 99,
                    generation: 99,
                }),
            },
        });
        let mut succeeded = service.engine().operation(&run_operation_id()).unwrap().clone();
        succeeded.revision = HarnessRevision::new(3).unwrap();
        succeeded.state = HarnessOperationStateV1::Succeeded;
        succeeded.updated_at_unix_ms = 14;
        succeeded.finished_at_unix_ms = Some(14);
        let before_unproved_binding = service.committed_snapshot();
        assert!(matches!(
            service.transition_run_operation(
                HarnessRevision::new(2).unwrap(),
                running,
                HarnessRevision::new(2).unwrap(),
                succeeded,
            ),
            Err(HarnessServiceError::AcceptedSpawnProofRequired)
        ));
        assert_eq!(service.committed_snapshot(), before_unproved_binding);
        service.close().unwrap();
        for candidate in [
            path.clone(),
            PathBuf::from(format!("{}-wal", path.display())),
            PathBuf::from(format!("{}-shm", path.display())),
        ] {
            if !candidate.is_file() {
                continue;
            }
            let bytes = fs::read(candidate).unwrap();
            assert!(!bytes.windows(PRIVATE_SPAWN_CANARY.len()).any(
                |window| window == PRIVATE_SPAWN_CANARY.as_bytes()
            ));
        }
        remove_database(&path);
    }

    #[test]
    fn harness_restart_checkpoint_tail_equivalence() {
        let path = database_path("restart");
        let expected = {
            let mut service = HarnessService::open(&path).unwrap();
            apply_task(&mut service);
            let snapshot = service.committed_snapshot();
            service.close().unwrap();
            snapshot
        };
        let reopened = HarnessService::open(&path).unwrap();
        assert_eq!(reopened.committed_snapshot(), expected);
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_rejects_newer_schema_without_rewrite() {
        let path = database_path("newer-schema");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(
            "CREATE TABLE future_owner(value TEXT NOT NULL);
             INSERT INTO future_owner(value) VALUES ('preserve-me');
             PRAGMA user_version = 2;",
        ).unwrap();
        connection.close().unwrap();
        let before = fs::read(&path).unwrap();
        assert!(matches!(
            HarnessService::open(&path),
            Err(HarnessServiceError::Store(HarnessStoreError::UnsupportedSchema(2)))
        ));
        let after = fs::read(&path).unwrap();
        assert_eq!(after, before);
        remove_database(&path);
    }

    #[test]
    fn harness_single_writer_publishes_only_committed_state() {
        let path = database_path("single-writer");
        let mut owner = HarnessService::open(&path).unwrap();
        assert!(HarnessService::open(&path).is_err());
        assert!(owner.engine().task(&task_id()).is_none());
        apply_task(&mut owner);
        assert!(owner.engine().task(&task_id()).is_some());
        owner.close().unwrap();
        let reopened = HarnessService::open(&path).unwrap();
        assert!(reopened.engine().task(&task_id()).is_some());
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn harness_store_bytes_lack_privacy_sentinels() {
        let path = database_path("privacy");
        let mut service = HarnessService::open(&path).unwrap();
        apply_task(&mut service);
        service.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        for sentinel in [
            "RAW_PROMPT_SENTINEL_4fca",
            "TRANSCRIPT_SENTINEL_77bd",
            "TOOL_PAYLOAD_SENTINEL_f931",
            "Bearer secret-token-sentinel",
            "C:\\Users\\owner\\.provider-home",
        ] {
            assert!(!bytes.windows(sentinel.len()).any(|window| window == sentinel.as_bytes()));
        }
        remove_database(&path);
    }

    #[test]
    fn continuation_grant_permissions_and_source_provider_are_fail_closed() {
        for (export, restore) in [(false, true), (true, false)] {
            let path = database_path("continuation-permissions");
            let (engine, dispatch_contexts, continuation) =
                continuation_fixture(export, restore);
            let mut service = HarnessService {
                store: Some(HarnessStore::open(&path).unwrap()),
                engine,
                dispatch_contexts,
                harness_mcp_reservations: BTreeMap::new(),
                operator_requests: BTreeMap::new(),
                scheduled_launches: BTreeMap::new(),
                issued_launches: BTreeMap::new(),
                poisoned: false,
            };
            let before = service.engine.checkpoint();
            assert!(service.prepare_continuation(continuation).is_err());
            assert_eq!(service.engine.checkpoint(), before);
            service.close().unwrap();
            remove_database(&path);
        }

        let path = database_path("continuation-provider");
        let (engine, dispatch_contexts, mut continuation) = continuation_fixture(true, true);
        continuation.source_provider = HarnessSelectorV1::new("codex").unwrap();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts,
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        let before = service.engine.checkpoint();
        assert!(service.prepare_continuation(continuation).is_err());
        assert_eq!(service.engine.checkpoint(), before);
        service.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn continuation_export_lease_linearizes_grant_revoke_and_never_reissues() {
        let path = database_path("continuation-revoke-before-lease");
        let (engine, dispatch_contexts, continuation) = continuation_fixture(true, true);
        let continuation_ref = continuation.continuation_ref.clone();
        let HarnessTransferAuthorityRefV1::ParentGrant { grant_id, .. } =
            &continuation.authority else { panic!("parent grant fixture") };
        let grant_id = grant_id.clone();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts,
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        service.prepare_continuation(continuation).unwrap();
        revoke_grant(&mut service, &grant_id, '7', 21);
        let before = service.committed_snapshot();
        assert!(service.begin_continuation_export(
            &continuation_ref,
            HarnessRevision::new(1).unwrap(),
            22,
        ).is_err());
        assert_eq!(service.committed_snapshot(), before);
        service.close().unwrap();
        remove_database(&path);

        let path = database_path("continuation-lease-before-revoke");
        let (engine, dispatch_contexts, continuation) = continuation_fixture(true, true);
        let continuation_ref = continuation.continuation_ref.clone();
        let HarnessTransferAuthorityRefV1::ParentGrant { grant_id, .. } =
            &continuation.authority else { panic!("parent grant fixture") };
        let grant_id = grant_id.clone();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts,
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        service.prepare_continuation(continuation).unwrap();
        let lease = service.begin_continuation_export(
            &continuation_ref,
            HarnessRevision::new(1).unwrap(),
            21,
        ).unwrap();
        revoke_grant(&mut service, &grant_id, '8', 22);
        service.mark_continuation_export_outcome_unknown(
            &lease,
            HarnessContinuationOutcomeUnknownReasonV1::Transport,
            23,
        ).unwrap();
        service.close().unwrap();
        let mut reopened = HarnessService::open(&path).unwrap();
        let before = reopened.committed_snapshot();
        assert!(reopened.begin_continuation_export(
            &continuation_ref,
            HarnessRevision::new(3).unwrap(),
            24,
        ).is_err());
        assert_eq!(reopened.committed_snapshot(), before);
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn delivery_staging_lease_rechecks_revocation_before_each_restart_issue() {
        let path = database_path("delivery-staging-lease");
        let source_root = path.with_extension("source");
        fs::create_dir(&source_root).unwrap();
        protect_delivery_test_path(&source_root);
        let source_file = source_root.join("SKILL.md");
        fs::write(&source_file, b"reviewed delivery").unwrap();
        protect_delivery_test_path(&source_file);
        let source = ReviewedDeliverySourceV2::new(
            source_root.clone(),
            None,
            DeliveryComponentKindV2::Skill,
            DeliveryScopeV2::Session,
        ).unwrap();
        let compiled = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("bundle.review-kit").unwrap(),
            SpawnBundleRevision::new("rev-7").unwrap(),
            &[source],
        ).unwrap();
        let grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine: delivery_engine(),
            dispatch_contexts: BTreeMap::new(),
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        service.prepare_delivery_from_compiled(
            delivery_ref(),
            grant_id.clone(),
            HarnessRevision::new(1).unwrap(),
            task_id(),
            run_id(),
            run_operation_id(),
            HarnessSelectorV1::new("review-kit").unwrap(),
            &compiled,
            13,
        ).unwrap();
        revoke_grant(&mut service, &grant_id, '9', 21);
        let route = gate4agent_c2_protocol::NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([7; 16]),
        };
        let before = service.committed_snapshot();
        assert!(service.issue_delivery_staging_lease(
            &delivery_ref(),
            route.clone(),
            compiled.clone(),
        ).is_err());
        assert_eq!(service.committed_snapshot(), before);
        service.close().unwrap();
        let reopened = HarnessService::open(&path).unwrap();
        let before = reopened.committed_snapshot();
        assert!(reopened.issue_delivery_staging_lease(
            &delivery_ref(),
            route,
            compiled,
        ).is_err());
        assert_eq!(reopened.committed_snapshot(), before);
        reopened.close().unwrap();
        remove_database(&path);
        fs::remove_dir_all(source_root).unwrap();
    }

    #[test]
    fn continuation_lost_export_reply_freezes_restart_and_incarnation_expiry() {
        let path = database_path("continuation-lost-reply");
        let (engine, dispatch_contexts, continuation) = continuation_fixture(true, true);
        let continuation_ref = continuation.continuation_ref.clone();
        let mut service = HarnessService {
            store: Some(HarnessStore::open(&path).unwrap()),
            engine,
            dispatch_contexts,
            harness_mcp_reservations: BTreeMap::new(),
            operator_requests: BTreeMap::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
            poisoned: false,
        };
        assert_eq!(
            service.prepare_continuation(continuation).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        let prepared = service.begin_continuation_export(
            &continuation_ref,
            HarnessRevision::new(1).unwrap(),
            21,
        ).unwrap();
        assert!(service.begin_continuation_export(
            &continuation_ref,
            HarnessRevision::new(1).unwrap(),
            21,
        ).is_err());
        service.mark_continuation_export_outcome_unknown(
            &prepared,
            HarnessContinuationOutcomeUnknownReasonV1::Transport,
            22,
        ).unwrap();
        assert_eq!(
            service.continuation(&continuation_ref).unwrap().state,
            HarnessContinuationStateV1::OutcomeUnknown,
        );
        service.close().unwrap();

        let mut reopened = HarnessService::open(&path).unwrap();
        assert_eq!(
            reopened.continuation(&continuation_ref).unwrap().state,
            HarnessContinuationStateV1::OutcomeUnknown,
        );
        assert!(reopened.begin_continuation_export(
            &continuation_ref,
            HarnessRevision::new(3).unwrap(),
            23,
        ).is_err());
        let changed_route = gate4agent_c2_protocol::NodeRoute {
            node_id: NodeId::new("node-a").unwrap(),
            expected_incarnation_id: NodeIncarnationId::from_bytes([8; 16]),
        };
        assert_eq!(
            reopened.expire_unbound_continuation_on_incarnation_change(
                &continuation_ref,
                &changed_route,
                23,
            ).unwrap(),
            HarnessApplyOutcome::Applied,
        );
        reopened.close().unwrap();

        let reopened = HarnessService::open(&path).unwrap();
        assert_eq!(
            reopened.continuation(&continuation_ref).unwrap().state,
            HarnessContinuationStateV1::Expired,
        );
        reopened.close().unwrap();
        let bytes = fs::read(&path).unwrap();
        for sentinel in [
            "RAW_CONTEXT_HISTORY_SENTINEL_82b1",
            "PROMPT_SENTINEL_a901",
            "C:\\private\\provider\\history.jsonl",
        ] {
            assert!(!bytes.windows(sentinel.len()).any(|window| window == sentinel.as_bytes()));
        }
        remove_database(&path);
    }

    #[test]
    fn accepted_spawn_proof_is_operation_and_fingerprint_bound_across_reopen() {
        let path = database_path("accepted-spawn-proof-bound");
        let incarnation = NodeIncarnationId::from_bytes([7; 16]);
        let mut service = HarnessService::open(&path).unwrap();
        apply_task(&mut service);
        let (mut run, mut operation) = create_run(&mut service);
        let spec = spawn_spec("accepted proof fixture");
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        run.revision = HarnessRevision::new(2).unwrap();
        run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        run.updated_at_unix_ms = 13;
        operation.revision = HarnessRevision::new(2).unwrap();
        operation.state = HarnessOperationStateV1::Dispatching;
        operation.updated_at_unix_ms = 13;
        operation.dispatched_at_unix_ms = Some(13);
        let mut context = dispatch_context(&operation, fingerprint.clone());
        context.node_incarnation_id = HarnessSelectorV1::new(
            incarnation.to_string(),
        ).unwrap();
        service.begin_run_dispatch(
            HarnessRevision::new(1).unwrap(),
            run,
            HarnessRevision::new(1).unwrap(),
            operation,
            context,
            &spec,
        ).unwrap();

        let session = SessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(7),
                generation: SessionGeneration(1),
            },
        };
        let proof = c2::accepted_spawn_binding_proof_for_test(
            run_operation_id(),
            fingerprint.clone(),
            service.engine().operation(&run_operation_id()).unwrap()
                .idempotency_ref.clone(),
            NodeId::new("node-a").unwrap(),
            incarnation,
            WorkspaceId::new("workspace-a").unwrap(),
            AgentId::new("claude").unwrap(),
            SessionMode::Pty,
            gate4agent_node_protocol::SessionRecordId::new("record-a").unwrap(),
            session.clone(),
            None,
            None,
        );
        let mut running = service.engine().run(&run_id()).unwrap().clone();
        running.revision = HarnessRevision::new(3).unwrap();
        running.lifecycle = HarnessRunLifecycleV1::Running;
        running.binding = Some(HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new(incarnation.to_string()).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-a").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: session.session.instance_id.0,
                    generation: session.session.generation.0,
                }),
            },
        });
        running.updated_at_unix_ms = 14;
        let mut succeeded = service.engine().operation(&run_operation_id()).unwrap().clone();
        succeeded.revision = HarnessRevision::new(3).unwrap();
        succeeded.state = HarnessOperationStateV1::Succeeded;
        succeeded.updated_at_unix_ms = 14;
        succeeded.finished_at_unix_ms = Some(14);

        let wrong_operation = c2::accepted_spawn_binding_proof_for_test(
            HarnessOperationId::new(format!("hop_{}", "c".repeat(24))).unwrap(),
            fingerprint.clone(),
            proof.idempotency_ref().clone(),
            NodeId::new("node-a").unwrap(),
            incarnation,
            WorkspaceId::new("workspace-a").unwrap(),
            AgentId::new("claude").unwrap(),
            SessionMode::Pty,
            gate4agent_node_protocol::SessionRecordId::new("record-a").unwrap(),
            session.clone(),
            None,
            None,
        );
        let before_rejects = service.committed_snapshot();
        assert!(service.transition_run_with_accepted_spawn(
            HarnessRevision::new(2).unwrap(),
            running.clone(),
            HarnessRevision::new(2).unwrap(),
            succeeded.clone(),
            &wrong_operation,
        ).is_err());
        let wrong_fingerprint = c2::accepted_spawn_binding_proof_for_test(
            run_operation_id(),
            HarnessRequestDigest::new("f".repeat(64)).unwrap(),
            proof.idempotency_ref().clone(),
            NodeId::new("node-a").unwrap(),
            incarnation,
            WorkspaceId::new("workspace-a").unwrap(),
            AgentId::new("claude").unwrap(),
            SessionMode::Pty,
            gate4agent_node_protocol::SessionRecordId::new("record-a").unwrap(),
            session,
            None,
            None,
        );
        assert!(service.transition_run_with_accepted_spawn(
            HarnessRevision::new(2).unwrap(),
            running.clone(),
            HarnessRevision::new(2).unwrap(),
            succeeded.clone(),
            &wrong_fingerprint,
        ).is_err());
        assert_eq!(service.committed_snapshot(), before_rejects);

        service.transition_run_with_accepted_spawn(
            HarnessRevision::new(2).unwrap(),
            running.clone(),
            HarnessRevision::new(2).unwrap(),
            succeeded.clone(),
            &proof,
        ).unwrap();
        service.close().unwrap();

        let mut reopened = HarnessService::open(&path).unwrap();
        let exact = reopened.committed_snapshot();
        reopened.transition_run_with_accepted_spawn(
            HarnessRevision::new(2).unwrap(),
            running.clone(),
            HarnessRevision::new(2).unwrap(),
            succeeded.clone(),
            &proof,
        ).unwrap();
        assert_eq!(reopened.committed_snapshot(), exact);
        assert!(reopened.transition_run_with_accepted_spawn(
            HarnessRevision::new(2).unwrap(),
            running,
            HarnessRevision::new(2).unwrap(),
            succeeded,
            &wrong_fingerprint,
        ).is_err());
        assert_eq!(reopened.committed_snapshot(), exact);
        reopened.close().unwrap();
        remove_database(&path);
    }

    #[test]
    fn combined_delivery_continuation_replay_after_sqlite_reopen_is_exact() {
        let path = database_path("combined-continuation-replay");
        let (engine, dispatch_contexts, mut continuation) = continuation_fixture(true, true);
        let continuation_ref = continuation.continuation_ref.clone();
        let continuation_receipt_ref = continuation.receipt_ref.clone();
        let incarnation = "07".repeat(16);
        let context = HarnessResolvedContextPackReceiptV1 {
            id: HarnessSelectorV1::new("context-a").unwrap(),
            digest: format!("sha256:{}", "a".repeat(64)),
            lineage: HarnessContextPackLineageV1 {
                source_node_id: HarnessSelectorV1::new("node-a").unwrap(),
                source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                source_instance_id: 7,
                source_generation: 1,
                source_provider: HarnessSelectorV1::new("claude").unwrap(),
            },
            source_message_count: 3,
            retained_message_count: 3,
            byte_len: 100,
            truncated: false,
        };
        continuation.revision = HarnessRevision::new(3).unwrap();
        continuation.state = HarnessContinuationStateV1::Exported;
        continuation.context = Some(context.clone());
        continuation.exporting_at_unix_ms = Some(21);
        continuation.exported_at_unix_ms = Some(22);
        continuation.updated_at_unix_ms = 22;

        let mut checkpoint = engine.checkpoint();
        let target_run = checkpoint.runs.iter_mut()
            .find(|run| run.run_id == run_id()).unwrap();
        target_run.revision = HarnessRevision::new(2).unwrap();
        target_run.intent.delivery_bundle = Some(HarnessSelectorV1::new("review-kit").unwrap());
        target_run.continuation_receipt = Some(continuation_receipt_ref.clone());
        target_run.updated_at_unix_ms = 22;
        checkpoint.grants[0].allowed_delivery_bundles = vec![
            HarnessSelectorV1::new("review-kit").unwrap(),
        ];
        let mut delivery = prepared_delivery();
        delivery.created_at_unix_ms = 20;
        delivery.updated_at_unix_ms = 22;
        delivery.revision = HarnessRevision::new(2).unwrap();
        delivery.state = HarnessDeliveryStateV1::Staged;
        delivery.stage_receipt = Some(HarnessDeliveryStageReceiptV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            bundle: delivery.bundle.clone(),
            staged_at_unix_ms: 22,
        });
        checkpoint.deliveries = vec![delivery.clone()];
        checkpoint.continuations = vec![continuation.clone()];
        let engine = HarnessEngine::restore(checkpoint).unwrap();
        let engine_checkpoint = engine.checkpoint();
        let persisted_checkpoint = HarnessServiceCheckpointV1 {
            version: HARNESS_SERVICE_CHECKPOINT_VERSION_V1,
            engine: engine_checkpoint.clone(),
            dispatch_contexts: dispatch_contexts.values().cloned().collect(),
            deliveries: engine_checkpoint.deliveries.clone(),
            continuations: engine_checkpoint.continuations.clone(),
            harness_mcp_reservations: Vec::new(),
            operator_requests: Vec::new(),
            scheduled_launches: BTreeMap::new(),
            issued_launches: BTreeMap::new(),
        };
        let persisted = encode_persisted_state(&persisted_checkpoint).unwrap();
        let target_operation = engine.operation(&run_operation_id()).unwrap().clone();
        let tail = encode_operation(&target_operation).unwrap();
        let mut store = HarnessStore::open(&path).unwrap();
        store.commit(&persisted, &tail).unwrap();
        store.close().unwrap();

        let mut service = HarnessService::open(&path).unwrap();
        let mut spec = spawn_spec("combined continuation");
        spec.overrides.bundle_id = SpawnOverride::Set {
            value: SpawnBundleId::new("bundle.review-kit").unwrap(),
        };
        spec.overrides.context_id = SpawnOverride::Set {
            value: SpawnContextId::new("context-a").unwrap(),
        };
        spec.overrides.terminal_size = SpawnOverride::Set {
            value: TerminalSize { rows: 40, columns: 120 },
        };
        let fingerprint = c2::spawn_spec_fingerprint(&spec).unwrap();
        let mut dispatching_run = service.engine().run(&run_id()).unwrap().clone();
        dispatching_run.revision = HarnessRevision::new(3).unwrap();
        dispatching_run.lifecycle = HarnessRunLifecycleV1::Dispatching;
        dispatching_run.updated_at_unix_ms = 25;
        let mut dispatching_operation = service.engine()
            .operation(&run_operation_id()).unwrap().clone();
        dispatching_operation.revision = HarnessRevision::new(2).unwrap();
        dispatching_operation.state = HarnessOperationStateV1::Dispatching;
        dispatching_operation.updated_at_unix_ms = 25;
        dispatching_operation.dispatched_at_unix_ms = Some(25);
        let context_record = HarnessDispatchContextV1 {
            operation_id: run_operation_id(),
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation_id: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            provider_profile: HarnessSelectorV1::new("claude-default").unwrap(),
            expected_provider: HarnessSelectorV1::new("claude").unwrap(),
            mode: HarnessExecutionModeV1::Pty,
            baseline_record_ids: Vec::new(),
            spawn_spec_fingerprint: fingerprint.clone(),
            dispatched_at_unix_ms: 25,
            idempotency_ref: dispatching_operation.idempotency_ref.clone(),
            managed_worktree_binding: None,
        };
        let _lease = service.issue_continuation_spawn_lease(
            HarnessRevision::new(2).unwrap(),
            dispatching_run.clone(),
            HarnessRevision::new(1).unwrap(),
            dispatching_operation.clone(),
            context_record.clone(),
            spec.clone(),
            &continuation_ref,
            Some(&delivery_ref()),
        ).unwrap();
        let leased_snapshot = service.committed_snapshot();
        assert!(service.issue_continuation_spawn_lease(
            HarnessRevision::new(3).unwrap(),
            dispatching_run,
            HarnessRevision::new(2).unwrap(),
            dispatching_operation,
            context_record,
            spec.clone(),
            &continuation_ref,
            Some(&delivery_ref()),
        ).is_err());
        assert_eq!(service.committed_snapshot(), leased_snapshot);
        service.close().unwrap();
        let mut service = HarnessService::open(&path).unwrap();
        let restored_run = service.engine().run(&run_id()).unwrap().clone();
        let restored_operation = service.engine()
            .operation(&run_operation_id()).unwrap().clone();
        let restored_context = service.dispatch_context(&run_operation_id()).unwrap().clone();
        let restored_before = service.committed_snapshot();
        assert!(service.issue_continuation_spawn_lease(
            restored_run.revision,
            restored_run,
            restored_operation.revision,
            restored_operation,
            restored_context,
            spec.clone(),
            &continuation_ref,
            Some(&delivery_ref()),
        ).is_err());
        assert_eq!(service.committed_snapshot(), restored_before);

        let target_session = SessionAddress {
            workspace_id: WorkspaceId::new("workspace-a").unwrap(),
            session: SessionKey {
                instance_id: AgentInstanceId(8),
                generation: SessionGeneration(1),
            },
        };
        let target_binding = HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new(&incarnation).unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-target").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 8,
                    generation: 1,
                }),
            },
        };
        let mut running_run = service.engine().run(&run_id()).unwrap().clone();
        running_run.revision = HarnessRevision::new(4).unwrap();
        running_run.lifecycle = HarnessRunLifecycleV1::Running;
        running_run.binding = Some(target_binding.clone());
        running_run.updated_at_unix_ms = 30;
        let delivery_receipt_ref = HarnessReceiptRef::new(format!(
            "hreceipt_{}", "f".repeat(24),
        )).unwrap();
        running_run.delivery_receipt = Some(delivery_receipt_ref.clone());
        let mut succeeded_operation = service.engine()
            .operation(&run_operation_id()).unwrap().clone();
        succeeded_operation.revision = HarnessRevision::new(3).unwrap();
        succeeded_operation.state = HarnessOperationStateV1::Succeeded;
        succeeded_operation.updated_at_unix_ms = 30;
        succeeded_operation.finished_at_unix_ms = Some(30);
        let mut committed_delivery = service.engine().delivery(&delivery_ref()).unwrap().clone();
        committed_delivery.revision = HarnessRevision::new(3).unwrap();
        committed_delivery.state = HarnessDeliveryStateV1::Committed;
        committed_delivery.updated_at_unix_ms = 30;
        committed_delivery.receipt = Some(HarnessDeliveryReceiptV1 {
            receipt_ref: delivery_receipt_ref,
            delivery_ref: committed_delivery.delivery_ref.clone(),
            authority: committed_delivery.authority.clone(),
            task_id: committed_delivery.task_id.clone(),
            run_id: committed_delivery.run_id.clone(),
            operation_id: committed_delivery.operation_id.clone(),
            binding: target_binding,
            bundle: committed_delivery.bundle.clone(),
            committed_at_unix_ms: 30,
        });
        let node_context = c2::harness_context_to_node(&context).unwrap();
        let node_bundle = ResolvedBundleReceipt {
            id: SpawnBundleId::new("bundle.review-kit").unwrap(),
            revision: SpawnBundleRevision::new("rev-7").unwrap(),
            digest: SpawnBundleDigest::new(format!("sha256:{}", "d".repeat(64))).unwrap(),
        };
        let proof = c2::accepted_spawn_binding_proof_for_test(
            run_operation_id(),
            fingerprint,
            succeeded_operation.idempotency_ref.clone(),
            NodeId::new("node-a").unwrap(),
            NodeIncarnationId::from_bytes([7; 16]),
            WorkspaceId::new("workspace-a").unwrap(),
            AgentId::new("claude").unwrap(),
            SessionMode::Pty,
            SessionRecordId::new("record-target").unwrap(),
            target_session.clone(),
            Some(node_bundle.clone()),
            Some(node_context),
        );
        service.transition_run_with_accepted_spawn_delivery_and_continuation(
            HarnessRevision::new(3).unwrap(),
            running_run.clone(),
            HarnessRevision::new(2).unwrap(),
            succeeded_operation.clone(),
            HarnessRevision::new(2).unwrap(),
            committed_delivery.clone(),
            &continuation_ref,
            HarnessRevision::new(3).unwrap(),
            &proof,
            30,
        ).unwrap();
        service.close().unwrap();

        let mut reopened = HarnessService::open(&path).unwrap();
        let before_replay = reopened.committed_snapshot();
        reopened.transition_run_with_accepted_spawn_delivery_and_continuation(
            HarnessRevision::new(4).unwrap(),
            running_run.clone(),
            HarnessRevision::new(3).unwrap(),
            succeeded_operation.clone(),
            HarnessRevision::new(3).unwrap(),
            committed_delivery.clone(),
            &continuation_ref,
            HarnessRevision::new(4).unwrap(),
            &proof,
            30,
        ).unwrap();
        assert_eq!(reopened.committed_snapshot(), before_replay);

        let mut changed_operation = succeeded_operation.clone();
        changed_operation.finished_at_unix_ms = Some(31);
        assert!(reopened.transition_run_with_accepted_spawn_delivery_and_continuation(
            HarnessRevision::new(4).unwrap(),
            running_run.clone(),
            HarnessRevision::new(3).unwrap(),
            changed_operation,
            HarnessRevision::new(3).unwrap(),
            committed_delivery.clone(),
            &continuation_ref,
            HarnessRevision::new(4).unwrap(),
            &proof,
            31,
        ).is_err());
        assert_eq!(reopened.committed_snapshot(), before_replay);

        let cross_context_receipt = ResolvedContextPackReceipt {
            id: SpawnContextId::new("context-cross").unwrap(),
            digest: SpawnContextDigest::new(format!("sha256:{}", "9".repeat(64))).unwrap(),
            lineage: ContextPackLineageReceipt {
                source_node_id: NodeId::new("node-a").unwrap(),
                source_session: SessionAddress {
                    workspace_id: WorkspaceId::new("workspace-a").unwrap(),
                    session: SessionKey {
                        instance_id: AgentInstanceId(7),
                        generation: SessionGeneration(1),
                    },
                },
                source_provider: AgentId::new("claude").unwrap(),
            },
            source_message_count: 3,
            retained_message_count: 3,
            byte_len: 100,
            truncated: false,
        };
        let cross_context = c2::accepted_spawn_binding_proof_for_test(
            run_operation_id(),
            proof.spawn_spec_fingerprint().clone(),
            proof.idempotency_ref().clone(),
            NodeId::new("node-a").unwrap(),
            NodeIncarnationId::from_bytes([7; 16]),
            WorkspaceId::new("workspace-a").unwrap(),
            AgentId::new("claude").unwrap(),
            SessionMode::Pty,
            SessionRecordId::new("record-target").unwrap(),
            target_session,
            Some(node_bundle),
            Some(cross_context_receipt),
        );
        assert!(reopened.transition_run_with_accepted_spawn_delivery_and_continuation(
            HarnessRevision::new(4).unwrap(),
            running_run,
            HarnessRevision::new(3).unwrap(),
            succeeded_operation,
            HarnessRevision::new(3).unwrap(),
            committed_delivery,
            &continuation_ref,
            HarnessRevision::new(4).unwrap(),
            &cross_context,
            30,
        ).is_err());
        assert_eq!(reopened.committed_snapshot(), before_replay);
        reopened.close().unwrap();
        remove_database(&path);
    }
}
