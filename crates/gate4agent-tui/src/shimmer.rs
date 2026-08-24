//! The hover "shimmer" text effect -- a cursor-proximity brush, not a
//! per-character decode cascade. See `docs/gate4agent/research/hover-
//! shimmer-and-animated-pet-spec-2026-08-24.md` section 1, the authority
//! for every number below. Every function here is a PURE function of
//! `(index, tick, pointer position)` -- no per-character state, no RNG, no
//! stored seed, matching the spec's own "the effect needs no memory
//! between frames" requirement (section 1.5/1.9) -- so `render.rs` can
//! recompute the whole displayed string from scratch every frame, and this
//! module can be unit-tested with zero engine/terminal/App dependency.

/// The exact 30-glyph substitution set (section 1.3): 10 ASCII digits then
/// 20 HALF-WIDTH katakana (each one column wide, unlike their full-width
/// counterparts, which would shift the layout every frame the spec's own
/// text explicitly warns against).
pub(crate) const SHIMMER_GLYPHS: [char; 30] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'ｱ', 'ｲ', 'ｳ', 'ｴ', 'ｵ', 'ｶ', 'ｷ', 'ｸ', 'ｹ',
    'ｺ', 'ｻ', 'ｼ', 'ｽ', 'ｾ', 'ｿ', 'ﾀ', 'ﾁ', 'ﾂ', 'ﾃ', 'ﾄ',
];

/// The clock quantum the effect re-rolls on (section 1.5): "derive an
/// integer `tick = floor(now_ms / 72)`". Exposed as its own constant so
/// `client::animation_wake_interval`'s own fast-cadence claim for a
/// hovered slot uses the SAME number this module quantizes on, rather than
/// two independently-typed 72s that could silently drift apart.
pub(crate) const SHIMMER_TICK_MILLIS: u64 = 72;

/// Brush radius (section 1.4): `R = 2.25` cells, so `R² = 5.0625`.
pub(crate) const SHIMMER_BRUSH_RADIUS_SQUARED: f64 = 5.0625;

/// `tick = floor(now_ms / 72)` -- a plain integer division already floors
/// for non-negative operands, so no explicit `.floor()` call is needed.
pub(crate) fn shimmer_tick_for_millis(now_millis: u64) -> u64 {
    now_millis / SHIMMER_TICK_MILLIS
}

/// The brush geometry only (section 1.4): whether the character cell at
/// `index` (0-based, local to the slot) falls inside the pointer's brush
/// circle. `cursor_col`/`cursor_row` are the pointer's position in the
/// SAME local coordinates (column 0 = the slot's own first cell; `row`
/// near 0 means the pointer sits on the slot's own row). Deliberately does
/// NOT check whether the underlying character is eligible (alphanumeric)
/// -- that is [`apply_shimmer_char`]'s job, kept separate so the brush's
/// own inclusion rule can be pinned by a test independent of any
/// particular character.
pub(crate) fn shimmer_brush_hits(index: usize, cursor_col: f64, cursor_row: f64) -> bool {
    let dx = (index as f64 + 0.5) - cursor_col;
    let dy = 0.5 - cursor_row;
    dx * dx + dy * dy <= SHIMMER_BRUSH_RADIUS_SQUARED
}

/// Combines `tick` and `i` via odd multiplicative constants, then runs a
/// splitmix64-style finalizer (three xor-shift/multiply rounds) for good
/// bit diffusion -- section 1.5's own "any hash with good bit diffusion
/// works; the requirement is only that the result depends on both `i` and
/// `tick` and is well spread across the 30 glyphs." No RNG, no stored
/// seed: a pure function of its two integer inputs.
fn shimmer_avalanche_hash(index: usize, tick: u64) -> u64 {
    const INDEX_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;
    const TICK_MULTIPLIER: u64 = 0xBF58_476D_1CE4_E5B9;
    let mut hash = (index as u64)
        .wrapping_mul(INDEX_MULTIPLIER)
        .wrapping_add(tick.wrapping_mul(TICK_MULTIPLIER));
    hash ^= hash >> 30;
    hash = hash.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    hash ^= hash >> 27;
    hash = hash.wrapping_mul(0x94D0_49BB_1331_11EB);
    hash ^= hash >> 31;
    hash
}

