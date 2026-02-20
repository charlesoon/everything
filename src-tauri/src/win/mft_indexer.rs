use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use rusqlite::params;
use tauri::AppHandle;

use super::path_resolver::PathResolver;
use super::volume;
use crate::{
    cached_effective_ignore_rules,
    db_connection, emit_index_progress, emit_index_state, emit_index_updated,
    get_meta, gitignore_filter, invalidate_search_caches, matches_ignore_pattern, now_epoch,
    mem_search::CompactEntry,
    refresh_and_emit_status_counts,
    restore_normal_pragmas, set_indexing_pragmas, set_meta, set_progress,
    should_skip_path_ext, update_status_counts, upsert_rows,
    AppState, IgnorePattern, IndexRow, IndexState, BUILTIN_SKIP_NAMES,
};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::FSCTL_ENUM_USN_DATA;

const MFT_BATCH_SIZE: usize = 50_000;
const EMIT_INTERVAL: Duration = Duration::from_millis(200);
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

pub struct MftScanResult {
    pub scanned: u64,
    pub indexed: u64,
    pub permission_errors: u64,
}

#[repr(C)]
struct MftEnumDataV0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

struct MftRecord {
    frn: u64,
    parent_frn: u64,
    name: String,
    attributes: u32,
    timestamp: Option<i64>,
}

/// Lightweight file entry — directories go into PathResolver instead.
struct MftFileEntry {
    parent_frn: u64,
    name: String,
    timestamp: Option<i64>,
}

