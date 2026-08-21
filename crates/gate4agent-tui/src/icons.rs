//! Baked codicon bitmaps for this crate's icon catalog -- the full
//! ~57-icon set (see [`IconId`]), not just the activity rail's original
//! 7. Offline-rasterized, raw RGBA8 bytes checked straight into the
//! crate via `include_bytes!` -- no `usvg`/`resvg`/`uzor-icon` runtime or
//! build dependency. Every icon is baked in TWO raster tiers (see
//! `render::render_rail_button` / `app::RailIcons`), switched globally
//! by the owner's own preference, not fixed per button, plus a plain
//! ASCII text tier:
//!
//! - Sixel (`RailIcons::Sixel`): a real raster image written directly to
//!   the terminal by `client::run`'s post-flush hook -- see [`sixel`].
//! - Braille (`RailIcons::Braille`): a `uzor_tui::canvas::PixelCanvas`
//!   painted straight into the cell buffer -- see [`braille`].
//! - Ascii (`RailIcons::Ascii`): a short (<=2 char) plain-text label --
//!   see [`ascii`]. The activity rail itself still paints its OWN
//!   `render::RailButton::ascii` literal in this mode rather than
//!   calling [`ascii`] (see "Wiring status" below); that field's values
//!   match what [`ascii`] returns for the same 7 icons regardless.
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
//! python tools/bake_icons.py
//! ```
//!
//! That tool's own header doc comment has the full pipeline (the exact
//! `resvg`/`ffmpeg` filter graphs for both raster tiers, byte-for-byte)
//! and the per-icon braille alpha-threshold selection algorithm; this
//! module doc does not reproduce either, to avoid the two drifting
//! apart. The original 7 rail icons' own thresholds/assets are
//! explicitly REPLAYED unchanged by that tool (see its own
//! `THRESHOLD_OVERRIDES` / `LEGACY_RAIL_SLUGS`), not re-derived -- this
//! generalization is a rename, not a re-bake, for those 7 (see this
//! module's own `tests::the_rails_original_7_icons_still_render_the_
//! same_braille_glyphs_as_before_this_catalog_generalized`).

use icy_sixel::{BackgroundMode, SixelImage};
use uzor_tui::canvas::{CanvasMode, PixelCanvas};
use uzor_tui::style::Color;

mod catalog;

pub use catalog::{ascii, braille, braille_compact, sixel, sixel_compact, IconId};

/// Assumed terminal cell size in pixels (Cascadia Mono 12pt, Windows
/// Terminal) -- the basis the sixel tier's own pixel target was derived
/// from. Not read at runtime (crossterm has no reliable cell-pixel
/// probe); retuning either constant means re-rasterizing every `.rgba`
/// sixel-tier asset at the new target size via `tools/bake_icons.py`,
/// not just editing a number here.
pub const ASSUMED_CELL_WIDTH_PX: u32 = 10;
pub const ASSUMED_CELL_HEIGHT_PX: u32 = 20;

// ---- Sixel tier -----------------------------------------------------

/// Baked bitmap pixel size shared by every sixel-tier icon: ~4 cells
/// wide x 2 rows tall at the assumed cell size above -- a SQUARE crop of
/// the button's own 4-cell x 3-row body (leaving the body's 3rd row as
/// plain button background), matching every source codicon's own square
/// aspect ratio with no distortion.
pub const SIXEL_ICON_WIDTH_PX: u32 = 40;
pub const SIXEL_ICON_HEIGHT_PX: u32 = 40;
/// Cell footprint the flush hook reserves on the rail for one sixel icon
/// (derived from the pixel size above and the assumed cell size, not
/// hand-synced).
pub const SIXEL_ICON_CELLS_WIDE: u16 = (SIXEL_ICON_WIDTH_PX / ASSUMED_CELL_WIDTH_PX) as u16;
pub const SIXEL_ICON_CELLS_TALL: u16 = (SIXEL_ICON_HEIGHT_PX / ASSUMED_CELL_HEIGHT_PX) as u16;

// ---- Braille tier -----------------------------------------------------

/// Baked bitmap pixel size shared by every braille-tier icon: 4 cells
/// wide x 3 rows tall (the button's FULL body) at braille's fixed 2x4
/// dots/cell density (see `uzor_tui::canvas::CanvasMode::Braille`).
pub const BRAILLE_ICON_WIDTH_PX: u32 = 8;
pub const BRAILLE_ICON_HEIGHT_PX: u32 = 12;
pub const BRAILLE_ICON_CELLS_WIDE: u16 = 4;
pub const BRAILLE_ICON_CELLS_TALL: u16 = 3;

