# Architecture

macOS "Everything" — a Tauri v2 desktop app that indexes the entire home directory into SQLite for sub-50ms file/folder name search.

## Tech Stack

| Layer | Technology |
|-------|-----------|
| App framework | Tauri v2 |
| Backend | Rust (rusqlite, jwalk, fsevent-sys, ignore) |
| Frontend | Svelte 4 (single component) |
| DB | SQLite WAL mode, LIKE-based search |
| Build | Vite 5, Cargo |

---

## Module Structure

```
src-tauri/src/
├── main.rs              # App state, DB, indexing, search, file actions, IPC handlers
├── query.rs             # Search query parser (SearchMode classification)
├── fd_search.rs         # jwalk-based live filesystem search
├── fsevent_watcher.rs   # Direct FSEvents binding (macOS only)
├── gitignore_filter.rs  # Recursive .gitignore discovery and matching
└── spotlight_search.rs  # mdfind-based Spotlight search fallback

src/
├── main.js              # Svelte mount point
└── App.svelte           # Entire UI (1,755-line single component)
```

### Module Dependencies

```
main.rs ──→ query.rs            (query parsing)
        ──→ fd_search.rs        (live search)
        ──→ fsevent_watcher.rs  (file watching, macOS only)
        ──→ gitignore_filter.rs (.gitignore filtering)
        ──→ spotlight_search.rs (Spotlight fallback)

query.rs           standalone (no dependencies)
fd_search.rs    ──→ main.rs (EntryDto, should_skip_path, IgnorePattern)
spotlight_search.rs ──→ main.rs (EntryDto)
gitignore_filter.rs    standalone (only ignore crate)
fsevent_watcher.rs     standalone (only fsevent-sys)
```

---

## App State

```rust
struct AppState {
    db_path: PathBuf,                     // index.db path
    home_dir: PathBuf,                    // $HOME
    cwd: PathBuf,                         // current working directory
    db_ready: Arc<AtomicBool>,            // DB initialization complete flag
    indexing_active: Arc<AtomicBool>,     // indexing in-progress flag
    status: Arc<Mutex<IndexStatus>>,      // indexing state (state, counts)
    path_ignores: Arc<Vec<PathBuf>>,      // ignored path list
    path_ignore_patterns: Arc<Vec<IgnorePattern>>,  // ignore patterns (glob)
    gitignore: SharedGitignoreFilter,     // Arc<GitignoreFilter>
    recent_ops: Arc<Mutex<Vec<RecentOp>>>,          // rename/trash 2-second TTL cache
    icon_cache: Arc<Mutex<HashMap<String, Vec<u8>>>>,   // extension→PNG icon
    fd_search_cache: Arc<Mutex<Option<FdSearchCache>>>, // live search cache
    negative_name_cache: Arc<Mutex<Vec<NegativeNameEntry>>>, // zero-result query 60s cache
    ignore_cache: Arc<Mutex<Option<IgnoreRulesCache>>>,      // ignore rules mtime cache
}
```

All fields are wrapped in `Arc` for `Clone` support. Injected into IPC handlers via Tauri `State<AppState>`.

---

## DB Schema

**Location**: `<app_data_dir>/index.db` | **Version**: `PRAGMA user_version = 4`

### entries table

```sql
CREATE TABLE entries (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    dir TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    ext TEXT,
    mtime INTEGER,
    size INTEGER,
    indexed_at INTEGER NOT NULL,
    run_id INTEGER NOT NULL DEFAULT 0
);
```

### Indexes

| Index | Purpose |
|-------|---------|
| `idx_entries_name_nocase` | `name COLLATE NOCASE` — NameSearch prefix/contains |
| `idx_entries_dir` | `dir` — PathSearch directory scope |
| `idx_entries_dir_ext_name_nocase` | `(dir, ext, name)` — PathSearch + ext shortcut |
| `idx_entries_ext` | `ext` — ExtSearch |
| `idx_entries_ext_name` | `(ext, name)` — ExtSearch + sorting |
| `idx_entries_mtime` | `mtime` — modified date sorting |
| `idx_entries_run_id` | `run_id` — stale row deletion during incremental indexing |

