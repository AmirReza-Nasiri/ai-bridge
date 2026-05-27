//! `aibridge rtk install/update` — safe auto-install/update of the third-party
//! rtk binary from `rtk-ai/rtk` GitHub releases. v0.20.0 (Task B).
//!
//! ## Policy change vs prior releases
//!
//! Prior to v0.20.0, AI Bridge intentionally never auto-downloaded rtk (per
//! `install::rtk_install_hint`). v0.20.0 introduces a verified auto-install path:
//! identity-checked + SHA256-verified + archive-safety-validated + atomic-replace
//! with backup-preserved-until-identity-confirmed.
//!
//! ## Safety design (Codex Stop-gate R1–R7)
//!
//! The install pipeline (in order):
//!
//! 1. Detect target via [`detect_target`] (Brew / NativeBin{writable} / NotInstalled / Unknown).
//! 2. Identity-check the EXISTING binary (if any) via [`rtk_identity_check`] — refuse
//!    to update anything that doesn't pass EITHER (a) `--version` banner contains the
//!    explicit `rtk-ai` / `Rust Token Killer` marker, OR (b) v0.20.2 fallback: banner
//!    matches the literal shape `rtk [v]X.Y.Z` (with NO trailing tokens) AND
//!    `gain --help` exits 0 — `gain` is rtk-ai/rtk's signature subcommand for the
//!    token-savings summary, not present in unrelated tools also named `rtk` (e.g.
//!    Rust Type Kit). Both halves required for fallback to fire — banner shape alone
//!    or `gain` alone is insufficient.
//! 3. Download asset + `checksums.txt` via [`ReleaseDownloader`].
//! 4. [`lookup_checksum`] → [`verify_sha256`] BEFORE any extraction.
//! 5. [`extract_rtk_binary`] with strict archive-entry validation (path traversal,
//!    symlinks, drive prefixes, UNC paths, NUL bytes all rejected).
//! 6. Stage into the target's PARENT directory (same filesystem → atomic rename).
//! 7. Swap: rename target aside as `.old.<pid>.<ts>` → rename staged into place.
//! 8. Post-install identity check the JUST-INSTALLED binary. On failure: rollback
//!    (remove bad new file, restore backup), with LOUD error if rollback itself fails.
//! 9. Only THEN remove the `.old.*` backup.
//!
//! ## Linux
//!
//! Auto-install/update on Linux is **deferred by policy** in v0.20.0 (upstream rtk
//! does publish Linux assets — see https://github.com/rtk-ai/rtk/releases — but
//! AI Bridge has not yet certified the Linux install path). [`detect_target`] on
//! Linux always returns [`RtkTarget::Unsupported`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli_update::{CommandRunner, PathResolver};

// ───────────────────────── public types ─────────────────────────

/// Where (and whether) to install rtk on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtkTarget {
    /// Homebrew-managed. Update via `brew upgrade rtk` (no archive handling).
    Brew,
    /// User-local native binary at `path`. `writable == true` only when the file
    /// and its parent directory are writable by the current user (probed live).
    NativeBin { path: PathBuf, writable: bool },
    /// rtk isn't on PATH. `install_to` is the canonical target location for a
    /// fresh install on this platform.
    NotInstalled { install_to: PathBuf },
    /// We can't safely auto-update — explicit user action required.
    Unknown { reason: String },
    /// rtk auto-install/update is not implemented for this platform in this
    /// release (Linux currently).
    Unsupported { reason: String },
}

/// Options for [`install_or_update`].
#[derive(Debug, Clone)]
pub struct InstallOpts {
    /// Auto-accept the "about to install/update" prompt. Without this, the
    /// caller MUST run in an interactive TTY (the function does not prompt
    /// itself — it relies on the caller's `Confirmer`).
    pub yes: bool,
    /// `true` when the user explicitly requested `aibridge rtk install` (i.e.
    /// install when missing). `false` when the user requested `aibridge rtk
    /// update` (i.e. only update an existing install; do nothing if missing).
    pub allow_fresh_install: bool,
}

// ───────────────────────── ReleaseDownloader seam ─────────────────────────

/// Fetches `asset` + `checksums.txt` for a GitHub release into `dst_dir`.
/// Implementations may shell out to `gh release download`, use HTTP directly,
/// or read fixtures (in tests).
pub trait ReleaseDownloader: Send + Sync {
    fn download(&self, slug: &str, tag: &str, asset: &str, dst_dir: &Path) -> Result<(), String>;
}

/// `gh release download <tag> --repo <slug> --pattern <asset> --pattern checksums.txt`.
pub struct GhReleaseDownloader;

impl ReleaseDownloader for GhReleaseDownloader {
    fn download(&self, slug: &str, tag: &str, asset: &str, dst_dir: &Path) -> Result<(), String> {
        use std::process::Command;
        let mut cmd = Command::new("gh");
        cmd.args([
            "release",
            "download",
            tag,
            "--repo",
            slug,
            "--pattern",
            asset,
            "--pattern",
            "checksums.txt",
            "--dir",
            &dst_dir.display().to_string(),
        ]);
        let out = crate::update::run_command_with_timeout(cmd, Duration::from_secs(180)).map_err(
            |e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    "rtk install/update needs the GitHub CLI (`gh`) — install it + `gh auth login`"
                        .to_string()
                } else if e.kind() == std::io::ErrorKind::TimedOut {
                    format!("timed out downloading {asset} from {slug}@{tag}")
                } else {
                    format!("rtk download failed: {e}")
                }
            },
        )?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let first = stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("download failed");
            return Err(format!(
                "rtk download failed from {slug}@{tag} (asset={asset}): {}",
                first.trim()
            ));
        }
        Ok(())
    }
}

// ───────────────────────── FsOps seam (for rollback testing) ─────────────────────────

/// Minimal filesystem operations needed by [`stage_swap_verify`]. Injectable so
/// rollback-failure paths are testable without root.
pub trait FsOps: Send + Sync {
    fn exists(&self, p: &Path) -> bool;
    fn rename(&self, src: &Path, dst: &Path) -> Result<(), String>;
    fn remove_file(&self, p: &Path) -> Result<(), String>;
    fn copy(&self, src: &Path, dst: &Path) -> Result<(), String>;
}

/// Production FsOps wrapper around `std::fs`.
pub struct RealFsOps;

impl FsOps for RealFsOps {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn rename(&self, src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::rename(src, dst).map_err(|e| e.to_string())
    }
    fn remove_file(&self, p: &Path) -> Result<(), String> {
        std::fs::remove_file(p).map_err(|e| e.to_string())
    }
    fn copy(&self, src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

// ───────────────────────── pure helpers ─────────────────────────

/// Map a Rust target triple to the rtk release asset name. Returns `None` for
/// targets we don't support yet (Linux, other archs). All known assets per
/// upstream as of v0.20.0:
/// - `x86_64-pc-windows-msvc` → `rtk-x86_64-pc-windows-msvc.zip`
/// - `aarch64-apple-darwin`   → `rtk-aarch64-apple-darwin.tar.gz`
/// - `x86_64-apple-darwin`    → `rtk-x86_64-apple-darwin.tar.gz`
pub fn rtk_asset_name(target: &str) -> Option<&'static str> {
    match target {
        "x86_64-pc-windows-msvc" => Some("rtk-x86_64-pc-windows-msvc.zip"),
        "aarch64-apple-darwin" => Some("rtk-aarch64-apple-darwin.tar.gz"),
        "x86_64-apple-darwin" => Some("rtk-x86_64-apple-darwin.tar.gz"),
        _ => None,
    }
}

/// rtk's binary file name inside the extracted archive on this platform.
pub fn rtk_binary_name(target: &str) -> &'static str {
    if target.contains("windows") {
        "rtk.exe"
    } else {
        "rtk"
    }
}

