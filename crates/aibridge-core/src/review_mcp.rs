//! Review-time codex MCP-server policy: which of codex's own configured MCP servers
//! stay ENABLED when AI Bridge spawns its warm review child.
//!
//! WHY: a code/plan review is pure reasoning over the diff/plan we hand codex. When
//! the model invokes one of the user's browser/scrape MCP servers (chrome-devtools /
//! firecrawl / playwright / scrapling) mid-review, it elicits or runs a long op and
//! the review STALLS (observed: a ~10-min hang on `mcp_tool_call_begin`). That
//! elicitation is between codex and ITS sub-server — one level below AI Bridge — so
//! AI Bridge can't answer it (the v0.5.6 elicitation fix doesn't reach it).
//!
//! FIX: when AI Bridge spawns its warm review child, pass per-server
//! `-c mcp_servers.<name>.enabled=<bool>` overrides (proven reliable; a blanket
//! `mcp_servers={}` does NOT work — codex merges the table). The user's real
//! `~/.codex/config.toml` is UNTOUCHED — codex keeps every server everywhere else;
//! only AI Bridge's review child is constrained. The set is USER-CONTROLLED and
//! persisted (`aibridge review-mcp …` → `~/.ai-bridge/review-mcp.json`), default NONE
//! (reviews run tool-free out of the box; opt servers back in explicitly).
//!
//! This is RELIABILITY isolation, NOT a security sandbox (Codex review): it stops
//! accidental tool invocation during reviews.
//!
//! DISPLAY + discovery use codex's AUTHORITATIVE inventory ([`codex_inventory`] =
//! `codex mcp list --json`, codex's own resolver → correct cross-platform + sees
//! project/profile/`$CODEX_HOME` config). ENFORCEMENT ([`spawn_overrides`]) still reads
//! the USER `~/.codex/config.toml` directly (it runs in the no-console MCP host where
//! spawning codex is unsafe), so review-override coverage is USER-CONFIG ONLY — a
//! project/profile/system-scoped server is DETECTED + WARNED (doctor) but NOT disabled
//! by the override. (Codex-reviewed B+; closing that enforcement gap is a follow-up.)

use aibridge_platform::{DefaultPlatform, Platform};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A codex MCP server as seen in the config, with a flattened command line used only
/// for the "looks browser/scrape" heuristic in `doctor`.
pub struct ServerInfo {
    pub name: String,
    pub cmdline: String,
}

fn home() -> Option<String> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
}

/// The persisted review allowlist file (`~/.ai-bridge/review-mcp.json`).
fn config_path() -> Option<PathBuf> {
    Some(
        Path::new(&home()?)
            .join(".ai-bridge")
            .join("review-mcp.json"),
    )
}

/// codex config path: `$CODEX_HOME/config.toml` else `~/.codex/config.toml`.
fn codex_config_path() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CODEX_HOME") {
        return Some(Path::new(&h).join("config.toml"));
    }
    Some(Path::new(&home()?).join(".codex").join("config.toml"))
}

/// Parse `[mcp_servers.<name>]` entries from a codex config.toml string with a REAL
/// TOML parser (robust to quoted/dotted names, unlike a regex). Each server's
/// command+args are flattened into `cmdline` for the doctor heuristic only.
fn servers_from_toml(s: &str) -> Vec<ServerInfo> {
    // Parse with the native TOML type (handles datetimes etc. that a serde_json::Value
    // target would choke on). A parse error → empty HERE, but this feeds only the
    // cmdline heuristic; ENFORCEMENT enumerates via `names_from_config`, which falls
    // back to a header scan and returns `None` (fail-closed) on a truly unparseable
    // config — so this emptiness is never the safety path.
    let parsed: toml::Value = match toml::from_str(s) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(table) = parsed.get("mcp_servers").and_then(|v| v.as_table()) else {
        return Vec::new();
    };
    table
        .iter()
        .map(|(name, def)| {
            let cmd = def.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let args = def
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            ServerInfo {
                name: name.clone(),
                cmdline: format!("{cmd} {args}").trim().to_string(),
            }
        })
        .collect()
}

/// All codex MCP servers defined in the user's config (empty if none / unreadable).
pub fn codex_servers() -> Vec<ServerInfo> {
    codex_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| servers_from_toml(&s))
        .unwrap_or_default()
}

