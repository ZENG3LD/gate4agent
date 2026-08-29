//! RAM-only, bounded ring buffer of raw PTY terminal frames, keyed by
//! [`RuntimeSessionKey`]. Terminal bytes captured here never cross into
//! `gate4agent-observation-api`/`-engine`/`-service` and are never journaled
//! into the SQLite-backed observation store -- `routed_event_to_ingress`
//! (`runtime.rs`) stays the sole SQLite ingress path and is untouched by this
//! module. This registry is a parallel, in-memory-only tap.
//!
//! `pub`: this whole module is reused verbatim by `gate4agent-harness-light`
//! (`crate::terminal` there) to serve `TerminalRead`. It is kernel-free by
//! construction (no `HarnessService`/SQLite/`HarnessC2Adapter` dependency --
//! only `NodeRoute`, wire types, and the plain `RuntimeSessionKey`), so unlike
//! the adapter-entangled `Prepared*`/`Pending*` C2 relay types in `c2.rs`,
//! this is a straight promotion with no light-local reimplementation.

use crate::runtime::OperatorRequestLogIdentity;
use gate4agent_c2_protocol::NodeRoute;
use gate4agent_harness_api::{
    HarnessOperatorTerminalEventV1, HarnessRuntimeMouseProtocolEncodingV1,
    HarnessRuntimeSessionAddressV1, HarnessRuntimeTerminalFrameV1, HarnessRuntimeTerminalSizeV1,
    PtyScreenStateV1, HARNESS_OPERATOR_WIRE_VERSION_V13,
};
use gate4agent_observation_api::RuntimeSessionKey;
use gate4agent_types::{PtyScreenState, TerminalFrame, TerminalMouseProtocolEncoding};
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::mpsc;

const TERMINAL_FRAMES_PER_SESSION_MAX: usize = 64;
const TERMINAL_BYTES_PER_SESSION_MAX: usize = 512 * 1024; // 512 KiB/session
const TERMINAL_SESSIONS_MAX: usize = 64; // worst case ~32 MiB total

#[derive(Default)]
pub struct TerminalBufferRegistry {
    sessions: HashMap<RuntimeSessionKey, TerminalSessionBuffer>,
    touch_order: VecDeque<RuntimeSessionKey>, // LRU eviction, most-recently-used at the back
}

#[derive(Default)]
struct TerminalSessionBuffer {
    frames: VecDeque<TerminalFrame>,
    dropped: u64,
    bytes: usize,
}

pub struct TerminalPageResult<'a> {
    pub frames: Vec<&'a TerminalFrame>,
    pub dropped: u64,
    pub transport_incomplete: bool,
    pub next_cursor: Option<u64>,
}

impl TerminalBufferRegistry {
    /// Appends a freshly received frame to its session's ring, evicting the
    /// oldest frame(s) when the per-session frame-count or byte budget is
    /// exceeded. Frames that are not strictly newer than the session's most
    /// recent retained frame are ignored (stale/duplicate delivery) rather
    /// than counted as a drop -- `dropped` tracks capacity evictions only.
    pub fn ingest(&mut self, key: RuntimeSessionKey, frame: TerminalFrame) {
        let is_new_session = !self.sessions.contains_key(&key);
        if is_new_session && self.sessions.len() >= TERMINAL_SESSIONS_MAX {
            if let Some(evicted) = self.touch_order.pop_front() {
                self.sessions.remove(&evicted);
            }
        }
        if let Some(position) = self.touch_order.iter().position(|touched| touched == &key) {
            self.touch_order.remove(position);
        }
        self.touch_order.push_back(key.clone());
        let buffer = self.sessions.entry(key).or_default();
        if buffer.frames.back().is_some_and(|last| frame.sequence <= last.sequence) {
            return;
        }
        buffer.bytes = buffer.bytes.saturating_add(frame_byte_footprint(&frame));
        buffer.frames.push_back(frame);
        while buffer.frames.len() > 1
            && (buffer.frames.len() > TERMINAL_FRAMES_PER_SESSION_MAX
                || buffer.bytes > TERMINAL_BYTES_PER_SESSION_MAX)
        {
            if let Some(evicted) = buffer.frames.pop_front() {
                buffer.bytes = buffer.bytes.saturating_sub(frame_byte_footprint(&evicted));
                buffer.dropped = buffer.dropped.saturating_add(1);
            }
        }
    }

