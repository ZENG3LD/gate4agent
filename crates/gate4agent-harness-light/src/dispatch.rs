//! Dispatches one authorized `HarnessOperatorRequestV1` to its A1-scope
//! handling and always produces a typed `HarnessOperatorReplyV1` -- never a
//! hard error; every rejection (typed `NotFound`/`Unsupported`/relay
//! failure) is itself the reply. Request coverage, per the A1 contract
//! slice:
//!
//! - **Served** from the maintained runtime inventory: `RuntimeInventoryList`.
//! - **Relayed** straight to the C2/Node verb (`crate::relay`): `SpawnSession`,
//!   `WriteSessionInput`, `ResizeSession`, `StopSession`, `ControlSession`,
//!   `WriteSessionBytes`, `PasteSession`, `RemoveSession`, `ResumeSession`.
//! - **Empty pages**: `TasksList`/`RunsList` -- light mode has no task
//!   kernel, so there is no kanban board to page over, by canon (see the
//!   app-harness protocol contract's own "Stays in the app" / "Dead by
//!   design" classification).
//! - **Typed `NotFound`**: every other task/run-scoped read (`TaskGet`,
//!   `RunGet`, `MonitorGet`, `TimelineRead`, `RunCorrelationGet`,
//!   `RunTransferGet`, `ReverseAttributionGet`, `ObserveRunContextSource`,
//!   `InspectRunWorkspace`, `ReadRunWorkspaceFile`, `ReadRunGitHistory`,
//!   `ReadRunGitDiff`, `LaunchPlansList`, `TaskExecutionSpecGet`,
//!   `TaskLaunchOptionsGet`) -- honestly true in light mode: no task/run by
//!   that id (or any id) will ever exist, so `NotFound` is the correct
//!   answer, not a placeholder.
//! - **Typed `Unsupported`** (new in this slice, see `gate4agent-harness-api`):
//!   everything else -- the task-kernel mutation family (`SubmitIntent` and
//!   its ten authorized siblings `CreateTask`..`StartTaskV2`), the node-
//!   workspace read/write family, the native-history and session-record
//!   families, the resource-mutation family, `TerminalRead`
//!   (`TerminalBufferRegistry` is deliberately deferred past A1), and
//!   `SubscribeEvents` (no push-subscription surface in A1 either). Every
//!   `Unsupported` rejection logs the operation name.

use gate4agent_harness_api::{
    HarnessOperatorHostErrorV1, HarnessOperatorReplyV1, HarnessOperatorRequestV1,
    HarnessOperatorResponseV1, RunPageV1, TaskPageV1,
};

use crate::relay::{self, SessionVerb};
use crate::LightState;

pub(crate) async fn handle_request(
    state: &LightState,
    request: HarnessOperatorRequestV1,
) -> HarnessOperatorReplyV1 {
    let operation = operation_name(&request);
    match request {
        HarnessOperatorRequestV1::RuntimeInventoryList { after_node_id, limit } => {
            crate::inventory::list(&state.inventory, after_node_id, limit).await
        }

        HarnessOperatorRequestV1::SpawnSession {
            node_id,
            workspace_id,
            provider,
            provider_profile,
            mode,
            terminal_size,
        } => {
            relay::spawn_session(state, node_id, workspace_id, provider, provider_profile, mode, terminal_size)
                .await
        }
        HarnessOperatorRequestV1::WriteSessionInput { session, text } => {
            relay::session_control(state, session, SessionVerb::Input { text }).await
        }
        HarnessOperatorRequestV1::ResizeSession { session, terminal_size } => {
            relay::session_control(state, session, SessionVerb::Resize { terminal_size }).await
        }
        HarnessOperatorRequestV1::StopSession { session, force } => {
            relay::session_control(state, session, SessionVerb::Stop { force }).await
        }
        HarnessOperatorRequestV1::ControlSession { session, control } => {
            relay::session_control(state, session, SessionVerb::Control { control }).await
        }
        HarnessOperatorRequestV1::WriteSessionBytes { session, bytes } => {
            relay::session_control(state, session, SessionVerb::Bytes { bytes }).await
        }
        HarnessOperatorRequestV1::PasteSession { session, text } => {
            relay::session_control(state, session, SessionVerb::Paste { text }).await
        }
        HarnessOperatorRequestV1::RemoveSession { session } => {
            relay::session_control(state, session, SessionVerb::Remove).await
        }
        HarnessOperatorRequestV1::ResumeSession { session, terminal_size } => {
            relay::session_control(state, session, SessionVerb::Resume { terminal_size }).await
        }

        HarnessOperatorRequestV1::TasksList { .. } => {
            tracing::debug!(operation, "harness-light: served empty (no task kernel in light mode)");
            HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::Tasks(TaskPageV1 { tasks: Vec::new(), next_cursor: None }),
            }
        }
        HarnessOperatorRequestV1::RunsList { .. } => {
            tracing::debug!(operation, "harness-light: served empty (no task kernel in light mode)");
            HarnessOperatorReplyV1::Ok {
                response: HarnessOperatorResponseV1::Runs(RunPageV1 { runs: Vec::new(), next_cursor: None }),
            }
        }

        HarnessOperatorRequestV1::TaskGet { .. }
        | HarnessOperatorRequestV1::RunGet { .. }
        | HarnessOperatorRequestV1::MonitorGet { .. }
        | HarnessOperatorRequestV1::TimelineRead { .. }
        | HarnessOperatorRequestV1::RunCorrelationGet { .. }
        | HarnessOperatorRequestV1::RunTransferGet { .. }
        | HarnessOperatorRequestV1::ReverseAttributionGet { .. }
        | HarnessOperatorRequestV1::ObserveRunContextSource { .. }
        | HarnessOperatorRequestV1::InspectRunWorkspace { .. }
        | HarnessOperatorRequestV1::ReadRunWorkspaceFile { .. }
        | HarnessOperatorRequestV1::ReadRunGitHistory { .. }
        | HarnessOperatorRequestV1::ReadRunGitDiff { .. }
        | HarnessOperatorRequestV1::LaunchPlansList { .. }
        | HarnessOperatorRequestV1::TaskExecutionSpecGet { .. }
        | HarnessOperatorRequestV1::TaskLaunchOptionsGet { .. } => {
            tracing::warn!(
                operation,
                "harness-light: task-kernel read rejected (light mode has no task kernel)",
            );
            HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::NotFound }
        }

        _ => {
            tracing::warn!(operation, "harness-light: request not supported in this slice");
            HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Unsupported }
        }
    }
}

/// Reads the wire `kind` tag straight off the request's own serde
/// representation rather than hand-matching every variant name a second
/// time, so the logged operation can never drift from the wire discriminant
/// as request variants are added -- the same technique
/// `gate4agent-harness-service::runtime`'s (private)
/// `OperatorRequestLogIdentity::describe` already uses for the same reason.
fn operation_name(request: &HarnessOperatorRequestV1) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("kind").and_then(|kind| kind.as_str().map(str::to_owned)))
        .unwrap_or_else(|| "unknown".to_owned())
}
