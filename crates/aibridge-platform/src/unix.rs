//! Unix (macOS/Linux) implementation. macOS maintainer (Mo) owns this file.

use crate::{Platform, SpawnKind, SpawnPlan};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Unix platform implementation.
pub struct UnixPlatform;

impl Platform for UnixPlatform {
    fn find_executable(name: &str) -> Result<PathBuf> {
        // Normal PATH resolution ALWAYS wins. On macOS, a GUI-launched MCP server
        // (Claude Code) inherits the minimal launchd PATH (/usr/bin:/bin:…), so tools
        // in /opt/homebrew/bin, ~/.local/bin, ~/.cargo/bin, or an npm global bin are
        // invisible to `which`. On a miss we retry the common macOS bin dirs (bare
        // names only; additive — never shadows a real PATH hit). v0.26.0.
        let fallback = macos_fallback_dirs();
        crate::resolve_with_fallback(name, |n| which::which(n).ok(), &fallback)
            .with_context(|| format!("executable '{name}' not on PATH"))
    }

    fn config_dir() -> Result<PathBuf> {
        let base = directories::BaseDirs::new().context("could not determine home directory")?;
        Ok(base.home_dir().join(".aibridge"))
    }

    fn command_for(exe: &Path) -> Command {
        // On Unix, real binaries / shebang scripts run directly.
        Command::new(exe)
    }

    fn spawn_plan(exe: &Path) -> SpawnPlan {
        // On Unix there is no batch-shim hazard: launch directly.
        SpawnPlan::new(
            Command::new(exe),
            SpawnKind::Direct,
            exe.display().to_string(),
        )
    }
}

/// macOS launchd-PATH fallback dirs (arch-aware via `cfg!(target_arch)`), HOME
/// expanded. Empty on non-macOS unix (Linux keeps plain `which` behavior). v0.26.0.
fn macos_fallback_dirs() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        match directories::BaseDirs::new() {
            Some(b) => crate::fallback_dirs_for_arch(b.home_dir(), cfg!(target_arch = "aarch64")),
            None => Vec::new(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}
