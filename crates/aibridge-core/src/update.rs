//! `aibridge update`: check for and install a newer release.
//!
//! Update channel = GitHub Releases, reached through the `gh` CLI (shell-out) so
//! we add no HTTP/TLS crates and reuse `gh`'s private-repo auth (Codex-vetted
//! release-first design). `update --check` / `doctor --check-updates` are
//! read-only; `update` downloads the matching `aibridge-<target>[.exe]` asset,
//! verifies its sha256 (the one new dep), and replaces the installed binary
//! (atomic rename on Unix; rename-aside on Windows, fail-safe with the new binary
//! left staged on error). `--from-source` is reserved (not implemented yet).

use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// `owner/repo` slug parsed from `CARGO_PKG_REPOSITORY` (the Cargo.toml
/// `repository` URL), e.g. `omega-do-it-solutions/ai-bridge`.
pub fn repo_slug() -> &'static str {
    // Computed at runtime but cheap; kept simple (no once_cell dep).
    const URL: &str = env!("CARGO_PKG_REPOSITORY");
    URL.trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit("github.com/")
        .next()
        .unwrap_or(URL)
}

/// A parsed `major.minor.patch` (stable releases only — prereleases are rejected).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse a tag/version into a stable `Version`. Strips a leading `v`, drops `+build`
/// metadata, and REJECTS prereleases (`-beta…`) and non-numeric/short forms — the
/// stable channel only compares clean `x.y.z`.
pub fn parse_version(raw: &str) -> Option<Version> {
    let s = raw.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let s = s.split('+').next().unwrap_or(s); // drop build metadata
    if s.contains('-') {
        return None; // prerelease — not a stable release
    }
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None; // more than three components
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// The compiled-in version of THIS binary (from Cargo.toml via `CARGO_PKG_VERSION`).
pub fn current_version() -> Option<Version> {
    parse_version(env!("CARGO_PKG_VERSION"))
}

/// Outcome of looking up the latest GitHub release via `gh`.
pub enum ReleaseLookup {
    /// A latest stable release was found.
    Found { tag: String, assets: Vec<String> },
    /// The repo has no published (non-draft, non-prerelease) release yet.
    None,
    /// `gh` isn't installed.
    GhMissing,
    /// `gh` ran but failed (auth/network/etc.) — message is a short reason.
    Failed(String),
    /// `gh` exceeded the deadline.
    Timeout,
}

/// Run a command with a hard timeout, capturing stdout/stderr. Reader threads
/// drain the pipes (so a chatty child can't deadlock on a full pipe), the child
/// is polled to the deadline, and killed if it overruns. `gh` is run with prompts
/// disabled so it can never block waiting for interactive input.
fn run_with_timeout(
    prog: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, std::io::Error> {
    let mut builder = Command::new(prog);
    builder
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        builder.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash
    }
    let mut child = builder.spawn()?;

    let mut so = child.stdout.take().expect("piped stdout");
    let mut se = child.stderr.take().expect("piped stderr");
    let (otx, orx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        let _ = otx.send(b);
    });
    thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        let _ = etx.send(b);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let stdout = orx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
                let stderr = erx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "command timed out",
                    ));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Look up the latest stable release for the configured repo via
