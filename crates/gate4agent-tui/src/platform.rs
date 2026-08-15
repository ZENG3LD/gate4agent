const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuxiliaryMouseButton {
    Back,
    Forward,
}

pub fn pressed_auxiliary_mouse_button() -> Option<AuxiliaryMouseButton> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_XBUTTON1, VK_XBUTTON2,
        };

        let back = unsafe { GetAsyncKeyState(VK_XBUTTON1 as i32) } < 0;
        let forward = unsafe { GetAsyncKeyState(VK_XBUTTON2 as i32) } < 0;
        auxiliary_mouse_button_from_states(back, forward)
    }

    #[cfg(not(windows))]
    {
        None
    }
}

fn auxiliary_mouse_button_from_states(
    back: bool,
    forward: bool,
) -> Option<AuxiliaryMouseButton> {
    match (back, forward) {
        (true, false) => Some(AuxiliaryMouseButton::Back),
        (false, true) => Some(AuxiliaryMouseButton::Forward),
        _ => None,
    }
}

pub fn normalize_ctrl_char(ch: char) -> char {
    if ch.is_ascii_alphabetic() {
        return ch.to_ascii_lowercase();
    }

    #[cfg(windows)]
    {
        normalize_ctrl_char_windows(ch).unwrap_or(ch)
    }

    #[cfg(not(windows))]
    {
        ch
    }
}

#[cfg(windows)]
fn normalize_ctrl_char_windows(ch: char) -> Option<char> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyboardLayout, VkKeyScanExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    let mut utf16 = [0u16; 2];
    let encoded = ch.encode_utf16(&mut utf16);
    if encoded.len() != 1 {
        return None;
    }

    let layout = unsafe {
        let foreground = GetForegroundWindow();
        let thread_id = if foreground.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, std::ptr::null_mut())
        };
        GetKeyboardLayout(thread_id)
    };
    if layout.is_null() {
        return None;
    }

    let mapping = unsafe { VkKeyScanExW(encoded[0], layout) };
    if mapping == -1 {
        return None;
    }
    let virtual_key = (mapping as u16 & 0xff) as u8;
    if virtual_key.is_ascii_uppercase() {
        Some((virtual_key + b'a' - b'A') as char)
    } else {
        None
    }
}

pub fn read_clipboard_text() -> Result<String, String> {
    #[cfg(windows)]
    {
        read_clipboard_text_windows()
    }

    #[cfg(not(windows))]
    {
        Err("clipboard is unsupported on this platform".to_owned())
    }
}

pub fn write_clipboard_text(text: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        write_clipboard_text_windows(text)
    }

    #[cfg(not(windows))]
    {
        let _ = text;
        Err("clipboard is unsupported on this platform".to_owned())
    }
}

fn checked_clipboard_bytes(utf16_units: usize) -> Result<usize, String> {
    let bytes = utf16_units
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| "clipboard text is too large".to_owned())?;
    if bytes > MAX_CLIPBOARD_BYTES {
        Err(format!(
            "clipboard text exceeds the {} byte limit",
            MAX_CLIPBOARD_BYTES
        ))
    } else {
        Ok(bytes)
    }
}

#[cfg(windows)]
struct OpenClipboardGuard;

#[cfg(windows)]
impl OpenClipboardGuard {
    fn acquire() -> Result<Self, String> {
        use windows_sys::Win32::System::DataExchange::OpenClipboard;

        const ATTEMPTS: usize = 10;
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

        for attempt in 0..ATTEMPTS {
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Self);
            }
            if attempt + 1 < ATTEMPTS {
                std::thread::sleep(RETRY_DELAY);
            }
        }
        Err(format!(
            "failed to open clipboard: {}",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(windows)]
impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::DataExchange::CloseClipboard();
        }
    }
}

#[cfg(windows)]
struct LockedGlobalMemory {
    handle: windows_sys::Win32::Foundation::HGLOBAL,
    pointer: *mut core::ffi::c_void,
}

#[cfg(windows)]
impl LockedGlobalMemory {
    fn lock(handle: windows_sys::Win32::Foundation::HGLOBAL) -> Result<Self, String> {
        let pointer = unsafe { windows_sys::Win32::System::Memory::GlobalLock(handle) };
        if pointer.is_null() {
            Err(format!(
                "failed to lock clipboard memory: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(Self { handle, pointer })
        }
    }
}

#[cfg(windows)]
impl Drop for LockedGlobalMemory {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Memory::GlobalUnlock(self.handle);
        }
    }
}

#[cfg(windows)]
struct OwnedGlobalMemory {
    handle: windows_sys::Win32::Foundation::HGLOBAL,
}

