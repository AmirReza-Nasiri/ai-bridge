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

    /// Build a [`Command`] that runs `exe`, wrapping `.cmd`/`.bat` shims through
    /// a shell where the OS requires it (Windows). Avoids the BatBadBut class of
    /// argument-escaping issues by never going through a shell on Unix.
    fn command_for(exe: &std::path::Path) -> Command;
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
