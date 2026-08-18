//! fork+setsid+dup2+execve process spawning and the `Child`/`ChildKiller`
//! lifecycle for the unix pty backend.

use std::ffi::CString;
use std::io;
use std::os::raw::{c_int, c_void};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};

use super::ffi;

/// Shared POSIX base errno values (identical on Linux and macOS).
const ESRCH: i32 = 3;
const EINTR: i32 = 4;
const ECHILD: i32 = 10;

pub(crate) fn spawn_command(
    slave_fd: c_int,
    cmd: crate::CommandBuilder,
) -> crate::Result<Box<dyn crate::Child + Send + Sync>> {
    let (program, argv) = cmd.exec_argv()?;
    let envp = cmd.exec_envp()?;
    let cwd = cmd.exec_cwd()?;

    let argv_ptrs = null_terminated_ptrs(&argv);
    let envp_ptrs = null_terminated_ptrs(&envp);
    let cwd_ptr = cwd
        .as_ref()
        .map_or(std::ptr::null(), |c| c.as_ptr());
    let program_ptr = program.as_ptr();

    let (err_read, err_write) = create_pipe()?;
    super::set_cloexec(err_read.as_raw_fd())?;
    super::set_cloexec(err_write.as_raw_fd())?;

    // SAFETY: fork() duplicates this process; the parent branch below only
    // touches already-initialized Rust values, and the child branch (pid ==
    // 0) only calls async-signal-safe primitives before execve()/_exit().
    let pid = unsafe { ffi::fork() };
    if pid < 0 {
        return Err(crate::Error::Spawn(io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: this is the freshly forked child; program_ptr/argv_ptrs/
        // envp_ptrs/cwd_ptr all point at memory allocated in the parent
        // before fork(), which the child inherited a valid read-only
        // (copy-on-write) view of. err_write is this child's own fd.
        unsafe {
            child_exec(
                slave_fd,
                cwd_ptr,
                program_ptr,
                &argv_ptrs,
                &envp_ptrs,
                err_write.as_raw_fd(),
            );
        }
        // child_exec never returns.
    }

    // Parent: close our copy of the write end so, on a successful exec, its
    // CLOEXEC closure is observable here as a plain EOF (0-byte read).
    drop(err_write);
    let exec_error = read_exec_error(err_read.as_raw_fd());
    drop(err_read);

    if let Some(errno) = exec_error {
        reap_failed_child(pid);
        return Err(crate::Error::Spawn(io::Error::from_raw_os_error(errno)));
    }

    Ok(Box::new(UnixChild { pid, exit: None }))
}

/// Runs in the child between `fork()` and `execve()`/`_exit()`. Only calls
/// async-signal-safe primitives: no allocation, no Rust std I/O, no locks.
unsafe fn child_exec(
    slave_fd: c_int,
    cwd: *const std::os::raw::c_char,
    program: *const std::os::raw::c_char,
    argv: &[*const std::os::raw::c_char],
    envp: &[*const std::os::raw::c_char],
    err_write: c_int,
) -> ! {
    // Reset signal dispositions inherited from the (possibly multi-threaded)
    // parent to their defaults before the new program takes over.
    for signal in [
        ffi::SIGHUP,
        ffi::SIGINT,
        ffi::SIGQUIT,
        ffi::SIGTERM,
        ffi::SIGALRM,
        ffi::SIGCHLD,
    ] {
        ffi::signal(signal, ffi::SIG_DFL);
    }

    if ffi::setsid() < 0 {
        report_and_exit(err_write);
    }
    // Steal the controlling terminal for this new session (third arg 0:
    // don't forcibly steal it from another still-attached session).
    if ffi::ioctl(slave_fd, ffi::TIOCSCTTY, 0 as c_int) < 0 {
        report_and_exit(err_write);
    }
    for fd in 0..3 {
        if ffi::dup2(slave_fd, fd) < 0 {
            report_and_exit(err_write);
        }
    }
    if slave_fd > 2 {
        ffi::close(slave_fd);
    }

    if !cwd.is_null() && ffi::chdir(cwd) != 0 {
        report_and_exit(err_write);
    }

    ffi::execve(program, argv.as_ptr(), envp.as_ptr());
    // execve() only returns on failure.
    report_and_exit(err_write);
}

/// Writes the current errno to the exec-status pipe and exits. Never
/// returns. Uses `std::io::Error::last_os_error()` (a plain errno read, not
/// an allocation) rather than declaring `__errno_location`/`__error`
/// ourselves, since that accessor's symbol name differs between Linux and
/// macOS.
unsafe fn report_and_exit(err_write: c_int) -> ! {
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(-1);
    let bytes = errno.to_ne_bytes();
    let mut written = 0usize;
    while written < bytes.len() {
        let n = ffi::write(
            err_write,
            bytes[written..].as_ptr() as *const c_void,
            bytes.len() - written,
        );
        if n <= 0 {
            break;
        }
        written += n as usize;
    }
    ffi::_exit(127);
}

fn null_terminated_ptrs(values: &[CString]) -> Vec<*const std::os::raw::c_char> {
    let mut ptrs: Vec<*const std::os::raw::c_char> = values.iter().map(|v| v.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    ptrs
}

fn create_pipe() -> crate::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: fds is a valid 2-element buffer for pipe(2) to fill.
    let rc = unsafe { ffi::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: pipe(2) succeeded, so fds[0]/fds[1] are freshly opened, valid,
    // process-unique descriptors this call now owns exclusively.
    unsafe {
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

/// Reads the child's reported errno, retrying across `EINTR`. `None` means
/// the write end closed (via `CLOEXEC`) without ever sending a value, i.e.
/// `execve` succeeded.
fn read_exec_error(fd: c_int) -> Option<i32> {
    let mut buf = [0u8; 4];
    let mut filled = 0usize;
    while filled < buf.len() {
        // SAFETY: fd is the parent's read end of the exec-status pipe; the
        // slice starting at `filled` has `buf.len() - filled` bytes
        // available for read(2) to fill.
        let n = unsafe {
            ffi::read(
                fd,
                buf[filled..].as_mut_ptr() as *mut c_void,
                buf.len() - filled,
            )
        };
        if n == 0 {
            return None;
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(EINTR) {
                continue;
            }
            return None;
        }
        filled += n as usize;
    }
    Some(i32::from_ne_bytes(buf))
}

/// Reaps a child that reported an exec/setup failure over the status pipe,
/// so it does not linger as a zombie.
fn reap_failed_child(pid: super::Pid) {
    let mut status: c_int = 0;
    loop {
        // SAFETY: pid is the pid this call just forked and knows failed
        // before ever running the target program.
        let r = unsafe { ffi::waitpid(pid, &mut status, 0) };
        if r >= 0 {
            return;
        }
        if io::Error::last_os_error().raw_os_error() != Some(EINTR) {
            return;
        }
    }
}

fn decode_status(status: c_int) -> crate::ExitStatus {
    if ffi::wifexited(status) {
        crate::ExitStatus::with_exit_code(ffi::wexitstatus(status) as u32)
    } else if ffi::wifsignaled(status) {
        let signal = ffi::wtermsig(status);
        crate::ExitStatus::with_signal(&ffi::signal_name(signal), ffi::wcoredump(status))
    } else {
        crate::ExitStatus::with_exit_code(1)
    }
}

/// Sends a signal to `pid`. A process that has already exited (`ESRCH`) is
/// reported as success: killing an already-dead process already achieved
/// the caller's goal, matching the Windows ConPTY backend's "already
/// exited" handling for the same call site (`ChildKiller::kill`).
fn send_signal(pid: super::Pid, signal: c_int) -> io::Result<()> {
    // SAFETY: pid was returned by fork() in this process; kill(2) on a pid
    // that has already exited is well-defined (ESRCH), not UB.
    let rc = unsafe { ffi::kill(pid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(ESRCH) {
        return Ok(());
    }
    Err(err)
}

#[derive(Debug)]
struct UnixChild {
    pid: super::Pid,
    exit: Option<crate::ExitStatus>,
}

impl crate::Child for UnixChild {
    fn try_wait(&mut self) -> io::Result<Option<crate::ExitStatus>> {
        if let Some(status) = &self.exit {
            return Ok(Some(status.clone()));
        }
        let mut status: c_int = 0;
        loop {
            // SAFETY: self.pid is this object's own owned child pid; WNOHANG
            // makes this call non-blocking.
            let r = unsafe { ffi::waitpid(self.pid, &mut status, ffi::WNOHANG) };
            if r == self.pid {
                let exit = decode_status(status);
                self.exit = Some(exit.clone());
                return Ok(Some(exit));
            }
            if r == 0 {
                return Ok(None);
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(EINTR) {
                continue;
            }
            if err.raw_os_error() == Some(ECHILD) {
                // Already reaped by a previous wait(): nothing left to
                // report, but not a failure either.
                return Ok(None);
            }
            return Err(err);
        }
    }

    fn wait(&mut self) -> io::Result<crate::ExitStatus> {
        if let Some(status) = &self.exit {
            return Ok(status.clone());
        }
        let mut status: c_int = 0;
        loop {
            // SAFETY: as in try_wait; options=0 makes this call blocking.
            let r = unsafe { ffi::waitpid(self.pid, &mut status, 0) };
            if r == self.pid {
                let exit = decode_status(status);
                self.exit = Some(exit.clone());
                return Ok(exit);
            }
            if r < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(EINTR) {
                    continue;
                }
                return Err(err);
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        u32::try_from(self.pid).ok()
    }
}

impl crate::ChildKiller for UnixChild {
    fn kill(&mut self) -> io::Result<()> {
        send_signal(self.pid, ffi::SIGKILL)
    }

    fn clone_killer(&self) -> Box<dyn crate::ChildKiller + Send + Sync> {
        Box::new(UnixChildKiller { pid: self.pid })
    }
}

#[derive(Debug, Clone)]
struct UnixChildKiller {
    pid: super::Pid,
}

impl crate::ChildKiller for UnixChildKiller {
    fn kill(&mut self) -> io::Result<()> {
        send_signal(self.pid, ffi::SIGKILL)
    }

    fn clone_killer(&self) -> Box<dyn crate::ChildKiller + Send + Sync> {
        Box::new(self.clone())
    }
}
