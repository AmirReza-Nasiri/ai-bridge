//! Discover a codex MCP server's TOOLS by briefly launching it and doing the MCP
//! handshake + `tools/list` — NO tool call, so it can't elicit/hang (verified). Used
//! by `review-mcp` per-tool control to SHOW a server's tools and compute the
//! `disabledTools` denylist for "some" mode.
//!
//! SAFETY: a server's startup can run code / open a browser / hit the network, so
//! discovery is ONLY ever invoked on an EXPLICIT user action (a CLI command / a TUI
//! keypress) — NEVER automatically. Results are cached WITH a config fingerprint so
//! per-tool enforcement is never applied from stale discovery (the caller fails
//! closed when the fingerprint no longer matches). These are the user's OWN
//! configured servers (trusted); we only ever ask them to list tools.

use aibridge_platform::{DefaultPlatform, Platform};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long to wait for a server to start + answer the handshake before giving up.
/// npm-shim cold starts (npx downloading a package) can be slow, hence generous.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(90);

/// A codex MCP server's launch spec (from `~/.codex/config.toml`).
#[derive(Clone)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The server's working dir from config (relative commands/args need it), if any.
    pub cwd: Option<String>,
}

impl ServerSpec {
    /// Stable SHA-256 fingerprint of the launch spec. When it changes (command/args/
    /// env edited, or a package pinned differently), cached tools are STALE and the
    /// caller must not enforce a per-tool denylist from them (fail-closed).
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.command.as_bytes());
        h.update([0u8]);
        for a in &self.args {
            h.update(a.as_bytes());
            h.update([0u8]);
        }
        let mut env = self.env.clone();
        env.sort();
        for (k, v) in &env {
            h.update(k.as_bytes());
            h.update(b"=");
            h.update(v.as_bytes());
            h.update([0u8]);
        }
        h.update(self.cwd.as_deref().unwrap_or("").as_bytes());
        h.update([0u8]);
        format!("{:x}", h.finalize())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build the spawn command. Direct for a real `.exe`; via `cmd /D /S /C` for a
/// Windows `.cmd`/`.bat` shim (npx etc.) — this runs from the CLI, which HAS a
/// console, so `cmd` doesn't hang (unlike the no-console MCP host). On Unix, exec
/// the resolved path directly.
fn build_command(command: &str, args: &[String]) -> std::process::Command {
    let resolved = DefaultPlatform::find_executable(command).ok();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(path) = &resolved {
            let lower = path.to_string_lossy().to_lowercase();
            if lower.ends_with(".cmd") || lower.ends_with(".bat") {
                // `/D /S /C "<exe> <args...>"` with each token double-quoted; `/S`
                // makes cmd strip only the outer pair and pass the rest verbatim.
                let mut line = format!("\"{}\"", path.display());
                for a in args {
                    line.push(' ');
                    line.push('"');
                    line.push_str(a);
                    line.push('"');
                }
                let mut c = std::process::Command::new("cmd");
                c.raw_arg("/D")
                    .raw_arg("/S")
                    .raw_arg("/C")
                    .raw_arg(format!("\"{line}\""));
                c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                return c;
            }
        }
        let mut c = std::process::Command::new(resolved.unwrap_or_else(|| PathBuf::from(command)));
        c.args(args);
        c.creation_flags(0x0800_0000);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = std::process::Command::new(resolved.unwrap_or_else(|| PathBuf::from(command)));
        c.args(args);
        use std::os::unix::process::CommandExt;
        c.process_group(0); // own group so we can kill the whole tree
        c
    }
}

/// Kill the discovery child's whole process tree (npx spawns node, etc.).
fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .status();
    }
}

/// True for a JSON-RPC NOTIFICATION (has `method`, no `id`).
fn is_notification(v: &Value) -> bool {
    v.get("method").is_some() && v.get("id").is_none()
}
/// True for a server→client REQUEST (has both `method` and `id`).
fn is_request(v: &Value) -> bool {
    v.get("method").is_some() && v.get("id").is_some()
}