/// Look up `<asset>`'s expected SHA256 from a GNU-coreutils-format `checksums.txt`
/// body. Returns the lowercase 64-char hex digest. Blank lines and `#` comment
/// lines are ignored. `*` binary-mode prefix on the filename is tolerated.
pub fn lookup_checksum(checksums_txt: &str, asset_name: &str) -> Result<String, String> {
    for raw in checksums_txt.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        let name = name.trim_start_matches('*');
        if name == asset_name {
            let lower = hash.to_ascii_lowercase();
            if lower.len() != 64 || !lower.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "checksum for {asset_name} is malformed (expected 64 hex chars, got '{hash}')"
                ));
            }
            return Ok(lower);
        }
    }
    Err(format!(
        "no checksum entry for {asset_name} in checksums.txt"
    ))
}

/// Compute the SHA256 of `path` and verify it equals `expected` (which MUST be
/// a lowercase 64-char hex digest — caller's responsibility, enforced here).
pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    if expected.len() != 64
        || !expected
            .chars()
            .all(|c| c.is_ascii_hexdigit() && (c.is_numeric() || c.is_ascii_lowercase()))
    {
        return Err(format!(
            "verify_sha256: 'expected' must be lowercase 64-char hex; got '{expected}'"
        ));
    }
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| format!("can't read {path:?}: {e}"))?;
    if bytes.is_empty() {
        return Err(format!("archive is empty: {path:?}"));
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got = hex_lower(&hasher.finalize());
    if got != expected {
        return Err(format!(
            "checksum mismatch for {path:?}: got {got}, expected {expected}"
        ));
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Reject archive entry names that could escape the destination directory or
/// otherwise cause unsafe extraction. Used by [`extract_rtk_binary`].
///
/// Rejects:
/// - Empty name.
/// - Names containing NUL bytes.
/// - Absolute paths (Unix: leading `/`; Windows: drive-letter prefix; UNC: `\\`).
/// - Any path component equal to `..` or `.`.
/// - Any `\` backslash (treated as a path separator on Windows, including in
///   zip entry names that came from a Windows-built archive).
pub fn validate_archive_entry_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("archive entry has empty name".into());
    }
    if name.contains('\0') {
        return Err(format!("archive entry name contains NUL byte: {name:?}"));
    }
    // Absolute Unix path.
    if name.starts_with('/') {
        return Err(format!("archive entry is absolute path: {name}"));
    }
    // Windows drive-letter (e.g. `C:\foo`).
    if name.len() >= 3 {
        let bytes = name.as_bytes();
        if bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            return Err(format!("archive entry has drive prefix: {name}"));
        }
    }
    // Windows UNC path.
    if name.starts_with("\\\\") {
        return Err(format!("archive entry is UNC path: {name}"));
    }
    // Backslash path separator (could be used to escape on Windows).
    if name.contains('\\') {
        return Err(format!("archive entry uses backslash separator: {name}"));
    }
    // `..` or `.` components.
    for comp in name.split('/') {
        if comp == ".." {
            return Err(format!("archive entry has parent-dir traversal: {name}"));
        }
        if comp == "." {
            return Err(format!("archive entry has current-dir component: {name}"));
        }
    }
    Ok(())
}

/// `true` iff `target.exists()` AND `target` plus its parent dir are writable
/// by the current user. Probes by attempting to create + remove a sentinel file
/// in the parent dir; that's the only cross-platform-reliable probe.
pub fn is_writable_install_path(target: &Path) -> bool {
    let Some(parent) = target.parent() else {
        return false;
    };
    if !parent.exists() {
        return false;
    }
    let probe = parent.join(format!(
        ".aibridge-rtk-write-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Canonical install location for a fresh `aibridge rtk install` on this
/// platform. Windows: `%USERPROFILE%\.local\bin\rtk.exe`. macOS:
/// `$HOME/.local/bin/rtk`. Linux returns the same path BUT is documented as
/// `Unsupported` in [`detect_target`].
pub fn default_install_path() -> Option<PathBuf> {
    let home = dirs_home()?;
    let bin = home.join(".local").join("bin");
    let name = if cfg!(windows) { "rtk.exe" } else { "rtk" };
    Some(bin.join(name))
}

fn dirs_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

// ───────────────────────── identity check ─────────────────────────

/// Tight banner-shape predicate for the v0.20.2 fallback identity path. Only the
/// literal upstream rtk-ai banner shape passes — the trimmed first line must be
/// EXACTLY `rtk X.Y.Z` or `rtk vX.Y.Z` (optional `v` prefix because rtk-ai ships
/// both forms in the wild). No trailing tokens, no prerelease/build suffix, no
/// extra version components. Case-sensitive on the `rtk ` prefix.
///
/// Returning `true` is necessary BUT NOT sufficient for identity — `rtk_identity_check`
/// also requires `gain --help` to exit 0, since the `gain` subcommand is
/// rtk-ai-specific (Rust Type Kit has no such subcommand).
fn banner_looks_like_rtk(s: &str) -> bool {
    let first = s.lines().next().unwrap_or("").trim();
    let Some(rest) = first.strip_prefix("rtk ") else {
        return false;
    };
    // STRICT exact: the entire remainder of the trimmed first line must be the
    // version token alone. `split_whitespace().next()` would have let
    // `rtk 1.2.3 garbage` pass — rejected by Codex review.
    let core = rest.strip_prefix('v').unwrap_or(rest);
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Run `<exe> --version` and confirm the banner identifies this as rtk-ai's rtk
/// (NOT Rust Type Kit or any other tool that also uses the `rtk` name).
///
/// Two accept paths (Codex R5 design):
/// - **Path A (marker, short-circuit)**: stdout+stderr contains either `rtk-ai` or
///   `Rust Token Killer`. Returns `Ok(())` immediately without probing further.
/// - **Path B (banner shape + `gain --help`)** [v0.20.2 fallback]: trimmed first
///   line matches EXACTLY `rtk [v]X.Y.Z` (via [`banner_looks_like_rtk`]) AND
///   `<exe> gain --help` exits 0. Required when upstream rtk-ai ships a banner
///   without the explicit marker (the 0.40.0 case the user reported).
///
/// Anything else → refuse. Caller decides what to do with the error
/// (manual-only fallback for `check_rtk`; abort for `install_or_update`).
pub fn rtk_identity_check(runner: &dyn CommandRunner, exe: &Path) -> Result<(), String> {
    let (ok, combined) = runner
        .run_path(exe, &["--version"], Duration::from_secs(3))
        .map_err(|e| format!("could not run {exe:?} --version: {e}"))?;
    if !ok {
        return Err(format!(
            "{exe:?} --version exited non-zero (output: {})",
            sanitize_snippet(&combined, 100)
        ));
    }
    // Path A: explicit marker — short-circuit, never probe gain.
    if combined.contains("rtk-ai") || combined.contains("Rust Token Killer") {
        return Ok(());
    }
    // Path B: plausible banner shape + gain --help success. `gain` is probed ONLY
    // when banner already looks plausible, so unrelated tools don't get an extra
    // spawn for no reason.
    if banner_looks_like_rtk(&combined) {
        if let Ok((true, _)) = runner.run_path(exe, &["gain", "--help"], Duration::from_secs(3)) {
            return Ok(());
        }
    }
    Err(format!(
        "{exe:?} identity not confirmed (banner missing 'rtk-ai' / 'Rust Token Killer' marker \
         AND either banner doesn't match clean 'rtk [v]X.Y.Z' shape OR `gain --help` failed; \
         got --version: {})",
        sanitize_snippet(&combined, 100)
    ))
}

/// UTF-8-safe character-count truncation. The prior `&s[..n]` form panicked on
/// multi-byte input (Persian, emoji, etc.) — reviewer F6 + plan_gate R3 B5.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        format!("{}…", &s[..cut])
    }
}

/// Sanitize a subprocess output snippet for embedding in a user-visible error.
/// Collapses control chars (newlines, NUL, etc.) to single spaces, collapses
/// runs of whitespace, then UTF-8-safe truncates to `max` chars. Prevents
/// secrets in stderr from being formatted into a multi-line error string
/// (reviewer F6).
fn sanitize_snippet(raw: &str, max: usize) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&cleaned, max)
}

