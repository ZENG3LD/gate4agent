//! Native pixel rasterisation of the app's own CURRENT frame --
//! `control_plane`'s `CaptureFrame` verb (see that module's own doc
//! comment for the wire shape) is the one caller of
//! [`render_frame_png`].
//!
//! "Native" means the app rasterising what it drew, from its own state --
//! never a screen grab (`PrintWindow`/`CopyFromScreen`, this crate's own
//! prior only way to SEE the app, see `gate4agent/CLAUDE.md`'s own
//! "Screenshotting and driving the window" section) and never a second,
//! independent renderer either: this module runs the SAME
//! `render::render` entry point `control_plane::dump_frame` and every
//! real redraw tick already call, then paints the resulting
//! `TerminalBuffer`'s cells -- their resolved foreground, background, and
//! [`Modifier`] bits -- and the SAME `render::render` call's own
//! [`LayoutRects::sixel_icons`] placements, straight into an RGBA canvas.
//! [`crate::png_encode`] then turns that canvas into PNG bytes; this
//! module owns everything upstream of that (rasterisation), that one
//! owns everything downstream (byte-level PNG assembly) -- the same
//! `icons.rs` (assets + compositing) / `render.rs` (drawing) split this
//! crate already uses elsewhere.
//!
//! ## What this DOES cover that `DumpFrame`'s plain-text projection cannot
//!
//! Every cell's resolved foreground, background, and [`Modifier`] bits --
//! [`paint_cell`]'s own doc comment has the exact pixel encoding each one
//! gets. Every `Rail`/`Strip`/`Gallery`/`Compact` baked sixel icon
//! placement `render::render` produced this frame -- [`blit_sixel_icon`]
//! composites the SAME raw RGBA bytes `client::flush_sixel_icon_into`
//! would sixel-encode and print, at the SAME cell position, using the
//! SAME compositing arithmetic ([`icons::composite_over_background`]),
//! reachable because those bytes are ordinary baked-in app state
//! (`icons::rail_source_rgba`/`strip_source_rgba`/`gallery_source_rgba`/
//! `compact_source_rgba`), not something only the sixel encoder itself
//! ever sees.
//!
//! ## What this does NOT cover (say so plainly, not silently)
//!
//! The Pet Bastion arcade board's own PIXEL-tier overlay
//! ([`crate::app::LayoutRects::pet_arcade_pixel_frame`]) is NOT
//! reachable as RGBA at capture time and is NOT painted into this
//! canvas. `render::render_pet_arcade` composes that overlay's RGBA
//! canvas (`gate4agent_arcade_engine::compose_frame`, driven by
//! interpolated, continuously-moving sim state -- tick-boundary
//! snapshots, wall-clock tick-alpha, aging combat effects) and
//! immediately sixel-ENCODES it (`encode_frame`) without ever storing the
//! RGBA canvas anywhere `App`/`LayoutRects` keeps: `PetArcadePixelPlacement::
//! encoded` (what DOES survive into `LayoutRects`) is already sixel text
//! bytes, ready to `Print`, never pixels. Recovering pixels from it would
//! mean either decoding sixel back to RGBA (a real sixel DECODER --
//! exactly the "second rasteriser" this module exists to avoid adding) or
//! re-running `compose_frame`'s own interpolation/effects pipeline a
//! second time here purely for this capture (real drift risk against the
//! real render path, for a debug verb that must never show something the
//! app did not actually draw). Neither is done. When the arcade board is
//! open on the `Pixel` visual tier, this capture still shows that board's
//! own GLYPH-tier paint underneath (`render_pet_arcade` always paints that
//! tier first, unconditionally -- the pixel tier only ever layers OVER
//! it for the real terminal) via the ordinary per-cell loop below, so the
//! PNG is informative there, just not sub-cell-accurate for that one
//! overlay.
//!
//! Glyph SHAPES are not reproduced either -- there is no font rasteriser
//! anywhere in this crate's own dependency tree reachable without adding
//! one (`uzor_tui::canvas`'s own doc comment: "there is no font to shape
//! text metrics against"; `cosmic-text`/`swash` are real, already-
//! compiled entries in this workspace's `Cargo.lock`, but only as
//! transitive dependencies of `uzor` -- the full desktop GUI framework --
//! via `uzor-text`, never usable from this crate's own code without
//! declaring either as a NEW direct dependency, which this delivery
//! deliberately did not do; see `crate::png_encode`'s own top doc comment
//! for the same reasoning applied to PNG encoding). [`paint_cell`] draws
//! each non-space cell's own ink as a plain coverage block in its
//! resolved foreground colour instead -- enough to SEE which cells carry
//! ink and in what colour/weight/decoration, not enough to read the
//! actual letterforms. A defect in colour, background, or attribute
//! styling is fully visible in this PNG; a defect in which GLYPH a font
//! would have shaped is not.

