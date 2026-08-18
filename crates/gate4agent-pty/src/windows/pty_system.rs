//! `ConPtySystem`/`ConPtyMasterPty`/`ConPtySlavePty`: the `PtySystem`/
//! `MasterPty`/`SlavePty` implementations, plus anonymous-pipe creation for
//! the pseudoconsole's stdin/stdout.

use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;
use std::sync::{Arc, Mutex};

use super::conpty::PsuedoCon;
use super::ffi::{self, Coord, Handle};
use super::handle::OwnedHandle;

#[derive(Default)]
pub(crate) struct ConPtySystem;

impl crate::PtySystem for ConPtySystem {
    fn openpty(&self, size: crate::PtySize) -> crate::Result<crate::PtyPair> {
        let (stdin_read, stdin_write) = anonymous_pipe()?;
        let (stdout_read, stdout_write) = anonymous_pipe()?;

        let con = PsuedoCon::new(
            Coord {
                x: size.cols as i16,
                y: size.rows as i16,
            },
            stdin_read.as_raw(),
            stdout_write.as_raw(),
        )?;
        // CreatePseudoConsole duplicates the handles it needs internally;
        // our copies of its two facing ends are no longer needed once
        // PsuedoCon::new has returned successfully.
        drop(stdin_read);
        drop(stdout_write);

        let inner = Inner {
            con,
            // SAFETY: stdout_read/stdin_write are freshly created,
            // uniquely owned pipe handles; File takes over ownership and
            // closes them on drop.
            readable: unsafe { File::from_raw_handle(stdout_read.into_raw() as std::os::windows::io::RawHandle) },
            writable: Some(unsafe {
                File::from_raw_handle(stdin_write.into_raw() as std::os::windows::io::RawHandle)
            }),
            size,
        };
        let inner = Arc::new(Mutex::new(inner));

        let master = ConPtyMasterPty {
            inner: inner.clone(),
        };
        let slave = ConPtySlavePty { inner };

        Ok(crate::PtyPair {
            slave: Box::new(slave),
            master: Box::new(master),
        })
    }
}

fn anonymous_pipe() -> crate::Result<(OwnedHandle, OwnedHandle)> {
    let mut read_handle: Handle = std::ptr::null_mut();
    let mut write_handle: Handle = std::ptr::null_mut();
    // SAFETY: read_handle/write_handle are valid out-pointers; a null
    // security-attributes pointer and a 0 size request the default
    // (non-inheritable) security descriptor and system default buffer
    // size, matching Microsoft's own ConPTY sample.
    let ok = unsafe { ffi::create_pipe(&mut read_handle, &mut write_handle, std::ptr::null(), 0) };
    if ok == 0 {
        return Err(crate::Error::Io(io::Error::last_os_error()));
    }
    // SAFETY: CreatePipe succeeded, so both handles are freshly created,
    // valid, and exclusively owned by this call.
    unsafe {
        Ok((
            OwnedHandle::from_raw(read_handle),
            OwnedHandle::from_raw(write_handle),
        ))
    }
}

struct Inner {
    con: PsuedoCon,
    readable: File,
    writable: Option<File>,
    size: crate::PtySize,
}

impl Inner {
    fn resize(&mut self, size: crate::PtySize) -> crate::Result<()> {
        self.con.resize(Coord {
            x: size.cols as i16,
            y: size.rows as i16,
        })?;
        self.size = size;
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ConPtyMasterPty {
    inner: Arc<Mutex<Inner>>,
}

pub(crate) struct ConPtySlavePty {
    inner: Arc<Mutex<Inner>>,
}

impl crate::MasterPty for ConPtyMasterPty {
    fn resize(&self, size: crate::PtySize) -> crate::Result<()> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.resize(size)
    }

    fn get_size(&self) -> crate::Result<crate::PtySize> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(inner.size)
    }

    fn try_clone_reader(&self) -> crate::Result<Box<dyn io::Read + Send>> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(Box::new(inner.readable.try_clone()?))
    }

    fn take_writer(&self) -> crate::Result<Box<dyn io::Write + Send>> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let writer = inner
            .writable
            .take()
            .ok_or_else(|| crate::Error::Invalid("pty writer already taken".into()))?;
        Ok(Box::new(writer))
    }
}

impl crate::SlavePty for ConPtySlavePty {
    fn spawn_command(
        &self,
        cmd: crate::CommandBuilder,
    ) -> crate::Result<Box<dyn crate::Child + Send + Sync>> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let child = inner.con.spawn_command(cmd)?;
        Ok(Box::new(child))
    }
}
