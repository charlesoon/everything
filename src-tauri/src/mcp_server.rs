//! MCP (Model Context Protocol) stdio server + auto-registration.
//!
//! `everything --mcp` runs a standalone MCP server over stdin/stdout serving a
//! `search` tool straight from the SQLite index, so agents get results even
//! when the GUI app is not running. On normal app startup `register_all`
//! writes the server entry into Claude Code (`~/.claude.json`) and Codex
//! (`~/.codex/config.toml`) so both agents pick it up automatically.

use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::query::{parse_query, SearchMode};
use crate::{get_meta, run_db_search, sort_entries_with_relevance, AppResult, EntryDto, MAX_LIMIT};

const SERVER_NAME: &str = "everything";
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const MCP_DEFAULT_LIMIT: u32 = 100;

const SERVER_INSTRUCTIONS: &str = "Instant file/folder name search over the \
local filesystem index built by the Everything app. Results come from a \
prebuilt SQLite index (no live filesystem walk), so prefer this over \
`find`/shell globbing when locating files by name anywhere on the machine.";

const SEARCH_TOOL_DESCRIPTION: &str = "Search local file/folder names in the \
Everything index and return matching absolute paths instantly. Query syntax: \
plain text matches names by substring (exact and prefix matches rank first); \
'*' and '?' are glob wildcards (e.g. 'report*.pdf'); '*.ext' finds files by \
extension; a query containing '/' restricts matches to a directory (e.g. \
'src/main', 'Documents/*.pdf', or 'Downloads/' to list a folder). Directories \
in the output have a trailing '/'. The index covers the user's home directory \
(macOS) or C:\\ (Windows), minus build/cache noise like .git and node_modules.";

// ---------------------------------------------------------------------------
// CLI entry
// ---------------------------------------------------------------------------

/// Handles MCP-related CLI flags before Tauri boots. Returns `true` when the
/// invocation was fully handled and the process should exit without starting
/// the GUI.
pub fn handle_cli_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--mcp") {
        run_stdio_server();
        return true;
    }
    if args.iter().any(|a| a == "--register-mcp") {
        for line in register_all() {
            eprintln!("[mcp] {line}");
        }
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Paths (resolved without Tauri: the MCP server runs outside the app)
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// The same location Tauri's `app_data_dir()` resolves for the app identifier
/// `com.everything.app`, computed without a Tauri context.
fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("EVERYTHING_MCP_DB") {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "macos")]
    {
        home_dir().join("Library/Application Support/com.everything.app/index.db")
    }
    #[cfg(target_os = "windows")]
    {
        let roaming = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join("AppData").join("Roaming"));
        roaming.join("com.everything.app").join("index.db")
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        home_dir().join(".local/share/com.everything.app/index.db")
    }
}

// ---------------------------------------------------------------------------
// Search over the index DB
// ---------------------------------------------------------------------------

fn open_search_connection(db_path: &Path) -> AppResult<Connection> {
    if !db_path.exists() {
        return Err(format!(
            "Index database not found at {}. Launch the Everything app once to build the index.",
            db_path.display()
        ));
    }
    // READ_WRITE (not CREATE) so WAL recovery can run, but the connection is
    // pinned read-only via query_only: the watcher/indexer own all writes.
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    conn.execute_batch(
        "PRAGMA query_only=ON;
         PRAGMA busy_timeout=2000;
         PRAGMA cache_size=-32768;
         PRAGMA mmap_size=1073741824;",
    )
    .map_err(|e| e.to_string())?;
    conn.set_prepared_statement_cache_capacity(64);
    Ok(conn)
}

/// FTS is usable when the shadow table exists and the app has not flagged it
/// dirty (mid-rebuild / crash). Mirrors the startup gating in `main.rs`.
fn compute_fts_ready(conn: &Connection) -> bool {
    let has_fts: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='entries_fts')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    has_fts && get_meta(conn, "fts_dirty").map_or(true, |v| v != "1")
}

struct SearchArgs {
    query: String,
    limit: u32,
    offset: u32,
    sort_by: String,
    sort_dir: String,
}

