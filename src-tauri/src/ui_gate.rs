//! Coalesces index updates while the native window is hidden.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::runtime::EventSink;
use parking_lot::Mutex;

static VISIBLE: AtomicBool = AtomicBool::new(true);

static HELD: Mutex<Held> = Mutex::new(Held::EMPTY);

/// Latest of each held event. Coalescing rather than queueing is deliberate:
/// the frontend renders current state, so replaying an evening's worth of
/// `index_updated` ticks would only buy it that many redundant re-searches.
struct Held {
    state: Option<(String, Option<String>)>,
    updated: Option<(u64, i64, u64)>,
    progress: Option<(u64, u64, String)>,
}

impl Held {
    const EMPTY: Self = Self {
        state: None,
        updated: None,
        progress: None,
    };
}

/// Whether the page is currently worth emitting to.
pub(crate) fn is_visible() -> bool {
    VISIBLE.load(Ordering::Acquire)
}

pub(crate) fn hold_state(state: String, message: Option<String>) {
    HELD.lock().state = Some((state, message));
}

pub(crate) fn hold_updated(entries_count: u64, last_updated: i64, permission_errors: u64) {
    HELD.lock().updated = Some((entries_count, last_updated, permission_errors));
}

pub(crate) fn hold_progress(scanned: u64, indexed: u64, current_path: String) {
    HELD.lock().progress = Some((scanned, indexed, current_path));
}

/// Delivers whatever was held while the page was hidden. Ordered the way a live
/// pass emits — progress, then state, then the count — so the frontend's
/// `index_state` bookkeeping still sees the count that belongs to it.
fn flush(app: &EventSink) {
    let held = std::mem::replace(&mut *HELD.lock(), Held::EMPTY);
    if let Some((scanned, indexed, current_path)) = held.progress {
        crate::emit_index_progress_now(app, scanned, indexed, current_path);
    }
    if let Some((state, message)) = held.state {
        crate::emit_index_state_now(app, &state, message);
    }
    if let Some((entries_count, last_updated, permission_errors)) = held.updated {
        crate::emit_index_updated_now(app, entries_count, last_updated, permission_errors);
    }
}

/// Reported by the frontend's `visibilitychange` handler.
pub(crate) fn set_visible(app: &EventSink, visible: bool) {
    VISIBLE.store(visible, Ordering::Release);
    if visible {
        flush(app);
    }
}