/// Minimal answer to a server→client request so the server never blocks on this
/// headless client during the handshake (we never call a tool, so no elicitation,
/// but a server may `ping` / ask for `roots`).
fn answer_request(v: &Value) -> Value {
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    match v.get("method").and_then(Value::as_str).unwrap_or("") {
        "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
        "roots/list" => json!({"jsonrpc":"2.0","id":id,"result":{"roots":[]}}),
        "elicitation/create" => {
            json!({"jsonrpc":"2.0","id":id,"result":{"action":"decline"}})
        }
        _ => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}}),
    }
}

/// Run the discovery handshake on the child's pipes (own thread). Sends the parsed
/// tool names (or an error) on `tx`. Ordering: initialize → wait for its response
/// (answering inbound requests, skipping notifications) → initialized + tools/list →
/// wait for the tools/list response → parse `result.tools[].name`.
fn handshake(
    mut stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    tx: mpsc::Sender<Result<Vec<String>, String>>,
) {
    let send_line = |stdin: &mut std::process::ChildStdin, v: &Value| -> std::io::Result<()> {
        stdin.write_all(v.to_string().as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    };
    let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2024-11-05","capabilities":{},
        "clientInfo":{"name":"aibridge-discover","version":"1"}}});
    if let Err(e) = send_line(&mut stdin, &init) {
        let _ = tx.send(Err(format!("write initialize: {e}")));
        return;
    }
    let mut reader = BufReader::new(stdout);
    let mut sent_list = false;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.send(Err("server closed before tools/list".to_string()));
                return;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Err(format!("read: {e}")));
                return;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
            continue; // tolerate non-JSON log noise on stdout
        };
        if is_request(&v) {
            let _ = send_line(&mut stdin, &answer_request(&v));
            continue;
        }
        if is_notification(&v) {
            continue;
        }
        // It's a response (has id, no method). Match by id.
        let id = v.get("id").and_then(Value::as_i64);
        if id == Some(1) && !sent_list {
            // initialize ack → send initialized + tools/list.
            let inited = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
            let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
            if send_line(&mut stdin, &inited)
                .and_then(|_| send_line(&mut stdin, &list))
                .is_err()
            {
                let _ = tx.send(Err("write tools/list failed".to_string()));
                return;
            }
            sent_list = true;
            continue;
        }
        if id == Some(2) {
            let tools = v
                .get("result")
                .and_then(|r| r.get("tools"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("name").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                });
            let _ = tx.send(match tools {
                Some(t) => Ok(t),
                None => Err("tools/list returned no tools array".to_string()),
            });
            return;
        }
    }
}

/// Launch `spec`, handshake, and return its tool names (sorted, deduped). Kills the
/// process (tree) on success/timeout/error. NEVER calls a tool.
pub fn discover(spec: &ServerSpec) -> Result<Vec<String>, String> {
    let mut cmd = build_command(&spec.command, &spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir); // honor a server's configured working dir
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn '{}': {e}", spec.command))?;
    let pid = child.id();
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(err);
            let mut b = String::new();
            while r.read_line(&mut b).unwrap_or(0) > 0 {
                b.clear();
            }
        });
    }
    let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
        (Some(i), Some(o)) => (i, o),
        _ => {
            kill_tree(pid);
            let _ = child.kill();
            return Err("child pipes unavailable".to_string());
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || handshake(stdin, stdout, tx));
    let result = rx
        .recv_timeout(DISCOVER_TIMEOUT)
        .unwrap_or_else(|_| Err(format!("timed out after {}s", DISCOVER_TIMEOUT.as_secs())));
    kill_tree(pid);
    let _ = child.kill();
    let _ = child.wait();
    result.map(|mut tools| {
        tools.sort();
        tools.dedup();
        tools
    })
}

// ---------------------------------------------------------------------------
// Cache: ~/.ai-bridge/mcp-tools-cache.json — { "<server>": CacheEntry }.
// ---------------------------------------------------------------------------

