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
                                                   subset -- for hand
                                                   comparison; does NOT
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
     `--force`.
  4. Regenerate `src/icons/catalog.rs` from the FULL manifest (only when
     not restricted by `--only`) -- one `IconId` enum variant, one sixel
     `LazyLock<String>` per tier/variant, one ascii literal, per icon.
  5. Print a report: asset-size totals.

Nothing here is a build-time Cargo dependency -- `resvg`/`ffmpeg` run
once, offline, from a developer's own PATH, producing checked-in
`.rgba` files `include_bytes!`'d at compile time (see `src/icons.rs`).

## Quality pass (dirty edges / blurred strokes / gamma-space compositing /
## cell-height mismatch / control-strip resize)

Five defects diagnosed against the running TUI, each verified against
this tool's own pipeline (not taken on faith) before fixing.

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
   transparent-encoded asset (see cause 4 below for what "keeps" now
   means) as its only available option. The rail tier has two backgrounds
   (`theme.active` at rest, `theme.accent` selected) so it gets two
   composited variants (`SixelVariant::GateActive`/`GateAccent`); the
   strip/gallery tiers below have exactly one (neither ever shows a
   selected state), so each gets one.

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
   (`linear_to_srgb`). Governs the `GateActive`/`GateAccent` (and strip/
   gallery `_gate`) pre-composited variants, where the exact background is
   known at bake time.

