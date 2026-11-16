use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::runtime::EventSink;
use rusqlite::params;

use super::path_resolver::PathResolver;
use super::volume;
use crate::{
    cached_effective_ignore_rules, cleanup_entries_gc_tables, db_connection, emit_index_progress,
    emit_index_state, emit_index_updated, get_meta, invalidate_search_caches,
    matches_ignore_pattern, mem_search::CompactEntry, now_epoch, refresh_and_emit_status_counts,
    restore_normal_pragmas, set_indexing_pragmas, set_meta, set_progress, set_state,
    update_status_counts, upsert_rows, AppState, IgnorePattern, IndexRow, IndexState,
    BUILTIN_SKIP_NAMES,
};

use windows::Win32::Foundation::{ERROR_HANDLE_EOF, HANDLE};
use windows::Win32::System::Ioctl::FSCTL_ENUM_USN_DATA;
use windows::Win32::System::IO::DeviceIoControl;

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

/// GUI-only progressive Ready (R5 / D4): publish a partial MemIndex built from
/// the directories scanned so far so the app can search immediately while files
/// finish loading, and report Ready. Returns the partial index (held until the
/// background pass swaps in the full one). The service skips this — it has no
/// searcher and must not report Ready mid-scan.
fn publish_progressive_ready(
    state: &AppState,
    app: Option<&EventSink>,
    dir_results: &[CompactEntry],
    scanned: u64,
    indexed: u64,
) -> Arc<crate::mem_search::MemIndex> {
    let early_cap = super::EARLY_MEM_INDEX_LIMIT.min(dir_results.len());
    let early_entries: Vec<CompactEntry> = dir_results.iter().take(early_cap).cloned().collect();
    let early_idx = Arc::new(crate::mem_search::MemIndex::build(early_entries));
    *state.mem_index.write() = Some(Arc::clone(&early_idx));
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
    if let Some(app) = app {
        emit_index_progress(app, scanned, indexed, String::new());
        emit_index_updated(app, indexed, now_epoch(), 0);
        emit_index_state(app, "Ready", None);
    }
    early_idx
}

