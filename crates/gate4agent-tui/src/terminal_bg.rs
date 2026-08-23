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
//! This also means the exchange needs NO raw-mode toggle of its own:
//! `ReadConsoleInputW` reads the console's raw input-record QUEUE
//! directly, unlike `ReadFile`/`ReadConsoleW` (what `std::io::Stdin`
//! resolves to), which is the API `ENABLE_LINE_INPUT`/`ENABLE_ECHO_INPUT`
//! (what raw mode toggles) actually governs -- so this runs correctly
//! before `client::run` ever calls `TerminalGuard::enter()` (raw mode +
//! alternate screen + mouse capture + bracketed paste), on the SAME
//! thread, with nothing else reading the console input queue
//! concurrently. That ordering is load-bearing, not cosmetic: reading via
//! a second thread (so the main thread could keep going) would leave a
//! blocking read racing crossterm's own later `event::poll`/`read` calls
//! against the identical console input queue for the rest of the
//! process's life if the terminal never answers -- an intermittent,
//! nearly-undebuggable dropped-keystroke defect, not a bounded-startup
//! one. Bounding the wait ([`QUERY_TIMEOUT`], checked via
//! `WaitForSingleObject`'s own millisecond timeout, never a blocking
//! read with no bound) is what keeps this single-threaded design from
//! ever hanging startup instead.
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

#[cfg(windows)]
fn query_osc11_background_windows(timeout: Duration) -> Option<(u8, u8, u8)> {
    use std::io::Write;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

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

    // SAFETY: `GetStdHandle` with a documented standard-handle constant
    // never does more than return whatever handle (possibly invalid) the
    // process already owns for that stream -- no buffer, no lifetime to
    // uphold.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
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
        // SAFETY: `handle` was validated non-null/non-invalid above; a
        // plain millisecond timeout with no callback/APC involved.
        let wait_result = unsafe { WaitForSingleObject(handle, wait_ms) };
        if wait_result != WAIT_OBJECT_0 {
            return None; // WAIT_TIMEOUT or WAIT_FAILED -- no answer.
        }
        let mut record = INPUT_RECORD::default();
        let mut read_count: u32 = 0;
        // SAFETY: `record`/`read_count` are valid, correctly-sized
        // out-params for a request of exactly 1 record; `WaitForSingleObject`
        // above already confirmed the queue is non-empty so this cannot
        // block further.
        let ok = unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut read_count) };
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
}