/// `gh api repos/<slug>/releases/latest` (which already excludes drafts and
/// prereleases). All failure modes are mapped to a non-fatal variant.
pub fn latest_release(timeout: Duration) -> ReleaseLookup {
    let endpoint = format!("repos/{}/releases/latest", repo_slug());
    let out = match run_with_timeout("gh", &["api", &endpoint], timeout) {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReleaseLookup::GhMissing,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => return ReleaseLookup::Timeout,
        Err(e) => return ReleaseLookup::Failed(e.to_string()),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // A 404 on releases/latest is ambiguous for a PRIVATE repo: it's either
        // "no releases yet" OR "this gh auth can't see the repo". Disambiguate with
        // a cheap repo-accessibility probe so an auth/access failure isn't reported
        // as "no releases" (Codex review).
        if err.contains("404") || err.contains("Not Found") {
            let slug = repo_slug();
            return match run_with_timeout("gh", &["api", &format!("repos/{slug}")], timeout) {
                Ok(o) if o.status.success() => ReleaseLookup::None, // repo visible → truly no release
                Ok(_) => ReleaseLookup::Failed(
                    "repo not accessible — check `gh auth status` and that the account can see it"
                        .to_string(),
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => ReleaseLookup::GhMissing,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => ReleaseLookup::Timeout,
                Err(e) => ReleaseLookup::Failed(e.to_string()),
            };
        }
        let reason = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("gh failed");
        return ReleaseLookup::Failed(reason.trim().to_string());
    }
    let json: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(e) => return ReleaseLookup::Failed(format!("invalid gh JSON: {e}")),
    };
    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if tag.is_empty() {
        return ReleaseLookup::Failed("release JSON had no tag_name".to_string());
    }
    let assets = json
        .get("assets")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ReleaseLookup::Found { tag, assets }
}

/// A human-readable `update --check` / `doctor --check-updates` result.
pub fn check_report(timeout: Duration) -> String {
    let current = env!("CARGO_PKG_VERSION");
    match latest_release(timeout) {
        ReleaseLookup::Found { tag, assets } => {
            let latest = parse_version(&tag);
            let cur = current_version();
            let verdict = match (cur, latest) {
                (Some(c), Some(l)) if l > c => format!(
                    "UPDATE AVAILABLE: {current} → {l}. Run `aibridge update` to install."
                ),
                (Some(c), Some(l)) if l == c => "You're on the latest release.".to_string(),
                (Some(_), Some(l)) => format!(
                    "You're ahead of the latest release ({l}) — likely a dev build."
                ),
                (_, None) => format!(
                    "Latest release tag '{tag}' isn't clean stable semver — can't compare."
                ),
                (None, _) => "Couldn't parse the current version.".to_string(),
            };
            let asset_note = if assets.is_empty() {
                "\n  (latest release has no downloadable assets yet)".to_string()
            } else {
                format!("\n  assets: {}", assets.join(", "))
            };
            format!("current: {current}\n  latest:  {tag}\n  {verdict}{asset_note}")
        }
        ReleaseLookup::None => format!(
            "current: {current}\n  No published releases yet on {} — nothing to update to.",
            repo_slug()
        ),
        ReleaseLookup::GhMissing => format!(
            "current: {current}\n  Update check needs the GitHub CLI (`gh`), which isn't installed. \
             Install it (https://cli.github.com) and `gh auth login`, or build from source."
        ),
        ReleaseLookup::Timeout => format!(
            "current: {current}\n  Update check timed out reaching GitHub."
        ),
        ReleaseLookup::Failed(why) => format!(
            "current: {current}\n  Update check couldn't reach GitHub: {why} \
             (check `gh auth status`)."
        ),
    }
}

// ── install metadata (~/.ai-bridge/install.json) ───────────────────────────────
//
// GLOBAL (per-binary), distinct from a project's `.ai-bridge/`. Records where the
// binary was installed so a later `update` replaces the RIGHT file. Optional: every
// reader falls back to `current_exe()` when it's missing (Codex-vetted chain).

fn global_dir() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".ai-bridge"))
}

fn metadata_path() -> Option<PathBuf> {
    Some(global_dir()?.join("install.json"))
}

/// Record install provenance. Called by `init` with the running exe path. Best-effort.
pub fn record_install(install_path: &str) {
    let Some(dir) = global_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let meta = serde_json::json!({
        "install_path": install_path,
        "repo": repo_slug(),
        "channel": "stable",
        "version": env!("CARGO_PKG_VERSION"),
        "git_sha": crate::GIT_SHA,
    });
    if let Some(p) = metadata_path() {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&meta).unwrap_or_default());
    }
}

