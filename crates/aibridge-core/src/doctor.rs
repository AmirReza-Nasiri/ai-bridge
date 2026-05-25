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
