use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::Connection;
use tauri::{AppHandle, Emitter};

use std::sync::atomic::Ordering as AtomicOrdering;

use crate::{
    db_connection, delete_paths, invalidate_search_caches,
    index_row_from_path_and_metadata, is_recently_touched,
    now_epoch, pathignore_active_entries, refresh_and_emit_status_counts,
    set_meta, should_skip_path, update_status_counts, upsert_rows,
    AppState, WATCH_DEBOUNCE,
};

const STATUS_EMIT_MIN_INTERVAL: Duration = Duration::from_secs(5);
const RENAME_PAIR_TIMEOUT: Duration = Duration::from_millis(500);
const TS_PERSIST_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum FileChange {
    Create(PathBuf),
    Delete(PathBuf),
    Modify(PathBuf),
    Rename { old: PathBuf, new: PathBuf },
}

/// Net effect of collapsing every event for a single path within one batch.
/// Deduping to this before touching the filesystem/DB turns a burst of repeated
/// events on the same path into a single stat + single write.
#[derive(Clone, Copy)]
enum PathAction {
    Upsert,
    Delete,
}

struct RenamePending {
    old_path: PathBuf,
    created_at: Instant,
}

pub fn start(app: Option<AppHandle>, state: AppState) -> Result<(), String> {
    start_with_roots(app, state, vec![PathBuf::from("C:\\")])
}

pub fn start_with_roots(
    app: Option<AppHandle>,
    state: AppState,
    roots: Vec<PathBuf>,
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

    let mut watcher = RecommendedWatcher::new(tx, Config::default())
        .map_err(|e| format!("notify watcher creation failed: {e}"))?;

    let mut watched = 0;
    for root in &roots {
        match watcher.watch(root, RecursiveMode::Recursive) {
            Ok(()) => watched += 1,
            Err(e) => eprintln!("[win/rdcw] skipping {}: {e}", root.display()),
        }
    }
    if watched == 0 {
        return Err("no directories could be watched".to_string());
    }
    eprintln!("[win/rdcw] watcher started on {}/{} root(s)", watched, roots.len());
    state.watcher_active.store(true, AtomicOrdering::Release);

    std::thread::spawn(move || {
        event_loop(app.as_ref(), &state, rx);
        state.watcher_active.store(false, AtomicOrdering::Release);
        // Keep watcher alive until event_loop exits — drop ends the watch
        drop(watcher);
        eprintln!("[win/rdcw] watcher stopped");
    });

    Ok(())
}

