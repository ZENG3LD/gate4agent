//! Windows child-process lifecycle adapter: `TerminateProcess`/
//! `GetExitCodeProcess`/`WaitForSingleObject` with the Win32 `BOOL`
//! convention (nonzero is success, zero is failure) applied everywhere, and
//! the process handle kept alive across `WaitForSingleObject` via an
//! independent `DuplicateHandle` clone rather than the locked original —
//! the two Gate4Agent lifecycle-fix patches this crate ports from the
//! vendored fork's `win/mod.rs`.

use std::io;
use std::sync::Mutex;

use super::ffi;
use super::handle::OwnedHandle;

#[derive(Debug)]
pub(crate) struct WinChild {
    process: Mutex<OwnedHandle>,
}

impl WinChild {
    pub(crate) fn new(process: OwnedHandle) -> Self {
        Self {
            process: Mutex::new(process),
        }
    }

    fn is_complete(&self) -> io::Result<Option<crate::ExitStatus>> {
        let process = {
            let guard = self
                .process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.try_clone()?
        };
        let mut status: u32 = 0;
        // SAFETY: `process` is this call's own independently owned
        // duplicate, valid for the duration of this call.
        let ok = unsafe { ffi::get_exit_code_process(process.as_raw(), &mut status) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if status == ffi::STILL_ACTIVE {
            Ok(None)
        } else {
            Ok(Some(crate::ExitStatus::with_exit_code(status)))
        }
    }

    fn do_kill(&self) -> io::Result<()> {
        let process = {
            let guard = self
                .process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.try_clone()?
        };
        // SAFETY: as above.
        let ok = unsafe { ffi::terminate_process(process.as_raw(), 1) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl crate::ChildKiller for WinChild {
    fn kill(&mut self) -> io::Result<()> {
        self.do_kill()
    }

    fn clone_killer(&self) -> Box<dyn crate::ChildKiller + Send + Sync> {
        let guard = self
            .process
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Box::new(WinChildKiller::from_clone_result(guard.try_clone()))
    }
}

impl crate::Child for WinChild {
    fn try_wait(&mut self) -> io::Result<Option<crate::ExitStatus>> {
        self.is_complete()
    }

    fn wait(&mut self) -> io::Result<crate::ExitStatus> {
        if let Ok(Some(status)) = self.is_complete() {
            return Ok(status);
        }
        let process = {
            let guard = self
                .process
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.try_clone()?
        };
        // SAFETY: `process` is this call's own independently owned
        // duplicate, kept alive across this entire blocking wait regardless
        // of what a concurrent `clone_killer()`/`Drop` does to the original
        // — the Gate4Agent lifecycle fix this crate ports: the waiting
        // thread never observes a handle invalidated out from under it.
        unsafe {
            ffi::wait_for_single_object(process.as_raw(), ffi::INFINITE);
        }
        let mut status: u32 = 0;
        // SAFETY: as above.
        let ok = unsafe { ffi::get_exit_code_process(process.as_raw(), &mut status) };
        if ok != 0 {
            Ok(crate::ExitStatus::with_exit_code(status))
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn process_id(&self) -> Option<u32> {
        let guard = self
            .process
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // SAFETY: guard's handle is valid for the duration of this call.
        let id = unsafe { ffi::get_process_id(guard.as_raw()) };
        if id == 0 {
            None
        } else {
            Some(id)
        }
    }
}

/// A termination-only handle split out from a `WinChild`, independent of
/// any PTY I/O contention. `DuplicateHandle` is fallible; rather than panic
/// when a clone fails (as the vendored fork's `.unwrap()` did), the failure
/// is captured and surfaces the next time `kill()` is actually called —
/// `clone_killer()` itself stays infallible, matching the trait signature
/// every consumer in this workspace already builds against.
#[derive(Debug)]
enum WinChildKiller {
    Handle(OwnedHandle),
    Unavailable(Option<i32>),
}

impl WinChildKiller {
    fn from_clone_result(result: io::Result<OwnedHandle>) -> Self {
        match result {
            Ok(handle) => WinChildKiller::Handle(handle),
            Err(error) => WinChildKiller::Unavailable(error.raw_os_error()),
        }
    }
}

impl crate::ChildKiller for WinChildKiller {
    fn kill(&mut self) -> io::Result<()> {
        match self {
            WinChildKiller::Handle(process) => {
                // SAFETY: process is a valid, exclusively owned handle.
                let ok = unsafe { ffi::terminate_process(process.as_raw(), 1) };
                if ok == 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            }
            WinChildKiller::Unavailable(code) => Err(match code {
                Some(code) => io::Error::from_raw_os_error(*code),
                None => io::Error::other(
                    "failed to duplicate the child process handle for an independent killer",
                ),
            }),
        }
    }

    fn clone_killer(&self) -> Box<dyn crate::ChildKiller + Send + Sync> {
        match self {
            WinChildKiller::Handle(process) => {
                Box::new(WinChildKiller::from_clone_result(process.try_clone()))
            }
            WinChildKiller::Unavailable(code) => Box::new(WinChildKiller::Unavailable(*code)),
        }
    }
}
