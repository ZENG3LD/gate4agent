//! Baked codicon bitmaps for this crate's icon catalog -- the full
//! ~57-icon set (see [`IconId`]), not just the activity rail's original
//! 7. Offline-rasterized, raw RGBA8 bytes checked straight into the
//! crate via `include_bytes!` -- no `usvg`/`resvg`/`uzor-icon` runtime or
//! build dependency. Every icon is baked in ONE raster tier (see
//! `render::render_rail_button` / `app::RailIcons`), switched globally
//! by the owner's own preference, not fixed per button, plus a plain
//! ASCII text tier:
//!
//! - Sixel (`RailIcons::Sixel`): a real raster image written directly to
//!   the terminal by `client::run`'s post-flush hook -- see [`sixel`].
//! - Ascii (`RailIcons::Ascii`): a short (<=2 char) plain-text label --
//!   see [`ascii`]. The activity rail itself still paints its OWN
//!   `render::RailButton::ascii` literal in this mode rather than
//!   calling [`ascii`] (see "Wiring status" below); that field's values
//!   match what [`ascii`] returns for the same 7 icons regardless.
//!
//! A third tier, Braille (a `uzor_tui::canvas::PixelCanvas` painted
//! straight into the cell buffer), shipped earlier and was removed
//! outright on the owner's own order: braille's fixed 2x4 dots/cell
//! density means the strip tier's own 2x1-cell button footprint is just
//! a 4x4 dot grid -- nothing left to improve, unusably low quality. Same
//! fate as the still-earlier half-block tier (see `app::RailIcons`'s own
//! doc comment).
//!
//! ## Wiring status
//!
//! Only the activity rail's original 7 icons (`Files`, `SourceControl`,
//! `Person`, `Project`, `SettingsGear`, `ChevronLeft`, `ChevronRight`)
//! are drawn anywhere today, via `render::render_activity_rail`'s own
//! `RailButton::icon: icons::IconId` field -- unchanged from before this
//! catalog generalized beyond the rail, just re-pointed at the wider
//! [`IconId`] enum so there is exactly one icon source in this crate
//! (there used to be a rail-only `RailIconId`; it no longer exists).
//! Every other [`IconId`] variant is baked and unit-tested (see this
//! module's own `tests`) but not yet drawn at any UI site -- a
//! deliberate, scoped-out next slice, not an oversight.
//!
//! ## Source, licence, regeneration
//!
//! Source: microsoft/vscode-codicons (MIT licence,
//! <https://github.com/microsoft/vscode-codicons>). Every icon's
//! `fill="currentColor"` is patched to `#cdd6f4` -- this crate's own
//! `pty_palette::GATE_FG` (the off-white already used for unselected rail
//! glyph/ascii labels) -- before rasterizing, so every tier reads as part
//! of the existing theme instead of an arbitrary new color. `resvg`
//! writes straight (non-premultiplied) alpha -- verified against this
//! exact resvg build by inspecting a partially-transparent output
//! pixel's own RGB channels, which stayed at the flat fill color
//! regardless of alpha -- so every "ink" source pixel below can be read
//! as (a flat icon color, a coverage/antialiasing weight) without an
//! un-premultiply step.
//!
//! Every `.rgba` asset under `src/icons/` and the generated
//! `src/icons/catalog.rs` module (see [`catalog`]) are produced by
//! `tools/bake_icons.py` (Python 3 + `resvg` 0.47 CLI + `ffmpeg` --
//! neither a dependency of this crate; both run once, offline). Re-run:
//!
//! ```text
//! python tools/bake_icons.py --force
//! ```
//!
//! That tool's own header doc comment has the full pipeline (the exact
//! `resvg`/`ffmpeg` filter graphs, byte-for-byte); this module doc does
//! not reproduce it, to avoid the two drifting apart. Every icon --
//! including the activity rail's original 7 -- is baked through this
//! SAME single pipeline; there is no separate frozen-legacy-asset path
//! (there used to be one, replaying the original 7's own hand-picked
//! braille alpha thresholds unchanged; it was retired along with the
//! braille tier itself).
//!
//! ## Sixel background variants ([`SixelVariant`])
//!
//! `icy_sixel`'s own encoder applies a hard alpha>=128 opacity threshold
//! per pixel with NO blend information in the encoded stream at all --
//! every surviving pixel carries only its flat, un-blended ink colour, so
//! anti-aliasing cannot survive this encoder in alpha, only in RGB (see
//! `tools/bake_icons.py`'s own header doc comment, cause 1, for the full
//! diagnosis). The fix is to composite every rail/strip/gallery sixel-tier
//! icon over the EXACT background colour its button paints, fully opaque,
//! so there is no transparent pixel left for the encoder's threshold to
//! drop and the anti-aliasing this buys back rides in RGB instead, where
//! that threshold cannot touch it. This USED to apply only where the
//! background was a fixed, known constant (`PtyColorMode::GateOverride`'s
//! own theme) -- `PtyColorMode::Inherited` read as having no knowable
//! background at bake OR render time (crossterm has no reliable query,
//! the same epistemic gap [`ASSUMED_CELL_WIDTH_PX`]'s own doc comment
//! already names for cell-pixel size). That premise was the actual bug:
//! the background was never unknowable at render time, only un-painted.
//! `render::render_rail_button`/`render_control_strip_button`/the icon
//! gallery swatches now paint an EXPLICIT truecolor background
//! unconditionally, in every `PtyColorMode` -- the button states what its
//! own background is instead of leaving it to the terminal, so it is
//! always known at bake time too, and there is exactly one composited
//! variant per background to request. [`SixelVariant`] now only spans
//! those backgrounds (`GateActive`/`GateAccent`) -- there is no more
//! `Transparent` case, and no more per-`PtyColorMode` branch, for the
//! rail/strip/gallery tiers this covers. The compact tier stays out of
//! scope (see `render::render_compact_icon_button`'s own doc comment) and
//! keeps real transparency, encoded with `BackgroundMode::Transparent`,
//! since it has no single known background to paint at all. The ascii
//! tier needs no equivalent of any of this: it is plain themed text drawn
//! directly with the button's own background style, not a raster image.
//!
//! ## Lucide (a second family, alongside codicons -- [`IconFamily`])
//!
//! Everything above describes codicons, which stay exactly as they are
//! and stay the default (`IconFamily::Codicons`, see that type's own doc
//! comment in `app.rs`). Lucide (<https://lucide.dev>,
//! <https://github.com/lucide-icons/lucide>) is baked ALONGSIDE it, at
//! the owner's own request, specifically because it is a STROKE-based
//! family: every glyph is an unfilled outline (`fill="none"`) whose ink
//! lives entirely in one `stroke-width` attribute, unlike codicons' filled
//! outlines (stroke weight baked into each path's own geometry, no
//! separate width knob at all) -- so Lucide is the one family this crate
//! can actually offer a thickness comparison against.
//!
//! Licence: read directly from lucide-icons/lucide's own `LICENSE` file at
//! authoring time (do not assume): ISC (Copyright (c) 2026 Lucide Icons
//! and Contributors) for the set as a whole, PLUS MIT (Copyright (c)
//! 2013-present Cole Bemis) for a named subset derived from the Feather
//! project -- both permissive, both permit shipping a derived raster
//! under this crate's own licence, same as codicons' MIT terms above; see
//! `tools/bake_icons.py`'s own header doc comment for the full text and
//! `src/icons/catalog.rs`'s own generated header for the same attribution
//! restated next to the code it covers.
//!
//! Mapping: every [`IconId`] maps to AT MOST one Lucide slug
//! (`tools/bake_icons.py::LUCIDE_SLUGS`, 55 of 57) -- [`lucide_slug`]
//! exposes that mapping at runtime. The two icons with no Lucide glyph
//! that carries the SAME MEANING (`CircleFilled` -- Lucide ships no
//! solid-fill glyph at all, a style gap, not a naming one; `RunAll` -- no
//! Lucide glyph distinctly means "run everything" rather than colliding
//! with `Play`'s own meaning) are a REPORTED gap
//! (`tools/bake_icons.py::LUCIDE_GAPS`), never an approximate
//! substitution -- [`sixel_lucide`]/[`sixel_strip_lucide`]/
//! [`sixel_gallery_lucide`]/[`sixel_compact_lucide`] return `None` for
//! them rather than silently resolving to something the wrong shape
//! implies. `render::render_gallery_size_swatch` is the one call site
//! that can actually observe a `None` today (the icon gallery includes
//! both gap icons specifically so the gap itself is visible, not just
//! documented) -- it paints a plain `n/a` label instead of a placement.
//!
//! Stroke lattice: Lucide's viewBox is a fixed 24 units (vs codicons' own
//! 16- or 24-unit grid, see this module's own cause-6 doc section above),
//! and every tier already renders its glyph at a WHOLE multiple of 16px
//! before padding onto the tier's own canvas (`tools/bake_icons.py::
//! glyph_lattice_size`/`rasterize_lattice_fit`, reused UNCHANGED for
//! Lucide -- only the fetch/patch step differs, see `tools/
//! bake_icons.py`'s own header). [`LUCIDE_STROKE_WIDTH`] (1.5, in Lucide's
//! own 24-unit source space) is chosen so that render lands on a whole
//! device pixel at all three glyph sizes at once: 16px (strip) -> 1.5 *
//! 16/24 = 1px; 32px (rail) -> 1.5 * 32/24 = 2px; 48px (gallery) -> 1.5 *
//! 48/24 = 3px -- ONE authored width, three whole-pixel strokes, verified
//! on the actual baked bytes by this module's own `lucide_stroke_lands_
//! on_whole_pixels_at_every_tier` test below, not just asserted by the
//! arithmetic. `app::LucideStrokeWidth` is the owner-facing Settings
//! knob this constant feeds -- see that type's own doc comment for why
//! only this ONE value is offered today.