// ───────────────────────── target detection ─────────────────────────

/// Decide what to do with rtk on this machine. Pure-ish (uses runner+resolver
/// seams + filesystem probe via [`is_writable_install_path`]).
pub fn detect_target(runner: &dyn CommandRunner, resolver: &dyn PathResolver) -> RtkTarget {
    // Linux is deferred by policy in v0.20.0.
    if cfg!(target_os = "linux") {
        return RtkTarget::Unsupported {
            reason: "rtk auto-install on Linux is deferred in v0.20.0 (upstream assets exist; \
                     AI Bridge has not certified the install path yet)"
                .into(),
        };
    }
    // Locate any existing rtk on PATH.
    match resolver.find("rtk") {
        Ok(path) => {
            // Try identity check; if it fails, refuse to auto-update.
            if let Err(why) = rtk_identity_check(runner, &path) {
                return RtkTarget::Unknown {
                    reason: format!(
                        "rtk found at {path:?} but identity check failed: {why}; refusing to auto-update"
                    ),
                };
            }
            // Brew detection: path under brew prefix AND `brew list rtk` confirms.
            let path_lower = path.to_string_lossy().to_ascii_lowercase();
            let looks_brew = path_lower.contains("/homebrew/")
                || path_lower.contains("/usr/local/cellar/")
                || path_lower.contains("/linuxbrew/");
            if looks_brew {
                if let Ok((ok, _)) = runner.run("brew", &["list", "rtk"], Duration::from_secs(5)) {
                    if ok {
                        return RtkTarget::Brew;
                    }
                }
            }
            // Native binary fallback.
            let writable = is_writable_install_path(&path);
            RtkTarget::NativeBin { path, writable }
        }
        Err(_) => {
            // Not installed. Pick the canonical install location.
            match default_install_path() {
                Some(p) => RtkTarget::NotInstalled { install_to: p },
                None => RtkTarget::Unknown {
                    reason: "no HOME / USERPROFILE — can't pick install location".into(),
                },
            }
        }
    }
}

// ───────────────────────── archive extraction ─────────────────────────

/// Extract the single rtk binary from `archive` into `out_dir`, returning the
/// path of the extracted binary. ALL archive entries are validated with
/// [`validate_archive_entry_name`] before any byte is written. Symlinks and
/// hard links are rejected. If the archive contains 0 or > 1 entries matching
/// the expected binary name, the function refuses.
pub fn extract_rtk_binary(
    archive: &Path,
    target_bin_name: &str,
    out_dir: &Path,
) -> Result<PathBuf, String> {
    let name_lower = archive
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    match name_lower.as_deref() {
        Some("zip") => extract_zip(archive, target_bin_name, out_dir),
        Some("gz") => extract_tar_gz(archive, target_bin_name, out_dir),
        _ => Err(format!(
            "unsupported archive type: {archive:?} (expected .zip or .tar.gz)"
        )),
    }
}