fn parse_search_args(arguments: &Value) -> Result<SearchArgs, String> {
    let query = arguments
        .get("query")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if query.is_empty() {
        return Err("`query` is required and must be a non-empty string.".to_string());
    }
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| (v as u32).clamp(1, MAX_LIMIT))
        .unwrap_or(MCP_DEFAULT_LIMIT);
    let offset = arguments
        .get("offset")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(u32::MAX as u64) as u32)
        .unwrap_or(0);
    let sort_by = match arguments.get("sort_by").and_then(|v| v.as_str()) {
        None => "name".to_string(),
        Some(s @ ("name" | "mtime" | "size" | "dir")) => s.to_string(),
        Some(other) => return Err(format!("Invalid sort_by {other:?} (name|mtime|size|dir).")),
    };
    let sort_dir = match arguments.get("sort_dir").and_then(|v| v.as_str()) {
        None => "asc".to_string(),
        Some(s @ ("asc" | "desc")) => s.to_string(),
        Some(other) => return Err(format!("Invalid sort_dir {other:?} (asc|desc).")),
    };
    Ok(SearchArgs {
        query,
        limit,
        offset,
        sort_by,
        sort_dir,
    })
}

fn mode_label(mode: &SearchMode) -> &'static str {
    match mode {
        SearchMode::Empty => "empty",
        SearchMode::NameSearch { .. } => "name",
        SearchMode::GlobName { .. } => "glob",
        SearchMode::ExtSearch { .. } => "ext",
        SearchMode::PathSearch { .. } => "path",
    }
}

fn format_results(args: &SearchArgs, label: &str, results: &[EntryDto]) -> String {
    if results.is_empty() {
        return format!(
            "No matches for {:?}{}.",
            args.query,
            if args.offset > 0 {
                format!(" at offset {}", args.offset)
            } else {
                String::new()
            }
        );
    }
    let more = if results.len() as u32 >= args.limit {
        format!(
            "; more may exist, pass offset={} for the next page",
            args.offset + results.len() as u32
        )
    } else {
        String::new()
    };
    let mut out = format!(
        "{} result(s) (mode: {label}, sort: {} {}, offset: {}{more})\n",
        results.len(),
        args.sort_by,
        args.sort_dir,
        args.offset,
    );
    for e in results {
        out.push_str(&e.path);
        if e.is_dir {
            out.push(std::path::MAIN_SEPARATOR);
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// MCP server (JSON-RPC 2.0 over stdio, newline-delimited)
// ---------------------------------------------------------------------------

struct McpServer {
    db_path: PathBuf,
    home_dir: PathBuf,
    conn: Option<Connection>,
}

impl McpServer {
    fn new(db_path: PathBuf, home_dir: PathBuf) -> Self {
        McpServer {
            db_path,
            home_dir,
            conn: None,
        }
    }

    /// Handles one incoming JSON-RPC message; `None` means nothing to send
    /// (notification, or a malformed message that carries no usable id).
    fn handle_line(&mut self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(error_response(
                    Value::Null,
                    -32700,
                    &format!("Parse error: {e}"),
                ))
            }
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        if method.is_empty() {
            // A response to a server-initiated request; we never send any.
            return None;
        }
        let Some(id) = id else {
            // Notification (notifications/initialized, notifications/cancelled, ...)
            return None;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(handle_initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": [search_tool_definition()] })),
            "tools/call" => self.handle_tools_call(&params),
            _ => Err((-32601, format!("Method not found: {method}"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, message)) => error_response(id, code, &message),
        })
    }

    fn handle_tools_call(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name != "search" {
            return Err((-32602, format!("Unknown tool: {name:?}")));
        }
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let text = match parse_search_args(&arguments).and_then(|args| self.do_search(&args)) {
            Ok(text) => return Ok(tool_result(&text, false)),
            Err(message) => message,
        };
        Ok(tool_result(&text, true))
    }

    fn do_search(&mut self, args: &SearchArgs) -> Result<String, String> {
        if self.conn.is_none() {
            self.conn = Some(open_search_connection(&self.db_path)?);
        }
        let conn = self.conn.as_ref().expect("connection opened above");
        let mode = parse_query(&args.query);
        let fts_ready = compute_fts_ready(conn);
        let searched = run_db_search(
            conn,
            &self.home_dir,
            fts_ready,
            &mode,
            &args.query,
            args.limit,
            args.offset,
            &args.sort_by,
            &args.sort_dir,
        );
        let mut results = match searched {
            Ok(r) => r,
            Err(e) => {
                // Drop the cached connection so a transient failure (index
                // rebuild, DB replaced by reset_index) heals on the next call.
                self.conn = None;
                return Err(format!("Search failed: {e}"));
            }
        };
        if args.offset == 0 && args.sort_by == "name" {
            sort_entries_with_relevance(&mut results, &args.query, &args.sort_by, &args.sort_dir);
        }
        Ok(format_results(args, mode_label(&mode), &results))
    }
}

fn handle_initialize(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": SERVER_NAME,
            "title": "Everything file search",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": SERVER_INSTRUCTIONS,
    })
}

fn search_tool_definition() -> Value {
    json!({
        "name": "search",
        "title": "Everything file search",
        "description": SEARCH_TOOL_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (substring, glob, '*.ext', or 'dir/name')."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "default": MCP_DEFAULT_LIMIT,
                    "description": "Maximum results to return."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Pagination offset."
                },
                "sort_by": {
                    "type": "string",
                    "enum": ["name", "mtime", "size", "dir"],
                    "default": "name",
                    "description": "Sort key; 'name' also ranks exact/prefix matches first."
                },
                "sort_dir": {
                    "type": "string",
                    "enum": ["asc", "desc"],
                    "default": "asc"
                }
            },
            "required": ["query"]
        }
    })
}

