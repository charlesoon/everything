//! Headless indexer daemon (`everything --daemon`).
//!
//! A single resident process that owns index writes so the index stays fresh
//! without the GUI. It builds/catches up the index at startup, then keeps it
//! current: on macOS via the FSEvents watcher (real-time), elsewhere via a
//! periodic catchup pass. A resident supervisory loop heals a failed build and
//! restarts a dead watcher so the daemon can never wedge silently.
//!
//! macOS lifecycle (design §4, D8): launchd runs the daemon as a login-item
//! LaunchAgent (`mac/launch_agent.rs`) with `KeepAlive.SuccessfulExit=false`,
//! making the exit code the supervision contract — `exit(0)` = dormant until
//! the next login/kickstart, `exit(1)` = relaunch from the on-disk binary.
//! States: Starting → WaitingOnboarding → Building → Ready → Quiescing,
//! reported over the UDS control socket (`daemon_ipc.rs`), which opens before
//! the first build so status/restart/disable always answer. SIGTERM (logout,
//! shutdown, unregister) quiesces cleanly. A supervision tick watches the
//! executable's identity (update/rollback → exit 1; deletion → dormant), the
//! FDA rising edge, and — in `--lease` mode (opt-out users) — the renewable
//! lease's liveness.
//!
//! Single-writer coordination (WAL allows only one writer):
//!   * daemon-vs-daemon — the `daemon.lock` advisory lock; a duplicate self-exits.
//!   * daemon-vs-GUI — the macOS GUI is a hard read-only reader (D4): it never
//!     writes, so there is nothing to hand over. Its `gui.lock` beacon remains
//!     for second-instance detection, the lease-liveness check, and as the
//!     yield signal pre-flip daemons still in the field honor (§7). The
//!     post-flip GUI also holds `gui_ro.lock`; a `gui.lock` holder *without*
//!     it is a pre-flip writer GUI (old build still installed) — the daemon
//!     cancels/defers full passes and skips exclusive maintenance while it is
//!     up, keeping the single-writer invariant across versions. On Linux the
//!     GUI still owns writes while running and the daemon defers to it.
//!
//! Bootstrap: the MCP server (`--mcp`) spawns this detached on startup (unless
//! a GUI or another daemon is already the writer), so an agent that only ever
//! runs MCP still gets a resident, self-healing indexer that outlives the MCP
//! session.

#[cfg(target_os = "macos")]
use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
#[cfg(target_os = "macos")]
use std::io;
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "windows"))]
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(not(target_os = "windows"))]
use std::sync::OnceLock;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

use fs2::FileExt;
#[cfg(target_os = "macos")]
use parking_lot::Mutex;

use crate::diag;
use crate::mcp_server::default_db_path;
use crate::{
    build_app_state, cleanup_entries_gc_tables, db_connection, ensure_db_indexes,
    finalize_fresh_index, get_meta, init_db_tables, resolve_home_dir, run_db_maintenance,
    run_incremental_index, AppState,
};
#[cfg(target_os = "macos")]
use crate::{
    cached_effective_ignore_rules, invalidate_search_caches, pathindexing,
    purge_ignored_entries, refresh_and_emit_status_counts, reset_index_storage, set_meta,
};

/// How often the resident daemon polls the GUI beacon so it yields writes
/// almost immediately when the GUI launches. This is the daemon half of the
/// handshake: the GUI waits for this exit before it starts writing, so the two
/// never overlap as WAL writers.
const GUI_POLL_INTERVAL: Duration = Duration::from_millis(300);
/// Cadence for heavier supervision (heal a failed build, restart a dead
/// watcher, FDA probe, exe-identity, lease liveness) — kept coarse; only the
/// GUI-yield/quiesce checks need to be fast.
#[cfg(target_os = "macos")]
const SUPERVISE_INTERVAL: Duration = Duration::from_secs(5);
/// Linux fallback reconcile cadence (no live watcher there).
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const CATCHUP_INTERVAL: Duration = Duration::from_secs(60);
/// How long the GUI waits for a resident daemon to exit before it starts
/// writing (the GUI half of the handshake). Bounded so startup can't hang.
/// Linux only — on Windows/macOS the service/daemon is the sole writer, so
/// there is no GUI↔daemon handshake (D6/D4).
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const DAEMON_EXIT_WAIT: Duration = Duration::from_secs(10);
/// While FDA is missing, re-probe on this cadence (not every supervise tick —
/// each probe touches tccd; 60s keeps the rising edge ≤1min behind, G9).
#[cfg(target_os = "macos")]
const FDA_PROBE_INTERVAL: Duration = Duration::from_secs(60);
/// A lease user (GUI bridge / MCP session) that hasn't renewed within the TTL
/// is presumed dead. Renewals come every ~60s, so this absorbs several missed
/// beats before a lease daemon self-retires (R10).
#[cfg(target_os = "macos")]
pub(crate) const LEASE_TTL: Duration = Duration::from_secs(300);
/// Consecutive supervise ticks the executable must be missing before that
/// counts as "app deleted" — Finder's replace-on-update briefly unlinks the
/// bundle, which must read as an update, not a deletion (G12).
#[cfg(target_os = "macos")]
const EXE_MISSING_TICKS: u32 = 3;

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

pub(crate) fn try_acquire_daemon_lock(db_path: &Path) -> Option<File> {
    try_acquire(&daemon_lock_path(db_path))
}

/// Whether a daemon is already resident (holds `daemon.lock`). Exposed so the
/// GUI can defer starting its own writers until a resident daemon has yielded.
pub(crate) fn daemon_running(db_path: &Path) -> bool {
    try_acquire_daemon_lock(db_path).is_none()
}

/// Whether the GUI is running: it holds `gui.lock` for its whole lifetime, so
/// if we cannot take that lock the GUI owns writes and the daemon must stand
/// down. (Probe only — the lock is released immediately.)
pub(crate) fn gui_is_running(db_path: &Path) -> bool {
    try_acquire(&gui_lock_path(db_path)).is_none()
}

#[cfg(target_os = "macos")]
fn gui_ro_lock_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("gui_ro.lock")
}

/// Whether a pre-flip *writer* GUI is up: `gui.lock` held without the
/// `gui_ro.lock` read-only marker (post-flip GUIs hold both; bench mode —
/// deliberately a writer — holds only `gui.lock`). While true the daemon
/// must not run full passes or exclusive maintenance (§7).
#[cfg(target_os = "macos")]
pub(crate) fn writer_gui_running(db_path: &Path) -> bool {
    gui_is_running(db_path) && try_acquire(&gui_ro_lock_path(db_path)).is_some()
}

#[cfg(not(target_os = "windows"))]
static GUI_BEACON: OnceLock<File> = OnceLock::new();

#[cfg(target_os = "macos")]
static GUI_RO_BEACON: OnceLock<File> = OnceLock::new();