use uzor_tui::{Cell, Color, Modifier, Style, TerminalBuffer};

use crate::app::{App, SixelIconPlacement, SixelIconSize};
use crate::icons;
use crate::png_encode;
use crate::pty_palette;

/// [`render_frame_png`]'s own result -- the PNG bytes plus the geometry
/// `control_plane::capture_frame` needs to fill in its own wire reply
/// without re-deriving it from `png` a second time.
pub(crate) struct CapturedFrame {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) width_px: u32,
    pub(crate) height_px: u32,
    pub(crate) png: Vec<u8>,
}

/// Renders `app`'s CURRENT frame (`app.terminal_cols` x `app.terminal_rows`,
/// clamped to at least 1x1 so a degenerate 0-sized terminal still produces
/// a valid, if trivial, PNG rather than an empty buffer `png_encode`
/// cannot size) to PNG bytes -- see this module's own top doc comment for
/// exactly what pixel content this does and does not contain.
pub(crate) fn render_frame_png(app: &App) -> CapturedFrame {
    let cols = app.terminal_cols.max(1);
    let rows = app.terminal_rows.max(1);
    let (width_px, height_px, canvas) = paint_frame_rgba(app, cols, rows);
    let png = png_encode::encode_png(width_px, height_px, &canvas);
    CapturedFrame { cols, rows, width_px, height_px, png }
}

/// The rasterisation half of [`render_frame_png`], split out so
/// `#[cfg(test)]` code in this module can assert on raw pixels without
/// also exercising [`png_encode::encode_png`] every time.
fn paint_frame_rgba(app: &App, cols: u16, rows: u16) -> (u32, u32, Vec<u8>) {
    let mut buffer = TerminalBuffer::new(cols, rows);
    // The SAME call `control_plane::dump_frame` makes, and the SAME one
    // every real redraw tick makes (`client::run`) -- this capture can
    // never show a frame the app is not actually capable of drawing to a
    // real terminal. Unlike `dump_frame`, this module KEEPS the returned
    // `LayoutRects` (`dump_frame`'s own doc comment explains why IT
    // discards it: a read-only verb must never perturb the last REAL
    // frame's own hit-testing state -- that reasoning is unchanged here,
    // this is a local variable, never written back to `app.layout`) --
    // its `sixel_icons` placements are exactly what [`blit_sixel_icon`]
    // needs.
    let layout = crate::render::render(app, &mut buffer);

    let cell_width = icons::ASSUMED_CELL_WIDTH_PX;
    let cell_height = icons::ASSUMED_CELL_HEIGHT_PX;
    let width_px = u32::from(cols) * cell_width;
    let height_px = u32::from(rows) * cell_height;
    let mut canvas = vec![0u8; (width_px as usize) * (height_px as usize) * 4];

    for row in 0..rows {
        for col in 0..cols {
            paint_cell(&mut canvas, width_px, height_px, col, row, buffer.get(col, row), app.terminal_background);
        }
    }
    for placement in &layout.sixel_icons {
        blit_sixel_icon(&mut canvas, width_px, height_px, app, &buffer, placement);
    }

    (width_px, height_px, canvas)
}

