//! Session-scoped VT100 projection cache for the PTY panel renderer.
//!
//! Before this existed, `render::render_terminal` built a brand new
//! `vt100::Parser` for the live screen AND one more per visible scrollback
//! row, on EVERY frame, for EVERY PTY panel on screen -- a 30-row panel
//! meant 31 parsers/frame, and the cost scaled with panel height. Parsing
//! is state that belongs to the session and its display geometry, not to
//! the frame: this module holds that state so a frame with no new PTY
//! output does nothing but copy already-projected cells.
//!
//! # Why reusing (not recreating) the live parser is safe
//!
//! [`SessionCache`] keeps ONE live `vt100::Parser` per session and resizes
//! it in place (`Screen::set_size`) instead of throwing it away, re-running
//! `process` only when the input bytes or the geometry actually changed.
//!
//! The bytes the wire hands us for the live screen are a FULL screen
//! (`gate4agent::pty::event`'s own doc comment: "ANSI-formatted visible
//! screen suitable for reconstructing decoration"), produced by vt100's own
//! `Screen::contents_formatted`, which does open with a clear-attrs +
//! clear-screen pair. That would already make replay into a dirty parser
//! equivalent to replay into a blank one -- but it is a property of the
//! data a remote sender happens to produce, not an invariant this cache can
//! enforce, and the failure it hides is silent: the previous frame stays
//! underneath the new one, wrong in a way that reads as a rendering bug
//! anywhere but here. So `refresh_current` blanks the screen itself first
//! (`SCREEN_RESET`) and the reuse is correct for ANY input.
//!
//! Scrollback rows get no such reuse for their (short-lived, one-shot)
//! parser: the owner's own mandate is that scrollback is never truncated,
//! so a row, once it has scrolled off the live screen, never changes
//! again -- it is parsed once, ever, and the resulting cells are kept
//! forever (until a width or color-mode change invalidates the whole
//! row set, since both change what "the projection of this row" means).

use std::collections::HashMap;

use uzor_tui::TerminalBuffer;

use crate::app::{PtyColorMode, SessionAddress};
use crate::pty_palette::apply_pty_palette;

/// Blanks the screen and clears pending attributes before a full-screen
/// snapshot is replayed into a reused parser. See `refresh_current`.
const SCREEN_RESET: &[u8] = &[0x1b, b'[', b'm', 0x1b, b'[', b'H', 0x1b, b'[', b'2', b'J'];

/// Owns every session's projection cache. Lives on `App`, wrapped in a
/// `RefCell` there: `render::render` and its entire call tree take `&App`
/// (over a hundred call sites across this crate's own test suite depend
/// on that), so the `RefCell` is the interior-mutability seam that lets a
/// read-only render pass still update a perf cache. It is not global
/// state -- it is owned by, and dropped with, the specific `App` instance
/// that created it, exactly like `terminal_scroll_offsets` or any other
/// per-session `App` field.
#[derive(Default)]
pub(crate) struct PtyRenderCache {
    entries: HashMap<SessionAddress, SessionCache>,
}

impl std::fmt::Debug for PtyRenderCache {
    /// Never prints entry internals: a `vt100::Parser` has no `Debug` impl
    /// of its own (see [`SessionCache`]'s doc comment), so this can only
    /// ever report shape, not content.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyRenderCache")
            .field("cached_sessions", &self.entries.len())
            .finish()
    }
}

