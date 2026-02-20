pub mod volume;
pub mod path_resolver;
pub mod mft_indexer;
pub mod usn_watcher;
pub mod rdcw_watcher;
pub mod context_menu;
pub mod search_catchup;

use tauri::AppHandle;

use std::sync::atomic::Ordering as AtomicOrdering;

use crate::{
    db_connection, get_meta,
    refresh_and_emit_status_counts, set_ready_with_cached_counts,
    start_full_index_worker, start_full_index_worker_silent,
    AppState,
};
use std::collections::{HashMap, HashSet};

pub fn start_windows_indexing(app: AppHandle, state: AppState) {
    std::thread::spawn(move || {
        let win_started = std::time::Instant::now();
        eprintln!("[startup/win] start_windows_indexing entered");
        let conn = match db_connection(&state.db_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[win] DB connection failed: {e}");
                let _ = start_full_index_worker(app, state);
                return;
            }
        };

        let stored_usn = get_meta(&conn, "win_last_usn")
            .and_then(|v| v.parse::<i64>().ok());
        let stored_journal_id = get_meta(&conn, "win_journal_id")
            .and_then(|v| v.parse::<u64>().ok());
        let index_complete = get_meta(&conn, "index_complete")
            .map(|v| v == "1")
            .unwrap_or(false);

        drop(conn);

        eprintln!(
            "[startup/win] +{}ms startup check: stored_usn={:?} stored_journal_id={:?} index_complete={}",
            win_started.elapsed().as_millis(), stored_usn, stored_journal_id, index_complete
        );

        // Try conditional startup: resume from USN if we have prior state AND index was complete
        if stored_usn.is_some() && stored_journal_id.is_some() && index_complete {
            eprintln!("[startup/win] +{}ms attempting USN resume...", win_started.elapsed().as_millis());
            match usn_watcher::start_with_resume(
                app.clone(),
                state.clone(),
                stored_usn.unwrap(),
                stored_journal_id.unwrap(),
            ) {
                Ok(()) => {
                    eprintln!("[startup/win] +{}ms USN resume succeeded → Ready", win_started.elapsed().as_millis());
                    set_ready_with_cached_counts(&app, &state);
                    return;
                }
                Err(e) => {
                    eprintln!("[win] USN resume failed ({e}), falling back to full index");
                }
            }
        }

        // Full index: try MFT first, then WalkDir fallback
        eprintln!("[startup/win] +{}ms attempting MFT scan...", win_started.elapsed().as_millis());
        match mft_indexer::scan_mft(&state, &app) {
            Ok(result) => {
                eprintln!(
                    "[win] MFT scan SUCCESS — scanned={} indexed={} errors={} \
                     (USN watcher starts after background DB upsert)",
                    result.scanned, result.indexed, result.permission_errors
                );
                // USN watcher is started by the background DB upsert thread
            }
            Err(e) => {
                // scan_mft sets indexing_active=true before failing; reset it.
                state.indexing_active.store(false, AtomicOrdering::Release);

                if index_complete {
                    // DB fully indexed from a previous run.
                    // Go Ready immediately with cached counts, then catchup offline changes in background.
                    eprintln!("[win] MFT failed ({e}), but DB complete — Ready + background catchup");
                    set_ready_with_cached_counts(&app, &state);

                    let catchup_app = app.clone();
                    let catchup_state = state.clone();
                    std::thread::spawn(move || {
                        let catchup_conn = db_connection(&catchup_state.db_path).ok();
                        let last_active_ts = catchup_conn
                            .as_ref()
                            .and_then(|c| get_meta(c, "win_last_active_ts"))
                            .and_then(|v| v.parse::<i64>().ok());
                        drop(catchup_conn);

                        if let Some(ts) = last_active_ts {
                            match search_catchup::run_catchup(&catchup_app, &catchup_state, ts) {
                                Ok(result) => {
                                    eprintln!(
                                        "[win] catchup done: method={} upserted={} deleted={}",
                                        result.method, result.upserted, result.deleted
                                    );
                                }
                                Err(err) => {
                                    eprintln!("[win] catchup failed: {err}");
                                }
                            }
                        } else {
                            eprintln!("[win] no last_active_ts — skipping catchup");
                        }

                        let _ = refresh_and_emit_status_counts(&catchup_app, &catchup_state);
                        // Clear catchup progress message
                        {
                            let mut status = catchup_state.status.lock();
                            status.message = None;
                        }
                        crate::emit_index_state(&catchup_app, "Ready", None);
                    });
                } else {
                    // Check if DB has existing entries — if so, go Ready immediately
                    // and run full index in background
                    let has_entries = db_connection(&state.db_path)
                        .ok()
                        .and_then(|c| {
                            c.query_row(
                                "SELECT EXISTS(SELECT 1 FROM entries LIMIT 1)",
                                [],
                                |row| row.get::<_, bool>(0),
                            )
                            .ok()
                        })
                        .unwrap_or(false);

                    if has_entries {
                        eprintln!("[win] MFT failed ({e}), index incomplete but DB has entries — Ready + background reindex");
                        set_ready_with_cached_counts(&app, &state);
                        let _ = start_full_index_worker_silent(app.clone(), state.clone());
                    } else {
                        eprintln!("[win] MFT failed ({e}), index incomplete, DB empty — starting full index");
                        let _ = start_full_index_worker(app.clone(), state.clone());
                    }
                }

                if let Err(e2) = usn_watcher::start(app.clone(), state.clone(), HashMap::new(), HashSet::new()) {
                    eprintln!("[win] USN watcher also failed ({e2}), trying RDCW fallback");
                    if let Err(e3) = rdcw_watcher::start(app, state) {
                        eprintln!("[win] RDCW watcher also failed ({e3}), no live updates");
                    }
                }
            }
        }
    });
}
