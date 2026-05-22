//! MCP stdio server: newline-delimited JSON-RPC 2.0.
//!
//! Tool surface: `consult` (isolated topic dialogues), `review_diff` and the
//! automatic `review_stop` gate (both share the warm Codex peer's reserved review
//! thread), plus `health` / `capability_status` / `budget_status`. `review_diff`
//! and `review_stop` capture the change set through the same `git::diff_bundle`
//! path so on-demand and automatic review judge an identical set of changes.

use crate::codex::CodexPeer;
use crate::{gate, health};
use aibridge_platform::{DefaultPlatform, Platform};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};

enum Handled {
    Result(Value),
    Error(i64, String),
    Notification,
}

/// Which conversation thread a call uses. Reviews (the Stop-gate + on-demand
/// `review_diff`) share a reserved thread, kept ISOLATED from consult topics so
/// review reasoning is never contaminated by consult context (or vice versa).
#[derive(Clone, PartialEq, Eq, Hash)]
enum TopicKey {
    /// The reserved review thread (Stop-gate + `review_diff`).
    Gate,
    /// A named, isolated consult dialogue.
    Consult(String),
}

/// Cap on simultaneously-tracked consult topics so a long session can't grow the
/// registry without bound. (The gate slot is separate and never evicted here.)
const MAX_CONSULT_TOPICS: usize = 32;

/// Reset the reserved review thread after this many reviews — bounds anchoring on
/// stale prior-diff findings while keeping warm-cache speed in between.
const GATE_RESET_EVERY: u32 = 10;

/// Holds the warm Codex child + a topic→threadId registry + per-(workspace,session)
/// gate state. ONE child hosts many ISOLATED threads (verified: distinct threadIds
/// don't leak into each other).
struct Server {
    codex: Option<CodexPeer>,
    /// A Codex child warmed in the background (see `spawn_warming`), arriving with
    /// a pre-opened gate thread so the first review is warm. Adopted lazily.
    warm_rx: Option<std::sync::mpsc::Receiver<(CodexPeer, String)>>,
    /// topic → threadId for the CURRENT child ONLY (codex threadIds do not survive
    /// a child restart, so this is cleared on every (re)spawn).
    threads: HashMap<TopicKey, String>,
    gate_reviews: u32,
    gates: HashMap<String, gate::GateState>,
}

impl Server {
    fn new() -> Self {
        Server {
            codex: None,
            warm_rx: None,
            threads: HashMap::new(),
            gate_reviews: 0,
            gates: HashMap::new(),
        }
    }

    /// Ensure a live Codex child, preferring the background-warmed one (which
    /// arrives with a pre-warmed gate thread). Invariant: `self.threads` only ever
    /// holds threadIds for the CURRENT child, so it is cleared on every (re)spawn.
    fn ensure_peer(&mut self) -> anyhow::Result<()> {
        if self.codex.is_some() {
            return Ok(());
        }
        // One-shot: consume the warming receiver. If ready, adopt the warmed child
        // + register its pre-opened gate thread. If not, dropping the rx lets the
        // warming child self-clean (its `send` fails on the closed channel) — no
        // leak — and we cold-spawn instead.
        if let Some(rx) = self.warm_rx.take() {
            if let Ok((peer, gate_tid)) = rx.try_recv() {
                self.codex = Some(peer);
                self.threads.clear();
                self.threads.insert(TopicKey::Gate, gate_tid);
                return Ok(());
            }
        }
        self.codex = Some(CodexPeer::spawn()?);
        self.threads.clear();
        Ok(())
    }

    /// Drop the child + all its (now-dead) thread mappings so the next call spawns
    /// fresh. A timed-out/wedged child must never be reused — its stdin may no
    /// longer drain, and the next (synchronous, unbounded) write would then hang.
    fn invalidate_peer(&mut self) {
        self.codex = None;
        self.threads.clear();
    }

