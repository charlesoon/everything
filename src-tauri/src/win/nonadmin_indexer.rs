use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use rusqlite::params;
use tauri::AppHandle;

use crate::mem_search::CompactEntry;
use crate::{
    cleanup_entries_gc_tables, db_connection, effective_ignore_rules, emit_index_progress, emit_index_state,
    emit_index_updated, get_meta, invalidate_search_caches, now_epoch,
    refresh_and_emit_status_counts, restore_normal_pragmas, set_indexing_pragmas,
    set_meta, set_progress, should_skip_path, update_status_counts, upsert_rows,
    AppState, IgnorePattern, IndexRow, IndexState,
};

const JWALK_THREADS: usize = 8;
const EMIT_INTERVAL: Duration = Duration::from_millis(200);
const DB_BATCH_SIZE: usize = 50_000;
/// Shallow scan depth for Phase 1 — captures most user-visible files quickly.
const SHALLOW_DEPTH: usize = 6;

/// Counters returned alongside entries from a scan.
struct ScanResult {
    entries: Vec<CompactEntry>,
    scanned: u64,
    indexed: u64,
    permission_errors: u64,
}

/// Non-admin fast indexing: shallow home scan first for quick search,
/// then deep scan + remaining directories in parallel.
/// Builds MemIndex for instant search while DB writes happen later.
pub fn run_nonadmin_index(app: AppHandle, state: AppState) {
    let started = Instant::now();
    let ts = || format!("{:.1}s", started.elapsed().as_secs_f32());

    eprintln!("[nonadmin +{}] starting non-admin fast index", ts());

    if state
        .indexing_active
        .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
        .is_err()
    {
        eprintln!("[nonadmin] already indexing, skipping");
        return;
    }

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
    emit_index_state(&app, "Indexing", None);

    // Build ignore rules
    let (ignored_roots, ignored_patterns) = effective_ignore_rules(
        &state.config_file_path,
        &state.home_dir,
        &state.cwd,
        state.path_ignores.as_ref(),
        state.path_ignore_patterns.as_ref(),
    );

    let arc_roots = Arc::new(ignored_roots);
    let arc_patterns = Arc::new(ignored_patterns);

    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut permission_errors: u64 = 0;

    // ── Phase 1: Shallow scan of home directory (depth ≤ SHALLOW_DEPTH) ──
    eprintln!(
        "[nonadmin +{}] Phase 1: shallow scan {} (depth≤{})",
        ts(),
        state.home_dir.display(),
        SHALLOW_DEPTH
    );

    let shallow = scan_dir_jwalk(
        &state.home_dir,
        &state,
        &app,
        &arc_roots,
        &arc_patterns,
        Some(SHALLOW_DEPTH),
        0,
        None,
    );
    scanned += shallow.scanned;
    indexed += shallow.indexed;
    permission_errors += shallow.permission_errors;

    eprintln!(
        "[nonadmin +{}] Phase 1 done: {} entries (scanned={} perm_err={})",
        ts(),
        shallow.entries.len(),
        scanned,
        permission_errors
    );

    // Build MemIndex from shallow entries (search works via MemIndex even while Indexing)
    let mut all_entries: Vec<CompactEntry> = Vec::with_capacity(2_000_000);

    if !shallow.entries.is_empty() {
        let early_cap = super::EARLY_MEM_INDEX_LIMIT.min(shallow.entries.len());
        let early_entries: Vec<CompactEntry> =
            shallow.entries.iter().take(early_cap).cloned().collect();
        let early_idx = Arc::new(crate::mem_search::MemIndex::build(early_entries));
        *state.mem_index.write() = Some(early_idx);

        let shallow_total = shallow.entries.len();
        all_entries.extend(shallow.entries);

        {
            let mut status = state.status.lock();
            status.scanned = scanned;
            status.indexed = indexed;
            status.entries_count = indexed;
            status.last_updated = Some(now_epoch());
        }
        emit_index_progress(&app, scanned, indexed, String::new());

        eprintln!(
            "[nonadmin +{}] Phase 1 MemIndex built ({} of {} entries searchable)",
            ts(),
            early_cap,
            shallow_total
        );
    }

    // ── Phase 2: Deep home scan + remaining C:\ roots (in parallel) ──
    eprintln!("[nonadmin +{}] Phase 2: deep scan + remaining roots (parallel)", ts());

    // Capture scan_root itself + direct file children
    if let Some(root_entry) = compact_entry_from_path(&state.scan_root) {
        all_entries.push(root_entry);
        scanned += 1;
        indexed += 1;
    }

    // Enumerate C:\ root children — expand dirs that contain home_dir
    let mut other_roots: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&state.scan_root) {
        for e in entries.flatten() {
            let path = e.path();

            if path == state.home_dir {
                continue;
            }

            if should_skip_path(
                &path,
                &arc_roots,
                &arc_patterns,
            ) {
                continue;
            }

            if !path.is_dir() {
                if let Some(entry) = compact_entry_from_path(&path) {
                    all_entries.push(entry);
                    scanned += 1;
                    indexed += 1;
                }
                continue;
            }

            // If this dir is an ancestor of home_dir (e.g. C:\Users), expand
            // its children to avoid re-scanning the home directory subtree.
            if state.home_dir.starts_with(&path) {
                if let Some(entry) = compact_entry_from_path(&path) {
                    all_entries.push(entry);
                    scanned += 1;
                    indexed += 1;
                }
                expand_children_excluding(
                    &path,
                    &state.home_dir,
                    &arc_roots,
                    &arc_patterns,
                    &mut other_roots,
                );
                continue;
            }

            other_roots.push(path);
        }
    }

    // Sort: commonly searched directories first, noisy system dirs last
    other_roots.sort_by_key(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.eq_ignore_ascii_case("Program Files") {
            0
        } else if name.eq_ignore_ascii_case("Program Files (x86)") {
            1
        } else if name.eq_ignore_ascii_case("ProgramData") {
            2
        } else if name.eq_ignore_ascii_case("Windows") {
            10
        } else {
            3
        }
    });

    // Reuse computed roots for RDCW watcher
    let mut watch_roots: Vec<PathBuf> = vec![state.home_dir.clone()];
    watch_roots.extend(other_roots.iter().cloned());

    // Run deep home scan and C:\ roots scan in PARALLEL
    // Shared progress counters so both threads contribute to one total.
    let shared_progress = Arc::new(SharedProgress {
        scanned: Arc::new(AtomicU64::new(scanned)),
        indexed: Arc::new(AtomicU64::new(indexed)),
    });

    let deep_state = state.clone();
    let deep_app = app.clone();
    let deep_roots = arc_roots.clone();
    let deep_patterns = arc_patterns.clone();
    let deep_home = state.home_dir.clone();
    let deep_sp = Arc::clone(&shared_progress);

    let deep_handle = std::thread::spawn(move || {
        let sp = deep_sp;
        scan_dir_jwalk(
            &deep_home,
            &deep_state,
            &deep_app,
            &deep_roots,
            &deep_patterns,
            None,          // no max_depth
            SHALLOW_DEPTH, // skip shallow entries already in Phase 1
            Some(&sp),
        )
    });

    // Scan other roots on this thread while deep scan runs in parallel
    let mut roots_results: Vec<(String, ScanResult)> = Vec::new();
    for root in &other_roots {
        let root_started = Instant::now();
        let root_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();

        let mut result = scan_dir_jwalk(
            root,
            &state,
            &app,
            &arc_roots,
            &arc_patterns,
            None,
            0,
            Some(&shared_progress),
        );

        // Add root dir itself (also count in shared progress)
        if let Some(entry) = compact_entry_from_path(root) {
            result.entries.push(entry);
            shared_progress.scanned.fetch_add(1, AtomicOrdering::Relaxed);
            shared_progress.indexed.fetch_add(1, AtomicOrdering::Relaxed);
        }

        eprintln!(
            "[nonadmin +{}]   root {} done: {} entries in {}ms",
            ts(),
            root_name,
            result.entries.len(),
            root_started.elapsed().as_millis()
        );
        roots_results.push((root_name, result));
    }

    // Wait for deep home scan to complete
    let deep_result = deep_handle.join().unwrap_or_else(|_| ScanResult {
        entries: Vec::new(),
        scanned: 0,
        indexed: 0,
        permission_errors: 0,
    });

    eprintln!(
        "[nonadmin +{}]   home deep scan: {} new entries",
        ts(),
        deep_result.entries.len()
    );

    // Merge all results — shared counters already have totals
    scanned = shared_progress.scanned.load(AtomicOrdering::Relaxed);
    indexed = shared_progress.indexed.load(AtomicOrdering::Relaxed);
    permission_errors += deep_result.permission_errors;
    all_entries.extend(deep_result.entries);

    for (_name, result) in roots_results {
        permission_errors += result.permission_errors;
        all_entries.extend(result.entries);
    }

    let total_entries = all_entries.len();
    eprintln!(
        "[nonadmin +{}] Phase 2 done: total {} entries (scanned={} perm_err={})",
        ts(),
        total_entries,
        scanned,
        permission_errors
    );

    // Build full MemIndex
    let full_idx = Arc::new(crate::mem_search::MemIndex::build(all_entries));
    *state.mem_index.write() = Some(Arc::clone(&full_idx));

    {
        let mut status = state.status.lock();
        status.state = IndexState::Ready;
        status.scanned = scanned;
        status.indexed = indexed;
        status.entries_count = total_entries as u64;
        status.permission_errors = 0; // expected in non-admin mode, don't surface
        status.message = None;
        status.last_updated = Some(now_epoch());
    }
    invalidate_search_caches(&state);
    emit_index_updated(&app, total_entries as u64, now_epoch(), 0);
    emit_index_state(&app, "Ready", None);

    eprintln!(
        "[nonadmin +{}] full MemIndex built ({} entries), starting background DB persist",
        ts(),
        total_entries
    );

    // ── Phase 3: Background DB persistence ──
    let bg_state = state.clone();
    let bg_app = app.clone();

    std::thread::spawn(move || {
        let watch_roots = watch_roots;
        let ts = || format!("{:.1}s", started.elapsed().as_secs_f32());
        eprintln!(
            "[nonadmin/bg +{}] starting DB bulk insert ({} entries)",
            ts(),
            total_entries
        );

        match background_db_insert(&bg_state, full_idx.entries(), started) {
            Ok((conn, run_id)) => {
                bg_state
                    .indexing_active
                    .store(false, AtomicOrdering::Release);
                eprintln!("[nonadmin/bg +{}] DB upserted, indexing_active=false", ts());

                if let Err(e) = background_db_finalize(
                    conn,
                    &bg_state,
                    &bg_app,
                    run_id,
                    total_entries > 0,
                    started,
                    || {
                        drop(full_idx);
                        *bg_state.mem_index.write() = None;
                        eprintln!(
                            "[nonadmin/bg +{}] MemIndex freed",
                            format!("{:.1}s", started.elapsed().as_secs_f32())
                        );
                    },
                ) {
                    eprintln!("[nonadmin/bg +{}] DB finalize error: {e}", ts());
                }
                eprintln!("[nonadmin/bg +{}] background work done", ts());
            }
            Err(e) => {
                eprintln!("[nonadmin/bg +{}] DB bulk insert error: {e}", ts());
                drop(full_idx);
                *bg_state.mem_index.write() = None;
                bg_state
                    .indexing_active
                    .store(false, AtomicOrdering::Release);
            }
        }

        // Start RDCW file watcher on indexed roots only (not all of C:\)
        if let Err(e) = super::rdcw_watcher::start_with_roots(bg_app, bg_state, watch_roots) {
            eprintln!("[nonadmin/bg] RDCW watcher failed: {e}");
        }
    });
}

