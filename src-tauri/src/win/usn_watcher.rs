use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};

use super::dir_rescan::DirRescanQueue;
use super::volume;
use crate::{
    db_connection, delete_paths, invalidate_search_caches,
    index_row_from_path_and_metadata, is_recently_touched,
    now_epoch, pathignore_active_entries, perf_log,
    refresh_and_emit_status_counts, set_meta,
    should_skip_path, update_status_counts, upsert_rows,
    AppState,
};

use windows::Win32::Foundation::{
    ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE, HANDLE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::FSCTL_READ_USN_JOURNAL;

const USN_FLUSH_INTERVAL: Duration = Duration::from_secs(30);
const STATUS_EMIT_MIN_INTERVAL: Duration = Duration::from_secs(2);

// Sleep-based polling (like Everything): read all accumulated records, then sleep.
const POLL_IDLE: Duration = Duration::from_secs(2);
const POLL_BUSY: Duration = Duration::from_millis(100);

// Debounce for flushing accumulated USN changes to DB.
// Higher than macOS FSEvent debounce (300ms) because USN records accumulate
// many noisy system changes, and we want fewer DB round-trips.
const USN_CHANGE_DEBOUNCE: Duration = Duration::from_secs(5);

const FALLBACK_CACHE_CLEAR_INTERVAL: Duration = Duration::from_secs(120);

// USN_REASON flags (only existence-related reasons; metadata changes are skipped)
const USN_REASON_FILE_CREATE: u32 = 0x00000100;
const USN_REASON_FILE_DELETE: u32 = 0x00000200;
const USN_REASON_RENAME_OLD_NAME: u32 = 0x00001000;
const USN_REASON_RENAME_NEW_NAME: u32 = 0x00002000;

/// Directory attribute bit in a USN record's FileAttributes field (S6).
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Bounded transient-read retries before the watcher exits so its supervisor
/// can restart it (S2). Journal-stale errors are handled separately.
const MAX_TRANSIENT_RETRIES: u32 = 5;

/// READ_USN_JOURNAL_DATA_V0 structure
#[repr(C)]
struct ReadUsnJournalDataV0 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
}

struct UsnChangeRecord {
    frn: u64,
    parent_frn: u64,
    reason: u32,
    attributes: u32,
    name: String,
}

#[derive(Clone)]
enum FileChange {
    Create(PathBuf),
    Delete(PathBuf),
    Rename { old: PathBuf, new: PathBuf },
}

struct RenamePending {
    old_path: PathBuf,
    created_at: Instant,
}

const RENAME_PAIR_TIMEOUT: Duration = Duration::from_millis(500);

/// FRN → directory path cache from MFT scan.
/// Enables zero-syscall path resolution for USN records.
type FrnPathCache = HashMap<u64, String>;

/// Start the USN watcher, reading from the current journal position.
/// `frn_cache`: pre-built FRN→path map from MFT scan (empty if unavailable).
/// `outside_scan_frns`: directory FRNs known to be outside scan_root (pre-populated skip set).
pub fn start(
    app: Option<AppHandle>,
    state: AppState,
    frn_cache: FrnPathCache,
    outside_scan_frns: HashSet<u64>,
) -> Result<(), String> {
    let vol = volume::open_volume('C')?;
    let journal = volume::query_usn_journal(&vol)?;

    perf_log(format!(
        "[win/usn] starting watcher, journal_id={} next_usn={} frn_cache={} skip_frns={}",
        journal.journal_id, journal.next_usn, frn_cache.len(), outside_scan_frns.len()
    ));

    let last_usn = journal.next_usn;
    let journal_id = journal.journal_id;

    spawn_poll_loop(app, state, vol, last_usn, journal_id, frn_cache, outside_scan_frns);
    Ok(())
}