    /// Drops every session buffered under `route`'s node+incarnation.
    /// Mirrors `HarnessRuntimeInventoryCache::invalidate`.
    pub fn invalidate(&mut self, route: &NodeRoute) {
        self.sessions.retain(|key, _| {
            !(key.node_id == route.node_id && key.incarnation_id == route.expected_incarnation_id)
        });
        self.prune_touch_order();
    }

    /// Drops every session whose node+incarnation is no longer part of the
    /// current topology. Mirrors `HarnessRuntimeInventoryCache::reconcile_topology`.
    pub fn reconcile_topology(&mut self, routes: &[NodeRoute]) {
        self.sessions.retain(|key, _| {
            routes.iter().any(|route| {
                route.node_id == key.node_id && route.expected_incarnation_id == key.incarnation_id
            })
        });
        self.prune_touch_order();
    }

    fn prune_touch_order(&mut self) {
        let remaining: HashSet<&RuntimeSessionKey> = self.sessions.keys().collect();
        self.touch_order.retain(|key| remaining.contains(key));
    }

    pub fn page(
        &self,
        key: &RuntimeSessionKey,
        after_sequence: Option<u64>,
        limit: u16,
    ) -> Option<TerminalPageResult<'_>> {
        let buffer = self.sessions.get(key)?;
        let oldest_retained = buffer.frames.front().map(|frame| frame.sequence);
        let transport_incomplete = match (after_sequence, oldest_retained) {
            (Some(after), Some(oldest)) => after.saturating_add(1) < oldest,
            _ => false,
        };
        let mut candidates = buffer.frames.iter()
            .filter(|frame| after_sequence.map_or(true, |after| frame.sequence > after))
            .collect::<Vec<_>>();
        let has_more = candidates.len() > usize::from(limit);
        candidates.truncate(usize::from(limit));
        let next_cursor = if has_more {
            candidates.last().map(|frame| frame.sequence)
        } else {
            None
        };
        Some(TerminalPageResult {
            frames: candidates,
            dropped: buffer.dropped,
            transport_incomplete,
            next_cursor,
        })
    }

    /// The single newest frame for `key`, or `None` if the ring has never
    /// seen this session -- the seed a fresh `SubscribeTerminal` sends
    /// immediately after registering (never the whole backlog: a push
    /// subscription only ever needs "what does the screen look like right
    /// now", unlike `TerminalRead`'s cursor-based catch-up paging).
    pub fn latest(&self, key: &RuntimeSessionKey) -> Option<&TerminalFrame> {
        self.sessions.get(key)?.frames.back()
    }
}

fn frame_byte_footprint(frame: &TerminalFrame) -> usize {
    frame.formatted.len()
        + frame.scrollback_formatted.iter().map(Vec::len).fold(0usize, usize::saturating_add)
}

// Not a `From` impl: both `TerminalFrame` (gate4agent-types) and
// `HarnessRuntimeTerminalFrameV1` (gate4agent-harness-api) are foreign to
// this crate, so the orphan rule forbids implementing the foreign `From`
// trait for a foreign type here. This crate is the only one that depends on
// both sides, so the translation lives here as a plain function instead.
///
/// `wire_version` is the REQUESTING PEER's own declared
/// `HarnessOperatorEnvelopeV1::version` -- never a default or a constant --
/// because `screen_state` is only ever present in the JSON for a peer that
/// declared `>= HARNESS_OPERATOR_WIRE_VERSION_V13`; a pre-V13 peer's
/// `deny_unknown_fields` decoder would reject the whole frame if the key
/// were sent regardless. Both call paths of this function -- the poll
/// (`TerminalRead`) and push (`SubscribeTerminal`) sides -- know their own
/// connection's declared version at the point they call this, unlike
/// `redact_runtime_inventory`'s cache-refresh call sites, which are
/// asynchronous background refreshers shared by many concurrently
/// connected peers and so have no single "the connection" to read a
/// version from; that is why this function threads a real per-call version
/// and `redact_runtime_inventory` does not (see this crate's own report on
/// that gap).
pub fn terminal_frame_to_wire(
    frame: &TerminalFrame,
    wire_version: u16,
) -> HarnessRuntimeTerminalFrameV1 {
    HarnessRuntimeTerminalFrameV1 {
        sequence: frame.sequence,
        size: HarnessRuntimeTerminalSizeV1 {
            rows: frame.size.rows,
            columns: frame.size.columns,
        },
        cursor_row: frame.cursor_row,
        cursor_column: frame.cursor_column,
        formatted: frame.formatted.clone(),
        scrollback_formatted: frame.scrollback_formatted.clone(),
        alternate_screen: frame.alternate_screen,
        mouse_protocol_enabled: frame.mouse_protocol_enabled,
        mouse_protocol_encoding: match frame.mouse_protocol_encoding {
            TerminalMouseProtocolEncoding::Default => {
                HarnessRuntimeMouseProtocolEncodingV1::Default
            }
            TerminalMouseProtocolEncoding::Utf8 => HarnessRuntimeMouseProtocolEncodingV1::Utf8,
            TerminalMouseProtocolEncoding::Sgr => HarnessRuntimeMouseProtocolEncodingV1::Sgr,
        },
        // Carried through unchanged onto the operator wire -- see
        // `TerminalFrame::produced_at_unix_ms`'s own doc for why no hop,
        // including this one, may recompute it.
        produced_at_unix_ms: frame.produced_at_unix_ms,
        screen_state: (wire_version >= HARNESS_OPERATOR_WIRE_VERSION_V13)
            .then(|| map_screen_state(&frame.screen_state)),
    }
}

