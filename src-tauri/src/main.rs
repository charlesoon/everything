#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        mpsc::{self, RecvTimeoutError},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
#[cfg(target_os = "macos")]
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};
use walkdir::WalkDir;

const DEFAULT_LIMIT: u32 = 300;
const SHORT_QUERY_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;
const BATCH_SIZE: usize = 4_000;
const RECENT_OP_TTL: Duration = Duration::from_secs(2);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);

const BLANK_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 8, 153, 99, 248, 15, 4, 0, 9, 251, 3, 253,
    167, 111, 89, 83, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

type AppResult<T> = Result<T, String>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EntryDto {
    path: String,
    name: String,
    dir: String,
    is_dir: bool,
    ext: Option<String>,
    mtime: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexStatusDto {
    state: String,
    entries_count: u64,
    last_updated: Option<i64>,
    permission_errors: u64,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexProgressEvent {
    scanned: u64,
    indexed: u64,
    current_path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexStateEvent {
    state: String,
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexUpdatedEvent {
    entries_count: u64,
    last_updated: i64,
    permission_errors: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveSearchUpdatedEvent {
    query: String,
}

#[derive(Debug, Clone)]
enum IndexState {
    Ready,
    Indexing,
    Error,
}

impl IndexState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Indexing => "Indexing",
            Self::Error => "Error",
        }
    }
}

#[derive(Debug, Clone)]
struct IndexStatus {
    state: IndexState,
    entries_count: u64,
    last_updated: Option<i64>,
    permission_errors: u64,
    message: Option<String>,
}

impl Default for IndexStatus {
    fn default() -> Self {
        Self {
            state: IndexState::Ready,
            entries_count: 0,
            last_updated: None,
            permission_errors: 0,
            message: None,
        }
    }
}

#[derive(Debug, Clone)]
struct RecentOp {
    old_path: Option<String>,
    new_path: Option<String>,
    op_type: &'static str,
    at: Instant,
}

#[derive(Debug, Clone)]
struct AppState {
    db_path: PathBuf,
    status: Arc<Mutex<IndexStatus>>,
    recent_ops: Arc<Mutex<Vec<RecentOp>>>,
    icon_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    live_search_cache: Arc<Mutex<HashMap<String, Vec<EntryDto>>>>,
    live_search_inflight: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug, Clone)]
struct IndexRow {
    path: String,
    name: String,
    dir: String,
    is_dir: i64,
    ext: Option<String>,
    mtime: Option<i64>,
    size: Option<i64>,
    indexed_at: i64,
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn db_connection(db_path: &Path) -> AppResult<Connection> {
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        r#"
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        PRAGMA temp_store=MEMORY;
        PRAGMA busy_timeout=3000;
        "#,
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn init_db(db_path: &Path) -> AppResult<()> {
    let conn = db_connection(db_path)?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS entries (
          id INTEGER PRIMARY KEY,
          path TEXT NOT NULL UNIQUE,
          name TEXT NOT NULL,
          dir TEXT NOT NULL,
          is_dir INTEGER NOT NULL,
          ext TEXT,
          mtime INTEGER,
          size INTEGER,
          indexed_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_entries_dir ON entries(dir);
        CREATE INDEX IF NOT EXISTS idx_entries_name ON entries(name);
        CREATE INDEX IF NOT EXISTS idx_entries_isdir ON entries(is_dir);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);

        CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
          name,
          path,
          content='entries',
          content_rowid='id',
          prefix='2 3 4 5 6'
        );

        CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
          INSERT INTO entries_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
        END;

        CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
          INSERT INTO entries_fts(entries_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path);
        END;

        CREATE TRIGGER IF NOT EXISTS entries_au AFTER UPDATE ON entries BEGIN
          INSERT INTO entries_fts(entries_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path);
          INSERT INTO entries_fts(rowid, name, path) VALUES (new.id, new.name, new.path);
        END;
        "#,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn update_counts(state: &AppState) -> AppResult<(u64, Option<i64>)> {
    let conn = db_connection(&state.db_path)?;

    let entries_count = conn
        .query_row("SELECT COUNT(*) FROM entries", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|e| e.to_string())?
        .max(0) as u64;

    let last_updated = conn
        .query_row("SELECT MAX(indexed_at) FROM entries", [], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .map_err(|e| e.to_string())?;

    Ok((entries_count, last_updated))
}

fn update_status_counts(state: &AppState) -> AppResult<(u64, Option<i64>)> {
    let (entries_count, last_updated) = update_counts(state)?;
    let mut status = state.status.lock();
    status.entries_count = entries_count;
    status.last_updated = last_updated;
    Ok((entries_count, last_updated))
}

fn current_permission_errors(state: &AppState) -> u64 {
    state.status.lock().permission_errors
}

fn normalize_query_token(token: &str) -> Option<String> {
    let normalized = token
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .collect::<String>();

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn build_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter_map(normalize_query_token)
        .map(|tok| format!("(name:{tok}* OR path:{tok}*)"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn sort_clause(sort_by: &str, sort_dir: &str) -> &'static str {
    match (sort_by, sort_dir) {
        ("name", "desc") => "e.name COLLATE NOCASE DESC, e.path COLLATE NOCASE DESC",
        ("mtime", "asc") => "COALESCE(e.mtime, 0) ASC, e.name COLLATE NOCASE ASC",
        ("mtime", "desc") => "COALESCE(e.mtime, 0) DESC, e.name COLLATE NOCASE ASC",
        _ => "e.name COLLATE NOCASE ASC, e.path COLLATE NOCASE ASC",
    }
}

fn should_skip_path(path: &Path) -> bool {
    let s = path.to_string_lossy();

    s.contains("/.git/")
        || s.ends_with("/.git")
        || s.contains("/node_modules/")
        || s.ends_with("/node_modules")
        || s.contains("/Library/Caches/")
        || s.ends_with("/Library/Caches")
        || s.contains("/.Trash/")
        || s.ends_with("/.Trash")
        || s.contains("/.Trashes/")
        || s.ends_with("/.Trashes")
}

fn extension_for(path: &Path, is_dir: bool) -> Option<String> {
    if is_dir {
        return None;
    }

    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
}

fn index_row_from_path(path: &Path) -> Option<IndexRow> {
    let metadata = fs::symlink_metadata(path).ok()?;
    let is_dir = metadata.is_dir();

    let name = path
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .or_else(|| {
            if path == Path::new("/") {
                Some("/".to_string())
            } else {
                None
            }
        })?;

    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string());

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

    Some(IndexRow {
        path: path.to_string_lossy().to_string(),
        name,
        dir,
        is_dir: if is_dir { 1 } else { 0 },
        ext: extension_for(path, is_dir),
        mtime,
        size,
        indexed_at: now_epoch(),
    })
}

fn collect_rows_shallow(dir_path: &Path) -> Vec<IndexRow> {
    let mut rows = Vec::new();

    if let Ok(entries) = fs::read_dir(dir_path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if should_skip_path(&entry_path) {
                continue;
            }
            if let Some(row) = index_row_from_path(&entry_path) {
                rows.push(row);
            }
        }
    }

    rows
}

fn collect_rows_recursive(root: &Path) -> Vec<IndexRow> {
    let mut rows = Vec::new();

    for entry in WalkDir::new(root).follow_links(false).into_iter().flatten() {
        let path = entry.path();
        if should_skip_path(path) {
            continue;
        }
        if let Some(row) = index_row_from_path(path) {
            rows.push(row);
        }
    }

    rows
}

fn entry_from_index_row(row: IndexRow) -> EntryDto {
    EntryDto {
        path: row.path,
        name: row.name,
        dir: row.dir,
        is_dir: row.is_dir == 1,
        ext: row.ext,
        mtime: row.mtime,
    }
}

fn entry_from_path(path: &Path) -> Option<EntryDto> {
    index_row_from_path(path).map(entry_from_index_row)
}

fn sort_entries(entries: &mut Vec<EntryDto>, sort_by: &str, sort_dir: &str) {
    entries.sort_by(|a, b| match sort_by {
        "mtime" => {
            let lhs = a.mtime.unwrap_or(0);
            let rhs = b.mtime.unwrap_or(0);
            let primary = if sort_dir == "desc" {
                rhs.cmp(&lhs)
            } else {
                lhs.cmp(&rhs)
            };

            if primary != Ordering::Equal {
                return primary;
            }

            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.path.to_lowercase().cmp(&b.path.to_lowercase()))
        }
        _ => {
            let lhs_name = a.name.to_lowercase();
            let rhs_name = b.name.to_lowercase();
            let primary = if sort_dir == "desc" {
                rhs_name.cmp(&lhs_name)
            } else {
                lhs_name.cmp(&rhs_name)
            };

            if primary != Ordering::Equal {
                return primary;
            }

            let lhs_path = a.path.to_lowercase();
            let rhs_path = b.path.to_lowercase();
            if sort_dir == "desc" {
                rhs_path.cmp(&lhs_path)
            } else {
                lhs_path.cmp(&rhs_path)
            }
        }
    });
}

fn merge_entries(
    mut primary: Vec<EntryDto>,
    secondary: Vec<EntryDto>,
    limit: usize,
    sort_by: &str,
    sort_dir: &str,
) -> Vec<EntryDto> {
    let mut seen = HashSet::with_capacity(primary.len() + secondary.len());
    for entry in &primary {
        seen.insert(entry.path.clone());
    }

    for entry in secondary {
        if seen.insert(entry.path.clone()) {
            primary.push(entry);
        }
    }

    sort_entries(&mut primary, sort_by, sort_dir);
    if primary.len() > limit {
        primary.truncate(limit);
    }
    primary
}

fn live_scan_with_rg(query: &str, limit: usize) -> AppResult<Vec<EntryDto>> {
    let mut child = Command::new("rg")
        .arg("--files")
        .arg("-uu")
        .arg("/")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "rg stdout 파이프를 열 수 없습니다.".to_string())?;
    let reader = BufReader::new(stdout);
    let query_lower = query.to_lowercase();

    let mut entries = Vec::with_capacity(limit);
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };

        if !line.to_lowercase().contains(&query_lower) {
            continue;
        }

        let path = PathBuf::from(&line);
        if should_skip_path(&path) {
            continue;
        }

        if let Some(entry) = entry_from_path(&path) {
            entries.push(entry);
            if entries.len() >= limit {
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    Ok(entries)
}

fn live_scan_with_walkdir(query: &str, limit: usize) -> Vec<EntryDto> {
    let query_lower = query.to_lowercase();
    let mut entries = Vec::with_capacity(limit);

    for entry in WalkDir::new("/").follow_links(false).into_iter().flatten() {
        let path = entry.path();
        if should_skip_path(path) {
            continue;
        }

        let path_str = path.to_string_lossy();
        if !path_str.to_lowercase().contains(&query_lower) {
            continue;
        }

        if let Some(item) = entry_from_path(path) {
            entries.push(item);
            if entries.len() >= limit {
                break;
            }
        }
    }

    entries
}

fn start_live_search_worker(
    app: &AppHandle,
    state: &AppState,
    query: String,
    limit: usize,
    sort_by: String,
    sort_dir: String,
) {
    if query.is_empty() || limit == 0 {
        return;
    }

    {
        let mut inflight = state.live_search_inflight.lock();
        if inflight.contains(&query) {
            return;
        }
        inflight.insert(query.clone());
    }

    let app_handle = app.clone();
    let state = state.clone();

    std::thread::spawn(move || {
        let mut entries =
            live_scan_with_rg(&query, limit).unwrap_or_else(|_| live_scan_with_walkdir(&query, limit));
        sort_entries(&mut entries, &sort_by, &sort_dir);
        if entries.len() > limit {
            entries.truncate(limit);
        }

        {
            let mut cache = state.live_search_cache.lock();
            if cache.len() > 120 {
                cache.clear();
            }
            cache.insert(query.clone(), entries);
        }

        {
            let mut inflight = state.live_search_inflight.lock();
            inflight.remove(&query);
        }

        let _ = app_handle.emit(
            "live_search_updated",
            LiveSearchUpdatedEvent {
                query: query.clone(),
            },
        );
    });
}

fn upsert_rows(conn: &mut Connection, rows: &[IndexRow]) -> AppResult<usize> {
    if rows.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        let mut stmt = tx
            .prepare(
                r#"
                INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at)
                VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(path) DO UPDATE SET
                  name = excluded.name,
                  dir = excluded.dir,
                  is_dir = excluded.is_dir,
                  ext = excluded.ext,
                  mtime = excluded.mtime,
                  size = excluded.size,
                  indexed_at = excluded.indexed_at
                "#,
            )
            .map_err(|e| e.to_string())?;

        for row in rows {
            stmt.execute(params![
                row.path,
                row.name,
                row.dir,
                row.is_dir,
                row.ext,
                row.mtime,
                row.size,
                row.indexed_at
            ])
            .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(rows.len())
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn delete_paths(conn: &mut Connection, raw_paths: &[String]) -> AppResult<usize> {
    if raw_paths.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut deleted = 0;

    {
        let mut stmt = tx
            .prepare("DELETE FROM entries WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\\\'")
            .map_err(|e| e.to_string())?;

        for path in raw_paths {
            let normalized = if path == "/" {
                "/".to_string()
            } else {
                path.trim_end_matches('/').to_string()
            };

            if normalized.is_empty() {
                continue;
            }

            let like_prefix = if normalized == "/" {
                "/".to_string()
            } else {
                normalized
            };
            let pattern = if like_prefix == "/" {
                "/%".to_string()
            } else {
                format!("{}/%", escape_like(&like_prefix))
            };

            deleted += stmt
                .execute(params![like_prefix, pattern])
                .map_err(|e| e.to_string())?;
        }
    }

    tx.commit().map_err(|e| e.to_string())?;
    Ok(deleted)
}

fn emit_index_state(app: &AppHandle, state: &str, message: Option<String>) {
    let _ = app.emit(
        "index_state",
        IndexStateEvent {
            state: state.to_string(),
            message,
        },
    );
}

fn emit_index_updated(
    app: &AppHandle,
    entries_count: u64,
    last_updated: i64,
    permission_errors: u64,
) {
    let _ = app.emit(
        "index_updated",
        IndexUpdatedEvent {
            entries_count,
            last_updated,
            permission_errors,
        },
    );
}

fn emit_index_progress(app: &AppHandle, scanned: u64, indexed: u64, current_path: String) {
    let _ = app.emit(
        "index_progress",
        IndexProgressEvent {
            scanned,
            indexed,
            current_path,
        },
    );
}

fn set_state(state: &AppState, next: IndexState, message: Option<String>) {
    let mut status = state.status.lock();
    status.state = next;
    status.message = message;
}

fn emit_status_counts(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let (entries_count, last_updated) = update_status_counts(state)?;
    emit_index_updated(
        app,
        entries_count,
        last_updated.unwrap_or_else(now_epoch),
        current_permission_errors(state),
    );
    Ok(())
}

fn start_full_index_worker(app: AppHandle, state: AppState) -> AppResult<()> {
    {
        let mut status = state.status.lock();
        if matches!(status.state, IndexState::Indexing) {
            return Ok(());
        }
        status.state = IndexState::Indexing;
        status.message = None;
    }

    emit_index_state(&app, "Indexing", None);

    std::thread::spawn(move || {
        let result = run_full_index(&app, &state);
        if let Err(err) = result {
            set_state(&state, IndexState::Error, Some(err.clone()));
            emit_index_state(&app, "Error", Some(err));
        }
    });

    Ok(())
}

fn run_full_index(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let mut conn = db_connection(&state.db_path)?;

    conn.execute("DELETE FROM entries", [])
        .map_err(|e| e.to_string())?;

    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut permission_errors: u64 = 0;
    let mut current_path = String::new();
    let mut batch: Vec<IndexRow> = Vec::with_capacity(BATCH_SIZE);
    let mut last_emit = Instant::now();

    for entry in WalkDir::new("/").follow_links(false).into_iter() {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                scanned += 1;
                current_path = path.to_string_lossy().to_string();

                if should_skip_path(path) {
                    continue;
                }

                if let Some(row) = index_row_from_path(path) {
                    batch.push(row);
                }

                if batch.len() >= BATCH_SIZE {
                    indexed += upsert_rows(&mut conn, &batch)? as u64;
                    batch.clear();
                }

                if last_emit.elapsed() >= Duration::from_millis(200) {
                    emit_index_progress(app, scanned, indexed, current_path.clone());
                    last_emit = Instant::now();
                }
            }
            Err(_) => {
                scanned += 1;
                permission_errors += 1;
            }
        }
    }

    if !batch.is_empty() {
        indexed += upsert_rows(&mut conn, &batch)? as u64;
    }

    let (entries_count, last_updated) = update_status_counts(state)?;
    let updated_at = last_updated.unwrap_or_else(now_epoch);

    {
        let mut status = state.status.lock();
        status.state = IndexState::Ready;
        status.permission_errors = permission_errors;
        status.message = if permission_errors > 0 {
            Some(format!(
                "권한/접근 오류 {}건이 발생했습니다.",
                permission_errors
            ))
        } else {
            None
        };
    }
    emit_index_progress(app, scanned, indexed, current_path);
    emit_index_updated(app, entries_count, updated_at, permission_errors);
    emit_index_state(
        app,
        "Ready",
        if permission_errors > 0 {
            Some(format!("권한/접근 오류 {}건", permission_errors))
        } else {
            None
        },
    );

    Ok(())
}

fn trim_recent_ops(ops: &mut Vec<RecentOp>) {
    let now = Instant::now();
    ops.retain(|op| now.duration_since(op.at) <= RECENT_OP_TTL);
}

fn remember_op(
    state: &AppState,
    op_type: &'static str,
    old_path: Option<String>,
    new_path: Option<String>,
) {
    let mut ops = state.recent_ops.lock();
    trim_recent_ops(&mut ops);
    ops.push(RecentOp {
        old_path,
        new_path,
        op_type,
        at: Instant::now(),
    });
}

fn is_recently_touched(state: &AppState, path: &str) -> bool {
    let mut ops = state.recent_ops.lock();
    trim_recent_ops(&mut ops);

    ops.iter().any(|op| match op.op_type {
        "rename" | "trash" => {
            op.old_path
                .as_deref()
                .map(|old| old == path)
                .unwrap_or(false)
                || op
                    .new_path
                    .as_deref()
                    .map(|new| new == path)
                    .unwrap_or(false)
        }
        _ => false,
    })
}

fn apply_path_changes(state: &AppState, paths: &[PathBuf]) -> AppResult<usize> {
    let mut to_upsert_map: HashMap<String, IndexRow> = HashMap::new();
    let mut to_delete = Vec::new();

    for path in paths {
        if should_skip_path(path) {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        if path_str.is_empty() {
            continue;
        }

        if is_recently_touched(state, &path_str) {
            continue;
        }

        if path.exists() {
            let is_dir = fs::symlink_metadata(path)
                .map(|meta| meta.is_dir())
                .unwrap_or(false);

            if is_dir {
                if let Some(row) = index_row_from_path(path) {
                    to_upsert_map.insert(row.path.clone(), row);
                }

                for row in collect_rows_shallow(path) {
                    to_upsert_map.insert(row.path.clone(), row);
                }
            } else if let Some(row) = index_row_from_path(path) {
                to_upsert_map.insert(row.path.clone(), row);
            }
        } else {
            to_delete.push(path_str);
        }
    }

    let to_upsert = to_upsert_map.into_values().collect::<Vec<_>>();

    if to_upsert.is_empty() && to_delete.is_empty() {
        return Ok(0);
    }

    let mut conn = db_connection(&state.db_path)?;
    let mut changed = 0;

    changed += upsert_rows(&mut conn, &to_upsert)?;
    changed += delete_paths(&mut conn, &to_delete)?;

    Ok(changed)
}

fn process_watcher_paths(app: &AppHandle, state: &AppState, pending: &mut HashSet<PathBuf>) {
    if pending.is_empty() {
        return;
    }

    let mut batch: Vec<PathBuf> = pending.drain().collect();
    batch.sort();

    match apply_path_changes(state, &batch) {
        Ok(changed) => {
            if changed > 0 {
                let _ = emit_status_counts(app, state);
            }
        }
        Err(err) => {
            let mut status = state.status.lock();
            if !matches!(status.state, IndexState::Indexing) {
                status.state = IndexState::Error;
            }
            status.message = Some(format!("watcher 업데이트 실패: {err}"));
            drop(status);
            emit_index_state(app, "Error", Some(format!("watcher 업데이트 실패: {err}")));
        }
    }
}

fn start_watcher_worker(app: AppHandle, state: AppState) {
    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel();

        let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                set_state(
                    &state,
                    IndexState::Error,
                    Some(format!("watcher 초기화 실패: {err}")),
                );
                emit_index_state(&app, "Error", Some(format!("watcher 초기화 실패: {err}")));
                return;
            }
        };

        if let Err(err) = watcher.watch(Path::new("/"), RecursiveMode::Recursive) {
            set_state(
                &state,
                IndexState::Error,
                Some(format!("watcher 시작 실패: {err}")),
            );
            emit_index_state(&app, "Error", Some(format!("watcher 시작 실패: {err}")));
            return;
        }

        let mut pending_paths: HashSet<PathBuf> = HashSet::new();
        let mut deadline: Option<Instant> = None;

        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Ok(event)) => {
                    for path in event.paths {
                        pending_paths.insert(path.clone());
                        if let Some(parent) = path.parent() {
                            pending_paths.insert(parent.to_path_buf());
                        }
                    }
                    deadline = Some(Instant::now() + WATCH_DEBOUNCE);
                }
                Ok(Err(_)) => {
                    // 개별 이벤트 오류는 무시하고 다음 배치를 계속 처리합니다.
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if let Some(due) = deadline {
                if Instant::now() >= due {
                    process_watcher_paths(&app, &state, &mut pending_paths);
                    deadline = None;
                }
            }
        }
    });
}