/// The recorded install path, if metadata exists and is valid.
pub fn recorded_install_path() -> Option<String> {
    let raw = std::fs::read_to_string(metadata_path()?).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get("install_path")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Resolve the binary path to act on: recorded metadata → `current_exe()`.
pub fn resolve_install_path() -> Option<String> {
    recorded_install_path().or_else(|| {
        std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string())
    })
}

// ── apply (download + verify + replace) ────────────────────────────────────────

/// The target triple this binary was built for (from `build.rs`).
pub fn current_target() -> &'static str {
    env!("AIBRIDGE_TARGET")
}

fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

/// The release asset name for THIS platform, e.g. `aibridge-x86_64-pc-windows-msvc.exe`.
pub fn asset_name() -> String {
    format!("aibridge-{}{}", current_target(), exe_suffix())
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Verify a downloaded file against its `<name>.sha256` sidecar (the first token is
/// the hex digest, GNU coreutils format). Rejects empty files / malformed sums.
fn verify_sha256(bin: &Path, sha_file: &Path) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(bin).map_err(|e| format!("can't read download: {e}"))?;
    if bytes.is_empty() {
        return Err("downloaded binary is empty".to_string());
    }
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let got = hex_lower(&hasher.finalize());
    let txt = std::fs::read_to_string(sha_file).map_err(|e| format!("can't read checksum: {e}"))?;
    let want = txt.split_whitespace().next().unwrap_or("").to_lowercase();
    if want.len() != 64 || !want.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("checksum file is malformed".to_string());
    }
    if got != want {
        return Err(format!("checksum mismatch (got {got}, expected {want})"));
    }
    Ok(())
}

/// Prompt y/N on stdin; non-interactive / EOF / anything but y|yes → false.
fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => false, // EOF / non-interactive → decline
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

fn make_temp_dir() -> Result<PathBuf, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("aibridge-update-{}-{}", std::process::id(), ts));
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't make temp dir: {e}"))?;
    Ok(dir)
}

/// Replace the installed binary with the verified `staged` file.
/// Unix: atomic rename (same dir → same filesystem). The running process keeps its
/// old inode. Windows: rename the in-use exe aside (allowed for open-for-execute
/// files), move the new one into place, best-effort delete the `.old`. On any
/// failure the staged file is LEFT in place (never a half-written install).
fn replace_binary(install: &Path, staged: &Path) -> Result<String, String> {
    #[cfg(windows)]
    {
        // UNIQUE backup name (pid+nanos): a previous `.old` may still be locked by a
        // running old MCP, so a fixed name would block this update (Codex review).
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let fname = install
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("aibridge.exe");
        let old = install.with_file_name(format!("{fname}.old.{}.{ts}", std::process::id()));
        if install.exists() {
            std::fs::rename(install, &old).map_err(|e| {
                format!(
                    "couldn't move the current binary aside ({e}); new binary staged at {staged:?}"
                )
            })?;
        }
        if let Err(e) = std::fs::rename(staged, install) {
            // Roll back so we never leave the install path missing.
            let _ = std::fs::rename(&old, install);
            return Err(format!(
                "couldn't move the new binary into place ({e}); rolled back; \
                 new binary staged at {staged:?}, previous binary at {old:?}"
            ));
        }
        let _ = std::fs::remove_file(&old); // best-effort; a running process keeps it
        sweep_old_backups(install); // best-effort: clear deletable leftovers from past updates
        Ok("replaced (restart Claude Code / reopen your shell to use the new version)".to_string())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(staged, install)
            .map_err(|e| format!("couldn't replace the binary ({e}); staged at {staged:?}"))?;
        Ok("replaced (reopen your shell / restart Claude Code to use the new version)".to_string())
    }
}

