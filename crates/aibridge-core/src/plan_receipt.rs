//! Plan-gate approval RECEIPT — lets a reload-resumed task re-approve an UNCHANGED
//! plan INSTANTLY (no redundant minutes-long Codex round) WITHOUT weakening the
//! per-task gate: `start_epoch` still re-arms a PENDING epoch on every prompt, and
//! the fast-path lives ONLY inside the `plan_gate` tool. A resume is granted only
//! when ALL bindings still hold — else full review (fail-safe):
//!   - the SAME plan text (stable SHA-256, not a process-local hash),
//!   - the SAME repo identity (canonical toplevel + git dir),
//!   - HEAD is the SAME commit, OR (v3) an ANCESTOR of the current HEAD — but an
//!     ancestor only resumes when NO high-risk command classes were authorized (a
//!     high-risk plan still needs the exact HEAD); the committed delta since the base
//!     stays in Stop-review scope (the frontier no longer advances past it),
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
/// v3: resume is no longer EXACT-HEAD only — a base that is an ancestor of HEAD may
/// resume too (guarded; see `head_match` / `receipt_authorizes`).
/// v4 (v0.31 P3): risk grants are STRUCTURED (class+shape), so a flat-class v3 receipt
/// must NOT silently authorize a WIDENED variant — every pre-v4 receipt fails closed
/// (full review), and a v4 receipt carries a `risk_grants` array.
pub const PLAN_RECEIPT_VERSION: u32 = 4;

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

/// How the receipt's `base_head` relates to the current HEAD (computed by [`head_match`]
/// with git; kept out of [`receipt_authorizes`] so the authorization policy stays pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadMatch {
    /// `base_head` == current HEAD (resume with any approved classes — v1 behavior).
    Exact,
    /// `base_head` is an ANCESTOR of HEAD (my own commits on top of the reviewed base).
    /// v3: resume ONLY when no high-risk command classes were authorized.
    Ancestor,
    /// No usable relation (different/garbage/gone base) → full review.
    No,
}

/// Pure authorization check: does this receipt JSON authorize a fast-path resume for
/// `want_plan_hash` given the HEAD relation `head_match` and time `now`? Returns the
/// approved risk GRANTS (class+shape) if EVERY binding holds; `None` otherwise
/// (→ full review). No IO, so it is unit-testable.
fn receipt_authorizes(
    v: &Value,
    want_plan_hash: &str,
    head_match: HeadMatch,
    want_effort: &str,
    now: u128,
) -> Option<Vec<crate::plan_gate::RiskGrant>> {
    if v.get("plan_receipt_version").and_then(Value::as_u64) != Some(PLAN_RECEIPT_VERSION as u64) {
        return None;
    }
    if v.get("plan_hash").and_then(Value::as_str) != Some(want_plan_hash) {
        return None;
    }
    // v0.30 #4: the receipt must have been minted under the SAME plan-review effort, so a
    // server restart with a changed effort forces a fresh review. ABSENT field → legacy
    // "xhigh" (pre-#4 receipts, all minted at xhigh); a MALFORMED present value (non-string)
    // fails closed → None, preserving the strict-receipt invariant.
    let receipt_effort: Option<&str> = match v.get("plan_review_effort") {
        None => Some("xhigh"),
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => None,
    };
    match receipt_effort {
        Some(e) if e == want_effort => {}
        _ => return None,
    }
    let created = v.get("created_ms").and_then(Value::as_u64).unwrap_or(0) as u128;
    // Reject a zero/absent timestamp, a future one (clock skew/tamper), or an expired one.
    if created == 0 || now < created || now - created > RECEIPT_TTL_MS {
        return None;
    }
    // v4 (v0.31 P3): `risk_grants` MUST be present, an ARRAY, and contain ONLY well-formed
    // grants — each an object with a KNOWN risk-class `class` and a KNOWN `shape`
    // (standard/widened). A missing field (a v3 receipt that the version gate already
    // rejected, or a truncated/forged v4), a non-array, a non-object element, an unknown
    // class, or an unknown shape ALL fail safe (→ None, full review) — so a resume can
    // never restore an unrecognized class or silently WIDEN a grant. An EMPTY array is
    // valid and common (a plan that needs no high-risk command).
    let arr = v.get("risk_grants").and_then(Value::as_array)?;
    let mut grants: Vec<crate::plan_gate::RiskGrant> = Vec::new();
    for item in arr {
        // `from_value` accepts ONLY a well-formed object with a KNOWN class + KNOWN shape;
        // a non-object element, unknown class, or unknown shape ⇒ None ⇒ fail safe (never
        // restore an unreviewed class or silently widen a grant).
        let g = crate::plan_gate::RiskGrant::from_value(item)?;
        if !grants.contains(&g) {
            grants.push(g); // dedupe
        }
    }
    // HEAD relation gate (v3, extended for v4). Exact resumes with any grants (v1
    // behavior); an Ancestor (my own commits atop the reviewed base) resumes ONLY when NO
    // high-risk grants were authorized — a plan that authorized publish/migrate/destructive
    // (or ANY widened variant) still needs exact HEAD or a fresh review, since the security
    // context can shift across those commits. (The committed delta itself is still
    // Stop-reviewed: the frontier no longer advances past it.) A non-empty grant set — even
    // an all-standard one — keeps the v3 rule that any authorized high-risk class needs
    // exact HEAD.
    match head_match {
        HeadMatch::No => None,
        HeadMatch::Exact => Some(grants),
        HeadMatch::Ancestor if grants.is_empty() => Some(grants),
        HeadMatch::Ancestor => None,
    }
}