fn validate_new_name(new_name: &str) -> AppResult<String> {
    let trimmed = new_name.trim();

    if trimmed.is_empty() {
        return Err("새 이름은 비어 있을 수 없습니다.".to_string());
    }
    if trimmed.contains('/') {
        return Err("새 이름에 '/' 문자를 포함할 수 없습니다.".to_string());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("유효하지 않은 이름입니다.".to_string());
    }

    Ok(trimmed.to_string())
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryDto> {
    Ok(EntryDto {
        path: row.get(0)?,
        name: row.get(1)?,
        dir: row.get(2)?,
        is_dir: row.get::<_, i64>(3)? == 1,
        ext: row.get(4)?,
        mtime: row.get(5)?,
    })
}

#[cfg(target_os = "macos")]
fn safe_file_type(ext: &str) -> String {
    if ext.eq_ignore_ascii_case("folder") {
        return "public.folder".to_string();
    }

    let filtered: String = ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();

    if filtered.is_empty() {
        "public.data".to_string()
    } else {
        filtered
    }
}

#[cfg(target_os = "macos")]
fn load_system_icon_png(ext: &str) -> Option<Vec<u8>> {
    let file_type = safe_file_type(ext);
    let script = format!(
        r#"import AppKit
import Foundation
let image = NSWorkspace.shared.icon(forFileType: \"{file_type}\")
image.size = NSSize(width: 16, height: 16)
if let tiff = image.tiffRepresentation,
   let rep = NSBitmapImageRep(data: tiff),
   let png = rep.representation(using: .png, properties: [:]) {{
  FileHandle.standardOutput.write(png)
}} else {{
  exit(1)
}}
"#
    );

    let output = Command::new("swift").arg("-e").arg(script).output().ok()?;
    if output.status.success() && !output.stdout.is_empty() {
        Some(output.stdout)
    } else {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn load_system_icon_png(_ext: &str) -> Option<Vec<u8>> {
    None
}

#[tauri::command]
fn get_index_status(state: State<'_, AppState>) -> IndexStatusDto {
    let snapshot = state.status.lock().clone();
    IndexStatusDto {
        state: snapshot.state.as_str().to_string(),
        entries_count: snapshot.entries_count,
        last_updated: snapshot.last_updated,
        permission_errors: snapshot.permission_errors,
        message: snapshot.message,
    }
}

#[tauri::command]
fn start_full_index(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    start_full_index_worker(app, state.inner().clone())
}

#[tauri::command]
fn reset_index(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    {
        let status = state.status.lock();
        if matches!(status.state, IndexState::Indexing) {
            return Err("인덱싱 진행 중에는 reset 할 수 없습니다.".to_string());
        }
    }

    let conn = db_connection(&state.db_path)?;
    conn.execute("DELETE FROM entries", [])
        .map_err(|e| e.to_string())?;

    {
        let mut status = state.status.lock();
        status.entries_count = 0;
        status.last_updated = None;
        status.permission_errors = 0;
        status.message = None;
    }

    state.live_search_cache.lock().clear();
    state.live_search_inflight.lock().clear();

    emit_index_updated(&app, 0, now_epoch(), 0);
    start_full_index_worker(app, state.inner().clone())
}

#[tauri::command]
fn search(
    app: AppHandle,
    query: String,
    limit: Option<u32>,
    sort_by: Option<String>,
    sort_dir: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<Vec<EntryDto>> {
    let query = query.trim().to_string();
    let base_limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let effective_limit = if query.chars().count() <= 1 {
        base_limit.min(SHORT_QUERY_LIMIT)
    } else {
        base_limit
    };

    let sort_by = sort_by.unwrap_or_else(|| "name".to_string());
    let sort_dir = sort_dir.unwrap_or_else(|| "asc".to_string());
    let order_by = sort_clause(&sort_by, &sort_dir);

    let conn = db_connection(&state.db_path)?;
    let mut results = Vec::with_capacity(effective_limit as usize);

    if query.is_empty() {
        let sql = format!(
            r#"
            SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
            FROM entries e
            ORDER BY {order_by}
            LIMIT ?1
            "#,
        );
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![effective_limit], row_to_entry)
            .map_err(|e| e.to_string())?;
        for row in rows {
            results.push(row.map_err(|e| e.to_string())?);
        }
    } else {
        let fts_query = build_fts_query(&query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        let sql = format!(
            r#"
            SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
            FROM entries_fts f
            JOIN entries e ON e.id = f.rowid
            WHERE entries_fts MATCH ?1
            ORDER BY {order_by}
            LIMIT ?2
            "#,
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![fts_query, effective_limit], row_to_entry)
            .map_err(|e| e.to_string())?;
        for row in rows {
            results.push(row.map_err(|e| e.to_string())?);
        }

        start_live_search_worker(
            &app,
            state.inner(),
            query.clone(),
            effective_limit as usize,
            sort_by.clone(),
            sort_dir.clone(),
        );

        let cached = state
            .live_search_cache
            .lock()
            .get(&query)
            .cloned()
            .unwrap_or_default();

        if !cached.is_empty() {
            return Ok(merge_entries(
                results,
                cached,
                effective_limit as usize,
                &sort_by,
                &sort_dir,
            ));
        }
    }

    Ok(results)
}

#[tauri::command]
fn open(paths: Vec<String>) -> AppResult<()> {
    for path in paths {
        let status = Command::new("open")
            .arg(&path)
            .status()
            .map_err(|e| e.to_string())?;

        if !status.success() {
            return Err(format!("열기 실패: {path}"));
        }
    }

    Ok(())
}

#[tauri::command]
fn open_with(path: String) -> AppResult<()> {
    reveal_in_finder(vec![path])
}

#[tauri::command]
fn reveal_in_finder(paths: Vec<String>) -> AppResult<()> {
    if paths.is_empty() {
        return Ok(());
    }

    if paths.len() == 1 {
        let status = Command::new("open")
            .arg("-R")
            .arg(&paths[0])
            .status()
            .map_err(|e| e.to_string())?;

        if !status.success() {
            return Err(format!("Finder에서 표시 실패: {}", paths[0]));
        }

        return Ok(());
    }

    let mut unique_parents: HashSet<PathBuf> = HashSet::new();
    for path in paths {
        let p = PathBuf::from(path);
        if let Some(parent) = p.parent() {
            unique_parents.insert(parent.to_path_buf());
        }
    }

    for parent in unique_parents {
        let status = Command::new("open")
            .arg(&parent)
            .status()
            .map_err(|e| e.to_string())?;

        if !status.success() {
            return Err(format!("Finder 열기 실패: {}", parent.to_string_lossy()));
        }
    }

    Ok(())
}

#[tauri::command]
fn copy_paths(paths: Vec<String>) -> String {
    paths.join("\n")
}

#[tauri::command]
fn move_to_trash(paths: Vec<String>, app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    let mut deleted_targets = Vec::new();

    for path in &paths {
        trash::delete(path).map_err(|e| e.to_string())?;
        remember_op(state.inner(), "trash", Some(path.clone()), None);
        deleted_targets.push(path.clone());
    }

    let mut conn = db_connection(&state.db_path)?;
    let _ = delete_paths(&mut conn, &deleted_targets)?;

    emit_status_counts(&app, state.inner())?;
    Ok(())
}

#[tauri::command]
fn rename(
    path: String,
    new_name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<EntryDto> {
    let validated_name = validate_new_name(&new_name)?;
    let old_path = PathBuf::from(&path);

    if !old_path.exists() {
        return Err("원본 파일이 존재하지 않습니다.".to_string());
    }

    let parent = old_path
        .parent()
        .ok_or_else(|| "상위 디렉토리를 찾을 수 없습니다.".to_string())?;

    let new_path = parent.join(&validated_name);
    if new_path == old_path {
        return Ok(EntryDto {
            path: path.clone(),
            name: old_path
                .file_name()
                .map(|v| v.to_string_lossy().to_string())
                .unwrap_or_else(|| validated_name.clone()),
            dir: parent.to_string_lossy().to_string(),
            is_dir: old_path.is_dir(),
            ext: extension_for(&old_path, old_path.is_dir()),
            mtime: fs::symlink_metadata(&old_path)
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
        });
    }

    if new_path.exists() {
        return Err("동일한 이름의 파일/폴더가 이미 존재합니다.".to_string());
    }

    let original_is_dir = old_path.is_dir();
    fs::rename(&old_path, &new_path).map_err(|e| e.to_string())?;

    let mut conn = db_connection(&state.db_path)?;
    let _ = delete_paths(&mut conn, &[path.clone()])?;

    if original_is_dir {
        let rows = collect_rows_recursive(&new_path);
        for chunk in rows.chunks(BATCH_SIZE) {
            let _ = upsert_rows(&mut conn, chunk)?;
        }
    } else {
        let row = index_row_from_path(&new_path)
            .ok_or_else(|| "변경된 파일 정보를 읽을 수 없습니다.".to_string())?;
        let _ = upsert_rows(&mut conn, &[row])?;
    }

    remember_op(
        state.inner(),
        "rename",
        Some(old_path.to_string_lossy().to_string()),
        Some(new_path.to_string_lossy().to_string()),
    );

    emit_status_counts(&app, state.inner())?;

    Ok(EntryDto {
        path: new_path.to_string_lossy().to_string(),
        name: validated_name,
        dir: parent.to_string_lossy().to_string(),
        is_dir: original_is_dir,
        ext: extension_for(&new_path, original_is_dir),
        mtime: fs::symlink_metadata(&new_path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
    })
}

#[tauri::command]
fn get_file_icon(ext: String, state: State<'_, AppState>) -> Vec<u8> {
    let key = if ext.trim().is_empty() {
        "__default__".to_string()
    } else {
        ext.to_lowercase()
    };

    if let Some(cached) = state.icon_cache.lock().get(&key).cloned() {
        return cached;
    }

    let icon = load_system_icon_png(&key).unwrap_or_else(|| BLANK_PNG.to_vec());
    state.icon_cache.lock().insert(key, icon.clone());
    icon
}

#[cfg(target_os = "macos")]
fn register_global_shortcut(app: &AppHandle) -> AppResult<()> {
    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyF);

    app.global_shortcut()
        .on_shortcut(shortcut, move |app_handle, _, _| {
            if let Some(window) = app_handle.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            let _ = app_handle.emit("focus_search", ());
        })
        .map_err(|e| format!("글로벌 단축키 등록 실패: {e}"))
}

#[cfg(not(target_os = "macos"))]
fn register_global_shortcut(_app: &AppHandle) -> AppResult<()> {
    Ok(())
}

fn setup_app(app: &mut tauri::App) -> AppResult<()> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app data dir 조회 실패: {e}"))?;
    fs::create_dir_all(&app_data_dir).map_err(|e| e.to_string())?;

    let db_path = app_data_dir.join("index.db");
    init_db(&db_path)?;

    let state = AppState {
        db_path,
        status: Arc::new(Mutex::new(IndexStatus::default())),
        recent_ops: Arc::new(Mutex::new(Vec::new())),
        icon_cache: Arc::new(Mutex::new(HashMap::new())),
        live_search_cache: Arc::new(Mutex::new(HashMap::new())),
        live_search_inflight: Arc::new(Mutex::new(HashSet::new())),
    };

    let (entries_count, last_updated) = update_counts(&state)?;
    {
        let mut status = state.status.lock();
        status.entries_count = entries_count;
        status.last_updated = last_updated;
    }

    app.manage(state.clone());
    register_global_shortcut(&app.handle())?;
    start_watcher_worker(app.handle().clone(), state.clone());
    start_full_index_worker(app.handle().clone(), state)?;

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            setup_app(app).map_err(|e| {
                Box::<dyn std::error::Error>::from(io::Error::new(io::ErrorKind::Other, e))
            })
        })
        .invoke_handler(tauri::generate_handler![
            get_index_status,
            start_full_index,
            reset_index,
            search,
            open,
            open_with,
            reveal_in_finder,
            copy_paths,
            move_to_trash,
            rename,
            get_file_icon
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    run();
}
