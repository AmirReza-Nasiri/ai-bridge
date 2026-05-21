//! MCP stdio server: newline-delimited JSON-RPC 2.0.
//!
//! Increment: `consult` is wired to the warm Codex peer; review tools land next.

use crate::codex::CodexPeer;
use crate::{gate, health};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

enum Handled {
    Result(Value),
    Error(i64, String),
    Notification,
}

/// Holds the warm Codex peer + per-(workspace,session) gate state across requests.
struct Server {
    codex: Option<CodexPeer>,
    gates: HashMap<String, gate::GateState>,
}

impl Server {
    fn new() -> Self {
        Server {
            codex: None,
            gates: HashMap::new(),
        }
    }

    fn peer(&mut self) -> anyhow::Result<&mut CodexPeer> {
        if self.codex.is_none() {
            self.codex = Some(CodexPeer::spawn()?);
        }
        self.codex
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("codex peer unavailable"))
    }

    fn handle(&mut self, method: &str, msg: &Value) -> Handled {
        match method {
            "initialize" => Handled::Result(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "aibridge", "version": crate::version() }
            })),
            "notifications/initialized" => Handled::Notification,
            "tools/list" => Handled::Result(json!({ "tools": tools() })),
            "tools/call" => Handled::Result(self.call_tool(msg)),
            other => Handled::Error(-32601, format!("method not found: {other}")),
        }
    }

    fn call_tool(&mut self, msg: &Value) -> Value {
        let name = msg
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = match name {
            "health" => health::report(),
            "capability_status" => health::capability_report(),
            "budget_status" => {
                "AI Bridge: no active review budget state yet (foundation).".to_string()
            }
            "consult" => {
                let question = msg
                    .pointer("/params/arguments/question")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if question.is_empty() {
                    "AI Bridge: `consult` requires a non-empty 'question' argument.".to_string()
                } else {
                    self.consult(question)
                }
            }
            "review_diff" => self.review_diff(),
            "review_stop" => self.review_stop(msg),
            other => format!("AI Bridge: unknown tool '{other}'."),
        };
        json!({ "content": [{ "type": "text", "text": text }] })
    }

    fn consult(&mut self, question: &str) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let prompt = format!(
            "You are a peer reviewer giving a concise, skeptical second opinion. \
             Be specific and call out risks. Question:\n{question}"
        );
        match self.peer() {
            Ok(peer) => match peer.ask(&prompt, &cwd) {
                Ok(reply) if !reply.trim().is_empty() => reply,
                Ok(_) => "AI Bridge: Codex returned an empty reply.".to_string(),
                Err(e) => format!(
                    "AI Bridge: consult unavailable (Codex error): {e}. \
                     Proceed without it, retry, or fix the issue?"
                ),
            },
            Err(e) => format!(
                "AI Bridge: could not start the Codex peer: {e}. \
                 Proceed without it, retry, or fix the issue?"
            ),
        }
    }

    fn review_diff(&mut self) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let diff = match crate::git::diff(&cwd) {
            Ok(d) => d,
            Err(e) => return format!("AI Bridge: could not read the git diff: {e}"),
        };
        if diff.trim().is_empty() {
            return "AI Bridge: no uncommitted changes to review (git diff is empty).".to_string();
        }
        let diff = truncate_for_review(&diff);
        let prompt = format!(
            "You are a skeptical peer reviewer. Review this git diff: find bugs, risks, \
             edge cases, and missing tests; cite file/line; if it looks good, say so \
             briefly.\n\n```diff\n{diff}\n```"
        );
        match self.peer() {
            Ok(peer) => match peer.ask(&prompt, &cwd) {
                Ok(reply) if !reply.trim().is_empty() => reply,
                Ok(_) => "AI Bridge: Codex returned an empty review.".to_string(),
                Err(e) => format!(
                    "AI Bridge: review unavailable (Codex error): {e}. \
                     Proceed without it, retry, or fix the issue?"
                ),
            },
            Err(e) => format!(
                "AI Bridge: could not start the Codex peer: {e}. \
                 Proceed without it, retry, or fix the issue?"
            ),
        }
    }

    /// The automatic Stop gate. Returns the hook-decision JSON as a string:
    /// `{}` (allow) or `{"decision":"block","reason":...}`.
    fn review_stop(&mut self, msg: &Value) -> String {
        let args = msg.pointer("/params/arguments");
        let cwd = resolve_cwd(args);
        log_gate(&cwd, "INVOKED"); // entry marker: proves the hook reached us
        let stop_active = args
            .and_then(|a| a.get("stop_hook_active"))
            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
            .unwrap_or(false);
        let session = args
            .and_then(|a| a.get("session_id").or_else(|| a.get("transcript_path")))
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();

        let decision = self.review_stop_inner(&cwd, stop_active, &session);
        // Always-on observability: prove the gate fired and what it decided
        // (so it can never be a silent no-op).
        log_gate(
            &cwd,
            &format!(
                "stop_hook_active={stop_active} decision={}",
                if decision == "{}" { "ALLOW" } else { "BLOCK" }
            ),
        );
        decision
    }

    fn review_stop_inner(&mut self, cwd: &str, stop_active: bool, session: &str) -> String {
        if stop_active {
            return allow(); // already in a stop-hook continuation; don't re-gate
        }
        let bundle = match crate::git::diff_bundle(cwd) {
            Ok(b) => b,
            Err(_) => return allow(), // not a git repo / git missing: nothing to gate
        };
        if bundle.is_empty {
            return allow();
        }
        let key = format!("{cwd}::{session}");
        let mut state = self.gates.remove(&key).unwrap_or_default();
        let decision = self.gate_decide(&mut state, &bundle, cwd);
        self.gates.insert(key, state);
        decision
    }

    fn gate_decide(
        &mut self,
        st: &mut gate::GateState,
        bundle: &crate::git::DiffBundle,
        cwd: &str,
    ) -> String {
        let dh = bundle.hash;

        if st.last_allowed_diff_hash == Some(dh) {
            return allow();
        }
        if st.fail_ask_pending {
            st.fail_ask_pending = false;
            if st.last_blocked_diff_hash == Some(dh) {
                return allow(); // the ask was surfaced last turn; let Claude stop now
            }
            // diff changed — fall through and review normally
        }
        // Same blocked diff stopping again with nothing changed → no progress.
        if st.last_blocked_diff_hash == Some(dh) {
            st.same_findings_blocks += 1;
            if st.same_findings_blocks >= gate::NO_PROGRESS_THRESHOLD {
                return self.fail_ask(
                    st,
                    dh,
                    "the diff hasn't changed but peer review still has unresolved findings",
                );
            }
            if let Some(reason) = st.cached_block_reason.clone() {
                return block(&reason); // re-block with cached findings; no new Codex call
            }
        }

        let prompt = gate::prompt(&bundle.text);
        let review = match self.peer() {
            Ok(peer) => peer.ask(&prompt, cwd),
            Err(e) => Err(e),
        };
        let review = match review {
            Ok(r) => r,
            Err(_) => {
                return self.fail_ask(
                    st,
                    dh,
                    "peer review couldn't run (Codex unavailable or quota exhausted)",
                )
            }
        };
        let trace = gate::write_trace(cwd, &bundle.text, &review);

        match gate::parse_verdict(&review) {
            gate::Verdict::Approve => {
                st.last_allowed_diff_hash = Some(dh);
                st.last_blocked_diff_hash = None;
                st.cached_block_reason = None;
                st.same_findings_blocks = 0;
                allow()
            }
            gate::Verdict::RequestChanges => {
                let findings = gate::findings(&review);
                let fh = gate::hash_str(&findings);
                if st.last_findings_hash == Some(fh)
                    && st.last_blocked_diff_hash.is_some()
                    && st.last_blocked_diff_hash != Some(dh)
                {
                    st.same_findings_blocks += 1;
                    if st.same_findings_blocks >= gate::NO_PROGRESS_THRESHOLD {
                        return self.fail_ask(
                            st,
                            dh,
                            "the same findings persist even though the code changed",
                        );
                    }
                } else {
                    st.same_findings_blocks = 1;
                }
                let reason = gate::compact_reason(&findings, &trace);
                st.last_blocked_diff_hash = Some(dh);
                st.last_findings_hash = Some(fh);
                st.cached_block_reason = Some(reason.clone());
                block(&reason)
            }
            gate::Verdict::Blocked | gate::Verdict::Unparseable => self.fail_ask(
                st,
                dh,
                "peer review could not complete (blocked or unparseable verdict)",
            ),
        }
    }

    fn fail_ask(&mut self, st: &mut gate::GateState, dh: u64, why: &str) -> String {
        st.fail_ask_pending = true;
        st.last_blocked_diff_hash = Some(dh);
        block(&format!(
            "AI Bridge: {why}. I won't finalize on my own — ask the user how to proceed \
             (continue without review / wait and retry / fix it first). The next stop will be \
             allowed so you can deliver that question."
        ))
    }
}

