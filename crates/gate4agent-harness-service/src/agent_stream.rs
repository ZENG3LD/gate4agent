//! Live, RAM-only fan-out of the node's outbound ACP content stream
//! (`NodeEvent::AgentStream`, the `agent-stream-events-v1` capability) to
//! every operator connection subscribed via `SubscribeAgentStream`. Sibling
//! module to `terminal.rs`, deliberately NOT a mirror of its buffering
//! discipline: `terminal.rs` pairs a ring buffer (`TerminalBufferRegistry`,
//! for `TerminalRead` catch-up and the fresh-subscriber seed) with a
//! coalescing subscriber registry, because a terminal frame is always a
//! full, self-contained screen -- a newer one safely supersedes a stale one,
//! and a late joiner can be seeded with just the latest frame. An agent-
//! stream chunk has neither property (see `HarnessOperatorAgentEventV1`'s
//! own doc comment in `gate4agent-harness-api`): a `Text`/`Thinking` delta
//! or an `InteractionPrompt` is a fact about one instant, not a snapshot, so
//! there is no ring buffer here, no seed on subscribe, and the subscriber
//! registry below never coalesces or replays -- it only ever delivers live
//! or honestly reports a loss.
//!
//! Kernel-free by construction, like `terminal.rs`: no `HarnessService`/
//! SQLite/`HarnessC2Adapter` dependency, only the plain `RuntimeSessionKey`
//! and wire types.

use crate::runtime::OperatorRequestLogIdentity;
use gate4agent_harness_api::{
    HarnessAgentStreamChunkKindV1, HarnessAgentStreamChunkV1, HarnessAgentStreamInteractionOptionV1,
    HarnessAgentStreamNamedIdV1, HarnessOperatorAgentEventV1, HarnessProviderConfigChoiceV1,
    HarnessProviderConfigOptionKindV1, HarnessProviderConfigOptionV1, HarnessProviderInteractionKindV1,
    HarnessRuntimeSessionAddressV1,
};
use gate4agent_node_protocol::{
    AgentStreamChunkKindV1, AgentStreamChunkV1, AgentStreamInteractionOptionV1,
    AgentStreamNamedIdV1, ProviderConfigChoice, ProviderConfigOption, ProviderConfigOptionKind,
    ProviderInteractionKind,
};
use gate4agent_observation_api::RuntimeSessionKey;
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;

/// Own dedicated pool, same isolation rationale as `HOST_SUBSCRIBER_LIMIT`/
/// `HOST_TERMINAL_SUBSCRIBER_LIMIT` (`runtime.rs`/`terminal.rs`): an
/// agent-stream-push connection must never compete with either of those two
/// for its slot, or vice versa.
pub const HOST_AGENT_STREAM_SUBSCRIBER_LIMIT: usize = 8;
/// FIFO, not coalesced -- unlike `HOST_TERMINAL_SUBSCRIBER_QUEUE_CAPACITY`
/// (sized for one pending frame per session, safe because a fresher frame
/// can always replace a stale one), this queue holds real, non-replaceable
/// content, so its overflow discipline is `HOST_SUBSCRIBER_QUEUE_CAPACITY`'s
/// (`runtime.rs`) drop-and-report-`Lagged` one, not `terminal.rs`'s
/// coalescing one. Sized the same as that constant for the same reason: a
/// slow reader should have real headroom to catch back up on its own before
/// anything is actually dropped.
pub const HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY: usize = 256;

// Not a `From` impl for the same orphan-rule reason `terminal::
// terminal_frame_to_wire`/`session_key_to_address` aren't ones: both sides
// of every conversion below are foreign to this crate (one half minted by
// `gate4agent-node-protocol`, the other by `gate4agent-harness-api`), and
// this crate is the only one that depends on both.
fn session_key_to_address(key: &RuntimeSessionKey) -> HarnessRuntimeSessionAddressV1 {
    HarnessRuntimeSessionAddressV1 {
        node_id: key.node_id.as_str().to_owned(),
        incarnation_id: key.incarnation_id.to_string(),
        workspace_id: key.workspace_id.as_str().to_owned(),
        instance_id: key.instance_id.0,
        generation: key.generation.0,
    }
}

