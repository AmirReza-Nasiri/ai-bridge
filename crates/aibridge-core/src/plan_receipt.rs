//! Plan-gate approval RECEIPT — lets a reload-resumed task re-approve an UNCHANGED
//! plan INSTANTLY (no redundant minutes-long Codex round) WITHOUT weakening the
//! per-task gate: `start_epoch` still re-arms a PENDING epoch on every prompt, and
//! the fast-path lives ONLY inside the `plan_gate` tool. A resume is granted only
//! when ALL bindings still hold — else full review (fail-safe):
//!   - the SAME plan text (stable SHA-256, not a process-local hash),
//!   - the SAME repo identity (canonical toplevel + git dir),
//!   - the EXACT same HEAD (no lenient ancestor in v1 — a commit after review can
//!     change the security context via hooks / generated files),
//!   - a matching `PLAN_RECEIPT_VERSION` (bumped when the plan-review semantics
//!     change), within a short TTL.
//!
//! The receipt lives OUTSIDE the repo (`~/.ai-bridge/plan-state/<repo-id>.json`):
//! the in-repo plan-gate state is agent-writable, so trusting an in-repo receipt
//! would let a forged file mint approvals. Restored command classes are EXACTLY the
//! ones a real reviewer authorized (`RISK-APPROVED`) at the original approval — a
//! resume never grants a class the receipt didn't record, so a plan needing a new or
//! broader high-risk command changes the plan text → hash miss → full review.
//!
//! THREAT BOUNDARY (Codex-reviewed): like the whole plan gate, this assumes a
//! COOPERATIVE agent and defends against MISTAKES + corruption — a truncated, old,
//! foreign, or otherwise malformed receipt fails safe to a full review, and restored
//! classes are validated against the known set. It does NOT defend against a
//! deliberately-malicious agent forging a receipt: such an agent already has
//! filesystem READ (so any on-disk secret/MAC would be readable too) and gets Bash on
//! ANY real approval, so the plan gate is simply not the control for that adversary.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bump when the plan-review prompt/semantics change, so receipts minted under the
/// old semantics stop fast-pathing and force a fresh review. v2: the plan-gate prompt
/// gained the "process steps are not review criteria" + compile-deference clauses.
pub const PLAN_RECEIPT_VERSION: u32 = 2;

/// How long after approval a receipt may fast-path a reload-resume (24h — long
/// enough to survive a reload/restart, short enough to bound replay).
const RECEIPT_TTL_MS: u128 = 24 * 60 * 60 * 1000;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// SHA-256 (hex) over the given parts, each NUL-terminated so concatenation is
/// unambiguous. A stable persisted digest (unlike a process-local `DefaultHasher`).
fn sha256_hex(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    format!("{:x}", h.finalize())
}

/// Stable hash of the plan text, normalized (CRLF→LF, trimmed) so a trivial
/// whitespace/line-ending difference on resume doesn't needlessly miss.
fn plan_hash(plan: &str) -> String {
    let norm = plan.replace("\r\n", "\n");
    sha256_hex(&[norm.trim()])
}

/// Strong, stable repo identity: canonical toplevel + absolute git dir (case-folded
/// for Windows). `None` outside a repo. Binding the git dir resists a saved approval
/// being reused after the same PATH is taken over by a different repo/clone.
fn repo_identity(cwd: &str) -> Option<String> {
    let root = crate::git::repo_root(cwd)?;
    let canon_root = std::fs::canonicalize(&root)
        .map(|p| p.display().to_string())
        .unwrap_or(root);
    let git_dir = crate::git::absolute_git_dir(cwd).unwrap_or_default();
    Some(sha256_hex(&[
        &canon_root.to_lowercase(),
        &git_dir.to_lowercase(),
    ]))
}

fn receipt_path(cwd: &str) -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let id = repo_identity(cwd)?;
    Some(
        Path::new(&home)
            .join(".ai-bridge")
            .join("plan-state")
            .join(format!("{id}.json")),
    )
}

