use crate::{
    credential::{CredentialBindingV1, VerifiedCredentialV1},
    runtime::ObservationSupportRegistry,
    HarnessService,
};
use gate4agent_harness_api::*;
use gate4agent_harness_engine::HarnessReadVisibilityV1;
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessEntityReadScopeV1, HarnessMonitoringVisibilityV1,
    HarnessSessionIdentityV1, HarnessWorktreeIntentV1, SessionGrantV1,
};
use gate4agent_observation_api::{
    ManagedSessionKey, ObservationTarget, ProjectionAvailability, ProjectionFreshness,
};
use gate4agent_observation_engine::{CorrelationProjection, CorrelationState, SessionProjection};
use gate4agent_observation_protocol::{
    ObservationEvidenceV1 as SourceEvidenceV1, ObservationInteractionOutcomeV1,
    ObservationKindV1, ObservationTodoStateV1,
};
use gate4agent_observation_service::ObservationService;

#[cfg(test)]
pub(crate) fn verify_and_execute_read(
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    authority: &crate::credential::CredentialAuthority,
    credential: &HarnessReadCredential,
    now_unix_ms: u64,
    request: HarnessReadRequestV1,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    let claims = authority.verify(harness.engine(), credential, now_unix_ms)
        .map_err(|_| HarnessReadHostErrorV1::Unauthorized)?;
    verify_observation_binding(observation, support, &claims)?;
    let response = execute_read(harness, observation, support, &claims, request)?;
    response.validate().map_err(|_| HarnessReadHostErrorV1::Internal)?;
    Ok(response)
}

#[cfg(test)]
fn verify_observation_binding(
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    claims: &VerifiedCredentialV1,
) -> Result<(), HarnessReadHostErrorV1> {
    verify_observation_credential_binding(observation, support, &claims.binding)
}

pub(crate) fn verify_observation_credential_binding(
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    binding: &crate::credential::CredentialBindingV1,
) -> Result<(), HarnessReadHostErrorV1> {
    let node_id = gate4agent_observation_api::NodeId::new(binding.node_id.as_str())
        .map_err(|_| HarnessReadHostErrorV1::Unauthorized)?;
    let incarnation_id = binding.node_incarnation.as_str().parse()
        .map_err(|_| HarnessReadHostErrorV1::Unauthorized)?;
    let record_id = gate4agent_observation_api::SessionRecordId::new(
        binding.record_id.as_str(),
    ).map_err(|_| HarnessReadHostErrorV1::Unauthorized)?;
    let key = ManagedSessionKey { node_id, incarnation_id, record_id };
    if !support.is_authoritative(&key.node_id, key.incarnation_id) {
        return Err(HarnessReadHostErrorV1::Unauthorized);
    }
    let runtime = observation.engine().managed_runtime(&key)
        .ok_or(HarnessReadHostErrorV1::Unauthorized)?;
    if runtime.workspace_id.as_str() != binding.workspace_id.as_str()
        || runtime.instance_id.0 != binding.instance_id
        || runtime.generation.0 != binding.generation
        || observation.projection(&ObservationTarget::Managed { key }).is_none()
    {
        return Err(HarnessReadHostErrorV1::Unauthorized);
    }
    Ok(())
}

pub(crate) fn execute_read(
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    claims: &VerifiedCredentialV1,
    request: HarnessReadRequestV1,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    execute_exact_binding_read(
        harness,
        observation,
        support,
        &claims.binding,
        request,
    )
}

pub(crate) fn execute_operator_monitor(
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    run_id: &HarnessRunId,
) -> Result<SessionMonitorV1, HarnessReadHostErrorV1> {
    match monitor(
        harness.engine(),
        observation,
        support,
        HarnessMonitoringVisibilityV1::Timeline,
        run_id,
    )? {
        HarnessReadResponseV1::Monitor(value) => Ok(value),
        _ => Err(HarnessReadHostErrorV1::Internal),
    }
}

pub(crate) fn execute_operator_timeline(
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    run_id: &HarnessRunId,
    after_sequence: Option<u64>,
    limit: u16,
) -> Result<TimelinePageV1, HarnessReadHostErrorV1> {
    match timeline(
        harness.engine(),
        observation,
        support,
        run_id,
        after_sequence,
        limit,
    )? {
        HarnessReadResponseV1::Timeline(value) => Ok(value),
        _ => Err(HarnessReadHostErrorV1::Internal),
    }
}

