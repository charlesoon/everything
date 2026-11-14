# Everything

Native file and folder search for macOS and Windows, built with Rust and GPUI.
The search engine uses the existing SQLite v7 index with LIKE and FTS5 trigram
queries. The macOS daemon and Windows service own indexing; the GUI reads it.

## Features

- Name, glob, extension and path searches; Korean and spaces in file names
- Virtual result table, sorting, adjustable columns and multiple selection
- Open, Reveal, Copy Path/Files, Trash confirmation, inline Rename and Quick Look
- System light/dark appearance, preserving the Tauri UI and saved window/column preferences
- Full Disk Access onboarding, background indexer health and auto-start controls
- The same executable supports `--mcp`, `--daemon` and Windows service commands

The UI renders directly through GPUI. No browser runtime or JavaScript assets
are bundled. `src-tauri/` retains its historical directory name for stable paths.

## Build and run

Use the Rust version in `rust-toolchain.toml` (1.97.1).

macOS requires macOS 13+, full Xcode and the Metal compiler. If the compiler is
missing, run `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcodebuild -downloadComponent MetalToolchain`.
Windows requires Windows 10/11 x64, the Rust MSVC toolchain, Visual Studio C++
build tools and a DirectX 11 compatible GPU. NSIS 3 is needed for installers.

```sh
# Native development window
cargo run --manifest-path src-tauri/Cargo.toml

# Release executable
cargo build --release --locked --manifest-path src-tauri/Cargo.toml

# macOS .app / .dmg (uses local signing settings when available)
python3 scripts/bundle_macos.py
scripts/build_dmg.sh

# Windows installer (PowerShell)
./scripts/build-windows.ps1

# Compile and run Rust regression tests
cargo test --locked --manifest-path src-tauri/Cargo.toml
```

On macOS, scope `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer` to
Cargo commands when the default developer directory is Command Line Tools.
The packaging script does this automatically. It also reads the ignored
`certificates/import.json` from the checkout, or the main checkout when using a
Git worktree. Its `apple.developer_id` selects the installed signing identity;
`apple.apple_id`, `apple.apple_password` and `apple.team_id` enable automatic
notarization and stapling. With this local file, `scripts/build_dmg.sh` needs no
environment variables. Keep the file and certificate bundles untracked.

`APPLE_SIGNING_IDENTITY` overrides the identity, and `NOTARY_PROFILE` selects an
existing notarytool keychain profile instead of the local Apple credentials.
`APPLE_SIGNING_IDENTITY=-` explicitly requests an ad-hoc build without automatic
notarization, as used in CI. Without local settings or overrides, packaging also
falls back to ad-hoc signing.

Every successful DMG build copies the final image into `releases/` and updates
`releases/latest-mac.yml` with its version, filename, SHA-512, size and release
date. This happens after signing and any notarization/stapling; a failed build
or notarization leaves the previous release files in place. Generated DMGs are
ignored by Git; the update manifest remains tracked.

Node is optional. Existing npm entry points are dependency-free aliases:
`npm run dev`, `npm run build`, `npm run bundle`, `npm run bundle:dmg`,
`npm run lint` (cargo check), `npm test` (compile tests), `npm run test:run`.
Legacy HOME/production-index stress benchmarks require `--ignored`; normal
tests use temporary fixtures.

## Shortcuts

| Shortcut | Action |
| --- | --- |
| Cmd+Shift+Space | Activate/focus search (macOS) |
| Cmd/Ctrl+F | Focus and select search text |
| Cmd/Ctrl++ (or Cmd/Ctrl+=) | Zoom in |
| Cmd/Ctrl+- | Zoom out |
| Cmd/Ctrl+0 | Reset zoom to 100% |
| Up/Down, PageUp/PageDown | Navigate; Shift extends selection |
| Cmd/Ctrl+A | Select loaded results when the table has focus |
| Enter | Open on both platforms |
| Click selected item again after a pause (macOS) | Inline Rename |
| F2 (Windows) | Inline Rename |
| Cmd/Ctrl+O, double click | Open |
| Cmd/Ctrl+Enter | Reveal in file manager |
| Cmd+C (macOS) | Copy Files |
| Alt+Cmd+C (macOS) / Ctrl+C (Windows) | Copy Path |
| Cmd+Shift+C | Copy Files (macOS) |
| Space | Quick Look (macOS) |
| Cmd+Delete / Cmd+Backspace (macOS) | Move to Trash immediately |
| Delete (macOS) | Confirm Move to Trash |
| Delete (Windows) | Move to Recycle Bin without confirmation |
| Shift+Delete (Windows) | Permanently delete without confirmation |
| Escape | Cancel editing/menu/dialog, clear selection |

Search inputs keep their normal text selection and clipboard shortcuts.
Confirmation dialogs default to Cancel; Tab/arrow keys select an action.
UI zoom ranges from 50% to 200% and is restored when the app restarts.
The Lucide zoom icon in the titlebar, immediately left of the file-tree toggle,
opens a themed popover with the current percentage, minus, plus and Reset.
The titlebar shows only the icon. At 100%, the icon stays visible while the
popover is open and disappears together with it when the popover closes.
Zoom shortcuts show the same popover for three seconds; hovering pauses dismissal.
Click outside or press Escape to close it. The titlebar controls and popover keep
their original size at every zoom level. A manually opened popover also supports
Tab/Shift+Tab or Left/Right to select a control, then Enter/Space to activate it.

## Source and verification

- `src-tauri/src/ui/`: GPUI lifecycle, rendering, actions and status notices
- `ui_model.rs`, `ui_prefs.rs`, `runtime.rs`: selection/search state, preferences, worker/event bridge
- `main.rs`, `query.rs`, `mac/`, `win/`: shared engine, file actions and indexers
- `scripts/ui_fixture.py`, `ui_driver.py`: isolated real-window test tools
- [Migration plan](doc/gpui-migration-plan.md)
- [Native architecture](doc/gpui-architecture.md)
- [Test cases and results](doc/gpui-test-cases.md)
- [Windows native verification](doc/gpui-windows-validation.md)
- [Performance comparison](doc/gpui-performance.md)

Product specs remain in `doc/spec.md` and `doc/spec_KR.md`. Migration decisions,
validation scope and known platform limits are recorded in the linked reports.