/// Returns true if the path is a reparse point (junction or symlink).
/// These cannot be reliably watched by ReadDirectoryChangesW and would
/// duplicate coverage of their targets.
fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(path)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

/// Expand a directory into its children, excluding the subtree rooted at
/// `exclude`. Used to avoid re-scanning the home directory when walking
/// its parent (e.g. C:\Users).
fn expand_children_excluding(
    parent: &Path,
    exclude: &Path,
    ignored_roots: &Arc<Vec<PathBuf>>,
    ignored_patterns: &Arc<Vec<IgnorePattern>>,
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        let child = e.path();
        if child == exclude || exclude.starts_with(&child) {
            continue;
        }
        if !child.is_dir() || is_reparse_point(&child) {
            continue;
        }
        if should_skip_path(
            &child,
            ignored_roots,
            ignored_patterns,
        ) {
            continue;
        }
        out.push(child);
    }
}

/// Compute the list of directories to watch for filesystem changes.
/// Returns home_dir + top-level dirs under scan_root (expanding ancestors of home_dir).
pub fn compute_watch_roots(state: &AppState) -> Vec<PathBuf> {
    let (ignored_roots, ignored_patterns) = effective_ignore_rules(
        &state.config_file_path,
        &state.home_dir,
        &state.cwd,
        state.path_ignores.as_ref(),
        state.path_ignore_patterns.as_ref(),
    );
    let arc_roots = Arc::new(ignored_roots);
    let arc_patterns = Arc::new(ignored_patterns);

    let mut roots: Vec<PathBuf> = vec![state.home_dir.clone()];

    if let Ok(entries) = std::fs::read_dir(&state.scan_root) {
        for e in entries.flatten() {
            let path = e.path();
            if path == state.home_dir || !path.is_dir() || is_reparse_point(&path) {
                continue;
            }
            if should_skip_path(&path, &arc_roots, &arc_patterns) {
                continue;
            }
            if state.home_dir.starts_with(&path) {
                // Ancestor of home_dir (e.g. C:\Users) — expand children excluding home
                expand_children_excluding(
                    &path,
                    &state.home_dir,
                    &arc_roots,
                    &arc_patterns,
                    &mut roots,
                );
            } else {
                roots.push(path);
            }
        }
    }
    roots
}

