//! Security descriptors for the indexer service (design D2, D3).
//!
//!  * [`pipe_security`] builds the DACL the control pipe is created with:
//!    SYSTEM / Administrators full control, INTERACTIVE logon read+write, and
//!    nobody else. Combined with `PIPE_REJECT_REMOTE_CLIENTS` this limits the
//!    pipe to local interactive-session callers.
//!  * [`grant_interactive_service_access`] adds — idempotently — an ACE to the
//!    *service object's* existing DACL so a non-elevated GUI in an interactive
//!    session can query and start (but not stop or delete) the service without
//!    a UAC prompt. It merges into the current DACL via `SetEntriesInAclW`
//!    rather than replacing it wholesale (which `sc sdset` would do), and never
//!    grants `AU` (which would include remote authenticated users).

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::{
    SECURITY_ATTRIBUTES, PSECURITY_DESCRIPTOR,
};

use super::com_guard::to_wide;

/// Owns a security descriptor allocated by the SDDL converter plus the
/// `SECURITY_ATTRIBUTES` that points at it. Keep it alive for the duration of
/// the `CreateNamedPipeW` calls that use `attrs`.
pub(crate) struct PipeSecurity {
    psd: PSECURITY_DESCRIPTOR,
    attrs: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    pub(crate) fn attributes(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attrs
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            let _ = LocalFree(HLOCAL(self.psd.0));
        }
    }
}

/// Build the pipe DACL: `SY`/`BA` full, `IU` (interactive) generic read+write.
pub(crate) fn pipe_security() -> Option<PipeSecurity> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };

    let sddl = to_wide("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)");
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
        .ok()?;
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd.0,
        bInheritHandle: false.into(),
    };
    Some(PipeSecurity { psd, attrs })
}

/// Idempotently grant `INTERACTIVE` `SERVICE_START | SERVICE_QUERY_STATUS` on
/// the service object. Best-effort: on failure the GUI simply needs a UAC
/// prompt to start a stopped service, so callers log and continue.
pub(crate) fn grant_interactive_service_access(service_name: &str) -> Result<(), String> {
    use windows::Win32::Security::Authorization::{
        GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
        GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_SERVICE, TRUSTEE_IS_SID,
        TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        CreateWellKnownSid, WinInteractiveSid, ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, PSID,
    };

    // Generic service access rights (SDK winsvc.h) — defined locally since this
    // windows-rs feature set doesn't re-export them by name.
    const SERVICE_START: u32 = 0x0010;
    const SERVICE_QUERY_STATUS: u32 = 0x0004;

    // INTERACTIVE well-known SID (S-1-5-4).
    let mut sid_buf = vec![0u8; 68];
    let psid = PSID(sid_buf.as_mut_ptr() as *mut core::ffi::c_void);
    let mut cb = sid_buf.len() as u32;
    unsafe {
        CreateWellKnownSid(WinInteractiveSid, None, psid, &mut cb)
            .map_err(|e| format!("CreateWellKnownSid failed: {e}"))?;
    }

    let name = to_wide(service_name);
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_SERVICE,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut psd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!("GetNamedSecurityInfoW failed (win32 {})", status.0));
    }

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: SERVICE_START | SERVICE_QUERY_STATUS,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: ACE_FLAGS(0),
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
            ptstrName: PWSTR(psid.0 as *mut u16),
        },
    };

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    let merged = unsafe { SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl) };
    if merged != ERROR_SUCCESS {
        unsafe {
            let _ = LocalFree(HLOCAL(psd.0));
        }
        return Err(format!("SetEntriesInAclW failed (win32 {})", merged.0));
    }

    let applied = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_SERVICE,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_dacl),
            None,
        )
    };
    unsafe {
        let _ = LocalFree(HLOCAL(psd.0));
        let _ = LocalFree(HLOCAL(new_dacl as *mut core::ffi::c_void));
    }
    if applied != ERROR_SUCCESS {
        return Err(format!("SetNamedSecurityInfoW failed (win32 {})", applied.0));
    }
    Ok(())
}

/// The pipe client's logon session and whether its token carries INTERACTIVE.
pub(crate) struct ClientInfo {
    pub(crate) session_id: u32,
    pub(crate) interactive: bool,
}

/// Impersonate the pipe client to inspect its access token (D3): the logon
/// session it belongs to and whether it carries the INTERACTIVE group
/// (S-1-5-4). This is defense-in-depth beyond the pipe DACL, and the session id
/// feeds the per-session connection cap and the audit log. Always reverts before
/// returning; yields `None` if impersonation or inspection fails.
pub(crate) fn inspect_client(pipe: HANDLE) -> Option<ClientInfo> {
    use windows::Win32::Foundation::{CloseHandle, BOOL};
    use windows::Win32::Security::{
        CheckTokenMembership, CreateWellKnownSid, GetTokenInformation, RevertToSelf, TokenSessionId,
        WinInteractiveSid, PSID, TOKEN_QUERY,
    };
    use windows::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    unsafe {
        ImpersonateNamedPipeClient(pipe).ok()?;

        // From here we MUST RevertToSelf before returning.
        let mut token = HANDLE::default();
        // openasself=true: open against our (SYSTEM) context, not the client's,
        // so the impersonation can't deny us the token handle.
        if OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token).is_err() {
            let _ = RevertToSelf();
            return None;
        }

        let mut session_id: u32 = 0;
        let mut ret_len: u32 = 0;
        let sess_ok = GetTokenInformation(
            token,
            TokenSessionId,
            Some(&mut session_id as *mut u32 as *mut core::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            &mut ret_len,
        )
        .is_ok();

        // CheckTokenMembership(None, ...) tests the current impersonation token —
        // i.e. the client's.
        let mut interactive: BOOL = false.into();
        let mut sid_buf = [0u8; 68];
        let psid = PSID(sid_buf.as_mut_ptr() as *mut core::ffi::c_void);
        let mut cb = sid_buf.len() as u32;
        if CreateWellKnownSid(WinInteractiveSid, None, psid, &mut cb).is_ok() {
            let _ = CheckTokenMembership(None, psid, &mut interactive);
        }

        let _ = CloseHandle(token);
        let _ = RevertToSelf();

        if !sess_ok {
            return None;
        }
        Some(ClientInfo {
            session_id,
            interactive: interactive.as_bool(),
        })
    }
}
