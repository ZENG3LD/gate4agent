//! `PROC_THREAD_ATTRIBUTE_LIST` plumbing: the two-phase
//! `InitializeProcThreadAttributeList` size-probe-then-fill pattern, and the
//! `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` attribute that attaches a
//! pseudoconsole to a `CreateProcessW` call.

use std::ffi::c_void;
use std::io;

use super::ffi;

pub(crate) struct ProcThreadAttributeList {
    data: Vec<u8>,
}

impl ProcThreadAttributeList {
    pub(crate) fn with_capacity(attribute_count: u32) -> io::Result<Self> {
        let mut bytes_required: usize = 0;
        // SAFETY: a null attribute-list pointer with a valid `size`
        // out-pointer is the documented size-probe call; it always "fails"
        // (returns 0) while still writing the required size.
        unsafe {
            ffi::initialize_proc_thread_attribute_list(
                std::ptr::null_mut(),
                attribute_count,
                0,
                &mut bytes_required,
            );
        }

        let mut data = vec![0u8; bytes_required];
        // SAFETY: data has exactly `bytes_required` bytes, matching the
        // probed size; the Win32 API fills it as an opaque, Windows-owned
        // structure.
        let ok = unsafe {
            ffi::initialize_proc_thread_attribute_list(
                data.as_mut_ptr() as *mut c_void,
                attribute_count,
                0,
                &mut bytes_required,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { data })
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut c_void {
        self.data.as_mut_ptr() as *mut c_void
    }

    /// Attaches `con` (a `PsuedoCon`'s `HPCON`) to this attribute list under
    /// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`.
    ///
    /// Per Microsoft's ConPTY sample, this attribute's value is the `HPCON`
    /// handle itself reinterpreted as the `lpValue` pointer (not a pointer
    /// to a variable holding the handle) — `cbSize` is `sizeof(HPCON)`.
    pub(crate) fn set_pty(&mut self, con: ffi::Handle) -> io::Result<()> {
        // SAFETY: self.data was sized and initialized above; `con` is the
        // caller's live pseudoconsole handle, passed by value (as the ConPTY
        // attribute contract requires) rather than by reference, so nothing
        // here depends on any stack frame outliving this call.
        let ok = unsafe {
            ffi::update_proc_thread_attribute(
                self.as_mut_ptr(),
                0,
                ffi::PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                con as *const c_void,
                std::mem::size_of::<ffi::Handle>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        // SAFETY: self.data was initialized by
        // InitializeProcThreadAttributeList in `with_capacity` and has not
        // been deleted yet (this is the only `Drop` impl for this type).
        unsafe {
            ffi::delete_proc_thread_attribute_list(self.as_mut_ptr());
        }
    }
}