/// Start the watcher resuming from an explicit position — the pre-scan journal
/// tip captured by the MFT scan (S5) — while carrying the MFT-built FRN caches.
/// Falls back to the current tip if the journal was recreated during the scan
/// or the captured position has since wrapped out of the journal.
pub fn start_at(
    app: Option<AppHandle>,
    state: AppState,
    start_usn: i64,
    journal_id: u64,
    frn_cache: FrnPathCache,
    outside_scan_frns: HashSet<u64>,
) -> Result<(), String> {
    let vol = volume::open_volume('C')?;
    let journal = volume::query_usn_journal(&vol)?;

    let (resume_usn, resume_jid) =
        if journal.journal_id == journal_id && start_usn >= journal.first_usn {
            (start_usn, journal_id)
        } else {
            perf_log(format!(
                "[win/usn] pre-scan position unusable (stored jid={} usn={}; current jid={} first_usn={}); \
                 starting at current tip {}",
                journal_id, start_usn, journal.journal_id, journal.first_usn, journal.next_usn
            ));
            (journal.next_usn, journal.journal_id)
        };

    perf_log(format!(
        "[win/usn] starting watcher at usn={} journal_id={} frn_cache={} skip_frns={}",
        resume_usn, resume_jid, frn_cache.len(), outside_scan_frns.len()
    ));

    spawn_poll_loop(app, state, vol, resume_usn, resume_jid, frn_cache, outside_scan_frns);
    Ok(())
}

fn spawn_poll_loop(
    app: Option<AppHandle>,
    state: AppState,
    vol: volume::VolumeHandle,
    initial_usn: i64,
    journal_id: u64,
    frn_cache: FrnPathCache,
    outside_scan_frns: HashSet<u64>,
) {
    std::thread::spawn(move || {
        poll_loop(app.as_ref(), &state, &vol, initial_usn, journal_id, frn_cache, outside_scan_frns);
    });
}