pub(crate) fn execute_exact_binding_read(
    harness: &HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    binding: &CredentialBindingV1,
    request: HarnessReadRequestV1,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    request.validate().map_err(|_| HarnessReadHostErrorV1::InvalidRequest)?;
    let engine = harness.engine();
    let grant = engine.grant(&binding.grant_id)
        .filter(|grant| {
            grant.revision == binding.grant_revision
                && grant.actor_run_id == binding.actor_run_id
        })
        .ok_or(HarnessReadHostErrorV1::Unauthorized)?;
    let visibility = engine.read_visibility(&grant.grant_id)
        .map_err(|_| HarnessReadHostErrorV1::Internal)?;
    authorize_request(grant, &request)?;
    match request {
        HarnessReadRequestV1::ContextGet => context(
            engine, observation, grant, binding, &visibility,
        ),
        HarnessReadRequestV1::MonitorGet { run_id } => {
            let run_id = authorized_monitor_run(engine, grant, binding, &visibility, run_id)?;
            monitor(
                engine,
                observation,
                support,
                grant.monitoring_visibility,
                &run_id,
            )
        }
        HarnessReadRequestV1::TimelineRead { run_id, after_sequence, limit } => {
            if grant.monitoring_visibility != HarnessMonitoringVisibilityV1::Timeline {
                return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
            }
            let run_id = authorized_monitor_run(engine, grant, binding, &visibility, run_id)?;
            timeline(engine, observation, support, &run_id, after_sequence, limit)
        }
        HarnessReadRequestV1::TasksList { after_task_id, state, limit } => {
            let mut values = visibility.task_ids()
                .filter(|task_id| after_task_id.as_ref().map_or(true, |after| *task_id > after))
                .filter_map(|task_id| engine.task(task_id))
                .filter(|task| state.map_or(true, |state| task.state == state))
                .map(|task| redact_task(task, &visibility))
                .take(usize::from(limit) + 1)
                .collect::<Vec<_>>();
            let has_more = values.len() > usize::from(limit);
            if has_more { values.pop(); }
            let next_cursor = has_more.then(|| {
                values.last().expect("nonzero page limit").task_id.clone()
            });
            Ok(HarnessReadResponseV1::Tasks(TaskPageV1 { tasks: values, next_cursor }))
        }
        HarnessReadRequestV1::TaskGet { task_id } => {
            if !visibility.task_visible(&task_id) {
                return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
            }
            let task = engine.task(&task_id)
                .ok_or(HarnessReadHostErrorV1::NotFoundOrDenied)?;
            Ok(HarnessReadResponseV1::Task(redact_task(task, &visibility)))
        }
        HarnessReadRequestV1::RunsList { task_id, after_run_id, lifecycle, limit } => {
            if task_id.as_ref().is_some_and(|task_id| !visibility.task_visible(task_id)) {
                return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
            }
            let mut values = visibility.run_ids()
                .filter(|run_id| after_run_id.as_ref().map_or(true, |after| *run_id > after))
                .filter_map(|run_id| engine.run(run_id))
                .filter(|run| task_id.as_ref().map_or(true, |task_id| &run.task_id == task_id))
                .filter(|run| lifecycle.map_or(true, |lifecycle| run.lifecycle == lifecycle))
                .map(|run| redact_run(run, &visibility))
                .take(usize::from(limit) + 1)
                .collect::<Vec<_>>();
            let has_more = values.len() > usize::from(limit);
            if has_more { values.pop(); }
            let next_cursor = has_more.then(|| {
                values.last().expect("nonzero page limit").run_id.clone()
            });
            Ok(HarnessReadResponseV1::Runs(RunPageV1 { runs: values, next_cursor }))
        }
        HarnessReadRequestV1::RunGet { run_id } => {
            if !visibility.run_visible(&run_id) {
                return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
            }
            let run = engine.run(&run_id)
                .ok_or(HarnessReadHostErrorV1::NotFoundOrDenied)?;
            Ok(HarnessReadResponseV1::Run(redact_run(run, &visibility)))
        }
        HarnessReadRequestV1::OperationGet { operation_id } => {
            if !visibility.operation_visible(&operation_id) {
                return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
            }
            let operation = engine.operation(&operation_id)
                .ok_or(HarnessReadHostErrorV1::NotFoundOrDenied)?;
            Ok(HarnessReadResponseV1::Operation(redact_operation(operation, &visibility)))
        }
    }
}

