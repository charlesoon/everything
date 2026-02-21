This document is the final implementation-ready design/spec for development with Tauri (v2) + Svelte. (Quick Look excluded, "search speed" is top priority, Enter=Rename, Double click=Open)

---

## 0. Document Info

- **Product name (tentative):** Everything
- **Platform:** macOS, Windows
- **Tech stack:** Tauri v2 + Rust + Svelte
- **Goal:** Ultrafast file/folder "name-based" search on par with Everything (Windows)
- **UI direction:** Everything (Windows) clone style — simple, dense interface centered on a search bar + result table
- **Window behavior:** Standard app window + instant activation via global shortcut (Cmd+Shift+Space on macOS)

---

## 1. Goals / Non-Goals

### 1.1 Goals

- Instant result updates as you type (perceived search response < 50ms)
- Smooth operation with 500K–1M entries without UI freezes
- "Search bar + instant filtering list" experience like Everything
- Always case-insensitive search
- Cross-platform support: macOS and Windows with native platform features
- Required actions:
  - Open
  - Open With... (MVP: Reveal in Finder/Explorer fallback)
  - Reveal in Finder / Explorer
  - Copy Path
  - Move to Trash / Recycle Bin
  - Rename (Enter)

### 1.2 Non-Goals (not in this version)

- Full-text content search
- Quick Look
- Network/remote drive indexing
- Full App Store sandbox compliance (future task)
- Search filters (file/folder/extension filters) — MVP searches everything without filters
- Linux support (partial — basic open/reveal/clipboard via xdg-open)

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
- **Cmd+Click (macOS) / Ctrl+Click (Windows):** Toggle select
- Available actions with multi-select: Open, Reveal in Finder/Explorer, Copy Path, Move to Trash
- Rename is only available in single-select mode (disabled during multi-select)

### 2.3 Keyboard Shortcuts (required)

- `Cmd+Shift+Space` — Global shortcut: activate app / focus search (macOS only)
- `Up/Down` — Move selection
- `Shift+Up/Down` — Extend range selection
- `PageUp/PageDown` — Quick navigation (selection)
- `Cmd+O` / `Ctrl+O` — Open
- `Cmd+Enter` / `Ctrl+Enter` — Reveal in Finder/Explorer
- `Cmd+C` / `Ctrl+C` — Copy Path
- `Del` or `Cmd+Backspace` — Move to Trash (default: confirmation dialog ON)
- `F2` — Rename (secondary, same as Enter)
- `Cmd+A` / `Ctrl+A` — Select all

Since Enter triggers Rename, `Cmd+O` / `Ctrl+O` is the primary "open" shortcut.

### 2.4 Right-Click Context Menu (required)

**macOS (custom menu):**
- Open
- Open With... -> Reveal in Finder (MVP)
- Reveal in Finder
- Copy Path
- Move to Trash
- Rename (shown only in single-select)

**Windows (native Explorer context menu):**
- Open, Reveal in Explorer, Copy Path (built-in items)
- Shell context menu items (Open with, Send to, etc.)

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
    - macOS: jwalk incremental 2-pass scan
    - Windows: NTFS MFT scan (rayon parallel)
  - Search engine (LIKE-based with multi-index optimization)
  - Action execution (open/reveal/trash/rename)
  - Watcher for incremental updates
    - macOS: FSEvents (direct fsevent-sys binding)
    - Windows: USN Change Journal → ReadDirectoryChangesW fallback
- **Storage (SQLite):**
  - `entries` table (normalized data)
  - Multiple indexes for search mode optimization

### 4.2 Data Flow

