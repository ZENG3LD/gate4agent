//! Frame-cadence and terminal-poll profiling for `client::run`'s redraw
//! loop.
//!
//! Before this module existed the owner's only signal for "the TUI lags"
//! was the word itself -- nothing in the process could turn that into a
//! number, let alone a distribution. Every series here answers one of two
//! questions client.rs could not answer before: how long did the pieces of
//! one redraw tick take (`render_us`/`flush_us`/`sixel_us`/`frame_us`/
//! `wait_us`), and what is the actual cost of polling the harness terminal
//! wire every `HARNESS_TERMINAL_POLL_INTERVAL` instead of being pushed to
//! (`terminal_rtt_us`/`terminal_frames`/`terminal_bytes`/`terminal_polls_*`).
//!
//! Two contracts every series here honours:
//! - **Distributions, not averages.** A mean hides exactly the stall an
//!   owner is trying to find (one 400ms sixel emission drowned in 999
//!   16ms frames reads as "18ms average" -- nothing to act on). Every
//!   duration/rate series keeps the last [`SAMPLE_WINDOW`] samples in a
//!   fixed ring ([`RingStats`]) and reports p50/p95/max plus the sample
//!   count, computed on demand rather than tracked incrementally.
//! - **Always-on cheap.** `push` is an array write and an index bump --
//!   no allocation, no lock, no syscall. The only per-sample cost this
//!   module ever adds to the hot path is that one write; the O(N log N)
//!   sort behind `stats()` only runs when something actually asks for a
//!   reading (the overlay paints, or the log's own multi-second cadence
//!   fires), never once per frame.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Ring capacity shared by every windowed series below -- "the last 256
/// samples," never more, never an average across the process lifetime.
/// 256 loop iterations at the ~16ms dirty-frame cadence is a little over
/// 4 seconds of frame-timing history, and at the 250ms terminal-poll
/// cadence it is over a minute of poll history -- enough to catch a stall
/// that just happened without growing unbounded.
const SAMPLE_WINDOW: usize = 256;

/// How often [`TuiProfiler::maybe_write_log`] actually writes a line --
/// "a few seconds," per spec, not every frame: the log is a trend record,
/// not a replacement for the live overlay.
const LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// One windowed series' current reading: nearest-rank p50/p95/max over
/// whatever [`RingStats`] currently holds, plus `count` -- the denominator
/// every caller must print alongside the three numbers, since `count` below
/// `SAMPLE_WINDOW` means "the process hasn't produced a full window yet,"
/// not "the window is smaller than advertised."
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Distribution {
    pub p50: u32,
    pub p95: u32,
    pub max: u32,
    pub count: usize,
}

