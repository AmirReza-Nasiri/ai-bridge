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

// ───────────────────────── executable-resolution helpers (v0.26.0) ─────────────────────────
//
// Pure, cross-platform helpers for the macOS launchd-PATH fallback (see `unix.rs`).
// They live here (not in the unix-only module) so they compile + unit-test on EVERY
// platform — the macOS GUI-spawn (launchd) failure class can't be reproduced from a
// Windows dev box, so thorough cross-platform unit tests are the primary safety net.

/// True only for a SIMPLE executable name (e.g. `codex`, `brew`) — exactly one normal
/// path component, no separators, not absolute, no `.`/`..`. Fallback-directory search
/// applies ONLY to bare names; an absolute path or a name with separators is left to
/// the normal resolver (which already handles those) so we never silently redirect a
/// caller's explicit path to a fallback dir.
///
/// Used in production only on macOS (via `unix.rs`); cross-platform-tested. Allowed
/// dead on non-unix where `unix.rs` (the sole production caller) isn't compiled.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn is_bare_name(name: &str) -> bool {
    use std::path::Component;
    // Reject separators explicitly: on Unix `\` is NOT a path separator, so a name
    // like `dir\codex` would otherwise parse as one Normal component and slip through.
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return false;
    }
    let p = std::path::Path::new(name);
    let mut comps = p.components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// macOS fallback bin directories, arch-aware. `apple_silicon` picks Homebrew's
/// Apple-Silicon prefix (`/opt/homebrew/bin`) first; Intel puts `/usr/local/bin`
/// first. `home` is expanded by the caller (never a literal `~`). Pure → both
/// orderings are unit-testable regardless of the host architecture.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn fallback_dirs_for_arch(home: &std::path::Path, apple_silicon: bool) -> Vec<PathBuf> {
    let brew_silicon = PathBuf::from("/opt/homebrew/bin");
    let brew_intel = PathBuf::from("/usr/local/bin");
    let user = [
        home.join(".local/bin"),
        home.join(".cargo/bin"),
        home.join(".npm-global/bin"),
    ];
    let mut dirs = Vec::with_capacity(5);
    if apple_silicon {
        dirs.push(brew_silicon);
        dirs.push(brew_intel);
    } else {
        dirs.push(brew_intel);
        dirs.push(brew_silicon);
    }
    dirs.extend(user);
    dirs
}

/// First directory in `dirs` that holds an executable regular file named `name`.
/// Returns `None` for a non-bare `name` (see [`is_bare_name`]). On Unix, requires the
/// owner/group/other execute bit; elsewhere a regular file is enough.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if !is_bare_name(name) {
        return None;
    }
    for dir in dirs {
        let candidate = dir.join(name);
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                continue; // not executable
            }
        }
        return Some(candidate);
    }
    None
}

/// Resolve `name` via `primary` FIRST (the normal PATH lookup); only on a miss try the
/// `fallback` directories. This guarantees normal-PATH resolution always wins — the
/// fallback can never shadow or reorder a result the OS PATH already provides.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn resolve_with_fallback(
    name: &str,
    primary: impl Fn(&str) -> Option<PathBuf>,
    fallback: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(p) = primary(name) {
        return Some(p);
    }
    find_in_dirs(name, fallback)
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn is_bare_name_accepts_simple_names() {
        assert!(is_bare_name("codex"));
        assert!(is_bare_name("brew"));
        assert!(is_bare_name("aibridge.exe"));
    }

    #[test]
    fn is_bare_name_rejects_absolute_and_separators_and_dotdot() {
        assert!(!is_bare_name("/usr/local/bin/codex"));
        assert!(!is_bare_name("dir/codex"));
        assert!(!is_bare_name("..")); // CurDir/ParentDir are not Normal
        assert!(!is_bare_name("."));
        assert!(!is_bare_name(""));
    }

    #[test]
    fn is_bare_name_rejects_backslash_on_all_platforms() {
        // On Unix `\` is not a path separator, but a spawn primitive must still
        // refuse to reinterpret a separator-containing command into a fallback dir.
        assert!(!is_bare_name("dir\\codex"));
        assert!(!is_bare_name("C:\\codex.exe"));
    }

    #[test]
    fn fallback_dirs_apple_silicon_homebrew_first() {
        let home = Path::new("/Users/x");
        let d = fallback_dirs_for_arch(home, true);
        assert_eq!(d[0], PathBuf::from("/opt/homebrew/bin"));
        assert_eq!(d[1], PathBuf::from("/usr/local/bin"));
        assert!(d.contains(&home.join(".cargo/bin")));
        assert!(d.contains(&home.join(".local/bin")));
        assert!(d.contains(&home.join(".npm-global/bin")));
    }

    #[test]
    fn fallback_dirs_intel_usrlocal_first() {
        let d = fallback_dirs_for_arch(Path::new("/Users/x"), false);
        assert_eq!(d[0], PathBuf::from("/usr/local/bin"));
        assert_eq!(d[1], PathBuf::from("/opt/homebrew/bin"));
    }

    #[test]
    fn fallback_dirs_expand_home_not_tilde() {
        let d = fallback_dirs_for_arch(Path::new("/Users/amir"), true);
        assert!(d.iter().all(|p| !p.to_string_lossy().contains('~')));
        assert!(d.contains(&PathBuf::from("/Users/amir/.cargo/bin")));
    }

    #[test]
    fn find_in_dirs_rejects_non_bare_name() {
        let tmp = std::env::temp_dir();
        assert!(find_in_dirs("/abs/codex", std::slice::from_ref(&tmp)).is_none());
        assert!(find_in_dirs("dir/codex", &[tmp]).is_none());
    }

    #[test]
    fn find_in_dirs_first_match_wins_and_skips_missing() {
        let base = std::env::temp_dir().join(format!(
            "aibridge-findtest-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        let d1 = base.join("d1");
        let d2 = base.join("d2");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::create_dir_all(&d2).unwrap();
        // Tool only in d2 → found there; d1 (missing) skipped.
        write_exec(&d2.join(exe_name("tool")));
        let got = find_in_dirs(&exe_name("tool"), &[d1.clone(), d2.clone()]);
        assert_eq!(got, Some(d2.join(exe_name("tool"))));
        // Now also in d1 → first dir wins.
        write_exec(&d1.join(exe_name("tool")));
        let got2 = find_in_dirs(&exe_name("tool"), &[d1.clone(), d2.clone()]);
        assert_eq!(got2, Some(d1.join(exe_name("tool"))));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_primary_wins_even_when_fallback_has_competing() {
        let primary_hit = PathBuf::from("/from/path/codex");
        let fb = vec![std::env::temp_dir()];
        // Even if a fallback file existed, primary's Some short-circuits before lookup.
        let got = resolve_with_fallback("codex", |_| Some(primary_hit.clone()), &fb);
        assert_eq!(got, Some(primary_hit));
    }

    #[test]
    fn resolve_uses_fallback_only_on_primary_miss() {
        let base = std::env::temp_dir().join(format!(
            "aibridge-resolvetest-{}-{}",
            std::process::id(),
            now_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        write_exec(&base.join(exe_name("mytool")));
        let got = resolve_with_fallback(&exe_name("mytool"), |_| None, std::slice::from_ref(&base));
        assert_eq!(got, Some(base.join(exe_name("mytool"))));
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- helpers ----
    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }
    fn exe_name(stem: &str) -> String {
        // The test creates a real file; on Windows our find_in_dirs only checks
        // is_file (no extension requirement), so a bare stem works on all platforms.
        stem.to_string()
    }
    fn write_exec(path: &Path) {
        std::fs::write(path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