// ---- Compact tier -------------------------------------------------------
//
// For dense, single-row inline buttons (Explorer/Git sidebar panel lists
// and their modals -- `render::render_compact_icon_button`) where the
// rail's own 4-cell x 2/3-row icon does not fit next to a text label in
// the SAME row. Baked as its own separate, much smaller raster per icon
// (not a runtime downscale of the rail-tier asset) by `tools/
// bake_icons.py`'s own `rasterize_compact_sixel`/`rasterize_compact_
// braille`.

/// Compact sixel-tier bitmap pixel size: exactly ONE assumed terminal
/// cell (see `ASSUMED_CELL_WIDTH_PX`/`ASSUMED_CELL_HEIGHT_PX` above).
pub const COMPACT_SIXEL_ICON_WIDTH_PX: u32 = ASSUMED_CELL_WIDTH_PX;
pub const COMPACT_SIXEL_ICON_HEIGHT_PX: u32 = ASSUMED_CELL_HEIGHT_PX;
pub const COMPACT_SIXEL_ICON_CELLS_WIDE: u16 = 1;
pub const COMPACT_SIXEL_ICON_CELLS_TALL: u16 = 1;

/// Compact braille-tier bitmap: 2 cells wide x 1 row tall (a braille cell
/// is a fixed 2x4 dot grid, so this is a 4x4-dot canvas) -- a SQUARE
/// target, unlike the rail tier's own 2:3 portrait (8x12) grid.
pub const COMPACT_BRAILLE_ICON_WIDTH_PX: u32 = 4;
pub const COMPACT_BRAILLE_ICON_HEIGHT_PX: u32 = 4;
pub const COMPACT_BRAILLE_ICON_CELLS_WIDE: u16 = 2;
pub const COMPACT_BRAILLE_ICON_CELLS_TALL: u16 = 1;

pub(crate) fn build_sixel(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, SIXEL_ICON_WIDTH_PX, SIXEL_ICON_HEIGHT_PX)
}

/// Same encoding as [`build_sixel`], for the compact tier's own smaller
/// per-icon asset (see this module's own "Compact tier" section above).
pub(crate) fn build_sixel_compact(rgba: &[u8]) -> String {
    build_sixel_sized(rgba, COMPACT_SIXEL_ICON_WIDTH_PX, COMPACT_SIXEL_ICON_HEIGHT_PX)
}

fn build_sixel_sized(rgba: &[u8], width: u32, height: u32) -> String {
    let image = SixelImage::try_from_rgba(rgba.to_vec(), width as usize, height as usize).expect(
        "every sixel-tier .rgba asset's byte length is asserted against its own tier's \
         WIDTH_PX * HEIGHT_PX * 4 by this module's own unit tests -- a mismatch here means a \
         baked asset was regenerated at a different size without updating these constants, a \
         build-time asset/constant drift, not a runtime condition",
    );
    image
        .with_background_mode(BackgroundMode::Transparent)
        .encode()
        .expect("encoding a fixed, already-validated, in-memory RGBA buffer to SIXEL does not fail")
}

