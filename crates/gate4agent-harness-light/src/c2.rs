//! Shared C2 call helpers: route resolution against the live topology and a
//! bounded node snapshot fetch, used by both `crate::inventory` (roster
//! maintenance) and `crate::relay` (spawn profile preflight). Mirrors
//! `gate4agent-harness-service::c2::HarnessC2Adapter::exact_route`/
//! `snapshot` (both `pub`, but `HarnessC2Adapter` keeps its `control:
//! C2ReconnectingHandle` field private and exposes no generic request path
//! by design -- see that type's own doc comment -- so reusing it here would
//! still leave this crate needing its own raw `C2ReconnectingHandle` for the
//! session-verb relay, and opening a second, separate C2 connection
//! alongside it would hold two authenticated operator sessions against the
//! same C2 for no reason). This crate instead owns one `C2ReconnectingHandle`
//! directly (`gate4agent-c2-client`, the same primitive
//! `HarnessC2Adapter::connect` itself wraps) and reimplements these two
//! small, pure read-path helpers against it.

use gate4agent_c2_client::{C2LinkState, C2ReconnectingHandle};
use gate4agent_c2_protocol::{C2NodeResponse, C2NodeSnapshot, C2Topology, NodeRoute, NodeTransportState};
use gate4agent_node_protocol::{NodeId, NodeRequest};
use tokio::sync::Mutex;

use crate::error::LightRelayError;

/// Pure decision function behind [`exact_route`], factored out so the
/// reconnect-vs-genuinely-unknown distinction is testable without any live
/// C2 connection -- the light-local mirror of
/// `gate4agent-harness-service::c2::resolve_exact_route`.
///
/// While `control`'s underlying physical connection is being re-established,
/// its cached topology (`current_topology()`) is stale-but-populated: the
/// last value seen before the link died, not an empty one. Resolving a route
/// out of that cache during an outage would wrongly report a live, healthy
/// node as unknown or offline. Gating on `link_state` FIRST, before ever
/// touching `topology`, turns that into the accurate `RelayReconnecting`
/// without changing the resolution logic below it at all.
pub(crate) fn resolve_exact_route(
    link_state: C2LinkState,
    topology: &C2Topology,
    node_id: &NodeId,
) -> Result<NodeRoute, LightRelayError> {
    if link_state == C2LinkState::Reconnecting {
        return Err(LightRelayError::RelayReconnecting);
    }
    let node = topology
        .nodes
        .iter()
        .find(|node| &node.node_id == node_id)
        .ok_or(LightRelayError::UnknownNode)?;
    if node.transport != NodeTransportState::Online {
        return Err(LightRelayError::NodeOffline);
    }
    let expected_incarnation_id = node
        .current_incarnation_id
        .ok_or(LightRelayError::MissingIncarnation)?;
    Ok(NodeRoute { node_id: node_id.clone(), expected_incarnation_id })
}

/// Resolves `node_id` against the live C2 topology into a `NodeRoute`
/// pinned to its current incarnation -- the light-local equivalent of
/// `HarnessC2Adapter::exact_route`.
pub(crate) fn exact_route(
    control: &C2ReconnectingHandle,
    node_id: &str,
) -> Result<NodeRoute, LightRelayError> {
    let node_id = NodeId::new(node_id).map_err(|_| LightRelayError::InvalidRequest)?;
    resolve_exact_route(control.link_state(), &control.current_topology(), &node_id)
}

/// Fetches a fresh `C2NodeSnapshot` for `route`, verifying the reply is
/// actually routed from the same node/incarnation this call targeted.
/// Returns the node's own `event_sequence` alongside the snapshot for
/// callers (`crate::inventory::refresh_route`) that want to record it.
pub(crate) async fn fetch_snapshot(
    control: &C2ReconnectingHandle,
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
/// concurrently over the one shared `C2ReconnectingHandle` was observed (via
/// a live-instrumented run) to occasionally trip `gate4agent-c2-client`'s own
/// control-owner loop into treating an unmatched reply as fatal and tearing
/// the underlying physical connection down.
///
/// That teardown is no longer the un-recoverable failure it once was: the
/// reconnect supervisor behind `C2ReconnectingHandle` re-establishes the
/// connection on its own with backoff, so it costs a `RelayReconnecting`
/// window for every request in flight during the outage rather than
/// `C2ControlError::Closed` forever after. It is still a real hazard worth
/// avoiding on its own terms -- an unwanted reconnect churns the physical
/// connection, drops every in-flight request for the duration, and briefly
/// makes every route resolution (`crate::c2::resolve_exact_route`) report
/// `RelayReconnecting` regardless of whether the node itself is fine -- so
/// this gate stays in place unconditionally rather than only mattering while
/// `connect_local` had no recovery path at all. One at a time is not
/// measurably slower at light mode's expected scale and is unconditionally
/// safe.
pub(crate) async fn fetch_snapshot_serialized(
    control: &C2ReconnectingHandle,
    gate: &Mutex<()>,
    route: &NodeRoute,
) -> Result<(u64, C2NodeSnapshot), LightRelayError> {
    let _gate = gate.lock().await;
    fetch_snapshot(control, route).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the actual defect fix: while this process's own c2 link is
    /// reconnecting, a stale-but-non-empty cached topology must never be
    /// read as "this node does not exist" -- the link gates first,
    /// regardless of what the (stale) topology says. An empty topology is
    /// used here as the simplest stand-in for "stale": the point under test
    /// is that `resolve_exact_route` never even looks at `topology` once
    /// `link_state` is `Reconnecting`.
    #[test]
    fn resolve_exact_route_reports_relay_reconnecting_not_unknown_node_while_down() {
        let topology = C2Topology { nodes: Vec::new() };
        let node_id = NodeId::new("node-a").unwrap();
        assert!(matches!(
            resolve_exact_route(C2LinkState::Reconnecting, &topology, &node_id),
            Err(LightRelayError::RelayReconnecting)
        ));
    }

    /// The companion proof: the fix does not blanket-suppress the genuine
    /// "no such node" case -- with the link up, an absent node is still
    /// exactly `UnknownNode`, not `RelayReconnecting`.
    #[test]
    fn resolve_exact_route_still_reports_unknown_node_when_connected() {
        let topology = C2Topology { nodes: Vec::new() };
        let node_id = NodeId::new("node-a").unwrap();
        assert!(matches!(
            resolve_exact_route(C2LinkState::Connected, &topology, &node_id),
            Err(LightRelayError::UnknownNode)
        ));
    }
}
