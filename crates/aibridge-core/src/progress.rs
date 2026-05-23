//! Live review-progress telemetry.
//!
//! A Codex review at `xhigh` takes MINUTES even for a small diff (the cost is the
//! model's reasoning, not the input size), and the warm peer blocks in a single
//! JSON-RPC request the whole time — so from the outside it looks hung. The
//! `codex mcp-server` child actually streams `codex/event` notifications during a
//! turn (reasoning, token counts, …); AI Bridge's request loop otherwise drops
//! them. This module relays them into a small, atomically-written
//! `.ai-bridge/review-status.json` so the user (and `aibridge status`) can watch a
//! review progress live — and, crucially, tell a still-thinking review (event
//! count / tokens climbing) apart from a genuinely hung codex (events stopped,
//! no result).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Don't rewrite the status file more than this often (events can stream fast).
const WRITE_THROTTLE_MS: u128 = 1500;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn status_path(cwd: &str) -> PathBuf {
    Path::new(cwd).join(".ai-bridge").join("review-status.json")
}

/// Live state of the in-flight Codex review, refreshed from the event stream.
pub struct ProgressSink {
    cwd: String,
    phase: String,
    started: Instant,
    started_ms: u128,
    last_write: Option<Instant>,
    events: u64,
    last_event: String,
    last_event_ms: u128,
    tokens: Option<u64>,
}

impl ProgressSink {
    /// Begin tracking a review; writes the initial `active` status immediately so a
    /// watcher sees the review the moment it starts (before the first event).
    pub fn new(cwd: &str, phase: &str) -> Self {
        let s = ProgressSink {
            cwd: cwd.to_string(),
            phase: phase.to_string(),
            started: Instant::now(),
            started_ms: now_ms(),
            last_write: None,
            events: 0,
            last_event: String::new(),
            last_event_ms: 0,
            tokens: None,
        };
        s.write(true);
        s
    }

    /// Fold one codex notification into the progress state (throttled to disk).
    pub fn note(&mut self, msg: &Value) {
        self.events += 1;
        self.last_event_ms = now_ms();
        let params = msg.get("params").unwrap_or(msg);
        if let Some(t) = event_type(msg, params) {
            self.last_event = t;
        }
        if let Some(tk) = find_u64(params, &["total_tokens", "tokens"]) {
            self.tokens = Some(tk);
        }
        let due = self
            .last_write
            .map(|w| w.elapsed().as_millis() >= WRITE_THROTTLE_MS)
            .unwrap_or(true);
        if due {
            self.write(true);
            self.last_write = Some(Instant::now());
        }
    }

    /// Mark the review finished (writes a final `active: false` snapshot).
    pub fn finish(&mut self) {
        self.write(false);
    }

    fn write(&self, active: bool) {
        let dir = Path::new(&self.cwd).join(".ai-bridge");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let body = json!({
            "active": active,
            "phase": self.phase,
            "started_ms": self.started_ms as u64,
            "updated_ms": now_ms() as u64,
            "elapsed_s": self.started.elapsed().as_secs(),
            "events": self.events,
            "last_event": self.last_event,
            "last_event_ms": self.last_event_ms as u64,
            "tokens": self.tokens,
        })
        .to_string();
        // Atomic temp+rename so `aibridge status` never reads a half-written file.
        let tmp = dir.join(format!("review-status.json.tmp.{}", std::process::id()));
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, status_path(&self.cwd));
        }
    }
}

/// The event's type string: prefer a `type` field anywhere in `params` (codex
/// events carry one), else fall back to the JSON-RPC `method`.
fn event_type(msg: &Value, params: &Value) -> Option<String> {
    find_str(params, "type").map(str::to_string).or_else(|| {
        msg.get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// First string value for `key` anywhere in `v` (bounded by the small event size).
fn find_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    match v {
        Value::Object(m) => {
            if let Some(s) = m.get(key).and_then(Value::as_str) {
                return Some(s);
            }
            m.values().find_map(|val| find_str(val, key))
        }
        Value::Array(a) => a.iter().find_map(|val| find_str(val, key)),
        _ => None,
    }
}

/// First u64 value for any of `keys` anywhere in `v` (finds e.g. a nested
/// `total_tokens` inside a `total_token_usage` object without hardcoding the path).
fn find_u64(v: &Value, keys: &[&str]) -> Option<u64> {
    match v {
        Value::Object(m) => {
            for k in keys {
                if let Some(n) = m.get(*k).and_then(Value::as_u64) {
                    return Some(n);
                }
            }
            m.values().find_map(|val| find_u64(val, keys))
        }
        Value::Array(a) => a.iter().find_map(|val| find_u64(val, keys)),
        _ => None,
    }
}

/// Read the current review status (for `aibridge status` / `doctor`).
pub fn read_status(cwd: &str) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(status_path(cwd)).ok()?).ok()
}

