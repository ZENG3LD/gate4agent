//! RAM-only, bounded ring buffer of raw PTY terminal frames, keyed by
//! [`RuntimeSessionKey`]. Terminal bytes captured here never cross into
//! `gate4agent-observation-api`/`-engine`/`-service` and are never journaled
//! into the SQLite-backed observation store -- `routed_event_to_ingress`
//! (`runtime.rs`) stays the sole SQLite ingress path and is untouched by this
//! module. This registry is a parallel, in-memory-only tap.

use gate4agent_c2_protocol::NodeRoute;
use gate4agent_harness_api::{
    HarnessRuntimeMouseProtocolEncodingV1, HarnessRuntimeTerminalFrameV1,
    HarnessRuntimeTerminalSizeV1,
};
use gate4agent_observation_api::RuntimeSessionKey;
use gate4agent_types::{TerminalFrame, TerminalMouseProtocolEncoding};
use std::collections::{HashMap, HashSet, VecDeque};

const TERMINAL_FRAMES_PER_SESSION_MAX: usize = 64;
const TERMINAL_BYTES_PER_SESSION_MAX: usize = 512 * 1024; // 512 KiB/session
const TERMINAL_SESSIONS_MAX: usize = 64; // worst case ~32 MiB total

#[derive(Default)]
pub(crate) struct TerminalBufferRegistry {
    sessions: HashMap<RuntimeSessionKey, TerminalSessionBuffer>,
    touch_order: VecDeque<RuntimeSessionKey>, // LRU eviction, most-recently-used at the back
}

#[derive(Default)]
struct TerminalSessionBuffer {
    frames: VecDeque<TerminalFrame>,
    dropped: u64,
    bytes: usize,
}

pub(crate) struct TerminalPageResult<'a> {
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
    pub(crate) fn ingest(&mut self, key: RuntimeSessionKey, frame: TerminalFrame) {
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
    pub(crate) fn invalidate(&mut self, route: &NodeRoute) {
        self.sessions.retain(|key, _| {
            !(key.node_id == route.node_id && key.incarnation_id == route.expected_incarnation_id)
        });
        self.prune_touch_order();
    }

    /// Drops every session whose node+incarnation is no longer part of the
    /// current topology. Mirrors `HarnessRuntimeInventoryCache::reconcile_topology`.
    pub(crate) fn reconcile_topology(&mut self, routes: &[NodeRoute]) {
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

    pub(crate) fn page(
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
pub(crate) fn terminal_frame_to_wire(frame: &TerminalFrame) -> HarnessRuntimeTerminalFrameV1 {
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
    }
}