/// Allow decision (let Claude finish).
fn allow() -> String {
    "{}".to_string()
}

/// Block decision JSON (send Claude back with `reason`).
fn block(reason: &str) -> String {
    json!({ "decision": "block", "reason": reason }).to_string()
}

/// Resolve the project directory for a review, robust to a missing or
/// unsubstituted `${cwd}` hook input: explicit arg → `CLAUDE_PROJECT_DIR` →
/// the MCP server's own working dir (Claude spawns it in the project). This
/// prevents the gate from silently allowing when `${cwd}` doesn't substitute.
fn resolve_cwd(args: Option<&Value>) -> String {
    if let Some(c) = args.and_then(|a| a.get("cwd")).and_then(Value::as_str) {
        if !c.is_empty() && !c.starts_with("${") && std::path::Path::new(c).exists() {
            return c.to_string();
        }
    }
    if let Ok(dir) = std::env::var("CLAUDE_PROJECT_DIR") {
        if !dir.is_empty() && std::path::Path::new(&dir).exists() {
            return dir;
        }
    }
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

/// Append a one-line record to `<cwd>/.ai-bridge/gate.log` so every gate
/// invocation (and its decision) is observable — never a silent no-op.
fn log_gate(cwd: &str, msg: &str) {
    let dir = std::path::Path::new(cwd).join(".ai-bridge");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join("gate.log");
    // Rotate at ~1 MB so a long-lived install never grows an unbounded log.
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, dir.join("gate.log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(format!("{ts} review_stop {msg}\n").as_bytes());
    }
}

/// Soft cap on diff size sent to the reviewer (avoids huge prompts; rtk-based
/// shrinking comes later).
const MAX_DIFF_CHARS: usize = 24_000;

fn truncate_for_review(diff: &str) -> String {
    match diff.char_indices().nth(MAX_DIFF_CHARS) {
        Some((idx, _)) => format!("{}\n... [diff truncated for review]", &diff[..idx]),
        None => diff.to_string(),
    }
}

/// Run the stdio JSON-RPC loop until EOF.
pub fn serve() -> anyhow::Result<()> {
    let mut server = Server::new();
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF: client closed the pipe.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = msg.get("id").cloned();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let reply = match (id, server.handle(&method, &msg)) {
            (Some(id), Handled::Result(result)) => {
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            (Some(id), Handled::Error(code, message)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
            }
            _ => continue, // notifications / no-id get no response
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn tools() -> Value {
    json!([
        tool_with(
            "review_diff",
            "Peer-review the current git diff with AI Bridge/Codex. Use when the user asks for \
             AI Bridge, a Codex review, a second AI review, or a review before shipping.",
            json!({ "type": "object", "properties": {} })
        ),
        tool_with(
            "consult",
            "Ask AI Bridge/Codex for a read-only second opinion on a plan, design, bug, or \
             tradeoff. Does not gate final output.",
            json!({
                "type": "object",
                "properties": { "question": { "type": "string", "description": "What to ask the Codex peer." } },
                "required": ["question"]
            })
        ),
        tool_with(
            "review_stop",
            "Hook-only. Reviews a final response/diff before Stop and returns allow/block JSON. \
             Do not call manually.",
            json!({ "type": "object", "properties": {} })
        ),
        tool_with(
            "health",
            "Check AI Bridge installation: Claude/Codex/rtk discovery and engine status.",
            json!({ "type": "object", "properties": {} })
        ),
        tool_with(
            "budget_status",
            "Show AI Bridge review budget, cooldown, and Codex quota-risk state.",
            json!({ "type": "object", "properties": {} })
        ),
        tool_with(
            "capability_status",
            "Show installed optional capabilities such as rtk and profile scoping.",
            json!({ "type": "object", "properties": {} })
        ),
    ])
}

fn tool_with(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}
