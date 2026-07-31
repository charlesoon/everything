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

- **`main.rs`** (~10,000 lines) — Core backend: app state (`AppState`), SQLite schema init, indexer (jwalk), search (LIKE-based multi-mode), file actions (open/reveal/trash/rename/quick_look), Tauri command handlers. Contains all `#[tauri::command]` functions and event emissions.
- **`daemon.rs`** — Resident indexer daemon (`everything --daemon`). On macOS it is the **sole index.db writer**, supervised by launchd (login-item LaunchAgent, `KeepAlive.SuccessfulExit=false`: exit 0 = dormant until next login/kickstart, exit 1 = relaunch from disk). Lifecycle (design §4): Starting → WaitingOnboarding (first-install gate — no build, no TCC access until the user decides; existing installs auto-pass as `granted-implicit`) → Building → Ready → Quiescing (shared graceful exit: cancel pass ≤2s → watcher join ≤3s → `wal_checkpoint(TRUNCATE)` best-effort). SIGTERM (logout/shutdown/unregister) quiesces. The 5s supervision tick heals `index_complete`, restarts a dead watcher, probes FDA (60s while missing; rising edge → forced catchup), watches exe identity (replaced → exit 1; missing 3 ticks → dormant — Finder's replace-on-update reads as replace), and in `--lease` mode retires once all renewable lease users (GUI bridge/MCP, 60s renewals, TTL 5min) age out and gui.lock is free. `ensure_daemon_running()` is the single revival gate: registered → `launchctl kickstart`; opt-out/OS-off → lease spawn; unregistered (dev) → detached spawn. Linux keeps the legacy yield-to-GUI polling daemon.
- **`daemon_ipc.rs`** (macOS) — UDS control socket `<data dir>/daemon.sock` (0600 + `getpeereid` same-uid check, newline-delimited JSON, one request/connection — port of `win/service_pipe.rs`): `status` / `reset_index` (600s cooldown) / `restart` (exit 1) / `disable` (exit 0) / `attach_lease`. The server opens **before** the startup build so control always answers; paths over the 104-byte `sockaddr_un` limit fall back to `$TMPDIR` with a `daemon.sock.path` sidecar.
- **`query.rs`** — Search query parser: classifies input into `SearchMode` variants (`Empty`, `NameSearch`, `GlobName`, `ExtSearch`, `PathSearch`). Handles glob-to-LIKE conversion and LIKE escaping. Has unit tests.
- **`rescan.rs`** — Streaming subtree rescan (`rescan_subtree` + `SubtreeDiff`): diffs a directory tree against the DB with a hash-compacted snapshot (~24 B/row), upserts only new/changed rows in batches, deletes vanished rows. Used by the MustScanSubDirs handler, directory rename, and (via `SubtreeDiff`) catchup workers. Has unit tests.
- **`fd_search.rs`** — jwalk-based live filesystem search (5s timeout, max 5000 entries)
- **`mem_search.rs`** — In-memory compact entry search (`MemIndex`): built during MFT/WalkDir scan for instant results while DB upsert runs in background. Uses binary search, ext/dir maps, and time-budgeted contains matching.
- **`gitignore_filter.rs`** — Lazy .gitignore discovery and matching (depth 3, `ignore` crate)
- **`mcp_server.rs`** — MCP stdio server (`everything --mcp`): serves a `search` tool for AI agents straight from index.db (read-only, `query_only` pragma), reusing `run_db_search` — works with the GUI app closed. Can register the binary into Claude Code (`~/.claude.json`) and Codex (`~/.codex/config.toml`) via the explicit `everything --register-mcp` flag only — app startup never writes to those configs; registration is idempotent and preserves existing config content.
- **`mac/`** — macOS-specific: FSEvents watcher (direct fsevent-sys binding; `PathEvent` carries the ItemRenamed/ItemIsDir flags for S5), Spotlight search fallback (mdfind), `launch_agent.rs` (SMAppService login-item registration + plist-bytes marker self-heal + `launchctl kickstart`; the agent plist ships in the bundle at `Contents/Library/LaunchAgents/com.everything.app.daemon.plist`), `prefs.rs` (`defaults` domain `com.everything.app`: `onboardingDecision`/`daemonOptOut`/`onboardingFirstSeen` — user intent survives data-dir deletion), `bridge.rs` (read-only GUI ↔ daemon status bridge), `cpu_monitor.rs` (field CPU/power self-accounting for fan complaints: `proc_pid_rusage` per-QoS-class CPU time + `task_info` `task_energy` joules; a watchdog thread in GUI+daemon logs an episode to the diag log when the process sustains ≥5W / ≥2 P-band cores — onset with per-class split, backed-off heartbeats, end summary — and every index pass / fresh finalize appends its `| cpu …` accounting to the existing diag lines. Note: per-class billing follows *effective* QoS incl. turnstile boosts — never `join()` a thread you're measuring mid-burn), `probe_full_disk_access` (1-byte TCC.db read)
- **`win/`** — Windows-specific: MFT indexer (NTFS metadata scan), non-admin WalkDir fallback (`nonadmin_indexer.rs`), USN journal watcher, ReadDirectoryChangesW fallback, Shell icon loading (IShellItemImageFactory), native Explorer context menu, offline catchup (Windows Search / mtime scan)

### Frontend (Svelte 5) — `src/`

- **`App.svelte`** (~2800 lines) — Single-component UI: search input, virtual-scrolled result table (26px row height), inline rename, context menu, keyboard shortcuts, icon cache (max 500), status bar, theme toggle, FDA banner (macOS). Communicates with backend via `invoke()` (Tauri IPC). Platform-specific behavior via `get_platform()`.
- **`search-utils.js`** — Search debounce (200ms leading+trailing), viewport-preserve logic for scroll position during re-search.
- **`main.js`** — Svelte mount point.

### Data Flow

0. **macOS process split (daemon-writer, design `design-macos-daemon-writer.html`)**: the login-item daemon owns *every* write below — schema init, fresh/catchup passes, FSEvents watcher, `.pathindexing` reconcile, FTS/VACUUM maintenance, `purge_ignored_entries`. The GUI starts `set_db_readonly()` + `is_writer=false` (it still holds `gui.lock` for second-instance detection, the lease-liveness check, and pre-flip field daemons) and runs `mac/bridge.rs`: 1s UDS status poll re-emitting the existing `index_*` events (change-deduped; a non-fresh Building maps to a `"Catchup: …"` message = `isCatchup`), `PRAGMA data_version` watch on a dedicated connection (on change: invalidate negative/fd caches, drop the search pool — daemon resets swap the entries table — re-derive `fts_ready`, emit `index_updated`), `service_health` outage classification with one `ensure_daemon_running` per outage, and `attach_lease` renewals while auto-start is off. GUI rename/trash are filesystem-only; the daemon's watcher reflects them (directory renames via the S5 subtree rescan). Search rides the Spotlight fallback until the daemon's DB is readable. Bench mode (`EVERYTHING_BENCH`) keeps the in-process writer path.
1. App start → scan filesystem → batch upsert into `entries` table (SQLite WAL mode)
   - macOS fresh index: parallel jwalk scan of `$HOME` (workers → bounded `sync_channel(8)` → single writer, sorted `INSERT OR IGNORE` batches — backpressure caps queued-batch memory); secondary indexes + FTS rebuild + ANALYZE are deferred to the background finalizing thread (`finalize_fresh_index` with `temp_store=FILE`, `fts_dirty` meta flag heals crashes)
   - macOS restart catchup: single parallel pass; each worker snapshots its root's rows into a hash-compacted `SubtreeDiff` over its own read connection, upserts only new/changed rows, and deletes vanished rows via snapshot leftovers (no per-row run_id stamping)
   - The finalizing thread ends with storage/memory maintenance: threshold-gated `VACUUM` (free pages ≥ 25% and ≥ 100MB), `wal_checkpoint(TRUNCATE)`, `shrink_memory`, and `malloc_zone_pressure_relief` (returns freed heap to the OS)
   - macOS scan cost control (`mac/sched.rs`): scan threads inherit the Tauri main thread's `USER_INTERACTIVE` QoS unless told otherwise, which pins them to P-cores at full clock. Every scan thread calls `apply_scan_scheduling(role)`, and the role — not the pass's urgency — picks the class, because **`QOS_CLASS_UTILITY` is not a throttle**: measured on a 6P+12E Mac over a 1.39M-entry `$HOME`, `UTILITY` draws 5.5 avg cores vs `USER_INTERACTIVE`'s 5.6 (the scheduler runs it on P-cores at full clock), while `QOS_CLASS_BACKGROUND` is the only class that confines work to the E-cluster (3.8 avg cores, ~2x wall). So `ScanRole::Walk` (rayon pool via `start_handler` + walk workers — the whole thread multiplier, and syscall-bound work E-cores handle well) is **always** `BACKGROUND`, while `ScanRole::FreshWriter` (the *single* SQLite writer/finalizer thread of a fresh pass, which is what the pass's wall time is actually made of) keeps `UTILITY` and one P-core; `ScanRole::IdleWriter` (catchup writer, subtree rescans) is `BACKGROUND`. Thermal pressure / Low Power Mode pushes even `FreshWriter` down. **Never pair any of this with `setiopolicy_np(IOPOL_SCOPE_THREAD, …)`** — it silently unsets the QoS class and permanently opts the thread out (see the module docs). `build_scan_pool` sizes the pool at `n_cpus/2` (2–6) plus a quarter as many walk workers, shrunk further under pressure — that sizing bounds oversubscription and per-worker memory, not CPU draw (the class does that). `ScanThrottle` duty-cycles the walk if the machine heats up mid-pass. `EVERYTHING_INDEX_FULL_SPEED=1` (implied by `EVERYTHING_BENCH`) restores the old oversubscribed, un-downgraded behavior. The scan-start log line reports `writer_qos=` and `walk_qos=` so a regression to the P-cores is visible in a field log.
   - Windows: NTFS MFT scan of `C:\` → builds MemIndex for instant search → background DB upsert
2. User types → Svelte calls `search` command → Rust checks MemIndex first, then queries SQLite (LIKE + FTS5 trigram multi-mode) → returns `SearchResultDto { entries, modeLabel, totalCount, totalKnown }`
3. If results are sparse, a live scan (jwalk/fd_search) runs in a background thread
4. File watcher detects changes → upsert/delete affected paths over a persistent write connection (`watcher_conn`)
   - macOS: FSEvents (direct fsevent-sys, supports event ID replay), owned by the daemon. One stream watches `$HOME` plus canonicalized `.pathindexing` extra roots (`/tmp` → `/private/tmp`; event paths are remapped back to the stored prefix — the home root is canonicalized/remapped too for symlinked homes). The stream is rebuilt with event-id continuity when `.pathindexing` changes. `MustScanSubDirs` (kernel event-queue overflow) queues a streaming `rescan_subtree` on a single-flight background thread — change-detected, batch-bounded memory, per-path 5-min cooldown. **S5**: external directory renames deliver no per-child events, so the watcher queues the same subtree rescan with a short 7s cooldown class for (a) dirs flagged ItemRenamed+ItemIsDir that still exist, and (b) freshly inserted dirs with children on disk but no DB descendants (probe-capped per batch; overlapping triggers keep the strictest cooldown)
   - Windows: USN Change Journal → ReadDirectoryChangesW fallback

### Key Design Decisions

- **Search modes:** Query containing `/` or `\` → path search; containing `*` or `?` → glob-to-LIKE; simple `*.ext` → extension lookup; otherwise → 3-phase name search (exact → prefix → contains). The contains phase and non-name sorts use the FTS5 trigram index (queries ≥ 3 chars, gated by `fts_ready`); globs with a leading wildcard use an FTS literal-run prefilter + LIKE verify.
- **MCP server:** the app binary doubles as an MCP server (`--mcp` flag, JSON-RPC over stdio, newline-delimited). The DB-search core is shared via `run_db_search` (no AppState), so the MCP process searches the same index without the app running. Registration into Claude Code/Codex configs is opt-in via `everything --register-mcp`; app launch never writes to those configs.
- **macOS daemon-writer:** the GUI never writes index.db. First-run onboarding (App.svelte modal) records `onboardingDecision` via `defaults` (survives data-dir deletion — G14); the daemon's gate waits on it (no build, no TCC access before consent), with a 7-day + `mcp.activity` escape hatch for MCP-only installs (Q7). The auto-start toggle implements §5.2: OFF records the opt-out first (the daemon start gate checks the record OR the SMAppService user-off state), then IPC `disable`, then unregister; while off, GUI/MCP keep a `--lease` daemon alive with 60s `attach_lease` renewals. Registration self-heal re-registers only when the embedded plist bytes differ from `agent_plist.marker` (re-registering churns the daemon and can re-fire the BTM notification — G7). Releases must be built with `scripts/build_dmg.sh` (Developer ID + notarization): SMAppService needs a signed, non-translocated bundle, and ad-hoc signing would invalidate FDA on every update. Verification: `scripts/verify_macos_daemon.sh` + `docs/macos-daemon-writer-verification.md`.
- **Search connections:** pooled warm read connections (`search_conn_pool`, max 3, 1GB mmap, cached prepared statements) reused across keystrokes; total counts use FTS-only `COUNT` (no join) where possible. The pool is only cleared on `reset_index`, not by watcher updates.
- **Sort:** Backend SQL `ORDER BY` (name/mtime/size × asc/desc), not relevance. Relevance sorting applied only on first page (offset=0) for name sort (decorate–sort–undecorate; ranks computed once per row).
- **Scan failures (`ScanFailures`):** the index pass splits filesystem observation failures into `denied` (EACCES/EPERM — on macOS, TCC) and `io` (everything else non-`NotFound`), and **does not count** mid-walk `NotFound` at all (a directory deleted between its parent's `readdir` and the walk reaching it is normal). Denied paths are collapsed to prefixes 2 components below the scan root (so several hundred `~/Library/Containers/<app>/Data` denials become one `~/Library/Containers`) and persisted to the `denied_prefixes` meta key, capped at 64. The FDA rising edge then rescans exactly those prefixes via `rescan_subtree` instead of running a second full 1M-entry catchup pass; it falls back to a full pass only when nothing was recorded. The status-bar line is built by `scan_failure_message` and names Full Disk Access when the probe says it is missing — the old rendering was a bare count in two places ("321 permission errors" plus "321 permission/access error(s) occurred.") with no cause and no action.
- **Recent ops cache:** 2-second TTL prevents watcher from re-processing app-initiated rename/trash operations
- **Icons:** requests are batched — the frontend collects a viewport's cache misses into one `get_file_icons` call (one batch in flight; the rest coalesce into the next), and `resolve_file_icons` renders the whole batch in one `render_icon_pngs` pass (a second pass only when a file yields no icon of its own and needs its extension icon). This matters on macOS, where each `swift -e` is a full compiler run (~1s), so per-row rendering piled processes onto the first index; `run_swift_png` also serializes spawns. Batch entries ride argv as `f:<path>` / `t:<type>` (tagged so newlines and leading dashes in paths are safe) and come back as length-prefixed frames. Icons are drawn into an explicit 32×32 bitmap (16pt@2x, what Finder shows) — serializing an `NSImage`'s own representation yields 1024×1024 PNGs, ~100× the bytes, for a 26px row. macOS: NSWorkspace, cached by extension (or path for `.app`), 20 common extensions prewarmed once indexing settles. Windows: IShellItemImageFactory for any real path + SHGetFileInfo by extension, in-process so the batch is just a loop
- **Virtual scroll:** Fixed 26px row height, OverlayScrollbars, renders only visible rows ± buffer
- **Indexing root:** macOS: `$HOME`, Windows: `C:\`. Skips `.git`, `node_modules`, `DerivedData`, `.build` suffixes, platform-specific noisy directories
- **Context menu:** macOS: custom frontend menu. Windows: native Explorer context menu via Shell API, actions returned via `context_menu_action` event
- **Enter key:** Opens the selected file(s) on both platforms. Cmd/Ctrl+Enter reveals in Finder/Explorer. F2 starts rename on both platforms.
- **MemIndex (Windows):** In-memory index built during MFT/WalkDir scan provides instant search before DB is populated. Freed after background DB upsert completes.
- **Windows fallback chain:** MFT scan → USN watcher → non-admin WalkDir → RDCW watcher

### Tauri IPC Commands

`get_index_status`, `get_home_dir`, `get_platform`, `start_full_index`, `reset_index`, `search`, `fd_search`, `open`, `open_with`, `reveal_in_finder`, `show_package_contents` (macOS), `copy_paths`, `copy_files` (macOS), `move_to_trash`, `rename`, `get_file_icons`, `show_context_menu`, `quick_look` (macOS), `check_full_disk_access` (macOS), `open_privacy_settings` (macOS), `open_login_items_settings` (macOS), `get_onboarding_state` (macOS), `record_onboarding_decision` (macOS), `register_daemon` (macOS), `set_daemon_autostart` (macOS), `open_pathignore`, `open_pathindexing`, `restart_app`, `register_service_elevated` (Windows), `set_native_theme`, `mark_frontend_ready`, `frontend_log`

CLI flags (handled before Tauri boots): `--mcp`, `--daemon` (`--lease` variant for opt-out users), `--register-mcp`, `--register-daemon` / `--unregister-daemon` (macOS login item; register starts the daemon immediately), `--uninstall` (macOS: unregister + clean runtime files, keeps the DB), `--version`, `--help`. Windows adds `--service` / `--register-service`.

### Backend Events (→ Frontend)

`index_progress`, `index_state` (includes `isCatchup` — produced by the macOS bridge for non-fresh daemon builds), `index_updated`, `context_menu_action` (Windows), `service_health` (Windows service / macOS daemon outage states: `not_installed`, `stopped`, `starting`, `unresponsive`, `version_mismatch`, plus macOS `not_registered`, `approval_required`, `disabled`), `pathignore_changed`, `pathindexing_changed`

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
