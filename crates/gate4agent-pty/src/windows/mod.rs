//! Windows ConPTY pty backend: raw `extern "system"` Win32 FFI, no
//! `winapi`/`windows-sys` crate. See `conpty` for the three Gate4Agent
//! patches this module ports from the vendored fork (ABI typing, fallible
//! loading, and process-handle lifetime across `WaitForSingleObject`).

mod child;
mod conpty;
mod ffi;
mod handle;
mod procthreadattr;
mod pty_system;
#[cfg(test)]
mod tests;

pub(crate) use pty_system::ConPtySystem;
