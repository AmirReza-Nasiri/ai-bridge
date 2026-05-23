//! Live review-progress telemetry.
//!
//! A Codex review at `xhigh` takes MINUTES even for a small diff (the cost is the
//! model's reasoning, not the input size), and the warm peer blocks in a single
//! JSON-RPC request the whole time — so from the outside it looks hung. The
//! `codex mcp-server` child actually streams `codex/event` notifications during a
//! turn (reasoning, token counts, …); AI Bridge's request loop otherwise drops
//! them. This module relays them into a small, atomically-written
//! `.ai-bridge/review-status.json` so the user (and `aibridge status`) can watch a
//! review progress live.
//!
//! Two freshness signals are kept SEPARATE (Codex review, validated by a real
//! capture where ~47s of silent model reasoning would have tripped a naive
//! "stall"): `last_bridge_heartbeat_ms` (the AI Bridge process is alive — bumped
//! by a heartbeat thread even while Codex is silent) vs `last_codex_event_ms`
//! (Codex actually emitted something). A long codex-event gap is normal
//! "thinking"; only a stale *bridge* heartbeat is a real stall.
//!
//! Disk writes happen OUTSIDE the state lock: mutating methods return a
//! [`StatusSnapshot`] the caller writes after releasing the mutex, so the reader
//! thread is never blocked on file I/O.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Don't rewrite the status file from codex events more than this often.
const WRITE_THROTTLE_MS: u128 = 1500;
/// Heartbeat cadence: refresh `elapsed`/bridge-heartbeat at least this often even
/// when Codex is silent, so a watcher sees the review is alive while it reasons.
const HEARTBEAT_EVERY_SECS: u64 = 5;
/// Keep this many most-recent events for at-a-glance detail.
const RECENT_CAP: usize = 8;
/// A bridge heartbeat older than this (while "active") means the AI Bridge process
/// itself stalled/died — a real problem, unlike Codex merely being silent.
pub const BRIDGE_STALL_MS: u64 = 30_000;
/// A codex-event gap longer than this is shown as "thinking" (informational, NOT
/// a stall): silent reasoning stretches this long are normal at high effort.
pub const CODEX_QUIET_MS: u64 = 20_000;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn status_path(cwd: &str) -> PathBuf {
    Path::new(cwd).join(".ai-bridge").join("review-status.json")
}

/// Process-wide monotonic sequence stamped on every snapshot AT CREATION (under
/// the sink lock), so creation order == logical order even though the disk write
/// happens later, off-lock. Lets [`write_status`] drop a delayed older snapshot.
static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(1);
fn next_seq() -> u64 {
    SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Serializes status writes AND records the last-written seq per cwd, so a newer
/// snapshot is never overwritten by a delayed older one (reader vs heartbeat
/// thread race — Codex review). A dedicated gate, NOT the sink mutex, so the
/// reader is never blocked on the sink while a write happens.
fn write_gate() -> &'static Mutex<HashMap<String, u64>> {
    static GATE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A point-in-time, serializable view of a review, written to disk OUTSIDE the
/// state lock (carries its own `cwd` so the writer needs nothing else, plus a
/// monotonic `seq` so a stale write loses to a newer one).
pub struct StatusSnapshot {
    cwd: String,
    seq: u64,
    body: Value,
}

/// Atomically write a snapshot, newest-wins. Temp+rename prevents torn READS;
/// the seq gate prevents a delayed OLDER snapshot from clobbering a newer one
/// (e.g. a late heartbeat landing after the terminal `completed` write). Per-write
/// unique temp name avoids two writers colliding on the same temp file.
pub fn write_status(snap: &StatusSnapshot) {
    // Key the gate by the CANONICAL dir so case/symlink/relative variants of the
    // same project never split into two gates for one status file (Codex review).
    let key = std::fs::canonicalize(&snap.cwd)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| snap.cwd.clone());
    let mut last = match write_gate().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    // `<=`: a duplicate/equal seq is structurally impossible (global atomic), so
    // dropping `<=` is stricter-and-harmless rather than relying on that invariant.
    if snap.seq <= last.get(&key).copied().unwrap_or(0) {
        return; // a newer (or equal) snapshot already landed for this project
    }
    let dir = Path::new(&snap.cwd).join(".ai-bridge");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(
        "review-status.json.tmp.{}.{}",
        std::process::id(),
        snap.seq
    ));
    if std::fs::write(&tmp, snap.body.to_string()).is_ok()
        && std::fs::rename(&tmp, status_path(&snap.cwd)).is_ok()
    {
        last.insert(key, snap.seq);
    } else {
        let _ = std::fs::remove_file(&tmp); // don't leak a temp on failure
    }
}

