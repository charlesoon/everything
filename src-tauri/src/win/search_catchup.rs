use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use rusqlite::Connection;
use tauri::AppHandle;
use walkdir::WalkDir;

use std::time::Duration;

use crate::{
    db_connection, delete_paths, emit_index_state, index_row_from_path_and_metadata,
    invalidate_search_caches, perf_log, refresh_and_emit_status_counts,
    should_skip_path, upsert_rows, AppResult, AppState, BATCH_SIZE,
};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const WSEARCH_TIMEOUT_SECS: u64 = 10;

pub struct CatchupResult {
    pub upserted: usize,
    pub deleted: usize,
    pub method: &'static str,
}

pub fn run_catchup(
    app: &AppHandle,
    state: &AppState,
    last_active_ts: i64,
) -> AppResult<CatchupResult> {
    let scan_root = &state.scan_root;
    let scan_str = scan_root.to_string_lossy().to_string();

    perf_log(format!(
        "[win/catchup] starting catchup, last_active_ts={last_active_ts}, scan_root={scan_str}"
    ));

    if let Some(paths) = try_wsearch_catchup(&scan_str, last_active_ts) {
        perf_log(format!(
            "[win/catchup] WSearch returned {} changed paths",
            paths.len()
        ));
        return apply_wsearch_results(app, state, paths);
    }

    perf_log("[win/catchup] WSearch unavailable, falling back to mtime scan");
    mtime_scan_catchup(app, state, last_active_ts)
}

