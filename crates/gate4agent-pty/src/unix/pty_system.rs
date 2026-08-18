//! Unix `PtySystem`: `posix_openpt`/`grantpt`/`unlockpt` pty allocation, plus
//! the `MasterPty`/`SlavePty` implementations.

use std::cell::Cell;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::raw::c_int;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

use super::ffi;

/// Linux and macOS both number `EIO` as 5 (shared POSIX base errno set).
const EIO: i32 = 5;

#[derive(Default)]
pub(crate) struct UnixPtySystem;

impl crate::PtySystem for UnixPtySystem {
    fn openpty(&self, size: crate::PtySize) -> crate::Result<crate::PtyPair> {
        let (master, slave_name) = open_master()?;
        let slave = open_slave(&slave_name)?;
        // The initial size is applied to the SLAVE, not the master: on macOS
        // the master rejects winsize ioctls with ENOTTY until the slave side
        // of the tty exists (Linux happens to accept either). Post-slave,
        // `resize()` on the master works on both platforms.
        set_window_size(slave.as_raw_fd(), size)?;

        let master = UnixMasterPty {
            // SAFETY: `master` is a freshly created, uniquely owned master
            // pty fd; File takes ownership of it.
            file: unsafe { File::from_raw_fd(master.into_raw_fd()) },
            took_writer: Cell::new(false),
        };
        let slave = UnixSlavePty { fd: slave };

        Ok(crate::PtyPair {
            slave: Box::new(slave),
            master: Box::new(master),
        })
    }
}

fn open_master() -> crate::Result<(OwnedFd, CString)> {
    // SAFETY: posix_openpt takes only integer flags and returns either a
    // freshly allocated fd or -1/errno; no pointers are involved.
    let raw = unsafe { ffi::posix_openpt(ffi::O_RDWR | ffi::O_NOCTTY) };
    if raw < 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: raw is a valid fd just returned by posix_openpt; OwnedFd takes
    // exclusive ownership so every early-return below closes it via Drop.
    let master = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: master.as_raw_fd() is the fd owned above, valid for this call.
    if unsafe { ffi::grantpt(master.as_raw_fd()) } != 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: as above.
    if unsafe { ffi::unlockpt(master.as_raw_fd()) } != 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }

    let slave_name = pts_name(master.as_raw_fd())?;
    super::set_cloexec(master.as_raw_fd())?;

    Ok((master, slave_name))
}

fn open_slave(path: &CStr) -> crate::Result<OwnedFd> {
    // SAFETY: path is a valid, nul-terminated C string naming the slave
    // device just unlocked above; O_NOCTTY means opening it here does not
    // implicitly acquire a controlling terminal for this (parent) process —
    // the child acquires it explicitly via setsid()+TIOCSCTTY.
    let raw = unsafe { ffi::open(path.as_ptr(), ffi::O_RDWR | ffi::O_NOCTTY) };
    if raw < 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: raw is a valid fd just returned by open().
    let slave = unsafe { OwnedFd::from_raw_fd(raw) };
    super::set_cloexec(slave.as_raw_fd())?;
    Ok(slave)
}

