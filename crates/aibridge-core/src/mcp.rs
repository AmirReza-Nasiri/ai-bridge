//! MCP stdio server: newline-delimited JSON-RPC 2.0.
//!
//! Increment: `consult` is wired to the warm Codex peer; review tools land next.

use crate::codex::CodexPeer;
use crate::health;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

enum Handled {
    Result(Value),
    Error(i64, String),
    Notification,
}

/// Holds the warm Codex peer across requests (spawned lazily on first use).
struct Server {
    codex: Option<CodexPeer>,
}

impl Server {
    fn new() -> Self {
        Server { codex: None }
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
            "review_diff" | "review_stop" => {
                format!("AI Bridge: `{name}` is not wired yet (next increment). `consult` is live.")
            }
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
