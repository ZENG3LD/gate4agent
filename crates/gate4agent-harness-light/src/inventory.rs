//! Maintains the light harness's runtime inventory (nodes, workspaces,
//! sessions, launch inventory) from live C2 snapshots and events, and serves
//! `RuntimeInventoryList`.
//!
//! Unlike `gate4agent-harness-service::runtime::HarnessRuntimeInventoryCache`
//! (`pub(crate)`, keyed to `HarnessObservationResync` -- the full harness's
//! own gap-tracked, event-sequence-recovering resync object built by its
//! observation bridge/adapter), this cache always rebuilds a node's whole
//! entry from a fresh `NodeRequest::Snapshot` rather than applying
//! incremental deltas. That is a deliberate light-local simplification, not
//! an oversight: `HarnessObservationResync` is entangled with the kernel's
//! own observation-recovery machinery (`ObservationSupportRegistry`,
//! `RouteObservationRecovery`, gap detection) that light mode has no
//! equivalent for and does not want (light is stateless -- no SQLite
//! observation store). A full re-snapshot per relevant event is simpler,
//! trivially correct (it can never drift from whatever C2/the node
//! currently reports), and cheap enough at light mode's expected traffic.
//!
//! The projection from a raw `SlimNodeInventory` to the wire's
//! `HarnessRuntimeInventoryV1` (workspace/session/managed-session/launch-
//! inventory field mapping) is NOT reimplemented here: it is pure,
//! kernel-independent data mapping, so it is reused verbatim via
//! `gate4agent_harness_service::runtime::redact_runtime_inventory` (promoted
//! `pub` for this crate -- see that function's doc comment).

use std::collections::BTreeMap;
use std::sync::Arc;

use gate4agent_c2_client::C2ControlHandle;
use gate4agent_c2_protocol::{
    C2NodeEvent, C2Topology, NodeRoute, RoutedNodeEvent, SlimNodeInventory, NodeTransportState,
};
use gate4agent_harness_api::{
    HarnessOperatorHostErrorV1, HarnessOperatorReplyV1, HarnessOperatorResponseV1,
    HarnessRuntimeInventoryPageV1, HarnessRuntimeNodeInventoryV1,
};
use gate4agent_harness_service::runtime::redact_runtime_inventory;
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::c2::fetch_snapshot_serialized;
use crate::util::unix_time_ms;
use crate::{LightCommand, LightState};

/// Roster keyed by the wire's own `node_id: String` (not the typed
/// `NodeId`): the map's natural iteration order then already matches
/// `HarnessRuntimeInventoryPageV1`'s required strictly-increasing
/// `node_id` string order, and the `RuntimeInventoryList` pagination
/// cursor (`after_node_id: Option<String>`) needs no round trip through a
/// typed id to bound a `range` query.
pub(crate) type SharedInventory = Arc<RwLock<BTreeMap<String, HarnessRuntimeNodeInventoryV1>>>;

pub(crate) fn new_shared() -> SharedInventory {
    Arc::new(RwLock::new(BTreeMap::new()))
}

