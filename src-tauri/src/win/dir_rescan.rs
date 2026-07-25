//! Single-flight, cooldown-throttled subtree rescan queue for the Windows USN
//! watcher (design §3.0 S6).
//!
//! A directory rename/move emits USN records only for the directory entry
//! itself, never for its descendants, so the descendants' stored paths silently
//! go stale. Reconciling BOTH prefixes with [`rescan::rescan_subtree`] repairs
//! this with one mechanism: the old prefix has vanished from disk, so its rows
//! are deleted; the new prefix exists, so its subtree is reinserted.
//!
//! A rescan can walk millions of entries over many seconds, so it must not run
//! on the watcher poll thread. This queue mirrors the macOS `MustScanSubDirs`
//! machinery in `main.rs`: coalesce paths under a queued ancestor, defer
//! overlapping rescans past a cooldown (deferred, never dropped), and run at
//! most one rescan at a time on a background thread that holds `indexing_active`
//! so it can't race the full-index writer or the live watcher's upserts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::AppHandle;

use crate::{
    cached_effective_ignore_rules, db_connection, invalidate_search_caches,
    refresh_and_emit_status_counts, rescan, update_status_counts, AppState,
};

/// Minimum spacing between two rescans of overlapping subtrees. Requests
/// arriving inside the window are deferred (kept queued), not dropped, so a
/// burst of directory renames can't trigger back-to-back full-subtree walks.
const RESCAN_COOLDOWN: Duration = Duration::from_secs(300);

/// Owned by the single USN poll thread. `queued` is touched only from that
/// thread (no lock needed); `finished`/`inflight` are shared with the
/// background rescan thread.
pub(crate) struct DirRescanQueue {
    queued: HashMap<PathBuf, Instant>,
    finished: Arc<Mutex<Vec<(PathBuf, Instant)>>>,
    inflight: Arc<AtomicBool>,
}

impl DirRescanQueue {
    pub(crate) fn new() -> Self {
        Self {
            queued: HashMap::new(),
            finished: Arc::new(Mutex::new(Vec::new())),
            inflight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Earliest instant `path` may be rescanned, given the cooldown of recent
    /// overlapping rescans. `Instant::now()` when nothing overlaps.
    fn not_before(&self, path: &Path) -> Instant {
        let mut nb = Instant::now();
        for (done, at) in self.finished.lock().iter() {
            if path.starts_with(done) || done.starts_with(path) {
                let earliest = *at + RESCAN_COOLDOWN;
                if earliest > nb {
                    nb = earliest;
                }
            }
        }
        nb
    }

    /// Queue `path` for a subtree rescan, collapsing paths already covered by a
    /// queued ancestor and deferring past the cooldown of recent overlapping
    /// rescans. The stored due time is a lower bound; [`pump`](Self::pump)
    /// re-checks the cooldown when it selects a path.
    pub(crate) fn queue(&mut self, path: PathBuf) {
        if self.queued.keys().any(|q| path.starts_with(q)) {
            return; // an equal or ancestor rescan is already queued
        }
        let nb = self.not_before(&path);
        // A wider rescan covers any queued descendants.
        self.queued.retain(|q, _| !q.starts_with(&path));
        self.queued.insert(path, nb);
    }

    /// Spawn the next due queued rescan on a background thread (single-flight).
    /// Skipped while an index pass is active — the pass reconciles the same
    /// ground — and retried on the next poll tick.
    pub(crate) fn pump(&mut self, app: Option<&AppHandle>, state: &AppState) {
        if self.queued.is_empty()
            || self.inflight.load(Ordering::Acquire)
            || state.indexing_active.load(Ordering::Acquire)
        {
            return;
        }
        let now = Instant::now();
        let Some(path) = self
            .queued
            .iter()
            .find(|(_, due)| **due <= now)
            .map(|(p, _)| p.clone())
        else {
            return;
        };
        // The stored due time is a lower bound; re-check now that an overlapping
        // rescan may have finished since this path was queued. If still cooling
        // down, push the due time out and leave it queued for a later tick.
        let nb = self.not_before(&path);
        if nb > now {
            self.queued.insert(path, nb);
            return;
        }
        self.queued.remove(&path);
        self.inflight.store(true, Ordering::Release);

        let app = app.cloned();
        let state = state.clone();
        let inflight = Arc::clone(&self.inflight);
        let finished = Arc::clone(&self.finished);
        std::thread::spawn(move || {
            // Hold the exclusive-writer guard for the rescan's duration: it opens
            // its own write connection and both walks and deletes over many
            // seconds, so without this it could race the full-index writer or the
            // live watcher's upserts (a deleted-and-recreated file wrongly swept
            // as vanished). Both already stand down on indexing_active.
            if state
                .indexing_active
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // An index pass started between the due-check and here; it
                // reconciles the same ground. Record no cooldown stamp so a
                // later rename re-queues this subtree.
                inflight.store(false, Ordering::Release);
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
            state.indexing_active.store(false, Ordering::Release);
            let succeeded = result.is_ok();
            match result {
                Ok((upserted, deleted)) => {
                    eprintln!(
                        "[win/usn] dir-rename rescan {}: upserted={} deleted={} {}ms",
                        path.display(),
                        upserted,
                        deleted,
                        started.elapsed().as_millis()
                    );
                    if upserted + deleted > 0 {
                        invalidate_search_caches(&state);
                        let _ = update_status_counts(&state);
                        let _ = refresh_and_emit_status_counts(app.as_ref(), &state);
                    }
                }
                Err(err) => eprintln!(
                    "[win/usn] dir-rename rescan {} failed: {err}",
                    path.display()
                ),
            }
            let mut finished = finished.lock();
            // Only stamp the cooldown on success: a failed rescan left the
            // subtree unreconciled, so it must not be blocked from a prompt retry.
            if succeeded {
                finished.push((path, Instant::now()));
            }
            finished.retain(|(_, at)| at.elapsed() < RESCAN_COOLDOWN);
            inflight.store(false, Ordering::Release);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_collapses_descendants_under_ancestor() {
        let mut q = DirRescanQueue::new();
        q.queue(PathBuf::from("C:\\a\\b"));
        // A descendant of an already-queued path is covered — not added.
        q.queue(PathBuf::from("C:\\a\\b\\c"));
        assert_eq!(q.queued.len(), 1);
        assert!(q.queued.contains_key(&PathBuf::from("C:\\a\\b")));

        // Queuing an ancestor subsumes the queued descendant.
        q.queue(PathBuf::from("C:\\a"));
        assert_eq!(q.queued.len(), 1);
        assert!(q.queued.contains_key(&PathBuf::from("C:\\a")));
        assert!(!q.queued.contains_key(&PathBuf::from("C:\\a\\b")));
    }

    #[test]
    fn queue_keeps_disjoint_paths() {
        let mut q = DirRescanQueue::new();
        q.queue(PathBuf::from("C:\\a\\b"));
        q.queue(PathBuf::from("C:\\x\\y"));
        assert_eq!(q.queued.len(), 2);
    }
}