fn authorize_request(
    grant: &SessionGrantV1,
    request: &HarnessReadRequestV1,
) -> Result<(), HarnessReadHostErrorV1> {
    match request {
        HarnessReadRequestV1::TasksList { .. } | HarnessReadRequestV1::TaskGet { .. }
            if grant.read_permissions.tasks == HarnessEntityReadScopeV1::None =>
        {
            return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
        }
        HarnessReadRequestV1::RunsList { .. } | HarnessReadRequestV1::RunGet { .. }
            if grant.read_permissions.runs == HarnessEntityReadScopeV1::None =>
        {
            return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
        }
        HarnessReadRequestV1::OperationGet { .. }
            if grant.read_permissions.operations == HarnessEntityReadScopeV1::None =>
        {
            return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
        }
        _ => {}
    }
    Ok(())
}

fn context(
    engine: &gate4agent_harness_engine::HarnessEngine,
    observation: &ObservationService,
    grant: &SessionGrantV1,
    binding: &CredentialBindingV1,
    visibility: &HarnessReadVisibilityV1,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    let run = engine.run(&binding.actor_run_id).ok_or(HarnessReadHostErrorV1::Unauthorized)?;
    let projection = projection_for_run(observation, run)?;
    let history = (grant.monitoring_visibility != HarnessMonitoringVisibilityV1::None)
        .then_some(projection)
        .flatten()
        .and_then(|projection| projection.history.as_ref());
    Ok(HarnessReadResponseV1::Context(SessionContextV1 {
        grant_id: grant.grant_id.clone(),
        grant_revision: grant.revision,
        actor_run: CallerRunV1 {
            run_id: run.run_id.clone(),
            task_id: visibility.task_visible(&run.task_id).then(|| run.task_id.clone()),
            parent_run_id: run.parent_run_id.as_ref()
                .filter(|run_id| visibility.run_visible(run_id)).cloned(),
            lifecycle: run.lifecycle,
            references_redacted: !visibility.task_visible(&run.task_id)
                || run.parent_run_id.as_ref().is_some_and(|run_id| {
                    !visibility.run_visible(run_id)
                }),
        },
        read_permissions: grant.read_permissions.clone(),
        monitoring_visibility: grant.monitoring_visibility,
        maximum_child_count: grant.maximum_child_count,
        maximum_child_depth: grant.maximum_child_depth,
        allowed_tool_ids: allowed_tool_ids(grant),
        history_message_count: history.map(|history| history.message_count),
        completed_turn_count: history.and_then(|history| history.completed_turn_count),
        total_tokens: history.and_then(|history| history.total_tokens),
    }))
}

fn authorized_monitor_run(
    engine: &gate4agent_harness_engine::HarnessEngine,
    grant: &SessionGrantV1,
    binding: &CredentialBindingV1,
    visibility: &HarnessReadVisibilityV1,
    requested: Option<HarnessRunId>,
) -> Result<HarnessRunId, HarnessReadHostErrorV1> {
    if grant.monitoring_visibility == HarnessMonitoringVisibilityV1::None {
        return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
    }
    let run_id = requested.unwrap_or_else(|| binding.actor_run_id.clone());
    if run_id != binding.actor_run_id && !visibility.run_visible(&run_id) {
        return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
    }
    if engine.run(&run_id).is_none() {
        return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
    }
    Ok(run_id)
}

fn monitor(
    engine: &gate4agent_harness_engine::HarnessEngine,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    visibility: HarnessMonitoringVisibilityV1,
    run_id: &HarnessRunId,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    let run = engine.run(run_id).ok_or(HarnessReadHostErrorV1::NotFoundOrDenied)?;
    let projection = projection_for_run(observation, run)?;
    let route_authoritative = route_authoritative_for_run(support, run);
    let (availability, freshness, transport_incomplete) = observation_state(
        projection,
        route_authoritative,
    );
    let todos = projection.and_then(|projection| projection.todos.current.as_ref())
        .map(|todo| todo.items.as_slice()).unwrap_or(&[]);
    let features = monitor_feature_states(projection, route_support_for_run(support, run));
    let detail_allowed = matches!(
        visibility,
        HarnessMonitoringVisibilityV1::Detail | HarnessMonitoringVisibilityV1::Timeline
    ) && matches!(
        availability,
        ProjectionAvailabilityV1::Current
            | ProjectionAvailabilityV1::Partial
            | ProjectionAvailabilityV1::Frozen
    );
    let detail = detail_allowed.then(|| monitor_detail(projection.expect("detail requires projection")));
    Ok(HarnessReadResponseV1::Monitor(SessionMonitorV1 {
        run_id: run_id.clone(),
        visibility,
        availability,
        freshness,
        transport_incomplete,
        features,
        todo_total: saturating_u16(todos.len()),
        todo_completed: saturating_u16(todos.iter().filter(|item| {
            item.state == ObservationTodoStateV1::Completed
        }).count()),
        active_tools: projection.map_or(0, |projection| active_count(&projection.tools)),
        active_subagents: projection.map_or(0, |projection| active_count(&projection.subagents)),
        active_interactions: projection.map_or(0, |projection| active_count(&projection.interactions)),
        active_processes: projection.map_or(0, |projection| active_count(&projection.owned_processes)),
        input_tokens: projection.map_or(0, |projection| projection.usage.observed_delta.input_tokens),
        output_tokens: projection.map_or(0, |projection| projection.usage.observed_delta.output_tokens),
        cache_read_tokens: projection.map_or(0, |projection| projection.usage.observed_delta.cache_read_tokens),
        cache_write_tokens: projection.map_or(0, |projection| projection.usage.observed_delta.cache_write_tokens),
        reasoning_tokens: projection.map_or(0, |projection| projection.usage.observed_delta.reasoning_tokens),
        context_window_tokens: projection.and_then(|projection| projection.usage.context_window),
        detail,
    }))
}

