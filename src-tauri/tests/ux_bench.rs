/// UI-less end-to-end benchmark: indexing, restart, and search latency profiling.
///
/// Run:  cargo test --manifest-path src-tauri/Cargo.toml --test ux_bench -- --nocapture
///
/// Uses the production DB when available, otherwise builds a fresh one.
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use walkdir::WalkDir;

const PROD_DB_PATH: &str =
    "/Users/al02402336/Library/Application Support/com.everything.app/index.db";
const BENCH_DB_DIR: &str = "/tmp/everything_ux_bench";
const DEFAULT_LIMIT: u32 = 300;
const SHORT_QUERY_LIMIT: u32 = 100;
const BATCH_SIZE: usize = 10_000;
const DB_VERSION: i32 = 4;

// SLO thresholds
const SLO_FAST_MS: f64 = 10.0;
const SLO_OK_MS: f64 = 30.0;
const SLO_WARN_MS: f64 = 50.0;

// ── DB helpers ──

fn db_connection(db_path: &Path) -> Connection {
    let conn = Connection::open(db_path).expect("open DB");
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA busy_timeout=5000;",
    )
    .expect("set pragmas");
    conn
}

fn db_connection_perf(db_path: &Path) -> Connection {
    let conn = db_connection(db_path);
    conn.execute_batch(
        "PRAGMA cache_size=-65536;
         PRAGMA mmap_size=268435456;",
    )
    .expect("set perf pragmas");
    conn
}

