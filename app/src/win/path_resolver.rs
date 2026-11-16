use std::collections::{HashMap, HashSet, VecDeque};

/// Resolves MFT File Reference Numbers (FRN) to full file system paths.
///
/// Only directory records should be added (files resolve via parent dir path + name).
pub struct PathResolver {
    drive_prefix: String,
    // FRN → (parent_frn, name) — directories only
    frn_map: HashMap<u64, (u64, String)>,
    // parent_frn → [child_dir_frn, ...] — directory-to-directory edges only
    children_map: HashMap<u64, Vec<u64>>,
    // FRN → resolved full path
    path_cache: HashMap<u64, String>,
}

// Root directory FRN on NTFS is always 5 (lower 48 bits)
const NTFS_ROOT_FRN: u64 = 5;

impl PathResolver {
    #[allow(dead_code)]
    pub fn new(drive_prefix: &str) -> Self {
        Self {
            drive_prefix: drive_prefix.trim_end_matches('\\').to_string(),
            frn_map: HashMap::new(),
            children_map: HashMap::new(),
            path_cache: HashMap::new(),
        }
    }

    pub fn with_capacity(drive_prefix: &str, capacity: usize) -> Self {
        Self {
            drive_prefix: drive_prefix.trim_end_matches('\\').to_string(),
            frn_map: HashMap::with_capacity(capacity),
            children_map: HashMap::with_capacity(capacity / 4),
            path_cache: HashMap::with_capacity(capacity),
        }
    }

    /// Add a directory record to the resolver (Pass 1).
    pub fn add_record(&mut self, frn: u64, parent_frn: u64, name: String) {
        self.frn_map.insert(frn, (parent_frn, name));
        self.children_map
            .entry(parent_frn)
            .or_default()
            .push(frn);
    }

    /// Find the FRN for a directory path like `C:\Users\USER` by walking
    /// path segments from the NTFS root. Case-insensitive matching.
    /// Returns None if the path doesn't exist in the FRN map.
    pub fn find_frn_by_path(&self, path: &str) -> Option<u64> {
        let stripped = path
            .strip_prefix(&self.drive_prefix)
            .unwrap_or(path)
            .trim_start_matches('\\');

        if stripped.is_empty() {
            return Some(NTFS_ROOT_FRN);
        }

        let segments: Vec<&str> = stripped.split('\\').filter(|s| !s.is_empty()).collect();
        let mut current_frn = NTFS_ROOT_FRN;

        for segment in &segments {
            let segment_lower = segment.to_lowercase();
            let children = self.children_map.get(&current_frn)?;
            let mut found = false;

            for &child_frn in children {
                if let Some((_, name)) = self.frn_map.get(&child_frn) {
                    if name.to_lowercase() == segment_lower {
                        current_frn = child_frn;
                        found = true;
                        break;
                    }
                }
            }

            if !found {
                return None;
            }
        }

        Some(current_frn)
    }

    /// Collect all FRNs that are descendants of the given FRN (BFS).
    /// Returns a set containing `root_frn` and all its descendants.
    #[allow(dead_code)]
    pub fn collect_subtree(&self, root_frn: u64) -> HashSet<u64> {
        let mut result = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(root_frn);

        while let Some(frn) = queue.pop_front() {
            result.insert(frn);
            if let Some(children) = self.children_map.get(&frn) {
                for &child in children {
                    if !result.contains(&child) {
                        queue.push_back(child);
                    }
                }
            }
        }

        result
    }

    /// Collect directory FRNs under root_frn via BFS, pruning entire subtrees
    /// whose directory name matches any of the skip_names.
    pub fn collect_subtree_pruned(
        &self,
        root_frn: u64,
        skip_names: &[&str],
        skip_frns: &HashSet<u64>,
    ) -> HashSet<u64> {
        let mut result = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(root_frn);

        while let Some(frn) = queue.pop_front() {
            // Prune directories by FRN (pathignore roots) or name (but never prune the root itself)
            if frn != root_frn {
                if skip_frns.contains(&frn) {
                    continue;
                }
                if let Some((_, name)) = self.frn_map.get(&frn) {
                    if skip_names.iter().any(|&skip| name == skip) {
                        continue;
                    }
                }
            }

            result.insert(frn);
            if let Some(children) = self.children_map.get(&frn) {
                for &child in children {
                    if !result.contains(&child) {
                        queue.push_back(child);
                    }
                }
            }
        }

        result
    }

    /// Resolve a FRN to its full file system path.
    /// Returns None if the parent chain is broken or leads to a cycle.
    pub fn resolve(&mut self, frn: u64) -> Option<String> {
        if let Some(cached) = self.path_cache.get(&frn) {
            return Some(cached.clone());
        }

        let mut chain: Vec<(u64, String)> = Vec::new();
        let mut current = frn;
        let mut visited = Vec::new();

        loop {
            if current == NTFS_ROOT_FRN {
                break;
            }

            if visited.contains(&current) {
                return None;
            }
            visited.push(current);

            if let Some(cached) = self.path_cache.get(&current) {
                let mut path = cached.clone();
                for (_, name) in chain.iter().rev() {
                    path.push('\\');
                    path.push_str(name);
                }
                self.path_cache.insert(frn, path.clone());
                return Some(path);
            }

            match self.frn_map.get(&current) {
                Some((parent_frn, name)) => {
                    chain.push((current, name.clone()));
                    current = *parent_frn;
                }
                None => {
                    return None;
                }
            }
        }

        let mut path = self.drive_prefix.clone();
        for (intermediate_frn, name) in chain.iter().rev() {
            path.push('\\');
            path.push_str(name);
            self.path_cache.insert(*intermediate_frn, path.clone());
        }

        Some(path)
    }

