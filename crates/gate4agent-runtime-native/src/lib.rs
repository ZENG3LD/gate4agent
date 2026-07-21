//! Tick-driven native runtime for embedding gate4agent in an owning app core.

mod launch_profiles;

pub use launch_profiles::{
    NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver, NativeLaunchProfile,
    NativeLaunchProfileError, NativeLaunchProfileId,
};

use gate4agent_catalog::{AgentRegistry, EnvMutation};
use gate4agent_handle::{
    bounded_control_plane, ControlPlaneKernelPort, Gate4AgentHandle,
    ProviderRuntimeError, PublishReport, ToolAuthorityHandle,
};
use gate4agent_kernel::{CommandOutcome, Gate4AgentKernel};
use gate4agent_provider_ports::{
    discover_history, load_history_session, prepare_resume, HistoryCandidate,
    HistoryDiscoveryRequest, HistoryLoadRequest, PreparedResume, ResumeAuthority,
    ResumeAuthorityDecision, ResumeOutcome, ResumeRequest,
};
use gate4agent_shell_capabilities::NativeCapabilityProbeAuthority;
use gate4agent_shell_history::NativeHistoryAuthority;
pub use gate4agent_shell_history::{NativeHistoryConfig, NativeHistoryRoot};
pub use gate4agent_shell_hooks::{HookIngressConfig, HookIngressEndpoint};
use gate4agent_shell_hooks::{HookIngressControl, HookIngressServer, HookIngressStartError};
pub use gate4agent_shell_native::{
    NativeProviderExecutor, NativeProviderExit, NativeProviderOperation,
    NativeProviderOperationError, NativeProviderResultPoll, PhysicalExitAck,
    ProviderOperationKey, ProviderStopCause, ProviderSupervisorFault,
    ProviderSupervisorFaultKind, ProviderSupervisorSnapshot, ProviderSupervisorState,
};
use gate4agent_shell_native::{
    NativeEffectShell, ProviderSupervisor, ProviderSupervisorBuildError,
    MAX_PROVIDER_SUPERVISOR_EVENTS,
};
use gate4agent_tool_engine::{
    CapabilityOwner, CapabilityProviderDescriptor, ProviderBindingId, ToolEngineError,
    ToolProviderId,
};
use gate4agent_types::{
    AgentId, AgentInstanceId, CapabilityProbeFailure, ControlEffect, ControlObservation,
    EffectEnvelope, HistoryCandidateSummary, HistoryMessageRecord, HistoryMessageRole,
    HistorySessionRecord, ObservationEnvelope, ResumeAuthorityTarget, ResumeLaunchRequest,
    SessionGeneration, SessionStatus, TransportKind, CONTROL_PROTOCOL_VERSION,
};
use launch_profiles::NativeLaunchProfiles;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};
use tokio::time::MissedTickBehavior;

type TerminalFrameKey = (AgentInstanceId, SessionGeneration);
const PROVIDER_EVENT_DRAIN_QUANTUM: usize = 64;
const MAX_PROVIDER_SHUTDOWN_TIMEOUT_MS: u64 = 86_400_000;

