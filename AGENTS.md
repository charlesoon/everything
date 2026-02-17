# Repository Guidelines

## Project Structure & Module Organization
- `src/` contains the Svelte frontend (`App.svelte`, `main.js`).
- `src-tauri/src/` contains the Rust backend. Keep command wiring and app lifecycle in `main.rs`; keep query parsing/search helpers in focused modules like `query.rs`.
- `src-tauri/tauri.conf.json` and `src-tauri/capabilities/` define desktop runtime configuration and permissions.
- `doc/` stores specs and review notes.
- Build artifacts are generated in `dist/` and `src-tauri/target/` and should stay untracked.

## Build, Test, and Development Commands
- `npm install`: install frontend and Tauri CLI dependencies.
- `npm run dev`: run the Vite frontend for UI iteration.
- `npm run tauri dev`: run the full desktop app (frontend + Rust backend).
- `npm run build`: build the frontend bundle into `dist/`.
- `npm run lint`: run project checks (`vite build` + `cargo check`).
- `npm run test`: compile Rust tests (`cargo test --no-run`).
- `cargo test --manifest-path src-tauri/Cargo.toml`: run Rust unit tests.

## Coding Style & Naming Conventions
- Svelte/JS: 2-space indentation, semicolons, and single quotes.
- Use `camelCase` for variables/functions and `PascalCase` for Svelte components.
- Rust: follow `rustfmt` defaults; use `snake_case` for functions/modules and `CamelCase` for types.
- Keep backend-to-frontend payloads in `camelCase` (for example, `#[serde(rename_all = "camelCase")]`).

## Testing Guidelines
- Put backend unit tests near the code under `#[cfg(test)] mod tests` (see `src-tauri/src/query.rs`).
- Name tests by behavior, for example `path_search_dir_only` and `glob_to_like_escapes`.
- Run `npm run test` before pushing. Run full `cargo test --manifest-path src-tauri/Cargo.toml` when touching parser, indexing, or DB logic.
- Frontend tests are not set up yet; manually verify key flows in `npm run tauri dev`.

## Commit & Pull Request Guidelines
- Current history is minimal (`Initial commit`), so keep commit subjects short, clear, and imperative.
- Use one logical change per commit and explain behavioral impact in the body when needed.
- PRs should include: purpose, linked issue (if available), validation steps/outputs, and screenshots for UI changes.
- Highlight changes to permissions, indexing scope, or database behavior explicitly.

## Security & Configuration Tips
- Avoid hardcoded user-specific paths; derive them from runtime APIs/config.
- Do not commit local databases, logs, or generated build output.
