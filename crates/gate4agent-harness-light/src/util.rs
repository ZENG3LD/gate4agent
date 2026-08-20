//! Small, dependency-free helpers shared by the credential authority and the
//! session-verb relay: hex encoding for freshly minted secrets/keys, and a
//! panic-free wall-clock read for `HarnessRuntimeNodeInventoryV1::observed_at_unix_ms`.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// Lowercase hex encoding, matching every other domain id/credential/digest
/// encoding already used across the gate4agent-harness-* wire (`g4aho_` +
/// 64 hex, `hop_`/`hidem_` + 24 hex, ...).
pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // `write!` into a `String` cannot fail.
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

/// Current wall-clock time in Unix milliseconds. Falls back to `0` rather
/// than panicking on a pre-epoch clock -- this only ever feeds an
/// `observed_at_unix_ms` freshness hint, never an authorization decision, so
/// a degenerate value is honest data, not a correctness hole worth a panic.
pub(crate) fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
