//! Windows indexer service (`everything --service`) — design §3.1, D2, D4.
//!
//! An SCM-managed LocalSystem process that is the single `index.db` writer.
//! Startup reports `START_PENDING` then, after only fast preparation, `RUNNING`
//! (D2: RUNNING means "controllable", NOT "index ready"); the potentially
//! multi-minute indexing runs afterwards in the background and its progress is
//! exposed via the control pipe's `status` command.
//!
//! Every index pass runs through ONE indexer worker (a single request queue) so
//! there is never more than one logical writer inside the process. Before a
//! pass the worker stops the USN watcher and waits for it to flush and exit
//! (the pass↔watcher barrier), then runs a headless MFT scan which rebuilds the
//! index and starts a fresh watcher — so the watcher's direct writes can never
//! race a pass, and its FRN caches are rebuilt after a full rescan.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};

use super::{service_log, service_paths, service_pipe, service_security};
use crate::{
    build_app_state_with_config, db_connection, get_meta, init_db_tables, resolve_home_dir,
    AppState,
};

pub const SERVICE_NAME: &str = "EverythingIndexer";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// Barrier wait: how long the worker waits for the watcher to flush + exit
/// before running a pass anyway (the watcher exits promptly on `watcher_stop`).
const WATCHER_STOP_WAIT: Duration = Duration::from_secs(10);
/// Periodic reconcile fallback cadence — only acts when USN can't keep the
/// index fresh (watcher down or last build incomplete).
const PERIODIC_RECONCILE: Duration = Duration::from_secs(3600);
/// First retry delay after a failed pass; doubles per consecutive failure up to
/// `PERIODIC_RECONCILE` so a transient scan error retries promptly, not hourly.
const FAILURE_RETRY_MIN: Duration = Duration::from_secs(30);
/// Overall budget for draining the worker during a graceful stop. Kept under
/// the SCM's 30s STOP_PENDING window; the worker preempts its in-flight pass
/// via `cancel_index` (S7) and normally exits within a few seconds.
const STOP_DEADLINE: Duration = Duration::from_secs(25);

/// A request for the single indexer worker. All index passes are enqueued here;
/// the worker executes at most one at a time (single-flight) and coalesces a
/// backlog into one pass, with `Reset` dominating.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IndexRequest {
    /// Startup build / crash heal.
    Build,
    /// Full re-index requested via the pipe (`reset_index`).
    Reset,
    /// Reconcile after a config change, on the periodic timer, or on journal
    /// loss — a full rescan reconciles in every case.
    Reconcile,
}

/// SCM entry point: connect to the service dispatcher. Blocks until the service
/// stops. Called from `main()` for `--service`.
pub fn run() {
    if let Err(e) = service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        service_log::event_error(&format!("service_dispatcher::start failed: {e:?}"));
    }
}

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        service_log::event_error(&format!("service exited with error: {e:?}"));
    }
}

fn set_status(
    handle: &ServiceStatusHandle,
    current_state: ServiceState,
    controls_accepted: ServiceControlAccept,
    checkpoint: u32,
    wait_hint: Duration,
) -> windows_service::Result<()> {
    handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state,
        controls_accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint,
        wait_hint,
        process_id: None,
    })
}

fn run_service() -> windows_service::Result<()> {
    let data_dir = match service_paths::data_dir() {
        Some(d) => d,
        None => {
            service_log::event_error("cannot resolve <ProgramData>\\...\\data — refusing to start");
            return Ok(());
        }
    };

    // The SCM control handler runs on its own thread; it signals STOP here.
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let event_handler = move |control: ServiceControl| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // START_PENDING while we do the fast preparation only.
    set_status(
        &status_handle,
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        1,
        Duration::from_secs(15),
    )?;

    let prep = quick_prep(&data_dir);
    let Prepared {
        state,
        index_tx,
        worker_stop,
        worker,
        pipe_stop,
        _daemon_lock,
    } = match prep {
        Ok(p) => p,
        Err(e) => {
            service_log::event_error(&format!("service startup preparation failed: {e}"));
            let _ = set_status(
                &status_handle,
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                0,
                Duration::default(),
            );
            return Ok(());
        }
    };

    // RUNNING = controllable. Indexing continues in the background (D2/D4).
    set_status(
        &status_handle,
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        0,
        Duration::default(),
    )?;
    service_log::file(&data_dir, "service RUNNING; kicking off initial build");
    let _ = index_tx.send(IndexRequest::Build);

    // Block until the SCM asks us to stop.
    let _ = stop_rx.recv();
    service_log::file(&data_dir, "STOP received; shutting down");

    // STOP_PENDING with a generous wait hint; preempt any in-flight pass.
    set_status(
        &status_handle,
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        2,
        Duration::from_secs(30),
    )?;
    graceful_stop(
        &status_handle,
        &state,
        worker,
        &worker_stop,
        &pipe_stop,
        &index_tx,
        &data_dir,
    );

    set_status(
        &status_handle,
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        0,
        Duration::default(),
    )?;
    Ok(())
}

