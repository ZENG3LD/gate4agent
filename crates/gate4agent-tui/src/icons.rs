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
//! per pixel with no blend information in the encoded stream, and Windows
//! Terminal's own sixel decoder does not implement the DEC-spec
//! "undrawn pixel stays transparent" semantics -- together this reads as
//! dirty edges on any transparently-encoded icon. The fix (see `tools/
//! bake_icons.py`'s own header doc comment for the full diagnosis) is to
//! composite every sixel-tier icon over the EXACT background colour its
//! button paints, fully opaque, wherever that background is a fixed,
//! known constant (`PtyColorMode::GateOverride`'s own theme) -- there is
//! then no transparent pixel left for either the encoder's threshold or
//! the terminal's own decoder to mishandle. `PtyColorMode::Inherited` has
//! no equivalent: the terminal's own actual background is not knowable
//! at bake OR at render time (crossterm has no reliable query, the same
//! epistemic gap [`ASSUMED_CELL_WIDTH_PX`]'s own doc comment already
//! names for cell-pixel size), so it keeps the original transparent-
//! encoded asset as its only available option (pre-corrected for gamma,
//! see `tools/bake_icons.py::precorrect_transparent_alpha`'s own doc
//! comment -- the SAME dark-background assumption this module's `ASSUMED_
//! CELL_WIDTH_PX` doc already documents the shape of, applied to colour
//! instead of geometry). The ascii tier needs no equivalent of this at
//! all: it is plain themed text drawn directly with the button's own
//! background style, not a raster image -- only a sixel image, a raster
//! the terminal has no concept of "this pixel belongs to a themed panel"
//! for, needs a pre-baked variant per background at all.

use icy_sixel::{BackgroundMode, EncodeOptions, SixelImage};

mod catalog;

pub use catalog::{ascii, sixel, sixel_compact, sixel_gallery, sixel_strip, IconId};

/// Which pre-baked background a sixel-tier icon asset was composited
/// against (or left transparent for) -- see this module's own "Sixel
/// background variants" doc section above for the full cause-1 diagnosis
/// and fix, and `render::Theme`'s own `mode` field for how a render call
/// site picks one from `app.color_mode` (+ a rail button's own `selected`
/// state).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SixelVariant {
    /// `PtyColorMode::Inherited`: no knowable exact background: the
    /// source image keeps `icy_sixel`'s `BackgroundMode::Transparent`
    /// semantics, byte-for-byte the same asset every tier shipped before
    /// this variant existed.
    Transparent,
    /// `PtyColorMode::GateOverride`'s own fixed "at rest" colour
    /// (`render::ACTIVE_BG`) -- pre-composited fully opaque at bake time.
    GateActive,
    /// `PtyColorMode::GateOverride`'s own fixed "selected" accent colour
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

pub(crate) fn build_sixel(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, SIXEL_ICON_WIDTH_PX, SIXEL_ICON_HEIGHT_PX, BackgroundMode::Transparent)
}

/// Same source pixels as [`build_sixel`], for a `SixelVariant::GateActive`/
/// `GateAccent` asset that was already pre-composited fully opaque at
/// bake time (see [`SixelVariant`]'s own doc comment) -- `BackgroundMode::
/// Opaque` here is a documentation choice, not a functional requirement:
/// every pixel in such a buffer already has alpha 255, so `icy_sixel`'s
/// own encoder would treat it identically either way (see `tools/
/// bake_icons.py`'s own header doc comment for why).
pub(crate) fn build_sixel_gate(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, SIXEL_ICON_WIDTH_PX, SIXEL_ICON_HEIGHT_PX, BackgroundMode::Opaque)
}

/// Same encoding as [`build_sixel`], for the compact tier's own smaller
/// per-icon asset (see this module's own "Compact tier" section above).
pub(crate) fn build_sixel_compact(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, COMPACT_SIXEL_ICON_WIDTH_PX, COMPACT_SIXEL_ICON_HEIGHT_PX, BackgroundMode::Transparent)
}

/// Same encoding as [`build_sixel`], for the strip tier's own per-icon
/// asset (see this module's own "Strip tier" section above).
pub(crate) fn build_sixel_strip(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, STRIP_SIXEL_ICON_WIDTH_PX, STRIP_SIXEL_ICON_HEIGHT_PX, BackgroundMode::Transparent)
}

/// Same relationship [`build_sixel_gate`] has to [`build_sixel`], for the
/// strip tier's own pre-composited asset.
pub(crate) fn build_sixel_strip_gate(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, STRIP_SIXEL_ICON_WIDTH_PX, STRIP_SIXEL_ICON_HEIGHT_PX, BackgroundMode::Opaque)
}

/// Same encoding as [`build_sixel`], for the gallery tier's own dedicated
/// per-icon asset (see this module's own "Gallery tier" section above).
pub(crate) fn build_sixel_gallery(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, GALLERY_SIXEL_ICON_WIDTH_PX, GALLERY_SIXEL_ICON_HEIGHT_PX, BackgroundMode::Transparent)
}

/// Same relationship [`build_sixel_gate`] has to [`build_sixel`], for the
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

    const SIXEL_VARIANTS: [SixelVariant; 3] = [SixelVariant::Transparent, SixelVariant::GateActive, SixelVariant::GateAccent];

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

    /// The `Transparent` variant's own source bytes are UNTOUCHED by the
    /// gate-compositing pass -- still a real transparent asset (at least
    /// one alpha byte below 255), not accidentally overwritten with a
    /// composited copy.
    #[test]
    fn transparent_variant_sources_still_carry_real_transparency() {
        for id in IconId::ALL {
            assert!(
                catalog::sixel_source_rgba(id).chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s rail Transparent source must still have transparent pixels"
            );
            assert!(
                catalog::sixel_strip_source_rgba(id).chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s strip Transparent source must still have transparent pixels"
            );
            assert!(
                catalog::sixel_gallery_source_rgba(id).chunks_exact(4).any(|px| px[3] < 255),
                "{id:?}'s gallery Transparent source must still have transparent pixels"
            );
        }
    }

    /// The actual "over" compositing arithmetic `tools/bake_icons.py::
    /// composite_over_background` performs, verified pixel-for-pixel
    /// against the real baked assets rather than trusted by construction:
    /// wherever the transparent source is fully uncovered (alpha 0), the
    /// composited pixel must be EXACTLY the flat background colour;
    /// wherever it is fully covered (alpha 255), the composited pixel
    /// must be EXACTLY the source's own (already `#cdd6f4`-tinted, per
    /// `patch_fill`) ink colour, unchanged. Both extremes survive `tools/
    /// bake_icons.py::precorrect_transparent_alpha`'s own alpha remap
    /// unchanged too (0 and 255 are its fixed points), so this holds for
    /// the shipped `Transparent`-variant source bytes exactly as baked,
    /// not just for some hypothetical unconverted buffer.
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
}
