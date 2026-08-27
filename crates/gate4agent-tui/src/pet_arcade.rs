//! The Pet Bastion arcade overlay: host-side glue between `gate4agent-tui`
//! and the `gate4agent-arcade` mini-game engine. Owns exactly one hosted
//! run (paused, in progress, or finished) and every piece of state its
//! glyph-tier board needs to be driven from either input device -- a
//! selected-tile/selected-anchor cursor (keyboard: `Tab`/`[`/`]`, still the
//! only way to MOVE the cursor without touching a tile directly) plus a
//! selected tower kind and an inspected-entity slot (mouse: `click_tile`/
//! `click_tower_kind`/`inspect_wave`, driven straight off the board's own
//! on-screen tile grid -- see `render::render_pet_arcade`'s own doc
//! comment for how a screen cell resolves to a board tile).
//!
//! # Free build placement, not fixed pads
//!
//! `gate4agent-arcade-pet-bastion`'s own board (`board.rs`) used to expose
//! ten fixed build pads (`PadId`/`PADS`). It now exposes every board cell
//! within its own build radius of a route (`Board::near_route_cells`,
//! surfaced per-run as `SimulationSnapshot::build_cells`), and `Command::
//! Place` addresses a `Tile` directly. This module tracks a `selected_tile:
//! Tile` cursor rather than a pad index, and every "which cells can I build
//! on right now" query (`Tab`/`BackTab` cycling, a tower drag's own board
//! highlight -- [`buildable_tiles`]) reads `SimulationSnapshot::build_cells`
//! -- never a hand-maintained pad table -- so this module never needs a
//! second update the next time that list's own eligibility rules change.
//!
//! Drawing lives in `render.rs` (`render_pet_arcade`), matching this
//! crate's own "every `render_*` fn lives in `render.rs`" convention --
//! this module owns simulation/session state and input handling only, the
//! same split `gate4agent-arcade-pet-bastion` (rules) vs. `gate4agent-
//! arcade-pet-bastion-render` (presentation) already draws one layer down.
//!
//! # Why `Rc<RefCell<PetArcade>>`, not a plain field
//!
//! `App` derives `Clone, Debug` (exercised by real, non-`cfg(test)`-gated
//! test code that clones a live `App` to render it twice under a mutated
//! field). Neither `gate4agent_arcade_engine::Runner` nor `GameScreen` nor
//! `ArcadeShell` implement `Clone` -- `Runner`'s own fields are private
//! with no public reconstruction API, so there is no way to hand-clone one
//! from outside the engine crate at all. `Rc::clone` sidesteps this
//! entirely (a refcount bump requires nothing of the pointee), so `App`'s
//! own derive is satisfied by a manual, non-inspecting [`std::fmt::Debug`]
//! impl on [`PetArcade`] below plus `Rc`'s unconditional `Clone` -- at the
//! cost of `App::clone()` sharing the SAME underlying session with its
//! clone rather than deep-copying it, which every existing caller of that
//! derive (icon-gallery/settings render-twice tests, unrelated to the
//! arcade) never observes.
//!
//! # Admission
//!
//! The engine's own `AdmissionSource` is a host-decided trust boundary the
//! engine never inspects (see `gate4agent_arcade_engine::admission`'s own
//! doc comment) -- it converts whatever real consumption signal a host
//! trusts into a plain credit count before `Runner::start` ever runs. This
//! TUI has no energy/session economy wired to the arcade yet, so
//! [`UnlimitedAdmission`] grants every run unconditionally; wiring a real
//! economy in later only ever replaces that one `AdmissionSource` impl,
//! never any call site below.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gate4agent_arcade_engine::{
    AdmissionCredit, AdmissionError, AdmissionSource, ArcadeOccupant, ArcadeShell, CellArea,
    GameCatalogEntry, GameEntry, GameScreen, MiniGame, Runner,
};
use gate4agent_arcade_pet_bastion::board::{AnchorId, BuildIneligibleReason, ANCHOR_COUNT, ANCHORS};
use gate4agent_arcade_pet_bastion::constants::{
    BLINK_COST, FIXED_SCALE, FULL_CIRCUIT_COST, PET_PULSE_COST,
};
use gate4agent_arcade_pet_bastion::geometry::{FixedPos, Tile};
use gate4agent_arcade_pet_bastion::ids::EntityId;
use gate4agent_arcade_pet_bastion::pet::{Evolution, PetState};
use gate4agent_arcade_pet_bastion::rune::Rune;
use gate4agent_arcade_pet_bastion::snapshot::{RunPhaseView, SimulationSnapshot, TowerView};
use gate4agent_arcade_pet_bastion::tower::{TowerKind, UpgradeBranch};
use gate4agent_arcade_pet_bastion::wave::Difficulty;
use gate4agent_arcade_pet_bastion::{Command, PetBastionParams, RunOutcome, Simulation};
use gate4agent_arcade_pet_bastion_render::interp::FramePresenter;

use crate::app::UiKey;

/// The hosted sim's own fixed tick, re-exported here so `client::run`'s
/// wake-cadence math never hardcodes a duplicate `50ms`/20Hz literal --
/// single source of truth is `Simulation::TICK` (`MiniGame::TICK`).
pub(crate) const TICK_INTERVAL: Duration = <Simulation as MiniGame>::TICK;

