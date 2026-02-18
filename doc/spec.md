This document is the final implementation-ready design/spec for development with Tauri (v2) + Svelte. (Quick Look excluded, "search speed" is top priority, Enter=Rename, Double click=Open)

---

## 0. Document Info

- **Product name (tentative):** Everything
- **Platform:** macOS
- **Tech stack:** Tauri v2 + Rust + Svelte
- **Goal:** Ultrafast file/folder "name-based" search on par with Everything (Windows)
- **UI direction:** Everything (Windows) clone style — simple, dense interface centered on a search bar + result table
- **Window behavior:** Standard app window + instant activation via global shortcut (Cmd+Shift+Space)

---

## 1. Goals / Non-Goals

### 1.1 Goals

- Instant result updates as you type (perceived search response < 50ms)
- Smooth operation with 500K–1M entries without UI freezes
- "Search bar + instant filtering list" experience like Everything
- Always case-insensitive search
- Required actions:
  - Open
  - Open With... (MVP: Reveal in Finder fallback)
  - Reveal in Finder
  - Copy Path
  - Move to Trash
  - Rename (Enter)

### 1.2 Non-Goals (not in this version)

- Full-text content search
- Quick Look
- Network/remote drive indexing
- Full App Store sandbox compliance (future task)
- Search filters (file/folder/extension filters) — MVP searches everything without filters

---

## 2. Core UX Spec

### 2.1 Main Screen Layout

- **Top:** Search input (auto-focused on app launch)
- **Center:** Result table (virtual scroll)
  - Name (file icon + name)
  - Path (Directory)
  - Kind (extension/file/folder)
  - Modified
- **Bottom status bar:**
  - Index status: Ready | Indexing | Error
  - Indexed entries count
  - Last updated timestamp

### 2.2 Input/Interaction Rules (finalized)

- **Double click:** Open
- **Enter (selected, not editing):** Rename (start inline edit)
- **Enter (while editing):** Confirm rename
- **Esc (while editing):** Cancel rename

Multi-select:
- **Shift+Click:** Range select
- **Cmd+Click:** Toggle select
- Available actions with multi-select: Open, Reveal in Finder, Copy Path, Move to Trash
- Rename is only available in single-select mode (disabled during multi-select)

### 2.3 Keyboard Shortcuts (required)

- `Cmd+Shift+Space` — Global shortcut: activate app / focus search
- `Up/Down` — Move selection
- `Shift+Up/Down` — Extend range selection
- `PageUp/PageDown` — Quick navigation (selection)
- `Cmd+O` — Open
- `Cmd+Enter` — Reveal in Finder
- `Cmd+C` — Copy Path
- `Del` or `Cmd+Backspace` — Move to Trash (default: confirmation dialog ON)
- `F2` — Rename (secondary, same as Enter)
- `Cmd+A` — Select all

Since Enter triggers Rename, `Cmd+O` is the primary "open" shortcut.

### 2.4 Right-Click Context Menu (required)

- Open
- Open With... -> Reveal in Finder (MVP)
- Reveal in Finder
- Copy Path
- Move to Trash
- Rename (shown only in single-select)

---

## 3. Performance Targets (required SLOs)

### 3.1 Search

- Backend response after input: p95 < 30ms
- Perceived with UI render: < 50ms
- Result limit: default limit=300 (configurable 100–1000)

### 3.2 Indexing

- Initial indexing runs in background, no UI freezes
- DB writes use batch transactions
- Change detection (watcher) uses debounce + partial rescan for stability

---

## 4. Architecture Overview

### 4.1 Components

- **Frontend (Svelte):** UI, input events, virtual scroll, context menu
- **Backend (Rust):**
  - Indexer (scan + DB upsert)
  - Search engine (FTS5-based)
  - Action execution (open/reveal/trash/rename)
  - Watcher (FSEvents) for incremental updates
