use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use walkdir::WalkDir;

fn prod_db_path() -> String {
    if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        format!("{home}/Library/Application Support/com.everything.app/index.db")
    } else {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| "C:\\Users\\USER\\AppData\\Roaming".to_string());
        format!("{appdata}/com.everything.app/index.db")
    }
}

fn bench_db_dir() -> String {
    if cfg!(target_os = "macos") {
        "/tmp/everything_bench".to_string()
    } else {
        let tmp = std::env::temp_dir();
        tmp.join("everything_bench").to_string_lossy().to_string()
    }
}
const DEFAULT_LIMIT: u32 = 300;
const SHORT_QUERY_LIMIT: u32 = 100;
const RUNS_PER_QUERY: usize = 3;
const BATCH_SIZE: usize = 10_000;
const DB_VERSION: i32 = 4;

// ── DB helpers ──

fn db_connection(db_path: &Path) -> Connection {
    let conn = Connection::open(db_path).expect("Failed to open DB");
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA busy_timeout=5000;",
    )
    .expect("Failed to set pragmas");
    conn
}

fn db_connection_perf(db_path: &Path) -> Connection {
    let conn = db_connection(db_path);
    conn.execute_batch(
        "PRAGMA cache_size=-65536;
         PRAGMA mmap_size=268435456;",
    )
    .expect("Failed to set perf pragmas");
    conn
}

fn init_db(db_path: &Path) {
    let conn = db_connection(db_path);
    let current_version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap_or(0);
    if current_version != DB_VERSION {
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS entries_ai;
             DROP TRIGGER IF EXISTS entries_ad;
             DROP TRIGGER IF EXISTS entries_au;
             DROP TABLE IF EXISTS entries_fts;
             DROP TABLE IF EXISTS entries;",
        )
        .unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {};", DB_VERSION))
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
         CREATE INDEX IF NOT EXISTS idx_entries_mtime ON entries(mtime);
         CREATE INDEX IF NOT EXISTS idx_entries_name_nocase ON entries(name COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_entries_ext ON entries(ext);
         CREATE INDEX IF NOT EXISTS idx_entries_ext_name ON entries(ext, name COLLATE NOCASE);
         CREATE INDEX IF NOT EXISTS idx_entries_run_id ON entries(run_id);
         CREATE TABLE IF NOT EXISTS meta (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )
    .unwrap();
}

fn entry_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
        .unwrap_or(0)
}

// ── Query parsing (duplicated from query.rs for standalone test) ──

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

#[derive(Debug)]
#[allow(dead_code)]
enum SearchMode {
    Empty,
    NameSearch {
        name_like: String,
    },
    GlobName {
        name_like: String,
    },
    ExtSearch {
        ext: String,
    },
    PathSearch {
        path_like: String,
        name_like: String,
    },
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

fn mode_label(query: &str) -> &'static str {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return "empty";
    }
    if trimmed.contains('/') {
        return "path";
    }
    if let Some(ext_part) = trimmed.strip_prefix("*.") {
        if !ext_part.is_empty() && !ext_part.contains('/') && !has_glob_chars(ext_part) {
            return "ext";
        }
    }
    if has_glob_chars(trimmed) {
        return "glob";
    }
    "name"
}

// ── Search execution ──

struct SearchResult {
    count: usize,
    first_names: Vec<String>,
}