/// The pixel tier's own target render cadence -- 60Hz/~16.7ms, deliberately
/// independent of [`TICK_INTERVAL`]'s own fixed 20Hz: the sim itself never
/// runs any faster than that (the owner's own "симуляция остаётся на 20
/// тиках в секунду" mandate, restated at `gate4agent_arcade_pet_bastion_
/// render::interp`'s own module doc comment) -- what a faster redraw buys
/// is smoother MOTION, via `interp::interpolated_dynamic_sprites`
/// interpolating between the two most recent tick-boundary snapshots
/// (`PetArcade::presenter`), never a faster sim. `App::pet_arcade_wake_
/// interval` is the one consumer: it claims this cadence only while the
/// owner's own `PetArcadeVisualTier` preference is `Pixel`, and falls back
/// to `TICK_INTERVAL` for `Glyph` (which has nothing new to show between
/// sim ticks at all).
pub(crate) const PIXEL_TIER_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// See this module's own doc comment.
struct UnlimitedAdmission;

impl AdmissionSource for UnlimitedAdmission {
    fn available(&self) -> AdmissionCredit {
        AdmissionCredit(u32::MAX)
    }

    fn try_debit(&mut self, _cost: AdmissionCredit) -> Result<(), AdmissionError> {
        Ok(())
    }
}

/// The arcade shell's own game-select catalog -- exactly one entry today.
/// A `OnceLock` rather than a `const`/`static` array literal because
/// `GameEntry::min_modal_size`/`preferred_modal_size` are ordinary (not
/// `const fn`) trait methods -- computed once, lazily, the first time
/// anything asks.
fn catalog() -> &'static [GameCatalogEntry] {
    static CATALOG: OnceLock<[GameCatalogEntry; 1]> = OnceLock::new();
    CATALOG.get_or_init(|| {
        [GameCatalogEntry {
            id: Simulation::ID,
            title: Simulation::TITLE,
            min_modal_size: Simulation::min_modal_size(),
            preferred_modal_size: Simulation::preferred_modal_size(),
        }]
    })
}

pub(crate) fn min_modal_size() -> CellArea {
    Simulation::min_modal_size()
}

pub(crate) fn preferred_modal_size() -> CellArea {
    Simulation::preferred_modal_size()
}

fn fresh_seed() -> u64 {
    // Not cryptographic, and does not need to be: `EngineRng` seeds a
    // deterministic-for-replay sim, not a security boundary. Falls back to
    // `0` only if the system clock is somehow before the Unix epoch, never
    // panics.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ 0x9E37_79B9_7F4A_7C15
}

/// What the overlay's own inspector card (see `render::render_pet_arcade_
/// inspect_card`) is currently showing -- set by clicking an entity on the
/// board (`PetArcade::click_tile`) or the header's own wave/phase line
/// (`PetArcade::inspect_wave`), cleared by any click that selects a build
/// tile instead (`PetArcade::click_tile`'s own build/anchor arms). `Enemy`
/// carries only an `EntityId`, never a borrowed/owned enemy snapshot (which
/// would tie this to one specific frame) -- the card re-resolves it against
/// whatever snapshot is current at RENDER time, so an inspected enemy that
/// died between clicks degrades to an honest "defeated" line instead of a
/// dangling stale readout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inspect {
    Enemy(EntityId),
    Boss,
    /// The header's own wave/phase line, clicked. `SimulationSnapshot`
    /// exposes only a wave NUMBER today (`snapshot.wave`), never that
    /// wave's own enemy roster/composition -- the card this renders shows
    /// exactly that number plus the current phase, and says so honestly
    /// rather than fabricating a composition list. Ready to show a real
    /// one the moment the snapshot carries one; nothing else about this
    /// variant needs to change for that.
    Wave,
}

/// Whether a build-radius cell (`SimulationSnapshot::build_cells`) a tower
/// drag is currently hovering would accept a drop RIGHT NOW -- what
/// [`buildable_tiles`] reports for every cell it returns, and what `render::
/// render_pet_arcade`'s own drag highlight paints each of them as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildDragState {
    /// Empty and affordable -- dropping here places the tower.
    Buildable,
    /// Empty, but the player cannot afford the kind being dragged.
    Unaffordable,
    /// Another tower already stands here.
    Occupied,
}

