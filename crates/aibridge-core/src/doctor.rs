//! `aibridge doctor` / `aibridge selftest`: ONE comprehensive check of the whole
//! install and its live connections on this platform.
//!
//! Fast by default (no model calls): binary discovery, a quota-free
//! `codex mcp-server` handshake, MCP registration + Stop-hook wiring, install
//! state. `--full` adds a real Codex round-trip (uses quota).

use aibridge_platform::{platform_name, DefaultPlatform, Platform};
use serde_json::Value;
use std::path::Path;

/// Outcome of a single check.
pub enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn tag(&self) -> &'static str {
        match self {
            Status::Pass => "[ ok ]",
            Status::Warn => "[warn]",
            Status::Fail => "[FAIL]",
        }
    }
}

/// A single named check + detail.
pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: String,
}

/// A full doctor report.
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// True when no check failed (warnings are tolerated).
    pub fn ok(&self) -> bool {
        !self.checks.iter().any(|c| matches!(c.status, Status::Fail))
    }

    /// Print the report and a one-line verdict.
    pub fn print(&self) {
        println!("AI Bridge doctor ({})\n", platform_name());
        for c in &self.checks {
            if c.detail.is_empty() {
                println!("  {} {}", c.status.tag(), c.name);
            } else {
                println!("  {} {} — {}", c.status.tag(), c.name, c.detail);
            }
        }
        let warns = self
            .checks
            .iter()
            .filter(|c| matches!(c.status, Status::Warn))
            .count();
        let fails = self
            .checks
            .iter()
            .filter(|c| matches!(c.status, Status::Fail))
            .count();
        println!();
        if fails > 0 {
            println!("RESULT: {fails} failed, {warns} warning(s) — fix the failures above.");
        } else if warns > 0 {
            println!(
                "RESULT: ok with {warns} warning(s) — usually: run `aibridge init`, then restart Claude."
            );
        } else {
            println!("RESULT: all good — AI Bridge is wired and connected.");
        }
    }
}

fn check(status: Status, name: &str, detail: impl Into<String>) -> Check {
    Check {
        status,
        name: name.to_string(),
        detail: detail.into(),
    }
}

fn version_of(name: &str) -> Option<(std::path::PathBuf, String)> {
    let exe = DefaultPlatform::find_executable(name).ok()?;
    let out = DefaultPlatform::command_for(&exe)
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().unwrap_or("").trim().to_string();
    Some((exe, line))
}

/// Run all checks for `project`. `full` adds a real (quota-using) Codex round-trip.
pub fn run(project: &Path, full: bool) -> Report {
    let mut checks = Vec::new();

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    checks.push(check(
        Status::Pass,
        "aibridge",
        format!("v{} ({exe})", crate::version()),
    ));

    match version_of("claude") {
        Some((p, v)) => checks.push(check(
            Status::Pass,
            "claude CLI",
            format!("{v} ({})", p.display()),
        )),
        None => checks.push(check(Status::Fail, "claude CLI", "not found")),
    }

    match version_of("codex") {
        Some((p, v)) => {
            checks.push(check(
                Status::Pass,
                "codex CLI",
                format!("{v} ({})", p.display()),
            ));
            match crate::codex::CodexPeer::spawn() {
                Ok(_) => checks.push(check(
                    Status::Pass,
                    "codex mcp-server handshake",
                    "connects (quota-free)",
                )),
                Err(e) => checks.push(check(
                    Status::Fail,
                    "codex mcp-server handshake",
                    e.to_string(),
                )),
            }
        }
        None => checks.push(check(Status::Fail, "codex CLI", "not found")),
    }

    match DefaultPlatform::find_executable("rtk") {
        Ok(p) => checks.push(check(
            Status::Pass,
            "rtk (output optimizer)",
            p.display().to_string(),
        )),
        Err(_) => checks.push(check(
            Status::Warn,
            "rtk (output optimizer)",
            "not installed (optional)",
        )),
    }

    match DefaultPlatform::find_executable("git") {
        Ok(_) => checks.push(check(Status::Pass, "git", "available")),
        Err(_) => checks.push(check(
            Status::Warn,
            "git",
            "not found (review_diff / gate need it)",
        )),
    }

    checks.push(mcp_registration(project));
    checks.push(stop_hook(project));
    checks.push(install_state(project));
    checks.push(spawned_context(project));

    if full {
        checks.push(e2e_roundtrip(project));
    }

    Report { checks }
}

