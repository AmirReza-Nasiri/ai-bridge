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

impl DiffBundle {
    /// Prepend the committed-since-task-base delta (from [`committed_delta`]) and
    /// refresh the hash + `is_empty`, so the Stop review covers work that was
    /// COMMITTED before the hook fired — not just the uncommitted tree.
    pub fn with_committed(self, committed_text: &str) -> DiffBundle {
        let text = format!(
            "{committed_text}\n\n# ── uncommitted working tree ──\n{}",
            self.text
        );
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        DiffBundle {
            hash: hasher.finish(),
            is_empty: false,
            text,
        }
    }
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

/// Git's well-known empty-tree object id — used as the "base" when a task started
/// in a repo with no commits yet, so a later first commit still diffs cleanly.
const EMPTY_TREE_OID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

fn git_path() -> Option<std::path::PathBuf> {
    DefaultPlatform::find_executable("git").ok()
}

/// Run a git command, returning trimmed stdout (empty on any failure).
fn git_stdout(cwd: &str, args: &[&str]) -> String {
    let Some(git) = git_path() else {
        return String::new();
    };
    DefaultPlatform::command_for(&git)
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Run a git command, returning whether it EXITED 0 (for predicate queries like
/// object existence / ancestry, where the exit code is the answer).
fn git_ok(cwd: &str, args: &[&str]) -> bool {
    let Some(git) = git_path() else {
        return false;
    };
    DefaultPlatform::command_for(&git)
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a CHECKED git command: `Some(trimmed stdout)` only when git EXITED 0 with
/// non-empty output. Needed where stdout is meaningless on failure — e.g. plain
/// `rev-parse HEAD` prints the literal "HEAD" (and errors) on an unborn repo.
fn git_checked(cwd: &str, args: &[&str]) -> Option<String> {
    let git = git_path()?;
    let out = DefaultPlatform::command_for(&git)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The repository root, or `None` outside a repo. Keys per-repo review state
/// independent of the (possibly sub-dir) cwd.
pub fn repo_root(cwd: &str) -> Option<String> {
    git_checked(cwd, &["rev-parse", "--show-toplevel"])
}

/// The current HEAD commit id, or `None` on an UNBORN repo (no commits yet).
/// Uses `--verify HEAD^{commit}` so an unborn HEAD returns `None` instead of the
/// literal "HEAD" that plain `rev-parse HEAD` prints while exiting non-zero.
pub fn head_oid(cwd: &str) -> Option<String> {
    git_checked(cwd, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
}

/// True iff `oid` resolves to an existing commit object in this repo.
pub fn object_exists(cwd: &str, oid: &str) -> bool {
    git_ok(cwd, &["cat-file", "-e", &format!("{oid}^{{commit}}")])
}

/// The absolute git dir for `cwd` (the checkout's `.git`, or a linked worktree's git
/// dir), or `None` outside a repo. Used to strengthen a repo's identity so a saved
/// approval can't be reused after the path is reused by a different repo/clone.
pub fn absolute_git_dir(cwd: &str) -> Option<String> {
    git_checked(cwd, &["rev-parse", "--absolute-git-dir"])
}

/// True iff `base` is an ancestor of `HEAD`.
fn is_ancestor(cwd: &str, base: &str) -> bool {
    git_ok(cwd, &["merge-base", "--is-ancestor", base, "HEAD"])
}

/// The task's review base: a recorded commit, or the empty tree (the repo had no
/// commits when the task started).
pub enum BaseSpec<'a> {
    Commit(&'a str),
    EmptyTree,
}

/// The work committed since a task's base. Best-effort and NEVER a silent skip: a
/// diverged or missing base still yields a (warned) net diff so the committed work
/// is reviewed, not dropped.
pub struct CommittedDelta {
    pub text: String,
    pub is_empty: bool,
    /// Set when the base wasn't a clean ancestor (diverged / gone). The reason is
    /// ALSO embedded at the top of `text` so the reviewer and user both see it.
    pub warning: Option<String>,
}

/// Compute the work COMMITTED since a task's base (so a `git commit` made before
/// the Stop hook can't hide it). Subtree-scoped, `.ai-bridge` excluded — same scope
/// as [`diff_bundle`]. Degrades SAFELY, never silently skipping:
/// - no HEAD yet ⇒ empty (nothing committed);
/// - base diverged from HEAD (rebase/reset/branch) ⇒ the net `base↔HEAD` tree diff,
///   warned;
/// - base object gone (gc/amend) ⇒ the FULL tree (empty-tree base) — strictly
///   conservative — warned.
pub fn committed_delta(cwd: &str, base: BaseSpec) -> CommittedDelta {
    // No HEAD yet (repo has no commits) ⇒ nothing committed regardless of base.
    if head_oid(cwd).is_none() {
        return CommittedDelta {
            text: String::new(),
            is_empty: true,
            warning: None,
        };
    }
    // Pick the diff source tree + any warning — never bail to a silent skip.
    let (from, warning) = match base {
        BaseSpec::EmptyTree => (EMPTY_TREE_OID.to_string(), None),
        BaseSpec::Commit(oid) => {
            if !object_exists(cwd, oid) {
                (
                    EMPTY_TREE_OID.to_string(),
                    Some(format!(
                        "recorded review base {oid} is GONE (rebase/amend/gc) — reviewing the \
                         FULL tree to be safe"
                    )),
                )
            } else if !is_ancestor(cwd, oid) {
                (
                    oid.to_string(),
                    Some(format!(
                        "review base {oid} is no longer an ancestor of HEAD \
                         (rebase/reset/branch change) — showing the net base↔HEAD diff"
                    )),
                )
            } else {
                (oid.to_string(), None)
            }
        }
    };
    // `git diff <from> HEAD` compares the two endpoints' trees — correct for a clean
    // ancestor, a diverged base, and the empty tree alike.
    let diff = git_stdout(
        cwd,
        &[
            "--no-pager",
            "diff",
            &from,
            "HEAD",
            "--",
            ".",
            ":(exclude).ai-bridge",
        ],
    );
    let list = if from == EMPTY_TREE_OID {
        git_stdout(cwd, &["--no-pager", "log", "--oneline", "HEAD", "--", "."])
    } else {
        git_stdout(
            cwd,
            &[
                "--no-pager",
                "log",
                "--oneline",
                &format!("{from}..HEAD"),
                "--",
                ".",
            ],
        )
    };
    let is_empty = diff.trim().is_empty();
    let warn_line = warning
        .as_ref()
        .map(|w| format!("# ⚠ {w}\n"))
        .unwrap_or_default();
    let text = format!(
        "{warn_line}# commits since task start ({from}..HEAD)\n{list}\n\n\
         # committed diff since task start\n{diff}"
    );
    CommittedDelta {
        text,
        is_empty,
        warning,
    }
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

    #[test]
    fn repo_root_head_and_object_resolve() {
        let repo = temp_repo();
        let dir = repo.0.to_str().unwrap();
        assert!(repo_root(dir).is_some());
        let head = head_oid(dir).expect("seed commit gives a HEAD");
        assert!(object_exists(dir, &head));
        assert!(!object_exists(
            dir,
            "0123456789012345678901234567890123456789"
        ));
    }

    #[test]
    fn committed_delta_catches_committed_work() {
        // THE bypass closure: work COMMITTED after the task base must be reviewable,
        // so `commit` before Stop can no longer hide it.
        let repo = temp_repo();
        let dir = repo.0.to_str().unwrap();
        let base = head_oid(dir).unwrap();
        std::fs::write(repo.0.join("feature.rs"), "fn boom() { let _x = nope; }\n").unwrap();
        git(&repo.0, &["add", "-A"]);
        git(&repo.0, &["commit", "-qm", "feat"]);
        let cd = committed_delta(dir, BaseSpec::Commit(&base));
        assert!(!cd.is_empty, "committed work must be detected");
        assert!(cd.warning.is_none(), "clean ancestor base ⇒ no warning");
        assert!(
            cd.text.contains("feature.rs"),
            "committed path should appear"
        );
        assert!(cd.text.contains("nope"), "committed content should appear");
    }

    #[test]
    fn committed_delta_empty_when_head_equals_base() {
        let repo = temp_repo();
        let dir = repo.0.to_str().unwrap();
        let base = head_oid(dir).unwrap();
        let cd = committed_delta(dir, BaseSpec::Commit(&base));
        assert!(cd.is_empty);
        assert!(cd.warning.is_none());
    }

    #[test]
    fn committed_delta_diverged_reviews_net_diff_with_warning() {
        let repo = temp_repo();
        let dir = repo.0.to_str().unwrap();
        let a = head_oid(dir).unwrap();
        std::fs::write(repo.0.join("b.txt"), "b\n").unwrap();
        git(&repo.0, &["add", "-A"]);
        git(&repo.0, &["commit", "-qm", "b"]);
        let b = head_oid(dir).unwrap();
        git(&repo.0, &["reset", "--hard", &a]); // HEAD back to A → B isn't an ancestor
                                                // Base B exists but diverged: review the net B↔HEAD diff, warned — not skipped.
        let cd = committed_delta(dir, BaseSpec::Commit(&b));
        assert!(cd.warning.is_some(), "diverged base must warn");
        assert!(!cd.is_empty, "net diff (removal of b.txt) is non-empty");
        assert!(cd.text.contains("b.txt"));
    }

    #[test]
    fn committed_delta_base_missing_reviews_full_tree_with_warning() {
        let repo = temp_repo();
        let dir = repo.0.to_str().unwrap();
        // Unknown base oid ⇒ fall back to the FULL tree (empty-tree base), warned —
        // strictly conservative, never a silent skip.
        let cd = committed_delta(
            dir,
            BaseSpec::Commit("0123456789012345678901234567890123456789"),
        );
        assert!(cd.warning.is_some(), "missing base must warn");
        assert!(!cd.is_empty);
        assert!(
            cd.text.contains("seed.txt"),
            "full tree includes seeded file"
        );
    }

    #[test]
    fn committed_delta_empty_tree_shows_all_committed() {
        let repo = temp_repo();
        let dir = repo.0.to_str().unwrap();
        let cd = committed_delta(dir, BaseSpec::EmptyTree);
        assert!(!cd.is_empty);
        assert!(cd.warning.is_none());
        assert!(
            cd.text.contains("seed.txt"),
            "empty-tree base shows all files"
        );
    }

    #[test]
    fn with_committed_combines_and_unempties() {
        let repo = temp_repo();
        // A clean tree → empty uncommitted bundle...
        let b = diff_bundle(repo.0.to_str().unwrap()).unwrap();
        assert!(b.is_empty);
        // ...but folding in a committed delta makes it non-empty + present.
        let combined = b.with_committed("# committed diff since task start\nfeature change");
        assert!(!combined.is_empty);
        assert!(combined.text.contains("feature change"));
        assert!(combined.text.contains("uncommitted working tree"));
    }
}
