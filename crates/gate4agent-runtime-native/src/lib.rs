//! Tick-driven native runtime for embedding gate4agent in an owning app core.

use gate4agent_catalog::AgentRegistry;
use gate4agent_handle::{bounded_port, Gate4AgentHandle, KernelPort, PublishReport};
use gate4agent_kernel::{CommandOutcome, Gate4AgentKernel};
use gate4agent_shell_native::NativeEffectShell;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeRuntimeConfig {
    pub command_capacity: usize,
    pub max_commands_per_tick: usize,
}

impl Default for NativeRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 256,
            max_commands_per_tick: 64,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRuntimeTick {
    pub command_outcomes: Vec<CommandOutcome>,
    pub effects_executed: usize,
    pub natural_exits_collected: usize,
    pub terminal_frames_collected: usize,
    pub snapshot_revision: u64,
    pub publish_report: PublishReport,
}

/// Owns the kernel tick and all native effect handles. Product apps retain
/// only the bounded handle returned by [`NativeRuntime::new`].
pub struct NativeRuntime {
    config: NativeRuntimeConfig,
    kernel: Gate4AgentKernel,
    port: KernelPort,
    shell: NativeEffectShell,
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
            shell: NativeEffectShell::new(catalog),
        };
        (handle, runtime)
    }

    /// Run one deterministic host tick:
    /// commands -> natural observations -> effects -> completions -> publish.
    pub async fn tick(&mut self) -> NativeRuntimeTick {
        let mut incoming_observations = self.shell.collect_terminal_frames();
        let mut terminal_frames_collected = incoming_observations.len();
        let natural_exits = self.shell.collect_exits().await;
        let natural_exits_collected = natural_exits.len();
        incoming_observations.extend(natural_exits);
        let commands = self
            .port
            .drain_commands(self.config.max_commands_per_tick.max(1));
        let first = self.kernel.step(commands, incoming_observations);
        let effects_executed = first.effects.len();
        let mut events = first.events;
        let mut completions = Vec::with_capacity(effects_executed);
        for effect in first.effects {
            completions.push(self.shell.execute(effect).await);
        }

        let settled = self.kernel.step([], completions);
        debug_assert!(settled.effects.is_empty());
        events.extend(settled.events);
        let terminal_frames = self.shell.collect_terminal_frames();
        terminal_frames_collected += terminal_frames.len();
        let published = self.kernel.step([], terminal_frames);
        debug_assert!(published.effects.is_empty());
        events.extend(published.events);
        let snapshot_revision = published.snapshot.revision;
        self.port.publish_snapshot(published.snapshot);
        let publish_report = self.port.publish_events(events);

        NativeRuntimeTick {
            command_outcomes: first.command_outcomes,
            effects_executed,
            natural_exits_collected,
            terminal_frames_collected,
            snapshot_revision,
            publish_report,
        }
    }

    pub fn active_native_sessions(&self) -> usize {
        self.shell.active_session_count()
    }
}