/// Shared progress counters for parallel Phase 2 scans.
struct SharedProgress {
    scanned: Arc<AtomicU64>,
    indexed: Arc<AtomicU64>,
}

/// Scan a directory with jwalk, returning entries and counters.
///
/// - `max_depth`: limit recursion depth (None = unlimited)
/// - `skip_depth`: skip entries at depth ≤ this value (0 = only skip root)
/// - `shared`: shared atomic counters for progress emission (parallel scans)
fn scan_dir_jwalk(
    root: &Path,
    state: &AppState,
    app: &AppHandle,
    ignored_roots: &Arc<Vec<PathBuf>>,
    ignored_patterns: &Arc<Vec<IgnorePattern>>,
    max_depth: Option<usize>,
    skip_depth: usize,
    shared: Option<&SharedProgress>,
) -> ScanResult {
    let mut entries = Vec::with_capacity(100_000);
    let mut last_emit = Instant::now();
    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut permission_errors: u64 = 0;

    let skip_roots = Arc::clone(ignored_roots);
    let skip_patterns = Arc::clone(ignored_patterns);

    let mut builder = jwalk::WalkDir::new(root)
        .follow_links(false)
        .skip_hidden(false);
    if let Some(md) = max_depth {
        builder = builder.max_depth(md);
    }
    let walker = builder
        .parallelism(jwalk::Parallelism::RayonNewPool(JWALK_THREADS))
        .process_read_dir(move |_depth, path, _state, children| {
            children.retain(|entry_result| {
                entry_result
                    .as_ref()
                    .map(|entry| {
                        let full_path = path.join(&entry.file_name);
                        !should_skip_path(
                            &full_path,
                            &skip_roots,
                            &skip_patterns,
                        )
                    })
                    .unwrap_or(false)
            });
        });

    for result in walker {
        match result {
            Ok(entry) => {
                if entry.depth <= skip_depth {
                    continue;
                }

                scanned += 1;

                let metadata = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => {
                        permission_errors += 1;
                        continue;
                    }
                };

                let path = entry.path();
                let is_dir = metadata.is_dir();
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };
                let dir = path
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();

                let ext = if !is_dir {
                    path.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.to_lowercase())
                } else {
                    None
                };

                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64);

                let size = if metadata.is_file() {
                    Some(metadata.len() as i64)
                } else {
                    None
                };

                entries.push(CompactEntry {
                    name,
                    dir,
                    is_dir,
                    ext,
                    mtime,
                    size,
                });
                indexed += 1;

                if last_emit.elapsed() >= EMIT_INTERVAL {
                    let current = path.to_string_lossy().to_string();
                    if let Some(sp) = shared {
                        // Parallel mode: increment shared counters, emit totals
                        let total_scanned = sp.scanned.fetch_add(scanned, AtomicOrdering::Relaxed) + scanned;
                        let total_indexed = sp.indexed.fetch_add(indexed, AtomicOrdering::Relaxed) + indexed;
                        scanned = 0;
                        indexed = 0;
                        set_progress(state, total_scanned, total_indexed, &current);
                        emit_index_progress(app, total_scanned, total_indexed, current);
                    } else {
                        set_progress(state, scanned, indexed, &current);
                        emit_index_progress(app, scanned, indexed, current);
                    }
                    last_emit = Instant::now();
                }
            }
            Err(_) => {
                scanned += 1;
                permission_errors += 1;
            }
        }
    }

    // Flush remaining local counts to shared counters
    if let Some(sp) = shared {
        sp.scanned.fetch_add(scanned, AtomicOrdering::Relaxed);
        sp.indexed.fetch_add(indexed, AtomicOrdering::Relaxed);
    }

    ScanResult {
        entries,
        scanned,
        indexed,
        permission_errors,
    }
}

