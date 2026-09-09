use crate::{
    credential::{CredentialBindingV1, VerifiedCredentialV1},
    runtime::ObservationSupportRegistry,
    HarnessService,
};
use gate4agent_harness_api::*;
use gate4agent_harness_engine::{
    HarnessEngineError, HarnessMailFetchResolutionV1, HarnessReadVisibilityV1,
};
use gate4agent_harness_protocol::{
    HarnessActorV1, HarnessEntityReadScopeV1, HarnessMonitoringVisibilityV1,
    HarnessSessionIdentityV1, HarnessWorktreeIntentV1, SessionGrantV1,
};
use gate4agent_observation_api::{
    ManagedSessionKey, ObservationTarget, ProjectionAvailability, ProjectionFreshness,
};
use gate4agent_observation_engine::{CorrelationProjection, CorrelationState, SessionProjection};
use gate4agent_observation_protocol::{
    BlockAuthorityV1, HostRequestDecisionV1, HostRequestOutcomeV1,
    ObservationEvidenceV1 as SourceEvidenceV1, ObservationInteractionOutcomeV1, ObservationKindV1,
    ObservationTodoStateV1, truncate_observation_text,
};
use gate4agent_observation_service::ObservationService;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservationAudience {
    Operator,
    GrantBound,
}

/// Collapse a mutating agent-tool call's error into the wire's `Internal`
/// WITHOUT losing it.
///
/// The `MailSend`/`MailAck`/`TaskCreate`/`TaskMove` arms each used a bare
/// `map_err(|_| Internal)`, and `Internal` reaches the calling agent as
/// "harness read unavailable" -- so mail refused on grant scope, a task moved
/// to a state it may not reach, and a genuine store failure were one
/// indistinguishable message whose cause existed in no log anywhere.
/// Measured live 2026-09-09: a cross-provider `g4a_mail_send` came back
/// `served=false` against a valid grant, with no other line recorded at all.
/// The returned error is unchanged; only the silence is.
fn internal_naming<E: std::fmt::Debug>(
    operation: &'static str,
) -> impl Fn(E) -> HarnessReadHostErrorV1 {
    move |error| {
        tracing::warn!(operation, error = ?error, "harness agent-tool call failed");
        HarnessReadHostErrorV1::Internal
    }
}

#[cfg(test)]
pub(crate) fn verify_and_execute_read(
    harness: &mut HarnessService,
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
    let runtime_inventory = crate::runtime::HarnessRuntimeInventoryCache::default();
    let dispatch = execute_read(harness, observation, support, &claims, request, &runtime_inventory)?;
    // This test-only wrapper has no C2/Node adapter (and no runtime
    // inventory), so it cannot complete a `ContextPack` or `WorkspacePath`
    // fetch -- see `dispatch_mail_fetch`'s own doc comment.
    let response = match dispatch {
        ReadDispatch::Response(response) => response,
        ReadDispatch::ContextPackFetch(_) => return Err(HarnessReadHostErrorV1::Internal),
        ReadDispatch::WorkspacePathFetch(_) => return Err(HarnessReadHostErrorV1::Internal),
    };
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

/// Verify a decoded credential binding against the observation engine's
/// current view of the node incarnation, managed runtime and projection.
///
/// This folds five distinct causes into one `Unauthorized` on the wire (the
/// wire contract and agent-visible behaviour are unchanged), but each
/// failing branch names precisely which check failed via `tracing::warn!`
/// before returning. Measured live 2026-09-09: a kimi PTY session's first
/// `g4a_context_get` call was refused here and the immediate retry
/// succeeded, with no way to tell which of the five checks was transiently
/// false -- this is that missing signal.
pub(crate) fn verify_observation_credential_binding(
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    binding: &crate::credential::CredentialBindingV1,
) -> Result<(), HarnessReadHostErrorV1> {
    let node_id = match gate4agent_observation_api::NodeId::new(binding.node_id.as_str()) {
        Ok(node_id) => node_id,
        Err(_) => {
            tracing::warn!(
                check = "parse-node-id",
                node_id = binding.node_id.as_str(),
                "harness MCP read call binding check failed",
            );
            return Err(HarnessReadHostErrorV1::Unauthorized);
        }
    };
    let incarnation_id = match binding
        .node_incarnation
        .as_str()
        .parse::<gate4agent_observation_api::NodeIncarnationId>()
    {
        Ok(incarnation_id) => incarnation_id,
        Err(_) => {
            tracing::warn!(
                check = "parse-node-incarnation",
                node_incarnation = binding.node_incarnation.as_str(),
                "harness MCP read call binding check failed",
            );
            return Err(HarnessReadHostErrorV1::Unauthorized);
        }
    };
    let record_id = match gate4agent_observation_api::SessionRecordId::new(binding.record_id.as_str()) {
        Ok(record_id) => record_id,
        Err(_) => {
            tracing::warn!(
                check = "parse-record-id",
                record_id = binding.record_id.as_str(),
                "harness MCP read call binding check failed",
            );
            return Err(HarnessReadHostErrorV1::Unauthorized);
        }
    };
    let key = ManagedSessionKey { node_id, incarnation_id, record_id };
    if !support.is_current(&key.node_id, key.incarnation_id) {
        tracing::warn!(
            check = "node-incarnation-not-current",
            node_id = %key.node_id,
            node_incarnation = %key.incarnation_id,
            "harness MCP read call binding check failed",
        );
        return Err(HarnessReadHostErrorV1::Unauthorized);
    }
    let Some(runtime) = observation.engine().managed_runtime(&key) else {
        tracing::warn!(
            check = "managed-runtime-absent",
            node_id = %key.node_id,
            node_incarnation = %key.incarnation_id,
            record_id = %key.record_id,
            "harness MCP read call binding check failed",
        );
        return Err(HarnessReadHostErrorV1::Unauthorized);
    };
    if runtime.workspace_id.as_str() != binding.workspace_id.as_str() {
        tracing::warn!(
            check = "runtime-workspace-mismatch",
            runtime_workspace = runtime.workspace_id.as_str(),
            binding_workspace = binding.workspace_id.as_str(),
            "harness MCP read call binding check failed",
        );
        return Err(HarnessReadHostErrorV1::Unauthorized);
    }
    if runtime.instance_id.0 != binding.instance_id {
        tracing::warn!(
            check = "runtime-instance-mismatch",
            runtime_instance = runtime.instance_id.0,
            binding_instance = binding.instance_id,
            "harness MCP read call binding check failed",
        );
        return Err(HarnessReadHostErrorV1::Unauthorized);
    }
    if runtime.generation.0 != binding.generation {
        tracing::warn!(
            check = "runtime-generation-mismatch",
            runtime_generation = runtime.generation.0,
            binding_generation = binding.generation,
            "harness MCP read call binding check failed",
        );
        return Err(HarnessReadHostErrorV1::Unauthorized);
    }
    if observation
        .projection(&ObservationTarget::Managed { key: key.clone() })
        .is_none()
    {
        tracing::warn!(
            check = "projection-absent",
            node_id = %key.node_id,
            node_incarnation = %key.incarnation_id,
            record_id = %key.record_id,
            "harness MCP read call binding check failed",
        );
        return Err(HarnessReadHostErrorV1::Unauthorized);
    }
    Ok(())
}

pub(crate) fn execute_read(
    harness: &mut HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    claims: &VerifiedCredentialV1,
    request: HarnessReadRequestV1,
    runtime_inventory: &crate::runtime::HarnessRuntimeInventoryCache,
) -> Result<ReadDispatch, HarnessReadHostErrorV1> {
    if let HarnessReadRequestV1::MailFetch { message_id, ref_index } = request {
        return dispatch_mail_fetch(harness, observation, support, &claims.binding, message_id, ref_index);
    }
    execute_exact_binding_read(
        harness,
        observation,
        support,
        &claims.binding,
        request,
        runtime_inventory,
    ).map(ReadDispatch::Response)
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
    harness: &mut HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    binding: &CredentialBindingV1,
    request: HarnessReadRequestV1,
    runtime_inventory: &crate::runtime::HarnessRuntimeInventoryCache,
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
    // Everything below this line that mutates (`SendMail`/`AckMail`/
    // `TaskCreate`/`TaskMove`/`RunFinish`) reads only owned values out of
    // `grant`/`binding` first, then calls into `harness` -- never
    // `engine`/`grant`/`visibility` again in that same arm -- so the
    // immutable borrow of `harness` those two hold ends before the mutable
    // one `HarnessService::send_mail`/`ack_mail`/`agent_create_task`/
    // `agent_move_task`/`crate::runtime::agent_finish_run` needs begins.
    match request {
        HarnessReadRequestV1::MailSend { to, subject, body, reply_to, refs } => {
            let grant_id = grant.grant_id.clone();
            let actor_run_id = binding.actor_run_id.clone();
            let now_unix_ms = unix_time_ms();
            let from = resolve_caller_record_ref(binding, runtime_inventory);
            let result = harness.send_mail(
                grant_id, actor_run_id, from, to, subject, body, reply_to, refs, now_unix_ms,
            ).map_err(internal_naming("g4a_mail_send"))?;
            Ok(HarnessReadResponseV1::MailSend(result))
        }
        HarnessReadRequestV1::MailInbox { since_unix_ms, limit } => {
            let page = mail_inbox_page(engine, &grant.grant_id, &binding.record_id, since_unix_ms, limit)?;
            Ok(HarnessReadResponseV1::MailInbox(page))
        }
        HarnessReadRequestV1::MailAck { message_id } => {
            let grant_id = grant.grant_id.clone();
            let reader_record_id = binding.record_id.clone();
            let actor_run_id = binding.actor_run_id.clone();
            let now_unix_ms = unix_time_ms();
            harness.ack_mail(
                grant_id,
                HarnessActorV1::ParentRun { run_id: actor_run_id },
                reader_record_id,
                message_id,
                now_unix_ms,
            ).map_err(internal_naming("g4a_mail_ack"))?;
            Ok(HarnessReadResponseV1::MailAck(HarnessMailAckResultV1 { acked: true }))
        }
        HarnessReadRequestV1::TaskCreate { title, body, parent_task_id } => {
            let grant_id = grant.grant_id.clone();
            let actor_run_id = binding.actor_run_id.clone();
            let now_unix_ms = unix_time_ms();
            let result = harness.agent_create_task(
                actor_run_id, grant_id, title, body, parent_task_id, now_unix_ms,
            ).map_err(internal_naming("g4a_task_create"))?;
            Ok(HarnessReadResponseV1::TaskCreate(result))
        }
        HarnessReadRequestV1::TaskMove { task_id, expected_revision, to } => {
            let grant_id = grant.grant_id.clone();
            let actor_run_id = binding.actor_run_id.clone();
            let now_unix_ms = unix_time_ms();
            let result = harness.agent_move_task(
                actor_run_id, grant_id, task_id, expected_revision, to, now_unix_ms,
            ).map_err(internal_naming("g4a_task_move"))?;
            Ok(HarnessReadResponseV1::TaskMove(result))
        }
        HarnessReadRequestV1::RunFinish { outcome, summary } => {
            let grant_id = grant.grant_id.clone();
            let actor_run_id = binding.actor_run_id.clone();
            let now_unix_ms = unix_time_ms();
            let node_id = gate4agent_observation_api::NodeId::new(binding.node_id.as_str())
                .map_err(internal_naming("g4a_run_finish"))?;
            let incarnation_id = binding.node_incarnation.as_str()
                .parse::<gate4agent_observation_api::NodeIncarnationId>()
                .map_err(internal_naming("g4a_run_finish"))?;
            let result = crate::runtime::agent_finish_run(
                harness, &actor_run_id, &node_id, incarnation_id, outcome, now_unix_ms,
            ).map_err(internal_naming("g4a_run_finish"))?;
            // D4/S10: the summary rides the proven mail channel, posted only
            // once the finish is already committed -- a mail failure (a
            // refusal, or a genuine send error) must never undo or block a
            // finish that already happened, so it is logged and swallowed,
            // never propagated as this call's own error.
            if let HarnessRunFinishResultV1::Finished { task_id, .. } = &result {
                if let Some(summary) = summary {
                    let from = resolve_caller_record_ref(binding, runtime_inventory);
                    let subject = run_finish_mail_subject(outcome);
                    match harness.send_mail(
                        grant_id,
                        actor_run_id,
                        from,
                        HarnessMailAddressV1::Task { task_id: task_id.clone() },
                        subject,
                        summary,
                        None,
                        Vec::new(),
                        now_unix_ms,
                    ) {
                        Ok(HarnessMailSendResultV1::Accepted { .. }) => {}
                        Ok(HarnessMailSendResultV1::Refused { reason }) => {
                            tracing::warn!(
                                reason,
                                "g4a_run_finish: summary mail refused; the run is already finished",
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = ?error,
                                "g4a_run_finish: summary mail failed; the run is already finished",
                            );
                        }
                    }
                }
            }
            Ok(HarnessReadResponseV1::RunFinish(result))
        }
        _ => execute_exact_binding_read_only(harness.engine(), observation, support, binding, grant, &visibility, request),
    }
}

fn execute_exact_binding_read_only(
    engine: &gate4agent_harness_engine::HarnessEngine,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    binding: &CredentialBindingV1,
    grant: &SessionGrantV1,
    visibility: &HarnessReadVisibilityV1,
    request: HarnessReadRequestV1,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    match request {
        HarnessReadRequestV1::ContextGet => context(
            engine, observation, grant, binding, visibility,
        ),
        HarnessReadRequestV1::MonitorGet { run_id } => {
            let run_id = authorized_monitor_run(engine, grant, binding, visibility, run_id)?;
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
            let run_id = authorized_monitor_run(engine, grant, binding, visibility, run_id)?;
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
                .map(|task| redact_task(task, visibility))
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
            Ok(HarnessReadResponseV1::Task(redact_task(task, visibility)))
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
                .map(|run| redact_run(run, visibility))
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
            Ok(HarnessReadResponseV1::Run(redact_run(run, visibility)))
        }
        HarnessReadRequestV1::OperationGet { operation_id } => {
            if !visibility.operation_visible(&operation_id) {
                return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
            }
            let operation = engine.operation(&operation_id)
                .ok_or(HarnessReadHostErrorV1::NotFoundOrDenied)?;
            Ok(HarnessReadResponseV1::Operation(redact_operation(operation, visibility)))
        }
        HarnessReadRequestV1::MailFetch { message_id, ref_index } => {
            mail_fetch(engine, observation, support, grant, binding, message_id, ref_index)
        }
        // `MailSend`/`MailInbox`/`MailAck`/`TaskCreate`/`TaskMove`/
        // `RunFinish` never reach this function -- `execute_exact_binding_
        // read` dispatches all six itself, before ever delegating here.
        // Never reached in practice; refused rather than panicking if it
        // somehow were.
        HarnessReadRequestV1::MailSend { .. }
        | HarnessReadRequestV1::MailInbox { .. }
        | HarnessReadRequestV1::MailAck { .. }
        | HarnessReadRequestV1::TaskCreate { .. }
        | HarnessReadRequestV1::TaskMove { .. }
        | HarnessReadRequestV1::RunFinish { .. } => Err(HarnessReadHostErrorV1::InvalidRequest),
    }
}

/// D3/Slice C's `g4a_mail_fetch`: first verifies the message is in
/// `binding.record_id`'s inbox (`HarnessEngine::mail_fetch`'s own membership
/// predicate -- the same one `mail_inbox` uses), then resolves the ref. An
/// engine refusal (not addressed to this reader, an out-of-range index)
/// travels back as `HarnessMailFetchResultV1::Refused` with the engine's own
/// `Display` text -- a named domain refusal, never a host error, same
/// pattern as `MailSend` -- except `HarnessEngineError::MailRefTargetUnknown`
/// (no run's own `context_pack` receipt carries the ref's digest), which
/// `mail_fetch_engine_error_result` maps onto its own typed
/// `MailRefContextPackUnknown` instead, mirroring `WorkspacePath`'s typed
/// refusal family (D3/Slice C2). Deliberately does NOT call
/// `authorized_monitor_run` -- that enforces the caller's own lineage
/// (`visibility.run_visible`), which a mailed `Run` ref is explicitly allowed
/// to cross; the ONLY gate here is `HarnessEngine::mail_fetch`'s own
/// inbox-membership check.
///
/// A resolved `ContextPack` or `WorkspacePath` is refused locally here --
/// this function (via `execute_exact_binding_read_only`) has no C2/Node
/// adapter (and, for `WorkspacePath`, no runtime inventory to resolve a
/// node from), so it cannot complete either byte fetch. The one caller that
/// CAN, `execute_read`'s own `HostCommand::Read` production path, never
/// reaches this function for a `MailFetch` request at all: it intercepts
/// the request before delegating down (see `dispatch_mail_fetch`), resolves
/// the ref itself, and only falls through to `project_resolved_mail_fetch`
/// for `Run`/`Result`. Direct test callers of `execute_exact_binding_read`
/// and the harness-MCP relay path (`relay_harness_mcp_read_call`) still
/// reach this function and its local refusals.
fn mail_fetch(
    engine: &gate4agent_harness_engine::HarnessEngine,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    grant: &SessionGrantV1,
    binding: &CredentialBindingV1,
    message_id: HarnessMailMessageId,
    ref_index: u16,
) -> Result<HarnessReadResponseV1, HarnessReadHostErrorV1> {
    let resolved = match engine.mail_fetch(
        &grant.grant_id,
        &binding.record_id,
        &message_id,
        usize::from(ref_index),
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            return Ok(HarnessReadResponseV1::MailFetch(mail_fetch_engine_error_result(error)));
        }
    };
    if let HarnessMailFetchResolutionV1::ContextPack { digest, .. } = &resolved {
        return Ok(HarnessReadResponseV1::MailFetch(HarnessMailFetchResultV1::Refused {
            reason: format!(
                "mailed context pack {digest} requires the node-connected read path",
            ),
        }));
    }
    if let HarnessMailFetchResolutionV1::WorkspacePath { workspace, path, .. } = &resolved {
        return Ok(HarnessReadResponseV1::MailFetch(HarnessMailFetchResultV1::Refused {
            reason: format!(
                "mailed workspace path {}:{path} requires the node-connected read path",
                workspace.as_str(),
            ),
        }));
    }
    project_resolved_mail_fetch(engine, observation, support, resolved)
        .map(HarnessReadResponseV1::MailFetch)
}

