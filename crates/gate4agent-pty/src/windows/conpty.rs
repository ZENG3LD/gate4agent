//! ConPTY: dynamic entry-point loading and the pseudoconsole wrapper
//! (create/resize/close/spawn).
//!
//! Ports the vendored fork's Gate4Agent patch faithfully:
//! - `CreatePseudoConsole`/`ResizePseudoConsole`/`ClosePseudoConsole` are
//!   typed `unsafe extern "system" fn` (`ffi::Coord`/`ffi::Handle`
//!   parameters), not a naive C/cdecl binding — Microsoft declares them
//!   `WINAPI`, which is `__stdcall` on x86 and only coincides with cdecl on
//!   x86_64. A cdecl-typed call corrupts the stack on 32-bit Windows only,
//!   the kind of bug a 64-bit-only dev loop never observes.
//! - Loading them is fallible, never a panic: `load_conpty` returns
//!   `Result`, so a Windows build predating the October 2018 ConPTY release
//!   surfaces `Error::ConPtyUnavailable` from `PsuedoCon::new` instead of
//!   panicking during process/library initialization.

use std::ffi::c_void;
use std::io;

use super::ffi::{self, Coord, Handle};
use super::handle::OwnedHandle;
use super::procthreadattr::ProcThreadAttributeList;

type CreatePseudoConsoleFn =
    unsafe extern "system" fn(size: Coord, h_input: Handle, h_output: Handle, flags: u32, hpc: *mut Handle) -> i32;
type ResizePseudoConsoleFn = unsafe extern "system" fn(hpc: Handle, size: Coord) -> i32;
type ClosePseudoConsoleFn = unsafe extern "system" fn(hpc: Handle);

struct ConPtyFuncs {
    create: CreatePseudoConsoleFn,
    resize: ResizePseudoConsoleFn,
    close: ClosePseudoConsoleFn,
}

fn resolve_symbol(module: Handle, name: &[u8]) -> Option<*mut c_void> {
    // SAFETY: `module` is either null (GetProcAddress simply fails on a
    // null module) or a handle returned by GetModuleHandleW/LoadLibraryW
    // below; `name` is a caller-supplied nul-terminated ASCII byte string.
    let addr = unsafe { ffi::get_proc_address(module, name.as_ptr() as *const i8) };
    if addr.is_null() {
        None
    } else {
        Some(addr)
    }
}

fn load_from(module: Handle) -> Option<ConPtyFuncs> {
    if module.is_null() {
        return None;
    }
    let create = resolve_symbol(module, b"CreatePseudoConsole\0")?;
    let resize = resolve_symbol(module, b"ResizePseudoConsole\0")?;
    let close = resolve_symbol(module, b"ClosePseudoConsole\0")?;
    // SAFETY: each symbol was resolved by its exact documented name against
    // a loaded module and is transmuted to the `extern "system"` signature
    // Microsoft publishes for it — see the module doc comment for why
    // "system" (not a bare `extern fn`) is the load-bearing detail here.
    unsafe {
        Some(ConPtyFuncs {
            create: std::mem::transmute::<*mut c_void, CreatePseudoConsoleFn>(create),
            resize: std::mem::transmute::<*mut c_void, ResizePseudoConsoleFn>(resize),
            close: std::mem::transmute::<*mut c_void, ClosePseudoConsoleFn>(close),
        })
    }
}

/// Resolves the three ConPTY entry points: `kernel32.dll` (always mapped)
/// first, then an optional sideloaded `conpty.dll` deployed next to the
/// executable. Re-resolved on every `PsuedoCon::new` call rather than
/// cached in a process-lifetime static — `openpty()` is a low-frequency,
/// once-per-session operation, so the cost of a fresh `GetProcAddress` call
/// each time is immaterial, and it avoids any global mutable/lazy-init state.
fn load_conpty() -> crate::Result<ConPtyFuncs> {
    let wide_kernel32: Vec<u16> = "kernel32.dll\0".encode_utf16().collect();
    // SAFETY: wide_kernel32 is a valid nul-terminated UTF-16 string;
    // kernel32.dll is mapped into every Win32 process, so this cannot fail.
    let kernel32 = unsafe { ffi::get_module_handle_w(wide_kernel32.as_ptr()) };
    if let Some(funcs) = load_from(kernel32) {
        return Ok(funcs);
    }

    let wide_sideload: Vec<u16> = "conpty.dll\0".encode_utf16().collect();
    // SAFETY: wide_sideload is a valid nul-terminated UTF-16 string;
    // LoadLibraryW's ordinary "module not found" failure is a null return,
    // handled by `load_from` above.
    let sideloaded = unsafe { ffi::load_library_w(wide_sideload.as_ptr()) };
    if let Some(funcs) = load_from(sideloaded) {
        return Ok(funcs);
    }

    Err(crate::Error::ConPtyUnavailable(
        "CreatePseudoConsole/ResizePseudoConsole/ClosePseudoConsole are not exported by \
         kernel32.dll and no sideloaded conpty.dll was found; Windows 10 October 2018 \
         (build 1809) or newer is required"
            .to_owned(),
    ))
}

