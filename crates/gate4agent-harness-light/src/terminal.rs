//! Wires `gate4agent-harness-service`'s promoted-`pub` `TerminalBufferRegistry`
//! into the light harness: live C2 terminal-frame events feed the ring
//! (mirrors `gate4agent-harness-service::runtime`'s own event-loop ingestion,
//! see that module's `C2NodeEvent::TerminalFrame`/`ResyncRequired` handling),
//! and `TerminalRead` pages it. The registry itself is reused verbatim -- see
//! `gate4agent_harness_service::terminal`'s own module doc comment for why it
//! is kernel-free by construction and so a straight promotion rather than a
//! light-local reimplementation, unlike `crate::relay`'s C2/Node relay types.
//!
//! Every function here takes `&SharedTerminalRegistry` directly rather than
//! `&LightState`, matching `crate::inventory::list`'s own convention: unlike
//! `crate::inventory::handle_event`/`reconcile_topology` (which also need
//! `state.control`/`state.snapshot_gate` for a live snapshot refresh),
//! terminal ingestion/paging never touches C2 at read or ingest time -- the
//! ring is a pure, self-contained tap, so it takes only what it needs.

use std::sync::Arc;

use gate4agent_c2_protocol::{C2NodeEvent, C2Topology, NodeRoute, NodeTransportState, RoutedNodeEvent};
use gate4agent_harness_api::{
    HarnessOperatorHostErrorV1, HarnessOperatorReplyV1, HarnessOperatorResponseV1,
    HarnessRuntimeSessionAddressV1, HarnessRuntimeTerminalPageV1,
};
use gate4agent_harness_service::terminal::{terminal_frame_to_wire, TerminalBufferRegistry};
use gate4agent_node_protocol::{NodeId, WorkspaceId};
use gate4agent_observation_api::RuntimeSessionKey;
use gate4agent_types::{AgentInstanceId, SessionGeneration};
use tokio::sync::RwLock;

pub(crate) type SharedTerminalRegistry = Arc<RwLock<TerminalBufferRegistry>>;

pub(crate) fn new_shared() -> SharedTerminalRegistry {
    Arc::new(RwLock::new(TerminalBufferRegistry::default()))
}

/// Reacts to one live `RoutedNodeEvent`: ingests a `TerminalFrame` into its
/// session's ring, or drops every buffer for the event's node+incarnation on
/// `ResyncRequired` -- the exact two event kinds
/// `gate4agent-harness-service::runtime`'s own event-loop handles the same
/// way (see that module's `C2NodeEvent::TerminalFrame`/`ResyncRequired` match
/// arms). Called from `lib.rs`'s select loop the same "detached per event,
/// respecting the drain doctrine" way `crate::inventory::handle_event`
/// already is -- see that loop's own doc comment for why processing is
/// spawned off the drain loop rather than awaited inline.
pub(crate) async fn handle_event(registry: &SharedTerminalRegistry, event: &RoutedNodeEvent) {
    match &event.event {
        C2NodeEvent::TerminalFrame { address, frame } => {
            let key = RuntimeSessionKey {
                node_id: event.node_id.clone(),
                incarnation_id: event.cursor.incarnation_id,
                workspace_id: address.workspace_id.clone(),
                instance_id: address.session.instance_id,
                generation: address.session.generation,
            };
            registry.write().await.ingest(key, frame.clone());
        }
        C2NodeEvent::ResyncRequired { .. } => {
            let route = NodeRoute {
                node_id: event.node_id.clone(),
                expected_incarnation_id: event.cursor.incarnation_id,
            };
            registry.write().await.invalidate(&route);
        }
        _ => {}
    }
}

/// Drops every buffered session whose node is no longer online or whose
/// incarnation has moved on -- mirrors `crate::inventory::reconcile_topology`'s
/// own online-routes derivation exactly, so the terminal ring and the
/// runtime-inventory roster stay in lockstep on every topology change.
pub(crate) async fn reconcile_topology(registry: &SharedTerminalRegistry, topology: &C2Topology) {
    let routes: Vec<NodeRoute> = topology.nodes.iter()
        .filter(|node| node.transport == NodeTransportState::Online)
        .filter_map(|node| {
            node.current_incarnation_id.map(|expected_incarnation_id| NodeRoute {
                node_id: node.node_id.clone(),
                expected_incarnation_id,
            })
        })
        .collect();
    registry.write().await.reconcile_topology(&routes);
}

