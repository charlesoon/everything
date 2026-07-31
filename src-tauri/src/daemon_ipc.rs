//! UDS control server + client for the macOS daemon (design D5).
//!
//! `<data dir>/daemon.sock`, newline-delimited JSON, one request/response per
//! connection — the macOS port of the Windows named-pipe protocol
//! (`win/service_pipe.rs` / `win/service_client.rs`). Commands: `status`,
//! `reset_index` (600s cooldown), `restart` (quiesce → exit 1), `disable`
//! (quiesce → exit 0), `attach_lease` (renewable lease, D8/G6).
//!
//! Security: the socket file is chmod 0600 and every accepted connection is
//! verified same-uid via `getpeereid` — the UDS analogue of the pipe DACL.
//! The server opens BEFORE the startup build (F6) so status/restart/disable
//! always answer, and it keeps answering through Quiescing (G15).
//!
//! Path length: a macOS `sockaddr_un` path caps at 104 bytes; when the data
//! dir is too deep the socket falls back to `$TMPDIR` and the actual path is
//! recorded in a `daemon.sock.path` sidecar (readable without the DB) and in
//! the status response (G13/R11).

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::daemon::{DaemonControl, LEASE_TTL};
use crate::{diag, AppState};

const SOCKET_FILE: &str = "daemon.sock";
const SOCKET_SIDECAR: &str = "daemon.sock.path";
/// `sockaddr_un.sun_path` is 104 bytes on macOS including the trailing NUL.
const MAX_SUN_PATH: usize = 103;
const MAX_LINE: usize = 128 * 1024; // mirror of service_pipe.rs
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const RESET_COOLDOWN: Duration = Duration::from_secs(600);
const MAX_ACTIVE: usize = 4;

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum IpcRequest {
    Status,
    ResetIndex,
    Restart,
    Disable,
    AttachLease { pid: u32, token: u64 },
}

// ---------------------------------------------------------------------------
// Server (daemon process)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct IpcContext {
    pub(crate) state: AppState,
    pub(crate) control: Arc<DaemonControl>,
}

/// Server-side socket path, falling back to `$TMPDIR` when the data-dir path
/// exceeds the sockaddr limit. Maintains the sidecar for clients: written on
/// fallback, removed otherwise (so clients never chase a stale tmp path).
fn server_socket_path(data_dir: &Path) -> PathBuf {
    let preferred = data_dir.join(SOCKET_FILE);
    if preferred.as_os_str().len() <= MAX_SUN_PATH {
        let _ = std::fs::remove_file(data_dir.join(SOCKET_SIDECAR));
        return preferred;
    }
    let fallback =
        std::env::temp_dir().join(format!("everything-daemon-{}.sock", unsafe { libc::getuid() }));
    let _ = std::fs::write(
        data_dir.join(SOCKET_SIDECAR),
        fallback.to_string_lossy().as_bytes(),
    );
    fallback
}

/// Client-side resolution: honor the sidecar when present.
pub(crate) fn client_socket_path(data_dir: &Path) -> PathBuf {
    if let Ok(content) = std::fs::read_to_string(data_dir.join(SOCKET_SIDECAR)) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    data_dir.join(SOCKET_FILE)
}

/// Bind the socket and start the accept loop. Called while holding
/// `daemon.lock`, so unlinking a stale socket file is safe — no live server
/// can own it. Returns the actual socket path (for status / logging).
pub(crate) fn start_server(ctx: IpcContext, data_dir: &Path) -> std::io::Result<PathBuf> {
    let path = server_socket_path(data_dir);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let accept_path = path.clone();
    std::thread::Builder::new()
        .name("everything-daemon-ipc".to_string())
        .spawn(move || accept_loop(listener, ctx, accept_path))?;
    Ok(path)
}

fn accept_loop(listener: UnixListener, ctx: IpcContext, path: PathBuf) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.load(Ordering::Acquire) >= MAX_ACTIVE {
            continue; // dropped: the client sees EOF and retries on its next poll
        }
        active.fetch_add(1, Ordering::AcqRel);
        let ctx = ctx.clone();
        let active2 = Arc::clone(&active);
        std::thread::spawn(move || {
            handle_connection(stream, &ctx);
            active2.fetch_sub(1, Ordering::AcqRel);
        });
    }
    // Only reachable if the listener breaks (e.g. socket unlinked from
    // outside); the daemon runs on, uncontrollable — worth a post-mortem line.
    diag::log(format!(
        "daemon IPC accept loop ended unexpectedly ({})",
        path.display()
    ));
}

fn same_uid_peer(stream: &UnixStream) -> bool {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let ok = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0;
    ok && uid == unsafe { libc::getuid() }
}

