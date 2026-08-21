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


def ensure_assets(spec: IconSpec, cache_dir: Path, force: bool) -> tuple[Path, Path]:
    """Ensure both `.rgba` outputs for `spec` exist on disk, baking
    whatever is missing (or everything, if `force`). Returns their paths.
    This is the idempotency boundary: a normal re-run with nothing new to
    bake touches no network and spawns no subprocess at all."""
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


def rust_string_literal(s: str) -> str:
    escaped = s.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def generate_catalog(analyses: dict[str, ThresholdChoice]) -> str:
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
    lines.append("//! not survive at 8x12 even at the best achievable threshold.")
    lines.append("")
    lines.append("use std::sync::LazyLock;")
    lines.append("")
    lines.append("use uzor_tui::canvas::{CanvasMode, PixelCanvas};")
    lines.append("")
    lines.append("use super::{build_sixel, rgba_to_canvas, BRAILLE_ICON_CELLS_TALL, BRAILLE_ICON_CELLS_WIDE};")
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
    lines.append("/// Encoded sixel string for `id` -- see `../icons.rs::build_sixel`'s own")
    lines.append("/// doc comment for why this is cached (`LazyLock`) rather than re-encoded")
    lines.append("/// per call.")
    lines.append("pub fn sixel(id: IconId) -> &'static str {")
    lines.append("    match id {")
    for spec in MANIFEST:
        lines.append(f"        IconId::{spec.rust_name} => {to_screaming_snake(spec.rust_name)}_SIXEL.as_str(),")
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

    for spec in MANIFEST:
        upper = to_screaming_snake(spec.rust_name)
        analysis = analyses[spec.slug]
        degraded_note = KNOWN_DEGRADED.get(spec.slug)
        lines.append(f"// ---- {spec.rust_name} ({spec.slug}) " + "-" * max(1, 60 - len(spec.rust_name) - len(spec.slug)))
        lines.append("")
        lines.append(f'const {upper}_RGBA: &[u8] = include_bytes!("{spec.stem()}.rgba");')
        lines.append(f"static {upper}_SIXEL: LazyLock<String> = LazyLock::new(|| build_sixel({upper}_RGBA));")
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

    return "\n".join(lines) + "\n"


def to_screaming_snake(pascal: str) -> str:
    """`SourceControl` -> `SOURCE_CONTROL`."""
    out = re.sub(r"(?<!^)(?=[A-Z])", "_", pascal).upper()
    return out


def print_report(analyses: dict[str, ThresholdChoice], asset_sizes: dict[str, tuple[int, int, bool]]) -> None:
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
    print(f"Combined:     {total_sixel + total_braille:>9} bytes total ({(total_sixel + total_braille) / 1024:.1f} KiB) -- {newly_baked_sixel + newly_baked_braille} bytes newly added")
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
    for spec in selected:
        sixel_existed = (ICONS_DIR / f"{spec.stem()}.rgba").exists()
        braille_existed = (ICONS_DIR / f"{spec.stem()}_braille.rgba").exists()
        sixel_path, braille_path = ensure_assets(spec, args.cache_dir, args.force)
        newly_baked = args.force or not sixel_existed or not braille_existed
        asset_sizes[spec.slug] = (sixel_path.stat().st_size, braille_path.stat().st_size, newly_baked)
        rgba = braille_path.read_bytes()
        analyses[spec.slug] = choose_threshold(rgba, THRESHOLD_OVERRIDES.get(spec.slug))

    if only is None:
        # Full manifest processed -- every icon has assets on disk and an
        # analysis in hand, safe to regenerate the complete catalog.
        CATALOG_RS.write_text(generate_catalog(analyses), encoding="utf-8", newline="\n")
        print(f"wrote {CATALOG_RS} ({CATALOG_RS.stat().st_size} bytes)")
    else:
        print(f"--only restricted this run to {sorted(only)} -- catalog.rs NOT regenerated (needs the full manifest)")

    print_report(analyses, asset_sizes)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
