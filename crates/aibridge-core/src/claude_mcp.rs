//! `claude_mcp` — read-only inspector for Claude Code's MCP servers (the host of
//! AI Bridge's own MCP server). Pairs with [`crate::review_mcp`], but Claude is the
//! HOST not the child: there is no per-invocation `mcp_servers.<n>.enabled` override
//! to inject (the way `review_mcp` does for Codex). So v1 is INSPECT-ONLY — the TUI
//! shows servers + their tools and the user edits `~/.claude.json` themselves (with
//! a Claude restart) to enable/disable. Management (atomic config edits + restart
//! nudge UX) is a deliberate v2.
//!
//! Sources read, in precedence ORDER (project wins on a name collision):
//! 1. **User scope**: `~/.claude.json` — `mcpServers` table (where `aibridge init`
//!    registers AI Bridge via `claude mcp add -s user`).
//! 2. **Project scope**: the CLOSEST-ANCESTOR `.mcp.json` walking up from the TUI's
//!    `cwd` — matches Claude's own "subdirectory-of-a-repo" resolution. NOT
//!    git-root: a monorepo can host multiple Claude projects.
//!
//! When the same server name exists in both, the project entry WINS (more specific
//! override) and the row is flagged `overrides_user = true` so the Inspector can
//! show it. Discovery is launched against the FULL `ClaudeServer` value (carrying
//! its scope + spec) so the post-override entry is the one actually run.
//!
//! Stdio launch cwd anchoring (Codex F1 finding): for a project-scope server with
//! no explicit `cwd`, the launch cwd is the `.mcp.json` parent directory (where
//! relative `command`/`args`/`cwd` resolve from). For user-scope, the TUI's cwd is
//! used (matches Claude's behavior of running user-scope MCPs from wherever Claude
//! itself was started).
//!
//! ISOLATION: discovery uses the existing UNCACHED entry point
//! `tool_discovery::discover` so it never writes the Codex-only cache file at
//! `~/.ai-bridge/mcp-tools-cache.json`. The structural test
//! `source_does_not_call_cache_writer` verifies the module never references
//! `discover_and_cache` / `store`.

use serde_json::Value;
use std::path::{Path, PathBuf};

fn home() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)
}

/// Where a particular `ClaudeServer` came from. The path is carried so
/// [`effective_cwd`] can anchor relative paths to the source file's parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudeScope {
    /// `~/.claude.json` (the user-scope registration `claude mcp add -s user` writes).
    User { source: PathBuf },
    /// `<closest-ancestor>/.mcp.json` (committed project scope).
    Project { source: PathBuf },
}

/// Typed MCP transport (Codex F-round finding: the v1 codex side erased transport
/// type and produced a misleading error; we keep it typed here from the start).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Stdio with a launchable command. `cwd_field` is the value of the config's
    /// `cwd` field VERBATIM (resolved against the source-file parent at launch time);
    /// `None` ⇒ no explicit `cwd` configured.
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        cwd_field: Option<String>,
    },
    /// Streamable HTTP / SSE — discovery via stdio launch is unsupported. The label
    /// is `"http"` or `"sse"` (or whatever the config's `type` says).
    Http { url: String, label: String },
    /// Type wasn't recognized — surface the label so the Inspector can show it.
    Unknown { label: String },
}

impl Transport {
    /// Short human label used in the Inspector ("stdio", "http", "sse", "?").
    pub fn label(&self) -> &str {
        match self {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { label, .. } => label,
            Transport::Unknown { label } => label,
        }
    }
}

/// One Claude MCP server (post-precedence). `overrides_user = true` means a same-named
/// user-scope entry was shadowed by this project-scope one.
#[derive(Clone, Debug)]
pub struct ClaudeServer {
    pub name: String,
    pub scope: ClaudeScope,
    pub transport: Transport,
    pub overrides_user: bool,
}

