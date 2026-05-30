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

// The in-flight review marker lives in its OWN atomic file (`pending`), NOT in
// `state.json`. `begin_review` (warm server) and `start_epoch` (UserPromptSubmit
// hook) run in different processes with no lock; keeping the review nonce out of
// the authority state means a `begin_review` write can never read-modify-write its
// way over a concurrent `start_epoch` reset (Codex review). Each entry is tagged
// with the epoch, so a stale marker from a previous epoch is simply ignored.
fn pending_path(cwd: &str) -> PathBuf {
    dir(cwd).join("pending")
}

/// Record THIS review's submission (epoch + plan hash) atomically in the separate
/// `pending` file. One write, no authority-state touch.
fn write_pending(cwd: &str, epoch: &str, plan_hash: u64) {
    let d = dir(cwd);
    if std::fs::create_dir_all(&d).is_err() {
        return;
    }
    let body = json!({ "epoch": epoch, "plan_hash": plan_hash }).to_string();
    let tmp = d.join(format!("pending.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, pending_path(cwd));
    }
}

/// The in-flight review's (epoch, plan_hash), if any.
fn read_pending(cwd: &str) -> Option<(String, u64)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(pending_path(cwd)).ok()?).ok()?;
    let epoch = v.get("epoch").and_then(Value::as_str)?.to_string();
    let hash = v.get("plan_hash").and_then(Value::as_u64)?;
    Some((epoch, hash))
}

/// Drop the in-flight review marker (new epoch / clean slate). Best-effort.
fn clear_pending(cwd: &str) {
    let _ = std::fs::remove_file(pending_path(cwd));
}

/// Insert/overwrite a top-level field in a JSON object value (no-op if `v` is not
/// an object). Shared by `record`/`revoke` so they all mutate state the same way.
fn set_field(v: &mut Value, k: &str, val: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(k.into(), val);
    }
}

/// Cap the stored approved-plan text so `state.json` stays small (the Stop gate
/// only needs the gist for a scope-vs-diff comparison).
fn cap_plan(plan: &str) -> String {
    const MAX: usize = 4000;
    if plan.chars().count() <= MAX {
        plan.to_string()
    } else {
        let head: String = plan.chars().take(MAX).collect();
        format!("{head}\n…(plan truncated)")
    }
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

// ---------------------------------------------------------------------------
// v0.31 P1: state-based invalidation (behind `planGate.resetOnUserTurn`, default
// TRUE = today's per-turn reset). When the flag is FALSE, a mid-task prompt no
// longer always re-arms the gate: a TRIVIAL continuation preserves the approval,
// while anything else (or an explicit cancel/reset) re-gates. The classification
// is CHEAP + PURE (no LLM/IO) and runs in the short-lived UserPromptSubmit hook;
// the PreToolUse hook makes the AUTHORITATIVE decision via [`reconcile_pending_user_turn`]
// before any mutator. The default direction is INVALIDATE-unless-trivially-safe
// (fail closed). High-risk commands STILL re-gate via the P3 risk-delta check even
// under a preserved approval, so the relaxation only ever preserves ordinary writes.
// ---------------------------------------------------------------------------

/// How a mid-task user prompt relates to an in-flight approved plan (cheap, pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnClass {
    /// Explicit cancel/pause/stop/reset → invalidate (fresh epoch).
    ExplicitReset,
    /// A pure affirmation/continuation from a tiny exact allowlist → may preserve.
    TrivialContinue,
    /// Anything else → unknown scope delta → invalidate (fail closed).
    UnknownDelta,
}

impl TurnClass {
    fn as_marker(self) -> &'static str {
        match self {
            TurnClass::ExplicitReset => "explicit_reset",
            TurnClass::TrivialContinue => "trivial_continue",
            TurnClass::UnknownDelta => "unknown_delta",
        }
    }
}

/// Cheap, pure classification of a mid-task prompt — NO LLM, NO IO (safe for the
/// short-lived UserPromptSubmit hook). Default is [`TurnClass::UnknownDelta`] so any
/// free-form text re-gates; only an explicit reset word or an EXACT trivial-continue
/// token is special-cased. NOTE: because a bare affirmation ("yes") could in principle
/// answer a scope-broadening question, this MVP relies on two backstops that remain
/// active under a preserved approval — the P3 risk-delta re-gate for any high-risk
/// command, and the Stop-hook diff review — and the relaxation ships behind a flag that
/// defaults OFF. Recording the clarification question + a tool-context file-scope check
/// (to also re-gate ordinary out-of-scope writes) is the required follow-up before the
/// default may flip.
fn classify_turn(prompt: &str) -> TurnClass {
    let p = prompt.trim().to_lowercase();
    if p.is_empty() {
        return TurnClass::TrivialContinue; // an empty re-prompt is a no-op continuation
    }
    // Explicit cancel/reset. Broad matching here is SAFE because it only ever invalidates.
    const RESET: &[&str] = &[
        "cancel",
        "abort",
        "reset",
        "stop",
        "pause",
        "start over",
        "never mind",
        "nevermind",
        "forget it",
        "scrap that",
    ];
    if RESET
        .iter()
        .any(|w| p == *w || p.starts_with(&format!("{w} ")) || p.starts_with(&format!("{w},")))
    {
        return TurnClass::ExplicitReset;
    }
    // Tiny EXACT-match trivial-continue allowlist (pure affirmations with no scope content).
    // A trailing run of `!.?,` is stripped so "ok." / "yes!" still match.
    let pp = p.trim_end_matches(|c: char| "!.?,".contains(c)).trim();
    const CONTINUE: &[&str] = &[
        "ok", "okay", "yes", "yeah", "yep", "y", "continue", "proceed", "go ahead", "go on",
    ];
    if CONTINUE.contains(&pp) {
        return TurnClass::TrivialContinue;
    }
    TurnClass::UnknownDelta
}

/// Pure decision for [`start_epoch`]: given the flag + whether a plan is currently
/// approved + the prompt's class, should we PRESERVE the approved epoch (recording a
/// pending user-turn marker for the PreToolUse authority) or start a FRESH epoch?
/// `true` = preserve. Kept pure so the policy is unit-testable without config/IO.
fn preserve_epoch_decision(reset_on_user_turn: bool, currently_approved: bool, class: TurnClass) -> bool {
    if reset_on_user_turn || !currently_approved {
        return false; // default behavior: every prompt re-arms a fresh epoch
    }
    // Flag OFF + an approved plan in flight: preserve unless the user explicitly reset.
    !matches!(class, TurnClass::ExplicitReset)
}

