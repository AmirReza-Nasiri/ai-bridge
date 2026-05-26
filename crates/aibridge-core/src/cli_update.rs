//! CLI-update awareness for tools that AI Bridge depends on (codex, claude, rtk)
//! plus read-only detection of MCP server version pins. Surfaces in:
//!
//! - `aibridge update` CLI flow (after the self-update step).
//! - The TUI Update tab (rows for each tool + a paragraph for MCP pins).
//! - The Debug tab (current-only, no network calls).
//!
//! ## Detection vs. mutation
//!
//! These are intentionally separate execution paths (Codex Stop-gate R6):
//! - **Detection** uses the [`CommandRunner`] seam — short timeout, captured
//!   stdout, parseable. [`RealCommandRunner`] resolves via the platform layer so
//!   Windows `.cmd` shims (npm, npx) work correctly. Mockable in tests via
//!   `FakeCommandRunner`.
//! - **Mutation** (`brew upgrade`, `npm i -g`) goes through [`apply_cli_update`]
//!   which spawns with INHERITED stdio (so the user sees real-time progress and
//!   can answer brew/sudo prompts) and no aggressive timeout. Never mocked in
//!   unit tests; only the chosen [`Action`] is asserted.
//!
//! ## Sources and their channels
//!
//! - **Brew**: `brew info --json=v2 <pkg>` → JSON parsed in Rust (`serde_json`).
//! - **Npm**: `npm view <pkg> version` → newline-stripped version line.
//! - **GitHub native**: `gh api repos/<slug>/releases/latest --jq .tag_name`.
//! - **Native installer** (claude, rtk on Windows): no auto-update — docs URL.
//! - **Cargo**: `cargo search` is NOT authoritative — treated as Unknown to
//!   prevent false update prompts.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::update::{parse_version, run_command_with_timeout, Version};
use aibridge_platform::{DefaultPlatform, Platform};

// ───────────────────────── public types ─────────────────────────

/// Where a CLI binary was installed from — drives the latest-version channel and
/// the update command we'd suggest. `Unknown` means "we can't make a safe
/// suggestion"; the caller renders `manual_note` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallSource {
    /// Homebrew formula. Latest via `brew info --json=v2 <pkg>`. Update via
    /// `brew upgrade <pkg>`.
    Brew { package: String },
    /// Global npm install. Latest via `npm view <pkg> version`. Update via
    /// `npm i -g <pkg>@latest`.
    Npm { package: String },
    /// Cargo crate. Latest is NOT looked up (`cargo search` isn't authoritative).
    Cargo,
    /// Native installer (claude on macOS, rtk binary on Windows). No upstream
    /// channel we'd trust to compare against; `docs_url` points the user at the
    /// official download / instructions.
    NativeInstaller { docs_url: String },
    /// Path heuristic was ambiguous or verification failed. Caller renders
    /// `manual_note`.
    Unknown { path: PathBuf, reason: String },
}

impl InstallSource {
    /// `true` only for sources we're confident about and would auto-run a
    /// well-known update command for. `--yes` SKIPS rows that return false.
    pub fn is_verified_pkg_manager(&self) -> bool {
        matches!(self, InstallSource::Brew { .. } | InstallSource::Npm { .. })
    }
    /// Short human-friendly label for tables/logs.
    pub fn label(&self) -> &'static str {
        match self {
            InstallSource::Brew { .. } => "brew",
            InstallSource::Npm { .. } => "npm",
            InstallSource::Cargo => "cargo",
            InstallSource::NativeInstaller { .. } => "native-installer",
            InstallSource::Unknown { .. } => "unknown",
        }
    }
}

/// One row of the CLI-update table.
#[derive(Debug, Clone)]
pub struct CliCheck {
    pub tool: &'static str,
    pub current: Option<Version>,
    pub latest: Option<Version>,
    pub source: InstallSource,
    /// `argv` for [`apply_cli_update`] — `Some` only for verified sources with a
    /// well-known update command. `None` triggers manual-only UX.
    pub suggested_command: Option<Vec<String>>,
    /// One-line hint shown to the user when `suggested_command` is `None` or in
    /// the "would update" line for verified sources (e.g. `brew upgrade codex`).
    pub manual_note: Option<String>,
    /// `true` when the tool is missing-but-installable (e.g. `aibridge rtk install`).
    /// Overrides `up_to_date()` so a missing-installable surfaces as actionable.
    /// v0.20.0 Codex Stop-gate R3 B2.
    pub installable: bool,
}

impl CliCheck {
    pub fn up_to_date(&self) -> bool {
        if self.installable {
            // Missing-installable is NOT up-to-date — it needs action.
            return false;
        }
        match (&self.current, &self.latest) {
            (Some(c), Some(l)) => c >= l,
            // If either side is unknown, we can't claim up-to-date OR outdated;
            // render as "unknown".
            _ => true,
        }
    }

    /// `true` only when AI Bridge is willing to auto-run `suggested_command`
    /// under `--yes`. Two safe shapes (Codex R7):
    /// 1. Verified package-manager source (brew / npm) — `is_verified_pkg_manager`.
    /// 2. The EXACT internal command shape `[current_exe, "rtk", "install"|"update", "--yes"]`.
    ///    The `current_exe` requirement prevents a stale PATH `aibridge` from being
    ///    invoked instead of THIS running build.
    pub fn safe_to_auto_run(&self) -> bool {
        if self.source.is_verified_pkg_manager() {
            return true;
        }
        let Some(argv) = self.suggested_command.as_deref() else {
            return false;
        };
        // Strict 4-token shape: [<current_exe>, "rtk", "install"|"update", "--yes"].
        if argv.len() != 4 {
            return false;
        }
        let exe_matches = current_exe_path()
            .map(|cur| std::path::Path::new(&argv[0]) == cur.as_path())
            .unwrap_or(false);
        let verb_ok = argv[2] == "install" || argv[2] == "update";
        let yes_ok = argv[3] == "--yes";
        exe_matches && argv[1] == "rtk" && verb_ok && yes_ok
    }
}

