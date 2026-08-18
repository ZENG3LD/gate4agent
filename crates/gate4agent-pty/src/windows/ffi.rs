//! Raw `extern "system"` Win32 FFI declarations for the ConPTY backend.
//!
//! Hand-typed against Microsoft's documented signatures — no `winapi`/
//! `windows-sys` crate. Follows the house pattern already used by
//! `gate4agent::pty::os_process::query_process_rows_native`: snake_case
//! Rust names bound to the real Win32 export via `#[link_name]`, structs
//! declared `#[repr(C)]` field-for-field, and `SAFETY:` comments living at
//! each call site rather than on the declarations themselves.
//!
//! The three ConPTY entry points (`CreatePseudoConsole`/
//! `ResizePseudoConsole`/`ClosePseudoConsole`) are loaded dynamically via
//! `GetProcAddress` in `windows::conpty` instead of being statically linked
//! here — see that module for why (older Windows builds do not export
//! them, and the fork's Gate4Agent patch this crate replaces exists
//! precisely because their calling convention must be `extern "system"`,
//! not the C/cdecl ABI a naive binding would give them on x86).

use std::ffi::c_void;

/// `HANDLE` — an opaque, process-relative Win32 handle.
pub(crate) type Handle = *mut c_void;
/// `HMODULE` — a loaded-module handle, returned by `GetProcAddress`'s module
/// argument lookups.
pub(crate) type HModule = Handle;
/// `BOOL` — Win32's "nonzero is success, zero is failure" convention.
pub(crate) type Bool = i32;
/// `HRESULT`.
pub(crate) type HResult = i32;

pub(crate) const S_OK: HResult = 0;
pub(crate) const STILL_ACTIVE: u32 = 259;
pub(crate) const INFINITE: u32 = 0xFFFF_FFFF;
pub(crate) const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

pub(crate) const STARTF_USESTDHANDLES: u32 = 0x0000_0100;
pub(crate) const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
pub(crate) const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;

pub(crate) const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;

pub(crate) const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
pub(crate) const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;

/// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, from Microsoft's ConPTY sample and
/// `processthreadsapi.h`.
pub(crate) const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

/// `COORD` — a 16-bit console cell coordinate pair.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Coord {
    pub(crate) x: i16,
    pub(crate) y: i16,
}

/// `STARTUPINFOW`, field-for-field per `processthreadsapi.h`.
#[repr(C)]
pub(crate) struct StartupInfoW {
    pub(crate) cb: u32,
    pub(crate) lp_reserved: *mut u16,
    pub(crate) lp_desktop: *mut u16,
    pub(crate) lp_title: *mut u16,
    pub(crate) dw_x: u32,
    pub(crate) dw_y: u32,
    pub(crate) dw_x_size: u32,
    pub(crate) dw_y_size: u32,
    pub(crate) dw_x_count_chars: u32,
    pub(crate) dw_y_count_chars: u32,
    pub(crate) dw_fill_attribute: u32,
    pub(crate) dw_flags: u32,
    pub(crate) w_show_window: u16,
    pub(crate) cb_reserved2: u16,
    pub(crate) lp_reserved2: *mut u8,
    pub(crate) h_std_input: Handle,
    pub(crate) h_std_output: Handle,
    pub(crate) h_std_error: Handle,
}

impl StartupInfoW {
    pub(crate) fn zeroed() -> Self {
        // SAFETY: every field of STARTUPINFOW is a plain integer or
        // pointer; the all-zero bit pattern is a valid value for each of
        // them (a null pointer, in the pointer fields' case).
        unsafe { std::mem::zeroed() }
    }
}

/// `STARTUPINFOEXW` — `STARTUPINFOW` plus the `PROC_THREAD_ATTRIBUTE_LIST`
/// pointer that carries the pseudoconsole attachment.
#[repr(C)]
pub(crate) struct StartupInfoExW {
    pub(crate) startup_info: StartupInfoW,
    pub(crate) lp_attribute_list: *mut c_void,
}

/// `PROCESS_INFORMATION`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct ProcessInformation {
    pub(crate) h_process: Handle,
    pub(crate) h_thread: Handle,
    pub(crate) dw_process_id: u32,
    pub(crate) dw_thread_id: u32,
}

impl ProcessInformation {
    pub(crate) fn zeroed() -> Self {
        // SAFETY: every field is a pointer or integer; all-zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

#[link(name = "kernel32")]
extern "system" {
    #[link_name = "CloseHandle"]
    pub(crate) fn close_handle(object: Handle) -> Bool;

    #[link_name = "DuplicateHandle"]
    pub(crate) fn duplicate_handle(
        source_process: Handle,
        source_handle: Handle,
        target_process: Handle,
        target_handle: *mut Handle,
        desired_access: u32,
        inherit_handle: Bool,
        options: u32,
    ) -> Bool;

    #[link_name = "GetCurrentProcess"]
    pub(crate) fn get_current_process() -> Handle;

    #[link_name = "TerminateProcess"]
    pub(crate) fn terminate_process(process: Handle, exit_code: u32) -> Bool;

    #[link_name = "GetExitCodeProcess"]
    pub(crate) fn get_exit_code_process(process: Handle, exit_code: *mut u32) -> Bool;

    #[link_name = "GetProcessId"]
    pub(crate) fn get_process_id(process: Handle) -> u32;

    #[link_name = "WaitForSingleObject"]
    pub(crate) fn wait_for_single_object(handle: Handle, milliseconds: u32) -> u32;

    #[link_name = "CreatePipe"]
    pub(crate) fn create_pipe(
        read_handle: *mut Handle,
        write_handle: *mut Handle,
        pipe_attributes: *const c_void,
        size: u32,
    ) -> Bool;

    #[link_name = "CreateProcessW"]
    pub(crate) fn create_process_w(
        application_name: *mut u16,
        command_line: *mut u16,
        process_attributes: *mut c_void,
        thread_attributes: *mut c_void,
        inherit_handles: Bool,
        creation_flags: u32,
        environment: *mut c_void,
        current_directory: *const u16,
        startup_info: *mut StartupInfoW,
        process_information: *mut ProcessInformation,
    ) -> Bool;

    #[link_name = "GetModuleHandleW"]
    pub(crate) fn get_module_handle_w(module_name: *const u16) -> Handle;

    #[link_name = "LoadLibraryW"]
    pub(crate) fn load_library_w(file_name: *const u16) -> Handle;

    #[link_name = "GetProcAddress"]
    pub(crate) fn get_proc_address(module: HModule, proc_name: *const i8) -> *mut c_void;

    #[link_name = "InitializeProcThreadAttributeList"]
    pub(crate) fn initialize_proc_thread_attribute_list(
        attribute_list: *mut c_void,
        attribute_count: u32,
        flags: u32,
        size: *mut usize,
    ) -> Bool;

    #[link_name = "UpdateProcThreadAttribute"]
    pub(crate) fn update_proc_thread_attribute(
        attribute_list: *mut c_void,
        flags: u32,
        attribute: usize,
        value: *const c_void,
        size: usize,
        previous_value: *mut c_void,
        return_size: *mut usize,
    ) -> Bool;

    #[link_name = "DeleteProcThreadAttributeList"]
    pub(crate) fn delete_proc_thread_attribute_list(attribute_list: *mut c_void);
}
