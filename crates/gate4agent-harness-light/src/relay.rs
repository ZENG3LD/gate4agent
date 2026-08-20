//! Relays the nine direct operator session verbs (`SpawnSession` plus the
//! eight thin session-control verbs) straight to C2/Node, mirroring the
//! `NodeRequest` shapes `gate4agent-harness-service::c2`'s
//! `HarnessC2Adapter::dispatch_session_spawn`/`PreparedSessionControl`
//! already use for the same operator wire verbs -- see the crate-level
//! report for why those are reimplemented light-local rather than reused:
//! both are `pub(crate)` to that crate and woven into its SWC-kernel spawn-
//! lease/profile-binding bookkeeping (`bind_prepared_spawn_profile`,
//! `validate_prepared_spawn`, `ensure_current_incarnation`'s kernel-side
//! callers), which light mode has nothing equivalent to and does not want.
//!
//! Every roster-affecting verb (`SpawnSession`, `StopSession`,
//! `RemoveSession`, `ResumeSession`) eagerly refreshes the affected node's
//! runtime-inventory entry before replying, mirroring the app-harness
//! protocol contract's own mutation-discipline principle ("roster-affecting
//! successes trigger targeted inventory invalidation so subscribers
//! converge") -- here, "converge" means the very next `RuntimeInventoryList`
//! already reflects it, since light mode has no push-subscription surface
//! in A1.

use std::time::Duration;

