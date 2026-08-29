//! Plain-data efficiency facts `NativeEffectShell` collects from real
//! (non-gated) work inside `collect_terminal_frames` and
//! `reclassify_foreground`.
//!
//! `gate4agent-shell-native` does not depend on `gate4agent-runtime-native`
//! (see this crate's own `Cargo.toml`), so it cannot hold that crate's
//! `RingStats`/`Distribution` types from `tick_profile`, and must not
//! re-derive a second copy of that percentile machinery here either --
//! that would be two independent sources of truth for the same math. The
//! split this crate honours instead: THIS crate reports plain facts (`u64`
//! counters, raw `Duration`/byte samples, no percentile computed anywhere
//! in this module); `gate4agent-runtime-native`'s worker loop, which
//! already calls both `collect_terminal_frames` and `reclassify_foreground`
//! once per iteration, is the only place on this side of the dependency
//! edge that can turn a fact into a distribution. Do not "fix" this by
//! importing `tick_profile` here -- there is no dependency edge for that
//! import to take, and adding one would make this crate depend on its own
//! downstream consumer.
use std::time::Duration;

use gate4agent::pty::ForegroundProbeTiming;

/// Upper bound on how many raw timing/byte samples one metric inside
/// [`ShellEfficiencyFacts`] can hold between drains.
///
/// A `NativeEffectShell` lives inside exactly one `AgentInstanceId`'s
/// worker task (see `gate4agent-runtime-native::run_effect_worker`), so its
/// `pty_sessions` map only ever holds THAT instance's own generations --
/// one in the steady state, briefly two during a resume race. The caller
/// drains this struct (`take`) every time it calls
/// `collect_terminal_frames`/`reclassify_foreground`, so the bound only
/// needs to cover one such call's worth of samples, never a shell's whole
/// lifetime -- it is sized generously above that realistic count for that
/// reason. If it is ever exceeded, the excess sample is silently dropped
/// from the array (the eventual distribution loses that one data point)
/// while every `u64` counter below -- which does not read from the array
/// -- stays exact regardless.
const MAX_EFFICIENCY_SAMPLES: usize = 32;

/// What [`crate::NativeEffectShell::collect_terminal_frames`] and
/// `::reclassify_foreground` have done since the last [`Self::take`],
/// handed to the caller and reset by it. See the module doc for why this
/// carries plain facts rather than a computed distribution.
#[derive(Debug)]
pub struct ShellEfficiencyFacts {
    terminal_state_samples: [Duration; MAX_EFFICIENCY_SAMPLES],
    terminal_state_sample_count: usize,
    /// Count of real `terminal_state()` calls since the last drain -- every
    /// call that passed the cheap `terminal_sequence()` gate, regardless of
    /// what its result turned out to be.
    terminal_state_captures: u64,
    /// Count of sessions the cheap `terminal_sequence()` gate skipped
    /// before ever calling `terminal_state()`, since the last drain --
    /// counts skip EVENTS (one per gated iteration of the session loop),
    /// not distinct sessions, so a session skipped on every call for a
    /// whole drain window is counted once per call, not once total.
    terminal_state_skips: u64,
    terminal_frame_byte_samples: [u64; MAX_EFFICIENCY_SAMPLES],
    terminal_frame_byte_sample_count: usize,
    terminal_frames_published: u64,
    terminal_frame_bytes_total: u64,
    /// The three components of one `observe_foreground_timed()` call --
    /// see [`ForegroundProbeTiming`] for what each one means and why a
    /// single combined duration is not interpretable. Kept as three
    /// independent series, not folded into one, for the same reason: a
    /// reader can add three numbers, but a fourth series that must equal
    /// their sum is a thing that can silently disagree with them.
    foreground_probe_queued_samples: [Duration; MAX_EFFICIENCY_SAMPLES],
    foreground_probe_queued_sample_count: usize,
    foreground_probe_lock_wait_samples: [Duration; MAX_EFFICIENCY_SAMPLES],
    foreground_probe_lock_wait_sample_count: usize,
    foreground_probe_walk_samples: [Duration; MAX_EFFICIENCY_SAMPLES],
    foreground_probe_walk_sample_count: usize,
    /// Count of probes, not components -- one `record_foreground_probe`
    /// call increments this by one regardless of how many of the three
    /// component arrays above it also wrote into.
    foreground_probes: u64,
}

