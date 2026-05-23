//! `aibridge init`: wire the gate into Claude Code for the current project.
//!
//! Default install is **local / per-machine / untracked** (Codex Round 26):
//! - MCP server → Claude USER scope in `~/.claude.json` (via `claude mcp add -s
//!   user`), NOT a committed `.mcp.json` with a machine-specific absolute path.
//!   User scope is casing-proof on Windows (local/project-keyed paths can split
//!   across `D:` vs `d:`); the tools are global, the gate stays per-project via
//!   the Stop hook below.
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

/// Wire AI Bridge into the project rooted at `project`. `plan_gate` is normally
/// true (DEFAULT-ON, the planning-phase gate — `init --no-plan-gate` opts out):
/// it wires UserPromptSubmit + a broad PreToolUse hook that denies writes/Bash
/// until the plan is approved (that hook also does rtk, subsuming the rtk-only
/// Bash hook). `rtk` only matters when `plan_gate` is off (wires the narrow hook).
pub fn init(project: &Path, rtk: bool, plan_gate: bool) -> Result<InitReport> {
    let exe = std::env::current_exe().context("resolving the aibridge executable path")?;
    let exe_str = exe.to_string_lossy().to_string();
    let mut actions = Vec::new();

    register_mcp_server(project, &exe_str, &mut actions)?;
    install_stop_hook(project, &mut actions)?;
    if plan_gate {
        // The plan-gate's PreToolUse hook uses a BROAD matcher (write tools + Bash)
        // and the shared `pretooluse` handler does rtk too, so it subsumes the
        // rtk-only Bash hook. Don't also wire that narrower one.
        install_plan_gate(project, &exe_str, &mut actions)?;
    } else if rtk {
        install_rtk_hook(project, &exe_str, &mut actions)?;
    }
    if (rtk || plan_gate) && DefaultPlatform::find_executable("rtk").is_err() {
        // Detect + nudge (never auto-download a third-party binary): the hook is
        // wired and fails open, but tell the user how to get the actual binary.
        actions.push(format!(
            "NOTE: PreToolUse hook wired, but `rtk` isn't on PATH yet — output \
             compression stays off (fail-open) until you install it: {}",
            rtk_install_hint()
        ));
    }
    add_gate_line(project, &mut actions)?;
    write_install_state(project, &exe_str, &mut actions)?;
    git_exclude(project, ".ai-bridge/", &mut actions);

    // Record global install provenance (which binary to replace on `update`).
    crate::update::record_install(&exe_str);

    Ok(InitReport {
        actions,
        restart_required: true,
    })
}

/// Install the rtk PreToolUse rewrite hook into `.claude/settings.local.json`.
/// It is `aibridge hook pretooluse` (exec form), which routes safe noisy
/// commands through `rtk` and fails open otherwise.
fn install_rtk_hook(project: &Path, exe: &str, actions: &mut Vec<String>) -> Result<()> {
    let path = project.join(".claude").join("settings.local.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", display(parent)))?;
    }
    let mut root = read_json(&path)?;
    if !root.is_object() {
        root = json!({});
    }
    let changed = {
        let obj = root.as_object_mut().expect("object");
        let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
        if !hooks.is_object() {
            *hooks = json!({});
        }
        let pre = hooks
            .as_object_mut()
            .expect("object")
            .entry("PreToolUse")
            .or_insert_with(|| json!([]));
        if !pre.is_array() {
            *pre = json!([]);
        }
        let arr = pre.as_array_mut().expect("array");
        let present = arr.iter().any(|g| {
            g.pointer("/hooks")
                .and_then(Value::as_array)
                .map(|hs| {
                    hs.iter().any(|h| {
                        h.get("args")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().any(|x| x.as_str() == Some("pretooluse")))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });
        if present {
            false
        } else {
            arr.push(json!({
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": exe,
                    "args": ["hook", "pretooluse"],
                    "timeout": 30
                }]
            }));
            true
        }
    };
    if changed {
        backup_if_exists(&path, actions)?;
        write_json(&path, &root)?;
        actions.push(format!(
            "wired the rtk PreToolUse optimizer hook in {} (safe mode)",
            display(&path)
        ));
    } else {
        actions.push("rtk PreToolUse hook already present".to_string());
    }
    Ok(())
}