/// Memoized `std::env::current_exe()` for `safe_to_auto_run` comparisons.
pub fn current_exe_path() -> Option<std::path::PathBuf> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    CACHE.get_or_init(|| std::env::current_exe().ok()).clone()
}

// ───────────────────────── PathResolver seam ─────────────────────────

/// Look up an executable on PATH. Production uses platform layer; tests inject
/// a fake to deterministically simulate installed/not-installed states.
pub trait PathResolver: Send + Sync {
    fn find(&self, name: &str) -> Result<std::path::PathBuf, String>;
}

/// Platform-layer-backed resolver.
pub struct RealPathResolver;

impl PathResolver for RealPathResolver {
    fn find(&self, name: &str) -> Result<std::path::PathBuf, String> {
        DefaultPlatform::find_executable(name).map_err(|e| e.to_string())
    }
}

/// One MCP-server version-pin observation (read-only — never auto-rewritten in
/// this PR). `version_pin == None` means the launch command auto-resolves to
/// latest at each spawn (e.g. `npx -y package` without `@<ver>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpVersionStatus {
    pub agent: &'static str, // "claude" or "codex"
    pub server_name: String,
    pub package: Option<String>,
    pub version_pin: Option<String>,
}

/// What the CLI's update flow should do for one row, given the current `Mode`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Skip with a printed message. Used for up-to-date, unknown latest, or
    /// manual-only sources under `--yes`.
    Skip { reason: String },
    /// Ask the user `[y/N]`. Pure-function chosen; the prompt itself is read at
    /// runtime by the CLI/TUI layer using [`prompt_parse`].
    Prompt { argv: Vec<String>, summary: String },
    /// Run the command unattended (only emitted under `Mode::YesAuto` AND
    /// verified-pkg-manager source).
    Run { argv: Vec<String>, summary: String },
}

/// How the update flow behaves overall. Computed by [`effective_mode`] from the
/// `--check` / `--yes` flags + stdin TTY-ness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Read-only; never mutates. Triggered by `--check` OR (no `--yes` AND
    /// stdin is not a TTY — non-TTY safety).
    Check,
    /// Interactive prompts default-no for each row.
    InteractiveTty,
    /// Unattended; auto-runs verified-pkg-manager rows, skips unknown/native.
    YesAuto,
}

// ───────────────────────── CommandRunner seam ─────────────────────────

/// Detection-time process runner. Captured stdout, short timeout. Implementors:
/// [`RealCommandRunner`] (platform-layer aware) and (test-only) fake.
pub trait CommandRunner: Send + Sync {
    fn run(&self, bin: &str, args: &[&str], timeout: Duration) -> Result<(bool, String), String>;
    /// Execute an ABSOLUTE-PATH binary (no PATH lookup). Returns
    /// `(success, stdout_then_stderr_combined)`. Default impl uses the platform
    /// layer's `command_for` + the shared `run_command_with_timeout`.
    /// rtk identity-check uses this (rtk prints its banner to stderr on some
    /// versions). v0.20.0 R7 B2.
    fn run_path(
        &self,
        exe: &std::path::Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<(bool, String), String> {
        let mut cmd = DefaultPlatform::command_for(exe);
        cmd.args(args);
        let out =
            run_command_with_timeout(cmd, timeout).map_err(|e| format!("spawn failed: {e}"))?;
        let mut combined = String::from_utf8_lossy(&out.stdout).to_string();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.success(), combined))
    }
}

/// Platform-layer-aware runner. Resolves the program via
/// [`DefaultPlatform::find_executable`] (so `.cmd`/`.bat` shims on Windows work)
/// then builds the `Command` via [`DefaultPlatform::command_for`].
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, bin: &str, args: &[&str], timeout: Duration) -> Result<(bool, String), String> {
        let exe =
            DefaultPlatform::find_executable(bin).map_err(|e| format!("{bin} not on PATH: {e}"))?;
        let mut cmd = DefaultPlatform::command_for(&exe);
        cmd.args(args);
        let out =
            run_command_with_timeout(cmd, timeout).map_err(|e| format!("{bin} failed: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        Ok((out.status.success(), stdout))
    }
}

// ───────────────────────── mutation path ─────────────────────────

/// Spawn a mutating package-manager command with INHERITED stdio so the user
/// sees real-time output and can answer interactive prompts (brew/sudo/npm).
/// Returns the exit code or an error. NEVER reuses the detection runner — uses
/// the platform layer directly so Windows shims work.
pub fn apply_cli_update(argv: &[String]) -> Result<i32, String> {
    if argv.is_empty() {
        return Err("empty argv".to_string());
    }
    let bin = &argv[0];
    // Absolute-path bypass: when argv[0] is an absolute path that exists, use it
    // directly (don't go through PATH lookup). v0.20.0 R7 B5: rtk's trusted
    // internal `aibridge rtk update --yes` invocation passes the current_exe path.
    let exe = if std::path::Path::new(bin).is_absolute() && std::path::Path::new(bin).exists() {
        std::path::PathBuf::from(bin)
    } else {
        DefaultPlatform::find_executable(bin).map_err(|e| format!("{bin} not on PATH: {e}"))?
    };
    let mut cmd = DefaultPlatform::command_for(&exe);
    cmd.args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn {bin}: {e}"))?;
    Ok(status.code().unwrap_or(-1))
}

// ───────────────────────── pure parsers ─────────────────────────

/// Compute the final [`Mode`] from flag combinations + TTY state. Pure.
pub fn effective_mode(check: bool, yes: bool, is_tty: bool) -> Mode {
    if check {
        return Mode::Check;
    }
    if yes {
        return Mode::YesAuto;
    }
    if is_tty {
        Mode::InteractiveTty
    } else {
        Mode::Check // non-TTY safety: read-only without --yes
    }
}

/// Decide what to do for one [`CliCheck`] under the given [`Mode`]. Pure.
pub fn decide_action(check: &CliCheck, mode: Mode) -> Action {
    if check.up_to_date() {
        return Action::Skip {
            reason: format!("{} is up to date", check.tool),
        };
    }
    let Some(argv) = &check.suggested_command else {
        return Action::Skip {
            reason: format!(
                "{}: manual update — {}",
                check.tool,
                check.manual_note.as_deref().unwrap_or("see docs")
            ),
        };
    };
    let summary = format!(
        "{}: {} → {} via {}",
        check.tool,
        check
            .current
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into()),
        check
            .latest
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into()),
        check.source.label(),
    );
    match mode {
        Mode::Check => Action::Skip {
            reason: format!("{summary} (read-only mode)"),
        },
        Mode::InteractiveTty => Action::Prompt {
            argv: argv.clone(),
            summary,
        },
        Mode::YesAuto => {
            // v0.20.0 R7 B3: use the strict `safe_to_auto_run` gate so the
            // trusted internal `aibridge rtk update --yes` form is also accepted,
            // while a stale-PATH "aibridge" is rejected.
            if check.safe_to_auto_run() {
                Action::Run {
                    argv: argv.clone(),
                    summary,
                }
            } else {
                Action::Skip {
                    reason: format!("{summary} (manual-only — skipped under --yes)"),
                }
            }
        }
    }
}