pub fn scan_mft(state: &AppState, app: &AppHandle) -> Result<MftScanResult, String> {
    use std::sync::atomic::Ordering as AtomicOrdering;

    let started = Instant::now();
    eprintln!("[win/mft] starting MFT scan");

    // Open volume FIRST — requires admin privileges.
    // Do NOT modify state/DB before this succeeds, so a failed open_volume
    // leaves index_complete and status untouched.
    let vol = volume::open_volume('C')?;
    eprintln!("[win/mft] volume opened in {}ms", started.elapsed().as_millis());

    state
        .indexing_active
        .store(true, AtomicOrdering::Release);

    // Mark index as incomplete — cleared when background DB finalize succeeds
    if let Ok(c) = db_connection(&state.db_path) {
        let _ = set_meta(&c, "index_complete", "0");
    }

    {
        let mut status = state.status.lock();
        status.state = IndexState::Indexing;
        status.message = None;
        status.scanned = 0;
        status.indexed = 0;
        status.current_path.clear();
    }
    emit_index_state(app, "Indexing", None);

    // ── Pass 1: Enumerate MFT — dirs into resolver, files into Vec ──
    let pass1_started = Instant::now();
    let mut resolver = PathResolver::with_capacity("C:", 300_000);
    let mut total_records: u64 = 0;
    let mut total_dirs: u64 = 0;
    let mut dir_entries: Vec<(u64, Option<i64>)> = Vec::with_capacity(300_000);
    let mut file_entries: Vec<MftFileEntry> = Vec::with_capacity(2_500_000);
    let mut pass1_last_emit = Instant::now();

    enumerate_mft(vol.raw(), |record| {
        total_records += 1;
        let is_dir = (record.attributes & FILE_ATTRIBUTE_DIRECTORY) != 0;

        if is_dir {
            total_dirs += 1;
            dir_entries.push((record.frn, record.timestamp));
            resolver.add_record(record.frn, record.parent_frn, record.name);
        } else {
            file_entries.push(MftFileEntry {
                parent_frn: record.parent_frn,
                name: record.name,
                timestamp: record.timestamp,
            });
        }

        if pass1_last_emit.elapsed() >= EMIT_INTERVAL {
            let msg = format!("Reading MFT... ({total_records} records)");
            set_progress(state, 0, 0, &msg);
            emit_index_progress(app, 0, 0, msg);
            pass1_last_emit = Instant::now();
        }
    })?;

    eprintln!(
        "[win/mft] pass1 done: {} records ({} dirs + {} files) in {}ms",
        total_records, total_dirs, file_entries.len(),
        pass1_started.elapsed().as_millis()
    );

    // ── Pass 1.5: Compute effective ignore rules + collect home subtree dirs ──
    let subtree_started = Instant::now();
    let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(state);
    let gi_filter = state.gitignore.get();

    // Build enhanced skip names: BUILTIN_SKIP_NAMES + AnySegment patterns from pathignore
    let extra_segment_names: Vec<String> = ignored_patterns.iter().filter_map(|p| {
        if let IgnorePattern::AnySegment(name) = p { Some(name.clone()) } else { None }
    }).collect();
    let mut all_skip_names: Vec<&str> = BUILTIN_SKIP_NAMES.to_vec();
    for name in &extra_segment_names {
        all_skip_names.push(name.as_str());
    }

    // Find FRNs for pathignore root directories to prune entire subtrees
    let mut skip_frns: HashSet<u64> = HashSet::new();
    for root in &ignored_roots {
        let root_win = root.to_string_lossy().replace('/', "\\");
        if let Some(frn) = resolver.find_frn_by_path(&root_win) {
            skip_frns.insert(frn);
        }
    }

    let scan_str = state.scan_root.to_string_lossy().to_string();
    let scan_path_win = scan_str.replace('/', "\\");

    let scan_frn = resolver.find_frn_by_path(&scan_path_win);
    let dir_subtree = match scan_frn {
        Some(frn) => {
            let subtree = resolver.collect_subtree_pruned(frn, &all_skip_names, &skip_frns);
            eprintln!(
                "[win/mft] pass1.5: scan_root FRN={} dir_subtree={} dirs \
                 (pruned from {} total dirs) skip_names={} skip_frns={} in {}ms",
                frn, subtree.len(), total_dirs,
                all_skip_names.len(), skip_frns.len(),
                subtree_started.elapsed().as_millis()
            );
            subtree
        }
        None => {
            eprintln!(
                "[win/mft] pass1.5: scan_root not found in MFT ({}), using all dirs",
                scan_path_win
            );
            dir_entries.iter().map(|(frn, _)| *frn).collect()
        }
    };

    resolver.drop_children_map();

    // Pre-resolve all directory paths in subtree (so file lookups are cache hits)
    let preresolve_started = Instant::now();
    for &dir_frn in &dir_subtree {
        let _ = resolver.resolve(dir_frn);
    }
    eprintln!(
        "[win/mft] pre-resolved {} dir paths in {}ms",
        dir_subtree.len(), preresolve_started.elapsed().as_millis()
    );

    // All subtree dirs are now in path_cache — frn_map no longer needed
    resolver.drop_frn_map();

    // ── Pass 2: Resolve paths, filter, stat → collect into memory ──
    let pass2_started = Instant::now();

    // Get path_cache early — all subtree dirs pre-resolved in pass 1.5
    let path_cache = resolver.path_cache();

    // --- Process directories (parallel, mtime from USN timestamp) ---
    let dir_results: Vec<CompactEntry> = dir_entries
        .par_iter()
        .filter(|(frn, _)| dir_subtree.contains(frn))
        .filter_map(|(frn, timestamp)| {
            let full_path = path_cache.get(frn)?;
            if should_skip_path_ext(Path::new(full_path), &ignored_roots, &ignored_patterns, Some(&gi_filter), Some(true)) {
                return None;
            }
            let path = Path::new(full_path);
            let name = path.file_name()?.to_string_lossy().to_string();
            let dir = path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            Some(CompactEntry { name, dir, is_dir: true, ext: None, mtime: *timestamp, size: None })
        })
        .collect();

    let dirs_in_subtree = dir_entries.iter().filter(|(frn, _)| dir_subtree.contains(frn)).count() as u64;
    let dir_indexed = dir_results.len() as u64;
    let mut scanned: u64 = dirs_in_subtree;
    let mut indexed: u64 = dir_indexed;
    let mut filtered_skip: u64 = dirs_in_subtree - dir_indexed;
    let mut mem_entries: Vec<CompactEntry> = Vec::with_capacity(dir_subtree.len() + file_entries.len());
    mem_entries.extend(dir_results);

    eprintln!(
        "[win/mft] dirs done: scanned={scanned} indexed={indexed} \
         skip={filtered_skip} resolve_fail=0 in {}ms",
        pass2_started.elapsed().as_millis()
    );

    emit_index_progress(app, scanned, indexed, String::new());

    // --- Process files (parallel + stat) ---
    let pass2_files_started = Instant::now();

    // Pre-extract only Glob patterns (AnySegment already handled by subtree pruning)
    let glob_patterns: Vec<&IgnorePattern> = ignored_patterns
        .iter()
        .filter(|p| matches!(p, IgnorePattern::Glob(_)))
        .collect();

    let file_results: Vec<CompactEntry> = file_entries
        .par_iter()
        .filter(|entry| dir_subtree.contains(&entry.parent_frn))
        .filter_map(|entry| {
            let parent_path = path_cache.get(&entry.parent_frn)?;

            if should_skip_file_in_pruned_subtree(
                parent_path, &entry.name, &gi_filter, &glob_patterns,
            ) {
                return None;
            }

            let ext = Path::new(&entry.name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase());

            Some(CompactEntry {
                name: entry.name.clone(),
                dir: parent_path.clone(),
                is_dir: false,
                ext,
                mtime: entry.timestamp,
                size: None,
            })
        })
        .collect();

    let files_in_subtree = file_entries
        .iter()
        .filter(|e| dir_subtree.contains(&e.parent_frn))
        .count() as u64;
    let file_indexed = file_results.len() as u64;
    scanned += files_in_subtree;
    indexed += file_indexed;
    filtered_skip += files_in_subtree - file_indexed;
    mem_entries.extend(file_results);

    eprintln!(
        "[win/mft] files done: {} files in subtree, indexed={file_indexed} skip={} in {}ms",
        files_in_subtree,
        files_in_subtree - file_indexed,
        pass2_files_started.elapsed().as_millis()
    );

    // Collect FRNs of directories known to be outside scan_root.
    // These pre-populate USN watcher's skip set to avoid syscalls.
    let outside_scan_frns: HashSet<u64> = dir_entries
        .iter()
        .filter(|(frn, _)| !dir_subtree.contains(frn))
        .map(|(frn, _)| *frn)
        .collect();

    // Free large temporaries before building MemIndex
    drop(file_entries);   // ~100MB+
    drop(dir_entries);    // ~2MB
    drop(dir_subtree);    // ~2MB

    let frn_cache = resolver.into_path_cache(); // also drops resolver.frn_map

    let mem_ms = started.elapsed().as_millis();
    eprintln!(
        "[win/mft] in-memory index ready: indexed={indexed} skip={filtered_skip} \
         entries={} mem_ms={mem_ms}ms",
        mem_entries.len(),
    );

    // ── Build indexed mem_index → Ready immediately ──
    let mem_idx = Arc::new(crate::mem_search::MemIndex::build(mem_entries));
    *state.mem_index.write() = Some(Arc::clone(&mem_idx));

    {
        let mut status = state.status.lock();
        status.state = IndexState::Ready;
        status.permission_errors = 0;
        status.scanned = scanned;
        status.indexed = indexed;
        status.message = None;
        status.entries_count = indexed;
        status.last_updated = Some(now_epoch());
    }

    emit_index_progress(app, scanned, indexed, String::new());
    emit_index_updated(app, indexed, now_epoch(), 0);
    emit_index_state(app, "Ready", None);

    // ── Background: DB upsert + USN watcher start ──
    let bg_state = state.clone();
    let bg_app = app.clone();
    let bg_vol = vol;
    let bg_mem_idx = Arc::clone(&mem_idx);

    eprintln!(
        "[win/mft] passing {} FRN path entries + {} outside-scan FRNs to USN watcher",
        frn_cache.len(), outside_scan_frns.len()
    );

    std::thread::spawn(move || {
        let bg_started = Instant::now();
        let entry_count = bg_mem_idx.entries().len();
        eprintln!("[win/mft/bg] starting background DB upsert ({entry_count} entries)");

        // Phase 1: Bulk insert (mtime from USN, size+mtime from FindFirstFileW cache)
        let bulk_result = background_db_bulk_insert(&bg_state, bg_mem_idx.entries());

        // Phase 2: Finalize DB
        match bulk_result {
            Ok((conn, current_run_id)) => {
                if let Err(e) = background_db_finalize(
                    conn, &bg_state, &bg_app, &bg_vol, current_run_id, entry_count > 0,
                    || {
                        // Callback: free MemIndex after primary index is created
                        drop(bg_mem_idx);
                        *bg_state.mem_index.write() = None;
                        eprintln!("[win/mft/bg] MemIndex freed (name index ready, building remaining)");
                    },
                ) {
                    eprintln!("[win/mft/bg] DB finalize FAILED: {e}");
                }
            }
            Err(e) => {
                eprintln!("[win/mft/bg] DB bulk insert FAILED: {e}");
                drop(bg_mem_idx);
                *bg_state.mem_index.write() = None;
            }
        }

        bg_state
            .indexing_active
            .store(false, AtomicOrdering::Release);

        eprintln!(
            "[win/mft/bg] background work done in {}ms",
            bg_started.elapsed().as_millis()
        );

        // Start USN watcher with FRN cache + outside-scan FRN set for zero-syscall path resolution
        if let Err(e) = super::usn_watcher::start(bg_app.clone(), bg_state.clone(), frn_cache, outside_scan_frns) {
            eprintln!("[win/mft/bg] USN watcher failed ({e}), trying RDCW fallback");
            if let Err(e2) = super::rdcw_watcher::start(bg_app, bg_state) {
                eprintln!("[win/mft/bg] RDCW watcher also failed ({e2}), no live updates");
            }
        }
    });

    Ok(MftScanResult {
        scanned,
        indexed,
        permission_errors: 0,
    })
}