/// Full MFT scan. `build_mem_index` controls the GUI-only MemIndex (progressive
/// Ready + the full in-memory searcher): the GUI passes `true`; the service
/// passes `false` and upserts straight from the raw entries (R5).
pub fn scan_mft(
    state: &AppState,
    app: Option<&EventSink>,
    build_mem_index: bool,
) -> Result<MftScanResult, String> {
    use std::sync::atomic::Ordering as AtomicOrdering;

    let started = Instant::now();
    let scan_phase = crate::diag::Phase::begin("mft.scan_records");
    let ts = || format!("{:.1}s", started.elapsed().as_secs_f32());
    eprintln!("[win/mft +{}] starting MFT scan", ts());

    // Open volume FIRST — requires admin privileges.
    // Do NOT modify state/DB before this succeeds, so a failed open_volume
    // leaves index_complete and status untouched.
    let vol = volume::open_volume('C')?;
    eprintln!("[win/mft +{}] volume opened", ts());

    // S5: capture the USN journal position BEFORE enumerating the MFT. The
    // watcher resumes from here and replays every change made during the
    // (multi-minute) scan; idempotent upserts absorb the overlap. The old code
    // captured this in finalize (after the scan), leaving a change-loss window
    // for the scan's whole duration. `None` when the journal can't be queried;
    // the caller then falls back to starting the watcher at the current tip.
    let start_journal = volume::query_usn_journal(&vol).ok();

    state.indexing_active.store(true, AtomicOrdering::Release);

    // Mark index as incomplete — cleared when background DB finalize succeeds
    if let Ok(c) = db_connection(&state.db_path) {
        let _ = set_meta(&c, "index_complete", "0");
    }

    {
        let mut status = state.status.lock();
        status.state = IndexState::Indexing;
        status.message = Some("Scanning file records".into());
        status.scanned = 0;
        status.indexed = 0;
        status.current_path.clear();
    }
    if let Some(app) = app {
        emit_index_state(app, "Indexing", None);
    }

    // ── Pass 1: Enumerate MFT — dirs into resolver, files into Vec ──
    let pass1_started = Instant::now();
    let enumerate_phase = crate::diag::Phase::begin("mft.enumerate");
    let mut resolver = PathResolver::with_capacity("C:", 300_000);
    let mut total_records: u64 = 0;
    let mut total_dirs: u64 = 0;
    let mut dir_entries: Vec<(u64, Option<i64>)> = Vec::with_capacity(300_000);
    let mut file_entries: Vec<MftFileEntry> = Vec::with_capacity(2_500_000);
    let mut pass1_last_emit = Instant::now();

    enumerate_mft(vol.raw(), &state.cancel_index, |record| {
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
            if let Some(app) = app {
                emit_index_progress(app, 0, 0, msg);
            }
            pass1_last_emit = Instant::now();
        }
    })?;

    enumerate_phase.finish(format!(
        "records={total_records} directories={total_dirs} files={}",
        file_entries.len()
    ));
    set_phase(state, "Resolving file paths");
    let resolve_phase = crate::diag::Phase::begin("mft.resolve_paths");

    eprintln!(
        "[win/mft +{}] pass1 done: {} records ({} dirs + {} files) in {}ms",
        ts(),
        total_records,
        total_dirs,
        file_entries.len(),
        pass1_started.elapsed().as_millis()
    );

    // ── Pass 1.5: Compute effective ignore rules + collect home subtree dirs ──
    let subtree_started = Instant::now();
    let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(state);

    // Build enhanced skip names: BUILTIN_SKIP_NAMES + AnySegment patterns from pathignore
    let extra_segment_names: Vec<String> = ignored_patterns
        .iter()
        .filter_map(|p| {
            if let IgnorePattern::AnySegment { segment, .. } = p {
                Some(segment.clone())
            } else {
                None
            }
        })
        .collect();
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
                "[win/mft +{}] pass1.5: scan_root FRN={} dir_subtree={} dirs \
                 (pruned from {} total dirs) skip_names={} skip_frns={} in {}ms",
                ts(),
                frn,
                subtree.len(),
                total_dirs,
                all_skip_names.len(),
                skip_frns.len(),
                subtree_started.elapsed().as_millis()
            );
            subtree
        }
        None => {
            eprintln!(
                "[win/mft +{}] pass1.5: scan_root not found in MFT ({}), using all dirs",
                ts(),
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
        "[win/mft +{}] pre-resolved {} dir paths in {}ms",
        ts(),
        dir_subtree.len(),
        preresolve_started.elapsed().as_millis()
    );

    // All subtree dirs are now in path_cache — frn_map no longer needed
    resolver.drop_frn_map();

    // ── Pass 2: Resolve paths, filter, stat → collect into memory ──
    let pass2_started = Instant::now();

    // Get path_cache early — all subtree dirs pre-resolved in pass 1.5
    let path_cache = resolver.path_cache();

    // Pre-extract Glob patterns for pruned-subtree skip checks
    let glob_patterns: Vec<&IgnorePattern> = ignored_patterns
        .iter()
        .filter(|p| matches!(p, IgnorePattern::Glob(_)))
        .collect();

    // Pre-compute backslash versions of BUILTIN_SKIP_PATHS for Windows path matching
    let skip_path_infixes: Vec<String> = crate::BUILTIN_SKIP_PATHS
        .iter()
        .map(|p| format!("\\{}\\", p.replace('/', "\\")))
        .collect();
    let skip_path_suffixes: Vec<String> = crate::BUILTIN_SKIP_PATHS
        .iter()
        .map(|p| format!("\\{}", p.replace('/', "\\")))
        .collect();

    // --- Process directories (parallel, mtime from USN timestamp) ---
    let dir_results: Vec<CompactEntry> = dir_entries
        .par_iter()
        .filter(|(frn, _)| dir_subtree.contains(frn))
        .filter_map(|(frn, timestamp)| {
            let full_path = path_cache.get(frn)?;
            if should_skip_dir_in_pruned_subtree(
                full_path,
                &glob_patterns,
                &skip_path_infixes,
                &skip_path_suffixes,
            ) {
                return None;
            }
            // Extract name/dir from Windows path directly (avoid Path allocations)
            let (dir, name) = match full_path.rfind('\\') {
                Some(pos) => (&full_path[..pos], &full_path[pos + 1..]),
                None => ("", full_path.as_str()),
            };
            if name.is_empty() {
                return None;
            }
            Some(CompactEntry {
                name: name.to_string(),
                dir: Arc::from(dir),
                is_dir: true,
                ext: None,
                mtime: *timestamp,
                size: None,
            })
        })
        .collect();

    let dirs_in_subtree = dir_entries
        .iter()
        .filter(|(frn, _)| dir_subtree.contains(frn))
        .count() as u64;
    let dir_indexed = dir_results.len() as u64;
    let mut scanned: u64 = dirs_in_subtree;
    let mut indexed: u64 = dir_indexed;
    let mut filtered_skip: u64 = dirs_in_subtree - dir_indexed;

    eprintln!(
        "[win/mft +{}] dirs done: scanned={scanned} indexed={indexed} \
         skip={filtered_skip} resolve_fail=0 in {}ms",
        ts(),
        pass2_started.elapsed().as_millis()
    );

    // Progressive Ready is a GUI-only accelerator (partial searcher + early
    // Ready). The service skips it: it has no searcher and reporting Ready
    // mid-scan would mislead the pipe `status` consumers (R5 / D4).
    let early_idx = if build_mem_index {
        eprintln!("[win/mft +{}] building partial MemIndex...", ts());
        let idx = publish_progressive_ready(state, app, &dir_results, scanned, indexed);
        eprintln!(
            "[win/mft +{}] progressive Ready emitted ({indexed} dirs)",
            ts()
        );
        Some(idx)
    } else {
        None
    };

    let mut mem_entries: Vec<CompactEntry> =
        Vec::with_capacity(dir_subtree.len() + file_entries.len());
    mem_entries.extend(dir_results);

    // --- Process files (parallel + stat) ---
    let pass2_files_started = Instant::now();

    // Interned parent paths: sibling files share one Arc<str> per directory
    // instead of each entry holding its own copy of the full parent path.
    let arc_dir_cache: HashMap<u64, Arc<str>> = path_cache
        .iter()
        .map(|(frn, path)| (*frn, Arc::from(path.as_str())))
        .collect();

    let file_results: Vec<CompactEntry> = file_entries
        .par_iter()
        .filter(|entry| dir_subtree.contains(&entry.parent_frn))
        .filter_map(|entry| {
            let parent_path = arc_dir_cache.get(&entry.parent_frn)?;
            if should_skip_file_in_pruned_subtree(parent_path, &entry.name, &glob_patterns) {
                return None;
            }

            let ext = entry.name.rfind('.').and_then(|pos| {
                if pos > 0 && pos < entry.name.len() - 1 {
                    Some(entry.name[pos + 1..].to_lowercase())
                } else {
                    None
                }
            });

            Some(CompactEntry {
                name: entry.name.clone(),
                dir: Arc::clone(parent_path),
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
        "[win/mft +{}] files done: {} files in subtree, indexed={file_indexed} skip={} in {}ms",
        ts(),
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
    drop(file_entries); // ~100MB+
    drop(dir_entries); // ~2MB
    drop(dir_subtree); // ~2MB

    let frn_cache = resolver.into_path_cache(); // also drops resolver.frn_map

    let entry_count = mem_entries.len();
    resolve_phase.finish(format!("entries={entry_count} filtered={filtered_skip}"));
    scan_phase.finish(format!("scanned={scanned} indexed={indexed}"));
    eprintln!(
        "[win/mft +{}] full index ready: indexed={indexed} skip={filtered_skip} \
         entries={entry_count}",
        ts(),
    );

    // Update status counts immediately (files done)
    {
        let mut status = state.status.lock();
        status.scanned = scanned;
        status.indexed = indexed;
        status.entries_count = indexed;
        status.last_updated = Some(now_epoch());
    }
    if let Some(app) = app {
        emit_index_progress(app, scanned, indexed, String::new());
        emit_index_updated(app, indexed, now_epoch(), 0);
    }

    // ── Background: MemIndex build + DB upsert + USN watcher start ──
    let bg_state = state.clone();
    let bg_app = app.cloned();
    let bg_start_journal = start_journal;

    eprintln!(
        "[win/mft +{}] passing {} FRN path entries + {} outside-scan FRNs to USN watcher",
        ts(),
        frn_cache.len(),
        outside_scan_frns.len()
    );

    let bg_started = started;
    let bg_build_mem = build_mem_index;
    std::thread::spawn(move || {
        let ts = || format!("{:.1}s", bg_started.elapsed().as_secs_f32());

        drop(early_idx);

        // GUI builds a searchable MemIndex so the app can search while the DB
        // fills (freed once the DB name index is ready). The service has no
        // searcher (R5), so it upserts straight from the raw entries and never
        // builds or publishes a MemIndex. Exactly one owner is built, so an enum
        // models the choice and ties both variants to a single `drop`.
        enum ScanEntries {
            Searchable(Arc<crate::mem_search::MemIndex>),
            Raw(Vec<CompactEntry>),
        }
        impl ScanEntries {
            fn entries(&self) -> &[CompactEntry] {
                match self {
                    ScanEntries::Searchable(m) => m.entries(),
                    ScanEntries::Raw(v) => v,
                }
            }
        }
        let scan = if bg_build_mem {
            eprintln!(
                "[win/mft/bg +{}] building full MemIndex ({entry_count} entries)...",
                ts()
            );
            let idx = Arc::new(crate::mem_search::MemIndex::build(mem_entries));
            eprintln!("[win/mft/bg +{}] full MemIndex built", ts());
            *bg_state.mem_index.write() = Some(Arc::clone(&idx));
            ScanEntries::Searchable(idx)
        } else {
            ScanEntries::Raw(mem_entries)
        };

        // DB upsert — entries come from whichever owner we built above.
        eprintln!(
            "[win/mft/bg +{}] starting DB upsert ({entry_count} entries)",
            ts()
        );
        let bulk_result = background_db_bulk_insert(&bg_state, scan.entries(), bg_started);

        let bg_ok = match bulk_result {
            Ok((conn, current_run_id)) => {
                if let Err(e) = background_db_finalize(
                    conn,
                    &bg_state,
                    bg_app.as_ref(),
                    bg_start_journal.clone(),
                    current_run_id,
                    entry_count > 0,
                    bg_started,
                    || {
                        // Free the scan's in-memory entries once the DB name
                        // index is ready (the mem_index clear is a no-op for the
                        // service, which never published one).
                        *bg_state.mem_index.write() = None;
                        drop(scan);
                        eprintln!(
                            "[win/mft/bg +{}] scan entries freed (name index ready)",
                            format!("{:.1}s", bg_started.elapsed().as_secs_f32())
                        );
                    },
                ) {
                    crate::diag::log(format!("phase=mft.finalize event=error error={e}"));
                    false
                } else {
                    true
                }
            }
            Err(e) => {
                crate::diag::log(format!("phase=mft.upsert event=error error={e}"));
                *bg_state.mem_index.write() = None;
                drop(scan);
                false
            }
        };

        if bg_ok {
            {
                let mut status = bg_state.status.lock();
                status.state = IndexState::Ready;
                status.message = None;
                status.current_path.clear();
            }
            if let Some(a) = &bg_app {
                emit_index_state(a, "Ready", None);
            }
        } else {
            let msg = "Background DB operation failed".to_string();
            set_state(&bg_state, IndexState::Error, Some(msg.clone()));
            if let Some(a) = &bg_app {
                emit_index_state(a, "Error", Some(msg));
            }
        }
        // Publish the final snapshot before releasing the single-flight flag;
        // the service worker reads it after observing this release.
        bg_state
            .indexing_active
            .store(false, AtomicOrdering::Release);
        crate::diag::log(format!(
            "phase=mft.complete event=end elapsed_ms={} ok={bg_ok}",
            bg_started.elapsed().as_millis()
        ));
        eprintln!("[win/mft/bg +{}] background work done (ok={})", ts(), bg_ok);

        // S5: resume the watcher from the pre-scan journal position so changes
        // made during the scan are replayed (idempotent upserts absorb the
        // overlap). Fall back to the current tip if the pre-scan capture failed.
        let watcher_result = match bg_start_journal {
            Some(journal) => super::usn_watcher::start_at(
                bg_app.clone(),
                bg_state.clone(),
                journal.next_usn,
                journal.journal_id,
                frn_cache,
                outside_scan_frns,
            ),
            None => super::usn_watcher::start(
                bg_app.clone(),
                bg_state.clone(),
                frn_cache,
                outside_scan_frns,
            ),
        };
        if let Err(e) = watcher_result {
            eprintln!(
                "[win/mft/bg +{}] USN watcher failed ({e}), trying RDCW fallback",
                format!("{:.1}s", bg_started.elapsed().as_secs_f32())
            );
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
    state: &AppState,
    entries: &[CompactEntry],
) -> HashMap<String, HashMap<String, (i64, i64)>> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
    };

    // Collect unique parent dirs from file entries
    let unique_dirs: HashSet<&str> = entries
        .iter()
        .filter(|e| !e.is_dir)
        .map(|e| &*e.dir)
        .collect();

    let dir_list: Vec<&str> = unique_dirs.into_iter().collect();
    let completed = std::sync::atomic::AtomicUsize::new(0);
    let progress = parking_lot::Mutex::new(Instant::now());
    let started = Instant::now();

    // Parallel enumeration via rayon
    let results: Vec<(String, HashMap<String, (i64, i64)>)> = dir_list
        .par_iter()
        .filter_map(|&dir_path| {
            let result = (|| {
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

            unsafe {
                let _ = FindClose(handle);
            }
            Some((dir_path.to_string(), dir_map))
            })();
            let done = completed.fetch_add(1,std::sync::atomic::Ordering::Relaxed) + 1;
            if let Some(mut last) = progress.try_lock() {
                if last.elapsed() >= std::time::Duration::from_secs(2) {
                    set_phase(state, format!("Reading file metadata · {done}/{} folders",dir_list.len()));
                    crate::diag::log(format!("phase=mft.read_metadata event=progress elapsed_ms={} directories={done} total={}",started.elapsed().as_millis(),dir_list.len()));
                    *last = Instant::now();
                }
            }
            result
        })
        .collect();

    results.into_iter().collect()
}

/// Extract a file name from WIN32_FIND_DATAW.cFileName (null-terminated UTF-16).
fn wide_name_to_string(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

/// Keep the name-order access path available to the read-only GUI throughout
/// a warm rebuild. Dropping it made a 500-row first page sort millions of rows.
fn set_phase(state: &AppState, message: impl Into<String>) {
    let mut status = state.status.lock();
    status.message = Some(message.into());
    status.current_path.clear();
}

fn prepare_bulk_indexes(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);
        DROP INDEX IF EXISTS idx_entries_dir_ext_name_nocase;
        DROP INDEX IF EXISTS idx_entries_mtime;
        DROP INDEX IF EXISTS idx_entries_ext_name;
    "#,
    )
    .map_err(|error| error.to_string())
}

/// Phase 1: Bulk insert entries into DB.
/// Returns (conn, current_run_id) so the caller can free MemIndex before Phase 2.
fn background_db_bulk_insert(
    state: &AppState,
    entries: &[CompactEntry],
    scan_started: Instant,
) -> Result<(rusqlite::Connection, i64), String> {
    let ts = || format!("{:.1}s", scan_started.elapsed().as_secs_f32());
    let mut conn = db_connection(&state.db_path)?;
    set_indexing_pragmas(&conn)?;

    let last_run_id: i64 = get_meta(&conn, "last_run_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let current_run_id = last_run_id + 1;
    let indexed_at = now_epoch();

    set_phase(state, "Preparing file-name search");
    let name_phase = crate::diag::Phase::begin("mft.prepare_name_index");
    prepare_bulk_indexes(&conn)?;
    name_phase.finish("name_index=available_during_rebuild");

    // Build dir stat cache: batch-retrieve file size+mtime via FindFirstFileW per directory
    let cache_started = Instant::now();
    set_phase(state, "Reading file metadata");
    let metadata_phase = crate::diag::Phase::begin("mft.read_metadata");
    let dir_stat_cache = build_dir_stat_cache(state, entries);
    metadata_phase.finish(format!("directories={}", dir_stat_cache.len()));
    eprintln!(
        "[win/mft/bg +{}] dir_stat_cache built: {} dirs in {}ms",
        ts(),
        dir_stat_cache.len(),
        cache_started.elapsed().as_millis()
    );

    let upsert_started = Instant::now();
    set_phase(state, "Updating file database");
    let upsert_phase = crate::diag::Phase::begin("mft.upsert");
    let mut written = 0usize;
    let mut progress_at = Instant::now();

    for chunk in entries.chunks(MFT_BATCH_SIZE) {
        // S7: cooperative cancellation between batches (service STOP / journal-
        // loss rescan preemption). Err leaves index_complete unset; the caller
        // heals the incomplete build on the next pass.
        if state
            .cancel_index
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("index upsert cancelled".to_string());
        }
        let chunk_rows: Vec<IndexRow> = chunk
            .par_iter()
            .map(|entry| {
                let (size, mtime) = if entry.is_dir {
                    // Dirs: use USN timestamp (already set in mtime)
                    (None, entry.mtime)
                } else if let Some(dir_cache) = dir_stat_cache.get(&*entry.dir) {
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
                    dir: entry.dir.to_string(),
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
        written += chunk_rows.len();
        if progress_at.elapsed() >= std::time::Duration::from_secs(2) {
            set_phase(
                state,
                format!("Updating file database · {written}/{}", entries.len()),
            );
            crate::diag::log(format!(
                "phase=mft.upsert event=progress elapsed_ms={} written={written} total={}",
                upsert_started.elapsed().as_millis(),
                entries.len()
            ));
            progress_at = Instant::now();
        }
    }
    upsert_phase.finish(format!("written={written}"));
    eprintln!(
        "[win/mft/bg +{}] upsert done: {} entries in {}ms",
        ts(),
        entries.len(),
        upsert_started.elapsed().as_millis()
    );

    Ok((conn, current_run_id))
}

/// Phase 2: Cleanup stale rows, recreate indexes, save USN position.
/// `free_mem_index` is called after the primary name index is built, allowing
/// MemIndex to be freed while remaining indexes are created (reduces peak memory).
fn background_db_finalize(
    conn: rusqlite::Connection,
    state: &AppState,
    app: Option<&EventSink>,
    start_journal: Option<volume::UsnJournalData>,
    current_run_id: i64,
    has_entries: bool,
    scan_started: Instant,
    free_mem_index: impl FnOnce(),
) -> Result<(), String> {
    let ts = || format!("{:.1}s", scan_started.elapsed().as_secs_f32());

    let finalize_phase = crate::diag::Phase::begin("mft.finalize");
    set_phase(state, "Removing stale file records");
    let cleanup_phase = crate::diag::Phase::begin("mft.cleanup");

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
    eprintln!(
        "[win/mft/bg +{}] cleanup: deleted={deleted_count} in {}ms",
        ts(),
        cleanup_started.elapsed().as_millis()
    );

    set_meta(&conn, "last_run_id", &current_run_id.to_string())?;
    cleanup_phase.finish(format!("deleted={deleted_count}"));

    // Create the primary name index FIRST — this allows DB search to work
    let idx_started = Instant::now();
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);",
    )
    .map_err(|e| e.to_string())?;
    eprintln!(
        "[win/mft/bg +{}] name index created in {}ms",
        ts(),
        idx_started.elapsed().as_millis()
    );

    // Free MemIndex now — DB search can use the name index
    free_mem_index();

    // Create remaining indexes (MemIndex freed, lower peak memory)
    let idx2_started = Instant::now();
    set_phase(state, "Finishing search indexes");
    let indexes_phase = crate::diag::Phase::begin("mft.secondary_indexes");
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_entries_dir_ext_name_nocase ON entries(dir, ext, name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
        CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);
        "#,
    )
    .map_err(|e| e.to_string())?;
    indexes_phase.finish("search_indexes=ready");
    eprintln!(
        "[win/mft/bg +{}] remaining indexes in {}ms",
        ts(),
        idx2_started.elapsed().as_millis()
    );

    let _ = restore_normal_pragmas(&conn);
    if let Err(e) = cleanup_entries_gc_tables(&conn) {
        eprintln!("[win/mft/bg] gc cleanup error: {e}");
    }

    // S5: persist the PRE-scan journal position captured before enumeration
    // (not a fresh post-scan query), so a restart resumes from before the scan
    // and replays everything that changed during it.
    if let Some(journal) = start_journal {
        let _ = set_meta(&conn, "win_last_usn", &journal.next_usn.to_string());
        let _ = set_meta(&conn, "win_journal_id", &journal.journal_id.to_string());
    }

    // Mark index as complete — startup will check this to decide catchup vs re-index
    let _ = set_meta(&conn, "index_complete", "1");

    if deleted_count > 0 || has_entries {
        invalidate_search_caches(state);
    }

    let (entries_count, last_updated) = update_status_counts(state)?;
    finalize_phase.finish(format!(
        "entries={entries_count} total_pass_ms={}",
        scan_started.elapsed().as_millis()
    ));
    // Persist cached counts for instant startup next time
    let _ = set_meta(&conn, "cached_entries_count", &entries_count.to_string());
    if let Some(lu) = last_updated {
        let _ = set_meta(&conn, "cached_last_updated", &lu.to_string());
    }
    let updated_at = last_updated.unwrap_or_else(now_epoch);
    if let Some(app) = app {
        emit_index_updated(app, entries_count, updated_at, 0);
    }
    let _ = refresh_and_emit_status_counts(app, state);

    // Mirror the macOS finalize: free SQLite's page cache, drop the writer
    // connection, then hand the full-index pass's high-water memory back to the
    // OS (Windows trims the process working set) so the idle service doesn't sit
    // on hundreds of MB left over from the one-time build.
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
    drop(conn);
    crate::release_memory_to_os();

    Ok(())
}

/// Lightweight skip check for directories already in the pruned subtree.
/// Subtree pruning already handles BUILTIN_SKIP_NAMES, ignored_roots, and
/// AnySegment patterns. This only checks BUILTIN_SKIP_PATHS (multi-segment),
/// BUILTIN_SKIP_SUFFIXES, and Glob patterns — with zero heap allocations.
fn should_skip_dir_in_pruned_subtree(
    full_path: &str,
    glob_patterns: &[&IgnorePattern],
    skip_path_infixes: &[String],
    skip_path_suffixes: &[String],
) -> bool {
    // BUILTIN_SKIP_PATHS: pre-computed backslash patterns (no allocation)
    if skip_path_infixes
        .iter()
        .any(|infix| full_path.contains(infix.as_str()))
        || skip_path_suffixes
            .iter()
            .any(|suffix| full_path.ends_with(suffix.as_str()))
    {
        return true;
    }

    // BUILTIN_SKIP_SUFFIXES: check last path segment
    if let Some(last_seg) = full_path.rsplit('\\').next() {
        if crate::BUILTIN_SKIP_SUFFIXES
            .iter()
            .any(|suf| last_seg.ends_with(suf))
        {
            return true;
        }
    }

    // Glob patterns (need forward-slash normalization)
    if !glob_patterns.is_empty() {
        thread_local! {
            static BUF: RefCell<String> = RefCell::new(String::with_capacity(512));
        }
        return BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            buf.clear();
            buf.push_str(full_path);
            // SAFETY: replacing ASCII '\' (0x5C) with '/' (0x2F) preserves UTF-8 validity.
            unsafe {
                buf.as_bytes_mut().iter_mut().for_each(|b| {
                    if *b == b'\\' {
                        *b = b'/';
                    }
                });
            }
            glob_patterns
                .iter()
                .any(|pat| matches_ignore_pattern(&buf, pat))
        });
    }

    false
}

/// Lightweight skip check for files whose parent directory is already in the
/// pruned subtree. Since subtree pruning already eliminates BUILTIN_SKIP_NAMES,
/// BUILTIN_SKIP_PATHS, ignored_roots, and AnySegment patterns, this only needs
/// to check Glob patterns.
fn should_skip_file_in_pruned_subtree(
    parent_path: &str,
    file_name: &str,
    glob_patterns: &[&IgnorePattern],
) -> bool {
    if glob_patterns.is_empty() {
        return false;
    }

    thread_local! {
        static BUF: RefCell<String> = RefCell::new(String::with_capacity(512));
    }

    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        buf.push_str(parent_path);
        buf.push('\\');
        buf.push_str(file_name);

        // Check Glob patterns only (AnySegment handled by subtree pruning)
        // SAFETY: replacing ASCII '\' (0x5C) with '/' (0x2F) preserves UTF-8 validity.
        unsafe {
            buf.as_bytes_mut().iter_mut().for_each(|b| {
                if *b == b'\\' {
                    *b = b'/';
                }
            });
        }
        for pat in glob_patterns {
            if matches_ignore_pattern(&buf, pat) {
                return true;
            }
        }

        false
    })
}