    /// Free the children_map after subtree collection is done (no longer needed).
    pub fn drop_children_map(&mut self) {
        self.children_map.clear();
        self.children_map.shrink_to_fit();
    }

    /// Free the frn_map after all needed paths are pre-resolved into path_cache.
    pub fn drop_frn_map(&mut self) {
        self.frn_map.clear();
        self.frn_map.shrink_to_fit();
    }

    /// Read-only access to the resolved path cache (FRN → full path).
    pub fn path_cache(&self) -> &HashMap<u64, String> {
        &self.path_cache
    }

    /// Consume the resolver and return only the path_cache (FRN → full path).
    /// Drops the heavier frn_map while keeping the resolved paths for USN watcher.
    pub fn into_path_cache(self) -> HashMap<u64, String> {
        self.path_cache
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_simple_chain() {
        let mut r = PathResolver::new("C:\\");
        r.add_record(100, NTFS_ROOT_FRN, "Users".to_string());
        r.add_record(200, 100, "user".to_string());
        // File — would NOT be in resolver in production, but resolve still works
        r.add_record(300, 200, "file.txt".to_string());

        assert_eq!(r.resolve(300), Some("C:\\Users\\user\\file.txt".to_string()));
        assert_eq!(r.resolve(200), Some("C:\\Users\\user".to_string()));
        assert_eq!(r.resolve(100), Some("C:\\Users".to_string()));
    }

    #[test]
    fn resolve_broken_chain_returns_none() {
        let mut r = PathResolver::new("C:\\");
        r.add_record(300, 999, "orphan.txt".to_string());
        assert_eq!(r.resolve(300), None);
    }

    #[test]
    fn resolve_uses_cache() {
        let mut r = PathResolver::new("C:\\");
        r.add_record(100, NTFS_ROOT_FRN, "Users".to_string());
        r.add_record(200, 100, "user".to_string());

        let _ = r.resolve(200);

        r.add_record(300, 200, "docs".to_string());
        r.add_record(400, 300, "file.txt".to_string());

        assert_eq!(
            r.resolve(400),
            Some("C:\\Users\\user\\docs\\file.txt".to_string())
        );
    }

    #[test]
    fn find_frn_by_path_works() {
        let mut r = PathResolver::new("C:");
        r.add_record(100, NTFS_ROOT_FRN, "Users".to_string());
        r.add_record(200, 100, "TestUser".to_string());
        r.add_record(300, 200, "Documents".to_string());

        assert_eq!(r.find_frn_by_path("C:\\Users\\TestUser"), Some(200));
        assert_eq!(r.find_frn_by_path("C:\\users\\testuser"), Some(200)); // case insensitive
        assert_eq!(r.find_frn_by_path("C:\\NoSuchDir"), None);
    }

    #[test]
    fn collect_subtree_works() {
        let mut r = PathResolver::new("C:");
        r.add_record(100, NTFS_ROOT_FRN, "Users".to_string());
        r.add_record(200, 100, "user".to_string());
        r.add_record(300, 200, "docs".to_string());
        r.add_record(400, 200, "pics".to_string());
        r.add_record(500, NTFS_ROOT_FRN, "Windows".to_string());
        r.add_record(600, 500, "System32".to_string());

        let subtree = r.collect_subtree(200);
        assert!(subtree.contains(&200));
        assert!(subtree.contains(&300));
        assert!(subtree.contains(&400));
        assert!(!subtree.contains(&100)); // parent, not descendant
        assert!(!subtree.contains(&500)); // different branch
        assert!(!subtree.contains(&600));
    }

    #[test]
    fn collect_subtree_pruned_skips_named_dirs() {
        let mut r = PathResolver::new("C:");
        r.add_record(100, NTFS_ROOT_FRN, "Users".to_string());
        r.add_record(200, 100, "user".to_string());
        r.add_record(300, 200, "project".to_string());
        r.add_record(400, 300, "node_modules".to_string());
        r.add_record(500, 400, "lodash".to_string()); // deep inside node_modules
        r.add_record(600, 300, "src".to_string());
        r.add_record(700, 200, ".git".to_string());
        r.add_record(800, 700, "objects".to_string());

        let subtree = r.collect_subtree_pruned(200, &["node_modules", ".git"], &HashSet::new());
        assert!(subtree.contains(&200));  // root
        assert!(subtree.contains(&300));  // project
        assert!(!subtree.contains(&400)); // node_modules — pruned
        assert!(!subtree.contains(&500)); // lodash — pruned (child of node_modules)
        assert!(subtree.contains(&600));  // src — kept
        assert!(!subtree.contains(&700)); // .git — pruned
        assert!(!subtree.contains(&800)); // objects — pruned (child of .git)
    }
}
