//! Stop-gate review frontier — the commit baseline a task's Stop review measures
//! from, so committing work does NOT hide it from review (closing the
//! "commit before Stop ⇒ clean tree ⇒ no review" bypass).
//!
//! State lives OUTSIDE the repo at
//! `~/.ai-bridge/review-state/<repo-key>/<session>.json` on purpose: a repo-local
//! file under `.ai-bridge` is both agent-writable AND excluded from the review
//! bundle, so trusting it there would just relocate the bypass (Codex review).
//!
//! The base advances to HEAD at task start (`UserPromptSubmit`) ONLY when the
//! previous task's review was resolved (approved / open / none) — NEVER over
//! unresolved review debt (a block / fail-ask), so blocked-but-then-committed work
//! is never quietly dropped from the next review.

use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STATUS_OPEN: &str = "open";
pub const STATUS_APPROVED: &str = "approved";
pub const STATUS_BLOCKED: &str = "blocked";
pub const STATUS_NEEDS_USER: &str = "needs_user";

/// Version of the review SEMANTICS (Stop prompt + bundle format) that an approved-diff
/// receipt was minted under. A persisted "this diff is approved" fast-path is honored
/// ONLY when the receipt's version matches — so changing the review prompt/bundle shape
/// (which can change what "approved" means) automatically invalidates old receipts and
/// forces a fresh review. Bump this whenever that semantics changes.
pub const REVIEW_POLICY_VERSION: u32 = 1;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The recorded review base for a task.
pub enum BaseKind {
    /// A specific commit (HEAD at task start).
    Commit(String),
    /// The repo had no commits at task start; review from the empty tree.
    EmptyTree,
    /// No base recorded (the task-start hook never ran for this session).
    None,
}

/// A task's review frontier: where to measure committed work from, plus the
/// previous review's resolution.
pub struct Frontier {
    pub base: BaseKind,
    pub status: String,
}

impl Frontier {
    /// Map to the git layer's base spec for [`crate::git::committed_delta`].
    /// `None` ⇒ no base recorded (caller falls back to uncommitted-only).
    pub fn base_spec(&self) -> Option<crate::git::BaseSpec<'_>> {
        match &self.base {
            BaseKind::Commit(oid) => Some(crate::git::BaseSpec::Commit(oid)),
            BaseKind::EmptyTree => Some(crate::git::BaseSpec::EmptyTree),
            BaseKind::None => None,
        }
    }
}

/// Advance the base at task start only when there is no unresolved review debt.
/// Pure (no IO) so it is unit-testable.
fn should_advance(prev_status: Option<&str>) -> bool {
    !matches!(prev_status, Some(STATUS_BLOCKED) | Some(STATUS_NEEDS_USER))
}

fn global_dir() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    Some(Path::new(&home).join(".ai-bridge").join("review-state"))
}

/// Stable per-repo key from the CANONICAL root path (case-folded so Windows'
/// case-insensitive FS doesn't split one repo into two keys).
fn repo_key(repo_root: &str) -> String {
    let canon = std::fs::canonicalize(repo_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| repo_root.to_string());
    let mut h = DefaultHasher::new();
    canon.to_lowercase().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Make a session id safe as a filename component (bounded, no path separators).
fn sanitize_session(session: &str) -> String {
    let s: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if s.is_empty() {
        "default".to_string()
    } else {
        s
    }
}

fn state_path(repo_root: &str, session: &str) -> Option<PathBuf> {
    Some(
        global_dir()?
            .join(repo_key(repo_root))
            .join(format!("{}.json", sanitize_session(session))),
    )
}

fn read_raw(repo_root: &str, session: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(state_path(repo_root, session)?).ok()?).ok()
}

fn write_raw(repo_root: &str, session: &str, v: &Value) {
    let Some(path) = state_path(repo_root, session) else {
        return;
    };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let body = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string());
    let tmp = path.with_file_name(format!(
        "{}.tmp.{}.{}",
        sanitize_session(session),
        std::process::id(),
        now_ms()
    ));
    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn frontier_from(v: &Value) -> Frontier {
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or(STATUS_OPEN)
        .to_string();
    let base = match v.get("base_kind").and_then(Value::as_str) {
        Some("empty_tree") => BaseKind::EmptyTree,
        Some("commit") => match v.get("base_oid").and_then(Value::as_str) {
            Some(o) if !o.is_empty() => BaseKind::Commit(o.to_string()),
            _ => BaseKind::None,
        },
        _ => BaseKind::None,
    };
    Frontier { base, status }
}

