//! Learns the terminal's own real background colour instead of imposing
//! one -- see `icons.rs`'s own "Sixel background variants" doc section
//! for why a sixel-tier icon needs a KNOWN, concrete background at all
//! (icy_sixel's encoder has no real alpha channel, only a hard opacity
//! threshold, so every icon must be composited fully opaque against
//! something). The prior fix stated a fixed constant (`SIDEBAR_BG`/
//! `ACTIVE_BG`) across the rail/strip/gallery unconditionally, which
//! read as a visibly lighter "plate" against a real terminal background
//! that is usually darker -- this module is the actual fix: ask the
//! terminal what its background really is, once, before anything else
//! touches the console.
//!
//! [`detect_background`] is the entry point `client::run` actually
//! calls: OSC 11 first (below), then the console's own screen-buffer
//! colour table if OSC 11 answers nothing (still the terminal's real
//! configured colour, just resolved without a VT round-trip -- see
//! [`query_console_info_background`]'s own doc comment), and
//! [`FALLBACK_BACKGROUND`] only if BOTH fail.
//!
//! ## The exchange (OSC 11)
//!
//! `ESC ] 11 ; ? BEL` is the traditional xterm query for "what is your
//! current background colour" -- a compliant terminal answers with
//! `ESC ] 11 ; rgb:RRRR/GGGG/BBBB` followed by either the same BEL or the
//! String Terminator (`ESC \\`), whichever convention it prefers (both
//! are accepted here -- see [`query_osc11_background`]'s own doc
//! comment). Windows Terminal answers it; a terminal that does not
//! (older conhost, a redirected/headless stdin in this crate's own test
//! harness) simply never sends a matching reply, which this module reads
//! as "unknown" rather than hanging or blocking startup -- see
//! [`resolve_background`]'s own doc comment for what "unknown" resolves
//! to.
//!
//! ## Windows: the console must be put IN a mode that can receive the reply
//!
//! Writing the query is not enough by itself -- the console's own input
//! processing has to be told, for the duration of this exchange, to
//! actually deliver the reply as input. MEASURED, not theoretical: with
//! the console left in its startup default (`ENABLE_LINE_INPUT`/
//! `ENABLE_ECHO_INPUT`/`ENABLE_PROCESSED_INPUT` on,
//! `ENABLE_VIRTUAL_TERMINAL_INPUT` off) the reply Windows Terminal
//! injects back into this process's console input queue never surfaces
//! as a queued `KEY_EVENT` record at all on the owner's real terminal --
//! cooked-mode input processing consumes it before
//! [`query_osc11_background`]'s own `ReadConsoleInputW` loop (below)
//! ever gets a chance to see it. An earlier version of this module
//! queried with no mode change at all, reasoning that `ReadConsoleInputW`
//! reads the raw record queue and so bypasses `ENABLE_LINE_INPUT`/
//! `ENABLE_ECHO_INPUT` the same way raw mode would -- true of the QUEUE
//! itself, but irrelevant, because cooked-mode processing decides what
//! ever REACHES that queue in the first place. Reading harder does not
//! fix a reply that was never delivered.
//!
//! [`query_osc11_background_windows`] fixes this narrowly: it saves the
//! console's current INPUT mode, sets exactly
//! `ENABLE_VIRTUAL_TERMINAL_INPUT` and clears exactly
//! `ENABLE_LINE_INPUT`/`ENABLE_ECHO_INPUT`/`ENABLE_PROCESSED_INPUT` for
//! the exchange, and restores the saved mode on every exit path --
//! success, timeout, malformed reply, or any earlier failure -- via
//! [`ConsoleModeScope`]'s own `Drop`, never a manual restore repeated
//! before each individual return (which the next branch added later
//! could simply forget). It does the mirrored thing on the OUTPUT
//! handle, best-effort since the write below can still go out either
//! way: setting `ENABLE_VIRTUAL_TERMINAL_PROCESSING` so the query bytes
//! are emitted as an escape sequence for the terminal to interpret,
//! never printed as literal unprintable characters.
//!
//! ## Windows: why raw console records, not crossterm's own event API
//!
//! [`query_osc11_background`]'s Windows implementation reads the reply
//! directly off the console input queue via `ReadConsoleInputW`
//! (`windows-sys`), NOT `crossterm::event::read()` -- a deliberate choice,
//! not an oversight. crossterm's own Windows backend
//! (`event::sys::windows::parse::parse_key_event_record`) reconstructs a
//! `KeyCode::Char` for any control-range `UnicodeChar` (0x00-0x1F,
//! including BEL, 0x07) by calling `ToUnicodeEx` against the synthetic
//! key event's `wVirtualKeyCode` -- but conpty's own VT-input translator
//! sets `wVirtualKeyCode` to 0 for a plain injected control byte with no
//! keyboard equivalent, and `ToUnicodeEx(0, ...)` returns no character,
//! so crossterm's own reconstruction silently DROPS the BEL terminator
//! this exchange depends on to know the reply is complete. Reading the
//! raw `KEY_EVENT_RECORD.uChar.UnicodeChar` field directly (this module's
//! own approach) sidesteps that reconstruction entirely -- the byte
//! conpty actually queued is the byte this module actually sees, control
//! range or not.
//!
//! This runs correctly before `client::run` ever calls
//! `TerminalGuard::enter()` (raw mode + alternate screen + mouse capture
//! + bracketed paste), on the SAME thread, with nothing else reading the
//! console input queue concurrently. That ordering is load-bearing, not
//! cosmetic: reading via a second thread (so the main thread could keep
//! going) would leave a blocking read racing crossterm's own later
//! `event::poll`/`read` calls against the identical console input queue
//! for the rest of the process's life if the terminal never answers -- an
//! intermittent, nearly-undebuggable dropped-keystroke defect, not a
//! bounded-startup one. Bounding the wait ([`QUERY_TIMEOUT`], checked via
//! `WaitForSingleObject`'s own millisecond timeout, never a blocking
//! read with no bound) is what keeps this single-threaded design from
//! ever hanging startup instead -- the console-mode change above is
//! bounded the exact same way, restored the moment this same bounded
//! exchange ends, never left changed past it.
//!
//! Non-Windows targets have no implementation at all yet (this crate's
//! own sixel/OSC-11 work is Windows Terminal-specific today -- see
//! `icons.rs`'s own module doc) and always resolve to [`None`], read the
//! same as "terminal did not answer" by [`resolve_background`].

