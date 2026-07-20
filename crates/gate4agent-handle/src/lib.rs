//! Bounded in-process ports for the Gate4Agent backend control plane.

mod provider_runtime;

pub use provider_runtime::{
    ProviderCancellation, ProviderCancellationToken, ProviderCompletionHandle,
    ProviderEffectPublishReport, ProviderInvocation, ProviderObservationOutcome,
    ProviderObservationStatus, ProviderRuntimeAuthorityHandle, ProviderRuntimeError,
    ProviderRuntimeHandle, ProviderRuntimeState, ProviderWork, MAX_PROVIDER_EFFECT_CAPACITY,
    MAX_PROVIDER_RUNTIMES,
};

use gate4agent_kernel::{
    BackendIngress, BackendIngressOutcome, BackendSnapshot, KernelStep,
    ToolAuthorityCommandOutcome, ToolRequestOutcome,
};
use gate4agent_tool_protocol::{
    CapabilityCompletionBatch, CapabilityCompletionEnvelope, CapabilityOwner,
    CapabilityProviderDescriptor, CapabilityRequestInput, CapabilityRequestSnapshot,
    ConsumerBoundCapabilityRequest, ConsumerId, PolicyGrant, ProviderBindingId,
    ProviderBoundCapabilityRequest, ToolActorId, ToolAuditEvent, ToolAuthorityCommand,
    ToolAuthorityEnvelope, ToolAuthorityOutcome, ToolInstanceState, ToolProviderId,
    CAPABILITY_PROTOCOL_VERSION,
};
use gate4agent_types::{
    AgentInstanceId, CommandEnvelope, ControlEvent, ControlSnapshot, SessionGeneration,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, RwLock, Weak};
use thiserror::Error;

pub const MAX_CONTROL_SUBSCRIBERS: usize = 128;
pub const MAX_TOOL_CLIENTS: usize = 128;
pub const MAX_TOOL_SUBSCRIBERS_PER_CLIENT: usize = 32;
pub const MAX_AUTHORITY_SUBSCRIBERS: usize = 32;
pub const MAX_INGRESS_CAPACITY: usize = 4_096;
pub const MAX_SUBSCRIPTION_CAPACITY: usize = 1_024;

pub trait AgentPort: Send + Sync {
    fn dispatch(&self, command: CommandEnvelope) -> Result<(), PortDispatchError>;
    fn snapshot(&self) -> Arc<ControlSnapshot>;
    fn subscribe(&self, capacity: usize) -> EventSubscription;
}

#[derive(Clone)]
pub struct Gate4AgentHandle {
    command_tx: GateIngressSender,
    edge: Arc<EdgeState>,
}

pub struct KernelPort {
    command_rx: Receiver<CommandEnvelope>,
    edge: Arc<EdgeState>,
}

pub struct ControlPlaneKernelPort {
    ingress_rx: Receiver<BackendIngress>,
    provider_runtime: provider_runtime::ProviderRuntimePort,
    edge: Arc<EdgeState>,
    authority: Arc<AuthorityState>,
}

#[derive(Clone)]
pub struct ToolAuthorityHandle {
    edge: Arc<EdgeState>,
    authority: Arc<AuthorityState>,
}

#[derive(Clone)]
pub struct ToolClientHandle {
    state: Arc<ClientState>,
}

pub struct EventSubscription {
    receiver: Receiver<ControlEvent>,
}

pub struct ToolRequestOutcomeSubscription {
    receiver: Receiver<ToolRequestOutcome>,
}

pub struct ToolCompletionSubscription {
    receiver: Receiver<ToolCompletionDelivery>,
}

