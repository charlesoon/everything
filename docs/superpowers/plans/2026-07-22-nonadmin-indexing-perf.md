# Windows Non-Admin Indexing Perf Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement every item of the non-admin indexing perf design — 2× faster non-admin indexing (D1–D4), UI-block removal via CPU management (C1–C3), and memory-peak mitigation (M1–M3).

**Architecture:** Rewrite `win/nonadmin_indexer.rs`'s scan core around the proven `main.rs` fresh-parallel-scan pattern: `jwalk::WalkDirGeneric` builds `CompactEntry` on shared-pool worker threads (`process_read_dir`), scanner threads stream bounded-channel batches to a single collector that feeds both `all_entries` (MemIndex) and a single DB-writer thread. One shared rayon pool capped at `max(2, n_cpus − 2)` with `THREAD_PRIORITY_BELOW_NORMAL` workers. `CompactEntry.dir` becomes `Arc<str>` (interned per read_dir callback).

**Tech Stack:** Rust, jwalk 0.8 (`WalkDirGeneric`, `RayonExistingPool`), windows 0.58 (`SetThreadPriority`), rusqlite, `std::sync::mpsc::sync_channel`.

## Global Constraints

- User CLAUDE.md: **no commits without fresh user approval** — implement + test only; the Commit steps of the standard template are intentionally omitted.
- User CLAUDE.md: zero behavior change for the refactor step (M3); surgical changes elsewhere.
- Existing behavior contracts to preserve:
  - Phase order: shallow home → early MemIndex ≤ 200k → deep home ∥ other roots → full MemIndex → **Ready** → (now-pipelined) DB persist → finalize → free MemIndex → RDCW watcher.
  - `indexing_active` stays `true` until DB upsert completes (finalize runs after release, as today).
  - `status.permission_errors = 0` surfaced (non-admin noise suppressed); `[nonadmin +Xs]` stderr timing logs kept (design §6 verification relies on them).
  - Progress emission throttled at 200ms (`EMIT_INTERVAL`).
  - Daemon path (`run_incremental_index`) untouched; nonadmin indexer is GUI-only (win/mod.rs:165), and the GUI↔daemon handshake (main.rs:7235) completes before `start_windows_indexing` (main.rs:7425), so starting DB writes during the scan (D4) keeps the single-writer invariant.
- Design item ↔ implementation map:
  - **D1** parallel stat+entry creation → `process_read_dir` builds `CompactEntry` into `client_state`; **D3** single `symlink_metadata` per entry (consumer never stats); **M1** streaming into one `all_entries` (no per-scan Vec merge); **M2** `Vec::new()` instead of `with_capacity(2_000_000)`.
  - **D2** one shared pool for all scans; **C1** pool size `max(2, n_cpus − 2)`; **C2** `SetThreadPriority(BELOW_NORMAL)` in `start_handler` (thread CPU priority only — NOT `PROCESS_MODE_BACKGROUND_BEGIN`); **C3** falls out of D2 (no per-root pool churn).
  - **D4** DB writer thread pipelined with scan via bounded channel; SQLite single-writer preserved (exactly one connection thread).
  - **M3** `CompactEntry.dir: Arc<str>`, interned per directory in `process_read_dir`; MFT path shares parent `Arc` via an FRN→`Arc<str>` cache so it does not regress.

---

### Task 1: M3 — `CompactEntry.dir: String` → `Arc<str>` (pure refactor, zero behavior change)

**Files:**
- Modify: `app/src/mem_search.rs` (struct + `to_entry_dto`; add `Debug` derive needed later by jwalk `DirEntryState`)
- Modify: `app/src/win/mft_indexer.rs` (construction sites :232, :306; uses :463, :564, :577-579)
- Modify: `app/src/win/nonadmin_indexer.rs` (construction sites :586, :663; IndexRow conversion :707)

**Interfaces:**
- Produces: `pub struct CompactEntry { pub name: String, pub dir: Arc<str>, ... }` with `#[derive(Debug, Clone)]`. `path()` and `dir_map`/`path_depth` consumers work through `Deref<Target=str>`.

