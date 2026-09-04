//! Live, RAM-only fan-out of the node's outbound ACP content stream
//! (`NodeEvent::AgentStream`, the `agent-stream-events-v1` capability) to
//! every operator connection subscribed via `SubscribeAgentStream`. Sibling
//! module to `terminal.rs`, but NOT a mirror of its ring-buffer discipline:
//! `terminal.rs` pairs a ring buffer (`TerminalBufferRegistry`, for
//! `TerminalRead` catch-up and the fresh-subscriber seed) with a coalescing
//! subscriber registry, because every terminal frame is a full,
//! self-contained screen -- nothing here is that uniform, so there is no
//! ring buffer and the subscriber registry below never coalesces: a chunk
//! that does not fit is dropped outright and the loss is reported
//! (`AgentStreamSubscriber::dropped`/`flush_lagged`), never stashed for
//! retry.
//!
//! Seeding, though, is not one rule for all six chunk kinds (see
//! `HarnessOperatorAgentEventV1`'s own doc comment in `gate4agent-harness-api`
//! for the full inventory) -- three different disciplines, not two:
//!
//! - **`Text`, `Thinking`, `Blocked`: instants, never seeded.** A fact about
//!   one moment; a subscriber that was not listening when it happened has no
//!   use for it after the fact, and replaying it stale would mislead.
//! - **`ModeCatalog`, `ConfigOptions`, `ModelCatalog`: current state, always
//!   seeded, latest replaces prior.** The latest one IS the truth about the
//!   session right now, so `AgentStreamSubscriberRegistry` holds exactly one
//!   per kind per session (`AgentStreamSessionState`, recorded by every
//!   `publish` whether or not a subscriber exists yet to see it live) and
//!   hands it to every subscriber that registers for that session, before
//!   any live chunk -- otherwise the one-time announcement at session start
//!   is gone, unobserved, before any subscriber can even name the session
//!   address to ask for it.
//! - **`InteractionPrompt`: state while unresolved, seeded until resolved.**
//!   An open question is exactly as much "current state" as a mode
//!   catalogue for as long as nothing has answered it -- an operator who
//!   connects while the agent is waiting must see that it is waiting, or an
//!   agent can sit stuck behind a gate with no subscriber ever finding out.
//!   The moment it resolves (a `ResolveInteraction` answered it, or
//!   `HostPolicy` decided it on a deadline -- either way something settled
//!   it), it leaves the seed set (`AgentStreamSubscriberRegistry::
//!   resolve_interaction`): the timeline already carries the resolved
//!   history, and seeding a closed question to a fresh operator would
//!   invite an answer to something already decided.
//!
//! Kernel-free by construction, like `terminal.rs`: no `HarnessService`/
//! SQLite/`HarnessC2Adapter` dependency, only the plain `RuntimeSessionKey`
//! and wire types.

use crate::runtime::OperatorRequestLogIdentity;
use gate4agent_harness_api::{
    HarnessAgentStreamChunkKindV1, HarnessAgentStreamChunkV1, HarnessAgentStreamInteractionOptionV1,
    HarnessAgentStreamNamedIdV1, HarnessBlockAuthorityV1, HarnessOperatorAgentEventV1,
    HarnessProviderConfigChoiceV1, HarnessProviderConfigOptionKindV1, HarnessProviderConfigOptionV1,
    HarnessProviderInteractionKindV1, HarnessRuntimeSessionAddressV1,
};
use gate4agent_node_protocol::{
    AgentStreamChunkKindV1, AgentStreamChunkV1, AgentStreamInteractionOptionV1,
    AgentStreamNamedIdV1, BlockAuthorityV1, ProviderConfigChoice, ProviderConfigOption,
    ProviderConfigOptionKind, ProviderInteractionKind,
};
use gate4agent_observation_api::RuntimeSessionKey;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
/// Cap on how many sessions' worth of state (`AgentStreamSessionState`) the
/// registry below holds at once -- mirrors `TerminalBufferRegistry`'s own
/// `TERMINAL_SESSIONS_MAX` (`terminal.rs`) for the identical reason: state
/// is recorded for every session `AgentStreamSubscriberRegistry::publish`
/// ever sees, whether or not a subscriber exists yet to receive it live, so
/// nothing else bounds how many sessions accumulate there except this cap
/// plus each session's own tiny, fixed-by-construction footprint (three
/// `Option<AgentStreamChunkV1>` slots and a handful of pending interaction
/// prompts -- see `AgentStreamSessionState`'s own doc comment).
const AGENT_STREAM_STATE_SESSIONS_MAX: usize = 64;

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

