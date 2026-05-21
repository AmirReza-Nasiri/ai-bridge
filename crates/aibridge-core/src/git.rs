//! Minimal git helpers (used to feed diffs to the reviewer).

use aibridge_platform::{DefaultPlatform, Platform};
use anyhow::{Context, Result};

/// Return the uncommitted diff for tracked files in `cwd`.
///
/// Uses `git diff HEAD` (staged + unstaged vs the last commit). Falls back to
/// `git diff` when there is no `HEAD` yet (a repo with no commits).
pub fn diff(cwd: &str) -> Result<String> {
    let git = DefaultPlatform::find_executable("git").context("locating git")?;

    let primary = DefaultPlatform::command_for(&git)
        .args(["--no-pager", "diff", "HEAD"])
        .current_dir(cwd)
        .output()
        .context("running `git diff HEAD`")?;
    if primary.status.success() {
        return Ok(String::from_utf8_lossy(&primary.stdout).into_owned());
    }

    // No HEAD (e.g. a fresh repo) — fall back to the working-tree diff.
    let fallback = DefaultPlatform::command_for(&git)
        .args(["--no-pager", "diff"])
        .current_dir(cwd)
        .output()
        .context("running `git diff`")?;
    Ok(String::from_utf8_lossy(&fallback.stdout).into_owned())
}