/// Paints one terminal cell's own [`icons::ASSUMED_CELL_WIDTH_PX`] x
/// [`icons::ASSUMED_CELL_HEIGHT_PX`] pixel box at `(col, row)` -- always
/// the resolved background first (fills the whole box), then, unless
/// [`Modifier::HIDDEN`] is set, an "ink" representation of the cell's own
/// foreground/attributes (see this module's own top doc comment for why
/// this is a coverage block, never a real glyph shape):
///
/// - Non-space content ([`Cell::symbol`] is not all whitespace) paints an
///   inset ink rectangle in the resolved foreground colour.
///   [`Modifier::BOLD`] shrinks the inset (a visibly THICKER block);
///   [`Modifier::DIM`] blends that colour 50% toward the background
///   (lower contrast) instead of resizing anything -- the two attributes
///   are independent in real terminals and stay independent here.
/// - [`Modifier::UNDERLINE`] draws a 2px bar across the cell's own bottom
///   edge, and [`Modifier::STRIKETHROUGH`] one through its own vertical
///   centre -- both independent of whether the cell carries ink at all
///   (a run of underlined spaces is visibly underlined in a real
///   terminal too).
/// - [`Modifier::REVERSE`] is resolved BEFORE any of the above by simply
///   swapping the two resolved colours ([`resolve_cell_colors`]) -- every
///   subsequent step (background fill, ink, bars) already reads the
///   swapped pair, so there is no separate reverse-specific drawing path.
/// - [`Modifier::ITALIC`]/[`Modifier::BLINK`] have no pixel effect here:
///   italic needs real glyph shaping to mean anything beyond an arbitrary
///   skew (see this module's own top doc comment on why no glyph
///   rasteriser is used at all), and blink cannot be shown in a single
///   still frame by definition.
fn paint_cell(
    canvas: &mut [u8],
    canvas_width_px: u32,
    canvas_height_px: u32,
    col: u16,
    row: u16,
    cell: &Cell,
    terminal_background: (u8, u8, u8),
) {
    let cell_width = icons::ASSUMED_CELL_WIDTH_PX;
    let cell_height = icons::ASSUMED_CELL_HEIGHT_PX;
    let origin_x = u32::from(col) * cell_width;
    let origin_y = u32::from(row) * cell_height;
    let (fg, bg) = resolve_cell_colors(cell.style, terminal_background);
    fill_rect(canvas, canvas_width_px, canvas_height_px, origin_x, origin_y, cell_width, cell_height, bg);

    if cell.style.modifiers.contains(Modifier::HIDDEN) {
        return;
    }

    if !cell.symbol.trim().is_empty() {
        let ink_color = if cell.style.modifiers.contains(Modifier::DIM) { blend(fg, bg, 0.5) } else { fg };
        let (inset_side, inset_top, inset_bottom) =
            if cell.style.modifiers.contains(Modifier::BOLD) { (1, 1, 2) } else { (2, 2, 3) };
        fill_rect(
            canvas,
            canvas_width_px,
            canvas_height_px,
            origin_x + inset_side,
            origin_y + inset_top,
            cell_width.saturating_sub(inset_side * 2),
            cell_height.saturating_sub(inset_top + inset_bottom),
            ink_color,
        );
    }

    if cell.style.modifiers.contains(Modifier::UNDERLINE) {
        fill_rect(canvas, canvas_width_px, canvas_height_px, origin_x + 1, origin_y + cell_height.saturating_sub(2), cell_width.saturating_sub(2), 2, fg);
    }
    if cell.style.modifiers.contains(Modifier::STRIKETHROUGH) {
        fill_rect(canvas, canvas_width_px, canvas_height_px, origin_x + 1, origin_y + cell_height / 2, cell_width.saturating_sub(2), 2, fg);
    }
}

