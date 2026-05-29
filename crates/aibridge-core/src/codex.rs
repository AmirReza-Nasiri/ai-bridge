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

use crate::progress::{write_status, ProgressSink};
use aibridge_platform::{DefaultPlatform, Platform};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared in-flight review progress: the reader + heartbeat threads fold codex's
/// streamed events / liveness into it while a call is active (set by
/// [`CodexPeer::begin_progress`] / cleared by [`CodexPeer::end_progress`]).
type Progress = Arc<Mutex<Option<ProgressSink>>>;

/// Is this parsed line a JSON-RPC NOTIFICATION? A notification has a `method` and
/// NO `id` MEMBER at all. A `method` WITH an `id` (even `id: null`) is a server
/// request, not a notification — classify by key presence, not by null (Codex
/// review). Responses have an `id` and no `method`.
fn is_notification(v: &Value) -> bool {
    match v.as_object() {
        Some(o) => o.contains_key("method") && !o.contains_key("id"),
        None => false,
    }
}

/// Build the JSON-RPC response to an INBOUND server→client request from codex, so
/// codex never blocks waiting on this (headless) client. Reviews run with no human
/// at the codex layer, so we DECLINE elicitations rather than hang; other
/// server-initiated requests get a graceful empty result or a `method not found`
/// error so codex always gets an answer and proceeds. `id` is echoed verbatim.
fn server_request_response(method: &str, id: &Value) -> Value {
    match method {
        // The model called a tool that wants user input — decline (no human here).
        "elicitation/create" => json!({"jsonrpc":"2.0","id":id,"result":{"action":"decline"}}),
        "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
        // We expose no filesystem roots to the peer.
        "roots/list" => json!({"jsonrpc":"2.0","id":id,"result":{"roots":[]}}),
        // Anything else server-initiated: a clean error so codex moves on rather than
        // waiting forever (covers sampling/createMessage and any future request).
        _ => json!({
            "jsonrpc":"2.0","id":id,
            "error":{"code":-32601,"message":format!("method '{method}' is not supported by the aibridge MCP client")}
        }),
    }
}

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

/// Reasoning effort for AI Bridge's review/consult Codex sessions, set on the
/// first turn and inherited by later `codex-reply` turns. Independent of the
/// user's global `~/.codex/config.toml`. The user prioritizes accuracy: `xhigh`
/// gives the most thorough review (measured ~286s on a real ~30KB diff vs ~22s at
/// `medium`) and the timeout backstop is sized so it always completes. Tune here
/// for a faster, shallower gate (`high` / `medium` / `low`).
pub const REVIEW_REASONING_EFFORT: &str = "xhigh";

/// Reasoning effort for the `implement` tool. Lower than the reviewer's `xhigh`:
/// the implementer only PROPOSES a patch that the gate then reviews at `xhigh`, so
/// spending the full review budget twice is wasteful. `high` keeps strong drafts
/// at lower latency/quota.
pub const IMPLEMENT_REASONING_EFFORT: &str = "high";

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

/// A warm, reusable connection to a `codex mcp-server` child process. One child
/// can host MANY isolated conversation threads (verified: distinct `threadId`s
/// stay isolated); the caller (`Server`) maps topics → threadIds and uses
/// [`CodexPeer::open_thread`] / [`CodexPeer::reply`].
pub struct CodexPeer {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<FromCodex>,
    next_id: i64,
    spawn_kind: &'static str,
    spawn_program: String,
    /// Live review-progress sink the reader + heartbeat threads update.
    progress: Progress,
    /// Set on drop to stop the heartbeat thread.
    hb_shutdown: Arc<AtomicBool>,
}

