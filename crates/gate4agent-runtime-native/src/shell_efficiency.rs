//! Folds `gate4agent_shell_native::ShellEfficiencyFacts` -- plain facts a
//! `NativeEffectShell` accumulates inside `collect_terminal_frames` and
//! `reclassify_foreground` -- into the same "distribution, not an average"
//! contract `tick_profile` uses for `NativeRuntime::tick`'s own phases.
//!
//! See `ShellEfficiencyFacts`'s own doc comment for why the split is: that
//! crate reports plain facts, this crate keeps statistics. It is the
//! dependency direction (`gate4agent-shell-native` does not, and must not,
//! depend on `gate4agent-runtime-native`) that makes this crate, not that
//! one, the only place a fact can become a percentile -- do not "fix" that
//! by moving `RingStats`/`Distribution` downstream or re-deriving a second
//! copy of them in `gate4agent-shell-native`.
//!
//! `run_effect_worker`'s worker-loop task calls both shell methods above
//! once per iteration and immediately drains their facts (see
//! `publish_shell_observations`), folding them into the
//! `Arc<Mutex<ShellEfficiencyProfile>>` that every `NativeWorkerContext`
//! shares with `NativeEffectDispatcher` -- one worker per `AgentInstanceId`,
//! all folding into the same shared profile. `NativeRuntime::
//! shell_efficiency_snapshot` is the only reader, called once per
//! drive-loop iteration by `gate4agent-node`, same cadence as
//! `NativeRuntime::tick_profile_snapshot`.
use crate::tick_profile::{duration_micros, Distribution, RingStats, SAMPLE_WINDOW};
use gate4agent_shell_native::ShellEfficiencyFacts;

/// One snapshot of every shell-efficiency series -- what
/// [`crate::NativeRuntime::shell_efficiency_snapshot`] returns, and what
/// `GET /metrics` renders alongside `TickProfileSnapshot`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShellEfficiencyProfileSnapshot {
    /// Duration of real (sequence-gate-passed) `terminal_state()` calls --
    /// backlog item 3's "what a changed-screen capture actually costs".
    pub terminal_state_us: Distribution,
    /// Lifetime count of those real captures.
    pub terminal_state_captures_total: u64,
    /// Lifetime count of sessions the cheap sequence gate skipped instead
    /// of paying for a real capture.
    pub terminal_state_skips_total: u64,
    /// Byte size of every published `TerminalFrame` -- backlog item 4's
    /// "what a frame costs on the wire".
    pub terminal_frame_bytes: Distribution,
    /// Lifetime count of frames published.
    pub terminal_frames_published_total: u64,
    /// Lifetime total bytes across every frame published.
    pub terminal_frame_bytes_total: u64,
    /// The three components of `reclassify_foreground`'s OS process-tree
    /// probes -- see `gate4agent::pty::ForegroundProbeTiming` for what each
    /// one means. Kept as three independent series rather than one combined
    /// duration: a total mixing pure `spawn_blocking`-queue waiting with
    /// the mutex-contention wait and the actual process-table walk's CPU
    /// leads to opposite conclusions depending on which of the three
    /// dominates, so collapsing them back into one number would throw away
    /// exactly what this split exists to preserve.
    pub foreground_probe_queued_us: Distribution,
    pub foreground_probe_lock_wait_us: Distribution,
    pub foreground_probe_walk_us: Distribution,
    /// Lifetime count of probes, not components -- one probe increments
    /// this by one regardless of how many of the three series above it
    /// also fed.
    pub foreground_probes_total: u64,
}

/// Owns the shell-efficiency rings and lifetime counters folded from every
/// per-instance worker's drained `ShellEfficiencyFacts`. Lives behind an
/// `Arc<Mutex<_>>` shared by `NativeEffectDispatcher` and every
/// `NativeWorkerContext` it spawns, since multiple worker tasks fold into
/// it concurrently -- locked only for one `fold`, never across an
/// `.await`, same rule as `gate4agent-node`'s `DriveLoopProfiler`.
#[derive(Clone, Debug, Default)]
pub struct ShellEfficiencyProfile {
    terminal_state_us: RingStats<SAMPLE_WINDOW>,
    terminal_state_captures_total: u64,
    terminal_state_skips_total: u64,
    terminal_frame_bytes: RingStats<SAMPLE_WINDOW>,
    terminal_frames_published_total: u64,
    terminal_frame_bytes_total: u64,
    foreground_probe_queued_us: RingStats<SAMPLE_WINDOW>,
    foreground_probe_lock_wait_us: RingStats<SAMPLE_WINDOW>,
    foreground_probe_walk_us: RingStats<SAMPLE_WINDOW>,
    foreground_probes_total: u64,
}