/// Fixed-size ring buffer of `u32` samples. `push` overwrites the oldest
/// entry once full -- the only state this holds is `values`/`len`/`next`,
/// all stack-sized by the const generic `N`, so a `TuiProfiler` holding
/// several of these never allocates past its own construction.
#[derive(Clone, Debug)]
struct RingStats<const N: usize> {
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
    fn push(&mut self, value: u32) {
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
    fn stats(&self) -> Distribution {
        if self.len == 0 {
            return Distribution::default();
        }
        let mut sorted = self.values;
        sorted[..self.len].sort_unstable();
        let rank = |percentile: usize| sorted[(self.len * percentile / 100).min(self.len - 1)];
        Distribution { p50: rank(50), p95: rank(95), max: sorted[self.len - 1], count: self.len }
    }
}

/// One redraw tick's own phase breakdown, handed to
/// [`TuiProfiler::record_frame`] so the remainder is always subtracted
/// from the same six numbers the named series report.
///
/// `frame_us` measured the whole tick and three of its pieces
/// (`render_us`/`flush_us`/`sixel_us`), which left the difference
/// unattributed: p50 1387us of frame against 1096us of measured pieces,
/// and a 21ms maximum with no way to say which step produced it. The
/// three phases added here -- the animation/pet integrators, the
/// terminal-resize action queueing, the two cursor syscalls -- close that
/// gap, and `frame_remainder_us` is what is left over after all six.
///
/// A remainder that stays near zero means the breakdown is complete; one
/// that grows means a step was added to the tick without being measured.
/// Same shape the node's own `drive_loop_phases_us` already uses.
#[derive(Clone, Copy, Debug, Default)]
pub struct FramePhases {
    /// `consume_redraw` + the spinner ticks + `step_pet` +
    /// `step_pet_arcade` -- everything before the paint begins.
    pub animate: Duration,
    /// `render::render` into the back buffer. Also reported on its own as
    /// `render_us`.
    pub render: Duration,
    /// `changed_terminal_sizes` and the `queue_action` calls it feeds --
    /// unmeasured before, and it can enqueue real work.
    pub queue: Duration,
    /// `Screen::flush`. Also reported on its own as `flush_us`.
    pub flush: Duration,
    /// The sixel icon pass plus the arcade's pixel frame. Also reported on
    /// its own as `sixel_us`.
    pub sixel: Duration,
    /// `execute!(Hide)`, the first write of the frame. Split from its
    /// sibling below because the two look identical on paper -- a few
    /// escape bytes each -- and measured 226us together at p50 against a
    /// `screen.flush()` that writes a whole frame's diff for 34us. Two
    /// tiny writes cannot cost seven times a large one, so one of these
    /// is not paying for its bytes: either `sync_cursor`'s own
    /// `visible_cursor_position` walk, or this one absorbing the
    /// terminal's back-pressure by being first through the door.
    pub cursor_hide: Duration,
    /// `sync_cursor`: `visible_cursor_position` plus the `MoveTo`/`Show`
    /// (or `Hide`) that follows from it, the last write of the frame.
    pub cursor_sync: Duration,
}

impl FramePhases {
    /// Everything this breakdown accounts for, for the remainder subtraction.
    fn total(self) -> Duration {
        self.animate
            + self.render
            + self.queue
            + self.flush
            + self.sixel
            + self.cursor_hide
            + self.cursor_sync
    }
}

/// Everything both sinks (the overlay, the log line) read -- a single
/// snapshot so the two can never disagree about "right now": both format
/// this same struct, never re-derive their own numbers from the live
/// counters.
// `Serialize`: `control_plane`'s `QueryState` verb reports this struct
// verbatim over the wire (see that module's own `ControlStateV1::profile`)
// -- one canonical shape rather than a hand-duplicated mirror struct kept
// in sync by hand.
#[derive(Clone, Debug, Serialize)]
pub struct ProfileSnapshot {
    /// The redraw tick's own phases, in the order the tick runs them.
    /// `render_us`/`flush_us`/`sixel_us` are three of the six; the other
    /// three and the leftover are described on [`FramePhases`].
    pub animate_us: Distribution,
    pub render_us: Distribution,
    pub flush_us: Distribution,
    pub sixel_us: Distribution,
    pub queue_us: Distribution,
    pub cursor_hide_us: Distribution,
    pub cursor_sync_us: Distribution,
    pub frame_us: Distribution,
    /// `frame_us` minus every named phase above. Near zero means the
    /// breakdown accounts for the whole tick; a growing remainder means a
    /// step joined the tick without being measured.
    pub frame_remainder_us: Distribution,
    pub wait_us: Distribution,
    pub terminal_rtt_us: Distribution,
    /// Age of a terminal frame at the moment this client received it:
    /// wall-clock milliseconds between the node materialising the screen
    /// (`TerminalFrame::produced_at_unix_ms`) and the frame arriving here.
    /// This is the provider-to-pixel number, and it is the only series
    /// that spans processes. Frames stamped 0 are older peers that do not
    /// send the field and are excluded rather than counted as decades old.
    /// Node and client share a clock today; across hosts this would carry
    /// clock skew as well as latency, and must be read as such.
    pub frame_age_ms: Distribution,
    /// Actual redraws delivered per wall-clock second, windowed over the
    /// last [`SAMPLE_WINDOW`] seconds. Compared against `fps_ceiling`, the
    /// most this loop's own `DIRTY_FRAME_INTERVAL` coalescing could ever
    /// produce -- the named denominator this series needs, since "58 fps"
    /// alone says nothing without knowing the cadence that caps it.
    pub fps: Distribution,
    pub fps_ceiling: u32,
    pub terminal_frames_per_sec: Distribution,
    pub terminal_bytes_per_sec: Distribution,
    /// Cumulative since process start -- these are ratios/totals, not
    /// distributions: there is no percentile of "was this poll empty."
    pub terminal_polls_total: u64,
    pub terminal_polls_empty: u64,
    pub terminal_frames_total: u64,
    pub terminal_bytes_total: u64,
    /// Cumulative since process start: frames the harness coalesced away
    /// (replaced before ever being sent) on a terminal-push subscription --
    /// see `TuiProfiler::record_terminal_coalesced`'s own doc comment for
    /// why this stays 0 for an idle-to-moderate session and only ever
    /// climbs when one is producing output faster than this connection
    /// drains it.
    pub terminal_coalesced_total: u64,
}

/// Owns every frame-cadence and terminal-poll measurement for one running
/// TUI session. Lives on `App` (`App::profiler`), not as a local in
/// `client::run`: `render::render` only ever receives `&App` (the whole
/// crate's own test suite depends on that signature), and the overlay this
/// module feeds has to read a live reading from inside that same render
/// pass -- putting the profiler anywhere `render::render` cannot reach
/// would mean either threading a new parameter through every renderer in
/// the crate, or duplicating this data on a side channel. `client::run`
/// still does 100% of the actual timing (it is the only place any of
/// these ticks happen) -- it just writes into `app.profiler` instead of a
/// loop-local variable, the same way it already writes `app.layout` every
/// redraw.
#[derive(Clone, Debug)]
pub struct TuiProfiler {
    animate_us: RingStats<SAMPLE_WINDOW>,
    render_us: RingStats<SAMPLE_WINDOW>,
    flush_us: RingStats<SAMPLE_WINDOW>,
    sixel_us: RingStats<SAMPLE_WINDOW>,
    queue_us: RingStats<SAMPLE_WINDOW>,
    cursor_hide_us: RingStats<SAMPLE_WINDOW>,
    cursor_sync_us: RingStats<SAMPLE_WINDOW>,
    frame_us: RingStats<SAMPLE_WINDOW>,
    frame_remainder_us: RingStats<SAMPLE_WINDOW>,
    wait_us: RingStats<SAMPLE_WINDOW>,
    terminal_rtt_us: RingStats<SAMPLE_WINDOW>,
    frame_age_ms: RingStats<SAMPLE_WINDOW>,
    fps: RingStats<SAMPLE_WINDOW>,
    terminal_frames_per_sec: RingStats<SAMPLE_WINDOW>,
    terminal_bytes_per_sec: RingStats<SAMPLE_WINDOW>,

