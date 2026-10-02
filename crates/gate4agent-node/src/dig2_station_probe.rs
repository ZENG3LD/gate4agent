//! Cheap dig2browser-station **path / named-pipe reachability** probe.
//!
//! Behind Cargo feature `dig2-station-probe`. Path / connectability only —
//! **never** cookie jars, session import bodies, OAuth, or proxy credentials.
//! Exclusive node-local lease lives in `dig2_station_lease` (same feature).
//! This module only answers “is the local station IPC endpoint present /
//! connectable?”
//!
//! Windows-first: dig2browser station IPC today is `\\.\pipe\{suffix}` with
//! default suffix [`DEFAULT_STATION_PIPE_SUFFIX`] (`dig2browser-station-v1`).
//! Linux / non-Windows: no dig2 unix-socket path yet — probe reports
//! [`StationProbeError::UnsupportedPlatform`] (resolve refuses rather than
//! fake success). Plan:
//! `dig2browser-station-probe-and-network-permit-set-2026-10-02.md` Track A.

/// Default pipe **suffix** (not the full `\\.\pipe\…` path). Mirrors
/// dig2browser-protocol `DEFAULT_STATION_PIPE`.
pub const DEFAULT_STATION_PIPE_SUFFIX: &str = "dig2browser-station-v1";

/// Optional operator override for the pipe **suffix** (same validation as
/// dig2browser `validate_pipe_suffix`). Full Windows path is derived locally.
pub const STATION_PIPE_SUFFIX_ENV: &str = "GATE4AGENT_DIG2_STATION_PIPE";

/// Tight connect / WaitNamedPipe budget for the cheap probe (milliseconds).
#[cfg_attr(not(windows), allow(dead_code))]
pub const STATION_PROBE_TIMEOUT_MS: u32 = 250;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StationProbeError {
    /// Configured suffix failed dig2-style validation.
    InvalidPipeSuffix,
    /// dig2browser station IPC has no path mapping on this OS yet.
    UnsupportedPlatform,
    /// Named pipe missing or not connectable within the probe timeout.
    #[cfg_attr(not(windows), allow(dead_code))]
    Unreachable,
}

/// Mirror dig2browser-protocol `validate_pipe_suffix`: non-empty, ≤128 bytes,
/// ASCII alphanumeric / `-` / `_` / `.` only.
pub fn validate_pipe_suffix(value: &str) -> Result<(), StationProbeError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(StationProbeError::InvalidPipeSuffix);
    }
    Ok(())
}

/// Windows named-pipe path for a validated suffix (`\\.\pipe\{suffix}`).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn full_windows_pipe_path(suffix: &str) -> String {
    format!(r"\\.\pipe\{suffix}")
}

/// Resolve the configured pipe suffix (env override or default) and validate.
pub fn configured_pipe_suffix() -> Result<String, StationProbeError> {
    match std::env::var(STATION_PIPE_SUFFIX_ENV) {
        Ok(value) if !value.is_empty() => {
            validate_pipe_suffix(&value)?;
            Ok(value)
        }
        _ => Ok(DEFAULT_STATION_PIPE_SUFFIX.to_owned()),
    }
}

/// Probe local dig2browser-station IPC reachability (path / connect only).
///
/// Does **not** send `ImportSession`, Health frames with session bodies, or
/// read cookies. On non-Windows returns [`StationProbeError::UnsupportedPlatform`].
pub fn probe_station_reachable() -> Result<(), StationProbeError> {
    let suffix = configured_pipe_suffix()?;
    probe_station_reachable_with_suffix(&suffix)
}

/// Probe with an explicit validated-or-to-validate suffix (tests / callers).
pub fn probe_station_reachable_with_suffix(suffix: &str) -> Result<(), StationProbeError> {
    validate_pipe_suffix(suffix)?;
    #[cfg(windows)]
    {
        probe_windows_named_pipe(&full_windows_pipe_path(suffix))
    }
    #[cfg(not(windows))]
    {
        let _ = suffix;
        Err(StationProbeError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
fn probe_windows_named_pipe(full_path: &str) -> Result<(), StationProbeError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_FILE_NOT_FOUND};
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

    let wide: Vec<u16> = std::ffi::OsStr::new(full_path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // WaitNamedPipeW: returns immediately with FALSE if no instances exist
    // (ERROR_FILE_NOT_FOUND). Success means an instance is available to
    // connect — enough for path reachability without framing cookies.
    let ok = unsafe { WaitNamedPipeW(wide.as_ptr(), STATION_PROBE_TIMEOUT_MS) };
    if ok != 0 {
        return Ok(());
    }
    let err = unsafe { GetLastError() };
    if err == ERROR_FILE_NOT_FOUND {
        return Err(StationProbeError::Unreachable);
    }
    // Timeout / busy / other: treat as not connectable for this cheap probe.
    let _ = err;
    Err(StationProbeError::Unreachable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_pipe_suffix_accepts_dig2_default() {
        assert!(validate_pipe_suffix(DEFAULT_STATION_PIPE_SUFFIX).is_ok());
        assert!(validate_pipe_suffix("lab_station.v2").is_ok());
    }

    #[test]
    fn validate_pipe_suffix_rejects_empty_and_illegal() {
        assert_eq!(
            validate_pipe_suffix(""),
            Err(StationProbeError::InvalidPipeSuffix)
        );
        assert_eq!(
            validate_pipe_suffix(r"\\.\pipe\nope"),
            Err(StationProbeError::InvalidPipeSuffix)
        );
        assert_eq!(
            validate_pipe_suffix("has spaces"),
            Err(StationProbeError::InvalidPipeSuffix)
        );
        assert_eq!(
            validate_pipe_suffix(&"x".repeat(129)),
            Err(StationProbeError::InvalidPipeSuffix)
        );
    }

    #[test]
    fn full_windows_pipe_path_formats_suffix() {
        assert_eq!(
            full_windows_pipe_path(DEFAULT_STATION_PIPE_SUFFIX),
            r"\\.\pipe\dig2browser-station-v1"
        );
    }

    #[test]
    fn probe_on_non_windows_is_unsupported() {
        #[cfg(not(windows))]
        {
            assert_eq!(
                probe_station_reachable_with_suffix(DEFAULT_STATION_PIPE_SUFFIX),
                Err(StationProbeError::UnsupportedPlatform)
            );
        }
        #[cfg(windows)]
        {
            // Missing default station pipe → unreachable (no dig2 stationd in
            // unit fixtures). Never cookies.
            let result = probe_station_reachable_with_suffix(DEFAULT_STATION_PIPE_SUFFIX);
            assert!(
                matches!(
                    result,
                    Ok(()) | Err(StationProbeError::Unreachable)
                ),
                "unexpected probe result: {result:?}"
            );
        }
    }

    #[test]
    fn configured_suffix_defaults_when_env_unset() {
        let previous = std::env::var_os(STATION_PIPE_SUFFIX_ENV);
        std::env::remove_var(STATION_PIPE_SUFFIX_ENV);
        let suffix = configured_pipe_suffix().unwrap();
        match previous {
            Some(value) => std::env::set_var(STATION_PIPE_SUFFIX_ENV, value),
            None => std::env::remove_var(STATION_PIPE_SUFFIX_ENV),
        }
        assert_eq!(suffix, DEFAULT_STATION_PIPE_SUFFIX);
    }
}
