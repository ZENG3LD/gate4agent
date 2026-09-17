//! Pure, prepare-then-accept state machine for durable harness authority.

use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessContinuationRef, HarnessContinuationStateV1,
    HarnessContinuationV1, HarnessDeliveryRef, HarnessDeliveryStateV1, HarnessDeliveryV1,
    HarnessEntityReadScopeV1, HarnessExpectedExecutionSpecRevisionV1,
    HarnessOperationId, HarnessOperationKindV1,
    HarnessOperationStateV1, HarnessOperationV1, HarnessResultRef, HarnessRevision,
    HarnessRunId, HarnessRunLifecycleV1, HarnessRunV1,
    HarnessTaskExecutionSpecV1,
    HarnessTaskExecutionSpecV2, HarnessTaskId, HarnessTaskLaunchIssuanceV1,
    HarnessTaskStateV1, HarnessTaskV1, HarnessTransferAuthorityRefV1,
    HarnessValidationError, SessionGrantId, SessionGrantStateV1, SessionGrantV1,
    HARNESS_CHILD_DEPTH_MAX, HARNESS_CONTINUATIONS_MAX, HARNESS_DELIVERIES_MAX,
    HARNESS_RESULTS_MAX, HARNESS_SCHEDULER_SCAN_MAX,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const HARNESS_ENGINE_CHECKPOINT_VERSION_V1: u16 = 1;
pub const HARNESS_VISIBILITY_SCAN_MAX: usize = 8_192;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HarnessApplyOutcome {
    Applied,
    Replayed,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "mutation", rename_all = "kebab-case")]
pub enum HarnessMutationV1 {
    CreateTask {
        operation: HarnessOperationV1,
        task: HarnessTaskV1,
    },
    ReplaceTask {
        operation: HarnessOperationV1,
        expected_revision: HarnessRevision,
        task: HarnessTaskV1,
    },
    PutExecutionSpec {
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        expected_spec_revision: HarnessExpectedExecutionSpecRevisionV1,
        spec: HarnessTaskExecutionSpecV1,
    },
    PutIssuedExecutionSpec {
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        expected_spec_revision: HarnessExpectedExecutionSpecRevisionV1,
        expected_issuance_revision: HarnessExpectedExecutionSpecRevisionV1,
        issuance: HarnessTaskLaunchIssuanceV1,
        spec: HarnessTaskExecutionSpecV2,
    },
    CreateRun {
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
        run: HarnessRunV1,
    },
    CreateIssuedRun {
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        expected_execution_spec_revision: HarnessRevision,
        expected_issuance_revision: HarnessRevision,
        task: HarnessTaskV1,
        run: HarnessRunV1,
        delivery: Option<HarnessDeliveryV1>,
        continuation: Option<HarnessContinuationV1>,
    },
    ReplaceRun {
        operation: HarnessOperationV1,
        expected_revision: HarnessRevision,
        run: HarnessRunV1,
    },
    CreateGrant {
        operation: HarnessOperationV1,
        grant: SessionGrantV1,
    },
    ReplaceGrant {
        operation: HarnessOperationV1,
        expected_revision: HarnessRevision,
        grant: SessionGrantV1,
    },
}

impl HarnessMutationV1 {
    pub fn operation(&self) -> &HarnessOperationV1 {
        match self {
            Self::CreateTask { operation, .. }
            | Self::ReplaceTask { operation, .. }
            | Self::PutExecutionSpec { operation, .. }
            | Self::PutIssuedExecutionSpec { operation, .. }
            | Self::CreateRun { operation, .. }
            | Self::CreateIssuedRun { operation, .. }
            | Self::ReplaceRun { operation, .. }
            | Self::CreateGrant { operation, .. }
            | Self::ReplaceGrant { operation, .. } => operation,
        }
    }

    pub fn operation_mut(&mut self) -> &mut HarnessOperationV1 {
        match self {
            Self::CreateTask { operation, .. }
            | Self::ReplaceTask { operation, .. }
            | Self::PutExecutionSpec { operation, .. }
            | Self::PutIssuedExecutionSpec { operation, .. }
            | Self::CreateRun { operation, .. }
            | Self::CreateIssuedRun { operation, .. }
            | Self::ReplaceRun { operation, .. }
            | Self::CreateGrant { operation, .. }
            | Self::ReplaceGrant { operation, .. } => operation,
        }
    }

    /// Validates every bounded protocol payload before callers allocate a
    /// canonical serialization buffer or compute its digest.
    pub fn validate_payload(&self) -> Result<(), HarnessEngineError> {
        self.operation().validate()?;
        match self {
            Self::CreateTask { task, .. } => task.validate()?,
            Self::ReplaceTask { expected_revision, task, .. } => {
                expected_revision.validate()?;
                task.validate()?;
            }
            Self::PutExecutionSpec {
                expected_task_revision,
                expected_spec_revision,
                spec,
                ..
            } => {
                expected_task_revision.validate()?;
                expected_spec_revision.validate()?;
                spec.validate()?;
            }
            Self::PutIssuedExecutionSpec {
                expected_task_revision,
                expected_spec_revision,
                expected_issuance_revision,
                issuance,
                spec,
                ..
            } => {
                expected_task_revision.validate()?;
                expected_spec_revision.validate()?;
                expected_issuance_revision.validate()?;
                issuance.validate()?;
                spec.validate()?;
            }
            Self::CreateRun {
                expected_task_revision,
                task,
                run,
                ..
            } => {
                expected_task_revision.validate()?;
                task.validate()?;
                run.validate()?;
            }
            Self::CreateIssuedRun {
                expected_task_revision,
                expected_execution_spec_revision,
                expected_issuance_revision,
                task,
                run,
                delivery,
                continuation,
                ..
            } => {
                expected_task_revision.validate()?;
                expected_execution_spec_revision.validate()?;
                expected_issuance_revision.validate()?;
                task.validate()?;
                run.validate()?;
                if let Some(delivery) = delivery { delivery.validate()?; }
                if let Some(continuation) = continuation { continuation.validate()?; }
            }
            Self::ReplaceRun { expected_revision, run, .. } => {
                expected_revision.validate()?;
                run.validate()?;
            }
            Self::CreateGrant { grant, .. } => grant.validate()?,
            Self::ReplaceGrant { expected_revision, grant, .. } => {
                expected_revision.validate()?;
                grant.validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HarnessEngineCheckpointV1 {
    pub version: u16,
    pub tasks: Vec<HarnessTaskV1>,
    pub runs: Vec<HarnessRunV1>,
    pub grants: Vec<SessionGrantV1>,
    pub operations: Vec<HarnessOperationV1>,
    #[serde(default)]
    pub execution_specs: Vec<HarnessTaskExecutionSpecV1>,
    #[serde(default)]
    pub issuances: Vec<HarnessTaskLaunchIssuanceV1>,
    #[serde(default)]
    pub execution_specs_v2: Vec<HarnessTaskExecutionSpecV2>,
    #[serde(default)]
    pub deliveries: Vec<HarnessDeliveryV1>,
    #[serde(default)]
    pub continuations: Vec<HarnessContinuationV1>,
}

/// Forward migration off the two mailbox fields this checkpoint used to
/// carry (removed 2026-09-17: the mailbox moved to its own service, see
/// `gate4agent/CLAUDE.md`). `HarnessEngineCheckpointV1` is the payload half
/// of `harness_checkpoint.payload` -- a real on-disk blob, not just an
/// in-memory shape -- so a database written by a binary that still had
/// `mail_messages`/`mail_acks` must keep opening under `deny_unknown_fields`.
/// The values are never reconstructable into a real type any more (the mail
/// message/ack shapes themselves are gone too), so both keys are accepted
/// as opaque `IgnoredAny` and dropped, the same way `SessionGrantV1`
/// tolerates and drops its own retired `mail` bit.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HarnessEngineCheckpointWireV1 {
    version: u16,
    tasks: Vec<HarnessTaskV1>,
    runs: Vec<HarnessRunV1>,
    grants: Vec<SessionGrantV1>,
    operations: Vec<HarnessOperationV1>,
    #[serde(default)]
    execution_specs: Vec<HarnessTaskExecutionSpecV1>,
    #[serde(default)]
    issuances: Vec<HarnessTaskLaunchIssuanceV1>,
    #[serde(default)]
    execution_specs_v2: Vec<HarnessTaskExecutionSpecV2>,
    #[serde(default)]
    deliveries: Vec<HarnessDeliveryV1>,
    #[serde(default)]
    continuations: Vec<HarnessContinuationV1>,
    #[serde(default)]
    mail_messages: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    mail_acks: Option<serde::de::IgnoredAny>,
}

impl<'de> Deserialize<'de> for HarnessEngineCheckpointV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HarnessEngineCheckpointWireV1::deserialize(deserializer)?;
        let _retired_mail_messages = wire.mail_messages;
        let _retired_mail_acks = wire.mail_acks;
        Ok(Self {
            version: wire.version,
            tasks: wire.tasks,
            runs: wire.runs,
            grants: wire.grants,
            operations: wire.operations,
            execution_specs: wire.execution_specs,
            issuances: wire.issuances,
            execution_specs_v2: wire.execution_specs_v2,
            deliveries: wire.deliveries,
            continuations: wire.continuations,
        })
    }
}

/// Which of `HarnessEngineError`'s two graph-validation variants a cached
/// `visibility_graph_cache` outcome carries. `HarnessEngineError` itself
/// does not derive `Clone` (several variants wrap protocol types that are
/// not `Clone`), so a cached `Result<(), HarnessEngineError>` cannot be
/// stored and replayed directly on a cache hit -- this small `Copy` tag
/// records which of the two failure shapes the last recomputation found,
/// and `read_visibility` reconstructs the real error from it on every
/// read, hit or miss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HarnessVisibilityGraphFaultV1 {
    ResourceExhausted,
    InvalidGraph,
}