/// Begin a fresh PENDING epoch for a new task (called by the UserPromptSubmit hook),
/// UNLESS `planGate.resetOnUserTurn` is false AND a plan is currently approved AND the
/// prompt is not an explicit reset — in which case the approved epoch is PRESERVED and a
/// pending user-turn marker is recorded for the PreToolUse authority to reconcile before
/// the next mutator. The epoch id ties an approval to THIS task so a later prompt re-gates.
pub fn start_epoch(cwd: &str, session: &str, prompt: &str) {
    let class = classify_turn(prompt);
    if preserve_epoch_decision(
        crate::review_mcp::reset_on_user_turn(),
        is_approved(cwd),
        class,
    ) {
        // Preserve the approval; record the pending user-turn for PreToolUse to reconcile.
        // A failed marker write must FAIL CLOSED — revoke so a preserved-but-unreconciled
        // turn can never leave stale approval live.
        if !set_pending_user_turn(cwd, class) {
            revoke(cwd, "pending_user_turn_write_failed");
        }
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let epoch = format!("{session}:{ts}:{:x}", hash_str(prompt));
    let state = json!({
        "epoch": epoch,
        "approved": false,
        "status": "pending",
        "revoked_reason": "new_epoch",
        "approved_plan_hash": Value::Null,
        "approved_command_classes": [],
        "approved_plan": "",
        "rounds": 0,
        "last_findings_hash": Value::Null,
        "same_findings": 0,
    });
    // A new task starts with no in-flight review (drop any prior epoch's marker).
    clear_pending(cwd);
    if write_state(cwd, &state).is_err() {
        // Fail closed: if we can't write the fresh PENDING epoch, delete any prior
        // (possibly APPROVED) state so the gate denies until a plan is re-approved,
        // rather than letting a stale approval unlock this new task.
        let _ = std::fs::remove_file(state_path(cwd));
    }
}

/// Record a pending user-turn marker on the current (preserved) approved epoch. Returns
/// `false` if state can't be read/written (caller fails closed). Stored INSIDE `state.json`
/// (not the separate review `pending` file) as a nullable object so it travels with the
/// authority state and a corrupt/missing value reads as "present" → fail closed.
fn set_pending_user_turn(cwd: &str, class: TurnClass) -> bool {
    let Some(mut s) = read_state(cwd) else {
        return false;
    };
    set_field(
        &mut s,
        "pending_user_turn",
        json!({ "classification": class.as_marker() }),
    );
    write_state(cwd, &s).is_ok()
}

/// Whether an UNCONSUMED pending user-turn marker is present (any non-null value, incl.
/// malformed → treated as present so [`effectively_approved`] fails closed).
fn has_pending_user_turn(cwd: &str) -> bool {
    read_state(cwd)
        .and_then(|s| s.get("pending_user_turn").cloned())
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// Drop the pending user-turn marker (best-effort; the marker was consumed by reconcile).
fn clear_pending_user_turn(cwd: &str) {
    if let Some(mut s) = read_state(cwd) {
        if let Some(o) = s.as_object_mut() {
            o.remove("pending_user_turn");
        }
        let _ = write_state(cwd, &s);
    }
}

/// PreToolUse authority (v0.31 P1): consume any pending user-turn marker BEFORE a mutator
/// runs. `None` → nothing pending, or a trivial continuation was preserved (approval stands).
/// `Some(deny_json)` → the prompt was a scope delta (or the marker is malformed): the approval
/// is revoked and the write is denied with `user_scope_delta` so the plan is re-reviewed.
/// Idempotent: clears the marker either way so it cannot loop. A no-op when the flag is on
/// (no marker is ever written) — so default behavior is byte-identical.
pub fn reconcile_pending_user_turn(cwd: &str) -> Option<String> {
    let class = match read_state(cwd).and_then(|s| s.get("pending_user_turn").cloned()) {
        None | Some(Value::Null) => return None, // nothing pending
        Some(v) => v
            .get("classification")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    // Trivial continuation → preserve approval (just consume the marker).
    if class.as_deref() == Some(TurnClass::TrivialContinue.as_marker()) {
        clear_pending_user_turn(cwd);
        return None;
    }
    // Anything else (unknown delta, explicit_reset that slipped through, or a malformed/
    // missing classification) → re-gate. Fail closed.
    revoke(cwd, "user_scope_delta");
    clear_pending_user_turn(cwd);
    Some(deny_json(BlockReason::UserScopeDelta))
}

/// The current epoch id (or a synthesized "manual" one if no prompt started a
/// task yet — keeps the `plan_gate` tool usable even with enforcement off).
pub fn current_epoch(cwd: &str) -> String {
    read_state(cwd)
        .and_then(|s| s.get("epoch").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "manual".to_string())
}

/// v0.23.0: Extract the Claude SESSION id from the current epoch. The epoch is
/// formatted as `"{session}:{ts}:{prompt_hash:x}"` by [`start_epoch`]; we split on
/// the first `:` and return the session part. Returns `None` when there is no
/// plan-gate state, no `UserPromptSubmit` ever ran (epoch is the synthesized
/// `"manual"` value), or the epoch is empty/malformed.
///
/// Caller relies on this to derive the session for review-frontier writes from
/// `mcp__aibridge__review_checkpoint`; treating "manual" as "no session" prevents
/// mutating frontier state under an ambiguous identity.
///
/// NOTE: assumes session ids do not contain `:`. Current `UserPromptSubmit` payloads
/// pass UUIDs which never include `:`, so this is safe in practice. A malformed epoch
/// (missing the `:ts:hash` suffix) returns `None` rather than treating the whole
/// string as a session — a corrupt-but-"approved" state must NOT derive an unsafe
/// session id that mutates review-frontier state.
pub fn current_session(cwd: &str) -> Option<String> {
    let state = read_state(cwd)?;
    let epoch = state.get("epoch").and_then(Value::as_str)?;
    session_from_epoch(epoch)
}

/// Pure epoch→session parser (no IO) so the malformed-input handling is unit-testable.
/// Returns the session segment ONLY for a well-formed `{session}:{ts}:{hash}` epoch
/// with three non-empty parts; `None` for `"manual"` or any malformed shape.
fn session_from_epoch(epoch: &str) -> Option<String> {
    if epoch == "manual" {
        return None;
    }
    let parts: Vec<&str> = epoch.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    if parts[0].is_empty() || parts[1].is_empty() || parts[2].is_empty() {
        return None;
    }
    Some(parts[0].to_string())
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

/// True when the current epoch is approved AND no DIFFERENT plan is currently under
/// review. The in-flight review marker (separate `pending` file) lets a changed-plan
/// re-submission re-block writes during its (minutes-long) review WITHOUT any
/// authority-state write from `begin_review`: if a pending review for THIS epoch
/// names a plan whose hash differs from the approved one, the prior approval is
/// treated as superseded until the new plan is approved.
fn effectively_approved(cwd: &str) -> bool {
    if !is_approved(cwd) {
        return false;
    }
    // v0.31 P1: an UNCONSUMED pending user-turn marker suspends effective approval until a
    // PreToolUse [`reconcile_pending_user_turn`] resolves it. Fail closed: a present-but-
    // malformed marker also reads as pending. (No marker is ever written when the flag is on,
    // so this is a no-op in the default per-turn-reset mode.)
    if has_pending_user_turn(cwd) {
        return false;
    }
    if !pending_path(cwd).exists() {
        return true; // no in-flight review → approval stands
    }
    match read_pending(cwd) {
        Some((pe, ph)) if pe == current_epoch(cwd) => {
            // counts as approved only if the in-flight review is the SAME plan that
            // was approved (idempotent re-check); a different plan is still in review.
            read_state(cwd).and_then(|s| s.get("approved_plan_hash").and_then(Value::as_u64))
                == Some(ph)
        }
        Some(_) => true, // marker from another epoch → ignore (start_epoch clears it)
        // Present but unreadable → conservatively treat as a changed-plan review in
        // flight (a corrupt marker must NOT fail open).
        None => false,
    }
}

/// The gate is currently HOLDING writes (enabled, not bypassed, current task not
/// effectively approved). Used by both the PreToolUse hook AND the in-process `run`
/// tool, so `run` (which executes arbitrary shell) can't bypass the hook.
pub fn blocks_writes(cwd: &str) -> bool {
    is_enabled(cwd) && !bypassed() && !effectively_approved(cwd)
}

/// v0.23.0: Public wrapper for [`effectively_approved`]. The current epoch is
/// approved AND no DIFFERENT plan is currently under review. Used by
/// `mcp__aibridge__review_checkpoint` so a stale approval cannot mutate the
/// frontier while a changed-plan re-submission is in flight.
pub fn is_effectively_approved(cwd: &str) -> bool {
    effectively_approved(cwd)
}

/// Is this tool one the gate must hold until approval? Includes `mcp__aibridge__run`
/// (arbitrary shell) — a write path the file-tool matcher would otherwise miss.
pub fn is_gated_tool(tool_name: &str) -> bool {
    tool_name == "Bash"
        || tool_name == "mcp__aibridge__run"
        || GATED_WRITE_TOOLS.contains(&tool_name)
}

/// PreToolUse enforcement (legacy 2-arg surface): `Some(deny_json)` to block a write
/// before approval, `None` to let the caller proceed. Thin wrapper over [`enforce_tool`]
/// with an EMPTY command — so the Bash path can never satisfy the (deferred) read-only
/// carve-out, keeping every existing 2-arg caller byte-identical to pre-P2 behavior.
pub fn enforce(cwd: &str, tool_name: &str) -> Option<String> {
    enforce_tool(cwd, tool_name, "")
}

/// v0.31 (P2): whether the read-only orientation carve-out may EXECUTE a Bash command
/// pre-approval. Returns `false` in P2 — the carve-out additionally requires a hardened
/// execution layer (trusted-exe resolution, clean env, no shell startup/functions/aliases,
/// argv execution) that does NOT yet exist; a parser-only proof can't prove read-only
/// EXECUTION (shell functions/aliases/builtins, PATH-hijack, `BASH_ENV`). Flipped to a
/// real capability check when that layer lands. Keeping it const-false makes the read-only
/// branch in [`enforce_tool`] INERT, so the gate's behavior is unchanged in P2.
fn read_only_execution_supported() -> bool {
    false
}

/// PreToolUse enforcement with the Bash command threaded in: `Some(deny_json)` to block
/// a write/Bash before approval, `None` to let the caller proceed (incl. its own rtk
/// handling for Bash). The command is only consulted on the Bash read-only-orientation
/// path; for every other tool (and an empty command) the decision is exactly as before.
pub fn enforce_tool(cwd: &str, tool_name: &str, command: &str) -> Option<String> {
    if !is_gated_tool(tool_name) {
        return None;
    }
    // v0.31 P1: when the gate is actually enforcing, consume any pending user-turn marker
    // BEFORE deciding. A scope-delta turn revokes + denies here (user_scope_delta); a trivial
    // continuation just clears the marker so `blocks_writes` below sees the preserved approval.
    // Skipped when bypassed/disabled (don't re-gate a turn while the gate is off), and a no-op
    // when `resetOnUserTurn` is on (no marker is ever written).
    if is_enabled(cwd) && !bypassed() {
        if let Some(deny) = reconcile_pending_user_turn(cwd) {
            return Some(deny);
        }
    }
    if !blocks_writes(cwd) {
        return None;
    }
    // v0.31 (P2) read-only orientation carve-out (DEFAULT OFF; currently INERT). When the
    // flag is ON *and* a hardened execution layer exists, a Bash command PROVEN read-only
    // could run with no approved plan. In P2 `read_only_execution_supported()` is const-false,
    // so this branch never allows — behavior is byte-identical to the strict default. The
    // command is threaded now so the follow-up only flips the capability + adds the proof.
    if tool_name == "Bash"
        && read_only_execution_supported()
        && crate::review_mcp::read_only_orientation()
        && read_only_proven(cwd, command)
    {
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
    // Pre-approval block. A flag-ON Bash command the read-only carve-out could not prove
    // safe denies with the dedicated parser reason code; everything else is the default
    // no-active-approval block. (Both are inert-equivalent in P2 because the carve-out is
    // disabled, but the code selection is wired for the follow-up.)
    let reason = if tool_name == "Bash"
        && read_only_execution_supported()
        && crate::review_mcp::read_only_orientation()
    {
        BlockReason::ReadOnlyParserDenial
    } else {
        BlockReason::NoActiveApproval
    };
    Some(deny_json(reason))
}

/// v0.31 (P2): whether a Bash `command` is PROVEN read-only (and path-confined to `cwd`).
/// DEFERRED — the sound implementation needs the hardened execution layer
/// ([`read_only_execution_supported`]); until then this always returns `false` so the
/// carve-out never allows. Threaded through [`enforce_tool`] so only this function + the
/// capability flag change when the parser/exec layer lands.
fn read_only_proven(_cwd: &str, _command: &str) -> bool {
    false
}

/// Machine-readable block reason codes for the `PLAN_GATE_REQUIRED:` family (v0.31
/// P6). Emitted as the first token after the tag — `PLAN_GATE_REQUIRED: <code> — …`
/// — so an operator or wrapping tool can branch on the CAUSE without parsing prose;
/// the human recovery hint still follows. Adding a code is OBSERVABILITY ONLY: it
/// does not change WHEN the gate blocks. The risk-delta and receipt-mismatch families
/// carry their own tags (`PLAN_RISK_DELTA_REQUIRED:` / `PLAN_RECEIPT_MISMATCH:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// No Codex-approved plan for the current task epoch (the default pre-approval block).
    NoActiveApproval,
    /// A previously-valid approval expired (TTL / session / receipt). Wired by P1/receipt.
    ApprovalExpired,
    /// The user's latest message changed scope/objective beyond the approved plan. Wired by P1.
    UserScopeDelta,
    /// HEAD moved to a commit the approval cannot reconcile against. Wired by P1/P4.
    HeadMoved,
    /// The working tree changed outside the approved frontier. Wired by P1.
    WorkingTreeDelta,
    /// A read-only-orientation command the parser could not prove safe. Wired by P2.
    ReadOnlyParserDenial,
}

impl BlockReason {
    /// The stable snake_case token emitted right after the `PLAN_GATE_REQUIRED:` tag.
    pub fn code(self) -> &'static str {
        match self {
            BlockReason::NoActiveApproval => "no_active_approval",
            BlockReason::ApprovalExpired => "approval_expired",
            BlockReason::UserScopeDelta => "user_scope_delta",
            BlockReason::HeadMoved => "head_moved",
            BlockReason::WorkingTreeDelta => "working_tree_delta",
            BlockReason::ReadOnlyParserDenial => "read_only_parser_denial",
        }
    }

    /// A short human recovery hint appended after the code.
    fn recovery_hint(self) -> &'static str {
        match self {
            BlockReason::NoActiveApproval => {
                "this task has no Codex-approved plan yet. Do NOT retry this tool. First gather \
                 context with Read/Grep/Glob, form a todolist, then call the MCP tool \
                 `mcp__aibridge__plan_gate` with a structured plan (todos, approach, \
                 intended_files, risk_surfaces, test_plan). Revise and call it again until it \
                 returns <AI-BRIDGE-APPROVE/>; only then will writes/Bash be allowed. If \
                 `mcp__aibridge__plan_gate` is NOT available, AI Bridge was just installed/updated \
                 — the tool connects only after a Claude Code restart: restart Claude Code, or \
                 relaunch it with AIBRIDGE_PLAN_GATE=0 set to bypass the gate for this session."
            }
            BlockReason::ApprovalExpired => {
                "the prior approval expired — re-file the plan with `mcp__aibridge__plan_gate` to \
                 refresh it before writing."
            }
            BlockReason::UserScopeDelta => {
                "your latest message changed the task's scope beyond the approved plan — re-file \
                 the updated plan with `mcp__aibridge__plan_gate` before writing."
            }
            BlockReason::HeadMoved => {
                "the commit HEAD moved in a way the approval cannot reconcile — re-file the plan \
                 with `mcp__aibridge__plan_gate` before writing."
            }
            BlockReason::WorkingTreeDelta => {
                "the working tree changed outside the approved frontier — re-file the plan with \
                 `mcp__aibridge__plan_gate` before writing."
            }
            BlockReason::ReadOnlyParserDenial => {
                "this command could not be proven read-only — run an approved plan via \
                 `mcp__aibridge__plan_gate`, or use Read/Grep/Glob for discovery."
            }
        }
    }
}

/// The `PLAN_GATE_REQUIRED:` deny prose for a given reason (code + recovery hint).
fn block_message(reason: BlockReason) -> String {
    format!(
        "PLAN_GATE_REQUIRED: {} — {}",
        reason.code(),
        reason.recovery_hint()
    )
}

/// The operational deny — names the machine-readable reason code AND tells Claude
/// exactly what to do (call the tool, do NOT retry the blocked edit), so it advances
/// the dialogue instead of looping.
fn deny_json(reason: BlockReason) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": block_message(reason)
        }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Revocable, scope-bound approval (plan-gate v2)
//
// A single APPROVE no longer permanently unlocks the whole epoch. Approval is
// revoked when: (a) a later non-APPROVE verdict comes back for THIS epoch
// (`record`), (b) a materially different plan is submitted while approved
// (`begin_review`), or (c) an unapproved HIGH-RISK command is attempted
// (`enforce_risk`). New user prompt = new epoch stays the outer boundary. We do
// NOT fence ordinary file writes (self-reported `intended_files` is a weak
// boundary and hard-fencing trains users to disable the gate) — instead the
// approved plan is fed to the Stop gate, which compares it against the actual diff
// (every changed file) and flags out-of-scope or unplanned high-risk changes.
// ---------------------------------------------------------------------------

fn push_unique(out: &mut Vec<&'static str>, c: &'static str) {
    if !out.contains(&c) {
        out.push(c);
    }
}

/// Does `tok` name the program `name`, tolerating a `.exe` suffix and a path
/// prefix (`/usr/bin/git`, `C:\\bin\\git.exe`)? Used for the leading program of a
/// (sub)command so `git.exe push` / `/bin/rm -rf` are not missed.
fn token_is_cmd(tok: &str, name: &str) -> bool {
    let base = tok.rsplit(['/', '\\']).next().unwrap_or(tok);
    base == name
        || base
            .strip_suffix(".exe")
            .map(|b| b == name)
            .unwrap_or(false)
}

/// A downloader piped into a shell interpreter (`curl … | sh`, `iwr … | iex`,
/// `wget … | sudo bash`). Checked on the raw lowered string because it is about
/// the pipe itself, before separators are flattened for tokenizing.
fn is_pipe_to_shell(lowered: &str) -> bool {
    let has_dl = [
        "curl ",
        "wget ",
        "iwr ",
        "irm ",
        "invoke-webrequest",
        "invoke-restmethod",
    ]
    .iter()
    .any(|p| lowered.contains(p));
    if !has_dl {
        return false;
    }
    lowered.split('|').skip(1).any(|seg| {
        let mut toks = seg.split_whitespace();
        let mut first = toks.next().unwrap_or("");
        if first == "sudo" {
            first = toks.next().unwrap_or("");
        }
        matches!(
            first,
            "sh" | "bash" | "zsh" | "dash" | "ksh" | "iex" | "pwsh" | "powershell"
        )
    })
}

/// The two SHAPES a high-risk command can take within its class (v0.31 P3). A grant of
/// the bare class authorizes only `Standard`; a WIDENED variant (e.g. a force/mirror/
/// tags/delete push) is more destructive within the SAME class and must be granted
/// explicitly (`class:widened`). For v1 P3, shape detection is scoped to the
/// `remote-publish` push family — every OTHER class is always `Standard` (so this
/// tightening adds no new false-deny surface to the existing families).
pub const SHAPE_STANDARD: &str = "standard";
pub const SHAPE_WIDENED: &str = "widened";

/// A reviewer-authorized risk grant: a class plus the SHAPE within that class it
/// authorizes. JSON-serializable (via [`RiskGrant::to_value`] / [`RiskGrant::from_value`])
/// so it round-trips through state + receipts without pulling in serde-derive (the crate
/// only depends on `serde_json`); P4 will canonicalize fingerprints over this. A
/// `widened` grant covers both shapes; a `standard` grant covers ONLY `standard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskGrant {
    pub class: String,
    pub shape: String,
}