/// Resolves a [`Style`]'s `fg`/`bg` to concrete, non-quantized RGB
/// triples and applies [`Modifier::REVERSE`] (a plain swap) -- see
/// [`resolve_color`]'s own doc comment for the colour math, and
/// [`paint_cell`]'s own doc comment for why REVERSE is handled exactly
/// here, once, rather than at every later drawing step. `Color::Reset`
/// means two different things depending on which side it is on: as a
/// FOREGROUND, there is no real terminal here to defer to, so it
/// resolves to [`pty_palette::GATE_FG_RGB`] (this app's own default ink
/// colour); as a BACKGROUND, `terminal_background` (`app.terminal_
/// background`, this app's own ONE measured fact about the real
/// terminal's colours -- see `crate::terminal_bg`'s own doc comment) is
/// the closer-to-truth answer, and this crate already resolves `Color::
/// Reset` backgrounds against exactly this value for sixel compositing
/// (`icons::resolve_variant_background`'s own `PtyColorMode::Inherited`
/// arm) -- reusing the same fact here keeps this capture consistent with
/// what the app already believes its own background to be, rather than
/// inventing a second opinion.
fn resolve_cell_colors(style: Style, terminal_background: (u8, u8, u8)) -> ((u8, u8, u8), (u8, u8, u8)) {
    let fg = resolve_color(style.fg, pty_palette::GATE_FG_RGB);
    let bg = resolve_color(style.bg, terminal_background);
    if style.modifiers.contains(Modifier::REVERSE) { (bg, fg) } else { (fg, bg) }
}

/// Resolves one [`Color`] to a concrete RGB triple. Deliberately NOT
/// `pty_palette::gate_foreground`'s own resolution: that function snaps
/// every `Indexed`/`Rgb` colour to the NEAREST of 16 catppuccin swatches
/// ([`pty_palette`]'s own `nearest_swatch`) -- a real, deliberate
/// `PtyColorMode::GateOverride` aesthetic choice for what a PTY pane
/// paints, not a fact about what colour a cell's `Style` actually carries.
/// A native capture exists to show exactly what the app drew; re-
/// quantizing a true 24-bit colour down to 16 swatches on the way into
/// the PNG would make this capture LESS faithful than the real terminal
/// in `PtyColorMode::Inherited` (where the PTY's own true colour reaches
/// the terminal unchanged). `Color::Rgb`/`Color::Indexed` therefore
/// resolve exactly ([`pty_palette::indexed_rgb`]'s own xterm 216-cube/
/// greyscale arithmetic, not a quantization); the 16 NAMED variants
/// (`Black`..`LightCyan`/`Gray`/`DarkGray`) have no finer truth to fall
/// back on than this app's own established 16-colour identity, so those
/// route through the SAME table via their own standard ANSI index
/// (`Gray` shares `White`'s own index 7, matching `pty_palette::
/// gate_foreground`'s own precedent for that one pair). `reset_rgb` is
/// the caller's own fallback for `Color::Reset` (fg and bg mean different
/// things for a reset colour -- see [`resolve_cell_colors`]'s own doc
/// comment).
fn resolve_color(color: Color, reset_rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Reset => reset_rgb,
        Color::Rgb(red, green, blue) => (red, green, blue),
        Color::Indexed(index) => pty_palette::indexed_rgb(index),
        Color::Black => pty_palette::indexed_rgb(0),
        Color::Red => pty_palette::indexed_rgb(1),
        Color::Green => pty_palette::indexed_rgb(2),
        Color::Yellow => pty_palette::indexed_rgb(3),
        Color::Blue => pty_palette::indexed_rgb(4),
        Color::Magenta => pty_palette::indexed_rgb(5),
        Color::Cyan => pty_palette::indexed_rgb(6),
        Color::White | Color::Gray => pty_palette::indexed_rgb(7),
        Color::DarkGray => pty_palette::indexed_rgb(8),
        Color::LightRed => pty_palette::indexed_rgb(9),
        Color::LightGreen => pty_palette::indexed_rgb(10),
        Color::LightYellow => pty_palette::indexed_rgb(11),
        Color::LightBlue => pty_palette::indexed_rgb(12),
        Color::LightMagenta => pty_palette::indexed_rgb(13),
        Color::LightCyan => pty_palette::indexed_rgb(14),
    }
}

/// Linear per-channel blend of `from` toward `to` by `amount` (`0.0` =
/// `from`, `1.0` = `to`) -- plain sRGB-byte-space lerp, not the gamma-
/// correct linear-light blend [`icons::composite_over_background`] uses
/// for baked icon anti-aliasing: [`Modifier::DIM`]'s own visual budget
/// here is "read as lower-contrast ink", not colour-managed precision, so
/// the cheap version is the right amount of engineering for it.
fn blend(from: (u8, u8, u8), to: (u8, u8, u8), amount: f64) -> (u8, u8, u8) {
    let lerp = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * amount).round() as u8;
    (lerp(from.0, to.0), lerp(from.1, to.1), lerp(from.2, to.2))
}