- [ ] **Step 1.1: mem_search.rs** — add `use std::sync::Arc;`, change field to `pub dir: Arc<str>`, derive `Debug`, and in `to_entry_dto` use `dir: self.dir.to_string()`.
- [ ] **Step 1.2: mft_indexer.rs** — dirs: `dir: Arc::from(dir)`; files: build a per-scan `HashMap<u64, Arc<str>>` from `path_cache` once (`Arc::from(path.as_str())` per unique parent FRN) so sibling files share one allocation, then `dir: Arc::clone(parent_arc)`. Fix uses: `e.dir.as_str()` → `&*e.dir`, `dir_stat_cache.get(&entry.dir)` → `.get(&*entry.dir)`, `entry.dir.clone()` (IndexRow) → `entry.dir.to_string()`.
- [ ] **Step 1.3: nonadmin_indexer.rs** — `compact_entry_from_path`: `dir: Arc::from(dir_str)`; scan loop + `background_db_insert` conversions adjusted (`entry.dir.to_string()` for IndexRow).
- [ ] **Step 1.4: Verify** — `cargo test --manifest-path app/Cargo.toml` passes; `cargo check` no warnings.

### Task 2: D2+C1+C2 — shared low-priority scan pool

**Files:**
- Modify: `app/Cargo.toml` (add `"Win32_System_Threading"` to windows features)
- Modify: `app/src/win/nonadmin_indexer.rs`

**Interfaces:**
- Produces: `fn build_nonadmin_scan_pool() -> Arc<jwalk::rayon::ThreadPool>` (size `max(2, n_cpus − 2)`, workers BELOW_NORMAL) and `fn lower_thread_priority()` (also applied to scanner threads).

```rust
fn lower_thread_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    // Thread CPU priority only: process-wide background mode
    // (PROCESS_MODE_BACKGROUND_BEGIN) would also drop I/O priority and slow
    // the scan, conflicting with the speed goal.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

fn build_nonadmin_scan_pool() -> Arc<jwalk::rayon::ThreadPool> {
    let n_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let threads = n_cpus.saturating_sub(2).max(2);
    Arc::new(
        jwalk::rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("nonadmin-scan-{i}"))
            .start_handler(|_| lower_thread_priority())
            .build()
            .expect("failed to build nonadmin scan pool"),
    )
}
```