fn timeline(
    engine: &gate4agent_harness_engine::HarnessEngine,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    run_id: &HarnessRunId,
    after_sequence: Option<u64>,
    limit: u16,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    let run = engine.run(run_id).ok_or(HarnessReadHostErrorV1::NotFoundOrDenied)?;
    let projection = projection_for_run(observation, run)?;
    let (availability, freshness, transport_incomplete) = observation_state(
        projection,
        route_authoritative_for_run(support, run),
    );
    let mut entries = projection.into_iter()
        .flat_map(|projection| projection.timeline.iter())
        .filter(|entry| after_sequence.map_or(true, |after| entry.cursor.sequence > after))
        .map(|entry| TimelineEntryV1 {
            sequence: entry.cursor.sequence,
            received_at_ms: entry.received_at_ms,
            category: timeline_category(&entry.kind),
            evidence: observation_evidence(entry.evidence),
        })
        .take(usize::from(limit) + 1)
        .collect::<Vec<_>>();
    let has_more = entries.len() > usize::from(limit);
    if has_more { entries.pop(); }
    let next_cursor = has_more.then(|| entries.last().expect("nonzero page limit").sequence);
    Ok(HarnessReadResponseV1::Timeline(TimelinePageV1 {
        run_id: run_id.clone(),
        availability,
        freshness,
        transport_incomplete,
        entries,
        next_cursor,
    }))
}

fn projection_for_run<'a>(
    observation: &'a ObservationService,
    run: &gate4agent_harness_protocol::HarnessRunV1,
) -> Result<Option<&'a SessionProjection>, HarnessReadHostErrorV1> {
    let Some(binding) = run.binding.as_ref() else { return Ok(None); };
    let HarnessSessionIdentityV1::Managed { record_id, .. } = &binding.session else {
        return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
    };
    let node_id = gate4agent_observation_api::NodeId::new(binding.node_id.as_str())
        .map_err(|_| HarnessReadHostErrorV1::Internal)?;
    let incarnation_id = binding.node_incarnation.as_str().parse()
        .map_err(|_| HarnessReadHostErrorV1::Internal)?;
    let record_id = gate4agent_observation_api::SessionRecordId::new(record_id.as_str())
        .map_err(|_| HarnessReadHostErrorV1::Internal)?;
    Ok(observation.projection(&ObservationTarget::Managed {
        key: ManagedSessionKey { node_id, incarnation_id, record_id },
    }))
}

fn observation_state(
    projection: Option<&SessionProjection>,
    route_authoritative: bool,
) -> (ProjectionAvailabilityV1, ProjectionFreshnessV1, bool) {
    let Some(projection) = projection else {
        return (
            ProjectionAvailabilityV1::Unknown,
            ProjectionFreshnessV1::Unavailable,
            false,
        );
    };
    if !route_authoritative {
        return (
            ProjectionAvailabilityV1::Frozen,
            ProjectionFreshnessV1::LastKnown,
            true,
        );
    }
    let availability = match projection.availability {
        ProjectionAvailability::Unknown => ProjectionAvailabilityV1::Unknown,
        ProjectionAvailability::NotObserved => ProjectionAvailabilityV1::NotObserved,
        ProjectionAvailability::Current => ProjectionAvailabilityV1::Current,
        ProjectionAvailability::Partial => ProjectionAvailabilityV1::Partial,
        ProjectionAvailability::Frozen => ProjectionAvailabilityV1::Frozen,
    };
    let freshness = match projection.freshness {
        ProjectionFreshness::Unavailable => ProjectionFreshnessV1::Unavailable,
        ProjectionFreshness::Live => ProjectionFreshnessV1::Live,
        ProjectionFreshness::Stale => ProjectionFreshnessV1::Stale,
        ProjectionFreshness::IncompleteAfterGap => ProjectionFreshnessV1::IncompleteAfterGap,
        ProjectionFreshness::LastKnown => ProjectionFreshnessV1::LastKnown,
        ProjectionFreshness::ReplacedIncarnation => ProjectionFreshnessV1::ReplacedIncarnation,
    };
    (availability, freshness, projection.transport_incomplete)
}

