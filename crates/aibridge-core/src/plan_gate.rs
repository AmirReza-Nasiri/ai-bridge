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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
    forced_write_failure()?; // test-only deterministic write-failure seam (no-op in release)
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
fn write_pending(cwd: &str, epoch: &str, plan_hash: u64) -> bool {
    if forced_write_failure().is_err() {
        return false; // test-only deterministic write-failure seam (no-op in release)
    }
    let d = dir(cwd);
    if std::fs::create_dir_all(&d).is_err() {
        return false;
    }
    let body = json!({ "epoch": epoch, "plan_hash": plan_hash }).to_string();
    let tmp = d.join(format!("pending.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        return std::fs::rename(&tmp, pending_path(cwd)).is_ok();
    }
    false
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

// ───────────── state authority serialization (v0.32 workflow-gate Unit A) ─────────────
// `state.json` is read-modify-written from THREE separate processes (the UserPromptSubmit /
// PreToolUse / Stop hooks) AND from MULTIPLE THREADS of the warm MCP server (begin_review /
// record / record_resume / revoke). `write_state`'s atomic temp+rename prevents torn READS but
// NOT lost UPDATES. Every RMW therefore runs under `with_state_lock`: an in-proc Mutex (serializes
// the warm server's threads) plus a cross-process advisory file lock on `state.lock` (serializes
// the hook processes; the OS releases it on process exit, so a crash never leaves a stale lock).
// Both lock-acquire failure and write failure FAIL CLOSED via the `force_block` poison sentinel.

// Test-only deterministic write-failure injection: when a test arms this thread-local,
// `write_state`/`write_pending` return an error so the fail-closed write-failure paths are
// testable. Thread-local → no cross-test race (cargo runs each test on its own thread).
#[cfg(test)]
thread_local! {
    static FORCE_WRITE_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `Ok(())` in release; in test builds, `Err` while the `FORCE_WRITE_FAIL` seam is armed.
#[cfg(test)]
fn forced_write_failure() -> std::io::Result<()> {
    if FORCE_WRITE_FAIL.with(|c| c.get()) {
        return Err(std::io::Error::other("forced write failure (test seam)"));
    }
    Ok(())
}
#[cfg(not(test))]
#[inline(always)]
fn forced_write_failure() -> std::io::Result<()> {
    Ok(())
}

// Bounded budget for acquiring the cross-process `state.lock`. A hook must never hang Claude, so
// acquisition is a bounded try-lock spin; exceeding the budget FAILS CLOSED. Prod is always 2s;
// only tests change it (per-thread) to exercise the timeout paths quickly.
thread_local! {
    static STATE_LOCK_BUDGET: std::cell::Cell<Duration> =
        const { std::cell::Cell::new(Duration::from_secs(2)) };
}

#[cfg(test)]
fn set_state_lock_budget_for_test(d: Duration) {
    STATE_LOCK_BUDGET.with(|c| c.set(d));
}

/// The poison sentinel: a presence-only file meaning "a lock-acquire or write FAILED, so the
/// integrity of `state.json` could not be guaranteed". While present (and not bypassed), the
/// central approval predicate [`effectively_approved`] reads FALSE — so PreToolUse, the in-process
/// `run` tool, and `review_checkpoint` all fail closed. Cleared only by a successful fresh-epoch
/// `start_epoch` (a known PENDING baseline), never by an approval — so it self-heals on the next
/// user turn without ever lifting on uncertain state.
fn force_block_path(cwd: &str) -> PathBuf {
    dir(cwd).join("force_block")
}
fn force_block_active(cwd: &str) -> bool {
    force_block_path(cwd).exists()
}
/// Best-effort: poison the gate (creates the parent dir if needed).
fn set_force_block(cwd: &str) {
    let _ = std::fs::create_dir_all(dir(cwd));
    let _ = std::fs::write(force_block_path(cwd), b"1\n");
}
/// Best-effort: lift the poison (only ever after a fresh PENDING epoch is persisted under lock).
fn clear_force_block(cwd: &str) {
    let _ = std::fs::remove_file(force_block_path(cwd));
}

/// Zero-sized proof-of-lock token. A `&StateGuard` is handed to the `*_locked` mutators so the
/// compiler forbids calling them without holding the state lock (Codex option A — explicit
/// `_locked` variants over a thread-local reentrant guard).
pub(crate) struct StateGuard {
    _private: (),
}

/// Per-lock-path in-process mutex: two THREADS of the warm server serialize their RMW even before
/// the cross-process file lock (whose intra-process thread semantics are subtle/platform-specific).
/// Keyed by path so different repos in one process don't contend.
fn in_proc_mutex(lock_path: &Path) -> Arc<Mutex<()>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let reg = REG.get_or_init(|| Mutex::new(HashMap::new()));
    // Registry poison guards only the map → recover and continue.
    let mut map = reg.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(lock_path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// The `ErrorKind` fs4 returns for a contended try-lock (platform-abstracted). Compared against
/// the SAME helper fs4 uses internally, so the match holds regardless of the concrete OS error.
fn lock_contended_error_kind() -> std::io::ErrorKind {
    fs4::lock_contended_error().kind()
}

/// Bounded try-lock spin. `true` once the exclusive lock is held; `false` on budget exhaustion or a
/// non-contention lock error (both FAIL CLOSED at the caller).
fn acquire_file_lock(file: &std::fs::File, budget: Duration) -> bool {
    let start = Instant::now();
    let contended = lock_contended_error_kind();
    loop {
        match fs4::fs_std::FileExt::try_lock_exclusive(file) {
            Ok(()) => return true,
            Err(e) if e.kind() == contended => {
                if start.elapsed() >= budget {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return false, // a real lock error → fail closed
        }
    }
}

/// Run `f` holding BOTH the in-proc mutex and the cross-process advisory lock on `state.lock`.
/// `None` if the lock could not be acquired within the budget (caller FAILS CLOSED). Releases both
/// on scope exit (fs4 unlocks on `File` drop too). `state.lock` is created once and never deleted —
/// only its OS lock is cycled (UFCS `fs4::fs_std::FileExt::*` avoids ambiguity with std 1.89's inherent
/// `File::lock`/`unlock`, keeping acquire+release on the SAME backend across the 1.82 MSRV).
fn with_state_lock<T>(cwd: &str, f: impl FnOnce(&StateGuard) -> T) -> Option<T> {
    let budget = STATE_LOCK_BUDGET.with(|c| c.get());
    let d = dir(cwd);
    if std::fs::create_dir_all(&d).is_err() {
        return None; // can't create the state dir → fail closed
    }
    let lock_path = d.join("state.lock");
    let m = in_proc_mutex(&lock_path);
    // The mutex guards only ORDERING; the protected resource is the atomic-on-disk file, so a panic
    // mid-section can never tear it. Recover from poison rather than wedge the warm server.
    let _in_proc = m.lock().unwrap_or_else(|e| e.into_inner());
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false) // a lock sentinel — never truncate its (irrelevant) contents
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(_) => return None, // fail closed
    };
    if !acquire_file_lock(&file, budget) {
        return None;
    }
    let out = f(&StateGuard { _private: () });
    let _ = fs4::fs_std::FileExt::unlock(&file); // also released on drop
    Some(out)
}

/// Single non-blocking attempt — for the hot `denied_writes` diagnostic counter, which must never
/// delay the deny path. Skips on ANY contention (the counter is best-effort).
fn with_state_lock_try<T>(cwd: &str, f: impl FnOnce(&StateGuard) -> T) -> Option<T> {
    let d = dir(cwd);
    if std::fs::create_dir_all(&d).is_err() {
        return None;
    }
    let lock_path = d.join("state.lock");
    let m = in_proc_mutex(&lock_path);
    let _in_proc = match m.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return None,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false) // a lock sentinel — never truncate its (irrelevant) contents
        .open(&lock_path)
        .ok()?;
    if fs4::fs_std::FileExt::try_lock_exclusive(&file).is_err() {
        return None;
    }
    let out = f(&StateGuard { _private: () });
    let _ = fs4::fs_std::FileExt::unlock(&file);
    Some(out)
}

/// v0.32 Unit B (operation LEASE) — INERT foundation (sub-units 1-3): the on-disk lease file, the
/// DURABLE deferred-review sentinel, and the read-only fail-closed VALIDITY predicate. NOTHING wires
/// these yet (sub-units 4-9 add operation_begin/end + the UserPromptSubmit/Stop/PreToolUse
/// integration), so `#![allow(dead_code)]` until then and behavior is byte-identical. The validity
/// predicate is Unit B's fail-closed heart: a too-lenient lease would suppress the gate's re-arm
/// across a scope-changing user turn, so it binds the lease to the LIVE approved + scope-fenced
/// epoch and fully bounds time. (Tests live in `plan_gate::tests`, which has the state-setup helpers.)
mod operation_lease {
    #![allow(dead_code)]
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    /// Hard cap on a lease's lifetime — a forged over-long `expires_at` must be rejected (Codex).
    pub(super) const MAX_TTL_MS: u64 = 60 * 60 * 1000; // 60 minutes

    pub(super) fn operation_path(cwd: &str) -> PathBuf {
        dir(cwd).join("operation.json")
    }

    /// Read the lease file (None if absent/unparseable → fail-closed at the caller).
    pub(super) fn read_operation(cwd: &str) -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(operation_path(cwd)).ok()?).ok()
    }

    /// Write the lease atomically (temp+rename). The CALLER must hold `with_state_lock` (the lease is
    /// part of the gate-authority RMW set); this helper only performs the atomic file write.
    pub(super) fn write_operation(cwd: &str, v: &Value) -> std::io::Result<()> {
        let d = dir(cwd);
        std::fs::create_dir_all(&d)?;
        let body = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_string());
        let tmp = d.join(format!("operation.json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, operation_path(cwd))
    }

    /// Best-effort: clear the lease (lease end / invalidation).
    pub(super) fn clear_operation(cwd: &str) {
        let _ = std::fs::remove_file(operation_path(cwd));
    }

    pub(super) fn operation_review_pending_path(cwd: &str) -> PathBuf {
        dir(cwd).join("operation_review_pending")
    }

    /// The DURABLE deferred-review obligation sentinel: set when a valid-lease Stop is SUPPRESSED,
    /// cleared ONLY after a successful deferred review. It outlives lease expiry/clear (unlike a flag
    /// inside operation.json), so "must review OR block" stays enforceable after the lease is gone.
    pub(super) fn operation_review_pending_active(cwd: &str) -> bool {
        // FAIL-CLOSED: a metadata/access ERROR ("cannot determine") must NOT read as "no obligation"
        // — treat it as ACTIVE so the future PreToolUse/Stop caller blocks rather than loses the
        // deferred-review obligation.
        operation_review_pending_path(cwd)
            .try_exists()
            .unwrap_or(true)
    }
    /// Persist the obligation. Returns `Err` if it cannot be written — the future caller MUST fail
    /// closed (block) when this fails, since a suppressed Stop whose obligation was not recorded would
    /// otherwise be lost. Honors the `forced_write_failure` test seam for a deterministic failure test.
    pub(super) fn set_operation_review_pending(cwd: &str) -> std::io::Result<()> {
        forced_write_failure()?;
        std::fs::create_dir_all(dir(cwd))?;
        std::fs::write(operation_review_pending_path(cwd), b"1\n")
    }
    pub(super) fn clear_operation_review_pending(cwd: &str) {
        let _ = std::fs::remove_file(operation_review_pending_path(cwd));
    }

    /// Current wall-clock ms since the UNIX epoch (0 if the clock predates the epoch → fail-safe: 0
    /// reads as before any real `created_at`, making the lease invalid).
    pub(super) fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// The FAIL-CLOSED lease validity predicate (read-only; no lock — atomic reads like
    /// `effectively_approved`). Returns true ONLY when EVERY condition holds; any missing field,
    /// type mismatch, time-bound violation, or live-state mismatch → false.
    pub(super) fn operation_lease_valid(cwd: &str) -> bool {
        operation_lease_valid_with(cwd, crate::review_mcp::operation_lease_enabled())
    }

    /// [`operation_lease_valid`] with the `operationLease` config flag INJECTED, so the predicate is
    /// unit-testable without the operator's real on-disk config (mirrors `start_epoch_inner` /
    /// `enforce_tool_scoped_with`). The public wrapper supplies the live value.
    pub(super) fn operation_lease_valid_with(cwd: &str, enabled: bool) -> bool {
        // 1. opt-in feature flag.
        if !enabled {
            return false;
        }
        // 2. poison ALWAYS wins over a lease.
        if is_force_blocked(cwd) {
            return false;
        }
        // 3. the LIVE approval must still be effective (not revoked/blocked/pending) AND scope-fenced.
        if !is_effectively_approved(cwd) || !scope_is_enforced(cwd) {
            return false;
        }
        let (Some(op), Some(state)) = (read_operation(cwd), read_state(cwd)) else {
            return false;
        };
        // 4. all required fields present + correctly typed.
        let (
            Some(lease_epoch),
            Some(lease_appr_epoch),
            Some(lease_plan_hash),
            Some(lease_generation),
            Some(created_at),
            Some(expires_at),
            Some(_operation_id),
        ) = (
            op.get("epoch").and_then(Value::as_str),
            op.get("approved_epoch").and_then(Value::as_str),
            op.get("approved_plan_hash").and_then(Value::as_u64),
            op.get("approved_generation").and_then(Value::as_u64),
            op.get("created_at").and_then(Value::as_u64),
            op.get("expires_at").and_then(Value::as_u64),
            op.get("operation_id").and_then(Value::as_str),
        )
        else {
            return false;
        };
        let lease_globs = match op.get("approved_allowed_globs").and_then(Value::as_array) {
            Some(a) if !a.is_empty() && a.iter().all(Value::is_string) => a,
            _ => return false, // absent / empty / non-array / non-string element → invalid
        };
        // 5. TIME fully bounded: created_at <= now <= expires_at, expires_at >= created_at, and the
        //    window <= MAX_TTL. The `created_at <= now` lower bound makes a BACKWARD wall-clock
        //    (rollback below created_at) AND a far-FUTURE created_at both fail closed. HONEST residual:
        //    within [created_at, expires_at] the wall clock is trusted; a forward jump only EXPIRES a
        //    lease early (fail-safe).
        let now = now_ms();
        if !(created_at <= now && now <= expires_at && expires_at >= created_at) {
            return false;
        }
        if expires_at.saturating_sub(created_at) > MAX_TTL_MS {
            return false;
        }
        // 6. bind to the LIVE epoch + approval identity + approval GENERATION (revoke + re-approve,
        //    new epoch, or changed plan → invalid). The monotonic generation is what makes a
        //    revoke→same-plan-re-approve cycle fail closed (collision-proof, timing-independent).
        if state.get("epoch").and_then(Value::as_str) != Some(lease_epoch)
            || state.get("approved_epoch").and_then(Value::as_str) != Some(lease_appr_epoch)
            || state.get("approved_plan_hash").and_then(Value::as_u64) != Some(lease_plan_hash)
            || state.get("approved_generation").and_then(Value::as_u64) != Some(lease_generation)
        {
            return false;
        }
        // 7. EXACT live-scope binding: the lease's globs must equal the LIVE approved scope (not just
        //    be a non-empty array of their own), so a forged lease can't fabricate a scope. Combined
        //    with `scope_is_enforced` above, this fails closed unless the lease mirrors a real fence.
        if state
            .get("approved_allowed_globs")
            .and_then(Value::as_array)
            != Some(lease_globs)
        {
            return false;
        }
        true
    }
}

/// Insert/overwrite a top-level field in a JSON object value (no-op if `v` is not
/// an object). Shared by `record`/`revoke` so they all mutate state the same way.
fn set_field(v: &mut Value, k: &str, val: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(k.into(), val);
    }
}

/// v0.32 Unit B: increment the MONOTONIC per-approval generation in `s` (absent → 0). The approval
/// writers call this under the state lock, so an operation lease can bind to a SPECIFIC approval
/// instance; a `revoke` + re-approve (via `record` OR `record_resume`) strictly increases it,
/// invalidating any stale lease regardless of timing. Read by nothing except the lease validity.
/// Returns `false` if the counter would OVERFLOW (a corrupt/hand-edited `u64::MAX`) — gate-authority
/// state must fail closed, never panic (debug) or wrap to 0 (release); the caller refuses approval.
#[must_use]
fn bump_approved_generation(s: &mut Value) -> bool {
    // Distinguish ABSENT/null (legacy/fresh → 0) from PRESENT-but-malformed (corrupt → fail closed).
    // A non-u64 present value must NOT silently reset to 1 (that could collide with a stale lease's
    // generation), and a u64::MAX must not wrap.
    let current = match s.get("approved_generation") {
        None | Some(Value::Null) => 0,
        Some(v) => match v.as_u64() {
            Some(n) => n,
            None => return false, // present but not a u64 → corrupt → fail closed
        },
    };
    let Some(gen) = current.checked_add(1) else {
        return false; // overflow → fail closed
    };
    set_field(s, "approved_generation", json!(gen));
    true
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

/// Pure decision for [`start_epoch`]: given the flag + whether a plan is currently approved +
/// whether a REAL enforced file scope is in force + the prompt's class, should we PRESERVE the
/// approved epoch (recording a pending user-turn marker for the PreToolUse authority) or start a
/// FRESH epoch? `true` = preserve. Kept pure so the policy is unit-testable without config/IO.
///
/// v0.32 Unit 5: `scope_enforced` couples preservation to an active write-time fence — an
/// approval may only be carried across a user turn when its writes are confined to a non-empty
/// reviewer-approved scope (so an out-of-scope write a "trivial yes" might smuggle in is fenced
/// by Unit 4). A non-declaring / declared-but-empty / corrupt scope re-arms exactly as today.
fn preserve_epoch_decision(
    reset_on_user_turn: bool,
    currently_approved: bool,
    scope_enforced: bool,
    class: TurnClass,
) -> bool {
    if reset_on_user_turn || !currently_approved || !scope_enforced {
        return false; // default behavior: every prompt re-arms a fresh epoch
    }
    // Flag OFF + an approved, scope-fenced plan in flight: preserve unless the user explicitly reset.
    !matches!(class, TurnClass::ExplicitReset)
}

/// v0.32 Unit 5: whether a REAL enforced file scope is currently in force — i.e. the approved
/// epoch has a present, non-empty, all-valid `approved_allowed_globs` ([`ScopeState::Globs`]).
/// A non-declaring scope ([`ScopeState::None`]), a declared-but-empty scope
/// ([`ScopeState::DenyAll`]), or corrupt scope state ([`ScopeState::Corrupt`]) is NOT a basis to
/// preserve an approval across a user turn (fail closed → re-arm).
fn scope_is_enforced(cwd: &str) -> bool {
    matches!(scope_in_force_state(cwd), ScopeState::Globs(_))
}

/// v0.31 P1 / v0.32 Unit 5: whether state-based invalidation may actually PRESERVE an approval
/// across a user turn. Returns `true` as of Unit 4 — the missing piece (a PreToolUse tool-context
/// file-scope check that re-gates writes outside the approved files) now exists ([`scope_fence`]),
/// so a preserved approval can no longer fail open on ordinary out-of-scope WRITE-tool writes:
/// those are denied at write-time. Preservation is additionally coupled to an active enforced
/// scope ([`scope_is_enforced`]) and stays behind `planGate.resetOnUserTurn=false` (DEFAULT TRUE,
/// so the default behavior is byte-identical). Accepted residual: Bash/`mcp__aibridge__run` are
/// NOT fenced (an out-of-scope write through them is caught only by the Stop-gate), and a
/// bare-affirmation that broadened the objective WITHIN the approved files is Stop-only; high-risk
/// commands still re-gate via the P3 risk-delta check. (Mirrors P2's `read_only_execution_supported`.)
fn p1_state_invalidation_supported() -> bool {
    true
}

/// Begin a fresh PENDING epoch for a new task (called by the UserPromptSubmit hook),
/// UNLESS state-based invalidation is supported AND `planGate.resetOnUserTurn` is false AND a
/// plan is currently approved AND a real enforced scope is in force AND the prompt is not an
/// explicit reset — in which case the approved epoch is PRESERVED and a pending user-turn marker
/// is recorded for the PreToolUse authority to reconcile before the next mutator. The epoch id
/// ties an approval to THIS task so a later prompt re-gates. With the default `resetOnUserTurn`
/// (true) this always re-arms a fresh epoch — byte-identical to pre-P1.
pub fn start_epoch(cwd: &str, session: &str, prompt: &str) {
    start_epoch_inner(
        cwd,
        session,
        prompt,
        crate::review_mcp::reset_on_user_turn(),
    )
}

/// [`start_epoch`] with the `resetOnUserTurn` config value INJECTED, so the opt-in preservation
/// path is unit-testable without real-HOME config IO. The public wrapper supplies the on-disk
/// value; behavior is otherwise identical.
fn start_epoch_inner(cwd: &str, session: &str, prompt: &str, reset_on_user_turn: bool) {
    if with_state_lock(cwd, |g| {
        start_epoch_locked(cwd, session, prompt, reset_on_user_turn, g)
    })
    .is_none()
    {
        // Lock-acquire failure for a NEW-EPOCH transition → poison + best-effort remove state so a
        // stale approval cannot unlock this new task (the central predicate then denies).
        set_force_block(cwd);
        let _ = std::fs::remove_file(state_path(cwd));
    }
}

/// The epoch transition under the held state lock (split out so the lock-acquire-failure
/// fail-closed path stays a thin wrapper in [`start_epoch_inner`]).
fn start_epoch_locked(
    cwd: &str,
    session: &str,
    prompt: &str,
    reset_on_user_turn: bool,
    g: &StateGuard,
) {
    let class = classify_turn(prompt);
    // A POISONED gate must re-establish a fresh PENDING baseline — NEVER preserve a possibly stale
    // approval, or `force_block` could be trapped forever behind a trivial continuation turn.
    if !force_block_active(cwd)
        && p1_state_invalidation_supported()
        && preserve_epoch_decision(
            reset_on_user_turn,
            is_approved(cwd),
            scope_is_enforced(cwd),
            class,
        )
    {
        // Preserve the approval; record the pending user-turn for PreToolUse to reconcile.
        // A failed marker write must FAIL CLOSED. `revoke_locked` preserves `approved_plan` for the
        // Stop gate, but if it could not persist (the state still reads as approved), DELETE the
        // state outright so a stale approval cannot remain effective for the new turn (mirrors the
        // fresh-epoch write-failure path below). The next prompt then re-approves from scratch.
        if !set_pending_user_turn_locked(cwd, g, class) {
            revoke_locked(cwd, g, "pending_user_turn_write_failed");
            if is_approved(cwd) {
                let _ = std::fs::remove_file(state_path(cwd));
            }
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
        // Fail closed: if we can't write the fresh PENDING epoch, poison + delete any prior
        // (possibly APPROVED) state so the gate denies until a plan is re-approved, rather than
        // letting a stale approval unlock this new task.
        set_force_block(cwd);
        let _ = std::fs::remove_file(state_path(cwd));
    } else {
        // Fresh known-PENDING baseline persisted under lock → safe to lift any poison.
        clear_force_block(cwd);
    }
}

/// Test-only locking wrapper so unit tests can seed a pending-user-turn marker the same way the
/// production caller ([`start_epoch_inner`], already under the state lock) does it via the
/// `_locked` form. Not compiled in release (the only prod path is the nested `_locked` call).
#[cfg(test)]
fn set_pending_user_turn(cwd: &str, class: TurnClass) -> bool {
    with_state_lock(cwd, |g| set_pending_user_turn_locked(cwd, g, class)).unwrap_or(false)
}

/// Record a pending user-turn marker on the current (preserved) approved epoch. Returns
/// `false` if state can't be read/written (caller fails closed). Stored INSIDE `state.json`
/// (not the separate review `pending` file) as a nullable object so it travels with the
/// authority state and a corrupt/missing value reads as "present" → fail closed.
fn set_pending_user_turn_locked(cwd: &str, _g: &StateGuard, class: TurnClass) -> bool {
    let Some(mut s) = read_state(cwd) else {
        return false;
    };
    // MONOTONIC (Codex-flagged fail-open fix): a later prompt must NEVER DOWNGRADE an
    // existing unconsumed marker. Otherwise a scope-delta turn ("also update the API")
    // could be erased by a following trivial "ok" before any PreToolUse reconciled it, and
    // the next write would run under the stale approval. If an existing marker is at least
    // as severe as the new class, keep it untouched (fail closed); only escalate/initialize.
    let new_sev = marker_severity(class.as_marker());
    if let Some(ev) = s.get("pending_user_turn").filter(|v| !v.is_null()) {
        let existing_sev = ev
            .get("classification")
            .and_then(Value::as_str)
            .map(marker_severity)
            .unwrap_or(u8::MAX); // present-but-malformed → maximally severe → never downgraded
        if existing_sev >= new_sev {
            return true; // keep the existing (>= severity) marker
        }
    }
    set_field(
        &mut s,
        "pending_user_turn",
        json!({ "classification": class.as_marker() }),
    );
    write_state(cwd, &s).is_ok()
}

/// Severity rank of a pending-user-turn classification, for the monotonic no-downgrade
/// rule in [`set_pending_user_turn`]. Higher = more likely to re-gate; an unknown/malformed
/// marker string ranks highest so it can never be lowered.
fn marker_severity(classification: &str) -> u8 {
    match classification {
        "trivial_continue" => 0,
        "unknown_delta" => 1,
        _ => u8::MAX, // explicit_reset / malformed / unrecognized → fail closed (never downgrade)
    }
}

/// Whether an UNCONSUMED pending user-turn marker is present (any non-null value, incl.
/// malformed → treated as present so [`effectively_approved`] fails closed).
fn has_pending_user_turn(cwd: &str) -> bool {
    read_state(cwd)
        .and_then(|s| s.get("pending_user_turn").cloned())
        .map(|v| !v.is_null())
        .unwrap_or(false)
}

/// Drop the pending user-turn marker (best-effort; the marker was consumed by reconcile). Always
/// called under the state lock (begin_review / reconcile paths), so it takes the proof token.
fn clear_pending_user_turn_locked(cwd: &str, _g: &StateGuard) {
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
    // Lock-free pre-check: keep the hot PreToolUse path lock-free in the common case (no marker —
    // a marker only ever exists in resetOnUserTurn=false preserve mode).
    if !has_pending_user_turn(cwd) {
        return None;
    }
    match with_state_lock(cwd, |g| {
        // Re-read UNDER the lock (the marker may have been consumed/changed since the pre-check).
        let class = match read_state(cwd).and_then(|s| s.get("pending_user_turn").cloned()) {
            None | Some(Value::Null) => return None, // already consumed → allow
            Some(v) => v
                .get("classification")
                .and_then(Value::as_str)
                .map(str::to_string),
        };
        // Trivial continuation → preserve approval (just consume the marker).
        if class.as_deref() == Some(TurnClass::TrivialContinue.as_marker()) {
            clear_pending_user_turn_locked(cwd, g);
            return None;
        }
        // Anything else (unknown delta, explicit_reset that slipped through, or a malformed/
        // missing classification) → re-gate. Fail closed.
        revoke_locked(cwd, g, "user_scope_delta");
        clear_pending_user_turn_locked(cwd, g);
        Some(deny_json(BlockReason::UserScopeDelta))
    }) {
        Some(decision) => decision,
        // Lock-acquire failure WITH a marker present: it could be a scope delta we cannot resolve,
        // so do not let the write through — fail closed: DENY (the marker stays for the next turn).
        None => Some(deny_json(BlockReason::UserScopeDelta)),
    }
}

/// v0.32: maintain the pending-user-turn marker for an admitted READ-ONLY discovery command.
/// Unlike [`reconcile_pending_user_turn`] (which a mutator triggers), a read-only command never
/// REVOKES — it writes nothing. But a TRIVIAL continuation marker is still consumed here, so the
/// preserved approval becomes effective again (a lingering marker would keep
/// [`effectively_approved`] false and make `review_checkpoint` refuse). A scope-delta / malformed
/// marker is LEFT untouched so the revoke defers to the first WRITE. No-op when nothing is pending.
fn reconcile_pending_user_turn_read_only(cwd: &str) {
    // Lock-free pre-check (hot path stays lock-free with no marker).
    if !has_pending_user_turn(cwd) {
        return;
    }
    // A read-only command writes NOTHING, so on lock-acquire failure we simply do nothing: leaving
    // the marker keeps `effectively_approved` false until a WRITE reconciles it (fail-closed).
    let _ = with_state_lock(cwd, |g| {
        let class = read_state(cwd)
            .and_then(|s| s.get("pending_user_turn").cloned())
            .and_then(|v| {
                v.get("classification")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if class.as_deref() == Some(TurnClass::TrivialContinue.as_marker()) {
            clear_pending_user_turn_locked(cwd, g);
        }
    });
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
    // v0.32 Unit A: a POISONED gate (a lock-acquire or write failure left `state.json` integrity
    // uncertain) reads as NOT approved on EVERY surface — PreToolUse + the in-process `run` tool
    // (via `blocks_writes`) AND `review_checkpoint` (via `is_effectively_approved`). `is_force_blocked`
    // mirrors enforcement scope (enabled + not bypassed), so a stale sentinel under a disabled gate
    // is inert and AIBRIDGE_PLAN_GATE=0 stays a uniform escape hatch. Lifted only by a fresh epoch.
    if is_force_blocked(cwd) {
        return false;
    }
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
/// with an EMPTY command — which the read-only carve-out never proves read-only
/// (`is_read_only("")` is false), keeping every existing 2-arg caller byte-identical.
pub fn enforce(cwd: &str, tool_name: &str) -> Option<String> {
    enforce_tool(cwd, tool_name, "")
}

/// v0.32: whether the read-only-orientation carve-out is BACKED by an implementation. Now
/// `true` — the pre-approval carve-out is driven by the lexical [`crate::read_only_exec`]
/// classifier (a tight allowlist behind a positive safe-character gate) and is gated by the
/// opt-in `planGate.readOnlyOrientation` config (default off). OWNER-ACCEPTED RESIDUAL: a
/// lexical check can't prove the resolved binary's identity / a clean exec env, and Git
/// read-only porcelain may touch `.git`-metadata (index refresh / optional locks) — never
/// working-tree/source content, never a file-scope bypass; the Stop-gate backstops. Kept as a
/// function (not a literal) so a future hardened-exec layer can refine it.
fn read_only_execution_supported() -> bool {
    true
}

/// Tool-context for the v0.32 Unit 4 write-time scope fence. A tri-state so the fence fails
/// CLOSED on a missing authoritative path while staying INERT for legacy callers:
/// - `Unknown` — no tool-context (the legacy `enforce`/`enforce_tool` surfaces) → the scope
///   fence is skipped entirely (byte-identical pre-Unit-4 behavior).
/// - `Missing` — the authoritative PreToolUse path for a gated WRITE tool that yielded NO
///   usable path (absent/non-string/empty `file_path`|`notebook_path`) → DENY under an active
///   non-empty scope (a write with no checkable target must not slip the fence).
/// - `Path(p)` — a concrete extracted target → canonicalize + scope-check.
///
/// ⚠ SAFETY INVARIANT — `Unknown` MUST NOT be produced for any tool in [`GATED_WRITE_TOOLS`].
/// `scope_fence` early-returns `None` (allow) for `Unknown`, so emitting it for a real write tool
/// with an unparseable payload would DISABLE the fence → FAIL-OPEN. The authoritative caller
/// (`optimizer::write_target_for`) maps every gated write tool to `Path` or `Missing`, never
/// `Unknown`; `optimizer`'s `write_target_for_*` tests + `scope_fence`'s `Missing`-denies tests
/// pin this end-to-end. A future caller change MUST preserve it.
pub enum WriteTarget<'a> {
    Unknown,
    Missing,
    Path(&'a str),
}

/// PreToolUse enforcement (legacy 3-arg surface): forwards with NO tool-context, so the
/// v0.32 scope fence stays inert and every existing 3-arg caller is byte-identical.
pub fn enforce_tool(cwd: &str, tool_name: &str, command: &str) -> Option<String> {
    enforce_tool_scoped(cwd, tool_name, WriteTarget::Unknown, command)
}

/// PreToolUse enforcement with tool-context (v0.32 Unit 4): the same pre-approval gate as
/// before, PLUS a post-approval write-time scope fence ([`scope_fence`]) for path-bearing
/// write tools when a reviewer-approved file scope is in force. `Some(deny_json)` blocks the
/// tool; `None` lets the caller proceed (incl. its own rtk handling for Bash). The command is
/// only consulted on the Bash read-only-orientation path; for every other tool (and an empty
/// command) the pre-approval decision is exactly as before.
pub fn enforce_tool_scoped(
    cwd: &str,
    tool_name: &str,
    target: WriteTarget,
    command: &str,
) -> Option<String> {
    enforce_tool_scoped_with(
        cwd,
        tool_name,
        target,
        command,
        crate::review_mcp::read_only_orientation(),
    )
}

/// [`enforce_tool_scoped`] with the `planGate.readOnlyOrientation` flag INJECTED, so the
/// pre-approval read-only carve-out branch is testable without reading the operator's real
/// `review-mcp.json`. The public wrapper supplies the real config value.
fn enforce_tool_scoped_with(
    cwd: &str,
    tool_name: &str,
    target: WriteTarget,
    command: &str,
    orientation_on: bool,
) -> Option<String> {
    if !is_gated_tool(tool_name) {
        return None;
    }
    // v0.32 read-only discovery carve-out (opt-in via `planGate.readOnlyOrientation`, default
    // off) — checked BEFORE the scope-delta reconcile below. A Bash command the lexical
    // classifier proves read-only WRITES NOTHING, so it must never trigger the user-turn REVOKE:
    // it just inspects the repo (`git status`/`diff`, `find`, `ls`…) without forcing a plan_gate
    // round. The marker is maintained without revoking — a trivial continuation IS consumed (so
    // the preserved approval is effective again), but a scope-delta / malformed marker is LEFT,
    // so the revoke correctly defers to the first WRITE. Owner-accepted residual (see
    // [`crate::read_only_exec`]): no working-tree/source write capability; Stop-gate reviews the diff.
    if tool_name == "Bash" && read_only_carveout(orientation_on, command) {
        // Maintain the user-turn marker WITHOUT revoking: a trivial continuation is consumed (so
        // the preserved approval is effective again — a lingering marker would keep
        // `effectively_approved` false and make `review_checkpoint` refuse), while a scope-delta /
        // malformed marker is LEFT so the revoke defers to the first WRITE.
        if is_enabled(cwd) && !bypassed() {
            reconcile_pending_user_turn_read_only(cwd);
        }
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
        // Approved / gate-off / bypassed. v0.32 Unit 4: apply the write-time scope fence —
        // a no-op unless a non-empty reviewer-approved scope is in force AND this is a
        // path-bearing write tool carrying tool-context.
        if let Some(deny) = scope_fence(cwd, tool_name, target) {
            return Some(deny);
        }
        return None;
    }
    // (The read-only discovery carve-out is checked at the TOP, before the scope-delta
    // reconcile, so a read-only command never eats a one-time deny or consumes the marker.)
    // Count repeated denied writes so the operator can see a wrong-loop (Claude
    // retrying the edit instead of calling plan_gate).
    // Best-effort diagnostic counter under a NON-blocking lock attempt — it must never delay the
    // deny path, and skipping on contention is harmless (a lost count only mis-reports a wrong-loop;
    // locking still prevents a stale read+write from clobbering a concurrent approval).
    let _ = with_state_lock_try(cwd, |_g| {
        if let Some(mut s) = read_state(cwd) {
            let n = s.get("denied_writes").and_then(Value::as_u64).unwrap_or(0) + 1;
            if let Some(o) = s.as_object_mut() {
                o.insert("denied_writes".into(), json!(n));
            }
            let _ = write_state(cwd, &s);
        }
    });
    // Pre-approval block. A POISONED gate (force_block) reports its own reason so the rare degraded
    // state is diagnosable; otherwise, when the read-only carve-out is ENABLED, a Bash command it
    // could not prove read-only denies with the dedicated parser reason code; everything else is
    // the default no-active-approval block.
    let reason = if is_force_blocked(cwd) {
        BlockReason::GateLockUnavailable
    } else if tool_name == "Bash" && read_only_execution_supported() && orientation_on {
        BlockReason::ReadOnlyParserDenial
    } else {
        BlockReason::NoActiveApproval
    };
    Some(deny_json(reason))
}

/// The reviewer-approved file scope currently in force, as a tri-state (v0.32 Unit 4). The
/// distinction is FAIL-CLOSED: ABSENT/EMPTY scope is backward-compatible (non-declaring plans
/// behave exactly as before), but a PRESENT-but-malformed scope must NEVER silently widen
/// authorization — it denies.
enum ScopeState {
    /// No scope was DECLARED (non-declaring/legacy plan): gate off/bypassed, or
    /// `approved_scope_declared` is false AND `approved_allowed_globs` is absent/null/empty →
    /// fence inert (backward-compat — these plans write as before, Stop-gate backstop).
    None,
    /// A scope WAS declared but ZERO globs were approved (`approved_scope_declared` true with an
    /// absent/empty `approved_allowed_globs`) → DENY EVERY write (the reviewer approved no file
    /// scope, so nothing is in scope). This is the fail-closed half of the empty-array ambiguity.
    DenyAll,
    /// `approved_allowed_globs` present but non-array, or an entry is non-string / fails
    /// [`crate::scope::validate_glob`] → DENY (corrupt scope data must not authorize a write).
    Corrupt,
    /// A present, non-empty scope whose every entry is a valid glob.
    Globs(Vec<String>),
}

/// Read the reviewer-approved scope from authority state as a [`ScopeState`]. Only meaningful
/// once an epoch is effectively approved (the fence calls it from the `!blocks_writes` branch).
fn scope_in_force_state(cwd: &str) -> ScopeState {
    // Gate off / bypassed → no scope to enforce.
    if !is_enabled(cwd) || bypassed() {
        return ScopeState::None;
    }
    let Some(s) = read_state(cwd) else {
        return ScopeState::None;
    };
    // Whether the approved plan DECLARED a file scope (an `ALLOWED-GLOBS:` line). This
    // disambiguates an empty `approved_allowed_globs`: declared+empty ⇒ DenyAll (reviewer
    // approved nothing); not-declared+empty ⇒ None (legacy/non-declaring → inert). ABSENT is
    // back-compat (false); a PRESENT-but-non-boolean value is corrupt authority state → deny.
    let declared = match s.get("approved_scope_declared") {
        std::option::Option::None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return ScopeState::Corrupt,
    };
    let empty_state = if declared {
        ScopeState::DenyAll
    } else {
        ScopeState::None
    };
    match s.get("approved_allowed_globs") {
        std::option::Option::None | Some(Value::Null) => empty_state,
        Some(Value::Array(arr)) if arr.is_empty() => empty_state,
        Some(Value::Array(arr)) => {
            let mut globs = Vec::with_capacity(arr.len());
            for item in arr {
                match item.as_str() {
                    Some(g) if crate::scope::validate_glob(g).is_ok() => globs.push(g.to_string()),
                    // a non-string entry OR an invalid glob → corrupt (fail closed).
                    _ => return ScopeState::Corrupt,
                }
            }
            ScopeState::Globs(globs)
        }
        // present but not an array → corrupt.
        Some(_) => ScopeState::Corrupt,
    }
}

/// Whether the plan contains an `ALLOWED-GLOBS:` declaration LABEL at all, regardless of how
/// many (if any) globs follow. A present-but-empty/malformed `ALLOWED-GLOBS:` line still counts
/// as a DECLARED scope, so it fails CLOSED (DenyAll) rather than being mistaken for a
/// non-declaring plan — unlike [`crate::scope::declared_globs`], which drops empty tokens and so
/// cannot distinguish a bare label from no declaration. Label match mirrors `declared_globs`
/// (whole-line trim, case-insensitive label).
fn scope_declared_in_plan(plan: &str) -> bool {
    const LABEL: &str = "allowed-globs:";
    plan.lines().any(|line| {
        line.trim()
            .get(..LABEL.len())
            .map(|head| head.eq_ignore_ascii_case(LABEL))
            .unwrap_or(false)
    })
}

/// v0.32 Unit 4 write-time scope fence. `Some(deny_json)` blocks a path-bearing write that is
/// outside (or cannot be confined to) the reviewer-approved scope; `None` lets it proceed.
/// Bash and `mcp__aibridge__run` are intentionally NOT fenced here (owner-accepted residual:
/// an out-of-scope write through them is caught by the Stop-gate, not at write-time — the
/// hardened-execution carve-out that would let them be fenced safely is a deferred follow-up).
fn scope_fence(cwd: &str, tool_name: &str, target: WriteTarget) -> Option<String> {
    // Only the path-bearing write tools are fenced; Bash/run pass through.
    if !GATED_WRITE_TOOLS.contains(&tool_name) {
        return None;
    }
    // Resolve what to check; `Unknown` (legacy callers) carries no tool-context → fence inert.
    let checkable: Option<&str> = match target {
        WriteTarget::Unknown => return None,
        WriteTarget::Missing => None,
        WriteTarget::Path(p) => Some(p),
    };
    match scope_in_force_state(cwd) {
        ScopeState::None => None, // no active scope → inert (backward-compat)
        // Declared-but-empty scope, or corrupt scope data → deny every write (path or not).
        ScopeState::DenyAll | ScopeState::Corrupt => Some(deny_json(BlockReason::OutOfScopePath)),
        ScopeState::Globs(globs) => match checkable {
            // An authoritative write with no checkable target under an active scope: fail closed.
            None => Some(deny_json(BlockReason::OutOfScopePath)),
            Some(p) => {
                // The confinement anchor: the git toplevel, else the gate's OWN repo-root anchor
                // (`root()` walks to the `.ai-bridge`/`.git` ancestor — the same boundary the gate
                // keys all its state on). Not a fail-open: `canonicalize_under_root` still
                // COMPONENT-confines the target under whatever root it is given, so a write can
                // never escape it. The fallback only matters in a missing-git environment; a
                // declared scope normally exists only inside a real repo. (Deny-instead-of-fallback
                // was considered but would break the legitimate non-git `.ai-bridge` case.)
                let repo_root = crate::git::repo_root(cwd)
                    .unwrap_or_else(|| root(cwd).to_string_lossy().into_owned());
                match crate::path_scope::canonicalize_under_root(&repo_root, p) {
                    Ok(rel) if crate::scope::path_in_allowed(&rel, &globs) => None,
                    // uncanonicalizable / escapes root / out of scope → deny.
                    _ => Some(deny_json(BlockReason::OutOfScopePath)),
                }
            }
        },
    }
}

/// v0.32: whether the read-only discovery carve-out ALLOWS `command` to run pre-approval.
/// Pure (no IO): the carve-out must be BACKED ([`read_only_execution_supported`]), ENABLED
/// (`orientation_on`, from the opt-in config), AND the command must pass the lexical
/// [`crate::read_only_exec::is_read_only`] classifier. Fail-closed: any `false` → still gated.
fn read_only_carveout(orientation_on: bool, command: &str) -> bool {
    read_only_execution_supported()
        && orientation_on
        && crate::read_only_exec::is_read_only(command)
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
    /// A path-bearing write targets a file outside the reviewer-approved ALLOWED-GLOBS scope
    /// (or a path that cannot be confined to the repo root). Wired by v0.32 Unit 4.
    OutOfScopePath,
    /// The gate is POISONED: a state-lock acquire or write FAILED, so `state.json` integrity could
    /// not be guaranteed and the `force_block` sentinel is in force. Self-heals on the next user
    /// turn (a fresh epoch). Wired by v0.32 Unit A.
    GateLockUnavailable,
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
            BlockReason::OutOfScopePath => "out_of_scope_path",
            BlockReason::GateLockUnavailable => "gate_lock_unavailable",
        }
    }

    /// A short human recovery hint appended after the code.
    fn recovery_hint(self) -> &'static str {
        match self {
            BlockReason::NoActiveApproval => {
                "this task has no Codex-approved plan yet. Do NOT retry this tool. First gather \
                 context with Read/Grep/Glob, form a todolist, then call the MCP tool \
                 `mcp__aibridge__plan_gate` with a structured plan (todos, approach, \
                 intended_files, risk_surfaces, test_plan — and, IF the task writes files, an \
                 `ALLOWED-GLOBS:` line declaring them). Revise and call it again until it \
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
            BlockReason::OutOfScopePath => {
                "this write targets a path outside the plan's approved ALLOWED-GLOBS scope (or a \
                 path that cannot be confined to the repo root). Do NOT retry. Re-file the plan \
                 with `mcp__aibridge__plan_gate`, adding this path to the `ALLOWED-GLOBS:` line, \
                 to expand the approved scope."
            }
            BlockReason::GateLockUnavailable => {
                "the plan gate could not safely read/update its state (a lock or write failed), so \
                 it is holding writes closed for safety. Do NOT retry the write. Send a new message \
                 (which starts a fresh task epoch and clears this), or relaunch Claude Code with \
                 AIBRIDGE_PLAN_GATE=0 to bypass the gate for this session."
            }
        }
    }
}

/// Whether the gate is POISONED (a state lock-acquire or write failed → `force_block`) AND actively
/// enforcing for this repo. Mirrors enforcement scope exactly — `is_enabled && !bypassed` — so a
/// STALE sentinel left after the gate is disabled/uninstalled (the `enabled` marker removed) is
/// inert, and `AIBRIDGE_PLAN_GATE=0` bypasses it. The MCP tools use this to give the RIGHT recovery
/// (a fresh epoch clears it; retrying `plan_gate` cannot, since `record`/`record_resume` refuse to
/// approve while poisoned). The single source of truth for "poisoned" across the gate.
pub fn is_force_blocked(cwd: &str) -> bool {
    is_enabled(cwd) && !bypassed() && force_block_active(cwd)
}

/// General poison-recovery prose shared by the MCP tools (`run`, `plan_gate` needs-info, and
/// `review_checkpoint`) when [`is_force_blocked`]. Calling `plan_gate` again will NOT lift it.
pub(crate) fn force_blocked_message() -> &'static str {
    "the plan gate is holding writes closed after a state lock/write failure — calling plan_gate \
     again will NOT lift this. Send a new message (a fresh task epoch clears it), or relaunch \
     Claude Code with AIBRIDGE_PLAN_GATE=0 to bypass the gate for this session"
}

/// The `run` tool's pre-execution refusal message, poison-aware. `None` → `run` may proceed
/// (subject to the high-risk-delta check). `Some` → refuse with this message.
pub(crate) fn run_tool_blocked_message(cwd: &str) -> Option<String> {
    if is_force_blocked(cwd) {
        Some(format!(
            "AI Bridge: `run` blocked — {}.",
            force_blocked_message()
        ))
    } else if blocks_writes(cwd) {
        Some(
            "AI Bridge: `run` is blocked by the plan gate — this task has no approved plan yet. \
             Call `plan_gate` with your plan and retry after <AI-BRIDGE-APPROVE/>."
                .to_string(),
        )
    } else {
        None
    }
}

/// The `plan_gate` tool's `NeedsInfo` presentation for the NON-poison case (a poisoned gate is
/// handled upstream by [`poisoned_outcome_message`], so this never needs to be poison-aware).
pub(crate) fn needs_info_message(findings: &str) -> String {
    format!(
        "AI Bridge: Codex needs more information to judge the plan (or is blocked). Provide what it \
         asks or check with the user, then call `plan_gate` again:\n\n{findings}"
    )
}

/// The `plan_gate` tool's EARLY refusal: when the gate is ALREADY poisoned, approval is impossible
/// until a fresh epoch clears it, so the MCP handler must short-circuit BEFORE any review work
/// (begin_review's pending marker + the minutes-long Codex round). `Some(message)` → return it now;
/// `None` → proceed with the normal review.
pub(crate) fn plan_gate_early_refusal(cwd: &str) -> Option<String> {
    if is_force_blocked(cwd) {
        Some(poisoned_outcome_message(&Outcome::NeedsInfo(String::new())))
    } else {
        None
    }
}

/// The SINGLE poison-aware presentation for ANY `plan_gate` outcome (Revise / Stuck / NeedsInfo)
/// when [`is_force_blocked`]. No outcome may tell the agent to "revise and call plan_gate again" —
/// retrying cannot approve until a fresh epoch clears the poison (`record`/`record_resume` refuse).
/// Reviewer findings (if any) are kept as context for the re-review after a fresh task starts.
pub(crate) fn poisoned_outcome_message(outcome: &Outcome) -> String {
    let findings = match outcome {
        Outcome::Revise(f) | Outcome::Stuck(f) | Outcome::NeedsInfo(f) => f.trim(),
        Outcome::Approved => "", // unreachable under poison (record refuses), handled defensively
    };
    let recovery = force_blocked_message();
    if findings.is_empty() {
        format!("AI Bridge: {recovery}.")
    } else {
        format!(
            "AI Bridge: {recovery}.\n\nReviewer notes (address these after starting a fresh \
             task):\n\n{findings}"
        )
    }
}

/// The `review_checkpoint` no-approval refusal message, poison-aware.
pub(crate) fn checkpoint_refused_message(cwd: &str) -> String {
    if is_force_blocked(cwd) {
        format!(
            "AI Bridge: `review_checkpoint` refused — {}.\nFrontier unchanged.",
            force_blocked_message()
        )
    } else {
        "AI Bridge: `review_checkpoint` refused: no currently approved plan_gate scope; call \
         plan_gate for this PR/task first.\nFrontier unchanged."
            .to_string()
    }
}

/// Recovery prose when `begin_review` could not persist the in-flight review marker and POISONED
/// the gate (a state lock-acquire failure). Retrying `plan_gate` will NOT help — `record` refuses
/// to approve while poisoned — so the recovery mirrors [`BlockReason::GateLockUnavailable`]: a fresh
/// task epoch (a new user message) clears the poison, or bypass with `AIBRIDGE_PLAN_GATE=0`.
pub(crate) fn marker_write_failed_message() -> &'static str {
    "AI Bridge: could not persist the in-flight review marker (a state lock/write failed), so the \
     plan gate is holding writes closed for safety. Retrying plan_gate will NOT lift this. Send a \
     new message (a fresh task epoch clears it), or relaunch Claude Code with AIBRIDGE_PLAN_GATE=0 \
     to bypass the gate for this session."
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
    ]) || (has_cmd("stripe")
        && has("capture")
        && has_any(&["charges", "charge", "payment_intents", "payment_intent"]))
        || (has_cmd("stripe")
            && has("checkout")
            && has_any(&["sessions", "session"])
            && has("create"))
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
        || (has_cmd("stripe")
            && has_any(&["webhook_endpoints", "webhook_endpoint"])
            && has("create"))
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
    // v0.32 scoped-approval: authorizes a RECURSIVE (Broad) scope glob in `approved_scope`.
    // Not a command class — consumed by `crate::scope::approved_scope` (broad_scope_granted),
    // not by the command-risk delta check.
    "broad-scope",
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

/// Max length (chars) of an owner review-policy before it is ignored — fail-closed:
/// an oversized policy is NEVER partially applied (Codex topic `gate-owner-review-policy`).
const REVIEW_POLICY_MAX_CHARS: usize = 4000;

/// Resolved state of `<root>/.ai-bridge/review-policy.md` for a Stop/checkpoint review.
/// The owner records NARROW accepted product/sequencing decisions there; it is fed to
/// the binding reviewer as untrusted DATA that may only DECLINE a finding solely based
/// on an accepted item — never waive correctness/safety/security.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewPolicy {
    /// No file, or whitespace-only → inject nothing, no diagnostic.
    Absent,
    /// Present but NOT honored (oversized, changed since approval, or no live approval)
    /// → inject nothing AND surface this reason as a diagnostic.
    Ignored(&'static str),
    /// Pinned-stable, in-bounds policy content (the exact text that was hashed + injected).
    Active(String),
}

impl ReviewPolicy {
    /// The policy text to inject into the review prompt — Some only for `Active`
    /// (Absent/Ignored inject nothing). Adapter to `gate::prompt_with_scope`.
    pub fn active_text(&self) -> Option<&str> {
        match self {
            ReviewPolicy::Active(s) => Some(s.as_str()),
            ReviewPolicy::Absent | ReviewPolicy::Ignored(_) => None,
        }
    }

    /// A fingerprint of the policy CONTENT that will be injected (or a fixed sentinel
    /// when none). Folded into the Stop fast-path key (`gate::mix_fp`) so any change in
    /// the active policy forces a fresh review and invalidates a prior allow / cached
    /// block. Absent and Ignored share the sentinel — both inject nothing, so they
    /// yield the same prompt and must yield the same fp.
    pub fn fp(&self) -> u64 {
        crate::gate::hash_str(self.active_text().unwrap_or("\u{0}no-review-policy"))
    }
}

/// Strip well-formed `<!-- ... -->` HTML comments (incl. multi-line). An UNBALANCED
/// `<!--` (no closing) is left intact — conservative: keeping content can only make a
/// policy read as PRESENT/active, never hide a real entry, so stripping is monotonic
/// toward safety. Lets `init` ship an all-comment, INERT review-policy template.
fn strip_html_comments(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                // No closing marker → keep the remainder verbatim (don't strip).
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Canonical form of raw policy content: the SAME string that is BOTH hashed and
/// injected (so a pin can never mismatch the injected text). HTML comments are
/// stripped first (so an all-comment scaffold is inert), then trimmed; empty → None.
fn normalize_policy(raw: &str) -> Option<String> {
    let stripped = strip_html_comments(raw);
    let t = stripped.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// SHA-256 (hex) of the canonical policy — a stable, platform-independent identity
/// (NOT `DefaultHasher`; Codex requirement for a persisted pin).
fn policy_hash(canonical: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(canonical.as_bytes());
    format!("{:x}", h.finalize())
}

/// Read the raw owner review-policy file, or None if absent/unreadable (incl. invalid
/// UTF-8) → treated as Absent (safe: inject nothing). Owner-curated + optional; lives
/// under the git-excluded `.ai-bridge/` so it is per-machine, never team-committed.
fn read_review_policy_raw(cwd: &str) -> Option<String> {
    std::fs::read_to_string(root(cwd).join(".ai-bridge").join("review-policy.md")).ok()
}

/// The hash to PIN at plan approval: SHA-256 of the current active, in-bounds policy,
/// else Null (absent / whitespace-only / oversized → nothing pinned → fail-closed at
/// review). Storing only the hash never leaks policy content into state.
fn review_policy_pin(cwd: &str) -> Value {
    match read_review_policy_raw(cwd)
        .as_deref()
        .and_then(normalize_policy)
    {
        Some(c) if c.chars().count() <= REVIEW_POLICY_MAX_CHARS => json!(policy_hash(&c)),
        _ => Value::Null,
    }
}

/// Resolve the owner review-policy for a Stop/checkpoint review of the CURRENT task.
/// Fail-CLOSED: a policy is honored ONLY when the gate is enabled, not bypassed, the
/// current epoch is effectively approved, AND the file is byte-stable since that
/// approval (its canonical content hashes to the pin stored at approval) — so a
/// post-approval edit (e.g. a Bash write into the excluded `.ai-bridge/` dir, or a
/// receipt resume that re-Nulls the pin) is ignored.
pub fn active_review_policy(cwd: &str) -> ReviewPolicy {
    let canonical = match read_review_policy_raw(cwd)
        .as_deref()
        .and_then(normalize_policy)
    {
        None => return ReviewPolicy::Absent,
        Some(c) => c,
    };
    if canonical.chars().count() > REVIEW_POLICY_MAX_CHARS {
        return ReviewPolicy::Ignored("policy too long");
    }
    // Honor ONLY under a live, effective approval (positive form of `blocks_writes`):
    // never from a lingering pin while the gate is off/bypassed, the approval was
    // revoked, or a different plan is in review.
    if !is_enabled(cwd) || bypassed() || !effectively_approved(cwd) {
        return ReviewPolicy::Ignored("no effective plan approval");
    }
    let state = read_state(cwd);
    let pinned = state
        .as_ref()
        .and_then(|s| s.get("review_policy_hash"))
        .and_then(Value::as_str);
    match pinned {
        Some(h) if h == policy_hash(&canonical) => ReviewPolicy::Active(canonical),
        Some(_) => ReviewPolicy::Ignored("policy changed since approval"),
        None => ReviewPolicy::Ignored("policy not pinned at approval"),
    }
}

/// True when an in-bounds owner review-policy file is present on disk (regardless of
/// whether it is currently pinned/effective). The plan-gate receipt fast-path is
/// SKIPPED in this case so a full `record()` approval runs and PINS the policy —
/// otherwise the no-review resume (which Nulls the pin) would leave the policy
/// inactive and the owner could never activate it by re-approving an identical plan.
pub fn review_policy_present(cwd: &str) -> bool {
    matches!(
        read_review_policy_raw(cwd).as_deref().and_then(normalize_policy),
        Some(c) if c.chars().count() <= REVIEW_POLICY_MAX_CHARS
    )
}

/// Revoke the current epoch's approval (re-arm the gate). `reason` is recorded for
/// `doctor`/diagnostics. Best-effort: a missing state file means nothing to revoke.
pub fn revoke(cwd: &str, reason: &str) {
    if with_state_lock(cwd, |g| revoke_locked(cwd, g, reason)).is_none() {
        // Lock-acquire failure on a de-authorization: a stale approval must NOT survive. Poison +
        // best-effort remove state so the central predicate denies until a fresh epoch.
        set_force_block(cwd);
        let _ = std::fs::remove_file(state_path(cwd));
    }
}

/// Revoke under the held state lock. Write-FAILURE is fail-closed too: if the revoked state can't
/// be persisted, a stale approval could remain effective → poison + best-effort remove state.
fn revoke_locked(cwd: &str, _g: &StateGuard, reason: &str) {
    if let Some(mut s) = read_state(cwd) {
        set_field(&mut s, "approved", json!(false));
        if let Some(o) = s.as_object_mut() {
            o.remove("approved_epoch");
        }
        set_field(&mut s, "status", json!("pending"));
        set_field(&mut s, "revoked_reason", json!(reason));
        set_field(&mut s, "review_policy_hash", Value::Null);
        if write_state(cwd, &s).is_err() {
            set_force_block(cwd);
            let _ = std::fs::remove_file(state_path(cwd));
        }
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
/// What a pending user-turn marker was when [`begin_review`] consumed it. The caller uses
/// this to gate the receipt fast-path: a NON-trivial pending turn at review start was NOT
/// reviewed, so the fast-path (which runs no fresh review) must be skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumedTurn {
    /// No pending marker at review start.
    None,
    /// A trivial continuation (safe to fast-path / resume).
    Trivial,
    /// An unknown-delta or malformed marker — a turn that must get a FULL review.
    NonTrivial,
}

/// Outcome of starting a review: `Ready` carries the consumed user-turn class; `MarkerWriteFailed`
/// means the in-flight review marker could not be persisted, so the caller MUST fail closed (the
/// stale/superseded-plan guard relies on that marker existing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStart {
    Ready(ConsumedTurn),
    MarkerWriteFailed,
}

pub fn begin_review(cwd: &str, plan: &str) -> ReviewStart {
    if !is_enabled(cwd) {
        return ReviewStart::Ready(ConsumedTurn::None);
    }
    // The whole critical section runs under ONE lock so the in-flight `(epoch, plan_hash)` marker
    // and the `pending_user_turn` snapshot are consistent and cannot race a concurrent start_epoch.
    match with_state_lock(cwd, |g| {
        let pending_written = write_pending(cwd, &current_epoch(cwd), hash_str(plan));
        // v0.31 P1: submitting a plan for review IS the agent's response to the current user
        // turn, so consume any pending user-turn marker here ("at review start") and REPORT what
        // it was. A marker that re-appears AFTER this (a new prompt during the minutes-long review)
        // is detected by `record`, which refuses to approve an unreviewed turn; and a NON-trivial
        // marker consumed here tells the caller to SKIP the receipt fast-path (it runs no review).
        // No-op in default reset mode (no marker is ever written).
        let consumed = match read_state(cwd)
            .and_then(|s| s.get("pending_user_turn").filter(|v| !v.is_null()).cloned())
        {
            None => ConsumedTurn::None,
            Some(v) => {
                if v.get("classification").and_then(Value::as_str)
                    == Some(TurnClass::TrivialContinue.as_marker())
                {
                    ConsumedTurn::Trivial
                } else {
                    ConsumedTurn::NonTrivial // unknown_delta / malformed → fail closed
                }
            }
        };
        // Fail closed: only consume the user-turn marker after the in-flight review marker is
        // confirmed on disk; otherwise revoke and leave the turn pending so the next gated tool
        // re-gates (a lost review marker must not also silently drop the user turn).
        if !pending_written {
            revoke_locked(cwd, g, "begin_review_pending_write_failed");
            return ReviewStart::MarkerWriteFailed;
        }
        clear_pending_user_turn_locked(cwd, g);
        ReviewStart::Ready(consumed)
    }) {
        Some(outcome) => outcome,
        // Lock-acquire failure: the in-flight review marker cannot be recorded → fail closed.
        // Poison AND best-effort remove state (mirroring the other stale-approval paths) so a stale
        // approval can't stay effective with no valid `pending` marker even if the sentinel write fails.
        None => {
            set_force_block(cwd);
            let _ = std::fs::remove_file(state_path(cwd));
            ReviewStart::MarkerWriteFailed
        }
    }
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
    match with_state_lock(cwd, |g| {
        record_locked(cwd, g, expected_epoch, plan, verdict, findings)
    }) {
        Some(o) => o,
        None => record_lock_failed(cwd, verdict, findings),
    }
}

/// Lock-acquire failure for `record`, branched BY VERDICT (Codex): an `Approve` simply cannot be
/// recorded (NeedsInfo, no poison — the prior PENDING/same-epoch state is already fail-closed), but
/// EVERY non-approve verdict must NOT leave a prior approval effective, so poison + best-effort
/// remove state, then return a non-approve outcome.
fn record_lock_failed(cwd: &str, verdict: &crate::gate::Verdict, findings: &str) -> Outcome {
    match verdict {
        crate::gate::Verdict::Approve => Outcome::NeedsInfo(
            "AI Bridge: could not acquire the plan-gate lock to record approval — retry, or restart \
             Claude and re-send the task."
                .to_string(),
        ),
        crate::gate::Verdict::RequestChanges => {
            set_force_block(cwd);
            let _ = std::fs::remove_file(state_path(cwd));
            Outcome::Revise(findings.to_string())
        }
        crate::gate::Verdict::Blocked | crate::gate::Verdict::Unparseable => {
            set_force_block(cwd);
            let _ = std::fs::remove_file(state_path(cwd));
            Outcome::NeedsInfo(findings.to_string())
        }
    }
}

fn record_locked(
    cwd: &str,
    _g: &StateGuard,
    expected_epoch: &str,
    plan: &str,
    verdict: &crate::gate::Verdict,
    findings: &str,
) -> Outcome {
    // A POISONED gate (force_block) must not MINT an approval the central predicate would render
    // ineffective — that would emit a misleading <AI-BRIDGE-APPROVE/> ("writes unlocked") while
    // every write is still denied. Refuse until a fresh epoch (start_epoch) re-establishes a known
    // baseline and clears the poison. `is_force_blocked` honors enable+bypass scope (a disabled
    // gate's manual `record` is unaffected by a stale sentinel). Empty findings → the MCP layer's
    // `poisoned_outcome_message` short-circuit supplies the single shared recovery prose.
    if is_force_blocked(cwd) {
        return Outcome::NeedsInfo(String::new());
    }
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
            // v0.31 P1: a pending user-turn marker present NOW appeared AFTER begin_review
            // consumed the prior one — i.e. a NEW user message arrived during the (minutes-long)
            // review and was NOT reviewed. Refuse so the agent re-submits a plan accounting for it
            // (rather than unlocking writes for an unreviewed turn). No-op in default reset mode.
            if s.get("pending_user_turn")
                .filter(|v| !v.is_null())
                .is_some()
            {
                return Outcome::NeedsInfo(
                    "AI Bridge: a new user message arrived while this plan was under review — \
                     re-submit the current plan to plan_gate before proceeding."
                        .to_string(),
                );
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
            // v0.32 scoped-approval: derive + store the reviewer-approved file scope from the
            // SAME findings just approved — Claude's `ALLOWED-GLOBS:` ∩ the reviewer's
            // `SCOPE-APPROVED:` echoes ∩ the breadth policy. The AUTHORITATIVE approve transition
            // owns it (unit 4's write-time check reads it). EMPTY until the reviewer prompt emits
            // SCOPE-APPROVED markers (unit 3B-ii) → inert today.
            let broad = parse_risk_approved(findings).contains(&"broad-scope");
            let approved_globs = crate::scope::approved_scope(plan, findings, broad);
            set_field(&mut s, "approved_allowed_globs", json!(approved_globs));
            // Record WHETHER the plan declared a file scope (the PRESENCE of an `ALLOWED-GLOBS:`
            // label — NOT how many globs the reviewer ultimately approved, and NOT whether any
            // parsed: a bare/empty `ALLOWED-GLOBS:` still counts). A declared-but-empty scope must
            // DENY every write (the fence reads this to disambiguate empty from non-declaring).
            set_field(
                &mut s,
                "approved_scope_declared",
                json!(scope_declared_in_plan(plan)),
            );
            set_field(&mut s, "approved_plan", json!(cap_plan(plan)));
            // Pin the owner review-policy (if any) at THIS approval so a mid-task edit
            // to the excluded `.ai-bridge/review-policy.md` can't mint an invisible waiver.
            set_field(&mut s, "review_policy_hash", review_policy_pin(cwd));
            set_field(&mut s, "status", json!("approved"));
            set_field(&mut s, "revoked_reason", Value::Null);
            set_field(&mut s, "same_findings", json!(0));
            // v0.32 Unit B: a MONOTONIC per-approval generation (under the state lock → atomic) so an
            // operation lease can bind to THIS approval instance; a revoke + re-approve bumps it,
            // invalidating a stale lease regardless of timing. Overflow (corrupt state) → fail closed.
            if !bump_approved_generation(&mut s) {
                // Corrupt/overflowed generation → fail closed AND neutralize any PRIOR (corrupt)
                // approval so it can't stay effective: poison + best-effort remove state.
                set_force_block(cwd);
                let _ = std::fs::remove_file(state_path(cwd));
                return Outcome::NeedsInfo(
                    "AI Bridge: the plan-gate state is corrupt (approval-generation). Send a new \
                     message to start a fresh task."
                        .to_string(),
                );
            }
            if write_state(cwd, &s).is_err() {
                // The approval did NOT persist → do not claim it. The prior PENDING/same-epoch
                // state remains effective-or-not exactly as before this call (fail-closed); no
                // poison needed (we never widened authorization).
                return Outcome::NeedsInfo(
                    "AI Bridge: approval could not be persisted (state write failed) — retry."
                        .to_string(),
                );
            }
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
            // A non-APPROVE drops any pinned review-policy (defense-in-depth; active_review_policy
            // already requires a live approval).
            set_field(&mut s, "review_policy_hash", Value::Null);
            let fh = hash_str(findings);
            let prev = s.get("last_findings_hash").and_then(Value::as_u64);
            let same = if prev == Some(fh) {
                s.get("same_findings").and_then(Value::as_u64).unwrap_or(0) + 1
            } else {
                1
            };
            set_field(&mut s, "last_findings_hash", json!(fh));
            set_field(&mut s, "same_findings", json!(same));
            if write_state(cwd, &s).is_err() {
                // A non-APPROVE revocation whose write FAILED must not leave a prior approval
                // effective (e.g. re-reviewing an already-approved plan) → poison + remove state.
                set_force_block(cwd);
                let _ = std::fs::remove_file(state_path(cwd));
            }
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
            set_field(&mut s, "review_policy_hash", Value::Null);
            if write_state(cwd, &s).is_err() {
                // A non-decision whose write FAILED must not leave writes open → poison + remove.
                set_force_block(cwd);
                let _ = std::fs::remove_file(state_path(cwd));
            }
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
pub fn record_resume(
    cwd: &str,
    expected_epoch: &str,
    plan: &str,
    grants: &[RiskGrant],
    allowed_globs: &[String],
) -> bool {
    // Lock-acquire failure → return false so the caller runs a full review (fail-closed). The
    // approve write inside already propagates its own failure via `is_ok()`.
    with_state_lock(cwd, |g| {
        record_resume_locked(cwd, g, expected_epoch, plan, grants, allowed_globs)
    })
    .unwrap_or(false)
}

fn record_resume_locked(
    cwd: &str,
    _g: &StateGuard,
    expected_epoch: &str,
    plan: &str,
    grants: &[RiskGrant],
    allowed_globs: &[String],
) -> bool {
    // A poisoned gate must not fast-path-approve either (see `record_locked`): the resume would be
    // rendered ineffective by `effectively_approved` yet still report success. Fail closed → the
    // caller runs a full review, and the poison clears on the next fresh epoch.
    if is_force_blocked(cwd) {
        return false;
    }
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
    // v0.31 P1: a pending user-turn marker is a prompt the PreToolUse authority has NOT
    // reconciled, and a receipt fast-path has NOT reviewed it. Refuse the fast-path for any
    // NON-TRIVIAL (or malformed) marker so a resume cannot bypass a scope delta; a trivial
    // continuation is cleared and the resume proceeds (just like a real `record` approve).
    if let Some(v) = s.get("pending_user_turn").filter(|v| !v.is_null()).cloned() {
        let trivial = v.get("classification").and_then(Value::as_str)
            == Some(TurnClass::TrivialContinue.as_marker());
        if !trivial {
            return false;
        }
        if let Some(o) = s.as_object_mut() {
            o.remove("pending_user_turn");
        }
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
    // v0.32 scoped-approval: restore the receipt's reviewer-approved scope onto state, so a
    // resume leaves the SAME authoritative scope a fresh `record` approve would.
    set_field(&mut s, "approved_allowed_globs", json!(allowed_globs));
    // Derive the declared-scope flag from the PLAN (exactly like `record`, via label PRESENCE),
    // NOT from the restored globs: a plan can declare `ALLOWED-GLOBS:` yet have ZERO globs
    // approved (e.g. a recursive glob dropped for a missing broad-scope grant), and a receipt is
    // saved even then. Inferring from `allowed_globs.is_empty()` would lose that distinction on
    // resume and reopen the fence.
    set_field(
        &mut s,
        "approved_scope_declared",
        json!(scope_declared_in_plan(plan)),
    );
    set_field(&mut s, "approved_plan", json!(cap_plan(plan)));
    // A receipt resume runs NO fresh review, so it must NOT (re-)pin a possibly
    // mid-task-edited policy: store Null → the owner review-policy stays INACTIVE after
    // a resume until a fresh full approval re-pins it (fail-closed). Persisting the
    // original hash through the receipt is a documented follow-up.
    set_field(&mut s, "review_policy_hash", Value::Null);
    set_field(&mut s, "status", json!("approved"));
    set_field(&mut s, "revoked_reason", Value::Null);
    set_field(&mut s, "same_findings", json!(0));
    // v0.32 Unit B: bump the monotonic per-approval generation HERE too — `record_resume` is a
    // separate approval writer that does NOT touch `rounds`, so without this a revoke + receipt-resume
    // re-approve could revive a stale lease. Overflow (corrupt state) → fail closed (no approval).
    if !bump_approved_generation(&mut s) {
        // Corrupt/overflowed generation → fail closed AND neutralize any prior corrupt approval.
        set_force_block(cwd);
        let _ = std::fs::remove_file(state_path(cwd));
        return false;
    }
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
         2b. FILE SCOPE (optional): if the plan declares a machine-readable `ALLOWED-GLOBS:` \
         line (the files it intends to write), ECHO each glob you APPROVE as its OWN standalone \
         `SCOPE-APPROVED: <glob>` line (one glob per line, verbatim). A glob you do NOT echo is \
         treated as out-of-scope. Breadth policy: echo an exact-file glob (`src/foo.rs`) or a \
         single-directory-level glob (`dir/*`, `*.rs`) normally; for a RECURSIVE glob — any \
         NON-leading `**`, e.g. `dir/**` or `src/**/*.rs` — you must ALSO add a \
         `RISK-APPROVED: broad-scope` line (a scope-BREADTH grant, NOT a command class) to \
         authorize that breadth, and only when the plan genuinely needs a whole subtree. NEVER \
         echo a repo-wide or unsupported glob — bare `*`, bare `**`, `**/*`, any LEADING `**` \
         (e.g. `**/*.rs`), `.`, or any rooted/`..`/drive/`?`/`[`/`{{` form — those are always \
         dropped.\n\
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
    fn prompt_includes_file_scope_protocol() {
        let p = prompt("ALLOWED-GLOBS: src/a.rs");
        // The reviewer is told to echo each approved glob.
        assert!(
            p.contains("SCOPE-APPROVED"),
            "prompt must instruct SCOPE-APPROVED echoes"
        );
        // `broad-scope` is framed as a scope-breadth grant (FILE SCOPE section), and the
        // command-class RISK-APPROVED list must NOT be polluted with it.
        assert!(p.contains("broad-scope"));
        assert!(p.contains("scope-BREADTH grant"));
        let cmd_line = p
            .lines()
            .find(|l| l.contains("comma-separated subset of"))
            .expect("the command RISK-APPROVED list line");
        assert!(
            !cmd_line.contains("broad-scope"),
            "broad-scope must NOT appear in the command-class list"
        );
        // Repo-wide policy text covers leading `**` / `**/*`.
        assert!(p.contains("**/*"));
        assert!(p.contains("LEADING"));
        // The brace example renders literally (guards the `{{` format-string escaping).
        assert!(
            p.contains("`{`"),
            "the brace glob example must render as a literal `{{`"
        );
    }

    #[test]
    fn no_active_approval_hint_mentions_allowed_globs() {
        assert!(BlockReason::NoActiveApproval
            .recovery_hint()
            .contains("ALLOWED-GLOBS"));
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
            &[RiskGrant::standard("remote-publish")],
            &[],
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
    fn record_stores_approved_allowed_globs_intersection() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        // The plan DECLARES two globs; the reviewer ECHOES only one → the intersection is stored.
        let plan = "do work\nALLOWED-GLOBS: src/a.rs, src/b.rs";
        begin_review(&cwd, plan);
        let findings = "ok\nSCOPE-APPROVED: src/a.rs";
        assert!(matches!(
            record(&cwd, &epoch, plan, &crate::gate::Verdict::Approve, findings),
            Outcome::Approved
        ));
        assert_eq!(
            read_state(&cwd).unwrap()["approved_allowed_globs"],
            json!(["src/a.rs"])
        );
    }

    #[test]
    fn record_stores_empty_scope_without_markers() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        begin_review(&cwd, "plain plan");
        assert!(matches!(
            record(
                &cwd,
                &epoch,
                "plain plan",
                &crate::gate::Verdict::Approve,
                "ok"
            ),
            Outcome::Approved
        ));
        // No ALLOWED-GLOBS / SCOPE-APPROVED markers → empty stored scope (inert, fail-safe).
        assert_eq!(
            read_state(&cwd).unwrap()["approved_allowed_globs"],
            json!([])
        );
    }

    #[test]
    fn record_stores_broad_scope_only_with_grant() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        let plan = "wide work\nALLOWED-GLOBS: src/**";
        begin_review(&cwd, plan);
        // Declared + echoed recursive glob, WITH the reviewer's broad-scope grant → kept.
        let findings = "ok\nSCOPE-APPROVED: src/**\nRISK-APPROVED: broad-scope";
        assert!(matches!(
            record(&cwd, &epoch, plan, &crate::gate::Verdict::Approve, findings),
            Outcome::Approved
        ));
        assert_eq!(
            read_state(&cwd).unwrap()["approved_allowed_globs"],
            json!(["src/**"])
        );
    }

    #[test]
    fn record_drops_broad_scope_without_grant() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        let plan = "wide work\nALLOWED-GLOBS: src/**";
        begin_review(&cwd, plan);
        // Same recursive glob declared + echoed but NO broad-scope grant → dropped (empty).
        let findings = "ok\nSCOPE-APPROVED: src/**";
        assert!(matches!(
            record(&cwd, &epoch, plan, &crate::gate::Verdict::Approve, findings),
            Outcome::Approved
        ));
        assert_eq!(
            read_state(&cwd).unwrap()["approved_allowed_globs"],
            json!([])
        );
    }

    #[test]
    fn record_resume_stores_passed_allowed_globs() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        begin_review(&cwd, "plan");
        assert!(record_resume(
            &cwd,
            &epoch,
            "plan",
            &[],
            &["src/x.rs".to_string()]
        ));
        assert_eq!(
            read_state(&cwd).unwrap()["approved_allowed_globs"],
            json!(["src/x.rs"])
        );
    }

    #[test]
    fn record_resume_keeps_declared_but_empty_scope_closed() {
        // A receipt for a DECLARED-but-empty scope (e.g. `src/**` with no broad-scope grant →
        // [] approved) is saved on approval. A resume must NOT reopen the fence: the declared
        // flag is derived from the PLAN, so an empty restored scope stays DenyAll, not inert.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        begin_review(&cwd, "ALLOWED-GLOBS: src/**");
        assert!(record_resume(
            &cwd,
            &epoch,
            "ALLOWED-GLOBS: src/**",
            &[],
            &[]
        ));
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "")
            .expect("resumed declared-but-empty scope must still deny");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
    }

    #[test]
    fn record_resume_refuses_stale_epoch() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task one");
        let stale = current_epoch(&cwd);
        start_epoch(&cwd, "sess", "task two"); // a new task → new epoch
                                               // A resume bound to the OLD epoch must not unlock the new task.
        assert!(!record_resume(&cwd, &stale, "plan A", &[], &[]));
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
    fn read_only_carveout_is_opt_in_and_obeys_injected_orientation() {
        // v0.32: the carve-out is opt-in. With orientation OFF a read-only Bash command is
        // still denied pre-approval (default code). With orientation ON the classifier admits
        // a read-only command but still denies a mutating one (parser code). All exercised via
        // the config-injected path so no test reads the operator's real review-mcp.json.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let reason_of = |deny: &str| -> String {
            let v: Value = serde_json::from_str(deny).expect("deny is valid json");
            v.pointer("/hookSpecificOutput/permissionDecisionReason")
                .and_then(Value::as_str)
                .unwrap()
                .to_string()
        };
        // OFF: `git status` denied with the default no_active_approval code.
        let deny =
            enforce_tool_scoped_with(&cwd, "Bash", WriteTarget::Unknown, "git status", false)
                .expect("read-only Bash denied pre-approval when carve-out off");
        assert!(reason_of(&deny).contains("PLAN_GATE_REQUIRED: no_active_approval"));
        // ON: a proven read-only command is allowed to RUN pre-approval.
        assert!(
            enforce_tool_scoped_with(&cwd, "Bash", WriteTarget::Unknown, "git status", true)
                .is_none(),
            "read-only `git status` should run pre-approval when carve-out on"
        );
        // ON: a mutating command is still denied, now with the parser reason code.
        let deny = enforce_tool_scoped_with(&cwd, "Bash", WriteTarget::Unknown, "rm -rf x", true)
            .expect("mutating Bash still denied when carve-out on");
        assert!(reason_of(&deny).contains("PLAN_GATE_REQUIRED: read_only_parser_denial"));
        // Writes never carve out, regardless of orientation.
        assert!(
            enforce_tool_scoped_with(&cwd, "Write", WriteTarget::Unknown, "", true).is_some(),
            "Write is never admitted by the read-only carve-out"
        );
        // The legacy enforce() wrapper passes an EMPTY command → never read-only → stays denied.
        assert!(enforce(&cwd, "Bash").is_some());
        assert_eq!(enforce(&cwd, "Bash"), enforce_tool(&cwd, "Bash", ""));
        // The carve-out is now backed; the pure decision helper gates on orientation + classifier.
        assert!(read_only_execution_supported());
        assert!(read_only_carveout(true, "git status"));
        assert!(!read_only_carveout(false, "git status"));
        assert!(!read_only_carveout(true, "rm -rf x"));
    }

    #[test]
    fn read_only_command_bypasses_the_scope_delta_revoke() {
        // The carve-out is checked BEFORE the user-turn reconcile, so a proven read-only command
        // arriving after a scope-delta turn (a) is admitted instead of eating a one-time
        // `user_scope_delta` deny, and (b) does NOT consume the pending marker — the revoke
        // correctly defers to the first WRITE.
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/*.rs"]));
        assert!(set_pending_user_turn(&cwd, TurnClass::UnknownDelta));
        // ON: read-only `git status` runs, and the marker is PRESERVED (not consumed/revoked).
        assert!(
            enforce_tool_scoped_with(&cwd, "Bash", WriteTarget::Unknown, "git status", true)
                .is_none(),
            "read-only discovery must bypass the scope-delta revoke"
        );
        assert!(
            has_pending_user_turn(&cwd),
            "a read-only command must NOT consume the pending-user-turn marker"
        );
        // The first WRITE now lands the revoke with `user_scope_delta`.
        let deny = enforce_tool_scoped_with(&cwd, "Write", WriteTarget::Path("src/x.rs"), "", true)
            .expect("the first write after a scope delta must re-gate");
        assert!(
            deny_code(&deny).contains("user_scope_delta"),
            "{}",
            deny_code(&deny)
        );

        // With orientation OFF the bypass does not apply: a scope-delta + read-only Bash still
        // hits reconcile and is denied with `user_scope_delta` (byte-identical to pre-carve-out).
        let cwd2 = tmp();
        approve_with_globs(&cwd2, json!(["src/*.rs"]));
        assert!(set_pending_user_turn(&cwd2, TurnClass::UnknownDelta));
        let deny =
            enforce_tool_scoped_with(&cwd2, "Bash", WriteTarget::Unknown, "git status", false)
                .expect("carve-out off: read-only Bash still reconciles the scope delta");
        assert!(
            deny_code(&deny).contains("user_scope_delta"),
            "{}",
            deny_code(&deny)
        );
    }

    #[test]
    fn read_only_command_consumes_a_trivial_continuation_marker() {
        // A TRIVIAL continuation marker must still be CONSUMED by an admitted read-only command,
        // so the preserved approval becomes effective again — otherwise a lingering marker keeps
        // `effectively_approved` false and `review_checkpoint` would refuse (regression guard).
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/*.rs"]));
        assert!(set_pending_user_turn(&cwd, TurnClass::TrivialContinue));
        assert!(has_pending_user_turn(&cwd));
        assert!(
            blocks_writes(&cwd),
            "a pending marker makes the approval not-yet-effective"
        );
        // Orientation-on read-only Bash is allowed AND clears the trivial marker.
        assert!(
            enforce_tool_scoped_with(&cwd, "Bash", WriteTarget::Unknown, "git status", true)
                .is_none(),
            "read-only `git status` should run under a trivial continuation"
        );
        assert!(
            !has_pending_user_turn(&cwd),
            "a trivial-continuation marker must be consumed by the read-only command"
        );
        assert!(
            is_effectively_approved(&cwd) && !blocks_writes(&cwd),
            "consuming the trivial marker restores the effective approval"
        );
    }

    // ── v0.32 Unit 4: post-approval write-time scope fence ──────────────────────────────
    //
    // Set up an APPROVED epoch with an explicit `approved_allowed_globs` (written directly so
    // corrupt shapes that `record` could never produce can be exercised). The state is
    // effectively-approved, so `enforce_tool_scoped` reaches the fence.
    fn approve_with_globs(cwd: &str, globs: Value) {
        enable(cwd).unwrap();
        start_epoch(cwd, "sess", "task");
        let epoch = current_epoch(cwd);
        let s = json!({
            "epoch": epoch,
            "approved": true,
            "approved_epoch": epoch,
            "approved_plan_hash": hash_str("the plan"),
            "approved_allowed_globs": globs,
            "status": "approved",
            "rounds": 1,
            "same_findings": 0,
        });
        write_state(cwd, &s).unwrap();
        assert!(!blocks_writes(cwd), "state should be effectively approved");
    }

    fn deny_code(deny: &str) -> String {
        let v: Value = serde_json::from_str(deny).expect("deny is valid json");
        v.pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn scope_fence_allows_in_scope_write() {
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/a.rs"]));
        assert!(enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "").is_none());
    }

    #[test]
    fn scope_fence_denies_out_of_scope_write() {
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/a.rs"]));
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/b.rs"), "")
            .expect("out-of-scope write must be denied");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
    }

    #[test]
    fn scope_fence_denies_uncanonicalizable_path() {
        // A `..` traversal can never be confined under the repo root → fail closed.
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/a.rs"]));
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("../escape.txt"), "")
            .expect("traversal must be denied");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
    }

    #[test]
    fn scope_fence_denies_missing_path_under_scope() {
        // The authoritative hook produced no usable path for a gated write under an active
        // scope → fail closed (finding: missing path must not slip the fence).
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/a.rs"]));
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Missing, "")
            .expect("missing path under active scope must be denied");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
    }

    #[test]
    fn scope_fence_inert_without_a_declared_scope() {
        // Empty scope = non-declaring plan → byte-identical to pre-Unit-4 (writes flow, the
        // Stop-gate is the backstop). Both a concrete path and a missing path are allowed.
        let cwd = tmp();
        approve_with_globs(&cwd, json!([]));
        assert!(
            enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("anywhere/x.rs"), "").is_none()
        );
        assert!(enforce_tool_scoped(&cwd, "Write", WriteTarget::Missing, "").is_none());
    }

    #[test]
    fn scope_fence_denies_corrupt_scope() {
        // PRESENT-but-malformed scope must DENY, never silently widen authorization.
        for corrupt in [
            json!("not-an-array"),     // non-array
            json!([123]),              // non-string entry
            json!(["src/a.rs", true]), // mixed non-string entry
            json!([r"src\x"]),         // an entry that fails validate_glob
        ] {
            let cwd = tmp();
            approve_with_globs(&cwd, corrupt.clone());
            let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "")
                .unwrap_or_else(|| panic!("corrupt scope {corrupt} must deny"));
            assert!(
                deny_code(&deny).contains("out_of_scope_path"),
                "for {corrupt}"
            );
        }
    }

    #[test]
    fn scope_fence_denies_all_when_scope_declared_but_empty() {
        // A plan DECLARED a scope but the reviewer approved ZERO globs (e.g. a recursive glob
        // dropped for a missing broad-scope grant). Nothing is in scope → EVERY write denies
        // (path or missing), but legacy (Unknown) + Bash stay free.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        let s = json!({
            "epoch": epoch,
            "approved": true,
            "approved_epoch": epoch,
            "approved_plan_hash": hash_str("the plan"),
            "approved_allowed_globs": [],
            "approved_scope_declared": true,
            "status": "approved",
            "rounds": 1,
            "same_findings": 0,
        });
        write_state(&cwd, &s).unwrap();
        assert!(!blocks_writes(&cwd));
        let d1 = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "")
            .expect("declared-but-empty scope denies a path write");
        assert!(deny_code(&d1).contains("out_of_scope_path"));
        let d2 = enforce_tool_scoped(&cwd, "Write", WriteTarget::Missing, "")
            .expect("declared-but-empty scope denies a pathless write");
        assert!(deny_code(&d2).contains("out_of_scope_path"));
        // legacy + Bash unaffected
        assert!(enforce_tool_scoped(&cwd, "Write", WriteTarget::Unknown, "").is_none());
        assert!(enforce_tool_scoped(&cwd, "Bash", WriteTarget::Unknown, "echo x > y").is_none());
    }

    #[test]
    fn scope_fence_denies_when_declared_flag_is_malformed() {
        // A PRESENT non-boolean `approved_scope_declared` is corrupt authority state → deny.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        let s = json!({
            "epoch": epoch,
            "approved": true,
            "approved_epoch": epoch,
            "approved_plan_hash": hash_str("the plan"),
            "approved_allowed_globs": [],
            "approved_scope_declared": "true", // a string, not a bool
            "status": "approved",
            "rounds": 1,
            "same_findings": 0,
        });
        write_state(&cwd, &s).unwrap();
        assert!(!blocks_writes(&cwd));
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "")
            .expect("malformed declared flag must deny");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
    }

    #[test]
    fn scope_declared_in_plan_detects_label_presence() {
        assert!(scope_declared_in_plan("ALLOWED-GLOBS: src/a.rs"));
        assert!(scope_declared_in_plan("intro\n  allowed-globs:\nmore")); // bare, case-insens, indented
        assert!(scope_declared_in_plan("ALLOWED-GLOBS:"));
        assert!(!scope_declared_in_plan(
            "no scope\nintended_files: src/a.rs"
        ));
        assert!(!scope_declared_in_plan(""));
    }

    #[test]
    fn record_with_bare_allowed_globs_label_denies_writes() {
        // A present-but-empty `ALLOWED-GLOBS:` line is a DECLARED scope with zero approved globs
        // → DenyAll (not mistaken for a non-declaring plan, even though no glob token parses).
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        assert!(matches!(
            record(
                &cwd,
                &current_epoch(&cwd),
                "intro\nALLOWED-GLOBS:\nmore",
                &crate::gate::Verdict::Approve,
                "",
            ),
            Outcome::Approved
        ));
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "")
            .expect("bare ALLOWED-GLOBS: must deny");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
    }

    #[test]
    fn record_resume_with_bare_allowed_globs_label_denies_writes() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        begin_review(&cwd, "ALLOWED-GLOBS:");
        assert!(record_resume(&cwd, &epoch, "ALLOWED-GLOBS:", &[], &[]));
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "")
            .expect("bare ALLOWED-GLOBS: resume must deny");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
    }

    #[test]
    fn scope_fence_leaves_bash_and_run_free() {
        // Owner-accepted residual: Bash + mcp__aibridge__run are NOT fenced (Stop-gate backstop).
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/a.rs"]));
        assert!(
            enforce_tool_scoped(&cwd, "Bash", WriteTarget::Unknown, "echo x > out.txt").is_none()
        );
        assert!(
            enforce_tool_scoped(&cwd, "mcp__aibridge__run", WriteTarget::Unknown, "x").is_none()
        );
    }

    #[test]
    fn scope_fence_is_inert_for_legacy_callers() {
        // The legacy `enforce`/`enforce_tool` surfaces carry no tool-context (Unknown) → the
        // fence never fires, so they stay byte-identical even under an active or corrupt scope.
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/a.rs"]));
        assert!(enforce(&cwd, "Write").is_none());
        assert!(enforce_tool(&cwd, "Write", "").is_none());
        let cwd2 = tmp();
        approve_with_globs(&cwd2, json!("corrupt"));
        assert!(
            enforce(&cwd2, "Write").is_none(),
            "legacy stays allowed even with corrupt scope"
        );
    }

    #[test]
    fn scope_fence_does_not_fire_before_approval() {
        // Pre-approval, the pre-approval block (no_active_approval) wins; the scope fence (a
        // post-approval check) is never reached, so an out-of-scope-looking path is NOT the reason.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/b.rs"), "")
            .expect("pre-approval write denied");
        let code = deny_code(&deny);
        assert!(code.contains("no_active_approval"));
        assert!(!code.contains("out_of_scope_path"));
    }

    #[test]
    fn scope_fence_covers_notebook_edit() {
        let cwd = tmp();
        approve_with_globs(&cwd, json!(["src/*"]));
        let deny = enforce_tool_scoped(&cwd, "NotebookEdit", WriteTarget::Path("nb/x.ipynb"), "")
            .expect("out-of-scope notebook write denied");
        assert!(deny_code(&deny).contains("out_of_scope_path"));
        // ...and an in-scope notebook path is allowed.
        let cwd2 = tmp();
        approve_with_globs(&cwd2, json!(["nb/*"]));
        assert!(
            enforce_tool_scoped(&cwd2, "NotebookEdit", WriteTarget::Path("nb/x.ipynb"), "")
                .is_none()
        );
    }

    #[test]
    fn out_of_scope_block_reason_round_trips() {
        assert_eq!(BlockReason::OutOfScopePath.code(), "out_of_scope_path");
        assert!(BlockReason::OutOfScopePath
            .recovery_hint()
            .contains("ALLOWED-GLOBS"));
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
            "git push -uf origin main", // clustered force
            "git push -fu origin main", // clustered force (reordered)
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
            "git push origin :stale",     // delete refspec
            "git push origin +main",      // leading-+ (force-update) refspec
            "git push origin tag v1.2.3", // explicit tag push
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
        for t in [
            "ok", "okay", "OK", "Yes", "yes!", "yeah", "yep", "y", "continue", "proceed",
            "go ahead", "go on", "",
        ] {
            assert_eq!(classify_turn(t), TrivialContinue, "{t:?} must be trivial");
        }
        for t in [
            "cancel",
            "stop",
            "reset",
            "pause",
            "abort",
            "never mind",
            "start over",
            "stop, do something else",
        ] {
            assert_eq!(
                classify_turn(t),
                ExplicitReset,
                "{t:?} must be explicit reset"
            );
        }
        for t in [
            "also add a delete endpoint",
            "actually use postgres instead",
            "yes but also push to prod",
            "ok now migrate the db",
            "do it", // not in the tiny allowlist → fail closed to delta
        ] {
            assert_eq!(
                classify_turn(t),
                UnknownDelta,
                "{t:?} must be unknown delta"
            );
        }
    }

    #[test]
    fn preserve_epoch_decision_is_invalidate_unless_trivially_safe() {
        use TurnClass::*;
        // Flag ON (default) → never preserve (today's per-turn reset).
        assert!(!preserve_epoch_decision(true, true, true, TrivialContinue));
        // No current approval → nothing to preserve.
        assert!(!preserve_epoch_decision(
            false,
            false,
            true,
            TrivialContinue
        ));
        // v0.32 Unit 5: no real enforced scope → never preserve (fail closed → re-arm).
        assert!(!preserve_epoch_decision(
            false,
            true,
            false,
            TrivialContinue
        ));
        assert!(!preserve_epoch_decision(false, true, false, UnknownDelta));
        // Flag OFF + approved + enforced scope: preserve for trivial AND unknown (the marker
        // defers the authoritative decision to reconcile), but NOT for an explicit reset.
        assert!(preserve_epoch_decision(false, true, true, TrivialContinue));
        assert!(preserve_epoch_decision(false, true, true, UnknownDelta));
        assert!(!preserve_epoch_decision(false, true, true, ExplicitReset));
    }

    #[test]
    fn p1_state_invalidation_now_supported() {
        // v0.32 Unit 5 flipped this on (Unit 4 provides the write-time scope fence it waited for).
        assert!(p1_state_invalidation_supported());
    }

    // A throwaway APPROVED epoch with an explicit `approved_allowed_globs` + `approved_scope_declared`,
    // so each ScopeState (Globs / None / DenyAll / Corrupt) can be set up for the Unit-5 tests.
    fn approve_with_scope_state(cwd: &str, globs: Value, declared: bool) {
        enable(cwd).unwrap();
        start_epoch(cwd, "sess", "task");
        let epoch = current_epoch(cwd);
        let s = json!({
            "epoch": epoch,
            "approved": true,
            "approved_epoch": epoch,
            "approved_plan_hash": hash_str("the plan"),
            "approved_allowed_globs": globs,
            "approved_scope_declared": declared,
            "status": "approved",
            "rounds": 1,
            "same_findings": 0,
            "approved_generation": 1, // v0.32 Unit B: a real approval carries a generation
        });
        write_state(cwd, &s).unwrap();
        assert!(!blocks_writes(cwd));
    }

    #[test]
    fn scope_is_enforced_only_for_real_nonempty_scope() {
        let g = tmp();
        approve_with_scope_state(&g, json!(["src/a.rs"]), true);
        assert!(scope_is_enforced(&g), "Globs → enforced");
        let none = tmp();
        approve_with_scope_state(&none, json!([]), false);
        assert!(
            !scope_is_enforced(&none),
            "empty/non-declaring (None) → not enforced"
        );
        let denyall = tmp();
        approve_with_scope_state(&denyall, json!([]), true);
        assert!(
            !scope_is_enforced(&denyall),
            "declared-but-empty (DenyAll) → not enforced"
        );
        let corrupt = tmp();
        approve_with_scope_state(&corrupt, json!("oops"), false);
        assert!(!scope_is_enforced(&corrupt), "corrupt → not enforced");
    }

    #[test]
    fn start_epoch_preserves_trivial_continue_with_enforced_scope() {
        // resetOnUserTurn=false + approved + Globs scope + a trivial "ok" → SAME epoch preserved,
        // a trivial_continue marker recorded, effective approval suspended until reconciled.
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let epoch_before = current_epoch(&cwd);
        start_epoch_inner(&cwd, "sess", "ok", false);
        assert_eq!(
            current_epoch(&cwd),
            epoch_before,
            "epoch preserved, not re-armed"
        );
        assert!(
            has_pending_user_turn(&cwd),
            "a pending user-turn marker is recorded"
        );
        assert_eq!(
            read_state(&cwd).unwrap()["pending_user_turn"]["classification"],
            json!(TurnClass::TrivialContinue.as_marker())
        );
        assert!(
            !effectively_approved(&cwd),
            "suspended until PreToolUse reconciles"
        );
    }

    #[test]
    fn start_epoch_rearms_trivial_continue_without_enforced_scope() {
        // resetOnUserTurn=false + approved but NO real enforced scope (None / DenyAll / Corrupt)
        // → re-arm a fresh epoch (fail closed): no preserved approval, no marker.
        for (globs, declared) in [(json!([]), false), (json!([]), true), (json!("x"), false)] {
            let cwd = tmp();
            approve_with_scope_state(&cwd, globs.clone(), declared);
            let epoch_before = current_epoch(&cwd);
            start_epoch_inner(&cwd, "sess", "ok", false);
            assert_ne!(
                current_epoch(&cwd),
                epoch_before,
                "no enforced scope must re-arm (globs={globs}, declared={declared})"
            );
            assert!(!is_approved(&cwd), "prior approval not carried over");
            assert!(blocks_writes(&cwd));
            assert!(!has_pending_user_turn(&cwd), "no preserve marker");
        }
    }

    #[test]
    fn start_epoch_marks_unknown_delta_then_pretooluse_regates() {
        // resetOnUserTurn=false + approved + Globs scope + a non-trivial turn → preserved as an
        // unknown_delta marker, then the next mutator re-gates with user_scope_delta (reconcile
        // runs BEFORE the scope fence) and revokes approval.
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        start_epoch_inner(&cwd, "sess", "also add a delete endpoint", false);
        assert_eq!(
            read_state(&cwd).unwrap()["pending_user_turn"]["classification"],
            json!(TurnClass::UnknownDelta.as_marker())
        );
        let deny = enforce_tool_scoped(&cwd, "Write", WriteTarget::Path("src/a.rs"), "")
            .expect("unknown-delta turn must re-gate the next write");
        assert!(deny_code(&deny).contains("user_scope_delta"));
        assert!(!is_approved(&cwd), "scope-delta turn revokes approval");
    }

    #[test]
    fn start_epoch_default_reset_mode_rearms_even_with_enforced_scope() {
        // The DEFAULT (resetOnUserTurn=true) re-arms every prompt regardless of scope.
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let epoch_before = current_epoch(&cwd);
        start_epoch_inner(&cwd, "sess", "ok", true);
        assert_ne!(current_epoch(&cwd), epoch_before, "default mode re-arms");
        assert!(!has_pending_user_turn(&cwd));
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
        assert!(
            !has_pending_user_turn(&cwd),
            "no marker in per-turn-reset mode"
        );
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
        assert!(
            enforce(&cwd, "Write").is_none(),
            "trivial continuation must auto-allow"
        );
        assert!(!has_pending_user_turn(&cwd), "marker consumed");
        assert!(
            is_approved(&cwd),
            "approval preserved across a trivial continuation"
        );
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
        assert!(
            !has_pending_user_turn(&cwd),
            "marker consumed even on re-gate"
        );
    }

    #[test]
    fn malformed_pending_turn_fails_closed() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        // A present-but-malformed marker (classification not a string) must NOT preserve.
        let mut s = read_state(&cwd).unwrap();
        set_field(
            &mut s,
            "pending_user_turn",
            json!({ "classification": 123 }),
        );
        write_state(&cwd, &s).unwrap();
        assert!(
            !effectively_approved(&cwd),
            "malformed marker suspends approval"
        );
        let deny = reconcile_pending_user_turn(&cwd);
        assert!(
            deny.is_some(),
            "malformed marker must re-gate (fail closed)"
        );
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
    fn a_trivial_prompt_cannot_downgrade_a_pending_scope_delta() {
        // Codex-flagged fail-open: a scope-delta marker followed by a trivial "ok" before any
        // reconcile must NOT be erased — the next write must still re-gate.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        set_pending_user_turn(&cwd, TurnClass::UnknownDelta); // "also update the API"
        assert!(set_pending_user_turn(&cwd, TurnClass::TrivialContinue)); // a following "ok"
                                                                          // The marker stays non-trivial → the write re-gates with user_scope_delta.
        let deny = enforce(&cwd, "Write").expect("downgraded marker must still re-gate");
        assert!(deny.contains("user_scope_delta"), "{deny}");
        assert!(!is_approved(&cwd));
    }

    #[test]
    fn a_scope_delta_escalates_a_prior_trivial_marker() {
        // The reverse direction DOES update: a trivial marker followed by a scope delta must
        // escalate so the write re-gates (monotonic upward).
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        set_pending_user_turn(&cwd, TurnClass::TrivialContinue);
        set_pending_user_turn(&cwd, TurnClass::UnknownDelta);
        let deny = enforce(&cwd, "Write").expect("escalated marker must re-gate");
        assert!(deny.contains("user_scope_delta"), "{deny}");
        assert!(!is_approved(&cwd));
    }

    #[test]
    fn begin_review_consumes_the_marker_then_approve_unlocks() {
        // The proper re-plan flow: a user turn set a marker; the agent re-submits a plan, so
        // begin_review consumes the marker ("at review start"); the subsequent APPROVE unlocks.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan v1");
        set_pending_user_turn(&cwd, TurnClass::UnknownDelta);
        assert!(!is_effectively_approved(&cwd), "marker suspends approval");
        begin_review(&cwd, "plan v2"); // re-plan consumes the marker at review start
        assert!(
            !has_pending_user_turn(&cwd),
            "begin_review consumed the marker"
        );
        record(
            &cwd,
            &current_epoch(&cwd),
            "plan v2",
            &crate::gate::Verdict::Approve,
            "",
        );
        assert!(
            is_effectively_approved(&cwd),
            "re-planned approval is effective"
        );
    }

    #[test]
    fn user_turn_arriving_during_review_refuses_approve() {
        // Codex-flagged race: if a NON-trivial prompt arrives AFTER begin_review (during the
        // in-flight Codex review), the marker re-appears and `record` must REFUSE the approve —
        // the new turn was never reviewed, so writes must not unlock.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        begin_review(&cwd, "plan A"); // review starts (marker, if any, consumed)
        set_pending_user_turn(&cwd, TurnClass::UnknownDelta); // a new prompt mid-review
        assert!(
            matches!(
                record(&cwd, &epoch, "plan A", &crate::gate::Verdict::Approve, ""),
                Outcome::NeedsInfo(_)
            ),
            "an unreviewed mid-review turn must refuse approve"
        );
        assert!(!is_approved(&cwd));
    }

    #[test]
    fn begin_review_reports_the_consumed_marker_class() {
        // The receipt fast-path in mcp.rs relies on this report to SKIP a resume when a
        // non-trivial turn was pending at review start (else the resume runs no review).
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        // Successful review starts wrap the consumed marker class in ReviewStart::Ready.
        assert!(matches!(
            begin_review(&cwd, "plan"),
            ReviewStart::Ready(ConsumedTurn::None)
        ));
        set_pending_user_turn(&cwd, TurnClass::TrivialContinue);
        assert!(matches!(
            begin_review(&cwd, "plan"),
            ReviewStart::Ready(ConsumedTurn::Trivial)
        ));
        set_pending_user_turn(&cwd, TurnClass::UnknownDelta);
        assert!(matches!(
            begin_review(&cwd, "plan"),
            ReviewStart::Ready(ConsumedTurn::NonTrivial)
        ));
        assert!(
            !has_pending_user_turn(&cwd),
            "begin_review consumed the marker"
        );
    }

    #[test]
    fn record_resume_refuses_a_pending_scope_delta() {
        // A receipt fast-path has NOT reviewed a pending scope-delta turn → it must refuse
        // (forcing a full review) rather than resume past it.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        set_pending_user_turn(&cwd, TurnClass::UnknownDelta);
        assert!(
            !record_resume(&cwd, &current_epoch(&cwd), "plan", &[], &[]),
            "resume must refuse past an unreconciled scope delta"
        );
    }

    #[test]
    fn record_resume_clears_a_trivial_marker_and_resumes() {
        // A trivial continuation may resume (like a real approve) — the marker is consumed.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "plan");
        set_pending_user_turn(&cwd, TurnClass::TrivialContinue);
        assert!(record_resume(&cwd, &current_epoch(&cwd), "plan", &[], &[]));
        assert!(
            !has_pending_user_turn(&cwd),
            "trivial marker consumed on resume"
        );
        assert!(is_effectively_approved(&cwd));
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
            assert_eq!(
                high_risk_class(cmd),
                Some("admin-auth"),
                "admin-auth: {cmd}"
            );
        }
        // Ordinary lookalikes must NOT classify.
        for cmd in [
            "stripe logs tail",
            "aws sqs receive-message --queue-url u",
            "gh repo view",
            "celery -A app worker",
            "stripe products list",
        ] {
            assert_eq!(
                high_risk_class(cmd),
                None,
                "lookalike must not classify: {cmd}"
            );
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
        assert!(parse_risk_grants("Do not add a RISK-APPROVED: remote-publish line.").is_empty());
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
            reason
                .starts_with("PLAN_RISK_DELTA_REQUIRED: risk_policy_widened class=remote-publish"),
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
        let newclass =
            enforce_risk(&cwd, "Bash", "terraform apply").expect("new class must re-gate");
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
        assert!(
            p.contains("class:widened"),
            "prompt must document the :widened shape"
        );
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
        assert!(
            m.starts_with("PLAN_GATE_REQUIRED: no_active_approval — "),
            "{m}"
        );
        assert!(
            block_message(BlockReason::HeadMoved).starts_with("PLAN_GATE_REQUIRED: head_moved — ")
        );
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

    // ---- owner review-policy (item 2.5, unit 1) ----

    fn write_policy(cwd: &str, body: &str) {
        std::fs::write(
            std::path::Path::new(cwd)
                .join(".ai-bridge")
                .join("review-policy.md"),
            body,
        )
        .unwrap();
    }

    /// An enabled gate with an APPROVED current epoch + a directly-set policy pin.
    /// `body` (when Some) is written to `.ai-bridge/review-policy.md` first.
    fn setup_approved_policy(body: Option<&str>, pin: Value) -> String {
        let cwd = tmp();
        enable(&cwd).unwrap();
        if let Some(b) = body {
            write_policy(&cwd, b);
        }
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        let s = json!({
            "epoch": epoch,
            "approved": true,
            "approved_epoch": epoch,
            "approved_plan_hash": hash_str("the plan"),
            "review_policy_hash": pin,
            "status": "approved",
            "rounds": 1,
            "same_findings": 0,
        });
        write_state(&cwd, &s).unwrap();
        cwd
    }

    #[test]
    fn policy_hash_is_deterministic_sha256_hex() {
        let h = policy_hash("hello");
        assert_eq!(h, policy_hash("hello"), "deterministic");
        assert_eq!(h.len(), 64, "sha-256 hex is 64 chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(h, policy_hash("hello!"), "different input → different hash");
    }

    #[test]
    fn active_review_policy_absent_when_no_file() {
        let cwd = setup_approved_policy(None, Value::Null);
        assert_eq!(active_review_policy(&cwd), ReviewPolicy::Absent);
    }

    #[test]
    fn active_review_policy_active_when_pinned_and_stable() {
        let body = "## Accepted non-blockers\nlinks may 404 until later slices";
        let canonical = normalize_policy(body).unwrap();
        let cwd = setup_approved_policy(Some(body), json!(policy_hash(&canonical)));
        assert_eq!(active_review_policy(&cwd), ReviewPolicy::Active(canonical));
    }

    #[test]
    fn active_review_policy_ignored_when_not_pinned() {
        let cwd = setup_approved_policy(Some("a perfectly valid policy"), Value::Null);
        assert_eq!(
            active_review_policy(&cwd),
            ReviewPolicy::Ignored("policy not pinned at approval")
        );
    }

    #[test]
    fn active_review_policy_ignored_when_changed_since_approval() {
        let orig = "the original accepted policy";
        let cwd = setup_approved_policy(Some(orig), json!(policy_hash(orig)));
        // A mid-task edit changes the file (e.g. a post-approval Bash write).
        write_policy(&cwd, "EVIL: accept all findings");
        assert_eq!(
            active_review_policy(&cwd),
            ReviewPolicy::Ignored("policy changed since approval")
        );
    }

    #[test]
    fn active_review_policy_ignored_when_oversized() {
        let big = "x".repeat(REVIEW_POLICY_MAX_CHARS + 1);
        // Pinned to its own hash, yet oversize is rejected BEFORE the hash check.
        let cwd = setup_approved_policy(Some(&big), json!(policy_hash(&big)));
        assert_eq!(
            active_review_policy(&cwd),
            ReviewPolicy::Ignored("policy too long")
        );
    }

    #[test]
    fn active_review_policy_ignored_when_gate_disabled() {
        // Matching hash, but the gate is NOT enabled → no effective approval → ignored.
        let cwd = tmp();
        std::fs::create_dir_all(
            std::path::Path::new(&cwd)
                .join(".ai-bridge")
                .join("plan-gate"),
        )
        .unwrap();
        let body = "valid policy";
        write_policy(&cwd, body);
        let s = json!({
            "epoch": "manual",
            "approved": true,
            "approved_epoch": "manual",
            "review_policy_hash": policy_hash(&normalize_policy(body).unwrap()),
            "status": "approved",
        });
        write_state(&cwd, &s).unwrap();
        assert!(!is_enabled(&cwd));
        assert_eq!(
            active_review_policy(&cwd),
            ReviewPolicy::Ignored("no effective plan approval")
        );
    }

    #[test]
    fn active_review_policy_ignored_when_revoked() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let body = "valid policy";
        write_policy(&cwd, body);
        approve(&cwd, "PLAN"); // pins the policy
        revoke(&cwd, "test"); // approved=false + clears the pin
        assert_eq!(
            active_review_policy(&cwd),
            ReviewPolicy::Ignored("no effective plan approval")
        );
    }

    #[test]
    fn record_pins_policy_and_resume_nulls_it() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        let body = "accepted: links may 404";
        write_policy(&cwd, body);
        approve(&cwd, "PLAN"); // real record() Approve → pins the current policy
        assert_eq!(
            read_state(&cwd).unwrap()["review_policy_hash"],
            json!(policy_hash(&normalize_policy(body).unwrap())),
            "record() pins the active policy hash"
        );
        // A mid-task edit + a receipt resume → the pin is Null'd (FIX C), so the changed
        // policy is NOT activated by a no-review resume.
        write_policy(&cwd, "EVIL changed policy");
        assert!(record_resume(&cwd, &epoch, "PLAN", &[], &[]));
        assert_eq!(read_state(&cwd).unwrap()["review_policy_hash"], Value::Null);
        assert_eq!(
            active_review_policy(&cwd),
            ReviewPolicy::Ignored("policy not pinned at approval")
        );
    }

    #[test]
    fn non_approve_clears_the_policy_pin() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        write_policy(&cwd, "policy");
        approve(&cwd, "PLAN"); // pins
        assert_ne!(read_state(&cwd).unwrap()["review_policy_hash"], Value::Null);
        let _ = record(
            &cwd,
            &current_epoch(&cwd),
            "PLAN",
            &crate::gate::Verdict::RequestChanges,
            "FINDINGS: x",
        );
        assert_eq!(read_state(&cwd).unwrap()["review_policy_hash"], Value::Null);
    }

    #[test]
    fn review_policy_active_text_and_fp() {
        assert_eq!(ReviewPolicy::Active("P".into()).active_text(), Some("P"));
        assert_eq!(ReviewPolicy::Absent.active_text(), None);
        assert_eq!(ReviewPolicy::Ignored("x").active_text(), None);
        let p = ReviewPolicy::Active("P".into()).fp();
        let q = ReviewPolicy::Active("Q".into()).fp();
        assert_ne!(p, q, "different policy content → different fp");
        assert_ne!(
            p,
            ReviewPolicy::Absent.fp(),
            "policy vs none → different fp"
        );
        assert_eq!(
            ReviewPolicy::Absent.fp(),
            ReviewPolicy::Ignored("any").fp(),
            "Absent and Ignored both inject nothing → same fp"
        );
    }

    #[test]
    fn review_policy_present_detects_in_bounds_file() {
        let cwd = tmp();
        std::fs::create_dir_all(std::path::Path::new(&cwd).join(".ai-bridge")).unwrap();
        assert!(!review_policy_present(&cwd), "no file → not present");
        write_policy(&cwd, "   \n  ");
        assert!(
            !review_policy_present(&cwd),
            "whitespace-only → not present"
        );
        write_policy(&cwd, "accepted: links may 404");
        assert!(review_policy_present(&cwd), "in-bounds policy → present");
        write_policy(&cwd, &"x".repeat(REVIEW_POLICY_MAX_CHARS + 1));
        assert!(
            !review_policy_present(&cwd),
            "oversized → not present (fail-closed)"
        );
    }

    #[test]
    fn strip_html_comments_drops_balanced_keeps_unbalanced() {
        assert_eq!(strip_html_comments("a<!--x-->b"), "ab");
        assert_eq!(strip_html_comments("a<!--\nmulti\nline-->b"), "ab");
        assert_eq!(strip_html_comments("<!--only-->"), "");
        // An unbalanced opener is kept verbatim (conservative → reads as present).
        assert_eq!(strip_html_comments("keep<!--no close"), "keep<!--no close");
        assert_eq!(strip_html_comments("no comments"), "no comments");
    }

    #[test]
    fn normalize_policy_treats_all_comment_scaffold_as_inert() {
        // The init scaffold (the whole file is one HTML comment) → None (Absent/inert).
        assert_eq!(
            normalize_policy("<!-- template, no entries yet -->\n"),
            None
        );
        // A real entry outside comments → Some(stripped entry); inline comments removed.
        let raw = "<!-- docs -->\n## Accepted\nlinks may 404 <!-- note --> until later";
        let got = normalize_policy(raw).unwrap();
        assert!(got.contains("## Accepted") && got.contains("links may 404"));
        assert!(
            !got.contains("docs") && !got.contains("note"),
            "comment text is stripped from the canonical"
        );
    }

    #[test]
    fn review_policy_present_false_for_all_comment_scaffold() {
        let cwd = tmp();
        std::fs::create_dir_all(std::path::Path::new(&cwd).join(".ai-bridge")).unwrap();
        write_policy(
            &cwd,
            "<!--\nAI Bridge review policy template — no entries yet.\n-->\n",
        );
        assert!(
            !review_policy_present(&cwd),
            "an all-comment scaffold must be inert (not present)"
        );
    }

    // ─────────── v0.32 workflow-gate Unit A: state RMW lock + force_block poison ───────────

    /// Hold the cross-process `state.lock` via a RAW handle (NOT through `with_state_lock`, so the
    /// in-proc mutex stays free) → a mutator's bounded try-lock times out and FAILS CLOSED. Works
    /// in-process: a second handle's `try_lock_exclusive` is WouldBlock while this one holds it.
    fn hold_state_lock(cwd: &str) -> std::fs::File {
        let d = dir(cwd);
        std::fs::create_dir_all(&d).unwrap();
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(d.join("state.lock"))
            .unwrap();
        fs4::fs_std::FileExt::lock_exclusive(&f).unwrap();
        f
    }

    #[test]
    fn state_lock_serializes_concurrent_writers() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        let counter = dir(&cwd).join("counter");
        std::fs::write(&counter, "0").unwrap();
        const N: u64 = 150;
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let cwd = cwd.clone();
                let counter = counter.clone();
                std::thread::spawn(move || {
                    for _ in 0..N {
                        let done = with_state_lock(&cwd, |_g| {
                            let v: u64 = std::fs::read_to_string(&counter)
                                .unwrap()
                                .trim()
                                .parse()
                                .unwrap();
                            std::thread::yield_now(); // widen the lost-update window
                            std::fs::write(&counter, (v + 1).to_string()).unwrap();
                        });
                        assert!(done.is_some(), "uncontended lock must acquire");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let final_v: u64 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(final_v, 2 * N, "the in-proc lock prevents lost updates");
    }

    #[test]
    fn lock_timeout_poisons_start_epoch() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        assert!(is_effectively_approved(&cwd));
        let _held = hold_state_lock(&cwd);
        set_state_lock_budget_for_test(Duration::from_millis(30));
        start_epoch(&cwd, "sess", "next task");
        assert!(force_block_active(&cwd), "lock-timeout start_epoch poisons");
        assert!(!is_effectively_approved(&cwd), "stale approval neutralized");
        assert!(blocks_writes(&cwd));
    }

    #[test]
    fn lock_timeout_poisons_revoke() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        let _held = hold_state_lock(&cwd);
        set_state_lock_budget_for_test(Duration::from_millis(30));
        revoke(&cwd, "test");
        assert!(force_block_active(&cwd));
        assert!(!is_effectively_approved(&cwd));
    }

    #[test]
    fn lock_timeout_begin_review_marker_write_failed() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        let _held = hold_state_lock(&cwd);
        set_state_lock_budget_for_test(Duration::from_millis(30));
        assert_eq!(
            begin_review(&cwd, "PLAN v2"),
            ReviewStart::MarkerWriteFailed
        );
        assert!(
            force_block_active(&cwd),
            "begin_review lock-timeout poisons"
        );
        assert!(!is_effectively_approved(&cwd));
        assert!(
            read_state(&cwd).is_none(),
            "stale approved state removed even if the sentinel cannot be relied on"
        );
    }

    #[test]
    fn lock_timeout_record_branches_by_verdict() {
        // Approve under a lock-timeout → NeedsInfo, NO poison (prior pending state is fail-closed).
        let a = tmp();
        enable(&a).unwrap();
        start_epoch(&a, "sess", "task");
        let ea = current_epoch(&a);
        let ha = hold_state_lock(&a);
        set_state_lock_budget_for_test(Duration::from_millis(30));
        assert!(matches!(
            record(&a, &ea, "PLAN", &crate::gate::Verdict::Approve, ""),
            Outcome::NeedsInfo(_)
        ));
        assert!(
            !force_block_active(&a),
            "Approve lock-timeout must NOT poison"
        );
        drop(ha);

        // RequestChanges under a lock-timeout → poison + a non-approve outcome (a prior approval,
        // e.g. re-reviewing the same plan, must NOT survive the failed revocation).
        let b = tmp();
        enable(&b).unwrap();
        start_epoch(&b, "sess", "task");
        approve(&b, "PLAN");
        let eb = current_epoch(&b);
        let _hb = hold_state_lock(&b);
        assert!(matches!(
            record(
                &b,
                &eb,
                "PLAN",
                &crate::gate::Verdict::RequestChanges,
                "FINDINGS: x"
            ),
            Outcome::Revise(_)
        ));
        assert!(
            force_block_active(&b),
            "RequestChanges lock-timeout poisons"
        );
        assert!(!is_effectively_approved(&b), "prior approval neutralized");
    }

    #[test]
    fn lock_timeout_record_resume_returns_false() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        let _held = hold_state_lock(&cwd);
        set_state_lock_budget_for_test(Duration::from_millis(30));
        assert!(
            !record_resume(&cwd, &epoch, "PLAN", &[], &[]),
            "lock-timeout resume → false (caller runs a full review)"
        );
    }

    #[test]
    fn lock_timeout_reconcile_denies_when_marker_present() {
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        assert!(set_pending_user_turn(&cwd, TurnClass::UnknownDelta));
        let _held = hold_state_lock(&cwd);
        set_state_lock_budget_for_test(Duration::from_millis(30));
        assert!(
            reconcile_pending_user_turn(&cwd).is_some(),
            "a marker present + lock-timeout must DENY (fail-closed), not silently consume"
        );
    }

    #[test]
    fn write_failure_poisons_revoke() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        assert!(is_effectively_approved(&cwd));
        FORCE_WRITE_FAIL.with(|c| c.set(true));
        revoke(&cwd, "test");
        FORCE_WRITE_FAIL.with(|c| c.set(false));
        assert!(force_block_active(&cwd), "revoke write-failure poisons");
        assert!(!is_effectively_approved(&cwd), "stale approval neutralized");
    }

    #[test]
    fn write_failure_record_paths() {
        // RequestChanges whose write FAILS → poison + still a Revise outcome.
        let a = tmp();
        enable(&a).unwrap();
        start_epoch(&a, "sess", "task");
        approve(&a, "PLAN");
        let ea = current_epoch(&a);
        FORCE_WRITE_FAIL.with(|c| c.set(true));
        let out = record(
            &a,
            &ea,
            "PLAN",
            &crate::gate::Verdict::RequestChanges,
            "FINDINGS: x",
        );
        FORCE_WRITE_FAIL.with(|c| c.set(false));
        assert!(matches!(out, Outcome::Revise(_)));
        assert!(force_block_active(&a), "non-approve write-failure poisons");
        assert!(!is_effectively_approved(&a));

        // Approve whose write FAILS → NeedsInfo (must NOT claim approval); no poison needed.
        let b = tmp();
        enable(&b).unwrap();
        start_epoch(&b, "sess", "task");
        let eb = current_epoch(&b);
        FORCE_WRITE_FAIL.with(|c| c.set(true));
        let out = record(&b, &eb, "PLAN", &crate::gate::Verdict::Approve, "");
        FORCE_WRITE_FAIL.with(|c| c.set(false));
        assert!(
            matches!(out, Outcome::NeedsInfo(_)),
            "Approve write-fail → not Approved"
        );
        assert!(
            !is_approved(&b),
            "the unpersisted approval did not take effect"
        );
    }

    #[test]
    fn write_failure_start_epoch_fresh_poisons() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        FORCE_WRITE_FAIL.with(|c| c.set(true));
        start_epoch_inner(&cwd, "sess", "a different task", true); // forced fresh-epoch
        FORCE_WRITE_FAIL.with(|c| c.set(false));
        assert!(
            force_block_active(&cwd),
            "fresh-epoch write-failure poisons"
        );
        assert!(!is_effectively_approved(&cwd));
    }

    #[test]
    fn force_block_denies_writes_on_all_surfaces() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        assert!(!blocks_writes(&cwd) && is_effectively_approved(&cwd));
        set_force_block(&cwd);
        // The CENTRAL predicate fails closed on BOTH surfaces.
        assert!(
            blocks_writes(&cwd),
            "PreToolUse + mcp run surface blocked under poison"
        );
        assert!(
            !is_effectively_approved(&cwd),
            "review_checkpoint surface blocked under poison"
        );
        let deny = enforce(&cwd, "Write").expect("a write is denied under force_block");
        assert!(
            deny.contains("gate_lock_unavailable"),
            "the deny carries the diagnostic reason"
        );
    }

    #[test]
    fn record_refuses_approval_under_force_block() {
        // A poisoned gate must never mint `Outcome::Approved` (the MCP layer emits
        // <AI-BRIDGE-APPROVE/> ONLY on Approved) — that would tell Claude "writes unlocked" while
        // every write is still denied by the central predicate.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        let epoch = current_epoch(&cwd);
        set_force_block(&cwd);
        assert!(
            matches!(
                record(&cwd, &epoch, "PLAN", &crate::gate::Verdict::Approve, ""),
                Outcome::NeedsInfo(_)
            ),
            "record must refuse to approve under force_block"
        );
        assert!(!is_effectively_approved(&cwd));
        // The receipt fast-path must refuse too (else a resume re-mints the ineffective approval).
        assert!(
            !record_resume(&cwd, &epoch, "PLAN", &[], &[]),
            "record_resume must refuse under force_block"
        );
    }

    #[test]
    fn force_block_cleared_by_fresh_epoch_not_by_approve() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        set_force_block(&cwd);
        // record approve must NOT lift the poison (only a fresh PENDING baseline may).
        let epoch = current_epoch(&cwd);
        let _ = record(&cwd, &epoch, "PLAN", &crate::gate::Verdict::Approve, "");
        assert!(
            force_block_active(&cwd),
            "record approve must not lift the poison"
        );
        assert!(!is_effectively_approved(&cwd));
        // A fresh-epoch start_epoch lifts it.
        start_epoch_inner(&cwd, "sess", "a new task", true);
        assert!(!force_block_active(&cwd), "fresh epoch lifts the poison");
    }

    #[test]
    fn force_block_forces_fresh_epoch_under_preserve_mode() {
        // resetOnUserTurn=false + approved + enforced scope + trivial "ok" would normally PRESERVE
        // the approval; an active force_block must override that and re-arm a fresh epoch (finding
        // #2 — else the poison is trapped forever behind trivial continuations).
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let epoch_before = current_epoch(&cwd);
        set_force_block(&cwd);
        start_epoch_inner(&cwd, "sess", "ok", false);
        assert_ne!(
            current_epoch(&cwd),
            epoch_before,
            "poison forces a fresh epoch (no preserve)"
        );
        assert!(
            !is_approved(&cwd),
            "stale approval not carried into the fresh epoch"
        );
        assert!(
            !force_block_active(&cwd),
            "the fresh epoch lifted the poison"
        );
        assert!(
            !has_pending_user_turn(&cwd),
            "no preserve marker was recorded"
        );
    }

    #[test]
    fn denied_writes_counter_skips_under_contention() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task"); // pending → blocks writes
        let _held = hold_state_lock(&cwd);
        // The deny path must still return a deny and not hang, even though the (non-blocking)
        // counter lock attempt loses the race for `state.lock`.
        assert!(
            enforce(&cwd, "Write").is_some(),
            "still denies pre-approval; counter contention is harmless"
        );
    }

    #[test]
    fn state_lock_recovers_from_poison() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        let c2 = cwd.clone();
        let h = std::thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                with_state_lock(&c2, |_g| panic!("intentional panic while holding the lock"));
            }));
        });
        h.join().unwrap();
        // The in-proc mutex for this path is now poisoned; a later acquire must RECOVER it rather
        // than wedge the warm server.
        assert_eq!(
            with_state_lock(&cwd, |_g| 42),
            Some(42),
            "a poisoned in-proc mutex recovers"
        );
    }

    #[test]
    fn gate_lock_unavailable_reason_code_and_hint() {
        assert_eq!(
            BlockReason::GateLockUnavailable.code(),
            "gate_lock_unavailable"
        );
        assert!(BlockReason::GateLockUnavailable
            .recovery_hint()
            .contains("AIBRIDGE_PLAN_GATE=0"));
    }

    #[test]
    fn marker_write_failed_message_points_to_fresh_epoch_not_retry() {
        // The MCP `MarkerWriteFailed` response must NOT tell the agent to retry plan_gate (record
        // refuses to approve while poisoned) — it must point at a fresh epoch / bypass.
        let m = marker_write_failed_message();
        assert!(
            !m.to_lowercase().contains("retry plan_gate"),
            "retrying plan_gate cannot lift the poison"
        );
        assert!(
            m.contains("new message") && m.contains("AIBRIDGE_PLAN_GATE=0"),
            "recovery must point at a fresh epoch or the bypass"
        );
    }

    #[test]
    fn stale_force_block_is_inert_when_gate_disabled() {
        // A `force_block` sentinel left over from a prior install must NOT affect behavior once the
        // gate is disabled (the `enabled` marker removed) — `is_force_blocked` mirrors enforcement
        // scope, so the gate-off contract is preserved.
        let cwd = tmp();
        std::fs::create_dir_all(dir(&cwd)).unwrap();
        set_force_block(&cwd);
        assert!(!is_enabled(&cwd), "gate is NOT enabled");
        assert!(
            !is_force_blocked(&cwd),
            "a disabled gate's stale sentinel is inert"
        );
        assert!(!blocks_writes(&cwd), "a disabled gate never blocks writes");
        assert!(
            run_tool_blocked_message(&cwd).is_none(),
            "`run` is not blocked by a stale sentinel under a disabled gate"
        );
        // Manual `record` (gate off) must NOT hit the poison refusal — it synthesizes harmless state.
        assert!(
            matches!(
                record(
                    &cwd,
                    &current_epoch(&cwd),
                    "PLAN",
                    &crate::gate::Verdict::Approve,
                    ""
                ),
                Outcome::Approved
            ),
            "disabled-gate manual approve proceeds despite a stale sentinel"
        );
    }

    #[test]
    fn mcp_recovery_messages_are_poison_aware() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        // Approved + not poisoned: `run` allowed; the normal NeedsInfo wrapper is unchanged.
        assert!(!is_force_blocked(&cwd));
        assert!(
            run_tool_blocked_message(&cwd).is_none(),
            "approved → run proceeds"
        );
        let ni_normal = needs_info_message("FINDINGS");
        assert!(
            ni_normal.contains("call `plan_gate` again") && ni_normal.contains("FINDINGS"),
            "non-poison NeedsInfo keeps the plan_gate-retry wrapper"
        );

        // Poison the gate → every recovery path points at a fresh epoch / bypass, NOT plan_gate.
        set_force_block(&cwd);
        assert!(is_force_blocked(&cwd));
        let run_msg = run_tool_blocked_message(&cwd).expect("run blocked under poison");
        assert!(
            run_msg.contains("new message") && run_msg.contains("AIBRIDGE_PLAN_GATE=0"),
            "run poison recovery → fresh epoch / bypass"
        );
        let ckpt = checkpoint_refused_message(&cwd);
        assert!(
            ckpt.contains("AIBRIDGE_PLAN_GATE=0") && ckpt.contains("Frontier unchanged"),
            "checkpoint poison recovery → fresh epoch / bypass"
        );
    }

    #[test]
    fn poisoned_outcome_message_never_says_retry_plan_gate() {
        // EVERY plan_gate outcome under poison routes through one shared recovery — including the
        // non-approve verdicts (Revise/Stuck) that the write/lock-failure paths can produce.
        for o in [
            Outcome::Revise("reviewer says X".to_string()),
            Outcome::Stuck("reviewer says X".to_string()),
        ] {
            let m = poisoned_outcome_message(&o);
            assert!(
                m.contains("AIBRIDGE_PLAN_GATE=0") && m.contains("reviewer says X"),
                "non-approve poison message gives recovery + keeps reviewer notes"
            );
            assert!(
                !m.contains("call `plan_gate` again"),
                "must NOT send the agent into a plan_gate retry loop"
            );
        }
        // Empty findings (record's poison refusal) → just the recovery, no dangling notes section.
        let m = poisoned_outcome_message(&Outcome::NeedsInfo(String::new()));
        assert!(m.contains("AIBRIDGE_PLAN_GATE=0") && !m.contains("Reviewer notes"));
    }

    #[test]
    fn plan_gate_short_circuits_when_poisoned() {
        // The plan_gate handler must refuse a poisoned gate BEFORE begin_review / the Codex round —
        // the decision is a pure function of cwd, so it's verifiable without driving the handler.
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        approve(&cwd, "PLAN");
        assert!(
            plan_gate_early_refusal(&cwd).is_none(),
            "not poisoned → proceed to review"
        );
        set_force_block(&cwd);
        let msg = plan_gate_early_refusal(&cwd).expect("poisoned gate refuses early");
        assert!(
            msg.contains("AIBRIDGE_PLAN_GATE=0") && !msg.contains("call `plan_gate` again"),
            "early refusal carries the fresh-epoch / bypass recovery"
        );
    }

    // ───────── v0.32 Unit B (operation LEASE): inert foundation — file helpers + validity ─────────

    /// Build a lease Value that mirrors the LIVE approved+scoped state, with the given time window.
    fn lease_value(cwd: &str, created_at: u64, expires_at: u64) -> Value {
        let s = read_state(cwd).unwrap();
        json!({
            "epoch": s["epoch"],
            "approved_epoch": s["approved_epoch"],
            "approved_plan_hash": s["approved_plan_hash"],
            "approved_allowed_globs": s["approved_allowed_globs"],
            "approved_generation": s["approved_generation"],
            "created_at": created_at,
            "expires_at": expires_at,
            "operation_id": "op-test",
        })
    }

    /// A REAL scoped approval via `record` (sets `approved_allowed_globs` AND bumps
    /// `approved_generation`), unlike the synthetic `approve_with_scope_state`. Caller does
    /// enable + start_epoch first; uses the same epoch each call (so a re-approve stays in-epoch).
    fn real_scoped_approve(cwd: &str) {
        let epoch = current_epoch(cwd);
        let plan = "do work\nALLOWED-GLOBS: src/a.rs";
        begin_review(cwd, plan);
        let findings = "ok\nSCOPE-APPROVED: src/a.rs";
        assert!(matches!(
            record(cwd, &epoch, plan, &crate::gate::Verdict::Approve, findings),
            Outcome::Approved
        ));
    }

    #[test]
    fn operation_lease_helpers_round_trip() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        let v = json!({ "operation_id": "x", "created_at": 1u64 });
        assert!(
            operation_lease::read_operation(&cwd).is_none(),
            "absent → None"
        );
        operation_lease::write_operation(&cwd, &v).unwrap();
        assert_eq!(
            operation_lease::read_operation(&cwd).unwrap()["operation_id"],
            "x"
        );
        operation_lease::clear_operation(&cwd);
        assert!(
            operation_lease::read_operation(&cwd).is_none(),
            "cleared → None"
        );
        // Durable sentinel.
        assert!(!operation_lease::operation_review_pending_active(&cwd));
        operation_lease::set_operation_review_pending(&cwd).unwrap();
        assert!(operation_lease::operation_review_pending_active(&cwd));
        operation_lease::clear_operation_review_pending(&cwd);
        assert!(!operation_lease::operation_review_pending_active(&cwd));
    }

    #[test]
    fn operation_lease_valid_happy_path_only() {
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let now = operation_lease::now_ms();
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now - 1000, now + 300_000))
            .unwrap();
        assert!(
            operation_lease::operation_lease_valid_with(&cwd, true),
            "config on + fresh in-window lease bound to the live approved+scoped epoch → VALID"
        );
        // Feature flag OFF → invalid even with a perfect lease. (The public `operation_lease_valid`
        // reader is config-backed and NOT asserted here — that would read the operator's real
        // review-mcp.json and break hermeticity; the config default is covered by the pure-parser
        // test `operation_lease_enabled_from_defaults_off`.)
        assert!(!operation_lease::operation_lease_valid_with(&cwd, false));
    }

    #[test]
    fn operation_lease_invalid_under_force_block() {
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let now = operation_lease::now_ms();
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now - 1000, now + 300_000))
            .unwrap();
        set_force_block(&cwd);
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "poison ALWAYS wins over a lease"
        );
    }

    #[test]
    fn operation_lease_invalid_missing_or_malformed_fields() {
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let now = operation_lease::now_ms();
        // Missing operation_id.
        let mut v = lease_value(&cwd, now - 1000, now + 300_000);
        v.as_object_mut().unwrap().remove("operation_id");
        operation_lease::write_operation(&cwd, &v).unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "missing field"
        );
        // Missing approved_generation.
        let mut v = lease_value(&cwd, now - 1000, now + 300_000);
        v.as_object_mut().unwrap().remove("approved_generation");
        operation_lease::write_operation(&cwd, &v).unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "missing generation"
        );
        // Empty globs.
        let mut v = lease_value(&cwd, now - 1000, now + 300_000);
        v["approved_allowed_globs"] = json!([]);
        operation_lease::write_operation(&cwd, &v).unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "empty globs"
        );
        // Non-array globs.
        let mut v = lease_value(&cwd, now - 1000, now + 300_000);
        v["approved_allowed_globs"] = json!("src/a.rs");
        operation_lease::write_operation(&cwd, &v).unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "non-array globs"
        );
    }

    #[test]
    fn operation_lease_invalid_time_bounds() {
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let now = operation_lease::now_ms();
        // Expired (now > expires_at).
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now - 10_000, now - 5_000))
            .unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "expired"
        );
        // Over-long TTL (window > MAX_TTL).
        operation_lease::write_operation(
            &cwd,
            &lease_value(&cwd, now - 1000, now + operation_lease::MAX_TTL_MS + 5000),
        )
        .unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "over-TTL"
        );
        // Future created_at == backward-clock rollback (now < created_at) → fail closed.
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now + 50_000, now + 350_000))
            .unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "future created_at / clock rollback fails closed"
        );
        // expires_at < created_at.
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now - 1000, now - 2000)).unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "expires<created"
        );
    }

    #[test]
    fn operation_lease_invalid_epoch_or_approval_mismatch() {
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let now = operation_lease::now_ms();
        for field in [
            "epoch",
            "approved_epoch",
            "approved_plan_hash",
            "approved_generation",
        ] {
            let mut v = lease_value(&cwd, now - 1000, now + 300_000);
            v[field] = if field == "approved_plan_hash" || field == "approved_generation" {
                json!(999_999u64)
            } else {
                json!("stale-mismatch")
            };
            operation_lease::write_operation(&cwd, &v).unwrap();
            assert!(
                !operation_lease::operation_lease_valid_with(&cwd, true),
                "{field} mismatch vs live state → invalid"
            );
        }
    }

    #[test]
    fn approved_generation_is_monotonic_across_approvals() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        real_scoped_approve(&cwd);
        let g1 = read_state(&cwd).unwrap()["approved_generation"]
            .as_u64()
            .unwrap();
        revoke(&cwd, "x");
        real_scoped_approve(&cwd);
        let g2 = read_state(&cwd).unwrap()["approved_generation"]
            .as_u64()
            .unwrap();
        assert!(g2 > g1, "each approval strictly increases the generation");
    }

    #[test]
    fn operation_lease_invalid_after_revoke_and_reapprove_via_record() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        real_scoped_approve(&cwd); // generation 1, scope enforced
        let now = operation_lease::now_ms();
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now - 1000, now + 300_000))
            .unwrap();
        assert!(
            operation_lease::operation_lease_valid_with(&cwd, true),
            "fresh lease bound to the live approval is valid"
        );
        revoke(&cwd, "scope changed");
        real_scoped_approve(&cwd); // SAME epoch/plan/scope, generation 2
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "a revoke + same-plan re-approve (record) must NOT revive the stale lease"
        );
    }

    #[test]
    fn operation_lease_invalid_after_revoke_and_reapprove_via_record_resume() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        start_epoch(&cwd, "sess", "task");
        real_scoped_approve(&cwd); // generation 1, scope enforced
        let now = operation_lease::now_ms();
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now - 1000, now + 300_000))
            .unwrap();
        assert!(operation_lease::operation_lease_valid_with(&cwd, true));
        revoke(&cwd, "scope changed");
        // Re-approve via the RECEIPT fast-path — a SEPARATE writer that does not touch `rounds`, so
        // only the monotonic generation guards it. Same epoch/plan/scope as the lease.
        let epoch = current_epoch(&cwd);
        assert!(record_resume(
            &cwd,
            &epoch,
            "do work\nALLOWED-GLOBS: src/a.rs",
            &[],
            &["src/a.rs".to_string()],
        ));
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "a revoke + same-plan re-approve (record_resume) must NOT revive the stale lease"
        );
    }

    #[test]
    fn approved_generation_corrupt_or_overflow_fails_closed_and_neutralizes_prior_approval() {
        // A present-but-malformed generation (non-u64) AND an overflow (u64::MAX) must fail closed
        // WITHOUT minting (or wrapping to) a new generation, AND must neutralize a PRIOR effective
        // approval (not merely refuse the new one) so a corrupt approved state can't stay open.
        for bad in [json!(u64::MAX), json!("1"), json!({})] {
            for resume in [false, true] {
                let cwd = tmp();
                approve_with_scope_state(&cwd, json!(["src/a.rs"]), true); // approved + scoped, gen=1
                let mut s = read_state(&cwd).unwrap();
                set_field(&mut s, "approved_generation", bad.clone());
                write_state(&cwd, &s).unwrap();
                assert!(
                    is_effectively_approved(&cwd),
                    "precondition: corrupt-but-effective approval (bad={bad})"
                );
                let epoch = current_epoch(&cwd);
                let plan = "do work\nALLOWED-GLOBS: src/a.rs";
                begin_review(&cwd, plan);
                if resume {
                    assert!(!record_resume(
                        &cwd,
                        &epoch,
                        plan,
                        &[],
                        &["src/a.rs".to_string()]
                    ));
                } else {
                    assert!(matches!(
                        record(
                            &cwd,
                            &epoch,
                            plan,
                            &crate::gate::Verdict::Approve,
                            "ok\nSCOPE-APPROVED: src/a.rs"
                        ),
                        Outcome::NeedsInfo(_)
                    ));
                }
                assert!(
                    !is_effectively_approved(&cwd),
                    "a corrupt generation neutralizes the prior approval (bad={bad}, resume={resume})"
                );
            }
        }
    }

    #[test]
    fn operation_review_pending_set_propagates_write_failure() {
        let cwd = tmp();
        enable(&cwd).unwrap();
        FORCE_WRITE_FAIL.with(|c| c.set(true));
        let r = operation_lease::set_operation_review_pending(&cwd);
        FORCE_WRITE_FAIL.with(|c| c.set(false));
        assert!(
            r.is_err(),
            "the durable obligation must surface a write failure (caller fails closed)"
        );
    }

    #[test]
    fn operation_lease_invalid_scope_not_enforced_or_glob_mismatch() {
        // Live approval with NO enforced scope (declared-but-empty) but the lease fabricates globs.
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!([]), true); // DenyAll → scope_is_enforced false
        let now = operation_lease::now_ms();
        let mut v = lease_value(&cwd, now - 1000, now + 300_000);
        v["approved_allowed_globs"] = json!(["src/forged.rs"]); // fabricated non-empty scope
        operation_lease::write_operation(&cwd, &v).unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "no live enforced scope → invalid even with fabricated lease globs"
        );
        // Live scope enforced, but the lease globs DIFFER from the live approved scope.
        let cwd2 = tmp();
        approve_with_scope_state(&cwd2, json!(["src/a.rs"]), true);
        let mut v = lease_value(&cwd2, now - 1000, now + 300_000);
        v["approved_allowed_globs"] = json!(["src/other.rs"]);
        operation_lease::write_operation(&cwd2, &v).unwrap();
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd2, true),
            "lease globs != live approved globs → invalid"
        );
    }

    #[test]
    fn operation_lease_invalid_when_revoked() {
        let cwd = tmp();
        approve_with_scope_state(&cwd, json!(["src/a.rs"]), true);
        let now = operation_lease::now_ms();
        operation_lease::write_operation(&cwd, &lease_value(&cwd, now - 1000, now + 300_000))
            .unwrap();
        assert!(operation_lease::operation_lease_valid_with(&cwd, true));
        revoke(&cwd, "test"); // approval no longer effective
        assert!(
            !operation_lease::operation_lease_valid_with(&cwd, true),
            "a revoked approval invalidates the lease"
        );
    }
}
