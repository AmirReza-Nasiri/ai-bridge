//! Minimal MCP stdio server: newline-delimited JSON-RPC 2.0.
//!
//! Foundation increment: a server Claude can connect to, exposing the v1 tool
//! surface with a real `health`/`capability_status`. The review/consult tools
//! are honest stubs until the warm Codex child lands in the next increment.

use crate::health;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

enum Handled {
    Result(Value),
    Error(i64, String),
    Notification,
}

/// Run the stdio JSON-RPC loop until EOF.
pub fn serve() -> anyhow::Result<()> {
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
            Err(_) => continue, // ignore non-JSON noise
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

        let reply = match (id, handle(method, &msg)) {
            (Some(id), Handled::Result(result)) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            (Some(id), Handled::Error(code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
            // Notifications (and anything without an id) get no response.
            _ => continue,
        };
        writeln!(stdout, "{reply}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle(method: &str, msg: &Value) -> Handled {
    match method {
        "initialize" => Handled::Result(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "aibridge", "version": crate::version() }
        })),
        "notifications/initialized" => Handled::Notification,
        "tools/list" => Handled::Result(json!({ "tools": tools() })),
        "tools/call" => Handled::Result(call_tool(msg)),
        other => Handled::Error(-32601, format!("method not found: {other}")),
    }
}

fn call_tool(msg: &Value) -> Value {
    let name = msg
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let text = match name {
        "health" => health::report(),
        "capability_status" => health::capability_report(),
        "budget_status" => "AI Bridge: no active review budget state yet (foundation).".to_string(),
        "review_diff" | "consult" | "review_stop" => format!(
            "AI Bridge: `{name}` is not wired to the warm Codex peer yet (foundation increment). \
             The MCP server, tool surface, and health checks are live; review lands next."
        ),
        other => format!("AI Bridge: unknown tool '{other}'."),
    };
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn tools() -> Value {
    json!([
        tool(
            "review_diff",
            "Peer-review the current git diff with AI Bridge/Codex. Use when the user asks for \
             AI Bridge, a Codex review, a second AI review, or a review before shipping."
        ),
        tool(
            "consult",
            "Ask AI Bridge/Codex for a read-only second opinion on a plan, design, bug, or \
             tradeoff. Does not gate final output."
        ),
        tool(
            "review_stop",
            "Hook-only. Reviews a final response/diff before Stop and returns allow/block JSON. \
             Do not call manually."
        ),
        tool(
            "health",
            "Check AI Bridge installation: Claude/Codex/rtk discovery and engine status."
        ),
        tool(
            "budget_status",
            "Show AI Bridge review budget, cooldown, and Codex quota-risk state."
        ),
        tool(
            "capability_status",
            "Show installed optional capabilities such as rtk and profile scoping."
        ),
    ])
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": {} }
    })
}