### meta table

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
```

| Key | Purpose |
|-----|---------|
| `last_run_id` | Last indexing run ID (baseline for incremental comparison) |
| `last_event_id` | FSEvents event ID (replay starting point on restart) |

### Pragma Settings

```
journal_mode=WAL  synchronous=NORMAL  temp_store=MEMORY  busy_timeout=3000
During indexing: cache_size=64MB  mmap_size=256MB  wal_autocheckpoint=OFF
After indexing:  cache_size=16MB  mmap_size=0     wal_autocheckpoint=1000 → TRUNCATE
```

---

## Startup Sequence

```
App launch
  │
  ├─ Resolve paths: app_data_dir, db_path, home_dir, cwd
  ├─ Load ignore rules: .pathignore + .gitignore roots + TCC roots
  ├─ Build GitignoreFilter: scan non-hidden dirs under $HOME up to depth 3
  ├─ Construct and register AppState
  ├─ Register global shortcut: Cmd+Shift+Space → show window + focus_search event
  │
  └─ Start background thread
       │
       ├─ init_db(): create tables/indexes, version check (recreate on mismatch)
       ├─ purge_ignored_entries(): delete existing DB entries for ignored paths
       ├─ db_ready = true (search now available)
       ├─ emit_status_counts → send current entry count to frontend
       │
       ├─ [Conditional start] last_event_id exists AND DB has entries?
       │    ├─ YES → Start FSEvents watcher (replay mode)
       │    │         Replay succeeds → Ready (skip full scan)
       │    │         MustScanSubDirs ≥ 10 → full scan fallback
       │    │
       │    └─ NO  → Start incremental indexing + FSEvents watcher (since now)
       │
       └─ Icon prewarming: preload 20 common extensions
```

---

## Indexing Flow

### Incremental Indexing (`run_incremental_index`)

```
run_incremental_index
  │
  ├─ Set indexing pragmas (large cache, mmap)
  ├─ current_run_id = last_run_id + 1
  ├─ Preload existing entries HashMap: path → (mtime, size)
  │
  ├─ Classify $HOME child directories
  │    ├─ priority roots: normal dirs excluding Library/.Trash
  │    └─ deferred roots: Library, .Trash, .Trashes
  │
  ├─ Pass 0 (shallow): jwalk depth ≤ 6, priority → deferred order
  │    ├─ No mtime+size change → UPDATE run_id only (lightweight)
  │    ├─ Changed or new → INSERT/UPDATE (all columns)
  │    ├─ Batch commit every 10,000 rows
  │    └─ Emit index_progress event every 200ms
  │    └─ Pass 0 complete → index_updated event (early search available)
  │
  ├─ Pass 1 (deep): jwalk unlimited depth, only depth > 6 entries
  │    └─ (same incremental logic)
  │
  ├─ Cleanup: DELETE FROM entries WHERE run_id < current_run_id
  ├─ meta.last_run_id = current_run_id
  ├─ ANALYZE + restore pragmas + WAL checkpoint
  └─ index_state=Ready, index_updated event emit
```

### Ignore System

```
should_skip_path(path)
  │
  ├─ BUILTIN_SKIP_NAMES: .git, node_modules, .Trash, .npm, .cache,
  │                       CMakeFiles, .qtc_clangd, __pycache__, .gradle
  ├─ BUILTIN_SKIP_PATHS: Library/Caches, Library/Developer/CoreSimulator,
  │                       Library/Logs, .vscode/extensions
  ├─ .pathignore: loaded from project root and $HOME
  ├─ macOS TCC roots: ~/Library/Mail, Safari, Messages, etc. (~40 paths)
  ├─ IgnorePattern::AnySegment: **/target etc., matches at any depth
  ├─ IgnorePattern::Glob: wildcard pattern matching
  └─ gitignore_filter: .gitignore rules (ignore crate)
```

---

## Search Flow

### Query Classification (`query.rs`)

| Input Pattern | SearchMode | Example |
|---------------|-----------|---------|
| Empty string | `Empty` | `""` |
| Contains `*` or `?` | `GlobName` | `*.rs`, `test?` |
| Simple `*.ext` | `ExtSearch` | `*.pdf` |
| Contains `/` | `PathSearch` | `src/ main`, `Projects/ *.rs` |
| Everything else | `NameSearch` | `readme`, `config` |

### Search Execution Sequence (`execute_search`)

```
User input
  │
  ├─ DB not ready → Spotlight fallback (mdfind) → return
  │
  ├─ Parse query → determine SearchMode
  │
  ├─ [NameSearch] Check negative cache
  │    ├─ Cache hit (within 300-550ms, unconfirmed) → find command single fallback
  │    └─ Cache hit (otherwise) → return empty result immediately
  │
  ├─ DB search (by mode)
  │    │
  │    ├─ Empty: SELECT ... ORDER BY sort LIMIT offset
  │    │
  │    ├─ NameSearch (3-phase):
  │    │    Phase 0: name = query (exact match)
  │    │    Phase 1: name LIKE 'query%' (prefix, idx_entries_name_nocase)
  │    │    Phase 2: 8ms probe → 30ms fetch (name LIKE '%query%')
  │    │
  │    ├─ GlobName: name LIKE (glob→LIKE conversion)
  │    │
  │    ├─ ExtSearch: ext = 'ext' (direct index lookup)
  │    │
  │    └─ PathSearch:
  │         dir hint resolvable → dir scoped query + ext shortcut
  │         dir hint unresolvable → dir LIKE + 2-phase probe
  │
  ├─ Zero results + not indexing + GlobName/ExtSearch
  │    → find command fallback (maxdepth 8)
  │
  ├─ Zero results + indexing
  │    → Spotlight fallback (mdfind, 3s timeout, max 300 results)
  │
  ├─ Post-processing
  │    ├─ Ignore rules filtering
  │    ├─ Relevance sorting (name sort, when offset=0)
  │    │    rank 0: exact match
  │    │    rank 1: prefix match
  │    │    rank 2: name contains
  │    │    rank 3: path-end match
  │    │    rank 4: path contains
  │    │    shallower paths preferred within same rank
  │    └─ NameSearch zero results → save to negative cache (60s TTL)
  │
  └─ Return SearchResultDto { entries, modeLabel }