impl Clone for PtyRenderCache {
    /// `vt100::Parser` implements neither `Clone` nor `Debug` (it wraps a
    /// private `vte` state machine), so an `App` clone -- this crate's own
    /// test suite clones `App` to compare renders before/after a mutation
    /// -- cannot carry live projections over. That is harmless: this is a
    /// pure performance cache keyed on session identity, geometry, input
    /// bytes and color mode, never a source of truth, so a cold clone
    /// only costs the next frame a cache miss, never a wrong pixel.
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PtyRenderCache {
    /// The palette-applied projection of the session's CURRENT (live)
    /// screen at `(width, height)` -- reprojects through vt100 only if
    /// `input` or the geometry changed since the last call for this
    /// `address`, and reapplies only the palette (skipping vt100 entirely)
    /// if just `mode` changed.
    pub(crate) fn project_current(
        &mut self,
        address: &SessionAddress,
        input: &[u8],
        width: u16,
        height: u16,
        mode: PtyColorMode,
    ) -> &TerminalBuffer {
        let width = width.max(1);
        let height = height.max(1);
        let entry = self
            .entries
            .entry(address.clone())
            .or_insert_with(|| SessionCache::new(width, height));
        entry.refresh_current(input, width, height, mode);
        &entry.painted
    }

    /// The palette-applied projection of one frozen scrollback row
    /// (`row_index`, index-aligned with `session.terminal_scrollback`).
    /// Parsed once, ever, per `(row_index, width, mode)`; a width or mode
    /// change invalidates every cached row at once (both change what the
    /// projection means), never just the one being read.
    pub(crate) fn project_row(
        &mut self,
        address: &SessionAddress,
        row_index: usize,
        row_bytes: &[u8],
        width: u16,
        mode: PtyColorMode,
    ) -> &TerminalBuffer {
        let width = width.max(1);
        let entry = self
            .entries
            .entry(address.clone())
            .or_insert_with(|| SessionCache::new(width, 1));
        entry.row(row_index, row_bytes, width, mode)
    }

    /// Drops cache entries for sessions that no longer exist -- called
    /// once per node-update reconciliation pass (`App::reconcile_pty_
    /// render_cache`), the same rhythm `terminal_scroll_offsets` already
    /// prunes on. Without this, a long-running TUI that opens and closes
    /// many PTY sessions (each with its own `SessionAddress` generation)
    /// would accumulate one dead `vt100::Parser` + scrollback projection
    /// per session, forever.
    pub(crate) fn retain_live(&mut self, mut is_live: impl FnMut(&SessionAddress) -> bool) {
        self.entries.retain(|address, _| is_live(address));
    }
}

/// One session's live-screen parser plus its frozen scrollback row cache.
/// Holds a real `vt100::Parser`, which is why this type -- and therefore
/// [`PtyRenderCache`] -- cannot derive `Clone`/`Debug`.
struct SessionCache {
    /// `(width, height)` the live parser and `raw`/`painted` are sized to.
    geometry: (u16, u16),
    /// Reused across frames -- see the module doc comment for why
    /// replaying a fresh full-screen snapshot into it is always correct
    /// regardless of what it previously held.
    parser: vt100::Parser,
    /// Last bytes actually fed to `parser`. A full copy rather than a
    /// hash: a false "unchanged" verdict would freeze the visible screen,
    /// and a `memcmp` is already far cheaper than the escape-sequence
    /// parse plus cell-grid rebuild it lets us skip.
    last_input: Vec<u8>,
    /// `vt100_to_buffer(parser.screen(), ..)` output, before the palette.
    raw: TerminalBuffer,
    /// `raw` with `apply_pty_palette` baked in, and the mode it was baked
    /// for -- what `project_current` actually hands back.
    painted: TerminalBuffer,
    painted_mode: PtyColorMode,

    /// Column width and color mode the cached rows below were projected
    /// for. Both invalidate the ENTIRE row cache on change (rewrap and
    /// recolor both apply to every row, not just the one being read).
    row_width: u16,
    row_mode: PtyColorMode,
    /// Index-aligned with `session.terminal_scrollback`. `None` means "not
    /// projected yet"; a row is never re-projected once `Some` (frozen
    /// scrollback -- see module doc comment), only dropped wholesale by a
    /// `row_width`/`row_mode` change.
    rows: Vec<Option<TerminalBuffer>>,
}

impl SessionCache {
    fn new(width: u16, height: u16) -> Self {
        Self {
            geometry: (width, height),
            parser: vt100::Parser::new(height, width, 0),
            last_input: Vec::new(),
            raw: TerminalBuffer::new(width, height),
            painted: TerminalBuffer::new(width, height),
            painted_mode: PtyColorMode::default(),
            row_width: width,
            row_mode: PtyColorMode::default(),
            rows: Vec::new(),
        }
    }

    /// Reprojects the live screen only when `input` or `(width, height)`
    /// actually changed; reapplies only the (cheap, single-pass) palette
    /// when just `mode` changed. A brand new entry's `last_input` is empty
    /// and `raw`/`painted` are already blank buffers matching a blank
    /// `vt100::Parser` -- the same state real input would produce anyway
    /// -- so the very first call self-corrects without needing a separate
    /// "not initialized yet" flag: real (non-empty) first input always
    /// differs from the empty sentinel and reprojects normally, and a
    /// legitimately-empty first input is already exactly what a blank
    /// buffer represents.
    fn refresh_current(&mut self, input: &[u8], width: u16, height: u16, mode: PtyColorMode) {
        let geometry_changed = self.geometry != (width, height);
        if geometry_changed {
            self.parser.screen_mut().set_size(height, width);
            self.geometry = (width, height);
            // Must happen before `vt100_to_buffer` below: it bounds every
            // write against `raw`'s OWN width/height, so a stale (smaller
            // or larger) size would silently drop or under-fill cells.
            self.raw.resize(width, height);
        }
        let input_changed = geometry_changed || self.last_input != input;
        if input_changed {
            // Blank the screen before replaying the snapshot. The bytes
            // that arrive here are a FULL screen, and vt100's own
            // `contents_formatted` does open with a clear -- but that is a
            // property of the data we happen to be sent over a wire, not
            // something this cache can enforce, and a reused parser that
            // trusts it silently accumulates the previous frame underneath
            // the new one the moment the property stops holding. Paying
            // three escape sequences per changed frame buys correctness
            // that does not depend on the sender.
            self.parser.process(SCREEN_RESET);
            self.parser.process(input);
            uzor_tui::vt100_to_buffer(self.parser.screen(), &mut self.raw);
            self.last_input = input.to_vec();
        }
        if input_changed || self.painted_mode != mode {
            self.painted = self.raw.clone();
            apply_pty_palette(&mut self.painted, mode);
            self.painted_mode = mode;
        }
    }

