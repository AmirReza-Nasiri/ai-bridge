//! Pre-execution plan gate (DEFAULT-ON; `init --no-plan-gate` to skip): before any file change in a task, Codex
//! reviews the task's todolist/approach in a continuous dialogue until APPROVE —
//! the planning-phase mirror of the Stop-gate (which reviews the result after).
//!
//! Enforcement is a `PreToolUse` hook that DENIES the write tools
//! (`Write`/`Edit`/`MultiEdit`/`NotebookEdit`) and `Bash` until the CURRENT task
//! epoch is approved. Bash is default-denied before approval because it can write
//! through countless paths (`python -c`, `sed -i`, `git apply`, redirection) that
//! can't be detected cheaply; read-only discovery uses Read/Grep/Glob (not gated).
//!
//! Three processes coordinate via on-disk state under `.ai-bridge/plan-gate/`:
//! - the `UserPromptSubmit` hook starts a fresh PENDING epoch each task,
//! - the `PreToolUse` hook reads the epoch to allow/deny writes,
//! - the warm MCP server's `plan_gate` tool runs the Codex dialogue and, on
//!   APPROVE, marks the epoch approved.
//!
//! Activation is two-step so installing/updating never deadlocks the current
//! session: `aibridge init` STAGES an `enabled.pending` marker, and the MCP server
//! PROMOTES it to `enabled` on its next startup — i.e. the gate only enforces once
//! a server that actually provides the `plan_gate` tool is running (after the
//! Claude Code restart `init` asks for). Merely shipping the binary gates no one.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Write tools denied until the plan is approved. `Bash` is handled separately
/// (default-deny too) so its approved path can still flow through rtk.
pub const GATED_WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Stop telling Claude to revise after this many materially-identical rejected
/// rounds; instead stop and ask the user (never auto-approve). Mirrors the
/// Stop-gate's no-progress bound.
pub const NO_PROGRESS_THRESHOLD: u32 = 2;

