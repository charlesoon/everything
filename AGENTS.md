# Repository Guidelines

## Project Structure & Module Organization
- `src-tauri/src/ui/` contains the native GPUI lifecycle, views and actions.
- `src-tauri/src/` contains the shared Rust engine, daemon, search state and preferences. Keep parsing/search helpers in focused modules such as `query.rs`.
- `scripts/bundle_macos.py` and `src-tauri/windows/installer.nsi` define native packaging; preserve bundle/service identifiers and the embedded LaunchAgent.
- `doc/` stores specs, TC results and performance evidence.
- Build artifacts in `src-tauri/target/`, `dist/` and `.artifacts/` stay untracked.

## Build, Test, and Development Commands
- `cargo run --manifest-path src-tauri/Cargo.toml`: run the native app.
- `cargo build --release --locked --manifest-path src-tauri/Cargo.toml`: build GPUI release.
- `npm run dev`, `npm run build`, `npm run lint`: optional aliases for Cargo run/build/check, with no npm dependencies.
- `npm test`: compile Rust tests. `cargo test --manifest-path src-tauri/Cargo.toml`: run regression tests.
- `python3 scripts/bundle_macos.py --dmg`: build/sign/package a macOS DMG.
- `scripts/build-windows.ps1`: build the Windows binary and NSIS installer.
- macOS needs full Xcode/Metal; scope `DEVELOPER_DIR` to the build when required.

## Coding Style & Naming Conventions
- Rust: follow `rustfmt` defaults; use `snake_case` for functions/modules and `CamelCase` for types.
- Keep serializable payloads in `camelCase`, for example `#[serde(rename_all = "camelCase")]`.
- Keep filesystem I/O, DB operations and shell commands off the GPUI foreground executor.
- Preserve generation checks for search responses and use the coalescing EventSink for status updates.

## Testing Guidelines
- Put backend and UI model unit tests near the code under `#[cfg(test)] mod tests`.
- Name tests by behavior, for example `path_search_dir_only`.
- Run `npm test` before pushing. Run full `cargo test --manifest-path src-tauri/Cargo.toml` when touching parser, indexing or DB logic.
- Legacy stress benchmarks are ignored by default: they scan HOME or open a production index. Run them explicitly with `--ignored` only when needed.
- Verify actual GPUI flows with an isolated fixture and the opt-in `ui-automation` feature; see `doc/gpui-test-cases.md`.
- Never reset/register a real daemon or change actual user preferences from fixture tests.

## Commit & Pull Request Guidelines
- Current history is minimal (`Initial commit`), so keep commit subjects short, clear, and imperative.
- Use one logical change per commit and explain behavioral impact in the body when needed.
- PRs should include: purpose, linked issue (if available), validation steps/outputs, and screenshots for UI changes.
- Highlight changes to permissions, indexing scope, or database behavior explicitly.

## Security & Configuration Tips
- Avoid hardcoded user-specific paths; derive them from runtime APIs/config.
- Do not commit local databases, logs, or generated build output.
