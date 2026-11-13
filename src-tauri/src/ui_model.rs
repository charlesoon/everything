//! Renderer-independent search/selection state. No DB writes or window APIs.

use std::{collections::BTreeSet, path::Path, sync::atomic::Ordering};

use crate::{runtime::EventSink, AppResult, AppState, EntryDto, IndexState, SearchResultDto};

#[derive(Default)]
pub(crate) struct SearchModel {
    pub rows: Vec<EntryDto>,
    pub loaded_count: usize,
    pub selected: BTreeSet<usize>,
    pub primary: Option<usize>,
    pub anchor: Option<usize>,
    pub generation: u64,
    pub query: String,
    pub total: u32,
    pub total_known: bool,
    pub mode: String,
    pub busy: bool,
    pub paging: bool,
    pub eof: bool,
    pub error: Option<String>,
    pub removed: BTreeSet<String>,
}

impl SearchModel {
    pub fn new_query(&mut self, query: String) -> u64 {
        self.generation += 1;
        self.query = query;
        self.busy = true;
        self.paging = false;
        self.error = None;
        self.clear_selection();
        self.loaded_count = 0;
        if self.query.trim().is_empty() {
            self.rows.clear();
            self.total = 0;
            self.total_known = true;
            self.eof = false;
        }
        self.generation
    }

    pub fn paths(&self) -> Vec<String> {
        self.selected
            .iter()
            .filter_map(|i| self.rows.get(*i).map(|e| e.path.clone()))
            .collect()
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.primary = None;
        self.anchor = None;
    }

    pub fn select(&mut self, index: usize, toggle: bool, range: bool) {
        if index >= self.rows.len() {
            return;
        }
        if range {
            let anchor = self.anchor.or(self.primary).unwrap_or(index);
            self.selected = (anchor.min(index)..=anchor.max(index)).collect();
            self.anchor = Some(anchor);
        } else if toggle {
            if !self.selected.remove(&index) {
                self.selected.insert(index);
            }
            self.anchor = Some(index);
        } else {
            self.selected = BTreeSet::from([index]);
            self.anchor = Some(index);
        }
        self.primary = if self.selected.contains(&index) {
            Some(index)
        } else {
            self.selected.iter().next_back().copied()
        };
    }

    /// Returns false for old queries and out-of-order pages; neither may mutate
    /// current rows, busy flags, or selection.
    pub fn apply(
        &mut self,
        generation: u64,
        offset: usize,
        result: SearchResultDto,
        preserve: bool,
        page_limit: usize,
    ) -> bool {
        if generation != self.generation || (offset > 0 && offset != self.loaded_count) {
            return false;
        }
        let paths: BTreeSet<_> = if preserve {
            self.paths().into_iter().collect()
        } else {
            BTreeSet::new()
        };
        let primary = if preserve {
            self.primary
                .and_then(|i| self.rows.get(i))
                .map(|e| e.path.clone())
        } else {
            None
        };
        let fetched = result.entries.len();
        self.loaded_count = offset + fetched;
        let entries = result
            .entries
            .into_iter()
            .filter(|entry| !self.is_removed(&entry.path))
            .collect::<Vec<_>>();
        if offset == 0 {
            self.rows = entries;
        } else {
            self.rows.extend(entries);
        }
        if offset == 0 {
            self.selected = self
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, e)| paths.contains(&e.path).then_some(i))
                .collect();
            self.primary = primary
                .and_then(|path| self.rows.iter().position(|e| e.path == path))
                .or_else(|| self.selected.iter().next().copied());
            self.anchor = self.primary;
        }
        self.total = result
            .total_count
            .saturating_sub(self.loaded_count.saturating_sub(self.rows.len()) as u32);
        self.total_known = result.total_known;
        self.mode = result.mode_label;
        self.busy = false;
        self.paging = false;
        self.error = None;
        self.eof =
            fetched < page_limit || (self.total_known && self.rows.len() >= self.total as usize);
        true
    }

    pub fn is_removed(&self, path: &str) -> bool {
        self.removed
            .iter()
            .any(|root| Path::new(path).starts_with(root))
    }

    pub fn remove_paths(&mut self, paths: &[String]) {
        self.removed.extend(paths.iter().cloned());
        let removed = &self.removed;
        let previous = self.rows.len();
        self.rows.retain(|entry| {
            !removed
                .iter()
                .any(|root| Path::new(&entry.path).starts_with(root))
        });
        self.total = self
            .total
            .saturating_sub((previous - self.rows.len()) as u32);
        self.clear_selection();
    }
}

