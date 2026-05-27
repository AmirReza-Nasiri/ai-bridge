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

mod clipboard_helper;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::UnixPlatform as DefaultPlatform;
#[cfg(windows)]
pub use windows::WindowsPlatform as DefaultPlatform;

// ───────────────────────── ClipboardWriter ─────────────────────────

/// Copy plain text to the OS clipboard. Injectable so the TUI can mock it in
/// unit tests (no real clipboard mutation in `cargo test`). Implementations
/// MUST be bounded — never block the caller longer than a few seconds.
pub trait ClipboardWriter: Send + Sync {
    /// Returns bytes-written count on success; descriptive `Err(reason)` otherwise.
    fn copy(&self, text: &str) -> Result<usize, String>;
}

/// Production [`ClipboardWriter`] using the platform's standard clipboard tool:
/// - Windows: `clip.exe` (built-in).
/// - macOS: `pbcopy`.
/// - Linux: tries `xclip -selection clipboard`, then `wl-copy`. Returns a clear
///   "no clipboard tool found" error if neither is installed.
pub struct RealClipboardWriter;

impl ClipboardWriter for RealClipboardWriter {
    fn copy(&self, text: &str) -> Result<usize, String> {
        use std::process::Command;
        use std::time::Duration;
        let bytes = text.as_bytes();
        let timeout = Duration::from_secs(2);
        #[cfg(windows)]
        {
            let cmd = Command::new("clip.exe");
            clipboard_helper::spawn_stdin_write_bounded(cmd, bytes, timeout)
                .map_err(|e| format!("clip.exe: {e}"))
        }
        #[cfg(target_os = "macos")]
        {
            let cmd = Command::new("pbcopy");
            clipboard_helper::spawn_stdin_write_bounded(cmd, bytes, timeout)
                .map_err(|e| format!("pbcopy: {e}"))
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let mut errs: Vec<String> = Vec::new();
            for (prog, args) in &[
                ("xclip", vec!["-selection", "clipboard"]),
                ("wl-copy", vec![]),
            ] {
                let mut cmd = Command::new(prog);
                for a in args {
                    cmd.arg(a);
                }
                match clipboard_helper::spawn_stdin_write_bounded(cmd, bytes, timeout) {
                    Ok(n) => return Ok(n),
                    Err(e) => errs.push(format!("{prog}: {e}")),
                }
            }
            Err(format!(
                "no clipboard tool found (tried: {})",
                errs.join("; ")
            ))
        }
    }
}

/// Factory: returns a heap-allocated production [`ClipboardWriter`]. Used by the
/// TUI's `App::new` constructor. Tests construct their own `App::new_for_test`
/// with a mock.
pub fn real_clipboard() -> Box<dyn ClipboardWriter> {
    Box::new(RealClipboardWriter)
}

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