    terminal_polls_total: u64,
    terminal_polls_empty: u64,
    terminal_frames_total: u64,
    terminal_bytes_total: u64,
    terminal_coalesced_total: u64,

    /// The current one-second bucket's own accumulators, rolled into the
    /// three `*_per_sec` rings above by [`Self::tick_second`] the moment a
    /// full second has elapsed since `second_start`.
    second_start: Instant,
    frames_this_second: u32,
    terminal_frames_this_second: u32,
    terminal_bytes_this_second: u32,

    /// When the most recent `AppAction::HarnessOpenTerminal` was queued --
    /// see [`Self::begin_terminal_poll`]'s own doc comment for why a
    /// single `Option` (not a per-request map) is the right amount of
    /// bookkeeping for this poll's own cadence.
    terminal_poll_issued_at: Option<Instant>,

    last_log_flush: Instant,
}

impl Default for TuiProfiler {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            animate_us: RingStats::default(),
            render_us: RingStats::default(),
            flush_us: RingStats::default(),
            sixel_us: RingStats::default(),
            queue_us: RingStats::default(),
            cursor_hide_us: RingStats::default(),
            cursor_sync_us: RingStats::default(),
            frame_us: RingStats::default(),
            frame_remainder_us: RingStats::default(),
            wait_us: RingStats::default(),
            terminal_rtt_us: RingStats::default(),
            frame_age_ms: RingStats::default(),
            fps: RingStats::default(),
            terminal_frames_per_sec: RingStats::default(),
            terminal_bytes_per_sec: RingStats::default(),
            terminal_polls_total: 0,
            terminal_polls_empty: 0,
            terminal_frames_total: 0,
            terminal_bytes_total: 0,
            terminal_coalesced_total: 0,
            second_start: now,
            frames_this_second: 0,
            terminal_frames_this_second: 0,
            terminal_bytes_this_second: 0,
            terminal_poll_issued_at: None,
            last_log_flush: now,
        }
    }
}

