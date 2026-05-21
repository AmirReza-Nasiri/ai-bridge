//! Minimal git helpers (used to feed diffs to the reviewer).

use aibridge_platform::{DefaultPlatform, Platform};
use anyhow::{Context, Result};
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Cap on total untracked-file content pulled into a review bundle.
const MAX_UNTRACKED_CHARS: usize = 8_000;

/// Return the uncommitted diff for tracked files in `cwd` (`git diff HEAD`,
/// with a `git diff` fallback when there is no `HEAD` yet).
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
    let fallback = DefaultPlatform::command_for(&git)
        .args(["--no-pager", "diff"])
        .current_dir(cwd)
        .output()
        .context("running `git diff`")?;
    Ok(String::from_utf8_lossy(&fallback.stdout).into_owned())
}

/// A normalized snapshot of all uncommitted change in a workspace, plus a hash
/// for cheap change detection. Includes untracked files so brand-new-file bugs
/// can't bypass the review gate.
pub struct DiffBundle {
    pub text: String,
    pub hash: u64,
    pub is_empty: bool,
}

/// Build a full diff bundle: porcelain status + staged + unstaged + untracked
/// file contents (size-capped).
pub fn diff_bundle(cwd: &str) -> Result<DiffBundle> {
    let git = DefaultPlatform::find_executable("git").context("locating git")?;
    let run = |args: &[&str]| -> String {
        DefaultPlatform::command_for(&git)
            .args(args)
            .current_dir(cwd)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };

    // Exclude AI Bridge's own trace dir so reviews don't see (and choke on) our
    // `.ai-bridge/` output, which would also change the diff hash every review.
    let status = run(&["--no-pager", "status", "--porcelain"])
        .lines()
        .filter(|l| !l.contains(".ai-bridge"))
        .collect::<Vec<_>>()
        .join("\n");
    let staged = run(&[
        "--no-pager",
        "diff",
        "--cached",
        "--",
        ".",
        ":(exclude).ai-bridge",
    ]);
    let unstaged = run(&["--no-pager", "diff", "--", ".", ":(exclude).ai-bridge"]);

    let mut untracked = String::new();
    let mut budget = MAX_UNTRACKED_CHARS;
    for line in status.lines() {
        if budget == 0 {
            untracked.push_str("\n[untracked content truncated]");
            break;
        }
        if let Some(path) = line.strip_prefix("?? ") {
            let path = path.trim();
            if let Ok(content) = std::fs::read_to_string(Path::new(cwd).join(path)) {
                let snippet: String = content.chars().take(budget).collect();
                budget = budget.saturating_sub(snippet.chars().count());
                untracked.push_str(&format!("\n--- untracked: {path} ---\n{snippet}\n"));
            }
        }
    }

    let is_empty =
        status.trim().is_empty() && staged.trim().is_empty() && unstaged.trim().is_empty();
    let text = format!(
        "# git status --porcelain\n{status}\n\n# staged diff\n{staged}\n\n# unstaged diff\n{unstaged}\n\n# untracked files{untracked}"
    );

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let hash = hasher.finish();

    Ok(DiffBundle {
        text,
        hash,
        is_empty,
    })
}
