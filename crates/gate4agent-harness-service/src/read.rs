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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationAudience {
    Operator,
    GrantBound,
}

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
        ObservationAudience::Operator,
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
        ObservationAudience::Operator,
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
                ObservationAudience::GrantBound,
            )
        }
        HarnessReadRequestV1::TimelineRead { run_id, after_sequence, limit } => {
            if grant.monitoring_visibility != HarnessMonitoringVisibilityV1::Timeline {
                return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
            }
            let run_id = authorized_monitor_run(engine, grant, binding, &visibility, run_id)?;
            timeline(
                engine,
                observation,
                support,
                &run_id,
                after_sequence,
                limit,
                ObservationAudience::GrantBound,
            )
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
    audience: ObservationAudience,
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
    let detail = detail_allowed.then(|| {
        monitor_detail(projection.expect("detail requires projection"), audience)
    });
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
        history: (audience == ObservationAudience::Operator).then(|| {
            projection.and_then(|projection| projection.history.as_ref()).map(|history| {
                SessionMonitorHistoryV1 {
                    message_count: history.message_count,
                    message_count_exact: history.message_count_exact,
                    completed_turn_count: history.completed_turn_count,
                    total_tokens: history.total_tokens,
                }
            })
        }).flatten(),
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
    audience: ObservationAudience,
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
        .map(|entry| {
            timeline_entry(
                projection.expect("timeline entry requires projection"),
                entry,
                audience,
            )
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
                HarnessWorktreeIntentV1::Managed { .. }
                | HarnessWorktreeIntentV1::ManagedProfile { .. } => {
                    RedactedWorktreeIntentV1::Managed
                }
            },
            has_delivery_bundle: run.intent.delivery_bundle.is_some(),
            has_continuation: run.intent.continuation.is_some(),
        },
        lifecycle: run.lifecycle,
        binding,
        result_disposition: run.result_disposition,
        failure_category: run.failure.as_ref().map(|failure| failure.category),
        context_pack: run.context_pack.clone(),
        git_facts: run.git_facts.clone(),
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

fn monitor_detail(
    projection: &SessionProjection,
    audience: ObservationAudience,
) -> SessionMonitorDetailV1 {
    let structured = audience == ObservationAudience::Operator;
    SessionMonitorDetailV1 {
        todo_facts: projection.todos.current.as_ref().map(|todo| todo.items.iter().map(|item| {
            TodoFactV1 {
                state: match item.state {
                    ObservationTodoStateV1::Pending => TodoStateV1::Pending,
                    ObservationTodoStateV1::InProgress => TodoStateV1::InProgress,
                    ObservationTodoStateV1::Completed => TodoStateV1::Completed,
                    ObservationTodoStateV1::Unknown => TodoStateV1::Unknown,
                },
                todo_id: structured.then(|| item.id.clone()).flatten(),
                label: structured.then(|| item.text.clone()),
                evidence: observation_evidence(todo.evidence),
            }
        }).take(HARNESS_MONITOR_FACTS_MAX).collect()).unwrap_or_default(),
        tool_facts: projection.tools.iter().enumerate()
            .map(|(index, value)| {
                activity_fact(value, ActivityClassV1::Tool, index, structured)
            })
            .take(HARNESS_MONITOR_FACTS_MAX).collect(),
        subagent_facts: projection.subagents.iter().enumerate()
            .map(|(index, value)| {
                activity_fact(value, ActivityClassV1::Subagent, index, structured)
            })
            .take(HARNESS_MONITOR_FACTS_MAX).collect(),
        interaction_facts: projection.interactions.iter().enumerate()
            .map(|(index, value)| interaction_fact(value, index, structured))
            .take(HARNESS_MONITOR_FACTS_MAX).collect(),
        process_facts: projection.owned_processes.iter().enumerate()
            .map(|(index, value)| {
                activity_fact(value, ActivityClassV1::OwnedProcess, index, structured)
            })
            .take(HARNESS_MONITOR_FACTS_MAX).collect(),
        file_facts: projection.files.iter().map(|value| FileFactV1 {
            action: FileActionV1::Changed,
            relative_path: structured.then(|| value.path.clone()).flatten(),
            evidence: observation_evidence(value.evidence),
        }).take(HARNESS_MONITOR_FACTS_MAX).collect(),
    }
}

fn activity_fact(
    value: &CorrelationProjection,
    class: ActivityClassV1,
    index: usize,
    structured: bool,
) -> ActivityFactV1 {
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
    ActivityFactV1 {
        class,
        state,
        label: structured.then(|| value.class.clone()).flatten(),
        correlation: structured.then(|| u16::try_from(index + 1).ok()).flatten(),
        evidence: observation_evidence(value.evidence),
    }
}

fn interaction_fact(
    value: &CorrelationProjection,
    index: usize,
    structured: bool,
) -> InteractionFactV1 {
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
        label: structured.then(|| value.class.clone()).flatten(),
        correlation: structured.then(|| u16::try_from(index + 1).ok()).flatten(),
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

fn timeline_entry(
    projection: &SessionProjection,
    entry: &gate4agent_observation_engine::TimelineEntry,
    audience: ObservationAudience,
) -> TimelineEntryV1 {
    if audience == ObservationAudience::GrantBound {
        return TimelineEntryV1 {
            sequence: entry.cursor.sequence,
            received_at_ms: entry.received_at_ms,
            category: timeline_category(&entry.kind),
            label: None,
            state: TimelineStateV1::Unknown,
            correlation: None,
            evidence: observation_evidence(entry.evidence),
        };
    }
    let (label, state, correlation) = match &entry.kind {
        ObservationKindV1::SourceCapabilities { .. } => {
            (Some("source-capabilities".to_owned()), TimelineStateV1::Updated, None)
        }
        ObservationKindV1::SessionStarted => {
            (Some("session".to_owned()), TimelineStateV1::Started, None)
        }
        ObservationKindV1::Ready => {
            (Some("session".to_owned()), TimelineStateV1::Active, None)
        }
        ObservationKindV1::Stopped => {
            (Some("session".to_owned()), TimelineStateV1::Completed, None)
        }
        ObservationKindV1::Exited { success: Some(false) } => {
            (Some("session".to_owned()), TimelineStateV1::Failed, None)
        }
        ObservationKindV1::Exited { .. } => {
            (Some("session".to_owned()), TimelineStateV1::Completed, None)
        }
        ObservationKindV1::TurnStarted => {
            (Some("turn".to_owned()), TimelineStateV1::Started, None)
        }
        ObservationKindV1::Working => {
            (Some("turn".to_owned()), TimelineStateV1::Active, None)
        }
        ObservationKindV1::TurnCompleted => {
            (Some("turn".to_owned()), TimelineStateV1::Completed, None)
        }
        ObservationKindV1::TurnInterrupted => {
            (Some("turn".to_owned()), TimelineStateV1::Interrupted, None)
        }
        ObservationKindV1::ToolStarted { correlation_id, class } => (
            Some(class.clone()),
            TimelineStateV1::Started,
            correlation_ordinal(&projection.tools, correlation_id),
        ),
        ObservationKindV1::ToolCompleted { correlation_id, class, success, .. } => (
            correlation_label(&projection.tools, correlation_id)
                .or_else(|| Some(class.clone())),
            if *success { TimelineStateV1::Completed } else { TimelineStateV1::Failed },
            correlation_ordinal(&projection.tools, correlation_id),
        ),
        ObservationKindV1::ApprovalRequested { correlation_id, tool_class }
        | ObservationKindV1::QuestionRequested { correlation_id, tool_class } => (
            Some(tool_class.clone()),
            TimelineStateV1::Required,
            correlation_ordinal(&projection.interactions, correlation_id),
        ),
        ObservationKindV1::ApprovalResolved { correlation_id, outcome }
        | ObservationKindV1::QuestionResolved { correlation_id, outcome }
        | ObservationKindV1::InteractionResolved { correlation_id, outcome } => (
            correlation_label(&projection.interactions, correlation_id),
            timeline_interaction_state(*outcome),
            correlation_ordinal(&projection.interactions, correlation_id),
        ),
        ObservationKindV1::SubagentStarted { correlation_id, class } => (
            Some(class.clone()),
            TimelineStateV1::Started,
            correlation_ordinal(&projection.subagents, correlation_id),
        ),
        ObservationKindV1::SubagentProgress { correlation_id } => (
            correlation_label(&projection.subagents, correlation_id),
            TimelineStateV1::Active,
            correlation_ordinal(&projection.subagents, correlation_id),
        ),
        ObservationKindV1::SubagentCompleted { correlation_id, success } => (
            correlation_label(&projection.subagents, correlation_id),
            if *success == Some(false) {
                TimelineStateV1::Failed
            } else {
                TimelineStateV1::Completed
            },
            correlation_ordinal(&projection.subagents, correlation_id),
        ),
        ObservationKindV1::TodoSnapshot { .. } => {
            (Some("todo-snapshot".to_owned()), TimelineStateV1::Updated, None)
        }
        ObservationKindV1::Usage { .. } => {
            (Some("token-usage".to_owned()), TimelineStateV1::Updated, None)
        }
        ObservationKindV1::ContextWindowUsage { .. } => {
            (Some("context-window-usage".to_owned()), TimelineStateV1::Updated, None)
        }
        ObservationKindV1::RateLimited => {
            (Some("rate-limit".to_owned()), TimelineStateV1::Waiting, None)
        }
        ObservationKindV1::OwnedProcessStarted { correlation_id, class } => (
            Some(class.clone()),
            TimelineStateV1::Started,
            correlation_ordinal(&projection.owned_processes, correlation_id),
        ),
        ObservationKindV1::OwnedProcessExited { correlation_id, success, .. } => (
            correlation_label(&projection.owned_processes, correlation_id),
            if *success == Some(false) {
                TimelineStateV1::Failed
            } else {
                TimelineStateV1::Completed
            },
            correlation_ordinal(&projection.owned_processes, correlation_id),
        ),
        ObservationKindV1::FileChanged { path } => {
            (path.clone(), TimelineStateV1::Changed, None)
        }
        ObservationKindV1::HistorySnapshot { .. } => {
            (Some("history".to_owned()), TimelineStateV1::Updated, None)
        }
        ObservationKindV1::Gap { .. } | ObservationKindV1::SourceReset => {
            (Some("observation-gap".to_owned()), TimelineStateV1::UnknownAfterGap, None)
        }
        ObservationKindV1::Stale => {
            (Some("observation".to_owned()), TimelineStateV1::Stale, None)
        }
        ObservationKindV1::Error { .. } => {
            (Some("observation-error".to_owned()), TimelineStateV1::Failed, None)
        }
    };
    TimelineEntryV1 {
        sequence: entry.cursor.sequence,
        received_at_ms: entry.received_at_ms,
        category: timeline_category(&entry.kind),
        label,
        state,
        correlation,
        evidence: observation_evidence(entry.evidence),
    }
}

fn correlation_ordinal(values: &[CorrelationProjection], correlation_id: &str) -> Option<u16> {
    values.iter().position(|value| value.correlation_id == correlation_id)
        .and_then(|index| u16::try_from(index + 1).ok())
}

fn correlation_label(values: &[CorrelationProjection], correlation_id: &str) -> Option<String> {
    values.iter().find(|value| value.correlation_id == correlation_id)
        .and_then(|value| value.class.clone())
}

fn timeline_interaction_state(
    outcome: ObservationInteractionOutcomeV1,
) -> TimelineStateV1 {
    match outcome {
        ObservationInteractionOutcomeV1::Approved
        | ObservationInteractionOutcomeV1::Answered
        | ObservationInteractionOutcomeV1::TurnEnded => TimelineStateV1::Completed,
        ObservationInteractionOutcomeV1::Denied
        | ObservationInteractionOutcomeV1::Superseded => TimelineStateV1::Dismissed,
        ObservationInteractionOutcomeV1::Interrupted => TimelineStateV1::Interrupted,
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
    use gate4agent_observation_api::{
        NodeCursor, NodeId, NodeIncarnationId, ObservationIngressEnvelope,
        ObservationIngressPayload, ObservationTransport, SessionRecordId,
    };
    use gate4agent_observation_protocol::{
        ObservationCapabilitiesV1, ObservationSourceFamilyV1, ObservationTodoItemV1,
        ObservationV1,
    };
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

    fn observation_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!(
            "gate4agent-read-{label}-{}-{nonce}.sqlite",
            std::process::id(),
        ))
    }

    fn managed_target(record_id: &str) -> ObservationTarget {
        ObservationTarget::Managed {
            key: ManagedSessionKey {
                node_id: NodeId::new("node-a").unwrap(),
                incarnation_id: NodeIncarnationId::from_bytes([4; 16]),
                record_id: SessionRecordId::new(record_id).unwrap(),
            },
        }
    }

    fn apply_observation(
        service: &mut ObservationService,
        record_id: &str,
        sequence: u64,
        evidence: SourceEvidenceV1,
        kind: ObservationKindV1,
    ) {
        service.apply_ingress(ObservationIngressEnvelope {
            node_id: NodeId::new("node-a").unwrap(),
            cursor: NodeCursor {
                incarnation_id: NodeIncarnationId::from_bytes([4; 16]),
                sequence,
            },
            received_at_ms: 1_000 + sequence,
            transport: ObservationTransport::C2,
            payload: ObservationIngressPayload::Observation {
                address: managed_target(record_id),
                observation: ObservationV1 {
                    source_sequence: sequence,
                    observed_at_unix_ms: Some(2_000 + sequence),
                    evidence,
                    kind,
                    truncated: false,
                },
            },
        }).unwrap();
    }

    fn all_capabilities() -> ObservationCapabilitiesV1 {
        ObservationCapabilitiesV1 {
            tools: true,
            attention: true,
            subagents: true,
            todo: true,
            usage: true,
            owned_processes: true,
            file_changes: true,
            history_summary: true,
        }
    }

    fn close_observation(service: ObservationService, path: &PathBuf) {
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
    fn operator_progress_projects_tool_subagent_and_usage_states() {
        let path = observation_path("structured-progress");
        let harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let mut observation = ObservationService::open(&path).unwrap();
        apply_observation(
            &mut observation,
            "record-a",
            1,
            SourceEvidenceV1::ManagedHook,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::ManagedHook,
                source_adapter: "provider-adapter-private".to_owned(),
                capabilities: all_capabilities(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            2,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ToolStarted {
                correlation_id: "tool-private-id".to_owned(),
                class: "Shell".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            3,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ToolCompleted {
                correlation_id: "tool-private-id".to_owned(),
                class: "Tool".to_owned(),
                success: true,
                duration_ms: Some(12),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            4,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::SubagentStarted {
                correlation_id: "subagent-private-id".to_owned(),
                class: "research".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            5,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::SubagentProgress {
                correlation_id: "subagent-private-id".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            6,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::SubagentCompleted {
                correlation_id: "subagent-private-id".to_owned(),
                success: Some(false),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            7,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::Usage {
                input_tokens: 20,
                output_tokens: 8,
                cache_read_tokens: 3,
                cache_write_tokens: 2,
                reasoning_tokens: 5,
                context_window: Some(128_000),
                is_cumulative: false,
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            8,
            SourceEvidenceV1::ManagedHook,
            ObservationKindV1::TodoSnapshot {
                revision: 1,
                items: vec![ObservationTodoItemV1 {
                    id: Some("todo-private-id".to_owned()),
                    text: "verify operator projection".to_owned(),
                    state: ObservationTodoStateV1::InProgress,
                }],
                complete: true,
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            9,
            SourceEvidenceV1::WorkspaceObservation,
            ObservationKindV1::FileChanged {
                path: Some("src/operator.rs".to_owned()),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            10,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ApprovalRequested {
                correlation_id: "approval-private-id".to_owned(),
                tool_class: "Shell".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            11,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ApprovalResolved {
                correlation_id: "approval-private-id".to_owned(),
                outcome: ObservationInteractionOutcomeV1::Approved,
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            12,
            SourceEvidenceV1::HistoryProjection,
            ObservationKindV1::HistorySnapshot {
                message_count: 42,
                message_count_exact: true,
                completed_turn_count: Some(7),
                total_tokens: Some(900),
            },
        );

        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let support = ObservationSupportRegistry::default();
        let monitor = execute_operator_monitor(&harness, &observation, &support, &run_id)
            .unwrap();
        let detail = monitor.detail.as_ref().unwrap();
        assert_eq!(monitor.features.tools, FeatureObservationStateV1::Observed);
        assert_eq!(monitor.features.subagents, FeatureObservationStateV1::Observed);
        assert_eq!(monitor.features.usage, FeatureObservationStateV1::Observed);
        assert_eq!(monitor.features.history, FeatureObservationStateV1::Observed);
        assert_eq!(monitor.input_tokens, 20);
        assert_eq!(monitor.output_tokens, 8);
        assert_eq!(
            monitor.history,
            Some(SessionMonitorHistoryV1 {
                message_count: 42,
                message_count_exact: true,
                completed_turn_count: Some(7),
                total_tokens: Some(900),
            }),
        );
        assert_eq!(detail.tool_facts[0].label.as_deref(), Some("Shell"));
        assert_eq!(detail.tool_facts[0].state, ActivityStateV1::Completed);
        assert_eq!(detail.tool_facts[0].correlation, Some(1));
        assert_eq!(detail.subagent_facts[0].label.as_deref(), Some("research"));
        assert_eq!(detail.subagent_facts[0].state, ActivityStateV1::Failed);
        assert_eq!(detail.subagent_facts[0].correlation, Some(1));
        assert_eq!(detail.interaction_facts[0].label.as_deref(), Some("Shell"));
        assert_eq!(detail.interaction_facts[0].state, InteractionStateV1::Responded);
        assert_eq!(detail.interaction_facts[0].correlation, Some(1));
        assert_eq!(detail.todo_facts[0].label.as_deref(), Some("verify operator projection"));
        assert_eq!(detail.todo_facts[0].todo_id.as_deref(), Some("todo-private-id"));
        assert_eq!(detail.file_facts[0].relative_path.as_deref(), Some("src/operator.rs"));

        let timeline = execute_operator_timeline(
            &harness,
            &observation,
            &support,
            &run_id,
            None,
            HARNESS_TIMELINE_PAGE_LIMIT_MAX,
        ).unwrap();
        let by_sequence = |sequence| {
            timeline.entries.iter().find(|entry| entry.sequence == sequence).unwrap()
        };
        assert_eq!(by_sequence(2).state, TimelineStateV1::Started);
        assert_eq!(by_sequence(2).label.as_deref(), Some("Shell"));
        assert_eq!(by_sequence(2).correlation, Some(1));
        assert_eq!(by_sequence(3).state, TimelineStateV1::Completed);
        assert_eq!(by_sequence(3).label.as_deref(), Some("Shell"));
        assert_eq!(by_sequence(4).state, TimelineStateV1::Started);
        assert_eq!(by_sequence(5).state, TimelineStateV1::Active);
        assert_eq!(by_sequence(6).state, TimelineStateV1::Failed);
        assert_eq!(by_sequence(7).state, TimelineStateV1::Updated);
        assert_eq!(by_sequence(7).category, TimelineCategoryV1::Usage);
        assert_eq!(by_sequence(10).state, TimelineStateV1::Required);
        assert_eq!(by_sequence(10).correlation, Some(1));
        assert_eq!(by_sequence(11).state, TimelineStateV1::Completed);
        assert_eq!(by_sequence(12).category, TimelineCategoryV1::History);
        assert_eq!(by_sequence(12).state, TimelineStateV1::Updated);

        monitor.validate_for(&run_id).unwrap();
        timeline.validate_for(&run_id).unwrap();
        close_observation(observation, &path);
    }

    #[test]
    fn grant_bound_monitor_and_timeline_keep_structured_operator_projection_private() {
        let path = observation_path("grant-bound-redaction");
        let mut checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();
        checkpoint.grants[0].monitoring_visibility = HarnessMonitoringVisibilityV1::Timeline;
        let harness = HarnessService::from_engine_for_test(
            gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap(),
        );
        let mut observation = ObservationService::open(&path).unwrap();
        apply_observation(
            &mut observation,
            "record-a",
            1,
            SourceEvidenceV1::ManagedHook,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::ManagedHook,
                source_adapter: "private-source-adapter".to_owned(),
                capabilities: all_capabilities(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            2,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ToolStarted {
                correlation_id: "private-tool-correlation".to_owned(),
                class: "Shell".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            3,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::SubagentStarted {
                correlation_id: "private-subagent-correlation".to_owned(),
                class: "research".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            4,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ApprovalRequested {
                correlation_id: "private-approval-correlation".to_owned(),
                tool_class: "Shell".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            5,
            SourceEvidenceV1::ManagedHook,
            ObservationKindV1::TodoSnapshot {
                revision: 1,
                items: vec![ObservationTodoItemV1 {
                    id: Some("private-todo-id".to_owned()),
                    text: "private todo text".to_owned(),
                    state: ObservationTodoStateV1::InProgress,
                }],
                complete: true,
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            6,
            SourceEvidenceV1::WorkspaceObservation,
            ObservationKindV1::FileChanged {
                path: Some("private/file.rs".to_owned()),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            7,
            SourceEvidenceV1::HistoryProjection,
            ObservationKindV1::HistorySnapshot {
                message_count: 77,
                message_count_exact: true,
                completed_turn_count: Some(9),
                total_tokens: Some(1_234),
            },
        );

        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let binding = crate::credential::tests::binding(1, 1);
        let support = ObservationSupportRegistry::default();
        let HarnessReadResponseV1::Monitor(monitor) = execute_exact_binding_read(
            &harness,
            &observation,
            &support,
            &binding,
            HarnessReadRequestV1::MonitorGet { run_id: Some(run_id.clone()) },
        ).unwrap() else {
            panic!("monitor response");
        };
        let HarnessReadResponseV1::Timeline(timeline) = execute_exact_binding_read(
            &harness,
            &observation,
            &support,
            &binding,
            HarnessReadRequestV1::TimelineRead {
                run_id: Some(run_id.clone()),
                after_sequence: None,
                limit: HARNESS_TIMELINE_PAGE_LIMIT_MAX,
            },
        ).unwrap() else {
            panic!("timeline response");
        };

        let detail = monitor.detail.as_ref().unwrap();
        assert_eq!(monitor.features.history, FeatureObservationStateV1::Observed);
        assert!(monitor.history.is_none());
        assert!(detail.todo_facts.iter().all(|fact| {
            fact.todo_id.is_none() && fact.label.is_none()
        }));
        assert!(detail.tool_facts.iter().chain(&detail.subagent_facts).all(|fact| {
            fact.label.is_none() && fact.correlation.is_none()
        }));
        assert!(detail.interaction_facts.iter().all(|fact| {
            fact.label.is_none() && fact.correlation.is_none()
        }));
        assert!(detail.file_facts.iter().all(|fact| fact.relative_path.is_none()));
        assert!(timeline.entries.iter().all(|entry| {
            entry.label.is_none()
                && entry.state == TimelineStateV1::Unknown
                && entry.correlation.is_none()
        }));
        monitor.validate_for(&run_id).unwrap();
        timeline.validate_for(&run_id).unwrap();

        let encoded = format!(
            "{}\n{}",
            serde_json::to_string(&monitor).unwrap(),
            serde_json::to_string(&timeline).unwrap(),
        );
        for forbidden in [
            "private-source-adapter",
            "private-tool-correlation",
            "private-subagent-correlation",
            "private-approval-correlation",
            "private-todo-id",
            "private todo text",
            "private/file.rs",
            "Shell",
            "research",
            "\"message_count\":77",
            "correlation_id",
        ] {
            assert!(!encoded.contains(forbidden), "grant-bound serialized {forbidden}");
        }
        close_observation(observation, &path);
    }

    #[test]
    fn operator_progress_isolated_by_exact_run_binding() {
        let path = observation_path("run-isolation");
        let harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let mut observation = ObservationService::open(&path).unwrap();
        apply_observation(
            &mut observation,
            "record-b",
            1,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ToolStarted {
                correlation_id: "other-run-tool".to_owned(),
                class: "foreign".to_owned(),
            },
        );
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let monitor = execute_operator_monitor(
            &harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &run_id,
        ).unwrap();
        assert_eq!(monitor.availability, ProjectionAvailabilityV1::Unknown);
        assert_eq!(monitor.active_tools, 0);
        assert!(monitor.detail.is_none());
        close_observation(observation, &path);
    }

    #[test]
    fn operator_timeline_preserves_order_and_128_bound() {
        let path = observation_path("timeline-bound");
        let harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let mut observation = ObservationService::open(&path).unwrap();
        for sequence in 1..=140 {
            apply_observation(
                &mut observation,
                "record-a",
                sequence,
                SourceEvidenceV1::NodeLifecycle,
                ObservationKindV1::Working,
            );
        }
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let page = execute_operator_timeline(
            &harness,
            &observation,
            &ObservationSupportRegistry::default(),
            &run_id,
            None,
            HARNESS_TIMELINE_PAGE_LIMIT_MAX,
        ).unwrap();
        assert_eq!(page.entries.len(), usize::from(HARNESS_TIMELINE_PAGE_LIMIT_MAX));
        assert_eq!(page.entries.first().unwrap().sequence, 1);
        assert_eq!(page.entries.last().unwrap().sequence, 128);
        assert_eq!(page.next_cursor, Some(128));
        assert!(page.entries.windows(2).all(|pair| {
            pair[0].sequence < pair[1].sequence
        }));
        page.validate_for(&run_id).unwrap();
        close_observation(observation, &path);
    }

    #[test]
    fn operator_todo_and_files_are_categorical_when_unsupported() {
        let path = observation_path("categorical-unsupported");
        let mut observation = ObservationService::open(&path).unwrap();
        let mut capabilities = all_capabilities();
        capabilities.todo = false;
        capabilities.file_changes = false;
        apply_observation(
            &mut observation,
            "record-a",
            1,
            SourceEvidenceV1::ManagedHook,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::ManagedHook,
                source_adapter: "bounded-adapter".to_owned(),
                capabilities,
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            2,
            SourceEvidenceV1::NodeLifecycle,
            ObservationKindV1::Stale,
        );
        let projection = observation.projection(&managed_target("record-a")).unwrap();
        let features = monitor_feature_states(Some(projection), None);
        assert_eq!(features.todo, FeatureObservationStateV1::NotSupportedByObservedSources);
        assert_eq!(features.files, FeatureObservationStateV1::NotSupportedByObservedSources);
        assert_eq!(features.tools, FeatureObservationStateV1::SupportedNotObserved);
        assert_eq!(
            observation_state(Some(projection), true).1,
            ProjectionFreshnessV1::Stale,
        );
        let detail = monitor_detail(projection, ObservationAudience::Operator);
        assert!(detail.todo_facts.is_empty());
        assert!(detail.file_facts.is_empty());
        close_observation(observation, &path);
    }

    #[test]
    fn operator_progress_serialization_excludes_private_observation_payloads() {
        let path = observation_path("serialization-privacy");
        let harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let mut observation = ObservationService::open(&path).unwrap();
        apply_observation(
            &mut observation,
            "record-a",
            1,
            SourceEvidenceV1::ManagedHook,
            ObservationKindV1::SourceCapabilities {
                source_family: ObservationSourceFamilyV1::ManagedHook,
                source_adapter: "private-provider-adapter".to_owned(),
                capabilities: all_capabilities(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            2,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ToolStarted {
                correlation_id: "private-provider-correlation".to_owned(),
                class: "command".to_owned(),
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            3,
            SourceEvidenceV1::ManagedHook,
            ObservationKindV1::Error {
                detail: "raw-output-credential-transcript".to_owned(),
            },
        );
        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let support = ObservationSupportRegistry::default();
        let monitor = execute_operator_monitor(&harness, &observation, &support, &run_id)
            .unwrap();
        let timeline = execute_operator_timeline(
            &harness,
            &observation,
            &support,
            &run_id,
            None,
            HARNESS_TIMELINE_PAGE_LIMIT_MAX,
        ).unwrap();
        let encoded = format!(
            "{}\n{}",
            serde_json::to_string(&monitor).unwrap(),
            serde_json::to_string(&timeline).unwrap(),
        );
        for forbidden in [
            "private-provider-adapter",
            "private-provider-correlation",
            "raw-output",
            "credential-transcript",
            "correlation_id",
        ] {
            assert!(!encoded.contains(forbidden), "serialized {forbidden}");
        }
        assert!(encoded.contains("command"));
        assert!(encoded.contains("\"correlation\":1"));
        close_observation(observation, &path);
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
