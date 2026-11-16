# CLAUDE.md

## Communication

- Answer in the language the user wrote the question in.

## Project

Everything: a file/folder name search app for macOS and Windows. Rust + GPUI 0.2.2 +
gpui-component 0.5.1 + SQLite (LIKE + FTS5 trigram). UI messages and specs are in Korean.

## Commands

```bash
cargo run --manifest-path app/Cargo.toml
cargo build --release --locked --manifest-path app/Cargo.toml
cargo check --locked --manifest-path app/Cargo.toml
cargo test --locked --manifest-path app/Cargo.toml
python3 scripts/bundle_macos.py --dmg      # macOS DMG (scripts/build_dmg.sh for Developer ID releases)
./scripts/build-windows.ps1                # Windows binary + NSIS installer
```

## Invariants

- The GUI never writes `index.db`. The macOS login-item daemon (`daemon.rs`) and the
  Windows service are the only writers; the GUI reads the index and talks to them over
  IPC (`daemon_ipc.rs`, `win/service_pipe.rs`).
- The daemon must not build the index or touch TCC-protected paths before the user's
  onboarding decision is recorded.
- App startup never writes Claude Code/Codex configs. MCP registration happens only via
  `everything --register-mcp`.
- macOS scan threads: only `QOS_CLASS_BACKGROUND` keeps work on E-cores (`UTILITY` is not
  a throttle). Never combine QoS with `setiopolicy_np(IOPOL_SCOPE_THREAD, …)`; it unsets
  the QoS class. See `app/src/mac/sched.rs`.
- Keep filesystem I/O, DB work and shell commands off the GPUI foreground executor.
- Bumping `PRAGMA user_version` (currently 7) clears all entries and re-indexes.

## Conventions

- Errors: `AppResult<T> = Result<T, String>`, shown by the native UI.
- DTOs use `#[serde(rename_all = "camelCase")]`.
- Platform code uses `#[cfg(target_os = "macos")]` / `#[cfg(target_os = "windows")]`.

## Docs

- Spec: `doc/spec.md`, `doc/spec_KR.md`
- Architecture: `doc/architecture.md`, `doc/gpui-architecture.md`
- UI test cases: `doc/gpui-test-cases.md`
- macOS daemon verification: `docs/macos-daemon-writer-verification.md`