fn map_provider_interaction_kind(kind: ProviderInteractionKind) -> HarnessProviderInteractionKindV1 {
    match kind {
        ProviderInteractionKind::Approval => HarnessProviderInteractionKindV1::Approval,
        ProviderInteractionKind::Question => HarnessProviderInteractionKindV1::Question,
    }
}

fn map_named_id(entry: AgentStreamNamedIdV1) -> HarnessAgentStreamNamedIdV1 {
    HarnessAgentStreamNamedIdV1 { id: entry.id, name: entry.name, description: entry.description }
}

fn map_interaction_option(
    option: AgentStreamInteractionOptionV1,
) -> HarnessAgentStreamInteractionOptionV1 {
    HarnessAgentStreamInteractionOptionV1 {
        option_id: option.option_id,
        name: option.name,
        kind: option.kind,
    }
}

fn map_provider_config_option_kind(kind: ProviderConfigOptionKind) -> HarnessProviderConfigOptionKindV1 {
    match kind {
        ProviderConfigOptionKind::Select => HarnessProviderConfigOptionKindV1::Select,
        ProviderConfigOptionKind::Boolean => HarnessProviderConfigOptionKindV1::Boolean,
        ProviderConfigOptionKind::Unknown => HarnessProviderConfigOptionKindV1::Unknown,
    }
}

fn map_provider_config_choice(choice: ProviderConfigChoice) -> HarnessProviderConfigChoiceV1 {
    HarnessProviderConfigChoiceV1 { value_json: choice.value_json, label: choice.label }
}

fn map_provider_config_option(option: ProviderConfigOption) -> HarnessProviderConfigOptionV1 {
    HarnessProviderConfigOptionV1 {
        id: option.id,
        name: option.name,
        description: option.description,
        category: option.category,
        kind: map_provider_config_option_kind(option.kind),
        value_json: option.value_json,
        choices: option.choices.into_iter().map(map_provider_config_choice).collect(),
    }
}

fn map_agent_stream_chunk_kind(kind: AgentStreamChunkKindV1) -> HarnessAgentStreamChunkKindV1 {
    match kind {
        AgentStreamChunkKindV1::Text { text, is_delta } => {
            HarnessAgentStreamChunkKindV1::Text { text, is_delta }
        }
        AgentStreamChunkKindV1::Thinking { text } => {
            HarnessAgentStreamChunkKindV1::Thinking { text }
        }
        AgentStreamChunkKindV1::InteractionPrompt {
            correlation_id,
            interaction_kind,
            tool_name,
            title,
            prompt,
            options,
        } => HarnessAgentStreamChunkKindV1::InteractionPrompt {
            correlation_id,
            interaction_kind: map_provider_interaction_kind(interaction_kind),
            tool_name,
            title,
            prompt,
            options: options.into_iter().map(map_interaction_option).collect(),
        },
        AgentStreamChunkKindV1::ModeCatalog { current, available } => {
            HarnessAgentStreamChunkKindV1::ModeCatalog {
                current,
                available: available.into_iter().map(map_named_id).collect(),
            }
        }
        AgentStreamChunkKindV1::ConfigOptions { options } => {
            HarnessAgentStreamChunkKindV1::ConfigOptions {
                options: options.into_iter().map(map_provider_config_option).collect(),
            }
        }
        AgentStreamChunkKindV1::ModelCatalog { current, available } => {
            HarnessAgentStreamChunkKindV1::ModelCatalog {
                current,
                available: available.into_iter().map(map_named_id).collect(),
            }
        }
    }
}

/// `gate4agent-node-protocol`'s `AgentStreamChunkV1` -> the operator wire's
/// own `HarnessAgentStreamChunkV1`, field for field.
fn agent_stream_chunk_to_wire(chunk: AgentStreamChunkV1) -> HarnessAgentStreamChunkV1 {
    HarnessAgentStreamChunkV1 {
        source_sequence: chunk.source_sequence,
        kind: map_agent_stream_chunk_kind(chunk.kind),
    }
}