fn poll_loop(
    app: Option<&AppHandle>,
    state: &AppState,
    vol: &volume::VolumeHandle,
    initial_usn: i64,
    journal_id: u64,
    mut frn_cache: FrnPathCache,
    outside_scan_frns: HashSet<u64>,
) {
    // Publish liveness so the service supervisor / indexer barrier can tell a
    // running watcher from a dead one and wait for it to exit before a pass.
    state.watcher_active.store(true, AtomicOrdering::Release);

    let scan_root = state.scan_root.clone();
    let scan_str = scan_root.to_string_lossy().to_string().replace('/', "\\");
    let scan_prefix = format!("{}\\", scan_str);

    let mut last_usn = initial_usn;
    let mut pending_changes: Vec<FileChange> = Vec::new();
    let mut pending_renames: HashMap<u64, RenamePending> = HashMap::new();
    let mut last_flush = Instant::now();
    let mut last_usn_persist = Instant::now();
    let mut last_status_emit = Instant::now();
    // S6: single-flight subtree rescans for directory renames.
    let mut dir_rescans = DirRescanQueue::new();
    // S2: consecutive transient read errors before the watcher gives up.
    let mut transient_retries: u32 = 0;

    // Persistent DB connection — avoids expensive per-flush Connection::open()
    let mut db_conn = db_connection(&state.db_path).ok();

    // Positive fallback cache: FRN → resolved PathBuf (new dirs under scan_root).
    // Cleared periodically to handle moved/renamed directories.
    let mut dir_cache: HashMap<u64, PathBuf> = HashMap::new();
    let mut last_cache_clear = Instant::now();
    // Negative cache: FRNs confirmed outside scan_root or unresolvable.
    // Pre-populated from MFT scan with known outside-scan directory FRNs.
    // Never cleared — these are system dirs that won't move into scan_root.
    let mut skip_frns = outside_scan_frns;
    let mut reusable_buffer: Vec<u8> = vec![0u8; 64 * 1024];

    // Diagnostic counters (logged periodically)
    let mut diag_total_records: u64 = 0;
    let mut diag_frn_cache_hits: u64 = 0;
    let mut diag_skip_hits: u64 = 0;
    let mut diag_syscalls: u64 = 0;
    let mut diag_home_matches: u64 = 0;
    let mut diag_polls: u64 = 0;
    let mut last_diag = Instant::now();

    let mut diag_read_us: u64 = 0;
    let mut diag_process_us: u64 = 0;
    let mut diag_apply_us: u64 = 0;

    // Track config file changes to emit pathignore_changed when rules change
    let mut last_config_entries = pathignore_active_entries(
        &std::fs::read_to_string(&state.config_file_path).unwrap_or_default(),
    );

    loop {
        if state.watcher_stop.load(AtomicOrdering::Acquire) {
            eprintln!("[win/usn] stop signal received, exiting");
            break;
        }

        let t0 = Instant::now();
        let (next_usn, records) =
            match read_usn_journal(vol.raw(), last_usn, journal_id, &mut reusable_buffer) {
                Ok(v) => {
                    transient_retries = 0;
                    v
                }
                // S2: journal deleted/wrapped — resuming is impossible. Flag a
                // full rescan for the supervisor and exit the watcher.
                Err(UsnReadError::JournalStale(msg)) => {
                    eprintln!(
                        "[win/usn] journal stale ({msg}); requesting full rescan, exiting watcher"
                    );
                    state.watcher_lost_events.store(true, AtomicOrdering::Release);
                    break;
                }
                // S2: transient error — bounded backoff retry, then exit so the
                // supervisor restarts (and rescans) instead of spinning forever.
                Err(UsnReadError::Transient(msg)) => {
                    transient_retries += 1;
                    eprintln!(
                        "[win/usn] transient read error ({msg}); retry {transient_retries}/{MAX_TRANSIENT_RETRIES}"
                    );
                    if transient_retries > MAX_TRANSIENT_RETRIES {
                        eprintln!("[win/usn] too many transient read errors; exiting watcher for supervisor restart");
                        // Signal the supervisor to restart the watcher via a full
                        // rescan (mirrors the JournalStale path above). Without
                        // this the watcher stays dead — and live updates stop —
                        // until the hourly periodic reconcile fires.
                        state.watcher_lost_events.store(true, AtomicOrdering::Release);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500 * transient_retries as u64));
                    continue;
                }
            };
        diag_read_us += t0.elapsed().as_micros() as u64;

        diag_polls += 1;
        let t1 = Instant::now();

        for record in &records {
            diag_total_records += 1;

            // Resolve parent path: FRN cache (MFT) → skip_frns → dir_cache → syscall
            let parent_path: Option<PathBuf> =
                if let Some(path) = frn_cache.get(&record.parent_frn) {
                    diag_frn_cache_hits += 1;
                    Some(PathBuf::from(path))
                } else if skip_frns.contains(&record.parent_frn) {
                    // Known outside scan_root or unresolvable — skip without syscall
                    diag_skip_hits += 1;
                    None
                } else if let Some(cached) = dir_cache.get(&record.parent_frn) {
                    diag_frn_cache_hits += 1;
                    Some(cached.clone())
                } else {
                    diag_syscalls += 1;
                    let resolved =
                        frn_to_path(vol.raw(), record.parent_frn).filter(|p| {
                            let s = p.to_string_lossy();
                            *s == *scan_str || s.starts_with(&scan_prefix)
                        });
                    match &resolved {
                        Some(p) => { dir_cache.insert(record.parent_frn, p.clone()); }
                        None => { skip_frns.insert(record.parent_frn); }
                    }
                    resolved
                };
            let full_path = match parent_path {
                Some(ref _p) => { diag_home_matches += 1; _p.join(&record.name) }
                None => continue,
            };

            // Detect config file changes before skip check (config is under ignored app_data_dir)
            if full_path == state.config_file_path {
                let new_entries = pathignore_active_entries(
                    &std::fs::read_to_string(&state.config_file_path).unwrap_or_default(),
                );
                if new_entries != last_config_entries {
                    last_config_entries = new_entries;
                    // Headless service: flag the supervisor to reload rules +
                    // reconcile (app is None there). The GUI/daemon watcher also
                    // emits the banner event.
                    state.config_dirty.store(true, AtomicOrdering::Release);
                    if let Some(app) = app {
                        app.emit("pathignore_changed", ()).ok();
                    }
                }
                continue;
            }
            // A `.pathindexing` edit changes the indexed roots — reconcile to apply.
            if full_path == state.pathindexing_file_path {
                state.config_dirty.store(true, AtomicOrdering::Release);
                continue;
            }

            // Early path filter: skip paths in ignored directories BEFORE
            // creating FileChange events (avoids expensive stat + DB ops)
            if should_skip_path(
                &full_path,
                &state.path_ignores,
                &state.path_ignore_patterns,
            ) {
                continue;
            }

            let reason = record.reason;

            // Invalidate caches when a directory may have been renamed
            if (reason & (USN_REASON_RENAME_OLD_NAME | USN_REASON_RENAME_NEW_NAME)) != 0 {
                frn_cache.remove(&record.frn);
                dir_cache.remove(&record.frn);
                skip_frns.remove(&record.frn);
            }

            if (reason & USN_REASON_RENAME_OLD_NAME) != 0 {
                // First half of rename pair
                cleanup_expired_renames(&mut pending_renames, &mut pending_changes);
                pending_renames.insert(
                    record.frn,
                    RenamePending {
                        old_path: full_path,
                        created_at: Instant::now(),
                    },
                );
                continue;
            }

            if (reason & USN_REASON_RENAME_NEW_NAME) != 0 {
                // Second half of rename pair.
                let is_dir = (record.attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                if let Some(old_rename) = pending_renames.remove(&record.frn) {
                    if is_dir {
                        // S6: a directory rename emits USN records only for the
                        // directory entry, not its descendants, so child rows
                        // keep stale paths. Reconcile BOTH prefixes: the old one
                        // has vanished (rows deleted) and the new one exists
                        // (subtree reinserted).
                        dir_rescans.queue(old_rename.old_path);
                        dir_rescans.queue(full_path);
                    } else {
                        pending_changes.push(FileChange::Rename {
                            old: old_rename.old_path,
                            new: full_path,
                        });
                    }
                } else if is_dir {
                    // Unpaired NEW_NAME for a directory: rescan the new subtree
                    // so its descendants are indexed under the new path.
                    dir_rescans.queue(full_path);
                } else {
                    // No matching OLD_NAME — treat as create.
                    pending_changes.push(FileChange::Create(full_path));
                }
                continue;
            }

            if (reason & USN_REASON_FILE_DELETE) != 0 {
                pending_changes.push(FileChange::Delete(full_path));
                continue;
            }

            if (reason & USN_REASON_FILE_CREATE) != 0 {
                pending_changes.push(FileChange::Create(full_path));
                continue;
            }
        }

        // S3: advance the cursor to the journal's next-USN (from the read
        // header) so it moves forward even when every record was filtered out.
        last_usn = next_usn;

        diag_process_us += t1.elapsed().as_micros() as u64;

        // Expire any pending rename OLD_NAMEs that didn't get matched
        cleanup_expired_renames(&mut pending_renames, &mut pending_changes);

        // Debounce: flush pending changes (5s debounce reduces DB round-trips).
        if !pending_changes.is_empty() && last_flush.elapsed() >= USN_CHANGE_DEBOUNCE {
            let ta = Instant::now();
            apply_changes(
                app,
                state,
                &mut pending_changes,
                &mut last_status_emit,
                &mut db_conn,
                last_usn,
                journal_id,
            );
            diag_apply_us += ta.elapsed().as_micros() as u64;
            last_flush = Instant::now();
        }

        // S6: run any queued directory-rename subtree rescan (single-flight,
        // on a background thread — never blocks this poll loop).
        dir_rescans.pump(app, state);

        // Periodically clear positive dir_cache to handle moved/deleted directories.
        // skip_frns (negative cache) is never cleared — system dirs won't move into scan_root.
        if last_cache_clear.elapsed() >= FALLBACK_CACHE_CLEAR_INTERVAL {
            dir_cache.clear();
            last_cache_clear = Instant::now();
        }

        // Periodically persist the wall-clock activity timestamp for offline
        // catchup. This is NOT the USN cursor (S4): the cursor advances
        // transactionally with content in apply_changes, and idle-only advances
        // go to the usn.cursor file below — so a long idle stretch of ignored
        // events never churns PRAGMA data_version.
        if last_usn_persist.elapsed() >= USN_FLUSH_INTERVAL {
            if let Some(ref conn) = db_conn {
                let _ = set_meta(conn, "win_last_active_ts", &now_epoch().to_string());
            }
            // Only advance the file cursor when nothing is pending; pending
            // content persists the DB cursor when it flushes.
            if pending_changes.is_empty() && pending_renames.is_empty() {
                write_usn_cursor(&state.db_path, journal_id, last_usn);
            }
            last_usn_persist = Instant::now();
        }

        // Diagnostic log every 30s
        if last_diag.elapsed() >= Duration::from_secs(30) {
            eprintln!(
                "[win/usn/diag] polls={} records={} frn_hits={} skip_hits={} syscalls={} home={} pending={} read_ms={} proc_ms={} apply_ms={}",
                diag_polls, diag_total_records, diag_frn_cache_hits,
                diag_skip_hits, diag_syscalls,
                diag_home_matches, pending_changes.len(),
                diag_read_us / 1000, diag_process_us / 1000, diag_apply_us / 1000
            );
            diag_polls = 0;
            diag_total_records = 0;
            diag_frn_cache_hits = 0;
            diag_skip_hits = 0;
            diag_syscalls = 0;
            diag_home_matches = 0;
            diag_read_us = 0;
            diag_process_us = 0;
            diag_apply_us = 0;
            last_diag = Instant::now();
        }

        // Sleep-based polling: read accumulated records, process, then sleep.
        // Only use POLL_BUSY for outstanding rename pairs (which have a 500ms timeout).
        // pending_changes don't need fast polling — they accumulate until the 5s debounce.
        let has_rename_pending = !pending_renames.is_empty();
        std::thread::sleep(if has_rename_pending { POLL_BUSY } else { POLL_IDLE });
    }

    // Barrier flush: persist any unpersisted changes before this watcher thread
    // exits (stop / journal-stale / transient exhaustion) so the indexer
    // worker's next pass never races an in-flight batch and no batch is lost.
    apply_changes(
        app,
        state,
        &mut pending_changes,
        &mut last_status_emit,
        &mut db_conn,
        last_usn,
        journal_id,
    );
    state.watcher_active.store(false, AtomicOrdering::Release);
}

