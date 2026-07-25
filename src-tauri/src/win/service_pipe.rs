//! Named-pipe control server for the indexer service (design D3).
//!
//! `\\.\pipe\EverythingIndexer`, newline-delimited JSON, one request/response
//! per connection. Commands: `status`, `reset_index`, `migrate_sidecars`.
//! Security: the pipe DACL (service_security) allows only SYSTEM /
//! Administrators / INTERACTIVE, and `PIPE_REJECT_REMOTE_CLIENTS` blocks remote
//! callers. Slow clients that don't send a full request within `IDLE_TIMEOUT`
//! have their I/O cancelled. `reset_index` is globally cooled down; the pipe
//! only enqueues onto the single indexer worker (execution/serialization is the
//! worker's job, §3.1).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::IO::CancelIoEx;

use super::com_guard::to_wide;
use super::service::IndexRequest;
use super::{service_log, service_security};
use crate::AppState;

pub(crate) const PIPE_NAME: &str = r"\\.\pipe\EverythingIndexer";
const MAX_INSTANCES: u32 = 4;
const MAX_LINE: usize = 128 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const RESET_COOLDOWN: Duration = Duration::from_secs(600);
const SIDECAR_MARKER: &str = "sidecars.migrated";

#[derive(Clone)]
pub(crate) struct PipeContext {
    pub(crate) state: AppState,
    pub(crate) index_tx: Sender<IndexRequest>,
    pub(crate) last_reset: Arc<Mutex<Option<Instant>>>,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum PipeRequest {
    Status,
    ResetIndex,
    MigrateSidecars {
        #[serde(default)]
        pathignore: Option<String>,
        #[serde(default)]
        pathindexing: Option<String>,
    },
}

fn data_dir_of(state: &AppState) -> PathBuf {
    state
        .db_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn marker_path(state: &AppState) -> PathBuf {
    data_dir_of(state).join(SIDECAR_MARKER)
}

pub(crate) fn start(ctx: PipeContext, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || accept_loop(ctx, stop));
}

fn accept_loop(ctx: PipeContext, stop: Arc<AtomicBool>) {
    let active = Arc::new(AtomicUsize::new(0));
    // D3: at most one concurrent connection per logon session.
    let sessions: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));
    let name = to_wide(PIPE_NAME);
    // One security descriptor for every instance: the SDDL is a constant and the
    // OS copies the descriptor into each pipe object at creation, so it needn't
    // be rebuilt per connection. Held alive for the whole accept loop.
    let sec = service_security::pipe_security();
    let sec_attrs = sec.as_ref().map(|s| s.attributes());
    while !stop.load(Ordering::Acquire) {
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                MAX_INSTANCES,
                4096,
                4096,
                0,
                sec_attrs,
            )
        };
        if handle.is_invalid() {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }

        let connected = unsafe { ConnectNamedPipe(handle, None) };
        let ok = connected.is_ok()
            || connected
                .as_ref()
                .err()
                .map(|e| e.code() == ERROR_PIPE_CONNECTED.to_hresult())
                .unwrap_or(false);
        if stop.load(Ordering::Acquire) {
            unsafe {
                let _ = DisconnectNamedPipe(handle);
                let _ = CloseHandle(handle);
            }
            break;
        }
        if !ok || active.load(Ordering::Acquire) >= MAX_INSTANCES as usize {
            unsafe {
                let _ = DisconnectNamedPipe(handle);
                let _ = CloseHandle(handle);
            }
            continue;
        }

        active.fetch_add(1, Ordering::AcqRel);
        let ctx = ctx.clone();
        let active2 = active.clone();
        let sessions2 = sessions.clone();
        // HANDLE isn't Send; pass the raw pointer value across the thread.
        let raw = handle.0 as isize;
        std::thread::spawn(move || {
            handle_connection(HANDLE(raw as *mut core::ffi::c_void), &ctx, &sessions2);
            active2.fetch_sub(1, Ordering::AcqRel);
        });
    }
}

