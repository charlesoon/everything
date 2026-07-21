//! Streaming subtree rescan: reconcile a directory tree on disk with the DB
//! without materializing the whole subtree in memory.
//!
//! Used by the FSEvents MustScanSubDirs handler (kernel event-queue overflow
//! forces a rescan of a watched subtree — possibly all of `$HOME`) and by
//! directory rename. Peak memory is one upsert batch plus the compact diff
//! snapshot (~24 bytes per known row) instead of the whole subtree as
//! `IndexRow`s (~400 bytes per row), and unchanged rows are never rewritten,
//! so a large rescan no longer floods the WAL or fires FTS triggers per row.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use walkdir::WalkDir;

use crate::{
    delete_paths, index_row_from_walkdir_entry, should_skip_path, subtree_range_bounds,
    upsert_rows, AppResult, IgnorePattern, IndexRow, BATCH_SIZE,
};

/// The subtree membership predicate: the root row itself plus every row
/// strictly under it, via index-friendly range bounds (`subtree_range_bounds`).
const SUBTREE_WHERE: &str = "path = ?1 OR (path >= ?2 AND path < ?3)";

/// Sentinel for a NULL mtime/size; real values are never `i64::MIN`.
const NONE_SENTINEL: i64 = i64::MIN;

fn encode(value: Option<i64>) -> i64 {
    value.unwrap_or(NONE_SENTINEL)
}

/// Deterministic 64-bit path hash (`DefaultHasher` uses fixed SipHash keys).
fn path_hash(path: &str) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(path.as_bytes());
    hasher.finish()
}

/// Compact snapshot of the DB rows under a subtree: path hash → (mtime, size).
///
/// A 64-bit hash collision (odds ≈ n²/2⁶⁵ per subtree) can at worst leave one
/// vanished row undeleted or one modified row unwritten until the next rescan.
pub(crate) struct SubtreeDiff {
    existing: HashMap<u64, (i64, i64)>,
    /// Prefixes whose enumeration failed (permission/I-O errors): their rows
    /// are excluded from vanished-row deletion — absence from the walk is not
    /// evidence of deletion there.
    errored_prefixes: Vec<PathBuf>,
}

impl SubtreeDiff {
    pub(crate) fn empty() -> Self {
        Self {
            existing: HashMap::new(),
            errored_prefixes: Vec::new(),
        }
    }

    /// Snapshot `dir_prefix` itself plus every row under it.
    pub(crate) fn load(conn: &Connection, dir_prefix: &str) -> Self {
        let (lo, hi) = subtree_range_bounds(dir_prefix);
        let mut diff = Self::empty();
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT path, mtime, size FROM entries WHERE {SUBTREE_WHERE}"
        )) else {
            return diff;
        };
        let rows = stmt.query_map(params![dir_prefix, lo, hi], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        });
        if let Ok(rows) = rows {
            for (path, mtime, size) in rows.flatten() {
                diff.existing
                    .insert(path_hash(&path), (encode(mtime), encode(size)));
            }
        }
        diff
    }

    /// Drop `path` from the snapshot without comparing (e.g. a root row
    /// maintained elsewhere must not count as vanished).
    pub(crate) fn forget(&mut self, path: &str) {
        self.existing.remove(&path_hash(path));
    }

    /// True when `row` already exists with the same mtime + size. The row is
    /// removed from the snapshot either way, so after the walk the remainder
    /// is exactly the set of vanished rows.
    pub(crate) fn check_unchanged(&mut self, row: &IndexRow) -> bool {
        matches!(
            self.existing.remove(&path_hash(&row.path)),
            Some((mtime, size)) if mtime == encode(row.mtime) && size == encode(row.size)
        )
    }

    /// Exclude `prefix` (and everything under it) from vanished-row deletion:
    /// its enumeration failed, so its rows must be kept, not deleted.
    pub(crate) fn mark_errored(&mut self, prefix: &Path) {
        self.errored_prefixes.push(prefix.to_path_buf());
    }

    /// Reconcile a walk enumeration error against the vanished-row policy: a
    /// confirmed `NotFound` means the path truly vanished, so its rows are left
    /// in the diff for deletion; any other error (permission, I/O, or a non-io
    /// walker error) means enumeration failed and the subtree's rows must be
    /// preserved via `mark_errored`. `path` is the error's path already resolved
    /// against a fallback root; `io_kind` is the walker error's io-error kind
    /// (both `walkdir` and `jwalk` expose `io_error()`).
    pub(crate) fn observe_walk_error(&mut self, io_kind: Option<std::io::ErrorKind>, path: &Path) {
        if io_kind != Some(std::io::ErrorKind::NotFound) {
            self.mark_errored(path);
        }
    }

    /// Paths of rows that were in the snapshot but never seen on disk, minus
    /// anything under an errored prefix. The snapshot only keeps hashes, so
    /// path strings are re-read from the DB (an index range scan).
    pub(crate) fn leftover_paths(self, conn: &Connection, dir_prefix: &str) -> Vec<String> {
        if self.existing.is_empty() {
            return Vec::new();
        }
        let (lo, hi) = subtree_range_bounds(dir_prefix);
        let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT path FROM entries WHERE {SUBTREE_WHERE}"
        )) else {
            return Vec::new();
        };
        let rows = stmt.query_map(params![dir_prefix, lo, hi], |row| {
            row.get::<_, String>(0)
        });
        let mut vanished = Vec::new();
        if let Ok(rows) = rows {
            for path in rows.flatten() {
                if !self.existing.contains_key(&path_hash(&path)) {
                    continue;
                }
                if self
                    .errored_prefixes
                    .iter()
                    .any(|prefix| Path::new(&path).starts_with(prefix))
                {
                    continue;
                }
                vanished.push(path);
            }
        }
        vanished
    }
}

