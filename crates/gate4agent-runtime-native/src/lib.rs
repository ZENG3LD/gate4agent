//! Tick-driven native runtime for embedding gate4agent in an owning app core.

use gate4agent_catalog::{AgentRegistry, EnvMutation};
use gate4agent_handle::{bounded_port, Gate4AgentHandle, KernelPort, PublishReport};
use gate4agent_kernel::{CommandOutcome, Gate4AgentKernel};
pub use gate4agent_shell_hooks::{HookIngressConfig, HookIngressEndpoint};
use gate4agent_shell_hooks::{HookIngressControl, HookIngressServer, HookIngressStartError};
use gate4agent_shell_native::NativeEffectShell;
use gate4agent_types::{
    AgentInstanceId, ControlEffect, ControlObservation, EffectEnvelope, ObservationEnvelope,
    SessionGeneration, SessionStatus, CONTROL_PROTOCOL_VERSION,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};
use tokio::time::MissedTickBehavior;

type TerminalFrameKey = (AgentInstanceId, SessionGeneration);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeRuntimeConfig {
    pub command_capacity: usize,
    pub max_commands_per_tick: usize,
    pub effect_capacity_per_session: usize,
    pub observation_capacity: usize,
    pub max_observations_per_tick: usize,
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
    port: KernelPort,
    effects: NativeEffectDispatcher,
    hook_ingress: Option<HookIngressServer>,
}

impl NativeRuntime {
    pub fn new(catalog: AgentRegistry, config: NativeRuntimeConfig) -> (Gate4AgentHandle, Self) {
        let (handle, port) = bounded_port(config.command_capacity);
        let runtime = Self {
            config,
            kernel: Gate4AgentKernel::new(catalog.clone()),
            handle: handle.clone(),
            port,
            effects: NativeEffectDispatcher::new(catalog, config),
            hook_ingress: None,
        };
        (handle, runtime)
    }

    /// Run one non-blocking host tick. Effects are dispatched in session order
    /// to per-instance workers; their observations enter later ticks.
    pub async fn tick(&mut self) -> NativeRuntimeTick {
        let (observations, terminal_frames_collected) = self
            .effects
            .drain_observations(self.config.max_observations_per_tick.max(1));
        let observations_applied = observations.len();
        let commands = self
            .port
            .drain_commands(self.config.max_commands_per_tick.max(1));
        let step = self.kernel.step(commands, observations);
        let effects_dispatched = step.effects.len();
        for effect in step.effects {
            self.effects.dispatch(effect);
        }

        self.port.publish_snapshot(step.snapshot.clone());
        let snapshot_revision = step.snapshot.revision;
        let publish_report = self.port.publish_events(step.events);

        NativeRuntimeTick {
            command_outcomes: step.command_outcomes,
            effects_dispatched,
            observations_applied,
            terminal_frames_collected,
            snapshot_revision,
            publish_report,
        }
    }

    pub fn active_native_sessions(&self) -> usize {
        self.effects.active_sessions.load(Ordering::Acquire)
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
    workers: HashMap<AgentInstanceId, EffectWorker>,
    control_tx: Sender<ObservationEnvelope>,
    control_rx: Receiver<ObservationEnvelope>,
    pending_failures: VecDeque<ObservationEnvelope>,
    terminal_frames: Arc<Mutex<BTreeMap<TerminalFrameKey, ObservationEnvelope>>>,
    active_sessions: Arc<AtomicUsize>,
    hook_ingress: Arc<RwLock<Option<HookIngressControl>>>,
}

impl NativeEffectDispatcher {
    fn new(catalog: AgentRegistry, config: NativeRuntimeConfig) -> Self {
        let (control_tx, control_rx) = mpsc::channel(config.observation_capacity.max(1));
        Self {
            catalog,
            config,
            workers: HashMap::new(),
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
        self.workers.retain(|_, worker| !worker.sender.is_closed());
        let instance_id = effect.instance_id;
        let pty_env = match self.hook_pty_env(&effect) {
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

    fn hook_pty_env(&self, effect: &EffectEnvelope) -> Result<Vec<EnvMutation>, String> {
        let ControlEffect::Spawn {
            agent_id,
            transport: gate4agent_types::TransportKind::Pty,
            ..
        } = &effect.effect
        else {
            return Ok(Vec::new());
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

    fn remove_hook_route(&self, effect: &EffectEnvelope) {
        if !matches!(effect.effect, ControlEffect::Spawn { .. }) {
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
        ControlEffect::Spawn { .. } => ControlObservation::SpawnFailed { message },
        ControlEffect::Stop { .. } => ControlObservation::StopFailed { message },
        ControlEffect::WriteInput { .. } => ControlObservation::InputFailed { message },
        ControlEffect::SubmitPrompt { .. } | ControlEffect::Interrupt => {
            ControlObservation::InputFailed { message }
        }
        ControlEffect::Resize { .. } => ControlObservation::ResizeFailed { message },
        ControlEffect::ObserveForeground => ControlObservation::ForegroundFailed { message },
    };
    ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: Some(effect.operation_id),
        instance_id: effect.instance_id,
        generation: effect.generation,
        observation,
    }
}
