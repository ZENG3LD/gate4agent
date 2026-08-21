//! Baked codicon bitmaps for the activity rail's icon buttons.
//! Offline-rasterized, raw RGBA8 bytes checked straight into the crate
//! via `include_bytes!` -- no `usvg`/`resvg`/`uzor-icon` runtime or
//! build dependency. Every rail icon is baked in TWO tiers (see
//! `render::render_rail_button` / `app::RailIcons`), switched globally
//! by the owner's own preference, not fixed per button:
//!
//! - Sixel (`RailIcons::Sixel`): a real raster image written directly to
//!   the terminal by `client::run`'s post-flush hook -- see [`sixel`].
//! - Braille (`RailIcons::Braille`): a `uzor_tui::canvas::PixelCanvas`
//!   painted straight into the cell buffer -- see [`braille`].
//!
//! `RailIcons::Ascii` never touches this module at all -- it draws
//! `render::RailButton::ascii` as plain text.
//!
//! Source: microsoft/vscode-codicons (MIT licence,
//! <https://github.com/microsoft/vscode-codicons>), files `files.svg`,
//! `source-control.svg`, `person.svg`, `project.svg`,
//! `settings-gear.svg`, `chevron-left.svg`, `chevron-right.svg`. Each
//! icon's `fill="currentColor"` was patched to `fill="#cdd6f4"` -- this
//! crate's own `pty_palette::GATE_FG` (the off-white already used for
//! unselected rail glyph/ascii labels) -- before rasterizing, so every
//! tier reads as part of the existing theme instead of an arbitrary new
//! color. `resvg` writes straight (non-premultiplied) alpha -- verified
//! against this exact resvg build by inspecting a partially-transparent
//! output pixel's own RGB channels, which stayed at the flat fill color
//! regardless of alpha -- so every "ink" source pixel below can be read
//! as (a flat icon color, a coverage/antialiasing weight) without an
//! un-premultiply step.
//!
//! ## Bake commands
//!
//! resvg 0.47 CLI + ffmpeg 8.1, run against a scratch copy of the
//! codicon SVGs with the fill patched as above -- neither tool is a
//! dependency of this crate, both ran once, offline, to produce the
//! checked-in `.rgba` files under `src/icons/`. `<icon>` stands for each
//! of the 7 file stems named above (`files`, `source-control`, `person`,
//! `project`, `settings-gear`, `chevron-left`, `chevron-right`).
//!
//! ```text
//! # Sixel tier, every icon: resvg's own `-w`/`-h` fits content to the
//! # SMALLER of the two requested dimensions preserving aspect ratio
//! # (verified empirically -- it does NOT stretch non-uniformly or pad
//! # its own output canvas); every source codicon's viewBox is square
//! # and the sixel target is square too, so this is a natural,
//! # undistorted fit with no separate pad step.
//! resvg -w 40 -h 40 <icon>.svg <icon>_40.png
//! ffmpeg -i <icon>_40.png -f rawvideo -pix_fmt rgba <icon>.rgba
//!
//! # Braille tier, every icon EXCEPT the two chevrons (see below): the
//! # braille target (8x12 dots -- the button's full 4-cell x 3-row body
//! # at braille's fixed 2x4 dots/cell) is a 2:3 portrait box (matching a
//! # terminal cell's ~1:2 aspect ratio) but the codicon is square, so a
//! # naive rasterize-straight-at-8x12 antialiases every thin stroke into
//! # a handful of faint, disconnected dots (the original tier-comparison
//! # demo's own complaint that motivated this rework). Instead:
//! # rasterize SUPERSAMPLED at 4x the target grid (32x48 -- the icon's
//! # own natural aspect-preserving fit lands on 32x32, padded onto that
//! # taller canvas with a transparent `black@0.0` border), then
//! # DOWNSAMPLE by area coverage (ffmpeg's `scale=...:flags=area`, a box
//! # filter -- for an exact 4x integer factor this is a plain average of
//! # each 4x4 source block, i.e. true area coverage, not a
//! # nearest/bilinear resample) back down to 8x12. Because resvg's own
//! # alpha channel is already a coverage/antialiasing weight (see this
//! # module's own doc comment above), the downsampled buffer's alpha
//! # directly IS each final dot's coverage fraction (0-255 standing for
//! # 0-100%) -- no separate coverage computation happens in Rust;
//! # `rgba_to_canvas`'s existing alpha-threshold gate (tuned per icon
//! # below, ~35-45% coverage, i.e. alpha ~89-115) reads it directly.
//! resvg -w 32 <icon>.svg <icon>_32.png
//! ffmpeg -i <icon>_32.png -vf "pad=32:48:0:8:color=black@0.0" <icon>_32x48.png
//! ffmpeg -i <icon>_32x48.png -vf "scale=8:12:flags=area" -pix_fmt rgba <icon>_8x12.png
//! ffmpeg -i <icon>_8x12.png -f rawvideo -pix_fmt rgba <icon>_braille.rgba
//!
//! # Braille tier, chevron-left/chevron-right ONLY: both draw a thin
//! # arrow inset well within their own 16x16 viewBox (VS Code pads them
//! # to align with the 24x24 icons elsewhere in the set); at the
//! # braille tier's small final grid that dead margin starves every
//! # dot's own area-coverage average below any usable lit threshold
//! # even with the supersample above (measured peak coverage ~34% with
//! # the recipe above -- see `CHEVRON_LEFT_BRAILLE_ALPHA_THRESHOLD`'s
//! # own doc comment). Fix: a scratch copy of each chevron SVG with
//! # `viewBox` overridden to a tight, hand-computed, 2:3-aspect crop
//! # around the glyph's own path bounding box (so the arrow itself
//! # fills the target frame instead of floating in dead padding) -- 2:3
//! # already matches the braille target's own aspect, so resvg's
//! # `-w 32 -h 48` on the cropped viewBox lands exactly on the padded
//! # canvas with no separate `pad` step needed.
//! # chevron-left  viewBox="2.55 0.95 9.4 14.1"
//! # chevron-right viewBox="4.05 0.95 9.4 14.1"
//! resvg -w 32 -h 48 <icon>.cropped.svg <icon>_32x48.png
//! ffmpeg -i <icon>_32x48.png -vf "scale=8:12:flags=area" -pix_fmt rgba <icon>_8x12.png
//! ffmpeg -i <icon>_8x12.png -f rawvideo -pix_fmt rgba <icon>_braille.rgba
//! ```