use std::time::Duration;

/// How long [`query_osc11_background`] waits for a reply before giving
/// up and reporting [`None`] -- short enough that a terminal which never
/// answers cannot meaningfully delay startup, long enough that a real
/// local round-trip (this exchange never crosses a network) comfortably
/// completes under normal load.
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(200);

/// The composite background used whenever the terminal's own real
/// background is not known -- either [`query_osc11_background`] timed
/// out/failed, or a reply arrived but did not parse as a well-formed OSC
/// 11 colour ([`parse_osc11_reply`] returned [`None`]). Pure black, not
/// `render::SIDEBAR_BG`/`ACTIVE_BG` or any other stated theme colour --
/// reusing either of THOSE here would silently reintroduce the exact
/// "icon sits on a lighter plate" defect this module exists to fix, just
/// on the specific terminals that do not answer OSC 11 instead of on
/// every terminal. Terminal colour schemes skew overwhelmingly dark, so
/// compositing against black is the closest a single fallback can land
/// to "blends into whatever is actually there" without ever guessing a
/// terminal's own specific palette.
pub const FALLBACK_BACKGROUND: (u8, u8, u8) = (0, 0, 0);

/// Resolves an already-obtained query result (`Some` from a real
/// [`query_osc11_background`] call, or injected directly by a test) to
/// the concrete background every sixel-tier composite must use --
/// [`FALLBACK_BACKGROUND`] when `queried` is [`None`]. This is the ONE
/// place that decision is made: [`query_osc11_background`] itself never
/// applies the fallback, so a caller cannot distinguish "the terminal
/// truly has this colour" from "we gave up and guessed" by inspecting
/// its return value alone, but every caller (the live startup path in
/// `client::run` and any test that injects a background instead of
/// querying) funnels through this SAME function either way -- there is
/// no second, divergent copy of the fallback rule.
pub fn resolve_background(queried: Option<(u8, u8, u8)>) -> (u8, u8, u8) {
    queried.unwrap_or(FALLBACK_BACKGROUND)
}