/// Maps `HarnessEngine::mail_fetch`'s error onto the wire, shared by both of
/// this crate's call sites (`mail_fetch` above and `dispatch_mail_fetch`
/// below): `MailRefTargetUnknown` (no run's own `context_pack` receipt
/// carries the ref's digest) is a typed refusal in its own right --
/// `MailRefContextPackUnknown`, naming its input as a field rather than
/// prose, the same doctrine `WorkspacePath`'s own named refusals (D3/Slice
/// C3) already follow. Every other engine error (`MailNotForReader`,
/// `MailRefIndexOutOfRange`, and a `NotFound` unknown message id) has no
/// bounded, reusable set of inputs the way that one does, so it stays
/// `Refused` with the engine's own `Display` text verbatim.
fn mail_fetch_engine_error_result(error: HarnessEngineError) -> HarnessMailFetchResultV1 {
    match error {
        HarnessEngineError::MailRefTargetUnknown { digest } => {
            HarnessMailFetchResultV1::MailRefContextPackUnknown { digest }
        }
        other => HarnessMailFetchResultV1::Refused { reason: other.to_string() },
    }
}

/// The `Run`/`Result` projection half of `mail_fetch`, split out so
/// `dispatch_mail_fetch` (the C2-connected production path) can reuse it
/// without re-resolving the ref through the engine a second time. Returns
/// `HarnessReadHostErrorV1::Internal` if given the `ContextPack` or
/// `WorkspacePath` resolution -- every caller special-cases both variants
/// itself first (see each caller's own doc comment).
fn project_resolved_mail_fetch(
    engine: &gate4agent_harness_engine::HarnessEngine,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    resolved: HarnessMailFetchResolutionV1,
) -> Result<HarnessMailFetchResultV1, HarnessReadHostErrorV1> {
    match resolved {
        HarnessMailFetchResolutionV1::Run { run_id } => {
            if engine.run(&run_id).is_none() {
                return Ok(HarnessMailFetchResultV1::Refused {
                    reason: format!("mailed run {run_id} no longer exists"),
                });
            }
            let HarnessReadResponseV1::Monitor(monitor_value) = monitor(
                engine,
                observation,
                support,
                HarnessMonitoringVisibilityV1::Timeline,
                &run_id,
                ObservationAudience::GrantBound,
            )? else {
                return Err(HarnessReadHostErrorV1::Internal);
            };
            let HarnessReadResponseV1::Timeline(timeline_value) = timeline(
                engine,
                observation,
                support,
                &run_id,
                None,
                HARNESS_TIMELINE_PAGE_LIMIT_MAX,
                ObservationAudience::GrantBound,
            )? else {
                return Err(HarnessReadHostErrorV1::Internal);
            };
            Ok(HarnessMailFetchResultV1::Run { monitor: monitor_value, timeline: timeline_value })
        }
        HarnessMailFetchResolutionV1::Result { result_ref } => {
            let Ok(run_id) = result_ref.run_id() else {
                return Ok(HarnessMailFetchResultV1::Refused {
                    reason: format!("mailed result reference {result_ref} is malformed"),
                });
            };
            let Some(run) = engine.run(&run_id) else {
                return Ok(HarnessMailFetchResultV1::Refused {
                    reason: format!("mailed result reference names a run that no longer exists: {run_id}"),
                });
            };
            // The lineage fence stays closed for cross-references reachable
            // FROM this run (its own parent/task/operation) -- only the run
            // the ref names was authorized by the mail, not its lineage.
            Ok(HarnessMailFetchResultV1::Result {
                run: redact_run(run, &HarnessReadVisibilityV1::default()),
            })
        }
        HarnessMailFetchResolutionV1::ContextPack { .. } => Err(HarnessReadHostErrorV1::Internal),
        HarnessMailFetchResolutionV1::WorkspacePath { .. } => Err(HarnessReadHostErrorV1::Internal),
    }
}