/// Batch-retrieve file size and mtime per directory using FindFirstFileW/FindNextFileW.
/// Returns dir_path → (name_lowercase → (size, mtime)).
/// Much faster than per-file fs::metadata() because it reads one directory listing at a time.
fn build_dir_stat_cache(
    entries: &[CompactEntry],
) -> HashMap<String, HashMap<String, (i64, i64)>> {
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
    };
    use windows::core::PCWSTR;

    // Collect unique parent dirs from file entries
    let unique_dirs: HashSet<&str> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.dir.as_str())
        .collect();

    let dir_list: Vec<&str> = unique_dirs.into_iter().collect();

    // Parallel enumeration via rayon
    let results: Vec<(String, HashMap<String, (i64, i64)>)> = dir_list
        .par_iter()
        .filter_map(|&dir_path| {
            let pattern = format!("{}\\*", dir_path);
            let wide: Vec<u16> = pattern.encode_utf16().chain(std::iter::once(0)).collect();

            let mut find_data = WIN32_FIND_DATAW::default();
            let handle = unsafe { FindFirstFileW(PCWSTR(wide.as_ptr()), &mut find_data) };

            let handle = match handle {
                Ok(h) => h,
                Err(_) => return None,
            };

            let mut dir_map: HashMap<String, (i64, i64)> = HashMap::new();

            loop {
                let name = wide_name_to_string(&find_data.cFileName);
                if name != "." && name != ".." {
                    let is_dir = (find_data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) != 0;
                    if !is_dir {
                        let size = ((find_data.nFileSizeHigh as i64) << 32)
                            | (find_data.nFileSizeLow as i64);
                        let ft = ((find_data.ftLastWriteTime.dwHighDateTime as i64) << 32)
                            | (find_data.ftLastWriteTime.dwLowDateTime as i64);
                        let mtime = filetime_to_unix(ft);
                        dir_map.insert(name.to_lowercase(), (size, mtime));
                    }
                }

                let ok = unsafe { FindNextFileW(handle, &mut find_data) };
                if ok.is_err() {
                    break;
                }
            }

            unsafe { let _ = FindClose(handle); }
            Some((dir_path.to_string(), dir_map))
        })
        .collect();

    results.into_iter().collect()
}

