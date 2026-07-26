pub mod com_guard;
pub mod volume;
pub mod path_resolver;
pub mod mft_indexer;
pub mod usn_watcher;
pub mod dir_rescan;
pub mod service_paths;
pub mod service_log;
pub mod service_security;
pub mod service_pipe;
pub mod service;
pub mod service_client;
pub mod legacy_cleanup;
pub mod rdcw_watcher;
pub mod context_menu;
pub mod icon;

pub const EARLY_MEM_INDEX_LIMIT: usize = 200_000;

/// While the indexer service is unreachable, re-classify its SCM health at most
/// this often (each check spawns an `sc query` subprocess) rather than every 1s
/// poll tick.
const HEALTH_RECHECK: std::time::Duration = std::time::Duration::from_secs(5);

use tauri::{AppHandle, Emitter};

use crate::{db_connection, set_ready_with_cached_counts, AppState};

/// Read-only Windows GUI startup (§3.3 / D4 / D5): the GUI never indexes — the
/// service owns writes. Seed Ready from the service's DB, then poll the service
/// for status (re-emitting the existing `index_*` events so the frontend is
/// unchanged) and watch `PRAGMA data_version` to refresh results when the index
/// changes. When the pipe is unreachable, classify service health via SCM and
/// emit a `service_health` banner event — auto-starting a stopped service (the
/// D2 DACL grant lets INTERACTIVE start it without UAC).
pub fn start_windows_readonly(app: AppHandle, state: AppState) {
    // Instant Ready if the service already has an index; the poll loop refines.
    set_ready_with_cached_counts(&app, &state);

    std::thread::spawn(move || {
        use std::time::Instant;
        // §5 ②: clear this user's leftover per-user uninstall entry + shortcuts
        // once (idempotent; short-circuits after the first run).
        legacy_cleanup::clean_legacy_peruser_artifacts();
        // Dedicated long-lived read-only connection for data_version polling —
        // data_version is per-connection, so it must not be shared (D4).
        let mut dv_conn = db_connection(&state.db_path).ok();
        let mut last_dv: Option<i64> = None;
        let mut migrate_attempted = false;

        // The poll runs every second but the service's status is steady-state
        // most of the time. Re-emitting the `index_*` events unchanged would make
        // the frontend re-run a full search every tick, so each event fires only
        // when its value actually changes.
        let mut last_progress: Option<(u64, u64)> = None;
        let mut last_state: Option<(String, Option<String>)> = None;
        let mut last_health: Option<String> = None;
        // While the pipe is unreachable, re-classify health (an `sc query`
        // subprocess) at most every HEALTH_RECHECK and issue `sc start` once per
        // stopped episode, rather than spawning a process every tick.
        let mut last_health_check: Option<Instant> = None;
        let mut start_requested = false;

        loop {
            match service_client::status() {
                Some(s) => {
                    start_requested = false;
                    last_health_check = None;

                    if last_progress != Some((s.scanned, s.indexed)) {
                        crate::emit_index_progress(&app, s.scanned, s.indexed, String::new());
                        last_progress = Some((s.scanned, s.indexed));
                    }
                    if last_state
                        .as_ref()
                        .map_or(true, |(st, msg)| st != &s.state || msg != &s.message)
                    {
                        crate::emit_index_state(&app, &s.state, s.message.clone());
                        last_state = Some((s.state.clone(), s.message.clone()));
                    }

                    // D5: a service running a different binary version (e.g. an
                    // interrupted upgrade) can't be trusted — surface a restart
                    // banner and stop trusting its index (migration + data_version
                    // polling below) until the versions match.
                    let version_ok = s
                        .version
                        .as_deref()
                        .map_or(true, |sv| sv == env!("CARGO_PKG_VERSION"));
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

                    // §5: one-time migration of this user's old per-user sidecars
                    // to the service-owned config (first-wins, idempotent). Mark
                    // attempted only once the transfer actually reaches the service;
                    // a transient pipe failure must be retried on a later poll, not
                    // silently skipped until GUI restart.
                    if version_ok
                        && !migrate_attempted
                        && s.sidecar_migrated == Some(false)
                    {
                        if migrate_old_sidecars().is_ok() {
                            migrate_attempted = true;
                        }
                    }

                    // Refresh search results when the index changes (Ready only).
                    if version_ok && s.state == "Ready" {
                        if dv_conn.is_none() {
                            dv_conn = db_connection(&state.db_path).ok();
                        }
                        if let Some(ref c) = dv_conn {
                            if let Ok(dv) =
                                c.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
                            {
                                if last_dv.is_some() && last_dv != Some(dv) {
                                    crate::emit_index_updated(&app, s.indexed, crate::now_epoch(), 0);
                                }
                                last_dv = Some(dv);
                            }
                        }
                    }
                }
                None => {
                    dv_conn = None;
                    last_dv = None;
                    if last_health_check.map_or(true, |t| t.elapsed() >= HEALTH_RECHECK) {
                        last_health_check = Some(Instant::now());
                        let health = classify_service_health();
                        if health == "stopped" {
                            // D5: try to start a stopped service without UAC —
                            // once per episode, not every tick.
                            if !start_requested {
                                start_requested = true;
                                let _ = sc_command()
                                    .args(["start", service::SERVICE_NAME])
                                    .output();
                            }
                        } else {
                            start_requested = false;
                        }
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
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}

/// A `sc.exe` command that spawns no console window. The read-only GUI is a
/// windowed process, so a bare `Command::new("sc")` pops a Windows Terminal
/// window per call on Windows 11; CREATE_NO_WINDOW keeps the health-poll's
/// `sc query` / `sc start` invisible (matches win/context_menu.rs).
fn sc_command() -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new("sc");
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Classify the service for the GUI health banner (D5) via SCM. `sc query`
/// exit 1060 = not installed; otherwise parse the reported state.
fn classify_service_health() -> &'static str {
    match sc_command()
        .args(["query", service::SERVICE_NAME])
        .output()
    {
        Ok(o) if o.status.code() == Some(1060) => "not_installed",
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("RUNNING") {
                "unresponsive"
            } else if s.contains("START_PENDING") {
                "starting"
            } else if s.contains("STOPPED") || s.contains("STOP_PENDING") {
                "stopped"
            } else {
                "unknown"
            }
        }
        Err(_) => "unknown",
    }
}

/// Relaunch `--register-service` elevated via `ShellExecute("runas")` — the GUI
/// banner action for a missing/broken service (D5). The elevated process
/// registers + starts the service; the GUI's health poll then clears the banner.
pub fn register_service_elevated() -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_NORMAL;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let file: Vec<u16> = exe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let params: Vec<u16> = "--register-service"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let inst = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_NORMAL,
        )
    };
    // ShellExecuteW returns an HINSTANCE; a value > 32 means success.
    if inst.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "ShellExecute(runas) failed (code {})",
            inst.0 as isize
        ))
    }
}

/// §5: read this user's old per-user (`%APPDATA%`) sidecars and hand them to the
/// service to become the machine-global config. First-wins + idempotent on the
/// service side, so even a no-op transfer claims the marker (no retry churn).
fn migrate_old_sidecars() -> Result<(), String> {
    let Some(roaming) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
        return Ok(()); // No %APPDATA% → nothing to read; don't keep retrying.
    };
    let old = roaming.join("com.everything.app");
    let pathignore = std::fs::read_to_string(old.join(".pathignore")).ok();
    let pathindexing = std::fs::read_to_string(old.join(".pathindexing")).ok();
    service_client::migrate_sidecars(pathignore, pathindexing)
}
