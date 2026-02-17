use std::{
    cmp::Ordering,
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use jwalk::WalkDir;

use crate::{should_skip_path, EntryDto, IgnorePattern};

#[derive(Debug)]
pub struct FdSearchCache {
    pub query: String,
    pub sort_by: String,
    pub sort_dir: String,
    pub ignore_fingerprint: u64,
    pub entries: Vec<EntryDto>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FdSearchResultDto {
    pub entries: Vec<EntryDto>,
    pub total: u64,
    pub timed_out: bool,
}

const MAX_COLLECT: usize = 5_000;
const MAX_DEPTH: usize = 15;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
fn num_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

enum QueryMode {
    NameTerms(Vec<Vec<u8>>),
    GlobName(String),
    PathSearch {
        dir_lower: Vec<u8>,
        name_mode: NameMatch,
    },
}

enum NameMatch {
    Any,
    Terms(Vec<Vec<u8>>),
    Glob(String),
}

fn parse_live_query(query: &str) -> QueryMode {
    let trimmed = query.trim();

    if trimmed.contains('/') {
        let last_slash = trimmed.rfind('/').unwrap();
        let dir_part = trimmed[..last_slash].trim().to_lowercase();
        let name_part = trimmed[last_slash + 1..].trim();

        let name_mode = if name_part.is_empty() {
            NameMatch::Any
        } else if name_part.contains('*') || name_part.contains('?') {
            NameMatch::Glob(name_part.to_lowercase())
        } else {
            NameMatch::Terms(
                name_part
                    .to_lowercase()
                    .split_whitespace()
                    .map(|s| s.as_bytes().to_vec())
                    .collect(),
            )
        };

        let dir_lower = if dir_part.is_empty() {
            Vec::new()
        } else {
            format!("/{}/", dir_part).into_bytes()
        };

        return QueryMode::PathSearch {
            dir_lower,
            name_mode,
        };
    }

    if trimmed.contains('*') || trimmed.contains('?') {
        return QueryMode::GlobName(trimmed.to_lowercase());
    }

    QueryMode::NameTerms(
        trimmed
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.as_bytes().to_vec())
            .collect(),
    )
}

#[cfg(unix)]
fn ascii_icontains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    let end = haystack.len() - needle.len();
    'outer: for i in 0..=end {
        for j in 0..needle.len() {
            if haystack[i + j].to_ascii_lowercase() != needle[j] {
                continue 'outer;
            }
        }
        return true;
    }
    false
}

#[cfg(not(unix))]
fn ascii_icontains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = String::from_utf8_lossy(haystack).to_lowercase();
    let n = String::from_utf8_lossy(needle);
    h.contains(n.as_ref())
}

fn ascii_iends_with(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    let start = haystack.len() - needle.len();
    for i in 0..needle.len() {
        if haystack[start + i].to_ascii_lowercase() != needle[i] {
            return false;
        }
    }
    true
}

fn matches_name_terms(name: &[u8], terms: &[Vec<u8>]) -> bool {
    terms.iter().all(|t| ascii_icontains(name, t))
}

fn matches_query_fast(mode: &QueryMode, name: &[u8], dir: Option<&[u8]>) -> bool {
    match mode {
        QueryMode::NameTerms(terms) => matches_name_terms(name, terms),
        QueryMode::GlobName(pattern) => {
            let name_str = String::from_utf8_lossy(name).to_lowercase();
            glob_matches(pattern, &name_str)
        }
        QueryMode::PathSearch {
            dir_lower,
            name_mode,
        } => {
            if !dir_lower.is_empty() {
                let d = dir.unwrap_or(&[]);
                let without_trailing = &dir_lower[..dir_lower.len() - 1];
                if !ascii_icontains(d, dir_lower) && !ascii_iends_with(d, without_trailing) {
                    return false;
                }
            }
            match name_mode {
                NameMatch::Any => true,
                NameMatch::Terms(terms) => matches_name_terms(name, terms),
                NameMatch::Glob(pattern) => {
                    let name_str = String::from_utf8_lossy(name).to_lowercase();
                    glob_matches(pattern, &name_str)
                }
            }
        }
    }
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (pn, tn) = (p.len(), t.len());

    let mut px = 0usize;
    let mut tx = 0usize;
    let mut star_px = usize::MAX;
    let mut star_tx = 0usize;

    while tx < tn {
        if px < pn && (p[px] == '?' || p[px] == t[tx]) {
            px += 1;
            tx += 1;
        } else if px < pn && p[px] == '*' {
            star_px = px;
            star_tx = tx;
            px += 1;
        } else if star_px != usize::MAX {
            px = star_px + 1;
            star_tx += 1;
            tx = star_tx;
        } else {
            return false;
        }
    }

    while px < pn && p[px] == '*' {
        px += 1;
    }

    px == pn
}