/// Graceful stop (D2 / §3.1(d)): preempt the in-flight pass, wait for the
/// indexer worker and the USN watcher to flush and exit — advancing the SCM
/// STOP_PENDING checkpoint the whole time so we are never killed as
/// unresponsive — then truncate the WAL so a freshly started reader (GUI/MCP)
/// opens a clean DB without `-shm` recovery (D1).
fn graceful_stop(
    status_handle: &ServiceStatusHandle,
    state: &AppState,
    worker: JoinHandle<()>,
    worker_stop: &Arc<AtomicBool>,
    pipe_stop: &Arc<AtomicBool>,
    index_tx: &Sender<IndexRequest>,
    data_dir: &Path,
) {
    // Preempt any in-flight pass and tell the watcher + pipe to exit.
    state.cancel_index.store(true, Ordering::Release);
    state.watcher_stop.store(true, Ordering::Release);
    worker_stop.store(true, Ordering::Release);
    pipe_stop.store(true, Ordering::Release);
    // Wake the worker if it is blocked waiting for a request.
    let _ = index_tx.send(IndexRequest::Reconcile);

    // Join the worker on a side thread so we can keep the SCM checkpoint moving
    // while it drains its current pass (D2: STOP_PENDING with rising checkpoint).
    let joined = Arc::new(AtomicBool::new(false));
    {
        let joined = joined.clone();
        std::thread::spawn(move || {
            let _ = worker.join();
            joined.store(true, Ordering::Release);
        });
    }
    let mut checkpoint = 2u32;
    let deadline = Instant::now() + STOP_DEADLINE;
    while !joined.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(400));
        // Re-assert cancellation: a pass that started exactly at stop time resets
        // cancel_index at its barrier, so keep re-raising it until the worker is
        // gone — the MFT enumeration checks it every DeviceIoControl call (S7).
        state.cancel_index.store(true, Ordering::Release);
        checkpoint = checkpoint.wrapping_add(1);
        let _ = set_status(
            status_handle,
            ServiceState::StopPending,
            ServiceControlAccept::empty(),
            checkpoint,
            Duration::from_secs(10),
        );
    }

    // §3.1(d): the pass starts a fresh watcher; wait for it to flush its last
    // batch and publish its exit (watcher_active=false) before checkpointing.
    let watcher_deadline = Instant::now() + WATCHER_STOP_WAIT;
    while state.watcher_active.load(Ordering::Acquire) && Instant::now() < watcher_deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Truncate the WAL so readers don't inherit a large -wal or need recovery.
    if let Ok(conn) = db_connection(&state.db_path) {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    service_log::file(data_dir, "graceful stop complete (worker joined, WAL checkpointed)");
}

struct Prepared {
    state: AppState,
    index_tx: Sender<IndexRequest>,
    worker_stop: Arc<AtomicBool>,
    worker: JoinHandle<()>,
    pipe_stop: Arc<AtomicBool>,
    /// Held for the service's lifetime — D6 safety net: a stray `--daemon`
    /// finds the lock taken and self-exits, so it can never become a 2nd writer.
    _daemon_lock: Option<std::fs::File>,
}