impl ShellEfficiencyProfile {
    /// Fold one drained `ShellEfficiencyFacts` in -- every raw duration/byte
    /// sample becomes a `RingStats::push`, every counter adds with
    /// saturating arithmetic so a lifetime `u64` total can never wrap.
    pub fn fold(&mut self, facts: &ShellEfficiencyFacts) {
        for &sample in facts.terminal_state_samples() {
            self.terminal_state_us.push(duration_micros(sample));
        }
        self.terminal_state_captures_total = self
            .terminal_state_captures_total
            .saturating_add(facts.terminal_state_captures());
        self.terminal_state_skips_total = self
            .terminal_state_skips_total
            .saturating_add(facts.terminal_state_skips());

        for &bytes in facts.terminal_frame_byte_samples() {
            self.terminal_frame_bytes.push(bytes.min(u64::from(u32::MAX)) as u32);
        }
        self.terminal_frames_published_total = self
            .terminal_frames_published_total
            .saturating_add(facts.terminal_frames_published());
        self.terminal_frame_bytes_total = self
            .terminal_frame_bytes_total
            .saturating_add(facts.terminal_frame_bytes_total());

        for &sample in facts.foreground_probe_queued_samples() {
            self.foreground_probe_queued_us.push(duration_micros(sample));
        }
        for &sample in facts.foreground_probe_lock_wait_samples() {
            self.foreground_probe_lock_wait_us.push(duration_micros(sample));
        }
        for &sample in facts.foreground_probe_walk_samples() {
            self.foreground_probe_walk_us.push(duration_micros(sample));
        }
        self.foreground_probes_total = self
            .foreground_probes_total
            .saturating_add(facts.foreground_probes());
    }

    pub fn snapshot(&self) -> ShellEfficiencyProfileSnapshot {
        ShellEfficiencyProfileSnapshot {
            terminal_state_us: self.terminal_state_us.stats(),
            terminal_state_captures_total: self.terminal_state_captures_total,
            terminal_state_skips_total: self.terminal_state_skips_total,
            terminal_frame_bytes: self.terminal_frame_bytes.stats(),
            terminal_frames_published_total: self.terminal_frames_published_total,
            terminal_frame_bytes_total: self.terminal_frame_bytes_total,
            foreground_probe_queued_us: self.foreground_probe_queued_us.stats(),
            foreground_probe_lock_wait_us: self.foreground_probe_lock_wait_us.stats(),
            foreground_probe_walk_us: self.foreground_probe_walk_us.stats(),
            foreground_probes_total: self.foreground_probes_total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_shell_native::ForegroundProbeTiming;
    use std::time::Duration;

    #[test]
    fn folding_a_drained_fact_set_updates_distributions_and_lifetime_counters() {
        let mut facts = ShellEfficiencyFacts::default();
        facts.record_terminal_state_capture(Duration::from_micros(1_500));
        facts.record_terminal_state_skip();
        facts.record_terminal_frame_published(4_096);
        facts.record_foreground_probe(ForegroundProbeTiming {
            queued: Duration::from_micros(2_500),
            lock_wait: Duration::from_micros(700),
            walk: Duration::from_micros(300),
        });
        let drained = facts.take();

        let mut profile = ShellEfficiencyProfile::default();
        profile.fold(&drained);
        let snapshot = profile.snapshot();

        assert_eq!(snapshot.terminal_state_us.max, 1_500);
        assert_eq!(snapshot.terminal_state_captures_total, 1);
        assert_eq!(snapshot.terminal_state_skips_total, 1);
        assert_eq!(snapshot.terminal_frame_bytes.max, 4_096);
        assert_eq!(snapshot.terminal_frames_published_total, 1);
        assert_eq!(snapshot.terminal_frame_bytes_total, 4_096);
        // Distinct values per component -- a copy-paste that recorded the
        // same duration into all three series would pass an assertion that
        // only checked one of them.
        assert_eq!(snapshot.foreground_probe_queued_us.max, 2_500);
        assert_eq!(snapshot.foreground_probe_lock_wait_us.max, 700);
        assert_eq!(snapshot.foreground_probe_walk_us.max, 300);
        assert_eq!(snapshot.foreground_probes_total, 1);
    }

    #[test]
    fn one_probe_increments_the_lifetime_count_by_one_not_by_the_component_count() {
        let mut facts = ShellEfficiencyFacts::default();
        facts.record_foreground_probe(ForegroundProbeTiming {
            queued: Duration::from_micros(10),
            lock_wait: Duration::from_micros(20),
            walk: Duration::from_micros(30),
        });
        let drained = facts.take();

        let mut profile = ShellEfficiencyProfile::default();
        profile.fold(&drained);
        let snapshot = profile.snapshot();

        assert_eq!(snapshot.foreground_probes_total, 1);
        assert_eq!(snapshot.foreground_probe_queued_us.count, 1);
        assert_eq!(snapshot.foreground_probe_lock_wait_us.count, 1);
        assert_eq!(snapshot.foreground_probe_walk_us.count, 1);
    }

    #[test]
    fn folding_an_empty_fact_set_changes_nothing() {
        let mut profile = ShellEfficiencyProfile::default();
        profile.fold(&ShellEfficiencyFacts::default());
        assert_eq!(profile.snapshot(), ShellEfficiencyProfileSnapshot::default());
    }
}