impl Default for ShellEfficiencyFacts {
    fn default() -> Self {
        Self {
            terminal_state_samples: [Duration::ZERO; MAX_EFFICIENCY_SAMPLES],
            terminal_state_sample_count: 0,
            terminal_state_captures: 0,
            terminal_state_skips: 0,
            terminal_frame_byte_samples: [0; MAX_EFFICIENCY_SAMPLES],
            terminal_frame_byte_sample_count: 0,
            terminal_frames_published: 0,
            terminal_frame_bytes_total: 0,
            foreground_probe_queued_samples: [Duration::ZERO; MAX_EFFICIENCY_SAMPLES],
            foreground_probe_queued_sample_count: 0,
            foreground_probe_lock_wait_samples: [Duration::ZERO; MAX_EFFICIENCY_SAMPLES],
            foreground_probe_lock_wait_sample_count: 0,
            foreground_probe_walk_samples: [Duration::ZERO; MAX_EFFICIENCY_SAMPLES],
            foreground_probe_walk_sample_count: 0,
            foreground_probes: 0,
        }
    }
}

impl ShellEfficiencyFacts {
    /// Record one real `terminal_state()` call -- the sequence gate already
    /// let it through, so `elapsed` is the actual render-and-clone cost the
    /// module doc on `collect_terminal_frames` describes.
    pub fn record_terminal_state_capture(&mut self, elapsed: Duration) {
        self.terminal_state_captures = self.terminal_state_captures.saturating_add(1);
        if self.terminal_state_sample_count < MAX_EFFICIENCY_SAMPLES {
            self.terminal_state_samples[self.terminal_state_sample_count] = elapsed;
            self.terminal_state_sample_count += 1;
        }
    }

    /// Record one session the cheap sequence gate skipped this call.
    pub fn record_terminal_state_skip(&mut self) {
        self.terminal_state_skips = self.terminal_state_skips.saturating_add(1);
    }

    /// Record one published `TerminalFrame`'s wire-relevant byte size.
    pub fn record_terminal_frame_published(&mut self, bytes: u64) {
        self.terminal_frames_published = self.terminal_frames_published.saturating_add(1);
        self.terminal_frame_bytes_total = self.terminal_frame_bytes_total.saturating_add(bytes);
        if self.terminal_frame_byte_sample_count < MAX_EFFICIENCY_SAMPLES {
            self.terminal_frame_byte_samples[self.terminal_frame_byte_sample_count] = bytes;
            self.terminal_frame_byte_sample_count += 1;
        }
    }

    /// Record one `observe_foreground_timed()` OS process-tree probe,
    /// successful or not -- the walk was paid for either way. Increments
    /// `foreground_probes` by exactly one call, and records each of the
    /// three timing components into its own series -- see the field docs
    /// above for why they stay separate.
    pub fn record_foreground_probe(&mut self, timing: ForegroundProbeTiming) {
        self.foreground_probes = self.foreground_probes.saturating_add(1);
        if self.foreground_probe_queued_sample_count < MAX_EFFICIENCY_SAMPLES {
            self.foreground_probe_queued_samples[self.foreground_probe_queued_sample_count] =
                timing.queued;
            self.foreground_probe_queued_sample_count += 1;
        }
        if self.foreground_probe_lock_wait_sample_count < MAX_EFFICIENCY_SAMPLES {
            self.foreground_probe_lock_wait_samples[self.foreground_probe_lock_wait_sample_count] =
                timing.lock_wait;
            self.foreground_probe_lock_wait_sample_count += 1;
        }
        if self.foreground_probe_walk_sample_count < MAX_EFFICIENCY_SAMPLES {
            self.foreground_probe_walk_samples[self.foreground_probe_walk_sample_count] =
                timing.walk;
            self.foreground_probe_walk_sample_count += 1;
        }
    }

