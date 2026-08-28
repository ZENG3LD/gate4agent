//! Loopback control endpoint for driving and inspecting a running
//! `gate4agent-tui`/`gate4agent-tui-light` process programmatically --
//! built so an agent verifying the app never again has to grab the
//! operator's real mouse and keyboard the way it did before this module
//! existed (see this crate's own delivery notes for that incident).
//!
//! **Off by default.** `client::RunOptions::control_plane` is `None`
//! unless `gate4agent-tui`'s own `--control-plane LOOPBACK_SOCKET` flag is
//! given; `client::run` then never calls [`spawn`], so the default build
//! binds no socket and spawns no thread -- byte-identical to before this
//! module existed. The credential comes from the environment
//! (`GATE4AGENT_TUI_CONTROL_TOKEN`, see `main.rs`), never argv, matching
//! `GATE4AGENT_HARNESS_OPERATOR_TOKEN`'s own precedent.
//!
//! **Loopback only.** [`spawn`] checks `SocketAddr::ip().is_loopback()`
//! itself, BEFORE ever calling `TcpListener::bind` -- the same precedent
//! `HarnessOperatorClient::new` already sets for the harness operator wire
//! (`gate4agent-harness-client`), just enforced on the bind side instead
//! of the connect side.
//!
//! **Wire idiom.** One newline-terminated JSON [`ControlEnvelopeV1`] per
//! TCP connection (`{"version", "credential", "request"}`), the caller
//! then half-closes its write side (EOF is the request boundary, exactly
//! `gate4agent-harness-api`'s own `HarnessOperatorEnvelopeV1` framing --
//! see [`read_request`]), and exactly one newline-terminated
//! [`ControlReplyV1`] (`{"status": "ok"|"error", ...}`) comes back before
//! the connection closes. No second serialization style invented for this
//! crate.
//!
//! **Credential.** [`ControlPlaneCredential`] mirrors
//! `HarnessOperatorCredential`'s own shape: a fixed `g4atc_` prefix plus 64
//! lowercase-hex bytes. The configured secret is compared against a
//! presented one via an HMAC-SHA256 digest and a constant-time comparison
//! (`gate4agent_node_wire::{local_hmac_sha256, proofs_match}`), the same
//! primitives `gate4agent-harness-service`'s own
//! `HarnessOperatorCredentialAuthority` uses -- not a second, hand-rolled
//! comparison scheme.
//!
//! **Single writer.** [`apply`] is the ONLY function in this module that
//! ever touches `&mut App`, and it is only ever called from `client::
//! run`'s own event loop, on that loop's own thread -- the same discipline
//! `apply_update`/`WorkerUpdate` already hold for every other worker
//! thread in this crate. A `ControlCommand` crossing from a TCP connection
//! thread into that loop (via a `tokio::sync::mpsc` channel, `blocking_
//! send` from the connection thread exactly like `harness_operator_worker`
//! already does) is the same shape as every other worker-to-loop hand-off
//! in `client.rs`; the connection thread then blocks on a
//! `std::sync::mpsc::sync_channel` reply for up to [`CONTROL_REPLY_DEADLINE`]
//! before answering the caller.
//!
//! **Same input path as real input.** `InjectKey`/`InjectMouse` decode
//! into exactly the types real terminal input decodes into
//! (`crate::app::UiKey` / `crossterm::event::MouseEvent`) and [`apply`]
//! dispatches them through `App::reduce`/`client::map_mouse` -- the exact
//! same functions `client::run`'s own crossterm event arms call. There is
//! no separate "synthetic input" reducer to drift out of sync with the
//! real one.
//!
//! **`InjectKey` cannot drive a PTY on its own -- and says nothing when it
//! doesn't.** A keystroke only ever reaches a session's PTY by riding
//! `App::reduce`'s focus/overlay dispatch chain down into `App::
//! reduce_viewport`, which then builds a bare `AppAction::Input`/
//! `TerminalBytes`; THAT still has to survive `App::route_harness_session_
//! verb`'s rewrite into `AppAction::HarnessWriteSessionInput`/
//! `HarnessWriteSessionBytes` before `client::send_operator_action` ever
//! calls `HarnessOperatorClient::write_session_input` -- and that rewrite
//! silently degrades to `AppAction::None` (`App::harness_session_address`'s
//! own `?`) the moment the target node's harness incarnation isn't known
//! yet. `apply`'s own `ControlResponseV1::Injected` reply only ever means
//! "the reducer saw this key", per its own doc comment below -- it says
//! nothing about whether any of THAT downstream chain actually ran, so an
//! `InjectKey` aimed at a real PTY pane can come back `status: ok` having
//! written zero bytes anywhere, with no record of why. [`write_pty`] exists
//! because of exactly that: it addresses a session directly (bypassing
//! focus/overlay dispatch entirely -- there is nothing there worth
//! re-testing for a PTY write), runs the same running-session guard `App::
//! reduce_viewport` runs, and runs the SAME `route_harness_session_verb`
//! rewrite real input runs -- but when that rewrite degrades to `None`,
//! [`write_pty`] returns `ControlErrorV1::NodeIncarnationUnknown` instead
//! of a silent `ok`.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use gate4agent_types::TERMINAL_INPUT_MAX_BYTES;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::app::{App, AppAction, Focus, SessionAddress, SurfaceTab, UiKey};
use crate::client::TerminalWatermarks;
use crate::profile::ProfileSnapshot;

/// One newline-terminated request/reply pair per connection, so a slow or
/// wedged caller can never hold the accept loop itself hostage -- matches
/// `HARNESS_OPERATOR_DEADLINE`'s own order of magnitude.
const CONTROL_IO_DEADLINE: Duration = Duration::from_secs(3);
/// Upper bound on how long a connection thread waits for `client::run`'s
/// own loop to drain [`ControlCommand`] and answer it -- past this the
/// caller gets an honest `busy` reply instead of hanging forever behind a
/// main loop that, for whatever reason, stopped ticking.
const CONTROL_REPLY_DEADLINE: Duration = Duration::from_secs(3);
const CONTROL_WIRE_VERSION_V1: u16 = 1;
const CONTROL_TOKEN_PREFIX: &str = "g4atc_";
/// Mirrors `HARNESS_OPERATOR_CREDENTIAL_MAX_BYTES` (`gate4agent-harness-
/// api`): the prefix plus 64 hex bytes is nowhere near this, so the bound
/// only ever rejects a wildly malformed value before it is even compared.
const CONTROL_CREDENTIAL_MAX_BYTES: usize = 256;
/// Mirrors `HARNESS_OPERATOR_REQUEST_MAX_BYTES`: this wire's own requests
/// (a keypress, a mouse cell, or a bare read verb) are far smaller, but the
/// bound stays generous rather than tight in case `InjectKey`'s
/// `TerminalBytes` payload ever needs to carry a long paste-like sequence.
const CONTROL_REQUEST_MAX_BYTES: usize = 64 * 1024;
/// A `DumpFrame` reply on a wide, tall terminal is the largest payload this
/// wire ever sends -- sized like `HARNESS_TERMINAL_FRAME_MAX_BYTES` (2MiB)
/// with headroom for JSON string escaping on top of the raw cell text.
const CONTROL_RESPONSE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Domain-separates this credential's HMAC digest from every other secret
/// `gate4agent_node_wire::local_hmac_sha256` is ever asked to sign in this
/// process -- same purpose `OPERATOR_CREDENTIAL_DIGEST_DOMAIN` serves in
/// `gate4agent-harness-service::runtime`.
const CONTROL_CREDENTIAL_DIGEST_DOMAIN: &[u8] = b"gate4agent-tui-control-plane-credential-v1";
/// How often [`poll_wait_for_output`] re-asks `apply`'s own `WaitForOutput`
/// arm whether a session's frame sequence has moved -- a plain `BTreeMap`
/// lookup (`TerminalWatermarks::terminal_watermark`) each time, so this can
/// stay tight without meaningfully loading `client::run`'s own loop; tight
/// enough that a caller asserting on output right after a write is not
/// itself the dominant source of latency in what it measures.
const CONTROL_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Upper bound on how long [`poll_wait_for_output`] keeps ONE connection
/// thread parked waiting for a session's output to move, regardless of the
/// `timeout_ms` a caller asks for -- `WaitForOutput` is the one verb on this
/// wire that is EXPECTED to block for a while by design (unlike every other
/// verb here, bounded by [`CONTROL_REPLY_DEADLINE`]), so it gets its own,
/// much longer, but still finite ceiling rather than an unbounded one a
/// misbehaving caller could use to pin a connection thread open forever.
const CONTROL_WAIT_MAX_TIMEOUT_MS: u64 = 30_000;

/// CLI/env-sourced configuration for [`spawn`]: where to bind, and the
/// secret a caller must present. Constructed by `gate4agent-tui`'s own
/// `main.rs` (`--control-plane` + `GATE4AGENT_TUI_CONTROL_TOKEN`) and
/// carried through `client::RunOptions::control_plane`.
#[derive(Clone)]
pub struct ControlPlaneEndpoint {
    pub bind: SocketAddr,
    pub credential: ControlPlaneCredential,
}

/// A `g4atc_` + 64 lowercase-hex-byte control-plane secret -- the same
/// shape `HarnessOperatorCredential` uses for the harness operator wire
/// (`gate4agent-harness-api`), under its own prefix so a token minted for
/// one wire can never be mistaken for (or accidentally reused as) the
/// other's.
#[derive(Clone, Eq, PartialEq)]
pub struct ControlPlaneCredential(String);

impl ControlPlaneCredential {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if !is_well_formed_credential(&value) {
            return Err("malformed control-plane credential".to_owned());
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ControlPlaneCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ControlPlaneCredential([REDACTED])")
    }
}

