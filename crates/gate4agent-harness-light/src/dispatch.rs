//! Dispatches one authorized `HarnessOperatorRequestV1` to its light-harness
//! handling and always produces a typed `HarnessOperatorReplyV1` -- never a
//! hard error; every rejection (typed `NotFound`/`Unsupported`/relay
//! failure) is itself the reply. Request coverage, per the app-harness
//! protocol contract (A1 session verbs + A2 workspace/history/record/
//! management/terminal + A3 event subscription; task-kernel mutations
//! remain `Unsupported` by canon -- light mode has no kernel to mutate):
//!
//! - **Never reaches this dispatcher**: `SubscribeEvents` (A3) branches out
//!   of the ordinary one-shot request/reply path in `lib.rs`'s
//!   `handle_connection`, before dispatch is ever called -- see that
//!   function's own doc comment and `crate::inventory::refresh_route`/
//!   `reconcile_topology` for where the roster changes it pushes
//!   (`RuntimeInventoryChanged`/`RuntimeInventoryRemoved`) originate.
//! - **Served** from the maintained runtime inventory: `RuntimeInventoryList`.
//! - **Served** from the maintained terminal ring (`crate::terminal`):
//!   `TerminalRead`.
//! - **Relayed** straight to the C2/Node verb (`crate::relay`):
//!   - Session verbs (A1): `SpawnSession`, `WriteSessionInput`,
//!     `ResizeSession`, `StopSession`, `ControlSession`, `WriteSessionBytes`,
//!     `PasteSession`, `RemoveSession`, `ResumeSession`.
//!   - Node-scoped workspace (A2): `InspectNodeWorkspace`,
//!     `ReadNodeWorkspaceFile`, `ReadNodeGitHistory`, `ReadNodeGitDiff`,
//!     `WriteNodeWorkspaceFile`, `CreateNodeWorkspaceFile`,
//!     `CreateNodeWorkspaceDirectory` (CAS conflict on a stale
//!     `expected_revision` surfaces as a typed `Conflict`).
//!   - Native history (A2): `CatalogNativeSessions`, `PageNativeSessions`,
//!     `PreviewNativeSession`, `PreviewSessionRecord`.
//!   - Session-record mutations (A2): `ResumeSessionRecord`,
//!     `RenameSessionRecord`, `SetSessionTask`, `ForgetSessionRecord`,
//!     `IndexProviderSession`, `IndexNativeSession` -- every settled mutation
//!     eagerly refreshes the affected node's runtime-inventory entry (the A1
//!     pattern, see `crate::relay`'s module doc comment).
//!   - Management (A2): `RegisterWorkspace`, `UnregisterWorkspace`,
//!     `CreateStandaloneWorkspace`, `CreateWorktree`, `RemoveWorktree`
//!     (workspace/worktree lifecycle -- refreshes the route on success),
//!     `ExportContextPack`, `ForgetContextPack` (no roster effect, no
//!     refresh), `BrowseHostDirectories`.
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
//! - **Typed `Unsupported`**: the task-kernel mutation family (`SubmitIntent`
//!   and its ten authorized siblings `CreateTask`..`StartTaskV2`) -- light
//!   mode has no task kernel to mutate, by canon. Every `Unsupported`
//!   rejection logs the operation name.

use gate4agent_harness_api::{
    HarnessOperatorHostErrorV1, HarnessOperatorReplyV1, HarnessOperatorRequestV1,
    HarnessOperatorResponseV1, RunPageV1, TaskPageV1,
};

use crate::relay::{
    self, NodeWorkspaceReadRequest, NodeWorkspaceWriteRequest, ResourceMutationRequest,
    SessionRecordMutationRequest, SessionVerb,
};
use crate::LightState;

