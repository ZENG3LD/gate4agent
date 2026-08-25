//! Fixed-window latency profiling for the phases inside
//! [`crate::NativeRuntime::tick`].
//!
//! Before this module existed, "the node burns CPU with zero live PTY
//! sessions" had no number attached to it anywhere in the process --
//! `tick()` ran six distinct pieces of work every call (drain observations,
//! drain ingress, reduce the control plane, dispatch effects, publish the
//! step, and walk the provider supervisors) and none of them were timed.
//! This module gives each of those six phases its own always-on reading.
//!
//! Two contracts every series here honours, mirrored from
//! `gate4agent-tui`'s `profile.rs` (that crate is a different workspace and
//! is not a dependency of this one, so the types are re-derived here rather
//! than imported -- see that module's own doc comment for the same
//! reasoning spelled out for the TUI's redraw loop):
//! - **Distributions, not an average.** A mean spreads one expensive tick
//!   across 255 idle ones and reports a number nobody can act on. Every
//!   phase keeps the last [`SAMPLE_WINDOW`] samples in a fixed ring
//!   ([`RingStats`]) and reports p50/p95/max plus the sample count,
//!   computed on demand rather than tracked incrementally.
//! - **Always-on cheap, no allocation after construction.** `push` is an
//!   array write and an index bump. The O(N log N) sort behind `stats()`
//!   only runs when something actually reads a snapshot (an HTTP `/metrics`
//!   request), never once per tick -- so this stays cheap enough to leave
//!   enabled unconditionally, which is the only way it can ever catch the
//!   stall it exists to find.
use std::time::Duration;

/// Ring capacity shared by every phase below -- "the last 256 ticks," never
/// an average across the process lifetime. At the ~10ms drive-loop cadence
/// this is a little over 2.5 seconds of tick history, enough to catch a
/// transient stall without growing unbounded.
pub const SAMPLE_WINDOW: usize = 256;

/// One windowed phase's current reading: nearest-rank p50/p95/max over
/// whatever [`RingStats`] currently holds, plus `count` -- the denominator
/// a reader must check alongside the three numbers, since `count` below
/// `SAMPLE_WINDOW` means "the process hasn't produced a full window yet,"
/// not "the window is smaller than advertised."
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Distribution {
    pub p50: u32,
    pub p95: u32,
    pub max: u32,
    pub count: usize,
}

/// Fixed-size ring buffer of `u32` samples. `push` overwrites the oldest
/// entry once full -- the only state this holds is `values`/`len`/`next`,
/// all stack-sized by the const generic `N`, so a profiler holding several
/// of these never allocates past its own construction.
#[derive(Clone, Debug)]
pub struct RingStats<const N: usize> {
    values: [u32; N],
    len: usize,
    next: usize,
}

impl<const N: usize> Default for RingStats<N> {
    fn default() -> Self {
        Self { values: [0; N], len: 0, next: 0 }
    }
}

impl<const N: usize> RingStats<N> {
    pub fn push(&mut self, value: u32) {
        self.values[self.next] = value;
        self.next = (self.next + 1) % N;
        self.len = (self.len + 1).min(N);
    }

    /// Nearest-rank percentiles over whatever the ring currently holds.
    /// Percentiles do not care about chronological order, so this sorts a
    /// stack-local COPY of the valid slice (`values` is `[u32; N]`, `Copy`
    /// because `u32` is `Copy` -- never a heap allocation) rather than the
    /// ring itself, which must keep its own write position intact for the
    /// next `push`.
    pub fn stats(&self) -> Distribution {
        if self.len == 0 {
            return Distribution::default();
        }
        let mut sorted = self.values;
        sorted[..self.len].sort_unstable();
        let rank = |percentile: usize| sorted[(self.len * percentile / 100).min(self.len - 1)];
        Distribution { p50: rank(50), p95: rank(95), max: sorted[self.len - 1], count: self.len }
    }
}

/// Clamped `Duration` -> microsecond sample: a tick phase measured in hours
/// would mean the process already hung far worse than this profiler needs
/// to describe, so this saturates at `u32::MAX` rather than panicking or
/// widening every sample to a `u128`.
pub fn duration_micros(elapsed: Duration) -> u32 {
    elapsed.as_micros().min(u128::from(u32::MAX)) as u32
}

/// One snapshot of every phase's current distribution -- what
/// [`crate::NativeRuntime::tick_profile_snapshot`] returns.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TickProfileSnapshot {
    pub drain_observations_us: Distribution,
    pub drain_ingress_us: Distribution,
    pub step_control_plane_us: Distribution,
    pub dispatch_effects_us: Distribution,
    pub publish_step_us: Distribution,
    pub provider_supervisors_us: Distribution,
}

/// Owns the six per-phase rings `NativeRuntime::tick` writes into every
/// call. Lives on `NativeRuntime` itself (not a side channel) so the timing
/// and the work it describes can never drift apart.
#[derive(Clone, Debug, Default)]
pub struct TickPhaseProfiler {
    drain_observations_us: RingStats<SAMPLE_WINDOW>,
    drain_ingress_us: RingStats<SAMPLE_WINDOW>,
    step_control_plane_us: RingStats<SAMPLE_WINDOW>,
    dispatch_effects_us: RingStats<SAMPLE_WINDOW>,
    publish_step_us: RingStats<SAMPLE_WINDOW>,
    provider_supervisors_us: RingStats<SAMPLE_WINDOW>,
}