/// Read the recorded frontier for (`cwd`'s repo, `session`). `None` when `cwd`
/// isn't a repo or no state exists yet (caller treats that as "not active for this
/// session" and falls back to reviewing the uncommitted tree only).
pub fn read(cwd: &str, session: &str) -> Option<Frontier> {
    let root = crate::git::repo_root(cwd)?;
    Some(frontier_from(&read_raw(&root, session)?))
}

/// `UserPromptSubmit` hook entry: parse the payload and record this task's review
/// base. Independent of the plan gate (the Stop gate needs a base even when the
/// plan gate is off). Never blocks the prompt.
pub fn on_user_prompt(stdin: &str) {
    let v: Value = match serde_json::from_str(stdin) {
        Ok(v) => v,
        Err(_) => return,
    };
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let session = v
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("default");
    on_task_start(cwd, session);
}

/// Task start (`UserPromptSubmit`): set the review base to current HEAD (or the
/// empty tree if the repo has no commits), UNLESS the previous task's review is
/// still unresolved — then keep the old base so its unreviewed work stays in scope.
pub fn on_task_start(cwd: &str, session: &str) {
    let Some(root) = crate::git::repo_root(cwd) else {
        return; // not a git repo: nothing to baseline
    };
    let prev = read_raw(&root, session);
    let prev_status = prev
        .as_ref()
        .and_then(|v| v.get("status").and_then(Value::as_str));
    if !should_advance(prev_status) {
        return; // unresolved debt: keep existing base + status
    }
    let (kind, oid) = match crate::git::head_oid(cwd) {
        Some(h) => ("commit", h),
        None => ("empty_tree", String::new()),
    };
    write_raw(
        &root,
        session,
        &json!({
            "base_kind": kind,
            "base_oid": oid,
            "status": STATUS_OPEN,
            "updated_ms": now_ms() as u64,
        }),
    );
}

/// Record a Stop review's outcome so the NEXT task start knows whether it may
/// advance the base. Leaves the recorded base unchanged.
pub fn set_status(cwd: &str, session: &str, status: &str) {
    let Some(root) = crate::git::repo_root(cwd) else {
        return;
    };
    let mut v = read_raw(&root, session).unwrap_or_else(|| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("status".into(), json!(status));
        o.insert("updated_ms".into(), json!(now_ms() as u64));
    }
    write_raw(&root, session, &v);
}

/// Pure receipt check: return the persisted approved-diff hash, but ONLY when the
/// receipt still matches the current review semantics (`review_policy_version`) AND
/// the current approved-plan scope (`expect_plan_hash`). Any mismatch — or a
/// pre-persistence frontier with no receipt fields — yields `None` so the Stop gate
/// re-reviews. Fail-safe by construction: a stale receipt can never auto-allow.
fn receipt_allows(v: &Value, expect_plan_hash: u64) -> Option<u64> {
    if v.get("review_policy_version").and_then(Value::as_u64) != Some(REVIEW_POLICY_VERSION as u64)
    {
        return None;
    }
    if v.get("allowed_plan_hash").and_then(Value::as_u64) != Some(expect_plan_hash) {
        return None;
    }
    v.get("allowed_diff_hash").and_then(Value::as_u64)
}

/// The persisted "this exact diff was already approved" hash for the Stop gate's
/// fast-path, so an MCP reconnect (VS Code reload) does NOT force a redundant,
/// minutes-long re-review of an unchanged-and-approved diff. Honored only when the
/// receipt's policy version + approved-plan scope still match (see [`receipt_allows`]);
/// otherwise `None` → the gate reviews normally. The hash is the same `DefaultHasher`
/// value the in-memory fast-path uses (`DiffBundle::hash`) so the two paths agree;
/// a binary upgrade that changes that hash just misses → re-review (never a false allow).
pub fn read_allowed_hash(cwd: &str, session: &str, expect_plan_hash: u64) -> Option<u64> {
    let root = crate::git::repo_root(cwd)?;
    receipt_allows(&read_raw(&root, session)?, expect_plan_hash)
}