fn handle_connection(mut stream: UnixStream, ctx: &IpcContext) {
    let _ = stream.set_read_timeout(Some(IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IDLE_TIMEOUT));
    let response = if !same_uid_peer(&stream) {
        json!({"ok": false, "error": "rejected: peer uid mismatch"}).to_string()
    } else {
        match read_line(&mut stream) {
            Some(line) => dispatch(&line, ctx),
            None => json!({"ok": false, "error": "no request or timeout"}).to_string(),
        }
    };
    let mut out = response.into_bytes();
    out.push(b'\n');
    let _ = stream.write_all(&out);
}

fn read_line(stream: &mut UnixStream) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    buf.truncate(pos);
                    break;
                }
                if buf.len() > MAX_LINE {
                    return None;
                }
            }
            Err(_) => break, // read timeout or connection reset
        }
    }
    if buf.is_empty() {
        return None;
    }
    String::from_utf8(buf)
        .ok()
        .map(|s| s.trim_end_matches('\r').to_string())
}

fn dispatch(line: &str, ctx: &IpcContext) -> String {
    match serde_json::from_str::<IpcRequest>(line) {
        Ok(IpcRequest::Status) => status_response(ctx),
        Ok(IpcRequest::ResetIndex) => reset_response(ctx),
        Ok(IpcRequest::Restart) => exit_response(ctx, 1),
        Ok(IpcRequest::Disable) => exit_response(ctx, 0),
        Ok(IpcRequest::AttachLease { pid, token }) => attach_response(ctx, pid, token),
        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}).to_string(),
    }
}

fn status_response(ctx: &IpcContext) -> String {
    let (index_state, scanned, indexed, entries, message) = {
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
        "state": ctx.control.lifecycle().as_str(),
        "indexState": index_state,
        "scanned": scanned,
        "indexed": indexed,
        "entriesCount": entries,
        "message": message,
        "version": env!("CARGO_PKG_VERSION"),
        "fdaOk": ctx.control.fda_ok.load(Ordering::Acquire),
        "degraded": ctx.control.degraded.load(Ordering::Acquire),
        "pid": std::process::id(),
        "socketPath": ctx
            .control
            .socket_path
            .lock()
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        "isFresh": ctx.control.is_fresh.load(Ordering::Acquire),
        "leaseMode": ctx.control.lease_mode,
    })
    .to_string()
}