#[cfg(target_os = "linux")]
fn pts_name(fd: c_int) -> crate::Result<CString> {
    let mut buf = vec![0u8; 128];
    // SAFETY: fd is a valid posix_openpt master; buf.len() bytes are
    // available at buf.as_mut_ptr() for ptsname_r to fill.
    let rc = unsafe {
        ffi::ptsname_r(
            fd,
            buf.as_mut_ptr() as *mut std::os::raw::c_char,
            buf.len(),
        )
    };
    if rc != 0 {
        // ptsname_r returns the error number directly; it does not set errno.
        return Err(crate::Error::Io(io::Error::from_raw_os_error(rc)));
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(end);
    CString::new(buf)
        .map_err(|_| crate::Error::Invalid("pty slave path contained an embedded NUL".into()))
}

/// macOS has no `ptsname_r`. `ptsname` writes into a static buffer that is
/// not reentrant/thread-safe, so every call in this process is serialized
/// through this lock; the returned pointer is copied into an owned
/// `CString` before the lock is released.
#[cfg(target_os = "macos")]
static PTSNAME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
fn pts_name(fd: c_int) -> crate::Result<CString> {
    let guard = PTSNAME_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // SAFETY: `guard` excludes every other caller of ptsname() in this
    // process; fd is a valid posix_openpt master.
    let ptr = unsafe { ffi::ptsname(fd) };
    if ptr.is_null() {
        drop(guard);
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: a non-null return is a valid nul-terminated C string owned by
    // libc's static buffer; it is copied out while `guard` is still held, so
    // no concurrent ptsname() call can overwrite it first.
    let owned = unsafe { CStr::from_ptr(ptr) }.to_owned();
    drop(guard);
    Ok(owned)
}

fn set_window_size(fd: c_int, size: crate::PtySize) -> crate::Result<()> {
    let ws = ffi::WinSize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    // SAFETY: fd is a valid pty master/slave fd; ws is a correctly laid out
    // `struct winsize` the kernel reads from for the duration of the call.
    let rc = unsafe { ffi::ioctl(fd, ffi::TIOCSWINSZ, &ws as *const ffi::WinSize) };
    if rc != 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    Ok(())
}

fn get_window_size(fd: c_int) -> crate::Result<crate::PtySize> {
    let mut ws = ffi::WinSize::default();
    // SAFETY: fd is a valid pty master fd; ws is writable for the duration
    // of the call.
    let rc = unsafe { ffi::ioctl(fd, ffi::TIOCGWINSZ, &mut ws as *mut ffi::WinSize) };
    if rc != 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    Ok(crate::PtySize {
        rows: ws.ws_row,
        cols: ws.ws_col,
        pixel_width: ws.ws_xpixel,
        pixel_height: ws.ws_ypixel,
    })
}

/// Wraps the master pty's `File` for reads only, translating `EIO` (some
/// Linux kernels report this instead of a clean 0-byte read when the slave
/// side has closed) into a plain EOF so `Read::read_to_end`-style consumers
/// terminate gracefully instead of treating pty teardown as an I/O error.
struct PtyReader(File);

impl io::Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buf) {
            Err(e) if e.raw_os_error() == Some(EIO) => Ok(0),
            other => other,
        }
    }
}

pub(crate) struct UnixMasterPty {
    file: File,
    took_writer: Cell<bool>,
}

impl crate::MasterPty for UnixMasterPty {
    fn resize(&self, size: crate::PtySize) -> crate::Result<()> {
        set_window_size(self.file.as_raw_fd(), size)
    }

    fn get_size(&self) -> crate::Result<crate::PtySize> {
        get_window_size(self.file.as_raw_fd())
    }

    fn try_clone_reader(&self) -> crate::Result<Box<dyn io::Read + Send>> {
        let cloned = self.file.try_clone()?;
        Ok(Box::new(PtyReader(cloned)))
    }

    fn take_writer(&self) -> crate::Result<Box<dyn io::Write + Send>> {
        if self.took_writer.replace(true) {
            return Err(crate::Error::Invalid(
                "cannot take the pty writer more than once".into(),
            ));
        }
        let cloned = self.file.try_clone()?;
        Ok(Box::new(cloned))
    }

    fn process_group_leader(&self) -> Option<super::Pid> {
        // SAFETY: self.file's fd is a valid, open pty master for the
        // lifetime of `&self`.
        let pid = unsafe { ffi::tcgetpgrp(self.file.as_raw_fd()) };
        if pid > 0 {
            Some(pid)
        } else {
            None
        }
    }
}

pub(crate) struct UnixSlavePty {
    pub(crate) fd: OwnedFd,
}

impl crate::SlavePty for UnixSlavePty {
    fn spawn_command(&self, cmd: crate::CommandBuilder) -> crate::Result<Box<dyn crate::Child + Send + Sync>> {
        super::spawn::spawn_command(self.fd.as_raw_fd(), cmd)
    }
}

/// Used by `CommandBuilder::resolve_program` to check candidate executables
/// found while walking `$PATH`.
pub(crate) fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    // No let-else: the Linux verification boxes run rustc 1.63 (see mod.rs).
    let cpath = match CString::new(path.as_os_str().as_bytes()) {
        Ok(cpath) => cpath,
        Err(_) => return false,
    };
    // SAFETY: cpath is a valid, nul-terminated C string for this call only.
    unsafe { ffi::access(cpath.as_ptr(), ffi::X_OK) == 0 }
}