impl RiskGrant {
    fn standard(class: &str) -> Self {
        RiskGrant {
            class: class.to_string(),
            shape: SHAPE_STANDARD.to_string(),
        }
    }

    /// Serialize to a `{ "class": .., "shape": .. }` JSON object.
    pub fn to_value(&self) -> Value {
        json!({ "class": self.class, "shape": self.shape })
    }

    /// Parse from a JSON object, accepting ONLY a known class + known shape (so a
    /// malformed/forged entry yields `None` and the caller fails safe). Both fields
    /// required and string-typed.
    pub fn from_value(v: &Value) -> Option<Self> {
        let class = v.get("class").and_then(Value::as_str)?;
        let shape = v.get("shape").and_then(Value::as_str)?;
        if !RISK_CLASSES.contains(&class) {
            return None;
        }
        if shape != SHAPE_STANDARD && shape != SHAPE_WIDENED {
            return None;
        }
        Some(RiskGrant {
            class: class.to_string(),
            shape: shape.to_string(),
        })
    }

    /// Serialize a slice of grants to a JSON array.
    pub fn vec_to_value(grants: &[RiskGrant]) -> Value {
        Value::Array(grants.iter().map(RiskGrant::to_value).collect())
    }
    /// Does this grant cover a command of `(class, shape)`? Same class AND the grant's
    /// shape covers the command's shape (a `widened` grant covers standard+widened; a
    /// `standard` grant covers ONLY standard).
    fn covers(&self, class: &str, shape: &str) -> bool {
        self.class == class && (self.shape == SHAPE_WIDENED || self.shape == shape)
    }
}

/// True when this `git push` token set is a WIDENED push (more destructive than a
/// plain publish): force/delete/mirror/all/prune/tag-publication forms. Detection is
/// token-based on the already-flattened tokens. Clustered short flags (`-uf`, `-fu`)
/// are handled by inspecting any single-dash (non-`--`) token's letters for `f`/`d`.
fn push_is_widened(tokens: &[&str]) -> bool {
    // A clustered short flag like `-f`/`-uf`/`-fu`/`-d` containing `letter`; excludes `--long`.
    let short_flag_has = |letter: char| {
        tokens
            .iter()
            .any(|t| t.starts_with('-') && !t.starts_with("--") && t[1..].contains(letter))
    };
    tokens.iter().any(|t| {
        matches!(
            *t,
            "--force"
                | "--force-if-includes"
                | "--mirror"
                | "--all"
                | "--prune"
                | "--delete"
                | "--tags"
                | "--follow-tags"
        ) || t.starts_with("--force-with-lease") // bare or `=<ref>`
            || t.starts_with('+') // leading-+ (force-update) refspec, e.g. `+main`/`+src:dst`
            || t.starts_with(':') // delete refspec, e.g. `:stale`
            || t.starts_with("refs/tags/") // explicit tag refspec
    }) || short_flag_has('f')
        || short_flag_has('d')
        // explicit `tag <name>` push form (`git push origin tag v1.2.3`)
        || tokens.windows(2).any(|w| w[0] == "tag" && !w[1].is_empty())
}

/// True when ANY `git push` SUB-COMMAND in `lowered` is a widened push. Segments the
/// command on shell separators FIRST, so widening flags from a SIBLING sub-command (e.g.
/// the `-f` in `git push origin main && rm -f stale.log`, or a `:done`/`+x` operand) can't
/// leak into a plain push's shape. Only the segment that actually contains `git … push`
/// is inspected by [`push_is_widened`]. (Class detection still uses the flattened tokens;
/// only SHAPE is segment-scoped — over-broad class detection merely fails closed.)
fn git_push_is_widened(lowered: &str) -> bool {
    lowered
        .split(|c: char| "|&;\n\r()".contains(c))
        .any(|segment| {
            let toks: Vec<&str> = segment
                .split_whitespace()
                .map(|t| t.trim_matches(|c: char| "`'\".,!?".contains(c)))
                .filter(|t| !t.is_empty())
                .collect();
            let is_push = toks.iter().any(|t| token_is_cmd(t, "git")) && toks.contains(&"push");
            is_push && push_is_widened(&toks)
        })
}

/// Scan free text (a command OR a plan) for ALL high-risk command grants present, each
/// as a `(class, shape)` [`RiskGrant`]. Token-based (not raw substring) so `warm -reset`/
/// `git pushd`/a path containing a risk phrase don't false-trigger, and `git.exe push`/
/// `/bin/rm -rf` aren't missed. Shell separators are flattened so each sub-command's
/// program is matched on its own; surrounding quotes/backticks/punctuation are trimmed so
/// prose like "run `git push`" classifies too. False positives only cost one extra plan
/// round. Shape is `widened` only for a widened `remote-publish` push (see
/// [`push_is_widened`]); every other class is `standard`.
fn scan_risk_grants(text: &str) -> Vec<RiskGrant> {
    let lowered = text.to_lowercase();
    let mut out: Vec<RiskGrant> = Vec::new();
    let mut push = |g: RiskGrant| {
        if !out.contains(&g) {
            out.push(g);
        }
    };
    if is_pipe_to_shell(&lowered) {
        push(RiskGrant::standard("pipe-to-shell"));
    }
    // Flatten shell separators so tokens from adjacent sub-commands don't fuse.
    let spaced: String = lowered
        .chars()
        .map(|c| if "|&;\n\r\t()".contains(c) { ' ' } else { c })
        .collect();
    let tokens: Vec<&str> = spaced
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| "`'\".,!?".contains(c)))
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return out;
    }
    let has = |t: &str| tokens.contains(&t);
    let has_any = |opts: &[&str]| tokens.iter().any(|x| opts.contains(x));
    let has_prefix = |p: &str| tokens.iter().any(|x| x.starts_with(p));
    let has_cmd = |name: &str| tokens.iter().any(|t| token_is_cmd(t, name));
    let seq = |a: &str, b: &str| tokens.windows(2).any(|w| w[0] == a && w[1] == b);
    // A clustered short flag like `-rf`/`-r`/`-fr` (recursive); excludes `--long`.
    let recursive_flag = || {
        tokens.iter().any(|t| {
            (t.starts_with('-') && !t.starts_with("--") && t.contains('r')) || *t == "--recursive"
        })
    };

    // destructive filesystem
    if (has_cmd("rm") && recursive_flag())
        || (has_cmd("remove-item") && has_any(&["-recurse", "-r"]))
        || (has_any(&["rmdir", "rd"]) && has("/s"))
        || (has_cmd("del") && has_any(&["/s", "/q"]))
    {
        push(RiskGrant::standard("destructive-fs"));
    }

    // remote publish / deploy
    let is_git_push = has_cmd("git") && has("push");
    if is_git_push
        || (has_any(&["npm", "yarn", "pnpm", "bun"]) && has("publish"))
        || (has_cmd("cargo") && has("publish"))
        || (has_cmd("gh") && has("release") && has_any(&["create", "upload", "edit", "delete"]))
        || (has_cmd("docker") && has("push"))
        || (has_any(&["vercel", "netlify", "wrangler", "firebase", "fly", "flyctl"])
            && has("deploy"))
        || (has_cmd("vercel") && has("--prod"))
    {
        // SHAPE is `widened` only for a widened `git push` (the one push family whose
        // flags meaningfully broaden destructiveness within the class). Other publish
        // forms stay `standard` for v1.
        let shape = if is_git_push && git_push_is_widened(&lowered) {
            SHAPE_WIDENED
        } else {
            SHAPE_STANDARD
        };
        push(RiskGrant {
            class: "remote-publish".to_string(),
            shape: shape.to_string(),
        });
    }

    // destructive DB / schema / migration execution
    if seq("drop", "table")
        || seq("drop", "database")
        || seq("truncate", "table")
        || (has_cmd("prisma") && has("migrate") && has_any(&["deploy", "reset"]))
        || (has_cmd("drizzle-kit") && has_any(&["migrate", "push"]))
        || (has_cmd("knex") && has_prefix("migrate"))
        || (has_cmd("alembic") && has("upgrade"))
        || (has_cmd("rails") && has_prefix("db:migrate"))
        || (has_any(&["sequelize", "sequelize-cli"]) && has_prefix("db:migrate"))
        || (has_cmd("typeorm") && has("migration:run"))
        || (has_cmd("supabase") && has("db") && has("push"))
    {
        push(RiskGrant::standard("db-migration"));
    }

    // infrastructure mutation
    if (has_cmd("terraform") && has_any(&["apply", "destroy"]))
        || (has_cmd("pulumi") && has_any(&["up", "destroy"]))
        || (has_cmd("kubectl") && has_any(&["apply", "delete"]))
    {
        push(RiskGrant::standard("infra-mutation"));
    }

    // payment money-movement (Stripe/Braintree): PaymentIntents/Checkout/charges/
    // captures/payouts/transfers — the modern money-movement surfaces. Tolerate the
    // singular/plural resource spellings the CLIs accept.
    let stripe_resource = |opts: &[&str]| has_cmd("stripe") && has("create") && has_any(opts);
    if stripe_resource(&[
        "payment_intents",
        "payment_intent",
        "paymentintents",
        "paymentintent",
        "charges",
        "charge",
        "payouts",
        "payout",
        "transfers",
        "transfer",
    ]) || (has_cmd("stripe") && has("capture") && has_any(&["charges", "charge", "payment_intents", "payment_intent"]))
        || (has_cmd("stripe") && has("checkout") && has_any(&["sessions", "session"]) && has("create"))
        || (has_cmd("braintree") && has("transaction") && has("sale"))
    {
        push(RiskGrant::standard("payment"));
    }

    // refund: explicit refund surfaces.
    if (has_cmd("stripe") && has_any(&["refunds", "refund"]) && has("create"))
        || (has_cmd("stripe") && has("refund"))
        || (has_cmd("braintree") && has("refund"))
    {
        push(RiskGrant::standard("refund"));
    }

    // webhook: triggering/sending or creating webhook endpoints.
    if (has_cmd("stripe") && has("trigger"))
        || (has_cmd("stripe") && has_any(&["webhook_endpoints", "webhook_endpoint"]) && has("create"))
        || (has_cmd("svix") && has_any(&["create", "send"]))
        || (has("webhook") && has_any(&["create", "trigger", "send"]))
    {
        push(RiskGrant::standard("webhook"));
    }

    // queue: enqueue/purge/drain on known message brokers.
    if (has_cmd("aws") && has("sqs") && has_any(&["send-message", "purge-queue"]))
        || (has_cmd("celery") && has("purge"))
        || (has_cmd("rabbitmqadmin") && has("publish"))
    {
        push(RiskGrant::standard("queue"));
    }

    // admin-auth: credential / token / access mutation.
    if (has_cmd("aws")
        && has("iam")
        && has_any(&["create-access-key", "create-user", "attach-user-policy"]))
        || (has_cmd("gh") && has("auth"))
        || (has_cmd("vercel") && has("tokens") && has("create"))
        || (has_cmd("gcloud")
            && has("iam")
            && has("service-accounts")
            && has("keys")
            && has("create"))
    {
        push(RiskGrant::standard("admin-auth"));
    }
    out
}

