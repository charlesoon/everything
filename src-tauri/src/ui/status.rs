//! Status and persistent notices from the original UI, independent of drawing.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::IndexStatusDto;

#[derive(Default)]
pub(super) struct StatusDisplay {
    pub pathignore_changed: bool,
    pub pathindexing_changed: bool,
    pub fda_settings_opened: bool,
    last_ready_count: u64,
    started: Option<Instant>,
    finished: Option<Duration>,
}

impl StatusDisplay {
    pub fn new(status: &IndexStatusDto) -> Self {
        Self {
            started: (status.state == "Indexing").then(Instant::now),
            ..Self::default()
        }
    }

    pub fn update(&mut self, previous: &IndexStatusDto, next: &IndexStatusDto, now: Instant) {
        if previous.state != "Indexing" && next.state == "Indexing" {
            if previous.state == "Ready" {
                self.last_ready_count = previous.entries_count;
            }
            self.started = Some(now);
            self.finished = None;
        } else if previous.state == "Indexing" && next.state != "Indexing" {
            self.finished = self
                .started
                .take()
                .map(|started| now.saturating_duration_since(started));
        }
    }

    pub fn receive(&mut self, events: &BTreeMap<&str, serde_json::Value>) {
        self.pathignore_changed |= events.contains_key("pathignore_changed");
        self.pathindexing_changed |= events.contains_key("pathindexing_changed");
    }

    pub fn clear_finished(&mut self) {
        self.finished = None;
    }

    pub fn state_text(&self, status: &IndexStatusDto, now: Instant) -> String {
        if status.state != "Indexing" {
            return format!("Index: {}", status.state);
        }
        let mut text = "Indexing".to_string();
        if self.last_ready_count > 0 {
            let percent = ((status.scanned as f64 / self.last_ready_count as f64) * 100.)
                .round()
                .min(99.);
            text.push_str(&format!(" ({percent:.0}%)"));
        }
        if let Some(started) = self.started {
            text.push_str(&format!(
                " · {}",
                format_elapsed(now.saturating_duration_since(started))
            ));
        }
        if !is_catchup(status) {
            text.push_str(&format!(" · {} scanned", format_count(status.scanned)));
        }
        text
    }

    pub fn finished_text(&self) -> Option<String> {
        self.finished
            .map(|elapsed| format!("in {}", format_elapsed(elapsed)))
    }
}

pub(super) fn is_catchup(status: &IndexStatusDto) -> bool {
    status
        .message
        .as_deref()
        .is_some_and(|message| message.starts_with("Catchup:"))
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

fn format_count(count: u64) -> String {
    let text = count.to_string();
    let mut result = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index > 0 && (text.len() - index) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str) -> IndexStatusDto {
        IndexStatusDto {
            state: state.into(),
            entries_count: 2000,
            last_updated: None,
            permission_errors: 0,
            message: None,
            scanned: 0,
            indexed: 0,
            current_path: String::new(),
            background_active: false,
        }
    }

    #[test]
    fn catchup_progress_keeps_elapsed_and_completion_without_fresh_scan_count() {
        let ready = status("Ready");
        let mut indexing = status("Indexing");
        indexing.scanned = 1000;
        let start = Instant::now();
        let mut display = StatusDisplay::new(&ready);
        display.update(&ready, &indexing, start);
        assert_eq!(
            display.state_text(&indexing, start + Duration::from_secs(65)),
            "Indexing (50%) · 1m 5s · 1,000 scanned"
        );
        indexing.message = Some("Catchup: checking changes".into());
        assert_eq!(
            display.state_text(&indexing, start + Duration::from_secs(65)),
            "Indexing (50%) · 1m 5s"
        );
        display.update(&indexing, &ready, start + Duration::from_secs(66));
        assert_eq!(display.finished_text().as_deref(), Some("in 1m 6s"));
        display.clear_finished();
        assert!(display.finished_text().is_none());
    }

    #[test]
    fn config_notices_survive_unrelated_events_until_individually_dismissed() {
        let mut display = StatusDisplay::default();
        display.receive(&BTreeMap::from([(
            "pathignore_changed",
            serde_json::Value::Null,
        )]));
        display.receive(&BTreeMap::from([(
            "index_updated",
            serde_json::Value::Null,
        )]));
        assert!(display.pathignore_changed);
        assert!(!display.pathindexing_changed);
        display.receive(&BTreeMap::from([(
            "pathindexing_changed",
            serde_json::Value::Null,
        )]));
        display.pathignore_changed = false;
        display.receive(&BTreeMap::new());
        assert!(!display.pathignore_changed);
        assert!(display.pathindexing_changed);
    }
}