/// Pure prompt parser. Default-no: only an explicit yes accepts. Used by both
/// CLI and TUI-after-exit code paths so default safety is consistent.
pub fn prompt_parse(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Parse `brew info --json=v2 <pkg>` JSON → stable version string. Returns the
/// first formulae entry's `versions.stable` (if any).
pub fn parse_brew_info_v2_stable_version(json_str: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let stable = v
        .get("formulae")?
        .as_array()?
        .first()?
        .get("versions")?
        .get("stable")?
        .as_str()?
        .to_string();
    Some(stable)
}

/// Parse `npm view <pkg> version` stdout → version string. npm may emit
/// `npm warn …` lines before the version; we skip those and return the first
/// non-warning, non-empty trimmed line.
pub fn parse_npm_view_output(stdout: &str) -> Option<String> {
    for raw in stdout.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("npm ") || line.starts_with("warn") {
            continue;
        }
        return Some(line.to_string());
    }
    None
}

/// Parse a `npx -y <pkg>[@<ver>]`-style package spec. Handles scoped packages
/// (`@scope/pkg`) by treating the FIRST `@` as the scope marker and the SECOND
/// `@` (if any) as the pin separator. Returns `(package, optional_pin)`. The
/// `latest`/`next` keywords are treated as unpinned (they auto-resolve).
pub fn parse_npx_package_pin(spec: &str) -> Option<(String, Option<String>)> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    let (pkg, pin) = if let Some(rest) = s.strip_prefix('@') {
        // Scoped: split on the FIRST `@` AFTER the scope marker.
        match rest.find('@') {
            Some(idx) => {
                let (scope_and_name, at_and_pin) = rest.split_at(idx);
                let pin = &at_and_pin[1..];
                (format!("@{scope_and_name}"), Some(pin.to_string()))
            }
            None => (format!("@{rest}"), None),
        }
    } else {
        // Unscoped: first `@` is the pin separator.
        match s.find('@') {
            Some(idx) => {
                let (name, at_and_pin) = s.split_at(idx);
                let pin = &at_and_pin[1..];
                (name.to_string(), Some(pin.to_string()))
            }
            None => (s.to_string(), None),
        }
    };
    // Normalize keyword pins → None (they're auto-resolving aliases).
    let normalized = match pin.as_deref() {
        Some("latest") | Some("next") => None,
        _ => pin,
    };
    Some((pkg, normalized))
}

/// Pure parser over a resolved MCP server's `(command, args)`. Returns `Some` if
/// it looks like `npx -y <pkg>` (or variants); `None` for non-npx commands. The
/// I/O wrapper [`scan_mcps`] feeds resolved server inventories into this.
pub fn parse_mcp_command_pin(command: &str, args: &[String]) -> Option<(String, Option<String>)> {
    let cmd_base = Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    if cmd_base != "npx" && cmd_base != "npx.cmd" {
        return None;
    }
    // npx accepts a few flags before the package: `-y`, `--yes`, `--no-install`,
    // `-p`/`--package` (with arg). We walk past them and take the first
    // positional that looks like a package spec.
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "-y" || a == "--yes" || a == "--no-install" || a == "--ignore-existing" {
            i += 1;
            continue;
        }
        if a == "-p" || a == "--package" {
            // Skip the flag and its value.
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return parse_npx_package_pin(a);
    }
    None
}

// ───────────────────────── I/O (runner-injected) ─────────────────────────

const DETECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Run `<tool> --version` via the runner and parse the first token-ish version
/// from the first non-empty line.
pub fn current_version_of(
    runner: &dyn CommandRunner,
    tool: &str,
    timeout: Duration,
) -> Option<Version> {
    let (ok, stdout) = runner.run(tool, &["--version"], timeout).ok()?;
    if !ok {
        return None;
    }
    // Find the first token that parses as a version.
    for line in stdout.lines() {
        for tok in line.split_whitespace() {
            // Strip trailing punctuation like commas/parens.
            let t = tok.trim_matches(|c: char| !(c.is_ascii_digit() || c == '.'));
            if let Some(v) = parse_version(t) {
                return Some(v);
            }
        }
    }
    None
}

/// Verify a brew install by asking brew. Returns `true` only on exit 0.
fn brew_list_confirms(runner: &dyn CommandRunner, package: &str) -> bool {
    runner
        .run("brew", &["list", package], DETECT_TIMEOUT)
        .map(|(ok, _)| ok)
        .unwrap_or(false)
}

/// Verify an npm install by asking npm. Returns `true` only on exit 0.
fn npm_global_confirms(runner: &dyn CommandRunner, package: &str) -> bool {
    runner
        .run("npm", &["ls", "-g", package, "--depth=0"], DETECT_TIMEOUT)
        .map(|(ok, _)| ok)
        .unwrap_or(false)
}