/// One connection's worth of push-agent-stream subscription state, owned
/// entirely by the select loop. Registered via `HostCommand::
/// SubscribeAgentStream`; pruned the moment its `sender` reports closed.
struct AgentStreamSubscriber {
    id: u64,
    sender: mpsc::Sender<HarnessOperatorAgentEventV1>,
    sessions: HashSet<RuntimeSessionKey>,
    /// Unreported-loss count per session. Unlike `TerminalSubscriber::
    /// pending`, this never holds content to retry -- once a chunk does not
    /// fit, it is gone -- only the tally that `flush_lagged` opportunistically
    /// reports as its own `Lagged` event. A session with a nonzero entry
    /// here is "owed a loss report": `deliver` refuses to send it anything
    /// new (including a fresher chunk) until that report actually goes out,
    /// so the operator is never shown content that arrived after a gap it
    /// was never told about.
    dropped: HashMap<RuntimeSessionKey, u64>,
    /// Per-subscription monotonic; never resets. No `Lagged`/`SnapshotBaseline`
    /// resync pair the way `HarnessEventSubscriber::next_sequence` has one --
    /// same reasoning as `TerminalSubscriber::next_sequence`'s own doc
    /// comment: there is nothing here to resync, only content already gone.
    next_sequence: u64,
    identity: OperatorRequestLogIdentity,
}