/// Fast startup work only (D2): validate the data directory (D1), initialize
/// the schema, build state with a separate config dir, self-grant the service
/// DACL, and start the indexer worker + control pipe. No indexing here.
fn quick_prep(data_dir: &Path) -> Result<Prepared, String> {
    // D1: refuse to run against a planted/reparse/wrong-owner data directory.
    service_paths::validate_data_dir(data_dir)?;

    let config_dir = service_paths::config_dir().ok_or("cannot resolve config dir")?;
    let _ = std::fs::create_dir_all(&config_dir);
    let db_path = service_paths::db_path().ok_or("cannot resolve db path")?;

    // The service owns all writes → it initializes the schema.
    init_db_tables(&db_path)?;

    // D6: hold daemon.lock for our lifetime so a stray `--daemon` self-exits.
    let daemon_lock = crate::daemon::try_acquire_daemon_lock(&db_path);
    if daemon_lock.is_none() {
        service_log::file(data_dir, "warning: daemon.lock already held by another process");
    }

    let state = build_app_state_with_config(db_path, resolve_home_dir(), data_dir, &config_dir);

    // D2: idempotently grant INTERACTIVE start/query on the service object so a
    // non-elevated GUI can start us without UAC. Best-effort.
    if let Err(e) = service_security::grant_interactive_service_access(SERVICE_NAME) {
        service_log::file(
            data_dir,
            &format!("service DACL self-grant failed (GUI will need UAC to start): {e}"),
        );
    }

    let worker_stop = Arc::new(AtomicBool::new(false));
    let (index_tx, index_rx) = mpsc::channel::<IndexRequest>();
    let worker = {
        let state = state.clone();
        let stop = worker_stop.clone();
        let data_dir = data_dir.to_path_buf();
        std::thread::spawn(move || indexer_worker(state, index_rx, stop, data_dir))
    };

    // Supervisor (§3.1): turn watcher-raised signals into index passes promptly
    // — a config-sidecar edit or a USN journal loss — instead of waiting for the
    // hourly reconcile timer. Detached; it exits within one tick of `worker_stop`.
    {
        let state = state.clone();
        let stop = worker_stop.clone();
        let index_tx = index_tx.clone();
        std::thread::spawn(move || supervise(state, index_tx, stop));
    }

    let pipe_stop = Arc::new(AtomicBool::new(false));
    service_pipe::start(
        service_pipe::PipeContext {
            state: state.clone(),
            index_tx: index_tx.clone(),
            last_reset: Arc::new(Mutex::new(None)),
        },
        pipe_stop.clone(),
    );

    Ok(Prepared {
        state,
        index_tx,
        worker_stop,
        worker,
        pipe_stop,
        _daemon_lock: daemon_lock,
    })
}

/// The single indexer worker (design §3.1): the only caller of an index pass.
fn indexer_worker(
    state: AppState,
    rx: Receiver<IndexRequest>,
    stop: Arc<AtomicBool>,
    data_dir: PathBuf,
) {
    // `None` = healthy: wait the full periodic-reconcile cadence for the next
    // request. `Some(d)` = a pass just failed: retry after `d`, doubling on each
    // consecutive failure up to the cadence. Without this, a transient scan
    // failure would leave the index empty until the hourly reconcile timer.
    let mut failure_backoff: Option<Duration> = None;
    loop {
        let wait = failure_backoff.unwrap_or(PERIODIC_RECONCILE);
        let req = match rx.recv_timeout(wait) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => IndexRequest::Reconcile,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if stop.load(Ordering::Acquire) {
            break;
        }

        // Coalesce the backlog into a single pass; a queued Reset dominates.
        let mut kind = req;
        while let Ok(more) = rx.try_recv() {
            if more == IndexRequest::Reset {
                kind = IndexRequest::Reset;
            }
        }

        // The periodic reconcile is only a fallback for when USN can't keep the
        // index fresh (watcher down) or a prior build didn't finish.
        if kind == IndexRequest::Reconcile
            && index_complete(&state)
            && state.watcher_active.load(Ordering::Acquire)
        {
            failure_backoff = None;
            continue;
        }

        failure_backoff = if run_pass(&state, &stop, &data_dir) {
            None
        } else {
            Some(
                failure_backoff
                    .map(|d| (d * 2).min(PERIODIC_RECONCILE))
                    .unwrap_or(FAILURE_RETRY_MIN),
            )
        };

        if stop.load(Ordering::Acquire) {
            break;
        }
    }
    service_log::file(&data_dir, "indexer worker exited");
}

/// Lightweight supervisor (§3.1 "supervise"): converts watcher-raised signals
/// into index passes promptly. A `.pathignore`/`.pathindexing` edit (config
/// lives in the Users-writable `config\`) reloads the rules on the next pass
/// (`effective_ignore_rules` re-reads the files) and reconciles the index; a USN
/// journal loss forces a full resync. Both enqueue `Build` — an unconditional
/// pass — because the worker deliberately drops a `Reconcile` when the index is
/// already complete and the watcher is live (that skip is only for the timer).
fn supervise(state: AppState, index_tx: Sender<IndexRequest>, stop: Arc<AtomicBool>) {
    const TICK: Duration = Duration::from_secs(5);
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(TICK);
        if stop.load(Ordering::Acquire) {
            break;
        }
        let config_changed = state.config_dirty.swap(false, Ordering::AcqRel);
        let journal_lost = state.watcher_lost_events.swap(false, Ordering::AcqRel);
        if config_changed || journal_lost {
            let _ = index_tx.send(IndexRequest::Build);
        }
    }
}

