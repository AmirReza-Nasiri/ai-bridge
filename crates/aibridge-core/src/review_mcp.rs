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
//! accidental tool invocation during reviews. It enumerates servers from
//! `~/.codex/config.toml`, so a server defined only in a project-local
//! `.codex/config.toml` would not be covered (a documented gap; `doctor` says so).

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

/// The launch spec (command/args/env) for `name` from `~/.codex/config.toml`, for
/// tool discovery + the cache fingerprint. `None` if not a defined server / no config.
pub fn server_spec(name: &str) -> Option<crate::tool_discovery::ServerSpec> {
    let s = codex_config_path().and_then(|p| std::fs::read_to_string(p).ok())?;
    let parsed: toml::Value = toml::from_str(&s).ok()?;
    let def = parsed.get("mcp_servers")?.as_table()?.get(name)?;
    let command = def.get("command")?.as_str()?.to_string();
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
    Some(crate::tool_discovery::ServerSpec {
        name: name.to_string(),
        command,
        args,
        env,
    })
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
    let all_on = discovered.iter().all(|t| enabled.iter().any(|e| e == t));
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

/// DISCOVER (launch) a server's tools + cache them — for the TUI's explicit 'd' key.
/// Slow/networked; the TUI runs it on a background thread.
pub fn discover_server(server: &str) -> Result<Vec<String>, String> {
    let spec =
        server_spec(server).ok_or_else(|| format!("'{server}' is not a codex MCP server"))?;
    crate::tool_discovery::discover_and_cache(&spec).map_err(|e| format!("discovery failed: {e}"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
