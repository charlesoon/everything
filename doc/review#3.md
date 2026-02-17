# Code Review: Uncommitted Changes (`git diff HEAD`)

## Critical Issues 🚨
None found.

## Major Issues ⚠️
1. Stale live-search cache can return deleted/renamed paths indefinitely for the same query.
   - `src-tauri/src/main.rs:1221` now skips `start_live_search_worker` whenever a cache entry exists.
   - `src-tauri/src/main.rs:1241` still merges cached results into responses.
   - Cache invalidation only happens on reset (`src-tauri/src/main.rs:1148`) or size overflow (`src-tauri/src/main.rs:581`), not after watcher/app mutations (`src-tauri/src/main.rs:955`, `src-tauri/src/main.rs:1324`, `src-tauri/src/main.rs:1381`).
   - Repro:
     1. Search `foo` so `live_search_cache["foo"]` is populated.
     2. Rename or trash a matched file.
     3. Search `foo` again.
     4. Old path can still appear from cache; open/reveal may fail for non-existent path.
   - Suggested fix: invalidate affected live-search cache entries (or clear cache) whenever filesystem mutations are applied, or add TTL/refresh per query.

## Minor Issues 💡
1. Requested static checks are not fully wired in this repo.
   - `package.json:6` has no `lint`/`test` scripts.
   - TypeScript compiler is not installed, so `npx tsc --noEmit` fails.

## Positive Highlights ✅
1. `Cmd/Ctrl+C` in text inputs is correctly fixed (`src/App.svelte:510`) and no longer overrides native text copy with path copy.
2. `live_search_updated` listener wiring is clean and scoped to current query (`src/App.svelte:678`, `src/App.svelte:679`) with proper cleanup (`src/App.svelte:684`, `src/App.svelte:694`).
3. Live-search worker launch dedupe remains in place via in-flight guard (`src-tauri/src/main.rs:561`).

## Validation Run
1. `npm run build` passed.
2. `cargo check` passed.
3. `cargo test --no-run` passed.
4. `npx tsc --noEmit` failed (TypeScript compiler not installed).
5. `npm run lint` failed (script missing).
6. `npm run test` failed (script missing).

## Scope
- Reviewed mode: `all` (`git diff HEAD`, staged + unstaged tracked changes).
- Untracked file `doc/review#1.md` was outside this diff scope.