```

### Spotlight Fallback (`spotlight_search.rs`)

```
search_spotlight(home_dir, query)
  │
  ├─ query < 2 chars → empty result
  ├─ Execute mdfind -name <query> -onlyin <home_dir>
  ├─ Stream stdout
  │    ├─ 3s timeout → timed_out = true, abort
  │    └─ 300 results reached → abort
  ├─ Kill child process
  └─ SpotlightResult { entries, timed_out }
```

---

## Watcher Flow

### FSEvents Architecture (`fsevent_watcher.rs`)

```
FsEventWatcher::new(root, since_event_id, tx)
  │
  ├─ Direct fsevent_sys binding (notify crate not used)
  ├─ Flags: FileEvents | NoDefer
  ├─ Latency: 0.3s
  ├─ Runs CFRunLoop on dedicated thread "everything-fsevents"
  │
  └─ Callback → FsEvent classification
       ├─ HistoryDone      (replay complete)
       ├─ MustScanSubDirs  (subtree rescan needed)
       └─ Paths            (normal file changes)
```

### Watcher Event Processing (`start_fsevent_watcher_worker`)

```
Event receive loop (100ms recv_timeout)
  │
  ├─ Paths → add to pending_paths, set debounce timer (300ms)
  │
  ├─ MustScanSubDirs → immediate subtree rescan + upsert
  │    (during conditional startup, count ≥ 10 → trigger full scan)
  │
  ├─ HistoryDone → flush pending immediately
  │    (end conditional startup)
  │
  ├─ Debounce expired → process_watcher_paths()
  │    ├─ Skip if indexing_active
  │    ├─ Each path: check should_skip / is_recently_touched
  │    ├─ Existing paths → upsert (including children for directories)
  │    └─ Missing paths → delete from DB
  │
  └─ Flush last_event_id to meta table every 30s
```

---

## IPC Commands

| Command | Direction | Description |
|---------|-----------|-------------|
| `get_index_status` | FE→BE | Indexing state, entry count, progress |
| `get_home_dir` | FE→BE | Home directory path |
| `start_full_index` | FE→BE | Trigger full re-indexing |
| `reset_index` | FE→BE | Reset DB and re-index |
| `search` | FE→BE | DB search → `SearchResultDto { entries, modeLabel }` |
| `fd_search` | FE→BE | jwalk live search → `FdSearchResultDto { entries, total, timedOut }` |
| `open` | FE→BE | Open via `open` command (fallback to `open -R` for directories) |
| `open_with` | FE→BE | Reveal in Finder |
| `reveal_in_finder` | FE→BE | `open -R` (single) / open parent directory (multi) |
| `copy_paths` | FE→BE | Copy paths to clipboard (pbcopy) |
| `move_to_trash` | FE→BE | Move to trash + delete from DB |
| `rename` | FE→BE | Rename + DB update → return new EntryDto |
| `get_file_icon` | FE→BE | Return system icon PNG per extension |

## Backend Events

| Event | Payload | Timing |
|-------|---------|--------|
| `index_progress` | `{ scanned, indexed, currentPath }` | Every 200ms during indexing |
| `index_state` | `{ state, message }` | On Indexing/Ready/Error transitions |
| `index_updated` | `{ entriesCount, lastUpdated, permissionErrors }` | After indexing complete, watcher updates, file actions |
| `focus_search` | (none) | Cmd+Shift+Space global shortcut |

---

## Frontend Architecture

### Single Component (`App.svelte`)

A 1,755-line single component containing search input, virtual-scrolled table, inline rename, context menu, keyboard shortcuts, icon cache, and status bar.

### State Management

Uses Svelte reactive variables (no stores).

| Category | Key Variables |
|----------|--------------|
| Search | `query`, `results`, `searchGeneration`, `dbLatencyMs`, `searchModeLabel`, `sortBy`, `sortDir` |
| Selection | `selectedIndices` (Set), `selectionAnchor`, `lastSelectedIndex` |
| Editing | `editing { active, path, index, draftName }` |
| Indexing | `indexStatus`, `scanned`, `indexed`, `currentPath`, `lastReadyCount` |
| Virtual scroll | `scrollTop`, `viewportHeight`, `colWidths` |
| Cache | `iconCache` (Map), `highlightCache` (Map) |

### Search Input → Result Sequence

```
User typing
  │
  ├─ on:input → scheduleSearch()
  │    ├─ 200ms+ elapsed → execute immediately (leading edge)
  │    └─ < 200ms → execute after 200ms (trailing edge)
  │
  ├─ runSearch()
  │    ├─ searchGeneration++ (prevent stale responses)
  │    ├─ invoke('search', { query, limit: 500, offset: 0, sort_by, sort_dir })
  │    ├─ Response: { entries, modeLabel }
  │    ├─ results = entries
  │    ├─ searchModeLabel = modeLabel
  │    └─ Selection restoration based on path
  │
  └─ Infinite scroll
       Within 10 rows of bottom → loadMore()
       → invoke('search', { offset: results.length })
       → append to results