fn run_search(conn: &Connection, query: &str, limit: u32) -> SearchResult {
    let effective_limit = if query.trim().chars().count() <= 1 {
        limit.min(SHORT_QUERY_LIMIT)
    } else {
        limit
    };
    let order_by = sort_clause("name", "asc", "e.");
    let bare_order = sort_clause("name", "asc", "");
    let mode = parse_query(query);
    let mut names: Vec<String> = Vec::new();

    match mode {
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

            // Phase 1a: exact match (fast index lookup)
            {
                let exact_sql = format!(
                    "SELECT name FROM entries
                     WHERE name COLLATE NOCASE = ?1
                     ORDER BY {bare_order} LIMIT ?2"
                );
                let mut stmt = conn.prepare_cached(&exact_sql).unwrap();
                let rows = stmt
                    .query_map(params![exact_query, effective_limit], |row| {
                        row.get::<_, String>(0)
                    })
                    .unwrap();
                for r in rows.flatten() {
                    names.push(r);
                }
            }

            // Phase 1b: prefix match excluding exact (uses index range scan)
            if (names.len() as u32) < effective_limit {
                let remaining = effective_limit - names.len() as u32;
                let prefix_sql = format!(
                    "SELECT name FROM entries INDEXED BY idx_entries_name_nocase
                     WHERE name LIKE ?1 ESCAPE '\\'
                       AND name COLLATE NOCASE != ?2
                     ORDER BY {bare_order} LIMIT ?3"
                );
                let mut stmt = conn.prepare_cached(&prefix_sql).unwrap();
                let rows = stmt
                    .query_map(params![prefix_like, exact_query, remaining], |row| {
                        row.get::<_, String>(0)
                    })
                    .unwrap();
                for r in rows.flatten() {
                    names.push(r);
                }
            }

            // Phase 2: contains-match with tight time budget (single pass)
            if names.is_empty() {
                let phase2_start = Instant::now();
                conn.progress_handler(
                    2_000,
                    Some(move || phase2_start.elapsed().as_millis() > 5),
                );

                {
                    let phase2_sql = format!(
                        "SELECT name FROM entries
                         WHERE name LIKE ?1 ESCAPE '\\'
                           AND name COLLATE NOCASE != ?2
                           AND name NOT LIKE ?3 ESCAPE '\\'
                         ORDER BY {bare_order} LIMIT ?4"
                    );
                    if let Ok(mut stmt2) = conn.prepare(&phase2_sql) {
                        if let Ok(rows2) = stmt2.query_map(
                            params![name_like, exact_query, prefix_like, effective_limit],
                            |row| row.get::<_, String>(0),
                        ) {
                            for r in rows2 {
                                match r {
                                    Ok(name) => names.push(name),
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
                .query_map(params![ext, effective_limit], |row| row.get::<_, String>(0))
                .unwrap();
            for r in rows.flatten() {
                names.push(r);
            }
        }
        SearchMode::PathSearch {
            path_like: _,
            name_like,
        } => {
            // Try to resolve dir_hint to absolute path for fast range scan
            let trimmed = query.trim();
            let last_slash = trimmed.rfind('/').unwrap_or(0);
            let dir_hint = trimmed[..last_slash].trim();
            let home_dir = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| if cfg!(windows) { "C:\\".to_string() } else { "/".to_string() });
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
                let sep = std::path::MAIN_SEPARATOR;
                let dir_exact = abs_dir.to_string_lossy().to_string();
                let dir_prefix = format!("{dir_exact}{sep}");
                let dir_prefix_end = format!("{dir_exact}\x7F");
                let sql = format!(
                    "SELECT e.name FROM entries e \
                     WHERE (e.dir = ?1 OR (e.dir >= ?2 AND e.dir < ?3)) \
                       AND e.name LIKE ?4 ESCAPE '\\' \
                     ORDER BY {order_by} LIMIT ?5"
                );
                let mut stmt = conn.prepare_cached(&sql).unwrap();
                let rows = stmt
                    .query_map(
                        params![
                            dir_exact,
                            dir_prefix,
                            dir_prefix_end,
                            name_like,
                            effective_limit
                        ],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap();
                for r in rows.flatten() {
                    names.push(r);
                }
            } else {
                // Use dir LIKE conditions instead of path LIKE (narrower column scan)
                let sep = std::path::MAIN_SEPARATOR;
                let native_hint = dir_hint.replace('/', &sep.to_string());
                let escaped_sep = escape_like(&sep.to_string());
                let dir_suffix = escape_like(&native_hint);
                let dir_like_exact = format!("%{escaped_sep}{dir_suffix}");
                let dir_like_sub = format!("%{escaped_sep}{dir_suffix}{escaped_sep}%");
                let ext_shortcut = extract_ext_from_like(&name_like);

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
                        let path_start = Instant::now();
                        conn.progress_handler(
                            2_000,
                            Some(move || path_start.elapsed().as_millis() > 5),
                        );

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
    SearchResult { count, first_names }
}

// ── Indexing (simplified from main.rs) ──

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
    let name = path.file_name().map(|v| v.to_string_lossy().to_string())?;
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
                row.path, row.name, row.dir, row.is_dir, row.ext, row.mtime, row.size, now, run_id,
            ]);
        }
    }
    tx.commit().unwrap();
}

/// Run a full $HOME index, reporting progress via atomics.
/// Returns (total_scanned, total_indexed, elapsed).
fn run_full_index(
    db_path: &Path,
    home_dir: &Path,
    scanned_counter: &AtomicU64,
    indexed_counter: &AtomicU64,
    done_flag: &AtomicBool,
) -> (u64, u64, Duration) {
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
    }

    if !batch.is_empty() {
        upsert_batch(&mut conn, &batch, run_id);
    }

    scanned_counter.store(scanned, Ordering::Relaxed);
    indexed_counter.store(indexed, Ordering::Relaxed);

    // Cleanup stale entries
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
    (scanned, indexed, start.elapsed())
}

// ── Benchmark runner ──

struct BenchResult {
    query: String,
    mode: String,
    count: usize,
    first_names: Vec<String>,
    min_ms: f64,
    avg_ms: f64,
    max_ms: f64,
}

fn bench_query(conn: &Connection, query: &str) -> BenchResult {
    let mut timings = Vec::with_capacity(RUNS_PER_QUERY);
    let mut last_result = SearchResult {
        count: 0,
        first_names: Vec::new(),
    };

    for _ in 0..RUNS_PER_QUERY {
        let start = Instant::now();
        last_result = run_search(conn, query, DEFAULT_LIMIT);
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let min_ms = timings.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_ms = timings.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let avg_ms = timings.iter().sum::<f64>() / timings.len() as f64;

    BenchResult {
        query: query.to_string(),
        mode: mode_label(query).to_string(),
        count: last_result.count,
        first_names: last_result.first_names,
        min_ms,
        avg_ms,
        max_ms,
    }
}

fn all_test_queries() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        (
            "Simple Name",
            vec![
                "readme",
                "package.json",
                "Cargo.toml",
                ".gitignore",
                "main.rs",
                "index.html",
                "tsconfig",
                "Makefile",
            ],
        ),
        ("Single Char", vec!["a", "m", "z", "1"]),
        (
            "Extension",
            vec![
                "*.rs", "*.md", "*.json", "*.png", "*.swift", "*.ts", "*.py", "*.txt", "*.PDF",
            ],
        ),
        (
            "Glob",
            vec!["test*", "spec?.md", "README*", "*.t?t", "index*", "config*"],
        ),
        (
            "Path",
            vec![
                "src/main",
                "desktop/*.png",
                "Documents/",
                "src/*.rs",
                "everything/",
            ],
        ),
        ("Korean", vec!["문서", "다운로드", "사진"]),
        ("Empty", vec![""]),
        (
            "Long/NoMatch",
            vec![
                "some very long search query that probably matches nothing at all xyz",
                "zzzzzzzzzzz_no_match_expected",
            ],
        ),
    ]
}

