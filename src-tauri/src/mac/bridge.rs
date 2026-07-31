//! Read-only macOS GUI startup (design D4/D5, §5.3) — the mirror of
//! `win/mod.rs::start_windows_readonly`.
//!
//! The GUI never indexes: the launchd daemon owns every write. This bridge
//! seeds Ready from the daemon's DB, then polls the daemon's UDS status once
//! a second, re-emitting the existing `index_*` events (deduped — an
//! unconditional re-emit would make the frontend re-search every tick) and
//! mirroring the polled values into `state.status` so `get_index_status`
//! tells the same story. A dedicated long-lived connection watches
//! `PRAGMA data_version` to refresh results when the daemon writes (S3:
//! negative/fd caches invalidated, pooled search connections dropped after a
//! table swap, `fts_ready` re-derived). While the socket is unreachable the
//! daemon is revived through the §4 single rule (`ensure_daemon_running`),
//! retried on `REVIVAL_RETRY` — a single attempt can legitimately no-op (the
//! dying daemon still holds `daemon.lock`, launchd's ~10s relaunch throttle,
//! the bundle mid-replace during an update) and must not strand the daemon
//! for the rest of the GUI session. The outage is classified into
//! `service_health` states for the banner. While the user has auto-start
//! turned off, the bridge keeps the lease daemon alive with `attach_lease`
//! renewals (G6).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};

use crate::daemon_ipc::{self, DaemonStatus};
use crate::{
    daemon, db_connection_for_search, diag, fts_usable, invalidate_search_caches, now_epoch,
    set_ready_with_cached_counts, AppState, IndexState,
};

/// While the daemon is unreachable, re-classify health at most this often
/// (each check runs SMAppService + `defaults` probes), mirroring the Windows
/// bridge's `sc query` throttle.
const HEALTH_RECHECK: Duration = Duration::from_secs(5);
/// An unreachable-daemon episode reads as "starting" for this long after the
/// revival attempt (launchd relaunch throttle is ~10s) before it hardens into
/// stopped/unresponsive.
const STARTING_GRACE: Duration = Duration::from_secs(15);
/// Lease renewal cadence while auto-start is off (TTL is 5 min — G6).
const LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(60);
/// Re-attempt daemon revival on this cadence while the outage persists. One
/// shot is not enough: `ensure_daemon_running` no-ops while the dying daemon
/// still holds `daemon.lock`, and `launchctl kickstart` can fail inside
/// launchd's ~10s relaunch throttle or while Finder is mid-replace of the
/// bundle. Longer than STARTING_GRACE's rationale (the launchd throttle) so
/// retries never hammer launchd.
const REVIVAL_RETRY: Duration = Duration::from_secs(15);

/// Whether a read-only open of the daemon's DB is currently possible and the
/// schema exists — the gate for `db_ready` (until then search rides the
/// Spotlight fallback).
pub(crate) fn macos_db_usable(db_path: &Path) -> bool {
    if !db_path.exists() {
        return false;
    }
    db_connection_for_search(db_path)
        .and_then(|c| {
            c.query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='entries'",
                [],
                |_| Ok(()),
            )
            .map_err(|e| e.to_string())
        })
        .is_ok()
}

/// Map the daemon's §4 lifecycle + classic index state onto the event labels
/// the frontend already understands. `Building` on a non-fresh pass gets the
/// `"Catchup: …"` message prefix `emit_index_state` derives `isCatchup` from.
fn map_status(s: &DaemonStatus) -> (&'static str, Option<String>) {
    if s.index_state == "Error" {
        return ("Error", s.message.clone());
    }
    match s.state.as_str() {
        // Carry the message into Ready: after a completed pass it holds the
        // "some paths could not be read" line, which is the only explanation
        // the user gets for a nonzero failure count. Dropping it here left the
        // status bar showing a bare number and no reason.
        "Ready" => ("Ready", s.message.clone()),
        "Building" => {
            if s.is_fresh {
                ("Indexing", s.message.clone())
            } else {
                ("Indexing", Some("Catchup: 변경분 반영 중".to_string()))
            }
        }
        // WaitingOnboarding surfaces as a quiet Indexing until the P3
        // onboarding UI takes over; Starting/Quiescing are transient.
        _ => ("Indexing", None),
    }
}