/// Identifies which of [`detect_background`]'s sources actually answered
/// -- purely descriptive, carries no behaviour of its own. The fallback
/// rule itself still lives ONLY in [`resolve_background`] (see that
/// function's own doc comment); this enum exists so a caller (`client::run`)
/// can report what happened instead of just the final colour, which alone
/// cannot distinguish "the terminal truly has this colour" from "we gave
/// up and guessed."
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundSource {
    /// The terminal answered the OSC 11 query directly -- see
    /// [`query_osc11_background`].
    Osc11,
    /// OSC 11 produced nothing usable, but the console's own screen-buffer
    /// colour table resolved a background without a VT round-trip -- see
    /// [`query_console_info_background`].
    ConsoleInfo,
    /// Neither source answered -- [`FALLBACK_BACKGROUND`] applies.
    Fallback,
}

impl BackgroundSource {
    /// A short machine-stable label for diagnostics -- see
    /// [`detect_background`]'s own call site in `client::run`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Osc11 => "osc11",
            Self::ConsoleInfo => "console-info",
            Self::Fallback => "fallback",
        }
    }
}

/// The full background-discovery chain `client::run` actually uses: OSC
/// 11 first (the terminal's own live answer, [`query_osc11_background`]),
/// then the console's own screen-buffer colour table if OSC 11 produced
/// nothing ([`query_console_info_background`] -- still the terminal's
/// real configured colour, just read a different way, never a stated
/// theme guess), and [`FALLBACK_BACKGROUND`] (via [`resolve_background`],
/// still the ONE place that decision is made) only if BOTH fail. Returns
/// which source actually answered alongside the resolved colour -- see
/// [`BackgroundSource`].
pub fn detect_background(timeout: Duration) -> (BackgroundSource, (u8, u8, u8)) {
    if let Some(rgb) = query_osc11_background(timeout) {
        return (BackgroundSource::Osc11, rgb);
    }
    if let Some(rgb) = query_console_info_background() {
        return (BackgroundSource::ConsoleInfo, rgb);
    }
    (BackgroundSource::Fallback, resolve_background(None))
}

/// Parses an OSC 11 background-colour reply's raw bytes (everything the
/// terminal sent back, including the leading `ESC ]` and the trailing
/// terminator) into an 8-bit-per-channel RGB triple, or `None` for
/// anything that does not match the expected shape exactly -- never a
/// partial/best-effort parse. Accepts either terminator convention
/// (`BEL` or `ESC \\`, see this module's own header doc comment) since a
/// replying terminal picks its own, not this crate's.
///
/// Each of the three `rgb:R.../G.../B...` components may carry 1-4 hex
/// digits -- the X11/xterm colour-spec convention, where a shorter
/// component is scaled UP to the full 4-digit (16-bit) range it would
/// represent, not left-padded as if it already were one (e.g. a 1-digit
/// `f` means "fully saturated", the same as `ffff`, not `000f`). This
/// module scales every component down to 8 bits with the same
/// range-preserving arithmetic regardless of how many digits the
/// terminal actually sent (Windows Terminal always sends 4; this is not
/// assumed).
pub fn parse_osc11_reply(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let body = text.strip_prefix('\u{1b}')?.strip_prefix(']')?;
    let body = if let Some(stripped) = body.strip_suffix('\u{7}') {
        stripped
    } else if let Some(stripped) = body.strip_suffix("\u{1b}\\") {
        stripped
    } else {
        return None;
    };
    let payload = body.strip_prefix("11;")?.strip_prefix("rgb:")?;
    let mut channels = payload.split('/');
    let red = scale_hex_channel(channels.next()?)?;
    let green = scale_hex_channel(channels.next()?)?;
    let blue = scale_hex_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    Some((red, green, blue))
}

/// Scales a 1-4 hex-digit colour channel (the X11/xterm `rgb:` component
/// shape -- see [`parse_osc11_reply`]'s own doc comment) to an 8-bit
/// value: parse as an integer out of `16^digits - 1`, then rescale into
/// `0..=255` with round-to-nearest, so a 2-digit component round-trips
/// EXACTLY (`max` is already 255) and a 4-digit component (Windows
/// Terminal's own convention, effectively every channel byte doubled --
/// `"0c0c"` for a `0x0c` byte) lands back on that same byte.
fn scale_hex_channel(hex: &str) -> Option<u8> {
    if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = (1u32 << (hex.len() as u32 * 4)) - 1;
    let scaled = (value * 255 + max / 2) / max;
    u8::try_from(scaled).ok()
}