/// Serves `TerminalRead`: pages the maintained ring for `session`, honoring
/// `limit`/`after_sequence` exactly as `gate4agent-harness-service::runtime`'s
/// own `TerminalRead` handler does (the same reused `page`/`terminal_frame_to_wire`
/// calls) -- a session absent from the ring (never spawned in this process,
/// or already evicted/reconciled away) is a typed `NotFound`, not an empty
/// page: the wire's own paging contract (`after_sequence` cursor, `dropped`
/// counter, `transport_incomplete` flag) only makes sense for a session the
/// ring actually knows about.
pub(crate) async fn read(
    registry: &SharedTerminalRegistry,
    session: HarnessRuntimeSessionAddressV1,
    after_sequence: Option<u64>,
    limit: u16,
) -> HarnessOperatorReplyV1 {
    let key = match terminal_session_key(&session) {
        Ok(key) => key,
        Err(()) => {
            tracing::warn!(
                operation = "terminal-read",
                node_id = session.node_id,
                "harness-light: terminal-read session address failed local validation",
            );
            return HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::InvalidRequest };
        }
    };
    let guard = registry.read().await;
    let Some(page) = guard.page(&key, after_sequence, limit) else {
        drop(guard);
        tracing::debug!(
            operation = "terminal-read",
            node_id = session.node_id,
            instance_id = session.instance_id,
            "harness-light: no terminal frames buffered for this session",
        );
        return HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::NotFound };
    };
    let response = HarnessRuntimeTerminalPageV1 {
        session,
        frames: page.frames.into_iter()
            .map(terminal_frame_to_wire)
            .collect(),
        dropped: page.dropped,
        transport_incomplete: page.transport_incomplete,
        next_cursor: page.next_cursor,
    };
    drop(guard);
    HarnessOperatorReplyV1::Ok { response: HarnessOperatorResponseV1::TerminalRead(response) }
}