/// Record the approved-diff receipt (diff hash + the approved-plan scope hash it was
/// approved under + the current policy version) so [`read_allowed_hash`] can fast-path
/// an identical, same-scope diff after a reconnect. RMW that preserves the frontier
/// base/status; the atomic temp+rename in [`write_raw`] keeps a concurrent reader safe.
/// A new task (`on_task_start`) rewrites a fresh object WITHOUT these fields, so the
/// receipt is naturally cleared when the task changes.
pub fn set_allowed_hash(cwd: &str, session: &str, diff_hash: u64, plan_hash: u64) {
    let Some(root) = crate::git::repo_root(cwd) else {
        return;
    };
    let mut v = read_raw(&root, session).unwrap_or_else(|| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("allowed_diff_hash".into(), json!(diff_hash));
        o.insert("allowed_plan_hash".into(), json!(plan_hash));
        o.insert("review_policy_version".into(), json!(REVIEW_POLICY_VERSION));
        o.insert("updated_ms".into(), json!(now_ms() as u64));
    }
    write_raw(&root, session, &v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_only_without_unresolved_debt() {
        assert!(should_advance(None)); // first task
        assert!(should_advance(Some(STATUS_OPEN)));
        assert!(should_advance(Some(STATUS_APPROVED)));
        assert!(!should_advance(Some(STATUS_BLOCKED)));
        assert!(!should_advance(Some(STATUS_NEEDS_USER)));
    }

    #[test]
    fn repo_key_is_stable_and_case_folded() {
        let a = repo_key("relative/path/that/does/not/exist");
        let b = repo_key("relative/path/that/does/not/exist");
        assert_eq!(a, b, "same input ⇒ same key");
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn sanitize_session_is_filesafe() {
        assert_eq!(sanitize_session("abc-123_DEF"), "abc-123_DEF");
        assert_eq!(sanitize_session("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_session(""), "default");
        assert!(sanitize_session(&"x".repeat(500)).len() <= 120);
    }

    #[test]
    fn frontier_parses_base_kinds() {
        let c = frontier_from(&json!({"base_kind":"commit","base_oid":"deadbeef","status":"open"}));
        assert!(matches!(c.base, BaseKind::Commit(o) if o == "deadbeef"));
        let e = frontier_from(&json!({"base_kind":"empty_tree","status":"approved"}));
        assert!(matches!(e.base, BaseKind::EmptyTree));
        assert_eq!(e.status, "approved");
        // commit kind but blank oid ⇒ treated as no base (safe)
        let n = frontier_from(&json!({"base_kind":"commit","base_oid":"","status":"open"}));
        assert!(matches!(n.base, BaseKind::None));
        // missing fields ⇒ None base, open status
        let d = frontier_from(&json!({}));
        assert!(matches!(d.base, BaseKind::None));
        assert_eq!(d.status, STATUS_OPEN);
    }

    #[test]
    fn receipt_allows_only_on_matching_policy_and_plan() {
        let good = json!({
            "allowed_diff_hash": 12345u64,
            "allowed_plan_hash": 999u64,
            "review_policy_version": REVIEW_POLICY_VERSION,
        });
        // Same policy + same approved-plan scope ⇒ the diff fast-path is honored.
        assert_eq!(receipt_allows(&good, 999), Some(12345));
        // A different approved plan (same diff) must NOT fast-path — re-review the scope.
        assert_eq!(receipt_allows(&good, 1000), None);
        // A stale policy version (review semantics changed) must NOT fast-path.
        let stale = json!({
            "allowed_diff_hash": 12345u64,
            "allowed_plan_hash": 999u64,
            "review_policy_version": REVIEW_POLICY_VERSION + 1,
        });
        assert_eq!(receipt_allows(&stale, 999), None);
        // A pre-persistence frontier (status only, no receipt) ⇒ None (re-review).
        assert_eq!(receipt_allows(&json!({"status": "approved"}), 999), None);
        // Plan-scope present but the diff hash missing ⇒ nothing to fast-path.
        assert_eq!(
            receipt_allows(
                &json!({"allowed_plan_hash": 999u64, "review_policy_version": REVIEW_POLICY_VERSION}),
                999
            ),
            None
        );
    }
}