/// Result of asking for the merged Claude MCP inventory. `warnings` carries
/// non-fatal issues — most importantly, a corrupt `~/.claude.json` even when the
/// project `.mcp.json` parsed cleanly. The Inspector renders warnings ABOVE the
/// server list so the user notices that a source was unreachable rather than
/// concluding "no MCPs configured" (Codex Stop-gate F2 finding).
#[derive(Debug)]
pub enum Inventory {
    /// One row per server (post-precedence). `servers` may be empty when configs
    /// exist but declare no MCPs. `warnings` is non-empty when at least one source
    /// failed to read/parse — propagate-and-surface, never silently drop.
    Available {
        servers: Vec<ClaudeServer>,
        warnings: Vec<String>,
    },
    /// NO source could be read/parsed. Distinct from `Available { warnings, servers: [] }`,
    /// which means at least one source loaded (even if empty).
    Unavailable(String),
}

/// `~/.claude.json` — where `claude mcp add -s user` writes registrations.
fn user_config_path() -> Option<PathBuf> {
    Some(home()?.join(".claude.json"))
}

/// Walk UP from `cwd` looking for the closest `.mcp.json`. Returns `None` if none
/// exists in any ancestor (incl. `cwd`). Matches Claude's "subdirectory-of-a-repo"
/// resolution — NOT git-root, which can be wrong in a monorepo. Bounded by filesystem
/// depth and never follows symlink loops (each step is `parent()`, monotone toward
/// the root).
fn project_mcp_json_path(cwd: &Path) -> Option<PathBuf> {
    // Use `canonicalize` only when it succeeds; if it fails (e.g. cwd doesn't exist
    // in a test), fall back to the original path so the walk-up still runs.
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut here = start.as_path();
    loop {
        let candidate = here.join(".mcp.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        match here.parent() {
            Some(parent) if parent != here => here = parent,
            _ => return None,
        }
    }
}

/// Parse one `mcpServers.<name>` JSON entry into a typed `Transport`. Tolerant of
/// missing/null fields — Claude's docs cover stdio and http; other shapes fall into
/// `Transport::Unknown` so the row still shows up in the Inspector (the user can
/// then fix the config).
fn parse_transport(def: &Value) -> Transport {
    let type_label = def
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    if let Some(command) = def.get("command").and_then(Value::as_str) {
        let args = def
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let env = def
            .get("env")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let cwd_field = def.get("cwd").and_then(Value::as_str).map(str::to_string);
        return Transport::Stdio {
            command: command.to_string(),
            args,
            env,
            cwd_field,
        };
    }
    if let Some(url) = def.get("url").and_then(Value::as_str) {
        let label = if !type_label.is_empty() {
            type_label
        } else {
            "http".to_string()
        };
        return Transport::Http {
            url: url.to_string(),
            label,
        };
    }
    Transport::Unknown {
        label: if type_label.is_empty() {
            "unknown".to_string()
        } else {
            type_label
        },
    }
}

/// Read the `mcpServers` table from a JSON config file. Returns `(servers, error)`:
/// - `servers` is `Some(vec)` when the file existed AND parsed (vec may be empty when
///   `mcpServers` is absent or empty);
/// - `error` is `Some(reason)` only when the file existed but couldn't be read/parsed
///   (so the inventory can surface that distinct from "absent").
fn read_mcp_servers(
    path: &Path,
    scope: impl Fn(PathBuf) -> ClaudeScope,
) -> (Option<Vec<(String, ClaudeServer)>>, Option<String>) {
    if !path.exists() {
        return (None, None);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return (None, Some(format!("read {}: {e}", path.display()))),
    };
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return (None, Some(format!("parse {}: {e}", path.display()))),
    };
    let Some(map) = v.get("mcpServers").and_then(Value::as_object) else {
        return (Some(Vec::new()), None);
    };
    let mut out = Vec::new();
    for (name, def) in map {
        if name.is_empty() {
            continue;
        }
        let transport = parse_transport(def);
        let scope_v = scope(path.to_path_buf());
        out.push((
            name.clone(),
            ClaudeServer {
                name: name.clone(),
                scope: scope_v,
                transport,
                overrides_user: false,
            },
        ));
    }
    (Some(out), None)
}