fn event_loop(
    app: Option<&AppHandle>,
    state: &AppState,
    rx: mpsc::Receiver<notify::Result<Event>>,
) {
    let mut pending_changes: Vec<FileChange> = Vec::new();
    let mut pending_renames: VecDeque<RenamePending> = VecDeque::new();
    let mut last_flush = Instant::now();
    let mut last_status_emit = Instant::now();
    let mut last_ts_persist = Instant::now();

    // Persistent write connection reused across flushes: opening one per batch
    // dominated the cost of small updates (mirrors the macOS `watcher_conn`).
    // Held for the event loop's lifetime; dropped when the loop exits.
    let mut db_conn: Option<Connection> = None;

    // Track config file changes to emit pathignore_changed when rules change
    let mut last_config_entries = pathignore_active_entries(
        &std::fs::read_to_string(&state.config_file_path).unwrap_or_default(),
    );

    // Fixed poll: sleep, then drain all accumulated events at once.
    // ~1 wake/sec keeps CPU near 0% even with hundreds of events/sec.
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    loop {
        // Sleep — events accumulate in the channel buffer
        std::thread::sleep(POLL_INTERVAL);

        if state.watcher_stop.load(AtomicOrdering::Acquire) {
            eprintln!("[win/rdcw] stop signal received, exiting");
            break;
        }

        // Drain all buffered events
        let mut drained = false;
        while let Ok(result) = rx.try_recv() {
            drained = true;
            match result {
                Ok(ev) => {
                    // Check for config file change before classifying
                    if ev.paths.iter().any(|p| *p == state.config_file_path) {
                        let new_entries = pathignore_active_entries(
                            &std::fs::read_to_string(&state.config_file_path).unwrap_or_default(),
                        );
                        if new_entries != last_config_entries {
                            last_config_entries = new_entries;
                            if let Some(app) = app {
                                app.emit("pathignore_changed", ()).ok();
                            }
                        }
                    }
                    classify_event(ev, &mut pending_changes, &mut pending_renames);
                }
                Err(e) => {
                    // A watch error (notably RDCW buffer overflow dropping
                    // notifications) means changes may have been missed. Flag a
                    // reconcile so the daemon repairs the drift; harmless in the
                    // GUI, which reconciles on its own startup.
                    eprintln!("[win/rdcw] watch error (flagging reconcile): {e}");
                    state
                        .watcher_lost_events
                        .store(true, AtomicOrdering::Release);
                }
            }
        }

        if !drained && pending_changes.is_empty() && pending_renames.is_empty() {
            continue;
        }

        cleanup_expired_renames(&mut pending_renames, &mut pending_changes);

        if !pending_changes.is_empty() && last_flush.elapsed() >= WATCH_DEBOUNCE {
            apply_changes(app, state, &mut pending_changes, &mut last_status_emit, &mut db_conn);
            last_flush = Instant::now();
        }

        // Periodically persist last active timestamp for startup catchup
        if last_ts_persist.elapsed() >= TS_PERSIST_INTERVAL {
            if let Ok(conn) = db_connection(&state.db_path) {
                let _ = set_meta(&conn, "win_last_active_ts", &now_epoch().to_string());
            }
            last_ts_persist = Instant::now();
        }
    }
}

fn classify_event(
    event: Event,
    pending_changes: &mut Vec<FileChange>,
    pending_renames: &mut VecDeque<RenamePending>,
) {
    match event.kind {
        EventKind::Create(_) => {
            for path in event.paths {
                pending_changes.push(FileChange::Create(path));
            }
        }
        EventKind::Remove(_) => {
            for path in event.paths {
                pending_changes.push(FileChange::Delete(path));
            }
        }
        EventKind::Modify(modify_kind) => {
            use notify::event::ModifyKind;
            match modify_kind {
                ModifyKind::Name(rename_mode) => {
                    use notify::event::RenameMode;
                    match rename_mode {
                        RenameMode::From => {
                            if let Some(path) = event.paths.into_iter().next() {
                                cleanup_expired_renames(pending_renames, pending_changes);
                                pending_renames.push_back(RenamePending {
                                    old_path: path,
                                    created_at: Instant::now(),
                                });
                            }
                        }
                        RenameMode::To => {
                            if let Some(new_path) = event.paths.into_iter().next() {
                                // Pair with the oldest pending From (FIFO order)
                                if let Some(old) = pending_renames.pop_front() {
                                    pending_changes.push(FileChange::Rename {
                                        old: old.old_path,
                                        new: new_path,
                                    });
                                } else {
                                    pending_changes.push(FileChange::Create(new_path));
                                }
                            }
                        }
                        RenameMode::Both => {
                            let mut paths = event.paths.into_iter();
                            if let (Some(old), Some(new)) = (paths.next(), paths.next()) {
                                pending_changes.push(FileChange::Rename { old, new });
                            }
                        }
                        _ => {
                            // RenameMode::Any or other — treat as modify
                            for path in event.paths {
                                pending_changes.push(FileChange::Modify(path));
                            }
                        }
                    }
                }
                _ => {
                    // Data, Metadata, Any, Other — all treated as modify.
                    // Windows RDCW typically sends ModifyKind::Any.
                    for path in event.paths {
                        pending_changes.push(FileChange::Modify(path));
                    }
                }
            }
        }
        _ => {}
    }
}

