//! Windows implementation. Windows maintainer (AmirReza) owns this file.

use crate::Platform;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Windows platform implementation.
pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn find_executable(name: &str) -> Result<PathBuf> {
        if let Ok(path) = which::which(name) {
            return Ok(path);
        }
        // npm-global shims (e.g. `codex.cmd`) are frequently missing from a
        // subprocess PATH on Windows; check %APPDATA%\npm explicitly.
        if let Ok(appdata) = std::env::var("APPDATA") {
            for ext in ["cmd", "exe"] {
                let candidate = PathBuf::from(&appdata)
                    .join("npm")
                    .join(format!("{name}.{ext}"));
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
        Err(anyhow::anyhow!(
            "executable '{name}' not found on PATH or %APPDATA%\\npm"
        ))
    }

    fn config_dir() -> Result<PathBuf> {
        let base = directories::BaseDirs::new().context("could not determine home directory")?;
        Ok(base.home_dir().join(".aibridge"))
    }

    fn command_for(exe: &Path) -> Command {
        // `.cmd`/`.bat` shims are not valid CreateProcess targets; they must be
        // run via `cmd /C`. Real `.exe` targets run directly.
        let is_script = exe
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
            .unwrap_or(false);
        if is_script {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(exe);
            cmd
        } else {
            Command::new(exe)
        }
    }
}