/// Clamped `Duration` -> microsecond sample: a redraw tick measured in
/// hours would mean the app already hung far worse than this profiler
/// needs to describe, so this saturates at `u32::MAX` rather than
/// panicking or growing every sample to a `u128`.
fn duration_micros(elapsed: Duration) -> u32 {
    elapsed.as_micros().min(u128::from(u32::MAX)) as u32
}

impl TuiProfiler {
    pub fn record_render(&mut self, elapsed: Duration) {
        self.render_us.push(duration_micros(elapsed));
    }

    pub fn record_flush(&mut self, elapsed: Duration) {
        self.flush_us.push(duration_micros(elapsed));
    }

    pub fn record_sixel(&mut self, elapsed: Duration) {
        self.sixel_us.push(duration_micros(elapsed));
    }

    /// The whole redraw tick, start to finish, plus the breakdown of what
    /// it spent that time on -- also the ONE place a completed redraw is
    /// counted toward `fps` (see `tick_second`'s own doc comment): a
    /// "frame" is exactly what this measures the duration of, so both
    /// belong at the same call site.
    ///
    /// `phases` carries all six spans, including the three that are also
    /// recorded on their own (`record_render`/`record_flush`/
    /// `record_sixel`), so the remainder is subtracted from exactly the
    /// numbers the named series report rather than from a second reading
    /// of the same clock.
    ///
    /// The subtraction saturates: the phases are measured one at a time
    /// and their total is a sum of independently rounded spans, so it can
    /// exceed `elapsed` by a microsecond. A remainder pinned at 0 says the
    /// breakdown is complete, which is exactly what it means.
    pub fn record_frame(&mut self, elapsed: Duration, phases: FramePhases) {
        self.frame_us.push(duration_micros(elapsed));
        self.animate_us.push(duration_micros(phases.animate));
        self.queue_us.push(duration_micros(phases.queue));
        self.cursor_hide_us.push(duration_micros(phases.cursor_hide));
        self.cursor_sync_us.push(duration_micros(phases.cursor_sync));
        self.frame_remainder_us
            .push(duration_micros(elapsed.saturating_sub(phases.total())));
        self.frames_this_second = self.frames_this_second.saturating_add(1);
    }

    /// How long this loop iteration's own `event::poll` actually blocked
    /// before returning -- the spare time this pass had available, capped
    /// at whatever deadline `FrameScheduler::poll_timeout` computed for it.
    pub fn record_wait(&mut self, elapsed: Duration) {
        self.wait_us.push(duration_micros(elapsed));
    }

    /// Rolls the current one-second bucket into the three `*_per_sec`
    /// rings the moment a full second has elapsed, then resets it -- call
    /// once per main-loop iteration, unconditionally, regardless of
    /// whether this tick redrew or polled anything. That unconditional
    /// call is what lets an idle second (nothing dirty, PTY silent) still
    /// land a real `fps == 0` sample instead of silently vanishing from
    /// the window -- the loop still wakes at least every
    /// `EVENT_POLL_INTERVAL`, so this is never starved of a chance to
    /// notice the boundary passed.
    ///
    /// Bounded to `SAMPLE_WINDOW` catch-up rolls: a suspend/resume or a
    /// debugger pause can leave `now` many seconds past `second_start`,
    /// and backfilling one zero-sample per elapsed second would otherwise
    /// turn this into an unbounded loop. Past that many seconds the gap
    /// is not a frame-rate story any distribution helps with anyway, so
    /// this resyncs `second_start` to `now` directly instead of grinding
    /// through the rest of it.
    pub fn tick_second(&mut self, now: Instant) {
        let mut rolled = 0;
        while now.saturating_duration_since(self.second_start) >= Duration::from_secs(1)
            && rolled < SAMPLE_WINDOW
        {
            self.fps.push(self.frames_this_second);
            self.terminal_frames_per_sec.push(self.terminal_frames_this_second);
            self.terminal_bytes_per_sec.push(self.terminal_bytes_this_second);
            self.frames_this_second = 0;
            self.terminal_frames_this_second = 0;
            self.terminal_bytes_this_second = 0;
            self.second_start += Duration::from_secs(1);
            rolled += 1;
        }
        if rolled == SAMPLE_WINDOW {
            self.second_start = now;
        }
    }