pub fn start_macos_readonly(app: AppHandle, state: AppState) {
    // Instant Ready if the daemon already built an index; the poll refines.
    set_ready_with_cached_counts(&app, &state);

    std::thread::spawn(move || {
        let data_dir: PathBuf = state
            .db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        // Dedicated long-lived read-only connection for data_version polling —
        // data_version is per-connection, so it must not be shared (D5).
        let mut dv_conn = db_connection_for_search(&state.db_path).ok();
        let mut last_dv: Option<i64> = None;

        let mut last_progress: Option<(u64, u64)> = None;
        let mut last_state: Option<(String, Option<String>)> = None;
        let mut last_health: Option<String> = None;
        let mut last_health_check: Option<Instant> = None;
        // Revival attempts are paced by REVIVAL_RETRY within an unreachable
        // episode (cleared when status answers again).
        let mut outage_since: Option<Instant> = None;
        let mut last_revival_attempt: Option<Instant> = None;
        // One automatic restart per version-mismatch episode (G15: only when
        // the daemon is older — a stray old GUI must not churn a new daemon).
        let mut restart_sent = false;
        let mut last_lease_renew: Option<Instant> = None;

        loop {
            match daemon_ipc::status(&data_dir) {
                Some(s) => {
                    outage_since = None;
                    last_revival_attempt = None;
                    last_health_check = None;

                    if !state.db_ready.load(Ordering::Acquire) && macos_db_usable(&state.db_path)
                    {
                        state.db_ready.store(true, Ordering::Release);
                        eprintln!("[bridge] daemon DB is readable; leaving Spotlight fallback");
                    }
                    state
                        .daemon_building
                        .store(s.state == "Building", Ordering::Release);

                    let (ui_state, ui_msg) = map_status(&s);
                    {
                        // Mirror into state.status so get_index_status (the
                        // frontend's pull path) agrees with the events.
                        let mut status = state.status.lock();
                        status.entries_count = s.entries_count;
                        status.scanned = s.scanned;
                        status.indexed = s.indexed;
                        status.permission_errors = s.permission_errors;
                        status.state = match ui_state {
                            "Ready" => IndexState::Ready,
                            "Error" => IndexState::Error,
                            _ => IndexState::Indexing,
                        };
                        status.message = ui_msg.clone();
                    }
                    if last_progress != Some((s.scanned, s.indexed)) {
                        crate::emit_index_progress(&app, s.scanned, s.indexed, String::new());
                        last_progress = Some((s.scanned, s.indexed));
                    }
                    if last_state
                        .as_ref()
                        .map_or(true, |(st, msg)| st != ui_state || msg != &ui_msg)
                    {
                        crate::emit_index_state(&app, ui_state, ui_msg.clone());
                        last_state = Some((ui_state.to_string(), ui_msg));
                    }

                    // A daemon running a different binary version can't be
                    // trusted. Only a daemon OLDER than this GUI is restarted
                    // (once per episode) — launchd then relaunches the current
                    // on-disk binary; a newer daemon means THIS GUI is the
                    // stale copy and must not churn it (G15).
                    let gui_version = env!("CARGO_PKG_VERSION");
                    let version_ok = s.version.as_deref().map_or(true, |v| v == gui_version);
                    if !version_ok {
                        let daemon_is_older = s
                            .version
                            .as_deref()
                            .map_or(false, |v| version_lt(v, gui_version));
                        if daemon_is_older && !restart_sent {
                            restart_sent = true;
                            diag::log(format!(
                                "bridge: daemon v{} older than GUI v{gui_version} → IPC restart",
                                s.version.as_deref().unwrap_or("?")
                            ));
                            let _ = daemon_ipc::restart(&data_dir);
                        }
                    } else {
                        restart_sent = false;
                    }
                    let health_key = if version_ok { "ok" } else { "version_mismatch" };
                    if last_health.as_deref() != Some(health_key) {
                        let payload = if version_ok {
                            serde_json::json!({ "ok": true })
                        } else {
                            serde_json::json!({ "ok": false, "state": "version_mismatch" })
                        };
                        let _ = app.emit("service_health", payload);
                        last_health = Some(health_key.to_string());
                    }

                    // Refresh search results when the index changes (Ready only).
                    if version_ok && s.state == "Ready" {
                        if dv_conn.is_none() {
                            dv_conn = db_connection_for_search(&state.db_path).ok();
                        }
                        if let Some(ref c) = dv_conn {
                            if let Ok(dv) =
                                c.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
                            {
                                if last_dv.is_some() && last_dv != Some(dv) {
                                    // S3: the daemon wrote — drop stale caches.
                                    invalidate_search_caches(&state);
                                    // A daemon-side reset swaps the entries
                                    // table; pooled statements must not
                                    // outlive it. fts_ready can flip either
                                    // way (reset → dirty; finalize → rebuilt).
                                    state.search_conn_pool.lock().clear();
                                    state
                                        .fts_ready
                                        .store(fts_usable(c), Ordering::Release);
                                    crate::emit_index_updated(
                                        &app,
                                        s.entries_count,
                                        now_epoch(),
                                        s.permission_errors,
                                    );
                                }
                                last_dv = Some(dv);
                            }
                        }
                    }

                    // Keep the lease daemon alive while auto-start is off (G6).
                    if s.lease_mode
                        && last_lease_renew.map_or(true, |t| t.elapsed() >= LEASE_RENEW_INTERVAL)
                    {
                        last_lease_renew = Some(Instant::now());
                        let _ = daemon_ipc::attach_lease(&data_dir);
                    }
                }
                None => {
                    dv_conn = None;
                    last_dv = None;
                    state.daemon_building.store(false, Ordering::Release);
                    let now = Instant::now();
                    let outage_started = *outage_since.get_or_insert(now);
                    if last_health_check.map_or(true, |t| t.elapsed() >= HEALTH_RECHECK) {
                        last_health_check = Some(now);
                        // Revive through the §4 single rule (kickstart / spawn
                        // / lease-spawn / nothing), retrying while the outage
                        // persists — the gate is idempotent when a daemon is
                        // already resident.
                        if last_revival_attempt.map_or(true, |t| t.elapsed() >= REVIVAL_RETRY) {
                            last_revival_attempt = Some(now);
                            daemon::ensure_daemon_running("bridge");
                        }
                        let health = classify_daemon_health(
                            &state.db_path,
                            now.duration_since(outage_started),
                        );
                        if last_health.as_deref() != Some(health) {
                            let _ = app.emit(
                                "service_health",
                                serde_json::json!({ "ok": false, "state": health }),
                            );
                            last_health = Some(health.to_string());
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// Classify an unreachable-daemon outage for the `service_health` banner.
/// The first `STARTING_GRACE` of every outage reads as "starting" (the
/// revival was just requested and launchd throttles relaunches to ~10s).
fn classify_daemon_health(db_path: &Path, outage_for: Duration) -> &'static str {
    use super::{launch_agent, launch_agent::RegState, prefs};
    if outage_for < STARTING_GRACE {
        return "starting";
    }
    if prefs::opted_out() {
        return "disabled";
    }
    match launch_agent::agent_status() {
        RegState::RequiresApproval => "approval_required",
        RegState::Enabled | RegState::NotFound => {
            if daemon::daemon_running(db_path) {
                // A process holds daemon.lock but the socket doesn't answer —
                // wedged or a pre-IPC daemon version.
                "unresponsive"
            } else {
                "stopped"
            }
        }
        RegState::NotRegistered => "not_registered",
        // Dev build / translocated copy: registration impossible; the revival
        // gate spawn is the only recovery, so keep reporting "starting" while
        // it retries (a persistent failure shows as repeated grace windows).
        RegState::Unsupported => {
            if daemon::daemon_running(db_path) {
                "unresponsive"
            } else {
                "stopped"
            }
        }
    }
}

/// Best-effort semver-ish comparison ("0.5.3" style). Unparseable segments
/// compare equal, so weird versions never trigger a restart storm.
fn version_lt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_lt_orders_dotted_versions() {
        assert!(version_lt("0.5.2", "0.5.3"));
        assert!(version_lt("0.5.3", "0.6.0"));
        assert!(!version_lt("0.6.0", "0.5.9"));
        assert!(!version_lt("0.5.3", "0.5.3"));
        assert!(version_lt("0.5", "0.5.1"));
    }

    #[test]
    fn map_status_labels_catchup_builds() {
        let mut s = DaemonStatus {
            state: "Building".to_string(),
            index_state: "Indexing".to_string(),
            ..Default::default()
        };
        let (label, msg) = map_status(&s);
        assert_eq!(label, "Indexing");
        assert!(msg.unwrap().starts_with("Catchup:"), "non-fresh build is a catchup");

        s.is_fresh = true;
        let (label, msg) = map_status(&s);
        assert_eq!(label, "Indexing");
        assert!(msg.is_none());

        s.state = "Ready".to_string();
        assert_eq!(map_status(&s).0, "Ready");

        s.index_state = "Error".to_string();
        s.message = Some("boom".to_string());
        let (label, msg) = map_status(&s);
        assert_eq!(label, "Error");
        assert_eq!(msg.as_deref(), Some("boom"));
    }
}
