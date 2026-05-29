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
pub const REVIEW_POLICY_VERSION: u32 = 2;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// The recorded review base for a task.
#[derive(Debug)]
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

/// Advance the base at task start only when doing so cannot drop unreviewed work.
/// Pure (no IO) so it is unit-testable.
///
/// - BLOCKED / NEEDS_USER → never advance (unresolved review debt stays in scope).
/// - OPEN → advance ONLY when there are no committed-but-unreviewed commits since the
///   prior base. 'open' means the prior task started but never got a TERMINAL Stop
///   review; if it committed code and a new prompt arrives before that review (mid-turn
///   interrupt / reload / stop_active continuation), advancing to HEAD would baseline
///   PAST those commits and they would never be reviewed. So we keep the old base and
///   carry them into the next Stop. (v0.30 fix for the interrupt/reload escape.)
/// - APPROVED / none → advance. APPROVED genuinely reviewed base..HEAD and HEAD cannot
///   have moved after the turn ended; `none` is the first task (nothing prior to lose).
///
/// v0.29 note (unchanged): a model change does NOT gate the advance here — the hook
/// can't know the server's active pinned model. Re-reviewing already-frontier-advanced
/// committed work after a model change is a separate documented limitation; the future
/// design is a server-owned repo-level approved-span ledger.
fn should_advance(prev_status: Option<&str>, unreviewed_commits: bool) -> bool {
    match prev_status {
        Some(STATUS_BLOCKED) | Some(STATUS_NEEDS_USER) => false,
        Some(STATUS_OPEN) => !unreviewed_commits,
        _ => true,
    }
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
    // Derive the EFFECTIVE prior frontier (frontier_from normalizes a MISSING status to
    // OPEN, so a persisted-base-but-no-status row is correctly gated, not treated as
    // absent). `None` is reserved for truly-absent state (first task).
    let prev = read_raw(&root, session).map(|v| frontier_from(&v));
    let prev_status = prev.as_ref().map(|f| f.status.as_str());
    // Only the 'open' branch can drop committed work, so only it pays the git check:
    // an 'open' prior task with committed-but-unreviewed work (or an ambiguous/gone
    // base) must NOT be baselined past — keep the old base so the next Stop reviews it.
    let unreviewed_commits = if prev_status == Some(STATUS_OPEN) {
        match prev.as_ref().and_then(|f| f.base_spec()) {
            Some(base) => {
                let cd = crate::git::committed_delta(cwd, base);
                !cd.is_empty || cd.warning.is_some()
            }
            None => false,
        }
    } else {
        false
    };
    if !should_advance(prev_status, unreviewed_commits) {
        return; // unresolved debt OR unreviewed committed work: keep existing base + status
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
fn receipt_allows(v: &Value, expect_plan_hash: u64, expect_model_fp: u64) -> Option<u64> {
    // v0.23.0: the fast-path is honored ONLY when the frontier status is APPROVED.
    // Otherwise a receipt left behind from a prior approve could fast-allow the next
    // Stop even after a checkpoint (or Stop) recorded BLOCKED/NEEDS_USER debt against
    // the SAME diff — laundering unresolved debt. A blocked/needs_user/open status
    // makes the receipt inert until a genuine re-review re-approves (Codex code-gate).
    if v.get("status").and_then(Value::as_str) != Some(STATUS_APPROVED) {
        return None;
    }
    if v.get("review_policy_version").and_then(Value::as_u64) != Some(REVIEW_POLICY_VERSION as u64)
    {
        return None;
    }
    if v.get("allowed_plan_hash").and_then(Value::as_u64) != Some(expect_plan_hash) {
        return None;
    }
    // v0.29 (O1b): the receipt is bound to the review-model fingerprint it was approved
    // under. A model change (different fp) → mismatch → None → fresh review on the new
    // model. A missing `allowed_model_fp` (pre-O1b receipt) also mismatches → re-review.
    if v.get("allowed_model_fp").and_then(Value::as_u64) != Some(expect_model_fp) {
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
pub fn read_allowed_hash(
    cwd: &str,
    session: &str,
    expect_plan_hash: u64,
    expect_model_fp: u64,
) -> Option<u64> {
    let root = crate::git::repo_root(cwd)?;
    receipt_allows(
        &read_raw(&root, session)?,
        expect_plan_hash,
        expect_model_fp,
    )
}

/// v0.23.0: Advance the review frontier base to `reviewed_head` after a checkpoint
/// review APPROVED. Refuses (returns `Err`) if HEAD has moved since the review started
/// (TOCTOU guard) or the working tree became dirty during review. On success: writes
/// `base_kind="commit"`, `base_oid=reviewed_head`, `status=STATUS_APPROVED`.
///
/// `reviewed_head` MUST be the HEAD oid captured at bundle-assembly time. The function
/// re-reads current HEAD and rejects mismatch — otherwise a concurrent commit during
/// the (minutes-long) Codex review would silently advance the base past an unreviewed
/// commit.
pub fn checkpoint_approved(cwd: &str, session: &str, reviewed_head: &str) -> Result<(), String> {
    let root = crate::git::repo_root(cwd).ok_or_else(|| "no git repository found".to_string())?;
    // TOCTOU: current HEAD must still equal the SHA the reviewer judged.
    let current_head =
        crate::git::head_oid(cwd).ok_or_else(|| "could not read current HEAD".to_string())?;
    if current_head != reviewed_head {
        return Err(format!(
            "HEAD advanced from {reviewed_head} to {current_head} during review; \
             frontier unchanged"
        ));
    }
    // Re-check working tree is still clean. A mid-review edit would mean the new
    // bundle is not what was reviewed.
    match crate::git::diff_bundle(cwd) {
        Ok(b) if b.is_empty => {}
        Ok(_) => {
            return Err("working tree became dirty during review; frontier unchanged".to_string());
        }
        Err(e) => return Err(format!("could not re-check working tree: {e}")),
    }
    // Safe to advance: write base=commit(reviewed_head), status=approved. CRUCIAL:
    // strip any prior Stop approval receipt (`allowed_diff_hash`/`allowed_plan_hash`/
    // `review_policy_version`) — that receipt was scoped to the OLD base, so leaving it
    // would let `review_stop_inner`'s fast-path hydrate a stale `last_allowed_diff_hash`
    // and false-allow a bundle measured from the old frontier (Codex code-gate find).
    let mut v = read_raw(&root, session).unwrap_or_else(|| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("base_kind".into(), json!("commit"));
        o.insert("base_oid".into(), json!(reviewed_head));
        o.insert("status".into(), json!(STATUS_APPROVED));
        o.insert("updated_ms".into(), json!(now_ms() as u64));
        o.remove("allowed_diff_hash");
        o.remove("allowed_plan_hash");
        o.remove("review_policy_version");
    }
    write_raw(&root, session, &v);
    Ok(())
}

/// Record the approved-diff receipt (diff hash + the approved-plan scope hash it was
/// approved under + the current policy version) so [`read_allowed_hash`] can fast-path
/// an identical, same-scope diff after a reconnect. RMW that preserves the frontier
/// base/status; the atomic temp+rename in [`write_raw`] keeps a concurrent reader safe.
/// A new task (`on_task_start`) rewrites a fresh object WITHOUT these fields, so the
/// receipt is naturally cleared when the task changes.
pub fn set_allowed_hash(cwd: &str, session: &str, diff_hash: u64, plan_hash: u64, model_fp: u64) {
    let Some(root) = crate::git::repo_root(cwd) else {
        return;
    };
    let mut v = read_raw(&root, session).unwrap_or_else(|| json!({}));
    if let Some(o) = v.as_object_mut() {
        o.insert("allowed_diff_hash".into(), json!(diff_hash));
        o.insert("allowed_plan_hash".into(), json!(plan_hash));
        o.insert("review_policy_version".into(), json!(REVIEW_POLICY_VERSION));
        // v0.29 (O1b): bind the receipt to the active review-model fingerprint.
        o.insert("allowed_model_fp".into(), json!(model_fp));
        o.insert("updated_ms".into(), json!(now_ms() as u64));
    }
    write_raw(&root, session, &v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advances_only_without_unresolved_debt() {
        assert!(should_advance(None, false)); // first task
        assert!(should_advance(Some(STATUS_APPROVED), false)); // reviewed → advance
                                                               // OPEN advances ONLY when there is no committed-but-unreviewed work.
        assert!(should_advance(Some(STATUS_OPEN), false)); // open + clean → advance
        assert!(!should_advance(Some(STATUS_OPEN), true)); // open + unreviewed commits → carry
                                                           // Unresolved review debt never advances, regardless of commits.
        assert!(!should_advance(Some(STATUS_BLOCKED), false));
        assert!(!should_advance(Some(STATUS_NEEDS_USER), false));
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
            "status": STATUS_APPROVED,
            "allowed_diff_hash": 12345u64,
            "allowed_plan_hash": 999u64,
            "review_policy_version": REVIEW_POLICY_VERSION,
            "allowed_model_fp": 77u64,
        });
        // Same policy + plan scope + model fp + APPROVED status ⇒ honored.
        assert_eq!(receipt_allows(&good, 999, 77), Some(12345));
        // A different approved plan (same diff) must NOT fast-path — re-review the scope.
        assert_eq!(receipt_allows(&good, 1000, 77), None);
        // v0.29 (O1b): a different review-model fingerprint must NOT fast-path.
        assert_eq!(receipt_allows(&good, 999, 88), None);
        // A stale policy version (review semantics changed) must NOT fast-path.
        let stale = json!({
            "status": STATUS_APPROVED,
            "allowed_diff_hash": 12345u64,
            "allowed_plan_hash": 999u64,
            "review_policy_version": REVIEW_POLICY_VERSION + 1,
            "allowed_model_fp": 77u64,
        });
        assert_eq!(receipt_allows(&stale, 999, 77), None);
        // A pre-persistence frontier (status only, no receipt) ⇒ None (re-review).
        assert_eq!(
            receipt_allows(&json!({"status": "approved"}), 999, 77),
            None
        );
        // A pre-O1b receipt (no allowed_model_fp) ⇒ None (mismatch → re-review).
        assert_eq!(
            receipt_allows(
                &json!({"status": STATUS_APPROVED, "allowed_diff_hash": 12345u64, "allowed_plan_hash": 999u64, "review_policy_version": REVIEW_POLICY_VERSION}),
                999,
                77
            ),
            None
        );
    }

    #[test]
    fn receipt_allows_requires_approved_status() {
        // v0.23.0: a complete + matching receipt is INERT unless status == approved,
        // so checkpoint/Stop debt (blocked/needs_user) can't be laundered into an allow.
        let mk = |status: &str| {
            json!({
                "status": status,
                "allowed_diff_hash": 12345u64,
                "allowed_plan_hash": 999u64,
                "review_policy_version": REVIEW_POLICY_VERSION,
                "allowed_model_fp": 77u64,
            })
        };
        assert_eq!(receipt_allows(&mk(STATUS_APPROVED), 999, 77), Some(12345));
        assert_eq!(receipt_allows(&mk(STATUS_BLOCKED), 999, 77), None);
        assert_eq!(receipt_allows(&mk(STATUS_NEEDS_USER), 999, 77), None);
        assert_eq!(receipt_allows(&mk(STATUS_OPEN), 999, 77), None);
        // status field entirely absent ⇒ None.
        assert_eq!(
            receipt_allows(
                &json!({
                    "allowed_diff_hash": 12345u64,
                    "allowed_plan_hash": 999u64,
                    "review_policy_version": REVIEW_POLICY_VERSION,
                    "allowed_model_fp": 77u64,
                }),
                999,
                77
            ),
            None
        );
    }

    #[test]
    fn read_allowed_hash_none_when_status_blocked() {
        // Integration: a real receipt written via set_allowed_hash becomes inert once
        // set_status records BLOCKED, even though the receipt fields remain on disk.
        let repo = make_test_repo();
        let session = "sess-blocked";
        on_task_start(&repo, session);
        set_allowed_hash(&repo, session, 0xABCD, 0x42, 0x77);
        set_status(&repo, session, STATUS_APPROVED);
        assert_eq!(read_allowed_hash(&repo, session, 0x42, 0x77), Some(0xABCD));
        // v0.29 (O1b): a different model fp must no longer fast-allow.
        assert_eq!(read_allowed_hash(&repo, session, 0x42, 0x99), None);
        // Record debt: the same receipt must no longer fast-allow.
        set_status(&repo, session, STATUS_BLOCKED);
        assert_eq!(read_allowed_hash(&repo, session, 0x42, 0x77), None);
    }

    /// Minimal real git repo in a fresh temp dir (mirrors mcp tests' helper).
    fn make_test_repo() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "aibridge-frontier-test-{}-{}-{}",
            std::process::id(),
            n,
            now_ms()
        ));
        std::fs::create_dir_all(&p).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&p)
                .output()
                .expect("git");
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(p.join("seed.txt"), "seed").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        p.display().to_string()
    }

    /// Make a new commit in an existing test repo; return the new HEAD oid.
    fn commit_file(repo: &str, name: &str, content: &str) -> String {
        std::fs::write(std::path::Path::new(repo).join(name), content).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["add", "."]);
        run(&["commit", "-m", name]);
        crate::git::head_oid(repo).expect("head after commit")
    }

    #[test]
    fn open_with_committed_work_keeps_base_and_status() {
        // An 'open' task that COMMITTED code, then a new prompt before any Stop review:
        // on_task_start must NOT baseline past the unreviewed commit — keep base + open.
        let repo = make_test_repo();
        let s = "sess-open-carry";
        on_task_start(&repo, s); // base = C0, status open
        let c0 = crate::git::head_oid(&repo).unwrap();
        commit_file(&repo, "a.txt", "a"); // C1 — committed but never Stop-reviewed
        on_task_start(&repo, s); // open + C0..C1 non-empty → must carry, not advance
        let f = read(&repo, s).unwrap();
        match f.base {
            BaseKind::Commit(oid) => {
                assert_eq!(oid, c0, "kept C0 base so C0..C1 stays in review scope")
            }
            other => panic!("expected Commit base, got {other:?}"),
        }
        assert_eq!(f.status, STATUS_OPEN);
    }

    #[test]
    fn open_missing_status_normalized_to_open_carries_committed_work() {
        // A persisted frontier with a base but NO `status` field: frontier_from defaults
        // it to OPEN, so on_task_start must still carry committed-but-unreviewed work
        // (not treat it like absent state and advance). Regression for finding 2.
        let repo = make_test_repo();
        let s = "sess-missing-status";
        let root = crate::git::repo_root(&repo).unwrap();
        let c0 = crate::git::head_oid(&repo).unwrap();
        write_raw(
            &root,
            s,
            &json!({ "base_kind": "commit", "base_oid": c0, "updated_ms": now_ms() as u64 }),
        ); // deliberately NO "status" field
        commit_file(&repo, "a.txt", "a"); // C1
        on_task_start(&repo, s); // missing status → normalized to open → carry, not advance
        let f = read(&repo, s).unwrap();
        match f.base {
            BaseKind::Commit(oid) => {
                assert_eq!(
                    oid, c0,
                    "missing-status base carried (not advanced past C0..C1)"
                )
            }
            other => panic!("expected Commit base, got {other:?}"),
        }
    }

    #[test]
    fn approved_then_new_head_advances() {
        // Normal flow not regressed: a reviewed (APPROVED) prior task advances to HEAD.
        let repo = make_test_repo();
        let s = "sess-approved-advance";
        on_task_start(&repo, s);
        set_status(&repo, s, STATUS_APPROVED);
        let c1 = commit_file(&repo, "a.txt", "a");
        on_task_start(&repo, s); // approved → advance to C1
        let f = read(&repo, s).unwrap();
        match f.base {
            BaseKind::Commit(oid) => assert_eq!(oid, c1, "advanced to new HEAD after approval"),
            other => panic!("expected Commit base, got {other:?}"),
        }
        assert_eq!(f.status, STATUS_OPEN);
    }

    #[test]
    fn open_with_gone_base_fails_safe_keeps_base() {
        // Fail-safe: an 'open' frontier whose base object is GONE (ambiguous) must NOT
        // advance — committed_delta warns, so we carry rather than drop.
        let repo = make_test_repo();
        let s = "sess-gone-base";
        let root = crate::git::repo_root(&repo).unwrap();
        let bogus = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        write_raw(
            &root,
            s,
            &json!({
                "base_kind": "commit",
                "base_oid": bogus,
                "status": STATUS_OPEN,
                "updated_ms": now_ms() as u64,
            }),
        );
        on_task_start(&repo, s); // gone base ⇒ warning ⇒ unreviewed ⇒ keep base
        let f = read(&repo, s).unwrap();
        match f.base {
            BaseKind::Commit(oid) => {
                assert_eq!(
                    oid, bogus,
                    "fail-safe: kept the gone base instead of advancing"
                )
            }
            other => panic!("expected Commit base, got {other:?}"),
        }
    }
}
