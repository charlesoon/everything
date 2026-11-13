//! Opt-in, headless benchmark of the production search path. This module is
//! also compiled into the archived Tauri reference by the comparison harness.

use std::{fs, path::PathBuf, sync::atomic::Ordering, time::Instant};

use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    query: String,
    #[serde(default = "name")]
    sort_by: String,
    #[serde(default = "asc")]
    sort_dir: String,
    #[serde(default)]
    offset: u32,
    #[serde(default = "limit")]
    limit: u32,
}
fn name() -> String {
    "name".into()
}
fn asc() -> String {
    "asc".into()
}
fn limit() -> u32 {
    300
}

#[derive(Deserialize)]
struct Suite {
    queries: Vec<Case>,
    warmups: usize,
    iterations: usize,
}

pub(crate) fn handle_cli_args() -> bool {
    let args = std::env::args().collect::<Vec<_>>();
    let Some(index) = args.iter().position(|a| a == "--search-bench") else {
        return false;
    };
    let result = run(args.get(index + 1).map(String::as_str));
    match result {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        Err(error) => {
            eprintln!("search benchmark: {error}");
            std::process::exit(1);
        }
    }
    true
}

fn run(suite_path: Option<&str>) -> crate::AppResult<serde_json::Value> {
    let path = suite_path.ok_or(
        "Usage: EVERYTHING_DATA_DIR=<isolated fixture data> everything --search-bench <suite.json>",
    )?;
    let dir = PathBuf::from(
        std::env::var_os("EVERYTHING_DATA_DIR")
            .ok_or("EVERYTHING_DATA_DIR must name the fixture data directory")?,
    );
    let suite: Suite = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if suite.iterations == 0 || suite.iterations > 1000 || suite.warmups > 100 {
        return Err("Invalid iteration count".into());
    }
    let db_path = dir.join(crate::DB_FILE_NAME);
    crate::set_db_readonly();
    let state = crate::build_app_state(db_path, dir.parent().unwrap_or(&dir).join("files"), &dir);
    let conn = crate::db_connection_for_search(&state.db_path)?;
    state.db_ready.store(true, Ordering::Release);
    state
        .fts_ready
        .store(crate::fts_usable(&conn), Ordering::Release);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    {
        let mut status = state.status.lock();
        status.state = crate::IndexState::Ready;
        status.entries_count = count as u64;
    }
    let mut reports = Vec::new();
    for case in suite.queries {
        let mut timings = Vec::new();
        let mut paths = Vec::new();
        let mut total = None;
        let mut mode = String::new();
        for iteration in 0..suite.warmups + suite.iterations {
            let started = Instant::now();
            let execution = crate::execute_search(
                &state,
                case.query.clone(),
                Some(case.limit),
                Some(case.offset),
                Some(case.sort_by.clone()),
                Some(case.sort_dir.clone()),
            )?;
            let current_total = crate::compute_total_count(&state, &execution);
            let elapsed = started.elapsed().as_secs_f64() * 1000.;
            if iteration >= suite.warmups {
                timings.push(elapsed);
            }
            if iteration == suite.warmups {
                paths = execution.results.iter().map(|e| e.path.clone()).collect();
                total = current_total;
                mode = execution.mode_label;
            }
        }
        let mut sorted = timings.clone();
        sorted.sort_by(f64::total_cmp);
        let percentile = |fraction: f64| {
            sorted[((sorted.len() as f64 * fraction).ceil() as usize)
                .saturating_sub(1)
                .min(sorted.len() - 1)]
        };
        reports.push(json!({"id":case.id,"query":case.query,"sortBy":case.sort_by,"sortDir":case.sort_dir,"offset":case.offset,"limit":case.limit,"mode":mode,"paths":paths,"total":total,"samplesMs":timings,"p50Ms":percentile(0.5),"p95Ms":percentile(0.95)}));
    }
    Ok(
        json!({"version":env!("CARGO_PKG_VERSION"),"profile":if cfg!(debug_assertions){"debug"}else{"release"},"entries":count,"warmups":suite.warmups,"iterations":suite.iterations,"cases":reports}),
    )
}
