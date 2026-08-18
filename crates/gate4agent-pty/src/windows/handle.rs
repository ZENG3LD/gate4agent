//! A minimal RAII owned Win32 `HANDLE`, replacing the vendored fork's
//! dependency on the `filedescriptor` crate's `OwnedHandle`.

use std::io;

use super::ffi::{self, Handle};

/// An owned Win32 `HANDLE`, closed on drop. Used for process/thread handles
/// (`hProcess`/`hThread`), which — unlike the ConPTY pipe endpoints — are
/// not file-like and are driven through `TerminateProcess`/
/// `GetExitCodeProcess`/`WaitForSingleObject`/`GetProcessId` rather than
/// `Read`/`Write`.
#[derive(Debug)]
pub(crate) struct OwnedHandle(Handle);

// SAFETY: a Win32 HANDLE is a process-relative integer-like identifier, not
// a pointer into thread-local state; the Win32 APIs used to operate on it
// here are documented as safe to call from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    /// Takes ownership of a handle returned by a Win32 API. The caller must
    /// not use `raw` again after this call.
    pub(crate) unsafe fn from_raw(raw: Handle) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(&self) -> Handle {
        self.0
    }

    /// Duplicates this handle within the current process
    /// (`DUPLICATE_SAME_ACCESS`), so the duplicate stays valid independently
    /// of this handle's lifetime. Used to keep a process handle alive across
    /// a `WaitForSingleObject` call running on a separate thread — the
    /// Gate4Agent lifecycle fix this crate ports from the vendored fork
    /// (see `windows::child`).
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        let mut duplicated: Handle = std::ptr::null_mut();
        // SAFETY: self.0 is a valid, currently-owned handle in the current
        // process; get_current_process() is a pseudo-handle valid for both
        // the source and target process arguments; `duplicated` is a valid
        // out-pointer.
        let ok = unsafe {
            ffi::duplicate_handle(
                ffi::get_current_process(),
                self.0,
                ffi::get_current_process(),
                &mut duplicated,
                0,
                0,
                ffi::DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(duplicated))
    }

    /// Releases ownership of the underlying handle without closing it. The
    /// caller becomes responsible for it — used when handing a pipe end off
    /// to `std::fs::File::from_raw_handle`, which takes over the close.
    pub(crate) fn into_raw(self) -> Handle {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0.is_null() || self.0 == ffi::INVALID_HANDLE_VALUE {
            return;
        }
        // SAFETY: self.0 is a handle exclusively owned by this struct; no
        // other owner will use it after this point.
        unsafe {
            ffi::close_handle(self.0);
        }
    }
}