pub(crate) struct PsuedoCon {
    con: Handle,
    funcs: ConPtyFuncs,
}

// SAFETY: `con` is an opaque Win32 handle (not thread-local state) and
// every ConPTY entry point is documented as callable from any thread.
unsafe impl Send for PsuedoCon {}
unsafe impl Sync for PsuedoCon {}

impl Drop for PsuedoCon {
    fn drop(&mut self) {
        // SAFETY: self.con was created by (self.funcs.create) in `new` and
        // has not been closed yet — this is the only `Drop` impl for it.
        unsafe { (self.funcs.close)(self.con) };
    }
}

impl PsuedoCon {
    pub(crate) fn new(size: Coord, input: Handle, output: Handle) -> crate::Result<Self> {
        let funcs = load_conpty()?;
        let mut con: Handle = ffi::INVALID_HANDLE_VALUE;
        // SAFETY: input/output are valid, currently-owned pipe handles for
        // the duration of this call; con is a valid out-pointer.
        let result = unsafe {
            (funcs.create)(
                size,
                input,
                output,
                ffi::PSEUDOCONSOLE_RESIZE_QUIRK | ffi::PSEUDOCONSOLE_WIN32_INPUT_MODE,
                &mut con,
            )
        };
        if result != ffi::S_OK {
            return Err(crate::Error::ConPtyCall {
                operation: "CreatePseudoConsole",
                hresult: result,
            });
        }
        Ok(Self { con, funcs })
    }

    pub(crate) fn resize(&self, size: Coord) -> crate::Result<()> {
        // SAFETY: self.con is a live pseudoconsole handle owned by `self`.
        let result = unsafe { (self.funcs.resize)(self.con, size) };
        if result != ffi::S_OK {
            return Err(crate::Error::ConPtyCall {
                operation: "ResizePseudoConsole",
                hresult: result,
            });
        }
        Ok(())
    }

    pub(crate) fn spawn_command(&self, cmd: crate::CommandBuilder) -> crate::Result<super::child::WinChild> {
        let mut startup_info = ffi::StartupInfoExW {
            startup_info: ffi::StartupInfoW::zeroed(),
            lp_attribute_list: std::ptr::null_mut(),
        };
        startup_info.startup_info.cb = std::mem::size_of::<ffi::StartupInfoExW>() as u32;
        // Explicitly invalid stdio handles: otherwise the spawned process
        // can inherit whatever stdio handles this (possibly redirected)
        // process happens to have, instead of talking only to the
        // pseudoconsole. Ported verbatim from the vendored fork, which
        // credits this to daemonized wezterm-mux-server originally leaking
        // its own redirected log-file stdio into spawned children.
        startup_info.startup_info.dw_flags = ffi::STARTF_USESTDHANDLES;
        startup_info.startup_info.h_std_input = ffi::INVALID_HANDLE_VALUE;
        startup_info.startup_info.h_std_output = ffi::INVALID_HANDLE_VALUE;
        startup_info.startup_info.h_std_error = ffi::INVALID_HANDLE_VALUE;

        let mut attributes = ProcThreadAttributeList::with_capacity(1)?;
        attributes.set_pty(self.con)?;
        startup_info.lp_attribute_list = attributes.as_mut_ptr();

        let mut process_information = ffi::ProcessInformation::zeroed();

        let (mut exe, mut cmdline) = cmd.cmdline()?;
        let cwd = cmd.current_directory();
        let mut environment_block = cmd.environment_block();

        // SAFETY: exe/cmdline/environment_block are nul-terminated (double
        // for the environment block) UTF-16 buffers owned by this call for
        // its duration; cwd, if present, is likewise nul-terminated;
        // startup_info points at a StartupInfoExW whose lp_attribute_list
        // was just filled by `attributes` (kept alive below this call by
        // ordinary scope, matching the vendored fork's own lifetime
        // structure); process_information is a valid out-pointer.
        let ok = unsafe {
            ffi::create_process_w(
                exe.as_mut_ptr(),
                cmdline.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                ffi::EXTENDED_STARTUPINFO_PRESENT | ffi::CREATE_UNICODE_ENVIRONMENT,
                environment_block.as_mut_ptr() as *mut c_void,
                cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                &mut startup_info.startup_info,
                &mut process_information,
            )
        };
        if ok == 0 {
            return Err(crate::Error::Spawn(io::Error::last_os_error()));
        }

        // Close the thread handle so it is not leaked; we only need the
        // process handle for the lifecycle adapter.
        // SAFETY: h_thread/h_process were just populated by a successful
        // CreateProcessW and are owned, unique handles this call is now
        // responsible for.
        drop(unsafe { OwnedHandle::from_raw(process_information.h_thread) });
        let process = unsafe { OwnedHandle::from_raw(process_information.h_process) };

        Ok(super::child::WinChild::new(process))
    }
}