/// Extract a file name from WIN32_FIND_DATAW.cFileName (null-terminated UTF-16).
fn wide_name_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

/// Phase 1: Bulk insert entries into DB.
/// Returns (conn, current_run_id) so the caller can free MemIndex before Phase 2.
fn background_db_bulk_insert(
    state: &AppState,
    entries: &[CompactEntry],
) -> Result<(rusqlite::Connection, i64), String> {
    let mut conn = db_connection(&state.db_path)?;
    set_indexing_pragmas(&conn)?;

    let last_run_id: i64 = get_meta(&conn, "last_run_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let current_run_id = last_run_id + 1;
    let indexed_at = now_epoch();

    // Drop indexes for faster bulk insert
    let _ = conn.execute_batch(
        r#"
        DROP INDEX IF EXISTS idx_entries_dir_ext_name_nocase;
        DROP INDEX IF EXISTS idx_entries_mtime;
        DROP INDEX IF EXISTS idx_entries_name_nocase;
        DROP INDEX IF EXISTS idx_entries_ext_name;
        "#,
    );
    eprintln!("[win/mft/bg] indexes dropped");

    // Build dir stat cache: batch-retrieve file size+mtime via FindFirstFileW per directory
    let cache_started = Instant::now();
    let dir_stat_cache = build_dir_stat_cache(entries);
    eprintln!(
        "[win/mft/bg] dir_stat_cache built: {} dirs in {}ms",
        dir_stat_cache.len(), cache_started.elapsed().as_millis()
    );

    let upsert_started = Instant::now();

    for chunk in entries.chunks(MFT_BATCH_SIZE) {
        let chunk_rows: Vec<IndexRow> = chunk
            .par_iter()
            .map(|entry| {
                let (size, mtime) = if entry.is_dir {
                    // Dirs: use USN timestamp (already set in mtime)
                    (None, entry.mtime)
                } else if let Some(dir_cache) = dir_stat_cache.get(&entry.dir) {
                    // Files: lookup from FindFirstFileW cache
                    if let Some(&(sz, mt)) = dir_cache.get(&entry.name.to_lowercase()) {
                        (Some(sz), Some(mt))
                    } else {
                        // Not found in cache — use USN timestamp, no size
                        (None, entry.mtime)
                    }
                } else {
                    // Dir enumeration failed — use USN timestamp fallback
                    (None, entry.mtime)
                };
                IndexRow {
                    path: entry.path(),
                    name: entry.name.clone(),
                    dir: entry.dir.clone(),
                    is_dir: if entry.is_dir { 1 } else { 0 },
                    ext: entry.ext.clone(),
                    mtime,
                    size,
                    indexed_at,
                    run_id: current_run_id,
                }
            })
            .collect();
        upsert_rows(&mut conn, &chunk_rows)?;
    }
    let upsert_ms = upsert_started.elapsed().as_millis();
    eprintln!("[win/mft/bg] upsert done: {} entries in {upsert_ms}ms", entries.len());

    Ok((conn, current_run_id))
}

/// Phase 2: Cleanup stale rows, recreate indexes, save USN position.
/// `free_mem_index` is called after the primary name index is built, allowing
/// MemIndex to be freed while remaining indexes are created (reduces peak memory).
fn background_db_finalize(
    conn: rusqlite::Connection,
    state: &AppState,
    app: &AppHandle,
    vol: &volume::VolumeHandle,
    current_run_id: i64,
    has_entries: bool,
    free_mem_index: impl FnOnce(),
) -> Result<(), String> {
    // Cleanup stale entries
    let cleanup_started = Instant::now();
    let deleted_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE run_id < ?1",
            params![current_run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    conn.execute(
        "DELETE FROM entries WHERE run_id < ?1",
        params![current_run_id],
    )
    .map_err(|e| e.to_string())?;
    eprintln!("[win/mft/bg] cleanup: deleted={deleted_count} in {}ms", cleanup_started.elapsed().as_millis());

    set_meta(&conn, "last_run_id", &current_run_id.to_string())?;

    // Create the primary name index FIRST — this allows DB search to work
    let idx_started = Instant::now();
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);",
    )
    .map_err(|e| e.to_string())?;
    eprintln!("[win/mft/bg] name index created in {}ms", idx_started.elapsed().as_millis());

    // Free MemIndex now — DB search can use the name index
    free_mem_index();

    // Create remaining indexes (MemIndex freed, lower peak memory)
    let idx2_started = Instant::now();
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_entries_dir_ext_name_nocase ON entries(dir, ext, name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
        CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);
        "#,
    )
    .map_err(|e| e.to_string())?;
    eprintln!("[win/mft/bg] remaining indexes in {}ms", idx2_started.elapsed().as_millis());

    let _ = restore_normal_pragmas(&conn);

    // Save USN journal position for future resume
    if let Ok(journal) = volume::query_usn_journal(vol) {
        let _ = set_meta(&conn, "win_last_usn", &journal.next_usn.to_string());
        let _ = set_meta(&conn, "win_journal_id", &journal.journal_id.to_string());
    }

    // Mark index as complete — startup will check this to decide catchup vs re-index
    let _ = set_meta(&conn, "index_complete", "1");

    if deleted_count > 0 || has_entries {
        invalidate_search_caches(state);
    }

    let (entries_count, last_updated) = update_status_counts(state)?;
    // Persist cached counts for instant startup next time
    let _ = set_meta(&conn, "cached_entries_count", &entries_count.to_string());
    if let Some(lu) = last_updated {
        let _ = set_meta(&conn, "cached_last_updated", &lu.to_string());
    }
    let updated_at = last_updated.unwrap_or_else(now_epoch);
    emit_index_updated(app, entries_count, updated_at, 0);
    let _ = refresh_and_emit_status_counts(app, state);

    Ok(())
}