const PLAN_GATE_NOTE: &str = "AI Bridge PLAN GATE is active here. Coding habit: for any task that will change files, FIRST do read-only discovery (Read/Grep/Glob), form a short todolist, then call `mcp__aibridge__plan_gate` with your plan (todos, approach, intended_files, risk_surfaces, test_plan) and iterate until it returns <AI-BRIDGE-APPROVE/>. Writes (Write/Edit/MultiEdit/NotebookEdit) and Bash are BLOCKED until the plan is approved for the current task.";

/// True iff a `PreToolUse` group is an AI Bridge `pretooluse` hook group, matched
/// by OWNERSHIP (command basename `aibridge[.exe]` AND args `["hook","pretooluse"]`)
/// rather than merely "an arg contains `pretooluse`", so a foreign hook is never
/// removed by the plan-gate de-dupe.
fn group_is_aibridge_pretooluse(g: &Value) -> bool {
    g.pointer("/hooks")
        .and_then(Value::as_array)
        .map(|hs| hs.iter().any(hook_is_aibridge_pretooluse))
        .unwrap_or(false)
}

fn hook_is_aibridge_pretooluse(h: &Value) -> bool {
    let args_match = h
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.len() == 2 && a[0].as_str() == Some("hook") && a[1].as_str() == Some("pretooluse")
        })
        .unwrap_or(false);
    let cmd_is_ours = h
        .get("command")
        .and_then(Value::as_str)
        .map(|c| {
            let base = Path::new(c)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(c)
                .to_ascii_lowercase();
            base == "aibridge" || base == "aibridge.exe"
        })
        .unwrap_or(false);
    args_match && cmd_is_ours
}

/// Install the plan gate (DEFAULT-ON; disable with `init --no-plan-gate`): an
/// `enabled` marker, a `UserPromptSubmit` hook that starts a fresh task epoch, a
/// broad `PreToolUse` hook that denies writes/Bash until the plan is approved (the
/// shared `pretooluse` handler also does rtk), and a coding-habit note in
/// `CLAUDE.local.md`. A per-session bypass is `AIBRIDGE_PLAN_GATE=0`.
fn install_plan_gate(project: &Path, exe: &str, actions: &mut Vec<String>) -> Result<()> {
    // STAGE the gate (enabled.pending), don't activate it yet — the MCP server
    // promotes it on its next startup. This prevents the install deadlock where the
    // gate would block this very session before the `plan_gate` tool (which needs a
    // Claude Code restart) is reachable.
    crate::plan_gate::enable_pending(&project.to_string_lossy())
        .context("staging the plan-gate marker")?;

    let path = project.join(".claude").join("settings.local.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", display(parent)))?;
    }
    let mut root = read_json(&path)?;
    if !root.is_object() {
        root = json!({});
    }
    let mut changed = false;
    {
        let obj = root.as_object_mut().expect("object");
        let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
        if !hooks.is_object() {
            *hooks = json!({});
        }
        let hooks = hooks.as_object_mut().expect("object");

        // PreToolUse: a broad matcher covering the write tools + Bash + the
        // in-repo `run` MCP tool (arbitrary shell — a bypass otherwise).
        let pre = hooks.entry("PreToolUse").or_insert_with(|| json!([]));
        if !pre.is_array() {
            *pre = json!([]);
        }
        let arr = pre.as_array_mut().expect("array");
        // De-dupe by OWNERSHIP: drop EVERY AI Bridge `pretooluse` group (the
        // rtk-only Bash one, or any prior plan-gate matcher) and re-add the single
        // canonical broad group. Leaving two would double-run the hook with
        // unspecified ordering. Ownership = command basename `aibridge[.exe]` AND
        // args `["hook","pretooluse"]`, so a FOREIGN hook that merely uses a
        // "pretooluse" arg is never dropped (matters now that this runs by default).
        let snapshot = arr.clone();
        arr.retain(|g| !group_is_aibridge_pretooluse(g));
        arr.push(json!({
            "matcher": "Write|Edit|MultiEdit|NotebookEdit|Bash|mcp__aibridge__run",
            "hooks": [{
                "type": "command",
                "command": exe,
                "args": ["hook", "pretooluse"],
                "timeout": 30
            }]
        }));
        if *arr != snapshot {
            changed = true;
        }

        // UserPromptSubmit: start a fresh task epoch on each new prompt.
        let ups = hooks.entry("UserPromptSubmit").or_insert_with(|| json!([]));
        if !ups.is_array() {
            *ups = json!([]);
        }
        let arr = ups.as_array_mut().expect("array");
        let present = arr.iter().any(|g| {
            g.pointer("/hooks")
                .and_then(Value::as_array)
                .map(|hs| {
                    hs.iter().any(|h| {
                        h.get("args")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().any(|x| x.as_str() == Some("user-prompt-submit")))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        });
        if !present {
            arr.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": exe,
                    "args": ["hook", "user-prompt-submit"],
                    "timeout": 10
                }]
            }));
            changed = true;
        }
    }
    if changed {
        backup_if_exists(&path, actions)?;
        write_json(&path, &root)?;
        actions.push(format!(
            "wired the plan gate (UserPromptSubmit + broad PreToolUse) in {}",
            display(&path)
        ));
    } else {
        actions.push("plan-gate hooks already present".to_string());
    }

    // Coding-habit note so Claude proactively calls plan_gate (not just after a deny).
    let note_path = project.join("CLAUDE.local.md");
    let existing = std::fs::read_to_string(&note_path).unwrap_or_default();
    if !existing.contains("AI Bridge PLAN GATE is active") {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(PLAN_GATE_NOTE);
        content.push('\n');
        std::fs::write(&note_path, content)
            .with_context(|| format!("writing {}", display(&note_path)))?;
        actions.push(format!(
            "added the plan-gate habit note to {}",
            display(&note_path)
        ));
    }
    git_exclude(project, "CLAUDE.local.md", actions);
    Ok(())
}