/// Best-effort removal of leftover `<name>.old.*` backups from past Windows
/// updates (skips any still locked by a running old process).
#[cfg(windows)]
fn sweep_old_backups(install: &Path) {
    let (Some(dir), Some(fname)) = (
        install.parent(),
        install.file_name().and_then(|s| s.to_str()),
    ) else {
        return;
    };
    let prefix = format!("{fname}.old.");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Some(n) = e.file_name().to_str() {
                if n.starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Options for [`apply_update`].
pub struct ApplyOptions {
    pub assume_yes: bool,
    pub from_source: bool,
    /// Override the binary to replace (testing / unusual installs); else resolved.
    pub target_path: Option<String>,
}

/// Download the latest release's binary for this platform, verify it, and replace
/// the installed binary. Returns a user-facing message. `Err` is a clean,
/// actionable failure (the install is never left half-written).
pub fn apply_update(opts: ApplyOptions) -> Result<String, String> {
    if opts.from_source {
        return Err(
            "`--from-source` isn't implemented yet — for now update by rebuilding: \
                    `git pull && cargo build --release`, then copy the binary onto your PATH."
                .to_string(),
        );
    }

    let (tag, _assets) = match latest_release(Duration::from_secs(20)) {
        ReleaseLookup::Found { tag, assets } => (tag, assets),
        ReleaseLookup::None => {
            return Ok("No published releases yet — nothing to update to.".to_string())
        }
        ReleaseLookup::GhMissing => {
            return Err(
                "update needs the GitHub CLI (`gh`) — install it + `gh auth login`.".to_string(),
            )
        }
        ReleaseLookup::Timeout => return Err("timed out reaching GitHub.".to_string()),
        ReleaseLookup::Failed(w) => return Err(format!("couldn't reach GitHub: {w}")),
    };

    let latest = parse_version(&tag)
        .ok_or_else(|| format!("latest release tag '{tag}' isn't clean stable semver"))?;
    let current = current_version();
    if let Some(c) = current {
        if latest <= c {
            return Ok(format!("Already on the latest release ({c})."));
        }
    }
    let cur_str = current
        .map(|c| c.to_string())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    if !opts.assume_yes && !confirm(&format!("Update {cur_str} → {latest}? [y/N] ")) {
        return Ok("Update cancelled.".to_string());
    }

    let install = opts
        .target_path
        .clone()
        .or_else(resolve_install_path)
        .ok_or("couldn't resolve which binary to replace")?;
    let install_path = PathBuf::from(&install);
    let install_dir = install_path
        .parent()
        .ok_or("install path has no parent directory")?;

    // Download the asset + its checksum into a fresh temp dir (no --clobber needed).
    let tmp = make_temp_dir()?;
    let asset = asset_name();
    let sha_asset = format!("{asset}.sha256");
    let slug = repo_slug();
    let dl = run_with_timeout(
        "gh",
        &[
            "release",
            "download",
            &tag,
            "--repo",
            slug,
            "--pattern",
            &asset,
            "--pattern",
            &sha_asset,
            "--dir",
            &tmp.display().to_string(),
        ],
        Duration::from_secs(180),
    );
    match dl {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let _ = std::fs::remove_dir_all(&tmp);
            let why = String::from_utf8_lossy(&o.stderr);
            let why = why
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("gh download failed");
            return Err(format!(
                "downloading {asset} from {tag} failed: {} \
                 (no asset for this platform '{}'? )",
                why.trim(),
                current_target()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err("update needs the GitHub CLI (`gh`).".to_string());
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("download failed: {e}"));
        }
    }

    let dl_bin = tmp.join(&asset);
    let dl_sha = tmp.join(&sha_asset);
    if !dl_bin.exists() || !dl_sha.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!(
            "download incomplete (missing {asset} or its .sha256)"
        ));
    }
    if let Err(e) = verify_sha256(&dl_bin, &dl_sha) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("refusing to install — {e}"));
    }

    // Stage in the install DIR (same filesystem as the target) so the swap is atomic.
    let staged = install_dir.join(format!("{}.new", asset_filename(&install_path)));
    if let Err(e) = std::fs::copy(&dl_bin, &staged) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("couldn't stage the new binary: {e}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }

    let note = match replace_binary(&install_path, &staged) {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
    };
    let _ = std::fs::remove_dir_all(&tmp);

    // Record the now-installed version (the tag we just placed), not the old one.
    write_installed_meta(&install, &latest.to_string());

    Ok(format!(
        "Updated {cur_str} → {latest} at {install} — {note}"
    ))
}