fn allowed_tool_ids(grant: &SessionGrantV1) -> Vec<String> {
    let mut tools = vec!["g4a_context_get"];
    if grant.monitoring_visibility != HarnessMonitoringVisibilityV1::None {
        tools.push("g4a_monitor_get");
    }
    if grant.monitoring_visibility == HarnessMonitoringVisibilityV1::Timeline {
        tools.push("g4a_timeline_read");
    }
    if grant.read_permissions.tasks != HarnessEntityReadScopeV1::None {
        tools.extend(["g4a_tasks_get", "g4a_tasks_list"]);
    }
    if grant.read_permissions.runs != HarnessEntityReadScopeV1::None {
        tools.extend(["g4a_runs_get", "g4a_runs_list"]);
    }
    if grant.read_permissions.operations != HarnessEntityReadScopeV1::None {
        tools.push("g4a_operation_get");
    }
    tools.sort_unstable();
    tools.into_iter().map(str::to_owned).collect()
}

fn redact_task(
    task: &gate4agent_harness_protocol::HarnessTaskV1,
    visibility: &HarnessReadVisibilityV1,
) -> RedactedTaskV1 {
    let parent_task_id = task.parent_task_id.as_ref()
        .filter(|task_id| visibility.task_visible(task_id)).cloned();
    let dependency_ids = task.dependencies.iter()
        .filter(|task_id| visibility.task_visible(task_id)).cloned().collect::<Vec<_>>();
    let run_ids = task.run_ids.iter()
        .filter(|run_id| visibility.run_visible(run_id)).cloned().collect::<Vec<_>>();
    let references_redacted = parent_task_id != task.parent_task_id
        || dependency_ids.len() != task.dependencies.len()
        || run_ids.len() != task.run_ids.len();
    RedactedTaskV1 {
        task_id: task.task_id.clone(),
        revision: task.revision,
        title: task.title.clone(),
        body: task.body.clone(),
        creator: match task.creator {
            HarnessActorV1::User { .. } => TaskCreatorCategoryV1::User,
            HarnessActorV1::ParentRun { .. } => TaskCreatorCategoryV1::ParentRun,
        },
        parent_task_id,
        dependency_ids,
        state: task.state,
        run_ids,
        references_redacted,
        result_refs: task.result_refs.clone(),
        artifact_refs: task.artifact_refs.clone(),
        created_at_unix_ms: task.created_at_unix_ms,
        updated_at_unix_ms: task.updated_at_unix_ms,
    }
}

fn redact_run(
    run: &gate4agent_harness_protocol::HarnessRunV1,
    visibility: &HarnessReadVisibilityV1,
) -> RedactedRunV1 {
    let parent_run_id = run.parent_run_id.as_ref()
        .filter(|run_id| visibility.run_visible(run_id)).cloned();
    let task_id = visibility.task_visible(&run.task_id).then(|| run.task_id.clone());
    let operation_id = visibility.operation_visible(&run.operation_id)
        .then(|| run.operation_id.clone());
    let references_redacted = parent_run_id != run.parent_run_id
        || task_id.is_none()
        || operation_id.is_none();
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
        parent_run_id,
        task_id,
        operation_id,
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
        references_redacted,
        created_at_unix_ms: run.created_at_unix_ms,
        updated_at_unix_ms: run.updated_at_unix_ms,
    }
}

fn redact_operation(
    operation: &gate4agent_harness_protocol::HarnessOperationV1,
    visibility: &HarnessReadVisibilityV1,
) -> RedactedOperationV1 {
    let task_id = operation.task_id.as_ref()
        .filter(|task_id| visibility.task_visible(task_id)).cloned();
    let run_id = operation.run_id.as_ref()
        .filter(|run_id| visibility.run_visible(run_id)).cloned();
    let reconciles_operation_id = operation.reconciles_operation_id.as_ref()
        .filter(|operation_id| visibility.operation_visible(operation_id)).cloned();
    let references_redacted = task_id != operation.task_id
        || run_id != operation.run_id
        || reconciles_operation_id != operation.reconciles_operation_id
        || operation.grant_id.is_some();
    RedactedOperationV1 {
        operation_id: operation.operation_id.clone(),
        revision: operation.revision,
        kind: operation.kind,
        state: operation.state,
        task_id,
        run_id,
        reconciles_operation_id,
        references_redacted,
        failure_category: operation.failure.as_ref().map(|failure| failure.category),
        outcome_unknown_reason: operation.outcome_unknown_reason,
        reconciliation_outcome: operation.reconciliation_outcome,
        created_at_unix_ms: operation.created_at_unix_ms,
        updated_at_unix_ms: operation.updated_at_unix_ms,
        dispatched_at_unix_ms: operation.dispatched_at_unix_ms,
        finished_at_unix_ms: operation.finished_at_unix_ms,
    }
}

