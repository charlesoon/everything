//! User-intent preferences (design G14): the onboarding decision and the
//! auto-start opt-out live in the app's defaults domain
//! (`~/Library/Preferences/com.everything.app.plist`), NOT the index data
//! directory — deleting a multi-GB index directory must not erase what the
//! user chose. Reads and writes shell out to `defaults` so they go through
//! cfprefsd: a direct file read can observe a stale, unflushed snapshot, and
//! the on-disk plist is binary anyway.

use std::process::Command;

const DOMAIN: &str = "com.everything.app";

/// First-run onboarding decision. Values:
/// - `granted`  — user granted Full Disk Access in the onboarding flow
/// - `skipped`  — user chose "later"; the daemon builds a degraded index
/// - `granted-implicit` — pre-daemon install migrated (an index already
///   existed, so the user never sees onboarding — design §7)
/// - `cli` — `--register-daemon` opted in from the command line
pub(crate) const ONBOARDING_KEY: &str = "onboardingDecision";
/// "1" when the user turned "run at login" off in the app (the app-side
/// opt-out record; the OS-side SMAppService status is checked separately — G1).
pub(crate) const OPT_OUT_KEY: &str = "daemonOptOut";

fn defaults_read(key: &str) -> Option<String> {
    let out = Command::new("defaults")
        .args(["read", DOMAIN, key])
        .output()
        .ok()?;
    if !out.status.success() {
        return None; // a missing key or domain makes `defaults` exit non-zero
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn defaults_write(key: &str, value: &str) -> bool {
    Command::new("defaults")
        .args(["write", DOMAIN, key, value])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub(crate) fn onboarding_decision() -> Option<String> {
    defaults_read(ONBOARDING_KEY)
}

pub(crate) fn onboarding_recorded() -> bool {
    onboarding_decision().is_some()
}

pub(crate) fn record_onboarding_decision(decision: &str) -> bool {
    defaults_write(ONBOARDING_KEY, decision)
}

pub(crate) fn opted_out() -> bool {
    defaults_read(OPT_OUT_KEY).map(|v| v == "1").unwrap_or(false)
}

pub(crate) fn set_opt_out(on: bool) -> bool {
    defaults_write(OPT_OUT_KEY, if on { "1" } else { "0" })
}
