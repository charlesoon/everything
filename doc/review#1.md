# Code Review Report

**Project**: Everything — macOS desktop file search (Tauri v2 + Rust + Svelte)
**Scope**: All uncommitted changes (`git diff HEAD`)
**Files changed**: `src-tauri/src/main.rs`, `src/App.svelte`
**Build**: Frontend compiles clean, zero warnings

---

## Major Issues ⚠️

### 1. Infinite live search re-trigger cycle

**Files**: `src-tauri/src/main.rs:1220-1229` + `src/App.svelte:678-682`

The combination of these two changes creates an infinite loop of filesystem scans when DB results are insufficient:

**Trace:**
1. User types query → `runSearch()` → backend `search()` returns 50 DB results
2. `50 < 300` (effective_limit) → `start_live_search_worker` spawns `rg --files -uu /` scan
3. Worker finishes → caches 150 results → removes query from `inflight` → emits `live_search_updated`
4. Frontend listener fires → query matches → calls `runSearch()` again
5. Backend `search()`: DB still returns 50 → `50 < 300` → starts **another** live worker (inflight was cleared in step 3)
6. Cache exists → returns merged ~200 results to frontend (correct data)
7. Worker finishes again → caches same 150 → emits event → go to step 4

The inflight guard only prevents concurrent runs, not re-runs after completion. The condition `results.len() < effective_limit` checks **DB results only** (before merge), so it's always true if the DB hasn't changed.

**Impact**: Continuous `rg --files -uu /` scanning the entire filesystem indefinitely for any query with fewer DB hits than the limit. Wastes CPU, I/O, and battery on a desktop app.

**Suggested fix** — check whether cache already has data before re-triggering:

```rust
if results.len() < effective_limit as usize {
    let has_cache = state.live_search_cache.lock().contains_key(&query);
    if !has_cache {
        start_live_search_worker(
            &app,
            state.inner(),
            query.clone(),
            effective_limit as usize,
            sort_by.clone(),
            sort_dir.clone(),
        );
    }
}
```

---

## Minor Issues 💡

### 2. Cmd+C behavior change in search input

**File**: `src/App.svelte:510-512`

```javascript
if (isMetaCopy && isTextInput) {
    return;  // lets browser handle native copy
}
```

Before this change, Cmd+C **always** triggered `copySelectedPaths()` regardless of focus. After, Cmd+C in the search input copies the query text instead of selected file paths.

This is likely intentional (and correctly fixes Cmd+C during rename), but note the UX shift: a user with selected results + focus in the search input pressing Cmd+C now copies query text, not file paths. Consider whether the early return should only apply during `editing.active`, or if this broader behavior is desired.

---

## Positive Highlights ✅

- **Good performance optimization** (`main.rs:1220`): Skipping live search when DB has enough results avoids unnecessary filesystem scans for well-indexed queries.
- **Proper helper extraction** (`App.svelte:496-502`): `isTextInputTarget()` reduces duplication and improves readability.
- **Correct event cleanup** (`App.svelte:684`): `unlistenLiveSearch` properly added to the cleanup array.
- **Live search → re-query pattern** is architecturally sound — the frontend re-querying to pick up cached live results is the right approach. The issue is just the missing re-trigger guard.
- **Formatting fix** (`main.rs:572-573`): Clean line-break improvement, no behavioral change.

---

## Summary

| Severity | Count |
|----------|-------|
| Critical | 0 |
| Major | 1 |
| Minor | 1 |

The core feature (live search with frontend re-query) is well-designed. The one major issue — infinite re-trigger — needs a guard before the live search worker is re-spawned for a query that already has cached results.