/// Called on read-only GUI startup, alongside `hold_gui_beacon`: marks this
/// `gui.lock` holder as a post-flip reader so the daemon keeps writing.
/// Bench mode must NOT call it — bench is a real writer the daemon yields to.
#[cfg(target_os = "macos")]
pub(crate) fn hold_gui_ro_beacon(db_path: &Path) {
    if let Some(file) = try_acquire(&gui_ro_lock_path(db_path)) {
        let _ = GUI_RO_BEACON.set(file);
    }
}

/// Called once on GUI startup: hold `gui.lock` for the process lifetime so any
/// daemon defers to the GUI as the single writer. Returns whether the beacon
/// was acquired — a failure means a resident daemon won't see the GUI and won't
/// yield, so the caller should surface it rather than silently overlapping.
#[cfg(not(target_os = "windows"))]
pub(crate) fn hold_gui_beacon(db_path: &Path) -> bool {
    match try_acquire(&gui_lock_path(db_path)) {
        Some(file) => {
            let _ = GUI_BEACON.set(file);
            true
        }
        None => {
            eprintln!(
                "[gui] WARNING: could not acquire gui.lock beacon ({}); a background \
                 daemon may not yield index writes to the GUI",
                gui_lock_path(db_path).display()
            );
            diag::log(format!(
                "WARNING: gui.lock beacon not acquired ({}) — another GUI instance owns index writes",
                gui_lock_path(db_path).display()
            ));
            false
        }
    }
}

/// The GUI half of the handshake (Linux only — see DAEMON_EXIT_WAIT): after
/// holding the beacon, block until any resident daemon has observed it and
/// exited (released `daemon.lock`) so the GUI can start writing without
/// overlapping the daemon. Bounded — if a daemon lingers past the deadline we
/// proceed anyway (WAL keeps writes safe).
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn wait_for_daemon_exit(db_path: &Path) {
    let started = std::time::Instant::now();
    let mut waited = false;
    while daemon_running(db_path) {
        waited = true;
        if started.elapsed() >= DAEMON_EXIT_WAIT {
            eprintln!(
                "[gui] daemon still resident after {}s; proceeding (WAL-safe: the \
                 daemon skips VACUUM while the GUI beacon is held)",
                DAEMON_EXIT_WAIT.as_secs()
            );
            diag::log(format!(
                "ALERT: daemon still resident after {}s wait — GUI proceeding \
                 (dual-writer window: the daemon is likely mid-build and passes can \
                 now fail with SQLITE_BUSY on either side)",
                DAEMON_EXIT_WAIT.as_secs()
            ));
            return;
        }
        std::thread::sleep(GUI_POLL_INTERVAL);
    }
    if waited {
        eprintln!("[gui] daemon yielded; GUI now owns index writes");
        diag::log("daemon yielded; GUI owns index writes");
    }
}

// ---------------------------------------------------------------------------
// Lifecycle state + IPC-shared control block (macOS, design §4)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    Starting,
    WaitingOnboarding,
    Building,
    Ready,
    Quiescing,
}

#[cfg(target_os = "macos")]
impl Lifecycle {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Lifecycle::Starting => "Starting",
            Lifecycle::WaitingOnboarding => "WaitingOnboarding",
            Lifecycle::Building => "Building",
            Lifecycle::Ready => "Ready",
            Lifecycle::Quiescing => "Quiescing",
        }
    }
}

/// State shared between the daemon's main loop and the UDS control server.
#[cfg(target_os = "macos")]
pub(crate) struct DaemonControl {
    lifecycle: Mutex<Lifecycle>,
    /// One-shot: the IPC server sets it, the resident loop consumes it.
    pub(crate) reset_requested: AtomicBool,
    /// Requested exit code (restart=1 / disable=0). `disable` wins: once a 0
    /// is recorded a later restart cannot upgrade it back to a relaunch.
    exit_request: Mutex<Option<i32>>,
    pub(crate) last_reset: Mutex<Option<Instant>>,
    /// Live lease users keyed by (pid, per-process token) → last renewal.
    leases: Mutex<HashMap<(u32, u64), Instant>>,
    pub(crate) fda_ok: AtomicBool,
    pub(crate) degraded: AtomicBool,
    /// Whether the current/last build is a fresh pass (vs catchup) — lets the
    /// GUI bridge label progress as catchup (isCatchup).
    pub(crate) is_fresh: AtomicBool,
    pub(crate) lease_mode: bool,
    pub(crate) socket_path: Mutex<Option<PathBuf>>,
}

#[cfg(target_os = "macos")]
impl DaemonControl {
    pub(crate) fn new(lease_mode: bool) -> Self {
        Self {
            lifecycle: Mutex::new(Lifecycle::Starting),
            reset_requested: AtomicBool::new(false),
            exit_request: Mutex::new(None),
            last_reset: Mutex::new(None),
            leases: Mutex::new(HashMap::new()),
            fda_ok: AtomicBool::new(false),
            degraded: AtomicBool::new(false),
            is_fresh: AtomicBool::new(false),
            lease_mode,
            socket_path: Mutex::new(None),
        }
    }

    pub(crate) fn lifecycle(&self) -> Lifecycle {
        *self.lifecycle.lock()
    }

    pub(crate) fn set_lifecycle(&self, lifecycle: Lifecycle) {
        *self.lifecycle.lock() = lifecycle;
    }

    /// Record an exit request. `disable` (0) wins over `restart` (1) — G15.
    pub(crate) fn request_exit(&self, code: i32) {
        let mut slot = self.exit_request.lock();
        *slot = Some(match *slot {
            Some(0) => 0,
            _ => code,
        });
    }

    pub(crate) fn requested_exit(&self) -> Option<i32> {
        *self.exit_request.lock()
    }

    pub(crate) fn attach_lease(&self, pid: u32, token: u64) {
        self.leases.lock().insert((pid, token), Instant::now());
    }

    /// Prunes expired leases and returns how many users are still live.
    pub(crate) fn live_lease_count(&self) -> usize {
        let mut leases = self.leases.lock();
        leases.retain(|_, renewed| renewed.elapsed() < LEASE_TTL);
        leases.len()
    }

    /// Cover the gap between a lease spawn and the first user's attach (the
    /// spawner can't attach before the UDS server answers).
    pub(crate) fn seed_grace_lease(&self) {
        self.leases.lock().insert((0, 0), Instant::now());
    }
}

// ---------------------------------------------------------------------------
// SIGTERM (logout / shutdown / unregister — G4)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
static QUIESCE_SIGNALED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
fn quiesce_signaled() -> bool {
    QUIESCE_SIGNALED.load(Ordering::Acquire)
}

#[cfg(target_os = "macos")]
extern "C" fn on_sigterm(_sig: libc::c_int) {
    // Async-signal-safe: a single atomic store. The resident loop (300ms) and
    // the build cancel probe observe it; launchd's default grace is 20s and
    // quiesce targets ≤5s.
    QUIESCE_SIGNALED.store(true, Ordering::Release);
}

#[cfg(target_os = "macos")]
fn install_sigterm_handler() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigterm as extern "C" fn(libc::c_int) as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

// ---------------------------------------------------------------------------
// Index build / readiness
// ---------------------------------------------------------------------------

