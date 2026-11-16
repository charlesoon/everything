# Repository Guidelines

## Communication
- Answer in the language the user wrote the question in.

## Project Structure
- `app/src/ui/`: native GPUI lifecycle, views and actions.
- `app/src/`: shared Rust engine, daemon, search state and preferences.
- `scripts/bundle_macos.py` and `app/windows/installer.nsi` define packaging; preserve bundle/service identifiers and the embedded LaunchAgent.

## Build and Test
- `cargo run --manifest-path app/Cargo.toml`: run the app.
- `cargo build --release --locked --manifest-path app/Cargo.toml`: release build.
- `cargo test --locked --manifest-path app/Cargo.toml`: regression tests.
- `python3 scripts/bundle_macos.py --dmg`: macOS DMG. `scripts/build-windows.ps1`: Windows installer.
- macOS builds need full Xcode/Metal; scope `DEVELOPER_DIR` to the build when required.

## Rules
- Keep filesystem I/O, DB operations and shell commands off the GPUI foreground executor.
- Keep serializable payloads in `camelCase` (`#[serde(rename_all = "camelCase")]`).
- Put unit tests next to the code in `#[cfg(test)] mod tests`.
- Stress benchmarks are `#[ignore]`d because they scan HOME or open a production index; run them with `--ignored` only when needed.
- Never reset/register a real daemon or change real user preferences from fixture tests.
- Do not commit local databases, logs or build output.