    /// Ask Codex on `key`'s thread: continue it if known (warm `codex-reply`) or
    /// open a fresh one. A transport error invalidates the peer and propagates; a
    /// stale thread ("Session not found") transparently reopens.
    fn ask_topic(&mut self, key: TopicKey, prompt: &str, cwd: &str) -> anyhow::Result<String> {
        self.ensure_peer()?;
        if let Some(tid) = self.threads.get(&key).cloned() {
            let reply = {
                let peer = self
                    .codex
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("codex peer unavailable"))?;
                peer.reply(&tid, prompt)
            };
            match reply {
                Ok(text) if !is_session_lost(&text) => return Ok(text),
                Ok(_) => {
                    self.threads.remove(&key); // stale thread → reopen below
                }
                Err(e) => {
                    self.invalidate_peer();
                    return Err(e);
                }
            }
        }
        self.ensure_peer()?;
        let opened = {
            let peer = self
                .codex
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("codex peer unavailable"))?;
            peer.open_thread(prompt, cwd)
        };
        match opened {
            Ok((tid, text)) => {
                if matches!(key, TopicKey::Consult(_)) {
                    self.evict_consult_if_full();
                }
                self.threads.insert(key, tid);
                Ok(text)
            }
            Err(e) => {
                self.invalidate_peer();
                Err(e)
            }
        }
    }

    /// Apply the periodic anti-anchoring reset of the reserved Gate thread. MUST be
    /// called before EVERY review on the Gate thread — both the automatic Stop gate
    /// AND manual `review_diff` — so the bound holds across both (otherwise a run of
    /// manual reviews would anchor the very thread the Stop gate later reuses).
    fn tick_gate_reset(&mut self) {
        self.gate_reviews += 1;
        if self.gate_reviews >= GATE_RESET_EVERY {
            self.threads.remove(&TopicKey::Gate);
            self.gate_reviews = 0;
        }
    }

    /// Keep the consult-topic count bounded (drops one tracked topic when full;
    /// that topic simply reopens cold next time it's used). Gate slot is exempt.
    fn evict_consult_if_full(&mut self) {
        let count = self
            .threads
            .keys()
            .filter(|k| matches!(k, TopicKey::Consult(_)))
            .count();
        if count >= MAX_CONSULT_TOPICS {
            if let Some(victim) = self
                .threads
                .keys()
                .find(|k| matches!(k, TopicKey::Consult(_)))
                .cloned()
            {
                self.threads.remove(&victim);
            }
        }
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
                let args = msg.pointer("/params/arguments");
                let question = args
                    .and_then(|a| a.get("question"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let topic = args
                    .and_then(|a| a.get("topic"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let reset = args
                    .and_then(|a| a.get("reset"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if question.is_empty() {
                    "AI Bridge: `consult` requires a non-empty 'question' argument.".to_string()
                } else {
                    self.consult(question, topic, reset)
                }
            }
            "review_diff" => self.review_diff(),
            "review_stop" => self.review_stop(msg),
            other => format!("AI Bridge: unknown tool '{other}'."),
        };
        json!({ "content": [{ "type": "text", "text": text }] })
    }

    /// On-demand second opinion on an isolated, named topic thread. Continuous
    /// across calls AND across sessions: a named topic's turns are persisted, so
    /// resuming it after a Claude restart replays recent context (codex threadIds
    /// don't survive a restart). Omitted/blank topic → ephemeral shared `scratch`
    /// (not persisted). `reset` archives the topic and starts cold.
    fn consult(&mut self, question: &str, topic: &str, reset: bool) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let topic = match normalize_topic(topic) {
            Ok(t) => t,
            Err(e) => return format!("AI Bridge: invalid consult topic — {e}"),
        };
        let key = TopicKey::Consult(topic.clone());
        let persisted = topic != "scratch";
        if reset {
            self.threads.remove(&key);
            if persisted {
                crate::topics::archive(&cwd, &topic); // keep history, start cold
            }
        }
        let base = "You are a peer reviewer giving a concise, skeptical second opinion. \
                    Be specific and call out risks.";
        // Resume across sessions: no live thread for this topic but a transcript
        // exists → seed the fresh thread with a bounded replay of recent turns.
        // NOTE: codex's own tool appears to resume the latest CWD session on a
        // fresh process's first call; our explicit replay dominates (verified:
        // topic-correct recall), but that ambient context still rides along. If a
        // codex `--no-resume`/`--fresh` option appears, pass it in `open_thread`
        // to drop the ambient injection entirely.
        let resuming =
            persisted && !self.threads.contains_key(&key) && crate::topics::exists(&cwd, &topic);
        let prompt = if resuming {
            let replay = crate::topics::replay(&cwd, &topic, crate::topics::REPLAY_BUDGET);
            format!(
                "{base}\n\n[Resuming dialogue topic '{topic}'. Prior exchange for context:]\n\
                 {replay}\n\n[End prior context. New question:]\n{question}"
            )
        } else {
            format!("{base} Question:\n{question}")
        };
        match self.ask_topic(key, &prompt, &cwd) {
            Ok(reply) if !reply.trim().is_empty() => {
                if persisted {
                    // Persist the RAW question + reply (not the seeded prompt).
                    crate::topics::append_turn(&cwd, &topic, question, &reply);
                }
                format!("{reply}\n\n— AI Bridge consult (topic: {topic})")
            }
            Ok(_) => "AI Bridge: Codex returned an empty reply.".to_string(),
            Err(e) => format!(
                "AI Bridge: consult unavailable (Codex error): {e}. \
                 Proceed without it, retry, or fix the issue?"
            ),
        }
    }

    fn review_diff(&mut self) -> String {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        // Capture the change set via the SAME path as the automatic Stop gate
        // (`diff_bundle`: status + staged + unstaged + UNTRACKED file contents,
        // project-subtree scoped, `.ai-bridge` excluded). A tracked-only
        // `git diff HEAD` here used to make on-demand `review_diff` report
        // "nothing to review" for brand-new (untracked) files that the gate WOULD
        // review/block — one source of truth avoids that drift.
        let bundle = match crate::git::diff_bundle(&cwd) {
            Ok(b) => b,
            Err(e) => return format!("AI Bridge: could not read the git diff: {e}"),
        };
        if bundle.is_empty {
            return "AI Bridge: no uncommitted changes to review (working tree clean).".to_string();
        }
        // Send the bundle verbatim — exactly as the Stop gate does (`gate::prompt`
        // also passes `bundle.text` with no size cap) — so on-demand and automatic
        // review judge a byte-for-byte identical change set. Deliberate trade-off:
        // only the UNTRACKED content is size-capped (inside `diff_bundle`); the
        // staged/unstaged patch is intentionally NOT size-capped on either path,
        // because a thorough review must see the whole change. A very large diff is
        // bounded by the Codex call DEADLINE (`CALL_TIMEOUT`), never by silently
        // dropping content (which would under-review). Accepted latency/quota cost.
        let prompt = format!(
            "You are a skeptical peer reviewer. Review the current uncommitted changes \
             below (git status + staged + unstaged + untracked file contents): find bugs, \
             risks, edge cases, and missing tests; cite file/line; if it looks good, say \
             so briefly.\n\n{}",
            bundle.text
        );
        // On-demand review shares the reserved review thread (isolated from consults)
        // and counts toward the same anti-anchoring reset bound as the Stop gate.
        self.tick_gate_reset();
        match self.ask_topic(TopicKey::Gate, &prompt, &cwd) {
            Ok(reply) if !reply.trim().is_empty() => reply,
            Ok(_) => "AI Bridge: Codex returned an empty review.".to_string(),
            Err(e) => format!(
                "AI Bridge: review unavailable (Codex error): {e}. \
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
        log_gate(
            cwd,
            &format!("reviewing diff bundle: {} bytes", bundle.text.len()),
        );
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
        // Periodic anti-anchoring reset of the reserved review thread (shared bound
        // with manual review_diff; warm-cache speed is kept for the runs between).
        self.tick_gate_reset();
        // Ensure the child (also adopts the warmed one) so we can log how it launched.
        if let Err(e) = self.ensure_peer() {
            log_gate(cwd, &format!("Codex review FAILED: {e}"));
            return self.fail_ask(
                st,
                dh,
                "peer review couldn't run (Codex unavailable, timed out, or quota exhausted)",
            );
        }
        let warm = self.threads.contains_key(&TopicKey::Gate);
        let kind = self.codex.as_ref().map(|p| p.spawn_kind()).unwrap_or("?");
        log_gate(
            cwd,
            &format!(
                "calling Codex [{kind}, {}]",
                if warm { "warm" } else { "cold" }
            ),
        );
        // ask_topic owns peer-invalidation on transport error.
        let review = match self.ask_topic(TopicKey::Gate, &prompt, cwd) {
            Ok(r) => {
                log_gate(cwd, "Codex review returned");
                r
            }
            Err(e) => {
                // Log the real cause (timeout / EOF / quota) for diagnosis; keep
                // the user-facing reason short. `ask_topic` already dropped the peer.
                log_gate(cwd, &format!("Codex review FAILED: {e}"));
                return self.fail_ask(
                    st,
                    dh,
                    "peer review couldn't run (Codex unavailable, timed out, or quota exhausted)",
                );
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

/// True when a `codex-reply` came back as a stale-thread notice rather than a real
/// reply. Codex returns this as normal result TEXT (not a JSON-RPC error) when the
/// threadId is unknown to the current child (e.g. after a restart).
fn is_session_lost(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("Session not found") || t.contains("Session not found for thread_id")
}

/// Normalize + validate a consult topic. Blank → the shared `scratch` channel.
/// Otherwise enforce a stable, semantic kebab-case name (mirrors the predecessor's
/// rules) so Claude can't fragment or collide topics with vague/auto-generated ids.
fn normalize_topic(raw: &str) -> Result<String, String> {
    let t = raw.trim().to_lowercase();
    if t.is_empty() {
        return Ok("scratch".to_string());
    }
    if t.len() < 3 || t.len() > 64 {
        return Err("topic length must be 3..64 characters".to_string());
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("topic must be kebab-case (a-z, 0-9, '-')".to_string());
    }
    if t.starts_with('-') || t.ends_with('-') || t.contains("--") {
        return Err("topic must not start/end with '-' or contain '--'".to_string());
    }
    // Reject hash-shaped ids (long all-hex) — they aren't semantic.
    if t.len() >= 12 && t.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err("topic looks hash-shaped; use a semantic name".to_string());
    }
    const VAGUE: &[&str] = &[
        "review", "fix", "task", "default", "misc", "temp", "tmp", "test", "stuff", "work", "todo",
        "scratch",
    ];
    if VAGUE.contains(&t.as_str()) {
        return Err(format!(
            "topic '{t}' is too vague; use a specific kebab-case name like repo-feature-phase"
        ));
    }
    // The topic becomes a filename (`<topic>.jsonl`); reject Windows reserved
    // device names (case-insensitive, reserved even with an extension).
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if RESERVED.contains(&t.as_str()) {
        return Err(format!(
            "topic '{t}' is a reserved device name; pick another"
        ));
    }
    Ok(t)
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

/// Record the MCP server's spawned-context environment (PATH + resolved CLIs) to
/// `.ai-bridge/runtime/snapshot.json` so `doctor` can detect a terminal-vs-Claude
/// PATH mismatch (a real Windows failure class).
fn write_runtime_snapshot() {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let dir = std::path::Path::new(&cwd)
        .join(".ai-bridge")
        .join("runtime");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let resolve = |name: &str| {
        DefaultPlatform::find_executable(name)
            .ok()
            .map(|p| p.display().to_string())
    };
    // How the warm Codex child WOULD be launched here (without spawning it), so
    // `doctor` can flag the degraded `cmd /C` shim path that hangs the gate.
    let codex_spawn = DefaultPlatform::find_executable("codex").ok().map(|exe| {
        let plan = DefaultPlatform::spawn_plan(&exe);
        json!({ "kind": plan.kind.as_str(), "program": plan.program })
    });
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let snapshot = json!({
        "ts_ms": ts,
        "pid": std::process::id(),
        "exe": std::env::current_exe().ok().map(|p| p.display().to_string()),
        "cwd": cwd,
        "path": std::env::var("PATH").unwrap_or_default(),
        "resolved": {
            "git": resolve("git"),
            "codex": resolve("codex"),
            "rtk": resolve("rtk"),
        },
        "codex_spawn": codex_spawn,
    });
    let _ = std::fs::write(
        dir.join("snapshot.json"),
        serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
    );
}

/// Primer for the reserved review thread: a tiny cold turn that establishes the
/// reviewer conversation and caches Codex's system prompt, so the first real
/// review is a fast `codex-reply` (measured ~2.3x faster; a cold turn on a large
/// diff can exceed the Stop-hook timeout).
const GATE_PRIMER: &str =
    "You are AI Bridge's strict code reviewer for this project. Each turn supplies \
     its own diff to review. Reply with the single token READY.";

/// Warm a Codex child in the background and pre-open the reserved GATE thread, so
/// the first review is warm. Hands back `(child, gate_threadId)`. Best-effort: the
/// gate cold-spawns lazily if this isn't ready in time. One tiny cold call per
/// server start pays the system-prompt cost once, off the review path.
fn spawn_warming() -> std::sync::mpsc::Receiver<(CodexPeer, String)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    std::thread::spawn(move || {
        if let Ok(mut peer) = CodexPeer::spawn() {
            // Only hand over a child whose gate thread actually opened.
            if let Ok((gate_tid, _)) = peer.open_thread(GATE_PRIMER, &cwd) {
                let _ = tx.send((peer, gate_tid));
            }
        }
    });
    rx
}

/// Run the stdio JSON-RPC loop until EOF.
pub fn serve() -> anyhow::Result<()> {
    let mut server = Server::new();
    write_runtime_snapshot(); // record the spawned-context PATH for `doctor`
    server.warm_rx = Some(spawn_warming()); // warm Codex while the user works
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
             tradeoff. Does not gate final output. Pass a stable `topic` to keep a continuous, \
             isolated dialogue across calls — reuse the SAME topic for follow-ups on one subject.",
            json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "What to ask the Codex peer." },
                    "topic": { "type": "string", "description": "Stable kebab-case dialogue topic for this subject (e.g. 'repo-feature-phase'). Reuse it for follow-ups so Codex keeps context. Omit for a one-off (shared 'scratch' channel)." },
                    "reset": { "type": "boolean", "description": "Start this topic fresh, discarding prior turns." }
                },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_topic_defaults_to_scratch() {
        assert_eq!(normalize_topic("").unwrap(), "scratch");
        assert_eq!(normalize_topic("   ").unwrap(), "scratch");
    }

    #[test]
    fn accepts_semantic_kebab_topics() {
        assert_eq!(
            normalize_topic("VoiceTyper-history-fix").unwrap(),
            "voicetyper-history-fix"
        );
        assert_eq!(
            normalize_topic("repo-feature-phase").unwrap(),
            "repo-feature-phase"
        );
    }

    #[test]
    fn rejects_vague_hash_and_malformed_topics() {
        assert!(normalize_topic("fix").is_err()); // vague
        assert!(normalize_topic("review").is_err()); // vague
        assert!(normalize_topic("a").is_err()); // too short
        assert!(normalize_topic("has space").is_err()); // not kebab
        assert!(normalize_topic("snake_case_topic").is_err()); // underscore not allowed
        assert!(normalize_topic("-leading").is_err());
        assert!(normalize_topic("double--dash").is_err());
        assert!(normalize_topic("deadbeefcafe123").is_err()); // hash-shaped
        assert!(normalize_topic("con").is_err()); // Windows reserved
        assert!(normalize_topic("com1").is_err()); // Windows reserved
        assert!(normalize_topic("nul").is_err()); // Windows reserved
        assert!(normalize_topic("scratch").is_err()); // internal-only sentinel
                                                      // a reserved name as a SUBSTRING is fine (only the exact base is reserved)
        assert!(normalize_topic("con-figuration").is_ok());
    }

    #[test]
    fn detects_stale_thread_notice() {
        assert!(is_session_lost(
            "Session not found for thread_id: 019e5051-9606-7870"
        ));
        assert!(is_session_lost("  Session not found"));
        assert!(!is_session_lost(
            "secret-alpha = LION; I do not know secret-beta."
        ));
    }

    #[test]
    fn topic_keys_are_distinct() {
        use std::collections::HashSet;
        let mut s = HashSet::new();
        s.insert(TopicKey::Gate);
        s.insert(TopicKey::Consult("alpha-x".into()));
        s.insert(TopicKey::Consult("beta-y".into()));
        s.insert(TopicKey::Consult("alpha-x".into())); // dup
        assert_eq!(s.len(), 3);
    }
}
