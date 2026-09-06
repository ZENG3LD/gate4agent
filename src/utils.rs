//! Internal utility functions shared across the crate.

use std::path::PathBuf;

/// Truncate a string to at most `max` bytes on a char boundary.
///
/// Canonical implementation — replaces all duplicates from original code.
pub(crate) fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Cross-platform home directory lookup without the `dirs` crate.
///
/// Tries `$HOME` first (Unix/MSYS), then `$USERPROFILE` (Windows).
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// Windows `CREATE_NO_WINDOW` process creation flag.
///
/// Exposed as a constant (not only through [`hide_console_window`]) so a
/// caller that already sets its own creation flags on the same `Command` --
/// e.g. `pipe::process::PipeProcess::configure_process_group`, which also
/// ORs in `CREATE_NEW_PROCESS_GROUP` -- can combine both bits in the single
/// `creation_flags` call Windows needs: calling `creation_flags` a second
/// time on the same `Command` replaces the flags already set, it does not
/// merge them.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Prevent a spawned child process from popping a console window on the
/// owner's desktop.
///
/// Every child process this crate spawns on Windows must carry
/// `CREATE_NO_WINDOW`, or `CreateProcess` allocates it a fresh console --
/// visible even for processes that only talk over pipes and never print to
/// a terminal. A no-op on every other platform (Unix has no such console
/// allocation to suppress). Touches nothing else on the `Command`: not the
/// program, args, env, working directory, or stdio wiring.
#[cfg(windows)]
pub(crate) fn hide_console_window(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn hide_console_window(_command: &mut std::process::Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hide_console_window_does_not_alter_program_or_args() {
        let mut command = std::process::Command::new("cmd");
        command.arg("/C").arg("exit 0");
        hide_console_window(&mut command);
        assert_eq!(command.get_program(), std::ffi::OsStr::new("cmd"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("/C"), std::ffi::OsStr::new("exit 0")]
        );
    }

    /// `std::os::windows::process::CommandExt` exposes only a setter for
    /// `creation_flags` -- there is no stable getter, so nothing in this
    /// crate can read the flag back off a `Command` to prove
    /// `CREATE_NO_WINDOW` actually reached the OS. What this test can
    /// honestly assert is that the constant handed to `creation_flags`
    /// matches the documented Windows value and every other site in this
    /// crate that already sets it directly (`pipe::process::PipeProcess::
    /// configure_process_group`, `pty::os_process`, `pty::process_tree`).
    /// Proof that no console window actually appears is a live check
    /// outside `cargo test` -- spawn any of the call sites this constant
    /// feeds and confirm nothing pops up on the desktop.
    #[cfg(windows)]
    #[test]
    fn create_no_window_constant_matches_the_documented_windows_value() {
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }
}

