use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use crate::query::SearchMode;
use crate::{perf_log, EntryDto};

/// Compact entry without redundant `path` field (path = dir + sep + name).
/// Saves ~104 bytes per entry vs EntryDto.
#[derive(Clone)]
pub struct CompactEntry {
    pub name: String,
    pub dir: String,
    pub is_dir: bool,
    pub ext: Option<String>,
    pub mtime: Option<i64>,
    pub size: Option<i64>,
}

impl CompactEntry {
    pub fn path(&self) -> String {
        format!("{}{}{}", self.dir, std::path::MAIN_SEPARATOR, self.name)
    }

    fn to_entry_dto(&self) -> EntryDto {
        EntryDto {
            path: self.path(),
            name: self.name.clone(),
            dir: self.dir.clone(),
            is_dir: self.is_dir,
            ext: self.ext.clone(),
            mtime: self.mtime,
            size: self.size,
        }
    }
}

/// Pre-indexed in-memory search structure.
/// Built once after MFT scan; all queries use pre-computed data.
pub struct MemIndex {
    entries: Vec<CompactEntry>,
    /// Pre-lowercased name for each entry (same index)
    names_lower: Vec<String>,
    /// Entry indices sorted by names_lower for binary search prefix lookups
    sorted_idx: Vec<u32>,
    /// ext → sorted [entry_idx, ...] (sorted by name_lower)
    ext_map: HashMap<String, Vec<u32>>,
    /// dir_lower → [entry_idx, ...]
    dir_map: HashMap<String, Vec<u32>>,
}

impl fmt::Debug for MemIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemIndex")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl MemIndex {
    pub fn build(entries: Vec<CompactEntry>) -> Self {
        let t0 = Instant::now();
        let n = entries.len();

        let mut names_lower = Vec::with_capacity(n);
        let mut sorted_idx: Vec<u32> = Vec::with_capacity(n);
        let mut ext_map: HashMap<String, Vec<u32>> = HashMap::new();
        let mut dir_map: HashMap<String, Vec<u32>> = HashMap::new();

        for (i, e) in entries.iter().enumerate() {
            let idx = i as u32;
            let nl = e.name.to_lowercase();
            sorted_idx.push(idx);
            names_lower.push(nl);

            if let Some(ref ext) = e.ext {
                ext_map.entry(ext.clone()).or_default().push(idx);
            }

            let dir_lower = e.dir.to_lowercase();
            dir_map.entry(dir_lower).or_default().push(idx);
        }

        sorted_idx.sort_unstable_by(|&a, &b| {
            names_lower[a as usize].cmp(&names_lower[b as usize])
        });

        // Sort ext_map values by name_lower for consistent output
        for idxs in ext_map.values_mut() {
            idxs.sort_unstable_by(|&a, &b| names_lower[a as usize].cmp(&names_lower[b as usize]));
        }

        eprintln!(
            "[mem_index] built: entries={n} ext_keys={} dir_keys={} in {}ms",
            ext_map.len(),
            dir_map.len(),
            t0.elapsed().as_millis(),
        );

        MemIndex {
            entries,
            names_lower,
            sorted_idx,
            ext_map,
            dir_map,
        }
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn entries(&self) -> &[CompactEntry] {
        &self.entries
    }
}