fn mcp_registration(project: &Path) -> Check {
    let out = DefaultPlatform::find_executable("claude")
        .ok()
        .and_then(|c| {
            DefaultPlatform::command_for(&c)
                .args(["mcp", "get", "aibridge"])
                .current_dir(project)
                .output()
                .ok()
        });
    match out {
        Some(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            if s.contains("Connected") {
                check(
                    Status::Pass,
                    "aibridge MCP registration",
                    "registered + connected",
                )
            } else {
                check(
                    Status::Warn,
                    "aibridge MCP registration",
                    "registered (restart Claude to connect)",
                )
            }
        }
        _ => check(
            Status::Warn,
            "aibridge MCP registration",
            "not registered — run `aibridge init`",
        ),
    }
}

fn stop_hook(project: &Path) -> Check {
    let present = std::fs::read_to_string(project.join(".claude").join("settings.local.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| has_aibridge_stop_hook(&v))
        .unwrap_or(false);
    if present {
        check(
            Status::Pass,
            "Stop review hook",
            "installed (.claude/settings.local.json)",
        )
    } else {
        check(
            Status::Warn,
            "Stop review hook",
            "not installed — run `aibridge init`",
        )
    }
}

fn install_state(project: &Path) -> Check {
    if project
        .join(".ai-bridge")
        .join("install-state.json")
        .exists()
    {
        check(
            Status::Pass,
            "install state",
            ".ai-bridge/install-state.json",
        )
    } else {
        check(
            Status::Warn,
            "install state",
            "missing — run `aibridge init`",
        )
    }
}

/// Compare the terminal's CLI resolution against the snapshot the MCP server
/// recorded at startup, to catch a "git/codex missing in the Claude-spawned
/// context" PATH mismatch (a real Windows failure class).
fn spawned_context(project: &Path) -> Check {
    let snap = std::fs::read_to_string(
        project
            .join(".ai-bridge")
            .join("runtime")
            .join("snapshot.json"),
    )
    .ok()
    .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let snap = match snap {
        Some(s) => s,
        None => return check(
            Status::Warn,
            "spawned-context PATH",
            "unknown — restart Claude (so the MCP server records its runtime), then re-run doctor",
        ),
    };

    let resolved_in_snapshot = |name: &str| {
        snap.pointer(&format!("/resolved/{name}"))
            .and_then(Value::as_str)
            .is_some()
    };
    let mut missing = Vec::new();
    for tool in ["git", "codex"] {
        if DefaultPlatform::find_executable(tool).is_ok() && !resolved_in_snapshot(tool) {
            missing.push(tool);
        }
    }
    if missing.is_empty() {
        check(
            Status::Pass,
            "spawned-context PATH",
            "git/codex resolvable in the Claude-spawned MCP server too",
        )
    } else {
        check(
            Status::Fail,
            "spawned-context PATH",
            format!(
                "{} found in terminal but MISSING in the Claude-spawned context — \
                 restart Claude from a terminal with these on PATH, or use absolute paths",
                missing.join(", ")
            ),
        )
    }
}

fn e2e_roundtrip(project: &Path) -> Check {
    let cwd = project.display().to_string();
    let result = crate::codex::CodexPeer::spawn().and_then(|mut p| {
        p.ask(
            "Reply with exactly this token and nothing else: AIBRIDGE_FULL_OK",
            &cwd,
        )
    });
    match result {
        Ok(r) if r.contains("AIBRIDGE_FULL_OK") => check(
            Status::Pass,
            "e2e Codex round-trip",
            "real model reply received",
        ),
        Ok(r) => check(
            Status::Warn,
            "e2e Codex round-trip",
            format!(
                "unexpected reply: {}",
                r.trim().chars().take(50).collect::<String>()
            ),
        ),
        Err(e) => check(Status::Fail, "e2e Codex round-trip", e.to_string()),
    }
}

fn has_aibridge_stop_hook(v: &Value) -> bool {
    v.pointer("/hooks/Stop")
        .and_then(Value::as_array)
        .map(|groups| {
            groups.iter().any(|g| {
                g.pointer("/hooks")
                    .and_then(Value::as_array)
                    .map(|hs| {
                        hs.iter().any(|h| {
                            h.get("server").and_then(Value::as_str) == Some("aibridge")
                                && h.get("tool").and_then(Value::as_str) == Some("review_stop")
                        })
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}
