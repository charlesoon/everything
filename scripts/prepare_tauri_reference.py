#!/usr/bin/env python3
"""Archive the Tauri baseline and add opt-in fixture/paint instrumentation.

The reference keeps its original renderer, debounce, IPC, and search code.
Instrumentation only selects the isolated DB, suppresses daemon registration
in fixture mode, and records callbacks after two animation frames.
"""

import argparse
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile


def replace_once(text, old, new):
    if text.count(old) != 1:
        raise RuntimeError(f'Expected one reference anchor: {old[:100]!r}')
    return text.replace(old, new, 1)


def prepare(repo, destination, reference):
    destination.mkdir(parents=True, exist_ok=True)
    source = subprocess.check_output(['git', 'archive', reference], cwd=repo)
    with tarfile.open(fileobj=io.BytesIO(source)) as archive:
        archive.extractall(destination)
    main = destination / 'src-tauri/src/main.rs'
    text = main.read_text()
    text = replace_once(text, 'mod fd_search;', 'mod fd_search;\nmod ui_search_bench;\n#[cfg(all(target_os="macos",feature="ui-automation"))]\nmod ui_automation;')
    text = replace_once(text, 'fn main() {', 'fn main() {\n    if ui_search_bench::handle_cli_args() { return; }')
    text = replace_once(text, 'async fn get_onboarding_state(state: State<\'_, AppState>) -> AppResult<OnboardingStateDto> {', '''async fn get_onboarding_state(state: State<'_, AppState>) -> AppResult<OnboardingStateDto> {
    if env_truthy("EVERYTHING_UI_TEST") {
        return Ok(OnboardingStateDto { recorded: true, fda_ok: true, registration: "unsupported".into(), ..Default::default() });
    }''')
    text = replace_once(text, 'fn check_full_disk_access() -> bool {', 'fn check_full_disk_access() -> bool {\n    if env_truthy("EVERYTHING_UI_TEST") { return true; }')
    anchor = '''    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {e}"))?;
    fs::create_dir_all(&app_data_dir).map_err(|e| e.to_string())?;'''
    text = replace_once(text, anchor, '''    #[cfg(all(target_os="macos",feature="ui-automation"))]
    ui_automation::start();
    let app_data_dir = if let Some(dir) = std::env::var_os("EVERYTHING_DATA_DIR") {
        PathBuf::from(dir)
    } else {
        app.path().app_data_dir().map_err(|e|e.to_string())?
    };
    fs::create_dir_all(&app_data_dir).map_err(|e|e.to_string())?;
    if env_truthy("EVERYTHING_UI_TEST") {
        set_db_readonly();
        let state = build_app_state(app_data_dir.join(DB_FILE_NAME), app_data_dir.parent().unwrap().join("files"), &app_data_dir);
        state.db_ready.store(true, AtomicOrdering::Release);
        let conn = db_connection_for_search(&state.db_path)?;
        state.fts_ready.store(fts_usable(&conn), AtomicOrdering::Release);
        {
            let mut status = state.status.lock();
            status.state = IndexState::Ready;
            status.entries_count = conn.query_row("SELECT COUNT(*) FROM entries", [], |r|r.get::<_,i64>(0)).map_err(|e|e.to_string())? as u64;
        }
        app.manage(state);
        return Ok(());
    }''')
    text = replace_once(text, 'fn setup_app(app: &mut tauri::App)', '''#[tauri::command]
fn record_ui_metric(mut payload: serde_json::Value) {
    if payload["event"] == "firstFrame" { payload["elapsedMs"] = serde_json::json!(startup_elapsed_ms()); }
    if let Some(path) = std::env::var_os("EVERYTHING_UI_METRICS") {
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) { let _=writeln!(file,"{payload}"); }
    }
}

fn setup_app(app: &mut tauri::App)''')
    text = replace_once(text, '.invoke_handler(tauri::generate_handler![', '.invoke_handler(tauri::generate_handler![\n            record_ui_metric,')
    main.write_text(text)
    shutil.copy2(repo / 'src-tauri/src/ui_search_bench.rs', main.parent / 'ui_search_bench.rs')
    shutil.copy2(repo / 'src-tauri/src/ui_automation.rs', main.parent / 'ui_automation.rs')
    manifest = destination / 'src-tauri/Cargo.toml'
    cargo = manifest.read_text()
    cargo = replace_once(cargo, "[target.'cfg(target_os = \"macos\")'.dependencies]", "[target.'cfg(target_os = \"macos\")'.dependencies]\ncocoa = { version = \"0.26\", optional = true }\nobjc = { version = \"0.2\", optional = true }\ndispatch = { version = \"0.2\", optional = true }")
    cargo = replace_once(cargo, '[features]', '[features]\nui-automation = ["dep:cocoa", "dep:objc", "dep:dispatch"]')
    manifest.write_text(cargo)
    config_path = destination / 'src-tauri/tauri.conf.json'
    config = json.loads(config_path.read_text())
    config['productName'] = 'Everything Reference'
    config['identifier'] = 'com.everything.reference'
    config['app']['windows'][0].update(width=900, height=560)
    config_path.write_text(json.dumps(config, indent=2))
    frontend = destination / 'src/App.svelte'
    text = frontend.read_text()
    text = replace_once(text, '<script>', '<script>\n  let __uiInputStarted = 0;')
    text = replace_once(text, "on:input={() => { indexingFinishedAt = ''; scheduleSearch(); }}", "on:input={() => { __uiInputStarted = performance.now(); indexingFinishedAt = ''; scheduleSearch(); }}")
    text = replace_once(text, '    const searchQuery = query;', '    const searchQuery = query;\n    const __uiInputTime = __uiInputStarted;\n    const __uiBackendStarted = performance.now();')
    text = replace_once(text, '      dbLastQuery = searchQuery;', '      const __uiBackendMs = performance.now() - __uiBackendStarted;\n      dbLastQuery = searchQuery;')
    text = replace_once(text, '''      updateViewportHeight();
    } catch (err) {
      if (gen !== searchGeneration) return;''', '''      updateViewportHeight();
      if (searchQuery.trim()) {
        requestAnimationFrame(() => requestAnimationFrame(() => {
          if (gen === searchGeneration) void invoke('record_ui_metric', { payload: { event: 'searchFrame', query: searchQuery, backendMs: __uiBackendMs, elapsedMs: performance.now() - __uiInputTime, rows: results.length, first: results[0]?.name, includeTotal } });
        }));
      }
    } catch (err) {
      if (gen !== searchGeneration) return;''')
    anchor = "    await step('invoke(mark_frontend_ready)', () => invoke('mark_frontend_ready'));"
    text = replace_once(text, anchor, anchor + "\n    requestAnimationFrame(() => requestAnimationFrame(() => void invoke('record_ui_metric', { payload: { event: 'firstFrame' } })));")
    frontend.write_text(text)
    print(f'Prepared Tauri reference {reference}: {destination}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('destination', type=Path)
    parser.add_argument('--ref', default='68e4ddf')
    args = parser.parse_args()
    prepare(Path(__file__).resolve().parents[1], args.destination.resolve(), args.ref)