fn tool_result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Blocking stdio loop: one JSON-RPC message per line in, one per line out.
/// Exits when stdin closes (the client owns the server's lifetime).
pub fn run_stdio_server() {
    let db_path = default_db_path();
    eprintln!(
        "[mcp] everything MCP server v{} serving {}",
        env!("CARGO_PKG_VERSION"),
        db_path.display()
    );
    let mut server = McpServer::new(db_path, home_dir());
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(response) = server.handle_line(line) {
            let mut out = stdout.lock();
            let payload = serde_json::to_string(&response).expect("response serializes");
            if writeln!(out, "{payload}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Auto-registration into Claude Code and Codex
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum RegisterOutcome {
    Updated,
    Unchanged,
    Skipped(&'static str),
}

/// Registers this binary (with `--mcp`) as an MCP server for every supported
/// agent CLI found on this machine. Best-effort and idempotent; returns
/// human-readable log lines. Called in the background on app startup and by
/// `--register-mcp`.
pub fn register_all() -> Vec<String> {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return vec![format!("registration skipped: current_exe failed: {e}")],
    };
    let home = home_dir();
    let mut logs = Vec::new();
    let targets: [(&str, fn(&Path, &Path) -> Result<RegisterOutcome, String>, PathBuf); 2] = [
        ("Claude Code", register_claude, home.join(".claude.json")),
        ("Codex", register_codex, home.join(".codex").join("config.toml")),
    ];
    for (agent, register, config_path) in targets {
        match register(&config_path, &exe) {
            Ok(RegisterOutcome::Updated) => {
                logs.push(format!("{agent}: registered in {}", config_path.display()))
            }
            Ok(RegisterOutcome::Unchanged) => {
                logs.push(format!("{agent}: already registered"))
            }
            Ok(RegisterOutcome::Skipped(reason)) => {
                logs.push(format!("{agent}: skipped ({reason})"))
            }
            Err(e) => logs.push(format!("{agent}: registration failed: {e}")),
        }
    }
    logs
}

fn atomic_write(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, contents).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// Claude Code user-scope config: `mcpServers.everything` in `~/.claude.json`.
/// Never clobbers an unreadable file, preserves all other keys, and only
/// writes when the entry actually changes.
fn register_claude(config_path: &Path, exe: &Path) -> Result<RegisterOutcome, String> {
    let claude_dir_exists = config_path
        .parent()
        .map(|d| d.join(".claude").is_dir())
        .unwrap_or(false);
    if !config_path.exists() && !claude_dir_exists {
        return Ok(RegisterOutcome::Skipped("Claude Code not detected"));
    }
    let mut root: Value = if config_path.exists() {
        let raw = fs::read_to_string(config_path).map_err(|e| e.to_string())?;
        serde_json::from_str(&raw)
            .map_err(|e| format!("{} is not valid JSON ({e}); not touching it", config_path.display()))?
    } else {
        json!({})
    };
    let obj = root
        .as_object_mut()
        .ok_or_else(|| format!("{} is not a JSON object", config_path.display()))?;
    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| "mcpServers is not a JSON object".to_string())?;
    let desired = json!({
        "type": "stdio",
        "command": exe.to_string_lossy(),
        "args": ["--mcp"],
        "env": {},
    });
    if servers.get(SERVER_NAME) == Some(&desired) {
        return Ok(RegisterOutcome::Unchanged);
    }
    servers.insert(SERVER_NAME.to_string(), desired);
    let mut serialized = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    serialized.push('\n');
    atomic_write(config_path, &serialized)?;
    Ok(RegisterOutcome::Updated)
}

/// Codex config: `[mcp_servers.everything]` in `~/.codex/config.toml`.
/// Edited with toml_edit so user comments/formatting survive.
fn register_codex(config_path: &Path, exe: &Path) -> Result<RegisterOutcome, String> {
    let codex_dir = config_path.parent().unwrap_or(Path::new("."));
    if !codex_dir.is_dir() {
        return Ok(RegisterOutcome::Skipped("Codex not detected"));
    }
    let raw = if config_path.exists() {
        fs::read_to_string(config_path).map_err(|e| e.to_string())?
    } else {
        String::new()
    };
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| format!("{} is not valid TOML ({e}); not touching it", config_path.display()))?;
    let exe_str = exe.to_string_lossy();

    let servers = doc
        .entry("mcp_servers")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let servers = servers
        .as_table_mut()
        .ok_or_else(|| "mcp_servers is not a TOML table".to_string())?;
    // Emit [mcp_servers.everything] directly, without a bare [mcp_servers] header.
    servers.set_implicit(true);

    if let Some(existing) = servers.get(SERVER_NAME) {
        let command_matches = existing
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| c == exe_str)
            .unwrap_or(false);
        let args_match = existing
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).eq(["--mcp"]))
            .unwrap_or(false);
        if command_matches && args_match {
            return Ok(RegisterOutcome::Unchanged);
        }
    }

    let mut server = toml_edit::Table::new();
    server["command"] = toml_edit::value(exe_str.as_ref());
    let mut args = toml_edit::Array::new();
    args.push("--mcp");
    server["args"] = toml_edit::value(args);
    servers.insert(SERVER_NAME, toml_edit::Item::Table(server));

    atomic_write(config_path, &doc.to_string())?;
    Ok(RegisterOutcome::Updated)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static CASE_SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_case_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "everything_mcp_{}_{}_{}",
            name,
            std::process::id(),
            CASE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_test_db(root: &Path) -> PathBuf {
        let db_path = root.join("index.db");
        crate::init_db_tables(&db_path).unwrap();
        crate::ensure_db_indexes(&db_path).unwrap();
        let conn = crate::db_connection(&db_path).unwrap();
        let rows: &[(&str, &str, &str, i64, Option<&str>)] = &[
            ("/home/u/docs", "docs", "/home/u", 1, None),
            ("/home/u/docs/report.pdf", "report.pdf", "/home/u/docs", 0, Some("pdf")),
            ("/home/u/docs/report_final.pdf", "report_final.pdf", "/home/u/docs", 0, Some("pdf")),
            ("/home/u/src/main.rs", "main.rs", "/home/u/src", 0, Some("rs")),
            ("/home/u/src/query.rs", "query.rs", "/home/u/src", 0, Some("rs")),
            ("/home/u/notes.txt", "notes.txt", "/home/u", 0, Some("txt")),
        ];
        for (i, (path, name, dir, is_dir, ext)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO entries(path, name, dir, is_dir, ext, mtime, size, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                rusqlite::params![path, name, dir, is_dir, ext, 100 + i as i64, 10 * i as i64],
            )
            .unwrap();
        }
        db_path
    }

    fn call_search(server: &mut McpServer, arguments: Value) -> (String, bool) {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "search", "arguments": arguments },
        });
        let resp = server.handle_line(&req.to_string()).unwrap();
        let result = &resp["result"];
        (
            result["content"][0]["text"].as_str().unwrap().to_string(),
            result["isError"].as_bool().unwrap(),
        )
    }

    fn test_server(root: &Path) -> McpServer {
        McpServer::new(build_test_db(root), root.to_path_buf())
    }

    #[test]
    fn initialize_echoes_supported_version_and_falls_back() {
        let root = temp_case_dir("init");
        let mut server = test_server(&root);
        let req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" },
        });
        let resp = server.handle_line(&req.to_string()).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);

        let req = json!({
            "jsonrpc": "2.0", "id": 2, "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" },
        });
        let resp = server.handle_line(&req.to_string()).unwrap();
        assert_eq!(resp["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
    }

    #[test]
    fn notifications_and_unknown_methods() {
        let root = temp_case_dir("dispatch");
        let mut server = test_server(&root);
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(server.handle_line(&note.to_string()).is_none());

        let bogus = json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" });
        let resp = server.handle_line(&bogus.to_string()).unwrap();
        assert_eq!(resp["error"]["code"], -32601);

        let resp = server.handle_line("{not json").unwrap();
        assert_eq!(resp["error"]["code"], -32700);

        let ping = json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" });
        let resp = server.handle_line(&ping.to_string()).unwrap();
        assert_eq!(resp["result"], json!({}));
    }

    #[test]
    fn tools_list_exposes_search() {
        let root = temp_case_dir("tools_list");
        let mut server = test_server(&root);
        let req = json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list" });
        let resp = server.handle_line(&req.to_string()).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "search");
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
    }

    #[test]
    fn search_name_glob_ext_path_modes() {
        let root = temp_case_dir("modes");
        let mut server = test_server(&root);

        let (text, is_error) = call_search(&mut server, json!({ "query": "report" }));
        assert!(!is_error);
        assert!(text.contains("/home/u/docs/report.pdf"));
        assert!(text.contains("/home/u/docs/report_final.pdf"));

        let (text, _) = call_search(&mut server, json!({ "query": "report*.pdf" }));
        assert!(text.contains("mode: glob"));
        assert!(text.contains("report_final.pdf"));

        let (text, _) = call_search(&mut server, json!({ "query": "*.rs" }));
        assert!(text.contains("mode: ext"));
        assert!(text.contains("main.rs") && text.contains("query.rs"));

        let (text, _) = call_search(&mut server, json!({ "query": "docs/report" }));
        assert!(text.contains("mode: path"));
        assert!(text.contains("/home/u/docs/report.pdf"));
        assert!(!text.contains("main.rs"));

        // Directory entries are marked with a trailing separator.
        let (text, _) = call_search(&mut server, json!({ "query": "docs" }));
        let dir_line = format!("/home/u/docs{}", std::path::MAIN_SEPARATOR);
        assert!(text.contains(&dir_line));

        let (text, is_error) = call_search(&mut server, json!({ "query": "zzz_nothing" }));
        assert!(!is_error);
        assert!(text.starts_with("No matches"));
    }

    #[test]
    fn search_rejects_bad_arguments() {
        let root = temp_case_dir("bad_args");
        let mut server = test_server(&root);
        let (text, is_error) = call_search(&mut server, json!({}));
        assert!(is_error);
        assert!(text.contains("`query` is required"));

        let (text, is_error) = call_search(&mut server, json!({ "query": "a", "sort_by": "bogus" }));
        assert!(is_error);
        assert!(text.contains("Invalid sort_by"));
    }

    #[test]
    fn search_pagination_reports_more() {
        let root = temp_case_dir("paging");
        let mut server = test_server(&root);
        let (text, _) = call_search(&mut server, json!({ "query": "report", "limit": 1 }));
        assert!(text.contains("more may exist"));
        assert!(text.contains("report.pdf"));
        let (text, _) =
            call_search(&mut server, json!({ "query": "report", "limit": 1, "offset": 1 }));
        assert!(text.contains("offset: 1"));
        assert!(text.contains("report_final.pdf"));
    }

    #[test]
    fn missing_db_is_a_tool_error_not_a_crash() {
        let root = temp_case_dir("missing_db");
        let mut server = McpServer::new(root.join("nope.db"), root.clone());
        let (text, is_error) = call_search(&mut server, json!({ "query": "x" }));
        assert!(is_error);
        assert!(text.contains("Index database not found"));
    }

    #[test]
    fn register_claude_creates_updates_and_stays_idempotent() {
        let root = temp_case_dir("claude_reg");
        let config = root.join(".claude.json");
        let exe = Path::new("/Applications/Everything.app/Contents/MacOS/Everything");

        // Not detected: neither ~/.claude.json nor ~/.claude exist.
        assert_eq!(
            register_claude(&config, exe).unwrap(),
            RegisterOutcome::Skipped("Claude Code not detected")
        );

        // Detected via ~/.claude dir: file is created from scratch.
        fs::create_dir_all(root.join(".claude")).unwrap();
        assert_eq!(register_claude(&config, exe).unwrap(), RegisterOutcome::Updated);
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed["mcpServers"]["everything"]["args"], json!(["--mcp"]));
        assert_eq!(
            parsed["mcpServers"]["everything"]["command"],
            json!(exe.to_string_lossy())
        );

        // Second run: no rewrite.
        assert_eq!(register_claude(&config, exe).unwrap(), RegisterOutcome::Unchanged);

        // Existing unrelated keys and servers survive an update.
        fs::write(
            &config,
            r#"{"theme":"dark","mcpServers":{"other":{"type":"stdio","command":"x"}},"projects":{"/p":{"history":[1,2]}}}"#,
        )
        .unwrap();
        assert_eq!(register_claude(&config, exe).unwrap(), RegisterOutcome::Updated);
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed["theme"], "dark");
        assert_eq!(parsed["mcpServers"]["other"]["command"], "x");
        assert_eq!(parsed["projects"]["/p"]["history"], json!([1, 2]));
        assert_eq!(parsed["mcpServers"]["everything"]["type"], "stdio");

        // Corrupt file: refuse to clobber.
        fs::write(&config, "{oops").unwrap();
        assert!(register_claude(&config, exe).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), "{oops");
    }

    #[test]
    fn register_codex_creates_updates_and_preserves_toml() {
        let root = temp_case_dir("codex_reg");
        let config = root.join(".codex").join("config.toml");
        let exe = Path::new("/usr/local/bin/everything");

        assert_eq!(
            register_codex(&config, exe).unwrap(),
            RegisterOutcome::Skipped("Codex not detected")
        );

        fs::create_dir_all(root.join(".codex")).unwrap();
        assert_eq!(register_codex(&config, exe).unwrap(), RegisterOutcome::Updated);
        let raw = fs::read_to_string(&config).unwrap();
        assert!(raw.contains("[mcp_servers.everything]"), "got: {raw}");
        assert!(raw.contains(r#"command = "/usr/local/bin/everything""#));
        assert!(raw.contains(r#"args = ["--mcp"]"#));

        assert_eq!(register_codex(&config, exe).unwrap(), RegisterOutcome::Unchanged);

        // Comments and unrelated settings survive; stale entry gets replaced.
        fs::write(
            &config,
            "# my codex config\nmodel = \"o3\"\n\n[mcp_servers.everything]\ncommand = \"/old/path\"\nargs = [\"--mcp\"]\n",
        )
        .unwrap();
        assert_eq!(register_codex(&config, exe).unwrap(), RegisterOutcome::Updated);
        let raw = fs::read_to_string(&config).unwrap();
        assert!(raw.contains("# my codex config"));
        assert!(raw.contains("model = \"o3\""));
        assert!(raw.contains(r#"command = "/usr/local/bin/everything""#));
        assert!(!raw.contains("/old/path"));

        // Corrupt TOML: refuse to clobber.
        fs::write(&config, "[broken").unwrap();
        assert!(register_codex(&config, exe).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), "[broken");
    }
}