    /// `AppAction::HarnessOpenTerminal` was just queued -- starts the
    /// clock `terminal_rtt_us` reads on the matching
    /// `WorkerUpdate::HarnessTerminalRead` (`record_terminal_poll`).
    /// Overwrites any still-outstanding mark rather than keeping a queue
    /// of them: `HARNESS_TERMINAL_POLL_INTERVAL` gates this call to at
    /// most once every 250ms and the worker thread that answers it
    /// processes requests in order, so in practice at most one request is
    /// ever in flight -- a genuinely overlapping pair just widens this
    /// series' own spread instead of reporting a wrong number outright, a
    /// known, accepted approximation for a diagnostic reading.
    pub fn begin_terminal_poll(&mut self, now: Instant) {
        self.terminal_poll_issued_at = Some(now);
    }

    /// A `WorkerUpdate::HarnessTerminalRead` response was just applied --
    /// closes the RTT clock `begin_terminal_poll` started, and folds this
    /// poll's own frame/byte counts into both the lifetime totals (the
    /// named denominator for the "how many polls actually returned
    /// anything" question) and the current second's bucket.
    /// `frame_count == 0` is exactly "an empty poll" -- the direct cost of
    /// this terminal riding a 250ms poll instead of a push.
    /// Folds one received frame's age into `frame_age_ms`.
    ///
    /// `produced_at_unix_ms == 0` means the sender did not stamp the frame
    /// (an older peer, or a fixture) and is skipped: recording it would
    /// report an age measured from 1970 and drag every percentile with it.
    /// A stamp from the future — a clock that stepped backwards between
    /// the node writing it and this client reading it — clamps to 0 rather
    /// than wrapping into an enormous positive number.
    pub fn record_frame_age(&mut self, produced_at_unix_ms: u64, received_at_unix_ms: u64) {
        if produced_at_unix_ms == 0 {
            return;
        }
        // Saturating into u32 caps a nonsense age at ~49 days rather than
        // truncating it into a small, believable-looking number: a wrong
        // reading must look wrong.
        let age_ms = received_at_unix_ms.saturating_sub(produced_at_unix_ms);
        self.frame_age_ms.push(u32::try_from(age_ms).unwrap_or(u32::MAX));
    }

    pub fn record_terminal_poll(&mut self, now: Instant, frame_count: usize, byte_count: usize) {
        if let Some(issued_at) = self.terminal_poll_issued_at.take() {
            self.terminal_rtt_us.push(duration_micros(now.saturating_duration_since(issued_at)));
        }
        self.terminal_polls_total = self.terminal_polls_total.saturating_add(1);
        if frame_count == 0 {
            self.terminal_polls_empty = self.terminal_polls_empty.saturating_add(1);
        }
        self.terminal_frames_total = self.terminal_frames_total.saturating_add(frame_count as u64);
        self.terminal_bytes_total = self.terminal_bytes_total.saturating_add(byte_count as u64);
        self.terminal_frames_this_second = self.terminal_frames_this_second
            .saturating_add(frame_count.min(u32::MAX as usize) as u32);
        self.terminal_bytes_this_second = self.terminal_bytes_this_second
            .saturating_add(byte_count.min(u32::MAX as usize) as u32);
    }