fn run_all_benchmarks(conn: &Connection, phase_label: &str) -> Vec<(String, Vec<BenchResult>)> {
    let sep = "=".repeat(80);
    println!("\n{sep}");
    println!("  {phase_label}");
    println!("{sep}");

    let count = entry_count(conn);
    println!("  DB entries: {count}");

    // Warmup: run each query once to warm OS page cache
    let queries = all_test_queries();
    for (_, query_list) in &queries {
        for query in query_list {
            let _ = run_search(conn, query, DEFAULT_LIMIT);
        }
    }
    println!("  (page cache warmed)\n");

    let mut all_results: Vec<(String, Vec<BenchResult>)> = Vec::new();
    let mut slow_count = 0u32;

    for (category, query_list) in &queries {
        println!("  --- {category} ---");
        let mut cat_results = Vec::new();

        for query in query_list {
            let r = bench_query(conn, query);
            let status = if r.avg_ms > 50.0 {
                slow_count += 1;
                "SLOW"
            } else if r.avg_ms > 30.0 {
                "WARN"
            } else {
                " OK "
            };

            println!(
                "    [{status}] {:<50} mode={:<5} cnt={:<4} min={:>8.2}ms avg={:>8.2}ms max={:>8.2}ms",
                format!("{:?}", r.query), r.mode, r.count, r.min_ms, r.avg_ms, r.max_ms,
            );
            if !r.first_names.is_empty() {
                let preview: Vec<&str> = r.first_names.iter().take(3).map(|s| s.as_str()).collect();
                println!("           top: {:?}", preview);
            }
            cat_results.push(r);
        }
        println!();
        all_results.push((category.to_string(), cat_results));
    }

    // stats
    let all_avg: Vec<f64> = all_results
        .iter()
        .flat_map(|(_, v)| v.iter().map(|r| r.avg_ms))
        .collect();
    let total = all_avg.len();
    let overall_avg = all_avg.iter().sum::<f64>() / total as f64;
    let overall_max = all_avg.iter().cloned().fold(0.0_f64, f64::max);
    let mut sorted = all_avg.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[total / 2];
    let p95 = sorted[(total as f64 * 0.95) as usize % total];
    let p99 = sorted[(total as f64 * 0.99) as usize % total];

    println!("  Queries: {total}  |  Slow(>50ms): {slow_count}");
    println!(
        "  Avg: {overall_avg:.2}ms  |  P50: {p50:.2}ms  |  P95: {p95:.2}ms  |  P99: {p99:.2}ms  |  Max: {overall_max:.2}ms"
    );
    println!(
        "  SLO p95<30ms: {}",
        if p95 < 30.0 { "PASS" } else { "FAIL" }
    );
    println!();

    all_results
}