fn drain_queue<T>(queue: &mut VecDeque<T>, limit: usize) -> Vec<T> {
    let count = limit.min(queue.len());
    queue.drain(..count).collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeRuntimeConfig {
    pub command_capacity: usize,
    pub max_commands_per_tick: usize,
    pub effect_capacity_per_session: usize,
    pub observation_capacity: usize,
    pub max_observations_per_tick: usize,
    pub provider_stop_grace_ms: u64,
    pub provider_shutdown_timeout_ms: u64,
    pub worker_poll_interval_ms: u64,
    pub worker_idle_timeout_ms: u64,
}

impl Default for NativeRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 256,
            max_commands_per_tick: 64,
            effect_capacity_per_session: 16,
            observation_capacity: 1_024,
            max_observations_per_tick: 256,
            provider_stop_grace_ms: 5_000,
            provider_shutdown_timeout_ms: 15_000,
            worker_poll_interval_ms: 20,
            worker_idle_timeout_ms: 60_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRuntimeTick {
    pub command_outcomes: Vec<CommandOutcome>,
    pub effects_dispatched: usize,
    pub observations_applied: usize,
    pub terminal_frames_collected: usize,
    pub snapshot_revision: u64,
    pub publish_report: PublishReport,
}

/// Owns the kernel tick and all native effect workers. Product apps retain
/// only the bounded handle returned by [`NativeRuntime::new`].
pub struct NativeRuntime {
    config: NativeRuntimeConfig,
    kernel: Gate4AgentKernel,
    handle: Gate4AgentHandle,
    tool_authority: ToolAuthorityHandle,
    tool_providers: BTreeMap<ToolProviderId, CapabilityProviderDescriptor>,
    provider_supervisors: BTreeMap<ToolProviderId, ProviderSupervisor>,
    port: ControlPlaneKernelPort,
    provider_exit_acks: VecDeque<PhysicalExitAck>,
    provider_faults: VecDeque<ProviderSupervisorFault>,
    provider_ack_cursor: Option<ToolProviderId>,
    provider_fault_cursor: Option<ToolProviderId>,
    effects: NativeEffectDispatcher,
    hook_ingress: Option<HookIngressServer>,
}

impl NativeRuntime {
    pub fn new(catalog: AgentRegistry, config: NativeRuntimeConfig) -> (Gate4AgentHandle, Self) {
        Self::new_with_optional_history(catalog, config, None)
    }

    pub fn new_with_history(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: NativeHistoryConfig,
    ) -> (Gate4AgentHandle, Self) {
        Self::new_with_optional_history(catalog, config, Some(history))
    }

    pub fn new_with_tool_providers(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        providers: impl IntoIterator<Item = CapabilityProviderDescriptor>,
    ) -> Result<(Gate4AgentHandle, Self), ToolEngineError> {
        let providers = providers.into_iter().collect::<Vec<_>>();
        let kernel = Gate4AgentKernel::with_tool_providers(catalog.clone(), providers.clone())?;
        Ok(Self::new_with_kernel_and_optional_history(
            catalog, config, None, kernel, providers,
        ))
    }

    pub fn new_with_history_and_tool_providers(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: NativeHistoryConfig,
        providers: impl IntoIterator<Item = CapabilityProviderDescriptor>,
    ) -> Result<(Gate4AgentHandle, Self), ToolEngineError> {
        let providers = providers.into_iter().collect::<Vec<_>>();
        let kernel = Gate4AgentKernel::with_tool_providers(catalog.clone(), providers.clone())?;
        Ok(Self::new_with_kernel_and_optional_history(
            catalog,
            config,
            Some(history),
            kernel,
            providers,
        ))
    }

    fn new_with_optional_history(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: Option<NativeHistoryConfig>,
    ) -> (Gate4AgentHandle, Self) {
        let kernel = Gate4AgentKernel::new(catalog.clone());
        Self::new_with_kernel_and_optional_history(catalog, config, history, kernel, Vec::new())
    }

    fn new_with_kernel_and_optional_history(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: Option<NativeHistoryConfig>,
        kernel: Gate4AgentKernel,
        providers: Vec<CapabilityProviderDescriptor>,
    ) -> (Gate4AgentHandle, Self) {
        let (handle, tool_authority, port) = bounded_control_plane(config.command_capacity);
        let runtime = Self {
            config,
            kernel,
            handle: handle.clone(),
            tool_authority,
            tool_providers: providers
                .into_iter()
                .map(|descriptor| (descriptor.id.clone(), descriptor))
                .collect(),
            provider_supervisors: BTreeMap::new(),
            port,
            provider_exit_acks: VecDeque::new(),
            provider_faults: VecDeque::new(),
            provider_ack_cursor: None,
            provider_fault_cursor: None,
            effects: NativeEffectDispatcher::new(catalog, config, history),
            hook_ingress: None,
        };
        (handle, runtime)
    }

    pub fn tool_authority(&self) -> ToolAuthorityHandle {
        self.tool_authority.clone()
    }

    pub fn install_native_provider(
        &mut self,
        provider_id: &ToolProviderId,
        work_capacity: usize,
        executor: Box<dyn NativeProviderExecutor>,
    ) -> Result<ProviderBindingId, NativeProviderControlError> {
        self.collect_provider_supervisor_events();
        if let Some(existing) = self.provider_supervisors.get(provider_id) {
            let snapshot = existing.snapshot();
            if snapshot.state != ProviderSupervisorState::Closed {
                return Err(NativeProviderControlError::AlreadyInstalled {
                    state: snapshot.state,
                });
            }
            if snapshot.buffered_exit_acks != 0 || snapshot.buffered_faults != 0 {
                return Err(NativeProviderControlError::PendingSupervisorEvents);
            }
        }
        self.provider_supervisors.remove(provider_id);

        let descriptor = self
            .tool_providers
            .get(provider_id)
            .cloned()
            .ok_or_else(|| NativeProviderControlError::UnknownProvider {
                provider_id: provider_id.clone(),
            })?;
        if !matches!(&descriptor.owner, CapabilityOwner::Gate) {
            return Err(NativeProviderControlError::UnsupportedOwner {
                provider_id: provider_id.clone(),
            });
        }
        let runtime = self
            .port
            .provider_authority()
            .bind_provider(provider_id.clone(), work_capacity)?;
        let binding_id = runtime.binding_id();
        let supervisor = ProviderSupervisor::new_with_stop_grace(
            descriptor,
            runtime,
            executor,
            Duration::from_millis(self.config.provider_stop_grace_ms.max(1)),
        )
        .map_err(NativeProviderControlError::Build)?;
        self.provider_supervisors
            .insert(provider_id.clone(), supervisor);
        Ok(binding_id)
    }

    pub fn retire_native_provider(
        &mut self,
        provider_id: &ToolProviderId,
    ) -> Result<(), NativeProviderControlError> {
        let supervisor = self
            .provider_supervisors
            .get_mut(provider_id)
            .ok_or_else(|| NativeProviderControlError::NotInstalled {
                provider_id: provider_id.clone(),
            })?;
        supervisor.begin_retirement()?;
        Ok(())
    }

    /// Begins non-blocking retirement for every installed native provider.
    ///
    /// The owner must keep calling [`NativeRuntime::tick`] until
    /// [`NativeRuntime::native_provider_shutdown_complete`] returns `true`
    /// before dropping the runtime. Dropping the runtime does not acknowledge
    /// physical provider exit or complete coordinated shutdown.
    pub fn retire_all_native_providers(&mut self) -> Result<(), NativeProviderControlError> {
        let mut first_error = None;
        for supervisor in self.provider_supervisors.values_mut() {
            if let Err(error) = supervisor.begin_retirement() {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    /// Returns `true` only after every installed provider supervisor is closed.
    pub fn native_provider_shutdown_complete(&self) -> bool {
        self.provider_supervisors
            .values()
            .all(|supervisor| supervisor.state() == ProviderSupervisorState::Closed)
    }

    /// Drives the canonical runtime path until every native provider has
    /// detached after physical teardown, or returns the still-owned snapshots
    /// at the configured shutdown deadline. Lifecycle events remain buffered
    /// for explicit operator drain after this method returns.
    pub async fn shutdown_native_providers(
        &mut self,
    ) -> Result<(), NativeProviderShutdownError> {
        let retirement_error = self.retire_all_native_providers().err();
        let shutdown_timeout_ms = self
            .config
            .provider_shutdown_timeout_ms
            .clamp(1, MAX_PROVIDER_SHUTDOWN_TIMEOUT_MS);
        let deadline = Instant::now() + Duration::from_millis(shutdown_timeout_ms);
        while !self.native_provider_shutdown_complete() {
            self.tick().await;
            if self.native_provider_shutdown_complete() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(NativeProviderShutdownError::TimedOut {
                    pending: self
                        .provider_supervisors
                        .values()
                        .filter(|supervisor| {
                            supervisor.state() != ProviderSupervisorState::Closed
                        })
                        .map(ProviderSupervisor::snapshot)
                        .collect(),
                });
            }
            let poll_interval =
                Duration::from_millis(self.config.worker_poll_interval_ms.max(1));
            tokio::time::sleep(poll_interval.min(deadline.duration_since(now))).await;
        }
        match retirement_error {
            Some(error) => Err(NativeProviderShutdownError::Control(error)),
            None => Ok(()),
        }
    }

    pub fn native_provider_snapshot(
        &self,
        provider_id: &ToolProviderId,
    ) -> Option<ProviderSupervisorSnapshot> {
        self.provider_supervisors
            .get(provider_id)
            .map(ProviderSupervisor::snapshot)
    }

    pub fn drain_provider_exit_acks(&mut self, limit: usize) -> Vec<PhysicalExitAck> {
        self.collect_provider_supervisor_events();
        drain_queue(&mut self.provider_exit_acks, limit)
    }

    pub fn drain_provider_faults(&mut self, limit: usize) -> Vec<ProviderSupervisorFault> {
        self.collect_provider_supervisor_events();
        drain_queue(&mut self.provider_faults, limit)
    }

    pub fn history_enabled(&self) -> bool {
        self.effects.history_config.is_some()
    }

    /// Run one non-blocking host tick. Effects are dispatched in session order
    /// to per-instance workers; their observations enter later ticks.
    pub async fn tick(&mut self) -> NativeRuntimeTick {
        let (observations, terminal_frames_collected) = self
            .effects
            .drain_observations(self.config.max_observations_per_tick.max(1));
        let observations_applied = observations.len();
        let ingress = self
            .port
            .drain_ingress(self.config.max_commands_per_tick.max(1));
        let step = self.kernel.step_control_plane(ingress, observations);
        let effects_dispatched = step.effects.len();
        for effect in step.effects.iter().cloned() {
            self.effects.dispatch(effect);
        }

        let snapshot_revision = step.snapshot.revision;
        let publish_report = self.port.publish_step(&step).control_events;
        for supervisor in self.provider_supervisors.values_mut() {
            supervisor.tick();
        }
        self.collect_provider_supervisor_events();

        NativeRuntimeTick {
            command_outcomes: step.command_outcomes,
            effects_dispatched,
            observations_applied,
            terminal_frames_collected,
            snapshot_revision,
            publish_report,
        }
    }

    fn collect_provider_supervisor_events(&mut self) {
        self.collect_provider_exit_acks();
        self.collect_provider_faults();
    }

    fn collect_provider_exit_acks(&mut self) {
        let remaining =
            MAX_PROVIDER_SUPERVISOR_EVENTS.saturating_sub(self.provider_exit_acks.len());
        if remaining == 0 {
            return;
        }
        let provider_ids = provider_ids_after(
            &self.provider_supervisors,
            self.provider_ack_cursor.as_ref(),
        );
        let mut remaining = remaining;
        for provider_id in provider_ids {
            let limit = remaining.min(PROVIDER_EVENT_DRAIN_QUANTUM);
            let drained = self
                .provider_supervisors
                .get_mut(&provider_id)
                .map_or_else(Vec::new, |supervisor| supervisor.drain_exit_acks(limit));
            if !drained.is_empty() {
                remaining -= drained.len();
                self.provider_exit_acks.extend(drained);
                self.provider_ack_cursor = Some(provider_id);
            }
            if remaining == 0 {
                break;
            }
        }
    }

    fn collect_provider_faults(&mut self) {
        let remaining =
            MAX_PROVIDER_SUPERVISOR_EVENTS.saturating_sub(self.provider_faults.len());
        if remaining == 0 {
            return;
        }
        let provider_ids = provider_ids_after(
            &self.provider_supervisors,
            self.provider_fault_cursor.as_ref(),
        );
        let mut remaining = remaining;
        for provider_id in provider_ids {
            let limit = remaining.min(PROVIDER_EVENT_DRAIN_QUANTUM);
            let drained = self
                .provider_supervisors
                .get_mut(&provider_id)
                .map_or_else(Vec::new, |supervisor| supervisor.drain_faults(limit));
            if !drained.is_empty() {
                remaining -= drained.len();
                self.provider_faults.extend(drained);
                self.provider_fault_cursor = Some(provider_id);
            }
            if remaining == 0 {
                break;
            }
        }
    }

    pub fn active_native_sessions(&self) -> usize {
        self.effects.active_sessions.load(Ordering::Acquire)
    }

    /// Installs or replaces a bounded host-only profile for future spawns.
    pub fn upsert_native_launch_profile(
        &mut self,
        profile: NativeLaunchProfile,
    ) -> Result<(), NativeLaunchProfileError> {
        self.effects.launch_profiles.upsert(profile)
    }

    /// Removes an unselected host-only profile.
    pub fn remove_native_launch_profile(
        &mut self,
        profile_id: &NativeLaunchProfileId,
    ) -> Result<bool, NativeLaunchProfileError> {
        self.effects.launch_profiles.remove(profile_id)
    }

    /// Selects a profile for future spawns of one exact instance.
    ///
    /// Spawn dispatch is the linearization point: a selection change does not
    /// alter a child that was already dispatched or started.
    pub fn select_native_launch_profile(
        &mut self,
        instance_id: AgentInstanceId,
        profile_id: NativeLaunchProfileId,
    ) -> Result<(), NativeLaunchProfileError> {
        self.effects.launch_profiles.select(instance_id, profile_id)
    }

    /// Clears one instance selection for future spawns only.
    pub fn clear_native_launch_profile_selection(
        &mut self,
        instance_id: AgentInstanceId,
    ) -> bool {
        self.effects.launch_profiles.clear_selection(instance_id)
    }

    /// Start the process-global loopback Hook listener. Call this before
    /// starting agent sessions so their PTY environments receive route-scoped
    /// endpoint coordinates.
    pub async fn start_hook_ingress(
        &mut self,
        config: HookIngressConfig,
    ) -> Result<HookIngressEndpoint, NativeHookIngressError> {
        if let Some(server) = &self.hook_ingress {
            if server.is_running() {
                return Ok(server.endpoint().clone());
            }
        }
        let has_started_session = self.handle.snapshot().sessions.iter().any(|session| {
            matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping
            )
        });
        if self.active_native_sessions() != 0 || has_started_session {
            return Err(NativeHookIngressError::ActiveSessions);
        }
        self.hook_ingress = None;
        let server = HookIngressServer::start(self.handle.clone(), config).await?;
        let endpoint = server.endpoint().clone();
        self.effects.set_hook_ingress(Some(server.control()));
        self.hook_ingress = Some(server);
        Ok(endpoint)
    }

    pub async fn stop_hook_ingress(&mut self) {
        self.effects.set_hook_ingress(None);
        if let Some(server) = self.hook_ingress.take() {
            server.stop().await;
        }
    }

    pub fn hook_ingress_endpoint(&self) -> Option<&HookIngressEndpoint> {
        self.hook_ingress
            .as_ref()
            .filter(|server| server.is_running())
            .map(HookIngressServer::endpoint)
    }

    pub fn active_hook_routes(&self) -> usize {
        self.hook_ingress
            .as_ref()
            .map_or(0, |server| server.control().active_route_count())
    }
}

fn provider_ids_after(
    supervisors: &BTreeMap<ToolProviderId, ProviderSupervisor>,
    cursor: Option<&ToolProviderId>,
) -> Vec<ToolProviderId> {
    let mut provider_ids = supervisors.keys().cloned().collect::<Vec<_>>();
    let Some(cursor) = cursor else {
        return provider_ids;
    };
    let start = provider_ids
        .iter()
        .position(|provider_id| provider_id > cursor)
        .unwrap_or(0);
    provider_ids.rotate_left(start);
    provider_ids
}

#[derive(Debug, Error)]
pub enum NativeProviderControlError {
    #[error("tool provider '{provider_id}' is not registered in this native runtime")]
    UnknownProvider { provider_id: ToolProviderId },
    #[error("tool provider '{provider_id}' is not owned by gate4agent")]
    UnsupportedOwner { provider_id: ToolProviderId },
    #[error("native provider supervisor is already installed in state {state:?}")]
    AlreadyInstalled { state: ProviderSupervisorState },
    #[error("native provider supervisor is not installed for '{provider_id}'")]
    NotInstalled { provider_id: ToolProviderId },
    #[error("retired provider supervisor still has undelivered lifecycle events")]
    PendingSupervisorEvents,
    #[error("native provider runtime failed: {0}")]
    Runtime(#[from] ProviderRuntimeError),
    #[error("native provider supervisor build failed: {0:?}")]
    Build(ProviderSupervisorBuildError),
}

#[derive(Debug, Error)]
pub enum NativeProviderShutdownError {
    #[error("native provider retirement reported an error: {0}")]
    Control(NativeProviderControlError),
    #[error("native provider shutdown timed out with physical owners retained")]
    TimedOut { pending: Vec<ProviderSupervisorSnapshot> },
}

#[derive(Debug, Error)]
pub enum NativeHookIngressError {
    #[error("hook ingress must start before native agent sessions")]
    ActiveSessions,
    #[error(transparent)]
    Start(#[from] HookIngressStartError),
}

struct EffectWorker {
    sender: Sender<NativeEffectRequest>,
}

struct NativeEffectRequest {
    effect: EffectEnvelope,
    pty_env: Vec<EnvMutation>,
}

#[derive(Clone)]
struct NativeWorkerContext {
    control_tx: Sender<ObservationEnvelope>,
    terminal_frames: Arc<Mutex<BTreeMap<TerminalFrameKey, ObservationEnvelope>>>,
    active_sessions: Arc<AtomicUsize>,
    hook_ingress: Arc<RwLock<Option<HookIngressControl>>>,
    poll_interval: Duration,
    idle_timeout: Duration,
}

struct NativeEffectDispatcher {
    catalog: AgentRegistry,
    config: NativeRuntimeConfig,
    launch_profiles: NativeLaunchProfiles,
    workers: HashMap<AgentInstanceId, EffectWorker>,
    authority_worker: Option<EffectWorker>,
    capability_worker: Option<EffectWorker>,
    history_config: Option<NativeHistoryConfig>,
    control_tx: Sender<ObservationEnvelope>,
    control_rx: Receiver<ObservationEnvelope>,
    pending_failures: VecDeque<ObservationEnvelope>,
    terminal_frames: Arc<Mutex<BTreeMap<TerminalFrameKey, ObservationEnvelope>>>,
    active_sessions: Arc<AtomicUsize>,
    hook_ingress: Arc<RwLock<Option<HookIngressControl>>>,
}

impl NativeEffectDispatcher {
    fn new(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history_config: Option<NativeHistoryConfig>,
    ) -> Self {
        let (control_tx, control_rx) = mpsc::channel(config.observation_capacity.max(1));
        Self {
            catalog,
            config,
            launch_profiles: NativeLaunchProfiles::new(),
            workers: HashMap::new(),
            authority_worker: None,
            capability_worker: None,
            history_config,
            control_tx,
            control_rx,
            pending_failures: VecDeque::new(),
            terminal_frames: Arc::new(Mutex::new(BTreeMap::new())),
            active_sessions: Arc::new(AtomicUsize::new(0)),
            hook_ingress: Arc::new(RwLock::new(None)),
        }
    }

    fn set_hook_ingress(&self, control: Option<HookIngressControl>) {
        *self
            .hook_ingress
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = control;
    }

    fn dispatch(&mut self, effect: EffectEnvelope) {
        if matches!(effect.effect, ControlEffect::ProbeCapabilities { .. }) {
            self.dispatch_capability(effect);
            return;
        }
        if matches!(
            effect.effect,
            ControlEffect::DiscoverHistory { .. }
                | ControlEffect::LoadHistory { .. }
                | ControlEffect::AuthorizeResume { .. }
        ) {
            self.dispatch_authority(effect);
            return;
        }
        self.workers.retain(|_, worker| !worker.sender.is_closed());
        let instance_id = effect.instance_id;
        let pty_env = match self.compose_pty_spawn_environment(&effect) {
            Ok(environment) => environment,
            Err(message) => {
                self.pending_failures
                    .push_back(effect_failure(effect, message));
                return;
            }
        };
        let mut pending = NativeEffectRequest { effect, pty_env };
        for _ in 0..2 {
            let sender = self.worker_sender(instance_id);
            match sender.try_send(pending) {
                Ok(()) => return,
                Err(TrySendError::Closed(request)) => {
                    self.workers.remove(&instance_id);
                    pending = request;
                }
                Err(TrySendError::Full(request)) => {
                    self.remove_hook_route(&request.effect);
                    self.pending_failures.push_back(effect_failure(
                        request.effect,
                        "native session effect queue is full".to_owned(),
                    ));
                    return;
                }
            }
        }
        self.remove_hook_route(&pending.effect);
        self.pending_failures.push_back(effect_failure(
            pending.effect,
            "native session effect worker is unavailable".to_owned(),
        ));
    }

    fn dispatch_capability(&mut self, effect: EffectEnvelope) {
        let mut pending = NativeEffectRequest {
            effect,
            pty_env: Vec::new(),
        };
        for _ in 0..2 {
            let sender = self.capability_sender();
            match sender.try_send(pending) {
                Ok(()) => return,
                Err(TrySendError::Closed(request)) => {
                    self.capability_worker = None;
                    pending = request;
                }
                Err(TrySendError::Full(request)) => {
                    self.pending_failures.push_back(effect_failure(
                        request.effect,
                        "native capability effect queue is full".to_owned(),
                    ));
                    return;
                }
            }
        }
        self.pending_failures.push_back(effect_failure(
            pending.effect,
            "native capability effect worker is unavailable".to_owned(),
        ));
    }

    fn capability_sender(&mut self) -> Sender<NativeEffectRequest> {
        if let Some(worker) = &self.capability_worker {
            if !worker.sender.is_closed() {
                return worker.sender.clone();
            }
        }
        let (sender, receiver) = mpsc::channel(self.config.effect_capacity_per_session.max(1));
        tokio::spawn(run_capability_worker(
            self.catalog.clone(),
            receiver,
            self.control_tx.clone(),
        ));
        self.capability_worker = Some(EffectWorker {
            sender: sender.clone(),
        });
        sender
    }

    fn dispatch_authority(&mut self, effect: EffectEnvelope) {
        if self.history_config.is_none()
            && matches!(
                effect.effect,
                ControlEffect::DiscoverHistory { .. } | ControlEffect::LoadHistory { .. }
            )
        {
            self.pending_failures.push_back(effect_failure(
                effect,
                "native history authority is not configured".to_owned(),
            ));
            return;
        }
        let mut pending = NativeEffectRequest {
            effect,
            pty_env: Vec::new(),
        };
        for _ in 0..2 {
            let sender = self.authority_sender();
            match sender.try_send(pending) {
                Ok(()) => return,
                Err(TrySendError::Closed(request)) => {
                    self.authority_worker = None;
                    pending = request;
                }
                Err(TrySendError::Full(request)) => {
                    self.pending_failures.push_back(effect_failure(
                        request.effect,
                        "native authority effect queue is full".to_owned(),
                    ));
                    return;
                }
            }
        }
        self.pending_failures.push_back(effect_failure(
            pending.effect,
            "native authority effect worker is unavailable".to_owned(),
        ));
    }

    fn authority_sender(&mut self) -> Sender<NativeEffectRequest> {
        if let Some(worker) = &self.authority_worker {
            if !worker.sender.is_closed() {
                return worker.sender.clone();
            }
        }
        let (sender, receiver) = mpsc::channel(self.config.effect_capacity_per_session.max(1));
        tokio::spawn(run_authority_worker(
            self.catalog.clone(),
            self.history_config.clone(),
            receiver,
            self.control_tx.clone(),
        ));
        self.authority_worker = Some(EffectWorker {
            sender: sender.clone(),
        });
        sender
    }

    fn hook_pty_env(&self, effect: &EffectEnvelope) -> Result<Vec<EnvMutation>, String> {
        let agent_id = match &effect.effect {
            ControlEffect::Spawn {
                agent_id,
                transport: gate4agent_types::TransportKind::Pty,
                ..
            }
            | ControlEffect::SpawnResume { agent_id, .. } => agent_id,
            _ => return Ok(Vec::new()),
        };
        let control = self
            .hook_ingress
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(control) = control else {
            return Ok(Vec::new());
        };
        let binding = self
            .catalog
            .get(agent_id)
            .and_then(|spec| spec.capabilities.adapters.hook.clone());
        let Some(binding) = binding else {
            return Ok(Vec::new());
        };
        let route = control
            .register_route(effect.instance_id, effect.generation, binding)
            .map_err(|error| error.to_string())?;
        Ok(route
            .environment()
            .into_iter()
            .map(|(key, value)| EnvMutation {
                key: key.into(),
                value: Some(value.into()),
            })
            .collect())
    }

    fn compose_pty_spawn_environment(
        &self,
        effect: &EffectEnvelope,
    ) -> Result<Vec<EnvMutation>, String> {
        let mut environment = self.profile_pty_env(effect)?;
        environment.extend(self.hook_pty_env(effect)?);
        Ok(environment)
    }

    fn profile_pty_env(&self, effect: &EffectEnvelope) -> Result<Vec<EnvMutation>, String> {
        let (agent_id, transport) = match &effect.effect {
            ControlEffect::Spawn {
                agent_id,
                transport,
                ..
            } => (agent_id, *transport),
            ControlEffect::SpawnResume { agent_id, .. } => (agent_id, TransportKind::Pty),
            _ => return Ok(Vec::new()),
        };
        self.launch_profiles
            .resolve_environment(effect.instance_id, agent_id, transport)
            .map_err(|error| error.to_string())
    }

    fn remove_hook_route(&self, effect: &EffectEnvelope) {
        if !matches!(
            effect.effect,
            ControlEffect::Spawn { .. } | ControlEffect::SpawnResume { .. }
        ) {
            return;
        }
        if let Some(control) = self
            .hook_ingress
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            control.remove_route(effect.instance_id, effect.generation);
        }
    }

    fn worker_sender(&mut self, instance_id: AgentInstanceId) -> Sender<NativeEffectRequest> {
        if let Some(worker) = self.workers.get(&instance_id) {
            return worker.sender.clone();
        }
        let (sender, receiver) = mpsc::channel(self.config.effect_capacity_per_session.max(1));
        tokio::spawn(run_effect_worker(
            self.catalog.clone(),
            receiver,
            NativeWorkerContext {
                control_tx: self.control_tx.clone(),
                terminal_frames: Arc::clone(&self.terminal_frames),
                active_sessions: Arc::clone(&self.active_sessions),
                hook_ingress: Arc::clone(&self.hook_ingress),
                poll_interval: Duration::from_millis(self.config.worker_poll_interval_ms.max(1)),
                idle_timeout: Duration::from_millis(self.config.worker_idle_timeout_ms.max(1)),
            },
        ));
        self.workers.insert(
            instance_id,
            EffectWorker {
                sender: sender.clone(),
            },
        );
        sender
    }

    fn drain_observations(&mut self, limit: usize) -> (Vec<ObservationEnvelope>, usize) {
        self.workers.retain(|_, worker| !worker.sender.is_closed());
        let mut observations = Vec::new();
        while observations.len() < limit {
            if let Some(observation) = self.pending_failures.pop_front() {
                observations.push(observation);
                continue;
            }
            match self.control_rx.try_recv() {
                Ok(observation) => observations.push(observation),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }

        let remaining = limit.saturating_sub(observations.len());
        let mut frames = self
            .terminal_frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys: Vec<_> = frames.keys().copied().take(remaining).collect();
        let terminal_frames_collected = keys.len();
        for key in keys {
            if let Some(frame) = frames.remove(&key) {
                observations.push(frame);
            }
        }
        (observations, terminal_frames_collected)
    }
}

const HISTORY_INSTANCE_DISCOVERIES_MAX: usize = 1_024;

struct InstanceHistoryDiscovery {
    generation: SessionGeneration,
    agent_id: AgentId,
    request: HistoryDiscoveryRequest,
    candidates: HashMap<String, HistoryCandidate>,
}

struct NativeAuthorityWorkerState {
    catalog: AgentRegistry,
    authority: Option<NativeHistoryAuthority>,
    discoveries: HashMap<AgentInstanceId, InstanceHistoryDiscovery>,
    discovery_order: VecDeque<AgentInstanceId>,
}

impl NativeAuthorityWorkerState {
    fn new(catalog: AgentRegistry, config: Option<NativeHistoryConfig>) -> Self {
        Self {
            catalog,
            authority: config.map(NativeHistoryAuthority::new),
            discoveries: HashMap::new(),
            discovery_order: VecDeque::new(),
        }
    }

    fn execute(&mut self, envelope: EffectEnvelope) -> ObservationEnvelope {
        let EffectEnvelope {
            protocol_version,
            operation_id,
            instance_id,
            generation,
            effect,
        } = envelope;
        let is_resume = matches!(effect, ControlEffect::AuthorizeResume { .. });
        let observation = if protocol_version != CONTROL_PROTOCOL_VERSION {
            authority_failure(
                is_resume,
                format!(
                    "authority effect protocol version {protocol_version} is unsupported; expected {CONTROL_PROTOCOL_VERSION}"
                ),
            )
        } else {
            match effect {
                ControlEffect::DiscoverHistory { agent_id, query } => {
                    self.discover(instance_id, generation, agent_id, query)
                }
                ControlEffect::LoadHistory {
                    agent_id,
                    candidate_id,
                } => self.load(instance_id, generation, agent_id, candidate_id),
                ControlEffect::AuthorizeResume {
                    agent_id,
                    target,
                    request,
                } => self.authorize_resume(instance_id, generation, agent_id, target, request),
                _ => {
                    history_failure("native authority worker received an invalid effect".to_owned())
                }
            }
        };
        ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(operation_id),
            instance_id,
            generation,
            observation,
        }
    }

    fn discover(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        agent_id: AgentId,
        query: gate4agent_types::HistoryQuery,
    ) -> ControlObservation {
        let Some(spec) = self.catalog.get(&agent_id).cloned() else {
            return history_failure(format!("agent '{agent_id}' is absent from native catalog"));
        };
        let request =
            match HistoryDiscoveryRequest::from_spec(&spec, query.working_directory, query.limit) {
                Ok(request) => request,
                Err(error) => return history_failure(error.to_string()),
            };
        let Some(authority) = self.authority.as_mut() else {
            return history_failure("native history authority is not configured".to_owned());
        };
        let candidates = match discover_history(authority, &request) {
            Ok(candidates) => candidates,
            Err(error) => return history_failure(error.to_string()),
        };
        let summaries = candidates
            .iter()
            .map(|candidate| HistoryCandidateSummary {
                id: candidate.id().as_str().to_owned(),
                session_id_hint: candidate.session_id_hint().to_owned(),
                modified_at_unix_ms: candidate.modified_at_unix_ms(),
            })
            .collect::<Vec<_>>();
        if summaries
            .iter()
            .any(|candidate| candidate.validate().is_err())
        {
            return history_failure(
                "native history authority returned an invalid candidate".to_owned(),
            );
        }
        let candidates = candidates
            .into_iter()
            .map(|candidate| (candidate.id().as_str().to_owned(), candidate))
            .collect();
        self.discoveries.insert(
            instance_id,
            InstanceHistoryDiscovery {
                generation,
                agent_id,
                request,
                candidates,
            },
        );
        self.touch_discovery(instance_id);
        ControlObservation::HistoryDiscovered {
            candidates: summaries,
        }
    }

    fn load(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        agent_id: AgentId,
        candidate_id: String,
    ) -> ControlObservation {
        let Some(discovery) = self.discoveries.get(&instance_id) else {
            return history_failure("history candidate discovery is expired".to_owned());
        };
        if discovery.generation != generation || discovery.agent_id != agent_id {
            return history_failure("history candidate generation is stale".to_owned());
        }
        let Some(candidate) = discovery.candidates.get(&candidate_id).cloned() else {
            return history_failure("history candidate is expired or unknown".to_owned());
        };
        let request = discovery.request.clone();
        self.touch_discovery(instance_id);
        let load = match HistoryLoadRequest::new(&request, candidate) {
            Ok(load) => load,
            Err(error) => return history_failure(error.to_string()),
        };
        let Some(authority) = self.authority.as_mut() else {
            return history_failure("native history authority is not configured".to_owned());
        };
        match load_history_session(authority, &load) {
            Ok(session) => {
                let session = history_session_record(session);
                if let Err(error) = session.validate() {
                    history_failure(error.to_string())
                } else {
                    ControlObservation::HistoryLoaded { session }
                }
            }
            Err(error) => history_failure(error.to_string()),
        }
    }

    fn authorize_resume(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        agent_id: AgentId,
        target: ResumeAuthorityTarget,
        request: ResumeLaunchRequest,
    ) -> ControlObservation {
        if let Err(error) = request.validate() {
            return resume_failure(error.to_string());
        }
        let Some(spec) = self.catalog.get(&agent_id).cloned() else {
            return resume_failure(format!("agent '{agent_id}' is absent from native catalog"));
        };

        let provider_session = match target {
            ResumeAuthorityTarget::ProviderSession { identity } => identity,
            ResumeAuthorityTarget::HistoryCandidate { candidate_id } => {
                let Some(discovery) = self.discoveries.get(&instance_id) else {
                    return resume_failure("history candidate discovery is expired".to_owned());
                };
                if discovery.generation != generation || discovery.agent_id != agent_id {
                    return resume_failure("history candidate generation is stale".to_owned());
                }
                let Some(candidate) = discovery.candidates.get(&candidate_id).cloned() else {
                    return resume_failure("history candidate is expired or unknown".to_owned());
                };
                let discovery_request = discovery.request.clone();
                self.touch_discovery(instance_id);
                let load = match HistoryLoadRequest::new(&discovery_request, candidate) {
                    Ok(load) => load,
                    Err(error) => return resume_failure(error.to_string()),
                };
                let Some(authority) = self.authority.as_mut() else {
                    return resume_failure("native history authority is not configured".to_owned());
                };
                let session = match load_history_session(authority, &load) {
                    Ok(session) => session,
                    Err(error) => return resume_failure(error.to_string()),
                };
                match authority.resume_provider_session(&load, session.session_id) {
                    Ok(identity) => identity,
                    Err(error) => return resume_failure(error.to_string()),
                }
            }
        };
        let resume_request = match ResumeRequest::from_provider_session(
            &spec,
            provider_session,
            Some(request.working_directory.clone()),
        ) {
            Ok(request) => request,
            Err(error) => return resume_failure(error.to_string()),
        };
        match prepare_resume(&mut ExplicitResumeAuthority, resume_request) {
            Ok(ResumeOutcome::Authorized(prepared)) => {
                if prepared.working_directory() != Some(request.working_directory.as_str()) {
                    return resume_failure(
                        "resume authority changed the requested working directory".to_owned(),
                    );
                }
                ControlObservation::ResumeAuthorized {
                    provider_session: prepared.provider_session().clone(),
                }
            }
            Ok(ResumeOutcome::Denied { reason }) => ControlObservation::ResumeDenied { reason },
            Err(error) => resume_failure(error.to_string()),
        }
    }

    fn touch_discovery(&mut self, instance_id: AgentInstanceId) {
        self.discovery_order
            .retain(|candidate| *candidate != instance_id);
        self.discovery_order.push_back(instance_id);
        while self.discoveries.len() > HISTORY_INSTANCE_DISCOVERIES_MAX {
            if let Some(expired) = self.discovery_order.pop_front() {
                self.discoveries.remove(&expired);
            }
        }
    }
}

async fn run_authority_worker(
    catalog: AgentRegistry,
    config: Option<NativeHistoryConfig>,
    mut effects: Receiver<NativeEffectRequest>,
    control_tx: Sender<ObservationEnvelope>,
) {
    let state = Arc::new(Mutex::new(NativeAuthorityWorkerState::new(catalog, config)));
    while let Some(request) = effects.recv().await {
        let fallback = request.effect.clone();
        let worker_state = Arc::clone(&state);
        let completion = match tokio::task::spawn_blocking(move || {
            worker_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .execute(request.effect)
        })
        .await
        {
            Ok(completion) => completion,
            Err(_) => effect_failure(fallback, "native authority worker task failed".to_owned()),
        };
        if control_tx.send(completion).await.is_err() {
            break;
        }
    }
}

async fn run_capability_worker(
    catalog: AgentRegistry,
    mut effects: Receiver<NativeEffectRequest>,
    control_tx: Sender<ObservationEnvelope>,
) {
    let mut authority = NativeCapabilityProbeAuthority::default();
    while let Some(request) = effects.recv().await {
        let EffectEnvelope {
            protocol_version,
            operation_id,
            instance_id,
            generation,
            effect,
        } = request.effect;
        let observation = if protocol_version != CONTROL_PROTOCOL_VERSION {
            ControlObservation::CapabilityProbeFailed {
                failure: CapabilityProbeFailure::AuthorityRejected,
            }
        } else if let ControlEffect::ProbeCapabilities { agent_id, request } = effect {
            if request.validate().is_err() {
                ControlObservation::CapabilityProbeFailed {
                    failure: CapabilityProbeFailure::AuthorityRejected,
                }
            } else if let Some(spec) = catalog.get(&agent_id) {
                match authority.probe(spec, &request.working_directory).await {
                    Ok(session_option_models) => ControlObservation::CapabilitiesProbed {
                        session_option_models,
                    },
                    Err(failure) => ControlObservation::CapabilityProbeFailed { failure },
                }
            } else {
                ControlObservation::CapabilityProbeFailed {
                    failure: CapabilityProbeFailure::AuthorityRejected,
                }
            }
        } else {
            ControlObservation::CapabilityProbeFailed {
                failure: CapabilityProbeFailure::AuthorityRejected,
            }
        };
        if control_tx
            .send(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(operation_id),
                instance_id,
                generation,
                observation,
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

fn history_failure(message: String) -> ControlObservation {
    ControlObservation::HistoryFailed { message }
}

fn resume_failure(message: String) -> ControlObservation {
    ControlObservation::ResumeFailed { message }
}

fn authority_failure(is_resume: bool, message: String) -> ControlObservation {
    if is_resume {
        resume_failure(message)
    } else {
        history_failure(message)
    }
}

struct ExplicitResumeAuthority;

impl ResumeAuthority for ExplicitResumeAuthority {
    type Error = Infallible;

    fn authorize(
        &mut self,
        _prepared: &PreparedResume,
    ) -> Result<ResumeAuthorityDecision, Self::Error> {
        Ok(ResumeAuthorityDecision::Authorized)
    }
}

fn history_session_record(session: gate4agent_adapters::HistorySession) -> HistorySessionRecord {
    HistorySessionRecord {
        session_id: session.session_id,
        title: session.title,
        cwd: session.cwd,
        model: session.model,
        message_count: session.message_count,
        total_tokens: session.total_tokens,
        messages: session
            .messages
            .into_iter()
            .map(|message| HistoryMessageRecord {
                role: match message.role {
                    gate4agent_adapters::HistoryRole::User => HistoryMessageRole::User,
                    gate4agent_adapters::HistoryRole::Assistant => HistoryMessageRole::Assistant,
                },
                text: message.text,
            })
            .collect(),
    }
}

async fn run_effect_worker(
    catalog: AgentRegistry,
    mut effects: Receiver<NativeEffectRequest>,
    context: NativeWorkerContext,
) {
    let mut shell = NativeEffectShell::new(catalog);
    let mut interval = tokio::time::interval(context.poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_effect = Instant::now();

    loop {
        tokio::select! {
            request = effects.recv() => {
                let Some(request) = request else {
                    break;
                };
                last_effect = Instant::now();
                let before = shell.active_session_count();
                let completion = shell
                    .execute_with_pty_env(request.effect, request.pty_env)
                    .await;
                update_active_count(&context.active_sessions, before, shell.active_session_count());
                remove_hook_route_for_observation(&context.hook_ingress, &completion);
                if closes_terminal_session(&completion.observation) {
                    context
                        .terminal_frames
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&(completion.instance_id, completion.generation));
                }
                if context.control_tx.send(completion).await.is_err() {
                    break;
                }
                if !publish_shell_observations(&mut shell, &context).await {
                    break;
                }
            }
            _ = interval.tick() => {
                if !publish_shell_observations(&mut shell, &context).await {
                    break;
                }
                if shell.active_session_count() == 0
                    && last_effect.elapsed() >= context.idle_timeout
                {
                    break;
                }
            }
        }
    }

    let remaining = shell.active_session_count();
    if remaining > 0 {
        context
            .active_sessions
            .fetch_sub(remaining, Ordering::AcqRel);
    }
}

async fn publish_shell_observations(
    shell: &mut NativeEffectShell,
    context: &NativeWorkerContext,
) -> bool {
    for observation in shell.collect_provider_events() {
        if context.control_tx.send(observation).await.is_err() {
            return false;
        }
    }

    for observation in shell.collect_terminal_frames() {
        if matches!(
            &observation.observation,
            ControlObservation::TerminalFrame { .. }
        ) {
            let key = (observation.instance_id, observation.generation);
            context
                .terminal_frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, observation);
        } else if context.control_tx.send(observation).await.is_err() {
            return false;
        }
    }

    let before = shell.active_session_count();
    for observation in shell.collect_exits().await {
        remove_hook_route_for_observation(&context.hook_ingress, &observation);
        context
            .terminal_frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(observation.instance_id, observation.generation));
        if context.control_tx.send(observation).await.is_err() {
            return false;
        }
    }
    update_active_count(
        &context.active_sessions,
        before,
        shell.active_session_count(),
    );
    true
}

fn remove_hook_route_for_observation(
    hook_ingress: &Arc<RwLock<Option<HookIngressControl>>>,
    observation: &ObservationEnvelope,
) {
    if !matches!(
        observation.observation,
        ControlObservation::SpawnFailed { .. }
            | ControlObservation::StopCompleted { .. }
            | ControlObservation::ProcessExited { .. }
    ) {
        return;
    }
    if let Some(control) = hook_ingress
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        control.remove_route(observation.instance_id, observation.generation);
    }
}

fn closes_terminal_session(observation: &ControlObservation) -> bool {
    matches!(
        observation,
        ControlObservation::StopCompleted { .. }
            | ControlObservation::StopFailed { .. }
            | ControlObservation::ProcessExited { .. }
    )
}

fn update_active_count(counter: &AtomicUsize, before: usize, after: usize) {
    if after > before {
        counter.fetch_add(after - before, Ordering::AcqRel);
    } else if before > after {
        counter.fetch_sub(before - after, Ordering::AcqRel);
    }
}

fn effect_failure(effect: EffectEnvelope, message: String) -> ObservationEnvelope {
    let observation = match effect.effect {
        ControlEffect::Spawn { .. } | ControlEffect::SpawnResume { .. } => {
            ControlObservation::SpawnFailed { message }
        }
        ControlEffect::Stop { .. } => ControlObservation::StopFailed { message },
        ControlEffect::WriteInput { .. } => ControlObservation::InputFailed { message },
        ControlEffect::SubmitPrompt { .. } | ControlEffect::Interrupt => {
            ControlObservation::InputFailed { message }
        }
        ControlEffect::ResolveInteraction { target, .. } => {
            ControlObservation::InteractionResolutionFailed {
                interaction_id: target.interaction_id,
                message,
            }
        }
        ControlEffect::Resize { .. } => ControlObservation::ResizeFailed { message },
        ControlEffect::ObserveForeground => ControlObservation::ForegroundFailed { message },
        ControlEffect::ProbeCapabilities { .. } => ControlObservation::CapabilityProbeFailed {
            failure: CapabilityProbeFailure::ExecutorUnavailable,
        },
        ControlEffect::DiscoverHistory { .. } | ControlEffect::LoadHistory { .. } => {
            ControlObservation::HistoryFailed { message }
        }
        ControlEffect::AuthorizeResume { .. } => ControlObservation::ResumeFailed { message },
    };
    ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: Some(effect.operation_id),
        instance_id: effect.instance_id,
        generation: effect.generation,
        observation,
    }
}
