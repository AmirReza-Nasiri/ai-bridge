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

/// Run a prebuilt `Command` with a hard timeout, capturing stdout/stderr. Reader
/// threads drain the pipes (so a chatty child can't deadlock on a full pipe), the
/// child is polled to the deadline, and killed if it overruns. The caller is
/// responsible for setting program/args; this helper adds null stdin, piped
/// stdout/stderr, sane env (NO_COLOR, GH_PROMPT_DISABLED so `gh` can't prompt),
/// and the Windows no-console flag. Used by both `update.rs` (legacy text-arg
/// shortcut) and `cli_update.rs` (which builds Commands via the platform layer
/// to handle Windows `.cmd` shims correctly).
pub(crate) fn run_command_with_timeout(
    mut builder: Command,
    timeout: Duration,
) -> Result<std::process::Output, std::io::Error> {
    builder
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

/// Thin wrapper around [`run_command_with_timeout`] for the common case where the
/// program is on PATH and there's no Windows-shim concern (e.g. `gh`, `curl`).
/// New code that may need to invoke `.cmd`/`.bat` shims (npm, npx) should resolve
/// via the platform layer and call `run_command_with_timeout` directly.
fn run_with_timeout(
    prog: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<std::process::Output, std::io::Error> {
    let mut builder = Command::new(prog);
    builder.args(args);
    run_command_with_timeout(builder, timeout)
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

pub(crate) fn global_dir() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".ai-bridge"))
}

fn metadata_path() -> Option<PathBuf> {
    Some(global_dir()?.join("install.json"))
}

