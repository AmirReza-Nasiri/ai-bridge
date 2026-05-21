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
    /// Wire hooks + MCP config + rtk into the project/user config.
    Init,
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
        Commands::Init => init(),
        Commands::Profile { action } => match action {
            ProfileAction::Apply { dry_run, fix } => {
                not_yet(&format!("profile apply (dry_run={dry_run}, fix={fix})"))
            }
        },
        Commands::Selftest { full } => doctor_cmd(full),
        Commands::Doctor => doctor_cmd(false),
    }
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

fn init() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let report = aibridge_core::install::init(&cwd)?;
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