// Simplified relevance ranking (0-3) for live filesystem search results.
// Differs from the main.rs version (0-5) which includes path-aware ranking
// for DB/indexed results (e.g., directory depth, path component matches).
fn relevance_rank(entry: &EntryDto, query_lower: &str) -> u8 {
    let name_lower = entry.name.to_lowercase();
    if name_lower == query_lower {
        return 0;
    }
    if name_lower.starts_with(query_lower) {
        return 1;
    }
    if name_lower.contains(query_lower) {
        return 2;
    }
    3
}

fn find_child_dir_icase(parent: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(parent).ok()?;
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(name)
        {
            let path = entry.path();
            if path.is_dir() {
                return Some(path);
            }
        }
    }
    None
}

pub struct LiveSearchResult {
    pub entries: Vec<EntryDto>,
    pub timed_out: bool,
}

pub fn run_fd_search(
    home_dir: &Path,
    ignored_roots: &[PathBuf],
    ignored_patterns: &[IgnorePattern],
    query: &str,
    sort_by: &str,
    sort_dir: &str,
) -> LiveSearchResult {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return LiveSearchResult {
            entries: Vec::new(),
            timed_out: false,
        };
    }

    let mode = parse_live_query(trimmed);

    let search_root: PathBuf = if let QueryMode::PathSearch { ref dir_lower, .. } = mode {
        let dir_str = String::from_utf8_lossy(dir_lower);
        let segment = dir_str.trim_matches('/').split('/').next().unwrap_or("");
        if !segment.is_empty() {
            find_child_dir_icase(home_dir, segment).unwrap_or_else(|| home_dir.to_path_buf())
        } else {
            home_dir.to_path_buf()
        }
    } else {
        home_dir.to_path_buf()
    };

    let needs_dir = matches!(mode, QueryMode::PathSearch { .. });
    let deadline = Instant::now() + SEARCH_TIMEOUT;

    let ignored = ignored_roots.to_vec();
    let ignored_patterns = ignored_patterns.to_vec();
    let walker = WalkDir::new(&search_root)
        .follow_links(false)
        .skip_hidden(false)
        .max_depth(MAX_DEPTH)
        .parallelism(jwalk::Parallelism::RayonNewPool(num_threads()))
        .process_read_dir(move |_depth, path, _state, children| {
            children.retain(|entry_result| {
                entry_result
                    .as_ref()
                    .map(|entry| {
                        let full_path = path.join(&entry.file_name);
                        !should_skip_path(&full_path, &ignored, &ignored_patterns)
                    })
                    .unwrap_or(false)
            });
        });

    let mut entries = Vec::with_capacity(1024);
    let mut timed_out = false;
    let mut count = 0u32;

    for result in walker {
        count += 1;
        if count % 8192 == 0 && Instant::now() >= deadline {
            timed_out = true;
            break;
        }

        let Ok(dir_entry) = result else { continue };

        #[cfg(unix)]
        let name = dir_entry.file_name().as_bytes();
        #[cfg(not(unix))]
        let name = dir_entry.file_name().to_string_lossy().as_bytes().to_vec();
        #[cfg(not(unix))]
        let name = name.as_slice();

        let dir_bytes: Option<Vec<u8>> = if needs_dir {
            Some(
                dir_entry
                    .parent_path()
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec(),
            )
        } else {
            None
        };

        if !matches_query_fast(&mode, name, dir_bytes.as_deref()) {
            continue;
        }

        let path = dir_entry.path();
        let is_dir = dir_entry.file_type().is_dir();

        let ext = if is_dir {
            None
        } else {
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
        };

        let mtime = dir_entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        let name_string = dir_entry.file_name().to_string_lossy().to_string();
        let dir_string = dir_bytes
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_else(|| dir_entry.parent_path().to_string_lossy().to_string());

        entries.push(EntryDto {
            path: path.to_string_lossy().to_string(),
            name: name_string,
            dir: dir_string,
            is_dir,
            ext,
            mtime,
        });

        if entries.len() >= MAX_COLLECT {
            break;
        }
    }

    let query_lower = trimmed.to_lowercase();
    sort_by_relevance(&mut entries, &query_lower, sort_by, sort_dir);

    LiveSearchResult { entries, timed_out }
}