/// Search the in-memory index.
pub fn search_mem_index(
    mem_index: &MemIndex,
    query: &str,
    mode: &SearchMode,
    effective_limit: u32,
    offset: u32,
    sort_by: &str,
    sort_dir: &str,
) -> Vec<EntryDto> {
    let t0 = Instant::now();
    let total_entries = mem_index.entries.len();

    let mode_label = match mode {
        SearchMode::Empty => "empty",
        SearchMode::NameSearch { .. } => "name",
        SearchMode::GlobName { .. } => "glob",
        SearchMode::ExtSearch { .. } => "ext",
        SearchMode::PathSearch { .. } => "path",
    };

    let t_filter = Instant::now();

    // For Empty mode, use sorted_idx directly (no filter/sort needed)
    if matches!(mode, SearchMode::Empty) {
        let start = offset as usize;
        let end = (start + effective_limit as usize).min(mem_index.sorted_idx.len());
        let page: Vec<EntryDto> = if start < mem_index.sorted_idx.len() {
            if sort_by == "mtime" || sort_by == "size" {
                let desc = sort_dir == "desc";
                let is_size = sort_by == "size";
                let mut all_idx: Vec<u32> = (0..total_entries as u32).collect();
                all_idx.sort_unstable_by(|&a, &b| {
                    let oa = if is_size {
                        mem_index.entries[a as usize].size
                    } else {
                        mem_index.entries[a as usize].mtime
                    };
                    let ob = if is_size {
                        mem_index.entries[b as usize].size
                    } else {
                        mem_index.entries[b as usize].mtime
                    };
                    cmp_opt_none_last(oa, ob, desc)
                });
                let end2 = (start + effective_limit as usize).min(all_idx.len());
                all_idx[start..end2]
                    .iter()
                    .map(|&i| mem_index.entries[i as usize].to_entry_dto())
                    .collect()
            } else {
                // Default: name sorted — use sorted_idx directly
                let iter = if sort_dir == "desc" {
                    // Reverse iteration
                    let rstart = mem_index.sorted_idx.len().saturating_sub(start + effective_limit as usize);
                    let rend = mem_index.sorted_idx.len().saturating_sub(start);
                    mem_index.sorted_idx[rstart..rend]
                        .iter()
                        .rev()
                        .map(|&idx| mem_index.entries[idx as usize].to_entry_dto())
                        .collect()
                } else {
                    mem_index.sorted_idx[start..end]
                        .iter()
                        .map(|&idx| mem_index.entries[idx as usize].to_entry_dto())
                        .collect()
                };
                iter
            }
        } else {
            Vec::new()
        };
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
        perf_log(format!(
            "mem_search query={query:?} mode={mode_label} entries={total_entries} \
             matched={total_entries} returned={} total={total_ms:.1}ms",
            page.len(),
        ));
        return page;
    }

    // ExtSearch: values are pre-sorted by name_lower — skip sort, paginate directly
    if let SearchMode::ExtSearch { ext, .. } = &mode {
        let ext_lower = ext.to_lowercase();
        let page = match mem_index.ext_map.get(&ext_lower) {
            Some(idxs) => {
                let start = offset as usize;
                let lim = effective_limit as usize;
                if sort_by == "mtime" || sort_by == "size" {
                    let mut sorted = idxs.clone();
                    let desc = sort_dir == "desc";
                    let is_size = sort_by == "size";
                    let opt_val = |idx: u32| -> Option<i64> {
                        if is_size {
                            mem_index.entries[idx as usize].size
                        } else {
                            mem_index.entries[idx as usize].mtime
                        }
                    };
                    let n = lim.min(sorted.len());
                    if n < sorted.len() {
                        sorted.select_nth_unstable_by(n, |&a, &b| {
                            cmp_opt_none_last(opt_val(a), opt_val(b), desc)
                        });
                        sorted.truncate(n);
                    }
                    sorted.sort_unstable_by(|&a, &b| {
                        cmp_opt_none_last(opt_val(a), opt_val(b), desc)
                    });
                    let end = (start + lim).min(sorted.len());
                    if start < sorted.len() {
                        sorted[start..end].iter()
                            .map(|&i| mem_index.entries[i as usize].to_entry_dto()).collect()
                    } else {
                        Vec::new()
                    }
                } else if sort_dir == "desc" {
                    // Pre-sorted by name asc, need desc — reverse iterate
                    let rstart = idxs.len().saturating_sub(start + lim);
                    let rend = idxs.len().saturating_sub(start);
                    idxs[rstart..rend].iter().rev()
                        .map(|&i| mem_index.entries[i as usize].to_entry_dto()).collect()
                } else {
                    // Pre-sorted by name asc — direct slice
                    let end = (start + lim).min(idxs.len());
                    if start < idxs.len() {
                        idxs[start..end].iter()
                            .map(|&i| mem_index.entries[i as usize].to_entry_dto()).collect()
                    } else {
                        Vec::new()
                    }
                }
            }
            None => Vec::new(),
        };
        let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
        perf_log(format!(
            "mem_search query={query:?} mode={mode_label} entries={total_entries} \
             returned={} total={total_ms:.1}ms (ext fast path)",
            page.len(),
        ));
        return page;
    }

    let mut indices: Vec<u32> = match mode {
        SearchMode::Empty => unreachable!(),
        SearchMode::ExtSearch { .. } => unreachable!(),
        SearchMode::NameSearch { .. } => {
            let q_lower = query.trim().to_lowercase();
            search_by_name_indexed(mem_index, &q_lower, effective_limit)
        }
        SearchMode::GlobName { name_like } => {
            search_by_glob_indexed(mem_index, name_like, effective_limit)
        }
        SearchMode::PathSearch {
            name_like,
            dir_hint,
            ..
        } => {
            search_by_path_indexed(mem_index, dir_hint, name_like, effective_limit)
        }
    };
    let filter_ms = t_filter.elapsed().as_secs_f64() * 1000.0;
    let matched = indices.len();

    let t_sort = Instant::now();
    // Use partial sort when result set is much larger than limit
    let lim = (offset as usize + effective_limit as usize).min(indices.len());
    if indices.len() > lim * 3 {
        partial_sort_indices(mem_index, &mut indices, query, sort_by, sort_dir, lim);
    } else {
        sort_indices(mem_index, &mut indices, query, sort_by, sort_dir);
    }
    let sort_ms = t_sort.elapsed().as_secs_f64() * 1000.0;

    let start = offset as usize;
    let end = (start + effective_limit as usize).min(indices.len());
    let page: Vec<EntryDto> = if start < indices.len() {
        indices[start..end]
            .iter()
            .map(|&i| mem_index.entries[i as usize].to_entry_dto())
            .collect()
    } else {
        Vec::new()
    };
    let returned = page.len();

    let total_ms = t0.elapsed().as_secs_f64() * 1000.0;
    perf_log(format!(
        "mem_search query={query:?} mode={mode_label} entries={total_entries} \
         matched={matched} returned={returned} filter={filter_ms:.1}ms sort={sort_ms:.1}ms total={total_ms:.1}ms",
    ));

    page
}

