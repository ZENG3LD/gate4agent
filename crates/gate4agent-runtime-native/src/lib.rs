//! Tick-driven native runtime for embedding gate4agent in an owning app core.

use gate4agent_catalog::AgentRegistry;
use gate4agent_handle::{bounded_port, Gate4AgentHandle, KernelPort, PublishReport};
use gate4agent_kernel::{CommandOutcome, Gate4AgentKernel};
use gate4agent_shell_native::NativeEffectShell;
use gate4agent_types::{
    AgentInstanceId, ControlEffect, ControlObservation, EffectEnvelope, ObservationEnvelope,
    SessionGeneration, CONTROL_PROTOCOL_VERSION,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
    port: KernelPort,
    effects: NativeEffectDispatcher,
}

impl NativeRuntime {
    pub fn new(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
    ) -> (Gate4AgentHandle, Self) {
        let (handle, port) = bounded_port(config.command_capacity);
        let runtime = Self {
            config,
            kernel: Gate4AgentKernel::new(catalog.clone()),
            port,
            effects: NativeEffectDispatcher::new(catalog, config),
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
}

struct EffectWorker {
    sender: Sender<EffectEnvelope>,
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
        }
    }

    fn dispatch(&mut self, effect: EffectEnvelope) {
        self.workers.retain(|_, worker| !worker.sender.is_closed());
        let instance_id = effect.instance_id;
        let mut pending = effect;
        for _ in 0..2 {
            let sender = self.worker_sender(instance_id);
            match sender.try_send(pending) {
                Ok(()) => return,
                Err(TrySendError::Closed(effect)) => {
                    self.workers.remove(&instance_id);
                    pending = effect;
                }
                Err(TrySendError::Full(effect)) => {
                    self.pending_failures.push_back(effect_failure(
                        effect,
                        "native session effect queue is full".to_owned(),
                    ));
                    return;
                }
            }
        }
        self.pending_failures.push_back(effect_failure(
            pending,
            "native session effect worker is unavailable".to_owned(),
        ));
    }

    fn worker_sender(&mut self, instance_id: AgentInstanceId) -> Sender<EffectEnvelope> {
        if let Some(worker) = self.workers.get(&instance_id) {
            return worker.sender.clone();
        }
        let (sender, receiver) = mpsc::channel(self.config.effect_capacity_per_session.max(1));
        tokio::spawn(run_effect_worker(
            self.catalog.clone(),
            receiver,
            self.control_tx.clone(),
            Arc::clone(&self.terminal_frames),
            Arc::clone(&self.active_sessions),
            Duration::from_millis(self.config.worker_poll_interval_ms.max(1)),
            Duration::from_millis(self.config.worker_idle_timeout_ms.max(1)),
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
    mut effects: Receiver<EffectEnvelope>,
    control_tx: Sender<ObservationEnvelope>,
    terminal_frames: Arc<Mutex<BTreeMap<TerminalFrameKey, ObservationEnvelope>>>,
    active_sessions: Arc<AtomicUsize>,
    poll_interval: Duration,
    idle_timeout: Duration,
) {
    let mut shell = NativeEffectShell::new(catalog);
    let mut interval = tokio::time::interval(poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_effect = Instant::now();

    loop {
        tokio::select! {
            effect = effects.recv() => {
                let Some(effect) = effect else {
                    break;
                };
                last_effect = Instant::now();
                let before = shell.active_session_count();
                let completion = shell.execute(effect).await;
                update_active_count(&active_sessions, before, shell.active_session_count());
                if closes_terminal_session(&completion.observation) {
                    terminal_frames
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&(completion.instance_id, completion.generation));
                }
                if control_tx.send(completion).await.is_err() {
                    break;
                }
                if !publish_shell_observations(
                    &mut shell,
                    &control_tx,
                    &terminal_frames,
                    &active_sessions,
                )
                .await
                {
                    break;
                }
            }
            _ = interval.tick() => {
                if !publish_shell_observations(
                    &mut shell,
                    &control_tx,
                    &terminal_frames,
                    &active_sessions,
                )
                .await
                {
                    break;
                }
                if shell.active_session_count() == 0 && last_effect.elapsed() >= idle_timeout {
                    break;
                }
            }
        }
    }

    let remaining = shell.active_session_count();
    if remaining > 0 {
        active_sessions.fetch_sub(remaining, Ordering::AcqRel);
    }
}

async fn publish_shell_observations(
    shell: &mut NativeEffectShell,
    control_tx: &Sender<ObservationEnvelope>,
    terminal_frames: &Arc<Mutex<BTreeMap<TerminalFrameKey, ObservationEnvelope>>>,
    active_sessions: &Arc<AtomicUsize>,
) -> bool {
    for observation in shell.collect_provider_events() {
        if control_tx.send(observation).await.is_err() {
            return false;
        }
    }

    for observation in shell.collect_terminal_frames() {
        if matches!(
            &observation.observation,
            ControlObservation::TerminalFrame { .. }
        ) {
            let key = (observation.instance_id, observation.generation);
            terminal_frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, observation);
        } else if control_tx.send(observation).await.is_err() {
            return false;
        }
    }

    let before = shell.active_session_count();
    for observation in shell.collect_exits().await {
        terminal_frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(observation.instance_id, observation.generation));
        if control_tx.send(observation).await.is_err() {
            return false;
        }
    }
    update_active_count(active_sessions, before, shell.active_session_count());
    true
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
    };
    ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: Some(effect.operation_id),
        instance_id: effect.instance_id,
        generation: effect.generation,
        observation,
    }
}
