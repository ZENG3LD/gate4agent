#!/usr/bin/env python3
"""Bake the gate4agent-tui icon catalog from microsoft/vscode-codicons.

Licence:  MIT (microsoft/vscode-codicons, <https://github.com/microsoft/
          vscode-codicons/blob/main/LICENSE>). Redistributing the baked
          RGBA raster derived from these SVGs under this crate's own
          licence is permitted by codicons' MIT terms; this header is the
          attribution.
Source:   raw.githubusercontent.com/microsoft/vscode-codicons/main/src/
          icons/<slug>.svg -- one file per `MANIFEST` entry below.
Re-run:   python tools/bake_icons.py            (from this crate's root,
                                                   "crates/gate4agent-tui")
          python tools/bake_icons.py --force     (re-bake every icon, even
                                                   ones already on disk)
          python tools/bake_icons.py --only add,close   (restrict to a
                                                   subset -- for threshold
                                                   iteration; does NOT
                                                   regenerate catalog.rs,
                                                   see below)

What this does, every run:
  1. For each `MANIFEST` entry, ensure its source SVG is present in
     `--cache-dir` (default `tools/.codicon-cache/`, gitignored -- a
     scratch mirror of upstream, not a source of truth): download once,
     reuse on every later run. A 404 aborts the ENTIRE run immediately
     with the offending slug and URL named in the error -- never silently
     skipped, never substituted automatically (see ICON LIST substitution
     policy below).
  2. Patch the SVG's `fill="currentColor"` (present exactly once, on the
     root `<svg>` element, for every codicon in this set -- verified
     against all 57 source files at authoring time) to `#cdd6f4`, this
     crate's own `pty_palette::GATE_FG` -- the same patch the original
     7-icon rail catalog already applied, so every tier reads as part of
     the existing theme.
  3. Rasterize BOTH tiers via `resvg` (SVG -> PNG) + `ffmpeg` (PNG ->
     padded/downsampled -> raw RGBA8) -- see `rasterize_sixel`/
     `rasterize_braille` below for the exact filter graphs, reproduced in
     each function's own doc comment so a human can re-run the equivalent
     `resvg`/`ffmpeg` CLI invocations by hand without reading Python.
     SKIPPED (idempotent, no network/subprocess work at all) for any icon
     whose both `.rgba` outputs already exist on disk with the expected
     byte length, unless `--force`. This is what keeps a normal re-run a
     no-op, and what keeps the original rail's 7 icons byte-for-byte
     untouched by this generalization (they were already baked before
     this tool existed; this tool's first run left them alone).
  4. Choose a per-icon braille alpha-coverage threshold: an explicit
     `MANIFEST` override if set (used for the original 7 rail icons, to
     replay their existing hand-tuned constants unchanged -- see
     `THRESHOLD_OVERRIDES`), else `choose_threshold`'s own coverage-
     histogram search (same ~10-58% band the original 7 were hand-tuned
     within, picked automatically per icon). This step is pure Python
     over already-baked bytes -- no network, no subprocess -- so re-
     tuning a threshold is instant: edit `THRESHOLD_OVERRIDES`, re-run.
  5. Regenerate `src/icons/catalog.rs` from the FULL manifest (only when
     not restricted by `--only`) -- one `IconId` enum variant, one sixel
     `LazyLock<String>`, one braille `LazyLock<PixelCanvas>`, one ascii
     literal, per icon.
  6. Print a report: per-icon coverage stats + a plain-text braille
     preview (the exact dot pattern `PixelCanvas::flush` will render),
     grouped the same way the task's own icon list was grouped, plus
     asset-size totals.

Nothing here is a build-time Cargo dependency -- `resvg`/`ffmpeg` run
once, offline, from a developer's own PATH, producing checked-in
`.rgba` files `include_bytes!`'d at compile time (see `src/icons.rs`).

## Quality pass (dirty edges / blurred strokes / control-strip resize)

Two defects diagnosed against the running TUI, both verified against this
tool's own pipeline (not taken on faith) before fixing:

1. DIRTY EDGES -- CONFIRMED, root cause identified precisely. Every sixel
   asset was baked with a transparent background and encoded via
   `icy_sixel::BackgroundMode::Transparent`. `icy_sixel` 0.6's own encoder
   (`encoder.rs::sixel_encode_impl`) applies a HARD alpha>=128 opacity
   threshold per pixel -- there is no partial-coverage/blend information
   in the encoded SIXEL stream at all, only "fully drawn, at this pixel's
   flat ink colour" or "fully undrawn". Windows Terminal's own sixel
   decoder does not implement the "undrawn -> show whatever is already
   there" transparency semantics DEC's P2=1 mode specifies, so "undrawn"
   pixels do not read as transparent in practice. FIX: composite every
   icon over the EXACT background colour the button paints (`GATE_ACTIVE_
   BG_RGB`/`GATE_ACCENT_BG_RGB` below, hand-synced to `render.rs`'s own
   `ACTIVE_BG`/`MAUVE`), fully opaque, so the encoder's threshold and the
   terminal's transparency support both become irrelevant -- there is no
   transparent pixel left to mishandle. This is done as a SEPARATE, pure-
   Python compositing pass (`composite_over_background`) over an already-
   rasterized buffer -- never a second resvg/ffmpeg call -- so it can
   never regress into cause 2's own double-resampling anti-pattern. Only
   possible for `PtyColorMode::GateOverride`, whose panel/rail colours are
   fixed, known constants; `PtyColorMode::Inherited` has no knowable exact
   background (crossterm has no reliable query for the terminal's own
   background colour, the same gap `icons.rs::ASSUMED_CELL_WIDTH_PX`'s own
   doc comment already names for cell-pixel size) and keeps the original
   transparent-encoded asset as its only available option. The rail tier
   has two backgrounds (`theme.active` at rest, `theme.accent` selected)
   so it gets two composited variants (`SixelVariant::GateActive`/
   `GateAccent`); the new strip tier below has exactly one (control-strip
   buttons never show a selected state), so it gets one.

2. BLURRED STROKES -- diagnosed as "rasterized on a non-integer scale from
   a 24-unit source grid"; PARTIALLY CONFIRMED, PARTIALLY REFUTED once
   checked against the actual 57-icon manifest and the actual pipeline:
   - The "24-unit grid" premise is WRONG for most of this set: 52/57
     source SVGs use a 16x16 viewBox, only 4 use 24x24 (`files`,
     `settings-gear`, `source-control`, `terminal`) and 1 uses 24x25
     (`output`, already a documented exception elsewhere in this file).
   - The "second ffmpeg downscale" claim is REFUTED for every SIXEL tier:
     `rasterize_sixel`/`rasterize_compact_sixel` each do exactly ONE
     resvg pass, directly at the target pixel size, followed only by a
     SAME-SIZE ffmpeg pad (verified at authoring time: a fresh direct
     `resvg -w 40 -h 40`+identity-pad rebake is byte-for-byte identical to
     the checked-in `check.rgba`). The braille tiers DO supersample-then-
     area-downscale, but that is a deliberate, necessary coverage
     computation for per-dot alpha thresholding, not a source of visible
     blur (braille's own final output is a binary lit/unlit dot pattern
     after `choose_threshold`, which a soft source edge does not survive
     unchanged either way).
   - The underlying mechanism IS real, though: rasterizing a straight,
     axis-aligned 1-source-unit stroke at a size that is not an integer
     multiple of its own source grid measurably softens it. Verified
     directly: `add.svg`'s (16-unit grid) plus-sign bar, rasterized via a
     single direct resvg pass, comes out as a crisp `[255]`/`[255, 255]`/
     `[255, 255, 255]` alpha run at 16/32/48px (exact multiples of 16) but
     a soft `[128, 255, 255]` run (one half-opacity fringe pixel) at the
     rail's own current 40px (40/16 = 2.5x, non-integer).
   FIX applied given the above: the new strip tier below (~20x20px,
   forced by the required 2-cell x 1-row button footprint, itself not an
   integer multiple of either 16 or 24) uses a single resvg AA pass
   DIRECTLY at that target size -- the third option this task's own brief
   named ("the target size with resvg's own high-quality AA applied
   ONCE"), and the only one available without either breaking the
   required button footprint or reintroducing a second resampling pass.
   The pre-existing rail (40x40) and compact (10x20) tiers keep their
   current pixel sizes unchanged -- neither's geometry is in this task's
   scope (rail geometry is explicitly frozen; compact tier is untouched
   panel content) -- so their modest, now-measured softening on axis-
   aligned strokes is a real but small, pre-existing, out-of-scope
   residual; cause 1's fix (the dominant defect by far) still applies to
   the rail tier's own pixel CONTENT (not its size) via the composited
   `GateActive`/`GateAccent` variants above.

Also: every sixel encode (all tiers, all variants) now goes through
`icons.rs::icon_encode_options()` -- `max_colors: 32` (was the library's
own default of 256) and `diffusion: 0.0` (was Floyd-Steinberg dithering
on by default) -- "encode with a small explicit palette" per this task's
own brief. A composited icon's true colour count is just its own number
of distinct alpha/coverage levels (measured on real baked assets at
authoring time: 6-26 across this manifest), so 32 is generous headroom,
not a visible compression; dithering exists to fake extra apparent colours
via spatial noise for photographic content and only ever adds speckle
noise to a flat-colour UI glyph like these, so it is switched off outright
rather than tuned down.
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

CODICON_URL_TEMPLATE = "https://raw.githubusercontent.com/microsoft/vscode-codicons/main/src/icons/{slug}.svg"
FILL_SOURCE = 'fill="currentColor"'
FILL_TARGET = 'fill="#cdd6f4"'  # this crate's pty_palette::GATE_FG

SIXEL_PX = 40
BRAILLE_SUPER_PX = 32  # supersampled fit-within square before padding
BRAILLE_PAD_H = 48  # supersampled canvas height (2:3 aspect, matches an 8x12 dot grid)
BRAILLE_FINAL_W = 8
BRAILLE_FINAL_H = 12
BRAILLE_TOTAL_DOTS = BRAILLE_FINAL_W * BRAILLE_FINAL_H

SIXEL_RGBA_LEN = SIXEL_PX * SIXEL_PX * 4
BRAILLE_RGBA_LEN = BRAILLE_FINAL_W * BRAILLE_FINAL_H * 4

# ---- Compact tier (dense single-row inline buttons -- Explorer/Git
# panel labelling, wave 1; see `icons.rs`'s own module doc for the tier's
# reasoning). Sixel: exactly ONE assumed terminal cell
# (`icons::ASSUMED_CELL_WIDTH_PX`/`_HEIGHT_PX` -- keep these two numbers
# in sync with that Rust module by hand, the same relationship `SIXEL_PX`
# above already has to the rail's own cell-footprint constants). Braille:
# 2 cells wide x 1 row tall (a braille cell is a fixed 2x4 dot grid, so
# this is a 4x4 dot canvas) -- a SQUARE target, unlike the rail tier's
# own 2:3 portrait (8x12) grid.
COMPACT_SIXEL_PX_W = 10
COMPACT_SIXEL_PX_H = 20
COMPACT_BRAILLE_SUPER_PX = 32  # supersampled fit-within square before downsampling
COMPACT_BRAILLE_FINAL_W = 4
COMPACT_BRAILLE_FINAL_H = 4
COMPACT_BRAILLE_TOTAL_DOTS = COMPACT_BRAILLE_FINAL_W * COMPACT_BRAILLE_FINAL_H

COMPACT_SIXEL_RGBA_LEN = COMPACT_SIXEL_PX_W * COMPACT_SIXEL_PX_H * 4
COMPACT_BRAILLE_RGBA_LEN = COMPACT_BRAILLE_FINAL_W * COMPACT_BRAILLE_FINAL_H * 4

# ---- Strip tier (sidebar content panels' own control-plane strip -- see
# `render::render_control_strip`/`render_control_strip_button`) -- 2 cells
# wide x 1 row tall, exactly `icons::STRIP_SIXEL_ICON_WIDTH_PX`/`_HEIGHT_
# PX` (keep these two numbers in sync with that Rust module by hand, same
# precedent as `COMPACT_SIXEL_PX_W`/`_H` above). SIXEL only -- the strip's
# own braille tier reuses the COMPACT braille assets/constants above
# unchanged (that tier's own 4x4-dot / 2-cell-wide-x-1-row footprint
# already matches this tier's required geometry exactly), so there is no
# separate `STRIP_BRAILLE_*` bake.
STRIP_SIXEL_PX_W = 20
STRIP_SIXEL_PX_H = 20
STRIP_SIXEL_RGBA_LEN = STRIP_SIXEL_PX_W * STRIP_SIXEL_PX_H * 4

# ---- Gallery tier (the icon gallery dev surface -- FIX4's own main
# deliverable, `app::SurfaceTab::IconGallery` / `render::
# render_icon_gallery`) -- the third of FIX2's three evenly-landing square
# sizes (20/40/60 on the assumed 10x20px cell grid). 20 and 40 already
# exist (the strip and rail tiers above, reused as-is by the gallery); 60
# has no other UI consumer and so gets its own dedicated bake here, same
# recipe as the strip tier (`STRIP_SIXEL_PX_W`/`_H` above): a single resvg
# AA pass directly at the target size, exactly `icons::
# GALLERY_SIXEL_ICON_WIDTH_PX`/`_HEIGHT_PX` (keep these two numbers in
# sync with that Rust module by hand, same precedent as
# `STRIP_SIXEL_PX_W`/`_H`).
GALLERY_SIXEL_PX_W = 60
GALLERY_SIXEL_PX_H = 60
GALLERY_SIXEL_RGBA_LEN = GALLERY_SIXEL_PX_W * GALLERY_SIXEL_PX_H * 4

# ---- GateOverride compositing (cause 1's fix -- see this module's own
# header doc comment). Hand-synced to render.rs's own fixed theme
# constants: `ACTIVE_BG` (the rail/strip button body's own "at rest"
# colour) and `MAUVE` (`theme.accent`, the rail's own "selected" colour).
# `PtyColorMode::GateOverride` only -- see `icons::SixelVariant`'s own doc
# comment for why `PtyColorMode::Inherited` has no equivalent.
GATE_ACTIVE_BG_RGB = (30, 30, 46)  # render.rs::ACTIVE_BG
GATE_ACCENT_BG_RGB = (203, 166, 247)  # render.rs::MAUVE / theme.accent

# Proportional to `MIN_LIT_DOTS_TARGET`'s own 18/96 (~19%) share of the
# rail tier's 96 total dots, applied to the compact braille tier's own
# 16 -- scaled down, not re-derived, so a canvas this much smaller does
# not just always fall through to the ladder's noisiest (and, per this
# tier's own review, LEAST informative -- see `COMPACT_THRESHOLD_
# OVERRIDES` below) rung.
COMPACT_MIN_LIT_DOTS_TARGET = 3

# Threshold candidates (alpha 0-255), descending, ~6%-58% coverage -- the
# same band the original 7-icon catalog's own hand-tuned constants (77,
# 89, 102) sit within, extended both up and down for the wider variety of
# stroke weights in the full 57-icon set.
THRESHOLD_LADDER = [148, 140, 128, 115, 102, 89, 77, 64, 51, 38, 26, 15]
# Calibrated against the original 7 baked assets at their existing
# thresholds: lit-dot counts there ranged 12 (the two chevrons, thin
# strokes, a documented floor) to 31 (files); the non-chevron 5 alone
# ranged 17-31. 18 sits just under that non-thin-stroke floor -- the
# auto-search prefers the HIGHEST (cleanest) threshold that still clears
# it, only dropping lower when nothing in the ladder does. An earlier,
# lower target (10) was tried and rejected during authoring: it let the
# search stop at the FIRST (highest, sparsest) ladder rung clearing a
# too-low bar, which for several icons (e.g. `new-folder`) meant stopping
# at a lopsided partial silhouette (right half only) instead of
# continuing down to a rung where the full shape actually appears.
MIN_LIT_DOTS_TARGET = 18
# Below this many lit dots even at the chosen threshold, flag the icon
# DEGRADED in the report (a dot-count floor; genuine shape-fidelity loss
# at moderate dot counts, e.g. the gear's teeth, is a separate qualitative
# call recorded directly in `KNOWN_DEGRADED` below, not derivable from a
# dot count alone -- the report's DEGRADED flag is the union of both).
DEGRADED_LIT_DOTS_FLOOR = 10


@dataclass(frozen=True)
class IconSpec:
    rust_name: str  # PascalCase IconId variant
    slug: str  # codicon file stem, e.g. "source-control"
    ascii: str  # 1-2 char ASCII label
    file_stem: str = field(default="")  # snake_case asset stem; derived if empty

    def stem(self) -> str:
        return self.file_stem or self.slug.replace("-", "_")


# The 7 icons the activity rail already used before this task (baseline
# commit 9f39758) -- their thresholds are REPLAYED unchanged (not
# re-derived) so re-pointing the rail at this catalog is a pure rename,
# not a re-bake. See `src/icons.rs`'s own module doc for why each of
# these 7 numbers was originally chosen.
THRESHOLD_OVERRIDES: dict[str, int] = {
    "files": 102,
    "source-control": 102,
    "person": 89,
    "project": 102,
    "settings-gear": 89,
    "chevron-left": 77,
    "chevron-right": 77,
    # New-icon overrides below were picked by hand after reviewing this
    # tool's own auto-suggested preview -- the auto search's own band
    # target (see `MIN_LIT_DOTS_TARGET`) still occasionally lands on a
    # rung that reads worse than a neighboring one; these override it.
    #
    # ellipsis: the 3 dots merge into ONE solid coverage band regardless
    # of threshold (verified: every alpha value in this icon's braille
    # buffer is 0, 48, or 49 -- there is no gradient to pick a threshold
    # WITHIN), so the auto search's own MIN_LIT_DOTS_TARGET=18 is
    # unreachable here (max achievable is 12) and it would fall through
    # to the "lowest rung with >=1 dot" tier -- an unnecessarily low
    # (~6%) threshold for a shape that doesn't gain any detail from it.
    # 38 (~15%) lights the same 12 dots as every rung down to 15 does.
    "ellipsis": 38,
    # checklist / split-horizontal / split-vertical: each reads as a
    # cleaner, more complete silhouette (a boxed list; two panels with a
    # visible divider) at 64 than at the higher rung the auto band would
    # otherwise stop on.
    "checklist": 64,
    "split-horizontal": 64,
    "split-vertical": 64,
    # link: the icon collapses to a single horizontal dot-row at every
    # threshold (a 16-wide glyph inset well within a 2:3 target box), so
    # -- same shape family as `ellipsis` above -- the auto band's own
    # target is unreachable and it would fall through to the noisiest
    # available rung (15, ~6%); 64 keeps the row legible without the
    # extra noise the lowest rungs add for no extra shape information.
    "link": 64,
    # close: a wide STABLE plateau (12 lit dots, a clean X) holds from
    # 140 all the way down to 26; only the single lowest rung (15) is
    # qualitatively different -- both center cells jump to fully solid
    # (all 8 sub-dots lit), collapsing the X into two solid blocks. The
    # auto search's own target is unreachable above that one rung, so
    # without this override it would land on the worst (blob) option
    # instead of the wide clean plateau one step above it.
    "close": 77,
    # eye: a stable plateau (12 dots, a recognizable curved lens/almond
    # outline) holds from 89 down to 64; the lowest rungs (26, 15) fill
    # the same two cells fully solid, losing the lens curve entirely --
    # same "auto target unreachable, lands on the one blob rung" issue
    # as `close` above.
    "eye": 77,
}

# The 7 slugs replayed unchanged from the pre-existing rail catalog (the
# first 7 keys of `THRESHOLD_OVERRIDES` above) -- split out so
# `generate_catalog`'s per-icon doc comment can say "replaying the
# original rail icon" ONLY for these 7, not for every other icon that
# also happens to carry a manual override.
LEGACY_RAIL_SLUGS: frozenset[str] = frozenset({"files", "source-control", "person", "project", "settings-gear", "chevron-left", "chevron-right"})

# Same idea as `THRESHOLD_OVERRIDES` above, but for the compact braille
# tier's own much smaller 4x4 (16-dot) canvas, where the auto search's
# own `COMPACT_MIN_LIT_DOTS_TARGET` is low enough that the ladder's
# highest-clearing rung is sometimes a lopsided/sparse 1-cell-only result
# rather than a rung one or two steps lower that actually uses both
# cells -- picked by hand after reviewing this tool's own printed report.
COMPACT_THRESHOLD_OVERRIDES: dict[str, int] = {
    # new-file / new-folder: both auto-select the SAME highest ladder rung
    # (148) here, landing on the SAME near-blank glyph (only the right
    # cell lit, 4/16 dots) for two icons that sit as ADJACENT buttons in
    # the Explorer panel -- indistinguishable from each other at the
    # compact size, not just individually sparse. 64 gives each its own
    # distinct, more fully-lit (10/16) shape.
    "new-file": 64,
    "new-folder": 64,
    # arrow-up / check: the auto search's own target (3 lit dots) stops
    # at the first rung clearing it, which for these two thin-stroke
    # glyphs is a very sparse 3-dot result; one rung lower still reads as
    # the same basic shape (arrow stem+head / check tick) with a fuller
    # silhouette.
    "arrow-up": 38,
    "check": 26,
}

# Icons where even the best-achievable threshold loses real shape detail
# (not just a low dot count) -- a qualitative call made by reviewing every
# icon's own preview, mirroring how the original SETTINGS_GEAR_BRAILLE_
# ALPHA_THRESHOLD doc comment already called out its own "hollow-centered
# blob, not a crisp multi-tooth gear" ceiling. Printed in the report;
# these icons still ship (per the task's own quality bar), just flagged.
KNOWN_DEGRADED: dict[str, str] = {
    "settings-gear": "teeth merge into a round, hollow-centered blob -- an honest 8x12 resolution ceiling, not a threshold bug (pre-existing, documented in src/icons.rs).",
    "ellipsis": "the 3 dots merge into ONE solid horizontal band (verified: every alpha value in the baked buffer is 0, 48, or 49 -- no threshold separates them); reads as a short dash/bar, not 3 distinct dots.",
    "checklist": "the checkmarks-in-a-list fine strokes read as a boxed/ruled texture, not legible individual ticks.",
    "layout": "3 separate rounded-rect panels within one 16x16 viewBox collapse into a repeating texture; reads as hatching, not a distinguishable 2-or-3-panel grid.",
    "link": "the two interlocking chain ovals collapse into a single horizontal dot-row at every threshold; the 'two links' meaning does not survive, though the row itself is not noise.",
    "pulse": "the heartbeat zig-zag's short segments partially merge; a general spike/wave shape survives, the fine zig-zag detail does not.",
}

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
    IconSpec("NewFile", "new-file", "N+"),
    IconSpec("NewFolder", "new-folder", "Nd"),
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

# Report grouping -- mirrors the task's own line-grouping of the icon
# list verbatim, purely for the printed report's readability.
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


def rasterize_sixel(patched_svg: Path, out_rgba: Path) -> None:
    """Sixel tier: fit-within 40x40 (resvg fits to the smaller of the two
    requested dimensions, preserving aspect -- for a square source this
    already lands on exactly 40x40; codicons are square with exactly one
    documented exception, `output.svg`'s 24x25 viewBox, so this always
    force-pads to an exact 40x40 canvas rather than assuming squareness).

    Equivalent hand-run commands:
        resvg -w 40 -h 40 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=40:40:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>.rgba
    """
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        run_tool(["resvg", "-w", str(SIXEL_PX), "-h", str(SIXEL_PX), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={SIXEL_PX}:{SIXEL_PX}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(out_rgba),
        ])
    actual = out_rgba.stat().st_size
    if actual != SIXEL_RGBA_LEN:
        die(f"sixel raster for {patched_svg} produced {actual} bytes, expected {SIXEL_RGBA_LEN}")


def rasterize_braille(patched_svg: Path, out_rgba: Path) -> None:
    """Braille tier: supersample fit-within 32x32, pad onto a 32x48
    (2:3 portrait, matching an 8x12 dot grid's own aspect) transparent
    canvas, then AREA-downsample (ffmpeg's box filter -- for this exact
    4x integer factor, a true per-block average, i.e. real coverage, not
    nearest/bilinear) to the final 8x12. resvg's alpha channel is already
    a straight (non-premultiplied) coverage weight, so the downsampled
    alpha directly IS each final dot's coverage fraction -- no separate
    coverage computation happens here; `choose_threshold` reads it
    directly, same as `icons::rgba_to_canvas` does at Rust runtime.

    Equivalent hand-run commands:
        resvg -w 32 -h 32 <slug>.patched.svg <slug>_raw32.png
        ffmpeg -i <slug>_raw32.png -vf "\\
            pad=32:32:(ow-iw)/2:(oh-ih)/2:color=black@0.0,\\
            pad=32:48:0:8:color=black@0.0,\\
            scale=8:12:flags=area" \\
            -f rawvideo -pix_fmt rgba <slug>_braille.rgba
    """
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw32.png"
        run_tool(["resvg", "-w", str(BRAILLE_SUPER_PX), "-h", str(BRAILLE_SUPER_PX), str(patched_svg), str(raw_png)])
        vf = (
            f"pad={BRAILLE_SUPER_PX}:{BRAILLE_SUPER_PX}:(ow-iw)/2:(oh-ih)/2:color=black@0.0,"
            f"pad={BRAILLE_SUPER_PX}:{BRAILLE_PAD_H}:0:8:color=black@0.0,"
            f"scale={BRAILLE_FINAL_W}:{BRAILLE_FINAL_H}:flags=area"
        )
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", vf,
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(out_rgba),
        ])
    actual = out_rgba.stat().st_size
    if actual != BRAILLE_RGBA_LEN:
        die(f"braille raster for {patched_svg} produced {actual} bytes, expected {BRAILLE_RGBA_LEN}")


def rasterize_compact_sixel(patched_svg: Path, out_rgba: Path) -> None:
    """Compact sixel tier: fit-within a `COMPACT_SIXEL_PX_W`-wide box (the
    smaller of the two target dimensions constrains a square source, same
    fit-within behaviour `rasterize_sixel` documents for the rail tier),
    then pad onto the full `COMPACT_SIXEL_PX_W` x `_H` canvas -- exactly
    one assumed terminal cell. For dense single-row buttons (Explorer/Git
    sidebar lists and their modals) where the rail's own 40x40 icon does
    not fit.

    Equivalent hand-run commands:
        resvg -w 10 -h 20 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=10:20:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>_compact.rgba
    """
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        run_tool(["resvg", "-w", str(COMPACT_SIXEL_PX_W), "-h", str(COMPACT_SIXEL_PX_H), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={COMPACT_SIXEL_PX_W}:{COMPACT_SIXEL_PX_H}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(out_rgba),
        ])
    actual = out_rgba.stat().st_size
    if actual != COMPACT_SIXEL_RGBA_LEN:
        die(f"compact sixel raster for {patched_svg} produced {actual} bytes, expected {COMPACT_SIXEL_RGBA_LEN}")


def rasterize_compact_braille(patched_svg: Path, out_rgba: Path) -> None:
    """Compact braille tier: supersample fit-within a SQUARE 32x32 box
    (no rectangular pad, unlike the rail tier's own 2:3 portrait target --
    the compact grid is itself square: 2 cells wide x 1 row tall = 4x4
    dots), then AREA-downsample straight to the final 4x4.

    Equivalent hand-run commands:
        resvg -w 32 -h 32 <slug>.patched.svg <slug>_raw32.png
        ffmpeg -i <slug>_raw32.png -vf "\\
            pad=32:32:(ow-iw)/2:(oh-ih)/2:color=black@0.0,\\
            scale=4:4:flags=area" \\
            -f rawvideo -pix_fmt rgba <slug>_compact_braille.rgba
    """
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw32.png"
        run_tool(["resvg", "-w", str(COMPACT_BRAILLE_SUPER_PX), "-h", str(COMPACT_BRAILLE_SUPER_PX), str(patched_svg), str(raw_png)])
        vf = (
            f"pad={COMPACT_BRAILLE_SUPER_PX}:{COMPACT_BRAILLE_SUPER_PX}:(ow-iw)/2:(oh-ih)/2:color=black@0.0,"
            f"scale={COMPACT_BRAILLE_FINAL_W}:{COMPACT_BRAILLE_FINAL_H}:flags=area"
        )
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", vf,
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(out_rgba),
        ])
    actual = out_rgba.stat().st_size
    if actual != COMPACT_BRAILLE_RGBA_LEN:
        die(f"compact braille raster for {patched_svg} produced {actual} bytes, expected {COMPACT_BRAILLE_RGBA_LEN}")


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


def rasterize_strip_sixel(patched_svg: Path, out_rgba: Path) -> None:
    """Control-plane strip tier: fit-within `STRIP_SIXEL_PX_W`x`_H` (2
    cells wide x 1 row tall, exactly the button's own full body -- unlike
    the rail tier's own square crop of a taller body, see `../icons.rs`'s
    own `SIXEL_ICON_WIDTH_PX` doc comment). Single resvg pass DIRECTLY at
    the target size -- the same "rasterize once, at the target size, with
    resvg's own high-quality AA" recipe `rasterize_sixel`/`rasterize_
    compact_sixel` already use (see this module's own header doc comment's
    cause-2 note: there is no larger intermediate render and no second
    ffmpeg scale here, only a same-size pad) -- except the exact fit-within
    pixel size handed to resvg is precomputed by hand (`svg_intrinsic_
    size`/`fit_within`) rather than resvg's own `-w`/`-h`, to sidestep a
    rounding edge case at this tier's own small absolute size (see `svg_
    intrinsic_size`'s own doc comment).

    Equivalent hand-run commands (for a square 16x16/24x24 source; a non-
    square source like `output.svg` fits within 20x20 first, see above):
        resvg -w 20 -h 20 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=20:20:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>_strip.rgba
    """
    fit_w, fit_h = fit_within(*svg_intrinsic_size(patched_svg), STRIP_SIXEL_PX_W, STRIP_SIXEL_PX_H)
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        run_tool(["resvg", "-w", str(fit_w), "-h", str(fit_h), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={STRIP_SIXEL_PX_W}:{STRIP_SIXEL_PX_H}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(out_rgba),
        ])
    actual = out_rgba.stat().st_size
    if actual != STRIP_SIXEL_RGBA_LEN:
        die(f"strip sixel raster for {patched_svg} produced {actual} bytes, expected {STRIP_SIXEL_RGBA_LEN}")


def rasterize_gallery_sixel(patched_svg: Path, out_rgba: Path) -> None:
    """Gallery tier: fit-within `GALLERY_SIXEL_PX_W`x`_H` (6 cells wide x
    3 rows tall -- the icon gallery's own 60x60 comparison column, see
    `icons::GALLERY_SIXEL_ICON_WIDTH_PX`'s own doc comment). Single resvg
    pass DIRECTLY at the target size -- the exact same recipe `rasterize_
    strip_sixel` already uses (see this module's own header doc comment's
    cause-2 note), including the same precomputed fit-within pixel size
    (`svg_intrinsic_size`/`fit_within`) rather than resvg's own `-w`/`-h`.

    Equivalent hand-run commands (for a square 16x16/24x24 source; a non-
    square source like `output.svg` fits within 60x60 first, see above):
        resvg -w 60 -h 60 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=60:60:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>_gallery.rgba
    """
    fit_w, fit_h = fit_within(*svg_intrinsic_size(patched_svg), GALLERY_SIXEL_PX_W, GALLERY_SIXEL_PX_H)
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        run_tool(["resvg", "-w", str(fit_w), "-h", str(fit_h), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={GALLERY_SIXEL_PX_W}:{GALLERY_SIXEL_PX_H}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(out_rgba),
        ])
    actual = out_rgba.stat().st_size
    if actual != GALLERY_SIXEL_RGBA_LEN:
        die(f"gallery sixel raster for {patched_svg} produced {actual} bytes, expected {GALLERY_SIXEL_RGBA_LEN}")


def composite_over_background(rgba: bytes, bg: tuple[int, int, int]) -> bytes:
    """Cause 1's actual fix: alpha-composite a straight (non-premultiplied)
    RGBA buffer -- resvg's own convention, where a partially-covered "ink"
    pixel's RGB channels stay at the flat fill colour regardless of alpha,
    verified in `../icons.rs`'s own module doc -- over a flat, fully
    opaque `bg` colour, per pixel: `out = ink * (a/255) + bg * (1 - a/255)`,
    `out_alpha = 255` throughout. Pure arithmetic over an ALREADY-
    rasterized buffer -- same width/height in and out, no interpolation --
    so this can never become a second resampling pass (see this module's
    own header doc comment on why that distinction matters for cause 2).
    The icon's own ink colour is already `#cdd6f4` from `patch_fill`
    above, so no separate "tint to the theme foreground" step is needed
    here -- the source is already the right colour, this only decides what
    shows through where it is not fully opaque."""
    bg_r, bg_g, bg_b = bg
    out = bytearray(len(rgba))
    for i in range(0, len(rgba), 4):
        r, g, b, a = rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]
        inv = 255 - a
        out[i] = (r * a + bg_r * inv + 127) // 255
        out[i + 1] = (g * a + bg_g * inv + 127) // 255
        out[i + 2] = (b * a + bg_b * inv + 127) // 255
        out[i + 3] = 255
    return bytes(out)


def ensure_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path, Path, Path]:
    """Ensure every rail-tier `.rgba` output for `spec` exists on disk,
    baking whatever is missing (or everything, if `force`). Returns
    (sixel, braille, gate_active, gate_accent) paths. This is the
    idempotency boundary: a normal re-run with nothing new to bake touches
    no network and spawns no subprocess at all -- the two GateOverride
    variants are pure Python derived from the already-baked transparent
    sixel bytes, so they cost nothing extra even on a cold cache."""
    sixel_path = ICONS_DIR / f"{spec.stem()}.rgba"
    braille_path = ICONS_DIR / f"{spec.stem()}_braille.rgba"

    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != SIXEL_RGBA_LEN
    need_braille = force or not braille_path.exists() or braille_path.stat().st_size != BRAILLE_RGBA_LEN

    if need_sixel or need_braille:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        if need_sixel:
            rasterize_sixel(patched, sixel_path)
        if need_braille:
            rasterize_braille(patched, braille_path)

    active_path, accent_path = ensure_gate_variants(sixel_path, force)

    return sixel_path, braille_path, active_path, accent_path


def ensure_gate_variants(sixel_path: Path, force: bool) -> tuple[Path, Path]:
    """Derive `<stem>_gate_active.rgba` / `<stem>_gate_accent.rgba` --
    cause 1's fix for the rail tier: the SAME 40x40 pixels `sixel_path`
    already has, alpha-composited (`composite_over_background`, pure
    Python, no subprocess) over `GATE_ACTIVE_BG_RGB`/`GATE_ACCENT_BG_RGB`.
    `PtyColorMode::GateOverride`'s own two fixed rail button backgrounds
    (at rest / selected) are known exactly at bake time, so there is no
    reason to ship a transparent image and hope the terminal blends it --
    see this module's own header doc comment."""
    stem = sixel_path.name.removesuffix(".rgba")
    active_path = ICONS_DIR / f"{stem}_gate_active.rgba"
    accent_path = ICONS_DIR / f"{stem}_gate_accent.rgba"
    need_active = force or not active_path.exists() or active_path.stat().st_size != SIXEL_RGBA_LEN
    need_accent = force or not accent_path.exists() or accent_path.stat().st_size != SIXEL_RGBA_LEN
    if need_active or need_accent:
        source = sixel_path.read_bytes()
        if need_active:
            active_path.write_bytes(composite_over_background(source, GATE_ACTIVE_BG_RGB))
        if need_accent:
            accent_path.write_bytes(composite_over_background(source, GATE_ACCENT_BG_RGB))
    return active_path, accent_path


def ensure_strip_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Ensure the control-plane strip's own two sixel-tier outputs:
    `<stem>_strip.rgba` (transparent, single-pass resvg raster directly at
    the strip's own 20x20 target -- see `rasterize_strip_sixel`) and
    `<stem>_strip_gate.rgba` (the same pixels alpha-composited over
    `GATE_ACTIVE_BG_RGB`, pure Python, no second raster pass -- see
    `composite_over_background`). The strip never shows a `selected`
    state (see `render::render_control_strip_button`'s own doc comment),
    so there is no `_strip_gate_accent` variant."""
    transparent_path = ICONS_DIR / f"{spec.stem()}_strip.rgba"
    gate_path = ICONS_DIR / f"{spec.stem()}_strip_gate.rgba"
    need_transparent = force or not transparent_path.exists() or transparent_path.stat().st_size != STRIP_SIXEL_RGBA_LEN
    need_gate = force or not gate_path.exists() or gate_path.stat().st_size != STRIP_SIXEL_RGBA_LEN

    if need_transparent:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        rasterize_strip_sixel(patched, transparent_path)
    if need_gate:
        # Always re-read from disk rather than threading a maybe-stale
        # in-memory copy through: correct whether or not `need_transparent`
        # was also true this run.
        gate_path.write_bytes(composite_over_background(transparent_path.read_bytes(), GATE_ACTIVE_BG_RGB))

    return transparent_path, gate_path


def ensure_gallery_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Ensure the icon gallery's own two 60x60 sixel-tier outputs:
    `<stem>_gallery.rgba` (transparent, single-pass resvg raster directly
    at the target size -- see `rasterize_gallery_sixel`) and `<stem>_
    gallery_gate.rgba` (the same pixels alpha-composited over
    `GATE_ACTIVE_BG_RGB`, pure Python, no second raster pass -- see
    `composite_over_background`). Same "no selected state, so no
    `_gallery_gate_accent` variant" precedent as `ensure_strip_assets`
    above -- the gallery is a read-only comparison grid."""
    transparent_path = ICONS_DIR / f"{spec.stem()}_gallery.rgba"
    gate_path = ICONS_DIR / f"{spec.stem()}_gallery_gate.rgba"
    need_transparent = force or not transparent_path.exists() or transparent_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN
    need_gate = force or not gate_path.exists() or gate_path.stat().st_size != GALLERY_SIXEL_RGBA_LEN

    if need_transparent:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        rasterize_gallery_sixel(patched, transparent_path)
    if need_gate:
        gate_path.write_bytes(composite_over_background(transparent_path.read_bytes(), GATE_ACTIVE_BG_RGB))

    return transparent_path, gate_path


def ensure_compact_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Same idempotency contract as `ensure_assets` above, for the
    compact tier's own two `.rgba` outputs."""
    sixel_path = ICONS_DIR / f"{spec.stem()}_compact.rgba"
    braille_path = ICONS_DIR / f"{spec.stem()}_compact_braille.rgba"

    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != COMPACT_SIXEL_RGBA_LEN
    need_braille = force or not braille_path.exists() or braille_path.stat().st_size != COMPACT_BRAILLE_RGBA_LEN

    if need_sixel or need_braille:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        if need_sixel:
            rasterize_compact_sixel(patched, sixel_path)
        if need_braille:
            rasterize_compact_braille(patched, braille_path)

    return sixel_path, braille_path


def braille_bit(local_x: int, local_y: int) -> int:
    """Standard 8-dot braille numbering -- MUST match
    `uzor_tui::canvas::braille_bit` bit-for-bit (dots 1/2/3/7 down the
    left column, 4/5/6/8 down the right column); this is what makes the
    preview printed below the EXACT glyphs `PixelCanvas::flush` renders,
    not an approximation."""
    return {
        (0, 0): 0x01, (0, 1): 0x02, (0, 2): 0x04, (0, 3): 0x40,
        (1, 0): 0x08, (1, 1): 0x10, (1, 2): 0x20, (1, 3): 0x80,
    }[(local_x, local_y)]


def braille_rows(rgba: bytes, threshold: int) -> list[str]:
    """3 rows of 4 braille glyphs each -- the exact 4-cell x 3-row body a
    rail/catalog button paints in braille mode, at `threshold`."""
    lit = [[rgba[(y * BRAILLE_FINAL_W + x) * 4 + 3] >= threshold for x in range(BRAILLE_FINAL_W)] for y in range(BRAILLE_FINAL_H)]
    rows = []
    for cy in range(3):
        chars = []
        for cx in range(4):
            mask = 0
            for ly in range(4):
                for lx in range(2):
                    if lit[cy * 4 + ly][cx * 2 + lx]:
                        mask |= braille_bit(lx, ly)
            chars.append(chr(0x2800 + mask))
        rows.append("".join(chars))
    return rows


@dataclass
class ThresholdChoice:
    threshold: int
    lit_dots: int
    coverage_pct: int
    auto: bool
    rows: list[str]


def choose_threshold(rgba: bytes, override: int | None) -> ThresholdChoice:
    alphas = [rgba[i] for i in range(3, len(rgba), 4)]

    def lit_count(t: int) -> int:
        return sum(1 for a in alphas if a >= t)

    if override is not None:
        threshold = override
    else:
        threshold = None
        for candidate in THRESHOLD_LADDER:
            if lit_count(candidate) >= MIN_LIT_DOTS_TARGET:
                threshold = candidate
                break
        if threshold is None:
            for candidate in reversed(THRESHOLD_LADDER):
                if lit_count(candidate) >= 1:
                    threshold = candidate
                    break
        if threshold is None:
            threshold = max(1, max(alphas))

    n_lit = lit_count(threshold)
    return ThresholdChoice(
        threshold=threshold,
        lit_dots=n_lit,
        coverage_pct=round(100 * threshold / 255),
        auto=override is None,
        rows=braille_rows(rgba, threshold),
    )


def compact_braille_glyph(rgba: bytes, threshold: int) -> str:
    """2 braille glyphs (2 cells wide x 1 row tall) -- the exact compact
    button body `PixelCanvas::flush` renders at `threshold`."""
    lit = [
        [rgba[(y * COMPACT_BRAILLE_FINAL_W + x) * 4 + 3] >= threshold for x in range(COMPACT_BRAILLE_FINAL_W)]
        for y in range(COMPACT_BRAILLE_FINAL_H)
    ]
    chars = []
    for cx in range(2):
        mask = 0
        for ly in range(4):
            for lx in range(2):
                if lit[ly][cx * 2 + lx]:
                    mask |= braille_bit(lx, ly)
        chars.append(chr(0x2800 + mask))
    return "".join(chars)


@dataclass
class CompactThresholdChoice:
    threshold: int
    lit_dots: int
    coverage_pct: int
    auto: bool
    glyph: str


def choose_compact_threshold(rgba: bytes, override: int | None) -> CompactThresholdChoice:
    alphas = [rgba[i] for i in range(3, len(rgba), 4)]

    def lit_count(t: int) -> int:
        return sum(1 for a in alphas if a >= t)

    if override is not None:
        threshold = override
    else:
        threshold = None
        for candidate in THRESHOLD_LADDER:
            if lit_count(candidate) >= COMPACT_MIN_LIT_DOTS_TARGET:
                threshold = candidate
                break
        if threshold is None:
            for candidate in reversed(THRESHOLD_LADDER):
                if lit_count(candidate) >= 1:
                    threshold = candidate
                    break
        if threshold is None:
            threshold = max(1, max(alphas))

    n_lit = lit_count(threshold)
    return CompactThresholdChoice(
        threshold=threshold,
        lit_dots=n_lit,
        coverage_pct=round(100 * threshold / 255),
        auto=override is None,
        glyph=compact_braille_glyph(rgba, threshold),
    )


def rust_string_literal(s: str) -> str:
    escaped = s.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def generate_catalog(analyses: dict[str, ThresholdChoice], compact_analyses: dict[str, CompactThresholdChoice]) -> str:
    lines: list[str] = []
    lines.append("//! GENERATED by `tools/bake_icons.py` -- do not hand-edit. Re-run:")
    lines.append("//!   python tools/bake_icons.py")
    lines.append("//!")
    lines.append("//! Licence: MIT (microsoft/vscode-codicons,")
    lines.append("//! <https://github.com/microsoft/vscode-codicons/blob/main/LICENSE>).")
    lines.append("//! Source: raw.githubusercontent.com/microsoft/vscode-codicons/main/src/")
    lines.append("//! icons/<slug>.svg, `fill=\"currentColor\"` patched to `#cdd6f4`")
    lines.append("//! (`pty_palette::GATE_FG`) before rasterizing -- see `../icons.rs`'s own")
    lines.append("//! module doc for the full tier/pipeline explanation and")
    lines.append("//! `tools/bake_icons.py`'s own header for the exact bake recipe. Per-icon")
    lines.append("//! braille alpha thresholds below are chosen by `bake_icons.py`'s own")
    lines.append("//! `choose_threshold` (an explicit override, replaying the original 7 rail")
    lines.append("//! icons' hand-tuned constants unchanged, or an auto coverage-histogram")
    lines.append("//! search) -- see that function, not prose duplicated per icon here, for")
    lines.append("//! the selection reasoning; a `DEGRADED` line marks icons whose shape does")
    lines.append("//! not survive at 8x12 even at the best achievable threshold. The compact")
    lines.append("//! tier (2x1-cell braille / 1-cell sixel, for dense single-row buttons --")
    lines.append("//! see `../icons.rs`'s own module doc) is chosen the same way by")
    lines.append("//! `choose_compact_threshold`, against `COMPACT_THRESHOLD_OVERRIDES`.")
    lines.append("")
    lines.append("use std::sync::LazyLock;")
    lines.append("")
    lines.append("use uzor_tui::canvas::{CanvasMode, PixelCanvas};")
    lines.append("")
    lines.append("use super::{")
    lines.append("    build_sixel, build_sixel_compact, build_sixel_gallery, build_sixel_gallery_gate, build_sixel_gate,")
    lines.append("    build_sixel_strip, build_sixel_strip_gate, rgba_to_canvas, SixelVariant, BRAILLE_ICON_CELLS_TALL,")
    lines.append("    BRAILLE_ICON_CELLS_WIDE, COMPACT_BRAILLE_ICON_CELLS_TALL, COMPACT_BRAILLE_ICON_CELLS_WIDE,")
    lines.append("};")
    lines.append("")
    lines.append("/// Every baked icon this crate ships, sixel + braille + ascii tiers, one")
    lines.append("/// enum covering the full catalog (not just the activity rail -- see")
    lines.append("/// `../icons.rs`'s own module doc). Only the activity rail's original 7")
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
    lines.append("/// Encoded rail-tier sixel string for `id` at `variant`'s own background")
    lines.append("/// (see [`SixelVariant`]'s own doc comment) -- see `../icons.rs::build_sixel`'s")
    lines.append("/// own doc comment for why this is cached (`LazyLock`) rather than re-encoded")
    lines.append("/// per call.")
    lines.append("pub fn sixel(id: IconId, variant: SixelVariant) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"        IconId::{spec.rust_name} => match variant {{")
        lines.append(f"            SixelVariant::Transparent => {upper}_SIXEL.as_str(),")
        lines.append(f"            SixelVariant::GateActive => {upper}_SIXEL_GATE_ACTIVE.as_str(),")
        lines.append(f"            SixelVariant::GateAccent => {upper}_SIXEL_GATE_ACCENT.as_str(),")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded control-plane-strip-tier sixel string for `id` at `variant`'s")
    lines.append("/// own background -- the strip never shows a selected state (see `../")
    lines.append("/// render.rs::render_control_strip_button`'s own doc comment), so")
    lines.append("/// `GateAccent` resolves to the SAME asset as `GateActive` here.")
    lines.append("pub fn sixel_strip(id: IconId, variant: SixelVariant) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"        IconId::{spec.rust_name} => match variant {{")
        lines.append(f"            SixelVariant::Transparent => {upper}_SIXEL_STRIP.as_str(),")
        lines.append(f"            SixelVariant::GateActive | SixelVariant::GateAccent => {upper}_SIXEL_STRIP_GATE.as_str(),")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded icon-gallery-tier (60x60, 6 cells wide x 3 rows tall) sixel")
    lines.append("/// string for `id` at `variant`'s own background -- the gallery is a")
    lines.append("/// read-only comparison grid with no selected state, so `GateAccent`")
    lines.append("/// resolves to the SAME asset as `GateActive` here (same fold as")
    lines.append("/// [`sixel_strip`]).")
    lines.append("pub fn sixel_gallery(id: IconId, variant: SixelVariant) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        lines.append(f"        IconId::{spec.rust_name} => match variant {{")
        lines.append(f"            SixelVariant::Transparent => {upper}_SIXEL_GALLERY.as_str(),")
        lines.append(f"            SixelVariant::GateActive | SixelVariant::GateAccent => {upper}_SIXEL_GALLERY_GATE.as_str(),")
        lines.append("        },")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Braille-tier [`PixelCanvas`] for `id` -- see `../icons.rs::rgba_to_canvas`'s")
    lines.append("/// own doc comment for the alpha-threshold silhouette rule.")
    lines.append("pub fn braille(id: IconId) -> &'static PixelCanvas {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => &*{to_screaming_snake(spec.rust_name)}_BRAILLE,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// Encoded COMPACT-tier sixel string for `id` (exactly one assumed")
    lines.append("/// terminal cell -- see `../icons.rs`'s own module doc) -- for dense")
    lines.append("/// single-row buttons where the rail's own 40x40 icon does not fit.")
    lines.append("pub fn sixel_compact(id: IconId) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_SIXEL_COMPACT.as_str(),")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("/// COMPACT-tier braille [`PixelCanvas`] for `id` (2 cells wide x 1 row")
    lines.append("/// tall -- see `../icons.rs`'s own module doc), same silhouette rule as")
    lines.append("/// [`braille`].")
    lines.append("pub fn braille_compact(id: IconId) -> &'static PixelCanvas {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => &*{to_screaming_snake(spec.rust_name)}_BRAILLE_COMPACT,")
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
    lines.append("// Raw baked-source lookups by id -- used only by this crate's own unit")
    lines.append("// tests (byte-length assertions, and the rail's own no-visual-regression")
    lines.append("// check against `../icons.rs`'s hand-written test module).")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn sixel_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_RGBA,")
    lines.append("    }")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("pub(crate) fn braille_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_BRAILLE_RGBA,")
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
    lines.append("pub(crate) fn braille_compact_source_rgba(id: IconId) -> &'static [u8] {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_COMPACT_BRAILLE_RGBA,")
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
        analysis = analyses[spec.slug]
        compact_analysis = compact_analyses[spec.slug]
        degraded_note = KNOWN_DEGRADED.get(spec.slug)
        lines.append(f"// ---- {spec.rust_name} ({spec.slug}) " + "-" * max(1, 60 - len(spec.rust_name) - len(spec.slug)))
        lines.append("")
        lines.append(f'const {upper}_RGBA: &[u8] = include_bytes!("{spec.stem()}.rgba");')
        lines.append(f"static {upper}_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel({upper}_RGBA));")
        lines.append("")
        lines.append("/// GateOverride, at-rest background (`render::ACTIVE_BG`) -- pre-composited")
        lines.append("/// opaque at bake time, cause 1's own fix (see this crate's own `tools/")
        lines.append("/// bake_icons.py` module doc).")
        lines.append(f'const {upper}_GATE_ACTIVE_RGBA: &[u8] = include_bytes!("{spec.stem()}_gate_active.rgba");')
        lines.append(f"static {upper}_SIXEL_GATE_ACTIVE: LazyLock<String> = LazyLock::new(|| build_sixel_gate({upper}_GATE_ACTIVE_RGBA));")
        lines.append("")
        lines.append("/// GateOverride, selected/accent background (`render::MAUVE`) -- same fix,")
        lines.append("/// the rail's own selected-state background.")
        lines.append(f'const {upper}_GATE_ACCENT_RGBA: &[u8] = include_bytes!("{spec.stem()}_gate_accent.rgba");')
        lines.append(f"static {upper}_SIXEL_GATE_ACCENT: LazyLock<String> = LazyLock::new(|| build_sixel_gate({upper}_GATE_ACCENT_RGBA));")
        lines.append("")
        lines.append(f'const {upper}_BRAILLE_RGBA: &[u8] = include_bytes!("{spec.stem()}_braille.rgba");')
        if spec.slug in LEGACY_RAIL_SLUGS:
            origin = "override (replaying the pre-existing rail icon's own hand-tuned constant, unchanged)"
        elif not analysis.auto:
            origin = "override, hand-picked after reviewing bake_icons.py's own auto-suggested preview"
        else:
            origin = "auto-selected by bake_icons.py::choose_threshold"
        lines.append(
            f"/// ~{analysis.coverage_pct}% coverage ({analysis.threshold}/255), {origin} -- "
            f"{analysis.lit_dots}/{BRAILLE_TOTAL_DOTS} dots lit at this threshold."
        )
        if degraded_note:
            lines.append(f"/// DEGRADED: {degraded_note}")
        lines.append(f"const {upper}_BRAILLE_ALPHA_THRESHOLD: u8 = {analysis.threshold};")
        lines.append(f"static {upper}_BRAILLE: LazyLock<PixelCanvas> = LazyLock::new(|| {{")
        lines.append("    rgba_to_canvas(")
        lines.append("        CanvasMode::Braille,")
        lines.append("        BRAILLE_ICON_CELLS_WIDE,")
        lines.append("        BRAILLE_ICON_CELLS_TALL,")
        lines.append(f"        {upper}_BRAILLE_RGBA,")
        lines.append(f"        {upper}_BRAILLE_ALPHA_THRESHOLD,")
        lines.append("    )")
        lines.append("});")
        lines.append("")
        lines.append(f'const {upper}_COMPACT_RGBA: &[u8] = include_bytes!("{spec.stem()}_compact.rgba");')
        lines.append(f"static {upper}_SIXEL_COMPACT: LazyLock<String> = LazyLock::new(|| build_sixel_compact({upper}_COMPACT_RGBA));")
        lines.append("")
        lines.append(f'const {upper}_COMPACT_BRAILLE_RGBA: &[u8] = include_bytes!("{spec.stem()}_compact_braille.rgba");')
        compact_origin = (
            "override, hand-picked after reviewing bake_icons.py's own printed report"
            if not compact_analysis.auto
            else "auto-selected by bake_icons.py::choose_compact_threshold"
        )
        lines.append(
            f"/// ~{compact_analysis.coverage_pct}% coverage ({compact_analysis.threshold}/255), {compact_origin} -- "
            f"{compact_analysis.lit_dots}/{COMPACT_BRAILLE_TOTAL_DOTS} dots lit at this threshold."
        )
        lines.append(f"const {upper}_COMPACT_BRAILLE_ALPHA_THRESHOLD: u8 = {compact_analysis.threshold};")
        lines.append(f"static {upper}_BRAILLE_COMPACT: LazyLock<PixelCanvas> = LazyLock::new(|| {{")
        lines.append("    rgba_to_canvas(")
        lines.append("        CanvasMode::Braille,")
        lines.append("        COMPACT_BRAILLE_ICON_CELLS_WIDE,")
        lines.append("        COMPACT_BRAILLE_ICON_CELLS_TALL,")
        lines.append(f"        {upper}_COMPACT_BRAILLE_RGBA,")
        lines.append(f"        {upper}_COMPACT_BRAILLE_ALPHA_THRESHOLD,")
        lines.append("    )")
        lines.append("});")
        lines.append("")
        lines.append(f'const {upper}_STRIP_RGBA: &[u8] = include_bytes!("{spec.stem()}_strip.rgba");')
        lines.append(f"static {upper}_SIXEL_STRIP: LazyLock<String> = LazyLock::new(|| build_sixel_strip({upper}_STRIP_RGBA));")
        lines.append("")
        lines.append("/// GateOverride, the strip's own single background (`render::ACTIVE_BG`) --")
        lines.append("/// pre-composited opaque at bake time, same fix as the rail tier above.")
        lines.append(f'const {upper}_STRIP_GATE_RGBA: &[u8] = include_bytes!("{spec.stem()}_strip_gate.rgba");')
        lines.append(f"static {upper}_SIXEL_STRIP_GATE: LazyLock<String> = LazyLock::new(|| build_sixel_strip_gate({upper}_STRIP_GATE_RGBA));")
        lines.append("")
        lines.append(f'const {upper}_GALLERY_RGBA: &[u8] = include_bytes!("{spec.stem()}_gallery.rgba");')
        lines.append(f"static {upper}_SIXEL_GALLERY: LazyLock<String> = LazyLock::new(|| build_sixel_gallery({upper}_GALLERY_RGBA));")
        lines.append("")
        lines.append("/// GateOverride, the icon gallery's own single background (`render::")
        lines.append("/// ACTIVE_BG`) -- pre-composited opaque at bake time, same fix as the")
        lines.append("/// rail/strip tiers above.")
        lines.append(f'const {upper}_GALLERY_GATE_RGBA: &[u8] = include_bytes!("{spec.stem()}_gallery_gate.rgba");')
        lines.append(f"static {upper}_SIXEL_GALLERY_GATE: LazyLock<String> = LazyLock::new(|| build_sixel_gallery_gate({upper}_GALLERY_GATE_RGBA));")
        lines.append("")

    return "\n".join(lines) + "\n"


def to_screaming_snake(pascal: str) -> str:
    """`SourceControl` -> `SOURCE_CONTROL`."""
    out = re.sub(r"(?<!^)(?=[A-Z])", "_", pascal).upper()
    return out


def print_report(
    analyses: dict[str, ThresholdChoice],
    compact_analyses: dict[str, CompactThresholdChoice],
    asset_sizes: dict[str, tuple[int, int, bool]],
    gate_asset_sizes: dict[str, tuple[int, int, bool]],
    compact_asset_sizes: dict[str, tuple[int, int, bool]],
    strip_asset_sizes: dict[str, tuple[int, int, bool]],
    gallery_asset_sizes: dict[str, tuple[int, int, bool]],
) -> None:
    print()
    print("=" * 78)
    print("ICON CATALOG BRAILLE PREVIEW (exact PixelCanvas::flush output, per icon)")
    print("=" * 78)
    by_slug = {spec.slug: spec for spec in MANIFEST}
    for group_name, slugs in REPORT_GROUPS:
        group_slugs = [s for s in slugs if s in analyses]
        if not group_slugs:
            continue
        print()
        print(f"-- {group_name} " + "-" * max(1, 60 - len(group_name)))
        for slug in group_slugs:
            spec = by_slug[slug]
            analysis = analyses[slug]
            auto_floor_hit = analysis.lit_dots < DEGRADED_LIT_DOTS_FLOOR
            degraded = " [DEGRADED]" if slug in KNOWN_DEGRADED or auto_floor_hit else ""
            if auto_floor_hit and slug not in KNOWN_DEGRADED:
                degraded += " (auto: below the lit-dot floor, not yet reviewed by hand)"
            print(f"  {spec.rust_name} ({slug})  ascii={spec.ascii!r}  thr={analysis.threshold} (~{analysis.coverage_pct}%)  lit={analysis.lit_dots}/{BRAILLE_TOTAL_DOTS}{degraded}")
            for row in analysis.rows:
                print(f"    [{row}]")
            if slug in compact_analyses:
                compact = compact_analyses[slug]
                print(f"    compact: thr={compact.threshold} (~{compact.coverage_pct}%)  lit={compact.lit_dots}/{COMPACT_BRAILLE_TOTAL_DOTS}  [{compact.glyph}]")

    print()
    print("=" * 78)
    print("ASSET SIZE TOTALS")
    print("=" * 78)
    total_sixel = sum(s for s, _, _ in asset_sizes.values())
    total_braille = sum(b for _, b, _ in asset_sizes.values())
    newly_baked_sixel = sum(s for s, _, new in asset_sizes.values() if new)
    newly_baked_braille = sum(b for _, b, new in asset_sizes.values() if new)
    n_new = sum(1 for *_, new in asset_sizes.values() if new)
    print(f"Icons total: {len(MANIFEST)}  (newly baked this run: {n_new})")
    print(f"Sixel tier:   {total_sixel:>9} bytes total ({total_sixel / 1024:.1f} KiB)  -- {newly_baked_sixel} bytes newly added")
    print(f"Braille tier: {total_braille:>9} bytes total ({total_braille / 1024:.1f} KiB) -- {newly_baked_braille} bytes newly added")
    total_gate = total_compact_sixel = total_compact_braille = total_strip = total_gallery = 0
    if gate_asset_sizes:
        total_gate = sum(a + c for a, c, _ in gate_asset_sizes.values())
        newly_baked_gate = sum(a + c for a, c, new in gate_asset_sizes.values() if new)
        print(f"Rail GateOverride variants (active+accent): {total_gate:>9} bytes total ({total_gate / 1024:.1f} KiB) -- {newly_baked_gate} bytes newly added")
    if compact_asset_sizes:
        total_compact_sixel = sum(s for s, _, _ in compact_asset_sizes.values())
        total_compact_braille = sum(b for _, b, _ in compact_asset_sizes.values())
        newly_baked_compact_sixel = sum(s for s, _, new in compact_asset_sizes.values() if new)
        newly_baked_compact_braille = sum(b for _, b, new in compact_asset_sizes.values() if new)
        print(f"Compact sixel tier:   {total_compact_sixel:>9} bytes total ({total_compact_sixel / 1024:.1f} KiB)  -- {newly_baked_compact_sixel} bytes newly added")
        print(f"Compact braille tier: {total_compact_braille:>9} bytes total ({total_compact_braille / 1024:.1f} KiB) -- {newly_baked_compact_braille} bytes newly added")
    if strip_asset_sizes:
        total_strip = sum(t + g for t, g, _ in strip_asset_sizes.values())
        newly_baked_strip = sum(t + g for t, g, new in strip_asset_sizes.values() if new)
        print(f"Strip sixel tier (transparent+gate): {total_strip:>9} bytes total ({total_strip / 1024:.1f} KiB) -- {newly_baked_strip} bytes newly added")
    if gallery_asset_sizes:
        total_gallery = sum(t + g for t, g, _ in gallery_asset_sizes.values())
        newly_baked_gallery = sum(t + g for t, g, new in gallery_asset_sizes.values() if new)
        print(f"Gallery sixel tier (transparent+gate): {total_gallery:>9} bytes total ({total_gallery / 1024:.1f} KiB) -- {newly_baked_gallery} bytes newly added")
    print(
        f"Combined:     {total_sixel + total_braille + total_gate + total_compact_sixel + total_compact_braille + total_strip + total_gallery:>9} bytes total "
        f"({(total_sixel + total_braille + total_gate + total_compact_sixel + total_compact_braille + total_strip + total_gallery) / 1024:.1f} KiB)"
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


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    only = set(s.strip() for s in args.only.split(",")) if args.only else None

    selected = [spec for spec in MANIFEST if only is None or spec.slug in only]
    if only is not None:
        missing = only - {spec.slug for spec in selected}
        if missing:
            die(f"--only names not in MANIFEST: {sorted(missing)}")

    analyses: dict[str, ThresholdChoice] = {}
    asset_sizes: dict[str, tuple[int, int, bool]] = {}
    gate_asset_sizes: dict[str, tuple[int, int, bool]] = {}
    compact_analyses: dict[str, CompactThresholdChoice] = {}
    compact_asset_sizes: dict[str, tuple[int, int, bool]] = {}
    strip_asset_sizes: dict[str, tuple[int, int, bool]] = {}
    gallery_asset_sizes: dict[str, tuple[int, int, bool]] = {}
    for spec in selected:
        sixel_existed = (ICONS_DIR / f"{spec.stem()}.rgba").exists()
        braille_existed = (ICONS_DIR / f"{spec.stem()}_braille.rgba").exists()
        active_existed = (ICONS_DIR / f"{spec.stem()}_gate_active.rgba").exists()
        accent_existed = (ICONS_DIR / f"{spec.stem()}_gate_accent.rgba").exists()
        sixel_path, braille_path, active_path, accent_path = ensure_assets(spec, args.cache_dir, args.force)
        newly_baked = args.force or not sixel_existed or not braille_existed
        asset_sizes[spec.slug] = (sixel_path.stat().st_size, braille_path.stat().st_size, newly_baked)
        gate_asset_sizes[spec.slug] = (
            active_path.stat().st_size,
            accent_path.stat().st_size,
            args.force or not active_existed or not accent_existed,
        )
        rgba = braille_path.read_bytes()
        analyses[spec.slug] = choose_threshold(rgba, THRESHOLD_OVERRIDES.get(spec.slug))

        compact_sixel_existed = (ICONS_DIR / f"{spec.stem()}_compact.rgba").exists()
        compact_braille_existed = (ICONS_DIR / f"{spec.stem()}_compact_braille.rgba").exists()
        compact_sixel_path, compact_braille_path = ensure_compact_assets(spec, args.cache_dir, args.force)
        compact_newly_baked = args.force or not compact_sixel_existed or not compact_braille_existed
        compact_asset_sizes[spec.slug] = (compact_sixel_path.stat().st_size, compact_braille_path.stat().st_size, compact_newly_baked)
        compact_rgba = compact_braille_path.read_bytes()
        compact_analyses[spec.slug] = choose_compact_threshold(compact_rgba, COMPACT_THRESHOLD_OVERRIDES.get(spec.slug))

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

    if only is None:
        # Full manifest processed -- every icon has assets on disk and an
        # analysis in hand, safe to regenerate the complete catalog.
        CATALOG_RS.write_text(generate_catalog(analyses, compact_analyses), encoding="utf-8", newline="\n")
        print(f"wrote {CATALOG_RS} ({CATALOG_RS.stat().st_size} bytes)")
    else:
        print(f"--only restricted this run to {sorted(only)} -- catalog.rs NOT regenerated (needs the full manifest)")

    print_report(analyses, compact_analyses, asset_sizes, gate_asset_sizes, compact_asset_sizes, strip_asset_sizes, gallery_asset_sizes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
