//! Raw `extern "C"` POSIX declarations for the unix pty/process backend.
//!
//! Std-only: these are hand-typed bindings against the platform C library,
//! not the `libc` crate. Every constant below is either identical across
//! Linux and macOS (documented POSIX values, e.g. `X_OK`, `WNOHANG`,
//! `F_SETFD`/`FD_CLOEXEC`) or is `cfg`-gated per target where the two
//! platforms genuinely diverge (open(2) flags, ioctl request numbers, and
//! `SIGCHLD`'s signal number).
//!
//! Floor: this module and everything under `unix/` must compile under
//! rustc/cargo 1.65 — the Linux verification boxes run 1.65. Avoid any std
//! API stabilized after 1.65 (e.g. `OsStr::as_encoded_bytes`, `File::create_new`);
//! use `std::os::unix::ffi::OsStrExt` (stable since 1.0) instead.

use std::os::raw::{c_char, c_int, c_void};

/// POSIX `pid_t` is a 32-bit signed integer on both Linux and macOS.
pub(crate) type Pid = i32;
/// POSIX `ssize_t` is pointer-width and signed on both targets.
type SSizeT = isize;
/// `size_t` is pointer-width and unsigned on both targets.
type SizeT = usize;
/// `sighandler_t` — POSIX types it as a function pointer, but every access
/// this module makes to it is the numeric constant `SIG_DFL`, so it is typed
/// as `usize` here (the same simplification the `libc` crate itself uses for
/// `sighandler_t`), avoiding an FFI function-pointer transmute for a value we
/// never call.
type SigHandler = usize;

// ---------------------------------------------------------------------------
// open(2) flags
// ---------------------------------------------------------------------------

pub(crate) const O_RDWR: c_int = 2; // identical on Linux and macOS

#[cfg(target_os = "linux")]
pub(crate) const O_NOCTTY: c_int = 0o400; // asm-generic/fcntl.h
#[cfg(target_os = "macos")]
pub(crate) const O_NOCTTY: c_int = 0x20000; // sys/fcntl.h

// ---------------------------------------------------------------------------
// fcntl(2): CLOEXEC hygiene. F_GETFD/F_SETFD/FD_CLOEXEC are the same on
// Linux and macOS (both trace to the original AT&T fcntl.h numbering).
// ---------------------------------------------------------------------------

pub(crate) const F_GETFD: c_int = 1;
pub(crate) const F_SETFD: c_int = 2;
pub(crate) const FD_CLOEXEC: c_int = 1;

// ---------------------------------------------------------------------------
// access(2) mode bits — POSIX-standard values, shared across platforms.
// ---------------------------------------------------------------------------

pub(crate) const X_OK: c_int = 1;

// ---------------------------------------------------------------------------
// ioctl(2) request numbers. These are NOT portable between Linux and BSD-
// derived (macOS) kernels; each is derived below from the platform's own
// `_IOC`/`_IOR`/`_IOW`/`_IO` encoding macros and documented alongside its
// derivation so the constant is auditable without a live box.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod ioctl_numbers {
    use std::os::raw::c_ulong;
    // asm-generic/ioctls.h — stable across Linux architectures that matter
    // here (x86_64, aarch64).
    pub(crate) const TIOCSCTTY: c_ulong = 0x540E;
    pub(crate) const TIOCGWINSZ: c_ulong = 0x5413;
    pub(crate) const TIOCSWINSZ: c_ulong = 0x5414;
}

#[cfg(target_os = "macos")]
mod ioctl_numbers {
    use std::os::raw::c_ulong;
    // sys/ttycom.h + sys/ioccom.h encoding:
    //   IOC_VOID=0x2000_0000 IOC_OUT=0x4000_0000 IOC_IN=0x8000_0000
    //   _IOC(inout,group,num,len) = inout | ((len & 0x1fff) << 16) | (group << 8) | num
    // TIOCSCTTY  = _IO('t', 97)                     = 0x2000_0000 | ('t'<<8) | 97
    // TIOCGWINSZ = _IOR('t', 104, struct winsize)    = 0x4000_0000 | (8<<16) | ('t'<<8) | 104
    // TIOCSWINSZ = _IOW('t', 103, struct winsize)    = 0x8000_0000 | (8<<16) | ('t'<<8) | 103
    // (sizeof(struct winsize) == 8: four u16 fields)
    pub(crate) const TIOCSCTTY: c_ulong = 0x2000_7461;
    pub(crate) const TIOCGWINSZ: c_ulong = 0x4008_7468;
    pub(crate) const TIOCSWINSZ: c_ulong = 0x8008_7467;
}

pub(crate) use ioctl_numbers::{TIOCGWINSZ, TIOCSCTTY, TIOCSWINSZ};

/// `struct winsize` — identical field layout on Linux and macOS.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WinSize {
    pub(crate) ws_row: u16,
    pub(crate) ws_col: u16,
    pub(crate) ws_xpixel: u16,
    pub(crate) ws_ypixel: u16,
}