/// Lightweight skip check for files whose parent directory is already in the
/// pruned subtree. Since subtree pruning already eliminates BUILTIN_SKIP_NAMES,
/// BUILTIN_SKIP_PATHS, ignored_roots, and AnySegment patterns, this only needs
/// to check gitignore rules and Glob patterns.
fn should_skip_file_in_pruned_subtree(
    parent_path: &str,
    file_name: &str,
    gi_filter: &gitignore_filter::GitignoreFilter,
    glob_patterns: &[&IgnorePattern],
) -> bool {
    let full_path_str = format!("{}\\{}", parent_path, file_name);

    // Check gitignore
    if gi_filter.is_ignored(Path::new(&full_path_str), false) {
        return true;
    }

    // Check Glob patterns only (AnySegment handled by subtree pruning)
    if !glob_patterns.is_empty() {
        let normalized = full_path_str.replace('\\', "/");
        for pat in glob_patterns {
            if matches_ignore_pattern(&normalized, pat) {
                return true;
            }
        }
    }

    false
}

fn enumerate_mft(
    handle: HANDLE,
    mut callback: impl FnMut(MftRecord),
) -> Result<(), String> {
    let mut med = MftEnumDataV0 {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: i64::MAX,
    };

    let mut buffer: Vec<u8> = vec![0u8; 64 * 1024];

    loop {
        let mut bytes_returned: u32 = 0;

        let result = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_ENUM_USN_DATA,
                Some(&med as *const _ as *const _),
                mem::size_of::<MftEnumDataV0>() as u32,
                Some(buffer.as_mut_ptr() as *mut _),
                buffer.len() as u32,
                Some(&mut bytes_returned),
                None,
            )
        };

        if result.is_err() {
            break;
        }

        if bytes_returned < 8 {
            break;
        }

        let next_frn = u64::from_le_bytes(buffer[0..8].try_into().unwrap());

        let mut offset = 8usize;
        while offset + 4 <= bytes_returned as usize {
            let record_len =
                u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;

            if record_len < 64 || offset + record_len > bytes_returned as usize {
                break;
            }

            if let Some(record) = parse_usn_record_v2(&buffer[offset..offset + record_len]) {
                callback(record);
            }

            offset += record_len;
        }

        med.start_file_reference_number = next_frn;
    }

    Ok(())
}

fn parse_usn_record_v2(data: &[u8]) -> Option<MftRecord> {
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

    // Bytes 32-40: TimeStamp (FILETIME) — last modification time from USN record
    let filetime_raw = i64::from_le_bytes(data[32..40].try_into().ok()?);
    let timestamp = if filetime_raw > 0 {
        Some(filetime_to_unix(filetime_raw))
    } else {
        None
    };

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

    Some(MftRecord {
        frn,
        parent_frn,
        name,
        attributes,
        timestamp,
    })
}

pub fn filetime_to_unix(filetime: i64) -> i64 {
    const FILETIME_UNIX_DIFF: i64 = 116_444_736_000_000_000;
    if filetime <= FILETIME_UNIX_DIFF {
        return 0;
    }
    (filetime - FILETIME_UNIX_DIFF) / 10_000_000
}