/// Explicit, isolated fixture mode for reproducible native UI tests. Never
/// starts/registers a daemon or touches the user's real preferences/index.
pub(crate) fn fixture_runtime(
    dir: &Path,
) -> AppResult<(AppState, EventSink, async_channel::Receiver<()>)> {
    if std::env::var_os("EVERYTHING_DATA_DIR").is_none() {
        return Err("EVERYTHING_UI_TEST requires an isolated EVERYTHING_DATA_DIR".into());
    }
    let db_path = dir.join(crate::DB_FILE_NAME);
    if !db_path.is_file() {
        return Err(format!("Fixture index missing: {}", db_path.display()));
    }
    crate::set_db_readonly();
    let state = crate::build_app_state(db_path, dir.parent().unwrap_or(dir).join("files"), dir);
    state.db_ready.store(true, Ordering::Release);
    state.frontend_ready.store(true, Ordering::Release);
    let conn = crate::db_connection_for_search(&state.db_path)?;
    state
        .fts_ready
        .store(crate::fts_usable(&conn), Ordering::Release);
    {
        let mut status = state.status.lock();
        status.state = IndexState::Ready;
        status.entries_count = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
            .map_err(|e| e.to_string())? as u64;
        status.last_updated =
            crate::get_meta(&conn, "cached_last_updated").and_then(|v| v.parse().ok());
    }
    let (sink, receiver) = EventSink::channel();
    Ok((state, sink, receiver))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str) -> EntryDto {
        EntryDto {
            path: path.into(),
            name: path.into(),
            dir: "/".into(),
            is_dir: false,
            ext: None,
            size: None,
            mtime: None,
        }
    }
    fn response(paths: &[&str]) -> SearchResultDto {
        SearchResultDto {
            entries: paths.iter().map(|p| row(p)).collect(),
            mode_label: "name".into(),
            total_count: 999,
            total_known: true,
        }
    }

    #[test]
    fn stale_search_and_out_of_order_page_cannot_replace_new_query() {
        let mut model = SearchModel::default();
        let old = model.new_query("old".into());
        let new = model.new_query("new".into());
        assert!(!model.apply(old, 0, response(&["/old"]), false, 300));
        assert!(model.busy);
        assert!(model.apply(new, 0, response(&["/new"]), false, 300));
        assert!(!model.apply(new, 300, response(&["/out-of-order"]), true, 300));
        assert_eq!(model.rows[0].path, "/new");
        model.new_query("".into());
        assert!(!model.apply(new, 0, response(&["/new"]), false, 300));
        assert!(model.rows.is_empty());
    }

    #[test]
    fn multi_selection_uses_stable_anchor_and_tracks_paths_on_refresh() {
        let mut model = SearchModel::default();
        model.apply(0, 0, response(&["/a", "/b", "/c", "/d"]), false, 300);
        model.select(1, false, false);
        model.select(3, false, true);
        assert_eq!(model.paths(), vec!["/b", "/c", "/d"]);
        model.select(2, false, true);
        assert_eq!(model.paths(), vec!["/b", "/c"]);
        model.select(0, true, false);
        model.apply(0, 0, response(&["/d", "/c", "/a"]), true, 300);
        assert_eq!(model.paths(), vec!["/c", "/a"]);
        assert_eq!(model.primary, Some(2));
    }

    #[test]
    fn trashed_subtree_is_hidden_without_hiding_sibling_prefix() {
        let mut model = SearchModel::default();
        model.removed.insert("/folder".into());
        model.apply(
            0,
            0,
            response(&["/folder", "/folder/file", "/folder-other/file"]),
            false,
            300,
        );
        assert_eq!(model.rows.len(), 1);
        assert_eq!(model.rows[0].path, "/folder-other/file");
        assert_eq!(model.total, 997);
        assert_eq!(model.loaded_count, 3);
        model.apply(0, 3, response(&["/another"]), true, 300);
        assert_eq!(model.total, 997);
        assert_eq!(model.loaded_count, 4);
    }

    #[test]
    fn hidden_trash_rows_do_not_end_paging_before_the_last_visible_result() {
        let mut model = SearchModel::default();
        model.removed.insert("/trashed".into());
        let mut first = response(&["/a", "/trashed", "/b"]);
        first.total_count = 4;
        model.apply(0, 0, first, false, 3);
        assert_eq!(model.total, 3);
        assert_eq!(model.loaded_count, 3);
        assert!(!model.eof);
        let mut last = response(&["/last"]);
        last.total_count = 4;
        model.apply(0, 3, last, true, 3);
        assert_eq!(model.rows.last().unwrap().path, "/last");
        assert!(model.eof);
    }
}