// ---------------------------------------------------------------------------
// Signals. Only SIGCHLD's number differs between Linux and macOS/BSD; every
// other signal used here is numbered identically on both.
// ---------------------------------------------------------------------------

pub(crate) const SIGHUP: c_int = 1;
pub(crate) const SIGINT: c_int = 2;
pub(crate) const SIGQUIT: c_int = 3;
pub(crate) const SIGKILL: c_int = 9;
pub(crate) const SIGALRM: c_int = 14;
pub(crate) const SIGTERM: c_int = 15;
#[cfg(target_os = "linux")]
pub(crate) const SIGCHLD: c_int = 17;
#[cfg(target_os = "macos")]
pub(crate) const SIGCHLD: c_int = 20;

pub(crate) const SIG_DFL: SigHandler = 0;

// ---------------------------------------------------------------------------
// waitpid(2) options and wait-status decoding. The low-level encoding of the
// wait status word (low 7 bits = termination cause, next byte = exit code)
// is shared, ancient AT&T Unix ABI, consistent across Linux and macOS.
// ---------------------------------------------------------------------------

pub(crate) const WNOHANG: c_int = 1;

pub(crate) fn wifexited(status: c_int) -> bool {
    (status & 0x7f) == 0
}

pub(crate) fn wexitstatus(status: c_int) -> c_int {
    (status >> 8) & 0xff
}

pub(crate) fn wifsignaled(status: c_int) -> bool {
    let low7 = status & 0x7f;
    low7 != 0 && low7 != 0x7f
}

pub(crate) fn wtermsig(status: c_int) -> c_int {
    status & 0x7f
}

pub(crate) fn wcoredump(status: c_int) -> bool {
    (status & 0x80) != 0
}

/// Best-effort signal name for `ExitStatus` display. Falls back to `signal
/// N` for anything not in this shared table, rather than depending on
/// `strsignal`'s locale-sensitive static buffer.
pub(crate) fn signal_name(signal: c_int) -> String {
    match signal {
        1 => "SIGHUP".to_owned(),
        2 => "SIGINT".to_owned(),
        3 => "SIGQUIT".to_owned(),
        4 => "SIGILL".to_owned(),
        5 => "SIGTRAP".to_owned(),
        6 => "SIGABRT".to_owned(),
        8 => "SIGFPE".to_owned(),
        9 => "SIGKILL".to_owned(),
        11 => "SIGSEGV".to_owned(),
        13 => "SIGPIPE".to_owned(),
        14 => "SIGALRM".to_owned(),
        15 => "SIGTERM".to_owned(),
        other if other == SIGCHLD => "SIGCHLD".to_owned(),
        other => format!("signal {other}"),
    }
}

// ---------------------------------------------------------------------------
// extern "C" declarations. Each call site carries its own `SAFETY:` comment;
// these blocks only declare the ABI shape.
// ---------------------------------------------------------------------------

extern "C" {
    pub(crate) fn posix_openpt(oflag: c_int) -> c_int;
    pub(crate) fn grantpt(fd: c_int) -> c_int;
    pub(crate) fn unlockpt(fd: c_int) -> c_int;

    #[cfg(target_os = "linux")]
    pub(crate) fn ptsname_r(fd: c_int, buf: *mut c_char, buflen: SizeT) -> c_int;
    /// macOS has no `ptsname_r`. `ptsname` writes into a static buffer that
    /// is not thread-safe; callers serialize access with `PTSNAME_LOCK`.
    #[cfg(target_os = "macos")]
    pub(crate) fn ptsname(fd: c_int) -> *mut c_char;

    pub(crate) fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    pub(crate) fn close(fd: c_int) -> c_int;
    pub(crate) fn read(fd: c_int, buf: *mut c_void, count: SizeT) -> SSizeT;
    pub(crate) fn write(fd: c_int, buf: *const c_void, count: SizeT) -> SSizeT;
    pub(crate) fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    pub(crate) fn pipe(pipefd: *mut c_int) -> c_int;
    pub(crate) fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    pub(crate) fn ioctl(fd: c_int, request: std::os::raw::c_ulong, ...) -> c_int;
    pub(crate) fn access(path: *const c_char, mode: c_int) -> c_int;
    pub(crate) fn chdir(path: *const c_char) -> c_int;

    pub(crate) fn fork() -> Pid;
    pub(crate) fn setsid() -> Pid;
    pub(crate) fn execve(
        path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> c_int;
    /// Never returns; the child calls this instead of `exit(3)` after
    /// `fork()` so C-runtime atexit handlers and stdio buffers inherited
    /// from the parent are not flushed twice.
    pub(crate) fn _exit(status: c_int) -> !;

    pub(crate) fn waitpid(pid: Pid, status: *mut c_int, options: c_int) -> Pid;
    pub(crate) fn kill(pid: Pid, sig: c_int) -> c_int;
    pub(crate) fn tcgetpgrp(fd: c_int) -> Pid;
    pub(crate) fn signal(signum: c_int, handler: SigHandler) -> SigHandler;
}
