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
    // 1. Pre-approval plan gate: deny writes/Bash until the plan is Codex-approved
    //    (deny wins; merged here so a denied pre-approval Bash is never also
    //    rtk-rewritten, per Codex review).
    if let Some(deny) = crate::plan_gate::enforce(cwd, tool_name) {
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
}
