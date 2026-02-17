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
        mpsc::{self, RecvTimeoutError},
        Arc, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
#[cfg(target_os = "macos")]
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};
use walkdir::WalkDir;

mod fd_search;
#[cfg(target_os = "macos")]
mod fsevent_watcher;
mod query;
use fd_search::{FdSearchCache, FdSearchResultDto};
use query::{escape_like, parse_query, SearchMode};

const DEFAULT_LIMIT: u32 = 300;
const SHORT_QUERY_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1000;
const BATCH_SIZE: usize = 10_000;
const RECENT_OP_TTL: Duration = Duration::from_secs(2);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(300);
const DB_VERSION: i32 = 4;
const DEFERRED_DIR_NAMES: &[&str] = &["Library", ".Trash", ".Trashes"];

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
    scanned: u64,
    indexed: u64,
    current_path: String,
}

impl Default for IndexStatus {
    fn default() -> Self {
        Self {
            state: IndexState::Ready,
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
struct RecentOp {
    old_path: Option<String>,
    new_path: Option<String>,
    op_type: &'static str,
    at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IgnorePattern {
    AnySegment(String),
    Glob(String),
}

#[derive(Debug, Clone)]
struct IgnoreRulesCache {
    roots: Vec<PathBuf>,
    patterns: Vec<IgnorePattern>,
    pathignore_mtime: Option<SystemTime>,
    gitignore_mtime: Option<SystemTime>,
}

#[derive(Debug, Clone)]
struct AppState {
    db_path: PathBuf,
    home_dir: PathBuf,
    cwd: PathBuf,
    path_ignores: Arc<Vec<PathBuf>>,
    path_ignore_patterns: Arc<Vec<IgnorePattern>>,
    db_ready: Arc<AtomicBool>,
    indexing_active: Arc<AtomicBool>,
    status: Arc<Mutex<IndexStatus>>,
    recent_ops: Arc<Mutex<Vec<RecentOp>>>,
    icon_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    fd_search_cache: Arc<Mutex<Option<FdSearchCache>>>,
    ignore_cache: Arc<Mutex<Option<IgnoreRulesCache>>>,
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
    run_id: i64,
}

fn now_epoch() -> i64 {
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

fn perf_log(message: impl AsRef<str>) {
    if perf_log_enabled() {
        eprintln!("[perf] {}", message.as_ref());
    }
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

fn set_indexing_pragmas(conn: &Connection) -> AppResult<()> {
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

fn restore_normal_pragmas(conn: &Connection) -> AppResult<()> {
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
    let conn = db_connection(db_path)?;

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

        CREATE INDEX IF NOT EXISTS idx_entries_dir ON entries(dir);
        CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
        CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_entries_ext ON entries(ext);
        CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);
        CREATE INDEX IF NOT EXISTS idx_entries_run_id ON entries(run_id);

        CREATE TABLE IF NOT EXISTS meta (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        "#,
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> AppResult<()> {
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

fn invalidate_search_caches(state: &AppState) {
    state.fd_search_cache.lock().take();
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
    {
        let key = root.to_string_lossy().to_string();
        if seen.insert(key) {
            roots.push(root);
        }
    }

    for pattern in pathignore_patterns {
        let key = ignore_pattern_key(&pattern);
        if seen_patterns.insert(key) {
            patterns.push(pattern);
        }
    }

    (roots, patterns)
}

fn cached_effective_ignore_rules(state: &AppState) -> (Vec<PathBuf>, Vec<IgnorePattern>) {
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

fn matches_ignore_pattern(path: &str, pattern: &IgnorePattern) -> bool {
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

fn should_skip_path(
    path: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> bool {
    let s = normalize_slashes(path.to_string_lossy().to_string());

    let builtin = s.contains("/.git/")
        || s.ends_with("/.git")
        || s.contains("/node_modules/")
        || s.ends_with("/node_modules")
        || s.contains("/Library/Caches/")
        || s.ends_with("/Library/Caches")
        || s.contains("/.Trash/")
        || s.ends_with("/.Trash")
        || s.contains("/.Trashes/")
        || s.ends_with("/.Trashes");

    builtin
        || ignored_roots
            .iter()
            .any(|root| path == root || path.starts_with(root))
        || ignored_patterns
            .iter()
            .any(|pattern| matches_ignore_pattern(&s, pattern))
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
        run_id: 0,
    })
}

fn index_row_from_walkdir_entry(entry: &walkdir::DirEntry) -> Option<IndexRow> {
    let path = entry.path();
    let metadata = entry.metadata().ok()?;
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

fn collect_rows_shallow(
    dir_path: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> Vec<IndexRow> {
    let mut rows = Vec::new();

    if let Ok(entries) = fs::read_dir(dir_path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if should_skip_path(&entry_path, ignored_roots, ignored_patterns) {
                continue;
            }
            if let Some(row) = index_row_from_path(&entry_path) {
                rows.push(row);
            }
        }
    }

    rows
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

fn upsert_rows(conn: &mut Connection, rows: &[IndexRow]) -> AppResult<usize> {
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

fn set_progress(state: &AppState, scanned: u64, indexed: u64, current_path: &str) {
    let mut status = state.status.lock();
    status.scanned = scanned;
    status.indexed = indexed;
    status.current_path = current_path.to_string();
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
    if state
        .indexing_active
        .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
        .is_err()
    {
        perf_log("index_start_skipped already_active=true");
        return Ok(());
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
    perf_log("index_state=Indexing");

    std::thread::spawn(move || {
        let result = run_incremental_index(&app, &state);
        if let Err(err) = result {
            set_state(&state, IndexState::Error, Some(err.clone()));
            emit_index_state(&app, "Error", Some(err));
            perf_log("index_state=Error");
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

    let last_run_id: i64 = get_meta(conn, "last_run_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let current_run_id = last_run_id + 1;

    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut permission_errors: u64 = 0;
    let mut current_path = String::new();
    let mut batch: Vec<IndexRow> = Vec::with_capacity(BATCH_SIZE);
    let mut stamp_batch: Vec<String> = Vec::with_capacity(BATCH_SIZE);
    let mut last_emit = Instant::now();
    let mut last_perf_emit = Instant::now();

    // Preload $HOME-level entries (direct children only, not recursive)
    let home_str = state.home_dir.to_string_lossy().to_string();
    let home_existing = preload_existing_entries(conn, &home_str);

    // Index $HOME itself
    if let Some(mut row) = index_row_from_path(&state.home_dir) {
        scanned += 1;
        row.run_id = current_run_id;
        if let Some((old_mtime, old_size)) = home_existing.get(&row.path) {
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

    if let Ok(entries) = fs::read_dir(&state.home_dir) {
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
                if let Some((old_mtime, old_size)) = home_existing.get(&row.path) {
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
                if is_deferred_dir(&child_path, &state.home_dir) {
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

    for root in &roots {
        let root_started = Instant::now();
        let mut root_scanned = 0u64;
        let mut root_indexed = 0u64;
        let mut root_permission_errors = 0u64;
        let root_str = root.to_string_lossy().to_string();
        let existing = preload_existing_entries(conn, &root_str);

        let iter = WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                !should_skip_path(
                    entry.path(),
                    &runtime_ignored_roots,
                    &runtime_ignored_patterns,
                )
            });

        for entry in iter {
            match entry {
                Ok(entry) => {
                    let path = entry.path();
                    if path == root.as_path() {
                        continue;
                    }
                    scanned += 1;
                    root_scanned += 1;
                    current_path = path.to_string_lossy().to_string();

                    if let Some(mut row) = index_row_from_walkdir_entry(&entry) {
                        row.run_id = current_run_id;
                        if let Some((old_mtime, old_size)) = existing.get(&row.path) {
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
                    if perf_log_enabled() && last_perf_emit.elapsed() >= Duration::from_secs(1) {
                        perf_log(format!(
                            "index_progress scanned={} indexed={} current_path={}",
                            scanned, indexed, current_path
                        ));
                        last_perf_emit = Instant::now();
                    }
                }
                Err(_) => {
                    scanned += 1;
                    root_scanned += 1;
                    permission_errors += 1;
                    root_permission_errors += 1;
                }
            }
        }

        perf_log(format!(
            "index_root_done root={} elapsed_ms={} scanned={} indexed={} permission_errors={}",
            root.to_string_lossy(),
            root_started.elapsed().as_millis(),
            root_scanned,
            root_indexed,
            root_permission_errors,
        ));
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

    set_meta(conn, "last_run_id", &current_run_id.to_string())?;

    if deleted_count > 0 || indexed > 0 {
        invalidate_search_caches(state);
    }

    let (entries_count, last_updated) = update_status_counts(state)?;
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

        if path.exists() {
            let is_dir = fs::symlink_metadata(path)
                .map(|meta| meta.is_dir())
                .unwrap_or(false);

            if is_dir {
                if let Some(row) = index_row_from_path(path) {
                    to_upsert_map.insert(row.path.clone(), row);
                }

                for row in collect_rows_shallow(path, &ignored_roots, &ignored_patterns) {
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
                invalidate_search_caches(state);
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

#[cfg(target_os = "macos")]
const EVENT_ID_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

#[cfg(target_os = "macos")]
fn persist_event_id(db_path: &Path, event_id: u64) -> AppResult<()> {
    let conn = db_connection(db_path)?;
    set_meta(&conn, "last_event_id", &event_id.to_string())
}

#[cfg(target_os = "macos")]
fn start_fsevent_watcher_worker(app: AppHandle, state: AppState, since_event_id: Option<u64>) {
    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel();

        let mut watcher =
            match fsevent_watcher::FsEventWatcher::new(&state.home_dir, since_event_id, tx) {
                Ok(w) => w,
                Err(err) => {
                    set_state(
                        &state,
                        IndexState::Error,
                        Some(format!("FSEvents watcher 초기화 실패: {err}")),
                    );
                    emit_index_state(
                        &app,
                        "Error",
                        Some(format!("FSEvents watcher 초기화 실패: {err}")),
                    );
                    return;
                }
            };

        let mut pending_paths: HashSet<PathBuf> = HashSet::new();
        let mut deadline: Option<Instant> = None;
        let mut last_flush = Instant::now();

        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(fsevent_watcher::FsEvent::Paths(paths)) => {
                    for path in paths {
                        if let Some(parent) = path.parent() {
                            pending_paths.insert(parent.to_path_buf());
                        }
                        pending_paths.insert(path);
                    }
                    deadline = Some(Instant::now() + WATCH_DEBOUNCE);
                }
                Ok(fsevent_watcher::FsEvent::MustScanSubDirs(path)) => {
                    let (ignored_roots, ignored_patterns) = cached_effective_ignore_rules(&state);
                    let rows = collect_rows_recursive(&path, &ignored_roots, &ignored_patterns);
                    if !rows.is_empty() {
                        if let Ok(mut conn) = db_connection(&state.db_path) {
                            let _ = upsert_rows(&mut conn, &rows);
                            invalidate_search_caches(&state);
                            let _ = emit_status_counts(&app, &state);
                        }
                    }
                }
                Ok(fsevent_watcher::FsEvent::HistoryDone) => {
                    // Replay complete — flush any pending paths
                    process_watcher_paths(&app, &state, &mut pending_paths);
                    deadline = None;
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

            // Periodic event_id flush
            if last_flush.elapsed() >= EVENT_ID_FLUSH_INTERVAL {
                let eid = watcher.last_event_id();
                let _ = persist_event_id(&state.db_path, eid);
                last_flush = Instant::now();
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

#[cfg(not(target_os = "macos"))]
fn load_system_icon_png(_ext: &str) -> Option<Vec<u8>> {
    None
}

#[tauri::command]
fn get_index_status(state: State<'_, AppState>) -> IndexStatusDto {
    let snapshot = state.status.lock().clone();
    let state_label = if state.indexing_active.load(AtomicOrdering::Acquire) {
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
    start_full_index_worker(app, state.inner().clone())
}

#[tauri::command]
async fn reset_index(app: AppHandle, state: State<'_, AppState>) -> AppResult<()> {
    let state = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        if state.indexing_active.load(AtomicOrdering::Acquire) {
            return Err("인덱싱 진행 중에는 reset 할 수 없습니다.".to_string());
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
        start_full_index_worker(app, state)
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
    let effective_limit = if query.chars().count() <= 1 {
        base_limit.min(SHORT_QUERY_LIMIT)
    } else {
        base_limit
    };
    let offset = offset.unwrap_or(0);

    let sort_by = sort_by.unwrap_or_else(|| "name".to_string());
    let sort_dir = sort_dir.unwrap_or_else(|| "asc".to_string());
    let (runtime_ignored_roots, runtime_ignored_patterns) = cached_effective_ignore_rules(state);

    if !state.db_ready.load(AtomicOrdering::Acquire) {
        return Ok(SearchExecution {
            query,
            sort_by,
            sort_dir,
            effective_limit,
            offset,
            mode_label: "db_not_ready".to_string(),
            results: Vec::new(),
        });
    }

    let is_indexing = matches!(state.status.lock().state, IndexState::Indexing);
    let order_by = sort_clause(&sort_by, &sort_dir, "e.");

    let conn = db_connection(&state.db_path)?;
    let mut results = Vec::with_capacity(effective_limit as usize);

    let mode = parse_query(&query);
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

    match mode {
        SearchMode::Empty => {
            let sql = format!(
                r#"
                SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
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
                    SELECT path, name, dir, is_dir, ext, mtime
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
                    SELECT path, name, dir, is_dir, ext, mtime
                    FROM entries
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
                let phase2_start = Instant::now();
                conn.progress_handler(
                    10_000,
                    Some(move || phase2_start.elapsed().as_millis() > 30),
                );

                let phase2_sql = format!(
                    r#"
                    SELECT path, name, dir, is_dir, ext, mtime
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

                conn.progress_handler(0, None::<fn() -> bool>);
            }
        }

        SearchMode::GlobName { name_like } => {
            let sql = format!(
                r#"
                SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
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
                SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
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
            let resolved_dir = resolve_dir_hint(&state.home_dir, &dir_hint);

            if let Some(abs_dir) = resolved_dir {
                let dir_exact = abs_dir.to_string_lossy().to_string();
                let dir_prefix = format!("{}/", dir_exact);
                let dir_prefix_end = format!("{}0", dir_exact);
                let sql = format!(
                    r#"
                    SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
                    FROM entries e
                    WHERE (e.dir = ?1 OR (e.dir >= ?2 AND e.dir < ?3))
                      AND e.name LIKE ?4 ESCAPE '\'
                    ORDER BY {order_by}
                    LIMIT ?5 OFFSET ?6
                    "#,
                );
                let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(
                        params![
                            dir_exact,
                            dir_prefix,
                            dir_prefix_end,
                            name_like,
                            effective_limit,
                            offset
                        ],
                        row_to_entry,
                    )
                    .map_err(|e| e.to_string())?;
                for row in rows {
                    results.push(row.map_err(|e| e.to_string())?);
                }
            } else {
                let dir_suffix = escape_like(&dir_hint);
                let dir_like_exact = format!("%/{}", dir_suffix);
                let dir_like_sub = format!("%/{}/%", dir_suffix);
                let ext_shortcut = extract_ext_from_like(&name_like);

                if let Some(ext_val) = ext_shortcut {
                    let sql = format!(
                        r#"
                        SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
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
                } else {
                    let path_start = Instant::now();
                    conn.progress_handler(
                        10_000,
                        Some(move || path_start.elapsed().as_millis() > 30),
                    );

                    let sql = format!(
                        r#"
                        SELECT e.path, e.name, e.dir, e.is_dir, e.ext, e.mtime
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

    results = filter_ignored_entries(results, &runtime_ignored_roots, &runtime_ignored_patterns);
    if sort_by == "name" && offset == 0 {
        sort_entries_with_relevance(&mut results, &query, &sort_by, &sort_dir);
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
) -> AppResult<Vec<EntryDto>> {
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

        Ok(execution.results)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open(paths: Vec<String>) -> AppResult<()> {
    tauri::async_runtime::spawn_blocking(move || {
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
    })
    .await
    .map_err(|e| e.to_string())?
}

fn reveal_in_finder_impl(paths: Vec<String>) -> AppResult<()> {
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

fn copy_with_command(program: &str, args: &[&str], text: &str) -> AppResult<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{program} 실행 실패: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("클립보드 쓰기 실패: {e}"))?;
    } else {
        return Err("클립보드 입력 스트림을 열 수 없습니다.".to_string());
    }

    let status = child
        .wait()
        .map_err(|e| format!("{program} 종료 대기 실패: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} 실행이 실패했습니다."))
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
        "지원되는 클립보드 도구를 찾지 못했습니다. wl-copy, xclip, xsel 중 하나를 설치하세요."
            .to_string()
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

        emit_status_counts(&app, &state)?;
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
            let rows =
                collect_rows_recursive(&new_path, &state.path_ignores, &state.path_ignore_patterns);
            for chunk in rows.chunks(BATCH_SIZE) {
                let _ = upsert_rows(&mut conn, chunk)?;
            }
        } else {
            let row = index_row_from_path(&new_path)
                .ok_or_else(|| "변경된 파일 정보를 읽을 수 없습니다.".to_string())?;
            let _ = upsert_rows(&mut conn, &[row])?;
        }

        invalidate_search_caches(&state);

        remember_op(
            &state,
            "rename",
            Some(old_path.to_string_lossy().to_string()),
            Some(new_path.to_string_lossy().to_string()),
        );

        emit_status_counts(&app, &state)?;

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
                &state.home_dir,
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
async fn get_file_icon(ext: String, state: State<'_, AppState>) -> AppResult<Vec<u8>> {
    let state = state.inner().clone();
    Ok(tauri::async_runtime::spawn_blocking(move || {
        let key = if ext.trim().is_empty() {
            "__default__".to_string()
        } else {
            ext.to_lowercase()
        };

        if let Some(cached) = state.icon_cache.lock().get(&key).cloned() {
            return cached;
        }

        let icon = load_system_icon_png(&key).unwrap_or_default();
        state.icon_cache.lock().insert(key, icon.clone());
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
                    index_message: Some("index ready 대기 시간 초과".to_string()),
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
    let bench_mode = bench_mode_enabled();

    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app data dir 조회 실패: {e}"))?;
    fs::create_dir_all(&app_data_dir).map_err(|e| e.to_string())?;

    let db_path = app_data_dir.join("index.db");
    let home_dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));
    let cwd = std::env::current_dir().unwrap_or_else(|_| home_dir.clone());
    let (mut path_ignores, path_ignore_patterns) = load_pathignore_rules(&home_dir, &cwd);
    for root in load_gitignore_roots(&home_dir, &cwd) {
        if !path_ignores.iter().any(|r| r == &root) {
            path_ignores.push(root);
        }
    }

    let state = AppState {
        db_path,
        home_dir,
        cwd,
        path_ignores: Arc::new(path_ignores),
        path_ignore_patterns: Arc::new(path_ignore_patterns),
        db_ready: Arc::new(AtomicBool::new(false)),
        indexing_active: Arc::new(AtomicBool::new(false)),
        status: Arc::new(Mutex::new(IndexStatus::default())),
        recent_ops: Arc::new(Mutex::new(Vec::new())),
        icon_cache: Arc::new(Mutex::new(HashMap::new())),
        fd_search_cache: Arc::new(Mutex::new(None)),
        ignore_cache: Arc::new(Mutex::new(None)),
    };

    app.manage(state.clone());
    if !bench_mode {
        register_global_shortcut(&app.handle())?;
    }
    if bench_mode {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.hide();
        }
    }

    let app_handle = app.handle().clone();
    std::thread::spawn(move || {
        if let Err(err) = init_db(&state.db_path) {
            set_state(&state, IndexState::Error, Some(err.clone()));
            emit_index_state(&app_handle, "Error", Some(err));
            return;
        }
        if let Err(err) = purge_ignored_entries(&state.db_path, &state.path_ignores) {
            set_state(&state, IndexState::Error, Some(err.clone()));
            emit_index_state(&app_handle, "Error", Some(err));
            return;
        }
        state.db_ready.store(true, AtomicOrdering::Release);

        let _ = emit_status_counts(&app_handle, &state);

        #[cfg(target_os = "macos")]
        {
            if bench_mode {
                let _ = start_full_index_worker(app_handle.clone(), state.clone());
            } else {
                let stored_event_id = db_connection(&state.db_path)
                    .ok()
                    .and_then(|conn| get_meta(&conn, "last_event_id"))
                    .and_then(|v| v.parse::<u64>().ok());

                let has_entries = db_connection(&state.db_path)
                    .ok()
                    .and_then(|c| {
                        c.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
                            .ok()
                    })
                    .unwrap_or(0)
                    > 0;

                if stored_event_id.is_some() && has_entries {
                    start_fsevent_watcher_worker(
                        app_handle.clone(),
                        state.clone(),
                        stored_event_id,
                    );
                    let _ = start_full_index_worker(app_handle.clone(), state.clone());
                } else {
                    let _ = start_full_index_worker(app_handle.clone(), state.clone());
                    start_fsevent_watcher_worker(app_handle.clone(), state.clone(), None);
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = start_full_index_worker(app_handle.clone(), state.clone());
        }

        if bench_mode {
            start_bench_runner(app_handle.clone(), state.clone());
        }

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
}