    /// Returns the cached projection of scrollback row `row_index`,
    /// computing it (once) if this is the first time it has been asked
    /// for, or if a width/mode change just invalidated the whole set.
    fn row(&mut self, row_index: usize, row_bytes: &[u8], width: u16, mode: PtyColorMode) -> &TerminalBuffer {
        if self.row_width != width || self.row_mode != mode {
            self.rows.clear();
            self.row_width = width;
            self.row_mode = mode;
        }
        if row_index >= self.rows.len() {
            self.rows.resize_with(row_index + 1, || None);
        }
        self.rows[row_index].get_or_insert_with(|| {
            // TWO rows, not one, for a screen we only ever read one row
            // from: a scrollback line longer than the panel is wide wraps,
            // and wrapping off the last row of a ONE-row vt100 screen
            // panics inside the crate itself (`vt100::grid`, subtraction
            // overflow while scrolling a screen with no row to scroll
            // into). A narrow panel plus one long history line was enough
            // to take the whole TUI down. The second row absorbs the wrap
            // and is then discarded, which is the same visible result as
            // the truncation this always did.
            let mut row_parser = vt100::Parser::new(2, width, 0);
            row_parser.process(row_bytes);
            let mut projected = TerminalBuffer::new(width, 2);
            uzor_tui::vt100_to_buffer(row_parser.screen(), &mut projected);
            let mut buffer = TerminalBuffer::new(width, 1);
            for column in 0..width {
                buffer.set(column, 0, projected.get(column, 0).clone());
            }
            apply_pty_palette(&mut buffer, mode);
            buffer
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(instance_id: u64) -> SessionAddress {
        SessionAddress {
            node_id: "node".to_owned(),
            workspace_id: "workspace".to_owned(),
            instance_id,
            generation: 0,
        }
    }

    /// Two frames with unchanged bytes/geometry/mode must read back the
    /// same cells -- the actual regression this cache exists to guard:
    /// a stale or wrongly-invalidated cache would either freeze the
    /// screen (never updating) or flicker/garble it (invalidating when it
    /// should not), and this pins the "steady state reads back exactly
    /// what was last projected" half of that contract.
    #[test]
    fn unchanged_input_reads_back_the_same_projection() {
        let mut cache = PtyRenderCache::default();
        let addr = address(1);
        let bytes = b"hello".to_vec();
        let first = cache.project_current(&addr, &bytes, 10, 3, PtyColorMode::Inherited).clone();
        let second = cache.project_current(&addr, &bytes, 10, 3, PtyColorMode::Inherited).clone();
        assert_eq!(first.get(0, 0), second.get(0, 0));
    }

    /// The other half: new bytes for the SAME session must actually
    /// change what is displayed, proving the cache invalidates on real
    /// change rather than freezing the panel (the owner's "cursor sticks,
    /// doesn't blink" complaint was exactly a symptom of stale display
    /// state -- a cache that never invalidates would reproduce it).
    #[test]
    fn changed_input_reprojects() {
        let mut cache = PtyRenderCache::default();
        let addr = address(2);
        let _ = cache.project_current(&addr, b"a", 10, 3, PtyColorMode::Inherited);
        let updated = cache
            .project_current(&addr, b"b", 10, 3, PtyColorMode::Inherited)
            .clone();
        assert_eq!(updated.get(0, 0).symbol.as_str(), "b");
    }

    /// A geometry change must reproject even with byte-identical input --
    /// the wrap/column layout depends on width, so serving a
    /// differently-sized-but-cached buffer would either panic downstream
    /// (size mismatch) or silently show the wrong dimensions.
    #[test]
    fn geometry_change_resizes_and_reprojects() {
        let mut cache = PtyRenderCache::default();
        let addr = address(3);
        let bytes = b"hi".to_vec();
        let _ = cache.project_current(&addr, &bytes, 10, 3, PtyColorMode::Inherited);
        let resized = cache.project_current(&addr, &bytes, 20, 5, PtyColorMode::Inherited);
        assert_eq!(resized.width(), 20);
        assert_eq!(resized.height(), 5);
    }

    /// A scrollback row is computed once and the SAME row index must keep
    /// reading back consistent content across repeated calls -- it is
    /// frozen the moment it scrolls off the live screen (the owner's own
    /// "never truncate history" mandate), so re-derivation on every call
    /// would be both wasted work and, if it ever diverged, a visible
    /// inconsistency in scrollback while scrolling.
    #[test]
    fn scrollback_row_projection_is_stable_across_calls() {
        let mut cache = PtyRenderCache::default();
        let addr = address(4);
        let row = b"archived line".to_vec();
        let first = cache.project_row(&addr, 0, &row, 12, PtyColorMode::Inherited).clone();
        let second = cache.project_row(&addr, 0, &row, 12, PtyColorMode::Inherited).clone();
        assert_eq!(first.get(0, 0), second.get(0, 0));
    }
}
