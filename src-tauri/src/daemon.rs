//! Headless indexer daemon (`everything --daemon`).
//!
//! A single resident process that owns index writes so the index stays fresh
//! without the GUI. It builds/catches up the index at startup, then keeps it
//! current: on macOS via the FSEvents watcher (real-time), elsewhere via a
//! periodic catchup pass. A resident supervisory loop heals a failed build and
//! restarts a dead watcher so the daemon can never wedge silently.
//!
//! Single-writer coordination (WAL allows only one writer):
//!   * daemon-vs-daemon — the `daemon.lock` advisory lock; a duplicate self-exits.
//!   * daemon-vs-GUI — the GUI holds a `gui.lock` beacon for its lifetime; the
//!     daemon defers to it (never spawns / exits promptly when the GUI appears),
//!     so the two never write concurrently.
//!
//! Bootstrap: the MCP server (`--mcp`) spawns this detached on startup (unless
//! a GUI or another daemon is already the writer), so an agent that only ever
//! runs MCP still gets a resident, self-healing indexer that outlives the MCP
//! session.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

use fs2::FileExt;

use crate::mcp_server::default_db_path;
use crate::{
    build_app_state, cleanup_entries_gc_tables, db_connection, ensure_db_indexes,
    finalize_fresh_index, get_meta, init_db_tables, resolve_home_dir, run_db_maintenance,
    run_incremental_index, AppState,
};

/// How often the resident daemon re-checks its invariants (GUI took over, a
/// build needs healing, the watcher died).
#[cfg(target_os = "macos")]
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(5);
/// Non-macOS reconcile cadence (no live watcher there).
#[cfg(not(target_os = "macos"))]
const CATCHUP_INTERVAL: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Advisory locks (writer coordination)
// ---------------------------------------------------------------------------

fn daemon_lock_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("daemon.lock")
}

fn gui_lock_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("gui.lock")
}

/// Try to take an exclusive advisory lock on `path`. Returns the held `File`
/// (release by dropping it or exiting the process) or `None` when another live
/// process holds it. Auto-releases on process death.
fn try_acquire(path: &Path) -> Option<File> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .ok()?;
    match file.try_lock_exclusive() {
        Ok(()) => Some(file),
        Err(_) => None,
    }
}

fn try_acquire_daemon_lock(db_path: &Path) -> Option<File> {
    try_acquire(&daemon_lock_path(db_path))
}

/// Whether a daemon is already resident (holds `daemon.lock`).
fn daemon_running(db_path: &Path) -> bool {
    try_acquire_daemon_lock(db_path).is_none()
}

/// Whether the GUI is running: it holds `gui.lock` for its whole lifetime, so
/// if we cannot take that lock the GUI owns writes and the daemon must stand
/// down. (Probe only — the lock is released immediately.)
pub(crate) fn gui_is_running(db_path: &Path) -> bool {
    try_acquire(&gui_lock_path(db_path)).is_none()
}

static GUI_BEACON: OnceLock<File> = OnceLock::new();

/// Called once on GUI startup: hold `gui.lock` for the process lifetime so any
/// daemon defers to the GUI as the single writer. Best-effort.
pub(crate) fn hold_gui_beacon(db_path: &Path) {
    if let Some(file) = try_acquire(&gui_lock_path(db_path)) {
        let _ = GUI_BEACON.set(file);
    }
}

// ---------------------------------------------------------------------------
// Index build / readiness
// ---------------------------------------------------------------------------

/// Whether the last index pass ran to completion (`index_complete=1`). Used by
/// the supervisory loop to decide whether a failed build needs healing.
fn index_complete(db_path: &Path) -> bool {
    db_connection(db_path)
        .ok()
        .and_then(|c| get_meta(&c, "index_complete"))
        .as_deref()
        == Some("1")
}

/// Read whether the next index run is fresh (no prior run) and whether the FTS
/// index is flagged dirty — captured before the run mutates `last_run_id`, so
/// the daemon can finalize (build indexes / rebuild FTS) itself; the `app:None`
/// run path skips the GUI's deferred finalizing thread.
fn fresh_and_dirty(db_path: &Path) -> (bool, bool) {
    match db_connection(db_path) {
        Ok(conn) => {
            let is_fresh = get_meta(&conn, "last_run_id")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0)
                == 0;
            let fts_dirty = get_meta(&conn, "fts_dirty").map(|v| v == "1").unwrap_or(false);
            (is_fresh, fts_dirty)
        }
        Err(_) => (true, false),
    }
}

