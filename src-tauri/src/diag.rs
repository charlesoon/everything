//! Post-mortem diagnostics: a persistent, size-rotated log shared by every
//! process that touches the index (GUI, `--daemon`, `--mcp`).
//!
//! Exists because the pathologies that matter here — a daemon stuck rebuilding
//! the index, two writers overlapping on the WAL, an FSEvents overflow storm —
//! happen on other people's machines while nobody is watching, and are next to
//! impossible to reproduce on demand. The detached daemon has no visible
//! stderr and a Finder-launched GUI loses its stderr too, so the app records
//! its own anomalies here for after-the-fact reconstruction.
//!
//! Discipline: state transitions, index-pass boundaries, and anomaly `ALERT`s
//! only — never per-event chatter. A pass logs O(1) lines and the watcher logs
//! only when a threshold trips, so the file stays small and readable.

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Rotate when the log exceeds this. Checked only at process start (`init`),
/// so the active pair is `everything.log` + `everything.log.1` and a
/// long-lived process keeps appending to its (possibly rotated-away) inode
/// harmlessly until it restarts.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

struct Diag {
    component: &'static str,
    path: PathBuf,
    file: Mutex<Option<File>>,
}

static DIAG: OnceLock<Diag> = OnceLock::new();

pub(crate) fn log_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("logs").join("everything.log")
}

/// Open the shared log for appending. Used by `spawn_detached` to point the
/// detached daemon's stderr at the same file, so its plain `eprintln!`
/// diagnostics land here instead of /dev/null.
#[cfg(not(target_os = "windows"))] // only spawn_detached (non-Windows) opens this
pub(crate) fn append_handle(app_data_dir: &Path) -> Option<File> {
    let path = log_path(app_data_dir);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// Initialize once per process; `component` ("gui" | "daemon" | "mcp") tags
/// every line so interleaved writers stay attributable. Later calls no-op.
pub(crate) fn init(app_data_dir: &Path, component: &'static str) {
    let path = log_path(app_data_dir);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok();
    if DIAG
        .set(Diag {
            component,
            path,
            file: Mutex::new(file),
        })
        .is_ok()
    {
        log(format!(
            "=== {component} start v{} pid={} ===",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        ));
    }
}

/// Append one timestamped line. Mirrors to stderr only when stderr is a
/// terminal — a detached daemon's stderr IS this file, so an unconditional
/// mirror would double every line. Never panics; a failed write drops the
/// handle so the next call reopens it.
pub(crate) fn log(msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    if std::io::stderr().is_terminal() {
        eprintln!("[diag] {msg}");
    }
    let Some(diag) = DIAG.get() else { return };
    let line = format!(
        "{} [{}] {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f %z"),
        diag.component,
        msg
    );
    let mut file = diag.file.lock().unwrap_or_else(|e| e.into_inner());
    if file.is_none() {
        *file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&diag.path)
            .ok();
    }
    if let Some(f) = file.as_mut() {
        if f.write_all(line.as_bytes()).is_err() {
            *file = None;
        }
    }
}

/// Sliding-window anomaly detector: `record()` stamps one event and returns
/// `Some(count)` when at least `threshold` events landed inside `window` —
/// at most once per window, so an ongoing storm produces one ALERT per
/// window instead of one per event.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct RateAlert {
    window: std::time::Duration,
    threshold: usize,
    events: std::collections::VecDeque<std::time::Instant>,
    last_alert: Option<std::time::Instant>,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl RateAlert {
    pub(crate) fn new(window: std::time::Duration, threshold: usize) -> Self {
        Self {
            window,
            threshold,
            events: std::collections::VecDeque::new(),
            last_alert: None,
        }
    }

    pub(crate) fn record(&mut self) -> Option<usize> {
        let now = std::time::Instant::now();
        self.events.push_back(now);
        while self
            .events
            .front()
            .map_or(false, |t| now.duration_since(*t) > self.window)
        {
            self.events.pop_front();
        }
        if self.events.len() >= self.threshold
            && self
                .last_alert
                .map_or(true, |t| now.duration_since(t) > self.window)
        {
            self.last_alert = Some(now);
            return Some(self.events.len());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rate_alert_fires_once_per_window() {
        let mut alert = RateAlert::new(Duration::from_secs(60), 3);
        assert_eq!(alert.record(), None);
        assert_eq!(alert.record(), None);
        assert_eq!(alert.record(), Some(3)); // threshold crossed
        assert_eq!(alert.record(), None); // suppressed within the same window
        assert_eq!(alert.record(), None);
    }

    #[test]
    fn rate_alert_below_threshold_never_fires() {
        let mut alert = RateAlert::new(Duration::from_secs(60), 10);
        for _ in 0..9 {
            assert_eq!(alert.record(), None);
        }
    }

    #[test]
    fn log_without_init_is_a_safe_noop() {
        // DIAG is process-global; other tests may have initialized it, so only
        // assert that logging never panics either way.
        log("noop line from test");
    }

    #[test]
    fn log_path_is_under_logs_dir() {
        let p = log_path(Path::new("/data/app"));
        assert_eq!(p, PathBuf::from("/data/app/logs/everything.log"));
    }
}
