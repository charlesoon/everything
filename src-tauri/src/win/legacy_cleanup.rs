//! One-time cleanup of a previous per-user install's leftovers, run on the GUI's
//! first read-only start (§5 ② / §3.3): the stale HKCU "Uninstall" entry and the
//! per-user Start Menu / Desktop shortcuts that still point at the removed
//! per-user binary. Registry + file DELETION ONLY — the old (user-writable)
//! uninstaller is never executed (D5 privilege-escalation invariant). The
//! routine is keyed on the stale HKCU key, so it short-circuits once it has run.

use std::path::{Path, PathBuf};

use super::com_guard::{to_wide, ComGuard};

/// A perMachine install writes its uninstall entry under HKLM, so an HKCU entry
/// can only be a stale per-user leftover. PRODUCTNAME = "Everything".
const OLD_UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Everything";

/// Remove the previous per-user install's leftover HKCU uninstall entry and
/// broken shortcuts. Idempotent and cheap on the common path (one registry
/// probe) once the key is gone.
pub fn clean_legacy_peruser_artifacts() {
    if !old_uninstall_key_exists() {
        return; // Already cleaned, or there was no per-user install.
    }
    // The old per-user install directory the leftover shortcuts point at.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        delete_shortcuts_targeting(&PathBuf::from(local).join("Everything"));
    }
    delete_old_uninstall_key();
    eprintln!("[win/legacy] removed stale per-user uninstall entry + shortcuts");
}

fn old_uninstall_key_exists() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    let key = to_wide(OLD_UNINSTALL_KEY);
    let mut hk = HKEY::default();
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), 0, KEY_READ, &mut hk) };
    if opened.is_ok() {
        unsafe {
            let _ = RegCloseKey(hk);
        }
        return true;
    }
    false
}

fn delete_old_uninstall_key() {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{RegDeleteKeyW, HKEY_CURRENT_USER};
    let key = to_wide(OLD_UNINSTALL_KEY);
    unsafe {
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()));
    }
}

/// Delete the per-user Start Menu + Desktop `Everything.lnk` shortcuts, but only
/// when they still target `old_install` — so the machine-wide install's own
/// shortcut (a different target) is never removed.
fn delete_shortcuts_targeting(old_install: &Path) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        candidates
            .push(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Everything.lnk"));
    }
    if let Some(userprofile) = std::env::var_os("USERPROFILE") {
        candidates.push(PathBuf::from(userprofile).join(r"Desktop\Everything.lnk"));
    }

    let _com = match ComGuard::init() {
        Ok(g) => g,
        Err(_) => return,
    };
    for lnk in candidates {
        if !lnk.exists() {
            continue;
        }
        if let Some(target) = shortcut_target(&lnk) {
            if target.starts_with(old_install) {
                let _ = std::fs::remove_file(&lnk);
            }
        }
    }
}

/// Resolve a `.lnk`'s target path via `IShellLink` (mirrors the installer's
/// `IsShortcutTarget`). `None` if it can't be read. COM must already be
/// initialized on this thread (see the caller's `ComGuard`).
fn shortcut_target(lnk: &Path) -> Option<PathBuf> {
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::System::Com::{
        CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLGP_RAWPATH};

    let lnk_w = to_wide(&lnk.to_string_lossy());
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        persist.Load(PCWSTR(lnk_w.as_ptr()), STGM_READ).ok()?;
        let mut buf = [0u16; 260]; // MAX_PATH
        link.GetPath(&mut buf, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32)
            .ok()?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        if len == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
    }
}