// ── Tests ──

/// Phase 1: Benchmark searches against the existing production DB.
/// This simulates the "app restart" scenario where index is already built.
#[test]
fn phase1_warm_search_on_existing_db() {
    let prod_path = prod_db_path();
    let db_path = PathBuf::from(&prod_path);
    if !db_path.exists() {
        println!("SKIP: Production DB not found at {prod_path}");
        return;
    }

    let conn = db_connection_perf(&db_path);
    let db_size = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let count = entry_count(&conn);

    println!("\n  Production DB: {prod_path}");
    println!(
        "  DB size: {:.1} MB  |  Entries: {count}",
        db_size as f64 / 1_048_576.0
    );

    run_all_benchmarks(
        &conn,
        "PHASE 1: Warm Search (existing DB, simulates restart)",
    );
}

/// Phase 2: Fresh index + concurrent search.
/// Creates a separate bench DB, indexes $HOME, runs searches during indexing.
#[test]
fn phase2_index_and_concurrent_search() {
    let home_dir = PathBuf::from(
        std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| if cfg!(windows) { "C:\\".to_string() } else { "/".to_string() }),
    );
    let bench_dir = PathBuf::from(bench_db_dir());
    let _ = fs::remove_dir_all(&bench_dir);
    fs::create_dir_all(&bench_dir).expect("Failed to create bench dir");
    let db_path = bench_dir.join("bench_index.db");

    init_db(&db_path);
    println!("\n  Bench DB: {}", db_path.display());

    let scanned = Arc::new(AtomicU64::new(0));
    let indexed = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));

    // Start indexing in background thread
    let db_path_clone = db_path.clone();
    let home_clone = home_dir.clone();
    let sc = scanned.clone();
    let ix = indexed.clone();
    let dn = done.clone();

    let index_thread =
        std::thread::spawn(move || run_full_index(&db_path_clone, &home_clone, &sc, &ix, &dn));

    // Run searches concurrently while indexing is in progress
    let sep = "=".repeat(80);
    println!("\n{sep}");
    println!("  PHASE 2: Search DURING Indexing");
    println!("{sep}");

    let sample_queries = vec![
        "readme",
        "*.rs",
        "package.json",
        "src/main",
        "a",
        "*.png",
        "test*",
        "Documents/",
        "main.rs",
        "config*",
    ];

    let mut iteration = 0u32;
    loop {
        if done.load(Ordering::Acquire) {
            break;
        }

        iteration += 1;
        let sc_val = scanned.load(Ordering::Relaxed);
        let ix_val = indexed.load(Ordering::Relaxed);

        // Open a fresh read connection each round
        let conn = db_connection_perf(&db_path);
        let count = entry_count(&conn);

        println!(
            "\n  --- Iteration {iteration} (scanned: {sc_val}, indexed: {ix_val}, DB entries: {count}) ---"
        );

        for query in &sample_queries {
            let start = Instant::now();
            let result = run_search(&conn, query, DEFAULT_LIMIT);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            let status = if ms > 50.0 {
                "SLOW"
            } else if ms > 30.0 {
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

        // Wait a bit before next iteration
        std::thread::sleep(Duration::from_secs(5));
    }

    // Wait for indexing to complete
    let (total_scanned, total_indexed, elapsed) = index_thread.join().unwrap();
    println!(
        "\n  Indexing complete: scanned={total_scanned}, indexed={total_indexed}, time={:.1}s",
        elapsed.as_secs_f64()
    );

    // Phase 2b: Search AFTER indexing completes
    let conn = db_connection_perf(&db_path);
    run_all_benchmarks(&conn, "PHASE 2b: Search AFTER Fresh Index (bench DB)");

    // Cleanup
    let _ = fs::remove_dir_all(&bench_dir);
}

/// Phase 3: Simulate "restart" by closing and reopening the production DB.
/// Measures cold-start overhead (no mmap warm cache).
#[test]
fn phase3_restart_simulation() {
    let prod_path = prod_db_path();
    let db_path = PathBuf::from(&prod_path);
    if !db_path.exists() {
        println!("SKIP: Production DB not found at {prod_path}");
        return;
    }

    // Drop OS page cache influence by opening with fresh connection
    // (can't truly flush OS cache without sudo, but fresh Connection helps)
    let conn = db_connection(&db_path); // no perf pragmas initially
    let count = entry_count(&conn);

    let sep = "=".repeat(80);
    println!("\n{sep}");
    println!("  PHASE 3: Restart Simulation (cold DB open, no mmap)");
    println!("{sep}");
    println!("  DB entries: {count}");
    println!();

    // First search without mmap/large cache (simulates cold start)
    println!("  --- Cold start (default pragmas) ---");
    let sample_queries = vec![
        "readme",
        "*.rs",
        "package.json",
        "src/main",
        "a",
        "test*",
        "Documents/",
        "main.rs",
    ];
    for query in &sample_queries {
        let start = Instant::now();
        let result = run_search(&conn, query, DEFAULT_LIMIT);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let status = if ms > 50.0 {
            "SLOW"
        } else if ms > 30.0 {
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
    println!();

    // Now enable perf pragmas (simulates app setting up after DB open)
    conn.execute_batch(
        "PRAGMA cache_size=-65536;
         PRAGMA mmap_size=268435456;",
    )
    .unwrap();

    println!("  --- After perf pragmas (mmap + large cache) ---");
    for query in &sample_queries {
        let start = Instant::now();
        let result = run_search(&conn, query, DEFAULT_LIMIT);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        let status = if ms > 50.0 {
            "SLOW"
        } else if ms > 30.0 {
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
    println!();

    // 10 rapid iterations to measure warm-up curve
    println!("  --- 10-iteration warm-up curve ---");
    for i in 1..=10 {
        let start = Instant::now();
        let _ = run_search(&conn, "readme", DEFAULT_LIMIT);
        let _ = run_search(&conn, "*.rs", DEFAULT_LIMIT);
        let _ = run_search(&conn, "src/main", DEFAULT_LIMIT);
        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "    Iteration {i:>2}: 3 queries in {total_ms:.2}ms ({:.2}ms/query)",
            total_ms / 3.0
        );
    }
}

// ── In-memory search types & functions ──

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct MemEntry {
    path: String,
    name: String,
    dir: String,
    is_dir: bool,
    ext: Option<String>,
    size: Option<i64>,
    mtime: Option<i64>,
}

fn load_entries_from_db(conn: &Connection) -> Vec<MemEntry> {
    let mut stmt = conn
        .prepare("SELECT path, name, dir, is_dir, ext, size, mtime FROM entries")
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok(MemEntry {
                path: row.get(0)?,
                name: row.get(1)?,
                dir: row.get(2)?,
                is_dir: row.get::<_, i64>(3)? != 0,
                ext: row.get(4)?,
                size: row.get(5)?,
                mtime: row.get(6)?,
            })
        })
        .unwrap();
    rows.flatten().collect()
}

// LIKE pattern matcher (mirrors mem_search.rs logic)
enum LikeSeg {
    Literal(String),
    Single,
    Any,
}

struct LikePat {
    segs: Vec<LikeSeg>,
}

impl LikePat {
    fn new(pattern: &str) -> Self {
        let mut segs = Vec::new();
        let mut chars = pattern.chars().peekable();
        let mut lit = String::new();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if let Some(esc) = chars.next() {
                        lit.push(esc);
                    }
                }
                '%' => {
                    if !lit.is_empty() {
                        segs.push(LikeSeg::Literal(std::mem::take(&mut lit).to_lowercase()));
                    }
                    while chars.peek() == Some(&'%') {
                        chars.next();
                    }
                    segs.push(LikeSeg::Any);
                }
                '_' => {
                    if !lit.is_empty() {
                        segs.push(LikeSeg::Literal(std::mem::take(&mut lit).to_lowercase()));
                    }
                    segs.push(LikeSeg::Single);
                }
                _ => lit.push(ch),
            }
        }
        if !lit.is_empty() {
            segs.push(LikeSeg::Literal(lit.to_lowercase()));
        }
        LikePat { segs }
    }

    fn matches_lowered(&self, value: &str) -> bool {
        Self::do_match(&self.segs, value, 0, 0)
    }

    #[allow(dead_code)]
    fn matches(&self, value: &str) -> bool {
        Self::do_match(&self.segs, &value.to_lowercase(), 0, 0)
    }

    fn literal_prefix(&self) -> Option<String> {
        match self.segs.first() {
            Some(LikeSeg::Literal(lit)) => Some(lit.clone()),
            _ => None,
        }
    }

    fn do_match(segs: &[LikeSeg], val: &str, si: usize, vp: usize) -> bool {
        if si >= segs.len() {
            return vp >= val.len();
        }
        let rem = &val[vp..];
        match &segs[si] {
            LikeSeg::Literal(lit) => {
                if !rem.starts_with(lit.as_str()) { return false; }
                Self::do_match(segs, val, si + 1, vp + lit.len())
            }
            LikeSeg::Single => {
                if rem.is_empty() { return false; }
                let clen = rem.chars().next().unwrap().len_utf8();
                Self::do_match(segs, val, si + 1, vp + clen)
            }
            LikeSeg::Any => {
                let ns = si + 1;
                if ns >= segs.len() { return true; }
                let mut pos = vp;
                if Self::do_match(segs, val, ns, pos) { return true; }
                for ch in rem.chars() {
                    pos += ch.len_utf8();
                    if Self::do_match(segs, val, ns, pos) { return true; }
                }
                false
            }
        }
    }
}