    /// Hand the caller everything recorded since the last `take`, and reset
    /// this struct back to empty -- see the module doc for why
    /// `gate4agent-runtime-native` is the only place that can do anything
    /// with what this returns.
    pub fn take(&mut self) -> Self {
        std::mem::take(self)
    }

    pub fn terminal_state_samples(&self) -> &[Duration] {
        &self.terminal_state_samples[..self.terminal_state_sample_count]
    }

    pub fn terminal_state_captures(&self) -> u64 {
        self.terminal_state_captures
    }

    pub fn terminal_state_skips(&self) -> u64 {
        self.terminal_state_skips
    }

    pub fn terminal_frame_byte_samples(&self) -> &[u64] {
        &self.terminal_frame_byte_samples[..self.terminal_frame_byte_sample_count]
    }

    pub fn terminal_frames_published(&self) -> u64 {
        self.terminal_frames_published
    }

    pub fn terminal_frame_bytes_total(&self) -> u64 {
        self.terminal_frame_bytes_total
    }

    pub fn foreground_probe_queued_samples(&self) -> &[Duration] {
        &self.foreground_probe_queued_samples[..self.foreground_probe_queued_sample_count]
    }

    pub fn foreground_probe_lock_wait_samples(&self) -> &[Duration] {
        &self.foreground_probe_lock_wait_samples[..self.foreground_probe_lock_wait_sample_count]
    }

    pub fn foreground_probe_walk_samples(&self) -> &[Duration] {
        &self.foreground_probe_walk_samples[..self.foreground_probe_walk_sample_count]
    }

    pub fn foreground_probes(&self) -> u64 {
        self.foreground_probes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_captures_then_a_drain_reports_two_and_a_second_drain_reports_zero() {
        let mut facts = ShellEfficiencyFacts::default();
        facts.record_terminal_state_capture(Duration::from_micros(10));
        facts.record_terminal_state_capture(Duration::from_micros(20));

        let drained = facts.take();
        assert_eq!(drained.terminal_state_captures(), 2);
        assert_eq!(drained.terminal_state_samples(), [
            Duration::from_micros(10),
            Duration::from_micros(20),
        ]);

        let drained_again = facts.take();
        assert_eq!(drained_again.terminal_state_captures(), 0);
        assert!(drained_again.terminal_state_samples().is_empty());
    }

    #[test]
    fn a_session_with_an_unchanged_sequence_increments_skips_and_not_captures() {
        // Mirrors the gate in `NativeEffectShell::collect_terminal_frames`:
        // an unchanged sequence takes the skip branch and must never call
        // `record_terminal_state_capture`.
        let unchanged_sequence = 5_u64;
        let last_captured_sequence = 5_u64;
        let mut facts = ShellEfficiencyFacts::default();
        if unchanged_sequence <= last_captured_sequence {
            facts.record_terminal_state_skip();
        } else {
            facts.record_terminal_state_capture(Duration::from_micros(1));
        }

        assert_eq!(facts.terminal_state_skips(), 1);
        assert_eq!(facts.terminal_state_captures(), 0);
    }

    #[test]
    fn saturating_totals_do_not_wrap_past_u64_max() {
        let mut facts = ShellEfficiencyFacts::default();
        facts.terminal_frame_bytes_total = u64::MAX - 1;
        facts.record_terminal_frame_published(10);
        assert_eq!(facts.terminal_frame_bytes_total(), u64::MAX);

        facts.terminal_state_captures = u64::MAX;
        facts.record_terminal_state_capture(Duration::from_micros(1));
        assert_eq!(facts.terminal_state_captures(), u64::MAX);
    }
}