/// The merged Claude MCP inventory for `tui_cwd`. Project entries WIN on name collision
/// (Codex F-round 3 finding); flag the surviving row with `overrides_user = true` so
/// the Inspector can show that.
///
/// `Unavailable` is reserved for "no source could be read" (every probed config is
/// either missing or unreadable). `Available(vec![])` is the correct shape for "a
/// config exists but lists no MCP servers" (Codex Stop-gate finding) — that flows
/// into the Inspector's friendly empty-state hint.
pub fn inventory(tui_cwd: &Path) -> Inventory {
    let mut user_pairs: Vec<(String, ClaudeServer)> = Vec::new();
    let mut user_err: Option<String> = None;
    let mut user_source_found = false;
    if let Some(p) = user_config_path() {
        let (pairs, err) = read_mcp_servers(&p, |src| ClaudeScope::User { source: src });
        if let Some(v) = pairs {
            user_pairs = v;
            user_source_found = true;
        }
        user_err = err;
    }
    let mut project_pairs: Vec<(String, ClaudeServer)> = Vec::new();
    let mut project_err: Option<String> = None;
    let mut project_source_found = false;
    if let Some(p) = project_mcp_json_path(tui_cwd) {
        let (pairs, err) = read_mcp_servers(&p, |src| ClaudeScope::Project { source: src });
        if let Some(v) = pairs {
            project_pairs = v;
            project_source_found = true;
        }
        project_err = err;
    }

    merge_inventory(
        user_pairs,
        user_source_found,
        user_err,
        project_pairs,
        project_source_found,
        project_err,
    )
}

/// Pure merge/classification step (factored out for unit testing — the surrounding
/// `inventory` reads HOME-relative paths that aren't safely overridable in tests).
///
/// Codex Stop-gate F2 (this round): warnings (e.g. a corrupt `~/.claude.json`) MUST
/// flow through even when the other source loaded successfully — empty or not.
/// Otherwise the Inspector silently prints "no MCPs configured" when in fact a
/// source failed to parse and the user's MCPs are unreadable.
fn merge_inventory(
    user_pairs: Vec<(String, ClaudeServer)>,
    user_source_found: bool,
    user_err: Option<String>,
    project_pairs: Vec<(String, ClaudeServer)>,
    project_source_found: bool,
    project_err: Option<String>,
) -> Inventory {
    // Collect non-fatal warnings (parse/read errors from any source). These attach
    // to an `Available` even when another source loaded successfully — silent drop
    // was the regression Codex flagged this round.
    let mut warnings: Vec<String> = Vec::new();
    if let Some(u) = user_err.clone() {
        warnings.push(format!("user-scope (~/.claude.json): {u}"));
    }
    if let Some(p) = project_err.clone() {
        warnings.push(format!("project-scope (.mcp.json): {p}"));
    }

    if user_pairs.is_empty() && project_pairs.is_empty() {
        // No servers from either source. If a source was at least found (parsed,
        // even if empty), that's a valid Available with no servers. Otherwise we
        // truly have no information → Unavailable.
        if user_source_found || project_source_found || !warnings.is_empty() {
            // If warnings exist but no source loaded, we still have Available with
            // warnings — gives the Inspector a chance to render them.
            return Inventory::Available {
                servers: Vec::new(),
                warnings,
            };
        }
        return Inventory::Unavailable(
            "no Claude MCP servers configured (looked in ~/.claude.json and \
             <project>/.mcp.json walking up from cwd)"
                .to_string(),
        );
    }
    // Project wins on name collision. Flag the surviving project row.
    let mut merged: std::collections::BTreeMap<String, ClaudeServer> =
        std::collections::BTreeMap::new();
    for (k, v) in user_pairs {
        merged.insert(k, v);
    }
    for (k, mut v) in project_pairs {
        let user_had = merged.contains_key(&k);
        v.overrides_user = user_had;
        merged.insert(k, v);
    }
    Inventory::Available {
        servers: merged.into_values().collect(),
        warnings,
    }
}