/// Two-stage install-source detection: a path heuristic followed by a
/// package-manager confirmation. If the confirmation fails or paths are
/// ambiguous, returns [`InstallSource::Unknown`] so we never auto-run a wrong
/// command. `npm_package` / `brew_package` carry the expected package names per
/// tool (e.g. `@openai/codex` vs the brew formula `codex`).
pub fn detect_install_source_with_verification(
    runner: &dyn CommandRunner,
    tool_path: &Path,
    npm_package: &str,
    brew_package: &str,
) -> InstallSource {
    let path_str = tool_path.to_string_lossy().to_ascii_lowercase();
    // Brew prefixes (mac, linuxbrew). Cellar paths still indicate brew.
    let looks_brew = path_str.contains("/homebrew/")
        || path_str.contains("/usr/local/cellar/")
        || path_str.contains("/linuxbrew/");
    // Npm globals on macOS/Linux end up under `lib/node_modules/.bin/` or
    // similar; on Windows under `%APPDATA%\npm\<name>.cmd`.
    let looks_npm = path_str.contains("\\appdata\\roaming\\npm\\")
        || path_str.contains("/node_modules/")
        || path_str.contains("/.nvm/")
        || path_str.ends_with(".cmd");
    // Cargo bin: explicit, but we don't auto-update from it.
    let looks_cargo = path_str.contains("/.cargo/bin/") || path_str.contains("\\.cargo\\bin\\");

    if looks_cargo {
        return InstallSource::Cargo;
    }
    if looks_brew && brew_list_confirms(runner, brew_package) {
        return InstallSource::Brew {
            package: brew_package.to_string(),
        };
    }
    if looks_npm && npm_global_confirms(runner, npm_package) {
        return InstallSource::Npm {
            package: npm_package.to_string(),
        };
    }
    // Path heuristic was ambiguous OR verification failed — refuse to guess.
    let reason = if looks_brew {
        "path looks like brew but `brew list` did not confirm".to_string()
    } else if looks_npm {
        "path looks like npm but `npm ls -g` did not confirm".to_string()
    } else {
        "no recognized package-manager prefix in path".to_string()
    };
    InstallSource::Unknown {
        path: tool_path.to_path_buf(),
        reason,
    }
}

/// Latest brew formula `stable` version. Returns `None` on any failure.
pub fn brew_latest_version(runner: &dyn CommandRunner, package: &str) -> Option<Version> {
    let (ok, stdout) = runner
        .run("brew", &["info", "--json=v2", package], DETECT_TIMEOUT)
        .ok()?;
    if !ok {
        return None;
    }
    let raw = parse_brew_info_v2_stable_version(&stdout)?;
    parse_version(&raw)
}

/// Latest npm version. Returns `None` on any failure.
pub fn npm_latest_version(runner: &dyn CommandRunner, package: &str) -> Option<Version> {
    let (ok, stdout) = runner
        .run("npm", &["view", package, "version"], DETECT_TIMEOUT)
        .ok()?;
    if !ok {
        return None;
    }
    let raw = parse_npm_view_output(&stdout)?;
    parse_version(&raw)
}

/// Latest GitHub-release tag (uses `gh api`, requires `gh` on PATH and authed).
pub fn gh_latest_release_tag(runner: &dyn CommandRunner, slug: &str) -> Option<Version> {
    let raw = gh_latest_release_tag_raw(runner, slug)?;
    parse_version(&raw)
}

