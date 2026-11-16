//! Unicode path clipboard shared by keyboard actions and the Explorer menu.

use std::{thread, time::Duration};

use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        Ole::CF_UNICODETEXT,
    },
};

struct Clipboard;

impl Clipboard {
    fn open() -> Result<Self, String> {
        // Another process can briefly hold the clipboard while handling input.
        for attempt in 0..20 {
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(error) if attempt == 19 => return Err(format!("OpenClipboard: {error}")),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        unreachable!()
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

struct GlobalMemory(HGLOBAL);

impl Drop for GlobalMemory {
    fn drop(&mut self) {
        let _ = unsafe { GlobalFree(self.0) };
    }
}

pub fn copy_text(text: &str) -> Result<(), String> {
    // CF_UNICODETEXT is NUL-terminated UTF-16, with no byte-order mark.
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let memory = GlobalMemory(
        unsafe { GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(wide.as_slice())) }
            .map_err(|error| format!("GlobalAlloc: {error}"))?,
    );
    let pointer = unsafe { GlobalLock(memory.0) };
    if pointer.is_null() {
        return Err(format!("GlobalLock: {}", std::io::Error::last_os_error()));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(wide.as_ptr(), pointer.cast::<u16>(), wide.len());
        let _ = GlobalUnlock(memory.0);
    }
    let _clipboard = Clipboard::open()?;
    unsafe {
        EmptyClipboard().map_err(|error| format!("EmptyClipboard: {error}"))?;
        SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(memory.0 .0))
            .map_err(|error| format!("SetClipboardData: {error}"))?;
    }
    // Windows owns the allocation after SetClipboardData succeeds.
    std::mem::forget(memory);
    Ok(())
}