/// Scan free text for ALL high-risk command CLASSES present (shape collapsed away).
/// Built on [`scan_risk_grants`]; kept for callers that only need class names.
fn scan_risk_classes(text: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for g in scan_risk_grants(text) {
        if let Some(canon) = RISK_CLASSES.iter().find(|c| **c == g.class) {
            push_unique(&mut out, canon);
        }
    }
    out
}

/// The single highest-risk class of one command (the first match), or `None` for
/// an ordinary command. Public for display; the gate uses [`unapproved_high_risk`]
/// which checks EVERY class so a chained unapproved command can't hide.
pub fn high_risk_class(command: &str) -> Option<&'static str> {
    scan_risk_classes(command).into_iter().next()
}

/// The canonical high-risk class names the gate understands (and that the reviewer
/// may authorize via a `RISK-APPROVED:` line).
pub const RISK_CLASSES: &[&str] = &[
    "destructive-fs",
    "remote-publish",
    "db-migration",
    "infra-mutation",
    "pipe-to-shell",
    "payment",
    "refund",
    "webhook",
    "queue",
    "admin-auth",
];

/// Parse the high-risk command classes the REVIEWER explicitly authorized, from a
/// `RISK-APPROVED: class, class` line in Codex's review text. Authorization comes
/// from the reviewer — which understands the plan's intent, including a "do NOT run
/// X" instruction — NOT from scanning the plan prose, so a plan merely *mentioning*
/// a dangerous command can't silently pre-authorize it. No line (or no known class)
/// → empty, so every high-risk command re-arms the gate (fail-safe).
pub fn parse_risk_approved(review: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for line in review.lines() {
        // Require a STANDALONE reviewer-owned line: after stripping leading markdown
        // list/quote/emphasis chars, it must START with the tag — so the tag merely
        // QUOTED inside explanatory prose can't authorize anything.
        let lower = line.to_lowercase();
        let head = lower.trim_start_matches(|c: char| c.is_whitespace() || "-*>#`".contains(c));
        let Some(rest) = head.strip_prefix("risk-approved:") else {
            continue;
        };
        for tok in rest.split(',') {
            let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-');
            if let Some(canon) = RISK_CLASSES.iter().find(|c| **c == t) {
                push_unique(&mut out, canon);
            }
        }
    }
    out
}

/// Parse the reviewer's authorized risk GRANTS — each a `(class, shape)` — from the same
/// standalone `RISK-APPROVED:` line(s) as [`parse_risk_approved`] (v0.31 P3). Each
/// comma-separated token is either a bare `class` (→ `standard` shape) or `class:shape`
/// where `shape` is `standard`/`widened`; an unknown shape falls back to `standard`
/// (never widens by accident), and an unknown class is ignored. So `remote-publish`
/// authorizes only a plain push, while `remote-publish:widened` authorizes a
/// force/mirror/tags/delete push too. Same reviewer-owned-line discipline as
/// `parse_risk_approved`, so a tag merely quoted in prose still authorizes nothing.
pub fn parse_risk_grants(review: &str) -> Vec<RiskGrant> {
    let mut out: Vec<RiskGrant> = Vec::new();
    for line in review.lines() {
        let lower = line.to_lowercase();
        let head = lower.trim_start_matches(|c: char| c.is_whitespace() || "-*>#`".contains(c));
        let Some(rest) = head.strip_prefix("risk-approved:") else {
            continue;
        };
        for tok in rest.split(',') {
            let cleaned = tok.trim();
            // Split an optional `:shape` suffix; both halves are trimmed of stray
            // non-alphanumeric/`-` punctuation (mirrors parse_risk_approved's token clean).
            let trim_word = |s: &str| {
                s.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')
                    .to_string()
            };
            let (class_raw, shape_raw) = match cleaned.split_once(':') {
                Some((c, s)) => (trim_word(c), trim_word(s)),
                None => (trim_word(cleaned), String::new()),
            };
            let Some(canon) = RISK_CLASSES.iter().find(|c| **c == class_raw) else {
                continue; // unknown class → ignore (fail-safe)
            };
            let shape = if shape_raw == SHAPE_WIDENED {
                SHAPE_WIDENED
            } else {
                SHAPE_STANDARD // bare or unknown shape → standard (never widen by accident)
            };
            let g = RiskGrant {
                class: canon.to_string(),
                shape: shape.to_string(),
            };
            // Dedupe by class, keeping the BROADEST shape (widened wins) so two tokens
            // for the same class can't downgrade a widened grant.
            if let Some(existing) = out.iter_mut().find(|e| e.class == g.class) {
                if g.shape == SHAPE_WIDENED {
                    existing.shape = SHAPE_WIDENED.to_string();
                }
            } else {
                out.push(g);
            }
        }
    }
    out
}