#[cfg(windows)]
impl OwnedGlobalMemory {
    fn allocate(bytes: usize) -> Result<Self, String> {
        use windows_sys::Win32::System::Memory::{GlobalAlloc, GMEM_MOVEABLE};

        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if handle.is_null() {
            Err(format!(
                "failed to allocate clipboard memory: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(Self { handle })
        }
    }

    fn transfer(mut self) {
        self.handle = std::ptr::null_mut();
    }
}

#[cfg(windows)]
impl Drop for OwnedGlobalMemory {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::GlobalFree(self.handle);
            }
        }
    }
}

#[cfg(windows)]
fn read_clipboard_text_windows() -> Result<String, String> {
    use windows_sys::Win32::System::DataExchange::{
        GetClipboardData, IsClipboardFormatAvailable,
    };
    use windows_sys::Win32::System::Memory::GlobalSize;

    const CF_UNICODETEXT: u32 = 13;

    let _clipboard = OpenClipboardGuard::acquire()?;
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
        return Err("clipboard does not contain Unicode text".to_owned());
    }

    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if handle.is_null() {
        return Err(format!(
            "failed to read clipboard data: {}",
            std::io::Error::last_os_error()
        ));
    }
    let bytes = unsafe { GlobalSize(handle) };
    if bytes == 0 {
        return Err(format!(
            "failed to measure clipboard data: {}",
            std::io::Error::last_os_error()
        ));
    }
    if bytes > MAX_CLIPBOARD_BYTES {
        return Err(format!(
            "clipboard text exceeds the {} byte limit",
            MAX_CLIPBOARD_BYTES
        ));
    }
    if bytes % std::mem::size_of::<u16>() != 0 {
        return Err("clipboard contains malformed UTF-16 storage".to_owned());
    }

    let locked = LockedGlobalMemory::lock(handle)?;
    let units = unsafe {
        std::slice::from_raw_parts(locked.pointer.cast::<u16>(), bytes / std::mem::size_of::<u16>())
    };
    let terminator = units.iter().position(|unit| *unit == 0).unwrap_or(units.len());
    String::from_utf16(&units[..terminator])
        .map_err(|error| format!("clipboard contains invalid UTF-16: {error}"))
}

#[cfg(windows)]
fn write_clipboard_text_windows(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};

    const CF_UNICODETEXT: u32 = 13;

    if text.contains('\0') {
        return Err("clipboard text contains an embedded NUL".to_owned());
    }
    let mut utf16: Vec<u16> = text.encode_utf16().collect();
    let bytes = checked_clipboard_bytes(
        utf16
            .len()
            .checked_add(1)
            .ok_or_else(|| "clipboard text is too large".to_owned())?,
    )?;
    utf16.push(0);

    let memory = OwnedGlobalMemory::allocate(bytes)?;
    {
        let locked = LockedGlobalMemory::lock(memory.handle)?;
        unsafe {
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), locked.pointer.cast::<u16>(), utf16.len());
        }
    }

    let _clipboard = OpenClipboardGuard::acquire()?;
    if unsafe { EmptyClipboard() } == 0 {
        return Err(format!(
            "failed to clear clipboard: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { SetClipboardData(CF_UNICODETEXT, memory.handle) }.is_null() {
        return Err(format!(
            "failed to write clipboard data: {}",
            std::io::Error::last_os_error()
        ));
    }
    memory.transfer();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_ctrl_chars_are_normalized_without_a_layout_lookup() {
        assert_eq!(normalize_ctrl_char('A'), 'a');
        assert_eq!(normalize_ctrl_char('z'), 'z');
        assert_eq!(normalize_ctrl_char('7'), '7');
        assert_eq!(normalize_ctrl_char('?'), '?');
    }

    #[test]
    fn clipboard_size_is_bounded() {
        assert_eq!(checked_clipboard_bytes(0), Ok(0));
        assert_eq!(
            checked_clipboard_bytes(MAX_CLIPBOARD_BYTES / 2),
            Ok(MAX_CLIPBOARD_BYTES)
        );
        assert!(checked_clipboard_bytes(MAX_CLIPBOARD_BYTES / 2 + 1).is_err());
        assert!(checked_clipboard_bytes(usize::MAX).is_err());
    }

    #[test]
    fn auxiliary_mouse_buttons_are_distinguished_without_ambiguous_chords() {
        assert_eq!(
            auxiliary_mouse_button_from_states(true, false),
            Some(AuxiliaryMouseButton::Back)
        );
        assert_eq!(
            auxiliary_mouse_button_from_states(false, true),
            Some(AuxiliaryMouseButton::Forward)
        );
        assert_eq!(auxiliary_mouse_button_from_states(false, false), None);
        assert_eq!(auxiliary_mouse_button_from_states(true, true), None);
    }
}
