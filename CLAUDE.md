# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Everything — a macOS "Everything"-style ultrafast file/folder name search app. Indexes the entire home directory into SQLite with FTS5, providing sub-50ms search response. UI messages and spec are in Korean.

**Stack:** Tauri v2 + Rust (backend) + Svelte 4 (frontend) + SQLite (FTS5)

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

- **`main.rs`** — Monolithic backend: app state, SQLite schema init, full-disk indexer (WalkDir), FSEvents watcher (notify crate), search (FTS5 + LIKE fallback + live scan), file actions (open/reveal/trash/rename), icon loading (NSWorkspace via swift CLI), Tauri command handlers
- **`query.rs`** — Search query parser: classifies input into `SearchMode` variants (`Empty`, `Fts`, `GlobName`, `PathSearch`). Handles FTS5 expression building, glob-to-LIKE conversion, and LIKE escaping. Has unit tests.

### Frontend (Svelte) — `src/`

- **`App.svelte`** — Single-component UI: search input, virtual-scrolled result table, inline rename, context menu, keyboard shortcuts, icon cache, status bar. Communicates with backend via `invoke()` (Tauri IPC).
- **`main.js`** — Svelte mount point.

### Data Flow

1. App start → full scan of `$HOME` via WalkDir → batch upsert into `entries` table (SQLite WAL mode)
2. FTS5 virtual table `entries_fts` synced via triggers (insert/delete/update)
3. User types → Svelte calls `search` command → Rust queries FTS5 MATCH + LIKE union → returns `EntryDto[]`
4. If FTS results are sparse, a live scan (ripgrep or WalkDir fallback) runs in a background thread; results merge into cache and emit `live_search_updated` event
5. FSEvents watcher debounces file changes (500ms) → upsert/delete affected paths → invalidates live search cache

### Key Design Decisions

- **Search modes:** Query containing `/` → path search (dir LIKE + name LIKE); containing `*` or `?` → glob-to-LIKE; otherwise → FTS5 prefix match with LIKE fallback union
- **Sort:** Backend SQL `ORDER BY` (name/mtime/dir × asc/desc), not FTS relevance
- **Recent ops cache:** 2-second TTL LRU prevents watcher from re-processing app-initiated rename/trash operations
- **Icons:** macOS system icons loaded via `swift -e` subprocess (NSWorkspace), cached by extension in `HashMap<String, Vec<u8>>`
- **Virtual scroll:** Fixed 28px row height, renders only visible rows ± 6 buffer rows
- **Indexing root:** `$HOME` (not `/`), skips `.git`, `node_modules`, `Library/Caches`, `.Trash`

### Tauri IPC Commands

`get_index_status`, `start_full_index`, `reset_index`, `search`, `open`, `open_with`, `reveal_in_finder`, `copy_paths`, `move_to_trash`, `rename`, `get_file_icon`

### Backend Events (→ Frontend)

`index_progress`, `index_state`, `index_updated`, `live_search_updated`, `focus_search`

## Design Spec

Full product spec is at `doc/spec.md` (Korean). Key SLOs: search p95 < 30ms backend, < 50ms with UI render. Default result limit 300 (100 for single-char queries, max 1000).

## Conventions

- Rust error handling: `AppResult<T> = Result<T, String>` — errors are string-mapped for Tauri IPC
- Serde: all DTOs use `#[serde(rename_all = "camelCase")]`
- DB version tracked via `PRAGMA user_version`; version bump clears all entries and re-indexes
- Batch size for DB writes: 4,000 rows per transaction
- Frontend state is plain Svelte reactive variables (no stores)