fn sort_by_relevance(entries: &mut [EntryDto], query_lower: &str, sort_by: &str, sort_dir: &str) {
    entries.sort_by(|a, b| {
        let rank_cmp = relevance_rank(a, query_lower).cmp(&relevance_rank(b, query_lower));
        if rank_cmp != Ordering::Equal {
            return rank_cmp;
        }
        match sort_by {
            "mtime" => {
                let lhs = a.mtime.unwrap_or(0);
                let rhs = b.mtime.unwrap_or(0);
                let primary = if sort_dir == "desc" {
                    rhs.cmp(&lhs)
                } else {
                    lhs.cmp(&rhs)
                };
                primary.then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            }
            "dir" => {
                let primary = if sort_dir == "desc" {
                    b.dir.to_lowercase().cmp(&a.dir.to_lowercase())
                } else {
                    a.dir.to_lowercase().cmp(&b.dir.to_lowercase())
                };
                primary.then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            }
            _ => {
                let primary = if sort_dir == "desc" {
                    b.name.to_lowercase().cmp(&a.name.to_lowercase())
                } else {
                    a.name.to_lowercase().cmp(&b.name.to_lowercase())
                };
                primary.then(a.path.to_lowercase().cmp(&b.path.to_lowercase()))
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_star() {
        assert!(glob_matches("*.md", "readme.md"));
        assert!(!glob_matches("*.md", "readme.txt"));
        assert!(glob_matches("test*", "testing"));
        assert!(glob_matches("*test*", "my_testing_file"));
    }

    #[test]
    fn glob_matches_question() {
        assert!(glob_matches("?.txt", "a.txt"));
        assert!(!glob_matches("?.txt", "ab.txt"));
    }

    #[test]
    fn glob_matches_combined() {
        assert!(glob_matches("t*t?.md", "test1.md"));
        assert!(!glob_matches("t*t?.md", "test.md"));
    }

    #[test]
    fn glob_matches_exact() {
        assert!(glob_matches("hello", "hello"));
        assert!(!glob_matches("hello", "helloworld"));
    }

    #[test]
    fn ascii_icontains_basic() {
        assert!(ascii_icontains(b"Hello_World", b"hello"));
        assert!(ascii_icontains(b"a_DESKTOP", b"a_desktop"));
        assert!(!ascii_icontains(b"desktop", b"a_desktop"));
        assert!(ascii_icontains(b"anything", b""));
    }

    #[test]
    fn query_mode_plain_text_bytes() {
        let mode = parse_live_query("a_desktop");
        assert!(matches!(mode, QueryMode::NameTerms(_)));
        assert!(matches_query_fast(&mode, b"a_desktop", None));
        assert!(matches_query_fast(&mode, b"my_a_desktop_file", None));
        assert!(!matches_query_fast(&mode, b"desktop", None));
        assert!(matches_query_fast(&mode, b"A_DESKTOP", None));
    }

    #[test]
    fn query_mode_multi_word_bytes() {
        let mode = parse_live_query("foo bar");
        assert!(matches_query_fast(&mode, b"foobar", None));
        assert!(matches_query_fast(&mode, b"bar_foo_baz", None));
        assert!(!matches_query_fast(&mode, b"foo", None));
    }

    #[test]
    fn query_mode_glob_bytes() {
        let mode = parse_live_query("*.md");
        assert!(matches_query_fast(&mode, b"readme.md", None));
        assert!(!matches_query_fast(&mode, b"readme.txt", None));
    }

    #[test]
    fn query_mode_path_search_bytes() {
        let mode = parse_live_query("desktop/*.png");
        assert!(matches_query_fast(
            &mode,
            b"photo.png",
            Some(b"/users/al/desktop")
        ));
        assert!(!matches_query_fast(
            &mode,
            b"photo.png",
            Some(b"/users/al/documents")
        ));
        assert!(!matches_query_fast(
            &mode,
            b"photo.jpg",
            Some(b"/users/al/desktop")
        ));
    }

    #[test]
    fn path_search_segment_boundary() {
        let mode = parse_live_query("a_desktop/ *.png");

        // direct child of a_desktop
        assert!(matches_query_fast(
            &mode,
            b"photo.png",
            Some(b"/Users/al02402336/a_desktop")
        ));
        // nested subdirectory
        assert!(matches_query_fast(
            &mode,
            b"shot.png",
            Some(b"/Users/al02402336/a_desktop/sub")
        ));
        // false positive: a_desktop_stuff should NOT match
        assert!(!matches_query_fast(
            &mode,
            b"photo.png",
            Some(b"/Users/al02402336/a_desktop_stuff")
        ));
        // false positive: not_a_desktop should NOT match
        assert!(!matches_query_fast(
            &mode,
            b"photo.png",
            Some(b"/Users/al02402336/not_a_desktop")
        ));
        // wrong extension
        assert!(!matches_query_fast(
            &mode,
            b"photo.jpg",
            Some(b"/Users/al02402336/a_desktop")
        ));
    }

    #[test]
    fn query_mode_path_search_name_terms_bytes() {
        let mode = parse_live_query("src/main");
        assert!(matches_query_fast(
            &mode,
            b"main.rs",
            Some(b"/home/user/src")
        ));
        assert!(!matches_query_fast(
            &mode,
            b"lib.rs",
            Some(b"/home/user/src")
        ));
        assert!(!matches_query_fast(
            &mode,
            b"main.rs",
            Some(b"/home/user/docs")
        ));
    }
}
