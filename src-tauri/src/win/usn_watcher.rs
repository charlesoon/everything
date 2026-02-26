use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::PathBuf;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};

use super::volume;
use crate::{
    db_connection, delete_paths, invalidate_search_caches,
    index_row_from_path_and_metadata, is_recently_touched,
    now_epoch, pathignore_active_entries, perf_log,
    refresh_and_emit_status_counts, set_meta,
    should_skip_path, update_status_counts, upsert_rows,
    AppState,
};

use windows::Win32::Foundation::HANDLE;
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
    usn: i64,
    reason: u32,
    name: String,
}

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
    app: AppHandle,
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

/// Start USN watcher with replay from a previously saved position.
/// Returns Err if the journal has been reset (different journal_id).
pub fn start_with_resume(
    app: AppHandle,
    state: AppState,
    stored_usn: i64,
    stored_journal_id: u64,
) -> Result<(), String> {
    let vol = volume::open_volume('C')?;
    let journal = volume::query_usn_journal(&vol)?;

    if journal.journal_id != stored_journal_id {
        return Err(format!(
            "journal_id changed: stored={} current={}",
            stored_journal_id, journal.journal_id
        ));
    }

    if stored_usn < journal.first_usn {
        return Err(format!(
            "stored USN {} < first_usn {}, journal wrapped",
            stored_usn, journal.first_usn
        ));
    }

    perf_log(format!(
        "[win/usn] resuming from stored_usn={} (current next_usn={})",
        stored_usn, journal.next_usn
    ));

    spawn_poll_loop(app, state, vol, stored_usn, journal.journal_id, HashMap::new(), HashSet::new());
    Ok(())
}

fn spawn_poll_loop(
    app: AppHandle,
    state: AppState,
    vol: volume::VolumeHandle,
    initial_usn: i64,
    journal_id: u64,
    frn_cache: FrnPathCache,
    outside_scan_frns: HashSet<u64>,
) {
    std::thread::spawn(move || {
        poll_loop(&app, &state, &vol, initial_usn, journal_id, frn_cache, outside_scan_frns);
    });
}

fn poll_loop(
    app: &AppHandle,
    state: &AppState,
    vol: &volume::VolumeHandle,
    initial_usn: i64,
    journal_id: u64,
    mut frn_cache: FrnPathCache,
    outside_scan_frns: HashSet<u64>,
) {
    let scan_root = state.scan_root.clone();
    let scan_str = scan_root.to_string_lossy().to_string().replace('/', "\\");
    let scan_prefix = format!("{}\\", scan_str);

    let mut last_usn = initial_usn;
    let mut pending_changes: Vec<FileChange> = Vec::new();
    let mut pending_renames: HashMap<u64, RenamePending> = HashMap::new();
    let mut last_flush = Instant::now();
    let mut last_usn_persist = Instant::now();
    let mut last_status_emit = Instant::now();

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
        let records = match read_usn_journal(vol.raw(), last_usn, journal_id, &mut reusable_buffer) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[win/usn] read error: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        diag_read_us += t0.elapsed().as_micros() as u64;

        diag_polls += 1;
        let t1 = Instant::now();

        for record in &records {
            last_usn = record.usn;
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
                    app.emit("pathignore_changed", ()).ok();
                }
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
                // Second half of rename pair
                if let Some(old_rename) = pending_renames.remove(&record.frn) {
                    pending_changes.push(FileChange::Rename {
                        old: old_rename.old_path,
                        new: full_path,
                    });
                } else {
                    // No matching OLD_NAME — treat as create
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

        diag_process_us += t1.elapsed().as_micros() as u64;

        // Expire any pending rename OLD_NAMEs that didn't get matched
        cleanup_expired_renames(&mut pending_renames, &mut pending_changes);

        // Debounce: flush pending changes (5s debounce reduces DB round-trips)
        if !pending_changes.is_empty() && last_flush.elapsed() >= USN_CHANGE_DEBOUNCE {
            let ta = Instant::now();
            apply_changes(app, state, &mut pending_changes, &mut last_status_emit, &mut db_conn);
            diag_apply_us += ta.elapsed().as_micros() as u64;
            last_flush = Instant::now();
        }

        // Periodically clear positive dir_cache to handle moved/deleted directories.
        // skip_frns (negative cache) is never cleared — system dirs won't move into scan_root.
        if last_cache_clear.elapsed() >= FALLBACK_CACHE_CLEAR_INTERVAL {
            dir_cache.clear();
            last_cache_clear = Instant::now();
        }

        // Periodically persist USN position + last active timestamp
        if last_usn_persist.elapsed() >= USN_FLUSH_INTERVAL {
            if let Some(ref conn) = db_conn {
                let _ = set_meta(conn, "win_last_usn", &last_usn.to_string());
                let _ = set_meta(conn, "win_journal_id", &journal_id.to_string());
                let _ = set_meta(conn, "win_last_active_ts", &now_epoch().to_string());
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
    app: &AppHandle,
    state: &AppState,
    changes: &mut Vec<FileChange>,
    last_status_emit: &mut Instant,
    db_conn: &mut Option<rusqlite::Connection>,
) {
    if changes.is_empty() {
        return;
    }

    // Deduplicate: keep only the last change per path.
    // This avoids redundant stat + DB ops for files changed multiple times.
    let mut deduped: HashMap<PathBuf, FileChange> = HashMap::new();
    for change in changes.drain(..) {
        match &change {
            FileChange::Create(p) | FileChange::Delete(p) => {
                deduped.insert(p.clone(), change);
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
                let path_str = path.to_string_lossy().to_string();
                if is_recently_touched(state, &path_str) {
                    continue;
                }
                to_delete.push(path_str);
            }
            FileChange::Rename { .. } => {
                // Already decomposed into Create + Delete above
            }
        }
    }

    if to_upsert.is_empty() && to_delete.is_empty() {
        return;
    }

    // Try to use persistent connection, reconnect if needed
    if db_conn.is_none() {
        *db_conn = db_connection(&state.db_path).ok();
    }

    let changed = match db_conn.as_mut() {
        Some(conn) => {
            let mut total = 0;
            if let Ok(n) = upsert_rows(conn, &to_upsert) {
                total += n;
            }
            if let Ok(n) = delete_paths(conn, &to_delete) {
                total += n;
            }
            total
        }
        None => {
            eprintln!("[win/usn] DB connection unavailable");
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

fn read_usn_journal(
    handle: HANDLE,
    start_usn: i64,
    journal_id: u64,
    buffer: &mut Vec<u8>,
) -> Result<Vec<UsnChangeRecord>, String> {
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

    if result.is_err() {
        return Ok(Vec::new());
    }

    if bytes_returned < 8 {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();

    // First 8 bytes: next USN
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

    Ok(records)
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
    let usn = i64::from_le_bytes(data[24..32].try_into().ok()?);
    let reason = u32::from_le_bytes(data[40..44].try_into().ok()?);

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
        usn,
        reason,
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
