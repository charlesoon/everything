# Everything

An ultrafast file/folder name search app for macOS and Windows. Inspired by [Everything](https://www.voidtools.com/) for Windows, it indexes the filesystem into SQLite, delivering sub-50ms search responses.

## Features

- **Instant search** — Results update as you type (backend p95 < 30ms)
- **Large-scale support** — Handles 500K–1M entries without UI freezes
- **Multiple search modes** — Name prefix/contains, glob patterns (`*`, `?`), extension search, path search (when query contains `/` or `\`)
- **File actions** — Open, Reveal in Finder/Explorer, Copy Path, Copy Files, Move to Trash, inline Rename, Quick Look (macOS)
- **Real-time sync** — File system watcher automatically reflects file changes
- **Native icons** — macOS: NSWorkspace system icons, Windows: IShellItemImageFactory per-file icons
- **Native context menu** — Windows: Explorer shell context menu integration
- **In-memory search** — Instant results during initial indexing (Windows)
- **Theme toggle** — Dark/light mode with system preference sync

## Platform Support

| Feature | macOS | Windows |
|---------|-------|---------|
| Indexing | jwalk incremental scan (`$HOME`) | NTFS MFT scan (`C:\`) → WalkDir fallback |
| File watcher | FSEvents (direct binding) | USN Change Journal → ReadDirectoryChangesW fallback |
| Icons | NSWorkspace (extension-based) | IShellItemImageFactory + SHGetFileInfo |
| Context menu | Custom frontend menu | Native Explorer context menu (Shell API) |
| Global shortcut | Cmd+Shift+Space | — |
| Quick Look | Space key | — |
| Full Disk Access | FDA banner check | — |

## Tech Stack

| Layer | Technology |
|-------|------------|
| Framework | Tauri v2 |
| Backend | Rust |
| Frontend | Svelte 5 |
| Database | SQLite (WAL mode, LIKE-based search) |
| macOS watcher | fsevent-sys |
| Windows indexer | Win32 MFT/USN APIs, notify crate (RDCW fallback) |

## Getting Started

### Prerequisites

**macOS:**
- macOS 11.0+
- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) (v18+)
- Xcode Command Line Tools

**Windows:**
- Windows 10/11
- [Rust](https://rustup.rs/) (stable, MSVC toolchain)
- [Node.js](https://nodejs.org/) (v18+)
- Visual Studio Build Tools (C++ workload)

### Install & Run

```bash
# Install dependencies
npm install

# Dev server (Vite on :1420 + Tauri window)
npm run tauri dev

# Production build
npm run tauri build
```

### Testing

```bash
# Rust tests
cargo test --manifest-path src-tauri/Cargo.toml

# Lint (frontend build + cargo check)
npm run lint
```

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Cmd+Shift+Space` | Global — Activate app / focus search (macOS) |
| `Enter` | Open (Windows) / Rename (macOS) |
| `F2` | Rename (inline edit) |
| `Space` | Quick Look (macOS) |
| `Cmd+O` / `Ctrl+O` | Open |
| `Cmd+Enter` / `Ctrl+Enter` | Reveal in Finder/Explorer |
| `Cmd+C` / `Ctrl+C` | Copy Path |
| `Cmd+F` / `Ctrl+F` | Focus search input |
| `Cmd+Backspace` / `Delete` | Move to Trash |
| `↑` / `↓` | Navigate selection |
| `Shift+Click` | Range select |
| `Cmd+Click` / `Ctrl+Click` | Toggle select |
| `Cmd+A` / `Ctrl+A` | Select all |

## Project Structure

```
src-tauri/src/
  main.rs          # App state, indexer, search, file actions, Tauri commands
  query.rs         # Search query parser (Glob/Path/Name/Ext mode classification)
  fd_search.rs     # jwalk-based live filesystem search
  mem_search.rs    # In-memory compact entry search (MemIndex)
  gitignore_filter.rs  # .gitignore rule matching

  mac/             # macOS-specific modules
    fsevent_watcher.rs   # Direct FSEvents binding
    spotlight_search.rs  # mdfind-based Spotlight fallback

  win/             # Windows-specific modules
    mft_indexer.rs       # NTFS Master File Table scan
    nonadmin_indexer.rs  # WalkDir fallback (non-admin)
    usn_watcher.rs       # USN Change Journal monitor
    rdcw_watcher.rs      # ReadDirectoryChangesW fallback
    search_catchup.rs    # Offline sync (Windows Search / mtime scan)
    icon.rs              # Shell icon loading (IShellItemImageFactory)
    context_menu.rs      # Native Explorer context menu
    volume.rs            # NTFS volume handle operations
    path_resolver.rs     # FRN-to-path resolution
    com_guard.rs         # COM lifecycle management

src/
  App.svelte       # Single-component UI (search, virtual scroll, context menu)
  main.js          # Svelte mount point
  search-utils.js  # Search debounce & viewport-preserve utilities

doc/
  architecture.md  # Detailed architecture documentation
  architecture_KR.md # Architecture documentation (Korean)
  spec.md          # Design spec (English)
  spec_KR.md       # Design spec (Korean)
```

## Architecture

### macOS
1. On launch, scans `$HOME` via jwalk and batch-upserts into SQLite
2. FSEvents watcher detects file changes for incremental updates
3. Supports event ID replay on restart (skip full scan if clean)
4. Spotlight (mdfind) used as search fallback during initial indexing

### Windows
1. On launch, enumerates NTFS Master File Table for near-instant full indexing
2. Builds in-memory MemIndex for instant search while DB upsert runs
3. USN Change Journal monitors file changes with zero-syscall path resolution
4. Falls back to WalkDir (non-admin) or ReadDirectoryChangesW if MFT/USN unavailable
5. Offline catchup via Windows Search service or mtime-based scan on restart

### Common
1. User input → Rust queries MemIndex or SQLite (LIKE-based, multi-mode) → returns results
2. If results are sparse, a background live scan (jwalk) supplements them
3. Relevance sorting: exact match > prefix > contains > path match