/// Maps the node's screen classification onto its hand-mirrored wire shape.
/// Whether the field is even carried on the wire (`Option`-wrapping) is the
/// CALLER's decision -- it depends on the requesting peer's declared wire
/// version -- but the variant mapping itself never does, so it is factored
/// out once here rather than duplicated at every call site that carries a
/// `PtyScreenState` onto the operator wire.
///
/// The version gate belongs wherever the value stops being shared. A
/// `TerminalFrame` is converted fresh for exactly one recipient every time
/// (`terminal_frame_to_wire`, above), so its gate sits right here, at
/// construction. `HarnessRuntimeNodeInventoryV1` is the opposite: it is
/// built once by `redact_runtime_inventory` (`runtime.rs`) and cached,
/// shared by every currently polling/subscribed peer regardless of that
/// peer's own declared version, so gating it here would either leak the
/// field to a pre-V13 peer or hide it from a V13 one -- see that
/// function's own doc comment. Its gate instead lives at
/// `downgrade_runtime_inventory` (`runtime.rs`), applied at each of the
/// per-connection edges where the shared cached value actually becomes one
/// connection's own copy (`write_operator_event`, the `RuntimeInventoryList`
/// poll handlers in both harnesses). The asymmetry between the two is
/// deliberate, not an oversight one side forgot to apply to the other.
pub fn map_screen_state(state: &PtyScreenState) -> PtyScreenStateV1 {
    match state {
        PtyScreenState::Unknown => PtyScreenStateV1::Unknown,
        PtyScreenState::NotAgent { observed_process } => {
            PtyScreenStateV1::NotAgent { observed_process: observed_process.clone() }
        }
        PtyScreenState::OperatorGate { gate } => {
            PtyScreenStateV1::OperatorGate { gate: gate.clone() }
        }
        PtyScreenState::Failing { reason } => {
            PtyScreenStateV1::Failing { reason: reason.clone() }
        }
        PtyScreenState::Ready => PtyScreenStateV1::Ready,
    }
}

// Not a `From` impl for the same orphan-rule reason `terminal_frame_to_wire`
// above isn't one: `RuntimeSessionKey` (gate4agent-observation-api) and
// `HarnessRuntimeSessionAddressV1` (gate4agent-harness-api) are both foreign
// to this crate. The reverse direction of `runtime::terminal_session_key`
// (which parses a client-supplied wire address into this same key type) --
// needed here because `TerminalSubscriberRegistry::publish`/`send_to` only
// ever see the internal `RuntimeSessionKey` the ingest path already built,
// never the client's own wire-shaped request.
fn session_key_to_address(key: &RuntimeSessionKey) -> HarnessRuntimeSessionAddressV1 {
    HarnessRuntimeSessionAddressV1 {
        node_id: key.node_id.as_str().to_owned(),
        incarnation_id: key.incarnation_id.to_string(),
        workspace_id: key.workspace_id.as_str().to_owned(),
        instance_id: key.instance_id.0,
        generation: key.generation.0,
    }
}