/// Register the MCP server in Claude's local scope via the `claude` CLI so the
/// project path is canonicalized exactly as Claude expects (idempotent: remove
/// then add).
fn register_mcp_server(project: &Path, exe: &str, actions: &mut Vec<String>) -> Result<()> {
    let claude = DefaultPlatform::find_executable("claude")
        .context("locating the `claude` CLI (needed to register the MCP server)")?;

    let _ = DefaultPlatform::command_for(&claude)
        .args(["mcp", "remove", "aibridge", "-s", "user"])
        .current_dir(project)
        .output(); // ignore: may not exist yet

    let out = DefaultPlatform::command_for(&claude)
        .args(["mcp", "add", "aibridge", "-s", "user", "--"])
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
    actions.push(
        "registered the `aibridge` MCP server (Claude user scope — all projects)".to_string(),
    );
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
                    // Generous: a thorough xhigh review can take several minutes,
                    // and the user prioritizes quality over speed. AI Bridge's own
                    // backstop (CALL_TIMEOUT) fires first; a crashed Codex is caught
                    // instantly via EOF, so this never masks the hang fix.
                    "timeout": 1800,
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

/// OS-appropriate command to install the OPTIONAL rtk binary. AI Bridge never
/// auto-downloads it (it's third-party); `doctor` and `init --rtk` print this so
/// the user can install it themselves.
pub fn rtk_install_hint() -> String {
    match aibridge_platform::platform_name() {
        "windows" => "download `rtk-x86_64-pc-windows-msvc` from \
                      https://github.com/rtk-ai/rtk/releases and put rtk.exe on PATH \
                      (e.g. ~/.local/bin)"
            .to_string(),
        "macos" => "`brew install rtk` (or `cargo install --git \
                    https://github.com/rtk-ai/rtk rtk`)"
            .to_string(),
        _ => "`brew install rtk` (or see https://github.com/rtk-ai/rtk)".to_string(),
    }
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

/// Best-effort: add `entry` to the repo's `.git/info/exclude` so a local-only
/// file stays untracked without editing the committed `.gitignore`.
///
/// Works when `project` is a SUBDIRECTORY of the repo: the exclude file lives at
/// the repo root (not `project/.git`) and its patterns match relative to that
/// root, so we resolve the file via `git rev-parse --git-path` and prefix `entry`
/// with the project's path-from-root (`--show-prefix`, empty at the root).
fn git_exclude(project: &Path, entry: &str, actions: &mut Vec<String>) {
    let git = match DefaultPlatform::find_executable("git") {
        Ok(g) => g,
        Err(_) => return,
    };
    let run = |args: &[&str]| -> Option<String> {
        let out = DefaultPlatform::command_for(&git)
            .args(args)
            .current_dir(project)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    // Locate the exclude file (handles subdirs and linked worktrees).
    let exclude = match run(&["rev-parse", "--git-path", "info/exclude"]) {
        Some(p) if !p.is_empty() => {
            let pb = Path::new(&p);
            if pb.is_absolute() {
                pb.to_path_buf()
            } else {
                project.join(pb)
            }
        }
        _ => return, // not a git repo
    };

    // Anchor the pattern to the repo root: empty prefix for a top-level project,
    // e.g. "sub/dir/" for a subdirectory project.
    let prefix = run(&["rev-parse", "--show-prefix"]).unwrap_or_default();
    let pattern = format!("{prefix}{entry}");

    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == pattern) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&pattern);
    next.push('\n');
    if std::fs::write(&exclude, next).is_ok() {
        actions.push(format!("git-ignored {pattern} via .git/info/exclude"));
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
        "mcp_server": "aibridge (claude user scope)",
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    fn tmp() -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("aibridge-install-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn read_settings(project: &Path) -> Value {
        let s =
            std::fs::read_to_string(project.join(".claude").join("settings.local.json")).unwrap();
        serde_json::from_str(&s).unwrap()
    }
    fn pre_groups(v: &Value) -> Vec<Value> {
        v.pointer("/hooks/PreToolUse")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }
    const CANON: &str = "Write|Edit|MultiEdit|NotebookEdit|Bash|mcp__aibridge__run";

    #[test]
    fn plan_gate_install_is_idempotent() {
        let p = tmp();
        install_plan_gate(&p, "aibridge", &mut Vec::new()).unwrap();
        let first = read_settings(&p);
        install_plan_gate(&p, "aibridge", &mut Vec::new()).unwrap();
        let second = read_settings(&p);
        assert_eq!(first, second, "re-running install must not change settings");
        let ours = pre_groups(&second)
            .iter()
            .filter(|g| group_is_aibridge_pretooluse(g))
            .count();
        assert_eq!(ours, 1, "exactly one AI Bridge PreToolUse group");
    }

    #[test]
    fn plan_gate_install_preserves_foreign_hooks() {
        let p = tmp();
        std::fs::create_dir_all(p.join(".claude")).unwrap();
        // A FOREIGN hook that happens to use a "pretooluse" arg must NOT be dropped.
        let seed = json!({ "hooks": { "PreToolUse": [{
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "/usr/bin/other-tool", "args": ["pretooluse"] }]
        }]}});
        std::fs::write(
            p.join(".claude").join("settings.local.json"),
            serde_json::to_string(&seed).unwrap(),
        )
        .unwrap();
        install_plan_gate(&p, "aibridge", &mut Vec::new()).unwrap();
        let groups = pre_groups(&read_settings(&p));
        assert!(
            groups
                .iter()
                .any(|g| g.pointer("/hooks/0/command").and_then(Value::as_str)
                    == Some("/usr/bin/other-tool")),
            "foreign hook must be preserved"
        );
        assert!(
            groups.iter().any(group_is_aibridge_pretooluse),
            "our canonical group must be added"
        );
    }

    #[test]
    fn plan_gate_install_replaces_old_aibridge_bash_hook() {
        let p = tmp();
        std::fs::create_dir_all(p.join(".claude")).unwrap();
        // An OLD AI Bridge rtk-only Bash pretooluse hook must be replaced, not doubled.
        let seed = json!({ "hooks": { "PreToolUse": [{
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": "aibridge", "args": ["hook", "pretooluse"] }]
        }]}});
        std::fs::write(
            p.join(".claude").join("settings.local.json"),
            serde_json::to_string(&seed).unwrap(),
        )
        .unwrap();
        install_plan_gate(&p, "aibridge", &mut Vec::new()).unwrap();
        let groups = pre_groups(&read_settings(&p));
        let ours: Vec<_> = groups
            .iter()
            .filter(|g| group_is_aibridge_pretooluse(g))
            .collect();
        assert_eq!(ours.len(), 1, "old Bash hook replaced, not duplicated");
        assert_eq!(
            ours[0].get("matcher").and_then(Value::as_str),
            Some(CANON),
            "the surviving group is the canonical broad one"
        );
    }
}