/// Queries the terminal's own real background colour over OSC 11 --
/// `None` on any timeout, I/O failure, or malformed reply (see
/// [`parse_osc11_reply`]); never panics, never blocks past
/// [`QUERY_TIMEOUT`]. See this module's own header doc comment for the
/// full exchange shape and why the Windows implementation reads raw
/// console records instead of going through crossterm's own event API.
/// Must run before `client::run` constructs its `TerminalGuard` (raw
/// mode + alternate screen) -- see that same doc comment for why.
pub fn query_osc11_background(timeout: Duration) -> Option<(u8, u8, u8)> {
    #[cfg(windows)]
    {
        query_osc11_background_windows(timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = timeout;
        None
    }
}

/// A malformed or runaway reply is abandoned once it grows past this --
/// the longest well-formed reply (`ESC ] 11 ; rgb:RRRR/GGGG/BBBB ESC \\`)
/// is under 30 bytes; this is generous headroom, not a tight fit, purely
/// to bound memory on a terminal that answers with garbage instead of
/// nothing (a timeout alone already bounds the WAIT; this bounds the
/// BUFFER for a terminal that keeps sending data without ever completing
/// a valid reply within that same window).
#[cfg(windows)]
const MAX_REPLY_BYTES: usize = 128;

/// The exact input-mode transform the OSC 11 exchange needs for the
/// reply to ever reach [`query_osc11_background_windows`]'s own
/// `ReadConsoleInputW` queue -- see this module's own header doc comment
/// ("the console must be put IN a mode that can receive the reply") for
/// why each bit is touched and none other. A free function, not inlined
/// at the one call site, so the transform itself -- not the syscalls
/// around it -- can be verified directly by a test without a real
/// console handle.
#[cfg(windows)]
fn query_input_mode(
    current: windows_sys::Win32::System::Console::CONSOLE_MODE,
) -> windows_sys::Win32::System::Console::CONSOLE_MODE {
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT,
    };
    (current | ENABLE_VIRTUAL_TERMINAL_INPUT)
        & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT)
}