/// Command classes already authorized by the current approved plan (legacy flat list,
/// kept for back-compat / display). The AUTHORITATIVE post-approval check is
/// [`approved_risk_grants`] (structured class+shape).
pub fn approved_command_classes(cwd: &str) -> Vec<String> {
    read_state(cwd)
        .and_then(|s| {
            s.get("approved_command_classes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// The structured risk GRANTS (class+shape) authorized for the current approved plan
/// (v0.31 P3). Only KNOWN classes and the two known shapes are accepted; any malformed
/// entry is dropped (fail-safe — a grant that can't be parsed authorizes nothing). This
/// is the authoritative source for [`unapproved_high_risk`].
fn approved_risk_grants(cwd: &str) -> Vec<RiskGrant> {
    read_state(cwd)
        .and_then(|s| {
            s.get("approved_risk_grants")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(RiskGrant::from_value).collect())
        })
        .unwrap_or_default()
}

/// The plan text approved for the current epoch (kept for the Stop gate's
/// scope-vs-diff comparison; cleared when a new epoch starts). Survives a revoke
/// so the Stop gate still knows what scope was last agreed.
pub fn approved_plan(cwd: &str) -> Option<String> {
    read_state(cwd)?
        .get("approved_plan")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Revoke the current epoch's approval (re-arm the gate). `reason` is recorded for
/// `doctor`/diagnostics. Best-effort: a missing state file means nothing to revoke.
pub fn revoke(cwd: &str, reason: &str) {
    if let Some(mut s) = read_state(cwd) {
        set_field(&mut s, "approved", json!(false));
        if let Some(o) = s.as_object_mut() {
            o.remove("approved_epoch");
        }
        set_field(&mut s, "status", json!("pending"));
        set_field(&mut s, "revoked_reason", json!(reason));
        let _ = write_state(cwd, &s);
    }
}

/// Called by the `plan_gate` tool BEFORE the (minutes-long) Codex review. Records
/// THIS submission (epoch + plan hash) in the separate `pending` file. Two effects,
/// both WITHOUT writing the authority state (so it can't race `start_epoch`):
///   1. While a plan whose hash differs from the approved one is in review,
///      [`effectively_approved`] returns false → writes re-block (closes the
///      "approve narrow phase-1, execute broad phase-2 under one epoch" gap).
///   2. [`record`] approves only if this marker is still the in-flight plan, so a
///      superseded review's late APPROVE can't take effect.
///
/// Re-submitting the SAME approved plan keeps approval (marker hash == approved).
pub fn begin_review(cwd: &str, plan: &str) {
    if !is_enabled(cwd) {
        return;
    }
    write_pending(cwd, &current_epoch(cwd), hash_str(plan));
}

/// A post-approval risk delta the approved plan did not cover (v0.31 P3). Either a
/// brand-NEW risk class, or a same-class WIDENED variant (e.g. a force/mirror/tags
/// push under a plain `remote-publish` grant). Both re-arm the gate; they differ only
/// in the reason code emitted so the operator/agent sees WHY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskDelta {
    /// A high-risk class the approved plan did not authorize at all.
    NewClass(&'static str),
    /// A WIDENED variant of an already-granted class (only the standard shape was granted).
    Widened(&'static str),
}

impl RiskDelta {
    fn class(&self) -> &'static str {
        match self {
            RiskDelta::NewClass(c) | RiskDelta::Widened(c) => c,
        }
    }
}

/// Post-approval risk DELTA the approved plan did NOT cover, for a Bash/run command —
/// `None` when the command is ordinary, fully covered by a grant, the gate is
/// off/bypassed, or the plan isn't approved (pre-approval is already blocked by
/// [`enforce`]). Checks EVERY `(class, shape)` the command contains against the
/// structured grants; reports the FIRST uncovered one (a chained
/// `git push && terraform destroy` can't pass just because its first class is granted).
/// A class that IS granted but only at `standard` while the command is `widened` yields
/// [`RiskDelta::Widened`]; an entirely ungranted class yields [`RiskDelta::NewClass`].
/// Does not mutate state.
fn risk_delta(cwd: &str, tool_name: &str, command: &str) -> Option<RiskDelta> {
    if !is_enabled(cwd) || bypassed() {
        return None;
    }
    if tool_name != "Bash" && tool_name != "mcp__aibridge__run" {
        return None;
    }
    if !effectively_approved(cwd) {
        return None;
    }
    let grants = approved_risk_grants(cwd);
    for g in scan_risk_grants(command) {
        // Map the scanned class to its 'static canonical so the delta carries a
        // 'static name (matches the public surface). Unknown classes are skipped.
        let Some(canon) = RISK_CLASSES.iter().find(|c| **c == g.class) else {
            continue;
        };
        if grants.iter().any(|a| a.covers(&g.class, &g.shape)) {
            continue; // covered
        }
        // Distinguish a brand-new class from a same-class widened variant.
        return Some(if grants.iter().any(|a| a.class == g.class) {
            RiskDelta::Widened(canon)
        } else {
            RiskDelta::NewClass(canon)
        });
    }
    None
}

/// Post-approval risk class that the approved plan did NOT cover, for a Bash/run
/// command — `None` when the command is ordinary, its class is already approved,
/// the gate is off/bypassed, or the plan isn't approved. Thin wrapper over
/// [`risk_delta`] returning just the class (the public surface mcp.rs run-tool uses).
pub fn unapproved_high_risk(cwd: &str, tool_name: &str, command: &str) -> Option<&'static str> {
    risk_delta(cwd, tool_name, command).map(|d| d.class())
}

/// Public form of [`risk_delta`] for the in-process `run` tool, so it can emit the
/// SAME widened-vs-new-class reason as the PreToolUse hook (a widened push gets the
/// `risk_policy_widened` message, not the generic new-class one). Does not mutate state.
pub fn run_risk_delta(cwd: &str, tool_name: &str, command: &str) -> Option<RiskDelta> {
    risk_delta(cwd, tool_name, command)
}

/// PreToolUse risk gate (runs AFTER [`enforce`] returns allow): if an approved
/// task attempts an unapproved high-risk command (a new class OR a widened same-class
/// variant), revoke approval and DENY so the plan is re-reviewed with the command in
/// scope. `None` to allow.
pub fn enforce_risk(cwd: &str, tool_name: &str, command: &str) -> Option<String> {
    let delta = risk_delta(cwd, tool_name, command)?;
    revoke(cwd, "high_risk_command_delta");
    Some(risk_deny_json(&delta))
}

/// Human-readable instruction for a NEW-class high-risk re-gate (shared by the hook
/// deny and the `run` tool's plain-text reply).
pub fn risk_delta_message(class: &str) -> String {
    format!(
        "PLAN_RISK_DELTA_REQUIRED: new_risk_surface={class} — this command is a high-risk class \
         ('{class}') that the approved plan did not cover, so the plan gate has re-armed. Do NOT \
         retry this command. Update your plan to name this exact command under risk_surfaces \
         (e.g. `git push`, `prisma migrate deploy`, `rm -rf`), call `mcp__aibridge__plan_gate` \
         again, and once it returns <AI-BRIDGE-APPROVE/> this command class is allowed for the task."
    )
}

/// Human-readable instruction for a same-class WIDENED re-gate (v0.31 P3): the class
/// was granted, but only its standard shape — this widened variant (force/mirror/tags/
/// delete push, etc.) needs an explicit `class:widened` grant.
pub fn risk_widened_message(class: &str) -> String {
    format!(
        "PLAN_RISK_DELTA_REQUIRED: risk_policy_widened class={class} — this command is a WIDENED \
         variant of the '{class}' class (e.g. a force/mirror/tags/delete push) that goes beyond the \
         plain '{class}' grant the plan was approved with, so the plan gate has re-armed. Do NOT \
         retry this command. Update your plan to name this exact widened command under \
         risk_surfaces, have the reviewer authorize it as `{class}:widened`, call \
         `mcp__aibridge__plan_gate` again, and once it returns <AI-BRIDGE-APPROVE/> this widened \
         command is allowed for the task."
    )
}

/// The deny prose for a risk delta — new-class vs widened-same-class (v0.31 P3).
pub fn risk_delta_reason(delta: &RiskDelta) -> String {
    match delta {
        RiskDelta::NewClass(c) => risk_delta_message(c),
        RiskDelta::Widened(c) => risk_widened_message(c),
    }
}

fn risk_deny_json(delta: &RiskDelta) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": risk_delta_reason(delta)
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
    set_field(&mut s, "rounds", json!(rounds));

    match verdict {
        crate::gate::Verdict::Approve => {
            // Stale-review guard: only approve the plan that is still the in-flight
            // submission. `begin_review` records (epoch, plan_hash) in the separate
            // `pending` file; if a NEWER plan was submitted for THIS epoch while this
            // review ran, refuse — its APPROVE is superseded. (No marker → skipped,
            // e.g. gate-off manual use or a direct unit-test call.)
            if pending_path(cwd).exists() {
                match read_pending(cwd) {
                    // A newer plan for THIS epoch superseded this review.
                    Some((pe, ph)) if pe == epoch && ph != hash_str(plan) => {
                        return Outcome::NeedsInfo(
                            "AI Bridge: a newer plan was submitted while this one was under \
                             review — re-submit the CURRENT plan to plan_gate before proceeding."
                                .to_string(),
                        );
                    }
                    // Present but unreadable → can't confirm this IS the current plan;
                    // refuse rather than approve against a corrupt marker.
                    None => {
                        return Outcome::NeedsInfo(
                            "AI Bridge: the in-flight review marker is unreadable — re-submit the \
                             current plan to plan_gate before proceeding."
                                .to_string(),
                        );
                    }
                    _ => {}
                }
            }
            set_field(&mut s, "approved", json!(true));
            // Bind approval to THIS epoch so is_approved() can reject a stale flag.
            set_field(&mut s, "approved_epoch", json!(epoch));
            set_field(&mut s, "approved_plan_hash", json!(hash_str(plan)));
            // Pre-authorize ONLY the high-risk classes/shapes the REVIEWER explicitly
            // allowed (its `RISK-APPROVED:` line) — never inferred from plan prose. The
            // structured grants (class+shape) are the AUTHORITATIVE post-approval check
            // (v0.31 P3); the legacy flat class list is kept for back-compat/display.
            // Keep the plan text for the Stop gate's scope-vs-diff comparison.
            set_field(
                &mut s,
                "approved_command_classes",
                json!(parse_risk_approved(findings)),
            );
            set_field(
                &mut s,
                "approved_risk_grants",
                RiskGrant::vec_to_value(&parse_risk_grants(findings)),
            );
            set_field(&mut s, "approved_plan", json!(cap_plan(plan)));
            set_field(&mut s, "status", json!("approved"));
            set_field(&mut s, "revoked_reason", Value::Null);
            set_field(&mut s, "same_findings", json!(0));
            let _ = write_state(cwd, &s);
            Outcome::Approved
        }
        crate::gate::Verdict::RequestChanges => {
            // A non-APPROVE verdict for THIS epoch REVOKES any prior approval — the
            // gate must never tell Claude to "revise" while writes stay unlocked.
            set_field(&mut s, "approved", json!(false));
            if let Some(o) = s.as_object_mut() {
                o.remove("approved_epoch");
            }
            set_field(&mut s, "status", json!("rejected"));
            set_field(&mut s, "revoked_reason", json!("request_changes"));
            let fh = hash_str(findings);
            let prev = s.get("last_findings_hash").and_then(Value::as_u64);
            let same = if prev == Some(fh) {
                s.get("same_findings").and_then(Value::as_u64).unwrap_or(0) + 1
            } else {
                1
            };
            set_field(&mut s, "last_findings_hash", json!(fh));
            set_field(&mut s, "same_findings", json!(same));
            let _ = write_state(cwd, &s);
            if same >= NO_PROGRESS_THRESHOLD as u64 {
                Outcome::Stuck(findings.to_string())
            } else {
                Outcome::Revise(findings.to_string())
            }
        }
        crate::gate::Verdict::Blocked | crate::gate::Verdict::Unparseable => {
            // Same fail-closed contract: a non-decision must not leave writes open.
            set_field(&mut s, "approved", json!(false));
            if let Some(o) = s.as_object_mut() {
                o.remove("approved_epoch");
            }
            set_field(&mut s, "status", json!("needs_info"));
            set_field(&mut s, "revoked_reason", json!("blocked"));
            let _ = write_state(cwd, &s);
            Outcome::NeedsInfo(findings.to_string())
        }
    }
}

/// Fast-path resume: re-approve the CURRENT epoch from a matching plan RECEIPT (a
/// reload-resume of an already-Codex-approved plan) WITHOUT a fresh review round.
/// Mirrors [`record`]'s Approve path, but the risk GRANTS come from the RECEIPT
/// (a real reviewer's prior `RISK-APPROVED`, with their authorized class+shape), never
/// re-parsed — so a resume can't grant a class or widen a shape that was never reviewed.
/// Same fail-safe state contract + epoch TOCTOU guard as `record`. Returns `true` only
/// if approval was actually recorded; `false` (task changed / missing state) means the
/// caller must run a full review.
pub fn record_resume(cwd: &str, expected_epoch: &str, plan: &str, grants: &[RiskGrant]) -> bool {
    // Fail-safe: when enabled, never approve from missing/unparseable state.
    let mut s = match read_state(cwd) {
        Some(s) => s,
        None if !is_enabled(cwd) => {
            json!({ "epoch": current_epoch(cwd), "approved": false })
        }
        None => return false,
    };
    let epoch = s
        .get("epoch")
        .and_then(Value::as_str)
        .unwrap_or("manual")
        .to_string();
    // The task changed under us (a new prompt started a new epoch) → refuse.
    if epoch != expected_epoch {
        return false;
    }
    // Legacy flat class list derived from the structured grants (back-compat/display).
    let classes: Vec<String> = grants.iter().map(|g| g.class.clone()).collect();
    set_field(&mut s, "approved", json!(true));
    set_field(&mut s, "approved_epoch", json!(epoch));
    set_field(&mut s, "approved_plan_hash", json!(hash_str(plan)));
    set_field(&mut s, "approved_command_classes", json!(classes));
    set_field(
        &mut s,
        "approved_risk_grants",
        RiskGrant::vec_to_value(grants),
    );
    set_field(&mut s, "approved_plan", json!(cap_plan(plan)));
    set_field(&mut s, "status", json!("approved"));
    set_field(&mut s, "revoked_reason", Value::Null);
    set_field(&mut s, "same_findings", json!(0));
    write_state(cwd, &s).is_ok()
}

/// The Codex prompt for a plan review round: judge the plan, end with one verdict
/// tag (reusing the Stop-gate sentinels so there is ONE verdict parser).
pub fn prompt(plan: &str) -> String {
    format!(
        "You are AI Bridge's PRE-EXECUTION plan gate. The agent is about to start coding and \
         has submitted the plan/todolist below. Judge the APPROACH before any code is written: \
         is the plan correct, complete, and safe? Look for wrong approach, missing steps, \
         unhandled edge cases, risky surfaces (auth/payments/migrations/data-loss/deploy), \
         scope creep, and missing tests. Judge the CODE approach only: PROCESS/meta steps the \
         plan lists (commit, push, checkpoint, advancing the review frontier, running gates/tests) \
         are NOT plan defects — do not flag them as missing. Compile-ability is verified by the \
         local build/clippy gate, not here — do not reject a plan on speculative \"won't compile\" \
         grounds.\n\n\
         Write:\n\
         1. FINDINGS: if the plan is sound, write \"No blocking concerns.\"; otherwise list ALL \
         blocking concerns with the plan in THIS single pass — be comprehensive so the agent can \
         address them together; do NOT hold a known concern back for a later round. Focus on PLAN \
         blockers (wrong approach, missing steps/invariants, unsafe sequencing, untested risky \
         surfaces, scope mismatch) — this is a plan review, not a whole-repo audit; do NOT pad \
         with non-blocking nitpicks.\n\
         2. If — and only if — the plan legitimately REQUIRES high-risk commands that you are \
         approving, add a line listing those classes (omit it entirely otherwise):\n\
         RISK-APPROVED: <comma-separated subset of: remote-publish, db-migration, destructive-fs, \
         infra-mutation, pipe-to-shell, payment, refund, webhook, queue, admin-auth>\n\
         Only list a class the plan genuinely needs; if the plan says NOT to run such a command, \
         do NOT list it. Unlisted high-risk commands will be re-gated before they run. A bare \
         class authorizes only its STANDARD form (e.g. `remote-publish` authorizes a plain \
         `git push` but NOT a force/mirror/tags/delete push, and not tag publication). To \
         authorize a WIDENED variant, write `class:widened` (e.g. `remote-publish:widened`); only \
         do so when the plan genuinely needs that more-destructive form.\n\
         3. A final line that is EXACTLY one of:\n\
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

    // --- plan-gate v2: revocable, scope-bound approval --------------------------

    fn approve(cwd: &str, plan: &str) {
        record(
            cwd,
            &current_epoch(cwd),
            plan,
            &crate::gate::Verdict::Approve,
            "",
        );
    }

    #[test]
    fn session_from_epoch_parses_only_well_formed() {
        // Well-formed 3-part epoch → session segment.
        assert_eq!(
            session_from_epoch("sess-uuid:1234:9abc").as_deref(),
            Some("sess-uuid")
        );
        // "manual" (no prompt started) → None.
        assert_eq!(session_from_epoch("manual"), None);
        // Single token (no delimiters) → None (must not treat whole string as session).
        assert_eq!(session_from_epoch("garbage-no-colons"), None);
        // Wrong part count → None.
        assert_eq!(session_from_epoch("a:b"), None);
        assert_eq!(session_from_epoch("a:b:c:d"), None);
        // Empty segments → None.
        assert_eq!(session_from_epoch("sess:123:"), None);
        assert_eq!(session_from_epoch(":123:abc"), None);
        assert_eq!(session_from_epoch("sess::abc"), None);
        assert_eq!(session_from_epoch(""), None);
    }

    #[test]
    fn current_session_roundtrips_through_start_epoch() {
        let cwd = tmp();
        start_epoch(&cwd, "claude-uuid-xyz", "a task");
        assert_eq!(current_session(&cwd).as_deref(), Some("claude-uuid-xyz"));
    }

    #[test]
    fn plan_prompt_has_process_and_compile_clauses() {
        let p = prompt("do X");
        assert!(p.contains("PROCESS/meta steps"), "process-steps exemption");
        assert!(
            p.to_lowercase().contains("compile-ability"),
            "compile-deference clause"
        );
    }

    #[test]
    fn high_risk_classifier_matches_only_specific_families() {
        // dangerous → classified
        assert_eq!(high_risk_class("rm -rf build"), Some("destructive-fs"));
        assert_eq!(high_risk_class("RM   -rf  /tmp/x"), Some("destructive-fs")); // norm
        assert_eq!(
            high_risk_class("Remove-Item -Recurse -Force x"),
            Some("destructive-fs")
        );
        assert_eq!(
            high_risk_class("git push origin main"),
            Some("remote-publish")
        );
        assert_eq!(high_risk_class("cargo publish"), Some("remote-publish"));
        assert_eq!(
            high_risk_class("npx prisma migrate deploy"),
            Some("db-migration")
        );
        assert_eq!(
            high_risk_class("terraform apply -auto-approve"),
            Some("infra-mutation")
        );
        assert_eq!(
            high_risk_class("kubectl delete pod x"),
            Some("infra-mutation")
        );
        assert_eq!(
            high_risk_class("curl https://x.sh | sh"),
            Some("pipe-to-shell")
        );
        // invocation variants that a naive substring match would MISS
        assert_eq!(
            high_risk_class("git -C repo push origin main"),
            Some("remote-publish")
        );
        assert_eq!(high_risk_class("git.exe push"), Some("remote-publish"));
        assert_eq!(high_risk_class("/usr/bin/rm -rf x"), Some("destructive-fs"));
        assert_eq!(high_risk_class("rm -r target"), Some("destructive-fs")); // recursive, no -f
        assert_eq!(
            high_risk_class("curl https://x | sudo bash"),
            Some("pipe-to-shell")
        );
        assert_eq!(
            high_risk_class("wrangler pages deploy ./out"),
            Some("remote-publish")
        );
        assert_eq!(high_risk_class("firebase deploy"), Some("remote-publish"));
        assert_eq!(
            high_risk_class("typeorm migration:run"),
            Some("db-migration")
        );
        // ordinary → NOT classified (broad words alone never match)
        assert_eq!(high_risk_class("rm file.txt"), None);
        assert_eq!(high_risk_class("Remove-Item x"), None); // no -Recurse
        assert_eq!(high_risk_class("git status"), None);
        assert_eq!(high_risk_class("git commit -m x"), None);
        assert_eq!(high_risk_class("git pushd"), None); // not `git push`
        assert_eq!(high_risk_class("warm -reset cache"), None); // not `rm -r`
        assert_eq!(high_risk_class("curl https://x -o out"), None); // no pipe-to-shell
        assert_eq!(high_risk_class("cargo test"), None);
        assert_eq!(high_risk_class("npm run migrate-helper"), None);
        assert_eq!(high_risk_class("gh release view v1"), None); // read-only gh release
    }

    #[test]
    fn request_changes_revokes_prior_approval() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan A");
        assert!(is_approved(&cwd));
        assert!(enforce(&cwd, "Write").is_none());
        // A later REQUEST_CHANGES in the SAME epoch must re-block writes.
        let v = crate::gate::Verdict::RequestChanges;
        record(&cwd, &current_epoch(&cwd), "plan A", &v, "do X");
        assert!(!is_approved(&cwd), "request_changes must revoke approval");
        assert!(blocks_writes(&cwd));
        assert!(enforce(&cwd, "Write").is_some());
    }

    #[test]
    fn blocked_verdict_revokes_prior_approval() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan A");
        assert!(is_approved(&cwd));
        record(
            &cwd,
            &current_epoch(&cwd),
            "plan A",
            &crate::gate::Verdict::Blocked,
            "need info",
        );
        assert!(!is_approved(&cwd), "blocked must revoke approval");
    }

    #[test]
    fn changed_plan_under_review_reblocks_writes_without_authority_write() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan A");
        // Re-submitting the SAME plan keeps approval (idempotent re-check).
        begin_review(&cwd, "plan A");
        assert!(!blocks_writes(&cwd), "same plan keeps approval");
        // A materially different plan under review re-blocks writes — via the
        // separate `pending` marker, WITHOUT touching the authority state (so it
        // can't race start_epoch). The raw approval flag is untouched.
        begin_review(&cwd, "plan B — much broader");
        assert!(
            blocks_writes(&cwd),
            "changed plan under review blocks writes"
        );
        assert!(
            is_approved(&cwd),
            "authority state untouched by begin_review"
        );
    }

    #[test]
    fn reviewer_risk_approved_authorizes_command_classes() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "deploy task");
        // The REVIEWER explicitly authorizes remote-publish in its review output.
        record(
            &cwd,
            &current_epoch(&cwd),
            "deploy plan",
            &crate::gate::Verdict::Approve,
            "Looks good.\nRISK-APPROVED: remote-publish",
        );
        assert!(approved_command_classes(&cwd)
            .iter()
            .any(|c| c == "remote-publish"));
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push origin main"),
            None
        );
        // …but a class the reviewer did NOT authorize is still gated.
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "prisma migrate deploy"),
            Some("db-migration")
        );
    }

    #[test]
    fn plan_prose_naming_a_command_does_not_authorize_it() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        // Plan MENTIONS git push but the reviewer added NO RISK-APPROVED line —
        // authorization must come from the reviewer, not from plan prose.
        record(
            &cwd,
            &current_epoch(&cwd),
            "Plan: we will run `git push` at the end.",
            &crate::gate::Verdict::Approve,
            "No blocking concerns.",
        );
        assert!(approved_command_classes(&cwd).is_empty());
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push"),
            Some("remote-publish")
        );
    }

    #[test]
    fn enforce_risk_revokes_and_denies_unapproved_command() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "refactor");
        approve(&cwd, "Plan: refactor the component."); // no high-risk command named
        assert!(is_approved(&cwd));
        // An ordinary command after approval is allowed and does not revoke.
        assert!(enforce_risk(&cwd, "Bash", "cargo test").is_none());
        assert!(is_approved(&cwd));
        // A high-risk command the plan didn't cover → deny + re-arm the gate.
        let deny = enforce_risk(&cwd, "Bash", "git push origin main");
        assert!(deny.is_some());
        assert!(deny.unwrap().contains("PLAN_RISK_DELTA_REQUIRED"));
        assert!(!is_approved(&cwd), "high-risk delta must revoke approval");
        assert!(blocks_writes(&cwd));
    }

    #[test]
    fn unapproved_high_risk_is_noop_before_approval() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        // Pre-approval is enforce()'s job; the risk gate must not fire yet.
        assert_eq!(unapproved_high_risk(&cwd, "Bash", "git push"), None);
        assert!(enforce_risk(&cwd, "Bash", "git push").is_none());
    }

    #[test]
    fn chained_command_cannot_hide_an_unapproved_class() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "deploy");
        // Reviewer authorizes remote-publish only.
        record(
            &cwd,
            &current_epoch(&cwd),
            "deploy plan",
            &crate::gate::Verdict::Approve,
            "ok\nRISK-APPROVED: remote-publish",
        );
        // First class (remote-publish) is approved, but the SECOND (infra-mutation)
        // is not — the gate must still catch it (no first-class masking).
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push && terraform destroy"),
            Some("infra-mutation")
        );
    }

    #[test]
    fn stale_review_approve_is_refused() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        // Plan A starts review (stamps pending=hash(A))…
        begin_review(&cwd, "plan A");
        // …but a newer plan B is submitted before A's verdict returns.
        begin_review(&cwd, "plan B");
        // A's late APPROVE is superseded → refused, gate stays closed.
        assert!(matches!(
            record(&cwd, &epoch, "plan A", &crate::gate::Verdict::Approve, ""),
            Outcome::NeedsInfo(_)
        ));
        assert!(!is_approved(&cwd));
        // The CURRENT plan (B) can still be approved normally.
        assert!(matches!(
            record(&cwd, &epoch, "plan B", &crate::gate::Verdict::Approve, ""),
            Outcome::Approved
        ));
        assert!(is_approved(&cwd));
    }

    #[test]
    fn approved_plan_survives_revoke_for_stop_gate() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "Plan: edit src/a.rs");
        assert_eq!(approved_plan(&cwd).as_deref(), Some("Plan: edit src/a.rs"));
        // approved_plan survives a later revoke (the Stop gate still needs the scope).
        revoke(&cwd, "high_risk_command_delta");
        assert!(!is_approved(&cwd));
        assert_eq!(approved_plan(&cwd).as_deref(), Some("Plan: edit src/a.rs"));
        // …but a NEW epoch clears it (start_epoch resets state).
        start_epoch(&cwd, "sess", "next task");
        assert_eq!(approved_plan(&cwd), None);
    }

    #[test]
    fn corrupt_pending_marker_fails_closed() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan A");
        assert!(!blocks_writes(&cwd)); // approved, no in-flight review
                                       // A present-but-unreadable pending marker must NOT fail open.
        std::fs::write(pending_path(&cwd), b"{ not json").unwrap();
        assert!(blocks_writes(&cwd), "corrupt pending marker must block");
        // …and record must refuse to approve against a corrupt marker.
        assert!(matches!(
            record(
                &cwd,
                &current_epoch(&cwd),
                "plan A",
                &crate::gate::Verdict::Approve,
                ""
            ),
            Outcome::NeedsInfo(_)
        ));
    }

    #[test]
    fn risk_approved_must_be_a_standalone_line() {
        // A standalone line authorizes (markdown bullet tolerated)…
        assert_eq!(
            parse_risk_approved("findings\n- RISK-APPROVED: remote-publish, db-migration"),
            vec!["remote-publish", "db-migration"]
        );
        // …but the tag merely QUOTED inside prose does not.
        assert!(parse_risk_approved("Do not add a RISK-APPROVED: remote-publish line.").is_empty());
        // Unknown class names are ignored.
        assert!(parse_risk_approved("RISK-APPROVED: launch-missiles").is_empty());
    }

    #[test]
    fn record_resume_unlocks_like_a_real_approve() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        // The receipt fast-path: begin_review stamps the pending marker for THIS plan,
        // then record_resume approves from the receipt's grants (no Codex round).
        begin_review(&cwd, "plan A");
        assert!(record_resume(
            &cwd,
            &epoch,
            "plan A",
            &[RiskGrant::standard("remote-publish")]
        ));
        assert!(is_approved(&cwd));
        // Writes must ACTUALLY unlock — guards that record_resume's approved_plan_hash
        // (legacy hash_str) matches begin_review's pending marker so effectively_approved
        // is true even though the RECEIPT layer hashes the plan with SHA-256 separately.
        assert!(!blocks_writes(&cwd));
        assert!(enforce(&cwd, "Write").is_none());
        // Restored classes are exactly the receipt's (RISK-APPROVED preserved).
        assert!(approved_command_classes(&cwd)
            .iter()
            .any(|c| c == "remote-publish"));
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push origin main"),
            None
        );
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "prisma migrate deploy"),
            Some("db-migration") // a class NOT in the receipt is still gated
        );
    }

    #[test]
    fn record_resume_refuses_stale_epoch() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task one");
        let stale = current_epoch(&cwd);
        start_epoch(&cwd, "sess", "task two"); // a new task → new epoch
                                               // A resume bound to the OLD epoch must not unlock the new task.
        assert!(!record_resume(&cwd, &stale, "plan A", &[]));
        assert!(!is_approved(&cwd));
        assert!(blocks_writes(&cwd));
    }

    // --- v0.31 Step 1: regression locks for the CURRENT strict behavior ----------
    // These pin behaviors that the planned relaxations will touch — P1 (state-based
    // invalidation), P2 (read-only orientation), P3 (structured risk grants), P4
    // (canonical fingerprints) — so each later change surfaces as a DELIBERATE,
    // test-visible diff instead of a silent safety regression. They assert TODAY's
    // behavior, not the target behavior. No env mutation / no real-HOME IO (B1).

    #[test]
    fn gated_tool_surface_is_exactly_writes_bash_and_run() {
        // The exact set the gate holds pre-approval. P2 will relax Bash specifically;
        // locking the surface makes that relaxation an explicit, reviewable change.
        for t in [
            "Write",
            "Edit",
            "MultiEdit",
            "NotebookEdit",
            "Bash",
            "mcp__aibridge__run",
        ] {
            assert!(is_gated_tool(t), "{t} must be gated");
        }
        for t in [
            "Read",
            "Grep",
            "Glob",
            "LS",
            "TodoWrite",
            "mcp__aibridge__plan_gate",
            "mcp__aibridge__review_diff",
        ] {
            assert!(!is_gated_tool(t), "{t} must NOT be gated");
        }
    }

    #[test]
    fn bash_is_gated_pre_approval_regardless_of_command() {
        // TODAY enforce() takes only the tool name and denies ALL Bash before approval —
        // even a read-only `git status`/`ls`. P2 will add a narrow read-only allowance
        // behind a flag; this documents the strict default so that change stays visible.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        assert!(
            enforce(&cwd, "Bash").is_some(),
            "all Bash is denied pre-approval today (no read-only carve-out yet)"
        );
    }

    #[test]
    fn enforce_tool_threads_command_but_is_behavior_neutral_in_p2() {
        // P2 wires the command through enforce_tool + an INERT read-only-orientation seam
        // (read_only_execution_supported() is const-false), so a Bash command is STILL
        // denied pre-approval — with the default no_active_approval code, not the parser
        // code — and the 2-arg enforce() wrapper is identical to enforce_tool(..,"").
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        // A read-only-looking Bash command is denied (carve-out is inert in P2).
        let deny = enforce_tool(&cwd, "Bash", "ls -la src").expect("Bash denied pre-approval");
        let v: Value = serde_json::from_str(&deny).expect("deny is valid json");
        let reason = v
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            reason.contains("PLAN_GATE_REQUIRED: no_active_approval"),
            "inert P2 carve-out must keep the default code, got: {reason}"
        );
        // Writes are denied regardless of any command argument.
        assert!(enforce_tool(&cwd, "Write", "ls").is_some());
        // The wrapper and the empty-command call agree (byte-identical OFF path).
        assert_eq!(enforce(&cwd, "Bash"), enforce_tool(&cwd, "Bash", ""));
        // read_only_proven is deferred → never proves anything in P2.
        assert!(!read_only_proven(&cwd, "ls"));
        assert!(!read_only_execution_supported());
    }

    #[test]
    fn different_findings_reset_the_no_progress_counter() {
        // Only REPEATED identical findings escalate to Stuck; a different finding resets
        // the counter so a productive multi-round dialogue is never cut off early.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let v = crate::gate::Verdict::RequestChanges;
        assert!(matches!(
            record(&cwd, &current_epoch(&cwd), "p1", &v, "finding X"),
            Outcome::Revise(_)
        ));
        // A DIFFERENT finding resets same_findings → still Revise (not Stuck).
        assert!(matches!(
            record(&cwd, &current_epoch(&cwd), "p2", &v, "finding Y"),
            Outcome::Revise(_)
        ));
        // Now the SAME finding repeats → second identical → Stuck.
        assert!(matches!(
            record(&cwd, &current_epoch(&cwd), "p3", &v, "finding Y"),
            Outcome::Stuck(_)
        ));
    }

    #[test]
    fn denied_write_increments_the_denied_counter() {
        // enforce() bumps `denied_writes` for operator diagnostics. P6 layers machine-
        // readable reason codes on this observability, so lock the counter now.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        assert!(enforce(&cwd, "Write").is_some());
        assert!(enforce(&cwd, "Write").is_some());
        let n = read_state(&cwd)
            .unwrap()
            .get("denied_writes")
            .and_then(Value::as_u64)
            .unwrap();
        assert_eq!(n, 2, "two denied writes must be counted");
    }

    #[test]
    fn same_class_widening_re_gates_under_a_standard_grant() {
        // v0.31 P3 (TIGHTENING): a bare `remote-publish` grant authorizes ONLY a plain
        // push; a WIDENED variant (force/mirror/tags/delete, etc.) re-gates. This was the
        // KNOWN GAP locked by the old `same_class_widening_is_not_separately_gated_today`
        // test — now the tightening is the asserted behavior.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "deploy");
        record(
            &cwd,
            &current_epoch(&cwd),
            "deploy plan",
            &crate::gate::Verdict::Approve,
            "ok\nRISK-APPROVED: remote-publish",
        );
        // Plain push is allowed (standard shape, in the granted shape).
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push origin main"),
            None
        );
        // Force push is now RE-GATED (widened shape beyond the standard grant).
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push --force origin main"),
            Some("remote-publish")
        );
        // …still classified as remote-publish, not a distinct class.
        assert_eq!(
            high_risk_class("git push --force origin main"),
            Some("remote-publish")
        );
    }

    #[test]
    fn every_widened_push_form_re_gates_under_standard_grant() {
        // EXHAUSTIVE: each widened `git push` form must re-gate when only the standard
        // `remote-publish` shape was granted (a standard grant must NOT cover any of the
        // higher-risk push variants — force/delete/mirror/all/prune/tag-publication).
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "deploy");
        record(
            &cwd,
            &current_epoch(&cwd),
            "deploy plan",
            &crate::gate::Verdict::Approve,
            "ok\nRISK-APPROVED: remote-publish",
        );
        for cmd in [
            "git push --force origin main",
            "git push -f origin main",
            "git push -uf origin main",     // clustered force
            "git push -fu origin main",     // clustered force (reordered)
            "git push --force-with-lease",
            "git push --force-with-lease=origin/main",
            "git push --force-if-includes origin main",
            "git push --mirror origin",
            "git push --tags origin",
            "git push --follow-tags origin main",
            "git push --prune origin",
            "git push --all origin",
            "git push --delete origin x",
            "git push -d origin x",
            "git push origin :stale",       // delete refspec
            "git push origin +main",        // leading-+ (force-update) refspec
            "git push origin tag v1.2.3",   // explicit tag push
            "git push origin refs/tags/v1.2.3",
        ] {
            assert_eq!(
                unapproved_high_risk(&cwd, "Bash", cmd),
                Some("remote-publish"),
                "widened push must re-gate: {cmd}"
            );
        }
        // …but plain / non-widening short clusters stay standard (allowed).
        for cmd in [
            "git push origin main",
            "git push",
            "git push -u origin main", // -u is not force/delete → standard
        ] {
            assert_eq!(
                unapproved_high_risk(&cwd, "Bash", cmd),
                None,
                "plain push must stay allowed: {cmd}"
            );
        }
    }

    #[test]
    fn explicit_widened_grant_allows_a_force_push() {
        // A reviewer who writes `remote-publish:widened` authorizes the widened variant too
        // (and a plain push remains allowed — widened covers standard).
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "deploy");
        record(
            &cwd,
            &current_epoch(&cwd),
            "force-deploy plan",
            &crate::gate::Verdict::Approve,
            "ok\nRISK-APPROVED: remote-publish:widened",
        );
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push --force origin main"),
            None
        );
        assert_eq!(
            unapproved_high_risk(&cwd, "Bash", "git push origin main"),
            None
        );
    }

    #[test]
    fn chained_plain_push_is_not_widened_by_a_sibling_subcommand() {
        // Regression: a plain `git push` chained with a sibling sub-command that happens to
        // carry an `-f`/`-d`/leading-`+`/leading-`:` token must STAY standard — the widening
        // flags belong to the sibling, not the push. (Pre-fix, scan_risk_grants flattened ALL
        // sub-commands into one token list and mis-flagged these as widened → over-strict.)
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "deploy");
        record(
            &cwd,
            &current_epoch(&cwd),
            "deploy plan",
            &crate::gate::Verdict::Approve,
            "ok\nRISK-APPROVED: remote-publish",
        );
        // NB: each sibling here is NOT itself a risk class (`rm -f` without -r is not
        // destructive-fs), so the ONLY grant in play is the push — isolating the shape bug.
        for cmd in [
            "git push origin main && rm -f stale.log", // -f in a sibling
            "git push origin main && git branch -d feature", // -d in a sibling
            "git push origin main && chmod +x foo",    // leading-+ in a sibling
            "git push origin main && echo :done",      // leading-: in a sibling
            "git push origin main; tar -df archive.tar", // clustered -df in a sibling
        ] {
            assert_eq!(
                unapproved_high_risk(&cwd, "Bash", cmd),
                None,
                "plain push chained with a sibling must stay standard: {cmd}"
            );
        }
        // …but a genuinely widened push chained with anything STILL re-gates.
        for cmd in [
            "git push --force origin main && echo done",
            "echo start && git push origin :stale",
        ] {
            assert_eq!(
                unapproved_high_risk(&cwd, "Bash", cmd),
                Some("remote-publish"),
                "widened push in a chain must still re-gate: {cmd}"
            );
        }
    }

    // --- v0.31 P1: state-based invalidation (behind resetOnUserTurn, default TRUE) -------

    #[test]
    fn classify_turn_only_trivial_affirmations_preserve() {
        use TurnClass::*;
        for t in ["ok", "okay", "OK", "Yes", "yes!", "yeah", "yep", "y", "continue", "proceed", "go ahead", "go on", ""] {
            assert_eq!(classify_turn(t), TrivialContinue, "{t:?} must be trivial");
        }
        for t in ["cancel", "stop", "reset", "pause", "abort", "never mind", "start over", "stop, do something else"] {
            assert_eq!(classify_turn(t), ExplicitReset, "{t:?} must be explicit reset");
        }
        for t in [
            "also add a delete endpoint",
            "actually use postgres instead",
            "yes but also push to prod",
            "ok now migrate the db",
            "do it", // not in the tiny allowlist → fail closed to delta
        ] {
            assert_eq!(classify_turn(t), UnknownDelta, "{t:?} must be unknown delta");
        }
    }

    #[test]
    fn preserve_epoch_decision_is_invalidate_unless_trivially_safe() {
        use TurnClass::*;
        // Flag ON (default) → never preserve (today's per-turn reset).
        assert!(!preserve_epoch_decision(true, true, TrivialContinue));
        // No current approval → nothing to preserve.
        assert!(!preserve_epoch_decision(false, false, TrivialContinue));
        // Flag OFF + approved: preserve for trivial AND unknown (the marker defers the
        // authoritative decision to reconcile), but NOT for an explicit reset.
        assert!(preserve_epoch_decision(false, true, TrivialContinue));
        assert!(preserve_epoch_decision(false, true, UnknownDelta));
        assert!(!preserve_epoch_decision(false, true, ExplicitReset));
    }

    #[test]
    fn default_reset_mode_still_re_gates_every_prompt() {
        // With the flag at its default (TRUE) — the config reader returns true in tests —
        // start_epoch must re-arm a fresh epoch even for a trivial "yes", and write NO marker.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        assert!(is_approved(&cwd));
        start_epoch(&cwd, "sess", "yes"); // a trivial continuation, but flag is ON
        assert!(!is_approved(&cwd), "default mode re-gates every prompt");
        assert!(!has_pending_user_turn(&cwd), "no marker in per-turn-reset mode");
        assert!(blocks_writes(&cwd));
    }

    #[test]
    fn pending_turn_suspends_approval_until_reconciled() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        assert!(!blocks_writes(&cwd));
        // Simulate the flag-OFF preserve path: a marker is recorded on the approved epoch.
        assert!(set_pending_user_turn(&cwd, TurnClass::TrivialContinue));
        // Effective approval is SUSPENDED until a PreToolUse reconciles it (fail-safe).
        assert!(!effectively_approved(&cwd));
        assert!(blocks_writes(&cwd));
    }

    #[test]
    fn trivial_continuation_auto_reconciles_and_allows() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        set_pending_user_turn(&cwd, TurnClass::TrivialContinue);
        // enforce (PreToolUse) reconciles the trivial marker → approval restored → allowed.
        assert!(enforce(&cwd, "Write").is_none(), "trivial continuation must auto-allow");
        assert!(!has_pending_user_turn(&cwd), "marker consumed");
        assert!(is_approved(&cwd), "approval preserved across a trivial continuation");
    }

    #[test]
    fn scope_delta_turn_re_gates_with_user_scope_delta() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        set_pending_user_turn(&cwd, TurnClass::UnknownDelta);
        let deny = enforce(&cwd, "Write").expect("scope delta must deny");
        let reason = serde_json::from_str::<Value>(&deny)
            .unwrap()
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        assert!(reason.contains("user_scope_delta"), "{reason}");
        assert!(!is_approved(&cwd), "scope delta revokes approval");
        assert!(blocks_writes(&cwd));
        assert!(!has_pending_user_turn(&cwd), "marker consumed even on re-gate");
    }

    #[test]
    fn malformed_pending_turn_fails_closed() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        // A present-but-malformed marker (classification not a string) must NOT preserve.
        let mut s = read_state(&cwd).unwrap();
        set_field(&mut s, "pending_user_turn", json!({ "classification": 123 }));
        write_state(&cwd, &s).unwrap();
        assert!(!effectively_approved(&cwd), "malformed marker suspends approval");
        let deny = reconcile_pending_user_turn(&cwd);
        assert!(deny.is_some(), "malformed marker must re-gate (fail closed)");
        assert!(!is_approved(&cwd));
    }

    #[test]
    fn reconcile_is_noop_without_a_marker() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        // No marker (default per-turn-reset mode never writes one) → reconcile does nothing.
        assert!(reconcile_pending_user_turn(&cwd).is_none());
        assert!(is_approved(&cwd));
        assert!(!blocks_writes(&cwd));
    }

    #[test]
    fn new_risk_classes_are_detected_and_lookalikes_are_not() {
        // Each new class (payment/refund/webhook/queue/admin-auth) is detected by a
        // representative command and NOT by an ordinary lookalike (false positives only
        // cost a plan round, false negatives are unsafe — so detect the real surfaces).
        for cmd in [
            "stripe payment_intents create --amount 1000",
            "stripe charges create --amount 500",
            "stripe checkout sessions create",
            "stripe payouts create",
            "stripe transfers create --amount 1000",
        ] {
            assert_eq!(high_risk_class(cmd), Some("payment"), "payment: {cmd}");
        }
        assert_eq!(
            high_risk_class("stripe refunds create --charge ch_1"),
            Some("refund")
        );
        for cmd in [
            "stripe trigger payment_intent.succeeded",
            "svix message create app_1 --data x",
        ] {
            assert_eq!(high_risk_class(cmd), Some("webhook"), "webhook: {cmd}");
        }
        for cmd in ["aws sqs purge-queue --queue-url u", "celery -A app purge"] {
            assert_eq!(high_risk_class(cmd), Some("queue"), "queue: {cmd}");
        }
        for cmd in ["aws iam create-access-key --user-name bob", "gh auth login"] {
            assert_eq!(high_risk_class(cmd), Some("admin-auth"), "admin-auth: {cmd}");
        }
        // Ordinary lookalikes must NOT classify.
        for cmd in [
            "stripe logs tail",
            "aws sqs receive-message --queue-url u",
            "gh repo view",
            "celery -A app worker",
            "stripe products list",
        ] {
            assert_eq!(high_risk_class(cmd), None, "lookalike must not classify: {cmd}");
        }
    }

    #[test]
    fn parse_risk_grants_respects_standalone_line_and_shapes() {
        // Standalone reviewer line: bare class → standard shape.
        assert_eq!(
            parse_risk_grants("findings\nRISK-APPROVED: remote-publish"),
            vec![RiskGrant::standard("remote-publish")]
        );
        // `class:widened` → widened shape.
        assert_eq!(
            parse_risk_grants("RISK-APPROVED: remote-publish:widened"),
            vec![RiskGrant {
                class: "remote-publish".to_string(),
                shape: "widened".to_string()
            }]
        );
        // Unknown shape suffix → standard (never widen by accident).
        assert_eq!(
            parse_risk_grants("RISK-APPROVED: remote-publish:bogus"),
            vec![RiskGrant::standard("remote-publish")]
        );
        // Unknown class → ignored.
        assert!(parse_risk_grants("RISK-APPROVED: launch-missiles").is_empty());
        // The tag merely QUOTED inside prose does NOT grant anything.
        assert!(
            parse_risk_grants("Do not add a RISK-APPROVED: remote-publish line.").is_empty()
        );
        // Two tokens for the same class: widened wins (can't be downgraded).
        assert_eq!(
            parse_risk_grants("RISK-APPROVED: remote-publish, remote-publish:widened"),
            vec![RiskGrant {
                class: "remote-publish".to_string(),
                shape: "widened".to_string()
            }]
        );
    }

    #[test]
    fn widened_vs_new_class_delta_messages() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "deploy");
        record(
            &cwd,
            &current_epoch(&cwd),
            "deploy plan",
            &crate::gate::Verdict::Approve,
            "ok\nRISK-APPROVED: remote-publish",
        );
        // A widened same-class command → risk_policy_widened.
        let widened = enforce_risk(&cwd, "Bash", "git push --force origin main")
            .expect("widened push must re-gate");
        let v: Value = serde_json::from_str(&widened).unwrap();
        let reason = v
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            reason.starts_with("PLAN_RISK_DELTA_REQUIRED: risk_policy_widened class=remote-publish"),
            "{reason}"
        );
        // Re-approve (revoked by the prior enforce_risk) then hit a brand-NEW class.
        start_epoch(&cwd, "sess", "deploy 2");
        record(
            &cwd,
            &current_epoch(&cwd),
            "deploy plan 2",
            &crate::gate::Verdict::Approve,
            "ok\nRISK-APPROVED: remote-publish",
        );
        let newclass = enforce_risk(&cwd, "Bash", "terraform apply")
            .expect("new class must re-gate");
        let v2: Value = serde_json::from_str(&newclass).unwrap();
        let reason2 = v2
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            reason2.starts_with("PLAN_RISK_DELTA_REQUIRED: new_risk_surface=infra-mutation"),
            "{reason2}"
        );
    }

    #[test]
    fn prompt_documents_new_classes_and_widened_shape() {
        let p = prompt("do X");
        for class in ["payment", "refund", "webhook", "queue", "admin-auth"] {
            assert!(p.contains(class), "prompt must list new class {class}");
        }
        assert!(p.contains("class:widened"), "prompt must document the :widened shape");
        assert!(
            p.contains("remote-publish:widened"),
            "prompt must give the widened example"
        );
    }

    // --- v0.31 Step 2 / P6: machine-readable block reason codes ------------------

    #[test]
    fn block_reason_codes_are_stable_snake_case() {
        // Each code is the stable token operators/tools branch on — pin them.
        assert_eq!(BlockReason::NoActiveApproval.code(), "no_active_approval");
        assert_eq!(BlockReason::ApprovalExpired.code(), "approval_expired");
        assert_eq!(BlockReason::UserScopeDelta.code(), "user_scope_delta");
        assert_eq!(BlockReason::HeadMoved.code(), "head_moved");
        assert_eq!(BlockReason::WorkingTreeDelta.code(), "working_tree_delta");
        assert_eq!(
            BlockReason::ReadOnlyParserDenial.code(),
            "read_only_parser_denial"
        );
        // Every code is lower snake_case (no spaces/uppercase) so it parses as one token.
        for r in [
            BlockReason::NoActiveApproval,
            BlockReason::ApprovalExpired,
            BlockReason::UserScopeDelta,
            BlockReason::HeadMoved,
            BlockReason::WorkingTreeDelta,
            BlockReason::ReadOnlyParserDenial,
        ] {
            let c = r.code();
            assert!(
                c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_') && !c.is_empty(),
                "code {c:?} must be lower snake_case"
            );
        }
    }

    #[test]
    fn block_message_carries_tag_then_code() {
        // `PLAN_GATE_REQUIRED: <code> — <hint>` so the code is the first token after the tag.
        let m = block_message(BlockReason::NoActiveApproval);
        assert!(m.starts_with("PLAN_GATE_REQUIRED: no_active_approval — "), "{m}");
        assert!(block_message(BlockReason::HeadMoved).starts_with("PLAN_GATE_REQUIRED: head_moved — "));
    }

    #[test]
    fn enforce_deny_is_json_with_the_reason_code() {
        // The PreToolUse deny payload is valid JSON whose reason names the code.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let deny = enforce(&cwd, "Write").expect("denied pre-approval");
        let v: Value = serde_json::from_str(&deny).expect("deny is valid json");
        let reason = v
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            reason.contains("PLAN_GATE_REQUIRED: no_active_approval"),
            "{reason}"
        );
    }

    #[test]
    fn risk_delta_message_names_the_new_risk_surface() {
        // The risk-delta family carries `new_risk_surface=<class>` after its own tag.
        let m = risk_delta_message("remote-publish");
        assert!(
            m.starts_with("PLAN_RISK_DELTA_REQUIRED: new_risk_surface=remote-publish"),
            "{m}"
        );
        // The hook deny wrapping it is valid JSON carrying the same token.
        let deny = risk_deny_json(&RiskDelta::NewClass("db-migration"));
        let v: Value = serde_json::from_str(&deny).unwrap();
        let reason = v
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap();
        assert!(reason.contains("new_risk_surface=db-migration"), "{reason}");
    }
}