/// Classify the receipt's `base_head` against the current `head` (git IO). Fail-closed:
/// a non-FULL / non-hex / gone / non-ancestor base — or any git error — yields `No`.
/// Requiring `base_head.len() == head.len()` (both full OIDs from `head_oid`) + all-hex
/// BEFORE any git lookup rejects a unique ABBREVIATED prefix that git would resolve.
fn head_match(cwd: &str, base_head: &str, head: &str) -> HeadMatch {
    if base_head.len() != head.len() || !base_head.chars().all(|c| c.is_ascii_hexdigit()) {
        return HeadMatch::No;
    }
    if base_head == head {
        return HeadMatch::Exact;
    }
    if crate::git::object_exists(cwd, base_head) && crate::git::is_ancestor(cwd, base_head) {
        return HeadMatch::Ancestor;
    }
    HeadMatch::No
}

/// If a saved receipt authorizes a fast-path resume of `plan` for this repo at — or an
/// ancestor of — the CURRENT HEAD within TTL, return the approved risk GRANTS
/// (class+shape) to restore. `None` → the caller runs a full plan review. Fail-safe: an
/// unborn repo (no HEAD to bind), a missing/corrupt receipt, or any binding mismatch all
/// yield `None`.
pub fn matching_classes(
    cwd: &str,
    plan: &str,
    want_effort: &str,
) -> Option<Vec<crate::plan_gate::RiskGrant>> {
    let head = crate::git::head_oid(cwd)?; // unborn repo → nothing to bind → full review
    let path = receipt_path(cwd)?;
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
    let base_head = v.get("base_head").and_then(Value::as_str)?;
    receipt_authorizes(
        &v,
        &plan_hash(plan),
        head_match(cwd, base_head, &head),
        want_effort,
        now_ms(),
    )
}