use std::sync::LazyLock;

use icy_sixel::{BackgroundMode, SixelImage};
use uzor_tui::canvas::{CanvasMode, PixelCanvas};
use uzor_tui::style::Color;

/// Assumed terminal cell size in pixels (Cascadia Mono 12pt, Windows
/// Terminal) -- the basis the sixel tier's own pixel target was derived
/// from. Not read at runtime (crossterm has no reliable cell-pixel
/// probe); retuning either constant means re-rasterizing every `.rgba`
/// sixel-tier asset at the new target size via this module's own bake
/// commands, not just editing a number here.
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

/// Which baked rail icon a button paints, independent of which tier is
/// active (`app::RailIcons::Sixel` / `Braille` -- see [`sixel`] /
/// [`braille`]). `render::RailButton::icon` maps each activity-rail
/// button to exactly one of these.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RailIconId {
    Files,
    SourceControl,
    Person,
    Project,
    SettingsGear,
    ChevronLeft,
    ChevronRight,
}

impl RailIconId {
    pub const ALL: [RailIconId; 7] = [
        RailIconId::Files,
        RailIconId::SourceControl,
        RailIconId::Person,
        RailIconId::Project,
        RailIconId::SettingsGear,
        RailIconId::ChevronLeft,
        RailIconId::ChevronRight,
    ];
}

/// Encoded sixel string for `id`, encoded once (never per frame -- sixel
/// encoding runs color quantization + palette building, real work) and
/// cached for the process lifetime; `client::run`'s post-flush hook
/// writes this string's bytes directly to the terminal at the button's
/// own recorded placement (`app::LayoutRects::sixel_icons`).
pub fn sixel(id: RailIconId) -> &'static str {
    match id {
        RailIconId::Files => FILES_SIXEL.as_str(),
        RailIconId::SourceControl => SOURCE_CONTROL_SIXEL.as_str(),
        RailIconId::Person => PERSON_SIXEL.as_str(),
        RailIconId::Project => PROJECT_SIXEL.as_str(),
        RailIconId::SettingsGear => SETTINGS_GEAR_SIXEL.as_str(),
        RailIconId::ChevronLeft => CHEVRON_LEFT_SIXEL.as_str(),
        RailIconId::ChevronRight => CHEVRON_RIGHT_SIXEL.as_str(),
    }
}