pub struct ToolAuthorityOutcomeSubscription {
    receiver: Receiver<ToolAuthorityCommandOutcome>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolCompletionDelivery {
    SourceGap(ToolCompletionSourceGap),
    Completion(CapabilityCompletionEnvelope),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ToolCompletionSourceGap {
    pub dropped_since_last_drain: u64,
    pub total_dropped: u64,
    pub next_sequence: u64,
    pub sequence_exhausted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolClientSnapshot {
    pub protocol_version: u16,
    pub backend_revision: u64,
    pub logical_tick: u64,
    pub tool_revision: u64,
    pub current_tick: u64,
    pub generations: Vec<(AgentInstanceId, SessionGeneration)>,
    pub instance_states: Vec<(AgentInstanceId, ToolInstanceState)>,
    pub providers: Vec<CapabilityProviderDescriptor>,
    pub available_providers: Vec<ToolProviderId>,
    pub grants: Vec<PolicyGrant>,
    pub requests: Vec<CapabilityRequestSnapshot>,
    pub audit_events: Vec<ToolAuditEvent>,
    pub dropped_audit_events: u64,
    pub next_completion_sequence: u64,
    pub dropped_completions: u64,
    pub completion_sequence_exhausted: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PublishReport {
    pub delivered: usize,
    pub disconnected_slow: usize,
    pub disconnected_closed: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ControlPlanePublishReport {
    pub control_events: PublishReport,
    pub request_outcomes: PublishReport,
    pub authority_outcomes: PublishReport,
    pub completions: PublishReport,
    pub provider_effects: ProviderEffectPublishReport,
    pub closed_clients: usize,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PortDispatchError {
    #[error("gate4agent command ingress is full")]
    Full,
    #[error("gate4agent kernel ingress is disconnected")]
    Disconnected,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ToolClientDispatchError {
    #[error("tool client is not active")]
    Inactive,
    #[error("tool provider '{provider_id}' has no active local runtime binding")]
    ProviderUnavailable { provider_id: ToolProviderId },
    #[error("gate4agent backend ingress is full")]
    Full,
    #[error("gate4agent backend ingress is disconnected")]
    Disconnected,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ToolAuthorityError {
    #[error("tool client registry is full")]
    ClientCapacityExceeded,
    #[error("tool authority counter '{counter}' is exhausted")]
    CounterExhausted { counter: &'static str },
    #[error("tool client belongs to another authority")]
    ForeignClient,
    #[error("tool client is already closed")]
    ClientClosed,
    #[error("CloseClient must be dispatched through close_client")]
    CloseRequiresMethod,
    #[error("gate4agent backend ingress is full")]
    Full,
    #[error("gate4agent backend ingress is disconnected")]
    Disconnected,
}

#[derive(Clone)]
enum GateIngressSender {
    Legacy(SyncSender<CommandEnvelope>),
    Backend(SyncSender<BackendIngress>),
}

struct EdgeState {
    publication: RwLock<EdgePublication>,
    control_subscribers: Mutex<Vec<SyncSender<ControlEvent>>>,
    completion_health: Mutex<CompletionPublishHealth>,
}

#[derive(Clone)]
struct EdgePublication {
    snapshot: Arc<BackendSnapshot>,
    available_provider_bindings: BTreeMap<ToolProviderId, ProviderBindingId>,
}

#[derive(Clone, Copy, Default)]
struct CompletionPublishHealth {
    sequence_exhaustion_announced: bool,
}

struct AuthorityState {
    ingress_tx: SyncSender<BackendIngress>,
    inner: Mutex<AuthorityInner>,
}

struct AuthorityInner {
    next_client_id: u64,
    next_authority_sequence: u64,
    clients: BTreeMap<ConsumerId, Arc<ClientState>>,
    closing_by_sequence: BTreeMap<u64, ConsumerId>,
    subscribers: Vec<SyncSender<ToolAuthorityCommandOutcome>>,
}

struct ClientState {
    consumer_id: ConsumerId,
    actor_id: ToolActorId,
    authority: Weak<AuthorityState>,
    edge: Arc<EdgeState>,
    inner: Mutex<ClientInner>,
}

struct ClientInner {
    lifecycle: ClientLifecycle,
    request_subscribers: Vec<SyncSender<ToolRequestOutcome>>,
    completion_subscribers: Vec<SyncSender<ToolCompletionDelivery>>,
}

#[derive(Clone)]
enum ClientLifecycle {
    Active,
    ClosingSent { sequence: u64 },
    ClosingRetry,
    CloseSucceeded,
    Closed,
}

pub fn bounded_port(command_capacity: usize) -> (Gate4AgentHandle, KernelPort) {
    let (command_tx, command_rx) = sync_channel(bounded_ingress_capacity(command_capacity));
    let edge = Arc::new(EdgeState::new());
    (
        Gate4AgentHandle {
            command_tx: GateIngressSender::Legacy(command_tx),
            edge: Arc::clone(&edge),
        },
        KernelPort { command_rx, edge },
    )
}

pub fn bounded_control_plane(
    ingress_capacity: usize,
) -> (
    Gate4AgentHandle,
    ToolAuthorityHandle,
    ControlPlaneKernelPort,
) {
    let (ingress_tx, ingress_rx) = sync_channel(bounded_ingress_capacity(ingress_capacity));
    let (_provider_authority, provider_runtime) =
        provider_runtime::provider_runtime(ingress_tx.clone());
    let edge = Arc::new(EdgeState::new());
    let authority = Arc::new(AuthorityState {
        ingress_tx: ingress_tx.clone(),
        inner: Mutex::new(AuthorityInner {
            next_client_id: 1,
            next_authority_sequence: 1,
            clients: BTreeMap::new(),
            closing_by_sequence: BTreeMap::new(),
            subscribers: Vec::new(),
        }),
    });
    (
        Gate4AgentHandle {
            command_tx: GateIngressSender::Backend(ingress_tx),
            edge: Arc::clone(&edge),
        },
        ToolAuthorityHandle {
            edge: Arc::clone(&edge),
            authority: Arc::clone(&authority),
        },
        ControlPlaneKernelPort {
            ingress_rx,
            provider_runtime,
            edge,
            authority,
        },
    )
}

impl AgentPort for Gate4AgentHandle {
    fn dispatch(&self, command: CommandEnvelope) -> Result<(), PortDispatchError> {
        match &self.command_tx {
            GateIngressSender::Legacy(sender) => sender.try_send(command).map_err(map_port_error),
            GateIngressSender::Backend(sender) => sender
                .try_send(BackendIngress::Control(command))
                .map_err(map_port_error),
        }
    }

    fn snapshot(&self) -> Arc<ControlSnapshot> {
        Arc::clone(&self.edge.load_snapshot().control)
    }

    fn subscribe(&self, capacity: usize) -> EventSubscription {
        let receiver = subscribe_bounded(
            &self.edge.control_subscribers,
            capacity,
            MAX_CONTROL_SUBSCRIBERS,
        );
        EventSubscription { receiver }
    }
}

impl Gate4AgentHandle {
    pub fn dispatch(&self, command: CommandEnvelope) -> Result<(), PortDispatchError> {
        AgentPort::dispatch(self, command)
    }

    pub fn snapshot(&self) -> Arc<ControlSnapshot> {
        AgentPort::snapshot(self)
    }

    pub fn subscribe(&self, capacity: usize) -> EventSubscription {
        AgentPort::subscribe(self, capacity)
    }
}

impl KernelPort {
    pub fn drain_commands(&self, limit: usize) -> Vec<CommandEnvelope> {
        drain_receiver(&self.command_rx, limit)
    }

    pub fn publish_snapshot(&self, snapshot: ControlSnapshot) {
        let current = self.edge.load_snapshot();
        self.edge.publish_snapshot(BackendSnapshot {
            revision: snapshot.revision,
            logical_tick: current.logical_tick,
            control: Arc::new(snapshot),
            tools: Arc::clone(&current.tools),
            provider_runtime: current.provider_runtime.clone(),
        });
    }

    /// Slow subscribers are disconnected instead of silently losing ordered
    /// control events. Their next receive observes channel disconnection.
    pub fn publish_events(&self, events: impl IntoIterator<Item = ControlEvent>) -> PublishReport {
        publish_many(&self.edge.control_subscribers, events)
    }
}

impl ControlPlaneKernelPort {
    pub fn drain_ingress(&self, limit: usize) -> Vec<BackendIngress> {
        self.provider_runtime.flush_closing_bindings();
        drain_receiver(&self.ingress_rx, limit)
    }

    pub fn provider_authority(&self) -> ProviderRuntimeAuthorityHandle {
        self.provider_runtime.authority_handle()
    }

    pub fn publish_events(&self, events: impl IntoIterator<Item = ControlEvent>) -> PublishReport {
        publish_many(&self.edge.control_subscribers, events)
    }

    /// Publishes one already-reduced kernel step in a fixed edge order.
    ///
    /// Provider outcomes and effects first reconcile the private executor
    /// boundary. The immutable combined snapshot is then replaced before
    /// request/authority outcomes, control events, exact scoped completions,
    /// and their trailing source-gap marker. A successfully closed client's
    /// subscriptions are disconnected only after its `ClientClosed`
    /// completions have been offered.
    pub fn publish_step(&self, step: &KernelStep) -> ControlPlanePublishReport {
        let mut report = ControlPlanePublishReport::default();
        for outcome in &step.ingress_outcomes {
            if let BackendIngressOutcome::ToolProvider(outcome) = outcome {
                self.provider_runtime.publish_outcome(outcome);
            }
        }
        for effect in step.tool_effects.iter().cloned() {
            report.provider_effects += self.provider_runtime.publish_effect(effect);
        }
        self.provider_runtime.finish_step();
        self.provider_runtime
            .reconcile_snapshot(&step.backend_snapshot.provider_runtime);
        self.edge.publish_control_plane(
            step.backend_snapshot.clone(),
            self.provider_runtime.active_bindings(),
        );

        let mut completed_closes = Vec::new();
        for outcome in &step.ingress_outcomes {
            match outcome {
                BackendIngressOutcome::Control(_) => {}
                BackendIngressOutcome::ToolRequest(outcome) => {
                    report.request_outcomes += self.publish_request_outcome(outcome.clone());
                }
                BackendIngressOutcome::ToolAuthority(outcome) => {
                    let (published, completed_close) =
                        self.publish_authority_outcome(outcome.clone());
                    report.authority_outcomes += published;
                    if let Some(client) = completed_close {
                        completed_closes.push(client);
                    }
                }
                BackendIngressOutcome::ToolProvider(_) => {}
            }
        }

        report.control_events = self.publish_events(step.events.iter().cloned());
        report.completions = self.publish_completions(&step.tool_completions);
        for client in completed_closes {
            if self.finalize_client_close(&client) {
                report.closed_clients += 1;
            }
        }
        report
    }

    fn publish_request_outcome(&self, outcome: ToolRequestOutcome) -> PublishReport {
        let Some(client) = self.client_for(
            &outcome.request_key.consumer_id,
            &outcome.request_key.actor_id,
        ) else {
            return PublishReport::default();
        };
        let mut inner = client
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        publish_one_locked(&mut inner.request_subscribers, outcome)
    }

    fn publish_authority_outcome(
        &self,
        outcome: ToolAuthorityCommandOutcome,
    ) -> (PublishReport, Option<Arc<ClientState>>) {
        let mut authority = self
            .authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let report = publish_one_locked(&mut authority.subscribers, outcome.clone());
        let Some(consumer_id) = authority.closing_by_sequence.remove(&outcome.sequence) else {
            return (report, None);
        };
        let client = authority.clients.get(&consumer_id).cloned();
        if let Some(client) = &client {
            let mut inner = client
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if matches!(
                outcome.result,
                Ok(ToolAuthorityOutcome::ClientClosed { .. })
            ) {
                inner.lifecycle = ClientLifecycle::CloseSucceeded;
            } else {
                inner.lifecycle = ClientLifecycle::ClosingRetry;
                return (report, None);
            }
        }
        (report, client)
    }

    fn publish_completions(&self, batch: &CapabilityCompletionBatch) -> PublishReport {
        let mut report = PublishReport::default();
        for completion in &batch.completions {
            let Some(client) = self.client_for(
                &completion.request_key.consumer_id,
                &completion.request_key.actor_id,
            ) else {
                continue;
            };
            let mut inner = client
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            report += publish_one_locked(
                &mut inner.completion_subscribers,
                ToolCompletionDelivery::Completion(completion.clone()),
            );
        }

        let announce_gap = {
            let mut health = self
                .edge
                .completion_health
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let new_exhaustion = batch.sequence_exhausted && !health.sequence_exhaustion_announced;
            health.sequence_exhaustion_announced = batch.sequence_exhausted;
            batch.dropped_since_last_drain > 0 || new_exhaustion
        };
        if announce_gap {
            let gap = ToolCompletionDelivery::SourceGap(ToolCompletionSourceGap {
                dropped_since_last_drain: batch.dropped_since_last_drain,
                total_dropped: batch.total_dropped,
                next_sequence: batch.next_sequence,
                sequence_exhausted: batch.sequence_exhausted,
            });
            let clients = self.all_clients();
            for client in clients {
                let mut inner = client
                    .inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                report += publish_one_locked(&mut inner.completion_subscribers, gap.clone());
            }
        }
        report
    }

    fn client_for(
        &self,
        consumer_id: &ConsumerId,
        actor_id: &ToolActorId,
    ) -> Option<Arc<ClientState>> {
        self.authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clients
            .get(consumer_id)
            .filter(|client| &client.actor_id == actor_id)
            .cloned()
    }

    fn all_clients(&self) -> Vec<Arc<ClientState>> {
        self.authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clients
            .values()
            .cloned()
            .collect()
    }

    fn finalize_client_close(&self, client: &Arc<ClientState>) -> bool {
        let mut authority = self
            .authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let is_registered = authority
            .clients
            .get(&client.consumer_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, client));
        if !is_registered {
            return false;
        }
        let mut inner = client
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(inner.lifecycle, ClientLifecycle::CloseSucceeded) {
            return false;
        }
        inner.lifecycle = ClientLifecycle::Closed;
        inner.request_subscribers.clear();
        inner.completion_subscribers.clear();
        authority.clients.remove(&client.consumer_id);
        true
    }
}

impl ToolAuthorityHandle {
    pub fn bind_client(
        &self,
        actor_id: ToolActorId,
    ) -> Result<ToolClientHandle, ToolAuthorityError> {
        let mut authority = self
            .authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if authority.clients.len() >= MAX_TOOL_CLIENTS {
            return Err(ToolAuthorityError::ClientCapacityExceeded);
        }
        let current = authority.next_client_id;
        let next = current
            .checked_add(1)
            .ok_or(ToolAuthorityError::CounterExhausted {
                counter: "consumer-id",
            })?;
        let consumer_id =
            ConsumerId::new(format!("gate4agent-client-{current}")).map_err(|_| {
                ToolAuthorityError::CounterExhausted {
                    counter: "consumer-id",
                }
            })?;
        let state = Arc::new(ClientState {
            consumer_id: consumer_id.clone(),
            actor_id,
            authority: Arc::downgrade(&self.authority),
            edge: Arc::clone(&self.edge),
            inner: Mutex::new(ClientInner {
                lifecycle: ClientLifecycle::Active,
                request_subscribers: Vec::new(),
                completion_subscribers: Vec::new(),
            }),
        });
        authority.next_client_id = next;
        authority.clients.insert(consumer_id, Arc::clone(&state));
        Ok(ToolClientHandle { state })
    }

    pub fn dispatch(&self, command: ToolAuthorityCommand) -> Result<u64, ToolAuthorityError> {
        if matches!(command, ToolAuthorityCommand::CloseClient { .. }) {
            return Err(ToolAuthorityError::CloseRequiresMethod);
        }
        let mut authority = self
            .authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = authority.next_authority_sequence;
        let next = sequence
            .checked_add(1)
            .ok_or(ToolAuthorityError::CounterExhausted {
                counter: "authority-sequence",
            })?;
        let envelope = ToolAuthorityEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence,
            command,
        };
        match self
            .authority
            .ingress_tx
            .try_send(BackendIngress::ToolAuthority(envelope))
        {
            Ok(()) => {
                authority.next_authority_sequence = next;
                Ok(sequence)
            }
            Err(TrySendError::Full(_)) => Err(ToolAuthorityError::Full),
            Err(TrySendError::Disconnected(_)) => Err(ToolAuthorityError::Disconnected),
        }
    }

    /// Deactivates every clone before attempting to enqueue `CloseClient`.
    ///
    /// A full ingress leaves the client inactive and retryable without
    /// reserving the global authority sequence or blocking unrelated authority
    /// commands. Once enqueued, repeated calls return the one queued sequence.
    pub fn close_client(&self, client: &ToolClientHandle) -> Result<u64, ToolAuthorityError> {
        let Some(client_authority) = client.state.authority.upgrade() else {
            return Err(ToolAuthorityError::ForeignClient);
        };
        if !Arc::ptr_eq(&client_authority, &self.authority) {
            return Err(ToolAuthorityError::ForeignClient);
        }

        let mut authority = self
            .authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !authority
            .clients
            .get(&client.state.consumer_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, &client.state))
        {
            return Err(ToolAuthorityError::ClientClosed);
        }
        let mut inner = client
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sequence = match &inner.lifecycle {
            ClientLifecycle::ClosingSent { sequence } => return Ok(*sequence),
            ClientLifecycle::CloseSucceeded | ClientLifecycle::Closed => {
                return Err(ToolAuthorityError::ClientClosed)
            }
            ClientLifecycle::Active | ClientLifecycle::ClosingRetry => {
                let sequence = authority.next_authority_sequence;
                sequence
                    .checked_add(1)
                    .ok_or(ToolAuthorityError::CounterExhausted {
                        counter: "authority-sequence",
                    })?;
                inner.lifecycle = ClientLifecycle::ClosingRetry;
                sequence
            }
        };
        let next_sequence =
            sequence
                .checked_add(1)
                .ok_or(ToolAuthorityError::CounterExhausted {
                    counter: "authority-sequence",
                })?;
        let envelope = ToolAuthorityEnvelope {
            protocol_version: CAPABILITY_PROTOCOL_VERSION,
            sequence,
            command: ToolAuthorityCommand::CloseClient {
                consumer_id: client.state.consumer_id.clone(),
                actor_id: client.state.actor_id.clone(),
            },
        };
        match self
            .authority
            .ingress_tx
            .try_send(BackendIngress::ToolAuthority(envelope))
        {
            Ok(()) => {
                authority.next_authority_sequence = next_sequence;
                authority
                    .closing_by_sequence
                    .insert(sequence, client.state.consumer_id.clone());
                inner.lifecycle = ClientLifecycle::ClosingSent { sequence };
                Ok(sequence)
            }
            Err(TrySendError::Full(_)) => {
                inner.lifecycle = ClientLifecycle::ClosingRetry;
                Err(ToolAuthorityError::Full)
            }
            Err(TrySendError::Disconnected(_)) => Err(ToolAuthorityError::Disconnected),
        }
    }

    pub fn snapshot(&self) -> Arc<BackendSnapshot> {
        self.edge.load_snapshot()
    }

    pub fn subscribe_outcomes(&self, capacity: usize) -> ToolAuthorityOutcomeSubscription {
        let (sender, receiver) = sync_channel(bounded_subscription_capacity(capacity));
        let mut authority = self
            .authority
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if authority.subscribers.len() < MAX_AUTHORITY_SUBSCRIBERS {
            authority.subscribers.push(sender);
        }
        ToolAuthorityOutcomeSubscription { receiver }
    }
}

impl ToolClientHandle {
    pub fn consumer_id(&self) -> &ConsumerId {
        &self.state.consumer_id
    }

    pub fn actor_id(&self) -> &ToolActorId {
        &self.state.actor_id
    }

    pub fn dispatch(
        &self,
        request: CapabilityRequestInput,
    ) -> Result<gate4agent_tool_protocol::CapabilityRequestKey, ToolClientDispatchError> {
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(inner.lifecycle, ClientLifecycle::Active) {
            return Err(ToolClientDispatchError::Inactive);
        }
        let Some(authority) = self.state.authority.upgrade() else {
            inner.lifecycle = ClientLifecycle::Closed;
            return Err(ToolClientDispatchError::Disconnected);
        };
        let publication = self.state.edge.load_publication();
        let provider_id = &request.provider_id;
        let provider_is_registered = publication
            .snapshot
            .tools
            .providers
            .iter()
            .any(|provider| &provider.id == provider_id);
        let provider_binding_id = publication
            .available_provider_bindings
            .get(provider_id)
            .copied();
        if provider_is_registered && provider_binding_id.is_none() {
            return Err(ToolClientDispatchError::ProviderUnavailable {
                provider_id: provider_id.clone(),
            });
        }
        let envelope = ConsumerBoundCapabilityRequest::new(
            self.state.consumer_id.clone(),
            self.state.actor_id.clone(),
            request,
        );
        let request_key = envelope.key();
        authority
            .ingress_tx
            .try_send(BackendIngress::ToolRequest(
                ProviderBoundCapabilityRequest::new(provider_binding_id, envelope),
            ))
            .map_err(|error| match error {
                TrySendError::Full(_) => ToolClientDispatchError::Full,
                TrySendError::Disconnected(_) => ToolClientDispatchError::Disconnected,
            })?;
        Ok(request_key)
    }

    pub fn snapshot(&self) -> Arc<ToolClientSnapshot> {
        let publication = self.state.edge.load_publication();
        let snapshot = publication.snapshot;
        let available_provider_bindings = publication.available_provider_bindings;
        let tools = &snapshot.tools;
        let consumer_id = &self.state.consumer_id;
        let actor_id = &self.state.actor_id;
        let grants = tools
            .grants
            .iter()
            .filter(|grant| {
                &grant.key.consumer_id == consumer_id && &grant.key.actor_id == actor_id
            })
            .cloned()
            .collect::<Vec<_>>();
        let requests = tools
            .requests
            .iter()
            .filter(|request| {
                &request.key.consumer_id == consumer_id && &request.key.actor_id == actor_id
            })
            .cloned()
            .collect::<Vec<_>>();
        let visible_instances = grants
            .iter()
            .map(|grant| grant.key.instance_id)
            .chain(requests.iter().map(|request| request.instance_id))
            .collect::<BTreeSet<_>>();
        Arc::new(ToolClientSnapshot {
            protocol_version: tools.protocol_version,
            backend_revision: snapshot.revision,
            logical_tick: snapshot.logical_tick,
            tool_revision: tools.revision,
            current_tick: tools.current_tick,
            generations: tools
                .generations
                .iter()
                .filter(|(instance_id, _)| visible_instances.contains(instance_id))
                .copied()
                .collect(),
            instance_states: tools
                .instance_states
                .iter()
                .filter(|(instance_id, _)| visible_instances.contains(instance_id))
                .copied()
                .collect(),
            providers: tools
                .providers
                .iter()
                .filter(|provider| match &provider.owner {
                    CapabilityOwner::Gate => true,
                    CapabilityOwner::Consumer(owner) => owner == consumer_id,
                })
                .cloned()
                .collect(),
            available_providers: available_provider_bindings
                .iter()
                .filter(|(provider_id, _)| {
                    tools.providers.iter().any(|provider| {
                        &provider.id == *provider_id
                            && match &provider.owner {
                                CapabilityOwner::Gate => true,
                                CapabilityOwner::Consumer(owner) => owner == consumer_id,
                            }
                    })
                })
                .map(|(provider_id, _)| provider_id.clone())
                .collect(),
            grants,
            requests,
            audit_events: tools
                .audit_events
                .iter()
                .filter(|event| {
                    event.subject.as_ref().is_some_and(|subject| {
                        &subject.request_key.consumer_id == consumer_id
                            && &subject.request_key.actor_id == actor_id
                    })
                })
                .cloned()
                .collect(),
            dropped_audit_events: tools.dropped_audit_events,
            next_completion_sequence: tools.next_completion_sequence,
            dropped_completions: tools.dropped_completions,
            completion_sequence_exhausted: tools.completion_sequence_exhausted,
        })
    }

    pub fn subscribe_request_outcomes(&self, capacity: usize) -> ToolRequestOutcomeSubscription {
        let (sender, receiver) = sync_channel(bounded_subscription_capacity(capacity));
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(inner.lifecycle, ClientLifecycle::Closed)
            && inner.request_subscribers.len() < MAX_TOOL_SUBSCRIBERS_PER_CLIENT
        {
            inner.request_subscribers.push(sender);
        }
        ToolRequestOutcomeSubscription { receiver }
    }

    pub fn subscribe_completions(&self, capacity: usize) -> ToolCompletionSubscription {
        let (sender, receiver) = sync_channel(bounded_subscription_capacity(capacity));
        let mut inner = self
            .state
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(inner.lifecycle, ClientLifecycle::Closed)
            && inner.completion_subscribers.len() < MAX_TOOL_SUBSCRIBERS_PER_CLIENT
        {
            inner.completion_subscribers.push(sender);
        }
        ToolCompletionSubscription { receiver }
    }
}

impl EventSubscription {
    pub fn try_recv(&self) -> Result<ControlEvent, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl ToolRequestOutcomeSubscription {
    pub fn try_recv(&self) -> Result<ToolRequestOutcome, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl ToolCompletionSubscription {
    pub fn try_recv(&self) -> Result<ToolCompletionDelivery, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl ToolAuthorityOutcomeSubscription {
    pub fn try_recv(&self) -> Result<ToolAuthorityCommandOutcome, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl EdgeState {
    fn new() -> Self {
        Self {
            publication: RwLock::new(EdgePublication {
                snapshot: Arc::new(BackendSnapshot::default()),
                available_provider_bindings: BTreeMap::new(),
            }),
            control_subscribers: Mutex::new(Vec::new()),
            completion_health: Mutex::new(CompletionPublishHealth::default()),
        }
    }

    fn load_snapshot(&self) -> Arc<BackendSnapshot> {
        Arc::clone(
            &self
                .publication
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .snapshot,
        )
    }

    fn load_publication(&self) -> EdgePublication {
        self.publication
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn publish_snapshot(&self, snapshot: BackendSnapshot) {
        self.publication
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot = Arc::new(snapshot);
    }

    fn publish_control_plane(
        &self,
        snapshot: BackendSnapshot,
        available_provider_bindings: BTreeMap<ToolProviderId, ProviderBindingId>,
    ) {
        *self
            .publication
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = EdgePublication {
            snapshot: Arc::new(snapshot),
            available_provider_bindings,
        };
    }
}

impl std::ops::AddAssign for PublishReport {
    fn add_assign(&mut self, other: Self) {
        self.delivered += other.delivered;
        self.disconnected_slow += other.disconnected_slow;
        self.disconnected_closed += other.disconnected_closed;
    }
}

fn map_port_error<T>(error: TrySendError<T>) -> PortDispatchError {
    match error {
        TrySendError::Full(_) => PortDispatchError::Full,
        TrySendError::Disconnected(_) => PortDispatchError::Disconnected,
    }
}

fn drain_receiver<T>(receiver: &Receiver<T>, limit: usize) -> Vec<T> {
    let mut items = Vec::new();
    for _ in 0..limit {
        match receiver.try_recv() {
            Ok(item) => items.push(item),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    items
}

fn subscribe_bounded<T>(
    subscribers: &Mutex<Vec<SyncSender<T>>>,
    capacity: usize,
    max_subscribers: usize,
) -> Receiver<T> {
    let (sender, receiver) = sync_channel(bounded_subscription_capacity(capacity));
    let mut subscribers = subscribers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if subscribers.len() < max_subscribers {
        subscribers.push(sender);
    }
    receiver
}

fn bounded_ingress_capacity(requested: usize) -> usize {
    requested.clamp(1, MAX_INGRESS_CAPACITY)
}

fn bounded_subscription_capacity(requested: usize) -> usize {
    requested.clamp(1, MAX_SUBSCRIPTION_CAPACITY)
}

fn publish_many<T: Clone>(
    subscribers: &Mutex<Vec<SyncSender<T>>>,
    items: impl IntoIterator<Item = T>,
) -> PublishReport {
    let mut report = PublishReport::default();
    let mut subscribers = subscribers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for item in items {
        report += publish_one_locked(&mut subscribers, item);
    }
    report
}

fn publish_one_locked<T: Clone>(subscribers: &mut Vec<SyncSender<T>>, item: T) -> PublishReport {
    let mut report = PublishReport::default();
    let mut index = 0;
    while index < subscribers.len() {
        match subscribers[index].try_send(item.clone()) {
            Ok(()) => {
                report.delivered += 1;
                index += 1;
            }
            Err(TrySendError::Full(_)) => {
                subscribers.swap_remove(index);
                report.disconnected_slow += 1;
            }
            Err(TrySendError::Disconnected(_)) => {
                subscribers.swap_remove(index);
                report.disconnected_closed += 1;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_types::{
        AgentId, CommandId, ControlCommand, ControlEventKind, TransportKind,
        CONTROL_PROTOCOL_VERSION,
    };

    fn command(id: u64) -> CommandEnvelope {
        CommandEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: CommandId(id),
            command: ControlCommand::Register {
                instance_id: AgentInstanceId(id),
                agent_id: AgentId::new("claude").unwrap(),
                transport: TransportKind::Pty,
            },
        }
    }

    fn event(sequence: u64) -> ControlEvent {
        ControlEvent {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            sequence,
            command_id: None,
            instance_id: AgentInstanceId(1),
            generation: SessionGeneration(1),
            event: ControlEventKind::Registered,
        }
    }

    #[test]
    fn legacy_command_ingress_remains_bounded_and_non_blocking() {
        let (handle, kernel) = bounded_port(1);
        handle.dispatch(command(1)).unwrap();
        assert_eq!(handle.dispatch(command(2)), Err(PortDispatchError::Full));
        assert_eq!(kernel.drain_commands(10), vec![command(1)]);
    }

    #[test]
    fn legacy_snapshot_publication_replaces_current_control_truth() {
        let (handle, kernel) = bounded_port(1);
        let snapshot = ControlSnapshot {
            revision: 7,
            ..ControlSnapshot::default()
        };
        kernel.publish_snapshot(snapshot.clone());
        assert_eq!(*handle.snapshot(), snapshot);
    }

    #[test]
    fn slow_control_subscriber_is_disconnected_without_silent_loss() {
        let (handle, kernel) = bounded_port(1);
        let subscription = handle.subscribe(1);
        assert_eq!(kernel.publish_events([event(1)]).delivered, 1);

        let report = kernel.publish_events([event(2)]);
        assert_eq!(report.disconnected_slow, 1);
        assert_eq!(subscription.try_recv().unwrap(), event(1));
        assert_eq!(subscription.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[test]
    fn closed_control_subscriber_is_pruned_explicitly() {
        let (handle, kernel) = bounded_port(1);
        let subscription = handle.subscribe(1);
        drop(subscription);

        let report = kernel.publish_events([event(1)]);
        assert_eq!(report.disconnected_closed, 1);
        assert_eq!(report.delivered, 0);
    }
}
