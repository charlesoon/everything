use std::io::BufRead;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::EntryDto;

const SPOTLIGHT_TIMEOUT: Duration = Duration::from_secs(3);
const SPOTLIGHT_MAX_RESULTS: usize = 300;

pub struct SpotlightResult {
    pub entries: Vec<EntryDto>,
    pub timed_out: bool,
}

pub fn search_spotlight(home_dir: &Path, query: &str) -> SpotlightResult {
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.chars().count() < 2 {
        return SpotlightResult {
            entries: Vec::new(),
            timed_out: false,
        };
    }

    let home_str = home_dir.to_string_lossy();

    let mut child = match Command::new("mdfind")
        .args(["-name", trimmed, "-onlyin", &home_str])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            return SpotlightResult {
                entries: Vec::new(),
                timed_out: false,
            }
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            return SpotlightResult {
                entries: Vec::new(),
                timed_out: false,
            }
        }
    };

    let reader = std::io::BufReader::new(stdout);
    let mut entries = Vec::with_capacity(SPOTLIGHT_MAX_RESULTS);
    let started = Instant::now();
    let mut timed_out = false;

    for line in reader.lines() {
        if started.elapsed() >= SPOTLIGHT_TIMEOUT {
            timed_out = true;
            break;
        }

        let Ok(path_str) = line else { continue };
        let path = Path::new(&path_str);

        let name = match path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };

        let dir = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/".to_string());

        let is_dir = path.is_dir();

        let ext = if is_dir {
            None
        } else {
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
        };

        let meta = std::fs::symlink_metadata(path).ok();
        let size = meta.as_ref().filter(|m| m.is_file()).map(|m| m.len() as i64);
        let mtime = meta
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);

        entries.push(EntryDto {
            path: path_str,
            name,
            dir,
            is_dir,
            ext,
            size,
            mtime,
        });

        if entries.len() >= SPOTLIGHT_MAX_RESULTS {
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    SpotlightResult { entries, timed_out }
}
