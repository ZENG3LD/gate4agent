//! In-house cross-platform PTY backend for gate4agent.
//!
//! Two real, independent backends:
//! - Windows: ConPTY via hand-typed `extern "system"` Win32 FFI (see [`windows`]).
//! - Unix (macOS/Linux): `posix_openpt`/fork+exec via hand-typed `extern "C"` FFI
//!   (see [`unix`]).
//!
//! Std-only. No `winapi`/`windows-sys`/`libc`/`nix`/`anyhow` — every declaration in
//! this crate is either plain `std` or a raw FFI binding written for this crate.
//!
//! ```no_run
//! use gate4agent_pty::{native_pty_system, CommandBuilder, PtySize};
//!
//! let pty_system = native_pty_system();
//! let pair = pty_system.openpty(PtySize::default())?;
//! let cmd = CommandBuilder::new("bash");
//! let mut child = pair.slave.spawn_command(cmd)?;
//! let _reader = pair.master.try_clone_reader()?;
//! child.wait()?;
//! # Ok::<(), gate4agent_pty::Error>(())
//! ```

mod cmdbuilder;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

pub use cmdbuilder::CommandBuilder;

use std::io;

/// The size of a pty's viewport, in character cells and (optionally) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    /// Number of lines of text.
    pub rows: u16,
    /// Number of columns of text.
    pub cols: u16,
    /// Width of a cell in pixels. Not all systems honor this.
    pub pixel_width: u16,
    /// Height of a cell in pixels. Not all systems honor this.
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// Errors produced by PTY creation, resize, and process-spawn setup.
///
/// `Child`/`ChildKiller` lifecycle operations (`wait`, `try_wait`, `kill`) report
/// `std::io::Error` directly instead of this type, because callers match on
/// `raw_os_error()` to special-case a Win32 "already exited" race.
#[derive(Debug)]
pub enum Error {
    /// The platform's ConPTY entry points could not be resolved. Distinguishes
    /// "this Windows build predates ConPTY" from every other failure so callers
    /// can react to it specifically instead of pattern-matching an opaque
    /// string. Never produced as a panic — see `windows::conpty::load_conpty`.
    #[cfg(windows)]
    ConPtyUnavailable(String),
    /// `CreatePseudoConsole`/`ResizePseudoConsole` returned a failing `HRESULT`.
    #[cfg(windows)]
    ConPtyCall {
        operation: &'static str,
        hresult: i32,
    },
    /// Process creation failed (`CreateProcessW` on Windows, fork/exec on unix).
    Spawn(io::Error),
    /// Any other I/O failure (pipe/pty device creation, ioctl, ...).
    Io(io::Error),
    /// A precondition was violated by the caller (e.g. the writer was already
    /// taken, or the master fd/handle was not backed by an executable path).
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(windows)]
            Error::ConPtyUnavailable(reason) => {
                write!(f, "ConPTY is unavailable on this system: {reason}")
            }
            #[cfg(windows)]
            Error::ConPtyCall { operation, hresult } => {
                write!(f, "{operation} failed: HRESULT 0x{hresult:08x}")
            }
            Error::Spawn(e) => write!(f, "failed to spawn process: {e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Spawn(e) | Error::Io(e) => Some(e),
            #[cfg(windows)]
            Error::ConPtyUnavailable(_) | Error::ConPtyCall { .. } => None,
            Error::Invalid(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

/// A convenience alias for this crate's fallible results.
pub type Result<T> = std::result::Result<T, Error>;

/// The exit status of a child process spawned into a pty.
#[derive(Debug, Clone)]
pub struct ExitStatus {
    code: u32,
    signal: Option<String>,
}

impl ExitStatus {
    /// Construct an `ExitStatus` from a process return code.
    pub fn with_exit_code(code: u32) -> Self {
        Self { code, signal: None }
    }

    /// Construct an `ExitStatus` from a signal name. Used internally by the
    /// unix backend when `waitpid` reports the child died from a signal.
    #[cfg(unix)]
    pub(crate) fn with_signal(signal: &str, core_dumped: bool) -> Self {
        Self {
            code: 1,
            signal: Some(if core_dumped {
                format!("{signal} (core dumped)")
            } else {
                signal.to_owned()
            }),
        }
    }

    /// True if the status indicates successful completion.
    pub fn success(&self) -> bool {
        match self.signal {
            None => self.code == 0,
            Some(_) => false,
        }
    }

    /// The exit code this `ExitStatus` was constructed with.
    pub fn exit_code(&self) -> u32 {
        self.code
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.success() {
            write!(f, "Success")
        } else {
            match &self.signal {
                Some(signal) => write!(f, "Terminated by {signal}"),
                None => write!(f, "Exited with code {}", self.code),
            }
        }
    }
}

/// Represents the master/control end of the pty.
pub trait MasterPty {
    /// Inform the kernel (and thus the child process) that the window
    /// resized.
    fn resize(&self, size: PtySize) -> Result<()>;
    /// Retrieve the size of the pty as known by the kernel.
    fn get_size(&self) -> Result<PtySize>;
    /// Obtain a readable handle; output from the slave is readable via this
    /// stream.
    fn try_clone_reader(&self) -> Result<Box<dyn io::Read + Send>>;
    /// Obtain a writable handle; writing to it sends data to the slave end.
    /// It is invalid to take the writer more than once.
    fn take_writer(&self) -> Result<Box<dyn io::Write + Send>>;
    /// The process group ID of the pty's foreground process group, if any.
    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<unix::Pid>;
}

/// Represents a child process spawned into the pty. Can be used to wait for
/// or terminate that child process.
pub trait Child: std::fmt::Debug + ChildKiller {
    /// Poll the child to see if it has completed. Does not block. Returns
    /// `None` if the child has not yet terminated, else its exit status.
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>>;
    /// Blocks until the child process has completed, yielding its exit
    /// status.
    fn wait(&mut self) -> io::Result<ExitStatus>;
    /// The process identifier of the child process, if applicable.
    fn process_id(&self) -> Option<u32>;
}

/// The ability to signal a `Child` to terminate.
pub trait ChildKiller: std::fmt::Debug {
    /// Terminate the child process.
    fn kill(&mut self) -> io::Result<()>;
    /// Clone an object that can be split out from the `Child` in order to
    /// send it signals independently from a thread that may be blocked in
    /// `.wait()`.
    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync>;
}

/// Represents the slave side of a pty. Used to spawn processes into the pty.
pub trait SlavePty {
    /// Spawn the command specified by the provided `CommandBuilder`.
    fn spawn_command(&self, cmd: CommandBuilder) -> Result<Box<dyn Child + Send + Sync>>;
}

/// A freshly created master/slave pty pair.
pub struct PtyPair {
    // slave is listed first so that it is dropped first; drop order is
    // stable and specified by RFC 1857.
    pub slave: Box<dyn SlavePty + Send>,
    pub master: Box<dyn MasterPty + Send>,
}

/// Allows an application to work with multiple possible pty implementations
/// at runtime.
pub trait PtySystem {
    /// Create a new pty instance with the window size set to the specified
    /// dimensions. Returns a (master, slave) pty pair; the master side is
    /// used to drive the slave side.
    fn openpty(&self, size: PtySize) -> Result<PtyPair>;
}

#[cfg(windows)]
type NativePtySystem = windows::ConPtySystem;
#[cfg(unix)]
type NativePtySystem = unix::UnixPtySystem;

/// The pty backend native to the current platform.
pub fn native_pty_system() -> Box<dyn PtySystem + Send> {
    Box::new(NativePtySystem::default())
}
