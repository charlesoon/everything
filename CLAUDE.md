# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Everything — an ultrafast file/folder name search app for macOS and Windows. Indexes the filesystem into SQLite, providing sub-50ms search response. UI messages and spec are in Korean.

**Stack:** Tauri v2 + Rust (backend) + Svelte 5 (frontend) + SQLite (LIKE-based search)

## Build & Development Commands

```bash
# Dev server (Vite on :1420 + Tauri window)
npm run tauri dev

# Production build
npm run tauri build

# Lint (frontend build + cargo check)
npm run lint

# Rust tests (query module)
cargo test --manifest-path src-tauri/Cargo.toml

# Run a single Rust test
cargo test --manifest-path src-tauri/Cargo.toml <test_name>

# Frontend only (no Tauri shell)
npm run dev
```

## Architecture

### Backend (Rust) — `src-tauri/src/`

- **`main.rs`** (~4800 lines) — Core backend: app state (`AppState`), SQLite schema init, indexer (jwalk), search (LIKE-based multi-mode), file actions (open/reveal/trash/rename/quick_look), Tauri command handlers. Contains all `#[tauri::command]` functions and event emissions.
- **`query.rs`** — Search query parser: classifies input into `SearchMode` variants (`Empty`, `NameSearch`, `GlobName`, `ExtSearch`, `PathSearch`). Handles glob-to-LIKE conversion and LIKE escaping. Has unit tests.
- **`rescan.rs`** — Streaming subtree rescan (`rescan_subtree` + `SubtreeDiff`): diffs a directory tree against the DB with a hash-compacted snapshot (~24 B/row), upserts only new/changed rows in batches, deletes vanished rows. Used by the MustScanSubDirs handler, directory rename, and (via `SubtreeDiff`) catchup workers. Has unit tests.
- **`fd_search.rs`** — jwalk-based live filesystem search (5s timeout, max 5000 entries)
- **`mem_search.rs`** — In-memory compact entry search (`MemIndex`): built during MFT/WalkDir scan for instant results while DB upsert runs in background. Uses binary search, ext/dir maps, and time-budgeted contains matching.
- **`gitignore_filter.rs`** — Lazy .gitignore discovery and matching (depth 3, `ignore` crate)
- **`mcp_server.rs`** — MCP stdio server (`everything --mcp`): serves a `search` tool for AI agents straight from index.db (read-only, `query_only` pragma), reusing `run_db_search` — works with the GUI app closed. Also auto-registers the binary into Claude Code (`~/.claude.json`) and Codex (`~/.codex/config.toml`) on app startup and via `everything --register-mcp`; registration is idempotent and preserves existing config content.
- **`mac/`** — macOS-specific: FSEvents watcher (direct fsevent-sys binding), Spotlight search fallback (mdfind)
- **`win/`** — Windows-specific: MFT indexer (NTFS metadata scan), non-admin WalkDir fallback (`nonadmin_indexer.rs`), USN journal watcher, ReadDirectoryChangesW fallback, Shell icon loading (IShellItemImageFactory), native Explorer context menu, offline catchup (Windows Search / mtime scan)

### Frontend (Svelte 5) — `src/`

- **`App.svelte`** (~2800 lines) — Single-component UI: search input, virtual-scrolled result table (26px row height), inline rename, context menu, keyboard shortcuts, icon cache (max 500), status bar, theme toggle, FDA banner (macOS). Communicates with backend via `invoke()` (Tauri IPC). Platform-specific behavior via `get_platform()`.
- **`search-utils.js`** — Search debounce (200ms leading+trailing), viewport-preserve logic for scroll position during re-search.
- **`main.js`** — Svelte mount point.

### Data Flow

