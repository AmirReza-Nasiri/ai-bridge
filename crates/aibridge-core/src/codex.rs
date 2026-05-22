//! Warm Codex peer: a long-lived `codex mcp-server` child reached over stdio
//! JSON-RPC. The first prompt opens a conversation via the `codex` tool; later
//! prompts continue it via `codex-reply` so Codex's prompt cache is reused
//! (measured ~99% cached, ~2.4s vs a cold ~51K-token one-shot).
//!
//! Reads are deadline-bounded via a reader thread + channel: a wedged or silent
//! Codex must surface as an error the gate turns into fail-ask, never an
//! infinite hang. On Windows the child is launched directly (resolving the npm
//! `.cmd` shim to `node <entry>.js`) so a no-console MCP host never deadlocks on
//! `cmd /C` (see `aibridge_platform::Platform::spawn_plan`).

use aibridge_platform::{DefaultPlatform, Platform};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Deadline for the MCP handshake (`initialize`). Generous enough for a cold
/// `codex mcp-server` start, short enough that a wedged child fails fast.
const INIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Generous backstop for a `tools/call` (a review round-trip). This is NOT a
/// quality cutoff: the user prioritizes review depth over speed, so a thorough
/// `xhigh` review (~5 min, more on large diffs) must run to completion. It only
/// guards against a Codex that is alive but silent forever — a *crashed* Codex is
/// caught instantly by EOF on the reader channel (independent of this deadline),
/// which is what keeps the original infinite-hang fixed. Real reviews never reach
/// this; it just ensures the editor can't block indefinitely on a frozen child.
const CALL_TIMEOUT: Duration = Duration::from_secs(1500);

/// Reasoning effort for AI Bridge's own Codex sessions, set on the first turn and
/// inherited by later `codex-reply` turns. Independent of the user's global
/// `~/.codex/config.toml`. The user prioritizes accuracy: `xhigh` gives the most
/// thorough review (measured ~286s on a real ~30KB diff vs ~22s at `medium`) and
/// the timeout backstop is sized so it always completes. Tune here for a faster,
/// shallower gate (`high` / `medium` / `low`).
const REVIEW_REASONING_EFFORT: &str = "xhigh";

/// The reasoning effort AI Bridge applies to its reviews (for `doctor` to report
/// the expected quality/latency so a slow review isn't mistaken for a hang).
pub fn review_reasoning_effort() -> &'static str {
    REVIEW_REASONING_EFFORT
}

/// A message handed from the reader thread to the request side.
enum FromCodex {
    /// A parsed JSON-RPC line from the child.
    Message(Value),
    /// The child closed stdout (exited or stdout pipe broke).
    Eof,
}

/// A warm, reusable connection to a `codex mcp-server` child process.
pub struct CodexPeer {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<FromCodex>,
    next_id: i64,
    thread_id: Option<String>,
    spawn_kind: &'static str,
    spawn_program: String,
}

impl CodexPeer {
    /// Spawn `codex mcp-server`, perform the MCP handshake, and keep it warm.
    pub fn spawn() -> Result<Self> {
        let exe = DefaultPlatform::find_executable("codex").context("locating codex")?;
        let plan = DefaultPlatform::spawn_plan(&exe);
        let spawn_kind = plan.kind.as_str();
        let spawn_program = plan.program.clone();
        let mut child = plan
            .into_command()
            .arg("mcp-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning codex mcp-server [{spawn_kind}] {spawn_program}"))?;

        let stdin = child
            .stdin
            .take()
            .context("codex child stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("codex child stdout unavailable")?;

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

        // Reader thread: forward each parsed JSON line over a channel so the
        // request side can apply a deadline instead of blocking forever.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(FromCodex::Eof);
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(trimmed) {
                            // Forward JSON; stop if the request side has gone away.
                            Ok(v) => {
                                if tx.send(FromCodex::Message(v)).is_err() {
                                    break;
                                }
                            }
                            // Skip non-JSON progress/log noise.
                            Err(_) => continue,
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(FromCodex::Eof);
                        break;
                    }
                }
            }
        });

        let mut peer = CodexPeer {
            child,
            stdin,
            rx,
            next_id: 1,
            thread_id: None,
            spawn_kind,
            spawn_program,
        };
        peer.initialize()?;
        Ok(peer)
    }

    /// How the codex child was launched (`direct` / `node-direct` / `cmd-shim`).
    pub fn spawn_kind(&self) -> &'static str {
        self.spawn_kind
    }

    /// The resolved launch line, for diagnostics/logging.
    pub fn spawn_program(&self) -> &str {
        &self.spawn_program
    }

    /// True once a conversation is established, so the next call reuses Codex's
    /// prompt cache via `codex-reply` (warm) instead of a cold `codex` turn.
    pub fn is_warm(&self) -> bool {
        self.thread_id.is_some()
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "aibridge", "version": crate::version() }
            }),
            INIT_TIMEOUT,
        )?;
        self.notify("notifications/initialized")?;
        Ok(())
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.write_msg(&json!({ "jsonrpc": "2.0", "method": method, "params": {} }))
    }

    /// Write one newline-delimited JSON-RPC message to the child's stdin.
    ///
    /// The write is synchronous and unbounded, which is safe because we only ever
    /// write to a child that has already completed `initialize` — a write+read
    /// round-trip that proves it drains stdin. A child that wedges mid-session is
    /// not reused: the caller (`Server::ask_peer` / `gate_decide`) drops it on the
    /// first read timeout, so the next `write_msg` always targets a fresh, draining
    /// child rather than a stuck one.
    fn write_msg(&mut self, msg: &Value) -> Result<()> {
        self.stdin.write_all(msg.to_string().as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Send a request and wait up to `timeout` for the matching response. A
    /// timeout / EOF returns an error (the gate turns it into fail-ask) rather
    /// than blocking forever.
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_msg(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(d) => d,
                None => {
                    return Err(anyhow!(
                        "codex timed out after {}s waiting for `{method}`",
                        timeout.as_secs()
                    ))
                }
            };
            match self.rx.recv_timeout(remaining) {
                Ok(FromCodex::Message(msg)) => {
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
                Ok(FromCodex::Eof) => {
                    return Err(anyhow!(
                        "codex closed its output before responding to `{method}`"
                    ))
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(anyhow!(
                        "codex timed out after {}s waiting for `{method}`",
                        timeout.as_secs()
                    ))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("codex reader thread ended unexpectedly"))
                }
            }
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
                CALL_TIMEOUT,
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
                            "cwd": cwd,
                            // Pin reasoning effort for THIS session only (it
                            // persists to later codex-reply turns), independent of
                            // the user's global ~/.codex/config.toml. See
                            // REVIEW_REASONING_EFFORT for the quality/latency tradeoff.
                            "config": { "model_reasoning_effort": REVIEW_REASONING_EFFORT }
                        }
                    }),
                    CALL_TIMEOUT,
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
