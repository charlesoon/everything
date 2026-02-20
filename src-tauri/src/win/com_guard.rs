use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};

pub struct ComGuard {
    needs_uninit: bool,
}

impl ComGuard {
    pub fn init() -> Result<Self, String> {
        unsafe {
            match CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
                Ok(()) => Ok(ComGuard { needs_uninit: true }),
                Err(e) => {
                    // RPC_E_CHANGED_MODE: COM already initialised with a different model.
                    // This is expected when called on the main thread (WebView2 may have
                    // already called CoInitialize).  Safe to proceed without uninit.
                    if e.code().0 as u32 == 0x80010106 {
                        Ok(ComGuard { needs_uninit: false })
                    } else {
                        Err(format!("CoInitializeEx failed: {e}"))
                    }
                }
            }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.needs_uninit {
            unsafe { CoUninitialize() }
        }
    }
}

pub fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