fn extract_zip(archive: &Path, target_bin_name: &str, out_dir: &Path) -> Result<PathBuf, String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("can't open {archive:?}: {e}"))?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("can't read zip {archive:?}: {e}"))?;
    let mut matches: Vec<usize> = Vec::new();
    for i in 0..zip.len() {
        let f = zip
            .by_index(i)
            .map_err(|e| format!("can't read zip entry {i}: {e}"))?;
        // Reject any entry that's a symlink (signaled by external attrs on Unix-built zips).
        let unix_mode = f.unix_mode();
        if let Some(mode) = unix_mode {
            const S_IFLNK: u32 = 0o120000;
            if mode & 0o170000 == S_IFLNK {
                return Err(format!(
                    "zip entry {:?} is a symlink — refusing to extract",
                    f.name()
                ));
            }
        }
        let raw_name = f.name().to_string();
        validate_archive_entry_name(&raw_name).map_err(|e| format!("zip entry rejected: {e}"))?;
        // Match by basename: `rtk.exe` at any safe depth.
        let basename = Path::new(&raw_name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if basename.eq_ignore_ascii_case(target_bin_name) && !f.is_dir() {
            matches.push(i);
        }
    }
    if matches.is_empty() {
        return Err(format!("no '{target_bin_name}' entry found in {archive:?}"));
    }
    if matches.len() > 1 {
        return Err(format!(
            "{} entries match '{target_bin_name}' in {archive:?} — refusing (ambiguous)",
            matches.len()
        ));
    }
    let mut f = zip
        .by_index(matches[0])
        .map_err(|e| format!("can't read matched zip entry: {e}"))?;
    let out_path = out_dir.join(target_bin_name);
    let mut out =
        std::fs::File::create(&out_path).map_err(|e| format!("can't write {out_path:?}: {e}"))?;
    std::io::copy(&mut f, &mut out)
        .map_err(|e| format!("can't copy bytes from zip to {out_path:?}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(out_path)
}

fn extract_tar_gz(
    archive: &Path,
    target_bin_name: &str,
    out_dir: &Path,
) -> Result<PathBuf, String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("can't open {archive:?}: {e}"))?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    let mut matches: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in tar
        .entries()
        .map_err(|e| format!("can't iterate tar {archive:?}: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("bad tar entry: {e}"))?;
        let header = entry.header();
        // Reject symlinks and hard links.
        use tar::EntryType;
        match header.entry_type() {
            EntryType::Symlink | EntryType::Link => {
                return Err(format!(
                    "tar entry {:?} is a link — refusing",
                    entry.path().ok()
                ));
            }
            _ => {}
        }
        let path = entry
            .path()
            .map_err(|e| format!("bad tar entry path: {e}"))?
            .to_string_lossy()
            .to_string();
        validate_archive_entry_name(&path).map_err(|e| format!("tar entry rejected: {e}"))?;
        let basename = Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if basename.eq_ignore_ascii_case(target_bin_name) && header.entry_type().is_file() {
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf)
                .map_err(|e| format!("can't read tar entry bytes: {e}"))?;
            matches.push((path, buf));
        }
    }
    if matches.is_empty() {
        return Err(format!("no '{target_bin_name}' entry found in {archive:?}"));
    }
    if matches.len() > 1 {
        return Err(format!(
            "{} entries match '{target_bin_name}' in {archive:?} — refusing (ambiguous)",
            matches.len()
        ));
    }
    let out_path = out_dir.join(target_bin_name);
    std::fs::write(&out_path, &matches[0].1)
        .map_err(|e| format!("can't write {out_path:?}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(out_path)
}

// ───────────────────────── stage + swap + identity ─────────────────────────

/// Build a unique backup-aside path (`<install>.old.<pid>.<ts>`) in the same
/// directory as `install` so subsequent rename is atomic.
pub fn make_backup_path(install: &Path) -> PathBuf {
    let fname = install
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("rtk");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    install.with_file_name(format!("{fname}.old.{}.{ts}", std::process::id()))
}

/// Stage the verified+extracted binary into the target's parent directory, then
/// rename it into place, then run a post-install identity check. On any failure
/// after rename: try to remove the bad new file AND restore the backup. ALL
/// failures (including failed rollback) are reported LOUDLY with the backup
/// path so the user can recover manually.
pub fn stage_swap_verify(
    install: &Path,
    extracted: &Path,
    runner: &dyn CommandRunner,
    fs: &dyn FsOps,
) -> Result<(), String> {
    let parent = install
        .parent()
        .ok_or_else(|| format!("install path {install:?} has no parent dir"))?;
    let staged = parent.join(format!(
        "{}.new.{}.{}",
        install
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("rtk"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs.copy(extracted, &staged)
        .map_err(|e| format!("couldn't stage rtk in target dir: {e}"))?;
    let had_prior = fs.exists(install);
    let backup = make_backup_path(install);
    if had_prior {
        if let Err(e) = fs.rename(install, &backup) {
            let _ = fs.remove_file(&staged);
            return Err(format!(
                "couldn't move current rtk aside ({e}); staged at {staged:?}"
            ));
        }
    }
    if let Err(e) = fs.rename(&staged, install) {
        let restore_msg = if had_prior {
            match fs.rename(&backup, install) {
                Ok(()) => "previous binary restored".to_string(),
                Err(re) => format!(
                    "RESTORE FAILED ({re}); backup at {backup:?}; restore manually: \
                     mv {backup:?} {install:?}"
                ),
            }
        } else {
            "no prior binary to restore".to_string()
        };
        return Err(format!(
            "couldn't move new rtk into place ({e}); {restore_msg}; staged at {staged:?}"
        ));
    }
    // Post-install identity check the just-installed binary.
    if let Err(id_err) = rtk_identity_check(runner, install) {
        let remove_msg = match fs.remove_file(install) {
            Ok(()) => "bad binary removed".to_string(),
            Err(re) => format!(
                "REMOVE FAILED ({re}); BAD BINARY REMAINS AT {install:?} — \
                 inspect and remove manually"
            ),
        };
        let restore_msg = if had_prior {
            match fs.rename(&backup, install) {
                Ok(()) => "previous binary restored".to_string(),
                Err(re) => format!(
                    "RESTORE FAILED ({re}); backup at {backup:?}; restore manually: \
                     mv {backup:?} {install:?}"
                ),
            }
        } else {
            "no prior binary to restore (fresh install)".to_string()
        };
        return Err(format!(
            "post-install identity failed: {id_err}; {remove_msg}; {restore_msg}"
        ));
    }
    // Only NOW remove the backup.
    if had_prior {
        let _ = fs.remove_file(&backup); // best-effort
    }
    Ok(())
}

// ───────────────────────── orchestrator ─────────────────────────

/// Top-level rtk install/update flow. Wires together all the seams:
/// detection → download → checksum → extract → stage/swap/identity.
pub fn install_or_update(
    runner: &dyn CommandRunner,
    resolver: &dyn PathResolver,
    downloader: &dyn ReleaseDownloader,
    fs: &dyn FsOps,
    _opts: InstallOpts,
) -> Result<String, String> {
    let target = detect_target(runner, resolver);
    let (install_path, brew_path) = match target {
        RtkTarget::Brew => (None, true),
        RtkTarget::NativeBin {
            path,
            writable: true,
        } => (Some(path), false),
        RtkTarget::NativeBin {
            path,
            writable: false,
        } => {
            return Err(format!(
                "rtk install path {path:?} is not writable by the current user; \
                 update manually or fix permissions"
            ));
        }
        RtkTarget::NotInstalled { install_to } => {
            if !_opts.allow_fresh_install {
                return Err(format!(
                    "rtk is not installed; run `aibridge rtk install [--yes]` to fetch it to {install_to:?}"
                ));
            }
            // Ensure parent dir exists for the fresh install location.
            if let Some(parent) = install_to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("can't create install dir {parent:?}: {e}"))?;
            }
            (Some(install_to), false)
        }
        RtkTarget::Unknown { reason } => {
            return Err(format!("rtk auto-update unavailable: {reason}"));
        }
        RtkTarget::Unsupported { reason } => {
            return Err(format!(
                "rtk auto-install unsupported on this platform: {reason}"
            ));
        }
    };

    if brew_path {
        return run_brew_upgrade(runner);
    }

    let install_path = install_path.expect("non-brew path guaranteed Some(path)");

    // Resolve target triple + asset.
    let target_triple = crate::update::current_target();
    let asset = rtk_asset_name(target_triple).ok_or_else(|| {
        format!(
            "no rtk asset published for target '{target_triple}' — \
             see https://github.com/rtk-ai/rtk/releases"
        )
    })?;
    let bin_name = rtk_binary_name(target_triple);

    // Look up latest tag.
    let tag = crate::cli_update::gh_latest_release_tag_raw(runner, "rtk-ai/rtk")
        .ok_or("could not resolve rtk's latest release tag from GitHub")?;

    // Download asset + checksums into a fresh temp dir.
    let tmp = make_temp_dir()?;
    let dl_res = downloader.download("rtk-ai/rtk", &tag, asset, &tmp);
    if let Err(e) = dl_res {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let archive = tmp.join(asset);
    let checksums_path = tmp.join("checksums.txt");
    if !archive.exists() || !checksums_path.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "rtk download incomplete (missing {asset} or checksums.txt in {tmp:?})"
        ));
    }
    let checksums_txt = std::fs::read_to_string(&checksums_path)
        .map_err(|e| format!("can't read checksums.txt: {e}"))?;
    let expected = match lookup_checksum(&checksums_txt, asset) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    if let Err(e) = verify_sha256(&archive, &expected) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("refusing to extract: {e}"));
    }
    let extracted = match extract_rtk_binary(&archive, bin_name, &tmp) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    // Stage + swap + identity check.
    if let Err(e) = stage_swap_verify(&install_path, &extracted, runner, fs) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    // Post-install PATH visibility warning (helpful for fresh installs).
    let mut msg = format!(
        "rtk installed at {} (from {asset} @ {tag})",
        install_path.display()
    );
    let mut visible_on_path = false;
    if let Ok(resolved) = resolver.find("rtk") {
        if crate::process_cleanup::same_install_path(&resolved, &install_path) {
            visible_on_path = true;
        }
    }
    if !visible_on_path {
        msg.push_str("\nWARNING: install dir is not on PATH; add it to your shell rc to use 'rtk' from your prompt.");
    }
    Ok(msg)
}