    /// Folds a PUSHED frame's count/bytes into the SAME throughput series
    /// `record_terminal_poll` feeds (`terminal_frames_total`/`terminal_
    /// bytes_total`, and the two `*_this_second` accumulators those same
    /// fields roll into every `tick_second`) -- total frame/byte throughput
    /// is transport-agnostic, so both paths belong in one series. What it
    /// deliberately does NOT touch is `terminal_rtt_us`/`terminal_polls_
    /// total`/`terminal_polls_empty`: those three must stay poll-only,
    /// because they are exactly the numbers this backlog item is judged
    /// against falling toward zero (see this crate's own terminal-push-
    /// channel plan, "how the result is measured") -- folding a pushed
    /// frame into them would corrupt the very measurement the feature
    /// exists to move.
    pub fn record_terminal_pushed(&mut self, frame_count: usize, byte_count: usize) {
        self.terminal_frames_total = self.terminal_frames_total.saturating_add(frame_count as u64);
        self.terminal_bytes_total = self.terminal_bytes_total.saturating_add(byte_count as u64);
        self.terminal_frames_this_second = self.terminal_frames_this_second
            .saturating_add(frame_count.min(u32::MAX as usize) as u32);
        self.terminal_bytes_this_second = self.terminal_bytes_this_second
            .saturating_add(byte_count.min(u32::MAX as usize) as u32);
    }

    /// A running count of frames the harness coalesced away (replaced
    /// before ever being sent -- see `TerminalSubscriberRegistry`'s own
    /// doc comment, `gate4agent-harness-service::terminal`) before they
    /// ever reached this client. 0 unless a session is producing output
    /// faster than this connection can drain it, in which case it is
    /// exactly the "how much did the push path drop" number the
    /// coalescing design needs to stay honest about -- a number this
    /// client had no way to see before this series existed, not a
    /// regression check against one it already had.
    pub fn record_terminal_coalesced(&mut self, count: u32) {
        self.terminal_coalesced_total = self.terminal_coalesced_total.saturating_add(u64::from(count));
    }

    pub fn snapshot(&self) -> ProfileSnapshot {
        // `1000 / DIRTY_FRAME_INTERVAL` -- the actual redraw-coalescing
        // constant `client::run` schedules against, not a duplicated
        // literal here: this ceiling moves automatically if that cadence
        // ever does.
        let interval_ms = crate::client::DIRTY_FRAME_INTERVAL.as_millis().max(1);
        let fps_ceiling = (1000u128 / interval_ms).min(u128::from(u32::MAX)) as u32;
        ProfileSnapshot {
            animate_us: self.animate_us.stats(),
            render_us: self.render_us.stats(),
            flush_us: self.flush_us.stats(),
            sixel_us: self.sixel_us.stats(),
            queue_us: self.queue_us.stats(),
            cursor_hide_us: self.cursor_hide_us.stats(),
            cursor_sync_us: self.cursor_sync_us.stats(),
            frame_us: self.frame_us.stats(),
            frame_remainder_us: self.frame_remainder_us.stats(),
            wait_us: self.wait_us.stats(),
            terminal_rtt_us: self.terminal_rtt_us.stats(),
            frame_age_ms: self.frame_age_ms.stats(),
            fps: self.fps.stats(),
            fps_ceiling,
            terminal_frames_per_sec: self.terminal_frames_per_sec.stats(),
            terminal_bytes_per_sec: self.terminal_bytes_per_sec.stats(),
            terminal_polls_total: self.terminal_polls_total,
            terminal_polls_empty: self.terminal_polls_empty,
            terminal_frames_total: self.terminal_frames_total,
            terminal_bytes_total: self.terminal_bytes_total,
            terminal_coalesced_total: self.terminal_coalesced_total,
        }
    }