impl From<HarnessVisibilityGraphFaultV1> for HarnessEngineError {
    fn from(fault: HarnessVisibilityGraphFaultV1) -> Self {
        match fault {
            HarnessVisibilityGraphFaultV1::ResourceExhausted => {
                HarnessEngineError::ReadVisibilityResourceExhausted
            }
            HarnessVisibilityGraphFaultV1::InvalidGraph => {
                HarnessEngineError::ReadVisibilityInvalidGraph
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct HarnessEngine {
    tasks: BTreeMap<HarnessTaskId, HarnessTaskV1>,
    runs: BTreeMap<HarnessRunId, HarnessRunV1>,
    grants: BTreeMap<SessionGrantId, SessionGrantV1>,
    operations: BTreeMap<HarnessOperationId, HarnessOperationV1>,
    execution_specs: BTreeMap<HarnessTaskId, HarnessTaskExecutionSpecV1>,
    issuances: BTreeMap<HarnessTaskId, HarnessTaskLaunchIssuanceV1>,
    execution_specs_v2: BTreeMap<HarnessTaskId, HarnessTaskExecutionSpecV2>,
    deliveries: BTreeMap<HarnessDeliveryRef, HarnessDeliveryV1>,
    continuations: BTreeMap<HarnessContinuationRef, HarnessContinuationV1>,
    /// Bumped exactly once per accepted (`HarnessApplyOutcome::Applied`)
    /// mutation, inside `finish` -- the single choke point every real
    /// state change already passes through before becoming a
    /// `PreparedHarnessMutation` (S9/single-writer-core: this crate invents
    /// no parallel mutation-tracking mechanism, it reuses that one).
    /// `visibility_graph_cache` is valid exactly when its stored generation
    /// equals this counter. Not durable state: `checkpoint`/`restore` never
    /// touch it, and `restore` (which never goes through `finish`) starting
    /// it at 0 is exactly the right "not yet validated" baseline.
    mutation_generation: u64,
    /// Cache for the two grant-independent whole-store structural
    /// validators `read_visibility` used to redo, from scratch, over the
    /// entire task/run/operation graph, on EVERY read regardless of which
    /// grant was asking or whether anything had changed since the last
    /// read -- this crate is a single-writer core (`runtime.rs`'s own
    /// select loop owns the one `HarnessEngine`/`HarnessService` with no
    /// `Arc`/`Mutex` anywhere around it), so a `RefCell` is exactly the
    /// existing interior-mutability idiom this codebase already reaches
    /// for, not a new one. `None` means "never computed"; `Some((generation,
    /// outcome))` is reused as-is while `generation == mutation_generation`
    /// and recomputed the moment it is not.
    visibility_graph_cache: RefCell<Option<(u64, Result<(), HarnessVisibilityGraphFaultV1>)>>,
}

/// Manual, deliberately narrower than `derive`: equality is about the
/// durable task/run/operation/grant/... graph this engine carries, never
/// about `mutation_generation` or `visibility_graph_cache`, which are a
/// write-mutation counter and a read cache respectively -- process-local
/// bookkeeping, not part of what makes two engines the same durable state.
/// Without this, two engines holding identical data could compare unequal
/// purely because one of them had served a `read_visibility` call and the
/// other had not.
impl PartialEq for HarnessEngine {
    fn eq(&self, other: &Self) -> bool {
        self.tasks == other.tasks
            && self.runs == other.runs
            && self.grants == other.grants
            && self.operations == other.operations
            && self.execution_specs == other.execution_specs
            && self.issuances == other.issuances
            && self.execution_specs_v2 == other.execution_specs_v2
            && self.deliveries == other.deliveries
            && self.continuations == other.continuations
    }
}

impl Eq for HarnessEngine {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Entity-local authorization result for one grant.
///
/// These sets intentionally preserve independent task/run/operation scopes.
/// Callers projecting records must test every emitted cross-entity reference
/// with the matching predicate and redact or reject hidden references. An
/// operation is included only when its actor is in the operation scope and all
/// of its task/run targets are visible under their independent scopes. Grant
/// operations are never included; reconciliation visibility is transitively
/// closed over an already-visible reconciled operation.
pub struct HarnessReadVisibilityV1 {
    task_ids: BTreeSet<HarnessTaskId>,
    run_ids: BTreeSet<HarnessRunId>,
    operation_ids: BTreeSet<HarnessOperationId>,
}

impl HarnessReadVisibilityV1 {
    pub fn task_visible(&self, task_id: &HarnessTaskId) -> bool {
        self.task_ids.contains(task_id)
    }

    pub fn run_visible(&self, run_id: &HarnessRunId) -> bool {
        self.run_ids.contains(run_id)
    }

    pub fn operation_visible(&self, operation_id: &HarnessOperationId) -> bool {
        self.operation_ids.contains(operation_id)
    }

    pub fn task_ids(&self) -> impl Iterator<Item = &HarnessTaskId> {
        self.task_ids.iter()
    }

    pub fn run_ids(&self) -> impl Iterator<Item = &HarnessRunId> {
        self.run_ids.iter()
    }

    pub fn operation_ids(&self) -> impl Iterator<Item = &HarnessOperationId> {
        self.operation_ids.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.task_ids.is_empty() && self.run_ids.is_empty() && self.operation_ids.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct PreparedHarnessMutation {
    next: HarnessEngine,
    outcome: HarnessApplyOutcome,
    operation: HarnessOperationV1,
}

impl PreparedHarnessMutation {
    pub fn outcome(&self) -> HarnessApplyOutcome {
        self.outcome
    }

    pub fn operation(&self) -> &HarnessOperationV1 {
        &self.operation
    }

    pub fn checkpoint(&self) -> HarnessEngineCheckpointV1 {
        self.next.checkpoint()
    }

    pub fn checkpoint_deliveries(&self) -> Vec<HarnessDeliveryV1> {
        self.next.deliveries.values().cloned().collect()
    }

    pub fn checkpoint_continuations(&self) -> Vec<HarnessContinuationV1> {
        self.next.continuations.values().cloned().collect()
    }
}

impl HarnessEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn task(&self, task_id: &HarnessTaskId) -> Option<&HarnessTaskV1> {
        self.tasks.get(task_id)
    }

    pub fn run(&self, run_id: &HarnessRunId) -> Option<&HarnessRunV1> {
        self.runs.get(run_id)
    }

    pub fn grant(&self, grant_id: &SessionGrantId) -> Option<&SessionGrantV1> {
        self.grants.get(grant_id)
    }

    pub fn operation(
        &self,
        operation_id: &HarnessOperationId,
    ) -> Option<&HarnessOperationV1> {
        self.operations.get(operation_id)
    }

    pub fn execution_spec(
        &self,
        task_id: &HarnessTaskId,
    ) -> Option<&HarnessTaskExecutionSpecV1> {
        self.execution_specs.get(task_id)
    }

    pub fn execution_specs(
        &self,
    ) -> impl Iterator<Item = &HarnessTaskExecutionSpecV1> {
        self.execution_specs.values()
    }

    pub fn task_launch_issuance(
        &self,
        task_id: &HarnessTaskId,
    ) -> Option<&HarnessTaskLaunchIssuanceV1> {
        self.issuances.get(task_id)
    }

    pub fn task_execution_spec_v2(
        &self,
        task_id: &HarnessTaskId,
    ) -> Option<&HarnessTaskExecutionSpecV2> {
        self.execution_specs_v2.get(task_id)
    }

    pub fn task_launch_issuances(
        &self,
    ) -> impl Iterator<Item = &HarnessTaskLaunchIssuanceV1> {
        self.issuances.values()
    }

    pub fn task_execution_specs_v2(
        &self,
    ) -> impl Iterator<Item = &HarnessTaskExecutionSpecV2> {
        self.execution_specs_v2.values()
    }

    pub fn delivery(&self, delivery_ref: &HarnessDeliveryRef) -> Option<&HarnessDeliveryV1> {
        self.deliveries.get(delivery_ref)
    }

    pub fn continuation(
        &self,
        continuation_ref: &HarnessContinuationRef,
    ) -> Option<&HarnessContinuationV1> {
        self.continuations.get(continuation_ref)
    }

    pub fn continuation_for_run(&self, run_id: &HarnessRunId) -> Option<&HarnessContinuationV1> {
        self.continuations.values().find(|record| &record.target_run_id == run_id)
    }

    pub fn delivery_for_run(&self, run_id: &HarnessRunId) -> Option<&HarnessDeliveryV1> {
        self.deliveries.values().find(|delivery| &delivery.run_id == run_id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &HarnessTaskV1> {
        self.tasks.values()
    }

    pub fn runs(&self) -> impl Iterator<Item = &HarnessRunV1> {
        self.runs.values()
    }

    pub fn grants(&self) -> impl Iterator<Item = &SessionGrantV1> {
        self.grants.values()
    }

    pub fn operations(&self) -> impl Iterator<Item = &HarnessOperationV1> {
        self.operations.values()
    }

    pub fn deliveries(&self) -> impl Iterator<Item = &HarnessDeliveryV1> {
        self.deliveries.values()
    }

    pub fn scheduler_pending_dispatch(
        &self,
    ) -> Result<Option<(&HarnessTaskV1, &HarnessRunV1, &HarnessOperationV1)>, HarnessEngineError> {
        Ok(self.scheduler_pending_dispatches()?.into_iter().next())
    }

    pub fn scheduler_pending_dispatches(
        &self,
    ) -> Result<Vec<(&HarnessTaskV1, &HarnessRunV1, &HarnessOperationV1)>, HarnessEngineError> {
        self.scheduler_pending_dispatches_inner(None)
    }

    pub fn scheduler_pending_dispatches_for(
        &self,
        operation_ids: &BTreeSet<HarnessOperationId>,
    ) -> Result<Vec<(&HarnessTaskV1, &HarnessRunV1, &HarnessOperationV1)>, HarnessEngineError> {
        self.scheduler_pending_dispatches_inner(Some(operation_ids))
    }

    fn scheduler_pending_dispatches_inner(
        &self,
        operation_ids: Option<&BTreeSet<HarnessOperationId>>,
    ) -> Result<Vec<(&HarnessTaskV1, &HarnessRunV1, &HarnessOperationV1)>, HarnessEngineError> {
        // This walks every stored run (there is no index by lifecycle or by
        // operation id), so the cost is the number of runs actually visited,
        // not the size of the store as a whole. Refuse only once the walk
        // itself has examined `HARNESS_SCHEDULER_SCAN_MAX` runs.
        let mut pending = Vec::new();
        for (scanned, run) in self.runs.values().enumerate() {
            if scanned >= HARNESS_SCHEDULER_SCAN_MAX {
                return Err(HarnessEngineError::SchedulerResourceExhausted);
            }
            if run.lifecycle != HarnessRunLifecycleV1::Requested
                || operation_ids.map(|ids| !ids.contains(&run.operation_id)).unwrap_or(false)
            {
                continue;
            }
            let operation = self.operations.get(&run.operation_id)
                .ok_or(HarnessEngineError::SchedulerInvalidGraph)?;
            if operation.kind != HarnessOperationKindV1::CreateRun
                || operation.state != HarnessOperationStateV1::Prepared
            {
                continue;
            }
            let task = self.tasks.get(&run.task_id)
                .ok_or(HarnessEngineError::SchedulerInvalidGraph)?;
            if task.state != HarnessTaskStateV1::Running
                || task.run_ids.binary_search(&run.run_id).is_err()
                || operation.run_id.as_ref() != Some(&run.run_id)
                || operation.task_id.as_ref() != Some(&task.task_id)
            {
                return Err(HarnessEngineError::SchedulerInvalidGraph);
            }
            pending.push((task, run, operation));
        }
        Ok(pending)
    }

    /// Finds the first `Ready`, dependency-satisfied task by walking every
    /// stored task in order -- there is no index by state, so this genuinely
    /// scans. Refuses only once the walk itself has examined
    /// `HARNESS_SCHEDULER_SCAN_MAX` tasks without returning; an eligible task
    /// found earlier in the walk is returned regardless of how large the
    /// store has grown.
    pub fn scheduler_ready_task(&self) -> Result<Option<&HarnessTaskV1>, HarnessEngineError> {
        for (scanned, task) in self.tasks.values().enumerate() {
            if scanned >= HARNESS_SCHEDULER_SCAN_MAX {
                return Err(HarnessEngineError::SchedulerResourceExhausted);
            }
            if task.state == HarnessTaskStateV1::Ready && self.scheduler_task_is_eligible(task)? {
                return Ok(Some(task));
            }
        }
        Ok(None)
    }

    /// Looks up one task by id and checks its own eligibility. The cost is
    /// that task's own `dependencies` and `run_ids`, never the size of the
    /// store, so this does not consult any store-wide scan bound.
    pub fn scheduler_ready_task_by_id(
        &self,
        task_id: &HarnessTaskId,
    ) -> Result<Option<&HarnessTaskV1>, HarnessEngineError> {
        let task = self.tasks.get(task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(task_id.to_string()))?;
        if task.state != HarnessTaskStateV1::Ready || !self.scheduler_task_is_eligible(task)? {
            return Ok(None);
        }
        Ok(Some(task))
    }

    fn scheduler_task_is_eligible(
        &self,
        task: &HarnessTaskV1,
    ) -> Result<bool, HarnessEngineError> {
        let dependencies_done = task.dependencies.iter().all(|dependency_id| {
            self.tasks.get(dependency_id)
                .map(|dependency| dependency.state == HarnessTaskStateV1::Done)
                .unwrap_or(false)
        });
        Ok(dependencies_done && !self.task_has_nonterminal_run(task)?)
    }

    /// Walks one task's own `run_ids` looking for a run that has not reached
    /// a terminal lifecycle, returning its id -- the run whose lifecycle
    /// projection owns the task's own state for as long as this keeps
    /// returning `Some` (`HarnessTaskMoveResultV1::TaskOwnedByLiveRun`, D5:
    /// while a run is non-terminal, a caller-initiated move that would change
    /// its task's state races the run's own projection and never wins it
    /// cleanly, so it is refused by name instead, naming this run). The cost
    /// is that task's own `run_ids.len()`, never the size of the store, so
    /// this does not consult any store-wide scan bound.
    pub fn task_nonterminal_run_id(
        &self,
        task: &HarnessTaskV1,
    ) -> Result<Option<HarnessRunId>, HarnessEngineError> {
        for run_id in &task.run_ids {
            let run = self.runs.get(run_id).ok_or(HarnessEngineError::SchedulerInvalidGraph)?;
            if run.task_id != task.task_id {
                return Err(HarnessEngineError::SchedulerInvalidGraph);
            }
            if !matches!(
                run.lifecycle,
                HarnessRunLifecycleV1::Completed
                    | HarnessRunLifecycleV1::Failed
                    | HarnessRunLifecycleV1::Cancelled
            ) {
                return Ok(Some(run_id.clone()));
            }
        }
        Ok(None)
    }

    /// Bare presence check built on `task_nonterminal_run_id` -- most callers
    /// (the scheduler's own admission checks, `operator_cancel_task`/
    /// `operator_retry_task`) only need to know whether a nonterminal run
    /// exists, not which one.
    pub fn task_has_nonterminal_run(
        &self,
        task: &HarnessTaskV1,
    ) -> Result<bool, HarnessEngineError> {
        Ok(self.task_nonterminal_run_id(task)?.is_some())
    }

    /// The subset of `task.dependencies` that `scheduler_task_is_eligible`'s
    /// `dependencies_done` check would treat as unmet -- a dependency id that
    /// does not resolve to a stored task, or one that resolves but has not
    /// reached `HarnessTaskStateV1::Done`. This never decides admission itself
    /// (that stays with `scheduler_task_is_eligible`, reached only through
    /// `scheduler_ready_task`/`scheduler_ready_task_by_id`); it exists so a
    /// caller that already knows a task was refused can name which specific
    /// dependency id it is still waiting on instead of reporting a bare
    /// ineligibility.
    pub fn unmet_task_dependencies(&self, task: &HarnessTaskV1) -> Vec<HarnessTaskId> {
        task.dependencies.iter()
            .filter(|dependency_id| {
                !self.tasks.get(*dependency_id)
                    .map(|dependency| dependency.state == HarnessTaskStateV1::Done)
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    pub fn restore_deliveries(
        mut self,
        deliveries: Vec<HarnessDeliveryV1>,
    ) -> Result<Self, HarnessEngineError> {
        if deliveries.len() > HARNESS_DELIVERIES_MAX {
            return Err(HarnessEngineError::DeliveryCapacityExceeded);
        }
        for delivery in deliveries {
            delivery.validate()?;
            let delivery_ref = delivery.delivery_ref.clone();
            if self.deliveries.insert(delivery_ref.clone(), delivery).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    delivery_ref.to_string(),
                ));
            }
        }
        self.validate_links()?;
        Ok(self)
    }

    pub fn read_visibility(
        &self,
        grant_id: &SessionGrantId,
    ) -> Result<HarnessReadVisibilityV1, HarnessEngineError> {
        let Some(grant) = self.grants.get(grant_id) else {
            return Ok(HarnessReadVisibilityV1::default());
        };
        if grant.state != SessionGrantStateV1::Active || grant.validate().is_err() {
            return Ok(HarnessReadVisibilityV1::default());
        }
        if !self.runs.contains_key(&grant.actor_run_id) {
            return Ok(HarnessReadVisibilityV1::default());
        }
        if grant.read_permissions == Default::default() {
            return Ok(HarnessReadVisibilityV1::default());
        }

        self.visibility_graph_validation()?;

        let task_attribution_runs = self.visible_runs_for_scope(
            grant.read_permissions.tasks,
            &grant.actor_run_id,
        )?;
        let run_ids = self.visible_runs_for_scope(
            grant.read_permissions.runs,
            &grant.actor_run_id,
        )?;
        let operation_actor_runs = self.visible_runs_for_scope(
            grant.read_permissions.operations,
            &grant.actor_run_id,
        )?;

        let task_ids = self.tasks_attributed_to_runs(&task_attribution_runs);
        let operation_ids = self.visible_operations(
            &operation_actor_runs,
            &task_ids,
            &run_ids,
        )?;

        Ok(HarnessReadVisibilityV1 {
            task_ids,
            run_ids,
            operation_ids,
        })
    }

    pub fn checkpoint(&self) -> HarnessEngineCheckpointV1 {
        HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: self.tasks.values().cloned().collect(),
            runs: self.runs.values().cloned().collect(),
            grants: self.grants.values().cloned().collect(),
            operations: self.operations.values().cloned().collect(),
            execution_specs: self.execution_specs.values().cloned().collect(),
            issuances: self.issuances.values().cloned().collect(),
            execution_specs_v2: self.execution_specs_v2.values().cloned().collect(),
            deliveries: self.deliveries.values().cloned().collect(),
            continuations: self.continuations.values().cloned().collect(),
        }
    }

    pub fn restore(
        checkpoint: HarnessEngineCheckpointV1,
    ) -> Result<Self, HarnessEngineError> {
        if checkpoint.version != HARNESS_ENGINE_CHECKPOINT_VERSION_V1 {
            return Err(HarnessEngineError::UnsupportedCheckpoint(checkpoint.version));
        }
        let mut engine = Self::new();
        for task in checkpoint.tasks {
            task.validate()?;
            let task_id = task.task_id.clone();
            if engine.tasks.insert(task_id.clone(), task).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(task_id.to_string()));
            }
        }
        for run in checkpoint.runs {
            run.validate()?;
            let run_id = run.run_id.clone();
            if engine.runs.insert(run_id.clone(), run).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(run_id.to_string()));
            }
        }
        for grant in checkpoint.grants {
            grant.validate()?;
            let grant_id = grant.grant_id.clone();
            if engine.grants.insert(grant_id.clone(), grant).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(grant_id.to_string()));
            }
        }
        for operation in checkpoint.operations {
            operation.validate()?;
            let operation_id = operation.operation_id.clone();
            if engine.operations.insert(operation_id.clone(), operation).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(operation_id.to_string()));
            }
        }
        for spec in checkpoint.execution_specs {
            validate_execution_spec_identity(&spec)?;
            let task_id = spec.task_id.clone();
            if engine.execution_specs.insert(task_id.clone(), spec).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    task_id.to_string(),
                ));
            }
        }
        for issuance in checkpoint.issuances {
            issuance.validate()?;
            let task_id = issuance.task_id.clone();
            if engine.execution_specs.contains_key(&task_id) {
                return Err(HarnessEngineError::DualExecutionSpec(task_id.to_string()));
            }
            if engine.issuances.insert(task_id.clone(), issuance).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    task_id.to_string(),
                ));
            }
        }
        for spec in checkpoint.execution_specs_v2 {
            spec.validate()?;
            let task_id = spec.task_id.clone();
            if engine.execution_specs.contains_key(&task_id) {
                return Err(HarnessEngineError::DualExecutionSpec(task_id.to_string()));
            }
            if engine.execution_specs_v2.insert(task_id.clone(), spec).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    task_id.to_string(),
                ));
            }
        }
        if checkpoint.deliveries.len() > HARNESS_DELIVERIES_MAX {
            return Err(HarnessEngineError::DeliveryCapacityExceeded);
        }
        for delivery in checkpoint.deliveries {
            delivery.validate()?;
            let delivery_ref = delivery.delivery_ref.clone();
            if engine.deliveries.insert(delivery_ref.clone(), delivery).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    delivery_ref.to_string(),
                ));
            }
        }
        if checkpoint.continuations.len() > HARNESS_CONTINUATIONS_MAX {
            return Err(HarnessEngineError::ContinuationCapacityExceeded);
        }
        for continuation in checkpoint.continuations {
            continuation.validate()?;
            let continuation_ref = continuation.continuation_ref.clone();
            if engine.continuations.insert(continuation_ref.clone(), continuation).is_some() {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    continuation_ref.to_string(),
                ));
            }
        }
        engine.validate_links()?;
        Ok(engine)
    }

    pub fn prepare(
        &self,
        mutation: HarnessMutationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        mutation.validate_payload()?;
        let requested_operation = mutation.operation();
        if let Some(existing) = self.operations.get(&requested_operation.operation_id) {
            let issued_projection_matches = match &mutation {
                HarnessMutationV1::CreateIssuedRun {
                    expected_execution_spec_revision,
                    expected_issuance_revision,
                    task,
                    run,
                    delivery,
                    continuation,
                    ..
                } => self.same_issued_run_projection(
                    *expected_execution_spec_revision,
                    *expected_issuance_revision,
                    task,
                    run,
                    delivery.as_ref(),
                    continuation.as_ref(),
                ),
                _ => true,
            };
            if !same_operation_request(existing, requested_operation)
                || !issued_projection_matches
            {
                return Err(HarnessEngineError::OperationIdConflict {
                    operation_id: requested_operation.operation_id.clone(),
                });
            }
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation: existing.clone(),
            });
        }
        self.validate_actor(&requested_operation.actor)?;
        require_first_revision(requested_operation.revision, "operation")?;

        let mut next = self.clone();
        match mutation {
            HarnessMutationV1::CreateTask { operation, task } => {
                require_kind(operation.kind, HarnessOperationKindV1::CreateTask)?;
                require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
                task.validate()?;
                require_first_revision(task.revision, "task")?;
                require_same_id(operation.task_id.as_ref(), &task.task_id, "task")?;
                if task.creator != operation.actor {
                    return Err(HarnessEngineError::MismatchedIdentity("task creator"));
                }
                if next.tasks.contains_key(&task.task_id) {
                    return Err(HarnessEngineError::AlreadyExists(task.task_id.to_string()));
                }
                next.validate_task_links(&task)?;
                next.tasks.insert(task.task_id.clone(), task);
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::ReplaceTask { operation, expected_revision, task } => {
                require_kind(operation.kind, HarnessOperationKindV1::MutateTask)?;
                require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
                require_operation_expected(&operation, expected_revision)?;
                task.validate()?;
                require_same_id(operation.task_id.as_ref(), &task.task_id, "task")?;
                let current = next.tasks.get(&task.task_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(task.task_id.to_string()))?;
                validate_replacement(
                    "task",
                    current.revision,
                    expected_revision,
                    task.revision,
                    current.created_at_unix_ms,
                    task.created_at_unix_ms,
                )?;
                if current.creator != task.creator {
                    return Err(HarnessEngineError::MismatchedIdentity("immutable task creator"));
                }
                next.validate_task_links(&task)?;
                next.tasks.insert(task.task_id.clone(), task);
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::PutExecutionSpec {
                operation,
                expected_task_revision,
                expected_spec_revision,
                spec,
            } => {
                require_kind(
                    operation.kind,
                    HarnessOperationKindV1::MutateExecutionSpec,
                )?;
                require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
                require_operation_expected(&operation, expected_task_revision)?;
                validate_execution_spec_identity(&spec)?;
                require_same_id(operation.task_id.as_ref(), &spec.task_id, "task")?;
                let task = next.tasks.get(&spec.task_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(spec.task_id.to_string()))?;
                if task.revision != expected_task_revision {
                    return Err(HarnessEngineError::ExpectedRevisionMismatch {
                        entity: "task",
                        expected: expected_task_revision.get(),
                        actual: Some(task.revision.get()),
                    });
                }
                match (
                    expected_spec_revision,
                    next.execution_specs.get(&spec.task_id),
                ) {
                    (HarnessExpectedExecutionSpecRevisionV1::Absent, None) => {
                        require_first_revision(spec.revision, "execution spec")?;
                    }
                    (HarnessExpectedExecutionSpecRevisionV1::Exact(expected), Some(current)) => {
                        validate_replacement(
                            "execution spec",
                            current.revision,
                            expected,
                            spec.revision,
                            current.created_at_unix_ms,
                            spec.created_at_unix_ms,
                        )?;
                        if current.execution_spec_id != spec.execution_spec_id
                            || current.task_id != spec.task_id
                        {
                            return Err(HarnessEngineError::MismatchedIdentity(
                                "immutable execution spec identity",
                            ));
                        }
                    }
                    (HarnessExpectedExecutionSpecRevisionV1::Absent, Some(current)) => {
                        return Err(HarnessEngineError::ExpectedRevisionMismatch {
                            entity: "execution spec",
                            expected: 0,
                            actual: Some(current.revision.get()),
                        });
                    }
                    (HarnessExpectedExecutionSpecRevisionV1::Exact(expected), current) => {
                        return Err(HarnessEngineError::ExpectedRevisionMismatch {
                            entity: "execution spec",
                            expected: expected.get(),
                            actual: current.map(|spec| spec.revision.get()),
                        });
                    }
                }
                next.execution_specs.insert(spec.task_id.clone(), spec);
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::PutIssuedExecutionSpec {
                operation,
                expected_task_revision,
                expected_spec_revision,
                expected_issuance_revision,
                issuance,
                spec,
            } => {
                require_kind(
                    operation.kind,
                    HarnessOperationKindV1::MutateExecutionSpec,
                )?;
                require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
                require_operation_expected(&operation, expected_task_revision)?;
                issuance.validate()?;
                spec.validate()?;
                require_same_id(operation.task_id.as_ref(), &issuance.task_id, "task")?;
                if spec.task_id != issuance.task_id {
                    return Err(HarnessEngineError::MismatchedIdentity(
                        "issued execution spec task",
                    ));
                }
                let task = next.tasks.get(&issuance.task_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(issuance.task_id.to_string()))?;
                if task.revision != expected_task_revision
                    || issuance.task_revision != expected_task_revision
                {
                    return Err(HarnessEngineError::ExpectedRevisionMismatch {
                        entity: "task",
                        expected: expected_task_revision.get(),
                        actual: Some(task.revision.get()),
                    });
                }
                if next.execution_specs.contains_key(&issuance.task_id) {
                    return Err(HarnessEngineError::DualExecutionSpec(
                        issuance.task_id.to_string(),
                    ));
                }
                validate_issued_execution_spec_link(&issuance, &spec)?;
                match (
                    expected_spec_revision,
                    expected_issuance_revision,
                    next.execution_specs_v2.get(&issuance.task_id),
                    next.issuances.get(&issuance.task_id),
                ) {
                    (
                        HarnessExpectedExecutionSpecRevisionV1::Absent,
                        HarnessExpectedExecutionSpecRevisionV1::Absent,
                        None,
                        None,
                    ) => {
                        require_first_revision(spec.revision, "issued execution spec")?;
                        require_first_revision(issuance.revision, "task launch issuance")?;
                    }
                    (
                        HarnessExpectedExecutionSpecRevisionV1::Exact(expected_spec),
                        HarnessExpectedExecutionSpecRevisionV1::Exact(expected_issuance),
                        Some(current_spec),
                        Some(current_issuance),
                    ) => {
                        validate_replacement(
                            "issued execution spec",
                            current_spec.revision,
                            expected_spec,
                            spec.revision,
                            current_spec.created_at_unix_ms,
                            spec.created_at_unix_ms,
                        )?;
                        validate_replacement(
                            "task launch issuance",
                            current_issuance.revision,
                            expected_issuance,
                            issuance.revision,
                            current_issuance.created_at_unix_ms,
                            issuance.created_at_unix_ms,
                        )?;
                        if current_spec.execution_spec_id != spec.execution_spec_id
                            || current_spec.task_id != spec.task_id
                            || current_issuance.issuance_id != issuance.issuance_id
                            || current_issuance.task_id != issuance.task_id
                        {
                            return Err(HarnessEngineError::MismatchedIdentity(
                                "immutable issued execution identity",
                            ));
                        }
                    }
                    (_, _, current_spec, current_issuance) => {
                        return Err(HarnessEngineError::IssuedExecutionCasMismatch {
                            spec_revision: current_spec.map(|record| record.revision.get()),
                            issuance_revision: current_issuance
                                .map(|record| record.revision.get()),
                        });
                    }
                }
                next.execution_specs_v2.insert(spec.task_id.clone(), spec);
                next.issuances.insert(issuance.task_id.clone(), issuance);
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::CreateRun {
                operation,
                expected_task_revision,
                task,
                run,
            } => {
                require_kind(operation.kind, HarnessOperationKindV1::CreateRun)?;
                require_operation_state(operation.state, HarnessOperationStateV1::Prepared)?;
                if run.lifecycle != gate4agent_harness_protocol::HarnessRunLifecycleV1::Requested
                    || run.binding.is_some()
                {
                    return Err(HarnessEngineError::InvalidInitialRunState);
                }
                require_operation_expected(&operation, expected_task_revision)?;
                task.validate()?;
                run.validate()?;
                require_first_revision(run.revision, "run")?;
                require_same_id(operation.run_id.as_ref(), &run.run_id, "run")?;
                require_same_id(operation.task_id.as_ref(), &run.task_id, "task")?;
                if operation.operation_id != run.operation_id {
                    return Err(HarnessEngineError::MismatchedIdentity("run operation"));
                }
                if next.runs.contains_key(&run.run_id) {
                    return Err(HarnessEngineError::AlreadyExists(run.run_id.to_string()));
                }
                let current_task = next.tasks.get(&run.task_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(run.task_id.to_string()))?;
                validate_replacement(
                    "task",
                    current_task.revision,
                    expected_task_revision,
                    task.revision,
                    current_task.created_at_unix_ms,
                    task.created_at_unix_ms,
                )?;
                if task.task_id != run.task_id || task.run_ids.binary_search(&run.run_id).is_err() {
                    return Err(HarnessEngineError::MismatchedIdentity("task run link"));
                }
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.validate_run_links(&run)?;
                next.tasks.insert(task.task_id.clone(), task);
                next.runs.insert(run.run_id.clone(), run);
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::CreateIssuedRun {
                operation,
                expected_task_revision,
                expected_execution_spec_revision,
                expected_issuance_revision,
                task,
                run,
                delivery,
                continuation,
            } => {
                require_kind(operation.kind, HarnessOperationKindV1::CreateRun)?;
                require_operation_state(operation.state, HarnessOperationStateV1::Prepared)?;
                require_operation_expected(&operation, expected_task_revision)?;
                if run.lifecycle != HarnessRunLifecycleV1::Requested || run.binding.is_some() {
                    return Err(HarnessEngineError::InvalidInitialRunState);
                }
                require_first_revision(run.revision, "run")?;
                require_same_id(operation.run_id.as_ref(), &run.run_id, "run")?;
                require_same_id(operation.task_id.as_ref(), &run.task_id, "task")?;
                if operation.operation_id != run.operation_id {
                    return Err(HarnessEngineError::MismatchedIdentity("run operation"));
                }
                if next.runs.contains_key(&run.run_id) {
                    return Err(HarnessEngineError::AlreadyExists(run.run_id.to_string()));
                }
                let current_task = next.tasks.get(&run.task_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(run.task_id.to_string()))?;
                let current_spec = next.execution_specs_v2.get(&run.task_id)
                    .ok_or(HarnessEngineError::IssuedExecutionAuthorityMissing)?;
                let current_issuance = next.issuances.get(&run.task_id)
                    .ok_or(HarnessEngineError::IssuedExecutionAuthorityMissing)?;
                if current_spec.revision != expected_execution_spec_revision
                    || current_issuance.revision != expected_issuance_revision
                    || current_issuance.task_revision != expected_task_revision
                {
                    return Err(HarnessEngineError::IssuedExecutionCasMismatch {
                        spec_revision: Some(current_spec.revision.get()),
                        issuance_revision: Some(current_issuance.revision.get()),
                    });
                }
                validate_issued_execution_spec_link(current_issuance, current_spec)?;
                if operation.actor != current_task.creator {
                    return Err(HarnessEngineError::MismatchedIdentity(
                        "issued run operator actor",
                    ));
                }
                validate_issued_run_task_projection(
                    current_task,
                    expected_task_revision,
                    &task,
                    &run,
                )?;
                validate_operator_issuance_run_intent(current_issuance, &run)?;
                let authority = HarnessTransferAuthorityRefV1::OperatorIssuance {
                    issuance: current_issuance.reference(),
                };
                if delivery.is_some() != current_issuance.delivery.is_some()
                    || continuation.is_some() != current_issuance.context_source.is_some()
                    || delivery.as_ref().is_some_and(|record| record.authority != authority)
                    || continuation.as_ref().is_some_and(|record| record.authority != authority)
                {
                    return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
                }
                if delivery.is_some() && next.deliveries.len() >= HARNESS_DELIVERIES_MAX {
                    return Err(HarnessEngineError::DeliveryCapacityExceeded);
                }
                if continuation.is_some()
                    && next.continuations.len() >= HARNESS_CONTINUATIONS_MAX
                {
                    return Err(HarnessEngineError::ContinuationCapacityExceeded);
                }
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.tasks.insert(task.task_id.clone(), task);
                next.runs.insert(run.run_id.clone(), run);
                if let Some(delivery) = delivery {
                    if delivery.state != HarnessDeliveryStateV1::Prepared {
                        return Err(HarnessEngineError::InvalidInitialDeliveryState);
                    }
                    require_first_revision(delivery.revision, "delivery")?;
                    if next.deliveries.contains_key(&delivery.delivery_ref)
                        || next.deliveries.values()
                            .any(|record| record.run_id == delivery.run_id)
                    {
                        return Err(HarnessEngineError::DeliveryIdConflict {
                            delivery_ref: delivery.delivery_ref,
                        });
                    }
                    next.validate_delivery_authority(&delivery)?;
                    next.deliveries.insert(delivery.delivery_ref.clone(), delivery);
                }
                if let Some(continuation) = continuation {
                    if continuation.state != HarnessContinuationStateV1::Prepared {
                        return Err(HarnessEngineError::InvalidInitialContinuationState);
                    }
                    require_first_revision(continuation.revision, "continuation")?;
                    if next.continuations.contains_key(&continuation.continuation_ref)
                        || next.continuations.values()
                            .any(|record| record.target_run_id == continuation.target_run_id)
                        || next.continuations.values()
                            .any(|record| record.receipt_ref == continuation.receipt_ref)
                    {
                        return Err(HarnessEngineError::ContinuationIdConflict {
                            continuation_ref: continuation.continuation_ref,
                        });
                    }
                    next.validate_continuation_authority(&continuation)?;
                    next.continuations.insert(
                        continuation.continuation_ref.clone(),
                        continuation,
                    );
                }
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::ReplaceRun { operation, expected_revision, run } => {
                require_kind(operation.kind, HarnessOperationKindV1::MutateRun)?;
                require_operation_expected(&operation, expected_revision)?;
                run.validate()?;
                require_same_id(operation.run_id.as_ref(), &run.run_id, "run")?;
                let current = next.runs.get(&run.run_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
                validate_replacement(
                    "run",
                    current.revision,
                    expected_revision,
                    run.revision,
                    current.created_at_unix_ms,
                    run.created_at_unix_ms,
                )?;
                if current.task_id != run.task_id || current.operation_id != run.operation_id {
                    return Err(HarnessEngineError::MismatchedIdentity("immutable run link"));
                }
                validate_run_immutable(current, &run)?;
                validate_run_lifecycle_transition(current, &run)?;
                validate_generic_run_operation(current, &run, &operation)?;
                next.validate_run_links(&run)?;
                next.runs.insert(run.run_id.clone(), run);
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::CreateGrant { operation, grant } => {
                require_kind(operation.kind, HarnessOperationKindV1::CreateGrant)?;
                require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
                grant.validate()?;
                if grant.state != gate4agent_harness_protocol::SessionGrantStateV1::Active {
                    return Err(HarnessEngineError::GrantMustStartActive);
                }
                require_first_revision(grant.revision, "grant")?;
                require_same_id(operation.grant_id.as_ref(), &grant.grant_id, "grant")?;
                if next.grants.contains_key(&grant.grant_id) {
                    return Err(HarnessEngineError::AlreadyExists(grant.grant_id.to_string()));
                }
                if !next.runs.contains_key(&grant.actor_run_id) {
                    return Err(HarnessEngineError::NotFound(grant.actor_run_id.to_string()));
                }
                next.grants.insert(grant.grant_id.clone(), grant);
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
            HarnessMutationV1::ReplaceGrant { operation, expected_revision, grant } => {
                if !matches!(operation.kind, HarnessOperationKindV1::MutateGrant | HarnessOperationKindV1::RevokeGrant) {
                    return Err(HarnessEngineError::WrongOperationKind);
                }
                require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
                require_operation_expected(&operation, expected_revision)?;
                grant.validate()?;
                require_same_id(operation.grant_id.as_ref(), &grant.grant_id, "grant")?;
                let current = next.grants.get(&grant.grant_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(grant.grant_id.to_string()))?;
                validate_replacement(
                    "grant",
                    current.revision,
                    expected_revision,
                    grant.revision,
                    current.created_at_unix_ms,
                    grant.created_at_unix_ms,
                )?;
                if current.actor_run_id != grant.actor_run_id {
                    return Err(HarnessEngineError::MismatchedIdentity("grant actor run"));
                }
                validate_grant_transition(current, &grant, operation.kind)?;
                next.grants.insert(grant.grant_id.clone(), grant);
                next.operations.insert(operation.operation_id.clone(), operation.clone());
                next.finish(operation, HarnessApplyOutcome::Applied)
            }
        }
    }

    pub fn prepare_delivery(
        &self,
        delivery: HarnessDeliveryV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        delivery.validate()?;
        if delivery.state != HarnessDeliveryStateV1::Prepared {
            return Err(HarnessEngineError::InvalidInitialDeliveryState);
        }
        require_first_revision(delivery.revision, "delivery")?;
        if let Some(current) = self.deliveries.get(&delivery.delivery_ref) {
            if current == &delivery {
                let operation = self.delivery_operation(current)?.clone();
                return Ok(PreparedHarnessMutation {
                    next: self.clone(),
                    outcome: HarnessApplyOutcome::Replayed,
                    operation,
                });
            }
            return Err(HarnessEngineError::DeliveryIdConflict {
                delivery_ref: delivery.delivery_ref,
            });
        }
        if self.deliveries.len() >= HARNESS_DELIVERIES_MAX {
            return Err(HarnessEngineError::DeliveryCapacityExceeded);
        }
        self.validate_delivery_authority(&delivery)?;
        let operation = self.delivery_operation(&delivery)?.clone();
        let mut next = self.clone();
        next.deliveries.insert(delivery.delivery_ref.clone(), delivery);
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_continuation(
        &self,
        continuation: HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        continuation.validate()?;
        if continuation.state != HarnessContinuationStateV1::Prepared {
            return Err(HarnessEngineError::InvalidInitialContinuationState);
        }
        require_first_revision(continuation.revision, "continuation")?;
        if let Some(current) = self.continuations.get(&continuation.continuation_ref) {
            if current == &continuation {
                return self.finish_continuation_replay(current);
            }
            return Err(HarnessEngineError::ContinuationIdConflict {
                continuation_ref: continuation.continuation_ref,
            });
        }
        if self.continuations.len() >= HARNESS_CONTINUATIONS_MAX {
            return Err(HarnessEngineError::ContinuationCapacityExceeded);
        }
        let operation = self.continuation_operation(&continuation)?.clone();
        let mut next = self.clone();
        next.validate_continuation_authority(&continuation)?;
        next.continuations.insert(continuation.continuation_ref.clone(), continuation);
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_scheduled_run_authorities(
        &self,
        operation_id: &HarnessOperationId,
        delivery: Option<HarnessDeliveryV1>,
        continuation: Option<HarnessContinuationV1>,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        let operation = self.operations.get(operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(operation_id.to_string()))?
            .clone();
        let run = operation.run_id.as_ref()
            .and_then(|run_id| self.runs.get(run_id))
            .ok_or(HarnessEngineError::SchedulerInvalidGraph)?;
        if operation.state != HarnessOperationStateV1::Prepared
            || run.lifecycle != HarnessRunLifecycleV1::Requested
        {
            return Err(HarnessEngineError::DeliveryAuthorityWindowClosed);
        }
        let mut next = self.clone();
        let mut applied = false;
        if let Some(delivery) = delivery {
            delivery.validate()?;
            if delivery.operation_id != *operation_id
                || delivery.state != HarnessDeliveryStateV1::Prepared
                || delivery.revision.get() != 1
            {
                return Err(HarnessEngineError::InvalidInitialDeliveryState);
            }
            match self.deliveries.get(&delivery.delivery_ref) {
                Some(current) if current == &delivery => {}
                Some(_) => return Err(HarnessEngineError::DeliveryIdConflict {
                    delivery_ref: delivery.delivery_ref,
                }),
                None => {
                    if next.deliveries.len() >= HARNESS_DELIVERIES_MAX {
                        return Err(HarnessEngineError::DeliveryCapacityExceeded);
                    }
                    next.validate_delivery_authority(&delivery)?;
                    next.deliveries.insert(delivery.delivery_ref.clone(), delivery);
                    applied = true;
                }
            }
        }
        if let Some(continuation) = continuation {
            continuation.validate()?;
            if continuation.operation_id != *operation_id
                || continuation.state != HarnessContinuationStateV1::Prepared
                || continuation.revision.get() != 1
            {
                return Err(HarnessEngineError::InvalidInitialContinuationState);
            }
            match self.continuations.get(&continuation.continuation_ref) {
                Some(current) if current == &continuation => {}
                Some(_) => return Err(HarnessEngineError::ContinuationIdConflict {
                    continuation_ref: continuation.continuation_ref,
                }),
                None => {
                    if next.continuations.len() >= HARNESS_CONTINUATIONS_MAX {
                        return Err(HarnessEngineError::ContinuationCapacityExceeded);
                    }
                    next.validate_continuation_authority(&continuation)?;
                    next.continuations.insert(
                        continuation.continuation_ref.clone(),
                        continuation,
                    );
                    applied = true;
                }
            }
        }
        next.finish(
            operation,
            if applied {
                HarnessApplyOutcome::Applied
            } else {
                HarnessApplyOutcome::Replayed
            },
        )
    }

    pub fn prepare_continuation_export(
        &self,
        expected_revision: HarnessRevision,
        continuation: HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        continuation.validate()?;
        let current = self.continuations.get(&continuation.continuation_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.continuation_ref.to_string()))?;
        if current == &continuation {
            return self.finish_continuation_replay(current);
        }
        validate_continuation_replacement(current, expected_revision, &continuation)?;
        if current.state != HarnessContinuationStateV1::Exporting
            || continuation.state != HarnessContinuationStateV1::Exported
        {
            return Err(HarnessEngineError::InvalidContinuationTransition);
        }
        let operation = self.continuation_operation(&continuation)?.clone();
        let current_run = self.runs.get(&continuation.target_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(
                continuation.target_run_id.to_string(),
            ))?;
        if current_run.continuation_receipt.is_some() {
            return Err(HarnessEngineError::InvalidContinuationRunBind);
        }
        let mut linked_run = current_run.clone();
        linked_run.revision = HarnessRevision::new(
            current_run.revision.get().checked_add(1)
                .ok_or(HarnessEngineError::InvalidNextRevision { entity: "run" })?,
        )?;
        linked_run.continuation_receipt = Some(continuation.receipt_ref.clone());
        linked_run.updated_at_unix_ms = continuation.updated_at_unix_ms;
        linked_run.validate()?;
        let mut next = self.clone();
        next.continuations.insert(
            continuation.continuation_ref.clone(),
            continuation.clone(),
        );
        next.runs.insert(linked_run.run_id.clone(), linked_run);
        next.validate_continuation_authority(&continuation)?;
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_continuation_export_begin(
        &self,
        expected_revision: HarnessRevision,
        continuation: HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        continuation.validate()?;
        let current = self.continuations.get(&continuation.continuation_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.continuation_ref.to_string()))?;
        if current == &continuation {
            return self.finish_continuation_replay(current);
        }
        validate_continuation_replacement(current, expected_revision, &continuation)?;
        if current.state != HarnessContinuationStateV1::Prepared
            || continuation.state != HarnessContinuationStateV1::Exporting
        {
            return Err(HarnessEngineError::InvalidContinuationTransition);
        }
        self.validate_continuation_authority(&continuation)?;
        let operation = self.continuation_operation(&continuation)?.clone();
        let mut next = self.clone();
        next.continuations.insert(continuation.continuation_ref.clone(), continuation);
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_continuation_export_outcome_unknown(
        &self,
        expected_revision: HarnessRevision,
        continuation: HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        continuation.validate()?;
        let current = self.continuations.get(&continuation.continuation_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.continuation_ref.to_string()))?;
        if current == &continuation {
            return self.finish_continuation_replay(current);
        }
        validate_continuation_replacement(current, expected_revision, &continuation)?;
        if current.state != HarnessContinuationStateV1::Exporting
            || continuation.state != HarnessContinuationStateV1::OutcomeUnknown
        {
            return Err(HarnessEngineError::InvalidContinuationTransition);
        }
        let operation = self.continuation_operation(&continuation)?.clone();
        let mut next = self.clone();
        next.continuations.insert(continuation.continuation_ref.clone(), continuation);
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_continuation_expiry(
        &self,
        expected_revision: HarnessRevision,
        continuation: HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        continuation.validate()?;
        let current = self.continuations.get(&continuation.continuation_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.continuation_ref.to_string()))?;
        if current == &continuation {
            return self.finish_continuation_replay(current);
        }
        validate_continuation_replacement(current, expected_revision, &continuation)?;
        if !matches!(
            current.state,
            HarnessContinuationStateV1::Prepared
                | HarnessContinuationStateV1::Exporting
                | HarnessContinuationStateV1::Exported
                | HarnessContinuationStateV1::OutcomeUnknown
        )
            || continuation.state != HarnessContinuationStateV1::Expired
        {
            return Err(HarnessEngineError::InvalidContinuationTransition);
        }
        let operation = self.continuation_operation(&continuation)?.clone();
        let mut next = self.clone();
        next.continuations.insert(continuation.continuation_ref.clone(), continuation);
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_accepted_spawn_continuation_bind(
        &self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_continuation_revision: HarnessRevision,
        continuation: HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        continuation.validate()?;
        run.validate()?;
        operation.validate()?;
        let current_continuation = self.continuations.get(&continuation.continuation_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.continuation_ref.to_string()))?;
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        let current_operation = self.operations.get(&operation.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(operation.operation_id.to_string()))?;
        if current_continuation == &continuation
            && current_run == &run
            && current_operation == &operation
        {
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation,
            });
        }
        validate_continuation_replacement(
            current_continuation,
            expected_continuation_revision,
            &continuation,
        )?;
        if current_continuation.state != HarnessContinuationStateV1::Exported
            || continuation.state != HarnessContinuationStateV1::Bound
        {
            return Err(HarnessEngineError::InvalidContinuationTransition);
        }
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        validate_run_immutable(current_run, &run)?;
        validate_operation_transition(current_operation, expected_operation_revision, &operation)?;
        validate_run_lifecycle_transition(current_run, &run)?;
        validate_coupled_run_operation(current_run, &run, &operation)?;
        if continuation.target_run_id != run.run_id
            || continuation.operation_id != operation.operation_id
            || current_run.continuation_receipt.as_ref() != Some(&continuation.receipt_ref)
            || run.continuation_receipt.as_ref() != Some(&continuation.receipt_ref)
            || continuation.target_binding.as_ref() != run.binding.as_ref()
            || current_run.delivery_receipt != run.delivery_receipt
            || current_run.result_disposition != run.result_disposition
            || current_run.failure != run.failure
        {
            return Err(HarnessEngineError::InvalidContinuationRunBind);
        }
        let mut next = self.clone();
        next.continuations.insert(continuation.continuation_ref.clone(), continuation);
        next.runs.insert(run.run_id.clone(), run);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_accepted_spawn_delivery_and_continuation_commit(
        &self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_delivery_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
        expected_continuation_revision: HarnessRevision,
        continuation: HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        run.validate()?;
        operation.validate()?;
        delivery.validate()?;
        continuation.validate()?;
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        let current_operation = self.operations.get(&operation.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(operation.operation_id.to_string()))?;
        let current_delivery = self.deliveries.get(&delivery.delivery_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.delivery_ref.to_string()))?;
        let current_continuation = self.continuations.get(&continuation.continuation_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.continuation_ref.to_string()))?;
        if current_run == &run
            && current_operation == &operation
            && current_delivery == &delivery
            && current_continuation == &continuation
        {
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation,
            });
        }
        validate_delivery_replacement(current_delivery, expected_delivery_revision, &delivery)?;
        validate_continuation_replacement(
            current_continuation,
            expected_continuation_revision,
            &continuation,
        )?;
        if current_delivery.state != HarnessDeliveryStateV1::Staged
            || delivery.state != HarnessDeliveryStateV1::Committed
            || current_continuation.state != HarnessContinuationStateV1::Exported
            || continuation.state != HarnessContinuationStateV1::Bound
        {
            return Err(HarnessEngineError::InvalidCombinedSpawnCommit);
        }
        let delivery_receipt = delivery.receipt.as_ref()
            .ok_or(HarnessEngineError::InvalidCombinedSpawnCommit)?;
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        validate_run_immutable(current_run, &run)?;
        validate_operation_transition(current_operation, expected_operation_revision, &operation)?;
        validate_run_lifecycle_transition(current_run, &run)?;
        validate_coupled_run_operation(current_run, &run, &operation)?;
        if delivery.run_id != run.run_id
            || continuation.target_run_id != run.run_id
            || delivery.operation_id != operation.operation_id
            || continuation.operation_id != operation.operation_id
            || current_run.delivery_receipt.is_some()
            || current_run.continuation_receipt.as_ref() != Some(&continuation.receipt_ref)
            || run.delivery_receipt.as_ref() != Some(&delivery_receipt.receipt_ref)
            || run.continuation_receipt.as_ref() != Some(&continuation.receipt_ref)
            || run.binding.as_ref() != Some(&delivery_receipt.binding)
            || continuation.target_binding.as_ref() != run.binding.as_ref()
            || current_run.result_disposition != run.result_disposition
            || current_run.failure != run.failure
        {
            return Err(HarnessEngineError::InvalidCombinedSpawnCommit);
        }
        let mut next = self.clone();
        next.deliveries.insert(delivery.delivery_ref.clone(), delivery);
        next.continuations.insert(continuation.continuation_ref.clone(), continuation);
        next.runs.insert(run.run_id.clone(), run);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_delivery_stage(
        &self,
        expected_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        delivery.validate()?;
        let current = self.deliveries.get(&delivery.delivery_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.delivery_ref.to_string()))?;
        if current == &delivery {
            let operation = self.delivery_operation(current)?.clone();
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation,
            });
        }
        match (current.state, delivery.state) {
            (HarnessDeliveryStateV1::Prepared, HarnessDeliveryStateV1::Staged) => {
                validate_delivery_replacement(current, expected_revision, &delivery)?;
            }
            (HarnessDeliveryStateV1::Staged, HarnessDeliveryStateV1::Staged) => {
                validate_delivery_restage(current, expected_revision, &delivery)?;
            }
            _ => return Err(HarnessEngineError::InvalidDeliveryTransition),
        }
        self.validate_delivery_authority(&delivery)?;
        let operation = self.delivery_operation(&delivery)?.clone();
        let mut next = self.clone();
        next.deliveries.insert(delivery.delivery_ref.clone(), delivery);
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_accepted_spawn_delivery_commit(
        &self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_delivery_revision: HarnessRevision,
        delivery: HarnessDeliveryV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        delivery.validate()?;
        run.validate()?;
        operation.validate()?;
        let current_delivery = self.deliveries.get(&delivery.delivery_ref)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.delivery_ref.to_string()))?;
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        let current_operation = self.operations.get(&operation.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(operation.operation_id.to_string()))?;
        if current_delivery == &delivery
            && current_run == &run
            && current_operation == &operation
        {
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation,
            });
        }
        validate_delivery_replacement(current_delivery, expected_delivery_revision, &delivery)?;
        if current_delivery.state != HarnessDeliveryStateV1::Staged
            || delivery.state != HarnessDeliveryStateV1::Committed
        {
            return Err(HarnessEngineError::InvalidDeliveryTransition);
        }
        let receipt = delivery.receipt.as_ref()
            .ok_or(HarnessEngineError::InvalidDeliveryTransition)?;
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        validate_run_immutable(current_run, &run)?;
        validate_operation_transition(
            current_operation,
            expected_operation_revision,
            &operation,
        )?;
        validate_run_lifecycle_transition(current_run, &run)?;
        validate_coupled_run_operation(current_run, &run, &operation)?;
        if run.operation_id != operation.operation_id
            || delivery.operation_id != operation.operation_id
            || delivery.run_id != run.run_id
            || current_run.delivery_receipt.is_some()
            || run.delivery_receipt.as_ref() != Some(&receipt.receipt_ref)
            || current_run.continuation_receipt != run.continuation_receipt
            || current_run.result_disposition != run.result_disposition
            || current_run.failure != run.failure
            || run.binding.as_ref() != Some(&receipt.binding)
        {
            return Err(HarnessEngineError::InvalidDeliveryRunCommit);
        }
        let mut next = self.clone();
        next.deliveries.insert(delivery.delivery_ref.clone(), delivery);
        next.runs.insert(run.run_id.clone(), run);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_operation_transition(
        &self,
        expected_revision: HarnessRevision,
        operation: HarnessOperationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        operation.validate()?;
        if operation.run_id.is_some() {
            return Err(HarnessEngineError::RunOperationRequiresAtomicTransition);
        }
        let current = self.operations.get(&operation.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(operation.operation_id.to_string()))?;
        validate_operation_transition(current, expected_revision, &operation)?;
        let mut next = self.clone();
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn prepare_run_operation_transition(
        &self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        run.validate()?;
        operation.validate()?;
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        if current_run.task_id != run.task_id || current_run.operation_id != run.operation_id {
            return Err(HarnessEngineError::MismatchedIdentity("immutable run link"));
        }
        validate_run_immutable(current_run, &run)?;
        let current_operation = self.operations.get(&operation.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(operation.operation_id.to_string()))?;
        validate_operation_transition(current_operation, expected_operation_revision, &operation)?;
        if run.operation_id != operation.operation_id {
            return Err(HarnessEngineError::MismatchedIdentity("run operation"));
        }
        validate_run_lifecycle_transition(current_run, &run)?;
        validate_coupled_run_operation(current_run, &run, &operation)?;
        let mut next = self.clone();
        next.runs.insert(run.run_id.clone(), run);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    /// Atomically records one run lifecycle event and projects it onto its task.
    /// The event operation is inserted exactly once; it is not the run's
    /// original CreateRun operation.
    pub fn prepare_run_event_commit(
        &self,
        operation: HarnessOperationV1,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        operation.validate()?;
        run.validate()?;
        task.validate()?;
        if let Some(current_operation) = self.operations.get(&operation.operation_id) {
            let exact_replay = current_operation == &operation
                && self.runs.get(&run.run_id) == Some(&run)
                && self.tasks.get(&task.task_id) == Some(&task)
                && operation.expected_revision == Some(expected_run_revision)
                && run.revision.get() == expected_run_revision.get().checked_add(1)
                    .ok_or(HarnessEngineError::InvalidNextRevision { entity: "run" })?
                && task.revision.get() == expected_task_revision.get().checked_add(1)
                    .ok_or(HarnessEngineError::InvalidNextRevision { entity: "task" })?;
            if exact_replay {
                return Ok(PreparedHarnessMutation {
                    next: self.clone(),
                    outcome: HarnessApplyOutcome::Replayed,
                    operation,
                });
            }
            return Err(HarnessEngineError::OperationIdConflict {
                operation_id: operation.operation_id,
            });
        }
        require_first_revision(operation.revision, "operation")?;
        require_kind(operation.kind, HarnessOperationKindV1::MutateRun)?;
        require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
        require_operation_expected(&operation, expected_run_revision)?;
        require_same_id(operation.run_id.as_ref(), &run.run_id, "run")?;
        if operation.actor != (HarnessActorV1::ParentRun {
            run_id: run.run_id.clone(),
        })
            || operation.grant_id.is_some()
            || operation.reconciles_operation_id.is_some()
            || run.task_id != task.task_id
            || task.run_ids.binary_search(&run.run_id).is_err()
            || task.updated_at_unix_ms != run.updated_at_unix_ms
            || operation.updated_at_unix_ms != run.updated_at_unix_ms
            || operation.finished_at_unix_ms != Some(run.updated_at_unix_ms)
        {
            return Err(HarnessEngineError::InvalidRunEventAuthority);
        }
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        let current_task = self.tasks.get(&task.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(task.task_id.to_string()))?;
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        validate_replacement(
            "task",
            current_task.revision,
            expected_task_revision,
            task.revision,
            current_task.created_at_unix_ms,
            task.created_at_unix_ms,
        )?;
        validate_run_immutable(current_run, &run)?;
        validate_run_lifecycle_transition(current_run, &run)?;
        validate_generic_run_operation(current_run, &run, &operation)?;
        validate_run_event_task_projection(current_task, &task, &run)?;
        let mut next = self.clone();
        next.runs.insert(run.run_id.clone(), run);
        next.tasks.insert(task.task_id.clone(), task);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    /// Atomically settles one `Running`/`Waiting` run to the retryable
    /// `Failed` lifecycle (`HarnessFailureCategoryV1::TargetUnavailable`,
    /// `retryable: true`), its task to `Failed`, and drops the run's own
    /// binding: the node incarnation the binding named is gone, so there is
    /// nothing left to bind to and nothing further this run will ever
    /// report. `OutcomeUnknown` is deliberately not this event's target --
    /// that lifecycle names "we never learned whether the DISPATCH worked"
    /// and ties to an originating operation that must itself be
    /// `OutcomeUnknown` (`validate_run_operation_coherence`); a run reaching
    /// this path already dispatched successfully and ran, so its
    /// originating `CreateRun` operation is `Succeeded` and always will be,
    /// making `Failed` (which `validate_run_operation_coherence` already
    /// accepts against a `Succeeded` originating operation, the same way it
    /// does for a rejected spawn) the coherent, truthful word for it. The
    /// caller (`gate4agent-harness-service`'s runtime) is the sole authority
    /// for WHEN this is legitimate -- this crate has no C2 client of its own
    /// and never decides the trigger, only records the settlement once the
    /// caller already knows the run's bound incarnation is provably stale.
    ///
    /// A dedicated function rather than a widening of
    /// `prepare_run_event_commit`: this settlement's authority shape and its
    /// task-side sibling (`validate_incarnation_settlement_task_projection`)
    /// stay independent of that function's, even though both land the run on
    /// `Failed` -- see `prepare_run_event_commit`'s own doc comment. Same
    /// CAS/idempotent-replay discipline as that function otherwise.
    pub fn prepare_run_incarnation_settlement(
        &self,
        operation: HarnessOperationV1,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        operation.validate()?;
        run.validate()?;
        task.validate()?;
        if let Some(current_operation) = self.operations.get(&operation.operation_id) {
            let exact_replay = current_operation == &operation
                && self.runs.get(&run.run_id) == Some(&run)
                && self.tasks.get(&task.task_id) == Some(&task)
                && operation.expected_revision == Some(expected_run_revision)
                && run.revision.get() == expected_run_revision.get().checked_add(1)
                    .ok_or(HarnessEngineError::InvalidNextRevision { entity: "run" })?
                && task.revision.get() == expected_task_revision.get().checked_add(1)
                    .ok_or(HarnessEngineError::InvalidNextRevision { entity: "task" })?;
            if exact_replay {
                return Ok(PreparedHarnessMutation {
                    next: self.clone(),
                    outcome: HarnessApplyOutcome::Replayed,
                    operation,
                });
            }
            return Err(HarnessEngineError::OperationIdConflict {
                operation_id: operation.operation_id,
            });
        }
        require_first_revision(operation.revision, "operation")?;
        require_kind(operation.kind, HarnessOperationKindV1::MutateRun)?;
        // `Succeeded`, not `Failed`: this operation is the settlement's own
        // bookkeeping record, and it succeeds at recording the run's
        // failure -- the same convention `prepare_run_event_commit`'s
        // callers already use for a `MutateRun` operation that lands a run
        // on `Failed` (see `commit_lifecycle_projection` in
        // `gate4agent-harness-service`'s runtime), where the substantive
        // failure lives on the run (`run.failure`), never on this wrapper
        // operation (`operation.failure` stays `None`, as `Succeeded`
        // requires).
        require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
        require_operation_expected(&operation, expected_run_revision)?;
        require_same_id(operation.run_id.as_ref(), &run.run_id, "run")?;
        if operation.actor != (HarnessActorV1::ParentRun {
            run_id: run.run_id.clone(),
        })
            || operation.grant_id.is_some()
            || operation.reconciles_operation_id.is_some()
            || run.task_id != task.task_id
            || task.run_ids.binary_search(&run.run_id).is_err()
            || task.updated_at_unix_ms != run.updated_at_unix_ms
            || operation.updated_at_unix_ms != run.updated_at_unix_ms
            || operation.finished_at_unix_ms != Some(run.updated_at_unix_ms)
        {
            return Err(HarnessEngineError::InvalidIncarnationSettlementAuthority);
        }
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        let current_task = self.tasks.get(&task.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(task.task_id.to_string()))?;
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        validate_replacement(
            "task",
            current_task.revision,
            expected_task_revision,
            task.revision,
            current_task.created_at_unix_ms,
            task.created_at_unix_ms,
        )?;
        validate_run_immutable(current_run, &run)?;
        validate_run_lifecycle_transition(current_run, &run)?;
        validate_incarnation_settlement_task_projection(current_task, &task, &run)?;
        let mut next = self.clone();
        next.runs.insert(run.run_id.clone(), run);
        next.tasks.insert(task.task_id.clone(), task);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    /// Narrow, single-entity, terminal-lifecycle-exempt mutation that records
    /// a durable ContextPack receipt a run's own session cleanly exported.
    /// Deliberately does not call `validate_run_lifecycle_transition` (that
    /// function rejects any mutation on a terminal run) and never touches
    /// task state. Idempotent on the exact same receipt (a lost reconcile
    /// race just replays); a second, different receipt is a hard conflict.
    pub fn prepare_run_context_pack_record(
        &self,
        operation: HarnessOperationV1,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        operation.validate()?;
        run.validate()?;
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        validate_context_pack_recording(current_run, &run)?;
        if current_run.context_pack.is_some() {
            // Idempotent replay: an earlier (possibly racing) reconcile pass
            // already recorded this exact receipt — never re-derives a
            // revision or touches `self.operations`.
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation,
            });
        }
        if self.operations.contains_key(&operation.operation_id) {
            return Err(HarnessEngineError::OperationIdConflict {
                operation_id: operation.operation_id,
            });
        }
        require_first_revision(operation.revision, "operation")?;
        require_kind(operation.kind, HarnessOperationKindV1::RecordRunContextPack)?;
        require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
        require_operation_expected(&operation, expected_run_revision)?;
        require_same_id(operation.run_id.as_ref(), &run.run_id, "run")?;
        if operation.actor != (HarnessActorV1::ParentRun { run_id: run.run_id.clone() })
            || operation.task_id.is_some()
            || operation.grant_id.is_some()
            || operation.reconciles_operation_id.is_some()
            || operation.updated_at_unix_ms != run.updated_at_unix_ms
            || operation.finished_at_unix_ms != Some(run.updated_at_unix_ms)
        {
            return Err(HarnessEngineError::InvalidContextPackRecordAuthority);
        }
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        let mut next = self.clone();
        next.runs.insert(run.run_id.clone(), run);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    /// Narrow, single-entity, terminal-lifecycle-exempt mutation that records
    /// a durable, one-shot git-facts capture of a run's workspace. Structural
    /// twin of `prepare_run_context_pack_record`; the only behavioral
    /// difference is that this fact is legal to write exactly once —
    /// `validate_git_facts_recording` treats any change to an already-`Some`
    /// `git_facts` as a hard conflict regardless of cause (the A3 design has
    /// no legitimate "second capture" scenario the way a context pack has a
    /// legitimate re-export).
    pub fn prepare_run_git_facts_record(
        &self,
        operation: HarnessOperationV1,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        operation.validate()?;
        run.validate()?;
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        validate_git_facts_recording(current_run, &run)?;
        if current_run.git_facts.is_some() {
            // Idempotent replay: an earlier (possibly racing) capture attempt
            // already recorded this exact value — never re-derives a
            // revision or touches `self.operations`.
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation,
            });
        }
        if self.operations.contains_key(&operation.operation_id) {
            return Err(HarnessEngineError::OperationIdConflict {
                operation_id: operation.operation_id,
            });
        }
        require_first_revision(operation.revision, "operation")?;
        require_kind(operation.kind, HarnessOperationKindV1::RecordRunGitFacts)?;
        require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
        require_operation_expected(&operation, expected_run_revision)?;
        require_same_id(operation.run_id.as_ref(), &run.run_id, "run")?;
        if operation.actor != (HarnessActorV1::ParentRun { run_id: run.run_id.clone() })
            || operation.task_id.is_some()
            || operation.grant_id.is_some()
            || operation.reconciles_operation_id.is_some()
            || operation.updated_at_unix_ms != run.updated_at_unix_ms
            || operation.finished_at_unix_ms != Some(run.updated_at_unix_ms)
        {
            return Err(HarnessEngineError::InvalidGitFactsRecordAuthority);
        }
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        let mut next = self.clone();
        next.runs.insert(run.run_id.clone(), run);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    /// Genuinely new mutation (no A2 analog): grows one task's `result_refs`
    /// index by exactly one entry, attributing the growth to the exact run
    /// whose already-committed result caused it. Never touches `task.state`
    /// — the index is a passive fact about a task's runs, not a task-state
    /// transition, the same doctrine A2 held for run state. Idempotent on an
    /// already-present ref, or on a task already at `HARNESS_RESULTS_MAX`
    /// capacity: both replay cleanly rather than conflicting.
    pub fn prepare_task_result_ref_record(
        &self,
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        operation.validate()?;
        task.validate()?;
        let current_task = self.tasks.get(&task.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(task.task_id.to_string()))?;
        validate_task_result_ref_recording(current_task, &task)?;
        if current_task.result_refs == task.result_refs {
            // Idempotent replay: the exact ref this attempt wanted to add
            // (or the exact no-op at capacity) is already reflected — never
            // re-derives a revision or touches `self.operations`.
            return Ok(PreparedHarnessMutation {
                next: self.clone(),
                outcome: HarnessApplyOutcome::Replayed,
                operation,
            });
        }
        let inserted = task.result_refs.iter()
            .find(|candidate| current_task.result_refs.binary_search(candidate).is_err())
            .ok_or(HarnessEngineError::InvalidResultRefRecordProjection)?;
        if self.operations.contains_key(&operation.operation_id) {
            return Err(HarnessEngineError::OperationIdConflict {
                operation_id: operation.operation_id,
            });
        }
        require_first_revision(operation.revision, "operation")?;
        require_kind(operation.kind, HarnessOperationKindV1::RecordTaskResultRef)?;
        require_operation_state(operation.state, HarnessOperationStateV1::Succeeded)?;
        require_operation_expected(&operation, expected_task_revision)?;
        require_same_id(operation.task_id.as_ref(), &task.task_id, "task")?;
        let expected_actor = inserted.run_id()
            .map(|run_id| HarnessActorV1::ParentRun { run_id })
            .map_err(|_| HarnessEngineError::InvalidResultRefRecordAuthority)?;
        if operation.actor != expected_actor
            || operation.run_id.is_some()
            || operation.grant_id.is_some()
            || operation.reconciles_operation_id.is_some()
            || operation.updated_at_unix_ms != task.updated_at_unix_ms
            || operation.finished_at_unix_ms != Some(task.updated_at_unix_ms)
        {
            return Err(HarnessEngineError::InvalidResultRefRecordAuthority);
        }
        validate_replacement(
            "task",
            current_task.revision,
            expected_task_revision,
            task.revision,
            current_task.created_at_unix_ms,
            task.created_at_unix_ms,
        )?;
        let mut next = self.clone();
        next.tasks.insert(task.task_id.clone(), task);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    /// Atomically records the categorical result of one leased CreateRun
    /// dispatch. Accepted outcomes use the authoritative spawn-proof seams.
    pub fn prepare_dispatch_outcome_commit(
        &self,
        expected_run_revision: HarnessRevision,
        run: HarnessRunV1,
        expected_operation_revision: HarnessRevision,
        operation: HarnessOperationV1,
        expected_task_revision: HarnessRevision,
        task: HarnessTaskV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        run.validate()?;
        operation.validate()?;
        task.validate()?;
        let current_run = self.runs.get(&run.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.run_id.to_string()))?;
        let current_operation = self.operations.get(&operation.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(operation.operation_id.to_string()))?;
        let current_task = self.tasks.get(&task.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(task.task_id.to_string()))?;
        if current_run == &run && current_operation == &operation && current_task == &task {
            let exact_replay = run.revision.get()
                == expected_run_revision.get().checked_add(1)
                    .ok_or(HarnessEngineError::InvalidNextRevision { entity: "run" })?
                && operation.revision.get()
                    == expected_operation_revision.get().checked_add(1)
                        .ok_or(HarnessEngineError::InvalidNextRevision {
                            entity: "operation",
                        })?
                && task.revision.get()
                    == expected_task_revision.get().checked_add(1)
                        .ok_or(HarnessEngineError::InvalidNextRevision { entity: "task" })?;
            if exact_replay {
                return Ok(PreparedHarnessMutation {
                    next: self.clone(),
                    outcome: HarnessApplyOutcome::Replayed,
                    operation,
                });
            }
            return Err(HarnessEngineError::InvalidDispatchOutcomeProjection);
        }
        validate_replacement(
            "run",
            current_run.revision,
            expected_run_revision,
            run.revision,
            current_run.created_at_unix_ms,
            run.created_at_unix_ms,
        )?;
        validate_operation_transition(
            current_operation,
            expected_operation_revision,
            &operation,
        )?;
        validate_replacement(
            "task",
            current_task.revision,
            expected_task_revision,
            task.revision,
            current_task.created_at_unix_ms,
            task.created_at_unix_ms,
        )?;
        validate_run_immutable(current_run, &run)?;
        validate_run_lifecycle_transition(current_run, &run)?;
        validate_coupled_run_operation(current_run, &run, &operation)?;
        if !matches!(
            (current_run.lifecycle, current_operation.state),
            (HarnessRunLifecycleV1::Requested, HarnessOperationStateV1::Prepared)
                | (HarnessRunLifecycleV1::Preparing, HarnessOperationStateV1::Prepared)
                | (HarnessRunLifecycleV1::Dispatching, HarnessOperationStateV1::Dispatching)
        )
            || current_operation.kind != HarnessOperationKindV1::CreateRun
            || current_operation.operation_id != current_run.operation_id
            || operation.operation_id != run.operation_id
            || operation.run_id.as_ref() != Some(&run.run_id)
            || operation.task_id.as_ref() != Some(&task.task_id)
            || run.task_id != task.task_id
            || task.run_ids.binary_search(&run.run_id).is_err()
            || current_run.binding != run.binding
            || current_run.delivery_receipt != run.delivery_receipt
            || current_run.continuation_receipt != run.continuation_receipt
            || task.updated_at_unix_ms != run.updated_at_unix_ms
            || operation.updated_at_unix_ms != run.updated_at_unix_ms
        {
            return Err(HarnessEngineError::InvalidDispatchOutcomeProjection);
        }
        validate_dispatch_outcome_task_projection(current_task, &task, &run, &operation)?;
        let mut next = self.clone();
        next.runs.insert(run.run_id.clone(), run);
        next.tasks.insert(task.task_id.clone(), task);
        next.operations.insert(operation.operation_id.clone(), operation.clone());
        next.finish(operation, HarnessApplyOutcome::Applied)
    }

    pub fn accept(&mut self, prepared: PreparedHarnessMutation) {
        *self = prepared.next;
    }

    fn finish(
        mut self,
        operation: HarnessOperationV1,
        outcome: HarnessApplyOutcome,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        self.validate_links()?;
        // The single point every real (non-replayed) mutation passes
        // through -- see `mutation_generation`'s own doc comment on
        // `HarnessEngine` for why this, and not a new counter, is what
        // invalidates `visibility_graph_cache`.
        if outcome == HarnessApplyOutcome::Applied {
            self.mutation_generation = self.mutation_generation.wrapping_add(1);
        }
        Ok(PreparedHarnessMutation { next: self, outcome, operation })
    }

    fn validate_links(&self) -> Result<(), HarnessEngineError> {
        for task in self.tasks.values() {
            self.validate_task_links(task)?;
        }
        for run in self.runs.values() {
            self.validate_run_links(run)?;
        }
        for grant in self.grants.values() {
            if !self.runs.contains_key(&grant.actor_run_id) {
                return Err(HarnessEngineError::NotFound(grant.actor_run_id.to_string()));
            }
        }
        for operation in self.operations.values() {
            self.validate_actor(&operation.actor)?;
            self.validate_operation_links(operation)?;
        }
        for spec in self.execution_specs.values() {
            validate_execution_spec_identity(spec)?;
            if !self.tasks.contains_key(&spec.task_id) {
                return Err(HarnessEngineError::NotFound(spec.task_id.to_string()));
            }
            if self.execution_specs_v2.contains_key(&spec.task_id)
                || self.issuances.contains_key(&spec.task_id)
            {
                return Err(HarnessEngineError::DualExecutionSpec(spec.task_id.to_string()));
            }
        }
        let mut issuance_ids = BTreeSet::new();
        let mut v2_spec_ids = BTreeSet::new();
        for issuance in self.issuances.values() {
            issuance.validate()?;
            if !issuance_ids.insert(issuance.issuance_id.clone()) {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    issuance.issuance_id.to_string(),
                ));
            }
            let task = self.tasks.get(&issuance.task_id)
                .ok_or_else(|| HarnessEngineError::NotFound(issuance.task_id.to_string()))?;
            if issuance.task_revision > task.revision {
                return Err(HarnessEngineError::InvalidIssuanceLink);
            }
            let spec = self.execution_specs_v2.get(&issuance.task_id)
                .ok_or(HarnessEngineError::OrphanIssuance)?;
            validate_issued_execution_spec_link(issuance, spec)?;
        }
        for spec in self.execution_specs_v2.values() {
            spec.validate()?;
            if !v2_spec_ids.insert(spec.execution_spec_id.clone()) {
                return Err(HarnessEngineError::DuplicateCheckpointId(
                    spec.execution_spec_id.to_string(),
                ));
            }
            if self.execution_specs.contains_key(&spec.task_id) {
                return Err(HarnessEngineError::DualExecutionSpec(spec.task_id.to_string()));
            }
            let issuance = self.issuances.get(&spec.task_id)
                .ok_or(HarnessEngineError::OrphanIssuedExecutionSpec)?;
            validate_issued_execution_spec_link(issuance, spec)?;
        }
        for delivery in self.deliveries.values() {
            self.validate_delivery_links(delivery)?;
            self.validate_delivery_authority_reference(delivery)?;
        }
        for continuation in self.continuations.values() {
            self.validate_continuation_links(continuation)?;
            self.validate_continuation_authority_reference(continuation)?;
        }
        let mut delivery_runs = BTreeSet::new();
        for delivery in self.deliveries.values() {
            if !delivery_runs.insert(delivery.run_id.clone()) {
                return Err(HarnessEngineError::DuplicateDeliveryRun);
            }
        }
        let mut continuation_runs = BTreeSet::new();
        let mut continuation_receipts = BTreeSet::new();
        for continuation in self.continuations.values() {
            if !continuation_runs.insert(continuation.target_run_id.clone()) {
                return Err(HarnessEngineError::DuplicateContinuationRun);
            }
            if !continuation_receipts.insert(continuation.receipt_ref.clone()) {
                return Err(HarnessEngineError::DuplicateContinuationReceipt);
            }
        }
        for run in self.runs.values() {
            let task = self.tasks.get(&run.task_id)
                .ok_or_else(|| HarnessEngineError::NotFound(run.task_id.to_string()))?;
            if task.run_ids.binary_search(&run.run_id).is_err() {
                return Err(HarnessEngineError::MismatchedIdentity("run task backlink"));
            }
        }
        self.validate_task_cycles()?;
        self.validate_run_cycles()?;
        Ok(())
    }

    fn delivery_operation(
        &self,
        delivery: &HarnessDeliveryV1,
    ) -> Result<&HarnessOperationV1, HarnessEngineError> {
        self.operations.get(&delivery.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.operation_id.to_string()))
    }

    fn continuation_operation(
        &self,
        continuation: &HarnessContinuationV1,
    ) -> Result<&HarnessOperationV1, HarnessEngineError> {
        self.operations.get(&continuation.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.operation_id.to_string()))
    }

    fn finish_continuation_replay(
        &self,
        continuation: &HarnessContinuationV1,
    ) -> Result<PreparedHarnessMutation, HarnessEngineError> {
        Ok(PreparedHarnessMutation {
            next: self.clone(),
            outcome: HarnessApplyOutcome::Replayed,
            operation: self.continuation_operation(continuation)?.clone(),
        })
    }

    fn validate_continuation_authority(
        &self,
        continuation: &HarnessContinuationV1,
    ) -> Result<(), HarnessEngineError> {
        self.validate_continuation_links(continuation)?;
        let source = self.runs.get(&continuation.source_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.source_run_id.to_string()))?;
        let target = self.runs.get(&continuation.target_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.target_run_id.to_string()))?;
        let operation = self.continuation_operation(continuation)?;
        match &continuation.authority {
            HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                let grant = self.grants.get(grant_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(grant_id.to_string()))?;
                if grant.revision != *revision
                    || grant.state != SessionGrantStateV1::Active
                    || !grant.context_permissions.export
                    || !grant.context_permissions.restore
                    || grant.actor_run_id != continuation.source_run_id
                    || target.parent_run_id.as_ref() != Some(&grant.actor_run_id)
                    || operation.actor != (HarnessActorV1::ParentRun {
                        run_id: grant.actor_run_id.clone(),
                    })
                    || !grant.allows_target(
                        &target.intent.node_id,
                        &target.intent.workspace_id,
                        &target.intent.provider_profile,
                        target.intent.mode,
                    )
                {
                    return Err(HarnessEngineError::ContinuationGrantDenied);
                }
            }
            HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                let current = self.operator_issuance_for_run(target, issuance)?;
                let Some(context_source) = &current.context_source else {
                    return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
                };
                // A `Durable` context source always carries `active_session:
                // None` (the receipt, not a live session, is authoritative),
                // while `source_binding.session`'s own `active_session` is
                // set once at spawn-accept and never clears back to `None`
                // in production. Requiring exact equality for a durable
                // source would reject every durable source unconditionally;
                // `record_id` (plus node/incarnation/workspace, checked
                // below) is what identifies it instead. The `Live` path is
                // unchanged.
                let source_session_matches = matches!(
                    &continuation.source_binding.session,
                    gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed {
                        record_id,
                        active_session,
                    } if record_id == &context_source.session_record_id
                        && (context_source.availability
                            == gate4agent_harness_protocol::HarnessContextSourceAvailabilityV1::Durable
                            || active_session.as_ref() == context_source.active_session.as_ref())
                );
                if context_source.source_run_id != continuation.source_run_id
                    || context_source.node_id != continuation.node_id
                    || context_source.node_incarnation != continuation.node_incarnation
                    || context_source.workspace_id != continuation.workspace_id
                    || &operation.actor != target_task_actor(self, target)?
                    || target.parent_run_id.is_some()
                    || !source_session_matches
                {
                    return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
                }
            }
        }
        let source_is_live_managed = matches!(
            &continuation.source_binding.session,
            gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed {
                active_session: Some(_),
                ..
            }
        );
        let source_is_durable_managed = source.context_pack.is_some()
            && matches!(
                &continuation.source_binding.session,
                gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed { .. }
            );
        // For a durable source, `continuation.source_binding` deliberately
        // does not equal `source.binding` byte-for-byte: the service layer
        // synthesizes it with the Node's then-current incarnation in its
        // `node_incarnation` field at continuation-creation time
        // (`HarnessService::start_task_v2`), while `source.binding` (this
        // run's own live, durable binding) stays fixed at its original
        // spawn-accept shape forever. What must still hold is that `source`
        // remains bound to the SAME managed record — node_id, workspace_id,
        // and record_id — the continuation was built against;
        // `node_incarnation`/`active_session` are not signals the durable
        // path can or needs to compare here (mirrors
        // `HarnessService`'s own `source_binding_is_current` in
        // `begin_continuation_export`, `lib.rs`). The live path is
        // unchanged: it still requires the binding to be byte-for-byte
        // identical.
        let source_binding_matches = match source.binding.as_ref() {
            Some(binding) if source_is_durable_managed => {
                binding.node_id == continuation.source_binding.node_id
                    && binding.workspace_id == continuation.source_binding.workspace_id
                    && matches!(
                        (&binding.session, &continuation.source_binding.session),
                        (
                            gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed {
                                record_id: left,
                                ..
                            },
                            gate4agent_harness_protocol::HarnessSessionIdentityV1::Managed {
                                record_id: right,
                                ..
                            },
                        ) if left == right
                    )
            }
            binding => binding == Some(&continuation.source_binding),
        };
        if !matches!(
            source.lifecycle,
            HarnessRunLifecycleV1::Running
                | HarnessRunLifecycleV1::Waiting
                | HarnessRunLifecycleV1::Completed
        )
            || !source_binding_matches
            || !(source_is_live_managed || source_is_durable_managed)
        {
            return Err(HarnessEngineError::InvalidContinuationLink);
        }
        if matches!(
            continuation.state,
            HarnessContinuationStateV1::Prepared
                | HarnessContinuationStateV1::Exporting
                | HarnessContinuationStateV1::Exported
        )
            && (target.lifecycle != HarnessRunLifecycleV1::Requested
                || operation.state != HarnessOperationStateV1::Prepared)
        {
            return Err(HarnessEngineError::ContinuationAuthorityWindowClosed);
        }
        Ok(())
    }

    fn validate_continuation_authority_reference(
        &self,
        continuation: &HarnessContinuationV1,
    ) -> Result<(), HarnessEngineError> {
        let target = self.runs.get(&continuation.target_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.target_run_id.to_string()))?;
        match &continuation.authority {
            HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                let grant = self.grants.get(grant_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(grant_id.to_string()))?;
                if revision.get() > grant.revision.get()
                    || grant.actor_run_id != continuation.source_run_id
                {
                    return Err(HarnessEngineError::InvalidContinuationLink);
                }
            }
            HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                let current = self.operator_issuance_for_run(target, issuance)?;
                let Some(context_source) = &current.context_source else {
                    return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
                };
                if context_source.source_run_id != continuation.source_run_id
                    || context_source.node_id != continuation.node_id
                    || context_source.node_incarnation != continuation.node_incarnation
                    || context_source.workspace_id != continuation.workspace_id
                {
                    return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
                }
            }
        }
        Ok(())
    }

    fn validate_continuation_links(
        &self,
        continuation: &HarnessContinuationV1,
    ) -> Result<(), HarnessEngineError> {
        continuation.validate()?;
        let source = self.runs.get(&continuation.source_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.source_run_id.to_string()))?;
        let target = self.runs.get(&continuation.target_run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(continuation.target_run_id.to_string()))?;
        let operation = self.continuation_operation(continuation)?;
        let selected_source = target.intent.continuation_source_run_id()?;
        if selected_source.as_ref() != Some(&continuation.source_run_id)
            || target.operation_id != continuation.operation_id
            || operation.kind != HarnessOperationKindV1::CreateRun
            || operation.run_id.as_ref() != Some(&continuation.target_run_id)
            || source.intent.node_id != continuation.node_id
            || source.intent.workspace_id != continuation.workspace_id
            || target.intent.node_id != continuation.node_id
        {
            return Err(HarnessEngineError::InvalidContinuationLink);
        }
        match continuation.state {
            HarnessContinuationStateV1::Exported | HarnessContinuationStateV1::Bound => {
                if target.continuation_receipt.as_ref() != Some(&continuation.receipt_ref)
                    || continuation.state == HarnessContinuationStateV1::Bound
                        && target.binding.as_ref() != continuation.target_binding.as_ref()
                {
                    return Err(HarnessEngineError::InvalidContinuationLink);
                }
            }
            HarnessContinuationStateV1::Expired if continuation.context.is_some() => {
                if target.continuation_receipt.as_ref() != Some(&continuation.receipt_ref) {
                    return Err(HarnessEngineError::InvalidContinuationLink);
                }
            }
            _ if target.continuation_receipt.is_some() => {
                return Err(HarnessEngineError::InvalidContinuationLink);
            }
            _ => {}
        }
        Ok(())
    }

    fn validate_delivery_authority(
        &self,
        delivery: &HarnessDeliveryV1,
    ) -> Result<(), HarnessEngineError> {
        self.validate_delivery_links(delivery)?;
        let run = self.runs.get(&delivery.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.run_id.to_string()))?;
        let operation = self.delivery_operation(delivery)?;
        if run.lifecycle != gate4agent_harness_protocol::HarnessRunLifecycleV1::Requested
            || operation.state != HarnessOperationStateV1::Prepared
        {
            return Err(HarnessEngineError::DeliveryAuthorityWindowClosed);
        }
        match &delivery.authority {
            HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                let grant = self.grants.get(grant_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(grant_id.to_string()))?;
                if grant.revision != *revision
                    || grant.state != SessionGrantStateV1::Active
                    || !grant.allows_delivery_bundle(&delivery.bundle.selector)
                    || !grant.allows_target(
                        &run.intent.node_id,
                        &run.intent.workspace_id,
                        &run.intent.provider_profile,
                        run.intent.mode,
                    )
                {
                    return Err(HarnessEngineError::DeliveryGrantDenied);
                }
            }
            HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                let current = self.operator_issuance_for_run(run, issuance)?;
                if current.delivery.as_ref().map(|selection| &selection.bundle)
                    != Some(&delivery.bundle)
                    || &operation.actor != target_task_actor(self, run)?
                    || run.parent_run_id.is_some()
                {
                    return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
                }
            }
        }
        Ok(())
    }

    fn validate_delivery_authority_reference(
        &self,
        delivery: &HarnessDeliveryV1,
    ) -> Result<(), HarnessEngineError> {
        let run = self.runs.get(&delivery.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.run_id.to_string()))?;
        match &delivery.authority {
            HarnessTransferAuthorityRefV1::ParentGrant { grant_id, revision } => {
                let grant = self.grants.get(grant_id)
                    .ok_or_else(|| HarnessEngineError::NotFound(grant_id.to_string()))?;
                if revision.get() > grant.revision.get() {
                    return Err(HarnessEngineError::InvalidDeliveryLink);
                }
            }
            HarnessTransferAuthorityRefV1::OperatorIssuance { issuance } => {
                let current = self.operator_issuance_for_run(run, issuance)?;
                if current.delivery.as_ref().map(|selection| &selection.bundle)
                    != Some(&delivery.bundle)
                {
                    return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
                }
            }
        }
        Ok(())
    }

    fn validate_delivery_links(
        &self,
        delivery: &HarnessDeliveryV1,
    ) -> Result<(), HarnessEngineError> {
        delivery.validate()?;
        let task = self.tasks.get(&delivery.task_id)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.task_id.to_string()))?;
        let run = self.runs.get(&delivery.run_id)
            .ok_or_else(|| HarnessEngineError::NotFound(delivery.run_id.to_string()))?;
        let operation = self.delivery_operation(delivery)?;
        if run.task_id != delivery.task_id
            || run.operation_id != delivery.operation_id
            || task.run_ids.binary_search(&delivery.run_id).is_err()
            || operation.kind != HarnessOperationKindV1::CreateRun
            || operation.task_id.as_ref() != Some(&delivery.task_id)
            || operation.run_id.as_ref() != Some(&delivery.run_id)
            || run.intent.delivery_bundle.as_ref() != Some(&delivery.bundle.selector)
        {
            return Err(HarnessEngineError::InvalidDeliveryLink);
        }
        if let Some(stage) = &delivery.stage_receipt {
            if stage.node_id != run.intent.node_id
                || stage.workspace_id != run.intent.workspace_id
            {
                return Err(HarnessEngineError::InvalidDeliveryLink);
            }
        }
        if let Some(receipt) = &delivery.receipt {
            if run.delivery_receipt.as_ref() != Some(&receipt.receipt_ref)
                || run.binding.as_ref() != Some(&receipt.binding)
            {
                return Err(HarnessEngineError::InvalidDeliveryLink);
            }
        }
        Ok(())
    }

    fn operator_issuance_for_run<'a>(
        &'a self,
        run: &HarnessRunV1,
        reference: &gate4agent_harness_protocol::HarnessTaskLaunchIssuanceRefV1,
    ) -> Result<&'a HarnessTaskLaunchIssuanceV1, HarnessEngineError> {
        let issuance = self.issuances.get(&run.task_id)
            .ok_or(HarnessEngineError::IssuedExecutionAuthorityMissing)?;
        let spec = self.execution_specs_v2.get(&run.task_id)
            .ok_or(HarnessEngineError::IssuedExecutionAuthorityMissing)?;
        validate_issued_execution_spec_link(issuance, spec)?;
        if issuance.reference() != *reference || spec.launch_issuance != *reference {
            return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
        }
        validate_operator_issuance_run_intent(issuance, run)?;
        Ok(issuance)
    }

    fn same_issued_run_projection(
        &self,
        expected_execution_spec_revision: HarnessRevision,
        expected_issuance_revision: HarnessRevision,
        task: &HarnessTaskV1,
        run: &HarnessRunV1,
        delivery: Option<&HarnessDeliveryV1>,
        continuation: Option<&HarnessContinuationV1>,
    ) -> bool {
        self.tasks.get(&task.task_id) == Some(task)
            && self.runs.get(&run.run_id) == Some(run)
            && self.execution_specs_v2.get(&task.task_id)
                .is_some_and(|spec| spec.revision == expected_execution_spec_revision)
            && self.issuances.get(&task.task_id)
                .is_some_and(|issuance| issuance.revision == expected_issuance_revision)
            && match delivery {
                Some(delivery) => self.deliveries.get(&delivery.delivery_ref) == Some(delivery),
                None => self.deliveries.values().all(|record| record.run_id != run.run_id),
            }
            && match continuation {
                Some(continuation) => self.continuations
                    .get(&continuation.continuation_ref) == Some(continuation),
                None => self.continuations.values()
                    .all(|record| record.target_run_id != run.run_id),
            }
    }

    /// Cached, grant-independent structural validity of the whole
    /// task/run/operation graph -- see `mutation_generation` and
    /// `visibility_graph_cache`'s own doc comments on `HarnessEngine` for
    /// the invalidation discipline. This used to be three separate calls
    /// on every `read_visibility`, one of them (the retired
    /// `validate_visibility_scan_bound`) refusing outright once
    /// `tasks.len() + runs.len() + operations.len()` passed
    /// `HARNESS_VISIBILITY_SCAN_MAX` -- a bound that faithfully reflected
    /// the cost of redoing this validation on EVERY read (S9: it was never
    /// a mismatch), but ordinary use grows `operations` (and often `tasks`)
    /// forever, so it was a cliff every long-lived store eventually walked
    /// off and never recovered from: a read is refused "purely because the
    /// history got long" is exactly the failure this cache exists to
    /// retire. Redoing the validation only when the graph could actually
    /// have changed (`mutation_generation` bumped) removes the cost this
    /// aggregate size gate existed to cap, so it is retired rather than
    /// moved: the traversals it gated are already correctly bounded on
    /// their own terms -- `validate_visibility_run_graph`'s cost is
    /// `runs.len() * HARNESS_CHILD_DEPTH_MAX` with cycles caught by its own
    /// `visited` set, and `validate_visibility_operation_graph`'s is
    /// amortized `operations.len()` (its `complete` memoization visits each
    /// operation at most once across the whole pass) with its own
    /// unchanged per-chain `HARNESS_VISIBILITY_SCAN_MAX` guard against one
    /// pathologically long reconciliation chain. Those per-chain bounds are
    /// what a read still fails closed on -- not the store's lifetime size.
    fn visibility_graph_validation(&self) -> Result<(), HarnessEngineError> {
        // Copied out of the `Ref` guard into a plain binding on its own
        // statement (not matched directly in the `if let` scrutinee) so
        // the guard is dropped before `borrow_mut` below runs, never
        // overlapping it.
        let cached = *self.visibility_graph_cache.borrow();
        if let Some((generation, outcome)) = cached {
            if generation == self.mutation_generation {
                return outcome.map_err(HarnessEngineError::from);
            }
        }
        let outcome = self.recompute_visibility_graph_validation();
        *self.visibility_graph_cache.borrow_mut() = Some((self.mutation_generation, outcome));
        outcome.map_err(HarnessEngineError::from)
    }

    fn recompute_visibility_graph_validation(
        &self,
    ) -> Result<(), HarnessVisibilityGraphFaultV1> {
        self.validate_visibility_run_graph()?;
        self.validate_visibility_operation_graph()?;
        Ok(())
    }

    fn validate_visibility_run_graph(&self) -> Result<(), HarnessVisibilityGraphFaultV1> {
        for run_id in self.runs.keys() {
            let mut current = Some(run_id);
            let mut visited = BTreeSet::new();
            let mut depth = 0_u16;
            while let Some(current_id) = current {
                if !visited.insert(current_id.clone()) {
                    return Err(HarnessVisibilityGraphFaultV1::InvalidGraph);
                }
                let run = self.runs.get(current_id)
                    .ok_or(HarnessVisibilityGraphFaultV1::InvalidGraph)?;
                current = run.parent_run_id.as_ref();
                if current.is_some() {
                    depth = depth.checked_add(1)
                        .ok_or(HarnessVisibilityGraphFaultV1::InvalidGraph)?;
                    if depth > HARNESS_CHILD_DEPTH_MAX {
                        return Err(HarnessVisibilityGraphFaultV1::InvalidGraph);
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_visibility_operation_graph(&self) -> Result<(), HarnessVisibilityGraphFaultV1> {
        let mut complete = BTreeSet::new();
        for operation_id in self.operations.keys() {
            if complete.contains(operation_id) {
                continue;
            }
            let mut path = Vec::new();
            let mut visiting = BTreeSet::new();
            let mut current = Some(operation_id);
            while let Some(current_id) = current {
                if complete.contains(current_id) {
                    break;
                }
                if !visiting.insert(current_id.clone()) {
                    return Err(HarnessVisibilityGraphFaultV1::InvalidGraph);
                }
                if path.len() >= HARNESS_VISIBILITY_SCAN_MAX {
                    return Err(HarnessVisibilityGraphFaultV1::ResourceExhausted);
                }
                let operation = self.operations.get(current_id)
                    .ok_or(HarnessVisibilityGraphFaultV1::InvalidGraph)?;
                if matches!(
                    &operation.actor,
                    HarnessActorV1::ParentRun { run_id } if !self.runs.contains_key(run_id)
                ) || operation.task_id.as_ref()
                    .is_some_and(|task_id| !self.tasks.contains_key(task_id))
                    || operation.run_id.as_ref()
                        .is_some_and(|run_id| !self.runs.contains_key(run_id))
                    || operation.grant_id.as_ref()
                        .is_some_and(|grant_id| !self.grants.contains_key(grant_id))
                {
                    return Err(HarnessVisibilityGraphFaultV1::InvalidGraph);
                }
                path.push(current_id.clone());
                current = operation.reconciles_operation_id.as_ref();
                if current.is_some_and(|target| !self.operations.contains_key(target)) {
                    return Err(HarnessVisibilityGraphFaultV1::InvalidGraph);
                }
            }
            complete.extend(path);
        }
        Ok(())
    }

    fn visible_operations(
        &self,
        actor_runs: &BTreeSet<HarnessRunId>,
        visible_tasks: &BTreeSet<HarnessTaskId>,
        visible_runs: &BTreeSet<HarnessRunId>,
    ) -> Result<BTreeSet<HarnessOperationId>, HarnessEngineError> {
        let mut visible = BTreeSet::new();
        let mut frontier = Vec::new();
        let mut waiting: BTreeMap<HarnessOperationId, Vec<HarnessOperationId>> =
            BTreeMap::new();

        for operation in self.operations.values() {
            let actor_visible = matches!(
                &operation.actor,
                HarnessActorV1::ParentRun { run_id } if actor_runs.contains(run_id)
            );
            let targets_visible = operation.task_id.as_ref()
                .map_or(true, |task_id| visible_tasks.contains(task_id))
                && operation.run_id.as_ref()
                    .map_or(true, |run_id| visible_runs.contains(run_id));
            if !actor_visible || !targets_visible || operation.grant_id.is_some() {
                continue;
            }
            if let Some(target_id) = &operation.reconciles_operation_id {
                waiting.entry(target_id.clone())
                    .or_default()
                    .push(operation.operation_id.clone());
            } else if visible.insert(operation.operation_id.clone()) {
                frontier.push(operation.operation_id.clone());
            }
        }

        let mut cursor = 0_usize;
        while cursor < frontier.len() {
            if cursor >= HARNESS_VISIBILITY_SCAN_MAX {
                return Err(HarnessEngineError::ReadVisibilityResourceExhausted);
            }
            let visible_id = frontier[cursor].clone();
            cursor += 1;
            if let Some(dependents) = waiting.remove(&visible_id) {
                for dependent_id in dependents {
                    if visible.insert(dependent_id.clone()) {
                        frontier.push(dependent_id);
                    }
                }
            }
        }
        Ok(visible)
    }

    fn visible_runs_for_scope(
        &self,
        scope: HarnessEntityReadScopeV1,
        actor_run_id: &HarnessRunId,
    ) -> Result<BTreeSet<HarnessRunId>, HarnessEngineError> {
        match scope {
            HarnessEntityReadScopeV1::None => Ok(BTreeSet::new()),
            HarnessEntityReadScopeV1::SelfOnly => {
                Ok(BTreeSet::from([actor_run_id.clone()]))
            }
            HarnessEntityReadScopeV1::Descendants => {
                let mut visible = BTreeSet::from([actor_run_id.clone()]);
                let mut changed = true;
                let mut depth = 0_u16;
                while changed {
                    changed = false;
                    for run in self.runs.values() {
                        if !visible.contains(&run.run_id)
                            && run.parent_run_id.as_ref()
                                .is_some_and(|parent| visible.contains(parent))
                        {
                            visible.insert(run.run_id.clone());
                            changed = true;
                        }
                    }
                    if changed {
                        depth = depth.checked_add(1)
                            .ok_or(HarnessEngineError::ReadVisibilityInvalidGraph)?;
                        if depth > HARNESS_CHILD_DEPTH_MAX {
                            return Err(HarnessEngineError::ReadVisibilityInvalidGraph);
                        }
                    }
                }
                Ok(visible)
            }
        }
    }

    fn tasks_attributed_to_runs(
        &self,
        attribution_runs: &BTreeSet<HarnessRunId>,
    ) -> BTreeSet<HarnessTaskId> {
        if attribution_runs.is_empty() {
            return BTreeSet::new();
        }
        let mut visible_tasks: BTreeSet<_> = attribution_runs.iter().filter_map(|run_id| {
            self.runs.get(run_id).map(|run| run.task_id.clone())
        }).collect();
        visible_tasks.extend(self.tasks.values().filter_map(|task| {
            let tied_to_run = task.run_ids.iter()
                .any(|run_id| attribution_runs.contains(run_id));
            let created_by_run = matches!(
                &task.creator,
                HarnessActorV1::ParentRun { run_id } if attribution_runs.contains(run_id)
            );
            (tied_to_run || created_by_run).then(|| task.task_id.clone())
        }));
        visible_tasks
    }

    /// The bounded set of task ids reachable from `root` by walking
    /// `parent_task_id` downward -- `root` itself is included. Mirrors
    /// `visible_runs_for_scope`'s own `Descendants` shape (above) and its
    /// `HARNESS_CHILD_DEPTH_MAX`-class bound, just walked over the task
    /// graph's own parent link instead of the run graph's, and over
    /// `depth_bound` rather than a fixed constant so a caller can choose
    /// how deep this particular subtree check reaches. Unlike
    /// `visible_runs_for_scope`, hitting the bound does not fail closed
    /// with an error: this is a query, not an authorization decision by
    /// itself, and the direction is safe either way it is used for one --
    /// a task past the cut is simply absent from the returned set, which
    /// is exactly the answer a caller checking subtree membership wants
    /// for anything it cannot prove is inside (D5, Slice D:
    /// `ParentOutsideOwnSubtree` / `TaskOutsideOwnSubtree`).
    pub fn task_subtree(
        &self,
        root: &HarnessTaskId,
        depth_bound: u16,
    ) -> BTreeSet<HarnessTaskId> {
        let mut visible = BTreeSet::from([root.clone()]);
        let mut changed = true;
        let mut depth = 0_u16;
        while changed {
            changed = false;
            for task in self.tasks.values() {
                if !visible.contains(&task.task_id)
                    && task.parent_task_id.as_ref().is_some_and(|parent| visible.contains(parent))
                {
                    visible.insert(task.task_id.clone());
                    changed = true;
                }
            }
            if changed {
                depth = depth.saturating_add(1);
                if depth > depth_bound {
                    break;
                }
            }
        }
        visible
    }

    /// Whether `task` is a strict (non-self) descendant of `root` --
    /// `task_subtree(root, HARNESS_CHILD_DEPTH_MAX)` minus `root` itself,
    /// the same depth fence the run graph's own `Descendants` scope uses.
    pub fn task_is_strict_descendant(&self, task: &HarnessTaskId, root: &HarnessTaskId) -> bool {
        task != root && self.task_subtree(root, HARNESS_CHILD_DEPTH_MAX).contains(task)
    }

    /// The number of tasks parented directly under `root` right now --
    /// `SessionContextV1::child_task_count`'s own live observation, backing
    /// `g4a_context_get`. Recomputed by scanning `self.tasks` on every call,
    /// never a stored counter denormalised onto the task record (the owner's
    /// ruling behind that field's own doc comment: detect a session's
    /// children, never cap them). Carries no upper bound of its own -- a
    /// task can parent arbitrarily many children and this simply reports
    /// how many exist.
    pub fn task_child_count(&self, root: &HarnessTaskId) -> u64 {
        self.tasks.values()
            .filter(|task| task.parent_task_id.as_ref() == Some(root))
            .count() as u64
    }

    /// The depth of the deepest chain of tasks parented (transitively)
    /// under `root` right now -- `SessionContextV1::child_task_subtree_
    /// depth`'s own live observation: `0` when `root` has no children, `1`
    /// when it has children but none of those have children of their own,
    /// and so on. Walks `parent_task_id` downward the same way
    /// `task_subtree` does, but returns the depth actually reached instead
    /// of the set of ids, and -- deliberately, unlike `task_subtree` -- is
    /// not bounded by `HARNESS_CHILD_DEPTH_MAX` or any other ceiling:
    /// termination comes from `self.tasks` being finite (each pass either
    /// inserts at least one task new to `visible` or the loop stops, and
    /// `visible` can never hold more entries than `self.tasks` does), backed
    /// by `validate_task_cycles` keeping the task graph a forest on every
    /// mutation, not from a policy bound.
    pub fn task_subtree_depth(&self, root: &HarnessTaskId) -> u64 {
        let mut visible = BTreeSet::from([root.clone()]);
        let mut depth = 0_u64;
        let mut changed = true;
        while changed {
            changed = false;
            for task in self.tasks.values() {
                if !visible.contains(&task.task_id)
                    && task.parent_task_id.as_ref().is_some_and(|parent| visible.contains(parent))
                {
                    visible.insert(task.task_id.clone());
                    changed = true;
                }
            }
            if changed {
                depth += 1;
            }
        }
        depth
    }

    /// The task a run's own grant governs -- `engine.run(run_id)?.task_id`,
    /// named so a caller deciding "is this the caller's own task"
    /// (`HarnessTaskMoveResultV1::TaskIsOwn`, D5) reads its intent rather
    /// than reaching into `HarnessRunV1` directly.
    pub fn own_task_of_run(&self, run_id: &HarnessRunId) -> Option<HarnessTaskId> {
        self.run(run_id).map(|run| run.task_id.clone())
    }

    /// Every persisted `HarnessOperationV1` naming `task_id`, newest first
    /// (`created_at_unix_ms` descending, ties broken by `operation_id`
    /// descending so the order is deterministic), bounded by `limit` --
    /// `HarnessOperationV1.actor` is stored today but had no read surface;
    /// this lets the service render an operator-side ledger with the raw
    /// actor for a given task.
    pub fn operations_for_task(
        &self,
        task_id: &HarnessTaskId,
        limit: usize,
    ) -> Vec<&HarnessOperationV1> {
        let mut matching: Vec<&HarnessOperationV1> = self.operations.values()
            .filter(|operation| operation.task_id.as_ref() == Some(task_id))
            .collect();
        matching.sort_by(|left, right| {
            right.created_at_unix_ms.cmp(&left.created_at_unix_ms)
                .then_with(|| right.operation_id.cmp(&left.operation_id))
        });
        matching.truncate(limit);
        matching
    }

    fn validate_task_links(&self, task: &HarnessTaskV1) -> Result<(), HarnessEngineError> {
        self.validate_actor(&task.creator)?;
        if let Some(parent) = &task.parent_task_id {
            if !self.tasks.contains_key(parent) {
                return Err(HarnessEngineError::NotFound(parent.to_string()));
            }
        }
        for dependency in &task.dependencies {
            if !self.tasks.contains_key(dependency) {
                return Err(HarnessEngineError::NotFound(dependency.to_string()));
            }
        }
        for run_id in &task.run_ids {
            let run = self.runs.get(run_id)
                .ok_or_else(|| HarnessEngineError::NotFound(run_id.to_string()))?;
            if run.task_id != task.task_id {
                return Err(HarnessEngineError::MismatchedIdentity("task run ownership"));
            }
        }
        Ok(())
    }

    fn validate_run_links(&self, run: &HarnessRunV1) -> Result<(), HarnessEngineError> {
        if !self.tasks.contains_key(&run.task_id) {
            return Err(HarnessEngineError::NotFound(run.task_id.to_string()));
        }
        if let Some(parent) = &run.parent_run_id {
            if !self.runs.contains_key(parent) {
                return Err(HarnessEngineError::NotFound(parent.to_string()));
            }
        }
        let operation = self.operations.get(&run.operation_id)
            .ok_or_else(|| HarnessEngineError::NotFound(run.operation_id.to_string()))?;
        if operation.kind != HarnessOperationKindV1::CreateRun
            || operation.run_id.as_ref() != Some(&run.run_id)
            || operation.task_id.as_ref() != Some(&run.task_id)
        {
            return Err(HarnessEngineError::MismatchedIdentity(
                "run original CreateRun operation",
            ));
        }
        validate_run_operation_coherence(run, operation)?;
        Ok(())
    }

    fn validate_task_cycles(&self) -> Result<(), HarnessEngineError> {
        let mut complete = BTreeSet::new();
        for task_id in self.tasks.keys() {
            visit_task(task_id, self, &mut BTreeSet::new(), &mut complete)?;
        }
        Ok(())
    }

    fn validate_run_cycles(&self) -> Result<(), HarnessEngineError> {
        let mut complete = BTreeSet::new();
        for run_id in self.runs.keys() {
            visit_run(run_id, self, &mut BTreeSet::new(), &mut complete)?;
        }
        Ok(())
    }

    fn validate_actor(&self, actor: &HarnessActorV1) -> Result<(), HarnessEngineError> {
        if let HarnessActorV1::ParentRun { run_id } = actor {
            if !self.runs.contains_key(run_id) {
                return Err(HarnessEngineError::NotFound(run_id.to_string()));
            }
        }
        Ok(())
    }

    fn validate_operation_links(
        &self,
        operation: &HarnessOperationV1,
    ) -> Result<(), HarnessEngineError> {
        if let Some(task_id) = &operation.task_id {
            if !self.tasks.contains_key(task_id) {
                return Err(HarnessEngineError::NotFound(task_id.to_string()));
            }
        }
        if let Some(run_id) = &operation.run_id {
            if !self.runs.contains_key(run_id) {
                return Err(HarnessEngineError::NotFound(run_id.to_string()));
            }
        }
        if let Some(grant_id) = &operation.grant_id {
            if !self.grants.contains_key(grant_id) {
                return Err(HarnessEngineError::NotFound(grant_id.to_string()));
            }
        }
        if let Some(reconciles) = &operation.reconciles_operation_id {
            if !self.operations.contains_key(reconciles) {
                return Err(HarnessEngineError::NotFound(reconciles.to_string()));
            }
        }
        Ok(())
    }
}

fn visit_task(
    task_id: &HarnessTaskId,
    engine: &HarnessEngine,
    visiting: &mut BTreeSet<HarnessTaskId>,
    complete: &mut BTreeSet<HarnessTaskId>,
) -> Result<(), HarnessEngineError> {
    if complete.contains(task_id) {
        return Ok(());
    }
    if !visiting.insert(task_id.clone()) {
        return Err(HarnessEngineError::Cycle("task graph"));
    }
    let task = engine.tasks.get(task_id)
        .ok_or_else(|| HarnessEngineError::NotFound(task_id.to_string()))?;
    if let Some(parent) = &task.parent_task_id {
        visit_task(parent, engine, visiting, complete)?;
    }
    for dependency in &task.dependencies {
        visit_task(dependency, engine, visiting, complete)?;
    }
    visiting.remove(task_id);
    complete.insert(task_id.clone());
    Ok(())
}

fn visit_run(
    run_id: &HarnessRunId,
    engine: &HarnessEngine,
    visiting: &mut BTreeSet<HarnessRunId>,
    complete: &mut BTreeSet<HarnessRunId>,
) -> Result<(), HarnessEngineError> {
    if complete.contains(run_id) {
        return Ok(());
    }
    if !visiting.insert(run_id.clone()) {
        return Err(HarnessEngineError::Cycle("run parent graph"));
    }
    let run = engine.runs.get(run_id)
        .ok_or_else(|| HarnessEngineError::NotFound(run_id.to_string()))?;
    if let Some(parent) = &run.parent_run_id {
        visit_run(parent, engine, visiting, complete)?;
    }
    visiting.remove(run_id);
    complete.insert(run_id.clone());
    Ok(())
}

fn same_operation_request(left: &HarnessOperationV1, right: &HarnessOperationV1) -> bool {
    left.operation_id == right.operation_id
        && left.actor == right.actor
        && left.kind == right.kind
        && left.task_id == right.task_id
        && left.run_id == right.run_id
        && left.grant_id == right.grant_id
        && left.reconciles_operation_id == right.reconciles_operation_id
        && left.expected_revision == right.expected_revision
        && left.request_digest == right.request_digest
        && left.idempotency_ref == right.idempotency_ref
        && left.created_at_unix_ms == right.created_at_unix_ms
}

fn require_first_revision(
    revision: HarnessRevision,
    entity: &'static str,
) -> Result<(), HarnessEngineError> {
    if revision.get() != 1 {
        return Err(HarnessEngineError::InvalidNextRevision { entity });
    }
    Ok(())
}

fn validate_execution_spec_identity(
    spec: &HarnessTaskExecutionSpecV1,
) -> Result<(), HarnessEngineError> {
    spec.validate()?;
    Ok(())
}

fn validate_issued_execution_spec_link(
    issuance: &HarnessTaskLaunchIssuanceV1,
    spec: &HarnessTaskExecutionSpecV2,
) -> Result<(), HarnessEngineError> {
    issuance.validate()?;
    spec.validate()?;
    if issuance.task_id != spec.task_id
        || issuance.revision != spec.revision
        || issuance.reference() != spec.launch_issuance
    {
        return Err(HarnessEngineError::InvalidIssuanceLink);
    }
    Ok(())
}

fn validate_issued_run_task_projection(
    current: &HarnessTaskV1,
    expected_revision: HarnessRevision,
    next: &HarnessTaskV1,
    run: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    validate_replacement(
        "task",
        current.revision,
        expected_revision,
        next.revision,
        current.created_at_unix_ms,
        next.created_at_unix_ms,
    )?;
    let mut expected_run_ids = current.run_ids.clone();
    match expected_run_ids.binary_search(&run.run_id) {
        Ok(_) => return Err(HarnessEngineError::AlreadyExists(run.run_id.to_string())),
        Err(index) => expected_run_ids.insert(index, run.run_id.clone()),
    }
    if current.state != HarnessTaskStateV1::Ready
        || next.state != HarnessTaskStateV1::Running
        || current.task_id != next.task_id
        || next.task_id != run.task_id
        || current.title != next.title
        || current.body != next.body
        || current.creator != next.creator
        || current.parent_task_id != next.parent_task_id
        || current.dependencies != next.dependencies
        || expected_run_ids != next.run_ids
        || current.result_refs != next.result_refs
        || current.artifact_refs != next.artifact_refs
    {
        return Err(HarnessEngineError::InvalidIssuedRunProjection);
    }
    Ok(())
}

fn validate_operator_issuance_run_intent(
    issuance: &HarnessTaskLaunchIssuanceV1,
    run: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    let worktree_matches = match (&issuance.target.worktree, &run.intent.worktree) {
        (
            gate4agent_harness_protocol::HarnessLaunchWorktreeSelectionV1::Existing,
            gate4agent_harness_protocol::HarnessWorktreeIntentV1::Existing,
        ) => true,
        (
            gate4agent_harness_protocol::HarnessLaunchWorktreeSelectionV1::Managed {
                profile_id,
                expected_profile_revision,
            },
            gate4agent_harness_protocol::HarnessWorktreeIntentV1::ManagedProfile {
                profile_id: actual_profile_id,
                expected_profile_revision: actual_revision,
            },
        ) => profile_id == actual_profile_id && expected_profile_revision == actual_revision,
        _ => false,
    };
    let delivery_selector = issuance.delivery.as_ref()
        .map(|selection| &selection.bundle.selector);
    let continuation_selector = issuance.context_source.as_ref()
        .map(|source| source.source_run_id.as_str());
    if issuance.task_id != run.task_id
        || issuance.task_revision.get() == 0
        || issuance.target.node_id != run.intent.node_id
        || issuance.target.source_workspace_id != run.intent.workspace_id
        || issuance.target.provider_profile != run.intent.provider_profile
        || issuance.target.mode != run.intent.mode
        || !worktree_matches
        || run.intent.delivery_bundle.as_ref() != delivery_selector
        || run.intent.continuation.as_ref().map(|selector| selector.as_str())
            != continuation_selector
    {
        return Err(HarnessEngineError::IssuedExecutionTransferMismatch);
    }
    Ok(())
}

fn target_task_actor<'a>(
    engine: &'a HarnessEngine,
    run: &HarnessRunV1,
) -> Result<&'a HarnessActorV1, HarnessEngineError> {
    engine.tasks.get(&run.task_id)
        .map(|task| &task.creator)
        .ok_or_else(|| HarnessEngineError::NotFound(run.task_id.to_string()))
}

fn require_kind(
    actual: HarnessOperationKindV1,
    expected: HarnessOperationKindV1,
) -> Result<(), HarnessEngineError> {
    if actual != expected {
        return Err(HarnessEngineError::WrongOperationKind);
    }
    Ok(())
}

fn require_operation_state(
    actual: HarnessOperationStateV1,
    expected: HarnessOperationStateV1,
) -> Result<(), HarnessEngineError> {
    if actual != expected {
        return Err(HarnessEngineError::WrongOperationState { expected, actual });
    }
    Ok(())
}

fn require_operation_expected(
    operation: &HarnessOperationV1,
    expected_revision: HarnessRevision,
) -> Result<(), HarnessEngineError> {
    if operation.expected_revision != Some(expected_revision) {
        return Err(HarnessEngineError::ExpectedRevisionMismatch {
            entity: "operation request",
            expected: expected_revision.get(),
            actual: operation.expected_revision.map(HarnessRevision::get),
        });
    }
    Ok(())
}

fn require_same_id<T: PartialEq>(
    operation_id: Option<&T>,
    entity_id: &T,
    entity: &'static str,
) -> Result<(), HarnessEngineError> {
    if operation_id != Some(entity_id) {
        return Err(HarnessEngineError::MismatchedIdentity(entity));
    }
    Ok(())
}

fn validate_replacement(
    entity: &'static str,
    current_revision: HarnessRevision,
    expected_revision: HarnessRevision,
    next_revision: HarnessRevision,
    current_created_at: u64,
    next_created_at: u64,
) -> Result<(), HarnessEngineError> {
    if current_revision != expected_revision {
        return Err(HarnessEngineError::ExpectedRevisionMismatch {
            entity,
            expected: expected_revision.get(),
            actual: Some(current_revision.get()),
        });
    }
    if next_revision.get() != expected_revision.get().checked_add(1).unwrap_or(0) {
        return Err(HarnessEngineError::InvalidNextRevision { entity });
    }
    if current_created_at != next_created_at {
        return Err(HarnessEngineError::CreatedTimestampChanged { entity });
    }
    Ok(())
}

fn validate_delivery_replacement(
    current: &HarnessDeliveryV1,
    expected_revision: HarnessRevision,
    next: &HarnessDeliveryV1,
) -> Result<(), HarnessEngineError> {
    validate_replacement(
        "delivery",
        current.revision,
        expected_revision,
        next.revision,
        current.created_at_unix_ms,
        next.created_at_unix_ms,
    )?;
    if current.delivery_ref != next.delivery_ref
        || current.authority != next.authority
        || current.task_id != next.task_id
        || current.run_id != next.run_id
        || current.operation_id != next.operation_id
        || current.bundle != next.bundle
        || current.stage_receipt.is_some() && current.stage_receipt != next.stage_receipt
        || current.receipt.is_some()
    {
        return Err(HarnessEngineError::InvalidDeliveryImmutableMutation);
    }
    Ok(())
}

fn validate_continuation_replacement(
    current: &HarnessContinuationV1,
    expected_revision: HarnessRevision,
    next: &HarnessContinuationV1,
) -> Result<(), HarnessEngineError> {
    validate_replacement(
        "continuation",
        current.revision,
        expected_revision,
        next.revision,
        current.created_at_unix_ms,
        next.created_at_unix_ms,
    )?;
    if current.continuation_ref != next.continuation_ref
        || current.receipt_ref != next.receipt_ref
        || current.authority != next.authority
        || current.source_run_id != next.source_run_id
        || current.target_run_id != next.target_run_id
        || current.operation_id != next.operation_id
        || current.node_id != next.node_id
        || current.node_incarnation != next.node_incarnation
        || current.workspace_id != next.workspace_id
        || current.source_provider != next.source_provider
        || current.source_binding != next.source_binding
        || current.prepared_at_unix_ms != next.prepared_at_unix_ms
        || current.cleanup_state != next.cleanup_state
        || current.context.is_some() && current.context != next.context
        || current.target_binding.is_some() && current.target_binding != next.target_binding
    {
        return Err(HarnessEngineError::InvalidContinuationImmutableMutation);
    }
    Ok(())
}

fn validate_delivery_restage(
    current: &HarnessDeliveryV1,
    expected_revision: HarnessRevision,
    next: &HarnessDeliveryV1,
) -> Result<(), HarnessEngineError> {
    validate_replacement(
        "delivery",
        current.revision,
        expected_revision,
        next.revision,
        current.created_at_unix_ms,
        next.created_at_unix_ms,
    )?;
    let (Some(current_stage), Some(next_stage)) =
        (&current.stage_receipt, &next.stage_receipt)
    else {
        return Err(HarnessEngineError::InvalidDeliveryTransition);
    };
    if current.delivery_ref != next.delivery_ref
        || current.authority != next.authority
        || current.task_id != next.task_id
        || current.run_id != next.run_id
        || current.operation_id != next.operation_id
        || current.bundle != next.bundle
        || current.receipt.is_some()
        || next.receipt.is_some()
        || current_stage.node_id != next_stage.node_id
        || current_stage.workspace_id != next_stage.workspace_id
        || current_stage.bundle != next_stage.bundle
        || current_stage.node_incarnation == next_stage.node_incarnation
        || next_stage.staged_at_unix_ms <= current_stage.staged_at_unix_ms
    {
        return Err(HarnessEngineError::InvalidDeliveryRestage);
    }
    Ok(())
}

fn validate_operation_transition(
    current: &HarnessOperationV1,
    expected_revision: HarnessRevision,
    next: &HarnessOperationV1,
) -> Result<(), HarnessEngineError> {
    validate_replacement(
        "operation",
        current.revision,
        expected_revision,
        next.revision,
        current.created_at_unix_ms,
        next.created_at_unix_ms,
    )?;
    if current.actor != next.actor
        || current.kind != next.kind
        || current.task_id != next.task_id
        || current.run_id != next.run_id
        || current.grant_id != next.grant_id
        || current.reconciles_operation_id != next.reconciles_operation_id
        || current.expected_revision != next.expected_revision
        || current.request_digest != next.request_digest
        || current.idempotency_ref != next.idempotency_ref
    {
        return Err(HarnessEngineError::MismatchedIdentity("immutable operation request"));
    }
    let allowed = matches!(
        (current.state, next.state),
        (HarnessOperationStateV1::Prepared, HarnessOperationStateV1::Dispatching)
            | (HarnessOperationStateV1::Prepared, HarnessOperationStateV1::Succeeded)
            | (HarnessOperationStateV1::Prepared, HarnessOperationStateV1::Failed)
            | (HarnessOperationStateV1::Prepared, HarnessOperationStateV1::OutcomeUnknown)
            | (HarnessOperationStateV1::Dispatching, HarnessOperationStateV1::OutcomeUnknown)
            | (HarnessOperationStateV1::Dispatching, HarnessOperationStateV1::Succeeded)
            | (HarnessOperationStateV1::Dispatching, HarnessOperationStateV1::Failed)
    );
    if !allowed {
        return Err(HarnessEngineError::InvalidOperationTransition {
            from: current.state,
            to: next.state,
        });
    }
    Ok(())
}

fn validate_run_event_task_projection(
    current: &HarnessTaskV1,
    next: &HarnessTaskV1,
    run: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    if current.task_id != next.task_id
        || current.title != next.title
        || current.body != next.body
        || current.creator != next.creator
        || current.parent_task_id != next.parent_task_id
        || current.dependencies != next.dependencies
        || current.run_ids != next.run_ids
        || current.result_refs != next.result_refs
        || current.artifact_refs != next.artifact_refs
        || !matches!(current.state, HarnessTaskStateV1::Running | HarnessTaskStateV1::Waiting)
    {
        return Err(HarnessEngineError::InvalidRunEventTaskProjection);
    }
    let expected_task_state = match run.lifecycle {
        HarnessRunLifecycleV1::Waiting => HarnessTaskStateV1::Waiting,
        HarnessRunLifecycleV1::Running => HarnessTaskStateV1::Running,
        HarnessRunLifecycleV1::Completed => HarnessTaskStateV1::Review,
        HarnessRunLifecycleV1::Failed => HarnessTaskStateV1::Failed,
        HarnessRunLifecycleV1::Cancelled => HarnessTaskStateV1::Cancelled,
        _ => return Err(HarnessEngineError::InvalidRunEventTaskProjection),
    };
    if next.state != expected_task_state {
        return Err(HarnessEngineError::InvalidRunEventTaskProjection);
    }
    Ok(())
}

/// `prepare_run_incarnation_settlement`'s task-side sibling of
/// `validate_run_event_task_projection`. Kept separate rather than reused
/// directly: the settlement projection has exactly one legal target
/// (`Failed` -- the same task state `validate_run_event_task_projection`
/// would derive for a `Failed` run, mirroring the existing rejected-spawn
/// convention: the work still needs doing, so `HarnessService::
/// operator_retry_task` is the actionable path back to `Ready`, the same
/// path every other `Failed` task already uses) regardless of which of the
/// two eligible source lifecycles (`Running`/`Waiting`) the run started in,
/// and `prepare_run_incarnation_settlement` keeps its own authority/CAS
/// discipline independent of `prepare_run_event_commit`'s.
fn validate_incarnation_settlement_task_projection(
    current: &HarnessTaskV1,
    next: &HarnessTaskV1,
    run: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    if current.task_id != next.task_id
        || current.title != next.title
        || current.body != next.body
        || current.creator != next.creator
        || current.parent_task_id != next.parent_task_id
        || current.dependencies != next.dependencies
        || current.run_ids != next.run_ids
        || current.result_refs != next.result_refs
        || current.artifact_refs != next.artifact_refs
        || !matches!(current.state, HarnessTaskStateV1::Running | HarnessTaskStateV1::Waiting)
        || run.lifecycle != HarnessRunLifecycleV1::Failed
        || next.state != HarnessTaskStateV1::Failed
    {
        return Err(HarnessEngineError::InvalidIncarnationSettlementProjection);
    }
    Ok(())
}

fn validate_dispatch_outcome_task_projection(
    current: &HarnessTaskV1,
    next: &HarnessTaskV1,
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
) -> Result<(), HarnessEngineError> {
    if current.task_id != next.task_id
        || current.title != next.title
        || current.body != next.body
        || current.creator != next.creator
        || current.parent_task_id != next.parent_task_id
        || current.dependencies != next.dependencies
        || current.run_ids != next.run_ids
        || current.result_refs != next.result_refs
        || current.artifact_refs != next.artifact_refs
        || current.state != HarnessTaskStateV1::Running
    {
        return Err(HarnessEngineError::InvalidDispatchOutcomeProjection);
    }
    let coherent = match (run.lifecycle, operation.state, next.state) {
        (
            HarnessRunLifecycleV1::Failed,
            HarnessOperationStateV1::Failed,
            HarnessTaskStateV1::Failed,
        ) => run.result_disposition
                == Some(gate4agent_harness_protocol::HarnessResultDispositionV1::Failed)
            && run.failure.is_some()
            && run.failure == operation.failure
            && operation.outcome_unknown_reason.is_none()
            && operation.finished_at_unix_ms == Some(run.updated_at_unix_ms),
        (
            HarnessRunLifecycleV1::OutcomeUnknown,
            HarnessOperationStateV1::OutcomeUnknown,
            HarnessTaskStateV1::Waiting,
        ) => run.result_disposition.is_none()
            && run.failure.is_none()
            && operation.failure.is_none()
            && operation.outcome_unknown_reason.is_some()
            && operation.finished_at_unix_ms.is_none(),
        _ => false,
    };
    if !coherent {
        return Err(HarnessEngineError::InvalidDispatchOutcomeProjection);
    }
    Ok(())
}

fn validate_run_lifecycle_transition(
    current: &HarnessRunV1,
    next: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    use gate4agent_harness_protocol::HarnessRunLifecycleV1;

    if matches!(
        current.lifecycle,
        HarnessRunLifecycleV1::Completed
            | HarnessRunLifecycleV1::Failed
            | HarnessRunLifecycleV1::Cancelled
            | HarnessRunLifecycleV1::OutcomeUnknown
    ) {
        if current.lifecycle == HarnessRunLifecycleV1::OutcomeUnknown {
            return Err(HarnessEngineError::OutcomeUnknownRunFrozen);
        }
        return Err(HarnessEngineError::TerminalRunMutation);
    }
    if current.lifecycle == next.lifecycle {
        return Ok(());
    }
    let allowed = matches!(
        (current.lifecycle, next.lifecycle),
        (HarnessRunLifecycleV1::Requested, HarnessRunLifecycleV1::Preparing)
            | (HarnessRunLifecycleV1::Requested, HarnessRunLifecycleV1::Dispatching)
            | (HarnessRunLifecycleV1::Requested, HarnessRunLifecycleV1::OutcomeUnknown)
            | (HarnessRunLifecycleV1::Requested, HarnessRunLifecycleV1::Failed)
            | (HarnessRunLifecycleV1::Requested, HarnessRunLifecycleV1::Cancelled)
            | (HarnessRunLifecycleV1::Preparing, HarnessRunLifecycleV1::Dispatching)
            | (HarnessRunLifecycleV1::Preparing, HarnessRunLifecycleV1::OutcomeUnknown)
            | (HarnessRunLifecycleV1::Preparing, HarnessRunLifecycleV1::Failed)
            | (HarnessRunLifecycleV1::Preparing, HarnessRunLifecycleV1::Cancelled)
            | (HarnessRunLifecycleV1::Dispatching, HarnessRunLifecycleV1::OutcomeUnknown)
            | (HarnessRunLifecycleV1::Dispatching, HarnessRunLifecycleV1::Running)
            | (HarnessRunLifecycleV1::Dispatching, HarnessRunLifecycleV1::Failed)
            | (HarnessRunLifecycleV1::Dispatching, HarnessRunLifecycleV1::Cancelled)
            | (HarnessRunLifecycleV1::Running, HarnessRunLifecycleV1::Waiting)
            | (HarnessRunLifecycleV1::Running, HarnessRunLifecycleV1::Completed)
            | (HarnessRunLifecycleV1::Running, HarnessRunLifecycleV1::Failed)
            | (HarnessRunLifecycleV1::Running, HarnessRunLifecycleV1::Cancelled)
            | (HarnessRunLifecycleV1::Waiting, HarnessRunLifecycleV1::Running)
            | (HarnessRunLifecycleV1::Waiting, HarnessRunLifecycleV1::Completed)
            | (HarnessRunLifecycleV1::Waiting, HarnessRunLifecycleV1::Failed)
            | (HarnessRunLifecycleV1::Waiting, HarnessRunLifecycleV1::Cancelled)
    );
    if !allowed {
        return Err(HarnessEngineError::InvalidRunLifecycleTransition {
            from: current.lifecycle,
            to: next.lifecycle,
        });
    }
    Ok(())
}

fn validate_generic_run_operation(
    current: &HarnessRunV1,
    next: &HarnessRunV1,
    operation: &HarnessOperationV1,
) -> Result<(), HarnessEngineError> {
    use gate4agent_harness_protocol::HarnessRunLifecycleV1;

    let allowed = match operation.kind {
        HarnessOperationKindV1::MutateRun => {
            operation.state == HarnessOperationStateV1::Succeeded
                && matches!(
                    current.lifecycle,
                    HarnessRunLifecycleV1::Running | HarnessRunLifecycleV1::Waiting
                )
                && matches!(
                    next.lifecycle,
                    HarnessRunLifecycleV1::Running
                        | HarnessRunLifecycleV1::Waiting
                        | HarnessRunLifecycleV1::Completed
                        | HarnessRunLifecycleV1::Failed
                        | HarnessRunLifecycleV1::Cancelled
                )
        }
        _ => false,
    };
    if !allowed {
        return Err(HarnessEngineError::InvalidGenericRunOperation);
    }
    Ok(())
}

/// Guards `prepare_run_context_pack_record`: only `context_pack`, `revision`,
/// and `updated_at_unix_ms` may differ between `current` and `next`. A run
/// already frozen by H4 (`OutcomeUnknown`) stays frozen; a second, different
/// receipt is a hard conflict, never a silent overwrite; and a replay of the
/// exact same receipt is accepted (checked structurally here, resolved to an
/// `Applied`/`Replayed` outcome by the caller).
fn validate_context_pack_recording(
    current: &HarnessRunV1,
    next: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    if current.lifecycle == HarnessRunLifecycleV1::OutcomeUnknown {
        return Err(HarnessEngineError::OutcomeUnknownRunFrozen);
    }
    if current.run_id != next.run_id
        || current.parent_run_id != next.parent_run_id
        || current.task_id != next.task_id
        || current.operation_id != next.operation_id
        || current.intent != next.intent
        || current.delivery_receipt != next.delivery_receipt
        || current.continuation_receipt != next.continuation_receipt
        || current.git_facts != next.git_facts
        || current.binding != next.binding
        || current.lifecycle != next.lifecycle
        || current.result_disposition != next.result_disposition
        || current.failure != next.failure
        || current.created_at_unix_ms != next.created_at_unix_ms
    {
        return Err(HarnessEngineError::InvalidContextPackRecordProjection);
    }
    let Some(candidate) = &next.context_pack else {
        return Err(HarnessEngineError::InvalidContextPackRecordProjection);
    };
    match &current.context_pack {
        None => Ok(()),
        Some(existing) if existing == candidate => Ok(()),
        Some(_) => Err(HarnessEngineError::TerminalRunMutation),
    }
}

/// Guards `prepare_run_git_facts_record`: only `git_facts`, `revision`, and
/// `updated_at_unix_ms` may differ between `current` and `next` (the
/// structural twin of `validate_context_pack_recording`, symmetrically
/// freezing `context_pack` alongside every other run field). A run already
/// frozen by H4 (`OutcomeUnknown`) stays frozen; `git_facts` transitions
/// `None -> Some(_)` exactly once — a replay of the exact same value is
/// accepted, any other change (including `Some(x)` to a different `Some(y)`)
/// is a hard conflict, never a silent overwrite.
fn validate_git_facts_recording(
    current: &HarnessRunV1,
    next: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    if current.lifecycle == HarnessRunLifecycleV1::OutcomeUnknown {
        return Err(HarnessEngineError::OutcomeUnknownRunFrozen);
    }
    if current.run_id != next.run_id
        || current.parent_run_id != next.parent_run_id
        || current.task_id != next.task_id
        || current.operation_id != next.operation_id
        || current.intent != next.intent
        || current.delivery_receipt != next.delivery_receipt
        || current.continuation_receipt != next.continuation_receipt
        || current.context_pack != next.context_pack
        || current.binding != next.binding
        || current.lifecycle != next.lifecycle
        || current.result_disposition != next.result_disposition
        || current.failure != next.failure
        || current.created_at_unix_ms != next.created_at_unix_ms
    {
        return Err(HarnessEngineError::InvalidGitFactsRecordProjection);
    }
    let Some(candidate) = &next.git_facts else {
        return Err(HarnessEngineError::InvalidGitFactsRecordProjection);
    };
    match &current.git_facts {
        None => Ok(()),
        Some(existing) if existing == candidate => Ok(()),
        Some(_) => Err(HarnessEngineError::TerminalRunMutation),
    }
}

/// Guards `prepare_task_result_ref_record`: every task field except
/// `result_refs`, `revision`, and `updated_at_unix_ms` must stay
/// byte-identical — `task.state` in particular never changes here. Below
/// `HARNESS_RESULTS_MAX` capacity, `next.result_refs` must equal
/// `current.result_refs` with either no change (a legal replay of an
/// already-present ref) or exactly one new ref inserted at its sorted
/// position; at capacity, only the unchanged no-op is legal — an attempt to
/// grow past the bound is rejected, never silently truncated.
fn validate_task_result_ref_recording(
    current: &HarnessTaskV1,
    next: &HarnessTaskV1,
) -> Result<(), HarnessEngineError> {
    if current.task_id != next.task_id
        || current.title != next.title
        || current.body != next.body
        || current.creator != next.creator
        || current.parent_task_id != next.parent_task_id
        || current.dependencies != next.dependencies
        || current.run_ids != next.run_ids
        || current.state != next.state
        || current.artifact_refs != next.artifact_refs
        || current.created_at_unix_ms != next.created_at_unix_ms
    {
        return Err(HarnessEngineError::InvalidResultRefRecordProjection);
    }
    if current.result_refs.len() >= HARNESS_RESULTS_MAX {
        return (next.result_refs == current.result_refs)
            .then_some(())
            .ok_or(HarnessEngineError::InvalidResultRefRecordProjection);
    }
    validate_single_sorted_insertion(&current.result_refs, &next.result_refs)
        .map_err(|()| HarnessEngineError::InvalidResultRefRecordProjection)
}

/// Verifies `next` equals `current` with either no change (a legal replay
/// of an already-present entry) or exactly one additional element inserted
/// at its correct sorted position. Both slices are required to already be
/// individually sorted-ascending (enforced by `HarnessTaskV1::validate()`
/// before this runs), so a merge-style scan is sufficient to reject every
/// other delta: removal, reorder, substitution, or more than one insertion.
fn validate_single_sorted_insertion(
    current: &[HarnessResultRef],
    next: &[HarnessResultRef],
) -> Result<(), ()> {
    if current == next {
        return Ok(());
    }
    if next.len() != current.len() + 1 {
        return Err(());
    }
    let mut current_iter = current.iter().peekable();
    let mut inserted = false;
    for item in next {
        if current_iter.peek() == Some(&item) {
            current_iter.next();
        } else if inserted {
            return Err(());
        } else {
            inserted = true;
        }
    }
    if current_iter.next().is_some() {
        return Err(());
    }
    Ok(())
}

fn validate_coupled_run_operation(
    current: &HarnessRunV1,
    next: &HarnessRunV1,
    operation: &HarnessOperationV1,
) -> Result<(), HarnessEngineError> {
    use gate4agent_harness_protocol::HarnessRunLifecycleV1;

    let allowed = operation.kind == HarnessOperationKindV1::CreateRun
        && matches!(
            (current.lifecycle, next.lifecycle, operation.state),
            (
                HarnessRunLifecycleV1::Requested,
                HarnessRunLifecycleV1::Dispatching,
                HarnessOperationStateV1::Dispatching,
            )
                | (
                    HarnessRunLifecycleV1::Requested,
                    HarnessRunLifecycleV1::Failed,
                    HarnessOperationStateV1::Failed,
                )
                | (
                    HarnessRunLifecycleV1::Requested,
                    HarnessRunLifecycleV1::OutcomeUnknown,
                    HarnessOperationStateV1::OutcomeUnknown,
                )
                | (
                    HarnessRunLifecycleV1::Preparing,
                    HarnessRunLifecycleV1::Failed,
                    HarnessOperationStateV1::Failed,
                )
                | (
                    HarnessRunLifecycleV1::Preparing,
                    HarnessRunLifecycleV1::OutcomeUnknown,
                    HarnessOperationStateV1::OutcomeUnknown,
                )
                | (
                    HarnessRunLifecycleV1::Dispatching,
                    HarnessRunLifecycleV1::OutcomeUnknown,
                    HarnessOperationStateV1::OutcomeUnknown,
                )
                | (
                    HarnessRunLifecycleV1::Dispatching,
                    HarnessRunLifecycleV1::Running,
                    HarnessOperationStateV1::Succeeded,
                )
                | (
                    HarnessRunLifecycleV1::Dispatching,
                    HarnessRunLifecycleV1::Failed,
                    HarnessOperationStateV1::Failed,
                )
        );
    if !allowed {
        return Err(HarnessEngineError::InvalidCoupledRunOperation);
    }
    Ok(())
}

fn validate_run_immutable(
    current: &HarnessRunV1,
    next: &HarnessRunV1,
) -> Result<(), HarnessEngineError> {
    if current.parent_run_id != next.parent_run_id
        || current.task_id != next.task_id
        || current.operation_id != next.operation_id
        || current.intent != next.intent
    {
        return Err(HarnessEngineError::MismatchedIdentity("immutable run request"));
    }
    Ok(())
}

fn validate_run_operation_coherence(
    run: &HarnessRunV1,
    operation: &HarnessOperationV1,
) -> Result<(), HarnessEngineError> {
    use gate4agent_harness_protocol::HarnessRunLifecycleV1;

    let coherent = match run.lifecycle {
        HarnessRunLifecycleV1::Requested | HarnessRunLifecycleV1::Preparing => {
            operation.state == HarnessOperationStateV1::Prepared
        }
        HarnessRunLifecycleV1::Dispatching => {
            operation.state == HarnessOperationStateV1::Dispatching
        }
        HarnessRunLifecycleV1::OutcomeUnknown => {
            operation.state == HarnessOperationStateV1::OutcomeUnknown
        }
        HarnessRunLifecycleV1::Running
        | HarnessRunLifecycleV1::Waiting
        | HarnessRunLifecycleV1::Completed => {
            operation.state == HarnessOperationStateV1::Succeeded
        }
        HarnessRunLifecycleV1::Failed => matches!(
            operation.state,
            HarnessOperationStateV1::Failed | HarnessOperationStateV1::Succeeded
        ),
        HarnessRunLifecycleV1::Cancelled => matches!(
            operation.state,
            HarnessOperationStateV1::Failed | HarnessOperationStateV1::Succeeded
        ),
    };
    if !coherent {
        return Err(HarnessEngineError::RunOperationStateIncoherent {
            lifecycle: run.lifecycle,
            operation_state: operation.state,
        });
    }
    Ok(())
}

fn validate_grant_transition(
    current: &SessionGrantV1,
    next: &SessionGrantV1,
    operation_kind: HarnessOperationKindV1,
) -> Result<(), HarnessEngineError> {
    use gate4agent_harness_protocol::SessionGrantStateV1;

    if current.state == SessionGrantStateV1::Revoked {
        return Err(HarnessEngineError::RevokedGrantMutation);
    }
    match operation_kind {
        HarnessOperationKindV1::RevokeGrant
            if next.state == SessionGrantStateV1::Revoked => Ok(()),
        HarnessOperationKindV1::MutateGrant if next.state == current.state => Ok(()),
        HarnessOperationKindV1::RevokeGrant => Err(HarnessEngineError::RevokeGrantMustRevoke),
        HarnessOperationKindV1::MutateGrant => Err(HarnessEngineError::GrantStateRequiresRevoke),
        _ => Err(HarnessEngineError::WrongOperationKind),
    }
}

#[derive(Debug, Error)]
pub enum HarnessEngineError {
    #[error(transparent)]
    Validation(#[from] HarnessValidationError),
    #[error("unsupported harness checkpoint version {0}")]
    UnsupportedCheckpoint(u16),
    #[error("duplicate checkpoint identity {0}")]
    DuplicateCheckpointId(String),
    #[error("harness entity {0} already exists")]
    AlreadyExists(String),
    #[error("harness entity {0} was not found")]
    NotFound(String),
    #[error("operation id {operation_id} was reused with a different request digest")]
    OperationIdConflict { operation_id: HarnessOperationId },
    #[error("task {0} cannot carry both legacy and issuance-bound execution specifications")]
    DualExecutionSpec(String),
    #[error("task launch issuance has no matching V2 execution specification")]
    OrphanIssuance,
    #[error("V2 execution specification has no matching task launch issuance")]
    OrphanIssuedExecutionSpec,
    #[error("task launch issuance and V2 execution specification do not have exact task/id/revision/digest linkage")]
    InvalidIssuanceLink,
    #[error("issued execution CAS does not match current spec {spec_revision:?} and issuance {issuance_revision:?} revisions")]
    IssuedExecutionCasMismatch {
        spec_revision: Option<u64>,
        issuance_revision: Option<u64>,
    },
    #[error("wrong harness operation kind for mutation")]
    WrongOperationKind,
    #[error("wrong harness operation state: expected {expected:?}, actual {actual:?}")]
    WrongOperationState {
        expected: HarnessOperationStateV1,
        actual: HarnessOperationStateV1,
    },
    #[error("mismatched {0} identity")]
    MismatchedIdentity(&'static str),
    #[error("{entity} expected revision {expected}, actual {actual:?}")]
    ExpectedRevisionMismatch {
        entity: &'static str,
        expected: u64,
        actual: Option<u64>,
    },
    #[error("{entity} next revision is not the exact successor")]
    InvalidNextRevision { entity: &'static str },
    #[error("{entity} created timestamp changed")]
    CreatedTimestampChanged { entity: &'static str },
    #[error("cycle in {0}")]
    Cycle(&'static str),
    #[error("invalid operation state transition from {from:?} to {to:?}")]
    InvalidOperationTransition {
        from: HarnessOperationStateV1,
        to: HarnessOperationStateV1,
    },
    #[error("invalid run lifecycle transition from {from:?} to {to:?}")]
    InvalidRunLifecycleTransition {
        from: gate4agent_harness_protocol::HarnessRunLifecycleV1,
        to: gate4agent_harness_protocol::HarnessRunLifecycleV1,
    },
    #[error("terminal harness run metadata is immutable")]
    TerminalRunMutation,
    #[error("OutcomeUnknown run is frozen until H4 authoritative reconciliation")]
    OutcomeUnknownRunFrozen,
    #[error("generic run mutation does not have an exact succeeded BindRun/MutateRun authority")]
    InvalidGenericRunOperation,
    #[error("run lifecycle event operation does not have exact target and actor authority")]
    InvalidRunEventAuthority,
    #[error("run lifecycle event does not have the exact atomic task projection")]
    InvalidRunEventTaskProjection,
    #[error("host-incarnation settlement operation does not have exact single-run atomic authority")]
    InvalidIncarnationSettlementAuthority,
    #[error("host-incarnation settlement does not have the exact atomic run/task projection")]
    InvalidIncarnationSettlementProjection,
    #[error("ContextPack record operation does not have exact single-run, task-free authority")]
    InvalidContextPackRecordAuthority,
    #[error("ContextPack record must not change any run field besides context_pack, revision, or updated_at_unix_ms")]
    InvalidContextPackRecordProjection,
    #[error("git-facts record operation does not have exact single-run, task-free authority")]
    InvalidGitFactsRecordAuthority,
    #[error("git-facts record must not change any run field besides git_facts, revision, or updated_at_unix_ms")]
    InvalidGitFactsRecordProjection,
    #[error("result-ref record operation does not have exact single-task, run-free authority")]
    InvalidResultRefRecordAuthority,
    #[error("result-ref record must not change any task field besides result_refs, revision, or updated_at_unix_ms")]
    InvalidResultRefRecordProjection,
    #[error("CreateRun dispatch outcome does not have the exact atomic run/operation/task projection")]
    InvalidDispatchOutcomeProjection,
    #[error("coupled CreateRun operation state does not match run lifecycle")]
    InvalidCoupledRunOperation,
    #[error("run operation requires an atomic run and operation transition")]
    RunOperationRequiresAtomicTransition,
    #[error("RevokeGrant operation must produce a revoked grant")]
    RevokeGrantMustRevoke,
    #[error("grant state can change only through RevokeGrant")]
    GrantStateRequiresRevoke,
    #[error("revoked grant is terminal and cannot be mutated or reactivated")]
    RevokedGrantMutation,
    #[error("new session grant must start active")]
    GrantMustStartActive,
    #[error("new harness run must start Requested without a binding")]
    InvalidInitialRunState,
    #[error("issued run requires exact current issuance and execution spec authority")]
    IssuedExecutionAuthorityMissing,
    #[error("issued run task projection must atomically move Ready to Running and append one run")]
    InvalidIssuedRunProjection,
    #[error("issued run transfer authority or exact issuance selection does not match")]
    IssuedExecutionTransferMismatch,
    #[error("run lifecycle {lifecycle:?} is incoherent with original operation state {operation_state:?}")]
    RunOperationStateIncoherent {
        lifecycle: gate4agent_harness_protocol::HarnessRunLifecycleV1,
        operation_state: HarnessOperationStateV1,
    },
    #[error("harness read visibility scan exceeded its fixed resource bound")]
    ReadVisibilityResourceExhausted,
    #[error("harness read visibility graph is invalid")]
    ReadVisibilityInvalidGraph,
    #[error("harness scheduler scan exceeded its fixed resource bound")]
    SchedulerResourceExhausted,
    #[error("harness scheduler durable task/run/operation graph is invalid")]
    SchedulerInvalidGraph,
    #[error("new delivery authority must start Prepared")]
    InvalidInitialDeliveryState,
    #[error("delivery reference {delivery_ref} was reused with a different authority")]
    DeliveryIdConflict { delivery_ref: HarnessDeliveryRef },
    #[error("delivery authority is not permitted by the exact active grant")]
    DeliveryGrantDenied,
    #[error("delivery authority has an invalid task/run/operation/grant link")]
    InvalidDeliveryLink,
    #[error("delivery state transition is invalid or terminal")]
    InvalidDeliveryTransition,
    #[error("delivery immutable authority or staged receipt changed")]
    InvalidDeliveryImmutableMutation,
    #[error("delivery commit must atomically attach its exact receipt to the bound run")]
    InvalidDeliveryRunCommit,
    #[error("new continuation authority must start Prepared")]
    InvalidInitialContinuationState,
    #[error("continuation reference {continuation_ref} was reused with different authority")]
    ContinuationIdConflict { continuation_ref: HarnessContinuationRef },
    #[error("continuation authority capacity exceeded")]
    ContinuationCapacityExceeded,
    #[error("continuation authority is not permitted by the exact active grant")]
    ContinuationGrantDenied,
    #[error("continuation authority has an invalid source/target/operation link")]
    InvalidContinuationLink,
    #[error("continuation authority may be prepared or exported only before dispatch")]
    ContinuationAuthorityWindowClosed,
    #[error("continuation state transition is invalid or terminal")]
    InvalidContinuationTransition,
    #[error("continuation immutable authority or exported receipt changed")]
    InvalidContinuationImmutableMutation,
    #[error("continuation bind must atomically attach exact receipt and target binding")]
    InvalidContinuationRunBind,
    #[error("delivery and continuation spawn must commit all authorities atomically")]
    InvalidCombinedSpawnCommit,
    #[error("a harness run cannot have multiple continuation authorities")]
    DuplicateContinuationRun,
    #[error("continuation receipt reference is not unique")]
    DuplicateContinuationReceipt,
    #[error("a harness run cannot have more than one durable delivery authority")]
    DuplicateDeliveryRun,
    #[error("delivery authority can be prepared or staged only before CreateRun dispatch")]
    DeliveryAuthorityWindowClosed,
    #[error("durable delivery authority exceeded its fixed record bound")]
    DeliveryCapacityExceeded,
    #[error("delivery restage must change only to a different current authoritative Node incarnation")]
    InvalidDeliveryRestage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_protocol::{
        HarnessActorV1, HarnessContextPermissionsV1, HarnessContextSourceSelectionV1,
        HarnessContextSourceAvailabilityV1,
        HarnessEntityReadScopeV1,
        HarnessContextPackLineageV1, HarnessContinuationCleanupStateV1,
        HarnessDeliveryBundleDigestV1, HarnessDeliveryBundleIdV1,
        HarnessDeliveryBundleRevisionV1, HarnessDeliveryBundleSelectionV1,
        HarnessDeliveryBundleV1, HarnessDeliveryComponentCountV1,
        HarnessDeliveryComponentKindV1,
        HarnessDeliveryManifestDigestV2,
        HarnessDeliveryRef, HarnessDeliveryStageReceiptV1,
        HarnessExecutionModeV1, HarnessExecutionSpecId, HarnessFailureCategoryV1,
        HarnessFailureV1, HarnessLaunchAuthorityRefV1, HarnessLaunchPlanRefV1,
        HarnessLaunchTargetSelectionV1, HarnessLaunchWorktreeSelectionV1,
        HarnessGrantTargetV1, HarnessIdempotencyRef,
        HarnessMonitoringVisibilityV1, HarnessOutcomeUnknownReasonV1,
        HarnessReadPermissionsV1,
        HarnessOperationKindV1, HarnessOperationStateV1, HarnessOperationTimeoutsV1,
        HarnessReceiptRef, HarnessRequestDigest, HarnessResolvedContextPackReceiptV1,
        HarnessResultDispositionV1,
        HarnessRunGitCommitSummaryV1, HarnessRunGitFactsOutcomeV1, HarnessRunGitFactsV1,
        HarnessRunGitStatusCodeV1, HarnessRunGitStatusEntryV1, HarnessRunGitSummaryV1,
        HarnessRunIntentV1, HarnessRunLifecycleV1, HarnessRuntimeIdentityV1,
        HarnessScheduledLaunchRefV2, HarnessTaskReviewPolicyV1,
        HarnessSelectorV1, HarnessSessionBindingV1, HarnessSessionIdentityV1,
        HarnessTaskLaunchIssuanceId, HarnessTaskPermissionsV1,
        HarnessTaskStateV1, HarnessTransferAuthorityRefV1, HarnessWorktreeIntentV1,
        SessionGrantStateV1,
    };

    fn delivery_ref(hex: char) -> HarnessDeliveryRef {
        HarnessDeliveryRef::new(format!("hdelivery_{}", hex.to_string().repeat(24))).unwrap()
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

    fn delivery(state: HarnessDeliveryStateV1, revision_value: u64) -> HarnessDeliveryV1 {
        let bundle = delivery_bundle();
        let stage_receipt = matches!(state, HarnessDeliveryStateV1::Staged | HarnessDeliveryStateV1::Committed)
            .then(|| HarnessDeliveryStageReceiptV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                node_incarnation: HarnessSelectorV1::new("incarnation-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                bundle: bundle.clone(),
                staged_at_unix_ms: 20,
            });
        HarnessDeliveryV1 {
            delivery_ref: delivery_ref('d'),
            revision: revision(revision_value),
            authority: HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id: grant_id(),
                revision: revision(1),
            },
            task_id: task_id('a'),
            run_id: run_id(),
            operation_id: operation_id('b'),
            bundle,
            state,
            stage_receipt,
            receipt: None,
            created_at_unix_ms: 15,
            updated_at_unix_ms: 15 + revision_value * 5,
        }
    }

    fn task_id(hex: char) -> HarnessTaskId {
        HarnessTaskId::new(format!("htask_{}", hex.to_string().repeat(24))).unwrap()
    }

    fn operation_id(hex: char) -> HarnessOperationId {
        HarnessOperationId::new(format!("hop_{}", hex.to_string().repeat(24))).unwrap()
    }

    fn run_id() -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap()
    }

    fn numbered_run_id(value: usize) -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{value:024x}")).unwrap()
    }

    fn numbered_task_id(value: usize) -> HarnessTaskId {
        HarnessTaskId::new(format!("htask_{value:024x}")).unwrap()
    }

    fn grant_id() -> SessionGrantId {
        SessionGrantId::new(format!("hgrant_{}", "a".repeat(24))).unwrap()
    }

    fn revision(value: u64) -> HarnessRevision {
        HarnessRevision::new(value).unwrap()
    }

    fn task(id: HarnessTaskId, revision_value: u64, title: &str) -> HarnessTaskV1 {
        HarnessTaskV1 {
            task_id: id,
            revision: revision(revision_value),
            title: title.to_owned(),
            body: String::new(),
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
            updated_at_unix_ms: 10 + revision_value,
        }
    }

    fn task_operation(
        id: HarnessOperationId,
        task_id: HarnessTaskId,
        digest_hex: char,
        expected_revision: Option<HarnessRevision>,
    ) -> HarnessOperationV1 {
        HarnessOperationV1 {
            operation_id: id,
            revision: revision(1),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: if expected_revision.is_some() {
                HarnessOperationKindV1::MutateTask
            } else {
                HarnessOperationKindV1::CreateTask
            },
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id),
            run_id: None,
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision,
            request_digest: HarnessRequestDigest::new(digest_hex.to_string().repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                digest_hex.to_string().repeat(24)
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 12,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(12),
        }
    }

    fn create_task(engine: &mut HarnessEngine) -> (HarnessTaskId, HarnessOperationV1) {
        let id = task_id('a');
        let operation = task_operation(operation_id('a'), id.clone(), 'a', None);
        let prepared = engine.prepare(HarnessMutationV1::CreateTask {
            operation: operation.clone(),
            task: task(id.clone(), 1, "first"),
        }).unwrap();
        engine.accept(prepared);
        (id, operation)
    }

    fn execution_spec(task_id: HarnessTaskId, revision_value: u64) -> HarnessTaskExecutionSpecV1 {
        HarnessTaskExecutionSpecV1 {
            execution_spec_id: HarnessExecutionSpecId::new(format!(
                "hespec_{}",
                "e".repeat(24),
            )).unwrap(),
            revision: revision(revision_value),
            task_id,
            scheduled_launch: HarnessScheduledLaunchRefV2 {
                plan: HarnessLaunchPlanRefV1 {
                    plan_id: HarnessSelectorV1::new("ordinary").unwrap(),
                    revision: revision(1),
                    digest: HarnessRequestDigest::new("a".repeat(64)).unwrap(),
                },
                authority: HarnessLaunchAuthorityRefV1::OrdinaryOperator,
            },
            scheduled_launch_digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
            review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20 + revision_value,
        }
    }

    fn task_launch_issuance(
        task_id: HarnessTaskId,
        revision_value: u64,
    ) -> HarnessTaskLaunchIssuanceV1 {
        HarnessTaskLaunchIssuanceV1 {
            issuance_id: HarnessTaskLaunchIssuanceId::new(format!(
                "hissue_{}",
                "f".repeat(24),
            )).unwrap(),
            revision: revision(revision_value),
            digest: HarnessRequestDigest::new(
                if revision_value == 1 { "c" } else { "d" }.repeat(64),
            ).unwrap(),
            task_id,
            task_revision: revision(1),
            plan: HarnessLaunchPlanRefV1 {
                plan_id: HarnessSelectorV1::new("issued").unwrap(),
                revision: revision(2),
                digest: HarnessRequestDigest::new("a".repeat(64)).unwrap(),
            },
            target: HarnessLaunchTargetSelectionV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessLaunchWorktreeSelectionV1::Managed {
                    profile_id: HarnessSelectorV1::new("review-tree").unwrap(),
                    expected_profile_revision: HarnessSelectorV1::new("revision-7").unwrap(),
                },
                provider_profile: HarnessSelectorV1::new("claude").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
            },
            context_source: None,
            delivery: None,
            policy_digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20 + revision_value,
        }
    }

    fn issued_execution_spec(
        issuance: &HarnessTaskLaunchIssuanceV1,
        revision_value: u64,
    ) -> HarnessTaskExecutionSpecV2 {
        HarnessTaskExecutionSpecV2 {
            execution_spec_id: HarnessExecutionSpecId::new(format!(
                "hespec_{}",
                "f".repeat(24),
            )).unwrap(),
            revision: revision(revision_value),
            task_id: issuance.task_id.clone(),
            launch_issuance: issuance.reference(),
            review_policy: HarnessTaskReviewPolicyV1::OperatorReview,
            created_at_unix_ms: 20,
            updated_at_unix_ms: 20 + revision_value,
        }
    }

    fn run(lifecycle: HarnessRunLifecycleV1, revision_value: u64) -> HarnessRunV1 {
        let binding = matches!(
            lifecycle,
            HarnessRunLifecycleV1::Running
                | HarnessRunLifecycleV1::Waiting
                | HarnessRunLifecycleV1::Completed
        ).then(|| HarnessSessionBindingV1 {
            node_id: HarnessSelectorV1::new("node-a").unwrap(),
            node_incarnation: HarnessSelectorV1::new("incarnation-a").unwrap(),
            workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
            session: HarnessSessionIdentityV1::Managed {
                record_id: HarnessSelectorV1::new("record-a").unwrap(),
                active_session: Some(HarnessRuntimeIdentityV1 {
                    instance_id: 1,
                    generation: 1,
                }),
            },
        });
        HarnessRunV1 {
            run_id: run_id(),
            revision: revision(revision_value),
            parent_run_id: None,
            task_id: task_id('a'),
            operation_id: operation_id('b'),
            intent: HarnessRunIntentV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: HarnessSelectorV1::new("claude").unwrap(),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding,
            lifecycle,
            result_disposition: match lifecycle {
                HarnessRunLifecycleV1::Completed => Some(HarnessResultDispositionV1::Succeeded),
                HarnessRunLifecycleV1::Failed => Some(HarnessResultDispositionV1::Failed),
                HarnessRunLifecycleV1::Cancelled => Some(HarnessResultDispositionV1::Cancelled),
                _ => None,
            },
            failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10 + revision_value,
        }
    }

    fn original_run_operation(state: HarnessOperationStateV1) -> HarnessOperationV1 {
        let dispatched = (state != HarnessOperationStateV1::Prepared).then_some(12);
        HarnessOperationV1 {
            operation_id: operation_id('b'),
            revision: revision(1),
            actor: HarnessActorV1::User {
                actor_id: HarnessSelectorV1::new("operator").unwrap(),
            },
            kind: HarnessOperationKindV1::CreateRun,
            state,
            task_id: Some(task_id('a')),
            run_id: Some(run_id()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(revision(1)),
            request_digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "b".repeat(24)
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: dispatched.unwrap_or(10),
            dispatched_at_unix_ms: dispatched,
            finished_at_unix_ms: None,
        }
    }

    fn grant(state: SessionGrantStateV1, revision_value: u64) -> SessionGrantV1 {
        SessionGrantV1 {
            grant_id: grant_id(),
            revision: revision(revision_value),
            actor_run_id: run_id(),
            allowed_targets: vec![HarnessGrantTargetV1 {
                node_id: HarnessSelectorV1::new("node-a").unwrap(),
                workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                provider_profile: HarnessSelectorV1::new("claude").unwrap(),
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
                request_run: false,
            },
            read_permissions: HarnessReadPermissionsV1::default(),
            monitoring_visibility: HarnessMonitoringVisibilityV1::Summary,
            context_permissions: HarnessContextPermissionsV1 {
                export: false,
                restore: false,
            },
            state,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10 + revision_value,
        }
    }

    fn visibility_run(
        id: HarnessRunId,
        parent_run_id: Option<HarnessRunId>,
        owning_task_id: HarnessTaskId,
    ) -> HarnessRunV1 {
        let mut value = run(HarnessRunLifecycleV1::Requested, 1);
        value.run_id = id;
        value.parent_run_id = parent_run_id;
        value.task_id = owning_task_id;
        value
    }

    fn visibility_reconcile_operation(
        id: HarnessOperationId,
        digest_hex: char,
        actor_run_id: HarnessRunId,
        run_id: HarnessRunId,
        target_id: HarnessOperationId,
    ) -> HarnessOperationV1 {
        let mut operation = task_operation(
            id,
            numbered_task_id(1),
            digest_hex,
            Some(revision(1)),
        );
        operation.actor = HarnessActorV1::ParentRun { run_id: actor_run_id };
        operation.kind = HarnessOperationKindV1::Reconcile;
        operation.task_id = None;
        operation.run_id = Some(run_id);
        operation.reconciles_operation_id = Some(target_id);
        operation
    }

    fn visibility_fixture() -> HarnessEngine {
        let root_run_id = numbered_run_id(1);
        let child_run_id = numbered_run_id(2);
        let grandchild_run_id = numbered_run_id(3);
        let outside_run_id = numbered_run_id(4);
        let root_task_id = numbered_task_id(1);
        let child_task_id = numbered_task_id(2);
        let grandchild_task_id = numbered_task_id(3);
        let outside_task_id = numbered_task_id(4);
        let authored_task_id = numbered_task_id(5);
        let root_authored_task_id = numbered_task_id(6);
        let cross_linked_task_id = numbered_task_id(7);

        let mut engine = HarnessEngine::new();
        for (id, run_ids, creator) in [
            (root_task_id.clone(), vec![root_run_id.clone()], None),
            (child_task_id.clone(), vec![child_run_id.clone()], None),
            (grandchild_task_id.clone(), vec![grandchild_run_id.clone()], None),
            (outside_task_id.clone(), vec![outside_run_id.clone()], None),
            (
                authored_task_id,
                Vec::new(),
                Some(HarnessActorV1::ParentRun { run_id: child_run_id.clone() }),
            ),
            (
                root_authored_task_id,
                Vec::new(),
                Some(HarnessActorV1::ParentRun { run_id: root_run_id.clone() }),
            ),
        ] {
            let mut record = task(id.clone(), 1, "visible fixture");
            record.run_ids = run_ids;
            if let Some(creator) = creator {
                record.creator = creator;
            }
            engine.tasks.insert(id, record);
        }
        let mut cross_linked = task(cross_linked_task_id.clone(), 1, "cross-tree link");
        cross_linked.parent_task_id = Some(root_task_id.clone());
        cross_linked.dependencies = vec![child_task_id.clone()];
        engine.tasks.insert(cross_linked_task_id, cross_linked);
        for record in [
            visibility_run(root_run_id.clone(), None, root_task_id.clone()),
            visibility_run(
                child_run_id.clone(),
                Some(root_run_id.clone()),
                child_task_id.clone(),
            ),
            visibility_run(
                grandchild_run_id,
                Some(child_run_id.clone()),
                grandchild_task_id,
            ),
            visibility_run(outside_run_id, None, outside_task_id.clone()),
        ] {
            engine.runs.insert(record.run_id.clone(), record);
        }

        let mut root_operation = task_operation(
            operation_id('1'),
            root_task_id,
            '1',
            None,
        );
        root_operation.actor = HarnessActorV1::ParentRun {
            run_id: root_run_id.clone(),
        };
        let mut child_operation = task_operation(
            operation_id('2'),
            child_task_id.clone(),
            '2',
            None,
        );
        child_operation.actor = HarnessActorV1::ParentRun {
            run_id: child_run_id.clone(),
        };
        let mut parent_operation = task_operation(
            operation_id('3'),
            outside_task_id.clone(),
            '3',
            None,
        );
        parent_operation.actor = HarnessActorV1::ParentRun {
            run_id: child_run_id.clone(),
        };
        let outside_operation = task_operation(
            operation_id('4'),
            outside_task_id,
            '4',
            None,
        );
        let mut child_run_operation = task_operation(
            operation_id('5'),
            child_task_id,
            '5',
            Some(revision(1)),
        );
        child_run_operation.actor = HarnessActorV1::ParentRun {
            run_id: child_run_id.clone(),
        };
        child_run_operation.kind = HarnessOperationKindV1::BindRun;
        child_run_operation.task_id = None;
        child_run_operation.run_id = Some(child_run_id);
        let mut grant_operation = task_operation(
            operation_id('6'),
            numbered_task_id(1),
            '6',
            None,
        );
        grant_operation.actor = HarnessActorV1::ParentRun {
            run_id: root_run_id.clone(),
        };
        grant_operation.kind = HarnessOperationKindV1::CreateGrant;
        grant_operation.task_id = None;
        grant_operation.grant_id = Some(grant_id());
        let visible_reconcile = visibility_reconcile_operation(
            operation_id('7'),
            '7',
            root_run_id.clone(),
            root_run_id.clone(),
            operation_id('1'),
        );
        let visible_reconcile_chain = visibility_reconcile_operation(
            operation_id('8'),
            '8',
            root_run_id.clone(),
            root_run_id.clone(),
            operation_id('7'),
        );
        let hidden_reconcile = visibility_reconcile_operation(
            operation_id('9'),
            '9',
            root_run_id.clone(),
            root_run_id.clone(),
            operation_id('4'),
        );
        for operation in [
            root_operation,
            child_operation,
            parent_operation,
            outside_operation,
            child_run_operation,
            grant_operation,
            visible_reconcile,
            visible_reconcile_chain,
            hidden_reconcile,
        ] {
            engine.operations.insert(operation.operation_id.clone(), operation);
        }

        let mut scoped_grant = grant(SessionGrantStateV1::Active, 1);
        scoped_grant.actor_run_id = root_run_id;
        scoped_grant.read_permissions = HarnessReadPermissionsV1 {
            tasks: HarnessEntityReadScopeV1::Descendants,
            runs: HarnessEntityReadScopeV1::Descendants,
            operations: HarnessEntityReadScopeV1::Descendants,
        };
        engine.grants.insert(scoped_grant.grant_id.clone(), scoped_grant);
        engine
    }

    #[test]
    fn harness_task_cas_conflict_never_mutates() {
        let mut engine = HarnessEngine::new();
        let (id, _) = create_task(&mut engine);
        let before = engine.checkpoint();
        let operation = task_operation(
            operation_id('b'),
            id.clone(),
            'b',
            Some(revision(2)),
        );
        let error = engine.prepare(HarnessMutationV1::ReplaceTask {
            operation,
            expected_revision: revision(2),
            task: task(id, 3, "stale"),
        }).unwrap_err();
        assert!(matches!(error, HarnessEngineError::ExpectedRevisionMismatch { .. }));
        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn harness_execution_spec_cas_replays_and_restores_from_checkpoint() {
        let mut engine = HarnessEngine::new();
        let (task_id, _) = create_task(&mut engine);
        let mut operation = task_operation(
            operation_id('b'),
            task_id.clone(),
            'b',
            Some(revision(1)),
        );
        operation.kind = HarnessOperationKindV1::MutateExecutionSpec;
        let first_spec = execution_spec(task_id.clone(), 1);
        let first = HarnessMutationV1::PutExecutionSpec {
            operation: operation.clone(),
            expected_task_revision: revision(1),
            expected_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            spec: first_spec.clone(),
        };
        let prepared = engine.prepare(first.clone()).unwrap();
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        engine.accept(prepared);
        assert_eq!(
            engine.prepare(first).unwrap().outcome(),
            HarnessApplyOutcome::Replayed,
        );

        let before = engine.checkpoint();
        let mut stale_operation = task_operation(
            operation_id('c'),
            task_id.clone(),
            'c',
            Some(revision(1)),
        );
        stale_operation.kind = HarnessOperationKindV1::MutateExecutionSpec;
        assert!(matches!(
            engine.prepare(HarnessMutationV1::PutExecutionSpec {
                operation: stale_operation,
                expected_task_revision: revision(1),
                expected_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                spec: first_spec.clone(),
            }),
            Err(HarnessEngineError::ExpectedRevisionMismatch {
                entity: "execution spec",
                ..
            }),
        ));
        assert_eq!(engine.checkpoint(), before);

        let mut replacement = first_spec;
        replacement.revision = revision(2);
        replacement.updated_at_unix_ms = 22;
        let mut replace_operation = task_operation(
            operation_id('d'),
            task_id.clone(),
            'd',
            Some(revision(1)),
        );
        replace_operation.kind = HarnessOperationKindV1::MutateExecutionSpec;
        let prepared = engine.prepare(HarnessMutationV1::PutExecutionSpec {
            operation: replace_operation,
            expected_task_revision: revision(1),
            expected_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Exact(revision(1)),
            spec: replacement.clone(),
        }).unwrap();
        engine.accept(prepared);
        let restored = HarnessEngine::restore(engine.checkpoint()).unwrap();
        assert_eq!(restored.execution_spec(&task_id), Some(&replacement));
    }

    #[test]
    fn issued_execution_spec_cas_is_atomic_replayable_and_restore_exact() {
        let mut engine = HarnessEngine::new();
        let (task_id, _) = create_task(&mut engine);
        let mut operation = task_operation(
            operation_id('b'),
            task_id.clone(),
            'b',
            Some(revision(1)),
        );
        operation.kind = HarnessOperationKindV1::MutateExecutionSpec;
        let issuance = task_launch_issuance(task_id.clone(), 1);
        let spec = issued_execution_spec(&issuance, 1);
        let mutation = HarnessMutationV1::PutIssuedExecutionSpec {
            operation: operation.clone(),
            expected_task_revision: revision(1),
            expected_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            expected_issuance_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
            issuance: issuance.clone(),
            spec: spec.clone(),
        };
        let prepared = engine.prepare(mutation.clone()).unwrap();
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        engine.accept(prepared);
        assert_eq!(
            engine.prepare(mutation).unwrap().outcome(),
            HarnessApplyOutcome::Replayed,
        );
        assert_eq!(engine.task_launch_issuance(&task_id), Some(&issuance));
        assert_eq!(engine.task_execution_spec_v2(&task_id), Some(&spec));

        let restored = HarnessEngine::restore(engine.checkpoint()).unwrap();
        assert_eq!(restored.task_launch_issuance(&task_id), Some(&issuance));
        assert_eq!(restored.task_execution_spec_v2(&task_id), Some(&spec));

        let before = engine.checkpoint();
        let mut mismatched_operation = task_operation(
            operation_id('c'),
            task_id.clone(),
            'c',
            Some(revision(1)),
        );
        mismatched_operation.kind = HarnessOperationKindV1::MutateExecutionSpec;
        let replacement_issuance = task_launch_issuance(task_id.clone(), 2);
        let replacement_spec = issued_execution_spec(&replacement_issuance, 2);
        assert!(matches!(
            engine.prepare(HarnessMutationV1::PutIssuedExecutionSpec {
                operation: mismatched_operation,
                expected_task_revision: revision(1),
                expected_spec_revision: HarnessExpectedExecutionSpecRevisionV1::Exact(revision(1)),
                expected_issuance_revision: HarnessExpectedExecutionSpecRevisionV1::Absent,
                issuance: replacement_issuance,
                spec: replacement_spec,
            }),
            Err(HarnessEngineError::IssuedExecutionCasMismatch { .. }),
        ));
        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn issued_execution_restore_rejects_dual_orphan_and_ref_mismatch() {
        let mut engine = HarnessEngine::new();
        let (task_id, _) = create_task(&mut engine);
        let issuance = task_launch_issuance(task_id.clone(), 1);
        let spec = issued_execution_spec(&issuance, 1);
        let mut checkpoint = engine.checkpoint();
        checkpoint.issuances.push(issuance.clone());
        checkpoint.execution_specs_v2.push(spec.clone());
        HarnessEngine::restore(checkpoint.clone()).unwrap();

        let mut dual = checkpoint.clone();
        dual.execution_specs.push(execution_spec(task_id.clone(), 1));
        assert!(matches!(
            HarnessEngine::restore(dual),
            Err(HarnessEngineError::DualExecutionSpec(_)),
        ));

        let mut orphan_spec = checkpoint.clone();
        orphan_spec.issuances.clear();
        assert!(matches!(
            HarnessEngine::restore(orphan_spec),
            Err(HarnessEngineError::OrphanIssuedExecutionSpec),
        ));

        let mut orphan_issuance = checkpoint.clone();
        orphan_issuance.execution_specs_v2.clear();
        assert!(matches!(
            HarnessEngine::restore(orphan_issuance),
            Err(HarnessEngineError::OrphanIssuance),
        ));

        let mut mismatched = checkpoint;
        mismatched.execution_specs_v2[0].launch_issuance.digest =
            HarnessRequestDigest::new("e".repeat(64)).unwrap();
        assert!(matches!(
            HarnessEngine::restore(mismatched),
            Err(HarnessEngineError::InvalidIssuanceLink),
        ));

        let mut divergent_revision = engine.checkpoint();
        divergent_revision.issuances.push(issuance);
        let mut divergent_spec = spec;
        divergent_spec.revision = revision(2);
        divergent_spec.updated_at_unix_ms = 22;
        divergent_revision.execution_specs_v2.push(divergent_spec);
        assert!(matches!(
            HarnessEngine::restore(divergent_revision),
            Err(HarnessEngineError::InvalidIssuanceLink),
        ));
    }

    #[test]
    fn create_issued_run_is_atomic_exact_replayable_and_restart_safe() {
        let mut engine = HarnessEngine::new();
        let (task_id, _) = create_task(&mut engine);
        engine.tasks.get_mut(&task_id).unwrap().state = HarnessTaskStateV1::Ready;

        let mut issuance = task_launch_issuance(task_id.clone(), 1);
        issuance.delivery = Some(HarnessDeliveryBundleSelectionV1 {
            bundle: delivery_bundle(),
            component_counts: vec![HarnessDeliveryComponentCountV1 {
                kind: HarnessDeliveryComponentKindV1::Skill,
                workspace_count: 1,
                session_count: 0,
            }],
        });
        let spec = issued_execution_spec(&issuance, 1);
        engine.issuances.insert(task_id.clone(), issuance.clone());
        engine.execution_specs_v2.insert(task_id.clone(), spec);

        let mut operation = original_run_operation(HarnessOperationStateV1::Prepared);
        operation.expected_revision = Some(revision(1));
        let mut requested_run = run(HarnessRunLifecycleV1::Requested, 1);
        requested_run.intent.worktree = HarnessWorktreeIntentV1::ManagedProfile {
            profile_id: HarnessSelectorV1::new("review-tree").unwrap(),
            expected_profile_revision: HarnessSelectorV1::new("revision-7").unwrap(),
        };
        requested_run.intent.delivery_bundle = Some(
            issuance.delivery.as_ref().unwrap().bundle.selector.clone(),
        );
        let mut running_task = engine.task(&task_id).unwrap().clone();
        running_task.revision = revision(2);
        running_task.state = HarnessTaskStateV1::Running;
        running_task.run_ids = vec![requested_run.run_id.clone()];
        running_task.updated_at_unix_ms = 20;
        let mut prepared_delivery = delivery(HarnessDeliveryStateV1::Prepared, 1);
        prepared_delivery.authority = HarnessTransferAuthorityRefV1::OperatorIssuance {
            issuance: issuance.reference(),
        };
        let mutation = HarnessMutationV1::CreateIssuedRun {
            operation: operation.clone(),
            expected_task_revision: revision(1),
            expected_execution_spec_revision: revision(1),
            expected_issuance_revision: revision(1),
            task: running_task.clone(),
            run: requested_run.clone(),
            delivery: Some(prepared_delivery.clone()),
            continuation: None,
        };

        let before = engine.checkpoint();
        let prepared = engine.prepare(mutation.clone()).unwrap();
        assert_eq!(engine.checkpoint(), before);
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        engine.accept(prepared);
        assert_eq!(engine.task(&task_id), Some(&running_task));
        assert_eq!(engine.run(&requested_run.run_id), Some(&requested_run));
        assert_eq!(engine.delivery(&prepared_delivery.delivery_ref), Some(&prepared_delivery));
        assert_eq!(
            engine.prepare(mutation.clone()).unwrap().outcome(),
            HarnessApplyOutcome::Replayed,
        );
        let restored = HarnessEngine::restore(engine.checkpoint()).unwrap();
        assert_eq!(restored.task(&task_id), Some(&running_task));
        assert_eq!(restored.run(&requested_run.run_id), Some(&requested_run));
        assert_eq!(
            restored.delivery(&prepared_delivery.delivery_ref),
            Some(&prepared_delivery),
        );

        let mut stale = mutation.clone();
        if let HarnessMutationV1::CreateIssuedRun {
            operation,
            expected_issuance_revision,
            run,
            delivery: Some(delivery),
            ..
        } = &mut stale
        {
            operation.operation_id = operation_id('c');
            run.operation_id = operation.operation_id.clone();
            delivery.operation_id = operation.operation_id.clone();
            operation.idempotency_ref = HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "c".repeat(24),
            )).unwrap();
            operation.request_digest = HarnessRequestDigest::new("c".repeat(64)).unwrap();
            *expected_issuance_revision = revision(2);
        }
        let stale_error = HarnessEngine::restore(before.clone()).unwrap()
            .prepare(stale).unwrap_err();
        assert!(
            matches!(stale_error, HarnessEngineError::IssuedExecutionCasMismatch { .. }),
            "unexpected stale error: {stale_error:?}",
        );

        let mut wrong_authority = mutation;
        if let HarnessMutationV1::CreateIssuedRun {
            operation,
            run,
            delivery: Some(delivery),
            ..
        } = &mut wrong_authority
        {
            operation.operation_id = operation_id('d');
            run.operation_id = operation.operation_id.clone();
            delivery.operation_id = operation.operation_id.clone();
            operation.idempotency_ref = HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                "d".repeat(24),
            )).unwrap();
            operation.request_digest = HarnessRequestDigest::new("d".repeat(64)).unwrap();
            delivery.authority = HarnessTransferAuthorityRefV1::ParentGrant {
                grant_id: grant_id(),
                revision: revision(1),
            };
        }
        assert!(matches!(
            HarnessEngine::restore(before).unwrap().prepare(wrong_authority),
            Err(HarnessEngineError::IssuedExecutionTransferMismatch),
        ));
    }

    #[test]
    fn create_issued_run_accepts_concrete_continuation_provider_and_rejects_wrong_route() {
        let mut engine = HarnessEngine::new();
        let (task_id, _) = create_task(&mut engine);
        engine.tasks.get_mut(&task_id).unwrap().state = HarnessTaskStateV1::Ready;

        let source_task_id = numbered_task_id(2);
        let source_run_id = numbered_run_id(2);
        let source_operation_id = operation_id('e');
        let mut source_task = task(source_task_id.clone(), 1, "source");
        source_task.state = HarnessTaskStateV1::Running;
        source_task.run_ids = vec![source_run_id.clone()];
        let mut source_operation = original_run_operation(HarnessOperationStateV1::Succeeded);
        source_operation.operation_id = source_operation_id.clone();
        source_operation.task_id = Some(source_task_id.clone());
        source_operation.run_id = Some(source_run_id.clone());
        source_operation.finished_at_unix_ms = Some(12);
        let mut source_run = run(HarnessRunLifecycleV1::Running, 1);
        source_run.task_id = source_task_id.clone();
        source_run.run_id = source_run_id.clone();
        source_run.operation_id = source_operation_id.clone();
        source_run.intent.provider_profile = HarnessSelectorV1::new("source-claude").unwrap();
        let source_binding = source_run.binding.clone().unwrap();
        engine.tasks.insert(source_task_id, source_task);
        engine.operations.insert(source_operation_id, source_operation);
        engine.runs.insert(source_run_id.clone(), source_run.clone());

        let (record_id, active_session) = match &source_binding.session {
            HarnessSessionIdentityV1::Managed {
                record_id,
                active_session: Some(active_session),
            } => (record_id.clone(), active_session.clone()),
            _ => unreachable!(),
        };
        let mut issuance = task_launch_issuance(task_id.clone(), 1);
        issuance.context_source = Some(HarnessContextSourceSelectionV1 {
            source_run_id: source_run_id.clone(),
            source_run_revision: source_run.revision,
            observed_at_unix_ms: 20,
            metadata_digest: HarnessRequestDigest::new("e".repeat(64)).unwrap(),
            node_id: source_binding.node_id.clone(),
            node_incarnation: source_binding.node_incarnation.clone(),
            workspace_id: source_binding.workspace_id.clone(),
            session_record_id: record_id,
            active_session: Some(active_session),
            message_count: 3,
            message_count_exact: true,
            completed_turn_count: Some(1),
            total_tokens: Some(100),
            availability: HarnessContextSourceAvailabilityV1::Live,
            context_pack: None,
        });
        let spec = issued_execution_spec(&issuance, 1);
        engine.issuances.insert(task_id.clone(), issuance.clone());
        engine.execution_specs_v2.insert(task_id.clone(), spec);

        let operation = original_run_operation(HarnessOperationStateV1::Prepared);
        let mut requested_run = run(HarnessRunLifecycleV1::Requested, 1);
        requested_run.intent.worktree = HarnessWorktreeIntentV1::ManagedProfile {
            profile_id: HarnessSelectorV1::new("review-tree").unwrap(),
            expected_profile_revision: HarnessSelectorV1::new("revision-7").unwrap(),
        };
        requested_run.intent.continuation = Some(
            HarnessSelectorV1::new(source_run_id.to_string()).unwrap(),
        );
        let mut running_task = engine.task(&task_id).unwrap().clone();
        running_task.revision = revision(2);
        running_task.state = HarnessTaskStateV1::Running;
        running_task.run_ids = vec![requested_run.run_id.clone()];
        running_task.updated_at_unix_ms = 20;
        let continuation = HarnessContinuationV1 {
            continuation_ref: HarnessContinuationRef::new(format!(
                "hcontinuation_{}",
                "f".repeat(24),
            )).unwrap(),
            receipt_ref: HarnessReceiptRef::new(format!(
                "hreceipt_{}",
                "f".repeat(24),
            )).unwrap(),
            revision: revision(1),
            state: HarnessContinuationStateV1::Prepared,
            authority: HarnessTransferAuthorityRefV1::OperatorIssuance {
                issuance: issuance.reference(),
            },
            source_run_id: source_run_id.clone(),
            target_run_id: requested_run.run_id.clone(),
            operation_id: operation.operation_id.clone(),
            node_id: source_binding.node_id.clone(),
            node_incarnation: source_binding.node_incarnation.clone(),
            workspace_id: source_binding.workspace_id.clone(),
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
        let mutation = HarnessMutationV1::CreateIssuedRun {
            operation,
            expected_task_revision: revision(1),
            expected_execution_spec_revision: revision(1),
            expected_issuance_revision: revision(1),
            task: running_task,
            run: requested_run.clone(),
            delivery: None,
            continuation: Some(continuation.clone()),
        };
        let mut wrong_route = mutation.clone();
        if let HarnessMutationV1::CreateIssuedRun {
            continuation: Some(continuation),
            ..
        } = &mut wrong_route
        {
            continuation.node_id = HarnessSelectorV1::new("node-b").unwrap();
            continuation.source_binding.node_id = continuation.node_id.clone();
        }
        assert!(matches!(
            engine.prepare(wrong_route),
            Err(HarnessEngineError::InvalidContinuationLink),
        ));

        let before = engine.checkpoint();
        let prepared = engine.prepare(mutation).unwrap();
        assert_eq!(engine.checkpoint(), before);
        engine.accept(prepared);
        assert_eq!(
            engine.continuation(&continuation.continuation_ref),
            Some(&continuation),
        );
        assert_eq!(
            HarnessEngine::restore(engine.checkpoint()).unwrap()
                .continuation(&continuation.continuation_ref),
            Some(&continuation),
        );
    }

    #[test]
    fn harness_scheduler_exact_id_selects_b_while_a_sorts_first() {
        let mut engine = HarnessEngine::new();
        let mut task_a = task(numbered_task_id(1), 1, "a");
        task_a.state = HarnessTaskStateV1::Ready;
        let mut task_b = task(numbered_task_id(2), 1, "b");
        task_b.state = HarnessTaskStateV1::Ready;
        engine.tasks.insert(task_a.task_id.clone(), task_a.clone());
        engine.tasks.insert(task_b.task_id.clone(), task_b.clone());
        assert_eq!(
            engine.scheduler_ready_task().unwrap().map(|task| &task.task_id),
            Some(&task_a.task_id),
        );
        assert_eq!(
            engine.scheduler_ready_task_by_id(&task_b.task_id)
                .unwrap()
                .map(|task| &task.task_id),
            Some(&task_b.task_id),
        );
    }

    #[test]
    fn harness_scheduler_selects_lowest_ready_task_with_done_dependencies() {
        let mut engine = HarnessEngine::new();
        let mut dependency = task(numbered_task_id(1), 1, "dependency");
        dependency.state = HarnessTaskStateV1::Done;
        let mut blocked = task(numbered_task_id(2), 1, "blocked");
        blocked.state = HarnessTaskStateV1::Ready;
        blocked.dependencies = vec![numbered_task_id(4)];
        let mut eligible_low = task(numbered_task_id(3), 1, "eligible-low");
        eligible_low.state = HarnessTaskStateV1::Ready;
        eligible_low.dependencies = vec![dependency.task_id.clone()];
        let mut unfinished = task(numbered_task_id(4), 1, "unfinished");
        unfinished.state = HarnessTaskStateV1::Backlog;
        let mut eligible_high = task(numbered_task_id(5), 1, "eligible-high");
        eligible_high.state = HarnessTaskStateV1::Ready;
        for item in [dependency, blocked, eligible_low.clone(), unfinished, eligible_high] {
            engine.tasks.insert(item.task_id.clone(), item);
        }

        assert_eq!(
            engine.scheduler_ready_task().unwrap().map(|task| task.task_id.clone()),
            Some(eligible_low.task_id),
        );
    }

    #[test]
    fn harness_scheduler_requires_every_dependency_done() {
        let mut engine = HarnessEngine::new();
        let dependency_id = numbered_task_id(1);
        let mut dependency = task(dependency_id.clone(), 1, "dependency");
        dependency.state = HarnessTaskStateV1::Review;
        let mut candidate = task(numbered_task_id(2), 1, "candidate");
        candidate.state = HarnessTaskStateV1::Ready;
        candidate.dependencies = vec![dependency_id.clone()];
        engine.tasks.insert(dependency_id.clone(), dependency);
        engine.tasks.insert(candidate.task_id.clone(), candidate.clone());
        assert!(engine.scheduler_ready_task().unwrap().is_none());

        engine.tasks.get_mut(&dependency_id).unwrap().state = HarnessTaskStateV1::Done;
        assert_eq!(
            engine.scheduler_ready_task().unwrap().map(|task| task.task_id.clone()),
            Some(candidate.task_id),
        );
    }

    // `harness_scheduler_scan_bound_is_categorical` used to assert that a
    // store holding more than `HARNESS_SCHEDULER_SCAN_MAX` tasks refuses
    // BOTH `scheduler_ready_task` and `scheduler_pending_dispatch` -- but
    // `scheduler_pending_dispatch` walks `self.runs`, not `self.tasks`, so
    // that second assertion was pinning the exact cliff this change removes:
    // ordinary task-history growth blocking a call whose real cost is the
    // number of runs. It is inverted below as
    // `harness_scheduler_pending_dispatch_not_bound_by_task_count`, and the
    // surviving half (a genuinely oversized `scheduler_ready_task` walk still
    // refuses) is kept, renamed to name what it actually tests.
    #[test]
    fn harness_scheduler_ready_task_scan_bound_is_categorical() {
        let mut engine = HarnessEngine::new();
        for index in 0..=HARNESS_SCHEDULER_SCAN_MAX {
            let item = task(numbered_task_id(index + 1), 1, "bounded");
            engine.tasks.insert(item.task_id.clone(), item);
        }
        // Every inserted task stays `Backlog`, so the walk never finds an
        // eligible `Ready` task and must examine the whole table -- a
        // genuinely large scan, not merely a large store.
        assert!(matches!(
            engine.scheduler_ready_task(),
            Err(HarnessEngineError::SchedulerResourceExhausted),
        ));
    }

    #[test]
    fn harness_scheduler_pending_dispatch_not_bound_by_task_count() {
        let mut engine = HarnessEngine::new();
        for index in 0..=HARNESS_SCHEDULER_SCAN_MAX {
            let item = task(numbered_task_id(index + 1), 1, "bounded");
            engine.tasks.insert(item.task_id.clone(), item);
        }
        // `scheduler_pending_dispatch` walks `self.runs`, which is empty
        // here. A task-only store this large must not refuse a call whose
        // cost has nothing to do with how many tasks are stored.
        assert_eq!(engine.scheduler_pending_dispatch().unwrap(), None);
    }

    #[test]
    fn harness_scheduler_pending_dispatch_scan_bound_is_categorical() {
        let mut engine = HarnessEngine::new();
        for index in 0..=HARNESS_SCHEDULER_SCAN_MAX {
            let mut item = run(HarnessRunLifecycleV1::Failed, 1);
            item.run_id = numbered_run_id(index + 1);
            engine.runs.insert(item.run_id.clone(), item);
        }
        // Every inserted run is terminal and skipped on sight, but the walk
        // still has to visit each one to know that -- a genuinely oversized
        // run scan still refuses.
        assert!(matches!(
            engine.scheduler_pending_dispatch(),
            Err(HarnessEngineError::SchedulerResourceExhausted),
        ));
    }

    #[test]
    fn harness_scheduler_ready_task_by_id_survives_oversized_store() {
        let mut engine = HarnessEngine::new();
        let filler_tasks = HARNESS_SCHEDULER_SCAN_MAX / 2 + 100;
        let filler_runs = HARNESS_SCHEDULER_SCAN_MAX / 2 + 100;
        assert!(filler_tasks + filler_runs > HARNESS_SCHEDULER_SCAN_MAX);
        for index in 0..filler_tasks {
            let item = task(numbered_task_id(index + 1_000), 1, "filler");
            engine.tasks.insert(item.task_id.clone(), item);
        }
        for index in 0..filler_runs {
            let mut item = run(HarnessRunLifecycleV1::Failed, 1);
            item.run_id = numbered_run_id(index + 1_000);
            engine.runs.insert(item.run_id.clone(), item);
        }

        let eligible_task_id = numbered_task_id(1);
        let eligible_run_id = numbered_run_id(1);
        let mut eligible_run = run(HarnessRunLifecycleV1::Completed, 1);
        eligible_run.run_id = eligible_run_id.clone();
        eligible_run.task_id = eligible_task_id.clone();
        engine.runs.insert(eligible_run_id.clone(), eligible_run);

        let mut eligible_task = task(eligible_task_id.clone(), 1, "eligible");
        eligible_task.state = HarnessTaskStateV1::Ready;
        eligible_task.run_ids = vec![eligible_run_id];
        engine.tasks.insert(eligible_task_id.clone(), eligible_task);

        // This is the live cliff: lifetime tasks+runs is well past the old
        // ceiling, yet the one eligible task named by id is still admitted.
        assert_eq!(
            engine.scheduler_ready_task_by_id(&eligible_task_id)
                .unwrap()
                .map(|task| task.task_id.clone()),
            Some(eligible_task_id),
        );
    }

    #[test]
    fn harness_scheduler_ready_task_by_id_still_refuses_ineligible_in_oversized_store() {
        let mut engine = HarnessEngine::new();
        let filler_tasks = HARNESS_SCHEDULER_SCAN_MAX / 2 + 100;
        let filler_runs = HARNESS_SCHEDULER_SCAN_MAX / 2 + 100;
        assert!(filler_tasks + filler_runs > HARNESS_SCHEDULER_SCAN_MAX);
        for index in 0..filler_tasks {
            let item = task(numbered_task_id(index + 1_000), 1, "filler");
            engine.tasks.insert(item.task_id.clone(), item);
        }
        for index in 0..filler_runs {
            let mut item = run(HarnessRunLifecycleV1::Failed, 1);
            item.run_id = numbered_run_id(index + 1_000);
            engine.runs.insert(item.run_id.clone(), item);
        }

        // Ineligible: not `Ready` at all.
        let backlog_task_id = numbered_task_id(1);
        let backlog_task = task(backlog_task_id.clone(), 1, "still-backlog");
        engine.tasks.insert(backlog_task_id.clone(), backlog_task);
        assert_eq!(engine.scheduler_ready_task_by_id(&backlog_task_id).unwrap(), None);

        // Ineligible: `Ready` with an unmet dependency.
        let unmet_task_id = numbered_task_id(2);
        let mut unmet_task = task(unmet_task_id.clone(), 1, "unmet-dependency");
        unmet_task.state = HarnessTaskStateV1::Ready;
        unmet_task.dependencies = vec![numbered_task_id(3)];
        engine.tasks.insert(unmet_task_id.clone(), unmet_task);
        assert_eq!(engine.scheduler_ready_task_by_id(&unmet_task_id).unwrap(), None);

        // Ineligible: `Ready` but still holding a nonterminal run.
        let running_task_id = numbered_task_id(4);
        let running_run_id = numbered_run_id(4);
        let mut running_run = run(HarnessRunLifecycleV1::Running, 1);
        running_run.run_id = running_run_id.clone();
        running_run.task_id = running_task_id.clone();
        engine.runs.insert(running_run_id.clone(), running_run);
        let mut running_task = task(running_task_id.clone(), 1, "nonterminal-run");
        running_task.state = HarnessTaskStateV1::Ready;
        running_task.run_ids = vec![running_run_id];
        engine.tasks.insert(running_task_id.clone(), running_task);
        assert_eq!(engine.scheduler_ready_task_by_id(&running_task_id).unwrap(), None);
    }

    #[test]
    fn harness_task_has_nonterminal_run_not_bound_by_store_size() {
        let mut engine = HarnessEngine::new();
        let filler_tasks = HARNESS_SCHEDULER_SCAN_MAX / 2 + 100;
        let filler_runs = HARNESS_SCHEDULER_SCAN_MAX / 2 + 100;
        assert!(filler_tasks + filler_runs > HARNESS_SCHEDULER_SCAN_MAX);
        for index in 0..filler_tasks {
            let item = task(numbered_task_id(index + 1_000), 1, "filler");
            engine.tasks.insert(item.task_id.clone(), item);
        }
        for index in 0..filler_runs {
            let mut item = run(HarnessRunLifecycleV1::Failed, 1);
            item.run_id = numbered_run_id(index + 1_000);
            engine.runs.insert(item.run_id.clone(), item);
        }

        let owner_task_id = numbered_task_id(1);
        let owned_run_id = numbered_run_id(1);
        let mut owned_run = run(HarnessRunLifecycleV1::Running, 1);
        owned_run.run_id = owned_run_id.clone();
        owned_run.task_id = owner_task_id.clone();
        engine.runs.insert(owned_run_id.clone(), owned_run);

        let mut owner = task(owner_task_id.clone(), 1, "owner");
        owner.run_ids = vec![owned_run_id.clone()];

        // Cost here is `owner.run_ids.len()`, not the store's total size.
        assert!(engine.task_has_nonterminal_run(&owner).unwrap());

        engine.runs.get_mut(&owned_run_id).unwrap().lifecycle = HarnessRunLifecycleV1::Completed;
        assert!(!engine.task_has_nonterminal_run(&owner).unwrap());
    }

    #[test]
    fn harness_request_idempotency_replays_or_conflicts() {
        let mut engine = HarnessEngine::new();
        let id = task_id('a');
        let operation = task_operation(operation_id('a'), id.clone(), 'a', None);
        let prepared = engine.prepare(HarnessMutationV1::CreateTask {
            operation: operation.clone(),
            task: task(id.clone(), 1, "first"),
        }).unwrap();
        engine.accept(prepared);

        let replay = engine.prepare(HarnessMutationV1::CreateTask {
            operation: operation.clone(),
            task: task(id.clone(), 1, "first"),
        }).unwrap();
        assert_eq!(replay.outcome(), HarnessApplyOutcome::Replayed);

        let mut wrong_digest = operation.clone();
        wrong_digest.request_digest = HarnessRequestDigest::new("b".repeat(64)).unwrap();
        assert!(matches!(
            engine.prepare(HarnessMutationV1::CreateTask {
                operation: wrong_digest,
                task: task(id.clone(), 1, "conflict"),
            }),
            Err(HarnessEngineError::OperationIdConflict { .. })
        ));

        let mut wrong_actor = operation;
        wrong_actor.actor = HarnessActorV1::User {
            actor_id: HarnessSelectorV1::new("different-operator").unwrap(),
        };
        assert!(matches!(
            engine.prepare(HarnessMutationV1::CreateTask {
                operation: wrong_actor,
                task: task(id, 1, "conflict"),
            }),
            Err(HarnessEngineError::OperationIdConflict { .. })
        ));
    }

    #[test]
    fn observation_never_creates_or_mutates_harness_task() {
        let mut engine = HarnessEngine::new();
        create_task(&mut engine);
        let before = engine.checkpoint();
        assert!(
            !include_str!("../Cargo.toml").contains("gate4agent-observation"),
            "the mutation engine must not acquire a monitoring dependency",
        );
        let observation_only_projection = serde_json::json!({
            "kind": "turn-activity",
            "sequence": 42,
        });
        assert!(observation_only_projection.is_object());
        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn restore_rejects_multi_node_task_cycle() {
        let first = task_id('a');
        let second = task_id('b');
        let mut first_task = task(first.clone(), 1, "first");
        first_task.dependencies = vec![second.clone()];
        let mut second_task = task(second, 1, "second");
        second_task.dependencies = vec![first];
        let checkpoint = HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![first_task, second_task],
            runs: Vec::new(),
            grants: Vec::new(),
            operations: Vec::new(),
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        };
        assert!(matches!(HarnessEngine::restore(checkpoint), Err(HarnessEngineError::Cycle(_))));
    }

    #[test]
    fn harness_run_lifecycle_is_exact_terminal_and_unknown_frozen() {
        let unknown = run(HarnessRunLifecycleV1::OutcomeUnknown, 1);
        let running = run(HarnessRunLifecycleV1::Running, 2);
        assert!(matches!(
            validate_run_lifecycle_transition(&unknown, &running),
            Err(HarnessEngineError::OutcomeUnknownRunFrozen)
        ));

        let completed = run(HarnessRunLifecycleV1::Completed, 3);
        let mut rewritten = completed.clone();
        rewritten.revision = revision(4);
        rewritten.updated_at_unix_ms = 20;
        assert!(matches!(
            validate_run_lifecycle_transition(&completed, &rewritten),
            Err(HarnessEngineError::TerminalRunMutation)
        ));

        let requested = run(HarnessRunLifecycleV1::Requested, 1);
        let mut changed_intent = run(HarnessRunLifecycleV1::Requested, 2);
        changed_intent.intent.workspace_id = HarnessSelectorV1::new("workspace-b").unwrap();
        assert!(matches!(
            validate_run_immutable(&requested, &changed_intent),
            Err(HarnessEngineError::MismatchedIdentity("immutable run request"))
        ));
    }

    #[test]
    fn run_event_commit_is_atomic_exact_replay_and_fail_closed() {
        for (current_lifecycle, current_task_state, next_lifecycle, task_state, marker) in [
            (
                HarnessRunLifecycleV1::Running,
                HarnessTaskStateV1::Running,
                HarnessRunLifecycleV1::Waiting,
                HarnessTaskStateV1::Waiting,
                'c',
            ),
            (
                HarnessRunLifecycleV1::Waiting,
                HarnessTaskStateV1::Waiting,
                HarnessRunLifecycleV1::Running,
                HarnessTaskStateV1::Running,
                'd',
            ),
            (
                HarnessRunLifecycleV1::Running,
                HarnessTaskStateV1::Running,
                HarnessRunLifecycleV1::Completed,
                HarnessTaskStateV1::Review,
                'e',
            ),
            (
                HarnessRunLifecycleV1::Running,
                HarnessTaskStateV1::Running,
                HarnessRunLifecycleV1::Failed,
                HarnessTaskStateV1::Failed,
                'f',
            ),
            (
                HarnessRunLifecycleV1::Running,
                HarnessTaskStateV1::Running,
                HarnessRunLifecycleV1::Cancelled,
                HarnessTaskStateV1::Cancelled,
                '9',
            ),
        ] {
            let mut current_task = task(task_id('a'), 2, "event task");
            current_task.state = current_task_state;
            current_task.run_ids = vec![run_id()];
            let current_run = run(current_lifecycle, 3);
            let mut create_run = original_run_operation(HarnessOperationStateV1::Succeeded);
            create_run.revision = revision(3);
            create_run.updated_at_unix_ms = current_run.updated_at_unix_ms;
            create_run.finished_at_unix_ms = Some(current_run.updated_at_unix_ms);
            let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
                version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
                tasks: vec![current_task.clone()],
                runs: vec![current_run.clone()],
                grants: Vec::new(),
                operations: vec![create_run],
                execution_specs: Vec::new(),
                issuances: Vec::new(),
                execution_specs_v2: Vec::new(),
                deliveries: Vec::new(),
                continuations: Vec::new(),
            }).unwrap();
            let before = engine.checkpoint();
            let mut next_run = run(next_lifecycle, 4);
            next_run.updated_at_unix_ms = 20;
            if next_lifecycle == HarnessRunLifecycleV1::Failed {
                next_run.failure = Some(HarnessFailureV1 {
                    category: HarnessFailureCategoryV1::Internal,
                    retryable: false,
                });
            }
            let mut next_task = current_task.clone();
            next_task.revision = revision(3);
            next_task.state = task_state;
            next_task.updated_at_unix_ms = 20;
            let operation = HarnessOperationV1 {
                operation_id: operation_id(marker),
                revision: revision(1),
                actor: HarnessActorV1::ParentRun { run_id: run_id() },
                kind: HarnessOperationKindV1::MutateRun,
                state: HarnessOperationStateV1::Succeeded,
                task_id: None,
                run_id: Some(run_id()),
                grant_id: None,
                reconciles_operation_id: None,
                expected_revision: Some(revision(3)),
                request_digest: HarnessRequestDigest::new(
                    marker.to_string().repeat(64),
                ).unwrap(),
                idempotency_ref: HarnessIdempotencyRef::new(format!(
                    "hidem_{}",
                    marker.to_string().repeat(24),
                )).unwrap(),
                failure: None,
                outcome_unknown_reason: None,
                reconciliation_outcome: None,
                created_at_unix_ms: 20,
                updated_at_unix_ms: 20,
                dispatched_at_unix_ms: None,
                finished_at_unix_ms: Some(20),
            };
            let prepared = engine.prepare_run_event_commit(
                operation.clone(),
                revision(3),
                next_run.clone(),
                revision(2),
                next_task.clone(),
            ).unwrap();
            assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
            assert_eq!(engine.checkpoint(), before);
            let mut committed = engine.clone();
            committed.accept(prepared);
            let committed_snapshot = committed.checkpoint();
            assert_eq!(committed.prepare_run_event_commit(
                operation.clone(),
                revision(3),
                next_run.clone(),
                revision(2),
                next_task.clone(),
            ).unwrap().outcome(), HarnessApplyOutcome::Replayed);
            assert_eq!(committed.checkpoint(), committed_snapshot);

            let mut changed_task = next_task.clone();
            changed_task.title.push_str(" changed");
            assert!(matches!(
                committed.prepare_run_event_commit(
                    operation.clone(),
                    revision(3),
                    next_run.clone(),
                    revision(2),
                    changed_task,
                ),
                Err(HarnessEngineError::OperationIdConflict { .. })
            ));
            assert_eq!(committed.checkpoint(), committed_snapshot);

            assert!(engine.prepare_run_event_commit(
                operation.clone(),
                revision(2),
                next_run.clone(),
                revision(2),
                next_task.clone(),
            ).is_err());
            let mut cross_operation = operation;
            cross_operation.task_id = Some(task_id('f'));
            assert!(engine.prepare_run_event_commit(
                cross_operation,
                revision(3),
                next_run,
                revision(2),
                next_task,
            ).is_err());
            assert_eq!(engine.checkpoint(), before);
        }
    }

    fn context_pack_fixture() -> HarnessEngine {
        let mut current_task = task(task_id('a'), 2, "context pack task");
        current_task.state = HarnessTaskStateV1::Review;
        current_task.run_ids = vec![run_id()];
        let current_run = run(HarnessRunLifecycleV1::Completed, 3);
        let mut create_run = original_run_operation(HarnessOperationStateV1::Succeeded);
        create_run.updated_at_unix_ms = current_run.updated_at_unix_ms;
        create_run.finished_at_unix_ms = Some(current_run.updated_at_unix_ms);
        HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![current_task],
            runs: vec![current_run],
            grants: Vec::new(),
            operations: vec![create_run],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap()
    }

    fn context_pack_receipt(digest_hex: char) -> HarnessResolvedContextPackReceiptV1 {
        HarnessResolvedContextPackReceiptV1 {
            id: HarnessSelectorV1::new("context-a").unwrap(),
            digest: format!("sha256:{}", digest_hex.to_string().repeat(64)),
            lineage: HarnessContextPackLineageV1 {
                source_node_id: HarnessSelectorV1::new("node-a").unwrap(),
                source_workspace_id: HarnessSelectorV1::new("workspace-a").unwrap(),
                source_instance_id: 1,
                source_generation: 1,
                source_provider: HarnessSelectorV1::new("claude").unwrap(),
            },
            source_message_count: 4,
            retained_message_count: 4,
            byte_len: 64,
            truncated: false,
        }
    }

    fn context_pack_record_operation(
        marker: char,
        expected_revision: HarnessRevision,
        at_unix_ms: u64,
    ) -> HarnessOperationV1 {
        HarnessOperationV1 {
            operation_id: operation_id(marker),
            revision: revision(1),
            actor: HarnessActorV1::ParentRun { run_id: run_id() },
            kind: HarnessOperationKindV1::RecordRunContextPack,
            state: HarnessOperationStateV1::Succeeded,
            task_id: None,
            run_id: Some(run_id()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(expected_revision),
            request_digest: HarnessRequestDigest::new(marker.to_string().repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                marker.to_string().repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: at_unix_ms,
            updated_at_unix_ms: at_unix_ms,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(at_unix_ms),
        }
    }

    #[test]
    fn context_pack_record_freezes_every_field_except_pack_revision_and_updated_at() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let operation = context_pack_record_operation('1', current_run.revision, 30);

        let mut tampered = current_run.clone();
        tampered.context_pack = Some(context_pack_receipt('a'));
        tampered.revision = revision(current_run.revision.get() + 1);
        tampered.updated_at_unix_ms = 30;
        tampered.parent_run_id = Some(numbered_run_id(999));
        let before = engine.checkpoint();
        assert!(matches!(
            engine.prepare_run_context_pack_record(operation.clone(), current_run.revision, tampered),
            Err(HarnessEngineError::InvalidContextPackRecordProjection),
        ));
        assert_eq!(engine.checkpoint(), before);

        let mut clean = current_run.clone();
        clean.context_pack = Some(context_pack_receipt('a'));
        clean.revision = revision(current_run.revision.get() + 1);
        clean.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_context_pack_record(
            operation,
            current_run.revision,
            clean.clone(),
        ).unwrap();
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        let mut committed = engine.clone();
        committed.accept(prepared);
        let recorded = committed.run(&run_id()).unwrap().clone();
        let mut expected = current_run;
        expected.context_pack = clean.context_pack;
        expected.revision = clean.revision;
        expected.updated_at_unix_ms = clean.updated_at_unix_ms;
        assert_eq!(recorded, expected);
    }

    #[test]
    fn context_pack_record_rejects_a_second_different_receipt_as_conflict() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let first_operation = context_pack_record_operation('2', current_run.revision, 30);
        let mut first_next = current_run.clone();
        first_next.context_pack = Some(context_pack_receipt('a'));
        first_next.revision = revision(current_run.revision.get() + 1);
        first_next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_context_pack_record(
            first_operation,
            current_run.revision,
            first_next,
        ).unwrap();
        let mut committed = engine.clone();
        committed.accept(prepared);
        let before = committed.checkpoint();

        let recorded_run = committed.run(&run_id()).unwrap().clone();
        let second_operation = context_pack_record_operation('3', recorded_run.revision, 40);
        let mut second_next = recorded_run.clone();
        second_next.context_pack = Some(context_pack_receipt('b'));
        second_next.revision = revision(recorded_run.revision.get() + 1);
        second_next.updated_at_unix_ms = 40;
        assert!(matches!(
            committed.prepare_run_context_pack_record(
                second_operation,
                recorded_run.revision,
                second_next,
            ),
            Err(HarnessEngineError::TerminalRunMutation),
        ));
        assert_eq!(committed.checkpoint(), before);
    }

    #[test]
    fn context_pack_record_is_idempotent_on_the_same_digest() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let operation = context_pack_record_operation('4', current_run.revision, 30);
        let mut next = current_run.clone();
        next.context_pack = Some(context_pack_receipt('a'));
        next.revision = revision(current_run.revision.get() + 1);
        next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_context_pack_record(
            operation,
            current_run.revision,
            next,
        ).unwrap();
        let mut committed = engine.clone();
        committed.accept(prepared);
        let committed_snapshot = committed.checkpoint();

        // A second reconcile pass rediscovers the exact same receipt against
        // a stale (pre-commit) revision snapshot — this must no-op, not
        // conflict, per the design's race mitigation (§10 risk #1).
        let replay_operation = context_pack_record_operation('5', current_run.revision, 31);
        let mut replay_next = current_run.clone();
        replay_next.context_pack = Some(context_pack_receipt('a'));
        replay_next.revision = revision(current_run.revision.get() + 1);
        replay_next.updated_at_unix_ms = 31;
        let replayed = committed.prepare_run_context_pack_record(
            replay_operation,
            current_run.revision,
            replay_next,
        ).unwrap();
        assert_eq!(replayed.outcome(), HarnessApplyOutcome::Replayed);
        assert_eq!(replayed.checkpoint(), committed_snapshot);
    }

    #[test]
    fn context_pack_record_keeps_outcome_unknown_run_frozen() {
        let mut current_task = task(task_id('a'), 2, "frozen task");
        current_task.state = HarnessTaskStateV1::Waiting;
        current_task.run_ids = vec![run_id()];
        let mut current_run = run(HarnessRunLifecycleV1::Requested, 3);
        current_run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
        let mut create_run = original_run_operation(HarnessOperationStateV1::OutcomeUnknown);
        create_run.outcome_unknown_reason = Some(HarnessOutcomeUnknownReasonV1::Timeout);
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![current_task],
            runs: vec![current_run.clone()],
            grants: Vec::new(),
            operations: vec![create_run],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap();
        let operation = context_pack_record_operation('6', current_run.revision, 30);
        let before = engine.checkpoint();
        assert!(matches!(
            engine.prepare_run_context_pack_record(operation, current_run.revision, current_run),
            Err(HarnessEngineError::OutcomeUnknownRunFrozen),
        ));
        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn context_pack_record_never_touches_task_state() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let original_task = engine.task(&task_id('a')).unwrap().clone();
        let operation = context_pack_record_operation('7', current_run.revision, 30);
        let mut next = current_run.clone();
        next.context_pack = Some(context_pack_receipt('a'));
        next.revision = revision(current_run.revision.get() + 1);
        next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_context_pack_record(
            operation,
            current_run.revision,
            next,
        ).unwrap();
        let mut committed = engine.clone();
        committed.accept(prepared);
        assert_eq!(committed.task(&task_id('a')).unwrap(), &original_task);
    }

    #[test]
    fn context_pack_record_freezes_git_facts_too() {
        // A3 fix: `validate_context_pack_recording` must reject a `next`
        // that smuggles a git_facts change alongside a legitimate
        // context_pack write — symmetric with `validate_git_facts_recording`
        // freezing `context_pack` (see the "freezes every field" tests for
        // both twins).
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let operation = context_pack_record_operation('8', current_run.revision, 30);
        let mut tampered = current_run.clone();
        tampered.context_pack = Some(context_pack_receipt('a'));
        tampered.git_facts = Some(git_facts('a', 30));
        tampered.revision = revision(current_run.revision.get() + 1);
        tampered.updated_at_unix_ms = 30;
        let before = engine.checkpoint();
        assert!(matches!(
            engine.prepare_run_context_pack_record(operation, current_run.revision, tampered),
            Err(HarnessEngineError::InvalidContextPackRecordProjection),
        ));
        assert_eq!(engine.checkpoint(), before);
    }

    fn git_facts_summary(marker: char) -> HarnessRunGitSummaryV1 {
        HarnessRunGitSummaryV1 {
            is_repository: true,
            branch: Some(format!("branch-{marker}")),
            status: vec![HarnessRunGitStatusEntryV1 {
                index_status: HarnessRunGitStatusCodeV1::Unmodified,
                worktree_status: HarnessRunGitStatusCodeV1::Modified,
                path: "src/lib.rs".to_owned(),
                previous_path: None,
            }],
            recent_commits: vec![HarnessRunGitCommitSummaryV1 {
                id: marker.to_string().repeat(40),
                summary: "commit summary".to_owned(),
            }],
            truncated: false,
        }
    }

    fn git_facts(marker: char, at_unix_ms: u64) -> HarnessRunGitFactsV1 {
        HarnessRunGitFactsV1 {
            captured_at_unix_ms: at_unix_ms,
            outcome: HarnessRunGitFactsOutcomeV1::Captured(git_facts_summary(marker)),
        }
    }

    fn git_facts_record_operation(
        marker: char,
        expected_revision: HarnessRevision,
        at_unix_ms: u64,
    ) -> HarnessOperationV1 {
        HarnessOperationV1 {
            operation_id: operation_id(marker),
            revision: revision(1),
            actor: HarnessActorV1::ParentRun { run_id: run_id() },
            kind: HarnessOperationKindV1::RecordRunGitFacts,
            state: HarnessOperationStateV1::Succeeded,
            task_id: None,
            run_id: Some(run_id()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(expected_revision),
            request_digest: HarnessRequestDigest::new(marker.to_string().repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                marker.to_string().repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: at_unix_ms,
            updated_at_unix_ms: at_unix_ms,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(at_unix_ms),
        }
    }

    #[test]
    fn git_facts_record_freezes_every_field_except_facts_revision_and_updated_at() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let operation = git_facts_record_operation('1', current_run.revision, 30);

        let mut tampered = current_run.clone();
        tampered.git_facts = Some(git_facts('a', 30));
        tampered.revision = revision(current_run.revision.get() + 1);
        tampered.updated_at_unix_ms = 30;
        tampered.parent_run_id = Some(numbered_run_id(999));
        let before = engine.checkpoint();
        assert!(matches!(
            engine.prepare_run_git_facts_record(operation.clone(), current_run.revision, tampered),
            Err(HarnessEngineError::InvalidGitFactsRecordProjection),
        ));
        assert_eq!(engine.checkpoint(), before);

        let mut clean = current_run.clone();
        clean.git_facts = Some(git_facts('a', 30));
        clean.revision = revision(current_run.revision.get() + 1);
        clean.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_git_facts_record(
            operation,
            current_run.revision,
            clean.clone(),
        ).unwrap();
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        let mut committed = engine.clone();
        committed.accept(prepared);
        let recorded = committed.run(&run_id()).unwrap().clone();
        let mut expected = current_run;
        expected.git_facts = clean.git_facts;
        expected.revision = clean.revision;
        expected.updated_at_unix_ms = clean.updated_at_unix_ms;
        assert_eq!(recorded, expected);
    }

    #[test]
    fn git_facts_record_rejects_a_second_different_value_as_conflict() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let first_operation = git_facts_record_operation('2', current_run.revision, 30);
        let mut first_next = current_run.clone();
        first_next.git_facts = Some(git_facts('a', 30));
        first_next.revision = revision(current_run.revision.get() + 1);
        first_next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_git_facts_record(
            first_operation,
            current_run.revision,
            first_next,
        ).unwrap();
        let mut committed = engine.clone();
        committed.accept(prepared);
        let before = committed.checkpoint();

        let recorded_run = committed.run(&run_id()).unwrap().clone();
        let second_operation = git_facts_record_operation('3', recorded_run.revision, 40);
        let mut second_next = recorded_run.clone();
        second_next.git_facts = Some(git_facts('b', 40));
        second_next.revision = revision(recorded_run.revision.get() + 1);
        second_next.updated_at_unix_ms = 40;
        assert!(matches!(
            committed.prepare_run_git_facts_record(
                second_operation,
                recorded_run.revision,
                second_next,
            ),
            Err(HarnessEngineError::TerminalRunMutation),
        ));
        assert_eq!(committed.checkpoint(), before);
    }

    #[test]
    fn git_facts_record_is_idempotent_on_the_same_value() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let operation = git_facts_record_operation('4', current_run.revision, 30);
        let mut next = current_run.clone();
        next.git_facts = Some(git_facts('a', 30));
        next.revision = revision(current_run.revision.get() + 1);
        next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_git_facts_record(
            operation,
            current_run.revision,
            next,
        ).unwrap();
        let mut committed = engine.clone();
        committed.accept(prepared);
        let committed_snapshot = committed.checkpoint();

        // A second reconcile pass rediscovers the exact same value against a
        // stale (pre-commit) revision snapshot — this must no-op, not
        // conflict, per the design's single-attempt race mitigation (§9 risk
        // #2).
        let replay_operation = git_facts_record_operation('5', current_run.revision, 31);
        let mut replay_next = current_run.clone();
        replay_next.git_facts = Some(git_facts('a', 30));
        replay_next.revision = revision(current_run.revision.get() + 1);
        replay_next.updated_at_unix_ms = 31;
        let replayed = committed.prepare_run_git_facts_record(
            replay_operation,
            current_run.revision,
            replay_next,
        ).unwrap();
        assert_eq!(replayed.outcome(), HarnessApplyOutcome::Replayed);
        assert_eq!(replayed.checkpoint(), committed_snapshot);
    }

    #[test]
    fn git_facts_record_keeps_outcome_unknown_run_frozen() {
        let mut current_task = task(task_id('a'), 2, "frozen task");
        current_task.state = HarnessTaskStateV1::Waiting;
        current_task.run_ids = vec![run_id()];
        let mut current_run = run(HarnessRunLifecycleV1::Requested, 3);
        current_run.lifecycle = HarnessRunLifecycleV1::OutcomeUnknown;
        let mut create_run = original_run_operation(HarnessOperationStateV1::OutcomeUnknown);
        create_run.outcome_unknown_reason = Some(HarnessOutcomeUnknownReasonV1::Timeout);
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![current_task],
            runs: vec![current_run.clone()],
            grants: Vec::new(),
            operations: vec![create_run],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap();
        let operation = git_facts_record_operation('6', current_run.revision, 30);
        let before = engine.checkpoint();
        assert!(matches!(
            engine.prepare_run_git_facts_record(operation, current_run.revision, current_run),
            Err(HarnessEngineError::OutcomeUnknownRunFrozen),
        ));
        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn git_facts_record_never_touches_task_state() {
        let engine = context_pack_fixture();
        let current_run = engine.run(&run_id()).unwrap().clone();
        let original_task = engine.task(&task_id('a')).unwrap().clone();
        let operation = git_facts_record_operation('7', current_run.revision, 30);
        let mut next = current_run.clone();
        next.git_facts = Some(git_facts('a', 30));
        next.revision = revision(current_run.revision.get() + 1);
        next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_run_git_facts_record(
            operation,
            current_run.revision,
            next,
        ).unwrap();
        let mut committed = engine.clone();
        committed.accept(prepared);
        assert_eq!(committed.task(&task_id('a')).unwrap(), &original_task);
    }

    fn result_ref_fixture() -> HarnessEngine {
        let mut current_task = task(task_id('a'), 2, "result ref task");
        current_task.state = HarnessTaskStateV1::Review;
        current_task.run_ids = vec![numbered_run_id(1), numbered_run_id(2)];

        let mut run_one = run(HarnessRunLifecycleV1::Completed, 3);
        run_one.run_id = numbered_run_id(1);
        run_one.operation_id = operation_id('e');
        let mut create_run_one = original_run_operation(HarnessOperationStateV1::Succeeded);
        create_run_one.operation_id = operation_id('e');
        create_run_one.run_id = Some(numbered_run_id(1));
        create_run_one.request_digest = HarnessRequestDigest::new("e".repeat(64)).unwrap();
        create_run_one.idempotency_ref = HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            "e".repeat(24),
        )).unwrap();
        create_run_one.updated_at_unix_ms = run_one.updated_at_unix_ms;
        create_run_one.finished_at_unix_ms = Some(run_one.updated_at_unix_ms);

        let mut run_two = run(HarnessRunLifecycleV1::Completed, 3);
        run_two.run_id = numbered_run_id(2);
        run_two.operation_id = operation_id('f');
        let mut create_run_two = original_run_operation(HarnessOperationStateV1::Succeeded);
        create_run_two.operation_id = operation_id('f');
        create_run_two.run_id = Some(numbered_run_id(2));
        create_run_two.request_digest = HarnessRequestDigest::new("f".repeat(64)).unwrap();
        create_run_two.idempotency_ref = HarnessIdempotencyRef::new(format!(
            "hidem_{}",
            "f".repeat(24),
        )).unwrap();
        create_run_two.updated_at_unix_ms = run_two.updated_at_unix_ms;
        create_run_two.finished_at_unix_ms = Some(run_two.updated_at_unix_ms);

        HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![current_task],
            runs: vec![run_one, run_two],
            grants: Vec::new(),
            operations: vec![create_run_one, create_run_two],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap()
    }

    fn result_ref_record_operation(
        marker: char,
        for_run_id: HarnessRunId,
        expected_task_revision: HarnessRevision,
        at_unix_ms: u64,
    ) -> HarnessOperationV1 {
        HarnessOperationV1 {
            operation_id: operation_id(marker),
            revision: revision(1),
            actor: HarnessActorV1::ParentRun { run_id: for_run_id },
            kind: HarnessOperationKindV1::RecordTaskResultRef,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id('a')),
            run_id: None,
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(expected_task_revision),
            request_digest: HarnessRequestDigest::new(marker.to_string().repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(format!(
                "hidem_{}",
                marker.to_string().repeat(24),
            )).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: at_unix_ms,
            updated_at_unix_ms: at_unix_ms,
            dispatched_at_unix_ms: None,
            finished_at_unix_ms: Some(at_unix_ms),
        }
    }

    #[test]
    fn result_ref_record_freezes_every_field_except_refs_revision_and_updated_at() {
        let engine = result_ref_fixture();
        let current_task = engine.task(&task_id('a')).unwrap().clone();
        let candidate = HarnessResultRef::for_run(&numbered_run_id(1));
        let operation = result_ref_record_operation(
            '1',
            numbered_run_id(1),
            current_task.revision,
            30,
        );

        // task.state must never change here — not even alongside an
        // otherwise-legal insertion.
        let mut tampered = current_task.clone();
        tampered.result_refs = vec![candidate.clone()];
        tampered.revision = revision(current_task.revision.get() + 1);
        tampered.updated_at_unix_ms = 30;
        tampered.state = HarnessTaskStateV1::Done;
        let before = engine.checkpoint();
        assert!(matches!(
            engine.prepare_task_result_ref_record(operation.clone(), current_task.revision, tampered),
            Err(HarnessEngineError::InvalidResultRefRecordProjection),
        ));
        assert_eq!(engine.checkpoint(), before);

        let mut clean = current_task.clone();
        clean.result_refs = vec![candidate.clone()];
        clean.revision = revision(current_task.revision.get() + 1);
        clean.updated_at_unix_ms = 30;
        let prepared = engine.prepare_task_result_ref_record(
            operation,
            current_task.revision,
            clean.clone(),
        ).unwrap();
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        let mut committed = engine.clone();
        committed.accept(prepared);
        let recorded = committed.task(&task_id('a')).unwrap().clone();
        let mut expected = current_task;
        expected.result_refs = clean.result_refs;
        expected.revision = clean.revision;
        expected.updated_at_unix_ms = clean.updated_at_unix_ms;
        assert_eq!(recorded, expected);
    }

    #[test]
    fn result_ref_record_inserts_at_sorted_position() {
        let engine = result_ref_fixture();
        let current_task = engine.task(&task_id('a')).unwrap().clone();
        let ref_one = HarnessResultRef::for_run(&numbered_run_id(1));
        let ref_two = HarnessResultRef::for_run(&numbered_run_id(2));
        assert!(ref_one < ref_two, "fixture run ids must sort ref_one before ref_two");

        // Insert the higher-sorting ref first.
        let first_operation = result_ref_record_operation(
            '1',
            numbered_run_id(2),
            current_task.revision,
            30,
        );
        let mut first_next = current_task.clone();
        first_next.result_refs = vec![ref_two.clone()];
        first_next.revision = revision(current_task.revision.get() + 1);
        first_next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_task_result_ref_record(
            first_operation,
            current_task.revision,
            first_next,
        ).unwrap();
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        let mut committed = engine.clone();
        committed.accept(prepared);

        // Insert the lower-sorting ref second — it must land BEFORE ref_two,
        // not appended after it, proving insertion at the correct sorted
        // position rather than a plain append.
        let after_first = committed.task(&task_id('a')).unwrap().clone();
        let second_operation = result_ref_record_operation(
            '2',
            numbered_run_id(1),
            after_first.revision,
            40,
        );
        let mut second_next = after_first.clone();
        second_next.result_refs = vec![ref_one.clone(), ref_two.clone()];
        second_next.revision = revision(after_first.revision.get() + 1);
        second_next.updated_at_unix_ms = 40;
        let prepared = committed.prepare_task_result_ref_record(
            second_operation,
            after_first.revision,
            second_next,
        ).unwrap();
        assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
        committed.accept(prepared);
        assert_eq!(
            committed.task(&task_id('a')).unwrap().result_refs,
            vec![ref_one, ref_two],
        );
    }

    #[test]
    fn result_ref_record_is_idempotent_on_replay() {
        let engine = result_ref_fixture();
        let current_task = engine.task(&task_id('a')).unwrap().clone();
        let candidate = HarnessResultRef::for_run(&numbered_run_id(1));
        let operation = result_ref_record_operation(
            '1',
            numbered_run_id(1),
            current_task.revision,
            30,
        );
        let mut next = current_task.clone();
        next.result_refs = vec![candidate.clone()];
        next.revision = revision(current_task.revision.get() + 1);
        next.updated_at_unix_ms = 30;
        let prepared = engine.prepare_task_result_ref_record(
            operation,
            current_task.revision,
            next,
        ).unwrap();
        let mut committed = engine.clone();
        committed.accept(prepared);
        let committed_snapshot = committed.checkpoint();

        // A second reconcile pass rediscovers the exact same ref against a
        // stale (pre-commit) revision snapshot — this must no-op, not
        // conflict, mirroring the context-pack/git-facts twins' race
        // mitigation.
        let replay_operation = result_ref_record_operation(
            '2',
            numbered_run_id(1),
            current_task.revision,
            31,
        );
        let mut replay_next = current_task.clone();
        replay_next.result_refs = vec![candidate];
        replay_next.revision = revision(current_task.revision.get() + 1);
        replay_next.updated_at_unix_ms = 31;
        let replayed = committed.prepare_task_result_ref_record(
            replay_operation,
            current_task.revision,
            replay_next,
        ).unwrap();
        assert_eq!(replayed.outcome(), HarnessApplyOutcome::Replayed);
        assert_eq!(replayed.checkpoint(), committed_snapshot);
    }

    #[test]
    fn result_ref_record_is_a_no_op_at_capacity() {
        let mut current_task = task(task_id('a'), 2, "full task");
        current_task.state = HarnessTaskStateV1::Review;
        current_task.result_refs = (0..HARNESS_RESULTS_MAX)
            .map(numbered_run_id)
            .map(|run_id| HarnessResultRef::for_run(&run_id))
            .collect();
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![current_task.clone()],
            runs: Vec::new(),
            grants: Vec::new(),
            operations: Vec::new(),
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap();

        // Same-value replay at capacity is a legal no-op: the delta is
        // empty, so this must never reach — let alone be rejected by — the
        // authority checks that a real insertion would require.
        let replay_operation = result_ref_record_operation(
            '1',
            numbered_run_id(0),
            current_task.revision,
            30,
        );
        let mut replay_next = current_task.clone();
        replay_next.revision = revision(current_task.revision.get() + 1);
        replay_next.updated_at_unix_ms = 30;
        let replayed = engine.prepare_task_result_ref_record(
            replay_operation,
            current_task.revision,
            replay_next,
        ).unwrap();
        assert_eq!(replayed.outcome(), HarnessApplyOutcome::Replayed);
        assert_eq!(replayed.checkpoint(), engine.checkpoint());

        // An attempted substitution while at capacity (same length, but a
        // different set of refs) is rejected — capacity only ever permits
        // the unchanged no-op, never a swap.
        let substitute_operation = result_ref_record_operation(
            '2',
            numbered_run_id(HARNESS_RESULTS_MAX),
            current_task.revision,
            30,
        );
        let mut substitute_next = current_task.clone();
        substitute_next.result_refs.pop();
        substitute_next.result_refs.push(
            HarnessResultRef::for_run(&numbered_run_id(HARNESS_RESULTS_MAX)),
        );
        substitute_next.result_refs.sort();
        substitute_next.revision = revision(current_task.revision.get() + 1);
        substitute_next.updated_at_unix_ms = 30;
        let before = engine.checkpoint();
        assert!(matches!(
            engine.prepare_task_result_ref_record(
                substitute_operation,
                current_task.revision,
                substitute_next,
            ),
            Err(HarnessEngineError::InvalidResultRefRecordProjection),
        ));
        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn result_ref_record_rejects_removal_or_reorder() {
        let mut current_task = task(task_id('a'), 2, "seeded task");
        current_task.state = HarnessTaskStateV1::Review;
        let ref_one = HarnessResultRef::for_run(&numbered_run_id(1));
        let ref_two = HarnessResultRef::for_run(&numbered_run_id(2));
        let ref_three = HarnessResultRef::for_run(&numbered_run_id(3));
        current_task.result_refs = vec![ref_one.clone(), ref_two.clone()];
        let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![current_task.clone()],
            runs: Vec::new(),
            grants: Vec::new(),
            operations: Vec::new(),
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        }).unwrap();
        let before = engine.checkpoint();

        // Removal: drop ref_two entirely.
        let removal_operation = result_ref_record_operation(
            '1',
            numbered_run_id(1),
            current_task.revision,
            30,
        );
        let mut removed = current_task.clone();
        removed.result_refs = vec![ref_one.clone()];
        removed.revision = revision(current_task.revision.get() + 1);
        removed.updated_at_unix_ms = 30;
        assert!(matches!(
            engine.prepare_task_result_ref_record(removal_operation, current_task.revision, removed),
            Err(HarnessEngineError::InvalidResultRefRecordProjection),
        ));

        // Reorder: same two entries, swapped positions — rejected by the
        // task's own struct validation (result_refs must stay sorted
        // ascending) before this mutation's own projection check ever runs.
        let reorder_operation = result_ref_record_operation(
            '2',
            numbered_run_id(1),
            current_task.revision,
            30,
        );
        let mut reordered = current_task.clone();
        reordered.result_refs = vec![ref_two.clone(), ref_one.clone()];
        reordered.revision = revision(current_task.revision.get() + 1);
        reordered.updated_at_unix_ms = 30;
        assert!(matches!(
            engine.prepare_task_result_ref_record(reorder_operation, current_task.revision, reordered),
            Err(HarnessEngineError::Validation(HarnessValidationError::CollectionNotCanonical {
                field: "result_refs",
            })),
        ));

        // Unrelated substitution: same length, but ref_two replaced by an
        // unrelated third ref — not an insertion, not a replay, rejected.
        let substitution_operation = result_ref_record_operation(
            '3',
            numbered_run_id(1),
            current_task.revision,
            30,
        );
        let mut substituted = current_task.clone();
        substituted.result_refs = vec![ref_one, ref_three];
        substituted.revision = revision(current_task.revision.get() + 1);
        substituted.updated_at_unix_ms = 30;
        assert!(matches!(
            engine.prepare_task_result_ref_record(
                substitution_operation,
                current_task.revision,
                substituted,
            ),
            Err(HarnessEngineError::InvalidResultRefRecordProjection),
        ));

        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn dispatch_outcome_commit_is_atomic_exact_replay_and_fail_closed() {
        for outcome_unknown in [false, true] {
            let mut current_task = task(task_id('a'), 2, "dispatch task");
            current_task.state = HarnessTaskStateV1::Running;
            current_task.run_ids = vec![run_id()];
            let current_run = run(HarnessRunLifecycleV1::Dispatching, 2);
            let mut current_operation = original_run_operation(
                HarnessOperationStateV1::Dispatching,
            );
            current_operation.revision = revision(2);
            let engine = HarnessEngine::restore(HarnessEngineCheckpointV1 {
                version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
                tasks: vec![current_task.clone()],
                runs: vec![current_run],
                grants: Vec::new(),
                operations: vec![current_operation.clone()],
                execution_specs: Vec::new(),
                issuances: Vec::new(),
                execution_specs_v2: Vec::new(),
                deliveries: Vec::new(),
                continuations: Vec::new(),
            }).unwrap();
            let before = engine.checkpoint();
            let next_lifecycle = if outcome_unknown {
                HarnessRunLifecycleV1::OutcomeUnknown
            } else {
                HarnessRunLifecycleV1::Failed
            };
            let mut next_run = run(next_lifecycle, 3);
            next_run.updated_at_unix_ms = 20;
            let mut next_operation = current_operation;
            next_operation.revision = revision(3);
            next_operation.updated_at_unix_ms = 20;
            let mut next_task = current_task;
            next_task.revision = revision(3);
            next_task.updated_at_unix_ms = 20;
            if outcome_unknown {
                next_operation.state = HarnessOperationStateV1::OutcomeUnknown;
                next_operation.outcome_unknown_reason = Some(
                    HarnessOutcomeUnknownReasonV1::ReplyLost,
                );
                next_task.state = HarnessTaskStateV1::Waiting;
            } else {
                let failure = HarnessFailureV1 {
                    category: HarnessFailureCategoryV1::Rejected,
                    retryable: false,
                };
                next_run.failure = Some(failure.clone());
                next_operation.state = HarnessOperationStateV1::Failed;
                next_operation.failure = Some(failure);
                next_operation.finished_at_unix_ms = Some(20);
                next_task.state = HarnessTaskStateV1::Failed;
            }
            let prepared = engine.prepare_dispatch_outcome_commit(
                revision(2),
                next_run.clone(),
                revision(2),
                next_operation.clone(),
                revision(2),
                next_task.clone(),
            ).unwrap();
            assert_eq!(prepared.outcome(), HarnessApplyOutcome::Applied);
            assert_eq!(engine.checkpoint(), before);
            let mut committed = engine.clone();
            committed.accept(prepared);
            let committed_snapshot = committed.checkpoint();
            assert_eq!(committed.prepare_dispatch_outcome_commit(
                revision(2),
                next_run.clone(),
                revision(2),
                next_operation.clone(),
                revision(2),
                next_task.clone(),
            ).unwrap().outcome(), HarnessApplyOutcome::Replayed);
            assert_eq!(committed.checkpoint(), committed_snapshot);

            let mut changed_task = next_task.clone();
            changed_task.title.push_str(" drift");
            assert!(committed.prepare_dispatch_outcome_commit(
                revision(2),
                next_run.clone(),
                revision(2),
                next_operation.clone(),
                revision(2),
                changed_task,
            ).is_err());
            assert_eq!(committed.checkpoint(), committed_snapshot);
            assert!(engine.prepare_dispatch_outcome_commit(
                revision(1),
                next_run.clone(),
                revision(2),
                next_operation.clone(),
                revision(2),
                next_task.clone(),
            ).is_err());
            let mut cross_operation = next_operation;
            cross_operation.run_id = Some(numbered_run_id(99));
            assert!(engine.prepare_dispatch_outcome_commit(
                revision(2),
                next_run,
                revision(2),
                cross_operation,
                revision(2),
                next_task,
            ).is_err());
            assert_eq!(engine.checkpoint(), before);
        }
    }

    #[test]
    fn harness_grant_revoke_is_terminal_and_cannot_reactivate() {
        let active = grant(SessionGrantStateV1::Active, 1);
        let still_active = grant(SessionGrantStateV1::Active, 2);
        assert!(matches!(
            validate_grant_transition(
                &active,
                &still_active,
                HarnessOperationKindV1::RevokeGrant,
            ),
            Err(HarnessEngineError::RevokeGrantMustRevoke)
        ));
        let revoked = grant(SessionGrantStateV1::Revoked, 2);
        assert!(validate_grant_transition(
            &active,
            &revoked,
            HarnessOperationKindV1::RevokeGrant,
        ).is_ok());
        assert!(matches!(
            validate_grant_transition(
                &revoked,
                &grant(SessionGrantStateV1::Active, 3),
                HarnessOperationKindV1::MutateGrant,
            ),
            Err(HarnessEngineError::RevokedGrantMutation)
        ));
    }

    #[test]
    fn harness_mutation_prevalidates_bounds_before_serialization() {
        let id = task_id('a');
        let mut unbounded = task(id.clone(), 1, "valid");
        unbounded.title = "x".repeat(
            gate4agent_harness_protocol::HARNESS_TITLE_MAX_BYTES + 1,
        );
        let mutation = HarnessMutationV1::CreateTask {
            operation: task_operation(operation_id('a'), id, 'a', None),
            task: unbounded,
        };
        assert!(matches!(
            mutation.validate_payload(),
            Err(HarnessEngineError::Validation(
                gate4agent_harness_protocol::HarnessValidationError::InvalidTitle
            ))
        ));
    }

    #[test]
    fn harness_entity_mutation_rejects_non_authoritative_operation_state() {
        let mut engine = HarnessEngine::new();
        let (id, _) = create_task(&mut engine);
        let before = engine.checkpoint();
        let mut operation = task_operation(
            operation_id('b'),
            id.clone(),
            'b',
            Some(revision(1)),
        );
        operation.state = HarnessOperationStateV1::Prepared;
        operation.finished_at_unix_ms = None;
        assert!(matches!(
            engine.prepare(HarnessMutationV1::ReplaceTask {
                operation,
                expected_revision: revision(1),
                task: task(id, 2, "changed"),
            }),
            Err(HarnessEngineError::WrongOperationState { .. })
        ));
        assert_eq!(engine.checkpoint(), before);
    }

    #[test]
    fn harness_run_operation_authority_rejects_generic_dispatch_shortcut() {
        let requested = run(HarnessRunLifecycleV1::Requested, 1);
        let dispatching = run(HarnessRunLifecycleV1::Dispatching, 2);
        let mut generic = task_operation(
            operation_id('c'),
            task_id('a'),
            'c',
            Some(revision(1)),
        );
        generic.kind = HarnessOperationKindV1::MutateRun;
        generic.task_id = None;
        generic.run_id = Some(run_id());
        assert!(matches!(
            validate_generic_run_operation(&requested, &dispatching, &generic),
            Err(HarnessEngineError::InvalidGenericRunOperation)
        ));
        generic.kind = HarnessOperationKindV1::BindRun;
        generic.state = HarnessOperationStateV1::Succeeded;
        assert!(matches!(
            validate_generic_run_operation(&requested, &dispatching, &generic),
            Err(HarnessEngineError::InvalidGenericRunOperation)
        ));

        let mut coupled = generic;
        coupled.kind = HarnessOperationKindV1::CreateRun;
        coupled.state = HarnessOperationStateV1::Dispatching;
        assert!(validate_coupled_run_operation(
            &requested,
            &dispatching,
            &coupled,
        ).is_ok());
        coupled.state = HarnessOperationStateV1::Prepared;
        assert!(matches!(
            validate_coupled_run_operation(&requested, &dispatching, &coupled),
            Err(HarnessEngineError::InvalidCoupledRunOperation)
        ));
    }

    #[test]
    fn harness_restore_rejects_orphan_and_incoherent_run_authority() {
        let mut linked_task = task(task_id('a'), 1, "linked");
        linked_task.run_ids = vec![run_id()];
        let requested = run(HarnessRunLifecycleV1::Requested, 1);
        let mut orphan = requested.clone();
        orphan.operation_id = operation_id('c');
        let orphan_checkpoint = HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![linked_task.clone()],
            runs: vec![orphan],
            grants: Vec::new(),
            operations: vec![original_run_operation(HarnessOperationStateV1::Prepared)],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        };
        assert!(matches!(
            HarnessEngine::restore(orphan_checkpoint),
            Err(HarnessEngineError::NotFound(_))
        ));

        let incoherent_checkpoint = HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![linked_task],
            runs: vec![requested],
            grants: Vec::new(),
            operations: vec![original_run_operation(HarnessOperationStateV1::Dispatching)],
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        };
        assert!(matches!(
            HarnessEngine::restore(incoherent_checkpoint),
            Err(HarnessEngineError::RunOperationStateIncoherent { .. })
        ));
    }

    #[test]
    fn harness_restore_rejects_missing_parent_run_actor() {
        let mut authored = task(task_id('a'), 1, "authored");
        authored.creator = HarnessActorV1::ParentRun { run_id: run_id() };
        let checkpoint = HarnessEngineCheckpointV1 {
            version: HARNESS_ENGINE_CHECKPOINT_VERSION_V1,
            tasks: vec![authored],
            runs: Vec::new(),
            grants: Vec::new(),
            operations: Vec::new(),
            execution_specs: Vec::new(),
            issuances: Vec::new(),
            execution_specs_v2: Vec::new(),
            deliveries: Vec::new(),
            continuations: Vec::new(),
        };
        assert!(matches!(
            HarnessEngine::restore(checkpoint),
            Err(HarnessEngineError::NotFound(_))
        ));
    }

    #[test]
    fn harness_read_visibility_allows_descendants_and_denies_cross_tree() {
        let engine = visibility_fixture();
        let visibility = engine.read_visibility(&grant_id()).unwrap();

        for run_id in [numbered_run_id(1), numbered_run_id(2), numbered_run_id(3)] {
            assert!(visibility.run_visible(&run_id));
        }
        assert!(!visibility.run_visible(&numbered_run_id(4)));
        for task_id in [
            numbered_task_id(1),
            numbered_task_id(2),
            numbered_task_id(3),
            numbered_task_id(5),
            numbered_task_id(6),
        ] {
            assert!(visibility.task_visible(&task_id));
        }
        assert!(!visibility.task_visible(&numbered_task_id(4)));
        assert!(!visibility.task_visible(&numbered_task_id(7)));
        for operation_id in [
            operation_id('1'),
            operation_id('2'),
            operation_id('5'),
            operation_id('7'),
            operation_id('8'),
        ] {
            assert!(visibility.operation_visible(&operation_id));
        }
        assert!(!visibility.operation_visible(&operation_id('3')));
        assert!(!visibility.operation_visible(&operation_id('4')));
        assert!(!visibility.operation_visible(&operation_id('6')));
        assert!(!visibility.operation_visible(&operation_id('9')));

        assert_eq!(visibility.run_ids().count(), 3);
        assert_eq!(visibility.task_ids().count(), 5);
        assert_eq!(visibility.operation_ids().count(), 5);
    }

    #[test]
    fn harness_read_visibility_self_only_requires_direct_root_attribution() {
        let mut engine = visibility_fixture();
        engine.grants.get_mut(&grant_id()).unwrap().read_permissions =
            HarnessReadPermissionsV1 {
                tasks: HarnessEntityReadScopeV1::SelfOnly,
                runs: HarnessEntityReadScopeV1::SelfOnly,
                operations: HarnessEntityReadScopeV1::SelfOnly,
            };

        let visibility = engine.read_visibility(&grant_id()).unwrap();
        assert!(visibility.run_visible(&numbered_run_id(1)));
        assert!(!visibility.run_visible(&numbered_run_id(2)));
        assert!(visibility.task_visible(&numbered_task_id(1)));
        assert!(visibility.task_visible(&numbered_task_id(6)));
        assert!(!visibility.task_visible(&numbered_task_id(2)));
        assert!(!visibility.task_visible(&numbered_task_id(5)));
        assert!(!visibility.task_visible(&numbered_task_id(7)));
        assert!(visibility.operation_visible(&operation_id('1')));
        assert!(!visibility.operation_visible(&operation_id('2')));
        assert!(!visibility.operation_visible(&operation_id('3')));
        assert!(!visibility.operation_visible(&operation_id('5')));
    }

    #[test]
    fn harness_read_visibility_mixed_scopes_do_not_cross_authorize_targets() {
        let mut engine = visibility_fixture();
        engine.grants.get_mut(&grant_id()).unwrap().read_permissions =
            HarnessReadPermissionsV1 {
                tasks: HarnessEntityReadScopeV1::SelfOnly,
                runs: HarnessEntityReadScopeV1::Descendants,
                operations: HarnessEntityReadScopeV1::Descendants,
            };

        let visibility = engine.read_visibility(&grant_id()).unwrap();
        assert!(visibility.run_visible(&numbered_run_id(2)));
        assert!(!visibility.task_visible(&numbered_task_id(2)));
        assert!(!visibility.operation_visible(&operation_id('2')));
        assert!(visibility.operation_visible(&operation_id('5')));

        engine.grants.get_mut(&grant_id()).unwrap().read_permissions =
            HarnessReadPermissionsV1 {
                tasks: HarnessEntityReadScopeV1::Descendants,
                runs: HarnessEntityReadScopeV1::SelfOnly,
                operations: HarnessEntityReadScopeV1::Descendants,
            };
        let visibility = engine.read_visibility(&grant_id()).unwrap();
        assert!(visibility.task_visible(&numbered_task_id(2)));
        assert!(!visibility.run_visible(&numbered_run_id(2)));
        assert!(visibility.operation_visible(&operation_id('2')));
        assert!(!visibility.operation_visible(&operation_id('5')));
    }

    #[test]
    fn harness_read_visibility_hidden_cross_tree_target_hides_operation() {
        let engine = visibility_fixture();
        let visibility = engine.read_visibility(&grant_id()).unwrap();

        assert!(!visibility.task_visible(&numbered_task_id(4)));
        assert!(!visibility.operation_visible(&operation_id('3')));
    }

    #[test]
    fn harness_read_visibility_hides_grant_operations_and_hidden_reconciliations() {
        let engine = visibility_fixture();
        let visibility = engine.read_visibility(&grant_id()).unwrap();

        assert!(!visibility.operation_visible(&operation_id('6')));
        assert!(visibility.operation_visible(&operation_id('7')));
        assert!(visibility.operation_visible(&operation_id('8')));
        assert!(!visibility.operation_visible(&operation_id('4')));
        assert!(!visibility.operation_visible(&operation_id('9')));
    }

    #[test]
    fn harness_read_visibility_reconcile_cycle_fails_closed() {
        let mut engine = visibility_fixture();
        let root_run_id = numbered_run_id(1);
        let first = visibility_reconcile_operation(
            operation_id('a'),
            'a',
            root_run_id.clone(),
            root_run_id.clone(),
            operation_id('b'),
        );
        let second = visibility_reconcile_operation(
            operation_id('b'),
            'b',
            root_run_id.clone(),
            root_run_id,
            operation_id('a'),
        );
        engine.operations.insert(first.operation_id.clone(), first);
        engine.operations.insert(second.operation_id.clone(), second);

        assert!(matches!(
            engine.read_visibility(&grant_id()),
            Err(HarnessEngineError::ReadVisibilityInvalidGraph),
        ));
    }

    #[test]
    fn harness_read_visibility_missing_reconcile_target_fails_closed() {
        let mut engine = visibility_fixture();
        let root_run_id = numbered_run_id(1);
        let invalid = visibility_reconcile_operation(
            operation_id('c'),
            'c',
            root_run_id.clone(),
            root_run_id,
            operation_id('d'),
        );
        engine.operations.insert(invalid.operation_id.clone(), invalid);

        assert!(matches!(
            engine.read_visibility(&grant_id()),
            Err(HarnessEngineError::ReadVisibilityInvalidGraph),
        ));
    }

    #[test]
    fn harness_read_visibility_revoked_invalid_and_missing_grants_are_empty() {
        let mut engine = visibility_fixture();
        assert!(engine.read_visibility(
            &SessionGrantId::new(format!("hgrant_{}", "b".repeat(24))).unwrap(),
        ).unwrap().is_empty());

        let stored = engine.grants.get_mut(&grant_id()).unwrap();
        stored.state = SessionGrantStateV1::Revoked;
        assert!(engine.read_visibility(&grant_id()).unwrap().is_empty());

        let stored = engine.grants.get_mut(&grant_id()).unwrap();
        stored.state = SessionGrantStateV1::Active;
        stored.task_permissions.read = false;
        assert!(engine.read_visibility(&grant_id()).unwrap().is_empty());
    }

    #[test]
    fn harness_read_visibility_cycle_fails_closed() {
        let mut engine = visibility_fixture();
        engine.runs.get_mut(&numbered_run_id(1)).unwrap().parent_run_id =
            Some(numbered_run_id(2));
        assert!(matches!(
            engine.read_visibility(&grant_id()),
            Err(HarnessEngineError::ReadVisibilityInvalidGraph),
        ));
    }

    #[test]
    fn harness_read_visibility_depth_fails_closed() {
        let mut engine = visibility_fixture();
        engine.runs.clear();
        let owning_task_id = numbered_task_id(1);
        let mut parent = None;
        for index in 0..=usize::from(HARNESS_CHILD_DEPTH_MAX) + 1 {
            let id = numbered_run_id(index + 1);
            let record = visibility_run(id.clone(), parent, owning_task_id.clone());
            engine.runs.insert(id.clone(), record);
            parent = Some(id);
        }
        engine.grants.get_mut(&grant_id()).unwrap().actor_run_id = numbered_run_id(1);
        assert!(matches!(
            engine.read_visibility(&grant_id()),
            Err(HarnessEngineError::ReadVisibilityInvalidGraph),
        ));
    }

    /// The measured live regression (2026-09-09): `validate_visibility_scan_bound`
    /// used to sum `tasks.len() + runs.len() + operations.len()` and refuse
    /// once that total passed `HARNESS_VISIBILITY_SCAN_MAX`, even though
    /// neither graph validator it gated (`validate_visibility_run_graph`,
    /// `validate_visibility_operation_graph`) ever inspects `tasks` at all,
    /// and even though both are already bounded by their own per-chain
    /// checks, never by total store size. Ordinary use grows `operations`
    /// (and often `tasks`) forever, so that aggregate bound was a cliff
    /// every long-lived store eventually walked off and never recovered
    /// from -- past it, every reader's mailbox and session-list reads
    /// failed `Internal` for the life of the store, with no recovery short
    /// of a new database. This fixture (padded with unlinked tasks well
    /// past the old ceiling, so the run/operation graphs the two
    /// validators actually walk are untouched and stay structurally valid)
    /// used to prove the refusal; it now proves the read survives it. This
    /// test's own name and assertion changed from `..._fails_closed` for
    /// exactly that reason: the old assertion encoded the bug this pass
    /// fixes, so keeping it "as written" would mean re-breaking the fix.
    #[test]
    fn harness_read_visibility_survives_a_store_past_the_old_scan_ceiling() {
        let mut engine = visibility_fixture();
        engine.tasks.clear();
        for index in 0..HARNESS_VISIBILITY_SCAN_MAX {
            let id = numbered_task_id(index + 1);
            engine.tasks.insert(id.clone(), task(id, 1, "bounded scan"));
        }
        assert!(
            engine.tasks.len() + engine.runs.len() + engine.operations.len()
                > HARNESS_VISIBILITY_SCAN_MAX,
        );
        assert!(engine.read_visibility(&grant_id()).is_ok());
    }

    #[test]
    fn harness_task_subtree_three_level_tree_and_depth_bound_cuts() {
        // Ids assigned in descending order down the tree (root's own id
        // sorts highest) so a single outer pass of `task_subtree` can only
        // ever pull in one generation at a time regardless of `BTreeMap`
        // iteration order -- the same property `visible_runs_for_scope`
        // relies on for its own depth fence, made explicit here so the
        // depth-bound assertion below is not an artifact of id ordering.
        let root_id = numbered_task_id(30);
        let child_id = numbered_task_id(20);
        let grandchild_id = numbered_task_id(10);
        let stranger_id = numbered_task_id(99);

        let mut engine = HarnessEngine::new();
        let root = task(root_id.clone(), 1, "root");
        let mut child = task(child_id.clone(), 1, "child");
        child.parent_task_id = Some(root_id.clone());
        let mut grandchild = task(grandchild_id.clone(), 1, "grandchild");
        grandchild.parent_task_id = Some(child_id.clone());
        let stranger = task(stranger_id.clone(), 1, "stranger");
        for record in [root, child, grandchild, stranger] {
            engine.tasks.insert(record.task_id.clone(), record);
        }

        let root_subtree = engine.task_subtree(&root_id, HARNESS_CHILD_DEPTH_MAX);
        assert_eq!(
            root_subtree,
            BTreeSet::from([root_id.clone(), child_id.clone(), grandchild_id.clone()]),
        );

        let leaf_subtree = engine.task_subtree(&grandchild_id, HARNESS_CHILD_DEPTH_MAX);
        assert_eq!(leaf_subtree, BTreeSet::from([grandchild_id.clone()]));

        assert!(!engine.task_is_strict_descendant(&stranger_id, &root_id));
        assert!(engine.task_is_strict_descendant(&child_id, &root_id));
        assert!(engine.task_is_strict_descendant(&grandchild_id, &root_id));
        assert!(!engine.task_is_strict_descendant(&root_id, &root_id), "a root is not its own strict descendant");

        let bounded = engine.task_subtree(&root_id, 0);
        assert_eq!(
            bounded,
            BTreeSet::from([root_id, child_id]),
            "a zero depth bound cuts the grandchild generation",
        );
    }

    /// `task_child_count`/`task_subtree_depth` back `SessionContextV1`'s own
    /// `child_task_count`/`child_task_subtree_depth` observations. Reuses
    /// the same three-level tree (root -> child -> grandchild, plus an
    /// unrelated stranger) `harness_task_subtree_three_level_tree_and_depth_
    /// bound_cuts` builds, so the depth assertion below exercises a genuine
    /// two-level subtree beneath the root, not just a single generation.
    #[test]
    fn harness_task_child_count_and_subtree_depth_observe_two_level_tree() {
        let root_id = numbered_task_id(30);
        let child_id = numbered_task_id(20);
        let grandchild_id = numbered_task_id(10);
        let stranger_id = numbered_task_id(99);

        let mut engine = HarnessEngine::new();
        let root = task(root_id.clone(), 1, "root");
        let mut child = task(child_id.clone(), 1, "child");
        child.parent_task_id = Some(root_id.clone());
        let mut grandchild = task(grandchild_id.clone(), 1, "grandchild");
        grandchild.parent_task_id = Some(child_id.clone());
        let stranger = task(stranger_id.clone(), 1, "stranger");
        for record in [root, child, grandchild, stranger] {
            engine.tasks.insert(record.task_id.clone(), record);
        }

        // A task with children: the root has exactly one direct child
        // (`child`, not `grandchild` -- that one is parented under `child`)
        // and its deepest chain reaches two generations down.
        assert_eq!(engine.task_child_count(&root_id), 1);
        assert_eq!(engine.task_subtree_depth(&root_id), 2);

        assert_eq!(engine.task_child_count(&child_id), 1);
        assert_eq!(engine.task_subtree_depth(&child_id), 1);

        // A task without children: both observations are zero, never an
        // error or a refusal -- there is no cap here to compare against.
        assert_eq!(engine.task_child_count(&grandchild_id), 0);
        assert_eq!(engine.task_subtree_depth(&grandchild_id), 0);
        assert_eq!(engine.task_child_count(&stranger_id), 0);
        assert_eq!(engine.task_subtree_depth(&stranger_id), 0);
    }

    #[test]
    fn harness_tasks_attributed_to_runs_includes_a_task_whose_creator_is_that_run() {
        let mut engine = HarnessEngine::new();
        let attributing_run = run_id();
        let mut authored = task(task_id('a'), 1, "authored by its own run");
        authored.creator = HarnessActorV1::ParentRun { run_id: attributing_run.clone() };
        engine.tasks.insert(authored.task_id.clone(), authored.clone());

        let attributed = engine.tasks_attributed_to_runs(&BTreeSet::from([attributing_run]));
        assert!(attributed.contains(&authored.task_id));

        assert!(engine.tasks_attributed_to_runs(&BTreeSet::new()).is_empty());

        let other_run = numbered_run_id(2);
        assert!(!engine.tasks_attributed_to_runs(&BTreeSet::from([other_run])).contains(&authored.task_id));
    }

    #[test]
    fn harness_operations_for_task_are_newest_first_bounded_and_store_the_prepared_actor() {
        // A `ParentRun` actor validates only against a run that already
        // exists, so two runs are seeded first, each with its own
        // placeholder task and coherent `CreateRun` operation (the same
        // direct-construction shape `visibility_fixture` uses elsewhere in
        // this module) -- neither is the task under test.
        let placeholder_task_id = task_id('e');
        let authoring_run_id = run_id();
        let replacing_run_id = numbered_run_id(9);
        let mut engine = HarnessEngine::new();
        let mut placeholder_task = task(placeholder_task_id.clone(), 1, "placeholder");
        // Sorted ascending (`validate_sorted_ids`/the backlink check both
        // read `run_ids` as a sorted set): `replacing_run_id` (`hrun_000...09`)
        // sorts before `authoring_run_id` (`hrun_aaa...`).
        placeholder_task.run_ids = vec![replacing_run_id.clone(), authoring_run_id.clone()];
        engine.tasks.insert(placeholder_task_id.clone(), placeholder_task);

        let mut authoring_run = run(HarnessRunLifecycleV1::Requested, 1);
        authoring_run.task_id = placeholder_task_id.clone();
        engine.runs.insert(authoring_run.run_id.clone(), authoring_run);
        let mut authoring_run_operation = original_run_operation(HarnessOperationStateV1::Prepared);
        authoring_run_operation.task_id = Some(placeholder_task_id.clone());
        engine.operations.insert(operation_id('b'), authoring_run_operation);

        let mut replacing_run = run(HarnessRunLifecycleV1::Requested, 1);
        replacing_run.run_id = replacing_run_id.clone();
        replacing_run.task_id = placeholder_task_id.clone();
        replacing_run.operation_id = operation_id('c');
        engine.runs.insert(replacing_run_id.clone(), replacing_run);
        let mut replacing_run_operation = original_run_operation(HarnessOperationStateV1::Prepared);
        replacing_run_operation.operation_id = operation_id('c');
        replacing_run_operation.run_id = Some(replacing_run_id.clone());
        replacing_run_operation.task_id = Some(placeholder_task_id);
        engine.operations.insert(operation_id('c'), replacing_run_operation);

        let id = task_id('a');
        let mut created_task = task(id.clone(), 1, "authored by its own run");
        created_task.creator = HarnessActorV1::ParentRun { run_id: authoring_run_id.clone() };
        // Deliberately the alphabetically LATER operation id for the
        // EARLIER (lower `created_at_unix_ms`) operation: if
        // `operations_for_task` ever regressed to an id-only sort instead
        // of a `created_at_unix_ms`-first one, this ordering would flip
        // and the assertions below would fail.
        let mut create_operation = task_operation(operation_id('f'), id.clone(), 'f', None);
        create_operation.actor = HarnessActorV1::ParentRun { run_id: authoring_run_id.clone() };
        let prepared = engine.prepare(HarnessMutationV1::CreateTask {
            operation: create_operation.clone(),
            task: created_task,
        }).unwrap();
        engine.accept(prepared);

        let mut replaced_task = task(id.clone(), 2, "authored by its own run, replaced");
        replaced_task.creator = HarnessActorV1::ParentRun { run_id: authoring_run_id.clone() };
        let mut replace_operation = task_operation(operation_id('a'), id.clone(), 'd', Some(revision(1)));
        replace_operation.actor = HarnessActorV1::ParentRun { run_id: replacing_run_id.clone() };
        replace_operation.created_at_unix_ms = 20;
        replace_operation.updated_at_unix_ms = 22;
        replace_operation.finished_at_unix_ms = Some(22);
        let prepared = engine.prepare(HarnessMutationV1::ReplaceTask {
            operation: replace_operation.clone(),
            expected_revision: revision(1),
            task: replaced_task,
        }).unwrap();
        engine.accept(prepared);

        let ledger = engine.operations_for_task(&id, 10);
        assert_eq!(ledger.len(), 2, "only the two operations naming this task, not the seeded run operations");
        assert_eq!(ledger[0].operation_id, replace_operation.operation_id, "newest (highest created_at_unix_ms) first");
        assert_eq!(ledger[1].operation_id, create_operation.operation_id);
        assert_eq!(ledger[0].actor, HarnessActorV1::ParentRun { run_id: replacing_run_id });
        assert_eq!(ledger[1].actor, HarnessActorV1::ParentRun { run_id: authoring_run_id });

        let bounded = engine.operations_for_task(&id, 1);
        assert_eq!(bounded.len(), 1);
        assert_eq!(bounded[0].operation_id, replace_operation.operation_id);
    }
}