/// Time budget for linear scan phases (contains, glob full scan, path)
const SCAN_BUDGET_MS: u128 = 30;

fn search_by_name_indexed(mem_index: &MemIndex, q_lower: &str, limit: u32) -> Vec<u32> {
    let cap = limit as usize;

    // Phase 1: exact + prefix via binary search on sorted_idx
    let lo = mem_index
        .sorted_idx
        .partition_point(|&idx| mem_index.names_lower[idx as usize].as_str() < q_lower);

    let mut exact: Vec<u32> = Vec::new();
    let mut i = lo;
    while i < mem_index.sorted_idx.len()
        && mem_index.names_lower[mem_index.sorted_idx[i] as usize] == q_lower
    {
        exact.push(mem_index.sorted_idx[i]);
        i += 1;
    }

    if exact.len() >= cap {
        exact.truncate(cap);
        return exact;
    }

    // Prefix matches: sorted entries from i to upper bound
    let prefix_end_str = increment_string(q_lower);
    let prefix_hi = match &prefix_end_str {
        Some(end_str) => mem_index
            .sorted_idx
            .partition_point(|&idx| {
                mem_index.names_lower[idx as usize].as_str() < end_str.as_str()
            }),
        None => mem_index.sorted_idx.len(),
    };

    let mut prefix: Vec<u32> = Vec::new();
    for j in i..prefix_hi {
        prefix.push(mem_index.sorted_idx[j]);
        if exact.len() + prefix.len() >= cap {
            break;
        }
    }

    if exact.len() + prefix.len() >= cap {
        exact.extend(prefix);
        exact.truncate(cap);
        return exact;
    }

    // Phase 2: contains matches (linear scan with time budget + early exit)
    let remaining = cap - exact.len() - prefix.len();
    let exact_prefix_set: std::collections::HashSet<u32> =
        exact.iter().chain(prefix.iter()).copied().collect();

    let scan_start = Instant::now();
    let mut contains: Vec<u32> = Vec::new();
    for (idx, nl) in mem_index.names_lower.iter().enumerate() {
        let idx = idx as u32;
        if exact_prefix_set.contains(&idx) {
            continue;
        }
        if nl.contains(q_lower) {
            contains.push(idx);
            if contains.len() >= remaining {
                break;
            }
        }
        // Check time budget every 64K entries
        if idx & 0x3FFF == 0 && scan_start.elapsed().as_millis() > SCAN_BUDGET_MS {
            break;
        }
    }

    exact.extend(prefix);
    exact.extend(contains);
    exact
}

