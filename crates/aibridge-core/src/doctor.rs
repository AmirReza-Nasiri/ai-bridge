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
/// `check_updates` adds a network check against GitHub Releases (off by default so
/// `doctor` stays fast + offline).
pub fn run(project: &Path, full: bool, check_updates: bool) -> Report {
    let mut checks = Vec::new();

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    checks.push(check(
        Status::Pass,
        "aibridge version",
        format!("{} ({exe})", crate::VERSION_FULL),
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

    checks.push(codex_launch_mode());
    checks.push(review_effort());

    match DefaultPlatform::find_executable("rtk") {
        Ok(p) => checks.push(check(
            Status::Pass,
            "rtk (output optimizer)",
            p.display().to_string(),
        )),
        Err(_) => checks.push(check(
            Status::Warn,
            "rtk (output optimizer)",
            format!(
                "not installed (optional). To enable safe command-output compression: {}, then `aibridge init --rtk`",
                crate::install::rtk_install_hint()
            ),
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
    checks.push(mcp_binary_path(project));
    checks.push(install_metadata());
    checks.push(stop_hook(project));
    checks.push(task_start_hook(project));
    checks.push(codex_mcp_servers(project));
    checks.push(review_mcp_policy());
    checks.push(review_feed_skills());
    checks.push(plan_gate_status(project));
    checks.push(install_state(project));
    checks.push(spawned_context(project));

    if check_updates {
        checks.push(update_check());
    }
    if full {
        checks.push(e2e_roundtrip(project));
    }

    Report { checks }
}

/// Offline: compare the recorded install path (`~/.ai-bridge/install.json`) to the
/// running exe. A mismatch means a later `update` could replace the WRONG binary.
fn install_metadata() -> Check {
    let running = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    match crate::update::recorded_install_path() {
        None => check(
            Status::Pass,
            "install metadata",
            "not recorded yet (run `aibridge init` so `update` knows which binary to replace)",
        ),
        Some(recorded) => {
            // Compare case-insensitively on Windows (drive-letter / case noise).
            let same = recorded.eq_ignore_ascii_case(&running)
                || recorded
                    .replace('\\', "/")
                    .eq_ignore_ascii_case(&running.replace('\\', "/"));
            if same {
                check(
                    Status::Pass,
                    "install metadata",
                    format!("recorded ({recorded})"),
                )
            } else {
                check(
                    Status::Warn,
                    "install metadata",
                    format!(
                        "recorded install path ({recorded}) differs from the running binary \
                         ({running}) — `update` would target the recorded one; re-run `aibridge init` \
                         from the intended install if that's wrong"
                    ),
                )
            }
        }
    }
}

/// Network (only with `--check-updates`): is a newer GitHub release available?
/// Warning-only — never fails doctor (offline/auth/missing-gh are expected).
fn update_check() -> Check {
    use std::time::Duration;
    match crate::update::latest_release(Duration::from_secs(3)) {
        crate::update::ReleaseLookup::Found { tag, .. } => {
            match (
                crate::update::current_version(),
                crate::update::parse_version(&tag),
            ) {
                (Some(c), Some(l)) if l > c => check(
                    Status::Warn,
                    "updates",
                    format!("newer release available: {c} → {l} (run `aibridge update`)"),
                ),
                (Some(c), Some(l)) if l == c => check(
                    Status::Pass,
                    "updates",
                    format!("on the latest release ({c})"),
                ),
                (Some(_), Some(l)) => check(
                    Status::Pass,
                    "updates",
                    format!("ahead of latest release {l} (dev build)"),
                ),
                _ => check(
                    Status::Warn,
                    "updates",
                    format!("latest tag '{tag}' isn't clean semver"),
                ),
            }
        }
        crate::update::ReleaseLookup::None => {
            check(Status::Pass, "updates", "no published releases yet")
        }
        crate::update::ReleaseLookup::GhMissing => check(
            Status::Warn,
            "updates",
            "can't check — GitHub CLI (`gh`) not installed",
        ),
        crate::update::ReleaseLookup::Timeout => {
            check(Status::Warn, "updates", "check timed out reaching GitHub")
        }
        crate::update::ReleaseLookup::Failed(why) => {
            check(Status::Warn, "updates", format!("check failed: {why}"))
        }
    }
}

/// Report the reasoning effort AI Bridge uses for reviews and the user's global
/// Codex setting. Informational: it sets the expectation that a thorough review
/// takes minutes (so a slow review isn't mistaken for a hang — the exact
/// confusion that masked the root cause during dogfood).
fn review_effort() -> Check {
    let effort = crate::codex::review_reasoning_effort();
    let global = codex_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| parse_reasoning_effort(&t));
    let global_note = match global {
        Some(g) if g != effort => format!("; your global Codex config is '{g}'"),
        _ => String::new(),
    };
    let latency = match effort {
        "xhigh" | "high" => "thorough — reviews take minutes, no cutoff",
        "minimal" | "low" => "fast — shallower review",
        _ => "balanced speed and depth",
    };
    check(
        Status::Pass,
        "review reasoning effort",
        format!("{effort} ({latency}){global_note}"),
    )
}

/// Path to the user's Codex `config.toml` (`$CODEX_HOME/config.toml`, else
/// `~/.codex/config.toml`), for reporting their global reasoning effort.
fn codex_config_path() -> Option<std::path::PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        return Some(Path::new(&home).join("config.toml"));
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".codex").join("config.toml"))
}

/// Best-effort scan for `model_reasoning_effort = "<x>"` in a Codex config.toml
/// (avoids a TOML dependency; only the common top-level form is needed here).
fn parse_reasoning_effort(toml: &str) -> Option<String> {
    toml.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("model_reasoning_effort")?;
        // Require a real key boundary so `model_reasoning_effort_foo` doesn't match.
        if !rest.starts_with([' ', '=', '\t']) {
            return None;
        }
        let v = rest
            .trim_start_matches([' ', '\t', '='])
            .trim()
            .trim_matches('"')
            .to_string();
        (!v.is_empty()).then_some(v)
    })
}

