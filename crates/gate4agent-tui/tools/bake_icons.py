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
     reading Python. This tool writes ONLY that raw, TRUE-coverage buffer
     to disk (`<stem>.rgba`/`<stem>_strip.rgba`/`<stem>_gallery.rgba`/
     `<stem>_compact.rgba`) -- it no longer derives any background-
     composited variant at all (see cause 1 below for why baking a fixed
     background here was itself the defect this tool used to ship: the
     button's own real background is a RUNTIME fact -- either a live OSC
     11 terminal query or the app's own override theme, decided per RUN,
     never knowable at bake time -- so compositing now happens in Rust,
     on demand, against whichever background is actually in play; see
     `../src/icons.rs::composite_over_background`). SKIPPED (idempotent,
     no network/subprocess work at all) for any icon whose raw outputs
     already exist on disk at the expected byte length, unless `--force`.
     Lucide reuses these EXACT SAME rasterize functions unchanged (see
     `ensure_lucide_assets` and friends, right below the codicon `ensure_
     *` functions) -- the lattice-fit machinery cause 6 below describes is
     family-agnostic, only the fetch/patch step (step 1/2 above) differs.
  4. Regenerate `src/icons/catalog.rs` from the FULL manifest (only when
     not restricted by `--only`) -- one `IconId` enum variant (shared by
     both families), one raw-bytes lookup per tier/family (the compact
     tier ALSO keeps its own pre-encoded `LazyLock<String>`, see cause 1
     below for why compact is the one tier that stays exactly as it was),
     one Lucide `Option`-wrapped raw-bytes lookup per tier (`None` for the
     two `LUCIDE_GAPS` icons), one ascii literal, one `lucide_slug`
     mapping entry, per icon.
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
   cannot survive this encoder in alpha at all, only in RGB. FIX: composite
   EVERY sixel-tier icon (rail/strip/gallery -- compact is out of scope,
   see `ensure_compact_assets`'s own doc comment) over the EXACT
   background colour the button actually shows there, fully opaque, so
   the encoder's threshold and the terminal's transparency support both
   become irrelevant -- there is no transparent pixel left to mishandle,
   and the anti-aliasing this buys back rides in the RGB channels
   instead, where the encoder's own hard threshold cannot touch it.

   FIRST ITERATION (retired) -- baked a FIXED composite per background at
   THIS tool's own bake time (`composite_over_background`, a pure-Python
   pass over an already-rasterized buffer -- never a second resvg/ffmpeg
   call, so it could never regress into cause 2's own double-resampling
   anti-pattern), against two hand-picked constants
   (`GATE_ACTIVE_BG_RGB`/`GATE_ACCENT_BG_RGB`, hand-synced to `render.rs`'s
   own `ACTIVE_BG`/`MAUVE`) written into every icon-bearing button's own
   cell background UNCONDITIONALLY, in every `PtyColorMode` (see cause 4
   below, retired). That traded the dirty-edge defect for a NEW one: in
   `PtyColorMode::Inherited` the terminal's own real background is neither
   of those two constants (measured on the owner's own Windows Terminal:
   (12,12,12), nowhere close to `ACTIVE_BG`'s (30,30,46)), so every icon-
   bearing surface now painted a visibly LIGHTER, "standing out" plate
   against the actually-darker terminal around it -- the button's own
   stated background was simply wrong, not merely un-painted. The
   underlying premise ("the background is unknowable at bake time, so
   fix ONE and paint it everywhere") was itself the mistake: a terminal's
   real background is not a build-time fact at all, it is whatever THAT
   terminal reports at THAT run, over OSC 11 (`ESC ] 11 ; ? BEL`) --
   knowable at RUNTIME, never at bake time, and never a single constant
   across every environment this binary ships to.

   CURRENT FIX -- compositing moved OUT of this tool entirely and into
   Rust, at RUNTIME (`../src/icons.rs::composite_over_background`, the
   exact same linear-light arithmetic cause 3 below established, ported
   byte-for-byte rather than re-derived): the client queries the real
   terminal background once at startup (`client.rs`, before the alternate
   screen takes over stdin) and composites each icon on demand against
   whichever concrete background is actually live for that placement --
   the queried terminal colour (or a documented dark fallback if the
   terminal never answers) for an UN-selected button in `PtyColorMode::
   Inherited`, the app's own fixed `GateOverride` theme colour for that
   same mode, and the fixed selected-state accent (`render.rs`'s own
   `MAUVE`) wherever a button is genuinely selected -- cached by (icon,
   tier, family, background RGB) so the encode itself still only happens
   once per distinct combination actually seen, never per frame. This
   tool's OWN job shrank accordingly: it rasterizes and ships ONLY the
   raw, TRUE-coverage source buffer per icon/tier/family (`ensure_assets`/
   `ensure_strip_assets`/`ensure_gallery_assets` and their Lucide
   equivalents below) -- there is no more `_gate`/`_gate_active`/
   `_gate_accent` variant baked here at all, and no more `GATE_ACTIVE_BG_
   RGB`/`GATE_ACCENT_BG_RGB`/`composite_over_background` in this file.

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
   worst at the smallest tier) -- CONFIRMED. A naive compositor blends the
   straight 8-bit sRGB channel bytes directly (`out = ink*(a/255) +
   bg*(1-a/255)`), which is wrong: sRGB is a non-linear encoding of
   light, so a coverage weight (what `a` actually is here -- resvg's own
   straight-alpha convention, see `../icons.rs`'s own module doc) must be
   blended in LINEAR light, not in the gamma-encoded byte domain. Against
   this crate's own ink `#cdd6f4` (204,214,242) on a near-black terminal
   background (12,12,12) the error is large and systematic (R channel):
   25% coverage -> naive 60, correct 109; 50% -> naive 108, correct 150;
   75% -> naive 156, correct 180 -- every anti-aliased edge pixel lands
   24-49 levels too dark. A codicon stroke is ~1.5 units in a 24-unit
   viewBox -- at the strip tier's own ~19px height that is barely more
   than one device pixel, i.e. ALMOST ENTIRELY edge pixels, which is
   exactly why the small tier reads as grainy and washed out while the
   rail tier (a wider stroke in device pixels, surviving core ink pixels)
   reads acceptable. FIX: convert both the ink and the background from
   sRGB to linear (`srgb_to_linear`, the exact piecewise transfer
   function -- the 0.04045 / 12.92 / 2.4 form, NOT a 2.2-power
   approximation), blend by the pixel's own TRUE coverage in linear
   space, then convert back (`linear_to_srgb`). This arithmetic now lives
   in `../src/icons.rs::composite_over_background` (see cause 1 above's
   own "CURRENT FIX" note for why it moved out of this file), NOT here
   any more -- this tool's own `srgb_to_linear`/`linear_to_srgb` were
   ported byte-for-byte into that Rust function and then deleted from
   this file so there is exactly one implementation, never two that could
   drift; every anti-aliased pixel a user actually sees goes through that
   ONE exact (never approximated) blend, regardless of which background
   it happens to composite against.

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

# No icon-button compositing background lives here any more (cause 1's own
# "CURRENT FIX" note, this module's own header doc comment): which RGB an
# icon composites against is a RUNTIME decision now (a live OSC 11 query,
# an override theme constant, or the selected-state accent), made in
# `../src/icons.rs`/`../src/render.rs`, never a bake-time Python constant.


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


# The straight-alpha (non-premultiplied) output assumption that
# `icons.rs::composite_over_background` and every colour in this pipeline
# rests on was established ONCE, by hand, against this exact resvg build
# (see `icons.rs`'s "cause 1" note). A resvg that emits premultiplied
# alpha, or rounds `-w`/`-h` differently, would silently corrupt every
# baked colour with no signal at all -- so the version is checked here
# rather than only asserted in prose, matching this tool's own
# fetch/patch contract: FAIL LOUDLY, never proceed on a guess.
REQUIRED_RESVG_VERSION = "0.47.0"


def require_resvg_version() -> None:
    try:
        result = subprocess.run(["resvg", "--version"], capture_output=True, text=True)
    except OSError as error:
        die(f"resvg is not on PATH: {error}")
    if result.returncode != 0:
        die(f"`resvg --version` failed: {result.stderr.strip()}")
    found = result.stdout.strip()
    if found != REQUIRED_RESVG_VERSION:
        die(
            f"resvg {found} is on PATH, but every baked asset in this crate was "
            f"produced with resvg {REQUIRED_RESVG_VERSION}, and the straight-alpha "
            "output assumption the whole compositing path depends on was verified "
            "against that build only. Install the pinned version, or re-verify the "
            "alpha assumption and update REQUIRED_RESVG_VERSION deliberately."
        )


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


def ensure_assets(spec: IconSpec, cache_dir: Path, force: bool) -> Path:
    """Ensure the rail-tier `.rgba` raw source for `spec` exists on disk
    (baking it if missing, or unconditionally if `force`) and return its
    path. This is the idempotency boundary: a normal re-run with nothing
    new to bake touches no network and spawns no subprocess at all.

    This is the ONLY rail-tier asset this tool ships now (cause 1's own
    "CURRENT FIX" note, this module's own header doc comment): there is
    no more `_gate`/`_gate_active`/`_gate_accent` variant baked here --
    compositing against a concrete background happens in Rust, at
    runtime, against whichever background is actually live for a given
    placement (`../src/icons.rs::composite_over_background`), so this
    tool has nothing left to precompute beyond the raw, TRUE-coverage
    buffer resvg itself produces."""
    sixel_path = ICONS_DIR / f"{spec.stem()}.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


def ensure_strip_assets(spec: IconSpec, cache_dir: Path, force: bool) -> Path:
    """Ensure the control-plane strip's own single sixel-tier output
    (`<stem>_strip.rgba`, raw true-coverage) exists on disk and return its
    path -- see `ensure_assets`'s own doc comment for why there is no
    separate `_gate` variant here any more.

    Sources from `spec.strip_source_slug()`, NOT `spec.slug` -- cause 7
    (this module's own header doc comment): `NewFile`/`NewFolder` bake
    their own strip tier from a different, badge-free codicon than their
    rail/compact/gallery tiers; every other icon's `strip_source_slug()`
    is just `slug` unchanged."""
    sixel_path = ICONS_DIR / f"{spec.stem()}_strip.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != STRIP_SIXEL_RGBA_LEN
    if need_sixel:
        slug = spec.strip_source_slug()
        svg = fetch_svg(slug, cache_dir)
        patched = patch_fill(svg, slug, cache_dir)
        raw = rasterize_strip_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


def ensure_gallery_assets(spec: IconSpec, cache_dir: Path, force: bool) -> Path:
    """Ensure the icon gallery's own single dedicated sixel-tier output
    (`<stem>_gallery.rgba`, raw true-coverage) exists on disk and return
    its path -- see `ensure_assets`'s own doc comment for why there is no
    separate `_gate` variant here any more."""
    sixel_path = ICONS_DIR / f"{spec.stem()}_gallery.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_gallery_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


def ensure_compact_assets(spec: IconSpec, cache_dir: Path, force: bool) -> Path:
    """Ensure the compact tier's own single sixel-tier output (`<stem>_
    compact.rgba`, real straight-alpha true coverage). This tier is
    `Transparent`-only -- out of scope for the runtime background-
    compositing fix cause 1 describes (see `render::render_compact_icon_
    button`'s own doc comment: dense panel/modal content with 3+ distinct
    backgrounds, unlike the rail's 2 and the strip's 1) -- this tier's own
    `.rgba` IS, and always was, the real, directly-shipped asset
    (`icons::build_sixel_compact` still encodes it with `BackgroundMode::
    Transparent`), unchanged by any of cause 1's iterations."""
    sixel_path = ICONS_DIR / f"{spec.stem()}_compact.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != COMPACT_SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_compact_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


# ---- Lucide bakes -- reuse EVERY rasterize_* function above completely
# unchanged (cause 6's own lattice-fit fix included): those functions
# only ever take "a patched SVG path" + "a target canvas size", never
# anything codicon-specific, so the SAME 16px/32px/48px lattice-fit
# machinery this module's own header doc comment describes for codicons
# applies to Lucide's own 24-unit viewBox identically -- only the FETCH
# (`fetch_lucide_svg` vs `fetch_svg`) and PATCH (`patch_lucide` vs `patch_
# fill`) steps differ, plus the `lucide_` filename prefix so neither
# family's own `.rgba` outputs can collide on disk. Mirrors `ensure_
# assets`/`ensure_strip_assets`/`ensure_gallery_assets`/`ensure_compact_
# assets` one-for-one -- same "raw source is the only shipped asset, no
# gate variant baked here" shape, same compact-tier exception (real
# transparency, no lattice-fit -- the compact canvas is 10x19, too small
# to host even one 16px lattice step in its own narrower dimension, same
# reasoning `glyph_lattice_size`'s own doc comment gives for why
# codicons' compact tier is unchanged by cause 6 either).


def ensure_lucide_assets(spec: IconSpec, slug: str, cache_dir: Path, force: bool) -> Path:
    """Lucide's own `ensure_assets` -- see that function's own doc
    comment; `slug` is `LUCIDE_SLUGS[spec.rust_name]` (the Lucide slug),
    NOT `spec.slug` (the codicon slug `spec` was authored around)."""
    sixel_path = ICONS_DIR / f"lucide_{spec.stem()}.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_lucide_svg(slug, cache_dir)
        patched = patch_lucide(svg, slug, cache_dir)
        raw = rasterize_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


def ensure_lucide_strip_assets(spec: IconSpec, slug: str, cache_dir: Path, force: bool) -> Path:
    """Lucide's own `ensure_strip_assets` -- see that function's own doc
    comment. Always sources from `slug` directly (Lucide has no
    equivalent of cause 7's own codicon-only `strip_slug` badge-swap --
    every `LUCIDE_SLUGS` glyph is a single coherent shape at this tier's
    own 16px lattice render, verified at authoring time)."""
    sixel_path = ICONS_DIR / f"lucide_{spec.stem()}_strip.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != STRIP_SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_lucide_svg(slug, cache_dir)
        patched = patch_lucide(svg, slug, cache_dir)
        raw = rasterize_strip_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


def ensure_lucide_gallery_assets(spec: IconSpec, slug: str, cache_dir: Path, force: bool) -> Path:
    """Lucide's own `ensure_gallery_assets` -- see that function's own doc
    comment."""
    sixel_path = ICONS_DIR / f"lucide_{spec.stem()}_gallery.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_lucide_svg(slug, cache_dir)
        patched = patch_lucide(svg, slug, cache_dir)
        raw = rasterize_gallery_sixel(patched)
        sixel_path.write_bytes(raw)
    return sixel_path


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
    lines.append("//! recipe (shared by both families -- only fetch/patch differ). This")
    lines.append("//! module ships ONLY raw, TRUE-coverage source buffers (plus the compact")
    lines.append("//! tier's own pre-encoded sixel, which never composites against a")
    lines.append("//! background at all) -- every OTHER tier's background compositing is a")
    lines.append("//! RUNTIME decision made by `../icons.rs::composite_over_background`, not")
    lines.append("//! something this generated file bakes in (see `tools/bake_icons.py`'s own")
    lines.append("//! header doc comment, cause 1's \"CURRENT FIX\" note).")
    lines.append("")
    lines.append("use std::sync::LazyLock;")
    lines.append("")
    lines.append("use super::build_sixel_compact;")
    lines.append("")
    lines.append("/// Every baked icon this crate ships, sixel + ascii tiers, one enum")
    lines.append("/// covering the full catalog (not just the activity rail -- see `../")
    lines.append("/// icons.rs`'s own module doc). Only the activity rail's original 7")
    lines.append("/// variants are wired into a UI site today; the rest are baked and tested")
    lines.append("/// but not yet drawn anywhere -- a deliberate, scoped-out next slice, not")
    lines.append("/// an oversight. `Hash` (alongside `Eq`) so `IconId` can key the runtime")
    lines.append("/// sixel cache (`../icons.rs`'s own module doc, \"Sixel background")
    lines.append("/// variants\" section).")
    lines.append("#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]")
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
    lines.append("/// Raw, TRUE-coverage rail-tier source bytes for `id` -- straight (non-")
    lines.append("/// premultiplied) RGBA8, `icons::SIXEL_ICON_WIDTH_PX` x `_HEIGHT_PX`. The")
    lines.append("/// ONLY rail-tier asset this crate ships (see this module's own header doc")
    lines.append("/// comment): `../icons.rs::composite_over_background` composites this on")
    lines.append("/// demand against whichever background a given placement actually needs,")
    lines.append("/// cached by (icon, tier, family, background RGB) rather than pre-baked")
    lines.append("/// per background here.")
    lines.append("pub(crate) fn sixel_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Strip-tier equivalent of [`sixel_source_rgba`] -- `icons::STRIP_SIXEL_")
    lines.append("/// ICON_WIDTH_PX` x `_HEIGHT_PX`.")
    lines.append("pub(crate) fn sixel_strip_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_STRIP_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Gallery-tier equivalent of [`sixel_source_rgba`] -- `icons::GALLERY_")
    lines.append("/// SIXEL_ICON_WIDTH_PX` x `_HEIGHT_PX`.")
    lines.append("pub(crate) fn sixel_gallery_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_GALLERY_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded COMPACT-tier sixel string for `id` (exactly one assumed")
    lines.append("/// terminal cell -- see `../icons.rs`'s own module doc) -- for dense")
    lines.append("/// single-row buttons where the rail's own icon does not fit. Unlike every")
    lines.append("/// other tier above, this one stays pre-encoded at MODULE LOAD (`LazyLock`)")
    lines.append("/// rather than composited on demand: it ships real transparency")
    lines.append("/// (`BackgroundMode::Transparent`), never a background to composite")
    lines.append("/// against at all (see `icons.rs`'s own \"Compact tier\" doc section).")
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
    lines.append("// Compact tier's own raw source, test-only (byte-length assertions) --")
    lines.append("// production code only ever reaches for the pre-encoded `sixel_compact`")
    lines.append("// above; this tier never composites against a background at runtime.")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_compact_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_COMPACT_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")

    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"// ---- {spec.rust_name} ({spec.slug}) " + "-" * max(1, 60 - len(spec.rust_name) - len(spec.slug)))
        lines.append("")
        lines.append(f'const {upper}_RGBA: &[u8] = include_bytes!("{spec.stem()}.rgba");')
        lines.append(f'const {upper}_COMPACT_RGBA: &[u8] = include_bytes!("{spec.stem()}_compact.rgba");')
        lines.append(f"static {upper}_SIXEL_COMPACT: LazyLock<String> = LazyLock::new(|| build_sixel_compact({upper}_COMPACT_RGBA));")
        lines.append(f'const {upper}_STRIP_RGBA: &[u8] = include_bytes!("{spec.stem()}_strip.rgba");')
        lines.append(f'const {upper}_GALLERY_RGBA: &[u8] = include_bytes!("{spec.stem()}_gallery.rgba");')
        lines.append("")

    # ---- Lucide dispatch -- alongside every codicon fn above, never
    # replacing it (see `app::IconFamily`'s own doc comment: `Codicons`
    # stays the default, unchanged). `Option` (never a bare `&'static
    # [u8]`) because the two `LUCIDE_GAPS` icons have no asset to resolve
    # at all -- a reported gap, not a panic and not a silent codicon
    # fallback (see `render::render_gallery_size_swatch`'s own doc
    # comment for how the ONE call site that can hit `None` today, the
    # icon gallery, handles it).
    lines.append("/// Lucide equivalent of [`sixel_source_rgba`] -- `None` for the two")
    lines.append("/// documented mapping gaps (`tools/bake_icons.py::LUCIDE_GAPS`); every")
    lines.append("/// other `IconId` is always `Some`.")
    lines.append("pub(crate) fn lucide_sixel_source_rgba(id: IconId) -> Option<&'static [u8]> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(LUCIDE_{upper}_RGBA),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Lucide equivalent of [`sixel_strip_source_rgba`] -- same `None`-for-")
    lines.append("/// gaps contract as [`lucide_sixel_source_rgba`].")
    lines.append("pub(crate) fn lucide_sixel_strip_source_rgba(id: IconId) -> Option<&'static [u8]> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(LUCIDE_{upper}_STRIP_RGBA),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Lucide equivalent of [`sixel_gallery_source_rgba`] -- same `None`-for-")
    lines.append("/// gaps contract as [`lucide_sixel_source_rgba`].")
    lines.append("pub(crate) fn lucide_sixel_gallery_source_rgba(id: IconId) -> Option<&'static [u8]> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(LUCIDE_{upper}_GALLERY_RGBA),")
        else:
            lines.append(f"        IconId::{spec.rust_name} => None,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Lucide equivalent of [`sixel_compact`] -- same `None`-for-gaps")
    lines.append("/// contract; pre-encoded (real transparency), same reason `sixel_compact`")
    lines.append("/// never composites against a background either.")
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
    lines.append("// Compact tier's own raw Lucide source, test-only -- same precedent as")
    lines.append("// the codicon `sixel_compact_source_rgba` above.")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn lucide_sixel_compact_source_rgba(id: IconId) -> Option<&'static [u8]> {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        if spec.rust_name in LUCIDE_SLUGS:
            lines.append(f"        IconId::{spec.rust_name} => Some(LUCIDE_{upper}_COMPACT_RGBA),")
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
        lines.append(f'const LUCIDE_{upper}_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}.rgba");')
        lines.append(f'const LUCIDE_{upper}_COMPACT_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_compact.rgba");')
        lines.append(f"static LUCIDE_{upper}_SIXEL_COMPACT: LazyLock<String> = LazyLock::new(|| build_sixel_compact(LUCIDE_{upper}_COMPACT_RGBA));")
        lines.append(f'const LUCIDE_{upper}_STRIP_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_strip.rgba");')
        lines.append(f'const LUCIDE_{upper}_GALLERY_RGBA: &[u8] = include_bytes!("lucide_{spec.stem()}_gallery.rgba");')
        lines.append("")

    return "\n".join(lines) + "\n"


def to_screaming_snake(pascal: str) -> str:
    """`SourceControl` -> `SOURCE_CONTROL`."""
    out = re.sub(r"(?<!^)(?=[A-Z])", "_", pascal).upper()
    return out


def print_report(
    asset_sizes: dict[str, tuple[int, bool]],
    compact_asset_sizes: dict[str, tuple[int, bool]],
    strip_asset_sizes: dict[str, tuple[int, bool]],
    gallery_asset_sizes: dict[str, tuple[int, bool]],
    lucide_asset_sizes: dict[str, tuple[int, int, int, int, bool]],
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
    print("ASSET SIZE TOTALS (raw, TRUE-coverage sources only -- background")
    print("compositing is a Rust-side runtime decision now, nothing more to bake)")
    print("=" * 78)
    total_sixel = sum(s for s, _ in asset_sizes.values())
    newly_baked_sixel = sum(s for s, new in asset_sizes.values() if new)
    n_new = sum(1 for _, new in asset_sizes.values() if new)
    print(f"Icons total: {len(MANIFEST)}  (newly baked this run: {n_new})")
    print(f"Rail sixel tier:      {total_sixel:>9} bytes total ({total_sixel / 1024:.1f} KiB)  -- {newly_baked_sixel} bytes newly added")
    total_compact_sixel = total_strip = total_gallery = 0
    if compact_asset_sizes:
        total_compact_sixel = sum(s for s, _ in compact_asset_sizes.values())
        newly_baked_compact_sixel = sum(s for s, new in compact_asset_sizes.values() if new)
        print(f"Compact sixel tier:   {total_compact_sixel:>9} bytes total ({total_compact_sixel / 1024:.1f} KiB)  -- {newly_baked_compact_sixel} bytes newly added")
    if strip_asset_sizes:
        total_strip = sum(s for s, _ in strip_asset_sizes.values())
        newly_baked_strip = sum(s for s, new in strip_asset_sizes.values() if new)
        print(f"Strip sixel tier:     {total_strip:>9} bytes total ({total_strip / 1024:.1f} KiB)  -- {newly_baked_strip} bytes newly added")
    if gallery_asset_sizes:
        total_gallery = sum(s for s, _ in gallery_asset_sizes.values())
        newly_baked_gallery = sum(s for s, new in gallery_asset_sizes.values() if new)
        print(f"Gallery sixel tier:   {total_gallery:>9} bytes total ({total_gallery / 1024:.1f} KiB)  -- {newly_baked_gallery} bytes newly added")
    codicon_combined = total_sixel + total_compact_sixel + total_strip + total_gallery
    print(f"Combined (codicons):     {codicon_combined:>9} bytes total ({codicon_combined / 1024:.1f} KiB)")
    lucide_combined = 0
    if lucide_asset_sizes:
        total_lucide_rail = sum(rail for rail, _, _, _, _ in lucide_asset_sizes.values())
        total_lucide_compact = sum(compact for _, compact, _, _, _ in lucide_asset_sizes.values())
        total_lucide_strip = sum(strip for _, _, strip, _, _ in lucide_asset_sizes.values())
        total_lucide_gallery = sum(gallery for _, _, _, gallery, _ in lucide_asset_sizes.values())
        newly_baked_lucide = sum(
            rail + compact + strip + gallery
            for rail, compact, strip, gallery, new in lucide_asset_sizes.values()
            if new
        )
        lucide_combined = total_lucide_rail + total_lucide_compact + total_lucide_strip + total_lucide_gallery
        print(f"Lucide icons total: {len(lucide_asset_sizes)}  (of {len(MANIFEST)} in MANIFEST, {len(LUCIDE_GAPS)} reported gaps)")
        print(f"Lucide rail sixel tier:              {total_lucide_rail:>9} bytes total ({total_lucide_rail / 1024:.1f} KiB)")
        print(f"Lucide compact tier:                 {total_lucide_compact:>9} bytes total ({total_lucide_compact / 1024:.1f} KiB)")
        print(f"Lucide strip tier:                   {total_lucide_strip:>9} bytes total ({total_lucide_strip / 1024:.1f} KiB)")
        print(f"Lucide gallery tier:                 {total_lucide_gallery:>9} bytes total ({total_lucide_gallery / 1024:.1f} KiB)")
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
    require_resvg_version()
    check_lucide_coverage()
    args = parse_args(argv)
    only = set(s.strip() for s in args.only.split(",")) if args.only else None

    selected = [spec for spec in MANIFEST if only is None or spec.slug in only]
    if only is not None:
        missing = only - {spec.slug for spec in selected}
        if missing:
            die(f"--only names not in MANIFEST: {sorted(missing)}")

    asset_sizes: dict[str, tuple[int, bool]] = {}
    compact_asset_sizes: dict[str, tuple[int, bool]] = {}
    strip_asset_sizes: dict[str, tuple[int, bool]] = {}
    gallery_asset_sizes: dict[str, tuple[int, bool]] = {}
    for spec in selected:
        sixel_existed = (ICONS_DIR / f"{spec.stem()}.rgba").exists()
        sixel_path = ensure_assets(spec, args.cache_dir, args.force)
        asset_sizes[spec.slug] = (sixel_path.stat().st_size, args.force or not sixel_existed)

        compact_sixel_existed = (ICONS_DIR / f"{spec.stem()}_compact.rgba").exists()
        compact_sixel_path = ensure_compact_assets(spec, args.cache_dir, args.force)
        compact_asset_sizes[spec.slug] = (compact_sixel_path.stat().st_size, args.force or not compact_sixel_existed)

        strip_existed = (ICONS_DIR / f"{spec.stem()}_strip.rgba").exists()
        strip_path = ensure_strip_assets(spec, args.cache_dir, args.force)
        strip_asset_sizes[spec.slug] = (strip_path.stat().st_size, args.force or not strip_existed)

        gallery_existed = (ICONS_DIR / f"{spec.stem()}_gallery.rgba").exists()
        gallery_path = ensure_gallery_assets(spec, args.cache_dir, args.force)
        gallery_asset_sizes[spec.slug] = (gallery_path.stat().st_size, args.force or not gallery_existed)

    lucide_asset_sizes: dict[str, tuple[int, int, int, int, bool]] = {}
    for spec in selected:
        lucide_slug = LUCIDE_SLUGS.get(spec.rust_name)
        if lucide_slug is None:
            continue  # LUCIDE_GAPS -- no asset to bake, see that dict's own doc comment.
        sixel_existed = (ICONS_DIR / f"lucide_{spec.stem()}.rgba").exists()
        sixel_path = ensure_lucide_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        compact_existed = (ICONS_DIR / f"lucide_{spec.stem()}_compact.rgba").exists()
        compact_path = ensure_lucide_compact_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        strip_existed = (ICONS_DIR / f"lucide_{spec.stem()}_strip.rgba").exists()
        strip_path = ensure_lucide_strip_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        gallery_existed = (ICONS_DIR / f"lucide_{spec.stem()}_gallery.rgba").exists()
        gallery_path = ensure_lucide_gallery_assets(spec, lucide_slug, DEFAULT_LUCIDE_CACHE_DIR, args.force)

        lucide_asset_sizes[spec.rust_name] = (
            sixel_path.stat().st_size,
            compact_path.stat().st_size,
            strip_path.stat().st_size,
            gallery_path.stat().st_size,
            args.force or not (sixel_existed and compact_existed and strip_existed and gallery_existed),
        )

    if only is None:
        # Full manifest processed -- every icon has assets on disk, safe
        # to regenerate the complete catalog.
        CATALOG_RS.write_text(generate_catalog(), encoding="utf-8", newline="\n")
        print(f"wrote {CATALOG_RS} ({CATALOG_RS.stat().st_size} bytes)")
    else:
        print(f"--only restricted this run to {sorted(only)} -- catalog.rs NOT regenerated (needs the full manifest)")

    print_report(asset_sizes, compact_asset_sizes, strip_asset_sizes, gallery_asset_sizes, lucide_asset_sizes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
