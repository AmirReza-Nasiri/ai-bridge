//! Platform abstraction layer.
//!
//! Unix-specific code lives in `unix.rs` (macOS maintainer owns it).
//! Windows-specific code lives in `windows.rs` (Windows maintainer owns it).
//! The `Platform` trait below is shared and needs cross-platform review.

use anyhow::Result;
use std::path::PathBuf;
use std::process::Command;

/// Platform-specific operations needed by AI Bridge.
pub trait Platform {
    /// Locate an executable (CLI) by name, with platform-specific fallbacks.
    fn find_executable(name: &str) -> Result<PathBuf>;

    /// The AI Bridge config/state directory (`~/.aibridge`).
    fn config_dir() -> Result<PathBuf>;

    /// Build a [`Command`] for a SHORT, output-captured call (`--version`,
    /// `rtk rewrite`). Wraps `.cmd`/`.bat` shims through `cmd /C` where the OS
    /// requires it (Windows); never goes through a shell on Unix (avoids the
    /// BatBadBut argument-escaping class). Suppresses a console window on Windows.
    fn command_for(exe: &std::path::Path) -> Command;

    /// Plan how to launch `exe` as a LONG-LIVED child with piped stdio (the warm
    /// `codex mcp-server`). Unlike [`Platform::command_for`], this resolves an npm
    /// `.cmd` shim to a direct `node <entry>.js` launch: spawning the batch shim
    /// via `cmd /C` from a parent with no console (the Claude Code MCP host on
    /// Windows) can wedge the child forever. The returned [`SpawnPlan`] also
    /// reports how it resolved, for diagnostics and `doctor`.
    fn spawn_plan(exe: &std::path::Path) -> SpawnPlan;
}

/// How a long-lived child will actually be launched. Recorded for diagnostics
/// and used by `doctor` to flag the degraded `cmd /C` shim path that can hang a
/// no-console parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnKind {
    /// A native executable launched directly.
    Direct,
    /// An npm `.cmd` shim resolved to `node <entry>.js` and launched directly.
    NodeDirect,
    /// Degraded fallback: `cmd /C <shim>.cmd` — can hang from a no-console parent.
    CmdShim,
}

impl SpawnKind {
    /// Stable lowercase label for logs and snapshots.
    pub fn as_str(self) -> &'static str {
        match self {
            SpawnKind::Direct => "direct",
            SpawnKind::NodeDirect => "node-direct",
            SpawnKind::CmdShim => "cmd-shim",
        }
    }

    /// True for the launch modes that are safe from a no-console parent.
    pub fn is_safe(self) -> bool {
        !matches!(self, SpawnKind::CmdShim)
    }
}

/// A ready-to-spawn [`Command`] plus how it was resolved.
pub struct SpawnPlan {
    command: Command,
    /// How `exe` was resolved into a launchable command.
    pub kind: SpawnKind,
    /// Human-readable launch line (resolved program + args), for diagnostics.
    pub program: String,
}

impl SpawnPlan {
    /// Build a plan from an already-configured command.
    pub fn new(command: Command, kind: SpawnKind, program: String) -> Self {
        SpawnPlan {
            command,
            kind,
            program,
        }
    }

    /// Consume the plan, yielding the configured [`Command`] to spawn.
    pub fn into_command(self) -> Command {
        self.command
    }
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::UnixPlatform as DefaultPlatform;
#[cfg(windows)]
pub use windows::WindowsPlatform as DefaultPlatform;

/// Human-readable name of the current platform.
pub fn platform_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "unix"
    }
}