/// How the warm Codex child will be launched. On Windows the npm `.cmd` shim
/// must resolve to a direct `node <entry>.js` launch; the degraded `cmd /C`
/// fallback can hang the review gate under the no-console Claude Code MCP host.
fn codex_launch_mode() -> Check {
    let exe = match DefaultPlatform::find_executable("codex") {
        Ok(e) => e,
        Err(_) => return check(Status::Warn, "codex launch mode", "codex not found"),
    };
    let plan = DefaultPlatform::spawn_plan(&exe);
    if plan.kind.is_safe() {
        check(
            Status::Pass,
            "codex launch mode",
            format!("{} — {}", plan.kind.as_str(), plan.program),
        )
    } else {
        check(
            Status::Fail,
            "codex launch mode",
            format!(
                "degraded '{}' — the npm .cmd shim wasn't resolved to a direct node launch, \
                 so the review gate can hang under Claude Code on Windows. Ensure `node` is on PATH.",
                plan.kind.as_str()
            ),
        )
    }
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

/// Flag a fragile install shape: the MCP server registered to a Cargo build
/// artifact (`target/release` or `target/debug`) instead of a stable path like
/// `~/.local/bin`. A rebuild or `cargo clean` would then break the running server
/// (Codex flagged this during the update-command design review).
fn mcp_binary_path(project: &Path) -> Check {
    let out = DefaultPlatform::find_executable("claude")
        .ok()
        .and_then(|c| {
            DefaultPlatform::command_for(&c)
                .args(["mcp", "get", "aibridge"])
                .current_dir(project)
                .output()
                .ok()
        });
    let text = match out {
        Some(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_lowercase(),
        _ => {
            return check(
                Status::Warn,
                "aibridge binary path",
                "could not read the MCP registration (run `aibridge init`)",
            )
        }
    };
    // Match `target` + sep + `release`/`debug` with a LEADING separator so it only
    // fires on a real path component (not a dir merely named "…target…"); the
    // trailing `\aibridge.exe` guarantees a bounded fragment is present.
    let in_build_dir = [
        "/target/release",
        "\\target\\release",
        "/target/debug",
        "\\target\\debug",
    ]
    .iter()
    .any(|m| text.contains(m));
    if in_build_dir {
        check(
            Status::Warn,
            "aibridge binary path",
            "MCP points at a Cargo build artifact (target/…) — re-register to a stable path so a \
             rebuild or `cargo clean` can't break it: `claude mcp remove aibridge -s user` then \
             `claude mcp add aibridge -s user -- <dir>/aibridge.exe mcp-server`",
        )
    } else {
        check(
            Status::Pass,
            "aibridge binary path",
            "registered to a stable path (not a build artifact)",
        )
    }
}

/// Report the plan gate's marker state. `Pending` means `init` staged it but it
/// won't enforce until the MCP server starts (i.e. Claude Code is restarted) — the
/// fix for the post-install deadlock.
fn plan_gate_status(project: &Path) -> Check {
    match crate::plan_gate::marker_state(&project.display().to_string()) {
        crate::plan_gate::MarkerState::Disabled => check(
            Status::Pass,
            "plan gate",
            "off (init wires it by default; --no-plan-gate to skip)",
        ),
        crate::plan_gate::MarkerState::Pending => check(
            Status::Warn,
            "plan gate",
            "staged — restart Claude Code to activate it (the plan_gate tool connects on restart)",
        ),
        crate::plan_gate::MarkerState::Active => check(
            Status::Pass,
            "plan gate",
            "active — Codex must approve the plan before writes/Bash. If the plan_gate tool ever \
             isn't reachable, restart Claude Code or set AIBRIDGE_PLAN_GATE=0 to bypass",
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

/// Which of codex's MCP servers stay enabled during AI Bridge REVIEWS (user policy
/// via `aibridge review-mcp`; default none → tool-free reviews). WARNS when a
/// review-enabled server looks browser/scrape — those elicit/run long and can STALL a
/// review (the bug this policy exists to prevent). Scans `~/.codex/config.toml` only —
/// a project-local codex config's servers aren't covered (documented limitation).
fn review_mcp_policy() -> Check {
    let names = match crate::review_mcp::codex_server_names() {
        Some(n) => n,
        None => {
            return check(
                Status::Warn,
                "review-mcp policy",
                "~/.codex/config.toml is present but unreadable/unparseable — AI Bridge will \
                 REFUSE to start a review peer (fail-closed) until it's fixed",
            )
        }
    };
    if names.is_empty() {
        return check(
            Status::Pass,
            "review-mcp policy",
            "no codex MCP servers → reviews run tool-free",
        );
    }
    let allow = crate::review_mcp::allowlist();
    let enabled: Vec<String> = names
        .iter()
        .filter(|n| allow.iter().any(|a| a == *n))
        .cloned()
        .collect();
    let risky: Vec<String> = enabled
        .iter()
        .filter(|n| crate::review_mcp::server_looks_interactive(n.as_str()))
        .cloned()
        .collect();
    let desc = if enabled.is_empty() {
        "none (pure reasoning)".to_string()
    } else {
        enabled.join(", ")
    };
    if risky.is_empty() {
        check(
            Status::Pass,
            "review-mcp policy",
            format!("codex servers enabled in reviews: {desc} (home config only; change via `aibridge review-mcp`)"),
        )
    } else {
        check(
            Status::Warn,
            "review-mcp policy",
            format!(
                "enabled in reviews: {desc} — ⚠ {} looks browser/scrape and can STALL a review; \
                 disable with `aibridge review-mcp disable <name>`",
                risky.join(", ")
            ),
        )
    }
}

/// The SKILLS half of the review "feed" audit (the MCP half is `review_mcp_policy`):
/// how many skills the Bridge's codex reviews actually load from `~/.agents/skills`.
/// Observability, not intelligence — codex itself auto-selects the relevant skill per
/// diff; this just makes the feed visible + warns when it's empty/unsynced. No
/// hardcoded "which skills are critical" judgement (that would fight the no-hardcode rule).
fn review_feed_skills() -> Check {
    let m = crate::skills::mirror_status();
    if m.agents_valid == 0 {
        return check(
            Status::Warn,
            "review feed (skills)",
            "no skills in ~/.agents/skills — the Bridge's codex reviews have NO skill knowledge; \
             run `aibridge skills sync` (mirrors the ~/.claude/skills hub)",
        );
    }
    // Stale mirror: a plain count is falsely green when the hub is AHEAD of agents —
    // codex would review against an outdated skill set. Warn with the drift (Codex find).
    if m.missing_from_agents > 0 || m.drifted > 0 {
        return check(
            Status::Warn,
            "review feed (skills)",
            format!(
                "{} skill(s) in ~/.agents/skills, but the ~/.claude/skills hub is AHEAD \
                 ({} not mirrored, {} drifted) — codex reviews use the STALE set; \
                 run `aibridge skills sync`",
                m.agents_valid, m.missing_from_agents, m.drifted
            ),
        );
    }
    // Hub fully mirrored, but agents has codex-installed EXTRAS not in the hub — the
    // feed still includes them, so don't claim plain "in sync" (Codex find).
    if m.claude_present && m.only_in_agents > 0 {
        return check(
            Status::Warn,
            "review feed (skills)",
            format!(
                "{} skill(s) in ~/.agents/skills — hub fully mirrored, but {} are AGENTS-ONLY \
                 (not in ~/.claude/skills) and still used in codex reviews; fold them into the hub \
                 (copy to ~/.claude/skills) for one source of truth, or leave them intentionally",
                m.agents_valid, m.only_in_agents
            ),
        );
    }
    let sync = if m.claude_present {
        "fully in sync with the hub"
    } else {
        "agents-only (no ~/.claude/skills hub)"
    };
    check(
        Status::Pass,
        "review feed (skills)",
        format!(
            "{} skill(s) in ~/.agents/skills ({sync}) — codex auto-selects the relevant one per \
             diff; skill/policy changes apply on the next review-peer spawn (reload the window)",
            m.agents_valid
        ),
    )
}

/// Report codex's configured MCP servers + any RECENT declined elicitation, so the
/// user can follow up. Informational by default (servers existing is normal); WARNS
/// only when a tool recently needed interactive input during a review (was declined
/// headlessly) — that's the actionable case. Reads config + the local log only.
fn codex_mcp_servers(project: &Path) -> Check {
    // Authoritative: ask codex (`codex mcp list --json`) — its own resolver, correct
    // cross-platform + project/profile aware. A failure is "unknown", never "none".
    let servers = match crate::review_mcp::codex_inventory(&project.display().to_string()) {
        crate::review_mcp::Inventory::Unavailable(why) => {
            return check(
                Status::Warn,
                "codex MCP servers",
                format!(
                    "unknown — couldn't query codex's MCP inventory ({why}). Reviews still \
                     enforce the policy from the config file; put codex on PATH for an \
                     authoritative list."
                ),
            );
        }
        crate::review_mcp::Inventory::Available(servers) => servers,
    };
    let names: Vec<String> = servers.iter().map(|s| s.name.clone()).collect();
    let listed = if names.is_empty() {
        "none configured".to_string()
    } else {
        format!("{} configured ({})", names.len(), names.join(", "))
    };

    // Enforcement (`spawn_overrides`) reads the USER config file; the authoritative
    // inventory may also include project/profile/system servers that the file read
    // misses → those would NOT be disabled in reviews. Surface that gap honestly
    // rather than implying coverage that doesn't exist.
    let file_names = crate::review_mcp::codex_server_names().unwrap_or_default();
    let extra: Vec<&str> = names
        .iter()
        .filter(|n| !file_names.iter().any(|f| f == *n))
        .map(String::as_str)
        .collect();
    let mismatch = if extra.is_empty() {
        String::new()
    } else {
        format!(
            " ⚠ {} server(s) come from project/profile config ({}) and are NOT covered by the \
             review override (which reads user config only) — they could run during reviews.",
            extra.len(),
            extra.join(", ")
        )
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    let elicit = crate::progress::last_declined_elicitation(&project.display().to_string())
        .filter(|(ms, _)| now.saturating_sub(*ms) <= DAY_MS);
    let status = if elicit.is_some() || !extra.is_empty() {
        Status::Warn
    } else {
        Status::Pass
    };
    let detail = match elicit {
        Some((_, summary)) => format!(
            "{listed}.{mismatch} A tool RECENTLY needed interactive input during a review and was \
             declined headlessly: {summary}. Configure that server for headless use (API key / \
             non-interactive flag / default target), or run the review interactively."
        ),
        None => format!(
            "{listed}{mismatch} — codex may use these in reviews; any that need interactive input \
             are declined headlessly (no hang). See `aibridge status` / .ai-bridge/elicitations.jsonl"
        ),
    };
    check(status, "codex MCP servers", detail)
}

/// The `UserPromptSubmit` task-start hook records the per-task review BASE the Stop
/// gate measures committed work from. Without it the Stop gate falls back to
/// reviewing the uncommitted tree only — so a `git commit` before the turn ends can
/// slip past review. Flag a missing one so the user re-runs `init`.
fn task_start_hook(project: &Path) -> Check {
    let present = std::fs::read_to_string(project.join(".claude").join("settings.local.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| has_aibridge_user_prompt_hook(&v))
        .unwrap_or(false);
    if present {
        check(
            Status::Pass,
            "task-start hook",
            "installed — Stop reviews committed-since-task work too (no commit-bypass)",
        )
    } else {
        check(
            Status::Warn,
            "task-start hook",
            "missing (UserPromptSubmit) — Stop reviews only the uncommitted tree; \
             a pre-Stop `git commit` can bypass review. Run `aibridge init`",
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
        p.open_thread(
            "Reply with exactly this token and nothing else: AIBRIDGE_FULL_OK",
            &cwd,
            "low", // connectivity check only — fastest effort
        )
        .map(|(_thread_id, text)| text)
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

fn has_aibridge_user_prompt_hook(v: &Value) -> bool {
    v.pointer("/hooks/UserPromptSubmit")
        .and_then(Value::as_array)
        .map(|groups| {
            groups.iter().any(|g| {
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
            })
        })
        .unwrap_or(false)
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

// ───────────────────────── Debug-tab report (TUI) ─────────────────────────
//
// The Debug tab in `aibridge status` shows a single copy-pasteable report. To keep
// secrets out of a casual paste, we BOTH (1) build the report from CURATED fields
// (e.g. env keys, not env values; install-state minimal fields; never the plan_gate
// approved-plan text or audit detail strings) AND (2) run the final text through a
// sanitizer that masks common secret shapes. The curated approach is the primary
// defense; the sanitizer is the safety net for fields whose contents we can't fully
// predict (URL query strings in doctor details, etc.). Codex F-round 3 / round 5.

const REDACTED: &str = "<redacted>";
const REDACTED_LONG: &str = "<redacted-long>";
const REDACTED_QUERY: &str = "<redacted-query>";

/// The fixed list of sensitive key NAMES that trigger value redaction. Matched
/// case-insensitively at word boundaries. Underscores and hyphens are both accepted
/// (`api_key` / `api-key`).
const SENSITIVE_KEYS: &[&str] = &[
    "token",
    "secret",
    "key",
    "password",
    "passwd",
    "pwd",
    "auth",
    "bearer",
    "apikey",
    "api_key",
    "api-key",
    "access_token",
    "access-token",
    "client_secret",
    "client-secret",
    "private_key",
    "private-key",
];

/// `true` when the byte is part of a URL (not a delimiter that ends one).
fn is_url_byte(b: u8) -> bool {
    !(b.is_ascii_whitespace() || b == b'"' || b == b'\'' || b == b'<' || b == b'>' || b == b'`')
}

/// Find the next case-insensitive occurrence of `needle` in `haystack` starting at
/// `from`. Returns the (start, end) of the match in byte indices.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<(usize, usize)> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || from > h.len() {
        return None;
    }
    let max = h.len().checked_sub(n.len())?;
    for i in from..=max {
        let mut ok = true;
        for (j, &nb) in n.iter().enumerate() {
            if !h[i + j].eq_ignore_ascii_case(&nb) {
                ok = false;
                break;
            }
        }
        if ok {
            return Some((i, i + n.len()));
        }
    }
    None
}

/// `true` when the byte at `i` is a SECRET-MATCHING word boundary: start/end of
/// string, or the previous byte is NOT ASCII alphanumeric. `_` and `-` ARE treated
/// as boundaries here (deliberately different from a Rust identifier boundary) so
/// prefixed env-style names like `OPENAI_API_KEY=...`, `ANTHROPIC_API_KEY=...`,
/// `GITHUB_TOKEN=...`, and `MY-SERVICE-SECRET=...` match the `key` / `token` /
/// `secret` needles. Codex Stop-gate finding: the v1 version inherited the Rust-
/// identifier rule and silently let those leak through `sanitize_text`.
fn at_word_boundary(s: &str, i: usize) -> bool {
    if i == 0 || i >= s.len() {
        return true;
    }
    !s.as_bytes()[i - 1].is_ascii_alphanumeric()
}

/// Step 1: redact URL credentials + query strings. Walks `line`, copies normal bytes,
/// and when it spots `http://` or `https://`, processes the URL through to its
/// natural end (whitespace or quote / angle bracket).
fn redact_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        // Find the next URL start (either scheme), taking the earlier of the two.
        let http_hit = find_ci(line, "http://", i);
        let https_hit = find_ci(line, "https://", i);
        let next = match (http_hit, https_hit) {
            (Some(a), Some(b)) if a.0 <= b.0 => Some(a),
            (Some(_), Some(b)) => Some(b),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some((start, end)) = next else {
            out.push_str(&line[i..]);
            break;
        };
        // Copy text before the URL.
        out.push_str(&line[i..start]);
        // Find URL end.
        let b = line.as_bytes();
        let mut url_end = end;
        while url_end < b.len() && is_url_byte(b[url_end]) {
            url_end += 1;
        }
        let url = &line[start..url_end];
        let scheme_len = end - start;
        let after_scheme = &url[scheme_len..];
        // Detect `userinfo@host` — basic auth in URL.
        let host_part = if let Some(at_idx) = after_scheme.find('@') {
            // Make sure the userinfo region doesn't cross a path separator.
            let userinfo = &after_scheme[..at_idx];
            if !userinfo.contains('/') && !userinfo.is_empty() {
                out.push_str(&url[..scheme_len]);
                out.push_str(REDACTED);
                out.push('@');
                &after_scheme[at_idx + 1..]
            } else {
                out.push_str(&url[..scheme_len]);
                after_scheme
            }
        } else {
            out.push_str(&url[..scheme_len]);
            after_scheme
        };
        // Detect `?query` — replace.
        if let Some(q) = host_part.find('?') {
            out.push_str(&host_part[..q]);
            out.push('?');
            out.push_str(REDACTED_QUERY);
        } else {
            out.push_str(host_part);
        }
        i = url_end;
    }
    out
}

/// Step 2: redact Bearer/Basic tokens. Looks for `Bearer` or `Basic` at a word
/// boundary, requires at least one whitespace after, then redacts the next token.
fn redact_bearer_basic(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let needles = ["bearer", "basic"];
        let mut hit: Option<(usize, usize, &str)> = None;
        for n in &needles {
            if let Some((s, e)) = find_ci(line, n, i) {
                if hit.map(|(hs, _, _)| s < hs).unwrap_or(true) {
                    hit = Some((s, e, n));
                }
            }
        }
        let Some((start, scheme_end, _kind)) = hit else {
            out.push_str(&line[i..]);
            break;
        };
        // Word boundary on the left.
        if !at_word_boundary(line, start) {
            out.push_str(&line[i..scheme_end]);
            i = scheme_end;
            continue;
        }
        // Right side: must be whitespace, then a token.
        let after = &line[scheme_end..];
        let bytes = after.as_bytes();
        let mut p = 0;
        while p < bytes.len() && bytes[p] == b' ' {
            p += 1;
        }
        if p == 0 || p >= bytes.len() || bytes[p].is_ascii_whitespace() {
            // No following whitespace or no token after — leave as-is.
            out.push_str(&line[i..scheme_end]);
            i = scheme_end;
            continue;
        }
        let token_start = scheme_end + p;
        let mut token_end = token_start;
        while token_end < line.len() {
            let b = line.as_bytes()[token_end];
            if b.is_ascii_whitespace() || b == b',' || b == b';' || b == b'"' || b == b'\'' {
                break;
            }
            token_end += 1;
        }
        out.push_str(&line[i..token_start]);
        out.push_str(REDACTED);
        i = token_end;
    }
    out
}

/// `true` when the byte continues an identifier (letter, digit, `_`, or `-`). Used
/// for the RIGHT-side boundary in `redact_sensitive_kv` so a needle like `api_key`
/// matched inside `api_key_path` is rejected (the `_` extends the identifier — we'd
/// otherwise rewrite `api_key_path = secret` as `api_key=<redacted>` and lose
/// `_path`). The LEFT side uses `at_word_boundary` (alphanumeric-only) so prefixed
/// env names like `OPENAI_API_KEY` still match — the asymmetry is intentional.
fn is_identifier_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Scan `line` for the first occurrence of `q` that ISN'T preceded by a `\X`
/// escape. Returns the byte index of the unescaped `q`, or `None` if not found.
/// Symmetric with the `\X = 2 bytes` rule inside `redact_sensitive_kv` so a
/// multi-line quoted secret terminated by `\"` doesn't end early. Codex
/// Stop-hook round 4: `sanitize_text` uses this to find where a carried-over
/// multi-line quoted value ends.
fn consume_until_unescaped_byte(line: &str, q: u8) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if b == q {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Step 3: redact sensitive `key = value` / `key: value` pairs, including JSON
/// shapes like `"api_key":"sk-..."` and `'access_token': 'abc'`. Match strategy:
/// - LEFT boundary via `at_word_boundary` (alphanumeric-only), so `OPENAI_API_KEY`
///   still finds `api_key` at the `_API_KEY` position.
/// - RIGHT boundary via `!is_identifier_byte`, so `api_key_path = secret` does NOT
///   match `api_key` (the `_` extends the identifier).
/// - Tolerate an optional closing quote on the key (JSON shape), then optional
///   whitespace, require `=`/`:`, then optional whitespace + optional opening
///   quote, then read the value until any of: whitespace / closing quote / `,` /
///   `;` / `}` / `]`. The trailing `}`/`]` are JSON value terminators.
///
/// Returns `(rewritten, unclosed_quote)`. `unclosed_quote = Some(q)` when the
/// function ran off the end of the line still inside a quoted sensitive value
/// (`sanitize_text` carries this across line boundaries so multi-line credentials
/// like `private_key="-----BEGIN\nABCDEF\n-----END"` get fully scrubbed instead of
/// leaking lines 2+). Codex Stop-hook round 4 finding.
fn redact_sensitive_kv(line: &str) -> (String, Option<u8>) {
    // Build a sorted-by-length-DESC needle list so `api_key` matches before `key`.
    let mut needles: Vec<&str> = SENSITIVE_KEYS.to_vec();
    needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let bytes = line.as_bytes();
    while i < line.len() {
        // Find the EARLIEST hit among all needles at-or-after i, with a word boundary.
        let mut hit: Option<(usize, usize, &str)> = None;
        for n in &needles {
            let mut probe = i;
            while let Some((s, e)) = find_ci(line, n, probe) {
                if at_word_boundary(line, s) {
                    // Right boundary: next byte must not extend the identifier.
                    let right_ok = e == line.len() || !is_identifier_byte(bytes[e]);
                    if right_ok {
                        if hit.map(|(hs, _, _)| s < hs).unwrap_or(true) {
                            hit = Some((s, e, n));
                        }
                        break;
                    }
                }
                probe = s + 1;
            }
        }
        let Some((_start, key_end, _n)) = hit else {
            out.push_str(&line[i..]);
            break;
        };
        // Tolerate an optional closing quote on the key (JSON-shaped: `"api_key":...`).
        let key_close_quote =
            if key_end < bytes.len() && (bytes[key_end] == b'"' || bytes[key_end] == b'\'') {
                Some(bytes[key_end])
            } else {
                None
            };
        let after_key = key_end + key_close_quote.map(|_| 1).unwrap_or(0);
        // Skip whitespace.
        let mut p = after_key;
        while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
            p += 1;
        }
        // Require `=` or `:` (else not a kv assignment — copy through the key).
        if p >= bytes.len() || (bytes[p] != b'=' && bytes[p] != b':') {
            out.push_str(&line[i..key_end]);
            i = key_end;
            continue;
        }
        let sep = p;
        // Skip whitespace after `=`/`:`.
        let mut v = sep + 1;
        while v < bytes.len() && (bytes[v] == b' ' || bytes[v] == b'\t') {
            v += 1;
        }
        // Optional opening quote on the value.
        let mut closing_quote: Option<u8> = None;
        if v < bytes.len() && (bytes[v] == b'"' || bytes[v] == b'\'') {
            closing_quote = Some(bytes[v]);
            v += 1;
        }
        if v >= bytes.len() {
            // EOL right after we consumed an opening quote (e.g. `private_key="`
            // at end of line). Codex Stop-hook round 4 B2: instead of bailing
            // and leaving the key visible, treat this as a zero-length quoted
            // value that ran off the line — emit `<key>=<redacted>` and signal
            // the carry-state so `sanitize_text` redacts subsequent lines until
            // the matching closing quote.
            if let Some(q) = closing_quote {
                out.push_str(&line[i..key_end]);
                if let Some(kq) = key_close_quote {
                    out.push(kq as char);
                }
                out.push('=');
                out.push_str(REDACTED);
                return (out, Some(q));
            }
            out.push_str(&line[i..key_end]);
            i = key_end;
            continue;
        }
        // Find value end. Terminators DEPEND on whether the value was quoted:
        // - Quoted: ONLY the matching closing quote terminates. Whitespace, commas,
        //   braces, etc. are part of the value (e.g. `password="correct horse battery"`
        //   used to leak `horse battery` because the unquoted terminator set was
        //   applied even when `closing_quote = Some('"')`). Codex Stop-hook round 3
        //   finding. A simple `\X` escape advances 2 bytes so `"he said \"hi\""`
        //   doesn't break early on the inner `\"`.
        // - Unquoted: the JSON/shell-friendly terminator set (whitespace / `,` / `;`
        //   / `}` / `]`) — unchanged from round 2.
        let value_start = v;
        let mut value_end = value_start;
        while value_end < bytes.len() {
            let b = bytes[value_end];
            match closing_quote {
                Some(q) => {
                    if b == b'\\' && value_end + 1 < bytes.len() {
                        value_end += 2;
                        continue;
                    }
                    if b == q {
                        break;
                    }
                    value_end += 1;
                }
                None => {
                    if b.is_ascii_whitespace() || b == b',' || b == b';' || b == b'}' || b == b']' {
                        break;
                    }
                    value_end += 1;
                }
            }
        }
        if value_end == value_start {
            out.push_str(&line[i..key_end]);
            i = key_end;
            continue;
        }
        // Normalize the rewrite: keep the key (preserving its closing quote if any)
        // and emit `=<redacted>`. The value's quotes are dropped — only `<redacted>`
        // survives so the original quote style doesn't leak length info.
        out.push_str(&line[i..key_end]);
        if let Some(q) = key_close_quote {
            out.push(q as char);
        }
        out.push('=');
        out.push_str(REDACTED);
        // Did we run off the line still inside a quoted value? If so, signal
        // the carry-state so `sanitize_text` continues redacting next line.
        // Codex Stop-hook round 4.
        let closed = match closing_quote {
            Some(q) => value_end < bytes.len() && bytes[value_end] == q,
            None => true,
        };
        if !closed {
            return (out, closing_quote);
        }
        // Skip past the closing quote on the value, if present.
        i = if let Some(q) = closing_quote {
            if value_end < bytes.len() && bytes[value_end] == q {
                value_end + 1
            } else {
                value_end
            }
        } else {
            value_end
        };
    }
    (out, None)
}

/// Step 4: redact high-entropy standalone tokens. A "token" here is a contiguous
/// run of `[A-Za-z0-9]` of length ≥ 40 that contains BOTH at least one digit AND at
/// least one letter — narrow enough to avoid false-positives on long identifiers
/// (all-letter names) and pure digit runs (timestamps).
///
/// Iterates via `char_indices` so multibyte UTF-8 (em-dashes etc.) are preserved
/// verbatim — a previous byte-by-byte cast corrupted "—" into garbage. Code-path
/// regression tested by `sanitize_preserves_emdash_in_report_header`.
fn redact_high_entropy(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut iter = line.char_indices().peekable();
    while let Some(&(start, ch)) = iter.peek() {
        if !ch.is_ascii_alphanumeric() {
            out.push(ch);
            iter.next();
            continue;
        }
        // Collect the run.
        let mut end = start;
        while let Some(&(idx, c)) = iter.peek() {
            if c.is_ascii_alphanumeric() {
                end = idx + c.len_utf8();
                iter.next();
            } else {
                break;
            }
        }
        let token = &line[start..end];
        let has_digit = token.bytes().any(|b| b.is_ascii_digit());
        let has_alpha = token.bytes().any(|b| b.is_ascii_alphabetic());
        if token.len() >= 40 && has_digit && has_alpha {
            out.push_str(REDACTED_LONG);
        } else {
            out.push_str(token);
        }
    }
    out
}

/// Single-line pass through the full pipeline. Returns the line's sanitized
/// form PLUS any unclosed-quote carry-state from the KV pass (so `sanitize_text`
/// can continue redacting subsequent lines that are still inside a quoted
/// sensitive value). Codex Stop-hook round 4.
fn sanitize_one_line(line: &str) -> (String, Option<u8>) {
    let s = redact_urls(line);
    let s = redact_bearer_basic(&s);
    let (s, carry) = redact_sensitive_kv(&s);
    let s = redact_high_entropy(&s);
    (s, carry)
}

/// Sanitize the whole text. Hand-rolled (no `regex` dep). Pipeline:
///   URL credentials/query → bearer/basic → sensitive key=value → high-entropy.
/// Idempotent: re-running on already-sanitized text yields the same text.
///
/// Cross-line carry-state (Codex Stop-hook round 4): when a line ends inside a
/// quoted sensitive value (e.g. `private_key="-----BEGIN`), the subsequent
/// lines are dropped entirely until the matching closing quote is seen, then
/// the rest of that line is sanitized normally. If the remainder itself opens
/// a new multi-line quoted sensitive value, that carry-state propagates too
/// (B3). The whole multi-line credential becomes one `<redacted>` on its first
/// line, blanks in between, and any post-quote tail sanitized normally.
pub fn sanitize_text(input: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut carry: Option<u8> = None;
    for line in input.lines() {
        let sanitized = if let Some(q) = carry {
            match consume_until_unescaped_byte(line, q) {
                Some(close_idx) => {
                    let remainder = &line[close_idx + 1..];
                    let (rest, new_carry) = sanitize_one_line(remainder);
                    carry = new_carry;
                    rest
                }
                None => String::new(),
            }
        } else {
            let (line_out, new_carry) = sanitize_one_line(line);
            carry = new_carry;
            line_out
        };
        out.push(sanitized);
    }
    let mut result = out.join("\n");
    if input.ends_with('\n') {
        result.push('\n');
    }
    result
}

// ───────────────────────── Debug-tab report assembly ─────────────────────────

fn now_iso() -> String {
    // Lightweight: just a UNIX-ms stamp (no chrono dep) plus the day-of-build for
    // context. The report header is meant to identify a snapshot, not be a calendar.
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("{ms}ms (build {})", crate::BUILD_DATE)
}

fn write_doctor_section(out: &mut String, project: &Path) {
    out.push_str("## Doctor checks\n");
    let report = run(project, false, false);
    for c in &report.checks {
        let tag = match c.status {
            Status::Pass => "ok  ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
        };
        if c.detail.is_empty() {
            out.push_str(&format!("  [{tag}] {}\n", c.name));
        } else {
            out.push_str(&format!("  [{tag}] {} — {}\n", c.name, c.detail));
        }
    }
    out.push('\n');
}

fn write_managed_skills_section(out: &mut String) {
    out.push_str("## Managed skills (plan)\n");
    let body = crate::managed_skills::plan();
    for line in body.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
}

fn write_codex_inventory_section(out: &mut String, project: &Path) {
    out.push_str("## Codex MCP inventory (curated)\n");
    let cwd = project.display().to_string();
    match crate::review_mcp::codex_inventory(&cwd) {
        crate::review_mcp::Inventory::Unavailable(why) => {
            out.push_str(&format!("  unavailable: {why}\n"));
        }
        crate::review_mcp::Inventory::Available(servers) => {
            if servers.is_empty() {
                out.push_str("  (none configured)\n");
            }
            for s in &servers {
                let cmd_basename = Path::new(&s.command)
                    .file_name()
                    .and_then(|x| x.to_str())
                    .unwrap_or(&s.command);
                let cmd_basename = if cmd_basename.is_empty() {
                    "(none)"
                } else {
                    cmd_basename
                };
                let env_keys: Vec<String> = s.env.iter().map(|(k, _)| k.clone()).collect();
                out.push_str(&format!(
                    "  - name={} enabled={} transport={} command={} args_count={} cwd_present={} env_keys={:?}\n",
                    s.name,
                    s.enabled,
                    if s.transport.is_empty() { "?" } else { s.transport.as_str() },
                    cmd_basename,
                    s.args.len(),
                    s.cwd.is_some(),
                    env_keys,
                ));
            }
        }
    }
    out.push('\n');
}

fn write_claude_inventory_section(out: &mut String, project: &Path) {
    out.push_str("## Claude MCP inventory (curated)\n");
    match crate::claude_mcp::inventory(project) {
        crate::claude_mcp::Inventory::Unavailable(why) => {
            out.push_str(&format!("  unavailable: {why}\n"));
        }
        crate::claude_mcp::Inventory::Available { servers, warnings } => {
            for w in &warnings {
                out.push_str(&format!("  ! warning: {w}\n"));
            }
            if servers.is_empty() {
                out.push_str("  (none configured)\n");
            }
            for s in &servers {
                let scope = match &s.scope {
                    crate::claude_mcp::ClaudeScope::User { .. } => "user",
                    crate::claude_mcp::ClaudeScope::Project { .. } => "project",
                };
                let (cmd_basename, args_count, env_keys, cwd_present) = match &s.transport {
                    crate::claude_mcp::Transport::Stdio {
                        command,
                        args,
                        env,
                        cwd_field,
                    } => {
                        let cb = Path::new(command)
                            .file_name()
                            .and_then(|x| x.to_str())
                            .unwrap_or(command);
                        let keys: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
                        (cb.to_string(), args.len(), keys, cwd_field.is_some())
                    }
                    _ => ("(none)".to_string(), 0, Vec::new(), false),
                };
                out.push_str(&format!(
                    "  - name={} scope={} overrides_user={} transport={} command={} args_count={} cwd_present={} env_keys={:?}\n",
                    s.name,
                    scope,
                    s.overrides_user,
                    s.transport.label(),
                    cmd_basename,
                    args_count,
                    cwd_present,
                    env_keys,
                ));
            }
        }
    }
    out.push('\n');
}

fn write_plan_gate_section(out: &mut String, project: &Path) {
    out.push_str("## Plan-gate state (curated)\n");
    let cwd_str = project.display().to_string();
    let marker = match crate::plan_gate::marker_state(&cwd_str) {
        crate::plan_gate::MarkerState::Active => "active",
        crate::plan_gate::MarkerState::Pending => "pending",
        crate::plan_gate::MarkerState::Disabled => "disabled",
    };
    out.push_str(&format!("  marker={marker}\n"));
    // Read state.json shallowly — never emit `approved_plan`, `approved_plan_hash`,
    // `last_findings_hash`, or any other free-form text that could carry secrets.
    let path = project
        .join(".ai-bridge")
        .join("plan-gate")
        .join("state.json");
    if !path.exists() {
        out.push_str("  state_file=absent\n\n");
        return;
    }
    let v: Option<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(v) = v {
        let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
        let epoch = v.get("epoch").and_then(Value::as_u64).unwrap_or(0);
        let approved = v.get("approved").and_then(Value::as_bool).unwrap_or(false);
        let same_findings = v.get("same_findings").and_then(Value::as_u64).unwrap_or(0);
        let revoked_reason = v
            .get("revoked_reason")
            .and_then(Value::as_str)
            .unwrap_or("");
        out.push_str(&format!(
            "  status={status} epoch={epoch} approved={approved} same_findings={same_findings} revoked_reason={}\n",
            if revoked_reason.is_empty() {
                "(none)"
            } else {
                revoked_reason
            }
        ));
    } else {
        out.push_str("  state_file=present_but_unparseable\n");
    }
    out.push('\n');
}

fn write_install_state_section(out: &mut String, project: &Path) {
    out.push_str("## Install state (curated)\n");
    let path = project.join(".ai-bridge").join("install-state.json");
    if !path.exists() {
        out.push_str("  absent (run `aibridge init`)\n\n");
        return;
    }
    let v: Option<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    if let Some(v) = v {
        let version = v.get("version").and_then(Value::as_str).unwrap_or("?");
        let installed_at_ms = v
            .get("installed_at_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let binary_path = v.get("binary_path").and_then(Value::as_str).unwrap_or("?");
        out.push_str(&format!(
            "  version={version} installed_at_ms={installed_at_ms} binary_path={binary_path}\n"
        ));
    } else {
        out.push_str("  present_but_unparseable\n");
    }
    out.push('\n');
}

fn write_audit_section(out: &mut String) {
    out.push_str("## Managed-skills audit (last 10; event/name/ok/ts only)\n");
    let Some(home) = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
    else {
        out.push_str("  (no home dir)\n\n");
        return;
    };
    let path = Path::new(&home)
        .join(".ai-bridge")
        .join("managed-skills.audit.jsonl");
    if !path.exists() {
        out.push_str("  (no audit log yet)\n\n");
        return;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut entries: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = entries.len();
    if entries.len() > 10 {
        entries = entries.split_off(entries.len() - 10);
    }
    out.push_str(&format!("  total_entries={total}\n"));
    for line in entries {
        let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
        let ts = v.get("ts_ms").and_then(Value::as_u64).unwrap_or(0);
        let event = v.get("event").and_then(Value::as_str).unwrap_or("?");
        let name = v.get("name").and_then(Value::as_str).unwrap_or("?");
        let ok = v.get("ok").and_then(Value::as_bool).unwrap_or(false);
        out.push_str(&format!(
            "  - ts_ms={ts} event={event} name={name} ok={ok}\n"
        ));
    }
    out.push('\n');
}

fn write_elicitation_section(out: &mut String, project: &Path) {
    out.push_str("## Last declined elicitation (within 24h)\n");
    let cwd = project.display().to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    match crate::progress::last_declined_elicitation(&cwd)
        .filter(|(ms, _)| now.saturating_sub(*ms) <= DAY_MS)
    {
        Some((ms, summary)) => {
            out.push_str(&format!("  ts_ms={ms} summary={summary}\n"));
        }
        None => out.push_str("  (none)\n"),
    }
    out.push('\n');
}

/// Build the comprehensive `Debug` tab report for `project`. The whole text is fed
/// through [`sanitize_text`] as the LAST step so any free-form strings inside the
/// curated sections (doctor `detail`s, elicitation summaries, etc.) are masked even
/// if they accidentally carry URLs/tokens.
pub fn debug_report(project: &Path) -> String {
    let mut out = String::new();
    out.push_str("AI Bridge — debug report\n");
    out.push_str(&format!("  version: {}\n", crate::VERSION_FULL));
    out.push_str(&format!("  platform: {}\n", platform_name()));
    out.push_str(&format!("  project: {}\n", project.display()));
    out.push_str(&format!("  generated: {}\n", now_iso()));
    out.push_str(
        "  note: this report is auto-sanitized (tokens, URL credentials, env values,\n        \
         plan/audit details masked). Review before sharing publicly.\n\n",
    );
    write_doctor_section(&mut out, project);
    write_managed_skills_section(&mut out);
    write_codex_inventory_section(&mut out, project);
    write_claude_inventory_section(&mut out, project);
    write_cli_versions_section(&mut out);
    write_mcp_pins_section(&mut out, project);
    write_plan_gate_section(&mut out, project);
    write_install_state_section(&mut out, project);
    write_audit_section(&mut out);
    write_elicitation_section(&mut out, project);
    sanitize_text(&out)
}

/// CLI versions (CURRENT-only — no network). Codex Stop-gate R3 bound: Debug must
/// not hit npm/brew/gh; use `aibridge update --check` for latest comparisons.
fn write_cli_versions_section(out: &mut String) {
    use crate::cli_update::{current_version_of, RealCommandRunner};
    use std::time::Duration;
    out.push_str("## CLI versions (curated, current-only)\n");
    let runner = RealCommandRunner;
    let timeout = Duration::from_secs(2);
    for tool in ["codex", "claude", "rtk"] {
        let cur = current_version_of(&runner, tool, timeout)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(not found / unreadable)".to_string());
        out.push_str(&format!("  - {tool}: current={cur}\n"));
    }
    out.push_str("  (latest-version info via `aibridge update --check`)\n\n");
}

/// MCP version-pin status (LOCAL config only — no network).
fn write_mcp_pins_section(out: &mut String, project: &Path) {
    out.push_str("## MCP version pins (curated)\n");
    let pins = crate::cli_update::scan_mcps(project);
    if pins.is_empty() {
        out.push_str("  (no npx-based MCP servers detected)\n\n");
        return;
    }
    for p in &pins {
        let pin = p
            .version_pin
            .as_deref()
            .map(|v| format!("pinned={v}"))
            .unwrap_or_else(|| "unpinned (auto-updates at next launch)".to_string());
        out.push_str(&format!(
            "  - [{agent}] {server} package={pkg} {pin}\n",
            agent = p.agent,
            server = p.server_name,
            pkg = p.package.as_deref().unwrap_or("?"),
        ));
    }
    out.push('\n');
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[test]
    fn sanitize_redacts_bearer_with_space() {
        let s = sanitize_text("Authorization: Bearer abc.def.ghi");
        assert!(s.contains("Bearer <redacted>"), "got: {s}");
        assert!(!s.contains("abc.def.ghi"));
    }

    #[test]
    fn sanitize_redacts_basic_auth_header() {
        let s = sanitize_text("Authorization: Basic dXNlcjpwYXNz");
        assert!(s.contains("Basic <redacted>"), "got: {s}");
    }

    #[test]
    fn sanitize_redacts_key_value_with_spaces_and_quotes() {
        let s = sanitize_text("API_KEY = \"sk-abc-1234567890abc\"");
        assert!(s.contains("API_KEY=<redacted>"), "got: {s}");
        assert!(!s.contains("sk-abc-1234567890abc"));
    }

    #[test]
    fn sanitize_redacts_key_value_no_quotes() {
        let s = sanitize_text("token: abc123def");
        assert!(s.contains("token=<redacted>"), "got: {s}");
    }

    #[test]
    fn sanitize_redacts_url_basic_auth() {
        let s = sanitize_text("see https://user:pw@example.com/x for details");
        assert!(s.contains("https://<redacted>@example.com"), "got: {s}");
        assert!(!s.contains("user:pw"));
    }

    #[test]
    fn sanitize_redacts_url_query_strings() {
        let s = sanitize_text("hit https://api.example.com/x?token=abc&q=1 end");
        assert!(s.contains("?<redacted-query>"), "got: {s}");
        assert!(!s.contains("token=abc"));
    }

    #[test]
    fn sanitize_redacts_high_entropy_long_string() {
        // 64-char alphanumeric blob — meets length+digit+letter rule.
        let blob = "a1b2c3d4e5f6g7h8i9j0a1b2c3d4e5f6g7h8i9j0a1b2c3d4e5f6g7h8i9j0a1b2";
        let s = sanitize_text(&format!("opaque {} appears", blob));
        assert!(s.contains("<redacted-long>"), "got: {s}");
        assert!(!s.contains(blob));
    }

    #[test]
    fn sanitize_does_not_redact_short_words() {
        let input = "Hello world this is fine";
        let s = sanitize_text(input);
        assert_eq!(s, input);
    }

    #[test]
    fn sanitize_redacts_multi_line_quoted_private_key() {
        // Codex Stop-hook round 4: a multi-line quoted secret used to leak
        // lines 2+ because sanitize_text processed each line independently.
        // Now: cross-line carry-state suppresses the body until the closing
        // quote.
        let input = "private_key=\"-----BEGIN PRIVATE KEY-----\nABCDEF1234567890\n-----END PRIVATE KEY-----\"";
        let s = sanitize_text(input);
        assert!(s.contains("<redacted>"), "got: {s}");
        assert!(
            !s.contains("ABCDEF1234567890"),
            "body leaked across line boundary: {s:?}"
        );
        assert!(!s.contains("-----BEGIN PRIVATE KEY-----"), "got: {s:?}");
        assert!(!s.contains("-----END PRIVATE KEY-----"), "got: {s:?}");
    }

    #[test]
    fn sanitize_multi_line_secret_with_trailing_content_keeps_after_quote() {
        // Trailing content AFTER the closing quote on the last line should be
        // sanitized normally (not dropped wholesale and not leaked).
        let input = "private_key=\"-----BEGIN\nabc123def456\n-----END\" username=alice";
        let s = sanitize_text(input);
        assert!(s.contains("<redacted>"), "got: {s}");
        assert!(!s.contains("abc123def456"), "body leaked: {s:?}");
        // The non-sensitive `username=alice` survives since `username` isn't a
        // sensitive key — and it must NOT be eaten by the carry.
        assert!(s.contains("username=alice"), "trailing dropped: {s:?}");
    }

    #[test]
    fn sanitize_multi_line_secret_with_escaped_quote_across_lines() {
        // Escaped quote (`\"`) inside the body must NOT terminate the carry
        // early — the `consume_until_unescaped_byte` helper handles `\X` as a
        // 2-byte sequence.
        let input = "secret=\"alpha\\\"beta\ngamma\\\"delta\nepsilon\"";
        let s = sanitize_text(input);
        assert!(s.contains("<redacted>"), "got: {s}");
        assert!(!s.contains("gamma"), "body leaked: {s:?}");
        assert!(!s.contains("epsilon"), "body leaked: {s:?}");
    }

    #[test]
    fn sanitize_multi_line_quoted_value_with_opening_quote_at_eol_carries() {
        // Codex Stop-hook round 4 B2: opening quote is the LAST byte of line 1
        // (`private_key="\n...`). The v1 code bailed out and left the key
        // visible. Now: emit <redacted> and carry the quote across lines.
        let input = "private_key=\"\nleak-me-1\nleak-me-2\"";
        let s = sanitize_text(input);
        assert!(s.contains("private_key=<redacted>"), "got: {s:?}");
        assert!(!s.contains("leak-me-1"), "line 2 leaked: {s:?}");
        assert!(!s.contains("leak-me-2"), "line 3 leaked: {s:?}");
    }

    #[test]
    fn sanitize_carries_new_open_quote_after_closing_previous_carry() {
        // Codex Stop-hook round 4 B3: after closing one multi-line secret on
        // a continuation line, a NEW multi-line secret may start in the
        // remainder. The carry-state must propagate from the remainder pass
        // forward to the next line.
        let input = "private_key=\"a\nend\" client_secret=\"b\nleak-tail\nmore-leak\"";
        let s = sanitize_text(input);
        assert!(s.contains("private_key=<redacted>"), "got: {s:?}");
        assert!(s.contains("client_secret=<redacted>"), "got: {s:?}");
        assert!(!s.contains("leak-tail"), "second secret leaked: {s:?}");
        assert!(!s.contains("more-leak"), "second secret leaked: {s:?}");
    }

    #[test]
    fn sanitize_redacts_quoted_value_with_spaces() {
        // Codex Stop-hook round 3: a quoted password with internal spaces used to
        // leak the tail because the loop applied the unquoted whitespace
        // terminator even when `closing_quote = Some('"')`. Now: quoted values
        // scan until the matching closing quote.
        let s = sanitize_text(r#"password="correct horse battery""#);
        assert!(s.contains("password=<redacted>"), "got: {s}");
        assert!(!s.contains("horse"), "leaked: {s}");
        assert!(!s.contains("battery"), "leaked: {s}");
    }

    #[test]
    fn sanitize_handles_escaped_quote_inside_quoted_value() {
        // The inner `\"` is an escape — not the closing quote.
        let s = sanitize_text(r#"secret="he said \"hi\" loudly""#);
        assert!(s.contains("secret=<redacted>"), "got: {s}");
        assert!(!s.contains("loudly"), "leaked: {s}");
    }

    #[test]
    fn sanitize_redacts_json_shaped_double_quoted() {
        // Codex Stop-gate F1: JSON-shaped credentials like {"api_key":"sk-abc"} used
        // to slip through because the right-boundary check rejected `"`. The new
        // !is_identifier_byte right boundary accepts the closing key quote.
        let s = sanitize_text(r#"{"api_key":"sk-abc-1234567890"}"#);
        assert!(s.contains("<redacted>"), "got: {s}");
        assert!(!s.contains("sk-abc-1234567890"), "leaked value: {s}");
    }

    #[test]
    fn sanitize_redacts_json_shaped_single_quoted() {
        let s = sanitize_text("'access_token': 'tok-aaaaaaaa'");
        assert!(s.contains("<redacted>"), "got: {s}");
        assert!(!s.contains("tok-aaaaaaaa"), "leaked value: {s}");
    }

    #[test]
    fn sanitize_redacts_json_value_with_brace_terminator() {
        // The `}` terminates the value as a JSON object closer.
        let s = sanitize_text(r#"{"secret":"hush123"}, after"#);
        assert!(s.contains("<redacted>"), "got: {s}");
        assert!(!s.contains("hush123"), "leaked value: {s}");
    }

    #[test]
    fn sanitize_does_not_redact_non_sensitive_with_quote_after_key() {
        // Sanity: a NON-sensitive key name ending with a quote/colon must not be
        // touched. `"keyword"` contains the substring `key` but `key` is at the
        // start (left boundary OK) and the next char is `w` (alphanumeric → right
        // boundary fails), so no match.
        let input = r#"{"keyword":"banana"}"#;
        assert_eq!(sanitize_text(input), input);
    }

    #[test]
    fn sanitize_does_not_redact_when_underscore_extends_identifier() {
        // Sanity for the asymmetric boundary: `api_key_path = secret`. We DO find
        // `api_key` at index 0 with left boundary OK (start of string), but the next
        // byte is `_` which is an identifier byte → right boundary fails. So no
        // truncated rewrite. (The full `api_key_path` isn't in needles, so it
        // remains visible. If the user wants this redacted, they should add it.)
        let input = "api_key_path = secret_path_value";
        assert_eq!(sanitize_text(input), input);
    }

    #[test]
    fn sanitize_redacts_prefixed_env_secret_keys() {
        // Codex Stop-gate regression: `at_word_boundary` previously treated `_` and
        // `-` as part of the same "word", so prefixed env-style names like
        // OPENAI_API_KEY / ANTHROPIC_API_KEY / GITHUB_TOKEN slipped past the matcher
        // and their values reached the copy-pasteable Debug report. The Debug tab is
        // EXPLICITLY meant to be safe to copy-paste after the final sanitizer pass.
        let cases = [
            (
                "OPENAI_API_KEY=sk-foo-bar-very-long-12345",
                "sk-foo-bar-very-long-12345",
            ),
            (
                "ANTHROPIC_API_KEY = \"sk-ant-abc123def456ghi789\"",
                "sk-ant-abc123def456ghi789",
            ),
            (
                "GITHUB_TOKEN: ghp_aaaabbbbccccddddeeeeffff",
                "ghp_aaaabbbbccccddddeeeeffff",
            ),
            ("MY-SERVICE-SECRET = topsecret123", "topsecret123"),
        ];
        for (input, value) in cases {
            let s = sanitize_text(input);
            assert!(
                !s.contains(value),
                "leaked value {value:?} in sanitized output {s:?}"
            );
            assert!(s.contains("<redacted>"), "expected <redacted>, got {s:?}");
        }
    }

    #[test]
    fn sanitize_preserves_emdash_in_report_header() {
        // Regression: previous byte-by-byte loop in `redact_high_entropy` corrupted
        // multibyte chars like `—` (3 UTF-8 bytes). The fix iterates by char_indices.
        let s = sanitize_text("AI Bridge — debug report");
        assert_eq!(s, "AI Bridge — debug report");
    }

    #[test]
    fn sanitize_is_idempotent() {
        let input = "Bearer abc API_KEY=secret https://u:p@h/x?q=v";
        let once = sanitize_text(input);
        let twice = sanitize_text(&once);
        assert_eq!(once, twice);
    }
}

#[cfg(test)]
mod debug_report_tests {
    use super::*;

    fn temp_project(label: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "aibridge-doctor-debug-{}-{}-{}",
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

    fn write_file(p: &Path, body: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn debug_report_excludes_approved_plan() {
        // A state.json with an `approved_plan` literal must NOT appear in the report;
        // the curated section only emits status/epoch/approved/same_findings/reason.
        let project = temp_project("noplan");
        let state_path = project
            .join(".ai-bridge")
            .join("plan-gate")
            .join("state.json");
        write_file(
            &state_path,
            r#"{"status":"approved","epoch":3,"approved":true,"approved_plan":"SECRET-PLAN-BODY","same_findings":0}"#,
        );
        let report = debug_report(&project);
        assert!(
            !report.contains("SECRET-PLAN-BODY"),
            "report leaked approved_plan: {report}"
        );
        assert!(report.contains("status=approved"));
        assert!(report.contains("epoch=3"));
    }

    #[test]
    fn debug_report_excludes_audit_detail() {
        // We can't easily inject ~/.ai-bridge/managed-skills.audit.jsonl in tests
        // without env-var hacks; this test instead asserts the CODE-LEVEL invariant
        // by inspecting the audit section's source.
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("doctor.rs");
        let src = std::fs::read_to_string(&path).expect("read own source");
        // Find the audit-section function body and make sure it never emits `detail`.
        let body = src
            .split("fn write_audit_section")
            .nth(1)
            .and_then(|s| s.split("fn ").next())
            .expect("audit section present");
        assert!(
            !body.contains("\"detail\""),
            "write_audit_section must not read the `detail` field"
        );
        assert!(
            !body.contains(".get(\"detail\")"),
            "write_audit_section must not read the `detail` field"
        );
    }

    #[test]
    fn debug_report_emits_env_keys_only() {
        // Build a synthetic curated line the same way the inventory writer does and
        // confirm env VALUES never appear, KEYS do.
        let mut out = String::new();
        let env_keys = vec!["MY_KEY".to_string(), "API_TOKEN".to_string()];
        out.push_str(&format!("env_keys={:?}\n", env_keys));
        let sanitized = sanitize_text(&out);
        assert!(sanitized.contains("MY_KEY"));
        assert!(sanitized.contains("API_TOKEN"));
        // No literal values reachable in the curated emit.
        assert!(!sanitized.contains("supersecret"));
    }

    #[test]
    fn debug_report_sanitizes_doctor_check_detail() {
        // Drive sanitize_text directly with a doctor-like detail string that contains
        // a basic-auth URL — confirm it's masked end-to-end.
        let detail = "  [ ok ] something — see https://user:pw@example.com/x?token=abc";
        let out = sanitize_text(detail);
        assert!(out.contains("https://<redacted>@example.com"), "got: {out}");
        assert!(out.contains("?<redacted-query>"));
    }

    #[test]
    fn debug_report_cli_section_excludes_latest_lookup() {
        // The ## CLI versions section is CURRENT-only — no network. Confirm the
        // section is present AND that it explicitly points at `aibridge update
        // --check` for latest info (rather than embedding latest itself).
        let project = temp_project("cli-no-latest");
        let report = debug_report(&project);
        assert!(
            report.contains("## CLI versions"),
            "section missing: {report}"
        );
        assert!(
            report.contains("aibridge update --check"),
            "report should point at --check for latest info: {report}"
        );
        // The string "latest=" must NOT appear in the CLI versions section
        // (it does appear in the curated MCP/Claude sections under different keys).
        let cli_section_start = report.find("## CLI versions").unwrap();
        let after_cli_section = report[cli_section_start..]
            .find("##")
            .map(|i| cli_section_start + i + 2)
            .unwrap_or(report.len());
        let next_section = report[after_cli_section..]
            .find("##")
            .map(|i| after_cli_section + i)
            .unwrap_or(report.len());
        let cli_block = &report[cli_section_start..next_section];
        assert!(
            !cli_block.contains("latest="),
            "CLI section should NOT embed latest=...; got: {cli_block}"
        );
    }

    #[test]
    fn debug_report_cli_section_is_sanitized() {
        // Smoke: the full report (which includes ## CLI versions + ## MCP version
        // pins) passes through sanitize_text without leaking the obvious-secret
        // strings we plant in env. We can't inject `<tool> --version` output, but
        // we CAN confirm the section structure survives sanitization (em-dash,
        // headers, parens) and contains only word-y characters.
        let project = temp_project("cli-sanitize");
        let report = debug_report(&project);
        assert!(report.contains("## CLI versions"));
        assert!(report.contains("## MCP version pins"));
        // Sanitizer must NOT corrupt the section header markup.
        assert!(report.contains("(curated, current-only)"));
    }

    #[test]
    fn debug_report_header_fields_present() {
        // Smoke: the header section has the four expected lines.
        let project = temp_project("header");
        let report = debug_report(&project);
        assert!(report.starts_with("AI Bridge — debug report"));
        assert!(report.contains("version:"));
        assert!(report.contains("platform:"));
        assert!(report.contains("project:"));
        assert!(report.contains("generated:"));
    }
}

#[cfg(test)]
mod tests {
    use super::parse_reasoning_effort;

    #[test]
    fn parses_quoted_and_unquoted() {
        assert_eq!(
            parse_reasoning_effort("model_reasoning_effort = \"xhigh\"").as_deref(),
            Some("xhigh")
        );
        assert_eq!(
            parse_reasoning_effort("model_reasoning_effort=\"medium\"").as_deref(),
            Some("medium")
        );
        assert_eq!(
            parse_reasoning_effort("model = \"gpt-5.5\"\nmodel_reasoning_effort = \"high\"\n")
                .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn ignores_lookalike_keys_and_comments() {
        assert_eq!(
            parse_reasoning_effort("model_reasoning_effort_extra = \"x\""),
            None
        );
        assert_eq!(
            parse_reasoning_effort("# model_reasoning_effort = \"high\""),
            None
        );
        assert_eq!(parse_reasoning_effort("model = \"gpt-5.5\""), None);
    }
}