struct RecentEvent {
    kind: String,
    elapsed_s: u64,
    tokens: Option<u64>,
}

/// Live state of the in-flight Codex review, refreshed from the event stream and a
/// periodic heartbeat. Mutating methods return a [`StatusSnapshot`] to write after
/// the lock is released (or `None` when throttled).
pub struct ProgressSink {
    cwd: String,
    phase: String,
    started: Instant,
    started_ms: u128,
    last_write: Option<Instant>,
    last_heartbeat_write: Option<Instant>,
    events: u64,
    last_event: String,
    last_codex_event_ms: u128,
    last_bridge_heartbeat_ms: u128,
    tokens: Option<u64>,
    tokens_source: Option<String>,
    recent: VecDeque<RecentEvent>,
}

impl ProgressSink {
    /// Begin tracking a review and write the initial `active` status immediately,
    /// so a watcher sees it the moment it starts (before the first event). Called
    /// before the sink is shared, so the direct write here can't block the reader.
    pub fn new(cwd: &str, phase: &str) -> Self {
        let now = now_ms();
        let s = ProgressSink {
            cwd: cwd.to_string(),
            phase: phase.to_string(),
            started: Instant::now(),
            started_ms: now,
            last_write: None,
            last_heartbeat_write: None,
            events: 0,
            last_event: String::new(),
            last_codex_event_ms: 0,
            last_bridge_heartbeat_ms: now,
            tokens: None,
            tokens_source: None,
            recent: VecDeque::new(),
        };
        write_status(&s.snapshot(true, "running"));
        s
    }

    /// Fold one codex notification into the state; returns a snapshot to write when
    /// a write is due (throttled), else `None`.
    pub fn note(&mut self, msg: &Value) -> Option<StatusSnapshot> {
        self.events += 1;
        let now = now_ms();
        self.last_codex_event_ms = now;
        self.last_bridge_heartbeat_ms = now; // receiving an event also proves liveness
        let params = msg.get("params").unwrap_or(msg);
        let kind = event_type(msg, params).unwrap_or_else(|| "unknown".to_string());
        self.last_event = kind.clone();
        if let Some((tk, src)) = extract_tokens(params) {
            self.tokens = Some(tk);
            self.tokens_source = Some(src);
        }
        self.recent.push_back(RecentEvent {
            kind,
            elapsed_s: self.started.elapsed().as_secs(),
            tokens: self.tokens,
        });
        while self.recent.len() > RECENT_CAP {
            self.recent.pop_front();
        }
        let due = self
            .last_write
            .map(|w| w.elapsed().as_millis() >= WRITE_THROTTLE_MS)
            .unwrap_or(true);
        if due {
            self.last_write = Some(Instant::now());
            Some(self.snapshot(true, "running"))
        } else {
            None
        }
    }

    /// Prove the bridge is alive while Codex is silent: bump the heartbeat and, at
    /// most every [`HEARTBEAT_EVERY_SECS`], return a refreshed snapshot (so a
    /// watcher sees `elapsed` climb during a long reasoning gap).
    pub fn heartbeat(&mut self) -> Option<StatusSnapshot> {
        self.last_bridge_heartbeat_ms = now_ms();
        let due = self
            .last_heartbeat_write
            .map(|w| w.elapsed().as_secs() >= HEARTBEAT_EVERY_SECS)
            .unwrap_or(true);
        if due {
            self.last_heartbeat_write = Some(Instant::now());
            Some(self.snapshot(true, "running"))
        } else {
            None
        }
    }

    /// Mark the review finished with a terminal `outcome`
    /// (`completed` / `error` / `timeout`); returns the final snapshot to write.
    pub fn finish(&mut self, outcome: &str) -> StatusSnapshot {
        self.snapshot(false, outcome)
    }