fn cleanup_expired_renames(
    pending_renames: &mut HashMap<u64, RenamePending>,
    pending_changes: &mut Vec<FileChange>,
) {
    let expired: Vec<u64> = pending_renames
        .iter()
        .filter(|(_, v)| v.created_at.elapsed() >= RENAME_PAIR_TIMEOUT)
        .map(|(k, _)| *k)
        .collect();

    for frn in expired {
        if let Some(old) = pending_renames.remove(&frn) {
            // Treat as delete (old path disappeared, new path never appeared)
            pending_changes.push(FileChange::Delete(old.old_path));
        }
    }
}

fn apply_changes(
    app: Option<&AppHandle>,
    state: &AppState,
    changes: &mut Vec<FileChange>,
    last_status_emit: &mut Instant,
    db_conn: &mut Option<rusqlite::Connection>,
    usn: i64,
    journal_id: u64,
) {
    if changes.is_empty() {
        return;
    }

    // Deduplicate: keep only the last change per path (avoids redundant stat +
    // DB ops). Iterate by reference — NOT drain — so a failed DB write leaves
    // `changes` intact for retry; the USN cursor must never advance past
    // changes that aren't yet durable (S4).
    let mut deduped: HashMap<PathBuf, FileChange> = HashMap::new();
    for change in changes.iter() {
        match change {
            FileChange::Create(p) | FileChange::Delete(p) => {
                deduped.insert(p.clone(), change.clone());
            }
            FileChange::Rename { old, new } => {
                deduped.insert(old.clone(), FileChange::Delete(old.clone()));
                deduped.insert(new.clone(), FileChange::Create(new.clone()));
            }
        }
    }

    let mut to_upsert = Vec::new();
    let mut to_delete = Vec::new();

    for (_, change) in deduped {
        match change {
            FileChange::Create(path) => {
                let path_str = path.to_string_lossy().to_string();
                if is_recently_touched(state, &path_str) {
                    continue;
                }
                if should_skip_path(&path, &state.path_ignores, &state.path_ignore_patterns) {
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
                let path_str = path.to_string_lossy().to_string();
                if is_recently_touched(state, &path_str) {
                    continue;
                }
                to_delete.push(path_str);
            }
            FileChange::Rename { .. } => {
                // Already decomposed into Create + Delete above.
            }
        }
    }

    if to_upsert.is_empty() && to_delete.is_empty() {
        // Everything deduped away (recently-touched / ignored) — nothing to
        // persist. Clear so the idle cursor file can advance past this range.
        changes.clear();
        return;
    }

    // Try to use the persistent connection, reconnect if needed.
    if db_conn.is_none() {
        *db_conn = db_connection(&state.db_path).ok();
    }
    let Some(conn) = db_conn.as_mut() else {
        eprintln!("[win/usn] DB connection unavailable; keeping changes for retry");
        return;
    };

    // Apply content. On any failure, keep `changes` for retry and do NOT
    // advance the cursor — dropping the in-memory batch AND advancing the
    // cursor was the S4 defect (permanent change loss on the next crash).
    let upserted = match upsert_rows(conn, &to_upsert) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[win/usn] upsert failed ({e}); keeping changes for retry");
            return;
        }
    };
    let deleted = match delete_paths(conn, &to_delete) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[win/usn] delete failed ({e}); keeping changes for retry");
            return;
        }
    };

    // Content is durable — advance the DB cursor now (content-then-cursor:
    // at-least-once. A crash between the content commit and here re-reads from
    // the old cursor on resume and idempotent upserts re-apply it, so no change
    // is ever lost — S4).
    let _ = set_meta(conn, "win_last_usn", &usn.to_string());
    let _ = set_meta(conn, "win_journal_id", &journal_id.to_string());

    changes.clear();

    if upserted + deleted > 0 {
        invalidate_search_caches(state);
        let _ = update_status_counts(state);
        if last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
            let _ = refresh_and_emit_status_counts(app, state);
            *last_status_emit = Instant::now();
        }
    }
}