struct MemSearchResult {
    count: usize,
    first_names: Vec<String>,
}

/// Indexed in-memory search structure (mirrors mem_search::MemIndex)
struct MemSearchIndex {
    entries: Vec<MemEntry>,
    names_lower: Vec<String>,
    sorted_names: Vec<(String, u32)>,
    ext_map: HashMap<String, Vec<u32>>,
    dir_map: HashMap<String, Vec<u32>>,
}

impl MemSearchIndex {
    fn build(entries: Vec<MemEntry>) -> Self {
        let n = entries.len();
        let mut names_lower = Vec::with_capacity(n);
        let mut sorted_names = Vec::with_capacity(n);
        let mut ext_map: HashMap<String, Vec<u32>> = HashMap::new();
        let mut dir_map: HashMap<String, Vec<u32>> = HashMap::new();

        for (i, e) in entries.iter().enumerate() {
            let idx = i as u32;
            let nl = e.name.to_lowercase();
            sorted_names.push((nl.clone(), idx));
            names_lower.push(nl);
            if let Some(ref ext) = e.ext {
                ext_map.entry(ext.clone()).or_default().push(idx);
            }
            let dl = e.dir.to_lowercase();
            dir_map.entry(dl).or_default().push(idx);
        }
        sorted_names.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        // Pre-sort ext_map values by name_lower for direct pagination
        for idxs in ext_map.values_mut() {
            idxs.sort_unstable_by(|&a, &b| names_lower[a as usize].cmp(&names_lower[b as usize]));
        }

        MemSearchIndex { entries, names_lower, sorted_names, ext_map, dir_map }
    }
}

