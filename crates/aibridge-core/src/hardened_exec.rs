//! Hardened execution primitives (item 3, OPTION B — the structured-run trust layer).
//!
//! INERT in this unit: pure-ish building blocks (trusted-exe resolution + env
//! sanitization + the run-mode taxonomy) with NO caller yet. Later units wire a
//! structured `run` tool on top. Raw Bash stays FREE (the owner-accepted Option-3
//! residual), so this layer does NOT "close" the residual — it is the OPT-IN trusted
//! execution path.

use aibridge_platform::{DefaultPlatform, Platform};
use std::path::{Path, PathBuf};

/// Execution-trust mode for a structured run (set by the caller per Codex's design).
/// Only `ReadOnly`/`WriteScoped` will run through the hardened path; `UnsafeUnmanaged`
/// is the explicit escape hatch that does NOT claim scope safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    ReadOnly,
    WriteScoped,
    UnsafeUnmanaged,
}

/// Why a candidate executable is NOT trusted. Fail-closed: any reason → reject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustedExeReject {
    /// `name` was empty or contained a path separator — only a BARE tool name is allowed
    /// (a caller must not point the resolver at an arbitrary file via `name`).
    NameNotBare,
    /// Not on PATH / unresolvable / unreadable.
    NotFound,
    /// Not an absolute path (can't confine identity).
    NotAbsolute,
    /// A script/shim by extension (`.cmd`/`.bat`/…) — would run an interpreter/shell.
    ShimOrScript(String),
    /// Exists but is a directory or a symlink (not a real regular file).
    NotRegularFile,
    /// (Unix) the file has no execute permission bit.
    NotExecutable,
    /// The file begins with `#!` — a shebang/interpreter script masquerading as a binary.
    ShebangScript,
}

/// Extensions that mark a script/shim rather than a directly-launchable binary.
const SHIM_EXTS: &[&str] = &["cmd", "bat", "ps1", "com", "vbs", "js", "sh", "py"];

/// Validate that an ALREADY-CANONICAL path is a trusted, directly-launchable binary.
/// PRIVATE: callers go through [`resolve_trusted_exe`] (which canonicalizes first), so a
/// symlinked / non-canonical path never reaches here; the symlink/dir rejection below is
/// defense-in-depth. This is extension + symlink + regular-file + (unix) exec-bit +
/// shebang hardening; deeper POSITIVE binary-identity (signature/magic allowlist) is a
/// documented LATER unit, not claimed here.
fn check_trusted_exe_path(path: &Path) -> Result<(), TrustedExeReject> {
    if !path.is_absolute() {
        return Err(TrustedExeReject::NotAbsolute);
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_ascii_lowercase();
        if SHIM_EXTS.contains(&ext.as_str()) {
            return Err(TrustedExeReject::ShimOrScript(ext));
        }
    }
    // `symlink_metadata` does NOT follow symlinks → a symlink (or a dir) is not a regular file.
    let meta = std::fs::symlink_metadata(path).map_err(|_| TrustedExeReject::NotFound)?;
    if !meta.file_type().is_file() {
        return Err(TrustedExeReject::NotRegularFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(TrustedExeReject::NotExecutable);
        }
    }
    // Header sniff: a real binary (ELF `\x7fELF` / Windows `MZ` / Mach-O) never starts `#!`.
    let mut buf = [0u8; 2];
    let n = {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|_| TrustedExeReject::NotFound)?;
        f.read(&mut buf).map_err(|_| TrustedExeReject::NotFound)?
    };
    if n >= 2 && &buf == b"#!" {
        return Err(TrustedExeReject::ShebangScript);
    }
    Ok(())
}