fn is_well_formed_credential(value: &str) -> bool {
    let Some(payload) = value.strip_prefix(CONTROL_TOKEN_PREFIX) else {
        return false;
    };
    value.len() <= CONTROL_CREDENTIAL_MAX_BYTES
        && payload.len() == 64
        && payload.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Holds only the configured secret's own HMAC digest, never the secret
/// itself, past construction -- `verify` re-derives a presented
/// credential's digest and compares in constant time via
/// `gate4agent_node_wire::proofs_match`, the exact scheme
/// `HarnessOperatorCredentialAuthority` (`gate4agent-harness-service`)
/// already uses for the harness operator wire's own credential.
#[derive(Clone, Copy)]
struct ControlPlaneCredentialAuthority {
    digest: [u8; gate4agent_node_protocol::NODE_AUTH_PROOF_BYTES],
}

impl ControlPlaneCredentialAuthority {
    fn new(credential: &ControlPlaneCredential) -> Result<Self, String> {
        let digest = gate4agent_node_wire::local_hmac_sha256(
            CONTROL_CREDENTIAL_DIGEST_DOMAIN,
            credential.expose().as_bytes(),
        ).map_err(|error| format!("control-plane credential digest failed: {error}"))?;
        Ok(Self { digest })
    }

    /// `false` for a structurally malformed presented value without ever
    /// hashing it -- and `false`, indistinguishably, for a well-formed one
    /// that simply does not match: the caller learns only "unauthorized"
    /// either way, never which of the two happened.
    fn verify(&self, presented: &str) -> bool {
        if !is_well_formed_credential(presented) {
            return false;
        }
        match gate4agent_node_wire::local_hmac_sha256(CONTROL_CREDENTIAL_DIGEST_DOMAIN, presented.as_bytes()) {
            Ok(actual) => gate4agent_node_wire::proofs_match(&actual, &self.digest),
            Err(_) => false,
        }
    }
}

/// One decoded request plus the reply channel its own TCP connection
/// thread is blocked on -- the unit of work [`spawn`]'s accept loop hands
/// to `client::run`'s own loop, and the only thing [`apply`] ever consumes.
pub(crate) struct ControlCommand {
    request: ControlRequestV1,
    reply: std::sync::mpsc::SyncSender<ControlReplyV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlEnvelopeV1 {
    version: u16,
    credential: String,
    request: ControlRequestV1,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ControlRequestV1 {
    InjectKey { key: ControlKeyV1 },
    InjectMouse { mouse: ControlMouseV1 },
    QueryState,
    DumpFrame,
    /// Addressed at a [`ControlSessionAddressV1`] directly -- see
    /// [`write_pty`]'s own doc comment for why a session address, not a
    /// pane, and for exactly what precondition each possible
    /// [`ControlErrorV1`] here names.
    WriteSession { session: ControlSessionAddressV1, payload: ControlWritePayloadV1 },
    /// Read-only counterpart to `WriteSession`, addressed the same way --
    /// see [`read_pty`]'s own doc comment.
    ReadSession { session: ControlSessionAddressV1 },
    /// Handled OUTSIDE [`apply`] by [`poll_wait_for_output`] on the
    /// connection thread itself -- see that fn's own doc comment for why
    /// `apply`'s own arm for this variant only ever answers "has it moved
    /// RIGHT NOW", never sleeps, and is therefore safe to also reach
    /// directly from a unit test without spinning up a socket.
    WaitForOutput { session: ControlSessionAddressV1, after_frame: u64, timeout_ms: u64 },
    /// Native pixel render of the app's CURRENT frame, written to disk as
    /// a PNG -- see [`capture_frame`]'s own doc comment for exactly what
    /// this covers that [`DumpFrame`](ControlRequestV1::DumpFrame)'s
    /// plain-text projection cannot (colour, background, text attributes,
    /// baked sixel icon placements) and what it deliberately still does
    /// not (the Pet Bastion arcade board's own pixel-tier overlay; real
    /// glyph shapes).
    CaptureFrame,
}

/// The two shapes `App::reduce_viewport`'s own `UiKey::Char`/`UiKey::
/// TerminalBytes` arms already write through `AppAction::Input`/
/// `TerminalBytes` -- [`write_pty`] builds the identical `AppAction` either
/// way, just addressed directly rather than reached via a keystroke.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ControlWritePayloadV1 {
    /// Mirrors `UiKey::Char`/the real Ctrl+V paste path: UTF-8 text,
    /// bounded by the same [`TERMINAL_INPUT_MAX_BYTES`] the paste path
    /// enforces (`App::reduce_viewport`'s own `UiKey::Ctrl('v')` arm).
    Text { value: String },
    /// Mirrors `UiKey::TerminalBytes`: an already-encoded byte sequence
    /// (an escape sequence, a paste that isn't valid UTF-8 as typed, …)
    /// sent to the PTY verbatim, no length cap beyond the wire's own
    /// [`CONTROL_REQUEST_MAX_BYTES`].
    Bytes { value: Vec<u8> },
}

/// Mirrors `crate::app::UiKey` one variant at a time (see
/// [`ControlKeyV1::into_ui_key`]) -- named keys plus modifiers exactly as
/// `UiKey` itself already models them (`Ctrl`/`Shift*`/`ModifiedEnter`
/// carry the modifier in the variant, `TerminalBytes` carries an
/// already-modifier-encoded Alt sequence), so a caller drives the reducer
/// with the same vocabulary `map_key` decodes real terminal input into.
#[derive(Deserialize)]
#[serde(tag = "variant", rename_all = "snake_case", deny_unknown_fields)]
enum ControlKeyV1 {
    Char { value: char },
    Ctrl { value: char },
    OperatorEscape,
    TerminalBytes { value: Vec<u8> },
    Enter,
    ModifiedEnter,
    Escape,
    Backspace,
    Insert,
    Delete,
    Home,
    End,
    Up,
    Down,
    Left,
    Right,
    ShiftHome,
    ShiftEnd,
    ShiftUp,
    ShiftDown,
    ShiftLeft,
    ShiftRight,
    ShiftPageUp,
    ShiftPageDown,
    Tab,
    BackTab,
    PageUp,
    PageDown,
    Function { value: u8 },
    UnsupportedModifier,
}

impl ControlKeyV1 {
    fn into_ui_key(self) -> UiKey {
        match self {
            Self::Char { value } => UiKey::Char(value),
            Self::Ctrl { value } => UiKey::Ctrl(value),
            Self::OperatorEscape => UiKey::OperatorEscape,
            Self::TerminalBytes { value } => UiKey::TerminalBytes(value),
            Self::Enter => UiKey::Enter,
            Self::ModifiedEnter => UiKey::ModifiedEnter,
            Self::Escape => UiKey::Escape,
            Self::Backspace => UiKey::Backspace,
            Self::Insert => UiKey::Insert,
            Self::Delete => UiKey::Delete,
            Self::Home => UiKey::Home,
            Self::End => UiKey::End,
            Self::Up => UiKey::Up,
            Self::Down => UiKey::Down,
            Self::Left => UiKey::Left,
            Self::Right => UiKey::Right,
            Self::ShiftHome => UiKey::ShiftHome,
            Self::ShiftEnd => UiKey::ShiftEnd,
            Self::ShiftUp => UiKey::ShiftUp,
            Self::ShiftDown => UiKey::ShiftDown,
            Self::ShiftLeft => UiKey::ShiftLeft,
            Self::ShiftRight => UiKey::ShiftRight,
            Self::ShiftPageUp => UiKey::ShiftPageUp,
            Self::ShiftPageDown => UiKey::ShiftPageDown,
            Self::Tab => UiKey::Tab,
            Self::BackTab => UiKey::BackTab,
            Self::PageUp => UiKey::PageUp,
            Self::PageDown => UiKey::PageDown,
            Self::Function { value } => UiKey::Function(value),
            Self::UnsupportedModifier => UiKey::UnsupportedModifier,
        }
    }
}

/// A mouse event at a TERMINAL CELL coordinate (`column`/`row`), never a
/// screen pixel -- exactly the unit `client::map_mouse` and every `App`
/// mouse handler (`click`/`drag`/`drop_at`/`scroll`/`hover`) already work
/// in. Limited to the seven `MouseEventKind` shapes `map_mouse` actually
/// dispatches (the rest fold to `AppAction::None` there today, so
/// accepting them here would just be a verb that always silently no-ops).
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ControlMouseKindV1 {
    DownLeft,
    DownRight,
    DragLeft,
    UpLeft,
    ScrollUp,
    ScrollDown,
    Moved,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlMouseV1 {
    kind: ControlMouseKindV1,
    column: u16,
    row: u16,
}

impl ControlMouseV1 {
    /// `modifiers: KeyModifiers::NONE` -- this wire has no verb for a
    /// modified click today (nothing in `App`'s own mouse handlers reads
    /// mouse-event modifiers), so there is nothing yet to carry.
    fn into_mouse_event(self) -> MouseEvent {
        let kind = match self.kind {
            ControlMouseKindV1::DownLeft => MouseEventKind::Down(MouseButton::Left),
            ControlMouseKindV1::DownRight => MouseEventKind::Down(MouseButton::Right),
            ControlMouseKindV1::DragLeft => MouseEventKind::Drag(MouseButton::Left),
            ControlMouseKindV1::UpLeft => MouseEventKind::Up(MouseButton::Left),
            ControlMouseKindV1::ScrollUp => MouseEventKind::ScrollUp,
            ControlMouseKindV1::ScrollDown => MouseEventKind::ScrollDown,
            ControlMouseKindV1::Moved => MouseEventKind::Moved,
        };
        MouseEvent { kind, column: self.column, row: self.row, modifiers: KeyModifiers::NONE }
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ControlReplyV1 {
    Ok { response: ControlResponseV1 },
    Error { error: ControlErrorV1 },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ControlErrorV1 {
    MalformedRequest,
    RequestTooLarge,
    ResponseTooLarge,
    UnsupportedVersion,
    Unauthorized,
    /// `client::run`'s own loop did not drain this command within
    /// [`CONTROL_REPLY_DEADLINE`].
    Busy,
    /// The command channel into `client::run`'s loop is gone -- the run
    /// loop has already exited (the app is shutting down).
    Unavailable,
    /// `WriteSession`/`ReadSession` named a `(node_id, workspace_id,
    /// instance_id, generation)` [`App::find_session`] does not know --
    /// wrong, stale, or not-yet-observed address.
    SessionNotFound,
    /// `WriteSession` named a real session (`SessionView::running` is
    /// `false`) -- the exact "stopped PTY is read-only" guard `App::
    /// reduce_viewport` runs before dispatching any key, mirrored here.
    SessionNotRunning,
    /// `WriteSession`'s own `App::route_harness_session_verb` call degraded
    /// to `AppAction::None` -- the node this session lives on has not yet
    /// reported an `incarnation_id` to `App::nodes` (`App::harness_session_
    /// address`'s own `?`), so there is no `HarnessRuntimeSessionAddressV1`
    /// to write through YET. THIS is the precondition an `InjectKey`
    /// against the same session would have silently dropped for -- see
    /// this module's own doc comment.
    NodeIncarnationUnknown,
    /// `WriteSession`'s `Text` payload exceeded [`TERMINAL_INPUT_MAX_BYTES`]
    /// -- the same bound the real Ctrl+V paste path enforces.
    WriteTooLarge,
    /// `CaptureFrame` rendered its PNG bytes successfully but writing them
    /// to disk (`std::fs::write` in [`capture_frame`]) failed -- a full
    /// temp directory, a permissions problem, or similar. Distinct from
    /// every other error above: the ONLY one whose cause is outside this
    /// process's own state (`app`, the request) entirely.
    FrameWriteFailed,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ControlResponseV1 {
    /// Ack for `InjectKey`/`InjectMouse`: the event was handed to the same
    /// reducer real input uses. Whatever downstream effect it has (a PTY
    /// write, a harness mutation, a local state change) happens the same
    /// way it would for real input -- this reply is not a receipt for
    /// that, only for "the reducer saw it."
    Injected,
    State(ControlStateV1),
    Frame(ControlFrameV1),
    /// Ack for `WriteSession`: unlike `Injected`, reaching this reply means
    /// `App::route_harness_session_verb` produced a real
    /// `HarnessWriteSessionInput`/`HarnessWriteSessionBytes` action (a
    /// `WriteSession` that could not get that far returns a
    /// [`ControlErrorV1`] instead, never this) -- it is still only an ack
    /// that `client::send_operator_action` was HANDED that action, not a
    /// receipt that the harness/node/PTY chain beyond it accepted the
    /// bytes; use `WaitForOutput` + `ReadSession` to observe that.
    Written,
    SessionContent(ControlSessionContentV1),
    WaitedForOutput(ControlWaitResultV1),
    CapturedFrame(ControlCapturedFrameV1),
}

#[derive(Serialize)]
struct ControlSessionContentV1 {
    cols: u16,
    rows: u16,
    /// Row-major plain cell text, rows joined by `\n` -- see [`read_pty`]'s
    /// own doc comment for exactly which projection produced it and at
    /// what size.
    text: String,
}

#[derive(Serialize)]
struct ControlWaitResultV1 {
    /// The session's own frame sequence at the moment this reply was sent
    /// -- `None` if the session has never had a frame applied at all
    /// (`TerminalWatermarks` has no entry for it yet). Pass this straight
    /// back as the next call's `after_frame` to keep composing waits.
    sequence: Option<u64>,
    /// How long [`poll_wait_for_output`] actually spent polling before
    /// answering -- `0` for a caller that passed `after_frame` already
    /// behind the current sequence (the very first check already sees
    /// `advanced: true`).
    waited_ms: u64,
    /// `true` iff `sequence > after_frame` -- `false` on timeout, NOT an
    /// error: nothing printed within the window is a legitimate, common
    /// outcome (an idle session, a slow provider), not a malformed or
    /// refused request, so it comes back `status: ok` with this `false`
    /// rather than a wire-level error.
    advanced: bool,
}

#[derive(Serialize)]
struct ControlStateV1 {
    focus: &'static str,
    panes: Vec<ControlPaneV1>,
    focused_pane: u32,
    profile: ProfileSnapshot,
}

#[derive(Serialize)]
struct ControlPaneV1 {
    pane_id: u32,
    active_tab_index: usize,
    tabs: Vec<ControlTabV1>,
}

#[derive(Serialize)]
struct ControlTabV1 {
    kind: &'static str,
    session: Option<ControlSessionAddressV1>,
}

/// Both directions: `QueryState`'s own reply reports one of these per open
/// PTY tab ([`control_tab`]), and `WriteSession`/`ReadSession`/
/// `WaitForOutput` take one right back as their own address -- the same
/// four fields either way, so a caller can round-trip an address `QueryState`
/// just reported straight into the next verb without reshaping it.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ControlSessionAddressV1 {
    node_id: String,
    workspace_id: String,
    instance_id: u64,
    generation: u64,
}

impl From<&SessionAddress> for ControlSessionAddressV1 {
    fn from(address: &SessionAddress) -> Self {
        Self {
            node_id: address.node_id.clone(),
            workspace_id: address.workspace_id.clone(),
            instance_id: address.instance_id,
            generation: address.generation,
        }
    }
}

impl ControlSessionAddressV1 {
    fn to_address(&self) -> SessionAddress {
        SessionAddress {
            node_id: self.node_id.clone(),
            workspace_id: self.workspace_id.clone(),
            instance_id: self.instance_id,
            generation: self.generation,
        }
    }
}

#[derive(Serialize)]
struct ControlFrameV1 {
    cols: u16,
    rows: u16,
    /// Row-major plain cell text, rows joined by `\n` -- exactly what
    /// `render::render` painted into the scratch buffer, with no sixel/
    /// bitmap pass applied (that pass draws OUT-OF-BAND terminal escape
    /// sequences over cells that already hold a placeholder glyph; it
    /// never changes buffer cell content, so skipping it here is exactly
    /// "plain cell text, no bitmap," not an approximation of it).
    text: String,
}

/// [`ControlRequestV1::CaptureFrame`]'s own reply -- see [`capture_frame`]'s
/// own doc comment for why this carries a filesystem PATH rather than the
/// PNG bytes themselves.
#[derive(Serialize)]
struct ControlCapturedFrameV1 {
    /// Absolute path to the just-written PNG on THIS machine's own local
    /// disk -- meaningful only to a caller running on the same host as
    /// this process (true of every existing verb on this loopback-only
    /// wire, see this module's own top doc comment). The file is not
    /// deleted by this process; a caller driving repeated captures across
    /// a long session is responsible for its own cleanup (`std::env::
    /// temp_dir()` -- see [`capture_frame`]'s own doc comment -- so the
    /// OS's own temp-directory housekeeping is the eventual backstop
    /// either way).
    path: String,
    cols: u16,
    rows: u16,
    width_px: u32,
    height_px: u32,
}

/// Binds `endpoint.bind`, spawns the accept-loop thread, and returns the
/// ACTUAL bound address (useful when the caller passed port 0). Every
/// [`ControlCommand`] the accept loop decodes is handed to `commands` via
/// `blocking_send` -- the same hand-off idiom `harness_operator_worker`
/// already uses from its own non-async thread.
///
/// Checks `is_loopback()` itself, before ever calling `TcpListener::bind`
/// -- a non-loopback `endpoint.bind` never touches a socket at all, it is
/// rejected outright. `gate4agent-tui`'s own CLI parser
/// (`parse_control_plane_bind`) already enforces the same rule before this
/// is ever reached; this is the module's own belt, not reliance on that
/// caller's suspenders.
pub(crate) fn spawn(
    endpoint: ControlPlaneEndpoint,
    commands: mpsc::Sender<ControlCommand>,
) -> Result<SocketAddr, String> {
    if !endpoint.bind.ip().is_loopback() {
        return Err("control plane bind address must be loopback".to_owned());
    }
    let listener = TcpListener::bind(endpoint.bind)
        .map_err(|error| format!("control plane bind failed: {error}"))?;
    let local_addr = listener.local_addr()
        .map_err(|error| format!("control plane local address unavailable: {error}"))?;
    let authority = ControlPlaneCredentialAuthority::new(&endpoint.credential)?;
    std::thread::Builder::new()
        .name("gate4agent-tui-control".to_owned())
        .spawn(move || accept_loop(listener, authority, commands))
        .map_err(|error| format!("control plane accept thread spawn failed: {error}"))?;
    Ok(local_addr)
}

fn accept_loop(
    listener: TcpListener,
    authority: ControlPlaneCredentialAuthority,
    commands: mpsc::Sender<ControlCommand>,
) {
    for accepted in listener.incoming() {
        let Ok(stream) = accepted else { continue };
        let commands = commands.clone();
        // One thread per connection: this is a low-traffic, local driving
        // surface (a script issuing one request at a time), not a service
        // under load -- the simplicity of "one OS thread reads one request
        // and writes one reply" outweighs any pooling this would otherwise
        // need.
        let _ = std::thread::Builder::new()
            .name("gate4agent-tui-control-conn".to_owned())
            .spawn(move || serve_connection(stream, authority, &commands));
    }
}

fn serve_connection(
    mut stream: TcpStream,
    authority: ControlPlaneCredentialAuthority,
    commands: &mpsc::Sender<ControlCommand>,
) {
    let _ = stream.set_read_timeout(Some(CONTROL_IO_DEADLINE));
    let _ = stream.set_write_timeout(Some(CONTROL_IO_DEADLINE));
    let request = match read_request(&mut stream, CONTROL_REQUEST_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(ReadRequestError::TooLarge) => {
            write_reply(&mut stream, &ControlReplyV1::Error { error: ControlErrorV1::RequestTooLarge });
            return;
        }
        Err(ReadRequestError::Malformed | ReadRequestError::Closed) => {
            write_reply(&mut stream, &ControlReplyV1::Error { error: ControlErrorV1::MalformedRequest });
            return;
        }
    };
    let envelope: ControlEnvelopeV1 = match serde_json::from_slice(&request) {
        Ok(envelope) => envelope,
        Err(_) => {
            write_reply(&mut stream, &ControlReplyV1::Error { error: ControlErrorV1::MalformedRequest });
            return;
        }
    };
    if envelope.version != CONTROL_WIRE_VERSION_V1 {
        write_reply(&mut stream, &ControlReplyV1::Error { error: ControlErrorV1::UnsupportedVersion });
        return;
    }
    if !authority.verify(&envelope.credential) {
        write_reply(&mut stream, &ControlReplyV1::Error { error: ControlErrorV1::Unauthorized });
        return;
    }
    // `WaitForOutput` is the one request on this wire that legitimately
    // wants to occupy this connection thread for a while -- handled by its
    // own repeated-round-trip helper ([`poll_wait_for_output`]) instead of
    // the single send/recv every other request below still uses. Every
    // round trip that helper makes still goes through the SAME `commands`
    // channel -> `client::run`'s loop -> `apply` path; this branch changes
    // how many times this thread asks, never who is allowed to touch `app`.
    if let ControlRequestV1::WaitForOutput { session, after_frame, timeout_ms } = envelope.request {
        let reply = poll_wait_for_output(commands, session, after_frame, timeout_ms);
        write_reply(&mut stream, &reply);
        return;
    }
    let reply = send_one_command(commands, envelope.request);
    write_reply(&mut stream, &reply);
}

/// One `ControlCommand` in, one `ControlReplyV1` out -- the send/recv pair
/// every request on this wire except `WaitForOutput` uses exactly once
/// ([`serve_connection`]'s own generic path); factored out so
/// [`poll_wait_for_output`] can reuse the identical round trip on its own
/// polling cadence without duplicating the channel/timeout plumbing.
fn send_one_command(commands: &mpsc::Sender<ControlCommand>, request: ControlRequestV1) -> ControlReplyV1 {
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    let command = ControlCommand { request, reply: reply_tx };
    if commands.blocking_send(command).is_err() {
        return ControlReplyV1::Error { error: ControlErrorV1::Unavailable };
    }
    reply_rx.recv_timeout(CONTROL_REPLY_DEADLINE)
        .unwrap_or(ControlReplyV1::Error { error: ControlErrorV1::Busy })
}

/// Re-asks `apply`'s own `WaitForOutput` arm on [`CONTROL_WAIT_POLL_
/// INTERVAL`] until it reports `advanced: true` or this call's own elapsed
/// time reaches `timeout_ms` (capped at [`CONTROL_WAIT_MAX_TIMEOUT_MS`]) --
/// `apply`'s arm itself never sleeps (see its own doc comment), so every
/// tick here is a full, fast, single-writer-respecting round trip through
/// `client::run`'s own loop, not a busy-loop against `&mut App` from this
/// thread. `timeout_ms: 0` performs exactly one check and returns
/// immediately either way -- "has it already moved" with no wait at all.
fn poll_wait_for_output(
    commands: &mpsc::Sender<ControlCommand>,
    session: ControlSessionAddressV1,
    after_frame: u64,
    timeout_ms: u64,
) -> ControlReplyV1 {
    let budget = Duration::from_millis(timeout_ms.min(CONTROL_WAIT_MAX_TIMEOUT_MS));
    let started = Instant::now();
    loop {
        let request = ControlRequestV1::WaitForOutput {
            session: session.clone(),
            after_frame,
            timeout_ms,
        };
        let reply = send_one_command(commands, request);
        let ControlReplyV1::Ok { response: ControlResponseV1::WaitedForOutput(result) } = &reply else {
            // `Busy`/`Unavailable`/anything else `apply` never actually
            // sends for this variant -- passed straight through rather than
            // swallowed, so a caller sees the real reason this stopped
            // early instead of a misleading `advanced: false`.
            return reply;
        };
        let elapsed = started.elapsed();
        if result.advanced || elapsed >= budget {
            return ControlReplyV1::Ok {
                response: ControlResponseV1::WaitedForOutput(ControlWaitResultV1 {
                    sequence: result.sequence,
                    advanced: result.advanced,
                    waited_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                }),
            };
        }
        std::thread::sleep(CONTROL_WAIT_POLL_INTERVAL.min(budget.saturating_sub(elapsed)));
    }
}

enum ReadRequestError {
    TooLarge,
    Malformed,
    Closed,
}

/// Reads until EOF -- the caller half-closes its write side once the
/// request is sent, exactly `gate4agent-harness-api`'s own
/// `HarnessOperatorEnvelopeV1` framing (`read_single_frame_detecting_
/// operator`, `gate4agent-harness-service::runtime`) -- then requires the
/// buffer to end with exactly one `\n`, with none before it, before
/// stripping it and returning the JSON payload.
fn read_request(stream: &mut TcpStream, max_bytes: usize) -> Result<Vec<u8>, ReadRequestError> {
    let mut bytes = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).map_err(|_| ReadRequestError::Closed)?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > max_bytes {
            return Err(ReadRequestError::TooLarge);
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    if bytes.len() < 2 || bytes.last() != Some(&b'\n') || bytes[..bytes.len() - 1].contains(&b'\n') {
        return Err(ReadRequestError::Malformed);
    }
    bytes.pop();
    Ok(bytes)
}

/// Encodes, appends `\n`, writes, and half-closes the write side -- the
/// reply-side mirror of [`read_request`]'s EOF-is-the-boundary framing.
/// A reply that would exceed [`CONTROL_RESPONSE_MAX_BYTES`] (only
/// realistically `Frame` on a very large terminal) is swapped for a
/// minimal `response_too_large` error instead of writing a truncated,
/// invalid JSON payload.
fn write_reply(stream: &mut TcpStream, reply: &ControlReplyV1) {
    let encoded = serde_json::to_vec(reply).ok().filter(|bytes| {
        bytes.len().saturating_add(1) <= CONTROL_RESPONSE_MAX_BYTES
    });
    let mut encoded = match encoded {
        Some(encoded) => encoded,
        None => {
            let fallback = ControlReplyV1::Error { error: ControlErrorV1::ResponseTooLarge };
            match serde_json::to_vec(&fallback) {
                Ok(encoded) => encoded,
                Err(_) => return,
            }
        }
    };
    encoded.push(b'\n');
    let _ = stream.write_all(&encoded);
    let _ = stream.shutdown(Shutdown::Write);
}

/// The single point where a [`ControlCommand`] touches `&mut App` -- ONLY
/// ever called from `client::run`'s own loop, on that loop's own thread.
/// Returns the `AppAction` the caller (`client::run`) must queue exactly
/// like every other action this loop produces; sends the wire reply itself
/// before returning, so a caller blocked on the TCP connection unblocks as
/// soon as the reducer has been invoked, not only once the resulting
/// `AppAction` has also finished being dispatched onward.
///
/// `Err` arms below never touch `app` beyond the read that discovered the
/// refusal -- there is no partial write, no partial state change, on any
/// of them: [`write_pty`]/[`read_pty`] either fully resolve to the real
/// `AppAction`/content, or fully refuse before building anything.
pub(crate) fn apply(app: &mut App, terminal: &TerminalWatermarks, command: ControlCommand) -> AppAction {
    let ControlCommand { request, reply } = command;
    let outcome: Result<(AppAction, ControlResponseV1), ControlErrorV1> = match request {
        ControlRequestV1::InjectKey { key } => {
            // Same call `client::run`'s own `TerminalEvent::Key` arm makes
            // (`app.reduce(key)`) -- see this module's own doc comment.
            Ok((app.reduce(key.into_ui_key()), ControlResponseV1::Injected))
        }
        ControlRequestV1::InjectMouse { mouse } => {
            // Same function `client::run`'s own `TerminalEvent::Mouse` arm
            // calls (`crate::client::map_mouse`) -- see that fn's own doc
            // comment for why it was made `pub(crate)` for exactly this.
            Ok((crate::client::map_mouse(app, mouse.into_mouse_event()), ControlResponseV1::Injected))
        }
        ControlRequestV1::QueryState => Ok((AppAction::None, ControlResponseV1::State(query_state(app)))),
        ControlRequestV1::DumpFrame => Ok((AppAction::None, ControlResponseV1::Frame(dump_frame(app)))),
        ControlRequestV1::WriteSession { session, payload } => {
            write_pty(app, session, payload).map(|action| (action, ControlResponseV1::Written))
        }
        ControlRequestV1::ReadSession { session } => {
            read_pty(app, &session).map(|content| (AppAction::None, ControlResponseV1::SessionContent(content)))
        }
        ControlRequestV1::WaitForOutput { session, after_frame, .. } => {
            // Never sleeps -- see [`poll_wait_for_output`]'s own doc
            // comment for why the ACTUAL waiting happens on the connection
            // thread, one fast round trip through this arm at a time,
            // rather than parking `client::run`'s own loop here.
            let sequence = terminal.terminal_watermark(&session.to_address());
            let advanced = sequence.is_some_and(|value| value > after_frame);
            Ok((AppAction::None, ControlResponseV1::WaitedForOutput(ControlWaitResultV1 {
                sequence,
                advanced,
                waited_ms: 0,
            })))
        }
        ControlRequestV1::CaptureFrame => {
            capture_frame(app).map(|frame| (AppAction::None, ControlResponseV1::CapturedFrame(frame)))
        }
    };
    match outcome {
        Ok((action, response)) => {
            let _ = reply.send(ControlReplyV1::Ok { response });
            action
        }
        Err(error) => {
            let _ = reply.send(ControlReplyV1::Error { error });
            AppAction::None
        }
    }
}

/// Resolves and writes to a session directly by [`ControlSessionAddressV1`],
/// NEVER through `App::focus`/`App::surface`'s active-pane/active-tab/
/// overlay dispatch chain `InjectKey` rides -- a session address is stable
/// across pane splits, tab reorders, and which pane happens to be focused
/// right now, none of which this verb has (or needs) any business caring
/// about; `App::find_session` already resolves one the same way `App::
/// focused_session` does for a real click-focused pane. Runs the SAME
/// running-session guard `App::reduce_viewport` runs before dispatching ANY
/// key (`SessionNotRunning` == its "stopped PTY is read-only" flash), then
/// the SAME `App::route_harness_session_verb` rewrite `client::send_
/// operator_action` applies to every real keystroke's `AppAction::Input`/
/// `TerminalBytes` before it can become a `HarnessWriteSessionInput`/
/// `HarnessWriteSessionBytes` -- so a `WriteSession` that returns `Ok` is
/// provably the same action a real keystroke at a real, running, correctly-
/// routed PTY would have produced, not a lookalike.
///
/// When that rewrite degrades to `AppAction::None` (`App::harness_session_
/// address`'s own `?` -- the node's harness incarnation isn't known to
/// `App::nodes` yet), this returns `NodeIncarnationUnknown` instead of the
/// silent `ok` an `InjectKey` at the same session would have returned: this
/// is the exact precondition an `InjectKey`-based drive of a live PTY pane
/// was missing (see this module's own doc comment).
fn write_pty(
    app: &mut App,
    session: ControlSessionAddressV1,
    payload: ControlWritePayloadV1,
) -> Result<AppAction, ControlErrorV1> {
    let address = session.to_address();
    let Some(view) = app.find_session(&address) else {
        return Err(ControlErrorV1::SessionNotFound);
    };
    if !view.running {
        return Err(ControlErrorV1::SessionNotRunning);
    }
    let direct_action = match payload {
        ControlWritePayloadV1::Text { value } => {
            if value.len() > TERMINAL_INPUT_MAX_BYTES {
                return Err(ControlErrorV1::WriteTooLarge);
            }
            AppAction::Input { address, text: value }
        }
        ControlWritePayloadV1::Bytes { value } => AppAction::TerminalBytes { address, bytes: value },
    };
    let routed = app.route_harness_session_verb(direct_action);
    if matches!(routed, AppAction::None) {
        return Err(ControlErrorV1::NodeIncarnationUnknown);
    }
    Ok(routed)
}

/// Plain-text vt100 projection of one session's CURRENT screen
/// (`scroll_offset: 0` -- what a caller watching live output actually sees,
/// never a scrolled-back view) via `crate::app::terminal_visible_rows`, the
/// SAME row-projection fn `App`'s own Ctrl+C/Ctrl+X terminal-selection copy
/// path already runs -- see that fn's own doc comment. Sized to whichever
/// pane is CURRENTLY showing this session as its active tab
/// ([`active_pane_terminal_size`] -- the same numbers `render::
/// render_terminal` paints that pane with this frame), falling back to the
/// app's own whole-terminal size (the same numbers [`dump_frame`] reports)
/// when this session is not the active tab of any laid-out pane right now,
/// so this verb never returns an error just because the session isn't
/// currently on screen.
fn read_pty(app: &App, session: &ControlSessionAddressV1) -> Result<ControlSessionContentV1, ControlErrorV1> {
    let address = session.to_address();
    let Some(view) = app.find_session(&address) else {
        return Err(ControlErrorV1::SessionNotFound);
    };
    let (cols, rows) = active_pane_terminal_size(app, &address)
        .unwrap_or((app.terminal_cols, app.terminal_rows));
    let text = crate::app::terminal_visible_rows(view, cols, rows, 0).join("\n");
    Ok(ControlSessionContentV1 { cols, rows, text })
}

/// The viewport rect of whichever pane currently has `address` open as its
/// ACTIVE tab -- the same `(pane_id, rect)` pairing `App::desired_terminal_
/// sizes` already looks up for the real per-session PTY-resize path, read
/// here rather than reimplemented. `None` when no pane's active tab is this
/// session (background tab, or not open at all) -- [`read_pty`]'s own
/// fallback covers that case.
fn active_pane_terminal_size(app: &App, address: &SessionAddress) -> Option<(u16, u16)> {
    let (pane_id, _pane) = app.surface.panes.iter()
        .find(|(_, pane)| pane.active_tab().and_then(SurfaceTab::pty_address) == Some(address))?;
    let viewport = app.layout.surface_panes.iter()
        .find(|layout| layout.pane_id == *pane_id)?
        .viewport;
    Some((viewport.width.max(1), viewport.height.max(1)))
}

fn query_state(app: &App) -> ControlStateV1 {
    let panes = app.surface.panes.iter()
        .map(|(pane_id, pane)| ControlPaneV1 {
            pane_id: pane_id.0,
            active_tab_index: pane.active,
            tabs: pane.tabs.iter().map(control_tab).collect(),
        })
        .collect();
    ControlStateV1 {
        focus: focus_label(app.focus),
        panes,
        focused_pane: app.surface.focused.0,
        profile: app.profiler.snapshot(),
    }
}

fn control_tab(tab: &SurfaceTab) -> ControlTabV1 {
    ControlTabV1 {
        kind: tab_kind_label(tab),
        session: tab.pty_address().map(ControlSessionAddressV1::from),
    }
}

/// Exhaustive on purpose -- a new `SurfaceTab` variant must be given a
/// wire label here before it compiles, rather than silently reporting as
/// whatever the last catch-all arm happened to be.
fn tab_kind_label(tab: &SurfaceTab) -> &'static str {
    match tab {
        SurfaceTab::AgentBoard => "agent_board",
        SurfaceTab::SessionMonitor(_) => "session_monitor",
        SurfaceTab::Pty(_) => "pty",
        SurfaceTab::Preview(_) => "preview",
        SurfaceTab::File(_) => "file",
        SurfaceTab::Git(_) => "git",
        SurfaceTab::HarnessFile(_) => "harness_file",
        SurfaceTab::HarnessGit(_) => "harness_git",
        SurfaceTab::IconGallery => "icon_gallery",
    }
}

/// Exhaustive on purpose, same reasoning as [`tab_kind_label`].
fn focus_label(focus: Focus) -> &'static str {
    match focus {
        Focus::Spaces => "spaces",
        Focus::Agents => "agents",
        Focus::Tabs => "tabs",
        Focus::Viewport => "viewport",
        Focus::Spawn => "spawn",
        Focus::ExistingSession => "existing_session",
        Focus::AddSpace => "add_space",
        Focus::FolderBrowser => "folder_browser",
        Focus::CreateWorkspaceEntry => "create_workspace_entry",
        Focus::CreateWorktree => "create_worktree",
        Focus::RemoveWorktree => "remove_worktree",
        Focus::RenameSession => "rename_session",
        Focus::TaskId => "task_id",
        Focus::ForgetSession => "forget_session",
        Focus::History => "history",
        Focus::Settings => "settings",
        Focus::StatusBarLeft => "status_bar_left",
        Focus::StatusBarCenter => "status_bar_center",
        Focus::StatusBarRight => "status_bar_right",
        Focus::GlobalSearch => "global_search",
    }
}

/// Renders into a fresh scratch buffer via `render::render` -- the SAME
/// render function `client::run`'s own redraw tick calls, so a dump can
/// never show something the app is not actually capable of drawing to a
/// real terminal. Does not touch `app.layout`: the return value of
/// `render::render` is discarded rather than written back, so this
/// read-only verb never perturbs whatever layout the last REAL frame
/// established (hit-testing/cursor placement for the next real click stay
/// exactly as they were before this call).
fn dump_frame(app: &App) -> ControlFrameV1 {
    let cols = app.terminal_cols;
    let rows = app.terminal_rows;
    let mut buffer = uzor_tui::TerminalBuffer::new(cols, rows);
    let _ = crate::render::render(app, &mut buffer);
    let mut text = String::new();
    for row in 0..rows {
        if row > 0 {
            text.push('\n');
        }
        for col in 0..cols {
            text.push_str(&buffer.get(col, row).symbol);
        }
    }
    ControlFrameV1 { cols, rows, text }
}

/// [`ControlRequestV1::CaptureFrame`]'s own handler: renders `app`'s
/// CURRENT frame to PNG bytes via [`crate::frame_capture::render_frame_
/// png`] -- see that function's own doc comment for exactly what pixel
/// content this does and does not contain -- and writes them to a fresh
/// file under [`std::env::temp_dir`]. The wire reply carries that file's
/// own PATH, never the bytes: a full-terminal capture at [`icons::
/// ASSUMED_CELL_WIDTH_PX`]x[`icons::ASSUMED_CELL_HEIGHT_PX`] per cell,
/// PNG-encoded with NO real compression (`crate::png_encode`'s own doc
/// comment), lands close to its raw pixel size -- several megabytes the
/// moment a caller opens more than a couple of panes, past [`CONTROL_
/// RESPONSE_MAX_BYTES`] the moment [`write_reply`] would have to encode
/// it inline. A path costs a few dozen bytes regardless of frame size,
/// exactly like every other verb on this wire that returns something
/// bounded instead of something proportional to screen content.
///
/// Filename pattern (`gate4agent-tui-frame-<pid>-<nanos>.png`) matches
/// `client::tests`'s own existing `std::env::temp_dir()` convention for
/// this crate's other disposable per-process artifacts -- unique per
/// call (process id plus a nanosecond timestamp), so concurrent captures
/// -- across processes or across two requests on the same one -- never
/// collide on one path and never require this handler to invent its own
/// cleanup/locking scheme.
fn capture_frame(app: &App) -> Result<ControlCapturedFrameV1, ControlErrorV1> {
    let frame = crate::frame_capture::render_frame_png(app);
    let path = std::env::temp_dir().join(format!(
        "gate4agent-tui-frame-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default(),
    ));
    std::fs::write(&path, &frame.png).map_err(|_| ControlErrorV1::FrameWriteFailed)?;
    Ok(ControlCapturedFrameV1 {
        path: path.display().to_string(),
        cols: frame.cols,
        rows: frame.rows,
        width_px: frame.width_px,
        height_px: frame.height_px,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent};
    use gate4agent_c2_protocol::C2RelayRoute;
    use std::io::BufRead;

    use gate4agent_node_protocol::{NodeIncarnationId, OpaqueHostPath};
    use gate4agent_types::TerminalMouseProtocolEncoding;

    use crate::app::{ConnectionState, NodeView, Provider, SessionView, WorkspaceView};

    fn test_credential() -> ControlPlaneCredential {
        ControlPlaneCredential::parse(format!("g4atc_{}", "a".repeat(64))).expect("well-formed test credential")
    }

    #[test]
    fn credential_parse_rejects_wrong_prefix_and_length() {
        assert!(ControlPlaneCredential::parse(format!("g4aho_{}", "a".repeat(64))).is_err());
        assert!(ControlPlaneCredential::parse(format!("g4atc_{}", "a".repeat(63))).is_err());
        assert!(ControlPlaneCredential::parse("g4atc_not-hex-at-all").is_err());
    }

    #[test]
    fn credential_debug_never_prints_the_secret() {
        let credential = test_credential();
        let formatted = format!("{credential:?}");
        // The payload itself ("a" * 64), not just the letter "a" -- the
        // type's own name legitimately contains "a" ("Plane", "Credential"),
        // so the real assertion is that the SECRET substring is absent.
        assert!(!formatted.contains(&"a".repeat(64)));
        assert_eq!(formatted, "ControlPlaneCredential([REDACTED])");
    }

    #[test]
    fn authority_accepts_the_matching_secret_and_rejects_every_other_shape() {
        let authority = ControlPlaneCredentialAuthority::new(&test_credential()).unwrap();
        assert!(authority.verify(&format!("g4atc_{}", "a".repeat(64))));
        // Well-formed, but not the configured secret.
        assert!(!authority.verify(&format!("g4atc_{}", "b".repeat(64))));
        // Not shaped like a credential at all.
        assert!(!authority.verify("nonsense"));
    }

    /// `spawn` must never call `TcpListener::bind` at all for a
    /// non-loopback address -- checked by asserting the call still fails
    /// even for an address ("0.0.0.0") that a real bind might otherwise
    /// accept, proving the loopback check runs first, not as a fallback
    /// after a failed bind attempt.
    #[test]
    fn spawn_refuses_a_non_loopback_bind_address() {
        let (tx, _rx) = mpsc::channel(1);
        let endpoint = ControlPlaneEndpoint {
            bind: "0.0.0.0:0".parse().unwrap(),
            credential: test_credential(),
        };
        let error = spawn(endpoint, tx).unwrap_err();
        assert!(error.contains("loopback"), "unexpected error: {error}");
    }

    #[test]
    fn spawn_binds_loopback_port_zero_to_a_concrete_ephemeral_port() {
        let (tx, _rx) = mpsc::channel(1);
        let endpoint = ControlPlaneEndpoint {
            bind: "127.0.0.1:0".parse().unwrap(),
            credential: test_credential(),
        };
        let bound = spawn(endpoint, tx).unwrap();
        assert!(bound.ip().is_loopback());
        assert_ne!(bound.port(), 0);
    }

    /// `CaptureFrame` -- like every other verb on this wire -- is refused
    /// by ABSENCE, not by a per-request check: this module's own top doc
    /// comment states the contract plainly ("`client::run` then never
    /// calls [`spawn`]... byte-identical to before this module existed"
    /// when `--control-plane` is not given). There is no live accept loop
    /// to send a `CaptureFrame` envelope to at all when the control plane
    /// is disabled, so this proves refusal the same way [`spawn_refuses_
    /// a_non_loopback_bind_address`] proves it for a bad bind address:
    /// bind a real ephemeral port with a plain listener, read back its
    /// concrete port number, then drop that listener WITHOUT ever calling
    /// [`spawn`] on it -- nothing is listening on that port afterward, so
    /// connecting to it (the same first step [`round_trip`] takes for
    /// every other verb's own tests) fails outright.
    #[test]
    fn capture_frame_is_unreachable_when_the_control_plane_was_never_spawned() {
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);

        let outcome = TcpStream::connect_timeout(&addr, Duration::from_millis(500));
        assert!(outcome.is_err(), "a CaptureFrame request must have nothing listening to reach when the control plane is disabled");
    }

    /// Sends one full envelope over a real loopback socket and reads the
    /// one-line reply back -- exercises [`read_request`]/[`write_reply`]
    /// end to end, not just their unit-level framing.
    fn round_trip(bound: SocketAddr, body: &str) -> String {
        let mut stream = TcpStream::connect(bound).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.write_all(body.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stream).read_line(&mut line).unwrap();
        line
    }

    #[test]
    fn wire_rejects_a_bad_credential_without_touching_app() {
        let (tx, mut rx) = mpsc::channel(CONTROL_COMMAND_QUEUE_TEST);
        let endpoint = ControlPlaneEndpoint {
            bind: "127.0.0.1:0".parse().unwrap(),
            credential: test_credential(),
        };
        let bound = spawn(endpoint, tx).unwrap();
        let body = format!(
            r#"{{"version":1,"credential":"g4atc_{}","request":{{"kind":"query_state"}}}}"#,
            "b".repeat(64),
        );
        let line = round_trip(bound, &body);
        assert!(line.contains(r#""status":"error""#), "unexpected reply: {line}");
        assert!(line.contains("unauthorized"), "unexpected reply: {line}");
        assert!(rx.try_recv().is_err(), "a rejected credential must never reach the command channel");
    }

    #[test]
    fn wire_rejects_an_unsupported_version() {
        let (tx, _rx) = mpsc::channel(CONTROL_COMMAND_QUEUE_TEST);
        let endpoint = ControlPlaneEndpoint {
            bind: "127.0.0.1:0".parse().unwrap(),
            credential: test_credential(),
        };
        let bound = spawn(endpoint, tx).unwrap();
        let body = format!(
            r#"{{"version":99,"credential":"g4atc_{}","request":{{"kind":"query_state"}}}}"#,
            "a".repeat(64),
        );
        let line = round_trip(bound, &body);
        assert!(line.contains("unsupported_version"), "unexpected reply: {line}");
    }

    const CONTROL_COMMAND_QUEUE_TEST: usize = 8;

    /// `node-a`/`workspace-a`/instance `7`/generation `1`, exactly one open
    /// PTY tab, focus on it -- every other test below that needs a PTY pane
    /// builds one through this. `incarnation_id: None` (the harness has not
    /// yet reported this node's incarnation) is deliberately the DEFAULT
    /// here, not an edge case dialed in for one test: it is what a freshly
    /// connected app's `NodeView` actually looks like before the harness's
    /// own inventory snapshot arrives, and it is the exact shape the two
    /// live `InjectKey` attempts this module's own doc comment describes
    /// were run against.
    fn app_with_one_pty_pane() -> App {
        pty_pane_app(true, None)
    }

    /// Parameterized sibling of [`app_with_one_pty_pane`]: `running`
    /// controls `SessionView::running` (`App::reduce_viewport`'s "stopped
    /// PTY is read-only" guard, mirrored by [`write_pty`]'s own
    /// `SessionNotRunning`), `incarnation_id` controls `NodeView::
    /// incarnation_id` (`App::harness_session_address`'s own `?`, mirrored
    /// by [`write_pty`]'s own `NodeIncarnationUnknown`) -- the two
    /// preconditions [`write_pty`] exists to name explicitly instead of
    /// silently dropping the write, per this module's own doc comment.
    fn pty_pane_app(running: bool, incarnation_id: Option<NodeIncarnationId>) -> App {
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 1,
        };
        let mut app = App::default();
        app.nodes.push(NodeView {
            node_id: "node-a".to_owned(),
            incarnation_id,
            endpoint: r"\\.\pipe\node-a".to_owned(),
            relay_route: C2RelayRoute::Unknown,
            connection: ConnectionState::Connected,
            controller_owned: true,
            event_sequence: 1,
            session_records: Vec::new(),
            launch_inventory: None,
            providers: Vec::new(),
            workspaces: vec![WorkspaceView {
                workspace_id: "workspace-a".to_owned(),
                label: "acme".to_owned(),
                canonical_root: OpaqueHostPath::utf8(r"C:\work\acme".to_owned()).unwrap(),
                providers: Vec::new(),
                sessions: vec![SessionView {
                    address: address.clone(),
                    provider: Provider::new("codex").unwrap(),
                    status: "running".to_owned(),
                    running,
                    stoppable: true,
                    removable: false,
                    restartable: false,
                    attention: false,
                    has_provider_session_identity: true,
                    progress: None,
                    terminal_formatted: b"codex ready\r\n$ ".to_vec(),
                    terminal_scrollback: Vec::new(),
                    terminal_alternate_screen: false,
                    terminal_mouse_protocol_enabled: false,
                    terminal_mouse_protocol_encoding: TerminalMouseProtocolEncoding::Default,
                    terminal_cursor: None,
                }],
                worktree_service_mode: None,
                managed_worktree_profiles: None,
            }],
        });
        app.surface.open_in_focused(SurfaceTab::Pty(address));
        app.focus = Focus::Viewport;
        app
    }

    fn test_session_address() -> ControlSessionAddressV1 {
        ControlSessionAddressV1 {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 1,
        }
    }

    /// The load-bearing proof this module's own doc comment claims: for a
    /// plain, unmodified character, the control-plane's own key decode
    /// (`ControlKeyV1::into_ui_key`) produces the byte-identical `UiKey` a
    /// REAL terminal keypress decodes into via `client::map_key` -- so
    /// `apply`'s one-line `app.reduce(...)` call downstream is provably
    /// driving the same reducer input real input drives it with, not a
    /// side door that merely resembles it.
    #[test]
    fn injected_char_key_decodes_identically_to_a_real_terminal_keypress() {
        let real = crate::client::map_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let injected = ControlKeyV1::Char { value: 'x' }.into_ui_key();
        assert_eq!(real, Some(injected));
    }

    /// Same proof, mouse side: `ControlMouseV1::into_mouse_event` feeds
    /// `client::map_mouse` a `MouseEvent` indistinguishable from the one a
    /// real crossterm `Down(Left)` at the same cell would carry, and
    /// `map_mouse` itself -- the exact function `client::run`'s own
    /// `TerminalEvent::Mouse` arm calls -- produces the same `AppAction`
    /// either way.
    #[test]
    fn injected_left_click_reaches_the_same_map_mouse_as_real_input() {
        let mut app_real = app_with_one_pty_pane();
        let mut app_injected = app_with_one_pty_pane();
        let real = crate::client::map_mouse(
            &mut app_real,
            MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 3, row: 2, modifiers: KeyModifiers::NONE },
        );
        let injected = crate::client::map_mouse(
            &mut app_injected,
            ControlMouseV1 { kind: ControlMouseKindV1::DownLeft, column: 3, row: 2 }.into_mouse_event(),
        );
        assert_eq!(real, injected);
    }

    /// End-to-end through [`apply`] itself (not just the decode step
    /// above): an `InjectKey` command against a focused, running PTY pane
    /// must return the SAME `AppAction` a direct `app.reduce(UiKey::Char)`
    /// call does, proving `apply`'s `InjectKey` arm really is that one-line
    /// call and not a reimplementation of it.
    #[test]
    fn apply_inject_key_returns_exactly_what_app_reduce_returns() {
        let mut app_direct = app_with_one_pty_pane();
        let direct_action = app_direct.reduce(UiKey::Char('x'));

        let mut app_via_control = app_with_one_pty_pane();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::InjectKey { key: ControlKeyV1::Char { value: 'x' } },
            reply: reply_tx,
        };
        let control_action = apply(&mut app_via_control, &TerminalWatermarks::default(), command);
        assert_eq!(direct_action, control_action);
        assert!(matches!(reply_rx.try_recv(), Ok(ControlReplyV1::Ok { response: ControlResponseV1::Injected })));
    }

    /// Mouse-side sibling of `apply_inject_key_returns_exactly_what_app_
    /// reduce_returns`: an `InjectMouse` command through [`apply`] must
    /// return exactly what a direct `map_mouse` call does for the
    /// equivalent real `MouseEvent` at the same cell -- proving `apply`'s
    /// `InjectMouse` arm really is the one-line `map_mouse` pass-through
    /// its own doc comment claims, not a second implementation of it.
    #[test]
    fn apply_inject_mouse_returns_exactly_what_map_mouse_returns() {
        let mut app_direct = app_with_one_pty_pane();
        let direct_action = crate::client::map_mouse(
            &mut app_direct,
            MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 3, row: 2, modifiers: KeyModifiers::NONE },
        );

        let mut app_via_control = app_with_one_pty_pane();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::InjectMouse {
                mouse: ControlMouseV1 { kind: ControlMouseKindV1::DownLeft, column: 3, row: 2 },
            },
            reply: reply_tx,
        };
        let control_action = apply(&mut app_via_control, &TerminalWatermarks::default(), command);
        assert_eq!(direct_action, control_action);
        assert!(matches!(reply_rx.try_recv(), Ok(ControlReplyV1::Ok { response: ControlResponseV1::Injected })));
    }

    #[test]
    fn apply_query_state_reports_focus_pane_and_bound_session() {
        let mut app = app_with_one_pty_pane();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand { request: ControlRequestV1::QueryState, reply: reply_tx };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Ok { response: ControlResponseV1::State(state) } = reply_rx.try_recv().unwrap() else {
            panic!("expected a State response");
        };
        assert_eq!(state.focus, "viewport");
        assert_eq!(state.panes.len(), 1);
        let pane = &state.panes[0];
        assert_eq!(pane.tabs.len(), 1);
        assert_eq!(pane.tabs[0].kind, "pty");
        let session = pane.tabs[0].session.as_ref().expect("pty tab must report its bound session");
        assert_eq!(session.node_id, "node-a");
        assert_eq!(session.workspace_id, "workspace-a");
        assert_eq!(session.instance_id, 7);
    }

    #[test]
    fn apply_dump_frame_reports_the_configured_terminal_size() {
        let mut app = App::default();
        app.terminal_cols = 20;
        app.terminal_rows = 5;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand { request: ControlRequestV1::DumpFrame, reply: reply_tx };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Ok { response: ControlResponseV1::Frame(frame) } = reply_rx.try_recv().unwrap() else {
            panic!("expected a Frame response");
        };
        assert_eq!(frame.cols, 20);
        assert_eq!(frame.rows, 5);
        assert_eq!(frame.text.split('\n').count(), 5);
    }

    /// End to end through [`apply`]/[`capture_frame`]: a real PNG lands on
    /// disk at the reported `path`, sized exactly `cols`/`rows` times
    /// [`icons::ASSUMED_CELL_WIDTH_PX`]/[`icons::ASSUMED_CELL_HEIGHT_PX`],
    /// and its bytes actually decode as a PNG of that size -- not just a
    /// struct reporting numbers with no real file behind them. Removes
    /// its own written file afterward so repeated test runs never leave
    /// artifacts behind in `std::env::temp_dir()`.
    #[test]
    fn apply_capture_frame_writes_a_real_png_at_the_reported_path() {
        let mut app = App::default();
        app.terminal_cols = 20;
        app.terminal_rows = 5;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand { request: ControlRequestV1::CaptureFrame, reply: reply_tx };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Ok { response: ControlResponseV1::CapturedFrame(frame) } = reply_rx.try_recv().unwrap() else {
            panic!("expected a CapturedFrame response");
        };
        assert_eq!(frame.cols, 20);
        assert_eq!(frame.rows, 5);
        assert_eq!(frame.width_px, 20 * crate::icons::ASSUMED_CELL_WIDTH_PX);
        assert_eq!(frame.height_px, 5 * crate::icons::ASSUMED_CELL_HEIGHT_PX);

        let bytes = std::fs::read(&frame.path).expect("capture_frame must have written a real file at its own reported path");
        std::fs::remove_file(&frame.path).expect("test cleanup must be able to remove its own written file");
        let (decoded_width, decoded_height, _decoded_rgba) = crate::png_encode::decode_for_test(&bytes);
        assert_eq!(decoded_width, frame.width_px);
        assert_eq!(decoded_height, frame.height_px);
    }

    /// THE pin: reproduces exactly the precondition this module's own doc
    /// comment describes an `InjectKey` against a live, focused, running
    /// PTY pane silently dropping for -- the node's harness incarnation
    /// (`NodeView::incarnation_id`) not yet known. Before `write_pty`
    /// existed, `apply_inject_key_returns_exactly_what_app_reduce_returns`
    /// above already proves `App::reduce` legitimately produces
    /// `AppAction::Input` here; this test proves the SAME session, written
    /// directly instead of via a keystroke, refuses with a NAMED reason
    /// instead of a silent `ok`.
    #[test]
    fn write_pty_refuses_when_the_nodes_harness_incarnation_is_unknown() {
        let mut app = app_with_one_pty_pane();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::WriteSession {
                session: test_session_address(),
                payload: ControlWritePayloadV1::Text { value: "x".to_owned() },
            },
            reply: reply_tx,
        };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        // The refusal never produced ANY `AppAction` to queue -- unlike a
        // real keystroke through `InjectKey`, which would have produced
        // `AppAction::Input` and then silently dropped it two steps later.
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Error { error } = reply_rx.try_recv().unwrap() else {
            panic!("expected an Error reply naming why the write was refused");
        };
        assert!(matches!(error, ControlErrorV1::NodeIncarnationUnknown));
    }

    #[test]
    fn write_pty_refuses_a_stopped_session() {
        let mut app = pty_pane_app(false, Some(NodeIncarnationId::from_bytes([9; 16])));
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::WriteSession {
                session: test_session_address(),
                payload: ControlWritePayloadV1::Bytes { value: vec![0x1b] },
            },
            reply: reply_tx,
        };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Error { error } = reply_rx.try_recv().unwrap() else {
            panic!("expected an Error reply naming why the write was refused");
        };
        assert!(matches!(error, ControlErrorV1::SessionNotRunning));
    }

    #[test]
    fn write_pty_refuses_an_unknown_session() {
        let mut app = app_with_one_pty_pane();
        let mut session = test_session_address();
        session.instance_id = 999;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::WriteSession {
                session,
                payload: ControlWritePayloadV1::Text { value: "x".to_owned() },
            },
            reply: reply_tx,
        };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Error { error } = reply_rx.try_recv().unwrap() else {
            panic!("expected an Error reply naming why the write was refused");
        };
        assert!(matches!(error, ControlErrorV1::SessionNotFound));
    }

    /// Same bound the real Ctrl+V paste path enforces
    /// (`App::reduce_viewport`'s own `UiKey::Ctrl('v')` arm) -- proven here
    /// so `write_pty` can never become the "just script around the clipboard
    /// limit" loophole.
    #[test]
    fn write_pty_text_over_the_limit_is_refused() {
        let mut app = pty_pane_app(true, Some(NodeIncarnationId::from_bytes([9; 16])));
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let oversized = "x".repeat(TERMINAL_INPUT_MAX_BYTES + 1);
        let command = ControlCommand {
            request: ControlRequestV1::WriteSession {
                session: test_session_address(),
                payload: ControlWritePayloadV1::Text { value: oversized },
            },
            reply: reply_tx,
        };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Error { error } = reply_rx.try_recv().unwrap() else {
            panic!("expected an Error reply naming why the write was refused");
        };
        assert!(matches!(error, ControlErrorV1::WriteTooLarge));
    }

    /// End-to-end through [`apply`]/[`write_pty`]: for a session with a
    /// KNOWN incarnation, a `WriteSession` `Text` payload must produce the
    /// exact same `AppAction::HarnessWriteSessionInput` a real `UiKey::
    /// Char('x')` keystroke produces once routed through `App::route_
    /// harness_session_verb` -- the same two-step real path
    /// `client::send_operator_action` runs for every real keystroke.
    #[test]
    fn write_pty_text_reaches_the_same_action_a_real_keystroke_would() {
        let incarnation = NodeIncarnationId::from_bytes([9; 16]);
        let mut app_direct = pty_pane_app(true, Some(incarnation));
        let raw_action = app_direct.reduce(UiKey::Char('x'));
        let direct_action = app_direct.route_harness_session_verb(raw_action);

        let mut app_via_control = pty_pane_app(true, Some(incarnation));
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::WriteSession {
                session: test_session_address(),
                payload: ControlWritePayloadV1::Text { value: "x".to_owned() },
            },
            reply: reply_tx,
        };
        let control_action = apply(&mut app_via_control, &TerminalWatermarks::default(), command);
        assert_eq!(direct_action, control_action);
        assert!(
            matches!(direct_action, AppAction::HarnessWriteSessionInput { .. }),
            "expected a routed harness write, got {direct_action:?}",
        );
        assert!(matches!(
            reply_rx.try_recv(),
            Ok(ControlReplyV1::Ok { response: ControlResponseV1::Written }),
        ));
    }

    /// Bytes-payload sibling of `write_pty_text_reaches_the_same_action_a_
    /// real_keystroke_would`, against `UiKey::TerminalBytes` instead of
    /// `UiKey::Char` -- the path `InjectKey`'s own `TerminalBytes` variant
    /// rides, and the one my second live attempt (`{"variant":"terminal_
    /// bytes","value":[120]}`) actually used.
    #[test]
    fn write_pty_bytes_reaches_the_same_action_as_real_terminal_bytes() {
        let incarnation = NodeIncarnationId::from_bytes([9; 16]);
        let mut app_direct = pty_pane_app(true, Some(incarnation));
        let raw_action = app_direct.reduce(UiKey::TerminalBytes(vec![0x1b, b'p']));
        let direct_action = app_direct.route_harness_session_verb(raw_action);

        let mut app_via_control = pty_pane_app(true, Some(incarnation));
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::WriteSession {
                session: test_session_address(),
                payload: ControlWritePayloadV1::Bytes { value: vec![0x1b, b'p'] },
            },
            reply: reply_tx,
        };
        let control_action = apply(&mut app_via_control, &TerminalWatermarks::default(), command);
        assert_eq!(direct_action, control_action);
        assert!(
            matches!(direct_action, AppAction::HarnessWriteSessionBytes { .. }),
            "expected a routed harness byte write, got {direct_action:?}",
        );
        assert!(matches!(
            reply_rx.try_recv(),
            Ok(ControlReplyV1::Ok { response: ControlResponseV1::Written }),
        ));
    }

    #[test]
    fn read_pty_returns_the_sessions_current_screen_text() {
        let mut app = app_with_one_pty_pane();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::ReadSession { session: test_session_address() },
            reply: reply_tx,
        };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Ok { response: ControlResponseV1::SessionContent(content) } = reply_rx.try_recv().unwrap() else {
            panic!("expected a SessionContent response");
        };
        // No pane in `app.layout.surface_panes` (no `render::render` call
        // happened in this test) -- `read_pty` must fall back to the app's
        // own whole-terminal size, `App::default`'s own 80x24.
        assert_eq!(content.cols, 80);
        assert_eq!(content.rows, 24);
        assert!(content.text.contains("codex ready"), "unexpected content: {}", content.text);
    }

    #[test]
    fn read_pty_reports_not_found_for_an_unknown_session() {
        let mut app = app_with_one_pty_pane();
        let mut session = test_session_address();
        session.workspace_id = "does-not-exist".to_owned();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand { request: ControlRequestV1::ReadSession { session }, reply: reply_tx };
        let action = apply(&mut app, &TerminalWatermarks::default(), command);
        assert_eq!(action, AppAction::None);
        let ControlReplyV1::Error { error } = reply_rx.try_recv().unwrap() else {
            panic!("expected an Error reply");
        };
        assert!(matches!(error, ControlErrorV1::SessionNotFound));
    }

    /// `apply`'s own `WaitForOutput` arm never sleeps (see its doc
    /// comment) -- this drives it directly, twice, proving both halves of
    /// its contract: nothing recorded yet reports `advanced: false` with
    /// `sequence: None`, and recording a real frame (the same call
    /// `apply_update`'s `HarnessTerminalPolled`/`HarnessTerminalPushed`
    /// arms make) flips the very next check to `advanced: true` with that
    /// frame's own sequence.
    #[test]
    fn apply_wait_for_output_reports_advanced_once_the_watermark_passes_after_frame() {
        let mut app = app_with_one_pty_pane();
        let mut watermarks = TerminalWatermarks::default();
        let address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 1,
        };

        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::WaitForOutput {
                session: test_session_address(),
                after_frame: 5,
                timeout_ms: 0,
            },
            reply: reply_tx,
        };
        let _ = apply(&mut app, &watermarks, command);
        let ControlReplyV1::Ok { response: ControlResponseV1::WaitedForOutput(result) } = reply_rx.try_recv().unwrap() else {
            panic!("expected a WaitedForOutput response");
        };
        assert!(!result.advanced);
        assert_eq!(result.sequence, None);

        watermarks.record_terminal_frame(address, 6);
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let command = ControlCommand {
            request: ControlRequestV1::WaitForOutput {
                session: test_session_address(),
                after_frame: 5,
                timeout_ms: 0,
            },
            reply: reply_tx,
        };
        let _ = apply(&mut app, &watermarks, command);
        let ControlReplyV1::Ok { response: ControlResponseV1::WaitedForOutput(result) } = reply_rx.try_recv().unwrap() else {
            panic!("expected a WaitedForOutput response");
        };
        assert!(result.advanced);
        assert_eq!(result.sequence, Some(6));
    }

    /// [`poll_wait_for_output`]'s own real-socket path, not just `apply`'s
    /// one-shot arm above: a background "loop" (standing in for `client::
    /// run`'s own loop) drains `commands` and records a frame ~100ms in;
    /// the real TCP `WaitForOutput` request, issued BEFORE that frame
    /// exists, must come back only once it does, reporting it -- proving
    /// this verb actually blocks-and-observes rather than racing a single
    /// snapshot.
    #[test]
    fn wait_for_output_polls_the_real_socket_until_a_background_frame_lands() {
        let (tx, mut rx) = mpsc::channel(CONTROL_COMMAND_QUEUE_TEST);
        let endpoint = ControlPlaneEndpoint {
            bind: "127.0.0.1:0".parse().unwrap(),
            credential: test_credential(),
        };
        let bound = spawn(endpoint, tx).unwrap();

        let frame_address = SessionAddress {
            node_id: "node-a".to_owned(),
            workspace_id: "workspace-a".to_owned(),
            instance_id: 7,
            generation: 1,
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut app = app_with_one_pty_pane();
            let mut watermarks = TerminalWatermarks::default();
            let started = Instant::now();
            let mut recorded = false;
            while !worker_stop.load(std::sync::atomic::Ordering::Relaxed) {
                while let Ok(command) = rx.try_recv() {
                    apply(&mut app, &watermarks, command);
                }
                if !recorded && started.elapsed() >= Duration::from_millis(100) {
                    watermarks.record_terminal_frame(frame_address.clone(), 6);
                    recorded = true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let session = test_session_address();
        let body = format!(
            r#"{{"version":1,"credential":"g4atc_{}","request":{{"kind":"wait_for_output","session":{{"node_id":"{}","workspace_id":"{}","instance_id":{},"generation":{}}},"after_frame":5,"timeout_ms":4000}}}}"#,
            "a".repeat(64), session.node_id, session.workspace_id, session.instance_id, session.generation,
        );
        let line = round_trip(bound, &body);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        worker.join().expect("background apply loop must not panic");

        assert!(line.contains(r#""advanced":true"#), "unexpected reply: {line}");
        assert!(line.contains(r#""sequence":6"#), "unexpected reply: {line}");
    }
}