1. App launch → scan filesystem → populate entries table
   - macOS: jwalk scan of `$HOME`
   - Windows: MFT enumeration of `C:\`
2. User searches → Rust runs LIKE-based query → returns top N results
3. Svelte renders list
4. File changes occur → watcher queue → path-level upsert/delete
   - macOS: FSEvents
   - Windows: USN journal / ReadDirectoryChangesW

---

## 5. Data Store Design (SQLite)

### 5.1 DB File Location

- `AppDataDir/index.db` (Tauri app data dir)

### 5.2 Schema (finalized)

**entries**
- `id` INTEGER PRIMARY KEY
- `path` TEXT NOT NULL UNIQUE (full path)
- `name` TEXT NOT NULL (basename)
- `dir` TEXT NOT NULL (parent directory path)
- `is_dir` INTEGER NOT NULL (0/1)
- `ext` TEXT (lowercase extension, NULL for directories)
- `mtime` INTEGER (unix epoch seconds, optional)
- `size` INTEGER (optional, can store or omit in initial MVP)
- `indexed_at` INTEGER NOT NULL
- `run_id` INTEGER NOT NULL DEFAULT 0

**Indexes:**
- `idx_entries_name_nocase` — `name COLLATE NOCASE` (prefix/contains search)
- `idx_entries_dir` — `dir` (PathSearch directory scope)
- `idx_entries_dir_ext_name_nocase` — `(dir, ext, name)` (PathSearch + ext shortcut)
- `idx_entries_ext` — `ext` (ExtSearch)
- `idx_entries_ext_name` — `(ext, name)` (ExtSearch + sorting)
- `idx_entries_mtime` — `mtime` (modified date sorting)
- `idx_entries_run_id` — `run_id` (stale row deletion during incremental indexing)

**meta table:**
- `key TEXT PRIMARY KEY, value TEXT NOT NULL`
- Stores: `last_run_id`, `last_event_id` (macOS), `usn_next` / `usn_journal_id` / `index_complete` (Windows)

---

## 6. Search Design (LIKE Query/Sort)

### 6.1 Default Search Mode (finalized)

- Case: always case-insensitive (`COLLATE NOCASE`)
- Query classification by input pattern:
  - Contains `*` or `?` → glob-to-LIKE conversion
  - Simple `*.ext` → direct extension lookup
  - Contains `/` or `\` → path search (dir scoped)
  - Everything else → name search (3-phase: exact → prefix → contains)

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

### 6.3 Short Input Optimization (required policy)

- Query length 0: recent items/favorites (optional) or empty screen
- Query length 1: search is performed but with lower limit (e.g., 100) + UI debounce (50ms)
- Query length 2+: normal limit (300)

---

## 7. Indexing Design

### 7.1 Indexing Root (finalized)

| Platform | Scan Root | Notes |
|----------|-----------|-------|
| macOS | `$HOME` | Home directory only |
| Windows | `C:\` | Entire C: drive |

No root selection UI — always indexes the platform default.

### 7.2 Full Scan (initial indexing)

**macOS — jwalk incremental 2-pass:**
- Pass 0 (shallow): depth ≤ 6, priority directories first
- Pass 1 (deep): unlimited depth, only entries below depth 6
- Batch transaction per 10,000 rows
- Upsert: `INSERT ... ON CONFLICT(path) DO UPDATE SET ...`

**Windows — MFT scan:**
- Direct NTFS Master File Table enumeration via `FSCTL_ENUM_USN_DATA`
- Two-pass: enumerate MFT → resolve paths (rayon parallel)
- Batch transaction per 50,000 rows
- Fallback: jwalk-based scan if MFT unavailable

Progress events:
- Send scanned_count, indexed_count, current_path to UI every 200ms

### 7.3 Incremental Updates (watcher)

**macOS — FSEvents:**
- Direct fsevent-sys binding (not notify crate)
- Events collected per-path and debounced (300ms)
- Supports event ID replay on restart (skip full scan if clean replay)
- Processing: path exists → upsert, path missing → delete

**Windows — USN Journal (primary):**
- Monitors NTFS Change Journal via `FSCTL_READ_USN_JOURNAL`
- Zero-syscall path resolution using FRN cache from MFT scan
- Filters: CREATE, DELETE, RENAME_OLD/NEW, CLOSE (skips metadata-only)
- Rename pairing: OLD_NAME + NEW_NAME with 500ms timeout
- Debounce: 30s (longer due to noisy system changes)

**Windows — ReadDirectoryChangesW (fallback):**
- Uses notify crate when USN unavailable
- Watch root: `C:\`, debounce: 300ms
- Persists last_active timestamp for offline catchup on restart

**Windows — Offline catchup (search_catchup):**
- On restart with prior index: tries Windows Search service (ADODB via PowerShell)
- Fallback: mtime-based WalkDir scan for recently modified files

### 7.4 Exclusion Rules (defaults + options)

Default exclusions:
- `.git/`, `node_modules/`, `.Trash`, `.npm`, `.cache`, `__pycache__`, `.gradle`

Platform-specific exclusions:
- macOS: `Library/Caches/`, `Library/Developer/CoreSimulator`, `Library/Logs`, TCC roots (~40 paths)
- Windows: `Windows/`, `Program Files/`, `$Recycle.Bin/`, `System Volume Information/`, `AppData/Local/Temp`, `AppData/Local/Microsoft`

Options:
- `.pathignore` file (project root and home dir)
- `.gitignore` rules (ignore crate, depth 3)

---

## 8. Action Design (file operations)

### 8.1 Open

- Open with default app
- Multi-select: open each selected item with its default app
- macOS: `open <path>`, Windows: `cmd /C start "" "<path>"`, Linux: `xdg-open`

### 8.2 Open With... (finalized: Reveal in file manager fallback)

- MVP uses Reveal in Finder/Explorer as fallback
- Windows: native context menu includes "Open with" via Shell API
- Future: recommended app list popover via macOS LaunchServices (Phase 2)

### 8.3 Reveal in Finder / Explorer

- Open file manager with the item selected
- Multi-select: open each item's parent folder
- macOS: `open -R`, Windows: `explorer /select,`, Linux: `xdg-open` parent

### 8.4 Copy Path (finalized: multi-select support)

- Copy path to clipboard
- Single select: one path line
- Multi-select: paths separated by newline (LF, `\n`)
- macOS: `pbcopy`, Windows: `cmd /C clip`, Linux: `wl-copy` / `xclip` / `xsel`

### 8.5 Move to Trash

- Move to Trash / Recycle Bin (uses `trash` crate for cross-platform support)
- Default: confirmation dialog ON
- Multi-select: "Move N items to Trash?" confirmation
- (Shift to skip confirmation is a future option)

### 8.6 Rename (Enter)

Rename only works in single-select mode. Enter/F2 is ignored during multi-select.

Rename includes filesystem change + DB update + watcher duplicate suppression.

Behavior definition:
- Enter -> inline edit
- Enter while editing -> confirm
- On confirm:
  1. Validate new name (no empty string, no path separators)
  2. Conflict check (same name exists in same dir)
  3. Execute `fs::rename(old_path, new_path)`
  4. DB update: modify entries.path/name/dir/ext
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
- `get_platform() -> String` ("windows", "macos", or other)
- `start_full_index()`
- `reset_index()`
- `search(query: String, limit: u32, sort_by: String, sort_dir: String) -> SearchResultDTO`
- `open(paths: Vec<String>)`
- `open_with(path: String)` (MVP: calls reveal_in_finder)
- `reveal_in_finder(paths: Vec<String>)`
- `copy_paths(paths: Vec<String>) -> String` (newline-separated paths)
- `move_to_trash(paths: Vec<String>) -> Result`
- `rename(path: String, new_name: String) -> Result<EntryDTO>`
- `get_file_icon(ext: String, path: Option<String>) -> Option<Vec<u8>>` (system icon per extension/path)
- `show_context_menu(paths: Vec<String>, x: f64, y: f64)` (Windows only — native Explorer context menu)

### 10.2 Events (Backend -> Frontend)

- `index_progress { scanned, indexed, current_path }`
- `index_state { state: Ready|Indexing|Error, message? }`
- `index_updated { entries_count, last_updated, permission_errors }`
- `focus_search` (macOS global shortcut)

DTO minimum fields (performance):
- `EntryDTO { path, name, dir, is_dir, ext?, mtime?, size? }`

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
- `platform: string` ("windows", "macos", or other)

### 11.2 Input Event Handling (state machine)

- Search input onInput:
  - Debounce 0–30ms (0 recommended by default)
  - `invoke('search', { query, limit, sort_by, sort_dir })`
- List keydown:
  - Enter:
    - If editing: confirm rename
    - If single-select: startRename()
    - If multi-select: ignore
  - Cmd+O / Ctrl+O: open(selected paths)
  - Cmd+Enter / Ctrl+Enter: reveal_in_finder
  - Cmd+C / Ctrl+C: copy_paths
  - Esc: cancel edit
  - Double click row: open(path)
  - Click: single select
  - Shift+Click: range select
  - Cmd+Click / Ctrl+Click: toggle select
- Right-click:
  - Windows: `invoke('show_context_menu', { paths, x, y })` (native Shell API)
  - macOS: custom frontend context menu

### 11.3 Virtual Scroll (required)

- Smooth performance even with hundreds of results
- Fixed row height (for performance)
- Icon/Kind computation cache

### 11.4 Inline Rename UI (required)

- Name column transforms into input
- Extension-excluding selection (recommended implementation)
- On error: toast + keep editing

### 11.5 File Icons (finalized)

**macOS:**
- Use macOS system icons (NSWorkspace via `swift -e` subprocess)
- Per-extension cache: load icon once per extension and cache
- Icon size: 16x16 (fits table row height)
- Prewarm 20 common extensions at startup

**Windows:**
- Per-file icons for executables: exe, lnk, ico, url, scr, appx
  - Loaded via IShellItemImageFactory (32x32 PNG, requires real file path)
- Extension-based fallback via SHGetFileInfo
- No prewarming (loaded on demand)

Common:
- Cache key: extension string (e.g., "pdf", "txt", "app")
- Folders: cache a single folder icon
- Files without extension: use default document icon
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
- **MFT scan failure (Windows):**
  - Fallback to USN-only or RDCW watcher mode

---

## 13. Settings (options)

- Limit (default 300)
- Include hidden files
- Edit exclusion patterns (`.pathignore`)
- Trash confirmation dialog on/off

---

## 14. Development Order (implementation checklist)

**Phase 0: Search MVP (first priority)**
1. SQLite init + entries schema + indexes
2. Full scan indexer (macOS: jwalk, Windows: MFT)
3. Search command (LIKE-based multi-mode + limit + ORDER BY)
4. Svelte UI (search bar + results + virtual scroll + file icons)
5. Double click open
6. Status bar index status
7. Column header sort (Name/Modified)

**Phase 1: Actions + Multi-select + Rename UX**
8. Multi-select UI (Shift/Cmd+Click, Ctrl+Click on Windows)
9. Reveal/Copy/Trash implementation (multi-select, cross-platform)
10. Enter=Rename (inline edit, single-select only) + rename command + DB sync
11. recent_ops cache for watcher duplicate prevention
12. Global shortcut (Cmd+Shift+Space) registration (macOS)

**Phase 2: Watcher**
13. macOS: FSEvents watcher connection
14. Windows: USN journal watcher + RDCW fallback
15. Debounce + path upsert/delete
16. Bulk change stress test

**Phase 3: Windows Native Features**
17. Windows native context menu (Shell API)
18. Per-file icon loading (exe, lnk, etc.)
19. Offline catchup (Windows Search service / mtime scan)
