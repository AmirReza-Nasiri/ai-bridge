//! `aibridge init`: wire the gate into Claude Code for the current project.
//!
//! Default install is **local / per-machine / untracked** (Codex Round 26):
//! - MCP server → Claude local scope in `~/.claude.json` (via `claude mcp add`,
//!   which canonicalizes the project path correctly), NOT a committed `.mcp.json`
//!   with a machine-specific absolute path.
//! - Stop hook → `.claude/settings.local.json` (gitignored by Claude), NOT the
//!   committed `settings.json`.
//! - Gate-awareness note → `CLAUDE.local.md` (+ `.git/info/exclude`), NOT the
//!   committed `CLAUDE.md`.
//! - Ownership recorded in `.ai-bridge/install-state.json`.
//!
//! A committed/team install (`.mcp.json` + `settings.json` + `CLAUDE.md`) is a
//! future `--shared` mode.

use aibridge_platform::{DefaultPlatform, Platform};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const GATE_LINE: &str = "AI Bridge is installed locally in this project. If the Stop hook blocks with peer-review findings, address them before finishing. If AI Bridge asks for a user decision, stop and ask the user.";

/// What `init` did, for a human-readable report.
pub struct InitReport {
    pub actions: Vec<String>,
    pub restart_required: bool,
}

/// Wire AI Bridge into the project rooted at `project` (local scope).
pub fn init(project: &Path) -> Result<InitReport> {
    let exe = std::env::current_exe().context("resolving the aibridge executable path")?;
    let exe_str = exe.to_string_lossy().to_string();
    let mut actions = Vec::new();

    register_mcp_server(project, &exe_str, &mut actions)?;
    install_stop_hook(project, &mut actions)?;
    add_gate_line(project, &mut actions)?;
    write_install_state(project, &exe_str, &mut actions)?;

    Ok(InitReport {
        actions,
        restart_required: true,
    })
}

/// Register the MCP server in Claude's local scope via the `claude` CLI so the
/// project path is canonicalized exactly as Claude expects (idempotent: remove
/// then add).
fn register_mcp_server(project: &Path, exe: &str, actions: &mut Vec<String>) -> Result<()> {
    let claude = DefaultPlatform::find_executable("claude")
        .context("locating the `claude` CLI (needed to register the MCP server)")?;

    let _ = DefaultPlatform::command_for(&claude)
        .args(["mcp", "remove", "aibridge", "-s", "local"])
        .current_dir(project)
        .output(); // ignore: may not exist yet

    let out = DefaultPlatform::command_for(&claude)
        .args(["mcp", "add", "aibridge", "-s", "local", "--"])
        .arg(exe)
        .arg("mcp-server")
        .current_dir(project)
        .output()
        .context("running `claude mcp add`")?;
    if !out.status.success() {
        return Err(anyhow!(
            "`claude mcp add` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    actions.push("registered the `aibridge` MCP server (Claude local scope)".to_string());
    Ok(())
}

/// Install the Stop hook into `.claude/settings.local.json` (untracked).
fn install_stop_hook(project: &Path, actions: &mut Vec<String>) -> Result<()> {
    let path = project.join(".claude").join("settings.local.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", display(parent)))?;
    }
    let mut root = read_json(&path)?;
    if !root.is_object() {
        root = json!({});
    }
    if root
        .pointer("/disableAllHooks")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        actions.push(
            "WARNING: disableAllHooks is true — the gate will not fire until you unset it"
                .to_string(),
        );
    }
    let changed = {
        let obj = root.as_object_mut().expect("object");
        let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
        if !hooks.is_object() {
            *hooks = json!({});
        }
        let stop = hooks
            .as_object_mut()
            .expect("object")
            .entry("Stop")
            .or_insert_with(|| json!([]));
        if !stop.is_array() {
            *stop = json!([]);
        }
        let arr = stop.as_array_mut().expect("array");
        if arr.iter().any(is_aibridge_stop_group) {
            false
        } else {
            arr.push(json!({
                "hooks": [{
                    "type": "mcp_tool",
                    "server": "aibridge",
                    "tool": "review_stop",
                    "statusMessage": "AI Bridge peer review",
                    "timeout": 120,
                    "input": {
                        "cwd": "${cwd}",
                        "stop_hook_active": "${stop_hook_active}",
                        "session_id": "${session_id}",
                        "transcript_path": "${transcript_path}"
                    }
                }]
            }));
            true
        }
    };
    if changed {
        backup_if_exists(&path, actions)?;
        write_json(&path, &root)?;
        actions.push(format!(
            "installed the Stop review hook in {}",
            display(&path)
        ));
    } else {
        actions.push(format!(
            "Stop review hook already present in {}",
            display(&path)
        ));
    }
    Ok(())
}

fn is_aibridge_stop_group(group: &Value) -> bool {
    group
        .pointer("/hooks")
        .and_then(Value::as_array)
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("server").and_then(Value::as_str) == Some("aibridge")
                    && h.get("tool").and_then(Value::as_str) == Some("review_stop")
            })
        })
        .unwrap_or(false)
}

/// Write the gate-awareness note to `CLAUDE.local.md` and keep it git-ignored.
fn add_gate_line(project: &Path, actions: &mut Vec<String>) -> Result<()> {
    let path = project.join("CLAUDE.local.md");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if !existing.contains("AI Bridge is installed locally") {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(GATE_LINE);
        content.push('\n');
        std::fs::write(&path, content).with_context(|| format!("writing {}", display(&path)))?;
        actions.push(format!(
            "added the gate-awareness note to {}",
            display(&path)
        ));
    }
    git_exclude(project, "CLAUDE.local.md", actions);
    Ok(())
}

/// Best-effort: add `entry` to `.git/info/exclude` so a local-only file stays
/// untracked without editing the committed `.gitignore`.
fn git_exclude(project: &Path, entry: &str, actions: &mut Vec<String>) {
    let exclude = project.join(".git").join("info").join("exclude");
    if !exclude.parent().map(Path::exists).unwrap_or(false) {
        return; // not a git repo
    }
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == entry) {
        return;
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(entry);
    next.push('\n');
    if std::fs::write(&exclude, next).is_ok() {
        actions.push(format!("git-ignored {entry} via .git/info/exclude"));
    }
}

/// Record what AI Bridge owns, for `doctor` / future `uninit`.
fn write_install_state(project: &Path, exe: &str, actions: &mut Vec<String>) -> Result<()> {
    let dir = project.join(".ai-bridge");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", display(&dir)))?;
    let state = json!({
        "version": crate::version(),
        "scope": "local",
        "aibridge_exe": exe,
        "mcp_server": "aibridge (claude local scope)",
        "stop_hook": ".claude/settings.local.json",
        "gate_note": "CLAUDE.local.md",
        "installed_at_ms": now_ms(),
    });
    let path = dir.join("install-state.json");
    write_json(&path, &state)?;
    actions.push(format!("recorded install state in {}", display(&path)));
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => {
            serde_json::from_str(&s).with_context(|| format!("parsing {}", display(path)))
        }
        _ => Ok(json!({})),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let pretty = serde_json::to_string_pretty(value)?;
    std::fs::write(path, pretty + "\n").with_context(|| format!("writing {}", display(path)))
}

fn backup_if_exists(path: &Path, actions: &mut Vec<String>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let bak = path.with_file_name(format!("{name}.aibridge-{}.bak", now_ms()));
    std::fs::copy(path, &bak).with_context(|| format!("backing up {}", display(path)))?;
    actions.push(format!("backed up {} → {}", display(path), display(&bak)));
    Ok(())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn display(path: &Path) -> String {
    path.display().to_string()
}
