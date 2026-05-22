//! Minimal git helpers (used to feed diffs to the reviewer).

use aibridge_platform::{DefaultPlatform, Platform};
use anyhow::{Context, Result};
use std::hash::{Hash, Hasher};
use std::path::Path;

/// Cap on total untracked-file content pulled into a review bundle.
const MAX_UNTRACKED_CHARS: usize = 8_000;

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
    let mut truncated = false;
    for path in untracked_list.split('\0').filter(|s| !s.is_empty()) {
        if budget == 0 {
            // More untracked files remain but the budget is spent.
            truncated = true;
            break;
        }
        if let Ok(content) = std::fs::read_to_string(Path::new(cwd).join(path)) {
            let full_len = content.chars().count();
            let snippet: String = content.chars().take(budget).collect();
            let took = snippet.chars().count();
            if took < full_len {
                // THIS file was itself cut off — mark it even when it's the only one
                // (the marker must not depend on a later iteration existing).
                truncated = true;
            }
            budget -= took;
            untracked.push_str(&format!("\n--- untracked: {path} ---\n{snippet}\n"));
        }
    }
    if truncated {
        untracked.push_str("\n[untracked content truncated]");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("spawn git")
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// A throwaway git repo under the temp dir, removed on drop.
    struct TempRepo(std::path::PathBuf);
    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    // Monotonic per-process counter so concurrently-running tests (cargo's default)
    // never collide on a temp-dir name — a same-nanosecond clash made this flaky.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_repo() -> TempRepo {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "aibridge-git-test-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@t.t"]);
        git(&dir, &["config", "user.name", "t"]);
        // Establish HEAD so the staged/unstaged queries hit the normal path.
        std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "init"]);
        TempRepo(dir)
    }

    #[test]
    fn clean_tree_is_empty() {
        let repo = temp_repo();
        let b = diff_bundle(repo.0.to_str().unwrap()).unwrap();
        assert!(b.is_empty, "a clean tree must report empty");
    }

    #[test]
    fn untracked_only_is_not_empty_and_included() {
        // The regression this guards: a brand-new (untracked) file must be visible
        // to the bundle, so on-demand `review_diff` (which now shares this path)
        // can't report "nothing to review" while the gate would review it.
        let repo = temp_repo();
        std::fs::write(
            repo.0.join("brand_new.rs"),
            "fn boom() { let _x = nope; }\n",
        )
        .unwrap();
        let b = diff_bundle(repo.0.to_str().unwrap()).unwrap();
        assert!(!b.is_empty, "untracked-only change must not be empty");
        assert!(
            b.text.contains("brand_new.rs"),
            "untracked path should appear"
        );
        assert!(b.text.contains("nope"), "untracked content should appear");
    }

    #[test]
    fn ai_bridge_dir_is_excluded() {
        // The gate's own trace dir must never churn the review/bundle.
        let repo = temp_repo();
        std::fs::create_dir_all(repo.0.join(".ai-bridge")).unwrap();
        std::fs::write(repo.0.join(".ai-bridge/trace.txt"), "noise\n").unwrap();
        let b = diff_bundle(repo.0.to_str().unwrap()).unwrap();
        assert!(
            b.is_empty,
            ".ai-bridge-only change must be excluded → empty"
        );
        assert!(
            !b.text.contains("trace.txt"),
            ".ai-bridge content must not leak in"
        );
    }

    #[test]
    fn large_untracked_content_is_truncated() {
        let repo = temp_repo();
        // First file (alphabetically) consumes the whole budget; the second then
        // confirms the marker fires when more files remain past the budget.
        std::fs::write(
            repo.0.join("a_big.txt"),
            "x".repeat(MAX_UNTRACKED_CHARS + 100),
        )
        .unwrap();
        std::fs::write(repo.0.join("b_small.txt"), "y\n").unwrap();
        let b = diff_bundle(repo.0.to_str().unwrap()).unwrap();
        assert!(
            b.text.contains("[untracked content truncated]"),
            "oversized untracked content should be marked truncated"
        );
    }

    #[test]
    fn single_oversized_untracked_is_marked() {
        // Regression: a SINGLE untracked file bigger than the budget must still be
        // marked truncated — the marker must not depend on a second file existing.
        let repo = temp_repo();
        std::fs::write(
            repo.0.join("only_big.txt"),
            "z".repeat(MAX_UNTRACKED_CHARS + 100),
        )
        .unwrap();
        let b = diff_bundle(repo.0.to_str().unwrap()).unwrap();
        assert!(
            b.text.contains("[untracked content truncated]"),
            "a single oversized untracked file must be marked truncated"
        );
    }
}