pub(crate) async fn handle_request(
    state: &LightState,
    request: HarnessOperatorRequestV1,
    // This connection's own declared `HarnessOperatorEnvelopeV1::version`,
    // read by `lib.rs`'s `handle_connection` before `request` moves here.
    // Threaded only to `TerminalRead` (the one response this dispatcher
    // builds that carries a version-gated field, `HarnessRuntimeTerminalFrameV1
    // ::screen_state`) -- see `terminal_frame_to_wire`.
    wire_version: u16,
) -> HarnessOperatorReplyV1 {
    let operation = operation_name(&request);
    match request {
        HarnessOperatorRequestV1::RuntimeInventoryList { after_node_id, limit } => {
            crate::inventory::list(&state.inventory, after_node_id, limit, wire_version).await
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

        HarnessOperatorRequestV1::InspectNodeWorkspace { node_id, workspace_id } => {
            relay::node_workspace_read(state, node_id, workspace_id, NodeWorkspaceReadRequest::Inspect).await
        }
        HarnessOperatorRequestV1::ReadNodeWorkspaceFile { node_id, workspace_id, path } => {
            relay::node_workspace_read(
                state, node_id, workspace_id, NodeWorkspaceReadRequest::File { path },
            ).await
        }
        HarnessOperatorRequestV1::ReadNodeGitHistory { node_id, workspace_id, path, before, limit } => {
            relay::node_workspace_read(
                state, node_id, workspace_id,
                NodeWorkspaceReadRequest::GitHistory { path, before, limit },
            ).await
        }
        HarnessOperatorRequestV1::ReadNodeGitDiff { node_id, workspace_id, mode, path } => {
            relay::node_workspace_read(
                state, node_id, workspace_id, NodeWorkspaceReadRequest::GitDiff { mode, path },
            ).await
        }
        HarnessOperatorRequestV1::WriteNodeWorkspaceFile {
            node_id, workspace_id, path, content, expected_revision,
        } => {
            relay::node_workspace_write(
                state, node_id, workspace_id,
                NodeWorkspaceWriteRequest::File { path, content, expected_revision },
            ).await
        }
        HarnessOperatorRequestV1::CreateNodeWorkspaceFile { node_id, workspace_id, path } => {
            relay::node_workspace_write(
                state, node_id, workspace_id, NodeWorkspaceWriteRequest::CreateFile { path },
            ).await
        }
        HarnessOperatorRequestV1::CreateNodeWorkspaceDirectory { node_id, workspace_id, path } => {
            relay::node_workspace_write(
                state, node_id, workspace_id, NodeWorkspaceWriteRequest::CreateDirectory { path },
            ).await
        }

        HarnessOperatorRequestV1::CatalogNativeSessions { route, limit } => {
            relay::catalog_native_sessions(state, route, limit).await
        }
        HarnessOperatorRequestV1::PageNativeSessions {
            route, window, catalog_revision, recent_cutoff_unix_ms, after_selection_id, limit,
        } => {
            relay::page_native_sessions(
                state, route, window, catalog_revision, recent_cutoff_unix_ms, after_selection_id, limit,
            ).await
        }
        HarnessOperatorRequestV1::PreviewNativeSession { selection, message_limit } => {
            relay::preview_native_session(state, selection, message_limit).await
        }
        HarnessOperatorRequestV1::PreviewSessionRecord { node_id, record_id, message_limit } => {
            relay::preview_session_record(state, node_id, record_id, message_limit).await
        }

        HarnessOperatorRequestV1::ResumeSessionRecord { node_id, record_id, terminal_size, initial_prompt } => {
            relay::session_record_mutation(
                state,
                SessionRecordMutationRequest::Resume { node_id, record_id, terminal_size, initial_prompt },
            ).await
        }
        HarnessOperatorRequestV1::RenameSessionRecord { node_id, record_id, display_name } => {
            relay::session_record_mutation(
                state, SessionRecordMutationRequest::Rename { node_id, record_id, display_name },
            ).await
        }
        HarnessOperatorRequestV1::SetSessionTask { node_id, record_id, expected_revision, target } => {
            relay::session_record_mutation(
                state,
                SessionRecordMutationRequest::SetTask { node_id, record_id, expected_revision, target },
            ).await
        }
        HarnessOperatorRequestV1::ForgetSessionRecord { node_id, record_id } => {
            relay::session_record_mutation(
                state, SessionRecordMutationRequest::Forget { node_id, record_id },
            ).await
        }
        HarnessOperatorRequestV1::IndexProviderSession {
            node_id, workspace_id, provider, identity, display_name,
        } => {
            relay::session_record_mutation(
                state,
                SessionRecordMutationRequest::IndexProvider {
                    node_id, workspace_id, provider, identity, display_name,
                },
            ).await
        }
        HarnessOperatorRequestV1::IndexNativeSession { selection, display_name } => {
            relay::session_record_mutation(
                state, SessionRecordMutationRequest::IndexNative { selection, display_name },
            ).await
        }

        HarnessOperatorRequestV1::RegisterWorkspace { node_id, workspace_id, root } => {
            relay::resource_mutation(
                state, ResourceMutationRequest::RegisterWorkspace { node_id, workspace_id, root },
            ).await
        }
        HarnessOperatorRequestV1::UnregisterWorkspace { node_id, workspace_id } => {
            relay::resource_mutation(
                state, ResourceMutationRequest::UnregisterWorkspace { node_id, workspace_id },
            ).await
        }
        HarnessOperatorRequestV1::CreateStandaloneWorkspace { node_id, workspace_id, root, initial_branch } => {
            relay::resource_mutation(
                state,
                ResourceMutationRequest::CreateStandaloneWorkspace {
                    node_id, workspace_id, root, initial_branch,
                },
            ).await
        }
        HarnessOperatorRequestV1::CreateWorktree {
            node_id, source_workspace_id, workspace_id, target_root, branch, base,
        } => {
            relay::resource_mutation(
                state,
                ResourceMutationRequest::CreateWorktree {
                    node_id, source_workspace_id, workspace_id, target_root, branch, base,
                },
            ).await
        }
        HarnessOperatorRequestV1::RemoveWorktree { node_id, source_workspace_id, target_root } => {
            relay::resource_mutation(
                state,
                ResourceMutationRequest::RemoveWorktree { node_id, source_workspace_id, target_root },
            ).await
        }
        HarnessOperatorRequestV1::ExportContextPack { session } => {
            relay::resource_mutation(state, ResourceMutationRequest::ExportContextPack { session }).await
        }
        HarnessOperatorRequestV1::ForgetContextPack { node_id, context_id } => {
            relay::resource_mutation(
                state, ResourceMutationRequest::ForgetContextPack { node_id, context_id },
            ).await
        }
        HarnessOperatorRequestV1::BrowseHostDirectories { node_id, directory, after } => {
            relay::browse_host_directories(state, node_id, directory, after).await
        }

        HarnessOperatorRequestV1::TerminalRead { session, after_sequence, limit } => {
            crate::terminal::read(&state.terminal, session, after_sequence, limit, wire_version).await
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
