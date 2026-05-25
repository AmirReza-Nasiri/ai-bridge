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

/// The review allowlist: server names kept ENABLED during AI Bridge reviews. Default
/// empty (no file / unreadable / malformed) → reviews run with NO codex MCP servers.
pub fn allowlist() -> Vec<String> {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("allow").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn write_allowlist(allow: &[String]) -> std::io::Result<()> {
    let path = config_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_string_pretty(&json!({ "allow": allow }))
        .unwrap_or_else(|_| "{}".to_string());
    std::fs::write(path, body)
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

/// Build the `-c mcp_servers.<name>.enabled=<bool>` args for the review child: every
/// codex server set to enabled=(name in `allow`). Pure (names + allow in) so it is
/// unit-testable. Returns the unsupported names too, for a doctor warning.
fn build_overrides(names: &[String], allow: &[String]) -> (Vec<String>, Vec<String>) {
    let mut args = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        let Some(seg) = key_segment(name) else {
            skipped.push(name.clone());
            continue;
        };
        let on = allow.iter().any(|a| a == name);
        args.push("-c".to_string());
        args.push(format!("mcp_servers.{seg}.enabled={on}"));
    }
    (args, skipped)
}

/// Args to append to the warm review child spawn (AFTER `mcp-server`), or `None` when
/// the policy can't be enforced — the caller MUST refuse to spawn (a review with
/// unfiltered MCP servers is the stall this prevents). `None` when: the codex config is
/// present but unenumerable (`codex_server_names`), OR a discovered server name can't be
/// safely emitted as a `-c` key (so it couldn't be disabled → would be inherited). Reads
/// the current servers + allowlist each spawn, so a changed allowlist applies on the
/// next (re)spawn — reload to apply immediately.
pub fn spawn_overrides() -> Option<Vec<String>> {
    let names = codex_server_names()?;
    let (args, skipped) = build_overrides(&names, &allowlist());
    if !skipped.is_empty() {
        return None; // a discovered server can't be safely disabled via -c → fail closed
    }
    Some(args)
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

/// `aibridge review-mcp enable <name>` — validates the name is a known codex server.
pub fn enable(name: &str) -> Result<String, String> {
    let names = codex_server_names().ok_or_else(|| {
        "AI Bridge review-mcp: can't read/parse ~/.codex/config.toml — fix it first.".to_string()
    })?;
    if !names.iter().any(|n| n == name) {
        return Err(format!(
            "AI Bridge review-mcp: '{name}' is not a codex MCP server in ~/.codex/config.toml. \
             Known: {}.",
            if names.is_empty() {
                "(none)".to_string()
            } else {
                names.join(", ")
            }
        ));
    }
    let mut allow = allowlist();
    if !allow.iter().any(|a| a == name) {
        allow.push(name.to_string());
        write_allowlist(&allow).map_err(|e| format!("AI Bridge review-mcp: write failed: {e}"))?;
    }
    Ok(format!(
        "AI Bridge review-mcp: '{name}' will be ENABLED during reviews. Reload the window to apply."
    ))
}

/// `aibridge review-mcp disable <name>`.
pub fn disable(name: &str) -> String {
    let mut allow = allowlist();
    let before = allow.len();
    allow.retain(|a| a != name);
    if allow.len() == before {
        return format!("AI Bridge review-mcp: '{name}' was already off during reviews.");
    }
    match write_allowlist(&allow) {
        Ok(()) => format!(
            "AI Bridge review-mcp: '{name}' will be DISABLED during reviews. Reload the window to apply."
        ),
        Err(e) => format!("AI Bridge review-mcp: write failed: {e}"),
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
    match write_allowlist(&allow) {
        Ok(()) if on => format!(
            "AI Bridge review-mcp: ALL {} codex server(s) will be enabled during reviews \
             (⚠ browser/scrape ones can stall a review). Reload to apply.",
            allow.len()
        ),
        Ok(()) => "AI Bridge review-mcp: NO codex servers during reviews (pure reasoning). \
                   Reload to apply."
            .to_string(),
        Err(e) => format!("AI Bridge review-mcp: write failed: {e}"),
    }
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
    fn build_overrides_sets_each_server_by_allowlist() {
        let names = vec![
            "context7".to_string(),
            "firecrawl".to_string(),
            "chrome-devtools".to_string(),
        ];
        let allow = vec!["context7".to_string()];
        let (args, skipped) = build_overrides(&names, &allow);
        assert!(skipped.is_empty());
        // context7 ON, the other two OFF — order follows `names`.
        assert_eq!(
            args,
            vec![
                "-c".to_string(),
                "mcp_servers.context7.enabled=true".to_string(),
                "-c".to_string(),
                "mcp_servers.firecrawl.enabled=false".to_string(),
                "-c".to_string(),
                "mcp_servers.chrome-devtools.enabled=false".to_string(),
            ]
        );
    }

    #[test]
    fn build_overrides_default_none_disables_all() {
        let names = vec!["a".to_string(), "b".to_string()];
        let (args, _) = build_overrides(&names, &[]);
        assert_eq!(
            args,
            vec![
                "-c",
                "mcp_servers.a.enabled=false",
                "-c",
                "mcp_servers.b.enabled=false",
            ]
        );
    }

    #[test]
    fn build_overrides_skips_unsafe_names() {
        let names = vec!["ok".to_string(), "bad\"name".to_string()];
        let (args, skipped) = build_overrides(&names, &[]);
        assert_eq!(args, vec!["-c", "mcp_servers.ok.enabled=false"]);
        assert_eq!(skipped, vec!["bad\"name".to_string()]);
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
        // enumerates them, so build_overrides disables them (NOT fail-open). This is
        // the regression the blocker was about.
        let broken = "this = = not valid toml\n[mcp_servers.firecrawl]\ncommand = \"npx\"\n[mcp_servers.chrome-devtools]\n";
        let names = names_from_config(broken).expect("headers recovered despite bad toml");
        assert!(names.contains(&"firecrawl".to_string()));
        assert!(names.contains(&"chrome-devtools".to_string()));
        // Build the overrides from the recovered names with default-none → all disabled.
        let (args, _) = build_overrides(&names, &[]);
        assert!(args.contains(&"mcp_servers.firecrawl.enabled=false".to_string()));
        // Unparseable AND no recoverable headers → None ⇒ caller FAILS CLOSED.
        assert_eq!(names_from_config("@@@ totally broken @@@ = ="), None);
        // A header with a quoted name is unquoted by the scan.
        assert_eq!(
            names_from_config("nope = =\n[mcp_servers.\"odd-name\"]\n"),
            Some(vec!["odd-name".to_string()])
        );
    }
}
