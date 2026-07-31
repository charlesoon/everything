//! launchd LaunchAgent registration via SMAppService (design D3).
//!
//! The agent plist ships inside the app bundle
//! (`Contents/Library/LaunchAgents/com.everything.app.daemon.plist`) and is
//! registered with `SMAppService.agent(plistName:)`, which lists the app
//! under System Settings → Login Items where the user can turn it off.
//! Registration is only possible from an installed, non-translocated bundle —
//! dev builds report `Unsupported` and callers fall back to `spawn_detached`.
//!
//! Two contracts callers must respect:
//! - `register()` starts the daemon **immediately**, not "at next login"
//!   (G3) — gate every call on the recorded onboarding decision.
//! - Re-registration churns a running daemon and can re-fire the BTM
//!   "background item added" notification, so `self_heal` re-registers only
//!   when the embedded plist's bytes differ from the marker written at the
//!   last successful registration (G7). Binary swaps without plist changes
//!   are covered by the daemon's exe-identity watch instead.

use objc2_foundation::NSString;
use objc2_service_management::{SMAppService, SMAppServiceStatus};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const AGENT_PLIST_NAME: &str = "com.everything.app.daemon.plist";
pub(crate) const AGENT_LABEL: &str = "com.everything.app.daemon";
/// Lives in the (recreatable) data dir, not the prefs domain: it caches
/// registration state derived from the bundle, not a user choice (G14).
const MARKER_FILE: &str = "agent_plist.marker";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegState {
    /// Registered and allowed to run at login.
    Enabled,
    /// The user disabled it in System Settings → Login Items (or macOS wants
    /// explicit approval). Never overridden programmatically (D3/G1).
    RequiresApproval,
    NotRegistered,
    /// The OS can't find the service definition for this bundle.
    NotFound,
    /// No embedded plist (dev build) or a translocated copy — registration
    /// is impossible; callers use `spawn_detached` instead.
    Unsupported,
}

fn service() -> objc2::rc::Retained<SMAppService> {
    unsafe { SMAppService::agentServiceWithPlistName(&NSString::from_str(AGENT_PLIST_NAME)) }
}

pub(crate) fn is_translocated() -> bool {
    std::env::current_exe()
        .map(|p| p.components().any(|c| c.as_os_str() == "AppTranslocation"))
        .unwrap_or(false)
}

/// Path of the plist embedded in the running bundle, when this process is an
/// installed .app (exe at `Contents/MacOS/<bin>`). `None` in dev builds.
pub(crate) fn bundled_plist_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?; // MacOS/ → Contents/
    if contents.file_name()? != "Contents" {
        return None;
    }
    let plist = contents.join("Library/LaunchAgents").join(AGENT_PLIST_NAME);
    plist.exists().then_some(plist)
}

pub(crate) fn registration_supported() -> bool {
    bundled_plist_path().is_some() && !is_translocated()
}

pub(crate) fn agent_status() -> RegState {
    if !registration_supported() {
        return RegState::Unsupported;
    }
    let status = unsafe { service().status() };
    if status == SMAppServiceStatus::Enabled {
        RegState::Enabled
    } else if status == SMAppServiceStatus::RequiresApproval {
        RegState::RequiresApproval
    } else if status == SMAppServiceStatus::NotFound {
        RegState::NotFound
    } else {
        RegState::NotRegistered
    }
}

/// Register the agent. **Starts the daemon immediately** (G3).
pub(crate) fn register() -> Result<(), String> {
    if !registration_supported() {
        return Err("not an installed app bundle (dev build or translocated copy)".to_string());
    }
    unsafe { service().registerAndReturnError() }
        .map_err(|e| format!("SMAppService register failed: {}", e.localizedDescription()))
}

/// Unregister the agent. launchd SIGTERMs the running daemon as part of this.
pub(crate) fn unregister() -> Result<(), String> {
    if !registration_supported() {
        return Err("not an installed app bundle (dev build or translocated copy)".to_string());
    }
    unsafe { service().unregisterAndReturnError() }
        .map_err(|e| format!("SMAppService unregister failed: {}", e.localizedDescription()))
}

/// `launchctl kickstart` — start the registered agent right now (the Windows
/// `sc start` equivalent). Preferred over spawning so launchd stays the
/// daemon's supervisor (G2).
#[allow(dead_code)] // consumed by the P2 revival gate (ensure_daemon_running)
pub(crate) fn kickstart() -> Result<(), String> {
    let target = format!("gui/{}/{}", unsafe { libc::getuid() }, AGENT_LABEL);
    let out = Command::new("launchctl")
        .args(["kickstart", &target])
        .output()
        .map_err(|e| format!("launchctl spawn failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl kickstart {target} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// System Settings → Login Items, for the "enable it back" banner action.
#[allow(dead_code)] // consumed by the P3 approval_required banner action
pub(crate) fn open_login_items_settings() {
    unsafe { SMAppService::openSystemSettingsLoginItems() };
}

fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKER_FILE)
}

/// Record the currently embedded plist as "registered" so `self_heal` only
/// re-registers when a future release actually changes the plist.
pub(crate) fn write_marker(data_dir: &Path) {
    if let Some(plist) = bundled_plist_path() {
        if let Ok(bytes) = std::fs::read(&plist) {
            let _ = std::fs::write(marker_path(data_dir), bytes);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SelfHeal {
    Registered,
    Reregistered,
    AlreadyCurrent,
    Skipped(&'static str),
    Failed(String),
}

/// D3 self-heal, run at app start: ensure the agent is registered and the
/// plist marker is current — without ever overriding the user's opt-out or an
/// OS-side disable, and never before the onboarding decision exists
/// (`register()` would start the daemon on the spot — G3).
pub(crate) fn self_heal(data_dir: &Path) -> SelfHeal {
    if !registration_supported() {
        return SelfHeal::Skipped("dev build or translocated bundle");
    }
    if !super::prefs::onboarding_recorded() {
        return SelfHeal::Skipped("onboarding decision not recorded yet");
    }
    if super::prefs::opted_out() {
        return SelfHeal::Skipped("user opted out of auto-start");
    }
    let status = agent_status();
    if status == RegState::RequiresApproval {
        return SelfHeal::Skipped("disabled in System Settings (user's choice wins)");
    }
    let Some(plist) = bundled_plist_path() else {
        return SelfHeal::Skipped("embedded plist missing");
    };
    let bundled = match std::fs::read(&plist) {
        Ok(bytes) => bytes,
        Err(e) => return SelfHeal::Failed(format!("read embedded plist: {e}")),
    };
    let marker = marker_path(data_dir);
    let marker_current = std::fs::read(&marker).ok().as_deref() == Some(bundled.as_slice());

    if marker_current && status == RegState::Enabled {
        return SelfHeal::AlreadyCurrent;
    }

    // Registered but the embedded plist changed: launchd only picks up new
    // plist content through an unregister → register cycle (G7 keeps this
    // rare). A merely missing/broken registration registers directly.
    let reregister = !marker_current && status == RegState::Enabled;
    if reregister {
        if let Err(e) = unregister() {
            return SelfHeal::Failed(format!("unregister before re-register: {e}"));
        }
    }
    if let Err(e) = register() {
        return SelfHeal::Failed(e);
    }
    if let Err(e) = std::fs::write(&marker, &bundled) {
        return SelfHeal::Failed(format!("registered, but marker write failed: {e}"));
    }
    if reregister {
        SelfHeal::Reregistered
    } else {
        SelfHeal::Registered
    }
}