/// Effective launch cwd for a stdio Claude MCP server, anchoring relative paths to
/// the source-file parent (Codex F-round 4 finding):
/// - project-scope, no explicit `cwd` ⇒ `.mcp.json` parent;
/// - project-scope, relative `cwd` ⇒ joined onto `.mcp.json` parent;
/// - project-scope, absolute `cwd` ⇒ used verbatim;
/// - user-scope, no explicit `cwd` ⇒ `tui_cwd` (matches Claude's behavior of running
///   user-scope MCPs from wherever Claude itself was started);
/// - user-scope, relative `cwd` ⇒ joined onto `~/.claude.json` parent (the home dir);
/// - user-scope, absolute `cwd` ⇒ used verbatim.
pub fn effective_cwd(server: &ClaudeServer, tui_cwd: &Path) -> PathBuf {
    let cwd_field = match &server.transport {
        Transport::Stdio { cwd_field, .. } => cwd_field.as_deref(),
        _ => None,
    };
    let source = match &server.scope {
        ClaudeScope::Project { source } => source.clone(),
        ClaudeScope::User { source } => source.clone(),
    };
    let anchor = source
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    match cwd_field {
        None => match &server.scope {
            ClaudeScope::Project { .. } => anchor,
            ClaudeScope::User { .. } => tui_cwd.to_path_buf(),
        },
        Some(c) => {
            let p = Path::new(c);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                anchor.join(p)
            }
        }
    }
}

