//! AI Bridge — warm peer-review orchestrator for AI coding CLIs.
//!
//! v4 successor to `codex-peer`. Design + usage: see `README.md`.
//!
//! CLI entry point: `mcp-server` (the warm engine Claude connects to), `init`
//! (wire a project — both gates by default), `doctor`/`selftest`, `update`, the
//! internal `hook` entry points, and a reserved `profile` command.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod tui;

#[derive(Parser, Debug)]
#[command(
    name = "aibridge",
    version = aibridge_core::VERSION_FULL,
    about = "AI Bridge — warm peer-review orchestrator for AI CLIs"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the AI Bridge MCP server (the warm peer engine).
    McpServer,
    /// Wire the MCP server + both review gates (plan + Stop) into the project (local).
    Init {
        /// Also wire the rtk output-optimizer PreToolUse hook (safe mode).
        #[arg(long)]
        rtk: bool,
        /// Do NOT wire the pre-execution plan gate. By default the plan gate is ON
        /// (symmetric with the Stop gate): before any write/Bash in a task, Codex
        /// must approve the plan. A quick per-session bypass is `AIBRIDGE_PLAN_GATE=0`.
        #[arg(long = "no-plan-gate")]
        no_plan_gate: bool,
    },
    /// Translate `ai-bridge.profile.toml` into native per-CLI config.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Verify the installation works on this platform.
    Selftest {
        /// Run the full end-to-end suite (calls Claude/Codex; uses quota).
        #[arg(long)]
        full: bool,
    },
    /// Diagnose the installation. (Hidden — it's the `status` dashboard's Health tab;
    /// still works for scripts/CI.)
    #[command(hide = true)]
    Doctor {
        /// Also check GitHub for a newer release (network; off by default).
        #[arg(long = "check-updates")]
        check_updates: bool,
    },
    /// Open the interactive dashboard — Health + live Review + Codex-MCP (per-server &
    /// per-tool) + Update — in one terminal screen. `--watch` follows the review as plain
    /// text instead; `--plain` prints one text line. Falls back to text without a TTY.
    Status {
        /// Follow the review live as plain text, refreshing each second until it finishes.
        #[arg(long)]
        watch: bool,
        /// Print a one-shot plain-text status line (no dashboard).
        #[arg(long)]
        plain: bool,
    },
    /// Check for / install a newer release. (Hidden — it's the `status` dashboard's
    /// Update tab; still works for scripts.)
    #[command(hide = true)]
    Update {
        /// Only check + report; don't change anything.
        #[arg(long)]
        check: bool,
        /// Apply without the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Build from source instead of downloading a release (not implemented yet).
        #[arg(long = "from-source")]
        from_source: bool,
        /// Replace this binary path instead of the auto-resolved one (advanced).
        #[arg(long)]
        target: Option<String>,
        /// Skip the AI Bridge self-update step; only check/update the dependent CLIs
        /// (codex, claude, rtk). Useful for periodic CLI maintenance.
        #[arg(long = "cli-only")]
        cli_only: bool,
    },
    /// Internal hook entry points (invoked by Claude Code hooks, not by you).
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    /// Control which codex MCP servers/tools run during reviews. (Hidden — it's the
    /// `status` dashboard's Codex MCP tab; still works for scripts.)
    #[command(hide = true)]
    ReviewMcp {
        #[command(subcommand)]
        action: ReviewMcpAction,
    },
    /// (Alias of `status`.) Open the interactive dashboard.
    #[command(hide = true)]
    Tui,
    /// Manage Agent Skills shared by Claude Code AND Codex (incl. the Bridge's reviews):
    /// inspect, and mirror the `~/.claude/skills` hub into the cross-agent `~/.agents/skills`.
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
}

