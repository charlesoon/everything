# Code Review: Live Search Optimization + Keyboard Fix

**Project**: everything (Tauri 2 desktop app, Rust + Svelte)
**Platform**: Single-user, local-only desktop application
**Scope**: 2 modified files (`main.rs`, `App.svelte`), 3 logical changes

## Build Validation

| Check | Status |
|-------|--------|
| `vite build` | Pass |
| `cargo check` | Pass (0 warnings) |

---

## Change Summary

1. **Rust** - Skip redundant live search when DB results are sufficient or cache exists
2. **Svelte** - Fix Cmd+C in text inputs (allow native copy instead of overriding with `copySelectedPaths`)
3. **Svelte** - Wire up `live_search_updated` event to re-query when live scan completes

---

## Critical Issues

None.

## Major Issues

None.

## Minor Issues

### 1. Cmd+C early return is too broad for non-text-input context

`src/App.svelte:510-512`

```javascript
if (isMetaCopy && isTextInput) {
  return;
}
```

This fixes the real bug (Cmd+C in the search box was calling `copySelectedPaths()` instead of native copy). However, note that the early return fires for **any** text input, including the rename `<input>`. During active rename (line 524: `if (editing.active)`), Cmd+C would also return early here before reaching the editing block. This is actually correct behavior (native copy should work during rename too), but it's worth being aware that the editing guard at line 524 will never see a Cmd+C event now.

**Verdict**: Not a bug. Behavior is correct. Just noting for awareness.

### 2. Consider empty-query edge case in the live search listener

`src/App.svelte:679`

```javascript
if ((event.payload?.query || '') === query.trim()) {
```

If `event.payload.query` is `""` and `query.trim()` is also `""`, this matches and calls `runSearch()`. In practice, the backend returns early from `start_live_search_worker` when `query.is_empty()` (line 556 of `main.rs`), so this event is never emitted with an empty query. The guard is effective by proxy, but it depends on backend behavior.

**Verdict**: Safe as-is. The backend guard prevents this scenario.

---

## Positive Highlights

### Correct live search optimization (`main.rs:1220-1232`)

The two-condition gate is well-designed:
- `results.len() < effective_limit` - only trigger filesystem scan when the DB index is incomplete for this query
- `!has_cache` - avoid re-scanning when results are already cached

This sits cleanly with the existing `inflight` guard inside `start_live_search_worker` (line 562), forming two layers of protection against redundant work.

### Proper event listener lifecycle (`App.svelte:678-684`)

The `live_search_updated` listener is correctly added to `unlistenFns` and cleaned up in `onDestroy`. The query-matching check prevents stale results from triggering a re-search when the user has already moved on to a different query.

### `isTextInputTarget` extraction (`App.svelte:496-502`)

Clean extraction - the logic was duplicated between Cmd+A handling and the new Cmd+C check. Making it a named function also makes the intent clearer at the call site.

### Bug fix: Cmd+C in text inputs (`App.svelte:510-512`)

Previously, pressing Cmd+C while the search input was focused would call `event.preventDefault()` + `copySelectedPaths()` (line 587-591), overriding native text copy. The early return correctly allows the browser's default copy behavior in text inputs while preserving path-copy behavior when focus is on the results list.

---

## Overall Assessment

Clean, focused changeset with three logical improvements. The live search optimization reduces unnecessary filesystem scans. The Cmd+C fix resolves a real usability bug. The event wiring completes the live search feedback loop. No issues found.
