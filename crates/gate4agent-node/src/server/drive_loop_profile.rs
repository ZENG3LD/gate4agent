//! Fixed-window latency/rate profiling for `drive_runtime_until_shutdown`'s
//! own per-iteration phases -- the top half of the answer to "the node
//! burns CPU with zero live PTY sessions and there is no number for where."
//!
//! `gate4agent_runtime_native::tick_profile` already owns the six phases
//! *inside* `NativeRuntime::tick`; this module covers the three things that
//! surround that call in the drive loop itself (draining control events,
//! publishing terminal frames, and whatever is left of the iteration before
//! the trailing sleep), plus the loop's own actual iteration rate. It
//! reuses that other module's [`RingStats`]/[`Distribution`]/
//! [`duration_micros`] rather than redefining them: both profilers need the
//! identical "last 256 samples, p50/p95/max, no allocation after
//! construction" contract, and `gate4agent-node` already depends on
//! `gate4agent-runtime-native` for [`gate4agent_runtime_native::NativeRuntime`]
//! itself.
use gate4agent_runtime_native::tick_profile::{
    duration_micros, Distribution, RingStats, SAMPLE_WINDOW,
};
use std::time::{Duration, Instant};

/// One snapshot of the drive loop's own phase/rate readings -- what
/// `GET /metrics` reports alongside `NativeRuntime`'s own
/// [`gate4agent_runtime_native::tick_profile::TickProfileSnapshot`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DriveLoopProfileSnapshot {
    pub(super) runtime_tick_us: Distribution,
    pub(super) event_drain_us: Distribution,
    pub(super) publish_terminal_frames_us: Distribution,
    pub(super) remainder_us: Distribution,
    /// Actual iterations completed per wall-clock second, windowed over the
    /// last [`SAMPLE_WINDOW`] seconds -- the number the ~100/sec figure
    /// implied by the loop's `sleep(Duration::from_millis(10))` needs
    /// checked against, since that sleep only bounds the loop from below;
    /// it says nothing about how many iterations the phases above actually
    /// leave room for.
    pub(super) iterations_per_sec: Distribution,
}

/// Owns every drive-loop phase/rate ring `drive_runtime_until_shutdown`
/// writes into once per iteration. Lives on `NodeShared` behind a
/// `std::sync::Mutex` that is only ever locked for a `push` and dropped
/// before the next `.await` -- never held across one, so it does not need
/// `tokio::sync::Mutex`.
#[derive(Clone, Debug)]
pub(super) struct DriveLoopProfiler {
    runtime_tick_us: RingStats<SAMPLE_WINDOW>,
    event_drain_us: RingStats<SAMPLE_WINDOW>,
    publish_terminal_frames_us: RingStats<SAMPLE_WINDOW>,
    remainder_us: RingStats<SAMPLE_WINDOW>,
    iterations_per_sec: RingStats<SAMPLE_WINDOW>,
    /// Start of the one-second bucket `iterations_this_second` is counting
    /// toward.
    second_start: Instant,
    iterations_this_second: u32,
}

impl Default for DriveLoopProfiler {
    fn default() -> Self {
        Self {
            runtime_tick_us: RingStats::default(),
            event_drain_us: RingStats::default(),
            publish_terminal_frames_us: RingStats::default(),
            remainder_us: RingStats::default(),
            iterations_per_sec: RingStats::default(),
            second_start: Instant::now(),
            iterations_this_second: 0,
        }
    }
}

impl DriveLoopProfiler {
    /// `runtime.tick().await` -- the whole native-runtime tick, whose own
    /// six internal phases are separately timed by
    /// `gate4agent_runtime_native::tick_profile`.
    pub(super) fn record_runtime_tick(&mut self, elapsed: Duration) {
        self.runtime_tick_us.push(duration_micros(elapsed));
    }

    /// Draining `events.try_recv()` in a loop and republishing each control
    /// event -- includes the per-event managed-record export lookup, but
    /// not the detached `tokio::spawn`'d export task itself.
    pub(super) fn record_event_drain(&mut self, elapsed: Duration) {
        self.event_drain_us.push(duration_micros(elapsed));
    }