/// One-line human summary of the current review for the CLI. `None` when there is
/// no status file at all.
pub fn status_report(cwd: &str) -> Option<String> {
    let s = read_status(cwd)?;
    let active = s.get("active").and_then(Value::as_bool).unwrap_or(false);
    let phase = s.get("phase").and_then(Value::as_str).unwrap_or("?");
    let elapsed = s.get("elapsed_s").and_then(Value::as_u64).unwrap_or(0);
    let events = s.get("events").and_then(Value::as_u64).unwrap_or(0);
    let last = s.get("last_event").and_then(Value::as_str).unwrap_or("");
    let tokens = s.get("tokens").and_then(Value::as_u64);
    // Gap since the last event — a large gap on an "active" review hints at a stall.
    let now = now_ms() as u64;
    let last_ev = s.get("last_event_ms").and_then(Value::as_u64).unwrap_or(0);
    let since_event = now.saturating_sub(last_ev) / 1000;

    let tok = tokens.map(|t| format!(", ~{t} tokens")).unwrap_or_default();
    if active {
        let stall = if events > 0 && since_event >= 30 {
            format!(" — ⚠ no event for {since_event}s (possible stall)")
        } else {
            String::new()
        };
        Some(format!(
            "review IN PROGRESS [{phase}]: {elapsed}s elapsed, {events} events{tok}, last: {last}{stall}"
        ))
    } else {
        Some(format!(
            "no review in progress (last [{phase}]: {elapsed}s, {events} events{tok})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    fn tmp() -> String {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("aibridge-progress-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p.display().to_string()
    }

    #[test]
    fn new_sink_writes_active_status() {
        let cwd = tmp();
        let _s = ProgressSink::new(&cwd, "plan-gate");
        let st = read_status(&cwd).unwrap();
        assert_eq!(st.get("active").and_then(Value::as_bool), Some(true));
        assert_eq!(st.get("phase").and_then(Value::as_str), Some("plan-gate"));
    }

    #[test]
    fn note_extracts_type_and_tokens_from_nested_event() {
        let cwd = tmp();
        let mut s = ProgressSink::new(&cwd, "review");
        // Shape mirrors a codex/event notification with nested usage.
        let ev = json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": { "msg": { "type": "agent_reasoning_delta",
                                 "info": { "total_token_usage": { "total_tokens": 4096 } } } }
        });
        s.note(&ev);
        let st = read_status(&cwd).unwrap();
        assert_eq!(
            st.get("last_event").and_then(Value::as_str),
            Some("agent_reasoning_delta")
        );
        assert_eq!(st.get("tokens").and_then(Value::as_u64), Some(4096));
        assert_eq!(st.get("events").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn finish_marks_inactive() {
        let cwd = tmp();
        let mut s = ProgressSink::new(&cwd, "consult:x");
        s.finish();
        let st = read_status(&cwd).unwrap();
        assert_eq!(st.get("active").and_then(Value::as_bool), Some(false));
        assert!(status_report(&cwd)
            .unwrap()
            .contains("no review in progress"));
    }

    #[test]
    fn event_type_falls_back_to_method() {
        let cwd = tmp();
        let mut s = ProgressSink::new(&cwd, "review");
        s.note(&json!({ "method": "codex/heartbeat", "params": {} }));
        let st = read_status(&cwd).unwrap();
        assert_eq!(
            st.get("last_event").and_then(Value::as_str),
            Some("codex/heartbeat")
        );
    }
}