/// v0.22.0: in-TUI variant of [`install_or_update`] limited to NATIVE rtk paths
/// (no brew). Emits stage names via `on_stage` before each major phase so the
/// caller (TUI) can render live progress. Returns `Err` for brew-managed rtk —
/// the brew path still uses [`install_or_update`] in the restored terminal.
///
/// Cancellation: NOT cooperative. Once started, runs to completion. The caller
/// (TUI) blocks `q` while a worker is active. Note: in a crossterm raw-mode
/// TUI, Ctrl+C is delivered as a key event (NOT an OS signal) and v0.22.0
/// installs no Ctrl+C handler — practical emergency abort is forced process
/// termination (closing the terminal window, OS kill). Termination during the
/// atomic-replace + identity-verify critical section may leave temp files,
/// staged files, backups, or an unverified replacement — manual recovery may
/// be required (delete `<system tmp>/aibridge-rtk-*`, restore from the rollback
/// backup at `<install>.old.<pid>.<ts>` if present).
pub fn install_or_update_native_with_progress(
    runner: &dyn CommandRunner,
    resolver: &dyn PathResolver,
    downloader: &dyn ReleaseDownloader,
    fs: &dyn FsOps,
    opts: InstallOpts,
    on_stage: &(dyn Fn(&str) + Send + Sync),
) -> Result<String, String> {
    on_stage("resolving target");
    let target = detect_target(runner, resolver);
    let install_path = match target {
        RtkTarget::Brew => {
            return Err(
                "not a native install — use install_or_update for brew-managed rtk".to_string(),
            );
        }
        RtkTarget::NativeBin {
            path,
            writable: true,
        } => path,
        RtkTarget::NativeBin {
            path,
            writable: false,
        } => {
            return Err(format!(
                "rtk install path {path:?} is not writable by the current user; \
                 update manually or fix permissions"
            ));
        }
        RtkTarget::NotInstalled { install_to } => {
            if !opts.allow_fresh_install {
                return Err(format!(
                    "rtk is not installed; run `aibridge rtk install [--yes]` to fetch it to {install_to:?}"
                ));
            }
            if let Some(parent) = install_to.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("can't create install dir {parent:?}: {e}"))?;
            }
            install_to
        }
        RtkTarget::Unknown { reason } => {
            return Err(format!("rtk auto-update unavailable: {reason}"));
        }
        RtkTarget::Unsupported { reason } => {
            return Err(format!(
                "rtk auto-install unsupported on this platform: {reason}"
            ));
        }
    };

    let target_triple = crate::update::current_target();
    let asset = rtk_asset_name(target_triple).ok_or_else(|| {
        format!(
            "no rtk asset published for target '{target_triple}' — \
             see https://github.com/rtk-ai/rtk/releases"
        )
    })?;
    let bin_name = rtk_binary_name(target_triple);

    on_stage("resolving latest release tag");
    let tag = crate::cli_update::gh_latest_release_tag_raw(runner, "rtk-ai/rtk")
        .ok_or("could not resolve rtk's latest release tag from GitHub")?;

    on_stage(&format!("downloading {asset}"));
    let tmp = make_temp_dir()?;
    let dl_res = downloader.download("rtk-ai/rtk", &tag, asset, &tmp);
    if let Err(e) = dl_res {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let archive = tmp.join(asset);
    let checksums_path = tmp.join("checksums.txt");
    if !archive.exists() || !checksums_path.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "rtk download incomplete (missing {asset} or checksums.txt in {tmp:?})"
        ));
    }
    let checksums_txt = std::fs::read_to_string(&checksums_path)
        .map_err(|e| format!("can't read checksums.txt: {e}"))?;
    let expected = match lookup_checksum(&checksums_txt, asset) {
        Ok(h) => h,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };

    on_stage("verifying SHA256");
    if let Err(e) = verify_sha256(&archive, &expected) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("refusing to extract: {e}"));
    }

    on_stage("extracting");
    let extracted = match extract_rtk_binary(&archive, bin_name, &tmp) {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };

    on_stage("atomic-replacing");
    if let Err(e) = stage_swap_verify(&install_path, &extracted, runner, fs) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&tmp);

    let mut msg = format!(
        "rtk installed at {} (from {asset} @ {tag})",
        install_path.display()
    );
    let mut visible_on_path = false;
    if let Ok(resolved) = resolver.find("rtk") {
        if crate::process_cleanup::same_install_path(&resolved, &install_path) {
            visible_on_path = true;
        }
    }
    if !visible_on_path {
        msg.push_str("\nWARNING: install dir is not on PATH; add it to your shell rc to use 'rtk' from your prompt.");
    }
    Ok(msg)
}

fn run_brew_upgrade(_runner: &dyn CommandRunner) -> Result<String, String> {
    // Use the inherited-stdio mutation path (same pattern as cli_update::apply_cli_update).
    let argv = vec!["brew".to_string(), "upgrade".to_string(), "rtk".to_string()];
    let code = crate::cli_update::apply_cli_update(&argv)?;
    if code == 0 {
        Ok("rtk upgraded via brew".to_string())
    } else {
        Err(format!("brew upgrade rtk exited {code}"))
    }
}

fn make_temp_dir() -> Result<PathBuf, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("aibridge-rtk-{}-{}", std::process::id(), ts));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't make rtk temp dir: {e}"))?;
    Ok(dir)
}

#[allow(dead_code)]
fn extension_lower(p: &Path) -> Option<String> {
    p.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
}

