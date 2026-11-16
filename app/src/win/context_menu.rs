use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use windows::core::{PCWSTR, PSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    IContextMenu, IShellFolder, SHBindToParent, SHParseDisplayName, CMINVOKECOMMANDINFO,
    GCS_VERBW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, InsertMenuItemW, TrackPopupMenu, HMENU, MENUITEMINFOW,
    MFT_SEPARATOR, MFT_STRING, MIIM_ID, MIIM_TYPE, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN,
};

use super::com_guard::{ComGuard, to_wide};

const CREATE_NO_WINDOW: u32 = 0x08000000;
const ID_OPEN: u32 = 1;
const ID_REVEAL: u32 = 2;
const ID_COPY_PATH: u32 = 3;
const ID_CMD_FIRST: u32 = 100;

fn insert_string_item(hmenu: HMENU, pos: u32, id: u32, text: &str) {
    let wide = to_wide(text);
    let mii = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_ID | MIIM_TYPE,
        fType: MFT_STRING,
        wID: id,
        dwTypeData: windows::core::PWSTR(wide.as_ptr() as *mut u16),
        cch: (wide.len() - 1) as u32,
        ..Default::default()
    };
    unsafe {
        let _ = InsertMenuItemW(hmenu, pos, true, &mii);
    }
}

fn insert_separator(hmenu: HMENU, pos: u32) {
    let mii = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_TYPE,
        fType: MFT_SEPARATOR,
        ..Default::default()
    };
    unsafe {
        let _ = InsertMenuItemW(hmenu, pos, true, &mii);
    }
}

/// Shows a native Windows Explorer context menu for the given paths.
/// `hwnd_raw` is the raw HWND as isize (to be Send-safe across threads).
/// A menu pick the app performs itself instead of the shell.
pub enum MenuChoice {
    Handled,
    /// The shell's Delete verb. The shell deletes on its own thread after the
    /// menu returns and the index catches up seconds later; the app's own trash
    /// action hides the rows at once.
    Delete,
}

pub fn show(
    hwnd_raw: isize,
    paths: &[String],
    screen_x: i32,
    screen_y: i32,
) -> Result<MenuChoice, String> {
    if paths.is_empty() {
        return Ok(MenuChoice::Handled);
    }

    let _com = ComGuard::init()?;
    let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
    let hmenu = unsafe { CreatePopupMenu().map_err(|e| format!("CreatePopupMenu: {e}"))? };

    insert_string_item(hmenu, 0, ID_OPEN, "Open");
    insert_string_item(hmenu, 1, ID_REVEAL, "Reveal in Explorer");
    insert_string_item(hmenu, 2, ID_COPY_PATH, "Copy Path");
    insert_separator(hmenu, 3);

    let shell_ctx = build_shell_context_menu(hmenu, paths, ID_CMD_FIRST);

    let selected = unsafe {
        TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
            screen_x,
            screen_y,
            0,
            hwnd,
            None,
        )
    };

    let cmd_id = selected.0 as u32;
    let mut choice = MenuChoice::Handled;
    if cmd_id == ID_OPEN {
        open_paths(paths);
    } else if cmd_id == ID_REVEAL {
        reveal_paths(paths);
    } else if cmd_id == ID_COPY_PATH {
        copy_paths_to_clipboard(paths);
    } else if cmd_id >= ID_CMD_FIRST {
        if let Ok(ref ctx) = shell_ctx {
            let cmd_offset = cmd_id - ID_CMD_FIRST;
            if shell_verb(ctx, cmd_offset).is_some_and(|verb| verb.eq_ignore_ascii_case("delete")) {
                choice = MenuChoice::Delete;
            } else {
                invoke_shell_command(ctx, cmd_offset);
            }
        }
    }

    unsafe { let _ = DestroyMenu(hmenu); }
    Ok(choice)
}

/// RAII guard that frees PIDL memory on drop.
struct PidlGuard(Vec<*mut ITEMIDLIST>);

impl Drop for PidlGuard {
    fn drop(&mut self) {
        for &pidl in &self.0 {
            if !pidl.is_null() {
                unsafe { CoTaskMemFree(Some(pidl as *const std::ffi::c_void)) };
            }
        }
    }
}

struct ShellContextInfo {
    context_menu: IContextMenu,
    // IShellFolder must outlive IContextMenu — shell extensions may hold
    // a back-reference without AddRef.
    _parent_folder: IShellFolder,
    // Absolute PIDLs must outlive IContextMenu — child PIDLs (used by
    // GetUIObjectOf) point into this memory.
    _pidls: PidlGuard,
}