/// Same as [`gh_latest_release_tag`] but returns the RAW tag string (e.g. `v0.42.0`).
/// Needed by `rtk::install_or_update` because `gh release download` takes the literal
/// tag, not a parsed `Version`. v0.20.0 R5 B4.
pub fn gh_latest_release_tag_raw(runner: &dyn CommandRunner, slug: &str) -> Option<String> {
    let endpoint = format!("repos/{slug}/releases/latest");
    let (ok, stdout) = runner
        .run(
            "gh",
            &["api", &endpoint, "--jq", ".tag_name"],
            DETECT_TIMEOUT,
        )
        .ok()?;
    if !ok {
        return None;
    }
    let raw = stdout.trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

// ───────────────────────── per-tool checks ─────────────────────────

/// Check the codex CLI. Resolves source (brew or npm), queries that source's
/// channel for latest. Cargo / Unknown sources don't get a latest claim.
pub fn check_codex(runner: &dyn CommandRunner) -> CliCheck {
    let current = current_version_of(runner, "codex", DETECT_TIMEOUT);
    let tool_path = DefaultPlatform::find_executable("codex").ok();
    let source = match &tool_path {
        Some(p) => detect_install_source_with_verification(runner, p, "@openai/codex", "codex"),
        None => InstallSource::Unknown {
            path: PathBuf::from("codex"),
            reason: "codex not on PATH".to_string(),
        },
    };
    let (latest, suggested, note) = match &source {
        InstallSource::Brew { package } => {
            let l = brew_latest_version(runner, package);
            let argv = vec!["brew".into(), "upgrade".into(), package.clone()];
            (l, Some(argv), Some(format!("brew upgrade {package}")))
        }
        InstallSource::Npm { package } => {
            let l = npm_latest_version(runner, package);
            let argv = vec![
                "npm".into(),
                "i".into(),
                "-g".into(),
                format!("{package}@latest"),
            ];
            (l, Some(argv), Some(format!("npm i -g {package}@latest")))
        }
        InstallSource::Cargo => (
            None,
            None,
            Some(
                "codex appears cargo-installed; aibridge does not auto-update third-party \
                 cargo crates. Use the upstream install instructions."
                    .to_string(),
            ),
        ),
        InstallSource::NativeInstaller { docs_url } => {
            (None, None, Some(format!("see {docs_url}")))
        }
        InstallSource::Unknown { reason, .. } => (
            None,
            None,
            Some(format!("unknown source ({reason}); update manually")),
        ),
    };
    CliCheck {
        tool: "codex",
        current,
        latest,
        source,
        suggested_command: suggested,
        manual_note: note,
        installable: false,
    }
}

/// Check the claude CLI. Native installer is the most common path (no `claude
/// update`); npm install is also possible (`@anthropic-ai/claude-code`).
pub fn check_claude(runner: &dyn CommandRunner) -> CliCheck {
    let current = current_version_of(runner, "claude", DETECT_TIMEOUT);
    let tool_path = DefaultPlatform::find_executable("claude").ok();
    let path_str = tool_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    // Heuristic: `/.local/bin/claude` (no other PM markers) = native installer.
    let looks_native = (path_str.contains("/.local/bin/") || path_str.contains("\\.local\\bin\\"))
        && !path_str.contains("/.nvm/")
        && !path_str.contains("\\appdata\\");
    let source = if looks_native {
        InstallSource::NativeInstaller {
            docs_url: "https://claude.com/download".to_string(),
        }
    } else if let Some(p) = &tool_path {
        detect_install_source_with_verification(runner, p, "@anthropic-ai/claude-code", "claude")
    } else {
        InstallSource::Unknown {
            path: PathBuf::from("claude"),
            reason: "claude not on PATH".to_string(),
        }
    };
    let (latest, suggested, note) = match &source {
        InstallSource::Npm { package } => {
            let l = npm_latest_version(runner, package);
            let argv = vec![
                "npm".into(),
                "i".into(),
                "-g".into(),
                format!("{package}@latest"),
            ];
            (l, Some(argv), Some(format!("npm i -g {package}@latest")))
        }
        InstallSource::Brew { package } => {
            let l = brew_latest_version(runner, package);
            let argv = vec!["brew".into(), "upgrade".into(), package.clone()];
            (l, Some(argv), Some(format!("brew upgrade {package}")))
        }
        InstallSource::NativeInstaller { docs_url } => (
            None,
            None,
            Some(format!(
                "claude CLI is a native installer; download the latest from {docs_url}"
            )),
        ),
        InstallSource::Cargo => (
            None,
            None,
            Some("claude appears cargo-installed; update manually".into()),
        ),
        InstallSource::Unknown { reason, .. } => (
            None,
            None,
            Some(format!("unknown source ({reason}); update manually")),
        ),
    };
    CliCheck {
        tool: "claude",
        current,
        latest,
        source,
        suggested_command: suggested,
        manual_note: note,
        installable: false,
    }
}

/// Check rtk. v0.20.0 (Task B): rtk gets a verified auto-install path via the
/// trusted internal `aibridge rtk install/update --yes` subcommand. Identity +
/// checksum + archive safety + atomic replace all run inside
/// `rtk::install_or_update`. `suggested_command` carries the current executable
/// path so [`CliCheck::safe_to_auto_run`] can match the exact shape.
pub fn check_rtk(runner: &dyn CommandRunner) -> CliCheck {
    check_rtk_with(runner, &RealPathResolver)
}

/// Resolver-injectable variant for tests. Production callers use [`check_rtk`].
pub fn check_rtk_with(runner: &dyn CommandRunner, resolver: &dyn PathResolver) -> CliCheck {
    let current = current_version_of(runner, "rtk", DETECT_TIMEOUT);
    let latest = gh_latest_release_tag(runner, "rtk-ai/rtk");
    let target = crate::rtk::detect_target(runner, resolver);
    let (source, suggested, note, installable) = match &target {
        crate::rtk::RtkTarget::Brew => (
            InstallSource::Brew {
                package: "rtk".to_string(),
            },
            current_exe_path().map(|exe| {
                vec![
                    exe.display().to_string(),
                    "rtk".into(),
                    "update".into(),
                    "--yes".into(),
                ]
            }),
            Some("brew-managed rtk; auto-update via `aibridge rtk update --yes`".to_string()),
            false,
        ),
        crate::rtk::RtkTarget::NativeBin {
            path,
            writable: true,
        } => (
            InstallSource::NativeInstaller {
                docs_url: format!("auto-update target {path:?}"),
            },
            current_exe_path().map(|exe| {
                vec![
                    exe.display().to_string(),
                    "rtk".into(),
                    "update".into(),
                    "--yes".into(),
                ]
            }),
            Some(
                "verified rtk install; auto-update via `aibridge rtk update --yes` \
                 (downloads + verifies SHA256 + atomic-replaces)"
                    .to_string(),
            ),
            false,
        ),
        crate::rtk::RtkTarget::NativeBin {
            path,
            writable: false,
        } => (
            InstallSource::Unknown {
                path: path.clone(),
                reason: "install path not writable; update manually".to_string(),
            },
            None,
            Some(format!(
                "rtk at {path:?} is not writable by the current user; update manually"
            )),
            false,
        ),
        crate::rtk::RtkTarget::NotInstalled { install_to } => (
            InstallSource::Unknown {
                path: install_to.clone(),
                reason: "rtk not installed".to_string(),
            },
            current_exe_path().map(|exe| {
                vec![
                    exe.display().to_string(),
                    "rtk".into(),
                    "install".into(),
                    "--yes".into(),
                ]
            }),
            Some(format!(
                "rtk is not installed; will install to {install_to:?} via \
                 `aibridge rtk install --yes` (downloads + verifies + identity-checks)"
            )),
            true, // missing-but-installable
        ),
        crate::rtk::RtkTarget::Unknown { reason } => (
            InstallSource::Unknown {
                path: PathBuf::from("rtk"),
                reason: reason.clone(),
            },
            None,
            Some(format!("rtk auto-update unavailable: {reason}")),
            false,
        ),
        crate::rtk::RtkTarget::Unsupported { reason } => (
            InstallSource::Unknown {
                path: PathBuf::from("rtk"),
                reason: reason.clone(),
            },
            None,
            Some(format!(
                "rtk auto-install not supported on this platform: {reason}"
            )),
            false,
        ),
    };
    CliCheck {
        tool: "rtk",
        current,
        latest,
        source,
        suggested_command: suggested,
        manual_note: note,
        installable,
    }
}

/// Run all three CLI checks sequentially. Cheap (3 short-timeout calls each);
/// parallel would over-engineer.
pub fn check_all(runner: &dyn CommandRunner) -> Vec<CliCheck> {
    vec![check_codex(runner), check_claude(runner), check_rtk(runner)]
}

// ───────────────────────── MCP-pin scanner ─────────────────────────

/// Scan all known MCP servers (Claude + Codex inventories) for npx-based
/// commands and surface their version-pin status. Read-only. Servers whose
/// launch command isn't `npx`-like are omitted (we'd have nothing to say).
pub fn scan_mcps(project: &Path) -> Vec<McpVersionStatus> {
    let mut out = Vec::new();

    // Claude side: file-only, no shell.
    if let crate::claude_mcp::Inventory::Available { servers, .. } =
        crate::claude_mcp::inventory(project)
    {
        for s in &servers {
            if let crate::claude_mcp::Transport::Stdio { command, args, .. } = &s.transport {
                if let Some((pkg, pin)) = parse_mcp_command_pin(command, args) {
                    out.push(McpVersionStatus {
                        agent: "claude",
                        server_name: s.name.clone(),
                        package: Some(pkg),
                        version_pin: pin,
                    });
                }
            }
        }
    }
    // Codex side: this shells out to `codex mcp list --json` via the existing
    // inventory function. Only stdio-transport servers have a command/args to
    // parse; others (streamable_http, sse) are skipped.
    if let crate::review_mcp::Inventory::Available(servers) =
        crate::review_mcp::codex_inventory(&project.display().to_string())
    {
        for s in &servers {
            if s.transport != "stdio" {
                continue;
            }
            if let Some((pkg, pin)) = parse_mcp_command_pin(&s.command, &s.args) {
                out.push(McpVersionStatus {
                    agent: "codex",
                    server_name: s.name.clone(),
                    package: Some(pkg),
                    version_pin: pin,
                });
            }
        }
    }
    out
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Test-only fake runner backed by a fixture table.
    pub struct FakeCommandRunner {
        responses: Mutex<HashMap<String, (bool, String)>>,
    }
    impl FakeCommandRunner {
        pub fn new() -> Self {
            Self {
                responses: Mutex::new(HashMap::new()),
            }
        }
        pub fn set(&self, bin: &str, args: &[&str], success: bool, stdout: &str) {
            let key = format!("{bin} {}", args.join(" "));
            self.responses
                .lock()
                .unwrap()
                .insert(key, (success, stdout.to_string()));
        }
    }
    impl CommandRunner for FakeCommandRunner {
        fn run(
            &self,
            bin: &str,
            args: &[&str],
            _timeout: Duration,
        ) -> Result<(bool, String), String> {
            let key = format!("{bin} {}", args.join(" "));
            self.responses
                .lock()
                .unwrap()
                .get(&key)
                .cloned()
                .ok_or_else(|| format!("FakeCommandRunner: no fixture for `{key}`"))
        }
    }

    // ─── effective_mode ───
    #[test]
    fn effective_mode_check_overrides_everything() {
        assert_eq!(effective_mode(true, true, true), Mode::Check);
        assert_eq!(effective_mode(true, false, false), Mode::Check);
    }
    #[test]
    fn effective_mode_yes_works_in_non_tty() {
        assert_eq!(effective_mode(false, true, false), Mode::YesAuto);
        assert_eq!(effective_mode(false, true, true), Mode::YesAuto);
    }
    #[test]
    fn effective_mode_no_flags_in_non_tty_falls_back_to_check() {
        assert_eq!(effective_mode(false, false, false), Mode::Check);
    }
    #[test]
    fn effective_mode_no_flags_with_tty_is_interactive() {
        assert_eq!(effective_mode(false, false, true), Mode::InteractiveTty);
    }

    // ─── prompt_parse ───
    #[test]
    fn prompt_parse_empty_declines() {
        assert!(!prompt_parse(""));
        assert!(!prompt_parse("   "));
    }
    #[test]
    fn prompt_parse_accepts_y_yes() {
        assert!(prompt_parse("y"));
        assert!(prompt_parse("Y"));
        assert!(prompt_parse("yes"));
        assert!(prompt_parse("YES"));
        assert!(prompt_parse(" Y "));
    }
    #[test]
    fn prompt_parse_declines_n_or_garbage() {
        assert!(!prompt_parse("n"));
        assert!(!prompt_parse("no"));
        assert!(!prompt_parse("garbage"));
        assert!(!prompt_parse("ya"));
    }

    // ─── parse_brew_info_v2_stable_version ───
    #[test]
    fn parse_brew_info_v2_stable_version_picks_stable() {
        let json = r#"{"formulae":[{"name":"codex","versions":{"stable":"0.132.0","head":"HEAD","bottle":true}}],"casks":[]}"#;
        assert_eq!(
            parse_brew_info_v2_stable_version(json),
            Some("0.132.0".to_string())
        );
    }
    #[test]
    fn parse_brew_info_v2_missing_returns_none() {
        assert_eq!(parse_brew_info_v2_stable_version("{}"), None);
        assert_eq!(parse_brew_info_v2_stable_version("not json"), None);
        assert_eq!(
            parse_brew_info_v2_stable_version(r#"{"formulae":[]}"#),
            None
        );
    }

    // ─── parse_npm_view_output ───
    #[test]
    fn parse_npm_view_output_simple_version() {
        assert_eq!(parse_npm_view_output("0.132.0\n"), Some("0.132.0".into()));
    }
    #[test]
    fn parse_npm_view_output_skips_warnings() {
        let out = "npm warn deprecated foo@0.1.0: please upgrade\n0.132.0\n";
        assert_eq!(parse_npm_view_output(out), Some("0.132.0".into()));
    }
    #[test]
    fn parse_npm_view_output_empty_returns_none() {
        assert_eq!(parse_npm_view_output(""), None);
        assert_eq!(parse_npm_view_output("\n\n"), None);
    }

    // ─── parse_npx_package_pin ───
    #[test]
    fn parse_npx_package_pin_unscoped_unpinned() {
        assert_eq!(parse_npx_package_pin("pkg"), Some(("pkg".into(), None)));
    }
    #[test]
    fn parse_npx_package_pin_unscoped_pinned() {
        assert_eq!(
            parse_npx_package_pin("pkg@1.2.3"),
            Some(("pkg".into(), Some("1.2.3".into())))
        );
    }
    #[test]
    fn parse_npx_package_pin_scoped_unpinned() {
        assert_eq!(
            parse_npx_package_pin("@openai/codex"),
            Some(("@openai/codex".into(), None))
        );
    }
    #[test]
    fn parse_npx_package_pin_scoped_pinned() {
        assert_eq!(
            parse_npx_package_pin("@openai/codex@0.132.0"),
            Some(("@openai/codex".into(), Some("0.132.0".into())))
        );
    }
    #[test]
    fn parse_npx_package_pin_treats_latest_keyword_as_unpinned() {
        assert_eq!(
            parse_npx_package_pin("pkg@latest"),
            Some(("pkg".into(), None))
        );
        assert_eq!(
            parse_npx_package_pin("@scope/pkg@latest"),
            Some(("@scope/pkg".into(), None))
        );
        assert_eq!(
            parse_npx_package_pin("pkg@next"),
            Some(("pkg".into(), None))
        );
    }

    // ─── parse_mcp_command_pin ───
    #[test]
    fn parse_mcp_command_pin_npx_unpinned_scoped() {
        let args: Vec<String> = vec![
            "-y".into(),
            "@modelcontextprotocol/server-everything".into(),
        ];
        assert_eq!(
            parse_mcp_command_pin("npx", &args),
            Some(("@modelcontextprotocol/server-everything".into(), None))
        );
    }
    #[test]
    fn parse_mcp_command_pin_npx_pinned() {
        let args: Vec<String> = vec!["-y".into(), "@scope/pkg@1.2.3".into()];
        assert_eq!(
            parse_mcp_command_pin("npx", &args),
            Some(("@scope/pkg".into(), Some("1.2.3".into())))
        );
    }
    #[test]
    fn parse_mcp_command_pin_skips_npx_flags() {
        let args: Vec<String> = vec!["--yes".into(), "pkg".into()];
        assert_eq!(
            parse_mcp_command_pin("npx", &args),
            Some(("pkg".into(), None))
        );
    }
    #[test]
    fn parse_mcp_command_pin_ignores_non_npx_command() {
        let args: Vec<String> = vec!["mcp-server".into()];
        assert_eq!(parse_mcp_command_pin("node", &args), None);
        assert_eq!(parse_mcp_command_pin("/usr/bin/codex", &args), None);
    }
    #[test]
    fn parse_mcp_command_pin_treats_latest_keyword_as_unpinned() {
        let args: Vec<String> = vec!["-y".into(), "pkg@latest".into()];
        assert_eq!(
            parse_mcp_command_pin("npx", &args),
            Some(("pkg".into(), None))
        );
    }

    // ─── decide_action ───
    fn check_outdated_brew() -> CliCheck {
        CliCheck {
            tool: "codex",
            current: parse_version("0.130.0"),
            latest: parse_version("0.132.0"),
            source: InstallSource::Brew {
                package: "codex".into(),
            },
            suggested_command: Some(vec!["brew".into(), "upgrade".into(), "codex".into()]),
            manual_note: Some("brew upgrade codex".into()),
            installable: false,
        }
    }
    fn check_outdated_native() -> CliCheck {
        CliCheck {
            tool: "claude",
            current: parse_version("2.1.146"),
            latest: parse_version("2.1.150"),
            source: InstallSource::NativeInstaller {
                docs_url: "https://claude.com/download".into(),
            },
            suggested_command: None,
            manual_note: Some("see https://claude.com/download".into()),
            installable: false,
        }
    }

    #[test]
    fn decide_action_check_never_mutates() {
        let a = decide_action(&check_outdated_brew(), Mode::Check);
        assert!(matches!(a, Action::Skip { .. }));
    }
    #[test]
    fn decide_action_yes_runs_verified_pkg_manager() {
        let a = decide_action(&check_outdated_brew(), Mode::YesAuto);
        match a {
            Action::Run { argv, .. } => assert_eq!(argv[0], "brew"),
            other => panic!("expected Run, got {other:?}"),
        }
    }
    #[test]
    fn decide_action_yes_skips_manual_only_without_failing() {
        let a = decide_action(&check_outdated_native(), Mode::YesAuto);
        assert!(matches!(a, Action::Skip { .. }));
    }
    #[test]
    fn decide_action_interactive_prompts_for_verified() {
        let a = decide_action(&check_outdated_brew(), Mode::InteractiveTty);
        assert!(matches!(a, Action::Prompt { .. }));
    }
    #[test]
    fn decide_action_interactive_skips_manual_only() {
        let a = decide_action(&check_outdated_native(), Mode::InteractiveTty);
        assert!(matches!(a, Action::Skip { .. }));
    }
    #[test]
    fn decide_action_up_to_date_skips() {
        let mut c = check_outdated_brew();
        c.current = c.latest;
        let a = decide_action(&c, Mode::InteractiveTty);
        assert!(matches!(a, Action::Skip { .. }));
    }
    #[test]
    fn decide_action_run_carries_argv_only() {
        let c = check_outdated_brew();
        let a = decide_action(&c, Mode::YesAuto);
        match a {
            Action::Run { argv, .. } => {
                assert_eq!(argv, vec!["brew", "upgrade", "codex"]);
            }
            _ => panic!(),
        }
    }
    #[test]
    fn string_compare_trap_regression() {
        // Lexicographically, "0.10.0" < "0.9.9" — but as semver, "0.10.0" > "0.9.9".
        // CliCheck::up_to_date uses Version cmp, not str cmp.
        let c = CliCheck {
            tool: "x",
            current: parse_version("0.10.0"),
            latest: parse_version("0.9.9"),
            source: InstallSource::Brew {
                package: "x".into(),
            },
            suggested_command: Some(vec!["brew".into()]),
            manual_note: None,
            installable: false,
        };
        assert!(c.up_to_date(), "0.10.0 must be >= 0.9.9");
    }
    #[test]
    fn up_to_date_when_latest_unknown_no_prompt() {
        // Latest = None → up_to_date() returns true (we can't claim newer).
        let c = CliCheck {
            tool: "x",
            current: parse_version("1.0.0"),
            latest: None,
            source: InstallSource::Cargo,
            suggested_command: None,
            manual_note: None,
            installable: false,
        };
        assert!(c.up_to_date());
        assert!(matches!(
            decide_action(&c, Mode::InteractiveTty),
            Action::Skip { .. }
        ));
    }

    // ─── current_version_of (via FakeCommandRunner) ───
    #[test]
    fn current_version_parses_codex_cli_format() {
        let r = FakeCommandRunner::new();
        r.set("codex", &["--version"], true, "codex-cli 0.132.0\n");
        let v = current_version_of(&r, "codex", DETECT_TIMEOUT);
        assert_eq!(v, parse_version("0.132.0"));
    }
    #[test]
    fn current_version_parses_claude_code_format() {
        let r = FakeCommandRunner::new();
        r.set("claude", &["--version"], true, "2.1.146 (Claude Code)\n");
        let v = current_version_of(&r, "claude", DETECT_TIMEOUT);
        assert_eq!(v, parse_version("2.1.146"));
    }
    #[test]
    fn current_version_handles_v_prefix() {
        let r = FakeCommandRunner::new();
        r.set("rtk", &["--version"], true, "rtk v0.4.2\n");
        let v = current_version_of(&r, "rtk", DETECT_TIMEOUT);
        assert_eq!(v, parse_version("0.4.2"));
    }
    #[test]
    fn current_version_returns_none_on_nonzero_exit() {
        let r = FakeCommandRunner::new();
        r.set("codex", &["--version"], false, "");
        assert_eq!(current_version_of(&r, "codex", DETECT_TIMEOUT), None);
    }
    #[test]
    fn current_version_returns_none_on_missing_fixture() {
        let r = FakeCommandRunner::new();
        // No fixture for this command.
        assert_eq!(current_version_of(&r, "ghost", DETECT_TIMEOUT), None);
    }

    // ─── brew/npm/gh latest via runner ───
    #[test]
    fn brew_latest_via_runner() {
        let r = FakeCommandRunner::new();
        r.set(
            "brew",
            &["info", "--json=v2", "codex"],
            true,
            r#"{"formulae":[{"versions":{"stable":"0.132.0"}}]}"#,
        );
        assert_eq!(brew_latest_version(&r, "codex"), parse_version("0.132.0"));
    }
    #[test]
    fn npm_latest_via_runner_skips_warnings() {
        let r = FakeCommandRunner::new();
        r.set(
            "npm",
            &["view", "@openai/codex", "version"],
            true,
            "npm warn deprecated\n0.132.0\n",
        );
        assert_eq!(
            npm_latest_version(&r, "@openai/codex"),
            parse_version("0.132.0")
        );
    }
    #[test]
    fn gh_latest_via_runner() {
        let r = FakeCommandRunner::new();
        r.set(
            "gh",
            &[
                "api",
                "repos/rtk-ai/rtk/releases/latest",
                "--jq",
                ".tag_name",
            ],
            true,
            "v0.4.2\n",
        );
        assert_eq!(
            gh_latest_release_tag(&r, "rtk-ai/rtk"),
            parse_version("0.4.2")
        );
    }

    // ─── detect_install_source_with_verification ───
    #[test]
    fn detect_install_source_brew_only_after_verification() {
        let r = FakeCommandRunner::new();
        r.set("brew", &["list", "codex"], true, "");
        let p = Path::new("/opt/homebrew/bin/codex");
        let s = detect_install_source_with_verification(&r, p, "@openai/codex", "codex");
        assert!(matches!(s, InstallSource::Brew { .. }));
    }
    #[test]
    fn detect_install_source_brew_path_without_verification_is_unknown() {
        let r = FakeCommandRunner::new();
        // No brew fixture → run returns Err → unverified.
        let p = Path::new("/opt/homebrew/bin/codex");
        let s = detect_install_source_with_verification(&r, p, "@openai/codex", "codex");
        assert!(matches!(s, InstallSource::Unknown { .. }));
    }
    #[test]
    fn detect_install_source_npm_windows_after_verification() {
        let r = FakeCommandRunner::new();
        r.set("npm", &["ls", "-g", "@openai/codex", "--depth=0"], true, "");
        let p = Path::new(r"C:\Users\X\AppData\Roaming\npm\codex.cmd");
        let s = detect_install_source_with_verification(&r, p, "@openai/codex", "codex");
        assert!(matches!(s, InstallSource::Npm { .. }));
    }
    #[test]
    fn detect_install_source_cargo_treated_distinctly() {
        let r = FakeCommandRunner::new();
        let p = Path::new("/Users/x/.cargo/bin/codex");
        let s = detect_install_source_with_verification(&r, p, "@openai/codex", "codex");
        assert!(matches!(s, InstallSource::Cargo));
    }
    #[test]
    fn detect_install_source_unrecognized_path_is_unknown() {
        let r = FakeCommandRunner::new();
        let p = Path::new("/random/place/codex");
        let s = detect_install_source_with_verification(&r, p, "@openai/codex", "codex");
        match s {
            InstallSource::Unknown { reason, .. } => {
                assert!(reason.contains("no recognized"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    // ─── apply_cli_update sanity ───
    #[test]
    fn apply_cli_update_rejects_empty_argv() {
        assert!(apply_cli_update(&[]).is_err());
    }
    #[test]
    fn apply_cli_update_errors_on_missing_executable() {
        let res = apply_cli_update(&["definitely_not_a_real_binary_xyz123".to_string()]);
        assert!(res.is_err(), "expected Err, got {res:?}");
        let msg = res.unwrap_err();
        assert!(
            msg.to_ascii_lowercase().contains("not on path")
                || msg.to_ascii_lowercase().contains("not found")
                || msg.contains("definitely_not_a_real_binary_xyz123"),
            "error should mention the missing binary or PATH: {msg}"
        );
    }
}
