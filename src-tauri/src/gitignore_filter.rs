use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;
use std::sync::Arc;

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

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        for gi in &self.matchers {
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

pub type SharedGitignoreFilter = Arc<GitignoreFilter>;

pub fn build_shared_filter(home_dir: &Path) -> SharedGitignoreFilter {
    Arc::new(GitignoreFilter::build(home_dir))
}