fn build_shell_context_menu(
    hmenu: HMENU,
    paths: &[String],
    id_cmd_first: u32,
) -> Result<ShellContextInfo, String> {
    let first_path = Path::new(&paths[0]);
    let parent_dir = first_path
        .parent()
        .ok_or_else(|| "No parent directory".to_string())?;
    let parent_str = parent_dir.to_string_lossy().to_string();

    // PidlGuard owns the absolute PIDLs; child PIDLs point into them.
    // Both are kept alive in ShellContextInfo until after TrackPopupMenu.
    let mut abs_pidls = PidlGuard(Vec::new());
    let mut child_pidls: Vec<*const ITEMIDLIST> = Vec::new();
    let mut parent_folder: Option<IShellFolder> = None;

    for p in paths {
        let pp = Path::new(p);
        let pp_parent = pp.parent().map(|d| d.to_string_lossy().to_string());
        if pp_parent.as_deref() != Some(&parent_str) {
            continue;
        }

        let wide = to_wide(p);
        let mut abs_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let hr = unsafe {
            SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut abs_pidl, 0, None)
        };
        if hr.is_err() || abs_pidl.is_null() {
            continue;
        }

        let mut child_pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let folder: Result<IShellFolder, _> = unsafe {
            SHBindToParent(abs_pidl as *const ITEMIDLIST, Some(&mut child_pidl))
        };

        match folder {
            Ok(f) => {
                abs_pidls.0.push(abs_pidl);
                if !child_pidl.is_null() {
                    child_pidls.push(child_pidl as *const ITEMIDLIST);
                }
                if parent_folder.is_none() {
                    parent_folder = Some(f);
                }
            }
            Err(e) => {
                unsafe { CoTaskMemFree(Some(abs_pidl as *const std::ffi::c_void)) };
                eprintln!("[context_menu] SHBindToParent failed for {p}: {e}");
            }
        }
    }

    let pf = parent_folder.ok_or_else(|| "No valid shell folder".to_string())?;
    if child_pidls.is_empty() {
        return Err("No valid child items".to_string());
    }

    let context_menu: IContextMenu = unsafe {
        pf.GetUIObjectOf(HWND::default(), &child_pidls, None)
            .map_err(|e| format!("GetUIObjectOf: {e}"))?
    };

    unsafe {
        context_menu
            .QueryContextMenu(hmenu, 4, id_cmd_first, id_cmd_first + 0x7FFF, 0)
            .map_err(|e| format!("QueryContextMenu: {e}"))?;
    }

    Ok(ShellContextInfo {
        context_menu,
        _parent_folder: pf,
        _pidls: abs_pidls,
    })
}

/// Canonical verb ("delete", "properties", …) of a shell menu command.
fn shell_verb(info: &ShellContextInfo, cmd_offset: u32) -> Option<String> {
    let mut buf = [0u16; 64];
    unsafe {
        info.context_menu
            .GetCommandString(
                cmd_offset as usize,
                GCS_VERBW,
                None,
                PSTR(buf.as_mut_ptr().cast()),
                buf.len() as u32,
            )
            .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn invoke_shell_command(info: &ShellContextInfo, cmd_offset: u32) {
    let invoke_info = CMINVOKECOMMANDINFO {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
        lpVerb: windows::core::PCSTR(cmd_offset as usize as *const u8),
        nShow: 1,
        ..Default::default()
    };
    unsafe {
        let _ = info.context_menu.InvokeCommand(&invoke_info);
    }
}

fn open_paths(paths: &[String]) {
    for path in paths {
        let mut cmd = Command::new("cmd");
        cmd.raw_arg(format!("/C start \"\" \"{}\"", path.replace('"', "")));
        cmd.creation_flags(CREATE_NO_WINDOW);
        let _ = cmd.spawn();
    }
}

fn reveal_paths(paths: &[String]) {
    for path in paths {
        let _ = Command::new("explorer")
            .arg(format!("/select,{}", path))
            .spawn();
    }
}

fn copy_paths_to_clipboard(paths: &[String]) {
    let text = paths.join("\r\n");
    // Share the Unicode-safe encoder with Ctrl+C, off the UI thread.
    std::thread::spawn(move || {
        if let Err(error) = crate::copy_text_to_clipboard(&text) {
            eprintln!("[context_menu] Copy Path failed: {error}");
        }
    });
}
