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
    /// Diagnose (and optionally repair) the installation.
    Doctor {
        /// Also check GitHub for a newer release (network; off by default).
        #[arg(long = "check-updates")]
        check_updates: bool,
    },
    /// Show the live status of an in-progress Codex review (--watch to follow it).
    Status {
        /// Follow the review live, refreshing each second until it finishes.
        #[arg(long)]
        watch: bool,
    },
    /// Check for and install a newer AI Bridge release.
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
    },
    /// Internal hook entry points (invoked by Claude Code hooks, not by you).
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
    /// Control which of codex's own MCP servers stay enabled during AI Bridge
    /// reviews (default: NONE — reviews run tool-free so a browser/scrape server
    /// can't stall them). Reload the window to apply a change to a running review.
    ReviewMcp {
        #[command(subcommand)]
        action: ReviewMcpAction,
    },
    /// Open the interactive dashboard — Health (doctor) + live Review status + Codex-MCP
    /// toggles — in one terminal screen. Needs an interactive terminal.
    Tui,
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
        Commands::Status { watch } => status_cmd(watch),
        Commands::Update {
            check,
            yes,
            from_source,
            target,
        } => update_cmd(check, yes, from_source, target),
        Commands::Hook { action } => match action {
            HookAction::Pretooluse => hook_pretooluse(),
            HookAction::UserPromptSubmit => hook_user_prompt_submit(),
        },
        Commands::ReviewMcp { action } => review_mcp_cmd(action),
        Commands::Tui => tui::run(),
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

fn status_cmd(watch: bool) -> Result<()> {
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
) -> Result<()> {
    use std::time::Duration;
    println!("AI Bridge {}\n", aibridge_core::VERSION_FULL);
    if check_only {
        println!(
            "{}",
            aibridge_core::update::check_report(Duration::from_secs(15))
        );
        return Ok(());
    }
    match aibridge_core::update::apply_update(aibridge_core::update::ApplyOptions {
        assume_yes: yes,
        from_source,
        target_path: target,
    }) {
        Ok(msg) => {
            println!("{msg}");
            Ok(())
        }
        Err(msg) => {
            eprintln!("AI Bridge update: {msg}");
            std::process::exit(1);
        }
    }
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