/// Resolve the project root from any starting dir by walking up to the first
/// ancestor that holds `.ai-bridge` (where install put state) or `.git`. This
/// makes the SEPARATE callers agree on ONE state location even when their cwd
/// differs: the hooks get cwd from the payload (possibly a subdir), while the
/// `plan_gate` MCP tool uses the warm server's `current_dir`.
fn root(cwd: &str) -> PathBuf {
    let start = Path::new(cwd);
    // Pass 1 — an ACTIVE `enabled` marker wins globally: a child's stale `.pending`
    // must never shadow an active ancestor (which would fail the gate open).
    let mut p = start;
    loop {
        if p.join(".ai-bridge")
            .join("plan-gate")
            .join("enabled")
            .exists()
        {
            return p.to_path_buf();
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    // Pass 2 — else a STAGED `.pending` marker, so promotion/state lookups from a
    // child (or past a nested `.git`) still find a parent's staged gate.
    let mut p = start;
    loop {
        if p.join(".ai-bridge")
            .join("plan-gate")
            .join("enabled.pending")
            .exists()
        {
            return p.to_path_buf();
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    // Pass 3 — fallback (gate off): first ancestor with `.ai-bridge` or `.git`, so
    // state co-locates with the rest of the install.
    let mut p = start;
    loop {
        if p.join(".ai-bridge").exists() || p.join(".git").exists() {
            return p.to_path_buf();
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => return start.to_path_buf(),
        }
    }
}

fn dir(cwd: &str) -> PathBuf {
    root(cwd).join(".ai-bridge").join("plan-gate")
}
fn state_path(cwd: &str) -> PathBuf {
    dir(cwd).join("state.json")
}
fn marker_path(cwd: &str) -> PathBuf {
    dir(cwd).join("enabled")
}

/// The gate enforces only when `aibridge init --plan-gate` wrote the marker.
pub fn is_enabled(cwd: &str) -> bool {
    marker_path(cwd).exists()
}

/// Turn enforcement on immediately (idempotent). Direct activation — used by tests
/// and any "activate now" path. (`init` uses [`enable_pending`] instead.)
pub fn enable(cwd: &str) -> std::io::Result<()> {
    let d = Path::new(cwd).join(".ai-bridge").join("plan-gate");
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("enabled"), b"1\n")
}

/// STAGE the gate without activating it yet (called by `init`). Writes
/// `enabled.pending`; the gate stays INACTIVE (`is_enabled` checks `enabled`) until
/// the MCP server promotes it on startup. This is the fix for the install deadlock:
/// the session that ran `init` is never hard-blocked before the `plan_gate` tool is
/// reachable (the tool connects only on the next Claude Code start, which is exactly
/// when promotion happens). Never downgrades an already-active gate.
pub fn enable_pending(cwd: &str) -> std::io::Result<()> {
    // Root-resolved check: if the gate is already active here OR at an ancestor,
    // don't stage an orphan child `.pending` over it.
    if is_enabled(cwd) {
        return Ok(());
    }
    let d = Path::new(cwd).join(".ai-bridge").join("plan-gate");
    std::fs::create_dir_all(&d)?;
    std::fs::write(d.join("enabled.pending"), b"1\n")
}

/// Promote a staged gate (`enabled.pending` → `enabled`). Called by the MCP server
/// on startup: once THIS session's server (which provides `plan_gate`) is up, the
/// gate goes active. Best-effort + idempotent: no pending → no-op; already active →
/// just clear any leftover pending marker.
pub fn promote_pending(cwd: &str) {
    let d = dir(cwd);
    let pending = d.join("enabled.pending");
    let enabled = d.join("enabled");
    if enabled.exists() {
        let _ = std::fs::remove_file(&pending);
        return;
    }
    if pending.exists() {
        // Atomic; if it loses a race with another server, the other outcome
        // (enabled now exists) is equally correct.
        let _ = std::fs::rename(&pending, &enabled);
    }
}

/// Install state of the gate for `doctor`: off, staged (awaiting restart), or active.
pub enum MarkerState {
    Disabled,
    Pending,
    Active,
}

/// Report the gate's marker state (active `enabled` wins over `enabled.pending`).
pub fn marker_state(cwd: &str) -> MarkerState {
    let d = dir(cwd);
    if d.join("enabled").exists() {
        MarkerState::Active
    } else if d.join("enabled.pending").exists() {
        MarkerState::Pending
    } else {
        MarkerState::Disabled
    }
}

fn read_state(cwd: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(state_path(cwd)).ok()?).ok()
}

/// Write state atomically (temp + rename) so a reader in another process never
/// sees a half-written/truncated file (the hooks and the warm server share this
/// file with no other lock).
fn write_state(cwd: &str, v: &Value) -> std::io::Result<()> {
    let d = dir(cwd);
    std::fs::create_dir_all(&d)?;
    let body = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string());
    let tmp = d.join(format!("state.json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, d.join("state.json"))
}

/// Stable non-cryptographic hash, shared shape with the Stop-gate.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// UserPromptSubmit hook entry: parse the payload and, IF the gate is enabled for
/// this project, start a fresh pending epoch. Never blocks the prompt.
pub fn on_user_prompt(stdin: &str) {
    let v: Value = match serde_json::from_str(stdin) {
        Ok(v) => v,
        Err(_) => return,
    };
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or(".");
    if !is_enabled(cwd) {
        return;
    }
    let session = v
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let prompt = v.get("prompt").and_then(Value::as_str).unwrap_or("");
    start_epoch(cwd, session, prompt);
}

/// Begin a fresh PENDING epoch for a new task (called by the UserPromptSubmit
/// hook). The epoch id ties an approval to THIS task so a later prompt re-gates.
pub fn start_epoch(cwd: &str, session: &str, prompt: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let epoch = format!("{session}:{ts}:{:x}", hash_str(prompt));
    let state = json!({
        "epoch": epoch,
        "approved": false,
        "approved_plan_hash": Value::Null,
        "rounds": 0,
        "last_findings_hash": Value::Null,
        "same_findings": 0,
    });
    if write_state(cwd, &state).is_err() {
        // Fail closed: if we can't write the fresh PENDING epoch, delete any prior
        // (possibly APPROVED) state so the gate denies until a plan is re-approved,
        // rather than letting a stale approval unlock this new task.
        let _ = std::fs::remove_file(state_path(cwd));
    }
}

/// The current epoch id (or a synthesized "manual" one if no prompt started a
/// task yet — keeps the `plan_gate` tool usable even with enforcement off).
pub fn current_epoch(cwd: &str) -> String {
    read_state(cwd)
        .and_then(|s| s.get("epoch").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "manual".to_string())
}

/// True when the CURRENT epoch is approved. Approval is bound to the epoch id, so
/// a stale/partially-written `approved` flag from another task can never carry
/// over (defense-in-depth on top of atomic writes + per-prompt epoch reset).
pub fn is_approved(cwd: &str) -> bool {
    match read_state(cwd) {
        Some(s) => {
            let approved = s.get("approved").and_then(Value::as_bool).unwrap_or(false);
            let epoch = s.get("epoch").and_then(Value::as_str);
            let approved_epoch = s.get("approved_epoch").and_then(Value::as_str);
            approved && epoch.is_some() && epoch == approved_epoch
        }
        None => false,
    }
}

/// Quick per-session escape hatch (mirrors rtk's): `AIBRIDGE_PLAN_GATE=0` or
/// `PLAN_GATE_DISABLE=1` turns OFF enforcement without un-installing — for a
/// trivial task where a full plan round isn't worth the wait. Set it before
/// launching Claude Code (hooks inherit that environment).
fn bypassed() -> bool {
    std::env::var("AIBRIDGE_PLAN_GATE")
        .map(|v| v == "0")
        .unwrap_or(false)
        || std::env::var("PLAN_GATE_DISABLE")
            .map(|v| v == "1")
            .unwrap_or(false)
}

/// The gate is currently HOLDING writes (enabled, not bypassed, current task not
/// yet approved). Used by both the PreToolUse hook AND the in-process `run` tool,
/// so `run` (which executes arbitrary shell) can't bypass the hook.
pub fn blocks_writes(cwd: &str) -> bool {
    is_enabled(cwd) && !bypassed() && !is_approved(cwd)
}

/// Is this tool one the gate must hold until approval? Includes `mcp__aibridge__run`
/// (arbitrary shell) — a write path the file-tool matcher would otherwise miss.
pub fn is_gated_tool(tool_name: &str) -> bool {
    tool_name == "Bash"
        || tool_name == "mcp__aibridge__run"
        || GATED_WRITE_TOOLS.contains(&tool_name)
}

/// PreToolUse enforcement: `Some(deny_json)` to block a write before approval,
/// `None` to let the caller proceed (incl. its own rtk handling for Bash).
pub fn enforce(cwd: &str, tool_name: &str) -> Option<String> {
    if !is_gated_tool(tool_name) || !blocks_writes(cwd) {
        return None;
    }
    // Count repeated denied writes so the operator can see a wrong-loop (Claude
    // retrying the edit instead of calling plan_gate).
    if let Some(mut s) = read_state(cwd) {
        let n = s.get("denied_writes").and_then(Value::as_u64).unwrap_or(0) + 1;
        if let Some(o) = s.as_object_mut() {
            o.insert("denied_writes".into(), json!(n));
        }
        let _ = write_state(cwd, &s);
    }
    Some(deny_json())
}

/// The operational deny — tells Claude exactly what to do (call the tool, do NOT
/// retry the blocked edit), so it advances the dialogue instead of looping.
fn deny_json() -> String {
    let reason = "PLAN_GATE_REQUIRED: this task has no Codex-approved plan yet. \
        Do NOT retry this tool. First gather context with Read/Grep/Glob, form a todolist, \
        then call the MCP tool `mcp__aibridge__plan_gate` with a structured plan \
        (todos, approach, intended_files, risk_surfaces, test_plan). Revise and call it \
        again until it returns <AI-BRIDGE-APPROVE/>; only then will writes/Bash be allowed. \
        If `mcp__aibridge__plan_gate` is NOT available, AI Bridge was just installed/updated — \
        the tool connects only after a Claude Code restart: restart Claude Code, or relaunch it \
        with AIBRIDGE_PLAN_GATE=0 set to bypass the gate for this session.";
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    })
    .to_string()
}

/// Outcome of a `plan_gate` round, after parsing Codex's verdict.
pub enum Outcome {
    /// Approved — writes are now unlocked for this epoch.
    Approved,
    /// Needs revision — `findings` returned to Claude to revise and re-submit.
    Revise(String),
    /// No progress (same findings repeated) — stop and ask the user.
    Stuck(String),
    /// Codex couldn't decide (blocked / unparseable verdict).
    NeedsInfo(String),
}

/// Apply a parsed verdict to the epoch state and decide what Claude should do.
/// `expected_epoch` is the epoch captured at the START of the review round; if the
/// on-disk epoch has since changed (a new task began while Codex was reviewing),
/// approval is refused so an old plan's verdict can't unlock a new task. Kept
/// separate from the Codex call so it is unit-testable.
pub fn record(
    cwd: &str,
    expected_epoch: &str,
    plan: &str,
    verdict: &crate::gate::Verdict,
    findings: &str,
) -> Outcome {
    // Fail-safe: when the gate is enabled, NEVER approve from missing/unparseable
    // state (a corrupted/half-written file must not become an approved epoch). Only
    // synthesize a state when the gate is OFF (manual `plan_gate` use, harmless).
    let mut s = match read_state(cwd) {
        Some(s) => s,
        None if !is_enabled(cwd) => {
            json!({ "epoch": current_epoch(cwd), "approved": false, "rounds": 0, "same_findings": 0 })
        }
        None => {
            return Outcome::NeedsInfo(
                "AI Bridge: no active task epoch found (plan-gate state is missing or unreadable). \
                 Cannot approve a plan right now — restart Claude / re-send the task, or ask the user."
                    .to_string(),
            )
        }
    };
    let epoch = s
        .get("epoch")
        .and_then(Value::as_str)
        .unwrap_or("manual")
        .to_string();
    // TOCTOU guard: the task changed under us (a new prompt started a new epoch
    // mid-review). Refuse to approve the stale plan against the new task.
    if epoch != expected_epoch {
        return Outcome::NeedsInfo(
            "AI Bridge: the task changed while this plan was under review — submit a fresh \
             plan_gate for the CURRENT task before proceeding."
                .to_string(),
        );
    }
    let rounds = s.get("rounds").and_then(Value::as_u64).unwrap_or(0) + 1;
    let set = |s: &mut Value, k: &str, v: Value| {
        if let Some(o) = s.as_object_mut() {
            o.insert(k.into(), v);
        }
    };
    set(&mut s, "rounds", json!(rounds));

    match verdict {
        crate::gate::Verdict::Approve => {
            set(&mut s, "approved", json!(true));
            // Bind approval to THIS epoch so is_approved() can reject a stale flag.
            set(&mut s, "approved_epoch", json!(epoch));
            set(&mut s, "approved_plan_hash", json!(hash_str(plan)));
            set(&mut s, "same_findings", json!(0));
            let _ = write_state(cwd, &s);
            Outcome::Approved
        }
        crate::gate::Verdict::RequestChanges => {
            let fh = hash_str(findings);
            let prev = s.get("last_findings_hash").and_then(Value::as_u64);
            let same = if prev == Some(fh) {
                s.get("same_findings").and_then(Value::as_u64).unwrap_or(0) + 1
            } else {
                1
            };
            set(&mut s, "last_findings_hash", json!(fh));
            set(&mut s, "same_findings", json!(same));
            let _ = write_state(cwd, &s);
            if same >= NO_PROGRESS_THRESHOLD as u64 {
                Outcome::Stuck(findings.to_string())
            } else {
                Outcome::Revise(findings.to_string())
            }
        }
        crate::gate::Verdict::Blocked | crate::gate::Verdict::Unparseable => {
            let _ = write_state(cwd, &s);
            Outcome::NeedsInfo(findings.to_string())
        }
    }
}

/// The Codex prompt for a plan review round: judge the plan, end with one verdict
/// tag (reusing the Stop-gate sentinels so there is ONE verdict parser).
pub fn prompt(plan: &str) -> String {
    format!(
        "You are AI Bridge's PRE-EXECUTION plan gate. The agent is about to start coding and \
         has submitted the plan/todolist below. Judge the APPROACH before any code is written: \
         is the plan correct, complete, and safe? Look for wrong approach, missing steps, \
         unhandled edge cases, risky surfaces (auth/payments/migrations/data-loss/deploy), \
         scope creep, and missing tests.\n\n\
         Write:\n\
         1. FINDINGS: if the plan is sound, write \"No blocking concerns.\"; otherwise list \
         concise, actionable changes the agent should make to the plan.\n\
         2. A final line that is EXACTLY one of:\n\
         <AI-BRIDGE-APPROVE/>       (plan is good to execute)\n\
         <AI-BRIDGE-REQUEST-CHANGES/> (revise the plan as noted)\n\
         <AI-BRIDGE-BLOCKED/>       (need more info to judge — ask in FINDINGS)\n\n\
         Approve only when the plan is genuinely ready to execute.\n\n\
         === PROPOSED PLAN ===\n{plan}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    fn tmp() -> String {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("aibridge-plangate-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p.display().to_string()
    }

    #[test]
    fn disabled_by_default_never_denies() {
        let cwd = tmp();
        // No marker → not enabled → no enforcement even for a write tool.
        assert!(!is_enabled(&cwd));
        assert!(enforce(&cwd, "Write").is_none());
        assert!(enforce(&cwd, "Bash").is_none());
    }

    #[test]
    fn enabled_denies_writes_until_approved() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "add a jalali date helper");
        // Pending → write tools and Bash are denied; read-only tools are not gated.
        assert!(enforce(&cwd, "Write").is_some());
        assert!(enforce(&cwd, "Edit").is_some());
        assert!(enforce(&cwd, "Bash").is_some());
        assert!(enforce(&cwd, "Read").is_none());
        assert!(enforce(&cwd, "Grep").is_none());
        // Approve the plan → writes unlock.
        assert!(matches!(
            record(
                &cwd,
                &current_epoch(&cwd),
                "the plan",
                &crate::gate::Verdict::Approve,
                ""
            ),
            Outcome::Approved
        ));
        assert!(is_approved(&cwd));
        assert!(enforce(&cwd, "Write").is_none());
        assert!(enforce(&cwd, "Bash").is_none());
    }

    #[test]
    fn new_epoch_re_gates() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task one");
        record(
            &cwd,
            &current_epoch(&cwd),
            "plan one",
            &crate::gate::Verdict::Approve,
            "",
        );
        assert!(enforce(&cwd, "Write").is_none()); // approved
                                                   // A new task (new prompt) must re-gate.
        start_epoch(&cwd, "sess", "task two");
        assert!(!is_approved(&cwd));
        assert!(enforce(&cwd, "Write").is_some());
    }

    #[test]
    fn repeated_identical_findings_become_stuck() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let v = crate::gate::Verdict::RequestChanges;
        assert!(matches!(
            record(&cwd, &current_epoch(&cwd), "p1", &v, "same finding"),
            Outcome::Revise(_)
        ));
        assert!(matches!(
            record(&cwd, &current_epoch(&cwd), "p2", &v, "same finding"),
            Outcome::Stuck(_)
        ));
    }

    #[test]
    fn pending_gate_is_inactive_until_promoted() {
        let cwd = tmp();
        // init stages it → NOT active yet (no deadlock: writes flow until promote).
        enable_pending(&cwd).unwrap();
        assert!(!is_enabled(&cwd), "staged gate must not enforce yet");
        assert!(matches!(marker_state(&cwd), MarkerState::Pending));
        assert!(enforce(&cwd, "Write").is_none());
        // server startup promotes it → now active.
        promote_pending(&cwd);
        assert!(is_enabled(&cwd), "promoted gate must enforce");
        assert!(matches!(marker_state(&cwd), MarkerState::Active));
        start_epoch(&cwd, "s", "t");
        assert!(enforce(&cwd, "Write").is_some());
    }

    #[test]
    fn enable_pending_never_downgrades_active_gate() {
        let cwd = tmp();
        enable(&cwd).unwrap(); // already active
        enable_pending(&cwd).unwrap(); // init re-run must not stage-over an active gate
        assert!(is_enabled(&cwd));
        assert!(matches!(marker_state(&cwd), MarkerState::Active));
    }

    #[test]
    fn pending_promotes_from_subdir_even_past_nested_git() {
        let root = tmp();
        enable_pending(&root).unwrap(); // staged at the project root
        let sub = format!("{root}/pkg");
        std::fs::create_dir_all(format!("{sub}/.git")).unwrap(); // nested git, must NOT shadow
                                                                 // From the nested subdir, state + promote resolve to the ROOT's staged marker.
        assert!(matches!(marker_state(&sub), MarkerState::Pending));
        promote_pending(&sub);
        assert!(
            is_enabled(&root),
            "root gate active after promote-from-subdir"
        );
        assert!(matches!(marker_state(&sub), MarkerState::Active));
    }

    #[test]
    fn active_parent_not_shadowed_by_stale_child_pending() {
        let root = tmp();
        enable(&root).unwrap(); // active at root
        let sub = format!("{root}/pkg");
        // a stale orphan child `.pending` (could exist from a pre-fix v0.4.0 install)
        let pg = Path::new(&sub).join(".ai-bridge").join("plan-gate");
        std::fs::create_dir_all(&pg).unwrap();
        std::fs::write(pg.join("enabled.pending"), b"1\n").unwrap();
        // The ACTIVE parent must win from the child (not fail open).
        assert!(
            is_enabled(&sub),
            "active parent must not be shadowed by child pending"
        );
        assert!(matches!(marker_state(&sub), MarkerState::Active));
        start_epoch(&sub, "s", "t");
        assert!(enforce(&sub, "Write").is_some());
    }

    #[test]
    fn enable_pending_skips_when_ancestor_active() {
        let root = tmp();
        enable(&root).unwrap(); // active at root
        let sub = format!("{root}/pkg");
        std::fs::create_dir_all(&sub).unwrap();
        enable_pending(&sub).unwrap(); // must NOT create an orphan child .pending
        assert!(!Path::new(&sub)
            .join(".ai-bridge")
            .join("plan-gate")
            .join("enabled.pending")
            .exists());
    }

    #[test]
    fn run_tool_is_gated_so_it_cannot_bypass() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        // `mcp__aibridge__run` runs arbitrary shell → must be held like Bash.
        assert!(is_gated_tool("mcp__aibridge__run"));
        assert!(enforce(&cwd, "mcp__aibridge__run").is_some());
        assert!(blocks_writes(&cwd));
        record(
            &cwd,
            &current_epoch(&cwd),
            "plan",
            &crate::gate::Verdict::Approve,
            "",
        );
        assert!(!blocks_writes(&cwd));
        assert!(enforce(&cwd, "mcp__aibridge__run").is_none());
    }

    #[test]
    fn corrupted_state_never_auto_approves() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        // Simulate a truncated/garbled state file.
        std::fs::write(state_path(&cwd), b"{ this is not json").unwrap();
        // Enabled + unreadable state → record refuses to approve (fail-closed).
        assert!(matches!(
            record(
                &cwd,
                &current_epoch(&cwd),
                "plan",
                &crate::gate::Verdict::Approve,
                ""
            ),
            Outcome::NeedsInfo(_)
        ));
        assert!(!is_approved(&cwd));
        assert!(blocks_writes(&cwd));
    }

    #[test]
    fn approval_is_bound_to_its_epoch() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        record(
            &cwd,
            &current_epoch(&cwd),
            "plan",
            &crate::gate::Verdict::Approve,
            "",
        );
        assert!(is_approved(&cwd));
        // Tamper: flip the epoch but leave approved=true → must NOT count as approved.
        let mut s = read_state(&cwd).unwrap();
        s.as_object_mut()
            .unwrap()
            .insert("epoch".into(), json!("some-other-epoch"));
        write_state(&cwd, &s).unwrap();
        assert!(!is_approved(&cwd));
    }

    #[test]
    fn approval_refused_if_task_changed_mid_review() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task one");
        let stale = current_epoch(&cwd); // captured at the start of the review
                                         // A new task begins while the review is in flight.
        start_epoch(&cwd, "sess", "task two");
        // Approving the OLD plan against the stale epoch must not unlock task two.
        assert!(matches!(
            record(&cwd, &stale, "old plan", &crate::gate::Verdict::Approve, ""),
            Outcome::NeedsInfo(_)
        ));
        assert!(!is_approved(&cwd));
        assert!(blocks_writes(&cwd));
    }
}