/// One server's cached discovery, with the fingerprint it was discovered under so a
/// later config change invalidates it (per-tool enforcement is fail-closed on a miss).
pub struct CacheEntry {
    pub tools: Vec<String>,
    pub discovered_ms: u64,
    pub fingerprint: String,
}

fn cache_path() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(
        Path::new(&home)
            .join(".ai-bridge")
            .join("mcp-tools-cache.json"),
    )
}

fn read_cache_raw() -> Value {
    cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// The cached entry for `name`, or `None` if absent/unreadable.
pub fn cached(name: &str) -> Option<CacheEntry> {
    let v = read_cache_raw();
    let e = v.get(name)?;
    Some(CacheEntry {
        tools: e
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        discovered_ms: e.get("discovered_ms").and_then(Value::as_u64).unwrap_or(0),
        fingerprint: e
            .get("fingerprint")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// The cached tools ONLY if the fingerprint still matches `spec` (i.e. FRESH). Used
/// by per-tool enforcement, which must never apply a denylist computed from stale
/// discovery. `None` ⇒ caller fails closed.
pub fn fresh_tools(spec: &ServerSpec) -> Option<Vec<String>> {
    let e = cached(&spec.name)?;
    (e.fingerprint == spec.fingerprint()).then_some(e.tools)
}

/// Write/replace the cache entry for `spec` after a successful discovery (atomic).
pub fn store(spec: &ServerSpec, tools: &[String]) {
    let Some(path) = cache_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let mut v = read_cache_raw();
    if let Some(o) = v.as_object_mut() {
        o.insert(
            spec.name.clone(),
            json!({
                "tools": tools,
                "discovered_ms": now_ms(),
                "fingerprint": spec.fingerprint(),
            }),
        );
    }
    let body = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Discover `spec` AND cache the result (the normal entry point for the CLI/TUI).
pub fn discover_and_cache(spec: &ServerSpec) -> Result<Vec<String>, String> {
    let tools = discover(spec)?;
    store(spec, &tools);
    Ok(tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let a = ServerSpec {
            name: "s".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "pkg".into()],
            env: vec![("K".into(), "V".into())],
            cwd: None,
        };
        let b = a.clone();
        assert_eq!(a.fingerprint(), b.fingerprint());
        let mut c = a.clone();
        c.args.push("--flag".into());
        assert_ne!(a.fingerprint(), c.fingerprint());
        let mut d = a.clone();
        d.env = vec![("K".into(), "V2".into())];
        assert_ne!(a.fingerprint(), d.fingerprint());
        // cwd is part of the launch identity → changing it changes the fingerprint.
        let mut e = a.clone();
        e.cwd = Some("/proj".into());
        assert_ne!(a.fingerprint(), e.fingerprint());
    }

    #[test]
    fn fingerprint_ignores_env_order() {
        let a = ServerSpec {
            name: "s".into(),
            command: "x".into(),
            args: vec![],
            env: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
            cwd: None,
        };
        let mut b = a.clone();
        b.env.reverse();
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn answer_request_shapes() {
        let ping = json!({"jsonrpc":"2.0","id":7,"method":"ping"});
        assert_eq!(answer_request(&ping)["result"], json!({}));
        let elicit = json!({"jsonrpc":"2.0","id":8,"method":"elicitation/create","params":{}});
        assert_eq!(answer_request(&elicit)["result"]["action"], "decline");
        let unknown = json!({"jsonrpc":"2.0","id":9,"method":"weird/thing"});
        assert_eq!(answer_request(&unknown)["error"]["code"], -32601);
    }

    #[test]
    fn classify_messages() {
        assert!(is_notification(
            &json!({"jsonrpc":"2.0","method":"notifications/x"})
        ));
        assert!(!is_notification(
            &json!({"jsonrpc":"2.0","id":1,"result":{}})
        ));
        assert!(is_request(&json!({"jsonrpc":"2.0","id":1,"method":"ping"})));
        assert!(!is_request(&json!({"jsonrpc":"2.0","id":1,"result":{}})));
    }
}
