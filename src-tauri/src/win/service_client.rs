//! Client for the indexer service control pipe (design D3/D4/D5).
//!
//! The read-only Windows GUI uses this to poll the service for status (re-emit
//! as the existing `index_*` events), request a reset, and migrate sidecars.
//! One request/response per connection, newline-delimited JSON — matching
//! `service_pipe`. Best-effort: an unreachable service returns `None`/`Err`, and
//! the caller surfaces a health banner rather than failing hard.

use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;

use serde_json::{json, Value};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
};

use super::com_guard::to_wide;
use super::service_pipe::PIPE_NAME;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
// The service impersonates the pipe client to confirm INTERACTIVE membership
// (service_security::inspect_client, D3). That requires the connection to carry
// SecurityImpersonation QOS; without SECURITY_SQOS_PRESENT the impersonation
// level is too low, so the service rejects us as "not a local interactive
// client" and the GUI shows a false "service unresponsive" banner.
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IMPERSONATION: u32 = 0x0002_0000;
const MAX_RESPONSE: usize = 1024 * 1024;

/// Send one JSON request and read one JSON-line response. `None` if the service
/// isn't reachable (not installed / stopped / all pipe instances busy).
fn send(request: &str) -> Option<String> {
    let name = to_wide(PIPE_NAME);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(SECURITY_SQOS_PRESENT | SECURITY_IMPERSONATION),
            None,
        )
    }
    .ok()?;
    if handle.is_invalid() {
        return None;
    }

    // Wrap the handle in a File so it's closed on drop (also on early return).
    let mut file = unsafe { std::fs::File::from_raw_handle(handle.0 as std::os::windows::io::RawHandle) };

    let mut msg = request.to_string();
    msg.push('\n');
    file.write_all(msg.as_bytes()).ok()?;
    file.flush().ok()?;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = file.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.contains(&b'\n') || buf.len() > MAX_RESPONSE {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    Some(text.lines().next().unwrap_or("").to_string())
}

/// The service's status reply (the typed half of the control protocol, mirroring
/// the `PipeRequest` enum on the request side). Unknown fields (e.g. the
/// GUI-unused `entriesCount`) are ignored; a missing `state` or an `{"ok":false}`
/// error envelope fails to parse / is rejected, so the caller degrades to the
/// health-banner path instead of misreading it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceStatus {
    #[serde(default)]
    ok: Option<bool>,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) scanned: u64,
    #[serde(default)]
    pub(crate) indexed: u64,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) version: Option<String>,
    #[serde(default)]
    pub(crate) sidecar_migrated: Option<bool>,
}

/// `status` — the parsed service status, or `None` if unreachable or the reply
/// isn't a well-formed status (a transient `{"ok":false}` busy envelope included).
pub(crate) fn status() -> Option<ServiceStatus> {
    let resp = send(r#"{"cmd":"status"}"#)?;
    let status: ServiceStatus = serde_json::from_str(&resp).ok()?;
    if status.ok == Some(false) {
        return None;
    }
    Some(status)
}

/// `reset_index` — request a full re-index. `Err` if unreachable or refused
/// (e.g. cooldown).
pub(crate) fn reset_index() -> Result<(), String> {
    let resp = send(r#"{"cmd":"reset_index"}"#).ok_or("indexer service not reachable")?;
    interpret_ok(&resp)
}

/// `migrate_sidecars` — one-time transfer of this user's sidecars to the
/// service-owned config (idempotent, first-wins — §5).
pub(crate) fn migrate_sidecars(
    pathignore: Option<String>,
    pathindexing: Option<String>,
) -> Result<(), String> {
    let req = json!({
        "cmd": "migrate_sidecars",
        "pathignore": pathignore,
        "pathindexing": pathindexing,
    })
    .to_string();
    let resp = send(&req).ok_or("indexer service not reachable")?;
    interpret_ok(&resp)
}

fn interpret_ok(resp: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(resp).map_err(|e| format!("bad response: {e}"))?;
    if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(())
    } else {
        Err(v
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("request failed")
            .to_string())
    }
}