4. THE SAME PROBLEM FOR `Transparent` VARIANTS -- the terminal, not this
   tool, performs that composite (see cause 1 above), so this tool cannot
   fix the blend itself; instead it pre-corrects the ALPHA it hands the
   terminal so that terminal's own (naive, gamma-space) blend lands close
   to the gamma-CORRECT result. `precorrect_transparent_alpha` replaces
   each transparent-tier pixel's TRUE coverage `a` with `a' = 255 *
   (a/255)**(1/2.4)` -- e.g. 50% true coverage bakes to alpha 191, not
   128. This is only EXACT against a fully black background (there, a
   naive gamma-space blend of `ink` and `0` reduces to `ink * (a'/255)`,
   the same product a linear-space blend against a TRUE black background
   would also produce, since black's own linear value is 0 either way);
   against a lighter background it is an approximation, and there is no
   way to do better without knowing the terminal's own actual background
   at bake time -- the SAME epistemic gap `../icons.rs::ASSUMED_CELL_
   WIDTH_PX`'s own doc comment already names for cell-pixel size, and
   `SixelVariant`'s own doc comment already names for why `PtyColorMode::
   Inherited` gets no exact-background compositing at all. Applied ONLY
   to the transparent-tier `.rgba` outputs actually shipped to the
   terminal -- NEVER to the buffer fed into `composite_over_background`,
   which always gets true coverage and does its own correct linear blend
   against a KNOWN background (cause 3 above) -- see `ensure_assets`'s
   own doc comment for the ordering this depends on.

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

# ---- GateOverride compositing (cause 1's fix -- see this module's own
# header doc comment). Hand-synced to render.rs's own fixed theme
# constants: `ACTIVE_BG` (the rail/strip button body's own "at rest"
# colour) and `MAUVE` (`theme.accent`, the rail's own "selected" colour).
# `PtyColorMode::GateOverride` only -- see `icons::SixelVariant`'s own doc
# comment for why `PtyColorMode::Inherited` has no equivalent.
GATE_ACTIVE_BG_RGB = (30, 30, 46)  # render.rs::ACTIVE_BG
GATE_ACCENT_BG_RGB = (203, 166, 247)  # render.rs::MAUVE / theme.accent


@dataclass(frozen=True)
class IconSpec:
    rust_name: str  # PascalCase IconId variant
    slug: str  # codicon file stem, e.g. "source-control"
    ascii: str  # 1-2 char ASCII label
    file_stem: str = field(default="")  # snake_case asset stem; derived if empty

    def stem(self) -> str:
        return self.file_stem or self.slug.replace("-", "_")


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
    """Rail tier: fit-within `SIXEL_PX_W` x `SIXEL_PX_H`, computed by hand
    (`svg_intrinsic_size`/`fit_within`) rather than left to resvg's own
    `-w`/`-h` rounding -- the SAME reasoning `rasterize_strip_sixel`
    already documents (see `svg_intrinsic_size`'s own doc comment), now
    needed here too since the correct 10x19 cell makes this tier's own
    box non-square (cause 5 in this module's own header doc comment) for
    the first time; the old, truly-square 40x40 box had no rounding
    ambiguity to avoid. Returns the TRUE-coverage raw RGBA8 bytes -- NOT
    written to `ICONS_DIR` directly, see `ensure_assets`'s own doc
    comment for why.

    Equivalent hand-run commands (for a square 16x16-viewBox source, the
    most common case in this manifest -- fit-within lands such a source
    on 38x38, the box's own height twice, since the box is wider than it
    is tall; a non-square source like `output.svg` fits within the box
    first, same as `rasterize_strip_sixel`):
        resvg -w 38 -h 38 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=40:38:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>.rgba
    """
    fit_w, fit_h = fit_within(*svg_intrinsic_size(patched_svg), SIXEL_PX_W, SIXEL_PX_H)
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        raw_rgba = Path(tmp) / "raw.rgba"
        run_tool(["resvg", "-w", str(fit_w), "-h", str(fit_h), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={SIXEL_PX_W}:{SIXEL_PX_H}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(raw_rgba),
        ])
        data = raw_rgba.read_bytes()
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


def rasterize_strip_sixel(patched_svg: Path) -> bytes:
    """Control-plane strip tier: fit-within `STRIP_SIXEL_PX_W`x`_H` (2
    cells wide x 1 row tall, exactly the button's own full body -- unlike
    the rail tier's own square crop of a taller body, see `../icons.rs`'s
    own `SIXEL_ICON_WIDTH_PX` doc comment). Single resvg pass DIRECTLY at
    the target size -- the same "rasterize once, at the target size, with
    resvg's own high-quality AA" recipe `rasterize_sixel`/`rasterize_
    compact_sixel` already use (see this module's own header doc
    comment's cause-2 note: there is no larger intermediate render and no
    second ffmpeg scale here, only a same-size pad) -- except the exact
    fit-within pixel size handed to resvg is precomputed by hand (`svg_
    intrinsic_size`/`fit_within`) rather than resvg's own `-w`/`-h`, to
    sidestep a rounding edge case at this tier's own small absolute size
    (see `svg_intrinsic_size`'s own doc comment). Returns the TRUE-
    coverage raw RGBA8 bytes -- see `rasterize_sixel`'s own doc comment
    for why this is not written to disk here.

    Equivalent hand-run commands (for a square 16x16-viewBox source; a
    non-square source like `output.svg` fits within the box first, see
    above):
        resvg -w 19 -h 19 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=20:19:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>_strip.rgba
    """
    fit_w, fit_h = fit_within(*svg_intrinsic_size(patched_svg), STRIP_SIXEL_PX_W, STRIP_SIXEL_PX_H)
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        raw_rgba = Path(tmp) / "raw.rgba"
        run_tool(["resvg", "-w", str(fit_w), "-h", str(fit_h), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={STRIP_SIXEL_PX_W}:{STRIP_SIXEL_PX_H}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(raw_rgba),
        ])
        data = raw_rgba.read_bytes()
    if len(data) != STRIP_SIXEL_RGBA_LEN:
        die(f"strip sixel raster for {patched_svg} produced {len(data)} bytes, expected {STRIP_SIXEL_RGBA_LEN}")
    return data


def rasterize_gallery_sixel(patched_svg: Path) -> bytes:
    """Gallery tier: fit-within `GALLERY_SIXEL_PX_W`x`_H` (6 cells wide x
    3 rows tall -- the icon gallery's own comparison column, see `icons::
    GALLERY_SIXEL_ICON_WIDTH_PX`'s own doc comment). Single resvg pass
    DIRECTLY at the target size -- the exact same recipe `rasterize_
    strip_sixel` already uses (see this module's own header doc comment's
    cause-2 note), including the same precomputed fit-within pixel size
    (`svg_intrinsic_size`/`fit_within`) rather than resvg's own `-w`/`-h`.
    Returns the TRUE-coverage raw RGBA8 bytes -- see `rasterize_sixel`'s
    own doc comment for why this is not written to disk here.

    Equivalent hand-run commands (for a square 16x16-viewBox source; a
    non-square source like `output.svg` fits within the box first, see
    above):
        resvg -w 57 -h 57 <slug>.patched.svg <slug>_raw.png
        ffmpeg -i <slug>_raw.png \\
            -vf "pad=60:57:(ow-iw)/2:(oh-ih)/2:color=black@0.0" \\
            -f rawvideo -pix_fmt rgba <slug>_gallery.rgba
    """
    fit_w, fit_h = fit_within(*svg_intrinsic_size(patched_svg), GALLERY_SIXEL_PX_W, GALLERY_SIXEL_PX_H)
    with tempfile.TemporaryDirectory() as tmp:
        raw_png = Path(tmp) / "raw.png"
        raw_rgba = Path(tmp) / "raw.rgba"
        run_tool(["resvg", "-w", str(fit_w), "-h", str(fit_h), str(patched_svg), str(raw_png)])
        run_tool([
            "ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
            "-i", str(raw_png),
            "-vf", f"pad={GALLERY_SIXEL_PX_W}:{GALLERY_SIXEL_PX_H}:(ow-iw)/2:(oh-ih)/2:color=black@0.0",
            "-f", "rawvideo", "-pix_fmt", "rgba",
            str(raw_rgba),
        ])
        data = raw_rgba.read_bytes()
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


def precorrect_transparent_alpha(rgba: bytes) -> bytes:
    """Cause 4's fix (see this module's own header doc comment): the
    `Transparent`-variant `.rgba` outputs are composited by the TERMINAL
    at render time, not by this tool -- there is no known background to
    blend against at bake time at all, so `composite_over_background`
    above does not apply here -- and the terminal's own blend is a naive
    gamma-space one this tool cannot change. Instead, this pre-corrects
    the ALPHA baked into the asset so that the terminal's naive blend
    lands close to the gamma-CORRECT result: replaces each pixel's TRUE
    coverage `a` with `a' = 255 * (a/255)**(1/2.4)` (e.g. 50% true
    coverage bakes to alpha 191, not 128). RGB channels are untouched --
    straight alpha, per `patch_fill`'s own already-tinted ink colour. The
    dark-background assumption this approximation rests on is spelled out
    in full in this module's own header doc comment, cause 4.

    Applied ONLY to the shipped `Transparent`-variant bytes, AFTER
    `composite_over_background` (where one exists for this tier) has
    already derived the exact-background `_gate*` variant(s) from the
    SAME buffer's own true coverage -- see `ensure_assets`'s own doc
    comment for the ordering this depends on: composited FIRST from true
    coverage, alpha-precorrected SECOND for the plain asset, never the
    other way around."""
    out = bytearray(rgba)
    for i in range(3, len(out), 4):
        coverage = out[i] / 255.0
        out[i] = max(0, min(255, round(255.0 * (coverage ** (1.0 / 2.4)))))
    return bytes(out)


def ensure_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path, Path]:
    """Ensure every rail-tier `.rgba` output for `spec` exists on disk,
    baking whatever is missing (or everything, if `force`). Returns
    (sixel, gate_active, gate_accent) paths. This is the idempotency
    boundary: a normal re-run with nothing new to bake touches no
    network and spawns no subprocess at all.

    ORDERING (load-bearing for cause 3/4's own correctness -- see this
    module's own header doc comment): whenever ANY of the three outputs
    needs rebuilding, this rasterizes ONE fresh TRUE-coverage buffer in
    memory (`raw`) and derives EVERYTHING from that SAME buffer --
    `composite_over_background` (cause 3: exact background, exact linear
    blend) for the two `_gate_*` variants, THEN `precorrect_transparent_
    alpha` (cause 4: approximate, unknown background) for the plain
    `.rgba` this stem's own `Transparent` variant ships. Deriving the
    `_gate_*` variants from anything OTHER than a just-rasterized `raw`
    -- e.g. reading the already-alpha-precorrected `.rgba` back off disk
    -- would double-apply cause 4's own approximation on top of cause 3's
    own exact blend, so this never reads the transparent asset back off
    disk to feed compositing, even on an idempotent re-run that only
    needs to rebuild the gate variants (the small correctness cost: such
    a re-run re-rasterizes even though the sixel output itself did not
    need it, trading a little idempotency for never risking that bug)."""
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
            sixel_path.write_bytes(precorrect_transparent_alpha(raw))

    return sixel_path, active_path, accent_path


def ensure_strip_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Ensure the control-plane strip's own two sixel-tier outputs:
    `<stem>_strip.rgba` (the `Transparent` variant, alpha-precorrected --
    see `precorrect_transparent_alpha`) and `<stem>_strip_gate.rgba` (the
    SAME raw pixels alpha-composited over `GATE_ACTIVE_BG_RGB` in linear
    light -- see `composite_over_background`). Same "derive both from one
    freshly-rasterized `raw` buffer, never read the precorrected asset
    back for compositing" ordering as `ensure_assets` above -- see that
    function's own doc comment. The strip never shows a `selected` state
    (see `render::render_control_strip_button`'s own doc comment), so
    there is no `_strip_gate_accent` variant."""
    transparent_path = ICONS_DIR / f"{spec.stem()}_strip.rgba"
    gate_path = ICONS_DIR / f"{spec.stem()}_strip_gate.rgba"
    need_transparent = force or not transparent_path.exists() or transparent_path.stat().st_size != STRIP_SIXEL_RGBA_LEN
    need_gate = force or not gate_path.exists() or gate_path.stat().st_size != STRIP_SIXEL_RGBA_LEN

    if need_transparent or need_gate:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_strip_sixel(patched)
        if need_gate:
            gate_path.write_bytes(composite_over_background(raw, GATE_ACTIVE_BG_RGB))
        if need_transparent:
            transparent_path.write_bytes(precorrect_transparent_alpha(raw))

    return transparent_path, gate_path


def ensure_gallery_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Ensure the icon gallery's own two dedicated sixel-tier outputs:
    `<stem>_gallery.rgba` (the `Transparent` variant, alpha-precorrected)
    and `<stem>_gallery_gate.rgba` (the same raw pixels composited over
    `GATE_ACTIVE_BG_RGB` in linear light). Same ordering precedent as
    `ensure_assets`/`ensure_strip_assets` above. Same "no selected state,
    so no `_gallery_gate_accent` variant" precedent as `ensure_strip_
    assets` -- the gallery is a read-only comparison grid."""
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
            transparent_path.write_bytes(precorrect_transparent_alpha(raw))

    return transparent_path, gate_path


def ensure_compact_assets(spec: IconSpec, cache_dir: Path, force: bool) -> Path:
    """Ensure the compact tier's own single sixel-tier output (`<stem>_
    compact.rgba`, alpha-precorrected -- see `precorrect_transparent_
    alpha`). This tier is `Transparent`-only -- out of scope for cause 1/
    3's own background-compositing fix (see `render::render_compact_icon_
    button`'s own doc comment: dense panel/modal content with 3+ distinct
    backgrounds, unlike the rail's 2 and the strip's 1) -- so there is no
    gate variant to derive here at all."""
    sixel_path = ICONS_DIR / f"{spec.stem()}_compact.rgba"
    need_sixel = force or not sixel_path.exists() or sixel_path.stat().st_size != COMPACT_SIXEL_RGBA_LEN
    if need_sixel:
        svg = fetch_svg(spec.slug, cache_dir)
        patched = patch_fill(svg, spec.slug, cache_dir)
        raw = rasterize_compact_sixel(patched)
        sixel_path.write_bytes(precorrect_transparent_alpha(raw))
    return sixel_path


def rust_string_literal(s: str) -> str:
    escaped = s.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def generate_catalog() -> str:
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
    lines.append("//! `tools/bake_icons.py`'s own header for the exact bake recipe.")
    lines.append("")
    lines.append("use std::sync::LazyLock;")
    lines.append("")
    lines.append("use super::{")
    lines.append("    build_sixel, build_sixel_compact, build_sixel_gallery, build_sixel_gallery_gate, build_sixel_gate,")
    lines.append("    build_sixel_strip, build_sixel_strip_gate, SixelVariant,")
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
    lines.append("/// Encoded icon-gallery-tier sixel string for `id` at `variant`'s own")
    lines.append("/// background -- the gallery is a read-only comparison grid with no")
    lines.append("/// selected state, so `GateAccent` resolves to the SAME asset as")
    lines.append("/// `GateActive` here (same fold as [`sixel_strip`]).")
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
    lines.append("// Raw baked-source lookups by id -- used only by this crate's own unit")
    lines.append("// tests (byte-length assertions and the gate-compositing pixel checks).")
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
        lines.append(f'const {upper}_RGBA: &[u8] = include_bytes!("{spec.stem()}.rgba");')
        lines.append(f"static {upper}_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel({upper}_RGBA));")
        lines.append("")
        lines.append("/// GateOverride, at-rest background (`render::ACTIVE_BG`) -- pre-composited")
        lines.append("/// opaque at bake time, gamma-correct linear blend (see this crate's own")
        lines.append("/// `tools/bake_icons.py` module doc, causes 1 and 3).")
        lines.append(f'const {upper}_GATE_ACTIVE_RGBA: &[u8] = include_bytes!("{spec.stem()}_gate_active.rgba");')
        lines.append(f"static {upper}_SIXEL_GATE_ACTIVE: LazyLock<String> = LazyLock::new(|| build_sixel_gate({upper}_GATE_ACTIVE_RGBA));")
        lines.append("")
        lines.append("/// GateOverride, selected/accent background (`render::MAUVE`) -- same fix,")
        lines.append("/// the rail's own selected-state background.")
        lines.append(f'const {upper}_GATE_ACCENT_RGBA: &[u8] = include_bytes!("{spec.stem()}_gate_accent.rgba");')
        lines.append(f"static {upper}_SIXEL_GATE_ACCENT: LazyLock<String> = LazyLock::new(|| build_sixel_gate({upper}_GATE_ACCENT_RGBA));")
        lines.append("")
        lines.append(f'const {upper}_COMPACT_RGBA: &[u8] = include_bytes!("{spec.stem()}_compact.rgba");')
        lines.append(f"static {upper}_SIXEL_COMPACT: LazyLock<String> = LazyLock::new(|| build_sixel_compact({upper}_COMPACT_RGBA));")
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
    asset_sizes: dict[str, tuple[int, bool]],
    gate_asset_sizes: dict[str, tuple[int, int, bool]],
    compact_asset_sizes: dict[str, tuple[int, bool]],
    strip_asset_sizes: dict[str, tuple[int, int, bool]],
    gallery_asset_sizes: dict[str, tuple[int, int, bool]],
) -> None:
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
    print(
        f"Combined:     {total_sixel + total_gate + total_compact_sixel + total_strip + total_gallery:>9} bytes total "
        f"({(total_sixel + total_gate + total_compact_sixel + total_strip + total_gallery) / 1024:.1f} KiB)"
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

    if only is None:
        # Full manifest processed -- every icon has assets on disk, safe
        # to regenerate the complete catalog.
        CATALOG_RS.write_text(generate_catalog(), encoding="utf-8", newline="\n")
        print(f"wrote {CATALOG_RS} ({CATALOG_RS.stat().st_size} bytes)")
    else:
        print(f"--only restricted this run to {sorted(only)} -- catalog.rs NOT regenerated (needs the full manifest)")

    print_report(asset_sizes, gate_asset_sizes, compact_asset_sizes, strip_asset_sizes, gallery_asset_sizes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