/// Write/refresh the receipt after a REAL Codex APPROVE so a later reload-resume of the
/// SAME plan can fast-path. `grants` are the reviewer-authorized risk grants (class+shape,
/// v0.31 P3). `effort` is the PINNED plan-review effort the review actually ran at
/// (v0.30 #4) — stamped so a later server restart with a different effort forces a fresh
/// review. Atomic temp+rename. Best-effort: an unborn repo / non-repo / unwritable home
/// simply means no future fast-path. The legacy flat `command_classes` is written too
/// (back-compat / display); the authoritative resume field is `risk_grants`.
pub fn write(cwd: &str, plan: &str, grants: &[crate::plan_gate::RiskGrant], effort: &str) {
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
    let command_classes: Vec<String> = grants.iter().map(|g| g.class.clone()).collect();
    let v = json!({
        "plan_hash": plan_hash(plan),
        "command_classes": command_classes,
        "risk_grants": crate::plan_gate::RiskGrant::vec_to_value(grants),
        "base_head": head,
        "plan_receipt_version": PLAN_RECEIPT_VERSION,
        "plan_review_effort": effort,
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

    use crate::plan_gate::RiskGrant;

    fn grant(class: &str, shape: &str) -> RiskGrant {
        RiskGrant {
            class: class.to_string(),
            shape: shape.to_string(),
        }
    }

    /// A v4 receipt carrying a single standard `remote-publish` risk grant.
    fn receipt(plan_hash_val: &str, head: &str, created_ms: u64, ver: u32) -> Value {
        json!({
            "plan_hash": plan_hash_val,
            "command_classes": ["remote-publish"],
            "risk_grants": [{ "class": "remote-publish", "shape": "standard" }],
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

        // Exact HEAD + all bindings → returns the approved grants.
        assert_eq!(
            receipt_authorizes(&good, &ph, HeadMatch::Exact, "xhigh", now),
            Some(vec![grant("remote-publish", "standard")])
        );
        // No usable HEAD relation → no resume.
        assert_eq!(
            receipt_authorizes(&good, &ph, HeadMatch::No, "xhigh", now),
            None
        );
        // Ancestor + NON-empty grants → no resume (high-risk needs exact HEAD). KEY guard.
        assert_eq!(
            receipt_authorizes(&good, &ph, HeadMatch::Ancestor, "xhigh", now),
            None
        );
        // Different plan → no resume.
        assert_eq!(
            receipt_authorizes(&good, &plan_hash("other"), HeadMatch::Exact, "xhigh", now),
            None
        );
        // Stale policy version → no resume.
        let oldver = receipt(&ph, head, now as u64, PLAN_RECEIPT_VERSION + 1);
        assert_eq!(
            receipt_authorizes(&oldver, &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // A v3 receipt (the pre-P3 default, even with valid risk_grants) is stale after the
        // v4 bump → no resume (so a flat-era receipt can never authorize a widened variant).
        let v3 = receipt(&ph, head, now as u64, 3);
        assert_eq!(
            receipt_authorizes(&v3, &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // Expired (older than TTL) → no resume.
        let old = receipt(
            &ph,
            head,
            (now - RECEIPT_TTL_MS - 1) as u64,
            PLAN_RECEIPT_VERSION,
        );
        assert_eq!(
            receipt_authorizes(&old, &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // Future timestamp (clock skew / tamper) → no resume.
        let future = receipt(&ph, head, (now + 10_000) as u64, PLAN_RECEIPT_VERSION);
        assert_eq!(
            receipt_authorizes(&future, &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // Zero/absent timestamp → no resume.
        let zero = receipt(&ph, head, 0, PLAN_RECEIPT_VERSION);
        assert_eq!(
            receipt_authorizes(&zero, &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // Within TTL (just under) → still authorized.
        let recent = receipt(
            &ph,
            head,
            (now - RECEIPT_TTL_MS + 1) as u64,
            PLAN_RECEIPT_VERSION,
        );
        assert!(receipt_authorizes(&recent, &ph, HeadMatch::Exact, "xhigh", now).is_some());
        // Empty/missing receipt object → no resume (fail-safe).
        assert_eq!(
            receipt_authorizes(&json!({}), &ph, HeadMatch::Exact, "xhigh", now),
            None
        );

        // --- strict risk_grants validation (fail-safe on malformed/forged) ---
        let base = |grants: Value| {
            json!({
                "plan_hash": ph,
                "base_head": head,
                "plan_receipt_version": PLAN_RECEIPT_VERSION,
                "created_ms": now as u64,
                "risk_grants": grants,
            })
        };
        // Missing risk_grants entirely (truncated/old-shape receipt) → no resume.
        let no_field = json!({
            "plan_hash": ph, "base_head": head,
            "plan_receipt_version": PLAN_RECEIPT_VERSION, "created_ms": now as u64,
            "command_classes": ["remote-publish"],
        });
        assert_eq!(
            receipt_authorizes(&no_field, &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // Not an array → no resume.
        assert_eq!(
            receipt_authorizes(
                &base(json!("remote-publish")),
                &ph,
                HeadMatch::Exact,
                "xhigh",
                now
            ),
            None
        );
        // Unknown class → no resume (never restore an unreviewed class).
        assert_eq!(
            receipt_authorizes(
                &base(json!([{ "class": "launch-missiles", "shape": "standard" }])),
                &ph,
                HeadMatch::Exact,
                "xhigh",
                now
            ),
            None
        );
        // Unknown shape → no resume (never restore an unrecognized shape).
        assert_eq!(
            receipt_authorizes(
                &base(json!([{ "class": "remote-publish", "shape": "bogus" }])),
                &ph,
                HeadMatch::Exact,
                "xhigh",
                now
            ),
            None
        );
        // Non-object element → no resume.
        assert_eq!(
            receipt_authorizes(&base(json!([123])), &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // Empty array is VALID (a plan that needs no high-risk command) → authorized at
        // BOTH Exact and Ancestor (the empty-grants case is the lenient-resume path).
        assert_eq!(
            receipt_authorizes(&base(json!([])), &ph, HeadMatch::Exact, "xhigh", now),
            Some(vec![])
        );
        assert_eq!(
            receipt_authorizes(&base(json!([])), &ph, HeadMatch::Ancestor, "xhigh", now),
            Some(vec![])
        );
        // Duplicate known grants are deduped (Exact).
        assert_eq!(
            receipt_authorizes(
                &base(json!([
                    { "class": "db-migration", "shape": "standard" },
                    { "class": "db-migration", "shape": "standard" }
                ])),
                &ph,
                HeadMatch::Exact,
                "xhigh",
                now
            ),
            Some(vec![grant("db-migration", "standard")])
        );
        // A WIDENED grant round-trips through the receipt (Exact).
        assert_eq!(
            receipt_authorizes(
                &base(json!([{ "class": "remote-publish", "shape": "widened" }])),
                &ph,
                HeadMatch::Exact,
                "xhigh",
                now
            ),
            Some(vec![grant("remote-publish", "widened")])
        );
    }

    #[test]
    fn plan_receipt_version_is_four() {
        assert_eq!(PLAN_RECEIPT_VERSION, 4);
    }

    #[test]
    fn receipt_effort_binding() {
        let now: u128 = 1_000_000_000_000;
        let ph = plan_hash("p");
        // Build a receipt with empty grants (so the effort gate is the only variable) and
        // an optional plan_review_effort (json!(null) ⇒ field ABSENT = pre-#4 legacy).
        let mk = |effort: Value| {
            let mut r = receipt(&ph, "abc123", now as u64, PLAN_RECEIPT_VERSION);
            r["risk_grants"] = json!([]);
            if !effort.is_null() {
                r["plan_review_effort"] = effort;
            }
            r
        };
        // Minted "high", want "xhigh" → mismatch → no resume (a deeper review is wanted).
        assert_eq!(
            receipt_authorizes(&mk(json!("high")), &ph, HeadMatch::Exact, "xhigh", now),
            None
        );
        // Minted "xhigh", want "high" → mismatch → no resume.
        assert_eq!(
            receipt_authorizes(&mk(json!("xhigh")), &ph, HeadMatch::Exact, "high", now),
            None
        );
        // Minted "high", want "high" → match → resume.
        assert_eq!(
            receipt_authorizes(&mk(json!("high")), &ph, HeadMatch::Exact, "high", now),
            Some(vec![])
        );
        // ABSENT field (pre-#4 receipt) → legacy xhigh: resumes ONLY when want == xhigh.
        assert_eq!(
            receipt_authorizes(&mk(json!(null)), &ph, HeadMatch::Exact, "xhigh", now),
            Some(vec![])
        );
        assert_eq!(
            receipt_authorizes(&mk(json!(null)), &ph, HeadMatch::Exact, "high", now),
            None
        );
        // MALFORMED present value (non-string) → fail closed regardless of want.
        for bad in [json!(7), json!(["x"]), json!({"a": 1}), json!(null)] {
            let mut r = receipt(&ph, "abc123", now as u64, PLAN_RECEIPT_VERSION);
            r["risk_grants"] = json!([]);
            r["plan_review_effort"] = bad;
            assert_eq!(
                receipt_authorizes(&r, &ph, HeadMatch::Exact, "xhigh", now),
                None,
                "malformed plan_review_effort must fail closed"
            );
        }
    }

    /// Minimal real git repo in a fresh temp dir (no receipt/home writes).
    fn make_repo() -> String {
        let p = std::env::temp_dir().join(format!(
            "aibridge-receipt-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&p).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&p)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "t@t.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(p.join("seed.txt"), "seed").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        p.display().to_string()
    }
    fn commit(repo: &str, name: &str) -> String {
        std::fs::write(std::path::Path::new(repo).join(name), name).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run(&["add", "."]);
        run(&["commit", "-m", name]);
        crate::git::head_oid(repo).expect("head")
    }

    #[test]
    fn head_match_classifies_and_fails_closed() {
        let repo = make_repo();
        let c0 = crate::git::head_oid(&repo).unwrap();
        // exact
        assert_eq!(head_match(&repo, &c0, &c0), HeadMatch::Exact);
        // ancestor: after a new commit, C0 is an ancestor of C1
        let c1 = commit(&repo, "a.txt");
        assert_eq!(head_match(&repo, &c0, &c1), HeadMatch::Ancestor);
        // a UNIQUE ABBREVIATED prefix of C0 (len != full) → No (abbrev-OID regression)
        assert_eq!(head_match(&repo, &c0[..8], &c1), HeadMatch::No);
        // non-hex base_head of the right length → No
        let nonhex: String = "z".repeat(c1.len());
        assert_eq!(head_match(&repo, &nonhex, &c1), HeadMatch::No);
        // a full-length but GONE oid → No (object_exists false)
        let gone = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        assert_eq!(head_match(&repo, gone, &c1), HeadMatch::No);
    }
}