fn cleanup_expired_renames(
    pending_renames: &mut VecDeque<RenamePending>,
    pending_changes: &mut Vec<FileChange>,
) {
    // VecDeque is ordered by insertion time, so expired entries are always at the front.
    while let Some(front) = pending_renames.front() {
        if front.created_at.elapsed() >= RENAME_PAIR_TIMEOUT {
            let old = pending_renames.pop_front().unwrap();
            pending_changes.push(FileChange::Delete(old.old_path));
        } else {
            break;
        }
    }
}

fn is_under_scan_root(path: &Path, scan_root: &Path) -> bool {
    path.starts_with(scan_root)
}

fn apply_changes(
    app: Option<&AppHandle>,
    state: &AppState,
    changes: &mut Vec<FileChange>,
    last_status_emit: &mut Instant,
    conn_slot: &mut Option<Connection>,
) {
    if changes.is_empty() {
        return;
    }

    // Stand down while an index pass owns the writer (daemon reconcile / GUI
    // full index): the pass reconciles the same ground. Leave events queued so
    // they apply once the pass releases the writer.
    if state.indexing_active.load(AtomicOrdering::Acquire) {
        return;
    }

    let scan_root = &state.scan_root;

    // Collapse every event for a path down to its net action (last event wins).
    // A single churny file (log, cache DB) can emit hundreds of Modify events
    // per second; deduping first turns that into one stat + one write instead
    // of hundreds, which is the dominant cost of each flush. Last-wins also
    // makes a delete-then-recreate within one batch resolve to the final state
    // (the previous per-event path always ran every upsert before every delete,
    // so a recreated path could be wrongly dropped).
    let mut actions: HashMap<PathBuf, PathAction> = HashMap::new();
    for change in changes.drain(..) {
        match change {
            FileChange::Create(path) | FileChange::Modify(path) => {
                actions.insert(path, PathAction::Upsert);
            }
            FileChange::Delete(path) => {
                actions.insert(path, PathAction::Delete);
            }
            FileChange::Rename { old, new } => {
                actions.insert(old, PathAction::Delete);
                actions.insert(new, PathAction::Upsert);
            }
        }
    }

    let mut to_upsert = Vec::new();
    let mut to_delete = Vec::new();

    for (path, action) in actions {
        if !is_under_scan_root(&path, scan_root) {
            continue;
        }
        let path_str = path.to_string_lossy().to_string();
        if is_recently_touched(state, &path_str) {
            continue;
        }
        match action {
            PathAction::Delete => to_delete.push(path_str),
            PathAction::Upsert => {
                if should_skip_path(
                    &path,
                    &state.path_ignores,
                    &state.path_ignore_patterns,
                ) {
                    continue;
                }
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => {
                        if let Some(row) = index_row_from_path_and_metadata(&path, &metadata) {
                            to_upsert.push(row);
                        }
                    }
                    Err(_) => to_delete.push(path_str),
                }
            }
        }
    }

    if to_upsert.is_empty() && to_delete.is_empty() {
        return;
    }

    // Reuse the persistent write connection; reopen it on the next flush if a
    // DB op fails (mirrors the macOS watcher's connection handling).
    if conn_slot.is_none() {
        match db_connection(&state.db_path) {
            Ok(conn) => *conn_slot = Some(conn),
            Err(e) => {
                eprintln!("[win/rdcw] DB open error: {e}");
                return;
            }
        }
    }
    let conn = conn_slot.as_mut().expect("watcher connection present");

    let mut changed = 0;
    let mut failed = false;
    match upsert_rows(conn, &to_upsert) {
        Ok(n) => changed += n,
        Err(e) => {
            eprintln!("[win/rdcw] upsert error: {e}");
            failed = true;
        }
    }
    if !failed {
        match delete_paths(conn, &to_delete) {
            Ok(n) => changed += n,
            Err(e) => {
                eprintln!("[win/rdcw] delete error: {e}");
                failed = true;
            }
        }
    }
    if failed {
        // Drop the connection so the next flush reopens cleanly.
        *conn_slot = None;
    }

    if changed > 0 {
        invalidate_search_caches(state);
        let _ = update_status_counts(state);

        if last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
            let _ = refresh_and_emit_status_counts(app, state);
            *last_status_emit = Instant::now();
        }
    }
}
