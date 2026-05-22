//! Per-topic consult transcripts → cross-session dialogue persistence.
//!
//! Codex `threadId`s don't survive a codex restart (verified: T2 "Session not
//! found"), so durable topic dialogue is reconstructed by REPLAY: each completed
//! consult turn is appended to `.ai-bridge/topics/<topic>.jsonl`, and resuming a
//! topic seeds a fresh thread with a bounded replay of recent turns.
//!
//! Crash-safety by construction: only COMPLETE turns are written (a crash mid
//! Codex-call simply loses the in-flight question — never a partial record), and
//! corrupt lines are skipped on read so one bad line can't poison a whole topic.
//! `.ai-bridge/` is git-ignored by `init` and excluded from the review bundle, so
//! transcripts never get committed or reviewed. Best-effort throughout:
//! persistence must never break a review.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Max characters of replayed history when resuming a topic (bounded so a long
/// dialogue can't blow up the prompt; the oldest turns are dropped first).
pub const REPLAY_BUDGET: usize = 6_000;

fn topics_dir(cwd: &str) -> PathBuf {
    Path::new(cwd).join(".ai-bridge").join("topics")
}

fn topic_file(cwd: &str, topic: &str) -> PathBuf {
    topics_dir(cwd).join(format!("{topic}.jsonl"))
}

/// True if a NON-EMPTY transcript exists (i.e. the topic is resumable). A
/// zero-byte file — e.g. created then crashed-before-write — is NOT resumable;
/// treating it as such would seed an empty replay frame on the next call.
pub fn exists(cwd: &str, topic: &str) -> bool {
    std::fs::metadata(topic_file(cwd, topic))
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Append a COMPLETED turn. Best-effort: any failure is silently ignored so
/// persistence can never break the consult itself.
pub fn append_turn(cwd: &str, topic: &str, question: &str, answer: &str) {
    let dir = topics_dir(cwd);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = now_ms();
    let line = json!({ "ts": ts, "q": question, "a": answer }).to_string();
    // O_APPEND single-line writes are OS-atomic (no mid-line interleave), so two
    // server processes (two Claude windows) on the same topic can't corrupt a
    // line — though their turns may land out of wall-clock order. Acceptable for
    // v1; an exclusive file lock (e.g. fs2) is the upgrade path if it matters.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(topic_file(cwd, topic))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Render a bounded replay of the most RECENT turns, in chronological order, for
/// resuming a topic. Skips corrupt/legacy lines. Returns "" if nothing usable.
pub fn replay(cwd: &str, topic: &str, budget: usize) -> String {
    let content = match std::fs::read_to_string(topic_file(cwd, topic)) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let turns: Vec<(String, String)> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            let q = v.get("q").and_then(Value::as_str)?.to_string();
            let a = v.get("a").and_then(Value::as_str)?.to_string();
            Some((q, a))
        })
        .collect();
    // Walk newest→oldest accumulating within budget, then restore chronological
    // order so the model reads the dialogue forwards.
    let mut chosen: Vec<&(String, String)> = Vec::new();
    let mut used = 0usize;
    for t in turns.iter().rev() {
        let cost = t.0.len() + t.1.len() + 16;
        if used + cost > budget && !chosen.is_empty() {
            break;
        }
        used += cost;
        chosen.push(t);
    }
    chosen.reverse();
    let mut out = String::new();
    for (q, a) in chosen {
        out.push_str(&format!("Q: {q}\nA: {a}\n\n"));
    }
    out.trim_end().to_string()
}

/// Archive (NOT delete) a topic's transcript on `reset`, so history is never lost.
pub fn archive(cwd: &str, topic: &str) {
    let f = topic_file(cwd, topic);
    if f.is_file() {
        let dest = topics_dir(cwd).join(format!("{topic}.jsonl.{}.bak", now_ms()));
        let _ = std::fs::rename(&f, dest);
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Monotonic per-process counter so concurrently-running tests (cargo's
    // default) never collide on a same-millisecond temp-dir name.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn tmp() -> String {
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "aibridge-topics-{}-{}-{seq}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p.to_str().unwrap().to_string()
    }

    #[test]
    fn roundtrip_append_and_replay() {
        let cwd = tmp();
        assert!(!exists(&cwd, "alpha-x"));
        append_turn(&cwd, "alpha-x", "q1", "a1");
        append_turn(&cwd, "alpha-x", "q2", "a2");
        assert!(exists(&cwd, "alpha-x"));
        let r = replay(&cwd, "alpha-x", REPLAY_BUDGET);
        assert!(r.contains("Q: q1") && r.contains("A: a1"));
        assert!(r.contains("Q: q2") && r.contains("A: a2"));
        // chronological order: q1 before q2
        assert!(r.find("q1").unwrap() < r.find("q2").unwrap());
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn replay_skips_corrupt_lines() {
        let cwd = tmp();
        std::fs::create_dir_all(topics_dir(&cwd)).unwrap();
        std::fs::write(
            topic_file(&cwd, "beta-y"),
            "{not json}\n{\"q\":\"good\",\"a\":\"reply\"}\n\x00partial",
        )
        .unwrap();
        let r = replay(&cwd, "beta-y", REPLAY_BUDGET);
        assert!(r.contains("Q: good") && r.contains("A: reply"));
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn replay_is_budget_bounded_keeping_recent() {
        let cwd = tmp();
        append_turn(&cwd, "gamma-z", "OLD", &"x".repeat(500));
        append_turn(&cwd, "gamma-z", "NEW", &"y".repeat(50));
        let r = replay(&cwd, "gamma-z", 200); // only the recent small turn fits
        assert!(r.contains("NEW"), "most recent turn must be kept");
        assert!(!r.contains("OLD"), "older oversized turn must be dropped");
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn zero_byte_file_is_not_resumable() {
        // A created-then-crashed (empty) transcript must NOT count as resumable,
        // else resume would seed an empty replay frame.
        let cwd = tmp();
        std::fs::create_dir_all(topics_dir(&cwd)).unwrap();
        std::fs::write(topic_file(&cwd, "empty-x"), "").unwrap();
        assert!(
            !exists(&cwd, "empty-x"),
            "zero-byte transcript is not resumable"
        );
        std::fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn archive_moves_file_away() {
        let cwd = tmp();
        append_turn(&cwd, "delta-w", "q", "a");
        assert!(exists(&cwd, "delta-w"));
        archive(&cwd, "delta-w");
        assert!(!exists(&cwd, "delta-w"), "archived topic no longer active");
        std::fs::remove_dir_all(&cwd).ok();
    }
}