/// Braille-tier [`PixelCanvas`] for `id`, built once (independent of the
/// button's own runtime background) and cached for the process lifetime
/// -- braille glyphs carry only one `fg` color per cell regardless of
/// selection state, so there is nothing selection-dependent to
/// recompute per frame; `render::render_rail_button`'s own
/// `matte_canvas_background` pass patches the (always-`Color::Reset`)
/// unlit-cell background to the button's current theme color at flush
/// time instead.
pub fn braille(id: RailIconId) -> &'static PixelCanvas {
    match id {
        RailIconId::Files => &*FILES_BRAILLE,
        RailIconId::SourceControl => &*SOURCE_CONTROL_BRAILLE,
        RailIconId::Person => &*PERSON_BRAILLE,
        RailIconId::Project => &*PROJECT_BRAILLE,
        RailIconId::SettingsGear => &*SETTINGS_GEAR_BRAILLE,
        RailIconId::ChevronLeft => &*CHEVRON_LEFT_BRAILLE,
        RailIconId::ChevronRight => &*CHEVRON_RIGHT_BRAILLE,
    }
}

// Raw baked-source lookups by id -- used only by this module's own unit
// tests below (byte-length assertions against the shared dimension
// constants); production code never needs the SOURCE bytes, only the
// already-built `sixel`/`braille` outputs above.
#[cfg(test)]
fn sixel_source_rgba(id: RailIconId) -> &'static [u8] {
    match id {
        RailIconId::Files => FILES_RGBA,
        RailIconId::SourceControl => SOURCE_CONTROL_RGBA,
        RailIconId::Person => PERSON_RGBA,
        RailIconId::Project => PROJECT_RGBA,
        RailIconId::SettingsGear => SETTINGS_GEAR_RGBA,
        RailIconId::ChevronLeft => CHEVRON_LEFT_RGBA,
        RailIconId::ChevronRight => CHEVRON_RIGHT_RGBA,
    }
}

#[cfg(test)]
fn braille_source_rgba(id: RailIconId) -> &'static [u8] {
    match id {
        RailIconId::Files => FILES_BRAILLE_RGBA,
        RailIconId::SourceControl => SOURCE_CONTROL_BRAILLE_RGBA,
        RailIconId::Person => PERSON_BRAILLE_RGBA,
        RailIconId::Project => PROJECT_BRAILLE_RGBA,
        RailIconId::SettingsGear => SETTINGS_GEAR_BRAILLE_RGBA,
        RailIconId::ChevronLeft => CHEVRON_LEFT_BRAILLE_RGBA,
        RailIconId::ChevronRight => CHEVRON_RIGHT_BRAILLE_RGBA,
    }
}

fn build_sixel(rgba: &[u8]) -> String {
    let image = SixelImage::try_from_rgba(rgba.to_vec(), SIXEL_ICON_WIDTH_PX as usize, SIXEL_ICON_HEIGHT_PX as usize).expect(
        "every sixel-tier .rgba asset's byte length is asserted against SIXEL_ICON_WIDTH_PX * \
         SIXEL_ICON_HEIGHT_PX * 4 by this module's own unit tests -- a mismatch here means a \
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
fn rgba_to_canvas(mode: CanvasMode, cell_width: u16, cell_height: u16, rgba: &[u8], alpha_threshold: u8) -> PixelCanvas {
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

// ---- Files ------------------------------------------------------------

const FILES_RGBA: &[u8] = include_bytes!("icons/files.rgba");
static FILES_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel(FILES_RGBA));

const FILES_BRAILLE_RGBA: &[u8] = include_bytes!("icons/files_braille.rgba");
/// ~40% coverage (102/255): `files.svg`'s folder-tab outline clears this
/// cutoff at every stroke measured while baking this asset, and it sits
/// mid-band of the ~35-45% coverage range that reads well across this
/// whole icon set.
const FILES_BRAILLE_ALPHA_THRESHOLD: u8 = 102;
static FILES_BRAILLE: LazyLock<PixelCanvas> =
    LazyLock::new(|| rgba_to_canvas(CanvasMode::Braille, BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL, FILES_BRAILLE_RGBA, FILES_BRAILLE_ALPHA_THRESHOLD));

// ---- Source Control -----------------------------------------------------

const SOURCE_CONTROL_RGBA: &[u8] = include_bytes!("icons/source_control.rgba");
static SOURCE_CONTROL_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel(SOURCE_CONTROL_RGBA));

const SOURCE_CONTROL_BRAILLE_RGBA: &[u8] = include_bytes!("icons/source_control_braille.rgba");
/// ~40% coverage (102/255), matching `FILES_BRAILLE_ALPHA_THRESHOLD`:
/// `source-control.svg`'s branch-merge strokes clear this cutoff at
/// every stroke measured while baking this asset, the same mid-band
/// choice.
const SOURCE_CONTROL_BRAILLE_ALPHA_THRESHOLD: u8 = 102;
static SOURCE_CONTROL_BRAILLE: LazyLock<PixelCanvas> = LazyLock::new(|| {
    rgba_to_canvas(
        CanvasMode::Braille,
        BRAILLE_ICON_CELLS_WIDE,
        BRAILLE_ICON_CELLS_TALL,
        SOURCE_CONTROL_BRAILLE_RGBA,
        SOURCE_CONTROL_BRAILLE_ALPHA_THRESHOLD,
    )
});

// ---- Person -------------------------------------------------------------

const PERSON_RGBA: &[u8] = include_bytes!("icons/person.rgba");
static PERSON_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel(PERSON_RGBA));

