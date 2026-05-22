//! Windows implementation. Windows maintainer (AmirReza) owns this file.

use crate::{Platform, SpawnKind, SpawnPlan};
use anyhow::{Context, Result};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Create the process without allocating/attaching a console window. We always
/// pipe or capture stdio, so a console is never wanted; allocating one is what
/// makes `cmd /C <npm-shim>.cmd` wedge under a no-console parent.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
        // run via `cmd /C`. Real `.exe` targets run directly. Safe here because
        // every caller captures output (`.output()`) — short and self-draining.
        let mut cmd = if is_script(exe) {
            let mut cmd = Command::new("cmd");
            cmd.arg("/C").arg(exe);
            cmd
        } else {
            Command::new(exe)
        };
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }

    fn spawn_plan(exe: &Path) -> SpawnPlan {
        // Real `.exe`: launch directly, no console.
        if !is_script(exe) {
            let mut cmd = Command::new(exe);
            cmd.creation_flags(CREATE_NO_WINDOW);
            return SpawnPlan::new(cmd, SpawnKind::Direct, exe.display().to_string());
        }
        // npm `.cmd` shim: recover the `node <entry>.js` launch it wraps and run
        // that directly, so a no-console parent never wedges on `cmd /C`.
        if let Some((node, entry)) = resolve_npm_shim(exe) {
            let mut cmd = Command::new(&node);
            cmd.arg(&entry);
            cmd.creation_flags(CREATE_NO_WINDOW);
            let program = format!("{} {}", node.display(), entry.display());
            return SpawnPlan::new(cmd, SpawnKind::NodeDirect, program);
        }
        // Degraded fallback: keep working from a terminal, but `doctor` flags this
        // because it can hang the gate under the Claude Code MCP host on Windows.
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(exe);
        cmd.creation_flags(CREATE_NO_WINDOW);
        SpawnPlan::new(cmd, SpawnKind::CmdShim, format!("cmd /C {}", exe.display()))
    }
}

/// A Windows batch shim (`.cmd`/`.bat`) — not a valid `CreateProcess` target.
fn is_script(exe: &Path) -> bool {
    exe.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
        .unwrap_or(false)
}

/// Recover `(node.exe, entry.js)` from an npm-generated `.cmd` shim so the
/// wrapped Node program can be launched directly. npm shims invoke
/// `"%_prog%" "%dp0%\…\<entry>.js" %*`; we read the first `%dp0%`-relative
/// `.js` path and resolve it (and `node`) against the shim's directory.
fn resolve_npm_shim(shim: &Path) -> Option<(PathBuf, PathBuf)> {
    let dir = shim.parent()?;
    let body = std::fs::read_to_string(shim).ok()?;
    let rel = npm_shim_entry(&body)?;
    let entry = dir.join(rel);
    if !entry.exists() {
        return None;
    }
    // Prefer a `node.exe` colocated with the shim (npm sometimes bundles it),
    // else the first `node` on PATH.
    let node = {
        let local = dir.join("node.exe");
        if local.exists() {
            local
        } else {
            which::which("node").ok()?
        }
    };
    Some((node, entry))
}

/// Extract the first `%dp0%`/`%~dp0`-relative `.js` entrypoint from an npm shim
/// body, as a path relative to the shim's directory.
fn npm_shim_entry(body: &str) -> Option<String> {
    for marker in ["%dp0%\\", "%~dp0\\"] {
        for seg in body.split(marker).skip(1) {
            if let Some(end) = seg.find('"') {
                let candidate = &seg[..end];
                if candidate.to_ascii_lowercase().ends_with(".js") {
                    return Some(candidate.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_npm_global_shim() {
        // Shape of an npm-generated global `.cmd` shim (e.g. codex.cmd).
        let body = r#"@ECHO off
SETLOCAL
CALL :find_dp0
IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
)
"%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*
"#;
        assert_eq!(
            npm_shim_entry(body).as_deref(),
            Some("node_modules\\@openai\\codex\\bin\\codex.js")
        );
    }

    #[test]
    fn ignores_shim_without_js_entry() {
        assert_eq!(npm_shim_entry("@ECHO off\r\nsome-native.exe %*\r\n"), None);
    }
}