fn monitor_detail(projection: &SessionProjection) -> SessionMonitorDetailV1 {
    SessionMonitorDetailV1 {
        todo_facts: projection.todos.current.as_ref().map(|todo| todo.items.iter().map(|item| {
            TodoFactV1 {
                state: match item.state {
                    ObservationTodoStateV1::Pending => TodoStateV1::Pending,
                    ObservationTodoStateV1::InProgress => TodoStateV1::InProgress,
                    ObservationTodoStateV1::Completed => TodoStateV1::Completed,
                    ObservationTodoStateV1::Unknown => TodoStateV1::Unknown,
                },
                evidence: observation_evidence(todo.evidence),
            }
        }).collect()).unwrap_or_default(),
        tool_facts: projection.tools.iter()
            .map(|value| activity_fact(value, ActivityClassV1::Tool)).collect(),
        subagent_facts: projection.subagents.iter()
            .map(|value| activity_fact(value, ActivityClassV1::Subagent)).collect(),
        interaction_facts: projection.interactions.iter().map(interaction_fact).collect(),
        process_facts: projection.owned_processes.iter()
            .map(|value| activity_fact(value, ActivityClassV1::OwnedProcess)).collect(),
        file_facts: projection.files.iter().map(|value| FileFactV1 {
            action: FileActionV1::Changed,
            evidence: observation_evidence(value.evidence),
        }).collect(),
    }
}

fn activity_fact(value: &CorrelationProjection, class: ActivityClassV1) -> ActivityFactV1 {
    let state = match value.state {
        CorrelationState::Pending => ActivityStateV1::Active,
        CorrelationState::Completed { success: Some(false) }
        | CorrelationState::OrphanCompletion { success: Some(false) } => ActivityStateV1::Failed,
        CorrelationState::Completed { .. }
        | CorrelationState::Resolved { .. }
        | CorrelationState::OrphanCompletion { .. }
        | CorrelationState::OrphanResolution { .. } => ActivityStateV1::Completed,
        CorrelationState::UnknownAfterGap => ActivityStateV1::UnknownAfterGap,
    };
    ActivityFactV1 { class, state, evidence: observation_evidence(value.evidence) }
}

fn interaction_fact(value: &CorrelationProjection) -> InteractionFactV1 {
    let state = match value.state {
        CorrelationState::Pending => InteractionStateV1::Required,
        CorrelationState::UnknownAfterGap => InteractionStateV1::UnknownAfterGap,
        CorrelationState::Resolved {
            outcome: ObservationInteractionOutcomeV1::Denied
                | ObservationInteractionOutcomeV1::Interrupted
                | ObservationInteractionOutcomeV1::Superseded,
        } | CorrelationState::OrphanResolution {
            outcome: ObservationInteractionOutcomeV1::Denied
                | ObservationInteractionOutcomeV1::Interrupted
                | ObservationInteractionOutcomeV1::Superseded,
        } => InteractionStateV1::Dismissed,
        _ => InteractionStateV1::Responded,
    };
    InteractionFactV1 {
        class: InteractionClassV1::Attention,
        state,
        evidence: observation_evidence(value.evidence),
    }
}

fn active_count(values: &[CorrelationProjection]) -> u16 {
    saturating_u16(values.iter().filter(|value| {
        matches!(value.state, CorrelationState::Pending)
    }).count())
}

fn saturating_u16(value: usize) -> u16 {
    value.min(usize::from(u16::MAX)) as u16
}

