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

use gate4agent_c2_protocol::{C2NodeResponse, NodeRequest, NodeRoute};
use gate4agent_harness_api::{
    HarnessExecutionModeV1, HarnessOperatorReplyV1, HarnessOperatorResponseV1,
    HarnessRuntimeSessionAddressV1, HarnessRuntimeTerminalSizeV1, HarnessTerminalControlV1,
};
use gate4agent_node_protocol::{
    SessionAddress, SessionKey, SessionMode, SpawnDeadlineMs, SpawnIdempotencyKey, SpawnOverride,
    SpawnOverrides, SpawnProfileId, SpawnRequiredCapabilities, SpawnSpec, SpawnTarget, WorkspaceId,
    CapabilityId, SPAWN_RUNTIME_RAW_PTY_LIFECYCLE,
};
use gate4agent_node_wire::random_nonce;
use gate4agent_types::{AgentId, AgentInstanceId, SessionGeneration, TerminalControl, TerminalSize};

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