fn increment_str(s: &str) -> Option<String> {
    let mut chars: Vec<char> = s.chars().collect();
    for i in (0..chars.len()).rev() {
        if let Some(nc) = char::from_u32(chars[i] as u32 + 1) {
            chars[i] = nc;
            return Some(chars[..=i].iter().collect());
        }
    }
    None
}

const SCAN_BUDGET_MS: u128 = 30;

fn mem_search(idx: &MemSearchIndex, query: &str, limit: u32) -> MemSearchResult {
    let effective_limit = if query.trim().chars().count() <= 1 {
        limit.min(SHORT_QUERY_LIMIT)
    } else {
        limit
    };
    let mode = parse_query(query);

    // Empty mode: use sorted_names directly (no filter/sort needed)
    if matches!(mode, SearchMode::Empty) {
        let end = effective_limit as usize;
        let page: Vec<String> = idx.sorted_names.iter()
            .take(end)
            .map(|(_, i)| idx.entries[*i as usize].name.clone())
            .collect();
        let count = page.len();
        let first_names: Vec<String> = page.into_iter().take(5).collect();
        return MemSearchResult { count, first_names };
    }

    // ExtSearch: pre-sorted by name — paginate directly
    if let SearchMode::ExtSearch { ext } = &mode {
        let ext_lower = ext.to_lowercase();
        let names = match idx.ext_map.get(&ext_lower) {
            Some(idxs) => {
                let end = (effective_limit as usize).min(idxs.len());
                idxs[..end].iter().map(|&i| idx.entries[i as usize].name.clone()).collect()
            }
            None => Vec::new(),
        };
        let count = names.len();
        let first_names: Vec<String> = names.into_iter().take(5).collect();
        return MemSearchResult { count, first_names };
    }

    let mut indices: Vec<u32> = match &mode {
        SearchMode::Empty | SearchMode::ExtSearch { .. } => unreachable!(),
        SearchMode::NameSearch { .. } => {
            let q_lower = query.trim().to_lowercase();
            let cap = effective_limit as usize;
            // Phase 1: exact
            let lo = idx.sorted_names.partition_point(|(n, _)| n.as_str() < q_lower.as_str());
            let mut exact: Vec<u32> = Vec::new();
            let mut i = lo;
            while i < idx.sorted_names.len() && idx.sorted_names[i].0 == q_lower {
                exact.push(idx.sorted_names[i].1);
                i += 1;
            }
            if exact.len() >= cap { exact.truncate(cap); exact }
            else {
                // Phase 2: prefix
                let end_str = increment_str(&q_lower);
                let hi = match &end_str {
                    Some(es) => idx.sorted_names.partition_point(|(n, _)| n.as_str() < es.as_str()),
                    None => idx.sorted_names.len(),
                };
                let mut prefix: Vec<u32> = Vec::new();
                for j in i..hi {
                    prefix.push(idx.sorted_names[j].1);
                    if exact.len() + prefix.len() >= cap { break; }
                }
                if exact.len() + prefix.len() >= cap {
                    exact.extend(prefix);
                    exact.truncate(cap);
                    exact
                } else {
                    // Phase 3: contains (with time budget)
                    let remaining = cap - exact.len() - prefix.len();
                    let ep_set: std::collections::HashSet<u32> = exact.iter().chain(prefix.iter()).copied().collect();
                    let scan_start = Instant::now();
                    let mut contains: Vec<u32> = Vec::new();
                    for (j, nl) in idx.names_lower.iter().enumerate() {
                        let j = j as u32;
                        if ep_set.contains(&j) { continue; }
                        if nl.contains(&q_lower) {
                            contains.push(j);
                            if contains.len() >= remaining { break; }
                        }
                        if j & 0x3FFF == 0 && scan_start.elapsed().as_millis() > SCAN_BUDGET_MS { break; }
                    }
                    exact.extend(prefix);
                    exact.extend(contains);
                    exact
                }
            }
        }
        SearchMode::GlobName { name_like } => {
            let pat = LikePat::new(name_like);
            // Optimization: if pattern starts with literal prefix, use binary search
            if let Some(prefix) = pat.literal_prefix() {
                if !prefix.is_empty() {
                    let lo = idx.sorted_names.partition_point(|(n, _)| n.as_str() < prefix.as_str());
                    let prefix_end = increment_str(&prefix);
                    let hi = match &prefix_end {
                        Some(es) => idx.sorted_names.partition_point(|(n, _)| n.as_str() < es.as_str()),
                        None => idx.sorted_names.len(),
                    };
                    let mut results: Vec<u32> = Vec::new();
                    for j in lo..hi {
                        let (ref nl, i) = idx.sorted_names[j];
                        if pat.matches_lowered(nl) {
                            results.push(i);
                            if results.len() >= effective_limit as usize { break; }
                        }
                    }
                    let count = results.len();
                    let first_names: Vec<String> = results.iter().take(5)
                        .map(|&i| idx.entries[i as usize].name.clone()).collect();
                    return MemSearchResult { count, first_names };
                }
            }
            // Fallback: full scan with time budget
            let scan_start = Instant::now();
            let mut results: Vec<u32> = Vec::new();
            for (i, nl) in idx.names_lower.iter().enumerate() {
                if pat.matches_lowered(nl) {
                    results.push(i as u32);
                }
                if (i as u32) & 0x3FFF == 0 && i > 0 && scan_start.elapsed().as_millis() > SCAN_BUDGET_MS { break; }
            }
            results
        }
        SearchMode::PathSearch { name_like, .. } => {
            let trimmed = query.trim();
            let last_slash = trimmed.rfind('/').unwrap_or(0);
            let dir_hint = trimmed[..last_slash].trim();
            let sep = std::path::MAIN_SEPARATOR;
            let dir_hint_norm = dir_hint.replace('/', &sep.to_string()).to_lowercase();
            let dir_suffix = format!("{sep}{dir_hint_norm}").to_lowercase();
            let dir_infix = format!("{sep}{dir_hint_norm}{sep}").to_lowercase();
            let scan_start = Instant::now();
            let collect_cap = effective_limit as usize * 30;
            let mut result: Vec<u32> = Vec::new();
            for (dk, idxs) in &idx.dir_map {
                if dk.ends_with(&dir_suffix) || dk.contains(&dir_infix) {
                    result.extend_from_slice(idxs);
                    if result.len() >= collect_cap { break; }
                }
                if scan_start.elapsed().as_millis() > SCAN_BUDGET_MS { break; }
            }
            if name_like != "%" {
                let name_pat = LikePat::new(name_like);
                result.retain(|&i| name_pat.matches_lowered(&idx.names_lower[i as usize]));
            }
            result
        }
    };

    // Partial sort when result set is much larger than limit
    let lim = effective_limit as usize;
    if indices.len() > lim * 3 {
        let k = lim.min(indices.len());
        let cmp = |a: &u32, b: &u32| idx.names_lower[*a as usize].cmp(&idx.names_lower[*b as usize]);
        indices.select_nth_unstable_by(k - 1, cmp);
        indices.truncate(k);
        indices.sort_unstable_by(cmp);
    } else {
        indices.sort_unstable_by(|&a, &b| idx.names_lower[a as usize].cmp(&idx.names_lower[b as usize]));
        indices.truncate(lim);
    }

    let count = indices.len();
    let first_names: Vec<String> = indices.iter().take(5).map(|&i| idx.entries[i as usize].name.clone()).collect();
    MemSearchResult { count, first_names }
}