use gate4agent_c2_protocol::{C2NodeResponse, NodeRequest, NodeRoute};
use gate4agent_harness_api::{
    HarnessExecutionModeV1, HarnessGitDiffModeV1, HarnessGitObjectIdV1, HarnessHostPathV1,
    HarnessNativeSessionCatalogWindowV1, HarnessNativeSessionRouteV1,
    HarnessNativeSessionSelectionV1, HarnessOperatorReplyV1, HarnessOperatorRequestV1,
    HarnessOperatorResponseV1, HarnessProviderSessionIdentityV1, HarnessRepositoryPathV1,
    HarnessRuntimeSessionAddressV1, HarnessRuntimeTerminalSizeV1, HarnessSelectorV1,
    HarnessSessionTaskTargetV1, HarnessTerminalControlV1, HarnessWorkspaceFileRevisionV1,
};
use gate4agent_harness_service::c2::{
    correlate_native_history_response, correlate_node_workspace_read_response,
    correlate_node_workspace_write_response, correlate_resource_mutation_response,
    correlate_session_record_mutation_response, native_history_wire_route,
    native_history_wire_selection, native_history_wire_window, project_host_directory_listing,
    provider_session_identity_from_api, session_task_target_from_api, PreparedNodeWorkspaceRead,
    PreparedNodeWorkspaceWrite, PreparedResourceMutation, PreparedSessionRecordMutation,
    ResourceMutationKind, SessionRecordMutationKind, WorkspaceReadKind, WorkspaceWriteKind,
};
use gate4agent_node_protocol::{
    GitDiffMode, GitDiffRequest, GitObjectId, NodeId, NodeIncarnationId, OpaqueHostPath,
    RepositoryPath, SessionAddress, SessionKey, SessionMode, SessionRecordId, SpawnContextId,
    SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride, SpawnOverrides, SpawnProfileId,
    SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorkspaceFileRevision, WorkspaceId,
    CapabilityId, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node_wire::random_nonce;
use gate4agent_types::{AgentId, AgentInstanceId, SessionGeneration, TerminalControl, TerminalSize};
use tokio::time::timeout;

use crate::c2::{exact_route, fetch_snapshot_serialized};
use crate::error::LightRelayError;
use crate::inventory::refresh_route;
use crate::util::encode_hex;
use crate::LightState;

/// Node-local processing budget for a direct `SpawnSession` dispatch.
/// Mirrors `gate4agent-harness-service::c2`'s own (private)
/// `SESSION_SPAWN_DEADLINE_MS` -- a plain literal with no kernel dependency,
/// so duplicating it here (rather than promoting a private `const`) keeps
/// this crate's spawn deadline in step with the full harness's own direct-
/// spawn deadline without adding a coupling either side has to maintain.
const SESSION_SPAWN_DEADLINE_MS: u64 = 20_000;

/// Direct operator spawn: no Task/Run/launch-plan catalog, matching
/// `HarnessOperatorRequestV1::SpawnSession`'s own doc comment. Preflights
/// the requested provider profile against a fresh node snapshot (the same
/// `launch_inventory.spawn_profiles` lookup `preflight_spawn_profile` uses)
/// so a stale/unknown profile is a typed `NotFound`, not a node-side crash.
pub(crate) async fn spawn_session(
    state: &LightState,
    node_id: String,
    workspace_id: String,
    provider: String,
    provider_profile: String,
    mode: HarnessExecutionModeV1,
    terminal_size: HarnessRuntimeTerminalSizeV1,
) -> HarnessOperatorReplyV1 {
    let result = spawn_session_inner(
        state,
        &node_id,
        &workspace_id,
        &provider,
        &provider_profile,
        mode,
        terminal_size,
    )
    .await;
    match result {
        Ok(address) => {
            tracing::info!(
                operation = "spawn-session",
                node_id,
                workspace_id,
                provider,
                provider_profile,
                instance_id = address.instance_id,
                generation = address.generation,
                "harness-light: session spawned",
            );
            HarnessOperatorReplyV1::Ok { response: HarnessOperatorResponseV1::SessionSpawned(address) }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation = "spawn-session",
                node_id,
                workspace_id,
                provider,
                provider_profile,
                error = %error,
                mapped = ?mapped,
                "harness-light: session spawn rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn spawn_session_inner(
    state: &LightState,
    node_id: &str,
    workspace_id: &str,
    provider: &str,
    provider_profile: &str,
    mode: HarnessExecutionModeV1,
    terminal_size: HarnessRuntimeTerminalSizeV1,
) -> Result<HarnessRuntimeSessionAddressV1, LightRelayError> {
    let route = exact_route(&state.control, node_id)?;
    let workspace = WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?;
    let profile_id =
        SpawnProfileId::new(provider_profile).map_err(|_| LightRelayError::InvalidRequest)?;
    let provider_agent = AgentId::new(provider).map_err(|_| LightRelayError::InvalidRequest)?;

    let (_, snapshot) =
        fetch_snapshot_serialized(&state.control, &state.snapshot_gate, &route).await?;
    let profile = snapshot
        .launch_inventory
        .as_ref()
        .and_then(|inventory| inventory.spawn_profiles.as_ref())
        .and_then(|profiles| profiles.iter().find(|profile| profile.id == profile_id))
        .ok_or(LightRelayError::SpawnProfileUnavailable)?;

    let required_capabilities = match mode {
        HarnessExecutionModeV1::Pty => SpawnRequiredCapabilities::new([CapabilityId::new(
            SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
        )
        .map_err(|_| LightRelayError::InvalidRequest)?])
        .map_err(|_| LightRelayError::InvalidRequest)?,
        HarnessExecutionModeV1::Inline => SpawnRequiredCapabilities::default(),
    };
    let spec = SpawnSpec {
        target: SpawnTarget { node_id: route.node_id.clone(), workspace_id: workspace, worktree_id: None },
        profile_id: profile_id.clone(),
        expected_profile_revision: profile.revision.clone(),
        overrides: SpawnOverrides {
            provider: SpawnOverride::Set { value: provider_agent },
            mode: SpawnOverride::Set { value: execution_mode(mode) },
            terminal_size: SpawnOverride::Set {
                value: TerminalSize { rows: terminal_size.rows, columns: terminal_size.columns },
            },
            prompt: SpawnOverride::Clear,
            bundle_id: SpawnOverride::Clear,
            context_id: SpawnOverride::Clear,
            environment_profile_id: SpawnOverride::Clear,
        },
        deadline_ms: SpawnDeadlineMs::new(SESSION_SPAWN_DEADLINE_MS)
            .map_err(|_| LightRelayError::InvalidRequest)?,
        idempotency_key: fresh_idempotency_key()?,
        required_capabilities,
    };

    let routed = state.control.request(route.clone(), NodeRequest::SpawnSpec { spec }).await?;
    if routed.node_id != route.node_id || routed.incarnation_id != route.expected_incarnation_id {
        return Err(LightRelayError::IncarnationChanged);
    }
    let receipt = match routed.response {
        Ok(C2NodeResponse::SpawnSpecAccepted { receipt }) => receipt,
        Ok(_) => return Err(LightRelayError::UnexpectedResponse),
        Err(failure) => return Err(LightRelayError::NodeRejected(failure.code)),
    };
    let address = HarnessRuntimeSessionAddressV1 {
        node_id: route.node_id.as_str().to_owned(),
        incarnation_id: route.expected_incarnation_id.to_string(),
        workspace_id: receipt.session.workspace_id.as_str().to_owned(),
        instance_id: receipt.session.session.instance_id.0,
        generation: receipt.session.session.generation.0,
    };
    refresh_route(&state.control, &state.snapshot_gate, &state.inventory, &route).await;
    Ok(address)
}

fn fresh_idempotency_key() -> Result<SpawnIdempotencyKey, LightRelayError> {
    let nonce = random_nonce().map_err(LightRelayError::Crypto)?;
    SpawnIdempotencyKey::new(encode_hex(&nonce)).map_err(|_| LightRelayError::InvalidRequest)
}

fn execution_mode(mode: HarnessExecutionModeV1) -> SessionMode {
    match mode {
        HarnessExecutionModeV1::Pty => SessionMode::Pty,
        HarnessExecutionModeV1::Inline => SessionMode::Inline,
    }
}

/// The eight thin session-control verbs, sharing one C2 relay shape and one
/// `C2NodeResponse::Accepted` reply -- the light-local mirror of
/// `gate4agent-harness-service::c2::SessionControlKind`.
pub(crate) enum SessionVerb {
    Input { text: String },
    Resize { terminal_size: HarnessRuntimeTerminalSizeV1 },
    Stop { force: bool },
    Control { control: HarnessTerminalControlV1 },
    Bytes { bytes: Vec<u8> },
    Paste { text: String },
    Remove,
    Resume { terminal_size: HarnessRuntimeTerminalSizeV1 },
}

impl SessionVerb {
    fn operation(&self) -> &'static str {
        match self {
            Self::Input { .. } => "write-session-input",
            Self::Resize { .. } => "resize-session",
            Self::Stop { .. } => "stop-session",
            Self::Control { .. } => "control-session",
            Self::Bytes { .. } => "write-session-bytes",
            Self::Paste { .. } => "paste-session",
            Self::Remove => "remove-session",
            Self::Resume { .. } => "resume-session",
        }
    }

    fn wire_request(&self, session: SessionAddress) -> NodeRequest {
        match self {
            Self::Input { text } => NodeRequest::Input { session, text: text.clone() },
            Self::Resize { terminal_size } => NodeRequest::Resize {
                session,
                size: TerminalSize { rows: terminal_size.rows, columns: terminal_size.columns },
            },
            Self::Stop { force } => NodeRequest::Stop { session, force: *force },
            Self::Control { control } => {
                NodeRequest::TerminalControl { session, control: map_terminal_control(*control) }
            }
            Self::Bytes { bytes } => NodeRequest::TerminalBytes { session, bytes: bytes.clone() },
            Self::Paste { text } => NodeRequest::Paste { session, text: text.clone() },
            Self::Remove => NodeRequest::Remove { session },
            // `initial_prompt: None` always: the operator wire's
            // `ResumeSession` (unlike `ResumeSessionRecord`) never carries
            // one -- see `HarnessOperatorRequestV1::ResumeSession`'s doc
            // comment.
            Self::Resume { terminal_size } => NodeRequest::Resume {
                session,
                terminal_size: TerminalSize { rows: terminal_size.rows, columns: terminal_size.columns },
                initial_prompt: None,
            },
        }
    }

    /// `Stop`/`Remove` make a session leave the roster; `Resume` changes its
    /// generation. The other five never affect which sessions/workspaces a
    /// node reports.
    fn affects_roster(&self) -> bool {
        matches!(self, Self::Stop { .. } | Self::Remove | Self::Resume { .. })
    }

    fn response(&self) -> HarnessOperatorResponseV1 {
        match self {
            Self::Input { .. } => HarnessOperatorResponseV1::SessionInputWritten,
            Self::Resize { .. } => HarnessOperatorResponseV1::SessionResized,
            Self::Stop { .. } => HarnessOperatorResponseV1::SessionStopped,
            Self::Control { .. } => HarnessOperatorResponseV1::SessionControlled,
            Self::Bytes { .. } => HarnessOperatorResponseV1::SessionBytesWritten,
            Self::Paste { .. } => HarnessOperatorResponseV1::SessionPasted,
            Self::Remove => HarnessOperatorResponseV1::SessionRemoved,
            Self::Resume { .. } => HarnessOperatorResponseV1::SessionResumed,
        }
    }
}

pub(crate) async fn session_control(
    state: &LightState,
    session: HarnessRuntimeSessionAddressV1,
    verb: SessionVerb,
) -> HarnessOperatorReplyV1 {
    let operation = verb.operation();
    match session_control_inner(state, &session, &verb).await {
        Ok(()) => {
            tracing::info!(
                operation,
                node_id = session.node_id,
                workspace_id = session.workspace_id,
                instance_id = session.instance_id,
                generation = session.generation,
                "harness-light: session control accepted",
            );
            HarnessOperatorReplyV1::Ok { response: verb.response() }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation,
                node_id = session.node_id,
                workspace_id = session.workspace_id,
                instance_id = session.instance_id,
                generation = session.generation,
                error = %error,
                mapped = ?mapped,
                "harness-light: session control rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn session_control_inner(
    state: &LightState,
    session: &HarnessRuntimeSessionAddressV1,
    verb: &SessionVerb,
) -> Result<(), LightRelayError> {
    let route = exact_route(&state.control, &session.node_id)?;
    if route.expected_incarnation_id.to_string() != session.incarnation_id {
        return Err(LightRelayError::IncarnationChanged);
    }
    let workspace_id =
        WorkspaceId::new(session.workspace_id.as_str()).map_err(|_| LightRelayError::InvalidRequest)?;
    let address = SessionAddress {
        workspace_id,
        session: SessionKey {
            instance_id: AgentInstanceId(session.instance_id),
            generation: SessionGeneration(session.generation),
        },
    };
    let routed = state.control.request(route.clone(), verb.wire_request(address.clone())).await?;
    if routed.node_id != route.node_id || routed.incarnation_id != route.expected_incarnation_id {
        return Err(LightRelayError::IncarnationChanged);
    }
    match routed.response {
        Ok(C2NodeResponse::Accepted) => {}
        Ok(_) => return Err(LightRelayError::UnexpectedResponse),
        Err(failure) => return Err(LightRelayError::NodeRejected(failure.code)),
    }
    if verb.affects_roster() {
        refresh_route(&state.control, &state.snapshot_gate, &state.inventory, &route).await;
    }
    // A settled `Stop` does not unbind itself on the node side -- nothing
    // there drops a stopped session's own binding on its own, so without an
    // explicit follow-up `Remove` it would keep reporting itself in the
    // runtime inventory forever. Fired detached, after this verb's own
    // outcome is already decided, so a slow or failed reap never delays or
    // fails `StopSession` itself -- mirrors `gate4agent-harness-service::
    // c2::HarnessC2Adapter::remove_stopped_session` (`pub(crate)`, not
    // reusable) exactly, including firing for every settled `Stop`, not only
    // a forced one.
    if matches!(verb, SessionVerb::Stop { .. }) {
        spawn_stop_reap(state, route, address);
    }
    Ok(())
}

fn spawn_stop_reap(state: &LightState, route: NodeRoute, session: SessionAddress) {
    let control = state.control.clone();
    let inventory = state.inventory.clone();
    let snapshot_gate = state.snapshot_gate.clone();
    tokio::spawn(async move {
        let node_id = route.node_id.as_str().to_owned();
        match control.request(route.clone(), NodeRequest::Remove { session }).await {
            Ok(routed)
                if routed.node_id == route.node_id
                    && routed.incarnation_id == route.expected_incarnation_id =>
            {
                match routed.response {
                    Ok(C2NodeResponse::Accepted) => {
                        tracing::debug!(node_id, "harness-light: stop reap accepted");
                    }
                    Ok(_) => {
                        tracing::warn!(node_id, "harness-light: stop reap got an unexpected response");
                    }
                    Err(failure) => {
                        tracing::warn!(
                            node_id,
                            code = ?failure.code,
                            "harness-light: stop reap rejected by the node",
                        );
                    }
                }
            }
            Ok(_) => {
                tracing::warn!(node_id, "harness-light: stop reap route/incarnation mismatch");
            }
            Err(error) => {
                tracing::warn!(node_id, error = %error, "harness-light: stop reap transport failed");
            }
        }
        // Best-effort either way: whether the node actually dropped the
        // binding or not, a fresh snapshot is what the roster should reflect
        // next.
        refresh_route(&control, &snapshot_gate, &inventory, &route).await;
    });
}

/// Exact mirror of `gate4agent_types::TerminalControl` <- the wire type
/// `HarnessTerminalControlV1`, exhaustive so a variant added to either side
/// without the other fails to compile here -- the same technique (and the
/// same duplication rationale) as `gate4agent-harness-service::c2`'s own
/// (private) `map_terminal_control`.
fn map_terminal_control(control: HarnessTerminalControlV1) -> TerminalControl {
    match control {
        HarnessTerminalControlV1::Interrupt => TerminalControl::Interrupt,
        HarnessTerminalControlV1::EndOfFile => TerminalControl::EndOfFile,
        HarnessTerminalControlV1::ControlA => TerminalControl::ControlA,
        HarnessTerminalControlV1::ControlB => TerminalControl::ControlB,
        HarnessTerminalControlV1::ControlE => TerminalControl::ControlE,
        HarnessTerminalControlV1::ControlF => TerminalControl::ControlF,
        HarnessTerminalControlV1::ControlG => TerminalControl::ControlG,
        HarnessTerminalControlV1::ControlH => TerminalControl::ControlH,
        HarnessTerminalControlV1::ControlI => TerminalControl::ControlI,
        HarnessTerminalControlV1::ControlJ => TerminalControl::ControlJ,
        HarnessTerminalControlV1::ControlK => TerminalControl::ControlK,
        HarnessTerminalControlV1::ControlL => TerminalControl::ControlL,
        HarnessTerminalControlV1::ControlM => TerminalControl::ControlM,
        HarnessTerminalControlV1::ControlN => TerminalControl::ControlN,
        HarnessTerminalControlV1::ControlO => TerminalControl::ControlO,
        HarnessTerminalControlV1::ControlP => TerminalControl::ControlP,
        HarnessTerminalControlV1::ControlQ => TerminalControl::ControlQ,
        HarnessTerminalControlV1::ControlR => TerminalControl::ControlR,
        HarnessTerminalControlV1::ControlS => TerminalControl::ControlS,
        HarnessTerminalControlV1::ControlT => TerminalControl::ControlT,
        HarnessTerminalControlV1::ControlU => TerminalControl::ControlU,
        HarnessTerminalControlV1::ControlV => TerminalControl::ControlV,
        HarnessTerminalControlV1::ControlW => TerminalControl::ControlW,
        HarnessTerminalControlV1::ControlX => TerminalControl::ControlX,
        HarnessTerminalControlV1::ControlY => TerminalControl::ControlY,
        HarnessTerminalControlV1::ControlZ => TerminalControl::ControlZ,
        HarnessTerminalControlV1::Enter => TerminalControl::Enter,
        HarnessTerminalControlV1::LineFeed => TerminalControl::LineFeed,
        HarnessTerminalControlV1::Escape => TerminalControl::Escape,
        HarnessTerminalControlV1::Backspace => TerminalControl::Backspace,
        HarnessTerminalControlV1::Tab => TerminalControl::Tab,
        HarnessTerminalControlV1::BackTab => TerminalControl::BackTab,
        HarnessTerminalControlV1::Insert => TerminalControl::Insert,
        HarnessTerminalControlV1::Delete => TerminalControl::Delete,
        HarnessTerminalControlV1::Home => TerminalControl::Home,
        HarnessTerminalControlV1::End => TerminalControl::End,
        HarnessTerminalControlV1::PageUp => TerminalControl::PageUp,
        HarnessTerminalControlV1::PageDown => TerminalControl::PageDown,
        HarnessTerminalControlV1::ArrowUp => TerminalControl::ArrowUp,
        HarnessTerminalControlV1::ArrowDown => TerminalControl::ArrowDown,
        HarnessTerminalControlV1::ArrowRight => TerminalControl::ArrowRight,
        HarnessTerminalControlV1::ArrowLeft => TerminalControl::ArrowLeft,
        HarnessTerminalControlV1::Function1 => TerminalControl::Function1,
        HarnessTerminalControlV1::Function2 => TerminalControl::Function2,
        HarnessTerminalControlV1::Function3 => TerminalControl::Function3,
        HarnessTerminalControlV1::Function4 => TerminalControl::Function4,
        HarnessTerminalControlV1::Function5 => TerminalControl::Function5,
        HarnessTerminalControlV1::Function6 => TerminalControl::Function6,
        HarnessTerminalControlV1::Function7 => TerminalControl::Function7,
        HarnessTerminalControlV1::Function8 => TerminalControl::Function8,
        HarnessTerminalControlV1::Function9 => TerminalControl::Function9,
        HarnessTerminalControlV1::Function10 => TerminalControl::Function10,
        HarnessTerminalControlV1::Function11 => TerminalControl::Function11,
        HarnessTerminalControlV1::Function12 => TerminalControl::Function12,
    }
}

// ===========================================================================
// A2: node-workspace read/write, native history, session-record mutation,
// resource mutation (management), host-directory browse.
//
// Every family below shares the same direct-relay shape the nine session
// verbs above already established (resolve a live route, build the wire
// request, `state.control.request` it, check the response is routed from the
// same node/incarnation, map the outcome) rather than the full harness's own
// `Prepared*`/`Pending*` C2-waiter split (`gate4agent-harness-service::c2`):
// that split exists to let the harness's single-writer host loop dispatch a
// request and poll its completion later without blocking the loop on the C2
// round trip, which this crate has no equivalent of (one task per operator
// connection already IS the concurrency unit -- see the crate-level report).
//
// What *is* reused from `gate4agent-harness-service::c2` (promoted `pub`, see
// each promoted item's own doc comment there) is every PURE piece: the
// `WorkspaceReadKind`/`WorkspaceWriteKind`/`SessionRecordMutationKind`/
// `ResourceMutationKind` enums and their `Prepared*` bundles (route + kind,
// no adapter dependency once built), `wire_request()`, and above all the
// `correlate_*_response` functions -- the git/workspace/native-history/
// managed-session projection logic (truncation markers, size caps, status
// mapping) is substantial and would be a correctness hazard to duplicate.
// What is NOT reused is `Prepared*::from_operator_request` (takes
// `&HarnessC2Adapter`, which this crate deliberately has no equivalent of --
// see `crate::c2`'s own module doc comment) and `Pending*::finish` (the
// C2-waiter half): this crate builds its own `Prepared*` via the promoted
// `new()` constructor after resolving its own route via `crate::c2::exact_route`,
// and awaits its own `state.control.request(...)` directly via `relay_to_route`.
//
// Deadlines: unlike the nine verbs above (which rely solely on the outer
// `crate::LIGHT_CONNECTION_DEADLINE`, 45s), every family below wraps its C2
// round trip in `tokio::time::timeout` under its own bucket, mirroring
// `gate4agent-harness-service::runtime`'s own `HOST_*_RESPONSE_DEADLINE`
// constants (plain literals, duplicated for the same no-kernel-dependency
// reason `SESSION_SPAWN_DEADLINE_MS` above already is) -- a hung node call on
// any of these must fail the *request* with a typed `Deadline`, not silently
// ride the connection down to a generic timeout at 45s regardless of which
// verb actually hung. Bounded worker pools (the full harness's own per-family
// `*_WORKERS_MAX` caps) are NOT mirrored: this crate handles one request per
// connection, so concurrency is already naturally bounded by how many
// operator connections are open, per the app-harness protocol contract's own
// framing of this slice.
const NATIVE_HISTORY_RESPONSE_DEADLINE: Duration = Duration::from_secs(40);
const NODE_WORKSPACE_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
const SESSION_RECORD_MUTATION_RESPONSE_DEADLINE: Duration = Duration::from_secs(28);
const HOST_DIRECTORY_BROWSE_RESPONSE_DEADLINE: Duration = Duration::from_secs(12);
const RESOURCE_MUTATION_RESPONSE_DEADLINE: Duration = Duration::from_secs(28);

/// Sends `wire_request` to `route` under `deadline`, verifying the reply is
/// actually routed from the same node/incarnation before handing back its
/// `C2NodeResponse` -- the shared "direct relay" shape every family below
/// uses, generalizing `spawn_session_inner`/`session_control_inner`'s own
/// inline route-echo check (above) over an arbitrary wire request/response
/// instead of duplicating it once per verb.
async fn relay_to_route(
    state: &LightState,
    route: &NodeRoute,
    deadline: Duration,
    wire_request: NodeRequest,
) -> Result<C2NodeResponse, LightRelayError> {
    let routed = timeout(deadline, state.control.request(route.clone(), wire_request))
        .await
        .map_err(|_| LightRelayError::Deadline)??;
    if routed.node_id != route.node_id || routed.incarnation_id != route.expected_incarnation_id {
        return Err(LightRelayError::IncarnationChanged);
    }
    match routed.response {
        Ok(response) => Ok(response),
        Err(failure) => Err(LightRelayError::NodeRejected(failure.code)),
    }
}

fn repository_path_from_api(path: &HarnessRepositoryPathV1) -> Result<RepositoryPath, LightRelayError> {
    RepositoryPath::utf8(path.as_str().to_owned()).map_err(|_| LightRelayError::InvalidRequest)
}

fn git_object_id_from_api(object_id: &HarnessGitObjectIdV1) -> Result<GitObjectId, LightRelayError> {
    GitObjectId::new(object_id.as_str().to_owned()).map_err(|_| LightRelayError::InvalidRequest)
}

fn git_diff_mode_from_api(mode: &HarnessGitDiffModeV1) -> Result<GitDiffMode, LightRelayError> {
    Ok(match mode {
        HarnessGitDiffModeV1::Working => GitDiffMode::Working,
        HarnessGitDiffModeV1::Staged => GitDiffMode::Staged,
        HarnessGitDiffModeV1::Commit { revision } => {
            GitDiffMode::Commit { revision: git_object_id_from_api(revision)? }
        }
    })
}

fn workspace_file_revision_from_api(
    revision: &HarnessWorkspaceFileRevisionV1,
) -> Result<WorkspaceFileRevision, LightRelayError> {
    WorkspaceFileRevision::new(revision.as_str().to_owned()).map_err(|_| LightRelayError::InvalidRequest)
}

fn host_path_from_api(path: &HarnessHostPathV1) -> Result<OpaqueHostPath, LightRelayError> {
    OpaqueHostPath::utf8(path.as_str().to_owned()).map_err(|_| LightRelayError::InvalidRequest)
}

/// The node-workspace read family's four verbs, carrying the wire's own API
/// types -- built inline at the `crate::dispatch` call site the same way
/// [`SessionVerb`] already is, then converted into the promoted
/// `gate4agent_harness_service::c2::WorkspaceReadKind` by [`Self::into_kind`].
pub(crate) enum NodeWorkspaceReadRequest {
    Inspect,
    File { path: HarnessRepositoryPathV1 },
    GitHistory {
        path: Option<HarnessRepositoryPathV1>,
        before: Option<HarnessGitObjectIdV1>,
        limit: u16,
    },
    GitDiff { mode: HarnessGitDiffModeV1, path: Option<HarnessRepositoryPathV1> },
}

impl NodeWorkspaceReadRequest {
    fn operation(&self) -> &'static str {
        match self {
            Self::Inspect => "inspect-node-workspace",
            Self::File { .. } => "read-node-workspace-file",
            Self::GitHistory { .. } => "read-node-git-history",
            Self::GitDiff { .. } => "read-node-git-diff",
        }
    }

    fn into_kind(self) -> Result<WorkspaceReadKind, LightRelayError> {
        Ok(match self {
            Self::Inspect => WorkspaceReadKind::InspectWorkspace,
            Self::File { path } => {
                WorkspaceReadKind::ReadWorkspaceFile { path: repository_path_from_api(&path)? }
            }
            Self::GitHistory { path, before, limit } => WorkspaceReadKind::ReadGitHistory {
                path: path.as_ref().map(repository_path_from_api).transpose()?,
                before: before.as_ref().map(git_object_id_from_api).transpose()?,
                limit,
            },
            Self::GitDiff { mode, path } => WorkspaceReadKind::ReadGitDiff {
                request: GitDiffRequest {
                    mode: git_diff_mode_from_api(&mode)?,
                    path: path.as_ref().map(repository_path_from_api).transpose()?,
                },
            },
        })
    }
}

pub(crate) async fn node_workspace_read(
    state: &LightState,
    node_id: String,
    workspace_id: String,
    request: NodeWorkspaceReadRequest,
) -> HarnessOperatorReplyV1 {
    let operation = request.operation();
    match node_workspace_read_inner(state, &node_id, &workspace_id, request).await {
        Ok(response) => {
            tracing::info!(operation, node_id, workspace_id, "harness-light: node-workspace read served");
            HarnessOperatorReplyV1::Ok { response }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation, node_id, workspace_id, error = %error, mapped = ?mapped,
                "harness-light: node-workspace read rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn node_workspace_read_inner(
    state: &LightState,
    node_id: &str,
    workspace_id: &str,
    request: NodeWorkspaceReadRequest,
) -> Result<HarnessOperatorResponseV1, LightRelayError> {
    let route = exact_route(&state.control, node_id)?;
    let workspace = WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?;
    let kind = request.into_kind()?;
    let prepared = PreparedNodeWorkspaceRead::new(route.clone(), workspace, kind);
    let response =
        relay_to_route(state, &route, NODE_WORKSPACE_RESPONSE_DEADLINE, prepared.wire_request()).await?;
    correlate_node_workspace_read_response(&prepared, response).map_err(LightRelayError::Projection)
}

/// Write/create sibling of [`NodeWorkspaceReadRequest`], same shape.
pub(crate) enum NodeWorkspaceWriteRequest {
    File {
        path: HarnessRepositoryPathV1,
        content: String,
        expected_revision: HarnessWorkspaceFileRevisionV1,
    },
    CreateFile { path: HarnessRepositoryPathV1 },
    CreateDirectory { path: HarnessRepositoryPathV1 },
}

impl NodeWorkspaceWriteRequest {
    fn operation(&self) -> &'static str {
        match self {
            Self::File { .. } => "write-node-workspace-file",
            Self::CreateFile { .. } => "create-node-workspace-file",
            Self::CreateDirectory { .. } => "create-node-workspace-directory",
        }
    }

    fn into_kind(self) -> Result<WorkspaceWriteKind, LightRelayError> {
        Ok(match self {
            Self::File { path, content, expected_revision } => WorkspaceWriteKind::WriteFile {
                path: repository_path_from_api(&path)?,
                expected_revision: workspace_file_revision_from_api(&expected_revision)?,
                content,
            },
            Self::CreateFile { path } => {
                WorkspaceWriteKind::CreateFile { path: repository_path_from_api(&path)? }
            }
            Self::CreateDirectory { path } => {
                WorkspaceWriteKind::CreateDirectory { path: repository_path_from_api(&path)? }
            }
        })
    }
}

pub(crate) async fn node_workspace_write(
    state: &LightState,
    node_id: String,
    workspace_id: String,
    request: NodeWorkspaceWriteRequest,
) -> HarnessOperatorReplyV1 {
    let operation = request.operation();
    match node_workspace_write_inner(state, &node_id, &workspace_id, request).await {
        Ok(response) => {
            tracing::info!(operation, node_id, workspace_id, "harness-light: node-workspace write accepted");
            HarnessOperatorReplyV1::Ok { response }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation, node_id, workspace_id, error = %error, mapped = ?mapped,
                "harness-light: node-workspace write rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn node_workspace_write_inner(
    state: &LightState,
    node_id: &str,
    workspace_id: &str,
    request: NodeWorkspaceWriteRequest,
) -> Result<HarnessOperatorResponseV1, LightRelayError> {
    let route = exact_route(&state.control, node_id)?;
    let workspace = WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?;
    let kind = request.into_kind()?;
    let prepared = PreparedNodeWorkspaceWrite::new(route.clone(), workspace, kind);
    let response =
        relay_to_route(state, &route, NODE_WORKSPACE_RESPONSE_DEADLINE, prepared.wire_request()).await?;
    // No eager roster refresh: a workspace-file write/create changes file
    // contents, never workspace/session presence, so it never affects what
    // `RuntimeInventoryList` reports -- matches `event_affects_roster`
    // (`crate::inventory`) not listing `WorkspaceFileWritten`/`Created`
    // either.
    correlate_node_workspace_write_response(&prepared, response).map_err(LightRelayError::Projection)
}

fn api_route_to_node_route(route: &HarnessNativeSessionRouteV1) -> Result<NodeRoute, LightRelayError> {
    Ok(NodeRoute {
        node_id: NodeId::new(route.node_id.as_str()).map_err(|_| LightRelayError::InvalidRequest)?,
        expected_incarnation_id: route.incarnation_id.parse().map_err(|_| LightRelayError::InvalidRequest)?,
    })
}

/// `CatalogNativeSessions`/`PageNativeSessions`/`PreviewNativeSession`: the
/// native-history pool proper, each carrying its own caller-pinned
/// `node_id`/`incarnation_id` (`HarnessNativeSessionRouteV1`/`...SelectionV1`)
/// rather than resolving one live via `crate::c2::exact_route` -- the
/// "incarnation-pinned routes where the full harness pins them" shape the A2
/// task description calls out, mirroring
/// `gate4agent_harness_service::c2::native_history_wire_request`'s own
/// identical trust decision for this exact trio.
pub(crate) async fn catalog_native_sessions(
    state: &LightState,
    route: HarnessNativeSessionRouteV1,
    limit: u16,
) -> HarnessOperatorReplyV1 {
    native_history_relay(
        state,
        "catalog-native-sessions",
        HarnessOperatorRequestV1::CatalogNativeSessions { route, limit },
    ).await
}

pub(crate) async fn page_native_sessions(
    state: &LightState,
    route: HarnessNativeSessionRouteV1,
    window: HarnessNativeSessionCatalogWindowV1,
    catalog_revision: u64,
    recent_cutoff_unix_ms: u64,
    after_selection_id: Option<String>,
    limit: u16,
) -> HarnessOperatorReplyV1 {
    native_history_relay(
        state,
        "page-native-sessions",
        HarnessOperatorRequestV1::PageNativeSessions {
            route, window, catalog_revision, recent_cutoff_unix_ms, after_selection_id, limit,
        },
    ).await
}

pub(crate) async fn preview_native_session(
    state: &LightState,
    selection: HarnessNativeSessionSelectionV1,
    message_limit: u16,
) -> HarnessOperatorReplyV1 {
    native_history_relay(
        state,
        "preview-native-session",
        HarnessOperatorRequestV1::PreviewNativeSession { selection, message_limit },
    ).await
}

async fn native_history_relay(
    state: &LightState,
    operation: &'static str,
    request: HarnessOperatorRequestV1,
) -> HarnessOperatorReplyV1 {
    match native_history_relay_inner(state, request).await {
        Ok(response) => {
            tracing::info!(operation, "harness-light: native-history request served");
            HarnessOperatorReplyV1::Ok { response }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation, error = %error, mapped = ?mapped,
                "harness-light: native-history request rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn native_history_relay_inner(
    state: &LightState,
    request: HarnessOperatorRequestV1,
) -> Result<HarnessOperatorResponseV1, LightRelayError> {
    let (route, wire_request) = match &request {
        HarnessOperatorRequestV1::CatalogNativeSessions { route, limit } => (
            api_route_to_node_route(route)?,
            NodeRequest::CatalogNativeSessions {
                route: native_history_wire_route(route).map_err(|_| LightRelayError::InvalidRequest)?,
                limit: *limit,
            },
        ),
        HarnessOperatorRequestV1::PageNativeSessions {
            route, window, catalog_revision, recent_cutoff_unix_ms, after_selection_id, limit,
        } => (
            api_route_to_node_route(route)?,
            NodeRequest::PageNativeSessions {
                route: native_history_wire_route(route).map_err(|_| LightRelayError::InvalidRequest)?,
                window: native_history_wire_window(*window),
                catalog_revision: *catalog_revision,
                recent_cutoff_unix_ms: *recent_cutoff_unix_ms,
                after_selection_id: after_selection_id.clone(),
                limit: *limit,
            },
        ),
        HarnessOperatorRequestV1::PreviewNativeSession { selection, message_limit } => (
            api_route_to_node_route(&selection.route)?,
            NodeRequest::PreviewNativeSession {
                selection: native_history_wire_selection(selection)
                    .map_err(|_| LightRelayError::InvalidRequest)?,
                message_limit: *message_limit,
            },
        ),
        _ => return Err(LightRelayError::InvalidRequest),
    };
    let response = relay_to_route(state, &route, NATIVE_HISTORY_RESPONSE_DEADLINE, wire_request).await?;
    correlate_native_history_response(request, response).map_err(LightRelayError::Projection)
}

/// `PreviewSessionRecord` rides the same native-history correlation function
/// as the pool above but is node-scoped (bare `node_id`, resolved live via
/// `crate::c2::exact_route`), not route-scoped like its three pool-mates --
/// mirrors `gate4agent_harness_service::c2::native_history_wire_request`'s
/// own special-cased branch for this one verb exactly.
pub(crate) async fn preview_session_record(
    state: &LightState,
    node_id: String,
    record_id: String,
    message_limit: u16,
) -> HarnessOperatorReplyV1 {
    match preview_session_record_inner(state, &node_id, &record_id, message_limit).await {
        Ok(response) => {
            tracing::info!(
                operation = "preview-session-record", node_id, record_id,
                "harness-light: session-record preview served",
            );
            HarnessOperatorReplyV1::Ok { response }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation = "preview-session-record", node_id, record_id, error = %error, mapped = ?mapped,
                "harness-light: session-record preview rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn preview_session_record_inner(
    state: &LightState,
    node_id: &str,
    record_id: &str,
    message_limit: u16,
) -> Result<HarnessOperatorResponseV1, LightRelayError> {
    let route = exact_route(&state.control, node_id)?;
    let record = SessionRecordId::new(record_id).map_err(|_| LightRelayError::InvalidRequest)?;
    let wire_request = NodeRequest::PreviewSessionRecord { record_id: record, message_limit };
    let response = relay_to_route(state, &route, NATIVE_HISTORY_RESPONSE_DEADLINE, wire_request).await?;
    let request = HarnessOperatorRequestV1::PreviewSessionRecord {
        node_id: node_id.to_owned(),
        record_id: record_id.to_owned(),
        message_limit,
    };
    correlate_native_history_response(request, response).map_err(LightRelayError::Projection)
}

/// The six session-record-mutation verbs, carrying the wire's own API types
/// -- built inline at the `crate::dispatch` call site, converted into the
/// promoted `SessionRecordMutationKind` by [`Self::into_kind`].
/// `IndexNativeSession` alone has no standalone `node_id` field on the wire
/// (see [`Self::node_id`]): it derives one from `selection.route.node_id`,
/// mirroring `gate4agent_harness_service::c2::PreparedSessionRecordMutation
/// ::from_operator_request`'s own identical derivation.
pub(crate) enum SessionRecordMutationRequest {
    Resume {
        node_id: String,
        record_id: String,
        terminal_size: HarnessRuntimeTerminalSizeV1,
        initial_prompt: Option<String>,
    },
    Rename { node_id: String, record_id: String, display_name: String },
    SetTask { node_id: String, record_id: String, expected_revision: u64, target: HarnessSessionTaskTargetV1 },
    Forget { node_id: String, record_id: String },
    IndexProvider {
        node_id: String,
        workspace_id: String,
        provider: String,
        identity: HarnessProviderSessionIdentityV1,
        display_name: String,
    },
    IndexNative { selection: HarnessNativeSessionSelectionV1, display_name: String },
}

impl SessionRecordMutationRequest {
    fn node_id(&self) -> &str {
        match self {
            Self::Resume { node_id, .. }
            | Self::Rename { node_id, .. }
            | Self::SetTask { node_id, .. }
            | Self::Forget { node_id, .. }
            | Self::IndexProvider { node_id, .. } => node_id,
            Self::IndexNative { selection, .. } => selection.route.node_id.as_str(),
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::Resume { .. } => "resume-session-record",
            Self::Rename { .. } => "rename-session-record",
            Self::SetTask { .. } => "set-session-task",
            Self::Forget { .. } => "forget-session-record",
            Self::IndexProvider { .. } => "index-provider-session",
            Self::IndexNative { .. } => "index-native-session",
        }
    }

    fn into_kind(self) -> Result<SessionRecordMutationKind, LightRelayError> {
        Ok(match self {
            Self::Resume { record_id, terminal_size, initial_prompt, .. } => {
                SessionRecordMutationKind::Resume {
                    record_id: SessionRecordId::new(record_id).map_err(|_| LightRelayError::InvalidRequest)?,
                    terminal_size: TerminalSize { rows: terminal_size.rows, columns: terminal_size.columns },
                    initial_prompt,
                }
            }
            Self::Rename { record_id, display_name, .. } => SessionRecordMutationKind::Rename {
                record_id: SessionRecordId::new(record_id).map_err(|_| LightRelayError::InvalidRequest)?,
                display_name,
            },
            Self::SetTask { record_id, expected_revision, target, .. } => SessionRecordMutationKind::SetTask {
                record_id: SessionRecordId::new(record_id).map_err(|_| LightRelayError::InvalidRequest)?,
                expected_revision,
                target: session_task_target_from_api(&target).map_err(|_| LightRelayError::InvalidRequest)?,
            },
            Self::Forget { record_id, .. } => SessionRecordMutationKind::Forget {
                record_id: SessionRecordId::new(record_id).map_err(|_| LightRelayError::InvalidRequest)?,
            },
            Self::IndexProvider { workspace_id, provider, identity, display_name, .. } => {
                SessionRecordMutationKind::IndexProvider {
                    workspace_id: WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?,
                    provider: AgentId::new(provider).map_err(|_| LightRelayError::InvalidRequest)?,
                    identity: provider_session_identity_from_api(&identity)
                        .map_err(|_| LightRelayError::InvalidRequest)?,
                    display_name,
                }
            }
            Self::IndexNative { selection, display_name } => SessionRecordMutationKind::IndexNative {
                selection: native_history_wire_selection(&selection)
                    .map_err(|_| LightRelayError::InvalidRequest)?,
                display_name,
            },
        })
    }
}

pub(crate) async fn session_record_mutation(
    state: &LightState,
    request: SessionRecordMutationRequest,
) -> HarnessOperatorReplyV1 {
    let operation = request.operation();
    let node_id = request.node_id().to_owned();
    match session_record_mutation_inner(state, request).await {
        Ok(response) => {
            tracing::info!(operation, node_id, "harness-light: session-record mutation accepted");
            HarnessOperatorReplyV1::Ok { response }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation, node_id, error = %error, mapped = ?mapped,
                "harness-light: session-record mutation rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn session_record_mutation_inner(
    state: &LightState,
    request: SessionRecordMutationRequest,
) -> Result<HarnessOperatorResponseV1, LightRelayError> {
    let route = exact_route(&state.control, request.node_id())?;
    let kind = request.into_kind()?;
    let prepared = PreparedSessionRecordMutation::new(route.clone(), kind);
    let response = relay_to_route(
        state, &route, SESSION_RECORD_MUTATION_RESPONSE_DEADLINE, prepared.wire_request(),
    ).await?;
    let response = correlate_session_record_mutation_response(&prepared, response)
        .map_err(LightRelayError::Projection)?;
    // All six session-record-mutation verbs change the node's managed-session
    // store, which the runtime inventory's `managed_sessions` roster caches --
    // unconditionally refreshed on success, mirroring
    // `gate4agent_harness_service::c2::PreparedSessionRecordMutation`'s own
    // doc comment on why this family never special-cases which of the six
    // actually changed anything (the A1 eager-refresh pattern, see
    // `crate::relay`'s own module doc comment above `SessionVerb`).
    refresh_route(&state.control, &state.snapshot_gate, &state.inventory, &route).await;
    Ok(response)
}

/// The seven management-family verbs, carrying the wire's own API types --
/// built inline at the `crate::dispatch` call site, converted into the
/// promoted `ResourceMutationKind` by [`Self::into_kind`]. `ExportContextPack`
/// alone has no standalone `node_id` field (see [`Self::node_id`]) and is
/// session-address-scoped rather than bare-`node_id`-scoped like its six
/// siblings -- see [`resource_mutation_inner`]'s incarnation-pin check.
pub(crate) enum ResourceMutationRequest {
    RegisterWorkspace { node_id: String, workspace_id: String, root: HarnessHostPathV1 },
    UnregisterWorkspace { node_id: String, workspace_id: String },
    CreateStandaloneWorkspace {
        node_id: String,
        workspace_id: String,
        root: HarnessHostPathV1,
        initial_branch: Option<String>,
    },
    CreateWorktree {
        node_id: String,
        source_workspace_id: String,
        workspace_id: String,
        target_root: HarnessHostPathV1,
        branch: String,
        base: Option<String>,
    },
    RemoveWorktree { node_id: String, source_workspace_id: String, target_root: HarnessHostPathV1 },
    ExportContextPack { session: HarnessRuntimeSessionAddressV1 },
    ForgetContextPack { node_id: String, context_id: HarnessSelectorV1 },
}

impl ResourceMutationRequest {
    fn node_id(&self) -> &str {
        match self {
            Self::RegisterWorkspace { node_id, .. }
            | Self::UnregisterWorkspace { node_id, .. }
            | Self::CreateStandaloneWorkspace { node_id, .. }
            | Self::CreateWorktree { node_id, .. }
            | Self::RemoveWorktree { node_id, .. }
            | Self::ForgetContextPack { node_id, .. } => node_id,
            Self::ExportContextPack { session } => session.node_id.as_str(),
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::RegisterWorkspace { .. } => "register-workspace",
            Self::UnregisterWorkspace { .. } => "unregister-workspace",
            Self::CreateStandaloneWorkspace { .. } => "create-standalone-workspace",
            Self::CreateWorktree { .. } => "create-worktree",
            Self::RemoveWorktree { .. } => "remove-worktree",
            Self::ExportContextPack { .. } => "export-context-pack",
            Self::ForgetContextPack { .. } => "forget-context-pack",
        }
    }

    fn into_kind(self) -> Result<ResourceMutationKind, LightRelayError> {
        Ok(match self {
            Self::RegisterWorkspace { workspace_id, root, .. } => ResourceMutationKind::RegisterWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?,
                root: host_path_from_api(&root)?,
            },
            Self::UnregisterWorkspace { workspace_id, .. } => ResourceMutationKind::UnregisterWorkspace {
                workspace_id: WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?,
            },
            Self::CreateStandaloneWorkspace { workspace_id, root, initial_branch, .. } => {
                ResourceMutationKind::CreateStandaloneWorkspace {
                    workspace_id: WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?,
                    root: host_path_from_api(&root)?,
                    initial_branch,
                }
            }
            Self::CreateWorktree { source_workspace_id, workspace_id, target_root, branch, base, .. } => {
                ResourceMutationKind::CreateWorktree {
                    source_workspace_id: WorkspaceId::new(source_workspace_id)
                        .map_err(|_| LightRelayError::InvalidRequest)?,
                    workspace_id: WorkspaceId::new(workspace_id).map_err(|_| LightRelayError::InvalidRequest)?,
                    target_root: host_path_from_api(&target_root)?,
                    branch,
                    base,
                }
            }
            Self::RemoveWorktree { source_workspace_id, target_root, .. } => {
                ResourceMutationKind::RemoveWorktree {
                    source_workspace_id: WorkspaceId::new(source_workspace_id)
                        .map_err(|_| LightRelayError::InvalidRequest)?,
                    target_root: host_path_from_api(&target_root)?,
                }
            }
            Self::ExportContextPack { session } => ResourceMutationKind::ExportContextPack {
                session: SessionAddress {
                    workspace_id: WorkspaceId::new(session.workspace_id.as_str())
                        .map_err(|_| LightRelayError::InvalidRequest)?,
                    session: SessionKey {
                        instance_id: AgentInstanceId(session.instance_id),
                        generation: SessionGeneration(session.generation),
                    },
                },
            },
            Self::ForgetContextPack { context_id, .. } => ResourceMutationKind::ForgetContextPack {
                context_id: SpawnContextId::new(context_id.as_str())
                    .map_err(|_| LightRelayError::InvalidRequest)?,
            },
        })
    }
}

pub(crate) async fn resource_mutation(
    state: &LightState,
    request: ResourceMutationRequest,
) -> HarnessOperatorReplyV1 {
    let operation = request.operation();
    let node_id = request.node_id().to_owned();
    match resource_mutation_inner(state, request).await {
        Ok(response) => {
            tracing::info!(operation, node_id, "harness-light: resource mutation accepted");
            HarnessOperatorReplyV1::Ok { response }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation, node_id, error = %error, mapped = ?mapped,
                "harness-light: resource mutation rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn resource_mutation_inner(
    state: &LightState,
    request: ResourceMutationRequest,
) -> Result<HarnessOperatorResponseV1, LightRelayError> {
    let route = exact_route(&state.control, request.node_id())?;
    // `ExportContextPack` is session-address-scoped: the caller pins an
    // incarnation via the session address, and a route that has since moved
    // on is a typed `Conflict` -- mirrors
    // `gate4agent_harness_service::c2::PreparedResourceMutation::from_operator_request`'s
    // own special-cased branch for this one verb exactly.
    if let ResourceMutationRequest::ExportContextPack { session } = &request {
        let expected: NodeIncarnationId =
            session.incarnation_id.parse().map_err(|_| LightRelayError::InvalidRequest)?;
        if route.expected_incarnation_id != expected {
            return Err(LightRelayError::IncarnationChanged);
        }
    }
    let kind = request.into_kind()?;
    let invalidates_runtime_inventory = kind.invalidates_runtime_inventory();
    let prepared = PreparedResourceMutation::new(route.clone(), kind);
    let response = relay_to_route(
        state, &route, RESOURCE_MUTATION_RESPONSE_DEADLINE, prepared.wire_request(),
    ).await?;
    let response = correlate_resource_mutation_response(&prepared, response)
        .map_err(LightRelayError::Projection)?;
    // Workspace/worktree lifecycle verbs change the node's `workspaces` map,
    // which the runtime inventory roster caches; `ExportContextPack`/
    // `ForgetContextPack` touch only the node's context-pack store, no
    // roster effect -- see `ResourceMutationKind::invalidates_runtime_inventory`'s
    // own doc comment (promoted alongside the enum) for why this stays
    // unconditional per-verb-group rather than special-cased per response.
    if invalidates_runtime_inventory {
        refresh_route(&state.control, &state.snapshot_gate, &state.inventory, &route).await;
    }
    Ok(response)
}

/// `BrowseHostDirectories`: node-scoped like the bare-`node_id` resource-
/// mutation verbs, but targets the host filesystem directly (no registered
/// workspace, no `Kind` enum -- there is only one verb in this family), so it
/// calls the promoted `project_host_directory_listing` projection directly
/// rather than through a `Prepared*`/`correlate_*` pair.
pub(crate) async fn browse_host_directories(
    state: &LightState,
    node_id: String,
    directory: Option<HarnessHostPathV1>,
    after: Option<HarnessHostPathV1>,
) -> HarnessOperatorReplyV1 {
    match browse_host_directories_inner(state, &node_id, directory, after).await {
        Ok(response) => {
            tracing::info!(operation = "browse-host-directories", node_id, "harness-light: host directories browsed");
            HarnessOperatorReplyV1::Ok { response }
        }
        Err(error) => {
            let mapped = error.into_host_error();
            tracing::warn!(
                operation = "browse-host-directories", node_id, error = %error, mapped = ?mapped,
                "harness-light: host-directory browse rejected",
            );
            HarnessOperatorReplyV1::Error { error: mapped }
        }
    }
}

async fn browse_host_directories_inner(
    state: &LightState,
    node_id: &str,
    directory: Option<HarnessHostPathV1>,
    after: Option<HarnessHostPathV1>,
) -> Result<HarnessOperatorResponseV1, LightRelayError> {
    let route = exact_route(&state.control, node_id)?;
    let directory = directory.as_ref().map(host_path_from_api).transpose()?;
    let after = after.as_ref().map(host_path_from_api).transpose()?;
    let wire_request = NodeRequest::BrowseHostDirectories { directory, after };
    let response =
        relay_to_route(state, &route, HOST_DIRECTORY_BROWSE_RESPONSE_DEADLINE, wire_request).await?;
    match response {
        C2NodeResponse::HostDirectoriesBrowsed { listing } => {
            let listing = project_host_directory_listing(listing).map_err(LightRelayError::Projection)?;
            Ok(HarnessOperatorResponseV1::HostDirectoriesBrowsed(listing))
        }
        _ => Err(LightRelayError::UnexpectedResponse),
    }
}