fn timeline_category(kind: &ObservationKindV1) -> TimelineCategoryV1 {
    match kind {
        ObservationKindV1::ToolStarted { .. } | ObservationKindV1::ToolCompleted { .. } => {
            TimelineCategoryV1::Tool
        }
        ObservationKindV1::ApprovalRequested { .. }
        | ObservationKindV1::QuestionRequested { .. }
        | ObservationKindV1::ApprovalResolved { .. }
        | ObservationKindV1::QuestionResolved { .. }
        | ObservationKindV1::InteractionResolved { .. } => TimelineCategoryV1::Interaction,
        ObservationKindV1::SubagentStarted { .. }
        | ObservationKindV1::SubagentProgress { .. }
        | ObservationKindV1::SubagentCompleted { .. } => TimelineCategoryV1::Subagent,
        ObservationKindV1::TodoSnapshot { .. } => TimelineCategoryV1::Todo,
        ObservationKindV1::Usage { .. }
        | ObservationKindV1::ContextWindowUsage { .. } => TimelineCategoryV1::Usage,
        ObservationKindV1::OwnedProcessStarted { .. }
        | ObservationKindV1::OwnedProcessExited { .. } => TimelineCategoryV1::Process,
        ObservationKindV1::FileChanged { .. } => TimelineCategoryV1::File,
        ObservationKindV1::HistorySnapshot { .. } => TimelineCategoryV1::History,
        _ => TimelineCategoryV1::Lifecycle,
    }
}

fn observation_evidence(evidence: SourceEvidenceV1) -> ObservationEvidenceV1 {
    match evidence {
        SourceEvidenceV1::StructuredProvider => ObservationEvidenceV1::StructuredProvider,
        SourceEvidenceV1::ManagedHook => ObservationEvidenceV1::ManagedHook,
        SourceEvidenceV1::NodeLifecycle => ObservationEvidenceV1::NodeLifecycle,
        SourceEvidenceV1::WorkspaceObservation => ObservationEvidenceV1::WorkspaceObservation,
        SourceEvidenceV1::HistoryProjection => ObservationEvidenceV1::History,
        SourceEvidenceV1::PtyHint => ObservationEvidenceV1::PtyHint,
    }
}

fn monitor_feature_states(
    projection: Option<&SessionProjection>,
    route_support: Option<Option<gate4agent_c2_protocol::C2ObservationSupport>>,
) -> MonitorFeatureStatesV1 {
    let Some(projection) = projection else {
        let state = match route_support {
            Some(None) => FeatureObservationStateV1::NotSupportedByObservedSources,
            Some(Some(support)) if !support.events
                || !support.managed_target
                || !support.workflow_detail => {
                    FeatureObservationStateV1::NotSupportedByObservedSources
                }
            _ => FeatureObservationStateV1::Unknown,
        };
        return MonitorFeatureStatesV1 {
            todo: state,
            tools: state,
            subagents: state,
            interactions: state,
            owned_processes: state,
            files: state,
            usage: state,
            history: state,
        };
    };
    let usage = &projection.usage;
    let usage_observed = usage.last_cumulative.is_some()
        || usage.context_window.is_some()
        || usage.observed_delta.input_tokens != 0
        || usage.observed_delta.output_tokens != 0
        || usage.observed_delta.cache_read_tokens != 0
        || usage.observed_delta.cache_write_tokens != 0
        || usage.observed_delta.reasoning_tokens != 0;
    MonitorFeatureStatesV1 {
        todo: feature_state(projection, projection.todos.current.is_some(), |capabilities| {
            capabilities.todo
        }),
        tools: feature_state(projection, !projection.tools.is_empty(), |capabilities| {
            capabilities.tools
        }),
        subagents: feature_state(projection, !projection.subagents.is_empty(), |capabilities| {
            capabilities.subagents
        }),
        interactions: feature_state(projection, !projection.interactions.is_empty(), |capabilities| {
            capabilities.attention
        }),
        owned_processes: feature_state(
            projection,
            !projection.owned_processes.is_empty(),
            |capabilities| capabilities.owned_processes,
        ),
        files: feature_state(projection, !projection.files.is_empty(), |capabilities| {
            capabilities.file_changes
        }),
        usage: feature_state(projection, usage_observed, |capabilities| capabilities.usage),
        history: feature_state(projection, projection.history.is_some(), |capabilities| {
            capabilities.history_summary
        }),
    }
}

fn route_support_for_run(
    support: &ObservationSupportRegistry,
    run: &gate4agent_harness_protocol::HarnessRunV1,
) -> Option<Option<gate4agent_c2_protocol::C2ObservationSupport>> {
    let binding = run.binding.as_ref()?;
    let node_id = gate4agent_observation_api::NodeId::new(binding.node_id.as_str()).ok()?;
    let incarnation_id = binding.node_incarnation.as_str().parse().ok()?;
    support.get(&node_id, incarnation_id)
}