fn bench_mem_query(idx: &MemSearchIndex, query: &str) -> BenchResult {
    let mut timings = Vec::with_capacity(RUNS_PER_QUERY);
    let mut last = MemSearchResult {
        count: 0,
        first_names: Vec::new(),
    };

    for _ in 0..RUNS_PER_QUERY {
        let start = Instant::now();
        last = mem_search(idx, query, DEFAULT_LIMIT);
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let min_ms = timings.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_ms = timings.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let avg_ms = timings.iter().sum::<f64>() / timings.len() as f64;

    BenchResult {
        query: query.to_string(),
        mode: mode_label(query).to_string(),
        count: last.count,
        first_names: last.first_names,
        min_ms,
        avg_ms,
        max_ms,
    }
}

/// Phase 4: In-memory search benchmark.
/// Loads all entries from prod DB into Vec, benchmarks search without DB.
#[test]
fn phase4_mem_search_benchmark() {
    let prod_path = prod_db_path();
    let db_path = PathBuf::from(&prod_path);
    if !db_path.exists() {
        println!("SKIP: Production DB not found at {prod_path}");
        return;
    }

    let conn = db_connection_perf(&db_path);
    let count = entry_count(&conn);
    let db_size = fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    println!("\n  Loading entries from prod DB into memory...");
    let load_start = Instant::now();
    let entries = load_entries_from_db(&conn);
    let load_ms = load_start.elapsed().as_millis();
    println!(
        "  Loaded {} entries in {load_ms}ms (DB: {:.1}MB, {count} rows)",
        entries.len(),
        db_size as f64 / 1_048_576.0,
    );
    drop(conn);

    println!("  Building indexed search structure...");
    let build_start = Instant::now();
    let idx = MemSearchIndex::build(entries);
    let build_ms = build_start.elapsed().as_millis();
    println!("  Index built in {build_ms}ms");

    let sep = "=".repeat(80);
    println!("\n{sep}");
    println!("  PHASE 4: In-Memory Search Benchmark ({} entries)", idx.entries.len());
    println!("{sep}");

    // Warmup
    let queries = all_test_queries();
    for (_, query_list) in &queries {
        for query in query_list {
            let _ = mem_search(&idx, query, DEFAULT_LIMIT);
        }
    }
    println!("  (warmup done)\n");

    let mut all_results: Vec<(String, Vec<BenchResult>)> = Vec::new();
    let mut slow_count = 0u32;

    for (category, query_list) in &queries {
        println!("  --- {category} ---");
        let mut cat_results = Vec::new();

        for query in query_list {
            let r = bench_mem_query(&idx, query);
            let status = if r.avg_ms > 50.0 {
                slow_count += 1;
                "SLOW"
            } else if r.avg_ms > 30.0 {
                "WARN"
            } else {
                " OK "
            };

            println!(
                "    [{status}] {:<50} mode={:<5} cnt={:<4} min={:>8.2}ms avg={:>8.2}ms max={:>8.2}ms",
                format!("{:?}", r.query), r.mode, r.count, r.min_ms, r.avg_ms, r.max_ms,
            );
            if !r.first_names.is_empty() {
                let preview: Vec<&str> = r.first_names.iter().take(3).map(|s| s.as_str()).collect();
                println!("           top: {:?}", preview);
            }
            cat_results.push(r);
        }
        println!();
        all_results.push((category.to_string(), cat_results));
    }

    // Stats
    let all_avg: Vec<f64> = all_results
        .iter()
        .flat_map(|(_, v)| v.iter().map(|r| r.avg_ms))
        .collect();
    let total = all_avg.len();
    let overall_avg = all_avg.iter().sum::<f64>() / total as f64;
    let overall_max = all_avg.iter().cloned().fold(0.0_f64, f64::max);
    let mut sorted = all_avg.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = sorted[total / 2];
    let p95 = sorted[(total as f64 * 0.95) as usize % total];
    let p99 = sorted[(total as f64 * 0.99) as usize % total];

    println!("  Queries: {total}  |  Slow(>50ms): {slow_count}");
    println!(
        "  Avg: {overall_avg:.2}ms  |  P50: {p50:.2}ms  |  P95: {p95:.2}ms  |  P99: {p99:.2}ms  |  Max: {overall_max:.2}ms"
    );
    println!(
        "  SLO p95<30ms: {}  (target for mem_search)",
        if p95 < 30.0 { "PASS" } else { "FAIL" }
    );
    println!();
}