#[derive(Subcommand, Debug)]
enum SkillsAction {
    /// Read-only: show the skill roots, flag invalid skills, and report Claude↔codex sync.
    Doctor,
    /// Mirror new `~/.claude/skills` into `~/.agents/skills` so codex/Bridge reviews see them.
    Sync {
        /// Actually copy (default is a dry run that only reports what would change).
        #[arg(long)]
        apply: bool,
    },
    /// Fold legacy `~/.codex/skills` into the `~/.claude/skills` hub (then `sync`).
    Migrate {
        /// Actually copy (default is a dry run).
        #[arg(long)]
        apply: bool,
    },
    /// Bridge-managed skills: declare a pinned set in a manifest, fetch them once, and
    /// mirror into BOTH Claude + Codex skill dirs (shareable across machines).
    Managed {
        #[command(subcommand)]
        action: ManagedAction,
    },
}

#[derive(Subcommand, Debug)]
enum ManagedAction {
    /// Write a starter manifest (`~/.ai-bridge/skills-managed.toml`) — nothing installs.
    Init,
    /// OFFLINE: show each managed skill's status (manifest vs lock vs filesystem).
    Plan,
    /// Fetch + install/update managed skills (the only command that touches the network).
    Apply {
        /// Apply one skill by name (default: all enabled skills).
        name: Option<String>,
        /// Re-mirror a skill whose mirror was hand-edited (discards those edits).
        #[arg(long)]
        repair: bool,
        /// Take over a same-named foreign skill folder whose content is byte-identical.
        #[arg(long)]
        adopt: bool,
    },
    /// OFFLINE: same as `plan` (alias kept for muscle memory).
    Doctor,
    /// Remove a managed skill's mirrors from both CLI dirs (keeps the source + lock).
    Disable {
        /// The managed skill name.
        name: String,
    },
    /// Fully remove a managed skill (mirrors if owned + source folder + lock entry).
    Remove {
        /// The managed skill name.
        name: String,
    },
    /// Quarantine a foreign same-named folder (move to ~/.ai-bridge/backups/) then install
    /// the managed skill. The foreign content is preserved (reversible). v0.17.0.
    MigrateAndInstall {
        /// The managed skill name.
        name: String,
    },
    /// Bring an existing personal skill (in ~/.claude/skills/<name>) under managed control:
    /// copy it into ~/.ai-bridge/imports/<name>/, append a manifest entry, and adopt. v0.17.0.
    Register {
        /// The personal skill name (must exist in ~/.claude/skills/).
        name: String,
    },
    /// Probe upstream (`git ls-remote <repo> <update_ref>`) for every enabled+tracked git
    /// skill and print which have a newer commit available. v0.17.0.
    CheckUpstream,
    /// Stage an upstream candidate, write the new SHA into the manifest's `ref`, and apply.
    /// The skill must have `update_ref` set in the manifest. v0.17.0.
    Bump {
        /// The managed skill name.
        name: String,
        /// The new full 40-hex commit SHA (from `check-upstream`).
        new_sha: String,
    },
}

#[derive(Subcommand, Debug)]
enum HookAction {
    /// PreToolUse: plan-gate enforcement (deny writes until approved) + rtk rewrite.
    Pretooluse,
    /// UserPromptSubmit: start a fresh plan-gate task epoch for the new prompt.
    UserPromptSubmit,
}