/// Lenient fallback enumeration: scan `[mcp_servers.<name>]` headers line-by-line
/// (mirrors doctor's proven scan). UNIONed with the strict TOML parse so a config the
/// strict parser trips on — but codex still loads — does NOT leave a server
/// un-disabled, which would be FAIL-OPEN (codex would inherit it during the review).
/// Best-effort for BARE/simple header names (the strict TOML path handles quoted or
/// dotted names correctly); only relevant when strict parsing already failed.
fn header_scan_names(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in s.lines() {
        if let Some(rest) = line.trim().strip_prefix("[mcp_servers.") {
            let raw: String = rest.chars().take_while(|&c| c != '.' && c != ']').collect();
            let name = raw.trim().trim_matches('"').to_string();
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

/// Enumerate codex server names from a config string. Pure (unit-testable).
/// `Some(names)` unions strict TOML keys with the header scan; `Some([])` = genuinely
/// no servers. `None` = the config is present but UNPARSEABLE with no recoverable
/// headers — the caller must FAIL CLOSED rather than inherit unfiltered servers.
fn names_from_config(s: &str) -> Option<Vec<String>> {
    let toml_ok = toml::from_str::<toml::Value>(s).is_ok();
    let mut names: Vec<String> = if toml_ok {
        servers_from_toml(s).into_iter().map(|si| si.name).collect()
    } else {
        Vec::new()
    };
    for n in header_scan_names(s) {
        if !names.iter().any(|x| x == &n) {
            names.push(n);
        }
    }
    if !toml_ok && names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// The codex server names for enforcement/CLI. `None` ⇒ a config is present but could
/// NOT be read/parsed (the caller FAILS CLOSED — it never inherits unfiltered servers,
/// the stall bug this prevents). `Some([])` ⇒ no config / no servers (safe: reviews are
/// tool-free anyway). `Some(names)` ⇒ the servers to apply the allowlist against.
pub fn codex_server_names() -> Option<Vec<String>> {
    let Some(path) = codex_config_path() else {
        return Some(Vec::new()); // no home → no discoverable config → codex has none
    };
    if !path.exists() {
        return Some(Vec::new()); // genuinely absent → codex has no servers → safe
    }
    match std::fs::read_to_string(&path) {
        Ok(s) => names_from_config(&s),
        Err(_) => None, // present but unreadable → fail closed
    }
}

/// One codex MCP server from the AUTHORITATIVE `codex mcp list --json` inventory.
pub struct CodexServer {
    pub name: String,
    /// codex's OWN enabled state (its config) — NOT our review-mcp policy.
    pub enabled: bool,
    /// Transport type ("stdio" / "streamable_http" / …). Only stdio is discoverable.
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    /// Explicit env from config. NOTE: codex's `env_vars` (names inherited from the
    /// parent environment) are NOT stored — discovery inherits the parent env anyway,
    /// so they're applied automatically; only this explicit map is set on top.
    pub env: Vec<(String, String)>,
    /// The server's working dir, if config sets one (relative commands/args need it).
    pub cwd: Option<String>,
}

/// Result of asking codex for its MCP inventory.
pub enum Inventory {
    /// codex answered: the servers it actually resolves (incl. project `.codex/config.toml`
    /// + profiles + `$CODEX_HOME` — things a raw `~/.codex/config.toml` read misses).
    Available(Vec<CodexServer>),
    /// codex CLI not found / errored / unparseable — callers must NOT report "none
    /// configured" (show "unknown"); they may fall back to the file read.
    Unavailable(String),
}

fn parse_codex_server(v: &Value) -> Option<CodexServer> {
    let name = v.get("name")?.as_str()?.to_string();
    let enabled = v.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let t = v.get("transport");
    let get = |k: &str| t.and_then(|t| t.get(k));
    let transport = get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let command = get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let args = get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let env = get("env")
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let cwd = get("cwd").and_then(Value::as_str).map(str::to_string);
    Some(CodexServer {
        name,
        enabled,
        transport,
        command,
        args,
        env,
        cwd,
    })
}

/// Authoritative MCP inventory via `codex mcp list --json` — codex's OWN config resolver,
/// so it's correct CROSS-PLATFORM and also sees project `.codex/config.toml` + profiles +
/// env that a raw `~/.codex/config.toml` read misses. Run from `cwd` (project config
/// resolves relative to it); inherits `$CODEX_HOME`. Launched via `spawn_plan` (node-direct
/// for the Windows npm shim) so it works even from a no-console host. Used by doctor / TUI /
/// discovery (NOT spawn_overrides, which keeps the fast file read).
pub fn codex_inventory(cwd: &str) -> Inventory {
    let exe = match DefaultPlatform::find_executable("codex") {
        Ok(e) => e,
        Err(_) => return Inventory::Unavailable("codex CLI not found on PATH".to_string()),
    };
    use std::io::Read;
    use std::time::{Duration, Instant};
    let mut child = match DefaultPlatform::spawn_plan(&exe)
        .into_command()
        .args(["mcp", "list", "--json"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Inventory::Unavailable(format!("running codex mcp list: {e}")),
    };
    // Read stdout concurrently (so a full pipe can't deadlock the wait) and bound the
    // whole thing with a timeout — a stalled codex must degrade to Unavailable, never hang.
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Inventory::Unavailable(
                        "timed out querying codex mcp list --json".to_string(),
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait(); // reap
                return Inventory::Unavailable(format!("waiting on codex: {e}"));
            }
        }
    };
    if !status.success() {
        return Inventory::Unavailable(format!(
            "codex mcp list exited {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "by signal".to_string())
        ));
    }
    let bytes = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    let v: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return Inventory::Unavailable(format!("codex mcp list JSON: {e}")),
    };
    match v.as_array() {
        Some(arr) => Inventory::Available(arr.iter().filter_map(parse_codex_server).collect()),
        None => Inventory::Unavailable("codex mcp list JSON was not an array".to_string()),
    }
}

/// Whether a server (by name) looks browser/scrape — uses its parsed command line when
/// available, else the name alone (so a fallback-enumerated server is still judged).
pub fn server_looks_interactive(name: &str) -> bool {
    match codex_servers().into_iter().find(|i| i.name == name) {
        Some(info) => looks_interactive(&info),
        None => looks_interactive(&ServerInfo {
            name: name.to_string(),
            cmdline: String::new(),
        }),
    }
}

/// Read the whole review-mcp policy file (`{ "allow": [...], "server_tools": {...} }`).
/// `{}` when absent/unreadable/malformed → default policy (no servers, no per-tool).
fn read_config() -> Value {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// Atomically write the whole policy file (preserves both `allow` and `server_tools`).
fn write_config(v: &Value) -> std::io::Result<()> {
    let path = config_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string());
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::other("rename failed"));
    }
    Ok(())
}