/// Fills a `width`x`height` pixel box at `(x, y)` with a flat colour,
/// clipped against `canvas_width_px`/`canvas_height_px` -- every caller
/// in this module already computes geometry that should land in-bounds,
/// but cell insets/bars are computed with plain (non-saturating in the
/// per-pixel loop) addition, so this stays defensive rather than trusting
/// every call site's own arithmetic never overshoots by a pixel at an
/// edge cell.
fn fill_rect(canvas: &mut [u8], canvas_width_px: u32, canvas_height_px: u32, x: u32, y: u32, width: u32, height: u32, color: (u8, u8, u8)) {
    for dy in 0..height {
        let py = y + dy;
        if py >= canvas_height_px {
            break;
        }
        for dx in 0..width {
            let px = x + dx;
            if px >= canvas_width_px {
                break;
            }
            let index = ((py * canvas_width_px + px) * 4) as usize;
            canvas[index] = color.0;
            canvas[index + 1] = color.1;
            canvas[index + 2] = color.2;
            canvas[index + 3] = 255;
        }
    }
}

/// Composites one baked sixel icon placement's own raw RGBA bytes
/// (`icons::rail_source_rgba`/`strip_source_rgba`/`gallery_source_rgba`/
/// `compact_source_rgba` -- see this module's own top doc comment) into
/// `canvas` at `placement.rect`'s own pixel position, using the SAME
/// [`icons::composite_over_background`] arithmetic
/// `icons::sixel_family`/etc. run before sixel-encoding -- reused
/// directly rather than re-derived, and called uncached (unlike
/// `icons.rs`'s own `SIXEL_CACHE`): a frame capture happens at most once
/// per operator request, never once per real repaint, so the cache's own
/// reason to exist (amortizing a cost paid every frame) does not apply.
///
/// `Rail`/`Strip`/`Gallery` composite against [`icons::resolve_variant_
/// background`]'s own resolved colour (the SAME flat background the real
/// terminal's sixel raster is composited against) -- correct because
/// `composite_over_background`'s output RGB already equals that
/// background at every pixel the icon's own glyph does not cover (see
/// that function's own doc comment), so painting every pixel
/// unconditionally reproduces the real result without needing this
/// function to separately track which pixels are "covered". `Compact`
/// has no such flat background in the real render (real sixel
/// transparency, see `icons::sixel_compact_family`'s own doc comment);
/// here it composites against the ONE terminal cell its own 1x1-cell
/// placement covers (`buffer.get(placement.rect.x, placement.rect.y)`,
/// resolved the SAME way [`paint_cell`] already resolved that cell's own
/// background) -- a PNG canvas has no "let the terminal show through"
/// concept to fall back on, so this is the closest available truth for
/// what pixel colour would show around this icon's own edges.
fn blit_sixel_icon(
    canvas: &mut [u8],
    canvas_width_px: u32,
    canvas_height_px: u32,
    app: &App,
    buffer: &TerminalBuffer,
    placement: &SixelIconPlacement,
) {
    let (raw, icon_width, icon_height) = match placement.size {
        SixelIconSize::Rail => (icons::rail_source_rgba(placement.icon, placement.family), icons::SIXEL_ICON_WIDTH_PX, icons::SIXEL_ICON_HEIGHT_PX),
        SixelIconSize::Strip => (icons::strip_source_rgba(placement.icon, placement.family), icons::STRIP_SIXEL_ICON_WIDTH_PX, icons::STRIP_SIXEL_ICON_HEIGHT_PX),
        SixelIconSize::Gallery => (icons::gallery_source_rgba(placement.icon, placement.family), icons::GALLERY_SIXEL_ICON_WIDTH_PX, icons::GALLERY_SIXEL_ICON_HEIGHT_PX),
        SixelIconSize::Compact => (icons::compact_source_rgba(placement.icon, placement.family), icons::COMPACT_SIXEL_ICON_WIDTH_PX, icons::COMPACT_SIXEL_ICON_HEIGHT_PX),
    };
    // A documented Lucide mapping gap (see `icons.rs`'s own "Lucide" doc
    // section) -- this placement's own themed body was already painted by
    // `paint_cell` above; skipping the icon glyph here matches exactly
    // what the real terminal shows for the same gap (`client::flush_
    // sixel_icon_into`'s own `None` arm).
    let Some(raw) = raw else { return };

    let background = match placement.size {
        SixelIconSize::Compact => {
            if placement.rect.x >= buffer.width() || placement.rect.y >= buffer.height() {
                return;
            }
            let (_, bg) = resolve_cell_colors(buffer.get(placement.rect.x, placement.rect.y).style, app.terminal_background);
            bg
        }
        SixelIconSize::Rail | SixelIconSize::Strip | SixelIconSize::Gallery => {
            icons::resolve_variant_background(placement.variant, app.color_mode, app.terminal_background)
        }
    };
    let composited = icons::composite_over_background(raw, background);

    let origin_x = u32::from(placement.rect.x) * icons::ASSUMED_CELL_WIDTH_PX;
    let origin_y = u32::from(placement.rect.y) * icons::ASSUMED_CELL_HEIGHT_PX;
    for y in 0..icon_height {
        let dst_y = origin_y + y;
        if dst_y >= canvas_height_px {
            break;
        }
        for x in 0..icon_width {
            let dst_x = origin_x + x;
            if dst_x >= canvas_width_px {
                break;
            }
            let src = ((y * icon_width + x) * 4) as usize;
            let dst = ((dst_y * canvas_width_px + dst_x) * 4) as usize;
            canvas[dst] = composited[src];
            canvas[dst + 1] = composited[src + 1];
            canvas[dst + 2] = composited[src + 2];
            canvas[dst + 3] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use uzor_tui::Rect;

    use super::*;
    use crate::app::IconFamily;
    use crate::icons::{IconId, SixelVariant};

    /// The dimension contract [`control_plane::CaptureFrame`]'s own reply
    /// promises: `width_px`/`height_px` are exactly `cols`/`rows` times
    /// [`icons::ASSUMED_CELL_WIDTH_PX`]/[`icons::ASSUMED_CELL_HEIGHT_PX`],
    /// and the PNG bytes actually decode to those same dimensions -- not
    /// just numbers reported in the struct without a matching real image
    /// behind them. `20x5` matches `control_plane::tests::apply_dump_
    /// frame_reports_the_configured_terminal_size`'s own size, a value
    /// already proven safe for a full `render::render` pass.
    #[test]
    fn render_frame_png_dimensions_match_cols_rows_times_the_assumed_cell_size() {
        let mut app = App::default();
        app.terminal_cols = 20;
        app.terminal_rows = 5;
        let frame = render_frame_png(&app);
        assert_eq!(frame.cols, 20);
        assert_eq!(frame.rows, 5);
        assert_eq!(frame.width_px, 20 * icons::ASSUMED_CELL_WIDTH_PX);
        assert_eq!(frame.height_px, 5 * icons::ASSUMED_CELL_HEIGHT_PX);

        let (decoded_width, decoded_height, decoded_rgba) = png_encode::decode_for_test(&frame.png);
        assert_eq!(decoded_width, frame.width_px);
        assert_eq!(decoded_height, frame.height_px);
        assert_eq!(decoded_rgba.len(), (frame.width_px as usize) * (frame.height_px as usize) * 4);
    }

    /// THE pin `dump_frame`'s own plain-text projection cannot make: two
    /// otherwise-identical cells (same glyph) whose `Style` differs must
    /// paint DIFFERENT pixels. Exercises colour (fg/bg) AND an attribute
    /// (`UNDERLINE`) in one style, rather than either alone, so this
    /// stands as proof for both halves of this module's own "every
    /// cell's fg/bg/attributes as styled" mandate at once.
    #[test]
    fn a_styled_cell_paints_different_pixels_than_an_unstyled_one() {
        let cell_width = icons::ASSUMED_CELL_WIDTH_PX;
        let cell_height = icons::ASSUMED_CELL_HEIGHT_PX;
        let box_len = (cell_width as usize) * (cell_height as usize) * 4;

        let mut plain_canvas = vec![0u8; box_len];
        paint_cell(&mut plain_canvas, cell_width, cell_height, 0, 0, &Cell::new("A"), (0, 0, 0));

        let mut styled_canvas = vec![0u8; box_len];
        let styled_style = Style::default()
            .fg(Color::Rgb(255, 0, 0))
            .bg(Color::Rgb(0, 255, 0))
            .add_modifier(Modifier::UNDERLINE);
        paint_cell(&mut styled_canvas, cell_width, cell_height, 0, 0, &Cell::styled("A", styled_style), (0, 0, 0));

        assert_ne!(plain_canvas, styled_canvas);
    }

    /// [`Modifier::REVERSE`] must swap the painted colours, not just be
    /// recorded somewhere unseen -- the top-left pixel (background fill)
    /// of a reversed cell must equal the UNREVERSED cell's own foreground
    /// ink colour, and vice versa via the ink block.
    #[test]
    fn reverse_modifier_swaps_the_painted_background_and_foreground() {
        let cell_width = icons::ASSUMED_CELL_WIDTH_PX;
        let cell_height = icons::ASSUMED_CELL_HEIGHT_PX;
        let box_len = (cell_width as usize) * (cell_height as usize) * 4;
        let style = Style::default().fg(Color::Rgb(255, 0, 0)).bg(Color::Rgb(0, 255, 0));

        let mut canvas = vec![0u8; box_len];
        paint_cell(&mut canvas, cell_width, cell_height, 0, 0, &Cell::styled(" ", style), (0, 0, 0));
        assert_eq!(&canvas[0..3], &[0, 255, 0], "unreversed top-left pixel must be the background colour");

        let mut reversed_canvas = vec![0u8; box_len];
        let reversed_style = style.add_modifier(Modifier::REVERSE);
        paint_cell(&mut reversed_canvas, cell_width, cell_height, 0, 0, &Cell::styled(" ", reversed_style), (0, 0, 0));
        assert_eq!(&reversed_canvas[0..3], &[255, 0, 0], "reversed top-left pixel must be the foreground colour");
    }

    /// Proves [`blit_sixel_icon`] actually reaches `icons::rail_source_
    /// rgba`/[`icons::composite_over_background`] and writes real pixels
    /// for a placement -- the sixel/raster-reachability half of this
    /// module's own mandate, not just the per-cell styling half. `Files`
    /// is a Codicons-family icon with no Lucide-style mapping gap, so
    /// this never depends on which gap happens to exist today.
    #[test]
    fn sixel_icon_placement_paints_the_resolved_background_into_the_canvas() {
        let app = App::default();
        let cols = icons::SIXEL_ICON_CELLS_WIDE;
        let rows = icons::SIXEL_ICON_CELLS_TALL;
        let buffer = TerminalBuffer::new(cols, rows);
        let width_px = u32::from(cols) * icons::ASSUMED_CELL_WIDTH_PX;
        let height_px = u32::from(rows) * icons::ASSUMED_CELL_HEIGHT_PX;
        let mut canvas = vec![0u8; (width_px as usize) * (height_px as usize) * 4];

        let placement = SixelIconPlacement {
            icon: IconId::Files,
            rect: Rect::new(0, 0, cols, rows),
            variant: SixelVariant::GateActive,
            size: SixelIconSize::Rail,
            family: IconFamily::Codicons,
        };
        blit_sixel_icon(&mut canvas, width_px, height_px, &app, &buffer, &placement);

        // `App::default()`'s own `color_mode` is `PtyColorMode::
        // GateOverride` (that enum's own `#[default]`), which resolves
        // `SixelVariant::GateActive` to `icons::OVERRIDE_ACTIVE_BG`
        // (30, 30, 46) regardless of `terminal_background` -- a codicon's
        // own glyph is inset/padded well away from its baked canvas's
        // corner (`tools/bake_icons.py`'s own lattice-fit padding), so
        // the top-left pixel is reliably UNCOVERED ink and must equal
        // that resolved background exactly.
        assert_eq!(&canvas[0..4], &[30, 30, 46, 255]);
    }
}