use icy_sixel::{BackgroundMode, EncodeOptions, SixelImage};

use crate::app::IconFamily;

mod catalog;

pub use catalog::{ascii, lucide_slug, sixel, sixel_compact, sixel_gallery, sixel_strip, IconId};

/// Hand-synced with `tools/bake_icons.py::LUCIDE_STROKE_WIDTH` -- see
/// this module's own "Lucide" doc section above for the exact arithmetic
/// this value's stroke lattice depends on. Not read by the bake tool (a
/// separate, Python-side copy feeds the actual SVG patch -- same "no
/// shared source of truth across the Python/Rust boundary" precedent
/// every other pixel-size constant pair in that tool already has, see
/// its own header doc comment); this copy exists so this crate's own
/// tests can verify the whole-pixel claim against the real baked bytes
/// without hand-copying the number a second time into a test literal.
pub const LUCIDE_STROKE_WIDTH: f32 = 1.5;

/// Resolves `id`'s rail-tier sixel string in `family` -- `Codicons`
/// always resolves ([`sixel`] has no gap to report); `Lucide` resolves
/// via [`catalog::sixel_lucide`], `None` for the two documented mapping
/// gaps (see this module's own "Lucide" doc section). The ONLY place
/// `family` actually changes which catalog a sixel-tier placement reads
/// from -- `client::flush_sixel_icon_into` calls this (and its `_strip`/
/// `_gallery`/`_compact` siblings below) instead of the bare, codicon-
/// only [`sixel`]/[`sixel_strip`]/[`sixel_gallery`]/[`sixel_compact`],
/// which stay exactly as they were (still used directly by this crate's
/// own pre-existing tests) for exactly that reason: adding a family
/// dimension must never change what a codicon-only call already resolved
/// to, byte for byte.
pub fn sixel_family(id: IconId, family: IconFamily, variant: SixelVariant) -> Option<&'static str> {
    match family {
        IconFamily::Codicons => Some(sixel(id, variant)),
        IconFamily::Lucide => catalog::sixel_lucide(id, variant),
    }
}

/// Strip-tier equivalent of [`sixel_family`] -- see that function's own
/// doc comment.
pub fn sixel_strip_family(id: IconId, family: IconFamily, variant: SixelVariant) -> Option<&'static str> {
    match family {
        IconFamily::Codicons => Some(sixel_strip(id, variant)),
        IconFamily::Lucide => catalog::sixel_strip_lucide(id, variant),
    }
}

/// Gallery-tier equivalent of [`sixel_family`] -- see that function's own
/// doc comment.
pub fn sixel_gallery_family(id: IconId, family: IconFamily, variant: SixelVariant) -> Option<&'static str> {
    match family {
        IconFamily::Codicons => Some(sixel_gallery(id, variant)),
        IconFamily::Lucide => catalog::sixel_gallery_lucide(id, variant),
    }
}

/// Compact-tier equivalent of [`sixel_family`] -- no `SixelVariant`, same
/// reason [`sixel_compact`] has none (real transparency, no pre-
/// composited background variant to pick between).
pub fn sixel_compact_family(id: IconId, family: IconFamily) -> Option<&'static str> {
    match family {
        IconFamily::Codicons => Some(sixel_compact(id)),
        IconFamily::Lucide => catalog::sixel_compact_lucide(id),
    }
}