/// Run one index pass and, when it was a fresh build (or crash recovery), the
/// finalization the `app:None` path skips: secondary indexes, FTS rebuild, GC,
/// and storage maintenance (VACUUM / WAL truncate) — mirroring the GUI's
/// finalizing thread. `indexing_active` is held for the whole pass so a live
/// watcher stands down instead of racing this writer.
fn build_index(db_path: &Path, state: &AppState) {
    state.indexing_active.store(true, Ordering::Release);
    let (is_fresh, fts_dirty) = fresh_and_dirty(db_path);
    let result = run_incremental_index(None, state);
    // Ensure the guard is cleared even on error (the Ok path also clears it).
    state.indexing_active.store(false, Ordering::Release);
    match result {
        Ok(()) => {
            if is_fresh || fts_dirty {
                finalize_fresh_index(state);
            }
            if let Err(e) = ensure_db_indexes(db_path) {
                eprintln!("[daemon] ensure_db_indexes failed: {e}");
            }
            // Storage/GC maintenance only after a full (re)build, matching the
            // GUI finalizing thread; a plain catchup barely changes the file.
            if is_fresh || fts_dirty {
                if let Ok(conn) = db_connection(db_path) {
                    if let Err(e) = cleanup_entries_gc_tables(&conn) {
                        eprintln!("[daemon] gc cleanup failed: {e}");
                    }
                }
                run_db_maintenance(state);
            }
        }
        Err(e) => eprintln!("[daemon] index pass failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Detached spawn (bootstrap from the MCP server)
// ---------------------------------------------------------------------------

/// Spawn `everything --daemon` as a detached background process that outlives
/// the caller. Skips when the GUI (writer owner) or another daemon is already
/// running, so repeated MCP sessions don't churn short-lived processes.
pub fn spawn_detached() {
    let db_path = default_db_path();
    if gui_is_running(&db_path) {
        eprintln!("[daemon] GUI owns index writes; not spawning a daemon");
        return;
    }
    if daemon_running(&db_path) {
        return; // one is already resident
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[daemon] spawn skipped: current_exe failed: {e}");
            return;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Detach from the caller's process group so the daemon survives the caller
    // (e.g. an MCP session) exiting or being group-killed.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    match cmd.spawn() {
        Ok(_) => eprintln!("[daemon] spawned detached indexer daemon"),
        Err(e) => eprintln!("[daemon] spawn failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Resident daemon
// ---------------------------------------------------------------------------

/// Entry point for `everything --daemon`. Runs until another writer owns the
/// index (GUI/another daemon) or startup fails hard; otherwise resident.
pub fn run_daemon() {
    let db_path = default_db_path();
    let home_dir = resolve_home_dir();
    eprintln!(
        "[daemon] everything indexer daemon v{} db={}",
        env!("CARGO_PKG_VERSION"),
        db_path.display()
    );

    // Defer to the GUI, which owns all writes while it runs.
    if gui_is_running(&db_path) {
        eprintln!("[daemon] GUI is running; it owns index writes — exiting");
        return;
    }
    let Some(_lock) = try_acquire_daemon_lock(&db_path) else {
        eprintln!("[daemon] another daemon already running; exiting");
        return;
    };

    let app_data_dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    if let Err(e) = fs::create_dir_all(&app_data_dir) {
        eprintln!("[daemon] create data dir failed: {e}");
        return;
    }
    if let Err(e) = init_db_tables(&db_path) {
        eprintln!("[daemon] init_db_tables failed: {e}");
        return;
    }

    let state = build_app_state(db_path.clone(), home_dir, &app_data_dir);

    // Startup reconcile: a fresh build if the DB is empty, otherwise a catchup
    // covering everything changed while no daemon was running.
    build_index(&db_path, &state);
    eprintln!("[daemon] index ready; entering resident mode");

    #[cfg(target_os = "macos")]
    run_macos_resident(db_path, state);
    #[cfg(not(target_os = "macos"))]
    run_polling_resident(db_path, state);
}

/// macOS: real-time freshness via the FSEvents watcher, supervised so a failed
/// build gets healed and a dead watcher gets restarted (instead of parking
/// forever), and so the daemon yields promptly once the GUI takes over.
#[cfg(target_os = "macos")]
fn run_macos_resident(db_path: PathBuf, state: AppState) {
    let mut watcher_started = false;
    loop {
        if gui_is_running(&db_path) {
            eprintln!("[daemon] GUI now running; yielding writes and exiting");
            // Stop the watcher so it flushes and stops writing before we exit.
            state.watcher_stop.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(200));
            return;
        }
        // Heal an incomplete/failed build so MCP never stays stuck "not ready".
        if !index_complete(&db_path) {
            eprintln!("[daemon] index incomplete; (re)building");
            build_index(&db_path, &state);
        }
        // (Re)start the watcher on first pass, or if it died (e.g. FSEvents
        // init failure left the worker thread dead).
        if !state.watcher_active.load(Ordering::Acquire) {
            if watcher_started {
                eprintln!("[daemon] watcher not active; restarting");
            }
            state.watcher_stop.store(false, Ordering::Release);
            // Replay from the last persisted event id to cover the gap since
            // the daemon last ran (idempotent upserts absorb any overlap).
            let since = stored_event_id(&db_path);
            crate::start_fsevent_watcher_worker(None, state.clone(), since, false);
            watcher_started = true;
        }
        std::thread::sleep(SUPERVISE_INTERVAL);
    }
}

/// The last FSEvents id the watcher persisted, if any.
#[cfg(target_os = "macos")]
fn stored_event_id(db_path: &Path) -> Option<u64> {
    db_connection(db_path)
        .ok()
        .and_then(|c| get_meta(&c, "last_event_id"))
        .and_then(|v| v.parse::<u64>().ok())
}

/// Non-macOS: no headless live watcher yet, so reconcile on a fixed cadence.
/// `build_index` heals a dirty FTS (fresh/crash-recovery finalize) as well.
#[cfg(not(target_os = "macos"))]
fn run_polling_resident(db_path: PathBuf, state: AppState) {
    loop {
        std::thread::sleep(CATCHUP_INTERVAL);
        if gui_is_running(&db_path) {
            eprintln!("[daemon] GUI now running; exiting");
            return;
        }
        build_index(&db_path, &state);
    }
}