fn try_wsearch_catchup(scan_root: &str, last_active_ts: i64) -> Option<Vec<String>> {
    // Check if Windows Search service is running
    let sc_output = Command::new("sc")
        .args(["query", "wsearch"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let sc_text = String::from_utf8_lossy(&sc_output.stdout);
    if !sc_text.contains("RUNNING") {
        eprintln!("[win/catchup] WSearch service not running");
        return None;
    }

    // Format timestamp for ADODB query (UTC ISO-8601)
    let ts_str = format_epoch_as_iso(last_active_ts);

    // Validate scan_root contains only safe path characters before embedding in SQL/PowerShell.
    // scan_root is app-controlled (derived from $USERPROFILE), but defense-in-depth.
    if scan_root.chars().any(|c| matches!(c, '"' | ';' | '(' | ')' | '$' | '`' | '&' | '|' | '<' | '>')) {
        eprintln!("[win/catchup] scan_root contains unsafe characters, skipping WSearch");
        return None;
    }
    // Escape backslashes and single-quotes for PowerShell embedding
    let scope_escaped = scan_root.replace('\\', "\\\\").replace('\'', "''");

    let ps_script = format!(
        r#"$conn = New-Object -ComObject ADODB.Connection; $conn.Open('Provider=Search.CollatorDSO;Extended Properties="Application=Windows"'); $rs = $conn.Execute("SELECT System.ItemPathDisplay FROM SystemIndex WHERE System.DateModified > '{ts_str}' AND SCOPE = 'file:{scope_escaped}'"); while (-not $rs.EOF) {{ $rs.Fields.Item('System.ItemPathDisplay').Value; $rs.MoveNext() }}; $rs.Close(); $conn.Close()"#,
    );

    let child = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    // Wait with timeout
    let output = wait_with_timeout(child, WSEARCH_TIMEOUT_SECS)?;

    if !output.status.success() {
        eprintln!("[win/catchup] PowerShell WSearch query failed (exit={})", output.status);
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Some(paths)
}

fn wait_with_timeout(
    child: std::process::Child,
    timeout_secs: u64,
) -> Option<std::process::Output> {
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(Ok(output)) => Some(output),
        Ok(Err(e)) => {
            eprintln!("[win/catchup] WSearch process error: {e}");
            None
        }
        Err(_) => {
            eprintln!("[win/catchup] WSearch query timed out after {timeout_secs}s");
            None
        }
    }
}

fn apply_wsearch_results(
    app: &AppHandle,
    state: &AppState,
    paths: Vec<String>,
) -> AppResult<CatchupResult> {
    let t0 = Instant::now();
    let mut conn = db_connection(&state.db_path)?;

    let mut to_upsert = Vec::new();
    let mut to_delete = Vec::new();

    for path_str in &paths {
        let path = PathBuf::from(path_str);

        if !path.starts_with(&state.scan_root) {
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
                to_delete.push(path_str.clone());
            }
        }
    }

    let mut total_upserted = 0;
    let mut total_deleted = 0;

    for chunk in to_upsert.chunks(BATCH_SIZE) {
        total_upserted += upsert_rows(&mut conn, chunk)?;
    }
    if !to_delete.is_empty() {
        total_deleted += delete_paths(&mut conn, &to_delete)?;
    }

    if total_upserted > 0 || total_deleted > 0 {
        invalidate_search_caches(state);
        let _ = refresh_and_emit_status_counts(app, state);
    }

    perf_log(format!(
        "[win/catchup] WSearch applied: upserted={total_upserted} deleted={total_deleted} in {}ms",
        t0.elapsed().as_millis()
    ));

    Ok(CatchupResult {
        upserted: total_upserted,
        deleted: total_deleted,
        method: "wsearch",
    })
}

fn mtime_scan_catchup(
    app: &AppHandle,
    state: &AppState,
    last_active_ts: i64,
) -> AppResult<CatchupResult> {
    let t0 = Instant::now();
    let scan_root = &state.scan_root;
    let ignores = state.path_ignores.clone();
    let patterns = state.path_ignore_patterns.clone();
    let mut conn = db_connection(&state.db_path)?;

    let mut total_upserted = 0;
    let mut total_deleted = 0;
    let mut dirs_scanned: u64 = 0;
    let mut dirs_changed: u64 = 0;
    let mut last_emit = Instant::now();
    const EMIT_INTERVAL: Duration = Duration::from_millis(200);

    // Walk directories only, pruning ignored subtrees via filter_entry
    let walker = WalkDir::new(scan_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if !e.file_type().is_dir() {
                return false;
            }
            !should_skip_path(e.path(), &ignores, &patterns)
        });

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        dirs_scanned += 1;

        // Emit progress periodically so UI doesn't appear frozen.
        // Use "Ready" state with a message to avoid overriding Ready → Indexing.
        if last_emit.elapsed() >= EMIT_INTERVAL {
            emit_index_state(
                app,
                "Ready",
                Some(format!("Catchup: {dirs_scanned} dirs scanned, {dirs_changed} changed")),
            );
            last_emit = Instant::now();
        }

        // Check if directory mtime is newer than last_active_ts
        let dir_mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if dir_mtime <= last_active_ts {
            continue;
        }

        dirs_changed += 1;

        // Load existing entries for this directory from DB
        let dir_str = path.to_string_lossy().to_string();
        let db_entries = load_entries_in_dir(&conn, &dir_str);

        // Read current directory contents
        let mut disk_entries: HashMap<String, std::fs::Metadata> = HashMap::new();
        if let Ok(read_dir) = std::fs::read_dir(path) {
            for child in read_dir.flatten() {
                let child_path = child.path();
                if should_skip_path(&child_path, &ignores, &patterns) {
                    continue;
                }
                if let Ok(meta) = std::fs::symlink_metadata(&child_path) {
                    let p = child_path.to_string_lossy().to_string();
                    disk_entries.insert(p, meta);
                }
            }
        }

        // Diff: upsert new/changed, delete missing
        let mut to_upsert = Vec::new();
        let mut to_delete = Vec::new();

        for (disk_path, meta) in &disk_entries {
            let disk_mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            let needs_update = match db_entries.get(disk_path) {
                Some(&db_mtime) => disk_mtime != db_mtime,
                None => true,
            };

            if needs_update {
                let p = PathBuf::from(disk_path);
                if let Some(row) = index_row_from_path_and_metadata(&p, meta) {
                    to_upsert.push(row);
                }
            }
        }

        for db_path in db_entries.keys() {
            if !disk_entries.contains_key(db_path) {
                to_delete.push(db_path.clone());
            }
        }

        for chunk in to_upsert.chunks(BATCH_SIZE) {
            total_upserted += upsert_rows(&mut conn, chunk)?;
        }
        if !to_delete.is_empty() {
            total_deleted += delete_paths(&mut conn, &to_delete)?;
        }
    }

    if total_upserted > 0 || total_deleted > 0 {
        invalidate_search_caches(state);
        let _ = refresh_and_emit_status_counts(app, state);
    }

    perf_log(format!(
        "[win/catchup] mtime scan: dirs_scanned={dirs_scanned} dirs_changed={dirs_changed} \
         upserted={total_upserted} deleted={total_deleted} in {}ms",
        t0.elapsed().as_millis()
    ));

    Ok(CatchupResult {
        upserted: total_upserted,
        deleted: total_deleted,
        method: "mtime_scan",
    })
}

fn load_entries_in_dir(conn: &Connection, dir: &str) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    let mut stmt = match conn.prepare("SELECT path, mtime FROM entries WHERE dir = ?1") {
        Ok(s) => s,
        Err(_) => return map,
    };
    let rows = match stmt.query_map([dir], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
    }) {
        Ok(r) => r,
        Err(_) => return map,
    };
    for row in rows.flatten() {
        map.insert(row.0, row.1.unwrap_or(0));
    }
    map
}

fn format_epoch_as_iso(epoch_secs: i64) -> String {
    // Convert Unix epoch to ISO-8601 format for ADODB query
    // Using chrono-free manual conversion
    let secs = epoch_secs as u64;
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;

    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Days from 1970-01-01
    let (year, month, day) = days_to_ymd(days_since_epoch);

    format!(
        "{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02}",
    )
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