1. App start → scan filesystem → batch upsert into `entries` table (SQLite WAL mode)
   - macOS fresh index: parallel jwalk scan of `$HOME` (workers → bounded `sync_channel(8)` → single writer, sorted `INSERT OR IGNORE` batches — backpressure caps queued-batch memory); secondary indexes + FTS rebuild + ANALYZE are deferred to the background finalizing thread (`finalize_fresh_index` with `temp_store=FILE`, `fts_dirty` meta flag heals crashes)
   - macOS restart catchup: single parallel pass; each worker snapshots its root's rows into a hash-compacted `SubtreeDiff` over its own read connection, upserts only new/changed rows, and deletes vanished rows via snapshot leftovers (no per-row run_id stamping)
   - The finalizing thread ends with storage/memory maintenance: threshold-gated `VACUUM` (free pages ≥ 25% and ≥ 100MB), `wal_checkpoint(TRUNCATE)`, `shrink_memory`, and `malloc_zone_pressure_relief` (returns freed heap to the OS)
   - Windows: NTFS MFT scan of `C:\` → builds MemIndex for instant search → background DB upsert
2. User types → Svelte calls `search` command → Rust checks MemIndex first, then queries SQLite (LIKE + FTS5 trigram multi-mode) → returns `SearchResultDto { entries, modeLabel, totalCount, totalKnown }`
3. If results are sparse, a live scan (jwalk/fd_search) runs in a background thread
4. File watcher detects changes → upsert/delete affected paths over a persistent write connection (`watcher_conn`)
   - macOS: FSEvents (direct fsevent-sys, supports event ID replay). One stream watches `$HOME` plus canonicalized `.pathindexing` extra roots (`/tmp` → `/private/tmp`; event paths are remapped back to the stored prefix). The stream is rebuilt with event-id continuity when `.pathindexing` changes. `MustScanSubDirs` (kernel event-queue overflow) queues a streaming `rescan_subtree` on a single-flight background thread — change-detected, batch-bounded memory, per-path 5-min cooldown — instead of materializing the whole subtree in one Vec on the watcher loop
   - Windows: USN Change Journal → ReadDirectoryChangesW fallback

### Key Design Decisions

- **Search modes:** Query containing `/` or `\` → path search; containing `*` or `?` → glob-to-LIKE; simple `*.ext` → extension lookup; otherwise → 3-phase name search (exact → prefix → contains). The contains phase and non-name sorts use the FTS5 trigram index (queries ≥ 3 chars, gated by `fts_ready`); globs with a leading wildcard use an FTS literal-run prefilter + LIKE verify.
- **MCP server:** the app binary doubles as an MCP server (`--mcp` flag, JSON-RPC over stdio, newline-delimited). The DB-search core is shared via `run_db_search` (no AppState), so the MCP process searches the same index without the app running. App launch re-registers the current binary path into Claude Code/Codex configs (self-healing after moves/updates).
- **Search connections:** pooled warm read connections (`search_conn_pool`, max 3, 1GB mmap, cached prepared statements) reused across keystrokes; total counts use FTS-only `COUNT` (no join) where possible. The pool is only cleared on `reset_index`, not by watcher updates.
- **Sort:** Backend SQL `ORDER BY` (name/mtime/size × asc/desc), not relevance. Relevance sorting applied only on first page (offset=0) for name sort (decorate–sort–undecorate; ranks computed once per row).
- **Recent ops cache:** 2-second TTL prevents watcher from re-processing app-initiated rename/trash operations
- **Icons:** macOS: NSWorkspace via swift subprocess, cached by extension, prewarmed. Windows: IShellItemImageFactory (per-file for exe/lnk) + SHGetFileInfo (extension-based fallback)
- **Virtual scroll:** Fixed 26px row height, OverlayScrollbars, renders only visible rows ± buffer
- **Indexing root:** macOS: `$HOME`, Windows: `C:\`. Skips `.git`, `node_modules`, `DerivedData`, `.build` suffixes, platform-specific noisy directories
- **Context menu:** macOS: custom frontend menu. Windows: native Explorer context menu via Shell API, actions returned via `context_menu_action` event
- **Enter key:** Opens the selected file(s) on both platforms. Cmd/Ctrl+Enter reveals in Finder/Explorer. F2 starts rename on both platforms.
- **MemIndex (Windows):** In-memory index built during MFT/WalkDir scan provides instant search before DB is populated. Freed after background DB upsert completes.
- **Windows fallback chain:** MFT scan → USN watcher → non-admin WalkDir → RDCW watcher

### Tauri IPC Commands

`get_index_status`, `get_home_dir`, `get_platform`, `start_full_index`, `reset_index`, `search`, `fd_search`, `open`, `open_with`, `reveal_in_finder`, `show_package_contents` (macOS), `copy_paths`, `copy_files` (macOS), `move_to_trash`, `rename`, `get_file_icon`, `show_context_menu`, `quick_look` (macOS), `check_full_disk_access` (macOS), `open_privacy_settings` (macOS), `set_native_theme`, `mark_frontend_ready`, `frontend_log`

### Backend Events (→ Frontend)

`index_progress`, `index_state` (includes `isCatchup`), `index_updated`, `context_menu_action` (Windows)

## Design Spec

Full product spec is at `doc/spec.md` (English) and `doc/spec_KR.md` (Korean). Detailed architecture at `doc/architecture.md` (English) and `doc/architecture_KR.md` (Korean). Key SLOs: search p95 < 30ms backend, < 50ms with UI render. Default result limit 300 (100 for single-char queries, max 1000).

## Conventions

- Rust error handling: `AppResult<T> = Result<T, String>` — errors are string-mapped for Tauri IPC
- Serde: all DTOs use `#[serde(rename_all = "camelCase")]`
- DB version tracked via `PRAGMA user_version` (currently 7); version bump clears all entries and re-indexes
- Batch size for DB writes: 10,000 rows (macOS), 50,000 rows (Windows MFT)
- Frontend state is plain Svelte 5 reactive variables (no stores)
- Platform-specific code uses `#[cfg(target_os = "macos")]` / `#[cfg(target_os = "windows")]` conditional compilation
- DB init is split: `init_db()` creates tables (fast, blocks search), `ensure_db_indexes()` creates indexes (deferred, non-blocking)
- Ignore rules: `BUILTIN_SKIP_NAMES` (dir names), `BUILTIN_SKIP_SUFFIXES` (`.build`), `BUILTIN_SKIP_PATHS` (multi-segment paths), `DEFERRED_DIR_NAMES` (Windows system dirs), `.pathignore`, `.gitignore` (LazyGitignoreFilter)