/// One session's worth of arcade state. See this module's own doc comment
/// for why `App` holds this behind `Rc<RefCell<_>>` rather than directly.
pub(crate) struct PetArcade {
    shell: ArcadeShell,
    screen: GameScreen<Simulation>,
    /// Commands queued since the last tick that actually consumed a batch
    /// -- see [`Self::advance`]'s own doc comment for why this is NOT
    /// drained on every call, only on a call where `Runner::advance`
    /// itself reports a tick ran.
    pending: Vec<Command>,
    last_advance: Instant,
    /// The board cursor -- see this module's own top doc comment for why
    /// this is a raw [`Tile`] rather than a pad index. Set from `Tab`/
    /// `BackTab` (cycles through `SimulationSnapshot::build_cells`, in that
    /// list's own order) or a click on any build-radius tile
    /// (`click_tile`). Not guaranteed buildable at every instant a caller
    /// reads it -- occupancy/afford checks always happen against a live
    /// snapshot at the point of use, never cached here.
    selected_tile: Tile,
    selected_anchor: u8,
    /// The tower palette's own current selection (`render::render_pet_
    /// arcade_hud`'s "Towers:" rows) -- set by `click_tower_kind`, read by
    /// both that same fn (to place on the already-selected tile) and the
    /// renderer (to highlight the chosen row). The keyboard's own `1`-`6`
    /// placement bindings never touch this field: they carry their own
    /// kind directly in the keystroke, exactly like before this existed.
    selected_tower_kind: TowerKind,
    /// See [`Inspect`]'s own doc comment.
    inspect: Option<Inspect>,
    difficulty: Difficulty,
    /// The previous and current tick-boundary snapshots -- the pixel
    /// tier's own interpolation input (`render::render_pet_arcade`'s own
    /// `interp::interpolated_dynamic_sprites` call). Pushed exactly once
    /// per completed sim tick ([`Self::advance`]) and once more, eagerly,
    /// the moment a fresh run starts ([`Self::start_run`]) so the pixel
    /// tier never has to special-case "no `curr` yet" for the ~50ms window
    /// before the first real tick fires.
    presenter: FramePresenter,
    /// The wall-clock instant the LAST tick that actually fired ran at --
    /// distinct from `last_advance` above, which updates on every
    /// [`Self::advance`] call regardless of whether a tick ran. [`Self::
    /// tick_alpha`] measures progress from THIS instant, matching the
    /// fixed-step-plus-interpolation recipe's own "how far past the last
    /// completed tick is render time right now" definition
    /// (`gate4agent_arcade_engine::tick_alpha`'s own doc comment).
    last_tick_at: Instant,
}

impl fmt::Debug for PetArcade {
    /// Deliberately does not inspect `screen`/`shell` -- neither
    /// `GameScreen` nor `ArcadeShell` implement `Debug` (see this module's
    /// own top doc comment), and this crate's own "never panic in library
    /// code" discipline rules out reaching for their private internals
    /// even if it were possible to.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PetArcade")
            .field("selected_tile", &self.selected_tile)
            .field("selected_anchor", &self.selected_anchor)
            .field("selected_tower_kind", &self.selected_tower_kind)
            .field("inspect", &self.inspect)
            .field("difficulty", &self.difficulty)
            .finish_non_exhaustive()
    }
}

impl PetArcade {
    pub(crate) fn new() -> Rc<RefCell<Self>> {
        let mut shell = ArcadeShell::new(catalog());
        shell.select(Simulation::ID);
        Rc::new(RefCell::new(Self {
            shell,
            screen: GameScreen::Home,
            pending: Vec::new(),
            last_advance: Instant::now(),
            selected_tile: Tile::new(0, 0),
            selected_anchor: 0,
            selected_tower_kind: TowerKind::ALL[0],
            inspect: None,
            difficulty: Difficulty::Standard,
            presenter: FramePresenter::new(),
            last_tick_at: Instant::now(),
        }))
    }

    fn start_run(&mut self) {
        let mut source = UnlimitedAdmission;
        let params = PetBastionParams::new(self.difficulty);
        self.presenter = FramePresenter::new();
        if let Ok(runner) = Runner::start(&mut source, AdmissionCredit(1), fresh_seed(), params) {
            // Seed the cursor on a real build TARGET from the very first
            // frame rather than an arbitrary `(0, 0)` -- `build_cells`'s
            // own first entry is not good enough on its own: it is
            // whichever near-route tile sorts first in raster order,
            // which is just as likely to be a route/anchor tile
            // (`is_build_target` rejects both) as a genuine build
            // candidate. This map always has at least one real candidate
            // (`board.rs`'s own test asserts as much), so this only ever
            // falls back to `(0, 0)` if that ever stops being true, never
            // panics either way.
            let snapshot: SimulationSnapshot = runner.snapshot();
            self.selected_tile = snapshot
                .build_cells
                .iter()
                .find(|cell| is_build_target(cell.reason))
                .map(|cell| Tile::new(cell.tile.0, cell.tile.1))
                .unwrap_or(Tile::new(0, 0));
            // Seeds `presenter`'s own `curr` immediately, `prev: None` --
            // see this struct's own `presenter` field doc comment for why
            // this eager push exists (a fresh run has no earlier tick to
            // interpolate FROM yet, and `interp::interpolated_dynamic_
            // sprites` already degrades correctly for that case).
            self.presenter.push_tick(snapshot);
            self.screen = GameScreen::InRun(runner);
        }
        self.pending.clear();
        self.selected_anchor = 0;
        self.selected_tower_kind = TowerKind::ALL[0];
        self.inspect = None;
        self.last_advance = Instant::now();
        self.last_tick_at = Instant::now();
    }

    /// Called when the overlay opens: starts a fresh run from `Home`,
    /// resumes an already-running one exactly where it was left, or
    /// leaves a finished run's own `Results` screen alone (`r` restarts it
    /// explicitly -- see [`Self::handle_key`]).
    pub(crate) fn resume(&mut self) {
        if matches!(self.screen, GameScreen::Home) {
            self.start_run();
        }
        self.screen.set_suspended(false);
        self.last_advance = Instant::now();
    }

