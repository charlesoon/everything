//! Native window/UI preferences, separate from the daemon's index/config.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

pub(crate) const MIN_ZOOM: f32 = 0.5;
pub(crate) const MAX_ZOOM: f32 = 2.;

pub(crate) fn normalize_zoom(zoom: f32) -> f32 {
    if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.
    }
}

pub(crate) fn app_data_dir() -> PathBuf {
    let dir = std::env::var_os("EVERYTHING_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(default_data_dir);
    dir.canonicalize().unwrap_or(dir)
}

// UI preferences belong to the user. In particular, Windows' shared index
// lives in a service-owned, read-only ProgramData directory.
fn default_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        crate::resolve_home_dir().join("Library/Application Support/com.everything.app")
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::resolve_home_dir().join("AppData/Roaming"))
            .join("com.everything.app")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| crate::resolve_home_dir().join(".local/share"))
            .join("com.everything.app")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Preferences {
    pub theme: String,
    pub columns: [f32; 4],
    pub window: Option<[f32; 4]>,
    pub fda_dismissed: bool,
    pub file_tree_visible: bool,
    pub file_tree_width: f32,
    pub zoom: f32,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            columns: [0.; 4],
            window: None,
            fda_dismissed: false,
            file_tree_visible: false,
            file_tree_width: 280.,
            zoom: 1.,
        }
    }
}

impl Preferences {
    pub fn load() -> Self {
        let mut prefs: Self = fs::read(app_data_dir().join("gpui-preferences.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if prefs
            .columns
            .iter()
            .zip([180., 240., 62., 150.])
            .any(|(width, min)| !width.is_finite() || *width < min)
        {
            prefs.columns = [0.; 4];
        }
        prefs.window = prefs.window.filter(|bounds| {
            bounds.iter().all(|n| n.is_finite()) && bounds[2] >= 400. && bounds[3] >= 300.
        });
        prefs.theme = "system".into();
        if !prefs.file_tree_width.is_finite() {
            prefs.file_tree_width = Self::default().file_tree_width;
        }
        prefs.file_tree_width = prefs.file_tree_width.clamp(180., 640.);
        prefs.zoom = normalize_zoom(prefs.zoom);
        prefs
    }

    pub fn save(&self) {
        let dir = app_data_dir();
        if let Ok(bytes) = serde_json::to_vec_pretty(self) {
            let _ = fs::create_dir_all(&dir);
            // Same-directory replace never leaves a truncated preferences file.
            let temp = dir.join("gpui-preferences.json.tmp");
            if fs::write(&temp, bytes).is_ok() {
                let _ = fs::rename(temp, dir.join("gpui-preferences.json"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_preferences_keep_default_zoom() {
        let prefs: Preferences = serde_json::from_str(r#"{"fileTreeVisible":true}"#).unwrap();
        assert!(prefs.file_tree_visible);
        assert_eq!(prefs.zoom, 1.);
    }

    #[test]
    fn invalid_zoom_is_safe_and_bounded() {
        for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(normalize_zoom(value), 1.);
        }
        assert_eq!(normalize_zoom(-10.), MIN_ZOOM);
        assert_eq!(normalize_zoom(10.), MAX_ZOOM);
        assert_eq!(normalize_zoom(1.25), 1.25);
    }
}
