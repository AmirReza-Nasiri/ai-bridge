//! Warm Codex peer: a long-lived `codex mcp-server` child reached over stdio
//! JSON-RPC. The first prompt opens a conversation via the `codex` tool; later
//! prompts continue it via `codex-reply` so Codex's prompt cache is reused
//! (measured ~99% cached, ~2.4s vs a cold ~51K-token one-shot).

use aibridge_platform::{DefaultPlatform, Platform};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

/// A warm, reusable connection to a `codex mcp-server` child process.
pub struct CodexPeer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    thread_id: Option<String>,
}

impl CodexPeer {
    /// Spawn `codex mcp-server`, perform the MCP handshake, and keep it warm.
    pub fn spawn() -> Result<Self> {
        let exe = DefaultPlatform::find_executable("codex").context("locating codex")?;
        let mut child = DefaultPlatform::command_for(&exe)
            .arg("mcp-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning `codex mcp-server`")?;

        let stdin = child
            .stdin
            .take()
            .context("codex child stdin unavailable")?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .context("codex child stdout unavailable")?,
        );

        // Drain stderr in the background so a full pipe never blocks the child.
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = String::new();
                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                    buf.clear();
                }
            });
        }

        let mut peer = CodexPeer {
            child,
            stdin,
            stdout,
            next_id: 1,
            thread_id: None,
        };
        peer.initialize()?;
        Ok(peer)
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "aibridge", "version": crate::version() }
            }),
        )?;
        self.notify("notifications/initialized")?;
        Ok(())
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.write_msg(&json!({ "jsonrpc": "2.0", "method": method, "params": {} }))
    }

    fn write_msg(&mut self, msg: &Value) -> Result<()> {
        self.stdin.write_all(msg.to_string().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_msg(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;

        let mut line = String::new();
        loop {
            line.clear();
            if self.stdout.read_line(&mut line)? == 0 {
                return Err(anyhow!("codex closed stdout before responding"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue, // skip non-JSON / progress noise
            };
            if msg.get("id").and_then(Value::as_i64) != Some(id) {
                continue; // notification or a different id
            }
            if let Some(err) = msg.get("error") {
                return Err(anyhow!("codex error: {err}"));
            }
            return msg
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("codex response missing `result`"));
        }
    }

    /// Send a prompt to the warm peer and return its text reply. Uses `codex`
    /// for the first turn (capturing the conversation id) and `codex-reply`
    /// afterwards for cache reuse.
    pub fn ask(&mut self, prompt: &str, cwd: &str) -> Result<String> {
        let result = match self.thread_id.clone() {
            Some(thread_id) => self.request(
                "tools/call",
                json!({
                    "name": "codex-reply",
                    "arguments": { "threadId": thread_id, "prompt": prompt }
                }),
            )?,
            None => {
                let result = self.request(
                    "tools/call",
                    json!({
                        "name": "codex",
                        "arguments": {
                            "prompt": prompt,
                            "sandbox": "read-only",
                            "approval-policy": "never",
                            "cwd": cwd
                        }
                    }),
                )?;
                if let Some(tid) = result
                    .pointer("/structuredContent/threadId")
                    .or_else(|| result.pointer("/structuredContent/conversationId"))
                    .and_then(Value::as_str)
                {
                    self.thread_id = Some(tid.to_string());
                }
                result
            }
        };
        Ok(extract_text(&result))
    }
}

impl Drop for CodexPeer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn extract_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}
