# Windows admin service writer — verification runbook

Implements `design-windows-service-writer.html` (Phases 0–3). This runbook covers
what can't be verified headlessly: the macOS build, and the elevated
service/installer/pipe/ACL paths (§7 scenarios ①–㉙). Headless status at time of
writing: `cargo check` 0 warnings; `cargo test` 95 pass (7 new: S1–S7 + pipe),
4 pre-existing Unix-path failures in `rescan.rs`/`mcp_server.rs` (unrelated).

## 0. Builds

```bash
# Windows backend (this dev box): 0 warnings expected
cargo check --manifest-path src-tauri/Cargo.toml
cargo test  --manifest-path src-tauri/Cargo.toml    # 4 pre-existing Unix-path failures only

# Frontend
npm run build

# macOS cross-platform check (run on a Mac / CI — the Windows box can't verify it).
# Confirms the cfg-gated shared edits (build_app_state_with_config, DB_READONLY
# gate in db_connection, MCP read-only open, gated spawn_detached/handshake fns).
cargo check --manifest-path src-tauri/Cargo.toml
```

## 1. Phase 0 — MFT/USN fixes (elevated GUI, no service needed)

Run today's admin GUI build (before the service exists) to validate S1–S7:

- **S1** MFT error propagation: on a volume where `FSCTL_ENUM_USN_DATA` errors mid-scan, the index must not be marked `index_complete` (check `meta`), and the stale-row delete must not run.
- **S4** cursor: create files, let them index; kill the process mid-batch; on restart confirm no changes are lost (resume = `max(meta win_last_usn, data\usn.cursor)`). While idle, confirm `usn.cursor` advances but `PRAGMA data_version` does **not** (scenario ㉑).
- **S6** directory rename: `ren C:\test\dir C:\test\dir2` with files under it → within ~seconds, old-prefix rows gone **and** new-prefix children present (scenario ④).
- **S2** journal loss: `fsutil usn deletejournal /d C:` → watcher exits + a full rescan is requested, not an infinite empty-retry loop (scenario ⑪).

## 2. Phase 1 — the service (elevated)

```powershell
cargo build --release --manifest-path src-tauri/Cargo.toml
$exe = "$PWD\src-tauri\target\release\everything.exe"
# Data root + ACL (installer normally does this — see §3):
New-Item -ItemType Directory -Force "C:\ProgramData\com.everything.app\data","C:\ProgramData\com.everything.app\config" | Out-Null
icacls "C:\ProgramData\com.everything.app\data" /inheritance:r /grant:r "NT AUTHORITY\SYSTEM:(OI)(CI)F" "BUILTIN\Administrators:(OI)(CI)F" "BUILTIN\Users:(OI)(CI)RX"
icacls "C:\ProgramData\com.everything.app\config" /inheritance:r /grant:r "NT AUTHORITY\SYSTEM:(OI)(CI)F" "BUILTIN\Administrators:(OI)(CI)F" "BUILTIN\Users:(OI)(CI)M"
sc.exe create EverythingIndexer binPath= "`"$exe`" --service" start= demand DisplayName= "Everything Indexer"
sc.exe start EverythingIndexer; Start-Sleep 3; sc.exe query EverythingIndexer   # expect RUNNING (fast — D2)
Get-Content "C:\ProgramData\com.everything.app\data\logs\service.log" -Tail 30    # MFT scan progress
```

Validate:
- **RUNNING quickly**, indexing continues after (D2). `index.db` grows in `data\`.
- **Pipe**: from a normal (non-elevated) session, `[System.IO.Pipes.NamedPipeClientStream]` or the GUI status poll gets a `status` reply; remote connection is rejected (scenario ⑮).
- **DACL self-grant (D2)**: after first start, a non-elevated `sc query EverythingIndexer` / `sc start` works without UAC (INTERACTIVE was granted).
- **STOP** preempts a scan within seconds (S7): `sc.exe stop EverythingIndexer` mid-scan → STOPPED promptly; log shows cancellation.
- **Planting (㉖)**: pre-create `...\data` as a junction to elsewhere → service refuses to start + Application event-log error (D1).
- Cleanup: `sc.exe stop EverythingIndexer; sc.exe delete EverythingIndexer`.

## 3. Phase 2 — installer (build + elevated install)

```bash
npm run tauri build        # produces the perMachine NSIS installer
```
- **Clean install** (scenario ①): installer elevates (UAC), creates `data\`(Users RX)/`config\`(Users M), registers + starts the service, confirms RUNNING. `icacls` on both dirs matches §2.
- **Silent (④/②)**: `installer.exe /S` from an elevated shell → unattended; from a non-elevated interactive shell → one UAC prompt; non-interactive non-elevated → fails with a non-zero exit (expected).
- **Upgrade (⑯)**: install over a prior per-user build → old service stopped/updated, old `%APPDATA%\com.everything.app` cleaned.
- **Uninstall (⑦)**: `data\` removed, `config\` **preserved**.
- **voidtools safety (㉙)** — see Deferred below (needs the template override).

## 4. Phase 3 — read-only GUI / MCP

- **GUI read-only**: launch the GUI (non-elevated) with the service running. Search works; status bar tracks the service (pipe status re-emitted as `index_*`). Editing a file (rename/trash) reflects within ~1–2s via `data_version` (scenario ③). Any GUI write attempt errors instead of corrupting (DB_READONLY).
- **Health banners (D5)**: stop the service → "starting…" banner + auto-start attempt; `sc delete` it → "not installed" banner with a **서비스 등록** button that elevates `--register-service` (scenario ⑥).
- **`--register-service` guard (⑨)**: run the portable/dev exe's `--register-service` → refused (binary not under Program Files).
- **MCP**: `everything --mcp` serves search from the ProgramData index read-only, spawns **no** daemon on Windows; with the old per-user exe deleted, a stale MCP registration fails explicitly (scenario ㉘) — recover with `everything --register-mcp`.
- **reset via pipe**: GUI reset → `reset_index` over the pipe, honoring the 10-min cooldown (scenario ③/DoS).

## Deferred / needs follow-up

- **NSIS `template` override** (§3.2): path-scoped process kill (protect voidtools `Everything.exe`) and multi-profile old-binary sweep. `hooks.nsh` documents the spots.
- **GUI first-run HKCU cleanup** (§5②): delete leftover per-user Uninstall keys/shortcuts — needs the old per-user NSIS product key path.
- **daemon Q3** (`run_windows_resident` deletion): currently retained; a stray `--daemon` on Windows is neutralized by the service's `daemon.lock` + the read-only data dir, so this is dead-code cleanup, not a correctness gap.
- `start_windows_indexing` retained under `#[allow(dead_code)]` to keep its helpers compiled; remove with the daemon cleanup.