/// Pure authorization check: does this receipt JSON authorize a fast-path resume for
/// `want_plan_hash` at HEAD `head` and time `now`? Returns the approved command
/// classes if EVERY binding holds; `None` otherwise (→ full review). No IO, so it is
/// unit-testable.
fn receipt_authorizes(
    v: &Value,
    want_plan_hash: &str,
    head: &str,
    now: u128,
) -> Option<Vec<String>> {
    if v.get("plan_receipt_version").and_then(Value::as_u64) != Some(PLAN_RECEIPT_VERSION as u64) {
        return None;
    }
    if v.get("plan_hash").and_then(Value::as_str) != Some(want_plan_hash) {
        return None;
    }
    // EXACT HEAD equality — no lenient-ancestor in v1 (a post-approval commit can
    // change the security context: hooks, generated files, follow-up edits).
    if v.get("base_head").and_then(Value::as_str) != Some(head) {
        return None;
    }
    let created = v.get("created_ms").and_then(Value::as_u64).unwrap_or(0) as u128;
    // Reject a zero/absent timestamp, a future one (clock skew/tamper), or an expired one.
    if created == 0 || now < created || now - created > RECEIPT_TTL_MS {
        return None;
    }
    // `command_classes` MUST be present, an ARRAY, and contain ONLY KNOWN risk-class
    // names — so a malformed/truncated/forged receipt fails safe (→ None, full review)
    // instead of authorizing, and a resume can never restore an unrecognized class. An
    // EMPTY array is valid and common (a plan that needs no high-risk command).
    let arr = v.get("command_classes").and_then(Value::as_array)?;
    let mut classes: Vec<String> = Vec::new();
    for item in arr {
        let s = item.as_str()?; // a non-string element ⇒ malformed ⇒ fail safe
        if !crate::plan_gate::RISK_CLASSES.contains(&s) {
            return None; // unknown class ⇒ fail safe (never restore an unreviewed class)
        }
        if !classes.iter().any(|c| c == s) {
            classes.push(s.to_string()); // dedupe
        }
    }
    Some(classes)
}

/// If a saved receipt authorizes a fast-path resume of `plan` for this repo at the
/// CURRENT HEAD within TTL, return the approved command classes to restore. `None` →
/// the caller runs a full plan review. Fail-safe: an unborn repo (no HEAD to bind),
/// a missing/corrupt receipt, or any binding mismatch all yield `None`.
pub fn matching_classes(cwd: &str, plan: &str) -> Option<Vec<String>> {
    let head = crate::git::head_oid(cwd)?; // unborn repo → nothing to bind → full review
    let path = receipt_path(cwd)?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    receipt_authorizes(&v, &plan_hash(plan), &head, now_ms())
}