/// Rebuilds one node's inventory entry from a fresh snapshot and upserts it,
/// unless a fresher response for the same node/incarnation has already
/// landed (see [`upsert_if_fresher`]). Best-effort: a failed refresh is
/// logged and leaves the previous entry (if any) in place rather than
/// failing the caller -- every caller (the initial sweep, the event/
/// topology-driven background refresh, and the eager roster-affecting-
/// mutation refresh in `crate::relay`) treats inventory freshness as
/// advisory, never as a precondition for the request that triggered it.
///
/// Concurrency note: multiple callers can each have their own in-flight
/// `NodeRequest::Snapshot` against the same route at once -- an eager post-
/// mutation refresh (`crate::relay`) racing the background event loop's own
/// refresh for the same node, for instance. `C2ControlHandle::request`
/// gives no guarantee that responses complete in dispatch order, so without
/// the freshness guard a snapshot issued *before* a mutation could still
/// land *after* one issued after it, overwriting fresher data with stale
/// data.
///
/// Every caller runs detached from `lib.rs`'s `run_light_host` select loop
/// (a per-connection task, or one of that loop's own detached per-event/
/// per-topology-change tasks) and so cannot touch the loop-owned
/// `SubscriberRegistry` directly -- see that loop's own doc comment for the
/// drain doctrine. When this call's `UpsertOutcome` actually changed
/// something observable (`emits_inventory_changed`), it reports the fresh
/// node value back to the loop via `commands` (`try_send`, never awaited:
/// this must never block a connection/event task on the loop's own pace,
/// including the very first call any of them ever makes -- the initial
/// `sweep_online_nodes` at `start_harness_light`, which runs *before*
/// `run_light_host` has even been spawned to start draining `commands` at
/// all; see `LIGHT_COMMAND_CAPACITY`'s doc comment for why that is safe
/// regardless).
pub(crate) async fn refresh_route(
    control: &C2ControlHandle,
    snapshot_gate: &Mutex<()>,
    inventory: &SharedInventory,
    commands: &mpsc::Sender<LightCommand>,
    route: &NodeRoute,
) {
    match fetch_snapshot_serialized(control, snapshot_gate, route).await {
        Ok((event_sequence, snapshot)) => {
            let node = HarnessRuntimeNodeInventoryV1 {
                node_id: route.node_id.as_str().to_owned(),
                incarnation_id: route.expected_incarnation_id.to_string(),
                observed_at_unix_ms: unix_time_ms(),
                event_sequence,
                inventory: redact_runtime_inventory(SlimNodeInventory::from_c2_snapshot(&snapshot)),
            };
            let node_id = node.node_id.clone();
            // Cloned before the map takes ownership: mirrors
            // `gate4agent-harness-service::runtime`'s own
            // `HarnessRuntimeInventoryCache::refresh`, which pays this exact
            // same one clone unconditionally (`self.nodes.insert(..,
            // node.clone())`) so it can still hand the original back to its
            // own caller as the `RuntimeInventoryChanged` payload.
            let node_for_emit = node.clone();
            let outcome = {
                let mut guard = inventory.write().await;
                upsert_if_fresher(&mut guard, node)
            };
            if emits_inventory_changed(outcome) {
                if commands.try_send(LightCommand::InventoryChanged(node_for_emit)).is_err() {
                    tracing::debug!(
                        node_id,
                        "harness-light: dropped an inventory-changed subscription notification \
                         (command queue full or host stopped)",
                    );
                }
            }
            match outcome {
                UpsertOutcome::Inserted | UpsertOutcome::Changed => {
                    tracing::debug!(node_id, "harness-light: runtime inventory entry refreshed");
                }
                UpsertOutcome::StaleIgnored => {
                    tracing::debug!(
                        node_id,
                        event_sequence,
                        "harness-light: stale snapshot response ignored",
                    );
                }
                UpsertOutcome::Unchanged => {}
            }
        }
        Err(error) => {
            tracing::warn!(
                node_id = route.node_id.as_str(),
                error = %error,
                "harness-light: runtime inventory refresh failed",
            );
        }
    }
}