fn route_authoritative_for_run(
    support: &ObservationSupportRegistry,
    run: &gate4agent_harness_protocol::HarnessRunV1,
) -> bool {
    let Some(binding) = run.binding.as_ref() else { return false; };
    let Ok(node_id) = gate4agent_observation_api::NodeId::new(binding.node_id.as_str()) else {
        return false;
    };
    let Ok(incarnation_id) = binding.node_incarnation.as_str().parse() else {
        return false;
    };
    support.is_authoritative(&node_id, incarnation_id)
}

fn feature_state(
    projection: &SessionProjection,
    observed: bool,
    supported: impl Fn(&gate4agent_observation_protocol::ObservationCapabilitiesV1) -> bool,
) -> FeatureObservationStateV1 {
    if observed {
        return FeatureObservationStateV1::Observed;
    }
    if projection.source_capabilities.is_empty() {
        return FeatureObservationStateV1::Unknown;
    }
    if projection.source_capabilities.iter().any(|entry| supported(&entry.capabilities)) {
        FeatureObservationStateV1::SupportedNotObserved
    } else {
        FeatureObservationStateV1::NotSupportedByObservedSources
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_protocol::*;
    use std::{fs, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

    fn selector(value: &str) -> HarnessSelectorV1 {
        HarnessSelectorV1::new(value).unwrap()
    }

    fn grant() -> SessionGrantV1 {
        SessionGrantV1 {
            grant_id: SessionGrantId::new(format!("hgrant_{}", "a".repeat(24))).unwrap(),
            revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap(),
            allowed_targets: vec![HarnessGrantTargetV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                provider_profile: selector("profile-a"),
                mode: HarnessExecutionModeV1::Pty,
            }],
            allowed_delivery_bundles: Vec::new(),
            maximum_child_count: 0,
            maximum_child_depth: 0,
            operation_timeouts: HarnessOperationTimeoutsV1 {
                dispatch_ms: 1,
                wait_ms: 1,
                reconciliation_ms: 1,
            },
            task_permissions: HarnessTaskPermissionsV1 {
                read: false,
                create: false,
                mutate: false,
                request_run: false,
            },
            read_permissions: HarnessReadPermissionsV1::default(),
            monitoring_visibility: HarnessMonitoringVisibilityV1::None,
            context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
            state: SessionGrantStateV1::Active,
            created_at_unix_ms: 1,
            updated_at_unix_ms: 1,
        }
    }

    #[test]
    fn raw_host_read_scope_none_is_denied_not_empty() {
        let grant = grant();
        let task_id = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();
        let run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let operation_id = HarnessOperationId::new(format!("hop_{}", "a".repeat(24))).unwrap();
        for request in [
            HarnessReadRequestV1::TasksList {
                after_task_id: None,
                state: None,
                limit: 1,
            },
            HarnessReadRequestV1::TaskGet { task_id },
            HarnessReadRequestV1::RunsList {
                task_id: None,
                after_run_id: None,
                lifecycle: None,
                limit: 1,
            },
            HarnessReadRequestV1::RunGet { run_id },
            HarnessReadRequestV1::OperationGet { operation_id },
        ] {
            assert_eq!(
                authorize_request(&grant, &request),
                Err(HarnessReadHostErrorV1::NotFoundOrDenied),
            );
        }
        assert_eq!(authorize_request(&grant, &HarnessReadRequestV1::ContextGet), Ok(()));
        assert!(allowed_tool_ids(&grant).contains(&"g4a_context_get".to_owned()));
    }

    #[test]
    fn operator_monitor_and_timeline_are_bounded_redacted_without_weakening_agent_denial() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-operator-observation-{}-{nonce}.sqlite",
            std::process::id(),
        ));
        let harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let observation = ObservationService::open(&path).unwrap();
        let support = ObservationSupportRegistry::default();
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let monitor = execute_operator_monitor(
            &harness,
            &observation,
            &support,
            &run_id,
        ).unwrap();
        assert_eq!(monitor.visibility, HarnessMonitoringVisibilityV1::Timeline);
        monitor.validate().unwrap();
        let timeline = execute_operator_timeline(
            &harness,
            &observation,
            &support,
            &run_id,
            None,
            HARNESS_TIMELINE_PAGE_LIMIT_MAX,
        ).unwrap();
        assert!(timeline.entries.len() <= usize::from(HARNESS_TIMELINE_PAGE_LIMIT_MAX));
        timeline.validate().unwrap();

        let denied_grant = grant();
        assert_eq!(
            authorize_request(
                &denied_grant,
                &HarnessReadRequestV1::RunGet { run_id },
            ),
            Err(HarnessReadHostErrorV1::NotFoundOrDenied),
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
}