/// Write/refresh the receipt after a REAL Codex APPROVE so a later reload-resume of
/// the SAME plan at the SAME HEAD can fast-path. Atomic temp+rename. Best-effort: an
/// unborn repo / non-repo / unwritable home simply means no future fast-path.
pub fn write(cwd: &str, plan: &str, command_classes: &[String]) {
    let Some(head) = crate::git::head_oid(cwd) else {
        return;
    };
    let Some(path) = receipt_path(cwd) else {
        return;
    };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let v = json!({
        "plan_hash": plan_hash(plan),
        "command_classes": command_classes,
        "base_head": head,
        "plan_receipt_version": PLAN_RECEIPT_VERSION,
        "created_ms": now_ms() as u64,
    });
    let body = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, &body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_hash_is_stable_and_normalized() {
        // CRLF vs LF and surrounding whitespace must not change the hash.
        assert_eq!(plan_hash("a\r\nb\n"), plan_hash("  a\nb  "));
        // But a real content difference must.
        assert_ne!(plan_hash("plan one"), plan_hash("plan two"));
        // 64 hex chars (SHA-256), deterministic across calls.
        let h = plan_hash("the plan");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(h, plan_hash("the plan"));
    }

    fn receipt(plan_hash_val: &str, head: &str, created_ms: u64, ver: u32) -> Value {
        json!({
            "plan_hash": plan_hash_val,
            "command_classes": ["remote-publish"],
            "base_head": head,
            "plan_receipt_version": ver,
            "created_ms": created_ms,
        })
    }

    #[test]
    fn receipt_authorizes_only_when_every_binding_holds() {
        let now: u128 = 1_000_000_000_000;
        let ph = plan_hash("my plan");
        let head = "abc123";
        let good = receipt(&ph, head, now as u64, PLAN_RECEIPT_VERSION);

        // All bindings match → returns the approved classes.
        assert_eq!(
            receipt_authorizes(&good, &ph, head, now),
            Some(vec!["remote-publish".to_string()])
        );
        // Different plan → no resume.
        assert_eq!(
            receipt_authorizes(&good, &plan_hash("other"), head, now),
            None
        );
        // Different HEAD → no resume (exact equality).
        assert_eq!(receipt_authorizes(&good, &ph, "deadbeef", now), None);
        // Stale policy version → no resume.
        let oldver = receipt(&ph, head, now as u64, PLAN_RECEIPT_VERSION + 1);
        assert_eq!(receipt_authorizes(&oldver, &ph, head, now), None);
        // A v1 receipt (the pre-v0.29 default) is stale after the v2 bump → no resume.
        let v1 = receipt(&ph, head, now as u64, 1);
        assert_eq!(receipt_authorizes(&v1, &ph, head, now), None);
        // Expired (older than TTL) → no resume.
        let old = receipt(
            &ph,
            head,
            (now - RECEIPT_TTL_MS - 1) as u64,
            PLAN_RECEIPT_VERSION,
        );
        assert_eq!(receipt_authorizes(&old, &ph, head, now), None);
        // Future timestamp (clock skew / tamper) → no resume.
        let future = receipt(&ph, head, (now + 10_000) as u64, PLAN_RECEIPT_VERSION);
        assert_eq!(receipt_authorizes(&future, &ph, head, now), None);
        // Zero/absent timestamp → no resume.
        let zero = receipt(&ph, head, 0, PLAN_RECEIPT_VERSION);
        assert_eq!(receipt_authorizes(&zero, &ph, head, now), None);
        // Within TTL (just under) → still authorized.
        let recent = receipt(
            &ph,
            head,
            (now - RECEIPT_TTL_MS + 1) as u64,
            PLAN_RECEIPT_VERSION,
        );
        assert!(receipt_authorizes(&recent, &ph, head, now).is_some());
        // Empty/missing receipt object → no resume (fail-safe).
        assert_eq!(receipt_authorizes(&json!({}), &ph, head, now), None);

        // --- strict command_classes validation (fail-safe on malformed/forged) ---
        let base = |classes: Value| {
            json!({
                "plan_hash": ph,
                "base_head": head,
                "plan_receipt_version": PLAN_RECEIPT_VERSION,
                "created_ms": now as u64,
                "command_classes": classes,
            })
        };
        // Missing command_classes entirely (truncated/old receipt) → no resume.
        let no_field = json!({
            "plan_hash": ph, "base_head": head,
            "plan_receipt_version": PLAN_RECEIPT_VERSION, "created_ms": now as u64,
        });
        assert_eq!(receipt_authorizes(&no_field, &ph, head, now), None);
        // Not an array → no resume.
        assert_eq!(
            receipt_authorizes(&base(json!("remote-publish")), &ph, head, now),
            None
        );
        // Unknown class → no resume (never restore an unreviewed class).
        assert_eq!(
            receipt_authorizes(&base(json!(["launch-missiles"])), &ph, head, now),
            None
        );
        // Non-string element → no resume.
        assert_eq!(
            receipt_authorizes(&base(json!([123])), &ph, head, now),
            None
        );
        // Empty array is VALID (a plan that needs no high-risk command) → authorized.
        assert_eq!(
            receipt_authorizes(&base(json!([])), &ph, head, now),
            Some(vec![])
        );
        // Duplicate known classes are deduped.
        assert_eq!(
            receipt_authorizes(
                &base(json!(["db-migration", "db-migration"])),
                &ph,
                head,
                now
            ),
            Some(vec!["db-migration".to_string()])
        );
    }
}