#[derive(Subcommand, Debug)]
enum ReviewMcpAction {
    /// List codex MCP servers and whether each is enabled during AI Bridge reviews.
    List,
    /// Enable a codex MCP server during AI Bridge reviews (name from `list`).
    Enable {
        /// The codex MCP server name.
        name: String,
    },
    /// Disable a codex MCP server during AI Bridge reviews (name from `list`).
    Disable {
        /// The codex MCP server name.
        name: String,
    },
    /// Enable ALL codex MCP servers during reviews (⚠ browser/scrape ones can stall).
    All,
    /// Disable ALL codex MCP servers during reviews (pure reasoning — the default).
    None,
    /// Discover + list a codex MCP server's TOOLS (briefly launches it for tools/list).
    Tools {
        /// The codex MCP server name.
        server: String,
    },
    /// Turn ONE tool of a codex MCP server on/off during reviews (mode "some").
    Tool {
        /// The codex MCP server name.
        server: String,
        /// The tool name (see `review-mcp tools <server>`).
        tool: String,
        /// `on` or `off`.
        state: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProfileAction {
    /// Apply the profile to native CLI config.
    Apply {
        /// Show what would change without writing.
        #[arg(long)]
        dry_run: bool,
        /// Repair drift between the profile and native config.
        #[arg(long)]
        fix: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::McpServer => aibridge_core::mcp::serve(),
        Commands::Init { rtk, no_plan_gate } => init(rtk, !no_plan_gate),
        Commands::Profile { action } => match action {
            ProfileAction::Apply { dry_run, fix } => {
                not_yet(&format!("profile apply (dry_run={dry_run}, fix={fix})"))
            }
        },
        Commands::Selftest { full } => doctor_cmd(full, false),
        Commands::Doctor { check_updates } => doctor_cmd(false, check_updates),
        Commands::Status { watch, plain } => status_cmd(watch, plain),
        Commands::Update {
            check,
            yes,
            from_source,
            target,
            cli_only,
        } => update_cmd(check, yes, from_source, target, cli_only),
        Commands::Hook { action } => match action {
            HookAction::Pretooluse => hook_pretooluse(),
            HookAction::UserPromptSubmit => hook_user_prompt_submit(),
        },
        Commands::ReviewMcp { action } => review_mcp_cmd(action),
        Commands::Tui => tui::run(),
        Commands::Skills { action } => {
            match action {
                SkillsAction::Doctor => println!("{}", aibridge_core::skills::doctor()),
                SkillsAction::Sync { apply } => println!("{}", aibridge_core::skills::sync(apply)),
                SkillsAction::Migrate { apply } => {
                    println!("{}", aibridge_core::skills::migrate(apply))
                }
                SkillsAction::Managed { action } => {
                    use aibridge_core::managed_skills as ms;
                    // init/plan/doctor are read-only (always ok); apply/disable/remove report
                    // ok=false on a partial → the CLI exits non-zero (for scripts/CI).
                    let res = match action {
                        ManagedAction::Init => ms::OpResult {
                            message: ms::init(),
                            ok: true,
                        },
                        ManagedAction::Plan => ms::OpResult {
                            message: ms::plan(),
                            ok: true,
                        },
                        ManagedAction::Doctor => ms::OpResult {
                            message: ms::doctor(),
                            ok: true,
                        },
                        ManagedAction::Apply {
                            name,
                            repair,
                            adopt,
                        } => ms::apply(
                            match name {
                                Some(n) => ms::Target::One(n),
                                None => ms::Target::All,
                            },
                            repair,
                            adopt,
                        ),
                        ManagedAction::Disable { name } => ms::disable(&name),
                        ManagedAction::Remove { name } => ms::remove(&name),
                        ManagedAction::MigrateAndInstall { name } => ms::migrate_and_install(&name),
                        ManagedAction::Register { name } => ms::register_personal(&name),
                        ManagedAction::CheckUpstream => {
                            let cands = ms::check_upstream();
                            let mut s = String::from("AI Bridge managed skills — check-upstream\n");
                            if cands.is_empty() {
                                s.push_str(
                                    "  no skills are tracking upstream (set `update_ref` in the manifest to enable).\n",
                                );
                            } else {
                                for c in &cands {
                                    let cur8 = &c.current_sha[..c.current_sha.len().min(8)];
                                    let line = match &c.upstream_sha {
                                        Some(up) if c.update_available() => format!(
                                            "  ↑ {:<22} {:<8} → {:<8}  (probed {})\n",
                                            c.name,
                                            cur8,
                                            &up[..up.len().min(8)],
                                            c.probed_ref
                                        ),
                                        Some(_) => format!(
                                            "    {:<22} {:<8}  up to date  (probed {})\n",
                                            c.name, cur8, c.probed_ref
                                        ),
                                        None => format!(
                                            "  ! {:<22} {:<8}  upstream probe FAILED  (probed {})\n",
                                            c.name, cur8, c.probed_ref
                                        ),
                                    };
                                    s.push_str(&line);
                                }
                            }
                            ms::OpResult {
                                message: s,
                                ok: true,
                            }
                        }
                        ManagedAction::Bump { name, new_sha } => {
                            match ms::bump_prepare(&name, &new_sha) {
                                Ok(preview) => ms::bump_commit(preview),
                                Err(e) => ms::OpResult {
                                    message: format!("AI Bridge managed skills: {e}"),
                                    ok: false,
                                },
                            }
                        }
                    };
                    println!("{}", res.message);
                    if !res.ok {
                        std::process::exit(1);
                    }
                }
            }
            Ok(())
        }
    }
}

fn review_mcp_cmd(action: ReviewMcpAction) -> Result<()> {
    use aibridge_core::review_mcp;
    match action {
        ReviewMcpAction::List => println!("{}", review_mcp::list_report()),
        ReviewMcpAction::Enable { name } => match review_mcp::enable(&name) {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        ReviewMcpAction::Disable { name } => println!("{}", review_mcp::disable(&name)),
        ReviewMcpAction::All => println!("{}", review_mcp::set_all(true)),
        ReviewMcpAction::None => println!("{}", review_mcp::set_all(false)),
        ReviewMcpAction::Tools { server } => match review_mcp::tools_report(&server) {
            Ok(m) => println!("{m}"),
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },
        ReviewMcpAction::Tool {
            server,
            tool,
            state,
        } => {
            let on = match state.to_ascii_lowercase().as_str() {
                "on" | "true" | "enable" => true,
                "off" | "false" | "disable" => false,
                _ => {
                    eprintln!("state must be 'on' or 'off'");
                    std::process::exit(2);
                }
            };
            match review_mcp::set_tool(&server, &tool, on) {
                Ok(m) => println!("{m}"),
                Err(e) => {
                    eprintln!("AI Bridge review-mcp: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
    Ok(())
}

fn status_cmd(watch: bool, plain: bool) -> Result<()> {
    use std::io::IsTerminal;
    // Default (interactive terminal, no flags) → the dashboard. `--watch`/`--plain`, or
    // no TTY (a pipe/script), → plain text so nothing hangs and logging still works.
    if !watch
        && !plain
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && !std::env::var("TERM")
            .map(|t| t.eq_ignore_ascii_case("dumb"))
            .unwrap_or(false)
    {
        return tui::run();
    }
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    loop {
        match aibridge_core::progress::status_report(&cwd) {
            Some(line) => println!("AI Bridge: {line}"),
            None => println!("AI Bridge: no review status yet for this project."),
        }
        if !watch {
            break;
        }
        // Stop following once the review is no longer active (one final line above).
        let active = aibridge_core::progress::read_status(&cwd)
            .and_then(|s| s.get("active").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        if !active {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    Ok(())
}

fn update_cmd(
    check_only: bool,
    yes: bool,
    from_source: bool,
    target: Option<String>,
    cli_only: bool,
) -> Result<()> {
    use aibridge_core::cli_update::{
        apply_cli_update, check_all, decide_action, effective_mode, prompt_parse, scan_mcps,
        Action, RealCommandRunner,
    };
    use std::io::{BufRead, IsTerminal, Write};
    use std::time::Duration;
    println!("AI Bridge {}\n", aibridge_core::VERSION_FULL);

    // Step 1: aibridge self-update (skipped under --cli-only).
    if !cli_only {
        if check_only {
            println!(
                "{}",
                aibridge_core::update::check_report(Duration::from_secs(15))
            );
        } else {
            match aibridge_core::update::apply_update(aibridge_core::update::ApplyOptions {
                assume_yes: yes,
                from_source,
                target_path: target,
            }) {
                Ok(msg) => println!("{msg}"),
                Err(msg) => {
                    eprintln!("AI Bridge update: {msg}");
                    std::process::exit(1);
                }
            }
        }
        println!();
    }

    // Step 2: dependent-CLI checks.
    let mode = effective_mode(check_only, yes, std::io::stdin().is_terminal());
    println!("CLI updates (mode: {mode:?})");
    let runner = RealCommandRunner;
    let checks = check_all(&runner);
    let mut pending_runs: Vec<(String /* tool */, Vec<String> /* argv */)> = Vec::new();
    let mut prompts: Vec<(String, Vec<String>, String)> = Vec::new();

    for c in &checks {
        let action = decide_action(c, mode);
        match action {
            Action::Skip { reason } => println!("  [{tool}] {reason}", tool = c.tool),
            Action::Prompt { argv, summary } => {
                prompts.push((c.tool.to_string(), argv, summary));
            }
            Action::Run { argv, summary } => {
                println!("  [{tool}] {summary}", tool = c.tool);
                pending_runs.push((c.tool.to_string(), argv));
            }
        }
    }

    // Step 3: process interactive prompts (TTY only — non-TTY became Check via
    // effective_mode and never gets here).
    for (tool, argv, summary) in prompts {
        print!(
            "  [{tool}] {summary}  Run `{cmd}` ? [y/N] ",
            cmd = argv.join(" ")
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        if prompt_parse(&line) {
            pending_runs.push((tool, argv));
        } else {
            println!("    declined.");
        }
    }

    // Step 4: actually run the accepted commands. Each uses inherited stdio so
    // brew/npm progress + prompts appear in real time.
    let mut any_failed = false;
    for (tool, argv) in pending_runs {
        println!("\n  [{tool}] running: {}", argv.join(" "));
        match apply_cli_update(&argv) {
            Ok(0) => println!("  [{tool}] success."),
            Ok(code) => {
                eprintln!("  [{tool}] exited with code {code}");
                any_failed = true;
            }
            Err(e) => {
                eprintln!("  [{tool}] failed to spawn: {e}");
                any_failed = true;
            }
        }
    }

    // Step 5: MCP pins summary (read-only — never prompts).
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let pins = scan_mcps(&cwd);
    if !pins.is_empty() {
        println!("\nMCP version pins:");
        for p in &pins {
            let pin = p
                .version_pin
                .as_deref()
                .map(|v| format!("pinned={v}"))
                .unwrap_or_else(|| "unpinned (auto-updates at next launch)".to_string());
            println!(
                "  - [{agent}] {server} package={pkg} {pin}",
                agent = p.agent,
                server = p.server_name,
                pkg = p.package.as_deref().unwrap_or("?"),
            );
        }
    }

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

fn hook_pretooluse() -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    print!("{}", aibridge_core::optimizer::pretooluse_str(&input));
    Ok(())
}

fn hook_user_prompt_submit() -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    // Start a fresh plan-gate epoch AND record the Stop-gate review base for this
    // prompt; never block the prompt. (The review base is recorded even when the
    // plan gate is off, so the Stop gate can review committed-since-task work.)
    aibridge_core::plan_gate::on_user_prompt(&input);
    aibridge_core::review_frontier::on_user_prompt(&input);
    print!("{{}}");
    Ok(())
}

fn doctor_cmd(full: bool, check_updates: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let report = aibridge_core::doctor::run(&cwd, full, check_updates);
    report.print();
    if !report.ok() {
        std::process::exit(1);
    }
    Ok(())
}

fn not_yet(what: &str) -> Result<()> {
    println!(
        "AI Bridge v{} on {} — `{}` is not implemented yet.",
        aibridge_core::version(),
        aibridge_platform::platform_name(),
        what
    );
    Ok(())
}

fn init(rtk: bool, plan_gate: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let report = aibridge_core::install::init(&cwd, rtk, plan_gate)?;
    println!("AI Bridge: wired into {}", cwd.display());
    for action in &report.actions {
        println!("  • {action}");
    }
    if report.restart_required {
        println!(
            "\nRESTART_REQUIRED: restart Claude Code so it connects the aibridge MCP server \
             and loads the hooks."
        );
    }
    if plan_gate {
        println!(
            "Then work normally — BOTH gates are automatic: the plan gate (Codex approves the \
             plan before any write/Bash) and the Stop gate (Codex reviews the result). Quick \
             bypass for a trivial task: set AIBRIDGE_PLAN_GATE=0."
        );
    } else {
        println!(
            "Then work normally — the automatic Stop peer-review gate is active (plan gate off)."
        );
    }
    Ok(())
}