    /// Called when the overlay closes: freezes the run in place
    /// (`Runner::set_paused` clears its own elapsed-time accumulator on
    /// this exact transition, never a partial carry -- see that fn's own
    /// doc comment) rather than dropping it, so reopening the modal
    /// resumes the SAME build/wave, not a fresh run.
    pub(crate) fn suspend(&mut self) {
        self.screen.set_suspended(true);
    }

    pub(crate) fn is_running(&self) -> bool {
        matches!(self.screen, GameScreen::InRun(_))
    }

    /// The wall-clock integrator -- reads real elapsed time directly, the
    /// same "step state, then render" shape `App::step_pet` already uses.
    /// Applies the WHOLE `pending` batch on the first tick that actually
    /// runs this call (`Runner::advance`'s own documented batching
    /// contract) and clears it ONLY when `Runner::advance` reports a tick
    /// ran -- most calls happen well inside one 50ms tick, so a call that
    /// consumed nothing must leave `pending` queued for the next one,
    /// never silently drop input typed between ticks.
    pub(crate) fn advance(&mut self, now: Instant) {
        let GameScreen::InRun(runner) = &mut self.screen else {
            return;
        };
        let elapsed = now.saturating_duration_since(self.last_advance);
        self.last_advance = now;
        let ran = runner.advance(elapsed, &self.pending);
        if !ran {
            return;
        }
        self.pending.clear();
        self.last_tick_at = now;
        let snapshot = runner.snapshot();
        let outcome = match snapshot.phase {
            RunPhaseView::Victory => Some(RunOutcome::Won),
            RunPhaseView::Defeat => Some(RunOutcome::Lost),
            _ => None,
        };
        // Pushed AFTER `outcome` is already decided from `snapshot.phase`
        // (a plain `Copy` read, not a move) so this can hand `presenter`
        // ownership of `snapshot` outright rather than cloning it -- the
        // last real use of this particular snapshot.
        self.presenter.push_tick(snapshot);
        if let Some(outcome) = outcome {
            let final_hash = runner.stable_hash();
            self.screen = GameScreen::Results { outcome, final_hash };
        }
    }

    /// The pixel tier's own interpolation input -- see `presenter`'s own
    /// field doc comment. `render::render_pet_arcade` reads `.previous()`
    /// off this to pair with whatever `Self::snapshot()` already returned
    /// as the CURRENT side of the lerp (the two are tick-identical
    /// whenever both are `Some`: nothing between one `Self::snapshot()`
    /// call and the next ever advances the sim, since rendering never
    /// calls [`Self::advance`]).
    pub(crate) fn presenter(&self) -> &FramePresenter {
        &self.presenter
    }

    /// How far real (wall-clock) time has progressed past `presenter`'s
    /// own last pushed tick, `0.0..=1.0` -- `now` is the caller's own read,
    /// never re-queried here, so a caller (the real render loop, or a
    /// test's own hand-advanced clock) controls exactly which instant this
    /// is measured against. See [`gate4agent_arcade_engine::tick_alpha`]'s
    /// own doc comment for the underlying arithmetic and its `0.0`/`1.0`
    /// clamping.
    pub(crate) fn tick_alpha(&self, now: Instant) -> f64 {
        gate4agent_arcade_engine::tick_alpha(now.saturating_duration_since(self.last_tick_at), TICK_INTERVAL)
    }

    fn queue(&mut self, command: Command) {
        if matches!(self.screen, GameScreen::InRun(_)) {
            self.pending.push(command);
        }
    }

