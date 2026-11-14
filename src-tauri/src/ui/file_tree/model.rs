use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}

impl Entry {
    fn directory(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .unwrap_or_else(|| path.as_os_str())
            .to_string_lossy()
            .into_owned();
        Self {
            path,
            name,
            is_dir: true,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum RowKind {
    Entry(Entry),
    Loading,
    Empty,
    Error(String),
}

#[derive(Clone, Debug)]
pub(super) struct Row {
    pub depth: usize,
    pub kind: RowKind,
}

impl Row {
    pub fn entry(&self) -> Option<&Entry> {
        match &self.kind {
            RowKind::Entry(entry) => Some(entry),
            _ => None,
        }
    }
}

enum Directory {
    Loading { id: u64, cached: Option<Vec<Entry>> },
    Ready(Vec<Entry>),
    Error(String),
}

pub(super) struct Request {
    pub path: PathBuf,
    id: u64,
}

#[derive(Default)]
pub(super) struct FileTree {
    pub root: Option<PathBuf>,
    pub selected: Option<PathBuf>,
    pub rows: Vec<Row>,
    pub reveal_pending: bool,
    expanded: HashSet<PathBuf>,
    directories: HashMap<PathBuf, Directory>,
    invalidated: HashSet<PathBuf>,
    next_request: u64,
}

impl FileTree {
    /// Keep a useful browsing context while revealing every ancestor of the
    /// result. A result outside that context starts at its immediate parent.
    pub fn follow(&mut self, path: PathBuf, is_dir: bool) {
        if self
            .root
            .as_ref()
            .is_none_or(|root| !path.starts_with(root))
        {
            self.set_root(path.parent().unwrap_or(&path).to_path_buf());
        }
        self.selected = Some(path.clone());
        self.expand_ancestors(&path);
        if is_dir {
            self.expanded.insert(path.clone());
        }
        // An indexed result may have appeared since we last read this folder.
        if let Some(parent) = path.parent() {
            if self
                .directories
                .get(parent)
                .is_some_and(|directory| match directory {
                    Directory::Ready(entries) => !entries.iter().any(|e| e.path == path),
                    Directory::Error(_) => true,
                    Directory::Loading { .. } => false,
                })
            {
                self.invalidated.insert(parent.to_path_buf());
            }
        }
        self.reveal_pending = true;
        self.rebuild();
    }

    fn set_root(&mut self, path: PathBuf) {
        self.root = Some(path.clone());
        self.expanded.clear();
        self.expanded.insert(path);
        self.directories.clear();
        self.invalidated.clear();
    }

    fn expand_ancestors(&mut self, path: &Path) {
        for parent in path.ancestors().skip(1) {
            if !self
                .root
                .as_ref()
                .is_some_and(|root| parent.starts_with(root))
            {
                break;
            }
            self.expanded.insert(parent.to_path_buf());
        }
    }

    pub fn go_up(&mut self) {
        let Some(parent) = self.root.as_ref().and_then(|root| root.parent()) else {
            return;
        };
        let previous = self.root.clone().unwrap();
        let parent = parent.to_path_buf();
        self.root = Some(parent.clone());
        self.expanded.insert(parent);
        self.expanded.insert(previous);
        if let Some(selected) = self.selected.clone() {
            self.expand_ancestors(&selected);
        }
        self.reveal_pending = true;
        self.rebuild();
    }

    pub fn refresh(&mut self) {
        // Keep the current rows while reading replacements. Temporarily
        // shrinking the list to a loading row clamps its scroll offset to zero.
        self.invalidated.extend(self.directories.keys().cloned());
    }

    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    pub fn is_loading(&self) -> bool {
        self.directories.iter().any(|(path, directory)| {
            !self.invalidated.contains(path) && matches!(directory, Directory::Loading { .. })
        })
    }

    pub fn toggle(&mut self, path: &Path) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
            // Reopening a folder also picks up changes made outside the app.
            self.invalidated.insert(path.to_path_buf());
        }
        self.reveal_pending = false;
        self.rebuild();
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.rows.iter().position(|row| {
            row.entry()
                .is_some_and(|entry| self.selected.as_ref() == Some(&entry.path))
        })
    }

    pub fn selection_error(&self) -> Option<String> {
        if !self.reveal_pending
            || self
                .rows
                .iter()
                .any(|row| matches!(row.kind, RowKind::Error(_)))
        {
            return None;
        }
        let mut child = self.selected.as_deref()?;
        for parent in child.ancestors().skip(1) {
            if !self
                .root
                .as_ref()
                .is_some_and(|root| parent.starts_with(root))
            {
                break;
            }
            match self.directories.get(parent)? {
                Directory::Loading { .. } => return None,
                Directory::Error(error) => return Some(format!("Cannot read folder: {error}")),
                Directory::Ready(entries) if !entries.iter().any(|entry| entry.path == child) => {
                    return Some(
                        "Selected item is no longer available. Refresh to try again.".into(),
                    );
                }
                Directory::Ready(_) => {}
            }
            child = parent;
        }
        None
    }

    pub fn requests(&mut self) -> Vec<Request> {
        let paths = self
            .expanded
            .iter()
            .filter(|path| {
                (!self.directories.contains_key(*path) || self.invalidated.contains(*path))
                    && path
                        .ancestors()
                        .skip(1)
                        .take_while(|parent| {
                            self.root
                                .as_ref()
                                .is_some_and(|root| parent.starts_with(root))
                        })
                        .all(|parent| self.expanded.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>();
        paths
            .into_iter()
            .map(|path| {
                self.next_request += 1;
                let id = self.next_request;
                let cached = match self.directories.remove(&path) {
                    Some(Directory::Ready(entries))
                    | Some(Directory::Loading {
                        cached: Some(entries),
                        ..
                    }) => Some(entries),
                    _ => None,
                };
                self.invalidated.remove(&path);
                self.directories
                    .insert(path.clone(), Directory::Loading { id, cached });
                Request { path, id }
            })
            .collect()
    }

    pub fn apply(&mut self, request: Request, result: Result<Vec<Entry>, String>) -> bool {
        // A refresh, new root, or collapse/reopen can supersede an in-flight
        // read of the same path. Its response must not replace the newer one.
        if self.invalidated.contains(&request.path)
            || !matches!(self.directories.get(&request.path), Some(Directory::Loading { id, .. }) if *id == request.id)
        {
            return false;
        }
        self.directories.insert(
            request.path,
            match result {
                Ok(entries) => Directory::Ready(entries),
                Err(error) => Directory::Error(error),
            },
        );
        self.rebuild();
        true
    }

    fn rebuild(&mut self) {
        let mut rows = Vec::new();
        if let Some(root) = &self.root {
            self.append(Entry::directory(root.clone()), 0, &mut rows);
        }
        self.rows = rows;
    }

    fn append(&self, entry: Entry, depth: usize, rows: &mut Vec<Row>) {
        let expanded = entry.is_dir && self.is_expanded(&entry.path);
        let directory = self.directories.get(&entry.path);
        rows.push(Row {
            depth,
            kind: RowKind::Entry(entry),
        });
        if !expanded {
            return;
        }
        let kind = match directory {
            Some(Directory::Ready(entries))
            | Some(Directory::Loading {
                cached: Some(entries),
                ..
            }) if !entries.is_empty() => {
                for child in entries {
                    self.append(child.clone(), depth + 1, rows);
                }
                return;
            }
            Some(Directory::Ready(_))
            | Some(Directory::Loading {
                cached: Some(_), ..
            }) => RowKind::Empty,
            Some(Directory::Error(error)) => RowKind::Error(error.clone()),
            _ => RowKind::Loading,
        };
        rows.push(Row {
            depth: depth + 1,
            kind,
        });
    }
}

/// Shallow, unfiltered disk listing: siblings need not match the search query
/// or belong to the index. Call only on a background executor.
pub(super) fn read_directory(path: &Path) -> Result<Vec<Entry>, String> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let is_dir = file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir());
            Ok(Entry {
                path: entry.path(),
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir,
            })
        })
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    entries
        .sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase(), entry.name.clone()));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> PathBuf {
        std::env::temp_dir().join("everything-tree-model")
    }

    fn file(path: PathBuf) -> Entry {
        Entry {
            name: path.file_name().unwrap().to_string_lossy().into(),
            path,
            is_dir: false,
        }
    }

    #[test]
    fn follows_nested_results_without_losing_siblings_or_browsing_context() {
        let root = context();
        let nested = root.join("folder/deeper");
        let selected = nested.join("target.txt");
        let mut tree = FileTree::default();
        tree.follow(root.join("first.txt"), false);
        tree.follow(selected.clone(), false);
        assert_eq!(tree.root.as_ref(), Some(&root));
        let requests = tree.requests();
        assert_eq!(requests.len(), 3);
        for request in requests {
            let entries = if request.path == root {
                vec![
                    Entry::directory(root.join("folder")),
                    file(root.join("first.txt")),
                ]
            } else if request.path == root.join("folder") {
                vec![Entry::directory(nested.clone())]
            } else {
                vec![file(selected.clone()), file(nested.join("sibling.md"))]
            };
            assert!(tree.apply(request, Ok(entries)));
        }
        let index = tree.selected_index().unwrap();
        assert_eq!(tree.rows[index].depth, 3);
        assert_eq!(tree.rows.len(), 6);
        tree.toggle(&root.join("folder"));
        assert!(tree.selected_index().is_none());
        tree.follow(selected, false);
        assert!(tree.selected_index().is_some());
    }

    #[test]
    fn refresh_keeps_cached_rows_and_selection_until_the_latest_read_finishes() {
        let root = context();
        let mut tree = FileTree::default();
        let selected = root.join("target.txt");
        let entries = || {
            (0..200)
                .map(|index| file(root.join(format!("sibling-{index:03}.txt"))))
                .chain([file(selected.clone())])
                .collect()
        };
        tree.follow(selected.clone(), false);
        let request = tree.requests().pop().unwrap();
        tree.apply(request, Ok(entries()));
        let index = tree.selected_index();
        tree.reveal_pending = false;

        tree.refresh();
        assert_eq!(tree.rows.len(), 202);
        assert_eq!(tree.selected_index(), index);
        let stale = tree.requests().pop().unwrap();
        assert!(tree.is_loading());
        assert_eq!(tree.rows.len(), 202);
        assert_eq!(tree.selected_index(), index);

        // Another refresh must reject the old response even before a new
        // request has been submitted, and keep displaying the cached entries.
        tree.refresh();
        assert!(!tree.apply(stale, Ok(Vec::new())));
        assert!(!tree.is_loading());
        let request = tree.requests().pop().unwrap();
        tree.follow(root.join("sibling-100.txt"), false);
        assert_eq!(tree.rows.len(), 202);
        assert!(tree.apply(request, Ok(entries())));
        assert_eq!(tree.selected_index(), Some(101));
        assert!(!tree.is_loading());
    }

    #[test]
    fn stale_reads_cannot_replace_a_new_root_or_a_refreshed_folder() {
        let root = context();
        let mut tree = FileTree::default();
        tree.follow(root.join("a/target"), false);
        let old = tree.requests().pop().unwrap();
        tree.follow(root.join("ab/target"), false);
        assert!(!tree.apply(old, Ok(Vec::new())));
        let old = tree.requests().pop().unwrap();
        tree.refresh();
        let new = tree.requests().pop().unwrap();
        assert!(tree.apply(new, Ok(vec![file(root.join("ab/target"))])));
        assert!(!tree.apply(old, Err("outdated failure".into())));
        assert!(tree.selected_index().is_some());
    }

    #[test]
    fn unavailable_result_is_reported_even_when_its_ancestor_is_missing_from_the_tree() {
        let root = context();
        let mut tree = FileTree::default();
        tree.follow(root.join("real.txt"), false);
        let request = tree.requests().pop().unwrap();
        tree.apply(request, Ok(vec![file(root.join("real.txt"))]));
        tree.follow(root.join("missing/target.txt"), false);
        let request = tree.requests().pop().unwrap();
        tree.apply(request, Err("Folder does not exist".into()));
        assert!(tree.selected_index().is_none());
        assert!(!tree
            .rows
            .iter()
            .any(|row| matches!(row.kind, RowKind::Error(_))));
        assert_eq!(
            tree.selection_error().as_deref(),
            Some("Cannot read folder: Folder does not exist")
        );
        tree.follow(root.join("real.txt"), false);
        assert!(tree.selection_error().is_none());
    }

    #[test]
    fn folder_selection_expands_contents_and_parent_navigation_preserves_reveal() {
        let root = context();
        let folder = root.join("parent/folder");
        let mut tree = FileTree::default();
        tree.follow(folder.clone(), true);
        assert!(tree.is_expanded(&folder));
        tree.go_up();
        assert_eq!(tree.root, Some(root.clone()));
        assert!(tree.is_expanded(&root.join("parent")));
        assert_eq!(tree.selected, Some(folder));
        assert_eq!(tree.requests().len(), 3);
    }

    #[test]
    fn collapsed_subtrees_are_not_read_on_refresh_and_errors_can_be_retried() {
        let root = context();
        let mut tree = FileTree::default();
        tree.follow(root.join("folder/target"), false);
        tree.go_up();
        tree.toggle(&root);
        assert!(tree.requests().is_empty());
        tree.toggle(&root);
        for request in tree.requests() {
            tree.apply(request, Err("Access denied".into()));
        }
        assert!(matches!(&tree.rows[1].kind, RowKind::Error(error) if error == "Access denied"));
        tree.refresh();
        assert!(!tree.requests().is_empty());
    }

    #[test]
    fn disk_listing_includes_hidden_and_unindexed_siblings_and_is_shallow() {
        let root =
            std::env::temp_dir().join(format!("everything-tree-disk-{}", std::process::id()));
        std::fs::create_dir_all(root.join("z-folder/nested")).unwrap();
        std::fs::write(root.join("z-folder/nested/child.txt"), b"child").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();
        std::fs::write(root.join("A.md"), b"a").unwrap();
        std::fs::write(root.join(".hidden"), b"hidden").unwrap();
        let entries = read_directory(&root).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["z-folder", ".hidden", "A.md", "b.txt"]
        );
        assert!(entries[0].is_dir);
        assert!(!entries.iter().any(|entry| entry.name == "child.txt"));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(read_directory(&root).is_err());
    }
}