/// Resolve a tool NAME to a canonical, directly-launchable, NON-SCRIPT binary path, or a
/// reject reason. Canonicalizes (resolving symlinks) BEFORE validating, so the returned
/// path is the real target, and rejects `.cmd`/`.bat`/shebang scripts + non-regular /
/// non-exec files (the inverse of platform `command_for`, which WRAPS shims).
///
/// ⚠ PROVENANCE IS NOT VERIFIED in this unit: a compiled binary that is simply FIRST on
/// `PATH` is accepted — a PATH-shadowed `git`/`gh` would pass these shape checks. Trusted-
/// install-root pinning / binary fingerprinting is a LATER unit, so callers MUST NOT treat
/// the result as identity-trusted yet (`trusted` here = "shape-validated, non-script").
pub fn resolve_trusted_exe(name: &str) -> Result<PathBuf, TrustedExeReject> {
    // BARE tool name only — never a path. A caller must not be able to point this at an
    // arbitrary file via `name`; PATH lookup is the ONLY resolution. Rejects path
    // separators, the Windows drive marker `:` (e.g. `C:git` is drive-relative), and the
    // `.`/`..` directory names.
    if name.is_empty() || name == "." || name == ".." || name.contains([':', '/', '\\']) {
        return Err(TrustedExeReject::NameNotBare);
    }
    let found = DefaultPlatform::find_executable(name).map_err(|_| TrustedExeReject::NotFound)?;
    let canonical = std::fs::canonicalize(&found).map_err(|_| TrustedExeReject::NotFound)?;
    check_trusted_exe_path(&canonical)?;
    Ok(canonical)
}

