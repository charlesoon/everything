# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Everything — an ultrafast file/folder name search app for macOS and Windows. Indexes the filesystem into SQLite, providing sub-50ms search response. UI messages and spec are in Korean.

**Stack:** Tauri v2 + Rust (backend) + Svelte 4 (frontend) + SQLite (LIKE-based search)

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

- **`main.rs`** — Core backend: app state, SQLite schema init, indexer (jwalk), search (LIKE-based multi-mode), file actions (open/reveal/trash/rename), Tauri command handlers
- **`query.rs`** — Search query parser: classifies input into `SearchMode` variants (`Empty`, `NameSearch`, `GlobName`, `ExtSearch`, `PathSearch`). Handles glob-to-LIKE conversion and LIKE escaping. Has unit tests.
- **`fd_search.rs`** — jwalk-based live filesystem search
- **`mem_search.rs`** — In-memory compact entry search
- **`gitignore_filter.rs`** — Recursive .gitignore discovery and matching
- **`mac/`** — macOS-specific: FSEvents watcher (direct fsevent-sys binding), Spotlight search fallback (mdfind)
- **`win/`** — Windows-specific: MFT indexer (NTFS metadata scan), USN journal watcher, ReadDirectoryChangesW fallback, Shell icon loading (IShellItemImageFactory), native Explorer context menu, offline catchup (Windows Search / mtime scan)

### Frontend (Svelte) — `src/`

- **`App.svelte`** — Single-component UI: search input, virtual-scrolled result table, inline rename, context menu, keyboard shortcuts, icon cache, status bar. Communicates with backend via `invoke()` (Tauri IPC). Detects platform via `get_platform()` for conditional behavior.
- **`main.js`** — Svelte mount point.

### Data Flow

1. App start → scan filesystem → batch upsert into `entries` table (SQLite WAL mode)
   - macOS: jwalk incremental scan of `$HOME`
   - Windows: NTFS MFT scan of `C:\`
2. User types → Svelte calls `search` command → Rust queries LIKE-based multi-mode → returns `EntryDto[]`
3. If results are sparse, a live scan (jwalk) runs in a background thread
4. File watcher detects changes → upsert/delete affected paths
   - macOS: FSEvents (direct fsevent-sys, supports event ID replay)
   - Windows: USN Change Journal → ReadDirectoryChangesW fallback

### Key Design Decisions

- **Search modes:** Query containing `/` or `\` → path search; containing `*` or `?` → glob-to-LIKE; simple `*.ext` → extension lookup; otherwise → 3-phase name search (exact → prefix → contains)
- **Sort:** Backend SQL `ORDER BY` (name/mtime/dir × asc/desc), not relevance
- **Recent ops cache:** 2-second TTL LRU prevents watcher from re-processing app-initiated rename/trash operations
- **Icons:** macOS: NSWorkspace via swift subprocess, cached by extension. Windows: IShellItemImageFactory (per-file for exe/lnk) + SHGetFileInfo (extension-based fallback)
- **Virtual scroll:** Fixed 28px row height, renders only visible rows ± buffer rows
- **Indexing root:** macOS: `$HOME`, Windows: `C:\`. Skips `.git`, `node_modules`, platform-specific noisy directories
- **Context menu:** macOS: custom frontend menu. Windows: native Explorer context menu via Shell API

### Tauri IPC Commands

`get_index_status`, `get_platform`, `start_full_index`, `reset_index`, `search`, `open`, `open_with`, `reveal_in_finder`, `copy_paths`, `move_to_trash`, `rename`, `get_file_icon`, `show_context_menu` (Windows only)

### Backend Events (→ Frontend)

`index_progress`, `index_state`, `index_updated`, `focus_search` (macOS)

## Design Spec

Full product spec is at `doc/spec.md` (English) and `doc/spec_KR.md` (Korean). Key SLOs: search p95 < 30ms backend, < 50ms with UI render. Default result limit 300 (100 for single-char queries, max 1000).

## Conventions

- Rust error handling: `AppResult<T> = Result<T, String>` — errors are string-mapped for Tauri IPC
- Serde: all DTOs use `#[serde(rename_all = "camelCase")]`
- DB version tracked via `PRAGMA user_version`; version bump clears all entries and re-indexes
- Batch size for DB writes: 10,000 rows (macOS), 50,000 rows (Windows MFT)
- Frontend state is plain Svelte reactive variables (no stores)
- Platform-specific code uses `#[cfg(target_os = "macos")]` / `#[cfg(target_os = "windows")]` conditional compilation