/// D3/Slice C2+C3: the one call site of `execute_read` that can complete a
/// resolved `ContextPack` or `WorkspacePath` ref's byte fetch
/// (`HostCommand::Read`'s production arm in `runtime.rs`, which owns a
/// `HarnessC2Adapter` and the runtime inventory a `WorkspacePath` ref needs
/// to resolve a node). Re-does the small slice of `execute_exact_binding_
/// read`'s prologue that applies to `MailFetch` (request validation, the
/// exact grant/binding CAS check, the `grant.mail` gate `authorize_request`
/// enforces for every other MailFetch path) rather than routing through
/// that function, so either C2-capable resolution can be signalled back as
/// its own `ReadDispatch` variant instead of a `HarnessReadResponseV1`.
pub(crate) fn dispatch_mail_fetch(
    harness: &mut HarnessService,
    observation: &ObservationService,
    support: &ObservationSupportRegistry,
    binding: &CredentialBindingV1,
    message_id: HarnessMailMessageId,
    ref_index: u16,
) -> Result<ReadDispatch, HarnessReadHostErrorV1> {
    message_id.validate().map_err(|_| HarnessReadHostErrorV1::InvalidRequest)?;
    let engine = harness.engine();
    let grant = engine.grant(&binding.grant_id)
        .filter(|grant| {
            grant.revision == binding.grant_revision
                && grant.actor_run_id == binding.actor_run_id
        })
        .ok_or(HarnessReadHostErrorV1::Unauthorized)?;
    if !grant.mail {
        return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
    }
    match engine.mail_fetch(&grant.grant_id, &binding.record_id, &message_id, usize::from(ref_index)) {
        Ok(HarnessMailFetchResolutionV1::ContextPack { run_id, digest }) => {
            Ok(ReadDispatch::ContextPackFetch(ContextPackFetchIntent { run_id, digest }))
        }
        Ok(HarnessMailFetchResolutionV1::WorkspacePath { workspace, path, sha256 }) => {
            Ok(ReadDispatch::WorkspacePathFetch(WorkspacePathFetchIntent {
                workspace: workspace.as_str().to_owned(),
                path,
                sha256,
            }))
        }
        Ok(resolved) => project_resolved_mail_fetch(engine, observation, support, resolved)
            .map(|result| ReadDispatch::Response(HarnessReadResponseV1::MailFetch(result))),
        Err(error) => Ok(ReadDispatch::Response(HarnessReadResponseV1::MailFetch(
            mail_fetch_engine_error_result(error),
        ))),
    }
}

/// `execute_read`'s outcome: either a complete response, or -- for a
/// `MailFetch` request whose ref resolved to a `ContextPack` or a
/// `WorkspacePath` -- the intent the caller (`HostCommand::Read`'s
/// production arm) must start a C2/Node round trip for before it can reply.
pub(crate) enum ReadDispatch {
    Response(HarnessReadResponseV1),
    ContextPackFetch(ContextPackFetchIntent),
    WorkspacePathFetch(WorkspacePathFetchIntent),
}

/// `dispatch_mail_fetch`'s deferred outcome: the run whose own
/// `context_pack.digest` matched the mailed ref, and that digest itself
/// (`HarnessEngine::mail_fetch` already proved they match). The caller
/// resolves that run's stored node binding and starts the fetch; this type
/// carries no node/route information itself since this crate's read layer
/// does not depend on C2.
pub(crate) struct ContextPackFetchIntent {
    pub(crate) run_id: HarnessRunId,
    pub(crate) digest: String,
}