impl AgentStreamSubscriber {
    /// Attempts an immediate delivery of `chunk` for `key`. See this
    /// module's own doc comment and `dropped`'s doc comment above for why a
    /// chunk that does not fit is dropped outright rather than stashed, and
    /// why a session already owed a loss report skips straight to counting
    /// this chunk against that same report instead of attempting to send it.
    /// Returns `true` if the subscriber's channel is discovered closed, so
    /// the caller can prune it exactly the way `TerminalSubscriber::deliver`
    /// does for its own registry.
    fn deliver(&mut self, key: &RuntimeSessionKey, chunk: AgentStreamChunkV1) -> bool {
        if let Some(dropped) = self.dropped.get_mut(key) {
            *dropped = dropped.saturating_add(1);
            return false;
        }
        let sequence = self.next_sequence;
        let event = HarnessOperatorAgentEventV1::AgentChunk {
            sequence,
            session: session_key_to_address(key),
            chunk: agent_stream_chunk_to_wire(chunk),
        };
        match self.sender.try_send(event) {
            Ok(()) => {
                self.next_sequence = self.next_sequence.wrapping_add(1);
                false
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.insert(key.clone(), 1);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => true,
        }
    }

    /// Opportunistically reports every session's unreported drop count as
    /// its own `Lagged` event -- called once per select-loop pass
    /// (`AgentStreamSubscriberRegistry::flush_lagged`, mirroring
    /// `TerminalSubscriberRegistry::flush_pending`'s own placement). A still-
    /// full queue leaves the count in place for the next pass; a successful
    /// send clears it, which is what re-admits that session to `deliver`'s
    /// live path above. Returns `true` if the subscriber's channel is
    /// discovered closed, same pruning contract as `deliver`.
    fn flush_lagged(&mut self) -> bool {
        let keys: Vec<RuntimeSessionKey> = self.dropped.keys().cloned().collect();
        for key in keys {
            let Some(&dropped) = self.dropped.get(&key) else { continue; };
            let sequence = self.next_sequence;
            let event = HarnessOperatorAgentEventV1::Lagged {
                sequence,
                session: session_key_to_address(&key),
                dropped,
            };
            match self.sender.try_send(event) {
                Ok(()) => {
                    self.next_sequence = self.next_sequence.wrapping_add(1);
                    self.dropped.remove(&key);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => return true,
            }
        }
        false
    }
}

/// Sibling to `terminal::TerminalSubscriberRegistry`, deliberately NOT
/// reusing its coalescing discipline -- see this module's own doc comment
/// for why a dropped agent-stream chunk cannot be replaced by a fresher one
/// the way a terminal frame can. Loop-owned (no Arc/Mutex, single-writer-
/// core, same as every other subscriber registry in this crate), a plain
/// `Vec` (subscriber count capped tiny by `HOST_AGENT_STREAM_SUBSCRIBER_LIMIT`,
/// so linear scan/removal costs nothing observable).
#[derive(Default)]
pub struct AgentStreamSubscriberRegistry {
    subscribers: Vec<AgentStreamSubscriber>,
    next_id: u64,
}

impl AgentStreamSubscriberRegistry {
    /// No seed on insert, unlike `TerminalSubscriberRegistry::insert`: there
    /// is no buffer to seed from (see this module's own doc comment) -- a
    /// fresh subscriber only ever sees content produced after it registered.
    pub fn insert(
        &mut self,
        sender: mpsc::Sender<HarnessOperatorAgentEventV1>,
        sessions: HashSet<RuntimeSessionKey>,
        identity: OperatorRequestLogIdentity,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.subscribers.push(AgentStreamSubscriber {
            id,
            sender,
            sessions,
            dropped: HashMap::new(),
            next_sequence: 0,
            identity,
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
            "harness agent stream subscriber closed",
        );
    }

    /// Called from the ingest call site for every incoming chunk (cheap
    /// no-op when `subscribers.is_empty()`, matching `TerminalSubscriberRegistry::
    /// publish`'s own early-return convention). Fans `chunk` out to every
    /// subscriber whose `sessions` contains `key`.
    pub fn publish(&mut self, key: &RuntimeSessionKey, chunk: &AgentStreamChunkV1) {
        if self.subscribers.is_empty() { return; }
        let mut index = 0;
        while index < self.subscribers.len() {
            let subscriber = &mut self.subscribers[index];
            if !subscriber.sessions.contains(key) {
                index += 1;
                continue;
            }
            if subscriber.deliver(key, chunk.clone()) {
                self.remove_at(index);
            } else {
                index += 1;
            }
        }
    }

    /// Runs once per select-loop pass (mirrors `TerminalSubscriberRegistry::
    /// flush_pending`'s own placement): retries every subscriber's
    /// unreported `Lagged` counts, so a session whose last overflow happened
    /// while the channel was still full is not left silently short.
    pub fn flush_lagged(&mut self) {
        let mut index = 0;
        while index < self.subscribers.len() {
            if self.subscribers[index].dropped.is_empty() {
                index += 1;
                continue;
            }
            if self.subscribers[index].flush_lagged() {
                self.remove_at(index);
            } else {
                index += 1;
            }
        }
    }

    /// Ping fan-out, mirrors `TerminalSubscriberRegistry::keepalive` exactly
    /// (same `HOST_SUBSCRIBER_KEEPALIVE_INTERVAL` tick drives this too, see
    /// that constant's own doc comment in `runtime.rs`).
    pub fn keepalive(&mut self) {
        let mut index = 0;
        while index < self.subscribers.len() {
            let subscriber = &mut self.subscribers[index];
            let sequence = subscriber.next_sequence;
            match subscriber.sender.try_send(HarnessOperatorAgentEventV1::Ping { sequence }) {
                Ok(()) => {
                    subscriber.next_sequence = subscriber.next_sequence.wrapping_add(1);
                    index += 1;
                }
                Err(mpsc::error::TrySendError::Full(_)) => index += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => self.remove_at(index),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_harness_api::HarnessOperatorRequestV1;
    use gate4agent_observation_api::{AgentInstanceId, NodeId, NodeIncarnationId, SessionGeneration, WorkspaceId};

    fn sample_key(node_id: &str, incarnation: char) -> RuntimeSessionKey {
        RuntimeSessionKey {
            node_id: NodeId::new(node_id).unwrap(),
            incarnation_id: incarnation.to_string().repeat(32).parse::<NodeIncarnationId>().unwrap(),
            workspace_id: WorkspaceId::new("primary").unwrap(),
            instance_id: AgentInstanceId(1),
            generation: SessionGeneration(1),
        }
    }

    fn sample_chunk(sequence: u64) -> AgentStreamChunkV1 {
        AgentStreamChunkV1 {
            source_sequence: sequence,
            kind: AgentStreamChunkKindV1::Text { text: format!("chunk-{sequence}"), is_delta: true },
        }
    }

    fn subscribe_agent_stream_identity() -> OperatorRequestLogIdentity {
        OperatorRequestLogIdentity::describe(
            &HarnessOperatorRequestV1::SubscribeAgentStream { sessions: Vec::new() },
        )
    }

    #[test]
    fn publish_delivers_immediately_when_the_channel_has_room() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, mut receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        registry.publish(&key, &sample_chunk(1));

        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorAgentEventV1::AgentChunk { sequence: 0, .. },
        ));
        assert!(receiver.try_recv().is_err(), "exactly one chunk for one publish");
    }

    #[test]
    fn publish_never_touches_a_subscriber_not_subscribed_to_that_session() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let other_key = sample_key("node-b", 'b');
        let (sender, mut receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(other_key);
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        registry.publish(&key, &sample_chunk(1));

        assert!(receiver.try_recv().is_err());
    }

    /// The overflow discipline itself: a chunk that cannot be sent
    /// immediately is dropped outright (never stashed for later delivery),
    /// counted against that session's unreported loss, and a chunk
    /// published while a loss is still unreported is dropped too rather
    /// than skipping ahead of it.
    #[test]
    fn publish_drops_on_overflow_and_never_reorders_past_an_unreported_loss() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, receiver) = mpsc::channel(1);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        // First publish is sent directly: the capacity-1 channel starts
        // empty.
        registry.publish(&key, &sample_chunk(1));
        assert!(registry.subscribers[0].dropped.is_empty());

        // Second publish: the channel is still full (nothing drained yet),
        // so this chunk is dropped and counted.
        registry.publish(&key, &sample_chunk(2));
        assert_eq!(*registry.subscribers[0].dropped.get(&key).unwrap(), 1);

        // Third publish: the session already owes a loss report, so this
        // one is dropped too (never delivered ahead of the report) and the
        // count accumulates rather than resetting.
        registry.publish(&key, &sample_chunk(3));
        assert_eq!(*registry.subscribers[0].dropped.get(&key).unwrap(), 2);

        drop(receiver);
    }

    /// `flush_lagged` reports the accumulated loss once the channel drains,
    /// scoped to exactly the session that lost it, and clears the count so
    /// the session's live path resumes afterward.
    #[test]
    fn flush_lagged_reports_the_drop_count_and_resumes_live_delivery() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, mut receiver) = mpsc::channel(1);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        registry.publish(&key, &sample_chunk(1)); // sent directly, fills the channel
        registry.publish(&key, &sample_chunk(2)); // dropped, dropped[key] = 1
        registry.publish(&key, &sample_chunk(3)); // dropped, dropped[key] = 2