    /// Best-effort append to `%LOCALAPPDATA%\Gate4Agent\tui-profile.log`,
    /// gated to once every `LOG_FLUSH_INTERVAL` -- same "resolve the path
    /// fresh, swallow every error, never let the diagnostic sink take the
    /// app down with it" contract as `App::append_event_to_diagnostics_
    /// file`, and the same directory as that file's own `tui-events.log`.
    /// A no-op call (checked every loop iteration) costs one `Instant`
    /// comparison; the actual write only happens on the rare iteration
    /// that crosses the interval.
    pub fn maybe_write_log(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_log_flush) < LOG_FLUSH_INTERVAL {
            return;
        }
        self.last_log_flush = now;
        let Some(path) = crate::preferences::default_path()
            .and_then(|config| config.parent().map(|dir| dir.join("tui-profile.log")))
        else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        let snapshot = self.snapshot();
        // Space-joined `name=p50/p95/max/n` fields, one per series, plus
        // the two counter pairs that have a denominator but no percentile
        // (`terminal_polls`) or lifetime total worth carrying alongside
        // the windowed rate (`terminal_frames_total`/`terminal_bytes_
        // total`) -- see `log_field`'s own doc comment for the per-field
        // shape a reading script parses.
        let fields = [
            log_field("animate_us", snapshot.animate_us),
            log_field("render_us", snapshot.render_us),
            log_field("flush_us", snapshot.flush_us),
            log_field("sixel_us", snapshot.sixel_us),
            log_field("queue_us", snapshot.queue_us),
            log_field("cursor_hide_us", snapshot.cursor_hide_us),
            log_field("cursor_sync_us", snapshot.cursor_sync_us),
            log_field("frame_us", snapshot.frame_us),
            log_field("frame_remainder_us", snapshot.frame_remainder_us),
            log_field("wait_us", snapshot.wait_us),
            log_field("fps", snapshot.fps),
            format!("fps_ceiling={}", snapshot.fps_ceiling),
            log_field("terminal_rtt_us", snapshot.terminal_rtt_us),
            log_field("frame_age_ms", snapshot.frame_age_ms),
            log_field("terminal_frames_per_sec", snapshot.terminal_frames_per_sec),
            log_field("terminal_bytes_per_sec", snapshot.terminal_bytes_per_sec),
            format!(
                "terminal_polls={}/{}",
                snapshot.terminal_polls_empty, snapshot.terminal_polls_total,
            ),
            format!("terminal_frames_total={}", snapshot.terminal_frames_total),
            format!("terminal_bytes_total={}", snapshot.terminal_bytes_total),
            format!("terminal_coalesced_total={}", snapshot.terminal_coalesced_total),
        ];
        let line = format!("{stamp} {}\n", fields.join(" "));
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// One `name=p50/p95/max/n` field for the log line -- a script reads this
/// with `name=(\d+)/(\d+)/(\d+)/(\d+)` per series; the slash-quad keeps
/// one line per sample window instead of four.
fn log_field(name: &str, dist: Distribution) -> String {
    format!("{name}={}/{}/{}/{}", dist.p50, dist.p95, dist.max, dist.count)
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

    /// The remainder is the whole point of the breakdown: it is what the
    /// named phases did NOT account for. A tick whose phases sum to its
    /// own duration leaves zero; a tick with an unmeasured step leaves
    /// exactly that step.
    #[test]
    fn the_frame_remainder_is_whatever_the_named_phases_did_not_account_for() {
        let mut profiler = TuiProfiler::default();
        let accounted = FramePhases {
            animate: Duration::from_micros(100),
            render: Duration::from_micros(900),
            queue: Duration::from_micros(10),
            flush: Duration::from_micros(40),
            sixel: Duration::from_micros(8),
            cursor_hide: Duration::from_micros(5),
            cursor_sync: Duration::from_micros(7),
        };
        profiler.record_frame(Duration::from_micros(1_070), accounted);
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.frame_remainder_us.max, 0, "a fully accounted tick leaves nothing over");
        assert_eq!(snapshot.animate_us.max, 100);
        assert_eq!(snapshot.queue_us.max, 10);
        assert_eq!(snapshot.cursor_hide_us.max, 5);
        assert_eq!(snapshot.cursor_sync_us.max, 7);

        // The same phases inside a tick that took 5ms longer: every one of
        // those microseconds is unattributed, and the remainder says so.
        profiler.record_frame(Duration::from_micros(6_070), accounted);
        assert_eq!(profiler.snapshot().frame_remainder_us.max, 5_000);
    }

    /// The subtraction must not wrap when the phase sum edges past the
    /// tick's own reading -- six independently rounded spans can total a
    /// microsecond more than the single span measured around them.
    #[test]
    fn a_phase_sum_past_the_tick_reads_as_zero_left_over() {
        let mut profiler = TuiProfiler::default();
        profiler.record_frame(
            Duration::from_micros(100),
            FramePhases { render: Duration::from_micros(101), ..FramePhases::default() },
        );
        assert_eq!(profiler.snapshot().frame_remainder_us.max, 0);
    }