/// The install binary's own file name (e.g. `aibridge.exe`), for staging `<name>.new`.
fn asset_filename(install: &Path) -> String {
    install
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("aibridge")
        .to_string()
}

/// Update install metadata after a successful replace: record the INSTALLED tag
/// version (not the running/old binary's), don't carry the old git SHA.
fn write_installed_meta(install_path: &str, version: &str) {
    let Some(dir) = global_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let meta = serde_json::json!({
        "install_path": install_path,
        "repo": repo_slug(),
        "channel": "stable",
        "version": version,
        "git_sha": "unknown",
    });
    if let Some(p) = metadata_path() {
        let _ = std::fs::write(p, serde_json::to_string_pretty(&meta).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clean_stable_versions() {
        assert_eq!(
            parse_version("0.2.0"),
            Some(Version {
                major: 0,
                minor: 2,
                patch: 0
            })
        );
        assert_eq!(
            parse_version("v1.4.10"),
            Some(Version {
                major: 1,
                minor: 4,
                patch: 10
            })
        );
        assert_eq!(
            parse_version("v2.0.0+build.7"),
            Some(Version {
                major: 2,
                minor: 0,
                patch: 0
            })
        );
    }

    #[test]
    fn rejects_prerelease_and_malformed() {
        assert_eq!(parse_version("0.3.0-beta.1"), None); // prerelease
        assert_eq!(parse_version("v0.3"), None); // too few components
        assert_eq!(parse_version("0.3.0.1"), None); // too many
        assert_eq!(parse_version("release-0.3.0"), None);
        assert_eq!(parse_version("abc"), None);
    }

    #[test]
    fn version_ordering_drives_update_decision() {
        let a = parse_version("0.2.0").unwrap();
        let b = parse_version("0.2.1").unwrap();
        let c = parse_version("0.10.0").unwrap();
        assert!(b > a);
        assert!(c > b); // numeric compare, not lexical (10 > 2)
        assert_eq!(a, parse_version("v0.2.0").unwrap());
    }

    #[test]
    fn repo_slug_is_owner_repo() {
        // Derived from Cargo.toml's repository URL.
        assert_eq!(repo_slug(), "omega-do-it-solutions/ai-bridge");
    }

    #[test]
    fn hex_lower_formats_bytes() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xab, 0xff]), "000fabff");
    }

    #[test]
    fn asset_name_has_target_and_platform_ext() {
        let n = asset_name();
        assert!(n.starts_with("aibridge-"), "{n}");
        assert!(n.contains(current_target()), "{n}");
        #[cfg(windows)]
        assert!(n.ends_with(".exe"), "{n}");
        #[cfg(not(windows))]
        assert!(!n.ends_with(".exe"), "{n}");
    }

    #[test]
    fn sha256_verify_accepts_match_rejects_mismatch_and_malformed() {
        let dir = std::env::temp_dir().join(format!("aibridge-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("f.bin");
        std::fs::write(&bin, b"abc").unwrap(); // sha256("abc") is well-known
        let good = dir.join("good.sha256");
        std::fs::write(
            &good,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  f.bin\n",
        )
        .unwrap();
        assert!(verify_sha256(&bin, &good).is_ok());

        let mism = dir.join("mism.sha256");
        std::fs::write(
            &mism,
            "0000000000000000000000000000000000000000000000000000000000000000  f.bin",
        )
        .unwrap();
        assert!(verify_sha256(&bin, &mism).is_err());

        let malformed = dir.join("bad.sha256");
        std::fs::write(&malformed, "not-a-hash").unwrap();
        assert!(verify_sha256(&bin, &malformed).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