const PERSON_BRAILLE_RGBA: &[u8] = include_bytes!("icons/person_braille.rgba");
/// ~35% coverage (89/255), the low end of the ~35-45% band:
/// `person.svg`'s head/shoulders strokes are thinner than `files.svg`'s
/// folder outline (fewer sample rows clear a higher cutoff even after
/// the 4x supersample + area-downsample -- measured while baking this
/// asset), so the lower end of the range is what keeps the silhouette
/// from thinning out toward a single stray dot.
const PERSON_BRAILLE_ALPHA_THRESHOLD: u8 = 89;
static PERSON_BRAILLE: LazyLock<PixelCanvas> =
    LazyLock::new(|| rgba_to_canvas(CanvasMode::Braille, BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL, PERSON_BRAILLE_RGBA, PERSON_BRAILLE_ALPHA_THRESHOLD));

// ---- Project (Board) ------------------------------------------------------

const PROJECT_RGBA: &[u8] = include_bytes!("icons/project.rgba");
static PROJECT_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel(PROJECT_RGBA));

const PROJECT_BRAILLE_RGBA: &[u8] = include_bytes!("icons/project_braille.rgba");
/// ~40% coverage (102/255), matching `FILES_BRAILLE_ALPHA_THRESHOLD`:
/// `project.svg`'s rounded-rectangle outline clears this cutoff at
/// every straight edge measured while baking this asset; the lower
/// ~35% band additionally lit the 4 rounded-corner diagonal dots, but
/// 40% already reads as a clean, unambiguous rectangle outline on its
/// own.
const PROJECT_BRAILLE_ALPHA_THRESHOLD: u8 = 102;
static PROJECT_BRAILLE: LazyLock<PixelCanvas> = LazyLock::new(|| {
    rgba_to_canvas(CanvasMode::Braille, BRAILLE_ICON_CELLS_WIDE, BRAILLE_ICON_CELLS_TALL, PROJECT_BRAILLE_RGBA, PROJECT_BRAILLE_ALPHA_THRESHOLD)
});

// ---- Settings gear --------------------------------------------------------

const SETTINGS_GEAR_RGBA: &[u8] = include_bytes!("icons/settings_gear.rgba");
static SETTINGS_GEAR_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel(SETTINGS_GEAR_RGBA));

const SETTINGS_GEAR_BRAILLE_RGBA: &[u8] = include_bytes!("icons/settings_gear_braille.rgba");
/// ~35% coverage (89/255), same reasoning as `PERSON_BRAILLE_ALPHA_THRESHOLD`:
/// the gear's own teeth are individually thin relative to the 8x12 dot
/// grid, and the lower end of the ~35-45% band is what keeps as much of
/// the teeth/notch texture as this resolution can show at all -- see
/// this crate's own handoff notes on this icon's braille legibility,
/// which reads as a round, hollow-centered blob rather than a crisp
/// multi-tooth gear (an honest resolution ceiling, not a threshold bug).
const SETTINGS_GEAR_BRAILLE_ALPHA_THRESHOLD: u8 = 89;
static SETTINGS_GEAR_BRAILLE: LazyLock<PixelCanvas> = LazyLock::new(|| {
    rgba_to_canvas(
        CanvasMode::Braille,
        BRAILLE_ICON_CELLS_WIDE,
        BRAILLE_ICON_CELLS_TALL,
        SETTINGS_GEAR_BRAILLE_RGBA,
        SETTINGS_GEAR_BRAILLE_ALPHA_THRESHOLD,
    )
});

// ---- Chevrons (sidebar collapse/expand) ------------------------------------