/// Classified USN journal read failure (S2).
enum UsnReadError {
    /// Journal deleted, being deleted, or entries aged out (wrapped). Resuming
    /// is impossible — the watcher must request a full rescan.
    JournalStale(String),
    /// Transient failure — bounded retry, then exit for the supervisor.
    Transient(String),
}

/// Classify a `FSCTL_READ_USN_JOURNAL` failure (S2). Journal-deleted /
/// delete-in-progress / not-active all mean the resume point is gone and only a
/// full rescan recovers; anything else is treated as transient.
fn classify_usn_read_error(e: &windows::core::Error) -> UsnReadError {
    let code = e.code();
    if code == ERROR_JOURNAL_ENTRY_DELETED.to_hresult()
        || code == ERROR_JOURNAL_DELETE_IN_PROGRESS.to_hresult()
        || code == ERROR_JOURNAL_NOT_ACTIVE.to_hresult()
    {
        UsnReadError::JournalStale(format!("{e}"))
    } else {
        UsnReadError::Transient(format!("{e}"))
    }
}

/// Read USN records starting at `start_usn`. Returns the journal's next-USN
/// (from the 8-byte buffer header — S3) alongside the parsed records, so the
/// caller advances its cursor even when every record was filtered out by the
/// reason mask.
fn read_usn_journal(
    handle: HANDLE,
    start_usn: i64,
    journal_id: u64,
    buffer: &mut Vec<u8>,
) -> Result<(i64, Vec<UsnChangeRecord>), UsnReadError> {
    let read_data = ReadUsnJournalDataV0 {
        start_usn,
        // Only track file existence changes (create/delete/rename).
        // Metadata changes (size/mtime) don't affect search results
        // and generate heavy system noise that wastes CPU on stat+DB ops.
        reason_mask: USN_REASON_FILE_CREATE
            | USN_REASON_FILE_DELETE
            | USN_REASON_RENAME_OLD_NAME
            | USN_REASON_RENAME_NEW_NAME,
        return_only_on_close: 0,
        timeout: 0,
        bytes_to_wait_for: 0,
        usn_journal_id: journal_id,
    };

    let mut bytes_returned: u32 = 0;

    let result = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_READ_USN_JOURNAL,
            Some(&read_data as *const _ as *const _),
            mem::size_of::<ReadUsnJournalDataV0>() as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            buffer.len() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };

    // S2: classify failures instead of masking them as an empty result — the
    // old code returned Ok(empty) on any error, so a deleted/wrapped journal
    // retried forever making no progress.
    if let Err(e) = &result {
        return Err(classify_usn_read_error(e));
    }

    // The first 8 bytes are the USN to resume from on the next read (S3). A
    // successful read always returns at least this header; if somehow not,
    // leave the cursor unchanged.
    if (bytes_returned as usize) < 8 {
        return Ok((start_usn, Vec::new()));
    }
    let next_usn = i64::from_le_bytes(buffer[0..8].try_into().unwrap());

    let mut records = Vec::new();
    let mut offset = 8usize;
    while offset + 4 <= bytes_returned as usize {
        let record_len =
            u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;

        if record_len < 64 || offset + record_len > bytes_returned as usize {
            break;
        }

        if let Some(record) = parse_usn_change_record(&buffer[offset..offset + record_len]) {
            records.push(record);
        }

        offset += record_len;
    }

    Ok((next_usn, records))
}