/// Light-local mirror of `gate4agent-harness-service::runtime`'s own (private)
/// `terminal_session_key`: parses the wire's client-supplied session address
/// into the ring's typed key. `Err(())` rather than a typed host error --
/// this is purely a local shape check the one caller (`read`) already turns
/// into `HarnessOperatorHostErrorV1::InvalidRequest` -- kept trivial for the
/// same reason `crate::relay::session_control_inner` builds its own
/// `SessionAddress` inline rather than through a shared helper with its own
/// error type.
fn terminal_session_key(session: &HarnessRuntimeSessionAddressV1) -> Result<RuntimeSessionKey, ()> {
    Ok(RuntimeSessionKey {
        node_id: NodeId::new(session.node_id.as_str()).map_err(|_| ())?,
        incarnation_id: session.incarnation_id.as_str().parse().map_err(|_| ())?,
        workspace_id: WorkspaceId::new(session.workspace_id.as_str()).map_err(|_| ())?,
        instance_id: AgentInstanceId(session.instance_id),
        generation: SessionGeneration(session.generation),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_node_protocol::NodeIncarnationId;
    use gate4agent_types::{
        PtyScreenState, TerminalFrame, TerminalMouseProtocolEncoding, TerminalSize,
    };

    fn sample_key(node_id: &str, incarnation: char) -> RuntimeSessionKey {
        RuntimeSessionKey {
            node_id: NodeId::new(node_id).unwrap(),
            incarnation_id: incarnation.to_string().repeat(32).parse::<NodeIncarnationId>().unwrap(),
            workspace_id: WorkspaceId::new("primary").unwrap(),
            instance_id: AgentInstanceId(1),
            generation: SessionGeneration(1),
        }
    }

    fn sample_frame(sequence: u64) -> TerminalFrame {
        TerminalFrame {
            sequence,
            size: TerminalSize { rows: 24, columns: 80 },
            cursor_row: 0,
            cursor_column: 0,
            formatted: format!("frame-{sequence}").into_bytes(),
            scrollback_formatted: Vec::new(),
            contents: String::new(),
            alternate_screen: false,
            mouse_protocol_enabled: false,
            mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
            produced_at_unix_ms: 0,
            screen_state: PtyScreenState::default(),
            bracketed_paste: None,
        }
    }

    fn sample_address(key: &RuntimeSessionKey) -> HarnessRuntimeSessionAddressV1 {
        HarnessRuntimeSessionAddressV1 {
            node_id: key.node_id.as_str().to_owned(),
            incarnation_id: key.incarnation_id.to_string(),
            workspace_id: key.workspace_id.as_str().to_owned(),
            instance_id: key.instance_id.0,
            generation: key.generation.0,
        }
    }

    /// Ingest a handful of frames, then page them back with an advancing
    /// `after_sequence` cursor -- proves the paging contract this crate's own
    /// `read` promises the wire (cursor advances to the last-returned
    /// sequence, `next_cursor` is `None` once the page is not full).
    #[tokio::test]
    async fn read_pages_ingested_frames_with_an_advancing_cursor() {
        let registry = new_shared();
        let key = sample_key("node-a", 'a');
        {
            let mut guard = registry.write().await;
            for sequence in 1..=5u64 {
                guard.ingest(key.clone(), sample_frame(sequence));
            }
        }
        let address = sample_address(&key);

        let HarnessOperatorReplyV1::Ok { response: HarnessOperatorResponseV1::TerminalRead(page) } =
            read(&registry, address.clone(), None, 2).await
        else {
            panic!("expected an Ok TerminalRead reply");
        };
        assert_eq!(page.frames.iter().map(|frame| frame.sequence).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(page.next_cursor, Some(2));
        assert_eq!(page.dropped, 0);
        assert!(!page.transport_incomplete);

        let HarnessOperatorReplyV1::Ok { response: HarnessOperatorResponseV1::TerminalRead(page) } =
            read(&registry, address, page.next_cursor, 16).await
        else {
            panic!("expected an Ok TerminalRead reply");
        };
        assert_eq!(page.frames.iter().map(|frame| frame.sequence).collect::<Vec<_>>(), [3, 4, 5]);
        assert!(page.next_cursor.is_none());
    }

    /// A session the ring has never heard of is a typed `NotFound`, not an
    /// empty page -- see `read`'s own doc comment for why.
    #[tokio::test]
    async fn read_on_an_unknown_session_is_typed_not_found() {
        let registry = new_shared();
        let address = sample_address(&sample_key("node-a", 'a'));

        let reply = read(&registry, address, None, 16).await;
        assert!(matches!(
            reply,
            HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::NotFound },
        ));
    }

    /// `handle_event` ingests `TerminalFrame` and drops the whole route on
    /// `ResyncRequired`, exactly mirroring
    /// `gate4agent-harness-service::runtime`'s own event-loop handling.
    #[tokio::test]
    async fn handle_event_ingests_frames_and_resync_required_invalidates_the_route() {
        use gate4agent_c2_protocol::NodeCursor;
        use gate4agent_node_protocol::{SessionAddress, SessionKey};

        let registry = new_shared();
        let key = sample_key("node-a", 'a');
        let cursor = NodeCursor { incarnation_id: key.incarnation_id, sequence: 1 };
        let frame_event = RoutedNodeEvent {
            node_id: key.node_id.clone(),
            cursor,
            event: C2NodeEvent::TerminalFrame {
                address: SessionAddress {
                    workspace_id: key.workspace_id.clone(),
                    session: SessionKey { instance_id: key.instance_id, generation: key.generation },
                },
                frame: sample_frame(1),
            },
        };
        handle_event(&registry, &frame_event).await;
        let address = sample_address(&key);
        assert!(matches!(
            read(&registry, address.clone(), None, 16).await,
            HarnessOperatorReplyV1::Ok { response: HarnessOperatorResponseV1::TerminalRead(_) },
        ));

        let resync_event = RoutedNodeEvent {
            node_id: key.node_id.clone(),
            cursor,
            event: C2NodeEvent::ResyncRequired { oldest_available_sequence: 0 },
        };
        handle_event(&registry, &resync_event).await;
        assert!(matches!(
            read(&registry, address, None, 16).await,
            HarnessOperatorReplyV1::Error { error: HarnessOperatorHostErrorV1::NotFound },
        ));
    }
}