fn allow_from(cfg: &Value) -> Vec<String> {
    cfg.get("allow")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Server names ENABLED during reviews (mode all OR some). Default empty → tool-free.
pub fn allowlist() -> Vec<String> {
    allow_from(&read_config())
}

/// A server's review mode (additive schema: a name in `allow` with a
/// `server_tools.<name>.enabled_tools` entry is "some", else "all"; not in `allow` is
/// "off"). `Some(list)` carries the user's ALLOWLIST of tool names to keep enabled.
pub enum Mode {
    Off,
    All,
    Some(Vec<String>),
}

pub fn server_mode(name: &str) -> Mode {
    let cfg = read_config();
    if !allow_from(&cfg).iter().any(|a| a == name) {
        return Mode::Off;
    }
    match cfg
        .get("server_tools")
        .and_then(|m| m.get(name))
        .and_then(|s| s.get("enabled_tools"))
        .and_then(Value::as_array)
    {
        Some(arr) => Mode::Some(
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
        ),
        None => Mode::All,
    }
}

/// Typed lookup result for a codex MCP server by name. Distinguishes a stdio server
/// (discoverable), a non-stdio server (HTTP/SSE — present in codex's config but tool
/// discovery requires stdio), and "not configured". Codex pointed out the v1
/// confusion: a HTTP-transport server like context7 was reported as "is not a codex
/// MCP server" because `server_spec` only returned `Some` for stdio.
#[derive(Debug)]
pub enum ServerLookup {
    /// Stdio server with a discoverable command — pass to `tool_discovery::discover`.
    Stdio(crate::tool_discovery::ServerSpec),
    /// Configured but uses a non-stdio transport (e.g. `streamable_http`, `sse`). The
    /// server-level allow/disallow still applies during reviews; only per-tool
    /// discovery via launch is unavailable.
    NonStdio { transport: String },
    /// The name isn't in codex's resolved inventory (and not in the fallback config).
    Unknown,
    /// codex CLI couldn't be queried AND the fallback config read also failed.
    Unavailable(String),
}

/// The launch spec (command/args/env/cwd) for `name`, for tool discovery + the cache
/// fingerprint. Thin wrapper over `server_lookup` kept for back-compat with the
/// existing per-tool enforcement paths (which only need to know "is this discoverable
/// stdio yes/no").
pub fn server_spec(name: &str) -> Option<crate::tool_discovery::ServerSpec> {
    match server_lookup(name) {
        ServerLookup::Stdio(spec) => Some(spec),
        _ => None,
    }
}

/// Authoritative-then-fallback lookup for a codex MCP server. Used by `discover_server`
/// to give a transport-aware error message (rather than the v1 "is not a codex MCP
/// server", which confused HTTP-transport users).
pub fn server_lookup(name: &str) -> ServerLookup {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    match codex_inventory(&cwd) {
        // codex is AUTHORITATIVE: it tells us the transport, so we can distinguish
        // stdio from HTTP/SSE without a second guess.
        Inventory::Available(servers) => match servers.into_iter().find(|s| s.name == name) {
            None => ServerLookup::Unknown,
            Some(s) if !s.command.is_empty() => {
                ServerLookup::Stdio(crate::tool_discovery::ServerSpec {
                    name: s.name,
                    command: s.command,
                    args: s.args,
                    env: s.env,
                    cwd: s.cwd,
                })
            }
            Some(s) => ServerLookup::NonStdio {
                transport: if s.transport.is_empty() {
                    "non-stdio".to_string()
                } else {
                    s.transport
                },
            },
        },
        // codex couldn't be queried → best-effort direct config read. The config file
        // doesn't carry the transport type explicitly, so we infer: `command` present
        // ⇒ stdio; `url` present and no `command` ⇒ HTTP-like (Unknown transport name
        // since the file doesn't say). Falling through to Unknown when neither is
        // present matches the original `server_spec` semantics.
        Inventory::Unavailable(why) => {
            let Some(s) = codex_config_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
                return ServerLookup::Unavailable(why);
            };
            let Ok(parsed) = toml::from_str::<toml::Value>(&s) else {
                return ServerLookup::Unavailable(why);
            };
            let Some(def) = parsed
                .get("mcp_servers")
                .and_then(|v| v.as_table())
                .and_then(|t| t.get(name))
            else {
                return ServerLookup::Unknown;
            };
            if let Some(command) = def.get("command").and_then(|v| v.as_str()) {
                let args = def
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let env = def
                    .get("env")
                    .and_then(|v| v.as_table())
                    .map(|t| {
                        t.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let cwd = def.get("cwd").and_then(|v| v.as_str()).map(str::to_string);
                ServerLookup::Stdio(crate::tool_discovery::ServerSpec {
                    name: name.to_string(),
                    command: command.to_string(),
                    args,
                    env,
                    cwd,
                })
            } else if def.get("url").and_then(|v| v.as_str()).is_some() {
                ServerLookup::NonStdio {
                    transport: "non-stdio".to_string(),
                }
            } else {
                ServerLookup::Unknown
            }
        }
    }
}

/// Serialize a list of tool names as a TOML inline array for a `-c` value. serde_json
/// is a real serializer whose output is valid TOML for the ASCII identifier names
/// `tools/list` returns (we only ever pass DISCOVERED names) — not hand-built quoting.
fn toml_array(items: &[String]) -> String {
    serde_json::to_string(items).unwrap_or_else(|_| "[]".to_string())
}

/// A safe TOML dotted-path segment for a server name in `mcp_servers.<seg>.enabled`:
/// a bare key as-is, otherwise a quoted key. `None` when it can't be safely emitted
/// (contains a double-quote, backslash, or control char) — caller SKIPS it so a
/// malformed name never produces a wrong override.
fn key_segment(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let bare = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if bare {
        Some(name.to_string())
    } else if name.contains('"') || name.contains('\\') || name.chars().any(|c| c.is_control()) {
        None
    } else {
        Some(format!("\"{name}\""))
    }
}

/// Pure: the `-c` args for ONE server given its mode + (for `some`) the FRESHLY
/// discovered tool list. Off→`enabled=false`; All→`enabled=true`; Some+fresh→
/// `enabled=true` plus `disabledTools=(discovered − enabled_tools)`; Some with NO
/// fresh discovery (`fresh=None`)→`enabled=false` (FAIL CLOSED — never apply a
/// per-tool denylist from stale/unknown discovery, which would silently re-enable a
/// newly-added tool). Unit-testable (seg + mode + fresh in).
fn overrides_for(seg: &str, mode: &Mode, fresh: Option<&[String]>) -> Vec<String> {
    let off = || vec!["-c".to_string(), format!("mcp_servers.{seg}.enabled=false")];
    match mode {
        Mode::Off => off(),
        Mode::All => vec!["-c".to_string(), format!("mcp_servers.{seg}.enabled=true")],
        Mode::Some(enabled_tools) => match fresh {
            Some(discovered) => {
                let disabled: Vec<String> = discovered
                    .iter()
                    .filter(|t| !enabled_tools.iter().any(|e| e == *t))
                    .cloned()
                    .collect();
                let mut a = vec!["-c".to_string(), format!("mcp_servers.{seg}.enabled=true")];
                if !disabled.is_empty() {
                    a.push("-c".to_string());
                    a.push(format!(
                        "mcp_servers.{seg}.disabledTools={}",
                        toml_array(&disabled)
                    ));
                }
                a
            }
            None => off(), // stale/unknown discovery ⇒ fail closed
        },
    }
}

/// Args to append to the warm review child spawn (AFTER `mcp-server`), or `None` when
/// the policy can't be enforced — the caller MUST refuse to spawn (a review with
/// unfiltered MCP servers is the stall this prevents). `None` when the codex config is
/// present but unenumerable, OR a server name can't be safely emitted as a `-c` key.
/// For a `some` server, the per-tool denylist is computed from FRESH discovery only
/// (fingerprint match); a stale/missing cache fails that server closed (enabled=false).
pub fn spawn_overrides() -> Option<Vec<String>> {
    let names = codex_server_names()?;
    let mut out = Vec::new();
    for name in &names {
        let seg = key_segment(name)?; // can't safely emit ⇒ fail closed
        let mode = server_mode(name);
        let fresh = if matches!(mode, Mode::Some(_)) {
            server_spec(name).and_then(|spec| crate::tool_discovery::fresh_tools(&spec))
        } else {
            None
        };
        out.extend(overrides_for(&seg, &mode, fresh.as_deref()));
    }
    Some(out)
}

// ───────────────────────── v0.29 (O1): review model config ─────────────────────────
//
// A user-selected codex MODEL (+ optional context window) for AI Bridge's review
// child, persisted additively in review-mcp.json's `codex` object and injected at
// spawn as `-c model="<slug>"` / `-c model_context_window=<n>` (overriding the user's
// ~/.codex/config.toml ONLY for the review child). Unset → codex uses its own default.

/// The persisted review model config (additive `codex` object in review-mcp.json).
/// Both fields optional → unset means "use codex's config.toml default".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexReviewConfig {
    pub model: Option<String>,
    pub model_context_window: Option<u64>,
}

/// Upper bound for a context-window override. codex parses the `-c` value as a TOML
/// integer (i64); this sane cap (well beyond any real window) keeps us inside i64 and
/// rejects absurd/typo values that codex would refuse.
const MAX_CONTEXT_WINDOW: u64 = 20_000_000;

/// A codex model slug we'll emit as `-c model="<slug>"`. Restrict to safe chars so the
/// emitted `-c` arg can never be malformed / injected.
fn is_valid_model_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Pure: extract the `codex` object from a policy Value (empty fields when absent).
fn codex_from(cfg: &Value) -> CodexReviewConfig {
    let c = cfg.get("codex");
    CodexReviewConfig {
        model: c
            .and_then(|c| c.get("model"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        model_context_window: c
            .and_then(|c| c.get("model_context_window"))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0 && *n <= MAX_CONTEXT_WINDOW),
    }
}

/// The persisted review model config (default = unset).
pub fn codex_config() -> CodexReviewConfig {
    codex_from(&read_config())
}

/// Pure: set/clear the `codex` object additively (preserves `allow`/`server_tools`).
/// The model is PRIMARY: a context-window override only applies WITH a model, so a
/// `None` model is a full clear (ctx ignored) — never leaves a lingering window
/// override. An empty config removes the `codex` key entirely.
fn set_codex_in(cfg: &mut Value, model: Option<String>, ctx: Option<u64>) {
    if !cfg.is_object() {
        *cfg = json!({});
    }
    let obj = cfg.as_object_mut().expect("object");
    let mut codex = serde_json::Map::new();
    if let Some(m) = model {
        codex.insert("model".into(), json!(m));
        if let Some(n) = ctx {
            codex.insert("model_context_window".into(), json!(n));
        }
    }
    if codex.is_empty() {
        obj.remove("codex");
    } else {
        obj.insert("codex".into(), Value::Object(codex));
    }
}

/// Persist the review model config. `model` is PRIMARY; a context window applies only
/// with a model. `None` model fully clears the override (ctx ignored). Rejects an
/// invalid slug shape (so a bad id is never stored / emitted).
pub fn set_codex_model(model: Option<String>, ctx: Option<u64>) -> Result<(), String> {
    if let Some(m) = &model {
        if !is_valid_model_slug(m) {
            return Err(format!(
                "invalid model id '{m}' (allowed: letters, digits, '.', '_', '-')"
            ));
        }
    }
    if let Some(n) = ctx {
        if n == 0 || n > MAX_CONTEXT_WINDOW {
            return Err(format!(
                "invalid context window {n} (must be 1..={MAX_CONTEXT_WINDOW})"
            ));
        }
    }
    let mut cfg = read_config();
    set_codex_in(&mut cfg, model, ctx);
    write_config(&cfg).map_err(|e| format!("write review-mcp.json: {e}"))
}

/// Pure: the spawn `-c` overrides for model/context from a policy Value. Empty when
/// unset; OMITS an invalid slug (never emits a malformed `-c`). The model value is
/// TOML-serialized; ctx is a bare integer.
fn codex_overrides_from(cfg: &Value) -> Vec<String> {
    let c = codex_from(cfg);
    let mut out = Vec::new();
    // Model is PRIMARY: emit the context window ONLY alongside a valid model, so a
    // manually-edited / legacy config with a window but an empty/invalid model can't
    // leak a lone `-c model_context_window=…` (which would override codex's default
    // window for whatever model it falls back to).
    if let Some(m) = c.model.filter(|m| is_valid_model_slug(m)) {
        out.push("-c".to_string());
        out.push(format!("model={}", toml::Value::String(m)));
        if let Some(n) = c.model_context_window {
            out.push("-c".to_string());
            out.push(format!("model_context_window={n}"));
        }
    }
    out
}

/// Spawn-time model/context `-c` overrides for the warm review child (v0.29 O1).
/// Empty when unset → codex uses its config.toml default (no behavior change).
pub fn codex_spawn_overrides() -> Vec<String> {
    codex_overrides_from(&read_config())
}

/// Names in the allowlist that are not (any longer) defined codex servers.
fn stale_entries(names: &[String], allow: &[String]) -> Vec<String> {
    allow
        .iter()
        .filter(|a| !names.iter().any(|n| n == *a))
        .cloned()
        .collect()
}

/// Heuristic: does this server look like a browser/scrape tool that tends to elicit
/// or run long ops (and therefore stall a review)? Matches name AND command line.
pub fn looks_interactive(info: &ServerInfo) -> bool {
    const NEEDLES: &[&str] = &[
        "chrome",
        "playwright",
        "puppeteer",
        "firecrawl",
        "scrap",
        "devtools",
        "browser",
        "selenium",
    ];
    let hay = format!("{} {}", info.name, info.cmdline).to_lowercase();
    NEEDLES.iter().any(|n| hay.contains(n))
}

// ---------------------------------------------------------------------------
// CLI surface (`aibridge review-mcp …`) — returns user-facing strings.
// ---------------------------------------------------------------------------

/// `aibridge review-mcp list`.
pub fn list_report() -> String {
    let names = match codex_server_names() {
        Some(n) => n,
        None => {
            return "AI Bridge review-mcp: ⚠ ~/.codex/config.toml is present but couldn't be \
                    read/parsed — reviews will be REFUSED until you fix it (fail-closed: AI Bridge \
                    won't run a review with unfiltered codex MCP servers)."
                .to_string()
        }
    };
    let allow = allowlist();
    if names.is_empty() {
        return "AI Bridge review-mcp: no codex MCP servers found (reviews run tool-free)."
            .to_string();
    }
    let mut out = String::from(
        "AI Bridge review-mcp — codex MCP servers DURING AI Bridge reviews \
         (default: all OFF; reviews are pure reasoning):\n",
    );
    for name in &names {
        let on = allow.iter().any(|a| a == name);
        let mark = if on { "ON " } else { "off" };
        let warn = if on && server_looks_interactive(name) {
            "  ⚠ browser/scrape — can stall a review"
        } else {
            ""
        };
        out.push_str(&format!("  [{mark}] {name}{warn}\n"));
    }
    let stale = stale_entries(&names, &allow);
    if !stale.is_empty() {
        out.push_str(&format!(
            "  (stale allow entries, not in codex config — ignored: {})\n",
            stale.join(", ")
        ));
    }
    out.push_str(
        "Change with: aibridge review-mcp enable <name> | disable <name> | all | none. \
         Reload the window to apply to a running review.",
    );
    out
}

fn set_allow(cfg: &mut Value, allow: Vec<String>) {
    if let Some(o) = cfg.as_object_mut() {
        o.insert("allow".into(), json!(allow));
    }
}
fn clear_server_tools(cfg: &mut Value, name: &str) {
    if let Some(st) = cfg.get_mut("server_tools").and_then(|v| v.as_object_mut()) {
        st.remove(name);
    }
}

/// Set a server's whole-server review state (mode ALL when `on`, OFF when not),
/// preserving OTHER servers' per-tool entries and CLEARING this one's (all/off carry
/// no per-tool denylist). `Err` if the codex config can't be read, `name` isn't a
/// known server (when enabling), or the write fails. Used by the TUI + CLI.
pub fn set_enabled(name: &str, on: bool) -> Result<(), String> {
    let names = codex_server_names()
        .ok_or_else(|| "can't read/parse ~/.codex/config.toml — fix it first".to_string())?;
    if on && !names.iter().any(|n| n == name) {
        return Err(format!(
            "'{name}' is not a codex MCP server (known: {})",
            if names.is_empty() {
                "(none)".to_string()
            } else {
                names.join(", ")
            }
        ));
    }
    let mut cfg = read_config();
    let mut allow = allow_from(&cfg);
    let present = allow.iter().any(|a| a == name);
    if on && !present {
        allow.push(name.to_string());
    } else if !on && present {
        allow.retain(|a| a != name);
    }
    set_allow(&mut cfg, allow);
    clear_server_tools(&mut cfg, name); // ALL/OFF ⇒ no per-tool denylist
    write_config(&cfg).map_err(|e| format!("write failed: {e}"))
}

/// `aibridge review-mcp enable <name>` — whole-server, all tools.
pub fn enable(name: &str) -> Result<String, String> {
    set_enabled(name, true).map_err(|e| format!("AI Bridge review-mcp: {e}"))?;
    Ok(format!(
        "AI Bridge review-mcp: '{name}' will be ENABLED (all tools) during reviews. Reload to apply."
    ))
}

/// `aibridge review-mcp disable <name>`.
pub fn disable(name: &str) -> String {
    let was_on = allowlist().iter().any(|a| a == name);
    match set_enabled(name, false) {
        Ok(()) if !was_on => {
            format!("AI Bridge review-mcp: '{name}' was already off during reviews.")
        }
        Ok(()) => format!(
            "AI Bridge review-mcp: '{name}' will be DISABLED during reviews. Reload to apply."
        ),
        Err(e) => format!("AI Bridge review-mcp: {e}"),
    }
}

/// `aibridge review-mcp all` / `none`.
pub fn set_all(on: bool) -> String {
    let allow = if on {
        match codex_server_names() {
            Some(n) => n,
            None => {
                return "AI Bridge review-mcp: can't read/parse ~/.codex/config.toml — fix it first."
                    .to_string()
            }
        }
    } else {
        Vec::new()
    };
    let mut cfg = read_config();
    set_allow(&mut cfg, allow.clone());
    if let Some(o) = cfg.as_object_mut() {
        o.remove("server_tools"); // all/none ⇒ no per-tool entries
    }
    match write_config(&cfg) {
        Ok(()) if on => format!(
            "AI Bridge review-mcp: ALL {} codex server(s) enabled (all tools) during reviews \
             (⚠ browser/scrape ones can stall a review). Reload to apply.",
            allow.len()
        ),
        Ok(()) => "AI Bridge review-mcp: NO codex servers during reviews (pure reasoning). \
                   Reload to apply."
            .to_string(),
        Err(e) => format!("AI Bridge review-mcp: write failed: {e}"),
    }
}

/// `aibridge review-mcp tools <server>` — DISCOVER the server's tools (launches it for
/// `tools/list` only; explicit user action) and print each with its current state.
pub fn tools_report(server: &str) -> Result<String, String> {
    let spec = server_spec(server)
        .ok_or_else(|| format!("AI Bridge review-mcp: '{server}' is not a codex MCP server."))?;
    let discovered = crate::tool_discovery::discover_and_cache(&spec)
        .map_err(|e| format!("AI Bridge review-mcp: discovering '{server}' failed: {e}"))?;
    let mode = server_mode(server);
    let mut out = format!("AI Bridge review-mcp — tools of '{server}' during reviews:\n");
    if discovered.is_empty() {
        out.push_str("  (the server reported no tools)\n");
    }
    for t in &discovered {
        let on = match &mode {
            Mode::All => true,
            Mode::Off => false,
            Mode::Some(list) => list.iter().any(|x| x == t),
        };
        out.push_str(&format!("  [{}] {t}\n", if on { "x" } else { " " }));
    }
    let state = match mode {
        Mode::Off => {
            "server is OFF for reviews — `review-mcp enable` or a `tool ... on` turns it on"
        }
        Mode::All => "server is ON, ALL tools",
        Mode::Some(_) => "server is ON, SOME tools",
    };
    out.push_str(&format!(
        "Server state: {state}. Toggle: `aibridge review-mcp tool {server} <tool> on|off`. Reload to apply."
    ));
    Ok(out)
}

/// `aibridge review-mcp tool <server> <tool> on|off` — fresh-discover, validate the
/// tool, and update the server's per-tool allowlist (mode SOME, or ALL when every
/// discovered tool ends up on). Ensures the server is enabled.
pub fn set_tool(server: &str, tool: &str, on: bool) -> Result<String, String> {
    let spec =
        server_spec(server).ok_or_else(|| format!("'{server}' is not a codex MCP server"))?;
    let discovered = crate::tool_discovery::discover_and_cache(&spec)
        .map_err(|e| format!("discovering '{server}' failed: {e}"))?;
    if !discovered.iter().any(|t| t == tool) {
        return Err(format!(
            "'{tool}' is not a tool of '{server}' (found: {})",
            if discovered.is_empty() {
                "(none)".to_string()
            } else {
                discovered.join(", ")
            }
        ));
    }
    // Base enabled set: the current 'some' list (intersected with what's still
    // discovered), else ALL discovered (server was 'all' or 'off').
    let mut enabled: Vec<String> = match server_mode(server) {
        Mode::Some(list) => list
            .into_iter()
            .filter(|t| discovered.contains(t))
            .collect(),
        _ => discovered.clone(),
    };
    if on {
        if !enabled.iter().any(|t| t == tool) {
            enabled.push(tool.to_string());
        }
    } else {
        enabled.retain(|t| t != tool);
    }
    apply_tool_selection(server, &discovered, &enabled)
        .map_err(|e| format!("write failed: {e}"))?;
    let note = if enabled.is_empty() {
        " — NOTE: the server is enabled but ALL its discovered tools are now disabled (it \
         contributes no tools); use `review-mcp disable` to turn the server off entirely"
    } else {
        ""
    };
    Ok(format!(
        "AI Bridge review-mcp: '{server}' tool '{tool}' = {}{note}. Applies to the next review \
         child spawn (reload to start one).",
        if on { "on" } else { "off" }
    ))
}

/// Does `enabled` cover EVERY discovered tool? (Pure.) WARNING: with an EMPTY
/// `discovered` this is vacuously TRUE — so a caller writing an all-DISABLED ("none")
/// state must NOT route through `apply_tool_selection` (which collapses all-covered to
/// mode ALL); `set_all_tools(false)` writes mode SOME([]) explicitly instead.
fn covers_all(discovered: &[String], enabled: &[String]) -> bool {
    discovered.iter().all(|t| enabled.iter().any(|e| e == t))
}

/// Persist a per-tool selection: ensure the server is in `allow`; if EVERY discovered
/// tool is enabled → mode ALL (drop the per-tool entry); else mode SOME with the list
/// (only discovered names — never junk).
fn apply_tool_selection(
    server: &str,
    discovered: &[String],
    enabled: &[String],
) -> std::io::Result<()> {
    let mut cfg = read_config();
    let mut allow = allow_from(&cfg);
    if !allow.iter().any(|a| a == server) {
        allow.push(server.to_string());
    }
    set_allow(&mut cfg, allow);
    let all_on = covers_all(discovered, enabled);
    if all_on {
        clear_server_tools(&mut cfg, server);
    } else {
        let list: Vec<String> = discovered
            .iter()
            .filter(|t| enabled.iter().any(|e| e == *t))
            .cloned()
            .collect();
        if cfg
            .get("server_tools")
            .and_then(|v| v.as_object())
            .is_none()
        {
            if let Some(o) = cfg.as_object_mut() {
                o.insert("server_tools".into(), json!({}));
            }
        }
        if let Some(st) = cfg.get_mut("server_tools").and_then(|v| v.as_object_mut()) {
            st.insert(server.to_string(), json!({ "enabled_tools": list }));
        }
    }
    write_config(&cfg)
}

/// A server's cached tools for the TUI: (tool, enabled-in-review) pairs from the CACHE
/// (no launch) + the current mode, plus whether the cache is FRESH (fingerprint still
/// matches the config). `None` ⇒ the server has never been discovered.
pub struct CachedTools {
    pub tools: Vec<(String, bool)>,
    pub fresh: bool,
}

/// Tool states for `server` from the cache (no relaunch) — for the TUI's tool view.
pub fn cached_tool_states(server: &str) -> Option<CachedTools> {
    let spec = server_spec(server)?;
    let entry = crate::tool_discovery::cached(server)?;
    let fresh = entry.fingerprint == spec.fingerprint();
    let mode = server_mode(server);
    let tools = entry
        .tools
        .into_iter()
        .map(|t| {
            let on = match &mode {
                Mode::All => true,
                Mode::Off => false,
                Mode::Some(list) => list.iter().any(|x| x == &t),
            };
            (t, on)
        })
        .collect();
    Some(CachedTools { tools, fresh })
}

/// The user-facing error string for the NonStdio branch of `discover_server`. Pure +
/// unit-tested so a future tweak (wording / pluralization) can't break the contract
/// the TUI relies on (names the server, names the transport, points to the workaround).
pub(crate) fn nonstdio_discovery_error(server: &str, transport: &str) -> String {
    format!(
        "'{server}' uses transport '{transport}' — tool discovery requires stdio; \
         the server-level toggle on the previous view still applies during reviews"
    )
}

/// DISCOVER (launch) a server's tools + cache them — for the TUI's explicit 'd' key.
/// Slow/networked; the TUI runs it on a background thread. Returns a transport-aware
/// error when the server is configured but uses a non-stdio transport (HTTP/SSE) so
/// the user sees WHY discovery isn't available rather than the misleading v1 "is not
/// a codex MCP server" (Codex finding — context7 on macOS).
pub fn discover_server(server: &str) -> Result<Vec<String>, String> {
    match server_lookup(server) {
        ServerLookup::Stdio(spec) => crate::tool_discovery::discover_and_cache(&spec)
            .map_err(|e| format!("discovery failed: {e}")),
        ServerLookup::NonStdio { transport } => Err(nonstdio_discovery_error(server, &transport)),
        ServerLookup::Unknown => Err(format!("'{server}' is not a codex MCP server")),
        ServerLookup::Unavailable(why) => Err(format!("codex inventory unavailable: {why}")),
    }
}

/// Toggle ONE tool using the FRESH cache (NO relaunch — for interactive toggling).
/// `Err` if the cache isn't fresh (the caller should discover first). Turning a tool
/// OFF that's already off is a no-op (won't accidentally enable an OFF server).
pub fn toggle_tool_cached(server: &str, tool: &str, on: bool) -> Result<String, String> {
    let spec =
        server_spec(server).ok_or_else(|| format!("'{server}' is not a codex MCP server"))?;
    let discovered = crate::tool_discovery::fresh_tools(&spec).ok_or_else(|| {
        "tools aren't freshly discovered — press 'd' to (re)discover first".to_string()
    })?;
    if !discovered.iter().any(|t| t == tool) {
        return Err(format!("'{tool}' is not a discovered tool of '{server}'"));
    }
    let current: Vec<String> = match server_mode(server) {
        Mode::Some(list) => list
            .into_iter()
            .filter(|t| discovered.contains(t))
            .collect(),
        Mode::All => discovered.clone(),
        Mode::Off => Vec::new(), // server off ⇒ no tools currently on
    };
    if !on && !current.iter().any(|t| t == tool) {
        return Ok(format!("'{tool}' is already off")); // no-op; don't enable an off server
    }
    let mut enabled = current;
    if on {
        if !enabled.iter().any(|t| t == tool) {
            enabled.push(tool.to_string());
        }
    } else {
        enabled.retain(|t| t != tool);
    }
    apply_tool_selection(server, &discovered, &enabled)
        .map_err(|e| format!("write failed: {e}"))?;
    Ok(format!(
        "'{server}' tool '{tool}' = {}",
        if on { "on" } else { "off" }
    ))
}

/// Tools-view "select ALL / NONE". EXPLICIT modes — does NOT route through
/// `apply_tool_selection`'s all-on collapse (which would turn "none" into mode ALL when
/// discovery returns an EMPTY tool set, silently enabling future tools — Codex find):
/// `on` → mode ALL (every tool incl. future); `!on` → mode SOME([]) (server enabled,
/// all CURRENT tools disabled), forced even if the tool set is empty. "none" needs a
/// fresh discovery so enforcement won't silently fail-closed and so it's deliberate.
pub fn set_all_tools(server: &str, on: bool) -> Result<String, String> {
    let count = if on {
        None
    } else {
        let spec =
            server_spec(server).ok_or_else(|| format!("'{server}' is not a codex MCP server"))?;
        Some(
            crate::tool_discovery::fresh_tools(&spec)
                .ok_or_else(|| {
                    "tools aren't freshly discovered — press 'd' to (re)discover first".to_string()
                })?
                .len(),
        )
    };
    let mut cfg = read_config();
    let mut allow = allow_from(&cfg);
    if !allow.iter().any(|a| a == server) {
        allow.push(server.to_string());
    }
    set_allow(&mut cfg, allow);
    if on {
        clear_server_tools(&mut cfg, server); // mode ALL
    } else {
        // Force mode SOME([]) explicitly — never collapse to ALL on an empty tool set.
        if cfg
            .get("server_tools")
            .and_then(|v| v.as_object())
            .is_none()
        {
            if let Some(o) = cfg.as_object_mut() {
                o.insert("server_tools".into(), json!({}));
            }
        }
        if let Some(st) = cfg.get_mut("server_tools").and_then(|v| v.as_object_mut()) {
            st.insert(server.to_string(), json!({ "enabled_tools": [] }));
        }
    }
    write_config(&cfg).map_err(|e| format!("write failed: {e}"))?;
    Ok(match count {
        None => format!("'{server}': ALL tools enabled for reviews"),
        Some(n) => format!("'{server}': all {n} tool(s) disabled for reviews"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── v0.29 (O1) review model config (pure; never touches the real review-mcp.json) ───
    #[test]
    fn codex_from_reads_fields_and_treats_empty_as_unset() {
        let cfg = json!({"codex": {"model": "gpt-5.5", "model_context_window": 272000}});
        let c = codex_from(&cfg);
        assert_eq!(c.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(c.model_context_window, Some(272_000));
        // empty model + zero ctx → unset
        let c2 = codex_from(&json!({"codex": {"model": "", "model_context_window": 0}}));
        assert!(c2.model.is_none() && c2.model_context_window.is_none());
        // no codex object → default
        assert_eq!(codex_from(&json!({})), CodexReviewConfig::default());
    }

    #[test]
    fn set_codex_in_is_additive_and_clears() {
        let mut cfg = json!({"allow": ["context7"]});
        set_codex_in(&mut cfg, Some("gpt-5.5".into()), Some(272_000));
        assert_eq!(cfg["allow"], json!(["context7"]), "allow preserved");
        assert_eq!(cfg["codex"]["model"], "gpt-5.5");
        assert_eq!(cfg["codex"]["model_context_window"], 272_000);
        // clearing removes the codex key but keeps allow
        set_codex_in(&mut cfg, None, None);
        assert!(cfg.get("codex").is_none(), "empty codex removed");
        assert_eq!(cfg["allow"], json!(["context7"]));
    }

    #[test]
    fn set_codex_in_none_model_fully_clears_even_with_ctx() {
        // a context window without a model must NOT linger — None model = full clear
        let mut cfg = json!({"codex": {"model": "gpt-5.5", "model_context_window": 272000}});
        set_codex_in(&mut cfg, None, Some(500_000));
        assert!(
            cfg.get("codex").is_none(),
            "ctx ignored when model is None → cleared"
        );
        assert!(
            codex_overrides_from(&cfg).is_empty(),
            "no lingering context-window override"
        );
    }

    #[test]
    fn codex_overrides_from_emits_valid_c_args() {
        assert_eq!(
            codex_overrides_from(
                &json!({"codex": {"model": "gpt-5.5", "model_context_window": 272000}})
            ),
            vec![
                "-c",
                "model=\"gpt-5.5\"",
                "-c",
                "model_context_window=272000"
            ]
        );
        assert_eq!(
            codex_overrides_from(&json!({"codex": {"model": "gpt-5.4"}})),
            vec!["-c", "model=\"gpt-5.4\""]
        );
        assert!(
            codex_overrides_from(&json!({})).is_empty(),
            "unset → no overrides"
        );
    }

    #[test]
    fn codex_overrides_omit_invalid_slug() {
        // a slug with spaces / shell metacharacters must NOT be emitted as a -c arg
        let cfg = json!({"codex": {"model": "gpt 5.5; rm -rf /"}});
        assert!(
            codex_overrides_from(&cfg).is_empty(),
            "invalid slug omitted"
        );
    }

    #[test]
    fn codex_overrides_never_leak_ctx_without_valid_model() {
        // manually-edited / legacy JSON: a context window but empty/invalid model must
        // NOT emit a lone `-c model_context_window=…` (spawn path reads arbitrary JSON).
        assert!(
            codex_overrides_from(&json!({"codex": {"model": "", "model_context_window": 272000}}))
                .is_empty(),
            "empty model + ctx → no overrides"
        );
        assert!(
            codex_overrides_from(
                &json!({"codex": {"model": "bad slug!", "model_context_window": 272000}})
            )
            .is_empty(),
            "invalid model + ctx → no overrides"
        );
    }

    #[test]
    fn set_codex_model_rejects_invalid_slug_before_any_write() {
        // validation happens BEFORE read/write_config, so this never touches the real file
        assert!(set_codex_model(Some("bad slug!".into()), None).is_err());
    }

    #[test]
    fn codex_from_drops_absurd_context_window() {
        // a window beyond the sane cap (or 0) is treated as unset → no leaked override
        let huge = json!({"codex": {"model": "gpt-5.5", "model_context_window": 9_999_999_999u64}});
        assert_eq!(
            codex_from(&huge).model_context_window,
            None,
            "absurd ctx dropped"
        );
        // the model still applies; just no context override
        assert_eq!(codex_overrides_from(&huge), vec!["-c", "model=\"gpt-5.5\""]);
    }

    #[test]
    fn set_codex_model_rejects_absurd_context_window_before_any_write() {
        assert!(set_codex_model(Some("gpt-5.5".into()), Some(0)).is_err());
        assert!(set_codex_model(Some("gpt-5.5".into()), Some(u64::MAX)).is_err());
    }

    #[test]
    fn parses_servers_with_command_and_args() {
        let cfg = r#"
model = "gpt-5.5"
[mcp_servers.context7]
command = "npx"
args = ["-y", "@upstash/context7-mcp"]
enabled = true
[mcp_servers.chrome-devtools]
command = "npx"
args = ["-y", "chrome-devtools-mcp@latest"]
enabled = false
"#;
        let servers = servers_from_toml(cfg);
        let names: Vec<_> = servers.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"context7") && names.contains(&"chrome-devtools"));
        let chrome = servers
            .iter()
            .find(|s| s.name == "chrome-devtools")
            .unwrap();
        assert!(chrome.cmdline.contains("chrome-devtools-mcp"));
    }

    #[test]
    fn covers_all_vacuous_on_empty_discovered() {
        let s = |x: &str| x.to_string();
        // THE edge: empty discovered ⇒ vacuously "all covered" ⇒ apply_tool_selection
        // would collapse to mode ALL. set_all_tools(false) must AVOID this path.
        assert!(covers_all(&[], &[]));
        // a real tool, none enabled ⇒ NOT all (so a real "none" is a genuine SOME([])).
        assert!(!covers_all(&[s("a")], &[]));
        // subset / superset behavior.
        assert!(covers_all(&[s("a")], &[s("a"), s("b")]));
        assert!(!covers_all(&[s("a"), s("b")], &[s("a")]));
    }

    /// Helper: synthesise the `ServerLookup::Stdio` / `NonStdio` branch from a parsed
    /// `CodexServer`. Mirrors the `Inventory::Available` arm of `server_lookup` but
    /// without touching the real codex CLI (so the unit test is hermetic).
    fn lookup_from_codex_server(s: CodexServer) -> ServerLookup {
        if !s.command.is_empty() {
            ServerLookup::Stdio(crate::tool_discovery::ServerSpec {
                name: s.name,
                command: s.command,
                args: s.args,
                env: s.env,
                cwd: s.cwd,
            })
        } else {
            ServerLookup::NonStdio {
                transport: if s.transport.is_empty() {
                    "non-stdio".to_string()
                } else {
                    s.transport
                },
            }
        }
    }

    #[test]
    fn server_lookup_distinguishes_http() {
        // A `streamable_http` server in codex's inventory has an empty `command`. The
        // lookup must surface that as `NonStdio { transport: "streamable_http" }`
        // (NOT the v1 misleading "is not a codex MCP server").
        let http_v = json!({
            "name": "context7",
            "enabled": true,
            "transport": {"type": "streamable_http", "url": "https://mcp.context7.com/mcp"}
        });
        let parsed = parse_codex_server(&http_v).expect("parses");
        match lookup_from_codex_server(parsed) {
            ServerLookup::NonStdio { transport } => assert_eq!(transport, "streamable_http"),
            other => panic!("expected NonStdio, got {other:?}"),
        }
    }

    #[test]
    fn server_lookup_stdio_returns_spec() {
        let stdio_v = json!({
            "name": "fs",
            "transport": {"type": "stdio", "command": "npx", "args": ["@anthropic/mcp-fs"]}
        });
        let parsed = parse_codex_server(&stdio_v).expect("parses");
        match lookup_from_codex_server(parsed) {
            ServerLookup::Stdio(spec) => {
                assert_eq!(spec.name, "fs");
                assert_eq!(spec.command, "npx");
                assert_eq!(spec.args, vec!["@anthropic/mcp-fs"]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn server_lookup_unknown_transport_falls_back_to_generic_label() {
        // A transport-less / unparsed server reaches the NonStdio branch with the
        // synthesized "non-stdio" label rather than an empty string.
        let weird = json!({"name": "weird", "transport": null});
        let parsed = parse_codex_server(&weird).expect("parses");
        match lookup_from_codex_server(parsed) {
            ServerLookup::NonStdio { transport } => assert_eq!(transport, "non-stdio"),
            other => panic!("expected NonStdio, got {other:?}"),
        }
    }

    #[test]
    fn discover_server_http_error_message_mentions_transport() {
        // Calls the SAME formatter the production path uses — so a future wording
        // change is caught here rather than at runtime. Regression for the misleading
        // v1 "is not a codex MCP server" on HTTP-transport servers.
        let err = nonstdio_discovery_error("context7", "streamable_http");
        assert!(err.contains("'context7'"), "names the server");
        assert!(err.contains("streamable_http"), "names the transport");
        assert!(
            err.contains("server-level toggle"),
            "guides the user to the workaround"
        );
        assert!(
            !err.contains("is not a codex MCP server"),
            "must NOT regress to the v1 misleading wording"
        );
    }

    #[test]
    fn parse_codex_server_handles_stdio_http_and_env() {
        // stdio with env=null + env_vars (inherited, NOT stored) + cwd.
        let stdio = json!({
            "name": "next-devtools", "enabled": true,
            "transport": {"type":"stdio","command":"npx","args":["-y","next-devtools-mcp"],
                          "env":null,"env_vars":["FOO_TOKEN"],"cwd":"/proj"}
        });
        let s = parse_codex_server(&stdio).unwrap();
        assert_eq!(s.name, "next-devtools");
        assert!(s.enabled);
        assert_eq!(s.command, "npx");
        assert_eq!(s.args, vec!["-y", "next-devtools-mcp"]);
        assert!(s.env.is_empty()); // env=null → empty (env_vars are inherited, not stored)
        assert_eq!(s.cwd.as_deref(), Some("/proj"));
        // stdio with an explicit env map.
        let withenv =
            json!({"name":"x","transport":{"type":"stdio","command":"c","env":{"K":"V"}}});
        assert_eq!(
            parse_codex_server(&withenv).unwrap().env,
            vec![("K".to_string(), "V".to_string())]
        );
        // HTTP / no command → command empty (server_spec treats it as non-discoverable).
        let http = json!({"name":"h","enabled":true,"transport":{"type":"streamable_http","url":"https://x"}});
        let h = parse_codex_server(&http).unwrap();
        assert_eq!(h.transport, "streamable_http");
        assert!(h.command.is_empty());
        // missing name → None.
        assert!(parse_codex_server(&json!({"transport":{"type":"stdio"}})).is_none());
    }

    #[test]
    fn no_mcp_servers_table_is_empty() {
        assert!(servers_from_toml("model = \"x\"").is_empty());
        assert!(servers_from_toml("not valid toml = =").is_empty());
    }

    #[test]
    fn key_segment_bare_quoted_and_unsafe() {
        assert_eq!(
            key_segment("chrome-devtools").as_deref(),
            Some("chrome-devtools")
        );
        assert_eq!(key_segment("seq_thinking").as_deref(), Some("seq_thinking"));
        // a dotted/spaced name is emitted QUOTED so the dotted path stays unambiguous
        assert_eq!(key_segment("weird.name").as_deref(), Some("\"weird.name\""));
        assert_eq!(key_segment("has space").as_deref(), Some("\"has space\""));
        // a name with a quote/backslash/control char can't be safely emitted
        assert_eq!(key_segment("bad\"name"), None);
        assert_eq!(key_segment("back\\slash"), None);
        assert_eq!(key_segment(""), None);
    }

    #[test]
    fn overrides_off_and_all() {
        assert_eq!(
            overrides_for("ctx", &Mode::Off, None),
            vec!["-c", "mcp_servers.ctx.enabled=false"]
        );
        assert_eq!(
            overrides_for("ctx", &Mode::All, None),
            vec!["-c", "mcp_servers.ctx.enabled=true"]
        );
    }

    #[test]
    fn overrides_some_fresh_computes_denylist() {
        // enable only `a` of discovered {a,b,c} → disabledTools = [b,c].
        let mode = Mode::Some(vec!["a".to_string()]);
        let discovered = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let args = overrides_for("fc", &mode, Some(&discovered));
        assert_eq!(
            args,
            vec![
                "-c".to_string(),
                "mcp_servers.fc.enabled=true".to_string(),
                "-c".to_string(),
                "mcp_servers.fc.disabledTools=[\"b\",\"c\"]".to_string(),
            ]
        );
    }

    #[test]
    fn overrides_some_all_tools_on_emits_no_denylist() {
        // every discovered tool enabled ⇒ enabled=true, no disabledTools.
        let mode = Mode::Some(vec!["a".to_string(), "b".to_string()]);
        let discovered = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            overrides_for("x", &mode, Some(&discovered)),
            vec!["-c", "mcp_servers.x.enabled=true"]
        );
    }

    #[test]
    fn toml_array_escapes_via_serializer() {
        assert_eq!(
            toml_array(&["a".to_string(), "b".to_string()]),
            "[\"a\",\"b\"]"
        );
        assert_eq!(toml_array(&[]), "[]");
        // A name with a quote/backslash is escaped to VALID TOML basic-string escapes
        // (defensive — we only ever pass discovered names, which are identifiers).
        assert_eq!(toml_array(&["a\"b".to_string()]), "[\"a\\\"b\"]");
        assert_eq!(toml_array(&["a\\b".to_string()]), "[\"a\\\\b\"]");
    }

    #[test]
    fn overrides_some_stale_fails_closed() {
        // No fresh discovery for a `some` server ⇒ enabled=false (never a partial
        // denylist from stale data, which could silently re-enable a new tool).
        let mode = Mode::Some(vec!["a".to_string()]);
        assert_eq!(
            overrides_for("x", &mode, None),
            vec!["-c", "mcp_servers.x.enabled=false"]
        );
    }

    #[test]
    fn key_segment_unsafe_is_none() {
        // (spawn_overrides turns this None into a fail-closed refusal.)
        assert_eq!(key_segment("bad\"name"), None);
    }

    #[test]
    fn stale_entries_are_detected() {
        let names = vec!["a".to_string(), "b".to_string()];
        let allow = vec!["a".to_string(), "gone".to_string()];
        assert_eq!(stale_entries(&names, &allow), vec!["gone".to_string()]);
    }

    #[test]
    fn interactive_heuristic_matches_browser_scrape() {
        let chrome = ServerInfo {
            name: "chrome-devtools".to_string(),
            cmdline: "npx chrome-devtools-mcp".to_string(),
        };
        let fire = ServerInfo {
            name: "fc".to_string(),
            cmdline: "npx firecrawl-mcp".to_string(),
        };
        let ctx = ServerInfo {
            name: "context7".to_string(),
            cmdline: "npx @upstash/context7-mcp".to_string(),
        };
        assert!(looks_interactive(&chrome));
        assert!(looks_interactive(&fire)); // matched via cmdline
        assert!(!looks_interactive(&ctx));
    }

    #[test]
    fn names_from_config_never_fails_open() {
        // Valid config → strict TOML keys.
        let ok =
            "[mcp_servers.context7]\ncommand=\"npx\"\n[mcp_servers.firecrawl]\ncommand=\"npx\"\n";
        assert_eq!(
            names_from_config(ok),
            Some(vec!["context7".to_string(), "firecrawl".to_string()])
        );
        // Valid TOML, no servers → Some([]) (genuinely none — safe).
        assert_eq!(names_from_config("model = \"x\""), Some(vec![]));
        // MALFORMED TOML but real [mcp_servers.X] headers → the header fallback STILL
        // enumerates them, so enforcement disables them (NOT fail-open). This is the
        // regression the blocker was about.
        let broken = "this = = not valid toml\n[mcp_servers.firecrawl]\ncommand = \"npx\"\n[mcp_servers.chrome-devtools]\n";
        let names = names_from_config(broken).expect("headers recovered despite bad toml");
        assert!(names.contains(&"firecrawl".to_string()));
        assert!(names.contains(&"chrome-devtools".to_string()));
        // A recovered server with default (Off) mode → enabled=false (all disabled).
        assert_eq!(
            overrides_for("firecrawl", &Mode::Off, None),
            vec!["-c", "mcp_servers.firecrawl.enabled=false"]
        );
        // Unparseable AND no recoverable headers → None ⇒ caller FAILS CLOSED.
        assert_eq!(names_from_config("@@@ totally broken @@@ = ="), None);
        // A header with a quoted name is unquoted by the scan.
        assert_eq!(
            names_from_config("nope = =\n[mcp_servers.\"odd-name\"]\n"),
            Some(vec!["odd-name".to_string()])
        );
    }
}