fn enumerate_mft(
    handle: HANDLE,
    cancel: &AtomicBool,
    mut callback: impl FnMut(MftRecord),
) -> Result<(), String> {
    let mut med = MftEnumDataV0 {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: i64::MAX,
    };

    let mut buffer: Vec<u8> = vec![0u8; 64 * 1024];

    loop {
        // S7: cooperative cancellation — a service STOP or journal-loss rescan
        // can preempt a multi-minute enumeration. Leaving via Err keeps
        // index_complete unset so a preempted scan is never confirmed.
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err("index scan cancelled".to_string());
        }

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

        // S1: only ERROR_HANDLE_EOF means the enumeration finished. Any other
        // failure must propagate as Err so a partial scan is never confirmed
        // (index_complete stays unset and the stale-row delete phase is skipped
        // by the early return up the call chain).
        if let Err(e) = &result {
            if e.code() == ERROR_HANDLE_EOF.to_hresult() {
                break;
            }
            return Err(format!("FSCTL_ENUM_USN_DATA failed: {e}"));
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
    let parent_frn = u64::from_le_bytes(data[16..24].try_into().ok()?) & 0x0000_FFFF_FFFF_FFFF;

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

#[cfg(test)]
mod search_availability_tests {
    use super::*;

    #[test]
    fn rebuilding_keeps_global_name_order_on_the_indexed_read_path() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::CREATE_ENTRIES_TABLE_SQL).unwrap();
        conn.execute_batch(crate::CREATE_ENTRIES_INDEXES_SQL)
            .unwrap();
        conn.execute_batch("INSERT INTO entries(path,name,dir,is_dir,indexed_at) VALUES ('c','charlie','',0,0),('a','Alpha','',0,0),('b','beta','',0,0);").unwrap();
        prepare_bulk_indexes(&conn).unwrap();
        let plan: Vec<String> = conn.prepare("EXPLAIN QUERY PLAN SELECT path,name,dir,is_dir,ext,size,mtime FROM entries ORDER BY name COLLATE NOCASE LIMIT 2").unwrap()
            .query_map([],|row|row.get(3)).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_entries_name_nocase")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|step| step.contains("TEMP B-TREE")),
            "{plan:?}"
        );
        let query = || {
            conn.prepare("SELECT name FROM entries ORDER BY name COLLATE NOCASE LIMIT 2")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(query(), vec!["Alpha", "beta"]);
        conn.execute(
            "INSERT INTO entries(path,name,dir,is_dir,indexed_at) VALUES ('0','aardvark','',0,0)",
            [],
        )
        .unwrap();
        assert_eq!(
            query(),
            vec!["aardvark", "Alpha"],
            "new rows remain globally sorted during the rebuild"
        );

        // Recovery from an interrupted older build also recreates the name
        // index before metadata collection and row upserts begin.
        conn.execute_batch("DROP INDEX idx_entries_name_nocase")
            .unwrap();
        prepare_bulk_indexes(&conn).unwrap();
        assert_eq!(query(), vec!["aardvark", "Alpha"]);
        assert!(conn.prepare("SELECT name FROM entries INDEXED BY idx_entries_name_nocase ORDER BY name COLLATE NOCASE LIMIT 2").is_ok());
    }
}