/// Own dedicated pool, mirroring `HOST_SUBSCRIBER_LIMIT`'s isolation
/// rationale: a terminal-push connection must never compete with the one
/// task/run/node `SubscribeEvents` slot, or vice versa.
pub const HOST_TERMINAL_SUBSCRIBER_LIMIT: usize = 8;
/// Coalesced, not queued (see `TerminalSubscriberRegistry`'s own doc
/// comment) -- this only needs headroom for one in-flight frame per
/// subscribed session before the forwarder drains it, so it is sized off
/// `HARNESS_TERMINAL_SUBSCRIPTION_SESSIONS_MAX` with 2x margin, not off
/// `HOST_SUBSCRIBER_QUEUE_CAPACITY` (256, sized for a `Lagged`-tolerant
/// FIFO of unrelated events -- see that constant's own doc comment,
/// `runtime.rs`).
pub const HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY: usize = 64;

/// One connection's worth of push-terminal subscription state, owned
/// entirely by the select loop. Registered via `HostCommand::
/// SubscribeTerminal`; pruned the moment its `sender` reports closed
/// (mirrors `HarnessEventSubscriber`'s own lifecycle exactly).
struct TerminalSubscriber {
    id: u64,
    sender: mpsc::Sender<HarnessOperatorTerminalEventV1>,
    sessions: HashSet<RuntimeSessionKey>,
    /// Coalescing slot: at most one pending frame per session, replaced
    /// (never queued) while the subscriber's channel is full.
    pending: HashMap<RuntimeSessionKey, TerminalFrame>,
    /// How many frames were coalesced away (replaced in `pending` before
    /// ever being sent) for a session since the last frame this subscriber
    /// actually received for it. Consumed (and reset to absent, i.e. 0) the
    /// moment a frame for that session is actually delivered -- see
    /// `HarnessOperatorTerminalEventV1::TerminalFrame::coalesced_since_last`'s
    /// own doc comment for what a reader does with this number.
    coalesced: HashMap<RuntimeSessionKey, u32>,
    /// Per-subscription monotonic; never resets. Unlike `HarnessEventSubscriber
    /// ::next_sequence` there is no `Lagged`/`SnapshotBaseline` pair to reset
    /// across -- see `TerminalSubscriberRegistry`'s own doc comment for why
    /// this registry has no such pair at all.
    next_sequence: u64,
    identity: OperatorRequestLogIdentity,
    /// This connection's own declared `HarnessOperatorEnvelopeV1::version`,
    /// set once at `insert` from the value `handle_connection` already
    /// validated for it and never revisited afterward (a push subscription
    /// lives for the whole connection, so there is no later point to read a
    /// fresher one from). Gates `screen_state` on every `TerminalFrame`
    /// event this subscriber receives -- see `terminal_frame_to_wire`.
    wire_version: u16,
}