- **Storage (SQLite):**
  - `entries` table (normalized data)
  - `entries_fts` (FTS5 virtual table) + triggers for sync

### 4.2 Data Flow

1. App launch -> full disk scan (/) -> populate entries/fts
2. User searches -> Rust runs FTS query -> returns top N results
3. Svelte renders list
4. File changes occur -> watcher queue -> path-level upsert/delete

---

## 5. Data Store Design (SQLite)

### 5.1 DB File Location

- `AppDataDir/index.db` (Tauri app data dir)

### 5.2 Schema (finalized)

**entries**
- `id` INTEGER PRIMARY KEY
- `path` TEXT NOT NULL UNIQUE (POSIX full path)
- `name` TEXT NOT NULL (basename)
- `dir` TEXT NOT NULL (parent directory path)
- `is_dir` INTEGER NOT NULL (0/1)
- `ext` TEXT (lowercase extension, NULL for directories)
- `mtime` INTEGER (unix epoch seconds, optional)
- `size` INTEGER (optional, can store or omit in initial MVP)
- `indexed_at` INTEGER NOT NULL

**Indexes:**
- `CREATE INDEX idx_entries_dir ON entries(dir);`
- `CREATE INDEX idx_entries_name ON entries(name);`
- `CREATE INDEX idx_entries_isdir ON entries(is_dir);`
- `CREATE INDEX idx_entries_mtime ON entries(mtime);`

**FTS:** entries_fts (FTS5, prefix enabled)
- `CREATE VIRTUAL TABLE entries_fts USING fts5(name, path, content='entries', content_rowid='id', prefix='2 3 4 5 6');`

FTS5 tokenizer uses the default (unicode61), which automatically provides case-insensitive matching.

**Triggers (FTS sync, finalized):**
- **insert:**
  `INSERT INTO entries_fts(rowid, name, path) VALUES (new.id, new.name, new.path);`
- **delete:**
  `INSERT INTO entries_fts(entries_fts, rowid, name, path) VALUES('delete', old.id, old.name, old.path);`
- **update:**
  delete old + insert new (FTS recommended pattern)

Rationale: LIKE-based queries get slow quickly at scale. FTS5 + prefix reliably achieves "instant-as-you-type" performance.

---

## 6. Search Design (FTS Query/Sort)

### 6.1 Default Search Mode (finalized)

- Case: always case-insensitive (FTS5 unicode61 tokenizer default behavior)
- Input string tokenized by whitespace
- Each token supports prefix matching: e.g., `tok*`
- Both name and path are search targets

Example query builder rules:
- Input `foo bar` -> `name:foo* OR path:foo* AND (name:bar* OR path:bar*)` (built using FTS MATCH syntax for implementation simplicity)

### 6.2 Column Sort (finalized)

Search results use pure column sorting, not relevance-based.

Supported sort modes:
- Name ASC (default)
- Name DESC
- Modified ASC (oldest first)
- Modified DESC (newest first)

Behavior rules:
- Column header click toggles sort direction (ASC -> DESC -> ASC)
- Current sort column/direction shown with arrow indicator (▲/▼) in header
- Sorting performed on backend (SQL ORDER BY) for performance
- FTS bm25 relevance is not used — pure column sorting only

### 6.3 Short Input Optimization (required policy)

- Query length 0: recent items/favorites (optional) or empty screen
- Query length 1: search is performed but with lower limit (e.g., 100) + UI debounce (50ms)
- Query length 2+: normal limit (300)

---

## 7. Indexing Design

### 7.1 Indexing Root (finalized)

- Default root: `/` (full disk)
- System directories included: `/System`, `/Library`, `/usr`, etc. are also indexing targets
- No root selection UI — always indexes the full disk
- Full Disk Access permission required; user is prompted on first launch

### 7.2 Full Scan (initial indexing)

- Directory traversal in Rust (recommended: ignore/walkdir)
- For each entry:
  - Populate path/name/dir/is_dir/ext/mtime/size