fn compact_entry_from_path(path: &Path) -> Option<CompactEntry> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let is_dir = metadata.is_dir();
    let name = path.file_name()?.to_str()?.to_string();
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let ext = if !is_dir {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
    } else {
        None
    };

    let mtime = metadata
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);

    let size = if metadata.is_file() {
        Some(metadata.len() as i64)
    } else {
        None
    };

    Some(CompactEntry {
        name,
        dir,
        is_dir,
        ext,
        mtime,
        size,
    })
}

/// Bulk insert entries into SQLite. Returns (connection, run_id) for finalize.
fn background_db_insert(
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

    let _ = conn.execute_batch(
        r#"
        DROP INDEX IF EXISTS idx_entries_dir_ext_name_nocase;
        DROP INDEX IF EXISTS idx_entries_mtime;
        DROP INDEX IF EXISTS idx_entries_name_nocase;
        DROP INDEX IF EXISTS idx_entries_ext_name;
        "#,
    );
    eprintln!("[nonadmin/bg +{}] indexes dropped", ts());

    let upsert_started = Instant::now();

    for chunk in entries.chunks(DB_BATCH_SIZE) {
        let chunk_rows: Vec<IndexRow> = chunk
            .iter()
            .map(|entry| IndexRow {
                path: entry.path(),
                name: entry.name.clone(),
                dir: entry.dir.clone(),
                is_dir: if entry.is_dir { 1 } else { 0 },
                ext: entry.ext.clone(),
                mtime: entry.mtime,
                size: entry.size,
                indexed_at,
                run_id: current_run_id,
            })
            .collect();
        upsert_rows(&mut conn, &chunk_rows)?;
    }
    eprintln!(
        "[nonadmin/bg +{}] upsert done: {} entries in {}ms",
        ts(),
        entries.len(),
        upsert_started.elapsed().as_millis()
    );

    Ok((conn, current_run_id))
}

