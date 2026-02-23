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
- **`fd_search.rs`** — jwalk-based live filesystem search (5s timeout, max 5000 entries)
- **`mem_search.rs`** — In-memory compact entry search (`MemIndex`): built during MFT/WalkDir scan for instant results while DB upsert runs in background. Uses binary search, ext/dir maps, and time-budgeted contains matching.
- **`gitignore_filter.rs`** — Lazy .gitignore discovery and matching (depth 3, `ignore` crate)
- **`mac/`** — macOS-specific: FSEvents watcher (direct fsevent-sys binding), Spotlight search fallback (mdfind)
- **`win/`** — Windows-specific: MFT indexer (NTFS metadata scan), non-admin WalkDir fallback (`nonadmin_indexer.rs`), USN journal watcher, ReadDirectoryChangesW fallback, Shell icon loading (IShellItemImageFactory), native Explorer context menu, offline catchup (Windows Search / mtime scan)

### Frontend (Svelte 5) — `src/`

- **`App.svelte`** (~2800 lines) — Single-component UI: search input, virtual-scrolled result table (26px row height), inline rename, context menu, keyboard shortcuts, icon cache (max 500), status bar, theme toggle, FDA banner (macOS). Communicates with backend via `invoke()` (Tauri IPC). Platform-specific behavior via `get_platform()`.
- **`search-utils.js`** — Search debounce (200ms leading+trailing), viewport-preserve logic for scroll position during re-search.
- **`main.js`** — Svelte mount point.

### Data Flow

1. App start → scan filesystem → batch upsert into `entries` table (SQLite WAL mode)
   - macOS: jwalk incremental scan of `$HOME`
   - Windows: NTFS MFT scan of `C:\` → builds MemIndex for instant search → background DB upsert
2. User types → Svelte calls `search` command → Rust checks MemIndex first, then queries SQLite (LIKE-based multi-mode) → returns `SearchResultDto { entries, modeLabel, totalCount, totalKnown }`
3. If results are sparse, a live scan (jwalk/fd_search) runs in a background thread
4. File watcher detects changes → upsert/delete affected paths
   - macOS: FSEvents (direct fsevent-sys, supports event ID replay)
   - Windows: USN Change Journal → ReadDirectoryChangesW fallback

### Key Design Decisions

- **Search modes:** Query containing `/` or `\` → path search; containing `*` or `?` → glob-to-LIKE; simple `*.ext` → extension lookup; otherwise → 3-phase name search (exact → prefix → contains)
- **Sort:** Backend SQL `ORDER BY` (name/mtime/size × asc/desc), not relevance. Relevance sorting applied only on first page (offset=0) for name sort.
- **Recent ops cache:** 2-second TTL prevents watcher from re-processing app-initiated rename/trash operations
- **Icons:** macOS: NSWorkspace via swift subprocess, cached by extension, prewarmed. Windows: IShellItemImageFactory (per-file for exe/lnk) + SHGetFileInfo (extension-based fallback)
- **Virtual scroll:** Fixed 26px row height, OverlayScrollbars, renders only visible rows ± buffer
- **Indexing root:** macOS: `$HOME`, Windows: `C:\`. Skips `.git`, `node_modules`, `DerivedData`, `.build` suffixes, platform-specific noisy directories
- **Context menu:** macOS: custom frontend menu. Windows: native Explorer context menu via Shell API, actions returned via `context_menu_action` event
- **Enter key:** Opens on Windows, starts rename on macOS. F2 starts rename on both platforms.
- **MemIndex (Windows):** In-memory index built during MFT/WalkDir scan provides instant search before DB is populated. Freed after background DB upsert completes.
- **Windows fallback chain:** MFT scan → USN watcher → non-admin WalkDir → RDCW watcher

### Tauri IPC Commands

`get_index_status`, `get_home_dir`, `get_platform`, `start_full_index`, `reset_index`, `search`, `fd_search`, `open`, `open_with`, `reveal_in_finder`, `copy_paths`, `copy_files` (macOS), `move_to_trash`, `rename`, `get_file_icon`, `show_context_menu`, `quick_look` (macOS), `check_full_disk_access` (macOS), `open_privacy_settings` (macOS), `set_native_theme`, `mark_frontend_ready`, `frontend_log`

### Backend Events (→ Frontend)

`index_progress`, `index_state` (includes `isCatchup`), `index_updated`, `context_menu_action` (Windows), `focus_search` (macOS)

## Design Spec

Full product spec is at `doc/spec.md` (English) and `doc/spec_KR.md` (Korean). Detailed architecture at `doc/architecture.md` (English) and `doc/architecture_KR.md` (Korean). Key SLOs: search p95 < 30ms backend, < 50ms with UI render. Default result limit 300 (100 for single-char queries, max 1000).

## Conventions

- Rust error handling: `AppResult<T> = Result<T, String>` — errors are string-mapped for Tauri IPC
- Serde: all DTOs use `#[serde(rename_all = "camelCase")]`
- DB version tracked via `PRAGMA user_version` (currently 5); version bump clears all entries and re-indexes
- Batch size for DB writes: 10,000 rows (macOS), 50,000 rows (Windows MFT)
- Frontend state is plain Svelte 5 reactive variables (no stores)
- Platform-specific code uses `#[cfg(target_os = "macos")]` / `#[cfg(target_os = "windows")]` conditional compilation
- DB init is split: `init_db()` creates tables (fast, blocks search), `ensure_db_indexes()` creates indexes (deferred, non-blocking)
- Ignore rules: `BUILTIN_SKIP_NAMES` (dir names), `BUILTIN_SKIP_SUFFIXES` (`.build`), `BUILTIN_SKIP_PATHS` (multi-segment paths), `DEFERRED_DIR_NAMES` (Windows system dirs), `.pathignore`, `.gitignore` (LazyGitignoreFilter)
