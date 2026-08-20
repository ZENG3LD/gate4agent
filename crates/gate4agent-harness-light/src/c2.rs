//! Shared C2 call helpers: route resolution against the live topology and a
//! bounded node snapshot fetch, used by both `crate::inventory` (roster
//! maintenance) and `crate::relay` (spawn profile preflight). Mirrors
//! `gate4agent-harness-service::c2::HarnessC2Adapter::exact_route`/
//! `snapshot` (both `pub`, but `HarnessC2Adapter` keeps its `control:
//! C2ControlHandle` field private and exposes no generic request path by
//! design -- see that type's own doc comment -- so reusing it here would
//! still leave this crate needing its own raw `C2ControlHandle` for the
//! session-verb relay, and opening a second, separate C2 connection
//! alongside it would hold two authenticated operator sessions against the
//! same C2 for no reason). This crate instead owns one `C2ControlHandle`
//! directly (`gate4agent-c2-client`, the same primitive
//! `HarnessC2Adapter::connect` itself wraps) and reimplements these two
//! small, pure read-path helpers against it.

use gate4agent_c2_client::C2ControlHandle;
use gate4agent_c2_protocol::{C2NodeResponse, C2NodeSnapshot, NodeRoute, NodeTransportState};
use gate4agent_node_protocol::{NodeId, NodeRequest};
use tokio::sync::Mutex;

use crate::error::LightRelayError;

/// Resolves `node_id` against the live C2 topology into a `NodeRoute`
/// pinned to its current incarnation -- the light-local equivalent of
/// `HarnessC2Adapter::exact_route`.
pub(crate) fn exact_route(
    control: &C2ControlHandle,
    node_id: &str,
) -> Result<NodeRoute, LightRelayError> {
    let node_id = NodeId::new(node_id).map_err(|_| LightRelayError::InvalidRequest)?;
    let topology = control.current_topology();
    let node = topology
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .ok_or(LightRelayError::UnknownNode)?;
    if node.transport != NodeTransportState::Online {
        return Err(LightRelayError::NodeOffline);
    }
    let expected_incarnation_id = node
        .current_incarnation_id
        .ok_or(LightRelayError::MissingIncarnation)?;
    Ok(NodeRoute { node_id, expected_incarnation_id })
}

/// Fetches a fresh `C2NodeSnapshot` for `route`, verifying the reply is
/// actually routed from the same node/incarnation this call targeted.
/// Returns the node's own `event_sequence` alongside the snapshot for
/// callers (`crate::inventory::refresh_route`) that want to record it.
pub(crate) async fn fetch_snapshot(
    control: &C2ControlHandle,
    route: &NodeRoute,
) -> Result<(u64, C2NodeSnapshot), LightRelayError> {
    let routed = control.request(route.clone(), NodeRequest::Snapshot).await?;
    if routed.node_id != route.node_id || routed.incarnation_id != route.expected_incarnation_id {
        return Err(LightRelayError::IncarnationChanged);
    }
    match routed.response {
        Ok(C2NodeResponse::Snapshot { event_sequence, snapshot, .. }) => {
            Ok((event_sequence, snapshot))
        }
        Ok(_) => Err(LightRelayError::UnexpectedResponse),
        Err(failure) => Err(LightRelayError::NodeRejected(failure.code)),
    }
}

/// Serializes every `NodeRequest::Snapshot` this process issues through one
/// gate (`LightState::snapshot_gate`), so no two are ever in flight at once.
///
/// Root-cause fix: `crate::inventory`'s roster maintenance (the initial
/// sweep, every live-event refresh, every roster-affecting-mutation eager
/// refresh) and `crate::relay`'s spawn preflight can all want a fresh
/// snapshot for the very same node within milliseconds of each other --
/// spawning a session alone fires a preflight snapshot, the eager post-
/// spawn refresh, *and* the live `Control`/`SessionRecordUpserted` events
/// the node emits for that same spawn each trigger their own. Firing these
/// concurrently over the one shared `C2ControlHandle` was observed (via a
/// live-instrumented run) to occasionally trip `gate4agent-c2-client`'s own
/// control-owner loop into treating an unmatched reply as fatal and tearing
/// the whole connection down (`C2ControlError::Closed` from then on, for
/// every request and the whole event/topology stream, for the rest of the
/// process's life) -- exactly the scenario this crate's design must never
/// create for itself. One at a time is not measurably slower at light
/// mode's expected scale and is unconditionally safe.
pub(crate) async fn fetch_snapshot_serialized(
    control: &C2ControlHandle,
    gate: &Mutex<()>,
    route: &NodeRoute,
) -> Result<(u64, C2NodeSnapshot), LightRelayError> {
    let _gate = gate.lock().await;
    fetch_snapshot(control, route).await
}