fn search_by_glob_indexed(mem_index: &MemIndex, name_like: &str, limit: u32) -> Vec<u32> {
    let pattern = LikePattern::new(name_like);

    // Optimization: if pattern starts with a literal prefix (before first wildcard),
    // use binary search to narrow the range
    if let Some(prefix) = pattern.literal_prefix() {
        if !prefix.is_empty() {
            let lo = mem_index
                .sorted_idx
                .partition_point(|&idx| {
                    mem_index.names_lower[idx as usize].as_str() < prefix.as_str()
                });
            let prefix_end = increment_string(&prefix);
            let hi = match &prefix_end {
                Some(end_str) => mem_index
                    .sorted_idx
                    .partition_point(|&idx| {
                        mem_index.names_lower[idx as usize].as_str() < end_str.as_str()
                    }),
                None => mem_index.sorted_idx.len(),
            };

            let mut results: Vec<u32> = Vec::new();
            for j in lo..hi {
                let idx = mem_index.sorted_idx[j];
                let nl = &mem_index.names_lower[idx as usize];
                if pattern.matches_pre_lowered(nl) {
                    results.push(idx);
                    if results.len() >= limit as usize {
                        break;
                    }
                }
            }
            return results;
        }
    }

    // Fallback: full scan with time budget
    let scan_start = Instant::now();
    let mut results: Vec<u32> = Vec::new();
    for (i, nl) in mem_index.names_lower.iter().enumerate() {
        if pattern.matches_pre_lowered(nl) {
            results.push(i as u32);
        }
        // Check time budget every 64K entries
        if (i as u32) & 0x3FFF == 0 && i > 0 && scan_start.elapsed().as_millis() > SCAN_BUDGET_MS {
            break;
        }
    }
    results
}

fn search_by_path_indexed(
    mem_index: &MemIndex,
    dir_hint: &str,
    name_like: &str,
    limit: u32,
) -> Vec<u32> {
    let sep = std::path::MAIN_SEPARATOR;
    let dir_hint_normalized = dir_hint.replace('/', &sep.to_string()).to_lowercase();
    let dir_suffix = format!("{sep}{dir_hint_normalized}").to_lowercase();
    let dir_infix = format!("{sep}{dir_hint_normalized}{sep}").to_lowercase();

    let scan_start = Instant::now();
    let collect_cap = (limit as usize) * 30; // Cap collection at 30x limit
    let mut matching_indices: Vec<u32> = Vec::new();
    for (dir_lower, idxs) in &mem_index.dir_map {
        if dir_lower.ends_with(&dir_suffix) || dir_lower.contains(&dir_infix) {
            matching_indices.extend_from_slice(idxs);
            if matching_indices.len() >= collect_cap {
                break;
            }
        }
        // Time budget for dir_map scan
        if scan_start.elapsed().as_millis() > SCAN_BUDGET_MS {
            break;
        }
    }

    if name_like == "%" {
        return matching_indices;
    }

    let pattern = LikePattern::new(name_like);
    matching_indices.retain(|&idx| {
        pattern.matches_pre_lowered(&mem_index.names_lower[idx as usize])
    });
    matching_indices
}

/// Increment the last character of a string to get the exclusive upper bound.
fn increment_string(s: &str) -> Option<String> {
    let mut chars: Vec<char> = s.chars().collect();
    for i in (0..chars.len()).rev() {
        if let Some(next_char) = char::from_u32(chars[i] as u32 + 1) {
            chars[i] = next_char;
            return Some(chars[..=i].iter().collect());
        }
    }
    None
}

fn sort_indices(
    mem_index: &MemIndex,
    indices: &mut [u32],
    query: &str,
    sort_by: &str,
    sort_dir: &str,
) {
    if sort_by == "name" && !query.is_empty() {
        let q_lower = query.trim().to_lowercase();
        indices.sort_unstable_by(|&a, &b| {
            let ra = relevance_rank_idx(mem_index, a, &q_lower);
            let rb = relevance_rank_idx(mem_index, b, &q_lower);
            if ra != rb {
                return ra.cmp(&rb);
            }
            if ra <= 3 {
                let da = path_depth(&mem_index.entries[a as usize].dir);
                let db = path_depth(&mem_index.entries[b as usize].dir);
                if da != db {
                    return da.cmp(&db);
                }
            }
            name_cmp_idx(mem_index, a, b, sort_dir)
        });
    } else {
        match sort_by {
            "mtime" | "size" => {
                let desc = sort_dir == "desc";
                let is_size = sort_by == "size";
                indices.sort_unstable_by(|&a, &b| {
                    let oa = if is_size {
                        mem_index.entries[a as usize].size
                    } else {
                        mem_index.entries[a as usize].mtime
                    };
                    let ob = if is_size {
                        mem_index.entries[b as usize].size
                    } else {
                        mem_index.entries[b as usize].mtime
                    };
                    cmp_opt_none_last(oa, ob, desc)
                });
            }
            _ => {
                indices.sort_unstable_by(|&a, &b| name_cmp_idx(mem_index, a, b, sort_dir));
            }
        }
    }
}