fn map_block_authority(authority: BlockAuthorityV1) -> HarnessBlockAuthorityV1 {
    match authority {
        BlockAuthorityV1::HarnessGate => HarnessBlockAuthorityV1::HarnessGate,
        BlockAuthorityV1::HarnessPolicy => HarnessBlockAuthorityV1::HarnessPolicy,
        BlockAuthorityV1::HarnessDeadline => HarnessBlockAuthorityV1::HarnessDeadline,
        BlockAuthorityV1::Operator => HarnessBlockAuthorityV1::Operator,
        BlockAuthorityV1::ProviderClassifier => HarnessBlockAuthorityV1::ProviderClassifier,
        BlockAuthorityV1::ProviderPermissionRule => HarnessBlockAuthorityV1::ProviderPermissionRule,
        BlockAuthorityV1::ProviderSandbox => HarnessBlockAuthorityV1::ProviderSandbox,
        BlockAuthorityV1::ProviderRefusal => HarnessBlockAuthorityV1::ProviderRefusal,
        BlockAuthorityV1::ProviderHook => HarnessBlockAuthorityV1::ProviderHook,
        BlockAuthorityV1::UserRejected => HarnessBlockAuthorityV1::UserRejected,
        BlockAuthorityV1::Unknown => HarnessBlockAuthorityV1::Unknown,
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
        AgentStreamChunkKindV1::Blocked {
            correlation_id,
            tool_class,
            authority,
            reason_kind,
            reason,
            help,
        } => HarnessAgentStreamChunkKindV1::Blocked {
            correlation_id,
            tool_class,
            authority: map_block_authority(authority),
            reason_kind,
            reason,
            help,
        },
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

/// The current-state chunks held for one session -- see this module's own
/// doc comment for why these, and only these, earn a seed. Three fixed
/// slots (`ModeCatalog`/`ConfigOptions`/`ModelCatalog`), each replaced --
/// never accumulated -- by the newest chunk of its kind, plus a map of
/// still-unresolved `InteractionPrompt`s.
#[derive(Default)]
struct AgentStreamSessionState {
    mode_catalog: Option<AgentStreamChunkV1>,
    config_options: Option<AgentStreamChunkV1>,
    model_catalog: Option<AgentStreamChunkV1>,
    /// Keyed by `InteractionPrompt::correlation_id` -- the same id both a
    /// `ResolveInteraction` names and `AgentStreamSubscriberRegistry::
    /// resolve_interaction` is called with, so more than one interaction
    /// open on the same session at once is represented correctly rather
    /// than one clobbering another's slot. `BTreeMap` only for a
    /// deterministic seed order across runs, not because the protocol
    /// itself orders interactions by correlation id.
    interaction_prompts: BTreeMap<String, AgentStreamChunkV1>,
}

impl AgentStreamSessionState {
    /// Every chunk this session currently owes a fresh subscriber, in a
    /// fixed order (mode catalogue, config options, model catalogue, then
    /// every still-unresolved interaction prompt in correlation-id order)
    /// -- the order `AgentStreamSubscriberRegistry::seed` delivers in.
    fn seed_chunks(&self) -> impl Iterator<Item = &AgentStreamChunkV1> {
        [self.mode_catalog.as_ref(), self.config_options.as_ref(), self.model_catalog.as_ref()]
            .into_iter()
            .flatten()
            .chain(self.interaction_prompts.values())
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
    /// fit, it is gone -- only the tally that `flush_lagged`/`deliver`
    /// opportunistically report as a `Lagged` event. A session with a
    /// nonzero entry here is "owed a loss report": `deliver` always tries to
    /// flush that report FIRST (never skipping the attempt, never leaving it
    /// to languish until the next standalone `flush_lagged` pass), and only
    /// once it lands does it go on to attempt the incoming chunk itself --
    /// so the operator is never shown content that arrived after a gap it
    /// was never told about, but a channel that has drained is never left
    /// muted either.
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
    /// chunk that does not fit is dropped outright rather than stashed. A
    /// session already owed a loss report gets that report flushed first
    /// (`try_send_lagged_report`, attempted fresh on every call -- NEVER a
    /// one-way gate that stops trying): if it lands, this chunk is then
    /// attempted right behind it in the same call; if the channel is still
    /// full, this chunk is counted onto the same still-unreported loss
    /// (no second warning -- the first drop of the episode already named
    /// it). Returns `true` if the subscriber's channel is discovered closed,
    /// so the caller can prune it exactly the way `TerminalSubscriber::
    /// deliver` does for its own registry.
    fn deliver(&mut self, key: &RuntimeSessionKey, chunk: AgentStreamChunkV1) -> bool {
        if let Some(&dropped) = self.dropped.get(key) {
            match self.try_send_lagged_report(key, dropped) {
                Ok(true) => {
                    self.dropped.remove(key);
                }
                Ok(false) => {
                    if let Some(count) = self.dropped.get_mut(key) {
                        *count = count.saturating_add(1);
                    }
                    return false;
                }
                Err(()) => return true,
            }
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
                tracing::warn!(
                    subscriber_id = self.id,
                    node_id = key.node_id.as_str(),
                    workspace_id = key.workspace_id.as_str(),
                    "harness agent stream subscriber queue full, dropping a chunk",
                );
                self.dropped.insert(key.clone(), 1);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => true,
        }
    }

    /// Delivers one held STATE chunk (`ModeCatalog`/`ConfigOptions`/
    /// `ModelCatalog`/an unresolved `InteractionPrompt` -- see this module's
    /// own doc comment) to a subscriber still being registered, before it
    /// can observe anything live. Deliberately NOT `deliver` above, on
    /// purpose, for two reasons that mirror the coordinator brief's own
    /// wording:
    ///
    /// - it must not slip past the `Lagged` discipline: a subscriber only
    ///   ever reaches this method from `AgentStreamSubscriberRegistry::seed`,
    ///   called from `insert` before the subscriber can have observed
    ///   anything, so `dropped` is always empty here -- there is no
    ///   unreported loss this could run ahead of. The `debug_assert!` below
    ///   makes that invariant a build-time-checked fact rather than an
    ///   assumption.
    /// - a seed that does not fit must not itself count as a loss: unlike a
    ///   `Text`/`Thinking` instant, the content here is current truth, not a
    ///   one-off fact, so if this `try_send` finds the channel full the very
    ///   next live update for this kind supersedes it regardless -- nothing
    ///   was actually lost the way a dropped live chunk is. A `Full` outcome
    ///   is therefore dropped silently rather than added to `dropped`,
    ///   exactly the way `AgentStreamSubscriberRegistry::keepalive` treats a
    ///   busy-but-alive channel.
    ///
    /// Returns `true` only when the channel is discovered closed, so the
    /// caller can prune it the same way every other send path here does.
    fn deliver_seed(&mut self, key: &RuntimeSessionKey, chunk: AgentStreamChunkV1) -> bool {
        debug_assert!(
            !self.dropped.contains_key(key),
            "a subscriber being seeded has not yet observed anything to lose",
        );
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
            Err(mpsc::error::TrySendError::Full(_)) => false,
            Err(mpsc::error::TrySendError::Closed(_)) => true,
        }
    }

    /// Attempts to flush a still-unreported `Lagged { dropped }` notice for
    /// `key` -- the one send path shared by `deliver` above (opportunistic,
    /// tried again on every incoming chunk for a session that already owes
    /// one) and `flush_lagged` below (the per-select-loop-pass backstop for
    /// a session with no further incoming chunks to trigger the
    /// opportunistic path, so a report is not left stranded just because
    /// nothing else arrived to ask for it). `Ok(true)` means the notice went
    /// out (the caller clears `dropped[key]`, and logs once that the loss
    /// was reported); `Ok(false)` means the channel is still full (the
    /// caller counts this as one more unreported loss, no new warning -- the
    /// first drop of the episode already named it); `Err(())` means the
    /// channel is discovered closed, same pruning contract as `deliver`.
    fn try_send_lagged_report(&mut self, key: &RuntimeSessionKey, dropped: u64) -> Result<bool, ()> {
        let sequence = self.next_sequence;
        let event = HarnessOperatorAgentEventV1::Lagged {
            sequence,
            session: session_key_to_address(key),
            dropped,
        };
        match self.sender.try_send(event) {
            Ok(()) => {
                self.next_sequence = self.next_sequence.wrapping_add(1);
                tracing::warn!(
                    subscriber_id = self.id,
                    node_id = key.node_id.as_str(),
                    workspace_id = key.workspace_id.as_str(),
                    dropped,
                    "harness agent stream subscriber flushed a lag report",
                );
                Ok(true)
            }
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(()),
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
            match self.try_send_lagged_report(&key, dropped) {
                Ok(true) => {
                    self.dropped.remove(&key);
                }
                Ok(false) => {}
                Err(()) => return true,
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
///
/// `state` is the seed source `insert` reads from -- see this module's own
/// doc comment for the three-way seeding split. It is a second, much
/// smaller piece of held data than `subscribers`, bounded by
/// `AGENT_STREAM_STATE_SESSIONS_MAX` rather than by subscriber count, since
/// it is recorded per session regardless of whether any subscriber exists.
#[derive(Default)]
pub struct AgentStreamSubscriberRegistry {
    subscribers: Vec<AgentStreamSubscriber>,
    next_id: u64,
    state: HashMap<RuntimeSessionKey, AgentStreamSessionState>,
    /// LRU order over `state`'s keys, oldest-touched at the front -- mirrors
    /// `TerminalBufferRegistry::touch_order` (`terminal.rs`) exactly, same
    /// eviction contract, over this registry's own separate map.
    state_touch_order: VecDeque<RuntimeSessionKey>,
}

impl AgentStreamSubscriberRegistry {
    /// Seeds the new subscriber with every STATE chunk currently held
    /// (`seed`, reading `state`) for the sessions it asked for -- see this
    /// module's own doc comment for the three-way seeding split this
    /// replaces the old "nothing to seed from" behaviour with.
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
            sessions: sessions.clone(),
            dropped: HashMap::new(),
            next_sequence: 0,
            identity,
        });
        self.seed(id, &sessions);
        id
    }

    /// Sends every STATE chunk this registry currently holds for each of
    /// `sessions` to subscriber `id`, before it can observe anything live --
    /// the fan-out half of `insert`'s seeding. Split out from `insert` only
    /// to keep the borrow of `self.state` (read) and `self.subscribers`
    /// (written via `deliver_seed`) from overlapping.
    fn seed(&mut self, id: u64, sessions: &HashSet<RuntimeSessionKey>) {
        let Some(index) = self.subscribers.iter().position(|subscriber| subscriber.id == id)
        else {
            return;
        };
        let mut seed_chunks: Vec<(RuntimeSessionKey, AgentStreamChunkV1)> = Vec::new();
        for key in sessions {
            let Some(state) = self.state.get(key) else { continue; };
            for chunk in state.seed_chunks() {
                seed_chunks.push((key.clone(), chunk.clone()));
            }
        }
        for (key, chunk) in seed_chunks {
            if self.subscribers[index].deliver_seed(&key, chunk) {
                self.remove_at(index);
                return;
            }
        }
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

    /// The session's state slot, touching it in `state_touch_order` and
    /// evicting the least-recently-touched session first if this would be a
    /// new one past `AGENT_STREAM_STATE_SESSIONS_MAX` -- mirrors
    /// `TerminalBufferRegistry::ingest`'s own LRU discipline (`terminal.rs`).
    fn state_slot(&mut self, key: &RuntimeSessionKey) -> &mut AgentStreamSessionState {
        let is_new_session = !self.state.contains_key(key);
        if is_new_session && self.state.len() >= AGENT_STREAM_STATE_SESSIONS_MAX {
            if let Some(evicted) = self.state_touch_order.pop_front() {
                self.state.remove(&evicted);
            }
        }
        if let Some(position) = self.state_touch_order.iter().position(|touched| touched == key) {
            self.state_touch_order.remove(position);
        }
        self.state_touch_order.push_back(key.clone());
        self.state.entry(key.clone()).or_default()
    }

    /// Records `chunk` into `state` if it is one of the seeded kinds (see
    /// this module's own doc comment); every other kind (`Text`, `Thinking`)
    /// is an instant and is never held. Called unconditionally from
    /// `publish` below, even with zero subscribers -- this is the fix for
    /// the fan-out this module's own doc comment describes: without it, a
    /// state chunk published before any subscriber exists would never be
    /// recorded anywhere for `insert` to seed a later one from.
    fn record_state(&mut self, key: &RuntimeSessionKey, chunk: &AgentStreamChunkV1) {
        match &chunk.kind {
            AgentStreamChunkKindV1::ModeCatalog { .. } => {
                self.state_slot(key).mode_catalog = Some(chunk.clone());
            }
            AgentStreamChunkKindV1::ConfigOptions { .. } => {
                self.state_slot(key).config_options = Some(chunk.clone());
            }
            AgentStreamChunkKindV1::ModelCatalog { .. } => {
                self.state_slot(key).model_catalog = Some(chunk.clone());
            }
            AgentStreamChunkKindV1::InteractionPrompt { correlation_id, .. } => {
                self.state_slot(key).interaction_prompts.insert(correlation_id.clone(), chunk.clone());
            }
            AgentStreamChunkKindV1::Text { .. }
            | AgentStreamChunkKindV1::Thinking { .. }
            | AgentStreamChunkKindV1::Blocked { .. } => {}
        }
    }

    /// Drops the pending `InteractionPrompt` recorded under `correlation_id`
    /// for `key`, if any -- called the moment the harness observes that
    /// interaction settle (`ApprovalResolved`/`QuestionResolved`/
    /// `InteractionResolved`; see `runtime.rs`'s own `C2NodeEvent::
    /// Observation` handling), regardless of whether an operator's own
    /// `ResolveInteraction` caused it or `HostPolicy` decided it on a
    /// deadline -- both mean the same thing here: the question is no longer
    /// open, so it leaves the seed set. A no-op (not an error) if `key` or
    /// `correlation_id` is not currently held, matching every other
    /// best-effort lookup in this module.
    pub fn resolve_interaction(&mut self, key: &RuntimeSessionKey, correlation_id: &str) {
        if let Some(state) = self.state.get_mut(key) {
            state.interaction_prompts.remove(correlation_id);
        }
    }

    /// Called from the ingest call site for every incoming chunk. Unlike
    /// the cheap no-op-when-empty convention this used to share verbatim
    /// with `TerminalSubscriberRegistry::publish`, `record_state` now runs
    /// unconditionally -- even with zero subscribers -- because that is
    /// exactly the situation `insert`'s seed exists to fix: a state chunk
    /// observed before anyone was listening must still be there for the
    /// next subscriber to be seeded with. The fan-out to `subscribers`
    /// immediately below remains the cheap no-op it always was.
    pub fn publish(&mut self, key: &RuntimeSessionKey, chunk: &AgentStreamChunkV1) {
        self.record_state(key, chunk);
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

    fn sample_mode_catalog_chunk(sequence: u64, current: &str) -> AgentStreamChunkV1 {
        AgentStreamChunkV1 {
            source_sequence: sequence,
            kind: AgentStreamChunkKindV1::ModeCatalog {
                current: Some(current.to_owned()),
                available: Vec::new(),
            },
        }
    }

    fn sample_interaction_prompt_chunk(sequence: u64, correlation_id: &str) -> AgentStreamChunkV1 {
        AgentStreamChunkV1 {
            source_sequence: sequence,
            kind: AgentStreamChunkKindV1::InteractionPrompt {
                correlation_id: correlation_id.to_owned(),
                interaction_kind: ProviderInteractionKind::Approval,
                tool_name: "tool".to_owned(),
                title: None,
                prompt: "proceed?".to_owned(),
                options: Vec::new(),
            },
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

    /// Item 1's direct proof: `deliver` itself -- NOT a separate
    /// `flush_lagged` call -- opportunistically flushes a pending `Lagged`
    /// report the moment the channel has room, then goes straight on to
    /// attempt the very chunk that triggered it, all from one `publish`.
    /// This is the fix for the old permanent-mute-on-first-overflow design:
    /// a session that owes a loss report is never left stuck waiting on a
    /// separate pass to notice the channel drained.
    #[test]
    fn deliver_recovers_a_pending_lag_report_and_the_next_chunk_in_one_call() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        let (sender, mut receiver) = mpsc::channel(2);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        registry.publish(&key, &sample_chunk(1)); // sent directly, 1/2 slots used
        registry.publish(&key, &sample_chunk(2)); // sent directly, 2/2 slots used (now full)
        registry.publish(&key, &sample_chunk(3)); // full -- dropped, dropped[key] = 1
        assert_eq!(*registry.subscribers[0].dropped.get(&key).unwrap(), 1);

        // Drain both already-sent chunks the forwarder task would have
        // taken, freeing the whole channel back up. No call to
        // `flush_lagged` anywhere in this test.
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorAgentEventV1::AgentChunk { sequence: 0, .. },
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            HarnessOperatorAgentEventV1::AgentChunk { sequence: 1, .. },
        ));

        // The very next publish for this session recovers on its own:
        // `deliver` flushes the pending `Lagged { dropped: 1 }` report
        // first, then this chunk right behind it, in the same call.
        registry.publish(&key, &sample_chunk(4));
        assert!(registry.subscribers[0].dropped.is_empty());

        let event = receiver.try_recv().unwrap();
        assert!(matches!(
            event,
            HarnessOperatorAgentEventV1::Lagged { sequence: 2, dropped: 1, .. },
        ));
        let event = receiver.try_recv().unwrap();
        assert!(matches!(event, HarnessOperatorAgentEventV1::AgentChunk { sequence: 3, .. }));
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

    /// Item 1's direct proof: a `ModeCatalog` published before anyone was
    /// subscribed is not gone -- a subscriber that registers afterward is
    /// seeded with it before anything live.
    #[test]
    fn insert_seeds_a_subscriber_with_a_state_chunk_published_before_registration() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        registry.publish(&key, &sample_mode_catalog_chunk(1, "default"));

        let (sender, mut receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        let event = receiver.try_recv().unwrap();
        let HarnessOperatorAgentEventV1::AgentChunk { sequence, chunk, .. } = event else {
            panic!("expected a seeded agent chunk");
        };
        assert_eq!(sequence, 0);
        assert!(matches!(
            chunk.kind,
            HarnessAgentStreamChunkKindV1::ModeCatalog { current: Some(current), .. }
                if current == "default"
        ));
        assert!(receiver.try_recv().is_err(), "exactly one seeded chunk, nothing live yet");
    }

    /// Item 2's direct proof: `Text` (and, by the same code path, `Thinking`)
    /// is an instant, never held in `state`, so a subscriber that registers
    /// after one was published sees nothing from it.
    #[test]
    fn insert_does_not_seed_an_instant_chunk_published_before_registration() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        registry.publish(&key, &sample_chunk(1));

        let (sender, mut receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        assert!(receiver.try_recv().is_err(), "a Text instant must never be seeded");
    }

    /// A second `ModeCatalog` replaces the first in `state` rather than
    /// accumulating alongside it -- a subscriber registering afterward sees
    /// exactly the latest one, never both and never the stale one.
    #[test]
    fn a_second_state_chunk_replaces_the_first_rather_than_accumulating() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        registry.publish(&key, &sample_mode_catalog_chunk(1, "fast"));
        registry.publish(&key, &sample_mode_catalog_chunk(2, "careful"));

        let (sender, mut receiver) = mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions = HashSet::new();
        sessions.insert(key.clone());
        registry.insert(sender, sessions, subscribe_agent_stream_identity());

        let event = receiver.try_recv().unwrap();
        let HarnessOperatorAgentEventV1::AgentChunk { chunk, .. } = event else {
            panic!("expected a seeded agent chunk");
        };
        assert!(matches!(
            chunk.kind,
            HarnessAgentStreamChunkKindV1::ModeCatalog { current: Some(current), .. }
                if current == "careful"
        ));
        assert!(receiver.try_recv().is_err(), "exactly one seeded ModeCatalog, the latest only");
    }

    /// The conditional half of state seeding: an `InteractionPrompt` is held
    /// and seeded while unresolved, and `resolve_interaction` (the sink for
    /// `runtime.rs`'s own `ApprovalResolved`/`QuestionResolved`/
    /// `InteractionResolved` observation handling) removes it from the seed
    /// set the moment it settles -- a subscriber registering afterward must
    /// not be asked to answer an already-closed question.
    #[test]
    fn insert_seeds_an_unresolved_interaction_prompt_and_not_a_resolved_one() {
        let mut registry = AgentStreamSubscriberRegistry::default();
        let key = sample_key("node-a", 'a');
        registry.publish(&key, &sample_interaction_prompt_chunk(1, "corr-1"));

        // A subscriber that registers while the prompt is still open is
        // seeded with it.
        let (sender_open, mut receiver_open) =
            mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions_open = HashSet::new();
        sessions_open.insert(key.clone());
        registry.insert(sender_open, sessions_open, subscribe_agent_stream_identity());

        let event = receiver_open.try_recv().unwrap();
        let HarnessOperatorAgentEventV1::AgentChunk { chunk, .. } = event else {
            panic!("expected a seeded agent chunk");
        };
        assert!(matches!(chunk.kind, HarnessAgentStreamChunkKindV1::InteractionPrompt { .. }));

        registry.resolve_interaction(&key, "corr-1");

        // A subscriber that registers after resolution must not see it.
        let (sender_resolved, mut receiver_resolved) =
            mpsc::channel(HOST_AGENT_STREAM_SUBSCRIBER_QUEUE_CAPACITY);
        let mut sessions_resolved = HashSet::new();
        sessions_resolved.insert(key.clone());
        registry.insert(sender_resolved, sessions_resolved, subscribe_agent_stream_identity());

        assert!(
            receiver_resolved.try_recv().is_err(),
            "a resolved interaction prompt must not be seeded",
        );
    }
}