/// `dispatch_mail_fetch`'s deferred outcome for a `WorkspacePath` ref
/// (D3/Slice C3): the ref's own fields verbatim, exactly as `HarnessEngine::
/// mail_fetch` resolved them -- unlike `ContextPackFetchIntent`, this names
/// no run at all, since a `WorkspacePath` ref carries no run binding to
/// resolve a node from. The caller resolves `workspace` to a node from its
/// own runtime inventory instead (`HarnessRuntimeInventoryCache::
/// all_nodes`), a lookup this crate's read layer does not depend on either.
pub(crate) struct WorkspacePathFetchIntent {
    pub(crate) workspace: String,
    pub(crate) path: String,
    pub(crate) sha256: String,
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
        HarnessReadRequestV1::MailSend { .. }
        | HarnessReadRequestV1::MailInbox { .. }
        | HarnessReadRequestV1::MailAck { .. }
        | HarnessReadRequestV1::MailFetch { .. } if !grant.mail =>
        {
            return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
        }
        HarnessReadRequestV1::TaskCreate { .. } if !grant.task_permissions.create => {
            return Err(HarnessReadHostErrorV1::NotFoundOrDenied);
        }
        HarnessReadRequestV1::TaskMove { .. } if !grant.task_permissions.mutate => {
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
    let task_visible = visibility.task_visible(&run.task_id);
    let (task, sibling_runs) = task_visible
        .then(|| task_and_sibling_runs(engine, run, visibility))
        .unwrap_or_default();
    Ok(HarnessReadResponseV1::Context(SessionContextV1 {
        grant_id: grant.grant_id.clone(),
        grant_revision: grant.revision,
        actor_run: CallerRunV1 {
            run_id: run.run_id.clone(),
            task_id: task_visible.then(|| run.task_id.clone()),
            parent_run_id: run.parent_run_id.as_ref()
                .filter(|run_id| visibility.run_visible(run_id)).cloned(),
            lifecycle: run.lifecycle,
            references_redacted: !task_visible
                || run.parent_run_id.as_ref().is_some_and(|run_id| {
                    !visibility.run_visible(run_id)
                }),
        },
        task,
        sibling_runs,
        read_permissions: grant.read_permissions.clone(),
        monitoring_visibility: grant.monitoring_visibility,
        maximum_child_count: grant.maximum_child_count,
        maximum_child_depth: grant.maximum_child_depth,
        mail: grant.mail,
        task_create: grant.task_permissions.create,
        task_mutate: grant.task_permissions.mutate,
        allowed_tool_ids: allowed_tool_ids(grant),
        history_message_count: history.map(|history| history.message_count),
        completed_turn_count: history.and_then(|history| history.completed_turn_count),
        total_tokens: history.and_then(|history| history.total_tokens),
    }))
}

/// The calling run's own task plus every OTHER run recorded against that same
/// task ("what previous sessions did on it"), gated solely by whether the
/// grant can see the task itself (same predicate as `actor_run.task_id`).
/// Deliberately independent of the grant's `runs` read scope: that scope
/// governs the general run-browsing tools (`g4a_runs_get`/`g4a_runs_list`),
/// not what a session can see about the one task it is attached to. Siblings
/// come from the task's own canonical `run_ids`, never from a scan, so no
/// other task's runs can surface here.
fn task_and_sibling_runs(
    engine: &gate4agent_harness_engine::HarnessEngine,
    run: &gate4agent_harness_protocol::HarnessRunV1,
    visibility: &HarnessReadVisibilityV1,
) -> (Option<RedactedTaskV1>, Vec<RedactedRunV1>) {
    let Some(task) = engine.task(&run.task_id) else {
        return (None, Vec::new());
    };
    let sibling_runs = task.run_ids.iter()
        .filter(|sibling_id| **sibling_id != run.run_id)
        .filter_map(|sibling_id| engine.run(sibling_id))
        .map(|sibling| redact_run(sibling, visibility))
        .collect();
    (Some(redact_task(task, visibility)), sibling_runs)
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
        active_blocks: projection.map_or(0, |projection| {
            u16::try_from(projection.blocked_count).unwrap_or(u16::MAX)
        }),
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

pub(crate) fn allowed_tool_ids(grant: &SessionGrantV1) -> Vec<String> {
    // S10: `g4a_run_finish` carries no grant-permission gate at all -- every
    // session needs the ability to report its own work finished regardless
    // of what else its grant allows, the same unconditional footing
    // `g4a_context_get` already stands on.
    let mut tools = vec!["g4a_context_get", "g4a_run_finish"];
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
    if grant.mail {
        tools.extend(["g4a_mail_send", "g4a_mail_inbox", "g4a_mail_ack", "g4a_mail_fetch"]);
    }
    if grant.task_permissions.create {
        tools.push("g4a_task_create");
    }
    if grant.task_permissions.mutate {
        tools.push("g4a_task_move");
    }
    tools.sort_unstable();
    tools.into_iter().map(str::to_owned).collect()
}

/// The tool id a served `HarnessReadRequestV1` corresponds to, using the
/// same naming `allowed_tool_ids` advertises. For logging a served harness
/// MCP call only -- `execute_exact_binding_read` dispatches on the request
/// value itself, not this id.
pub(crate) fn harness_mcp_tool_id(request: &HarnessReadRequestV1) -> &'static str {
    match request {
        HarnessReadRequestV1::ContextGet => "g4a_context_get",
        HarnessReadRequestV1::MonitorGet { .. } => "g4a_monitor_get",
        HarnessReadRequestV1::TimelineRead { .. } => "g4a_timeline_read",
        HarnessReadRequestV1::TasksList { .. } => "g4a_tasks_list",
        HarnessReadRequestV1::TaskGet { .. } => "g4a_tasks_get",
        HarnessReadRequestV1::RunsList { .. } => "g4a_runs_list",
        HarnessReadRequestV1::RunGet { .. } => "g4a_runs_get",
        HarnessReadRequestV1::OperationGet { .. } => "g4a_operation_get",
        HarnessReadRequestV1::MailSend { .. } => "g4a_mail_send",
        HarnessReadRequestV1::MailInbox { .. } => "g4a_mail_inbox",
        HarnessReadRequestV1::MailAck { .. } => "g4a_mail_ack",
        HarnessReadRequestV1::MailFetch { .. } => "g4a_mail_fetch",
        HarnessReadRequestV1::TaskCreate { .. } => "g4a_task_create",
        HarnessReadRequestV1::TaskMove { .. } => "g4a_task_move",
        HarnessReadRequestV1::RunFinish { .. } => "g4a_run_finish",
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(1).max(1)
}

/// The short, fixed subject `g4a_run_finish`'s own summary mail carries --
/// never caller-supplied prose (the summary itself is the body); just names
/// which of the two outcomes this notice is about.
fn run_finish_mail_subject(outcome: HarnessRunFinishOutcomeV1) -> String {
    match outcome {
        HarnessRunFinishOutcomeV1::Done => "run finished: done".to_owned(),
        HarnessRunFinishOutcomeV1::Failed => "run finished: failed".to_owned(),
    }
}

/// The sender identity a mail message's `from` carries -- read once, at send
/// time, off the harness's own cached runtime inventory
/// (`gate4agent-harness-service::runtime::HarnessRuntimeInventoryCache`, the
/// same source `HarnessRuntimeManagedSessionV1.provider` already reads);
/// never recomputed on a later `g4a_mail_inbox` read, which returns the
/// stored `HarnessMailMessageV1.from` verbatim. Falls back to the literal
/// `"unknown"` -- itself always a valid `HarnessSelectorV1` -- when the
/// cache has no entry yet for this record (a fresh session, or an `Inline`
/// caller with no `ManagedSessionRecord` at all): informational only, S9's
/// mailbox never reads `from` to decide anything.
fn resolve_caller_record_ref(
    binding: &CredentialBindingV1,
    runtime_inventory: &crate::runtime::HarnessRuntimeInventoryCache,
) -> gate4agent_harness_protocol::HarnessRecordRef {
    let provider_name = gate4agent_observation_api::NodeId::new(binding.node_id.as_str())
        .ok()
        .and_then(|node_id| runtime_inventory.node(&node_id))
        .and_then(|node| {
            node.inventory.managed_sessions.iter()
                .find(|session| session.record_id == binding.record_id.as_str())
                .map(|session| session.provider.clone())
        });
    let provider = provider_name
        .and_then(|name| HarnessSelectorV1::new(name).ok())
        .unwrap_or_else(|| {
            HarnessSelectorV1::new("unknown")
                .expect("the literal \"unknown\" satisfies HarnessSelectorV1's charset and length bound")
        });
    gate4agent_harness_protocol::HarnessRecordRef {
        record_id: binding.record_id.clone(),
        provider,
    }
}

/// Builds the `g4a_mail_inbox` / operator `MailInbox` page for
/// `reader_record_id` under `grant_id`'s own visibility
/// (`HarnessEngine::mail_inbox`), converting each engine entry to the wire
/// shape and counting unacked entries.
fn mail_inbox_page(
    engine: &gate4agent_harness_engine::HarnessEngine,
    grant_id: &SessionGrantId,
    reader_record_id: &HarnessSelectorV1,
    since_unix_ms: Option<u64>,
    limit: u16,
) -> Result<HarnessMailInboxPageV1, HarnessReadHostErrorV1> {
    // Shared by the operator wire and by every agent's own `g4a_mail_inbox`, so
    // a swallowed error here reads as an unexplained `Internal` on both. Seen
    // live 2026-09-09: `mail inbox` refused every record on a stack whose
    // agents were simultaneously failing to read their own mail.
    let entries = engine.mail_inbox(grant_id, reader_record_id, since_unix_ms, usize::from(limit))
        .map_err(internal_naming("mail-inbox-page"))?;
    let unacked = entries.iter().filter(|entry| !entry.acked).count() as u64;
    let messages = entries.into_iter().map(|entry| HarnessMailInboxEntryV1 {
        message_id: entry.message.message_id,
        from: entry.message.from,
        to: entry.message.to,
        subject: entry.message.subject,
        body: entry.message.body,
        reply_to: entry.message.reply_to,
        task_id: entry.message.task_id,
        refs: entry.message.refs,
        created_at_unix_ms: entry.message.created_at_unix_ms,
        acked: entry.acked,
        thread: entry.thread.into_iter()
            .map(|message| message.message_id)
            .take(usize::from(HARNESS_MAIL_THREAD_IDS_MAX))
            .collect(),
    }).collect();
    Ok(HarnessMailInboxPageV1 { messages, unacked })
}

/// The operator wire's `MailInbox`, keyed by a bare `record_id` rather than
/// a grant -- resolves the record's own current mail-permitted grant
/// (`crate::mail_grant_for_record`) and reuses `mail_inbox_page` when one
/// exists. Without one (a dormant record with no currently mail-permitted
/// run bound to it), falls back to a direct scan of session-addressed mail
/// only: task-addressed/forum mail needs a grant's own visibility and stays
/// unavailable without one, and this fallback's entries carry an empty
/// `thread` (the transitive `reply_to` closure is `HarnessEngine::mail_inbox`'s
/// own internal algorithm, not reproduced here without a grant to call it
/// through).
pub(crate) fn operator_mail_inbox(
    engine: &gate4agent_harness_engine::HarnessEngine,
    record_id: &HarnessSelectorV1,
    since_unix_ms: Option<u64>,
    limit: u16,
) -> Result<HarnessMailInboxPageV1, HarnessReadHostErrorV1> {
    if let Some(grant_id) = crate::mail_grant_for_record(engine, record_id) {
        return mail_inbox_page(engine, &grant_id, record_id, since_unix_ms, limit);
    }
    let mut matches: Vec<_> = engine.mail_messages()
        .filter(|message| matches!(
            &message.to,
            HarnessMailAddressV1::Session { record_id: to } if to == record_id
        ))
        .filter(|message| since_unix_ms.map_or(true, |since| message.created_at_unix_ms >= since))
        .cloned()
        .collect();
    matches.sort_by(|left, right| {
        let left_acked = engine.mail_ack(&left.message_id, record_id).is_some();
        let right_acked = engine.mail_ack(&right.message_id, record_id).is_some();
        left_acked.cmp(&right_acked)
            .then(left.created_at_unix_ms.cmp(&right.created_at_unix_ms))
            .then(left.message_id.cmp(&right.message_id))
    });
    matches.truncate(usize::from(limit));
    let unacked = matches.iter()
        .filter(|message| engine.mail_ack(&message.message_id, record_id).is_none())
        .count() as u64;
    let messages = matches.into_iter().map(|message| {
        let acked = engine.mail_ack(&message.message_id, record_id).is_some();
        HarnessMailInboxEntryV1 {
            message_id: message.message_id,
            from: message.from,
            to: message.to,
            subject: message.subject,
            body: message.body,
            reply_to: message.reply_to,
            task_id: message.task_id,
            refs: message.refs,
            created_at_unix_ms: message.created_at_unix_ms,
            acked,
            thread: Vec::new(),
        }
    }).collect();
    Ok(HarnessMailInboxPageV1 { messages, unacked })
}

/// The `unread_mail` fill for one managed session (`runtime.rs`'s
/// `fill_managed_session_blocked_stats`): the same page `operator_mail_inbox`
/// would build for this record right now, counted rather than paged.
pub(crate) fn count_unread_mail(
    engine: &gate4agent_harness_engine::HarnessEngine,
    record_id: &HarnessSelectorV1,
) -> u64 {
    operator_mail_inbox(engine, record_id, None, HARNESS_ENTITY_PAGE_LIMIT_MAX)
        .map(|page| page.unacked)
        .unwrap_or(0)
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
        block_facts: projection.timeline.iter()
            .filter_map(|entry| block_fact(projection, entry, structured))
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

/// A `BlockFactV1` for one `ObservationKindV1::ActionBlocked` timeline
/// entry, `None` for every other kind -- see `SessionMonitorDetailV1::
/// block_facts`'s own doc comment for why this reads the session's bounded
/// `timeline` (`SessionProjection::timeline`, the same list `timeline_entry`
/// below renders the `g4a_timeline_read` page from) rather than a
/// correlation-tracked list like `tools`/`interactions`. `correlation`
/// resolves `ActionBlocked::correlation_id` against `projection.tools` --
/// the SAME lookup, and the same meaning ("the tool call this block belongs
/// to"), `timeline_entry`'s own `ActionBlocked` arm already uses.
fn block_fact(
    projection: &SessionProjection,
    entry: &gate4agent_observation_engine::TimelineEntry,
    structured: bool,
) -> Option<BlockFactV1> {
    let ObservationKindV1::ActionBlocked { correlation_id, authority, reason, .. } = &entry.kind
    else {
        return None;
    };
    Some(BlockFactV1 {
        state: BlockStateV1::Blocked,
        label: structured.then(|| block_fact_label(*authority, reason)),
        correlation: structured.then(|| {
            correlation_id.as_deref().and_then(|id| correlation_ordinal(&projection.tools, id))
        }).flatten(),
        evidence: observation_evidence(entry.evidence),
    })
}

/// `<authority slug>: <reason>`, truncated on a UTF-8 boundary to
/// `HARNESS_OBSERVATION_BLOCK_LABEL_MAX_BYTES` -- see that constant's own
/// doc comment for why it is wider than the 64-byte activity-label bound.
/// `reason` is already bounded verbatim at the observation layer
/// (`OBSERVATION_ACTION_BLOCKED_REASON_MAX_BYTES`, 1024 bytes); this
/// re-bounds the COMBINED string to the monitor wire's own, smaller label,
/// using the same producer-side cut (`truncate_observation_text`) that
/// mints `reason` itself.
fn block_fact_label(authority: BlockAuthorityV1, reason: &str) -> String {
    let combined = format!("{}: {reason}", block_authority_slug(authority));
    truncate_observation_text(&combined, HARNESS_OBSERVATION_BLOCK_LABEL_MAX_BYTES).0
}

/// The wire's own kebab-case name for a `BlockAuthorityV1` variant --
/// written out rather than round-tripped through `serde_json` so the label
/// never depends on that crate's own encoding of what is, on this type, a
/// plain string enum.
fn block_authority_slug(authority: BlockAuthorityV1) -> &'static str {
    match authority {
        BlockAuthorityV1::HarnessGate => "harness-gate",
        BlockAuthorityV1::HarnessPolicy => "harness-policy",
        BlockAuthorityV1::HarnessDeadline => "harness-deadline",
        BlockAuthorityV1::Operator => "operator",
        BlockAuthorityV1::ProviderClassifier => "provider-classifier",
        BlockAuthorityV1::ProviderPermissionRule => "provider-permission-rule",
        BlockAuthorityV1::ProviderSandbox => "provider-sandbox",
        BlockAuthorityV1::ProviderRefusal => "provider-refusal",
        BlockAuthorityV1::ProviderHook => "provider-hook",
        BlockAuthorityV1::UserRejected => "user-rejected",
        BlockAuthorityV1::ProviderQuota => "provider-quota",
        BlockAuthorityV1::Unknown => "unknown",
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
        | ObservationKindV1::InteractionResolved { .. }
        | ObservationKindV1::HostRequestObserved { .. }
        | ObservationKindV1::ActionBlocked { .. } => TimelineCategoryV1::Interaction,
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
        ObservationKindV1::HostRequestObserved { class, decision, outcome } => (
            // A `Granted` request that failed WHILE EXECUTING renders its
            // own bounded, verbatim error text as the label -- the same
            // convention `ActionBlocked`'s `reason` uses below -- rather
            // than the coarse `class` bucket an `Executed` outcome shows;
            // an operator scanning the timeline needs to see WHY it failed,
            // not just that a `Terminal`/`Fs` request happened.
            match outcome {
                HostRequestOutcomeV1::Failed { error } => Some(error.clone()),
                HostRequestOutcomeV1::Executed => Some(class.clone()),
            },
            match (decision, outcome) {
                // A pending approval must never render as `Failed` -- see
                // `HostRequestDecisionV1::Deferred`'s own doc comment.
                (HostRequestDecisionV1::Granted { .. }, HostRequestOutcomeV1::Failed { .. }) => {
                    TimelineStateV1::Failed
                }
                (HostRequestDecisionV1::Granted { .. }, HostRequestOutcomeV1::Executed) => {
                    TimelineStateV1::Completed
                }
                (HostRequestDecisionV1::Denied { .. }, _) => TimelineStateV1::Failed,
                (HostRequestDecisionV1::Deferred, _) => TimelineStateV1::Waiting,
            },
            None,
        ),
        // `reason` is verbatim, already-bounded free text (see
        // `ObservationKindV1::ActionBlocked`'s own doc comment) -- it is the
        // label outright, not a class bucket. `help` has no categorical
        // field to land in on this projection today (`TimelineEntryV1` carries
        // no free-text detail), so it is dropped here rather than smuggled
        // into `label`/`state`/`correlation`.
        ObservationKindV1::ActionBlocked { correlation_id, reason, .. } => (
            Some(reason.clone()),
            TimelineStateV1::Failed,
            correlation_id.as_deref().and_then(|id| correlation_ordinal(&projection.tools, id)),
        ),
        ObservationKindV1::UnrecognizedNotification { method } => {
            (Some(method.clone()), TimelineStateV1::Unknown, None)
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
        HostDecisionAuthorityV1, ObservationCapabilitiesV1, ObservationSourceFamilyV1,
        ObservationTodoItemV1, ObservationV1,
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
            mail: false,
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

    fn task_id_a() -> HarnessTaskId {
        HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap()
    }

    fn task_id_b() -> HarnessTaskId {
        HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap()
    }

    fn run_id_1() -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap()
    }

    fn run_id_2() -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap()
    }

    fn run_id_3() -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap()
    }

    fn run_id_4() -> HarnessRunId {
        HarnessRunId::new(format!("hrun_{}", "d".repeat(24))).unwrap()
    }

    fn operation_id_2() -> HarnessOperationId {
        HarnessOperationId::new(format!("hop_{}", "b".repeat(24))).unwrap()
    }

    fn operation_id_3() -> HarnessOperationId {
        HarnessOperationId::new(format!("hop_{}", "c".repeat(24))).unwrap()
    }

    fn operation_id_4() -> HarnessOperationId {
        HarnessOperationId::new(format!("hop_{}", "d".repeat(24))).unwrap()
    }

    /// Task A owns three runs: the actor's own (`run_id_1`, unchanged from
    /// `credential::tests::engine`), a completed sibling with a context-pack
    /// receipt and git facts (`run_id_2`), and a failed sibling whose parent
    /// points at a run of an unrelated task (`run_id_3` -> `run_id_4`). Task
    /// A also depends on decoy task B, whose only run (`run_id_4`) must never
    /// surface through task A's grant. The grant's `runs`/`operations` read
    /// scopes stay `None`, to prove `sibling_runs` is populated from task
    /// visibility alone, not from those unrelated scopes.
    fn task_with_siblings_engine() -> gate4agent_harness_engine::HarnessEngine {
        let mut checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();

        let node_incarnation = checkpoint.runs[0].binding.as_ref()
            .expect("actor run is bound in the credential fixture")
            .node_incarnation.clone();

        checkpoint.tasks[0].dependencies = vec![task_id_b()];
        checkpoint.tasks[0].run_ids = vec![run_id_1(), run_id_2(), run_id_3()];
        checkpoint.tasks[0].result_refs = vec![HarnessResultRef::for_run(&run_id_2())];

        checkpoint.grants[0].read_permissions = HarnessReadPermissionsV1 {
            tasks: HarnessEntityReadScopeV1::SelfOnly,
            runs: HarnessEntityReadScopeV1::None,
            operations: HarnessEntityReadScopeV1::None,
        };

        checkpoint.runs.push(HarnessRunV1 {
            run_id: run_id_2(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id_a(),
            operation_id: operation_id_2(),
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("claude-default"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: Some(HarnessResolvedContextPackReceiptV1 {
                id: selector("context-sibling-b"),
                digest: format!("sha256:{}", "b".repeat(64)),
                lineage: HarnessContextPackLineageV1 {
                    source_node_id: selector("node-a"),
                    source_workspace_id: selector("workspace-a"),
                    source_instance_id: 7,
                    source_generation: 1,
                    source_provider: selector("claude-default"),
                },
                source_message_count: 3,
                retained_message_count: 3,
                byte_len: 100,
                truncated: false,
            }),
            git_facts: Some(HarnessRunGitFactsV1 {
                captured_at_unix_ms: 15,
                outcome: HarnessRunGitFactsOutcomeV1::Unavailable,
            }),
            binding: Some(HarnessSessionBindingV1 {
                node_id: selector("node-a"),
                node_incarnation: node_incarnation.clone(),
                workspace_id: selector("workspace-a"),
                session: HarnessSessionIdentityV1::Managed {
                    record_id: selector("record-b"),
                    active_session: None,
                },
            }),
            lifecycle: HarnessRunLifecycleV1::Completed,
            result_disposition: Some(HarnessResultDispositionV1::Succeeded),
            failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 12,
        });
        checkpoint.operations.push(HarnessOperationV1 {
            operation_id: operation_id_2(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User { actor_id: selector("operator") },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id_a()),
            run_id: Some(run_id_2()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("b".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(
                format!("hidem_{}", "b".repeat(24)),
            ).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 12,
            dispatched_at_unix_ms: Some(10),
            finished_at_unix_ms: Some(12),
        });

        checkpoint.runs.push(HarnessRunV1 {
            run_id: run_id_3(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: Some(run_id_4()),
            task_id: task_id_a(),
            operation_id: operation_id_3(),
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("claude-default"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Failed,
            result_disposition: Some(HarnessResultDispositionV1::Failed),
            failure: Some(HarnessFailureV1 {
                category: HarnessFailureCategoryV1::Internal,
                retryable: false,
            }),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 14,
        });
        checkpoint.operations.push(HarnessOperationV1 {
            operation_id: operation_id_3(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User { actor_id: selector("operator") },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Failed,
            task_id: Some(task_id_a()),
            run_id: Some(run_id_3()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("c".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(
                format!("hidem_{}", "c".repeat(24)),
            ).unwrap(),
            failure: Some(HarnessFailureV1 {
                category: HarnessFailureCategoryV1::Internal,
                retryable: false,
            }),
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 14,
            dispatched_at_unix_ms: Some(10),
            finished_at_unix_ms: Some(14),
        });

        // Decoy task B and its run: reachable only through task B's own
        // grant, never through task A's.
        checkpoint.tasks.push(HarnessTaskV1 {
            task_id: task_id_b(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "decoy task".to_owned(),
            body: "decoy body".to_owned(),
            creator: HarnessActorV1::User { actor_id: selector("operator") },
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Running,
            run_ids: vec![run_id_4()],
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        });
        checkpoint.runs.push(HarnessRunV1 {
            run_id: run_id_4(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: task_id_b(),
            operation_id: operation_id_4(),
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("claude-default"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: Some(HarnessSessionBindingV1 {
                node_id: selector("node-a"),
                node_incarnation,
                workspace_id: selector("workspace-a"),
                session: HarnessSessionIdentityV1::Managed {
                    record_id: selector("record-d"),
                    active_session: Some(HarnessRuntimeIdentityV1 { instance_id: 9, generation: 1 }),
                },
            }),
            lifecycle: HarnessRunLifecycleV1::Running,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        });
        checkpoint.operations.push(HarnessOperationV1 {
            operation_id: operation_id_4(),
            revision: HarnessRevision::new(1).unwrap(),
            actor: HarnessActorV1::User { actor_id: selector("operator") },
            kind: HarnessOperationKindV1::CreateRun,
            state: HarnessOperationStateV1::Succeeded,
            task_id: Some(task_id_b()),
            run_id: Some(run_id_4()),
            grant_id: None,
            reconciles_operation_id: None,
            expected_revision: Some(HarnessRevision::new(1).unwrap()),
            request_digest: HarnessRequestDigest::new("d".repeat(64)).unwrap(),
            idempotency_ref: HarnessIdempotencyRef::new(
                format!("hidem_{}", "d".repeat(24)),
            ).unwrap(),
            failure: None,
            outcome_unknown_reason: None,
            reconciliation_outcome: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
            dispatched_at_unix_ms: Some(10),
            finished_at_unix_ms: Some(10),
        });

        gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap()
    }

    #[test]
    fn context_task_and_sibling_runs_are_scoped_to_the_actor_task_and_never_cross_tasks() {
        let path = observation_path("context-task-siblings");
        let mut harness = HarnessService::from_engine_for_test(task_with_siblings_engine());
        let observation = ObservationService::open(&path).unwrap();
        let binding = crate::credential::tests::binding(1, 1);
        let support = ObservationSupportRegistry::default();

        let HarnessReadResponseV1::Context(context) = execute_exact_binding_read(
            &mut harness,
            &observation,
            &support,
            &binding,
            HarnessReadRequestV1::ContextGet,
            &crate::runtime::HarnessRuntimeInventoryCache::default(),
        ).unwrap() else {
            panic!("context response");
        };
        context.validate().unwrap();

        let task = context.task.as_ref().expect("own task is visible under a SelfOnly task scope");
        assert_eq!(task.task_id, task_id_a());
        assert_eq!(task.title, "credential fixture");
        assert_eq!(task.state, HarnessTaskStateV1::Running);
        assert_eq!(task.result_refs, vec![HarnessResultRef::for_run(&run_id_2())]);
        // Task B is a real dependency, but it sits outside this grant's task
        // scope, so the reference is stripped rather than exposed.
        assert!(task.dependency_ids.is_empty());
        // The embedded task's own `run_ids` stays gated by the (unrelated)
        // `runs` read scope, which this grant sets to `None` -- it comes
        // back empty even though `sibling_runs` below is fully populated.
        assert!(task.run_ids.is_empty());
        assert!(task.references_redacted);

        assert_eq!(context.sibling_runs.len(), 2);
        let completed = context.sibling_runs.iter().find(|run| run.run_id == run_id_2())
            .expect("completed sibling run is present");
        assert_eq!(completed.lifecycle, HarnessRunLifecycleV1::Completed);
        assert_eq!(completed.result_disposition, Some(HarnessResultDispositionV1::Succeeded));
        assert!(completed.failure_category.is_none());
        assert_eq!(
            completed.context_pack.as_ref().map(|pack| pack.digest.clone()),
            Some(format!("sha256:{}", "b".repeat(64))),
        );
        assert!(completed.git_facts.is_some());
        assert_eq!(completed.task_id, Some(task_id_a()));

        let failed = context.sibling_runs.iter().find(|run| run.run_id == run_id_3())
            .expect("failed sibling run is present");
        assert_eq!(failed.lifecycle, HarnessRunLifecycleV1::Failed);
        assert_eq!(failed.result_disposition, Some(HarnessResultDispositionV1::Failed));
        assert_eq!(failed.failure_category, Some(HarnessFailureCategoryV1::Internal));
        // Its parent is task B's run: outside the grant's run scope, so the
        // reference is stripped the same way `redact_run` already strips any
        // other out-of-scope cross-reference.
        assert!(failed.parent_run_id.is_none());
        assert!(failed.operation_id.is_none());
        assert!(failed.references_redacted);

        assert!(context.sibling_runs.iter().all(|run| run.run_id != run_id_1()));
        assert!(context.sibling_runs.iter().all(|run| run.run_id != run_id_4()));

        let encoded = serde_json::to_string(&context).unwrap();
        for forbidden in [
            task_id_b().to_string(),
            run_id_4().to_string(),
            "record-d".to_owned(),
            "decoy".to_owned(),
        ] {
            assert!(!encoded.contains(&forbidden), "task-scoped context leaked {forbidden}");
        }
        close_observation(observation, &path);
    }

    #[test]
    fn context_hides_task_and_sibling_runs_when_grant_denies_task_reads() {
        let path = observation_path("context-task-siblings-denied");
        let mut harness = HarnessService::from_engine_for_test(
            crate::credential::tests::engine(
                1,
                SessionGrantStateV1::Active,
                1,
                HarnessRunLifecycleV1::Running,
            ),
        );
        let observation = ObservationService::open(&path).unwrap();
        let binding = crate::credential::tests::binding(1, 1);
        let support = ObservationSupportRegistry::default();

        let HarnessReadResponseV1::Context(context) = execute_exact_binding_read(
            &mut harness,
            &observation,
            &support,
            &binding,
            HarnessReadRequestV1::ContextGet,
            &crate::runtime::HarnessRuntimeInventoryCache::default(),
        ).unwrap() else {
            panic!("context response");
        };
        context.validate().unwrap();
        assert!(context.actor_run.task_id.is_none());
        assert!(context.task.is_none());
        assert!(context.sibling_runs.is_empty());
        close_observation(observation, &path);
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

    /// Measured live 2026-09-05: a provider quota block (`ActionBlocked {
    /// authority: ProviderQuota, reason_kind: Some("usageLimitExceeded"), .. }`)
    /// reached the operator stream and the runtime inventory's `blocked_count`,
    /// but `harnessctl monitor <run>` showed no trace of it at all. Proof this
    /// gap is closed: two `ActionBlocked` observations with different
    /// authorities become two `block_facts` with the right `<authority
    /// slug>: <reason>` labels and bump `active_blocks` to 2, while a sibling
    /// `HostRequestObserved { outcome: Failed }` -- "authorized but failed
    /// running", never a refusal, see `HostRequestOutcomeV1`'s own doc
    /// comment -- contributes no block fact at all.
    #[test]
    fn action_blocked_observations_surface_as_monitor_block_facts_not_host_request_failures() {
        let path = observation_path("action-blocked-monitor");
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
            ObservationKindV1::ActionBlocked {
                correlation_id: None,
                tool_class: "bash".to_owned(),
                authority: BlockAuthorityV1::HarnessGate,
                reason_kind: None,
                reason: "rule=deny-write".to_owned(),
                help: None,
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            2,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::ActionBlocked {
                correlation_id: None,
                tool_class: "session/prompt".to_owned(),
                authority: BlockAuthorityV1::ProviderQuota,
                reason_kind: Some("usageLimitExceeded".to_owned()),
                reason: "usage limit exceeded".to_owned(),
                help: None,
            },
        );
        apply_observation(
            &mut observation,
            "record-a",
            3,
            SourceEvidenceV1::StructuredProvider,
            ObservationKindV1::HostRequestObserved {
                class: "Terminal".to_owned(),
                decision: HostRequestDecisionV1::Granted { by: HostDecisionAuthorityV1::Policy },
                outcome: HostRequestOutcomeV1::Failed {
                    error: "terminal/create spawn failed: os error 3".to_owned(),
                },
            },
        );

        let run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let support = ObservationSupportRegistry::default();
        let monitor = execute_operator_monitor(&harness, &observation, &support, &run_id)
            .unwrap();
        let detail = monitor.detail.as_ref().unwrap();
        assert_eq!(detail.block_facts.len(), 2);
        assert_eq!(detail.block_facts[0].state, BlockStateV1::Blocked);
        assert_eq!(
            detail.block_facts[0].label.as_deref(),
            Some("harness-gate: rule=deny-write"),
        );
        assert_eq!(
            detail.block_facts[1].label.as_deref(),
            Some("provider-quota: usage limit exceeded"),
        );
        assert_eq!(monitor.active_blocks, 2);

        monitor.validate_for(&run_id).unwrap();
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
        let mut harness = HarnessService::from_engine_for_test(
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
            &mut harness,
            &observation,
            &support,
            &binding,
            HarnessReadRequestV1::MonitorGet { run_id: Some(run_id.clone()) },
            &crate::runtime::HarnessRuntimeInventoryCache::default(),
        ).unwrap() else {
            panic!("monitor response");
        };
        let HarnessReadResponseV1::Timeline(timeline) = execute_exact_binding_read(
            &mut harness,
            &observation,
            &support,
            &binding,
            HarnessReadRequestV1::TimelineRead {
                run_id: Some(run_id.clone()),
                after_sequence: None,
                limit: HARNESS_TIMELINE_PAGE_LIMIT_MAX,
            },
            &crate::runtime::HarnessRuntimeInventoryCache::default(),
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

    /// D3/Slice C, end to end through `execute_exact_binding_read`: A mails
    /// B a `Run` ref naming A's own run plus a `ContextPack` ref. B's own
    /// grant (`HarnessReadPermissionsV1::default()`, same fixture default
    /// every grant in this file uses) has empty `read_visibility` -- B
    /// cannot see A's run by lineage at all -- yet `g4a_mail_fetch` resolves
    /// the `Run` ref because the message was addressed to B, never
    /// consulting that empty visibility. A third record, C, never addressed
    /// by the message, gets the named membership refusal
    /// (`MailNotForReader`) on the identical fetch; B's second ref
    /// (`ContextPack`, a digest no run in this fixture carries) is refused
    /// as `MailRefContextPackUnknown`.
    #[test]
    fn mail_fetch_crosses_lineage_for_the_addressed_reader_and_refuses_everyone_and_everything_else() {
        let path = observation_path("mail-fetch");
        let mut checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();
        // Sender A already exists in the base fixture (task/run/grant
        // 'a'*24, bound to "record-a"); flip on the mail permission it
        // starts without.
        checkpoint.grants[0].mail = true;

        let node_incarnation = HarnessSelectorV1::new(
            gate4agent_node_protocol::NodeIncarnationId::from_bytes([4; 16]).to_string(),
        ).unwrap();

        let recipient_task_id = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let recipient_run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let recipient_grant_id = SessionGrantId::new(format!("hgrant_{}", "b".repeat(24))).unwrap();
        let recipient_operation_id = HarnessOperationId::new(format!("hop_{}", "b".repeat(24))).unwrap();

        let stranger_task_id = HarnessTaskId::new(format!("htask_{}", "c".repeat(24))).unwrap();
        let stranger_run_id = HarnessRunId::new(format!("hrun_{}", "c".repeat(24))).unwrap();
        let stranger_grant_id = SessionGrantId::new(format!("hgrant_{}", "c".repeat(24))).unwrap();
        let stranger_operation_id = HarnessOperationId::new(format!("hop_{}", "c".repeat(24))).unwrap();

        for (task_id, run_id, grant_id, operation_id, record_id, digest_hex) in [
            (recipient_task_id, recipient_run_id.clone(), recipient_grant_id.clone(), recipient_operation_id, "record-b", 'b'),
            (stranger_task_id, stranger_run_id.clone(), stranger_grant_id.clone(), stranger_operation_id, "record-c", 'c'),
        ] {
            checkpoint.tasks.push(HarnessTaskV1 {
                task_id: task_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                title: "recipient fixture".to_owned(),
                body: String::new(),
                creator: HarnessActorV1::User { actor_id: selector("operator") },
                parent_task_id: None,
                dependencies: Vec::new(),
                state: HarnessTaskStateV1::Running,
                run_ids: vec![run_id.clone()],
                result_refs: Vec::new(),
                artifact_refs: Vec::new(),
                created_at_unix_ms: 10,
                updated_at_unix_ms: 10,
            });
            checkpoint.runs.push(HarnessRunV1 {
                run_id: run_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                parent_run_id: None,
                task_id: task_id.clone(),
                operation_id: operation_id.clone(),
                intent: HarnessRunIntentV1 {
                    node_id: selector("node-a"),
                    workspace_id: selector("workspace-a"),
                    worktree: HarnessWorktreeIntentV1::Existing,
                    provider_profile: selector("claude-default"),
                    mode: HarnessExecutionModeV1::Pty,
                    delivery_bundle: None,
                    continuation: None,
                },
                delivery_receipt: None,
                continuation_receipt: None,
                context_pack: None,
                git_facts: None,
                binding: Some(HarnessSessionBindingV1 {
                    node_id: selector("node-a"),
                    node_incarnation: node_incarnation.clone(),
                    workspace_id: selector("workspace-a"),
                    session: HarnessSessionIdentityV1::Managed {
                        record_id: selector(record_id),
                        active_session: Some(HarnessRuntimeIdentityV1 { instance_id: 7, generation: 1 }),
                    },
                }),
                lifecycle: HarnessRunLifecycleV1::Running,
                result_disposition: None,
                failure: None,
                created_at_unix_ms: 10,
                updated_at_unix_ms: 10,
            });
            checkpoint.grants.push(SessionGrantV1 {
                grant_id: grant_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                actor_run_id: run_id.clone(),
                allowed_targets: vec![HarnessGrantTargetV1 {
                    node_id: selector("node-a"),
                    workspace_id: selector("workspace-a"),
                    provider_profile: selector("claude-default"),
                    mode: HarnessExecutionModeV1::Pty,
                }],
                allowed_delivery_bundles: Vec::new(),
                maximum_child_count: 0,
                maximum_child_depth: 0,
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
                monitoring_visibility: HarnessMonitoringVisibilityV1::Timeline,
                context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
                mail: true,
                state: SessionGrantStateV1::Active,
                created_at_unix_ms: 10,
                updated_at_unix_ms: 11,
            });
            checkpoint.operations.push(HarnessOperationV1 {
                operation_id: operation_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                actor: HarnessActorV1::User { actor_id: selector("operator") },
                kind: HarnessOperationKindV1::CreateRun,
                state: HarnessOperationStateV1::Succeeded,
                task_id: Some(task_id),
                run_id: Some(run_id),
                grant_id: None,
                reconciles_operation_id: None,
                expected_revision: Some(HarnessRevision::new(1).unwrap()),
                request_digest: HarnessRequestDigest::new(digest_hex.to_string().repeat(64)).unwrap(),
                idempotency_ref: HarnessIdempotencyRef::new(format!(
                    "hidem_{}", digest_hex.to_string().repeat(24),
                )).unwrap(),
                failure: None,
                outcome_unknown_reason: None,
                reconciliation_outcome: None,
                created_at_unix_ms: 10,
                updated_at_unix_ms: 10,
                dispatched_at_unix_ms: Some(10),
                finished_at_unix_ms: Some(10),
            });
        }

        let mut harness = HarnessService::from_engine_for_test(
            gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap(),
        );
        let observation = ObservationService::open(&path).unwrap();
        let support = ObservationSupportRegistry::default();
        let inventory = crate::runtime::HarnessRuntimeInventoryCache::default();

        let sender_binding = crate::credential::tests::binding(1, 1);
        let sender_run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let recipient_binding = CredentialBindingV1 {
            grant_id: recipient_grant_id.clone(),
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: recipient_run_id,
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            node_incarnation: node_incarnation.clone(),
            record_id: selector("record-b"),
            instance_id: 7,
            generation: 1,
        };
        let stranger_binding = CredentialBindingV1 {
            grant_id: stranger_grant_id,
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: stranger_run_id,
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            node_incarnation,
            record_id: selector("record-c"),
            instance_id: 7,
            generation: 1,
        };

        let send_result = execute_exact_binding_read(
            &mut harness, &observation, &support, &sender_binding,
            HarnessReadRequestV1::MailSend {
                to: HarnessMailAddressV1::Session { record_id: selector("record-b") },
                subject: "handoff".to_owned(),
                body: "see the attached run".to_owned(),
                reply_to: None,
                refs: vec![
                    HarnessMailRefV1::Run { run_id: sender_run_id.clone() },
                    HarnessMailRefV1::ContextPack { digest: format!("sha256:{}", "a".repeat(64)) },
                ],
            },
            &inventory,
        ).unwrap();
        let HarnessReadResponseV1::MailSend(HarnessMailSendResultV1::Accepted { message_id }) = send_result else {
            panic!("expected an accepted send, got {send_result:?}");
        };

        // Proof this is a real lineage boundary, not a fixture accident: B's
        // own grant sees nothing at all.
        assert!(harness.engine().read_visibility(&recipient_grant_id).unwrap().is_empty());

        let fetched_run = execute_exact_binding_read(
            &mut harness, &observation, &support, &recipient_binding,
            HarnessReadRequestV1::MailFetch { message_id: message_id.clone(), ref_index: 0 },
            &inventory,
        ).unwrap();
        let HarnessReadResponseV1::MailFetch(HarnessMailFetchResultV1::Run { monitor, .. }) = fetched_run else {
            panic!("expected B to fetch the mailed run, got {fetched_run:?}");
        };
        assert_eq!(monitor.run_id, sender_run_id);

        let fetched_context_pack = execute_exact_binding_read(
            &mut harness, &observation, &support, &recipient_binding,
            HarnessReadRequestV1::MailFetch { message_id: message_id.clone(), ref_index: 1 },
            &inventory,
        ).unwrap();
        let HarnessReadResponseV1::MailFetch(HarnessMailFetchResultV1::MailRefContextPackUnknown {
            digest,
        }) = fetched_context_pack else {
            panic!("expected the ContextPack ref to be MailRefContextPackUnknown, got {fetched_context_pack:?}");
        };
        // No run in this fixture carries the fixture digest -- `MailRefTargetUnknown`
        // from the engine, mapped to this typed refusal, not free-text `Refused`.
        assert_eq!(digest, format!("sha256:{}", "a".repeat(64)));

        let fetched_by_stranger = execute_exact_binding_read(
            &mut harness, &observation, &support, &stranger_binding,
            HarnessReadRequestV1::MailFetch { message_id, ref_index: 0 },
            &inventory,
        ).unwrap();
        let HarnessReadResponseV1::MailFetch(HarnessMailFetchResultV1::Refused { reason }) = fetched_by_stranger else {
            panic!("expected the stranger's fetch to be refused, got {fetched_by_stranger:?}");
        };
        assert!(reason.contains("not addressed to this reader"), "{reason}");

        close_observation(observation, &path);
    }

    /// D5, Slice D, end to end through `execute_exact_binding_read`: A
    /// (create+mutate) creates a child task under its own task and it is
    /// visible via `g4a_tasks_get` (created_by_run attribution, D5's read
    /// side); A moves it Backlog -> Ready (`Moved`); A is refused moving its
    /// own task (`TaskIsOwn` -- a session never ends its own oversight); a
    /// stranger grant that DOES hold `mutate` is refused moving A's child
    /// because the child sits outside the stranger's own subtree
    /// (`TaskOutsideOwnSubtree`); a grant without `mutate` is refused at the
    /// `authorize_request` gate, before the engine is ever reached
    /// (`NotFoundOrDenied`).
    #[test]
    fn agent_task_create_and_move_stay_inside_the_callers_own_subtree() {
        let path = observation_path("agent-task-create-move");
        let mut checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();
        // A (task/run/grant 'a', from the base fixture) gets create+mutate
        // plus enough task read scope to see what it just created.
        checkpoint.grants[0].task_permissions.create = true;
        checkpoint.grants[0].task_permissions.mutate = true;
        checkpoint.grants[0].read_permissions.tasks = HarnessEntityReadScopeV1::SelfOnly;

        let node_incarnation = HarnessSelectorV1::new(
            gate4agent_node_protocol::NodeIncarnationId::from_bytes([4; 16]).to_string(),
        ).unwrap();

        let stranger_task_id = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let stranger_run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let stranger_grant_id = SessionGrantId::new(format!("hgrant_{}", "b".repeat(24))).unwrap();
        let stranger_operation_id = HarnessOperationId::new(format!("hop_{}", "b".repeat(24))).unwrap();

        let unmutated_task_id = HarnessTaskId::new(format!("htask_{}", "d".repeat(24))).unwrap();
        let unmutated_run_id = HarnessRunId::new(format!("hrun_{}", "d".repeat(24))).unwrap();
        let unmutated_grant_id = SessionGrantId::new(format!("hgrant_{}", "d".repeat(24))).unwrap();
        let unmutated_operation_id = HarnessOperationId::new(format!("hop_{}", "d".repeat(24))).unwrap();

        for (task_id, run_id, grant_id, operation_id, mutate, digest_hex) in [
            (stranger_task_id, stranger_run_id.clone(), stranger_grant_id.clone(), stranger_operation_id, true, 'b'),
            (unmutated_task_id, unmutated_run_id.clone(), unmutated_grant_id.clone(), unmutated_operation_id, false, 'd'),
        ] {
            checkpoint.tasks.push(HarnessTaskV1 {
                task_id: task_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                title: "stranger fixture".to_owned(),
                body: String::new(),
                creator: HarnessActorV1::User { actor_id: selector("operator") },
                parent_task_id: None,
                dependencies: Vec::new(),
                state: HarnessTaskStateV1::Running,
                run_ids: vec![run_id.clone()],
                result_refs: Vec::new(),
                artifact_refs: Vec::new(),
                created_at_unix_ms: 10,
                updated_at_unix_ms: 10,
            });
            checkpoint.runs.push(HarnessRunV1 {
                run_id: run_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                parent_run_id: None,
                task_id: task_id.clone(),
                operation_id: operation_id.clone(),
                intent: HarnessRunIntentV1 {
                    node_id: selector("node-a"),
                    workspace_id: selector("workspace-a"),
                    worktree: HarnessWorktreeIntentV1::Existing,
                    provider_profile: selector("claude-default"),
                    mode: HarnessExecutionModeV1::Pty,
                    delivery_bundle: None,
                    continuation: None,
                },
                delivery_receipt: None,
                continuation_receipt: None,
                context_pack: None,
                git_facts: None,
                binding: None,
                lifecycle: HarnessRunLifecycleV1::Running,
                result_disposition: None,
                failure: None,
                created_at_unix_ms: 10,
                updated_at_unix_ms: 10,
            });
            checkpoint.grants.push(SessionGrantV1 {
                grant_id: grant_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                actor_run_id: run_id.clone(),
                allowed_targets: vec![HarnessGrantTargetV1 {
                    node_id: selector("node-a"),
                    workspace_id: selector("workspace-a"),
                    provider_profile: selector("claude-default"),
                    mode: HarnessExecutionModeV1::Pty,
                }],
                allowed_delivery_bundles: Vec::new(),
                maximum_child_count: 0,
                maximum_child_depth: 0,
                operation_timeouts: HarnessOperationTimeoutsV1 {
                    dispatch_ms: 1_000,
                    wait_ms: 1_000,
                    reconciliation_ms: 1_000,
                },
                task_permissions: HarnessTaskPermissionsV1 {
                    read: true,
                    create: false,
                    mutate,
                    request_run: false,
                },
                read_permissions: HarnessReadPermissionsV1::default(),
                monitoring_visibility: HarnessMonitoringVisibilityV1::None,
                context_permissions: HarnessContextPermissionsV1 { export: false, restore: false },
                mail: false,
                state: SessionGrantStateV1::Active,
                created_at_unix_ms: 10,
                updated_at_unix_ms: 11,
            });
            checkpoint.operations.push(HarnessOperationV1 {
                operation_id: operation_id.clone(),
                revision: HarnessRevision::new(1).unwrap(),
                actor: HarnessActorV1::User { actor_id: selector("operator") },
                kind: HarnessOperationKindV1::CreateRun,
                state: HarnessOperationStateV1::Succeeded,
                task_id: Some(task_id),
                run_id: Some(run_id),
                grant_id: None,
                reconciles_operation_id: None,
                expected_revision: Some(HarnessRevision::new(1).unwrap()),
                request_digest: HarnessRequestDigest::new(digest_hex.to_string().repeat(64)).unwrap(),
                idempotency_ref: HarnessIdempotencyRef::new(format!(
                    "hidem_{}", digest_hex.to_string().repeat(24),
                )).unwrap(),
                failure: None,
                outcome_unknown_reason: None,
                reconciliation_outcome: None,
                created_at_unix_ms: 10,
                updated_at_unix_ms: 10,
                dispatched_at_unix_ms: Some(10),
                finished_at_unix_ms: Some(10),
            });
        }

        let mut harness = HarnessService::from_engine_for_test(
            gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap(),
        );
        let observation = ObservationService::open(&path).unwrap();
        let support = ObservationSupportRegistry::default();
        let inventory = crate::runtime::HarnessRuntimeInventoryCache::default();

        let own_binding = crate::credential::tests::binding(1, 1);
        let own_task_id = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();
        let stranger_binding = CredentialBindingV1 {
            grant_id: stranger_grant_id,
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: stranger_run_id,
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            node_incarnation: node_incarnation.clone(),
            record_id: selector("record-b"),
            instance_id: 7,
            generation: 1,
        };
        let unmutated_binding = CredentialBindingV1 {
            grant_id: unmutated_grant_id,
            grant_revision: HarnessRevision::new(1).unwrap(),
            actor_run_id: unmutated_run_id,
            node_id: selector("node-a"),
            workspace_id: selector("workspace-a"),
            node_incarnation,
            record_id: selector("record-d"),
            instance_id: 7,
            generation: 1,
        };

        let created = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::TaskCreate {
                title: "child work".to_owned(),
                body: "narrow scope".to_owned(),
                parent_task_id: None,
            },
            &inventory,
        ).unwrap();
        let HarnessReadResponseV1::TaskCreate(HarnessTaskCreateResultV1::Created {
            task_id: child_task_id, revision: created_revision,
        }) = created else {
            panic!("expected Created, got {created:?}");
        };
        assert_eq!(created_revision, HarnessRevision::new(1).unwrap());

        let fetched = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::TaskGet { task_id: child_task_id.clone() },
            &inventory,
        ).unwrap();
        let HarnessReadResponseV1::Task(fetched_task) = fetched else {
            panic!("expected Task, got {fetched:?}");
        };
        assert_eq!(fetched_task.task_id, child_task_id);
        assert_eq!(fetched_task.state, HarnessTaskStateV1::Backlog);

        let moved = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::TaskMove {
                task_id: child_task_id.clone(),
                expected_revision: HarnessRevision::new(1).unwrap(),
                to: HarnessTaskStateV1::Ready,
            },
            &inventory,
        ).unwrap();
        assert_eq!(
            moved,
            HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::Moved {
                task_id: child_task_id.clone(),
                revision: HarnessRevision::new(2).unwrap(),
                from: HarnessTaskStateV1::Backlog,
                to: HarnessTaskStateV1::Ready,
            }),
        );

        let own_task_refused = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::TaskMove {
                task_id: own_task_id.clone(),
                expected_revision: HarnessRevision::new(1).unwrap(),
                to: HarnessTaskStateV1::Ready,
            },
            &inventory,
        ).unwrap();
        assert_eq!(
            own_task_refused,
            HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::TaskIsOwn { task_id: own_task_id }),
        );

        let stranger_refused = execute_exact_binding_read(
            &mut harness, &observation, &support, &stranger_binding,
            HarnessReadRequestV1::TaskMove {
                task_id: child_task_id.clone(),
                expected_revision: HarnessRevision::new(2).unwrap(),
                to: HarnessTaskStateV1::Review,
            },
            &inventory,
        ).unwrap();
        let HarnessReadResponseV1::TaskMove(HarnessTaskMoveResultV1::TaskOutsideOwnSubtree {
            task_id: refused_task_id,
            own_task_id: stranger_own_task_id,
        }) = stranger_refused else {
            panic!("expected TaskOutsideOwnSubtree, got {stranger_refused:?}");
        };
        assert_eq!(refused_task_id, child_task_id);
        assert_eq!(stranger_own_task_id.as_str(), format!("htask_{}", "b".repeat(24)));

        let gated = execute_exact_binding_read(
            &mut harness, &observation, &support, &unmutated_binding,
            HarnessReadRequestV1::TaskMove {
                task_id: child_task_id,
                expected_revision: HarnessRevision::new(2).unwrap(),
                to: HarnessTaskStateV1::Review,
            },
            &inventory,
        );
        assert_eq!(gated, Err(HarnessReadHostErrorV1::NotFoundOrDenied));

        close_observation(observation, &path);
    }

    /// Builds a second, wholly independent task/run/grant ("run B") on top
    /// of `crate::credential::tests::engine`'s own base fixture ("run A" --
    /// `htask_aaa.../hrun_aaa.../hgrant_aaa...`), for the authority test:
    /// `g4a_run_finish` resolves its identity from the caller's OWN grant
    /// binding alone, so a session bound to run A must never be able to
    /// touch run B's state, and there is no argument that could even name
    /// it.
    fn push_other_run(
        checkpoint: &mut gate4agent_harness_engine::HarnessEngineCheckpointV1,
    ) -> (HarnessTaskId, HarnessRunId) {
        let other_task_id = HarnessTaskId::new(format!("htask_{}", "b".repeat(24))).unwrap();
        let other_run_id = HarnessRunId::new(format!("hrun_{}", "b".repeat(24))).unwrap();
        let other_operation_id = HarnessOperationId::new(format!("hop_{}", "b".repeat(24))).unwrap();
        checkpoint.tasks.push(HarnessTaskV1 {
            task_id: other_task_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            title: "a different session's own work".to_owned(),
            body: String::new(),
            creator: HarnessActorV1::User { actor_id: selector("operator") },
            parent_task_id: None,
            dependencies: Vec::new(),
            state: HarnessTaskStateV1::Running,
            run_ids: vec![other_run_id.clone()],
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        });
        checkpoint.runs.push(HarnessRunV1 {
            run_id: other_run_id.clone(),
            revision: HarnessRevision::new(1).unwrap(),
            parent_run_id: None,
            task_id: other_task_id.clone(),
            operation_id: other_operation_id,
            intent: HarnessRunIntentV1 {
                node_id: selector("node-a"),
                workspace_id: selector("workspace-a"),
                worktree: HarnessWorktreeIntentV1::Existing,
                provider_profile: selector("claude-default"),
                mode: HarnessExecutionModeV1::Pty,
                delivery_bundle: None,
                continuation: None,
            },
            delivery_receipt: None,
            continuation_receipt: None,
            context_pack: None,
            git_facts: None,
            binding: None,
            lifecycle: HarnessRunLifecycleV1::Running,
            result_disposition: None,
            failure: None,
            created_at_unix_ms: 10,
            updated_at_unix_ms: 10,
        });
        (other_task_id, other_run_id)
    }

    /// `done` moves the caller's OWN run to `Completed`/`Succeeded` and its
    /// own task to `Review` -- never `Done`, the review gate a session
    /// cannot skip. A second `g4a_run_finish` call against the now-terminal
    /// run is refused by name (`AlreadyFinished`), never silently accepted
    /// as if it were the first call. Run B -- a wholly different session's
    /// task/run/grant -- is untouched throughout: this is the authority
    /// test, since there is no run-id argument for a caller to misuse in
    /// the first place.
    #[test]
    fn agent_run_finish_done_moves_own_run_to_review_never_done_and_a_repeat_is_refused_by_name() {
        let path = observation_path("agent-run-finish-done");
        let mut checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();
        let (other_task_id, other_run_id) = push_other_run(&mut checkpoint);

        let mut harness = HarnessService::from_engine_for_test(
            gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap(),
        );
        let observation = ObservationService::open(&path).unwrap();
        let support = ObservationSupportRegistry::default();
        let inventory = crate::runtime::HarnessRuntimeInventoryCache::default();
        let own_binding = crate::credential::tests::binding(1, 1);
        let own_run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let own_task_id = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();

        let finished = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::RunFinish { outcome: HarnessRunFinishOutcomeV1::Done, summary: None },
            &inventory,
        ).unwrap();
        assert_eq!(
            finished,
            HarnessReadResponseV1::RunFinish(HarnessRunFinishResultV1::Finished {
                run_id: own_run_id.clone(),
                task_id: own_task_id.clone(),
                result: HarnessRunFinishOutcomeV1::Done,
            }),
        );
        let run = harness.engine().run(&own_run_id).unwrap();
        assert_eq!(run.lifecycle, HarnessRunLifecycleV1::Completed);
        assert_eq!(run.result_disposition, Some(HarnessResultDispositionV1::Succeeded));
        let task = harness.engine().task(&own_task_id).unwrap();
        assert_eq!(task.state, HarnessTaskStateV1::Review);
        assert_ne!(task.state, HarnessTaskStateV1::Done);

        let repeated = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::RunFinish { outcome: HarnessRunFinishOutcomeV1::Done, summary: None },
            &inventory,
        ).unwrap();
        assert_eq!(
            repeated,
            HarnessReadResponseV1::RunFinish(HarnessRunFinishResultV1::AlreadyFinished {
                run_id: own_run_id,
                lifecycle: HarnessRunLifecycleV1::Completed,
            }),
        );

        let other_run = harness.engine().run(&other_run_id).unwrap();
        assert_eq!(other_run.lifecycle, HarnessRunLifecycleV1::Running);
        assert!(other_run.result_disposition.is_none());
        let other_task = harness.engine().task(&other_task_id).unwrap();
        assert_eq!(other_task.state, HarnessTaskStateV1::Running);

        close_observation(observation, &path);
    }

    /// `failed` moves the run to `Failed` and records a `Rejected`,
    /// `retryable: true` failure -- a definitive self-report, never
    /// `Internal`'s "the host hit an unexpected fault."
    #[test]
    fn agent_run_finish_failed_records_a_retryable_rejected_failure() {
        let path = observation_path("agent-run-finish-failed");
        let checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();
        let mut harness = HarnessService::from_engine_for_test(
            gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap(),
        );
        let observation = ObservationService::open(&path).unwrap();
        let support = ObservationSupportRegistry::default();
        let inventory = crate::runtime::HarnessRuntimeInventoryCache::default();
        let own_binding = crate::credential::tests::binding(1, 1);
        let own_run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();
        let own_task_id = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();

        let finished = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::RunFinish { outcome: HarnessRunFinishOutcomeV1::Failed, summary: None },
            &inventory,
        ).unwrap();
        assert_eq!(
            finished,
            HarnessReadResponseV1::RunFinish(HarnessRunFinishResultV1::Finished {
                run_id: own_run_id.clone(),
                task_id: own_task_id.clone(),
                result: HarnessRunFinishOutcomeV1::Failed,
            }),
        );
        let run = harness.engine().run(&own_run_id).unwrap();
        assert_eq!(run.lifecycle, HarnessRunLifecycleV1::Failed);
        assert_eq!(run.result_disposition, Some(HarnessResultDispositionV1::Failed));
        assert_eq!(
            run.failure,
            Some(HarnessFailureV1 { category: HarnessFailureCategoryV1::Rejected, retryable: true }),
        );
        let task = harness.engine().task(&own_task_id).unwrap();
        assert_eq!(task.state, HarnessTaskStateV1::Failed);

        close_observation(observation, &path);
    }

    /// A `summary` produces exactly one mail to the task's own forum when
    /// the grant holds `mail`, addressed and worded as the finish demands.
    #[test]
    fn agent_run_finish_summary_posts_one_mail_to_the_task_forum() {
        let path = observation_path("agent-run-finish-summary-mail");
        let mut checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();
        checkpoint.grants[0].mail = true;
        let mut harness = HarnessService::from_engine_for_test(
            gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap(),
        );
        let observation = ObservationService::open(&path).unwrap();
        let support = ObservationSupportRegistry::default();
        let inventory = crate::runtime::HarnessRuntimeInventoryCache::default();
        let own_binding = crate::credential::tests::binding(1, 1);
        let own_task_id = HarnessTaskId::new(format!("htask_{}", "a".repeat(24))).unwrap();

        let finished = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::RunFinish {
                outcome: HarnessRunFinishOutcomeV1::Done,
                summary: Some("handed off cleanly".to_owned()),
            },
            &inventory,
        ).unwrap();
        assert!(matches!(
            finished,
            HarnessReadResponseV1::RunFinish(HarnessRunFinishResultV1::Finished { .. }),
        ));
        let mail_messages = harness.engine().checkpoint().mail_messages;
        assert_eq!(mail_messages.len(), 1);
        assert_eq!(mail_messages[0].to, HarnessMailAddressV1::Task { task_id: own_task_id });
        assert_eq!(mail_messages[0].body, "handed off cleanly");

        close_observation(observation, &path);
    }

    /// A `summary` that cannot be posted (here: the grant holds no `mail`
    /// permission at all, so the engine itself refuses the send) never
    /// undoes or blocks the finish that already committed -- the run still
    /// lands `Completed`/`Succeeded` and no mail is recorded, but the call
    /// still reports `Finished`, not an error.
    #[test]
    fn agent_run_finish_survives_a_summary_mail_that_cannot_be_posted() {
        let path = observation_path("agent-run-finish-summary-mail-refused");
        let checkpoint = crate::credential::tests::engine(
            1,
            SessionGrantStateV1::Active,
            1,
            HarnessRunLifecycleV1::Running,
        ).checkpoint();
        assert!(!checkpoint.grants[0].mail, "fixture must start without mail permission");
        let mut harness = HarnessService::from_engine_for_test(
            gate4agent_harness_engine::HarnessEngine::restore(checkpoint).unwrap(),
        );
        let observation = ObservationService::open(&path).unwrap();
        let support = ObservationSupportRegistry::default();
        let inventory = crate::runtime::HarnessRuntimeInventoryCache::default();
        let own_binding = crate::credential::tests::binding(1, 1);
        let own_run_id = HarnessRunId::new(format!("hrun_{}", "a".repeat(24))).unwrap();

        let finished = execute_exact_binding_read(
            &mut harness, &observation, &support, &own_binding,
            HarnessReadRequestV1::RunFinish {
                outcome: HarnessRunFinishOutcomeV1::Done,
                summary: Some("nobody will ever read this".to_owned()),
            },
            &inventory,
        ).unwrap();
        assert!(matches!(
            finished,
            HarnessReadResponseV1::RunFinish(HarnessRunFinishResultV1::Finished { .. }),
        ));
        let run = harness.engine().run(&own_run_id).unwrap();
        assert_eq!(run.lifecycle, HarnessRunLifecycleV1::Completed);
        assert_eq!(run.result_disposition, Some(HarnessResultDispositionV1::Succeeded));
        assert!(harness.engine().checkpoint().mail_messages.is_empty());

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