fn partial_sort_indices(
    mem_index: &MemIndex,
    indices: &mut Vec<u32>,
    query: &str,
    sort_by: &str,
    sort_dir: &str,
    k: usize,
) {
    if indices.is_empty() || k == 0 {
        return;
    }
    let k = k.min(indices.len());

    if sort_by == "name" && !query.is_empty() {
        let q_lower = query.trim().to_lowercase();
        let cmp = |a: &u32, b: &u32| {
            let ra = relevance_rank_idx(mem_index, *a, &q_lower);
            let rb = relevance_rank_idx(mem_index, *b, &q_lower);
            if ra != rb {
                return ra.cmp(&rb);
            }
            if ra <= 3 {
                let da = path_depth(&mem_index.entries[*a as usize].dir);
                let db = path_depth(&mem_index.entries[*b as usize].dir);
                if da != db {
                    return da.cmp(&db);
                }
            }
            name_cmp_idx(mem_index, *a, *b, sort_dir)
        };
        indices.select_nth_unstable_by(k - 1, cmp);
        indices.truncate(k);
        indices.sort_unstable_by(cmp);
    } else {
        match sort_by {
            "mtime" | "size" => {
                let desc = sort_dir == "desc";
                let is_size = sort_by == "size";
                let cmp = |a: &u32, b: &u32| {
                    let oa = if is_size {
                        mem_index.entries[*a as usize].size
                    } else {
                        mem_index.entries[*a as usize].mtime
                    };
                    let ob = if is_size {
                        mem_index.entries[*b as usize].size
                    } else {
                        mem_index.entries[*b as usize].mtime
                    };
                    cmp_opt_none_last(oa, ob, desc)
                };
                indices.select_nth_unstable_by(k - 1, cmp);
                indices.truncate(k);
                indices.sort_unstable_by(cmp);
            }
            _ => {
                let cmp = |a: &u32, b: &u32| name_cmp_idx(mem_index, *a, *b, sort_dir);
                indices.select_nth_unstable_by(k - 1, cmp);
                indices.truncate(k);
                indices.sort_unstable_by(cmp);
            }
        }
    }
}

/// Compare two Option<i64> values, pushing None to the end regardless of sort direction.
fn cmp_opt_none_last(a: Option<i64>, b: Option<i64>, desc: bool) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,  // None goes last
        (Some(_), None) => std::cmp::Ordering::Less,     // Some goes first
        (Some(va), Some(vb)) => if desc { vb.cmp(&va) } else { va.cmp(&vb) },
    }
}

fn name_cmp_idx(mem_index: &MemIndex, a: u32, b: u32, sort_dir: &str) -> std::cmp::Ordering {
    let na = &mem_index.names_lower[a as usize];
    let nb = &mem_index.names_lower[b as usize];
    if sort_dir == "desc" { nb.cmp(na) } else { na.cmp(nb) }
}

fn relevance_rank_idx(mem_index: &MemIndex, idx: u32, q_lower: &str) -> u8 {
    let name_lower = &mem_index.names_lower[idx as usize];
    if name_lower == q_lower {
        return 1;
    }
    let e = &mem_index.entries[idx as usize];
    if let Some(dot_pos) = e.name.rfind('.') {
        if e.name[..dot_pos].to_lowercase() == *q_lower {
            return 2;
        }
    }
    if name_lower.starts_with(q_lower) {
        return 3;
    }
    if name_lower.contains(q_lower) {
        return 5;
    }
    9
}

fn path_depth(path: &str) -> usize {
    path.chars().filter(|&c| c == '/' || c == '\\').count()
}

/// Simple SQL LIKE pattern matcher with backslash escape.
struct LikePattern {
    segments: Vec<LikeSegment>,
}

enum LikeSegment {
    Literal(String), // already lowercased
    SingleChar,
    AnyChars,
}