/// Which pre-baked, fully-opaque background a rail/strip/gallery sixel-
/// tier icon asset was composited against -- see this module's own
/// "Sixel background variants" doc section above for the full diagnosis
/// and fix. Every icon-bearing button now paints one of these two
/// backgrounds EXPLICITLY, in every `PtyColorMode` -- there is no more
/// mode-dependent case, so a render call site picks a variant purely from
/// the button's own `selected` state (`GateAccent` when selected,
/// `GateActive` otherwise), never from `app.color_mode`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SixelVariant {
    /// The icon-bearing button's own fixed "at rest" colour
    /// (`render::ACTIVE_BG`) -- pre-composited fully opaque at bake time.
    GateActive,
    /// The icon-bearing button's own fixed "selected" accent colour
    /// (`render::MAUVE`) -- pre-composited fully opaque at bake time.
    /// Only the activity rail's own selected state ever requests this;
    /// [`sixel_strip`] folds it into the same asset as `GateActive` since
    /// the control-plane strip has no selected state at all (see
    /// `render::render_control_strip_button`'s own doc comment).
    GateAccent,
}

/// Assumed terminal cell size in pixels (Cascadia Mono 12pt, Windows
/// Terminal) -- the basis every sixel tier's own pixel target is derived
/// from. Not read at runtime (crossterm has no reliable cell-pixel
/// probe); retuning either constant means re-rasterizing every `.rgba`
/// sixel-tier asset at the new target size via `tools/bake_icons.py`, not
/// just editing a number here.
///
/// Measured directly against the owner's own Windows Terminal window (a
/// 1129x635 window: the rail's 6 columns span 60px -- 10.0px/col -- and
/// four consecutive gallery rows span 76px -- 19.0px/row), NOT the
/// earlier assumed 10x20: a body sized off an over-estimated cell height
/// overflows its own row budget and bleeds into the terminal row below
/// it (unsafe -- visible ghosting/misalignment against whatever that
/// next row paints); a body sized off an under-estimate merely leaves an
/// unused blank pixel row inside its own last cell (safe). Every tier
/// below is therefore FLOOR-rounded to a whole multiple of this height,
/// never rounded up -- see [`SIXEL_ICON_HEIGHT_PX`]'s own doc comment for
/// the one tier this floor-rounding makes non-square.
pub const ASSUMED_CELL_WIDTH_PX: u32 = 10;
pub const ASSUMED_CELL_HEIGHT_PX: u32 = 19;

// ---- Sixel tier -----------------------------------------------------

/// Rail-tier sixel icon footprint, in whole assumed terminal cells --
/// "keep 4 cells wide x 2 rows tall, make switching either number a
/// one-line change" (see `render::render_activity_rail`'s own geometry
/// doc comment). `render::render_activity_rail`'s own button-body height
/// derives DIRECTLY from [`SIXEL_ICON_CELLS_TALL`], so changing either
/// constant here is the entire rail-size swap; the only other step is
/// re-rasterizing this tier's own `.rgba` assets at the new size via
/// `tools/bake_icons.py`. The icon gallery (`app::SurfaceTab::
/// IconGallery`) renders the strip/rail/gallery columns side by side (see
/// this module's own "Gallery tier" section below) so the owner can judge
/// an alternative before ever touching these constants.
pub const SIXEL_ICON_CELLS_WIDE: u16 = 4;
pub const SIXEL_ICON_CELLS_TALL: u16 = 2;
/// Baked bitmap pixel width: [`SIXEL_ICON_CELLS_WIDE`] whole
/// [`ASSUMED_CELL_WIDTH_PX`] cells.
pub const SIXEL_ICON_WIDTH_PX: u32 = ASSUMED_CELL_WIDTH_PX * 4;
/// Baked bitmap pixel height: [`SIXEL_ICON_CELLS_TALL`] whole
/// [`ASSUMED_CELL_HEIGHT_PX`] cells -- deliberately NOT equal to
/// [`SIXEL_ICON_WIDTH_PX`] (38 vs 40) even though every source codicon is
/// square: floor-rounding a 40-tall icon to whole 19px cells lands on 38,
/// not back up to 40 (see [`ASSUMED_CELL_HEIGHT_PX`]'s own doc comment on
/// why floor, never ceiling). `tools/bake_icons.py::rasterize_sixel` fits
/// a source icon within this non-square box by hand for exactly the
/// reason `rasterize_strip_sixel`/`rasterize_gallery_sixel` already did.
pub const SIXEL_ICON_HEIGHT_PX: u32 = ASSUMED_CELL_HEIGHT_PX * 2;

// ---- Compact tier -------------------------------------------------------
//
// For dense, single-row inline buttons (Explorer/Git sidebar panel lists
// and their modals -- `render::render_compact_icon_button`) where the
// rail's own 4-cell x 2-row icon does not fit next to a text label in the
// SAME row. Baked as its own separate, much smaller raster per icon (not
// a runtime downscale of the rail-tier asset) by `tools/bake_icons.py`'s
// own `rasterize_compact_sixel`.

/// Compact sixel-tier bitmap pixel size: exactly ONE assumed terminal
/// cell (see `ASSUMED_CELL_WIDTH_PX`/`ASSUMED_CELL_HEIGHT_PX` above).
pub const COMPACT_SIXEL_ICON_WIDTH_PX: u32 = ASSUMED_CELL_WIDTH_PX;
pub const COMPACT_SIXEL_ICON_HEIGHT_PX: u32 = ASSUMED_CELL_HEIGHT_PX;
pub const COMPACT_SIXEL_ICON_CELLS_WIDE: u16 = 1;
pub const COMPACT_SIXEL_ICON_CELLS_TALL: u16 = 1;

// ---- Strip tier -----------------------------------------------------
//
// For the sidebar content panels' own control-plane strip (`render::
// render_control_strip`/`render_control_strip_button`) -- 2 cells wide x
// 1 row tall, roughly a quarter the rail tier's own area. Baked as its
// own separate raster per icon (single-pass resvg AA directly at this
// target size, same recipe as the rail/compact tiers -- see `tools/
// bake_icons.py`'s own `rasterize_strip_sixel`), not a runtime downscale
// of the rail tier's own asset.

/// Strip sixel-tier bitmap pixel size: 2 assumed terminal cells wide x 1
/// row tall (see `ASSUMED_CELL_WIDTH_PX`/`ASSUMED_CELL_HEIGHT_PX` above).
pub const STRIP_SIXEL_ICON_WIDTH_PX: u32 = ASSUMED_CELL_WIDTH_PX * 2;
pub const STRIP_SIXEL_ICON_HEIGHT_PX: u32 = ASSUMED_CELL_HEIGHT_PX;
pub const STRIP_SIXEL_ICON_CELLS_WIDE: u16 = 2;
pub const STRIP_SIXEL_ICON_CELLS_TALL: u16 = 1;

