#!/usr/bin/env python3
"""Bake the gate4agent-tui icon catalog from microsoft/vscode-codicons,
ALONGSIDE a second bake of the same `IconId` set from lucide-icons/lucide
(never a replacement -- codicons stay the default, see `app::IconFamily`).

Licence (codicons):  MIT (microsoft/vscode-codicons, <https://github.com/
          microsoft/vscode-codicons/blob/main/LICENSE>). Redistributing
          the baked RGBA raster derived from these SVGs under this
          crate's own licence is permitted by codicons' MIT terms; this
          header is the attribution.
Source (codicons):   raw.githubusercontent.com/microsoft/vscode-codicons/
          main/src/icons/<slug>.svg -- one file per `MANIFEST` entry.

Licence (Lucide):    read directly from <https://github.com/lucide-icons/
          lucide/blob/main/LICENSE> at authoring time (do not assume --
          verify): the ISC License (Copyright (c) 2026 Lucide Icons and
          Contributors) covers the set as a whole; a named subset of
          icons "derived from the Feather project" is ADDITIONALLY under
          the MIT License (Copyright (c) 2013-present Cole Bemis) per
          that same file's own second block -- both permissive, both
          permit redistributing a derived raster under this crate's own
          licence, same as codicons above; this header is the
          attribution. Several of `LUCIDE_SLUGS`' own values below fall
          in the named Feather subset (e.g. `arrow-down`, `check`,
          `chevron-left`, `info`, `search`, `trash`) -- covered either
          way, so this header does not split by which licence applies to
          which icon.
Source (Lucide):     raw.githubusercontent.com/lucide-icons/lucide/main/
          icons/<slug>.svg -- one file per `LUCIDE_SLUGS` entry below.

Re-run:   python tools/bake_icons.py            (from this crate's root,
                                                   "crates/gate4agent-tui")
          python tools/bake_icons.py --force     (re-bake every icon, even
                                                   ones already on disk,
                                                   both families)
          python tools/bake_icons.py --only add,close   (restrict to a
                                                   subset (codicon slugs)
                                                   -- for hand comparison;
                                                   does NOT regenerate
                                                   catalog.rs, see below)

What this does, every run:
  1. For each `MANIFEST` entry, ensure its CODICON source SVG is present
     in `--cache-dir` (default `tools/.codicon-cache/`, gitignored -- a
     scratch mirror of upstream, not a source of truth): download once,
     reuse on every later run. A 404 aborts the ENTIRE run immediately
     with the offending slug and URL named in the error -- never silently
     skipped, never substituted automatically (see ICON LIST substitution
     policy below). Separately, for each `LUCIDE_SLUGS` entry, the same
     fetch-once/reuse/fail-loudly contract against `.lucide-cache/`
     (`fetch_lucide_svg`) -- the two caches never share a slug namespace,
     so neither family's own scratch files can collide with the other's.
  2. Patch the codicon SVG's `fill="currentColor"` (present exactly once,
     on the root `<svg>` element, for every codicon in this set --
     verified against all 57 source files at authoring time) to
     `#cdd6f4`, this crate's own `pty_palette::GATE_FG` -- the same patch
     the original 7-icon rail catalog already applied, so every tier
     reads as part of the existing theme. Separately, patch the Lucide
     SVG's `stroke="currentColor"` to the SAME `#cdd6f4` and its
     `stroke-width="2"` to `LUCIDE_STROKE_WIDTH` (`patch_lucide`) --
     Lucide's ink lives in the stroke, not the fill (`fill="none"`
     throughout, untouched), which is the entire reason this family is
     worth baking alongside codicons at all: one number governs every
     glyph's line weight, unlike codicons' per-path fixed geometry.
  3. Rasterize every sixel tier (rail/compact/strip/gallery) via `resvg`
     (SVG -> PNG) + `ffmpeg` (PNG -> padded -> raw RGBA8) -- see
     `rasterize_sixel`/`rasterize_compact_sixel`/`rasterize_strip_sixel`/
     `rasterize_gallery_sixel` below for the exact filter graphs,
     reproduced in each function's own doc comment so a human can re-run
     the equivalent `resvg`/`ffmpeg` CLI invocations by hand without
     reading Python. Each tier's own rail/strip/gallery variant then
     derives its `_gate`/`_gate_active`/`_gate_accent` (exact background,
     gamma-correct blend) and plain `Transparent` (alpha-precorrected)
     outputs from that SAME rasterized buffer -- see `ensure_assets`'s
     own doc comment for why the ordering there matters. SKIPPED
     (idempotent, no network/subprocess work at all) for any icon whose
     outputs already exist on disk at the expected byte length, unless
     `--force`. Lucide reuses these EXACT SAME rasterize functions
     unchanged (see `ensure_lucide_assets` and friends, right below the
     codicon `ensure_*` functions) -- the lattice-fit machinery cause 6
     below describes is family-agnostic, only the fetch/patch step
     (step 1/2 above) differs.
  4. Regenerate `src/icons/catalog.rs` from the FULL manifest (only when
     not restricted by `--only`) -- one `IconId` enum variant (shared by
     both families), one codicon sixel `LazyLock<String>` per tier/
     variant, one Lucide `Option`-wrapped equivalent per tier/variant
     (`None` for the two `LUCIDE_GAPS` icons), one ascii literal, per
     icon.
  5. Bake `LUCIDE_SLUGS`' own assets (skipping `LUCIDE_GAPS` entirely --
     no file, no catalog entry, a real gap, not an invented substitute).
  6. Print a report: the full `IconId` -> Lucide slug mapping, the
     reported gaps and why, and asset-size totals for both families.

Nothing here is a build-time Cargo dependency -- `resvg`/`ffmpeg` run
once, offline, from a developer's own PATH, producing checked-in
`.rgba` files `include_bytes!`'d at compile time (see `src/icons.rs`).

## Quality pass (dirty edges / blurred strokes / gamma-space compositing /
## cell-height mismatch / control-strip resize / strip-tier glyph clutter)

Seven defects diagnosed against the running TUI, each verified against
this tool's own pipeline (not taken on faith) before fixing. Cause 4
below was RETIRED once cause 1's own fix was widened to cover every
`PtyColorMode`, not just `GateOverride` -- its own entry stays in place,
marked retired, so the numbering below still lines up with `icons.rs`'s
own doc comments and this crate's own git history.

1. DIRTY EDGES -- CONFIRMED, root cause identified precisely. Every sixel
   asset was baked with a transparent background and encoded via
   `icy_sixel::BackgroundMode::Transparent`. `icy_sixel` 0.6's own encoder
   (`encoder.rs::sixel_encode_impl`) applies a HARD alpha>=128 opacity
   threshold per pixel -- there is no partial-coverage/blend information
   in the encoded SIXEL stream at all, only "fully drawn, at this pixel's
   flat ink colour" or "fully undrawn". Windows Terminal's own sixel
   decoder does not implement the "undrawn -> show whatever is already
   there" transparency semantics DEC's P2=1 mode specifies, so "undrawn"
   pixels do not read as transparent in practice, and even where a
   partial-coverage pixel DOES survive the threshold, the only RGB this
   encoder ever sees for it is the flat, un-blended ink colour (see
   `../icons.rs`'s own module doc on straight alpha) -- so anti-aliasing
   cannot survive this encoder in alpha at all, only in RGB. FIX:
   composite EVERY sixel-tier icon (rail/strip/gallery -- compact is out
   of scope, see `ensure_compact_assets`'s own doc comment) over the EXACT
   background colour the button paints (`GATE_ACTIVE_BG_RGB`/
   `GATE_ACCENT_BG_RGB` below, hand-synced to `render.rs`'s own
   `ACTIVE_BG`/`MAUVE`), fully opaque, so the encoder's threshold and the
   terminal's transparency support both become irrelevant -- there is no
   transparent pixel left to mishandle, and the anti-aliasing this buys
   back rides in the RGB channels instead, where the encoder's own hard
   threshold cannot touch it. This is done as a SEPARATE, pure-Python
   compositing pass (`composite_over_background`) over an already-
   rasterized buffer -- never a second resvg/ffmpeg call -- so it can
   never regress into cause 2's own double-resampling anti-pattern. This
   USED to be possible only for `PtyColorMode::GateOverride`, whose panel/
   rail colours are fixed, known constants -- `PtyColorMode::Inherited`
   read as having no knowable exact background (crossterm has no reliable
   query for the terminal's own background colour, the same gap
   `icons.rs::ASSUMED_CELL_WIDTH_PX`'s own doc comment already names for
   cell-pixel size). That premise was the actual bug: the background was
   never unknowable at RENDER time, only un-PAINTED -- `render_rail_
   button`/`render_control_strip_button`/the icon gallery swatches simply
   left it to the terminal's own default instead of stating one. Now every
   icon-bearing button paints this SAME explicit truecolor background in
   EVERY `PtyColorMode` (see cause 4 below, retired), so there is exactly
   ONE composited asset per background to bake, never a mode-dependent
   pair. The rail tier has two backgrounds (`theme.active` at rest,
   `theme.accent` selected) so it gets two composited variants
   (`SixelVariant::GateActive`/`GateAccent`); the strip/gallery tiers below
   have exactly one (neither ever shows a selected state), so each gets
   one.

2. BLURRED STROKES -- diagnosed as "rasterized on a non-integer scale from
   a 24-unit source grid"; PARTIALLY CONFIRMED, PARTIALLY REFUTED once
   checked against the actual 57-icon manifest and the actual pipeline:
   - The "24-unit grid" premise is WRONG for most of this set: 52/57
     source SVGs use a 16x16 viewBox, only 4 use 24x24 (`files`,
     `settings-gear`, `source-control`, `terminal`) and 1 uses 24x25
     (`output`, already a documented exception elsewhere in this file).
   - The "second ffmpeg downscale" claim is REFUTED for every SIXEL tier:
     each rasterize function does exactly ONE resvg pass, directly at (or
     fit within) the target pixel size, followed only by a SAME-SIZE
     ffmpeg pad.
   - The underlying mechanism IS real, though: rasterizing a straight,
     axis-aligned 1-source-unit stroke at a size that is not an integer
     multiple of its own source grid measurably softens it. The strip/
     gallery tiers (~20x19/60x57px, forced by their own required cell
     footprint, not an integer multiple of either 16 or 24) use a single
     resvg AA pass DIRECTLY at that target size -- the third option this
     task's own original brief named ("the target size with resvg's own
     high-quality AA applied ONCE"), and the only one available without
     either breaking the required button footprint or reintroducing a
     second resampling pass. The rail/compact tiers keep their own
     analogous single-pass treatment (see cause 5 below for their own
     pixel-size fix, orthogonal to this one).

3. GAMMA-SPACE COMPOSITING (anti-aliased edges read grainy/washed-out,
   worst at the smallest tier) -- CONFIRMED. `composite_over_background`
   used to blend the straight 8-bit sRGB channel bytes directly (`out =
   ink*(a/255) + bg*(1-a/255)`), which is wrong: sRGB is a non-linear
   encoding of light, so a coverage weight (what `a` actually is here --
   resvg's own straight-alpha convention, see `../icons.rs`'s own module
   doc) must be blended in LINEAR light, not in the gamma-encoded byte
   domain. Against this crate's own ink `#cdd6f4` (204,214,242) on a
   near-black terminal background (12,12,12) the error is large and
   systematic (R channel): 25% coverage -> naive 60, correct 109; 50% ->
   naive 108, correct 150; 75% -> naive 156, correct 180 -- every
   anti-aliased edge pixel lands 24-49 levels too dark. A codicon stroke
   is ~1.5 units in a 24-unit viewBox -- at the strip tier's own ~19px
   height that is barely more than one device pixel, i.e. ALMOST ENTIRELY
   edge pixels, which is exactly why the small tier reads as grainy and
   washed out while the rail tier (a wider stroke in device pixels,
   surviving core ink pixels) reads acceptable. FIX: `composite_over_
   background` now converts both the ink and the background from sRGB to
   linear (`srgb_to_linear`, the exact piecewise transfer function -- the
   0.04045 / 12.92 / 2.4 form, NOT a 2.2-power approximation), blends by
   the pixel's own TRUE coverage in linear space, then converts back
   (`linear_to_srgb`). Governs every rail/strip/gallery sixel-tier output
   this tool ships now (`GateActive`/`GateAccent`, strip `_gate`, gallery
   `_gate`) -- since cause 4 below was retired, those pre-composited
   variants are the ONLY sixel-tier output those three tiers have, so this
   exact (never approximated) blend is what every anti-aliased pixel a
   user actually sees goes through, not a special case for one mode.

4. RETIRED -- "approximate the alpha for an unknown background" turned out
   not to be a real case. This tool used to ship a SECOND, `Transparent`
   sixel variant per rail/strip/gallery icon for `PtyColorMode::Inherited`
   (whose background it could not know at bake time), pre-correcting that
   variant's own ALPHA channel (`a' = 255 * (a/255)**(1/2.4)`) so a
   terminal's own naive gamma-space blend against an unknown background
   would land close to the gamma-correct result cause 3 above computes
   exactly. That whole approach solved the wrong problem: the background
   was never actually unknowable at RENDER time, only un-PAINTED --
   `render_rail_button`/`render_control_strip_button`/the icon gallery
   swatches simply left an icon-bearing button's own background to
   whatever the terminal already had there instead of stating one, the
   same gap cause 1 above now closes by removing it rather than
   approximating around it. With every icon-bearing button painting the
   SAME explicit truecolor background in every `PtyColorMode`, cause 3's
   own EXACT linear-light compositing applies universally and there is no
   more unknown-background asset left to approximate for at all --
   `SixelVariant::Transparent` (the Rust-side selector for that retired
   asset) no longer exists for the rail/strip/gallery tiers this fix
   covers, and this tool's own `precorrect_transparent_alpha` function
   went with it. The one tier this does NOT touch is `compact` (`ensure_
   compact_assets`) -- see that function's own doc comment for why it
   keeps real transparency and a real, still-necessary encoder-side
   BackgroundMode::Transparent, unrelated to this retired approximation.

5. CELL HEIGHT MISMATCH ("iconки неравномерно располагаются относительно
   подсветок" / rail icons overflow their own row) -- CONFIRMED. Every
   pixel-tier constant below was derived assuming a 10x20px terminal cell
   (`ASSUMED_CELL_WIDTH_PX`/`ASSUMED_CELL_HEIGHT_PX` -- keep these two
   numbers in sync BY HAND with the identically-named pair in
   `../icons.rs`, the same "no shared source of truth across the Python/
   Rust boundary" precedent every other pixel-size constant pair in this
   file already has). Measured against the owner's actual Windows
   Terminal / Cascadia Mono setup the real cell is 10x19, not 10x20: in a
   1129x635 window the rail's 6 columns span 60px (10.0px/col) and four
   consecutive gallery rows span 76px (19.0px/row) -- a 40px-tall rail
   icon (4 whole 10x20 cells... 2 rows at the OLD assumed 20px height)
   spans 40/19 = 2.1 real rows, i.e. it overflows its own 2-row (38px)
   cell footprint by 2px, bleeding into the row below. FIX:
   `ASSUMED_CELL_HEIGHT_PX` is now 19, and every tier's own HEIGHT
   constant is a whole multiple of it, FLOOR-rounded, never rounded up --
   undershooting a cell is safe (a blank pixel row inside the icon's own
   last cell); overshooting is not (it bleeds into whatever the next
   terminal row paints). This makes the rail tier's own pixel box
   NON-square for the first time (`SIXEL_PX_W`=40, `SIXEL_PX_H`=38, was
   40x40) -- `rasterize_sixel` below now fits a source icon within that
   non-square box by hand (`fit_within`, the same non-square-safe
   computation `rasterize_strip_sixel`/`rasterize_gallery_sixel` already
   used for their own already-non-square boxes) rather than relying on
   resvg's own `-w`/`-h` fit the way the old truly-square 40x40 box could.

6. FRACTIONAL-PIXEL STROKE LATTICE ("иконки читаются как точки, не линии"
   / icons read as dots, not lines, not a font glyph's own crisp bar) --
   CONFIRMED, exact mechanism identified. Every codicon in this manifest
   is built on a stroke lattice with EXACTLY 16 steps across its own
   viewBox: the 4 icons on a 24x24 viewBox (files/settings-gear/source-
   control/terminal) use a 1.5-unit stroke width (24/1.5 = 16 steps --
   verified against `files.svg`'s own structural path coordinates, every
   straight-segment endpoint a multiple of 1.5 except the rounded-corner
   arc control points, off-lattice by construction and meant to stay
   anti-aliased); the 52 icons on a 16x16 viewBox use a 1-unit stroke
   width (16/1 = 16 steps -- verified against `add.svg`: its plus-sign
   bar spans x=7..8 and y=7..8, both plain integers -- the SAME 16-step
   lattice at a different absolute scale, not a coincidence: codicons
   are one design system at two viewBox sizes). None of this tool's own
   per-tier target pixel sizes (`SIXEL_PX_W`x`_H` 40x38, `STRIP_SIXEL_PX_
   W`x`_H` 20x19, `GALLERY_SIXEL_PX_W`x`_H` 60x57) is a whole multiple of
   16, so the old single resvg pass fit directly to that size always
   landed the stroke lattice on a FRACTIONAL pixel -- measured on the
   actual shipped assets, alpha across a stroke: rail middle row 191,
   255, 227; strip middle row 227, 191, 191, 227 -- a smeared band, never
   a solid full-alpha core (no pixel boundary coincides with a stroke
   edge, so no pixel gets full coverage). Compare the strip/rail's own
   `─`/`│` divider glyphs elsewhere in this crate's UI: those are FONT
   glyphs, and DirectWrite grid-fits a font's stems to whole device
   pixels -- the difference the owner is seeing is grid fitting, not
   resolution. FIX: `rasterize_lattice_fit` renders the glyph at the
   LARGEST whole multiple of 16px that fits the tier's own canvas
   (`glyph_lattice_size`) -- 16px for the strip tier (1px strokes), 32px
   for the rail tier (2px strokes), 48px for the gallery tier (3px
   strokes) -- where the render scale (glyph_px / viewBox) is itself an
   exact multiple or reciprocal of 16, so every lattice-aligned stroke
   boundary maps to a whole pixel; the glyph is then padded onto the
   tier's own full canvas at an EXPLICIT, hand-computed INTEGER pixel
   offset -- never ffmpeg's own symbolic `(ow-iw)/2` expression, which
   this tool has no guarantee rounds to the SAME integer this fix's own
   correctness depends on (see `rasterize_lattice_fit`'s own doc
   comment). The one non-square source (`output.svg`, 24x25 -- cause 2
   above) cannot land both axes on the lattice at once: fitting within a
   SQUARE glyph box scales both axes by the SAME factor, dictated by
   whichever dimension is tighter (here, height), so its width axis ends
   up scaled by 16/25 rather than the lattice-exact 16/24 -- it keeps its
   own aspect ratio (never distorted to force alignment) at the cost of
   a slightly softer width-axis stroke; the one accepted, documented
   exception, same precedent as cause 2's own `output.svg` note. The
   compact tier's own canvas (10x19) cannot host a single 16px lattice
   step in its narrower dimension AT ALL (10 < 16), so `rasterize_
   compact_sixel` is UNCHANGED by this fix -- same single-pass-at-canvas-
   size treatment causes 2 and 5 already established for it; inventing a
   smaller lattice unit with no basis in the source design would be
   worse than leaving it as it already was. Curved and diagonal segments
   stay anti-aliased exactly as before -- this fix only ever changes
   scale/offset arithmetic feeding the SAME single resvg AA pass cause 2
   already established, never resvg's own anti-aliasing, and a curve
   cannot sit on an axis-aligned pixel lattice by definition. Gamma-
   correct compositing (cause 3) runs AFTER this, completely unchanged, on
   whatever buffer this produces -- this fix only changes WHERE the ink
   pixels land, never how they get colored.

7. STRIP-TIER GLYPH CLUTTER (owner: the 2x1 control-plane buttons read as
   having "какие-то полосы или линии... там просто что-то кроме
   необходимого" -- something beyond the necessary shape) -- CONFIRMED for
   `new-file`/`new-folder`. Both source SVGs carry codicons' own filled
   circle-with-plus "add" badge overlapping the file/folder body outline
   (verified against the cached source: the badge is a second, separately
   readable shape, not a decorative stroke inside one shape) -- at the
   strip tier's own 16px lattice-fit render (cause 6 above) the badge
   covers roughly a third of the glyph and collides with the body outline,
   reading as clutter rather than a single recognizable icon; this is the
   icon's own design, not a rasterization defect, so no pixel-pipeline fix
   applies. FIX: `IconSpec.strip_slug` lets a spec's STRIP-tier bake pull
   from a DIFFERENT source codicon than its rail/compact/gallery tiers --
   `NewFile`/`NewFolder` point their own strip bake at `file`/`folder`
   (the plain, badge-free glyphs this manifest already ships for
   `IconId::File`/`IconId::Folder`), while every other tier keeps the
   badge version, which reads fine at the rail/compact/gallery tiers'
   own larger sizes -- see `render::render_compact_icon_button`'s own doc
   comment for why the compact tier is large enough to keep the badge.
   This changes ONLY the strip-tier PIXELS for these two icons, never the
   Rust-side `IconId::NewFile`/`NewFolder` identity, their ascii labels,
   or what their buttons do. Every other icon actually used at the strip
   tier (`Add`, `Trash`, `Refresh`, `RepoForked`, `GoToFile` -- see
   `render.rs`'s own `ControlStripButton` call sites) was checked against
   the same "a glyph whose detail cannot survive 16px" question: `add`/
   `trash`/`refresh` are each a single coherent shape at the codicon
   set's own native 16px design size and read fine; `repo-forked`'s three
   small fork-node circles and `go-to-file`'s own compound file+arrow
   glyph are busier by design and worth the owner's own judgment call, but
   neither carries a SEPARATE overlapping badge the way `new-file`/`new-
   folder` did, so neither is substituted here -- flagged, not silently
   changed.

Also: every sixel encode (all tiers, all variants) goes through
`icons.rs::icon_encode_options()` -- `max_colors: 32` (the library's own
default is 256) and `diffusion: 0.0` (the library's own default is
Floyd-Steinberg dithering) -- "encode with a small explicit palette" per
this task's own original brief. A composited icon's true colour count is
just its own number of distinct alpha/coverage levels (measured on real
baked assets at authoring time: 6-26 across this manifest), so 32 is
generous headroom, not a visible compression; dithering exists to fake
extra apparent colours via spatial noise for photographic content and
only ever adds speckle noise to a flat-colour UI glyph like these, so it
is switched off outright rather than tuned down.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

CRATE_ROOT = Path(__file__).resolve().parent.parent
ICONS_DIR = CRATE_ROOT / "src" / "icons"
CATALOG_RS = ICONS_DIR / "catalog.rs"
DEFAULT_CACHE_DIR = Path(__file__).resolve().parent / ".codicon-cache"
# Alongside the codicon cache, gitignored, not a source of truth -- see
# `fetch_lucide_svg`'s own doc comment. No `--cache-dir`-style CLI override
# (the codicon one exists mainly for hand comparison against an alternate
# checkout; Lucide has no equivalent need yet).
DEFAULT_LUCIDE_CACHE_DIR = Path(__file__).resolve().parent / ".lucide-cache"

CODICON_URL_TEMPLATE = "https://raw.githubusercontent.com/microsoft/vscode-codicons/main/src/icons/{slug}.svg"
FILL_SOURCE = 'fill="currentColor"'
FILL_TARGET = 'fill="#cdd6f4"'  # this crate's pty_palette::GATE_FG

# ---- Assumed terminal cell size (cause 5 above) -- the single source of
# truth every pixel-tier box below is derived from. Keep these two
# numbers in sync BY HAND with `icons::ASSUMED_CELL_WIDTH_PX`/
# `ASSUMED_CELL_HEIGHT_PX` in `../src/icons.rs` -- there is no shared
# source of truth across the Python/Rust boundary, same precedent every
# other pixel-size constant pair in this file already has.
ASSUMED_CELL_WIDTH_PX = 10
ASSUMED_CELL_HEIGHT_PX = 19

# ---- Rail tier (the activity rail's own 4-cell x 2-row button body).
# Deliberately non-square (40x38, not 40x40) now that the cell itself is
# non-square -- see cause 5 above.
SIXEL_PX_W = ASSUMED_CELL_WIDTH_PX * 4
SIXEL_PX_H = ASSUMED_CELL_HEIGHT_PX * 2
SIXEL_RGBA_LEN = SIXEL_PX_W * SIXEL_PX_H * 4

# ---- Compact tier (dense single-row inline buttons -- Explorer/Git
# panel labelling, wave 1; see `icons.rs`'s own module doc for the tier's
# reasoning). Exactly ONE assumed terminal cell.
COMPACT_SIXEL_PX_W = ASSUMED_CELL_WIDTH_PX
COMPACT_SIXEL_PX_H = ASSUMED_CELL_HEIGHT_PX
COMPACT_SIXEL_RGBA_LEN = COMPACT_SIXEL_PX_W * COMPACT_SIXEL_PX_H * 4

# ---- Strip tier (sidebar content panels' own control-plane strip -- see
# `render::render_control_strip`/`render_control_strip_button`) -- 2 cells
# wide x 1 row tall, exactly `icons::STRIP_SIXEL_ICON_WIDTH_PX`/`_HEIGHT_
# PX` (keep these two numbers in sync with that Rust module by hand, same
# precedent as `COMPACT_SIXEL_PX_W`/`_H` above).
STRIP_SIXEL_PX_W = ASSUMED_CELL_WIDTH_PX * 2
STRIP_SIXEL_PX_H = ASSUMED_CELL_HEIGHT_PX
STRIP_SIXEL_RGBA_LEN = STRIP_SIXEL_PX_W * STRIP_SIXEL_PX_H * 4

# ---- Gallery tier (the icon gallery dev surface -- FIX4's own main
# deliverable, `app::SurfaceTab::IconGallery` / `render::
# render_icon_gallery`) -- the third of FIX2's three evenly-landing sizes
# on the assumed cell grid. The strip and rail tiers above already exist
# and are reused as-is by the gallery; this third size has no other UI
# consumer and so gets its own dedicated bake here, same recipe as the
# strip tier (`STRIP_SIXEL_PX_W`/`_H` above): a single resvg AA pass
# directly at (or fit within) the target size, exactly `icons::
# GALLERY_SIXEL_ICON_WIDTH_PX`/`_HEIGHT_PX` (keep these two numbers in
# sync with that Rust module by hand, same precedent as
# `STRIP_SIXEL_PX_W`/`_H`).
GALLERY_SIXEL_PX_W = ASSUMED_CELL_WIDTH_PX * 6
GALLERY_SIXEL_PX_H = ASSUMED_CELL_HEIGHT_PX * 3
GALLERY_SIXEL_RGBA_LEN = GALLERY_SIXEL_PX_W * GALLERY_SIXEL_PX_H * 4

# ---- Stroke lattice (cause 6's fix -- see this module's own header doc
# comment). Every codicon's own stroke lattice divides its viewBox into
# EXACTLY 16 steps (24-unit viewBox / 1.5-unit stroke, or 16-unit viewBox
# / 1-unit stroke -- the same design grid at two absolute scales), so
# rendering the glyph itself at any whole multiple of this many pixels
# maps every lattice-aligned stroke edge onto a whole device pixel. Not a
# per-tier constant -- see `glyph_lattice_size`/`rasterize_lattice_fit`.
LATTICE_STEP_PX = 16

# ---- Icon-button compositing background (cause 1's fix -- see this
# module's own header doc comment). Hand-synced to render.rs's own fixed
# theme constants: `ACTIVE_BG` (the rail/strip button body's own "at rest"
# colour) and `MAUVE` (`theme.accent`, the rail's own "selected" colour).
# Every icon-bearing button paints one of these two, EXPLICITLY, in every
# `PtyColorMode` -- there is no longer a mode this pair does not cover
# (see cause 4's own retirement note above).
GATE_ACTIVE_BG_RGB = (30, 30, 46)  # render.rs::ACTIVE_BG
GATE_ACCENT_BG_RGB = (203, 166, 247)  # render.rs::MAUVE / theme.accent


# ---- Lucide (owner-visible ALONGSIDE codicons, never a replacement --
# `app::IconFamily`, default `Codicons`) ------------------------------
#
# Licence: read directly from <https://github.com/lucide-icons/lucide/blob/
# main/LICENSE> at authoring time (do not assume -- verify): the ISC
# License (Copyright (c) 2026 Lucide Icons and Contributors) covers the
# set as a whole; a named subset of icons "derived from the Feather
# project" is ADDITIONALLY available under the MIT License (Copyright (c)
# 2013-present Cole Bemis) per that same LICENSE file's own second block
# -- both permissive, both permit redistributing a derived raster under
# this crate's own licence, same as codicons' MIT terms above. Several of
# this manifest's own LUCIDE_SLUGS values fall in that named Feather
# subset (e.g. `arrow-down`, `check`, `chevron-left`, `info`, `search`,
# `trash`) -- covered either way, so this header does not split the
# manifest by which of the two licences applies to which icon.
# Source: raw.githubusercontent.com/lucide-icons/lucide/main/icons/
# <slug>.svg -- one file per `LUCIDE_SLUGS` entry below, same per-icon
# fetch shape as `CODICON_URL_TEMPLATE` above.
LUCIDE_URL_TEMPLATE = "https://raw.githubusercontent.com/lucide-icons/lucide/main/icons/{slug}.svg"
LUCIDE_STROKE_SOURCE = 'stroke="currentColor"'
LUCIDE_STROKE_TARGET = 'stroke="#cdd6f4"'  # same pty_palette::GATE_FG FILL_TARGET patches codicons to
LUCIDE_WIDTH_SOURCE = 'stroke-width="2"'  # Lucide's own published default

# The one authored stroke width this tool bakes every Lucide asset at --
# see `app::LucideStrokeWidth`'s own doc comment for why only this ONE
# value is exposed as an owner-facing setting today. Chosen so it lands
# on a WHOLE device pixel at every tier's own `glyph_lattice_size` render
# (16px strip / 32px rail / 48px gallery -- the SAME lattice-fit machinery
# codicons already use, reused unchanged for Lucide below): Lucide's own
# viewBox is a fixed 24 units, so `LUCIDE_STROKE_WIDTH * (glyph_px / 24)`
# must be a whole number at all three glyph sizes at once. 16/24 = 2/3,
# 32/24 = 4/3, 48/24 = 2 -- for ALL THREE of those products to land on a
# whole number from one shared width, the width need only make the FIRST
# one (2/3) whole, since 4/3 and 2 are then automatically whole too (each
# is 2x/3x the first); the smallest positive value with that property is
# 1.5 (1.5 * 2/3 = 1, 1.5 * 4/3 = 2, 1.5 * 2 = 3 -- exactly 1px/2px/3px).
# Every further multiple of 1.5 (3.0, 4.5, ...) ALSO satisfies the same
# arithmetic, but this tool only ever bakes the one that has actually been
# rasterized and eyeballed against this manifest's own tightest glyphs
# (the parallel bars in `square-split-horizontal`/`_vertical`, the
# `ellipsis` dot spacing) without the ink crowding together -- offering an
# unverified thicker width in the Settings row this constant feeds would
# be inventing a fractional option this task's own brief explicitly warns
# against, not a rounding shortcut.
LUCIDE_STROKE_WIDTH = 1.5
LUCIDE_WIDTH_TARGET = f'stroke-width="{LUCIDE_STROKE_WIDTH:g}"'

# IconId.rust_name -> Lucide slug, one entry per icon this bake actually
# ships a Lucide asset for. Every `MANIFEST` entry's `rust_name` must
# appear in EXACTLY ONE of this dict or `LUCIDE_GAPS` below (checked by
# `main` at startup) -- there is no third, silently-uncovered case.
# Picked by MEANING, not by nearest slug spelling (verified against this
# crate's own actual button semantics and, where a codicon glyph's own
# shape was ambiguous from its slug alone, the cached SVG path data --
# see this task's own report for the per-icon reasoning); duplicate
# targets ARE allowed where two `IconId`s genuinely share one concept
# (`SourceControl`/`GitBranch` -> `git-branch`, both a branch topology;
# `NewFile`/`DiffAdded` -> `file-plus`, both "content added to a file")
# -- that is a documented reuse, not a collision, and never crosses into a
# WRONG meaning (see `LUCIDE_GAPS` below for the two cases where no
# existing Lucide glyph clears that bar at all).
LUCIDE_SLUGS: dict[str, str] = {
    "Files": "files",
    "SourceControl": "git-branch",
    "Person": "user",
    "Project": "kanban",
    "SettingsGear": "settings",
    "ChevronLeft": "chevron-left",
    "ChevronRight": "chevron-right",
    "ChevronDown": "chevron-down",
    "NewFile": "file-plus",
    "NewFolder": "folder-plus",
    "Folder": "folder",
    "FolderOpened": "folder-open",
    "File": "file",
    "Save": "save",
    "Refresh": "refresh-cw",
    "Add": "plus",
    "Trash": "trash-2",
    "Search": "search",
    "Check": "check",
    "Close": "x",
    "ArrowUp": "arrow-up",
    "ArrowDown": "arrow-down",
    "ArrowLeft": "arrow-left",
    "ArrowRight": "arrow-right",
    "ArrowSwap": "arrow-left-right",
    "GitCommit": "git-commit-horizontal",
    "GitBranch": "git-branch",
    "Diff": "file-diff",
    "DiffAdded": "file-plus",
    "GitCompare": "git-compare",
    "Repo": "book-marked",
    "RepoForked": "git-fork",
    "DebugStop": "square",
    "DebugRestart": "rotate-ccw",
    "Edit": "pencil",
    "History": "clock-fading",
    "Terminal": "terminal",
    "Output": "file-text",
    "CloudDownload": "cloud-download",
    "Ellipsis": "ellipsis",
    "Link": "link",
    "CircleSlash": "circle-slash",
    "Warning": "triangle-alert",
    "Error": "octagon-alert",
    "Info": "info",
    "Play": "play",
    "Sync": "refresh-ccw",
    "GoToFile": "file-symlink",
    "Pulse": "activity",
    "Checklist": "list-checks",
    "Eye": "eye",
    "Layout": "layout-grid",
    "SplitHorizontal": "square-split-horizontal",
    "SplitVertical": "square-split-vertical",
    "Preview": "monitor",
}

# The two `MANIFEST` icons this tool deliberately does NOT bake a Lucide
# asset for -- reported, never approximated (this task's own brief: "a
# wrong-meaning icon is worse than a reported gap"). Value is the reason,
# printed verbatim in this tool's own report.
LUCIDE_GAPS: dict[str, str] = {
    "CircleFilled": (
        "Lucide ships no solid-fill glyph at all (a STYLE gap, not a "
        "naming one): every Lucide icon is stroke-only by design (see "
        "this crate's own IconFamily doc comment on codicons' filled "
        "outlines vs Lucide's stroke geometry), so there is no glyph "
        "whose FILLED-dot meaning (an 'unsaved/has content' marker) "
        "survives -- Lucide's own plain 'circle' is an outline, which "
        "reads as the OPPOSITE state in most editor conventions, i.e. "
        "exactly the wrong-meaning substitution this task's own brief "
        "warns against."
    ),
    "RunAll": (
        "no Lucide glyph distinctly means 'run every item', only 'run "
        "one' (Lucide's own 'play'). Substituting 'play' would collide "
        "with IconId::Play's own meaning (both read identically under "
        "Lucide); 'fast-forward' (two triangles) was considered and "
        "rejected -- it means skip/speed, a different action, not 'all'."
    ),
}


@dataclass(frozen=True)
class IconSpec:
    rust_name: str  # PascalCase IconId variant
    slug: str  # codicon file stem, e.g. "source-control"
    ascii: str  # 1-2 char ASCII label
    file_stem: str = field(default="")  # snake_case asset stem; derived if empty
    # cause 7 (this module's own header doc comment): overrides `slug` for
    # the STRIP tier's own bake only, when that tier needs a different
    # source codicon than the rail/compact/gallery tiers (e.g. a badge-
    # free glyph at 16px where the badge version reads as clutter).
    # `""` (the default) means "use `slug`, same as every other tier".
    strip_slug: str = field(default="")

    def stem(self) -> str:
        return self.file_stem or self.slug.replace("-", "_")

    def strip_source_slug(self) -> str:
        return self.strip_slug or self.slug


MANIFEST: list[IconSpec] = [
    # -- Activity rail / primary nav (existing 7 + 1 new) --------------
    IconSpec("Files", "files", "F"),
    IconSpec("SourceControl", "source-control", "G"),
    IconSpec("Person", "person", "A"),
    IconSpec("Project", "project", "K"),
    IconSpec("SettingsGear", "settings-gear", "S"),
    IconSpec("ChevronLeft", "chevron-left", "<"),
    IconSpec("ChevronRight", "chevron-right", ">"),
    IconSpec("ChevronDown", "chevron-down", "v"),
    # -- File ops --------------------------------------------------------
    # NewFile/NewFolder: cause 7 (this module's own header doc comment) --
    # the badge codicons read as clutter at the strip tier's own 16px
    # lattice render, so that tier alone bakes from the plain `file`/
    # `folder` glyphs this manifest already ships below.
    IconSpec("NewFile", "new-file", "N+", strip_slug="file"),
    IconSpec("NewFolder", "new-folder", "Nd", strip_slug="folder"),
    IconSpec("Folder", "folder", "Fd"),
    IconSpec("FolderOpened", "folder-opened", "Fo"),
    IconSpec("File", "file", "Fl"),
    IconSpec("Save", "save", "Sa"),
    IconSpec("Refresh", "refresh", "R"),
    IconSpec("Add", "add", "+"),
    IconSpec("Trash", "trash", "Tr"),
    IconSpec("Search", "search", "Se"),
    IconSpec("Check", "check", "OK"),
    IconSpec("Close", "close", "x"),
    # -- Arrows ------------------------------------------------------------
    IconSpec("ArrowUp", "arrow-up", "^"),
    IconSpec("ArrowDown", "arrow-down", "v"),
    IconSpec("ArrowLeft", "arrow-left", "<"),
    IconSpec("ArrowRight", "arrow-right", ">"),
    IconSpec("ArrowSwap", "arrow-swap", "<>"),
    # -- Git / diff / repo -------------------------------------------------
    IconSpec("GitCommit", "git-commit", "Gc"),
    IconSpec("GitBranch", "git-branch", "Gb"),
    IconSpec("Diff", "diff", "Df"),
    IconSpec("DiffAdded", "diff-added", "D+"),
    IconSpec("GitCompare", "git-compare", "Gx"),
    IconSpec("Repo", "repo", "Rp"),
    IconSpec("RepoForked", "repo-forked", "Rf"),
    # -- Dev tools -----------------------------------------------------------
    IconSpec("DebugStop", "debug-stop", "Ds"),
    IconSpec("DebugRestart", "debug-restart", "Dr"),
    IconSpec("Edit", "edit", "E"),
    IconSpec("History", "history", "H"),
    IconSpec("Terminal", "terminal", "T"),
    IconSpec("Output", "output", "O"),
    IconSpec("CloudDownload", "cloud-download", "Cd"),
    IconSpec("Ellipsis", "ellipsis", ".."),
    IconSpec("Link", "link", "Lk"),
    # -- Status indicators -----------------------------------------------
    IconSpec("CircleFilled", "circle-filled", "Cf"),
    IconSpec("CircleSlash", "circle-slash", "Cs"),
    IconSpec("Warning", "warning", "!"),
    IconSpec("Error", "error", "Er"),
    IconSpec("Info", "info", "i"),
    # -- Actions / misc ----------------------------------------------------
    IconSpec("RunAll", "run-all", "R>"),
    IconSpec("Play", "play", "Pl"),
    IconSpec("Sync", "sync", "Sy"),
    IconSpec("GoToFile", "go-to-file", "Gf"),
    IconSpec("Pulse", "pulse", "Pu"),
    IconSpec("Checklist", "checklist", "Cl"),
    IconSpec("Eye", "eye", "Ey"),
    # -- Layout --------------------------------------------------------------
    IconSpec("Layout", "layout", "Ly"),
    IconSpec("SplitHorizontal", "split-horizontal", "Sh"),
    IconSpec("SplitVertical", "split-vertical", "Sv"),
    IconSpec("Preview", "preview", "Pv"),
]

# Report grouping -- mirrors the task's own original line-grouping of the
# icon list verbatim, purely for the printed report's readability.
REPORT_GROUPS: list[tuple[str, list[str]]] = [
    ("Activity rail / primary nav", ["files", "source-control", "person", "project", "settings-gear", "chevron-left", "chevron-right", "chevron-down"]),
    ("File ops", ["new-file", "new-folder", "folder", "folder-opened", "file", "save", "refresh", "add", "trash", "search", "check", "close"]),
    ("Arrows", ["arrow-up", "arrow-down", "arrow-left", "arrow-right", "arrow-swap"]),
    ("Git / diff / repo", ["git-commit", "git-branch", "diff", "diff-added", "git-compare", "repo", "repo-forked"]),
    ("Dev tools", ["debug-stop", "debug-restart", "edit", "history", "terminal", "output", "cloud-download", "ellipsis", "link"]),
    ("Status indicators", ["circle-filled", "circle-slash", "warning", "error", "info"]),
    ("Actions / misc", ["run-all", "play", "sync", "go-to-file", "pulse", "checklist", "eye"]),
    ("Layout", ["layout", "split-horizontal", "split-vertical", "preview"]),
]


def die(message: str) -> "None":
    print(f"bake_icons: ERROR: {message}", file=sys.stderr)
    sys.exit(1)


def run_tool(cmd: list[str]) -> None:
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        die(f"command failed: {' '.join(cmd)}\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}")


def fetch_svg(slug: str, cache_dir: Path) -> Path:
    """Return the cached source SVG for `slug`, downloading it first if
    absent. A 404 aborts the whole run immediately, naming the exact slug
    and URL -- this is the "FAIL LOUDLY, never silently skip" contract for
    an icon name that does not exist in the upstream codicon set."""
    cache_dir.mkdir(parents=True, exist_ok=True)
    dest = cache_dir / f"{slug}.svg"
    if dest.exists():
        return dest
    url = CODICON_URL_TEMPLATE.format(slug=slug)
    try:
        with urllib.request.urlopen(url, timeout=20) as response:
            data = response.read()
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            die(
                f"codicon '{slug}' does NOT exist upstream (404 at {url}). "
                f"Per this tool's own policy: substitute the closest existing "
                f"codicon in MANIFEST above and record the substitution in the "
                f"task report -- do not invent an asset, do not skip this icon."
            )
        die(f"fetching '{slug}' failed: HTTP {exc.code} at {url}")
    except urllib.error.URLError as exc:
        die(f"fetching '{slug}' failed: {exc.reason} at {url}")
    dest.write_bytes(data)
    return dest


def patch_fill(svg_path: Path, slug: str, cache_dir: Path) -> Path:
    """Patch `fill="currentColor"` -> `fill="#cdd6f4"` (this crate's
    `pty_palette::GATE_FG`), caching the patched copy alongside the
    source. Every codicon in this manifest carries exactly one
    `fill="currentColor"`, on the root `<svg>` element, inherited by every
    child `<path>` (verified against all 57 sources at authoring time --
    see this module's own doc comment); a source that does not match that
    shape is a real structural surprise, not something to patch partially
    and hope for the best, so a non-1 replacement count fails loudly."""
    dest = cache_dir / f"{slug}.patched.svg"
    if dest.exists():
        return dest
    text = svg_path.read_text(encoding="utf-8")
    count = text.count(FILL_SOURCE)
    if count != 1:
        die(
            f"codicon '{slug}' has {count} occurrences of {FILL_SOURCE!r} "
            f"(expected exactly 1, on the root <svg> element) -- this icon's "
            f"source structure doesn't match every other codicon in this set; "
            f"inspect {svg_path} by hand before baking it."
        )
    patched = text.replace(FILL_SOURCE, FILL_TARGET)
    dest.write_text(patched, encoding="utf-8")
    return dest


def fetch_lucide_svg(slug: str, cache_dir: Path) -> Path:
    """Lucide's own `fetch_svg`: same cache-once/reuse-forever contract,
    same "404 aborts the ENTIRE run, names the slug and URL, never
    silently skipped" policy -- see `fetch_svg`'s own doc comment, which
    this mirrors against `LUCIDE_URL_TEMPLATE` instead of `CODICON_URL_
    TEMPLATE`. A 404 here means a `LUCIDE_SLUGS` entry above was typed
    wrong (every slug in that dict was verified to exist upstream at
    authoring time), never a real "closest existing icon" substitution
    call -- that judgment already happened once, by hand, choosing
    `LUCIDE_SLUGS`'s own values (or `LUCIDE_GAPS`, for the two icons with
    no Lucide counterpart at all); this function's job is only to fetch
    the slug it is handed."""
    cache_dir.mkdir(parents=True, exist_ok=True)
    dest = cache_dir / f"{slug}.svg"
    if dest.exists():
        return dest
    url = LUCIDE_URL_TEMPLATE.format(slug=slug)
    try:
        with urllib.request.urlopen(url, timeout=20) as response:
            data = response.read()
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            die(
                f"lucide icon '{slug}' does NOT exist upstream (404 at {url}). "
                f"This means LUCIDE_SLUGS above names a slug that isn't real -- "
                f"fix the mapping, do not invent an asset, do not skip this icon."
            )
        die(f"fetching '{slug}' failed: HTTP {exc.code} at {url}")
    except urllib.error.URLError as exc:
        die(f"fetching '{slug}' failed: {exc.reason} at {url}")
    dest.write_bytes(data)
    return dest


def patch_lucide(svg_path: Path, slug: str, cache_dir: Path) -> Path:
    """Patch a Lucide source SVG for baking: `stroke="currentColor"` ->
    `stroke="#cdd6f4"` (same ink colour `patch_fill` gives codicons -- see
    that function's own doc comment) AND `stroke-width="2"` ->
    `LUCIDE_WIDTH_TARGET` (`LUCIDE_STROKE_WIDTH`, see that constant's own
    doc comment for why this exact number). `fill="none"` is left
    untouched -- Lucide glyphs carry their ink in the STROKE, not the
    fill (see `IconFamily`'s own doc comment on why that is the whole
    point of shipping this family alongside codicons at all), so there is
    no fill to retarget the way `patch_fill` retargets codicons' `fill=
    "currentColor"`. Same "exactly 1 occurrence on the root <svg>, a non-1
    count is a structural surprise to fail loudly on, not patch partially"
    discipline as `patch_fill` -- verified against every `LUCIDE_SLUGS`
    source at authoring time."""
    dest = cache_dir / f"{slug}.patched.svg"
    if dest.exists():
        return dest
    text = svg_path.read_text(encoding="utf-8")
    stroke_count = text.count(LUCIDE_STROKE_SOURCE)
    if stroke_count != 1:
        die(
            f"lucide icon '{slug}' has {stroke_count} occurrences of {LUCIDE_STROKE_SOURCE!r} "
            f"(expected exactly 1, on the root <svg> element) -- inspect {svg_path} by hand."
        )
    width_count = text.count(LUCIDE_WIDTH_SOURCE)
    if width_count != 1:
        die(
            f"lucide icon '{slug}' has {width_count} occurrences of {LUCIDE_WIDTH_SOURCE!r} "
            f"(expected exactly 1, on the root <svg> element) -- inspect {svg_path} by hand."
        )
    patched = text.replace(LUCIDE_STROKE_SOURCE, LUCIDE_STROKE_TARGET).replace(LUCIDE_WIDTH_SOURCE, LUCIDE_WIDTH_TARGET)
    dest.write_text(patched, encoding="utf-8")
    return dest


def srgb_to_linear(channel: int) -> float:
    """Piecewise sRGB EOTF (IEC 61966-2-1), one 8-bit channel value ->
    linear light in [0, 1]. NOT a 2.2-power approximation -- the exact
    two-segment curve, so `composite_over_background`'s own linear-space
    blend round-trips EXACTLY back to the original byte at full/zero
    coverage (verified by this crate's own `icons.rs::tests::gate_
    compositing_matches_the_background_and_ink_colours_exactly_at_full_
    coverage`, which compares composited output against source bytes for
    bit-exact equality at those two extremes)."""
    c = channel / 255.0
    if c <= 0.04045:
        return c / 12.92
    return ((c + 0.055) / 1.055) ** 2.4


def linear_to_srgb(value: float) -> int:
    """Inverse of `srgb_to_linear` -- linear light in [0, 1] -> an 8-bit
    sRGB channel byte, rounded to the nearest integer (clamped: floating-
    point round-trip error could in principle push a value a hair outside
    [0, 255])."""
    if value <= 0.0031308:
        srgb = value * 12.92
    else:
        srgb = 1.055 * (value ** (1.0 / 2.4)) - 0.055
    return max(0, min(255, round(srgb * 255.0)))


# Every ink byte this crate ever bakes is looked up through this table
# rather than recomputing `srgb_to_linear` per pixel per channel -- the
# hot loop in `composite_over_background` below.
_SRGB_TO_LINEAR_LUT: list[float] = [srgb_to_linear(v) for v in range(256)]


def rasterize_sixel(patched_svg: Path) -> bytes:
    """Rail tier: `rasterize_lattice_fit` at `SIXEL_PX_W`x`_H` (40x38) --
    see that function's own doc comment (and cause 6 in this module's own
    header doc comment) for why the glyph itself renders at 32x32 (the
    largest whole multiple of 16 that fits 40x38), not 40x38 directly.
    Returns the TRUE-coverage raw RGBA8 bytes -- NOT written to `ICONS_
    DIR` directly, see `ensure_assets`'s own doc comment for why.
    """
    data = rasterize_lattice_fit(patched_svg, SIXEL_PX_W, SIXEL_PX_H)
    if len(data) != SIXEL_RGBA_LEN:
        die(f"sixel raster for {patched_svg} produced {len(data)} bytes, expected {SIXEL_RGBA_LEN}")
    return data


def rasterize_compact_sixel(patched_svg: Path) -> bytes:
    """Compact sixel tier: fit-within a `COMPACT_SIXEL_PX_W`-wide box (the
    smaller of the two target dimensions constrains a square source, same
    fit-within behaviour `rasterize_sixel` documents for the rail tier,
    left to resvg's own `-w`/`-h` here rather than hand-computed -- this
    tier's own absolute size is small enough, and its own required cell
    footprint fixed enough, that the rounding edge case `svg_intrinsic_
    size`'s own doc comment describes has not been observed for it), then
    pad onto the full `COMPACT_SIXEL_PX_W` x `_H` canvas -- exactly one
    assumed terminal cell. For dense single-row buttons (Explorer/Git
    sidebar lists and their modals) where the rail's own icon does not
    fit. Returns the TRUE-coverage raw RGBA8 bytes -- see `rasterize_
    sixel`'s own doc comment for why this is not written to disk here.

    Equivalent hand-run commands:
        resvg -w 10 -h 19 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=10:19:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>_compact.rgba
    """
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        raw_rgba = Path(tmp) / "raw.rgba"
        run_tool(["resvg", "-w", str(COMPACT_SIXEL_PX_W), "-h", str(COMPACT_SIXEL_PX_H), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={COMPACT_SIXEL_PX_W}:{COMPACT_SIXEL_PX_H}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(raw_rgba),
        ])
        data = raw_rgba.read_bytes()
    if len(data) != COMPACT_SIXEL_RGBA_LEN:
        die(f"compact sixel raster for {patched_svg} produced {len(data)} bytes, expected {COMPACT_SIXEL_RGBA_LEN}")
    return data


def svg_intrinsic_size(svg_path: Path) -> tuple[int, int]:
    """Read the root `<svg>`'s own `width="..."` / `height="..."` (both
    always present, always an integer pixel-equivalent unit count, on
    every codicon source in this manifest -- verified against all 57 at
    authoring time, same verification precedent as `patch_fill`'s own
    `fill="currentColor"` count check). Used to precompute exact
    fit-within target dimensions ourselves rather than relying on
    resvg's own dual `-w`/`-h` rounding, which was found (at authoring
    time, baking the strip tier below -- see this module's own header
    doc comment) to occasionally round a non-square source's OTHER axis
    slightly PAST a small requested box: `output.svg`'s 24x25 (the one
    documented non-square exception) rasterized to 20x21 -- one pixel
    too tall -- when asked to fit within 20x20 directly, even though the
    correct floor-rounded fit is 19x20."""
    text = svg_path.read_text(encoding="utf-8")
    width_match = re.search(r'\bwidth="(\d+)"', text)
    height_match = re.search(r'\bheight="(\d+)"', text)
    if not width_match or not height_match:
        die(f"{svg_path} has no numeric width=/height= on its root <svg> -- cannot compute a fit-within size")
    return int(width_match.group(1)), int(height_match.group(1))


def fit_within(src_w: int, src_h: int, box_w: int, box_h: int) -> tuple[int, int]:
    """Standard "fit within a `box_w`x`box_h` box, preserve aspect, never
    exceed either bound" -- floor-rounded so the result can never round UP
    past the box (see `svg_intrinsic_size`'s own doc comment for why this
    is computed by hand rather than left to resvg's own `-w`/`-h`
    rounding)."""
    scale = min(box_w / src_w, box_h / src_h)
    return max(1, int(src_w * scale)), max(1, int(src_h * scale))


def glyph_lattice_size(canvas_w: int, canvas_h: int) -> int:
    """Largest whole multiple of `LATTICE_STEP_PX` that fits inside BOTH
    canvas dimensions -- the render size at which every codicon lattice
    coordinate (a whole multiple of 1/16th its own viewBox) lands on a
    whole device pixel, so a straight stroke gets hard, fully-opaque
    edges instead of the smeared, sub-pixel band cause 6 (this module's
    own header doc comment) measures on the pre-fix assets. Returns 0
    when the canvas itself is smaller than one lattice step in its own
    shorter dimension -- the compact tier's own 10x19 canvas, where no
    multiple of 16 fits at all; `rasterize_compact_sixel` never calls
    this and keeps its own pre-existing single-pass-at-canvas-size
    treatment, per this fix's own explicit scope."""
    return (min(canvas_w, canvas_h) // LATTICE_STEP_PX) * LATTICE_STEP_PX


def rasterize_lattice_fit(patched_svg: Path, canvas_w: int, canvas_h: int) -> bytes:
    """Shared recipe for the rail/strip/gallery tiers (cause 6, this
    module's own header doc comment): render the glyph at `glyph_lattice_
    size(canvas_w, canvas_h)` -- 16/32/48px for the strip/rail/gallery
    canvases respectively -- fit-within that square preserving the
    source's own aspect ratio (`fit_within`, same non-square-safe
    computation every other tier already used before this fix), THEN pad
    onto the tier's own full canvas at an offset THIS FUNCTION computes
    itself as a plain integer (`//`) and hands to ffmpeg as a literal,
    rather than ffmpeg's own symbolic `(ow-iw)/2` expression -- the
    previous recipe every rasterize function here used, and still
    correct for tiers this fix does not touch, but this fix's own
    correctness depends on the offset being EXACTLY an integer number of
    pixels (see this function's own cause-6 doc comment: a half-pixel
    offset would silently destroy the lattice alignment the larger glyph
    render size just bought), which a symbolic expression evaluated
    somewhere inside ffmpeg is not a documented guarantee of. For the 56
    of 57 sources with a square viewBox, `fit_within` returns exactly
    `glyph_lattice_size` on both axes and every lattice coordinate lands
    on a whole pixel; for the one non-square exception (`output.svg`,
    24x25 -- cause 2 above), fitting within a SQUARE glyph box scales
    both axes by the SAME factor (dictated by the taller dimension), so
    its own width axis ends up slightly short of the lattice-exact
    scale -- aspect is preserved (never distorted to force alignment),
    at the cost of a softer width-axis stroke on that one icon only.
    Returns the TRUE-coverage raw RGBA8 bytes at `canvas_w`x`canvas_h`,
    caller-length-checked -- see `rasterize_sixel`'s own doc comment for
    why this is not written to `ICONS_DIR` directly."""
    glyph = glyph_lattice_size(canvas_w, canvas_h)
    fit_w, fit_h = fit_within(*svg_intrinsic_size(patched_svg), glyph, glyph)
    pad_x = (canvas_w - fit_w) // 2
    pad_y = (canvas_h - fit_h) // 2
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        raw_rgba = Path(tmp) / "raw.rgba"
        run_tool(["resvg", "-w", str(fit_w), "-h", str(fit_h), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={canvas_w}:{canvas_h}:{pad_x}:{pad_y}:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(raw_rgba),
        ])
        return raw_rgba.read_bytes()


def rasterize_strip_sixel(patched_svg: Path) -> bytes:
    """Control-plane strip tier: `rasterize_lattice_fit` at `STRIP_SIXEL_
    PX_W`x`_H` (20x19) -- see that function's own doc comment (and cause
    6 in this module's own header doc comment) for why the glyph itself
    renders at 16x16, not 20x19 directly. Returns the TRUE-coverage raw
    RGBA8 bytes -- see `rasterize_sixel`'s own doc comment for why this
    is not written to disk here.
    """
    data = rasterize_lattice_fit(patched_svg, STRIP_SIXEL_PX_W, STRIP_SIXEL_PX_H)
    if len(data) != STRIP_SIXEL_RGBA_LEN:
        die(f"strip sixel raster for {patched_svg} produced {len(data)} bytes, expected {STRIP_SIXEL_RGBA_LEN}")
    return data


def rasterize_gallery_sixel(patched_svg: Path) -> bytes:
    """Gallery tier: `rasterize_lattice_fit` at `GALLERY_SIXEL_PX_W`x`_H`
    (60x57) -- see that function's own doc comment (and cause 6 in this
    module's own header doc comment) for why the glyph itself renders at
    48x48, not 60x57 directly. Returns the TRUE-coverage raw RGBA8 bytes
    -- see `rasterize_sixel`'s own doc comment for why this is not
    written to disk here.
    """
    data = rasterize_lattice_fit(patched_svg, GALLERY_SIXEL_PX_W, GALLERY_SIXEL_PX_H)
    if len(data) != GALLERY_SIXEL_RGBA_LEN:
        die(f"gallery sixel raster for {patched_svg} produced {len(data)} bytes, expected {GALLERY_SIXEL_RGBA_LEN}")
    return data


def composite_over_background(rgba: bytes, bg: tuple[int, int, int]) -> bytes:
    """This function is BOTH cause 1's own fix (composite over the
    button's exact background instead of leaving the pixel transparent,
    so `icy_sixel`'s hard alpha threshold and Windows Terminal's own lack
    of sixel transparency support both become irrelevant -- see this
    module's own header doc comment) AND cause 3's own fix (the actual
    arithmetic below is gamma-correct, not a naive byte-domain blend --
    same doc comment, cause 3): alpha-composite a straight (non-
    premultiplied) RGBA buffer -- resvg's own convention, where a
    partially-covered "ink" pixel's RGB channels stay at the flat fill
    colour regardless of alpha, verified in `../icons.rs`'s own module
    doc -- over a flat, fully opaque `bg` colour, IN LINEAR LIGHT: per
    pixel, convert both `ink` and `bg` to linear (`srgb_to_linear`), blend
    by the pixel's own TRUE coverage `a/255`, convert back (`linear_to_
    srgb`). `out_alpha = 255` throughout. Pure arithmetic over an
    ALREADY-rasterized buffer -- same width/height in and out, no
    interpolation -- so this can never become a second resampling pass
    (see this module's own header doc comment on why that distinction
    matters for cause 2). The icon's own ink colour is already `#cdd6f4`
    from `patch_fill` above, so no separate "tint to the theme
    foreground" step is needed here -- the source is already the right
    colour, this only decides what shows through where it is not fully
    opaque."""
    bg_linear = tuple(srgb_to_linear(channel) for channel in bg)
    out = bytearray(len(rgba))
    for i in range(0, len(rgba), 4):
        coverage = rgba[i + 3] / 255.0
        inv = 1.0 - coverage
        for channel in range(3):
            ink_linear = _SRGB_TO_LINEAR_LUT[rgba[i + channel]]
            blended = ink_linear * coverage + bg_linear[channel] * inv
            out[i + channel] = linear_to_srgb(blended)
        out[i + 3] = 255
    return bytes(out)


def ensure_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path, Path]:
    """Ensure every rail-tier `.rgba` output for `spec` exists on disk,
    baking whatever is missing (or everything, if `force`). Returns
    (raw_source, gate_active, gate_accent) paths. This is the idempotency
    boundary: a normal re-run with nothing new to bake touches no
    network and spawns no subprocess at all.

    `raw_source` (`<stem>.rgba`) is NOT a shipped `SixelVariant` any more
    (cause 4's own retirement note, this module's own header doc comment):
    `GateActive`/`GateAccent` are the ONLY rail-tier sixel this crate ships
    now, since every icon-bearing button paints an explicit truecolor
    background unconditionally. This file stays on disk and gets rebaked
    like any other output purely so `icons.rs`'s own `#[cfg(test)]`
    compositing-correctness tests (`gate_compositing_matches_the_
    background_and_ink_colours_exactly_at_full_coverage` and friends) can
    verify `composite_over_background` against a REAL checked-in true-
    coverage buffer rather than trusting the arithmetic by construction --
    Rust never reaches for it outside `#[cfg(test)]`.

    ORDERING (load-bearing for cause 3's own correctness -- see this
    module's own header doc comment): whenever ANY of the three outputs
    needs rebuilding, this rasterizes ONE fresh TRUE-coverage buffer in
    memory (`raw`) and derives EVERYTHING from that SAME buffer via
    `composite_over_background` (exact background, exact linear blend),
    writing `raw` itself to `raw_source`'s own path unchanged. Re-deriving
    the `_gate_*` variants from anything OTHER than a just-rasterized
    `raw` -- e.g. reading `raw_source` back off disk -- would be
    pointless indirection for the exact same bytes, so this never reads
    it back off disk to feed compositing, even on an idempotent re-run
    that only needs to rebuild the gate variants (the small correctness
    cost: such a re-run re-rasterizes even though `raw_source` itself did
    not need it, trading a little idempotency for one less code path)."""
    sixel_path = ICONS_DIR / f"{spec.stem()}.rgba"
    active_path = ICONS_DIR / f"{spec.stem()}_gate_active.rgba"
    accent_path = ICONS_DIR / f"{spec.stem()}_gate_accent.rgba"

    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != SIXEL_RGBA_LEN
    need_active = force or not active_path.exists() or active_path.stat().st_size != SIXEL_RGBA_LEN
    need_accent = force or not accent_path.exists() or accent_path.stat().st_size != SIXEL_RGBA_LEN

    if need_sixel or need_active or need_accent:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_sixel(patched)
        if need_active:
            active_path.write_bytes(composite_over_background(raw, GATE_ACTIVE_BG_RGB))
        if need_accent:
            accent_path.write_bytes(composite_over_background(raw, GATE_ACCENT_BG_RGB))
        if need_sixel:
            sixel_path.write_bytes(raw)

    return sixel_path, active_path, accent_path


def ensure_strip_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Ensure the control-plane strip's own two sixel-tier outputs:
    `<stem>_strip.rgba` (the raw true-coverage source, test-only -- see
    `ensure_assets`'s own doc comment for why this stays on disk) and
    `<stem>_strip_gate.rgba` (the SAME raw pixels alpha-composited over
    `GATE_ACTIVE_BG_RGB` in linear light -- see `composite_over_
    background`; the ONLY strip-tier sixel this crate ships). Same "derive
    both from one freshly-rasterized `raw` buffer" precedent as `ensure_
    assets` above. The strip never shows a `selected` state (see `render::
    render_control_strip_button`'s own doc comment), so there is no
    `_strip_gate_accent` variant.

    Sources from `spec.strip_source_slug()`, NOT `spec.slug` -- cause 7
    (this module's own header doc comment): `NewFile`/`NewFolder` bake
    their own strip tier from a different, badge-free codicon than their
    rail/compact/gallery tiers; every other icon's `strip_source_slug()`
    is just `slug` unchanged."""
    transparent_path = ICONS_DIR / f"{spec.stem()}_strip.rgba"
    gate_path = ICONS_DIR / f"{spec.stem()}_strip_gate.rgba"
    need_transparent = force or not transparent_path.exists() or transparent_path.stat().st_size != STRIP_SIXEL_RGBA_LEN
    need_gate = force or not gate_path.exists() or gate_path.stat().st_size != STRIP_SIXEL_RGBA_LEN

    if need_transparent or need_gate:
        slug = spec.strip_source_slug()
        svg = fetch_svg(slug, cache_dir)
        patched = patch_fill(svg, slug, cache_dir)
        raw = rasterize_strip_sixel(patched)
        if need_gate:
            gate_path.write_bytes(composite_over_background(raw, GATE_ACTIVE_BG_RGB))
        if need_transparent:
            transparent_path.write_bytes(raw)

    return transparent_path, gate_path


def ensure_gallery_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Ensure the icon gallery's own two dedicated sixel-tier outputs:
    `<stem>_gallery.rgba` (the raw true-coverage source, test-only -- see
    `ensure_assets`'s own doc comment) and `<stem>_gallery_gate.rgba` (the
    same raw pixels composited over `GATE_ACTIVE_BG_RGB` in linear light;
    the ONLY gallery-tier sixel this crate ships). Same ordering precedent
    as `ensure_assets`/`ensure_strip_assets` above. Same "no selected
    state, so no `_gallery_gate_accent` variant" precedent as `ensure_
    strip_assets` -- the gallery is a read-only comparison grid."""
    transparent_path = ICONS_DIR / f"{spec.stem()}_gallery.rgba"
    gate_path = ICONS_DIR / f"{spec.stem()}_gallery_gate.rgba"
    need_transparent = force or not transparent_path.exists() or transparent_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN
    need_gate = force or not gate_path.exists() or gate_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN

    if need_transparent or need_gate:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_gallery_sixel(patched)
        if need_gate:
            gate_path.write_bytes(composite_over_background(raw, GATE_ACTIVE_BG_RGB))
        if need_transparent:
            transparent_path.write_bytes(raw)

    return transparent_path, gate_path


def ensure_compact_assets(spec: IconSpec, cache_dir: Path, force: bool) -> Path:
    """Ensure the compact tier's own single sixel-tier output (`<stem>_
    compact.rgba`, real straight-alpha true coverage). This tier is
    `Transparent`-only -- out of scope for cause 1/3's own background-
    compositing fix (see `render::render_compact_icon_button`'s own doc
    comment: dense panel/modal content with 3+ distinct backgrounds,
    unlike the rail's 2 and the strip's 1) -- so there is no gate variant
    to derive here at all, and this tier's own `.rgba` IS the real,
    directly-shipped asset (`icons::build_sixel_compact` still encodes it
    with `BackgroundMode::Transparent`), not a test-only source the way
    `ensure_assets`/`ensure_strip_assets`/`ensure_gallery_assets`'s own
    raw outputs now are."""
    sixel_path = ICONS_DIR / f"{spec.stem()}_compact.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != COMPACT_SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_compact_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


# ---- Lucide bakes -- reuse EVERY rasterize_*/composite_over_background
# function above completely unchanged (cause 6's own lattice-fit fix
# included): those functions only ever take "a patched SVG path" + "a
# target canvas size" + (for compositing) "a background colour", never
# anything codicon-specific, so the SAME 16px/32px/48px lattice-fit
# machinery this module's own header doc comment describes for codicons
# applies to Lucide's own 24-unit viewBox identically -- only the FETCH
# (`fetch_lucide_svg` vs `fetch_svg`) and PATCH (`patch_lucide` vs `patch_
# fill`) steps differ, plus the `lucide_` filename prefix so neither
# family's own `.rgba` outputs can collide on disk. Mirrors `ensure_
# assets`/`ensure_strip_assets`/`ensure_gallery_assets`/`ensure_compact_
# assets` one-for-one -- same ordering precedent (one fresh raw rasterize
# feeds every derived variant), same "raw source stays on disk test-only,
# gate variants are the only shipped asset" split, same compact-tier
# exception (real transparency, no gate variant, no lattice-fit -- the
# compact canvas is 10x19, too small to host even one 16px lattice step
# in its own narrower dimension, same reasoning `glyph_lattice_size`'s own
# doc comment gives for why codicons' compact tier is unchanged by cause
# 6 either).


def ensure_lucide_assets(spec: IconSpec, slug: str, cache_dir: Path, force: bool) -> tuple[Path, Path, Path]:
    """Lucide's own `ensure_assets` -- see that function's own doc
    comment; `slug` is `LUCIDE_SLUGS[spec.rust_name]` (the Lucide slug),
    NOT `spec.slug` (the codicon slug `spec` was authored around)."""
    sixel_path = ICONS_DIR / f"lucide_{spec.stem()}.rgba"
    active_path = ICONS_DIR / f"lucide_{spec.stem()}_gate_active.rgba"
    accent_path = ICONS_DIR / f"lucide_{spec.stem()}_gate_accent.rgba"

    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != SIXEL_RGBA_LEN
    need_active = force or not active_path.exists() or active_path.stat().st_size != SIXEL_RGBA_LEN
    need_accent = force or not accent_path.exists() or accent_path.stat().st_size != SIXEL_RGBA_LEN

    if need_sixel or need_active or need_accent:
        svg = fetch_lucide_svg(slug, cache_dir)
        patched = patch_lucide(svg, slug, cache_dir)
        raw = rasterize_sixel(patched)
        if need_active:
            active_path.write_bytes(composite_over_background(raw, GATE_ACTIVE_BG_RGB))
        if need_accent:
            accent_path.write_bytes(composite_over_background(raw, GATE_ACCENT_BG_RGB))
        if need_sixel:
            sixel_path.write_bytes(raw)

    return sixel_path, active_path, accent_path


def ensure_lucide_strip_assets(spec: IconSpec, slug: str, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Lucide's own `ensure_strip_assets` -- see that function's own doc
    comment. Always sources from `slug` directly (Lucide has no
    equivalent of cause 7's own codicon-only `strip_slug` badge-swap --
    every `LUCIDE_SLUGS` glyph is a single coherent shape at this tier's
    own 16px lattice render, verified at authoring time)."""
    transparent_path = ICONS_DIR / f"lucide_{spec.stem()}_strip.rgba"
    gate_path = ICONS_DIR / f"lucide_{spec.stem()}_strip_gate.rgba"
    need_transparent = force or not transparent_path.exists() or transparent_path.stat().st_size != STRIP_SIXEL_RGBA_LEN
    need_gate = force or not gate_path.exists() or gate_path.stat().st_size != STRIP_SIXEL_RGBA_LEN

    if need_transparent or need_gate:
        svg = fetch_lucide_svg(slug, cache_dir)
        patched = patch_lucide(svg, slug, cache_dir)
        raw = rasterize_strip_sixel(patched)
        if need_gate:
            gate_path.write_bytes(composite_over_background(raw, GATE_ACTIVE_BG_RGB))
        if need_transparent:
            transparent_path.write_bytes(raw)

    return transparent_path, gate_path


def ensure_lucide_gallery_assets(spec: IconSpec, slug: str, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Lucide's own `ensure_gallery_assets` -- see that function's own doc
    comment."""
    transparent_path = ICONS_DIR / f"lucide_{spec.stem()}_gallery.rgba"
    gate_path = ICONS_DIR / f"lucide_{spec.stem()}_gallery_gate.rgba"
    need_transparent = force or not transparent_path.exists() or transparent_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN
    need_gate = force or not gate_path.exists() or gate_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN

    if need_transparent or need_gate:
        svg = fetch_lucide_svg(slug, cache_dir)
        patched = patch_lucide(svg, slug, cache_dir)
        raw = rasterize_gallery_sixel(patched)
        if need_gate:
            gate_path.write_bytes(composite_over_background(raw, GATE_ACTIVE_BG_RGB))
        if need_transparent:
            transparent_path.write_bytes(raw)

    return transparent_path, gate_path


def ensure_lucide_compact_assets(spec: IconSpec, slug: str, cache_dir: Path, force: bool) -> Path:
    """Lucide's own `ensure_compact_assets` -- see that function's own doc
    comment (real transparency, no gate variant, this tier's `.rgba` IS
    the shipped asset)."""
    sixel_path = ICONS_DIR / f"lucide_{spec.stem()}_compact.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != COMPACT_SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_lucide_svg(slug, cache_dir)
        patched = patch_lucide(svg, slug, cache_dir)
        raw = rasterize_compact_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


def rust_string_literal(s: str) -> str:
    escaped = s.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def generate_catalog() -> str:
    lines: list[str] = []
    lines.append("//! GENERATED by `tools/bake_icons.py` -- do not hand-edit. Re-run:")
    lines.append("//!   python tools/bake_icons.py")
    lines.append("//!")
    lines.append("//! Licence (codicons): MIT (microsoft/vscode-codicons,")
    lines.append("//! <https://github.com/microsoft/vscode-codicons/blob/main/LICENSE>).")
    lines.append("//! Source: raw.githubusercontent.com/microsoft/vscode-codicons/main/src/")
    lines.append("//! icons/<slug>.svg, `fill=\"currentColor\"` patched to `#cdd6f4`")
    lines.append("//! (`pty_palette::GATE_FG`) before rasterizing.")
    lines.append("//!")
    lines.append("//! Licence (Lucide, the `_lucide`/`lucide_*`-prefixed items below):")
    lines.append("//! ISC (Copyright (c) 2026 Lucide Icons and Contributors) for the set as a")
    lines.append("//! whole, PLUS MIT (Copyright (c) 2013-present Cole Bemis) for a named")
    lines.append("//! subset derived from the Feather project -- both permissive, read")
    lines.append("//! directly from <https://github.com/lucide-icons/lucide/blob/main/")
    lines.append("//! LICENSE> at authoring time, not assumed. Source: raw.githubusercontent.")
    lines.append("//! com/lucide-icons/lucide/main/icons/<slug>.svg, `stroke=\"currentColor\"`")
    lines.append("//! patched to `#cdd6f4` and `stroke-width=\"2\"` to `LUCIDE_STROKE_WIDTH`")
    lines.append("//! (`tools/bake_icons.py`'s own constant) before rasterizing.")
    lines.append("//!")
    lines.append("//! See `../icons.rs`'s own module doc for the full tier/pipeline")
    lines.append("//! explanation and `tools/bake_icons.py`'s own header for the exact bake")
    lines.append("//! recipe (shared by both families -- only fetch/patch differ).")
    lines.append("")
    lines.append("use std::sync::LazyLock;")
    lines.append("")
    lines.append("use super::{")
    lines.append("    build_sixel_compact, build_sixel_gallery_gate, build_sixel_gate, build_sixel_strip_gate,")
    lines.append("    SixelVariant,")
    lines.append("};")
    lines.append("")
    lines.append("/// Every baked icon this crate ships, sixel + ascii tiers, one enum")
    lines.append("/// covering the full catalog (not just the activity rail -- see `../")
    lines.append("/// icons.rs`'s own module doc). Only the activity rail's original 7")
    lines.append("/// variants are wired into a UI site today; the rest are baked and tested")
    lines.append("/// but not yet drawn anywhere -- a deliberate, scoped-out next slice, not")
    lines.append("/// an oversight.")
    lines.append("#[derive(Clone, Copy, Debug, Eq, PartialEq)]")
    lines.append("pub enum IconId {")
    for spec in MANIFEST:
        lines.append(f"    {spec.rust_name},")
    lines.append("}")
    lines.append("")
    lines.append("impl IconId {")
    lines.append(f"    pub const ALL: [IconId; {len(MANIFEST)}] = [")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name},")
    lines.append("    ];")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded rail-tier sixel string for `id` at `variant`'s own explicit")
    lines.append("/// truecolor background (see [`SixelVariant`]'s own doc comment) -- see")
    lines.append("/// `../icons.rs::build_sixel_gate`'s own doc comment for why this is cached")
    lines.append("/// (`LazyLock`) rather than re-encoded per call.")
    lines.append("pub fn sixel(id: IconId, variant: SixelVariant) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"        IconId::{spec.rust_name} => match variant {{")
        lines.append(f"            SixelVariant::GateActive => {upper}_SIXEL_GATE_ACTIVE.as_str(),")
        lines.append(f"            SixelVariant::GateAccent => {upper}_SIXEL_GATE_ACCENT.as_str(),")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded control-plane-strip-tier sixel string for `id` at `variant`'s")
    lines.append("/// own explicit truecolor background -- the strip never shows a selected")
    lines.append("/// state (see `../render.rs::render_control_strip_button`'s own doc")
    lines.append("/// comment), so `GateAccent` resolves to the SAME asset as `GateActive`")
    lines.append("/// here.")
    lines.append("pub fn sixel_strip(id: IconId, variant: SixelVariant) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"        IconId::{spec.rust_name} => match variant {{")
        lines.append(f"            SixelVariant::GateActive | SixelVariant::GateAccent => {upper}_SIXEL_STRIP_GATE.as_str(),")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded icon-gallery-tier sixel string for `id` at `variant`'s own")
    lines.append("/// explicit truecolor background -- the gallery is a read-only comparison")
    lines.append("/// grid with no selected state, so `GateAccent` resolves to the SAME asset")
    lines.append("/// as `GateActive` here (same fold as [`sixel_strip`]).")
    lines.append("pub fn sixel_gallery(id: IconId, variant: SixelVariant) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"        IconId::{spec.rust_name} => match variant {{")
        lines.append(f"            SixelVariant::GateActive | SixelVariant::GateAccent => {upper}_SIXEL_GALLERY_GATE.as_str(),")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded COMPACT-tier sixel string for `id` (exactly one assumed")
    lines.append("/// terminal cell -- see `../icons.rs`'s own module doc) -- for dense")
    lines.append("/// single-row buttons where the rail's own icon does not fit.")
    lines.append("pub fn sixel_compact(id: IconId) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_SIXEL_COMPACT.as_str(),")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Short (<=2 char) plain-ASCII label for `id` -- the rail's own existing")
    lines.append("/// letters (`F`/`G`/`A`/`K`/`S`/`<`/`>`) are reproduced unchanged for the")
    lines.append("/// original 7; every other icon gets an obvious short mark (see")
    lines.append("/// `tools/bake_icons.py::MANIFEST` for the reasoning per icon).")
    lines.append("pub fn ascii(id: IconId) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {rust_string_literal(spec.ascii)},")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("// Raw true-coverage baked-source lookups by id -- used ONLY by this")
    lines.append("// crate's own unit tests (byte-length assertions and the gate-compositing")
    lines.append("// pixel checks): the rail/strip/gallery raw sources are no longer a")
    lines.append("// shipped `SixelVariant` (cause 4's own retirement, `tools/bake_icons.py`'s")
    lines.append("// own header doc comment), so their underlying `_RGBA` consts below are")
    lines.append("// `#[cfg(test)]`-gated too -- nothing outside this test module ever reaches")
    lines.append("// for them. The compact tier's own raw source stays unconditional: it IS")
    lines.append("// the real, directly-shipped asset (see `sixel_compact` above).")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_compact_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_COMPACT_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_gate_active_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_GATE_ACTIVE_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_gate_accent_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_GATE_ACCENT_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_strip_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_STRIP_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_strip_gate_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_STRIP_GATE_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_gallery_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_GALLERY_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_gallery_gate_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_GALLERY_GATE_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")

    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"// ---- {spec.rust_name} ({spec.slug}) " + "-" * max(1, 60 - len(spec.rust_name) - len(spec.slug)))
        lines.append("")
        lines.append("/// Raw true-coverage source, test-only -- no longer a shipped")
        lines.append("/// `SixelVariant` (see `tools/bake_icons.py`'s own header doc comment,")
        lines.append("/// cause 4's retirement); kept solely so this crate's own compositing-")
        lines.append("/// correctness tests can check `GATE_ACTIVE`/`GATE_ACCENT` below against")
        lines.append("/// a real checked-in buffer.")
        lines.append("#[cfg(test)]")
        lines.append(f'const {upper}_RGBA: &[u8] = include_bytes!("{spec.stem()}.rgba");')
        lines.append("")
        lines.append("/// At-rest background (`render::ACTIVE_BG`) -- pre-composited opaque at")
        lines.append("/// bake time, gamma-correct linear blend, painted by every icon-bearing")
        lines.append("/// button in every `PtyColorMode` (see this crate's own `tools/")
        lines.append("/// bake_icons.py` module doc, causes 1, 3 and 4).")
        lines.append(f'const {upper}_GATE_ACTIVE_RGBA: &[u8] = include_bytes!("{spec.stem()}_gate_active.rgba");')
        lines.append(f"static {upper}_SIXEL_GATE_ACTIVE: LazyLock<String> = LazyLock::new(|| build_sixel_gate({upper}_GATE_ACTIVE_RGBA));")
        lines.append("")
        lines.append("/// Selected/accent background (`render::MAUVE`) -- same fix, the rail's")
        lines.append("/// own selected-state background.")
        lines.append(f'const {upper}_GATE_ACCENT_RGBA: &[u8] = include_bytes!("{spec.stem()}_gate_accent.rgba");')
        lines.append(f"static {upper}_SIXEL_GATE_ACCENT: LazyLock<String> = LazyLock::new(|| build_sixel_gate({upper}_GATE_ACCENT_RGBA));")
        lines.append("")
        lines.append(f'const {upper}_COMPACT_RGBA: &[u8] = include_bytes!("{spec.stem()}_compact.rgba");')
        lines.append(f"static {upper}_SIXEL_COMPACT: LazyLock<String> = LazyLock::new(|| build_sixel_compact({upper}_COMPACT_RGBA));")
        lines.append("")
        lines.append("/// Raw true-coverage strip-tier source, test-only -- same retirement as")
        lines.append(f"/// {upper}_RGBA above.")
        lines.append("#[cfg(test)]")
        lines.append(f'const {upper}_STRIP_RGBA: &[u8] = include_bytes!("{spec.stem()}_strip.rgba");')
        lines.append("")
        lines.append("/// The strip's own single background (`render::ACTIVE_BG`) -- pre-")
        lines.append("/// composited opaque at bake time, same fix as the rail tier above; the")
        lines.append("/// ONLY strip-tier sixel this crate ships.")
        lines.append(f'const {upper}_STRIP_GATE_RGBA: &[u8] = include_bytes!("{spec.stem()}_strip_gate.rgba");')
        lines.append(f"static {upper}_SIXEL_STRIP_GATE: LazyLock<String> = LazyLock::new(|| build_sixel_strip_gate({upper}_STRIP_GATE_RGBA));")
        lines.append("")
        lines.append("/// Raw true-coverage gallery-tier source, test-only -- same retirement as")
        lines.append(f"/// {upper}_RGBA above.")
        lines.append("#[cfg(test)]")
        lines.append(f'const {upper}_GALLERY_RGBA: &[u8] = include_bytes!("{spec.stem()}_gallery.rgba");')
        lines.append("")
        lines.append("/// The icon gallery's own single background (`render::ACTIVE_BG`) --")
        lines.append("/// pre-composited opaque at bake time, same fix as the rail/strip tiers")
        lines.append("/// above; the ONLY gallery-tier sixel this crate ships.")
        lines.append(f'const {upper}_GALLERY_GATE_RGBA: &[u8] = include_bytes!("{spec.stem()}_gallery_gate.rgba");')
        lines.append(f"static {upper}_SIXEL_GALLERY_GATE: LazyLock<String> = LazyLock::new(|| build_sixel_gallery_gate({upper}_GALLERY_GATE_RGBA));")
        lines.append("")

    # ---- Lucide dispatch -- alongside every codicon fn above, never
    # replacing it (see `app::IconFamily`'s own doc comment: `Codicons`
    # stays the default, unchanged). `Option` (never a bare `&'static
    # str`) because the two `LUCIDE_GAPS` icons have no asset to resolve
    # at all -- a reported gap, not a panic and not a silent codicon
    # fallback (see `render::render_gallery_size_swatch`'s own doc
    # comment for how the ONE call site that can hit `None` today, the
    # icon gallery, handles it).
    lines.append("/// Encoded rail-tier Lucide sixel string for `id` at `variant`'s own")
    lines.append("/// explicit truecolor background -- `None` for the two documented")
    lines.append("/// mapping gaps (`tools/bake_icons.py::LUCIDE_GAPS`); every other `IconId`")
    lines.append("/// is always `Some`. Mirrors [`sixel`], the codicon equivalent.")
    lines.append("pub fn sixel_lucide(id: IconId, variant: SixelVariant) -> Option<&'static str> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(match variant {{")
            lines.append(f"            SixelVariant::GateActive => LUCIDE_{upper}_SIXEL_GATE_ACTIVE.as_str(),")
            lines.append(f"            SixelVariant::GateAccent => LUCIDE_{upper}_SIXEL_GATE_ACCENT.as_str(),")
            lines.append("        }),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Lucide equivalent of [`sixel_strip`] -- same `None`-for-gaps contract")
    lines.append("/// as [`sixel_lucide`], same `GateAccent` fold into `GateActive` (the")
    lines.append("/// strip has no selected state either family needs a distinct asset for).")
    lines.append("pub fn sixel_strip_lucide(id: IconId, variant: SixelVariant) -> Option<&'static str> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(match variant {{")
            lines.append(f"            SixelVariant::GateActive | SixelVariant::GateAccent => LUCIDE_{upper}_SIXEL_STRIP_GATE.as_str(),")
            lines.append("        }),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Lucide equivalent of [`sixel_gallery`] -- same `None`-for-gaps")
    lines.append("/// contract, same `GateAccent` fold as [`sixel_strip_lucide`].")
    lines.append("pub fn sixel_gallery_lucide(id: IconId, variant: SixelVariant) -> Option<&'static str> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(match variant {{")
            lines.append(f"            SixelVariant::GateActive | SixelVariant::GateAccent => LUCIDE_{upper}_SIXEL_GALLERY_GATE.as_str(),")
            lines.append("        }),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Lucide equivalent of [`sixel_compact`] -- same `None`-for-gaps")
    lines.append("/// contract; no `SixelVariant` parameter, same reason `sixel_compact`")
    lines.append("/// has none (real transparency, no pre-composited background variant).")
    lines.append("pub fn sixel_compact_lucide(id: IconId) -> Option<&'static str> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(LUCIDE_{upper}_SIXEL_COMPACT.as_str()),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// The Lucide slug `id` was baked from, or `None` for the two documented")
    lines.append("/// mapping gaps -- the single source of truth `render::render_gallery_")
    lines.append("/// size_swatch` and this crate's own tests both check before assuming a")
    lines.append("/// Lucide asset exists for `id` at all.")
    lines.append("pub fn lucide_slug(id: IconId) -> Option<&'static str> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some({rust_string_literal(LUCIDE_SLUGS[spec.rust_name])}),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("// Raw true-coverage Lucide sources, test-only -- same retirement/")
    lines.append("// `#[cfg(test)]`-gating precedent as the codicon accessors above;")
    lines.append("// `Option` for the same `LUCIDE_GAPS` reason every dispatch fn above has.")
    for test_fn_name, field_suffix in [
        ("lucide_sixel_source_rgba", "_RGBA"),
        ("lucide_sixel_compact_source_rgba", "_COMPACT_RGBA"),
        ("lucide_sixel_gate_active_source_rgba", "_GATE_ACTIVE_RGBA"),
        ("lucide_sixel_gate_accent_source_rgba", "_GATE_ACCENT_RGBA"),
        ("lucide_sixel_strip_source_rgba", "_STRIP_RGBA"),
        ("lucide_sixel_strip_gate_source_rgba", "_STRIP_GATE_RGBA"),
        ("lucide_sixel_gallery_source_rgba", "_GALLERY_RGBA"),
        ("lucide_sixel_gallery_gate_source_rgba", "_GALLERY_GATE_RGBA"),
    ]:
        lines.append("#[cfg(test)]")
        lines.append(f"pub(crate) fn {test_fn_name}(id: IconId) -> Option<&'static [u8]> {{")
        lines.append("    match id {")
        for spec in MANIFEST:
            upper = to_screaming_snake(spec.rust_name)
            if spec.rust_name in LUCIDE_SLUGS:
                lines.append(f"        IconId::{spec.rust_name} => Some(LUCIDE_{upper}{field_suffix}),")
            else:
                lines.append(f"        IconId::{spec.rust_name} => None,")
        lines.append("    }")
        lines.append("}")
        lines.append("")

    for spec in MANIFEST:
        if spec.rust_name not in LUCIDE_SLUGS:
            continue
        upper = to_screaming_snake(spec.rust_name)
        slug = LUCIDE_SLUGS[spec.rust_name]
        lines.append(f"// ---- Lucide {spec.rust_name} ({slug}) " + "-" * max(1, 52 - len(spec.rust_name) - len(slug)))
        lines.append("")
        lines.append("#[cfg(test)]")
        lines.append(f'const LUCIDE_{upper}_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}.rgba");')
        lines.append("")
        lines.append(f'const LUCIDE_{upper}_GATE_ACTIVE_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_gate_active.rgba");')
        lines.append(f"static LUCIDE_{upper}_SIXEL_GATE_ACTIVE: LazyLock<String> = LazyLock::new(|| build_sixel_gate(LUCIDE_{upper}_GATE_ACTIVE_RGBA));")
        lines.append("")
        lines.append(f'const LUCIDE_{upper}_GATE_ACCENT_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_gate_accent.rgba");')
        lines.append(f"static LUCIDE_{upper}_SIXEL_GATE_ACCENT: LazyLock<String> = LazyLock::new(|| build_sixel_gate(LUCIDE_{upper}_GATE_ACCENT_RGBA));")
        lines.append("")
        lines.append(f'const LUCIDE_{upper}_COMPACT_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_compact.rgba");')
        lines.append(f"static LUCIDE_{upper}_SIXEL_COMPACT: LazyLock<String> = LazyLock::new(|| build_sixel_compact(LUCIDE_{upper}_COMPACT_RGBA));")
        lines.append("")
        lines.append("#[cfg(test)]")
        lines.append(f'const LUCIDE_{upper}_STRIP_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_strip.rgba");')
        lines.append("")
        lines.append(f'const LUCIDE_{upper}_STRIP_GATE_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_strip_gate.rgba");')
        lines.append(f"static LUCIDE_{upper}_SIXEL_STRIP_GATE: LazyLock<String> = LazyLock::new(|| build_sixel_strip_gate(LUCIDE_{upper}_STRIP_GATE_RGBA));")
        lines.append("")
        lines.append("#[cfg(test)]")
        lines.append(f'const LUCIDE_{upper}_GALLERY_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_gallery.rgba");')
        lines.append("")
        lines.append(f'const LUCIDE_{upper}_GALLERY_GATE_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_gallery_gate.rgba");')
        lines.append(f"static LUCIDE_{upper}_SIXEL_GALLERY_GATE: LazyLock<String> = LazyLock::new(|| build_sixel_gallery_gate(LUCIDE_{upper}_GALLERY_GATE_RGBA));")
        lines.append("")

    return "\n".join(lines) + "\n"


def to_screaming_snake(pascal: str) -> str:
    """`SourceControl` -> `SOURCE_CONTROL`."""
    out = re.sub(r"(?<!^)(?=[A-Z])", "_", pascal).upper()
    return out


def print_report(
    asset_sizes: dict[str, tuple[int, bool]],
    gate_asset_sizes: dict[str, tuple[int, int, bool]],
    compact_asset_sizes: dict[str, tuple[int, bool]],
    strip_asset_sizes: dict[str, tuple[int, int, bool]],
    gallery_asset_sizes: dict[str, tuple[int, int, bool]],
    lucide_asset_sizes: dict[str, tuple[int, int, int, int, int, bool]],
) -> None:
    print()
    print("=" * 78)
    print("LUCIDE MAPPING (IconId -> Lucide slug, one entry per icon)")
    print("=" * 78)
    for spec in MANIFEST:
        slug = LUCIDE_SLUGS.get(spec.rust_name)
        if slug is not None:
            print(f"  {spec.rust_name:<18} -> {slug}")
    print()
    print(f"REPORTED GAPS (no Lucide glyph carries the same meaning -- {len(LUCIDE_GAPS)}):")
    for name, reason in LUCIDE_GAPS.items():
        print(f"  {name}: {reason}")
    print()
    print("=" * 78)
    print("ASSET SIZE TOTALS")
    print("=" * 78)
    total_sixel = sum(s for s, _ in asset_sizes.values())
    newly_baked_sixel = sum(s for s, new in asset_sizes.values() if new)
    n_new = sum(1 for _, new in asset_sizes.values() if new)
    print(f"Icons total: {len(MANIFEST)}  (newly baked this run: {n_new})")
    print(f"Sixel tier:   {total_sixel:>9} bytes total ({total_sixel / 1024:.1f} KiB)  -- {newly_baked_sixel} bytes newly added")
    total_gate = total_compact_sixel = total_strip = total_gallery = 0
    if gate_asset_sizes:
        total_gate = sum(a + c for a, c, _ in gate_asset_sizes.values())
        newly_baked_gate = sum(a + c for a, c, new in gate_asset_sizes.values() if new)
        print(f"Rail GateOverride variants (active+accent): {total_gate:>9} bytes total ({total_gate / 1024:.1f} KiB) -- {newly_baked_gate} bytes newly added")
    if compact_asset_sizes:
        total_compact_sixel = sum(s for s, _ in compact_asset_sizes.values())
        newly_baked_compact_sixel = sum(s for s, new in compact_asset_sizes.values() if new)
        print(f"Compact sixel tier:   {total_compact_sixel:>9} bytes total ({total_compact_sixel / 1024:.1f} KiB)  -- {newly_baked_compact_sixel} bytes newly added")
    if strip_asset_sizes:
        total_strip = sum(t + g for t, g, _ in strip_asset_sizes.values())
        newly_baked_strip = sum(t + g for t, g, new in strip_asset_sizes.values() if new)
        print(f"Strip sixel tier (transparent+gate): {total_strip:>9} bytes total ({total_strip / 1024:.1f} KiB) -- {newly_baked_strip} bytes newly added")
    if gallery_asset_sizes:
        total_gallery = sum(t + g for t, g, _ in gallery_asset_sizes.values())
        newly_baked_gallery = sum(t + g for t, g, new in gallery_asset_sizes.values() if new)
        print(f"Gallery sixel tier (transparent+gate): {total_gallery:>9} bytes total ({total_gallery / 1024:.1f} KiB) -- {newly_baked_gallery} bytes newly added")
    codicon_combined = total_sixel + total_gate + total_compact_sixel + total_strip + total_gallery
    print(f"Combined (codicons):     {codicon_combined:>9} bytes total ({codicon_combined / 1024:.1f} KiB)")
    lucide_combined = 0
    if lucide_asset_sizes:
        total_lucide_gate = sum(gate for gate, _, _, _, _, _ in lucide_asset_sizes.values())
        total_lucide_compact = sum(compact for _, compact, _, _, _, _ in lucide_asset_sizes.values())
        total_lucide_strip = sum(strip for _, _, strip, _, _, _ in lucide_asset_sizes.values())
        total_lucide_gallery = sum(gallery for _, _, _, gallery, _, _ in lucide_asset_sizes.values())
        newly_baked_lucide = sum(
            gate + compact + strip + gallery
            for gate, compact, strip, gallery, _, new in lucide_asset_sizes.values()
            if new
        )
        lucide_combined = total_lucide_gate + total_lucide_compact + total_lucide_strip + total_lucide_gallery
        print(f"Lucide icons total: {len(lucide_asset_sizes)}  (of {len(MANIFEST)} in MANIFEST, {len(LUCIDE_GAPS)} reported gaps)")
        print(f"Lucide rail GateActive+GateAccent:  {total_lucide_gate:>9} bytes total ({total_lucide_gate / 1024:.1f} KiB)")
        print(f"Lucide compact tier:                {total_lucide_compact:>9} bytes total ({total_lucide_compact / 1024:.1f} KiB)")
        print(f"Lucide strip tier (gate):           {total_lucide_strip:>9} bytes total ({total_lucide_strip / 1024:.1f} KiB)")
        print(f"Lucide gallery tier (gate):         {total_lucide_gallery:>9} bytes total ({total_lucide_gallery / 1024:.1f} KiB)")
        print(f"Combined (Lucide):        {lucide_combined:>9} bytes total ({lucide_combined / 1024:.1f} KiB) -- {newly_baked_lucide} bytes newly added")
    print(
        f"Combined (codicons + Lucide): {codicon_combined + lucide_combined:>9} bytes total "
        f"({(codicon_combined + lucide_combined) / 1024:.1f} KiB)"
    )
    if CATALOG_RS.exists():
        catalog_size = CATALOG_RS.stat().st_size
        print(f"catalog.rs generated source: {catalog_size} bytes ({catalog_size / 1024:.1f} KiB)")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--cache-dir", type=Path, default=DEFAULT_CACHE_DIR, help="SVG download cache (default: tools/.codicon-cache/)")
    parser.add_argument("--force", action="store_true", help="re-bake every icon's raster assets even if already present")
    parser.add_argument("--only", type=str, default=None, help="comma-separated slugs to restrict processing to (skips catalog.rs regeneration)")
    return parser.parse_args(argv)


def check_lucide_coverage() -> None:
    """Every `MANIFEST` icon must appear in EXACTLY ONE of `LUCIDE_SLUGS`
    (has a Lucide asset) or `LUCIDE_GAPS` (documented, deliberate, no
    asset) -- never neither (a silently-uncovered icon) and never both
    (an icon this file cannot decide about). Run once at the top of
    `main`, before any network or subprocess work, so a manifest edit
    that adds an `IconId` without updating either dict fails loudly and
    immediately rather than baking an incomplete catalog."""
    manifest_names = {spec.rust_name for spec in MANIFEST}
    slug_names = set(LUCIDE_SLUGS)
    gap_names = set(LUCIDE_GAPS)
    overlap = slug_names & gap_names
    if overlap:
        die(f"icons in BOTH LUCIDE_SLUGS and LUCIDE_GAPS: {sorted(overlap)}")
    covered = slug_names | gap_names
    uncovered = manifest_names - covered
    if uncovered:
        die(f"MANIFEST icons with NO Lucide decision (add to LUCIDE_SLUGS or LUCIDE_GAPS): {sorted(uncovered)}")
    stale = covered - manifest_names
    if stale:
        die(f"LUCIDE_SLUGS/LUCIDE_GAPS name icons not in MANIFEST: {sorted(stale)}")


def main(argv: list[str]) -> int:
    check_lucide_coverage()
    args = parse_args(argv)
    only = set(s.strip() for s in args.only.split(",")) if args.only else None

    selected = [spec for spec in MANIFEST if only is None or spec.slug in only]
    if only is not None:
        missing = only - {spec.slug for spec in selected}
        if missing:
            die(f"--only names not in MANIFEST: {sorted(missing)}")

    asset_sizes: dict[str, tuple[int, bool]] = {}
    gate_asset_sizes: dict[str, tuple[int, int, bool]] = {}
    compact_asset_sizes: dict[str, tuple[int, bool]] = {}
    strip_asset_sizes: dict[str, tuple[int, int, bool]] = {}
    gallery_asset_sizes: dict[str, tuple[int, int, bool]] = {}
    for spec in selected:
        sixel_existed = (ICONS_DIR / f"{spec.stem()}.rgba").exists()
        active_existed = (ICONS_DIR / f"{spec.stem()}_gate_active.rgba").exists()
        accent_existed = (ICONS_DIR / f"{spec.stem()}_gate_accent.rgba").exists()
        sixel_path, active_path, accent_path = ensure_assets(spec, args.cache_dir, args.force)
        asset_sizes[spec.slug] = (sixel_path.stat().st_size, args.force or not sixel_existed)
        gate_asset_sizes[spec.slug] = (
            active_path.stat().st_size,
            accent_path.stat().st_size,
            args.force or not active_existed or not accent_existed,
        )

        compact_sixel_existed = (ICONS_DIR / f"{spec.stem()}_compact.rgba").exists()
        compact_sixel_path = ensure_compact_assets(spec, args.cache_dir, args.force)
        compact_asset_sizes[spec.slug] = (compact_sixel_path.stat().st_size, args.force or not compact_sixel_existed)

        strip_existed = (ICONS_DIR / f"{spec.stem()}_strip.rgba").exists()
        strip_gate_existed = (ICONS_DIR / f"{spec.stem()}_strip_gate.rgba").exists()
        strip_path, strip_gate_path = ensure_strip_assets(spec, args.cache_dir, args.force)
        strip_asset_sizes[spec.slug] = (
            strip_path.stat().st_size,
            strip_gate_path.stat().st_size,
            args.force or not strip_existed or not strip_gate_existed,
        )

        gallery_existed = (ICONS_DIR / f"{spec.stem()}_gallery.rgba").exists()
        gallery_gate_existed = (ICONS_DIR / f"{spec.stem()}_gallery_gate.rgba").exists()
        gallery_path, gallery_gate_path = ensure_gallery_assets(spec, args.cache_dir, args.force)
        gallery_asset_sizes[spec.slug] = (
            gallery_path.stat().st_size,
            gallery_gate_path.stat().st_size,
            args.force or not gallery_existed or not gallery_gate_existed,
        )

    lucide_asset_sizes: dict[str, tuple[int, int, int, int, int, bool]] = {}
    for spec in selected:
        lucide_slug = LUCIDE_SLUGS.get(spec.rust_name)
        if lucide_slug is None:
            continue  # LUCIDE_GAPS -- no asset to bake, see that dict's own doc comment.
        sixel_existed = (ICONS_DIR / f"lucide_{spec.stem()}.rgba").exists()
        active_existed = (ICONS_DIR / f"lucide_{spec.stem()}_gate_active.rgba").exists()
        accent_existed = (ICONS_DIR / f"lucide_{spec.stem()}_gate_accent.rgba").exists()
        _, active_path, accent_path = ensure_lucide_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        compact_existed = (ICONS_DIR / f"lucide_{spec.stem()}_compact.rgba").exists()
        compact_path = ensure_lucide_compact_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        strip_existed = (ICONS_DIR / f"lucide_{spec.stem()}_strip.rgba").exists()
        strip_gate_existed = (ICONS_DIR / f"lucide_{spec.stem()}_strip_gate.rgba").exists()
        _, strip_gate_path = ensure_lucide_strip_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        gallery_existed = (ICONS_DIR / f"lucide_{spec.stem()}_gallery.rgba").exists()
        gallery_gate_existed = (ICONS_DIR / f"lucide_{spec.stem()}_gallery_gate.rgba").exists()
        _, gallery_gate_path = ensure_lucide_gallery_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        lucide_asset_sizes[spec.rust_name] = (
            active_path.stat().st_size + accent_path.stat().st_size,
            compact_path.stat().st_size,
            strip_gate_path.stat().st_size,
            gallery_gate_path.stat().st_size,
            0,
            args.force or not (
                sixel_existed and active_existed and accent_existed and compact_existed
                and strip_existed and strip_gate_existed and gallery_existed and gallery_gate_existed
            ),
        )

    if only is None:
        # Full manifest processed -- every icon has assets on disk, safe
        # to regenerate the complete catalog.
        CATALOG_RS.write_text(generate_catalog(), encoding="utf-8", newline="\n")
        print(f"wrote {CATALOG_RS} ({CATALOG_RS.stat().st_size} bytes)")
    else:
        print(f"--only restricted this run to {sorted(only)} -- catalog.rs NOT regenerated (needs the full manifest)")

    print_report(asset_sizes, gate_asset_sizes, compact_asset_sizes, strip_asset_sizes, gallery_asset_sizes, lucide_asset_sizes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