fn parse_usn_change_record(data: &[u8]) -> Option<UsnChangeRecord> {
    if data.len() < 64 {
        return None;
    }

    let major = u16::from_le_bytes(data[4..6].try_into().ok()?);
    if major != 2 {
        return None;
    }

    let frn = u64::from_le_bytes(data[8..16].try_into().ok()?) & 0x0000_FFFF_FFFF_FFFF;
    let parent_frn =
        u64::from_le_bytes(data[16..24].try_into().ok()?) & 0x0000_FFFF_FFFF_FFFF;
    let reason = u32::from_le_bytes(data[40..44].try_into().ok()?);
    // FileAttributes (offset 52) — used to detect directory renames (S6).
    let attributes = u32::from_le_bytes(data[52..56].try_into().ok()?);

    let name_len = u16::from_le_bytes(data[56..58].try_into().ok()?) as usize;
    let name_offset = u16::from_le_bytes(data[58..60].try_into().ok()?) as usize;

    if name_offset + name_len > data.len() || name_len == 0 {
        return None;
    }

    let name_bytes = &data[name_offset..name_offset + name_len];
    let utf16: Vec<u16> = name_bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let name = String::from_utf16_lossy(&utf16);

    if name.starts_with('$') {
        return None;
    }

    Some(UsnChangeRecord {
        frn,
        parent_frn,
        reason,
        attributes,
        name,
    })
}