impl TickPhaseProfiler {
    /// `effects.drain_observations` -- pulling completed native-provider
    /// observations (and any terminal frames riding along with them) off
    /// the effect dispatcher before the kernel reduces this tick.
    pub fn record_drain_observations(&mut self, elapsed: Duration) {
        self.drain_observations_us.push(duration_micros(elapsed));
    }

    /// `port.drain_ingress` -- pulling queued operator/control commands off
    /// the bounded control-plane port.
    pub fn record_drain_ingress(&mut self, elapsed: Duration) {
        self.drain_ingress_us.push(duration_micros(elapsed));
    }

    /// `kernel.step_control_plane` -- the synchronous kernel reduction
    /// itself: ingress and observations in, effects and a fresh backend
    /// snapshot out. This phase is the leading suspect for idle-tick cost,
    /// since `step_control_plane` builds a full `KernelStep` (including a
    /// backend snapshot clone) unconditionally on every call, whether or
    /// not ingress/observations carried anything to reduce.
    pub fn record_step_control_plane(&mut self, elapsed: Duration) {
        self.step_control_plane_us.push(duration_micros(elapsed));
    }

    /// The loop dispatching this tick's own kernel effects to per-instance
    /// native workers (`effects.dispatch`).
    pub fn record_dispatch_effects(&mut self, elapsed: Duration) {
        self.dispatch_effects_us.push(duration_micros(elapsed));
    }

    /// `port.publish_step` -- publishing the step's snapshot/events back
    /// out through the control-plane port.
    pub fn record_publish_step(&mut self, elapsed: Duration) {
        self.publish_step_us.push(duration_micros(elapsed));
    }

    /// Walking every installed `ProviderSupervisor::tick` plus draining
    /// their exit-ack/fault queues (`collect_provider_supervisor_events`).
    pub fn record_provider_supervisors(&mut self, elapsed: Duration) {
        self.provider_supervisors_us.push(duration_micros(elapsed));
    }

    pub fn snapshot(&self) -> TickProfileSnapshot {
        TickProfileSnapshot {
            drain_observations_us: self.drain_observations_us.stats(),
            drain_ingress_us: self.drain_ingress_us.stats(),
            step_control_plane_us: self.step_control_plane_us.stats(),
            dispatch_effects_us: self.dispatch_effects_us.stats(),
            publish_step_us: self.publish_step_us.stats(),
            provider_supervisors_us: self.provider_supervisors_us.stats(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_stats_reports_nearest_rank_percentiles() {
        let mut ring: RingStats<8> = RingStats::default();
        for value in [10, 20, 30, 40, 50, 60, 70, 80] {
            ring.push(value);
        }
        let stats = ring.stats();
        assert_eq!(stats.count, 8);
        assert_eq!(stats.max, 80);
        assert_eq!(stats.p50, 50);
    }

    #[test]
    fn ring_stats_evicts_oldest_once_full() {
        let mut ring: RingStats<4> = RingStats::default();
        for value in 1..=6u32 {
            ring.push(value);
        }
        let stats = ring.stats();
        // Only the last 4 pushes (3, 4, 5, 6) survive.
        assert_eq!(stats.count, 4);
        assert_eq!(stats.max, 6);
        assert_eq!(stats.p50, 5);
    }

    #[test]
    fn empty_ring_reports_zeroed_distribution() {
        let ring: RingStats<SAMPLE_WINDOW> = RingStats::default();
        assert_eq!(ring.stats(), Distribution::default());
    }

    #[test]
    fn tick_phase_profiler_snapshot_reflects_every_recorded_phase() {
        let mut profiler = TickPhaseProfiler::default();
        profiler.record_drain_observations(Duration::from_micros(10));
        profiler.record_drain_ingress(Duration::from_micros(20));
        profiler.record_step_control_plane(Duration::from_micros(1_500));
        profiler.record_dispatch_effects(Duration::from_micros(30));
        profiler.record_publish_step(Duration::from_micros(40));
        profiler.record_provider_supervisors(Duration::from_micros(50));
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.drain_observations_us.max, 10);
        assert_eq!(snapshot.drain_ingress_us.max, 20);
        assert_eq!(snapshot.step_control_plane_us.max, 1_500);
        assert_eq!(snapshot.dispatch_effects_us.max, 30);
        assert_eq!(snapshot.publish_step_us.max, 40);
        assert_eq!(snapshot.provider_supervisors_us.max, 50);
        for distribution in [
            snapshot.drain_observations_us,
            snapshot.drain_ingress_us,
            snapshot.step_control_plane_us,
            snapshot.dispatch_effects_us,
            snapshot.publish_step_us,
            snapshot.provider_supervisors_us,
        ] {
            assert_eq!(distribution.count, 1);
        }
    }
}