impl TerminalSubscriber {
    /// Attempts an immediate delivery of `frame` for `key`; on `Full`,
    /// stashes it into `pending` instead of queuing behind it or marking the
    /// subscriber stale -- see `TerminalSubscriberRegistry`'s own doc
    /// comment for why a terminal frame is disposable in a way a
    /// task/run/node change is not. Returns `true` if the subscriber's
    /// channel is discovered closed, so the caller can prune it exactly the
    /// way `SubscriberSendOutcome::Closed` (`runtime.rs`) does for the
    /// task/run/node registry.
    fn deliver(&mut self, key: &RuntimeSessionKey, frame: TerminalFrame) -> bool {
        // Taken, not merely read: a successful send below consumes this
        // count (the recipient is told exactly how much it missed since the
        // last frame it actually got); a `Full` outcome puts it straight
        // back, since nothing was actually delivered to reset it against.
        let coalesced_since_last = self.coalesced.remove(key).unwrap_or(0);
        let sequence = self.next_sequence;
        let event = HarnessOperatorTerminalEventV1::TerminalFrame {
            sequence,
            session: session_key_to_address(key),
            frame: terminal_frame_to_wire(&frame, self.wire_version),
            coalesced_since_last,
        };
        match self.sender.try_send(event) {
            Ok(()) => {
                self.next_sequence = self.next_sequence.wrapping_add(1);
                false
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if coalesced_since_last > 0 {
                    self.coalesced.insert(key.clone(), coalesced_since_last);
                }
                self.pending.insert(key.clone(), frame);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => true,
        }
    }
}

/// Sibling to `gate4agent_harness_service::runtime::SubscriberRegistry`,
/// deliberately NOT sharing its `Vec<HarnessEventSubscriber>`/queue: that
/// queue's overflow discipline is `Lagged` + a full `SnapshotBaseline`
/// rebuild, which is both wrong for a terminal frame (there is no
/// "task/run/node" to resync, only a screen that is already self-contained)
/// and unaffordable at terminal-frame volume (a single busy PTY filling that
/// queue would force every task/run/node the subscriber holds to be
/// re-sent). This registry coalesces instead: a frame that cannot be sent
/// immediately replaces whatever was already waiting for that session
/// (`TerminalSubscriber::pending`), and delivery is retried opportunistically
/// on the next `flush_pending` pass -- never a resync obligation, because
/// every terminal frame the pipeline produces is already a full,
/// self-contained screen (backlog item 4 is explicit that deltas are out of
/// scope here).
///
/// Reuses every applicable idiom from `SubscriberRegistry` instead of
/// inventing a new one: loop-owned (no Arc/Mutex, single-writer-core, same as
/// `SubscriberRegistry`), a plain `Vec` (subscriber count capped tiny by
/// `HOST_TERMINAL_SUBSCRIBER_LIMIT` the same way, so linear scan/removal
/// costs nothing observable), `try_send`+`Closed` pruning, and a keepalive
/// for the same dead-peer-detection reason (`SubscriberRegistry`'s own doc
/// comment on `HOST_SUBSCRIBER_KEEPALIVE_INTERVAL` applies verbatim here,
/// `runtime.rs`). The one thing it does NOT reuse is `emit`'s `Full ->
/// needs_baseline -> Lagged` path.
#[derive(Default)]
pub struct TerminalSubscriberRegistry {
    subscribers: Vec<TerminalSubscriber>,
    next_id: u64,
}

impl TerminalSubscriberRegistry {
    pub fn insert(
        &mut self,
        sender: mpsc::Sender<HarnessOperatorTerminalEventV1>,
        sessions: HashSet<RuntimeSessionKey>,
        identity: OperatorRequestLogIdentity,
        wire_version: u16,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.subscribers.push(TerminalSubscriber {
            id,
            sender,
            sessions,
            pending: HashMap::new(),
            coalesced: HashMap::new(),
            next_sequence: 0,
            identity,
            wire_version,
        });
        id
    }

    pub fn is_empty(&self) -> bool {
        self.subscribers.is_empty()
    }

    fn remove_at(&mut self, index: usize) {
        let removed = self.subscribers.swap_remove(index);
        tracing::info!(
            subscriber_id = removed.id,
            operation = %removed.identity.operation,
            node_id = removed.identity.node_id(),
            workspace_id = removed.identity.workspace_id(),
            "harness terminal event subscriber closed",
        );
    }

    /// Seeds one session for one subscriber right after `insert` -- the
    /// terminal-push counterpart to `SubscriberRegistry::send_to`'s
    /// mandatory first `SnapshotBaseline` push. A no-op (not an error) when
    /// `frame` is the tail of an empty ring's caller: see the call site in
    /// `runtime.rs`'s `HostCommand::SubscribeTerminal` arm, which only calls
    /// this when `TerminalBufferRegistry::latest` actually returned a frame
    /// -- a session with no output yet simply gets nothing here, never a
    /// wire error.
    pub fn send_to(&mut self, id: u64, key: &RuntimeSessionKey, frame: TerminalFrame) {
        let Some(index) = self.subscribers.iter().position(|subscriber| subscriber.id == id)
        else {
            return;
        };
        if self.subscribers[index].deliver(key, frame) {
            self.remove_at(index);
        }
    }