    /// `shared.publish_terminal_frames()` -- snapshotting sessions, then
    /// diffing and broadcasting any advanced terminal frame.
    pub(super) fn record_publish_terminal_frames(&mut self, elapsed: Duration) {
        self.publish_terminal_frames_us.push(duration_micros(elapsed));
    }

    /// Whatever is left of the iteration after the three phases above and
    /// before the trailing `sleep` -- today that is the shutdown-progress
    /// check (a no-op outside shutdown) plus this profiler's own
    /// bookkeeping.
    pub(super) fn record_remainder(&mut self, elapsed: Duration) {
        self.remainder_us.push(duration_micros(elapsed));
    }

    /// Call exactly once per drive-loop iteration, regardless of what that
    /// iteration did -- rolls the current one-second bucket into
    /// `iterations_per_sec` the moment a full second has elapsed, then
    /// counts this iteration toward the (possibly just-rolled) bucket.
    /// Rolling before counting means an iteration observed after a second
    /// boundary is credited to the new second, not folded into the one
    /// being flushed.
    ///
    /// Bounded to `SAMPLE_WINDOW` catch-up rolls for the same reason as
    /// `hatchery-tui`'s `tick_second`: a long idle gap (e.g. the process
    /// paused under a debugger) must resync rather than spin backfilling a
    /// zero-sample per elapsed second.
    pub(super) fn note_iteration(&mut self, now: Instant) {
        let mut rolled = 0;
        while now.saturating_duration_since(self.second_start) >= Duration::from_secs(1)
            && rolled < SAMPLE_WINDOW
        {
            self.iterations_per_sec.push(self.iterations_this_second);
            self.iterations_this_second = 0;
            self.second_start += Duration::from_secs(1);
            rolled += 1;
        }
        if rolled == SAMPLE_WINDOW {
            self.second_start = now;
        }
        self.iterations_this_second = self.iterations_this_second.saturating_add(1);
    }

    pub(super) fn snapshot(&self) -> DriveLoopProfileSnapshot {
        DriveLoopProfileSnapshot {
            runtime_tick_us: self.runtime_tick_us.stats(),
            event_drain_us: self.event_drain_us.stats(),
            publish_terminal_frames_us: self.publish_terminal_frames_us.stats(),
            remainder_us: self.remainder_us.stats(),
            iterations_per_sec: self.iterations_per_sec.stats(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recorded_phase_reaches_the_snapshot() {
        let mut profiler = DriveLoopProfiler::default();
        profiler.record_runtime_tick(Duration::from_micros(100));
        profiler.record_event_drain(Duration::from_micros(5));
        profiler.record_publish_terminal_frames(Duration::from_micros(7));
        profiler.record_remainder(Duration::from_micros(1));
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.runtime_tick_us.max, 100);
        assert_eq!(snapshot.event_drain_us.max, 5);
        assert_eq!(snapshot.publish_terminal_frames_us.max, 7);
        assert_eq!(snapshot.remainder_us.max, 1);
    }

    #[test]
    fn note_iteration_rolls_exactly_once_per_elapsed_second() {
        let start = Instant::now();
        let mut profiler = DriveLoopProfiler::default();
        profiler.second_start = start;
        profiler.note_iteration(start);
        profiler.note_iteration(start + Duration::from_millis(10));
        profiler.note_iteration(start + Duration::from_millis(1_001));
        let snapshot = profiler.snapshot();
        // The first two iterations land in the second that just rolled.
        assert_eq!(snapshot.iterations_per_sec.count, 1);
        assert_eq!(snapshot.iterations_per_sec.max, 2);
    }

    #[test]
    fn note_iteration_caps_catch_up_rolls_after_a_long_gap() {
        let start = Instant::now();
        let mut profiler = DriveLoopProfiler::default();
        profiler.second_start = start;
        profiler.note_iteration(start + Duration::from_secs(10_000));
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.iterations_per_sec.count, SAMPLE_WINDOW);
        assert_eq!(snapshot.iterations_per_sec.max, 0);
    }
}