/// Whether the last index pass ran to completion (`index_complete=1`). Used by
/// the supervisory loop to decide whether a failed build needs healing.
#[cfg(target_os = "macos")]
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

/// External cancellation probe for a build pass, polled every
/// `GUI_POLL_INTERVAL` by the pass monitor thread.
type CancelProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// Run one index pass and, when it was a fresh build (or crash recovery), the
/// finalization the `app:None` path skips: secondary indexes, FTS rebuild, GC,
/// and storage maintenance (VACUUM / WAL truncate) — mirroring the GUI's
/// finalizing thread. `indexing_active` is held for the whole pass so a live
/// watcher stands down instead of racing this writer. Returns the pass result
/// so the supervisory loops can count consecutive failures (rebuild-loop
/// detection in the diag log).
///
/// `should_cancel` is polled while the (possibly minutes-long) pass runs; when
/// it turns true, `cancel_index` makes the pass unwind within ~1-2s (index
/// left incomplete — the heal loop or the next start finishes it). Callers use
/// it for the GUI beacon (pre-flip) and for quiesce/restart/reset requests.
fn build_index(db_path: &Path, state: &AppState, should_cancel: CancelProbe) -> Result<(), String> {
    // Clear any cancel signal a previously-cancelled pass may have left set.
    state.cancel_index.store(false, Ordering::Release);
    state.indexing_active.store(true, Ordering::Release);
    let (is_fresh, fts_dirty) = fresh_and_dirty(db_path);

    let monitor_stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let cancel = Arc::clone(&state.cancel_index);
        let stop = Arc::clone(&monitor_stop);
        let should_cancel = Arc::clone(&should_cancel);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if should_cancel() {
                    cancel.store(true, Ordering::Release);
                    return;
                }
                std::thread::sleep(GUI_POLL_INTERVAL);
            }
        })
    };

    let result = run_incremental_index(None, state);

    monitor_stop.store(true, Ordering::Release);
    let _ = monitor.join();
    // Ensure the guard is cleared even on error (the Ok path also clears it).
    state.indexing_active.store(false, Ordering::Release);
    match &result {
        Ok(()) => {
            if is_fresh || fts_dirty {
                finalize_fresh_index(state);
            }
            if let Err(e) = ensure_db_indexes(db_path) {
                eprintln!("[daemon] ensure_db_indexes failed: {e}");
            }
            // Storage/GC maintenance only after a full (re)build, matching the
            // GUI finalizing thread; a plain catchup barely changes the file.
            // macOS (D7): the post-flip GUI is a reader — VACUUM proceeds and
            // concurrent readers ride busy_timeout + the S2 retry. A pre-flip
            // *writer* GUI (§7) still blocks it, same as Linux: VACUUM's
            // *exclusive* lock (not WAL-concurrent) would hand that GUI a
            // hard SQLITE_BUSY.
            #[cfg(target_os = "macos")]
            let gui_blocks_maintenance = writer_gui_running(db_path);
            #[cfg(not(target_os = "macos"))]
            let gui_blocks_maintenance = gui_is_running(db_path);
            if (is_fresh || fts_dirty) && !gui_blocks_maintenance {
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
    result
}

/// Wrap a build pass in the Building/Ready lifecycle and record whether it
/// was a fresh pass (for the bridge's catchup labeling).
#[cfg(target_os = "macos")]
fn run_build(db_path: &Path, state: &AppState, control: &Arc<DaemonControl>) -> Result<(), String> {
    control.set_lifecycle(Lifecycle::Building);
    let (is_fresh, _) = fresh_and_dirty(db_path);
    control.is_fresh.store(is_fresh, Ordering::Release);
    let result = build_index(db_path, state, build_cancel_probe(db_path, control));
    control.set_lifecycle(Lifecycle::Ready);
    result
}

/// A pass must unwind when SIGTERM arrived, when IPC asked for exit/reset, or
/// when a pre-flip *writer* GUI appeared (§7 — post-flip GUIs are readers and
/// never cancel anything, D4).
#[cfg(target_os = "macos")]
fn build_cancel_probe(db_path: &Path, control: &Arc<DaemonControl>) -> CancelProbe {
    let control = Arc::clone(control);
    let db_path = db_path.to_path_buf();
    Arc::new(move || {
        quiesce_signaled()
            || control.requested_exit().is_some()
            || control.reset_requested.load(Ordering::Acquire)
            || writer_gui_running(&db_path)
    })
}

// ---------------------------------------------------------------------------
// Detached spawn + revival gate
// ---------------------------------------------------------------------------

/// Spawn `everything --daemon` as a detached background process that outlives
/// the caller (Linux legacy bootstrap — macOS goes through
/// `ensure_daemon_running`, and on Windows the SCM-managed service is the
/// guaranteed writer, §3.4). Skips when the GUI (writer owner) or another
/// daemon is already running, so repeated MCP sessions don't churn
/// short-lived processes.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn spawn_detached() {
    let db_path = default_db_path();
    if gui_is_running(&db_path) {
        eprintln!("[daemon] GUI owns index writes; not spawning a daemon");
        diag::log("bootstrap: GUI owns index writes; not spawning a daemon");
        return;
    }
    spawn_detached_with(&[], "bootstrap", true);
}

/// The one true spawn: every daemon-spawning path funnels through here (§4).
/// `skip_if_resident=false` is for self-relaunch, where the exiting process
/// still holds `daemon.lock` at spawn time.
#[cfg(not(target_os = "windows"))]
fn spawn_detached_with(extra_args: &[&str], reason: &str, skip_if_resident: bool) {
    let db_path = default_db_path();
    if skip_if_resident && daemon_running(&db_path) {
        diag::log(format!("{reason}: daemon already resident"));
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[daemon] spawn skipped: current_exe failed: {e}");
            diag::log(format!("daemon spawn skipped: current_exe failed: {e}"));
            return;
        }
    };
    // The detached daemon has no terminal, so its stderr (every existing
    // eprintln! diagnostic) goes to the shared diag log instead of /dev/null —
    // field incidents (rebuild loops, failed passes) stay reconstructible.
    let stderr = db_path
        .parent()
        .and_then(diag::append_handle)
        .map(Stdio::from)
        .unwrap_or_else(Stdio::null);
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
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
        Ok(_) => {
            eprintln!("[daemon] spawned detached indexer daemon ({reason})");
            diag::log(format!("spawned detached indexer daemon ({reason})"));
        }
        Err(e) => {
            eprintln!("[daemon] spawn failed: {e}");
            diag::log(format!("daemon spawn FAILED: {e}"));
        }
    }
}

/// How a caller that needs a live daemon should revive one (design §4 single
/// rule): a registered agent is kickstarted so launchd stays the supervisor
/// (G2); opt-out / OS-disabled users only ever get a lease daemon (G1); a
/// plain spawn is reserved for unregistered contexts (dev builds,
/// translocation, pre-registration installs).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevivalAction {
    Kickstart,
    SpawnDetached,
    SpawnLease,
    Nothing,
}

