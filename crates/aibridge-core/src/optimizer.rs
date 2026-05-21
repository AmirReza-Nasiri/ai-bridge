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

/// Conservative allowlist of read-only, high-noise commands worth compressing.
const SAFE_PREFIXES: &[&str] = &[
    "git status",
    "git diff",
    "git log",
    "git branch",
    "git show",
    "ls",
    "dir",
    "tree",
];

/// Parse a PreToolUse hook payload (raw stdin) and return the hook-output JSON.
pub fn pretooluse_str(stdin: &str) -> String {
    let v: Value = serde_json::from_str(stdin).unwrap_or_else(|_| json!({}));
    pretooluse(&v)
}

/// Decide a PreToolUse rewrite. Returns `{}` (no change) or an `updatedInput`
/// rewrite that routes the command through `rtk`.
pub fn pretooluse(hook_input: &Value) -> String {
    let no_change = "{}".to_string();
    if hook_input.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return no_change;
    }
    let tool_input = match hook_input.get("tool_input") {
        Some(v) => v,
        None => return no_change,
    };
    let command = tool_input
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("");
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
    if !out.status.success() {
        return None;
    }
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