fn handle_connection(handle: HANDLE, ctx: &PipeContext, sessions: &Arc<Mutex<HashSet<u32>>>) {
    // Slow-client defense: cancel pending I/O if a full request doesn't arrive
    // within IDLE_TIMEOUT, so one silent client can't hold an instance open.
    let done = Arc::new(AtomicBool::new(false));
    {
        let done = done.clone();
        let raw = handle.0 as isize;
        std::thread::spawn(move || {
            let start = Instant::now();
            while !done.load(Ordering::Acquire) {
                if start.elapsed() >= IDLE_TIMEOUT {
                    unsafe {
                        let _ = CancelIoEx(HANDLE(raw as *mut core::ffi::c_void), None);
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
    }

    let mut pid: u32 = 0;
    unsafe {
        let _ = GetNamedPipeClientProcessId(handle, &mut pid);
    }

    // D3: confirm the caller is a local interactive-session process (token check
    // beyond the pipe DACL) and learn its session for the per-session cap + audit.
    let response = match service_security::inspect_client(handle) {
        Some(c) if c.interactive => {
            // At most one concurrent connection per logon session.
            let claimed = sessions.lock().unwrap().insert(c.session_id);
            if !claimed {
                json!({
                    "ok": false,
                    "error": "busy: another request from this session is in progress"
                })
                .to_string()
            } else {
                let r = match read_line(handle, MAX_LINE) {
                    Some(line) => dispatch(&line, ctx, pid, c.session_id),
                    None => json!({"ok": false, "error": "no request or timeout"}).to_string(),
                };
                sessions.lock().unwrap().remove(&c.session_id);
                r
            }
        }
        _ => json!({"ok": false, "error": "rejected: not a local interactive client"}).to_string(),
    };
    done.store(true, Ordering::Release);

    let mut out = response.into_bytes();
    out.push(b'\n');
    unsafe {
        let mut written: u32 = 0;
        let _ = WriteFile(handle, Some(&out), Some(&mut written), None);
        let _ = FlushFileBuffers(handle);
        let _ = DisconnectNamedPipe(handle);
        let _ = CloseHandle(handle);
    }
}

fn read_line(handle: HANDLE, max: usize) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut read: u32 = 0;
        let ok = unsafe { ReadFile(handle, Some(&mut chunk), Some(&mut read), None) };
        if ok.is_err() || read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read as usize]);
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.truncate(pos);
            break;
        }
        if buf.len() > max {
            return None;
        }
    }
    if buf.is_empty() {
        return None;
    }
    String::from_utf8(buf)
        .ok()
        .map(|s| s.trim_end_matches('\r').to_string())
}

fn dispatch(line: &str, ctx: &PipeContext, pid: u32, session: u32) -> String {
    match serde_json::from_str::<PipeRequest>(line) {
        Ok(PipeRequest::Status) => status_response(ctx),
        Ok(PipeRequest::ResetIndex) => reset_response(ctx, pid, session),
        Ok(PipeRequest::MigrateSidecars {
            pathignore,
            pathindexing,
        }) => migrate_response(ctx, pid, session, pathignore, pathindexing),
        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}).to_string(),
    }
}

fn status_response(ctx: &PipeContext) -> String {
    let (state_str, scanned, indexed, entries, message) = {
        let s = ctx.state.status.lock();
        (
            format!("{:?}", s.state),
            s.scanned,
            s.indexed,
            s.entries_count,
            s.message.clone(),
        )
    };
    json!({
        "ok": true,
        "state": state_str,
        "scanned": scanned,
        "indexed": indexed,
        "entriesCount": entries,
        "message": message,
        "version": env!("CARGO_PKG_VERSION"),
        "sidecarMigrated": marker_path(&ctx.state).exists(),
    })
    .to_string()
}