#[cfg(target_os = "macos")]
pub(crate) fn revival_action(
    reg: crate::mac::launch_agent::RegState,
    opted_out: bool,
    daemon_alive: bool,
) -> RevivalAction {
    use crate::mac::launch_agent::RegState;
    if daemon_alive {
        return RevivalAction::Nothing;
    }
    if opted_out || reg == RegState::RequiresApproval {
        return RevivalAction::SpawnLease;
    }
    match reg {
        RegState::Enabled => RevivalAction::Kickstart,
        // Unsupported (dev/translocated) / NotRegistered / NotFound.
        _ => RevivalAction::SpawnDetached,
    }
}

/// The single revival gate every "daemon must be running" path calls
/// (GUI bridge self-heal, MCP bootstrap, hot-WAL recovery, toggle-ON).
#[cfg(target_os = "macos")]
#[allow(dead_code)] // wired up by the P2.2 bridge and P2.3 MCP bootstrap
pub(crate) fn ensure_daemon_running(reason: &str) {
    let db_path = default_db_path();
    let action = revival_action(
        crate::mac::launch_agent::agent_status(),
        crate::mac::prefs::opted_out(),
        daemon_running(&db_path),
    );
    match action {
        RevivalAction::Nothing => {}
        RevivalAction::Kickstart => match crate::mac::launch_agent::kickstart() {
            Ok(()) => diag::log(format!("revival ({reason}): launchctl kickstart")),
            Err(e) => diag::log(format!("revival ({reason}): kickstart failed: {e}")),
        },
        RevivalAction::SpawnDetached => spawn_detached_with(&[], reason, true),
        RevivalAction::SpawnLease => spawn_detached_with(&["--lease"], reason, true),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[allow(dead_code)]
pub(crate) fn ensure_daemon_running(_reason: &str) {
    // No launchd equivalent here; the legacy spawn gate applies.
    spawn_detached();
}

// ---------------------------------------------------------------------------
// Resident daemon
// ---------------------------------------------------------------------------

/// Entry point for `everything --daemon`. The process exit code is the
/// launchd supervision contract on macOS (§4): 0 = dormant, non-zero =
/// relaunch (KeepAlive.SuccessfulExit=false).
pub fn run_daemon() {
    std::process::exit(run_daemon_impl());
}

fn run_daemon_impl() -> i32 {
    let db_path = default_db_path();
    let home_dir = resolve_home_dir();
    let app_data_dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Structured post-mortem trail alongside raw stderr (which spawn_detached
    // already points at the same file): timestamps + pass boundaries + ALERTs.
    diag::init(&app_data_dir, "daemon");
    eprintln!(
        "[daemon] everything indexer daemon v{} db={}{}",
        env!("CARGO_PKG_VERSION"),
        db_path.display(),
        // launchd-started daemons carry this from the agent plist; a missing
        // marker means a spawn_detached/manual start.
        if std::env::var_os("EVERYTHING_LAUNCHD").is_some() {
            " (launchd)"
        } else {
            ""
        }
    );

    #[cfg(target_os = "macos")]
    {
        run_daemon_macos(db_path, home_dir, app_data_dir)
    }

    #[cfg(not(target_os = "macos"))]
    {
        // Defer to the GUI, which owns all writes while it runs.
        if gui_is_running(&db_path) {
            eprintln!("[daemon] GUI is running; it owns index writes — exiting");
            diag::log("GUI is running; exiting (it owns index writes)");
            return 0;
        }
        let Some(_lock) = try_acquire_daemon_lock(&db_path) else {
            eprintln!("[daemon] another daemon already running; exiting");
            diag::log("another daemon already resident; exiting");
            return 0;
        };

        if let Err(e) = fs::create_dir_all(&app_data_dir) {
            eprintln!("[daemon] create data dir failed: {e}");
            diag::log(format!("FATAL: create data dir failed: {e}"));
            return 1;
        }
        if let Err(e) = init_db_tables(&db_path) {
            eprintln!("[daemon] init_db_tables failed: {e}");
            diag::log(format!("FATAL: init_db_tables failed: {e}"));
            return 1;
        }

        let state = build_app_state(db_path.clone(), home_dir, &app_data_dir);

        // Startup reconcile: a fresh build if the DB is empty, otherwise a catchup
        // covering everything changed while no daemon was running. A failure is
        // not fatal — the supervisory loop below heals it (and logs the loop).
        let cancel_probe: CancelProbe = {
            let db_path = db_path.clone();
            Arc::new(move || gui_is_running(&db_path))
        };
        let _ = build_index(&db_path, &state, cancel_probe);
        if gui_is_running(&db_path) {
            eprintln!("[daemon] GUI appeared during startup build; yielding writes");
        } else {
            eprintln!("[daemon] index ready; entering resident mode");
        }
        diag::log("entering resident mode");

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        run_polling_resident(db_path, state);
        // Windows: the SCM-managed indexer service is the sole writer (Q3). A stray
        // `--daemon` already self-exited above when it couldn't take daemon.lock, and
        // the ProgramData data dir is read-only to a non-SYSTEM process, so there is
        // no Windows resident fallback.
        #[cfg(target_os = "windows")]
        let _ = (db_path, state);
        0
    }
}

/// macOS §4 lifecycle: Starting → (WaitingOnboarding) → Building → Ready →
/// Quiescing, with the UDS server and SIGTERM handler installed before the
/// first build.
#[cfg(target_os = "macos")]
fn run_daemon_macos(db_path: PathBuf, home_dir: PathBuf, app_data_dir: PathBuf) -> i32 {
    use crate::mac::{launch_agent, prefs};

    // §4-1 — daemon.lock. A duplicate is dormant, not an error. Brief retry:
    // a self-relaunch (exe replaced outside launchd) spawns while its parent
    // still holds the lock for a few hundred ms.
    let lock_deadline = Instant::now() + Duration::from_secs(3);
    let _lock = loop {
        if let Some(lock) = try_acquire_daemon_lock(&db_path) {
            break lock;
        }
        if Instant::now() >= lock_deadline {
            eprintln!("[daemon] another daemon already resident; dormant exit(0)");
            diag::log("duplicate daemon; dormant exit(0)");
            return 0;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if let Err(e) = fs::create_dir_all(&app_data_dir) {
        eprintln!("[daemon] create data dir failed: {e}");
        diag::log(format!("FATAL: create data dir failed: {e}"));
        return 1; // launchd relaunches (10s throttle)
    }

    let lease_mode = std::env::args().skip(1).any(|a| a == "--lease");
    let launchd_started = std::env::var_os("EVERYTHING_LAUNCHD").is_some();

    // Field CPU accounting (see mac/cpu_monitor.rs): this process owns every
    // index build, so if a fan complaint ever comes back, its diag log must
    // already contain who burned what, on which cluster, at how many watts.
    crate::mac::cpu_monitor::start_watchdog();

    // §4-2 — SIGTERM handler + UDS server BEFORE any build (G4/F6): logout and
    // shutdown quiesce cleanly, and status/restart/disable answer even during
    // a minutes-long fresh build.
    install_sigterm_handler();
    let control = Arc::new(DaemonControl::new(lease_mode));
    if lease_mode {
        control.seed_grace_lease();
    }
    let state = build_app_state(db_path.clone(), home_dir, &app_data_dir);
    match crate::daemon_ipc::start_server(
        crate::daemon_ipc::IpcContext {
            state: state.clone(),
            control: Arc::clone(&control),
        },
        &app_data_dir,
    ) {
        Ok(path) => {
            eprintln!("[daemon] control socket at {}", path.display());
            *control.socket_path.lock() = Some(path);
        }
        Err(e) => diag::log(format!(
            "ALERT: daemon IPC server failed to start: {e} — this run has no status/control channel"
        )),
    }

    // §4-3 — opt-out / OS-OFF gate (G1). launchd may legitimately start us
    // right after the user turned auto-start off (KeepAlive races); the
    // launchd instance self-selects dormancy. Manual and lease starts pass.
    if launchd_started && !lease_mode {
        let os_off = launch_agent::agent_status() == launch_agent::RegState::RequiresApproval;
        if prefs::opted_out() || os_off {
            eprintln!("[daemon] auto-start is off (opt-out or Login Items); dormant exit(0)");
            diag::log("auto-start off (app opt-out or OS setting); dormant exit(0)");
            return 0;
        }
    }

    // §4-4 — onboarding gate (F3/G8): no build, no TCC access until the user
    // decided. Existing installs adopt implicitly (§7 migration).
    match wait_for_onboarding_gate(&db_path, &control) {
        GateOutcome::Proceed => {}
        GateOutcome::Exit(code) => {
            let code = finish_quiesce(&db_path, &state, &control, code);
            if code != 0 && !launchd_started {
                // An IPC restart while gated must still relaunch when there is
                // no launchd supervisor — same as the resident loop (G15).
                relaunch_self_detached(lease_mode);
            }
            return code;
        }
    }

    // §4-5 — schema + startup build (fresh or catchup).
    if let Err(e) = init_db_tables(&db_path) {
        eprintln!("[daemon] init_db_tables failed: {e}");
        diag::log(format!("FATAL: init_db_tables failed: {e}"));
        return 1;
    }
    let fda = crate::mac::probe_full_disk_access();
    control.fda_ok.store(fda, Ordering::Release);
    control.degraded.store(!fda, Ordering::Release);
    if let Ok(conn) = db_connection(&db_path) {
        let _ = set_meta(&conn, "last_fda_ok", if fda { "1" } else { "0" });
    }
    let _ = run_build(&db_path, &state, &control);
    // SIGTERM / an IPC exit during the startup build unwound it via the cancel
    // probe; skip the reconcile/purge housekeeping too so quiesce stays ≤5s
    // (G4) — the resident loop's first iteration performs the actual quiesce.
    if !quiesce_signaled() && control.requested_exit().is_none() {
        reconcile_extra_roots(&state, &control);
        // Housekeeping the GUI used to gate on frontend_ready (§5.3): rows
        // under since-ignored roots are purged by the writer that owns the DB.
        let (ignored_roots, _) = cached_effective_ignore_rules(&state);
        if let Err(e) = purge_ignored_entries(&db_path, &ignored_roots) {
            eprintln!("[daemon] purge_ignored_entries failed: {e}");
        }
    }

    eprintln!("[daemon] index ready; entering resident mode");
    diag::log("entering resident mode");
    run_macos_resident(db_path, state, control, launchd_started)
}

#[cfg(target_os = "macos")]
enum GateOutcome {
    Proceed,
    Exit(i32),
}

/// §7 migration: whether an onboarding decision is on record, adopting
/// `granted-implicit` first when a usable index already exists on disk (an
/// install that predates onboarding must never stall or see the modal).
/// Opens the DB read-only, so the GUI's `get_onboarding_state` calls it too —
/// on a first post-upgrade launch no daemon may be running yet, and waiting
/// for the daemon's gate to adopt would flash the modal at an existing user.
#[cfg(target_os = "macos")]
pub(crate) fn onboarding_recorded_or_adopt(db_path: &Path) -> bool {
    use crate::mac::prefs;
    if prefs::onboarding_recorded() {
        return true;
    }
    if db_path.exists() {
        if let Ok(conn) = crate::db_connection_for_search(db_path) {
            let complete = get_meta(&conn, "index_complete").as_deref() == Some("1");
            let nonempty = conn
                .query_row("SELECT EXISTS(SELECT 1 FROM entries)", [], |r| {
                    r.get::<_, bool>(0)
                })
                .unwrap_or(false);
            if complete || nonempty {
                prefs::record_onboarding_decision("granted-implicit");
                diag::log("onboarding: existing index adopted (granted-implicit, §7)");
                return true;
            }
        }
    }
    false
}

/// First-install onboarding gate (D6/F3): hold in `WaitingOnboarding` until a
/// decision is recorded. Existing installs (a usable index already on disk)
/// adopt `granted-implicit` so upgrades never stall (§7). The Q7 escape hatch
/// lets MCP-only users out after 7 days of observed search activity.
#[cfg(target_os = "macos")]
fn wait_for_onboarding_gate(db_path: &Path, control: &Arc<DaemonControl>) -> GateOutcome {
    use crate::mac::prefs;
    if onboarding_recorded_or_adopt(db_path) {
        return GateOutcome::Proceed;
    }
    prefs::mark_first_seen_now();
    control.set_lifecycle(Lifecycle::WaitingOnboarding);
    eprintln!("[daemon] first install: waiting for the onboarding decision");
    diag::log("first install: WaitingOnboarding (no build, no TCC access)");
    loop {
        if quiesce_signaled() {
            return GateOutcome::Exit(0);
        }
        if let Some(code) = control.requested_exit() {
            return GateOutcome::Exit(code);
        }
        if prefs::onboarding_recorded() {
            diag::log("onboarding decision recorded; proceeding");
            return GateOutcome::Proceed;
        }
        // Q7 escape hatch: gated ≥7 days + MCP search activity within 24h →
        // treat as "skipped" and build degraded, so an MCP-only user is never
        // stuck with an empty index (G8).
        if let Some(first_seen) = prefs::first_seen_epoch() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if now.saturating_sub(first_seen) >= 7 * 24 * 3600
                && mcp_activity_recent(db_path, Duration::from_secs(24 * 3600))
            {
                prefs::record_onboarding_decision("skipped");
                diag::log("onboarding gate: 7d elapsed + MCP activity → proceeding degraded (Q7)");
                return GateOutcome::Proceed;
            }
        }
        std::thread::sleep(SUPERVISE_INTERVAL);
    }
}

/// Whether the MCP server observed search activity within `window` (it
/// touches `<data dir>/mcp.activity`, throttled).
#[cfg(target_os = "macos")]
fn mcp_activity_recent(db_path: &Path, window: Duration) -> bool {
    db_path
        .parent()
        .map(|dir| dir.join("mcp.activity"))
        .and_then(|p| fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|age| age <= window)
        .unwrap_or(false)
}

/// Extra roots added/removed in `.pathindexing` while no writer was running
/// (the GUI used to reconcile this at startup — §5.3). Compares the stored
/// `indexed_extra_roots` meta against the roots loaded at state build.
///
/// The added-root scan can take minutes on a large tree, so it runs under the
/// same cancel monitor as a build pass: SIGTERM / IPC exit / reset flips
/// `cancel_index` and the scan unwinds within ~1-2s, keeping the ≤5s quiesce
/// contract (G4). A cancelled reconcile leaves `indexed_extra_roots` stale,
/// so the next daemon start simply retries it.
#[cfg(target_os = "macos")]
fn reconcile_extra_roots(state: &AppState, control: &Arc<DaemonControl>) {
    let stored_roots: Vec<PathBuf> = db_connection(&state.db_path)
        .ok()
        .and_then(|c| get_meta(&c, "indexed_extra_roots"))
        .map(|v| v.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default();
    let current_roots = state.extra_roots.lock().clone();
    let stored_set: std::collections::HashSet<&PathBuf> = stored_roots.iter().collect();
    let added = current_roots.iter().filter(|r| !stored_set.contains(r)).count();
    let removed = stored_roots.iter().filter(|r| !current_roots.contains(r)).count();
    if added == 0 && removed == 0 {
        return;
    }
    if state
        .pathindexing_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    diag::log(format!(
        ".pathindexing changed while the daemon was down: +{added} -{removed} → extra-root reconcile"
    ));
    let should_cancel = build_cancel_probe(&state.db_path, control);
    while state.indexing_active.load(Ordering::Acquire) {
        if should_cancel() {
            state.pathindexing_active.store(false, Ordering::Release);
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    state.cancel_index.store(false, Ordering::Release);
    let monitor_stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let cancel = Arc::clone(&state.cancel_index);
        let stop = Arc::clone(&monitor_stop);
        let should_cancel = Arc::clone(&should_cancel);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if should_cancel() {
                    cancel.store(true, Ordering::Release);
                    return;
                }
                std::thread::sleep(GUI_POLL_INTERVAL);
            }
        })
    };
    let (ign_roots, ign_patterns) = cached_effective_ignore_rules(state);
    let outcome = pathindexing::handle_pathindexing_change(
        state,
        &stored_roots,
        &current_roots,
        &ign_roots,
        &ign_patterns,
    );
    monitor_stop.store(true, Ordering::Release);
    let _ = monitor.join();
    match outcome {
        Ok(()) => {
            if let Ok(c) = db_connection(&state.db_path) {
                let roots_str: Vec<String> = current_roots
                    .iter()
                    .map(|r| r.to_string_lossy().to_string())
                    .collect();
                let _ = set_meta(&c, "indexed_extra_roots", &roots_str.join("\n"));
            }
            let _ = refresh_and_emit_status_counts(None, state);
        }
        Err(e) => diag::log(format!("extra-root reconcile FAILED: {e}")),
    }
    state.pathindexing_active.store(false, Ordering::Release);
}

/// The shared graceful shutdown (§4): cancel a running pass (≤2s), stop and
/// join the watcher (≤3s), checkpoint the WAL best-effort (G10 — an active
/// reader can hold it off; the next writer's open recovers), and resolve the
/// final exit code (a late IPC disable/restart re-targets it; disable wins).
#[cfg(target_os = "macos")]
fn finish_quiesce(db_path: &Path, state: &AppState, control: &Arc<DaemonControl>, code: i32) -> i32 {
    control.set_lifecycle(Lifecycle::Quiescing);
    state.cancel_index.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    while state.indexing_active.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    state.watcher_stop.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(3);
    while state.watcher_active.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Ok(conn) = db_connection(db_path) {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    let code = control.requested_exit().unwrap_or(code);
    diag::log(format!("quiesce complete; exit({code})"));
    code
}

/// IPC `reset_index`: stop the watcher, swap the tables (shared
/// `reset_index_storage`), rebuild fresh, restart the watcher. The cooldown
/// was already enforced at the IPC layer.
#[cfg(target_os = "macos")]
fn perform_reset(db_path: &Path, state: &AppState, control: &Arc<DaemonControl>) {
    diag::log("IPC reset_index: stop watcher → swap tables → fresh build");
    state.watcher_stop.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.watcher_active.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    match db_connection(db_path).and_then(|conn| reset_index_storage(&conn)) {
        Ok(()) => {
            {
                let mut status = state.status.lock();
                status.entries_count = 0;
                status.scanned = 0;
                status.indexed = 0;
                status.message = None;
            }
            invalidate_search_caches(state);
        }
        Err(e) => diag::log(format!("ALERT: reset_index storage swap failed: {e}")),
    }
    let _ = run_build(db_path, state, control);
    start_watcher(db_path, state);
}

// ---------------------------------------------------------------------------
// Executable identity (update / rollback / deletion — F5, G12)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
struct ExeIdentity {
    path: PathBuf,
    ino: u64,
    mtime: Option<std::time::SystemTime>,
}

#[cfg(target_os = "macos")]
impl ExeIdentity {
    fn capture() -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let path = std::env::current_exe().ok()?;
        let meta = fs::metadata(&path).ok()?;
        Some(Self {
            path,
            ino: meta.ino(),
            mtime: meta.modified().ok(),
        })
    }
}

#[cfg(target_os = "macos")]
enum ExeCheck {
    Unchanged,
    Replaced,
    Missing,
}

#[cfg(target_os = "macos")]
fn exe_identity_check(identity: &Option<ExeIdentity>, missing_ticks: &mut u32) -> ExeCheck {
    let Some(identity) = identity else {
        return ExeCheck::Unchanged;
    };
    match fs::metadata(&identity.path) {
        Ok(meta) => {
            use std::os::unix::fs::MetadataExt;
            // A path that vanished and reappeared different is a replacement
            // (Finder's delete-then-copy update) — reclassified here (G12).
            if meta.ino() != identity.ino || meta.modified().ok() != identity.mtime {
                return ExeCheck::Replaced;
            }
            *missing_ticks = 0;
            ExeCheck::Unchanged
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            *missing_ticks += 1;
            if *missing_ticks >= EXE_MISSING_TICKS {
                ExeCheck::Missing
            } else {
                ExeCheck::Unchanged
            }
        }
        Err(_) => ExeCheck::Unchanged,
    }
}

/// Re-exec ourselves detached — the restart path for daemons launchd does not
/// supervise (spawned/lease/manual — G15). The exiting process still holds
/// `daemon.lock`, so the child skips the resident check and briefly retries
/// the lock instead.
#[cfg(target_os = "macos")]
fn relaunch_self_detached(lease_mode: bool) {
    if lease_mode {
        spawn_detached_with(&["--lease"], "self-relaunch", false);
    } else {
        spawn_detached_with(&[], "self-relaunch", false);
    }
}

// ---------------------------------------------------------------------------
// Resident loops
// ---------------------------------------------------------------------------

/// macOS: real-time freshness via the FSEvents watcher, supervised (§4). The
/// GUI beacon is polled on the fast `GUI_POLL_INTERVAL` (transitional yield),
/// as are quiesce/exit/reset requests; heavier supervision runs on the coarse
/// `SUPERVISE_INTERVAL`: heal, watcher restart, FDA rising edge, exe identity,
/// lease liveness. Returns the process exit code.
#[cfg(target_os = "macos")]
fn run_macos_resident(
    db_path: PathBuf,
    state: AppState,
    control: Arc<DaemonControl>,
    launchd_started: bool,
) -> i32 {
    start_watcher(&db_path, &state);
    // `None` => supervise on the first loop iteration, so a failed startup build
    // is healed immediately rather than after one SUPERVISE_INTERVAL.
    let mut last_supervise: Option<Instant> = None;
    // Self-diagnosis: a healthy daemon heals at most once (after a failed
    // startup build), so repeated heals or watcher restarts are the signature
    // of the field pathologies we can't reproduce on demand — a rebuild loop
    // (every heal rescans all roots at full parallelism = sustained multi-core
    // CPU) or a flapping FSEvents worker. Count them and ALERT in the diag log.
    let mut heal_times: VecDeque<Instant> = VecDeque::new();
    let mut consecutive_failed_builds: u32 = 0;
    // Log-once edge for "a pre-flip writer GUI is up, builds deferred" (§7).
    let mut deferred_to_writer_gui = false;
    let mut watcher_restarts = diag::RateAlert::new(Duration::from_secs(3600), 5);
    let exe_identity = ExeIdentity::capture();
    let mut exe_missing_ticks: u32 = 0;
    let mut last_fda_probe = Instant::now();
    loop {
        if quiesce_signaled() {
            diag::log("SIGTERM: quiescing (logout/shutdown/unregister — G4)");
            return finish_quiesce(&db_path, &state, &control, 0);
        }
        if let Some(code) = control.requested_exit() {
            diag::log(format!("IPC exit request: quiescing toward exit({code})"));
            let code = finish_quiesce(&db_path, &state, &control, code);
            if code != 0 && !launchd_started {
                // No launchd supervisor to relaunch us — re-exec (G15).
                relaunch_self_detached(control.lease_mode);
            }
            return code;
        }
        if control.reset_requested.swap(false, Ordering::AcqRel) {
            perform_reset(&db_path, &state, &control);
        }
        if last_supervise.map_or(true, |t| t.elapsed() >= SUPERVISE_INTERVAL) {
            last_supervise = Some(Instant::now());
            // §7: a pre-flip writer GUI owns index writes while it runs; defer
            // builds (heal, pathignore catchup) instead of dueling it. The
            // watcher keeps running — its incremental writes are WAL-safe.
            let writer_gui_up = writer_gui_running(&db_path);
            if writer_gui_up && !deferred_to_writer_gui {
                deferred_to_writer_gui = true;
                diag::log("pre-flip writer GUI detected; deferring builds until it exits (§7)");
            } else if !writer_gui_up && deferred_to_writer_gui {
                deferred_to_writer_gui = false;
                diag::log("pre-flip writer GUI gone; resuming builds");
            }
            // .pathignore edits apply live now that the daemon owns all writes
            // (the GUI's old restart-to-apply path has no writer to act on it):
            // purge newly ignored rows, then a catchup pass collects newly
            // un-ignored paths. The bridge's data_version watch refreshes the
            // GUI's results.
            // (writer_gui_up is checked FIRST so a deferred tick doesn't
            // consume the dirty flag and lose the change.)
            if !writer_gui_up && state.pathignore_dirty.swap(false, Ordering::AcqRel) {
                diag::log(".pathignore changed: purging ignored entries + catchup pass");
                let (ignored_roots, _) = cached_effective_ignore_rules(&state);
                if let Err(e) = purge_ignored_entries(&db_path, &ignored_roots) {
                    diag::log(format!("ALERT: pathignore purge failed: {e}"));
                }
                invalidate_search_caches(&state);
                let _ = refresh_and_emit_status_counts(None, &state);
                let _ = run_build(&db_path, &state, &control);
            }
            // Heal an incomplete/failed build so MCP never stays stuck.
            if !index_complete(&db_path) && !writer_gui_up {
                eprintln!("[daemon] index incomplete; (re)building");
                heal_times.push_back(Instant::now());
                while heal_times
                    .front()
                    .map_or(false, |t| t.elapsed() > Duration::from_secs(3600))
                {
                    heal_times.pop_front();
                }
                diag::log(format!(
                    "supervise heal #{} this hour: index_complete != 1 → full index pass \
                     (consecutive failures so far: {})",
                    heal_times.len(),
                    consecutive_failed_builds
                ));
                if heal_times.len() == 3 {
                    diag::log(
                        "ALERT: 3 supervise heals within an hour — rebuild loop: index_complete \
                         never sticks (persistently failing pass, or another writer clearing it); \
                         every heal rescans all roots at full parallelism",
                    );
                }
                match run_build(&db_path, &state, &control) {
                    Ok(()) => {
                        consecutive_failed_builds = 0;
                        if !index_complete(&db_path) {
                            diag::log(
                                "ALERT: index pass succeeded but index_complete is still != 1 — \
                                 the meta write was lost or clobbered; the next supervise tick \
                                 will rebuild again",
                            );
                        }
                    }
                    Err(_) => {
                        consecutive_failed_builds += 1;
                        if consecutive_failed_builds == 3 {
                            diag::log(format!(
                                "ALERT: {consecutive_failed_builds} consecutive index passes \
                                 failed; daemon keeps retrying every {}s (error details above)",
                                SUPERVISE_INTERVAL.as_secs()
                            ));
                        }
                    }
                }
            }
            // Restart the watcher if it died (e.g. FSEvents init failure left
            // the worker thread dead).
            if !state.watcher_active.load(Ordering::Acquire) {
                eprintln!("[daemon] watcher not active; restarting");
                if let Some(n) = watcher_restarts.record() {
                    diag::log(format!(
                        "ALERT: FSEvents watcher restarted {n} times in the last hour \
                         (stream init failing or the worker thread keeps dying)"
                    ));
                }
                start_watcher(&db_path, &state);
            }
            supervise_fda(&db_path, &state, &control, &mut last_fda_probe);
            match exe_identity_check(&exe_identity, &mut exe_missing_ticks) {
                ExeCheck::Unchanged => {}
                ExeCheck::Replaced => {
                    diag::log(
                        "exe identity changed (update/rollback): quiesce + exit(1) → relaunch \
                         from the on-disk binary (F5)",
                    );
                    let code = finish_quiesce(&db_path, &state, &control, 1);
                    if !launchd_started {
                        relaunch_self_detached(control.lease_mode);
                    }
                    return code;
                }
                ExeCheck::Missing => {
                    diag::log(format!(
                        "exe missing for {EXE_MISSING_TICKS} ticks (app deleted): quiesce + \
                         dormant exit(0) (G12)"
                    ));
                    return finish_quiesce(&db_path, &state, &control, 0);
                }
            }
            if control.lease_mode
                && control.live_lease_count() == 0
                && !gui_is_running(&db_path)
            {
                diag::log("lease expired: no live users and no GUI — exit(0)");
                return finish_quiesce(&db_path, &state, &control, 0);
            }
        }
        std::thread::sleep(GUI_POLL_INTERVAL);
    }
}

/// FDA supervision (D6): while missing, probe every `FDA_PROBE_INTERVAL`; on
/// the rising edge (granted), persist it and force a catchup pass so paths the
/// earlier degraded pass couldn't read get collected.
#[cfg(target_os = "macos")]
fn supervise_fda(
    db_path: &Path,
    state: &AppState,
    control: &Arc<DaemonControl>,
    last_probe: &mut Instant,
) {
    if control.fda_ok.load(Ordering::Acquire) {
        return;
    }
    if last_probe.elapsed() < FDA_PROBE_INTERVAL {
        return;
    }
    *last_probe = Instant::now();
    if !crate::mac::probe_full_disk_access() {
        return;
    }
    control.fda_ok.store(true, Ordering::Release);
    control.degraded.store(false, Ordering::Release);
    if let Ok(conn) = db_connection(db_path) {
        let _ = set_meta(&conn, "last_fda_ok", "1");
    }
    eprintln!("[daemon] Full Disk Access granted; catching up protected paths");
    if rescan_denied_prefixes(db_path, state) {
        return;
    }
    diag::log("FDA rising edge: no recorded denials → full catchup pass (D6)");
    let _ = run_build(db_path, state, control);
}

/// Rescan only the subtrees the last pass could not enumerate (the prefixes it
/// recorded under `denied_prefixes`), for the Full Disk Access rising edge.
///
/// The alternative is what this used to do: a full catchup pass, re-walking all
/// ~1M entries within a minute or two of the startup build finishing. On a first
/// install that doubles the app's CPU window at exactly the moment the user is
/// watching it, and it is almost entirely wasted work — the only paths whose
/// readability changed are the ones that were denied. Those collapse onto a
/// couple of dozen prefixes (`~/Library/Containers`, `~/Library/Mail`, …), so
/// rescanning exactly them costs seconds.
///
/// Returns false when there is nothing recorded to work from (a DB written
/// before the key existed), so the caller can fall back to a full pass.
#[cfg(target_os = "macos")]
fn rescan_denied_prefixes(db_path: &Path, state: &AppState) -> bool {
    let prefixes = crate::load_denied_prefixes(db_path);
    if prefixes.is_empty() {
        return false;
    }
    // Same exclusive-writer guard a full pass takes: the live watcher and any
    // queued subtree rescan stand down on `indexing_active`. Losing the race
    // means a pass is already running and reconciles the same ground, so the
    // work is covered either way — never fall back to a second full pass here.
    if state
        .indexing_active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        diag::log("FDA rising edge: an index pass is already running; it covers the denied paths");
        return true;
    }
    // Nobody is waiting on this, and it is a single-threaded walk: E-cluster.
    // A later `run_build` on this thread re-applies its own class.
    crate::apply_scan_scheduling(crate::ScanRole::IdleWriter);
    diag::log(format!(
        "FDA rising edge: granted → rescanning {} previously-denied prefix(es) (D6)",
        prefixes.len()
    ));
    let started = Instant::now();
    let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(state);
    let mut upserted = 0usize;
    let mut deleted = 0usize;
    let mut failed = 0usize;
    match db_connection(db_path) {
        Ok(mut conn) => {
            for prefix in &prefixes {
                if quiesce_signaled() {
                    break;
                }
                match crate::rescan::rescan_subtree(
                    &mut conn,
                    Path::new(prefix),
                    &ignored_roots,
                    &ignored_patterns,
                ) {
                    Ok((up, del)) => {
                        upserted += up;
                        deleted += del;
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("[daemon] denied-prefix rescan {prefix} failed: {e}");
                    }
                }
            }
            let _ = conn.execute_batch("PRAGMA shrink_memory;");
        }
        Err(e) => {
            diag::log(format!("FDA rising edge: rescan could not open the DB: {e}"));
            failed = prefixes.len();
        }
    }
    state.indexing_active.store(false, Ordering::Release);
    if upserted > 0 || deleted > 0 {
        invalidate_search_caches(state);
    }
    let _ = refresh_and_emit_status_counts(None, state);
    diag::log(format!(
        "denied-prefix rescan done in {:.1}s: upserted={upserted} deleted={deleted} failed={failed}",
        started.elapsed().as_secs_f32()
    ));
    // A prefix that still cannot be read stays recorded for the next attempt;
    // one that now reads clean is dropped by the next full pass's rewrite.
    true
}

/// Start the FSEvents watcher, replaying from the last persisted event id to
/// cover the gap since the daemon last ran (idempotent upserts absorb overlap).
/// Waits briefly for the worker to publish `watcher_active` so a slow FSEvents
/// init isn't mistaken for a dead watcher by the next supervise tick (which
/// would double-start a second watcher).
#[cfg(target_os = "macos")]
fn start_watcher(db_path: &Path, state: &AppState) {
    state.watcher_stop.store(false, Ordering::Release);
    let since = stored_event_id(db_path);
    crate::start_fsevent_watcher_worker(None, state.clone(), since, false);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state.watcher_active.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The last FSEvents id the watcher persisted, if any.
#[cfg(target_os = "macos")]
fn stored_event_id(db_path: &Path) -> Option<u64> {
    db_connection(db_path)
        .ok()
        .and_then(|c| get_meta(&c, "last_event_id"))
        .and_then(|v| v.parse::<u64>().ok())
        // Legacy DBs may hold u64::MAX (the SinceNow sentinel) from the old
        // max-tracker bug; treat it as "no prior id" rather than replaying from
        // "since now" (which would skip everything changed while we were down).
        .filter(|&id| id != u64::MAX)
}

/// Linux fallback: no headless live watcher, so reconcile on `CATCHUP_INTERVAL`.
/// The GUI beacon is still polled on the fast `GUI_POLL_INTERVAL` so the daemon
/// yields promptly (handshake). `build_index` also heals a dirty FTS.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn run_polling_resident(db_path: PathBuf, state: AppState) {
    let mut last_catchup = std::time::Instant::now();
    let mut consecutive_failed_builds: u32 = 0;
    loop {
        if gui_is_running(&db_path) {
            eprintln!("[daemon] GUI now running; exiting");
            diag::log("GUI appeared; yielding index writes and exiting");
            return;
        }
        if last_catchup.elapsed() >= CATCHUP_INTERVAL {
            last_catchup = std::time::Instant::now();
            let cancel_probe: CancelProbe = {
                let db_path = db_path.clone();
                Arc::new(move || gui_is_running(&db_path))
            };
            match build_index(&db_path, &state, cancel_probe) {
                Ok(()) => consecutive_failed_builds = 0,
                Err(_) => {
                    consecutive_failed_builds += 1;
                    if consecutive_failed_builds == 3 {
                        diag::log(format!(
                            "ALERT: {consecutive_failed_builds} consecutive index passes failed; \
                             daemon keeps retrying every {}s (error details above)",
                            CATCHUP_INTERVAL.as_secs()
                        ));
                    }
                }
            }
        }
        std::thread::sleep(GUI_POLL_INTERVAL);
    }
}
