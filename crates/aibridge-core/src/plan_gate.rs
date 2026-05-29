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

/// Scan free text (a command OR a plan) for ALL high-risk command CLASSES present.
/// Token-based (not raw substring) so `warm -reset`/`git pushd`/a path containing a
/// risk phrase don't false-trigger, and `git.exe push`/`/bin/rm -rf` aren't missed.
/// Shell separators are flattened so each sub-command's program is matched on its
/// own; surrounding quotes/backticks/punctuation are trimmed so prose like
/// "run `git push`" classifies too. False positives only cost one extra plan round.
fn scan_risk_classes(text: &str) -> Vec<&'static str> {
    let lowered = text.to_lowercase();
    let mut out: Vec<&'static str> = Vec::new();
    if is_pipe_to_shell(&lowered) {
        push_unique(&mut out, "pipe-to-shell");
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
        push_unique(&mut out, "destructive-fs");
    }

    // remote publish / deploy
    if (has_cmd("git") && has("push"))
        || (has_any(&["npm", "yarn", "pnpm", "bun"]) && has("publish"))
        || (has_cmd("cargo") && has("publish"))
        || (has_cmd("gh") && has("release") && has_any(&["create", "upload", "edit", "delete"]))
        || (has_cmd("docker") && has("push"))
        || (has_any(&["vercel", "netlify", "wrangler", "firebase", "fly", "flyctl"])
            && has("deploy"))
        || (has_cmd("vercel") && has("--prod"))
    {
        push_unique(&mut out, "remote-publish");
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
        push_unique(&mut out, "db-migration");
    }

    // infrastructure mutation
    if (has_cmd("terraform") && has_any(&["apply", "destroy"]))
        || (has_cmd("pulumi") && has_any(&["up", "destroy"]))
        || (has_cmd("kubectl") && has_any(&["apply", "delete"]))
    {
        push_unique(&mut out, "infra-mutation");
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

/// Command classes already authorized by the current approved plan.
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

/// Post-approval risk class that the approved plan did NOT cover, for a Bash/run
/// command — `None` when the command is ordinary, its class is already approved,
/// the gate is off/bypassed, or the plan isn't approved (pre-approval is already
/// blocked by [`enforce`]). Does not mutate state.
pub fn unapproved_high_risk(cwd: &str, tool_name: &str, command: &str) -> Option<&'static str> {
    if !is_enabled(cwd) || bypassed() {
        return None;
    }
    if tool_name != "Bash" && tool_name != "mcp__aibridge__run" {
        return None;
    }
    if !effectively_approved(cwd) {
        return None;
    }
    // Check EVERY class in the command (a chained `git push && terraform destroy`
    // must not pass just because its FIRST class is approved). Deny on the first
    // class the approved plan did not authorize.
    let approved = approved_command_classes(cwd);
    scan_risk_classes(command)
        .into_iter()
        .find(|class| !approved.iter().any(|a| a == class))
}

/// PreToolUse risk gate (runs AFTER [`enforce`] returns allow): if an approved
/// task attempts an unapproved high-risk command, revoke approval and DENY so the
/// plan is re-reviewed with the command in scope. `None` to allow.
pub fn enforce_risk(cwd: &str, tool_name: &str, command: &str) -> Option<String> {
    let class = unapproved_high_risk(cwd, tool_name, command)?;
    revoke(cwd, "high_risk_command_delta");
    Some(risk_deny_json(class))
}

/// Human-readable instruction for a high-risk re-gate (shared by the hook deny and
/// the `run` tool's plain-text reply).
pub fn risk_delta_message(class: &str) -> String {
    format!(
        "PLAN_RISK_DELTA_REQUIRED: this command is a high-risk class ('{class}') that the \
         approved plan did not cover, so the plan gate has re-armed. Do NOT retry this command. \
         Update your plan to name this exact command under risk_surfaces (e.g. `git push`, \
         `prisma migrate deploy`, `rm -rf`), call `mcp__aibridge__plan_gate` again, and once it \
         returns <AI-BRIDGE-APPROVE/> this command class is allowed for the task."
    )
}

fn risk_deny_json(class: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": risk_delta_message(class)
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
            // Pre-authorize ONLY the high-risk classes the REVIEWER explicitly
            // allowed (its `RISK-APPROVED:` line) — never inferred from plan prose.
            // Keep the plan text for the Stop gate's scope-vs-diff comparison.
            set_field(
                &mut s,
                "approved_command_classes",
                json!(parse_risk_approved(findings)),
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
/// Mirrors [`record`]'s Approve path, but the command classes come from the RECEIPT
/// (a real reviewer's prior `RISK-APPROVED`), never re-parsed — so a resume can't
/// grant a class that was never reviewed. Same fail-safe state contract + epoch
/// TOCTOU guard as `record`. Returns `true` only if approval was actually recorded;
/// `false` (task changed / missing state) means the caller must run a full review.
pub fn record_resume(cwd: &str, expected_epoch: &str, plan: &str, classes: &[String]) -> bool {
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
    set_field(&mut s, "approved", json!(true));
    set_field(&mut s, "approved_epoch", json!(epoch));
    set_field(&mut s, "approved_plan_hash", json!(hash_str(plan)));
    set_field(&mut s, "approved_command_classes", json!(classes));
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
         infra-mutation, pipe-to-shell>\n\
         Only list a class the plan genuinely needs; if the plan says NOT to run such a command, \
         do NOT list it. Unlisted high-risk commands will be re-gated before they run.\n\
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
        // then record_resume approves from the receipt's classes (no Codex round).
        begin_review(&cwd, "plan A");
        assert!(record_resume(
            &cwd,
            &epoch,
            "plan A",
            &["remote-publish".to_string()]
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
}