// ---- Gallery tier -----------------------------------------------------
//
// The icon gallery dev surface (`app::SurfaceTab::IconGallery`, `render::
// render_icon_gallery`) shows every icon at all three sizes FIX2 landed
// on side by side -- the strip tier's own pixel size (reused as-is), the
// rail tier's own pixel size (reused as-is), and a third size no other UI
// site needs and so has no existing bake. This is that third size's own
// dedicated raster (single-pass resvg AA directly at the target size,
// same recipe as the rail/strip/compact tiers -- see `tools/
// bake_icons.py`'s own `rasterize_gallery_sixel`), not a runtime upscale
// of the rail asset (which would just blur the existing raster, defeating
// the whole point of a size comparison).

/// Gallery sixel-tier bitmap pixel size: 6 assumed terminal cells wide x
/// 3 rows tall (see `ASSUMED_CELL_WIDTH_PX`/`ASSUMED_CELL_HEIGHT_PX`
/// above) -- the third of FIX2's three evenly-landing sizes.
pub const GALLERY_SIXEL_ICON_WIDTH_PX: u32 = ASSUMED_CELL_WIDTH_PX * 6;
pub const GALLERY_SIXEL_ICON_HEIGHT_PX: u32 = ASSUMED_CELL_HEIGHT_PX * 3;
pub const GALLERY_SIXEL_ICON_CELLS_WIDE: u16 = 6;
pub const GALLERY_SIXEL_ICON_CELLS_TALL: u16 = 3;

/// Every baked icon in this crate is encoded with this SAME small,
/// explicit, non-dithered palette -- "encode with a small explicit
/// palette" per this task's own brief (see `tools/bake_icons.py`'s own
/// header doc comment's cause-1 note). A composited icon's true colour
/// count is just its own number of distinct alpha/coverage levels
/// (measured on real baked assets at authoring time: 6-26 across this
/// crate's manifest), so 32 is generous headroom, not a visible
/// compression; Floyd-Steinberg dithering (the library's own default) is
/// switched off outright rather than tuned down, since it exists to fake
/// extra apparent colours via spatial noise for photographic content and
/// only ever adds speckle noise to a flat-colour UI glyph like these.
fn icon_encode_options() -> EncodeOptions {
    EncodeOptions {
        max_colors: 32,
        diffusion: 0.0,
        ..EncodeOptions::default()
    }
}

/// Same source pixels as [`build_sixel_gate`], for a `SixelVariant::
/// GateActive`/`GateAccent` asset that was already pre-composited fully
/// opaque at bake time (see [`SixelVariant`]'s own doc comment) --
/// `BackgroundMode::Opaque` here is a documentation choice, not a
/// functional requirement: every pixel in such a buffer already has alpha
/// 255, so `icy_sixel`'s own encoder would treat it identically either
/// way (see `tools/bake_icons.py`'s own header doc comment for why).
pub(crate) fn build_sixel_gate(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, SIXEL_ICON_WIDTH_PX, SIXEL_ICON_HEIGHT_PX, BackgroundMode::Opaque)
}

/// Same encoding as [`build_sixel`], for the compact tier's own smaller
/// per-icon asset (see this module's own "Compact tier" section above).
pub(crate) fn build_sixel_compact(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, COMPACT_SIXEL_ICON_WIDTH_PX, COMPACT_SIXEL_ICON_HEIGHT_PX, BackgroundMode::Transparent)
}

/// Same relationship [`build_sixel_gate`] has to the rail tier, for the
/// strip tier's own pre-composited asset -- the ONLY strip-tier sixel
/// this crate ships (see [`SixelVariant`]'s own doc comment).
pub(crate) fn build_sixel_strip_gate(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, STRIP_SIXEL_ICON_WIDTH_PX, STRIP_SIXEL_ICON_HEIGHT_PX, BackgroundMode::Opaque)
}

/// Same relationship [`build_sixel_gate`] has to the rail tier, for the
/// gallery tier's own pre-composited asset -- composited over the exact
/// background `render::render_icon_gallery` paints each swatch cell with
/// (`render::ACTIVE_BG`, the same fixed colour the rail/strip "at rest"
/// bodies already use), per this tier's own "composite over the exact
/// background the gallery paints" brief.
pub(crate) fn build_sixel_gallery_gate(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, GALLERY_SIXEL_ICON_WIDTH_PX, GALLERY_SIXEL_ICON_HEIGHT_PX, BackgroundMode::Opaque)
}

