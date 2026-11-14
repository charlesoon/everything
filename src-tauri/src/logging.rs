//! Runtime diagnostics switch. Windows shares it between the GUI and service.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

struct Settings {
    path: PathBuf,
    enabled: AtomicBool,
    default: bool,
    update: parking_lot::Mutex<()>,
    observers: parking_lot::Mutex<Vec<crate::runtime::EventSink>>,
}

static SETTINGS: OnceLock<Arc<Settings>> = OnceLock::new();
static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn settings_path() -> PathBuf {
    let app_data = crate::ui_prefs::app_data_dir();
    #[cfg(target_os = "windows")]
    if !crate::env_truthy("EVERYTHING_UI_TEST") {
        return crate::win::service_paths::config_dir()
            .unwrap_or(app_data)
            .join("setting.ini");
    }
    app_data.join("setting.ini")
}

pub(crate) fn init() {
    if !cfg!(target_os = "windows") {
        return;
    }
    SETTINGS.get_or_init(|| {
        // Preserve existing diagnostics on other platforms. Windows logging
        // is opt-in through the machine-wide config directory.
        let default = !cfg!(target_os = "windows");
        let path = settings_path();
        let settings = Arc::new(Settings {
            enabled: AtomicBool::new(read_enabled(&path, default)),
            path,
            default,
            update: parking_lot::Mutex::new(()),
            observers: parking_lot::Mutex::new(Vec::new()),
        });
        let watch = settings.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            let (enabled, previous) = {
                let _update = watch.update.lock();
                let enabled = read_enabled(&watch.path, watch.default);
                (enabled, watch.enabled.swap(enabled, Ordering::AcqRel))
            };
            if enabled != previous {
                notify(&watch, enabled);
            }
            if enabled && !previous {
                crate::diag::log("logging enabled source=setting.ini");
            }
        });
        settings
    });
}

pub(crate) fn subscribe(sink: crate::runtime::EventSink) {
    if let Some(settings) = SETTINGS.get() {
        settings.observers.lock().push(sink);
    }
}

fn notify(settings: &Settings, enabled: bool) {
    for sink in settings.observers.lock().iter() {
        let _ = sink.emit("logging_changed", serde_json::json!({"enabled":enabled}));
    }
}

/// Cached atomic read: rendering the settings menu never touches the disk.
pub(crate) fn enabled() -> bool {
    SETTINGS
        .get()
        .map_or(!cfg!(target_os = "windows"), |settings| {
            settings.enabled.load(Ordering::Acquire)
        })
}

/// Runs on a background executor, including when invoked by the menu.
pub(crate) fn set_enabled(enabled: bool) -> Result<(), String> {
    let settings = SETTINGS
        .get()
        .ok_or("Logging settings are not initialized")?;
    {
        let _update = settings.update.lock();
        write_enabled(&settings.path, enabled).map_err(|error| error.to_string())?;
        settings.enabled.store(enabled, Ordering::Release);
    }
    notify(settings, enabled);
    if enabled {
        crate::diag::log("logging enabled source=settings-menu");
    }
    Ok(())
}

fn parse_enabled(text: &str, default: bool) -> bool {
    text.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            if !key.trim().eq_ignore_ascii_case("logging") {
                return None;
            }
            match value.split([';', '#']).next()?.trim() {
                value if value.eq_ignore_ascii_case("true") => Some(true),
                value if value.eq_ignore_ascii_case("false") => Some(false),
                _ => None,
            }
        })
        .last()
        .unwrap_or(default)
}

fn read_enabled(path: &Path, default: bool) -> bool {
    fs::read_to_string(path).map_or(default, |text| parse_enabled(&text, default))
}

fn update_text(text: &str, enabled: bool) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let bom = if text.starts_with('\u{feff}') {
        "\u{feff}"
    } else {
        ""
    };
    let mut found = false;
    let mut lines: Vec<String> = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(|line| {
            if line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("logging"))
            {
                found = true;
                format!("logging={enabled}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.insert(0, format!("logging={enabled}"));
    }
    format!("{bom}{}{newline}", lines.join(newline))
}

fn write_enabled(path: &Path, enabled: bool) -> std::io::Result<()> {
    let old = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("Missing settings directory"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".setting.ini.{}.{}.tmp",
        std::process::id(),
        NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(update_text(&old, enabled).as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logging_is_opt_in_and_parses_bom_comments_and_case() {
        assert!(!parse_enabled("", false));
        assert!(!parse_enabled("# logging=true\nlogging=invalid", false));
        assert!(parse_enabled(
            "\u{feff}Logging = TRUE ; diagnostics\r\n",
            false
        ));
        assert!(!parse_enabled("logging=true\nlogging=false", true));
        assert!(parse_enabled("another=true", true));
    }

    #[test]
    fn toggling_preserves_other_settings_and_line_endings() {
        let old = "\u{feff}; user settings\r\nlogging=false\r\nother=value\r\n";
        let on = update_text(old, true);
        assert_eq!(
            on,
            "\u{feff}; user settings\r\nlogging=true\r\nother=value\r\n"
        );
        assert_eq!(update_text(&on, false), old);
        assert_eq!(
            update_text("other=value\n", true),
            "logging=true\nother=value\n"
        );
        assert!(!parse_enabled(
            &update_text("logging=true\nlogging=true", false),
            true
        ));
    }

    #[test]
    fn setting_file_round_trip_preserves_unrelated_values() {
        let root = std::env::temp_dir().join(format!(
            "everything-log-setting-{}-{}",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("setting.ini");
        fs::create_dir_all(&root).unwrap();
        fs::write(&path, "other=value\r\nlogging=false\r\n").unwrap();
        write_enabled(&path, true).unwrap();
        assert!(read_enabled(&path, false));
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("other=value\r\n"));
        write_enabled(&path, false).unwrap();
        assert!(!read_enabled(&path, true));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_file(path).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