- DB writes:
  - Batch transaction per 2,000–10,000 rows
  - Upsert: `INSERT ... ON CONFLICT(path) DO UPDATE SET ...`

Progress events:
- Send scanned_count, indexed_count, current_path to UI every 200ms

### 7.3 Incremental Updates (watcher)

- macOS: Monitor entire `/` via FSEvents
- Events are collected per-path and debounced (300–800ms)
- Processing strategy (stability-first, simple implementation):
  - On file/folder change event:
    1. If path exists: stat -> upsert
    2. If path doesn't exist: delete
  - rename/move events can be complex:
    - Converge to per-path upsert/delete
    - Optional "parent dir rescan" fallback

### 7.4 Exclusion Rules (defaults + options)

Default exclusions:
- `.git/`, `node_modules/`, `Library/Caches/`, `Trash`, etc.

Options:
- "Include hidden files"
- "Edit exclusion rules"

---

## 8. Action Design (file operations)

### 8.1 Open

- Open with default app
- Multi-select: open each selected item with its default app

### 8.2 Open With... (finalized: Reveal in Finder fallback)

- MVP uses Reveal in Finder as fallback
- Menu shows "Open With... -> Reveal in Finder"
- Future: recommended app list popover via macOS LaunchServices (Phase 2)

### 8.3 Reveal in Finder

- Open Finder with the item selected
- Multi-select: open each item's parent folder in Finder

### 8.4 Copy Path (finalized: multi-select support)

- Copy POSIX path to clipboard
- Single select: one path line
- Multi-select: paths separated by newline (LF, `\n`)
- Example (3 selected):
```
/Users/foo/bar.txt
/Users/foo/baz.png
/Applications/Safari.app
```

### 8.5 Move to Trash

- Move to Trash (use OS standard API when possible)
- Default: confirmation dialog ON
- Multi-select: "Move N items to Trash?" confirmation
- (Shift to skip confirmation is a future option)

### 8.6 Rename (Enter)

Rename only works in single-select mode. Enter/F2 is ignored during multi-select.

Rename includes filesystem change + DB/FTS update + watcher duplicate suppression.

Behavior definition:
- Enter -> inline edit
- Enter while editing -> confirm
- On confirm:
  1. Validate new name (no empty string, no path separators)
  2. Conflict check (same name exists in same dir)
  3. Execute `fs::rename(old_path, new_path)`
  4. DB update:
     - Modify entries.path/name/dir/ext
     - Automatically reflected via FTS triggers
  5. UI update: refresh selected item's path

Extension selection rules (recommended):
- Default selection range on edit start excludes the extension
  - Example: `report.pdf` -> only `report` is selected
- Folders: select entire name

---

## 9. Duplicate Event / Race Prevention (required)

Actions performed directly by the app (Rename/Trash/Open, etc.) can re-enter through watcher events.

### 9.1 "Recent Ops Cache" (required)

- Maintain `recent_ops` (LRU/HashMap) in Rust
- Key: old_path/new_path, op_type, timestamp
- TTL: 2 seconds
- When processing watcher events:
  - If identified as the same op within TTL, ignore/merge

Without this, "flickering" or "duplicate delete/upsert" frequently occurs after rename.

---

## 10. Tauri Command API (finalized)

### 10.1 Commands

- `get_index_status() -> IndexStatusDTO`
- `start_full_index()`
- `search(query: String, limit: u32, sort_by: String, sort_dir: String) -> Vec<EntryDTO>`
- `open(paths: Vec<String>)`
- `open_with(path: String)` (MVP: calls reveal_in_finder)
- `reveal_in_finder(paths: Vec<String>)`
- `copy_paths(paths: Vec<String>) -> String` (newline-separated POSIX paths)
- `move_to_trash(paths: Vec<String>) -> Result`
- `rename(path: String, new_name: String) -> Result<EntryDTO>`
- `get_file_icon(ext: String) -> Vec<u8>` (system icon per extension)