impl LikePattern {
    fn new(pattern: &str) -> Self {
        let mut segments = Vec::new();
        let mut chars = pattern.chars().peekable();
        let mut literal = String::new();

        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        literal.push(escaped);
                    }
                }
                '%' => {
                    if !literal.is_empty() {
                        segments.push(LikeSegment::Literal(
                            std::mem::take(&mut literal).to_lowercase(),
                        ));
                    }
                    while chars.peek() == Some(&'%') {
                        chars.next();
                    }
                    segments.push(LikeSegment::AnyChars);
                }
                '_' => {
                    if !literal.is_empty() {
                        segments.push(LikeSegment::Literal(
                            std::mem::take(&mut literal).to_lowercase(),
                        ));
                    }
                    segments.push(LikeSegment::SingleChar);
                }
                _ => literal.push(ch),
            }
        }
        if !literal.is_empty() {
            segments.push(LikeSegment::Literal(literal.to_lowercase()));
        }

        LikePattern { segments }
    }

    /// Extract leading literal prefix before first wildcard (for binary search optimization)
    fn literal_prefix(&self) -> Option<String> {
        match self.segments.first() {
            Some(LikeSegment::Literal(lit)) => Some(lit.clone()),
            _ => None,
        }
    }

    /// Match against a value that is already lowercased.
    fn matches_pre_lowered(&self, value: &str) -> bool {
        like_match(&self.segments, value, 0, 0)
    }

    #[cfg(test)]
    fn matches(&self, value: &str) -> bool {
        like_match(&self.segments, &value.to_lowercase(), 0, 0)
    }
}

fn like_match(segments: &[LikeSegment], value: &str, seg_idx: usize, val_pos: usize) -> bool {
    if seg_idx >= segments.len() {
        return val_pos >= value.len();
    }

    let remaining = &value[val_pos..];

    match &segments[seg_idx] {
        LikeSegment::Literal(lit) => {
            if !remaining.starts_with(lit.as_str()) {
                return false;
            }
            like_match(segments, value, seg_idx + 1, val_pos + lit.len())
        }
        LikeSegment::SingleChar => {
            if remaining.is_empty() {
                return false;
            }
            let char_len = remaining.chars().next().unwrap().len_utf8();
            like_match(segments, value, seg_idx + 1, val_pos + char_len)
        }
        LikeSegment::AnyChars => {
            let next_seg = seg_idx + 1;
            if next_seg >= segments.len() {
                return true;
            }
            let mut pos = val_pos;
            if like_match(segments, value, next_seg, pos) {
                return true;
            }
            for ch in remaining.chars() {
                pos += ch.len_utf8();
                if like_match(segments, value, next_seg, pos) {
                    return true;
                }
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, value: &str) -> bool {
        LikePattern::new(pattern).matches(value)
    }

    #[test]
    fn literal_match() {
        assert!(matches("hello", "hello"));
        assert!(matches("hello", "Hello"));
        assert!(!matches("hello", "world"));
    }

    #[test]
    fn percent_wildcard() {
        assert!(matches("%hello%", "say hello world"));
        assert!(matches("hello%", "hello world"));
        assert!(matches("%world", "hello world"));
        assert!(!matches("%xyz%", "hello world"));
    }

    #[test]
    fn underscore_wildcard() {
        assert!(matches("h_llo", "hello"));
        assert!(matches("h_llo", "hallo"));
        assert!(!matches("h_llo", "hllo"));
    }

    #[test]
    fn escaped_percent() {
        assert!(matches("100\\%", "100%"));
        assert!(!matches("100\\%", "100abc"));
    }

    #[test]
    fn escaped_underscore() {
        assert!(matches("a\\_b", "a_b"));
        assert!(!matches("a\\_b", "axb"));
    }

    #[test]
    fn glob_like_pattern() {
        assert!(matches("%.png", "image.png"));
        assert!(matches("%.png", "foo.PNG"));
        assert!(!matches("%.png", "image.jpg"));
    }

    #[test]
    fn ext_glob() {
        assert!(matches("test%.png", "test_file.png"));
        assert!(!matches("test%.png", "other.png"));
    }

    #[test]
    fn korean_match() {
        assert!(matches("%문서%", "공증 문서 스캔"));
        assert!(matches("%영등포%", "251021 영등포1의2 임시총회공고"));
    }

    #[test]
    fn literal_prefix_extraction() {
        assert_eq!(LikePattern::new("test%").literal_prefix(), Some("test".to_string()));
        assert_eq!(LikePattern::new("%test").literal_prefix(), None);
        assert_eq!(LikePattern::new("hello").literal_prefix(), Some("hello".to_string()));
    }
}
