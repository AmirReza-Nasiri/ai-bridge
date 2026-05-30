//! rtk output-optimizer wiring (SAFE MODE, Codex Round 30).
//!
//! A PreToolUse rewrite that delegates ONLY safe, read-only, high-noise
//! inspection commands to `rtk rewrite`, so compression never drops detail the
//! model needs. It never rewrites mutations, test/build/lint, diagnostics, or
//! compound commands, and it always fails open to the original command.
//!
//! rtk is orchestrated, not reimplemented; if `rtk` is absent the command runs
//! unchanged. Raw bypass: `AIBRIDGE_RTK=0`, `RTK_DISABLE=1`, or a
//! `# ai-bridge:raw` marker in the command.

use aibridge_platform::{DefaultPlatform, Platform};
use serde_json::{json, Value};

/// Conservative allowlist of read-only, high-noise commands whose output is
/// structural/navigational — safe to compress. Deliberately EXCLUDES `git diff`
/// and `git show` (no `--stat`): their output IS the content Claude reasons about,
/// so compressing it could hide a real change/bug (Codex Round 31).
const SAFE_PREFIXES: &[&str] = &[
    "git status",
    "git diff --stat", // file-level summary only — never the full diff
    "git log",
    "git branch",
    "ls",
    "dir",
    "tree",
];

/// Parse a PreToolUse hook payload (raw stdin) and return the hook-output JSON.
pub fn pretooluse_str(stdin: &str) -> String {
    let v: Value = serde_json::from_str(stdin).unwrap_or_else(|_| json!({}));
    pretooluse(&v)
}

/// Decide a PreToolUse outcome. First enforces the plan gate (default-on) — denying a
/// write/Bash tool until the task's plan is Codex-approved — then, for an allowed
/// Bash command, returns `{}` (no change) or an `updatedInput` rtk rewrite.
pub fn pretooluse(hook_input: &Value) -> String {
    let no_change = "{}".to_string();
    let tool_name = hook_input
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    // The hook payload carries cwd + tool_input.
    let cwd = hook_input.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let tool_input_opt = hook_input.get("tool_input");
    let command = tool_input_opt
        .and_then(|t| t.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // v0.32 Unit 4: extract the authoritative write target for the post-approval scope fence.
    // Read ONLY the field this tool actually uses (NotebookEdit → `notebook_path`; every other
    // gated write tool → `file_path`) so a payload carrying BOTH cannot trick the fence into
    // checking the wrong path. A present non-empty value is the target; an absent/empty/non-string
    // one becomes `Missing` so the fence fails closed under an active scope. Non-write tools
    // carry no target.
    let target = if crate::plan_gate::GATED_WRITE_TOOLS.contains(&tool_name) {
        let field = if tool_name == "NotebookEdit" {
            "notebook_path"
        } else {
            "file_path"
        };
        match tool_input_opt
            .and_then(|t| t.get(field))
            .and_then(Value::as_str)
        {
            Some(p) if !p.is_empty() => crate::plan_gate::WriteTarget::Path(p),
            _ => crate::plan_gate::WriteTarget::Missing,
        }
    } else {
        crate::plan_gate::WriteTarget::Unknown
    };
    // 1. Pre-approval plan gate: deny writes/Bash until the plan is Codex-approved
    //    (deny wins; merged here so a denied pre-approval Bash is never also
    //    rtk-rewritten, per Codex review). Post-approval, the same call applies the
    //    v0.32 write-time scope fence for path-bearing write tools.
    if let Some(deny) = crate::plan_gate::enforce_tool_scoped(cwd, tool_name, target, command) {
        return deny;
    }
    // 2. Post-approval risk delta: an APPROVED task attempting an unapproved
    //    high-risk command (publish/deploy/migration/destructive shell) re-arms the
    //    gate so the plan is re-reviewed with that command in scope. AI Bridge does
    //    NOT hard-fence ordinary file writes (self-reported scope is a weak boundary
    //    that would train users to disable the gate); instead the approved plan is
    //    fed to the Stop gate, which compares it against the actual diff.
    if let Some(deny) = crate::plan_gate::enforce_risk(cwd, tool_name, command) {
        return deny;
    }
    if tool_name != "Bash" {
        return no_change;
    }
    let tool_input = match tool_input_opt {
        Some(v) => v,
        None => return no_change,
    };
    if command.is_empty() || raw_bypassed(command) || !is_rtk_safe(command) {
        return no_change;
    }
    match rtk_rewrite(command) {
        Some(rewritten) => {
            let mut new_input = tool_input.clone();
            if let Some(obj) = new_input.as_object_mut() {
                obj.insert("command".to_string(), json!(rewritten));
            }
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "updatedInput": new_input
                }
            })
            .to_string()
        }
        None => no_change, // fail-open: run the original command
    }
}