/// Walk `root` and reconcile the DB with the filesystem: upsert new/changed
/// rows in `BATCH_SIZE` batches, then delete rows whose files vanished.
/// Subtrees that failed to enumerate (permission errors) are excluded from
/// deletion so an unreadable directory doesn't wipe its rows from the index.
/// Returns (upserted, deleted).
pub(crate) fn rescan_subtree(
    conn: &mut Connection,
    root: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> AppResult<(usize, usize)> {
    let root_str = root.to_string_lossy().to_string();
    let mut diff = SubtreeDiff::load(conn, &root_str);

    let mut batch: Vec<IndexRow> = Vec::with_capacity(BATCH_SIZE);
    let mut upserted = 0usize;

    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_skip_path(entry.path(), ignored_roots, ignored_patterns));
    for result in walker {
        let entry = match result {
            Ok(entry) => entry,
            Err(err) => {
                // NotFound means the path vanished mid-walk — deletion is the
                // correct outcome. Anything else (permission, I/O) must keep
                // the existing rows for that subtree.
                diff.observe_walk_error(
                    err.io_error().map(|e| e.kind()),
                    err.path().unwrap_or(root),
                );
                continue;
            }
        };
        let Some(row) = index_row_from_walkdir_entry(&entry) else {
            // Metadata unreadable: keep any existing row rather than deleting it.
            diff.mark_errored(entry.path());
            continue;
        };
        if diff.check_unchanged(&row) {
            continue;
        }
        batch.push(row);
        if batch.len() >= BATCH_SIZE {
            upserted += upsert_rows(conn, &batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        upserted += upsert_rows(conn, &batch)?;
    }

    let mut deleted = 0usize;
    let vanished = diff.leftover_paths(conn, &root_str);
    for chunk in vanished.chunks(BATCH_SIZE) {
        deleted += delete_paths(conn, chunk)?;
    }
    Ok((upserted, deleted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE entries (
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
            );",
        )
        .expect("create entries");
        conn
    }

    fn temp_dir(case: &str) -> PathBuf {
        let dir = crate::temp_case_dir(&format!("rescan_{case}"));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn db_paths(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT path FROM entries ORDER BY path")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect()
    }

    #[test]
    fn rescan_inserts_updates_and_deletes() {
        let root = temp_dir("iud");
        fs::write(root.join("a.txt"), b"aaaa").unwrap();
        fs::write(root.join("b.txt"), b"bb").unwrap();
        fs::write(root.join("keep.txt"), b"k").unwrap();

        let mut conn = test_conn();
        let (upserted, deleted) = rescan_subtree(&mut conn, &root, &[], &[]).unwrap();
        assert_eq!(upserted, 4, "root dir + 3 files inserted");
        assert_eq!(deleted, 0);

        // Sentinel indexed_at so we can prove unchanged rows are not rewritten.
        conn.execute("UPDATE entries SET indexed_at = 111", [])
            .unwrap();

        fs::write(root.join("b.txt"), b"bbbbbbbb").unwrap(); // size change
        fs::remove_file(root.join("a.txt")).unwrap();
        fs::write(root.join("c.txt"), b"c").unwrap(); // new file

        let (upserted, deleted) = rescan_subtree(&mut conn, &root, &[], &[]).unwrap();
        // b (changed), c (new), and the root dir (mtime changed) are rewritten.
        assert!(upserted >= 2 && upserted <= 3, "upserted={upserted}");
        assert_eq!(deleted, 1, "a.txt removed");

        let paths = db_paths(&conn);
        let root_str = root.to_string_lossy();
        assert!(!paths.contains(&format!("{root_str}/a.txt")));
        assert!(paths.contains(&format!("{root_str}/b.txt")));
        assert!(paths.contains(&format!("{root_str}/c.txt")));

        let keep_indexed_at: i64 = conn
            .query_row(
                "SELECT indexed_at FROM entries WHERE path = ?1",
                params![format!("{root_str}/keep.txt")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(keep_indexed_at, 111, "unchanged row must not be rewritten");

        let b_size: i64 = conn
            .query_row(
                "SELECT size FROM entries WHERE path = ?1",
                params![format!("{root_str}/b.txt")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(b_size, 8);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rescan_of_vanished_root_deletes_all_rows() {
        let root = temp_dir("vanish");
        fs::write(root.join("x.txt"), b"x").unwrap();

        let mut conn = test_conn();
        rescan_subtree(&mut conn, &root, &[], &[]).unwrap();
        assert_eq!(db_paths(&conn).len(), 2);

        fs::remove_dir_all(&root).unwrap();
        let (upserted, deleted) = rescan_subtree(&mut conn, &root, &[], &[]).unwrap();
        assert_eq!(upserted, 0);
        assert_eq!(deleted, 2);
        assert!(db_paths(&conn).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rescan_via_symlink_root_uses_stored_prefix() {
        let real = temp_dir("symreal");
        fs::write(real.join("f.txt"), b"f").unwrap();
        let link = crate::temp_case_dir("rescan_symlink");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut conn = test_conn();
        rescan_subtree(&mut conn, &link, &[], &[]).unwrap();
        let link_str = link.to_string_lossy();
        assert!(
            db_paths(&conn).contains(&format!("{link_str}/f.txt")),
            "rows must be stored under the symlink prefix, got {:?}",
            db_paths(&conn)
        );

        // Deletion must also be detected through the symlink root.
        fs::remove_file(real.join("f.txt")).unwrap();
        let (_, deleted) = rescan_subtree(&mut conn, &link, &[], &[]).unwrap();
        assert_eq!(deleted, 1);

        fs::remove_file(&link).ok();
        fs::remove_dir_all(&real).ok();
    }

    #[test]
    fn subtree_diff_only_covers_prefix() {
        let conn = test_conn();
        conn.execute_batch(
            "INSERT INTO entries(path, name, dir, is_dir, mtime, size, indexed_at) VALUES
                ('/t/sub', 'sub', '/t', 1, 1, NULL, 1),
                ('/t/sub/file', 'file', '/t/sub', 0, 1, 1, 1),
                ('/t/subling/file', 'file', '/t/subling', 0, 1, 1, 1);",
        )
        .unwrap();

        let diff = SubtreeDiff::load(&conn, "/t/sub");
        let mut vanished = diff.leftover_paths(&conn, "/t/sub");
        vanished.sort();
        assert_eq!(
            vanished,
            vec!["/t/sub".to_string(), "/t/sub/file".to_string()],
            "sibling '/t/subling' must not be treated as part of the subtree"
        );
    }

    #[test]
    fn errored_prefixes_are_excluded_from_deletion() {
        let conn = test_conn();
        conn.execute_batch(
            "INSERT INTO entries(path, name, dir, is_dir, mtime, size, indexed_at) VALUES
                ('/t/sub/locked', 'locked', '/t/sub', 1, 1, NULL, 1),
                ('/t/sub/locked/file', 'file', '/t/sub/locked', 0, 1, 1, 1),
                ('/t/sub/gone.txt', 'gone.txt', '/t/sub', 0, 1, 1, 1);",
        )
        .unwrap();

        let mut diff = SubtreeDiff::load(&conn, "/t/sub");
        diff.mark_errored(Path::new("/t/sub/locked"));
        let vanished = diff.leftover_paths(&conn, "/t/sub");
        assert_eq!(
            vanished,
            vec!["/t/sub/gone.txt".to_string()],
            "rows under an unreadable prefix must be kept, not deleted"
        );
    }
}
