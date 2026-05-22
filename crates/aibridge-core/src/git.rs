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

    // Scope every query to the project subtree (`-- .`) so a sibling project in
    // the SAME git repo can't pollute the review, and exclude AI Bridge's own
    // `.ai-bridge/` trace dir via pathspec (it would otherwise churn the diff
    // hash every review). Without `-- .` the porcelain status is repo-wide.
    let status = run(&[
        "--no-pager",
        "status",
        "--porcelain",
        "--",
        ".",
        ":(exclude).ai-bridge",
    ]);
    let staged = run(&[
        "--no-pager",
        "diff",
        "--cached",
        "--",
        ".",
        ":(exclude).ai-bridge",
    ]);
    let unstaged = run(&["--no-pager", "diff", "--", ".", ":(exclude).ai-bridge"]);

    // Untracked file CONTENTS via NUL-delimited `ls-files`: it lists individual
    // files (porcelain status collapses a brand-new dir to `?? dir/`), honors
    // ignore rules (`--exclude-standard`), and prints cwd-relative, UNquoted paths
    // (porcelain C-quotes names with spaces/newlines, which broke the path join).
    let untracked_list = run(&[
        "--no-pager",
        "ls-files",
        "--others",
        "--exclude-standard",
        "-z",
        "--",
        ".",
        ":(exclude).ai-bridge",
    ]);
    let mut untracked = String::new();
    let mut budget = MAX_UNTRACKED_CHARS;
    for path in untracked_list.split('\0').filter(|s| !s.is_empty()) {
        if budget == 0 {
            untracked.push_str("\n[untracked content truncated]");
            break;
        }
        if let Ok(content) = std::fs::read_to_string(Path::new(cwd).join(path)) {
            let snippet: String = content.chars().take(budget).collect();
            budget = budget.saturating_sub(snippet.chars().count());
            untracked.push_str(&format!("\n--- untracked: {path} ---\n{snippet}\n"));
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