/// Build a MINIMAL, sanitized environment from `input` for a hardened run. DROPS, by
/// prefix, EVERY loader-injection (`LD_*`/`DYLD_*`) and EVERY Git execution-control var
/// (`GIT_*`: config-injection, `GIT_EXEC_PATH`, `GIT_ASKPASS`, `GIT_DIR`/`GIT_WORK_TREE`/
/// `GIT_INDEX_FILE`/`GIT_OBJECT_DIRECTORY`/`GIT_ALTERNATE_OBJECT_DIRECTORIES`, pager, diff,
/// ssh, …), plus the `PAGER`/`SSH_ASKPASS` helpers — a hardened run passes what it needs as
/// explicit argv (e.g. `git --no-pager -C <dir>`), never via inherited env. Then RE-ADDS
/// only known-safe Git controls. It deliberately does NOT force a bare `GIT_PAGER=cat`
/// (that would resolve an attacker-controlled `cat` through a poisoned `PATH`); pager
/// disabling is done at command construction (`--no-pager`) in a later unit, as is `PATH`
/// confinement. Pure over the input slice so it is deterministic to test.
pub fn sanitize_env(input: &[(String, String)]) -> Vec<(String, String)> {
    let dropped = |key: &str| {
        let up = key.to_ascii_uppercase();
        up.starts_with("GIT_")
            || up.starts_with("LD_")
            || up.starts_with("DYLD_")
            || up.starts_with("BASH_FUNC_") // exported shell functions (ShellShock-style)
            || matches!(
                up.as_str(),
                // pager/askpass helpers + per-language code-injection knobs.
                "PAGER"
                    | "SSH_ASKPASS"
                    | "NODE_OPTIONS"
                    | "PYTHONPATH"
                    | "PYTHONSTARTUP"
                    | "PYTHONHOME"
                    | "RUBYOPT"
                    | "RUBYLIB"
                    | "PERL5OPT"
                    | "PERL5LIB"
                    | "BASH_ENV"
                    | "ENV"
            )
    };
    let mut out: Vec<(String, String)> = input
        .iter()
        .filter(|(k, _)| !dropped(k))
        .cloned()
        .collect();
    // Re-add ONLY known-safe Git controls: system config OFF, GLOBAL/user config routed to
    // the null device (so `core.pager`/`diff.external`/include in ~/.gitconfig or
    // $XDG_CONFIG_HOME can't run a helper — GIT_CONFIG_GLOBAL overrides both), never prompt.
    const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
    for (k, v) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", NULL_DEVICE),
        ("GIT_TERMINAL_PROMPT", "0"),
    ] {
        out.push((k.to_string(), v.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    fn tmp_dir() -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("aibridge-hexec-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn trusted_exe_accepts_a_regular_binary_file() {
        let f = tmp_dir().join("tool.exe");
        std::fs::write(&f, b"MZ\x90\x00 binary").unwrap();
        #[cfg(unix)]
        chmod(&f, 0o755);
        assert_eq!(check_trusted_exe_path(&f), Ok(()));
    }

    #[test]
    fn trusted_exe_rejects_every_shim_extension() {
        let d = tmp_dir();
        for &ext in SHIM_EXTS {
            let f = d.join(format!("git.{ext}"));
            std::fs::write(&f, b"echo hi").unwrap();
            #[cfg(unix)]
            chmod(&f, 0o755);
            assert!(
                matches!(check_trusted_exe_path(&f), Err(TrustedExeReject::ShimOrScript(_))),
                "{ext} must be rejected by extension"
            );
        }
    }

    #[test]
    fn trusted_exe_accepts_shape_valid_binary_without_provenance() {
        // Documents the UNIT-1 limitation: PROVENANCE is NOT checked — a shape-valid binary
        // named like a real tool passes (a PATH-shadowed `git` would too). Trusted-root /
        // fingerprint pinning is a later unit; callers must not assume identity trust.
        let f = tmp_dir().join("git"); // a "tool" we never installed
        std::fs::write(&f, b"\x7fELF fake-but-shape-valid").unwrap();
        #[cfg(unix)]
        chmod(&f, 0o755);
        assert_eq!(check_trusted_exe_path(&f), Ok(()));
    }

    #[test]
    fn trusted_exe_rejects_relative_missing_and_dir() {
        assert_eq!(
            check_trusted_exe_path(Path::new("relative/git")),
            Err(TrustedExeReject::NotAbsolute)
        );
        let d = tmp_dir();
        assert_eq!(
            check_trusted_exe_path(&d.join("does-not-exist")),
            Err(TrustedExeReject::NotFound)
        );
        assert_eq!(check_trusted_exe_path(&d), Err(TrustedExeReject::NotRegularFile));
    }

    #[test]
    fn trusted_exe_rejects_a_shebang_script_with_no_extension() {
        // A shell script renamed `git` (no extension) that ext checks would miss.
        let f = tmp_dir().join("git");
        std::fs::write(&f, b"#!/bin/sh\necho pwned").unwrap();
        #[cfg(unix)]
        chmod(&f, 0o755);
        assert_eq!(
            check_trusted_exe_path(&f),
            Err(TrustedExeReject::ShebangScript)
        );
    }

    #[cfg(unix)]
    #[test]
    fn trusted_exe_rejects_non_executable_and_symlink_on_unix() {
        let d = tmp_dir();
        let f = d.join("tool");
        std::fs::write(&f, b"ELFbin").unwrap();
        chmod(&f, 0o644); // no execute bit
        assert_eq!(
            check_trusted_exe_path(&f),
            Err(TrustedExeReject::NotExecutable)
        );
        chmod(&f, 0o755);
        let link = d.join("git-link");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        // symlink_metadata does not follow the link → not a regular file.
        assert_eq!(
            check_trusted_exe_path(&link),
            Err(TrustedExeReject::NotRegularFile)
        );
    }

    #[test]
    fn sanitize_env_drops_all_loader_and_git_control_vars() {
        let input = vec![
            ("HOME".into(), "/home/me".into()),
            ("PATH".into(), "/usr/bin".into()),
            ("LD_PRELOAD".into(), "/evil.so".into()),
            ("DYLD_INSERT_LIBRARIES".into(), "/evil.dylib".into()),
            ("PAGER".into(), "evil".into()),
            ("pager".into(), "evil".into()), // case-insensitive drop
            ("SSH_ASKPASS".into(), "/evil".into()),
            // per-language code-injection knobs — all must go.
            ("NODE_OPTIONS".into(), "--require /evil".into()),
            ("PYTHONPATH".into(), "/evil".into()),
            ("RUBYOPT".into(), "-r/evil".into()),
            ("PERL5OPT".into(), "-M/evil".into()),
            ("BASH_ENV".into(), "/evil".into()),
            ("BASH_FUNC_ls%%".into(), "() { evil; }".into()),
            // Git execution-control vars — ALL must go.
            ("GIT_EXTERNAL_DIFF".into(), "evil".into()),
            ("GIT_PAGER".into(), "evil".into()),
            ("GIT_SSH_COMMAND".into(), "evil".into()),
            ("GIT_EXEC_PATH".into(), "/evil".into()),
            ("GIT_ASKPASS".into(), "/evil".into()),
            ("GIT_DIR".into(), "/evil".into()),
            ("GIT_WORK_TREE".into(), "/evil".into()),
            ("GIT_INDEX_FILE".into(), "/evil".into()),
            ("GIT_OBJECT_DIRECTORY".into(), "/evil".into()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES".into(), "/evil".into()),
            ("GIT_CONFIG".into(), "/evil".into()),
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.pager".into()),
            ("git_config_global".into(), "/evil".into()), // case-insensitive
        ];
        let out = sanitize_env(&input);
        let get = |k: &str| out.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        // Every loader/git-control/pager/askpass var is gone (the forced safe values are
        // RE-ADDED below, so we check the dangerous INPUT keys, not the forced ones).
        for gone in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "PAGER",
            "SSH_ASKPASS",
            "NODE_OPTIONS",
            "PYTHONPATH",
            "RUBYOPT",
            "PERL5OPT",
            "BASH_ENV",
            "BASH_FUNC_ls%%",
            "GIT_EXTERNAL_DIFF",
            "GIT_PAGER",
            "GIT_SSH_COMMAND",
            "GIT_EXEC_PATH",
            "GIT_ASKPASS",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
        ] {
            // None of these keys survives (the forced GIT_CONFIG_NOSYSTEM/GIT_CONFIG_GLOBAL/
            // GIT_TERMINAL_PROMPT are distinct keys, not in this list).
            assert!(
                out.iter().all(|(k, _)| !k.eq_ignore_ascii_case(gone)),
                "{gone} must be dropped"
            );
        }
        // No bare pager is FORCED (a poisoned PATH could otherwise run an attacker `cat`).
        assert_eq!(get("GIT_PAGER"), None, "GIT_PAGER must not be forced");
        assert_eq!(get("PAGER"), None, "PAGER must not be forced");
        // Benign vars kept.
        assert_eq!(get("HOME"), Some("/home/me"));
        assert_eq!(get("PATH"), Some("/usr/bin"));
        // Known-safe Git controls re-added exactly once; the input `git_config_global=/evil`
        // is replaced by the forced null device (global/user config isolated).
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        assert_eq!(get("GIT_CONFIG_NOSYSTEM"), Some("1"));
        assert_eq!(get("GIT_CONFIG_GLOBAL"), Some(null));
        assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(
            out.iter().filter(|(k, _)| k.eq_ignore_ascii_case("GIT_CONFIG_GLOBAL")).count(),
            1
        );
    }

    #[test]
    fn resolve_trusted_exe_rejects_non_bare_names() {
        // A path (not a bare tool name) is rejected BEFORE any PATH lookup.
        assert_eq!(resolve_trusted_exe(""), Err(TrustedExeReject::NameNotBare));
        assert_eq!(resolve_trusted_exe("dir/git"), Err(TrustedExeReject::NameNotBare));
        assert_eq!(resolve_trusted_exe("/abs/git"), Err(TrustedExeReject::NameNotBare));
        assert_eq!(resolve_trusted_exe("a\\b"), Err(TrustedExeReject::NameNotBare));
        assert_eq!(resolve_trusted_exe("C:git"), Err(TrustedExeReject::NameNotBare));
        assert_eq!(resolve_trusted_exe("."), Err(TrustedExeReject::NameNotBare));
        assert_eq!(resolve_trusted_exe(".."), Err(TrustedExeReject::NameNotBare));
    }

    #[test]
    fn run_mode_variants_are_distinct() {
        assert_ne!(RunMode::ReadOnly, RunMode::WriteScoped);
        assert_ne!(RunMode::WriteScoped, RunMode::UnsafeUnmanaged);
    }
}
