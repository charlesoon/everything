use std::{collections::HashSet, fs, io, path::Path};

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::{subtree_range_bounds, AppResult};

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrashResult {
    pub(crate) removed_paths: Vec<String>,
    pub(crate) failures: Vec<TrashFailure>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrashFailure {
    pub(crate) path: String,
    pub(crate) error: String,
}

pub(crate) fn move_to_trash(paths: &[String]) -> TrashResult {
    let context = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    let context = {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut context = context;
        // The default Finder backend needs Apple Events permission. FileManager
        // moves to the system Trash directly, including on external volumes.
        context.set_delete_method(DeleteMethod::NsFileManager);
        context
    };

    trash_paths_with(paths, |path| match context.delete(path) {
        Ok(()) => Ok(()),
        Err(_) if is_missing(Path::new(path)) => Ok(()),
        Err(error) => Err(error.to_string()),
    })
}

fn is_missing(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

fn trash_paths_with(
    paths: &[String],
    mut trash_path: impl FnMut(&str) -> AppResult<()>,
) -> TrashResult {
    let mut seen = HashSet::new();
    let mut targets: Vec<&String> = paths.iter().filter(|path| seen.insert(*path)).collect();
    // A selected directory can contain other selected results. Trash it first
    // and count its descendants as removed instead of failing on missing files.
    targets.sort_by_key(|path| Path::new(path).components().count());
    let mut removed_dirs = Vec::new();
    let mut result = TrashResult::default();

    for path in targets {
        let covered = removed_dirs
            .iter()
            .any(|dir| Path::new(path).starts_with(dir));
        let is_dir = fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir());
        let outcome = if covered { Ok(()) } else { trash_path(path) };
        match outcome {
            Ok(()) => {
                if is_dir {
                    removed_dirs.push(Path::new(path));
                }
                result.removed_paths.push(path.clone());
            }
            Err(error) => result.failures.push(TrashFailure {
                path: path.clone(),
                error,
            }),
        }
    }
    result
}

/// The GUI stays read-only. Hide successful deletions while the daemon's
/// watcher catches up; release them once no indexed subtree remains, or when
/// a file is recreated at the same path. symlink_metadata preserves symlinks.
pub(crate) fn pending_trash_paths(conn: &Connection, paths: &[String]) -> AppResult<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE path = ?1)
                 OR EXISTS(SELECT 1 FROM entries WHERE path >= ?2 AND path < ?3)",
        )
        .map_err(|error| error.to_string())?;
    let mut pending = Vec::new();
    for path in paths {
        if !is_missing(Path::new(path)) {
            continue;
        }
        let (start, end) = subtree_range_bounds(path);
        let indexed: bool = stmt
            .query_row(params![path, start, end], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if indexed {
            pending.push(path.clone());
        }
    }
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trash_continues_after_failure_and_reports_each_result() {
        let paths = vec!["/locked.txt".into(), "/ok.txt".into(), "/other.txt".into()];
        let mut attempted = Vec::new();
        let result = trash_paths_with(&paths, |path| {
            attempted.push(path.to_string());
            if path == "/locked.txt" {
                Err("Permission denied".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(attempted, paths);
        assert_eq!(result.removed_paths, vec!["/ok.txt", "/other.txt"]);
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].path, "/locked.txt");
    }

    #[test]
    fn trash_deduplicates_and_skips_children_of_trashed_directory() {
        let root = std::env::temp_dir().join(format!("everything-trash-{}", std::process::id()));
        let dir = root.join("folder");
        fs::create_dir_all(&dir).unwrap();
        let parent = dir.to_string_lossy().into_owned();
        let child = dir.join("child.txt").to_string_lossy().into_owned();
        let sibling = root.join("folder-other").to_string_lossy().into_owned();
        let mut attempted = Vec::new();
        let result = trash_paths_with(
            &[
                child.clone(),
                parent.clone(),
                parent.clone(),
                sibling.clone(),
            ],
            |path| {
                attempted.push(path.to_string());
                Ok(())
            },
        );
        assert_eq!(attempted, vec![parent.clone(), sibling.clone()]);
        assert_eq!(result.removed_paths, vec![parent, sibling, child]);
        assert!(result.failures.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_trash_waits_for_indexed_descendants_but_allows_recreation() {
        let root =
            std::env::temp_dir().join(format!("everything-pending-trash-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let removed = root.join("removed").to_string_lossy().into_owned();
        let recreated = root.join("recreated");
        fs::write(&recreated, "new file").unwrap();
        let recreated = recreated.to_string_lossy().into_owned();
        let prefix = root.join("prefix").to_string_lossy().into_owned();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE entries(path TEXT UNIQUE);")
            .unwrap();
        for path in [
            Path::new(&removed)
                .join("child.txt")
                .to_string_lossy()
                .into_owned(),
            recreated.clone(),
            format!("{prefix}-other"),
        ] {
            conn.execute("INSERT INTO entries VALUES (?1)", [path])
                .unwrap();
        }
        let paths = vec![removed.clone(), recreated, prefix];
        assert_eq!(pending_trash_paths(&conn, &paths).unwrap(), vec![removed]);
        conn.execute("DELETE FROM entries", []).unwrap();
        assert!(pending_trash_paths(&conn, &paths).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "moves temporary fixtures to the macOS Trash and restores them; run explicitly"]
    fn macos_trashes_multiple_files_and_selected_directory_with_child() {
        use objc2_foundation::{
            NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask, NSString, NSURL,
        };

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("everything-trash-check-{}-{stamp}", std::process::id());
        let root = std::env::temp_dir().join(&prefix);
        let folder = root.join(format!("{prefix}-folder"));
        fs::create_dir_all(&folder).unwrap();
        let first = root.join(format!("{prefix}-한글 one.txt"));
        let second = root.join(format!("{prefix}-two.txt"));
        let child = folder.join("child.txt");
        for path in [&first, &second, &child] {
            fs::write(path, "trash verification fixture").unwrap();
        }
        let manager = NSFileManager::defaultManager();
        let url = NSURL::fileURLWithPath(&NSString::from_str(first.to_str().unwrap()));
        let trash = manager
            .URLForDirectory_inDomain_appropriateForURL_create_error(
                NSSearchPathDirectory::TrashDirectory,
                NSSearchPathDomainMask::UserDomainMask,
                Some(&url),
                false,
            )
            .unwrap();
        let trash = std::path::PathBuf::from(trash.path().unwrap().to_string());
        let paths: Vec<String> = [&child, &first, &second, &folder]
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let result = move_to_trash(&paths);
        let sources_missing = paths.iter().all(|path| is_missing(Path::new(path)));
        let trashed_child = trash.join(folder.file_name().unwrap()).join("child.txt");
        let child_preserved = fs::read_to_string(trashed_child)
            .is_ok_and(|text| text == "trash verification fixture");
        // Restore only our uniquely named fixtures, then remove the test tree.
        for path in [&first, &second, &folder] {
            let trashed = trash.join(path.file_name().unwrap());
            if trashed.exists() {
                fs::rename(trashed, path).unwrap();
            }
        }
        fs::remove_dir_all(root).unwrap();
        assert!(result.failures.is_empty(), "{result:?}");
        assert_eq!(result.removed_paths.len(), 4);
        assert!(sources_missing, "all selected sources must disappear");
        assert!(
            child_preserved,
            "directory contents must reach the Trash intact"
        );
    }
}