```

### Virtual Scroll

```
Fixed row height: 28px
Buffer: 10 rows above and below (total overscan ~20 rows)

scrollTop
  → startIndex = max(0, floor(scrollTop / 28) - 10)
  → endIndex = min(results.length, startIndex + visibleCount)
  → visibleRows = results.slice(startIndex, endIndex)
  → translateY = startIndex * 28

DOM:
  <div class="table-body">          ← scroll container
    <div style="height:{totalHeight}px">  ← total virtual height
      <div style="transform:translateY({translateY}px)">  ← offset
        {#each visibleRows}...
      </div>
    </div>
  </div>
```

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Escape` | Deselect, focus search input |
| `Up` / `Down` | Move selection (Shift: range select) |
| `PageUp` / `PageDown` | Page-level navigation |
| `Enter` | Start inline rename |
| `F2` | Start inline rename |
| `Cmd+O` | Open selected items |
| `Cmd+Enter` | Reveal in Finder |
| `Cmd+C` | Copy paths |
| `Cmd+A` | Select all |
| `Delete` / `Cmd+Backspace` | Move to trash |

### Context Menu

Shown on right-click: Open, Open With, Reveal in Finder, Copy Path, Move to Trash, Rename (single-select only)

### Icon System

```
visibleRows change
  → call ensureIcon(entry)
  → if in iconCache, return immediately
  → if not, invoke('get_file_icon', { ext })
       → backend: swift -e NSWorkspace 16x16 PNG
       → convert to base64 data URI, store in iconCache
  → Prewarm 20 common extensions at startup
```

### Theme

Syncs with system settings (`prefers-color-scheme: dark`). Based on CSS custom properties.

```css
:root {
  --bg-app, --text-primary, --surface, --row-hover,
  --row-selected, --border-soft, --focus-ring, ...
}
```

### Status Bar

| State | Display |
|-------|---------|
| Indexing (has entries) | `● Searchable` + progress % + elapsed time + entry count |
| Indexing (no entries) | `Starting indexing...` |
| Ready | `Index: Ready` + entry count + indexing duration |
| Spotlight fallback | Orange `Spotlight temporary search` |
| Search complete | `"query" Xms · N results` |

---

## Key Constants

| Constant | Value | Location |
|----------|-------|----------|
| `DEFAULT_LIMIT` | 300 | Default search result count |
| `SHORT_QUERY_LIMIT` | 100 | Single-char query result limit |
| `MAX_LIMIT` | 1,000 | Maximum result count |
| `BATCH_SIZE` | 10,000 | DB batch write unit |
| `SHALLOW_SCAN_DEPTH` | 6 | Pass 0 max depth |
| `JWALK_NUM_THREADS` | 4 | Parallel worker count |
| `WATCH_DEBOUNCE` | 300ms | File change debounce |
| `RECENT_OP_TTL` | 2s | Rename/trash duplicate prevention |
| `NEGATIVE_CACHE_TTL` | 60s | Zero-result query cache |
| `SPOTLIGHT_TIMEOUT` | 3s | mdfind timeout |
| `SPOTLIGHT_MAX_RESULTS` | 300 | mdfind max results |
| `MUST_SCAN_THRESHOLD` | 10 | Full scan trigger during replay |
| `EVENT_ID_FLUSH_INTERVAL` | 30s | event_id DB save interval |
| `PAGE_SIZE` (FE) | 500 | Frontend page size |
| `rowHeight` (FE) | 28px | Virtual scroll row height |
