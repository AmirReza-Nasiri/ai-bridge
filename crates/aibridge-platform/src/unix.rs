//! Unix (macOS/Linux) implementation. macOS maintainer (Mo) owns this file.

use crate::Platform;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unix platform implementation.
pub struct UnixPlatform;

impl Platform for UnixPlatform {
    fn find_executable(name: &str) -> Result<PathBuf> {
        which::which(name).with_context(|| format!("executable '{name}' not on PATH"))
    }

    fn config_dir() -> Result<PathBuf> {
        let base = directories::BaseDirs::new().context("could not determine home directory")?;
        Ok(base.home_dir().join(".aibridge"))
    }

    fn command_for(exe: &Path) -> Command {
        // On Unix, real binaries / shebang scripts run directly.
        Command::new(exe)
    }
}
