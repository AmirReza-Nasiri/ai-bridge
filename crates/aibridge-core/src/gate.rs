//! The Stop-gate review strategy (`single_critic_gate`): turn a Codex review of
//! the current diff into an allow/block decision, with no-progress + fail-ask
//! state so the loop never caps artificially, never loops forever, and never
//! silently ships unresolved findings.
//!
//! AI Bridge owns all hook-decision JSON; Codex only emits a sentinel verdict
//! tag, so malformed reviewer output can never leak into hook control flow.

use std::hash::{Hash, Hasher};

/// Per (workspace + session) loop state, held in the warm server.
#[derive(Default)]
pub struct GateState {
    pub last_allowed_diff_hash: Option<u64>,
    pub last_blocked_diff_hash: Option<u64>,
    pub last_findings_hash: Option<u64>,
    pub same_findings_blocks: u32,
    pub fail_ask_pending: bool,
    pub cached_block_reason: Option<String>,
}

/// Reviewer verdict parsed from the final sentinel line.
pub enum Verdict {
    Approve,
    RequestChanges,
    Blocked,
    Unparseable,
}

/// Fail-ask / no-progress threshold (consecutive blocks before we stop and ask).
pub const NO_PROGRESS_THRESHOLD: u32 = 2;

/// The review prompt: review the CURRENT diff only, end with exactly one tag.
pub fn prompt(diff_bundle: &str) -> String {
    prompt_with_scope(diff_bundle, None)
}

/// Like [`prompt`], but when a pre-approved plan exists for the task, also asks the
/// reviewer to flag changes outside the approved scope or unplanned high-risk
/// actions. This is the soft-telemetry half of plan-gate v2 — AI Bridge never
/// hard-fences files; the Stop gate compares the approved plan against the real
/// diff instead.
pub fn prompt_with_scope(diff_bundle: &str, approved_plan: Option<&str>) -> String {
    let scope = match approved_plan {
        Some(p) if !p.trim().is_empty() => format!(
            "\nThis task had a PRE-APPROVED plan/scope; treat it as the INTENDED scope — a \
             reference, NOT a brittle whitelist (necessary implementation detail that serves the \
             plan is fine). Flag material deviations: a change clearly OUTSIDE this scope, planned \
             work that is missing, or any high-risk action (publish/deploy/migrations/destructive \
             shell/data loss) the plan did not mention:\n\
             === APPROVED PLAN ===\n{p}\n=== END APPROVED PLAN ===\n"
        ),
        _ => String::new(),
    };
    format!(
        "You are AI Bridge's Stop-gate peer reviewer.\n\
         Review the FULL change set for THIS task shown below. It may include work COMMITTED \
         since the task started AND the current uncommitted working tree — treat ALL sections as \
         ONE combined task diff and review every section, including any labeled 'committed diff \
         since task start' (do NOT skip a change just because it is already committed). If the \
         same issue spans the committed and uncommitted sections, report it ONCE against the \
         final state. Any earlier turns in this conversation reviewed DIFFERENT, now-superseded \
         diffs — do NOT carry their findings or assumptions into this review; judge strictly the \
         changes shown here.\n\n\
         Write:\n\
         1. FINDINGS: if no blocking issues, write \"No blocking findings.\"; otherwise list ALL \
         material blocking findings you can verify from this diff in THIS single pass — be \
         complete, do NOT defer a known blocker to a later round. Give path/line where possible. \
         Do NOT pad with speculative issues or non-blocking nitpicks; list those separately (if \
         at all) and do not let them drive the verdict.\n\
         2. A final line that is EXACTLY one of:\n\
         <AI-BRIDGE-APPROVE/>\n\
         <AI-BRIDGE-REQUEST-CHANGES/>\n\
         <AI-BRIDGE-BLOCKED/>\n\n\
         Be strict. Use REQUEST-CHANGES for ANY introduced or task-relevant correctness/safety \
         bug — crashes, undefined names, broken tests, missed requirements, regressions, or \
         unhandled edge cases (empty input, division by zero, null/None, out-of-bounds). A \
         PRE-EXISTING issue should block only if this task worsens it, relies on it, or the \
         approved plan claimed to fix it. Use APPROVE only when the diff is genuinely safe to \
         ship as-is. Use BLOCKED only when required context is missing.\n\
         {scope}\n\
         === TASK CHANGES (committed since task start + uncommitted) ===\n{diff_bundle}"
    )
}

/// Parse the last non-empty line as the verdict tag.
pub fn parse_verdict(review: &str) -> Verdict {
    let last = review
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    match last {
        "<AI-BRIDGE-APPROVE/>" => Verdict::Approve,
        "<AI-BRIDGE-REQUEST-CHANGES/>" => Verdict::RequestChanges,
        "<AI-BRIDGE-BLOCKED/>" => Verdict::Blocked,
        _ => Verdict::Unparseable,
    }
}

/// The review text minus the trailing verdict tag (the findings body).
pub fn findings(review: &str) -> String {
    let mut lines: Vec<&str> = review.lines().collect();
    while let Some(last) = lines.last() {
        if last.trim().is_empty() {
            lines.pop();
        } else {
            break;
        }
    }
    if lines
        .last()
        .map(|l| l.trim().starts_with("<AI-BRIDGE-"))
        .unwrap_or(false)
    {
        lines.pop();
    }
    lines.join("\n").trim().to_string()
}

/// Build the compact `reason` placed in the hook block decision.
pub fn compact_reason(findings: &str, trace: &str) -> String {
    let body: String = findings.chars().take(900).collect();
    let truncated = if findings.chars().count() > 900 {
        "\n…(truncated; full review in trace)"
    } else {
        ""
    };
    format!(
        "AI Bridge peer review found issues. Address these, then try to finish again:\n\n{body}{truncated}\n\nFull review: {trace}"
    )
}

/// Stable hash for change detection (not cryptographic).
pub fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Write the raw review + diff to a per-review trace dir; return a display path.
pub fn write_trace(cwd: &str, diff_bundle: &str, review: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::path::Path::new(cwd)
        .join(".ai-bridge")
        .join("reviews")
        .join(ts.to_string());
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("review.txt"), review);
        let _ = std::fs::write(dir.join("diff.txt"), diff_bundle);
    }
    format!(".ai-bridge/reviews/{ts}/review.txt")
}
