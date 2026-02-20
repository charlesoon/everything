#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::{Mutex, RwLock};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
#[cfg(target_os = "macos")]
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use walkdir::WalkDir;

mod fd_search;
mod gitignore_filter;
#[cfg(target_os = "macos")]
mod mac;
mod mem_search;
mod query;
#[cfg(target_os = "windows")]
mod win;
use fd_search::{FdSearchCache, FdSearchResultDto};
use query::{escape_like, parse_query, SearchMode};

const DEFAULT_LIMIT: u32 = 300;
const SHORT_QUERY_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;
pub(crate) const BATCH_SIZE: usize = 10_000;
const RECENT_OP_TTL: Duration = Duration::from_secs(2);
pub(crate) const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(60);
const NEGATIVE_CACHE_FALLBACK_WINDOW: Duration = Duration::from_millis(550);
const DB_VERSION: i32 = 4;
const DEFERRED_DIR_NAMES: &[&str] = &[
    "Library", ".Trash", ".Trashes",
    // Windows system directories (deferred when scan_root is C:\)
    "Windows", "Program Files", "Program Files (x86)",
    "$Recycle.Bin", "System Volume Information", "Recovery", "PerfLogs",
];
const SHALLOW_SCAN_DEPTH: usize = 6;
const JWALK_NUM_THREADS: usize = 8;

pub(crate) const BUILTIN_SKIP_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    ".Trash",
    ".Trashes",
    ".npm",
    ".cache",
    "CMakeFiles",
    ".qtc_clangd",
    "__pycache__",
    ".gradle",
    "DerivedData",
];

/// Path segments whose name ends with one of these suffixes are skipped.
/// e.g. "*.build" skips Xcode intermediate build directories like "MyApp.build".
pub(crate) const BUILTIN_SKIP_SUFFIXES: &[&str] = &[
    ".build", // Xcode intermediate build dir (MyApp.build, Objects-normal, etc.)
];

pub(crate) const BUILTIN_SKIP_PATHS: &[&str] = &[
    // macOS
    "Library/Caches",
    "Library/Developer/CoreSimulator",
    "Library/Logs",
    // Cross-platform
    ".vscode/extensions",
    // Windows: noisy system directories under AppData
    "AppData/Local/Temp",
    "AppData/Local/Microsoft",
    "AppData/Local/Google",
    "AppData/Local/Packages",
];