fn init_db(db_path: &Path) {
    let conn = db_connection(db_path);
    let ver: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if ver != DB_VERSION {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS entries_ai;
             DROP TRIGGER IF EXISTS entries_ad;
             DROP TRIGGER IF EXISTS entries_au;
             DROP TABLE IF EXISTS entries_fts;
             DROP TABLE IF EXISTS entries;",
        )
        .unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {DB_VERSION};"))
            .unwrap();
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS entries (
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
         CREATE INDEX IF NOT EXISTS idx_entries_dir_ext_name_nocase ON entries(dir, ext, name COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
         CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_entries_ext ON entries(ext);
         CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_entries_run_id ON entries(run_id);
         CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .unwrap();
}

fn entry_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .unwrap_or(0)
}

fn db_size_mb(db_path: &Path) -> f64 {
    let main = fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let wal = fs::metadata(db_path.with_extension("db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    (main + wal) as f64 / 1_048_576.0
}

// ── Query parsing (standalone copy) ──

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn glob_to_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    for ch in pattern.chars() {
        match ch {
            '*' => out.push('%'),
            '?' => out.push('_'),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out
}

fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

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

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum SearchMode {
    Empty,
    NameSearch { name_like: String },
    GlobName { name_like: String },
    ExtSearch { ext: String },
    PathSearch { path_like: String, name_like: String },
}

fn parse_query(query: &str) -> SearchMode {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return SearchMode::Empty;
    }
    if trimmed.contains('/') {
        let last_slash = trimmed.rfind('/').unwrap();
        let dir_part = trimmed[..last_slash].trim();
        let name_part = trimmed[last_slash + 1..].trim();
        let path_like = if dir_part.is_empty() {
            "%".to_string()
        } else if has_glob_chars(dir_part) {
            format!("%{}/%", glob_to_like(dir_part))
        } else {
            format!("%{}/%", escape_like(dir_part))
        };
        let name_like = if name_part.is_empty() {
            "%".to_string()
        } else if has_glob_chars(name_part) {
            glob_to_like(name_part)
        } else {
            format!("%{}%", escape_like(name_part))
        };
        return SearchMode::PathSearch {
            path_like,
            name_like,
        };
    }
    if let Some(ext_part) = trimmed.strip_prefix("*.") {
        if !ext_part.is_empty() && !ext_part.contains('/') && !has_glob_chars(ext_part) {
            return SearchMode::ExtSearch {
                ext: ext_part.to_lowercase(),
            };
        }
    }
    if has_glob_chars(trimmed) {
        return SearchMode::GlobName {
            name_like: glob_to_like(trimmed),
        };
    }
    SearchMode::NameSearch {
        name_like: format!("%{}%", escape_like(trimmed)),
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
        _ => format!("{prefix}name COLLATE NOCASE ASC, {prefix}path COLLATE NOCASE ASC"),
    }
}

fn mode_label(mode: &SearchMode) -> &'static str {
    match mode {
        SearchMode::Empty => "empty",
        SearchMode::NameSearch { .. } => "name",
        SearchMode::GlobName { .. } => "glob",
        SearchMode::ExtSearch { .. } => "ext",
        SearchMode::PathSearch { .. } => "path",
    }
}

// ── Search execution ──

struct SearchResult {
    count: usize,
    first_names: Vec<String>,
    mode: SearchMode,
}

fn run_search(conn: &Connection, query: &str, limit: u32) -> SearchResult {
    run_search_sorted(conn, query, limit, "name", "asc")
}

fn run_search_sorted(
    conn: &Connection,
    query: &str,
    limit: u32,
    sort_by: &str,
    sort_dir: &str,
) -> SearchResult {
    let effective_limit = if query.trim().chars().count() <= 1 {
        limit.min(SHORT_QUERY_LIMIT)
    } else {
        limit
    };
    let order_by = sort_clause(sort_by, sort_dir, "e.");
    let bare_order = sort_clause(sort_by, sort_dir, "");
    let mode = parse_query(query);
    let mut names: Vec<String> = Vec::new();

    match &mode {
        SearchMode::Empty => {
            let sql = format!("SELECT e.name FROM entries e ORDER BY {order_by} LIMIT ?1");
            let mut stmt = conn.prepare_cached(&sql).unwrap();
            let rows = stmt
                .query_map(params![effective_limit], |row| row.get::<_, String>(0))
                .unwrap();
            for r in rows.flatten() {
                names.push(r);
            }
        }
        SearchMode::NameSearch { name_like } => {
            let escaped_query = escape_like(query.trim());
            let exact_query = query.trim().to_string();
            let prefix_like = format!("{}%", escaped_query);

            // Phase 1a: exact match
            {
                let sql = format!(
                    "SELECT name FROM entries
                     WHERE name COLLATE NOCASE = ?1
                     ORDER BY {bare_order} LIMIT ?2"
                );
                let mut stmt = conn.prepare_cached(&sql).unwrap();
                let rows = stmt
                    .query_map(params![exact_query, effective_limit], |row| {
                        row.get::<_, String>(0)
                    })
                    .unwrap();
                for r in rows.flatten() {
                    names.push(r);
                }
            }

            // Phase 1b: prefix match
            if (names.len() as u32) < effective_limit {
                let remaining = effective_limit - names.len() as u32;
                let sql = format!(
                    "SELECT name FROM entries INDEXED BY idx_entries_name_nocase
                     WHERE name LIKE ?1 ESCAPE '\\'
                       AND name COLLATE NOCASE != ?2
                     ORDER BY {bare_order} LIMIT ?3"
                );
                let mut stmt = conn.prepare_cached(&sql).unwrap();
                let rows = stmt
                    .query_map(params![prefix_like, exact_query, remaining], |row| {
                        row.get::<_, String>(0)
                    })
                    .unwrap();
                for r in rows.flatten() {
                    names.push(r);
                }
            }

            // Phase 2a: quick existence probe (tight budget)
            if names.is_empty() {
                let probe_start = Instant::now();
                conn.progress_handler(5_000, Some(move || probe_start.elapsed().as_millis() > 8));

                let probe_sql =
                    "SELECT 1 FROM entries
                     WHERE name LIKE ?1 ESCAPE '\\'
                       AND name COLLATE NOCASE != ?2
                       AND name NOT LIKE ?3 ESCAPE '\\'
                     LIMIT 1";
                let has_match = conn
                    .prepare(probe_sql)
                    .and_then(|mut s| {
                        s.query_row(params![name_like, exact_query, prefix_like], |_| Ok(true))
                    })
                    .unwrap_or(false);
                conn.progress_handler(0, None::<fn() -> bool>);

                // Phase 2b: full fetch only when probe found a match
                if has_match {
                    let start = Instant::now();
                    conn.progress_handler(10_000, Some(move || start.elapsed().as_millis() > 30));

                    let sql = format!(
                        "SELECT name FROM entries
                         WHERE name LIKE ?1 ESCAPE '\\'
                           AND name COLLATE NOCASE != ?2
                           AND name NOT LIKE ?3 ESCAPE '\\'
                         ORDER BY {bare_order} LIMIT ?4"
                    );
                    if let Ok(mut stmt) = conn.prepare(&sql) {
                        if let Ok(rows) = stmt.query_map(
                            params![name_like, exact_query, prefix_like, effective_limit],
                            |row| row.get::<_, String>(0),
                        ) {
                            for r in rows {
                                match r {
                                    Ok(name) => names.push(name),
                                    Err(_) => break,
                                }
                            }
                        }
                    }
                    conn.progress_handler(0, None::<fn() -> bool>);
                }
            }
        }
        SearchMode::GlobName { name_like } => {
            let sql = format!(
                "SELECT e.name FROM entries e WHERE e.name LIKE ?1 ESCAPE '\\' ORDER BY {order_by} LIMIT ?2"
            );
            let mut stmt = conn.prepare_cached(&sql).unwrap();
            let rows = stmt
                .query_map(params![name_like, effective_limit], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap();
            for r in rows.flatten() {
                names.push(r);
            }
        }
        SearchMode::ExtSearch { ext } => {
            let sql = format!(
                "SELECT e.name FROM entries e WHERE e.ext = ?1 ORDER BY {order_by} LIMIT ?2"
            );
            let mut stmt = conn.prepare_cached(&sql).unwrap();
            let rows = stmt
                .query_map(params![ext, effective_limit], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap();
            for r in rows.flatten() {
                names.push(r);
            }
        }
        SearchMode::PathSearch {
            path_like: _,
            name_like,
        } => {
            let trimmed = query.trim();
            let last_slash = trimmed.rfind('/').unwrap_or(0);
            let dir_hint = trimmed[..last_slash].trim();
            let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            let home = Path::new(&home_dir);

            let resolved = if !dir_hint.is_empty() && !has_glob_chars(dir_hint) {
                let candidate = if dir_hint.starts_with('/') {
                    PathBuf::from(dir_hint)
                } else {
                    home.join(dir_hint)
                };
                if candidate.is_dir() {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(abs_dir) = resolved {
                let dir_exact = abs_dir.to_string_lossy().to_string();
                let dir_prefix = format!("{}/", dir_exact);
                let dir_prefix_end = format!("{}0", dir_exact);
                let sql = format!(
                    "SELECT e.name FROM entries e \
                     WHERE (e.dir = ?1 OR (e.dir >= ?2 AND e.dir < ?3)) \
                       AND e.name LIKE ?4 ESCAPE '\\' \
                     ORDER BY {order_by} LIMIT ?5"
                );
                let mut stmt = conn.prepare_cached(&sql).unwrap();
                let rows = stmt
                    .query_map(
                        params![dir_exact, dir_prefix, dir_prefix_end, name_like, effective_limit],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap();
                for r in rows.flatten() {
                    names.push(r);
                }
            } else {
                let dir_suffix = escape_like(dir_hint);
                let dir_like_exact = format!("%/{}", dir_suffix);
                let dir_like_sub = format!("%/{}/%", dir_suffix);
                let ext_shortcut = extract_ext_from_like(name_like);

                if let Some(ext_val) = ext_shortcut {
                    let sql = format!(
                        "SELECT e.name FROM entries e \
                         WHERE e.ext = ?1 \
                           AND (e.dir LIKE ?2 ESCAPE '\\' OR e.dir LIKE ?3 ESCAPE '\\') \
                         ORDER BY {order_by} LIMIT ?4"
                    );
                    let mut stmt = conn.prepare_cached(&sql).unwrap();
                    let rows = stmt
                        .query_map(
                            params![ext_val, dir_like_exact, dir_like_sub, effective_limit],
                            |row| row.get::<_, String>(0),
                        )
                        .unwrap();
                    for r in rows.flatten() {
                        names.push(r);
                    }
                } else {
                    // Phase A: fast prefix search via name index
                    let prefix_like = if name_like.starts_with('%') {
                        let rest = &name_like[1..];
                        if !rest.is_empty() && !rest.starts_with('%') {
                            Some(rest.to_string())
                        } else {
                            None
                        }
                    } else {
                        Some(name_like.to_string())
                    };

                    if let Some(ref pfx) = prefix_like {
                        let sql = format!(
                            "SELECT e.name FROM entries e INDEXED BY idx_entries_name_nocase \
                             WHERE e.name LIKE ?1 ESCAPE '\\' \
                               AND (e.dir LIKE ?2 ESCAPE '\\' OR e.dir LIKE ?3 ESCAPE '\\') \
                             ORDER BY {order_by} LIMIT ?4"
                        );
                        if let Ok(mut stmt) = conn.prepare_cached(&sql) {
                            if let Ok(rows) = stmt.query_map(
                                params![pfx, dir_like_exact, dir_like_sub, effective_limit],
                                |row| row.get::<_, String>(0),
                            ) {
                                for r in rows.flatten() {
                                    names.push(r);
                                }
                            }
                        }
                    }

                    // Phase B: time-budgeted contains fallback if prefix found too few
                    if (names.len() as i64) < effective_limit as i64 {
                        let start = Instant::now();
                        conn.progress_handler(10_000, Some(move || start.elapsed().as_millis() > 30));

                        let sql = format!(
                            "SELECT e.name FROM entries e \
                             WHERE (e.dir LIKE ?1 ESCAPE '\\' OR e.dir LIKE ?2 ESCAPE '\\') \
                               AND e.name LIKE ?3 ESCAPE '\\' \
                             ORDER BY {order_by} LIMIT ?4"
                        );
                        if let Ok(mut stmt) = conn.prepare_cached(&sql) {
                            if let Ok(rows) = stmt.query_map(
                                params![dir_like_exact, dir_like_sub, name_like, effective_limit],
                                |row| row.get::<_, String>(0),
                            ) {
                                for r in rows {
                                    match r {
                                        Ok(name) => names.push(name),
                                        Err(_) => break,
                                    }
                                }
                            }
                        }
                        conn.progress_handler(0, None::<fn() -> bool>);

                        let mut seen = std::collections::HashSet::new();
                        names.retain(|n| seen.insert(n.clone()));
                        names.truncate(effective_limit as usize);
                    }
                }
            }
        }
    }

    let count = names.len();
    let first_names: Vec<String> = names.into_iter().take(5).collect();
    SearchResult {
        count,
        first_names,
        mode,
    }
}

// ── EXPLAIN helper for slow query diagnosis ──

fn explain_search(conn: &Connection, query: &str) {
    let mode = parse_query(query);
    let order_by = sort_clause("name", "asc", "e.");
    let bare_order = sort_clause("name", "asc", "");

    let explain_sql = match &mode {
        SearchMode::Empty => {
            format!("EXPLAIN QUERY PLAN SELECT e.name FROM entries e ORDER BY {order_by} LIMIT 300")
        }
        SearchMode::NameSearch { .. } => {
            let escaped = escape_like(query.trim());
            let prefix_like = format!("{}%", escaped);
            // Show phase 1b (prefix) plan which is the main query path
            format!(
                "EXPLAIN QUERY PLAN SELECT name FROM entries \
                 WHERE name LIKE '{}' ESCAPE '\\' ORDER BY {bare_order} LIMIT 300",
                prefix_like
            )
        }
        SearchMode::GlobName { name_like } => {
            format!(
                "EXPLAIN QUERY PLAN SELECT e.name FROM entries e \
                 WHERE e.name LIKE '{}' ESCAPE '\\' ORDER BY {order_by} LIMIT 300",
                name_like
            )
        }
        SearchMode::ExtSearch { ext } => {
            format!(
                "EXPLAIN QUERY PLAN SELECT e.name FROM entries e \
                 WHERE e.ext = '{}' ORDER BY {order_by} LIMIT 300",
                ext
            )
        }
        SearchMode::PathSearch { name_like, .. } => {
            let trimmed = query.trim();
            let last_slash = trimmed.rfind('/').unwrap_or(0);
            let dir_hint = trimmed[..last_slash].trim();
            let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
            let home = Path::new(&home_dir);
            let resolved = if !dir_hint.is_empty() && !has_glob_chars(dir_hint) {
                let candidate = if dir_hint.starts_with('/') {
                    PathBuf::from(dir_hint)
                } else {
                    home.join(dir_hint)
                };
                if candidate.is_dir() {
                    Some(candidate)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(abs_dir) = resolved {
                let dir_exact = abs_dir.to_string_lossy().to_string();
                let dir_prefix = format!("{}/", dir_exact);
                format!(
                    "EXPLAIN QUERY PLAN SELECT e.name FROM entries e \
                     WHERE (e.dir = '{dir_exact}' OR (e.dir >= '{dir_prefix}' AND e.dir < '{dir_prefix}0')) \
                       AND e.name LIKE '{name_like}' ESCAPE '\\' \
                     ORDER BY {order_by} LIMIT 300"
                )
            } else {
                let dir_suffix = escape_like(dir_hint);
                format!(
                    "EXPLAIN QUERY PLAN SELECT e.name FROM entries e \
                     WHERE (e.dir LIKE '%/{dir_suffix}' ESCAPE '\\' OR e.dir LIKE '%/{dir_suffix}/%' ESCAPE '\\') \
                       AND e.name LIKE '{name_like}' ESCAPE '\\' \
                     ORDER BY {order_by} LIMIT 300"
                )
            }
        }
    };

    if let Ok(mut stmt) = conn.prepare(&explain_sql) {
        if let Ok(rows) = stmt.query_map([], |row| {
            let detail: String = row.get(3)?;
            Ok(detail)
        }) {
            for r in rows.flatten() {
                println!("           PLAN: {r}");
            }
        }
    }
}

// ── Indexing ──

const BUILTIN_SKIP_NAMES: &[&str] = &[
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
];

const BUILTIN_SKIP_PATHS: &[&str] = &[
    "Library/Caches",
    "Library/Developer/CoreSimulator",
    "Library/Logs",
    ".vscode/extensions",
];

fn should_skip_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if s.split('/').any(|seg| BUILTIN_SKIP_NAMES.contains(&seg)) {
        return true;
    }
    BUILTIN_SKIP_PATHS.iter().any(|pat| {
        let infix = format!("/{pat}/");
        let suffix = format!("/{pat}");
        s.contains(&infix) || s.ends_with(&suffix)
    })
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

struct IndexRow {
    path: String,
    name: String,
    dir: String,
    is_dir: i64,
    ext: Option<String>,
    mtime: Option<i64>,
    size: Option<i64>,
}

fn index_row_from_entry(entry: &walkdir::DirEntry) -> Option<IndexRow> {
    let path = entry.path();
    let metadata = entry.metadata().ok()?;
    let is_dir = metadata.is_dir();
    let name = path
        .file_name()
        .map(|v| v.to_string_lossy().to_string())?;
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
    let ext = if is_dir {
        None
    } else {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
    };
    Some(IndexRow {
        path: path.to_string_lossy().to_string(),
        name,
        dir,
        is_dir: if is_dir { 1 } else { 0 },
        ext,
        mtime,
        size,
    })
}

fn upsert_batch(conn: &mut Connection, rows: &[IndexRow], run_id: i64) {
    if rows.is_empty() {
        return;
    }
    let tx = conn.transaction().unwrap();
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at, run_id)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(path) DO UPDATE SET
                   name=excluded.name, dir=excluded.dir, is_dir=excluded.is_dir,
                   ext=excluded.ext, mtime=excluded.mtime, size=excluded.size,
                   indexed_at=excluded.indexed_at, run_id=excluded.run_id",
            )
            .unwrap();
        let now = now_epoch();
        for row in rows {
            let _ = stmt.execute(params![
                row.path, row.name, row.dir, row.is_dir, row.ext, row.mtime, row.size, now,
                run_id,
            ]);
        }
    }
    tx.commit().unwrap();
}

struct IndexProgress {
    elapsed_ms: u128,
    scanned: u64,
    indexed: u64,
    db_entries: i64,
    current_path: String,
}

/// Full index with progress snapshots every 2 seconds.
fn run_full_index_with_progress(
    db_path: &Path,
    home_dir: &Path,
    scanned_counter: &AtomicU64,
    indexed_counter: &AtomicU64,
    done_flag: &AtomicBool,
) -> (u64, u64, Duration, Vec<IndexProgress>) {
    let start = Instant::now();
    let mut conn = db_connection(db_path);
    conn.execute_batch(
        "PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-65536;
         PRAGMA mmap_size=268435456;
         PRAGMA wal_autocheckpoint=0;",
    )
    .unwrap();

    let run_id: i64 = 1;
    let mut batch: Vec<IndexRow> = Vec::with_capacity(BATCH_SIZE);
    let mut scanned: u64 = 0;
    let mut indexed: u64 = 0;
    let mut snapshots: Vec<IndexProgress> = Vec::new();
    let mut last_snapshot = Instant::now();
    let iter = WalkDir::new(home_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_skip_path(entry.path()));

    for entry in iter {
        let Ok(entry) = entry else {
            scanned += 1;
            scanned_counter.store(scanned, Ordering::Relaxed);
            continue;
        };
        scanned += 1;
        let current_path = entry.path().to_string_lossy().to_string();

        if let Some(row) = index_row_from_entry(&entry) {
            indexed += 1;
            batch.push(row);
        }

        if batch.len() >= BATCH_SIZE {
            upsert_batch(&mut conn, &batch, run_id);
            batch.clear();
            scanned_counter.store(scanned, Ordering::Relaxed);
            indexed_counter.store(indexed, Ordering::Relaxed);
        }

        if last_snapshot.elapsed() >= Duration::from_secs(2) {
            let db_entries = entry_count(&conn);
            snapshots.push(IndexProgress {
                elapsed_ms: start.elapsed().as_millis(),
                scanned,
                indexed,
                db_entries,
                current_path: current_path.clone(),
            });
            last_snapshot = Instant::now();
        }
    }

    if !batch.is_empty() {
        upsert_batch(&mut conn, &batch, run_id);
    }

    scanned_counter.store(scanned, Ordering::Relaxed);
    indexed_counter.store(indexed, Ordering::Relaxed);

    conn.execute("DELETE FROM entries WHERE run_id < ?1", params![run_id])
        .unwrap();
    let _ = conn.execute_batch("ANALYZE");
    conn.execute_batch(
        "PRAGMA wal_autocheckpoint=1000;
         PRAGMA cache_size=-16384;
         PRAGMA mmap_size=0;",
    )
    .unwrap();
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");

    done_flag.store(true, Ordering::Release);
    (scanned, indexed, start.elapsed(), snapshots)
}

// ── Benchmark runner ──

#[allow(dead_code)]
struct CaseResult {
    id: String,
    query: String,
    mode: String,
    sort_by: String,
    sort_dir: String,
    count: usize,
    first_names: Vec<String>,
    timings_ms: Vec<f64>,
    min_ms: f64,
    avg_ms: f64,
    max_ms: f64,
}

struct TestCase {
    id: &'static str,
    query: &'static str,
    sort_by: &'static str,
    sort_dir: &'static str,
    runs: usize,
    description: &'static str,
}

fn realistic_test_cases() -> Vec<TestCase> {
    vec![
        // ── Scenario 1: User opens app, types incrementally ──
        TestCase {
            id: "S01_single_char",
            query: "r",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "User types first character 'r'",
        },
        TestCase {
            id: "S02_two_chars",
            query: "re",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "User types 're'",
        },
        TestCase {
            id: "S03_prefix",
            query: "read",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "User types 'read'",
        },
        TestCase {
            id: "S04_full_name",
            query: "readme",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "User types full 'readme'",
        },
        // ── Scenario 2: Looking for specific file types ──
        TestCase {
            id: "S05_ext_rs",
            query: "*.rs",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find all Rust source files",
        },
        TestCase {
            id: "S06_ext_json",
            query: "*.json",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find all JSON files",
        },
        TestCase {
            id: "S07_ext_png",
            query: "*.png",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find all PNG images",
        },
        TestCase {
            id: "S08_ext_md",
            query: "*.md",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find all Markdown files",
        },
        TestCase {
            id: "S09_ext_pdf",
            query: "*.pdf",
            sort_by: "mtime",
            sort_dir: "desc",
            runs: 5,
            description: "Find PDFs sorted by newest",
        },
        // ── Scenario 3: Finding config/project files ──
        TestCase {
            id: "S10_package_json",
            query: "package.json",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find package.json files",
        },
        TestCase {
            id: "S11_cargo_toml",
            query: "Cargo.toml",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find Cargo.toml files",
        },
        TestCase {
            id: "S12_gitignore",
            query: ".gitignore",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find .gitignore files",
        },
        TestCase {
            id: "S13_makefile",
            query: "Makefile",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find Makefiles",
        },
        TestCase {
            id: "S14_dockerfile",
            query: "Dockerfile",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find Dockerfiles",
        },
        // ── Scenario 4: Path-based search ──
        TestCase {
            id: "S15_path_src_main",
            query: "src/main",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Search for 'main' inside src/ dirs",
        },
        TestCase {
            id: "S16_path_documents",
            query: "Documents/",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "List contents of Documents",
        },
        TestCase {
            id: "S17_path_desktop_png",
            query: "Desktop/*.png",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "PNGs on Desktop",
        },
        TestCase {
            id: "S18_path_src_rs",
            query: "src/*.rs",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Rust files in src/ dirs",
        },
        // ── Scenario 5: Glob patterns ──
        TestCase {
            id: "S19_glob_test_star",
            query: "test*",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "All test-prefixed files/dirs",
        },
        TestCase {
            id: "S20_glob_config_star",
            query: "config*",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "All config-prefixed files",
        },
        TestCase {
            id: "S21_glob_index_star",
            query: "index*",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "All index-prefixed files",
        },
        TestCase {
            id: "S22_glob_question",
            query: "spec?.md",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Glob with ? (single char wildcard)",
        },
        // ── Scenario 6: Common user queries ──
        TestCase {
            id: "S23_download",
            query: "download",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Search for 'download'",
        },
        TestCase {
            id: "S24_screenshot",
            query: "screenshot",
            sort_by: "mtime",
            sort_dir: "desc",
            runs: 5,
            description: "Search screenshots by newest",
        },
        TestCase {
            id: "S25_log",
            query: "*.log",
            sort_by: "mtime",
            sort_dir: "desc",
            runs: 5,
            description: "Find log files by newest",
        },
        TestCase {
            id: "S26_env",
            query: ".env",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Find .env files",
        },
        // ── Scenario 7: Empty / no-match ──
        TestCase {
            id: "S27_empty",
            query: "",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Empty query (list all)",
        },
        TestCase {
            id: "S28_no_match",
            query: "zzzzxyznofile_definitely_not_existing_ever",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Query with zero matches",
        },
        // ── Scenario 8: Sort variations ──
        TestCase {
            id: "S29_mtime_desc",
            query: "readme",
            sort_by: "mtime",
            sort_dir: "desc",
            runs: 5,
            description: "readme sorted by newest first",
        },
        TestCase {
            id: "S30_name_desc",
            query: "*.ts",
            sort_by: "name",
            sort_dir: "desc",
            runs: 5,
            description: "TypeScript files Z→A",
        },
        // ── Scenario 9: Edge cases ──
        TestCase {
            id: "S31_dots",
            query: ".DS_Store",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "macOS dotfiles",
        },
        TestCase {
            id: "S32_spaces",
            query: "Screen Shot",
            sort_by: "name",
            sort_dir: "asc",
            runs: 5,
            description: "Query with spaces",
        },
        TestCase {
            id: "S33_unicode",
            query: "한글",
            sort_by: "name",
            sort_dir: "asc",
            runs: 3,
            description: "Korean characters",
        },
        TestCase {
            id: "S34_long_query",
            query: "this is a very long search query that should not match anything at all",
            sort_by: "name",
            sort_dir: "asc",
            runs: 3,
            description: "Very long query (no match expected)",
        },
    ]
}

fn run_test_cases(conn: &Connection, label: &str) -> Vec<CaseResult> {
    let sep = "=".repeat(100);
    println!("\n{sep}");
    println!("  {label}");
    println!("{sep}");

    let count = entry_count(conn);
    let db_path_str = "(in-memory or connected)";
    println!("  DB entries: {count}  |  Connection: {db_path_str}");

    // Warmup
    let cases = realistic_test_cases();
    for tc in &cases {
        let _ = run_search(conn, tc.query, DEFAULT_LIMIT);
    }
    println!("  Cache warmed ({} queries)\n", cases.len());

    let mut results: Vec<CaseResult> = Vec::new();
    let mut slow_queries: Vec<(String, f64)> = Vec::new();

    let mut current_scenario = "";

    for tc in &cases {
        // Print scenario header when it changes
        let scenario = tc.id.split('_').next().unwrap_or("");
        if scenario != current_scenario {
            current_scenario = scenario;
            println!("  ── {} ──", tc.description.split('\'').next().unwrap_or(tc.description));
        }

        let mut timings = Vec::with_capacity(tc.runs);
        let mut last_result = SearchResult {
            count: 0,
            first_names: Vec::new(),
            mode: SearchMode::Empty,
        };

        for _ in 0..tc.runs {
            let start = Instant::now();
            last_result = run_search_sorted(conn, tc.query, DEFAULT_LIMIT, tc.sort_by, tc.sort_dir);
            timings.push(start.elapsed().as_secs_f64() * 1000.0);
        }

        let min_ms = timings.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_ms = timings.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let avg_ms = timings.iter().sum::<f64>() / timings.len() as f64;

        let status = if avg_ms > SLO_WARN_MS {
            "SLOW"
        } else if avg_ms > SLO_OK_MS {
            "WARN"
        } else if avg_ms < SLO_FAST_MS {
            "FAST"
        } else {
            " OK "
        };

        let query_display = if tc.query.is_empty() {
            "(empty)".to_string()
        } else if tc.query.len() > 40 {
            format!("{}...", &tc.query[..37])
        } else {
            tc.query.to_string()
        };

        let sort_info = if tc.sort_by != "name" || tc.sort_dir != "asc" {
            format!(" [{}:{}]", tc.sort_by, tc.sort_dir)
        } else {
            String::new()
        };

        println!(
            "    [{status}] {:<6} {:<42}{} mode={:<5} cnt={:<4} min={:>7.2}ms avg={:>7.2}ms max={:>7.2}ms",
            tc.id,
            query_display,
            sort_info,
            mode_label(&last_result.mode),
            last_result.count,
            min_ms,
            avg_ms,
            max_ms,
        );

        if !last_result.first_names.is_empty() {
            let preview: Vec<&str> = last_result
                .first_names
                .iter()
                .take(3)
                .map(|s| s.as_str())
                .collect();
            println!("           top results: {:?}", preview);
        }

        if avg_ms > SLO_OK_MS {
            slow_queries.push((tc.query.to_string(), avg_ms));
        }

        results.push(CaseResult {
            id: tc.id.to_string(),
            query: tc.query.to_string(),
            mode: mode_label(&last_result.mode).to_string(),
            sort_by: tc.sort_by.to_string(),
            sort_dir: tc.sort_dir.to_string(),
            count: last_result.count,
            first_names: last_result.first_names,
            timings_ms: timings,
            min_ms,
            avg_ms,
            max_ms,
        });
    }

    // ── Summary statistics ──
    println!("\n  ── Summary ──");
    let all_avg: Vec<f64> = results.iter().map(|r| r.avg_ms).collect();
    let total = all_avg.len();
    let overall_avg = all_avg.iter().sum::<f64>() / total as f64;
    let overall_max = all_avg.iter().cloned().fold(0.0_f64, f64::max);
    let mut sorted = all_avg.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[total / 2];
    let p95 = sorted[((total as f64 * 0.95) as usize).min(total - 1)];
    let p99 = sorted[((total as f64 * 0.99) as usize).min(total - 1)];

    let fast_count = results.iter().filter(|r| r.avg_ms < SLO_FAST_MS).count();
    let ok_count = results
        .iter()
        .filter(|r| r.avg_ms >= SLO_FAST_MS && r.avg_ms <= SLO_OK_MS)
        .count();
    let warn_count = results
        .iter()
        .filter(|r| r.avg_ms > SLO_OK_MS && r.avg_ms <= SLO_WARN_MS)
        .count();
    let slow_count = results.iter().filter(|r| r.avg_ms > SLO_WARN_MS).count();

    println!("  Total: {total} test cases");
    println!(
        "  FAST(<{SLO_FAST_MS}ms): {fast_count}  |  OK(<{SLO_OK_MS}ms): {ok_count}  |  WARN(<{SLO_WARN_MS}ms): {warn_count}  |  SLOW(>{SLO_WARN_MS}ms): {slow_count}"
    );
    println!(
        "  Avg: {overall_avg:.2}ms  |  P50: {p50:.2}ms  |  P95: {p95:.2}ms  |  P99: {p99:.2}ms  |  Max: {overall_max:.2}ms"
    );
    println!(
        "  SLO (p95 < 30ms): {}",
        if p95 < 30.0 { "PASS" } else { "FAIL" }
    );

    // ── Slow query diagnosis ──
    if !slow_queries.is_empty() {
        println!("\n  ── Slow Query Diagnosis (>{SLO_OK_MS}ms avg) ──");
        for (query, avg) in &slow_queries {
            let q_display = if query.is_empty() {
                "(empty)"
            } else {
                query
            };
            println!("\n    Query: {:?}  avg={:.2}ms", q_display, avg);
            explain_search(conn, query);
        }
    }

    // ── Per-mode breakdown ──
    println!("\n  ── Per-mode Latency ──");
    let mut mode_map: HashMap<String, Vec<f64>> = HashMap::new();
    for r in &results {
        mode_map
            .entry(r.mode.clone())
            .or_default()
            .push(r.avg_ms);
    }
    let mut modes: Vec<_> = mode_map.into_iter().collect();
    modes.sort_by_key(|(k, _)| k.clone());
    for (mode, latencies) in &modes {
        let mode_avg = latencies.iter().sum::<f64>() / latencies.len() as f64;
        let mode_max = latencies.iter().cloned().fold(0.0_f64, f64::max);
        println!(
            "    {:<6} n={:<3} avg={:.2}ms  max={:.2}ms",
            mode,
            latencies.len(),
            mode_avg,
            mode_max,
        );
    }

    println!();
    results
}

// ── Tests ──

/// Scenario A: Index from scratch with detailed progress logging.
/// Measures: total time, throughput, progress snapshots.
#[test]
fn scenario_a_fresh_index() {
    let home_dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));
    let bench_dir = PathBuf::from(BENCH_DB_DIR);
    let _ = fs::remove_dir_all(&bench_dir);
    fs::create_dir_all(&bench_dir).expect("create bench dir");
    let db_path = bench_dir.join("ux_bench.db");
    init_db(&db_path);

    let sep = "=".repeat(100);
    println!("\n{sep}");
    println!("  SCENARIO A: Fresh Index (full $HOME scan)");
    println!("{sep}");
    println!("  Home: {}", home_dir.display());
    println!("  DB:   {}", db_path.display());

    let scanned = Arc::new(AtomicU64::new(0));
    let indexed = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let dp = db_path.clone();
    let hd = home_dir.clone();
    let sc = scanned.clone();
    let ix = indexed.clone();
    let dn = done.clone();

    let index_thread =
        std::thread::spawn(move || run_full_index_with_progress(&dp, &hd, &sc, &ix, &dn));

    // Monitor progress from main thread
    println!("\n  ── Index Progress ──");
    println!(
        "  {:>8}  {:>12}  {:>12}  {:>10}  {:>10}",
        "Time(s)", "Scanned", "Indexed", "DB Entries", "Rate/s"
    );
    let monitor_start = Instant::now();
    let mut last_scanned = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let s = scanned.load(Ordering::Relaxed);
        let i = indexed.load(Ordering::Relaxed);
        let elapsed = monitor_start.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            (s - last_scanned) as f64 / 3.0
        } else {
            0.0
        };
        last_scanned = s;

        let conn = db_connection(&db_path);
        let db_count = entry_count(&conn);

        println!(
            "  {:>7.1}s  {:>12}  {:>12}  {:>10}  {:>9.0}/s",
            elapsed, s, i, db_count, rate,
        );

        if done.load(Ordering::Acquire) {
            break;
        }
    }

    let (total_scanned, total_indexed, elapsed, snapshots) = index_thread.join().unwrap();
    let conn = db_connection_perf(&db_path);
    let final_count = entry_count(&conn);
    let size = db_size_mb(&db_path);

    println!("\n  ── Index Complete ──");
    println!("  Total scanned:  {total_scanned}");
    println!("  Total indexed:  {total_indexed}");
    println!("  DB entries:     {final_count}");
    println!("  DB size:        {size:.1} MB");
    println!("  Elapsed:        {:.1}s", elapsed.as_secs_f64());
    println!(
        "  Throughput:     {:.0} entries/sec",
        total_indexed as f64 / elapsed.as_secs_f64()
    );

    if !snapshots.is_empty() {
        println!("\n  ── Progress Snapshots ({} captured) ──", snapshots.len());
        for (i, snap) in snapshots.iter().enumerate() {
            if i % 5 == 0 || i == snapshots.len() - 1 {
                let home = home_dir.to_string_lossy();
                let short = snap.current_path.strip_prefix(home.as_ref()).unwrap_or(&snap.current_path);
                println!(
                    "    [{:>5}ms] scanned={:<10} indexed={:<10} db={:<10} path=~{}",
                    snap.elapsed_ms,
                    snap.scanned,
                    snap.indexed,
                    snap.db_entries,
                    short,
                );
            }
        }
    }

    // Run search tests on freshly indexed DB
    run_test_cases(&conn, "SCENARIO A: Search on Fresh Index");

    // Cleanup
    let _ = fs::remove_dir_all(&bench_dir);
}

/// Scenario B: Restart simulation — open existing production DB and search immediately.
/// Measures: cold-start latency, warm-up curve, sort variation impact.
#[test]
fn scenario_b_restart_and_search() {
    let db_path = PathBuf::from(PROD_DB_PATH);
    if !db_path.exists() {
        println!("SKIP: Production DB not found at {PROD_DB_PATH}");
        return;
    }

    let sep = "=".repeat(100);
    println!("\n{sep}");
    println!("  SCENARIO B: Restart Simulation (existing production DB)");
    println!("{sep}");

    let size = db_size_mb(&db_path);
    println!("  DB path: {PROD_DB_PATH}");
    println!("  DB size: {size:.1} MB");

    // ── B1: Cold open (no perf pragmas) ──
    println!("\n  ── B1: Cold Start (default pragmas) ──");
    {
        let conn = db_connection(&db_path);
        let count = entry_count(&conn);
        println!("  DB entries: {count}");

        let cold_queries = [
            "readme",
            "*.rs",
            "package.json",
            "src/main",
            "a",
            "test*",
            "Documents/",
            ".gitignore",
            "*.png",
            "Cargo.toml",
        ];

        for query in &cold_queries {
            let start = Instant::now();
            let result = run_search(&conn, query, DEFAULT_LIMIT);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let status = if ms > SLO_WARN_MS {
                "SLOW"
            } else if ms > SLO_OK_MS {
                "WARN"
            } else {
                " OK "
            };
            println!(
                "    [{status}] {:<30} cnt={:<4} {:.2}ms",
                format!("{:?}", query),
                result.count,
                ms,
            );
        }
    }

    // ── B2: With perf pragmas (simulates normal app state) ──
    println!("\n  ── B2: With Perf Pragmas (mmap + 64MB cache) ──");
    {
        let conn = db_connection_perf(&db_path);
        run_test_cases(&conn, "SCENARIO B2: Full benchmark with perf pragmas");
    }

    // ── B3: Warm-up curve ──
    println!("  ── B3: Warm-up Curve (20 iterations × 5 queries) ──");
    {
        let conn = db_connection_perf(&db_path);
        let warmup_queries = ["readme", "*.rs", "src/main", "package.json", "test*"];

        for i in 1..=20 {
            let start = Instant::now();
            let mut total_count = 0;
            for query in &warmup_queries {
                let r = run_search(&conn, query, DEFAULT_LIMIT);
                total_count += r.count;
            }
            let total_ms = start.elapsed().as_secs_f64() * 1000.0;
            let per_query = total_ms / warmup_queries.len() as f64;
            let bar_len = (per_query * 2.0).min(60.0) as usize;
            let bar: String = "█".repeat(bar_len);
            println!(
                "    Iter {i:>2}: {total_ms:>7.2}ms total ({per_query:>6.2}ms/q, {total_count:>4} results) {bar}"
            );
        }
    }
}

/// Scenario C: Search during active indexing.
/// Measures: search latency degradation under concurrent write load.
#[test]
fn scenario_c_search_during_indexing() {
    let home_dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string()));
    let bench_dir = PathBuf::from(BENCH_DB_DIR);
    let _ = fs::remove_dir_all(&bench_dir);
    fs::create_dir_all(&bench_dir).expect("create bench dir");
    let db_path = bench_dir.join("ux_bench.db");
    init_db(&db_path);

    let sep = "=".repeat(100);
    println!("\n{sep}");
    println!("  SCENARIO C: Search During Active Indexing");
    println!("{sep}");

    let scanned = Arc::new(AtomicU64::new(0));
    let indexed = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let dp = db_path.clone();
    let hd = home_dir.clone();
    let sc = scanned.clone();
    let ix = indexed.clone();
    let dn = done.clone();

    let index_thread =
        std::thread::spawn(move || run_full_index_with_progress(&dp, &hd, &sc, &ix, &dn));

    let concurrent_queries = [
        "readme",
        "*.rs",
        "package.json",
        "src/main",
        "a",
        "*.png",
        "test*",
        "Documents/",
        ".gitignore",
        "config*",
    ];

    let mut iteration = 0u32;
    loop {
        if done.load(Ordering::Acquire) {
            break;
        }

        // Wait for some entries to be indexed before starting
        if indexed.load(Ordering::Relaxed) < 1000 {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }

        iteration += 1;
        let sc_val = scanned.load(Ordering::Relaxed);
        let ix_val = indexed.load(Ordering::Relaxed);

        let conn = db_connection_perf(&db_path);
        let db_count = entry_count(&conn);

        println!(
            "\n  ── Iteration {iteration} (scanned: {sc_val}, indexed: {ix_val}, DB: {db_count}) ──"
        );

        let mut iter_total_ms = 0.0;
        for query in &concurrent_queries {
            let start = Instant::now();
            let result = run_search(&conn, query, DEFAULT_LIMIT);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            iter_total_ms += ms;
            let status = if ms > SLO_WARN_MS {
                "SLOW"
            } else if ms > SLO_OK_MS {
                "WARN"
            } else {
                " OK "
            };
            println!(
                "    [{status}] {:<30} cnt={:<4} {:.2}ms",
                format!("{:?}", query),
                result.count,
                ms,
            );
        }
        println!(
            "    Total: {iter_total_ms:.1}ms ({:.1}ms/query)",
            iter_total_ms / concurrent_queries.len() as f64
        );

        std::thread::sleep(Duration::from_secs(5));
    }

    let (total_scanned, total_indexed, elapsed, _) = index_thread.join().unwrap();
    println!(
        "\n  Indexing complete: scanned={total_scanned}, indexed={total_indexed}, time={:.1}s",
        elapsed.as_secs_f64()
    );

    // Final benchmark after indexing
    let conn = db_connection_perf(&db_path);
    run_test_cases(&conn, "SCENARIO C: Post-indexing benchmark");

    let _ = fs::remove_dir_all(&bench_dir);
}