    /// Called from the ingest call site for every incoming frame (cheap
    /// no-op when `subscribers.is_empty()`, matching `notify_task_changed`'s
    /// own early-return convention, `runtime.rs`). Fans `frame` out to every
    /// subscriber whose `sessions` contains `key`. A subscriber already
    /// sitting on a stashed `pending` frame for this session skips the
    /// direct-send attempt entirely and just replaces that stash: trying a
    /// direct send anyway could race ahead of the still-undelivered older
    /// frame and land out of order the moment `flush_pending` later drains
    /// it, which would walk the client's screen backward in time with no
    /// guarantee anything newer ever arrives to correct it. Skipping the
    /// direct attempt keeps `pending` the single source of truth for "what
    /// this session will next deliver" once anything is stashed for it.
    pub fn publish(&mut self, key: &RuntimeSessionKey, frame: &TerminalFrame) {
        if self.subscribers.is_empty() { return; }
        let mut index = 0;
        while index < self.subscribers.len() {
            let subscriber = &mut self.subscribers[index];
            if !subscriber.sessions.contains(key) {
                index += 1;
                continue;
            }
            if subscriber.pending.contains_key(key) {
                subscriber.pending.insert(key.clone(), frame.clone());
                *subscriber.coalesced.entry(key.clone()).or_insert(0) += 1;
                index += 1;
                continue;
            }
            if subscriber.deliver(key, frame.clone()) {
                self.remove_at(index);
            } else {
                index += 1;
            }
        }
    }

    /// Runs once per select-loop pass (mirrors `SubscriberRegistry::
    /// recover_lagged`'s placement): retries every subscriber's stashed
    /// `pending` frames, so a session whose LAST update happened to land
    /// while the channel was full is not left waiting for another update
    /// that may never come.
    pub fn flush_pending(&mut self) {
        let mut index = 0;
        while index < self.subscribers.len() {
            let subscriber = &mut self.subscribers[index];
            if subscriber.pending.is_empty() {
                index += 1;
                continue;
            }
            let keys: Vec<RuntimeSessionKey> = subscriber.pending.keys().cloned().collect();
            let mut closed = false;
            for key in keys {
                let Some(frame) = subscriber.pending.remove(&key) else { continue; };
                if subscriber.deliver(&key, frame) {
                    closed = true;
                    break;
                }
            }
            if closed {
                self.remove_at(index);
            } else {
                index += 1;
            }
        }
    }

    /// Ping fan-out, mirrors `emit_subscriber_keepalive` (`runtime.rs`) --
    /// same `HOST_SUBSCRIBER_KEEPALIVE_INTERVAL` tick, reused rather than
    /// duplicated, drives this too (see that constant's own doc comment for
    /// why a subscriber that abandons its connection without a real event
    /// ever needing to reach it would otherwise sit occupying its slot
    /// indefinitely -- the same failure mode applies here, on this
    /// registry's own, separate `HOST_TERMINAL_SUBSCRIBER_LIMIT` pool).
    pub fn keepalive(&mut self) {
        let mut index = 0;
        while index < self.subscribers.len() {
            let subscriber = &mut self.subscribers[index];
            let sequence = subscriber.next_sequence;
            match subscriber.sender.try_send(HarnessOperatorTerminalEventV1::Ping { sequence }) {
                Ok(()) => {
                    subscriber.next_sequence = subscriber.next_sequence.wrapping_add(1);
                    index += 1;
                }
                // A full queue on a keepalive is not a coalescing occasion
                // (there is no per-session frame to replace) and not a
                // failure either -- the subscriber is simply busy draining
                // real frames, which is itself proof it is alive. Leave it
                // be; the next tick tries again.
                Err(mpsc::error::TrySendError::Full(_)) => index += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => self.remove_at(index),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_api::{HarnessOperatorRequestV1, HARNESS_OPERATOR_WIRE_VERSION_V12};
    use gate4agent_observation_api::{
        AgentInstanceId, NodeId, NodeIncarnationId, SessionGeneration, WorkspaceId,
    };
    use gate4agent_types::{PtyScreenState, TerminalSize};

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
        }
    }

    /// The version gate at the one place `screen_state` actually crosses
    /// onto the operator wire: the same input frame produces `None` for a
    /// pre-V13 peer and `Some` reflecting its real classification for a V13
    /// peer, asserted side by side so neither path can silently forget the
    /// other's expectation.
    #[test]
    fn terminal_frame_to_wire_gates_screen_state_on_the_callers_declared_wire_version() {
        let mut frame = sample_frame(1);
        frame.screen_state = PtyScreenState::OperatorGate { gate: "workspace-trust".to_owned() };

        let pre_v13 = terminal_frame_to_wire(&frame, HARNESS_OPERATOR_WIRE_VERSION_V12);
        assert!(pre_v13.screen_state.is_none());

        let v13 = terminal_frame_to_wire(&frame, HARNESS_OPERATOR_WIRE_VERSION_V13);
        assert_eq!(v13.screen_state, Some(map_screen_state(&frame.screen_state)));
    }

    // `sessions: Vec::new()` is structurally invalid for `SubscribeTerminal`
    // itself (`validate()` rejects an empty list), but `describe()` never
    // validates -- it only reads the request's shape to build a log
    // identity, exactly like `SubscriberRegistry`'s own tests build one from
    // a bare `HarnessOperatorRequestV1::SubscribeEvents {}`.
    fn subscribe_terminal_identity() -> OperatorRequestLogIdentity {
        OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::SubscribeTerminal { sessions: Vec::new() },
        )
    }