/// v0.29 (B1): testable core — record install provenance into the EXACT `dir` given
/// (`dir/install.json`), NEVER `global_dir()`/`metadata_path()`, so a temp-dir caller
/// can't touch the real `~/.ai-bridge/install.json`. Best-effort.
pub(crate) fn record_install_in(dir: &Path, install_path: &str) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let meta = serde_json::json!({
        "install_path": install_path,
        "repo": repo_slug(),
        "channel": "stable",
        "version": env!("CARGO_PKG_VERSION"),
        "git_sha": crate::GIT_SHA,
    });
    let _ = std::fs::write(
        dir.join("install.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );
}

/// Record install provenance into the REAL (`global_dir()`) metadata. Called by `init`
/// with the running exe path. Best-effort. Production wrapper over [`record_install_in`].
pub fn record_install(install_path: &str) {
    let Some(dir) = global_dir() else { return };
    record_install_in(&dir, install_path);
}

/// Pure parser for the `install_path` field of install.json. Separated so the raw
/// value extraction is unit-testable without touching the filesystem.
pub(crate) fn parse_recorded_install_path(json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(json).ok()?;
    v.get("install_path")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The recorded install path, if metadata exists and is valid. Returns the RAW
/// stored value (doctor surfaces it verbatim); trust-filtering happens only in
/// [`resolve_install_path`].
pub fn recorded_install_path() -> Option<String> {
    let raw = std::fs::read_to_string(metadata_path()?).ok()?;
    parse_recorded_install_path(&raw)
}

/// v0.28: a recorded install path is trustworthy only if it still EXISTS and is NOT
/// under the OS temp dir. Guards against a stale path recorded by a one-off run from
/// a temp / staged-test location silently winning over the real installed binary
/// (the macOS self-update-to-a-dead-temp-path bug). Pure → unit-testable.
pub(crate) fn install_path_is_trustworthy(path: &Path, temp_dir: &Path, exists: bool) -> bool {
    exists && !path_starts_with_ci(path, temp_dir)
}

/// Component-wise prefix check. Case-INSENSITIVE on Windows — paths there are
/// case-insensitive, so the case-sensitive `Path::starts_with` would miss `c:\temp`
/// vs `C:\Temp` and wrongly trust a temp path; case-sensitive elsewhere. Component-wise
/// (not string-prefix) so `/tmpfoo` is NOT treated as under `/tmp`.
///
/// Limitation: Windows verbatim (`\\?\`) / UNC prefix variants are NOT normalized, so a
/// recorded path and the temp root spelled with different prefix forms wouldn't match.
/// In practice recorded install paths come from `current_exe()`/`init` in normal form,
/// and the existence check in [`install_path_is_trustworthy`] is the primary guard;
/// full prefix normalization is a deliberate non-goal here.
fn path_starts_with_ci(path: &Path, prefix: &Path) -> bool {
    #[cfg(windows)]
    {
        let comps = |p: &Path| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                .collect::<Vec<_>>()
        };
        let (p, pre) = (comps(path), comps(prefix));
        pre.len() <= p.len() && p[..pre.len()] == pre[..]
    }
    #[cfg(not(windows))]
    {
        path.starts_with(prefix)
    }
}

/// v0.28: pure precedence resolver — a trustworthy recorded path wins; otherwise fall
/// back to the running executable. Testable seam for [`resolve_install_path`] so the
/// recorded-vs-current_exe precedence (and the trust filter) is provable without I/O.
pub(crate) fn resolve_install_path_with(
    recorded: Option<String>,
    current_exe: Option<String>,
    temp_dir: &Path,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<String> {
    if let Some(r) = recorded {
        let p = Path::new(&r);
        if install_path_is_trustworthy(p, temp_dir, exists(p)) {
            return Some(r);
        }
    }
    current_exe
}

/// Resolve the binary path to act on: a TRUSTWORTHY recorded metadata path →
/// `current_exe()`. v0.28: a recorded path that no longer exists or lives under the
/// OS temp dir is ignored (falls back to the running binary) so self-update can never
/// target a stale temp location.
pub fn resolve_install_path() -> Option<String> {
    resolve_install_path_with(
        recorded_install_path(),
        std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string()),
        &std::env::temp_dir(),
        &|p| p.exists(),
    )
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
pub(crate) fn verify_sha256(bin: &Path, sha_file: &Path) -> Result<(), String> {
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

/// v0.25.0: Download the release `asset` + its `.sha256` for `tag` into `dst_dir`
/// via `gh release download`, verify the checksum, and return the verified binary
/// path. Shared by the existing CLI apply path's sibling AND the staged-update flow
/// (`staged_update.rs`) so both fetch + verify identically. Does NOT stage/replace.
pub(crate) fn download_and_verify_asset(tag: &str, dst_dir: &Path) -> Result<PathBuf, String> {
    let asset = asset_name();
    let sha_asset = format!("{asset}.sha256");
    let slug = repo_slug();
    let dl = run_with_timeout(
        "gh",
        &[
            "release",
            "download",
            tag,
            "--repo",
            slug,
            "--pattern",
            &asset,
            "--pattern",
            &sha_asset,
            "--dir",
            &dst_dir.display().to_string(),
        ],
        Duration::from_secs(180),
    );
    match dl {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let why = String::from_utf8_lossy(&o.stderr);
            let why = why
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("gh download failed");
            return Err(format!(
                "downloading {asset} from {tag} failed: {}",
                why.trim()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("update needs the GitHub CLI (`gh`).".to_string());
        }
        Err(e) => return Err(format!("download failed: {e}")),
    }
    let dl_bin = dst_dir.join(&asset);
    let dl_sha = dst_dir.join(&sha_asset);
    if !dl_bin.exists() || !dl_sha.exists() {
        return Err(format!(
            "download incomplete (missing {asset} or its .sha256)"
        ));
    }
    verify_sha256(&dl_bin, &dl_sha).map_err(|e| format!("refusing to install — {e}"))?;
    Ok(dl_bin)
}

/// Pure consent decision. Non-TTY ALWAYS declines — a piped/redirected stdin must
/// not be able to auto-consent (callers use `--yes` / `assume_yes` for scripted
/// flows). On a TTY, only a trimmed `y`/`yes` (case-insensitive) consents; a line
/// that wasn't read (`None`: EOF/error) declines. Extracted so the decision is
/// unit-testable without touching real stdin.
fn confirm_line_is_yes(is_tty: bool, line: Option<&str>) -> bool {
    if !is_tty {
        return false;
    }
    match line {
        None => false,
        Some(l) => matches!(l.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

/// Prompt y/N on stdin; non-interactive (not a TTY) / EOF / anything but y|yes →
/// false. v0.24.0: fail closed when stdin is not a terminal BEFORE prompting/reading
/// — a piped `yes` must not pass the gate (rtk install/update + self-update consent).
fn confirm(prompt: &str) -> bool {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return false; // non-interactive → decline (use --yes / assume_yes instead)
    }
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => false, // EOF / read error → decline
        Ok(_) => confirm_line_is_yes(true, Some(&line)),
    }
}

/// RAII guard for a temp directory: the path is created on `new()` and removed
/// by `Drop` unless explicitly `disarm`-ed. This replaces the v0.20.0 pattern of
/// scattered `let _ = std::fs::remove_dir_all(&tmp)` lines across every error
/// path — a maintenance hazard (reviewer F2). The guard cleans up on panic too.
pub(crate) struct TempDirGuard {
    path: Option<PathBuf>,
}

impl TempDirGuard {
    pub fn new(label: &str) -> Result<Self, String> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("aibridge-{}-{}-{}", label, std::process::id(), ts));
        std::fs::create_dir_all(&dir).map_err(|e| format!("can't make temp dir: {e}"))?;
        Ok(Self { path: Some(dir) })
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref().expect("path before disarm")
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Replace the installed binary with the verified `staged` file.
/// Unix: atomic rename (same dir → same filesystem). The running process keeps its
/// old inode. Windows: rename the in-use exe aside (allowed for open-for-execute
/// files), move the new one into place, best-effort delete the `.old`. On any
/// failure the staged file is LEFT in place (never a half-written install).
pub(crate) fn replace_binary(install: &Path, staged: &Path) -> Result<String, String> {
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

// ───────────────────────── plan/apply split (v0.20.0 hotfix) ─────────────────────────

/// A complete, validated update plan ready to apply. Constructed only by
/// [`plan_update`] (or its injection variant). `apply_planned_update` accepts
/// only this struct — compile-time impossible to call apply with a Skip decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedUpdate {
    pub install_path: PathBuf,
    pub tag: String,
    pub from: Option<Version>,
    pub to: Version,
}

/// Result of [`plan_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDecision {
    /// No update needed. `reason` is user-facing.
    Skip { reason: String },
    /// An update IS available and valid. Hand to `apply_planned_update`.
    Apply(PlannedUpdate),
}

/// Cleanup mode for stale-process handling. Computed by `decide_cleanup_mode`
/// from the `--yes`, `--close-stale`, and TTY flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupMode {
    /// Interactive TTY default — ask the user `[y/N]`.
    Prompt,
    /// CLI passed `--close-stale` (and possibly `--yes`) — auto-close without asking.
    AutoClose,
    /// Non-TTY without `--close-stale` — refuse with a helpful error.
    RefuseNonInteractive,
}

/// Pure helper: decide cleanup mode from flags + TTY. Tested via table-driven cases.
pub fn decide_cleanup_mode(yes: bool, close_stale: bool, is_tty: bool) -> CleanupMode {
    if close_stale {
        CleanupMode::AutoClose
    } else if is_tty {
        CleanupMode::Prompt
    } else if yes {
        // --yes alone in non-TTY: still refuse (can't get explicit cleanup consent).
        CleanupMode::RefuseNonInteractive
    } else {
        CleanupMode::RefuseNonInteractive
    }
}

/// Confirmation seam. Production uses `RealConfirmer` (reads stdin); tests use
/// `FakeConfirmer` (predetermined responses).
pub trait Confirmer: Send + Sync {
    fn confirm(&self, prompt: &str) -> bool;
    fn confirm_close_pids(&self, install: &Path, pids: &[u32]) -> bool;
}

pub struct RealConfirmer;
impl Confirmer for RealConfirmer {
    fn confirm(&self, prompt: &str) -> bool {
        confirm(prompt)
    }
    fn confirm_close_pids(&self, install: &Path, pids: &[u32]) -> bool {
        println!(
            "Found {} stale aibridge process(es) at {}:",
            pids.len(),
            install.display()
        );
        for pid in pids {
            println!("  PID {pid}");
        }
        confirm("Close these and apply the update? [y/N] ")
    }
}

pub struct AlwaysYesConfirmer;
impl Confirmer for AlwaysYesConfirmer {
    fn confirm(&self, _: &str) -> bool {
        true
    }
    fn confirm_close_pids(&self, _: &Path, _: &[u32]) -> bool {
        true
    }
}

/// Orchestration options for [`orchestrate_update`].
pub struct OrchestrationOpts {
    /// `true` when the update was already confirmed (e.g. TUI user pressed `u`,
    /// or CLI `--yes`). Skips the "Update X → Y? [y/N]" prompt.
    pub update_already_confirmed: bool,
    /// What to do when stale aibridge processes hold the install path.
    pub cleanup_mode: CleanupMode,
}

/// Phase-2 orchestrator: takes a precomputed `UpdateDecision`, runs the
/// update-confirmation gate (unless already confirmed), enumerates + kills (or
/// declines) stale processes, then invokes `apply_fn`. ALL I/O is injected.
pub fn orchestrate_update(
    decision: UpdateDecision,
    opts: OrchestrationOpts,
    enumerator: &dyn crate::process_cleanup::ProcessEnumerator,
    killer: &dyn crate::process_cleanup::ProcessKiller,
    confirmer: &dyn Confirmer,
    apply_fn: &dyn Fn(PlannedUpdate) -> Result<String, String>,
) -> Result<String, String> {
    let planned = match decision {
        UpdateDecision::Skip { reason } => return Ok(reason),
        UpdateDecision::Apply(p) => p,
    };
    // 1. Update-confirmation gate (skipped if already confirmed).
    if !opts.update_already_confirmed {
        let from_s = planned
            .from
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into());
        if !confirmer.confirm(&format!("Update {from_s} → {to}? [y/N] ", to = planned.to)) {
            return Ok("Update cancelled.".to_string());
        }
    }
    // 2. Stale-process enumeration.
    let all_stale = enumerator
        .list_aibridge()
        .map_err(|e| format!("process enumeration failed: {e}"))?;
    let target = crate::process_cleanup::select_stale_processes(
        &all_stale,
        &planned.install_path,
        std::process::id(),
        crate::process_cleanup::parent_pid(),
    );
    if !target.is_empty() {
        // 3. Cleanup gate.
        match opts.cleanup_mode {
            CleanupMode::RefuseNonInteractive => {
                let pids: Vec<u32> = target.iter().map(|p| p.pid).collect();
                return Err(format!(
                    "{} stale aibridge process(es) at {} (PIDs {:?}); \
                     re-run with `--close-stale` (or in an interactive shell)",
                    target.len(),
                    planned.install_path.display(),
                    pids
                ));
            }
            CleanupMode::Prompt => {
                let pids: Vec<u32> = target.iter().map(|p| p.pid).collect();
                if !confirmer.confirm_close_pids(&planned.install_path, &pids) {
                    return Ok("Cleanup declined; update cancelled.".to_string());
                }
            }
            CleanupMode::AutoClose => { /* proceed */ }
        }
        // 4. Kill.
        let report =
            crate::process_cleanup::kill_stale_processes(&target, &planned.install_path, killer);
        if !report.failed.is_empty() {
            return Err(format!(
                "couldn't terminate {} stale aibridge process(es): {:?}",
                report.failed.len(),
                report.failed
            ));
        }
    }
    // 5. Apply.
    apply_fn(planned)
}

/// Pure planner intermediate: a successful Apply WITHOUT the install_path yet.
/// Lets us validate assets BEFORE doing install-path resolution I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InitialPlan {
    Skip {
        reason: String,
    },
    Apply {
        tag: String,
        from: Option<Version>,
        to: Version,
    },
}

/// Pure planner: from a release-lookup result + current version, return Skip or
/// Apply-without-install-path. Validates that the release has THIS platform's
/// asset + checksum sidecar BEFORE returning Apply.
fn plan_from_lookup(
    lookup: ReleaseLookup,
    current: Option<Version>,
) -> Result<InitialPlan, String> {
    let (tag, assets) = match lookup {
        ReleaseLookup::Found { tag, assets } => (tag, assets),
        ReleaseLookup::None => {
            return Ok(InitialPlan::Skip {
                reason: "No published releases yet — nothing to update to.".to_string(),
            })
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
    if let Some(c) = current {
        if latest <= c {
            return Ok(InitialPlan::Skip {
                reason: format!("Already on the latest release ({c})."),
            });
        }
    }
    // Asset validation: refuse to plan an Apply if this platform's asset or its
    // checksum sidecar is missing from the release. Prevents "kill stale procs
    // then fail at download" UX.
    let need_bin = asset_name();
    let need_sha = format!("{need_bin}.sha256");
    if !assets.iter().any(|a| a == &need_bin) {
        return Err(format!(
            "release {tag} has no asset '{need_bin}' for this platform; available: {assets:?}"
        ));
    }
    if !assets.iter().any(|a| a == &need_sha) {
        return Err(format!(
            "release {tag} is missing checksum sidecar '{need_sha}'; refusing to update"
        ));
    }
    Ok(InitialPlan::Apply {
        tag,
        from: current,
        to: latest,
    })
}

/// Phase-1 planner WITH injected release lookup. Tests pass a closure that
/// panics if called (proves no network for `--from-source` short-circuit).
pub fn plan_update_with_lookup(
    opts: &ApplyOptions,
    lookup_fn: impl FnOnce(Duration) -> ReleaseLookup,
) -> Result<UpdateDecision, String> {
    // 1. --from-source: short-circuit BEFORE any I/O.
    if opts.from_source {
        return Err(
            "`--from-source` isn't implemented yet — for now update by rebuilding: \
             `git pull && cargo build --release`, then copy the binary onto your PATH."
                .to_string(),
        );
    }
    // 2. Network lookup.
    let lookup = lookup_fn(Duration::from_secs(20));
    // 3. Pure planner (asset validation included).
    let initial = plan_from_lookup(lookup, current_version())?;
    // 4. If Skip → no install_path resolution needed.
    let (tag, from, to) = match initial {
        InitialPlan::Skip { reason } => return Ok(UpdateDecision::Skip { reason }),
        InitialPlan::Apply { tag, from, to } => (tag, from, to),
    };
    // 5. NOW resolve install_path (honors --target).
    let install = opts
        .target_path
        .clone()
        .or_else(resolve_install_path)
        .ok_or("couldn't resolve which binary to replace")?;
    Ok(UpdateDecision::Apply(PlannedUpdate {
        install_path: PathBuf::from(install),
        tag,
        from,
        to,
    }))
}

/// Production phase-1 planner — uses real `latest_release`.
pub fn plan_update(opts: &ApplyOptions) -> Result<UpdateDecision, String> {
    plan_update_with_lookup(opts, latest_release)
}

/// Phase-2 applier. Takes a validated `PlannedUpdate` (no Skip possible).
/// Retains an abort-only stale-process guard as a safety net — caller should
/// have already closed same-path processes via the orchestrator, but a race
/// between cleanup and apply (or a direct call bypassing the orchestrator)
/// could leave one alive. The guard never kills — just aborts cleanly.
pub fn apply_planned_update(planned: PlannedUpdate) -> Result<String, String> {
    apply_planned_update_with_enumerator(planned, &crate::process_cleanup::RealProcessEnumerator)
}

/// Variant of [`apply_planned_update`] with an injectable `ProcessEnumerator`.
/// Tests use a `FakeEnumerator` to assert fail-closed semantics; production
/// goes through the wrapper above. v0.20.1 Codex Stop-gate R3 B4.
pub fn apply_planned_update_with_enumerator(
    planned: PlannedUpdate,
    enumerator: &dyn crate::process_cleanup::ProcessEnumerator,
) -> Result<String, String> {
    let install_path = planned.install_path.clone();
    let tag = planned.tag.clone();
    let latest = planned.to;
    let cur_str = planned
        .from
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    let install_dir = install_path
        .parent()
        .ok_or("install path has no parent directory")?;

    // Safety net: refuse if same-path stale processes remain. v0.20.1 R3 (reviewer
    // F7): fail CLOSED on enumeration error — the prior `if let Ok(stale) = ...`
    // silently allowed the update through on sysinfo errors.
    let stale =
        crate::process_cleanup::ProcessEnumerator::list_aibridge(enumerator).map_err(|e| {
            format!(
                "can't verify stale aibridge processes ({e}); \
                 refusing to update — re-run after the process table is readable"
            )
        })?;
    let target = crate::process_cleanup::select_stale_processes(
        &stale,
        &install_path,
        std::process::id(),
        crate::process_cleanup::parent_pid(),
    );
    if !target.is_empty() {
        let pids: Vec<u32> = target.iter().map(|p| p.pid).collect();
        return Err(format!(
            "race detected: {} stale aibridge process(es) at {} (PIDs {:?}); \
             close them and re-run",
            target.len(),
            install_path.display(),
            pids
        ));
    }

    // Download the asset + its checksum into a fresh temp dir. v0.20.1 reviewer F2:
    // RAII guard removes the dir on any error path INCLUDING panics — no more
    // scattered `let _ = std::fs::remove_dir_all(&tmp)` lines.
    let tmp_guard = TempDirGuard::new("update")?;
    let tmp = tmp_guard.path().to_path_buf();
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
            return Err("update needs the GitHub CLI (`gh`).".to_string());
        }
        Err(e) => {
            return Err(format!("download failed: {e}"));
        }
    }

    let dl_bin = tmp.join(&asset);
    let dl_sha = tmp.join(&sha_asset);
    if !dl_bin.exists() || !dl_sha.exists() {
        return Err(format!(
            "download incomplete (missing {asset} or its .sha256)"
        ));
    }
    if let Err(e) = verify_sha256(&dl_bin, &dl_sha) {
        return Err(format!("refusing to install — {e}"));
    }

    // Stage in the install DIR (same filesystem as the target) so the swap is atomic.
    let staged = install_dir.join(format!("{}.new", asset_filename(&install_path)));
    if let Err(e) = std::fs::copy(&dl_bin, &staged) {
        return Err(format!("couldn't stage the new binary: {e}"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }

    let note = replace_binary(&install_path, &staged)?;
    // tmp_guard drops here and removes the temp dir.

    // Record the now-installed version (the tag we just placed), not the old one.
    let install_str = install_path.display().to_string();
    write_installed_meta(&install_str, &latest.to_string());

    Ok(format!(
        "Updated {cur_str} → {latest} at {install_str} — {note}"
    ))
}

/// Backward-compat wrapper around the v0.20.0 plan/apply split. Used by tests +
/// any caller that doesn't want to drive the orchestrator manually. Preserves
/// the historical update-confirmation prompt (`assume_yes=false` → asks
/// `[y/N]`); the inline stale-process guard remains in `apply_planned_update`.
pub fn apply_update(opts: ApplyOptions) -> Result<String, String> {
    apply_update_with(opts, latest_release, &RealConfirmer, apply_planned_update)
}

/// Injectable variant of [`apply_update`]. Tests use `FakeLookup` /
/// `FakeConfirmer` / fake `apply_fn` to prove ordering + no-network behavior
/// for the `from_source` / `assume_yes` paths.
pub fn apply_update_with(
    opts: ApplyOptions,
    lookup_fn: impl FnOnce(Duration) -> ReleaseLookup,
    confirmer: &dyn Confirmer,
    apply_fn: impl FnOnce(PlannedUpdate) -> Result<String, String>,
) -> Result<String, String> {
    // 1. --from-source short-circuit (no network, no fs).
    if opts.from_source {
        return Err(
            "`--from-source` isn't implemented yet — for now update by rebuilding: \
             `git pull && cargo build --release`, then copy the binary onto your PATH."
                .to_string(),
        );
    }
    // 2. Plan via injected lookup.
    let decision = plan_update_with_lookup(&opts, lookup_fn)?;
    let planned = match decision {
        UpdateDecision::Skip { reason } => return Ok(reason),
        UpdateDecision::Apply(p) => p,
    };
    // 3. Update-confirmation gate (PRESERVED from v0.19.0 behavior).
    if !opts.assume_yes {
        let from_s = planned
            .from
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into());
        if !confirmer.confirm(&format!("Update {from_s} → {to}? [y/N] ", to = planned.to)) {
            return Ok("Update cancelled.".to_string());
        }
    }
    // 4. Apply.
    apply_fn(planned)
}

/// The install binary's own file name (e.g. `aibridge.exe`), for staging `<name>.new`.
fn asset_filename(install: &Path) -> String {
    install
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("aibridge")
        .to_string()
}

/// v0.29 (B1): testable core — write install metadata into the EXACT `dir` given
/// (`dir/install.json`), NEVER `global_dir()`/`metadata_path()`. So a caller that
/// passes a temp dir (the staged-apply tests) can never corrupt the real
/// `~/.ai-bridge/install.json`. Best-effort. Records the INSTALLED tag version (not
/// the running/old binary's); `git_sha` is "unknown" (we only know the tag here).
pub(crate) fn write_installed_meta_in(dir: &Path, install_path: &str, version: &str) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let meta = serde_json::json!({
        "install_path": install_path,
        "repo": repo_slug(),
        "channel": "stable",
        "version": version,
        "git_sha": "unknown",
    });
    let _ = std::fs::write(
        dir.join("install.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );
}

/// Update the REAL (`global_dir()`) install metadata after a successful replace.
/// Production wrapper over [`write_installed_meta_in`].
pub(crate) fn write_installed_meta(install_path: &str, version: &str) {
    let Some(dir) = global_dir() else { return };
    write_installed_meta_in(&dir, install_path, version);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── v0.28: install-path resolution (stale/temp recorded path must not win) ───
    #[test]
    fn parse_recorded_install_path_returns_raw_value() {
        let json = r#"{"install_path":"/Users/x/.cargo/bin/aibridge","version":"0.27.0"}"#;
        assert_eq!(
            parse_recorded_install_path(json).as_deref(),
            Some("/Users/x/.cargo/bin/aibridge")
        );
        assert_eq!(parse_recorded_install_path("not json"), None);
        assert_eq!(parse_recorded_install_path(r#"{"other":1}"#), None);
    }

    #[test]
    fn install_path_trustworthy_only_when_exists_and_not_temp() {
        let tmp = Path::new("/tmp");
        assert!(install_path_is_trustworthy(
            Path::new("/usr/local/bin/aibridge"),
            tmp,
            true
        ));
        // under temp → never trustworthy, even if it exists
        assert!(!install_path_is_trustworthy(
            Path::new("/tmp/aibridge-staged-test-1/bin/aibridge"),
            tmp,
            true
        ));
        // missing → never trustworthy
        assert!(!install_path_is_trustworthy(
            Path::new("/usr/local/bin/aibridge"),
            tmp,
            false
        ));
        // component-wise (NOT string prefix): /tmpfoo is NOT under /tmp → trustworthy
        assert!(install_path_is_trustworthy(
            Path::new("/tmpfoo/aibridge"),
            tmp,
            true
        ));
    }

    #[cfg(windows)]
    #[test]
    fn install_path_temp_check_is_case_insensitive_on_windows() {
        let temp = Path::new(r"C:\Temp");
        // different casing of the same temp root → still under-temp ⇒ NOT trustworthy
        assert!(!install_path_is_trustworthy(
            Path::new(r"c:\temp\aibridge.exe"),
            temp,
            true
        ));
        // partial-name sibling must NOT be a false prefix ⇒ trustworthy
        assert!(install_path_is_trustworthy(
            Path::new(r"c:\tempfoo\aibridge.exe"),
            temp,
            true
        ));
    }

    #[test]
    fn resolve_install_path_precedence_valid_recorded_wins() {
        // Recorded path exists and is not under temp → it wins over current_exe.
        let got = resolve_install_path_with(
            Some("/opt/aibridge".to_string()),
            Some("/proc/self/exe-current".to_string()),
            Path::new("/tmp"),
            &|_p| true, // everything "exists"
        );
        assert_eq!(got.as_deref(), Some("/opt/aibridge"));
    }

    #[test]
    fn resolve_install_path_falls_back_when_recorded_missing_on_disk() {
        // Recorded path does NOT exist → fall back to current_exe.
        let got = resolve_install_path_with(
            Some("/gone/aibridge".to_string()),
            Some("/real/current/aibridge".to_string()),
            Path::new("/tmp"),
            &|p| p != Path::new("/gone/aibridge"),
        );
        assert_eq!(got.as_deref(), Some("/real/current/aibridge"));
    }

    #[test]
    fn resolve_install_path_falls_back_when_recorded_under_temp() {
        // The exact bug: recorded path is a stale temp/staged-test dir → ignore it.
        let got = resolve_install_path_with(
            Some("/tmp/aibridge-staged-test-9/bin/aibridge".to_string()),
            Some("/Users/x/.cargo/bin/aibridge".to_string()),
            Path::new("/tmp"),
            &|_p| true, // even if the temp path still exists
        );
        assert_eq!(got.as_deref(), Some("/Users/x/.cargo/bin/aibridge"));
    }

    #[test]
    fn resolve_install_path_uses_current_exe_when_no_recorded() {
        let got = resolve_install_path_with(
            None,
            Some("/real/current/aibridge".to_string()),
            Path::new("/tmp"),
            &|_p| true,
        );
        assert_eq!(got.as_deref(), Some("/real/current/aibridge"));
    }

    // ─── v0.29 (B1): metadata writers must hit the GIVEN dir, never global_dir() ───
    fn b1_tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "aibridge-b1-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|t| t.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_installed_meta_in_writes_only_to_given_dir() {
        let dir = b1_tmp_dir("wim");
        write_installed_meta_in(&dir, "/some/install/aibridge", "9.9.9");
        let raw = std::fs::read_to_string(dir.join("install.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["install_path"], "/some/install/aibridge");
        assert_eq!(v["version"], "9.9.9");
        // round-trips through the pure parser
        assert_eq!(
            parse_recorded_install_path(&raw).as_deref(),
            Some("/some/install/aibridge")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_install_in_writes_only_to_given_dir() {
        let dir = b1_tmp_dir("rec");
        record_install_in(&dir, "/some/install/aibridge");
        let raw = std::fs::read_to_string(dir.join("install.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["install_path"], "/some/install/aibridge");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── v0.24.0: confirmation must fail closed on non-TTY ───
    #[test]
    fn confirm_non_tty_always_declines() {
        // A piped/redirected stdin must NOT auto-consent, even with "yes".
        assert!(!confirm_line_is_yes(false, Some("yes")));
        assert!(!confirm_line_is_yes(false, Some("y")));
        assert!(!confirm_line_is_yes(false, Some("yes\n")));
        assert!(!confirm_line_is_yes(false, None));
    }

    #[test]
    fn confirm_tty_accepts_y_yes_only() {
        assert!(confirm_line_is_yes(true, Some("y\n")));
        assert!(confirm_line_is_yes(true, Some("yes\n")));
        assert!(confirm_line_is_yes(true, Some("  YES  ")));
        assert!(!confirm_line_is_yes(true, Some("no\n")));
        assert!(!confirm_line_is_yes(true, Some("")));
        assert!(!confirm_line_is_yes(true, None));
    }

    // ─── plan/apply split (v0.20.0 hotfix) ───

    fn fake_lookup_found(tag: &str, assets: Vec<&str>) -> ReleaseLookup {
        ReleaseLookup::Found {
            tag: tag.to_string(),
            assets: assets.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn plan_from_lookup_skip_when_no_release() {
        let r = plan_from_lookup(ReleaseLookup::None, parse_version("1.0.0")).unwrap();
        assert!(matches!(r, InitialPlan::Skip { .. }));
    }

    #[test]
    fn plan_from_lookup_skip_when_already_latest() {
        let r = plan_from_lookup(
            fake_lookup_found(
                "0.5.0",
                vec![&asset_name(), &format!("{}.sha256", asset_name())],
            ),
            parse_version("0.5.0"),
        )
        .unwrap();
        assert!(matches!(r, InitialPlan::Skip { .. }));
    }

    #[test]
    fn plan_from_lookup_apply_when_newer_with_assets() {
        let r = plan_from_lookup(
            fake_lookup_found(
                "9.9.9",
                vec![&asset_name(), &format!("{}.sha256", asset_name())],
            ),
            parse_version("0.5.0"),
        )
        .unwrap();
        assert!(matches!(r, InitialPlan::Apply { .. }));
    }

    #[test]
    fn plan_from_lookup_err_when_platform_asset_missing() {
        let r = plan_from_lookup(
            fake_lookup_found("9.9.9", vec!["aibridge-unrelated-target"]),
            parse_version("0.5.0"),
        );
        assert!(r.is_err(), "got: {r:?}");
    }

    #[test]
    fn plan_from_lookup_err_when_checksum_missing() {
        let r = plan_from_lookup(
            fake_lookup_found("9.9.9", vec![&asset_name()]),
            parse_version("0.5.0"),
        );
        assert!(r.is_err(), "expected missing-checksum error, got: {r:?}");
    }

    #[test]
    fn plan_from_lookup_err_when_gh_missing() {
        assert!(plan_from_lookup(ReleaseLookup::GhMissing, None).is_err());
    }

    #[test]
    fn plan_update_with_lookup_from_source_short_circuits_without_calling_lookup() {
        let opts = ApplyOptions {
            assume_yes: true,
            from_source: true,
            target_path: None,
        };
        let r = plan_update_with_lookup(&opts, |_| {
            panic!("lookup must NOT be called when --from-source is set");
        });
        assert!(r.is_err(), "got: {r:?}");
        assert!(r.unwrap_err().contains("from-source"));
    }

    #[test]
    fn apply_update_with_from_source_short_circuits_without_calling_lookup_or_apply() {
        let opts = ApplyOptions {
            assume_yes: true,
            from_source: true,
            target_path: None,
        };
        let r = apply_update_with(
            opts,
            |_| panic!("lookup must NOT be called"),
            &AlwaysYesConfirmer,
            |_| panic!("apply must NOT be called"),
        );
        assert!(r.is_err(), "got: {r:?}");
    }

    struct DeclineConfirmer;
    impl Confirmer for DeclineConfirmer {
        fn confirm(&self, _: &str) -> bool {
            false
        }
        fn confirm_close_pids(&self, _: &Path, _: &[u32]) -> bool {
            false
        }
    }

    #[test]
    fn apply_update_with_assume_yes_false_decline_does_not_apply() {
        // Lookup returns newer with proper assets; confirmer DECLINES; apply must NOT be called.
        let opts = ApplyOptions {
            assume_yes: false,
            from_source: false,
            target_path: Some("/tmp/test-aibridge".to_string()),
        };
        let r = apply_update_with(
            opts,
            |_| {
                fake_lookup_found(
                    "99.0.0",
                    vec![&asset_name(), &format!("{}.sha256", asset_name())],
                )
            },
            &DeclineConfirmer,
            |_| panic!("apply must NOT be called when user declines"),
        );
        let s = r.unwrap();
        assert!(s.contains("cancelled"), "got: {s}");
    }

    #[test]
    fn apply_update_with_assume_yes_true_skips_confirm_and_applies() {
        let opts = ApplyOptions {
            assume_yes: true,
            from_source: false,
            target_path: Some("/tmp/test-aibridge".to_string()),
        };
        let r = apply_update_with(
            opts,
            |_| {
                fake_lookup_found(
                    "99.0.0",
                    vec![&asset_name(), &format!("{}.sha256", asset_name())],
                )
            },
            &DeclineConfirmer, // would decline, but assume_yes skips it
            |_| Ok("applied-by-test".to_string()),
        );
        assert_eq!(r.unwrap(), "applied-by-test");
    }

    #[test]
    fn plan_update_with_lookup_honors_target_path() {
        let opts = ApplyOptions {
            assume_yes: true,
            from_source: false,
            target_path: Some("/tmp/override-aibridge".to_string()),
        };
        let r = plan_update_with_lookup(&opts, |_| {
            fake_lookup_found(
                "99.0.0",
                vec![&asset_name(), &format!("{}.sha256", asset_name())],
            )
        })
        .unwrap();
        match r {
            UpdateDecision::Apply(p) => {
                assert_eq!(p.install_path, PathBuf::from("/tmp/override-aibridge"));
            }
            other => panic!("expected Apply, got {other:?}"),
        }
    }

    // ─── decide_cleanup_mode ───
    #[test]
    fn decide_cleanup_mode_close_stale_wins_always() {
        assert_eq!(
            decide_cleanup_mode(true, true, true),
            CleanupMode::AutoClose
        );
        assert_eq!(
            decide_cleanup_mode(false, true, false),
            CleanupMode::AutoClose
        );
    }

    #[test]
    fn decide_cleanup_mode_tty_prompts_by_default() {
        assert_eq!(decide_cleanup_mode(false, false, true), CleanupMode::Prompt);
        assert_eq!(decide_cleanup_mode(true, false, true), CleanupMode::Prompt);
    }

    #[test]
    fn decide_cleanup_mode_non_tty_refuses_without_close_stale() {
        assert_eq!(
            decide_cleanup_mode(false, false, false),
            CleanupMode::RefuseNonInteractive
        );
        assert_eq!(
            decide_cleanup_mode(true, false, false),
            CleanupMode::RefuseNonInteractive
        );
    }

    // ─── orchestrate_update ───
    use crate::process_cleanup as pc;

    struct EmptyEnumerator;
    impl pc::ProcessEnumerator for EmptyEnumerator {
        fn list_aibridge(&self) -> Result<Vec<pc::StaleProcess>, String> {
            Ok(Vec::new())
        }
    }

    struct StaleEnumerator {
        stale: Vec<pc::StaleProcess>,
    }
    impl pc::ProcessEnumerator for StaleEnumerator {
        fn list_aibridge(&self) -> Result<Vec<pc::StaleProcess>, String> {
            Ok(self.stale.clone())
        }
    }

    struct NoopKiller;
    impl pc::ProcessKiller for NoopKiller {
        fn kill_one(&self, _: &pc::StaleProcess, _: &Path) -> pc::KillOutcome {
            pc::KillOutcome::TerminatedGracefully
        }
    }

    fn fake_plan(install: &str) -> UpdateDecision {
        UpdateDecision::Apply(PlannedUpdate {
            install_path: PathBuf::from(install),
            tag: "v9.9.9".into(),
            from: parse_version("0.1.0"),
            to: parse_version("9.9.9").unwrap(),
        })
    }

    #[test]
    fn orchestrate_skip_returns_reason_without_calling_apply() {
        let decision = UpdateDecision::Skip {
            reason: "Already on the latest release (0.5.0).".to_string(),
        };
        let r = orchestrate_update(
            decision,
            OrchestrationOpts {
                update_already_confirmed: true,
                cleanup_mode: CleanupMode::Prompt,
            },
            &EmptyEnumerator,
            &NoopKiller,
            &AlwaysYesConfirmer,
            &|_| panic!("apply must NOT be called for Skip"),
        )
        .unwrap();
        assert!(r.contains("Already"));
    }

    #[test]
    fn orchestrate_no_stale_proceeds_to_apply() {
        let r = orchestrate_update(
            fake_plan("/tmp/aib"),
            OrchestrationOpts {
                update_already_confirmed: true,
                cleanup_mode: CleanupMode::Prompt,
            },
            &EmptyEnumerator,
            &NoopKiller,
            &AlwaysYesConfirmer,
            &|p| Ok(format!("applied at {}", p.install_path.display())),
        )
        .unwrap();
        assert!(r.contains("applied at /tmp/aib"));
    }

    #[test]
    fn orchestrate_update_confirmation_declined_cancels() {
        let r = orchestrate_update(
            fake_plan("/tmp/aib"),
            OrchestrationOpts {
                update_already_confirmed: false,
                cleanup_mode: CleanupMode::Prompt,
            },
            &EmptyEnumerator,
            &NoopKiller,
            &DeclineConfirmer, // declines BOTH prompts
            &|_| panic!("apply must NOT be called when update is declined"),
        )
        .unwrap();
        assert!(r.contains("cancelled"));
    }

    #[test]
    fn orchestrate_stale_refuse_non_interactive_aborts() {
        let stale = vec![pc::StaleProcess {
            pid: 999,
            exe_path: Some(PathBuf::from("/tmp/aib")),
            start_time_secs: Some(1),
        }];
        let r = orchestrate_update(
            fake_plan("/tmp/aib"),
            OrchestrationOpts {
                update_already_confirmed: true,
                cleanup_mode: CleanupMode::RefuseNonInteractive,
            },
            &StaleEnumerator { stale },
            &NoopKiller,
            &AlwaysYesConfirmer,
            &|_| panic!("apply must NOT be called when stale + non-interactive"),
        );
        let err = r.unwrap_err();
        assert!(
            err.contains("re-run with") || err.contains("close-stale"),
            "got: {err}"
        );
    }

    #[test]
    fn orchestrate_stale_cleanup_declined_cancels() {
        let stale = vec![pc::StaleProcess {
            pid: 999,
            exe_path: Some(PathBuf::from("/tmp/aib")),
            start_time_secs: Some(1),
        }];
        let r = orchestrate_update(
            fake_plan("/tmp/aib"),
            OrchestrationOpts {
                update_already_confirmed: true,
                cleanup_mode: CleanupMode::Prompt,
            },
            &StaleEnumerator { stale },
            &NoopKiller,
            &DeclineConfirmer, // declines the cleanup prompt
            &|_| panic!("apply must NOT be called when cleanup is declined"),
        )
        .unwrap();
        assert!(r.contains("Cleanup declined"));
    }

    #[test]
    fn orchestrate_stale_auto_close_kills_then_applies() {
        let stale = vec![pc::StaleProcess {
            pid: 999,
            exe_path: Some(PathBuf::from("/tmp/aib")),
            start_time_secs: Some(1),
        }];
        let r = orchestrate_update(
            fake_plan("/tmp/aib"),
            OrchestrationOpts {
                update_already_confirmed: true,
                cleanup_mode: CleanupMode::AutoClose,
            },
            &StaleEnumerator { stale },
            &NoopKiller, // returns TerminatedGracefully for all
            &AlwaysYesConfirmer,
            &|_| Ok("applied-after-cleanup".to_string()),
        )
        .unwrap();
        assert!(r.contains("applied-after-cleanup"));
    }

    // ─── v0.20.1 TempDirGuard + apply_planned_update_with_enumerator ───
    #[test]
    fn tempdir_guard_removes_on_drop() {
        let path = {
            let g = TempDirGuard::new("test-guard").expect("guard");
            let p = g.path().to_path_buf();
            assert!(p.exists(), "guard path must exist while held");
            p
        }; // guard dropped here
        assert!(!path.exists(), "guard must remove dir on drop");
    }

    struct FailingEnumerator {
        msg: &'static str,
    }
    impl crate::process_cleanup::ProcessEnumerator for FailingEnumerator {
        fn list_aibridge(&self) -> Result<Vec<crate::process_cleanup::StaleProcess>, String> {
            Err(self.msg.to_string())
        }
    }

    #[test]
    fn apply_planned_update_with_enumerator_aborts_on_enumeration_error() {
        let p = PlannedUpdate {
            install_path: PathBuf::from("/tmp/aibridge-test-target"),
            tag: "v99.0.0".into(),
            from: parse_version("0.20.1"),
            to: parse_version("99.0.0").unwrap(),
        };
        let enumer = FailingEnumerator {
            msg: "simulated permission denied",
        };
        let r = apply_planned_update_with_enumerator(p, &enumer);
        assert!(r.is_err());
        let err = r.unwrap_err();
        assert!(
            err.contains("refusing to update") && err.contains("simulated permission denied"),
            "expected fail-closed error message, got: {err}"
        );
    }

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