    fn snapshot(&self, active: bool, status: &str) -> StatusSnapshot {
        let recent: Vec<Value> = self
            .recent
            .iter()
            .map(|r| json!({ "type": r.kind, "elapsed_s": r.elapsed_s, "tokens": r.tokens }))
            .collect();
        // Stamp the seq HERE (callers hold the sink lock), so seq order matches the
        // order state actually changed, regardless of when the disk write happens.
        let seq = next_seq();
        StatusSnapshot {
            cwd: self.cwd.clone(),
            seq,
            body: json!({
                "seq": seq,
                "active": active,
                "status": status,
                "phase": self.phase,
                "started_ms": self.started_ms as u64,
                "updated_ms": now_ms() as u64,
                "elapsed_s": self.started.elapsed().as_secs(),
                "events": self.events,
                "last_event": self.last_event,
                "last_codex_event_ms": self.last_codex_event_ms as u64,
                "last_bridge_heartbeat_ms": self.last_bridge_heartbeat_ms as u64,
                "tokens": self.tokens,
                "tokens_source": self.tokens_source,
                "recent_events": recent,
            }),
        }
    }
}

/// Event type via PRIORITIZED paths (codex carries it at `params.msg.type`), then
/// `params.type`, then the JSON-RPC `method`. Avoids a recursive search that could
/// latch onto an unrelated nested `type` (Codex review).
fn event_type(msg: &Value, params: &Value) -> Option<String> {
    if let Some(t) = params.pointer("/msg/type").and_then(Value::as_str) {
        return Some(t.to_string());
    }
    if let Some(t) = params.get("type").and_then(Value::as_str) {
        return Some(t.to_string());
    }
    msg.get("method")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Token total via PRIORITIZED known paths first; only then a restricted recursive
/// fallback for the exact key `total_tokens` (never the looser `tokens`, which
/// could be a per-message/cache/rate-limit counter — Codex review). Returns the
/// value plus the source path for debuggability.
fn extract_tokens(params: &Value) -> Option<(u64, String)> {
    const PATHS: &[&str] = &[
        "/msg/info/total_token_usage/total_tokens",
        "/info/total_token_usage/total_tokens",
        "/msg/total_token_usage/total_tokens",
        "/total_token_usage/total_tokens",
        "/msg/info/total_tokens",
        "/info/total_tokens",
    ];
    for p in PATHS {
        if let Some(n) = params.pointer(p).and_then(Value::as_u64) {
            return Some((n, p.to_string()));
        }
    }
    find_u64_key(params, "total_tokens").map(|n| (n, "recursive:total_tokens".to_string()))
}

/// Recursive search for a u64 under EXACTLY `key` (kept narrow on purpose).
fn find_u64_key(v: &Value, key: &str) -> Option<u64> {
    match v {
        Value::Object(m) => {
            if let Some(n) = m.get(key).and_then(Value::as_u64) {
                return Some(n);
            }
            m.values().find_map(|val| find_u64_key(val, key))
        }
        Value::Array(a) => a.iter().find_map(|val| find_u64_key(val, key)),
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
    let status = s.get("status").and_then(Value::as_str).unwrap_or("?");
    let phase = s.get("phase").and_then(Value::as_str).unwrap_or("?");
    let elapsed = s.get("elapsed_s").and_then(Value::as_u64).unwrap_or(0);
    let events = s.get("events").and_then(Value::as_u64).unwrap_or(0);
    let last = s.get("last_event").and_then(Value::as_str).unwrap_or("");
    let tokens = s.get("tokens").and_then(Value::as_u64);
    let tok = tokens.map(|t| format!(", ~{t} tokens")).unwrap_or_default();

    if !active {
        return Some(format!(
            "no review in progress (last [{phase}]: {status}, {elapsed}s, {events} events{tok})"
        ));
    }

    let now = now_ms() as u64;
    let bridge_hb = s
        .get("last_bridge_heartbeat_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let codex_ev = s
        .get("last_codex_event_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // A stale BRIDGE heartbeat is a real stall; a quiet CODEX is just thinking.
    let note = if bridge_hb > 0 && now.saturating_sub(bridge_hb) >= BRIDGE_STALL_MS {
        format!(
            " — ⚠ bridge heartbeat stale {}s (possible stall)",
            now.saturating_sub(bridge_hb) / 1000
        )
    } else if codex_ev > 0 && now.saturating_sub(codex_ev) >= CODEX_QUIET_MS {
        format!(
            " — thinking (no codex event for {}s)",
            now.saturating_sub(codex_ev) / 1000
        )
    } else {
        String::new()
    };
    Some(format!(
        "review IN PROGRESS [{phase}]: {elapsed}s elapsed, {events} events{tok}, last: {last}{note}"
    ))
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
    fn new_sink_writes_active_running_status() {
        let cwd = tmp();
        let _s = ProgressSink::new(&cwd, "plan-gate");
        let st = read_status(&cwd).unwrap();
        assert_eq!(st.get("active").and_then(Value::as_bool), Some(true));
        assert_eq!(st.get("status").and_then(Value::as_str), Some("running"));
        assert_eq!(st.get("phase").and_then(Value::as_str), Some("plan-gate"));
    }

    #[test]
    fn note_extracts_type_and_tokens_from_prioritized_paths() {
        let cwd = tmp();
        let mut s = ProgressSink::new(&cwd, "review");
        let ev = json!({
            "jsonrpc": "2.0",
            "method": "codex/event",
            "params": { "msg": { "type": "agent_reasoning_delta",
                                 "info": { "total_token_usage": { "total_tokens": 4096 } } } }
        });
        let snap = s.note(&ev).expect("first event should produce a snapshot");
        write_status(&snap);
        let st = read_status(&cwd).unwrap();
        assert_eq!(
            st.get("last_event").and_then(Value::as_str),
            Some("agent_reasoning_delta")
        );
        assert_eq!(st.get("tokens").and_then(Value::as_u64), Some(4096));
        assert_eq!(
            st.get("tokens_source").and_then(Value::as_str),
            Some("/msg/info/total_token_usage/total_tokens")
        );
        assert_eq!(st.get("events").and_then(Value::as_u64), Some(1));
        assert_eq!(
            st.get("recent_events")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[test]
    fn token_search_ignores_unrelated_nested_counters() {
        // A loose recursive search would wrongly pick `rate_limits.tokens`; the
        // prioritized + total_tokens-only logic must not.
        let params = json!({
            "msg": { "type": "token_count",
                     "rate_limits": { "tokens": 999 },
                     "info": { "total_token_usage": { "total_tokens": 7777 } } }
        });
        assert_eq!(extract_tokens(&params).map(|(n, _)| n), Some(7777));
        // No total_tokens anywhere → None (not the stray `tokens`).
        let only_stray = json!({ "msg": { "rate_limits": { "tokens": 999 } } });
        assert_eq!(extract_tokens(&only_stray), None);
    }

    #[test]
    fn event_type_prefers_msg_type_then_method() {
        let p = json!({ "msg": { "type": "exec_command_end" } });
        assert_eq!(
            event_type(&json!({"method":"codex/event"}), &p).as_deref(),
            Some("exec_command_end")
        );
        // No type anywhere → fall back to method.
        assert_eq!(
            event_type(&json!({"method":"codex/heartbeat"}), &json!({})).as_deref(),
            Some("codex/heartbeat")
        );
    }

    #[test]
    fn finish_marks_inactive_with_outcome() {
        let cwd = tmp();
        let mut s = ProgressSink::new(&cwd, "consult:x");
        write_status(&s.finish("completed"));
        let st = read_status(&cwd).unwrap();
        assert_eq!(st.get("active").and_then(Value::as_bool), Some(false));
        assert_eq!(st.get("status").and_then(Value::as_str), Some("completed"));
        assert!(status_report(&cwd)
            .unwrap()
            .contains("no review in progress"));
        assert!(status_report(&cwd).unwrap().contains("completed"));
    }

    #[test]
    fn heartbeat_keeps_status_fresh() {
        let cwd = tmp();
        let mut s = ProgressSink::new(&cwd, "review");
        let snap = s.heartbeat().expect("first heartbeat writes");
        write_status(&snap);
        let st = read_status(&cwd).unwrap();
        assert!(
            st.get("last_bridge_heartbeat_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
        );
        assert_eq!(st.get("active").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn recent_events_ring_is_bounded() {
        let cwd = tmp();
        let mut s = ProgressSink::new(&cwd, "review");
        for i in 0..(RECENT_CAP + 5) {
            let _ = s.note(&json!({ "method": "codex/event",
                                    "params": { "msg": { "type": format!("e{i}") } } }));
        }
        write_status(&s.snapshot(true, "running"));
        let st = read_status(&cwd).unwrap();
        assert_eq!(
            st.get("recent_events")
                .and_then(Value::as_array)
                .map(|a| a.len()),
            Some(RECENT_CAP)
        );
    }
}