    /// Sibling test to `TerminalBufferRegistry`'s existing `page`-based
    /// coverage (exercised in `gate4agent-harness-light`'s own test module):
    /// `latest` returns `None` for a session the ring has never seen, and
    /// the newest ingested frame once it has.
    #[test]
    fn latest_returns_the_newest_ingested_frame_or_none_for_an_unknown_session() {
        let mut registry = TerminalBufferRegistry::default();
        let key = sample_key("node-a", 'a');
        assert!(registry.latest(&key).is_none());
        for sequence in 1..=3u64 {
            registry.ingest(key.clone(), sample_frame(sequence));
        }
        assert_eq!(registry.latest(&key).unwrap().sequence, 3);
    }

    #[test]
    fn publish_delivers_immediately_when_the_channel_has_room() {
        let mut registry = TerminalSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, mut receiver) = mpsc::channel(HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);

        registry.publish(&key, &sample_frame(1));

        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorTerminalEventV1::TerminalFrame { sequence: 0, coalesced_since_last: 0, .. },
        ));
        assert!(receiver.try_recv().is_err(), "exactly one frame for one publish");
    }

    /// The direct, code-level proof of item 6's isolation half: a subscriber
    /// that never asked for `key` must never see anything published under it,
    /// even while other subscribers (none, here) do.
    #[test]
    fn publish_never_touches_a_subscriber_not_subscribed_to_that_session() {
        let mut registry = TerminalSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let other_key = sample_key("node-b", 'b');
        let (sender, mut receiver) = mpsc::channel(HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(other_key);
        registry.insert(sender, sessions, subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);

        registry.publish(&key, &sample_frame(1));

        assert!(receiver.try_recv().is_err());
    }

    /// The coalescing discipline itself: a frame that cannot be sent
    /// immediately replaces whatever was already waiting for that session,
    /// and `coalesced` increments by exactly one per replacement -- not two,
    /// nor once per publish call regardless of whether anything was actually
    /// replaced.
    #[test]
    fn publish_coalesces_a_still_undrained_session_without_double_counting() {
        let mut registry = TerminalSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, receiver) = mpsc::channel(1);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);

        // First publish is sent directly: the capacity-1 channel starts
        // empty, nothing is stashed.
        registry.publish(&key, &sample_frame(1));
        assert!(registry.subscribers[0].pending.is_empty());

        // Second publish: the channel is still full (the receiver has not
        // drained it), so this one is stashed. Nothing was already pending,
        // so no coalesce is counted for it yet.
        registry.publish(&key, &sample_frame(2));
        assert_eq!(registry.subscribers[0].pending.get(&key).unwrap().sequence, 2);
        assert!(registry.subscribers[0].coalesced.get(&key).is_none());

        // Third publish: something WAS already stashed (frame 2), so this
        // replacement counts as exactly one coalesced frame -- not two.
        registry.publish(&key, &sample_frame(3));
        assert_eq!(registry.subscribers[0].pending.get(&key).unwrap().sequence, 3);
        assert_eq!(*registry.subscribers[0].coalesced.get(&key).unwrap(), 1);

        drop(receiver);
    }

    /// `flush_pending` retries a stashed frame once the channel drains, and
    /// the delivered event's `coalesced_since_last` reports exactly how many
    /// frames were replaced away before it.
    #[test]
    fn flush_pending_delivers_a_stashed_frame_and_reports_the_coalesced_count() {
        let mut registry = TerminalSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, mut receiver) = mpsc::channel(1);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);

        registry.publish(&key, &sample_frame(1)); // sent directly, fills the channel
        registry.publish(&key, &sample_frame(2)); // stashed
        registry.publish(&key, &sample_frame(3)); // replaces the stash, coalesced = 1

        // Drain the one frame the forwarder task would have already sent,
        // freeing room for `flush_pending` to land the stashed one.
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorTerminalEventV1::TerminalFrame { sequence: 0, coalesced_since_last: 0, .. },
        ));
        registry.flush_pending();

        let event = receiver.try_recv().unwrap();
        let HarnessOperatorTerminalEventV1::TerminalFrame { sequence, frame, coalesced_since_last, .. } =
            event
        else {
            panic!("expected a terminal frame event");
        };
        assert_eq!(sequence, 1);
        assert_eq!(frame.sequence, 3, "the coalesced-away frame 2 must never be delivered");
        assert_eq!(coalesced_since_last, 1);
        assert!(registry.subscribers[0].pending.is_empty());
    }

    /// `insert` + `send_to` seeds an already-populated session and stays
    /// silent (never a wire error) for a session the ring has no frames for
    /// yet -- mirrors `runtime.rs`'s own `HostCommand::SubscribeTerminal`
    /// arm, which only calls `send_to` when `TerminalBufferRegistry::latest`
    /// actually returned something.
    #[test]
    fn send_to_seeds_a_populated_session_and_is_silent_for_an_empty_one() {
        let mut buffers = TerminalBufferRegistry::default();
        let key = sample_key("node-a", 'a');
        buffers.ingest(key.clone(), sample_frame(1));
        let empty_key = sample_key("node-b", 'b');

        let mut registry = TerminalSubscriberRegistry::default();
        let (sender, mut receiver) = mpsc::channel(HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        sessions.insert(empty_key.clone());
        let id = registry.insert(sender, sessions, subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);

        for seed_key in [&key, &empty_key] {
            if let Some(frame) = buffers.latest(seed_key) {
                registry.send_to(id, seed_key, frame.clone());
            }
        }

        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorTerminalEventV1::TerminalFrame { .. },
        ));
        assert!(receiver.try_recv().is_err(), "the empty session must not have produced a frame");
    }

    /// A closed receiver is pruned outright on the next attempted send --
    /// mirrors `SubscriberRegistry`'s own equivalent coverage
    /// (`subscriber_registry_full_then_lagged_recovery_and_closed_removal`,
    /// `runtime.rs`) for this registry's own, separate `Vec`.
    #[test]
    fn a_closed_receiver_is_pruned_on_the_next_publish() {
        let mut registry = TerminalSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, receiver) = mpsc::channel(HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);
        assert_eq!(registry.subscribers.len(), 1);

        drop(receiver);
        registry.publish(&key, &sample_frame(1));
        assert!(registry.subscribers.is_empty());
    }

    /// Keepalive sibling of `keepalive_tick_reaps_a_subscriber_whose_write_
    /// fails` (`runtime.rs`): an abandoned terminal-push connection sits on
    /// its own, separate `HOST_TERMINAL_SUBSCRIBER_LIMIT` slot until a
    /// keepalive write discovers it closed.
    #[test]
    fn keepalive_reaps_a_subscriber_whose_receiver_is_gone() {
        let mut registry = TerminalSubscriberRegistry::default();
        let (sender, receiver) = mpsc::channel::<HarnessOperatorTerminalEventV1>(
            HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY,
        );
        registry.insert(sender, HashSet::new(), subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);
        drop(receiver);

        registry.keepalive();

        assert!(registry.subscribers.is_empty());
    }

    /// Sibling of the above: a live subscriber survives the tick and
    /// receives exactly one `Ping`.
    #[test]
    fn keepalive_does_not_disturb_a_live_subscriber() {
        let mut registry = TerminalSubscriberRegistry::default();
        let (sender, mut receiver) = mpsc::channel::<HarnessOperatorTerminalEventV1>(
            HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY,
        );
        registry.insert(sender, HashSet::new(), subscribe_terminal_identity(), HARNESS_OPERATOR_WIRE_VERSION_V13);

        registry.keepalive();

        assert_eq!(registry.subscribers.len(), 1);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorTerminalEventV1::Ping { sequence: 0 },
        ));
        assert!(receiver.try_recv().is_err(), "exactly one keep-alive frame per tick");
    }
}
