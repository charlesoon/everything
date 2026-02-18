# Everything

An ultrafast file/folder name search app for macOS. Inspired by [Everything](https://www.voidtools.com/) for Windows, it indexes the entire home directory into SQLite with FTS5, delivering sub-50ms search responses.

## Features

- **Instant search** — Results update as you type (backend p95 < 30ms)
- **Large-scale support** — Handles 500K–1M entries without UI freezes
- **Multiple search modes** — FTS5 prefix match, glob patterns (`*`, `?`), path search (when query contains `/`)
- **File actions** — Open, Reveal in Finder, Copy Path, Move to Trash, inline Rename
- **Real-time sync** — FSEvents watcher automatically reflects file changes
- **Native macOS icons** — System icons via NSWorkspace

## Tech Stack

| Layer | Technology |
|-------|------------|
| Framework | Tauri v2 |
| Backend | Rust |
| Frontend | Svelte 4 |
| Search Engine | SQLite FTS5 |
| File Watcher | FSEvents (notify crate) |
| Directory Traversal | WalkDir / jwalk |

## Getting Started

### Prerequisites

- macOS 11.0+
- [Rust](https://rustup.rs/) (stable)
- [Node.js](https://nodejs.org/) (v18+)
- Xcode Command Line Tools

### Install & Run

```bash
# Install dependencies
npm install

# Dev server (Vite on :1420 + Tauri window)
npm run tauri dev

# Production build (DMG)
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
| `Cmd+Shift+Space` | Global — Activate app / focus search |
| `Enter` | Rename (inline edit) |
| `Cmd+O` | Open |
| `Cmd+Enter` | Reveal in Finder |
| `Cmd+C` | Copy Path |
| `Cmd+Backspace` | Move to Trash |
| `↑` / `↓` | Navigate selection |
| `Shift+Click` | Range select |
| `Cmd+Click` | Toggle select |
| `Cmd+A` | Select all |

## Project Structure

```
src-tauri/src/
  main.rs      # App state, indexer, search, file actions, Tauri commands
  query.rs     # Search query parser (FTS/Glob/Path mode classification)

src/
  App.svelte   # Single-component UI (search, virtual scroll, context menu)
  main.js      # Svelte mount point

doc/
  spec.md      # Detailed design spec (Korean)
```

## Architecture

1. On launch, scans all of `$HOME` via WalkDir and batch-upserts into SQLite
2. FTS5 virtual table stays in sync automatically via triggers
3. User input → Rust queries FTS5 MATCH + LIKE union → returns results
4. If FTS results are sparse, a background live scan supplements them
5. FSEvents watcher detects file changes for incremental updates