- [ ] **Step 2.1:** Cargo.toml feature; helpers above; delete `JWALK_THREADS`.
- [ ] **Step 2.2:** All walkers use `jwalk::Parallelism::RayonExistingPool { pool, busy_timeout: None }` (done as part of Task 3's rewrite).

### Task 3: D1+D3+M1+M2 — parallel entry creation, streaming collection

**Files:**
- Modify: `app/src/win/nonadmin_indexer.rs` (replace `ScanResult`/`scan_dir_jwalk`/`SharedProgress`)

**Interfaces:**
- Produces:
  - `struct ScanBatch { entries: Vec<CompactEntry>, scanned: u64, indexed: u64, permission_errors: u64, current_path: String }`
  - `fn stream_scan_dir(root, pool, ignored_roots, ignored_patterns, max_depth: Option<usize>, skip_depth: usize, tx: &SyncSender<ScanBatch>)` — runs on a scanner thread; entry construction happens in `process_read_dir` on pool workers via `WalkDirGeneric::<((), Option<CompactEntry>)>`; batches of `crate::BATCH_SIZE` (or 200ms of counter drift) flushed to `tx`.
  - `fn compact_entry_in_worker(dir: &Arc<str>, file_name: &OsStr, full_path: &Path) -> Option<CompactEntry>` — the single `symlink_metadata` call per entry (D3); `ext` via `Path::extension` (same semantics as today).
  - Collector loop (run thread): drains `Receiver<ScanBatch>`, updates totals, throttled progress emit, `all_entries.extend`, forwards IndexRow batches to `db_tx` (Task 4).
- Consumes: `build_nonadmin_scan_pool()` (Task 2), `CompactEntry` with `Arc<str>` dir (Task 1).

Key semantics:
- `process_read_dir` interns one `Arc<str>` per directory (M3 synergy); children at `depth ≤ skip_depth` are retained for traversal but get no `client_state` (deep-home scan skips depths ≤ 6 exactly as today).
- Consumer skips `entry.depth <= skip_depth`; `client_state == None` past that depth counts a permission error (same as main.rs:3193-3200; permission_errors are log-only in this path).
- Phase 1 = one scanner thread (shallow, `max_depth=6`) + collector on run thread; Phase 2 = two scanner threads (deep home `skip_depth=6`; other-roots serial loop preserving the Program Files→Windows priority order) + same collector. Scanner threads also call `lower_thread_priority()`.
- `Vec::with_capacity(2_000_000)` → `Vec::new()` (M2); per-root result Vecs disappear (M1).
- `SharedProgress` deleted — the collector is the single progress emitter.

- [ ] **Step 3.1:** Write `compact_entry_in_worker` + `stream_scan_dir` + `ScanBatch`.
- [ ] **Step 3.2:** Rewrite `run_nonadmin_index` phases around scanner threads + collector; delete `scan_dir_jwalk`, `ScanResult`, `SharedProgress`.
- [ ] **Step 3.3:** Windows-only unit test for `compact_entry_in_worker` (temp dir: file with extension → name/dir/ext/size populated, dir entry → `is_dir`, no ext).
- [ ] **Step 3.4:** `cargo test` + `cargo check` clean.

### Task 4: D4 — DB upsert pipelined with scan

**Files:**
- Modify: `app/src/win/nonadmin_indexer.rs`

**Interfaces:**
- Produces:
  - `fn start_db_writer(state) -> Result<(SyncSender<Vec<IndexRow>>, JoinHandle<Result<rusqlite::Connection, String>>, i64 /*run_id*/), String>` — opens the connection on the run thread (pragmas, `last_run_id`+1, drop secondary indexes — exactly `background_db_insert`'s prologue), then moves the connection into the single writer thread which loops `upsert_rows` over received batches. **Single writer preserved: only this thread touches the connection.**
  - Collector converts each `ScanBatch` to `Vec<IndexRow>` (`indexed_at`, `run_id` fixed at setup) and `send`s — bounded channel (cap 8) provides backpressure; wall-clock = max(scan, upsert).
- Consumes: Task 3's collector; existing `upsert_rows`, `set_indexing_pragmas`, `background_db_finalize` (unchanged).

Flow changes in `run_nonadmin_index`:
1. Before Phase 1: `start_db_writer`. On error: log, continue scan without DB (MemIndex-only; watcher still started; MemIndex NOT freed — search keeps working).
2. Direct entries (scan_root, C:\ children, per-root self entries) are also forwarded as rows.
3. After full MemIndex + Ready emit (timing unchanged): background thread joins the writer handle → `indexing_active = false` (same point as today: after upsert, before finalize) → `background_db_finalize(conn, …, free_mem_index)` (unchanged) → RDCW watcher (unchanged).
4. `background_db_insert` deleted (superseded by writer thread); its `[nonadmin/bg]` log lines preserved in the writer.

- [ ] **Step 4.1:** Implement `start_db_writer` + collector forwarding + background join/finalize restructure; delete `background_db_insert`.
- [ ] **Step 4.2:** `cargo test` + `cargo check` clean.

### Task 5: Verification

- [ ] **Step 5.1:** `cargo test --manifest-path app/Cargo.toml` — all pass.
- [ ] **Step 5.2:** `npm run lint` (frontend build + cargo check) — no warnings.
- [ ] **Step 5.3:** Self-review against design: every badge item D1-D4/C1-C3/M1-M3 mapped to shipped code; no `RayonNewPool` remains; exactly one `SetThreadPriority` helper; single DB writer.
- [ ] **Step 5.4:** Runtime measurement (user executes, design §6): `[nonadmin +Xs]` Ready / Phase-3 timestamps, peak working set, UI responsiveness during indexing.