    #[test]
    fn tick_second_rolls_exactly_once_per_elapsed_second() {
        let start = Instant::now();
        let mut profiler = TuiProfiler::default();
        profiler.second_start = start;
        profiler.record_frame(Duration::from_micros(500), FramePhases::default());
        profiler.record_frame(Duration::from_micros(500), FramePhases::default());
        profiler.tick_second(start + Duration::from_millis(1_001));
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.fps.count, 1);
        assert_eq!(snapshot.fps.max, 2);
    }

    #[test]
    fn tick_second_caps_catch_up_rolls_after_a_long_gap() {
        let start = Instant::now();
        let mut profiler = TuiProfiler::default();
        profiler.second_start = start;
        // A gap far wider than the window must not spin proportionally to
        // the elapsed time -- it resyncs instead of rolling thousands of
        // zero samples.
        profiler.tick_second(start + Duration::from_secs(10_000));
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.fps.count, SAMPLE_WINDOW);
        assert_eq!(snapshot.fps.max, 0);
    }

    #[test]
    fn terminal_poll_round_trip_records_rtt_and_counters() {
        let start = Instant::now();
        let mut profiler = TuiProfiler::default();
        profiler.begin_terminal_poll(start);
        profiler.record_terminal_poll(start + Duration::from_millis(12), 3, 900);
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.terminal_rtt_us.count, 1);
        assert_eq!(snapshot.terminal_rtt_us.max, 12_000);
        assert_eq!(snapshot.terminal_polls_total, 1);
        assert_eq!(snapshot.terminal_polls_empty, 0);
        assert_eq!(snapshot.terminal_frames_total, 3);
        assert_eq!(snapshot.terminal_bytes_total, 900);
    }

    #[test]
    fn terminal_pushed_frames_fold_into_throughput_but_never_poll_counters() {
        // The whole point of `record_terminal_pushed`: it must move
        // `terminal_frames_total`/`terminal_bytes_total` (throughput is
        // transport-agnostic) while leaving `terminal_rtt_us`/`terminal_
        // polls_total`/`terminal_polls_empty` at zero -- those three are
        // exactly the numbers this backlog item needs to fall toward zero,
        // and a pushed frame folding into them would be the bug this test
        // exists to catch.
        let mut profiler = TuiProfiler::default();
        profiler.record_terminal_pushed(2, 128);
        profiler.record_terminal_pushed(1, 32);
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.terminal_frames_total, 3);
        assert_eq!(snapshot.terminal_bytes_total, 160);
        assert_eq!(snapshot.terminal_rtt_us.count, 0);
        assert_eq!(snapshot.terminal_polls_total, 0);
        assert_eq!(snapshot.terminal_polls_empty, 0);
    }

    #[test]
    fn terminal_coalesced_accumulates_independently_of_every_other_counter() {
        let mut profiler = TuiProfiler::default();
        profiler.record_terminal_pushed(1, 10);
        profiler.record_terminal_coalesced(3);
        profiler.record_terminal_coalesced(2);
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.terminal_coalesced_total, 5);
        // Still transport-agnostic-throughput-only, unaffected by the
        // coalesce count landing alongside it.
        assert_eq!(snapshot.terminal_frames_total, 1);
    }

    #[test]
    fn empty_terminal_poll_counts_toward_the_empty_denominator() {
        let mut profiler = TuiProfiler::default();
        profiler.record_terminal_poll(Instant::now(), 0, 0);
        profiler.record_terminal_poll(Instant::now(), 2, 40);
        let snapshot = profiler.snapshot();
        assert_eq!(snapshot.terminal_polls_total, 2);
        assert_eq!(snapshot.terminal_polls_empty, 1);
    }

    #[test]
    fn snapshot_fps_ceiling_matches_dirty_frame_interval() {
        let profiler = TuiProfiler::default();
        let snapshot = profiler.snapshot();
        assert_eq!(
            snapshot.fps_ceiling,
            (1000 / crate::client::DIRTY_FRAME_INTERVAL.as_millis()) as u32
        );
    }
}