/// Resolve a File Reference Number to a filesystem path using
/// GetFinalPathNameByHandleW (via OpenFileById).
fn frn_to_path(volume_handle: HANDLE, frn: u64) -> Option<PathBuf> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, OpenFileById, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_ID_TYPE,
        FILE_NAME_NORMALIZED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file_id = FILE_ID_DESCRIPTOR {
        dwSize: mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FILE_ID_TYPE(0), // FileIdType
        Anonymous: FILE_ID_DESCRIPTOR_0 {
            FileId: frn as i64,
        },
    };

    let handle = unsafe {
        OpenFileById(
            volume_handle,
            &file_id,
            0, // no access needed, just path resolution
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
        .ok()?
    };

    let mut buf = vec![0u16; 512];
    let len = unsafe {
        GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED)
    };

    unsafe {
        let _ = CloseHandle(handle);
    }

    if len == 0 || len as usize > buf.len() {
        return None;
    }

    let path_str = String::from_utf16_lossy(&buf[..len as usize]);
    // GetFinalPathNameByHandleW returns "\\?\C:\..." prefix; strip it
    let cleaned = path_str
        .strip_prefix("\\\\?\\")
        .unwrap_or(&path_str);

    Some(PathBuf::from(cleaned))
}

/// Path of the idle USN cursor file (next to index.db). See S4.
pub(crate) fn usn_cursor_path(db_path: &Path) -> PathBuf {
    db_path.with_file_name("usn.cursor")
}

