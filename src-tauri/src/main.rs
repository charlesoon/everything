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
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
mod fd_search;
#[cfg(target_os = "macos")]
mod mac;
mod mcp_server;
mod mem_search;
mod pathindexing;
mod query;
mod rescan;
#[cfg(target_os = "windows")]
mod win;
use fd_search::{FdSearchCache, FdSearchResultDto};
use query::{escape_like, parse_query, SearchMode};

const DEFAULT_LIMIT: u32 = 300;
const SHORT_QUERY_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;
pub(crate) const BATCH_SIZE: usize = 10_000;
/// In-flight batches between scan workers and the single DB writer. Workers
/// block (backpressure) instead of queueing unbounded row batches in memory
/// when SQLite falls behind the filesystem walk.
const SCAN_CHANNEL_CAP: usize = 8;
const RECENT_OP_TTL: Duration = Duration::from_secs(2);
pub(crate) const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);
const NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(60);
const NEGATIVE_CACHE_FALLBACK_WINDOW: Duration = Duration::from_millis(550);
const DB_VERSION: i32 = 7;
/// Index DB filename inside the app data dir. Shared with the MCP server's
/// fallback path derivation (`mcp_server::default_db_path`).
pub(crate) const DB_FILE_NAME: &str = "index.db";

/// Clamp + short-query cap shared by the app `search` command and the MCP
/// server, so the DB-protection limit policy can't drift between surfaces.
/// Only the `default` differs per surface.
pub(crate) fn effective_search_limit(query: &str, requested: Option<u32>, default: u32) -> u32 {
    let base = requested.unwrap_or(default).clamp(1, MAX_LIMIT);
    if !query.is_empty() && query.chars().count() <= 1 {
        base.min(SHORT_QUERY_LIMIT)
    } else {
        base
    }
}

/// Home directory resolution shared by app startup and the MCP server, so
/// path-mode queries resolve against the same root in both processes.
pub(crate) fn resolve_home_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| if cfg!(windows) { "C:\\".to_string() } else { "/".to_string() }),
    )
}

const CREATE_ENTRIES_TABLE_SQL: &str = "\
CREATE TABLE IF NOT EXISTS entries (
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    dir        TEXT NOT NULL,
    is_dir     INTEGER NOT NULL,
    ext        TEXT,
    mtime      INTEGER,
    size       INTEGER,
    indexed_at INTEGER NOT NULL,
    run_id     INTEGER NOT NULL DEFAULT 0
);";

const DROP_FTS_TRIGGERS_SQL: &str = "\
DROP TRIGGER IF EXISTS entries_ai;
DROP TRIGGER IF EXISTS entries_ad;
DROP TRIGGER IF EXISTS entries_au;";

const CREATE_FTS_TRIGGERS_SQL: &str = "\
CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(rowid, name) VALUES (new.id, new.name);
END;
CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, name) VALUES ('delete', old.id, old.name);
END;
CREATE TRIGGER IF NOT EXISTS entries_au AFTER UPDATE OF name ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, name) VALUES ('delete', old.id, old.name);
    INSERT INTO entries_fts(rowid, name) VALUES (new.id, new.name);
END;";

const REBUILD_FTS_SQL: &str = "INSERT INTO entries_fts(entries_fts) VALUES('rebuild');";

/// Secondary indexes on `entries`. Single source of truth shared by
/// `ensure_db_indexes` (startup/catchup) and `finalize_fresh_index` (which
/// builds them before ANALYZE so the planner gets stats for all of them).
const CREATE_ENTRIES_INDEXES_SQL: &str = "\
CREATE INDEX IF NOT EXISTS idx_entries_dir_ext_name_nocase ON entries(dir, ext, name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_entries_indexed_at ON entries(indexed_at);";
const DEFERRED_DIR_NAMES: &[&str] = &[
    "Library", ".Trash", ".Trashes",
    // Windows system directories (deferred when scan_root is C:\)
    "Windows", "Program Files", "Program Files (x86)",
    "$Recycle.Bin", "System Volume Information", "Recovery", "PerfLogs",
];


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

// Pre-computed infix/suffix strings for BUILTIN_SKIP_PATHS to avoid hot-path allocations.
static BUILTIN_SKIP_PATH_CHECKS: OnceLock<Vec<(String, String)>> = OnceLock::new();
fn builtin_skip_path_checks() -> &'static [(String, String)] {
    BUILTIN_SKIP_PATH_CHECKS.get_or_init(|| {
        BUILTIN_SKIP_PATHS
            .iter()
            .map(|pat| (format!("/{pat}/"), format!("/{pat}")))
            .collect()
    })
}

static SEARCH_LOG_ENABLED: OnceLock<bool> = OnceLock::new();
static PERF_LOG_ENABLED: OnceLock<bool> = OnceLock::new();
static BENCH_MODE_ENABLED: OnceLock<bool> = OnceLock::new();
static STARTUP_T0: OnceLock<Instant> = OnceLock::new();

fn startup_elapsed_ms() -> u128 {
    STARTUP_T0
        .get()
        .map(|started| started.elapsed().as_millis())
        .unwrap_or(0)
}

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
    background_active: bool,
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
    total_count: u32,
    /// True when total_count is exact. False means frontend should treat it as unknown.
    total_known: bool,
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
    /// Matches any path segment equal to `segment`.
    /// `suffix` = `"/{segment}"`, `infix` = `"/{segment}/"` — pre-computed to avoid
    /// per-call format! allocations in the hot path.
    AnySegment {
        segment: String,
        suffix: String,
        infix: String,
    },
    Glob(String),
}

impl IgnorePattern {
    pub(crate) fn any_segment(segment: &str) -> Self {
        IgnorePattern::AnySegment {
            suffix: format!("/{segment}"),
            infix: format!("/{segment}/"),
            segment: segment.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct IgnoreRulesCache {
    roots: Vec<PathBuf>,
    patterns: Vec<IgnorePattern>,
    pathignore_mtime: Option<SystemTime>,
    config_file_mtime: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) db_path: PathBuf,
    pub(crate) home_dir: PathBuf,
    pub(crate) scan_root: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) config_file_path: PathBuf,
    pub(crate) pathindexing_file_path: PathBuf,
    pub(crate) extra_roots: Arc<Mutex<Vec<PathBuf>>>,
    pub(crate) path_ignores: Arc<Vec<PathBuf>>,
    pub(crate) path_ignore_patterns: Arc<Vec<IgnorePattern>>,
    pub(crate) db_ready: Arc<AtomicBool>,
    pub(crate) indexing_active: Arc<AtomicBool>,
    pub(crate) status: Arc<Mutex<IndexStatus>>,
    pub(crate) recent_ops: Arc<Mutex<Vec<RecentOp>>>,
    pub(crate) icon_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    pub(crate) fd_search_cache: Arc<Mutex<Option<FdSearchCache>>>,
    pub(crate) negative_name_cache: Arc<Mutex<HashMap<String, NegativeNameEntry>>>,
    pub(crate) ignore_cache: Arc<Mutex<Option<IgnoreRulesCache>>>,
    /// FTS index is in sync with entries table. Set to false during fresh index
    /// (triggers dropped for bulk insert), set to true after FTS rebuild completes.
    /// When false, search falls back to LIKE-based queries instead of FTS.
    pub(crate) fts_ready: Arc<AtomicBool>,
    /// In-memory index for instant search while DB upsert runs in background.
    /// Set by MFT scan, cleared after background DB upsert completes.
    pub(crate) mem_index: Arc<RwLock<Option<Arc<mem_search::MemIndex>>>>,
    /// Signal to stop the file watcher (RDCW / USN). Set to true on reset_index.
    pub(crate) watcher_stop: Arc<AtomicBool>,
    /// Set to true while a file watcher event loop is running.
    pub(crate) watcher_active: Arc<AtomicBool>,
    /// Set to true once frontend onMount has completed enough to accept user input.
    pub(crate) frontend_ready: Arc<AtomicBool>,
    /// Guards against concurrent pathindexing background threads.
    pub(crate) pathindexing_active: Arc<AtomicBool>,
    /// Warm read connections reused across searches. Opening a connection per
    /// search costs ~1-2ms (open + PRAGMAs + schema parse) and starts with a
    /// cold SQLite page cache; reuse keeps hot pages and prepared statements.
    pub(crate) search_conn_pool: Arc<Mutex<Vec<Connection>>>,
    /// Persistent write connection for watcher-driven incremental updates.
    /// Opening a connection per event batch dominated single-file update cost.
    pub(crate) watcher_conn: Arc<Mutex<Option<Connection>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct NegativeNameEntry {
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

/// Unique per-test scratch directory (not created). Shared by the main.rs and
/// mcp_server.rs test modules; on Windows it stays under the cwd because the
/// system temp dir has proven flaky for DB files there.
#[cfg(test)]
pub(crate) fn temp_case_dir(case: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = if cfg!(target_os = "windows") {
        std::env::current_dir()
            .map(|dir| dir.join("tmp-test-dirs"))
            .unwrap_or_else(|_| std::env::temp_dir())
    } else {
        std::env::temp_dir()
    };
    base.join(format!(
        "everything_{case}_{}_{}",
        std::process::id(),
        stamp
    ))
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

/// Connection for maintenance passes (index builds, FTS rebuild, ANALYZE,
/// VACUUM): their sorts and rebuilds spill through SQLite's temp store, and
/// the default `temp_store=MEMORY` would pull hundreds of MB of spill into
/// the heap on a large DB — this connection sends it to disk instead.
fn db_connection_for_maintenance(db_path: &Path) -> AppResult<Connection> {
    let conn = db_connection(db_path)?;
    conn.execute_batch("PRAGMA temp_store = FILE;")
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

fn db_connection_for_search(db_path: &Path) -> AppResult<Connection> {
    let conn = db_connection_with_timeout(db_path, 500)?;
    // mmap_size must cover the whole DB file (entries + FTS index) so reads hit
    // the shared OS page cache instead of per-connection pread into a cold cache.
    conn.execute_batch(
        "PRAGMA cache_size = -32768;
         PRAGMA mmap_size = 1073741824;",
    )
    .map_err(|e| e.to_string())?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(conn)
}

const SEARCH_CONN_POOL_MAX: usize = 3;

/// A search connection borrowed from `AppState::search_conn_pool`; returned to
/// the pool on drop. Derefs to `rusqlite::Connection`.
struct PooledSearchConn {
    conn: Option<Connection>,
    pool: Arc<Mutex<Vec<Connection>>>,
}

impl std::ops::Deref for PooledSearchConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("pooled connection present until drop")
    }
}

impl Drop for PooledSearchConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let mut pool = self.pool.lock();
            if pool.len() < SEARCH_CONN_POOL_MAX {
                pool.push(conn);
            }
        }
    }
}

fn pooled_search_connection(state: &AppState) -> AppResult<PooledSearchConn> {
    let reused = state.search_conn_pool.lock().pop();
    let conn = match reused {
        Some(conn) => {
            // A time-budget progress handler must never leak across borrows: a
            // stale handler would interrupt every future query on this conn.
            conn.progress_handler(0, None::<fn() -> bool>);
            conn
        }
        None => db_connection_for_search(&state.db_path)?,
    };
    Ok(PooledSearchConn {
        conn: Some(conn),
        pool: state.search_conn_pool.clone(),
    })
}

pub(crate) fn set_indexing_pragmas(conn: &Connection) -> AppResult<()> {
    conn.execute_batch(
        r#"
        PRAGMA synchronous = OFF;
        PRAGMA cache_size = -131072;
        PRAGMA mmap_size = 536870912;
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
        PRAGMA mmap_size = 268435456;
        PRAGMA wal_autocheckpoint = 1000;
        "#,
    )
    .map_err(|e| e.to_string())?;
    // PASSIVE checkpoint: does not block readers/writers; checkpoints as many
    // WAL frames as possible without waiting. The WAL will be fully checkpointed
    // when the next auto-checkpoint threshold is hit.
    conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Phase 1: creates tables only. Fast even on large DBs.
fn init_db_tables(db_path: &Path) -> AppResult<()> {
    let t = Instant::now();
    let conn = db_connection(db_path)?;
    eprintln!("[init_db] +{}ms db_connection opened", t.elapsed().as_millis());

    let current_version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);

    if current_version != DB_VERSION {
        // Fast schema migration: rename entries → entries_gc_{old_version} (instant),
        // create new empty entries, rebuild FTS from empty content table (instant),
        // reset relevant meta keys. Background thread drops the renamed old table.
        // This avoids the 20-30s synchronous DROP TABLE entries_fts on large DBs.
        let old_table = format!("entries_gc_{}", current_version);

        let entries_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entries'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;

        if entries_exists {
            // Drop triggers before rename: SQLite keeps trigger names when a table is renamed,
            // so CREATE TRIGGER with the same names would fail after rename.
            let _ = conn.execute_batch(DROP_FTS_TRIGGERS_SQL);

            // Also handle re-entrant case: if entries_gc_{old_version} already exists
            // (e.g., previous run crashed after rename but before user_version update),
            // drop it first so the rename can succeed.
            let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {old_table};"));

            // Rename entries (fast, no data movement)
            conn.execute_batch(&format!("ALTER TABLE entries RENAME TO {old_table};"))
                .map_err(|e| e.to_string())?;

            // Create new empty entries (FTS triggers + rebuild deferred to after unconditional block below)
            conn.execute_batch(CREATE_ENTRIES_TABLE_SQL).map_err(|e| e.to_string())?;

            // Reset meta keys so startup triggers a full index scan
            let meta_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if meta_exists {
                let _ = conn.execute_batch(
                    "DELETE FROM meta WHERE key IN \
                     ('index_complete','last_run_id','last_event_id');",
                );
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO meta(key,value) VALUES('entries_pending_drop',?1)",
                    params![old_table],
                );
            }

            // Old table will be dropped by the GC cleanup in the finalizing
            // thread after indexing completes — avoids WAL lock contention with
            // the indexer that starts shortly after init_db returns.
            eprintln!(
                "[init_db] +{}ms schema migrated v={} -> v={}: {} deferred for gc",
                t.elapsed().as_millis(),
                current_version,
                DB_VERSION,
                old_table
            );
        }

        conn.execute_batch(&format!("PRAGMA user_version = {};", DB_VERSION))
            .map_err(|e| e.to_string())?;
    }
    eprintln!("[init_db] +{}ms version check done (v={})", t.elapsed().as_millis(), current_version);

    conn.execute_batch(CREATE_ENTRIES_TABLE_SQL).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
           name,
           content='entries',
           content_rowid='id',
           tokenize='trigram'
         );",
    )
    .map_err(|e| e.to_string())?;
    conn.execute_batch(CREATE_FTS_TRIGGERS_SQL).map_err(|e| e.to_string())?;
    eprintln!("[init_db] +{}ms tables ensured", t.elapsed().as_millis());

    Ok(())
}

/// Phase 2: creates indexes and runs legacy migrations. May be slow on large DBs
/// when new indexes are added; runs after db_ready so it does not block startup.
fn ensure_db_indexes(db_path: &Path) -> AppResult<()> {
    let t = Instant::now();
    let conn = db_connection_for_maintenance(db_path)?;

    conn.execute_batch(CREATE_ENTRIES_INDEXES_SQL)
        .map_err(|e| e.to_string())?;
    eprintln!("[init_db] +{}ms indexes ensured", t.elapsed().as_millis());

    let legacy_key = "migration_drop_idx_entries_dir_name_nocase_v1";
    let migrated = get_meta(&conn, legacy_key)
        .map(|v| v == "1")
        .unwrap_or(false);
    if !migrated {
        conn.execute_batch("DROP INDEX IF EXISTS idx_entries_dir_name_nocase;")
            .map_err(|e| e.to_string())?;
        set_meta(&conn, legacy_key, "1")?;
    }
    eprintln!("[init_db] +{}ms total (indexes + migration)", t.elapsed().as_millis());

    Ok(())
}

pub(crate) fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")
        .ok()?
        .query_row(params![key], |row| row.get(0))
        .ok()
}

/// Whether the FTS trigram index is trustworthy: the shadow table exists and
/// no crashed/in-flight rebuild has flagged it dirty. Single source of the
/// gating rule, shared by the startup thread and the MCP server (which
/// re-derives readiness from the DB because it runs in a separate process).
pub(crate) fn fts_usable(conn: &Connection) -> bool {
    let has_fts: bool = conn
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='entries_fts')")
        .and_then(|mut stmt| stmt.query_row([], |r| r.get(0)))
        .unwrap_or(false);
    has_fts && get_meta(conn, "fts_dirty").map_or(true, |v| v != "1")
}

pub(crate) fn set_meta(conn: &Connection, key: &str, value: &str) -> AppResult<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
        params![key, value],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Persist the status-bar counts so the next startup can seed them without a
/// COUNT(*)/MAX() scan over `entries`. Best-effort: write errors are ignored.
pub(crate) fn persist_cached_counts(conn: &Connection, entries_count: u64, last_updated: Option<i64>) {
    let _ = set_meta(conn, "cached_entries_count", &entries_count.to_string());
    if let Some(lu) = last_updated {
        let _ = set_meta(conn, "cached_last_updated", &lu.to_string());
    }
}

/// Read the status-bar counts persisted by [`persist_cached_counts`]. Returns
/// `(None, _)` when no count has been cached yet (e.g. a never-indexed DB).
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn load_cached_counts(conn: &Connection) -> (Option<u64>, Option<i64>) {
    let count = get_meta(conn, "cached_entries_count").and_then(|v| v.parse::<u64>().ok());
    let last_updated = get_meta(conn, "cached_last_updated").and_then(|v| v.parse::<i64>().ok());
    (count, last_updated)
}

pub(crate) fn cleanup_entries_gc_tables(conn: &Connection) -> AppResult<()> {
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'entries_gc_%'",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut tables = Vec::new();
        for row in rows {
            tables.push(row.map_err(|e| e.to_string())?);
        }
        tables
    };

    for table in tables {
        if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            eprintln!("[gc] skipping invalid table name: {table}");
            continue;
        }
        eprintln!("[gc] dropping orphaned table {table}");
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"));
        let _ = conn.execute(
            "DELETE FROM meta WHERE key='entries_pending_drop' AND value=?1",
            rusqlite::params![table],
        );
    }

    Ok(())
}

