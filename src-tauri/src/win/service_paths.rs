//! ProgramData layout + integrity validation for the Windows indexer service
//! (design D1).
//!
//! The DB and its derived data live under a machine-global
//! `<ProgramData>\com.everything.app\` because the LocalSystem service has no
//! per-user `%APPDATA%` and indexes the whole `C:\` volume once for every user.
//!
//! Two anti-tampering guards back this up:
//!   * paths are resolved via the Known Folder API (or `%ProgramData%`), never a
//!     hardcoded `C:\ProgramData` string;
//!   * before use, the data directory is validated through an open handle — it
//!     must not be a reparse point and must be owned by SYSTEM or the
//!     Administrators group, so a pre-planted junction can't redirect
//!     LocalSystem writes to an attacker-chosen location.

use std::path::{Path, PathBuf};

/// App folder under ProgramData — matches the existing bundle identifier.
const APP_DIR_NAME: &str = "com.everything.app";
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// `<ProgramData>` via the Known Folder API, falling back to `%ProgramData%`.
/// Returns `None` only if neither is available (should never happen on Windows).
pub(crate) fn program_data_dir() -> Option<PathBuf> {
    if let Some(p) = known_folder_program_data() {
        return Some(p);
    }
    std::env::var_os("ProgramData").map(PathBuf::from)
}

fn known_folder_program_data() -> Option<PathBuf> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_ProgramData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    unsafe {
        let pwstr =
            SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, HANDLE::default()).ok()?;
        let s = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as *const core::ffi::c_void));
        s.filter(|s| !s.is_empty()).map(PathBuf::from)
    }
}

/// `<ProgramData>\com.everything.app`.
pub(crate) fn app_root() -> Option<PathBuf> {
    program_data_dir().map(|p| p.join(APP_DIR_NAME))
}

/// `<app_root>\data` — index.db, WAL/SHM, usn.cursor, markers, logs (Users: RX).
pub(crate) fn data_dir() -> Option<PathBuf> {
    app_root().map(|p| p.join("data"))
}

/// `<app_root>\config` — `.pathignore` / `.pathindexing` sidecars (Users: Modify).
pub(crate) fn config_dir() -> Option<PathBuf> {
    app_root().map(|p| p.join("config"))
}

/// `<app_root>\data\index.db`.
pub(crate) fn db_path() -> Option<PathBuf> {
    data_dir().map(|p| p.join("index.db"))
}

/// Validate the data directory through an open handle before the service uses
/// it (D1): reject reparse points and any owner other than SYSTEM /
/// Administrators. The installer creates and ACLs this directory in an elevated
/// context before first start; this is the service's own defense against a
/// junction planted at the expected path.
pub(crate) fn validate_data_dir(dir: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // FILE_FLAG_OPEN_REPARSE_POINT: open the link itself, not its target, so a
    // junction is detected here rather than silently followed. READ_CONTROL is
    // required so `GetSecurityInfo` can read the owner below — a 0-access handle
    // makes that call fail with ERROR_ACCESS_DENIED (5) even for LocalSystem,
    // which would refuse every legitimately-owned data dir.
    const READ_CONTROL: u32 = 0x0002_0000;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            READ_CONTROL, // owner read (GetSecurityInfo) needs READ_CONTROL
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| format!("cannot open data dir {}: {e}", dir.display()))?;

    let result = validate_handle(handle, dir);
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

fn validate_handle(handle: windows::Win32::Foundation::HANDLE, dir: &Path) -> Result<(), String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    // Reject reparse points (junction / symlink redirection).
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }
        .map_err(|e| format!("GetFileInformationByHandle failed: {e}"))?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(format!(
            "data dir {} is a reparse point — refusing to use it",
            dir.display()
        ));
    }

    if !owner_is_system_or_admins(handle)? {
        return Err(format!(
            "data dir {} is not owned by SYSTEM/Administrators — refusing to use it",
            dir.display()
        ));
    }
    Ok(())
}

fn owner_is_system_or_admins(
    handle: windows::Win32::Foundation::HANDLE,
) -> Result<bool, String> {
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID,
    };

    let mut owner = PSID::default();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    // GetSecurityInfo allocates the security descriptor; free it with LocalFree.
    let status = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            Some(&mut psd),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("GetSecurityInfo failed (win32 error {})", status.0));
    }

    let ok = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
    };

    unsafe {
        let _ = LocalFree(HLOCAL(psd.0));
    }
    Ok(ok)
}