/// Whether `outcome` warrants a `RuntimeInventoryChanged` subscription push
/// -- the Eq-diff gate `refresh_route` applies before ever touching
/// `commands`: an `Unchanged` (byte-for-byte duplicate) or `StaleIgnored`
/// (superseded by an already-landed fresher response) refresh must never
/// fan a spurious event out to a live subscriber, only an
/// `Inserted`/`Changed` one that actually altered what `RuntimeInventoryList`
/// reports. Split out, mirroring `event_affects_roster`/`stale_node_ids`
/// below, so this decision is unit-testable without a live C2 connection
/// (`refresh_route` itself needs one, for the snapshot fetch the outcome is
/// computed from).
fn emits_inventory_changed(outcome: UpsertOutcome) -> bool {
    matches!(outcome, UpsertOutcome::Inserted | UpsertOutcome::Changed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpsertOutcome {
    /// No entry existed for this node id yet.
    Inserted,
    /// An entry existed and this response replaced it with different data.
    Changed,
    /// An entry existed and this response is byte-for-byte identical to it.
    Unchanged,
    /// An entry exists for the same node *and* incarnation, with a strictly
    /// higher `event_sequence` than this response -- a fresher response
    /// already landed, so this (older) one is dropped on the floor.
    StaleIgnored,
}

/// Pure upsert decision behind `refresh_route`'s freshness guard -- split
/// out so it is unit-testable against a synthetic "fresher response landed
/// first" ordering with no live C2 connection (see
/// `tests::upsert_if_fresher_*` below).
fn upsert_if_fresher(
    map: &mut BTreeMap<String, HarnessRuntimeNodeInventoryV1>,
    node: HarnessRuntimeNodeInventoryV1,
) -> UpsertOutcome {
    match map.get(&node.node_id) {
        Some(existing)
            if existing.incarnation_id == node.incarnation_id
                && existing.event_sequence > node.event_sequence =>
        {
            UpsertOutcome::StaleIgnored
        }
        Some(existing) if existing == &node => UpsertOutcome::Unchanged,
        Some(_) => {
            map.insert(node.node_id.clone(), node);
            UpsertOutcome::Changed
        }
        None => {
            map.insert(node.node_id.clone(), node);
            UpsertOutcome::Inserted
        }
    }
}

/// Refreshes every currently online node from the live topology -- the
/// initial roster sweep at `start_harness_light`, and the re-population half
/// of `reconcile_topology` after a topology change.
pub(crate) async fn sweep_online_nodes(state: &LightState) {
    let topology = state.control.current_topology();
    for node in &topology.nodes {
        if node.transport != NodeTransportState::Online {
            continue;
        }
        let Some(expected_incarnation_id) = node.current_incarnation_id else { continue };
        let route = NodeRoute { node_id: node.node_id.clone(), expected_incarnation_id };
        refresh_route(&state.control, &state.snapshot_gate, &state.inventory, &state.commands, &route).await;
    }
}

/// Drops any cached entry whose node is no longer online or whose
/// incarnation has moved on since it was cached, then re-sweeps: a node that
/// just restarted or came back online lands with a fresh entry in the same
/// pass, a node that left the fleet simply stays dropped.
pub(crate) async fn reconcile_topology(state: &LightState, topology: &C2Topology) {
    let mut online = BTreeMap::new();
    for node in &topology.nodes {
        if node.transport != NodeTransportState::Online {
            continue;
        }
        if let Some(incarnation_id) = node.current_incarnation_id {
            online.insert(node.node_id.as_str().to_owned(), incarnation_id.to_string());
        }
    }
    let stale = {
        let guard = state.inventory.read().await;
        stale_node_ids(&guard, &online)
    };
    if !stale.is_empty() {
        let mut guard = state.inventory.write().await;
        for node_id in &stale {
            guard.remove(node_id);
        }
        drop(guard);
        for node_id in &stale {
            tracing::debug!(node_id, "harness-light: runtime inventory entry dropped (topology change)");
            // `try_send`, never awaited -- same never-block-the-sender
            // rationale as `refresh_route`'s own `InventoryChanged` push
            // (this function already runs detached from the select loop
            // that owns `SubscriberRegistry`, see that loop's own doc
            // comment).
            if state.commands.try_send(LightCommand::InventoryRemoved(node_id.clone())).is_err() {
                tracing::debug!(
                    node_id,
                    "harness-light: dropped an inventory-removed subscription notification \
                     (command queue full or host stopped)",
                );
            }
        }
    }
    sweep_online_nodes(state).await;
}

/// Pure half of `reconcile_topology`: which cached node ids no longer match
/// `online` (the current topology's own online node id -> incarnation-id-
/// string map) -- either the node dropped offline/left the fleet entirely,
/// or its incarnation moved on since the cached entry was built. Split out
/// so this decision is unit-testable against synthetic inventory entries
/// with no live C2 connection (see `tests::stale_node_ids_*` below).
fn stale_node_ids(
    inventory: &BTreeMap<String, HarnessRuntimeNodeInventoryV1>,
    online: &BTreeMap<String, String>,
) -> Vec<String> {
    inventory
        .iter()
        .filter(|(node_id, entry)| online.get(*node_id) != Some(&entry.incarnation_id))
        .map(|(node_id, _)| node_id.clone())
        .collect()
}

/// Whether `event` can possibly change anything `redact_runtime_inventory`
/// projects (workspace/session/managed-session presence or status).
/// `Observation`/`ManagedObservation` (per-turn provider telemetry),
/// `ControllerChanged` (node-side write-lease ownership), and
/// `HarnessMcpReadCall` never do -- a live session emits many of these per
/// second, and refreshing on every one needlessly multiplies concurrent
/// `NodeRequest::Snapshot` pressure (see `crate::c2::fetch_snapshot_
/// serialized`'s doc comment for why that pressure matters) for zero roster
/// benefit.
fn event_affects_roster(event: &C2NodeEvent) -> bool {
    matches!(
        event,
        C2NodeEvent::Control { .. }
            | C2NodeEvent::WorkspaceAdded { .. }
            | C2NodeEvent::WorkspaceRemoved { .. }
            | C2NodeEvent::SessionRecordUpserted { .. }
            | C2NodeEvent::SessionRecordRemoved { .. }
            | C2NodeEvent::ManagedWorktreeUpserted { .. }
            | C2NodeEvent::ManagedWorktreeRemoved { .. }
            | C2NodeEvent::ResyncRequired { .. }
    )
}

/// Reacts to one live `RoutedNodeEvent`: re-snapshots that event's node if,
/// and only if, it is roster-relevant (`event_affects_roster`) and routed
/// against the node's *current* incarnation (a stale event for an
/// incarnation the topology has already moved past is ignored, the same
/// staleness guard `crate::c2::fetch_snapshot`'s own route-echo check
/// applies to a direct request).
pub(crate) async fn handle_event(state: &LightState, event: &RoutedNodeEvent) {
    if !event_affects_roster(&event.event) {
        return;
    }
    let topology = state.control.current_topology();
    let Some(node) = topology.nodes.iter().find(|node| node.node_id == event.node_id) else {
        return;
    };
    if node.transport != NodeTransportState::Online {
        return;
    }
    let Some(expected_incarnation_id) = node.current_incarnation_id else { return };
    if expected_incarnation_id != event.cursor.incarnation_id {
        return;
    }
    let route = NodeRoute { node_id: event.node_id.clone(), expected_incarnation_id };
    refresh_route(&state.control, &state.snapshot_gate, &state.inventory, &state.commands, &route).await;
}

/// Serves `RuntimeInventoryList` from the maintained roster: a plain,
/// cursor-paged read over the sorted map, no C2 round trip. `limit` is
/// trusted as already bounded by `HarnessOperatorRequestV1::validate()`
/// (run by `crate::dispatch` before this is called).
pub(crate) async fn list(
    inventory: &SharedInventory,
    after_node_id: Option<String>,
    limit: u16,
) -> HarnessOperatorReplyV1 {
    let limit = usize::from(limit);
    let guard = inventory.read().await;
    let mut iter: Box<dyn Iterator<Item = &HarnessRuntimeNodeInventoryV1> + '_> = match &after_node_id
    {
        Some(cursor) => Box::new(
            guard
                .range::<String, _>((
                    std::ops::Bound::Excluded(cursor.clone()),
                    std::ops::Bound::Unbounded,
                ))
                .map(|(_, node)| node),
        ),
        None => Box::new(guard.values()),
    };
    let nodes = iter.by_ref().take(limit).cloned().collect::<Vec<_>>();
    // A `next_cursor` is only meaningful once the page actually stopped
    // short of the full roster: probing `iter` for one more item (past what
    // `take(limit)` already consumed) is how `HarnessRuntimeInventoryPageV1
    // ::validate()`'s "`next_cursor` is `Some` iff there was more" invariant
    // is honored without a second, separate count pass over the map.
    let next_cursor = if iter.next().is_some() {
        nodes.last().map(|node| node.node_id.clone())
    } else {
        None
    };
    // `iter` must drop before `guard`: its `Box<dyn Iterator<Item = &..>>`
    // drop glue is opaque to the borrow checker, which conservatively keeps
    // `guard` borrowed for as long as `iter` is in scope otherwise.
    drop(iter);
    drop(guard);
    let page = HarnessRuntimeInventoryPageV1 { nodes, next_cursor };
    if let Err(error) = page.validate() {
        // Unreachable in practice -- every entry in the map was built by
        // this crate's own `redact_runtime_inventory` call and is already
        // wire-valid -- but a page that somehow failed validation must not
        // be sent to the client as if it were Ok; `write_operator_reply`
        // would reject it anyway (see its own `reply.validate()` call), so
        // this just gives that failure an honest, typed shape instead of a
        // dropped connection.
        tracing::warn!(error = ?error, "harness-light: runtime inventory page failed validation");
        return HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::Internal };
    }
    HarnessOperatorReplyV1::Ok { response: HarnessOperatorResponseV1::RuntimeInventory(page) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_api::HarnessRuntimeInventoryV1;

    /// A minimal, wire-valid `HarnessRuntimeNodeInventoryV1` -- the
    /// "synthetic snapshot" this module's tests build sequences of, standing
    /// in for what `refresh_route` would otherwise only ever build from a
    /// real `C2NodeSnapshot`.
    fn sample_node(node_id: &str, incarnation_id: char, event_sequence: u64) -> HarnessRuntimeNodeInventoryV1 {
        HarnessRuntimeNodeInventoryV1 {
            node_id: node_id.to_owned(),
            incarnation_id: incarnation_id.to_string().repeat(32),
            observed_at_unix_ms: 1,
            event_sequence,
            inventory: HarnessRuntimeInventoryV1 {
                enabled_providers: Vec::new(),
                workspaces: BTreeMap::new(),
                workspace_count: 0,
                workspaces_truncated: false,
                session_count: 0,
                sessions_truncated: false,
                managed_sessions: Vec::new(),
                managed_session_count: 0,
                managed_sessions_truncated: false,
                launch_inventory: None,
            },
        }
    }

    async fn seeded(nodes: impl IntoIterator<Item = HarnessRuntimeNodeInventoryV1>) -> SharedInventory {
        let inventory = new_shared();
        {
            let mut guard = inventory.write().await;
            for node in nodes {
                guard.insert(node.node_id.clone(), node);
            }
        }
        inventory
    }

    #[tokio::test]
    async fn list_pages_in_node_id_order_with_a_cursor_only_when_more_remain() {
        let inventory = seeded([
            sample_node("node-a", 'a', 1),
            sample_node("node-b", 'b', 1),
            sample_node("node-c", 'c', 1),
        ]).await;

        let HarnessOperatorReplyV1::Ok {
            response: HarnessOperatorResponseV1::RuntimeInventory(first_page),
        } = list(&inventory, None, 2).await else {
            panic!("expected an Ok RuntimeInventory reply");
        };
        assert_eq!(
            first_page.nodes.iter().map(|node| node.node_id.as_str()).collect::<Vec<_>>(),
            ["node-a", "node-b"],
        );
        assert_eq!(first_page.next_cursor.as_deref(), Some("node-b"));

        let HarnessOperatorReplyV1::Ok {
            response: HarnessOperatorResponseV1::RuntimeInventory(second_page),
        } = list(&inventory, first_page.next_cursor, 2).await else {
            panic!("expected an Ok RuntimeInventory reply");
        };
        assert_eq!(
            second_page.nodes.iter().map(|node| node.node_id.as_str()).collect::<Vec<_>>(),
            ["node-c"],
        );
        assert!(second_page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_on_an_empty_roster_returns_an_empty_page() {
        let inventory = new_shared();
        let HarnessOperatorReplyV1::Ok {
            response: HarnessOperatorResponseV1::RuntimeInventory(page),
        } = list(&inventory, None, 16).await else {
            panic!("expected an Ok RuntimeInventory reply");
        };
        assert!(page.nodes.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn stale_node_ids_drops_offline_and_incarnation_changed_entries_only() {
        let inventory = BTreeMap::from([
            ("node-a".to_owned(), sample_node("node-a", 'a', 1)),
            ("node-b".to_owned(), sample_node("node-b", 'b', 1)),
            ("node-c".to_owned(), sample_node("node-c", 'c', 1)),
        ]);
        let online = BTreeMap::from([
            // node-a: unchanged, stays cached.
            ("node-a".to_owned(), "a".repeat(32)),
            // node-b: same node id, but its incarnation moved on (e.g. a
            // restart) -- the cached entry is now stale.
            ("node-b".to_owned(), "z".repeat(32)),
            // node-c: absent from the online set entirely (offline, or left
            // the fleet) -- also stale.
            // node-d: online but never cached -- `stale_node_ids` only ever
            // reports entries the cache already holds, never additions.
        ]);

        let mut stale = stale_node_ids(&inventory, &online);
        stale.sort();
        assert_eq!(stale, vec!["node-b".to_owned(), "node-c".to_owned()]);
    }

    #[test]
    fn stale_node_ids_on_a_fully_matching_topology_reports_nothing() {
        let inventory = BTreeMap::from([("node-a".to_owned(), sample_node("node-a", 'a', 1))]);
        let online = BTreeMap::from([("node-a".to_owned(), "a".repeat(32))]);
        assert!(stale_node_ids(&inventory, &online).is_empty());
    }

    /// Reproduces the race `refresh_route`'s freshness guard exists for: two
    /// concurrent `NodeRequest::Snapshot` fetches for the same route (e.g.
    /// the eager post-spawn refresh and a live `Control{Running}` event's
    /// refresh) can complete in either order. A response carrying a lower
    /// `event_sequence` than what is already cached for the same node and
    /// incarnation must never win, no matter when it happens to land.
    #[test]
    fn upsert_if_fresher_drops_an_older_response_that_lands_after_a_newer_one() {
        let mut map = BTreeMap::new();
        assert_eq!(
            upsert_if_fresher(&mut map, sample_node("node-a", 'a', 13)),
            UpsertOutcome::Inserted,
        );
        // The stale response (sequence 3, e.g. captured while the session
        // was still `Starting`) lands after the fresher one (sequence 13,
        // `Running`) already did -- it must be ignored, not overwrite it.
        assert_eq!(
            upsert_if_fresher(&mut map, sample_node("node-a", 'a', 3)),
            UpsertOutcome::StaleIgnored,
        );
        assert_eq!(map.get("node-a").unwrap().event_sequence, 13);
    }

    #[test]
    fn upsert_if_fresher_accepts_a_newer_response_and_reports_no_change_for_a_duplicate() {
        let mut map = BTreeMap::new();
        assert_eq!(
            upsert_if_fresher(&mut map, sample_node("node-a", 'a', 3)),
            UpsertOutcome::Inserted,
        );
        assert_eq!(
            upsert_if_fresher(&mut map, sample_node("node-a", 'a', 13)),
            UpsertOutcome::Changed,
        );
        assert_eq!(
            upsert_if_fresher(&mut map, sample_node("node-a", 'a', 13)),
            UpsertOutcome::Unchanged,
        );
        assert_eq!(map.get("node-a").unwrap().event_sequence, 13);
    }

    /// The Eq-diff gate `refresh_route` applies before ever touching
    /// `commands`: only an outcome that actually altered the cached entry
    /// (`Inserted`/`Changed`) warrants a `RuntimeInventoryChanged` push --
    /// an `Unchanged` (duplicate) or `StaleIgnored` (superseded) refresh
    /// must never fan a spurious event out to a live subscriber.
    #[test]
    fn emits_inventory_changed_gates_on_inserted_or_changed_only() {
        assert!(emits_inventory_changed(UpsertOutcome::Inserted));
        assert!(emits_inventory_changed(UpsertOutcome::Changed));
        assert!(!emits_inventory_changed(UpsertOutcome::Unchanged));
        assert!(!emits_inventory_changed(UpsertOutcome::StaleIgnored));
    }

    /// A lower `event_sequence` for a *different* incarnation is not stale
    /// at all -- it is the node's next life restarting its own sequence
    /// space, and must always win.
    #[test]
    fn upsert_if_fresher_always_accepts_a_new_incarnation_regardless_of_sequence() {
        let mut map = BTreeMap::new();
        assert_eq!(
            upsert_if_fresher(&mut map, sample_node("node-a", 'a', 13)),
            UpsertOutcome::Inserted,
        );
        assert_eq!(
            upsert_if_fresher(&mut map, sample_node("node-a", 'b', 1)),
            UpsertOutcome::Changed,
        );
        let current = map.get("node-a").unwrap();
        assert_eq!(current.event_sequence, 1);
        assert_eq!(current.incarnation_id, "b".repeat(32));
    }
}
