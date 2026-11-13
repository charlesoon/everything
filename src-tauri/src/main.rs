#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::{Mutex, RwLock};
use runtime::EventSink;
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
mod runtime;
mod ui;
#[cfg(all(target_os = "macos", feature = "ui-automation"))]
mod ui_automation;
mod ui_model;
mod ui_prefs;
mod ui_search_bench;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(not(target_os = "windows"))]
use std::process::Stdio;
mod daemon;
#[cfg(target_os = "macos")]
mod daemon_ipc;
mod diag;
mod fd_search;
mod file_actions;
#[cfg(target_os = "macos")]
mod mac;
mod mcp_server;
mod mem_search;
mod pathindexing;
mod query;
mod rescan;
mod ui_gate;
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
#[cfg(not(target_os = "macos"))]
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
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "C:\\".to_string()
                } else {
                    "/".to_string()
                }
            }),
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
    "Library",
    ".Trash",
    ".Trashes",
    // Windows system directories (deferred when scan_root is C:\)
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "$Recycle.Bin",
    "System Volume Information",
    "Recovery",
    "PerfLogs",
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
    // Windows: noisy application directories under AppData; keep Temp searchable.
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
static INDEX_FULL_SPEED: OnceLock<bool> = OnceLock::new();
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

#[cfg(not(target_os = "macos"))]
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
    /// Cooperative-cancel flag for an in-progress index pass. The headless
    /// daemon sets this when the GUI appears mid-build so the pass unwinds
    /// promptly — returning an error and leaving `index_complete` unset — and
    /// the GUI's bounded `wait_for_daemon_exit` always succeeds without a
    /// concurrent-writer window. Never set on the GUI's own index path.
    pub(crate) cancel_index: Arc<AtomicBool>,
    pub(crate) status: Arc<Mutex<IndexStatus>>,
    #[cfg(not(target_os = "macos"))]
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
    /// Generation stamp for watcher workers: every start bumps it, and a
    /// worker exits (without touching `watcher_active`) once the stamp moves
    /// on. A stop-and-restart (reset) that timed out waiting on a worker
    /// blocked mid-iteration therefore can't resurrect it into a second live
    /// watcher when the restart clears `watcher_stop`.
    pub(crate) watcher_gen: Arc<AtomicU64>,
    /// Set by the Windows RDCW watcher when the OS reports dropped change
    /// notifications (buffer overflow). The daemon's resident loop observes it
    /// and triggers an immediate reconcile pass instead of waiting for the next
    /// scheduled one, so a burst of changes can't silently drift the index.
    #[cfg(target_os = "windows")]
    pub(crate) watcher_lost_events: Arc<AtomicBool>,
    /// Set by the USN watcher when a config sidecar is edited; the service
    /// supervisor consumes it to reload rules + enqueue a reconcile pass (§3.1).
    #[cfg(target_os = "windows")]
    pub(crate) config_dirty: Arc<AtomicBool>,
    /// Set to true once frontend onMount has completed enough to accept user input.
    pub(crate) frontend_ready: Arc<AtomicBool>,
    /// Guards against concurrent pathindexing background threads.
    #[cfg(target_os = "macos")]
    pub(crate) pathindexing_active: Arc<AtomicBool>,
    /// Set by the FSEvents watcher when `.pathignore`'s active entries change;
    /// the daemon's supervise tick consumes it (purge newly ignored rows +
    /// catchup for un-ignored paths). The read-only GUI can no longer apply
    /// the change at restart (D4), so the daemon applies it live.
    #[cfg(target_os = "macos")]
    pub(crate) pathignore_dirty: Arc<AtomicBool>,
    /// Warm read connections reused across searches. Opening a connection per
    /// search costs ~1-2ms (open + PRAGMAs + schema parse) and starts with a
    /// cold SQLite page cache; reuse keeps hot pages and prepared statements.
    pub(crate) search_conn_pool: Arc<Mutex<Vec<Connection>>>,
    /// Persistent write connection for watcher-driven incremental updates.
    /// Opening a connection per event batch dominated single-file update cost.
    #[cfg(target_os = "macos")]
    pub(crate) watcher_conn: Arc<Mutex<Option<Connection>>>,
    /// Whether the daemon reports a build in progress (bridge-maintained).
    /// Read-only GUI's substitute for `indexing_active`, e.g. to defer the
    /// icon prewarm's Swift run out of the fresh-index CPU window.
    #[cfg(target_os = "macos")]
    pub(crate) daemon_building: Arc<AtomicBool>,
    /// Whether this process owns index writes. True only for the GUI instance
    /// that acquired the `gui.lock` beacon (single-writer invariant). A second
    /// GUI instance that lost the race runs read-only: it never starts the
    /// watcher / indexer / purge, so two GUIs can't write the same WAL DB.
    pub(crate) is_writer: Arc<AtomicBool>,
}

/// Construct `AppState` from resolved paths, without Tauri. Shared by GUI
/// startup (`setup_app`) and the headless MCP index builder, which has no
/// `EventSink`. `app_data_dir` (the DB's parent) holds the `.pathignore` and
/// `.pathindexing` sidecars and is itself excluded from indexing.
pub(crate) fn build_app_state(
    db_path: PathBuf,
    home_dir: PathBuf,
    app_data_dir: &Path,
) -> AppState {
    // Default layout: the config sidecars live alongside the DB (GUI / daemon /
    // MCP). The Windows service overrides this via build_app_state_with_config.
    build_app_state_with_config(db_path, home_dir, app_data_dir, app_data_dir)
}