/// Builds a [`PixelCanvas`] from a baked RGBA8 buffer, one `set_pixel`
/// per pixel at or above `alpha_threshold` (straight, non-premultiplied
/// alpha -- see this module's own doc comment); a pixel below the
/// threshold is left unset, matching the sixel tier's own
/// transparent-background convention so both tiers silhouette the same
/// glyph rather than one of them painting a filled color block. `rgba`'s
/// length is validated by this module's own unit tests, not here -- a
/// buffer shorter than `canvas.px_width() * px_height() * 4` degrades to
/// leaving the missing tail unset (the same "off-canvas input degrades
/// quietly" stance `PixelCanvas::set_pixel` itself documents) rather
/// than panicking.
pub(crate) fn rgba_to_canvas(mode: CanvasMode, cell_width: u16, cell_height: u16, rgba: &[u8], alpha_threshold: u8) -> PixelCanvas {
    let mut canvas = PixelCanvas::new(mode, cell_width, cell_height);
    let px_width = canvas.px_width();
    let px_height = canvas.px_height();
    for y in 0..px_height {
        for x in 0..px_width {
            let idx = ((y * px_width + x) * 4) as usize;
            if idx + 4 > rgba.len() {
                continue;
            }
            let (r, g, b, a) = (rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]);
            if a >= alpha_threshold {
                canvas.set_pixel(x as i64, y as i64, Color::Rgb(r, g, b));
            }
        }
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use uzor_tui::buffer::TerminalBuffer;
    use uzor_tui::rect::Rect;

    #[test]
    fn every_sixel_rgba_matches_its_own_declared_dimensions() {
        let expected = (SIXEL_ICON_WIDTH_PX * SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::sixel_source_rgba(id).len(), expected, "{id:?} sixel rgba length");
        }
    }

    #[test]
    fn every_braille_rgba_matches_its_own_declared_dimensions() {
        let expected = (BRAILLE_ICON_WIDTH_PX * BRAILLE_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::braille_source_rgba(id).len(), expected, "{id:?} braille rgba length");
        }
    }

    #[test]
    fn every_icon_resolves_in_all_three_tiers_without_panicking() {
        for id in IconId::ALL {
            let _sixel = sixel(id);
            let _braille = braille(id);
            let _ascii = ascii(id);
        }
    }

    #[test]
    fn every_sixel_encodes_to_a_non_empty_dcs_sequence() {
        for id in IconId::ALL {
            let encoded = sixel(id);
            assert!(encoded.starts_with('\u{1b}'), "{id:?} sixel output must start with the DCS introducer ESC");
            assert!(encoded.len() > 16, "{id:?} sixel output for a 40x40 icon with real ink must not be a near-empty stub");
        }
    }

    #[test]
    fn every_braille_canvas_pixel_dimensions_match_the_baked_asset() {
        for id in IconId::ALL {
            let canvas = braille(id);
            assert_eq!(canvas.px_width(), BRAILLE_ICON_WIDTH_PX, "{id:?}");
            assert_eq!(canvas.px_height(), BRAILLE_ICON_HEIGHT_PX, "{id:?}");
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
    fn every_compact_braille_rgba_matches_its_own_declared_dimensions() {
        let expected = (COMPACT_BRAILLE_ICON_WIDTH_PX * COMPACT_BRAILLE_ICON_HEIGHT_PX * 4) as usize;
        for id in IconId::ALL {
            assert_eq!(catalog::braille_compact_source_rgba(id).len(), expected, "{id:?} compact braille rgba length");
        }
    }

    #[test]
    fn every_icon_resolves_in_both_compact_tiers_without_panicking() {
        for id in IconId::ALL {
            let _sixel_compact = sixel_compact(id);
            let _braille_compact = braille_compact(id);
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
    fn every_compact_braille_canvas_pixel_dimensions_match_the_baked_asset() {
        for id in IconId::ALL {
            let canvas = braille_compact(id);
            assert_eq!(canvas.px_width(), COMPACT_BRAILLE_ICON_WIDTH_PX, "{id:?}");
            assert_eq!(canvas.px_height(), COMPACT_BRAILLE_ICON_HEIGHT_PX, "{id:?}");
        }
    }

    #[test]
    fn every_compact_braille_canvas_lights_at_least_one_dot() {
        for id in IconId::ALL {
            let mut buf = TerminalBuffer::new(COMPACT_BRAILLE_ICON_CELLS_WIDE, COMPACT_BRAILLE_ICON_CELLS_TALL);
            braille_compact(id).flush(Rect::new(0, 0, COMPACT_BRAILLE_ICON_CELLS_WIDE, COMPACT_BRAILLE_ICON_CELLS_TALL), &mut buf);
            let lit_cells = (0..COMPACT_BRAILLE_ICON_CELLS_TALL)
                .flat_map(|y| (0..COMPACT_BRAILLE_ICON_CELLS_WIDE).map(move |x| (x, y)))
                .filter(|&(x, y)| buf.get(x, y).symbol.as_str() != "\u{2800}")
                .count();
            assert!(lit_cells > 0, "{id:?} compact braille canvas rendered fully blank");
        }
    }

    #[test]
    fn every_braille_canvas_lights_at_least_one_dot() {
        for id in IconId::ALL {
            let mut buf = TerminalBuffer::new(BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL);
            braille(id).flush(Rect::new(0, 0, BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL), &mut buf);
            let lit_cells = (0..BRAILLE_ICON_CELLS_TALL)
                .flat_map(|y| (0..BRAILLE_ICON_CELLS_WIDE).map(move |x| (x, y)))
                .filter(|&(x, y)| buf.get(x, y).symbol.as_str() != "\u{2800}")
                .count();
            assert!(lit_cells > 0, "{id:?} braille canvas rendered fully blank");
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

    /// The activity rail's original 7 icons (baseline commit 9f39758,
    /// before this catalog generalized to the full ~57-icon set) must
    /// still render byte-for-byte the same braille glyphs and carry the
    /// same sixel-tier source bytes -- re-pointing the rail at the wider
    /// [`IconId`] enum is a pure rename, not a re-bake, for these 7 (see
    /// `tools/bake_icons.py`'s own `LEGACY_RAIL_SLUGS`/
    /// `THRESHOLD_OVERRIDES`, which replay each one's original constant
    /// unchanged rather than re-deriving it).
    ///
    /// Each assertion below independently `include_bytes!`s the SAME
    /// on-disk asset the catalog's own `IconId` variant is wired to (a
    /// fresh read at this call site, not a reuse of `catalog.rs`'s own
    /// constant) and, for the braille tier, rebuilds a [`PixelCanvas`]
    /// at the ORIGINAL hand-tuned threshold via the same production
    /// `rgba_to_canvas` gate, then compares the actual RENDERED glyph
    /// strings -- the true end-to-end "what paints on screen" check, not
    /// an internal struct comparison (`PixelCanvas` has no `PartialEq`).
    /// A future mis-wiring in `catalog.rs`'s per-icon match arms (wrong
    /// asset file OR wrong threshold swapped into one variant) changes
    /// either the source bytes or the rendered glyphs and this test
    /// catches it.
    #[test]
    fn the_rails_original_7_icons_still_render_the_same_braille_glyphs_as_before_this_catalog_generalized() {
        fn rendered_glyphs(canvas: &PixelCanvas) -> Vec<String> {
            let mut buf = TerminalBuffer::new(BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL);
            canvas.flush(Rect::new(0, 0, BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL), &mut buf);
            let mut glyphs = Vec::new();
            for y in 0..BRAILLE_ICON_CELLS_TALL {
                for x in 0..BRAILLE_ICON_CELLS_WIDE {
                    glyphs.push(buf.get(x, y).symbol.to_string());
                }
            }
            glyphs
        }

        let originals: [(IconId, &[u8], u8); 7] = [
            (IconId::Files, include_bytes!("icons/files_braille.rgba"), 102),
            (IconId::SourceControl, include_bytes!("icons/source_control_braille.rgba"), 102),
            (IconId::Person, include_bytes!("icons/person_braille.rgba"), 89),
            (IconId::Project, include_bytes!("icons/project_braille.rgba"), 102),
            (IconId::SettingsGear, include_bytes!("icons/settings_gear_braille.rgba"), 89),
            (IconId::ChevronLeft, include_bytes!("icons/chevron_left_braille.rgba"), 77),
            (IconId::ChevronRight, include_bytes!("icons/chevron_right_braille.rgba"), 77),
        ];
        for (id, rgba, threshold) in originals {
            let expected = rgba_to_canvas(CanvasMode::Braille, BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL, rgba, threshold);
            assert_eq!(
                rendered_glyphs(braille(id)),
                rendered_glyphs(&expected),
                "{id:?} braille glyphs changed from the pre-generalization (commit 9f39758) baseline"
            );
        }

        // Sixel tier: the encoder (`build_sixel`) is unchanged and
        // covered by `every_sixel_encodes_to_a_non_empty_dcs_sequence`
        // above, so a byte-identical source RGBA is sufficient here.
        assert_eq!(catalog::sixel_source_rgba(IconId::Files), include_bytes!("icons/files.rgba") as &[u8]);
        assert_eq!(catalog::sixel_source_rgba(IconId::SourceControl), include_bytes!("icons/source_control.rgba") as &[u8]);
        assert_eq!(catalog::sixel_source_rgba(IconId::Person), include_bytes!("icons/person.rgba") as &[u8]);
        assert_eq!(catalog::sixel_source_rgba(IconId::Project), include_bytes!("icons/project.rgba") as &[u8]);
        assert_eq!(catalog::sixel_source_rgba(IconId::SettingsGear), include_bytes!("icons/settings_gear.rgba") as &[u8]);
        assert_eq!(catalog::sixel_source_rgba(IconId::ChevronLeft), include_bytes!("icons/chevron_left.rgba") as &[u8]);
        assert_eq!(catalog::sixel_source_rgba(IconId::ChevronRight), include_bytes!("icons/chevron_right.rgba") as &[u8]);
    }
}