fn update_counts(conn: &Connection) -> AppResult<(u64, Option<i64>)> {
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

pub(crate) fn invalidate_search_caches(state: &AppState) {
    // Note: search_conn_pool is intentionally NOT cleared here — pooled
    // connections stay valid across data changes (this runs on every watcher
    // batch), and dropping them would re-cold-start the page cache.
    state.fd_search_cache.lock().take();
    state.negative_name_cache.lock().clear();
}

fn prune_negative_name_cache(cache: &mut HashMap<String, NegativeNameEntry>) {
    let now = Instant::now();
    cache.retain(|_, entry| now.duration_since(entry.created_at) <= NEGATIVE_CACHE_TTL);
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
        .filter(|(key, _)| q.contains(key.as_str()))
        .max_by_key(|(key, _)| key.len())
        .map(|(key, entry)| NegativeCacheHit {
            query_lower: key.clone(),
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

    if cache.contains_key(&normalized) {
        return;
    }

    cache.insert(
        normalized,
        NegativeNameEntry {
            created_at: Instant::now(),
            fallback_checked: false,
        },
    );
    const MAX_NEGATIVE_CACHE: usize = 512;
    if cache.len() > MAX_NEGATIVE_CACHE {
        let mut entries: Vec<_> =
            cache.iter().map(|(k, v)| (k.clone(), v.created_at)).collect();
        entries.sort_by_key(|(_, t)| *t);
        let drop_count = cache.len() - MAX_NEGATIVE_CACHE;
        for (key, _) in entries.into_iter().take(drop_count) {
            cache.remove(&key);
        }
    }
}

fn remove_negative_name_query(state: &AppState, query: &str) {
    if query.is_empty() {
        return;
    }
    let normalized = query.to_lowercase();
    state.negative_name_cache.lock().remove(&normalized);
}

fn mark_negative_name_fallback_checked(state: &AppState, query_lower: &str) {
    let mut cache = state.negative_name_cache.lock();
    if let Some(entry) = cache.get_mut(query_lower) {
        entry.fallback_checked = true;
    }
}

/// Sort keys/dirs `sort_clause` dispatches on (its `_` arm falls back to
/// name-asc). The MCP tool schema and argument validation reference these so
/// the advertised vocabulary can't drift from the SQL dispatch below.
pub(crate) const SORT_KEYS: &[&str] = &["name", "mtime", "size", "dir"];
pub(crate) const SORT_DIRS: &[&str] = &["asc", "desc"];

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

/// Quote a plain query string as an FTS5 phrase (`"..."`), so trigram MATCH
/// performs a substring lookup with no query-syntax interpretation.
fn fts_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// Build an FTS5 trigram prefilter for a glob pattern: every literal run of
/// 3+ chars must be present as a substring (`"lit1" AND "lit2"`). Returns None
/// when no run is long enough to form a trigram (prefilter would not narrow).
fn glob_fts_match_expr(glob: &str) -> Option<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in glob.chars() {
        if ch == '*' || ch == '?' {
            if !cur.is_empty() {
                runs.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    let exprs: Vec<String> = runs
        .iter()
        .filter(|r| r.chars().count() >= 3)
        .map(|r| fts_phrase(r))
        .collect();
    if exprs.is_empty() {
        None
    } else {
        Some(exprs.join(" AND "))
    }
}

/// A LIKE pattern starting with a wildcard cannot use the name index range
/// scan; those are the glob shapes worth routing through the FTS prefilter.
fn like_pattern_unindexable(name_like: &str) -> bool {
    name_like.starts_with('%') || name_like.starts_with('_')
}

/// The FTS trigram prefilter for a `GlobName` query: `Some(match_expr)` when the
/// pattern's LIKE form can't use the name index (leading wildcard) and has a
/// 3+ char literal run to narrow on; `None` means evaluate the plain LIKE.
/// `execute_search` and `compute_total_count` share this so the counted set and
/// the returned page always route the same way.
fn glob_fts_prefilter(fts_ready: bool, name_like: &str, query: &str) -> Option<String> {
    if fts_ready && like_pattern_unindexable(name_like) {
        glob_fts_match_expr(query)
    } else {
        None
    }
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

fn normalize_hint_to_native(dir_hint: &str) -> (String, bool) {
    let native = dir_hint.replace('/', &std::path::MAIN_SEPARATOR.to_string());
    let is_abs = Path::new(&native).is_absolute();
    (native, is_abs)
}

fn last_segment(dir_hint: &str) -> &str {
    dir_hint
        .rsplit(|c| c == '/' || c == '\\')
        .find(|seg| !seg.is_empty())
        .unwrap_or(dir_hint)
}

fn resolve_dir_hint(home_dir: &Path, dir_hint: &str) -> Option<PathBuf> {
    if dir_hint.is_empty() || contains_glob_meta(dir_hint) {
        return None;
    }

    let candidate = if dir_hint == "~" {
        home_dir.to_path_buf()
    } else if let Some(rest) = dir_hint.strip_prefix("~/") {
        home_dir.join(rest)
    } else {
        let (native_hint, is_absolute) = normalize_hint_to_native(dir_hint);
        if is_absolute {
            PathBuf::from(native_hint)
        } else {
            home_dir.join(native_hint)
        }
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
    let last_seg = last_segment(dir_hint);
    let sep = std::path::MAIN_SEPARATOR;
    let (native_hint, is_absolute) = normalize_hint_to_native(dir_hint);
    let sep_str = sep.to_string();
    let escaped_sep = escape_like(&sep_str);
    let path_pattern = if is_absolute {
        format!("%{}", escape_like(&native_hint))
    } else {
        format!("%{escaped_sep}{}", escape_like(&native_hint))
    };
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
    if s.contains('\\') { s.replace('\\', "/") } else { s }
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
            return Some(IgnorePattern::any_segment(segment));
        }
    }

    Some(IgnorePattern::Glob(normalize_ignore_pattern(
        trimmed, base_dir, home_dir,
    )))
}

fn ignore_pattern_key(pattern: &IgnorePattern) -> String {
    match pattern {
        IgnorePattern::AnySegment { segment, .. } => format!("seg:{segment}"),
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

#[cfg(test)]
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

const DEFAULT_PATHIGNORE_CONTENTS: &str = "\
# Everything - index exclusion rules
# **/name  : exclude directories with this name anywhere
# /abs/path : exclude a specific absolute path

# Build artifacts
**/dist
**/build
**/out
**/.next
**/.nuxt
**/.svelte-kit
**/coverage
**/.parcel-cache
**/.turbo

# Home directory exclusions
~/.cursor/
~/.gemini
~/.cargo
";

/// Extracts the effective rule lines from a .pathignore file's contents:
/// trims whitespace, drops empty lines, comments (#), and negation lines (!).
/// Used to detect meaningful changes while ignoring formatting edits.
pub(crate) fn pathignore_active_entries(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .map(|l| l.to_string())
        .collect()
}

fn ensure_pathignore_exists(path: &Path) -> AppResult<()> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, DEFAULT_PATHIGNORE_CONTENTS).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn load_pathignore_rules(config_file: &Path, home_dir: &Path, cwd: &Path) -> (Vec<PathBuf>, Vec<IgnorePattern>) {
    let _ = ensure_pathignore_exists(config_file);

    let mut files = Vec::new();
    // App-data config file takes priority.
    files.push(config_file.to_path_buf());

    // Legacy: cwd/.pathignore or ~/ .pathignore for backward compatibility.
    if let Some(file) = find_file_upward(cwd, ".pathignore") {
        if file != config_file {
            files.push(file);
        }
    } else {
        let cwd_file = cwd.join(".pathignore");
        if cwd_file != config_file {
            files.push(cwd_file);
        }
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
        // C:\ system directories (Windows is deferred, not excluded)
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

pub(crate) fn effective_ignore_rules(
    config_file: &Path,
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

    let (pathignore_roots, pathignore_patterns) = load_pathignore_rules(config_file, home_dir, cwd);
    for root in pathignore_roots
        .into_iter()
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
    let config_file = &state.config_file_path;

    // Check mtime of config file (app_data_dir/.pathignore) first, then legacy locations.
    let config_file_mtime = fs::metadata(config_file)
        .ok()
        .and_then(|m| m.modified().ok());

    let pathignore_mtime = find_file_upward(cwd, ".pathignore")
        .filter(|p| p != config_file)
        .or_else(|| Some(home_dir.join(".pathignore")))
        .and_then(|p| fs::metadata(&p).ok())
        .and_then(|m| m.modified().ok());

    let mut cache = state.ignore_cache.lock();
    if let Some(ref cached) = *cache {
        if cached.config_file_mtime == config_file_mtime && cached.pathignore_mtime == pathignore_mtime
        {
            return (cached.roots.clone(), cached.patterns.clone());
        }
    }

    let (roots, patterns) = effective_ignore_rules(
        config_file,
        home_dir,
        cwd,
        state.path_ignores.as_ref(),
        state.path_ignore_patterns.as_ref(),
    );

    *cache = Some(IgnoreRulesCache {
        roots: roots.clone(),
        patterns: patterns.clone(),
        pathignore_mtime,
        config_file_mtime,
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
        let (lo, hi) = subtree_range_bounds(prefix);
        conn.execute(
            "DELETE FROM entries WHERE path = ?1 OR (path >= ?2 AND path < ?3)",
            params![prefix, lo, hi],
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
        IgnorePattern::AnySegment { suffix, infix, .. } => {
            path.ends_with(suffix.as_str()) || path.contains(infix.as_str())
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
    let s = normalize_slashes(path.to_string_lossy().to_string());

    // Single-component builtin names: check path segments
    if s.split('/').any(|seg| {
        BUILTIN_SKIP_NAMES.contains(&seg)
            || BUILTIN_SKIP_SUFFIXES.iter().any(|suf| seg.ends_with(suf))
    }) {
        return true;
    }

    // Multi-component builtin paths
    if builtin_skip_path_checks()
        .iter()
        .any(|(infix, suffix)| s.contains(infix.as_str()) || s.ends_with(suffix.as_str()))
    {
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

pub(crate) fn index_row_from_walkdir_entry(entry: &walkdir::DirEntry) -> Option<IndexRow> {
    let metadata = entry.metadata().ok()?;
    index_row_from_path_and_metadata(entry.path(), &metadata)
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

fn relevance_rank(entry: &EntryDto, query_lower: &str, path_suffix: &str) -> u8 {
    if query_lower.is_empty() {
        return 255;
    }

    let name = entry.name.to_lowercase();

    if name == query_lower {
        return 0;
    }
    if name.starts_with(query_lower) {
        return 1;
    }
    if name.contains(query_lower) {
        return 2;
    }

    // Path lowercasing is deferred: most entries rank 0-2 on name alone.
    let path = entry.path.to_lowercase();
    if path.ends_with(path_suffix) {
        return 3;
    }
    if path.contains(query_lower) {
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
    let query_lower = query.trim().to_lowercase();
    if query_lower.is_empty() {
        sort_entries(entries, sort_by, sort_dir);
        return;
    }
    let path_suffix = format!("/{query_lower}");

    // Rank every entry once (decorate–sort–undecorate): relevance_rank
    // lowercases name/path, far too expensive to recompute per comparison.
    let mut decorated: Vec<(u8, usize, EntryDto)> = entries
        .drain(..)
        .map(|entry| {
            let rank = relevance_rank(&entry, &query_lower, &path_suffix);
            // For highly-relevant matches, prefer shallower paths first
            // so `~/name` ranks above deep descendants with the same name.
            let depth = if rank <= 3 { path_depth(&entry.path) } else { 0 };
            (rank, depth, entry)
        })
        .collect();
    decorated.sort_by(|a, b| {
        if a.0 != b.0 {
            return a.0.cmp(&b.0);
        }
        if a.0 <= 3 && a.1 != b.1 {
            return a.1.cmp(&b.1);
        }
        entry_cmp(&a.2, &b.2, sort_by, sort_dir)
    });
    entries.extend(decorated.into_iter().map(|(_, _, entry)| entry));
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

/// Bulk insert for the fresh-index path: every path is new, so the upsert
/// arm is unnecessary. OR IGNORE keeps overlapping extra roots harmless.
/// Callers on the hot path pre-sort batches by path in the scan workers so
/// the UNIQUE(path) b-tree sees near-sequential inserts.
/// Run one transaction binding every row of `rows` to `sql` — a 9-placeholder
/// INSERT variant (`path, name, dir, is_dir, ext, mtime, size, indexed_at,
/// run_id`). Shared by `insert_rows_fresh` and `upsert_rows`, which differ only
/// in the INSERT conflict clause.
fn write_rows(conn: &mut Connection, rows: &[IndexRow], sql: &str) -> AppResult<usize> {
    if rows.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction().map_err(|e| e.to_string())?;
    {
        let mut stmt = tx.prepare(sql).map_err(|e| e.to_string())?;
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

pub(crate) fn insert_rows_fresh(conn: &mut Connection, rows: &[IndexRow]) -> AppResult<usize> {
    write_rows(
        conn,
        rows,
        r#"
        INSERT OR IGNORE INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
        VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        "#,
    )
}

pub(crate) fn upsert_rows(conn: &mut Connection, rows: &[IndexRow]) -> AppResult<usize> {
    write_rows(
        conn,
        rows,
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
}

/// B-tree range bounds selecting every row strictly under `path`:
/// `path >= "{path}{sep}" AND path < "{path}{sep+1}"`. The exclusive upper
/// bound is the separator's successor char, so only true descendants match —
/// a \x7F bound would also capture prefix siblings ("proj0" would take
/// "proj00" down with it). Use this instead of LIKE+ESCAPE: SQLite disables
/// the index range-scan optimization when an ESCAPE clause is present,
/// causing a full table scan even though `path` has a UNIQUE index.
pub(crate) fn subtree_range_bounds(path: &str) -> (String, String) {
    let sep = std::path::MAIN_SEPARATOR;
    (
        format!("{path}{sep}"),
        format!("{path}{}", (sep as u8 + 1) as char),
    )
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

            let (range_start, range_end) = subtree_range_bounds(&normalized);
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

#[allow(dead_code)]
pub(crate) fn update_status_counts(state: &AppState) -> AppResult<(u64, Option<i64>)> {
    let conn = db_connection(&state.db_path)?;
    let (entries_count, last_updated) = update_counts(&conn)?;
    {
        let mut status = state.status.lock();
        status.entries_count = entries_count;
        status.last_updated = last_updated;
    }
    Ok((entries_count, last_updated))
}

pub(crate) fn refresh_and_emit_status_counts(app: &AppHandle, state: &AppState) -> AppResult<()> {
    let conn = db_connection(&state.db_path)?;
    let (entries_count, last_updated) = update_counts(&conn)?;
    {
        let mut status = state.status.lock();
        status.entries_count = entries_count;
        status.last_updated = last_updated;
    }
    // Persist cached counts so next startup can read them instantly
    persist_cached_counts(&conn, entries_count, last_updated);
    emit_status_counts(app, state);
    Ok(())
}

/// Emit status-bar counts from the in-memory `IndexStatus` (maintained
/// incrementally by the watcher) and persist them for instant startup reads.
/// Unlike `refresh_and_emit_status_counts` this never queries `entries`, so
/// it is safe on every watcher batch: the COUNT(*)/MAX() recount there was a
/// periodic whole-table scan — a visible CPU spike every ~2s on large DBs.
#[cfg(target_os = "macos")]
fn emit_and_persist_cached_counts(app: &AppHandle, state: &AppState) {
    let (entries_count, last_updated) = {
        let status = state.status.lock();
        (status.entries_count, status.last_updated)
    };
    // Best-effort persist over the shared watcher connection; the next full
    // recount rewrites the cache if the connection isn't open.
    if let Some(conn) = state.watcher_conn.lock().as_ref() {
        persist_cached_counts(conn, entries_count, last_updated);
    }
    emit_status_counts(app, state);
}

/// Load cached entries count from meta table (instant) and emit Ready state + counts.
/// Used on Windows startup paths where the index is already complete from a prior run.
#[cfg(target_os = "windows")]
pub(crate) fn set_ready_with_cached_counts(app: &AppHandle, state: &AppState) {
    let (count, last_updated) = db_connection(&state.db_path)
        .ok()
        .map(|conn| {
            let (c, lu) = load_cached_counts(&conn);
            (c.unwrap_or(0), lu)
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

#[cfg(not(target_os = "windows"))]
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

    // Mark index as incomplete -- cleared when run_incremental_index succeeds
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
        let result = run_incremental_index(Some(&app), &state);
        if let Err(ref err) = result {
            eprintln!("[index] run_incremental_index failed: {err}");
            if !silent {
                set_state(&state, IndexState::Error, Some(err.clone()));
                emit_index_state(&app, "Error", Some(err.clone()));
            }
            // Silent mode: keep Ready state, just log the error
            // On error: release indexing_active here (finalizing thread was not spawned)
            state.indexing_active.store(false, AtomicOrdering::Release);
        }
        // On success: indexing_active is released by the finalizing bg thread
        // (spawned inside run_incremental_index) after Ready is emitted.
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

/// Deferred fresh-index finalization: secondary indexes, FTS rebuild (+
/// triggers), and ANALYZE. Runs on the finalizing thread after Ready so the
/// user-visible indexing phase does not wait for DDL. Search stays correct
/// meanwhile: prefix queries fall back when INDEXED BY fails, and fts_ready
/// gates every FTS path. The watcher is still paused (indexing_active), so
/// no upsert can slip past the not-yet-recreated FTS triggers.
fn finalize_fresh_index(state: &AppState) {
    let Ok(conn) = db_connection_for_maintenance(&state.db_path) else {
        return;
    };
    let t_idx = Instant::now();
    let _ = conn.execute_batch(CREATE_ENTRIES_INDEXES_SQL);
    eprintln!("[timing] create_indexes {}ms", t_idx.elapsed().as_millis());

    let fts_t = Instant::now();
    let _ = conn.execute_batch(CREATE_FTS_TRIGGERS_SQL);
    let _ = conn.execute_batch(REBUILD_FTS_SQL);
    state.fts_ready.store(true, AtomicOrdering::Release);
    let _ = set_meta(&conn, "fts_dirty", "0");
    eprintln!("[index] fts_rebuild {}ms", fts_t.elapsed().as_millis());

    let t_analyze = Instant::now();
    let _ = conn.execute_batch("ANALYZE");
    eprintln!("[timing] analyze {}ms", t_analyze.elapsed().as_millis());
}

/// Free pages must exceed this many bytes (and 25% of the DB) before the
/// maintenance VACUUM kicks in — it rewrites the whole file, so it should only
/// run when mass rewrites have left real garbage behind.
const VACUUM_MIN_FREE_BYTES: i64 = 100 * 1024 * 1024;

/// Post-indexing storage maintenance, run while the watcher is still paused:
/// reclaim accumulated free pages (threshold-gated VACUUM), truncate the WAL,
/// and release this connection's page cache.
fn run_db_maintenance(state: &AppState) {
    let Ok(conn) = db_connection_for_maintenance(&state.db_path) else {
        return;
    };

    let pragma_i64 = |name: &str| -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .unwrap_or(0)
    };
    let free_pages = pragma_i64("freelist_count");
    let total_pages = pragma_i64("page_count");
    let free_bytes = free_pages.saturating_mul(pragma_i64("page_size"));
    if total_pages > 0 && free_pages * 4 >= total_pages && free_bytes >= VACUUM_MIN_FREE_BYTES {
        let t = Instant::now();
        match conn.execute_batch("VACUUM") {
            Ok(()) => eprintln!(
                "[maintenance] vacuum reclaimed {}MB in {}ms",
                free_bytes / (1024 * 1024),
                t.elapsed().as_millis()
            ),
            Err(e) => eprintln!("[maintenance] vacuum failed: {e}"),
        }
    }
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
}

/// Ask macOS malloc to hand freed-but-retained regions back to the OS. Without
/// this a large indexing pass leaves hundreds of MB of empty malloc regions
/// resident until system memory pressure reclaims them.
fn release_memory_to_os() {
    #[cfg(target_os = "macos")]
    unsafe {
        extern "C" {
            fn malloc_zone_pressure_relief(
                zone: *mut std::os::raw::c_void,
                goal: usize,
            ) -> usize;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// Remove `row`'s path from the direct-children preload map, returning true
/// when it was present AND unchanged (same mtime + size). Paths left in the
/// map after the scan are the vanished ones to delete. (Whole-subtree diffs
/// use the hash-compacted `rescan::SubtreeDiff` instead; this String-keyed map
/// only ever holds one directory level.)
fn row_unchanged(
    existing: &mut HashMap<String, (Option<i64>, Option<i64>)>,
    row: &IndexRow,
) -> bool {
    matches!(
        existing.remove(&row.path),
        Some((old_mtime, old_size)) if old_mtime == row.mtime && old_size == row.size
    )
}

/// Existing rows for `dir_path` itself and its DIRECT children only.
/// (A recursive subtree load for the scan root would be the entire DB.)
fn preload_direct_children(
    conn: &Connection,
    dir_path: &str,
) -> HashMap<String, (Option<i64>, Option<i64>)> {
    let mut map = HashMap::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT path, mtime, size FROM entries WHERE dir = ?1 OR path = ?1")
    else {
        return map;
    };
    let rows = match stmt.query_map(params![dir_path], |row| {
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

/// Build the shared rayon thread pool for a parallel filesystem scan. Scanning
/// is I/O-bound (stat() blocks on disk), so we oversubscribe cores. Returns the
/// pool, its thread count, and the CPU count (callers derive worker count from
/// it differently). Shared by the fresh and catchup scan branches.
fn build_scan_pool() -> (Arc<jwalk::rayon::ThreadPool>, usize, usize) {
    let n_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let pool_threads = (n_cpus * 2).max(8);
    let pool = Arc::new(
        jwalk::rayon::ThreadPoolBuilder::new()
            .num_threads(pool_threads)
            .build()
            .expect("failed to build rayon pool for parallel scan"),
    );
    (pool, pool_threads, n_cpus)
}

/// `app: None` runs the index pipeline without UI event emission and without
/// spawning the background finalizing thread (the caller finalizes explicitly)
/// — used by benchmarks/tests to exercise the real indexing path.
fn run_incremental_index(app: Option<&AppHandle>, state: &AppState) -> AppResult<()> {
    let started = Instant::now();
    perf_log(format!(
        "index_run_start home={} db={}",
        state.home_dir.to_string_lossy(),
        state.db_path.to_string_lossy(),
    ));

    let mut conn = db_connection(&state.db_path)?;
    set_indexing_pragmas(&conn)?;

    // For fresh index (empty DB), drop secondary indexes before bulk insert and
    // recreate them after. This avoids per-row BTREE maintenance on every insert,
    // which can be 3-5x faster when loading hundreds of thousands of rows.
    let is_fresh_run: bool = get_meta(&conn, "last_run_id")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        == 0;
    // A previous run crashed between dropping the FTS triggers and completing
    // the rebuild: the FTS index may silently miss rows. Stop trusting it now;
    // finalize_fresh_index below rebuilds it and clears the flag.
    let fts_dirty: bool = get_meta(&conn, "fts_dirty")
        .map(|v| v == "1")
        .unwrap_or(false);
    if fts_dirty && !is_fresh_run {
        state.fts_ready.store(false, AtomicOrdering::Release);
    }
    if is_fresh_run {
        let _ = conn.execute_batch(
            "DROP INDEX IF EXISTS idx_entries_dir_ext_name_nocase;
             DROP INDEX IF EXISTS idx_entries_mtime;
             DROP INDEX IF EXISTS idx_entries_name_nocase;
             DROP INDEX IF EXISTS idx_entries_ext_name;",
        );
    }

    let result = run_incremental_index_inner(app, state, &mut conn);

    // Fresh runs defer secondary-index creation, FTS rebuild, and ANALYZE to
    // finalize_fresh_index (background finalizing thread): search is already
    // functional via the LIKE/INDEXED-BY fallbacks and fts_ready gating, so
    // the user-visible indexing phase ends without waiting for DDL.
    if !is_fresh_run {
        let t_analyze = Instant::now();
        // Catchup barely shifts table statistics; PRAGMA optimize re-analyzes
        // only when SQLite deems it worthwhile (usually a no-op) instead of
        // rescanning every index on every start.
        let _ = conn.execute_batch("PRAGMA optimize;");
        eprintln!("[timing] analyze {}ms", t_analyze.elapsed().as_millis());
    }
    let t_checkpoint = Instant::now();
    let _ = restore_normal_pragmas(&conn);
    eprintln!("[timing] checkpoint {}ms", t_checkpoint.elapsed().as_millis());

    match &result {
        Ok(_) => {
            let _ = set_meta(&conn, "index_complete", "1");
            let extra_roots = state.extra_roots.lock().clone();
            let roots_str: Vec<String> = extra_roots.iter().map(|r| r.to_string_lossy().to_string()).collect();
            let _ = set_meta(&conn, "indexed_extra_roots", &roots_str.join("\n"));
            // Transition to Ready, then run deferred DDL (fresh finalization,
            // ensure_db_indexes) + GC in one background thread. Search works
            // during the DDL window via LIKE/INDEXED-BY fallbacks; the watcher
            // stays paused until indexing_active is released at the end, so
            // no rows can slip past the not-yet-recreated FTS triggers.
            if let Some(app) = app {
                let fin_app = app.clone();
                let fin_state = state.clone();
                std::thread::spawn(move || {
                    // Guard: ensure indexing_active is released even on panic.
                    struct IndexingGuard(Arc<AtomicBool>);
                    impl Drop for IndexingGuard {
                        fn drop(&mut self) {
                            self.0.store(false, AtomicOrdering::Release);
                        }
                    }
                    let _guard = IndexingGuard(Arc::clone(&fin_state.indexing_active));

                    // 1. Transition to Ready: update state and emit event
                    // (index_updated already emitted by run_incremental_index_inner)
                    let message = fin_state.status.lock().message.clone();
                    {
                        let mut status = fin_state.status.lock();
                        status.state = IndexState::Ready;
                    }
                    emit_index_state(&fin_app, "Ready", message);
                    perf_log("index_state=Ready (finalizing continues in background)");

                    // 2. Fresh runs (or crash recovery): build secondary
                    // indexes, rebuild FTS, ANALYZE.
                    if is_fresh_run || fts_dirty {
                        finalize_fresh_index(&fin_state);
                    }
                    // 3. Ensure any new indexes exist (DDL, may take a few seconds on large DB)
                    if let Err(e) = ensure_db_indexes(&fin_state.db_path) {
                        eprintln!("[finalizing] ensure_db_indexes error: {e}");
                    }
                    // 4. Drop any orphaned entries_gc_* tables from crashed sessions
                    if let Ok(c) = db_connection(&fin_state.db_path) {
                        if let Err(e) = cleanup_entries_gc_tables(&c) {
                            eprintln!("[gc] cleanup error: {e}");
                        }
                    }
                    // 5. Storage + memory maintenance: reclaim free pages
                    // (threshold-gated VACUUM), truncate the WAL, and return
                    // freed heap to the OS while the watcher is still paused.
                    run_db_maintenance(&fin_state);
                    release_memory_to_os();
                    // indexing_active released by _guard Drop
                    perf_log("finalizing complete");
                });
            } else {
                // Bench/test path: the caller runs finalize_fresh_index /
                // ensure_db_indexes itself so each phase can be timed.
                state.status.lock().state = IndexState::Ready;
                state.indexing_active.store(false, AtomicOrdering::Release);
            }
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
    app: Option<&AppHandle>,
    state: &AppState,
    conn: &mut Connection,
) -> AppResult<()> {

    let (runtime_ignored_roots, runtime_ignored_patterns) = effective_ignore_rules(
        &state.config_file_path,
        &state.home_dir,
        &state.cwd,
        state.path_ignores.as_ref(),
        state.path_ignore_patterns.as_ref(),
    );

    let last_run_id: i64 = get_meta(conn, "last_run_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let current_run_id = last_run_id + 1;
    // Fresh index: DB was empty, skip preload/stamp, single unlimited-depth pass
    let is_fresh = last_run_id == 0;

    // Fresh index has no secondary indexes to maintain, so we can use larger
    // batches to reduce transaction overhead (fewer commits = faster).
    let flush_batch_size = if is_fresh { BATCH_SIZE * 5 } else { BATCH_SIZE };

    // Fresh index: drop FTS triggers to avoid per-row FTS overhead during bulk insert.
    // A single FTS rebuild runs after all rows are written (much faster than 3.6M trigger fires).
    // Mark FTS as not ready so search falls back to LIKE queries during bulk insert.
    if is_fresh {
        state.fts_ready.store(false, AtomicOrdering::Release);
        // Persisted so a crash before the FTS rebuild completes is healed on
        // the next run (finalize_fresh_index runs whenever the flag is set).
        let _ = set_meta(conn, "fts_dirty", "1");
        let _ = conn.execute_batch(DROP_FTS_TRIGGERS_SQL);
    }

    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut permission_errors: u64 = 0;
    let mut current_path = String::new();
    let mut batch: Vec<IndexRow> = Vec::with_capacity(flush_batch_size);
    // Paths removed because they vanished from disk (catchup set-difference).
    let mut catchup_deleted: u64 = 0;
    let mut last_emit = Instant::now();
    let mut last_perf_emit = Instant::now();

    // Preload scan_root-level entries (direct children only, not recursive).
    // Seen entries are removed; leftovers are vanished top-level paths whose
    // subtrees are deleted below.
    let scan_str = state.scan_root.to_string_lossy().to_string();
    let mut root_existing = preload_direct_children(conn, &scan_str);

    // Index scan_root itself
    if let Some(mut row) = index_row_from_path(&state.scan_root) {
        scanned += 1;
        row.run_id = current_run_id;
        if !row_unchanged(&mut root_existing, &row) {
            batch.push(row);
        }
    }

    // Partition direct children into priority vs deferred
    let mut priority_roots: Vec<PathBuf> = Vec::new();
    let mut deferred_roots: Vec<PathBuf> = Vec::new();

    if let Ok(entries) = fs::read_dir(&state.scan_root) {
        for dir_entry in entries.flatten() {
            let child_path = dir_entry.path();

            if should_skip_path(
                &child_path,
                &runtime_ignored_roots,
                &runtime_ignored_patterns,
            ) {
                continue;
            }

            scanned += 1;
            current_path = child_path.to_string_lossy().to_string();

            if let Some(mut row) = index_row_from_path(&child_path) {
                row.run_id = current_run_id;
                if !row_unchanged(&mut root_existing, &row) {
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

    // Direct children still in the map vanished from disk (or are newly
    // ignored): their entire subtrees are stale.
    if !is_fresh && !root_existing.is_empty() {
        let vanished: Vec<String> = root_existing.into_keys().collect();
        catchup_deleted += delete_paths(conn, &vanished)? as u64;
    }

    priority_roots.sort();
    deferred_roots.sort();

    let extra_roots = state.extra_roots.lock().clone();
    let scanned_extra_roots: Vec<PathBuf> = extra_roots
        .into_iter()
        .filter(|r| r.is_dir() && !r.starts_with(&state.scan_root))
        .collect();
    // Extra roots that were indexed on a previous run but are no longer
    // scanned (removed from .pathindexing or vanished) leave stale subtrees.
    if !is_fresh {
        if let Some(prev) = get_meta(conn, "indexed_extra_roots") {
            let current_set: std::collections::HashSet<String> = scanned_extra_roots
                .iter()
                .map(|r| r.to_string_lossy().to_string())
                .collect();
            let stale: Vec<String> = prev
                .split('\n')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .filter(|s| !current_set.contains(*s) && !Path::new(s).starts_with(&state.scan_root))
                .map(|s| s.to_string())
                .collect();
            if !stale.is_empty() {
                catchup_deleted += delete_paths(conn, &stale)? as u64;
            }
        }
    }
    let roots: Vec<PathBuf> = priority_roots
        .into_iter()
        .chain(deferred_roots)
        .chain(scanned_extra_roots)
        .collect();
    perf_log(format!(
        "index_scan_roots total={} (incl. extra pathindexing roots)",
        roots.len()
    ));

    // Both branches below use jwalk for parallel scanning.
    let arc_ignored_roots = Arc::new(runtime_ignored_roots.clone());
    let arc_ignored_patterns = Arc::new(runtime_ignored_patterns.clone());


    if is_fresh {
        // FRESH INDEX: parallel root scan.
        // All roots are scanned concurrently by N worker threads; the main thread
        // receives row batches via channel and writes them to SQLite.
        // FTS triggers were dropped above; FTS is rebuilt in one pass after all rows
        // are written (avoids ~3.6M individual B-tree trigger inserts).

        // Flush scan_root + direct-children batch before parallel workers start.
        if !batch.is_empty() {
            batch.sort_unstable_by(|a, b| a.path.cmp(&b.path));
            insert_rows_fresh(conn, &batch)?;
            batch.clear();
        }

        // Shared rayon pool: all workers share one pool sized for maximum I/O throughput.
        let (shared_pool, pool_threads, n_cpus) = build_scan_pool();
        let n_workers = n_cpus.min(roots.len().max(1));
        eprintln!(
            "[index] fresh parallel scan: {} workers, {} pool threads, {} roots",
            n_workers,
            pool_threads,
            roots.len()
        );

        type ScanMsg = (Vec<IndexRow>, u64, u64, u64, String);
        let (row_tx, row_rx) = std::sync::mpsc::sync_channel::<ScanMsg>(SCAN_CHANNEL_CAP);
        let roots_arc: Arc<Vec<PathBuf>> = Arc::new(roots);
        let par_started = Instant::now();

        std::thread::scope(|scope| -> AppResult<()> {
            for worker_idx in 0..n_workers {
                let tx = row_tx.clone();
                let skip_roots = arc_ignored_roots.clone();
                let skip_patterns = arc_ignored_patterns.clone();
                let roots_ref = roots_arc.clone();
                let run_id = current_run_id;
                let n_w = n_workers;
                let pool = shared_pool.clone();
                // Use BATCH_SIZE (not flush_batch_size) so workers send batches every
                // 10k rows instead of 50k → progress updates reach the main thread sooner.
                let flush_size = BATCH_SIZE;

                scope.spawn(move || {
                    let mut local_batch: Vec<IndexRow> = Vec::with_capacity(flush_size);
                    let mut local_scanned = 0u64;
                    let mut local_indexed = 0u64;
                    let mut local_perm_errors = 0u64;
                    let mut local_path = String::new();

                    // Interleaved distribution: worker 0 handles roots[0, n_w, 2*n_w, …]
                    for i in (worker_idx..roots_ref.len()).step_by(n_w) {
                        let root = &roots_ref[i];
                        let root_started = Instant::now();
                        let mut root_scanned = 0u64;
                        let mut root_indexed = 0u64;
                        let mut root_perm_errors = 0u64;
                        let root_str = root.to_string_lossy().to_string();

                        let s_roots = skip_roots.clone();
                        let s_patterns = skip_patterns.clone();
                        let walker =
                            jwalk::WalkDirGeneric::<((), Option<fs::Metadata>)>::new(root)
                                .follow_links(false)
                                .skip_hidden(false)
                                .parallelism(jwalk::Parallelism::RayonExistingPool {
                                    pool: pool.clone(),
                                    busy_timeout: None,
                                })
                                .process_read_dir(move |_depth, path, _state, children| {
                                    children.retain_mut(|entry_result| match entry_result {
                                        Ok(entry) => {
                                            let full_path = path.join(&entry.file_name);
                                            if should_skip_path(
                                                &full_path,
                                                &s_roots,
                                                &s_patterns,
                                            ) {
                                                return false;
                                            }
                                            entry.client_state =
                                                fs::symlink_metadata(&full_path).ok();
                                            true
                                        }
                                        Err(_) => false,
                                    });
                                });

                        for result in walker {
                            match result {
                                Ok(entry) => {
                                    let path = entry.path();
                                    if path == root.as_path() {
                                        continue;
                                    }
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    local_path = path.to_string_lossy().to_string();

                                    let metadata = match entry.client_state {
                                        Some(m) => m,
                                        None => {
                                            local_perm_errors += 1;
                                            root_perm_errors += 1;
                                            continue;
                                        }
                                    };

                                    if let Some(mut row) =
                                        index_row_from_path_and_metadata(&path, &metadata)
                                    {
                                        row.run_id = run_id;
                                        local_indexed += 1;
                                        root_indexed += 1;
                                        local_batch.push(row);
                                    }

                                    if local_batch.len() >= flush_size {
                                        // Sort on the worker (parallel) so the
                                        // single writer thread gets b-tree
                                        // friendly, pre-ordered batches.
                                        local_batch
                                            .sort_unstable_by(|a, b| a.path.cmp(&b.path));
                                        let _ = tx.send((
                                            std::mem::replace(&mut local_batch, Vec::with_capacity(flush_size)),
                                            local_scanned,
                                            local_indexed,
                                            local_perm_errors,
                                            local_path.clone(),
                                        ));
                                        local_scanned = 0;
                                        local_indexed = 0;
                                        local_perm_errors = 0;
                                    }
                                }
                                Err(err) => {
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    local_perm_errors += 1;
                                    root_perm_errors += 1;
                                    if local_perm_errors <= 20 || perf_log_enabled() {
                                        eprintln!("[index] permission error: {}", err);
                                    }
                                }
                            }
                        }

                        eprintln!(
                            "[timing]   walk_fresh_par {} {}ms scanned={} indexed={} err={}",
                            root_str,
                            root_started.elapsed().as_millis(),
                            root_scanned,
                            root_indexed,
                            root_perm_errors,
                        );
                    }

                    // Flush partial batch remaining after all roots processed.
                    if !local_batch.is_empty() || local_scanned > 0 || local_perm_errors > 0 {
                        local_batch.sort_unstable_by(|a, b| a.path.cmp(&b.path));
                        let _ = tx.send((
                            local_batch,
                            local_scanned,
                            local_indexed,
                            local_perm_errors,
                            local_path,
                        ));
                    }
                });
            }

            // Drop original sender: channel exhausts when all worker clones drop.
            drop(row_tx);

            // Main thread: receive row batches from workers and write to SQLite.
            for (worker_batch, s, i, pe, path) in row_rx {
                scanned += s;
                indexed += i;
                permission_errors += pe;
                if !path.is_empty() {
                    current_path = path;
                }
                if !worker_batch.is_empty() {
                    insert_rows_fresh(conn, &worker_batch)?;
                }
                if last_emit.elapsed() >= Duration::from_millis(200) {
                    set_progress(state, scanned, indexed, &current_path);
                    if let Some(app) = app {
                        emit_index_progress(app, scanned, indexed, current_path.clone());
                    }
                    last_emit = Instant::now();
                }
            }
            Ok(())
        })?;

        perf_log(format!(
            "index_pass_done pass=fresh_par elapsed_ms={} scanned={} indexed={}",
            par_started.elapsed().as_millis(),
            scanned,
            indexed,
        ));
    } else {
        // INCREMENTAL CATCHUP: single parallel pass with worker-side change
        // detection. Each worker preloads its root's existing rows over its own
        // read connection (WAL allows readers concurrent with the writer),
        // walks the root once, and sends only new/changed rows for upsert plus
        // vanished paths (preload leftovers) for deletion. Unchanged rows are
        // left untouched — the previous design walked every root twice and
        // rewrote run_id on every unchanged row (one UPDATE per file).

        // Flush scan_root + direct-children batch before parallel workers start.
        if !batch.is_empty() {
            upsert_rows(conn, &batch)?;
            batch.clear();
        }

        let (shared_pool, pool_threads, n_cpus) = build_scan_pool();
        // Each worker holds one root's preload map in memory; cap workers to
        // bound peak memory when several large roots are processed at once.
        let n_workers = n_cpus.min(roots.len().max(1)).min(8);
        eprintln!(
            "[index] catchup parallel scan: {} workers, {} pool threads, {} roots",
            n_workers,
            pool_threads,
            roots.len()
        );

        // (rows to upsert, vanished paths to delete, scanned, indexed, perm_errors, current path)
        type CatchupMsg = (Vec<IndexRow>, Vec<String>, u64, u64, u64, String);
        let (row_tx, row_rx) = std::sync::mpsc::sync_channel::<CatchupMsg>(SCAN_CHANNEL_CAP);
        let roots_arc: Arc<Vec<PathBuf>> = Arc::new(roots);
        let par_started = Instant::now();
        let worker_db_path = state.db_path.clone();

        std::thread::scope(|scope| -> AppResult<()> {
            for worker_idx in 0..n_workers {
                let tx = row_tx.clone();
                let skip_roots = arc_ignored_roots.clone();
                let skip_patterns = arc_ignored_patterns.clone();
                let roots_ref = roots_arc.clone();
                let run_id = current_run_id;
                let n_w = n_workers;
                let pool = shared_pool.clone();
                let flush_size = BATCH_SIZE;
                let worker_db = worker_db_path.clone();

                scope.spawn(move || {
                    let worker_conn = db_connection(&worker_db).ok();
                    let mut local_batch: Vec<IndexRow> = Vec::with_capacity(flush_size);
                    let mut local_scanned = 0u64;
                    let mut local_indexed = 0u64;
                    let mut local_perm_errors = 0u64;
                    let mut local_path = String::new();

                    for i in (worker_idx..roots_ref.len()).step_by(n_w) {
                        let root = &roots_ref[i];
                        let root_started = Instant::now();
                        let mut root_scanned = 0u64;
                        let mut root_indexed = 0u64;
                        let mut root_perm_errors = 0u64;
                        let root_str = root.to_string_lossy().to_string();

                        let mut existing = worker_conn
                            .as_ref()
                            .map(|c| rescan::SubtreeDiff::load(c, &root_str))
                            .unwrap_or_else(rescan::SubtreeDiff::empty);
                        // The walk below skips the root entry itself (its row is
                        // maintained by the scan_root direct-children loop) — it
                        // must not be treated as vanished.
                        existing.forget(&root_str);

                        let s_roots = skip_roots.clone();
                        let s_patterns = skip_patterns.clone();
                        let walker =
                            jwalk::WalkDirGeneric::<((), Option<fs::Metadata>)>::new(root)
                                .follow_links(false)
                                .skip_hidden(false)
                                .parallelism(jwalk::Parallelism::RayonExistingPool {
                                    pool: pool.clone(),
                                    busy_timeout: None,
                                })
                                .process_read_dir(move |_depth, path, _state, children| {
                                    children.retain_mut(|entry_result| match entry_result {
                                        Ok(entry) => {
                                            let full_path = path.join(&entry.file_name);
                                            if should_skip_path(
                                                &full_path,
                                                &s_roots,
                                                &s_patterns,
                                            ) {
                                                return false;
                                            }
                                            entry.client_state =
                                                fs::symlink_metadata(&full_path).ok();
                                            true
                                        }
                                        Err(_) => false,
                                    });
                                });

                        for result in walker {
                            match result {
                                Ok(entry) => {
                                    let path = entry.path();
                                    if path == root.as_path() {
                                        continue;
                                    }
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    local_path = path.to_string_lossy().to_string();

                                    let metadata = match entry.client_state {
                                        Some(m) => m,
                                        None => {
                                            local_perm_errors += 1;
                                            root_perm_errors += 1;
                                            // Unreadable metadata: keep the
                                            // existing row, don't delete it.
                                            existing.mark_errored(&path);
                                            continue;
                                        }
                                    };

                                    if let Some(mut row) =
                                        index_row_from_path_and_metadata(&path, &metadata)
                                    {
                                        row.run_id = run_id;
                                        if !existing.check_unchanged(&row) {
                                            local_indexed += 1;
                                            root_indexed += 1;
                                            local_batch.push(row);
                                        }
                                    }

                                    if local_batch.len() >= flush_size {
                                        let _ = tx.send((
                                            std::mem::replace(
                                                &mut local_batch,
                                                Vec::with_capacity(flush_size),
                                            ),
                                            Vec::new(),
                                            local_scanned,
                                            local_indexed,
                                            local_perm_errors,
                                            local_path.clone(),
                                        ));
                                        local_scanned = 0;
                                        local_indexed = 0;
                                        local_perm_errors = 0;
                                    }
                                }
                                Err(err) => {
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    local_perm_errors += 1;
                                    root_perm_errors += 1;
                                    // Keep rows under an unreadable directory;
                                    // NotFound means it truly vanished and its
                                    // rows should become deletion leftovers.
                                    let vanished = err
                                        .io_error()
                                        .map(|e| e.kind() == std::io::ErrorKind::NotFound)
                                        .unwrap_or(false);
                                    if !vanished {
                                        if let Some(p) = err.path() {
                                            existing.mark_errored(p);
                                        }
                                    }
                                    if local_perm_errors <= 20 || perf_log_enabled() {
                                        eprintln!("[index] permission error: {}", err);
                                    }
                                }
                            }
                        }

                        // Rows still in the snapshot were not seen on disk:
                        // deleted. Path strings are re-read from the DB (the
                        // snapshot only keeps hashes).
                        if let Some(c) = worker_conn.as_ref() {
                            let mut deletes = existing.leftover_paths(c, &root_str);
                            while !deletes.is_empty() {
                                let rest = deletes.split_off(flush_size.min(deletes.len()));
                                let chunk = std::mem::replace(&mut deletes, rest);
                                let _ = tx.send((Vec::new(), chunk, 0, 0, 0, String::new()));
                            }
                        }

                        eprintln!(
                            "[timing]   walk_catchup {} {}ms scanned={} indexed={} err={}",
                            root_str,
                            root_started.elapsed().as_millis(),
                            root_scanned,
                            root_indexed,
                            root_perm_errors,
                        );
                    }

                    if !local_batch.is_empty() || local_scanned > 0 || local_perm_errors > 0 {
                        let _ = tx.send((
                            local_batch,
                            Vec::new(),
                            local_scanned,
                            local_indexed,
                            local_perm_errors,
                            local_path,
                        ));
                    }
                });
            }

            drop(row_tx);

            for (worker_batch, deletes, s, i, pe, path) in row_rx {
                scanned += s;
                indexed += i;
                permission_errors += pe;
                if !path.is_empty() {
                    current_path = path;
                }
                if !worker_batch.is_empty() {
                    upsert_rows(conn, &worker_batch)?;
                }
                if !deletes.is_empty() {
                    catchup_deleted += delete_paths(conn, &deletes)? as u64;
                }
                if last_emit.elapsed() >= Duration::from_millis(200) {
                    set_progress(state, scanned, indexed, &current_path);
                    if let Some(app) = app {
                        emit_index_progress(app, scanned, indexed, current_path.clone());
                    }
                    last_emit = Instant::now();
                }
                if perf_log_enabled() && last_perf_emit.elapsed() >= Duration::from_secs(1) {
                    perf_log(format!(
                        "index_progress pass=catchup_par scanned={} indexed={} current_path={}",
                        scanned, indexed, current_path
                    ));
                    last_perf_emit = Instant::now();
                }
            }
            Ok(())
        })?;

        perf_log(format!(
            "index_pass_done pass=catchup_par elapsed_ms={} scanned={} indexed={} deleted={}",
            par_started.elapsed().as_millis(),
            scanned,
            indexed,
            catchup_deleted,
        ));
    }

    if !batch.is_empty() {
        upsert_rows(conn, &batch)?;
    }

    // Fresh index: FTS rebuild + trigger recreation happen in
    // finalize_fresh_index on the background finalizing thread; fts_ready
    // stays false so search keeps using the LIKE fallbacks until then.

    set_progress(state, scanned, indexed, &current_path);
    if let Some(app) = app {
        emit_index_progress(app, scanned, indexed, current_path.clone());
    }

    // Deleted files were removed as preload-map leftovers per root during the
    // catchup scan (set difference), replacing the old full-table run_id sweep.
    let deleted_count: i64 = catchup_deleted as i64;

    set_meta(conn, "last_run_id", &current_run_id.to_string())?;

    if deleted_count > 0 || indexed > 0 {
        invalidate_search_caches(state);
    }

    let (entries_count, last_updated) = update_counts(conn)?;
    {
        let mut status = state.status.lock();
        status.entries_count = entries_count;
        status.last_updated = last_updated;
    }
    // Persist cached counts for instant startup next time
    persist_cached_counts(conn, entries_count, last_updated);
    let updated_at = last_updated.unwrap_or_else(now_epoch);

    {
        let mut status = state.status.lock();
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
    if let Some(app) = app {
        emit_index_progress(app, scanned, indexed, current_path);
        emit_index_updated(app, entries_count, updated_at, permission_errors);
    }
    perf_log(format!(
        "index_scan_done scanned={} indexed={} deleted={} entries={} permission_errors={}",
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

/// Result of one watcher batch: `changed` mirrors the old rows-touched count,
/// `count_delta` is the exact net change in `entries` row count (inserted
/// minus deleted), so callers can maintain `entries_count` incrementally
/// instead of re-running `COUNT(*)` over the whole table.
#[cfg(target_os = "macos")]
struct PathChangeOutcome {
    changed: usize,
    count_delta: i64,
}

/// How many of `rows` already exist in `entries`, checked in chunks that stay
/// under SQLite's bound-parameter limit. Point lookups on the UNIQUE path
/// index — cheap even for large watcher batches.
#[cfg(target_os = "macos")]
fn count_existing_paths(conn: &Connection, rows: &[IndexRow]) -> AppResult<usize> {
    let mut existing: i64 = 0;
    for chunk in rows.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT COUNT(*) FROM entries WHERE path IN ({placeholders})");
        existing += conn
            .query_row(&sql, params_from_iter(chunk.iter().map(|r| &r.path)), |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(existing.max(0) as usize)
}

#[cfg(target_os = "macos")]
fn apply_path_changes(state: &AppState, paths: &[PathBuf]) -> AppResult<PathChangeOutcome> {
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
        return Ok(PathChangeOutcome {
            changed: 0,
            count_delta: 0,
        });
    }

    let op_start = std::time::Instant::now();
    // Reuse one persistent write connection across event batches: opening a
    // connection per batch dominated the cost of small (single-file) updates.
    let mut conn_slot = state.watcher_conn.lock();
    if conn_slot.is_none() {
        *conn_slot = Some(db_connection(&state.db_path).map_err(|e| {
            eprintln!(
                "[watcher] db_connection FAILED after {}ms: {} (upsert={} delete={} indexing_active={})",
                op_start.elapsed().as_millis(),
                e,
                to_upsert.len(),
                to_delete.len(),
                state.indexing_active.load(AtomicOrdering::Acquire)
            );
            e
        })?);
    }
    let conn = conn_slot.as_mut().expect("watcher connection present");

    let result: AppResult<PathChangeOutcome> = (|| {
        let existing = count_existing_paths(conn, &to_upsert)?;
        let up = upsert_rows(conn, &to_upsert)?;
        let del = delete_paths(conn, &to_delete)?;
        Ok(PathChangeOutcome {
            changed: up + del,
            count_delta: to_upsert.len() as i64 - existing as i64 - del as i64,
        })
    })();
    if result.is_err() {
        // Drop the connection on failure so the next batch reopens cleanly
        // (the caller already handles busy-retry by re-queueing paths).
        *conn_slot = None;
    }
    result
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
        Ok(outcome) => {
            *deadline = None;
            if outcome.changed > 0 {
                invalidate_search_caches(state);
                {
                    // Maintain counts incrementally — the rows just written
                    // carry indexed_at = now, and outcome.count_delta is the
                    // exact net row change, so no COUNT(*)/MAX() needed.
                    let mut status = state.status.lock();
                    status.entries_count =
                        (status.entries_count as i64 + outcome.count_delta).max(0) as u64;
                    status.last_updated = Some(now_epoch());
                }
                if last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
                    emit_and_persist_cached_counts(app, state);
                    *last_status_emit = Instant::now();
                    *pending_status_emit = false;
                } else {
                    *pending_status_emit = true;
                }
            }
        }
        Err(err) => {
            let is_busy = err.contains("locked") || err.contains("busy") || err.contains("unable to open");
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

/// Minimum spacing between two MustScanSubDirs subtree rescans of the same
/// path. Extra events arriving inside the window are deferred (kept queued),
/// not dropped, so an FSEvents overflow storm can't trigger back-to-back
/// full-subtree rescans.
#[cfg(target_os = "macos")]
const RESCAN_COOLDOWN: Duration = Duration::from_secs(300);

/// Watch roots for the FSEvents stream: `$HOME` plus canonicalized
/// `.pathindexing` extra roots (FSEvents needs real paths — `/tmp` is a
/// symlink to `/private/tmp`). Also returns (canonical → stored) prefix pairs
/// so event paths can be mapped back to the form rows are stored under.
#[cfg(target_os = "macos")]
fn fsevent_watch_roots(state: &AppState) -> (Vec<PathBuf>, Vec<(PathBuf, PathBuf)>) {
    let mut roots = vec![state.home_dir.clone()];
    let mut remaps: Vec<(PathBuf, PathBuf)> = Vec::new();
    for stored in state.extra_roots.lock().iter() {
        let canonical = fs::canonicalize(stored).unwrap_or_else(|_| stored.clone());
        // The remap is independent of watch selection: even a root watched by
        // proxy (canonical form inside $HOME or duplicating another root)
        // still delivers canonical event paths that must translate back.
        if canonical != *stored {
            remaps.push((canonical.clone(), stored.clone()));
        }
        if canonical.starts_with(&state.home_dir) || roots.contains(&canonical) {
            continue; // already covered by another registered root
        }
        roots.push(canonical);
    }
    // Longest canonical prefix first so nested roots remap correctly.
    remaps.sort_by_key(|(canonical, _)| std::cmp::Reverse(canonical.as_os_str().len()));
    (roots, remaps)
}

/// Translate FSEvents' canonical paths back to stored-prefix form at the
/// single point events enter the worker, so every consumer sees paths in the
/// form rows are stored under.
#[cfg(target_os = "macos")]
fn remap_fs_event(
    event: mac::fsevent_watcher::FsEvent,
    remaps: &[(PathBuf, PathBuf)],
) -> mac::fsevent_watcher::FsEvent {
    use mac::fsevent_watcher::FsEvent;
    if remaps.is_empty() {
        return event;
    }
    match event {
        FsEvent::Paths(paths) => FsEvent::Paths(
            paths
                .into_iter()
                .map(|p| remap_event_path(p, remaps))
                .collect(),
        ),
        FsEvent::MustScanSubDirs(p) => FsEvent::MustScanSubDirs(remap_event_path(p, remaps)),
        FsEvent::HistoryDone => FsEvent::HistoryDone,
    }
}

/// Map an FSEvents path (always canonical) back to the stored-prefix form,
/// e.g. `/private/tmp/x` → `/tmp/x` when `/tmp` is the indexed root.
#[cfg(target_os = "macos")]
fn remap_event_path(path: PathBuf, remaps: &[(PathBuf, PathBuf)]) -> PathBuf {
    for (canonical, stored) in remaps {
        if let Ok(rest) = path.strip_prefix(canonical) {
            return if rest.as_os_str().is_empty() {
                stored.clone()
            } else {
                stored.join(rest)
            };
        }
    }
    path
}

/// Earliest instant `path` may be rescanned, given the cooldown of recent
/// overlapping rescans. `Instant::now()` when nothing overlaps.
#[cfg(target_os = "macos")]
fn rescan_not_before(path: &Path, finished: &Mutex<Vec<(PathBuf, Instant)>>) -> Instant {
    let mut not_before = Instant::now();
    for (done, at) in finished.lock().iter() {
        if path.starts_with(done) || done.starts_with(path) {
            let earliest = *at + RESCAN_COOLDOWN;
            if earliest > not_before {
                not_before = earliest;
            }
        }
    }
    not_before
}

/// Queue `path` for a subtree rescan, collapsing paths already covered by a
/// queued ancestor and deferring past the cooldown of recent overlapping
/// rescans. The stored due time is only a lower bound — `spawn_due_subtree_rescan`
/// re-checks the cooldown when it selects a path, so a rescan that is still
/// in flight when `path` is queued (and thus not yet in `finished`) is honored
/// once it completes.
#[cfg(target_os = "macos")]
fn queue_subtree_rescan(
    path: PathBuf,
    queued: &mut HashMap<PathBuf, Instant>,
    finished: &Mutex<Vec<(PathBuf, Instant)>>,
) {
    if queued.keys().any(|q| path.starts_with(q)) {
        return; // an equal or ancestor rescan is already queued
    }
    let not_before = rescan_not_before(&path, finished);
    // Queued descendants are covered by this wider rescan.
    queued.retain(|q, _| !q.starts_with(&path));
    queued.insert(path, not_before);
}

/// Spawn the next due queued subtree rescan on a background thread
/// (single-flight via `inflight`): a rescan can walk millions of entries over
/// minutes and must not block the watcher loop from draining events. Skipped
/// while an index pass is active — the pass reconciles the same ground — and
/// retried on the next loop tick.
#[cfg(target_os = "macos")]
fn spawn_due_subtree_rescan(
    app: &AppHandle,
    state: &AppState,
    queued: &mut HashMap<PathBuf, Instant>,
    inflight: &Arc<AtomicBool>,
    finished: &Arc<Mutex<Vec<(PathBuf, Instant)>>>,
) {
    if queued.is_empty()
        || inflight.load(AtomicOrdering::Acquire)
        || state.indexing_active.load(AtomicOrdering::Acquire)
    {
        return;
    }
    let now = Instant::now();
    let Some(path) = queued
        .iter()
        .find(|(_, due)| **due <= now)
        .map(|(path, _)| path.clone())
    else {
        return;
    };
    // The stored due time is a lower bound; re-check the cooldown now that an
    // overlapping rescan may have finished since this path was queued (e.g. the
    // just-completed inflight rescan of the same subtree). If still cooling
    // down, push the due time out and leave it queued for a later tick.
    let not_before = rescan_not_before(&path, finished);
    if not_before > now {
        queued.insert(path, not_before);
        return;
    }
    queued.remove(&path);
    inflight.store(true, AtomicOrdering::Release);

    let app = app.clone();
    let state = state.clone();
    let inflight = Arc::clone(inflight);
    let finished = Arc::clone(finished);
    std::thread::spawn(move || {
        // Hold the exclusive-writer guard for the rescan's duration: it opens
        // its own write connection and both walks and deletes over many
        // seconds, so without this it can race the full-index writer (fresh
        // insert hits SQLITE_BUSY and aborts), the .pathindexing scan, and the
        // live watcher's upserts (a deleted-and-recreated file gets wrongly
        // swept as vanished). All three already stand down on indexing_active.
        if state
            .indexing_active
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_err()
        {
            // An index pass started between the due-check and here; it
            // reconciles the same ground. Record no cooldown stamp, so a later
            // MustScanSubDirs re-queues this subtree.
            inflight.store(false, AtomicOrdering::Release);
            return;
        }
        let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(&state);
        let started = Instant::now();
        let result = db_connection(&state.db_path).and_then(|mut conn| {
            let counts =
                rescan::rescan_subtree(&mut conn, &path, &ignored_roots, &ignored_patterns)?;
            let _ = conn.execute_batch("PRAGMA shrink_memory;");
            Ok(counts)
        });
        state.indexing_active.store(false, AtomicOrdering::Release);
        let succeeded = result.is_ok();
        match result {
            Ok((upserted, deleted)) => {
                eprintln!(
                    "[watcher] MustScanSubDirs rescan {}: upserted={} deleted={} {}ms",
                    path.display(),
                    upserted,
                    deleted,
                    started.elapsed().as_millis()
                );
                if upserted + deleted > 0 {
                    invalidate_search_caches(&state);
                    touch_status_updated(&state);
                    let _ = refresh_and_emit_status_counts(&app, &state);
                }
                if upserted + deleted >= BATCH_SIZE {
                    release_memory_to_os();
                }
            }
            Err(err) => eprintln!(
                "[watcher] MustScanSubDirs rescan {} failed: {err}",
                path.display()
            ),
        }
        let mut finished = finished.lock();
        // Only stamp the cooldown on success: a failed rescan left the subtree
        // unreconciled, so it must not be blocked from a prompt retry.
        if succeeded {
            finished.push((path, Instant::now()));
        }
        finished.retain(|(_, at)| at.elapsed() < RESCAN_COOLDOWN);
        inflight.store(false, AtomicOrdering::Release);
    });
}

#[cfg(target_os = "macos")]
enum WatcherExit {
    Stop,
    Rebuild,
}

#[cfg(target_os = "macos")]
fn start_fsevent_watcher_worker(
    app: AppHandle,
    state: AppState,
    since_event_id: Option<u64>,
    conditional: bool,
) {
    std::thread::spawn(move || {
        let mut since_event_id = since_event_id;
        let mut replay = conditional;
        // Rescan bookkeeping outlives stream rebuilds: the cooldown history
        // and the single-flight guard for the background rescan thread.
        let rescan_inflight = Arc::new(AtomicBool::new(false));
        let finished_rescans: Arc<Mutex<Vec<(PathBuf, Instant)>>> =
            Arc::new(Mutex::new(Vec::new()));
        // Queued-but-not-yet-spawned rescans must also outlive stream rebuilds:
        // a .pathindexing edit rebuilds the stream, and a rescan deferred by
        // cooldown or the inflight guard would otherwise be silently dropped.
        let mut queued_rescans: HashMap<PathBuf, Instant> = HashMap::new();

        // Each pass runs one FSEvents stream; the stream is rebuilt (with
        // event-id continuity) when .pathindexing roots change.
        loop {
            let (tx, rx) = std::sync::mpsc::channel();
            let (watch_roots, remaps) = fsevent_watch_roots(&state);

            let built = mac::fsevent_watcher::FsEventWatcher::new(
                &watch_roots,
                since_event_id,
                tx.clone(),
            );
            let built = match built {
                Err(err) if watch_roots.len() > 1 => {
                    eprintln!(
                        "[watcher] FSEvents init failed for {} roots ({err}); falling back to home-only watch",
                        watch_roots.len()
                    );
                    mac::fsevent_watcher::FsEventWatcher::new(
                        std::slice::from_ref(&state.home_dir),
                        since_event_id,
                        tx,
                    )
                }
                other => other,
            };
            let mut watcher = match built {
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
                    // A prior iteration may have set watcher_active on a
                    // successful build; clear it before bailing so reset_index
                    // doesn't spin for its full 5s deadline waiting on a
                    // watcher thread that has already exited.
                    state.watcher_active.store(false, AtomicOrdering::Release);
                    return;
                }
            };

            state.watcher_active.store(true, AtomicOrdering::Release);

            if replay {
                perf_log("conditional_startup: watcher started, awaiting history replay");
                set_state(&state, IndexState::Ready, None);
                emit_index_state(&app, "Ready", None);
            }

            let exit = run_fsevent_stream(
                &app,
                &state,
                &rx,
                &watcher,
                &remaps,
                replay,
                &rescan_inflight,
                &finished_rescans,
                &mut queued_rescans,
            );

            let eid = watcher.last_event_id();
            let _ = persist_event_id(&state.db_path, eid);
            watcher.stop();

            match exit {
                WatcherExit::Rebuild => {
                    since_event_id = Some(eid);
                    replay = false;
                    eprintln!("[watcher] rebuilding FSEvents stream with updated watch roots");
                }
                WatcherExit::Stop => break,
            }
        }
        state.watcher_active.store(false, AtomicOrdering::Release);
        eprintln!("[watcher] fsevent watcher stopped");
    });
}

/// Drive one FSEvents stream until shutdown or a watch-roots change.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn run_fsevent_stream(
    app: &AppHandle,
    state: &AppState,
    rx: &std::sync::mpsc::Receiver<mac::fsevent_watcher::FsEvent>,
    watcher: &mac::fsevent_watcher::FsEventWatcher,
    remaps: &[(PathBuf, PathBuf)],
    replay: bool,
    rescan_inflight: &Arc<AtomicBool>,
    finished_rescans: &Arc<Mutex<Vec<(PathBuf, Instant)>>>,
    queued_rescans: &mut HashMap<PathBuf, Instant>,
) -> WatcherExit {
    use std::sync::mpsc::RecvTimeoutError;

    let mut pending_paths: HashSet<PathBuf> = HashSet::new();
    let mut deadline: Option<Instant> = None;
    let mut last_flush = Instant::now();
    let mut last_status_emit = Instant::now();
    let mut pending_status_emit = false;

    let mut must_scan_count: usize = 0;
    let mut replay_phase = replay;
    let mut full_scan_triggered = false;
    let mut rebuild_requested = false;

    // Snapshot config file entries at stream start so we only emit
    // pathignore_changed when actual rule entries change (ignores whitespace/comments).
    let mut last_config_entries =
        pathignore_active_entries(&fs::read_to_string(&state.config_file_path).unwrap_or_default());
    let mut last_pathindexing_entries =
        pathindexing::pathindexing_active_entries(&fs::read_to_string(&state.pathindexing_file_path).unwrap_or_default());

    loop {
        if state.watcher_stop.load(AtomicOrdering::Acquire) {
            break;
        }

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
        match rx.recv_timeout(wait).map(|ev| remap_fs_event(ev, remaps)) {
            Ok(mac::fsevent_watcher::FsEvent::Paths(paths)) => {
                let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(&state);
                let prev_len = pending_paths.len();
                for path in paths {
                    if path == state.config_file_path {
                        let new_entries = pathignore_active_entries(
                            &fs::read_to_string(&state.config_file_path).unwrap_or_default(),
                        );
                        if new_entries != last_config_entries {
                            last_config_entries = new_entries;
                            app.emit("pathignore_changed", ()).ok();
                        }
                        continue;
                    }
                    if path == state.pathindexing_file_path {
                        let new_entries = pathindexing::pathindexing_active_entries(
                            &fs::read_to_string(&state.pathindexing_file_path).unwrap_or_default(),
                        );
                        if new_entries != last_pathindexing_entries {
                            let old_roots = pathindexing::parse_pathindexing_paths_unchecked(
                                &last_pathindexing_entries.join("\n"),
                            );
                            let new_roots = pathindexing::load_pathindexing_roots(&state.pathindexing_file_path);
                            last_pathindexing_entries = new_entries;

                            let bg_state = state.clone();
                            let bg_app = app.clone();
                            let bg_new_roots = new_roots.clone();
                            *state.extra_roots.lock() = new_roots;
                            // Roots changed: rebuild the FSEvents stream so
                            // the new extra roots are watched live.
                            rebuild_requested = true;
                            if state.pathindexing_active.compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire).is_err() {
                                eprintln!("[pathindexing] scan already in progress, skipping");
                                continue;
                            }
                            set_state(&state, IndexState::Indexing, None);
                            emit_index_state(&app, "Indexing", None);
                            std::thread::spawn(move || {
                                // Wait for initial indexing to finish before writing to DB
                                while bg_state.indexing_active.load(AtomicOrdering::Acquire) {
                                    std::thread::sleep(Duration::from_secs(1));
                                    eprintln!("[pathindexing] waiting for indexing to finish...");
                                }
                                eprintln!("[pathindexing] background scan starting...");
                                let mut prev_roots = old_roots;
                                let mut target_roots = bg_new_roots;
                                loop {
                                    let (ign_roots, ign_patterns) = cached_effective_ignore_rules(&bg_state);
                                    match pathindexing::handle_pathindexing_change(
                                        &bg_state, &prev_roots, &target_roots, &ign_roots, &ign_patterns,
                                    ) {
                                        Ok(()) => {
                                            eprintln!("[pathindexing] background scan done");
                                            if let Ok(c) = db_connection(&bg_state.db_path) {
                                                let roots_str: Vec<String> = target_roots.iter().map(|r| r.to_string_lossy().to_string()).collect();
                                                let _ = set_meta(&c, "indexed_extra_roots", &roots_str.join("\n"));
                                            }
                                            let _ = refresh_and_emit_status_counts(&bg_app, &bg_state);
                                        }
                                        Err(e) => {
                                            eprintln!("[pathindexing] change handling error: {e}");
                                        }
                                    }
                                    // Check if extra_roots changed while we were scanning
                                    let current = bg_state.extra_roots.lock().clone();
                                    if current == target_roots {
                                        break;
                                    }
                                    eprintln!("[pathindexing] extra_roots changed during scan, reconciling...");
                                    prev_roots = target_roots;
                                    target_roots = current;
                                }
                                set_state(&bg_state, IndexState::Ready, None);
                                emit_index_state(&bg_app, "Ready", None);
                                bg_app.emit("pathindexing_changed", ()).ok();
                                bg_state.pathindexing_active.store(false, AtomicOrdering::Release);
                            });
                        }
                        continue;
                    }
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
                // Dropped events mean this subtree must be reconciled with
                // disk. Queue a streaming rescan (spawned from the loop
                // tail): bounded memory, change-detected, and rate-limited.
                queue_subtree_rescan(path, queued_rescans, finished_rescans);
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
            emit_and_persist_cached_counts(&app, &state);
            last_status_emit = Instant::now();
            pending_status_emit = false;
        }

        // Hold rescans until replay finishes: during replay the MustScanSubDirs
        // count decides whether to escalate to a full scan, and a rescan
        // grabbing the indexing_active guard first would preempt that. Queued
        // rescans survive to drain once replay ends (or are subsumed by the
        // full scan, which reconciles the same ground).
        if !replay_phase {
            spawn_due_subtree_rescan(
                &app,
                &state,
                queued_rescans,
                rescan_inflight,
                finished_rescans,
            );
        }

        // Periodic event_id flush
        if last_flush.elapsed() >= EVENT_ID_FLUSH_INTERVAL {
            let eid = watcher.last_event_id();
            let _ = persist_event_id(&state.db_path, eid);
            last_flush = Instant::now();
        }

        if rebuild_requested {
            break;
        }
    }

    // Final flush before this stream goes away (shutdown or rebuild).
    process_watcher_paths(
        app,
        state,
        &mut pending_paths,
        &mut deadline,
        &mut last_status_emit,
        &mut pending_status_emit,
    );

    if rebuild_requested && !state.watcher_stop.load(AtomicOrdering::Acquire) {
        WatcherExit::Rebuild
    } else {
        WatcherExit::Stop
    }
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

    run_swift_png(&script, None)
}

#[cfg(target_os = "macos")]
fn run_swift_png(script: &str, env: Option<(&str, &str)>) -> Option<Vec<u8>> {
    let mut cmd = Command::new("swift");
    cmd.arg("-e").arg(script);
    if let Some((key, value)) = env {
        cmd.env(key, value);
    }
    let output = cmd.output().ok()?;
    if output.status.success() && !output.stdout.is_empty() {
        Some(output.stdout)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn load_path_icon_png(path: &str) -> Option<Vec<u8>> {
    // Path is delivered via env var so it never touches the Swift source (no escaping issues).
    // Render at 16pt/2x so AppKit selects the small Retina representation used by Finder.
    // A 32pt/1x context selects different artwork that looks soft when scaled down in the UI.
    let script = r#"import AppKit
import Foundation
guard let path = ProcessInfo.processInfo.environment["EVERYTHING_ICON_PATH"] else {
  exit(1)
}
let image = NSWorkspace.shared.icon(forFile: path)
let sidePixels = 32
let sidePoints: CGFloat = 16
guard let rep = NSBitmapImageRep(
  bitmapDataPlanes: nil, pixelsWide: sidePixels, pixelsHigh: sidePixels,
  bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
  colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
) else {
  exit(1)
}
rep.size = NSSize(width: sidePoints, height: sidePoints)
NSGraphicsContext.saveGraphicsState()
guard let context = NSGraphicsContext(bitmapImageRep: rep) else {
  NSGraphicsContext.restoreGraphicsState()
  exit(1)
}
context.imageInterpolation = .high
NSGraphicsContext.current = context
image.draw(
  in: NSRect(x: 0, y: 0, width: sidePoints, height: sidePoints),
  from: .zero, operation: .copy, fraction: 1.0
)
NSGraphicsContext.restoreGraphicsState()
if let png = rep.representation(using: .png, properties: [:]) {
  FileHandle.standardOutput.write(png)
} else {
  exit(1)
}
"#;

    // Serialize spawns: each `swift -e` is a full compiler run, and a screen of
    // .app results would otherwise launch dozens of them at once.
    static SPAWN_GUARD: Mutex<()> = Mutex::new(());
    let _guard = SPAWN_GUARD.lock();
    run_swift_png(script, Some(("EVERYTHING_ICON_PATH", path)))
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

#[cfg(target_os = "macos")]
fn is_per_file_icon_ext(ext: &str) -> bool {
    // .app bundles carry their own icon; everything else is fine per-extension.
    ext == "app"
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_per_file_icon_ext(_ext: &str) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn load_icon_from_path(path: &str, _ext: &str) -> Option<Vec<u8>> {
    win::icon::load_icon_png(path)
}

#[cfg(target_os = "macos")]
fn load_icon_from_path(path: &str, ext: &str) -> Option<Vec<u8>> {
    if is_per_file_icon_ext(ext) {
        load_path_icon_png(path)
    } else {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn load_icon_from_path(_path: &str, _ext: &str) -> Option<Vec<u8>> {
    None
}

#[tauri::command]
fn get_index_status(state: State<'_, AppState>) -> IndexStatusDto {
    let started = Instant::now();
    let snapshot = state.status.lock().clone();
    let snapshot_state = snapshot.state.as_str().to_string();
    let db_ready = state.db_ready.load(AtomicOrdering::Acquire);
    let indexing_active = state.indexing_active.load(AtomicOrdering::Acquire);
    let state_label = if matches!(snapshot.state, IndexState::Error) {
        snapshot.state.as_str().to_string()
    } else if matches!(snapshot.state, IndexState::Ready) {
        "Ready".to_string()
    } else if indexing_active || !db_ready {
        "Indexing".to_string()
    } else {
        snapshot.state.as_str().to_string()
    };
    let dto = IndexStatusDto {
        state: state_label,
        entries_count: snapshot.entries_count,
        last_updated: snapshot.last_updated,
        permission_errors: snapshot.permission_errors,
        message: snapshot.message,
        scanned: snapshot.scanned,
        indexed: snapshot.indexed,
        current_path: snapshot.current_path,
        background_active: indexing_active,
    };
    if cfg!(debug_assertions) {
        eprintln!(
            "[rpc/get_index_status] elapsed={}ms state={} snapshot_state={} db_ready={} indexing_active={} entries={} scanned={} indexed={}",
            started.elapsed().as_millis(),
            dto.state,
            snapshot_state,
            db_ready,
            indexing_active,
            dto.entries_count,
            dto.scanned,
            dto.indexed,
        );
    }
    dto
}

#[tauri::command]
fn get_home_dir(state: State<'_, AppState>) -> String {
    state.home_dir.to_string_lossy().to_string()
}

#[tauri::command]
fn check_full_disk_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::fs::metadata("/Library/Application Support/com.apple.TCC/TCC.db").is_ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[tauri::command]
fn open_privacy_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .spawn();
    }
}

#[tauri::command]
fn open_pathignore(state: State<'_, AppState>) -> AppResult<()> {
    let path = &state.config_file_path;
    ensure_pathignore_exists(path)?;
    #[cfg(target_os = "macos")]
    Command::new("open").arg(path).spawn().map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    Command::new("cmd")
        .args(["/C", "start", "", &path.to_string_lossy().to_string()])
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = path;
    Ok(())
}

#[tauri::command]
fn open_pathindexing(state: State<'_, AppState>) -> AppResult<()> {
    pathindexing::open_pathindexing_file(&state.pathindexing_file_path)
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
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

        // Fast reset: rename entries (O(1), no data movement) → create new empty table
        // → FTS rebuild from empty content table (instant) → background DROP old table.
        // This avoids per-row WAL writes that make DELETE FROM entries slow at scale.
        let _ = set_meta(&conn, "fts_dirty", "1");
        let _ = conn.execute_batch(DROP_FTS_TRIGGERS_SQL);
        let _ = conn.execute_batch("DROP TABLE IF EXISTS entries_gc_reset;");
        conn.execute_batch("ALTER TABLE entries RENAME TO entries_gc_reset;")
            .map_err(|e| e.to_string())?;
        conn.execute_batch(CREATE_ENTRIES_TABLE_SQL).map_err(|e| e.to_string())?;
        conn.execute_batch(CREATE_FTS_TRIGGERS_SQL).map_err(|e| e.to_string())?;
        let _ = conn.execute_batch(REBUILD_FTS_SQL);

        // entries_gc_reset will be dropped by the GC cleanup in the finalizing thread
        // after indexing completes — avoids a race between DROP TABLE and the new indexer
        // both competing for the SQLite WAL write lock.

        // Clears every meta row (last_run_id, cached counts, and the fts_dirty=1
        // set above) so the follow-up index runs fresh with a clean FTS.
        conn.execute("DELETE FROM meta", [])
            .map_err(|e| e.to_string())?;

        {
            let mut status = state.status.lock();
            status.state = IndexState::Indexing;
            status.entries_count = 0;
            status.last_updated = None;
            status.permission_errors = 0;
            status.message = None;
            status.scanned = 0;
            status.indexed = 0;
            status.current_path.clear();
        }

        invalidate_search_caches(&state);
        // Schema was swapped (entries renamed + recreated): drop pooled search
        // connections so nothing holds statements against the old table.
        state.search_conn_pool.lock().clear();

        emit_index_state(&app, "Indexing", None);
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
            start_full_index_worker(app.clone(), state.clone())?;
            start_fsevent_watcher_worker(app, state, None, false);
            Ok(())
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
/// Returns `None` when the total is unknown or intentionally skipped.
fn compute_total_count(state: &AppState, execution: &SearchExecution) -> Option<u32> {
    let home_dir = &state.home_dir;
    // Read once here, as `execute_search` does independently — the count path
    // and the result path each snapshot `fts_ready` after the search runs.
    let fts_ready = state.fts_ready.load(AtomicOrdering::Acquire);
    // When fewer results than the limit were returned the total is exact.
    if (execution.results.len() as u32) < execution.effective_limit {
        return Some(
            execution
                .offset
                .saturating_add(execution.results.len() as u32),
        );
    }
    // For paginated pages after the first we leave total tracking to the frontend.
    if execution.offset > 0 {
        return None;
    }
    // Non-SQL fast paths already return all matching entries.
    if execution.mode_label.starts_with("mem_")
        || execution.mode_label == "spotlight"
        || execution.mode_label == "spotlight_timeout"
        || execution.mode_label == "find_fallback"
        || execution.mode_label == "name_neg_cache"
    {
        return Some(execution.results.len() as u32);
    }
    let Ok(conn) = pooled_search_connection(state) else {
        return None;
    };
    // Counting matches from the trigram postings alone: joining entries just to
    // count doubles the work, and the FTS index is authoritative while in sync.
    let fts_only_count = |query: &str| -> u32 {
        conn.query_row(
            "SELECT COUNT(*) FROM entries_fts WHERE entries_fts MATCH ?1",
            params![fts_phrase(query)],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    let mode = parse_query(&execution.query);
    let total = match mode {
        SearchMode::Empty => conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap_or(0),
        SearchMode::NameSearch { name_like } => {
            let fts_ok = fts_ready && execution.query.chars().count() >= 3;
            if execution.sort_by != "name" && fts_ok {
                fts_only_count(&execution.query)
            } else {
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
                } else if fts_ok {
                    // Phase-2 contains fallback was served by FTS.
                    fts_only_count(&execution.query)
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
        }
        SearchMode::GlobName { name_like } => {
            let fts_prefilter = glob_fts_prefilter(fts_ready, &name_like, &execution.query);
            if let Some(match_expr) = fts_prefilter {
                conn.query_row(
                    "SELECT COUNT(*) FROM entries_fts f JOIN entries e ON e.id = f.rowid \
                     WHERE entries_fts MATCH ?1 AND e.name LIKE ?2 ESCAPE '\\'",
                    params![match_expr, name_like],
                    |r| r.get(0),
                )
                .unwrap_or(0)
            } else {
                conn.query_row(
                    "SELECT COUNT(*) FROM entries WHERE name LIKE ?1 ESCAPE '\\'",
                    params![name_like],
                    |r| r.get(0),
                )
                .unwrap_or(0)
            }
        }
        SearchMode::ExtSearch { ext, .. } => conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE ext = ?1",
                params![ext],
                |r| r.get(0),
            )
            .unwrap_or(0),
        SearchMode::PathSearch {
            name_like,
            dir_hint,
            ..
        } => {
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
                let sql =
                    format!("SELECT COUNT(*) FROM entries e WHERE ({dir_where}){name_filter}");
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
    };

    Some(total)
}

/// Core DB search shared by the Tauri `search` command and the MCP server
/// (`mcp_server::run_stdio_server`): dispatches the parsed `mode` to the
/// mode-specific SQL against `conn`. Takes no AppState so it can run against a
/// bare read connection when the app itself is not running.
#[allow(clippy::too_many_arguments)]
fn run_db_search(
    conn: &Connection,
    home_dir: &Path,
    fts_ready: bool,
    mode: &SearchMode,
    query: &str,
    effective_limit: u32,
    offset: u32,
    sort_by: &str,
    sort_dir: &str,
) -> AppResult<Vec<EntryDto>> {
    let order_by = sort_clause(sort_by, sort_dir, "e.");
    let mut results = Vec::with_capacity(effective_limit as usize);
    match mode {
        SearchMode::Empty => {
            let sql = format!(
                r#"
                SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                FROM entries e
                ORDER BY {order_by}
                LIMIT ?1 OFFSET ?2
                "#,
            );
            let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![effective_limit, offset], row_to_entry)
                .map_err(|e| e.to_string())?;
            for row in rows {
                results.push(row.map_err(|e| e.to_string())?);
            }
        }

        SearchMode::NameSearch { name_like } => {
            if sort_by != "name" && query.chars().count() >= 3 && fts_ready {
                // Non-name sort: use FTS5 trigram for globally correct ordering.
                // The 3-phase approach only returns prefix matches for non-empty
                // prefix results, causing contains matches to be silently excluded
                // (e.g. a large file "myapp_foo.zip" missing from size-desc results).
                // FTS5 trigram index covers all substring matches in one indexed pass.
                let fts_match = fts_phrase(query);
                let sql = format!(
                    r#"
                    SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                    FROM entries_fts f
                    JOIN entries e ON e.id = f.rowid
                    WHERE entries_fts MATCH ?1
                    ORDER BY {order_by}
                    LIMIT ?2 OFFSET ?3
                    "#,
                );
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![fts_match, effective_limit, offset], row_to_entry)
                    .map_err(|e| e.to_string())?;
                for row in rows {
                    results.push(row.map_err(|e| e.to_string())?);
                }
            } else {

            let escaped_query = escape_like(query);
            let exact_query = query.to_string();
            let prefix_like = format!("{}%", escaped_query);
            let bare_order = sort_clause(sort_by, sort_dir, "");

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
                let mut stmt = conn.prepare_cached(&exact_sql).map_err(|e| e.to_string())?;
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

                // Try indexed prefix search first; fall back to unindexed if
                // the index is temporarily unavailable (during background DB
                // rebuild the index may be dropped then recreated).
                let prefix_sql_indexed = format!(
                    r#"
                    SELECT path, name, dir, is_dir, ext, size, mtime
                    FROM entries INDEXED BY idx_entries_name_nocase
                    WHERE name LIKE ?1 ESCAPE '\'
                      AND name COLLATE NOCASE != ?2
                    ORDER BY {bare_order}
                    LIMIT ?3 OFFSET ?4
                    "#,
                );
                let prefix_sql_fallback = format!(
                    r#"
                    SELECT path, name, dir, is_dir, ext, size, mtime
                    FROM entries
                    WHERE name LIKE ?1 ESCAPE '\'
                      AND name COLLATE NOCASE != ?2
                    ORDER BY {bare_order}
                    LIMIT ?3 OFFSET ?4
                    "#,
                );
                let mut stmt = match conn.prepare_cached(&prefix_sql_indexed) {
                    Ok(s) => s,
                    Err(_) => {
                        eprintln!("[search] idx_entries_name_nocase unavailable, using fallback");
                        conn.prepare_cached(&prefix_sql_fallback).map_err(|e| e.to_string())?
                    }
                };
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

            if results.is_empty() && fts_ready && query.chars().count() >= 3 {
                // Phase 2: contains-match via FTS5 trigram — indexed, complete
                // (no time budget), and paginatable. Reached only when exact and
                // prefix found nothing, so no exclusion predicates are needed.
                // For offset>0 the page belongs to the contains result set only
                // when the query has no prefix matches at all (guard below).
                let serve_contains_page = if offset == 0 {
                    true
                } else {
                    !conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM entries WHERE name LIKE ?1 ESCAPE '\\')",
                            params![prefix_like],
                            |r| r.get::<_, bool>(0),
                        )
                        .unwrap_or(true)
                };
                if serve_contains_page {
                    let fts_match = fts_phrase(query);
                    let phase2_sql = format!(
                        r#"
                        SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                        FROM entries_fts f
                        JOIN entries e ON e.id = f.rowid
                        WHERE entries_fts MATCH ?1
                        ORDER BY {order_by}
                        LIMIT ?2 OFFSET ?3
                        "#,
                    );
                    let mut stmt2 =
                        conn.prepare_cached(&phase2_sql).map_err(|e| e.to_string())?;
                    let rows2 = stmt2
                        .query_map(params![fts_match, effective_limit, offset], row_to_entry)
                        .map_err(|e| e.to_string())?;
                    for row in rows2 {
                        results.push(row.map_err(|e| e.to_string())?);
                    }
                }
            } else if results.is_empty() && offset == 0 {
                // Phase 2 fallback (query < 3 chars or FTS rebuilding):
                // contains-match (LIKE '%q%') with tight time budget.
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

            } // end sort_by == "name" branch
        }

        SearchMode::GlobName { name_like } => {
            // Leading-wildcard patterns can't use the name index; narrow with the
            // FTS trigram index on literal runs first, then verify the full glob
            // with LIKE. Prefix-shaped patterns keep the plain LIKE (index range).
            let fts_prefilter = glob_fts_prefilter(fts_ready, name_like, query);
            if let Some(match_expr) = fts_prefilter {
                let sql = format!(
                    r#"
                    SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                    FROM entries_fts f
                    JOIN entries e ON e.id = f.rowid
                    WHERE entries_fts MATCH ?1
                      AND e.name LIKE ?2 ESCAPE '\'
                    ORDER BY {order_by}
                    LIMIT ?3 OFFSET ?4
                    "#,
                );
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(
                        params![match_expr, name_like, effective_limit, offset],
                        row_to_entry,
                    )
                    .map_err(|e| e.to_string())?;
                for row in rows {
                    results.push(row.map_err(|e| e.to_string())?);
                }
            } else {
                let sql = format!(
                    r#"
                    SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.size, e.mtime
                    FROM entries e
                    WHERE e.name LIKE ?1 ESCAPE '\'
                    ORDER BY {order_by}
                    LIMIT ?2 OFFSET ?3
                    "#,
                );
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![name_like, effective_limit, offset], row_to_entry)
                    .map_err(|e| e.to_string())?;
                for row in rows {
                    results.push(row.map_err(|e| e.to_string())?);
                }
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
            let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
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
            let resolved_dirs: Vec<String> = resolve_dir_hint(home_dir, dir_hint)
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
                // Dynamic shape (one condition per resolved dir): not worth caching.
                let mut stmt = conn.prepare(sql.as_str()).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params_from_iter(sql_params.iter()), row_to_entry)
                    .map_err(|e| e.to_string())?;
                for row in rows {
                    results.push(row.map_err(|e| e.to_string())?);
                }
            } else {
                let sep = std::path::MAIN_SEPARATOR;
                let sep_str = sep.to_string();
                let escaped_sep = escape_like(&sep_str);
                let (native_hint, is_absolute) = normalize_hint_to_native(dir_hint);
                let dir_suffix = escape_like(&native_hint);
                let dir_prefix = if is_absolute {
                    String::new()
                } else {
                    escaped_sep.clone()
                };
                let dir_like_exact = format!("%{dir_prefix}{dir_suffix}");
                let dir_like_sub = format!("%{dir_prefix}{dir_suffix}{escaped_sep}%");
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
                    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
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
                    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
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
                            if let Ok(mut stmt) = conn.prepare_cached(&sql) {
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
                        if let Ok(mut stmt) = conn.prepare_cached(&sql) {
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
    Ok(results)
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
    let effective_limit = effective_search_limit(&query, limit, DEFAULT_LIMIT);
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
    let fts_ready = state.fts_ready.load(AtomicOrdering::Acquire);

    // Only a placeholder for the DB-unavailable path; every successful path
    // wholly reassigns it (run_db_search owns the real allocation).
    let mut results = Vec::new();
    let mode = parse_query(&query);
    let is_name_mode = matches!(&mode, SearchMode::NameSearch { .. });
    let allow_find_fallback = !is_indexing
        && matches!(
            &mode,
            SearchMode::GlobName { .. } | SearchMode::ExtSearch { .. }
        );
    let mut mode_label = mode.label().to_string();

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
    match pooled_search_connection(state) {
        Ok(conn) => {
            #[cfg(target_os = "macos")]
            { db_unavailable = false; }
            results = run_db_search(
                &conn,
                &state.home_dir,
                fts_ready,
                &mode,
                &query,
                effective_limit,
                offset,
                &sort_by,
                &sort_dir,
            )?;

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
    include_total: Option<bool>,
    state: State<'_, AppState>,
) -> AppResult<SearchResultDto> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let rpc_started = Instant::now();
        let execute_started = Instant::now();
        let execution = execute_search(&state, query, limit, offset, sort_by, sort_dir)?;
        let execute_elapsed_ms = execute_started.elapsed().as_secs_f64() * 1000.0;

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
                execute_elapsed_ms,
                top,
            ));
        }

        let include_total = include_total.unwrap_or(true);
        let count_started = Instant::now();
        let (total_count, total_known) = if include_total {
            match compute_total_count(&state, &execution) {
                Some(v) => (v, true),
                None => (0, false),
            }
        } else {
            (0, false)
        };
        let count_elapsed_ms = if include_total {
            count_started.elapsed().as_secs_f64() * 1000.0
        } else {
            0.0
        };

        let rpc_elapsed_ms = rpc_started.elapsed().as_secs_f64() * 1000.0;
        if cfg!(debug_assertions) {
            let db_ready = state.db_ready.load(AtomicOrdering::Acquire);
            let indexing_active = state.indexing_active.load(AtomicOrdering::Acquire);
            eprintln!(
                "[rpc/search] total={:.3}ms execute={:.3}ms total_count={:.3}ms include_total={} query={:?} mode={} sort={}/{} limit={} offset={} results={} total_count={} total_known={} db_ready={} indexing_active={}",
                rpc_elapsed_ms,
                execute_elapsed_ms,
                count_elapsed_ms,
                include_total,
                execution.query,
                execution.mode_label,
                execution.sort_by,
                execution.sort_dir,
                execution.effective_limit,
                execution.offset,
                execution.results.len(),
                total_count,
                total_known,
                db_ready,
                indexing_active,
            );
        }
        Ok(SearchResultDto {
            entries: execution.results,
            mode_label: execution.mode_label,
            total_count,
            total_known,
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
                    // kLSApplicationNotFoundErr: no app claims this file type.
                    // Hand the open to Finder, which shows the system
                    // "no application set to open" chooser dialog.
                    if stderr.contains("kLSApplicationNotFoundErr") || stderr.contains("-10814") {
                        let fallback = Command::new("open")
                            .args(["-a", "Finder", path])
                            .status()
                            .map_err(|e| e.to_string())?;
                        if fallback.success() {
                            continue;
                        }
                    }
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

/// Directory extensions Finder presents as packages (bundles). Gates the
/// "Show Package Contents" context-menu item.
#[cfg(target_os = "macos")]
const PACKAGE_EXTENSIONS: &[&str] = &[
    "app",
    "bundle",
    "framework",
    "plugin",
    "kext",
    "prefpane",
    "appex",
    "xpc",
    "qlgenerator",
    "xcodeproj",
    "photoslibrary",
];

#[cfg(target_os = "macos")]
fn is_macos_package(path: &str) -> bool {
    let p = Path::new(path);
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PACKAGE_EXTENSIONS.iter().any(|pkg| pkg.eq_ignore_ascii_case(e)))
        && p.is_dir()
}

/// Finder-style "Show Package Contents": browse a package directory (e.g. an
/// .app bundle) as a folder. Plain `open` would launch the bundle and Finder
/// rejects the `folder` coercion for packages, so a new Finder window is
/// pointed at the package root instead; the path travels via argv to avoid
/// AppleScript string escaping.
#[cfg(target_os = "macos")]
#[tauri::command]
async fn show_package_contents(path: String) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || {
        let status = Command::new("osascript")
            .args([
                "-e", "on run argv",
                "-e", "tell application \"Finder\"",
                "-e", "set w to make new Finder window",
                "-e", "set target of w to (POSIX file (item 1 of argv) as alias)",
                "-e", "activate",
                "-e", "end tell",
                "-e", "end run",
                &path,
            ])
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("Failed to show package contents: {path}"));
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
async fn show_package_contents(_path: String) -> AppResult<()> {
    Err("show_package_contents is only supported on macOS".to_string())
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
            let _ = rescan::rescan_subtree(
                &mut conn,
                &new_path,
                &state.path_ignores,
                &state.path_ignore_patterns,
            )?;
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

        {
            let cache = state.fd_search_cache.lock();
            if let Some(cached) = cache.as_ref() {
                let cache_hit = cached.query == query
                    && cached.sort_by == sort_by
                    && cached.sort_dir == sort_dir
                    && cached.ignore_fingerprint == ignore_fingerprint;
                if cache_hit {
                    let total = cached.entries.len() as u64;
                    let end = (offset + limit).min(cached.entries.len());
                    let page = if offset < cached.entries.len() {
                        cached.entries[offset..end].to_vec()
                    } else {
                        Vec::new()
                    };
                    return Ok(FdSearchResultDto {
                        entries: page,
                        total,
                        timed_out: false,
                    });
                }
            }
        }

        let result = fd_search::run_fd_search(
            &state.scan_root,
            &runtime_ignored_roots,
            &runtime_ignored_patterns,
            &query,
            &sort_by,
            &sort_dir,
        );
        let total = result.entries.len() as u64;
        let end = (offset + limit).min(result.entries.len());
        let page = if offset < result.entries.len() {
            result.entries[offset..end].to_vec()
        } else {
            Vec::new()
        };

        {
            let mut cache = state.fd_search_cache.lock();
            *cache = Some(FdSearchCache {
                query: query.clone(),
                sort_by: sort_by.clone(),
                sort_dir: sort_dir.clone(),
                ignore_fingerprint,
                entries: result.entries,
            });
        }

        Ok(FdSearchResultDto {
            entries: page,
            total,
            timed_out: result.timed_out,
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
fn mark_frontend_ready(state: State<'_, AppState>) {
    state.frontend_ready.store(true, AtomicOrdering::Release);
    if cfg!(debug_assertions) {
        eprintln!("[startup] frontend_ready=true");
    }
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
    paths: Vec<String>,
    x: f64,
    y: f64,
    single_selection: bool,
    app: AppHandle,
) -> AppResult<()> {
    use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem};

    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;

    let show_package = single_selection
        && paths.first().map(|p| is_macos_package(p)).unwrap_or(false);

    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

    let app_clone = app.clone();
    app.run_on_main_thread(move || {
        let app = app_clone;
        let result = (|| -> Result<(), tauri::Error> {
            let open = MenuItem::with_id(&app, "ctx_open", "Open", true, None::<&str>)?;
            let show_pkg = MenuItem::with_id(
                &app,
                "ctx_show_package_contents",
                "Show Package Contents",
                true,
                None::<&str>,
            )?;
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
            if show_package {
                // Finder places "Show Package Contents" directly after "Open".
                items.insert(1, &show_pkg);
            }
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
async fn set_native_theme(theme: String, app: AppHandle) -> AppResult<()> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "Main window not found".to_string())?;
    let is_light = theme == "light";
    // Follow the OS appearance for the native window. Forcing a theme here
    // would pin the webview's prefers-color-scheme, so the frontend's system
    // theme listener would never fire again. Only the window background color
    // (anti-flash on resize) tracks the theme the frontend reports.
    window.set_theme(None).map_err(|e| e.to_string())?;
    use tauri::window::Color;
    let bg = if is_light {
        Color(0xF5, 0xF5, 0xF7, 0xFF)
    } else {
        Color(0x1E, 0x1E, 0x21, 0xFF)
    };
    let _ = window.set_background_color(Some(bg));
    Ok(())
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

        let cache_key = if is_per_file_icon_ext(&ext_lower) {
            path.as_deref().unwrap_or(&ext_lower).to_string()
        } else {
            ext_lower.clone()
        };

        if let Some(cached) = state.icon_cache.lock().get(&cache_key).cloned() {
            return cached;
        }

        let icon = path
            .as_deref()
            .filter(|p| !p.is_empty())
            .and_then(|p| load_icon_from_path(p, &ext_lower))
            .or_else(|| {
                // Per-path miss: serve the generic ext icon from the ext-keyed
                // cache (prewarmed for common exts) instead of regenerating it
                // for every path, and store it back under the ext key.
                if cache_key != ext_lower {
                    if let Some(cached) = state.icon_cache.lock().get(&ext_lower).cloned() {
                        return Some(cached);
                    }
                }
                let system = load_system_icon_png(&ext_lower);
                if cache_key != ext_lower {
                    if let Some(png) = &system {
                        state.icon_cache.lock().insert(ext_lower.clone(), png.clone());
                    }
                }
                system
            });

        let icon = icon.unwrap_or_default();
        // Don't cache failures: a transient miss (e.g. an .app bundle still being
        // written) would otherwise block every future retry for this key.
        if !icon.is_empty() {
            state.icon_cache.lock().insert(cache_key, icon.clone());
        }
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

fn setup_app(app: &mut tauri::App) -> AppResult<()> {
    let setup_started = std::time::Instant::now();
    eprintln!("[startup] setup_app() entered");
    let bench_mode = bench_mode_enabled();

    #[cfg(target_os = "macos")]
    if let Some(window) = app.get_webview_window("main") {
        use tauri::window::Color;
        let is_dark = window
            .theme()
            .map(|t| t == tauri::Theme::Dark)
            .unwrap_or(false);
        let bg = if is_dark {
            Color(0x1E, 0x1E, 0x21, 0xFF)
        } else {
            Color(0xF5, 0xF5, 0xF7, 0xFF)
        };
        let _ = window.set_background_color(Some(bg));
        let _ = window.show();
    }

    #[cfg(target_os = "windows")]
    if let Some(window) = app.get_webview_window("main") {
        eprintln!("[startup +{}ms] windows setup: before window.show()", startup_elapsed_ms());
        use tauri::window::Color;
        let is_dark = window.theme().map(|t| t == tauri::Theme::Dark).unwrap_or(false);
        let bg = if is_dark {
            Color(0x1E, 0x1E, 0x21, 0xFF)
        } else {
            Color(0xF5, 0xF5, 0xF7, 0xFF)
        };
        let _ = window.set_background_color(Some(bg));
        if let Err(e) = window.set_decorations(false) {
            eprintln!(
                "[startup +{}ms] windows setup: set_decorations(false) failed: {}",
                startup_elapsed_ms(),
                e
            );
        }
        let _ = window.show();
        eprintln!("[startup +{}ms] windows setup: window.show() returned", startup_elapsed_ms());
    }

    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {e}"))?;
    fs::create_dir_all(&app_data_dir).map_err(|e| e.to_string())?;

    let db_path = app_data_dir.join(DB_FILE_NAME);
    let home_dir = resolve_home_dir();

    // Register this binary as an MCP server for Claude Code / Codex so agents
    // pick it up automatically. Passes the Tauri-resolved DB path so the
    // standalone MCP process serves exactly this index instead of guessing
    // the app-data layout. Best-effort, off the startup path.
    {
        let mcp_db_path = db_path.clone();
        std::thread::spawn(move || mcp_server::register_all_and_log(Some(mcp_db_path)));
    }
    let scan_root = if cfg!(windows) {
        PathBuf::from("C:\\")
    } else {
        home_dir.clone()
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| home_dir.clone());
    let config_file_path = app_data_dir.join(".pathignore");
    eprintln!("[startup] +{}ms loading pathignore rules...", setup_started.elapsed().as_millis());
    let (mut path_ignores, path_ignore_patterns) = load_pathignore_rules(&config_file_path, &home_dir, &cwd);
    if !path_ignores.iter().any(|r| r == &app_data_dir) {
        path_ignores.push(app_data_dir.clone());
    }
    eprintln!("[startup] +{}ms pathignore done", setup_started.elapsed().as_millis());

    let pathindexing_file_path = app_data_dir.join(".pathindexing");
    let extra_roots = pathindexing::load_pathindexing_roots(&pathindexing_file_path);
    eprintln!("[startup] +{}ms pathindexing loaded: {} extra roots", setup_started.elapsed().as_millis(), extra_roots.len());

    let state = AppState {
        db_path,
        home_dir,
        scan_root,
        cwd,
        config_file_path,
        pathindexing_file_path,
        extra_roots: Arc::new(Mutex::new(extra_roots)),
        path_ignores: Arc::new(path_ignores),
        path_ignore_patterns: Arc::new(path_ignore_patterns),
        db_ready: Arc::new(AtomicBool::new(false)),
        indexing_active: Arc::new(AtomicBool::new(false)),
        status: Arc::new(Mutex::new(IndexStatus::default())),
        recent_ops: Arc::new(Mutex::new(Vec::new())),
        icon_cache: Arc::new(Mutex::new(HashMap::new())),
        fd_search_cache: Arc::new(Mutex::new(None)),
        negative_name_cache: Arc::new(Mutex::new(HashMap::new())),
        ignore_cache: Arc::new(Mutex::new(None)),
        fts_ready: Arc::new(AtomicBool::new(true)),
        mem_index: Arc::new(RwLock::new(None)),
        watcher_stop: Arc::new(AtomicBool::new(false)),
        watcher_active: Arc::new(AtomicBool::new(false)),
        frontend_ready: Arc::new(AtomicBool::new(false)),
        pathindexing_active: Arc::new(AtomicBool::new(false)),
        search_conn_pool: Arc::new(Mutex::new(Vec::new())),
        watcher_conn: Arc::new(Mutex::new(None)),
    };

    eprintln!("[startup] +{}ms AppState created", setup_started.elapsed().as_millis());
    app.manage(state.clone());
    // Context menu item IDs use the "ctx_" prefix by convention.
    // All matching IDs are forwarded as "context_menu_action" events to the frontend.
    #[cfg(target_os = "macos")]
    {
        app.handle().on_menu_event(|app, event| {
            let action = match event.id().as_ref() {
                "ctx_open" => "open",
                "ctx_show_package_contents" => "show_package_contents",
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

        eprintln!("[startup/thread] +{}ms calling init_db_tables...", thread_started.elapsed().as_millis());
        if let Err(err) = init_db_tables(&state.db_path) {
            set_state(&state, IndexState::Error, Some(err.clone()));
            emit_index_state(&app_handle, "Error", Some(err));
            return;
        }
        eprintln!("[startup/thread] +{}ms init_db_tables done", thread_started.elapsed().as_millis());

        state.db_ready.store(true, AtomicOrdering::Release);
        eprintln!("[startup/thread] +{}ms db_ready=true -- launching indexing immediately", thread_started.elapsed().as_millis());

        // If a previous run crashed mid FTS rebuild, don't trust the FTS index
        // until the healing rebuild (finalize_fresh_index) completes.
        if let Ok(c) = db_connection(&state.db_path) {
            if !fts_usable(&c) {
                eprintln!("[startup/thread] FTS dirty or missing -- disabled until rebuild");
                state.fts_ready.store(false, AtomicOrdering::Release);
            }
        }

        // Deferred housekeeping -- purge + status counts run in background
        {
            let hk_app = app_handle.clone();
            let hk_state = state.clone();
            std::thread::spawn(move || {
                // Brief pause so indexing thread can start first
                std::thread::sleep(std::time::Duration::from_millis(500));
                let hk_started = std::time::Instant::now();
                let _ = refresh_and_emit_status_counts(&hk_app, &hk_state);
                eprintln!("[startup/housekeeping] refresh_and_emit_status_counts done in {}ms", hk_started.elapsed().as_millis());
                // Wait for frontend readiness so initial paint and first input are not contended by purge I/O.
                const FRONTEND_READY_TIMEOUT: Duration = Duration::from_secs(180);
                const POST_READY_GRACE: Duration = Duration::from_secs(2);
                let wait_started = std::time::Instant::now();
                while !hk_state.frontend_ready.load(AtomicOrdering::Acquire)
                    && wait_started.elapsed() < FRONTEND_READY_TIMEOUT
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                if hk_state.frontend_ready.load(AtomicOrdering::Acquire) {
                    eprintln!(
                        "[startup/housekeeping] frontend_ready observed after {}ms; purging in {}ms",
                        wait_started.elapsed().as_millis(),
                        POST_READY_GRACE.as_millis()
                    );
                } else {
                    eprintln!(
                        "[startup/housekeeping] frontend_ready wait timed out after {}ms; proceeding with purge",
                        FRONTEND_READY_TIMEOUT.as_millis()
                    );
                }
                std::thread::sleep(POST_READY_GRACE);
                let purge_started = std::time::Instant::now();
                if let Err(err) = purge_ignored_entries(&hk_state.db_path, &hk_state.path_ignores) {
                    eprintln!("[startup/housekeeping] purge_ignored_entries failed: {err}");
                } else {
                    eprintln!(
                        "[startup/housekeeping] purge_ignored_entries done in {}ms",
                        purge_started.elapsed().as_millis()
                    );
                }
                eprintln!("[startup/housekeeping] all done in {}ms", hk_started.elapsed().as_millis());
            });
        }

        #[cfg(target_os = "macos")]
        {
            if bench_mode {
                let _ = start_full_index_worker(app_handle.clone(), state.clone());
            } else {
                let (stored_event_id, index_complete, cached_count, cached_updated) =
                    db_connection(&state.db_path)
                        .ok()
                        .map(|c| {
                            let eid = get_meta(&c, "last_event_id")
                                .and_then(|v| v.parse::<u64>().ok());
                            let complete = get_meta(&c, "index_complete")
                                .map(|v| v == "1")
                                .unwrap_or(false);
                            let (count, updated) = load_cached_counts(&c);
                            (eid, complete, count, updated)
                        })
                        .unwrap_or((None, false, None, None));

                let entries_empty = db_connection(&state.db_path)
                    .ok()
                    .map(|c| {
                        c.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get::<_, i64>(0))
                            .unwrap_or(0) == 0
                    })
                    .unwrap_or(true);

                let effective_complete = index_complete && !entries_empty;

                if stored_event_id.is_some() && effective_complete {
                    // Seed status counts from the cached meta (read above on
                    // the same connection) before the watcher starts, so its
                    // incremental count updates build on the last known total
                    // instead of 0 (the HistoryDone full recount later corrects
                    // any replay drift).
                    if let Some(count) = cached_count {
                        let mut status = state.status.lock();
                        status.entries_count = count;
                        status.last_updated = cached_updated;
                    }
                    // Conditional startup: try watcher replay first, skip full scan if OK
                    start_fsevent_watcher_worker(
                        app_handle.clone(),
                        state.clone(),
                        stored_event_id,
                        true,
                    );
                    // This path skips run_incremental_index, whose finalizing
                    // thread is the only other ensure_db_indexes call site —
                    // without this, indexes added to the schema after this DB
                    // was created would never be built (e.g. a missing
                    // idx_entries_indexed_at degrades MAX(indexed_at) to a
                    // full-table scan on every status recount).
                    {
                        let idx_state = state.clone();
                        std::thread::spawn(move || {
                            // Let the startup replay settle first; if replay
                            // escalated to a full scan, wait it out — its
                            // finalizing thread ensures indexes itself and the
                            // CREATE INDEX IF NOT EXISTS here just no-ops.
                            std::thread::sleep(Duration::from_secs(5));
                            while idx_state.indexing_active.load(AtomicOrdering::Acquire) {
                                std::thread::sleep(Duration::from_secs(1));
                            }
                            if let Err(e) = ensure_db_indexes(&idx_state.db_path) {
                                eprintln!("[startup] ensure_db_indexes error: {e}");
                            }
                        });
                    }
                    // Scan any extra roots added to .pathindexing while the app was not running
                    {
                        let scan_state = state.clone();
                        let scan_app = app_handle.clone();
                        std::thread::spawn(move || {
                            let stored_roots: Vec<PathBuf> = db_connection(&scan_state.db_path)
                                .ok()
                                .and_then(|c| get_meta(&c, "indexed_extra_roots"))
                                .map(|v| {
                                    v.lines()
                                        .filter(|l| !l.is_empty())
                                        .map(PathBuf::from)
                                        .collect()
                                })
                                .unwrap_or_default();
                            let current_roots = scan_state.extra_roots.lock().clone();
                            let stored_set: std::collections::HashSet<&PathBuf> = stored_roots.iter().collect();
                            let added: Vec<PathBuf> = current_roots
                                .iter()
                                .filter(|r| !stored_set.contains(r))
                                .cloned()
                                .collect();
                            let removed: Vec<PathBuf> = stored_roots
                                .iter()
                                .filter(|r| !current_roots.contains(r))
                                .cloned()
                                .collect();
                            if added.is_empty() && removed.is_empty() {
                                return;
                            }
                            if scan_state.pathindexing_active.compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire).is_err() {
                                eprintln!("[pathindexing] scan already in progress, skipping startup diff");
                                return;
                            }
                            eprintln!(
                                "[pathindexing] startup diff: +{} added, -{} removed",
                                added.len(),
                                removed.len()
                            );
                            // Wait for initial watcher replay to finish
                            while scan_state.indexing_active.load(std::sync::atomic::Ordering::Acquire) {
                                std::thread::sleep(Duration::from_secs(1));
                            }
                            let (ign_roots, ign_patterns) = cached_effective_ignore_rules(&scan_state);
                            match pathindexing::handle_pathindexing_change(
                                &scan_state, &stored_roots, &current_roots, &ign_roots, &ign_patterns,
                            ) {
                                Ok(()) => {
                                    eprintln!("[pathindexing] startup scan done");
                                    // Update stored roots
                                    if let Ok(c) = db_connection(&scan_state.db_path) {
                                        let roots_str: Vec<String> = current_roots.iter().map(|r| r.to_string_lossy().to_string()).collect();
                                        let _ = set_meta(&c, "indexed_extra_roots", &roots_str.join("\n"));
                                    }
                                    let _ = refresh_and_emit_status_counts(&scan_app, &scan_state);
                                    scan_app.emit("pathindexing_changed", ()).ok();
                                }
                                Err(e) => eprintln!("[pathindexing] startup scan error: {e}"),
                            }
                            scan_state.pathindexing_active.store(false, AtomicOrdering::Release);
                        });
                    }
                } else {
                    if !effective_complete {
                        eprintln!("[mac] index incomplete or entries empty; starting full index");
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
    let _ = STARTUP_T0.set(Instant::now());
    eprintln!("[startup +{}ms] run() entered", startup_elapsed_ms());
    #[cfg(target_os = "windows")]
    if cfg!(debug_assertions) {
        // Safety: called at program start before any threads are spawned.
        // Will require `unsafe` block when migrating to Rust edition 2024.
        std::env::set_var(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            "--no-proxy-server --disable-http-cache --disk-cache-size=1 --media-cache-size=1",
        );
        eprintln!(
            "[startup +{}ms] set WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--no-proxy-server --disable-http-cache --disk-cache-size=1 --media-cache-size=1",
            startup_elapsed_ms()
        );
    }
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_decorum::init())
        .plugin(tauri_plugin_drag::init());

    if !cfg!(debug_assertions) {
        builder = builder.plugin(tauri_plugin_window_state::Builder::default().build());
    } else {
        eprintln!(
            "[startup +{}ms] debug mode: window-state plugin disabled for startup A/B test",
            startup_elapsed_ms()
        );
    }

    builder
        .on_page_load(|window, payload| {
            eprintln!(
                "[startup/page +{}ms] on_page_load label={} event={:?} url={}",
                startup_elapsed_ms(),
                window.label(),
                payload.event(),
                payload.url()
            );
        })
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
            show_package_contents,
            copy_paths,
            copy_files,
            move_to_trash,
            rename,
            get_file_icon,
            get_platform,
            show_context_menu,
            set_native_theme,
            frontend_log,
            mark_frontend_ready,
            check_full_disk_access,
            open_privacy_settings,
            open_pathignore,
            open_pathindexing,
            restart_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    // `--mcp` / `--register-mcp` run headless and must not boot the GUI.
    if mcp_server::handle_cli_args() {
        return;
    }
    run();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(target_os = "macos")]
    #[test]
    fn remap_event_path_maps_canonical_to_stored_prefix() {
        let remaps = vec![(PathBuf::from("/private/tmp"), PathBuf::from("/tmp"))];
        assert_eq!(
            remap_event_path(PathBuf::from("/private/tmp/foo/bar.txt"), &remaps),
            PathBuf::from("/tmp/foo/bar.txt")
        );
        assert_eq!(
            remap_event_path(PathBuf::from("/private/tmp"), &remaps),
            PathBuf::from("/tmp")
        );
        assert_eq!(
            remap_event_path(PathBuf::from("/private/var/x"), &remaps),
            PathBuf::from("/private/var/x"),
            "paths outside any remap prefix must pass through"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn queue_subtree_rescan_collapses_covered_paths() {
        let finished = Mutex::new(Vec::new());
        let mut queued = HashMap::new();
        queue_subtree_rescan(PathBuf::from("/h/a/b"), &mut queued, &finished);
        queue_subtree_rescan(PathBuf::from("/h/a/b/c"), &mut queued, &finished);
        assert_eq!(queued.len(), 1, "descendant is covered by queued ancestor");
        queue_subtree_rescan(PathBuf::from("/h/a"), &mut queued, &finished);
        assert_eq!(queued.len(), 1, "wider rescan replaces queued descendants");
        assert!(queued.contains_key(Path::new("/h/a")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn queue_subtree_rescan_defers_within_cooldown() {
        let finished = Mutex::new(vec![(PathBuf::from("/h/a"), Instant::now())]);
        let mut queued = HashMap::new();
        queue_subtree_rescan(PathBuf::from("/h/a/x"), &mut queued, &finished);
        let due = queued.get(Path::new("/h/a/x")).expect("queued");
        assert!(
            *due > Instant::now() + RESCAN_COOLDOWN - Duration::from_secs(60),
            "overlapping rescan must be deferred past the cooldown window"
        );
    }

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

    fn test_state_for(db_path: PathBuf, home_dir: PathBuf, cwd: PathBuf) -> AppState {
        AppState {
            config_file_path: home_dir.join(".pathignore"),
            pathindexing_file_path: home_dir.join(".pathindexing"),
            extra_roots: Arc::new(Mutex::new(Vec::new())),
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
            negative_name_cache: Arc::new(Mutex::new(HashMap::new())),
            ignore_cache: Arc::new(Mutex::new(None)),
            fts_ready: Arc::new(AtomicBool::new(true)),
            mem_index: Arc::new(RwLock::new(None)),
            watcher_stop: Arc::new(AtomicBool::new(false)),
            watcher_active: Arc::new(AtomicBool::new(false)),
            frontend_ready: Arc::new(AtomicBool::new(true)),
            pathindexing_active: Arc::new(AtomicBool::new(false)),
            search_conn_pool: Arc::new(Mutex::new(Vec::new())),
            watcher_conn: Arc::new(Mutex::new(None)),
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
        let patterns = vec![IgnorePattern::any_segment("target")];

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
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
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

    #[cfg(target_os = "macos")]
    #[test]
    fn apply_path_changes_count_delta_matches_db() {
        let root = temp_case_dir("apply_count_delta");
        let home = root.join("home");
        // Work under a subdirectory: loading ignore rules recreates
        // home/.pathignore (ensure_pathignore_exists), so home itself can
        // never play the "vanished directory" role.
        let docs = home.join("docs");
        fs::create_dir_all(&docs).unwrap();

        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
        let state = test_state_for(db_path.clone(), home.clone(), root.clone());

        let file_a = docs.join("a.txt");
        let file_b = docs.join("b.txt");
        fs::write(&file_a, "a").unwrap();
        fs::write(&file_b, "b").unwrap();

        // Two new files → +2
        let out = apply_path_changes(&state, &[file_a.clone(), file_b.clone()]).unwrap();
        assert_eq!(out.count_delta, 2);
        assert_eq!(out.changed, 2);

        // Re-upserting an existing path → no net change
        let out = apply_path_changes(&state, &[file_a.clone()]).unwrap();
        assert_eq!(out.count_delta, 0);

        // Vanished file → -1
        fs::remove_file(&file_b).unwrap();
        let out = apply_path_changes(&state, &[file_b.clone()]).unwrap();
        assert_eq!(out.count_delta, -1);

        // A vanished directory sweeps its remaining subtree rows: a.txt is
        // still in the DB and goes with the range delete → -1.
        fs::remove_dir_all(&docs).unwrap();
        let out = apply_path_changes(&state, &[docs.clone()]).unwrap();
        assert_eq!(out.count_delta, -1);

        // Accumulated deltas must equal the authoritative COUNT(*)
        let conn = db_connection(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

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
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
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
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
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

        // Query: "jp.naver.line/log/" - dir listing, name_like = "%"
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

        // Query with trailing dot: "jp.naver.line/log/ *." - name_like becomes "%."
        let result3 = execute_search(
            &state,
            "jp.naver.line/log/ *.".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();

        // "*." matches files ending with "." - none of our test files end with "."
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
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
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

    #[test]
    fn fts_phrase_quotes_and_escapes() {
        assert_eq!(fts_phrase("test"), "\"test\"");
        assert_eq!(fts_phrase("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn glob_fts_match_expr_extracts_literal_runs() {
        assert_eq!(
            glob_fts_match_expr("*test*.js"),
            Some("\"test\" AND \".js\"".to_string())
        );
        assert_eq!(glob_fts_match_expr("icon*"), Some("\"icon\"".to_string()));
        // runs shorter than a trigram are dropped
        assert_eq!(glob_fts_match_expr("*ab*cd*"), None);
        assert_eq!(glob_fts_match_expr("a?b"), None);
        // ? splits runs like * (".md" is 3 chars, kept)
        assert_eq!(
            glob_fts_match_expr("spec?.md"),
            Some("\"spec\" AND \".md\"".to_string())
        );
        assert_eq!(glob_fts_match_expr("***"), None);
    }

    #[test]
    fn like_pattern_unindexable_detects_leading_wildcard() {
        assert!(like_pattern_unindexable("%test%.js"));
        assert!(like_pattern_unindexable("_est"));
        assert!(!like_pattern_unindexable("icon%"));
        assert!(!like_pattern_unindexable("te_st%"));
    }

    /// Contains-search via FTS phase 2 must return complete results and support
    /// pagination (offset > 0) when the query has no prefix matches.
    #[test]
    fn execute_search_fts_contains_returns_and_paginates() {
        let root = temp_case_dir("fts_contains_paginate");
        fs::create_dir_all(&root).unwrap();
        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        let now = now_epoch();
        // Names that CONTAIN "zzfrag" but never start with it.
        for i in 0..10 {
            let name = format!("file_zzfrag_{i:02}.txt");
            let path = root.join(&name);
            conn.execute(
                "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
                 VALUES(?1, ?2, ?3, 0, 'txt', NULL, NULL, ?4, 1)",
                params![
                    path.to_string_lossy().to_string(),
                    name,
                    root.to_string_lossy().to_string(),
                    now
                ],
            )
            .unwrap();
        }
        drop(conn);

        let state = test_state_for(db_path.clone(), root.clone(), root.clone());
        state.status.lock().state = IndexState::Ready;

        // Page 1: all 6 requested rows present (complete contains results).
        let page1 = execute_search(
            &state,
            "zzfrag".to_string(),
            Some(6),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();
        assert_eq!(page1.results.len(), 6, "page1: {:?}", page1.mode_label);
        // Page 2: remaining 4 rows via offset pagination.
        let page2 = execute_search(
            &state,
            "zzfrag".to_string(),
            Some(6),
            Some(6),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();
        assert_eq!(page2.results.len(), 4, "page2: {:?}", page2.mode_label);
        let mut all: Vec<String> = page1
            .results
            .iter()
            .chain(page2.results.iter())
            .map(|e| e.name.clone())
            .collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 10, "no dup/missing rows across pages");

        // Total count matches the contains set.
        let total = compute_total_count(&state, &page1);
        assert_eq!(total, Some(10));

        let _ = fs::remove_dir_all(root);
    }

    /// Glob with leading wildcard goes through the FTS prefilter and must return
    /// the same rows as plain LIKE evaluation.
    #[test]
    fn execute_search_glob_fts_prefilter_matches_like() {
        let root = temp_case_dir("glob_fts_prefilter");
        fs::create_dir_all(&root).unwrap();
        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        let now = now_epoch();
        for (name, ext) in [
            ("alpha_test_one.js", "js"),
            ("beta_test_two.js", "js"),
            ("gamma_test.ts", "ts"),
            ("delta_other.js", "js"),
            ("TEST_upper.JS", "js"),
        ] {
            let path = root.join(name);
            conn.execute(
                "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
                 VALUES(?1, ?2, ?3, 0, ?4, NULL, NULL, ?5, 1)",
                params![
                    path.to_string_lossy().to_string(),
                    name,
                    root.to_string_lossy().to_string(),
                    ext,
                    now
                ],
            )
            .unwrap();
        }
        drop(conn);

        let state = test_state_for(db_path.clone(), root.clone(), root.clone());
        state.status.lock().state = IndexState::Ready;

        let result = execute_search(
            &state,
            "*test*.js".to_string(),
            Some(300),
            Some(0),
            Some("name".to_string()),
            Some("asc".to_string()),
        )
        .unwrap();
        let mut names: Vec<&str> = result.results.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        // LIKE is ASCII-case-insensitive: TEST_upper.JS matches too.
        assert_eq!(
            names,
            vec!["TEST_upper.JS", "alpha_test_one.js", "beta_test_two.js"]
        );

        let _ = fs::remove_dir_all(root);
    }

    /// Indexing benchmark against a synthetic tree (BENCH_TREE env var).
    ///
    /// Measures the real pipeline: fresh index (parallel scan + bulk insert +
    /// FTS rebuild + index build), no-change restart catchup, churn catchup,
    /// and watcher-style incremental updates (apply_path_changes).
    ///
    /// Run:
    /// ```sh
    /// BENCH_TREE=/path/to/tree cargo test --release --manifest-path src-tauri/Cargo.toml \
    ///   -- --ignored bench_index_speed --nocapture
    /// ```
    #[test]
    #[ignore]
    fn bench_index_speed() {
        let Ok(tree) = std::env::var("BENCH_TREE") else {
            eprintln!("BENCH_TREE not set; skipping");
            return;
        };
        let tree_root = PathBuf::from(&tree);
        assert!(tree_root.is_dir(), "BENCH_TREE does not exist");
        let work = temp_case_dir("bench_index_speed");
        fs::create_dir_all(&work).unwrap();
        let db_path = work.join("index.db");
        init_db_tables(&db_path).unwrap();

        let state = test_state_for(db_path.clone(), tree_root.clone(), tree_root.clone());

        // 1) Fresh index (empty DB). The scan phase ends the user-visible
        // "Indexing" state; DDL finalization runs on the finalizing thread in
        // production — timed separately here.
        let t = Instant::now();
        run_incremental_index(None, &state).expect("fresh index");
        let fresh_ready_ms = t.elapsed().as_millis();
        let t_fin = Instant::now();
        finalize_fresh_index(&state);
        ensure_db_indexes(&db_path).unwrap();
        let fresh_finalize_ms = t_fin.elapsed().as_millis();
        let entries_after_fresh: i64 = db_connection(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        eprintln!(
            "IDXBENCH fresh_ms={} fresh_ready_ms={fresh_ready_ms} fresh_finalize_bg_ms={fresh_finalize_ms} entries={entries_after_fresh}",
            fresh_ready_ms + fresh_finalize_ms
        );
        assert!(entries_after_fresh > 0);
        // FTS must be in sync after finalization (search relies on it).
        let fts_count: i64 = db_connection(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM entries_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, entries_after_fresh, "FTS in sync after fresh");

        // 2) No-change catchup (app restart with warm index).
        let t = Instant::now();
        run_incremental_index(None, &state).expect("catchup");
        eprintln!("IDXBENCH catchup_nochange_ms={}", t.elapsed().as_millis());
        let entries_after_catchup: i64 = db_connection(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(entries_after_fresh, entries_after_catchup);

        // 3) Churn catchup: +1000 new files, -500 deleted, 500 modified.
        let dirs: Vec<PathBuf> = fs::read_dir(&tree_root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        assert!(!dirs.is_empty());
        let mut created = Vec::new();
        for i in 0..1000 {
            let p = dirs[i % dirs.len()].join(format!("churn_new_{i:04}.tmp"));
            fs::write(&p, b"new").unwrap();
            created.push(p);
        }
        let existing_files: Vec<PathBuf> = walkdir::WalkDir::new(&dirs[0])
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .filter(|p| {
                // exclude the churn files created above
                !p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("churn_new_"))
                    .unwrap_or(false)
            })
            .take(1000)
            .collect();
        assert!(existing_files.len() >= 1000, "need 1000 files in first dir");
        for p in &existing_files[..500] {
            fs::remove_file(p).unwrap();
        }
        for p in &existing_files[500..1000] {
            fs::write(p, b"modified_content_larger").unwrap();
        }
        let t = Instant::now();
        run_incremental_index(None, &state).expect("churn catchup");
        eprintln!("IDXBENCH catchup_churn_ms={}", t.elapsed().as_millis());
        let entries_after_churn: i64 = db_connection(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            entries_after_churn,
            entries_after_fresh + 1000 - 500,
            "churn bookkeeping"
        );

        // 4) Watcher-style updates. Realistic shapes: single-file change and
        //    a 20-file burst, measured through apply_path_changes.
        let mut single_ms = Vec::new();
        for i in 0..20usize {
            let p = &created[i];
            fs::write(p, format!("touch_{i}_x")).unwrap();
            let batch = vec![p.clone()];
            let t = Instant::now();
            apply_path_changes(&state, &batch).expect("watcher single");
            single_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let mut burst_ms = Vec::new();
        for round in 0..10usize {
            let batch: Vec<PathBuf> = (0..20)
                .map(|i| {
                    let p = created[100 + round * 20 + i].clone();
                    fs::write(&p, format!("burst_{round}_{i}_xx")).unwrap();
                    p
                })
                .collect();
            let t = Instant::now();
            apply_path_changes(&state, &batch).expect("watcher burst");
            burst_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let med = |v: &mut Vec<f64>| -> f64 {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        eprintln!(
            "IDXBENCH watcher_single_med_ms={:.3} watcher_burst20_med_ms={:.3}",
            med(&mut single_ms),
            med(&mut burst_ms)
        );

        // Restore tree for reuse: remove churn artifacts, recreate deleted files.
        for p in &created {
            let _ = fs::remove_file(p);
        }
        for p in &existing_files[..500] {
            let _ = fs::write(p, b"");
        }
        for p in &existing_files[500..1000] {
            let _ = fs::write(p, b"");
        }
        let _ = fs::remove_dir_all(work);
    }

    /// Latency benchmark against a copy of a real index DB.
    ///
    /// Run:
    /// ```sh
    /// BENCH_DB=/path/to/bench.db BENCH_OUT=/path/to/report.json \
    ///   cargo test --release --manifest-path src-tauri/Cargo.toml \
    ///   -- --ignored bench_search_latency --nocapture
    /// ```
    /// Measures execute_search + compute_total_count per query (one UI keystroke
    /// equals both). Negative-name cache is cleared between iterations so the DB
    /// path is always exercised.
    #[test]
    #[ignore]
    fn bench_search_latency() {
        let Ok(db) = std::env::var("BENCH_DB") else {
            eprintln!("BENCH_DB not set; skipping");
            return;
        };
        let db_path = PathBuf::from(db);
        assert!(db_path.exists(), "BENCH_DB does not exist");
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        let iters: u32 = std::env::var("BENCH_ITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7);

        let state = test_state_for(db_path.clone(), home.clone(), home.clone());
        // Steady state: index complete (IndexStatus defaults to Indexing, which
        // would trigger spotlight merging for sub-limit results).
        state.status.lock().state = IndexState::Ready;

        // (label, query, sort_by, sort_dir)
        let cases: Vec<(&str, &str, &str, &str)> = vec![
            ("type_t", "t", "name", "asc"),
            ("type_te", "te", "name", "asc"),
            ("type_tes", "tes", "name", "asc"),
            ("type_test", "test", "name", "asc"),
            ("contains_icon", "icon", "name", "asc"),
            ("contains_index", "index", "name", "asc"),
            ("contains_readme", "readme", "name", "asc"),
            ("exact_main_rs", "main.rs", "name", "asc"),
            ("exact_package_json", "package.json", "name", "asc"),
            ("prefix_read", "read", "name", "asc"),
            ("ext_png", "*.png", "name", "asc"),
            ("ext_js", "*.js", "name", "asc"),
            ("glob_test_js", "*test*.js", "name", "asc"),
            ("glob_icon_star", "icon*", "name", "asc"),
            ("path_src_js", "src/ *.js", "name", "asc"),
            ("path_everything_main", "everything/ main", "name", "asc"),
            ("sort_mtime_test", "test", "mtime", "desc"),
            ("sort_size_test", "test", "size", "desc"),
            ("empty_query", "", "name", "asc"),
            ("no_match", "zzqx_no_match_9", "name", "asc"),
        ];

        fn fnv1a(s: &str) -> u64 {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in s.as_bytes() {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            h
        }

        let mut report = Vec::new();
        for (label, query, sort_by, sort_dir) in &cases {
            let mut search_ms = Vec::new();
            let mut count_ms = Vec::new();
            let mut mode = String::new();
            let mut n_results = 0usize;
            let mut total_count: Option<u32> = None;
            let mut results_hash: u64 = 0;
            let mut first_paths: Vec<String> = Vec::new();
            for _ in 0..iters {
                state.negative_name_cache.lock().clear();
                let t0 = Instant::now();
                let execution = execute_search(
                    &state,
                    query.to_string(),
                    Some(300),
                    Some(0),
                    Some(sort_by.to_string()),
                    Some(sort_dir.to_string()),
                )
                .expect("search failed");
                search_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
                let t1 = Instant::now();
                total_count = compute_total_count(&state, &execution);
                count_ms.push(t1.elapsed().as_secs_f64() * 1000.0);
                mode = execution.mode_label.clone();
                n_results = execution.results.len();
                let joined = execution
                    .results
                    .iter()
                    .map(|e| e.path.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                results_hash = fnv1a(&joined);
                first_paths = execution
                    .results
                    .iter()
                    .take(3)
                    .map(|e| e.path.clone())
                    .collect();
            }
            let med = |v: &mut Vec<f64>| -> f64 {
                v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                v[v.len() / 2]
            };
            let s_med = med(&mut search_ms);
            let c_med = med(&mut count_ms);
            let s_min = search_ms.first().copied().unwrap_or(0.0);
            let s_max = search_ms.last().copied().unwrap_or(0.0);
            eprintln!(
                "BENCH {label:>22} q={query:?} mode={mode:<14} n={n_results:<4} total={total_count:?} search med={s_med:8.2}ms (min={s_min:.2} max={s_max:.2}) count med={c_med:8.2}ms keystroke={:8.2}ms",
                s_med + c_med
            );
            report.push(serde_json::json!({
                "label": label,
                "query": query,
                "sort_by": sort_by,
                "sort_dir": sort_dir,
                "mode": mode,
                "results": n_results,
                "total_count": total_count,
                "results_hash": format!("{results_hash:016x}"),
                "first_paths": first_paths,
                "search_ms": search_ms,
                "count_ms": count_ms,
                "search_med_ms": s_med,
                "count_med_ms": c_med,
                "keystroke_med_ms": s_med + c_med,
            }));
        }

        if let Ok(out) = std::env::var("BENCH_OUT") {
            let json = serde_json::to_string_pretty(&report).unwrap();
            fs::write(&out, json).unwrap();
            eprintln!("BENCH report written to {out}");
        }
    }
}