/// DISCOVER (launch) `server`'s tools — for the Inspector's explicit 'd' key. Returns a
/// transport-aware error when the server is HTTP/Unknown (parity with `review_mcp`).
/// Uses [`crate::tool_discovery::discover`] (the UNCACHED entry point) so the codex-
/// only cache at `~/.ai-bridge/mcp-tools-cache.json` is NEVER touched by this path.
pub fn discover(server: &ClaudeServer, tui_cwd: &Path) -> Result<Vec<String>, String> {
    match &server.transport {
        Transport::Stdio {
            command, args, env, ..
        } => {
            let cwd = effective_cwd(server, tui_cwd).to_string_lossy().to_string();
            let spec = crate::tool_discovery::ServerSpec {
                name: server.name.clone(),
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
                cwd: Some(cwd),
            };
            crate::tool_discovery::discover(&spec).map_err(|e| format!("discovery failed: {e}"))
        }
        Transport::Http { label, .. } => Err(format!(
            "'{}' uses transport '{label}' — tool discovery requires stdio. \
             To toggle this server in Claude, edit ~/.claude.json (or the project's \
             .mcp.json) and restart Claude.",
            server.name
        )),
        Transport::Unknown { label } => Err(format!(
            "'{}' has unrecognized transport '{label}' — tool discovery is unavailable.",
            server.name
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_file(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn temp_root(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aibridge-claude-mcp-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parse_servers_stdio_http_sse_unknown() {
        // stdio: command present + env/args propagated.
        let stdio = json!({
            "command": "npx",
            "args": ["-y", "@anthropic/server-filesystem"],
            "env": {"ROOT": "/tmp"},
            "cwd": "./scripts"
        });
        match parse_transport(&stdio) {
            Transport::Stdio {
                command,
                args,
                env,
                cwd_field,
            } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "@anthropic/server-filesystem"]);
                assert_eq!(env, vec![("ROOT".to_string(), "/tmp".to_string())]);
                assert_eq!(cwd_field.as_deref(), Some("./scripts"));
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
        // http: url present, no command; type label propagates.
        let http = json!({"type": "http", "url": "https://mcp.example.com/x"});
        match parse_transport(&http) {
            Transport::Http { url, label } => {
                assert_eq!(url, "https://mcp.example.com/x");
                assert_eq!(label, "http");
            }
            other => panic!("expected Http, got {other:?}"),
        }
        // sse: url + type=sse → label = "sse".
        let sse = json!({"type": "sse", "url": "https://mcp.example.com/sse"});
        assert_eq!(parse_transport(&sse).label(), "sse");
        // unknown: no command, no url.
        let weird = json!({"foo": "bar"});
        match parse_transport(&weird) {
            Transport::Unknown { label } => assert_eq!(label, "unknown"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn project_mcp_lookup_picks_closest_ancestor() {
        // Two `.mcp.json` files on the way up; the CLOSER one wins.
        let root = temp_root("closest");
        write_file(
            &root.join(".mcp.json"),
            r#"{"mcpServers":{"root_one":{"command":"echo"}}}"#,
        );
        let inner_dir = root.join("a").join("b");
        std::fs::create_dir_all(&inner_dir).unwrap();
        write_file(
            &inner_dir.join(".mcp.json"),
            r#"{"mcpServers":{"inner_one":{"command":"echo"}}}"#,
        );
        let cwd = inner_dir.join("c").join("d");
        std::fs::create_dir_all(&cwd).unwrap();
        let p = project_mcp_json_path(&cwd).expect("closest ancestor present");
        // canonicalize on Windows can produce a UNC prefix; compare via canonicalize on both.
        let expect = std::fs::canonicalize(inner_dir.join(".mcp.json")).unwrap();
        let got = std::fs::canonicalize(&p).unwrap();
        assert_eq!(got, expect);
    }

    #[test]
    fn project_mcp_lookup_returns_none_when_absent() {
        let root = temp_root("absent");
        let cwd = root.join("a").join("b");
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(project_mcp_json_path(&cwd).is_none());
    }

    #[test]
    fn project_overrides_user_when_names_collide() {
        // Two configs both define `ctx`; project value must survive and be flagged.
        let root = temp_root("collide");
        let user_path = root.join(".claude.json");
        write_file(
            &user_path,
            r#"{"mcpServers":{"ctx":{"command":"user-cmd","args":["U"]}}}"#,
        );
        let proj_dir = root.join("proj");
        std::fs::create_dir_all(&proj_dir).unwrap();
        let proj_path = proj_dir.join(".mcp.json");
        write_file(
            &proj_path,
            r#"{"mcpServers":{"ctx":{"command":"proj-cmd","args":["P"]}}}"#,
        );
        let (user_pairs, _) = read_mcp_servers(&user_path, |src| ClaudeScope::User { source: src });
        let (proj_pairs, _) =
            read_mcp_servers(&proj_path, |src| ClaudeScope::Project { source: src });
        let mut merged: std::collections::BTreeMap<String, ClaudeServer> =
            std::collections::BTreeMap::new();
        for (k, v) in user_pairs.unwrap_or_default() {
            merged.insert(k, v);
        }
        for (k, mut v) in proj_pairs.unwrap_or_default() {
            let user_had = merged.contains_key(&k);
            v.overrides_user = user_had;
            merged.insert(k, v);
        }
        let ctx = merged.get("ctx").expect("present");
        assert!(matches!(ctx.scope, ClaudeScope::Project { .. }));
        assert!(ctx.overrides_user);
        match &ctx.transport {
            Transport::Stdio { command, args, .. } => {
                assert_eq!(command, "proj-cmd");
                assert_eq!(args, &vec!["P".to_string()]);
            }
            other => panic!("expected Stdio (project), got {other:?}"),
        }
    }

    #[test]
    fn distinct_names_merge_with_correct_scopes() {
        let root = temp_root("distinct");
        let user_path = root.join(".claude.json");
        write_file(
            &user_path,
            r#"{"mcpServers":{"a":{"command":"echo","args":["A"]}}}"#,
        );
        let proj_dir = root.join("proj");
        std::fs::create_dir_all(&proj_dir).unwrap();
        let proj_path = proj_dir.join(".mcp.json");
        write_file(
            &proj_path,
            r#"{"mcpServers":{"b":{"command":"echo","args":["B"]}}}"#,
        );
        let (user_pairs, _) = read_mcp_servers(&user_path, |src| ClaudeScope::User { source: src });
        let (proj_pairs, _) =
            read_mcp_servers(&proj_path, |src| ClaudeScope::Project { source: src });
        let mut merged: std::collections::BTreeMap<String, ClaudeServer> =
            std::collections::BTreeMap::new();
        for (k, v) in user_pairs.unwrap_or_default() {
            merged.insert(k, v);
        }
        for (k, mut v) in proj_pairs.unwrap_or_default() {
            v.overrides_user = merged.contains_key(&k);
            merged.insert(k, v);
        }
        let a = merged.get("a").unwrap();
        assert!(matches!(a.scope, ClaudeScope::User { .. }));
        assert!(!a.overrides_user);
        let b = merged.get("b").unwrap();
        assert!(matches!(b.scope, ClaudeScope::Project { .. }));
        assert!(!b.overrides_user);
    }

    #[test]
    fn effective_cwd_project_no_explicit_cwd() {
        let mcp_path = PathBuf::from("/tmp/a/.mcp.json");
        let server = ClaudeServer {
            name: "x".into(),
            scope: ClaudeScope::Project {
                source: mcp_path.clone(),
            },
            transport: Transport::Stdio {
                command: "node".into(),
                args: vec!["./srv.js".into()],
                env: vec![],
                cwd_field: None,
            },
            overrides_user: false,
        };
        let tui = PathBuf::from("/tmp/a/b/c");
        let eff = effective_cwd(&server, &tui);
        assert_eq!(eff, PathBuf::from("/tmp/a"));
    }

    #[test]
    fn effective_cwd_project_relative_cwd() {
        let mcp_path = PathBuf::from("/tmp/a/.mcp.json");
        let server = ClaudeServer {
            name: "x".into(),
            scope: ClaudeScope::Project { source: mcp_path },
            transport: Transport::Stdio {
                command: "node".into(),
                args: vec![],
                env: vec![],
                cwd_field: Some("scripts".into()),
            },
            overrides_user: false,
        };
        let eff = effective_cwd(&server, Path::new("/tmp/a/b/c"));
        assert_eq!(eff, PathBuf::from("/tmp/a").join("scripts"));
    }

    #[test]
    fn effective_cwd_project_absolute_cwd() {
        let mcp_path = PathBuf::from("/tmp/a/.mcp.json");
        let abs_cwd = if cfg!(windows) { "C:\\srv" } else { "/srv" };
        let server = ClaudeServer {
            name: "x".into(),
            scope: ClaudeScope::Project { source: mcp_path },
            transport: Transport::Stdio {
                command: "node".into(),
                args: vec![],
                env: vec![],
                cwd_field: Some(abs_cwd.to_string()),
            },
            overrides_user: false,
        };
        let eff = effective_cwd(&server, Path::new("/tmp/a/b/c"));
        assert_eq!(eff, PathBuf::from(abs_cwd));
    }

    #[test]
    fn effective_cwd_user_falls_back_to_tui_cwd() {
        let user_path = PathBuf::from("/home/user/.claude.json");
        let server = ClaudeServer {
            name: "x".into(),
            scope: ClaudeScope::User { source: user_path },
            transport: Transport::Stdio {
                command: "node".into(),
                args: vec![],
                env: vec![],
                cwd_field: None,
            },
            overrides_user: false,
        };
        let tui = PathBuf::from("/work/project");
        assert_eq!(effective_cwd(&server, &tui), tui);
    }

    #[test]
    fn merge_inventory_returns_available_empty_when_source_found_but_empty() {
        // A config FILE that exists but lists no MCP servers must produce
        // `Available { servers: [], warnings: [] }`, NOT `Unavailable`.
        let inv = merge_inventory(
            Vec::new(),
            /*user_source_found=*/ true,
            /*user_err=*/ None,
            Vec::new(),
            /*project_source_found=*/ false,
            /*project_err=*/ None,
        );
        match inv {
            Inventory::Available { servers, warnings } => {
                assert!(servers.is_empty());
                assert!(warnings.is_empty());
            }
            Inventory::Unavailable(why) => panic!("expected Available, got Unavailable({why})"),
        }
        // Also: when ONLY project source is found-but-empty.
        let inv = merge_inventory(Vec::new(), false, None, Vec::new(), true, None);
        assert!(matches!(
            inv,
            Inventory::Available { servers, warnings }
                if servers.is_empty() && warnings.is_empty()
        ));
    }

    #[test]
    fn merge_inventory_returns_unavailable_when_no_source_found_and_no_warnings() {
        // No sources, no errors → Unavailable with the friendly hint.
        let inv = merge_inventory(Vec::new(), false, None, Vec::new(), false, None);
        match inv {
            Inventory::Unavailable(why) => {
                assert!(why.contains("~/.claude.json"));
                assert!(why.contains(".mcp.json"));
            }
            Inventory::Available { .. } => panic!("expected Unavailable when no source found"),
        }
    }

    #[test]
    fn merge_inventory_surfaces_errors_when_other_source_was_found_but_empty() {
        // Codex Stop-gate F2 regression: corrupt `~/.claude.json` + empty project
        // `.mcp.json` previously returned `Available(vec![])` and the Inspector
        // silently showed "no MCPs configured". The user's MCPs were unreadable.
        // Now: warnings flow through so the Inspector can render them.
        let inv = merge_inventory(
            Vec::new(),
            /*user_source_found=*/ false,
            /*user_err=*/ Some("parse ~/.claude.json: expected `:` at line 5".into()),
            Vec::new(),
            /*project_source_found=*/ true,
            /*project_err=*/ None,
        );
        match inv {
            Inventory::Available { servers, warnings } => {
                assert!(servers.is_empty(), "no servers expected");
                assert_eq!(warnings.len(), 1, "warnings = {warnings:?}");
                assert!(
                    warnings[0].contains("user-scope") && warnings[0].contains("parse"),
                    "warning should name the source + the error: {warnings:?}"
                );
            }
            Inventory::Unavailable(why) => {
                panic!("expected Available + warnings, got Unavailable({why})")
            }
        }
    }

    #[test]
    fn merge_inventory_surfaces_errors_alongside_servers() {
        // Even when the OTHER source DID return servers, an error on the failed
        // source must not be silently dropped. Without this, a user with a
        // working project `.mcp.json` would never learn their user `~/.claude.json`
        // is broken.
        let project_pairs = vec![(
            "x".to_string(),
            ClaudeServer {
                name: "x".into(),
                scope: ClaudeScope::Project {
                    source: PathBuf::from("/p/.mcp.json"),
                },
                transport: Transport::Stdio {
                    command: "echo".into(),
                    args: vec![],
                    env: vec![],
                    cwd_field: None,
                },
                overrides_user: false,
            },
        )];
        let inv = merge_inventory(
            Vec::new(),
            false,
            Some("EACCES /home/u/.claude.json".into()),
            project_pairs,
            true,
            None,
        );
        match inv {
            Inventory::Available { servers, warnings } => {
                assert_eq!(servers.len(), 1, "the project server must survive");
                assert_eq!(servers[0].name, "x");
                assert_eq!(warnings.len(), 1, "the user-scope error must survive");
                assert!(warnings[0].contains("EACCES"));
            }
            Inventory::Unavailable(why) => {
                panic!("expected Available with warnings, got Unavailable({why})")
            }
        }
    }

    #[test]
    fn source_does_not_call_cache_writer() {
        // Structural regression test: the PRODUCTION code in this module never
        // references the codex-only cache helpers, so a Claude discovery can NEVER
        // mutate ~/.ai-bridge/mcp-tools-cache.json. Read the source, drop comments
        // AND drop the `#[cfg(test)]` block (so the assertion strings here don't
        // false-positive against themselves).
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("claude_mcp.rs");
        let src = std::fs::read_to_string(&path).expect("read own source");
        let prod = src
            .split("#[cfg(test)]")
            .next()
            .expect("source has non-test portion");
        let code_only: String = prod
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // Look for the fully-qualified call form so the doc comments + struct field
        // names can't false-positive (e.g. a future field named `cached` would be fine).
        assert!(
            !code_only.contains("tool_discovery::discover_and_cache"),
            "production code must not call tool_discovery::discover_and_cache"
        );
        assert!(
            !code_only.contains("tool_discovery::store"),
            "production code must not call tool_discovery::store"
        );
    }
}
