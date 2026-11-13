//! Webview visibility gate + repaint watchdog.
//!
//! Field failure (macOS, 2026-08-04, v0.5.4): the window sat occluded behind
//! other windows for an evening. WebKit marked the view invisible, suspended the
//! WebContent process, and on every suspend called
//! `RemoteLayerTreeDrawingAreaProxy::hideContentUntilPendingUpdate` — the remote
//! layer tree then stays hidden until the web process sends a fresh commit. The
//! bridge's 1s `PRAGMA data_version` poll kept emitting `index_updated`, and
//! Tauri delivers events by evaluating JS in the page, so each emit woke the
//! suspended process (844 wakeups / 791 suspend cycles in one hour) only for
//! WebKit to re-suspend it before a commit landed. When the machine went idle
//! the wakeups stopped too, stranding the layer tree hidden with no pending
//! commit — and AppKit never re-evaluated occlusion (WebKit's last
//! `isViewVisible()` was 16 hours earlier), so nothing ever asked for one. Net
//! effect: live process, live webview, zero JS for 13 hours, and a blank window
//! until the user quit and relaunched.
//!
//! Two halves, both needed:
//!
//! * **Gate** — while the page reports itself hidden (`document.visibilityState`,
//!   which WKWebView drives off the same activity state that decides
//!   suspension), `emit_index_*` holds the payload instead of poking the
//!   webview. The webview stays suspended — no wake/re-suspend thrash, no
//!   battery burn — and gets one coalesced flush when it comes back.
//! * **Watchdog** — the gate alone cannot self-heal, because a suspended page
//!   runs no JS and so can never report that it is visible again. On window
//!   focus (an AppKit key-window event, independent of the occlusion tracking
//!   that went stale) we ping the webview and wait for a
//!   `requestAnimationFrame` acknowledgement. rAF only runs when the page
//!   actually renders a frame, which is the one signal separating "painted"
//!   from "JS ran but the layer tree is still hidden" — every one of the 844
//!   wakeups above ran JS. No ack inside the grace period means the layer tree
//!   is stranded, and a reload is what forces the fresh commit.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use parking_lot::Mutex;
#[cfg(target_os = "macos")]
use tauri::Manager;
use tauri::{AppHandle, Emitter};

/// How long the webview gets to answer a focus probe with a rendered frame.
/// WebKit only has to resume the process and run one frame, so this is several
/// times what a healthy page needs while still recovering within a blink.
#[cfg(target_os = "macos")]
const PAINT_ACK_GRACE: Duration = Duration::from_millis(2500);

/// Floor between forced reloads, so a webview that never acknowledges can't be
/// reloaded on every focus event.
#[cfg(target_os = "macos")]
const RELOAD_COOLDOWN: Duration = Duration::from_secs(30);

/// False only while the frontend has reported `document.hidden`. Defaults to
/// visible: a build without the frontend half, or a page that never reports,
/// behaves exactly as it did before this module existed.
static VISIBLE: AtomicBool = AtomicBool::new(true);

/// Monotonic probe id the frontend echoes back after a rendered frame.
#[cfg(target_os = "macos")]
static PROBE_SEQ: AtomicU64 = AtomicU64::new(0);
static ACKED_SEQ: AtomicU64 = AtomicU64::new(0);
/// One probe at a time — focus events can arrive in bursts (app switching,
/// Mission Control) and each probe owns a sleeping thread.
#[cfg(target_os = "macos")]
static PROBE_INFLIGHT: AtomicBool = AtomicBool::new(false);
/// `now_ms()` of the last forced reload; 0 = never reloaded.
#[cfg(target_os = "macos")]
static LAST_RELOAD_MS: AtomicU64 = AtomicU64::new(0);

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

#[cfg(target_os = "macos")]
fn now_ms() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
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
fn flush(app: &AppHandle) {
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
pub(crate) fn set_visible(app: &AppHandle, visible: bool) {
    VISIBLE.store(visible, Ordering::Release);
    if visible {
        flush(app);
    }
}

/// The frontend rendered a frame for probe `seq`. `fetch_max` because a stale
/// ack from an earlier probe must never satisfy a newer one.
pub(crate) fn note_painted(seq: u64) {
    ACKED_SEQ.fetch_max(seq, Ordering::AcqRel);
}

/// The OS says the user is looking at this window. Re-opens the gate — a page
/// that stopped running while hidden can no longer report its own visibility —
/// and on macOS checks that the webview still paints.
pub(crate) fn on_window_focused(app: &AppHandle) {
    if !VISIBLE.swap(true, Ordering::AcqRel) {
        flush(app);
    }
    #[cfg(target_os = "macos")]
    probe_paint(app);
}

/// Emits a probe and reloads the webview if no frame comes back. macOS-only:
/// the stranded-layer-tree failure this recovers from is WebKit's remote layer
/// tree, and a reload is disruptive enough not to arm it where it can't help.
#[cfg(target_os = "macos")]
fn probe_paint(app: &AppHandle) {
    // Before the first page load there is no frame to wait for, and reloading
    // mid-startup would restart onMount for nothing.
    let ready = app
        .try_state::<crate::AppState>()
        .map(|s| s.frontend_ready.load(Ordering::Acquire))
        .unwrap_or(false);
    if !ready {
        return;
    }
    if PROBE_INFLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }

    let seq = PROBE_SEQ.fetch_add(1, Ordering::AcqRel) + 1;
    let _ = app.emit("ui_repaint_probe", seq);

    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(PAINT_ACK_GRACE);
        PROBE_INFLIGHT.store(false, Ordering::Release);
        if ACKED_SEQ.load(Ordering::Acquire) >= seq {
            return;
        }

        let now = now_ms();
        let last = LAST_RELOAD_MS.load(Ordering::Acquire);
        if last != 0 && now.saturating_sub(last) < RELOAD_COOLDOWN.as_millis() as u64 {
            return;
        }
        LAST_RELOAD_MS.store(now, Ordering::Release);

        crate::diag::log(format!(
            "[gui] webview focused but rendered no frame in {}ms (probe {}); reloading",
            PAINT_ACK_GRACE.as_millis(),
            seq
        ));
        if let Some(window) = app.get_webview_window("main") {
            if let Err(e) = window.reload() {
                crate::diag::log(format!("[gui] webview reload failed: {e}"));
            }
        }
    });
}