/// The glyph a scrambled character at `index` shows on quantum `tick`
/// (section 1.5) -- `hash mod 30` into [`SHIMMER_GLYPHS`].
pub(crate) fn shimmer_glyph_for(index: usize, tick: u64) -> char {
    let position = (shimmer_avalanche_hash(index, tick) % SHIMMER_GLYPHS.len() as u64) as usize;
    SHIMMER_GLYPHS[position]
}

/// The full per-character transform (sections 1.3-1.5): `ch` scrambles
/// only if it is an ASCII alphanumeric AND its cell falls inside the
/// brush -- punctuation, separators, spaces, and any non-ASCII character
/// are always passed through unchanged, so a clock keeps its colons and a
/// pet's face glyphs never scramble.
pub(crate) fn apply_shimmer_char(
    ch: char,
    index: usize,
    cursor_col: f64,
    cursor_row: f64,
    tick: u64,
) -> char {
    if !ch.is_ascii_alphanumeric() {
        return ch;
    }
    if !shimmer_brush_hits(index, cursor_col, cursor_row) {
        return ch;
    }
    shimmer_glyph_for(index, tick)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Section 1.4's inclusion rule is `d² ≤ R²` -- inclusive at the exact
    /// boundary, not a strict `<`. Pin both the exact-boundary "still
    /// scrambles" case and one epsilon past it ("no longer scrambles").
    #[test]
    fn brush_inclusion_is_inclusive_at_the_exact_radius_boundary() {
        // Character 0's cell centre is at local (0.5, 0.5). Put the
        // pointer on the same row (cy = 0.5, so dy = 0) and exactly
        // R = 2.25 cells to the left: d² == R² exactly.
        let cursor_row = 0.5;
        let cursor_col_at_boundary = 0.5 + 2.25;
        assert!(shimmer_brush_hits(0, cursor_col_at_boundary, cursor_row));

        // One thousandth of a cell further away must fall outside.
        let cursor_col_past_boundary = cursor_col_at_boundary + 0.001;
        assert!(!shimmer_brush_hits(0, cursor_col_past_boundary, cursor_row));
    }

    /// Section 1.4: punctuation/separators/spaces are never scrambled even
    /// when the brush sits directly on top of them (distance 0).
    #[test]
    fn non_alphanumeric_characters_never_scramble_even_at_brush_centre() {
        for ch in [':', '.', '/', ' ', '-', '_', '°'] {
            assert_eq!(
                apply_shimmer_char(ch, 3, 3.5, 0.5, 42),
                ch,
                "expected {ch:?} to pass through unchanged"
            );
        }
    }

    /// An eligible (ASCII alphanumeric) character under the brush is
    /// replaced by EXACTLY `shimmer_glyph_for`'s own deterministic output
    /// for that `(index, tick)` -- not merely "some other character."
    #[test]
    fn alphanumeric_characters_under_the_brush_use_the_deterministic_glyph() {
        let index = 3;
        let tick = 42;
        let expected = shimmer_glyph_for(index, tick);
        assert_eq!(apply_shimmer_char('A', index, 3.5, 0.5, tick), expected);
        assert_eq!(apply_shimmer_char('7', index, 3.5, 0.5, tick), expected);
    }

    /// Pure function: the exact same `(index, tick)` always yields the
    /// exact same glyph, with no hidden RNG/seed/frame-counter state.
    #[test]
    fn glyph_selection_is_a_pure_deterministic_function_of_index_and_tick() {
        for index in 0..8 {
            for tick in 0..8 {
                assert_eq!(shimmer_glyph_for(index, tick), shimmer_glyph_for(index, tick));
            }
        }
    }

    /// Section 1.5: "well spread across the 30 glyphs" -- sampling a
    /// single index across many successive ticks must eventually visit
    /// every glyph in the set, not collapse onto a handful.
    #[test]
    fn glyph_selection_is_spread_across_every_glyph_in_the_set() {
        let mut seen = HashSet::new();
        for tick in 0..2000u64 {
            seen.insert(shimmer_glyph_for(5, tick));
        }
        assert_eq!(seen.len(), SHIMMER_GLYPHS.len(), "did not cover every glyph: {seen:?}");
    }

    #[test]
    fn tick_quantizes_wall_clock_milliseconds_to_seventy_two_ms_steps() {
        assert_eq!(shimmer_tick_for_millis(0), 0);
        assert_eq!(shimmer_tick_for_millis(71), 0);
        assert_eq!(shimmer_tick_for_millis(72), 1);
        assert_eq!(shimmer_tick_for_millis(143), 1);
        assert_eq!(shimmer_tick_for_millis(144), 2);
    }
}