type AppResult<T> = Result<T, String>;
static SEARCH_LOG_ENABLED: OnceLock<bool> = OnceLock::new();
static PERF_LOG_ENABLED: OnceLock<bool> = OnceLock::new();
static BENCH_MODE_ENABLED: OnceLock<bool> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDto {
    pub path: String,
    pub name: String,
    pub dir: String,
    pub is_dir: bool,
    pub ext: Option<String>,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexStatusDto {
    state: String,
    entries_count: u64,
    last_updated: Option<i64>,
    permission_errors: u64,
    message: Option<String>,
    scanned: u64,
    indexed: u64,
    current_path: String,
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
    is_catchup: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct IndexUpdatedEvent {
    entries_count: u64,
    last_updated: i64,
    permission_errors: u64,
}

#[derive(Debug, Clone)]
struct SearchExecution {
    query: String,
    sort_by: String,
    sort_dir: String,
    effective_limit: u32,
    offset: u32,
    mode_label: String,
    results: Vec<EntryDto>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResultDto {
    entries: Vec<EntryDto>,
    mode_label: String,
    /// Total number of results matching the query (ignoring LIMIT/OFFSET).
    /// 0 means unknown — frontend should fall back to entries.length.
    total_count: u32,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct BenchCase {
    id: &'static str,
    query: &'static str,
    sort_by: &'static str,
    sort_dir: &'static str,
    limit: u32,
    offset: u32,
    expected_min_results: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchCaseResult {
    id: String,
    query: String,
    mode: String,
    sort_by: String,
    sort_dir: String,
    limit: u32,
    offset: u32,
    elapsed_ms: f64,
    result_count: usize,
    expected_min_results: usize,
    passed: bool,
    top_results: Vec<String>,
    error: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BenchReport {
    run_label: String,
    started_at: i64,
    completed_at: i64,
    home_dir: String,
    db_path: String,
    index_wait_ms: u128,
    index_scanned: u64,
    index_indexed: u64,
    index_entries_count: u64,
    index_permission_errors: u64,
    index_message: Option<String>,
    search_iterations: u32,
    search_results: Vec<BenchCaseResult>,
}

#[derive(Debug, Clone)]
pub(crate) enum IndexState {
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
pub(crate) struct IndexStatus {
    pub(crate) state: IndexState,
    pub(crate) entries_count: u64,
    pub(crate) last_updated: Option<i64>,
    pub(crate) permission_errors: u64,
    pub(crate) message: Option<String>,
    pub(crate) scanned: u64,
    pub(crate) indexed: u64,
    pub(crate) current_path: String,
}

impl Default for IndexStatus {
    fn default() -> Self {
        Self {
            state: IndexState::Indexing,
            entries_count: 0,
            last_updated: None,
            permission_errors: 0,
            message: None,
            scanned: 0,
            indexed: 0,
            current_path: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct RecentOp {
    old_path: Option<String>,
    new_path: Option<String>,
    op_type: &'static str,
    at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IgnorePattern {
    AnySegment(String),
    Glob(String),
}

#[derive(Debug, Clone)]
pub(crate) struct IgnoreRulesCache {
    roots: Vec<PathBuf>,
    patterns: Vec<IgnorePattern>,
    pathignore_mtime: Option<SystemTime>,
    gitignore_mtime: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) db_path: PathBuf,
    pub(crate) home_dir: PathBuf,
    pub(crate) scan_root: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) path_ignores: Arc<Vec<PathBuf>>,
    pub(crate) path_ignore_patterns: Arc<Vec<IgnorePattern>>,
    pub(crate) db_ready: Arc<AtomicBool>,
    pub(crate) indexing_active: Arc<AtomicBool>,
    pub(crate) status: Arc<Mutex<IndexStatus>>,
    pub(crate) recent_ops: Arc<Mutex<Vec<RecentOp>>>,
    pub(crate) icon_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    pub(crate) fd_search_cache: Arc<Mutex<Option<FdSearchCache>>>,
    pub(crate) negative_name_cache: Arc<Mutex<Vec<NegativeNameEntry>>>,
    pub(crate) ignore_cache: Arc<Mutex<Option<IgnoreRulesCache>>>,
    pub(crate) gitignore: Arc<gitignore_filter::LazyGitignoreFilter>,
    /// In-memory index for instant search while DB upsert runs in background.
    /// Set by MFT scan, cleared after background DB upsert completes.
    pub(crate) mem_index: Arc<RwLock<Option<Arc<mem_search::MemIndex>>>>,
    /// Signal to stop the file watcher (RDCW / USN). Set to true on reset_index.
    pub(crate) watcher_stop: Arc<AtomicBool>,
    /// Set to true while a file watcher event loop is running.
    pub(crate) watcher_active: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub(crate) struct NegativeNameEntry {
    query_lower: String,
    created_at: Instant,
    fallback_checked: bool,
}

#[derive(Debug, Clone)]
struct NegativeCacheHit {
    query_lower: String,
    age: Duration,
    fallback_checked: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexRow {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) dir: String,
    pub(crate) is_dir: i64,
    pub(crate) ext: Option<String>,
    pub(crate) mtime: Option<i64>,
    pub(crate) size: Option<i64>,
    pub(crate) indexed_at: i64,
    pub(crate) run_id: i64,
}

pub(crate) fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn bench_mode_enabled() -> bool {
    *BENCH_MODE_ENABLED.get_or_init(|| env_truthy("EVERYTHING_BENCH"))
}

fn perf_log_enabled() -> bool {
    *PERF_LOG_ENABLED.get_or_init(|| env_truthy("FASTFIND_PERF_LOG") || bench_mode_enabled())
}

pub(crate) fn perf_log(message: impl AsRef<str>) {
    if perf_log_enabled() {
        eprintln!("[perf] {}", message.as_ref());
    }
}

fn db_connection_with_timeout(db_path: &Path, busy_timeout_ms: u32) -> AppResult<Connection> {
    let conn = Connection::open(db_path).map_err(|e| e.to_string())?;
    conn.execute_batch(&format!(
        r#"
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        PRAGMA temp_store=MEMORY;
        PRAGMA busy_timeout={busy_timeout_ms};
        "#,
    ))
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

pub(crate) fn db_connection(db_path: &Path) -> AppResult<Connection> {
    db_connection_with_timeout(db_path, 3000)
}

fn db_connection_for_search(db_path: &Path) -> AppResult<Connection> {
    let conn = db_connection_with_timeout(db_path, 200)?;
    conn.execute_batch(
        "PRAGMA cache_size = -32768;
         PRAGMA mmap_size = 268435456;",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

pub(crate) fn set_indexing_pragmas(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(
        r#"
        PRAGMA synchronous = NORMAL;
        PRAGMA cache_size = -65536;
        PRAGMA mmap_size = 268435456;
        PRAGMA wal_autocheckpoint = 0;
        "#,
    )
    .map_err(|e| e.to_string())
}

pub(crate) fn restore_normal_pragmas(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(
        r#"
        PRAGMA synchronous = NORMAL;
        PRAGMA cache_size = -16384;
        PRAGMA mmap_size = 0;
        PRAGMA wal_autocheckpoint = 1000;
        "#,
    )
    .map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn init_db(db_path: &Path) -> AppResult<()> {
    let t = Instant::now();
    let conn = db_connection(db_path)?;
    eprintln!("[init_db] +{}ms db_connection opened", t.elapsed().as_millis());

    let current_version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if current_version != DB_VERSION {
        conn.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS entries_ai;
            DROP TRIGGER IF EXISTS entries_ad;
            DROP TRIGGER IF EXISTS entries_au;
            DROP TABLE IF EXISTS entries_fts;
            DROP TABLE IF EXISTS entries;
            "#,
        )
        .map_err(|e| e.to_string())?;
        conn.execute_batch(&format!("PRAGMA user_version = {};", DB_VERSION))
            .map_err(|e| e.to_string())?;
    }
    eprintln!("[init_db] +{}ms version check done (v={})", t.elapsed().as_millis(), current_version);

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
          indexed_at INTEGER NOT NULL,
          run_id INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_entries_dir_ext_name_nocase ON entries(dir, ext, name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
        CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);

        CREATE TABLE IF NOT EXISTS meta (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        "#,
    )
    .map_err(|e| e.to_string())?;
    eprintln!("[init_db] +{}ms schema ensured", t.elapsed().as_millis());

    let legacy_index_migration_key = "migration_drop_idx_entries_dir_name_nocase_v1";
    let migrated = get_meta(&conn, legacy_index_migration_key)
        .map(|v| v == "1")
        .unwrap_or(false);
    if !migrated {
        conn.execute_batch("DROP INDEX IF EXISTS idx_entries_dir_name_nocase;")
            .map_err(|e| e.to_string())?;
        set_meta(&conn, legacy_index_migration_key, "1")?;
    }
    eprintln!("[init_db] +{}ms total", t.elapsed().as_millis());

    Ok(())
}

pub(crate) fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> AppResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
        params![key, value],
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

pub(crate) fn update_status_counts(state: &AppState) -> AppResult<(u64, Option<i64>)> {
    let (entries_count, last_updated) = update_counts(state)?;
    let mut status = state.status.lock();
    status.entries_count = entries_count;
    status.last_updated = last_updated;
    Ok((entries_count, last_updated))
}

pub(crate) fn invalidate_search_caches(state: &AppState) {
    state.fd_search_cache.lock().take();
    state.negative_name_cache.lock().clear();
}

fn prune_negative_name_cache(cache: &mut Vec<NegativeNameEntry>) {
    let now = Instant::now();
    cache.retain(|entry| now.duration_since(entry.created_at) <= NEGATIVE_CACHE_TTL);
}

fn negative_name_cache_lookup(state: &AppState, query: &str) -> Option<NegativeCacheHit> {
    if query.is_empty() {
        return None;
    }
    let q = query.to_lowercase();
    let now = Instant::now();
    let mut cache = state.negative_name_cache.lock();
    prune_negative_name_cache(&mut cache);
    cache
        .iter()
        .find(|entry| q.contains(&entry.query_lower))
        .map(|entry| NegativeCacheHit {
            query_lower: entry.query_lower.clone(),
            age: now.duration_since(entry.created_at),
            fallback_checked: entry.fallback_checked,
        })
}

fn remember_negative_name_query(state: &AppState, query: &str) {
    if query.is_empty() {
        return;
    }
    let normalized = query.to_lowercase();
    let mut cache = state.negative_name_cache.lock();
    prune_negative_name_cache(&mut cache);

    if cache
        .iter()
        .any(|existing| existing.query_lower == normalized)
    {
        return;
    }

    cache.push(NegativeNameEntry {
        query_lower: normalized,
        created_at: Instant::now(),
        fallback_checked: false,
    });
    const MAX_NEGATIVE_CACHE: usize = 512;
    if cache.len() > MAX_NEGATIVE_CACHE {
        let drop_count = cache.len() - MAX_NEGATIVE_CACHE;
        cache.drain(0..drop_count);
    }
}

fn remove_negative_name_query(state: &AppState, query: &str) {
    if query.is_empty() {
        return;
    }
    let normalized = query.to_lowercase();
    let mut cache = state.negative_name_cache.lock();
    cache.retain(|entry| entry.query_lower != normalized);
}

fn mark_negative_name_fallback_checked(state: &AppState, query_lower: &str) {
    let mut cache = state.negative_name_cache.lock();
    if let Some(entry) = cache
        .iter_mut()
        .find(|entry| entry.query_lower == query_lower)
    {
        entry.fallback_checked = true;
    }
}

fn sort_clause(sort_by: &str, sort_dir: &str, prefix: &str) -> String {
    match (sort_by, sort_dir) {
        ("name", "desc") => {
            format!("{prefix}name COLLATE NOCASE DESC, {prefix}path COLLATE NOCASE DESC")
        }
        ("mtime", "asc") => {
            format!("COALESCE({prefix}mtime, 0) ASC, {prefix}name COLLATE NOCASE ASC")
        }
        ("mtime", "desc") => {
            format!("COALESCE({prefix}mtime, 0) DESC, {prefix}name COLLATE NOCASE ASC")
        }
        ("dir", "asc") => {
            format!("{prefix}dir COLLATE NOCASE ASC, {prefix}name COLLATE NOCASE ASC")
        }
        ("dir", "desc") => {
            format!("{prefix}dir COLLATE NOCASE DESC, {prefix}name COLLATE NOCASE ASC")
        }
        ("size", "asc") => {
            format!("{prefix}size IS NULL ASC, {prefix}size ASC, {prefix}name COLLATE NOCASE ASC")
        }
        ("size", "desc") => {
            format!("{prefix}size IS NULL ASC, {prefix}size DESC, {prefix}name COLLATE NOCASE ASC")
        }
        _ => format!("{prefix}name COLLATE NOCASE ASC, {prefix}path COLLATE NOCASE ASC"),
    }
}

fn contains_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// Detect if a LIKE pattern is a pure extension filter (e.g. `%.rs`).
/// Returns the lowercase extension if matched, None otherwise.
fn extract_ext_from_like(like_pattern: &str) -> Option<String> {
    let rest = like_pattern.strip_prefix('%')?;
    let ext = rest.strip_prefix('.')?;
    if ext.is_empty()
        || ext.contains('%')
        || ext.contains('_')
        || ext.contains('\\')
        || ext.contains('/')
        || ext.contains('.')
    {
        return None;
    }
    Some(ext.to_lowercase())
}

fn resolve_dir_hint(home_dir: &Path, dir_hint: &str) -> Option<PathBuf> {
    if dir_hint.is_empty() || contains_glob_meta(dir_hint) {
        return None;
    }

    let candidate = if dir_hint == "~" {
        home_dir.to_path_buf()
    } else if let Some(rest) = dir_hint.strip_prefix("~/") {
        home_dir.join(rest)
    } else if dir_hint.starts_with('/') {
        PathBuf::from(dir_hint)
    } else {
        home_dir.join(dir_hint)
    };

    if candidate.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

const RESOLVE_DIRS_MAX: usize = 20;

fn resolve_dirs_from_db(conn: &Connection, dir_hint: &str) -> Vec<String> {
    if dir_hint.is_empty() || contains_glob_meta(dir_hint) {
        return Vec::new();
    }
    let last_seg = match dir_hint.rsplit('/').next() {
        Some(s) if !s.is_empty() => s,
        _ => dir_hint,
    };
    let sep = std::path::MAIN_SEPARATOR;
    let native_hint = dir_hint.replace('/', &sep.to_string());
    let escaped_sep = escape_like(&sep.to_string());
    let path_pattern = format!("%{escaped_sep}{}", escape_like(&native_hint));
    let fetch_limit = (RESOLVE_DIRS_MAX + 1) as i64;
    let mut stmt = match conn.prepare(
        "SELECT path FROM entries WHERE name = ?1 COLLATE NOCASE AND is_dir = 1 AND path LIKE ?2 ESCAPE '\\' LIMIT ?3",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map(params![last_seg, path_pattern, fetch_limit], |row| {
        row.get::<_, String>(0)
    }) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let dirs: Vec<String> = rows.filter_map(|r| r.ok()).collect();
    if dirs.len() > RESOLVE_DIRS_MAX {
        return Vec::new();
    }
    dirs
}

fn normalize_slashes(s: String) -> String {
    s.replace('\\', "/")
}

fn resolve_ignore_path(raw: &str, base_dir: &Path, home_dir: &Path) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
        return None;
    }

    let mut path = if trimmed == "~" {
        home_dir.to_path_buf()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home_dir.join(rest)
    } else {
        PathBuf::from(trimmed)
    };

    if !path.is_absolute() {
        path = base_dir.join(path);
    }

    Some(fs::canonicalize(&path).unwrap_or(path))
}

fn normalize_ignore_pattern(raw: &str, base_dir: &Path, home_dir: &Path) -> String {
    let trimmed = raw.trim().trim_end_matches('/').to_string();
    if trimmed == "~" {
        return normalize_slashes(home_dir.to_string_lossy().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return normalize_slashes(home_dir.join(rest).to_string_lossy().to_string());
    }
    if trimmed.starts_with('/') || trimmed.starts_with("**/") {
        return trimmed;
    }
    normalize_slashes(base_dir.join(&trimmed).to_string_lossy().to_string())
}

fn parse_ignore_pattern(raw: &str, base_dir: &Path, home_dir: &Path) -> Option<IgnorePattern> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    if let Some(segment) = trimmed.strip_prefix("**/") {
        if !segment.is_empty() && !segment.contains('/') && !contains_glob_meta(segment) {
            return Some(IgnorePattern::AnySegment(segment.to_string()));
        }
    }

    Some(IgnorePattern::Glob(normalize_ignore_pattern(
        trimmed, base_dir, home_dir,
    )))
}

fn ignore_pattern_key(pattern: &IgnorePattern) -> String {
    match pattern {
        IgnorePattern::AnySegment(segment) => format!("seg:{segment}"),
        IgnorePattern::Glob(glob) => format!("glob:{glob}"),
    }
}

fn find_file_upward(start: &Path, file_name: &str) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let marker = dir.join(".git");
        if marker.is_dir() || marker.is_file() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn load_pathignore_rules(home_dir: &Path, cwd: &Path) -> (Vec<PathBuf>, Vec<IgnorePattern>) {
    let mut files = Vec::new();
    if let Some(file) = find_file_upward(cwd, ".pathignore") {
        files.push(file);
    } else {
        files.push(cwd.join(".pathignore"));
    }
    let home_file = home_dir.join(".pathignore");
    if !files.iter().any(|p| p == &home_file) {
        files.push(home_file);
    }

    let mut roots = Vec::new();
    let mut patterns = Vec::new();
    let mut seen_roots = HashSet::new();
    let mut seen_patterns = HashSet::new();

    for file in files {
        let Ok(contents) = fs::read_to_string(&file) else {
            continue;
        };
        let base_dir = file.parent().unwrap_or_else(|| Path::new("/"));
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                continue;
            }

            if contains_glob_meta(trimmed) {
                let Some(pattern) = parse_ignore_pattern(trimmed, base_dir, home_dir) else {
                    continue;
                };
                let key = ignore_pattern_key(&pattern);
                if seen_patterns.insert(key) {
                    patterns.push(pattern);
                }
                continue;
            }

            let Some(path) = resolve_ignore_path(trimmed, base_dir, home_dir) else {
                continue;
            };

            let key = path.to_string_lossy().to_string();
            if seen_roots.insert(key) {
                roots.push(path);
            }
        }
    }

    (roots, patterns)
}

fn load_gitignore_roots(home_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let gitignore = if let Some(root) = find_git_root(cwd) {
        root.join(".gitignore")
    } else if let Some(file) = find_file_upward(&cwd, ".gitignore") {
        file
    } else {
        return Vec::new();
    };
    let contents = match fs::read_to_string(&gitignore) {
        Ok(contents) => contents,
        Err(_) => return Vec::new(),
    };

    let mut roots = Vec::new();
    let mut seen = HashSet::new();

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
            continue;
        }
        if contains_glob_meta(trimmed) || trimmed.contains('[') {
            continue;
        }

        let normalized = trimmed.trim_start_matches('/').trim_end_matches('/');
        if normalized.is_empty() {
            continue;
        }

        let Some(path) = resolve_ignore_path(normalized, &cwd, home_dir) else {
            continue;
        };

        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            roots.push(path);
        }
    }

    roots
}

#[cfg(target_os = "macos")]
fn macos_tcc_ignore_roots(home_dir: &Path) -> Vec<PathBuf> {
    let library = home_dir.join("Library");
    let app_support = library.join("Application Support");
    let caches = library.join("Caches");

    let dirs: Vec<PathBuf> = vec![
        home_dir.join(".Trash"),
        library.join("Accounts"),
        library.join("AppleMediaServices"),
        library.join("Assistant").join("SiriVocabulary"),
        library.join("Autosave Information"),
        library.join("Biome"),
        library.join("Calendars"),
        library.join("com.apple.aiml.instrumentation"),
        library.join("ContainerManager"),
        library.join("Cookies"),
        library.join("CoreFollowUp"),
        library.join("Daemon Containers"),
        library.join("DoNotDisturb"),
        library.join("DuetExpertCenter"),
        library.join("Group Containers"),
        library.join("HomeKit"),
        library.join("Developer").join("Xcode").join("DerivedData"),
        library.join("IdentityServices"),
        library.join("IntelligencePlatform"),
        library.join("Mail"),
        library.join("Messages"),
        library.join("Metadata").join("CoreSpotlight"),
        library.join("PersonalizationPortrait"),
        library.join("Reminders"),
        library.join("Safari"),
        library.join("Sharing"),
        library.join("Shortcuts"),
        library.join("StatusKit"),
        library.join("Suggestions"),
        library.join("Trial"),
        library.join("Weather"),
        app_support.join("AddressBook"),
        app_support.join("CallHistoryDB"),
        app_support.join("CallHistoryTransactions"),
        app_support.join("CloudDocs"),
        app_support.join("com.apple.avfoundation"),
        app_support.join("com.apple.sharedfilelist"),
        app_support.join("com.apple.TCC"),
        app_support.join("DifferentialPrivacy"),
        app_support.join("FaceTime"),
        app_support.join("FileProvider"),
        app_support.join("Knowledge"),
        caches.join("CloudKit"),
        caches.join("com.apple.ap.adprivacyd"),
        caches.join("com.apple.containermanagerd"),
        caches.join("com.apple.findmy.fmfcore"),
        caches.join("com.apple.findmy.fmipcore"),
        caches.join("com.apple.homed"),
        caches.join("com.apple.HomeKit"),
        caches.join("com.apple.Safari"),
        caches.join("FamilyCircle"),
    ];

    dirs.into_iter().filter(|p| p.exists()).collect()
}

#[cfg(not(target_os = "macos"))]
fn macos_tcc_ignore_roots(_home_dir: &Path) -> Vec<PathBuf> {
    Vec::new()
}

/// Directories under home that generate heavy background I/O on Windows
/// (browser caches, Windows Update temp files, UWP package state, etc.).
/// Pruning these from the MFT subtree eliminates noisy USN records.
#[cfg(target_os = "windows")]
fn windows_noisy_roots(home_dir: &Path) -> Vec<PathBuf> {
    let local = home_dir.join("AppData").join("Local");
    let dirs = vec![
        local.join("Temp"),
        local.join("Microsoft"),
        local.join("Google"),
        local.join("Packages"),
        // C:\ system directories
        PathBuf::from("C:\\Windows"),
        PathBuf::from("C:\\$Recycle.Bin"),
        PathBuf::from("C:\\System Volume Information"),
        PathBuf::from("C:\\Recovery"),
        PathBuf::from("C:\\PerfLogs"),
    ];
    dirs.into_iter().filter(|p| p.exists()).collect()
}

#[cfg(not(target_os = "windows"))]
fn windows_noisy_roots(_home_dir: &Path) -> Vec<PathBuf> {
    Vec::new()
}

fn effective_ignore_rules(
    home_dir: &Path,
    cwd: &Path,
    base_roots: &[PathBuf],
    base_patterns: &[IgnorePattern],
) -> (Vec<PathBuf>, Vec<IgnorePattern>) {
    let mut roots = base_roots.to_vec();
    let mut patterns = base_patterns.to_vec();
    let mut seen: HashSet<String> = roots
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let mut seen_patterns: HashSet<String> = patterns.iter().map(ignore_pattern_key).collect();

    let (pathignore_roots, pathignore_patterns) = load_pathignore_rules(home_dir, cwd);
    for root in pathignore_roots
        .into_iter()
        .chain(load_gitignore_roots(home_dir, cwd).into_iter())
        .chain(macos_tcc_ignore_roots(home_dir).into_iter())
        .chain(windows_noisy_roots(home_dir).into_iter())
    {
        let key = root.to_string_lossy().to_string();
        if seen.insert(key) {
            roots.push(root);
        }
    }

    for pattern in pathignore_patterns
        .into_iter()
        .chain(builtin_ignore_patterns(home_dir))
    {
        let key = ignore_pattern_key(&pattern);
        if seen_patterns.insert(key) {
            patterns.push(pattern);
        }
    }

    (roots, patterns)
}

fn builtin_ignore_patterns(home_dir: &Path) -> Vec<IgnorePattern> {
    let home = normalize_slashes(home_dir.to_string_lossy().to_string());
    vec![
        // ~/Library/Application Support/*/Cache*
        IgnorePattern::Glob(format!("{home}/Library/Application Support/*/Cache*")),
        // ~/Library/Application Support/*/Code Cache
        IgnorePattern::Glob(format!("{home}/Library/Application Support/*/Code Cache")),
        // ~/.rustup/toolchains/*/share/doc
        IgnorePattern::Glob(format!("{home}/.rustup/toolchains/*/share/doc")),
        // ~/.pyenv/versions/*/lib
        IgnorePattern::Glob(format!("{home}/.pyenv/versions/*/lib")),
    ]
}

pub(crate) fn cached_effective_ignore_rules(state: &AppState) -> (Vec<PathBuf>, Vec<IgnorePattern>) {
    let home_dir = &state.home_dir;
    let cwd = &state.cwd;

    let pathignore_mtime = find_file_upward(cwd, ".pathignore")
        .or_else(|| Some(home_dir.join(".pathignore")))
        .and_then(|p| fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok());

    let gitignore_mtime = find_git_root(cwd)
        .map(|root| root.join(".gitignore"))
        .or_else(|| find_file_upward(cwd, ".gitignore"))
        .and_then(|p| fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok());

    let mut cache = state.ignore_cache.lock();
    if let Some(ref cached) = *cache {
        if cached.pathignore_mtime == pathignore_mtime && cached.gitignore_mtime == gitignore_mtime
        {
            return (cached.roots.clone(), cached.patterns.clone());
        }
    }

    let (roots, patterns) = effective_ignore_rules(
        home_dir,
        cwd,
        state.path_ignores.as_ref(),
        state.path_ignore_patterns.as_ref(),
    );

    *cache = Some(IgnoreRulesCache {
        roots: roots.clone(),
        patterns: patterns.clone(),
        pathignore_mtime,
        gitignore_mtime,
    });

    (roots, patterns)
}

fn ignore_rules_fingerprint(roots: &[PathBuf], patterns: &[IgnorePattern]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for root in roots {
        normalize_slashes(root.to_string_lossy().to_string()).hash(&mut hasher);
    }
    for pattern in patterns {
        ignore_pattern_key(pattern).hash(&mut hasher);
    }
    hasher.finish()
}

fn purge_ignored_entries(db_path: &Path, ignored_roots: &[PathBuf]) -> AppResult<()> {
    if ignored_roots.is_empty() {
        return Ok(());
    }

    let conn = db_connection(db_path)?;
    for root in ignored_roots {
        let root_str = root.to_string_lossy().to_string();
        if root_str.is_empty() {
            continue;
        }

        if root_str == "/" {
            conn.execute("DELETE FROM entries", [])
                .map_err(|e| e.to_string())?;
            break;
        }

        let prefix = root_str.trim_end_matches('/');
        let descendant_like = format!("{}%", escape_like(&format!("{prefix}/")));
        conn.execute(
            "DELETE FROM entries WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'",
            params![prefix, descendant_like],
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn glob_match_path(pattern: &str, text: &str) -> bool {
    let pat = pattern.as_bytes();
    let txt = text.as_bytes();
    let (pn, tn) = (pat.len(), txt.len());

    let mut px = 0usize;
    let mut tx = 0usize;
    let mut star_px = usize::MAX;
    let mut star_tx = 0usize;
    let mut star_is_double = false;

    while tx < tn {
        if px < pn && px + 1 < pn && pat[px] == b'*' && pat[px + 1] == b'*' {
            star_px = px;
            star_tx = tx;
            star_is_double = true;
            px += 2;
        } else if px < pn && pat[px] == b'*' {
            star_px = px;
            star_tx = tx;
            star_is_double = false;
            px += 1;
        } else if px < pn && (pat[px] == b'?' && txt[tx] != b'/') {
            px += 1;
            tx += 1;
        } else if px < pn && pat[px] == txt[tx] {
            px += 1;
            tx += 1;
        } else if star_px != usize::MAX {
            star_tx += 1;
            if !star_is_double && txt[star_tx - 1] == b'/' {
                return false;
            }
            px = if star_is_double {
                star_px + 2
            } else {
                star_px + 1
            };
            tx = star_tx;
        } else {
            return false;
        }
    }

    while px < pn && pat[px] == b'*' {
        px += 1;
    }

    px == pn
}

pub(crate) fn matches_ignore_pattern(path: &str, pattern: &IgnorePattern) -> bool {
    match pattern {
        IgnorePattern::AnySegment(segment) => {
            let suffix = format!("/{segment}");
            let infix = format!("/{segment}/");
            path.ends_with(&suffix) || path.contains(&infix)
        }
        IgnorePattern::Glob(glob) => {
            let mut cur = path;
            loop {
                if glob_match_path(glob, cur) {
                    return true;
                }

                if cur == "/" {
                    break;
                }

                let Some(pos) = cur.rfind('/') else {
                    break;
                };

                cur = if pos == 0 { "/" } else { &cur[..pos] };
            }
            false
        }
    }
}

pub(crate) fn should_skip_path(
    path: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> bool {
    should_skip_path_ext(path, ignored_roots, ignored_patterns, None, None)
}

pub(crate) fn should_skip_path_ext(
    path: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
    gitignore: Option<&gitignore_filter::GitignoreFilter>,
    is_dir_hint: Option<bool>,
) -> bool {
    let s = normalize_slashes(path.to_string_lossy().to_string());

    // Single-component builtin names: check path segments
    if s.split('/').any(|seg| {
        BUILTIN_SKIP_NAMES.contains(&seg)
            || BUILTIN_SKIP_SUFFIXES.iter().any(|suf| seg.ends_with(suf))
    }) {
        return true;
    }

    // Multi-component builtin paths
    if BUILTIN_SKIP_PATHS.iter().any(|pat| {
        let infix = format!("/{pat}/");
        let suffix = format!("/{pat}");
        s.contains(&infix) || s.ends_with(&suffix)
    }) {
        return true;
    }

    if ignored_roots
        .iter()
        .any(|root| path == root || path.starts_with(root))
    {
        return true;
    }
    if ignored_patterns
        .iter()
        .any(|pattern| matches_ignore_pattern(&s, pattern))
    {
        return true;
    }
    if let Some(gi) = gitignore {
        let is_dir = is_dir_hint.unwrap_or_else(|| path.is_dir());
        if gi.is_ignored(path, is_dir) {
            return true;
        }
    }
    false
}

fn extension_for(path: &Path, is_dir: bool) -> Option<String> {
    if is_dir {
        return None;
    }

    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
}

pub(crate) fn index_row_from_path_and_metadata(path: &Path, metadata: &fs::Metadata) -> Option<IndexRow> {
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
        run_id: 0,
    })
}

fn index_row_from_path(path: &Path) -> Option<IndexRow> {
    let metadata = fs::symlink_metadata(path).ok()?;
    index_row_from_path_and_metadata(path, &metadata)
}

fn index_row_from_walkdir_entry(entry: &walkdir::DirEntry) -> Option<IndexRow> {
    let metadata = entry.metadata().ok()?;
    index_row_from_path_and_metadata(entry.path(), &metadata)
}

fn index_row_from_jwalk_entry(entry: &jwalk::DirEntry<((), ())>) -> Option<IndexRow> {
    let metadata = entry.metadata().ok()?;
    index_row_from_path_and_metadata(&entry.path(), &metadata)
}

fn collect_rows_recursive(
    root: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> Vec<IndexRow> {
    let mut rows = Vec::new();

    let iter = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_skip_path(entry.path(), ignored_roots, ignored_patterns));

    for entry in iter.flatten() {
        if let Some(row) = index_row_from_walkdir_entry(&entry) {
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
        size: row.size,
        mtime: row.mtime,
    }
}

pub fn entry_from_path(path: &Path) -> Option<EntryDto> {
    index_row_from_path(path).map(entry_from_index_row)
}

fn entry_cmp(a: &EntryDto, b: &EntryDto, sort_by: &str, sort_dir: &str) -> Ordering {
    match sort_by {
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
        "dir" => {
            let lhs = a.dir.to_lowercase();
            let rhs = b.dir.to_lowercase();
            let primary = if sort_dir == "desc" {
                rhs.cmp(&lhs)
            } else {
                lhs.cmp(&rhs)
            };
            if primary != Ordering::Equal {
                return primary;
            }
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
        "size" => {
            let lhs = a.size.unwrap_or(0);
            let rhs = b.size.unwrap_or(0);
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
    }
}

fn relevance_rank(entry: &EntryDto, query: &str) -> u8 {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return 255;
    }

    let name = entry.name.to_lowercase();
    let path = entry.path.to_lowercase();

    if name == q {
        return 0;
    }
    if name.starts_with(&q) {
        return 1;
    }
    if name.contains(&q) {
        return 2;
    }
    if path.ends_with(&format!("/{q}")) {
        return 3;
    }
    if path.contains(&q) {
        return 4;
    }

    5
}

fn path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

pub fn sort_entries(entries: &mut Vec<EntryDto>, sort_by: &str, sort_dir: &str) {
    entries.sort_by(|a, b| entry_cmp(a, b, sort_by, sort_dir));
}

fn sort_entries_with_relevance(
    entries: &mut Vec<EntryDto>,
    query: &str,
    sort_by: &str,
    sort_dir: &str,
) {
    if query.trim().is_empty() {
        sort_entries(entries, sort_by, sort_dir);
        return;
    }

    entries.sort_by(|a, b| {
        let ra = relevance_rank(a, query);
        let rb = relevance_rank(b, query);
        if ra != rb {
            return ra.cmp(&rb);
        }

        // For highly-relevant matches, prefer shallower paths first
        // so `~/name` ranks above deep descendants with the same name.
        if ra <= 3 {
            let da = path_depth(&a.path);
            let db = path_depth(&b.path);
            if da != db {
                return da.cmp(&db);
            }
        }

        entry_cmp(a, b, sort_by, sort_dir)
    });
}

fn filter_ignored_entries(
    entries: Vec<EntryDto>,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> Vec<EntryDto> {
    entries
        .into_iter()
        .filter(|entry| !should_skip_path(Path::new(&entry.path), ignored_roots, ignored_patterns))
        .collect()
}

fn find_search(
    home_dir: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
    query: &str,
    limit: usize,
    sort_by: &str,
    sort_dir: &str,
) -> Vec<EntryDto> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut search_root = home_dir.to_path_buf();
    let mut dir_filter_pattern: Option<String> = None;
    let mut name_filter_pattern: Option<String> = None;
    let mut name_filter_glob = false;

    if trimmed.contains('/') {
        let last_slash = trimmed.rfind('/').unwrap();
        let dir_part = trimmed[..last_slash].trim();
        let name_part = trimmed[last_slash + 1..].trim();

        if !dir_part.is_empty() {
            let mut hinted = if dir_part == "~" {
                Some(home_dir.to_path_buf())
            } else if let Some(rest) = dir_part.strip_prefix("~/") {
                Some(home_dir.join(rest))
            } else {
                let p = PathBuf::from(dir_part);
                if p.is_absolute() {
                    Some(p)
                } else if !dir_part.contains('*')
                    && !dir_part.contains('?')
                    && !dir_part.contains('[')
                {
                    Some(home_dir.join(dir_part))
                } else {
                    None
                }
            };

            if let Some(path) = hinted.take() {
                if path.exists() && path.is_dir() {
                    search_root = path;
                } else {
                    dir_filter_pattern = Some(format!("*{}/*", dir_part));
                }
            } else {
                dir_filter_pattern = Some(format!("*{}/*", dir_part));
            }
        }

        if !name_part.is_empty() {
            if name_part.contains('*') || name_part.contains('?') {
                name_filter_pattern = Some(name_part.to_string());
                name_filter_glob = true;
            } else {
                name_filter_pattern = Some(format!("*{}*", name_part));
            }
        }
    } else if trimmed.contains('*') || trimmed.contains('?') {
        name_filter_pattern = Some(trimmed.to_string());
        name_filter_glob = true;
    } else {
        name_filter_pattern = Some(format!("*{}*", trimmed));
    }

    let mut cmd = Command::new("find");
    cmd.arg(&search_root);

    if search_root == home_dir {
        cmd.args(["-maxdepth", "8"]);
    }

    if let Some(dir_pattern) = dir_filter_pattern.as_deref() {
        cmd.args(["-ipath", dir_pattern]);
    }
    if let Some(name_pattern) = name_filter_pattern.as_deref() {
        cmd.args(["-iname", name_pattern]);
    }
    if name_filter_glob {
        cmd.args(["-type", "f"]);
    }

    cmd.args(["!", "-path", "*/.git/*"])
        .args(["!", "-path", "*/node_modules/*"])
        .args(["!", "-path", "*/.Trash/*"])
        .args(["!", "-path", "*/.Trashes/*"])
        .args(["!", "-path", "*/Library/Caches/*"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => return Vec::new(),
    };

    let reader = BufReader::new(stdout);
    let mut entries = Vec::with_capacity(limit);

    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let path = PathBuf::from(&line);
        if should_skip_path(&path, ignored_roots, ignored_patterns) {
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

    sort_entries_with_relevance(&mut entries, trimmed, sort_by, sort_dir);
    entries
}

pub(crate) fn upsert_rows(conn: &mut Connection, rows: &[IndexRow]) -> AppResult<usize> {
    if rows.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        let mut stmt = tx
            .prepare(
                r#"
                INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
                VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(path) DO UPDATE SET
                  name = excluded.name,
                  dir = excluded.dir,
                  is_dir = excluded.is_dir,
                  ext = excluded.ext,
                  mtime = excluded.mtime,
                  size = excluded.size,
                  indexed_at = excluded.indexed_at,
                  run_id = excluded.run_id
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
                row.indexed_at,
                row.run_id
            ])
            .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(rows.len())
}

pub(crate) fn delete_paths(conn: &mut Connection, raw_paths: &[String]) -> AppResult<usize> {
    if raw_paths.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut deleted = 0;

    {
        let mut stmt_exact = tx
            .prepare("DELETE FROM entries WHERE path = ?1")
            .map_err(|e| e.to_string())?;
        let mut stmt_children = tx
            .prepare("DELETE FROM entries WHERE path >= ?1 AND path < ?2")
            .map_err(|e| e.to_string())?;

        for path in raw_paths {
            let is_root = path == "/" || path == "\\";
            let normalized = if is_root {
                path.clone()
            } else {
                path.trim_end_matches(&['/', '\\'][..]).to_string()
            };

            if normalized.is_empty() {
                continue;
            }

            if is_root {
                deleted += tx
                    .execute("DELETE FROM entries", [])
                    .map_err(|e| e.to_string())?;
                continue;
            }

            deleted += stmt_exact
                .execute(params![&normalized])
                .map_err(|e| e.to_string())?;

            // Delete children using B-tree range scan: path >= "{normalized}{sep}" AND path < "{normalized}\x7F"
            // \x7F (127) is higher than both '/' (47) and '\' (92), so this captures all children.
            let sep = std::path::MAIN_SEPARATOR;
            let range_start = format!("{normalized}{sep}");
            let range_end = format!("{normalized}\x7F");
            deleted += stmt_children
                .execute(params![&range_start, &range_end])
                .map_err(|e| e.to_string())?;
        }
    }

    tx.commit().map_err(|e| e.to_string())?;
    Ok(deleted)
}

pub(crate) fn emit_index_state(app: &AppHandle, state: &str, message: Option<String>) {
    let is_catchup = message.as_ref().map_or(false, |m| m.starts_with("Catchup:"));
    let _ = app.emit(
        "index_state",
        IndexStateEvent {
            state: state.to_string(),
            message,
            is_catchup,
        },
    );
}

pub(crate) fn emit_index_updated(
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

pub(crate) fn emit_index_progress(app: &AppHandle, scanned: u64, indexed: u64, current_path: String) {
    let _ = app.emit(
        "index_progress",
        IndexProgressEvent {
            scanned,
            indexed,
            current_path,
        },
    );
}

pub(crate) fn set_state(state: &AppState, next: IndexState, message: Option<String>) {
    let mut status = state.status.lock();
    status.state = next;
    status.message = message;
}

pub(crate) fn set_progress(state: &AppState, scanned: u64, indexed: u64, current_path: &str) {
    let mut status = state.status.lock();
    status.scanned = scanned;
    status.indexed = indexed;
    status.current_path = current_path.to_string();
}

fn emit_status_counts(app: &AppHandle, state: &AppState) {
    let status = state.status.lock();
    emit_index_updated(
        app,
        status.entries_count,
        status.last_updated.unwrap_or_else(now_epoch),
        status.permission_errors,
    );
}

pub(crate) fn refresh_and_emit_status_counts(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let (entries_count, last_updated) = update_status_counts(state)?;
    // Persist cached counts so next startup can read them instantly
    if let Ok(conn) = db_connection(&state.db_path) {
        let _ = set_meta(&conn, "cached_entries_count", &entries_count.to_string());
        if let Some(lu) = last_updated {
            let _ = set_meta(&conn, "cached_last_updated", &lu.to_string());
        }
    }
    emit_status_counts(app, state);
    Ok(())
}

/// Load cached entries count from meta table (instant) and emit Ready state + counts.
/// Used on Windows startup paths where the index is already complete from a prior run.
#[cfg(target_os = "windows")]
pub(crate) fn set_ready_with_cached_counts(app: &AppHandle, state: &AppState) {
    let (count, last_updated) = db_connection(&state.db_path)
        .ok()
        .map(|conn| {
            let c = get_meta(&conn, "cached_entries_count")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let lu = get_meta(&conn, "cached_last_updated")
                .and_then(|v| v.parse::<i64>().ok());
            (c, lu)
        })
        .unwrap_or((0, None));

    {
        let mut status = state.status.lock();
        status.state = IndexState::Ready;
        status.entries_count = count;
        status.last_updated = last_updated;
    }

    emit_index_state(app, "Ready", None);
    emit_index_updated(app, count, last_updated.unwrap_or_else(now_epoch), 0);
}

#[cfg(target_os = "macos")]
fn touch_status_updated(state: &AppState) {
    state.status.lock().last_updated = Some(now_epoch());
}

pub(crate) fn start_full_index_worker(app: AppHandle, state: AppState) -> AppResult<()> {
    start_full_index_worker_inner(app, state, false)
}

#[cfg(target_os = "windows")]
pub(crate) fn start_full_index_worker_silent(app: AppHandle, state: AppState) -> AppResult<()> {
    start_full_index_worker_inner(app, state, true)
}

fn start_full_index_worker_inner(app: AppHandle, state: AppState, silent: bool) -> AppResult<()> {
    if state
        .indexing_active
        .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
        .is_err()
    {
        perf_log("index_start_skipped already_active=true");
        return Ok(());
    }

    // Mark index as incomplete — cleared when run_incremental_index succeeds
    if let Ok(c) = db_connection(&state.db_path) {
        let _ = set_meta(&c, "index_complete", "0");
    }

    if !silent {
        {
            let mut status = state.status.lock();
            status.state = IndexState::Indexing;
            status.message = None;
            status.scanned = 0;
            status.indexed = 0;
            status.current_path.clear();
        }

        emit_index_state(&app, "Indexing", None);
        perf_log("index_state=Indexing");
    } else {
        perf_log("index_start silent=true (background reindex)");
    }

    std::thread::spawn(move || {
        let result = run_incremental_index(&app, &state);
        if let Err(ref err) = result {
            eprintln!("[index] run_incremental_index failed: {err}");
            if !silent {
                set_state(&state, IndexState::Error, Some(err.clone()));
                emit_index_state(&app, "Error", Some(err.clone()));
            }
            // Silent mode: keep Ready state, just log the error
        }
        state.indexing_active.store(false, AtomicOrdering::Release);
    });

    Ok(())
}

fn is_deferred_dir(path: &Path, home_dir: &Path) -> bool {
    path.parent() == Some(home_dir)
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|name| DEFERRED_DIR_NAMES.contains(&name))
            .unwrap_or(false)
}

fn preload_existing_entries(
    conn: &Connection,
    dir_prefix: &str,
) -> HashMap<String, (Option<i64>, Option<i64>)> {
    let like_pattern = format!("{}/%", escape_like(dir_prefix));
    let mut stmt = match conn.prepare(
        "SELECT path, mtime, size FROM entries WHERE path LIKE ?1 ESCAPE '\\' OR path = ?2",
    ) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };

    let mut map = HashMap::new();
    let rows = match stmt.query_map(params![like_pattern, dir_prefix], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    }) {
        Ok(r) => r,
        Err(_) => return map,
    };

    for row in rows.flatten() {
        map.insert(row.0, (row.1, row.2));
    }
    map
}

fn stamp_run_id_batch(conn: &mut Connection, paths: &[String], run_id: i64) -> AppResult<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        let mut stmt = tx
            .prepare("UPDATE entries SET run_id = ?1 WHERE path = ?2")
            .map_err(|e| e.to_string())?;
        for path in paths {
            stmt.execute(params![run_id, path])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(())
}

fn run_incremental_index(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let started = Instant::now();
    perf_log(format!(
        "index_run_start home={} db={}",
        state.home_dir.to_string_lossy(),
        state.db_path.to_string_lossy(),
    ));

    let mut conn = db_connection(&state.db_path)?;
    set_indexing_pragmas(&conn)?;

    let result = run_incremental_index_inner(app, state, &mut conn);

    let _ = conn.execute_batch("ANALYZE");
    let _ = restore_normal_pragmas(&conn);

    match &result {
        Ok(_) => {
            let _ = set_meta(&conn, "index_complete", "1");
            let snapshot = state.status.lock().clone();
            perf_log(format!(
                "index_run_done elapsed_ms={} scanned={} indexed={} entries={} permission_errors={} message={:?}",
                started.elapsed().as_millis(),
                snapshot.scanned,
                snapshot.indexed,
                snapshot.entries_count,
                snapshot.permission_errors,
                snapshot.message,
            ));
        }
        Err(err) => {
            perf_log(format!(
                "index_run_error elapsed_ms={} err={}",
                started.elapsed().as_millis(),
                err,
            ));
        }
    }

    result
}

fn run_incremental_index_inner(
    app: &AppHandle,
    state: &AppState,
    conn: &mut Connection,
) -> AppResult<()> {

    let (runtime_ignored_roots, runtime_ignored_patterns) = effective_ignore_rules(
        &state.home_dir,
        &state.cwd,
        state.path_ignores.as_ref(),
        state.path_ignore_patterns.as_ref(),
    );
    let gi_filter = state.gitignore.get();

    let last_run_id: i64 = get_meta(conn, "last_run_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let current_run_id = last_run_id + 1;
    // Fresh index: DB was empty, skip preload/stamp, single unlimited-depth pass
    let is_fresh = last_run_id == 0;

    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut permission_errors: u64 = 0;
    let mut current_path = String::new();
    let mut batch: Vec<IndexRow> = Vec::with_capacity(BATCH_SIZE);
    let mut stamp_batch: Vec<String> = Vec::with_capacity(BATCH_SIZE);
    let mut last_emit = Instant::now();
    let mut last_perf_emit = Instant::now();

    // Preload scan_root-level entries (direct children only, not recursive)
    let scan_str = state.scan_root.to_string_lossy().to_string();
    let root_existing = preload_existing_entries(conn, &scan_str);

    // Index scan_root itself
    if let Some(mut row) = index_row_from_path(&state.scan_root) {
        scanned += 1;
        row.run_id = current_run_id;
        if let Some((old_mtime, old_size)) = root_existing.get(&row.path) {
            if *old_mtime == row.mtime && *old_size == row.size {
                stamp_batch.push(row.path);
            } else {
                batch.push(row);
            }
        } else {
            batch.push(row);
        }
    }

    // Partition direct children into priority vs deferred
    let mut priority_roots: Vec<PathBuf> = Vec::new();
    let mut deferred_roots: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = fs::read_dir(&state.scan_root) {
        for dir_entry in entries.flatten() {
            let child_path = dir_entry.path();

            if should_skip_path_ext(
                &child_path,
                &runtime_ignored_roots,
                &runtime_ignored_patterns,
                Some(&gi_filter),
                None,
            ) {
                continue;
            }

            scanned += 1;
            current_path = child_path.to_string_lossy().to_string();

            if let Some(mut row) = index_row_from_path(&child_path) {
                row.run_id = current_run_id;
                if let Some((old_mtime, old_size)) = root_existing.get(&row.path) {
                    if *old_mtime == row.mtime && *old_size == row.size {
                        stamp_batch.push(row.path);
                    } else {
                        batch.push(row);
                    }
                } else {
                    batch.push(row);
                }
            }

            if child_path.is_dir() {
                if is_deferred_dir(&child_path, &state.scan_root) {
                    deferred_roots.push(child_path);
                } else {
                    priority_roots.push(child_path);
                }
            }
        }
    }


    // Flush home-level stamp batch
    if stamp_batch.len() >= BATCH_SIZE {
        stamp_run_id_batch(conn, &stamp_batch, current_run_id)?;
        stamp_batch.clear();
    }

    priority_roots.sort();
    deferred_roots.sort();

    let roots: Vec<PathBuf> = priority_roots.into_iter().chain(deferred_roots).collect();
    perf_log(format!(
        "index_scan_roots total={} priority+deferred",
        roots.len()
    ));

    // 2-pass indexing: shallow first (depth <= SHALLOW_SCAN_DEPTH), then deep
    // Both passes use jwalk for parallel scanning
    let arc_ignored_roots = Arc::new(runtime_ignored_roots.clone());
    let arc_ignored_patterns = Arc::new(runtime_ignored_patterns.clone());


    for pass in 0..2u8 {
        // Fresh index: single unlimited-depth pass — avoids double filesystem traversal
        if pass == 1 && is_fresh {
            break;
        }
        let pass_label = if pass == 0 { "shallow" } else { "deep" };
        let pass_started = Instant::now();
        let mut pass_scanned = 0u64;
        let mut pass_indexed = 0u64;

        for root in &roots {
            let root_started = Instant::now();
            let mut root_scanned = 0u64;
            let mut root_indexed = 0u64;
            let mut root_permission_errors = 0u64;
            let root_str = root.to_string_lossy().to_string();

            // Fresh index: DB is empty, preload is unnecessary
            let existing = if is_fresh {
                HashMap::new()
            } else {
                let e = preload_existing_entries(conn, &root_str);
                e
            };

            let gi_ref = gi_filter.clone();
            let skip_roots = arc_ignored_roots.clone();
            let skip_patterns = arc_ignored_patterns.clone();

            let mut builder = jwalk::WalkDir::new(root)
                .follow_links(false)
                .skip_hidden(false)
                .parallelism(jwalk::Parallelism::RayonNewPool(JWALK_NUM_THREADS));
            // Fresh index: no depth limit — walk everything in one pass
            if pass == 0 && !is_fresh {
                builder = builder.max_depth(SHALLOW_SCAN_DEPTH);
            }
            let walker = builder.process_read_dir(move |_depth, path, _state, children| {
                children.retain(|entry_result| {
                    entry_result
                        .as_ref()
                        .map(|entry| {
                            let full_path = path.join(&entry.file_name);
                            let is_dir = Some(entry.file_type.is_dir());
                            !should_skip_path_ext(
                                &full_path,
                                &skip_roots,
                                &skip_patterns,
                                Some(&gi_ref),
                                is_dir,
                            )
                        })
                        .unwrap_or(false)
                });
            });

            for result in walker {
                match result {
                    Ok(entry) => {
                        let path = entry.path();
                        if path == root.as_path() {
                            continue;
                        }

                        // Pass 1 (incremental only): skip shallow entries already indexed in pass 0
                        if pass == 1 && entry.depth <= SHALLOW_SCAN_DEPTH {
                            continue;
                        }

                        scanned += 1;
                        root_scanned += 1;
                        current_path = path.to_string_lossy().to_string();

                        if let Some(mut row) = index_row_from_jwalk_entry(&entry) {
                            row.run_id = current_run_id;
                            if is_fresh {
                                // Fresh index: all entries are new, skip existing check
                                indexed += 1;
                                root_indexed += 1;
                                batch.push(row);
                            } else if let Some((old_mtime, old_size)) = existing.get(&row.path) {
                                if *old_mtime == row.mtime && *old_size == row.size {
                                    stamp_batch.push(row.path);
                                } else {
                                    indexed += 1;
                                    root_indexed += 1;
                                    batch.push(row);
                                }
                            } else {
                                indexed += 1;
                                root_indexed += 1;
                                batch.push(row);
                            }
                        }

                        if batch.len() >= BATCH_SIZE {
                            upsert_rows(conn, &batch)?;
                            batch.clear();
                        }

                        if stamp_batch.len() >= BATCH_SIZE {
                            stamp_run_id_batch(conn, &stamp_batch, current_run_id)?;
                            stamp_batch.clear();
                        }

                        if last_emit.elapsed() >= Duration::from_millis(200) {
                            set_progress(state, scanned, indexed, &current_path);
                            emit_index_progress(app, scanned, indexed, current_path.clone());
                            last_emit = Instant::now();
                        }
                        if perf_log_enabled() && last_perf_emit.elapsed() >= Duration::from_secs(1)
                        {
                            perf_log(format!(
                                "index_progress pass={} scanned={} indexed={} current_path={}",
                                pass_label, scanned, indexed, current_path
                            ));
                            last_perf_emit = Instant::now();
                        }
                    }
                    Err(err) => {
                        scanned += 1;
                        root_scanned += 1;
                        permission_errors += 1;
                        root_permission_errors += 1;
                        if permission_errors <= 20 || perf_log_enabled() {
                            eprintln!("[index] permission error: {}", err);
                        }
                    }
                }
            }

            pass_scanned += root_scanned;
            pass_indexed += root_indexed;

            eprintln!("[timing]   walk_{} {} {}ms scanned={} indexed={} err={}",
                pass_label, root_str, root_started.elapsed().as_millis(),
                root_scanned, root_indexed, root_permission_errors);
            perf_log(format!(
                "index_root_done pass={} root={} elapsed_ms={} scanned={} indexed={} permission_errors={}",
                pass_label,
                root.to_string_lossy(),
                root_started.elapsed().as_millis(),
                root_scanned,
                root_indexed,
                root_permission_errors,
            ));
        }

        // Flush between passes
        if !batch.is_empty() {
            upsert_rows(conn, &batch)?;
            batch.clear();
        }
        if !stamp_batch.is_empty() {
            stamp_run_id_batch(conn, &stamp_batch, current_run_id)?;
            stamp_batch.clear();
        }

        perf_log(format!(
            "index_pass_done pass={} elapsed_ms={} scanned={} indexed={}",
            pass_label,
            pass_started.elapsed().as_millis(),
            pass_scanned,
            pass_indexed,
        ));

        // After shallow pass (incremental only), emit progress so UI shows searchable state early
        // Fresh index uses per-root emit above instead.
        if pass == 0 && !is_fresh {
            let _ = refresh_and_emit_status_counts(app, state);
            let _ = app.emit(
                "index_updated",
                IndexUpdatedEvent {
                    entries_count: state.status.lock().entries_count,
                    last_updated: now_epoch(),
                    permission_errors,
                },
            );
        }
    }

    if !batch.is_empty() {
        upsert_rows(conn, &batch)?;
    }

    if !stamp_batch.is_empty() {
        stamp_run_id_batch(conn, &stamp_batch, current_run_id)?;
    }

    set_progress(state, scanned, indexed, &current_path);
    emit_index_progress(app, scanned, indexed, current_path.clone());

    // Remove entries not seen in this run (deleted files)
    // Fresh index: DB was empty before this run, no stale entries possible
    let deleted_count: i64 = if is_fresh {
        0
    } else {
        let count = conn
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
        count
    };

    set_meta(conn, "last_run_id", &current_run_id.to_string())?;

    if deleted_count > 0 || indexed > 0 {
        invalidate_search_caches(state);
    }

    let (entries_count, last_updated) = update_status_counts(state)?;
    // Persist cached counts for instant startup next time
    let _ = set_meta(conn, "cached_entries_count", &entries_count.to_string());
    if let Some(lu) = last_updated {
        let _ = set_meta(conn, "cached_last_updated", &lu.to_string());
    }
    let updated_at = last_updated.unwrap_or_else(now_epoch);

    {
        let mut status = state.status.lock();
        status.state = IndexState::Ready;
        status.permission_errors = permission_errors;
        status.scanned = scanned;
        status.indexed = indexed;
        status.current_path = current_path.clone();
        status.message = if permission_errors > 0 {
            Some(format!(
                "{} permission/access error(s) occurred.",
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
            Some(format!("{} permission/access error(s)", permission_errors))
        } else {
            None
        },
    );
    perf_log(format!(
        "index_state=Ready scanned={} indexed={} deleted={} entries={} permission_errors={}",
        scanned, indexed, deleted_count, entries_count, permission_errors
    ));

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

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn is_recently_touched(state: &AppState, path: &str) -> bool {
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

#[cfg(target_os = "macos")]
fn apply_path_changes(state: &AppState, paths: &[PathBuf]) -> AppResult<usize> {
    let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(state);
    let mut to_upsert_map: HashMap<String, IndexRow> = HashMap::new();
    let mut to_delete = Vec::new();

    for path in paths {
        if should_skip_path(path, &ignored_roots, &ignored_patterns) {
            continue;
        }

        let path_str = path.to_string_lossy().to_string();
        if path_str.is_empty() {
            continue;
        }

        if is_recently_touched(state, &path_str) {
            continue;
        }

        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if let Some(row) = index_row_from_path_and_metadata(path, &metadata) {
                    to_upsert_map.insert(row.path.clone(), row);
                }
            }
            Err(_) => {
                to_delete.push(path_str);
            }
        }
    }

    let to_upsert = to_upsert_map.into_values().collect::<Vec<_>>();

    if to_upsert.is_empty() && to_delete.is_empty() {
        return Ok(0);
    }

    let op_start = std::time::Instant::now();
    let mut conn = db_connection(&state.db_path).map_err(|e| {
        eprintln!(
            "[watcher] db_connection FAILED after {}ms: {} (upsert={} delete={} indexing_active={})",
            op_start.elapsed().as_millis(),
            e,
            to_upsert.len(),
            to_delete.len(),
            state.indexing_active.load(AtomicOrdering::Acquire)
        );
        e
    })?;
    let mut changed = 0;

    changed += upsert_rows(&mut conn, &to_upsert)?;
    changed += delete_paths(&mut conn, &to_delete)?;

    Ok(changed)
}

#[cfg(target_os = "macos")]
const STATUS_EMIT_MIN_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(target_os = "macos")]
const DB_BUSY_RETRY_DELAY: Duration = Duration::from_secs(3);

#[cfg(target_os = "macos")]
fn process_watcher_paths(
    app: &AppHandle,
    state: &AppState,
    pending: &mut HashSet<PathBuf>,
    deadline: &mut Option<Instant>,
    last_status_emit: &mut Instant,
    pending_status_emit: &mut bool,
) {
    if pending.is_empty() {
        return;
    }

    if state.indexing_active.load(AtomicOrdering::Acquire) {
        return;
    }

    let mut batch: Vec<PathBuf> = pending.drain().collect();
    batch.sort();

    match apply_path_changes(state, &batch) {
        Ok(changed) => {
            *deadline = None;
            if changed > 0 {
                invalidate_search_caches(state);
                touch_status_updated(state);
                if last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
                    let _ = refresh_and_emit_status_counts(app, state);
                    *last_status_emit = Instant::now();
                    *pending_status_emit = false;
                } else {
                    *pending_status_emit = true;
                }
            }
        }
        Err(err) => {
            let is_busy = err.contains("locked") || err.contains("busy");
            if is_busy {
                eprintln!(
                    "[watcher] DB busy, will retry in {}s: {} | batch_size={}",
                    DB_BUSY_RETRY_DELAY.as_secs(),
                    err,
                    batch.len()
                );
                for path in batch {
                    pending.insert(path);
                }
                *deadline = Some(Instant::now() + DB_BUSY_RETRY_DELAY);
                return;
            }
            eprintln!(
                "[watcher] process_watcher_paths ERROR: {} | batch_size={} indexing_active={}",
                err,
                batch.len(),
                state.indexing_active.load(AtomicOrdering::Acquire)
            );
            *deadline = None;
            if state.indexing_active.load(AtomicOrdering::Acquire) {
                return;
            }
            let mut status = state.status.lock();
            if !matches!(status.state, IndexState::Indexing) {
                status.state = IndexState::Error;
            }
            status.message = Some(format!("Watcher update failed: {err}"));
            drop(status);
            emit_index_state(app, "Error", Some(format!("Watcher update failed: {err}")));
        }
    }
}

#[cfg(target_os = "macos")]
const EVENT_ID_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(target_os = "macos")]
fn persist_event_id(db_path: &Path, event_id: u64) -> AppResult<()> {
    let conn = db_connection(db_path)?;
    set_meta(&conn, "last_event_id", &event_id.to_string())
}

#[cfg(target_os = "macos")]
const MUST_SCAN_THRESHOLD: usize = 10;

#[cfg(target_os = "macos")]
fn start_fsevent_watcher_worker(
    app: AppHandle,
    state: AppState,
    since_event_id: Option<u64>,
    conditional: bool,
) {
    use std::sync::mpsc::{self, RecvTimeoutError};
    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel();

        let mut watcher =
            match mac::fsevent_watcher::FsEventWatcher::new(&state.home_dir, since_event_id, tx) {
                Ok(w) => w,
                Err(err) => {
                    set_state(
                        &state,
                        IndexState::Error,
                        Some(format!("FSEvents watcher initialization failed: {err}")),
                    );
                    emit_index_state(
                        &app,
                        "Error",
                        Some(format!("FSEvents watcher initialization failed: {err}")),
                    );
                    return;
                }
            };

        if conditional {
            perf_log("conditional_startup: watcher started, awaiting history replay");
            set_state(&state, IndexState::Ready, None);
            emit_index_state(&app, "Ready", None);
        }

        let mut pending_paths: HashSet<PathBuf> = HashSet::new();
        let mut deadline: Option<Instant> = None;
        let mut last_flush = Instant::now();
        let mut last_status_emit = Instant::now();
        let mut pending_status_emit = false;
        let mut last_count_refresh = Instant::now();
        let mut must_scan_count: usize = 0;
        let mut replay_phase = conditional;
        let mut full_scan_triggered = false;

        loop {
            let wait = match deadline {
                Some(due) => {
                    let now = Instant::now();
                    if now >= due {
                        Duration::from_millis(0)
                    } else {
                        due - now
                    }
                }
                None => Duration::from_secs(1),
            };
            match rx.recv_timeout(wait) {
                Ok(mac::fsevent_watcher::FsEvent::Paths(paths)) => {
                    let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(&state);
                    let prev_len = pending_paths.len();
                    for path in paths {
                        if !should_skip_path(&path, &ignored_roots, &ignored_patterns) {
                            pending_paths.insert(path);
                        }
                    }
                    if pending_paths.len() > prev_len {
                        deadline = Some(Instant::now() + WATCH_DEBOUNCE);
                    }
                }
                Ok(mac::fsevent_watcher::FsEvent::MustScanSubDirs(path)) => {
                    must_scan_count += 1;
                    if replay_phase
                        && must_scan_count >= MUST_SCAN_THRESHOLD
                        && !full_scan_triggered
                    {
                        perf_log(format!(
                            "conditional_startup: MustScanSubDirs={} >= threshold, triggering full scan",
                            must_scan_count
                        ));
                        full_scan_triggered = true;
                        let _ = start_full_index_worker(app.clone(), state.clone());
                    }
                    if !state.indexing_active.load(AtomicOrdering::Acquire) {
                        let (ignored_roots, ignored_patterns) =
                            cached_effective_ignore_rules(&state);
                        let rows = collect_rows_recursive(&path, &ignored_roots, &ignored_patterns);
                        if !rows.is_empty() {
                            eprintln!(
                                "[watcher] MustScanSubDirs DB write: rows={} path={}",
                                rows.len(),
                                path.display()
                            );
                            if let Ok(mut conn) = db_connection(&state.db_path) {
                                if upsert_rows(&mut conn, &rows).is_ok() {
                                    invalidate_search_caches(&state);
                                    touch_status_updated(&state);
                                    if last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
                                        let _ = refresh_and_emit_status_counts(&app, &state);
                                        last_status_emit = Instant::now();
                                        pending_status_emit = false;
                                    } else {
                                        pending_status_emit = true;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(mac::fsevent_watcher::FsEvent::HistoryDone) => {
                    eprintln!(
                        "[watcher] HistoryDone: pending_paths={} indexing_active={}",
                        pending_paths.len(),
                        state.indexing_active.load(AtomicOrdering::Acquire)
                    );
                    process_watcher_paths(
                        &app,
                        &state,
                        &mut pending_paths,
                        &mut deadline,
                        &mut last_status_emit,
                        &mut pending_status_emit,
                    );
                    if replay_phase {
                        replay_phase = false;
                        if !full_scan_triggered {
                            perf_log(format!(
                                "conditional_startup: HistoryDone, MustScanSubDirs={}, skipping full scan",
                                must_scan_count
                            ));
                            let _ = refresh_and_emit_status_counts(&app, &state);
                            last_status_emit = Instant::now();
                            pending_status_emit = false;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }

            if let Some(due) = deadline {
                if Instant::now() >= due {
                    process_watcher_paths(
                        &app,
                        &state,
                        &mut pending_paths,
                        &mut deadline,
                        &mut last_status_emit,
                        &mut pending_status_emit,
                    );
                }
            }

            if pending_status_emit && last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
                let _ = refresh_and_emit_status_counts(&app, &state);
                last_status_emit = Instant::now();
                pending_status_emit = false;
            }

            // Periodic event_id flush
            if last_flush.elapsed() >= EVENT_ID_FLUSH_INTERVAL {
                let eid = watcher.last_event_id();
                let _ = persist_event_id(&state.db_path, eid);
                last_flush = Instant::now();
            }

            // Periodic count refresh to keep in-memory counts accurate
            if last_count_refresh.elapsed() >= EVENT_ID_FLUSH_INTERVAL {
                let _ = update_status_counts(&state);
                last_count_refresh = Instant::now();
            }
        }

        // Final flush on shutdown
        let eid = watcher.last_event_id();
        let _ = persist_event_id(&state.db_path, eid);
        watcher.stop();
    });
}

fn validate_new_name(new_name: &str) -> AppResult<String> {
    let trimmed = new_name.trim();

    if trimmed.is_empty() {
        return Err("New name cannot be empty.".to_string());
    }
    if trimmed.contains('/') {
        return Err("New name cannot contain '/'.".to_string());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("Invalid name.".to_string());
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
        size: row.get(5)?,
        mtime: row.get(6)?,
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
let image = NSWorkspace.shared.icon(forFileType: "{file_type}")
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

#[cfg(target_os = "windows")]
fn load_system_icon_png(ext: &str) -> Option<Vec<u8>> {
    win::icon::load_icon_png_by_ext(ext)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn load_system_icon_png(_ext: &str) -> Option<Vec<u8>> {
    None
}

#[cfg(target_os = "windows")]
fn is_per_file_icon_ext(ext: &str) -> bool {
    matches!(ext, "exe" | "lnk" | "ico" | "url" | "scr" | "appx")
}

#[tauri::command]
fn get_index_status(state: State<'_, AppState>) -> IndexStatusDto {
    let snapshot = state.status.lock().clone();
    let state_label = if matches!(snapshot.state, IndexState::Error) {
        snapshot.state.as_str().to_string()
    } else if matches!(snapshot.state, IndexState::Ready) {
        "Ready".to_string()
    } else if state.indexing_active.load(AtomicOrdering::Acquire)
        || !state.db_ready.load(AtomicOrdering::Acquire)
    {
        "Indexing".to_string()
    } else {
        snapshot.state.as_str().to_string()
    };
    IndexStatusDto {
        state: state_label,
        entries_count: snapshot.entries_count,
        last_updated: snapshot.last_updated,
        permission_errors: snapshot.permission_errors,
        message: snapshot.message,
        scanned: snapshot.scanned,
        indexed: snapshot.indexed,
        current_path: snapshot.current_path,
    }
}

#[tauri::command]
fn get_home_dir(state: State<'_, AppState>) -> String {
    state.home_dir.to_string_lossy().to_string()
}

#[tauri::command]
fn start_full_index(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    #[cfg(target_os = "windows")]
    {
        win::start_windows_indexing(app, state.inner().clone());
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        start_full_index_worker(app, state.inner().clone())
    }
}

#[tauri::command]
async fn reset_index(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if state.indexing_active.load(AtomicOrdering::Acquire) {
            return Err("Cannot reset while indexing is in progress.".to_string());
        }

        // Stop existing file watcher and wait for it to fully exit
        state.watcher_stop.store(true, AtomicOrdering::Release);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.watcher_active.load(AtomicOrdering::Acquire) {
            if std::time::Instant::now() >= deadline {
                eprintln!("[reset] watcher did not stop within 5s, proceeding anyway");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let conn = db_connection(&state.db_path)?;
        conn.execute("DELETE FROM entries", [])
            .map_err(|e| e.to_string())?;
        conn.execute("DELETE FROM meta", [])
            .map_err(|e| e.to_string())?;

        {
            let mut status = state.status.lock();
            status.entries_count = 0;
            status.last_updated = None;
            status.permission_errors = 0;
            status.message = None;
            status.scanned = 0;
            status.indexed = 0;
            status.current_path.clear();
        }

        invalidate_search_caches(&state);

        emit_index_updated(&app, 0, now_epoch(), 0);

        // Allow new watcher to start
        state.watcher_stop.store(false, AtomicOrdering::Release);

        #[cfg(target_os = "windows")]
        {
            win::start_windows_indexing(app, state);
            Ok(())
        }
        #[cfg(not(target_os = "windows"))]
        {
            start_full_index_worker(app, state)
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

fn log_search(db_path: &Path, query: &str, mode: &str, results: &[EntryDto]) {
    if !search_log_enabled() {
        return;
    }

    let log_path = db_path.with_file_name("search.log");
    let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };
    let top: Vec<&str> = results.iter().take(10).map(|e| e.name.as_str()).collect();
    let _ = writeln!(
        f,
        "[{}] Q={:?} MODE={} COUNT={} TOP={:?}",
        now_epoch(),
        query,
        mode,
        results.len(),
        top,
    );
}

fn search_log_enabled() -> bool {
    *SEARCH_LOG_ENABLED.get_or_init(|| env_truthy("FASTFIND_SEARCH_LOG"))
}

/// Returns the total number of entries matching `query` without LIMIT/OFFSET.
/// Returns 0 when the total is unknown or not worth computing (PathSearch with
/// complex WHERE, non-zero offset, or non-SQL paths).
fn compute_total_count(db_path: &Path, home_dir: &Path, execution: &SearchExecution) -> u32 {
    // When fewer results than the limit were returned the total is exact.
    if (execution.results.len() as u32) < execution.effective_limit {
        return execution.offset.saturating_add(execution.results.len() as u32);
    }
    // For paginated pages after the first we leave total tracking to the frontend.
    if execution.offset > 0 {
        return 0;
    }
    // Non-SQL fast paths already return all matching entries.
    if execution.mode_label.starts_with("mem_")
        || execution.mode_label == "spotlight"
        || execution.mode_label == "spotlight_timeout"
        || execution.mode_label == "find_fallback"
        || execution.mode_label == "name_neg_cache"
    {
        return execution.results.len() as u32;
    }
    let Ok(conn) = db_connection_for_search(db_path) else {
        return 0;
    };
    let mode = parse_query(&execution.query);
    match mode {
        SearchMode::Empty => conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap_or(0),
        SearchMode::NameSearch { name_like } => {
            let escaped = escape_like(&execution.query);
            let prefix_like = format!("{}%", escaped);
            let prefix_count: u32 = conn
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE name LIKE ?1 ESCAPE '\\'",
                    params![prefix_like],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if prefix_count > 0 {
                prefix_count
            } else {
                // Phase-2 contains fallback was used.
                conn.query_row(
                    "SELECT COUNT(*) FROM entries WHERE name LIKE ?1 ESCAPE '\\'",
                    params![name_like],
                    |r| r.get(0),
                )
                .unwrap_or(0)
            }
        }
        SearchMode::GlobName { name_like } => conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE name LIKE ?1 ESCAPE '\\'",
                params![name_like],
                |r| r.get(0),
            )
            .unwrap_or(0),
        SearchMode::ExtSearch { ext, .. } => conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE ext = ?1",
                params![ext],
                |r| r.get(0),
            )
            .unwrap_or(0),
        SearchMode::PathSearch { name_like, dir_hint, .. } => {
            let resolved_dirs = resolve_dir_hint(home_dir, &dir_hint)
                .map(|p| vec![p.to_string_lossy().to_string()])
                .unwrap_or_default();
            let resolved_dirs = if resolved_dirs.is_empty() {
                resolve_dirs_from_db(&conn, &dir_hint)
            } else {
                resolved_dirs
            };

            if !resolved_dirs.is_empty() {
                let mut sql_params: Vec<SqlValue> = Vec::new();
                let mut dir_conditions = Vec::new();
                let sep = std::path::MAIN_SEPARATOR;
                for d in &resolved_dirs {
                    let i = sql_params.len();
                    let pfx = format!("{d}{sep}");
                    let pfx_end = format!("{d}\x7F");
                    sql_params.push(SqlValue::Text(d.clone()));
                    sql_params.push(SqlValue::Text(pfx));
                    sql_params.push(SqlValue::Text(pfx_end));
                    dir_conditions.push(format!(
                        "(e.dir = ?{} OR (e.dir >= ?{} AND e.dir < ?{}))",
                        i + 1,
                        i + 2,
                        i + 3
                    ));
                }
                let dir_where = dir_conditions.join(" OR ");
                let name_filter = if name_like == "%" {
                    String::new()
                } else {
                    let ext_shortcut = extract_ext_from_like(&name_like);
                    if let Some(ext_val) = ext_shortcut {
                        let i = sql_params.len();
                        sql_params.push(SqlValue::Text(ext_val));
                        format!(" AND e.ext = ?{}", i + 1)
                    } else {
                        let i = sql_params.len();
                        sql_params.push(SqlValue::Text(name_like.clone()));
                        format!(" AND e.name LIKE ?{} ESCAPE '\\'", i + 1)
                    }
                };
                let sql = format!(
                    "SELECT COUNT(*) FROM entries e WHERE ({dir_where}){name_filter}"
                );
                conn.query_row(&sql, params_from_iter(sql_params.iter()), |r| r.get(0))
                    .unwrap_or(0)
            } else {
                let sep = std::path::MAIN_SEPARATOR;
                let native_hint = dir_hint.replace('/', &sep.to_string());
                let escaped_sep = escape_like(&sep.to_string());
                let dir_suffix = escape_like(&native_hint);
                let dir_like_exact = format!("%{escaped_sep}{dir_suffix}");
                let dir_like_sub = format!("%{escaped_sep}{dir_suffix}{escaped_sep}%");
                let ext_shortcut = extract_ext_from_like(&name_like);
                if let Some(ext_val) = ext_shortcut {
                    conn.query_row(
                        "SELECT COUNT(*) FROM entries e WHERE e.ext = ?1 AND (e.dir LIKE ?2 ESCAPE '\\' OR e.dir LIKE ?3 ESCAPE '\\')",
                        params![ext_val, dir_like_exact, dir_like_sub],
                        |r| r.get(0),
                    )
                    .unwrap_or(0)
                } else if name_like == "%" {
                    conn.query_row(
                        "SELECT COUNT(*) FROM entries e WHERE e.dir LIKE ?1 ESCAPE '\\' OR e.dir LIKE ?2 ESCAPE '\\'",
                        params![dir_like_exact, dir_like_sub],
                        |r| r.get(0),
                    )
                    .unwrap_or(0)
                } else {
                    conn.query_row(
                        "SELECT COUNT(*) FROM entries e WHERE (e.dir LIKE ?1 ESCAPE '\\' OR e.dir LIKE ?2 ESCAPE '\\') AND e.name LIKE ?3 ESCAPE '\\'",
                        params![dir_like_exact, dir_like_sub, name_like],
                        |r| r.get(0),
                    )
                    .unwrap_or(0)
                }
            }
        }
    }
}

fn execute_search(
    state: &AppState,
    query: String,
    limit: Option<u32>,
    offset: Option<u32>,
    sort_by: Option<String>,
    sort_dir: Option<String>,
) -> AppResult<SearchExecution> {
    let query = query.trim().to_string();
    let base_limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let effective_limit = if !query.is_empty() && query.chars().count() <= 1 {
        base_limit.min(SHORT_QUERY_LIMIT)
    } else {
        base_limit
    };
    let offset = offset.unwrap_or(0);

    let sort_by = sort_by.unwrap_or_else(|| "name".to_string());
    let sort_dir = sort_dir.unwrap_or_else(|| "asc".to_string());
    let (runtime_ignored_roots, runtime_ignored_patterns) = cached_effective_ignore_rules(state);

    #[cfg(target_os = "macos")]
    if !state.db_ready.load(AtomicOrdering::Acquire) {
        let spotlight = mac::spotlight_search::search_spotlight(&state.home_dir, &query);
        let mode_label = if spotlight.entries.is_empty() {
            "db_not_ready".to_string()
        } else if spotlight.timed_out {
            "spotlight_timeout".to_string()
        } else {
            "spotlight".to_string()
        };
        perf_log(format!(
            "spotlight_fallback db_not_ready query={:?} results={} timed_out={}",
            query,
            spotlight.entries.len(),
            spotlight.timed_out
        ));
        return Ok(SearchExecution {
            query,
            sort_by,
            sort_dir,
            effective_limit,
            offset,
            mode_label,
            results: spotlight.entries,
        });
    }

    let is_indexing = matches!(state.status.lock().state, IndexState::Indexing);
    let order_by = sort_clause(&sort_by, &sort_dir, "e.");

    let mut results = Vec::with_capacity(effective_limit as usize);
    let mode = parse_query(&query);
    let is_name_mode = matches!(&mode, SearchMode::NameSearch { .. });
    let allow_find_fallback = !is_indexing
        && matches!(
            &mode,
            SearchMode::GlobName { .. } | SearchMode::ExtSearch { .. }
        );
    let mut mode_label = match &mode {
        SearchMode::Empty => "empty",
        SearchMode::NameSearch { .. } => "name",
        SearchMode::GlobName { .. } => "glob",
        SearchMode::ExtSearch { .. } => "ext",
        SearchMode::PathSearch { .. } => "path",
    }
    .to_string();

    // Fast path: search in-memory index if available (DB upsert still in progress)
    {
        let guard = state.mem_index.read();
        if let Some(ref mi) = *guard {
            let mem_results = mem_search::search_mem_index(
                mi, &query, &mode, effective_limit, offset, &sort_by, &sort_dir,
            );
            mode_label = format!("mem_{mode_label}");
            return Ok(SearchExecution {
                query,
                sort_by,
                sort_dir,
                effective_limit,
                offset,
                mode_label,
                results: mem_results,
            });
        }
    }

    if is_name_mode && !is_indexing && offset == 0 {
        if let Some(cache_hit) = negative_name_cache_lookup(state, &query) {
            if cache_hit.age >= WATCH_DEBOUNCE
                && cache_hit.age <= NEGATIVE_CACHE_FALLBACK_WINDOW
                && !cache_hit.fallback_checked
            {
                let fallback_results = find_search(
                    &state.home_dir,
                    &runtime_ignored_roots,
                    &runtime_ignored_patterns,
                    &query,
                    effective_limit as usize,
                    &sort_by,
                    &sort_dir,
                );
                if !fallback_results.is_empty() {
                    remove_negative_name_query(state, &cache_hit.query_lower);
                    perf_log(format!(
                        "search_negative_cache_fallback_hit query={:?} age_ms={} results={}",
                        query,
                        cache_hit.age.as_millis(),
                        fallback_results.len()
                    ));
                    return Ok(SearchExecution {
                        query,
                        sort_by,
                        sort_dir,
                        effective_limit,
                        offset,
                        mode_label: "find_fallback".to_string(),
                        results: fallback_results,
                    });
                }
                mark_negative_name_fallback_checked(state, &cache_hit.query_lower);
            }

            perf_log(format!(
                "search_negative_cache_hit query={:?} age_ms={}",
                query,
                cache_hit.age.as_millis()
            ));
            return Ok(SearchExecution {
                query,
                sort_by,
                sort_dir,
                effective_limit,
                offset,
                mode_label: "name_neg_cache".to_string(),
                results: Vec::new(),
            });
        }
    }

    #[cfg(target_os = "macos")]
    let db_unavailable;
    match db_connection_for_search(&state.db_path) {
        Ok(conn) => {
            #[cfg(target_os = "macos")]
            { db_unavailable = false; }
            match &mode {
                SearchMode::Empty => {
                    let sql = format!(
                        r#"
                        SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                        FROM entries e
                        ORDER BY {order_by}
                        LIMIT ?1 OFFSET ?2
                        "#,
                    );
                    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                    let rows = stmt
                        .query_map(params![effective_limit, offset], row_to_entry)
                        .map_err(|e| e.to_string())?;
                    for row in rows {
                        results.push(row.map_err(|e| e.to_string())?);
                    }
                }

                SearchMode::NameSearch { name_like } => {
                    let escaped_query = escape_like(&query);
                    let exact_query = query.clone();
                    let prefix_like = format!("{}%", escaped_query);
                    let bare_order = sort_clause(&sort_by, &sort_dir, "");

                    if offset == 0 {
                        let exact_sql = format!(
                            r#"
                            SELECT path, name, dir, is_dir, ext, size, mtime
                            FROM entries
                            WHERE name COLLATE NOCASE = ?1
                            ORDER BY {bare_order}
                            LIMIT ?2
                            "#,
                        );
                        let mut stmt = conn.prepare(&exact_sql).map_err(|e| e.to_string())?;
                        let rows = stmt
                            .query_map(params![exact_query, effective_limit], row_to_entry)
                            .map_err(|e| e.to_string())?;
                        for row in rows {
                            results.push(row.map_err(|e| e.to_string())?);
                        }
                    }

                    if (results.len() as u32) < effective_limit {
                        let remaining = effective_limit - results.len() as u32;
                        let adj_offset = if offset > 0 {
                            offset.saturating_sub(results.len() as u32)
                        } else {
                            0
                        };
                        let prefix_sql = format!(
                            r#"
                            SELECT path, name, dir, is_dir, ext, size, mtime
                            FROM entries INDEXED BY idx_entries_name_nocase
                            WHERE name LIKE ?1 ESCAPE '\'
                              AND name COLLATE NOCASE != ?2
                            ORDER BY {bare_order}
                            LIMIT ?3 OFFSET ?4
                            "#,
                        );
                        let mut stmt = conn.prepare(&prefix_sql).map_err(|e| e.to_string())?;
                        let rows = stmt
                            .query_map(
                                params![prefix_like, exact_query, remaining, adj_offset],
                                row_to_entry,
                            )
                            .map_err(|e| e.to_string())?;
                        for row in rows {
                            results.push(row.map_err(|e| e.to_string())?);
                        }
                    }

                    if results.is_empty() && offset == 0 {
                        // Phase 2: contains-match (LIKE '%q%') with tight time budget.
                        // Combines probe + fetch in one pass to avoid double scan overhead.
                        let phase2_start = Instant::now();
                        conn.progress_handler(
                            2_000,
                            Some(move || phase2_start.elapsed().as_millis() > 5),
                        );

                        {

                            let phase2_sql = format!(
                                r#"
                                SELECT path, name, dir, is_dir, ext, size, mtime
                                FROM entries
                                WHERE name LIKE ?1 ESCAPE '\'
                                  AND name COLLATE NOCASE != ?2
                                  AND name NOT LIKE ?3 ESCAPE '\'
                                ORDER BY {bare_order}
                                LIMIT ?4
                                "#,
                            );
                            if let Ok(mut stmt2) = conn.prepare(&phase2_sql) {
                                if let Ok(rows2) = stmt2.query_map(
                                    params![name_like, exact_query, prefix_like, effective_limit],
                                    row_to_entry,
                                ) {
                                    for row in rows2 {
                                        match row {
                                            Ok(entry) => results.push(entry),
                                            Err(_) => break,
                                        }
                                    }
                                }
                            }
                        }

                        conn.progress_handler(0, None::<fn() -> bool>);
                    }
                }

                SearchMode::GlobName { name_like } => {
                    let sql = format!(
                        r#"
                        SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                        FROM entries e
                        WHERE e.name LIKE ?1 ESCAPE '\'
                        ORDER BY {order_by}
                        LIMIT ?2 OFFSET ?3
                        "#,
                    );
                    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                    let rows = stmt
                        .query_map(params![name_like, effective_limit, offset], row_to_entry)
                        .map_err(|e| e.to_string())?;
                    for row in rows {
                        results.push(row.map_err(|e| e.to_string())?);
                    }
                }

                SearchMode::ExtSearch { ext, name_like: _ } => {
                    let sql = format!(
                        r#"
                        SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                        FROM entries e
                        WHERE e.ext = ?1
                        ORDER BY {order_by}
                        LIMIT ?2 OFFSET ?3
                        "#,
                    );
                    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                    let rows = stmt
                        .query_map(params![ext, effective_limit, offset], row_to_entry)
                        .map_err(|e| e.to_string())?;
                    for row in rows {
                        results.push(row.map_err(|e| e.to_string())?);
                    }
                }

                SearchMode::PathSearch {
                    path_like: _,
                    name_like,
                    dir_hint,
                } => {
                    let resolved_dirs: Vec<String> = resolve_dir_hint(&state.home_dir, dir_hint)
                        .map(|p| vec![p.to_string_lossy().to_string()])
                        .unwrap_or_default();
                    let resolved_dirs = if resolved_dirs.is_empty() {
                        resolve_dirs_from_db(&conn, dir_hint)
                    } else {
                        resolved_dirs
                    };

                    if !resolved_dirs.is_empty() {
                        let ext_shortcut = extract_ext_from_like(name_like);
                        let mut sql_params: Vec<SqlValue> = Vec::new();
                        let mut dir_conditions = Vec::new();
                        let sep = std::path::MAIN_SEPARATOR;

                        for d in &resolved_dirs {
                            let i = sql_params.len();
                            let pfx = format!("{d}{sep}");
                            let pfx_end = format!("{d}\x7F");
                            sql_params.push(SqlValue::Text(d.clone()));
                            sql_params.push(SqlValue::Text(pfx));
                            sql_params.push(SqlValue::Text(pfx_end));
                            dir_conditions.push(format!(
                                "(e.dir = ?{} OR (e.dir >= ?{} AND e.dir < ?{}))",
                                i + 1,
                                i + 2,
                                i + 3
                            ));
                        }
                        let dir_where = dir_conditions.join(" OR ");

                        let name_filter = if name_like == "%" {
                            String::new()
                        } else if let Some(ref ext_val) = ext_shortcut {
                            let i = sql_params.len();
                            sql_params.push(SqlValue::Text(ext_val.clone()));
                            format!(" AND e.ext = ?{}", i + 1)
                        } else {
                            let i = sql_params.len();
                            sql_params.push(SqlValue::Text(name_like.clone()));
                            format!(" AND e.name LIKE ?{} ESCAPE '\\'", i + 1)
                        };

                        let limit_idx = sql_params.len() + 1;
                        let offset_idx = sql_params.len() + 2;
                        sql_params.push(SqlValue::Integer(effective_limit as i64));
                        sql_params.push(SqlValue::Integer(offset as i64));

                        let sql = format!(
                            r#"
                            SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                            FROM entries e
                            WHERE ({dir_where}){name_filter}
                            ORDER BY {order_by}
                            LIMIT ?{limit_idx} OFFSET ?{offset_idx}
                            "#,
                        );
                        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                        let rows = stmt
                            .query_map(params_from_iter(sql_params.iter()), row_to_entry)
                            .map_err(|e| e.to_string())?;
                        for row in rows {
                            results.push(row.map_err(|e| e.to_string())?);
                        }
                    } else {
                        let sep = std::path::MAIN_SEPARATOR;
                        let native_hint = dir_hint.replace('/', &sep.to_string());
                        let escaped_sep = escape_like(&sep.to_string());
                        let dir_suffix = escape_like(&native_hint);
                        let dir_like_exact = format!("%{escaped_sep}{dir_suffix}");
                        let dir_like_sub = format!("%{escaped_sep}{dir_suffix}{escaped_sep}%");
                        let ext_shortcut = extract_ext_from_like(name_like);

                        if let Some(ext_val) = ext_shortcut {
                            let sql = format!(
                                r#"
                                SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                                FROM entries e
                                WHERE e.ext = ?1
                                  AND (e.dir LIKE ?2 ESCAPE '\' OR e.dir LIKE ?3 ESCAPE '\')
                                ORDER BY {order_by}
                                LIMIT ?4 OFFSET ?5
                                "#,
                            );
                            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                            let rows = stmt
                                .query_map(
                                    params![
                                        ext_val,
                                        dir_like_exact,
                                        dir_like_sub,
                                        effective_limit,
                                        offset
                                    ],
                                    row_to_entry,
                                )
                                .map_err(|e| e.to_string())?;
                            for row in rows {
                                results.push(row.map_err(|e| e.to_string())?);
                            }
                        } else if name_like == "%" {
                            // Directory listing: no name filter needed, no time budget
                            let sql = format!(
                                r#"
                                SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                                FROM entries e
                                WHERE e.dir LIKE ?1 ESCAPE '\' OR e.dir LIKE ?2 ESCAPE '\'
                                ORDER BY {order_by}
                                LIMIT ?3 OFFSET ?4
                                "#,
                            );
                            let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                            let rows = stmt
                                .query_map(
                                    params![dir_like_exact, dir_like_sub, effective_limit, offset],
                                    row_to_entry,
                                )
                                .map_err(|e| e.to_string())?;
                            for row in rows {
                                results.push(row.map_err(|e| e.to_string())?);
                            }
                        } else {
                            // Phase A: fast prefix search via name index
                            // "%main%" -> strip leading '%' -> "main%" can use idx_entries_name_nocase
                            let prefix_like = if name_like.starts_with('%') {
                                let rest = &name_like[1..];
                                if !rest.is_empty() && !rest.starts_with('%') {
                                    Some(rest.to_string())
                                } else {
                                    None
                                }
                            } else {
                                // Already a prefix pattern (e.g. "test%"), use as-is
                                Some(name_like.clone())
                            };

                            if offset == 0 {
                                if let Some(ref pfx) = prefix_like {
                                    let sql = format!(
                                        r#"
                                        SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                                        FROM entries e INDEXED BY idx_entries_name_nocase
                                        WHERE e.name LIKE ?1 ESCAPE '\'
                                          AND (e.dir LIKE ?2 ESCAPE '\' OR e.dir LIKE ?3 ESCAPE '\')
                                        ORDER BY {order_by}
                                        LIMIT ?4
                                        "#,
                                    );
                                    if let Ok(mut stmt) = conn.prepare(&sql) {
                                        if let Ok(rows) = stmt.query_map(
                                            params![
                                                pfx,
                                                dir_like_exact,
                                                dir_like_sub,
                                                effective_limit
                                            ],
                                            row_to_entry,
                                        ) {
                                            for row in rows {
                                                match row {
                                                    Ok(entry) => results.push(entry),
                                                    Err(_) => break,
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Phase B: time-budgeted contains fallback if prefix found too few
                            if results.len() < effective_limit as usize {
                                let path_start = Instant::now();
                                conn.progress_handler(
                                    2_000,
                                    Some(move || path_start.elapsed().as_millis() > 5),
                                );

                                let sql = format!(
                                    r#"
                                    SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                                    FROM entries e
                                    WHERE (e.dir LIKE ?1 ESCAPE '\' OR e.dir LIKE ?2 ESCAPE '\')
                                      AND e.name LIKE ?3 ESCAPE '\'
                                    ORDER BY {order_by}
                                    LIMIT ?4 OFFSET ?5
                                    "#,
                                );
                                if let Ok(mut stmt) = conn.prepare(&sql) {
                                    if let Ok(rows) = stmt.query_map(
                                        params![
                                            dir_like_exact,
                                            dir_like_sub,
                                            name_like,
                                            effective_limit,
                                            offset
                                        ],
                                        row_to_entry,
                                    ) {
                                        for row in rows {
                                            match row {
                                                Ok(entry) => results.push(entry),
                                                Err(_) => break,
                                            }
                                        }
                                    }
                                }

                                conn.progress_handler(0, None::<fn() -> bool>);

                                // Deduplicate (Phase A prefix results overlap with Phase B contains)
                                let mut seen = std::collections::HashSet::new();
                                results.retain(|e| seen.insert(e.path.clone()));
                                results.truncate(effective_limit as usize);
                            }
                        }
                    }
                }
            }

            if results.is_empty() && !query.is_empty() && offset == 0 && allow_find_fallback {
                results = find_search(
                    &state.home_dir,
                    &runtime_ignored_roots,
                    &runtime_ignored_patterns,
                    &query,
                    effective_limit as usize,
                    &sort_by,
                    &sort_dir,
                );
                mode_label = "find_fallback".to_string();
            }
        }
        Err(_) => {
            #[cfg(target_os = "macos")]
            { db_unavailable = true; }
            perf_log(format!("search_db_unavailable query={:?}", query));
        }
    }

    #[cfg(target_os = "macos")]
    if !query.is_empty() && offset == 0 {
        let should_spotlight = if results.is_empty() {
            true
        } else if results.len() >= effective_limit as usize {
            false
        } else {
            is_indexing || db_unavailable
        };
        if should_spotlight {
            let spotlight = mac::spotlight_search::search_spotlight(&state.home_dir, &query);
            if !spotlight.entries.is_empty() {
                if results.is_empty() {
                    perf_log(format!(
                        "spotlight_fallback empty_results query={:?} results={} timed_out={}",
                        query,
                        spotlight.entries.len(),
                        spotlight.timed_out
                    ));
                    results = spotlight.entries;
                    mode_label = if spotlight.timed_out {
                        "spotlight_timeout".to_string()
                    } else {
                        "spotlight".to_string()
                    };
                } else {
                    let existing_paths: std::collections::HashSet<String> =
                        results.iter().map(|e| e.path.clone()).collect();
                    let mut merged_count = 0usize;
                    for entry in spotlight.entries {
                        if !existing_paths.contains(&entry.path) {
                            results.push(entry);
                            merged_count += 1;
                        }
                    }
                    if merged_count > 0 {
                        perf_log(format!(
                            "spotlight_merge indexing query={:?} merged={} timed_out={}",
                            query, merged_count, spotlight.timed_out
                        ));
                        mode_label = format!("{}_+spotlight", mode_label);
                    }
                }
            }
        }
    }

    results = filter_ignored_entries(results, &runtime_ignored_roots, &runtime_ignored_patterns);
    results.truncate(effective_limit as usize);
    if offset == 0 {
        if sort_by == "name" {
            sort_entries_with_relevance(&mut results, &query, &sort_by, &sort_dir);
        } else {
            sort_entries(&mut results, &sort_by, &sort_dir);
        }
    }
    if is_name_mode && !is_indexing && offset == 0 && results.is_empty() && !query.is_empty() {
        remember_negative_name_query(state, &query);
    }

    Ok(SearchExecution {
        query,
        sort_by,
        sort_dir,
        effective_limit,
        offset,
        mode_label,
        results,
    })
}

#[tauri::command]
async fn search(
    _app: AppHandle,
    query: String,
    limit: Option<u32>,
    offset: Option<u32>,
    sort_by: Option<String>,
    sort_dir: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<SearchResultDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let started = Instant::now();
        let execution = execute_search(&state, query, limit, offset, sort_by, sort_dir)?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

        log_search(
            &state.db_path,
            &execution.query,
            &execution.mode_label,
            &execution.results,
        );

        if perf_log_enabled() {
            let top: Vec<&str> = execution
                .results
                .iter()
                .take(5)
                .map(|entry| entry.name.as_str())
                .collect();
            perf_log(format!(
                "search query={:?} mode={} sort={}/{} limit={} offset={} results={} elapsed_ms={:.3} top={:?}",
                execution.query,
                execution.mode_label,
                execution.sort_by,
                execution.sort_dir,
                execution.effective_limit,
                execution.offset,
                execution.results.len(),
                elapsed_ms,
                top,
            ));
        }

        let total_count = compute_total_count(&state.db_path, &state.home_dir, &execution);
        Ok(SearchResultDto {
            entries: execution.results,
            mode_label: execution.mode_label,
            total_count,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn quick_look(path: String) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        {
            Command::new("qlmanage")
                .args(["-p", &path])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        #[cfg(target_os = "windows")]
        {
            Command::new("explorer")
                .arg(&path)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Command::new("xdg-open")
                .arg(&path)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open(paths: Vec<String>) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || {
        for path in &paths {
            #[cfg(target_os = "macos")]
            {
                let output = Command::new("open")
                    .arg(path)
                    .output()
                    .map_err(|e| e.to_string())?;

                if !output.status.success() && Path::new(path).is_dir() {
                    let fallback = Command::new("open")
                        .args(["-R", path])
                        .status()
                        .map_err(|e| e.to_string())?;

                    if !fallback.success() {
                        return Err(format!("Failed to open: {path}"));
                    }
                } else if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("Failed to open: {path} ({stderr})",));
                }
            }
            #[cfg(target_os = "windows")]
            {
                let mut cmd = Command::new("cmd");
                cmd.raw_arg(format!("/C start \"\" \"{}\"", path.replace('"', "")));
                let status = cmd.status().map_err(|e| e.to_string())?;
                if !status.success() {
                    return Err(format!("Failed to open: {path}"));
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let status = Command::new("xdg-open")
                    .arg(path)
                    .status()
                    .map_err(|e| e.to_string())?;
                if !status.success() {
                    return Err(format!("Failed to open: {path}"));
                }
            }
        }

        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

fn reveal_in_finder_impl(paths: Vec<String>) -> AppResult<()> {
    if paths.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        if paths.len() == 1 {
            let status = Command::new("open")
                .arg("-R")
                .arg(&paths[0])
                .status()
                .map_err(|e| e.to_string())?;

            if !status.success() {
                return Err(format!("Failed to reveal in Finder: {}", paths[0]));
            }

            return Ok(());
        }

        let mut unique_parents: HashSet<PathBuf> = HashSet::new();
        for path in &paths {
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
                return Err(format!(
                    "Failed to open in Finder: {}",
                    parent.to_string_lossy()
                ));
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        for path in &paths {
            let mut cmd = Command::new("explorer");
            cmd.raw_arg(format!("/select,\"{}\"", path.replace('"', "")));
            let _ = cmd.status();
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        for path in &paths {
            let target = Path::new(path);
            let dir = if target.is_dir() {
                path.as_str()
            } else {
                target
                    .parent()
                    .map(|p| p.to_str().unwrap_or("/"))
                    .unwrap_or("/")
            };
            let _ = Command::new("xdg-open").arg(dir).status();
        }
    }

    Ok(())
}

fn copy_with_command(program: &str, args: &[&str], text: &str) -> AppResult<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to run {program}: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("Failed to write to clipboard: {e}"))?;
    } else {
        return Err("Cannot open clipboard input stream.".to_string());
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed to wait for {program}: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} execution failed."))
    }
}

#[cfg(target_os = "macos")]
fn copy_text_to_clipboard(text: &str) -> AppResult<()> {
    copy_with_command("pbcopy", &[], text)
}

#[cfg(target_os = "windows")]
fn copy_text_to_clipboard(text: &str) -> AppResult<()> {
    copy_with_command("cmd", &["/C", "clip"], text)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn copy_text_to_clipboard(text: &str) -> AppResult<()> {
    let mut last_error = None;

    match copy_with_command("wl-copy", &[], text) {
        Ok(()) => return Ok(()),
        Err(err) => last_error = Some(err),
    }
    match copy_with_command("xclip", &["-selection", "clipboard"], text) {
        Ok(()) => return Ok(()),
        Err(err) => last_error = Some(err),
    }
    match copy_with_command("xsel", &["--clipboard", "--input"], text) {
        Ok(()) => return Ok(()),
        Err(err) => last_error = Some(err),
    }

    Err(last_error.unwrap_or_else(|| {
        "No supported clipboard tool found. Please install wl-copy, xclip, or xsel.".to_string()
    }))
}

#[tauri::command]
async fn open_with(path: String) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || reveal_in_finder_impl(vec![path]))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn reveal_in_finder(paths: Vec<String>) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || reveal_in_finder_impl(paths))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn copy_paths(paths: Vec<String>) -> AppResult<()> {
    copy_text_to_clipboard(&paths.join("\n"))
}

#[cfg(target_os = "macos")]
fn copy_files_to_clipboard(paths: &[String]) -> AppResult<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let file_exprs: Vec<String> = paths
        .iter()
        .map(|p| {
            let escaped = p.replace('\\', "\\\\").replace('"', "\\\"");
            format!("POSIX file \"{}\"", escaped)
        })
        .collect();
    let script = if file_exprs.len() == 1 {
        format!("set the clipboard to {}", file_exprs[0])
    } else {
        format!("set the clipboard to {{{}}}", file_exprs.join(", "))
    };
    let status = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("Failed to copy files to clipboard".to_string());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[tauri::command]
fn copy_files(paths: Vec<String>) -> AppResult<()> {
    copy_files_to_clipboard(&paths)
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn copy_files(_paths: Vec<String>) -> AppResult<()> {
    Err("copy_files is only supported on macOS".to_string())
}

#[tauri::command]
async fn move_to_trash(
    paths: Vec<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut deleted_targets = Vec::new();

        for path in &paths {
            trash::delete(path).map_err(|e| e.to_string())?;
            remember_op(&state, "trash", Some(path.clone()), None);
            deleted_targets.push(path.clone());
        }

        let mut conn = db_connection(&state.db_path)?;
        let _ = delete_paths(&mut conn, &deleted_targets)?;
        invalidate_search_caches(&state);

        refresh_and_emit_status_counts(&app, &state)?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn rename(
    path: String,
    new_name: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<EntryDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let validated_name = validate_new_name(&new_name)?;
        let old_path = PathBuf::from(&path);

        if !old_path.exists() {
            return Err("Source file does not exist.".to_string());
        }

        let parent = old_path
            .parent()
            .ok_or_else(|| "Parent directory not found.".to_string())?;

        let new_path = parent.join(&validated_name);
        if new_path == old_path {
            let meta = fs::symlink_metadata(&old_path).ok();
            return Ok(EntryDto {
                path: path.clone(),
                name: old_path
                    .file_name()
                    .map(|v| v.to_string_lossy().to_string())
                    .unwrap_or_else(|| validated_name.clone()),
                dir: parent.to_string_lossy().to_string(),
                is_dir: old_path.is_dir(),
                ext: extension_for(&old_path, old_path.is_dir()),
                size: meta
                    .as_ref()
                    .filter(|m| m.is_file())
                    .map(|m| m.len() as i64),
                mtime: meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64),
            });
        }

        if new_path.exists() {
            return Err("A file/folder with the same name already exists.".to_string());
        }

        let original_is_dir = old_path.is_dir();
        fs::rename(&old_path, &new_path).map_err(|e| e.to_string())?;

        let mut conn = db_connection(&state.db_path)?;
        let _ = delete_paths(&mut conn, &[path.clone()])?;

        if original_is_dir {
            let rows =
                collect_rows_recursive(&new_path, &state.path_ignores, &state.path_ignore_patterns);
            for chunk in rows.chunks(BATCH_SIZE) {
                let _ = upsert_rows(&mut conn, chunk)?;
            }
        } else {
            let row = index_row_from_path(&new_path)
                .ok_or_else(|| "Cannot read renamed file info.".to_string())?;
            let _ = upsert_rows(&mut conn, &[row])?;
        }

        invalidate_search_caches(&state);

        remember_op(
            &state,
            "rename",
            Some(old_path.to_string_lossy().to_string()),
            Some(new_path.to_string_lossy().to_string()),
        );

        refresh_and_emit_status_counts(&app, &state)?;

        let new_meta = fs::symlink_metadata(&new_path).ok();
        Ok(EntryDto {
            path: new_path.to_string_lossy().to_string(),
            name: validated_name,
            dir: parent.to_string_lossy().to_string(),
            is_dir: original_is_dir,
            ext: extension_for(&new_path, original_is_dir),
            size: new_meta
                .as_ref()
                .filter(|m| m.is_file())
                .map(|m| m.len() as i64),
            mtime: new_meta
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn fd_search(
    query: String,
    limit: Option<u32>,
    offset: Option<u32>,
    sort_by: Option<String>,
    sort_dir: Option<String>,
    state: State<'_, AppState>,
) -> AppResult<FdSearchResultDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let query = query.trim().to_string();
        let limit = limit.unwrap_or(500).clamp(1, 5000) as usize;
        let offset = offset.unwrap_or(0) as usize;
        let sort_by = sort_by.unwrap_or_else(|| "name".to_string());
        let sort_dir = sort_dir.unwrap_or_else(|| "asc".to_string());
        let (runtime_ignored_roots, runtime_ignored_patterns) =
            cached_effective_ignore_rules(&state);
        let ignore_fingerprint =
            ignore_rules_fingerprint(&runtime_ignored_roots, &runtime_ignored_patterns);

        if query.is_empty() {
            return Ok(FdSearchResultDto {
                entries: Vec::new(),
                total: 0,
                timed_out: false,
            });
        }

        let mut cache = state.fd_search_cache.lock();

        let cache_hit = cache
            .as_ref()
            .map(|c| {
                c.query == query
                    && c.sort_by == sort_by
                    && c.sort_dir == sort_dir
                    && c.ignore_fingerprint == ignore_fingerprint
            })
            .unwrap_or(false);

        let mut timed_out = false;

        if !cache_hit {
            let result = fd_search::run_fd_search(
                &state.scan_root,
                &runtime_ignored_roots,
                &runtime_ignored_patterns,
                &query,
                &sort_by,
                &sort_dir,
            );
            timed_out = result.timed_out;

            *cache = Some(FdSearchCache {
                query: query.clone(),
                sort_by: sort_by.clone(),
                sort_dir: sort_dir.clone(),
                ignore_fingerprint,
                entries: result.entries,
            });
        }

        let cached = cache.as_ref().unwrap();
        let total = cached.entries.len() as u64;
        let end = (offset + limit).min(cached.entries.len());
        let page = if offset < cached.entries.len() {
            cached.entries[offset..end].to_vec()
        } else {
            Vec::new()
        };

        Ok(FdSearchResultDto {
            entries: page,
            total,
            timed_out,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn frontend_log(msg: String) {
    eprintln!("{msg}");
}

#[tauri::command]
fn get_platform() -> String {
    if cfg!(target_os = "windows") {
        "windows".to_string()
    } else if cfg!(target_os = "macos") {
        "macos".to_string()
    } else {
        "linux".to_string()
    }
}

#[cfg(target_os = "windows")]
#[tauri::command]
async fn show_context_menu(
    paths: Vec<String>,
    x: f64,
    y: f64,
    _single_selection: bool,
    app: AppHandle,
) -> AppResult<()> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;

    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let win_pos = window.inner_position().map_err(|e| e.to_string())?;
    let screen_x = win_pos.x + (x * scale) as i32;
    let screen_y = win_pos.y + (y * scale) as i32;

    let hwnd_raw = window.hwnd().map_err(|e| e.to_string())?.0 as isize;

    // TrackPopupMenu must run on the thread that owns the HWND (main UI thread).
    // Use a channel to relay the result back to the async context.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    app.run_on_main_thread(move || {
        let result = win::context_menu::show(hwnd_raw, &paths, screen_x, screen_y);
        let _ = tx.send(result);
    })
    .map_err(|e| e.to_string())?;

    tauri::async_runtime::spawn_blocking(move || {
        rx.recv().map_err(|e| format!("context menu channel: {e}"))?
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(target_os = "macos")]
#[tauri::command]
async fn show_context_menu(
    _paths: Vec<String>,
    x: f64,
    y: f64,
    single_selection: bool,
    app: AppHandle,
) -> AppResult<()> {
    use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem};

    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;

    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    let app_clone = app.clone();
    app.run_on_main_thread(move || {
        let app = app_clone;
        let result = (|| -> Result<(), tauri::Error> {
            let open = MenuItem::with_id(&app, "ctx_open", "Open", true, None::<&str>)?;
            let quick_look =
                MenuItem::with_id(&app, "ctx_quick_look", "Quick Look", true, None::<&str>)?;
            let open_with =
                MenuItem::with_id(&app, "ctx_open_with", "Open With...", true, None::<&str>)?;
            let sep1 = PredefinedMenuItem::separator(&app)?;
            let reveal = MenuItem::with_id(
                &app,
                "ctx_reveal",
                "Reveal in Finder",
                true,
                None::<&str>,
            )?;
            let sep2 = PredefinedMenuItem::separator(&app)?;
            let copy_files =
                MenuItem::with_id(&app, "ctx_copy_files", "Copy", true, None::<&str>)?;
            let copy_path =
                MenuItem::with_id(&app, "ctx_copy_path", "Copy Path", true, None::<&str>)?;
            let sep3 = PredefinedMenuItem::separator(&app)?;
            let trash = MenuItem::with_id(
                &app,
                "ctx_trash",
                "Move to Trash",
                true,
                None::<&str>,
            )?;
            let rename =
                MenuItem::with_id(&app, "ctx_rename", "Rename", true, None::<&str>)?;

            let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![
                &open, &quick_look, &open_with, &sep1, &reveal, &sep2, &copy_files, &copy_path, &sep3, &trash,
            ];
            if single_selection {
                items.push(&rename);
            }

            let menu = Menu::with_items(&app, &items)?;
            // Position is window-relative logical pixels, matching clientX/clientY.
            window.popup_menu_at(&menu, tauri::LogicalPosition::new(x, y))?;
            Ok(())
        })();
        let _ = tx.send(result.map_err(|e| e.to_string()));
    })
    .map_err(|e| e.to_string())?;

    tauri::async_runtime::spawn_blocking(move || {
        rx.recv().map_err(|e| format!("context menu channel: {e}"))?
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[tauri::command]
async fn show_context_menu(
    _paths: Vec<String>,
    _x: f64,
    _y: f64,
    _single_selection: bool,
    _app: AppHandle,
) -> AppResult<()> {
    Err("Native context menu is only supported on Windows and macOS".to_string())
}

#[tauri::command]
async fn get_file_icon(
    path: Option<String>,
    ext: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<u8>> {
    let state = state.inner().clone();
    Ok(tauri::async_runtime::spawn_blocking(move || {
        let ext_lower = if ext.trim().is_empty() {
            "__default__".to_string()
        } else {
            ext.to_lowercase()
        };

        #[cfg(target_os = "windows")]
        let cache_key = {
            if is_per_file_icon_ext(&ext_lower) {
                path.as_deref().unwrap_or(&ext_lower).to_string()
            } else {
                ext_lower.clone()
            }
        };
        #[cfg(not(target_os = "windows"))]
        let cache_key = ext_lower.clone();

        if let Some(cached) = state.icon_cache.lock().get(&cache_key).cloned() {
            return cached;
        }

        #[cfg(target_os = "windows")]
        let icon = {
            let from_path = path
                .as_deref()
                .filter(|p| !p.is_empty())
                .and_then(win::icon::load_icon_png);
            from_path.or_else(|| load_system_icon_png(&ext_lower))
        };
        #[cfg(not(target_os = "windows"))]
        let icon = {
            let _ = path;
            load_system_icon_png(&ext_lower)
        };

        let icon = icon.unwrap_or_default();
        state.icon_cache.lock().insert(cache_key, icon.clone());
        icon
    })
    .await
    .unwrap_or_default())
}

fn default_bench_cases() -> Vec<BenchCase> {
    vec![
        BenchCase {
            id: "TC01_exact_name",
            query: "report_00042",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 1,
        },
        BenchCase {
            id: "TC02_prefix_name",
            query: "report_00",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 10,
        },
        BenchCase {
            id: "TC03_contains_name",
            query: "invoice",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 1,
        },
        BenchCase {
            id: "TC04_ext_md",
            query: "*.md",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 1,
        },
        BenchCase {
            id: "TC05_path_glob_png",
            query: "Desktop/ *.png",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 1,
        },
        BenchCase {
            id: "TC06_path_term",
            query: "Projects/rust",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 1,
        },
        BenchCase {
            id: "TC07_path_ext_rs",
            query: "Projects/ *.rs",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 1,
        },
        BenchCase {
            id: "TC08_no_match",
            query: "zzzz_not_exists_12345",
            sort_by: "name",
            sort_dir: "asc",
            limit: 300,
            offset: 0,
            expected_min_results: 0,
        },
    ]
}

fn bench_iterations() -> u32 {
    std::env::var("EVERYTHING_BENCH_ITERATIONS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v.clamp(1, 50))
        .unwrap_or(5)
}

fn bench_wait_timeout() -> Duration {
    let seconds = std::env::var("EVERYTHING_BENCH_WAIT_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|v| v.clamp(10, 7200))
        .unwrap_or(1800);
    Duration::from_secs(seconds)
}

fn bench_run_label() -> String {
    std::env::var("EVERYTHING_BENCH_RUN_LABEL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("run-{}", now_epoch()))
}

fn bench_output_path(db_path: &Path, run_label: &str) -> PathBuf {
    std::env::var("EVERYTHING_BENCH_OUTPUT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| db_path.with_file_name(format!("bench-report-{run_label}.json")))
}

fn write_bench_report(path: &Path, report: &BenchReport) -> AppResult<()> {
    let json = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

fn start_bench_runner(app_handle: AppHandle, state: AppState) {
    std::thread::spawn(move || {
        let run_label = bench_run_label();
        let output_path = bench_output_path(&state.db_path, &run_label);
        let iterations = bench_iterations();
        let wait_timeout = bench_wait_timeout();
        let started_at = now_epoch();

        perf_log(format!(
            "bench_start run_label={} output={} iterations={} wait_timeout_s={}",
            run_label,
            output_path.to_string_lossy(),
            iterations,
            wait_timeout.as_secs(),
        ));

        let wait_started = Instant::now();
        let mut saw_indexing = false;
        let index_snapshot: IndexStatus;
        loop {
            let snapshot = state.status.lock().clone();
            let active = state.indexing_active.load(AtomicOrdering::Acquire)
                || matches!(snapshot.state, IndexState::Indexing);

            if active {
                saw_indexing = true;
            }

            if state.db_ready.load(AtomicOrdering::Acquire) && saw_indexing && !active {
                index_snapshot = snapshot;
                break;
            }

            if wait_started.elapsed() >= wait_timeout {
                let report = BenchReport {
                    run_label: run_label.clone(),
                    started_at,
                    completed_at: now_epoch(),
                    home_dir: state.home_dir.to_string_lossy().to_string(),
                    db_path: state.db_path.to_string_lossy().to_string(),
                    index_wait_ms: wait_started.elapsed().as_millis(),
                    index_scanned: snapshot.scanned,
                    index_indexed: snapshot.indexed,
                    index_entries_count: snapshot.entries_count,
                    index_permission_errors: snapshot.permission_errors,
                    index_message: Some("Timed out waiting for index ready".to_string()),
                    search_iterations: iterations,
                    search_results: Vec::new(),
                };
                let _ = write_bench_report(&output_path, &report);
                perf_log(format!(
                    "bench_timeout run_label={} waited_ms={}",
                    run_label,
                    wait_started.elapsed().as_millis()
                ));
                if env_truthy("EVERYTHING_BENCH_EXIT") {
                    app_handle.exit(2);
                }
                return;
            }

            std::thread::sleep(Duration::from_millis(200));
        }

        let index_wait_ms = wait_started.elapsed().as_millis();
        perf_log(format!(
            "bench_index_ready run_label={} waited_ms={} scanned={} indexed={} entries={} permission_errors={}",
            run_label,
            index_wait_ms,
            index_snapshot.scanned,
            index_snapshot.indexed,
            index_snapshot.entries_count,
            index_snapshot.permission_errors,
        ));

        let mut search_results = Vec::new();
        for case in default_bench_cases() {
            let mut elapsed_sum = 0.0f64;
            let mut success_count = 0u32;
            let mut result_count = 0usize;
            let mut mode = String::new();
            let mut top_results = Vec::new();
            let mut case_error = None;

            for iter in 1..=iterations {
                let started = Instant::now();
                match execute_search(
                    &state,
                    case.query.to_string(),
                    Some(case.limit),
                    Some(case.offset),
                    Some(case.sort_by.to_string()),
                    Some(case.sort_dir.to_string()),
                ) {
                    Ok(execution) => {
                        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                        elapsed_sum += elapsed_ms;
                        success_count += 1;
                        result_count = execution.results.len();
                        mode = execution.mode_label;
                        top_results = execution
                            .results
                            .iter()
                            .take(5)
                            .map(|entry| entry.path.clone())
                            .collect();

                        perf_log(format!(
                            "bench_query run_label={} case={} iter={}/{} query={:?} mode={} results={} elapsed_ms={:.3}",
                            run_label,
                            case.id,
                            iter,
                            iterations,
                            case.query,
                            mode,
                            result_count,
                            elapsed_ms
                        ));
                    }
                    Err(err) => {
                        case_error = Some(err);
                        break;
                    }
                }
            }

            let elapsed_ms = if success_count > 0 {
                elapsed_sum / success_count as f64
            } else {
                0.0
            };
            let passed = case_error.is_none() && result_count >= case.expected_min_results;

            search_results.push(BenchCaseResult {
                id: case.id.to_string(),
                query: case.query.to_string(),
                mode,
                sort_by: case.sort_by.to_string(),
                sort_dir: case.sort_dir.to_string(),
                limit: case.limit,
                offset: case.offset,
                elapsed_ms,
                result_count,
                expected_min_results: case.expected_min_results,
                passed,
                top_results,
                error: case_error,
            });
        }

        let report = BenchReport {
            run_label: run_label.clone(),
            started_at,
            completed_at: now_epoch(),
            home_dir: state.home_dir.to_string_lossy().to_string(),
            db_path: state.db_path.to_string_lossy().to_string(),
            index_wait_ms,
            index_scanned: index_snapshot.scanned,
            index_indexed: index_snapshot.indexed,
            index_entries_count: index_snapshot.entries_count,
            index_permission_errors: index_snapshot.permission_errors,
            index_message: index_snapshot.message.clone(),
            search_iterations: iterations,
            search_results,
        };

        match write_bench_report(&output_path, &report) {
            Ok(()) => perf_log(format!(
                "bench_report_written run_label={} path={}",
                run_label,
                output_path.to_string_lossy()
            )),
            Err(err) => perf_log(format!(
                "bench_report_write_error run_label={} path={} err={}",
                run_label,
                output_path.to_string_lossy(),
                err
            )),
        }

        if env_truthy("EVERYTHING_BENCH_EXIT") {
            app_handle.exit(0);
        }
    });
}

#[cfg(target_os = "macos")]
fn register_global_shortcut(app: &AppHandle) -> AppResult<()> {
    let shortcut = Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::Space);

    app.global_shortcut()
        .on_shortcut(shortcut, move |app_handle, _, _| {
            if let Some(window) = app_handle.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
            let _ = app_handle.emit("focus_search", ());
        })
        .map_err(|e| format!("Failed to register global shortcut: {e}"))
}

#[cfg(not(target_os = "macos"))]
fn register_global_shortcut(_app: &AppHandle) -> AppResult<()> {
    Ok(())
}

fn setup_app(app: &mut tauri::App) -> AppResult<()> {
    let setup_started = std::time::Instant::now();
    eprintln!("[startup] setup_app() entered");
    let bench_mode = bench_mode_enabled();

    #[cfg(target_os = "macos")]
    if let Some(window) = app.get_webview_window("main") {
        use window_vibrancy::{apply_vibrancy, NSVisualEffectMaterial};
        if let Err(e) = apply_vibrancy(&window, NSVisualEffectMaterial::UnderWindowBackground, None, None) {
            eprintln!("[vibrancy] apply failed: {e}");
        }
        let _ = window.show();
    }

    #[cfg(target_os = "windows")]
    if let Some(window) = app.get_webview_window("main") {
        use tauri::window::Color;
        let is_dark = window.theme().map(|t| t == tauri::Theme::Dark).unwrap_or(false);
        let bg = if is_dark {
            Color(0x1f, 0x1f, 0x1f, 0xff)
        } else {
            Color(0xf4, 0xf5, 0xf7, 0xff)
        };
        let _ = window.set_background_color(Some(bg));
        // Show window after background color is set to avoid white flash.
        // The skeleton HTML renders on top of this matching background.
        let _ = window.show();
    }

    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {e}"))?;
    fs::create_dir_all(&app_data_dir).map_err(|e| e.to_string())?;

    let db_path = app_data_dir.join("index.db");
    let home_dir = PathBuf::from(
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| if cfg!(windows) { "C:\\".to_string() } else { "/".to_string() }),
    );
    let scan_root = if cfg!(windows) {
        PathBuf::from("C:\\")
    } else {
        home_dir.clone()
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| home_dir.clone());
    eprintln!("[startup] +{}ms loading pathignore rules...", setup_started.elapsed().as_millis());
    let (mut path_ignores, path_ignore_patterns) = load_pathignore_rules(&home_dir, &cwd);
    if !path_ignores.iter().any(|r| r == &app_data_dir) {
        path_ignores.push(app_data_dir.clone());
    }
    for root in load_gitignore_roots(&home_dir, &cwd) {
        if !path_ignores.iter().any(|r| r == &root) {
            path_ignores.push(root);
        }
    }
    eprintln!("[startup] +{}ms pathignore done", setup_started.elapsed().as_millis());

    let gitignore = Arc::new(gitignore_filter::LazyGitignoreFilter::new(home_dir.clone()));

    let state = AppState {
        db_path,
        home_dir,
        scan_root,
        cwd,
        path_ignores: Arc::new(path_ignores),
        path_ignore_patterns: Arc::new(path_ignore_patterns),
        db_ready: Arc::new(AtomicBool::new(false)),
        indexing_active: Arc::new(AtomicBool::new(false)),
        status: Arc::new(Mutex::new(IndexStatus::default())),
        recent_ops: Arc::new(Mutex::new(Vec::new())),
        icon_cache: Arc::new(Mutex::new(HashMap::new())),
        fd_search_cache: Arc::new(Mutex::new(None)),
        negative_name_cache: Arc::new(Mutex::new(Vec::new())),
        ignore_cache: Arc::new(Mutex::new(None)),
        gitignore,
        mem_index: Arc::new(RwLock::new(None)),
        watcher_stop: Arc::new(AtomicBool::new(false)),
        watcher_active: Arc::new(AtomicBool::new(false)),
    };

    eprintln!("[startup] +{}ms AppState created", setup_started.elapsed().as_millis());
    app.manage(state.clone());
    if !bench_mode {
        register_global_shortcut(&app.handle())?;
    }
    // Context menu item IDs use the "ctx_" prefix by convention.
    // All matching IDs are forwarded as "context_menu_action" events to the frontend.
    #[cfg(target_os = "macos")]
    {
        app.handle().on_menu_event(|app, event| {
            let action = match event.id().as_ref() {
                "ctx_open" => "open",
                "ctx_quick_look" => "quick_look",
                "ctx_open_with" => "open_with",
                "ctx_reveal" => "reveal",
                "ctx_copy_files" => "copy_files",
                "ctx_copy_path" => "copy_path",
                "ctx_trash" => "trash",
                "ctx_rename" => "rename",
                _ => return,
            };
            let _ = app.emit("context_menu_action", action);
        });
    }
    eprintln!("[startup] +{}ms global shortcut registered", setup_started.elapsed().as_millis());
    if bench_mode {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.hide();
        }
    }

    eprintln!("[startup] +{}ms setup_app() done, spawning init thread", setup_started.elapsed().as_millis());

    let app_handle = app.handle().clone();
    std::thread::spawn(move || {
        let thread_started = std::time::Instant::now();
        eprintln!("[startup/thread] init thread started");

        eprintln!("[startup/thread] +{}ms calling init_db...", thread_started.elapsed().as_millis());
        if let Err(err) = init_db(&state.db_path) {
            set_state(&state, IndexState::Error, Some(err.clone()));
            emit_index_state(&app_handle, "Error", Some(err));
            return;
        }
        eprintln!("[startup/thread] +{}ms init_db done", thread_started.elapsed().as_millis());

        state.db_ready.store(true, AtomicOrdering::Release);
        eprintln!("[startup/thread] +{}ms db_ready=true — launching indexing immediately", thread_started.elapsed().as_millis());

        // Deferred housekeeping — purge + status counts run in background
        {
            let hk_app = app_handle.clone();
            let hk_state = state.clone();
            std::thread::spawn(move || {
                // Brief pause so indexing thread can start first
                std::thread::sleep(std::time::Duration::from_millis(500));
                let hk_started = std::time::Instant::now();
                let _ = refresh_and_emit_status_counts(&hk_app, &hk_state);
                eprintln!("[startup/housekeeping] refresh_and_emit_status_counts done in {}ms", hk_started.elapsed().as_millis());
                if let Err(err) = purge_ignored_entries(&hk_state.db_path, &hk_state.path_ignores) {
                    eprintln!("[startup/housekeeping] purge_ignored_entries failed: {err}");
                }
                eprintln!("[startup/housekeeping] all done in {}ms", hk_started.elapsed().as_millis());
            });
        }

        #[cfg(target_os = "macos")]
        {
            if bench_mode {
                let _ = start_full_index_worker(app_handle.clone(), state.clone());
            } else {
                let (stored_event_id, index_complete) = db_connection(&state.db_path)
                    .ok()
                    .map(|c| {
                        let eid = get_meta(&c, "last_event_id")
                            .and_then(|v| v.parse::<u64>().ok());
                        let complete = get_meta(&c, "index_complete")
                            .map(|v| v == "1")
                            .unwrap_or(false);
                        (eid, complete)
                    })
                    .unwrap_or((None, false));

                if stored_event_id.is_some() && index_complete {
                    // Conditional startup: try watcher replay first, skip full scan if OK
                    start_fsevent_watcher_worker(
                        app_handle.clone(),
                        state.clone(),
                        stored_event_id,
                        true,
                    );
                } else {
                    if !index_complete {
                        eprintln!("[mac] index incomplete — starting full index");
                    }
                    let _ = start_full_index_worker(app_handle.clone(), state.clone());
                    start_fsevent_watcher_worker(app_handle.clone(), state.clone(), None, false);
                }
            }
        }

        #[cfg(target_os = "windows")]
        {
            win::start_windows_indexing(app_handle.clone(), state.clone());
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = start_full_index_worker(app_handle.clone(), state.clone());
        }

        if bench_mode {
            start_bench_runner(app_handle.clone(), state.clone());
        }

        #[cfg(not(target_os = "windows"))]
        if !bench_mode {
            let icon_cache = state.icon_cache.clone();
            std::thread::spawn(move || {
                let exts = [
                    "txt", "pdf", "png", "jpg", "md", "json", "swift", "rs", "js", "ts", "html",
                    "css", "py", "zip", "dmg", "app", "doc", "xls", "ppt", "mov",
                ];
                for ext in &exts {
                    let key = ext.to_string();
                    if icon_cache.lock().contains_key(&key) {
                        continue;
                    }
                    if let Some(icon) = load_system_icon_png(ext) {
                        icon_cache.lock().insert(key, icon);
                    }
                }
            });
        }
    });

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_drag::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            setup_app(app).map_err(|e| {
                Box::<dyn std::error::Error>::from(io::Error::new(io::ErrorKind::Other, e))
            })
        })
        .invoke_handler(tauri::generate_handler![
            get_index_status,
            get_home_dir,
            start_full_index,
            reset_index,
            search,
            fd_search,
            quick_look,
            open,
            open_with,
            reveal_in_finder,
            copy_paths,
            copy_files,
            move_to_trash,
            rename,
            get_file_icon,
            get_platform,
            show_context_menu,
            frontend_log
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn mk_entry(path: &str, name: &str) -> EntryDto {
        let parent = Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        EntryDto {
            path: path.to_string(),
            name: name.to_string(),
            dir: parent,
            is_dir: false,
            ext: None,
            size: None,
            mtime: None,
        }
    }

    fn temp_case_dir(case: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "everything_{case}_{}_{}",
            std::process::id(),
            stamp
        ))
    }

    fn test_state_for(db_path: PathBuf, home_dir: PathBuf, cwd: PathBuf) -> AppState {
        AppState {
            db_path,
            home_dir: home_dir.clone(),
            scan_root: home_dir.clone(),
            cwd,
            path_ignores: Arc::new(Vec::new()),
            path_ignore_patterns: Arc::new(Vec::new()),
            db_ready: Arc::new(AtomicBool::new(true)),
            indexing_active: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new(IndexStatus::default())),
            recent_ops: Arc::new(Mutex::new(Vec::new())),
            icon_cache: Arc::new(Mutex::new(HashMap::new())),
            fd_search_cache: Arc::new(Mutex::new(None)),
            negative_name_cache: Arc::new(Mutex::new(Vec::new())),
            ignore_cache: Arc::new(Mutex::new(None)),
            gitignore: Arc::new(gitignore_filter::LazyGitignoreFilter::new(home_dir.clone())),
            mem_index: Arc::new(RwLock::new(None)),
            watcher_stop: Arc::new(AtomicBool::new(false)),
            watcher_active: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn should_skip_path_for_ignored_descendant() {
        let ignored = vec![PathBuf::from(
            "/Users/al02402336/Library/Developer/Xcode/DerivedData",
        )];
        let target = Path::new(
            "/Users/al02402336/Library/Developer/Xcode/DerivedData/LINE-bqqx/Localization/Strings/-Users-al02402336-a_desktop",
        );

        assert!(should_skip_path(target, &ignored, &[]));
    }

    #[test]
    fn filter_ignored_entries_removes_pathignore_matches() {
        let ignored = vec![PathBuf::from(
            "/Users/al02402336/Library/Developer/Xcode/DerivedData",
        )];
        let entries = vec![
            mk_entry(
                "/Users/al02402336/Library/Developer/Xcode/DerivedData/LINE-bqqx/Localization/Strings/-Users-al02402336-a_desktop",
                "-Users-al02402336-a_desktop",
            ),
            mk_entry("/Users/al02402336/a_desktop", "a_desktop"),
        ];

        let filtered = filter_ignored_entries(entries, &ignored, &[]);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].path, "/Users/al02402336/a_desktop");
    }

    #[test]
    fn should_skip_path_for_glob_any_segment_target() {
        let patterns = vec![IgnorePattern::AnySegment("target".to_string())];

        assert!(should_skip_path(
            Path::new("/Users/al02402336/work/rust/target"),
            &[],
            &patterns
        ));
        assert!(should_skip_path(
            Path::new("/Users/al02402336/work/rust/target/debug/deps/foo.o"),
            &[],
            &patterns
        ));
        assert!(!should_skip_path(
            Path::new("/Users/al02402336/work/rust/targets/debug/deps/foo.o"),
            &[],
            &patterns
        ));
    }

    #[test]
    fn relevance_sort_prefers_shallow_exact_match() {
        let mut entries = vec![
            mk_entry("/Users/al02402336/work/a_desktop", "a_desktop"),
            mk_entry("/Users/al02402336/a_desktop", "a_desktop"),
            mk_entry(
                "/Users/al02402336/Library/Developer/Xcode/DerivedData/-Users-al02402336-a_desktop",
                "-Users-al02402336-a_desktop",
            ),
        ];

        sort_entries_with_relevance(&mut entries, "a_desktop", "name", "asc");

        assert_eq!(entries[0].path, "/Users/al02402336/a_desktop");
    }

    #[test]
    fn resolved_dir_range_excludes_sibling_with_same_prefix() {
        let dir_exact = "/Users/user/Projects";
        let dir_prefix = format!("{}/", dir_exact);
        let dir_prefix_end = format!("{}0", dir_exact);

        let in_scope = |dir: &str| {
            dir == dir_exact || (dir >= dir_prefix.as_str() && dir < dir_prefix_end.as_str())
        };

        assert!(in_scope("/Users/user/Projects"));
        assert!(in_scope("/Users/user/Projects/src"));
        assert!(in_scope("/Users/user/Projects/src/deeper"));

        assert!(!in_scope("/Users/user/Projects-archived"));
        assert!(!in_scope("/Users/user/Projects.archived"));
        assert!(!in_scope("/Users/user/Projects_archived"));
        assert!(!in_scope("/Users/user/Projectz"));
    }

    #[test]
    fn delete_paths_root_clears_all_entries() {
        let root = temp_case_dir("delete_paths_root");
        let dir_a = root.join("alpha");
        let dir_b = root.join("beta").join("nested");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let db_path = root.join("index.db");
        init_db(&db_path).unwrap();
        let mut conn = db_connection(&db_path).unwrap();
        let now = now_epoch();

        for (path, name, dir, ext) in [
            (dir_a.join("a.txt"), "a.txt", dir_a.clone(), "txt"),
            (dir_b.join("b.rs"), "b.rs", dir_b.clone(), "rs"),
        ] {
            conn.execute(
                "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
                 VALUES(?1, ?2, ?3, 0, ?4, NULL, NULL, ?5, 1)",
                params![
                    path.to_string_lossy().to_string(),
                    name,
                    dir.to_string_lossy().to_string(),
                    ext,
                    now
                ],
            )
            .unwrap();
        }

        let deleted = delete_paths(&mut conn, &["/".to_string()]).unwrap();
        assert!(deleted >= 2);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 0);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn execute_search_resolved_path_does_not_include_prefixed_sibling_dirs() {
        let root = temp_case_dir("resolved_path_scope_execute");
        let projects_src = root.join("Projects").join("src");
        let archived_src = root.join("Projects-archived").join("src");
        fs::create_dir_all(&projects_src).unwrap();
        fs::create_dir_all(&archived_src).unwrap();

        let db_path = root.join("index.db");
        init_db(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        let now = now_epoch();

        let project_path = projects_src.join("main.rs");
        let archived_path = archived_src.join("legacy.rs");

        conn.execute(
            "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
             VALUES(?1, ?2, ?3, 0, ?4, NULL, NULL, ?5, 1)",
            params![
                project_path.to_string_lossy().to_string(),
                "main.rs",
                projects_src.to_string_lossy().to_string(),
                "rs",
                now
            ],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
             VALUES(?1, ?2, ?3, 0, ?4, NULL, NULL, ?5, 1)",
            params![
                archived_path.to_string_lossy().to_string(),
                "legacy.rs",
                archived_src.to_string_lossy().to_string(),
                "rs",
                now
            ],
        )
        .unwrap();

        let state = test_state_for(db_path.clone(), root.clone(), root.clone());
        let result = execute_search(
            &state,
            "Projects/ *.rs".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();

        assert!(result
            .results
            .iter()
            .any(|entry| entry.path == project_path.to_string_lossy()));
        assert!(result
            .results
            .iter()
            .all(|entry| !entry.path.contains("Projects-archived")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_file_upward_locates_repo_level_pathignore() {
        let root = temp_case_dir("pathignore_upward");
        let nested = root.join("src-tauri").join("target");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(".pathignore"), "node_modules\n").unwrap();

        let found = find_file_upward(&nested, ".pathignore");
        assert_eq!(found, Some(root.join(".pathignore")));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn find_git_root_works_from_nested_directory() {
        let root = temp_case_dir("git_root");
        let nested = root.join("src-tauri").join("target");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();

        let found = find_git_root(&nested);
        assert_eq!(found, Some(root.clone()));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extract_ext_from_like_rejects_multi_dot() {
        assert_eq!(extract_ext_from_like("%.tar.gz"), None);
        assert_eq!(extract_ext_from_like("%.d.ts"), None);
        assert_eq!(extract_ext_from_like("%.rs"), Some("rs".to_string()));
        assert_eq!(extract_ext_from_like("%.png"), Some("png".to_string()));
    }

    #[test]
    fn negative_cache_fallback_removes_matched_key_not_query() {
        let state = test_state_for(
            PathBuf::from("/tmp/neg_cache_test.db"),
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
        );

        remember_negative_name_query(&state, "abc");

        let hit = negative_name_cache_lookup(&state, "xabc");
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().query_lower, "abc");

        remove_negative_name_query(&state, "abc");

        let hit_after = negative_name_cache_lookup(&state, "xabc");
        assert!(hit_after.is_none());
    }

    #[test]
    fn dir_listing_deep_nested_path_without_time_budget() {
        let root = temp_case_dir("dir_listing_deep");
        // Simulate: ~/Library/Containers/jp.naver.line.mac/Data/Library/Containers/jp.naver.line/log/
        let deep_dir = root
            .join("Library")
            .join("Containers")
            .join("jp.naver.line.mac")
            .join("Data")
            .join("Library")
            .join("Containers")
            .join("jp.naver.line")
            .join("log");
        let sub_dir = deep_dir.join("archive");
        fs::create_dir_all(&deep_dir).unwrap();
        fs::create_dir_all(&sub_dir).unwrap();

        let db_path = root.join("index.db");
        init_db(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        let now = now_epoch();

        let file1 = deep_dir.join("20260211_14.txt");
        let file2 = sub_dir.join("old.log");
        for (path, name, dir, ext) in [
            (&file1, "20260211_14.txt", &deep_dir, "txt"),
            (&file2, "old.log", &sub_dir, "log"),
        ] {
            conn.execute(
                "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
                 VALUES(?1, ?2, ?3, 0, ?4, NULL, NULL, ?5, 1)",
                params![
                    path.to_string_lossy().to_string(),
                    name,
                    dir.to_string_lossy().to_string(),
                    ext,
                    now
                ],
            )
            .unwrap();
        }

        let state = test_state_for(db_path, root.clone(), root.clone());

        // Query: "jp.naver.line/log/" — dir listing, name_like = "%"
        let result = execute_search(
            &state,
            "jp.naver.line/log/".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();

        assert_eq!(
            result.results.len(),
            2,
            "should find both files under log/ and log/archive/"
        );
        let names: Vec<&str> = result.results.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"20260211_14.txt"));
        assert!(names.contains(&"old.log"));

        // Query with glob: "jp.naver.line/log/ *"
        let result2 = execute_search(
            &state,
            "jp.naver.line/log/ *".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();

        assert_eq!(
            result2.results.len(),
            2,
            "glob * should also list all files"
        );

        // Query with trailing dot: "jp.naver.line/log/ *." — name_like becomes "%."
        let result3 = execute_search(
            &state,
            "jp.naver.line/log/ *.".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();

        // "*." matches files ending with "." — none of our test files end with "."
        // so this correctly returns 0 (this is expected, NOT a bug)
        assert_eq!(
            result3.results.len(),
            0,
            "*. matches only files ending with dot"
        );

        // But plain "/" at end should list all
        let result4 = execute_search(
            &state,
            "jp.naver.line/log/".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();

        assert_eq!(
            result4.results.len(),
            2,
            "trailing slash should list all files"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_search_multi_dot_ext_uses_like_not_ext_shortcut() {
        let root = temp_case_dir("multi_dot_ext");
        let proj_dir = root.join("Projects");
        fs::create_dir_all(&proj_dir).unwrap();

        let db_path = root.join("index.db");
        init_db(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        let now = now_epoch();

        let file_path = proj_dir.join("archive.tar.gz");
        conn.execute(
            "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
             VALUES(?1, ?2, ?3, 0, ?4, NULL, NULL, ?5, 1)",
            params![
                file_path.to_string_lossy().to_string(),
                "archive.tar.gz",
                proj_dir.to_string_lossy().to_string(),
                "gz",
                now
            ],
        )
        .unwrap();

        let state = test_state_for(db_path, root.clone(), root.clone());
        let result = execute_search(
            &state,
            "Projects/ *.tar.gz".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();

        assert_eq!(result.results.len(), 1);
        assert!(result.results[0].name == "archive.tar.gz");

        let _ = fs::remove_dir_all(root);
    }
}
