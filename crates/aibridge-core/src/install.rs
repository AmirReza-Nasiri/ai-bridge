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

/// Wire AI Bridge into the project rooted at `project`. With `rtk`, also wire the
/// rtk output-optimizer PreToolUse hook (safe mode). With `plan_gate`, wire the
/// opt-in pre-execution plan gate (UserPromptSubmit + a broad PreToolUse hook that
/// denies writes/Bash until the plan is approved — that hook also does rtk).
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

/// Install the OPT-IN plan gate: an `enabled` marker, a `UserPromptSubmit` hook
/// that starts a fresh task epoch, a broad `PreToolUse` hook that denies
/// writes/Bash until the plan is approved (the shared `pretooluse` handler also
/// does rtk), and a coding-habit note in `CLAUDE.local.md`.
fn install_plan_gate(project: &Path, exe: &str, actions: &mut Vec<String>) -> Result<()> {
    crate::plan_gate::enable(&project.to_string_lossy())
        .context("writing the plan-gate enabled marker")?;

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
        // De-dupe by ownership: drop EVERY AI Bridge `pretooluse` group (the
        // rtk-only Bash one, or any prior plan-gate matcher) and re-add the single
        // canonical broad group. Leaving two would double-run the hook with
        // unspecified ordering; matching by "calls our pretooluse" survives matcher
        // string changes across versions.
        let snapshot = arr.clone();
        arr.retain(|g| {
            !g.pointer("/hooks")
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
            "wired the OPT-IN plan gate (UserPromptSubmit + broad PreToolUse) in {}",
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