/// Cleanup stale rows, recreate indexes, mark index complete.
fn background_db_finalize(
    conn: rusqlite::Connection,
    state: &AppState,
    app: &AppHandle,
    current_run_id: i64,
    has_entries: bool,
    scan_started: Instant,
    free_mem_index: impl FnOnce(),
) -> Result<(), String> {
    let ts = || format!("{:.1}s", scan_started.elapsed().as_secs_f32());

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
        "[nonadmin/bg +{}] cleanup: deleted={} in {}ms",
        ts(),
        deleted_count,
        cleanup_started.elapsed().as_millis()
    );

    set_meta(&conn, "last_run_id", &current_run_id.to_string())?;

    let idx_started = Instant::now();
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);",
    )
    .map_err(|e| e.to_string())?;
    eprintln!(
        "[nonadmin/bg +{}] name index created in {}ms",
        ts(),
        idx_started.elapsed().as_millis()
    );

    free_mem_index();

    let idx2_started = Instant::now();
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_entries_dir_ext_name_nocase ON entries(dir, ext, name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
        CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);
        "#,
    )
    .map_err(|e| e.to_string())?;
    eprintln!(
        "[nonadmin/bg +{}] remaining indexes in {}ms",
        ts(),
        idx2_started.elapsed().as_millis()
    );

    let _ = conn.execute_batch("ANALYZE");
    let _ = restore_normal_pragmas(&conn);
    if let Err(e) = cleanup_entries_gc_tables(&conn) {
        eprintln!("[nonadmin/bg] gc cleanup error: {e}");
    }

    let _ = set_meta(&conn, "index_complete", "1");
    let _ = set_meta(&conn, "win_last_active_ts", &now_epoch().to_string());

    if deleted_count > 0 || has_entries {
        invalidate_search_caches(state);
    }

    let (entries_count, last_updated) = update_status_counts(state)?;
    let _ = set_meta(&conn, "cached_entries_count", &entries_count.to_string());
    if let Some(lu) = last_updated {
        let _ = set_meta(&conn, "cached_last_updated", &lu.to_string());
    }
    let updated_at = last_updated.unwrap_or_else(now_epoch);
    emit_index_updated(app, entries_count, updated_at, 0);
    let _ = refresh_and_emit_status_counts(app, state);

    Ok(())
}