fn raw_bypassed(command: &str) -> bool {
    std::env::var("AIBRIDGE_RTK")
        .map(|v| v == "0")
        .unwrap_or(false)
        || std::env::var("RTK_DISABLE")
            .map(|v| v == "1")
            .unwrap_or(false)
        || command.contains("# ai-bridge:raw")
}

/// Only safe, read-only, high-noise inspection commands, and never when the
/// command is compound / redirected / a subshell / a substitution.
fn is_rtk_safe(command: &str) -> bool {
    let c = command.trim();
    if c.contains("&&")
        || c.contains("||")
        || c.contains(';')
        || c.contains('|')
        || c.contains('>')
        || c.contains('<')
        || c.contains("$(")
        || c.contains('`')
    {
        return false;
    }
    SAFE_PREFIXES
        .iter()
        .any(|p| c == *p || c.starts_with(&format!("{p} ")))
}

fn rtk_rewrite(command: &str) -> Option<String> {
    let rtk = DefaultPlatform::find_executable("rtk").ok()?;
    let out = DefaultPlatform::command_for(&rtk)
        .arg("rewrite")
        .arg(command)
        .output()
        .ok()?;
    // Trust stdout, not the exit code: rtk prints the rewrite to stdout but may
    // exit non-zero (e.g. 3, a "no hook installed" nudge) while still emitting a
    // valid rewrite; it exits 1 with empty stdout when there is no equivalent.
    let rewritten = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // Accept only a non-empty rewrite of the same top-level command category.
    let first = command.split_whitespace().next().unwrap_or("");
    if rewritten.is_empty() || rewritten == command || !rewritten.contains(first) {
        return None;
    }
    Some(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(c: &str) -> Value {
        json!({"tool_name": "Bash", "tool_input": {"command": c}})
    }

    #[test]
    fn rejects_non_bash() {
        assert_eq!(pretooluse(&json!({"tool_name": "Read"})), "{}");
    }

    #[test]
    fn rejects_unsafe_commands() {
        // mutations / tests / compound / diagnostics are never rtk-safe
        assert!(!is_rtk_safe("git commit -m x"));
        assert!(!is_rtk_safe("rm -rf build"));
        assert!(!is_rtk_safe("pytest -q"));
        assert!(!is_rtk_safe("cargo test"));
        assert!(!is_rtk_safe("git status && rm x"));
        assert!(!is_rtk_safe("git diff | head"));
        assert!(!is_rtk_safe("cat err.log"));
        assert!(!is_rtk_safe("lsof -i"));
        // full diff content must stay raw — only `git diff --stat` is safe
        assert!(!is_rtk_safe("git diff"));
        assert!(!is_rtk_safe("git diff -p"));
        assert!(!is_rtk_safe("git show HEAD"));
    }

    #[test]
    fn accepts_safe_inspection() {
        assert!(is_rtk_safe("git status"));
        assert!(is_rtk_safe("git diff --stat"));
        assert!(is_rtk_safe("git log --oneline -20"));
        assert!(is_rtk_safe("ls -la"));
        assert!(is_rtk_safe("tree src"));
    }

    #[test]
    fn unsafe_command_is_passthrough() {
        // even if rtk were present, an unsafe command must not be rewritten
        assert_eq!(pretooluse(&cmd("rm -rf build")), "{}");
    }

    #[test]
    fn pretooluse_threads_command_to_plan_gate_but_p2_is_behavior_neutral() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let cwd = std::env::temp_dir().join(format!(
            "aibridge-optimizer-p2-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.display().to_string();
        // Enable an unapproved gate: a read-only-looking Bash command must STILL be
        // denied pre-approval (the P2 carve-out is inert), and the deny carries the
        // default no_active_approval code — proving the command is threaded through
        // enforce_tool without yet enabling any pre-approval allowance.
        crate::plan_gate::enable(&cwd).unwrap();
        crate::plan_gate::start_epoch(&cwd, "sess", "task");
        let payload = json!({
            "tool_name": "Bash",
            "cwd": cwd,
            "tool_input": {"command": "ls -la src"}
        });
        let out = pretooluse(&payload);
        let v: Value = serde_json::from_str(&out).expect("deny is valid json");
        let reason = v
            .pointer("/hookSpecificOutput/permissionDecisionReason")
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            reason.contains("PLAN_GATE_REQUIRED: no_active_approval"),
            "flag-OFF / inert carve-out keeps the strict default, got: {reason}"
        );
    }

    // ── v0.32 Unit 4: pretooluse extracts the write target + applies the scope fence ─────
    //
    // An APPROVED epoch whose reviewer-approved scope is exactly `src/a.rs` (declared in the
    // plan, echoed by the reviewer).
    fn scoped_cwd() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let cwd = std::env::temp_dir().join(format!(
            "aibridge-opt-scope-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.display().to_string();
        crate::plan_gate::enable(&cwd).unwrap();
        crate::plan_gate::start_epoch(&cwd, "sess", "task");
        let ep = crate::plan_gate::current_epoch(&cwd);
        assert!(matches!(
            crate::plan_gate::record(
                &cwd,
                &ep,
                "ALLOWED-GLOBS: src/a.rs",
                &crate::gate::Verdict::Approve,
                "SCOPE-APPROVED: src/a.rs",
            ),
            crate::plan_gate::Outcome::Approved
        ));
        cwd
    }

    #[test]
    fn pretooluse_allows_in_scope_write() {
        let cwd = scoped_cwd();
        let out = pretooluse(&json!({
            "tool_name": "Write",
            "cwd": cwd,
            "tool_input": {"file_path": "src/a.rs"}
        }));
        assert_eq!(out, "{}", "in-scope write must pass through unchanged");
    }

    #[test]
    fn pretooluse_denies_out_of_scope_write() {
        let cwd = scoped_cwd();
        let out = pretooluse(&json!({
            "tool_name": "Write",
            "cwd": cwd,
            "tool_input": {"file_path": "src/b.rs"}
        }));
        assert!(out.contains("out_of_scope_path"), "got: {out}");
        assert!(out.contains("\"deny\""));
    }

    #[test]
    fn pretooluse_denies_write_with_missing_path_under_scope() {
        // A gated write tool whose authoritative payload has no `file_path` fails closed.
        let cwd = scoped_cwd();
        let out = pretooluse(&json!({
            "tool_name": "Write",
            "cwd": cwd,
            "tool_input": {}
        }));
        assert!(out.contains("out_of_scope_path"), "got: {out}");
    }

    #[test]
    fn pretooluse_extracts_notebook_path() {
        // NotebookEdit carries `notebook_path` (not `file_path`); an out-of-scope one denies.
        let cwd = scoped_cwd();
        let out = pretooluse(&json!({
            "tool_name": "NotebookEdit",
            "cwd": cwd,
            "tool_input": {"notebook_path": "src/b.ipynb"}
        }));
        assert!(out.contains("out_of_scope_path"), "got: {out}");
    }

    #[test]
    fn pretooluse_leaves_bash_free_under_scope() {
        // Owner-accepted residual: Bash is not fenced (the Stop-gate is its backstop).
        let cwd = scoped_cwd();
        let out = pretooluse(&json!({
            "tool_name": "Bash",
            "cwd": cwd,
            "tool_input": {"command": "echo hello"}
        }));
        assert!(!out.contains("out_of_scope_path"), "Bash must not be scope-fenced, got: {out}");
        assert!(!out.contains("\"deny\""), "got: {out}");
    }

    #[test]
    fn pretooluse_denies_all_when_scope_declared_but_unapproved() {
        // End-to-end: a plan DECLARES `src/**` but the reviewer never grants `broad-scope`, so
        // the recursive glob is dropped and ZERO globs are approved. The fence must DENY every
        // write (declared-but-empty ⇒ DenyAll) — NOT treat the empty scope as "no scope".
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let cwd = std::env::temp_dir().join(format!(
            "aibridge-opt-emptyscope-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.display().to_string();
        crate::plan_gate::enable(&cwd).unwrap();
        crate::plan_gate::start_epoch(&cwd, "sess", "task");
        let ep = crate::plan_gate::current_epoch(&cwd);
        assert!(matches!(
            crate::plan_gate::record(
                &cwd,
                &ep,
                "ALLOWED-GLOBS: src/**",
                &crate::gate::Verdict::Approve,
                "SCOPE-APPROVED: src/**", // echoed, but NO `RISK-APPROVED: broad-scope` → dropped
            ),
            crate::plan_gate::Outcome::Approved
        ));
        let out = pretooluse(&json!({
            "tool_name": "Write",
            "cwd": cwd,
            "tool_input": {"file_path": "secrets.txt"}
        }));
        assert!(out.contains("out_of_scope_path"), "declared-but-empty scope must deny, got: {out}");
    }

    #[test]
    fn pretooluse_notebook_edit_ignores_file_path_field() {
        // A NotebookEdit payload carrying BOTH an in-scope `file_path` and an out-of-scope
        // `notebook_path` must be checked on `notebook_path` (the field it actually writes).
        let cwd = scoped_cwd(); // scope = src/a.rs
        let out = pretooluse(&json!({
            "tool_name": "NotebookEdit",
            "cwd": cwd,
            "tool_input": {"file_path": "src/a.rs", "notebook_path": "outside.ipynb"}
        }));
        assert!(out.contains("out_of_scope_path"), "must check notebook_path, not file_path; got: {out}");
    }
}