#[allow(dead_code)]
fn collect_set(s: &[String]) -> HashSet<String> {
    s.iter().cloned().collect()
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ─── v0.20.1 sanitize_snippet (multibyte safe, control chars stripped) ───
    #[test]
    fn sanitize_snippet_handles_multibyte_safely() {
        let input = "rtk نسخه ۰.۴۰.۰\n\t\x00secret-token\r🦀more";
        // Must not panic on the multi-byte input.
        let out = sanitize_snippet(input, 100);
        // No control chars (newline, tab, NUL, CR) survive.
        assert!(!out.contains('\n'));
        assert!(!out.contains('\t'));
        assert!(!out.contains('\0'));
        assert!(!out.contains('\r'));
        // Persian + emoji preserved.
        assert!(out.contains("نسخه"));
        assert!(out.contains("🦀"));
    }

    #[test]
    fn sanitize_snippet_truncates_at_char_boundary() {
        // 200 Persian chars = 400 bytes (each char is 2 bytes in UTF-8).
        let long = "ع".repeat(200);
        let out = sanitize_snippet(&long, 50);
        // Must not panic; output should be ≤ 50 chars (+ ellipsis).
        assert!(out.chars().count() <= 51);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_chars_safe_on_multibyte() {
        let s = "abcعصلام";
        // count is 7 chars
        assert_eq!(truncate_chars(s, 10), s);
        let cut = truncate_chars(s, 4);
        assert_eq!(cut, "abcع…");
    }

    #[test]
    fn rtk_asset_name_windows_x86_64() {
        assert_eq!(
            rtk_asset_name("x86_64-pc-windows-msvc"),
            Some("rtk-x86_64-pc-windows-msvc.zip")
        );
    }
    #[test]
    fn rtk_asset_name_macos_aarch64() {
        assert_eq!(
            rtk_asset_name("aarch64-apple-darwin"),
            Some("rtk-aarch64-apple-darwin.tar.gz")
        );
    }
    #[test]
    fn rtk_asset_name_macos_x86_64() {
        assert_eq!(
            rtk_asset_name("x86_64-apple-darwin"),
            Some("rtk-x86_64-apple-darwin.tar.gz")
        );
    }
    #[test]
    fn rtk_asset_name_unsupported_target_returns_none() {
        assert_eq!(rtk_asset_name("x86_64-unknown-linux-gnu"), None);
        assert_eq!(rtk_asset_name("riscv64-unknown-linux-gnu"), None);
    }

    // ─── lookup_checksum ───
    // A real 64-char lowercase hex digest fixture (sha256 of empty string).
    const HEX_A: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    const HEX_B: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn lookup_checksum_finds_match_gnu_format() {
        let text = format!(
            "{HEX_A}  rtk-x86_64-pc-windows-msvc.zip\n{HEX_B}  rtk-aarch64-apple-darwin.tar.gz\n"
        );
        assert_eq!(
            lookup_checksum(&text, "rtk-x86_64-pc-windows-msvc.zip"),
            Ok(HEX_A.to_string())
        );
    }
    #[test]
    fn lookup_checksum_handles_asterisk_prefix() {
        let text = format!("{HEX_A} *rtk.zip\n");
        assert!(lookup_checksum(&text, "rtk.zip").is_ok());
    }
    #[test]
    fn lookup_checksum_ignores_blank_and_comment_lines() {
        let text = format!("# checksums for v0.42.0\n\n{HEX_A}  asset.zip\n");
        assert!(lookup_checksum(&text, "asset.zip").is_ok());
    }
    #[test]
    fn lookup_checksum_returns_err_for_unknown_asset() {
        let text = format!("{HEX_A}  other.zip\n");
        assert!(lookup_checksum(&text, "missing.zip").is_err());
    }
    #[test]
    fn lookup_checksum_returns_err_for_malformed_hex() {
        let text = "not-hex-chars-here-just-text  asset.zip\n";
        let err = lookup_checksum(text, "asset.zip").unwrap_err();
        assert!(err.contains("malformed"), "got: {err}");
    }

    // ─── verify_sha256 ───
    #[test]
    fn verify_sha256_accepts_correct_hash() {
        let tmp = std::env::temp_dir().join(format!("rtk-test-{}.bin", std::process::id()));
        std::fs::write(&tmp, b"hello").unwrap();
        // sha256 of "hello":
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(&tmp, expected).is_ok());
        let _ = std::fs::remove_file(&tmp);
    }
    #[test]
    fn verify_sha256_rejects_mismatch() {
        let tmp =
            std::env::temp_dir().join(format!("rtk-test-mismatch-{}.bin", std::process::id()));
        std::fs::write(&tmp, b"hello").unwrap();
        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_sha256(&tmp, wrong).unwrap_err();
        assert!(err.contains("mismatch"), "got: {err}");
        let _ = std::fs::remove_file(&tmp);
    }
    #[test]
    fn verify_sha256_rejects_uppercase_hex_in_expected() {
        let tmp = std::env::temp_dir().join(format!("rtk-test-upper-{}.bin", std::process::id()));
        std::fs::write(&tmp, b"hello").unwrap();
        let upper = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        let err = verify_sha256(&tmp, upper).unwrap_err();
        assert!(err.contains("lowercase"), "got: {err}");
        let _ = std::fs::remove_file(&tmp);
    }

    // ─── validate_archive_entry_name ───
    #[test]
    fn validate_accepts_simple_name() {
        assert!(validate_archive_entry_name("rtk").is_ok());
        assert!(validate_archive_entry_name("bin/rtk").is_ok());
    }
    #[test]
    fn validate_rejects_absolute_unix() {
        assert!(validate_archive_entry_name("/etc/passwd").is_err());
    }
    #[test]
    fn validate_rejects_drive_prefix() {
        assert!(validate_archive_entry_name("C:\\Users\\evil").is_err());
        assert!(validate_archive_entry_name("C:/Users/evil").is_err());
    }
    #[test]
    fn validate_rejects_unc_path() {
        assert!(validate_archive_entry_name("\\\\server\\share\\evil").is_err());
    }
    #[test]
    fn validate_rejects_backslash_separator() {
        assert!(validate_archive_entry_name("..\\evil").is_err());
        assert!(validate_archive_entry_name("subdir\\rtk").is_err());
    }
    #[test]
    fn validate_rejects_parent_traversal() {
        assert!(validate_archive_entry_name("../evil").is_err());
        assert!(validate_archive_entry_name("a/../b").is_err());
    }
    #[test]
    fn validate_rejects_current_dir_component() {
        assert!(validate_archive_entry_name("./rtk").is_err());
    }
    #[test]
    fn validate_rejects_null_byte() {
        assert!(validate_archive_entry_name("a\0b").is_err());
    }
    #[test]
    fn validate_rejects_empty() {
        assert!(validate_archive_entry_name("").is_err());
    }

    // ─── identity check (via FakeCommandRunner.run_path) ───
    struct FakeRunner {
        path_responses: std::collections::HashMap<String, (bool, String)>,
        responses: std::collections::HashMap<String, (bool, String)>,
    }
    impl FakeRunner {
        fn new() -> Self {
            Self {
                path_responses: Default::default(),
                responses: Default::default(),
            }
        }
        fn set_path(&mut self, exe: &Path, args: &[&str], ok: bool, out: &str) {
            let key = format!("{} {}", exe.display(), args.join(" "));
            self.path_responses.insert(key, (ok, out.to_string()));
        }
        #[allow(dead_code)]
        fn set(&mut self, bin: &str, args: &[&str], ok: bool, out: &str) {
            let key = format!("{bin} {}", args.join(" "));
            self.responses.insert(key, (ok, out.to_string()));
        }
    }
    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            bin: &str,
            args: &[&str],
            _timeout: Duration,
        ) -> Result<(bool, String), String> {
            let key = format!("{bin} {}", args.join(" "));
            self.responses
                .get(&key)
                .cloned()
                .ok_or_else(|| format!("no fixture for `{key}`"))
        }
        fn run_path(
            &self,
            exe: &Path,
            args: &[&str],
            _timeout: Duration,
        ) -> Result<(bool, String), String> {
            let key = format!("{} {}", exe.display(), args.join(" "));
            self.path_responses
                .get(&key)
                .cloned()
                .ok_or_else(|| format!("no path-fixture for `{key}`"))
        }
    }

    #[test]
    fn identity_check_accepts_rtk_ai_banner() {
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "rtk-ai/rtk 0.42.0\n");
        assert!(rtk_identity_check(&r, &path).is_ok());
    }
    #[test]
    fn identity_check_accepts_full_product_name() {
        let path = PathBuf::from("/opt/homebrew/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "Rust Token Killer v0.42.0\n");
        assert!(rtk_identity_check(&r, &path).is_ok());
    }
    #[test]
    fn identity_check_rejects_unknown_rtk() {
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "rtk - Rust Type Kit 1.0\n");
        let err = rtk_identity_check(&r, &path).unwrap_err();
        assert!(err.contains("identity not confirmed"), "got: {err}");
    }
    #[test]
    fn identity_check_rejects_when_version_fails() {
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], false, "");
        assert!(rtk_identity_check(&r, &path).is_err());
    }

    // ─── v0.20.2: marker path A short-circuits (no `gain` fixture needed) ───
    #[test]
    fn identity_check_accepts_marker_without_gain() {
        // Marker present → short-circuit. Test deliberately provides NO `gain --help`
        // fixture: if the implementation accidentally probes `gain`, FakeRunner would
        // error and rtk_identity_check would fail.
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "rtk-ai/rtk 0.42.0\n");
        // NOTE: no `gain --help` fixture — must not be called.
        assert!(rtk_identity_check(&r, &path).is_ok());
    }

    #[test]
    fn identity_check_accepts_full_product_name_without_gain() {
        let path = PathBuf::from("/opt/homebrew/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "Rust Token Killer v0.42.0\n");
        // NOTE: no `gain --help` fixture.
        assert!(rtk_identity_check(&r, &path).is_ok());
    }

    // ─── v0.20.2: fallback path B (banner shape + `gain --help` exit 0) ───
    #[test]
    fn identity_check_accepts_plausible_banner_and_gain() {
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "rtk 0.40.0\n");
        r.set_path(&path, &["gain", "--help"], true, "Show token savings\n");
        assert!(rtk_identity_check(&r, &path).is_ok());
    }

    #[test]
    fn identity_check_accepts_v_prefix_banner_and_gain() {
        // rtk-ai also ships `rtk v0.4.2`-style banners (per existing cli_update test).
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "rtk v0.4.2\n");
        r.set_path(&path, &["gain", "--help"], true, "Show token savings\n");
        assert!(rtk_identity_check(&r, &path).is_ok());
    }

    #[test]
    fn identity_check_rejects_plausible_banner_when_gain_fails() {
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "rtk 0.40.0\n");
        r.set_path(&path, &["gain", "--help"], false, "unrecognized subcommand");
        let err = rtk_identity_check(&r, &path).unwrap_err();
        assert!(err.contains("identity not confirmed"), "got: {err}");
    }

    #[test]
    fn identity_check_rejects_unrelated_banner_even_with_gain() {
        // Banner shape fails → `gain` not probed even though we provide it.
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "Rust Type Kit 1.0\n");
        r.set_path(&path, &["gain", "--help"], true, "fake gain output");
        let err = rtk_identity_check(&r, &path).unwrap_err();
        assert!(err.contains("identity not confirmed"), "got: {err}");
    }

    #[test]
    fn identity_check_rejects_rust_type_kit_with_gain_subcommand() {
        // The specific collision case Codex flagged: an unrelated `rtk`-named tool
        // (Rust Type Kit) that happens to have a `gain` subcommand. Banner-shape
        // check rejects because "- Rust Type Kit 1.0" is not `X.Y.Z`-shaped.
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(&path, &["--version"], true, "rtk - Rust Type Kit 1.0\n");
        r.set_path(&path, &["gain", "--help"], true, "fake gain subcommand");
        let err = rtk_identity_check(&r, &path).unwrap_err();
        assert!(err.contains("identity not confirmed"), "got: {err}");
    }

    #[test]
    fn identity_check_rejects_implausible_banner_without_probing_gain() {
        // Banner doesn't match `rtk [v]X.Y.Z`. `gain --help` MUST NOT be probed —
        // proved by NOT providing a fixture: if the helper called it, FakeRunner
        // would return an Err that bubbles back as a different error string. We
        // assert the error is the regular identity-not-confirmed one (which means
        // gain was never called; otherwise the error would be the missing-fixture form).
        let path = PathBuf::from("/usr/local/bin/rtk");
        let mut r = FakeRunner::new();
        r.set_path(
            &path,
            &["--version"],
            true,
            "completely-different-tool 1.0\n",
        );
        // NOTE: no `gain --help` fixture.
        let err = rtk_identity_check(&r, &path).unwrap_err();
        assert!(
            err.contains("identity not confirmed") && !err.contains("no path-fixture"),
            "expected identity-not-confirmed (gain unprobed), got: {err}"
        );
    }

    // ─── v0.20.2: banner_looks_like_rtk helper ───
    #[test]
    fn banner_looks_like_rtk_accepts_plain_semver() {
        assert!(banner_looks_like_rtk("rtk 0.40.0"));
        assert!(banner_looks_like_rtk("rtk 99.99.99"));
        assert!(banner_looks_like_rtk("rtk 1.2.3\n"));
    }

    #[test]
    fn banner_looks_like_rtk_accepts_v_prefix() {
        assert!(banner_looks_like_rtk("rtk v0.4.2"));
        assert!(banner_looks_like_rtk("rtk v1.0.0"));
    }

    #[test]
    fn banner_looks_like_rtk_accepts_trailing_whitespace() {
        // Helper trims the first line — surrounding whitespace is harmless.
        assert!(banner_looks_like_rtk("rtk 0.40.0 "));
    }

    #[test]
    fn banner_looks_like_rtk_accepts_leading_whitespace_on_first_line() {
        assert!(banner_looks_like_rtk("  rtk 0.40.0\n"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_trailing_garbage() {
        assert!(!banner_looks_like_rtk("rtk 1.2.3 garbage"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_trailing_rust_type_kit() {
        assert!(!banner_looks_like_rtk("rtk 1.2.3 - Rust Type Kit"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_prerelease() {
        assert!(!banner_looks_like_rtk("rtk 1.2.3-beta"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_malformed_dash() {
        assert!(!banner_looks_like_rtk("rtk 1.2.3-"));
        assert!(!banner_looks_like_rtk("rtk 1.2.3-%%%"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_extra_component() {
        assert!(!banner_looks_like_rtk("rtk 1.2.3.4"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_two_components() {
        assert!(!banner_looks_like_rtk("rtk 1.2"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_non_digit() {
        assert!(!banner_looks_like_rtk("rtk x.y.z"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_uppercase() {
        // Case-sensitive on the prefix — real banner is lowercase `rtk `.
        assert!(!banner_looks_like_rtk("RTK 0.40.0"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_no_space_after_rtk() {
        assert!(!banner_looks_like_rtk("rtkXYZ 0.40.0"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_substring_match() {
        // Must START with `rtk ` (after trim) — embedded `rtk X.Y.Z` doesn't count.
        assert!(!banner_looks_like_rtk("some random rtk 0.40.0"));
    }

    #[test]
    fn banner_looks_like_rtk_rejects_empty() {
        assert!(!banner_looks_like_rtk(""));
    }

    // ─── make_backup_path ───
    #[test]
    fn make_backup_path_is_in_same_dir() {
        let install = PathBuf::from("/tmp/install/rtk");
        let bk = make_backup_path(&install);
        assert_eq!(bk.parent(), install.parent());
        assert!(bk
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("rtk.old."));
    }

    // ─── archive extraction safety (real in-memory zip) ───
    fn write_zip(out: &Path, entries: &[(&str, &[u8])]) {
        use std::io::Write;
        let f = std::fs::File::create(out).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(body).unwrap();
        }
        zw.finish().unwrap();
    }

    fn tmp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "rtk-test-{}-{}-{}",
            label,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }
    /// Same as [`tmp_path`] but appends `.zip` so `extract_rtk_binary` dispatches
    /// to the zip branch. Avoids polluting the bare label with the extension.
    fn tmp_zip(label: &str) -> PathBuf {
        let mut p = tmp_path(label);
        p.set_extension("zip");
        p
    }

    #[test]
    fn extract_zip_rejects_path_traversal() {
        let zip_path = tmp_zip("trav");
        write_zip(&zip_path, &[("../evil", b"x")]);
        let out_dir = tmp_path("trav-out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let err = extract_rtk_binary(&zip_path, "rtk.exe", &out_dir).unwrap_err();
        assert!(
            err.contains("parent-dir") || err.contains("rejected"),
            "got: {err}"
        );
        let _ = std::fs::remove_file(&zip_path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    #[test]
    fn extract_zip_rejects_backslash_separator() {
        let zip_path = tmp_zip("bs");
        write_zip(&zip_path, &[("subdir\\rtk.exe", b"x")]);
        let out_dir = tmp_path("bs-out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let err = extract_rtk_binary(&zip_path, "rtk.exe", &out_dir).unwrap_err();
        assert!(
            err.contains("backslash") || err.contains("rejected"),
            "got: {err}"
        );
        let _ = std::fs::remove_file(&zip_path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    #[test]
    fn extract_zip_rejects_no_match() {
        let zip_path = tmp_zip("nomatch");
        write_zip(&zip_path, &[("README.md", b"x")]);
        let out_dir = tmp_path("nomatch-out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let err = extract_rtk_binary(&zip_path, "rtk.exe", &out_dir).unwrap_err();
        assert!(err.contains("no 'rtk.exe' entry"), "got: {err}");
        let _ = std::fs::remove_file(&zip_path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    #[test]
    fn extract_zip_rejects_multiple_matches() {
        let zip_path = tmp_zip("multi");
        write_zip(&zip_path, &[("rtk.exe", b"a"), ("subdir/rtk.exe", b"b")]);
        let out_dir = tmp_path("multi-out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let err = extract_rtk_binary(&zip_path, "rtk.exe", &out_dir).unwrap_err();
        assert!(err.contains("ambiguous"), "got: {err}");
        let _ = std::fs::remove_file(&zip_path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }
    #[test]
    fn extract_zip_extracts_single_match() {
        let zip_path = tmp_zip("single");
        let payload = b"fake-rtk-binary-bytes";
        write_zip(&zip_path, &[("rtk.exe", payload)]);
        let out_dir = tmp_path("single-out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let path = extract_rtk_binary(&zip_path, "rtk.exe", &out_dir).unwrap();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got, payload);
        let _ = std::fs::remove_file(&zip_path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    // ─── stage_swap_verify with FailingFsOps ───
    struct RecordingFs {
        events: std::sync::Mutex<Vec<String>>,
        fail_remove_install: bool,
        fail_restore: bool,
        prior_exists: bool,
        real: RealFsOps,
    }
    impl FsOps for RecordingFs {
        fn exists(&self, p: &Path) -> bool {
            self.events
                .lock()
                .unwrap()
                .push(format!("exists({})", p.display()));
            self.prior_exists
        }
        fn rename(&self, src: &Path, dst: &Path) -> Result<(), String> {
            self.events.lock().unwrap().push(format!(
                "rename({},{})",
                src.display(),
                dst.display()
            ));
            // Detect a restore-call shape: src is the backup, dst is the install.
            if self.fail_restore
                && src
                    .file_name()
                    .map(|n| n.to_string_lossy().contains(".old."))
                    .unwrap_or(false)
            {
                return Err("restore failed (simulated)".into());
            }
            self.real.rename(src, dst)
        }
        fn remove_file(&self, p: &Path) -> Result<(), String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("remove_file({})", p.display()));
            if self.fail_remove_install {
                return Err("remove failed (simulated)".into());
            }
            self.real.remove_file(p)
        }
        fn copy(&self, src: &Path, dst: &Path) -> Result<(), String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("copy({},{})", src.display(), dst.display()));
            self.real.copy(src, dst)
        }
    }

    #[test]
    fn stage_swap_loud_when_identity_remove_also_fails() {
        let tmp = tmp_path("swap-bad");
        std::fs::create_dir_all(&tmp).unwrap();
        let install = tmp.join("rtk");
        let extracted = tmp.join("extracted-rtk");
        std::fs::write(&extracted, b"fake-bytes").unwrap();
        // Set up FakeRunner that says "wrong identity" on the post-install path.
        let mut runner = FakeRunner::new();
        runner.set_path(&install, &["--version"], true, "Rust Type Kit\n");
        let fs = RecordingFs {
            events: Default::default(),
            fail_remove_install: true,
            fail_restore: false,
            prior_exists: false,
            real: RealFsOps,
        };
        let err = stage_swap_verify(&install, &extracted, &runner, &fs).unwrap_err();
        assert!(err.contains("post-install identity failed"), "got: {err}");
        assert!(err.contains("BAD BINARY REMAINS"), "got: {err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ─── v0.22.0: install_or_update_native_with_progress refusal paths ───
    // These tests verify the function correctly refuses non-native targets
    // BEFORE any download/io. We cannot run the full happy-path in unit tests
    // because it shells out to `gh` and mutates a real install location.

    /// PathResolver that says rtk is at a brew-shaped path (triggers Brew detection).
    struct BrewLikePathResolver;
    impl PathResolver for BrewLikePathResolver {
        fn find(&self, name: &str) -> Result<PathBuf, String> {
            if name == "rtk" {
                Ok(PathBuf::from("/opt/homebrew/bin/rtk"))
            } else {
                Err(format!("not found: {name}"))
            }
        }
    }

    /// PathResolver that says rtk isn't installed (triggers NotInstalled).
    struct MissingPathResolver;
    impl PathResolver for MissingPathResolver {
        fn find(&self, _name: &str) -> Result<PathBuf, String> {
            Err("not found".into())
        }
    }

    /// Stub Downloader/FsOps for tests that should fail BEFORE any download.
    struct UnreachableDownloader;
    impl ReleaseDownloader for UnreachableDownloader {
        fn download(&self, _: &str, _: &str, _: &str, _: &Path) -> Result<(), String> {
            panic!("downloader must not be called for refusal-path tests")
        }
    }
    struct UnreachableFs;
    impl FsOps for UnreachableFs {
        fn exists(&self, _p: &Path) -> bool {
            panic!("fs must not be called for refusal-path tests")
        }
        fn rename(&self, _s: &Path, _d: &Path) -> Result<(), String> {
            panic!("fs must not be called")
        }
        fn remove_file(&self, _p: &Path) -> Result<(), String> {
            panic!("fs must not be called")
        }
        fn copy(&self, _s: &Path, _d: &Path) -> Result<(), String> {
            panic!("fs must not be called")
        }
    }

    fn noop_stage(_s: &str) {}

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn install_or_update_native_refuses_brew_target() {
        // BrewLikePathResolver + FakeRunner whose `brew list rtk` confirms brew →
        // detect_target returns Brew. Native variant must refuse.
        let mut runner = FakeRunner::new();
        let path = PathBuf::from("/opt/homebrew/bin/rtk");
        runner.set_path(&path, &["--version"], true, "rtk-ai/rtk 0.42.0\n");
        runner.set("brew", &["list", "rtk"], true, "/opt/homebrew/bin/rtk\n");
        let result = install_or_update_native_with_progress(
            &runner,
            &BrewLikePathResolver,
            &UnreachableDownloader,
            &UnreachableFs,
            InstallOpts {
                yes: true,
                allow_fresh_install: false,
            },
            &noop_stage,
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("not a native install"),
            "expected brew refusal, got: {err}"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn install_or_update_native_refuses_not_installed_when_no_fresh_install() {
        let runner = FakeRunner::new();
        let result = install_or_update_native_with_progress(
            &runner,
            &MissingPathResolver,
            &UnreachableDownloader,
            &UnreachableFs,
            InstallOpts {
                yes: true,
                allow_fresh_install: false,
            },
            &noop_stage,
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("rtk is not installed"),
            "expected 'not installed' error, got: {err}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn install_or_update_native_refuses_on_linux() {
        // Linux returns Unsupported from detect_target.
        let runner = FakeRunner::new();
        let result = install_or_update_native_with_progress(
            &runner,
            &MissingPathResolver,
            &UnreachableDownloader,
            &UnreachableFs,
            InstallOpts {
                yes: true,
                allow_fresh_install: true,
            },
            &noop_stage,
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("unsupported on this platform"),
            "expected unsupported, got: {err}"
        );
    }

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn install_or_update_native_emits_first_stage_before_target_detect() {
        // Captures that on_stage IS called at least once with "resolving target"
        // before any error path returns. Uses BrewLikePathResolver so we trip
        // the refusal AFTER the first stage emit.
        let mut runner = FakeRunner::new();
        let path = PathBuf::from("/opt/homebrew/bin/rtk");
        runner.set_path(&path, &["--version"], true, "rtk-ai/rtk 0.42.0\n");
        runner.set("brew", &["list", "rtk"], true, "/opt/homebrew/bin/rtk\n");
        let stages = std::sync::Mutex::new(Vec::<String>::new());
        let cb = |s: &str| stages.lock().unwrap().push(s.to_string());
        let _ = install_or_update_native_with_progress(
            &runner,
            &BrewLikePathResolver,
            &UnreachableDownloader,
            &UnreachableFs,
            InstallOpts {
                yes: true,
                allow_fresh_install: false,
            },
            &cb,
        );
        let s = stages.lock().unwrap().clone();
        assert_eq!(s, vec!["resolving target".to_string()]);
    }
}
