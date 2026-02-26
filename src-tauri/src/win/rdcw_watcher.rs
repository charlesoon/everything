use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
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

struct RenamePending {
    old_path: PathBuf,
    created_at: Instant,
}

pub fn start(app: AppHandle, state: AppState) -> Result<(), String> {
    start_with_roots(app, state, vec![PathBuf::from("C:\\")])
}

pub fn start_with_roots(
    app: AppHandle,
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
        event_loop(&app, &state, rx);
        state.watcher_active.store(false, AtomicOrdering::Release);
        // Keep watcher alive until event_loop exits — drop ends the watch
        drop(watcher);
        eprintln!("[win/rdcw] watcher stopped");
    });

    Ok(())
}

fn event_loop(
    app: &AppHandle,
    state: &AppState,
    rx: mpsc::Receiver<notify::Result<Event>>,
) {
    let mut pending_changes: Vec<FileChange> = Vec::new();
    let mut pending_renames: VecDeque<RenamePending> = VecDeque::new();
    let mut last_flush = Instant::now();
    let mut last_status_emit = Instant::now();
    let mut last_ts_persist = Instant::now();

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
            if let Ok(ev) = result {
                // Check for config file change before classifying
                if ev.paths.iter().any(|p| *p == state.config_file_path) {
                    let new_entries = pathignore_active_entries(
                        &std::fs::read_to_string(&state.config_file_path).unwrap_or_default(),
                    );
                    if new_entries != last_config_entries {
                        last_config_entries = new_entries;
                        app.emit("pathignore_changed", ()).ok();
                    }
                }
                classify_event(ev, &mut pending_changes, &mut pending_renames);
            }
        }

        if !drained && pending_changes.is_empty() && pending_renames.is_empty() {
            continue;
        }

        cleanup_expired_renames(&mut pending_renames, &mut pending_changes);

        if !pending_changes.is_empty() && last_flush.elapsed() >= WATCH_DEBOUNCE {
            apply_changes(app, state, &mut pending_changes, &mut last_status_emit);
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
    app: &AppHandle,
    state: &AppState,
    changes: &mut Vec<FileChange>,
    last_status_emit: &mut Instant,
) {
    if changes.is_empty() {
        return;
    }

    let scan_root = &state.scan_root;
    let batch: Vec<FileChange> = changes.drain(..).collect();

    let mut to_upsert = Vec::new();
    let mut to_delete = Vec::new();

    for change in batch {
        match change {
            FileChange::Create(path) | FileChange::Modify(path) => {
                if !is_under_scan_root(&path, scan_root) {
                    continue;
                }
                let path_str = path.to_string_lossy().to_string();
                if is_recently_touched(state, &path_str) {
                    continue;
                }
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
                    Err(_) => {
                        to_delete.push(path_str);
                    }
                }
            }
            FileChange::Delete(path) => {
                if !is_under_scan_root(&path, scan_root) {
                    continue;
                }
                let path_str = path.to_string_lossy().to_string();
                if is_recently_touched(state, &path_str) {
                    continue;
                }
                to_delete.push(path_str);
            }
            FileChange::Rename { old, new } => {
                let old_under = is_under_scan_root(&old, scan_root);
                let new_under = is_under_scan_root(&new, scan_root);
                if !old_under && !new_under {
                    continue;
                }

                let old_str = old.to_string_lossy().to_string();
                let new_str = new.to_string_lossy().to_string();

                if old_under && !is_recently_touched(state, &old_str) {
                    to_delete.push(old_str);
                }

                if new_under
                    && !is_recently_touched(state, &new_str)
                    && !should_skip_path(
                        &new,
                        &state.path_ignores,
                        &state.path_ignore_patterns,
                    )
                {
                    if let Ok(metadata) = std::fs::symlink_metadata(&new) {
                        if let Some(row) = index_row_from_path_and_metadata(&new, &metadata) {
                            to_upsert.push(row);
                        }
                    }
                }
            }
        }
    }

    if to_upsert.is_empty() && to_delete.is_empty() {
        return;
    }

    let changed = match db_connection(&state.db_path) {
        Ok(mut conn) => {
            let mut total = 0;
            if let Ok(n) = upsert_rows(&mut conn, &to_upsert) {
                total += n;
            }
            if let Ok(n) = delete_paths(&mut conn, &to_delete) {
                total += n;
            }
            total
        }
        Err(e) => {
            eprintln!("[win/rdcw] DB error: {e}");
            return;
        }
    };

    if changed > 0 {
        invalidate_search_caches(state);
        let _ = update_status_counts(state);

        if last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
            let _ = refresh_and_emit_status_counts(app, state);
            *last_status_emit = Instant::now();
        }
    }
}
