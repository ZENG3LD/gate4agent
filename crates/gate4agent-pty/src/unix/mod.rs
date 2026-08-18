//! Unix (macOS/Linux) pty backend: `posix_openpt` + fork/exec on raw
//! `extern "C"` POSIX FFI. Std-only — no `libc`/`nix` crate.
//!
//! Floor: rustc 1.63 (the Linux verification boxes report cargo 1.65 but
//! carry Debian's rustc 1.63 underneath — the compiler is the floor). Every
//! item in this crate sticks to APIs and syntax stable at or before 1.63:
//! `std::os::unix::io::OwnedFd`/`AsRawFd`/`FromRawFd`/`IntoRawFd` (1.63),
//! `std::sync::Mutex::new` as a `const fn` (1.63), and
//! `std::os::unix::ffi::OsStrExt` (since 1.0). No `let ... else` (1.65), no
//! `OsStr::as_encoded_bytes` (1.74), no `std::sync::OnceLock` (1.70).

mod ffi;
mod pty_system;
mod spawn;
#[cfg(test)]
mod tests;

pub(crate) use ffi::Pid;
pub(crate) use pty_system::{is_executable_file, UnixPtySystem};

use std::io;
use std::os::raw::c_int;

/// Sets the close-on-exec flag on `fd` via `fcntl(F_GETFD)`/`fcntl(F_SETFD)`
/// — the same read-modify-write pattern used throughout this backend so a
/// pty/pipe fd this crate opens is never inherited by an unrelated spawn.
pub(crate) fn set_cloexec(fd: c_int) -> crate::Result<()> {
    // SAFETY: fd is a valid, open descriptor owned by the caller for the
    // duration of this call.
    let flags = unsafe { ffi::fcntl(fd, ffi::F_GETFD) };
    if flags < 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: as above; `flags` was just read successfully from the same fd.
    let result = unsafe { ffi::fcntl(fd, ffi::F_SETFD, flags | ffi::FD_CLOEXEC) };
    if result < 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    Ok(())
}