fn reset_response(ctx: &PipeContext, pid: u32, session: u32) -> String {
    {
        let mut last = ctx.last_reset.lock().unwrap();
        if let Some(t) = *last {
            let elapsed = t.elapsed();
            if elapsed < RESET_COOLDOWN {
                return json!({
                    "ok": false,
                    "error": "cooldown",
                    "retryAfterSecs": (RESET_COOLDOWN - elapsed).as_secs(),
                })
                .to_string();
            }
        }
        *last = Some(Instant::now());
    }
    service_log::file(
        &data_dir_of(&ctx.state),
        &format!("pipe reset_index (pid={pid} session={session})"),
    );
    let _ = ctx.index_tx.send(IndexRequest::Reset);
    json!({"ok": true, "queued": "reset_index"}).to_string()
}

fn migrate_response(
    ctx: &PipeContext,
    pid: u32,
    session: u32,
    pathignore: Option<String>,
    pathindexing: Option<String>,
) -> String {
    let marker = marker_path(&ctx.state);
    // First-wins is atomic: create_new succeeds for exactly one caller even if
    // two users' GUIs migrate concurrently. The marker lives in the
    // Users-read-only data dir, so it can't be tampered with (§5).
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .is_err()
    {
        return json!({"ok": true, "status": "already_migrated"}).to_string();
    }

    if let Some(config_dir) = ctx.state.config_file_path.parent() {
        let _ = std::fs::create_dir_all(config_dir);
        // The whole request line is already bounded by MAX_LINE in read_line, so
        // sidecar content is inherently size-capped — write it verbatim. A prior
        // per-file cap here silently dropped larger sidecars while still claiming
        // the migration marker below, permanently losing the user's rules.
        if let Some(content) = pathignore {
            let _ = std::fs::write(config_dir.join(".pathignore"), content);
        }
        if let Some(content) = pathindexing {
            let _ = std::fs::write(
                config_dir.join(".pathindexing"),
                filter_pathindexing(&content),
            );
        }
    }
    service_log::file(
        &data_dir_of(&ctx.state),
        &format!("pipe migrate_sidecars (pid={pid} session={session})"),
    );
    let _ = ctx.index_tx.send(IndexRequest::Reconcile);
    json!({"ok": true, "status": "migrated"}).to_string()
}

/// Keep only `.pathindexing` roots safe for a LocalSystem service (D1):
/// absolute, on the `C:` volume, no `~`/home-relative rules. Comments and blank
/// lines pass through; everything else is dropped.
fn filter_pathindexing(content: &str) -> String {
    let mut out = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if trimmed.starts_with('~') {
            continue;
        }
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("C:\\") || upper.starts_with("C:/") {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathindexing_filter_keeps_only_c_absolute() {
        let input = "# comment\nC:\\Users\\me\\proj\nD:\\other\n~/home\n\nC:/forward/slash\n\\\\server\\share\n";
        let out = filter_pathindexing(input);
        assert!(out.contains("C:\\Users\\me\\proj"));
        assert!(out.contains("C:/forward/slash"));
        assert!(out.contains("# comment"));
        assert!(!out.contains("D:\\other"), "non-C volume must be dropped");
        assert!(!out.contains("~/home"), "home-relative must be dropped");
        assert!(!out.contains("server"), "UNC path must be dropped");
    }

    #[test]
    fn pipe_request_parses_commands() {
        assert!(matches!(
            serde_json::from_str::<PipeRequest>(r#"{"cmd":"status"}"#),
            Ok(PipeRequest::Status)
        ));
        assert!(matches!(
            serde_json::from_str::<PipeRequest>(r#"{"cmd":"reset_index"}"#),
            Ok(PipeRequest::ResetIndex)
        ));
        assert!(matches!(
            serde_json::from_str::<PipeRequest>(
                r#"{"cmd":"migrate_sidecars","pathignore":"node_modules"}"#
            ),
            Ok(PipeRequest::MigrateSidecars { .. })
        ));
        assert!(serde_json::from_str::<PipeRequest>(r#"{"cmd":"nope"}"#).is_err());
        assert!(serde_json::from_str::<PipeRequest>("not json").is_err());
    }
}