const CHEVRON_LEFT_RGBA: &[u8] = include_bytes!("icons/chevron_left.rgba");
static CHEVRON_LEFT_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel(CHEVRON_LEFT_RGBA));

const CHEVRON_LEFT_BRAILLE_RGBA: &[u8] = include_bytes!("icons/chevron_left_braille.rgba");
/// Deliberately BELOW the ~35-45% band the rest of this icon set uses:
/// even after the viewBox crop documented in this module's own "Bake
/// commands" doc comment (recovering the arrow from the dead padding VS
/// Code ships around it), the thin stroke's own peak area-coverage in
/// the best-covered dot measured ~34% while baking this asset -- a 35%+
/// cutoff lights ZERO dots, the exact "sparse dots" failure this whole
/// supersample rework exists to fix, just pushed one tier lower. 30%
/// (77/255) is the highest cutoff that still lights a legible two-stroke
/// chevron notch instead of nothing.
const CHEVRON_LEFT_BRAILLE_ALPHA_THRESHOLD: u8 = 77;
static CHEVRON_LEFT_BRAILLE: LazyLock<PixelCanvas> = LazyLock::new(|| {
    rgba_to_canvas(
        CanvasMode::Braille,
        BRAILLE_ICON_CELLS_WIDE,
        BRAILLE_ICON_CELLS_TALL,
        CHEVRON_LEFT_BRAILLE_RGBA,
        CHEVRON_LEFT_BRAILLE_ALPHA_THRESHOLD,
    )
});

const CHEVRON_RIGHT_RGBA: &[u8] = include_bytes!("icons/chevron_right.rgba");
static CHEVRON_RIGHT_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel(CHEVRON_RIGHT_RGBA));

const CHEVRON_RIGHT_BRAILLE_RGBA: &[u8] = include_bytes!("icons/chevron_right_braille.rgba");
/// See `CHEVRON_LEFT_BRAILLE_ALPHA_THRESHOLD`'s own doc comment -- the
/// mirrored glyph has the same stroke width and the same measured ~34%
/// peak coverage, so the same below-band 30% (77/255) cutoff applies.
const CHEVRON_RIGHT_BRAILLE_ALPHA_THRESHOLD: u8 = 77;
static CHEVRON_RIGHT_BRAILLE: LazyLock<PixelCanvas> = LazyLock::new(|| {
    rgba_to_canvas(
        CanvasMode::Braille,
        BRAILLE_ICON_CELLS_WIDE,
        BRAILLE_ICON_CELLS_TALL,
        CHEVRON_RIGHT_BRAILLE_RGBA,
        CHEVRON_RIGHT_BRAILLE_ALPHA_THRESHOLD,
    )
});

#[cfg(test)]
mod tests {
    use super::*;
    use uzor_tui::buffer::TerminalBuffer;
    use uzor_tui::rect::Rect;

    #[test]
    fn every_sixel_rgba_matches_its_own_declared_dimensions() {
        let expected = (SIXEL_ICON_WIDTH_PX * SIXEL_ICON_HEIGHT_PX * 4) as usize;
        for id in RailIconId::ALL {
            assert_eq!(sixel_source_rgba(id).len(), expected, "{id:?} sixel rgba length");
        }
    }

    #[test]
    fn every_braille_rgba_matches_its_own_declared_dimensions() {
        let expected = (BRAILLE_ICON_WIDTH_PX * BRAILLE_ICON_HEIGHT_PX * 4) as usize;
        for id in RailIconId::ALL {
            assert_eq!(braille_source_rgba(id).len(), expected, "{id:?} braille rgba length");
        }
    }

    #[test]
    fn every_braille_canvas_pixel_dimensions_match_the_baked_asset() {
        for id in RailIconId::ALL {
            let canvas = braille(id);
            assert_eq!(canvas.px_width(), BRAILLE_ICON_WIDTH_PX, "{id:?}");
            assert_eq!(canvas.px_height(), BRAILLE_ICON_HEIGHT_PX, "{id:?}");
        }
    }

    #[test]
    fn every_braille_canvas_lights_at_least_one_dot() {
        for id in RailIconId::ALL {
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
    fn every_sixel_encodes_to_a_non_empty_dcs_sequence() {
        for id in RailIconId::ALL {
            let encoded = sixel(id);
            assert!(encoded.starts_with('\u{1b}'), "{id:?} sixel output must start with the DCS introducer ESC");
            assert!(encoded.len() > 16, "{id:?} sixel output for a 40x40 icon with real ink must not be a near-empty stub");
        }
    }
}