fn build_sixel_sized(rgba: &[u8], width: u32, height: u32, background_mode: BackgroundMode) -> String {
    let image = SixelImage::try_from_rgba(rgba.to_vec(), width as usize, height as usize).expect(
        "every sixel-tier .rgba asset's byte length is asserted against its own tier's \
         WIDTH_PX * HEIGHT_PX * 4 by this module's own unit tests -- a mismatch here means a \
         baked asset was regenerated at a different size without updating these constants, a \
         build-time asset/constant drift, not a runtime condition",
    );
    image
        .with_background_mode(background_mode)
        .encode_with(&icon_encode_options())
        .expect("encoding a fixed, already-validated, in-memory RGBA buffer to SIXEL does not fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sixel_rgba_matches_its_own_declared_dimensions() {
        let expected = (SIXEL_ICON_WIDTH_PX * SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::sixel_source_rgba(id).len(), expected, "{id:?} sixel rgba length");
        }
    }

    const SIXEL_VARIANTS: [SixelVariant; 2] = [SixelVariant::GateActive, SixelVariant::GateAccent];

    #[test]
    fn every_icon_resolves_in_every_tier_without_panicking() {
        for id in IconId::ALL {
            for variant in SIXEL_VARIANTS {
                let _sixel = sixel(id, variant);
            }
            let _ascii = ascii(id);
        }
    }

    #[test]
    fn every_sixel_encodes_to_a_non_empty_dcs_sequence() {
        for id in IconId::ALL {
            for variant in SIXEL_VARIANTS {
                let encoded = sixel(id, variant);
                assert!(encoded.starts_with('\u{1b}'), "{id:?}/{variant:?} sixel output must start with the DCS introducer ESC");
                assert!(encoded.len() > 16, "{id:?}/{variant:?} sixel output for a real icon with real ink must not be a near-empty stub");
            }
        }
    }

    /// Cause 1's actual fix, locked in at the asset level: a `GateActive`/
    /// `GateAccent` rail-tier source buffer must be FULLY opaque (every
    /// alpha byte 255) -- there must be no transparent pixel left at all
    /// for `icy_sixel`'s own hard alpha threshold or Windows Terminal's
    /// own lack of sixel transparency support to mishandle (see this
    /// module's own "Sixel background variants" doc section).
    #[test]
    fn every_gate_composited_sixel_source_is_fully_opaque() {
        for id in IconId::ALL {
            for (label, rgba) in [
                ("gate_active", catalog::sixel_gate_active_source_rgba(id)),
                ("gate_accent", catalog::sixel_gate_accent_source_rgba(id)),
                ("strip_gate", catalog::sixel_strip_gate_source_rgba(id)),
                ("gallery_gate", catalog::sixel_gallery_gate_source_rgba(id)),
            ] {
                assert!(
                    rgba.chunks_exact(4).all(|px| px[3] == 255),
                    "{id:?}'s {label} source must be fully opaque (every alpha byte 255)"
                );
            }
        }
    }

    /// The raw pre-composite source bytes (test-only -- see `tools/
    /// bake_icons.py::ensure_assets`'s own doc comment for why these are
    /// no longer a shipped `SixelVariant`) are UNTOUCHED by the gate-
    /// compositing pass -- still real TRUE coverage (at least one alpha
    /// byte below 255), never accidentally overwritten with a composited
    /// copy or pre-thresholded before `composite_over_background` ever
    /// sees it. This is what [`gate_compositing_matches_the_background_
    /// and_ink_colours_exactly_at_full_coverage`] below depends on: its
    /// own "uncovered"/"fully covered" cases mean nothing if this source
    /// never actually has a partial-coverage pixel in between.
    #[test]
    fn raw_precomposite_sources_still_carry_real_coverage_variation() {
        for id in IconId::ALL {
            assert!(
                catalog::sixel_source_rgba(id).chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s rail raw source must still have partial-coverage pixels"
            );
            assert!(
                catalog::sixel_strip_source_rgba(id).chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s strip raw source must still have partial-coverage pixels"
            );
            assert!(
                catalog::sixel_gallery_source_rgba(id).chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s gallery raw source must still have partial-coverage pixels"
            );
        }
    }

    /// The actual "over" compositing arithmetic `tools/bake_icons.py::
    /// composite_over_background` performs, verified pixel-for-pixel
    /// against the real baked assets rather than trusted by construction:
    /// wherever the raw source is fully uncovered (alpha 0), the
    /// composited pixel must be EXACTLY the flat background colour;
    /// wherever it is fully covered (alpha 255), the composited pixel
    /// must be EXACTLY the source's own (already `#cdd6f4`-tinted, per
    /// `patch_fill`) ink colour, unchanged. This holds for the exact raw
    /// bytes `composite_over_background` was actually handed at bake time
    /// (see `tools/bake_icons.py::ensure_assets`'s own doc comment for why
    /// that source is checked in unmodified), not just for some
    /// hypothetical buffer.
    #[test]
    fn gate_compositing_matches_the_background_and_ink_colours_exactly_at_full_coverage() {
        // Hand-synced to render.rs's own (private) ACTIVE_BG/MAUVE
        // constants and tools/bake_icons.py's own GATE_ACTIVE_BG_RGB/
        // GATE_ACCENT_BG_RGB -- see this module's own "Sixel background
        // variants" doc section.
        const GATE_ACTIVE_BG: (u8, u8, u8) = (30, 30, 46);
        const GATE_ACCENT_BG: (u8, u8, u8) = (203, 166, 247);

        fn assert_matches_at_extremes(id: IconId, label: &str, source: &[u8], composited: &[u8], bg: (u8, u8, u8)) {
            assert_eq!(source.len(), composited.len(), "{id:?}/{label} source/composited length mismatch");
            for (source_px, composited_px) in source.chunks_exact(4).zip(composited.chunks_exact(4)) {
                match source_px[3] {
                    0 => assert_eq!(
                        (composited_px[0], composited_px[1], composited_px[2]),
                        bg,
                        "{id:?}/{label}: an uncovered source pixel must composite to the flat background colour exactly"
                    ),
                    255 => assert_eq!(
                        (composited_px[0], composited_px[1], composited_px[2]),
                        (source_px[0], source_px[1], source_px[2]),
                        "{id:?}/{label}: a fully-covered source pixel's ink colour must survive compositing unchanged"
                    ),
                    _ => {}
                }
            }
        }

        for id in IconId::ALL {
            assert_matches_at_extremes(id, "rail_gate_active", catalog::sixel_source_rgba(id), catalog::sixel_gate_active_source_rgba(id), GATE_ACTIVE_BG);
            assert_matches_at_extremes(id, "rail_gate_accent", catalog::sixel_source_rgba(id), catalog::sixel_gate_accent_source_rgba(id), GATE_ACCENT_BG);
            assert_matches_at_extremes(id, "strip_gate", catalog::sixel_strip_source_rgba(id), catalog::sixel_strip_gate_source_rgba(id), GATE_ACTIVE_BG);
            assert_matches_at_extremes(id, "gallery_gate", catalog::sixel_gallery_source_rgba(id), catalog::sixel_gallery_gate_source_rgba(id), GATE_ACTIVE_BG);
        }
    }

    #[test]
    fn every_gate_sixel_rgba_matches_its_own_declared_dimensions() {
        let expected = (SIXEL_ICON_WIDTH_PX * SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::sixel_gate_active_source_rgba(id).len(), expected, "{id:?} gate_active rgba length");
            assert_eq!(catalog::sixel_gate_accent_source_rgba(id).len(), expected, "{id:?} gate_accent rgba length");
        }
    }

    #[test]
    fn every_strip_sixel_rgba_matches_its_own_declared_dimensions() {
        let expected = (STRIP_SIXEL_ICON_WIDTH_PX * STRIP_SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::sixel_strip_source_rgba(id).len(), expected, "{id:?} strip rgba length");
            assert_eq!(catalog::sixel_strip_gate_source_rgba(id).len(), expected, "{id:?} strip_gate rgba length");
        }
    }

    #[test]
    fn every_icon_resolves_in_the_strip_tier_without_panicking() {
        for id in IconId::ALL {
            for variant in SIXEL_VARIANTS {
                let _strip = sixel_strip(id, variant);
            }
        }
    }

    #[test]
    fn every_strip_sixel_encodes_to_a_non_empty_dcs_sequence() {
        for id in IconId::ALL {
            for variant in SIXEL_VARIANTS {
                let encoded = sixel_strip(id, variant);
                assert!(encoded.starts_with('\u{1b}'), "{id:?}/{variant:?} strip sixel output must start with the DCS introducer ESC");
            }
        }
    }

    /// [`sixel_strip`]'s own documented fold: the strip tier has no
    /// selected state at all, so `GateAccent` must resolve to the exact
    /// same bytes as `GateActive`, byte-for-byte, not a distinct asset.
    #[test]
    fn strip_gate_accent_folds_into_the_same_bytes_as_gate_active() {
        for id in IconId::ALL {
            assert_eq!(sixel_strip(id, SixelVariant::GateAccent), sixel_strip(id, SixelVariant::GateActive), "{id:?}");
        }
    }

    #[test]
    fn every_gallery_sixel_rgba_matches_its_own_declared_dimensions() {
        let expected = (GALLERY_SIXEL_ICON_WIDTH_PX * GALLERY_SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::sixel_gallery_source_rgba(id).len(), expected, "{id:?} gallery rgba length");
            assert_eq!(catalog::sixel_gallery_gate_source_rgba(id).len(), expected, "{id:?} gallery_gate rgba length");
        }
    }

    #[test]
    fn every_icon_resolves_in_the_gallery_tier_without_panicking() {
        for id in IconId::ALL {
            for variant in SIXEL_VARIANTS {
                let _gallery = sixel_gallery(id, variant);
            }
        }
    }

    #[test]
    fn every_gallery_sixel_encodes_to_a_non_empty_dcs_sequence() {
        for id in IconId::ALL {
            for variant in SIXEL_VARIANTS {
                let encoded = sixel_gallery(id, variant);
                assert!(encoded.starts_with('\u{1b}'), "{id:?}/{variant:?} gallery sixel output must start with the DCS introducer ESC");
            }
        }
    }

    /// Same fold as [`strip_gate_accent_folds_into_the_same_bytes_as_gate_active`]:
    /// the gallery tier is a read-only comparison grid, never a selected
    /// button state, so `GateAccent` resolves to the same bytes as
    /// `GateActive`.
    #[test]
    fn gallery_gate_accent_folds_into_the_same_bytes_as_gate_active() {
        for id in IconId::ALL {
            assert_eq!(sixel_gallery(id, SixelVariant::GateAccent), sixel_gallery(id, SixelVariant::GateActive), "{id:?}");
        }
    }

    #[test]
    fn every_compact_sixel_rgba_matches_its_own_declared_dimensions() {
        let expected = (COMPACT_SIXEL_ICON_WIDTH_PX * COMPACT_SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::sixel_compact_source_rgba(id).len(), expected, "{id:?} compact sixel rgba length");
        }
    }

    #[test]
    fn every_icon_resolves_in_the_compact_tier_without_panicking() {
        for id in IconId::ALL {
            let _sixel_compact = sixel_compact(id);
        }
    }

    #[test]
    fn every_compact_sixel_encodes_to_a_non_empty_dcs_sequence() {
        for id in IconId::ALL {
            let encoded = sixel_compact(id);
            assert!(encoded.starts_with('\u{1b}'), "{id:?} compact sixel output must start with the DCS introducer ESC");
        }
    }

    #[test]
    fn every_ascii_label_is_non_empty_and_at_most_two_chars() {
        for id in IconId::ALL {
            let label = ascii(id);
            assert!(!label.is_empty(), "{id:?} ascii label must not be empty");
            assert!(label.chars().count() <= 2, "{id:?} ascii label {label:?} is longer than 2 chars");
        }
    }

    // ---- Lucide (see this module's own "Lucide" doc section) ------------

    /// `lucide_slug` is the single source of truth for "does `id` have a
    /// Lucide asset at all" -- every other Lucide accessor below (raw
    /// sources, `sixel_*_family`) must agree with it exactly: `Some` for
    /// the 55 mapped icons, `None` for the two documented gaps
    /// (`CircleFilled`, `RunAll`), never a third icon on either side.
    #[test]
    fn lucide_slug_covers_exactly_the_mapped_icons_and_reports_exactly_two_gaps() {
        let mapped = IconId::ALL.iter().filter(|id| lucide_slug(**id).is_some()).count();
        let gaps: Vec<IconId> = IconId::ALL.into_iter().filter(|id| lucide_slug(*id).is_none()).collect();
        assert_eq!(mapped, 55, "expected 55 of {} IconId variants to carry a Lucide slug", IconId::ALL.len());
        assert_eq!(
            gaps,
            vec![IconId::CircleFilled, IconId::RunAll],
            "the two documented Lucide mapping gaps must be exactly these two, no more, no fewer",
        );
    }

    /// Every raw Lucide source (test-only, same retirement precedent as
    /// the codicon `_RGBA` consts -- see `tools/bake_icons.py::ensure_
    /// lucide_assets`'s own doc comment) is present with the tier's own
    /// declared byte length wherever `lucide_slug` says an asset exists,
    /// and absent (`None`) everywhere it says it does not -- the SAME
    /// `Some`/`None` split as [`lucide_slug`] itself, checked against the
    /// real baked bytes rather than just the mapping table.
    #[test]
    fn every_lucide_sixel_rgba_matches_its_own_declared_dimensions_or_is_a_documented_gap() {
        let sixel_expected = (SIXEL_ICON_WIDTH_PX * SIXEL_ICON_HEIGHT_PX * 4) as usize;
        let compact_expected = (COMPACT_SIXEL_ICON_WIDTH_PX * COMPACT_SIXEL_ICON_HEIGHT_PX * 4) as usize;
        let strip_expected = (STRIP_SIXEL_ICON_WIDTH_PX * STRIP_SIXEL_ICON_HEIGHT_PX * 4) as usize;
        let gallery_expected = (GALLERY_SIXEL_ICON_WIDTH_PX * GALLERY_SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            let mapped = lucide_slug(id).is_some();
            for (label, actual, expected) in [
                ("sixel", catalog::lucide_sixel_source_rgba(id), sixel_expected),
                ("gate_active", catalog::lucide_sixel_gate_active_source_rgba(id), sixel_expected),
                ("gate_accent", catalog::lucide_sixel_gate_accent_source_rgba(id), sixel_expected),
                ("compact", catalog::lucide_sixel_compact_source_rgba(id), compact_expected),
                ("strip", catalog::lucide_sixel_strip_source_rgba(id), strip_expected),
                ("strip_gate", catalog::lucide_sixel_strip_gate_source_rgba(id), strip_expected),
                ("gallery", catalog::lucide_sixel_gallery_source_rgba(id), gallery_expected),
                ("gallery_gate", catalog::lucide_sixel_gallery_gate_source_rgba(id), gallery_expected),
            ] {
                match actual {
                    Some(bytes) => {
                        assert!(mapped, "{id:?}/{label}: has raw bytes but lucide_slug says no mapping");
                        assert_eq!(bytes.len(), expected, "{id:?}/{label} lucide rgba length");
                    }
                    None => assert!(!mapped, "{id:?}/{label}: lucide_slug says mapped but raw bytes are None"),
                }
            }
        }
    }

    const LUCIDE_GAP_IDS: [IconId; 2] = [IconId::CircleFilled, IconId::RunAll];

    /// Every mapped icon resolves in every Lucide tier without panicking;
    /// both documented gaps resolve to `None` in every tier, never a
    /// panic and never a silent codicon fallback -- see `client::flush_
    /// sixel_icon_into`'s own doc comment for how production code handles
    /// that `None`.
    #[test]
    fn every_icon_resolves_in_every_lucide_tier_or_reports_the_documented_gap() {
        for id in IconId::ALL {
            let mapped = lucide_slug(id).is_some();
            assert_eq!(mapped, !LUCIDE_GAP_IDS.contains(&id), "{id:?}");
            for variant in SIXEL_VARIANTS {
                assert_eq!(sixel_family(id, IconFamily::Lucide, variant).is_some(), mapped, "{id:?}/{variant:?} rail");
                assert_eq!(sixel_strip_family(id, IconFamily::Lucide, variant).is_some(), mapped, "{id:?}/{variant:?} strip");
                assert_eq!(sixel_gallery_family(id, IconFamily::Lucide, variant).is_some(), mapped, "{id:?}/{variant:?} gallery");
            }
            assert_eq!(sixel_compact_family(id, IconFamily::Lucide).is_some(), mapped, "{id:?} compact");
            // `Codicons` never has a gap -- every `IconId` was baked from
            // the original 57-icon codicon manifest with no exceptions.
            for variant in SIXEL_VARIANTS {
                assert!(sixel_family(id, IconFamily::Codicons, variant).is_some(), "{id:?}/{variant:?} codicons rail");
            }
        }
    }

    #[test]
    fn every_lucide_sixel_encodes_to_a_non_empty_dcs_sequence() {
        for id in IconId::ALL {
            if lucide_slug(id).is_none() {
                continue;
            }
            for variant in SIXEL_VARIANTS {
                let rail = sixel_family(id, IconFamily::Lucide, variant).expect("mapped icon");
                assert!(rail.starts_with('\u{1b}'), "{id:?}/{variant:?} lucide rail sixel must start with the DCS introducer ESC");
                let strip = sixel_strip_family(id, IconFamily::Lucide, variant).expect("mapped icon");
                assert!(strip.starts_with('\u{1b}'), "{id:?}/{variant:?} lucide strip sixel must start with the DCS introducer ESC");
                let gallery = sixel_gallery_family(id, IconFamily::Lucide, variant).expect("mapped icon");
                assert!(gallery.starts_with('\u{1b}'), "{id:?}/{variant:?} lucide gallery sixel must start with the DCS introducer ESC");
            }
            let compact = sixel_compact_family(id, IconFamily::Lucide).expect("mapped icon");
            assert!(compact.starts_with('\u{1b}'), "{id:?} lucide compact sixel must start with the DCS introducer ESC");
        }
    }

    /// Same lock-in as [`tests::every_gate_composited_sixel_source_is_
    /// fully_opaque`] (cause 1's own fix), for Lucide: a rail/strip/
    /// gallery gate-composited source must be fully opaque, no exception
    /// for the stroke-based family -- `composite_over_background` runs
    /// the identical arithmetic regardless of which family's raw buffer
    /// it is handed.
    #[test]
    fn every_lucide_gate_composited_sixel_source_is_fully_opaque() {
        for id in IconId::ALL {
            let Some(_) = lucide_slug(id) else { continue };
            for (label, rgba) in [
                ("gate_active", catalog::lucide_sixel_gate_active_source_rgba(id).expect("mapped icon")),
                ("gate_accent", catalog::lucide_sixel_gate_accent_source_rgba(id).expect("mapped icon")),
                ("strip_gate", catalog::lucide_sixel_strip_gate_source_rgba(id).expect("mapped icon")),
                ("gallery_gate", catalog::lucide_sixel_gallery_gate_source_rgba(id).expect("mapped icon")),
            ] {
                assert!(
                    rgba.chunks_exact(4).all(|px| px[3] == 255),
                    "{id:?}'s lucide {label} source must be fully opaque (every alpha byte 255)"
                );
            }
        }
    }

    #[test]
    fn lucide_raw_precomposite_sources_still_carry_real_coverage_variation() {
        for id in IconId::ALL {
            let Some(_) = lucide_slug(id) else { continue };
            assert!(
                catalog::lucide_sixel_source_rgba(id).expect("mapped icon").chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s lucide rail raw source must still have partial-coverage pixels"
            );
            assert!(
                catalog::lucide_sixel_strip_source_rgba(id).expect("mapped icon").chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s lucide strip raw source must still have partial-coverage pixels"
            );
            assert!(
                catalog::lucide_sixel_gallery_source_rgba(id).expect("mapped icon").chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s lucide gallery raw source must still have partial-coverage pixels"
            );
        }
    }

    /// Same exact-arithmetic lock-in as [`tests::gate_compositing_
    /// matches_the_background_and_ink_colours_exactly_at_full_coverage`],
    /// for Lucide: `composite_over_background` does not know or care
    /// which family's buffer it is compositing.
    #[test]
    fn lucide_gate_compositing_matches_the_background_and_ink_colours_exactly_at_full_coverage() {
        const GATE_ACTIVE_BG: (u8, u8, u8) = (30, 30, 46);
        const GATE_ACCENT_BG: (u8, u8, u8) = (203, 166, 247);

        fn assert_matches_at_extremes(id: IconId, label: &str, source: &[u8], composited: &[u8], bg: (u8, u8, u8)) {
            assert_eq!(source.len(), composited.len(), "{id:?}/{label} source/composited length mismatch");
            for (source_px, composited_px) in source.chunks_exact(4).zip(composited.chunks_exact(4)) {
                match source_px[3] {
                    0 => assert_eq!(
                        (composited_px[0], composited_px[1], composited_px[2]),
                        bg,
                        "{id:?}/{label}: an uncovered lucide source pixel must composite to the flat background colour exactly"
                    ),
                    255 => assert_eq!(
                        (composited_px[0], composited_px[1], composited_px[2]),
                        (source_px[0], source_px[1], source_px[2]),
                        "{id:?}/{label}: a fully-covered lucide source pixel's ink colour must survive compositing unchanged"
                    ),
                    _ => {}
                }
            }
        }

        for id in IconId::ALL {
            let Some(_) = lucide_slug(id) else { continue };
            assert_matches_at_extremes(
                id, "lucide_rail_gate_active",
                catalog::lucide_sixel_source_rgba(id).expect("mapped icon"),
                catalog::lucide_sixel_gate_active_source_rgba(id).expect("mapped icon"),
                GATE_ACTIVE_BG,
            );
            assert_matches_at_extremes(
                id, "lucide_rail_gate_accent",
                catalog::lucide_sixel_source_rgba(id).expect("mapped icon"),
                catalog::lucide_sixel_gate_accent_source_rgba(id).expect("mapped icon"),
                GATE_ACCENT_BG,
            );
            assert_matches_at_extremes(
                id, "lucide_strip_gate",
                catalog::lucide_sixel_strip_source_rgba(id).expect("mapped icon"),
                catalog::lucide_sixel_strip_gate_source_rgba(id).expect("mapped icon"),
                GATE_ACTIVE_BG,
            );
            assert_matches_at_extremes(
                id, "lucide_gallery_gate",
                catalog::lucide_sixel_gallery_source_rgba(id).expect("mapped icon"),
                catalog::lucide_sixel_gallery_gate_source_rgba(id).expect("mapped icon"),
                GATE_ACTIVE_BG,
            );
        }
    }

    /// The task this module's own "Lucide" doc section documents: one
    /// authored stroke width (`LUCIDE_STROKE_WIDTH`, 1.5 source units)
    /// must land exactly 1/2/3 device pixels' worth of ink at the strip/
    /// rail/gallery tiers respectively. Verified on the REAL baked bytes
    /// of `IconId::SplitHorizontal` (`square-split-horizontal`), whose
    /// `<line x1="12" x2="12" y1="4" y2="20"/>` is a plain axis-aligned
    /// vertical stroke crossing dead-center -- a middle-row scan measures
    /// its width directly. The verification is total COVERAGE WEIGHT
    /// (sum of alpha across the crossing, divided by 255) rather than "a
    /// literal run of N consecutive 255 bytes": `x=12` sits exactly on a
    /// PIXEL BOUNDARY at every one of this tool's own render scales
    /// (12 is a multiple of 3, and 3 * {2/3, 4/3, 2} is always a whole
    /// number -- see this module's own "Lucide" doc section), which is
    /// the CORRECT, crisp outcome at the rail tier's own EVEN 2px width
    /// (both edges land on whole pixels: a clean, isolated 2-pixel run of
    /// alpha 255 with zero on either side, asserted below byte-for-byte)
    /// but means the strip/gallery tiers' own ODD 1px/3px widths
    /// necessarily straddle that same boundary by half a device pixel on
    /// each side instead of concentrating in one run (an odd-width
    /// stroke centered exactly ON a pixel edge cannot land as a single
    /// whole pixel -- that is not a defect, it is the same "N pixels'
    /// worth of ink, whichever pixels it falls across" guarantee the
    /// stroke-width constant actually makes, and coverage-weight is the
    /// property that is invariant regardless of which parity a given
    /// icon's own coordinates happen to hit). Both shapes are reported
    /// here rather than only the clean one, precisely because this task
    /// asked for the real profile, not a cherry-picked one.
    #[test]
    fn lucide_stroke_lands_on_the_correct_whole_pixel_ink_weight_at_every_tier() {
        let strip = catalog::lucide_sixel_strip_source_rgba(IconId::SplitHorizontal).expect("mapped icon");
        let rail = catalog::lucide_sixel_source_rgba(IconId::SplitHorizontal).expect("mapped icon");
        let gallery = catalog::lucide_sixel_gallery_source_rgba(IconId::SplitHorizontal).expect("mapped icon");

        fn middle_row_alpha(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
            let row = (height / 2) as usize;
            let width = width as usize;
            rgba[row * width * 4..(row + 1) * width * 4]
                .chunks_exact(4)
                .map(|px| px[3])
                .collect()
        }

        // Coverage weight of one crossing (a contiguous non-zero slice),
        // in device pixels -- `sum(alpha) / 255`, exact for a hard edge,
        // a whole number within +/-1 byte of rounding for a split one.
        fn crossing_weight(profile: &[u8], range: std::ops::Range<usize>) -> f32 {
            profile[range].iter().map(|&a| a as f32).sum::<f32>() / 255.0
        }

        let strip_profile = middle_row_alpha(strip, STRIP_SIXEL_ICON_WIDTH_PX, STRIP_SIXEL_ICON_HEIGHT_PX);
        let rail_profile = middle_row_alpha(rail, SIXEL_ICON_WIDTH_PX, SIXEL_ICON_HEIGHT_PX);
        let gallery_profile = middle_row_alpha(gallery, GALLERY_SIXEL_ICON_WIDTH_PX, GALLERY_SIXEL_ICON_HEIGHT_PX);

        // Center crossing only (the `<line>` at x=12); the two corner-
        // bracket paths near the left/right edges are a different shape
        // (curved) and not this test's own concern.
        let strip_weight = crossing_weight(&strip_profile, 9..11);
        let rail_weight = crossing_weight(&rail_profile, 19..21);
        let gallery_weight = crossing_weight(&gallery_profile, 28..32);

        assert!((strip_weight - 1.0).abs() < 0.02, "strip crossing weight {strip_weight} != 1px worth of ink; profile={strip_profile:?}");
        assert!((rail_weight - 2.0).abs() < 0.02, "rail crossing weight {rail_weight} != 2px worth of ink; profile={rail_profile:?}");
        assert!((gallery_weight - 3.0).abs() < 0.02, "gallery crossing weight {gallery_weight} != 3px worth of ink; profile={gallery_profile:?}");

        // The rail tier's own even width additionally lands as a single
        // hard-edged run (both x=12's own position AND the 2px width are
        // boundary-aligned at this scale) -- the strongest form of "whole
        // pixel" this pipeline can produce, asserted exactly since this
        // specific icon/tier pair is known to hit it.
        assert_eq!(&rail_profile[19..21], &[255, 255], "rail crossing must be two full-opacity pixels with a hard edge");
        assert_eq!(rail_profile[18], 0, "rail crossing must have zero coverage immediately outside its own hard edge");
        assert_eq!(rail_profile[21], 0, "rail crossing must have zero coverage immediately outside its own hard edge");
    }
}