### 10.2 Events (Backend -> Frontend)

- `index_progress { scanned, indexed, current_path }`
- `index_state { state: Ready|Indexing|Error, message? }`
- `index_updated { entries_count, last_updated }`

DTO minimum fields (performance):
- `EntryDTO { path, name, dir, is_dir, ext?, mtime? }`

---

## 11. Frontend (Svelte) Implementation Spec

### 11.1 State Model

- `query: string`
- `results: EntryDTO[]`
- `selectedIndices: Set<number>` (multi-select support)
- `lastSelectedIndex: number` (Shift selection anchor)
- `editing: { active: boolean, path: string, draftName: string }`
- `indexStatus: IndexStatusDTO`
- `sortBy: 'name' | 'mtime'` (default: `'name'`)
- `sortDir: 'asc' | 'desc'` (default: `'asc'`)

### 11.2 Input Event Handling (state machine)

- Search input onInput:
  - Debounce 0–30ms (0 recommended by default)
  - `invoke('search', { query, limit, sort_by, sort_dir })`
- List keydown:
  - Enter:
    - If editing: confirm rename
    - If single-select: startRename()
    - If multi-select: ignore
  - Cmd+O: open(selected paths)
  - Cmd+Enter: reveal_in_finder
  - Cmd+C: copy_paths
  - Esc: cancel edit
  - Double click row: open(path)
  - Click: single select
  - Shift+Click: range select
  - Cmd+Click: toggle select

### 11.3 Virtual Scroll (required)

- Smooth performance even with hundreds of results
- Fixed row height (for performance)
- Icon/Kind computation cache

### 11.4 Inline Rename UI (required)

- Name column transforms into input
- Extension-excluding selection (recommended implementation)
- On error: toast + keep editing

### 11.5 File Icons (finalized)

- Use macOS system icons (NSWorkspace.icon(forFileType:))
- Per-extension cache: load icon once per extension and cache
- Cache key: extension string (e.g., "pdf", "txt", "app")
- Folders: cache a single folder icon
- Files without extension: use default document icon
- Icon size: 16x16 (fits table row height)
- Frontend maintains icon cache as `Map<string, dataURL>`

### 11.6 Column Header Sort UI

- Clicking Name or Modified column header toggles sort direction
- Current sort column shows direction indicator: ▲ (ASC) / ▼ (DESC)
- Path and Kind columns do not support sorting

---

## 12. Error Handling / Recovery

- **DB open failure:**
  - "Reset index" button (delete file and recreate)
- **Permission errors during indexing:**
  - Skip the path + show warning count in status bar
- **Rename/trash failure:**
  - Show error message to user (permissions/not found/conflict)

---

## 13. Settings (options)

- Limit (default 300)
- Include hidden files
- Edit exclusion patterns
- Trash confirmation dialog on/off

---

## 14. Development Order (implementation checklist)

**Phase 0: Search MVP (first priority)**
1. SQLite init + entries/FTS schema + triggers
2. Full scan indexer (root: /)
3. Search command (FTS MATCH + limit + ORDER BY)
4. Svelte UI (search bar + results + virtual scroll + file icons)
5. Double click open
6. Status bar index status
7. Column header sort (Name/Modified)

**Phase 1: Actions + Multi-select + Rename UX**
8. Multi-select UI (Shift/Cmd+Click)
9. Reveal/Copy/Trash implementation (multi-select support)
10. Enter=Rename (inline edit, single-select only) + rename command + DB/FTS sync
11. recent_ops cache for watcher duplicate prevention (can prepare alongside Phase 2)
12. Global shortcut (Cmd+Shift+Space) registration

**Phase 2: Watcher**
13. FSEvents watcher connection
14. Debounce + path upsert/delete
15. Bulk change stress test