/// Run one index pass behind the watcher barrier. Returns `false` if the scan
/// failed, so the worker can retry with backoff instead of waiting for the
/// hourly reconcile.
fn run_pass(state: &AppState, stop: &Arc<AtomicBool>, data_dir: &Path) -> bool {
    // Barrier: stop the watcher and wait for it to flush pending changes and
    // exit, so its direct writes can't race this pass.
    state.watcher_stop.store(true, Ordering::Release);
    let deadline = Instant::now() + WATCHER_STOP_WAIT;
    while state.watcher_active.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Let the fresh watcher that this pass starts (at the end of scan_mft) run.
    state.watcher_stop.store(false, Ordering::Release);
    state.cancel_index.store(false, Ordering::Release);

    service_log::file(data_dir, "index pass: starting headless MFT scan");
    // Headless: no AppHandle and no MemIndex — the service has no searcher (R5).
    match super::mft_indexer::scan_mft(state, None, false) {
        Ok(result) => {
            // scan_mft returns after the synchronous scan; the background
            // upsert+finalize (and fresh-watcher start) continue on their own
            // thread. Wait for single-flight completion unless STOP preempts.
            while state.indexing_active.load(Ordering::Acquire) {
                if stop.load(Ordering::Acquire) {
                    state.cancel_index.store(true, Ordering::Release);
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            service_log::file(
                data_dir,
                &format!(
                    "index pass: complete (scanned={} indexed={} permission_errors={})",
                    result.scanned, result.indexed, result.permission_errors
                ),
            );
            true
        }
        Err(e) => {
            // Clear the guard the GUI path clears in win/mod.rs; the worker
            // retries with backoff (heal) instead of waiting the hourly timer.
            state.indexing_active.store(false, Ordering::Release);
            service_log::file(data_dir, &format!("index pass FAILED: {e}"));
            false
        }
    }
}

fn index_complete(state: &AppState) -> bool {
    db_connection(&state.db_path)
        .ok()
        .and_then(|c| get_meta(&c, "index_complete"))
        .as_deref()
        == Some("1")
}

/// `everything --register-service` (D5): register the *installed* binary as the
/// service and start it, elevated. Refuses a user-writable binary
/// (portable/dev) — registering such an exe as LocalSystem is a
/// privilege-escalation vector. Updates the binPath if the service exists.
pub fn register_service() -> Result<(), String> {
    use std::process::Command;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    if !is_admin_only_install_path(&exe) {
        return Err(format!(
            "refusing to register the service for a user-writable binary ({}). \
             Install Everything with the installer instead.",
            exe.display()
        ));
    }
    let bin = format!("\"{}\" --service", exe.display());

    let create = Command::new("sc")
        .args([
            "create",
            SERVICE_NAME,
            "binPath=",
            &bin,
            "start=",
            "delayed-auto",
            "DisplayName=",
            "Everything Indexer",
        ])
        .output()
        .map_err(|e| format!("failed to run sc create: {e}"))?;
    if !create.status.success() {
        // Likely already exists (upgrade) — update the binPath instead.
        let cfg = Command::new("sc")
            .args([
                "config",
                SERVICE_NAME,
                "binPath=",
                &bin,
                "start=",
                "delayed-auto",
            ])
            .output()
            .map_err(|e| format!("failed to run sc config: {e}"))?;
        if !cfg.status.success() {
            return Err(format!(
                "sc create/config failed (exit={:?})",
                cfg.status.code()
            ));
        }
    }

    // Recovery policy (best-effort) then start.
    let _ = Command::new("sc")
        .args([
            "failure",
            SERVICE_NAME,
            "reset=",
            "86400",
            "actions=",
            "restart/5000/restart/30000/restart/60000",
        ])
        .output();
    let _ = Command::new("sc")
        .args(["failureflag", SERVICE_NAME, "1"])
        .output();

    let start = Command::new("sc")
        .args(["start", SERVICE_NAME])
        .output()
        .map_err(|e| format!("failed to run sc start: {e}"))?;
    if !start.status.success() {
        return Err(format!("sc start failed (exit={:?})", start.status.code()));
    }
    Ok(())
}

/// D5 invariant (proxy): the binary must live under Program Files, which is
/// admin-write-only by default, not a user-writable location. A full path-ACL
/// check is a refinement documented in the runbook.
fn is_admin_only_install_path(exe: &Path) -> bool {
    for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Ok(dir) = std::env::var(var) {
            if !dir.is_empty() && exe.starts_with(&dir) {
                return true;
            }
        }
    }
    false
}