fn reset_response(ctx: &IpcContext) -> String {
    {
        let mut last = ctx.control.last_reset.lock();
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
    diag::log("IPC reset_index queued");
    ctx.control.reset_requested.store(true, Ordering::Release);
    json!({"ok": true, "queued": "reset_index"}).to_string()
}

fn exit_response(ctx: &IpcContext, code: i32) -> String {
    ctx.control.request_exit(code);
    json!({"ok": true, "exit": code}).to_string()
}

fn attach_response(ctx: &IpcContext, pid: u32, token: u64) -> String {
    ctx.control.attach_lease(pid, token);
    json!({
        "ok": true,
        "leaseMode": ctx.control.lease_mode,
        "ttlSecs": LEASE_TTL.as_secs(),
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Client (GUI bridge / MCP / CLI)
// ---------------------------------------------------------------------------

/// Daemon status as seen over IPC. Superset of the Windows `ServiceStatus`
/// (`service_client.rs`): `state` is the §4 lifecycle
/// (Starting/WaitingOnboarding/Building/Ready/Quiescing), `index_state` the
/// classic Ready/Indexing/Error label the frontend already understands.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
#[allow(dead_code)] // consumed by the P2.2 GUI bridge and the P2.3 MCP path
pub(crate) struct DaemonStatus {
    pub(crate) ok: bool,
    pub(crate) state: String,
    pub(crate) index_state: String,
    pub(crate) scanned: u64,
    pub(crate) indexed: u64,
    pub(crate) entries_count: u64,
    pub(crate) message: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) fda_ok: bool,
    pub(crate) degraded: bool,
    pub(crate) pid: u32,
    pub(crate) socket_path: Option<String>,
    pub(crate) is_fresh: bool,
    pub(crate) lease_mode: bool,
}

fn send_line(data_dir: &Path, line: &str) -> Option<String> {
    let path = client_socket_path(data_dir);
    let mut stream = UnixStream::connect(&path).ok()?;
    stream.set_read_timeout(Some(IDLE_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IDLE_TIMEOUT)).ok()?;
    stream.write_all(line.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    read_line(&mut stream)
}

#[allow(dead_code)] // consumed by the P2.2 GUI bridge and the P2.3 MCP path
pub(crate) fn status(data_dir: &Path) -> Option<DaemonStatus> {
    let resp = send_line(data_dir, r#"{"cmd":"status"}"#)?;
    serde_json::from_str::<DaemonStatus>(&resp).ok().filter(|s| s.ok)
}

fn interpret_ok(resp: Option<String>) -> Result<(), String> {
    let Some(resp) = resp else {
        return Err("daemon unreachable".to_string());
    };
    let v: serde_json::Value =
        serde_json::from_str(&resp).map_err(|e| format!("bad response: {e}"))?;
    if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Ok(());
    }
    let mut err = v
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("request failed")
        .to_string();
    if let Some(retry) = v.get("retryAfterSecs").and_then(|r| r.as_u64()) {
        err = format!("{err} (retry in {retry}s)");
    }
    Err(err)
}

#[allow(dead_code)] // consumed by the P2.2 reset_index delegation
pub(crate) fn reset_index(data_dir: &Path) -> Result<(), String> {
    interpret_ok(send_line(data_dir, r#"{"cmd":"reset_index"}"#))
}

#[allow(dead_code)] // consumed by the P2.2 version-mismatch recovery
pub(crate) fn restart(data_dir: &Path) -> Result<(), String> {
    interpret_ok(send_line(data_dir, r#"{"cmd":"restart"}"#))
}

#[allow(dead_code)] // consumed by the P3 auto-start toggle
pub(crate) fn disable(data_dir: &Path) -> Result<(), String> {
    interpret_ok(send_line(data_dir, r#"{"cmd":"disable"}"#))
}

/// Register/renew this process as a lease user (renewable lease — G6). The
/// token disambiguates pid reuse; a crashed holder ages out via the TTL.
#[allow(dead_code)] // consumed by the P2.2 bridge / P2.3 MCP lease renewal
pub(crate) fn attach_lease(data_dir: &Path) -> Result<(), String> {
    let line = format!(
        r#"{{"cmd":"attach_lease","pid":{},"token":{}}}"#,
        std::process::id(),
        lease_token()
    );
    interpret_ok(send_line(data_dir, &line))
}

/// Per-process lease identity: stable for the process lifetime, unique enough
/// across pid reuse (first-call clock nanos mixed with the pid).
fn lease_token() -> u64 {
    use std::sync::OnceLock;
    static TOKEN: OnceLock<u64> = OnceLock::new();
    *TOKEN.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        nanos ^ (u64::from(std::process::id()) << 32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_requests() {
        assert!(matches!(
            serde_json::from_str::<IpcRequest>(r#"{"cmd":"status"}"#),
            Ok(IpcRequest::Status)
        ));
        assert!(matches!(
            serde_json::from_str::<IpcRequest>(r#"{"cmd":"reset_index"}"#),
            Ok(IpcRequest::ResetIndex)
        ));
        assert!(matches!(
            serde_json::from_str::<IpcRequest>(r#"{"cmd":"attach_lease","pid":42,"token":7}"#),
            Ok(IpcRequest::AttachLease { pid: 42, token: 7 })
        ));
        assert!(serde_json::from_str::<IpcRequest>(r#"{"cmd":"shutdown"}"#).is_err());
    }

    #[test]
    fn socket_path_falls_back_when_data_dir_too_deep() {
        let root = std::env::temp_dir().join(format!(
            "everything-ipc-longpath-{}-{}",
            std::process::id(),
            line!()
        ));
        let deep = root.join("a".repeat(120));
        std::fs::create_dir_all(&deep).unwrap();

        let path = server_socket_path(&deep);
        assert!(
            path.as_os_str().len() <= MAX_SUN_PATH,
            "fallback path itself must fit: {}",
            path.display()
        );
        assert!(deep.join(SOCKET_SIDECAR).exists(), "sidecar written on fallback");
        assert_eq!(
            client_socket_path(&deep),
            path,
            "clients must resolve through the sidecar"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn status_roundtrip_over_uds() {
        let root = std::env::temp_dir().join(format!(
            "everything-ipc-rt-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("index.db");
        let state = crate::build_app_state(db_path, root.join("home"), &root);
        let control = Arc::new(DaemonControl::new(true));
        let ctx = IpcContext {
            state,
            control: Arc::clone(&control),
        };
        let path = start_server(ctx, &root).expect("server starts");
        assert!(path.exists());

        let status = status(&root).expect("status answers");
        assert!(status.ok);
        assert_eq!(status.state, "Starting");
        assert!(status.lease_mode);
        assert_eq!(status.pid, std::process::id());

        attach_lease(&root).expect("attach_lease answers");
        assert!(control.live_lease_count() >= 1);

        let _ = std::fs::remove_dir_all(root);
    }
}