/// Atomically persist the idle cursor (journal_id + USN) via temp-file + rename
/// (Windows `MoveFileEx` replaces atomically). Best-effort: the DB cursor is the
/// durable source for content-bearing advances; this file only covers idle
/// stretches of ignored events, kept out of the DB so it can't churn
/// `PRAGMA data_version` (S4).
fn write_usn_cursor(db_path: &Path, journal_id: u64, usn: i64) {
    let path = usn_cursor_path(db_path);
    let tmp = db_path.with_file_name("usn.cursor.tmp");
    let body = format!("{journal_id}\n{usn}\n");
    if std::fs::write(&tmp, body.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Read the idle cursor file, returning `(journal_id, usn)` when present and
/// well-formed. Callers reconcile it against the DB cursor via `max` (S4).
/// Exercised by tests; the production read/resume path awaits service wiring.
#[allow(dead_code)]
pub(crate) fn read_usn_cursor(db_path: &Path) -> Option<(u64, i64)> {
    let text = std::fs::read_to_string(usn_cursor_path(db_path)).ok()?;
    let mut lines = text.lines();
    let journal_id = lines.next()?.trim().parse::<u64>().ok()?;
    let usn = lines.next()?.trim().parse::<i64>().ok()?;
    Some((journal_id, usn))
}

/// S4: pick the watcher resume position from the DB cursor and the idle cursor
/// file. `db` is `(usn, journal_id)` from meta; `file` is `(journal_id, usn)`
/// from `usn.cursor`. When both are present under the same journal, resume from
/// the higher USN — only ignored events lie between them, so it is loss-free.
/// Otherwise prefer the DB cursor, falling back to the file.
/// Exercised by tests; the production resume path awaits service wiring.
#[allow(dead_code)]
pub(crate) fn resolve_resume_position(
    db: (Option<i64>, Option<u64>),
    file: Option<(u64, i64)>,
) -> (Option<i64>, Option<u64>) {
    match file {
        Some((file_jid, file_usn)) => match db {
            (Some(db_usn), Some(db_jid)) if db_jid == file_jid => {
                (Some(db_usn.max(file_usn)), Some(db_jid))
            }
            (Some(db_usn), Some(db_jid)) => (Some(db_usn), Some(db_jid)),
            _ => (Some(file_usn), Some(file_jid)),
        },
        None => db,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;

    #[test]
    fn classifies_journal_stale_vs_transient() {
        for stale in [
            ERROR_JOURNAL_ENTRY_DELETED,
            ERROR_JOURNAL_DELETE_IN_PROGRESS,
            ERROR_JOURNAL_NOT_ACTIVE,
        ] {
            let e = windows::core::Error::from(stale.to_hresult());
            assert!(matches!(
                classify_usn_read_error(&e),
                UsnReadError::JournalStale(_)
            ));
        }
        let e = windows::core::Error::from(ERROR_ACCESS_DENIED.to_hresult());
        assert!(matches!(
            classify_usn_read_error(&e),
            UsnReadError::Transient(_)
        ));
    }

    #[test]
    fn resume_position_prefers_max_on_matching_journal() {
        // Same journal → higher USN wins (only ignored events lie between).
        assert_eq!(
            resolve_resume_position((Some(100), Some(7)), Some((7, 250))),
            (Some(250), Some(7))
        );
        assert_eq!(
            resolve_resume_position((Some(300), Some(7)), Some((7, 250))),
            (Some(300), Some(7))
        );
        // Different journal id → trust the DB cursor.
        assert_eq!(
            resolve_resume_position((Some(100), Some(7)), Some((9, 250))),
            (Some(100), Some(7))
        );
        // No DB cursor → use the file.
        assert_eq!(
            resolve_resume_position((None, None), Some((9, 250))),
            (Some(250), Some(9))
        );
        // No file → use the DB cursor.
        assert_eq!(
            resolve_resume_position((Some(100), Some(7)), None),
            (Some(100), Some(7))
        );
    }

    #[test]
    fn usn_cursor_roundtrip_and_malformed() {
        let dir = crate::temp_case_dir("usn_cursor");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("index.db");

        assert_eq!(read_usn_cursor(&db_path), None);

        write_usn_cursor(&db_path, 12345, 67890);
        assert_eq!(read_usn_cursor(&db_path), Some((12345, 67890)));

        // Overwrite replaces atomically.
        write_usn_cursor(&db_path, 12345, 99999);
        assert_eq!(read_usn_cursor(&db_path), Some((12345, 99999)));

        // Malformed content → None (never a panic).
        std::fs::write(usn_cursor_path(&db_path), b"garbage").unwrap();
        assert_eq!(read_usn_cursor(&db_path), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
