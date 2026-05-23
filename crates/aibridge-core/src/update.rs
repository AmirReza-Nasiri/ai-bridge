//! `aibridge update` support (Phase 2a: read-only check + install metadata).
//!
//! Update channel = GitHub Releases, reached through the `gh` CLI (shell-out) so
//! we add NO HTTP/TLS/zip crates and reuse `gh`'s private-repo auth (Codex-vetted
//! release-first design). This module is READ-ONLY: it reports current vs latest;
//! the actual download/replace lands in Phase 2c.

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
                    "UPDATE AVAILABLE: {current} → {l}. (Automatic install isn't in this build \
                     yet — rebuild from source / re-run the installer for now.)"
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
}
