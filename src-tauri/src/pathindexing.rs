use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::AppResult;

#[cfg(target_os = "macos")]
use std::collections::HashSet;
#[cfg(target_os = "macos")]
use crate::{
    db_connection, delete_paths, index_row_from_path_and_metadata,
    invalidate_search_caches, should_skip_path, touch_status_updated, upsert_rows,
    AppState, IgnorePattern, BATCH_SIZE,
};

const DEFAULT_PATHINDEXING_CONTENTS: &str = "\
# Everything - additional index directories
# Add absolute paths to directories you want to index (one per line).
# Lines starting with # are comments.
# Example:
# /Volumes/ExternalDrive/Projects
# /opt/mydata
";

pub(crate) fn ensure_pathindexing_exists(path: &Path) -> AppResult<()> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, DEFAULT_PATHINDEXING_CONTENTS).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn load_pathindexing_roots(path: &Path) -> Vec<PathBuf> {
    // Windows: never auto-create the sidecar (D1); the editor command creates it
    // on demand. Other platforms keep seeding a default template at startup.
    #[cfg(not(target_os = "windows"))]
    let _ = ensure_pathindexing_exists(path);
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_pathindexing_entries(&contents)
}

pub(crate) fn parse_pathindexing_entries(content: &str) -> Vec<PathBuf> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let p = PathBuf::from(l);
            if !p.is_absolute() || !p.is_dir() {
                return None;
            }
            // D1: `config\` is Users-writable, so a directly-edited
            // `.pathindexing` is untrusted input to a LocalSystem service.
            // Only allow roots that actually resolve onto the C: volume, so an
            // edited entry (or a junction) can't redirect the service to scan
            // another volume / network / device path (confused-deputy guard).
            #[cfg(target_os = "windows")]
            if !is_indexable_windows_root(&p) {
                eprintln!(
                    "[pathindexing] ignoring root outside the C: volume: {}",
                    p.display()
                );
                return None;
            }
            Some(p)
        })
        .collect()
}

/// D1: true only when `p` resolves (following any reparse points) onto the `C:`
/// volume. `canonicalize` fails for a non-existent path and rewrites junctions/
/// symlinks to their real target, so a redirect off C: is caught here.
#[cfg(target_os = "windows")]
fn is_indexable_windows_root(p: &Path) -> bool {
    let Ok(canon) = std::fs::canonicalize(p) else {
        return false;
    };
    let s = canon.to_string_lossy();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    let b = s.as_bytes();
    b.len() >= 2 && b[0].eq_ignore_ascii_case(&b'C') && b[1] == b':'
}

#[cfg(target_os = "macos")]
pub(crate) fn parse_pathindexing_paths_unchecked(content: &str) -> Vec<PathBuf> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let p = PathBuf::from(l);
            if p.is_absolute() { Some(p) } else { None }
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub(crate) fn pathindexing_active_entries(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

pub(crate) fn open_pathindexing_file(path: &Path) -> AppResult<()> {
    ensure_pathindexing_exists(path)?;
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

#[cfg(target_os = "macos")]
pub(crate) fn scan_extra_roots(
    state: &AppState,
    roots: &[PathBuf],
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> AppResult<usize> {
    if roots.is_empty() {
        return Ok(0);
    }

    let mut conn = db_connection(&state.db_path)?;
    let mut total_changed = 0;
    let mut batch = Vec::with_capacity(BATCH_SIZE);

    for root in roots {
        if !root.is_dir() {
            continue;
        }
        eprintln!("[pathindexing] scanning extra root: {}", root.display());

        // Index the root directory itself so resolve_dirs_from_db can find it
        // (enables fast range-based queries instead of LIKE full-table scan)
        if let Ok(meta) = fs::symlink_metadata(root) {
            if let Some(row) = index_row_from_path_and_metadata(root, &meta) {
                batch.push(row);
            }
        }

        let skip_roots: Vec<PathBuf> = ignored_roots.to_vec();
        let skip_patterns: Vec<IgnorePattern> = ignored_patterns.to_vec();
        let walker = jwalk::WalkDirGeneric::<((), Option<fs::Metadata>)>::new(root)
            .follow_links(false)
            .skip_hidden(false)
            .process_read_dir(move |_depth, path, _state, children| {
                children.retain_mut(|entry_result| match entry_result {
                    Ok(entry) => {
                        let full_path = path.join(&entry.file_name);
                        if should_skip_path(&full_path, &skip_roots, &skip_patterns) {
                            return false;
                        }
                        entry.client_state = fs::symlink_metadata(&full_path).ok();
                        true
                    }
                    Err(_) => false,
                });
            });

        for result in walker {
            if let Ok(entry) = result {
                let path = entry.path();
                if path == root.as_path() {
                    continue;
                }
                let metadata = match entry.client_state {
                    Some(m) => m,
                    None => continue,
                };
                if let Some(row) = index_row_from_path_and_metadata(&path, &metadata) {
                    batch.push(row);
                    if batch.len() >= BATCH_SIZE {
                        total_changed += upsert_rows(&mut conn, &batch)?;
                        batch.clear();
                    }
                }
            }
        }
    }

    if !batch.is_empty() {
        total_changed += upsert_rows(&mut conn, &batch)?;
    }
    Ok(total_changed)
}

#[cfg(target_os = "macos")]
pub(crate) fn remove_extra_root_entries(
    state: &AppState,
    roots: &[PathBuf],
) -> AppResult<usize> {
    if roots.is_empty() {
        return Ok(0);
    }

    let mut conn = db_connection(&state.db_path)?;
    let mut total = 0;
    for root in roots {
        let root_str = root.to_string_lossy().to_string();
        let paths_to_delete = vec![root_str];
        total += delete_paths(&mut conn, &paths_to_delete)?;
    }
    Ok(total)
}

#[cfg(target_os = "macos")]
pub(crate) fn handle_pathindexing_change(
    state: &AppState,
    old_roots: &[PathBuf],
    new_roots: &[PathBuf],
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
) -> AppResult<()> {
    let old_set: HashSet<&PathBuf> = old_roots.iter().collect();
    let new_set: HashSet<&PathBuf> = new_roots.iter().collect();

    let added: Vec<PathBuf> = new_set.difference(&old_set).map(|p| (*p).clone()).collect();
    let removed: Vec<PathBuf> = old_set.difference(&new_set).map(|p| (*p).clone()).collect();

    if !removed.is_empty() {
        eprintln!("[pathindexing] removing {} roots from index", removed.len());
        remove_extra_root_entries(state, &removed)?;
    }

    if !added.is_empty() {
        eprintln!("[pathindexing] scanning {} new roots", added.len());
        scan_extra_roots(state, &added, ignored_roots, ignored_patterns)?;
    }

    if !added.is_empty() || !removed.is_empty() {
        invalidate_search_caches(state);
        touch_status_updated(state);
    }

    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn parse_entries_skips_comments_and_empty() {
        let content = "# comment\n\n  /tmp/foo  \n# another\n/tmp/bar\n";
        let entries = pathindexing_active_entries(content);
        assert_eq!(entries, vec!["/tmp/foo", "/tmp/bar"]);
    }

}