/// Like [`build_app_state`] but with the config sidecars in a directory separate
/// from the data directory — the Windows service layout (D1), where `data\` is
/// read-only to users and `config\` is user-writable. Both directories are
/// excluded from indexing.
pub(crate) fn build_app_state_with_config(
    db_path: PathBuf,
    home_dir: PathBuf,
    data_dir: &Path,
    config_dir: &Path,
) -> AppState {
    let scan_root = if cfg!(windows) {
        PathBuf::from("C:\\")
    } else {
        home_dir.clone()
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| home_dir.clone());
    let config_file_path = config_dir.join(".pathignore");
    let (mut path_ignores, path_ignore_patterns) =
        load_pathignore_rules(&config_file_path, &home_dir, &cwd);
    if !path_ignores.iter().any(|r| r == data_dir) {
        path_ignores.push(data_dir.to_path_buf());
    }
    if config_dir != data_dir && !path_ignores.iter().any(|r| r == config_dir) {
        path_ignores.push(config_dir.to_path_buf());
    }
    let pathindexing_file_path = config_dir.join(".pathindexing");
    let extra_roots = pathindexing::load_pathindexing_roots(&pathindexing_file_path);
    AppState {
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
        cancel_index: Arc::new(AtomicBool::new(false)),
        status: Arc::new(Mutex::new(IndexStatus::default())),
        #[cfg(not(target_os = "macos"))]
        recent_ops: Arc::new(Mutex::new(Vec::new())),
        icon_cache: Arc::new(Mutex::new(HashMap::new())),
        fd_search_cache: Arc::new(Mutex::new(None)),
        negative_name_cache: Arc::new(Mutex::new(HashMap::new())),
        ignore_cache: Arc::new(Mutex::new(None)),
        fts_ready: Arc::new(AtomicBool::new(true)),
        mem_index: Arc::new(RwLock::new(None)),
        watcher_stop: Arc::new(AtomicBool::new(false)),
        watcher_active: Arc::new(AtomicBool::new(false)),
        watcher_gen: Arc::new(AtomicU64::new(0)),
        #[cfg(target_os = "windows")]
        watcher_lost_events: Arc::new(AtomicBool::new(false)),
        #[cfg(target_os = "windows")]
        config_dirty: Arc::new(AtomicBool::new(false)),
        frontend_ready: Arc::new(AtomicBool::new(false)),
        #[cfg(target_os = "macos")]
        pathindexing_active: Arc::new(AtomicBool::new(false)),
        #[cfg(target_os = "macos")]
        pathignore_dirty: Arc::new(AtomicBool::new(false)),
        search_conn_pool: Arc::new(Mutex::new(Vec::new())),
        #[cfg(target_os = "macos")]
        watcher_conn: Arc::new(Mutex::new(None)),
        #[cfg(target_os = "macos")]
        daemon_building: Arc::new(AtomicBool::new(false)),
        // Default false; the GUI sets it from the beacon result in setup_app.
        // The headless daemon builds via its own path and never consults this.
        is_writer: Arc::new(AtomicBool::new(false)),
    }
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

/// Escape hatch restoring the pre-0.5.3 indexing behavior: cores oversubscribed
/// 2x and no QoS downgrade. Fastest wall-clock at the cost of pinning the whole
/// fan-out to performance cores, so it is opt-in (and implied by bench mode,
/// which measures throughput rather than power).
fn index_full_speed() -> bool {
    *INDEX_FULL_SPEED
        .get_or_init(|| env_truthy("EVERYTHING_INDEX_FULL_SPEED") || bench_mode_enabled())
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

/// Set once at startup by a pure-reader process — the Windows GUI and the MCP
/// server (§3.3). When set, `db_connection` opens the index read-only instead
/// of read-write + WAL, so the process reads the service-owned, Users-read-only
/// DB and any stray write surfaces as an immediate error instead of silent
/// corruption. The service process leaves it false and owns all writes.
static DB_READONLY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Mark this process a pure reader (see [`DB_READONLY`]). Call once at startup,
/// before any DB connection is opened.
pub(crate) fn set_db_readonly() {
    DB_READONLY.store(true, std::sync::atomic::Ordering::Release);
}

fn db_connection_with_timeout(db_path: &Path, busy_timeout_ms: u32) -> AppResult<Connection> {
    if DB_READONLY.load(std::sync::atomic::Ordering::Acquire) {
        // Pure reader: read-only open, no WAL/schema pragmas. A read-only
        // connection reads a WAL DB fine while the writer keeps -wal/-shm live;
        // SQLITE_READONLY_RECOVERY is the caller's transient case (D1).
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())?;
        conn.execute_batch(&format!("PRAGMA busy_timeout={busy_timeout_ms};"))
            .map_err(|e| e.to_string())?;
        return Ok(conn);
    }
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
    // Read-only vs read-write is governed process-wide by DB_READONLY (set for
    // the Windows GUI / MCP), so this shares the one connection opener.
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
        self.conn
            .as_ref()
            .expect("pooled connection present until drop")
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
            let _ = conn.progress_handler(0, None::<fn() -> bool>);
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
    // Windows installers may have to terminate an old indexer that cannot
    // drain. Keep WAL checkpoint ordering crash-safe during bulk indexing too.
    let synchronous = if cfg!(target_os = "windows") {
        "NORMAL"
    } else {
        "OFF"
    };
    conn.execute_batch(&format!(
        r#"
        PRAGMA synchronous = {synchronous};
        PRAGMA cache_size = -131072;
        PRAGMA mmap_size = 536870912;
        PRAGMA wal_autocheckpoint = 0;
        "#,
    ))
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
    eprintln!(
        "[init_db] +{}ms db_connection opened",
        t.elapsed().as_millis()
    );

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
            conn.execute_batch(CREATE_ENTRIES_TABLE_SQL)
                .map_err(|e| e.to_string())?;

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
    eprintln!(
        "[init_db] +{}ms version check done (v={})",
        t.elapsed().as_millis(),
        current_version
    );

    conn.execute_batch(CREATE_ENTRIES_TABLE_SQL)
        .map_err(|e| e.to_string())?;
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
    conn.execute_batch(CREATE_FTS_TRIGGERS_SQL)
        .map_err(|e| e.to_string())?;
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
    eprintln!(
        "[init_db] +{}ms total (indexes + migration)",
        t.elapsed().as_millis()
    );

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

/// Read a meta value written by [`set_meta_list`]. Several meta keys hold a list
/// of paths (`indexed_extra_roots`, `denied_prefixes`), and the newline codec was
/// open-coded at every site — one pair keeps encode and decode symmetrical, and
/// keeps "blank lines are not entries" from being re-decided each time.
pub(crate) fn meta_list(conn: &Connection, key: &str) -> Vec<String> {
    get_meta(conn, key)
        .map(|value| {
            value
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn set_meta_list<S: AsRef<str>>(
    conn: &Connection,
    key: &str,
    entries: impl IntoIterator<Item = S>,
) -> AppResult<()> {
    let joined = entries
        .into_iter()
        .map(|e| e.as_ref().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    set_meta(conn, key, &joined)
}

/// Persist the status-bar counts so the next startup can seed them without a
/// COUNT(*)/MAX() scan over `entries`. Best-effort: write errors are ignored.
pub(crate) fn persist_cached_counts(
    conn: &Connection,
    entries_count: u64,
    last_updated: Option<i64>,
) {
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
        let mut entries: Vec<_> = cache
            .iter()
            .map(|(k, v)| (k.clone(), v.created_at))
            .collect();
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
    if s.contains('\\') {
        s.replace('\\', "/")
    } else {
        s
    }
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

fn load_pathignore_rules(
    config_file: &Path,
    home_dir: &Path,
    cwd: &Path,
) -> (Vec<PathBuf>, Vec<IgnorePattern>) {
    // Windows: never auto-create the sidecar (D1 — a missing file means the
    // builtin defaults; the editor command creates it on demand). Other
    // platforms keep seeding a default template at startup.
    #[cfg(not(target_os = "windows"))]
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
/// (browser caches, UWP package state, etc.).
/// Pruning these from the MFT subtree eliminates noisy USN records.
#[cfg(target_os = "windows")]
fn windows_noisy_roots(home_dir: &Path) -> Vec<PathBuf> {
    let local = home_dir.join("AppData").join("Local");
    let dirs = vec![
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

pub(crate) fn cached_effective_ignore_rules(
    state: &AppState,
) -> (Vec<PathBuf>, Vec<IgnorePattern>) {
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
        if cached.config_file_mtime == config_file_mtime
            && cached.pathignore_mtime == pathignore_mtime
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

// Housekeeping purge is a write, so it only runs in the writer (non-Windows
// GUI); the Windows GUI is read-only and the service owns index hygiene (§3.3).
#[cfg(not(target_os = "windows"))]
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

/// Per-child form of [`should_skip_path`], for jwalk's `process_read_dir` where
/// the parent directory has **already** cleared `should_skip_path`. Callers
/// must uphold that invariant — see the `root_vetted` guards at the walker call
/// sites.
///
/// `should_skip_path` re-derives the answer from the whole path on every entry:
/// a string allocation, a segment split checked against every builtin name, and
/// a substring search per multi-segment rule — roughly O(depth) work repeated
/// for each of ~1M files, even though only the leaf is ever new. The invariant
/// collapses each check to its leaf case:
///
///   * builtin names/suffixes are per-segment, and every segment but the leaf
///     belongs to the parent;
///   * the multi-segment builtin paths and `AnySegment` patterns each test an
///     `infix` (`/pat/`) and a `suffix` (`/pat`). An `infix` hit on the child
///     lies either wholly inside the parent (already false) or across the join
///     slash — which is exactly a parent `suffix` hit, also false. Only the
///     child's own `suffix` arm can newly fire;
///   * `ignored_roots` matches with `starts_with`, and any strict-ancestor hit
///     would already have matched the parent, leaving only equality;
///   * `matches_ignore_pattern` walks a glob up through every ancestor, all of
///     which the parent cleared, leaving only the child itself.
///
/// `dir_str` is the parent path, slash-normalized once per directory rather
/// than once per child. `scratch` is a caller-owned buffer reused across one
/// directory's children so building the joined path costs a memcpy rather than
/// an allocation.
fn should_skip_child(
    dir_str: &str,
    name: &str,
    child: &Path,
    scratch: &mut String,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> bool {
    // A literal backslash in a macOS file name becomes a separator once
    // normalized, so the leaf would really be several segments. Those names are
    // vanishingly rare; hand them to the general check rather than reasoning
    // about them here (on Windows a single component never contains one).
    if name.contains('\\') {
        return should_skip_path(child, ignored_roots, ignored_patterns);
    }

    if BUILTIN_SKIP_NAMES.contains(&name)
        || BUILTIN_SKIP_SUFFIXES.iter().any(|suf| name.ends_with(suf))
    {
        return true;
    }

    scratch.clear();
    scratch.push_str(dir_str);
    if !scratch.ends_with('/') {
        scratch.push('/');
    }
    scratch.push_str(name);

    if builtin_skip_path_checks()
        .iter()
        .any(|(_, suffix)| scratch.ends_with(suffix.as_str()))
    {
        return true;
    }

    if ignored_roots.iter().any(|root| child == root) {
        return true;
    }

    ignored_patterns.iter().any(|pattern| match pattern {
        IgnorePattern::AnySegment { segment, .. } => name == segment,
        IgnorePattern::Glob(glob) => glob_match_path(glob, scratch),
    })
}

fn extension_for(path: &Path, is_dir: bool) -> Option<String> {
    if is_dir {
        return None;
    }

    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_lowercase())
}

pub(crate) fn index_row_from_path_and_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Option<IndexRow> {
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
            let depth = if rank <= 3 {
                path_depth(&entry.path)
            } else {
                0
            };
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

#[cfg(not(target_os = "windows"))]
fn find_search(
    home_dir: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
    query: &str,
    limit: usize,
    sort_by: &str,
    sort_dir: &str,
) -> Vec<EntryDto> {
    use std::io::{BufRead, BufReader};
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

/// Windows has no Unix `find`; `Command::new("find")` there launches find.exe
/// (a text filter) which errors on these args, returns nothing, and pops a
/// console window per call. The service keeps a complete index and the
/// fd_search (jwalk) path covers live scans, so this fallback is a no-op here.
#[cfg(target_os = "windows")]
fn find_search(
    _home_dir: &Path,
    _ignored_roots: &[PathBuf],
    _ignored_patterns: &[IgnorePattern],
    _query: &str,
    _limit: usize,
    _sort_by: &str,
    _sort_dir: &str,
) -> Vec<EntryDto> {
    Vec::new()
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

// Coalesce index updates while the native window is inactive, then flush the
// latest payload on activation. The daemon does not depend on a window.

pub(crate) fn emit_index_state(app: &EventSink, state: &str, message: Option<String>) {
    if !ui_gate::is_visible() {
        ui_gate::hold_state(state.to_string(), message);
        return;
    }
    emit_index_state_now(app, state, message);
}

pub(crate) fn emit_index_state_now(app: &EventSink, state: &str, message: Option<String>) {
    let is_catchup = message
        .as_ref()
        .map_or(false, |m| m.starts_with("Catchup:"));
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
    app: &EventSink,
    entries_count: u64,
    last_updated: i64,
    permission_errors: u64,
) {
    if !ui_gate::is_visible() {
        ui_gate::hold_updated(entries_count, last_updated, permission_errors);
        return;
    }
    emit_index_updated_now(app, entries_count, last_updated, permission_errors);
}

pub(crate) fn emit_index_updated_now(
    app: &EventSink,
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

pub(crate) fn emit_index_progress(
    app: &EventSink,
    scanned: u64,
    indexed: u64,
    current_path: String,
) {
    if !ui_gate::is_visible() {
        ui_gate::hold_progress(scanned, indexed, current_path);
        return;
    }
    emit_index_progress_now(app, scanned, indexed, current_path);
}

pub(crate) fn emit_index_progress_now(
    app: &EventSink,
    scanned: u64,
    indexed: u64,
    current_path: String,
) {
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

fn emit_status_counts(app: &EventSink, state: &AppState) {
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

pub(crate) fn refresh_and_emit_status_counts(
    app: Option<&EventSink>,
    state: &AppState,
) -> AppResult<()> {
    let conn = db_connection(&state.db_path)?;
    let (entries_count, last_updated) = update_counts(&conn)?;
    {
        let mut status = state.status.lock();
        status.entries_count = entries_count;
        status.last_updated = last_updated;
    }
    // Persist cached counts so next startup can read them instantly. This runs
    // headless too (daemon mode, app=None) — only the UI emit is skipped.
    persist_cached_counts(&conn, entries_count, last_updated);
    if let Some(app) = app {
        emit_status_counts(app, state);
    }
    Ok(())
}

/// Emit status-bar counts from the in-memory `IndexStatus` (maintained
/// incrementally by the watcher) and persist them for instant startup reads.
/// Unlike `refresh_and_emit_status_counts` this never queries `entries`, so
/// it is safe on every watcher batch: the COUNT(*)/MAX() recount there was a
/// periodic whole-table scan — a visible CPU spike every ~2s on large DBs.
#[cfg(target_os = "macos")]
fn emit_and_persist_cached_counts(app: Option<&EventSink>, state: &AppState) {
    let (entries_count, last_updated) = {
        let status = state.status.lock();
        (status.entries_count, status.last_updated)
    };
    // Best-effort persist over the shared watcher connection; the next full
    // recount rewrites the cache if the connection isn't open.
    if let Some(conn) = state.watcher_conn.lock().as_ref() {
        persist_cached_counts(conn, entries_count, last_updated);
    }
    if let Some(app) = app {
        emit_status_counts(app, state);
    }
}

/// Load cached entries count from meta table (instant) and emit Ready state + counts.
/// Used on read-only-GUI startup paths (Windows service reader, macOS daemon
/// reader) where the index is already complete from a prior writer run.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) fn set_ready_with_cached_counts(app: &EventSink, state: &AppState) {
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
pub(crate) fn start_full_index_worker(app: EventSink, state: AppState) -> AppResult<()> {
    start_full_index_worker_inner(app, state, false)
}

#[cfg(not(target_os = "windows"))]
fn start_full_index_worker_inner(app: EventSink, state: AppState, silent: bool) -> AppResult<()> {
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
    // Attribution trail: index_complete=0 is what the daemon's supervisory
    // loop keys its heals on, so record every writer of that state. If this
    // process dies before the pass succeeds, the flag stays 0 and the next
    // daemon will (correctly) rebuild — this line explains why.
    diag::log("full index pass requested (index_complete=0 until it succeeds)");

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
pub(crate) fn finalize_fresh_index(state: &AppState) {
    let Ok(conn) = db_connection_for_maintenance(&state.db_path) else {
        return;
    };
    // Declare the role here rather than trusting the caller's: the GUI reaches
    // this on a dedicated finalizing thread that sets `FreshWriter`, but the
    // daemon calls it inline on its build thread (daemon.rs `build_index`),
    // where the class is whatever the pass latched — `IdleWriter`/Background for
    // a catchup healing a dirty FTS, which would put the minutes-long rebuild
    // below on the E-cluster. The work's role is unambiguous, so it should not
    // depend on which caller got here.
    apply_scan_scheduling(ScanRole::FreshWriter);
    // The longest single-core stretch in the app (~6W on one P-core for up to
    // minutes on a 1M-row DB) — worth its own accounting line in the diag log
    // so a field CPU report can tell it apart from a misscheduled walk.
    let started = Instant::now();
    #[cfg(target_os = "macos")]
    let cpu_start = mac::cpu_monitor::Snapshot::take();
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

    #[cfg(target_os = "macos")]
    let cpu_note = mac::cpu_monitor::pass_summary(cpu_start);
    #[cfg(not(target_os = "macos"))]
    let cpu_note = String::new();
    diag::log(format!(
        "fresh finalize done in {:.1}s (indexes + FTS rebuild + ANALYZE){cpu_note}",
        started.elapsed().as_secs_f32()
    ));
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

/// Hand an indexing pass's freed-but-retained memory back to the OS. Without
/// this a large pass leaves hundreds of MB resident until system memory
/// pressure reclaims it. macOS: malloc pressure relief. Windows: trim the
/// process working set (pages fault back in on demand).
pub(crate) fn release_memory_to_os() {
    #[cfg(target_os = "macos")]
    unsafe {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::os::raw::c_void, goal: usize) -> usize;
        }
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    #[cfg(target_os = "windows")]
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
        // (SIZE_T)-1 for both bounds trims the working set to the minimum.
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// What a scan thread does, which decides how far it steps out of the
/// foreground's way. This is a *role*, not an urgency: `UTILITY` does not bound
/// CPU draw, so what matters is how many threads a role runs on rather than how
/// badly the user wants the pass to finish. `mac::sched` has the measurements
/// this rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanRole {
    /// The parallel walk: rayon pool threads plus walk workers. The entire
    /// thread multiplier lives here, so this role is always E-cluster resident.
    /// `readdir`/`lstat` is syscall-bound work E-cores handle well, and the walk
    /// has slack anyway — a fresh pass's critical path is the writer below.
    Walk,
    /// The one SQLite writer thread of a fresh pass, and the finalizer (index
    /// creation + FTS rebuild) that follows it. Single-threaded, genuinely
    /// CPU-bound, and what a fresh pass's wall time is actually made of, so it
    /// keeps a performance core: one busy P-core is cheap, nine are a fan.
    FreshWriter,
    /// Writer/maintenance work nobody is waiting on: restart catchup, subtree
    /// rescans, the FSEvents watcher and anything it spawns. Same class as
    /// [`ScanRole::Walk`] today; kept distinct because it documents intent at
    /// the call site and the two need not stay tied.
    IdleWriter,
}

/// The class a role takes right now. Split out from [`apply_scan_scheduling`] so
/// the scan-start log can name the walk fan-out's class before its threads exist
/// — the whole point of the writer/walk split is invisible otherwise, which is
/// how the `UTILITY`-is-a-throttle assumption survived a release.
#[cfg(target_os = "macos")]
fn scan_qos(role: ScanRole) -> mac::sched::Qos {
    use mac::sched::{Pressure, Qos};
    // A hot or battery-saving machine pushes even the critical-path writer
    // down: finishing sooner is not worth being throttled.
    match (role, mac::sched::pressure()) {
        (ScanRole::FreshWriter, Pressure::None | Pressure::Elevated) => Qos::Utility,
        _ => Qos::Background,
    }
}

/// Step the calling thread out of the foreground's way for the rest of an
/// indexing pass.
///
/// macOS only: sets the thread's QoS class. Without this every scan thread
/// inherits the Tauri main thread's `USER_INTERACTIVE` class, which pins the
/// whole fan-out to performance cores at full clock — the direct cause of the
/// 400%+ CPU and fan spin-up during a first-run index on Apple Silicon. Must be
/// called *from* the thread it should affect.
///
/// No-op elsewhere: on Windows the indexer already lives in a separate service
/// process that the SCM runs at a lower priority.
#[allow(unused_variables)]
pub(crate) fn apply_scan_scheduling(role: ScanRole) {
    #[cfg(target_os = "macos")]
    {
        if index_full_speed() {
            return;
        }
        let qos = scan_qos(role);
        let errno = mac::sched::apply(qos);
        if errno != 0 {
            // Survivable (the thread keeps its inherited class) but it means the
            // CPU spike this whole path exists to prevent is back, so leave a
            // trail instead of failing silently. Once per process: this runs on
            // every scan thread.
            static REPORTED: AtomicBool = AtomicBool::new(false);
            if !REPORTED.swap(true, AtomicOrdering::Relaxed) {
                diag::log(format!(
                    "WARNING: could not lower indexing QoS to {qos:?} (errno={errno}); \
                     indexing will run at the inherited priority"
                ));
            }
        }
    }
}

/// Filesystem observation failures a pass ran into, split by what the user can
/// actually do about them.
///
/// This replaces a single `permission_errors` counter that lumped three
/// unrelated things together and reported the total as "N permission/access
/// error(s) occurred":
///
/// * **Denials.** On macOS these are TCC, and a first install before Full Disk
///   Access is granted produces hundreds: one per `~/Library/Containers/<app>/
///   Data` (a Mac with many sandboxed apps has several hundred), one per
///   `~/Library/Group Containers/<id>`, plus the ~20 classic protected
///   `~/Library` subtrees (Mail, Messages, Safari, Suggestions, …). Expected,
///   and fixable by granting access.
/// * **Mid-walk deletions.** A directory that vanishes between its parent's
///   `readdir` and the walk reaching it. Unbounded on a busy machine (browser
///   caches, build output) and nothing to report — these are no longer counted
///   at all.
/// * **Real I/O faults.** A failing disk, a stalled network mount. Rare, not
///   user-fixable, and previously indistinguishable from the other two.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ScanFailures {
    /// `EACCES`/`EPERM` — a path we are not allowed to enumerate.
    pub(crate) denied: u64,
    /// Any other non-`NotFound` error while enumerating or stat-ing.
    pub(crate) io: u64,
}

impl ScanFailures {
    /// Failures worth reporting to the user.
    pub(crate) fn total(self) -> u64 {
        self.denied + self.io
    }

    fn is_empty(self) -> bool {
        self.total() == 0
    }

    /// Classify one walk or metadata error. Returns whether it was counted —
    /// `false` means the path simply vanished mid-walk, which is normal on a
    /// live filesystem and neither reported nor logged.
    fn observe(&mut self, kind: Option<io::ErrorKind>) -> bool {
        match kind {
            Some(io::ErrorKind::NotFound) => false,
            Some(io::ErrorKind::PermissionDenied) => {
                self.denied += 1;
                true
            }
            _ => {
                self.io += 1;
                true
            }
        }
    }

    fn merge(&mut self, other: Self) {
        self.denied += other.denied;
        self.io += other.io;
    }
}

/// The status-bar line for a pass's observation failures, or `None` when there
/// is nothing actionable to say. Named in the UI's language (Korean) and phrased
/// so the number is self-explanatory: the old "321 permission/access error(s)
/// occurred." told the user something had gone wrong without saying what, and
/// the answer was almost always "Full Disk Access is not granted yet".
fn scan_failure_message(failures: ScanFailures, fda_granted: bool) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if failures.denied > 0 {
        parts.push(if fda_granted {
            format!("접근 권한이 없어 {}개 건너뜀", failures.denied)
        } else {
            format!(
                "보호된 항목 {}개 건너뜀 — 전체 디스크 접근 권한을 허용하면 모두 검색됩니다",
                failures.denied
            )
        });
    }
    if failures.io > 0 {
        parts.push(format!("읽기 실패 {}개", failures.io));
    }
    Some(parts.join(" / "))
}

/// Whether the writer process last saw Full Disk Access. Read from the
/// `last_fda_ok` meta the daemon maintains rather than re-probing TCC.db:
/// non-macOS has no such gate and reads as granted.
fn fda_granted(conn: &Connection) -> bool {
    #[cfg(target_os = "macos")]
    {
        return get_meta(conn, "last_fda_ok").as_deref() != Some("0");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = conn;
        true
    }
}

/// How many distinct denied prefixes a pass records for the FDA-rising-edge
/// rescan. Only a backstop: a few hundred denials collapse onto ~25 prefixes at
/// [`DENIED_PREFIX_DEPTH`], so the cap exists so that a pathological tree cannot
/// grow the meta row without bound.
const DENIED_PREFIX_LIMIT: usize = 64;
/// How many path components below the scan root a denied path is truncated to
/// before being recorded. 2 collapses the several-hundred
/// `~/Library/Containers/<app>/Data` denials onto the single
/// `~/Library/Containers`, which is what keeps the rescan set small enough to
/// walk directly instead of repeating a whole 1M-entry pass.
const DENIED_PREFIX_DEPTH: usize = 2;
/// Meta key holding the newline-joined denied prefixes of the last full pass.
const DENIED_PREFIXES_META: &str = "denied_prefixes";

/// The prefix recorded for a denied `path`: truncated to [`DENIED_PREFIX_DEPTH`]
/// components below `scan_root`, or left whole when it lies outside the scan
/// root (a `.pathindexing` extra root, which is user-configured and small).
fn denied_prefix_for(path: &Path, scan_root: &Path) -> String {
    let keep = scan_root.components().count() + DENIED_PREFIX_DEPTH;
    if !path.starts_with(scan_root) || path.components().count() <= keep {
        return path.to_string_lossy().to_string();
    }
    path.components()
        .take(keep)
        .collect::<PathBuf>()
        .to_string_lossy()
        .to_string()
}

/// Distinct denied prefixes accumulated across a pass's walk workers. `BTreeSet`
/// so the persisted list is stable between passes (and so a diff of the meta row
/// is readable in a field report).
type DeniedPrefixes = Arc<Mutex<BTreeSet<String>>>;

fn note_denied_prefix(denied: &DeniedPrefixes, path: &Path, scan_root: &Path) {
    let prefix = denied_prefix_for(path, scan_root);
    let mut set = denied.lock();
    if set.len() < DENIED_PREFIX_LIMIT || set.contains(&prefix) {
        set.insert(prefix);
    }
}

/// One walk worker's failure accounting: the counts it will ship to the writer,
/// plus the shared denied-prefix set and the root the prefixes are relative to.
///
/// Exists so classification lives in exactly one place. Every call site used to
/// repeat the same three steps — classify, bump the per-root tally, then re-test
/// `kind == PermissionDenied` to record the prefix — and the copies had already
/// drifted on whether a mid-walk deletion counts.
struct WalkFailures {
    counts: ScanFailures,
    /// Failures for the root currently being walked, for its `[timing]` line.
    root_errors: u64,
    denied: DeniedPrefixes,
    scan_root: PathBuf,
}

impl WalkFailures {
    fn new(denied: DeniedPrefixes, scan_root: PathBuf) -> Self {
        Self {
            counts: ScanFailures::default(),
            root_errors: 0,
            denied,
            scan_root,
        }
    }

    /// Classify one walk/metadata error against `path`, recording a denied
    /// prefix when that is what it was. Returns false when the path merely
    /// vanished mid-walk, which callers treat as a non-event.
    fn observe(&mut self, kind: Option<io::ErrorKind>, path: &Path) -> bool {
        if !self.counts.observe(kind) {
            return false;
        }
        self.root_errors += 1;
        if kind == Some(io::ErrorKind::PermissionDenied) {
            note_denied_prefix(&self.denied, path, &self.scan_root);
        }
        true
    }

    /// Whether this failure is within the per-worker stderr budget — the first
    /// 20 get a line each, then only under `perf_log`.
    fn should_log(&self) -> bool {
        self.counts.total() <= 20 || perf_log_enabled()
    }

    /// Take the counts for a channel flush, leaving the accumulator empty.
    fn take_counts(&mut self) -> ScanFailures {
        std::mem::take(&mut self.counts)
    }

    fn start_root(&mut self) {
        self.root_errors = 0;
    }
}

/// The prefixes the last full pass could not enumerate, for the FDA-rising-edge
/// rescan. Empty when nothing was recorded (a pass that hit no denials, or a DB
/// written before this key existed).
pub(crate) fn load_denied_prefixes(db_path: &Path) -> Vec<String> {
    db_connection(db_path)
        .map(|conn| meta_list(&conn, DENIED_PREFIXES_META))
        .unwrap_or_default()
}

/// One-line description of the scheduling in force for a pass: the calling
/// (writer) thread's actual class plus the class the walk fan-out will take.
/// Makes a silently-failed QoS downgrade — and the writer/walk split itself —
/// visible in a field log instead of something only a profiler would catch.
pub(crate) fn scan_scheduling_label() -> String {
    #[cfg(target_os = "macos")]
    {
        if index_full_speed() {
            return format!(
                "writer_qos={} walk_qos=inherited full_speed=1",
                mac::sched::current_class_name()
            );
        }
        return format!(
            "writer_qos={} walk_qos={}",
            mac::sched::current_class_name(),
            mac::sched::class_name_of(scan_qos(ScanRole::Walk)),
        );
    }
    #[cfg(not(target_os = "macos"))]
    "qos=n/a".to_string()
}

/// Divisor applied to the scan fan-out when the machine is already hot or in
/// Low Power Mode. Always 1 where there is no thermal signal to read, or when
/// the full-speed escape hatch is set.
fn scan_pressure_divisor() -> usize {
    #[cfg(target_os = "macos")]
    if !index_full_speed() {
        return match mac::sched::pressure() {
            mac::sched::Pressure::None => 1,
            mac::sched::Pressure::Elevated => 2,
            mac::sched::Pressure::High => 4,
        };
    }
    1
}

/// Duty-cycle limiter for the walk loops.
///
/// Thread count and QoS are both fixed when a pass starts, so if the machine
/// heats up mid-run this is the only lever left. Rather than guessing a fixed
/// nap it sleeps for a multiple of the time just spent working, which
/// self-calibrates to whatever a walk actually costs on this machine.
pub(crate) struct ScanThrottle {
    last_tick: Instant,
    enabled: bool,
}

impl ScanThrottle {
    /// Longest single nap. Bounds the damage when the gap between ticks is
    /// dominated by something other than CPU — a slow network mount, or a
    /// laptop that was asleep across the interval.
    const MAX_NAP: Duration = Duration::from_millis(250);

    pub(crate) fn new() -> Self {
        Self {
            last_tick: Instant::now(),
            enabled: !index_full_speed(),
        }
    }

    /// Call from a scan worker on a coarse counter (every few thousand
    /// entries). Cheap when the machine is cool: the pressure probe is cached
    /// for 2s and returns without sleeping.
    pub(crate) fn tick(&mut self) {
        if !self.enabled {
            return;
        }
        let busy = self.last_tick.elapsed();
        let ratio = Self::backoff_ratio();
        if ratio > 0.0 {
            std::thread::sleep(busy.mul_f32(ratio).min(Self::MAX_NAP));
        }
        self.last_tick = Instant::now();
    }

    /// Sleep time as a multiple of time worked: 0.5 holds the walk near a 2/3
    /// duty cycle, 2.0 near 1/3.
    #[cfg(target_os = "macos")]
    fn backoff_ratio() -> f32 {
        match mac::sched::pressure() {
            mac::sched::Pressure::None => 0.0,
            mac::sched::Pressure::Elevated => 0.5,
            mac::sched::Pressure::High => 2.0,
        }
    }

    /// No thermal signal to act on outside macOS.
    #[cfg(not(target_os = "macos"))]
    fn backoff_ratio() -> f32 {
        0.0
    }
}

/// Remove `row`'s path from the direct-children preload map, returning true
/// when it was present AND unchanged (same mtime + size). Paths left in the
/// map after the scan are only reconciliation candidates; callers must confirm
/// policy exclusion or NotFound before deletion. (Whole-subtree diffs use the
/// hash-compacted `rescan::SubtreeDiff` instead; this map holds one level.)
fn row_unchanged(
    existing: &mut HashMap<String, (Option<i64>, Option<i64>)>,
    row: &IndexRow,
) -> bool {
    matches!(
        existing.remove(&row.path),
        Some((old_mtime, old_size)) if old_mtime == row.mtime && old_size == row.size
    )
}

/// Existing DIRECT children of `dir_path` only. The root row is deliberately
/// excluded: it is maintained separately and must never become a subtree
/// deletion candidate when its metadata cannot be read.
fn preload_direct_children(
    conn: &Connection,
    dir_path: &str,
) -> AppResult<HashMap<String, (Option<i64>, Option<i64>)>> {
    let mut map = HashMap::new();
    let mut stmt = conn
        .prepare("SELECT path, mtime, size FROM entries WHERE dir = ?1 AND path <> ?1")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![dir_path], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (path, mtime, size) = row.map_err(|e| e.to_string())?;
        map.insert(path, (mtime, size));
    }
    Ok(map)
}

fn preload_path_signature(
    conn: &Connection,
    path: &str,
) -> AppResult<Option<(Option<i64>, Option<i64>)>> {
    conn.query_row(
        "SELECT mtime, size FROM entries WHERE path = ?1",
        params![path],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(|e| e.to_string())
}

fn filesystem_observation_error(stage: &str, path: &Path, error: &io::Error) -> String {
    format!(
        "filesystem observation failed: stage={stage} path={} kind={:?} os_error={:?}: {error}",
        path.display(),
        error.kind(),
        error.raw_os_error(),
    )
}

struct ObservedDirectChild {
    path: PathBuf,
    metadata: fs::Metadata,
}

/// Obtain a complete direct-child observation. Any open/iterator error, or a
/// metadata error other than a child disappearing mid-iteration, rejects the
/// observation so callers cannot use a partial snapshot for deletion.
fn observe_direct_children<Open, Entries, Metadata, Skip>(
    scan_root: &Path,
    open: Open,
    mut metadata_for: Metadata,
    mut should_skip: Skip,
) -> AppResult<Vec<ObservedDirectChild>>
where
    Open: FnOnce(&Path) -> io::Result<Entries>,
    Entries: IntoIterator<Item = io::Result<PathBuf>>,
    Metadata: FnMut(&Path) -> io::Result<fs::Metadata>,
    Skip: FnMut(&Path) -> bool,
{
    let entries = open(scan_root)
        .map_err(|error| filesystem_observation_error("read_dir_open", scan_root, &error))?;
    let mut observed = Vec::new();
    for entry in entries {
        let child_path = entry
            .map_err(|error| filesystem_observation_error("read_dir_entry", scan_root, &error))?;
        if should_skip(&child_path) {
            continue;
        }
        match metadata_for(&child_path) {
            Ok(metadata) => observed.push(ObservedDirectChild {
                path: child_path,
                metadata,
            }),
            // A path may disappear between readdir and stat. If it existed in
            // the DB it remains a leftover and is re-confirmed below.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(filesystem_observation_error(
                    "direct_child_metadata",
                    &child_path,
                    &error,
                ));
            }
        }
    }
    Ok(observed)
}

struct DirectChildReconciliation {
    delete_paths: Vec<String>,
    present: Vec<ObservedDirectChild>,
}

/// Classify paths left over only after a complete parent enumeration. An
/// unseen path is deletable solely when policy explicitly ignores it or a
/// final metadata probe confirms NotFound. Every other error aborts the
/// reconciliation before any delete is applied.
fn reconcile_unseen_direct_children<Metadata, Skip>(
    existing: &HashMap<String, (Option<i64>, Option<i64>)>,
    mut metadata_for: Metadata,
    mut should_skip: Skip,
) -> AppResult<DirectChildReconciliation>
where
    Metadata: FnMut(&Path) -> io::Result<fs::Metadata>,
    Skip: FnMut(&Path) -> bool,
{
    let mut paths: Vec<&String> = existing.keys().collect();
    paths.sort_unstable();

    let mut delete_paths = Vec::new();
    let mut present = Vec::new();
    for path_str in paths {
        let path = Path::new(path_str);
        if should_skip(path) {
            delete_paths.push(path_str.clone());
            continue;
        }

        match metadata_for(path) {
            Ok(metadata) => present.push(ObservedDirectChild {
                path: path.to_path_buf(),
                metadata,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                delete_paths.push(path_str.clone());
            }
            Err(error) => {
                return Err(filesystem_observation_error(
                    "leftover_metadata",
                    path,
                    &error,
                ));
            }
        }
    }

    Ok(DirectChildReconciliation {
        delete_paths,
        present,
    })
}

fn stage_direct_child(
    child: ObservedDirectChild,
    scan_root: &Path,
    current_run_id: i64,
    existing: &mut HashMap<String, (Option<i64>, Option<i64>)>,
    batch: &mut Vec<IndexRow>,
    priority_roots: &mut Vec<PathBuf>,
    deferred_roots: &mut Vec<PathBuf>,
) {
    if let Some(mut row) = index_row_from_path_and_metadata(&child.path, &child.metadata) {
        row.run_id = current_run_id;
        if !row_unchanged(existing, &row) {
            batch.push(row);
        }
    }

    // Preserve the previous behavior for a direct symlink to a directory:
    // jwalk may use it as a root while still refusing nested symlink traversal.
    let is_dir =
        child.metadata.is_dir() || (child.metadata.file_type().is_symlink() && child.path.is_dir());
    if is_dir {
        if is_deferred_dir(&child.path, scan_root) {
            deferred_roots.push(child.path);
        } else {
            priority_roots.push(child.path);
        }
    }
}

/// Cores a [`ScanRole::Walk`] thread can actually run on: the efficiency cluster
/// on Apple Silicon, and half the machine anywhere else (the old heuristic, kept
/// for single-cluster hardware where there is no narrower budget to read).
fn walk_core_budget(n_cpus: usize) -> usize {
    #[cfg(target_os = "macos")]
    if let Some(efficiency) = mac::sched::efficiency_cores() {
        return efficiency;
    }
    n_cpus / 2
}

/// Build the shared rayon thread pool for a parallel filesystem scan. Returns
/// the pool, its thread count, and the number of walk workers callers should
/// spawn against it. Shared by the fresh and catchup scan branches.
///
/// This used to oversubscribe cores 2x on the theory that scanning is I/O-bound
/// and `stat()` blocks on disk. On an SSD that premise does not hold: `readdir`
/// + `lstat` against APFS is dominated by in-kernel b-tree work, so the extra
/// threads bought almost no throughput while keeping every core busy. On an M2
/// Pro the old sizing produced ~20 pool threads plus ~10 walk workers plus the
/// writer, all at the inherited `USER_INTERACTIVE` QoS — 400%+ CPU with the
/// fans audible throughout a first-run index.
///
/// Every thread built here is a [`ScanRole::Walk`] thread and drops to the
/// E-cluster via `start_handler`, the only hook that runs on a rayon worker
/// before it picks up work. That class — not this sizing — is what bounds CPU
/// draw: measured at `BACKGROUND`, going from 3 pool threads to 12 moved avg
/// cores only 2.4 → 3.9 because the kernel confines them to the E-cluster
/// regardless.
///
/// Which is also why the budget below is the *efficiency* core count rather than
/// half the machine: it is the cluster these threads share, so scaling off the
/// whole machine (an M2 Pro would get 5 pool threads from `n_cpus/2`) says
/// nothing about the resource they actually contend for.
///
/// Note that pool threads + walk workers deliberately *exceed* that budget by a
/// little, because a walk thread is blocked in `readdir`/`lstat` most of the
/// time — threads are not running cores. Measured at `BACKGROUND` on a 12-E
/// machine, achieved concurrency by total walk threads: 4 → 2.4 cores, 6 → 3.3,
/// 9 → 3.8, 16 → 3.9. So the workload's own parallelism tops out near 3.9 cores
/// however many threads it gets, and sizing strictly *within* the cluster would
/// leave E-cores idle on I/O without saving any power — the QoS class, not the
/// thread count, is what bounds draw.
pub(crate) fn build_scan_pool() -> (Arc<jwalk::rayon::ThreadPool>, usize, usize) {
    let n_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let (pool_threads, workers) = if index_full_speed() {
        ((n_cpus * 2).max(8), n_cpus)
    } else {
        // Walk work (pool) outweighs row-building work (workers) roughly 2:1.
        let pool = walk_core_budget(n_cpus).clamp(2, 6);
        (pool, (pool / 2).clamp(1, 3))
    };

    // Already hot or saving power: shrink the fan-out rather than add to it.
    let divisor = scan_pressure_divisor();
    let pool_threads = (pool_threads / divisor).max(2);
    let workers = (workers / divisor).max(1);

    let pool = Arc::new(
        jwalk::rayon::ThreadPoolBuilder::new()
            .num_threads(pool_threads)
            .thread_name(|i| format!("everything-scan-{i}"))
            .start_handler(|_| apply_scan_scheduling(ScanRole::Walk))
            .build()
            .expect("failed to build rayon pool for parallel scan"),
    );
    (pool, pool_threads, workers)
}

/// `app: None` runs the index pipeline without UI event emission and without
/// spawning the background finalizing thread (the caller finalizes explicitly)
/// — used by benchmarks/tests to exercise the real indexing path.
pub(crate) fn run_incremental_index(app: Option<&EventSink>, state: &AppState) -> AppResult<()> {
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
    // One line per pass boundary in the diag log (start + ok/FAILED below):
    // enough to reconstruct pass frequency and duration after a field
    // incident, without perf_log's env-gated verbosity.
    diag::log(format!(
        "index pass start (fresh={is_fresh_run} fts_dirty={fts_dirty} extra_roots={})",
        state.extra_roots.lock().len()
    ));
    // Pass-boundary CPU accounting: the ok/FAILED line below carries what the
    // pass cost (CPU per QoS class + joules), so a field log answers "was it
    // the indexer, and was it on the right cluster" without a reproduction.
    #[cfg(target_os = "macos")]
    let cpu_pass_start = mac::cpu_monitor::Snapshot::take();
    // This thread is the pass's SQLite writer for the whole run. A fresh pass's
    // wall time is essentially this thread (~1M inserts), so it keeps a
    // performance core while the walk fan-out below runs on the E-cluster.
    apply_scan_scheduling(if is_fresh_run {
        ScanRole::FreshWriter
    } else {
        ScanRole::IdleWriter
    });
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
    #[cfg(target_os = "macos")]
    let cpu_note = mac::cpu_monitor::pass_summary(cpu_pass_start);
    #[cfg(not(target_os = "macos"))]
    let cpu_note = String::new();

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
    eprintln!(
        "[timing] checkpoint {}ms",
        t_checkpoint.elapsed().as_millis()
    );

    match &result {
        Ok(_) => {
            let _ = set_meta(&conn, "index_complete", "1");
            let extra_roots = state.extra_roots.lock().clone();
            let roots_str: Vec<String> = extra_roots
                .iter()
                .map(|r| r.to_string_lossy().to_string())
                .collect();
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
                    // Single-threaded, but CREATE INDEX + FTS rebuild + ANALYZE
                    // over ~1M rows is minutes of sustained work; search runs on
                    // LIKE fallbacks until it lands, so this one thread keeps a
                    // performance core.
                    apply_scan_scheduling(ScanRole::FreshWriter);

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
            diag::log(format!(
                "index pass ok in {:.1}s (scanned={} indexed={} entries={} permission_errors={}){cpu_note}",
                started.elapsed().as_secs_f32(),
                snapshot.scanned,
                snapshot.indexed,
                snapshot.entries_count,
                snapshot.permission_errors,
            ));
        }
        Err(err) => {
            perf_log(format!(
                "index_run_error elapsed_ms={} err={}",
                started.elapsed().as_millis(),
                err,
            ));
            diag::log(format!(
                "index pass FAILED after {:.1}s: {err}{cpu_note}",
                started.elapsed().as_secs_f32()
            ));
        }
    }

    result
}

fn run_incremental_index_inner(
    app: Option<&EventSink>,
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
    let mut failures = ScanFailures::default();
    // Shared by the walk workers below: the subtrees this pass could not
    // enumerate, so a later Full Disk Access grant can rescan exactly those
    // instead of repeating the whole pass.
    let denied_prefixes: DeniedPrefixes = Arc::new(Mutex::new(BTreeSet::new()));
    let mut current_path = String::new();
    let mut batch: Vec<IndexRow> = Vec::with_capacity(flush_batch_size);
    // Paths removed because they vanished from disk (catchup set-difference).
    let mut catchup_deleted: u64 = 0;
    let mut last_emit = Instant::now();
    let mut last_perf_emit = Instant::now();

    // Preload scan_root-level entries (direct children only, not recursive).
    // The scan root itself is loaded separately so it can never become a
    // subtree-deletion candidate when its metadata is temporarily unreadable.
    let scan_str = state.scan_root.to_string_lossy().to_string();
    let mut root_existing = preload_direct_children(conn, &scan_str)?;
    let root_signature = preload_path_signature(conn, &scan_str)?;

    // Index scan_root itself. Failure aborts this pass before any entry delete
    // is applied; the old root row and every descendant remain searchable.
    let root_metadata = fs::symlink_metadata(&state.scan_root).map_err(|error| {
        filesystem_observation_error("scan_root_metadata", &state.scan_root, &error)
    })?;
    if let Some(mut row) = index_row_from_path_and_metadata(&state.scan_root, &root_metadata) {
        scanned += 1;
        row.run_id = current_run_id;
        let unchanged = matches!(
            root_signature,
            Some((mtime, size)) if mtime == row.mtime && size == row.size
        );
        if !unchanged {
            batch.push(row);
        }
    }

    // Partition direct children into priority vs deferred
    let mut priority_roots: Vec<PathBuf> = Vec::new();
    let mut deferred_roots: Vec<PathBuf> = Vec::new();

    let observed_direct_children = observe_direct_children(
        &state.scan_root,
        |root| {
            fs::read_dir(root).map(|entries| entries.map(|entry| entry.map(|entry| entry.path())))
        },
        |path| fs::symlink_metadata(path),
        |path| should_skip_path(path, &runtime_ignored_roots, &runtime_ignored_patterns),
    )?;
    for child in observed_direct_children {
        scanned += 1;
        current_path = child.path.to_string_lossy().to_string();
        stage_direct_child(
            child,
            &state.scan_root,
            current_run_id,
            &mut root_existing,
            &mut batch,
            &mut priority_roots,
            &mut deferred_roots,
        );
    }

    // A successful parent enumeration only makes leftovers candidates. Each
    // candidate is re-probed, and deletion is limited to explicit ignores or
    // confirmed NotFound. Classification finishes before any delete runs.
    if !is_fresh && !root_existing.is_empty() {
        let reconciliation = reconcile_unseen_direct_children(
            &root_existing,
            |path| fs::symlink_metadata(path),
            |path| should_skip_path(path, &runtime_ignored_roots, &runtime_ignored_patterns),
        )?;

        for child in reconciliation.present {
            scanned += 1;
            current_path = child.path.to_string_lossy().to_string();
            stage_direct_child(
                child,
                &state.scan_root,
                current_run_id,
                &mut root_existing,
                &mut batch,
                &mut priority_roots,
                &mut deferred_roots,
            );
        }

        if !reconciliation.delete_paths.is_empty() {
            if reconciliation.delete_paths.iter().any(|path| {
                path == &scan_str || Path::new(path).parent() != Some(state.scan_root.as_path())
            }) {
                return Err("direct-child deletion escaped the scan-root boundary".to_string());
            }
            catchup_deleted += delete_paths(conn, &reconciliation.delete_paths)? as u64;
        }
    }

    priority_roots.sort();
    priority_roots.dedup();
    deferred_roots.sort();
    deferred_roots.dedup();

    let extra_roots = state.extra_roots.lock().clone();
    let configured_extra_roots: Vec<PathBuf> = extra_roots
        .into_iter()
        .filter(|root| !root.starts_with(&state.scan_root))
        .collect();
    let configured_extra_set: HashSet<String> = configured_extra_roots
        .iter()
        .map(|root| root.to_string_lossy().to_string())
        .collect();
    let mut scanned_extra_roots = Vec::new();
    // Same accumulator the walk workers use, so an unreadable extra root is
    // classified and prefix-recorded by the one code path that does that.
    let mut root_failures =
        WalkFailures::new(Arc::clone(&denied_prefixes), state.scan_root.clone());
    for root in configured_extra_roots {
        // metadata() intentionally follows aliases such as /tmp ->
        // /private/tmp. Any observation error preserves previously indexed
        // rows; only explicit removal from .pathindexing authorizes cleanup.
        match fs::metadata(&root) {
            Ok(metadata) if metadata.is_dir() => scanned_extra_roots.push(root),
            Ok(_) => {
                // A configured root that is not a directory is a `.pathindexing`
                // mistake, not an access failure — but it is still something the
                // user should see rather than a silent no-op.
                root_failures.counts.io += 1;
                eprintln!(
                    "[index] configured extra root is not a directory; preserving existing rows: {}",
                    root.display()
                );
            }
            Err(error) => {
                if root_failures.observe(Some(error.kind()), &root) {
                    eprintln!(
                        "[index] {}; preserving existing extra-root rows",
                        filesystem_observation_error("extra_root_metadata", &root, &error)
                    );
                }
            }
        }
    }
    failures.merge(root_failures.take_counts());
    // Extra roots removed from .pathindexing are intentional policy changes
    // and may be deleted. A configured-but-unreadable root remains in the
    // configured set and is never mistaken for a removed root.
    if !is_fresh {
        if let Some(prev) = get_meta(conn, "indexed_extra_roots") {
            let stale: Vec<String> = prev
                .split('\n')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .filter(|s| {
                    !configured_extra_set.contains(*s)
                        && !Path::new(s).starts_with(&state.scan_root)
                })
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

        // Shared rayon pool: all workers share one background-QoS pool.
        let (shared_pool, pool_threads, max_workers) = build_scan_pool();
        let n_workers = max_workers.min(roots.len().max(1));
        eprintln!(
            "[index] fresh parallel scan: {} workers, {} pool threads, {} roots, {}",
            n_workers,
            pool_threads,
            roots.len(),
            scan_scheduling_label(),
        );

        type ScanMsg = (Vec<IndexRow>, u64, u64, ScanFailures, String);
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
                let cancel = Arc::clone(&state.cancel_index);
                let denied = Arc::clone(&denied_prefixes);
                let scan_root = state.scan_root.clone();

                scope.spawn(move || {
                    apply_scan_scheduling(ScanRole::Walk);
                    let mut throttle = ScanThrottle::new();
                    let mut local_batch: Vec<IndexRow> = Vec::with_capacity(flush_size);
                    let mut local_scanned = 0u64;
                    let mut local_indexed = 0u64;
                    let mut failures = WalkFailures::new(denied, scan_root);
                    let mut local_path = String::new();

                    // Interleaved distribution: worker 0 handles roots[0, n_w, 2*n_w, …]
                    'roots: for i in (worker_idx..roots_ref.len()).step_by(n_w) {
                        let root = &roots_ref[i];
                        let root_started = Instant::now();
                        let mut root_scanned = 0u64;
                        let mut root_indexed = 0u64;
                        failures.start_root();
                        let root_str = root.to_string_lossy().to_string();

                        let s_roots = skip_roots.clone();
                        let s_patterns = skip_patterns.clone();
                        // `should_skip_child` assumes the parent already cleared
                        // `should_skip_path`. jwalk guarantees that for every
                        // subdirectory it descends into, and
                        // `observe_direct_children` guarantees it for the roots
                        // under $HOME — but an extra `.pathindexing` root is
                        // scanned as configured without passing that filter, so
                        // such a root falls back to the full per-entry check.
                        let root_vetted = !should_skip_path(root, &skip_roots, &skip_patterns);
                        let walker = jwalk::WalkDirGeneric::<((), Option<fs::Metadata>)>::new(root)
                            .follow_links(false)
                            .skip_hidden(false)
                            .parallelism(jwalk::Parallelism::RayonExistingPool {
                                pool: pool.clone(),
                                busy_timeout: None,
                            })
                            .process_read_dir(move |_depth, path, _state, children| {
                                // Normalize the parent once per directory
                                // instead of the whole path once per child.
                                let dir_str = normalize_slashes(path.to_string_lossy().to_string());
                                let mut scratch = String::with_capacity(dir_str.len() + 64);
                                children.retain_mut(|entry_result| match entry_result {
                                    Ok(entry) => {
                                        let full_path = path.join(&entry.file_name);
                                        let skip = if root_vetted {
                                            should_skip_child(
                                                &dir_str,
                                                &entry.file_name.to_string_lossy(),
                                                &full_path,
                                                &mut scratch,
                                                &s_roots,
                                                &s_patterns,
                                            )
                                        } else {
                                            should_skip_path(&full_path, &s_roots, &s_patterns)
                                        };
                                        if skip {
                                            return false;
                                        }
                                        entry.client_state = fs::symlink_metadata(&full_path).ok();
                                        true
                                    }
                                    Err(_) => false,
                                });
                            });

                        for result in walker {
                            // Cooperative cancel (daemon yielding to the GUI):
                            // break the whole worker so no further roots are
                            // walked; the pass returns Err and is discarded.
                            // The same coarse counter paces the thermal
                            // duty-cycle check.
                            if local_scanned % 4096 == 0 {
                                if cancel.load(AtomicOrdering::Acquire) {
                                    break 'roots;
                                }
                                throttle.tick();
                            }
                            match result {
                                Ok(entry) => {
                                    let path = entry.path();
                                    if path == root.as_path() {
                                        continue;
                                    }
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    // Overwrite the buffer in place: to_string_lossy
                                    // borrows for valid UTF-8, so after warmup this
                                    // is a memcpy rather than the ~1M short-lived
                                    // Strings the old assignment allocated.
                                    local_path.clear();
                                    local_path.push_str(&path.to_string_lossy());

                                    let metadata = match entry.client_state {
                                        Some(m) => m,
                                        // The `process_read_dir` stat failed. Re-probe
                                        // rather than counting blind: it gives a
                                        // transient failure one cheap retry, and the
                                        // error kind is what separates "vanished
                                        // mid-walk" (normal) from a real denial.
                                        None => match fs::symlink_metadata(&path) {
                                            Ok(metadata) => metadata,
                                            Err(error) => {
                                                failures.observe(Some(error.kind()), &path);
                                                continue;
                                            }
                                        },
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
                                        local_batch.sort_unstable_by(|a, b| a.path.cmp(&b.path));
                                        let _ = tx.send((
                                            std::mem::replace(
                                                &mut local_batch,
                                                Vec::with_capacity(flush_size),
                                            ),
                                            local_scanned,
                                            local_indexed,
                                            failures.take_counts(),
                                            local_path.clone(),
                                        ));
                                        local_scanned = 0;
                                        local_indexed = 0;
                                    }
                                }
                                Err(err) => {
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    let error_path = err.path().unwrap_or(root.as_path());
                                    // False = vanished between its parent's readdir
                                    // and this walk: normal, not the user's problem.
                                    if !failures
                                        .observe(err.io_error().map(|e| e.kind()), error_path)
                                    {
                                        continue;
                                    }
                                    if failures.should_log() {
                                        eprintln!("[index] walk error: {}", err);
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
                            failures.root_errors,
                        );
                    }

                    // Flush partial batch remaining after all roots processed.
                    let counts = failures.take_counts();
                    if !local_batch.is_empty() || local_scanned > 0 || !counts.is_empty() {
                        local_batch.sort_unstable_by(|a, b| a.path.cmp(&b.path));
                        let _ = tx.send((
                            local_batch,
                            local_scanned,
                            local_indexed,
                            counts,
                            local_path,
                        ));
                    }
                });
            }

            // Drop original sender: channel exhausts when all worker clones drop.
            drop(row_tx);

            // Main thread: receive row batches from workers and write to SQLite.
            for (worker_batch, s, i, f, path) in row_rx {
                // On cancel keep draining (so bounded-channel senders never
                // block and the scope can join) but stop writing.
                if state.cancel_index.load(AtomicOrdering::Acquire) {
                    continue;
                }
                scanned += s;
                indexed += i;
                failures.merge(f);
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

        let (shared_pool, pool_threads, max_workers) = build_scan_pool();
        // Each worker holds one root's preload map in memory, so the worker cap
        // also bounds peak memory when several large roots run at once. The 8
        // that used to be spelled out here is now subsumed by build_scan_pool's
        // own ceiling, which is lower still.
        let n_workers = max_workers.min(roots.len().max(1));
        eprintln!(
            "[index] catchup parallel scan: {} workers, {} pool threads, {} roots, {}",
            n_workers,
            pool_threads,
            roots.len(),
            scan_scheduling_label(),
        );

        // (rows to upsert, vanished paths to delete, scanned, indexed, failures, current path)
        type CatchupMsg = (Vec<IndexRow>, Vec<String>, u64, u64, ScanFailures, String);
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
                let cancel = Arc::clone(&state.cancel_index);
                let denied = Arc::clone(&denied_prefixes);
                let scan_root = state.scan_root.clone();

                scope.spawn(move || {
                    apply_scan_scheduling(ScanRole::Walk);
                    let mut throttle = ScanThrottle::new();
                    let worker_conn = db_connection(&worker_db).ok();
                    let mut local_batch: Vec<IndexRow> = Vec::with_capacity(flush_size);
                    let mut local_scanned = 0u64;
                    let mut local_indexed = 0u64;
                    let mut failures = WalkFailures::new(denied, scan_root);
                    let mut local_path = String::new();

                    'roots: for i in (worker_idx..roots_ref.len()).step_by(n_w) {
                        let root = &roots_ref[i];
                        let root_started = Instant::now();
                        let mut root_scanned = 0u64;
                        let mut root_indexed = 0u64;
                        failures.start_root();
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
                        // See the fresh branch: the fast per-child check needs a
                        // parent that already cleared `should_skip_path`.
                        let root_vetted = !should_skip_path(root, &skip_roots, &skip_patterns);
                        let walker = jwalk::WalkDirGeneric::<((), Option<fs::Metadata>)>::new(root)
                            .follow_links(false)
                            .skip_hidden(false)
                            .parallelism(jwalk::Parallelism::RayonExistingPool {
                                pool: pool.clone(),
                                busy_timeout: None,
                            })
                            .process_read_dir(move |_depth, path, _state, children| {
                                let dir_str = normalize_slashes(path.to_string_lossy().to_string());
                                let mut scratch = String::with_capacity(dir_str.len() + 64);
                                children.retain_mut(|entry_result| match entry_result {
                                    Ok(entry) => {
                                        let full_path = path.join(&entry.file_name);
                                        let skip = if root_vetted {
                                            should_skip_child(
                                                &dir_str,
                                                &entry.file_name.to_string_lossy(),
                                                &full_path,
                                                &mut scratch,
                                                &s_roots,
                                                &s_patterns,
                                            )
                                        } else {
                                            should_skip_path(&full_path, &s_roots, &s_patterns)
                                        };
                                        if skip {
                                            return false;
                                        }
                                        entry.client_state = fs::symlink_metadata(&full_path).ok();
                                        true
                                    }
                                    // Preserve read-dir errors so the
                                    // outer iterator can mark their
                                    // prefixes as errored before diffing.
                                    Err(_) => true,
                                });
                            });

                        for result in walker {
                            // Cooperative cancel (daemon yielding to the GUI):
                            // break the whole worker. Breaking 'roots skips this
                            // root's leftover-delete step, so a partially-walked
                            // root never has its unseen rows deleted. The same
                            // coarse counter paces the thermal duty-cycle check.
                            if local_scanned % 4096 == 0 {
                                if cancel.load(AtomicOrdering::Acquire) {
                                    break 'roots;
                                }
                                throttle.tick();
                            }
                            match result {
                                Ok(entry) => {
                                    let path = entry.path();
                                    if path == root.as_path() {
                                        continue;
                                    }
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    // In-place overwrite; see the fresh branch.
                                    local_path.clear();
                                    local_path.push_str(&path.to_string_lossy());

                                    let metadata = match entry.client_state {
                                        Some(m) => m,
                                        None => match fs::symlink_metadata(&path) {
                                            // A transient first failure gets
                                            // one cheap retry before deciding.
                                            Ok(metadata) => metadata,
                                            Err(error)
                                                if error.kind() == io::ErrorKind::NotFound =>
                                            {
                                                // Confirmed vanished: leave it
                                                // in the diff for deletion.
                                                continue;
                                            }
                                            Err(error) => {
                                                failures.observe(Some(error.kind()), &path);
                                                existing.mark_errored(&path);
                                                if failures.should_log() {
                                                    eprintln!(
                                                        "[index] {}; preserving existing subtree",
                                                        filesystem_observation_error(
                                                            "catchup_worker_metadata",
                                                            &path,
                                                            &error,
                                                        )
                                                    );
                                                }
                                                continue;
                                            }
                                        },
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
                                            failures.take_counts(),
                                            local_path.clone(),
                                        ));
                                        local_scanned = 0;
                                        local_indexed = 0;
                                    }
                                }
                                Err(err) => {
                                    local_scanned += 1;
                                    root_scanned += 1;
                                    // Keep rows under an unreadable directory;
                                    // NotFound means it truly vanished and its
                                    // rows should become deletion leftovers.
                                    let io_kind = err.io_error().map(|e| e.kind());
                                    let error_path = err.path().unwrap_or(root.as_path());
                                    existing.observe_walk_error(io_kind, error_path);
                                    if !failures.observe(io_kind, error_path) {
                                        continue;
                                    }
                                    if failures.should_log() {
                                        if let Some(error) = err.io_error() {
                                            eprintln!(
                                                "[index] {}",
                                                filesystem_observation_error(
                                                    "catchup_worker_walk",
                                                    error_path,
                                                    error,
                                                )
                                            );
                                        } else {
                                            eprintln!("[index] catchup worker walk error: {}", err);
                                        }
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
                                let _ = tx.send((
                                    Vec::new(),
                                    chunk,
                                    0,
                                    0,
                                    ScanFailures::default(),
                                    String::new(),
                                ));
                            }
                        }

                        eprintln!(
                            "[timing]   walk_catchup {} {}ms scanned={} indexed={} err={}",
                            root_str,
                            root_started.elapsed().as_millis(),
                            root_scanned,
                            root_indexed,
                            failures.root_errors,
                        );
                    }

                    let counts = failures.take_counts();
                    if !local_batch.is_empty() || local_scanned > 0 || !counts.is_empty() {
                        let _ = tx.send((
                            local_batch,
                            Vec::new(),
                            local_scanned,
                            local_indexed,
                            counts,
                            local_path,
                        ));
                    }
                });
            }

            drop(row_tx);

            for (worker_batch, deletes, s, i, f, path) in row_rx {
                // On cancel keep draining (so bounded-channel senders never
                // block and the scope can join) but stop writing.
                if state.cancel_index.load(AtomicOrdering::Acquire) {
                    continue;
                }
                scanned += s;
                indexed += i;
                failures.merge(f);
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

    // If the daemon asked us to yield (the GUI appeared mid-build), unwind now
    // — before persisting last_run_id / cached counts — so the partial index
    // stays "incomplete" (index_complete unset) and the GUI, or a later daemon
    // pass, finishes it. A fresh run also leaves fts_dirty=1, which self-heals
    // on the next run's finalize.
    if state.cancel_index.load(AtomicOrdering::Acquire) {
        return Err("index pass cancelled (yielding writer to GUI)".to_string());
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

    // Record what this pass could not read, so an FDA grant can rescan exactly
    // those subtrees instead of repeating the whole pass. Written on every full
    // pass (including the empty case) so the list can never outlive the denials
    // it describes.
    {
        let prefixes = denied_prefixes.lock();
        let _ = set_meta_list(conn, DENIED_PREFIXES_META, prefixes.iter());
        if !prefixes.is_empty() {
            diag::log(format!(
                "{} path(s) denied under {} prefix(es): {}",
                failures.denied,
                prefixes.len(),
                prefixes.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }

    let reported_failures = failures.total();
    {
        let mut status = state.status.lock();
        status.permission_errors = reported_failures;
        status.scanned = scanned;
        status.indexed = indexed;
        status.current_path = current_path.clone();
        // `last_fda_ok` is this process's own already-probed answer (the daemon
        // stores it at startup and on the rising edge), so the message needs no
        // second TCC.db read — which also keeps `probe_full_disk_access`'s
        // documented side effect out of a string formatter.
        status.message = scan_failure_message(failures, fda_granted(conn));
    }
    if let Some(app) = app {
        emit_index_progress(app, scanned, indexed, current_path);
        emit_index_updated(app, entries_count, updated_at, reported_failures);
    }
    perf_log(format!(
        "index_scan_done scanned={} indexed={} deleted={} entries={} denied={} io_errors={}",
        scanned, indexed, deleted_count, entries_count, failures.denied, failures.io
    ));

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn trim_recent_ops(ops: &mut Vec<RecentOp>) {
    let now = Instant::now();
    ops.retain(|op| now.duration_since(op.at) <= RECENT_OP_TTL);
}

// The recent-ops cache suppresses the app's own writes from being re-processed
// by its watcher. On Windows and macOS the watcher lives in the indexer
// service/daemon (a separate process) and the GUI never writes, so nothing
// populates this cache there — see `move_to_trash`/`rename`. Linux-only.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
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

#[cfg(target_os = "windows")]
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
/// instead of re-running `COUNT(*)` over the whole table. `rescan_candidates`
/// are freshly inserted directories that have children on disk but no
/// descendants in the DB — subtrees that arrived without per-child events
/// (S5 trigger (b)) and need a streaming subtree rescan.
#[cfg(target_os = "macos")]
struct PathChangeOutcome {
    changed: usize,
    count_delta: i64,
    retry_paths: Vec<PathBuf>,
    rescan_candidates: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
struct PreparedPathChanges {
    to_upsert: Vec<IndexRow>,
    to_delete: Vec<String>,
    retry_paths: Vec<PathBuf>,
}

#[cfg(target_os = "macos")]
fn prepare_path_changes<Metadata>(
    _state: &AppState,
    paths: &[PathBuf],
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
    mut metadata_for: Metadata,
) -> PreparedPathChanges
where
    Metadata: FnMut(&Path) -> io::Result<fs::Metadata>,
{
    let mut to_upsert_map: HashMap<String, IndexRow> = HashMap::new();
    let mut to_delete = Vec::new();
    let mut retry_paths = Vec::new();

    for path in paths {
        if should_skip_path(path, ignored_roots, ignored_patterns) {
            continue;
        }

        // (Pre-flip this also consulted the recent-ops cache; the macOS GUI no
        // longer writes, so the daemon's watcher is the only reflector and
        // every event is authoritative — D4/D7/V1.)
        let path_str = path.to_string_lossy().to_string();
        if path_str.is_empty() {
            continue;
        }

        match metadata_for(path) {
            Ok(metadata) => {
                if let Some(row) = index_row_from_path_and_metadata(path, &metadata) {
                    to_upsert_map.insert(row.path.clone(), row);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                to_delete.push(path_str);
            }
            Err(error) => {
                eprintln!(
                    "[watcher] {}; preserving row and scheduling retry",
                    filesystem_observation_error("path_change_metadata", path, &error)
                );
                retry_paths.push(path.clone());
            }
        }
    }

    PreparedPathChanges {
        to_upsert: to_upsert_map.into_values().collect(),
        to_delete,
        retry_paths,
    }
}

/// Which of `rows` already exist in `entries`, checked in chunks that stay
/// under SQLite's bound-parameter limit. Point lookups on the UNIQUE path
/// index — cheap even for large watcher batches.
#[cfg(target_os = "macos")]
fn select_existing_paths(conn: &Connection, rows: &[IndexRow]) -> AppResult<HashSet<String>> {
    let mut existing = HashSet::new();
    for chunk in rows.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("SELECT path FROM entries WHERE path IN ({placeholders})");
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut found = stmt
            .query(params_from_iter(chunk.iter().map(|r| &r.path)))
            .map_err(|e| e.to_string())?;
        while let Some(row) = found.next().map_err(|e| e.to_string())? {
            existing.insert(row.get::<_, String>(0).map_err(|e| e.to_string())?);
        }
    }
    Ok(existing)
}

/// Cap on per-batch S5 trigger-(b) probes. Mass-created directories (git
/// clone, npm install) deliver their children as their own events, so
/// skipping the tail of a huge batch costs nothing; the cap only bounds the
/// readdir+EXISTS work a single watcher batch can incur.
#[cfg(target_os = "macos")]
const NEW_DIR_RESCAN_PROBE_CAP: usize = 32;

/// S5 trigger (b): among freshly inserted directory rows, find ones that have
/// at least one child on disk but no descendants in the DB — the signature of
/// a subtree that arrived without per-child events (a directory rename the
/// kernel delivered unflagged, or a tree moved in from an unwatched root).
/// Runs after the batch upsert, so children that arrived in the same batch
/// already count as DB descendants and suppress the rescan.
#[cfg(target_os = "macos")]
fn collect_new_dir_rescan_candidates(
    conn: &Connection,
    upserted: &[IndexRow],
    existing: &HashSet<String>,
) -> AppResult<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    let mut probed = 0usize;
    for row in upserted {
        if row.is_dir == 0 || existing.contains(&row.path) {
            continue;
        }
        if probed >= NEW_DIR_RESCAN_PROBE_CAP {
            eprintln!(
                "[watcher] new-dir rescan probe cap ({NEW_DIR_RESCAN_PROBE_CAP}) reached; \
                 remaining new dirs rely on their own child events"
            );
            break;
        }
        probed += 1;
        let path = Path::new(&row.path);
        let has_child_on_disk = fs::read_dir(path)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if !has_child_on_disk {
            continue;
        }
        let (lo, hi) = subtree_range_bounds(&row.path);
        let has_db_descendant: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE path >= ?1 AND path < ?2)",
                params![lo, hi],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if !has_db_descendant {
            candidates.push(path.to_path_buf());
        }
    }
    Ok(candidates)
}

#[cfg(target_os = "macos")]
fn apply_path_changes(
    state: &AppState,
    paths: &[PathBuf],
    collect_rescan_candidates: bool,
) -> AppResult<PathChangeOutcome> {
    let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(state);
    let PreparedPathChanges {
        to_upsert,
        to_delete,
        retry_paths,
    } = prepare_path_changes(state, paths, &ignored_roots, &ignored_patterns, |path| {
        fs::symlink_metadata(path)
    });

    if to_upsert.is_empty() && to_delete.is_empty() {
        return Ok(PathChangeOutcome {
            changed: 0,
            count_delta: 0,
            retry_paths,
            rescan_candidates: Vec::new(),
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
        let existing = select_existing_paths(conn, &to_upsert)?;
        let up = upsert_rows(conn, &to_upsert)?;
        let del = delete_paths(conn, &to_delete)?;
        let rescan_candidates = if collect_rescan_candidates {
            collect_new_dir_rescan_candidates(conn, &to_upsert, &existing)?
        } else {
            Vec::new()
        };
        Ok(PathChangeOutcome {
            changed: up + del,
            count_delta: to_upsert.len() as i64 - existing.len() as i64 - del as i64,
            retry_paths,
            rescan_candidates,
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

/// Consecutive SQLITE_BUSY watcher batches before a diag ALERT: ~30s of
/// blocked writes means another writer (daemon overlap, second instance) is
/// holding the DB — exactly the single-writer violation the handshake is
/// supposed to prevent, and worth a post-mortem record when it happens.
#[cfg(target_os = "macos")]
const WATCHER_BUSY_ALERT_STREAK: u32 = 10;

#[cfg(target_os = "macos")]
const FS_OBSERVATION_RETRY_DELAY: Duration = Duration::from_secs(3);

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn process_watcher_paths(
    app: Option<&EventSink>,
    state: &AppState,
    pending: &mut HashSet<PathBuf>,
    deadline: &mut Option<Instant>,
    last_status_emit: &mut Instant,
    pending_status_emit: &mut bool,
    busy_streak: &mut u32,
    queued_rescans: &mut HashMap<PathBuf, QueuedRescan>,
    finished_rescans: &Mutex<Vec<(PathBuf, Instant, Duration)>>,
    collect_rescan_candidates: bool,
) {
    if pending.is_empty() {
        return;
    }

    if state.indexing_active.load(AtomicOrdering::Acquire) {
        return;
    }

    let mut batch: Vec<PathBuf> = pending.drain().collect();
    batch.sort();

    match apply_path_changes(state, &batch, collect_rescan_candidates) {
        Ok(outcome) => {
            if *busy_streak >= WATCHER_BUSY_ALERT_STREAK {
                diag::log(format!(
                    "watcher writes unblocked after {} busy retries",
                    *busy_streak
                ));
            }
            *busy_streak = 0;
            let PathChangeOutcome {
                changed,
                count_delta,
                retry_paths,
                rescan_candidates,
            } = outcome;
            for dir in rescan_candidates {
                queue_subtree_rescan(
                    dir,
                    RENAME_RESCAN_COOLDOWN,
                    queued_rescans,
                    finished_rescans,
                );
            }
            if retry_paths.is_empty() {
                *deadline = None;
            } else {
                for path in retry_paths {
                    pending.insert(path);
                }
                *deadline = Some(Instant::now() + FS_OBSERVATION_RETRY_DELAY);
            }
            if changed > 0 {
                invalidate_search_caches(state);
                {
                    // Maintain counts incrementally — the rows just written
                    // carry indexed_at = now, and count_delta is the
                    // exact net row change, so no COUNT(*)/MAX() needed.
                    let mut status = state.status.lock();
                    status.entries_count =
                        (status.entries_count as i64 + count_delta).max(0) as u64;
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
            let is_busy =
                err.contains("locked") || err.contains("busy") || err.contains("unable to open");
            if is_busy {
                *busy_streak += 1;
                if *busy_streak == WATCHER_BUSY_ALERT_STREAK {
                    diag::log(format!(
                        "ALERT: watcher writes blocked ~{}s by another writer (SQLITE_BUSY x{}) — \
                         single-writer violation (daemon/GUI overlap?): {err}",
                        u64::from(WATCHER_BUSY_ALERT_STREAK) * DB_BUSY_RETRY_DELAY.as_secs(),
                        WATCHER_BUSY_ALERT_STREAK
                    ));
                }
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
            *busy_streak = 0;
            eprintln!(
                "[watcher] process_watcher_paths ERROR: {} | batch_size={} indexing_active={}",
                err,
                batch.len(),
                state.indexing_active.load(AtomicOrdering::Acquire)
            );
            diag::log(format!(
                "watcher update error (batch={}): {err}",
                batch.len()
            ));
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
            if let Some(app) = app {
                emit_index_state(app, "Error", Some(format!("Watcher update failed: {err}")));
            }
        }
    }
}

#[cfg(target_os = "macos")]
const EVENT_ID_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(target_os = "macos")]
fn persist_event_id(db_path: &Path, event_id: u64) -> AppResult<()> {
    // Never persist the `kFSEventStreamEventIdSinceNow` sentinel (u64::MAX): a
    // stored MAX makes the next launch replay "since now" (skipping the offline
    // gap) and re-pins the tracker. Its presence means no real event was seen yet.
    if event_id == u64::MAX {
        return Ok(());
    }
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

/// Cooldown class for rename-triggered subtree rescans (S5). Much shorter
/// than the MustScanSubDirs window: a rename rescan must land promptly or the
/// renamed subtree stays unsearchable, and repeated renames of the same tree
/// are the only churn this needs to absorb.
#[cfg(target_os = "macos")]
const RENAME_RESCAN_COOLDOWN: Duration = Duration::from_secs(7);

/// A queued subtree rescan: earliest spawn time plus the cooldown class its
/// completion stamps into the finished history (MustScanSubDirs 300s vs
/// rename 7s). When triggers of both classes overlap, the stricter (longer)
/// class wins so overflow storms stay rate-limited.
#[cfg(target_os = "macos")]
struct QueuedRescan {
    due: Instant,
    cooldown: Duration,
}

/// Watch roots for the FSEvents stream: `$HOME` plus canonicalized
/// `.pathindexing` extra roots (FSEvents needs real paths — `/tmp` is a
/// symlink to `/private/tmp`). Also returns (canonical → stored) prefix pairs
/// so event paths can be mapped back to the form rows are stored under.
#[cfg(target_os = "macos")]
fn fsevent_watch_roots(state: &AppState) -> (Vec<PathBuf>, Vec<(PathBuf, PathBuf)>) {
    // The home root gets the same canonicalize+remap treatment as extra
    // roots: rows are stored under the configured form, but FSEvents always
    // delivers canonical paths — with a symlinked $HOME (rare, but real: NFS
    // homes, test rigs under /tmp) unmapped events would upsert duplicate
    // canonical-form rows and range-deletes would miss the stored ones.
    let home_canonical =
        fs::canonicalize(&state.home_dir).unwrap_or_else(|_| state.home_dir.clone());
    let mut roots = vec![home_canonical.clone()];
    let mut remaps: Vec<(PathBuf, PathBuf)> = Vec::new();
    if home_canonical != state.home_dir {
        remaps.push((home_canonical.clone(), state.home_dir.clone()));
    }
    for stored in state.extra_roots.lock().iter() {
        let canonical = fs::canonicalize(stored).unwrap_or_else(|_| stored.clone());
        // The remap is independent of watch selection: even a root watched by
        // proxy (canonical form inside $HOME or duplicating another root)
        // still delivers canonical event paths that must translate back.
        if canonical != *stored {
            remaps.push((canonical.clone(), stored.clone()));
        }
        if canonical.starts_with(&home_canonical) || roots.contains(&canonical) {
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
        FsEvent::Paths(events) => FsEvent::Paths(
            events
                .into_iter()
                .map(|mut ev| {
                    ev.path = remap_event_path(ev.path, remaps);
                    ev
                })
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

/// Earliest instant `path` may be rescanned, given each overlapping finished
/// rescan's own cooldown class. `None` when nothing overlaps — callers must
/// treat that as "no constraint", NOT as "now": returning a freshly taken
/// `Instant::now()` here made the spawn-time recheck (`not_before > now`,
/// with `now` captured a few µs earlier) defer every queued rescan forever.
#[cfg(target_os = "macos")]
fn rescan_not_before(
    path: &Path,
    finished: &Mutex<Vec<(PathBuf, Instant, Duration)>>,
) -> Option<Instant> {
    let mut not_before: Option<Instant> = None;
    for (done, at, cooldown) in finished.lock().iter() {
        if path.starts_with(done) || done.starts_with(path) {
            let earliest = *at + *cooldown;
            if not_before.map_or(true, |nb| earliest > nb) {
                not_before = Some(earliest);
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
/// once it completes. A covering entry absorbs the strictest (longest)
/// cooldown class among the triggers it subsumes.
#[cfg(target_os = "macos")]
fn queue_subtree_rescan(
    path: PathBuf,
    cooldown: Duration,
    queued: &mut HashMap<PathBuf, QueuedRescan>,
    finished: &Mutex<Vec<(PathBuf, Instant, Duration)>>,
) {
    // An equal or ancestor rescan is already queued: it covers this subtree.
    if let Some(entry) = queued
        .iter_mut()
        .find_map(|(q, entry)| path.starts_with(q).then_some(entry))
    {
        entry.cooldown = entry.cooldown.max(cooldown);
        return;
    }
    let due = rescan_not_before(&path, finished).unwrap_or_else(Instant::now);
    // Queued descendants are covered by this wider rescan.
    let mut cooldown = cooldown;
    queued.retain(|q, entry| {
        if q.starts_with(&path) {
            cooldown = cooldown.max(entry.cooldown);
            false
        } else {
            true
        }
    });
    eprintln!(
        "[watcher] subtree rescan queued: {} (cooldown {}s, due in {}ms)",
        path.display(),
        cooldown.as_secs(),
        due.saturating_duration_since(Instant::now()).as_millis()
    );
    queued.insert(path, QueuedRescan { due, cooldown });
}

/// Spawn the next due queued subtree rescan on a background thread
/// (single-flight via `inflight`): a rescan can walk millions of entries over
/// minutes and must not block the watcher loop from draining events. Skipped
/// while an index pass is active — the pass reconciles the same ground — and
/// retried on the next loop tick.
#[cfg(target_os = "macos")]
fn spawn_due_subtree_rescan(
    app: Option<&EventSink>,
    state: &AppState,
    queued: &mut HashMap<PathBuf, QueuedRescan>,
    inflight: &Arc<AtomicBool>,
    finished: &Arc<Mutex<Vec<(PathBuf, Instant, Duration)>>>,
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
        .find(|(_, entry)| entry.due <= now)
        .map(|(path, _)| path.clone())
    else {
        return;
    };
    // The stored due time is a lower bound; re-check the cooldown now that an
    // overlapping rescan may have finished since this path was queued (e.g. the
    // just-completed inflight rescan of the same subtree). If still cooling
    // down, push the due time out and leave it queued for a later tick.
    if let Some(not_before) = rescan_not_before(&path, finished) {
        if not_before > now {
            if let Some(entry) = queued.get_mut(&path) {
                entry.due = not_before;
            }
            return;
        }
    }
    let cooldown = queued
        .remove(&path)
        .map(|entry| entry.cooldown)
        .unwrap_or(RESCAN_COOLDOWN);
    inflight.store(true, AtomicOrdering::Release);

    let app = app.cloned();
    let state = state.clone();
    let inflight = Arc::clone(inflight);
    let finished = Arc::clone(finished);
    std::thread::spawn(move || {
        // Single-threaded, but a MustScanSubDirs overflow can hand this all of
        // `$HOME` — minutes of walking and writing that nobody is waiting on.
        // Without this it would inherit the watcher thread's foreground class.
        apply_scan_scheduling(ScanRole::IdleWriter);
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
                    "[watcher] subtree rescan {}: upserted={} deleted={} {}ms",
                    path.display(),
                    upserted,
                    deleted,
                    started.elapsed().as_millis()
                );
                // Routine rescans stay out of the diag log; a minutes-long one
                // (e.g. all of $HOME after a kernel queue overflow) is exactly
                // the kind of unexplained CPU burn worth a post-mortem record.
                if started.elapsed() >= Duration::from_secs(60) {
                    diag::log(format!(
                        "slow subtree rescan: {} took {:.0}s (upserted={upserted} deleted={deleted})",
                        path.display(),
                        started.elapsed().as_secs_f32()
                    ));
                }
                if upserted + deleted > 0 {
                    invalidate_search_caches(&state);
                    touch_status_updated(&state);
                    // Persist the recount even headless (daemon); emit only in GUI.
                    let _ = refresh_and_emit_status_counts(app.as_ref(), &state);
                }
                if upserted + deleted >= BATCH_SIZE {
                    release_memory_to_os();
                }
            }
            Err(err) => {
                eprintln!("[watcher] subtree rescan {} failed: {err}", path.display());
                diag::log(format!(
                    "subtree rescan FAILED for {}: {err}",
                    path.display()
                ));
            }
        }
        let mut finished = finished.lock();
        // Only stamp the cooldown on success: a failed rescan left the subtree
        // unreconciled, so it must not be blocked from a prompt retry.
        if succeeded {
            finished.push((path, Instant::now(), cooldown));
        }
        finished.retain(|(_, at, cooldown)| at.elapsed() < *cooldown);
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
    app: Option<EventSink>,
    state: AppState,
    since_event_id: Option<u64>,
    conditional: bool,
) {
    // Invalidate any previous worker. One may still be alive past a
    // stop-and-restart's bounded wait (blocked in a single loop iteration on
    // e.g. an unresponsive network extra root); once the restart clears
    // `watcher_stop` that worker would otherwise resurrect alongside this one.
    // It exits at its next generation check instead, and no longer touches
    // `watcher_active`.
    let my_gen = state.watcher_gen.fetch_add(1, AtomicOrdering::AcqRel) + 1;
    std::thread::spawn(move || {
        // The daemon starts this from its build thread, whose class the startup
        // pass already latched (`Utility` after a fresh build) — and QoS is
        // inherited through `pthread_create`, so this worker would carry that
        // for the process's life and hand it down to every thread it spawns
        // (subtree rescans, the `.pathindexing` reconcile). Declaring it here
        // fixes the parent instead of each child.
        apply_scan_scheduling(ScanRole::IdleWriter);
        let mut since_event_id = since_event_id;
        let mut replay = conditional;
        // Rescan bookkeeping outlives stream rebuilds: the cooldown history
        // and the single-flight guard for the background rescan thread.
        let rescan_inflight = Arc::new(AtomicBool::new(false));
        let finished_rescans: Arc<Mutex<Vec<(PathBuf, Instant, Duration)>>> =
            Arc::new(Mutex::new(Vec::new()));
        // Queued-but-not-yet-spawned rescans must also outlive stream rebuilds:
        // a .pathindexing edit rebuilds the stream, and a rescan deferred by
        // cooldown or the inflight guard would otherwise be silently dropped.
        let mut queued_rescans: HashMap<PathBuf, QueuedRescan> = HashMap::new();

        // Each pass runs one FSEvents stream; the stream is rebuilt (with
        // event-id continuity) when .pathindexing roots change.
        loop {
            let (tx, rx) = std::sync::mpsc::channel();
            let (watch_roots, remaps) = fsevent_watch_roots(&state);

            let built =
                mac::fsevent_watcher::FsEventWatcher::new(&watch_roots, since_event_id, tx.clone());
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
                    if let Some(app) = app.as_ref() {
                        emit_index_state(
                            app,
                            "Error",
                            Some(format!("FSEvents watcher initialization failed: {err}")),
                        );
                    }
                    // A prior iteration may have set watcher_active on a
                    // successful build; clear it before bailing so reset_index
                    // doesn't spin for its full 5s deadline waiting on a
                    // watcher thread that has already exited. (Skipped when a
                    // newer generation took over — its liveness is not ours.)
                    if state.watcher_gen.load(AtomicOrdering::Acquire) == my_gen {
                        state.watcher_active.store(false, AtomicOrdering::Release);
                    }
                    return;
                }
            };

            state.watcher_active.store(true, AtomicOrdering::Release);

            if replay {
                perf_log("conditional_startup: watcher started, awaiting history replay");
                set_state(&state, IndexState::Ready, None);
                if let Some(app) = app.as_ref() {
                    emit_index_state(app, "Ready", None);
                }
            }

            let exit = run_fsevent_stream(
                app.as_ref(),
                &state,
                my_gen,
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
        if state.watcher_gen.load(AtomicOrdering::Acquire) == my_gen {
            state.watcher_active.store(false, AtomicOrdering::Release);
        }
        eprintln!("[watcher] fsevent watcher stopped");
    });
}

/// Drive one FSEvents stream until shutdown or a watch-roots change.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn run_fsevent_stream(
    app: Option<&EventSink>,
    state: &AppState,
    my_gen: u64,
    rx: &std::sync::mpsc::Receiver<mac::fsevent_watcher::FsEvent>,
    watcher: &mac::fsevent_watcher::FsEventWatcher,
    remaps: &[(PathBuf, PathBuf)],
    replay: bool,
    rescan_inflight: &Arc<AtomicBool>,
    finished_rescans: &Arc<Mutex<Vec<(PathBuf, Instant, Duration)>>>,
    queued_rescans: &mut HashMap<PathBuf, QueuedRescan>,
) -> WatcherExit {
    use std::sync::mpsc::RecvTimeoutError;

    let mut pending_paths: HashSet<PathBuf> = HashSet::new();
    let mut deadline: Option<Instant> = None;
    let mut last_flush = Instant::now();
    let mut last_status_emit = Instant::now();
    let mut pending_status_emit = false;
    // Consecutive SQLITE_BUSY batches (see WATCHER_BUSY_ALERT_STREAK).
    let mut busy_streak: u32 = 0;

    let mut must_scan_count: usize = 0;
    // ≥20 kernel event-queue overflows within 5min is a storm (super-churny
    // watched root, e.g. /private/tmp under heavy tooling) — each one queues a
    // subtree rescan, so record the storm once per window in the diag log.
    let mut must_scan_storm = diag::RateAlert::new(Duration::from_secs(300), 20);
    let mut replay_phase = replay;
    let mut full_scan_triggered = false;
    let mut rebuild_requested = false;

    // Snapshot config file entries at stream start so we only emit
    // pathignore_changed when actual rule entries change (ignores whitespace/comments).
    let mut last_config_entries =
        pathignore_active_entries(&fs::read_to_string(&state.config_file_path).unwrap_or_default());
    let mut last_pathindexing_entries = pathindexing::pathindexing_active_entries(
        &fs::read_to_string(&state.pathindexing_file_path).unwrap_or_default(),
    );

    loop {
        if state.watcher_stop.load(AtomicOrdering::Acquire)
            || state.watcher_gen.load(AtomicOrdering::Acquire) != my_gen
        {
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
            Ok(mac::fsevent_watcher::FsEvent::Paths(events)) => {
                let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(&state);
                let prev_len = pending_paths.len();
                for event in events {
                    let mac::fsevent_watcher::PathEvent {
                        path,
                        renamed,
                        is_dir,
                    } = event;
                    if path == state.config_file_path {
                        let new_entries = pathignore_active_entries(
                            &fs::read_to_string(&state.config_file_path).unwrap_or_default(),
                        );
                        if new_entries != last_config_entries {
                            last_config_entries = new_entries;
                            // Daemon (app=None): its supervise tick applies the
                            // change live (purge + catchup). GUI-writer paths
                            // (bench) keep the restart banner.
                            state.pathignore_dirty.store(true, AtomicOrdering::Release);
                            if let Some(app) = app {
                                app.emit("pathignore_changed", ()).ok();
                            }
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
                            let new_roots = pathindexing::load_pathindexing_roots(
                                &state.pathindexing_file_path,
                            );
                            last_pathindexing_entries = new_entries;

                            let bg_state = state.clone();
                            let bg_app = app.cloned();
                            let bg_new_roots = new_roots.clone();
                            *state.extra_roots.lock() = new_roots;
                            // Roots changed: rebuild the FSEvents stream so
                            // the new extra roots are watched live.
                            rebuild_requested = true;
                            if state
                                .pathindexing_active
                                .compare_exchange(
                                    false,
                                    true,
                                    AtomicOrdering::AcqRel,
                                    AtomicOrdering::Acquire,
                                )
                                .is_err()
                            {
                                eprintln!("[pathindexing] scan already in progress, skipping");
                                continue;
                            }
                            set_state(&state, IndexState::Indexing, None);
                            if let Some(app) = app {
                                emit_index_state(app, "Indexing", None);
                            }
                            std::thread::spawn(move || {
                                // Wait for initial indexing to finish before writing to DB
                                while bg_state.indexing_active.load(AtomicOrdering::Acquire) {
                                    std::thread::sleep(Duration::from_secs(1));
                                    eprintln!("[pathindexing] waiting for indexing to finish...");
                                }
                                eprintln!("[pathindexing] background scan starting...");
                                let mut prev_roots = old_roots;
                                let mut target_roots = bg_new_roots;
                                let scan_started = Instant::now();
                                diag::log(format!(
                                    "pathindexing changed: extra-root scan start ({} -> {} roots; \
                                     added roots are walked and re-upserted in full)",
                                    prev_roots.len(),
                                    target_roots.len()
                                ));
                                loop {
                                    let (ign_roots, ign_patterns) =
                                        cached_effective_ignore_rules(&bg_state);
                                    match pathindexing::handle_pathindexing_change(
                                        &bg_state,
                                        &prev_roots,
                                        &target_roots,
                                        &ign_roots,
                                        &ign_patterns,
                                    ) {
                                        Ok(()) => {
                                            eprintln!("[pathindexing] background scan done");
                                            diag::log(format!(
                                                "pathindexing change scan done in {:.0}s",
                                                scan_started.elapsed().as_secs_f32()
                                            ));
                                            if let Ok(c) = db_connection(&bg_state.db_path) {
                                                let roots_str: Vec<String> = target_roots
                                                    .iter()
                                                    .map(|r| r.to_string_lossy().to_string())
                                                    .collect();
                                                let _ = set_meta(
                                                    &c,
                                                    "indexed_extra_roots",
                                                    &roots_str.join("\n"),
                                                );
                                            }
                                            let _ = refresh_and_emit_status_counts(
                                                bg_app.as_ref(),
                                                &bg_state,
                                            );
                                        }
                                        Err(e) => {
                                            eprintln!("[pathindexing] change handling error: {e}");
                                            diag::log(format!(
                                                "pathindexing change scan FAILED after {:.0}s: {e}",
                                                scan_started.elapsed().as_secs_f32()
                                            ));
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
                                if let Some(bg_app) = &bg_app {
                                    emit_index_state(bg_app, "Ready", None);
                                    bg_app.emit("pathindexing_changed", ()).ok();
                                }
                                bg_state
                                    .pathindexing_active
                                    .store(false, AtomicOrdering::Release);
                            });
                        }
                        continue;
                    }
                    if should_skip_path(&path, &ignored_roots, &ignored_patterns) {
                        continue;
                    }
                    // S5 trigger (a): an external directory rename delivers
                    // only flagged events for the old and new dir paths — no
                    // per-child events — so the new subtree must be walked
                    // explicitly or it stays missing until the next full
                    // catchup (the old subtree is range-deleted by the normal
                    // batch when its path probes NotFound). Only the surviving
                    // (new) path still exists as a directory on disk.
                    if renamed
                        && is_dir
                        && fs::symlink_metadata(&path)
                            .map(|m| m.file_type().is_dir())
                            .unwrap_or(false)
                    {
                        queue_subtree_rescan(
                            path.clone(),
                            RENAME_RESCAN_COOLDOWN,
                            queued_rescans,
                            finished_rescans,
                        );
                    }
                    pending_paths.insert(path);
                }
                if pending_paths.len() > prev_len {
                    deadline = Some(Instant::now() + WATCH_DEBOUNCE);
                }
            }
            Ok(mac::fsevent_watcher::FsEvent::MustScanSubDirs(path)) => {
                must_scan_count += 1;
                if let Some(n) = must_scan_storm.record() {
                    diag::log(format!(
                        "ALERT: FSEvents overflow storm — {n} MustScanSubDirs events within \
                         5min (last={}); subtree rescans will churn until the source calms down",
                        path.display()
                    ));
                }
                if replay_phase && must_scan_count >= MUST_SCAN_THRESHOLD && !full_scan_triggered {
                    perf_log(format!(
                        "conditional_startup: MustScanSubDirs={} >= threshold, triggering full scan",
                        must_scan_count
                    ));
                    diag::log(format!(
                        "startup replay: MustScanSubDirs={must_scan_count} ≥ {MUST_SCAN_THRESHOLD} \
                         → escalating to a full index pass"
                    ));
                    full_scan_triggered = true;
                    if let Some(app) = app {
                        let _ = start_full_index_worker(app.clone(), state.clone());
                    }
                }
                // Dropped events mean this subtree must be reconciled with
                // disk. Queue a streaming rescan (spawned from the loop
                // tail): bounded memory, change-detected, and rate-limited.
                queue_subtree_rescan(path, RESCAN_COOLDOWN, queued_rescans, finished_rescans);
            }
            Ok(mac::fsevent_watcher::FsEvent::HistoryDone) => {
                eprintln!(
                    "[watcher] HistoryDone: pending_paths={} indexing_active={}",
                    pending_paths.len(),
                    state.indexing_active.load(AtomicOrdering::Acquire)
                );
                process_watcher_paths(
                    app,
                    &state,
                    &mut pending_paths,
                    &mut deadline,
                    &mut last_status_emit,
                    &mut pending_status_emit,
                    &mut busy_streak,
                    queued_rescans,
                    finished_rescans,
                    !replay_phase,
                );
                if replay_phase {
                    replay_phase = false;
                    if !full_scan_triggered {
                        perf_log(format!(
                            "conditional_startup: HistoryDone, MustScanSubDirs={}, skipping full scan",
                            must_scan_count
                        ));
                        let _ = refresh_and_emit_status_counts(app, &state);
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
                    app,
                    &state,
                    &mut pending_paths,
                    &mut deadline,
                    &mut last_status_emit,
                    &mut pending_status_emit,
                    &mut busy_streak,
                    queued_rescans,
                    finished_rescans,
                    !replay_phase,
                );
            }
        }

        if pending_status_emit && last_status_emit.elapsed() >= STATUS_EMIT_MIN_INTERVAL {
            emit_and_persist_cached_counts(app, &state);
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
                app,
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
        &mut busy_streak,
        queued_rescans,
        finished_rescans,
        !replay_phase,
    );

    if rebuild_requested
        && !state.watcher_stop.load(AtomicOrdering::Acquire)
        && state.watcher_gen.load(AtomicOrdering::Acquire) == my_gen
    {
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

/// What a batch entry renders its icon from: the file itself (bundles and
/// executables carry their own) or its type.
enum IconSource<'a> {
    File(&'a str),
    Type(&'a str),
}

/// Render a whole batch of icons, positionally (`None` where the platform
/// produced nothing).
///
/// macOS renders the batch in **one** `swift -e`. That matters because `swift -e`
/// is not a script interpreter — it runs the Swift frontend and LLVM, about a
/// second of wall time per invocation — so resolving a screen of results one
/// icon at a time piled compiler runs onto a machine that was already busy
/// building the first index. Windows' Shell API is in-process, so there is
/// nothing to amortize and the batch is just a loop.
#[cfg(target_os = "macos")]
fn render_icon_pngs(sources: &[IconSource<'_>]) -> Vec<Option<Vec<u8>>> {
    if sources.is_empty() {
        return Vec::new();
    }

    // Requests ride argv, one `f:`/`t:`-tagged argument each: paths are
    // arbitrary user data that may contain newlines (so a newline-joined env
    // payload could not carry them), and the tag also keeps a path that starts
    // with a dash from being read as a compiler flag by the Swift driver.
    //
    // Icons are drawn into an explicit 32x32 bitmap — 16pt at 2x, the small
    // Retina representation Finder uses. Serializing the NSImage's own
    // tiffRepresentation instead would emit its *largest* representation:
    // 1024x1024, up to 200KB of PNG, to paint a 26px row.
    let script = r#"import AppKit
import Foundation

func renderIcon(_ image: NSImage) -> Data? {
  let sidePixels = 32
  let sidePoints: CGFloat = 16
  guard let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil, pixelsWide: sidePixels, pixelsHigh: sidePixels,
    bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
    colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
  ) else {
    return nil
  }
  rep.size = NSSize(width: sidePoints, height: sidePoints)
  NSGraphicsContext.saveGraphicsState()
  guard let context = NSGraphicsContext(bitmapImageRep: rep) else {
    NSGraphicsContext.restoreGraphicsState()
    return nil
  }
  context.imageInterpolation = .high
  NSGraphicsContext.current = context
  image.draw(
    in: NSRect(x: 0, y: 0, width: sidePoints, height: sidePoints),
    from: .zero, operation: .copy, fraction: 1.0
  )
  NSGraphicsContext.restoreGraphicsState()
  return rep.representation(using: .png, properties: [:])
}

let workspace = NSWorkspace.shared
let out = FileHandle.standardOutput
for spec in CommandLine.arguments.dropFirst() {
  let value = String(spec.dropFirst(2))
  let image = spec.hasPrefix("f:")
    ? workspace.icon(forFile: value)
    : workspace.icon(forFileType: value)
  let payload = renderIcon(image) ?? Data()
  var len = UInt32(payload.count).littleEndian
  out.write(Data(bytes: &len, count: 4))
  out.write(payload)
}
"#;

    let args: Vec<String> = sources
        .iter()
        .map(|source| match source {
            IconSource::File(path) => format!("f:{path}"),
            IconSource::Type(ext) => format!("t:{}", safe_file_type(ext)),
        })
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some(stream) = run_swift_png(script, &args) else {
        return vec![None; sources.len()];
    };
    parse_icon_frames(&stream, sources.len())
}

#[cfg(not(target_os = "macos"))]
fn render_icon_pngs(sources: &[IconSource<'_>]) -> Vec<Option<Vec<u8>>> {
    sources
        .iter()
        .map(|source| match source {
            IconSource::File(path) => load_icon_from_path(path),
            IconSource::Type(ext) => load_system_icon_png(ext),
        })
        .collect()
}

/// Split a batched render's stdout into `count` positional icons.
///
/// Frames are length-prefixed: a little-endian `u32` byte count followed by that
/// many PNG bytes, in request order. A zero-length frame means AppKit had no
/// icon for that request; a truncated stream (subprocess killed mid-write)
/// leaves the tail unfilled.
#[cfg(target_os = "macos")]
fn parse_icon_frames(stream: &[u8], count: usize) -> Vec<Option<Vec<u8>>> {
    let mut icons = Vec::with_capacity(count);
    let mut cursor = 0usize;
    while icons.len() < count {
        let Some(header) = stream.get(cursor..cursor + 4) else {
            break;
        };
        let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        cursor += 4;
        let Some(payload) = len
            .checked_add(cursor)
            .and_then(|end| stream.get(cursor..end))
        else {
            break;
        };
        cursor += len;
        icons.push((!payload.is_empty()).then(|| payload.to_vec()));
    }
    icons.resize(count, None);
    icons
}

#[cfg(target_os = "macos")]
fn run_swift_png(script: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = Command::new("swift");
    cmd.arg("-e").arg(script).args(args);
    // Serialize spawns: each `swift -e` is a full compiler run (~1s of CPU), so
    // overlapping them just multiplies a spike that already competes with
    // indexing. Renders are batched, so this only orders whole batches against
    // each other — never one icon against the next.
    static SPAWN_GUARD: Mutex<()> = Mutex::new(());
    let _guard = SPAWN_GUARD.lock();
    let output = cmd.output().ok()?;
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
fn load_icon_from_path(path: &str) -> Option<Vec<u8>> {
    win::icon::load_icon_png(path)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn load_icon_from_path(_path: &str) -> Option<Vec<u8>> {
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

/// Whether a row's icon should be rendered from the file itself rather than from
/// its type. Windows' Shell API returns better artwork for any real file, so it
/// always consults the path; macOS only does for the bundle types that carry
/// their own icon, and other platforms have no per-file source at all.
#[cfg(target_os = "windows")]
fn renders_from_file(_ext: &str) -> bool {
    true
}

#[cfg(target_os = "macos")]
fn renders_from_file(ext: &str) -> bool {
    is_per_file_icon_ext(ext)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn renders_from_file(_ext: &str) -> bool {
    false
}

fn get_index_status(state: &AppState) -> IndexStatusDto {
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

fn get_home_dir(state: &AppState) -> String {
    state.home_dir.to_string_lossy().to_string()
}

fn check_full_disk_access() -> bool {
    #[cfg(target_os = "macos")]
    {
        // D6: a real 1-byte read of TCC.db — fs::metadata can succeed without
        // FDA. The failed attempt is also what lists the app (toggled off) in
        // the FDA settings pane, so the user finds it there during onboarding.
        mac::probe_full_disk_access()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn open_privacy_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .spawn();
    }
}

/// System Settings → Login Items — the `approval_required` banner action.
fn open_login_items_settings() {
    #[cfg(target_os = "macos")]
    mac::launch_agent::open_login_items_settings();
}

/// Everything the onboarding modal / auto-start toggle needs in one snapshot
/// (macOS; other platforms report a completed state so no UI shows).
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct OnboardingStateDto {
    recorded: bool,
    decision: Option<String>,
    fda_ok: bool,
    opted_out: bool,
    /// enabled | requires_approval | not_registered | not_found | unsupported
    registration: String,
    daemon_state: Option<String>,
    daemon_degraded: bool,
}

async fn get_onboarding_state(state: &AppState) -> AppResult<OnboardingStateDto> {
    #[cfg(target_os = "macos")]
    {
        use mac::launch_agent::{self, RegState};
        let data_dir = daemon_data_dir(&state);
        let db_path = state.db_path.clone();
        // Everything below blocks: the UDS status round-trip has 5s read/write
        // timeouts, each `defaults` read is a subprocess, the FDA probe touches
        // TCC.db, and agent_status is an XPC call. The frontend calls this in
        // onMount and after every onboarding/toggle action, so keep it off the
        // main thread (a wedged daemon would otherwise freeze the window).
        runtime::spawn_blocking(move || {
            let daemon = daemon_ipc::status(&data_dir);
            OnboardingStateDto {
                // §7: adopt an existing index as granted-implicit here too —
                // on a first post-upgrade launch no daemon may be running yet,
                // and existing installs must never see the onboarding modal.
                recorded: daemon::onboarding_recorded_or_adopt(&db_path),
                decision: mac::prefs::onboarding_decision(),
                fda_ok: mac::probe_full_disk_access(),
                opted_out: mac::prefs::opted_out(),
                registration: match launch_agent::agent_status() {
                    RegState::Enabled => "enabled",
                    RegState::RequiresApproval => "requires_approval",
                    RegState::NotRegistered => "not_registered",
                    RegState::NotFound => "not_found",
                    RegState::Unsupported => "unsupported",
                }
                .to_string(),
                daemon_state: daemon.as_ref().map(|s| s.state.clone()),
                daemon_degraded: daemon.map(|s| s.degraded || !s.fda_ok).unwrap_or(false),
            }
        })
        .await
        .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state;
        Ok(OnboardingStateDto {
            recorded: true,
            fda_ok: true,
            registration: "unsupported".to_string(),
            ..Default::default()
        })
    }
}

/// Record the first-run onboarding decision ("granted" | "skipped") and kick
/// the registration self-heal — the recorded decision releases the daemon's
/// gate, and the first registration starts it immediately (G3).
async fn record_onboarding_decision(decision: String, state: &AppState) -> AppResult<()> {
    #[cfg(target_os = "macos")]
    {
        if decision != "granted" && decision != "skipped" {
            return Err(format!("invalid onboarding decision: {decision:?}"));
        }
        let data_dir = daemon_data_dir(&state);
        // The persist is a `defaults write` subprocess — off the main thread.
        // The registration self-heal (XPC, can start the daemon — G3) stays on
        // its own thread so the modal button doesn't wait on it.
        runtime::spawn_blocking(move || {
            if !mac::prefs::record_onboarding_decision(&decision) {
                return Err("failed to persist the onboarding decision".to_string());
            }
            std::thread::spawn(move || {
                let outcome = mac::launch_agent::self_heal(&data_dir);
                diag::log(format!(
                    "onboarding: decision recorded ({decision}); registration self-heal: {outcome:?}"
                ));
            });
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (decision, state);
        Ok(())
    }
}

/// The `not_registered` banner action: register the login-item agent now
/// (starts the daemon immediately — G3).
async fn register_daemon(state: &AppState) -> AppResult<()> {
    #[cfg(target_os = "macos")]
    {
        let data_dir = daemon_data_dir(&state);
        // `defaults` subprocesses + the SMAppService XPC registration — off
        // the main thread.
        runtime::spawn_blocking(move || {
            // The gate needs a decision on record before a daemon may build; a
            // user registering from the banner without onboarding effectively
            // skipped it (FDA state is whatever it is — the probe decides).
            if !mac::prefs::onboarding_recorded() {
                mac::prefs::record_onboarding_decision("skipped");
            }
            mac::prefs::set_opt_out(false);
            mac::launch_agent::register()?;
            mac::launch_agent::write_marker(&data_dir);
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = state;
        Ok(())
    }
}

/// The auto-start toggle (§5.2). OFF: record opt-out FIRST (the start gate's
/// final defense), stop the daemon (dormant), unregister. ON (G5): clear the
/// opt-out, clear any lease/unmanaged daemon holding the lock, then
/// kickstart/register — the user sees the daemon come up immediately.
async fn set_daemon_autostart(enabled: bool, state: &AppState) -> AppResult<()> {
    #[cfg(target_os = "macos")]
    {
        use mac::launch_agent::{self, RegState};
        let data_dir = daemon_data_dir(&state);
        let db_path = state.db_path.clone();
        runtime::spawn_blocking(move || {
            if enabled {
                mac::prefs::set_opt_out(false);
                // A lease/unmanaged daemon may hold daemon.lock; ask it to
                // stand down so the launchd instance wins the lock (G5).
                let _ = daemon_ipc::disable(&data_dir);
                let deadline = std::time::Instant::now() + Duration::from_secs(6);
                while daemon::daemon_running(&db_path) && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                }
                match launch_agent::agent_status() {
                    RegState::Enabled => launch_agent::kickstart(),
                    RegState::Unsupported => {
                        // Dev build: no registration possible — plain revival.
                        daemon::ensure_daemon_running("toggle-on");
                        Ok(())
                    }
                    _ => {
                        launch_agent::register()?;
                        launch_agent::write_marker(&data_dir);
                        Ok(())
                    }
                }
            } else {
                if !mac::prefs::set_opt_out(true) {
                    return Err("failed to persist the auto-start opt-out".to_string());
                }
                let _ = daemon_ipc::disable(&data_dir);
                if launch_agent::agent_status() == RegState::Enabled {
                    // Best-effort: the opt-out record + start gate already
                    // guarantee no full daemon returns even if this fails.
                    if let Err(e) = launch_agent::unregister() {
                        diag::log(format!(
                            "toggle-off: unregister failed (gate still holds): {e}"
                        ));
                    }
                }
                Ok(())
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (enabled, state);
        Ok(())
    }
}

fn open_pathignore(state: &AppState) -> AppResult<()> {
    let path = &state.config_file_path;
    ensure_pathignore_exists(path)?;
    #[cfg(target_os = "macos")]
    Command::new("open")
        .arg(path)
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    Command::new("cmd")
        .args(["/C", "start", "", &path.to_string_lossy().to_string()])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW: no console flash on Win11
        .spawn()
        .map_err(|e| e.to_string())?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = path;
    Ok(())
}

fn open_pathindexing(state: &AppState) -> AppResult<()> {
    pathindexing::open_pathindexing_file(&state.pathindexing_file_path)
}

fn restart_app(app: EventSink) {
    app.restart();
}

/// Relaunch `--register-service` elevated (UAC) — the GUI banner action for a
/// missing/broken indexer service (D5). No-op off Windows.
fn register_service_elevated() -> AppResult<()> {
    #[cfg(target_os = "windows")]
    {
        win::register_service_elevated()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(())
    }
}

async fn start_full_index(app: EventSink, state: &AppState) -> AppResult<()> {
    // macOS read-only GUI (D4): a full re-index is the daemon's job — mapped
    // to its reset_index (G15), same as the Windows service model. The UDS
    // call blocks (5s read/write timeouts), so it runs off the async runtime,
    // mirroring the reset_index command.
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        let data_dir = daemon_data_dir(&state);
        return runtime::spawn_blocking(move || daemon_ipc::reset_index(&data_dir))
            .await
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        if !state.is_writer.load(AtomicOrdering::Acquire) {
            return Err(
                "Another Everything instance owns the index; this window is read-only.".to_string(),
            );
        }
        #[cfg(target_os = "windows")]
        {
            // Read-only GUI (§3.3): a full (re)index is the service's job —
            // delegate, off the runtime (the control-pipe call blocks with no
            // client-side read timeout).
            let _ = (app, state);
            runtime::spawn_blocking(win::service_client::reset_index)
                .await
                .map_err(|e| e.to_string())?
        }
        #[cfg(not(target_os = "windows"))]
        {
            start_full_index_worker(app, state.clone())
        }
    }
}

/// The daemon's data dir (UDS socket + sidecars live next to the DB).
#[cfg(target_os = "macos")]
fn daemon_data_dir(state: &AppState) -> PathBuf {
    state
        .db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn reset_index(app: EventSink, state: &AppState) -> AppResult<()> {
    #[cfg(target_os = "windows")]
    let out: AppResult<()> = {
        // Read-only GUI (§3.3/D3): delegate the re-index to the service, which
        // owns the cooldown and execution. The control-pipe call does blocking
        // I/O with no client-side read timeout, so run it off the async runtime —
        // a stalled service handler would otherwise freeze the GUI's IPC.
        let _ = (app, state);
        runtime::spawn_blocking(win::service_client::reset_index)
            .await
            .map_err(|e| e.to_string())?
    };
    #[cfg(target_os = "macos")]
    let out: AppResult<()> = {
        // Read-only GUI (D4/D5): the daemon owns the reset and its 600s
        // cooldown. Blocking UDS I/O runs off the async runtime, mirroring
        // the Windows arm.
        let _ = app;
        let data_dir = daemon_data_dir(&state);
        runtime::spawn_blocking(move || daemon_ipc::reset_index(&data_dir))
            .await
            .map_err(|e| e.to_string())?
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let out: AppResult<()> = {
        // A second, non-writer GUI instance is read-only — refuse to reset it.
        if !state.is_writer.load(AtomicOrdering::Acquire) {
            return Err(
                "Another Everything instance owns the index; this window is read-only.".to_string(),
            );
        }
        let state = state.clone();
        runtime::spawn_blocking(move || {
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
            reset_index_storage(&conn)?;

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
                start_fsevent_watcher_worker(Some(app), state, None, false);
                Ok(())
            }
        })
        .await
        .map_err(|e| e.to_string())?
    };
    out
}

/// The table-swap core of a reset: rename `entries` aside (O(1), no data
/// movement) → recreate it empty with fresh FTS triggers → FTS rebuild from
/// the empty content table (instant) → wipe `meta` (last_run_id, cached
/// counts, and the fts_dirty=1 set here) so the follow-up pass runs fresh.
/// The renamed `entries_gc_reset` is dropped later by the finalizing thread's
/// GC — DROPping it here would contend with the new indexer for the WAL write
/// lock. Shared by the GUI reset command and the daemon's IPC `reset_index`.
pub(crate) fn reset_index_storage(conn: &Connection) -> AppResult<()> {
    let _ = set_meta(conn, "fts_dirty", "1");
    let _ = conn.execute_batch(DROP_FTS_TRIGGERS_SQL);
    let _ = conn.execute_batch("DROP TABLE IF EXISTS entries_gc_reset;");
    conn.execute_batch("ALTER TABLE entries RENAME TO entries_gc_reset;")
        .map_err(|e| e.to_string())?;
    conn.execute_batch(CREATE_ENTRIES_TABLE_SQL)
        .map_err(|e| e.to_string())?;
    conn.execute_batch(CREATE_FTS_TRIGGERS_SQL)
        .map_err(|e| e.to_string())?;
    let _ = conn.execute_batch(REBUILD_FTS_SQL);
    conn.execute("DELETE FROM meta", [])
        .map_err(|e| e.to_string())?;
    Ok(())
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
                            eprintln!(
                                "[search] idx_entries_name_nocase unavailable, using fallback"
                            );
                            conn.prepare_cached(&prefix_sql_fallback)
                                .map_err(|e| e.to_string())?
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
                        let mut stmt2 = conn
                            .prepare_cached(&phase2_sql)
                            .map_err(|e| e.to_string())?;
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
                    let _ = conn.progress_handler(
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

                    let _ = conn.progress_handler(0, None::<fn() -> bool>);
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
                resolve_dirs_from_db(conn, dir_hint)
            } else {
                resolved_dirs
            };

            if !resolved_dirs.is_empty() {
                let ext_shortcut = extract_ext_from_like(name_like);
                let mut sql_params: Vec<SqlValue> = Vec::new();
                let mut dir_conditions = Vec::new();

                for d in &resolved_dirs {
                    let i = sql_params.len();
                    let (pfx, pfx_end) = subtree_range_bounds(d);
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
                                    params![pfx, dir_like_exact, dir_like_sub, effective_limit],
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
                        let _ = conn.progress_handler(
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

                        let _ = conn.progress_handler(0, None::<fn() -> bool>);

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
                mi,
                &query,
                &mode,
                effective_limit,
                offset,
                &sort_by,
                &sort_dir,
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
            {
                db_unavailable = false;
            }
            let outcome = run_db_search(
                &conn,
                &state.home_dir,
                fts_ready,
                &mode,
                &query,
                effective_limit,
                offset,
                &sort_by,
                &sort_dir,
            );
            // S2 (macOS, read-only GUI): the daemon's VACUUM / FTS rebuild can
            // surface as SQLITE_CORRUPT ("database disk image is malformed")
            // on a long-lived pooled reader — a transient illusion, not real
            // corruption. Discard every pooled connection and retry once on a
            // fresh one (MCP precedent: mcp_server.rs fresh-conn-per-attempt).
            // Hot-WAL open failures land in the Err(_) arm below instead:
            // Spotlight covers the gap and the bridge revives the daemon.
            #[cfg(target_os = "macos")]
            let outcome = match outcome {
                Err(e) if e.contains("malformed") => {
                    perf_log(format!("search_corrupt_retry query={query:?} err={e}"));
                    drop(conn);
                    state.search_conn_pool.lock().clear();
                    match pooled_search_connection(state) {
                        Ok(fresh) => run_db_search(
                            &fresh,
                            &state.home_dir,
                            fts_ready,
                            &mode,
                            &query,
                            effective_limit,
                            offset,
                            &sort_by,
                            &sort_dir,
                        ),
                        Err(open_err) => Err(open_err),
                    }
                }
                other => other,
            };
            results = outcome?;

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
            {
                db_unavailable = true;
            }
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

async fn search(
    _app: EventSink,
    query: String,
    limit: Option<u32>,
    offset: Option<u32>,
    sort_by: Option<String>,
    sort_dir: Option<String>,
    include_total: Option<bool>,
    state: &AppState,
) -> AppResult<SearchResultDto> {
    let state = state.clone();
    runtime::spawn_blocking(move || {
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

async fn quick_look(path: String) -> AppResult<()> {
    runtime::spawn_blocking(move || {
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

async fn open(paths: Vec<String>) -> AppResult<()> {
    runtime::spawn_blocking(move || {
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
                cmd.raw_arg(format!("/C start \"\" \"{}\"", path.replace('"', "")))
                    .creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash on Win11
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

#[cfg(not(target_os = "windows"))]
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
    win::clipboard::copy_text(text)
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

async fn open_with(path: String) -> AppResult<()> {
    runtime::spawn_blocking(move || reveal_in_finder_impl(vec![path]))
        .await
        .map_err(|e| e.to_string())?
}

async fn reveal_in_finder(paths: Vec<String>) -> AppResult<()> {
    runtime::spawn_blocking(move || reveal_in_finder_impl(paths))
        .await
        .map_err(|e| e.to_string())?
}

fn copy_paths(paths: Vec<String>) -> AppResult<()> {
    copy_text_to_clipboard(&paths.join("\n"))
}

#[cfg(target_os = "macos")]
fn copy_files(paths: Vec<String>) -> AppResult<()> {
    mac::clipboard::copy_files(&paths)
}

#[cfg(not(target_os = "macos"))]
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
fn has_package_extension(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            PACKAGE_EXTENSIONS
                .iter()
                .any(|pkg| pkg.eq_ignore_ascii_case(e))
        })
}

/// Finder-style "Show Package Contents": browse a package directory (e.g. an
/// .app bundle) as a folder. Plain `open` would launch the bundle and Finder
/// rejects the `folder` coercion for packages, so a new Finder window is
/// pointed at the package root instead; the path travels via argv to avoid
/// AppleScript string escaping.
#[cfg(target_os = "macos")]
async fn show_package_contents(path: String) -> AppResult<()> {
    runtime::spawn_blocking(move || {
        let status = Command::new("osascript")
            .args([
                "-e",
                "on run argv",
                "-e",
                "tell application \"Finder\"",
                "-e",
                "set w to make new Finder window",
                "-e",
                "set target of w to (POSIX file (item 1 of argv) as alias)",
                "-e",
                "activate",
                "-e",
                "end tell",
                "-e",
                "end run",
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
async fn show_package_contents(_path: String) -> AppResult<()> {
    Err("show_package_contents is only supported on macOS".to_string())
}

async fn move_to_trash(
    paths: Vec<String>,
    app: EventSink,
    state: &AppState,
) -> AppResult<file_actions::TrashResult> {
    let state = state.clone();
    runtime::spawn_blocking(move || {
        // Windows/macOS (D4): the indexer service/daemon owns index.db (this
        // process is read-only) and reflects the deletion through its own
        // watcher, so the GUI performs only the filesystem operation and keeps
        // no recent-ops cache. Linux still reflects the change in the DB here.
        let result = file_actions::move_to_trash(&paths);

        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            for path in &result.removed_paths {
                remember_op(&state, "trash", Some(path.clone()), None);
            }
            let mut conn = db_connection(&state.db_path)?;
            let _ = delete_paths(&mut conn, &result.removed_paths)?;
            invalidate_search_caches(&state);
            refresh_and_emit_status_counts(Some(&app), &state)?;
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let _ = (&app, &state);

        Ok(result)
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn get_pending_trash_paths(paths: Vec<String>, state: &AppState) -> AppResult<Vec<String>> {
    let state = state.clone();
    runtime::spawn_blocking(move || {
        let conn = db_connection_for_search(&state.db_path)?;
        file_actions::pending_trash_paths(&conn, &paths)
    })
    .await
    .map_err(|error| error.to_string())?
}

async fn rename(
    path: String,
    new_name: String,
    app: EventSink,
    state: &AppState,
) -> AppResult<EntryDto> {
    let state = state.clone();
    runtime::spawn_blocking(move || {
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

        // Windows/macOS (D4): index.db is owned by the indexer service/daemon
        // and updated via its own watcher — directory subtree fix-ups included
        // (Windows S6 / macOS S5) — so the GUI does no DB reflection and keeps
        // no recent-ops cache. Linux updates the DB in place.
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
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

            refresh_and_emit_status_counts(Some(&app), &state)?;
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        let _ = (&app, &state);

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

async fn fd_search(
    query: String,
    limit: Option<u32>,
    offset: Option<u32>,
    sort_by: Option<String>,
    sort_dir: Option<String>,
    state: &AppState,
) -> AppResult<FdSearchResultDto> {
    let state = state.clone();
    runtime::spawn_blocking(move || {
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

fn frontend_log(msg: String) {
    eprintln!("{msg}");
}

fn mark_frontend_ready(state: &AppState) {
    state.frontend_ready.store(true, AtomicOrdering::Release);
    if cfg!(debug_assertions) {
        eprintln!("[startup] frontend_ready=true");
    }
}

/// One row's icon request. `path` comes along for every row so the formats whose
/// icon lives inside the file (`.app` bundles, `.exe`) can be rendered from it;
/// everything else resolves by extension.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IconRequest {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    ext: String,
}

/// A normalized request: `key` is what `icon_cache` is keyed by, `ext` the type
/// to fall back to when the file itself yields no icon.
struct IconJob {
    key: String,
    ext: String,
    path: Option<String>,
}

fn icon_job(request: &IconRequest) -> IconJob {
    let ext = if request.ext.trim().is_empty() {
        "__default__".to_string()
    } else {
        request.ext.to_lowercase()
    };
    let path = request
        .path
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let key = if is_per_file_icon_ext(&ext) {
        path.clone().unwrap_or_else(|| ext.clone())
    } else {
        ext.clone()
    };
    IconJob { key, ext, path }
}

/// One cache miss and every row waiting on it.
struct IconMiss<'a> {
    job: &'a IconJob,
    rows: Vec<usize>,
    png: Option<Vec<u8>>,
}

/// Resolve a batch of icon requests, rendering everything the cache misses in a
/// single pass — one subprocess on macOS, where a `swift -e` is a full compiler
/// run (roughly a second of CPU) and resolving a screen of results one row at a
/// time therefore cost one process per distinct icon, right while the first
/// index was running.
///
/// Results are positional: `requests[i]` → `icons[i]`. An empty entry means no
/// icon could be produced, and those are deliberately not cached so a later
/// retry can still succeed (e.g. an `.app` bundle still being written).
fn resolve_file_icons(
    cache: &Mutex<HashMap<String, Vec<u8>>>,
    requests: &[IconRequest],
) -> Vec<Vec<u8>> {
    let started = Instant::now();
    let jobs: Vec<IconJob> = requests.iter().map(icon_job).collect();
    let mut icons: Vec<Vec<u8>> = vec![Vec::new(); jobs.len()];

    // Cache misses, deduplicated by key: a screen full of .txt renders once and
    // the result lands on every row that asked for it.
    let mut misses: Vec<IconMiss> = Vec::new();
    {
        let cache = cache.lock();
        let mut slot_of: HashMap<&str, usize> = HashMap::new();
        for (row, job) in jobs.iter().enumerate() {
            if let Some(hit) = cache.get(&job.key) {
                icons[row] = hit.clone();
            } else if let Some(&slot) = slot_of.get(job.key.as_str()) {
                misses[slot].rows.push(row);
            } else {
                slot_of.insert(job.key.as_str(), misses.len());
                misses.push(IconMiss {
                    job,
                    rows: vec![row],
                    png: None,
                });
            }
        }
    }
    if misses.is_empty() {
        return icons;
    }

    // One render for the whole batch: rows whose icon lives in the file render
    // from their path, the rest from their type.
    let sources: Vec<IconSource> = misses
        .iter()
        .map(|miss| match &miss.job.path {
            Some(path) if renders_from_file(&miss.job.ext) => IconSource::File(path),
            _ => IconSource::Type(&miss.job.ext),
        })
        .collect();
    for (miss, png) in misses.iter_mut().zip(render_icon_pngs(&sources)) {
        miss.png = png;
    }
    let from_file = misses
        .iter()
        .zip(&sources)
        .filter(|(miss, source)| miss.png.is_some() && matches!(source, IconSource::File(_)))
        .count();

    // A file that yielded no icon of its own (an .app bundle still being written,
    // say) falls back to its type icon, which is stored under the type key too so
    // the next such file costs nothing. This is rare enough that it is worth a
    // second render rather than pre-rendering every fallback up front.
    let fallbacks: Vec<usize> = (0..misses.len())
        .filter(|&slot| misses[slot].png.is_none() && matches!(sources[slot], IconSource::File(_)))
        .collect();
    if !fallbacks.is_empty() {
        let mut pending: Vec<(&str, Vec<usize>)> = Vec::new();
        {
            let cache = cache.lock();
            let mut slot_of: HashMap<&str, usize> = HashMap::new();
            for slot in fallbacks {
                let ext = misses[slot].job.ext.as_str();
                if let Some(hit) = cache.get(ext) {
                    misses[slot].png = Some(hit.clone());
                } else if let Some(&pos) = slot_of.get(ext) {
                    pending[pos].1.push(slot);
                } else {
                    slot_of.insert(ext, pending.len());
                    pending.push((ext, vec![slot]));
                }
            }
        }
        if !pending.is_empty() {
            let sources: Vec<IconSource> = pending
                .iter()
                .map(|(ext, _)| IconSource::Type(ext))
                .collect();
            let pngs = render_icon_pngs(&sources);
            let mut cache = cache.lock();
            for ((ext, slots), png) in pending.iter().zip(pngs) {
                let Some(png) = png else { continue };
                cache.insert((*ext).to_string(), png.clone());
                for &slot in slots {
                    misses[slot].png = Some(png.clone());
                }
            }
        }
    }

    // A batch only gets here when it had to render, so this stays quiet once the
    // cache is warm — and it reports a batch that resolved nothing, which is the
    // shape a missing Swift toolchain takes in the field.
    eprintln!(
        "[icons] batch: {} row(s), {} miss(es) in {}ms ({} rendered from the file itself, \
         {} unresolved)",
        jobs.len(),
        misses.len(),
        started.elapsed().as_millis(),
        from_file,
        misses.iter().filter(|miss| miss.png.is_none()).count(),
    );

    let mut cache = cache.lock();
    for miss in &misses {
        let Some(png) = &miss.png else { continue };
        cache.insert(miss.job.key.clone(), png.clone());
        for &row in &miss.rows {
            icons[row] = png.clone();
        }
    }
    icons
}

async fn get_file_icons(requests: Vec<IconRequest>, state: &AppState) -> AppResult<Vec<Vec<u8>>> {
    let state = state.clone();
    Ok(
        runtime::spawn_blocking(move || resolve_file_icons(&state.icon_cache, &requests))
            .await
            .unwrap_or_default(),
    )
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

fn start_bench_runner(app_handle: EventSink, state: AppState) {
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

fn setup_native() -> AppResult<(AppState, EventSink, async_channel::Receiver<()>)> {
    let setup_started = std::time::Instant::now();
    eprintln!("[startup] setup_app() entered");
    let bench_mode = bench_mode_enabled();

    let app_data_dir = ui_prefs::app_data_dir();
    if env_truthy("EVERYTHING_UI_TEST") {
        return ui_model::fixture_runtime(&app_data_dir);
    }
    fs::create_dir_all(&app_data_dir).map_err(|e| e.to_string())?;

    // Post-mortem diagnostics (shared with --daemon/--mcp): rebuild loops,
    // writer overlap, and event storms self-report to logs/everything.log —
    // field incidents are otherwise unreproducible, and a Finder-launched
    // process has no visible stderr. (db_path is resolved per-platform below.)
    diag::init(&app_data_dir, "gui");
    // Field CPU accounting: if this process ever burns fan-relevant power
    // (P-band CPU ≥ ~5W sustained), the episode lands in the diag log with a
    // per-QoS-class split, so a "fans spun up" report is diagnosable from the
    // log alone. Quiet otherwise.
    #[cfg(target_os = "macos")]
    mac::cpu_monitor::start_watchdog();
    let home_dir = resolve_home_dir();

    // D3 self-heal: keep the login-item agent registered and its plist marker
    // current. Inert until an onboarding decision exists (recorded by the
    // onboarding flow, or by `--register-daemon` today), and it never
    // overrides the user's opt-out or an OS-side disable. Own thread:
    // SMAppService calls and `defaults` shell-outs must not block window
    // bring-up.
    #[cfg(target_os = "macos")]
    {
        let data_dir = app_data_dir.clone();
        std::thread::spawn(move || {
            use mac::launch_agent::SelfHeal;
            match mac::launch_agent::self_heal(&data_dir) {
                outcome @ (SelfHeal::AlreadyCurrent | SelfHeal::Skipped(_)) => {
                    eprintln!("[launch-agent] self-heal: {outcome:?}");
                }
                outcome @ (SelfHeal::Registered | SelfHeal::Reregistered) => {
                    diag::log(format!("launch-agent self-heal: {outcome:?}"));
                }
                SelfHeal::Failed(e) => {
                    diag::log(format!("ALERT: launch-agent self-heal failed: {e}"));
                }
            }
        });
    }

    // Windows (service model, §3.3): mark this process a pure reader and point
    // it at the machine-global ProgramData index the service writes; the GUI
    // never touches per-user %APPDATA% and never writes. config\ (user-writable)
    // is separate from data\ (Users read-only). Elsewhere: per-user app-data.
    #[cfg(target_os = "windows")]
    let (db_path, state) = {
        set_db_readonly();
        let data_dir = win::service_paths::data_dir().unwrap_or_else(|| app_data_dir.clone());
        let config_dir = win::service_paths::config_dir().unwrap_or_else(|| app_data_dir.clone());
        let _ = fs::create_dir_all(&config_dir);
        let db_path = win::service_paths::db_path().unwrap_or_else(|| data_dir.join(DB_FILE_NAME));
        let state = build_app_state_with_config(db_path.clone(), home_dir, &data_dir, &config_dir);
        (db_path, state)
    };
    #[cfg(not(target_os = "windows"))]
    let (db_path, state) = {
        let db_path = app_data_dir.join(DB_FILE_NAME);
        let state = build_app_state(db_path.clone(), home_dir, &app_data_dir);
        (db_path, state)
    };

    // MCP registration is opt-in: app startup never touches agent CLI configs.
    // Run `everything --register-mcp` to write the entry into Claude Code /
    // Codex explicitly.
    //
    // Non-Windows: the GUI owns index writes while running, so hold the beacon
    // (single-writer handshake with the daemon) and record whether we won it —
    // a second instance that loses the race runs read-only. On Windows the SCM
    // service is the sole writer: the GUI holds no beacon and stays read-only
    // (is_writer defaults false, D6). `state` is already built per-platform above.
    #[cfg(not(target_os = "windows"))]
    {
        let beacon_held = daemon::hold_gui_beacon(&db_path);
        #[cfg(target_os = "macos")]
        {
            // D4 ownership flip: the launchd daemon is the sole writer and the
            // GUI is hard read-only regardless of the beacon. The beacon stays
            // held anyway — it detects a second GUI instance and it is the
            // yield signal pre-flip daemons still in the field react to (§7).
            // Exception: bench mode measures the in-process indexer, so it
            // keeps the legacy writer path.
            if bench_mode {
                state.is_writer.store(beacon_held, AtomicOrdering::Release);
            } else {
                set_db_readonly();
                state.is_writer.store(false, AtomicOrdering::Release);
                let _ = beacon_held;
                // Read-only marker: a gui.lock holder WITHOUT gui_ro.lock is a
                // pre-flip writer GUI the daemon yields to (§7); holding it
                // tells the daemon this GUI is a post-flip reader, so it keeps
                // writing. Bench mode (above) is a real writer and must not
                // hold it.
                daemon::hold_gui_ro_beacon(&db_path);
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            state.is_writer.store(beacon_held, AtomicOrdering::Release);
            if !beacon_held {
                eprintln!(
                    "[startup] gui.lock beacon not acquired -- another instance owns index \
                     writes; this GUI runs READ-ONLY (no watcher / indexer / purge)"
                );
            }
        }
    }
    eprintln!(
        "[startup] +{}ms AppState created ({} extra roots)",
        setup_started.elapsed().as_millis(),
        state.extra_roots.lock().len()
    );
    let (app_handle, receiver) = EventSink::channel();
    let ui_sink = app_handle.clone();
    let native_state = state.clone();
    std::thread::spawn(move || {
        let thread_started = std::time::Instant::now();
        eprintln!("[startup/thread] init thread started");

        // Linux: the GUI may be the writer, so it initializes the schema.
        // Windows / macOS (D4): the service/daemon owns the schema and the GUI
        // is a read-only reader — it can't create tables; it just waits for
        // the writer's DB. macOS bench mode is the one in-process writer left.
        #[cfg(not(target_os = "windows"))]
        {
            #[cfg(target_os = "macos")]
            let init_here = state.is_writer.load(AtomicOrdering::Acquire);
            #[cfg(not(target_os = "macos"))]
            let init_here = true;
            if init_here {
                eprintln!(
                    "[startup/thread] +{}ms calling init_db_tables...",
                    thread_started.elapsed().as_millis()
                );
                if let Err(err) = init_db_tables(&state.db_path) {
                    set_state(&state, IndexState::Error, Some(err.clone()));
                    emit_index_state(&app_handle, "Error", Some(err));
                    return;
                }
                eprintln!(
                    "[startup/thread] +{}ms init_db_tables done",
                    thread_started.elapsed().as_millis()
                );
            }
        }

        #[cfg(target_os = "macos")]
        {
            let usable = state.is_writer.load(AtomicOrdering::Acquire)
                || mac::bridge::macos_db_usable(&state.db_path);
            state.db_ready.store(usable, AtomicOrdering::Release);
            if !usable {
                eprintln!(
                    "[startup/thread] daemon DB not readable yet; searches ride the \
                     Spotlight fallback until the bridge sees it"
                );
            }
        }
        #[cfg(not(target_os = "macos"))]
        state.db_ready.store(true, AtomicOrdering::Release);
        eprintln!(
            "[startup/thread] +{}ms db_ready set -- continuing startup",
            thread_started.elapsed().as_millis()
        );

        // If a previous run crashed mid FTS rebuild, don't trust the FTS index
        // until the healing rebuild (finalize_fresh_index) completes.
        if let Ok(c) = db_connection(&state.db_path) {
            if !fts_usable(&c) {
                eprintln!("[startup/thread] FTS dirty or missing -- disabled until rebuild");
                state.fts_ready.store(false, AtomicOrdering::Release);
            }
        }

        // Handshake (Linux only — macOS flipped to the daemon-writer model,
        // D4/D7): the GUI beacon (gui.lock) is already held (setup_app).
        // Before the GUI starts *writing* (housekeeping purge, indexing,
        // watcher), wait for any resident daemon to observe the beacon and
        // exit — releasing daemon.lock — so the two never overlap as WAL
        // writers. Bounded — falls through (WAL-safe: the daemon skips VACUUM
        // while the beacon is held).
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        daemon::wait_for_daemon_exit(&state.db_path);

        // Single-writer gate. Only the beacon-holding GUI starts the index
        // writers below (watcher / indexer / purge); a second GUI instance runs
        // read-only — search still works via the read connection pool — so two
        // GUIs can never write the same WAL DB concurrently. On Windows the GUI
        // is always read-only (is_writer=false): the service is the sole writer.
        let gui_is_writer = state.is_writer.load(AtomicOrdering::Acquire);
        // Linux keeps the writer handshake; on macOS the GUI is a reader (the
        // bridge below seeds/streams state) and on Windows the same has always
        // been true — neither should blind-emit Ready here.
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        if gui_is_writer {
            // Never overlap a resident daemon that is still mid-build: keep
            // waiting here (this runs on the init thread only; the UI already
            // searches read-only) until the daemon observes our beacon, yields,
            // and releases daemon.lock — then we take over as sole writer.
            // Generously capped so a wedged daemon can't park the GUI indexer
            // forever (fallback: proceed anyway — WAL keeps writes safe).
            let deferral_started = std::time::Instant::now();
            const DAEMON_YIELD_CAP: Duration = Duration::from_secs(300);
            let mut logged = false;
            while daemon::daemon_running(&state.db_path) {
                if deferral_started.elapsed() >= DAEMON_YIELD_CAP {
                    eprintln!(
                        "[startup] daemon still resident after {}s; proceeding as writer (fallback)",
                        DAEMON_YIELD_CAP.as_secs()
                    );
                    diag::log(format!(
                        "ALERT: daemon did not yield within {}s; GUI proceeding as writer \
                         (dual-writer risk — expect SQLITE_BUSY on one side)",
                        DAEMON_YIELD_CAP.as_secs()
                    ));
                    break;
                }
                if !logged {
                    eprintln!("[startup] daemon still building the index; GUI stays read-only until it yields");
                    diag::log(
                        "daemon mid-build at GUI launch; GUI defers index writes until it yields",
                    );
                    logged = true;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        } else {
            eprintln!("[startup] read-only instance; not starting index writers (watcher / indexer / purge)");
            diag::log("read-only GUI instance: watcher/indexer disabled (lost gui.lock beacon)");
            emit_index_state(&app_handle, "Ready", None);
        }

        // Deferred housekeeping -- purge + status counts run in background.
        // Non-Windows only: the Windows GUI is read-only (gui_is_writer is always
        // false there) and the service owns index hygiene, so purge_ignored_entries
        // (a non-Windows fn) must not be compiled into the Windows binary (§3.3).
        #[cfg(not(target_os = "windows"))]
        if gui_is_writer {
            let hk_app = app_handle.clone();
            let hk_state = state.clone();
            std::thread::spawn(move || {
                // Brief pause so indexing thread can start first
                std::thread::sleep(std::time::Duration::from_millis(500));
                let hk_started = std::time::Instant::now();
                let _ = refresh_and_emit_status_counts(Some(&hk_app), &hk_state);
                eprintln!(
                    "[startup/housekeeping] refresh_and_emit_status_counts done in {}ms",
                    hk_started.elapsed().as_millis()
                );
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
                eprintln!(
                    "[startup/housekeeping] all done in {}ms",
                    hk_started.elapsed().as_millis()
                );
            });
        }

        #[cfg(target_os = "macos")]
        if gui_is_writer {
            // Bench mode (EVERYTHING_BENCH) is the only in-process writer left
            // on macOS after the D4 flip — it measures the indexer itself.
            let _ = start_full_index_worker(app_handle.clone(), state.clone());
        } else {
            // Read-only reader (D4): no indexing — the launchd daemon owns
            // every write (schema, fresh/catchup passes, FSEvents watcher,
            // .pathindexing reconcile, maintenance). Seed Ready from its DB
            // and mirror its status/health over the UDS bridge; the legacy
            // in-process fresh-vs-catchup startup decision lives in the
            // daemon now (daemon.rs run_daemon_macos).
            mac::bridge::start_macos_readonly(app_handle.clone(), state.clone());
        }

        #[cfg(target_os = "windows")]
        {
            // Read-only reader (§3.3): no indexing — the service owns it. Mark
            // ready from the service's DB and poll the service for status/health.
            win::start_windows_readonly(app_handle.clone(), state.clone());
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        if gui_is_writer {
            let _ = start_full_index_worker(app_handle.clone(), state.clone());
        }

        if gui_is_writer && bench_mode {
            start_bench_runner(app_handle.clone(), state.clone());
        }

        // Prime the extension→PNG cache for the extensions a first screen of
        // results is most likely to show. This used to spawn one `swift -e` per
        // extension — twenty Swift compiler runs, ~1s each — concurrently with
        // the first-run index, which is a meaningful slice of the startup CPU
        // spike on its own. Now it waits for indexing to settle and renders the
        // whole set in a single process. `start_full_index_worker` above sets
        // indexing_active synchronously, so this observes it without a race.
        #[cfg(not(target_os = "windows"))]
        if !bench_mode {
            let icon_cache = state.icon_cache.clone();
            // macOS reads the daemon's build state (bridge-maintained) — the
            // read-only GUI never sets indexing_active itself.
            #[cfg(target_os = "macos")]
            let indexing_active = state.daemon_building.clone();
            #[cfg(not(target_os = "macos"))]
            let indexing_active = state.indexing_active.clone();
            std::thread::spawn(move || {
                const PREWARM_EXTS: [&str; 20] = [
                    "txt", "pdf", "png", "jpg", "md", "json", "swift", "rs", "js", "ts", "html",
                    "css", "py", "zip", "dmg", "app", "doc", "xls", "ppt", "mov",
                ];
                // Bounded so a wedged pass can never strand the cache empty;
                // icons fall back to per-request rendering until then.
                const INDEX_WAIT_TIMEOUT: Duration = Duration::from_secs(600);
                // Let the first bridge status poll land (macOS) so a daemon
                // mid-fresh-build is actually observed before we decide the
                // coast is clear.
                std::thread::sleep(Duration::from_secs(2));
                let wait_started = Instant::now();
                while indexing_active.load(AtomicOrdering::Acquire)
                    && wait_started.elapsed() < INDEX_WAIT_TIMEOUT
                {
                    std::thread::sleep(Duration::from_millis(500));
                }

                let waited = wait_started.elapsed();
                let pending: Vec<&str> = {
                    let cache = icon_cache.lock();
                    PREWARM_EXTS
                        .iter()
                        .copied()
                        .filter(|ext| !cache.contains_key(*ext))
                        .collect()
                };
                let render_started = Instant::now();
                let sources: Vec<IconSource> =
                    pending.iter().map(|ext| IconSource::Type(ext)).collect();
                let pngs = render_icon_pngs(&sources);
                let mut rendered = 0usize;
                {
                    let mut cache = icon_cache.lock();
                    for (ext, png) in pending.iter().zip(pngs) {
                        if let Some(png) = png {
                            cache.insert((*ext).to_string(), png);
                            rendered += 1;
                        }
                    }
                }
                eprintln!(
                    "[icons] prewarm: waited {}ms for indexing, rendered {}/{} ext(s) \
                     in one swift process in {}ms",
                    waited.as_millis(),
                    rendered,
                    pending.len(),
                    render_started.elapsed().as_millis(),
                );
            });
        }
    });

    Ok((native_state, ui_sink, receiver))
}

fn main() {
    if ui_search_bench::handle_cli_args() {
        return;
    }
    // `--mcp` / `--register-mcp` run headless and must not boot the GUI.
    if mcp_server::handle_cli_args() {
        return;
    }
    // Windows: headless service commands run before any Tauri/WebView init.
    #[cfg(target_os = "windows")]
    {
        let args: Vec<String> = std::env::args().skip(1).collect();
        // The SCM-managed indexer service.
        if args.iter().any(|a| a == "--service") {
            win::service::run();
            return;
        }
        // Self-recovery: (re)register the installed service, elevated (D5). The
        // GUI banner invokes this via ShellExecute(runas).
        if args.iter().any(|a| a == "--register-service") {
            match win::service::register_service() {
                Ok(()) => eprintln!("[register-service] EverythingIndexer registered and started"),
                Err(e) => {
                    eprintln!("[register-service] failed: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
    }
    ui::run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_bulk_writer_survives_forced_stop() {
        const CHILD_FIXTURE: &str = "EVERYTHING_WAL_CRASH_FIXTURE";
        const MARKER: &str = ".everything-wal-crash-fixture";
        if let Some(root) = std::env::var_os(CHILD_FIXTURE) {
            let root = PathBuf::from(root);
            assert!(root.is_absolute() && root.join(MARKER).is_file());
            let conn = db_connection(&root.join("crash.db")).unwrap();
            set_indexing_pragmas(&conn).unwrap();
            assert_eq!(
                conn.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
                    .unwrap(),
                "wal"
            );
            assert_eq!(
                conn.pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            conn.execute_batch("CREATE TABLE durable(value BLOB); INSERT INTO durable VALUES ('committed'); PRAGMA cache_size=2; PRAGMA cache_spill=2;").unwrap();
            let wal = root.join("crash.db-wal");
            let committed_size = fs::metadata(&wal).unwrap().len();
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            conn.execute(
                "INSERT INTO durable VALUES (?1)",
                params![vec![42u8; 1024 * 1024]],
            )
            .unwrap();
            assert!(
                fs::metadata(wal).unwrap().len() > committed_size,
                "uncommitted pages must reach WAL before the forced stop"
            );
            fs::write(root.join("ready"), "ready").unwrap();
            loop {
                std::thread::park();
            }
        }

        let root = temp_case_dir("wal_crash");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(MARKER), "isolated crash recovery test").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::windows_bulk_writer_survives_forced_stop",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_FIXTURE, &root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .creation_flags(0x0800_0000)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !root.join("ready").exists() {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                panic!(
                    "crash fixture exited early: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("crash fixture did not become ready");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // On Windows Child::kill uses TerminateProcess, matching the installer's
        // fallback rather than letting SQLite close or checkpoint the writer.
        child.kill().unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        let conn = db_connection(&root.join("crash.db")).unwrap();
        let integrity: String = conn
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
        let values: Vec<String> = conn
            .prepare("SELECT value FROM durable")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(values, vec!["committed"]);
        drop(conn);
        assert!(root.is_absolute() && root.join(MARKER).is_file());
        fs::remove_dir_all(root).unwrap();
    }

    /// The invariant the fans depend on. `UTILITY` measurably does not keep work
    /// off performance cores (see `mac::sched`), so the multi-threaded walk must
    /// map to `BACKGROUND` under *every* pressure state — including a cool, idle,
    /// mains-powered machine, which is exactly the first-install case that spun
    /// an M2 Pro's fans at 320% CPU.
    #[cfg(target_os = "macos")]
    #[test]
    fn walk_threads_are_background_regardless_of_pressure() {
        use mac::sched::{Pressure, Qos};
        assert_eq!(scan_qos(ScanRole::Walk), Qos::Background);
        assert_eq!(scan_qos(ScanRole::IdleWriter), Qos::Background);
        // The single writer thread of a fresh pass is the pass's critical path
        // and keeps a performance core while the machine is cool.
        let expected = match mac::sched::pressure() {
            Pressure::None | Pressure::Elevated => Qos::Utility,
            Pressure::High => Qos::Background,
        };
        assert_eq!(scan_qos(ScanRole::FreshWriter), expected);

        // The scan-start log must name both halves; a regression that silently
        // put the walk back on P-cores would otherwise be invisible in the field.
        let label = scan_scheduling_label();
        assert!(
            label.contains("walk_qos=background") || label.contains("full_speed=1"),
            "scan log must record the walk's class, got {label:?}"
        );
    }

    #[test]
    fn scan_failures_ignore_paths_that_vanished_mid_walk() {
        let mut failures = ScanFailures::default();
        // The case that inflated the old counter: a directory deleted between
        // its parent's readdir and the walk reaching it. Normal, unbounded on a
        // busy machine, and nothing for the user to act on.
        assert!(!failures.observe(Some(io::ErrorKind::NotFound)));
        assert_eq!(failures, ScanFailures::default());
        assert_eq!(scan_failure_message(failures, true), None);

        assert!(failures.observe(Some(io::ErrorKind::PermissionDenied)));
        assert!(failures.observe(Some(io::ErrorKind::Other)));
        // A walker error with no io::Error behind it is still a real failure.
        assert!(failures.observe(None));
        assert_eq!(failures, ScanFailures { denied: 1, io: 2 });
        assert_eq!(failures.total(), 3);
    }

    /// Only the no-FDA wording names the permission — with access granted a
    /// denial is a genuine oddity, not an onboarding step the user skipped.
    #[test]
    fn scan_failure_message_names_full_disk_access_only_when_missing() {
        let denied = ScanFailures { denied: 7, io: 0 };
        let without = scan_failure_message(denied, false).unwrap();
        assert!(without.contains("전체 디스크 접근 권한"), "{without}");
        assert!(without.contains('7'), "{without}");
        let with = scan_failure_message(denied, true).unwrap();
        assert!(!with.contains("전체 디스크 접근 권한"), "{with}");

        // I/O faults are reported on their own terms, never as a permission story.
        let io_only = scan_failure_message(ScanFailures { denied: 0, io: 3 }, false).unwrap();
        assert_eq!(io_only, "읽기 실패 3개");
    }

    #[test]
    fn scan_failures_merge_across_workers() {
        let mut total = ScanFailures { denied: 2, io: 1 };
        total.merge(ScanFailures { denied: 3, io: 0 });
        assert_eq!(total, ScanFailures { denied: 5, io: 1 });
    }

    #[test]
    fn denied_prefix_collapses_per_app_container_denials() {
        let home = Path::new("/Users/u");
        // Several hundred of these on a Mac with many sandboxed apps: without the
        // collapse they would be the whole rescan set instead of one prefix.
        assert_eq!(
            denied_prefix_for(Path::new("/Users/u/Library/Containers/com.a.b/Data"), home),
            "/Users/u/Library/Containers".replace('/', std::path::MAIN_SEPARATOR_STR)
        );
        assert_eq!(
            denied_prefix_for(
                Path::new("/Users/u/Library/Group Containers/ABC.com.x/Library"),
                home
            ),
            "/Users/u/Library/Group Containers".replace('/', std::path::MAIN_SEPARATOR_STR)
        );
        // Already at or above the depth cap: kept whole.
        assert_eq!(
            denied_prefix_for(Path::new("/Users/u/Library/Mail"), home),
            "/Users/u/Library/Mail"
        );
        // Outside the scan root (a .pathindexing extra root): kept whole, since
        // truncating against the wrong root could name an unrelated ancestor.
        assert_eq!(
            denied_prefix_for(Path::new("/Volumes/ext/a/b/c/d"), home),
            "/Volumes/ext/a/b/c/d"
        );
    }

    #[test]
    fn note_denied_prefix_dedups_and_stays_bounded() {
        let denied: DeniedPrefixes = Arc::new(Mutex::new(BTreeSet::new()));
        let home = Path::new("/Users/u");
        for i in 0..500 {
            note_denied_prefix(
                &denied,
                &PathBuf::from(format!("/Users/u/Library/Containers/app{i}/Data")),
                home,
            );
        }
        assert_eq!(
            denied.lock().iter().cloned().collect::<Vec<_>>(),
            vec!["/Users/u/Library/Containers".replace('/', std::path::MAIN_SEPARATOR_STR)],
            "per-app denials must collapse onto one prefix"
        );
        for i in 0..DENIED_PREFIX_LIMIT * 2 {
            note_denied_prefix(&denied, &PathBuf::from(format!("/Users/u/r{i}/x/y")), home);
        }
        assert_eq!(denied.lock().len(), DENIED_PREFIX_LIMIT);
    }

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
        queue_subtree_rescan(
            PathBuf::from("/h/a/b"),
            RESCAN_COOLDOWN,
            &mut queued,
            &finished,
        );
        queue_subtree_rescan(
            PathBuf::from("/h/a/b/c"),
            RENAME_RESCAN_COOLDOWN,
            &mut queued,
            &finished,
        );
        assert_eq!(queued.len(), 1, "descendant is covered by queued ancestor");
        queue_subtree_rescan(
            PathBuf::from("/h/a"),
            RENAME_RESCAN_COOLDOWN,
            &mut queued,
            &finished,
        );
        assert_eq!(queued.len(), 1, "wider rescan replaces queued descendants");
        let entry = queued.get(Path::new("/h/a")).expect("wider path queued");
        assert_eq!(
            entry.cooldown, RESCAN_COOLDOWN,
            "covering entry absorbs the strictest cooldown class it subsumes"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn queue_subtree_rescan_defers_within_cooldown() {
        let finished = Mutex::new(vec![(
            PathBuf::from("/h/a"),
            Instant::now(),
            RESCAN_COOLDOWN,
        )]);
        let mut queued = HashMap::new();
        queue_subtree_rescan(
            PathBuf::from("/h/a/x"),
            RESCAN_COOLDOWN,
            &mut queued,
            &finished,
        );
        let entry = queued.get(Path::new("/h/a/x")).expect("queued");
        assert!(
            entry.due > Instant::now() + RESCAN_COOLDOWN - Duration::from_secs(60),
            "overlapping rescan must be deferred past the cooldown window"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rename_class_rescan_uses_short_cooldown() {
        // A rename-triggered rescan defers only by its own short class — a
        // finished rename rescan must not impose the 5-minute
        // MustScanSubDirs window on the next rename of the same subtree.
        let finished = Mutex::new(vec![(
            PathBuf::from("/h/r"),
            Instant::now(),
            RENAME_RESCAN_COOLDOWN,
        )]);
        let mut queued = HashMap::new();
        queue_subtree_rescan(
            PathBuf::from("/h/r"),
            RENAME_RESCAN_COOLDOWN,
            &mut queued,
            &finished,
        );
        let entry = queued.get(Path::new("/h/r")).expect("queued");
        assert!(
            entry.due <= Instant::now() + RENAME_RESCAN_COOLDOWN,
            "rename-class rescan is due after its own short cooldown"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn queue_subtree_rescan_upgrades_cooldown_class() {
        let finished = Mutex::new(Vec::new());
        let mut queued = HashMap::new();
        queue_subtree_rescan(
            PathBuf::from("/h/a"),
            RENAME_RESCAN_COOLDOWN,
            &mut queued,
            &finished,
        );
        queue_subtree_rescan(
            PathBuf::from("/h/a/b"),
            RESCAN_COOLDOWN,
            &mut queued,
            &finished,
        );
        assert_eq!(queued.len(), 1, "covered by the queued ancestor");
        let entry = queued.get(Path::new("/h/a")).expect("ancestor entry");
        assert_eq!(
            entry.cooldown, RESCAN_COOLDOWN,
            "a MustScanSubDirs trigger under a queued rename rescan upgrades its class"
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
            is_writer: Arc::new(AtomicBool::new(true)),
            path_ignores: Arc::new(Vec::new()),
            path_ignore_patterns: Arc::new(Vec::new()),
            db_ready: Arc::new(AtomicBool::new(true)),
            indexing_active: Arc::new(AtomicBool::new(false)),
            cancel_index: Arc::new(AtomicBool::new(false)),
            status: Arc::new(Mutex::new(IndexStatus::default())),
            #[cfg(not(target_os = "macos"))]
            recent_ops: Arc::new(Mutex::new(Vec::new())),
            icon_cache: Arc::new(Mutex::new(HashMap::new())),
            fd_search_cache: Arc::new(Mutex::new(None)),
            negative_name_cache: Arc::new(Mutex::new(HashMap::new())),
            ignore_cache: Arc::new(Mutex::new(None)),
            fts_ready: Arc::new(AtomicBool::new(true)),
            mem_index: Arc::new(RwLock::new(None)),
            watcher_stop: Arc::new(AtomicBool::new(false)),
            watcher_active: Arc::new(AtomicBool::new(false)),
            watcher_gen: Arc::new(AtomicU64::new(0)),
            #[cfg(target_os = "windows")]
            watcher_lost_events: Arc::new(AtomicBool::new(false)),
            #[cfg(target_os = "windows")]
            config_dirty: Arc::new(AtomicBool::new(false)),
            frontend_ready: Arc::new(AtomicBool::new(true)),
            #[cfg(target_os = "macos")]
            pathindexing_active: Arc::new(AtomicBool::new(false)),
            #[cfg(target_os = "macos")]
            pathignore_dirty: Arc::new(AtomicBool::new(false)),
            search_conn_pool: Arc::new(Mutex::new(Vec::new())),
            #[cfg(target_os = "macos")]
            watcher_conn: Arc::new(Mutex::new(None)),
            #[cfg(target_os = "macos")]
            daemon_building: Arc::new(AtomicBool::new(false)),
        }
    }

    fn insert_test_entry(conn: &Connection, path: &Path, is_dir: bool, run_id: i64) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let dir = path
            .parent()
            .map(|parent| parent.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());
        conn.execute(
            "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
             VALUES(?1, ?2, ?3, ?4, NULL, NULL, NULL, ?5, ?6)",
            params![
                path.to_string_lossy().to_string(),
                name,
                dir,
                if is_dir { 1 } else { 0 },
                now_epoch(),
                run_id,
            ],
        )
        .unwrap();
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
    fn windows_temp_documents_are_not_skipped() {
        let document = Path::new(
            r"C:\Users\test-user\AppData\Local\Temp\claude\session\scratchpad\add_comment_args.json",
        );
        assert!(!should_skip_path(document, &[], &[]));
        assert_eq!(
            walk_child_checks(
                "C:/Users/test-user",
                &[
                    "AppData",
                    "Local",
                    "Temp",
                    "claude",
                    "session",
                    "scratchpad",
                    "add_comment_args.json",
                ],
                &[],
                &[],
            ),
            None,
            "full scans must descend into Temp documents"
        );
    }

    /// Mirror of the walker's invariant: descend from `root` along `segments`,
    /// checking at every level that the fast per-child check agrees with the
    /// full-path check, and stopping at the first skip exactly as jwalk's
    /// `process_read_dir` prunes. Returns the depth at which the walk stopped,
    /// or `None` when every segment was kept.
    fn walk_child_checks(
        root: &str,
        segments: &[&str],
        roots: &[PathBuf],
        patterns: &[IgnorePattern],
    ) -> Option<usize> {
        assert!(
            !should_skip_path(Path::new(root), roots, patterns),
            "test root {root} must itself be vetted"
        );
        let mut dir = normalize_slashes(root.to_string());
        let mut scratch = String::new();
        for (depth, name) in segments.iter().enumerate() {
            let child = Path::new(&dir).join(name);
            let fast = should_skip_child(&dir, name, &child, &mut scratch, roots, patterns);
            let full = should_skip_path(&child, roots, patterns);
            assert_eq!(
                fast,
                full,
                "should_skip_child disagreed with should_skip_path at {}",
                child.display()
            );
            if fast {
                return Some(depth);
            }
            dir = normalize_slashes(child.to_string_lossy().to_string());
        }
        None
    }

    #[test]
    fn should_skip_child_matches_should_skip_path_for_builtin_rules() {
        // Single-segment name, deep below the root.
        assert_eq!(
            walk_child_checks("/Users/u", &["work", "proj", "node_modules", "a"], &[], &[]),
            Some(2)
        );
        // Suffix rule (*.build) on the leaf.
        assert_eq!(
            walk_child_checks("/Users/u", &["work", "MyApp.build", "Objects"], &[], &[]),
            Some(1)
        );
        // Multi-segment rule: only fires once both segments are present, which
        // is the case the leaf-only fast path must not lose.
        assert_eq!(
            walk_child_checks("/Users/u", &["Library", "Caches", "pip"], &[], &[]),
            Some(1)
        );
        // Lookalikes that must survive: prefix of a builtin name, and a
        // multi-segment rule whose first half matches but second does not.
        assert_eq!(
            walk_child_checks("/Users/u", &["Library", "CachesX", "keep"], &[], &[]),
            None
        );
        assert_eq!(
            walk_child_checks("/Users/u", &["node_modules_old", "src", "a.rs"], &[], &[]),
            None
        );
        // "Caches" alone, not under Library.
        assert_eq!(
            walk_child_checks("/Users/u", &["work", "Caches", "x"], &[], &[]),
            None
        );
    }

    #[test]
    fn should_skip_child_matches_should_skip_path_for_ignored_roots() {
        let roots = vec![PathBuf::from("/Users/u/work/secret")];
        // Skips at the root itself, so descendants are never asked — the
        // equality that matters is that `starts_with` collapsing to `==` does
        // not change the pruning depth.
        assert_eq!(
            walk_child_checks("/Users/u", &["work", "secret", "deep", "f"], &roots, &[]),
            Some(1)
        );
        assert_eq!(
            walk_child_checks("/Users/u", &["work", "secretive", "f"], &roots, &[]),
            None
        );
    }

    #[test]
    fn should_skip_child_matches_should_skip_path_for_patterns() {
        let any_segment = vec![IgnorePattern::any_segment("target")];
        assert_eq!(
            walk_child_checks("/Users/u", &["rust", "target", "debug"], &[], &any_segment),
            Some(1)
        );
        assert_eq!(
            walk_child_checks("/Users/u", &["rust", "targets", "debug"], &[], &any_segment),
            None
        );

        // Globs are matched against every ancestor by should_skip_path; the
        // fast path only tests the child, which relies on the parent already
        // having been cleared.
        let globs = vec![
            IgnorePattern::Glob("/Users/u/Library/Application Support/*/Cache*".to_string()),
            IgnorePattern::Glob("/Users/u/.pyenv/versions/*/lib".to_string()),
        ];
        assert_eq!(
            walk_child_checks(
                "/Users/u",
                &["Library", "Application Support", "Slack", "Cache", "v1"],
                &[],
                &globs,
            ),
            Some(3)
        );
        assert_eq!(
            walk_child_checks(
                "/Users/u",
                &[".pyenv", "versions", "3.12.1", "lib", "site-packages"],
                &[],
                &globs,
            ),
            Some(3)
        );
        assert_eq!(
            walk_child_checks(
                "/Users/u",
                &["Library", "Application Support", "Slack", "Logs"],
                &[],
                &globs,
            ),
            None
        );
    }

    /// Guards the framing and the argv tagging of the batched renderer: a
    /// misframed stream or a swallowed argument would silently put one row's
    /// icon on another row, which no other test would notice. Also pins the
    /// output size — serializing an NSImage's own representation instead of
    /// drawing into the 32x32 context yields 1024x1024 PNGs, ~100x the bytes,
    /// to paint a 26px row.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "spawns the Swift toolchain; run explicitly with --ignored"]
    fn batched_icons_stay_aligned_with_their_requests() {
        let terminal = "/System/Applications/Utilities/Terminal.app";
        let calculator = "/System/Applications/Calculator.app";
        // This path does not exist and holds a newline: AppKit still returns the
        // generic icon for it, but a newline-joined payload would have split it
        // into two requests and shifted every frame after it.
        let missing = "/nope\nnewline.app";
        let sources = [
            IconSource::File(terminal),
            IconSource::Type("txt"),
            IconSource::File(calculator),
            IconSource::Type("zip"),
            IconSource::File(missing),
            IconSource::Type("folder"),
        ];
        let icons = render_icon_pngs(&sources);

        assert_eq!(icons.len(), sources.len(), "results must be positional");
        for (slot, icon) in icons.iter().enumerate() {
            let png = icon
                .as_ref()
                .unwrap_or_else(|| panic!("no icon rendered for source {slot}"));
            assert_eq!(
                &png[..8],
                b"\x89PNG\r\n\x1a\n",
                "source {slot} is not a PNG"
            );
            // IHDR width/height, big-endian, right after the 8-byte magic and
            // the 8-byte chunk header.
            assert_eq!(
                (&png[16..20], &png[20..24]),
                (&[0, 0, 0, 32][..], &[0, 0, 0, 32][..]),
                "source {slot} is not 32x32 — the oversized representation is back"
            );
        }
        // Neither distinct paths nor distinct types may collapse onto one frame.
        assert_ne!(icons[0], icons[2], "Terminal and Calculator should differ");
        assert_ne!(icons[1], icons[3], "txt and zip should differ");
        // And a one-entry batch must reproduce exactly the frame a longer batch
        // put at that position.
        assert_eq!(
            render_icon_pngs(&[IconSource::File(terminal)]),
            vec![icons[0].clone()]
        );
        assert_eq!(
            render_icon_pngs(&[IconSource::Type("zip")]),
            vec![icons[3].clone()]
        );
    }

    fn icon_request(path: Option<&str>, ext: &str) -> IconRequest {
        IconRequest {
            path: path.map(str::to_string),
            ext: ext.to_string(),
        }
    }

    /// A batch is positional and deduplicated by cache key: rows that share an
    /// icon must all receive it, and none of them may render a second time. Only
    /// cached keys are used here, so nothing spawns a renderer.
    #[test]
    fn resolve_file_icons_serves_every_row_sharing_a_cache_key() {
        let cache = Mutex::new(HashMap::new());
        {
            let mut warm = cache.lock();
            warm.insert("txt".to_string(), vec![1u8]);
            warm.insert("folder".to_string(), vec![2u8]);
            warm.insert("__default__".to_string(), vec![3u8]);
        }

        let requests = vec![
            icon_request(Some("/u/a.txt"), "txt"),
            icon_request(Some("/u/docs"), "folder"),
            // Same key as the first row, reached through a different case.
            icon_request(Some("/u/b.TXT"), "TXT"),
            // No extension falls back to the default key, not to the path.
            icon_request(Some("/u/README"), ""),
            icon_request(None, "txt"),
        ];

        assert_eq!(
            resolve_file_icons(&cache, &requests),
            vec![vec![1], vec![2], vec![1], vec![3], vec![1]]
        );
    }

    #[test]
    fn should_skip_child_defers_backslash_names_to_the_full_check() {
        // A macOS file name may contain a literal backslash, which
        // should_skip_path turns into a separator when it normalizes. The fast
        // path must hand those to the general check rather than treat the whole
        // string as one segment.
        let dir = "/Users/u/work";
        let child = Path::new(dir).join("a\\node_modules");
        let mut scratch = String::new();
        assert_eq!(
            should_skip_child(dir, "a\\node_modules", &child, &mut scratch, &[], &[]),
            should_skip_path(&child, &[], &[]),
        );
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

    #[test]
    fn preload_direct_children_never_includes_scan_root_or_grandchildren() {
        let root = temp_case_dir("preload_direct_children");
        let scan_root = root.join("home");
        let child = scan_root.join("docs");
        let grandchild = child.join("note.txt");
        fs::create_dir_all(&child).unwrap();
        fs::write(&grandchild, "note").unwrap();

        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        insert_test_entry(&conn, &scan_root, true, 1);
        insert_test_entry(&conn, &child, true, 1);
        insert_test_entry(&conn, &grandchild, false, 1);

        let scan_str = scan_root.to_string_lossy().to_string();
        let existing = preload_direct_children(&conn, &scan_str).unwrap();
        assert_eq!(existing.len(), 1);
        assert!(existing.contains_key(&child.to_string_lossy().to_string()));
        assert!(!existing.contains_key(&scan_str));
        assert!(!existing.contains_key(&grandchild.to_string_lossy().to_string()));
        assert!(preload_path_signature(&conn, &scan_str).unwrap().is_some());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn direct_child_observation_rejects_open_iterator_and_metadata_errors() {
        let scan_root = PathBuf::from("/virtual/home");

        let open_error = observe_direct_children(
            &scan_root,
            |_| -> io::Result<std::vec::IntoIter<io::Result<PathBuf>>> {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            },
            |_| -> io::Result<fs::Metadata> { unreachable!() },
            |_| false,
        )
        .err()
        .expect("open failure");
        assert!(open_error.contains("stage=read_dir_open"));
        assert!(open_error.contains("kind=PermissionDenied"));

        let iterator_error = observe_direct_children(
            &scan_root,
            |_| Ok(vec![Err(io::Error::new(io::ErrorKind::Other, "iterator I/O"))].into_iter()),
            |_| -> io::Result<fs::Metadata> { unreachable!() },
            |_| false,
        )
        .err()
        .expect("iterator failure");
        assert!(iterator_error.contains("stage=read_dir_entry"));

        let child = scan_root.join("Documents");
        let metadata_error = observe_direct_children(
            &scan_root,
            |_| Ok(vec![Ok(child.clone())].into_iter()),
            |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            |_| false,
        )
        .err()
        .expect("metadata failure");
        assert!(metadata_error.contains("stage=direct_child_metadata"));
        assert!(metadata_error.contains(child.to_string_lossy().as_ref()));
    }

    #[test]
    fn unseen_direct_children_delete_only_ignored_or_confirmed_not_found() {
        let root = temp_case_dir("direct_child_reconcile");
        fs::create_dir_all(&root).unwrap();
        let ignored = root.join("ignored");
        let missing = root.join("missing");
        let present = root.join("present.txt");
        fs::write(&present, "present").unwrap();

        let existing = HashMap::from([
            (ignored.to_string_lossy().to_string(), (None, None)),
            (missing.to_string_lossy().to_string(), (None, None)),
            (present.to_string_lossy().to_string(), (None, None)),
        ]);
        let reconciliation = reconcile_unseen_direct_children(
            &existing,
            |path| {
                if path == missing.as_path() {
                    Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
                } else {
                    fs::symlink_metadata(path)
                }
            },
            |path| path == ignored.as_path(),
        )
        .unwrap();

        let deleted: HashSet<String> = reconciliation.delete_paths.into_iter().collect();
        assert_eq!(
            deleted,
            HashSet::from([
                ignored.to_string_lossy().to_string(),
                missing.to_string_lossy().to_string(),
            ])
        );
        assert_eq!(reconciliation.present.len(), 1);
        assert_eq!(reconciliation.present[0].path, present);

        let denied = root.join("denied");
        let denied_existing = HashMap::from([(denied.to_string_lossy().to_string(), (None, None))]);
        let error = reconcile_unseen_direct_children(
            &denied_existing,
            |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            |_| false,
        )
        .err()
        .expect("permission failure must abort before deletion");
        assert!(error.contains("stage=leftover_metadata"));
        assert!(error.contains("kind=PermissionDenied"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn catchup_read_dir_failure_preserves_scan_root_subtree() {
        let root = temp_case_dir("catchup_read_dir_failure");
        fs::create_dir_all(&root).unwrap();
        // A regular file has readable metadata but read_dir deterministically
        // fails with NotADirectory on every supported platform.
        let scan_root = root.join("home-file");
        fs::write(&scan_root, "not a directory").unwrap();
        let child = scan_root.join("Documents");
        let descendant = child.join("report.txt");

        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        insert_test_entry(&conn, &scan_root, false, 1);
        insert_test_entry(&conn, &child, true, 1);
        insert_test_entry(&conn, &descendant, false, 1);
        set_meta(&conn, "last_run_id", "1").unwrap();
        set_meta(&conn, "index_complete", "1").unwrap();
        drop(conn);

        let state = test_state_for(db_path.clone(), scan_root, root.clone());
        let error = run_incremental_index(None, &state)
            .err()
            .expect("read_dir failure must reject catchup");
        assert!(error.contains("stage=read_dir_open"));

        let conn = db_connection(&db_path).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 3, "no existing row may be deleted");
        assert_eq!(get_meta(&conn, "last_run_id").as_deref(), Some("1"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn catchup_deletes_confirmed_missing_direct_child_subtree() {
        let root = temp_case_dir("catchup_confirmed_missing");
        let scan_root = root.join("home");
        fs::create_dir_all(&scan_root).unwrap();
        let missing = scan_root.join("old-project");
        let descendant = missing.join("stale.txt");

        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        insert_test_entry(&conn, &scan_root, true, 1);
        insert_test_entry(&conn, &missing, true, 1);
        insert_test_entry(&conn, &descendant, false, 1);
        set_meta(&conn, "last_run_id", "1").unwrap();
        drop(conn);

        let state = test_state_for(db_path.clone(), scan_root, root.clone());
        run_incremental_index(None, &state).expect("safe catchup");

        let conn = db_connection(&db_path).unwrap();
        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE path = ?1 OR path = ?2",
                params![
                    missing.to_string_lossy().to_string(),
                    descendant.to_string_lossy().to_string(),
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0, "confirmed NotFound subtree must be removed");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cancelled_catchup_returns_err_and_skips_deletions() {
        // With cancel_index set, a catchup pass must unwind before persisting
        // last_run_id and before any worker emits leftover deletions. A row
        // whose file is gone under a still-present directory — which a normal
        // catchup would sweep — must therefore survive, and the run is left
        // "incomplete". This is the safety contract behind the daemon yielding
        // the writer to the GUI mid-build.
        let root = temp_case_dir("cancelled_catchup");
        let scan_root = root.join("home");
        let present_dir = scan_root.join("present-dir");
        fs::create_dir_all(&present_dir).unwrap();
        // In the DB but not on disk: a normal catchup sweeps this as a leftover
        // once its parent directory is walked.
        let gone = present_dir.join("gone.txt");

        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        insert_test_entry(&conn, &scan_root, true, 1);
        insert_test_entry(&conn, &present_dir, true, 1);
        insert_test_entry(&conn, &gone, false, 1);
        set_meta(&conn, "last_run_id", "1").unwrap();
        drop(conn);

        let state = test_state_for(db_path.clone(), scan_root, root.clone());
        state.cancel_index.store(true, AtomicOrdering::Release);

        let error = run_incremental_index(None, &state)
            .err()
            .expect("a cancelled pass must return an error");
        assert!(error.contains("cancelled"), "unexpected error: {error}");

        let conn = db_connection(&db_path).unwrap();
        let gone_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE path = ?1",
                params![gone.to_string_lossy().to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(gone_rows, 1, "cancel must skip the leftover deletion");
        assert_eq!(
            get_meta(&conn, "last_run_id").as_deref(),
            Some("1"),
            "a cancelled pass must not bump last_run_id",
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn configured_unreadable_extra_root_is_preserved() {
        let root = temp_case_dir("unreadable_extra_root");
        let scan_root = root.join("home");
        fs::create_dir_all(&scan_root).unwrap();
        let extra_root = root.join("offline-volume");
        let extra_child = extra_root.join("cached.txt");

        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        let conn = db_connection(&db_path).unwrap();
        insert_test_entry(&conn, &scan_root, true, 1);
        insert_test_entry(&conn, &extra_root, true, 1);
        insert_test_entry(&conn, &extra_child, false, 1);
        set_meta(&conn, "last_run_id", "1").unwrap();
        set_meta(
            &conn,
            "indexed_extra_roots",
            extra_root.to_string_lossy().as_ref(),
        )
        .unwrap();
        drop(conn);

        let state = test_state_for(db_path.clone(), scan_root, root.clone());
        state.extra_roots.lock().push(extra_root.clone());
        run_incremental_index(None, &state).expect("unreadable extra root is non-destructive");

        let conn = db_connection(&db_path).unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE path = ?1 OR path = ?2",
                params![
                    extra_root.to_string_lossy().to_string(),
                    extra_child.to_string_lossy().to_string(),
                ],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 2);

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn watcher_metadata_errors_delete_only_not_found_and_retry_others() {
        let root = temp_case_dir("watcher_metadata_errors");
        fs::create_dir_all(&root).unwrap();
        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        let state = test_state_for(db_path, root.clone(), root.clone());
        let missing = root.join("missing.txt");
        let denied = root.join("denied.txt");

        let prepared = prepare_path_changes(
            &state,
            &[missing.clone(), denied.clone()],
            &[],
            &[],
            |path| {
                if path == missing.as_path() {
                    Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
                } else {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
                }
            },
        );

        assert!(prepared.to_upsert.is_empty());
        assert_eq!(
            prepared.to_delete,
            vec![missing.to_string_lossy().to_string()]
        );
        assert_eq!(prepared.retry_paths, vec![denied]);

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
        let out = apply_path_changes(&state, &[file_a.clone(), file_b.clone()], false).unwrap();
        assert_eq!(out.count_delta, 2);
        assert_eq!(out.changed, 2);

        // Re-upserting an existing path → no net change
        let out = apply_path_changes(&state, &[file_a.clone()], false).unwrap();
        assert_eq!(out.count_delta, 0);

        // Vanished file → -1
        fs::remove_file(&file_b).unwrap();
        let out = apply_path_changes(&state, &[file_b.clone()], false).unwrap();
        assert_eq!(out.count_delta, -1);

        // A vanished directory sweeps its remaining subtree rows: a.txt is
        // still in the DB and goes with the range delete → -1.
        fs::remove_dir_all(&docs).unwrap();
        let out = apply_path_changes(&state, &[docs.clone()], false).unwrap();
        assert_eq!(out.count_delta, -1);

        // Accumulated deltas must equal the authoritative COUNT(*)
        let conn = db_connection(&db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let _ = fs::remove_dir_all(root);
    }

    /// S5 trigger (b): a directory that appears with children already on disk
    /// but no descendants in the DB (external rename without per-child
    /// events) must surface as a rescan candidate; once its children are in
    /// the DB it must not.
    #[cfg(target_os = "macos")]
    #[test]
    fn apply_path_changes_flags_new_dir_with_unindexed_children() {
        let root = temp_case_dir("rescan_candidate_new_dir");
        let home = root.join("home");
        let moved = home.join("moved-in");
        fs::create_dir_all(moved.join("sub")).unwrap();
        fs::write(moved.join("sub/child.txt"), "x").unwrap();

        let db_path = root.join("index.db");
        init_db_tables(&db_path).unwrap();
        ensure_db_indexes(&db_path).unwrap();
        let state = test_state_for(db_path, home, root.clone());

        // Only the directory event arrives (no per-child events).
        let out = apply_path_changes(&state, &[moved.clone()], true).unwrap();
        assert_eq!(
            out.rescan_candidates,
            vec![moved.clone()],
            "new dir with on-disk children and no DB descendants needs a rescan"
        );

        // After the subtree lands in the DB, the same event is quiet.
        let out = apply_path_changes(
            &state,
            &[
                moved.clone(),
                moved.join("sub"),
                moved.join("sub/child.txt"),
            ],
            true,
        )
        .unwrap();
        assert!(
            out.rescan_candidates.is_empty(),
            "dir with DB descendants must not re-trigger: {:?}",
            out.rescan_candidates
        );

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
    ///
    /// macOS-only: step 4 measures the watcher path through `apply_path_changes`,
    /// which is itself `#[cfg(target_os = "macos")]`.
    #[cfg(target_os = "macos")]
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
            apply_path_changes(&state, &batch, false).expect("watcher single");
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
            apply_path_changes(&state, &batch, false).expect("watcher burst");
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
