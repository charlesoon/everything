//! Session-0 logging for the indexer service (design §3.1).
//!
//! Session 0 has no console, so routine diagnostics go to a size-rolled file
//! under `data\logs\service.log`, and fatal errors additionally go to the
//! Windows Application event log (visible to SCM recovery and admins without
//! any console). Both are best-effort — a logging failure never aborts the
//! service.

use std::path::Path;

const EVENT_SOURCE: &str = super::service::SERVICE_NAME;

/// Append a line to `<data_dir>\logs\service.log`, rolling to `.1` once it
/// exceeds `MAX_LOG_BYTES`.
pub(crate) fn file(data_dir: &Path, msg: &str) {
    crate::diag::init_named(data_dir, "service", "service.log");
    crate::diag::log(msg);
}

/// Report a fatal error to the Windows Application event log. Used when the
/// data directory (and thus the file log) may not be available — e.g. a failed
/// SCM dispatch or a rejected data directory.
pub(crate) fn event_error(msg: &str) {
    eprintln!("[svc/ERROR] {msg}");
    write_event(msg);
}

fn write_event(msg: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::System::EventLog::{
        DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE,
    };

    let source = super::com_guard::to_wide(EVENT_SOURCE);
    let text = super::com_guard::to_wide(msg);

    unsafe {
        let handle = match RegisterEventSourceW(PCWSTR::null(), PCWSTR(source.as_ptr())) {
            Ok(h) => h,
            Err(_) => return,
        };
        let strings = [PCWSTR(text.as_ptr())];
        // Event id 1000 = generic service error. Category 0, no SID, no raw data.
        let _ = ReportEventW(
            handle,
            EVENTLOG_ERROR_TYPE,
            0,
            1000,
            None,
            0,
            Some(&strings),
            None,
        );
        let _ = DeregisterEventSource(handle);
    }
}
