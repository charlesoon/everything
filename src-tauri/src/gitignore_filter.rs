use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

pub struct GitignoreFilter {
    matchers: Vec<Gitignore>,
}

impl std::fmt::Debug for GitignoreFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitignoreFilter")
            .field("matchers_count", &self.matchers.len())
            .finish()
    }
}

impl GitignoreFilter {
    pub fn build(home_dir: &Path) -> Self {
        let matchers = discover_gitignores(home_dir);
        Self { matchers }
    }

    #[allow(dead_code)]
    pub fn root_paths(&self) -> Vec<&Path> {
        self.matchers.iter().map(|gi| gi.path()).collect()
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        for gi in &self.matchers {
            // Only apply a gitignore to paths that are under its root directory.
            // An unanchored pattern like `target/` in /proj-a/.gitignore must not
            // match /proj-b/src/target — the ignore crate resolves it as `**/target`
            // and would otherwise match across unrelated projects.
            if !path.starts_with(gi.path()) {
                continue;
            }
            match gi.matched(path, is_dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }
        false
    }
}

fn discover_gitignores(home_dir: &Path) -> Vec<Gitignore> {
    let mut result = Vec::new();

    let Ok(entries) = std::fs::read_dir(home_dir) else {
        return result;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "Library" {
            continue;
        }

        collect_gitignores_recursive(&path, 0, 3, &mut result);
    }

    result
}

fn collect_gitignores_recursive(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    result: &mut Vec<Gitignore>,
) {
    if depth > max_depth {
        return;
    }

    let git_dir = dir.join(".git");
    if git_dir.exists() {
        let gitignore_path = dir.join(".gitignore");
        if gitignore_path.is_file() {
            let mut builder = GitignoreBuilder::new(dir);
            if builder.add(&gitignore_path).is_none() {
                if let Ok(gi) = builder.build() {
                    result.push(gi);
                }
            }
        }
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.')
            || name_str == "node_modules"
            || name_str == "Library"
            || name_str == "target"
        {
            continue;
        }

        collect_gitignores_recursive(&path, depth + 1, max_depth, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_gitignore(dir: &Path, contents: &str) {
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".gitignore"), contents).unwrap();
    }

    #[test]
    fn cross_project_unanchored_pattern_does_not_bleed() {
        let tmp = std::env::temp_dir().join(format!("gi_test_{}", std::process::id()));
        let proj_a = tmp.join("proj-a");
        let proj_b = tmp.join("proj-b");
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&proj_b).unwrap();

        // proj-a has `target/` — an unanchored pattern that the ignore crate resolves to `**/target`
        write_gitignore(&proj_a, "target/\n");
        // proj-b has a `target/` directory that must NOT be ignored
        let target_in_b = proj_b.join("target");
        fs::create_dir_all(&target_in_b).unwrap();
        write_gitignore(&proj_b, "dist/\n");

        let filter = GitignoreFilter::build(&tmp);
        // proj-b/target must not be ignored just because proj-a has `target/`
        assert!(!filter.is_ignored(&target_in_b, true), "cross-project gitignore bleed: target in proj-b was ignored by proj-a's .gitignore");
        // proj-a/target MUST be ignored (its own rule)
        let target_in_a = proj_a.join("target");
        fs::create_dir_all(&target_in_a).unwrap();
        assert!(filter.is_ignored(&target_in_a, true), "proj-a/target should be ignored by its own .gitignore");

        let _ = fs::remove_dir_all(&tmp);
    }
}

pub type SharedGitignoreFilter = Arc<GitignoreFilter>;

pub fn build_shared_filter(home_dir: &Path) -> SharedGitignoreFilter {
    Arc::new(GitignoreFilter::build(home_dir))
}

/// Lazy gitignore filter — defers expensive filesystem scan until first access.
pub struct LazyGitignoreFilter {
    home_dir: PathBuf,
    inner: OnceLock<SharedGitignoreFilter>,
}

impl std::fmt::Debug for LazyGitignoreFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyGitignoreFilter")
            .field("initialized", &self.inner.get().is_some())
            .finish()
    }
}

impl LazyGitignoreFilter {
    pub fn new(home_dir: PathBuf) -> Self {
        Self {
            home_dir,
            inner: OnceLock::new(),
        }
    }

    pub fn get(&self) -> SharedGitignoreFilter {
        self.inner
            .get_or_init(|| {
                eprintln!("[gitignore] building filter (lazy init)...");
                let started = std::time::Instant::now();
                let filter = build_shared_filter(&self.home_dir);
                eprintln!("[gitignore] filter built in {}ms", started.elapsed().as_millis());
                filter
            })
            .clone()
    }
}