impl CodexPeer {
    /// Spawn `codex mcp-server`, perform the MCP handshake, and keep it warm.
    pub fn spawn() -> Result<Self> {
        let exe = DefaultPlatform::find_executable("codex").context("locating codex")?;
        let plan = DefaultPlatform::spawn_plan(&exe);
        let spawn_kind = plan.kind.as_str();
        let spawn_program = plan.program.clone();
        // Constrain which of codex's own MCP servers this REVIEW child may use
        // (`-c mcp_servers.<name>.enabled=<bool>`), default none — so the model can't
        // derail a review by invoking a browser/scrape server that elicits or hangs.
        // User-controlled via `aibridge review-mcp`. FAIL CLOSED: if the codex config is
        // present but unenumerable we refuse to start (never inherit unfiltered servers).
        let mut review_mcp_overrides = crate::review_mcp::spawn_overrides().ok_or_else(|| {
            anyhow::anyhow!(
                "can't read/parse ~/.codex/config.toml to enforce the review-mcp policy — \
                 refusing to start the review peer with unfiltered MCP servers; fix the codex \
                 config (see `aibridge review-mcp list`)"
            )
        })?;
        // v0.29 (O1): append the user-selected review model / context-window overrides
        // (empty when unset → codex uses its config.toml default). Kept SEPARATE from the
        // fail-closed mcp-policy overrides above so an unset/invalid model never weakens
        // policy enforcement.
        review_mcp_overrides.extend(crate::review_mcp::codex_spawn_overrides());
        let mut child = plan
            .into_command()
            .arg("mcp-server")
            // Overrides go AFTER the subcommand (proven placement).
            .args(&review_mcp_overrides)
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
        // request side can apply a deadline instead of blocking forever. It ALSO
        // folds codex's streamed turn-event notifications into the shared progress
        // sink (when a review is active) — these would otherwise be dropped by the
        // request loop, leaving a multi-minute review looking hung.
        let (tx, rx) = mpsc::channel();
        let progress: Progress = Arc::new(Mutex::new(None));
        let reader_progress = Arc::clone(&progress);
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
                            // Relay turn-event notifications to the live progress
                            // sink, then forward JSON; stop if the request side has
                            // gone away. The disk write happens AFTER the lock is
                            // released (snapshot pattern) so file I/O never blocks
                            // the reader while holding the shared mutex.
                            Ok(v) => {
                                if is_notification(&v) {
                                    let snap = reader_progress
                                        .lock()
                                        .ok()
                                        .and_then(|mut g| g.as_mut().and_then(|s| s.note(&v)));
                                    if let Some(snap) = snap {
                                        write_status(&snap);
                                    }
                                }
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

        // Heartbeat thread: while a review is active, refresh its elapsed/bridge
        // heartbeat (~every second, throttled) even when Codex emits nothing — so a
        // long silent reasoning gap reads as "thinking", not "hung", and a watcher
        // sees the clock move. Exits when the peer is dropped.
        let hb_shutdown = Arc::new(AtomicBool::new(false));
        let hb_progress = Arc::clone(&progress);
        let hb_flag = Arc::clone(&hb_shutdown);
        std::thread::spawn(move || {
            while !hb_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let snap = hb_progress
                    .lock()
                    .ok()
                    .and_then(|mut g| g.as_mut().and_then(|s| s.heartbeat()));
                if let Some(snap) = snap {
                    write_status(&snap);
                }
            }
        });

        let mut peer = CodexPeer {
            child,
            stdin,
            rx,
            next_id: 1,
            spawn_kind,
            spawn_program,
            progress,
            hb_shutdown,
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

    /// Start live progress tracking for a review in `cwd` (`phase` labels it, e.g.
    /// `plan-gate` / `review` / `consult:<topic>`). The reader + heartbeat threads
    /// then stream codex's turn events / liveness into `.ai-bridge/review-status.json`.
    /// Pair with [`end_progress`](Self::end_progress). `ProgressSink::new` writes the
    /// initial status itself (before the sink is shared), so no I/O under the lock.
    pub fn begin_progress(&self, cwd: &str, phase: &str) {
        let sink = ProgressSink::new(cwd, phase);
        if let Ok(mut g) = self.progress.lock() {
            *g = Some(sink);
        }
    }

    /// Record a DECLINED elicitation (a codex tool wanted interactive input we can't
    /// answer headlessly) into the live progress, so `aibridge status`, the review
    /// result, and `doctor` can show which server to configure. Best-effort; the disk
    /// write of the status snapshot happens after the sink lock is released.
    fn record_elicitation(&self, params: &Value) {
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| {
                params
                    .pointer("/requestedSchema/title")
                    .and_then(Value::as_str)
            })
            .unwrap_or("(no message provided)");
        let keys: Vec<String> = params
            .pointer("/requestedSchema/properties")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let snap = self
            .progress
            .lock()
            .ok()
            .and_then(|mut g| g.as_mut().map(|s| s.note_elicitation(message, &keys)));
        if let Some(snap) = snap {
            write_status(&snap);
        }
    }

    /// A one-line note iff THIS review turn had to decline an elicitation (for the
    /// caller to append to the review result). `begin_progress` resets it per turn.
    pub fn last_elicitation_note(&self) -> Option<String> {
        self.progress
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|s| s.last_elicitation_summary()))
    }

    /// Stop tracking the current review and write a final snapshot with the terminal
    /// `outcome` (`completed` / `error` / `timeout`). The disk write happens after
    /// the lock is released.
    pub fn end_progress(&self, outcome: &str) {
        let snap = self
            .progress
            .lock()
            .ok()
            .and_then(|mut g| g.take().map(|mut s| s.finish(outcome)));
        if let Some(snap) = snap {
            write_status(&snap);
        }
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
                    // Classify by SHAPE, not by id — codex's request-id space is
                    // separate from ours, so an id collision is possible (Codex review):
                    // - `method` + `id`  → an INBOUND server→client request: ANSWER it
                    //   so codex never blocks on us (e.g. `elicitation/create` when the
                    //   model calls one of codex's own MCP tools mid-review — that
                    //   deadlocked a real plan_gate until CALL_TIMEOUT), then keep
                    //   waiting for OUR response.
                    // - `method`, no `id` → a notification (already relayed to the
                    //   progress sink by the reader): ignore.
                    // - no `method`       → a response: return it iff the id is ours.
                    if let Some(method) = msg.get("method").and_then(Value::as_str) {
                        if let Some(rid) = msg.get("id").filter(|v| !v.is_null()).cloned() {
                            // Capture the elicitation params BEFORE answering (so the
                            // user can see which tool wanted input + configure it).
                            let elicit = (method == "elicitation/create")
                                .then(|| msg.get("params").cloned())
                                .flatten();
                            // If we can't answer, surface a transport error NOW rather
                            // than degrade back into waiting for `CALL_TIMEOUT` (Codex
                            // review) — the caller invalidates + re-warms the peer.
                            self.write_msg(&server_request_response(method, &rid))
                                .map_err(|e| {
                                    anyhow!("failed to answer codex `{method}` request: {e}")
                                })?;
                            if let Some(params) = elicit {
                                self.record_elicitation(&params); // best-effort visibility
                            }
                        }
                        continue;
                    }
                    if msg.get("id").and_then(Value::as_i64) != Some(id) {
                        continue; // a stray/late response to some other request
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

    /// Open a NEW conversation thread (a cold `codex` turn) and return its
    /// `threadId` plus the reply text. `effort` pins the reasoning effort for this
    /// thread (it persists to later `reply` turns), independent of the user's
    /// global `~/.codex/config.toml` — pass [`REVIEW_REASONING_EFFORT`] for
    /// reviews/consults or [`IMPLEMENT_REASONING_EFFORT`] for the implementer.
    pub fn open_thread(
        &mut self,
        prompt: &str,
        cwd: &str,
        effort: &str,
    ) -> Result<(String, String)> {
        let result = self.request(
            "tools/call",
            json!({
                "name": "codex",
                "arguments": {
                    "prompt": prompt,
                    "sandbox": "read-only",
                    "approval-policy": "never",
                    "cwd": cwd,
                    "config": { "model_reasoning_effort": effort }
                }
            }),
            CALL_TIMEOUT,
        )?;
        let thread_id = result
            .pointer("/structuredContent/threadId")
            .or_else(|| result.pointer("/structuredContent/conversationId"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("codex did not return a threadId"))?
            .to_string();
        Ok((thread_id, extract_text(&result)))
    }

    /// Continue an existing thread via `codex-reply` (warm: reuses Codex's prompt
    /// cache for that conversation). NOTE: if `thread_id` is stale (e.g. the child
    /// restarted), Codex returns a normal result whose TEXT is "Session not found
    /// for thread_id: …" rather than a JSON-RPC error — the caller detects that.
    pub fn reply(&mut self, thread_id: &str, prompt: &str) -> Result<String> {
        let result = self.request(
            "tools/call",
            json!({
                "name": "codex-reply",
                "arguments": { "threadId": thread_id, "prompt": prompt }
            }),
            CALL_TIMEOUT,
        )?;
        Ok(extract_text(&result))
    }
}

impl Drop for CodexPeer {
    fn drop(&mut self) {
        // Stop the heartbeat thread.
        self.hb_shutdown.store(true, Ordering::Relaxed);
        // Drop-guard: if a review was still in flight (the child died / a call
        // panicked before end_progress), record a terminal `error` so the status
        // file never stays stuck on `active: true`.
        let snap = self
            .progress
            .lock()
            .ok()
            .and_then(|mut g| g.take().map(|mut s| s.finish("error")));
        if let Some(snap) = snap {
            write_status(&snap);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_jsonrpc_notifications_by_id_key_presence() {
        // Notification: method present, NO id member.
        assert!(is_notification(
            &json!({"jsonrpc":"2.0","method":"codex/event","params":{}})
        ));
        // Server REQUEST: method WITH id — not a notification, even if id is null.
        assert!(!is_notification(
            &json!({"jsonrpc":"2.0","id":7,"method":"elicit","params":{}})
        ));
        assert!(!is_notification(
            &json!({"jsonrpc":"2.0","id":null,"method":"elicit","params":{}})
        ));
        // Response: id + result, no method.
        assert!(!is_notification(
            &json!({"jsonrpc":"2.0","id":2,"result":{}})
        ));
        // Non-object lines are never notifications.
        assert!(!is_notification(&json!("hello")));
    }

    #[test]
    fn answers_inbound_server_requests_so_codex_never_blocks() {
        let id = json!(7);
        // Elicitation → decline (headless: no human to answer).
        let elicit = server_request_response("elicitation/create", &id);
        assert_eq!(
            elicit.pointer("/result/action").and_then(Value::as_str),
            Some("decline")
        );
        assert_eq!(elicit.get("id"), Some(&id), "id echoed verbatim");
        // ping → empty result.
        assert_eq!(
            server_request_response("ping", &id).pointer("/result"),
            Some(&json!({}))
        );
        // roots/list → empty roots.
        assert!(server_request_response("roots/list", &id)
            .pointer("/result/roots")
            .and_then(Value::as_array)
            .unwrap()
            .is_empty());
        // anything else → method-not-found error (a clean answer, never a hang).
        assert_eq!(
            server_request_response("sampling/createMessage", &id)
                .pointer("/error/code")
                .and_then(Value::as_i64),
            Some(-32601)
        );
        // string ids are echoed too.
        let sid = json!("abc-1");
        assert_eq!(server_request_response("ping", &sid).get("id"), Some(&sid));
    }

    /// Real codex round-trip proving the reader relays codex's turn events into the
    /// live progress sink. Ignored by default (needs `codex` logged in + quota);
    /// run manually: `cargo test -p aibridge-core --release -- --ignored progress`.
    #[test]
    #[ignore = "hits real codex; run with --ignored"]
    fn progress_captures_live_codex_events() {
        let cwd = std::env::temp_dir().join(format!("aibridge-codexprog-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwds = cwd.to_string_lossy().to_string();

        let mut peer = CodexPeer::spawn().expect("spawn codex mcp-server");
        peer.begin_progress(&cwds, "test");
        let _ = peer
            .open_thread("Reply with the single word OK.", &cwds, "low")
            .expect("codex open_thread");
        peer.end_progress("completed");

        let st = crate::progress::read_status(&cwds).expect("status file written");
        eprintln!("CAPTURED STATUS: {st}");
        let events = st.get("events").and_then(Value::as_u64).unwrap_or(0);
        assert!(
            events > 0,
            "expected codex to stream >=1 progress event during the turn, got {events}"
        );
    }
}