    /// The tower currently standing on `selected_tile`, if any -- the ONE
    /// place both the keyboard's own `u`/`p`/`o`/`x` bindings and this
    /// module's own mouse click handlers (`click_upgrade_l2`/`click_
    /// upgrade_l3`/`click_sell`) resolve "the selected tower" against, and
    /// what `render::render_pet_arcade_hud`'s own context card reads to
    /// decide which action rows to draw. Returns the full [`TowerView`]
    /// (not just its id) so the renderer never needs a second lookup to
    /// show that tower's own kind/level.
    pub(crate) fn selected_tower_view<'a>(&self, snapshot: &'a SimulationSnapshot) -> Option<&'a TowerView> {
        snapshot
            .towers
            .iter()
            .find(|tower| tower.position == (self.selected_tile.x, self.selected_tile.y))
    }

    /// Moves `selected_tile` to the next (`forward`) or previous build-
    /// radius cell in `snapshot.build_cells`'s own order -- the `Tab`/
    /// `BackTab` keyboard bindings' shared implementation. Wraps at either
    /// end; a cursor that is not currently ON a listed cell (only possible
    /// immediately after `start_run`'s own seed, before this ever ran)
    /// falls back to the list's first entry rather than doing nothing.
    fn cycle_selected_tile(&mut self, snapshot: &SimulationSnapshot, forward: bool) {
        let cells = &snapshot.build_cells;
        if cells.is_empty() {
            return;
        }
        let current = cells
            .iter()
            .position(|cell| cell.tile == (self.selected_tile.x, self.selected_tile.y));
        let len = cells.len();
        let next_index = match current {
            Some(index) if forward => (index + 1) % len,
            Some(index) => (index + len - 1) % len,
            None => 0,
        };
        let tile = cells[next_index].tile;
        self.selected_tile = Tile::new(tile.0, tile.1);
    }

    /// Every keyboard binding this overlay recognizes -- see `render::
    /// render_pet_arcade`'s own hint line for the player-facing legend,
    /// kept in sync by hand against this exact match.
    pub(crate) fn handle_key(&mut self, key: UiKey) {
        match &self.screen {
            GameScreen::Home => {
                if matches!(key, UiKey::Enter | UiKey::Char(' ')) {
                    self.start_run();
                }
            }
            GameScreen::Results { .. } => {
                if matches!(key, UiKey::Char('r') | UiKey::Char('R')) {
                    self.start_run();
                }
            }
            GameScreen::InRun(_) => {}
        }
        let snapshot = match &self.screen {
            GameScreen::InRun(runner) => runner.snapshot(),
            _ => return,
        };
        self.handle_run_key(key, &snapshot);
    }

    fn handle_run_key(&mut self, key: UiKey, snapshot: &SimulationSnapshot) {
        match key {
            UiKey::Tab => self.cycle_selected_tile(snapshot, true),
            UiKey::BackTab => self.cycle_selected_tile(snapshot, false),
            UiKey::Char('[') => {
                self.selected_anchor =
                    (self.selected_anchor + ANCHOR_COUNT as u8 - 1) % ANCHOR_COUNT as u8;
            }
            UiKey::Char(']') => {
                self.selected_anchor = (self.selected_anchor + 1) % ANCHOR_COUNT as u8;
            }
            UiKey::Char(digit @ '1'..='6') => {
                let index = digit as usize - '1' as usize;
                self.queue(Command::Place { tile: self.selected_tile, kind: TowerKind::ALL[index] });
            }
            UiKey::Char('u') | UiKey::Char('U') => {
                if let Some(tower) = self.selected_tower_view(snapshot) {
                    self.queue(Command::UpgradeToL2 { tower: tower.id });
                }
            }
            UiKey::Char('p') | UiKey::Char('P') => {
                if let Some(tower) = self.selected_tower_view(snapshot) {
                    self.queue(Command::UpgradeToL3 { tower: tower.id, branch: UpgradeBranch::Power });
                }
            }
            UiKey::Char('o') | UiKey::Char('O') => {
                if let Some(tower) = self.selected_tower_view(snapshot) {
                    self.queue(Command::UpgradeToL3 { tower: tower.id, branch: UpgradeBranch::Utility });
                }
            }
            UiKey::Char('x') | UiKey::Char('X') => {
                if let Some(tower) = self.selected_tower_view(snapshot) {
                    self.queue(Command::Sell { tower: tower.id });
                }
            }
            UiKey::Char('m') | UiKey::Char('M') => {
                self.queue(Command::MovePet { anchor: AnchorId(self.selected_anchor) });
            }
            UiKey::Char('b') | UiKey::Char('B') => {
                self.queue(Command::Blink { anchor: AnchorId(self.selected_anchor) });
            }
            UiKey::Char('g') | UiKey::Char('G') => {
                self.queue(Command::PetPulse);
            }
            UiKey::Char('f') | UiKey::Char('F') => {
                self.queue(Command::FullCircuit);
            }
            UiKey::Char(' ') => {
                self.queue(Command::StartWave);
            }
            UiKey::Function(number @ 1..=3) => match snapshot.phase {
                RunPhaseView::RuneDraft => {
                    if let Some(rune) = snapshot.rune_options.get(usize::from(number - 1)).copied() {
                        self.queue(Command::DraftRune(rune));
                    }
                }
                RunPhaseView::EvolutionChoice => {
                    let evolution = match number {
                        1 => Evolution::Moth,
                        2 => Evolution::Crab,
                        _ => Evolution::Wisp,
                    };
                    self.queue(Command::ChooseEvolution(evolution));
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Resolves a click on board tile `(x, y)` -- the mouse counterpart to
    /// `Tab`/`[`/`]` plus whatever action key the cursor they moved was
    /// aimed at. Checked in this fixed priority order: a build TARGET cell
    /// (`snapshot.build_cells`, filtered by [`is_build_target`] -- empty
    /// and eligible, or already holding another tower, but never a route/
    /// anchor/out-of-zone cell that merely happens to sit inside the build
    /// radius) always selects (never places -- placement is a second
    /// click, on a palette row: [`Self::click_tower_kind`]), an anchor
    /// selects AND immediately repositions the pet there (one click, not
    /// two -- there is no "confirm" step for a move the way there is for a
    /// placement), and anything else is checked against the current
    /// snapshot's own live enemies/boss for the inspector. A tile that
    /// matches none of these (open route, Heartseed, blank board) is a
    /// silent no-op, the same "click landed somewhere with no assigned
    /// meaning" convention `App::click`'s own catch-all arms already use
    /// throughout this crate.
    pub(crate) fn click_tile(&mut self, x: u8, y: u8) {
        let Some(snapshot) = self.snapshot() else { return };
        let tile = Tile::new(i32::from(x), i32::from(y));

        let is_build_target_tile = snapshot
            .build_cells
            .iter()
            .any(|cell| cell.tile == (tile.x, tile.y) && is_build_target(cell.reason));
        if is_build_target_tile {
            self.selected_tile = tile;
            self.inspect = None;
            return;
        }
        if let Some(anchor) = ANCHORS.iter().position(|&a| a == tile) {
            self.selected_anchor = anchor as u8;
            self.inspect = None;
            self.queue(Command::MovePet { anchor: AnchorId(anchor as u8) });
            return;
        }
        if let Some(enemy) = snapshot
            .enemies
            .iter()
            .find(|enemy| round_to_tile(enemy.position) == (tile.x, tile.y))
        {
            self.inspect = Some(Inspect::Enemy(enemy.id));
            return;
        }
        if let Some(boss) = &snapshot.boss {
            if boss.bodies.iter().any(|body| round_to_tile(body.position) == (tile.x, tile.y)) {
                self.inspect = Some(Inspect::Boss);
            }
        }
    }

    /// One click on the tower palette (`render::render_pet_arcade_hud`'s
    /// own "Towers:" rows): always selects `kind` (so the palette's own
    /// highlight follows the click even when nothing gets placed), and
    /// places it on `selected_tile` in the SAME click when that tile is
    /// currently free -- the sim's own `place_tower` already no-ops for an
    /// occupied/ineligible tile or insufficient Sap (see that fn's own doc
    /// comment in `sim.rs`), so this never needs to duplicate either check
    /// to decide whether to queue the command.
    pub(crate) fn click_tower_kind(&mut self, kind: TowerKind) {
        self.selected_tower_kind = kind;
        let Some(snapshot) = self.snapshot() else { return };
        if self.selected_tower_view(&snapshot).is_none() {
            self.queue(Command::Place { tile: self.selected_tile, kind });
        }
    }

    /// The context card's own `[U] Upgrade L2` row -- mouse counterpart to
    /// the `u` key, see [`Self::selected_tower_view`]'s own doc comment.
    pub(crate) fn click_upgrade_l2(&mut self) {
        let Some(snapshot) = self.snapshot() else { return };
        if let Some(tower) = self.selected_tower_view(&snapshot) {
            self.queue(Command::UpgradeToL2 { tower: tower.id });
        }
    }

    /// The context card's own `[P] L3 Power` / `[O] L3 Utility` rows --
    /// mouse counterpart to the `p`/`o` keys.
    pub(crate) fn click_upgrade_l3(&mut self, branch: UpgradeBranch) {
        let Some(snapshot) = self.snapshot() else { return };
        if let Some(tower) = self.selected_tower_view(&snapshot) {
            self.queue(Command::UpgradeToL3 { tower: tower.id, branch });
        }
    }

    /// The context card's own `[X] Sell` row -- mouse counterpart to the
    /// `x` key.
    pub(crate) fn click_sell(&mut self) {
        let Some(snapshot) = self.snapshot() else { return };
        if let Some(tower) = self.selected_tower_view(&snapshot) {
            self.queue(Command::Sell { tower: tower.id });
        }
    }

    /// A click on the header's own wave/phase line -- opens the wave
    /// inspector card. See [`Inspect::Wave`]'s own doc comment for why its
    /// card cannot show a composition list yet.
    pub(crate) fn inspect_wave(&mut self) {
        self.inspect = Some(Inspect::Wave);
    }

    /// The HUD's own `[G] Pulse` button -- mouse counterpart to the `g`
    /// key. Queued unconditionally, exactly like every other action button
    /// in this crate (`click_upgrade_l2`, the tower palette's own
    /// `click_tower_kind`, ...): the sim already no-ops a `PetPulse` it
    /// cannot afford (`sim.rs`'s own `use_pet_pulse`, which checks Spark
    /// and requires the pet be standing at an anchor before it spends
    /// any), so there is no second affordability check to duplicate here --
    /// only the render side (`render::render_pet_arcade_hud`, via
    /// [`Self::can_pet_pulse`]) needs to know the cost, to grey the button
    /// out.
    pub(crate) fn click_pet_pulse(&mut self) {
        self.queue(Command::PetPulse);
    }

    /// The HUD's own `[B] Blink` button -- mouse counterpart to the `b`
    /// key, targeting whatever anchor `Tab`/`[`/`]`/an anchor-tile click
    /// most recently selected (`selected_anchor`), same as the keyboard
    /// binding.
    pub(crate) fn click_blink(&mut self) {
        self.queue(Command::Blink { anchor: AnchorId(self.selected_anchor) });
    }

    /// The HUD's own `[F] Circuit` button -- mouse counterpart to the `f`
    /// key.
    pub(crate) fn click_full_circuit(&mut self) {
        self.queue(Command::FullCircuit);
    }

    /// The HUD's own explicit `[M] Move Pet` button -- mouse counterpart
    /// to the `m` key AND to clicking an anchor tile directly
    /// (`click_tile`'s own anchor arm), which already both select the
    /// anchor AND queue the move in one click. This button exists for the
    /// case the owner asked for explicitly: reposition the pet to whatever
    /// anchor is ALREADY selected without needing to re-click that same
    /// tile.
    pub(crate) fn click_move_pet(&mut self) {
        self.queue(Command::MovePet { anchor: AnchorId(self.selected_anchor) });
    }

    /// The HUD's own `[Space] Start Wave` button -- mouse counterpart to
    /// the `Space` key. Queued unconditionally; `apply_command`'s own
    /// `Command::StartWave` arm already no-ops outside the Build phase, the
    /// same "sim gates it, render just greys it" split every other action
    /// button here uses (see [`Self::can_start_wave`]).
    pub(crate) fn click_start_wave(&mut self) {
        self.queue(Command::StartWave);
    }

    /// One of the rune-draft card's own `F1`-`F3` options, clicked -- mouse
    /// counterpart to the keyboard's own `UiKey::Function` handling in
    /// [`Self::handle_run_key`]. Reachable only while `render_pet_arcade_
    /// context`'s own `RuneDraft` arm actually painted this rune (that arm
    /// is the only place `HitTarget::PetArcadeDraftRune` is ever pushed),
    /// so there is no separate phase check to duplicate here either.
    pub(crate) fn click_draft_rune(&mut self, rune: Rune) {
        self.queue(Command::DraftRune(rune));
    }

    /// One of the evolution-choice card's own `F1`-`F3` options, clicked --
    /// see [`Self::click_draft_rune`]'s own doc comment for the matching
    /// rationale.
    pub(crate) fn click_choose_evolution(&mut self, evolution: Evolution) {
        self.queue(Command::ChooseEvolution(evolution));
    }

    /// The Results screen's own `[R] Play Again` button -- mouse
    /// counterpart to the `r` key (see [`Self::handle_key`]'s own
    /// `Results` arm). Guarded the same way that arm is: a click reaching
    /// this while the screen is NOT `Results` (should never happen --
    /// `HitTarget::PetArcadeRestart` is only ever pushed by `render_pet_
    /// arcade_results`) is a silent no-op rather than restarting an
    /// in-progress run out from under the player.
    pub(crate) fn click_restart(&mut self) {
        if matches!(self.screen, GameScreen::Results { .. }) {
            self.start_run();
        }
    }

    /// A drag-and-drop tower placement landing on board tile `(x, y)` --
    /// the pointer's own counterpart to the palette click's "place on the
    /// already-selected tile" shortcut (`click_tower_kind`), except this
    /// one places wherever the player actually dropped it. Queued
    /// unconditionally, same as every other action here: `place_tower`'s
    /// own eligibility/occupied/cost checks already cover a drop on
    /// anything else (open route, another tower's own tile, out of Sap),
    /// so this never charges Sap for a cancelled drop.
    pub(crate) fn drop_tower(&mut self, kind: TowerKind, x: u8, y: u8) {
        let tile = Tile::new(i32::from(x), i32::from(y));
        self.queue(Command::Place { tile, kind });
    }

    /// `true` once the pet has enough Spark banked to afford a Pulse AND is
    /// actually standing at an anchor (`use_pet_pulse`'s own two guards,
    /// mirrored here so the button can grey out for either reason instead
    /// of silently doing nothing when clicked while it cannot fire).
    pub(crate) fn can_pet_pulse(snapshot: &SimulationSnapshot) -> bool {
        snapshot.pet.spark >= PET_PULSE_COST && matches!(snapshot.pet.state, PetState::AtAnchor(_))
    }

    /// `true` once a Blink is affordable -- Wisp's own evolution perk
    /// (free blinks) or enough Spark for the flat cost, exactly
    /// `apply_command`'s own `Command::Blink` guard.
    pub(crate) fn can_blink(snapshot: &SimulationSnapshot) -> bool {
        snapshot.pet.evolution == Some(Evolution::Wisp) || snapshot.pet.spark >= BLINK_COST
    }

    /// `true` once Full Circuit is affordable.
    pub(crate) fn can_full_circuit(snapshot: &SimulationSnapshot) -> bool {
        snapshot.pet.spark >= FULL_CIRCUIT_COST
    }

    /// `true` only during the Build phase -- `apply_command`'s own
    /// `Command::StartWave` guard, mirrored so the button can grey out
    /// mid-Combat instead of silently doing nothing.
    pub(crate) fn can_start_wave(snapshot: &SimulationSnapshot) -> bool {
        matches!(snapshot.phase, RunPhaseView::Build { .. })
    }

    /// `ArcadeShell::negotiate_size` -- "that layer owns sizing
    /// negotiation" (see `gate4agent_arcade_engine::shell`'s own module
    /// doc). `None` means the terminal has no room for this game at all.
    pub(crate) fn negotiate_size(&self, available: CellArea) -> Option<CellArea> {
        self.shell.negotiate_size(&self.screen, available)
    }

    /// `Some` only while a run is actually in progress -- `Home`/`Results`
    /// have no board state to paint.
    pub(crate) fn snapshot(&self) -> Option<SimulationSnapshot> {
        match &self.screen {
            GameScreen::InRun(runner) => Some(runner.snapshot()),
            _ => None,
        }
    }

    pub(crate) fn results(&self) -> Option<(RunOutcome, u64)> {
        match &self.screen {
            GameScreen::Results { outcome, final_hash } => Some((*outcome, *final_hash)),
            _ => None,
        }
    }

    pub(crate) fn selected_tile(&self) -> (u8, u8) {
        (self.selected_tile.x as u8, self.selected_tile.y as u8)
    }

    pub(crate) fn selected_anchor(&self) -> u8 {
        self.selected_anchor
    }

    /// The tower palette's own current selection -- see `selected_tower_
    /// kind`'s own field doc comment.
    pub(crate) fn selected_tower_kind(&self) -> TowerKind {
        self.selected_tower_kind
    }

    /// What the inspector card is currently showing, if anything -- see
    /// [`Inspect`]'s own doc comment.
    pub(crate) fn inspect(&self) -> Option<Inspect> {
        self.inspect
    }

    /// Test-only window onto `pending` -- the ONE way a click-driven test
    /// can prove a button's own handler ran and queued the RIGHT command,
    /// for the several action buttons whose real-money cost this crate's
    /// own test fixtures cannot legitimately afford without first playing
    /// through live combat for kill-reward Sap (`render::tests`' own doc
    /// comments on each such test explain exactly which and why). Never
    /// compiled into a real build.
    #[cfg(test)]
    pub(crate) fn pending_commands_for_test(&self) -> &[Command] {
        &self.pending
    }

    /// Test-only seam that jumps straight to the Results screen without
    /// requiring a full, real playthrough to actually win or lose one --
    /// `GameScreen::Results`'s own two fields (`RunOutcome`, a hash) are
    /// both plain, publicly constructible values (unlike `GameScreen::
    /// InRun`, which wraps the engine's own opaque `Runner` and so cannot
    /// be hand-built from outside `gate4agent-arcade` at all -- see this
    /// module's own top doc comment). Never compiled into a real build.
    #[cfg(test)]
    pub(crate) fn force_results_for_test(&mut self, outcome: RunOutcome) {
        self.screen = GameScreen::Results { outcome, final_hash: 0 };
    }
}

/// Whether a `BuildCellView::reason` still marks its tile as a genuine
/// build TARGET -- empty and eligible (`None`) or already holding another
/// tower (`Some(Occupied)`), as opposed to a cell that merely sits inside
/// the build radius for an unrelated reason (on a route, on a pet anchor,
/// or -- `static_build_reason`'s own `OutOfZone` arm -- not really in the
/// zone at all). Shared by [`PetArcade::click_tile`] (which of the many
/// `build_cells` entries a click may select) and [`buildable_tiles`]
/// (which of them a tower drag still shows at all).
fn is_build_target(reason: Option<BuildIneligibleReason>) -> bool {
    matches!(reason, None | Some(BuildIneligibleReason::Occupied))
}

/// Every build-radius cell (`SimulationSnapshot::build_cells`) worth
/// showing to a tower-placement drag right now, paired with whether
/// dropping `kind` THERE would actually place it -- the one place both the
/// drag's own board-cell highlight (`render::render_pet_arcade`) and its
/// ghost read "where can this go" today. Reads `snapshot.build_cells`
/// directly rather than any host-side pad table, so this stays correct
/// automatically the next time that field's own eligibility rules change --
/// nothing here hardcodes a tile count or a fixed candidate set. Cells
/// [`is_build_target`] excludes (route, anchor, out of the build radius
/// entirely) are left out here too: they are not genuine "almost worked"
/// candidates, and are already visually obvious as routes/anchors on the
/// board itself.
pub(crate) fn buildable_tiles(snapshot: &SimulationSnapshot, kind: TowerKind) -> Vec<(Tile, BuildDragState)> {
    let cost = kind.base_stats().cost;
    snapshot
        .build_cells
        .iter()
        .filter(|cell| is_build_target(cell.reason))
        .map(|cell| {
            let tile = Tile::new(cell.tile.0, cell.tile.1);
            let state = match cell.reason {
                None if cost <= snapshot.sap => BuildDragState::Buildable,
                None => BuildDragState::Unaffordable,
                Some(_) => BuildDragState::Occupied,
            };
            (tile, state)
        })
        .collect()
}

/// Rounds a continuous fixed-point board position to its nearest tile,
/// round-half-up, integer-only -- the SAME formula `gate4agent-arcade-pet-
/// bastion-render`'s own (private, not reusable from here) `round_to_tile`
/// uses to decide which cell an enemy/boss body's own glyph is painted on.
/// Click resolution (`PetArcade::click_tile`) needs the identical rounding
/// rule so a click lands on whatever entity is ACTUALLY drawn under the
/// pointer, never a second, independently-drifting approximation -- every
/// coordinate this fn ever rounds is non-negative (board positions never go
/// negative), so there is no away-from-zero-vs-toward-zero ambiguity to
/// guard against.
fn round_to_tile(pos: FixedPos) -> (i32, i32) {
    let x = (pos.x + FIXED_SCALE / 2) / FIXED_SCALE;
    let y = (pos.y + FIXED_SCALE / 2) / FIXED_SCALE;
    (x as i32, y as i32)
}