        // Drain the one chunk the forwarder task would have already sent,
        // freeing room for `flush_lagged` to land the loss report.
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorAgentEventV1::AgentChunk { sequence: 0, .. },
        ));
        registry.flush_lagged();

        let event = receiver.try_recv().unwrap();
        assert!(matches!(
            event,
            HarnessOperatorAgentEventV1::Lagged { sequence: 1, dropped: 2, .. },
        ));
        assert!(registry.subscribers[0].dropped.is_empty());

        // The session's live path is re-admitted now that its loss has been
        // reported.
        registry.publish(&key, &sample_chunk(4));
        let event = receiver.try_recv().unwrap();
        assert!(matches!(event, HarnessOperatorAgentEventV1::AgentChunk { sequence: 2, .. }));
    }

    #[test]
    fn a_closed_receiver_is_pruned_on_the_next_publish() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());
        assert_eq!(registry.subscribers.len(), 1);

        drop(receiver);
        registry.publish(&key, &sample_chunk(1));
        assert!(registry.subscribers.is_empty());
    }

    #[test]
    fn keepalive_reaps_a_subscriber_whose_receiver_is_gone() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let (sender, receiver) = mpsc::channel::<HarnessOperatorAgentEventV1>(
            HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY,
        );
        registry.insert(sender, HashSet::new(), subscribe_agent_stream_identity());
        drop(receiver);

        registry.keepalive();

        assert!(registry.subscribers.is_empty());
    }

    #[test]
    fn keepalive_does_not_disturb_a_live_subscriber() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let (sender, mut receiver) = mpsc::channel::<HarnessOperatorAgentEventV1>(
            HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY,
        );
        registry.insert(sender, HashSet::new(), subscribe_agent_stream_identity());

        registry.keepalive();

        assert_eq!(registry.subscribers.len(), 1);
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorAgentEventV1::Ping { sequence: 0 },
        ));
        assert!(receiver.try_recv().is_err(), "exactly one keep-alive frame per tick");
    }
}