/// The mirrored OUTPUT-side transform: only ever ADDS
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING` so the query bytes are emitted as
/// an escape sequence the terminal can interpret rather than printed as
/// literal unprintable characters -- see [`query_input_mode`]'s own doc
/// comment for why this is a free function.
#[cfg(windows)]
fn query_output_mode(
    current: windows_sys::Win32::System::Console::CONSOLE_MODE,
) -> windows_sys::Win32::System::Console::CONSOLE_MODE {
    current | windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING
}

/// RAII guard that saves a console handle's current mode on construction
/// and puts it back on [`Drop`], unconditionally, on every exit path out
/// of the scope it guards -- including an early `?`/`return` from the
/// function that opened it. That guarantee comes from `Drop` itself, not
/// from a restore call repeated before each individual `return`, which a
/// later-added branch could forget on one path and not the others.
/// Constructing one is the ONLY way this module ever changes a console
/// mode -- see [`query_osc11_background_windows`]'s own two call sites:
/// the input scope is REQUIRED (the exchange cannot work without it, so
/// its caller bails via `?` when `enter` fails); the output scope is
/// best-effort (the write below still goes out either way).
#[cfg(windows)]
struct ConsoleModeScope {
    handle: windows_sys::Win32::Foundation::HANDLE,
    original: windows_sys::Win32::System::Console::CONSOLE_MODE,
}

#[cfg(windows)]
impl ConsoleModeScope {
    /// Reads `handle`'s current mode, applies `transform` to compute the
    /// mode this exchange needs, and installs it if different -- `None`
    /// if either Win32 call fails, most commonly because `handle` is not
    /// a real console at all (this crate's own headless test harness
    /// always fails `GetConsoleMode` first, before anything is ever
    /// changed).
    fn enter(
        handle: windows_sys::Win32::Foundation::HANDLE,
        transform: impl FnOnce(
            windows_sys::Win32::System::Console::CONSOLE_MODE,
        ) -> windows_sys::Win32::System::Console::CONSOLE_MODE,
    ) -> Option<Self> {
        use windows_sys::Win32::System::Console::{GetConsoleMode, SetConsoleMode};

        let mut original = 0;
        // SAFETY: the caller already validated `handle` non-null/non-invalid;
        // `original` is a valid out-param for exactly one mode.
        if unsafe { GetConsoleMode(handle, &mut original) } == 0 {
            return None;
        }
        let wanted = transform(original);
        if wanted != original {
            // SAFETY: same validated handle; `wanted` is a plain mode
            // bitmask, no pointer/lifetime concern.
            if unsafe { SetConsoleMode(handle, wanted) } == 0 {
                return None;
            }
        }
        Some(Self { handle, original })
    }
}

#[cfg(windows)]
impl Drop for ConsoleModeScope {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::SetConsoleMode;
        // Best-effort: there is nowhere further to report a failure to
        // here, and panicking out of `Drop` would leave the console in a
        // WORSE state than just leaving today's (already-working) mode in
        // place. `TerminalGuard` -- the very next thing `client::run`
        // constructs after this whole query resolves -- sets its own
        // raw-mode bits unconditionally regardless, so the live path
        // recovers either way.
        // SAFETY: `self.handle` was validated when this guard was built.
        let _ = unsafe { SetConsoleMode(self.handle, self.original) };
    }
}

#[cfg(windows)]
fn query_osc11_background_windows(timeout: Duration) -> Option<(u8, u8, u8)> {
    use std::io::Write;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    // SAFETY: `GetStdHandle` with a documented standard-handle constant
    // never does more than return whatever handle (possibly invalid) the
    // process already owns for that stream -- no buffer, no lifetime to
    // uphold.
    let input_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if input_handle.is_null() || input_handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: same as above, the output side's standard handle.
    let output_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if output_handle.is_null() || output_handle == INVALID_HANDLE_VALUE {
        return None;
    }

    // REQUIRED, not best-effort -- see this module's own header doc
    // comment ("the console must be put IN a mode that can receive the
    // reply"). `?` here means: if the console cannot be put into a mode
    // that can receive the reply at all (including "this is not a real
    // console"), there is no point writing the query, so bail before
    // touching stdout. Restored by `Drop` on every exit path below.
    let _input_scope = ConsoleModeScope::enter(input_handle, query_input_mode)?;
    // Best-effort, unlike the input side above: the request bytes below
    // still go out even if this fails (a real Windows Terminal session
    // commonly already has this on) -- it only means a terminal that
    // genuinely needs it now depends on whatever the console already had
    // configured. Restored by `Drop` the same way as the input scope.
    let _output_scope = ConsoleModeScope::enter(output_handle, query_output_mode);

    // Query bytes travel over plain stdout (conpty forwards them to the
    // real terminal, which answers by injecting the reply back into the
    // SAME process's console input queue) -- flushed immediately since
    // this runs before any buffered screen writer exists yet.
    if std::io::stdout().write_all(b"\x1b]11;?\x07").is_err() {
        return None;
    }
    if std::io::stdout().flush().is_err() {
        return None;
    }

    let deadline = Instant::now() + timeout;
    let mut reply: Vec<u8> = Vec::with_capacity(32);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        // Millisecond precision is enough for a bound this coarse
        // (`QUERY_TIMEOUT` is 200ms); rounding UP (`+ 1`) only ever makes
        // an individual wait very slightly longer, never past the
        // OUTER `deadline` check above, which re-measures real elapsed
        // time on every loop iteration regardless.
        let wait_ms = u32::try_from(remaining.as_millis() + 1).unwrap_or(u32::MAX);
        // SAFETY: `input_handle` was validated non-null/non-invalid above;
        // a plain millisecond timeout with no callback/APC involved.
        let wait_result = unsafe { WaitForSingleObject(input_handle, wait_ms) };
        if wait_result != WAIT_OBJECT_0 {
            return None; // WAIT_TIMEOUT or WAIT_FAILED -- no answer.
        }
        let mut record = INPUT_RECORD::default();
        let mut read_count: u32 = 0;
        // SAFETY: `record`/`read_count` are valid, correctly-sized
        // out-params for a request of exactly 1 record; `WaitForSingleObject`
        // above already confirmed the queue is non-empty so this cannot
        // block further.
        let ok = unsafe { ReadConsoleInputW(input_handle, &mut record, 1, &mut read_count) };
        if ok == 0 || read_count == 0 {
            return None;
        }
        if u32::from(record.EventType) != KEY_EVENT {
            continue; // mouse/resize/focus record -- not part of this reply.
        }
        // SAFETY: `EventType == KEY_EVENT` just confirmed the active
        // union member is `KeyEvent`.
        let key_event = unsafe { record.Event.KeyEvent };
        if key_event.bKeyDown == 0 {
            continue; // the key-up half of a synthesized press.
        }
        // SAFETY: reading the `UnicodeChar` union member is always valid
        // for a `KEY_EVENT_RECORD` -- both members are plain integers,
        // never a pointer whose validity depends on which was written.
        let unit = unsafe { key_event.uChar.UnicodeChar };
        let Ok(byte) = u8::try_from(unit) else {
            return None; // non-ASCII in what must be a plain OSC reply.
        };
        reply.push(byte);
        if reply.len() > MAX_REPLY_BYTES {
            return None;
        }
        let terminated = reply.last() == Some(&0x07)
            || (reply.len() >= 2 && reply[reply.len() - 2] == 0x1b && reply[reply.len() - 1] == b'\\');
        if terminated {
            return parse_osc11_reply(&reply);
        }
    }
}

/// Reads the terminal's own configured background WITHOUT a VT round-trip
/// -- the console's screen-buffer colour table (`GetConsoleScreenBufferInfoEx`)
/// already carries the real palette the terminal was configured with; the
/// current text attributes' background nibble is a direct index into
/// that SAME table (the classic 16-colour console attribute model -- see
/// [`background_palette_index`]'s own doc comment). This is
/// [`detect_background`]'s second source, tried once OSC 11 answers
/// nothing -- still the terminal's real colour, never a stated theme
/// constant, just resolved a different way. `None` on any Win32 failure,
/// including "this handle is not a real console" (this crate's own
/// headless test harness).
pub fn query_console_info_background() -> Option<(u8, u8, u8)> {
    #[cfg(windows)]
    {
        query_console_info_background_windows()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The background-colour palette index (`0..=15`) encoded in a console's
/// current text attributes -- the low nibble of the HIGH byte of the
/// legacy attribute word (`BACKGROUND_BLUE`/`BACKGROUND_GREEN`/
/// `BACKGROUND_RED`/`BACKGROUND_INTENSITY`, bits 4-7), a direct index
/// into `CONSOLE_SCREEN_BUFFER_INFOEX::ColorTable`. A free function, not
/// inlined at its one call site, so this shape can be verified without a
/// real console handle -- see this module's own tests.
#[cfg(windows)]
fn background_palette_index(attributes: windows_sys::Win32::System::Console::CONSOLE_CHARACTER_ATTRIBUTES) -> usize {
    usize::from((attributes >> 4) & 0xF)
}

/// Converts a Win32 `COLORREF` (`0x00bbggrr` -- the OPPOSITE byte order
/// from this module's own `(r, g, b)` tuples) to one. A free function for
/// the same reason as [`background_palette_index`].
#[cfg(windows)]
fn rgb_from_colorref(colorref: windows_sys::Win32::Foundation::COLORREF) -> (u8, u8, u8) {
    let red = (colorref & 0xFF) as u8;
    let green = ((colorref >> 8) & 0xFF) as u8;
    let blue = ((colorref >> 16) & 0xFF) as u8;
    (red, green, blue)
}

#[cfg(windows)]
fn query_console_info_background_windows() -> Option<(u8, u8, u8)> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleScreenBufferInfoEx, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFOEX,
        STD_OUTPUT_HANDLE,
    };

    // SAFETY: a documented standard-handle constant; returns whatever
    // handle (possibly invalid) the process already owns, nothing to
    // uphold beyond the null/invalid check right below.
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = CONSOLE_SCREEN_BUFFER_INFOEX {
        cbSize: u32::try_from(core::mem::size_of::<CONSOLE_SCREEN_BUFFER_INFOEX>()).ok()?,
        ..Default::default()
    };
    // SAFETY: `info.cbSize` is set to the struct's own real size, the
    // documented requirement for every Win32 "Ex" info struct; `info` is
    // a valid, correctly-sized out-param for exactly one call.
    if unsafe { GetConsoleScreenBufferInfoEx(handle, &mut info) } == 0 {
        return None;
    }
    let index = background_palette_index(info.wAttributes);
    let colorref = *info.ColorTable.get(index)?;
    Some(rgb_from_colorref(colorref))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_background_prefers_the_queried_colour_when_present() {
        assert_eq!(resolve_background(Some((12, 34, 56))), (12, 34, 56));
    }

    #[test]
    fn resolve_background_falls_back_to_the_darkest_default_when_absent() {
        assert_eq!(resolve_background(None), FALLBACK_BACKGROUND);
        assert_eq!(FALLBACK_BACKGROUND, (0, 0, 0), "the fallback must be genuinely dark, not a stated theme colour");
    }

    #[test]
    fn background_source_label_is_machine_stable_and_distinct() {
        assert_eq!(BackgroundSource::Osc11.label(), "osc11");
        assert_eq!(BackgroundSource::ConsoleInfo.label(), "console-info");
        assert_eq!(BackgroundSource::Fallback.label(), "fallback");
    }

    #[test]
    fn parses_a_bel_terminated_reply_with_four_digit_channels() {
        // Windows Terminal's own real reply shape for (12, 12, 12) --
        // each byte doubled into a 4-digit channel ("0c0c" == 0x0c/0x0c).
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:0c0c/0c0c/0c0c\x07"), Some((12, 12, 12)));
    }

    #[test]
    fn parses_a_string_terminated_reply() {
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:ffff/0000/8080\x1b\\"), Some((255, 0, 128)));
    }

    #[test]
    fn parses_two_digit_channels_without_scaling_drift() {
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:1e/1e/2e\x07"), Some((30, 30, 46)));
    }

    #[test]
    fn parses_a_single_digit_channel_as_full_range_scaled() {
        // A lone `f` means fully saturated (== `ffff`, not `000f`) --
        // see `scale_hex_channel`'s own doc comment.
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:f/0/0\x07"), Some((255, 0, 0)));
    }

    #[test]
    fn rejects_a_reply_with_the_wrong_osc_number() {
        assert_eq!(parse_osc11_reply(b"\x1b]10;rgb:0c0c/0c0c/0c0c\x07"), None, "OSC 10 is the foreground query, not background");
    }

    #[test]
    fn rejects_a_reply_missing_its_terminator() {
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:0c0c/0c0c/0c0c"), None);
    }

    #[test]
    fn rejects_a_reply_with_too_few_channels() {
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:0c0c/0c0c\x07"), None);
    }

    #[test]
    fn rejects_a_reply_with_too_many_channels() {
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:0c0c/0c0c/0c0c/0c0c\x07"), None);
    }

    #[test]
    fn rejects_a_reply_with_non_hex_digits() {
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:zzzz/0c0c/0c0c\x07"), None);
    }

    #[test]
    fn rejects_a_reply_with_an_empty_channel() {
        assert_eq!(parse_osc11_reply(b"\x1b]11;rgb:/0c0c/0c0c\x07"), None);
    }

    #[test]
    fn rejects_non_utf8_bytes() {
        assert_eq!(parse_osc11_reply(&[0x1b, b']', 0xff, 0xfe]), None);
    }

    #[test]
    fn rejects_a_reply_missing_the_leading_escape_bracket() {
        assert_eq!(parse_osc11_reply(b"11;rgb:0c0c/0c0c/0c0c\x07"), None);
    }

    #[test]
    fn every_channel_digit_count_from_one_to_four_round_trips_its_own_extremes() {
        for digits in 1..=4usize {
            let low = "0".repeat(digits);
            let high = "f".repeat(digits);
            let reply_low = format!("\x1b]11;rgb:{low}/{low}/{low}\x07");
            let reply_high = format!("\x1b]11;rgb:{high}/{high}/{high}\x07");
            assert_eq!(parse_osc11_reply(reply_low.as_bytes()), Some((0, 0, 0)), "digits={digits}");
            assert_eq!(parse_osc11_reply(reply_high.as_bytes()), Some((255, 255, 255)), "digits={digits}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn query_input_mode_enables_vt_input_and_clears_exactly_the_cooked_mode_bits() {
        use windows_sys::Win32::System::Console::{
            ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_INSERT_MODE, ENABLE_LINE_INPUT,
            ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE,
            ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_WINDOW_INPUT,
        };
        // A realistic console startup-default input mode: cooked-mode
        // bits on, VT input off, plus assorted bits this transform has no
        // business touching either way.
        let startup_default = ENABLE_PROCESSED_INPUT
            | ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_INSERT_MODE
            | ENABLE_QUICK_EDIT_MODE
            | ENABLE_EXTENDED_FLAGS
            | ENABLE_MOUSE_INPUT
            | ENABLE_WINDOW_INPUT;
        let query_mode = query_input_mode(startup_default);
        assert_ne!(query_mode & ENABLE_VIRTUAL_TERMINAL_INPUT, 0, "VT input must be ON or the reply never reaches the record queue");
        assert_eq!(query_mode & ENABLE_LINE_INPUT, 0, "cooked-mode line editing must be OFF");
        assert_eq!(query_mode & ENABLE_ECHO_INPUT, 0, "echo must be OFF");
        assert_eq!(query_mode & ENABLE_PROCESSED_INPUT, 0, "processed input must be OFF");
        // Bits this exchange has no business touching survive untouched.
        assert_ne!(query_mode & ENABLE_INSERT_MODE, 0);
        assert_ne!(query_mode & ENABLE_QUICK_EDIT_MODE, 0);
        assert_ne!(query_mode & ENABLE_EXTENDED_FLAGS, 0);
        assert_ne!(query_mode & ENABLE_MOUSE_INPUT, 0);
        assert_ne!(query_mode & ENABLE_WINDOW_INPUT, 0);
    }

    #[cfg(windows)]
    #[test]
    fn query_input_mode_is_idempotent_once_already_in_query_shape() {
        use windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_INPUT;
        assert_eq!(query_input_mode(ENABLE_VIRTUAL_TERMINAL_INPUT), ENABLE_VIRTUAL_TERMINAL_INPUT);
    }

    #[cfg(windows)]
    #[test]
    fn query_output_mode_only_ever_adds_virtual_terminal_processing() {
        use windows_sys::Win32::System::Console::{
            ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WRAP_AT_EOL_OUTPUT,
        };
        let startup_default = ENABLE_PROCESSED_OUTPUT | ENABLE_WRAP_AT_EOL_OUTPUT;
        let query_mode = query_output_mode(startup_default);
        assert_ne!(query_mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
        assert_ne!(query_mode & ENABLE_PROCESSED_OUTPUT, 0, "unrelated output bits must survive untouched");
        assert_ne!(query_mode & ENABLE_WRAP_AT_EOL_OUTPUT, 0, "unrelated output bits must survive untouched");
    }

    #[cfg(windows)]
    #[test]
    fn background_palette_index_reads_the_high_byte_low_nibble() {
        use windows_sys::Win32::System::Console::{
            BACKGROUND_BLUE, BACKGROUND_GREEN, BACKGROUND_INTENSITY, BACKGROUND_RED,
            FOREGROUND_BLUE, FOREGROUND_GREEN, FOREGROUND_INTENSITY, FOREGROUND_RED,
        };
        // Index 0: no background bits set at all, regardless of foreground.
        assert_eq!(background_palette_index(FOREGROUND_RED | FOREGROUND_INTENSITY), 0);
        // Index 15 (0xF): every background bit set, the brightest palette
        // entry -- foreground bits must never leak into this index.
        let all_background = BACKGROUND_BLUE | BACKGROUND_GREEN | BACKGROUND_RED | BACKGROUND_INTENSITY;
        assert_eq!(background_palette_index(all_background | FOREGROUND_BLUE | FOREGROUND_GREEN), 15);
        // A single mid bit: BACKGROUND_RED alone is index 4 (bit 6 -> nibble bit 2 -> value 4).
        assert_eq!(background_palette_index(BACKGROUND_RED), 4);
    }

    #[cfg(windows)]
    #[test]
    fn rgb_from_colorref_reverses_the_bgr_byte_order() {
        // 0x00_0c_0c_0c: (12, 12, 12) written in COLORREF's own
        // 0x00bbggrr order -- every channel identical, so this alone
        // would not catch a byte-order bug; the next two cases do.
        assert_eq!(rgb_from_colorref(0x000c0c0c), (12, 12, 12));
        // Blue channel only (0x00FF0000) must land in the tuple's THIRD
        // slot, not its first.
        assert_eq!(rgb_from_colorref(0x00ff0000), (0, 0, 255));
        // Red channel only (0x000000FF) must land in the tuple's FIRST
        // slot, not its third.
        assert_eq!(rgb_from_colorref(0x000000ff), (255, 0, 0));
    }
}
