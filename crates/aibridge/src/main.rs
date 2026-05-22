//! AI Bridge — warm peer-review orchestrator for AI coding CLIs.
//!
//! v4 successor to `codex-peer`. Design: `docs/architecture/AI-BRIDGE-REDESIGN-FA.md`.
//!
//! This is the Phase-0 foundation: the CLI surface is wired as stubs so the
//! shape is real and `--version` works. Functionality lands in later phases.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "aibridge",
    version,
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
    /// Wire the MCP server + Stop review hook into the project (local).
    Init {
        /// Also wire the rtk output-optimizer PreToolUse hook (safe mode).
        #[arg(long)]
        rtk: bool,
        /// Also wire the OPT-IN pre-execution plan gate: before any write/Bash in a
        /// task, Codex must approve the plan (UserPromptSubmit + PreToolUse hooks).
        #[arg(long = "plan-gate")]
        plan_gate: bool,
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
    Doctor,
    /// Internal hook entry points (invoked by Claude Code hooks, not by you).
    Hook {
        #[command(subcommand)]
        action: HookAction,
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
        Commands::Init { rtk, plan_gate } => init(rtk, plan_gate),
        Commands::Profile { action } => match action {
            ProfileAction::Apply { dry_run, fix } => {
                not_yet(&format!("profile apply (dry_run={dry_run}, fix={fix})"))
            }
        },
        Commands::Selftest { full } => doctor_cmd(full),
        Commands::Doctor => doctor_cmd(false),
        Commands::Hook { action } => match action {
            HookAction::Pretooluse => hook_pretooluse(),
            HookAction::UserPromptSubmit => hook_user_prompt_submit(),
        },
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
    // Start a fresh plan-gate epoch for this prompt; never block the prompt.
    aibridge_core::plan_gate::on_user_prompt(&input);
    print!("{{}}");
    Ok(())
}

fn doctor_cmd(full: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let report = aibridge_core::doctor::run(&cwd, full);
    report.print();
    if !report.ok() {
        std::process::exit(1);
    }
    Ok(())
}

fn not_yet(what: &str) -> Result<()> {
    println!(
        "AI Bridge v{} (foundation) on {} — `{}` is not implemented yet.",
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
             and loads the Stop hook."
        );
    }
    println!("Then work normally — the automatic peer-review gate is active.");
    Ok(())
}
